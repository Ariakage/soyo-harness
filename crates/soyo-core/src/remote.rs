//! Remote worker workspace sync (`F-13` D-113 path (c)).
//!
//! A remote worker is a separate trust domain. The owning helper ships the
//! task workspace as a content-addressed manifest plus blobs (never a file
//! copy), and treats the reported result as unverified external input. Worker
//! changes re-enter the owning workspace only through an explicit three-way
//! apply that refuses to overwrite files which changed locally after dispatch.
//!
//! This module owns the shared vocabulary and storage used by both sides:
//! manifest/change/result records, a digest-addressed blob cache under
//! `<data_dir>/remote/`, bounded workspace scanning, and the apply check.

use crate::{
    contract::{BoundaryError, ErrorInfo, PermissionMode, RunId, RunStatus, WorkMode, error_codes},
    subagent::validate_relative_workspace_path,
    task_queue::{TaskId, TaskStatus},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Wire schema of the remote-worker records published by this module.
pub const REMOTE_PROTOCOL_SCHEMA_VERSION: u32 = 1;
/// A workspace manifest larger than this would not fit the NDJSON frame cap.
pub const MAX_REMOTE_DISPATCH_BYTES: usize = 4 * 1024 * 1024;
/// Raw bytes per blob chunk; base64 plus framing stays far below the frame cap.
pub const REMOTE_BLOB_CHUNK_BYTES: usize = 1024 * 1024;
pub const MAX_REMOTE_WORKER_ID_BYTES: usize = 128;
pub const MAX_REMOTE_SUMMARY_BYTES: usize = 32 * 1024;
pub const MAX_REMOTE_WORKERS: usize = 128;
pub const MAX_REMOTE_PROGRESS_BYTES: usize = 512;

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

fn sync_error(message: impl Into<String>) -> BoundaryError {
    failure(error_codes::REMOTE_SYNC_ERROR, message)
}

fn limit_error(message: impl Into<String>) -> BoundaryError {
    failure(error_codes::REMOTE_SYNC_LIMIT, message)
}

/// Bounds applied to a remote workspace snapshot and its changed-file result.
///
/// The defaults mirror the subagent workspace defaults so a workspace that is
/// too large for `subagent_spawn` is also too large for remote execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteSyncLimits {
    pub max_files: usize,
    pub max_total_bytes: u64,
    pub max_file_bytes: u64,
    pub max_changed_files: usize,
}

impl Default for RemoteSyncLimits {
    fn default() -> Self {
        Self {
            max_files: 10_000,
            max_total_bytes: 64 * 1024 * 1024,
            max_file_bytes: 16 * 1024 * 1024,
            max_changed_files: 256,
        }
    }
}

impl RemoteSyncLimits {
    pub fn validate(&self) -> Result<(), BoundaryError> {
        if self.max_files == 0
            || self.max_files > 50_000
            || self.max_total_bytes < 1024
            || self.max_total_bytes > 512 * 1024 * 1024
            || self.max_file_bytes < 1024
            || self.max_file_bytes > self.max_total_bytes
            || self.max_changed_files == 0
            || self.max_changed_files > 2048
        {
            return Err(failure(
                error_codes::INVALID_CONFIG,
                "remote sync limits are outside their supported range",
            ));
        }
        Ok(())
    }
}

/// One file in the dispatch-time workspace snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteFileEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    #[serde(default)]
    pub executable: bool,
}

/// Kind of a worker-reported workspace change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteChangeKind {
    Added,
    Modified,
    Deleted,
}

/// One changed file reported by a worker after execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteFileChange {
    pub path: String,
    pub kind: RemoteChangeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_sha256: Option<String>,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub executable: bool,
}

/// Worker-reported result stored on the owning task, never trusted as an
/// approval to write files: `apply_result` re-checks everything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTaskResult {
    pub schema_version: u32,
    pub worker_id: String,
    pub run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub changed_files: Vec<RemoteFileChange>,
    pub received_at: String,
}

impl RemoteTaskResult {
    pub fn validate(&self, limits: &RemoteSyncLimits) -> Result<(), BoundaryError> {
        limits.validate()?;
        if self.schema_version != REMOTE_PROTOCOL_SCHEMA_VERSION
            || !valid_worker_id(&self.worker_id)
            || self.received_at.trim().is_empty()
            || self.received_at.len() > 128
            || self
                .summary
                .as_ref()
                .is_some_and(|value| value.len() > MAX_REMOTE_SUMMARY_BYTES)
            || self.changed_files.len() > limits.max_changed_files
        {
            return Err(failure(
                error_codes::INVALID_CONFIG,
                "remote task result exceeds its storage limits",
            ));
        }
        let mut total = 0u64;
        let mut paths = BTreeSet::new();
        for change in &self.changed_files {
            change.validate(self.worker_id.as_str(), limits)?;
            if !paths.insert(change.path.as_str()) {
                return Err(failure(
                    error_codes::INVALID_CONFIG,
                    "remote task result repeats a changed path",
                ));
            }
            total = total.saturating_add(change.bytes);
        }
        if total > limits.max_total_bytes {
            return Err(limit_error(
                "remote result exceeds the workspace byte bound",
            ));
        }
        Ok(())
    }
}

impl RemoteFileChange {
    fn validate(&self, worker_id: &str, limits: &RemoteSyncLimits) -> Result<(), BoundaryError> {
        validate_relative_workspace_path(&self.path)?;
        if let Some(base) = &self.base_sha256
            && !valid_sha256(base)
        {
            return Err(failure(
                error_codes::INVALID_CONFIG,
                "remote change base digest is invalid",
            ));
        }
        match self.kind {
            RemoteChangeKind::Added | RemoteChangeKind::Modified => {
                if !self.new_sha256.as_deref().is_some_and(valid_sha256) {
                    return Err(failure(
                        error_codes::INVALID_CONFIG,
                        "remote change requires a valid new digest",
                    ));
                }
                if self.bytes > limits.max_file_bytes {
                    return Err(limit_error("remote change exceeds the file byte bound"));
                }
            }
            RemoteChangeKind::Deleted => {
                if self.new_sha256.is_some() {
                    return Err(failure(
                        error_codes::INVALID_CONFIG,
                        "a deleted remote change cannot carry a new digest",
                    ));
                }
            }
        }
        let _ = worker_id;
        Ok(())
    }
}

/// One applied file receipt returned by an explicit apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteApplyReceipt {
    pub path: String,
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_sha256: Option<String>,
}

