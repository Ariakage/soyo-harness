//! `soyo remote-worker`: execute Runs dispatched by an owning helper
//! (`F-13` D-113 path (c), `D-117`).
//!
//! The worker connects *out* over the authenticated HTTP(S) transport, so no
//! inbound listener exists on the remote machine. It materializes the task
//! workspace from a content-addressed manifest, executes one Run with the same
//! runtime entry points as a local task attempt, and reports the result plus
//! changed blobs. The owning side never receives anything until an explicit
//! apply, and the worker never sees the owning workspace layout beyond the
//! relative paths it was sent.

use clap::Args;
use serde_json::json;
use soyo_app::interaction::RunCancellation;
use soyo_app::remote_worker::RemoteExecRequest;
use soyo_core::{
    config::Config,
    contract::{BoundaryError, ErrorInfo, RunResult},
    remote::{
        self, RemoteDispatch, RemoteFinish, RemoteHeartbeat, RemoteSyncLimits,
        RemoteWorkerRegistration, create_worker_workspace, diff_workspace, read_workspace_file,
        write_workspace_file,
    },
};
use soyo_ipc::{HelperClient, RemoteEndpoint, types::DesktopError};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Heartbeat cadence; comfortably below the owning helper's default 60 s lease.
const HEARTBEAT_SECONDS: u64 = 10;
const RECONNECT_SECONDS: u64 = 5;

#[derive(Args, Debug, Clone)]
pub struct RemoteWorkerArgs {
    /// Owning helper endpoint (`https://host:port`, or loopback `http://`).
    #[arg(long)]
    pub endpoint: String,
    /// Local directory where per-task workspaces are materialized.
    #[arg(long)]
    pub workspace: PathBuf,
    /// Stable worker id reported to the helper (default: host name).
    #[arg(long)]
    pub worker_id: Option<String>,
    /// Environment variable holding the bearer token (default SOYO_REMOTE_TOKEN).
    #[arg(long)]
    pub token_env: Option<String>,
    /// PEM root CA for a self-signed helper certificate (default SOYO_REMOTE_CA).
    #[arg(long)]
    pub ca_path: Option<PathBuf>,
    /// Poll interval in seconds while no task is queued for this worker.
    #[arg(long, default_value_t = 2)]
    pub poll_seconds: u64,
}

fn failure(code: &str, message: impl Into<String>) -> BoundaryError {
    BoundaryError(
        ErrorInfo {
            code: code.into(),
            message: message.into(),
        },
        None,
        None,
    )
}

fn from_desktop(error: DesktopError) -> BoundaryError {
    BoundaryError::new(error.code, error.message)
}

fn is_disconnected_code(code: &str) -> bool {
    matches!(
        code,
        "helper_disconnected" | "helper_io" | "helper_protocol"
    )
}

fn default_worker_id() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: gethostname writes at most `buffer.len()` bytes and sets errno on failure.
    let result =
        unsafe { libc::gethostname(buffer.as_mut_ptr().cast::<libc::c_char>(), buffer.len()) };
    if result != 0 {
        return "remote-worker".into();
    }
    let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(0);
    let host = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
    let mut id: String = host
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    id.truncate(64);
    let id = id.trim_matches('-').to_owned();
    if remote::valid_worker_id(&id) {
        id
    } else {
        "remote-worker".into()
    }
}

