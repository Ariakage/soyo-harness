//! Explicit user-side checkpoint administration (`F-05`).
//!
//! A host-bound [`CheckpointStore`] already backs the five model tools; this
//! service reuses the same store and exposes list/diff/accept/restore/recover
//! scoped to exactly one session. Prepared actions are single-use and retain
//! approved observations; the caller owns scheduling, presentation and the
//! per-file restore authorization callback.
use serde_json::{Value, json};
use soyo_core::{
    checkpoint::{
        ChangedFiles, CheckpointDiff, CheckpointSource, CheckpointStore, CheckpointSummary,
        CheckpointToken, CheckpointWorkspace, PreparedRestore, PreparedSnapshot,
    },
    contract::{BoundaryError, ErrorInfo, RunId, SessionId, WorkMode},
    credentials::SecretRegistry,
    sqlite::{SessionLock, SqliteStore},
};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub const MAX_VIEW_BYTES: usize = 64 * 1024;
const PAGE_ITEMS: usize = 12;
const MAX_PATHS: usize = 128;

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
fn check_cancel(cancel: &AtomicBool) -> Result<(), BoundaryError> {
    if cancel.load(Ordering::SeqCst) {
        return Err(failure("cancelled", "Checkpoint administration cancelled"));
    }
    Ok(())
}
/// Lowercase canonical UUID format (`8-4-4-4-12` hex), matching the engine's
/// `valid_id` without pulling a uuid dependency into this crate.
fn valid_uuid(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        match index {
            8 | 13 | 18 | 23 => {
                if *byte != b'-' {
                    return false;
                }
            }
            _ => {
                if !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase() {
                    return false;
                }
            }
        }
    }
    true
}
fn check_token(token: &CheckpointToken) -> Result<(), BoundaryError> {
    if !valid_uuid(&token.id)
        || token.sha256.len() != 64
        || !token
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(failure(
            "invalid_args",
            "Checkpoint token identity is invalid",
        ));
    }
    Ok(())
}
fn check_paths(paths: &[String]) -> Result<(), BoundaryError> {
    if paths.is_empty() || paths.len() > MAX_PATHS {
        return Err(failure(
            "invalid_args",
            "Checkpoint path selection exceeds its bounds",
        ));
    }
    for path in paths {
        if path.is_empty()
            || path.len() > 1024
            || path.chars().any(soyo_core::text::is_terminal_control)
            || path.contains('\\')
            || path.split('/').count() > 16
            || path
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
        {
            return Err(failure("invalid_args", "Checkpoint path is invalid"));
        }
    }
    Ok(())
}

/// Resolve which session a checkpoint command applies to. `session_id` wins;
/// otherwise the most recently updated session stored for `config.cwd` is
/// selected. Storage opens poll file locks, so callers dispatch through
/// [`crate::offload`].
pub async fn resolve_session_id(
    config: &soyo_core::config::Config,
    session_id: Option<SessionId>,
) -> Result<SessionId, BoundaryError> {
    if let Some(id) = session_id {
        return Ok(id);
    }
    let workspace = std::fs::canonicalize(&config.cwd)
        .map_err(|_| failure("checkpoint_scope", "Workspace is unavailable"))?;
    let store = {
        let owned = config.clone();
        crate::offload(move || crate::session::open(&owned))
            .await
            .map_err(|error| match error.downcast::<BoundaryError>() {
                Ok(error) => *error,
                Err(error) => BoundaryError::new("storage_error", error.to_string()),
            })?
    };
    let summaries = store.list_sessions(0, 64).await?;
    summaries
        .into_iter()
        .find(|summary| summary.workspace == workspace)
        .map(|summary| summary.id)
        .ok_or_else(|| {
            failure(
                "checkpoint_scope",
                "No session is stored for this workspace; pass --session",
            )
        })
}