/// A path whose local content diverged from the dispatch snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteConflict {
    pub path: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_sha256: Option<String>,
}

/// Apply outcome. Conflicts are all-or-nothing: nothing is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteApplyOutcome {
    Applied(Vec<RemoteApplyReceipt>),
    Conflict(Vec<RemoteConflict>),
}

/// One chunk of a content-addressed blob download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBlobChunk {
    pub sha256: String,
    pub total_bytes: u64,
    pub next_offset: u64,
    pub data_b64: String,
}

impl RemoteBlobChunk {
    pub fn encode(
        sha256: &str,
        offset: u64,
        total_bytes: u64,
        bytes: &[u8],
    ) -> Result<Self, BoundaryError> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        Ok(Self {
            sha256: sha256.to_owned(),
            total_bytes,
            next_offset: offset.saturating_add(bytes.len() as u64),
            data_b64: STANDARD.encode(bytes),
        })
    }

    pub fn data(&self) -> Result<Vec<u8>, BoundaryError> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        STANDARD
            .decode(&self.data_b64)
            .map_err(|_| failure(error_codes::INVALID_ARGS, "blob chunk is not valid base64"))
    }
}

/// Task dispatch handed to a claiming worker over the helper wire contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDispatch {
    pub schema_version: u32,
    pub task_id: TaskId,
    pub attempt: u32,
    pub run_id: RunId,
    pub input: String,
    pub mode: WorkMode,
    pub permissions: PermissionMode,
    pub workspace: Vec<RemoteFileEntry>,
    pub max_file_bytes: u64,
    pub max_changed_files: usize,
}

impl RemoteDispatch {
    pub fn validate(&self, limits: &RemoteSyncLimits) -> Result<(), BoundaryError> {
        if self.schema_version != REMOTE_PROTOCOL_SCHEMA_VERSION
            || self.input.trim().is_empty()
            || self.workspace.len() > limits.max_files
            || self.max_changed_files == 0
            || self.max_changed_files > limits.max_changed_files
        {
            return Err(sync_error("remote dispatch is malformed"));
        }
        let mut total = 0u64;
        for entry in &self.workspace {
            validate_relative_workspace_path(&entry.path)?;
            if !valid_sha256(&entry.sha256) || entry.bytes > limits.max_file_bytes {
                return Err(sync_error("remote dispatch entry is malformed"));
            }
            total = total.saturating_add(entry.bytes);
        }
        if total > limits.max_total_bytes {
            return Err(limit_error(
                "remote dispatch exceeds the workspace byte bound",
            ));
        }
        Ok(())
    }
}

/// Worker registration sent before any claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteWorkerRegistration {
    pub schema_version: u32,
    pub worker_id: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub sandbox_backends: Vec<String>,
    #[serde(default)]
    pub version: String,
}

impl RemoteWorkerRegistration {
    pub fn validate(&self) -> Result<(), BoundaryError> {
        if self.schema_version != REMOTE_PROTOCOL_SCHEMA_VERSION
            || !valid_worker_id(&self.worker_id)
            || !valid_label(&self.arch)
            || !valid_label(&self.version)
            || self.sandbox_backends.len() > 8
            || self.sandbox_backends.iter().any(|name| !valid_label(name))
        {
            return Err(failure(
                error_codes::INVALID_ARGS,
                "remote worker registration is malformed",
            ));
        }
        Ok(())
    }
}

/// Last progress report for a worker's in-flight task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteWorkerProgress {
    pub task_id: String,
    pub attempt: u32,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub at: String,
}

/// Helper-side view of a registered worker (never persisted; a worker
/// re-registers after a daemon restart).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteWorkerInfo {
    pub worker_id: String,
    pub arch: String,
    pub sandbox_backends: Vec<String>,
    pub version: String,
    pub registered_at: String,
    pub last_seen_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<RemoteWorkerProgress>,
}

/// Heartbeat answer: whether the lease was renewed and whether the owning side
/// asks the worker to cancel the Run (pause/cancel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHeartbeat {
    pub renewed: bool,
    pub cancel: bool,
}

/// Worker completion report. The owning side maps it through the same task
/// state machine as a local attempt; nothing is auto-applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteFinish {
    pub schema_version: u32,
    pub worker_id: String,
    pub task_id: TaskId,
    pub attempt: u32,
    pub run_id: RunId,
    pub status: RunStatus,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub persistence_saved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub changed_files: Vec<RemoteFileChange>,
}

impl RemoteFinish {
    pub fn validate(&self, limits: &RemoteSyncLimits) -> Result<(), BoundaryError> {
        if self.schema_version != REMOTE_PROTOCOL_SCHEMA_VERSION
            || !valid_worker_id(&self.worker_id)
        {
            return Err(failure(
                error_codes::INVALID_ARGS,
                "remote finish report is malformed",
            ));
        }
        if self
            .summary
            .as_ref()
            .is_some_and(|value| value.len() > MAX_REMOTE_SUMMARY_BYTES)
        {
            return Err(limit_error("remote finish summary exceeds its bound"));
        }
        let result = RemoteTaskResult {
            schema_version: REMOTE_PROTOCOL_SCHEMA_VERSION,
            worker_id: self.worker_id.clone(),
            run_id: self.run_id,
            summary: self.summary.clone(),
            changed_files: self.changed_files.clone(),
            received_at: "validate".into(),
        };
        result.validate(limits)
    }
}

/// Owner-side persisted workspace snapshot for one dispatched task.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredManifest {
    schema_version: u32,
    task_id: TaskId,
    entries: Vec<RemoteFileEntry>,
}

/// Digest-addressed blob cache and dispatch manifests under `<data_dir>/remote`.
///
/// Blobs are keyed by their SHA-256, so serving or accepting a blob never
/// trusts a path; the digest is re-verified on whole-file reads and on upload
/// completion. There is no garbage collection in this first cut.
#[derive(Clone)]
pub struct RemoteSyncStore {
    root: PathBuf,
}

