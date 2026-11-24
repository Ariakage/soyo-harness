//! Task queue lifecycle RPC surface: thin adapters over the shared
//! `soyo_app::task_queue` workflows plus a daemon-owned `task_continue` job.
//!
//! Semantics mirror the CLI and TUI exactly: `continue` executes one queued
//! attempt as a background job keyed by task id. Task jobs are daemon-scoped —
//! they outlive the client connection that submitted them (durable records
//! keep lease and dispatch evidence) and are stopped only via
//! `task_pause`/`task_cancel`.

use super::peripheral::{require_config_clone, require_idle_config_clone};
use super::service::HostService;
use soyo_app::interaction::RunCancellation;
use soyo_app::task_queue::{self, TaskRunContext};
use soyo_core::{
    config::Config,
    contract::PermissionMode,
    runtime::PersonalContext,
    task_queue::{TaskId, TaskRecord, TaskStatus},
};
use soyo_ipc::redaction;
use soyo_ipc::types::{DesktopError, TaskDto, TaskJobDto, TaskRunSummaryDto};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::task::JoinHandle;

pub struct TaskJob {
    pub action: &'static str,
    pub cancel: Arc<RunCancellation>,
    pub state: Arc<StdMutex<TaskJobState>>,
    pub join: JoinHandle<()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskJobState {
    Running,
    Succeeded,
    Failed(String),
}

fn failure(code: &str, message: impl Into<String>) -> DesktopError {
    DesktopError::new(code, message.into())
}

pub(crate) fn parse_task_id(raw: &str) -> Result<TaskId, DesktopError> {
    raw.parse::<TaskId>()
        .map_err(|_| failure("invalid_args", "Task id is invalid"))
}

fn status_text(status: TaskStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{status:?}").to_lowercase())
}

fn map_task(task: &TaskRecord, running_action: Option<String>, secrets: &[String]) -> TaskDto {
    TaskDto {
        task_id: task.id.0.to_string(),
        session_id: task.session_id.0.to_string(),
        workspace: task.workspace.display().to_string(),
        profile: task.profile.clone(),
        mode: format!("{:?}", task.mode).to_lowercase(),
        permissions: format!("{:?}", task.permissions).to_lowercase(),
        input: task.input.clone(),
        status: status_text(task.status),
        failure: task.failure.as_ref().map(|error| {
            redaction::redact_str(&format!("{}: {}", error.code, error.message), secrets)
        }),
        last_run_id: task.last_run_id.map(|run| run.0.to_string()),
        last_result: task.last_result.as_ref().map(|result| TaskRunSummaryDto {
            run_id: result.run_id.0.to_string(),
            status: format!("{:?}", result.status).to_lowercase(),
            stop_reason: result
                .stop_reason
                .as_ref()
                .map(|reason| redaction::redact_str(reason, secrets)),
            text: result
                .text
                .as_ref()
                .map(|text| redaction::redact_str(text, secrets)),
            error_code: result.error_code.clone(),
            persistence_saved: result.persistence_saved,
            finished_at: result.finished_at.clone(),
        }),
        running_action,
        created_at: task.created_at.clone(),
        updated_at: task.updated_at.clone(),
    }
}

fn task_context(config: &Config, cancel: Arc<RunCancellation>) -> TaskRunContext {
    TaskRunContext {
        permissions: config.permissions_mode,
        confirmation: None,
        input: None,
        approvals: None,
        personal_context: PersonalContext::default(),
        anthropic_prompt_cache: false,
        accept_config_change: false,
        cancellation: cancel,
    }
}

impl HostService {
    fn running_task_action(&self, task_id: &str) -> Option<String> {
        self.task_jobs.lock().ok().and_then(|jobs| {
            jobs.get(task_id).and_then(|job| {
                job.state.lock().ok().and_then(|state| match &*state {
                    TaskJobState::Running => Some(job.action.to_owned()),
                    _ => None,
                })
            })
        })
    }

    pub(crate) fn map_task_with_job(&self, task: &TaskRecord, secrets: &[String]) -> TaskDto {
        map_task(
            task,
            self.running_task_action(&task.id.0.to_string()),
            secrets,
        )
    }

    /// Spawn one daemon-owned Task job. A task admits a single running job at
    /// a time; `task_busy` is returned when another action is in flight.
    fn spawn_task_job(
        &self,
        task_key: String,
        action: &'static str,
        task: impl std::future::Future<Output = Result<TaskRecord, DesktopError>> + Send + 'static,
    ) -> Result<(), DesktopError> {
        let mut jobs = self
            .task_jobs
            .lock()
            .map_err(|_| failure("task_storage", "Task job registry is unavailable"))?;
        if jobs
            .get(&task_key)
            .and_then(|job| {
                job.state
                    .lock()
                    .ok()
                    .map(|state| *state == TaskJobState::Running)
            })
            .unwrap_or(false)
        {
            return Err(failure(
                "task_busy",
                "Another Task action is already running for this Task",
            ));
        }
        jobs.retain(|_, job| {
            job.state
                .lock()
                .map(|state| *state == TaskJobState::Running)
                .unwrap_or(false)
        });
        let cancel = Arc::new(RunCancellation::new());
        let state = Arc::new(StdMutex::new(TaskJobState::Running));
        let job_state = state.clone();
        let join = tokio::spawn(async move {
            let outcome = task.await;
            if let Ok(mut guard) = job_state.lock() {
                *guard = match outcome {
                    Ok(_) => TaskJobState::Succeeded,
                    Err(error) => {
                        TaskJobState::Failed(format!("{}: {}", error.code, error.message))
                    }
                };
            }
        });
        jobs.insert(
            task_key,
            TaskJob {
                action,
                cancel,
                state,
                join,
            },
        );
        Ok(())
    }

