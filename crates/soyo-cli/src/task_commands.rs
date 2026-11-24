//! CLI adapter for the durable cross-run task queue (`F-08`).
//!
//! Tasks are submitted with an input, leased to a worker, driven through one
//! Run per attempt, and resolved by an explicit terminal transition. A worker
//! crash after the dispatch intent is recovered as `waiting_unknown` and never
//! replayed automatically — only an explicit `resolve` requeues or blocks it.

use super::{Cancel, Cli, Terminal, personal_context, signals};
use clap::Subcommand;
use serde_json::{Value, json};
use soyo_app::task_queue::{TaskRunContext, open_task_store, run_task_once};
use soyo_core::{
    approvals::Preauthorizations,
    config::{Config, Output},
    contract::{BoundaryError, ErrorInfo},
    task_queue::{TaskId, TaskRecord, TaskStatus},
};
use std::{io::IsTerminal, path::PathBuf, sync::Arc};

fn failure(code: &str, message: &str) -> BoundaryError {
    BoundaryError(
        ErrorInfo {
            code: code.into(),
            message: message.into(),
        },
        None,
        None,
    )
}

fn start_signals(cancel: Arc<Cancel>) -> Result<signals::CancellationListener, BoundaryError> {
    signals::CancellationListener::start(cancel)
        .map_err(|_| failure("signal_error", "Task signal handling is unavailable"))
}

#[derive(Debug, Clone, Subcommand)]
pub enum TaskAction {
    /// Submit a new task and its dedicated session (queued, not started).
    Submit { input: String },
    /// List persisted tasks.
    List,
    /// Show one complete task record.
    Status { task_id: String },
    /// Pause a queued, running or blocked task.
    Pause { task_id: String },
    /// Requeue a paused task.
    Resume { task_id: String },
    /// Cancel a non-terminal task.
    Cancel { task_id: String },
    /// Resolve a task waiting on an unknown Run outcome. Retry is never implicit.
    Resolve {
        task_id: String,
        /// Requeue for a fresh attempt; omit to block for external input.
        #[arg(long)]
        retry: bool,
    },
    /// Drive exactly one queued task attempt under a durable lease.
    Continue {
        task_id: String,
        #[arg(long)]
        approval_file: Option<PathBuf>,
        #[arg(long)]
        accept_config_change: bool,
    },
    /// Recover in-flight tasks whose worker lease expired (mark unknown, never replay).
    Recover,
}

fn parse_task(raw: &str) -> Result<TaskId, BoundaryError> {
    raw.parse()
}

fn secrets(config: &Config) -> Result<Vec<String>, BoundaryError> {
    config
        .secret_env_names()
        .iter()
        .map(|name| config.secret(name).map(str::to_owned))
        .collect()
}

fn print_value(config: &Config, value: &Value, text: impl FnOnce()) {
    match config.output {
        Output::Text => text(),
        Output::Json => println!("{value}"),
        Output::Jsonl => println!("{value}"),
    }
}

fn task_text(task: &TaskRecord) {
    println!("Task {}", task.id.0);
    println!("Status: {:?}", task.status);
    println!("{}", task.input.chars().take(200).collect::<String>());
    if let Some(failure) = &task.failure {
        println!("Failure: {} — {}", failure.code, failure.message);
    }
    if let Some(result) = &task.last_result {
        println!(
            "Last run: {} · {:?} · {}",
            result.run_id.0,
            result.status,
            result
                .stop_reason
                .clone()
                .unwrap_or_else(|| "(no reason)".into())
        );
    }
}

fn terminal(config: &Config) -> Result<Option<Arc<Terminal>>, BoundaryError> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Terminal::open(secrets(config)?)
            .map(Some)
            .map(|terminal| terminal.map(Arc::new))
            .map_err(|_| failure("terminal_error", "Task terminal is unavailable"))
    } else {
        Ok(None)
    }
}