impl RemoteSyncStore {
    pub fn open(data_dir: &Path) -> Result<Self, BoundaryError> {
        let root = data_dir.join("remote");
        for path in [
            root.clone(),
            root.join("blobs"),
            root.join("manifests"),
            root.join("tmp"),
        ] {
            private_dir(&path)?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn blob_path(&self, sha256: &str) -> Result<PathBuf, BoundaryError> {
        if !valid_sha256(sha256) {
            return Err(failure(error_codes::INVALID_ARGS, "blob digest is invalid"));
        }
        Ok(self.root.join("blobs").join(&sha256[..2]).join(sha256))
    }

    fn part_path(&self, sha256: &str) -> Result<PathBuf, BoundaryError> {
        if !valid_sha256(sha256) {
            return Err(failure(error_codes::INVALID_ARGS, "blob digest is invalid"));
        }
        Ok(self.root.join("tmp").join(format!("{sha256}.part")))
    }

    fn manifest_path(&self, task_id: TaskId) -> PathBuf {
        self.root
            .join("manifests")
            .join(format!("{}.json", task_id.0))
    }

    pub fn has_blob(&self, sha256: &str) -> bool {
        self.blob_path(sha256)
            .map(|path| fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()))
            .unwrap_or(false)
    }

    /// Stored size of a committed blob, if present (no content read).
    pub fn blob_len(&self, sha256: &str) -> Option<u64> {
        let path = self.blob_path(sha256).ok()?;
        let meta = fs::symlink_metadata(&path).ok()?;
        (meta.is_file() && !meta.file_type().is_symlink()).then_some(meta.len())
    }

    /// Store a complete blob, refusing a mismatched digest or oversized file.
    pub fn put_blob(&self, sha256: &str, bytes: &[u8]) -> Result<(), BoundaryError> {
        let path = self.blob_path(sha256)?;
        if bytes.len() as u64 > RemoteSyncLimits::default().max_file_bytes {
            return Err(limit_error("blob exceeds the per-file byte bound"));
        }
        if sha256_hex(bytes) != sha256 {
            return Err(failure(
                error_codes::INVALID_ARGS,
                "blob content does not match its digest",
            ));
        }
        write_atomic(&path, bytes, 0o600)
    }

    /// Read a complete blob and re-verify its digest.
    pub fn get_blob(&self, sha256: &str) -> Result<Vec<u8>, BoundaryError> {
        let path = self.blob_path(sha256)?;
        let limit = RemoteSyncLimits::default().max_file_bytes;
        let bytes = read_file_bounded(&path, limit)?;
        if sha256_hex(&bytes) != sha256 {
            return Err(failure(
                error_codes::REMOTE_SYNC_ERROR,
                "stored blob failed digest verification",
            ));
        }
        Ok(bytes)
    }

    /// Read one chunk of a blob for a chunked wire download.
    pub fn read_blob_range(
        &self,
        sha256: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<(Vec<u8>, u64), BoundaryError> {
        if max_bytes == 0 {
            return Err(failure(error_codes::INVALID_ARGS, "chunk size is zero"));
        }
        let path = self.blob_path(sha256)?;
        let meta = fs::symlink_metadata(&path)
            .map_err(|_| failure(error_codes::REMOTE_SYNC_ERROR, "blob is not available"))?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(failure(error_codes::PATH_DENIED, "blob path is unsafe"));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|_| failure(error_codes::REMOTE_SYNC_ERROR, "blob is unreadable"))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| failure(error_codes::REMOTE_SYNC_ERROR, "blob seek failed"))?;
        let mut buf = Vec::new();
        Read::by_ref(&mut file)
            .take(max_bytes as u64)
            .read_to_end(&mut buf)
            .map_err(|_| failure(error_codes::REMOTE_SYNC_ERROR, "blob read failed"))?;
        Ok((buf, meta.len()))
    }

    /// Start (or restart) a chunked blob upload.
    pub fn begin_blob(&self, sha256: &str, total_bytes: u64) -> Result<(), BoundaryError> {
        let path = self.part_path(sha256)?;
        if total_bytes > RemoteSyncLimits::default().max_file_bytes {
            return Err(limit_error("blob exceeds the per-file byte bound"));
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|_| failure(error_codes::REMOTE_SYNC_ERROR, "blob upload is unavailable"))?;
        file.set_len(0)
            .map_err(|_| failure(error_codes::REMOTE_SYNC_ERROR, "blob upload is unavailable"))?;
        Ok(())
    }

    /// Append one chunk; `offset` must equal the bytes already received.
    pub fn write_blob_chunk(
        &self,
        sha256: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<(), BoundaryError> {
        let path = self.part_path(sha256)?;
        let meta = fs::symlink_metadata(&path)
            .map_err(|_| failure(error_codes::INVALID_ARGS, "blob upload is not started"))?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(failure(
                error_codes::PATH_DENIED,
                "blob upload path is unsafe",
            ));
        }
        if meta.len() != offset {
            return Err(failure(
                error_codes::INVALID_ARGS,
                "blob chunk offset does not match the received length",
            ));
        }
        let mut file = OpenOptions::new()
            .append(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|_| failure(error_codes::REMOTE_SYNC_ERROR, "blob upload is unavailable"))?;
        file.write_all(data)
            .map_err(|_| failure(error_codes::REMOTE_SYNC_ERROR, "blob upload write failed"))?;
        Ok(())
    }

    /// Finish a chunked upload: verify size and digest, then move it in place.
    pub fn finish_blob(&self, sha256: &str, total_bytes: u64) -> Result<(), BoundaryError> {
        let part = self.part_path(sha256)?;
        let limit = RemoteSyncLimits::default().max_file_bytes;
        if total_bytes > limit {
            return Err(limit_error("blob exceeds the per-file byte bound"));
        }
        let bytes = read_file_bounded(&part, limit)?;
        if bytes.len() as u64 != total_bytes {
            return Err(failure(
                error_codes::INVALID_ARGS,
                "blob upload ended before its declared size",
            ));
        }
        let target = self.blob_path(sha256)?;
        if sha256_hex(&bytes) != sha256 {
            let _ = fs::remove_file(&part);
            return Err(failure(
                error_codes::INVALID_ARGS,
                "blob content does not match its digest",
            ));
        }
        if let Some(parent) = target.parent() {
            ensure_dir(parent)?;
        }
        fs::rename(&part, &target)
            .map_err(|_| failure(error_codes::REMOTE_SYNC_ERROR, "blob commit failed"))?;
        Ok(())
    }