    fn cancel_task_job(&self, task_key: &str) {
        if let Ok(jobs) = self.task_jobs.lock()
            && let Some(job) = jobs.get(task_key)
        {
            job.cancel.cancel();
        }
    }

    fn task_state_of(&self, job_state: &StdMutex<TaskJobState>) -> (String, Option<String>) {
        match job_state.lock().map(|state| state.clone()) {
            Ok(TaskJobState::Running) => ("running".into(), None),
            Ok(TaskJobState::Succeeded) => ("succeeded".into(), None),
            Ok(TaskJobState::Failed(error)) => ("failed".into(), Some(error)),
            Err(_) => ("unknown".into(), None),
        }
    }

    pub async fn task_jobs(&self) -> Result<Vec<TaskJobDto>, DesktopError> {
        let jobs = self
            .task_jobs
            .lock()
            .map_err(|_| failure("task_storage", "Task job registry is unavailable"))?;
        Ok(jobs
            .iter()
            .map(|(task_id, job)| {
                let (state, error) = self.task_state_of(&job.state);
                TaskJobDto {
                    task_id: task_id.clone(),
                    action: job.action.to_owned(),
                    state,
                    error: error.map(|text| redaction::redact_str(&text, &self.current_secrets())),
                }
            })
            .collect())
    }

    pub async fn task_list(&self) -> Result<Vec<TaskDto>, DesktopError> {
        let config = require_config_clone(self).await?;
        let secrets = self.current_secrets();
        let tasks = task_queue::list_tasks(&config)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(tasks
            .iter()
            .map(|task| self.map_task_with_job(task, &secrets))
            .collect())
    }

    pub async fn task_status(&self, task_id: &str) -> Result<TaskDto, DesktopError> {
        let config = require_config_clone(self).await?;
        let secrets = self.current_secrets();
        let task = task_queue::load_task(&config, parse_task_id(task_id)?)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_task_with_job(&task, &secrets))
    }

    pub async fn task_submit(&self, input: &str) -> Result<TaskDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        let task = task_queue::submit_task(&config, input)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_task_with_job(&task, &secrets))
    }

    async fn task_transition(
        &self,
        task_id: &str,
        status: TaskStatus,
        stop_job: bool,
    ) -> Result<TaskDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        let key = parse_task_id(task_id)?;
        if stop_job {
            self.cancel_task_job(&key.0.to_string());
        }
        let task = task_queue::transition_task(&config, key, status)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_task_with_job(&task, &secrets))
    }

    pub async fn task_pause(&self, task_id: &str) -> Result<TaskDto, DesktopError> {
        self.task_transition(task_id, TaskStatus::Paused, true)
            .await
    }

    pub async fn task_resume(&self, task_id: &str) -> Result<TaskDto, DesktopError> {
        self.task_transition(task_id, TaskStatus::Queued, false)
            .await
    }

    pub async fn task_cancel(&self, task_id: &str) -> Result<TaskDto, DesktopError> {
        self.task_transition(task_id, TaskStatus::Cancelled, true)
            .await
    }

    pub async fn task_resolve(&self, task_id: &str, retry: bool) -> Result<TaskDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        let task = task_queue::resolve_task(&config, parse_task_id(task_id)?, retry)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_task_with_job(&task, &secrets))
    }

    pub async fn task_recover(&self) -> Result<Vec<TaskDto>, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        let tasks = task_queue::recover_tasks(&config)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(tasks
            .iter()
            .map(|task| self.map_task_with_job(task, &secrets))
            .collect())
    }

    /// Drive exactly one queued attempt as a daemon job.
    pub async fn task_continue(&self, task_id: &str) -> Result<TaskDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        if config.permissions_mode == PermissionMode::Manual {
            return Err(failure(
                "task_authorization",
                "task execution requires auto or bypass permissions",
            ));
        }
        let key = parse_task_id(task_id)?;
        let task_key = key.0.to_string();
        let cancel = Arc::new(RunCancellation::new());
        let job_cancel = cancel.clone();
        let job_config = config.clone();
        self.spawn_task_job(task_key, "continue", async move {
            match task_queue::run_task_once(&job_config, key, task_context(&job_config, job_cancel))
                .await
            {
                Ok(outcome) => Ok(outcome.task),
                Err(error) => Err(failure(error.code(), error.message().to_owned())),
            }
        })?;
        let task = task_queue::load_task(&config, key)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_task_with_job(&task, &secrets))
    }
}
