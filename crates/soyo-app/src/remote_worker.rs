//! Worker-side execution of a dispatched remote task (`F-13` D-113 path (c)).
//!
//! The worker runs the same runtime entry points as a local attempt
//! (`spawn_build_runtime` + one Run) against its own materialized workspace,
//! with the task's work and permission modes. It never touches the owning
//! helper's task store: the dispatch carries everything it needs.

use crate::{
    Launch, interaction::RunCancellation, new_session, session, spawn_build_runtime,
    task_queue::failed_result,
};
use soyo_core::{
    approvals::Preauthorizations,
    config::Config,
    contract::{BoundaryError, ErrorInfo, PermissionMode, RunId, RunResult, WorkMode},
    runtime::PersonalContext,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
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

/// One dispatched attempt, already materialized into `workspace`.
pub struct RemoteExecRequest {
    pub workspace: PathBuf,
    pub input: String,
    pub mode: WorkMode,
    pub permissions: PermissionMode,
    pub run_id: RunId,
}

/// Execute a dispatched task on the worker machine.
///
/// The Run uses the worker's own configuration (model profile, sandbox
/// backend, limits) and the owning side's pre-allocated Run id. Manual
/// Approval is refused: there is no interactive approver on a remote worker,
/// mirroring the helper's `task_authorization` rule.
pub async fn run_remote_attempt(
    config: &Config,
    request: RemoteExecRequest,
    cancellation: Arc<RunCancellation>,
) -> Result<RunResult, BoundaryError> {
    if request.permissions == PermissionMode::Manual {
        return Err(failure(
            "task_authorization",
            "remote execution requires auto or bypass permissions",
        ));
    }
    let workspace = std::fs::canonicalize(&request.workspace).map_err(|_| {
        failure(
            "invalid_workspace",
            "the worker task workspace is unavailable",
        )
    })?;
    let mut selected = config.clone();
    selected.cwd = workspace;
    selected.mode = request.mode;
    selected.permissions_mode = request.permissions;
    let mut session = new_session(&selected)
        .map_err(|_| failure("task_session", "remote session configuration is invalid"))?;
    session.work_mode = request.mode;
    let mut prepared = session::create(&selected, session)
        .await
        .map_err(|_| failure("task_session", "remote session cannot be created"))?;
    if session::append(&mut prepared, request.input.clone())
        .await
        .is_err()
    {
        return Err(failure(
            "task_session",
            "remote task input cannot be persisted",
        ));
    }
    let summary = format!("Remote task: {}", request.input);
    let (mut runtime, _streamed) = spawn_build_runtime(
        selected.clone(),
        prepared.session.clone(),
        summary,
        Launch {
            permissions: request.permissions,
            store: prepared.store.clone(),
            cancellation,
            confirmation: None,
            input: None,
            approvals: Preauthorizations::default(),
            personal_context: PersonalContext::default(),
            anthropic_prompt_cache: false,
            run_id: Some(request.run_id),
            secret_registry: None,
        },
        |model, _| model,
    )
    .await
    .map_err(|_| failure("task_runtime", "remote runtime cannot be assembled"))?;
    let mut session = prepared.session.clone();
    Ok(runtime
        .run(&mut session)
        .await
        .unwrap_or_else(|error| failed_result(session.id, request.run_id, error)))
}

/// Human-readable relative path used in worker logs.
pub fn display_workspace(root: &Path) -> String {
    root.display().to_string()
}