    /// Persist the dispatch-time snapshot so apply can use it as the three-way
    /// base even after a helper restart.
    pub fn store_manifest(
        &self,
        task_id: TaskId,
        entries: &BTreeMap<String, RemoteFileEntry>,
    ) -> Result<(), BoundaryError> {
        let stored = StoredManifest {
            schema_version: REMOTE_PROTOCOL_SCHEMA_VERSION,
            task_id,
            entries: entries.values().cloned().collect(),
        };
        let encoded = serde_json::to_vec(&stored)
            .map_err(|_| sync_error("remote manifest cannot be encoded"))?;
        if encoded.len() > MAX_REMOTE_DISPATCH_BYTES {
            return Err(limit_error(
                "remote manifest exceeds the dispatch byte bound",
            ));
        }
        write_atomic(&self.manifest_path(task_id), &encoded, 0o600)
    }

    pub fn load_manifest(
        &self,
        task_id: TaskId,
    ) -> Result<Option<BTreeMap<String, RemoteFileEntry>>, BoundaryError> {
        let path = self.manifest_path(task_id);
        let raw = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
                read_file_bounded(&path, MAX_REMOTE_DISPATCH_BYTES as u64)?
            }
            Ok(_) => {
                return Err(failure(
                    error_codes::PATH_DENIED,
                    "remote manifest path is unsafe",
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(sync_error("remote manifest is unreadable")),
        };
        let stored: StoredManifest =
            serde_json::from_slice(&raw).map_err(|_| sync_error("remote manifest is invalid"))?;
        if stored.schema_version != REMOTE_PROTOCOL_SCHEMA_VERSION || stored.task_id != task_id {
            return Err(sync_error("remote manifest identity is invalid"));
        }
        let mut entries = BTreeMap::new();
        for entry in stored.entries {
            validate_relative_workspace_path(&entry.path)?;
            if !valid_sha256(&entry.sha256) {
                return Err(sync_error("remote manifest digest is invalid"));
            }
            entries.insert(entry.path.clone(), entry);
        }
        Ok(Some(entries))
    }
}

/// Scan a workspace into a bounded, digest-addressed manifest.
///
/// `.git` internals and symlinks/special files are never included. Ignore
/// files apply inside a repository so build outputs excluded from version
/// control are not shipped to the worker.
pub fn scan_workspace(
    root: &Path,
    protected: &[PathBuf],
    limits: &RemoteSyncLimits,
) -> Result<BTreeMap<String, RemoteFileEntry>, BoundaryError> {
    limits.validate()?;
    let root = fs::canonicalize(root).map_err(|_| {
        failure(
            error_codes::INVALID_WORKSPACE,
            "task workspace is unavailable",
        )
    })?;
    let mut manifest = BTreeMap::new();
    let mut total_bytes: u64 = 0;
    let mut builder = ignore::WalkBuilder::new(&root);
    builder
        .hidden(false)
        .follow_links(false)
        .parents(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .filter_entry(|entry| entry.file_name() != ".git");
    for item in builder.build() {
        let entry = item.map_err(|err| sync_error(format!("workspace scan failed: {err}")))?;
        let path = entry.path();
        if path == root {
            continue;
        }
        let meta = fs::symlink_metadata(path)
            .map_err(|err| sync_error(format!("cannot inspect {}: {err}", path.display())))?;
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(&root)
            .map_err(|_| sync_error("scanned path escaped the workspace"))?;
        let Some(rel) = rel.to_str() else {
            return Err(failure(
                error_codes::INVALID_PATH,
                "workspace contains a non-UTF-8 path",
            ));
        };
        if validate_relative_workspace_path(rel).is_err() || overlaps_protected(path, protected) {
            continue;
        }
        if manifest.len() >= limits.max_files {
            return Err(limit_error(
                "workspace file count exceeds the remote sync bound",
            ));
        }
        let remaining = limits
            .max_total_bytes
            .saturating_sub(total_bytes)
            .min(limits.max_file_bytes);
        let (bytes, mode) = read_file_with_mode(path, remaining)?;
        total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        manifest.insert(
            rel.to_owned(),
            RemoteFileEntry {
                path: rel.to_owned(),
                sha256: sha256_hex(&bytes),
                bytes: bytes.len() as u64,
                executable: mode & 0o111 != 0,
            },
        );
    }
    Ok(manifest)
}

/// Diff the current workspace against a dispatch-time snapshot.
pub fn diff_workspace(
    root: &Path,
    base: &BTreeMap<String, RemoteFileEntry>,
    protected: &[PathBuf],
    limits: &RemoteSyncLimits,
) -> Result<Vec<RemoteFileChange>, BoundaryError> {
    let current = scan_workspace(root, protected, limits)?;
    let mut paths = BTreeSet::new();
    paths.extend(base.keys().cloned());
    paths.extend(current.keys().cloned());
    let mut changes = Vec::new();
    for path in paths {
        let before = base.get(&path);
        let after = current.get(&path);
        let (kind, base_sha, new_sha, bytes, executable) = match (before, after) {
            (None, Some(entry)) => (
                RemoteChangeKind::Added,
                None,
                Some(entry.sha256.clone()),
                entry.bytes,
                entry.executable,
            ),
            (Some(entry), None) => (
                RemoteChangeKind::Deleted,
                Some(entry.sha256.clone()),
                None,
                0,
                false,
            ),
            (Some(before), Some(after)) if before.sha256 != after.sha256 => (
                RemoteChangeKind::Modified,
                Some(before.sha256.clone()),
                Some(after.sha256.clone()),
                after.bytes,
                after.executable,
            ),
            _ => continue,
        };
        if changes.len() >= limits.max_changed_files {
            return Err(limit_error(
                "changed file count exceeds the remote sync bound",
            ));
        }
        changes.push(RemoteFileChange {
            path,
            kind,
            base_sha256: base_sha,
            new_sha256: new_sha,
            bytes,
            executable,
        });
    }
    Ok(changes)
}

/// Path of a task's materialized workspace on the worker side. Each attempt
/// gets its own directory so a retried task never observes a previous run.
pub fn worker_task_dir(workspace_root: &Path, task_id: TaskId, attempt: u32) -> PathBuf {
    workspace_root.join(format!("{}-{attempt}", task_id.0))
}

/// Create a fresh, empty task workspace directory.
pub fn create_worker_workspace(
    workspace_root: &Path,
    task_id: TaskId,
    attempt: u32,
) -> Result<PathBuf, BoundaryError> {
    let root = fs::canonicalize(workspace_root).map_err(|_| {
        failure(
            error_codes::INVALID_WORKSPACE,
            "worker workspace is unavailable",
        )
    })?;
    let dir = worker_task_dir(&root, task_id, attempt);
    match fs::symlink_metadata(&dir) {
        Ok(_) => {
            return Err(failure(
                error_codes::REMOTE_SYNC_ERROR,
                "worker task workspace already exists",
            ));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(sync_error("worker task workspace is unreadable")),
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|_| sync_error("worker task workspace cannot be created"))?;
    Ok(dir)
}

/// Write one worker-side workspace file from a verified blob.
pub fn write_workspace_file(
    root: &Path,
    rel: &str,
    bytes: &[u8],
    executable: bool,
    max_bytes: u64,
) -> Result<(), BoundaryError> {
    let rel = validate_relative_workspace_path(rel)?;
    check_no_symlinks(root, &rel)?;
    if bytes.len() as u64 > max_bytes {
        return Err(limit_error(
            "workspace file exceeds the remote sync byte bound",
        ));
    }
    write_atomic(
        &root.join(rel),
        bytes,
        if executable { 0o755 } else { 0o644 },
    )
}

/// Read one workspace file for upload, refusing symlinked or oversized paths.
pub fn read_workspace_file(
    root: &Path,
    rel: &str,
    max_bytes: u64,
) -> Result<(Vec<u8>, RemoteFileEntry), BoundaryError> {
    let rel = validate_relative_workspace_path(rel)?;
    check_no_symlinks(root, &rel)?;
    let abs = root.join(&rel);
    let (bytes, mode) = read_file_with_mode(&abs, max_bytes)?;
    let rel = rel
        .to_str()
        .ok_or_else(|| failure(error_codes::INVALID_PATH, "workspace path is not UTF-8"))?
        .to_owned();
    let entry = RemoteFileEntry {
        path: rel,
        sha256: sha256_hex(&bytes),
        bytes: bytes.len() as u64,
        executable: mode & 0o111 != 0,
    };
    Ok((bytes, entry))
}

/// Apply a worker result to the owning task workspace.
///
/// The persisted dispatch manifest — not the worker-reported digest — is the
/// base of the three-way check: a path is written only when its local content
/// still matches the snapshot (or already matches the worker's result). Any
/// conflict refuses the whole apply.
pub fn apply_result(
    workspace: &Path,
    base: &BTreeMap<String, RemoteFileEntry>,
    result: &RemoteTaskResult,
    blobs: &RemoteSyncStore,
    protected: &[PathBuf],
    selected: Option<&[String]>,
    limits: &RemoteSyncLimits,
) -> Result<RemoteApplyOutcome, BoundaryError> {
    result.validate(limits)?;
    let root = fs::canonicalize(workspace).map_err(|_| {
        failure(
            error_codes::INVALID_WORKSPACE,
            "task workspace is unavailable",
        )
    })?;
    let selected: Option<BTreeSet<&str>> =
        selected.map(|paths| paths.iter().map(String::as_str).collect());
    if let Some(filter) = &selected {
        let available: BTreeSet<&str> = result
            .changed_files
            .iter()
            .map(|change| change.path.as_str())
            .collect();
        if let Some(missing) = filter.iter().find(|path| !available.contains(**path)) {
            return Err(failure(
                error_codes::INVALID_ARGS,
                format!("path `{missing}` was not changed by this worker result"),
            ));
        }
    }

    struct Plan {
        path: String,
        rel: PathBuf,
        kind: RemoteChangeKind,
        pre_sha256: Option<String>,
        post_sha256: Option<String>,
        bytes: Option<Vec<u8>>,
        executable: bool,
    }
    let mut plan = Vec::new();
    let mut conflicts = Vec::new();

    for change in &result.changed_files {
        if let Some(filter) = &selected
            && !filter.contains(change.path.as_str())
        {
            continue;
        }
        let rel = validate_relative_workspace_path(&change.path)?;
        check_no_symlinks(&root, &rel)?;
        let abs = root.join(&rel);
        if overlaps_protected(&abs, protected) {
            return Err(failure(
                error_codes::PATH_DENIED,
                "remote apply target addresses protected host storage",
            ));
        }
        let snapshot_sha = base.get(&change.path).map(|entry| entry.sha256.clone());
        if snapshot_sha != change.base_sha256 {
            return Err(failure(
                error_codes::CONFLICT,
                format!(
                    "worker-reported base for `{}` does not match the dispatch snapshot",
                    change.path
                ),
            ));
        }
        if change.kind == RemoteChangeKind::Added && snapshot_sha.is_some() {
            return Err(failure(
                error_codes::CONFLICT,
                format!("`{}` already existed at dispatch time", change.path),
            ));
        }
        if change.kind == RemoteChangeKind::Deleted && snapshot_sha.is_none() {
            return Err(failure(
                error_codes::CONFLICT,
                format!("`{}` did not exist at dispatch time", change.path),
            ));
        }
        let local_sha = match read_file_with_mode_opt(&abs, limits.max_file_bytes) {
            Ok(Some((bytes, _))) => Some(sha256_hex(&bytes)),
            Ok(None) => None,
            Err(_) => {
                conflicts.push(RemoteConflict {
                    path: change.path.clone(),
                    reason: "local file is unreadable or exceeds the sync bound".into(),
                    base_sha256: snapshot_sha.clone(),
                    local_sha256: None,
                    remote_sha256: change.new_sha256.clone(),
                });
                continue;
            }
        };
        if local_sha == change.new_sha256 {
            plan.push(Plan {
                path: change.path.clone(),
                rel,
                kind: change.kind,
                pre_sha256: local_sha,
                post_sha256: change.new_sha256.clone(),
                bytes: None,
                executable: change.executable,
            });
            continue;
        }
        if local_sha != snapshot_sha {
            conflicts.push(RemoteConflict {
                path: change.path.clone(),
                reason: "local file changed since the task was dispatched".into(),
                base_sha256: snapshot_sha,
                local_sha256: local_sha,
                remote_sha256: change.new_sha256.clone(),
            });
            continue;
        }
        let bytes = match change.kind {
            RemoteChangeKind::Added | RemoteChangeKind::Modified => {
                let digest = change.new_sha256.as_deref().ok_or_else(|| {
                    failure(error_codes::INVALID_ARGS, "remote change has no new digest")
                })?;
                let bytes = blobs.get_blob(digest)?;
                if bytes.len() as u64 != change.bytes {
                    return Err(failure(
                        error_codes::CONFLICT,
                        format!(
                            "stored blob size for `{}` does not match the result",
                            change.path
                        ),
                    ));
                }
                Some(bytes)
            }
            RemoteChangeKind::Deleted => None,
        };
        plan.push(Plan {
            path: change.path.clone(),
            rel,
            kind: change.kind,
            pre_sha256: local_sha,
            post_sha256: change.new_sha256.clone(),
            bytes,
            executable: change.executable,
        });
    }

    if !conflicts.is_empty() {
        return Ok(RemoteApplyOutcome::Conflict(conflicts));
    }

    let mut receipts = Vec::with_capacity(plan.len());
    for item in plan {
        if item.pre_sha256 == item.post_sha256 {
            receipts.push(RemoteApplyReceipt {
                path: item.path,
                operation: "unchanged".into(),
                pre_sha256: item.pre_sha256,
                post_sha256: item.post_sha256,
            });
            continue;
        }
        check_no_symlinks(&root, &item.rel)?;
        let abs = root.join(&item.rel);
        match item.kind {
            RemoteChangeKind::Added | RemoteChangeKind::Modified => {
                let bytes = item.bytes.as_deref().unwrap_or(&[]);
                write_atomic(&abs, bytes, if item.executable { 0o755 } else { 0o644 })?;
                let (verified, _) = read_file_with_mode(&abs, limits.max_file_bytes)?;
                if Some(sha256_hex(&verified).as_str()) != item.post_sha256.as_deref() {
                    return Err(failure(
                        error_codes::CONFLICT,
                        format!("post-apply digest verification failed for {}", item.path),
                    ));
                }
                receipts.push(RemoteApplyReceipt {
                    path: item.path,
                    operation: if item.kind == RemoteChangeKind::Added {
                        "created"
                    } else {
                        "modified"
                    }
                    .into(),
                    pre_sha256: item.pre_sha256,
                    post_sha256: item.post_sha256,
                });
            }
            RemoteChangeKind::Deleted => {
                match fs::symlink_metadata(&abs) {
                    Ok(meta) => {
                        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                            return Err(failure(
                                error_codes::PATH_DENIED,
                                "cannot delete a non-regular or symlinked target",
                            ));
                        }
                        fs::remove_file(&abs)
                            .map_err(|_| sync_error(format!("cannot delete {}", item.path)))?;
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err(sync_error(format!("cannot stat {}", item.path))),
                }
                receipts.push(RemoteApplyReceipt {
                    path: item.path,
                    operation: "deleted".into(),
                    pre_sha256: item.pre_sha256,
                    post_sha256: None,
                });
            }
        }
    }
    Ok(RemoteApplyOutcome::Applied(receipts))
}

/// Whether a task status permits applying a stored remote result.
pub fn apply_allowed(status: TaskStatus) -> bool {
    status == TaskStatus::Completed
}

pub fn valid_worker_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REMOTE_WORKER_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_label(value: &str) -> bool {
    value.len() <= 64 && !value.chars().any(crate::text::is_terminal_control)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn private_dir(path: &Path) -> Result<(), BoundaryError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.is_dir() || meta.file_type().is_symlink() {
                return Err(failure(
                    error_codes::PATH_DENIED,
                    "remote storage path is unsafe",
                ));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|_| sync_error("remote storage directory cannot be created"))?;
        }
        Err(_) => return Err(sync_error("remote storage directory is unreadable")),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| sync_error("remote storage directory permissions cannot be set"))
}

fn ensure_dir(path: &Path) -> Result<(), BoundaryError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(failure(
            error_codes::PATH_DENIED,
            "remote storage path is unsafe",
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|_| sync_error("remote storage directory cannot be created")),
        Err(_) => Err(sync_error("remote storage directory is unreadable")),
    }
}

