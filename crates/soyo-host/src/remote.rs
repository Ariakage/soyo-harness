//! Remote worker orchestration (`F-13` D-113 path (c), `D-117`).
//!
//! A `soyo remote-worker` process connects *out* over the authenticated
//! HTTP(S) transport, registers a stable id, and claims tasks that an operator
//! explicitly assigned to it. The owning helper keeps the `task_*` rows as the
//! single source of truth: the claim acquires the ordinary worker lease and
//! records a dispatch intent, the worker confirms the Run started before any
//! execution, and the terminal report goes through the same task state
//! machine as a local attempt. Workspace inputs travel as a content-addressed
//! manifest plus blobs; changed files are held back until an explicit
//! `remote_task_apply` passes the three-way check.
//!
//! The worker registry below is helper-process-local: it carries liveness and
//! progress only, so a daemon restart loses it and the worker re-registers.
//! Task binding, dispatch evidence and leases stay durable in the task store.

use super::peripheral::require_config_clone;
use super::service::HostService;
use super::task::parse_task_id;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use soyo_app::task_queue;
use soyo_core::{
    config::Config,
    contract::{BoundaryError, PermissionMode, RunId, error_codes},
    metrics::RunMetrics,
    remote::{
        self, MAX_REMOTE_DISPATCH_BYTES, MAX_REMOTE_PROGRESS_BYTES, MAX_REMOTE_SUMMARY_BYTES,
        MAX_REMOTE_WORKERS, REMOTE_BLOB_CHUNK_BYTES, REMOTE_PROTOCOL_SCHEMA_VERSION,
        RemoteApplyReceipt, RemoteBlobChunk, RemoteConflict, RemoteDispatch, RemoteFileEntry,
        RemoteFinish, RemoteHeartbeat, RemoteSyncLimits, RemoteSyncStore, RemoteTaskResult,
        RemoteWorkerInfo, RemoteWorkerProgress, RemoteWorkerRegistration,
    },
    task_queue::{
        TaskDispatch, TaskId, TaskRecord, TaskRunKind, TaskRunSummary, TaskStatus, TaskStore,
        timestamp,
    },
};
use soyo_ipc::types::{DesktopError, TaskDto};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex as StdMutex},
    time::{SystemTime, UNIX_EPOCH},
};

/// Default claim lease; renewed by worker heartbeats while the Run is alive.
const DEFAULT_REMOTE_LEASE_MS: u64 = 60_000;
const MIN_REMOTE_LEASE_MS: u64 = 1_000;
const MAX_REMOTE_LEASE_MS: u64 = 600_000;
const MAX_TASK_RESULT_BYTES: usize = 64 * 1024;
const MAX_STOP_REASON_BYTES: usize = 1024;

/// Registry entry: the public view plus the lease TTL used by heartbeats.
#[derive(Debug, Clone)]
pub struct RemoteWorkerState {
    pub info: RemoteWorkerInfo,
    pub lease_ttl_ms: u64,
}

pub type RemoteWorkerRegistry = Arc<StdMutex<BTreeMap<String, RemoteWorkerState>>>;

pub fn new_worker_registry() -> RemoteWorkerRegistry {
    Arc::new(StdMutex::new(BTreeMap::new()))
}

/// Outcome of `remote_task_apply` (files are written only when `applied`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteApplyReport {
    pub applied: bool,
    #[serde(default)]
    pub receipts: Vec<RemoteApplyReceipt>,
    #[serde(default)]
    pub conflicts: Vec<RemoteConflict>,
}

/// Task view plus its held-back remote result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteTaskShow {
    pub task: TaskDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<RemoteTaskResult>,
}