/// Per-session user-side administration. The session id binds every capture
/// and restore source; the lock is held only while a mutating action is
/// prepared or confirmed.
pub struct CheckpointAdmin {
    store: CheckpointStore,
    workspace: PathBuf,
    session_id: SessionId,
    mode: WorkMode,
    secrets: Arc<SecretRegistry>,
    _lock: Option<SessionLock>,
}

/// The constructor is private. A UI can display the review but cannot replace
/// the selected paths, token or captured observations after approval.
pub struct PreparedCheckpointAction {
    workspace: CheckpointWorkspace,
    operation_id: String,
    review: Value,
    action: PreparedAction,
}
enum PreparedAction {
    Snapshot(PreparedSnapshot),
    Restore(PreparedRestore),
}
impl PreparedCheckpointAction {
    pub fn review(&self) -> &Value {
        &self.review
    }
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
}

impl CheckpointAdmin {
    /// Bind the administration view to `session_id`. `write` additionally takes
    /// the exclusive session lock so restores and captures cannot race an
    /// owning run.
    fn bind(
        config: &soyo_core::config::Config,
        secrets: Arc<SecretRegistry>,
        session_id: SessionId,
        write: bool,
    ) -> Result<(Self, soyo_core::contract::Session), BoundaryError> {
        secrets.register(
            &config
                .secret_env_names()
                .iter()
                .map(|name| config.secret(name).map(str::to_owned))
                .collect::<Result<Vec<_>, _>>()?,
        )?;
        let store = CheckpointStore::open(
            config.data_dir.join("checkpoints"),
            secrets.clone(),
            soyo_core::checkpoint::CheckpointLimits::default(),
        )?;
        let session = SqliteStore::peek_session(&config.data_dir, session_id)?
            .ok_or_else(|| failure("checkpoint_scope", "Session not found"))?;
        let workspace = std::fs::canonicalize(&session.workspace)
            .map_err(|_| failure("checkpoint_scope", "Session workspace is unavailable"))?;
        let lock = write
            .then(|| SqliteStore::lock_existing(&config.data_dir, session.id))
            .transpose()?;
        Ok((
            Self {
                store,
                workspace,
                session_id: session.id,
                mode: session.work_mode,
                secrets,
                _lock: lock,
            },
            session,
        ))
    }
    /// Open a read-only administration view (list/diff/recover inspection).
    pub fn open(
        config: &soyo_core::config::Config,
        secrets: Arc<SecretRegistry>,
        session_id: SessionId,
    ) -> Result<Self, BoundaryError> {
        Self::bind(config, secrets, session_id, false).map(|(admin, _)| admin)
    }
    /// Open the view and hold the session's exclusive lock for restore,
    /// accept, create and recover writes. The lock is released on drop.
    pub fn open_locked(
        config: &soyo_core::config::Config,
        secrets: Arc<SecretRegistry>,
        session_id: SessionId,
    ) -> Result<Self, BoundaryError> {
        Self::bind(config, secrets, session_id, true).map(|(admin, _)| admin)
    }
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }
    /// Canonicalized session workspace — the CLI's restore write root.
    pub fn workspace_root(&self) -> &PathBuf {
        &self.workspace
    }
    /// Host-private roots a restore target must never resolve into.
    pub fn protected_roots(&self) -> Vec<PathBuf> {
        vec![self.store.protected_path().to_owned()]
    }
    fn workspace(&self, cancel: &AtomicBool) -> Result<CheckpointWorkspace, BoundaryError> {
        check_cancel(cancel)?;
        self.store.bind_workspace(
            &self.workspace,
            self.session_id,
            &self.protected_root_list(),
        )
    }
    /// Host-private roots that checkpoint paths must never resolve into.
    fn protected_root_list(&self) -> Vec<PathBuf> {
        self.protected_roots()
    }
    fn source(&self, label: &str) -> CheckpointSource {
        CheckpointSource {
            session_id: self.session_id,
            run_id: RunId::new(),
            revision: 0,
            label: label.into(),
        }
    }
    fn check_writable(&self) -> Result<(), BoundaryError> {
        if self.mode == WorkMode::Plan {
            return Err(failure(
                "mode_denied",
                "PLAN permits read-only checkpoint administration",
            ));
        }
        Ok(())
    }
    fn safe(&self, mut value: Value) -> Result<Value, BoundaryError> {
        let encoded = serde_json::to_vec(&value)
            .map_err(|_| failure("checkpoint_output", "Checkpoint view cannot be encoded"))?;
        if encoded.len() > MAX_VIEW_BYTES {
            return Err(failure(
                "checkpoint_view_limit",
                "Checkpoint view exceeds its bound; choose a smaller selection",
            ));
        }
        soyo_core::redaction::redact(&mut value, &self.secrets.snapshot()?)?;
        if serde_json::to_vec(&value).map_or(true, |bytes| bytes.len() > MAX_VIEW_BYTES) {
            return Err(failure(
                "checkpoint_view_limit",
                "Checkpoint view exceeds its byte limit",
            ));
        }
        Ok(value)
    }

    /// List this session's checkpoints in stable revision order.
    pub fn list(&self, page: usize, cancel: &AtomicBool) -> Result<Value, BoundaryError> {
        let workspace = self.workspace(cancel)?;
        let mut summaries = workspace.list()?;
        summaries.sort_by(|left, right| {
            left.source
                .revision
                .cmp(&right.source.revision)
                .then(left.token.id.cmp(&right.token.id))
        });
        let (start, end, page) = page_bounds(summaries.len(), page)?;
        let data = json!({
            "session_id": self.session_id.0.to_string(),
            "workspace": workspace.identity().path,
            "page": page,
            "total": summaries.len(),
            "next_page": (end < summaries.len()).then_some(page + 1),
            "checkpoints": summaries[start..end]
                .iter()
                .map(summary_json)
                .collect::<Vec<_>>(),
        });
        self.safe(data)
    }
    /// Bounded diff between one checkpoint and the observed workspace files.
    pub fn diff(
        &self,
        checkpoint: &CheckpointToken,
        cancel: &AtomicBool,
    ) -> Result<CheckpointDiff, BoundaryError> {
        check_token(checkpoint)?;
        self.workspace(cancel)?.diff(checkpoint, cancel)
    }
    /// Render a diff as a bounded, redacted JSON view.
    pub fn diff_view(
        &self,
        checkpoint: &CheckpointToken,
        cancel: &AtomicBool,
    ) -> Result<Value, BoundaryError> {
        let diff = self.diff(checkpoint, cancel)?;
        self.safe(json!({
            "checkpoint": diff.checkpoint,
            "source": diff.source,
            "preview_truncated": diff.preview_truncated,
            "files": diff.files.iter().map(|file| json!({
                "path": file.path,
                "change": file.change,
                "baseline": file.baseline,
                "current": file.current,
                "unified_diff": file.unified_diff,
                "preview_truncated": file.preview_truncated,
            })).collect::<Vec<_>>(),
        }))
    }
    /// Observe interrupted restore journals for this session without replaying
    /// any workspace write. Returns the recovered outcomes.
    pub fn recover(&self, cancel: &AtomicBool) -> Result<Vec<ChangedFiles>, BoundaryError> {
        self.check_writable()?;
        self.workspace(cancel)?.recover(cancel)
    }

    /// Stage one explicit user action. No workspace publication occurs here.
    /// `create`/`accept` capture current file bytes; `restore` only reads.
    pub fn prepare(
        &self,
        action: CheckpointAction<'_>,
        label: &str,
        cancel: &AtomicBool,
    ) -> Result<PreparedCheckpointAction, BoundaryError> {
        self.check_writable()?;
        check_cancel(cancel)?;
        if label.is_empty()
            || label.len() > 256
            || label.chars().any(soyo_core::text::is_terminal_control)
        {
            return Err(failure("invalid_args", "Checkpoint label is invalid"));
        }
        let workspace = self.workspace(cancel)?;
        let source = self.source(label);
        let operation_id = format!("op_{}", RunId::new().0.simple());
        let (action, review) = match action {
            CheckpointAction::Create { paths } => {
                check_paths(paths)?;
                let prepared = workspace.prepare_capture(paths, source, cancel)?;
                let review = self.safe(json!({
                    "operation_id": operation_id,
                    "kind": "capture",
                    "paths": prepared.paths(),
                    "observations": prepared.observations(),
                    "source": prepared.source(),
                }))?;
                (PreparedAction::Snapshot(prepared), review)
            }
            CheckpointAction::Accept { checkpoint, paths } => {
                check_token(checkpoint)?;
                check_paths(paths)?;
                let prepared = workspace.prepare_accept(checkpoint, paths, source, cancel)?;
                let review = self.safe(json!({
                    "operation_id": operation_id,
                    "kind": "accept",
                    "checkpoint": checkpoint,
                    "paths": prepared.paths(),
                    "observations": prepared.observations(),
                    "source": prepared.source(),
                }))?;
                (PreparedAction::Snapshot(prepared), review)
            }
            CheckpointAction::Restore { checkpoint, paths } => {
                check_token(checkpoint)?;
                check_paths(paths)?;
                let prepared = workspace.prepare_restore(checkpoint, paths, source, cancel)?;
                let review = self.safe(json!({
                    // The confirm token is the staged operation id; the engine
                    // journal id is kept alongside for recovery correlation.
                    "operation_id": operation_id,
                    "journal_id": prepared.operation_id(),
                    "kind": "restore",
                    "checkpoint": checkpoint,
                    "changes": prepared.changes(),
                    "source": prepared.source(),
                }))?;
                (PreparedAction::Restore(prepared), review)
            }
        };
        check_cancel(cancel)?;
        Ok(PreparedCheckpointAction {
            workspace,
            operation_id,
            review,
            action,
        })
    }
    /// Apply exactly the reviewed action once. The caller must obtain explicit
    /// user confirmation first; `authorize` runs immediately before every
    /// restored file and must reject targets it does not approve.
    pub fn confirm(
        &self,
        prepared: PreparedCheckpointAction,
        cancel: &AtomicBool,
        authorize: impl FnMut(&soyo_core::checkpoint::RestoreOperation) -> Result<(), BoundaryError>,
    ) -> Result<Value, BoundaryError> {
        self.check_writable()?;
        check_cancel(cancel)?;
        let value = match prepared.action {
            PreparedAction::Snapshot(snapshot) => {
                let summary = prepared.workspace.publish_snapshot(snapshot, cancel)?;
                json!({
                    "operation_id": prepared.operation_id,
                    "status": "captured",
                    "checkpoint": summary_json(&summary),
                })
            }
            PreparedAction::Restore(restore) => {
                let changed = prepared.workspace.apply(&restore, cancel, authorize)?;
                json!({
                    "operation_id": prepared.operation_id,
                    "status": changed.status,
                    "checkpoint": changed.checkpoint,
                    "files": changed.files.iter().map(|file| json!({
                        "path": file.operation.path,
                        "status": file.status,
                        "publication_observed": file.publication_observed,
                        "observed": file.observed,
                        "error": file.error,
                    })).collect::<Vec<_>>(),
                    "error": changed.error,
                })
            }
        };
        self.safe(value)
    }
}

