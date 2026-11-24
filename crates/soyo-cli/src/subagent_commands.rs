//! CLI adapter for persistent subagent jobs (`F-09`, `D-110`).
//!
//! Persistent children are stored in the same durable task database as `F-08`
//! tasks but are listed and driven through `soyo subagent`. An attempt whose
//! worker stopped before recording its outcome waits as `waiting_unknown` and
//! is never replayed automatically.

use super::{Cancel, Cli, Terminal, personal_context, signals};
use clap::Subcommand;
use serde_json::{Value, json};
use soyo_app::{subagent_jobs, task_queue::TaskRunContext};
use soyo_core::{
    approvals::Preauthorizations,
    config::{Config, Output},
    contract::{BoundaryError, ErrorInfo, RunStatus},
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
        .map_err(|_| failure("signal_error", "Subagent signal handling is unavailable"))
}

#[derive(Debug, Clone, Subcommand)]
pub enum SubagentAction {
    /// List persisted subagent jobs.
    List,
    /// Show one persistent subagent snapshot.
    Status { subagent_id: String },
    /// Show changed files and a bounded patch for a persistent subagent.
    Diff {
        subagent_id: String,
        /// Omit the unified patch and return only the change manifest.
        #[arg(long)]
        no_patch: bool,
    },
    /// Conflict-check and merge a completed persistent subagent into the
    /// current parent workspace.
    Merge {
        subagent_id: String,
        /// Merge only these workspace-relative paths.
        #[arg(long = "path")]
        paths: Vec<String>,
        /// Keep the isolated workspace after a successful merge.
        #[arg(long)]
        keep_workspace: bool,
    },
    /// Cancel a queued or running persistent subagent.
    Cancel {
        subagent_id: String,
        /// Remove the isolated workspace after cancelling.
        #[arg(long)]
        discard: bool,
    },
    /// Resolve a job waiting on an unknown outcome. Retry is never implicit.
    Resolve {
        subagent_id: String,
        /// Requeue for a fresh attempt; omit to block for external input.
        #[arg(long)]
        retry: bool,
    },
    /// Drive exactly one queued persistent-subagent attempt.
    Continue {
        subagent_id: String,
        #[arg(long)]
        approval_file: Option<PathBuf>,
        #[arg(long)]
        accept_config_change: bool,
    },
    /// Recover in-flight jobs whose worker lease expired (mark unknown, never replay).
    Recover,
}

fn print_value(config: &Config, value: &Value, text: impl FnOnce()) {
    match config.output {
        Output::Text => text(),
        Output::Json | Output::Jsonl => println!("{value}"),
    }
}

fn secrets(config: &Config) -> Result<Vec<String>, BoundaryError> {
    config
        .secret_env_names()
        .iter()
        .map(|name| config.secret(name).map(str::to_owned))
        .collect()
}

fn terminal(config: &Config) -> Result<Option<Arc<Terminal>>, BoundaryError> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Terminal::open(secrets(config)?)
            .map(Some)
            .map(|terminal| terminal.map(Arc::new))
            .map_err(|_| failure("terminal_error", "Subagent terminal is unavailable"))
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
                .map_err(|_| failure("invalid_approval_file", "Subagent approval file is invalid"))
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
            .map_err(|_| failure("personal_context", "Subagent personal context is invalid"))?,
        anthropic_prompt_cache: cli.anthropic_prompt_cache,
        accept_config_change,
        cancellation,
    })
}

fn snapshot_text(snapshot: &soyo_core::subagent::SubagentSnapshot) {
    println!("Subagent {}", snapshot.id);
    println!("Label: {}", snapshot.label);
    println!("State: {}", snapshot.state.as_str());
    println!(
        "Mode: {:?} · isolation {:?}",
        snapshot.work_mode, snapshot.isolation_kind
    );
    if let Some(summary) = &snapshot.summary {
        println!("Summary: {}", summary.chars().take(400).collect::<String>());
    }
    if let Some(error) = &snapshot.error {
        println!("Error: {} — {}", error.code, error.message);
    }
    if !snapshot.changed_files.is_empty() {
        println!("Changed files: {}", snapshot.changed_files.len());
    }
}

fn job_text(task: &soyo_core::task_queue::TaskRecord) {
    let label = task
        .subagent
        .as_ref()
        .map(|job| job.label.as_str())
        .unwrap_or("(missing)");
    println!(
        "{} {:?} {} — {}",
        task.id.0,
        task.status,
        label,
        task.input.chars().take(80).collect::<String>()
    );
}