fn overlaps_protected(abs: &Path, protected: &[PathBuf]) -> bool {
    protected
        .iter()
        .any(|path| abs.starts_with(path) || path.starts_with(abs))
}

fn check_no_symlinks(root: &Path, rel: &Path) -> Result<(), BoundaryError> {
    let mut cursor = root.to_path_buf();
    for component in rel.components() {
        cursor.push(component);
        match fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(failure(
                    error_codes::PATH_DENIED,
                    "symlink targets are forbidden in remote sync operations",
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(sync_error("workspace path is unreadable")),
        }
    }
    Ok(())
}

fn read_file_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, BoundaryError> {
    read_file_with_mode(path, max_bytes).map(|(bytes, _)| bytes)
}

fn read_file_with_mode(path: &Path, max_bytes: u64) -> Result<(Vec<u8>, u32), BoundaryError> {
    read_file_with_mode_opt(path, max_bytes)?
        .ok_or_else(|| sync_error(format!("cannot inspect {}: not found", path.display())))
}

fn read_file_with_mode_opt(
    path: &Path,
    max_bytes: u64,
) -> Result<Option<(Vec<u8>, u32)>, BoundaryError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(sync_error(format!(
                "cannot inspect {}: {err}",
                path.display()
            )));
        }
    };
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(failure(
            error_codes::PATH_DENIED,
            "symlinks and special files are not allowed in remote sync operations",
        ));
    }
    if meta.len() > max_bytes {
        return Err(limit_error(
            "workspace file exceeds the remote sync byte bound",
        ));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|err| sync_error(format!("cannot open {}: {err}", path.display())))?;
    let opened = file
        .metadata()
        .map_err(|_| sync_error(format!("cannot stat {}", path.display())))?;
    if !opened.is_file() || (opened.dev(), opened.ino()) != (meta.dev(), meta.ino()) {
        return Err(failure(
            error_codes::PATH_DENIED,
            "file changed during read",
        ));
    }
    let mut buf = Vec::with_capacity(usize::try_from(opened.len()).unwrap_or(0));
    Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|_| sync_error(format!("failed reading {}", path.display())))?;
    if buf.len() as u64 > max_bytes {
        return Err(limit_error(
            "workspace file exceeds the remote sync byte bound",
        ));
    }
    Ok(Some((buf, opened.mode())))
}

