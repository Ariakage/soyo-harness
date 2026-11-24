//! Explicit per-session checkpoint administration (`F-05`). The caller owns
//! scheduling, printing and exit-code mapping. Read paths hold no lock; every
//! mutating action takes the session's exclusive lock through
//! [`CheckpointAdmin::open_locked`].
use clap::Subcommand;
use serde_json::{Value, json};
pub use soyo_app::checkpoint_admin::resolve_session_id;
use soyo_app::checkpoint_admin::{CheckpointAction, CheckpointAdmin};
use soyo_core::{
    checkpoint::{CheckpointToken, RestoreOperation},
    config::Config,
    contract::{BoundaryError, ErrorInfo, SessionId},
    credentials::SecretRegistry,
};
use std::{
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Subcommand)]
pub enum CheckpointAction_ {
    /// List this session's checkpoints in revision order.
    List {
        /// Explicit session; defaults to the newest session for this workspace.
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 1)]
        page: usize,
    },
    /// Bounded unified diff between a checkpoint and the workspace files.
    Diff {
        /// Explicit session; defaults to the newest session for this workspace.
        #[arg(long)]
        session: Option<String>,
        /// Checkpoint id (`8-4-4-4-12` lowercase UUID).
        #[arg(long)]
        id: String,
        /// Checkpoint content digest (64 lowercase hex chars).
        #[arg(long)]
        sha256: String,
    },
    /// Capture explicit workspace-relative paths into a new checkpoint.
    Create {
        /// Explicit session; defaults to the newest session for this workspace.
        #[arg(long)]
        session: Option<String>,
        /// Workspace-relative file paths to capture.
        #[arg(long = "path", required = true)]
        paths: Vec<String>,
        /// Human label recorded on the checkpoint source.
        #[arg(long, default_value = "cli")]
        label: String,
    },
    /// Re-capture the files a checkpoint tracks, refreshing its contents.
    Accept {
        /// Explicit session; defaults to the newest session for this workspace.
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        id: String,
        #[arg(long)]
        sha256: String,
        #[arg(long = "path", required = true)]
        paths: Vec<String>,
        #[arg(long, default_value = "cli-accept")]
        label: String,
    },
    /// Restore selected files to a checkpoint's captured contents.
    /// Requires `--yes` when no controlling terminal is available; with a TTY
    /// each file is confirmed after showing its unified diff.
    Restore {
        /// Explicit session; defaults to the newest session for this workspace.
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        id: String,
        #[arg(long)]
        sha256: String,
        #[arg(long = "path", required = true)]
        paths: Vec<String>,
        /// Approve every restore without the interactive per-file prompt.
        #[arg(long)]
        yes: bool,
        #[arg(long, default_value = "cli-restore")]
        label: String,
    },
    /// Observe interrupted restore journals for this session (no replay).
    Recover {
        /// Explicit session; defaults to the newest session for this workspace.
        #[arg(long)]
        session: Option<String>,
    },
}
impl CheckpointAction_ {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::List { .. } => "list",
            Self::Diff { .. } => "diff",
            Self::Create { .. } => "create",
            Self::Accept { .. } => "accept",
            Self::Restore { .. } => "restore",
            Self::Recover { .. } => "recover",
        }
    }
    pub(crate) fn session(&self) -> Option<&str> {
        match self {
            Self::List { session, .. }
            | Self::Diff { session, .. }
            | Self::Create { session, .. }
            | Self::Accept { session, .. }
            | Self::Restore { session, .. }
            | Self::Recover { session } => session.as_deref(),
        }
    }
    /// Parse the `--session` flag into a [`SessionId`] for `resolve_session_id`.
    pub(crate) fn session_id(&self) -> Result<Option<SessionId>, BoundaryError> {
        self.session().map(parse_session).transpose()
    }
}

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
    if cancel.load(Ordering::Acquire) {
        return Err(failure("cancelled", "Checkpoint administration cancelled"));
    }
    Ok(())
}
fn parse_session(value: &str) -> Result<SessionId, BoundaryError> {
    soyo_app::session::parse_id(value).map_err(|_| failure("invalid_args", "Session id is invalid"))
}
fn parse_token(id: &str, sha256: &str) -> Result<CheckpointToken, BoundaryError> {
    let token = CheckpointToken {
        id: id.to_owned(),
        sha256: sha256.to_owned(),
    };
    if token.id.len() != 36
        || token.sha256.len() != 64
        || !token
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(failure(
            "invalid_args",
            "Checkpoint identity must be a lowercase UUID and SHA-256 digest",
        ));
    }
    Ok(token)
}
fn known_registry(config: &Config) -> Result<Arc<SecretRegistry>, BoundaryError> {
    let values = config
        .secret_env_names()
        .iter()
        .map(|name| config.secret(name).map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()?;
    let registry = Arc::new(SecretRegistry::default());
    registry.register(&values)?;
    Ok(registry)
}
fn safe_output(mut value: Value, secrets: &SecretRegistry) -> Result<Value, BoundaryError> {
    let known = secrets.snapshot()?;
    let encoded = serde_json::to_vec(&value).map_err(|_| {
        failure(
            "output_error",
            "Checkpoint administration output cannot be encoded",
        )
    })?;
    if encoded.len() > MAX_OUTPUT_BYTES {
        return Err(failure(
            "checkpoint_output_limit",
            "Checkpoint administration output exceeds its limit; use a smaller page",
        ));
    }
    soyo_core::redaction::redact(&mut value, &known)?;
    let encoded = serde_json::to_string(&value).map_err(|_| {
        failure(
            "output_error",
            "Checkpoint administration output cannot be encoded",
        )
    })?;
    if encoded.len() > MAX_OUTPUT_BYTES || soyo_core::redaction::contains_secret(&encoded, &known) {
        return Err(failure(
            "sensitive_data",
            "Checkpoint administration output was withheld",
        ));
    }
    Ok(value)
}

/// Mirror of `runtime_checkpoint`'s restore-target rule so the CLI approves
/// exactly the same roots the model-facing restore path is bound to: resolved
/// targets must stay inside the session's allowed write roots (here the
/// checkpoint workspace itself) and never address protected host storage.
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
/// the per-file paths the user already approved in the interactive preview;
/// the path must additionally resolve inside the session workspace (the CLI's
/// write root) and outside the store's protected directory.
fn restore_authorize(
    workspace_root: &Path,
    protected: &Path,
    approved: Option<&std::collections::BTreeSet<String>>,
) -> impl FnMut(&RestoreOperation) -> Result<(), BoundaryError> {
    let workspace_root = workspace_root.to_owned();
    let protected = protected.to_owned();
    let approved = approved.cloned();
    move |operation: &RestoreOperation| {
        if let Some(approved) = &approved {
            if !approved.contains(&operation.path) {
                return Err(failure(
                    "permission_denied",
                    "Restore was not approved for this file",
                ));
            }
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

/// Stage one mutating action up to its review. Returned to the async caller
/// so the user can approve the exact bounded review JSON before `confirm`
/// runs; `None` means the action completed in one step (list/diff/recover).
pub enum Prepared {
    /// A finished read-only or single-phase result, already safe to print.
    Done(Value),
    /// A restore awaiting the user's per-file approval. `review` is the
    /// bounded redacted JSON shown to the user; `confirm` applies it once.
    Restore {
        review: Value,
        confirm: Box<
            dyn FnOnce(Option<std::collections::BTreeSet<String>>) -> Result<Value, BoundaryError>
                + Send,
        >,
    },
}

/// Perform one explicit command. Blocking filesystem work; the caller runs it
/// on a bounded administration worker, owns signal exit codes, and resolves
/// the session id (via [`resolve_session_id`]) before dispatch. For
/// `restore` without `--yes` the returned [`Prepared::Restore`] carries the
/// review to approve; the caller passes back the approved path set.
pub fn run_cancellable(
    action: &CheckpointAction_,
    config: &Config,
    session_id: SessionId,
    cancel: &Arc<AtomicBool>,
) -> Result<Prepared, BoundaryError> {
    check_cancel(cancel)?;
    let secrets = known_registry(config)?;
    let data = match action {
        CheckpointAction_::List { page, .. } => {
            let admin = CheckpointAdmin::open(config, secrets.clone(), session_id)?;
            admin.list(*page, cancel.as_ref())?
        }
        CheckpointAction_::Diff { id, sha256, .. } => {
            let admin = CheckpointAdmin::open(config, secrets.clone(), session_id)?;
            admin.diff_view(&parse_token(id, sha256)?, cancel.as_ref())?
        }
        CheckpointAction_::Recover { .. } => {
            let admin = CheckpointAdmin::open_locked(config, secrets.clone(), session_id)?;
            let recovered = admin.recover(cancel.as_ref())?;
            json!({
                "session_id": admin.session_id().0.to_string(),
                "recovered": recovered.len(),
                "outcomes": recovered.iter().map(|outcome| json!({
                    "operation_id": outcome.operation_id,
                    "checkpoint": outcome.checkpoint,
                    "status": outcome.status,
                    "files": outcome.files.iter().map(|file| json!({
                        "path": file.operation.path,
                        "status": file.status,
                    })).collect::<Vec<_>>(),
                    "error": outcome.error,
                })).collect::<Vec<_>>(),
            })
        }
        CheckpointAction_::Create { paths, label, .. } => {
            let admin = CheckpointAdmin::open_locked(config, secrets.clone(), session_id)?;
            let prepared =
                admin.prepare(CheckpointAction::Create { paths }, label, cancel.as_ref())?;
            let value = admin.confirm(prepared, cancel.as_ref(), |_| Ok(()))?;
            return Ok(Prepared::Done(safe_output(
                json!({"schema_version":1,"kind":"checkpoint","action":"create","ok":true,"data":value}),
                &secrets,
            )?));
        }
        CheckpointAction_::Accept {
            id,
            sha256,
            paths,
            label,
            ..
        } => {
            let admin = CheckpointAdmin::open_locked(config, secrets.clone(), session_id)?;
            let token = parse_token(id, sha256)?;
            let prepared = admin.prepare(
                CheckpointAction::Accept {
                    checkpoint: &token,
                    paths,
                },
                label,
                cancel.as_ref(),
            )?;
            let value = admin.confirm(prepared, cancel.as_ref(), |_| Ok(()))?;
            return Ok(Prepared::Done(safe_output(
                json!({"schema_version":1,"kind":"checkpoint","action":"accept","ok":true,"data":value}),
                &secrets,
            )?));
        }
        CheckpointAction_::Restore {
            id,
            sha256,
            paths,
            yes,
            label,
            ..
        } => {
            let admin = CheckpointAdmin::open_locked(config, secrets.clone(), session_id)?;
            let token = parse_token(id, sha256)?;
            let prepared = admin.prepare(
                CheckpointAction::Restore {
                    checkpoint: &token,
                    paths,
                },
                label,
                cancel.as_ref(),
            )?;
            let review = prepared.review().clone();
            if *yes {
                let workspace = admin.workspace_root().clone();
                let protected = admin.protected_roots().into_iter().next().ok_or_else(|| {
                    failure(
                        "checkpoint_config",
                        "Checkpoint protected root is unavailable",
                    )
                })?;
                let value = admin.confirm(
                    prepared,
                    cancel.as_ref(),
                    restore_authorize(&workspace, &protected, None),
                )?;
                return Ok(Prepared::Done(safe_output(
                    json!({"schema_version":1,"kind":"checkpoint","action":"restore","ok":true,"data":value}),
                    &secrets,
                )?));
            }
            // Interactive path: return the bounded review; the caller collects
            // the approved path set and calls `confirm` once on its worker.
            let workspace = admin.workspace_root().clone();
            let protected = admin.protected_roots().into_iter().next().ok_or_else(|| {
                failure(
                    "checkpoint_config",
                    "Checkpoint protected root is unavailable",
                )
            })?;
            let secrets_confirm = secrets.clone();
            let cancel_confirm = cancel.clone();
            return Ok(Prepared::Restore {
                review: safe_output(review, &secrets)?,
                confirm: Box::new(move |approved| {
                    let value = admin.confirm(
                        prepared,
                        cancel_confirm.as_ref(),
                        restore_authorize(&workspace, &protected, approved.as_ref()),
                    )?;
                    safe_output(
                        json!({"schema_version":1,"kind":"checkpoint","action":"restore","ok":true,"data":value}),
                        &secrets_confirm,
                    )
                }),
            });
        }
    };
    check_cancel(cancel)?;
    Ok(Prepared::Done(safe_output(
        json!({"schema_version":1,"kind":"checkpoint","action":action.name(),"ok":true,"data":data}),
        &secrets,
    )?))
}
