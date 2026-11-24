//! Durable task queue controls (`F-08`): submit, inspect, pause, resume,
//! cancel, resolve unknowns and drive one attempt per `/task continue`.

use super::*;
use crate::{i18n::services as text, view::Details};
use soyo_app::{
    session,
    task_queue::{TaskRunContext, open_task_store, run_task_once},
};
use soyo_core::{
    config::Config,
    contract::PermissionMode,
    task_queue::{TaskId, TaskRecord, TaskStatus},
};
use std::sync::Arc;

fn failure(code: &str, message: &str) -> BoundaryError {
    BoundaryError(
        soyo_core::contract::ErrorInfo {
            code: code.into(),
            message: message.into(),
        },
        None,
        None,
    )
}

fn store(config: &Config) -> std::result::Result<soyo_core::task_queue::TaskStore, BoundaryError> {
    open_task_store(config)
}

fn task_lines(task: &TaskRecord) -> Vec<String> {
    let mut lines = vec![
        format!("Task {}", task.id.0),
        format!("Status: {:?}", task.status),
        format!("Input: {}", task.input),
        format!("Workspace: {}", task.workspace.display()),
        format!("Profile: {} · Mode: {:?}", task.profile, task.mode),
        format!(
            "Created: {} · Updated: {}",
            task.created_at, task.updated_at
        ),
    ];
    if let Some(failure) = &task.failure {
        lines.push(format!("Failure: {} — {}", failure.code, failure.message));
    }
    if let Some(result) = &task.last_result {
        lines.push(format!(
            "Last run: {} · {:?} · {}",
            result.run_id.0,
            result.status,
            result
                .stop_reason
                .clone()
                .unwrap_or_else(|| "(no reason)".into())
        ));
        if let Some(text) = &result.text {
            lines.push(format!("  {}", text.chars().take(200).collect::<String>()));
        }
    }
    lines
}

fn details(task: &TaskRecord) -> Details {
    Details {
        title: format!("Task · {:?}", task.status),
        lines: task_lines(task),
    }
}

fn tasks_details(tasks: Vec<TaskRecord>) -> Details {
    let mut lines = vec![format!("{} tasks", tasks.len())];
    for task in &tasks {
        lines.push(format!(
            "{} · {:?} · {}",
            task.id.0,
            task.status,
            task.input.chars().take(80).collect::<String>()
        ));
    }
    Details {
        title: "Tasks".into(),
        lines,
    }
}

fn parse_id(raw: &str) -> std::result::Result<TaskId, BoundaryError> {
    raw.parse()
}

async fn finish_job(
    task: TaskRecord,
    cancelled: bool,
) -> std::result::Result<services::UiResult, BoundaryError> {
    if cancelled {
        return Err(failure("cancelled", text::UI_QUERY_CANCELLED));
    }
    Ok(services::UiResult::Details(details(&task)))
}

impl App {
    pub(super) async fn open_tasks(&mut self) -> Result<()> {
        if self.ui_job.is_some() {
            return Err(text::UI_ACTIVE.into());
        }
        session::check_roots(&self.config, &self.config.cwd, self.current_mode()?)?;
        let config = self.config.clone();
        let cancel = Arc::new(RunCancellation::new());
        let checking = cancel.clone();
        let join = tokio::spawn(async move {
            let tasks = store(&config)?.list().await?;
            if checking.is_cancelled() {
                return Err(failure("cancelled", text::UI_QUERY_CANCELLED));
            }
            Ok(services::UiResult::Details(tasks_details(tasks)))
        });
        self.begin_ui(cancel, join);
        Ok(())
    }