fn run_context(
    cli: &Cli,
    config: &Config,
    approval_file: Option<&PathBuf>,
    cancellation: Arc<Cancel>,
    accept_config_change: bool,
) -> Result<TaskRunContext, BoundaryError> {
    let terminal = terminal(config)?;
    let approvals = approval_file
        .map(|path| {
            Preauthorizations::from_file(path, &config.cwd)
                .map_err(|_| failure("invalid_approval_file", "Task approval file is invalid"))
        })
        .transpose()?;
    Ok(TaskRunContext {
        permissions: config.permissions_mode,
        confirmation: terminal
            .as_ref()
            .map(|terminal| terminal.clone() as Arc<dyn soyo_core::runtime::ApprovalHandler>),
        input: terminal.map(|terminal| terminal as Arc<dyn soyo_core::input::InputHandler>),
        approvals,
        personal_context: personal_context(cli, config)
            .map_err(|_| failure("personal_context", "Task personal context is invalid"))?,
        anthropic_prompt_cache: cli.anthropic_prompt_cache,
        accept_config_change,
        cancellation,
    })
}

pub async fn run(action: &TaskAction, cli: &Cli, config: &Config) -> Result<u8, BoundaryError> {
    let value = match action {
        TaskAction::Submit { input } => {
            let task = soyo_app::task_queue::submit_task(config, input).await?;
            let value = json!({"schema_version":1,"data":{"task":task.clone()}});
            print_value(config, &value, || {
                println!("Task {} queued", task.id.0);
            });
            return Ok(0);
        }
        TaskAction::List => {
            let tasks = soyo_app::task_queue::list_tasks(config).await?;
            let value = json!({"schema_version":1,"data":{"tasks":tasks.clone()}});
            print_value(config, &value, || {
                for task in tasks {
                    println!(
                        "{} {:?} {}",
                        task.id.0,
                        task.status,
                        task.input.chars().take(80).collect::<String>()
                    );
                }
            });
            return Ok(0);
        }
        TaskAction::Status { task_id } => {
            let task = soyo_app::task_queue::load_task(config, parse_task(task_id)?).await?;
            let value = json!({"schema_version":1,"data":{"task":task.clone()}});
            print_value(config, &value, || task_text(&task));
            return Ok(0);
        }
        TaskAction::Pause { task_id } => {
            let task = soyo_app::task_queue::transition_task(
                config,
                parse_task(task_id)?,
                TaskStatus::Paused,
            )
            .await?;
            json!({"schema_version":1,"data":{"task":task}})
        }
        TaskAction::Resume { task_id } => {
            let task = soyo_app::task_queue::transition_task(
                config,
                parse_task(task_id)?,
                TaskStatus::Queued,
            )
            .await?;
            json!({"schema_version":1,"data":{"task":task}})
        }
        TaskAction::Cancel { task_id } => {
            let task = soyo_app::task_queue::transition_task(
                config,
                parse_task(task_id)?,
                TaskStatus::Cancelled,
            )
            .await?;
            json!({"schema_version":1,"data":{"task":task}})
        }
        TaskAction::Resolve { task_id, retry } => {
            let task =
                soyo_app::task_queue::resolve_task(config, parse_task(task_id)?, *retry).await?;
            json!({"schema_version":1,"data":{"task":task}})
        }
        TaskAction::Recover => {
            let tasks = soyo_app::task_queue::recover_tasks(config).await?;
            json!({"schema_version":1,"data":{"tasks":tasks}})
        }
        TaskAction::Continue {
            task_id,
            approval_file,
            accept_config_change,
        } => {
            let store = open_task_store(config)?;
            let task_id = parse_task(task_id)?;
            let cancel = Arc::new(Cancel::new());
            let _signals = start_signals(cancel.clone())?;
            let context = run_context(
                cli,
                config,
                approval_file.as_ref(),
                cancel,
                *accept_config_change,
            )?;
            let outcome = run_task_once(config, task_id, context).await?;
            drop(store);
            return Ok(json_exit(&outcome.task, &outcome.result));
        }
    };
    print_value(config, &value, || {});
    Ok(0)
}

fn json_exit(task: &TaskRecord, result: &soyo_core::contract::RunResult) -> u8 {
    println!(
        "{}",
        json!({"schema_version":1,"data":{"task":task,"run":result}})
    );
    match result.status {
        soyo_core::contract::RunStatus::Completed => 0,
        soyo_core::contract::RunStatus::Cancelled => 130,
        _ => 1,
    }
}