fn resolve_worker_id(args: &RemoteWorkerArgs) -> Result<String, BoundaryError> {
    let id = args
        .worker_id
        .clone()
        .or_else(|| std::env::var("SOYO_REMOTE_WORKER_ID").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(default_worker_id);
    if !remote::valid_worker_id(&id) {
        return Err(failure(
            "invalid_args",
            "remote worker id must be 1-128 ASCII alphanumerics, '-', '_' or '.'",
        ));
    }
    Ok(id)
}

fn resolve_token(args: &RemoteWorkerArgs) -> Result<String, BoundaryError> {
    let name = args
        .token_env
        .clone()
        .unwrap_or_else(|| "SOYO_REMOTE_TOKEN".into());
    match std::env::var(&name) {
        Ok(token) if !token.trim().is_empty() => Ok(token),
        _ => Err(failure(
            "invalid_config",
            format!("remote worker bearer token is not set in {name}"),
        )),
    }
}

fn resolve_ca(args: &RemoteWorkerArgs) -> Option<PathBuf> {
    args.ca_path.clone().or_else(|| {
        std::env::var_os("SOYO_REMOTE_CA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
}

async fn connect(
    endpoint: &RemoteEndpoint,
    token: &str,
    ca: Option<&std::path::Path>,
) -> Result<Arc<HelperClient>, DesktopError> {
    let stream = soyo_ipc::remote::connect_remote(endpoint, token, ca).await?;
    HelperClient::from_stream(stream).await
}

async fn register(client: &HelperClient, worker_id: &str) -> Result<(), DesktopError> {
    let registration = RemoteWorkerRegistration {
        schema_version: remote::REMOTE_PROTOCOL_SCHEMA_VERSION,
        worker_id: worker_id.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        sandbox_backends: available_sandbox_backends(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let _: serde_json::Value = client
        .call_typed("remote_worker_register", &registration)
        .await?;
    Ok(())
}

/// Report which native sandbox backends this worker can select. Informational
/// only: the actual backend is chosen by the worker's own configuration.
fn available_sandbox_backends() -> Vec<String> {
    #[cfg(target_os = "macos")]
    let backends = vec!["seatbelt".to_owned()];
    #[cfg(target_os = "linux")]
    let backends = if which_socat() {
        vec!["bubblewrap".to_owned(), "seccomp".to_owned()]
    } else {
        Vec::new()
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let backends = Vec::new();
    backends
}

#[cfg(target_os = "linux")]
fn which_socat() -> bool {
    std::path::Path::new("/usr/bin/socat").exists() || std::path::Path::new("/bin/socat").exists()
}

async fn report_progress(
    client: &HelperClient,
    worker_id: &str,
    dispatch: &RemoteDispatch,
    phase: &str,
    detail: Option<String>,
) {
    let _: Result<serde_json::Value, DesktopError> = client
        .call_typed(
            "remote_task_progress",
            &json!({
                "worker_id": worker_id,
                "task_id": dispatch.task_id.0.to_string(),
                "attempt": dispatch.attempt,
                "phase": phase,
                "detail": detail,
            }),
        )
        .await;
}

async fn fetch_blob(
    client: &HelperClient,
    sha256: &str,
    size: u64,
) -> Result<Vec<u8>, BoundaryError> {
    let mut data: Vec<u8> = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    while (data.len() as u64) < size {
        let chunk: remote::RemoteBlobChunk = client
            .call_typed(
                "remote_blob_get",
                &json!({"sha256": sha256, "offset": data.len()}),
            )
            .await
            .map_err(from_desktop)?;
        if chunk.total_bytes != size || chunk.next_offset <= data.len() as u64 {
            return Err(failure(
                "remote_sync_error",
                "the helper returned an inconsistent blob chunk",
            ));
        }
        data.extend(
            chunk
                .data()
                .map_err(|error| failure("remote_sync_error", error.0.message))?,
        );
    }
    if data.len() as u64 != size || remote::sha256_hex(&data) != sha256 {
        return Err(failure(
            "remote_sync_error",
            "a workspace blob failed digest verification",
        ));
    }
    Ok(data)
}

async fn upload_blob(
    client: &HelperClient,
    sha256: &str,
    bytes: &[u8],
) -> Result<(), DesktopError> {
    let _: serde_json::Value = client
        .call_typed(
            "remote_blob_begin",
            &json!({"sha256": sha256, "total_bytes": bytes.len()}),
        )
        .await?;
    let mut offset = 0usize;
    while offset < bytes.len() {
        let end = (offset + remote::REMOTE_BLOB_CHUNK_BYTES).min(bytes.len());
        let chunk = remote::RemoteBlobChunk::encode(
            sha256,
            offset as u64,
            bytes.len() as u64,
            &bytes[offset..end],
        )
        .map_err(|error| DesktopError::new(error.0.code, error.0.message))?;
        let next: u64 = client
            .call_typed(
                "remote_blob_put",
                &json!({
                    "sha256": sha256,
                    "offset": offset,
                    "data_b64": chunk.data_b64,
                }),
            )
            .await?;
        if next != end as u64 {
            return Err(DesktopError::new(
                "remote_sync_error",
                "the helper acknowledged the wrong blob offset",
            ));
        }
        offset = end;
    }
    let _: serde_json::Value = client
        .call_typed(
            "remote_blob_finish",
            &json!({"sha256": sha256, "total_bytes": bytes.len()}),
        )
        .await?;
    Ok(())
}

fn spawn_heartbeat(
    client: Arc<HelperClient>,
    worker_id: String,
    dispatch: &RemoteDispatch,
    cancel: Arc<RunCancellation>,
) -> tokio::task::JoinHandle<()> {
    let task_id = dispatch.task_id.0.to_string();
    let attempt = dispatch.attempt;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECONDS));
        interval.tick().await;
        loop {
            interval.tick().await;
            let heartbeat: Result<RemoteHeartbeat, DesktopError> = client
                .call_typed(
                    "remote_task_heartbeat",
                    &json!({
                        "worker_id": worker_id,
                        "task_id": task_id,
                        "attempt": attempt,
                    }),
                )
                .await;
            match heartbeat {
                Ok(heartbeat) if heartbeat.cancel => {
                    cancel.cancel();
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    // A lost lease or a broken connection means the owning side
                    // can no longer confirm this attempt; stop instead of
                    // running an unowned side effect.
                    cancel.cancel();
                    break;
                }
            }
        }
    })
}

fn finish_report(
    worker_id: &str,
    dispatch: &RemoteDispatch,
    result: &RunResult,
    changed_files: Vec<remote::RemoteFileChange>,
) -> RemoteFinish {
    RemoteFinish {
        schema_version: remote::REMOTE_PROTOCOL_SCHEMA_VERSION,
        worker_id: worker_id.to_owned(),
        task_id: dispatch.task_id,
        attempt: dispatch.attempt,
        run_id: dispatch.run_id,
        status: result.status,
        stop_reason: result.stop_reason.clone(),
        text: result.text.clone(),
        error_code: result.error.as_ref().map(|error| error.code.clone()),
        persistence_saved: result
            .persistence
            .as_ref()
            .is_some_and(|persistence| persistence.saved),
        summary: None,
        changed_files,
    }
}

async fn execute_dispatch(
    client: &Arc<HelperClient>,
    config: &Config,
    workspace_root: &std::path::Path,
    worker_id: &str,
    dispatch: &RemoteDispatch,
    current: &Arc<StdMutex<Option<Arc<RunCancellation>>>>,
) -> Result<(), BoundaryError> {
    let limits = RemoteSyncLimits {
        max_file_bytes: dispatch.max_file_bytes,
        max_changed_files: dispatch.max_changed_files,
        ..RemoteSyncLimits::default()
    };
    report_progress(
        client,
        worker_id,
        dispatch,
        "syncing",
        Some(format!("{} workspace files", dispatch.workspace.len())),
    )
    .await;
    let workspace = create_worker_workspace(workspace_root, dispatch.task_id, dispatch.attempt)?;
    let mut base = std::collections::BTreeMap::new();
    for entry in &dispatch.workspace {
        let bytes = fetch_blob(client, &entry.sha256, entry.bytes).await?;
        write_workspace_file(
            &workspace,
            &entry.path,
            &bytes,
            entry.executable,
            limits.max_file_bytes,
        )?;
        base.insert(entry.path.clone(), entry.clone());
    }

    report_progress(client, worker_id, dispatch, "running", None).await;
    client
        .call_typed::<_, serde_json::Value>(
            "remote_task_start",
            &json!({
                "worker_id": worker_id,
                "task_id": dispatch.task_id.0.to_string(),
                "attempt": dispatch.attempt,
            }),
        )
        .await
        .map_err(from_desktop)?;

    let cancel = Arc::new(RunCancellation::new());
    if let Ok(mut slot) = current.lock() {
        *slot = Some(cancel.clone());
    }
    let heartbeat = spawn_heartbeat(
        client.clone(),
        worker_id.to_owned(),
        dispatch,
        cancel.clone(),
    );
    let result = soyo_app::remote_worker::run_remote_attempt(
        config,
        RemoteExecRequest {
            workspace: workspace.clone(),
            input: dispatch.input.clone(),
            mode: dispatch.mode,
            permissions: dispatch.permissions,
            run_id: dispatch.run_id,
        },
        cancel,
    )
    .await;
    heartbeat.abort();
    let _ = heartbeat.await;
    if let Ok(mut slot) = current.lock() {
        *slot = None;
    }
    let result = result?;

    report_progress(client, worker_id, dispatch, "finishing", None).await;
    let changes = diff_workspace(&workspace, &base, &[], &limits)?;
    for change in &changes {
        let Some(digest) = change.new_sha256.as_deref() else {
            continue;
        };
        let (bytes, entry) = read_workspace_file(&workspace, &change.path, limits.max_file_bytes)?;
        if entry.sha256 != digest {
            return Err(failure(
                "remote_sync_error",
                format!("`{}` changed while it was being uploaded", change.path),
            ));
        }
        upload_blob(client, digest, &bytes)
            .await
            .map_err(from_desktop)?;
    }
    let report = finish_report(worker_id, dispatch, &result, changes);
    let _: serde_json::Value = client
        .call_typed("remote_task_finish", &report)
        .await
        .map_err(from_desktop)?;
    Ok(())
}

async fn work_loop(
    client: &Arc<HelperClient>,
    config: &Config,
    workspace_root: &std::path::Path,
    poll_seconds: u64,
    worker_id: &str,
    interrupt: &Arc<AtomicBool>,
    current: &Arc<StdMutex<Option<Arc<RunCancellation>>>>,
) -> Result<(), BoundaryError> {
    while !interrupt.load(Ordering::Relaxed) {
        let claim: Option<RemoteDispatch> = client
            .call_typed("remote_task_claim", &json!({"worker_id": worker_id}))
            .await
            .map_err(from_desktop)?;
        match claim {
            Some(dispatch) => {
                if interrupt.load(Ordering::Relaxed) {
                    // Leave the unstarted dispatch to its lease: the owning
                    // side returns it to the queue without replaying anything.
                    return Ok(());
                }
                eprintln!(
                    "soyo remote-worker {worker_id}: task {} attempt {}",
                    dispatch.task_id.0, dispatch.attempt
                );
                match execute_dispatch(
                    client,
                    config,
                    workspace_root,
                    worker_id,
                    &dispatch,
                    current,
                )
                .await
                {
                    Ok(()) => eprintln!(
                        "soyo remote-worker {worker_id}: task {} finished",
                        dispatch.task_id.0
                    ),
                    Err(error) => {
                        if is_disconnected_code(&error.0.code) {
                            return Err(error);
                        }
                        eprintln!(
                            "soyo remote-worker {worker_id}: task {} stopped: {}: {}",
                            dispatch.task_id.0, error.0.code, error.0.message
                        );
                    }
                }
            }
            None => {
                tokio::time::sleep(Duration::from_secs(poll_seconds.max(1))).await;
            }
        }
    }
    Ok(())
}

fn spawn_interrupt(
    interrupt: Arc<AtomicBool>,
    current: Arc<StdMutex<Option<Arc<RunCancellation>>>>,
) {
    tokio::spawn(async move {
        let mut interrupted = false;
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            if interrupted {
                std::process::exit(130);
            }
            interrupted = true;
            interrupt.store(true, Ordering::Relaxed);
            if let Ok(slot) = current.lock()
                && let Some(cancel) = slot.as_ref()
            {
                cancel.cancel();
            }
            eprintln!("soyo remote-worker: interrupt received; stopping after the current step");
        }
    });
}

pub async fn run(args: &RemoteWorkerArgs, config: &Config) -> Result<u8, BoundaryError> {
    let endpoint = RemoteEndpoint::parse(&args.endpoint).map_err(from_desktop)?;
    let token = resolve_token(args)?;
    let ca = resolve_ca(args);
    let worker_id = resolve_worker_id(args)?;
    std::fs::create_dir_all(&args.workspace)
        .map_err(|_| failure("invalid_workspace", "worker workspace cannot be created"))?;
    let workspace_root = std::fs::canonicalize(&args.workspace)
        .map_err(|_| failure("invalid_workspace", "worker workspace is unavailable"))?;

    let interrupt = Arc::new(AtomicBool::new(false));
    let current: Arc<StdMutex<Option<Arc<RunCancellation>>>> = Arc::new(StdMutex::new(None));
    spawn_interrupt(interrupt.clone(), current.clone());
    eprintln!(
        "soyo remote-worker {worker_id} → {} (workspace {})",
        endpoint.url(),
        workspace_root.display()
    );
    loop {
        if interrupt.load(Ordering::Relaxed) {
            return Ok(130);
        }
        let client = match connect(&endpoint, &token, ca.as_deref()).await {
            Ok(client) => client,
            Err(error) => {
                if error.code == "helper_unauthorized" {
                    eprintln!(
                        "soyo remote-worker: helper rejected the bearer token: {}",
                        error.message
                    );
                    return Ok(1);
                }
                eprintln!("soyo remote-worker: connect failed: {}", error.message);
                tokio::time::sleep(Duration::from_secs(RECONNECT_SECONDS)).await;
                continue;
            }
        };
        if let Err(error) = register(&client, &worker_id).await {
            if error.code == "helper_unauthorized" || error.code == "unknown_method" {
                eprintln!(
                    "soyo remote-worker: register failed: {}: {}",
                    error.code, error.message
                );
                return Ok(1);
            }
            eprintln!(
                "soyo remote-worker: register failed: {}; retrying",
                error.message
            );
            tokio::time::sleep(Duration::from_secs(RECONNECT_SECONDS)).await;
            continue;
        }
        match work_loop(
            &client,
            config,
            &workspace_root,
            args.poll_seconds,
            &worker_id,
            &interrupt,
            &current,
        )
        .await
        {
            Ok(()) => return Ok(130),
            Err(error) => {
                if interrupt.load(Ordering::Relaxed) {
                    return Ok(130);
                }
                eprintln!(
                    "soyo remote-worker: helper connection lost ({}); reconnecting",
                    error.0.message
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}