pub async fn run(action: &SubagentAction, cli: &Cli, config: &Config) -> Result<u8, BoundaryError> {
    match action {
        SubagentAction::List => {
            let jobs = subagent_jobs::list_jobs(config).await?;
            let value = json!({
                "schema_version": 1,
                "data": {
                    "subagents": jobs
                        .iter()
                        .map(|(task, job)| job.snapshot(task))
                        .collect::<Vec<_>>()
                }
            });
            print_value(config, &value, || {
                for (task, _) in &jobs {
                    job_text(task);
                }
            });
            Ok(0)
        }
        SubagentAction::Status { subagent_id } => {
            let snapshot = subagent_jobs::status(config, subagent_id).await?;
            let value = json!({"schema_version": 1, "data": {"subagent": snapshot.clone()}});
            print_value(config, &value, || snapshot_text(&snapshot));
            Ok(0)
        }
        SubagentAction::Diff {
            subagent_id,
            no_patch,
        } => {
            let report = subagent_jobs::diff(config, subagent_id, !no_patch).await?;
            let value = json!({"schema_version": 1, "data": {"diff": report}});
            print_value(config, &value, || {
                for change in &report.changed_files {
                    println!(
                        "{:?} {} {}{}",
                        change.kind,
                        change.path,
                        change.bytes,
                        if change.has_conflict {
                            " (conflict)"
                        } else {
                            ""
                        }
                    );
                }
                if !report.patch.is_empty() {
                    println!("{}", report.patch);
                }
            });
            Ok(0)
        }
        SubagentAction::Merge {
            subagent_id,
            paths,
            keep_workspace,
        } => {
            let paths = (!paths.is_empty()).then(|| paths.clone());
            let report = subagent_jobs::merge(config, subagent_id, paths, !*keep_workspace).await?;
            let value = json!({
                "schema_version": 1,
                "data": {
                    "subagent": report.snapshot.clone(),
                    "receipts": report.receipts,
                    "conflicts": report.conflicts,
                }
            });
            print_value(config, &value, || {
                if report.conflicts.is_empty() {
                    println!("Merged {} file(s)", report.receipts.len());
                } else {
                    println!(
                        "Merge aborted with zero writes; {} conflict(s)",
                        report.conflicts.len()
                    );
                    for conflict in &report.conflicts {
                        println!("{}: {}", conflict.path, conflict.reason);
                    }
                }
            });
            Ok(if report.conflicts.is_empty() { 0 } else { 1 })
        }
        SubagentAction::Cancel {
            subagent_id,
            discard,
        } => {
            let snapshot = subagent_jobs::cancel(config, subagent_id, *discard).await?;
            let value = json!({"schema_version": 1, "data": {"subagent": snapshot.clone()}});
            print_value(config, &value, || snapshot_text(&snapshot));
            Ok(0)
        }
        SubagentAction::Resolve { subagent_id, retry } => {
            let snapshot = subagent_jobs::resolve(config, subagent_id, *retry).await?;
            let value = json!({"schema_version": 1, "data": {"subagent": snapshot.clone()}});
            print_value(config, &value, || snapshot_text(&snapshot));
            Ok(0)
        }
        SubagentAction::Recover => {
            let snapshots = subagent_jobs::recover(config).await?;
            let value = json!({"schema_version": 1, "data": {"subagents": snapshots}});
            print_value(config, &value, || {
                for snapshot in &snapshots {
                    snapshot_text(snapshot);
                }
            });
            Ok(0)
        }
        SubagentAction::Continue {
            subagent_id,
            approval_file,
            accept_config_change,
        } => {
            let task_id = subagent_id.parse().map_err(|_| {
                failure(
                    "invalid_task_id",
                    "Persistent subagent identifier is invalid",
                )
            })?;
            let cancel = Arc::new(Cancel::new());
            let _signals = start_signals(cancel.clone())?;
            let context = run_context(
                cli,
                config,
                approval_file.as_ref(),
                cancel,
                *accept_config_change,
            )?;
            let outcome = subagent_jobs::run_subagent_once(config, task_id, context).await?;
            println!(
                "{}",
                json!({"schema_version": 1, "data": {"task": outcome.task, "run": outcome.result}})
            );
            Ok(match outcome.result.status {
                RunStatus::Completed => 0,
                RunStatus::Cancelled => 130,
                _ => 1,
            })
        }
    }
}