/// User-selected administration action for [`CheckpointAdmin::prepare`].
pub enum CheckpointAction<'a> {
    Create {
        paths: &'a [String],
    },
    Accept {
        checkpoint: &'a CheckpointToken,
        paths: &'a [String],
    },
    Restore {
        checkpoint: &'a CheckpointToken,
        paths: &'a [String],
    },
}

/// Mirror of `runtime_checkpoint`'s restore-target rule so callers approve
/// exactly the same roots the model-facing restore path is bound to: resolved
/// targets must stay inside the session's allowed write roots (the checkpoint
/// workspace itself) and never address protected host storage.
fn resolve_future(path: &Path) -> Result<PathBuf, BoundaryError> {
    if !path.is_absolute()
        || path.as_os_str().len() > 4096
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(failure(
            "checkpoint_config",
            "Checkpoint host paths must be bounded absolute paths",
        ));
    }
    let mut parent = path.to_path_buf();
    let mut suffix = Vec::new();
    loop {
        match std::fs::canonicalize(&parent) {
            Ok(mut resolved) => {
                for name in suffix.into_iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(&parent).is_ok() {
                    return Err(failure(
                        "checkpoint_path",
                        "Checkpoint target has a dangling link",
                    ));
                }
                suffix.push(
                    parent
                        .file_name()
                        .ok_or_else(|| {
                            failure(
                                "checkpoint_path",
                                "Checkpoint path has no existing ancestor",
                            )
                        })?
                        .to_owned(),
                );
                parent.pop();
            }
            Err(_) => {
                return Err(failure(
                    "checkpoint_path",
                    "Checkpoint path cannot be resolved safely",
                ));
            }
        }
    }
}
fn path_within(path: &Path, root: &Path) -> bool {
    let fold = |path: &Path| {
        path.to_string_lossy()
            .chars()
            .flat_map(|c| c.to_lowercase())
            .collect::<String>()
    };
    let path = fold(path);
    let root = fold(root);
    let root = root.trim_end_matches('/');
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Decide whether one restore operation may write to disk. `approved` holds
/// the per-file paths the user already approved in the review step; the path
/// must additionally resolve inside the session workspace (the caller's write
/// root) and outside the store's protected directory. Pass `None` for a
/// batch-approved restore where the review itself is the approval.
pub fn restore_authorize(
    workspace_root: &Path,
    protected: &Path,
    approved: Option<&BTreeSet<String>>,
) -> impl FnMut(&soyo_core::checkpoint::RestoreOperation) -> Result<(), BoundaryError> {
    let workspace_root = workspace_root.to_owned();
    let protected = protected.to_owned();
    let approved = approved.cloned();
    move |operation: &soyo_core::checkpoint::RestoreOperation| {
        if let Some(approved) = &approved
            && !approved.contains(&operation.path)
        {
            return Err(failure(
                "permission_denied",
                "Restore was not approved for this file",
            ));
        }
        let target = workspace_root.join(&operation.path);
        let resolved = resolve_future(&target)?;
        if !path_within(&resolved, &workspace_root) {
            return Err(failure(
                "path_denied",
                "Restore target is outside the session workspace",
            ));
        }
        if path_within(&resolved, &protected) {
            return Err(failure(
                "path_denied",
                "Restore target addresses protected host storage",
            ));
        }
        Ok(())
    }
}

fn summary_json(summary: &CheckpointSummary) -> Value {
    json!({
        "token": summary.token,
        "parent": summary.parent,
        "workspace_kind": summary.workspace_kind,
        "source": summary.source,
        "files": summary.files,
        "bytes": summary.bytes,
    })
}

fn page_bounds(total: usize, page: usize) -> Result<(usize, usize, usize), BoundaryError> {
    if page == 0 {
        return Err(failure("invalid_args", "Page must be a positive number"));
    }
    let start = page
        .checked_sub(1)
        .and_then(|page| page.checked_mul(PAGE_ITEMS))
        .ok_or_else(|| failure("invalid_args", "Page exceeds its bounds"))?;
    if (total == 0 && page != 1) || (total > 0 && start >= total) {
        return Err(failure("invalid_args", "Page is outside this result set"));
    }
    Ok((start, start.saturating_add(PAGE_ITEMS).min(total), page))
}
