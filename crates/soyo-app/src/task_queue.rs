//! Durable cross-run task queue orchestration (`F-08`).
//!
//! A Task is one persisted unit of work: submitted with an input, leased to a
//! worker, driven through exactly one Run per attempt, and resolved by an
//! explicit terminal transition. A worker crash after the dispatch intent is
//! recovered as `waiting_unknown` and never replayed automatically — only an
//! explicit `resolve` requeues or blocks the task.

use crate::{
    Launch,
    events::timestamp,
    session::{self, ResumeOptions},
    spawn_build_runtime,
};
use soyo_core::{
    approvals::Preauthorizations,
    config::Config,
    contract::{BoundaryError, ErrorInfo, RunId, RunResult, RunStatus, SCHEMA_VERSION},
    input::InputHandler,
    runtime::{ApprovalHandler, PersonalContext},
    task_queue::{
        StoreSettings, TaskId, TaskRecord, TaskRunKind, TaskRunSummary, TaskSeed, TaskStatus,
        TaskStore,
    },
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

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

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Per-attempt UI and runtime adapters supplied by the caller (CLI, TUI,
/// desktop helper). The queue owns no presentation state.
pub struct TaskRunContext {
    pub permissions: soyo_core::contract::PermissionMode,
    pub confirmation: Option<Arc<dyn ApprovalHandler>>,
    pub input: Option<Arc<dyn InputHandler>>,
    pub approvals: Option<Preauthorizations>,
    pub personal_context: PersonalContext,
    pub anthropic_prompt_cache: bool,
    pub accept_config_change: bool,
    pub cancellation: Arc<crate::interaction::RunCancellation>,
}

pub struct TaskRunOutcome {
    pub task: TaskRecord,
    pub result: RunResult,
    pub streamed: bool,
}

pub(crate) fn failed_result(
    session_id: soyo_core::contract::SessionId,
    run_id: RunId,
    error: BoundaryError,
) -> RunResult {
    RunResult {
        schema_version: SCHEMA_VERSION,
        session_id: Some(session_id),
        run_id: Some(run_id),
        status: RunStatus::Failed,
        stop_reason: Some("task_run_failed".into()),
        text: None,
        error: Some(error.0),
        usage: None,
        artifacts: vec![],
        persistence: None,
    }
}

fn task_secrets(config: &Config) -> Result<Vec<String>, BoundaryError> {
    config
        .secret_env_names()
        .iter()
        .map(|name| config.secret(name).map(str::to_owned))
        .collect()
}

fn tasks_enabled(config: &Config) -> Result<(), BoundaryError> {
    if config.tasks.enabled {
        Ok(())
    } else {
        Err(failure(
            "task_queue_disabled",
            "the persistent task queue is disabled; set [tasks] enabled = true",
        ))
    }
}

/// Open the durable task store. PLAN-mode safety is preserved: storage must
/// live outside the current workspace roots.
pub fn open_task_store(config: &Config) -> Result<TaskStore, BoundaryError> {
    tasks_enabled(config)?;
    session::check_roots(config, &config.cwd, config.mode).map_err(|_| {
        failure(
            "plan_write_denied",
            "PLAN task storage must be outside the current workspace",
        )
    })?;
    let settings = StoreSettings::default();
    TaskStore::open_with_settings(&config.data_dir, task_secrets(config)?, settings)
}

/// List every persisted ordinary Task in stable store order. Persistent
/// subagent jobs share the database but are listed through `soyo subagent`.
pub async fn list_tasks(config: &Config) -> Result<Vec<TaskRecord>, BoundaryError> {
    Ok(open_task_store(config)?
        .list()
        .await?
        .into_iter()
        .filter(|task| task.run_kind == TaskRunKind::Task)
        .collect())
}

/// Load one Task record by id.
pub async fn load_task(config: &Config, task_id: TaskId) -> Result<TaskRecord, BoundaryError> {
    open_task_store(config)?.load(task_id).await
}

/// Create a Task and its dedicated session. The task is queued, not started.
pub async fn submit_task(config: &Config, input: &str) -> Result<TaskRecord, BoundaryError> {
    tasks_enabled(config)?;
    if input.trim().is_empty() {
        return Err(failure("invalid_task", "Task input cannot be empty"));
    }
    let mut selected = config.clone();
    selected.mode = config.mode;
    let mut new = crate::new_session(&selected)
        .map_err(|_| failure("invalid_task", "Task session configuration is invalid"))?;
    new.work_mode = config.mode;
    let prepared = session::create(&selected, new)
        .await
        .map_err(|_| failure("task_session", "Task session cannot be created"))?;
    let session_id = prepared.session.id;
    let workspace = prepared.session.workspace.clone();
    drop(prepared);
    let profile = selected
        .profile
        .clone()
        .ok_or_else(|| failure("invalid_task", "Task profile is not selected"))?;
    let task = TaskRecord::new(TaskSeed {
        session_id,
        workspace,
        profile,
        mode: selected.mode,
        permissions: selected.permissions_mode,
        input: input.to_owned(),
        now: timestamp()?,
    })?;
    open_task_store(config)?.create(task).await
}

/// Operator-driven status transition. Pause is allowed from Queued, Running or
/// Blocked; resume only requeues a Paused task; cancel is allowed from any
/// non-terminal state. Unknown outcomes are never requeued here.
pub async fn transition_task(
    config: &Config,
    task_id: TaskId,
    target: TaskStatus,
) -> Result<TaskRecord, BoundaryError> {
    open_task_store(config)?
        .transition(task_id, target, timestamp()?)
        .await
}

/// Resolve a task waiting on an unknown Run outcome. `retry` requeues the task
/// for a fresh attempt; otherwise the task is blocked for external input.
/// Only explicit resolution requeues — never automatic replay.
pub async fn resolve_task(
    config: &Config,
    task_id: TaskId,
    retry: bool,
) -> Result<TaskRecord, BoundaryError> {
    open_task_store(config)?
        .resolve_unknown(task_id, retry, timestamp()?)
        .await
}

/// Mark in-flight dispatches whose worker lease expired as `unknown` and move
/// their tasks to `waiting_unknown`. Returns the recovered records.
pub async fn recover_tasks(config: &Config) -> Result<Vec<TaskRecord>, BoundaryError> {
    open_task_store(config)?
        .recover_unknown(now_ms(), timestamp()?)
        .await
}

/// Drive exactly one queued Task attempt under a durable lease. The dispatch
/// intent is recorded before the Run starts; a crash after that point is
/// recovered as unknown on the next pass rather than replayed.
pub async fn run_task_once(
    config: &Config,
    task_id: TaskId,
    mut context: TaskRunContext,
) -> Result<TaskRunOutcome, BoundaryError> {
    let store = open_task_store(config)?;
    let owner = format!("task-{}", now_ms());
    let ttl = config
        .limits
        .active_run_timeout_seconds
        .saturating_add(60)
        .max(60)
        .saturating_mul(1000);
    let lease = store
        .acquire_lease(task_id, owner, now_ms(), Some(ttl))
        .await?;
    let result = async {
        let task = store.load(task_id).await?;
        if task.run_kind != TaskRunKind::Task {
            return Err(failure(
                "invalid_task_state",
                "the durable row is a persistent subagent job; use `soyo subagent continue`",
            ));
        }
        if task.status != TaskStatus::Queued {
            return Err(failure(
                "invalid_task_state",
                "Task must be queued before running an attempt",
            ));
        }
        run_attempt(config, &store, &lease, &task, &mut context).await
    }
    .await;
    let release = store.release_lease(&lease).await;
    match (result, release) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

async fn run_attempt(
    config: &Config,
    store: &TaskStore,
    lease: &soyo_core::task_queue::TaskLease,
    task: &TaskRecord,
    context: &mut TaskRunContext,
) -> Result<TaskRunOutcome, BoundaryError> {
    let mut prepared = session::resume(
        config,
        &task.session_id.0.to_string(),
        ResumeOptions {
            accept_config_change: context.accept_config_change,
            cwd_explicit: false,
            profile_explicit: false,
            mode_override: Some(task.mode),
        },
    )
    .await
    .map_err(|_| failure("task_session", "Task session cannot be resumed"))?;
    let run_id = RunId::new();
    let now = timestamp()?;
    let dispatch = store.begin_attempt(lease, run_id, now_ms(), now).await?;
    store.mark_running(&dispatch, lease, now_ms()).await?;
    if session::append(&mut prepared, task.input.clone())
        .await
        .is_err()
    {
        return Err(failure("task_session", "Task input cannot be persisted"));
    }
    let summary = format!("Durable Task: {}", task.input);
    let (mut runtime, streamed) = spawn_build_runtime(
        config.clone(),
        prepared.session.clone(),
        summary,
        Launch {
            permissions: context.permissions,
            store: prepared.store.clone(),
            cancellation: context.cancellation.clone(),
            confirmation: context.confirmation.clone(),
            input: context.input.clone(),
            approvals: context.approvals.take().unwrap_or_default(),
            personal_context: context.personal_context.clone(),
            anthropic_prompt_cache: context.anthropic_prompt_cache,
            run_id: Some(run_id),
            secret_registry: None,
        },
        |model, _| model,
    )
    .await
    .map_err(|_| failure("task_runtime", "Task runtime cannot be assembled"))?;
    let mut session = prepared.session.clone();
    let heartbeat_store = store.clone();
    let mut heartbeat_lease = lease.clone();
    let heartbeat_ttl = lease.expires_at_ms.saturating_sub(now_ms()).max(1);
    let heartbeat_wait = Duration::from_millis((heartbeat_ttl / 3).clamp(1_000, 30_000));
    let heartbeat_error = Arc::new(Mutex::new(None::<BoundaryError>));
    let heartbeat_failure = heartbeat_error.clone();
    let heartbeat_cancellation = context.cancellation.clone();
    let heartbeat = tokio::spawn(async move {
        loop {
            tokio::time::sleep(heartbeat_wait).await;
            if let Err(error) = heartbeat_store
                .renew_lease(&mut heartbeat_lease, now_ms(), Some(heartbeat_ttl))
                .await
            {
                heartbeat_cancellation.cancel();
                if let Ok(mut failure) = heartbeat_failure.lock() {
                    *failure = Some(error);
                }
                break;
            }
        }
    });
    let control_store = store.clone();
    let control_cancellation = context.cancellation.clone();
    let control_task_id = task.id;
    let control = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            match control_store.load(control_task_id).await {
                Ok(current)
                    if matches!(current.status, TaskStatus::Paused | TaskStatus::Cancelled) =>
                {
                    control_cancellation.cancel();
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    control_cancellation.cancel();
                    break;
                }
            }
        }
    });
    let result = runtime
        .run(&mut session)
        .await
        .unwrap_or_else(|error| failed_result(session.id, run_id, error));
    let metrics = prepared
        .store
        .run_metrics(session.id)
        .await?
        .into_iter()
        .rev()
        .find(|record| record.run_id == run_id)
        .map(|record| record.metrics)
        .unwrap_or_default();
    heartbeat.abort();
    control.abort();
    let _ = heartbeat.await;
    let _ = control.await;
    if let Some(error) = heartbeat_error
        .lock()
        .ok()
        .and_then(|mut failure| failure.take())
    {
        return Err(error);
    }
    let task = store
        .finish_attempt(
            lease,
            &dispatch,
            TaskRunSummary::from_result(&result, timestamp()?)?,
            &metrics,
            timestamp()?,
        )
        .await?;
    Ok(TaskRunOutcome {
        task,
        result,
        streamed,
    })
}