fn write_atomic(target: &Path, bytes: &[u8], mode: u32) -> Result<(), BoundaryError> {
    let parent = target
        .parent()
        .ok_or_else(|| failure(error_codes::INVALID_PATH, "target file has no parent"))?;
    ensure_dir(parent)?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| failure(error_codes::INVALID_PATH, "target filename is invalid"))?;
    let tmp = parent.join(format!(".{name}.soyo-remote-{}", Uuid::now_v7().simple()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode & 0o777)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, target)?;
        if let Some(parent) = target.parent()
            && let Ok(dir) = fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        return Err(sync_error("atomic remote write failed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> RemoteSyncLimits {
        RemoteSyncLimits::default()
    }

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn scan_skips_git_symlinks_and_ignored_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        write(root, "src/main.rs", b"fn main() {}\n");
        write(root, "target/build.bin", b"ignored\n");
        write(root, ".gitignore", b"target/\n");
        std::os::unix::fs::symlink("/etc/passwd", root.join("leak")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        write(root, ".git/config", b"secret\n");

        let manifest = scan_workspace(root, &[], &limits()).unwrap();
        let paths: Vec<&str> = manifest.keys().map(String::as_str).collect();
        assert_eq!(paths, vec![".gitignore", "src/main.rs"]);
        assert_eq!(manifest["src/main.rs"].bytes, 13);
        assert_eq!(
            manifest["src/main.rs"].sha256,
            sha256_hex(b"fn main() {}\n")
        );
    }

    #[test]
    fn scan_enforces_file_count_and_byte_caps() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.txt", b"aaaa");
        write(root, "b.txt", b"bbbb");
        let tight = RemoteSyncLimits {
            max_files: 1,
            ..limits()
        };
        assert_eq!(
            scan_workspace(root, &[], &tight).unwrap_err().0.code,
            error_codes::REMOTE_SYNC_LIMIT
        );
        let tight = RemoteSyncLimits {
            max_total_bytes: 1024,
            max_file_bytes: 1024,
            ..limits()
        };
        // Fits the count cap but exceeds the tiny byte budget.
        write(root, "c.bin", &vec![b'x'; 2048]);
        assert_eq!(
            scan_workspace(root, &[], &tight).unwrap_err().0.code,
            error_codes::REMOTE_SYNC_LIMIT
        );
    }

    #[test]
    fn blob_store_round_trips_and_verifies_digests() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteSyncStore::open(dir.path()).unwrap();
        let data = b"hello remote worker";
        let sha = sha256_hex(data);
        store.put_blob(&sha, data).unwrap();
        assert!(store.has_blob(&sha));
        assert_eq!(store.get_blob(&sha).unwrap(), data);
        let (chunk, total) = store.read_blob_range(&sha, 0, 5).unwrap();
        assert_eq!(chunk, b"hello");
        assert_eq!(total, data.len() as u64);
        assert_eq!(
            store.put_blob(&sha, b"tampered").unwrap_err().0.code,
            error_codes::INVALID_ARGS
        );
        assert_eq!(
            store.get_blob(&"0".repeat(64)).unwrap_err().0.code,
            error_codes::REMOTE_SYNC_ERROR
        );
    }

    #[test]
    fn chunked_upload_commits_only_with_matching_digest() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteSyncStore::open(dir.path()).unwrap();
        let data = b"chunked upload payload";
        let sha = sha256_hex(data);
        store.begin_blob(&sha, data.len() as u64).unwrap();
        store.write_blob_chunk(&sha, 0, &data[..8]).unwrap();
        assert_eq!(
            store
                .write_blob_chunk(&sha, 99, &data[8..])
                .unwrap_err()
                .0
                .code,
            error_codes::INVALID_ARGS
        );
        store.write_blob_chunk(&sha, 8, &data[8..]).unwrap();
        store.finish_blob(&sha, data.len() as u64).unwrap();
        assert_eq!(store.get_blob(&sha).unwrap(), data);

        let wrong = sha256_hex(b"different");
        store.begin_blob(&wrong, 3).unwrap();
        store.write_blob_chunk(&wrong, 0, b"abc").unwrap();
        assert_eq!(
            store.finish_blob(&wrong, 3).unwrap_err().0.code,
            error_codes::INVALID_ARGS
        );
        assert!(!store.has_blob(&wrong));
    }

    #[test]
    fn manifest_round_trips_and_rejects_traversal_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteSyncStore::open(dir.path()).unwrap();
        let task = TaskId::new();
        let mut entries = BTreeMap::new();
        entries.insert(
            "src/lib.rs".to_owned(),
            RemoteFileEntry {
                path: "src/lib.rs".into(),
                sha256: "a".repeat(64),
                bytes: 3,
                executable: false,
            },
        );
        store.store_manifest(task, &entries).unwrap();
        let loaded = store.load_manifest(task).unwrap().unwrap();
        assert_eq!(loaded, entries);
        assert_eq!(store.load_manifest(TaskId::new()).unwrap(), None);

        let task = TaskId::new();
        let mut bad = BTreeMap::new();
        bad.insert(
            "../escape".to_owned(),
            RemoteFileEntry {
                path: "../escape".into(),
                sha256: "a".repeat(64),
                bytes: 3,
                executable: false,
            },
        );
        store.store_manifest(task, &bad).unwrap();
        assert!(store.load_manifest(task).is_err());
    }

    fn snapshot(root: &Path) -> BTreeMap<String, RemoteFileEntry> {
        scan_workspace(root, &[], &limits()).unwrap()
    }

    fn result_for(changes: Vec<RemoteFileChange>) -> RemoteTaskResult {
        RemoteTaskResult {
            schema_version: REMOTE_PROTOCOL_SCHEMA_VERSION,
            worker_id: "worker-1".into(),
            run_id: RunId::new(),
            summary: Some("done".into()),
            changed_files: changes,
            received_at: "2026-11-24T00:00:00Z".into(),
        }
    }

    #[test]
    fn apply_writes_changes_through_the_three_way_check() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        fs::create_dir_all(&root).unwrap();
        write(&root, "keep.txt", b"keep\n");
        write(&root, "edit.txt", b"before\n");
        let base = snapshot(&root);

        let blobs = RemoteSyncStore::open(&dir.path().join("data")).unwrap();
        let new_edit = b"after\n";
        let new_file = b"created\n";
        blobs.put_blob(&sha256_hex(new_edit), new_edit).unwrap();
        blobs.put_blob(&sha256_hex(new_file), new_file).unwrap();
        let result = result_for(vec![
            RemoteFileChange {
                path: "edit.txt".into(),
                kind: RemoteChangeKind::Modified,
                base_sha256: Some(base["edit.txt"].sha256.clone()),
                new_sha256: Some(sha256_hex(new_edit)),
                bytes: new_edit.len() as u64,
                executable: false,
            },
            RemoteFileChange {
                path: "added.txt".into(),
                kind: RemoteChangeKind::Added,
                base_sha256: None,
                new_sha256: Some(sha256_hex(new_file)),
                bytes: new_file.len() as u64,
                executable: false,
            },
        ]);
        let outcome = apply_result(&root, &base, &result, &blobs, &[], None, &limits()).unwrap();
        let RemoteApplyOutcome::Applied(receipts) = outcome else {
            panic!("expected apply");
        };
        assert_eq!(receipts.len(), 2);
        assert_eq!(fs::read(root.join("edit.txt")).unwrap(), new_edit);
        assert_eq!(fs::read(root.join("added.txt")).unwrap(), new_file);

        // A second apply against the same dispatch snapshot is idempotent:
        // both paths already match the result.
        let outcome = apply_result(&root, &base, &result, &blobs, &[], None, &limits()).unwrap();
        match outcome {
            RemoteApplyOutcome::Applied(receipts) => {
                assert!(receipts.iter().all(|r| r.operation == "unchanged"));
            }
            RemoteApplyOutcome::Conflict(_) => panic!("idempotent apply must not conflict"),
        }
    }

    #[test]
    fn apply_refuses_local_divergence_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        fs::create_dir_all(&root).unwrap();
        write(&root, "a.txt", b"base\n");
        write(&root, "b.txt", b"base\n");
        let base = snapshot(&root);
        // Local edit after dispatch: a.txt diverges from the snapshot.
        write(&root, "a.txt", b"local\n");

        let blobs = RemoteSyncStore::open(&dir.path().join("data")).unwrap();
        let new = b"remote\n";
        blobs.put_blob(&sha256_hex(new), new).unwrap();
        let changes = ["a.txt", "b.txt"]
            .iter()
            .map(|path| RemoteFileChange {
                path: (*path).into(),
                kind: RemoteChangeKind::Modified,
                base_sha256: Some(base[*path].sha256.clone()),
                new_sha256: Some(sha256_hex(new)),
                bytes: new.len() as u64,
                executable: false,
            })
            .collect();
        let result = result_for(changes);
        let outcome = apply_result(&root, &base, &result, &blobs, &[], None, &limits()).unwrap();
        let RemoteApplyOutcome::Conflict(conflicts) = outcome else {
            panic!("expected conflict");
        };
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "a.txt");
        // All-or-nothing: b.txt was not overwritten either.
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"local\n");
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"base\n");
    }

    #[test]
    fn apply_rejects_a_result_whose_base_differs_from_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        fs::create_dir_all(&root).unwrap();
        write(&root, "a.txt", b"base\n");
        let base = snapshot(&root);
        let blobs = RemoteSyncStore::open(&dir.path().join("data")).unwrap();
        let new = b"remote\n";
        blobs.put_blob(&sha256_hex(new), new).unwrap();
        let result = result_for(vec![RemoteFileChange {
            path: "a.txt".into(),
            kind: RemoteChangeKind::Modified,
            base_sha256: Some(sha256_hex(b"lie\n")),
            new_sha256: Some(sha256_hex(new)),
            bytes: new.len() as u64,
            executable: false,
        }]);
        let err = apply_result(&root, &base, &result, &blobs, &[], None, &limits()).unwrap_err();
        assert_eq!(err.0.code, error_codes::CONFLICT);
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"base\n");
    }

    #[test]
    fn apply_rejects_paths_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        fs::create_dir_all(&root).unwrap();
        let base = BTreeMap::new();
        let blobs = RemoteSyncStore::open(&dir.path().join("data")).unwrap();
        let result = result_for(vec![RemoteFileChange {
            path: "../escape".into(),
            kind: RemoteChangeKind::Added,
            base_sha256: None,
            new_sha256: Some(sha256_hex(b"x")),
            bytes: 1,
            executable: false,
        }]);
        assert_eq!(
            apply_result(&root, &base, &result, &blobs, &[], None, &limits())
                .unwrap_err()
                .0
                .code,
            error_codes::INVALID_PATH
        );
    }

    #[test]
    fn worker_workspace_is_fresh_per_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let task = TaskId::new();
        let first = create_worker_workspace(dir.path(), task, 1).unwrap();
        assert!(first.ends_with(format!("{}-1", task.0)));
        assert_eq!(
            create_worker_workspace(dir.path(), task, 1)
                .unwrap_err()
                .0
                .code,
            error_codes::REMOTE_SYNC_ERROR
        );
        let second = create_worker_workspace(dir.path(), task, 2).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn diff_reports_added_modified_and_deleted_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "modified.txt", b"before\n");
        write(root, "deleted.txt", b"gone\n");
        let base = snapshot(root);
        write(root, "modified.txt", b"after\n");
        fs::remove_file(root.join("deleted.txt")).unwrap();
        write(root, "added.txt", b"new\n");
        let changes = diff_workspace(root, &base, &[], &limits()).unwrap();
        let summary: Vec<(String, RemoteChangeKind)> = changes
            .iter()
            .map(|change| (change.path.clone(), change.kind))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("added.txt".to_owned(), RemoteChangeKind::Added),
                ("deleted.txt".to_owned(), RemoteChangeKind::Deleted),
                ("modified.txt".to_owned(), RemoteChangeKind::Modified),
            ]
        );
        let modified = changes
            .iter()
            .find(|change| change.path == "modified.txt")
            .unwrap();
        assert_eq!(
            modified.base_sha256.as_deref(),
            Some(base["modified.txt"].sha256.as_str())
        );
        assert_eq!(
            modified.new_sha256.as_deref(),
            Some(sha256_hex(b"after\n").as_str())
        );
    }

    #[test]
    fn worker_ids_and_results_are_validated() {
        assert!(valid_worker_id("worker-1.local_2"));
        assert!(!valid_worker_id(""));
        assert!(!valid_worker_id("bad id"));
        assert!(!valid_worker_id(
            &"x".repeat(MAX_REMOTE_WORKER_ID_BYTES + 1)
        ));
        let registration = RemoteWorkerRegistration {
            schema_version: REMOTE_PROTOCOL_SCHEMA_VERSION,
            worker_id: "worker-1".into(),
            arch: "aarch64".into(),
            sandbox_backends: vec!["seatbelt".into()],
            version: "0.3.0".into(),
        };
        registration.validate().unwrap();
        let mut bad = registration.clone();
        bad.sandbox_backends = vec!["x".repeat(65)];
        assert!(bad.validate().is_err());
    }
}