fn failure(code: &str, message: impl Into<String>) -> DesktopError {
    DesktopError::new(code, message.into())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn lease_owner(worker_id: &str) -> String {
    format!("remote:{worker_id}")
}

fn boundary_error(service: &HostService, error: BoundaryError) -> DesktopError {
    DesktopError::from_boundary(&error, &service.current_secrets())
}

fn truncate(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    value.truncate(max_bytes);
    while !value.is_char_boundary(value.len()) {
        value.pop();
    }
    value
}

fn valid_phase(phase: &str) -> bool {
    !phase.is_empty()
        && phase.len() <= 32
        && phase
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

impl HostService {
    fn touch_worker(&self, worker_id: &str) -> Result<(), DesktopError> {
        let now = timestamp().map_err(|error| boundary_error(self, error))?;
        let mut registry = self
            .remote_workers
            .lock()
            .map_err(|_| failure("state_lock", "remote worker registry is unavailable"))?;
        let state = registry.get_mut(worker_id).ok_or_else(|| {
            failure(
                "remote_worker_unknown",
                "register the remote worker before claiming tasks",
            )
        })?;
        state.info.last_seen_at = now;
        Ok(())
    }

    fn worker_lease_ttl(&self, worker_id: &str) -> Result<u64, DesktopError> {
        let registry = self
            .remote_workers
            .lock()
            .map_err(|_| failure("state_lock", "remote worker registry is unavailable"))?;
        Ok(registry
            .get(worker_id)
            .map(|state| state.lease_ttl_ms)
            .unwrap_or(DEFAULT_REMOTE_LEASE_MS))
    }

    fn record_progress(
        &self,
        worker_id: &str,
        task_id: TaskId,
        attempt: u32,
        phase: &str,
        detail: Option<String>,
    ) -> Result<(), DesktopError> {
        let now = timestamp().map_err(|error| boundary_error(self, error))?;
        let secrets = self.current_secrets();
        let mut registry = self
            .remote_workers
            .lock()
            .map_err(|_| failure("state_lock", "remote worker registry is unavailable"))?;
        let state = registry.get_mut(worker_id).ok_or_else(|| {
            failure(
                "remote_worker_unknown",
                "register the remote worker before reporting progress",
            )
        })?;
        state.info.last_seen_at = now.clone();
        state.info.progress = Some(RemoteWorkerProgress {
            task_id: task_id.0.to_string(),
            attempt,
            phase: phase.to_owned(),
            detail: detail.map(|value| {
                truncate(
                    soyo_ipc::redaction::redact_str(&value, &secrets),
                    MAX_REMOTE_PROGRESS_BYTES,
                )
            }),
            at: now,
        });
        Ok(())
    }

    async fn open_remote(service: &HostService) -> Result<(Config, TaskStore), DesktopError> {
        let config = require_config_clone(service).await?;
        let store = task_queue::open_task_store(&config)
            .map_err(|error| DesktopError::from_boundary(&error, &service.current_secrets()))?;
        Ok((config, store))
    }

    fn protected_paths(config: &Config) -> Vec<std::path::PathBuf> {
        vec![
            config.data_dir.clone(),
            config.state_dir.clone(),
            config.cache_dir.clone(),
        ]
    }

    /// Register (or refresh) a worker identity. Registration is cheap and
    /// idempotent; the registry only carries liveness and progress.
    pub async fn remote_worker_register(
        &self,
        registration: RemoteWorkerRegistration,
    ) -> Result<RemoteWorkerInfo, DesktopError> {
        registration
            .validate()
            .map_err(|error| boundary_error(self, error))?;
        let now = timestamp().map_err(|error| boundary_error(self, error))?;
        let mut registry = self
            .remote_workers
            .lock()
            .map_err(|_| failure("state_lock", "remote worker registry is unavailable"))?;
        let existing = registry.get(registration.worker_id.as_str());
        if existing.is_none() && registry.len() >= MAX_REMOTE_WORKERS {
            return Err(failure(
                "remote_workers_full",
                "the helper remote worker registry is full",
            ));
        }
        let registered_at = existing
            .map(|state| state.info.registered_at.clone())
            .unwrap_or_else(|| now.clone());
        let progress = existing.and_then(|state| state.info.progress.clone());
        let info = RemoteWorkerInfo {
            worker_id: registration.worker_id.clone(),
            arch: registration.arch,
            sandbox_backends: registration.sandbox_backends,
            version: registration.version,
            registered_at,
            last_seen_at: now,
            progress,
        };
        let lease_ttl_ms = existing
            .map(|state| state.lease_ttl_ms)
            .unwrap_or(DEFAULT_REMOTE_LEASE_MS);
        registry.insert(
            registration.worker_id,
            RemoteWorkerState {
                info: info.clone(),
                lease_ttl_ms,
            },
        );
        Ok(info)
    }

    pub async fn remote_workers(&self) -> Result<Vec<RemoteWorkerInfo>, DesktopError> {
        let registry = self
            .remote_workers
            .lock()
            .map_err(|_| failure("state_lock", "remote worker registry is unavailable"))?;
        Ok(registry.values().map(|state| state.info.clone()).collect())
    }

    /// Bind a queued task to a worker (or clear the binding).
    pub async fn remote_task_assign(
        &self,
        task_id: &str,
        worker_id: Option<&str>,
    ) -> Result<TaskDto, DesktopError> {
        let (_, store) = Self::open_remote(self).await?;
        let secrets = self.current_secrets();
        let key = parse_task_id(task_id)?;
        if let Some(worker_id) = worker_id
            && !remote::valid_worker_id(worker_id)
        {
            return Err(failure(
                "invalid_args",
                "Remote worker identifier is invalid",
            ));
        }
        let task = store
            .load(key)
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        if task.permissions == PermissionMode::Manual {
            return Err(failure(
                "task_authorization",
                "remote execution requires auto or bypass permissions",
            ));
        }
        let now = timestamp().map_err(|error| boundary_error(self, error))?;
        let task = store
            .assign_worker(key, worker_id.map(str::to_owned), now)
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        Ok(self.map_task_with_job(&task, &secrets))
    }

    /// Claim the oldest queued task assigned to `worker_id`.
    ///
    /// The claim acquires the durable worker lease and records a dispatch
    /// intent, but never marks the attempt running: the worker confirms the
    /// start (`remote_task_start`) after it materialized the workspace. An
    /// intent whose lease expires is therefore provably side-effect free.
    pub async fn remote_task_claim(
        &self,
        worker_id: &str,
        lease_ttl_ms: Option<u64>,
    ) -> Result<Option<RemoteDispatch>, DesktopError> {
        if !remote::valid_worker_id(worker_id) {
            return Err(failure(
                "invalid_args",
                "Remote worker identifier is invalid",
            ));
        }
        let ttl = match lease_ttl_ms {
            Some(ttl) if (MIN_REMOTE_LEASE_MS..=MAX_REMOTE_LEASE_MS).contains(&ttl) => ttl,
            Some(_) => {
                return Err(failure(
                    "invalid_args",
                    format!(
                        "lease_ttl_ms must be between {MIN_REMOTE_LEASE_MS} and {MAX_REMOTE_LEASE_MS}"
                    ),
                ));
            }
            None => DEFAULT_REMOTE_LEASE_MS,
        };
        self.touch_worker(worker_id)?;
        let (config, store) = Self::open_remote(self).await?;
        let secrets = self.current_secrets();
        // Recover this worker's own abandoned dispatches first: an unconfirmed
        // intent becomes claimable again, while a confirmed start stays
        // `waiting_unknown` for an explicit resolution.
        let now = timestamp().map_err(|error| boundary_error(self, error))?;
        store
            .recover_unknown_for(Some(worker_id), now_ms(), now)
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        let sync_store = RemoteSyncStore::open(&config.data_dir)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        let limits = RemoteSyncLimits::default();
        let protected = Self::protected_paths(&config);
        let mut tasks = store
            .list()
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        tasks.retain(|task| {
            task.run_kind == TaskRunKind::Task
                && task.status == TaskStatus::Queued
                && task.remote_worker.as_deref() == Some(worker_id)
                && task.permissions != PermissionMode::Manual
        });
        tasks.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.0.cmp(&right.id.0))
        });
        for task in tasks {
            let lease = match store
                .acquire_lease(task.id, lease_owner(worker_id), now_ms(), Some(ttl))
                .await
            {
                Ok(lease) => lease,
                Err(error) if error.0.code == "task_leased" => continue,
                Err(error) => return Err(DesktopError::from_boundary(&error, &secrets)),
            };
            let run_id = RunId::new();
            let manifest = match self
                .prepare_dispatch_manifest(&sync_store, &task, &protected, &limits)
                .await
            {
                Ok(manifest) => manifest,
                Err(error) => {
                    let _ = store.release_lease(&lease).await;
                    return Err(error);
                }
            };
            let dispatch = RemoteDispatch {
                schema_version: REMOTE_PROTOCOL_SCHEMA_VERSION,
                task_id: task.id,
                attempt: 0,
                run_id,
                input: task.input.clone(),
                mode: task.mode,
                permissions: task.permissions,
                workspace: manifest.values().cloned().collect(),
                max_file_bytes: limits.max_file_bytes,
                max_changed_files: limits.max_changed_files,
            };
            let encoded = serde_json::to_vec(&dispatch)
                .map_err(|_| failure("internal_error", "remote dispatch cannot be encoded"))?;
            if encoded.len() > MAX_REMOTE_DISPATCH_BYTES {
                let _ = store.release_lease(&lease).await;
                return Err(failure(
                    error_codes::REMOTE_SYNC_LIMIT,
                    "workspace manifest exceeds the dispatch byte bound; reduce the workspace",
                ));
            }
            let now = timestamp().map_err(|error| boundary_error(self, error))?;
            let dispatch = match store.begin_attempt(&lease, run_id, now_ms(), now).await {
                Ok(recorded) => RemoteDispatch {
                    attempt: recorded.attempt,
                    ..dispatch
                },
                Err(error) => {
                    let _ = store.release_lease(&lease).await;
                    if error.0.code == "invalid_task_state" {
                        continue;
                    }
                    return Err(DesktopError::from_boundary(&error, &secrets));
                }
            };
            let _ = self.record_progress(
                worker_id,
                dispatch.task_id,
                dispatch.attempt,
                "dispatched",
                Some(format!("{} workspace files", dispatch.workspace.len())),
            );
            return Ok(Some(dispatch));
        }
        Ok(None)
    }

    async fn prepare_dispatch_manifest(
        &self,
        sync_store: &RemoteSyncStore,
        task: &TaskRecord,
        protected: &[std::path::PathBuf],
        limits: &RemoteSyncLimits,
    ) -> Result<BTreeMap<String, RemoteFileEntry>, DesktopError> {
        let manifest = remote::scan_workspace(&task.workspace, protected, limits)
            .map_err(|error| boundary_error(self, error))?;
        for entry in manifest.values() {
            if sync_store.has_blob(&entry.sha256) {
                continue;
            }
            let (bytes, actual) =
                remote::read_workspace_file(&task.workspace, &entry.path, limits.max_file_bytes)
                    .map_err(|error| boundary_error(self, error))?;
            if actual.sha256 != entry.sha256 {
                return Err(failure(
                    error_codes::CONFLICT,
                    "the task workspace changed while it was being dispatched",
                ));
            }
            sync_store
                .put_blob(&entry.sha256, &bytes)
                .map_err(|error| boundary_error(self, error))?;
        }
        sync_store
            .store_manifest(task.id, &manifest)
            .map_err(|error| boundary_error(self, error))?;
        Ok(manifest)
    }

    /// Confirm the Run is starting after the workspace was materialized. This
    /// is the side-effect boundary: only a confirmed start can end as
    /// `waiting_unknown` when its lease expires.
    pub async fn remote_task_start(
        &self,
        worker_id: &str,
        task_id: &str,
        attempt: u32,
    ) -> Result<(), DesktopError> {
        if !remote::valid_worker_id(worker_id) {
            return Err(failure(
                "invalid_args",
                "Remote worker identifier is invalid",
            ));
        }
        if attempt == 0 {
            return Err(failure("invalid_args", "attempt must be positive"));
        }
        let key = parse_task_id(task_id)?;
        let (_, store) = Self::open_remote(self).await?;
        let secrets = self.current_secrets();
        let task = store
            .load(key)
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        if task.remote_worker.as_deref() != Some(worker_id) {
            return Err(failure(
                "invalid_args",
                "task is not assigned to this remote worker",
            ));
        }
        let run_id = task
            .last_run_id
            .ok_or_else(|| failure("invalid_task_state", "task has no dispatched Run to start"))?;
        let lease = store
            .lease(key)
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?
            .filter(|lease| lease.owner == lease_owner(worker_id))
            .ok_or_else(|| failure("task_lease_lost", "remote worker lease is missing"))?;
        let dispatch = TaskDispatch {
            task_id: key,
            attempt,
            run_id,
        };
        store
            .mark_running(&dispatch, &lease, now_ms())
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        self.record_progress(worker_id, key, attempt, "running", None)?;
        Ok(())
    }

    /// Renew the claim lease and answer whether the owning side wants the Run
    /// cancelled (pause or cancel was requested).
    pub async fn remote_task_heartbeat(
        &self,
        worker_id: &str,
        task_id: &str,
        attempt: u32,
    ) -> Result<RemoteHeartbeat, DesktopError> {
        if !remote::valid_worker_id(worker_id) {
            return Err(failure(
                "invalid_args",
                "Remote worker identifier is invalid",
            ));
        }
        let key = parse_task_id(task_id)?;
        let (_, store) = Self::open_remote(self).await?;
        let secrets = self.current_secrets();
        let mut lease = store
            .lease(key)
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?
            .filter(|lease| lease.owner == lease_owner(worker_id))
            .ok_or_else(|| failure("task_lease_lost", "remote worker lease is missing"))?;
        let ttl = self.worker_lease_ttl(worker_id)?;
        store
            .renew_lease(&mut lease, now_ms(), Some(ttl))
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        let task = store
            .load(key)
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        self.touch_worker(worker_id)?;
        let _ = attempt;
        Ok(RemoteHeartbeat {
            renewed: true,
            cancel: matches!(task.status, TaskStatus::Paused | TaskStatus::Cancelled),
        })
    }

    /// Record a short progress phase for `remote workers` display.
    pub async fn remote_task_progress(
        &self,
        worker_id: &str,
        task_id: &str,
        attempt: u32,
        phase: &str,
        detail: Option<String>,
    ) -> Result<(), DesktopError> {
        if !remote::valid_worker_id(worker_id) {
            return Err(failure(
                "invalid_args",
                "Remote worker identifier is invalid",
            ));
        }
        if !valid_phase(phase) {
            return Err(failure("invalid_args", "progress phase is invalid"));
        }
        let key = parse_task_id(task_id)?;
        self.record_progress(worker_id, key, attempt, phase, detail)
    }

    pub async fn remote_blob_get(
        &self,
        sha256: &str,
        offset: u64,
    ) -> Result<RemoteBlobChunk, DesktopError> {
        let (config, _) = Self::open_remote(self).await?;
        let secrets = self.current_secrets();
        let store = RemoteSyncStore::open(&config.data_dir)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        let (bytes, total) = store
            .read_blob_range(sha256, offset, REMOTE_BLOB_CHUNK_BYTES)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        RemoteBlobChunk::encode(sha256, offset, total, &bytes)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))
    }

    pub async fn remote_blob_begin(
        &self,
        sha256: &str,
        total_bytes: u64,
    ) -> Result<(), DesktopError> {
        let (config, _) = Self::open_remote(self).await?;
        let secrets = self.current_secrets();
        let store = RemoteSyncStore::open(&config.data_dir)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        store
            .begin_blob(sha256, total_bytes)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))
    }

    pub async fn remote_blob_put(
        &self,
        sha256: &str,
        offset: u64,
        data_b64: &str,
    ) -> Result<u64, DesktopError> {
        let (config, _) = Self::open_remote(self).await?;
        let secrets = self.current_secrets();
        let bytes = STANDARD
            .decode(data_b64)
            .map_err(|_| failure("invalid_args", "blob chunk is not valid base64"))?;
        if bytes.len() > REMOTE_BLOB_CHUNK_BYTES {
            return Err(failure(
                error_codes::REMOTE_SYNC_LIMIT,
                "blob chunk exceeds the transfer bound",
            ));
        }
        let store = RemoteSyncStore::open(&config.data_dir)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        store
            .write_blob_chunk(sha256, offset, &bytes)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        Ok(offset.saturating_add(bytes.len() as u64))
    }

    pub async fn remote_blob_finish(
        &self,
        sha256: &str,
        total_bytes: u64,
    ) -> Result<(), DesktopError> {
        let (config, _) = Self::open_remote(self).await?;
        let secrets = self.current_secrets();
        let store = RemoteSyncStore::open(&config.data_dir)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        store
            .finish_blob(sha256, total_bytes)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))
    }

    /// Accept a worker completion report. The task transitions exactly like a
    /// local attempt; changed files are stored, never applied.
    pub async fn remote_task_finish(&self, finish: RemoteFinish) -> Result<TaskDto, DesktopError> {
        let limits = RemoteSyncLimits::default();
        finish
            .validate(&limits)
            .map_err(|error| boundary_error(self, error))?;
        if finish.attempt == 0 {
            return Err(failure("invalid_args", "attempt must be positive"));
        }
        let (config, store) = Self::open_remote(self).await?;
        let secrets = self.current_secrets();
        let task = store
            .load(finish.task_id)
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        if task.remote_worker.as_deref() != Some(finish.worker_id.as_str()) {
            return Err(failure(
                "invalid_args",
                "task is not assigned to this remote worker",
            ));
        }
        if task.last_run_id != Some(finish.run_id) {
            return Err(failure(
                "task_dispatch_conflict",
                "remote result does not match the dispatched Run",
            ));
        }
        let lease = store
            .lease(finish.task_id)
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?
            .filter(|lease| lease.owner == lease_owner(&finish.worker_id))
            .ok_or_else(|| failure("task_lease_lost", "remote worker lease is missing"))?;
        let sync_store = RemoteSyncStore::open(&config.data_dir)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        let manifest = sync_store
            .load_manifest(finish.task_id)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?
            .ok_or_else(|| {
                failure(
                    "remote_result_invalid",
                    "the dispatch workspace snapshot is missing",
                )
            })?;
        let mut paths = BTreeSet::new();
        for change in &finish.changed_files {
            if !paths.insert(change.path.as_str()) {
                return Err(failure(
                    error_codes::INVALID_ARGS,
                    "the worker reported a repeated changed path",
                ));
            }
            let expected_base = manifest.get(&change.path).map(|entry| entry.sha256.clone());
            if expected_base != change.base_sha256 {
                return Err(failure(
                    error_codes::CONFLICT,
                    format!(
                        "worker-reported base for `{}` does not match the dispatch snapshot",
                        change.path
                    ),
                ));
            }
            match change.kind {
                remote::RemoteChangeKind::Added if expected_base.is_some() => {
                    return Err(failure(
                        error_codes::CONFLICT,
                        format!("`{}` already existed at dispatch time", change.path),
                    ));
                }
                remote::RemoteChangeKind::Added => {}
                remote::RemoteChangeKind::Modified | remote::RemoteChangeKind::Deleted
                    if expected_base.is_none() =>
                {
                    return Err(failure(
                        error_codes::CONFLICT,
                        format!("`{}` did not exist at dispatch time", change.path),
                    ));
                }
                _ => {}
            }
            if let Some(digest) = &change.new_sha256 {
                let stored = sync_store.blob_len(digest);
                if stored != Some(change.bytes) {
                    return Err(failure(
                        "remote_result_invalid",
                        format!(
                            "the uploaded blob for `{}` is missing or the wrong size",
                            change.path
                        ),
                    ));
                }
            }
        }
        let now = timestamp().map_err(|error| boundary_error(self, error))?;
        let summary = TaskRunSummary {
            run_id: finish.run_id,
            status: finish.status,
            stop_reason: finish.stop_reason.map(|value| {
                truncate(
                    soyo_ipc::redaction::redact_str(&value, &secrets),
                    MAX_STOP_REASON_BYTES,
                )
            }),
            text: finish.text.map(|value| {
                truncate(
                    soyo_ipc::redaction::redact_str(&value, &secrets),
                    MAX_TASK_RESULT_BYTES,
                )
            }),
            error_code: finish.error_code,
            persistence_saved: finish.persistence_saved,
            finished_at: now.clone(),
        };
        let result = RemoteTaskResult {
            schema_version: REMOTE_PROTOCOL_SCHEMA_VERSION,
            worker_id: finish.worker_id.clone(),
            run_id: finish.run_id,
            summary: finish.summary.map(|value| {
                truncate(
                    soyo_ipc::redaction::redact_str(&value, &secrets),
                    MAX_REMOTE_SUMMARY_BYTES,
                )
            }),
            changed_files: finish.changed_files,
            received_at: now.clone(),
        };
        let dispatch = TaskDispatch {
            task_id: finish.task_id,
            attempt: finish.attempt,
            run_id: finish.run_id,
        };
        let task = store
            .finish_remote_attempt(
                &lease,
                &dispatch,
                summary,
                &RunMetrics::default(),
                result,
                now,
            )
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        let _ = self.record_progress(
            &finish.worker_id,
            finish.task_id,
            finish.attempt,
            "finished",
            None,
        );
        Ok(self.map_task_with_job(&task, &secrets))
    }

    /// Apply a stored worker result through the three-way check. Only a
    /// completed task can be applied, and any conflict writes nothing.
    pub async fn remote_task_apply(
        &self,
        task_id: &str,
        paths: Option<Vec<String>>,
    ) -> Result<RemoteApplyReport, DesktopError> {
        let key = parse_task_id(task_id)?;
        let (config, store) = Self::open_remote(self).await?;
        let secrets = self.current_secrets();
        let task = store
            .load(key)
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        if !remote::apply_allowed(task.status) {
            return Err(failure(
                "invalid_task_state",
                "only a completed remote task can be applied",
            ));
        }
        let result = task.remote_result.clone().ok_or_else(|| {
            failure(
                "invalid_task_state",
                "the task carries no remote result to apply",
            )
        })?;
        let sync_store = RemoteSyncStore::open(&config.data_dir)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        let manifest = sync_store
            .load_manifest(key)
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?
            .ok_or_else(|| {
                failure(
                    "remote_result_invalid",
                    "the dispatch workspace snapshot is missing",
                )
            })?;
        let limits = RemoteSyncLimits::default();
        let protected = Self::protected_paths(&config);
        let outcome = remote::apply_result(
            &task.workspace,
            &manifest,
            &result,
            &sync_store,
            &protected,
            paths.as_deref(),
            &limits,
        )
        .map_err(|error| boundary_error(self, error))?;
        Ok(match outcome {
            remote::RemoteApplyOutcome::Applied(receipts) => RemoteApplyReport {
                applied: true,
                receipts,
                conflicts: Vec::new(),
            },
            remote::RemoteApplyOutcome::Conflict(conflicts) => RemoteApplyReport {
                applied: false,
                receipts: Vec::new(),
                conflicts,
            },
        })
    }

    /// Task view plus the held-back worker result (for `soyo remote show`).
    pub async fn remote_task_show(&self, task_id: &str) -> Result<RemoteTaskShow, DesktopError> {
        let key = parse_task_id(task_id)?;
        let (_, store) = Self::open_remote(self).await?;
        let secrets = self.current_secrets();
        let task = store
            .load(key)
            .await
            .map_err(|error| DesktopError::from_boundary(&error, &secrets))?;
        Ok(RemoteTaskShow {
            task: self.map_task_with_job(&task, &secrets),
            result: task.remote_result.clone(),
        })
    }
}