    pub(super) async fn task(&mut self, argument: &str) -> Result<()> {
        if argument.is_empty() {
            return self.open_tasks().await;
        }
        if self.ui_job.is_some() {
            return Err(text::UI_ACTIVE.into());
        }
        if argument == "submit" || argument.starts_with("submit ") {
            let input = argument
                .strip_prefix("submit")
                .map(str::trim)
                .filter(|input| !input.is_empty())
                .ok_or(text::TASK_SUBMIT_USAGE)?;
            self.require_idle()?;
            let config = self.config.clone();
            let input = input.to_owned();
            let cancel = Arc::new(RunCancellation::new());
            let checking = cancel.clone();
            let join = tokio::spawn(async move {
                let task = soyo_app::task_queue::submit_task(&config, &input).await?;
                finish_job(task, checking.is_cancelled()).await
            });
            self.begin_ui(cancel, join);
            return Ok(());
        }
        if argument == "recover" {
            let config = self.config.clone();
            let cancel = Arc::new(RunCancellation::new());
            let checking = cancel.clone();
            let join = tokio::spawn(async move {
                let tasks = soyo_app::task_queue::recover_tasks(&config).await?;
                if checking.is_cancelled() {
                    return Err(failure("cancelled", text::UI_QUERY_CANCELLED));
                }
                Ok(services::UiResult::Details(tasks_details(tasks)))
            });
            self.begin_ui(cancel, join);
            return Ok(());
        }
        let (action, rest) = argument.split_once(' ').unwrap_or((argument, ""));
        let (id, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        match action {
            "status" | "pause" | "resume" | "cancel" if !id.is_empty() && tail.is_empty() => {
                let config = self.config.clone();
                let task_id = parse_id(id)?;
                let action = action.to_owned();
                let cancel = Arc::new(RunCancellation::new());
                let checking = cancel.clone();
                let join = tokio::spawn(async move {
                    let task = match action.as_str() {
                        "status" => store(&config)?.load(task_id).await?,
                        "pause" => {
                            store(&config)?
                                .transition(
                                    task_id,
                                    TaskStatus::Paused,
                                    soyo_app::events::timestamp()?,
                                )
                                .await?
                        }
                        "resume" => {
                            store(&config)?
                                .transition(
                                    task_id,
                                    TaskStatus::Queued,
                                    soyo_app::events::timestamp()?,
                                )
                                .await?
                        }
                        _ => {
                            store(&config)?
                                .transition(
                                    task_id,
                                    TaskStatus::Cancelled,
                                    soyo_app::events::timestamp()?,
                                )
                                .await?
                        }
                    };
                    finish_job(task, checking.is_cancelled()).await
                });
                self.begin_ui(cancel, join);
            }
            "resolve" if !id.is_empty() => {
                let retry = tail.trim() == "retry";
                let config = self.config.clone();
                let task_id = parse_id(id)?;
                let cancel = Arc::new(RunCancellation::new());
                let checking = cancel.clone();
                let join = tokio::spawn(async move {
                    let task = store(&config)?
                        .resolve_unknown(task_id, retry, soyo_app::events::timestamp()?)
                        .await?;
                    finish_job(task, checking.is_cancelled()).await
                });
                self.begin_ui(cancel, join);
            }
            "continue" if !id.is_empty() && tail.is_empty() => {
                self.require_idle()?;
                if self.config.permissions_mode == PermissionMode::Manual {
                    return Err(text::TASK_MANUAL_UNAVAILABLE.into());
                }
                let config = self.config.clone();
                let options = self.options.personal_context.clone();
                let task_id = parse_id(id)?;
                let cancel = Arc::new(RunCancellation::new());
                let checking = cancel.clone();
                let context = TaskRunContext {
                    permissions: config.permissions_mode,
                    confirmation: None,
                    input: None,
                    approvals: None,
                    personal_context: options,
                    anthropic_prompt_cache: self.options.anthropic_prompt_cache,
                    accept_config_change: false,
                    cancellation: cancel.clone(),
                };
                let join = tokio::spawn(async move {
                    let outcome = run_task_once(&config, task_id, context).await?;
                    finish_job(outcome.task, checking.is_cancelled()).await
                });
                self.begin_ui(cancel, join);
            }
            _ => return Err(text::TASK_USAGE.into()),
        }
        Ok(())
    }
}
