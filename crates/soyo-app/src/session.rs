//! Durable session creation, validation, recovery and exclusive ownership.
use soyo_core::{
    boundary::Store,
    config::{Config, MediaCapabilities, Protocol},
    contract::{HistoryItem, ModelProtocol, Session, SessionId, WorkMode},
    output_store::OutputStore,
};
use soyo_core::{
    boundary::StoreChange,
    sqlite::{RecoverySummary, SessionLock, SqliteStore},
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Prepared {
    pub store: Arc<SqliteStore>,
    pub media: Arc<OutputStore>,
    pub media_capabilities: MediaCapabilities,
    pub session: Session,
    pub _lock: SessionLock,
    pub recovery: RecoverySummary,
}

pub struct ResumeOptions {
    pub accept_config_change: bool,
    pub cwd_explicit: bool,
    pub profile_explicit: bool,
    pub mode_override: Option<WorkMode>,
}

fn resolved(path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut current = path.to_path_buf();
    let mut missing = Vec::new();
    while !current.exists() {
        missing.push(
            current
                .file_name()
                .ok_or("invalid storage path")?
                .to_owned(),
        );
        current = current.parent().ok_or("invalid storage path")?.to_owned();
    }
    let mut result = std::fs::canonicalize(current)?;
    for component in missing.into_iter().rev() {
        result.push(component);
    }
    Ok(result)
}
/// Validate PLAN storage separation before opening or creating storage.
pub fn check_roots(
    config: &Config,
    workspace: &Path,
    mode: WorkMode,
) -> Result<(), Box<dyn std::error::Error>> {
    if mode == WorkMode::Plan {
        let root = std::fs::canonicalize(workspace)?;
        for path in [&config.data_dir, &config.state_dir, &config.cache_dir] {
            let location = resolved(path)?;
            if location.starts_with(&root) || root.starts_with(&location) {
                return Err("PLAN storage directories overlap the workspace".into());
            }
        }
    }
    Ok(())
}
/// Open durable storage with configured redaction. May create or migrate the database.
pub fn open(config: &Config) -> Result<Arc<SqliteStore>, Box<dyn std::error::Error>> {
    let secrets = config
        .secret_env_names()
        .iter()
        .map(|name| config.secret(name).map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Arc::new(SqliteStore::open(&config.data_dir, secrets)?))
}
fn media_store(config: &Config) -> Result<Arc<OutputStore>, Box<dyn std::error::Error>> {
    let secrets = config
        .secret_env_names()
        .iter()
        .map(|name| config.secret(name).map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()?;
    let registry = Arc::new(soyo_core::credentials::SecretRegistry::default());
    registry.register(&secrets)?;
    Ok(Arc::new(OutputStore::open(
        config.data_dir.join("outputs"),
        config.output_capture.limits,
        registry,
    )?))
}
fn capture_history(
    session: &mut Session,
    media: &OutputStore,
    capabilities: MediaCapabilities,
) -> Result<Vec<soyo_core::media::MediaCapture>, Box<dyn std::error::Error>> {
    let mut captures = Vec::new();
    for item in &mut session.history {
        let HistoryItem::UserText(text) = item else {
            continue;
        };
        if !soyo_core::media::has_media(text) {
            continue;
        }
        let capture = soyo_core::media::capture_user(text, capabilities, media, session.id)?;
        *text = capture.text.clone();
        captures.push(capture);
    }
    Ok(captures)
}
/// Parse a session identifier, returning a validation error for malformed input.
pub fn parse_id(raw: &str) -> Result<SessionId, Box<dyn std::error::Error>> {
    Ok(serde_json::from_value(serde_json::json!(raw)).map_err(|_| "invalid session ID")?)
}
/// Create durable session metadata and acquire an exclusive lock held by Prepared.
pub async fn create(
    config: &Config,
    mut session: Session,
) -> Result<Prepared, Box<dyn std::error::Error>> {
    check_roots(config, &session.workspace, session.work_mode)?;
    let profile = config
        .profiles
        .get(&session.profile)
        .ok_or("session profile not found")?;
    let media_capabilities = profile.multimodal;
    let fingerprint = profile.fingerprint()?;
    // Store opens poll file locks and capture hashing does bounded file I/O;
    // keep them on a blocking thread so the caller's executor stays responsive.
    let owned = config.clone();
    let (store, media, lock, session, captures) = crate::offload(move || {
        let store = open(&owned)?;
        let media = media_store(&owned)?;
        let lock = store.lock_session(session.id)?;
        let captures = capture_history(&mut session, &media, media_capabilities)?;
        Ok((store, media, lock, session, captures))
    })
    .await?;
    store.create_session(&session, &fingerprint).await?;
    drop(captures);
    Ok(Prepared {
        store,
        media,
        media_capabilities,
        session,
        _lock: lock,
        recovery: RecoverySummary::default(),
    })
}
fn read_candidates(
    config: &Config,
    id: SessionId,
) -> Result<Vec<soyo_core::sqlite::GuardCandidate>, Box<dyn std::error::Error>> {
    if let Some(candidates) = SqliteStore::guard_candidates(&config.data_dir, id)? {
        return Ok(candidates);
    }
    let session = SqliteStore::peek_session(&config.data_dir, id)?.ok_or("session not found")?;
    Ok(vec![soyo_core::sqlite::GuardCandidate {
        workspace: session.workspace,
        work_mode: session.work_mode,
    }])
}
/// Validate current configuration under an exclusive lock and recover interrupted runs.
/// Historical approvals are never restored. Lock release follows Prepared lifetime.
pub async fn resume(
    config: &Config,
    raw: &str,
    options: ResumeOptions,
) -> Result<Prepared, Box<dyn std::error::Error>> {
    let id = parse_id(raw)?;
    let preflight = |candidates: &[soyo_core::sqlite::GuardCandidate]| -> Result<(), Box<dyn std::error::Error>> {
        for candidate in candidates {
            let workspace = if options.cwd_explicit { &config.cwd } else { &candidate.workspace };
            check_roots(config, workspace, options.mode_override.unwrap_or(candidate.work_mode))?;
        }
        Ok(())
    };
    let candidates = {
        let owned = config.clone();
        crate::offload(move || read_candidates(&owned, id)).await?
    };
    preflight(&candidates)?;
    let (lock, locked_preview) = {
        let owned = config.clone();
        crate::offload(move || {
            let lock = SqliteStore::lock_existing(&owned.data_dir, id)?;
            let locked = read_candidates(&owned, id)?;
            Ok((lock, locked))
        })
        .await?
    };
    preflight(&locked_preview)?;
    let store = {
        let owned = config.clone();
        crate::offload(move || open(&owned)).await?
    };
    let mut session = store.load_session(id).await?.ok_or("session not found")?;
    if !locked_preview.iter().any(|candidate| {
        candidate.workspace == session.workspace && candidate.work_mode == session.work_mode
    }) {
        return Err("session guard does not match stored metadata".into());
    }
    if options.profile_explicit && config.profile.as_ref() != Some(&session.profile) {
        return Err("session profile differs; start a new session".into());
    }
    let profile = config
        .profiles
        .get(&session.profile)
        .ok_or("session profile not found")?;
    let protocol = match profile.protocol {
        Protocol::ChatCompletions => ModelProtocol::ChatCompletions,
        Protocol::Responses => ModelProtocol::Responses,
        Protocol::AnthropicMessages => ModelProtocol::AnthropicMessages,
    };
    if session.model != profile.model || session.protocol != protocol {
        return Err("session model or protocol changed; start a new session".into());
    }
    if config.routing.enabled {
        let (_, from_protocols) = soyo_core::provider::routing::inspect_incompatible_opaque_items(
            &session.history,
            &session.profile,
            protocol,
            &session.profile,
        );
        if !from_protocols.is_empty() {
            match config.routing.opaque_history {
                soyo_core::config::OpaqueHistoryPolicy::Reject => {
                    return Err(soyo_core::contract::BoundaryError::new(
                        soyo_core::contract::error_codes::HISTORY_MIGRATION_REQUIRED,
                        "session history contains incompatible opaque provider items; fork a migrated session",
                    )
                    .into());
                }
                soyo_core::config::OpaqueHistoryPolicy::RequireNewSession => {
                    return Err(soyo_core::contract::BoundaryError::new(
                        soyo_core::contract::error_codes::NEW_SESSION_REQUIRED,
                        "session history contains incompatible opaque provider items; start a new session",
                    )
                    .into());
                }
                soyo_core::config::OpaqueHistoryPolicy::MigrateCompatible => {}
            }
        }
    }
    let fingerprint = profile.fingerprint()?;
    let original = store.fingerprint(id).await?;
    if original != fingerprint && !options.accept_config_change {
        return Err("session configuration changed; use --accept-config-change".into());
    }
    let workspace = if options.cwd_explicit {
        std::fs::canonicalize(&config.cwd)?
    } else {
        std::fs::canonicalize(&session.workspace)
            .map_err(|_| "workspace is unavailable; use --cwd")?
    };
    if !options.cwd_explicit && workspace != session.workspace {
        return Err("workspace moved; use --cwd".into());
    }
    let mode = options.mode_override.unwrap_or(session.work_mode);
    check_roots(config, &workspace, mode)?;
    if original != fingerprint || session.workspace != workspace || session.work_mode != mode {
        session.workspace = workspace;
        session.work_mode = mode;
        store.update_session_config(&session, &fingerprint).await?;
    }
    if SqliteStore::guard_candidates(&config.data_dir, id)?.is_none() || locked_preview.len() > 1 {
        store
            .commit(&[StoreChange::Session(session.clone())])
            .await?;
    }
    let mut recovery = store.recover_interrupted(id).await?;
    if recovery.interrupted_runs > 0 {
        session = store.load_session(id).await?.ok_or("session not found")?;
    }
    // Observe any interrupted checkpoint restore journals bound to this
    // session. Best-effort: checkpoint recovery never blocks a resume, and
    // observed Unknown outcomes are reported, never replayed.
    recovery.checkpoint_journals = {
        let owned = config.clone();
        let workspace = session.workspace.clone();
        crate::offload(move || {
            let secrets = owned
                .secret_env_names()
                .iter()
                .map(|name| owned.secret(name).map(str::to_owned))
                .collect::<Result<Vec<_>, _>>()?;
            let registry = Arc::new(soyo_core::credentials::SecretRegistry::default());
            registry.register(&secrets)?;
            let store = soyo_core::checkpoint::CheckpointStore::open(
                owned.data_dir.join("checkpoints"),
                registry,
                soyo_core::checkpoint::CheckpointLimits::default(),
            )?;
            let bound = store.bind_workspace(&workspace, id, &[store.protected_path().to_owned()])?;
            let journals = bound.recover(&std::sync::atomic::AtomicBool::new(false))?;
            Ok::<usize, Box<dyn std::error::Error>>(journals.len())
        })
        .await
        .unwrap_or(0)
    };
    let media = {
        let owned = config.clone();
        crate::offload(move || media_store(&owned)).await?
    };
    let media_capabilities = profile.multimodal;
    Ok(Prepared {
        store,
        media,
        media_capabilities,
        session,
        _lock: lock,
        recovery,
    })
}

/// Create a brand-new forked session on the configured target profile by
/// projecting compatible semantic history and stripping incompatible/endpoint-bound
/// opaque provider items (`F-16`). Never mutates or rewrites the source session
/// and never drops executed tool calls or tool results.
pub async fn fork_migrated(
    config: &Config,
    raw: &str,
    options: ResumeOptions,
) -> Result<
    (
        Prepared,
        Option<soyo_core::provider::routing::HistoryMigrationInfo>,
    ),
    Box<dyn std::error::Error>,
> {
    let id = parse_id(raw)?;
    let store = {
        let owned = config.clone();
        crate::offload(move || open(&owned)).await?
    };
    let source = store.load_session(id).await?.ok_or("session not found")?;
    let target_profile = config
        .profile
        .as_ref()
        .ok_or("target profile not selected")?
        .clone();
    let profile = config
        .profiles
        .get(&target_profile)
        .ok_or("target profile not found")?;
    let target_protocol = match profile.protocol {
        Protocol::ChatCompletions => ModelProtocol::ChatCompletions,
        Protocol::Responses => ModelProtocol::Responses,
        Protocol::AnthropicMessages => ModelProtocol::AnthropicMessages,
    };
    let (migrated_history, migration_info) =
        soyo_core::provider::routing::project_compatible_history(
            &source.history,
            &target_profile,
            target_protocol,
            &source.profile,
            soyo_core::config::OpaqueHistoryPolicy::MigrateCompatible,
        )?;
    let workspace = if options.cwd_explicit {
        std::fs::canonicalize(&config.cwd)?
    } else {
        std::fs::canonicalize(&source.workspace)
            .map_err(|_| "workspace is unavailable; use --cwd")?
    };
    let mode = options.mode_override.unwrap_or(source.work_mode);
    check_roots(config, &workspace, mode)?;
    let fingerprint = profile.fingerprint()?;
    let forked = Session {
        id: SessionId::new(),
        workspace,
        work_mode: mode,
        profile: target_profile,
        protocol: target_protocol,
        model: profile.model.clone(),
        history: migrated_history,
    };
    let media_capabilities = profile.multimodal;
    let owned = config.clone();
    let forked_clone = forked.clone();
    let (media, lock) = crate::offload(move || {
        let media = media_store(&owned)?;
        let lock = store.lock_session(forked_clone.id)?;
        Ok((media, lock))
    })
    .await?;
    // Re-open with shared store handle used by create_session
    let store_handle = {
        let owned = config.clone();
        crate::offload(move || open(&owned)).await?
    };
    store_handle.create_session(&forked, &fingerprint).await?;
    Ok((
        Prepared {
            store: store_handle,
            media,
            media_capabilities,
            session: forked,
            _lock: lock,
            recovery: RecoverySummary::default(),
        },
        migration_info,
    ))
}
/// Persist user input, then reload the durable session. Propagates storage errors.
pub async fn append(
    prepared: &mut Prepared,
    text: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let media = prepared.media.clone();
    let capabilities = prepared.media_capabilities;
    let session_id = prepared.session.id;
    // Snapshot capture performs bounded file I/O, hashing and a durable fsync;
    // keep it off the async/UI worker so input and rendering remain responsive.
    let capture = tokio::task::spawn_blocking(move || {
        soyo_core::media::capture_user(&text, capabilities, &media, session_id)
    })
    .await
    .map_err(|_| "media capture worker failed")??;
    let item = HistoryItem::UserText(capture.text.clone());
    prepared
        .store
        .commit(&[StoreChange::History {
            session_id: prepared.session.id,
            item: item.clone(),
        }])
        .await?;
    drop(capture);
    prepared.session = prepared
        .store
        .load_session(prepared.session.id)
        .await?
        .ok_or("session not found")?;
    Ok(())
}
