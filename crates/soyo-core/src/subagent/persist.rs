//! Durable persistent subagent jobs (`F-09`, `D-110`).
//!
//! A persistent spawn stores its bounded configuration, isolated workspace
//! reference, and baseline manifest outside the parent run. The job record is a
//! `run_kind = "subagent"` task row, so the F-08 lease/dispatch/recovery
//! machinery applies unchanged: an attempt whose worker disappeared waits as
//! `waiting_unknown` and is never replayed without an explicit resolution.

use super::{
    SubagentConflict, SubagentFileChange, SubagentIsolation, SubagentMergeReceipt,
    SubagentSnapshot, SubagentState, SubagentStepTrace, WorkspaceIsolationKind,
    validate_relative_workspace_path,
    workspace::{BaseFileEntry, IsolatedWorkspace},
};
use crate::{
    contract::{BoundaryError, ErrorInfo, SessionId, Usage, error_codes},
    task_queue::{TaskId, TaskRecord, TaskRunKind, TaskStatus, TaskStore},
    text,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const BASELINE_FILE: &str = "baseline.json";
const MAX_BASELINE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_JOB_SUMMARY_BYTES: usize = 256 * 1024;
const MAX_JOB_PATHS: usize = 64;
const MAX_JOB_FILES: usize = 2048;
const MAX_JOB_TRACES: usize = 256;
const MAX_JOB_LABEL_BYTES: usize = 64;

fn failure(code: &str, message: impl Into<String>) -> BoundaryError {
    BoundaryError::new(code, message)
}

fn sha256_hex_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_label(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_JOB_LABEL_BYTES
        && !value.chars().any(text::is_terminal_control)
}

/// Durable configuration and last known outcome of one persistent child
/// subagent. Stored inside the owning `TaskRecord`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistentSubagentJob {
    pub parent_session_id: SessionId,
    pub parent_workspace: PathBuf,
    pub label: String,
    pub isolation: SubagentIsolation,
    pub isolation_kind: WorkspaceIsolationKind,
    pub allow_exec: bool,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    pub timeout_seconds: u64,
    /// Delegation depth of the child runtime (hops from the top-level run).
    pub depth: usize,
    /// Isolated child workspace root, or the parent workspace for read-only jobs.
    pub workspace_root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_common_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub changed_files: Vec<SubagentFileChange>,
    #[serde(default)]
    pub conflicts: Vec<SubagentConflict>,
    #[serde(default)]
    pub merge_receipts: Vec<SubagentMergeReceipt>,
    #[serde(default)]
    pub step_traces: Vec<SubagentStepTrace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub execution_requests: u64,
    #[serde(default)]
    pub tool_calls: u64,
    /// A merge was applied to the parent at least once.
    #[serde(default)]
    pub merged: bool,
    /// The isolated workspace was removed after merge or explicit discard.
    #[serde(default)]
    pub discarded: bool,
}

impl PersistentSubagentJob {
    /// Reject malformed durable state before it is written or trusted.
    pub fn validate(&self) -> Result<(), BoundaryError> {
        if !valid_label(&self.label)
            || !self.parent_workspace.is_absolute()
            || !self.workspace_root.is_absolute()
            || self.allowed_paths.len() > MAX_JOB_PATHS
            || self.timeout_seconds == 0
            || self.timeout_seconds > super::MAX_ALLOWED_TIMEOUT_SECONDS
            || self.depth > super::MAX_ALLOWED_DEPTH
            || self
                .summary
                .as_ref()
                .is_some_and(|value| value.len() > MAX_JOB_SUMMARY_BYTES)
            || self.changed_files.len() > MAX_JOB_FILES
            || self.conflicts.len() > MAX_JOB_FILES
            || self.merge_receipts.len() > MAX_JOB_FILES
            || self.step_traces.len() > MAX_JOB_TRACES
        {
            return Err(failure(
                error_codes::INVALID_CONFIG,
                "persistent subagent job exceeds its storage limits",
            ));
        }
        for path in &self.allowed_paths {
            validate_relative_workspace_path(path)?;
        }
        if self.isolation == SubagentIsolation::ReadOnly {
            if self.isolation_kind != WorkspaceIsolationKind::ReadOnly
                || self.baseline_path.is_some()
                || self.scratch_root.is_some()
            {
                return Err(failure(
                    error_codes::SUBAGENT_WORKSPACE_ERROR,
                    "read-only persistent subagent cannot carry an isolated workspace",
                ));
            }
        } else if self.isolation_kind == WorkspaceIsolationKind::ReadOnly {
            return Err(failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "isolated persistent subagent requires an isolated workspace",
            ));
        }
        for change in &self.changed_files {
            validate_relative_workspace_path(&change.path)?;
            for digest in [&change.base_sha256, &change.new_sha256]
                .into_iter()
                .flatten()
            {
                if !sha256_hex_valid(digest) {
                    return Err(failure(
                        error_codes::INVALID_CONFIG,
                        "persistent subagent change digest is invalid",
                    ));
                }
            }
        }
        for conflict in &self.conflicts {
            validate_relative_workspace_path(&conflict.path)?;
        }
        Ok(())
    }

    /// Subagent-state projection for `subagent_status`.
    pub fn state(&self, task: &TaskRecord) -> SubagentState {
        // A merged job may also have discarded its workspace; report the merge
        // so the receipt stays visible.
        if self.merged {
            return SubagentState::Merged;
        }
        if self.discarded {
            return SubagentState::Discarded;
        }
        match task.status {
            TaskStatus::Queued | TaskStatus::Running | TaskStatus::Paused => SubagentState::Running,
            TaskStatus::Blocked | TaskStatus::WaitingUnknown => SubagentState::NeedsConfirmation,
            TaskStatus::Completed => SubagentState::Completed,
            TaskStatus::Failed => SubagentState::Failed,
            TaskStatus::Cancelled => SubagentState::Cancelled,
        }
    }

    /// Build the model-facing snapshot from durable state.
    pub fn snapshot(&self, task: &TaskRecord) -> SubagentSnapshot {
        let error = if task.status == TaskStatus::WaitingUnknown {
            Some(ErrorInfo {
                code: "task_outcome_unknown".into(),
                message: "the persistent subagent outcome is unknown; resolve the task explicitly"
                    .into(),
            })
        } else {
            task.failure.clone()
        };
        SubagentSnapshot {
            id: task.id.0.to_string(),
            label: self.label.clone(),
            state: self.state(task),
            work_mode: task.mode,
            isolation: self.isolation,
            isolation_kind: self.isolation_kind,
            allow_exec: self.allow_exec,
            allowed_paths: self.allowed_paths.clone(),
            summary: self.summary.clone().or_else(|| {
                task.last_result
                    .as_ref()
                    .and_then(|result| result.text.clone())
            }),
            error,
            usage: self.usage.clone(),
            execution_requests: self.execution_requests,
            tool_calls: self.tool_calls,
            step_traces: self.step_traces.clone(),
            changed_files: self.changed_files.clone(),
            merge_receipts: self.merge_receipts.clone(),
            persistent: true,
        }
    }
}

/// Store handle and scratch root for persistent subagent jobs. Cloned into the
/// runtime supervisor; all state lives in the shared F-08 task database.
#[derive(Clone)]
pub struct SubagentPersistence {
    store: TaskStore,
    root: PathBuf,
}

impl SubagentPersistence {
    /// Open the task store and the persistent-subagent scratch root.
    pub fn open(data_dir: &Path, secrets: Vec<String>) -> Result<Self, BoundaryError> {
        let store = TaskStore::open(data_dir, secrets)?;
        Self::new(store, data_dir.join("subagents"))
    }

    /// Attach to an already-open task store.
    pub fn new(store: TaskStore, root: PathBuf) -> Result<Self, BoundaryError> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
            .map_err(|_| {
                failure(
                    error_codes::SUBAGENT_WORKSPACE_ERROR,
                    "persistent subagent root cannot be created",
                )
            })?;
        let root = fs::canonicalize(&root).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent root cannot be resolved",
            )
        })?;
        let metadata = fs::symlink_metadata(&root).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent root cannot be inspected",
            )
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent root is unsafe",
            ));
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent root permissions cannot be tightened",
            )
        })?;
        Ok(Self { store, root })
    }

    pub fn store(&self) -> &TaskStore {
        &self.store
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Allocate the private job directory `<root>/<task-id>`.
    pub fn allocate_job_dir(&self, task_id: TaskId) -> Result<PathBuf, BoundaryError> {
        let dir = self.root.join(task_id.0.to_string());
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|_| {
                failure(
                    error_codes::SUBAGENT_WORKSPACE_ERROR,
                    "persistent subagent job directory cannot be created",
                )
            })?;
        fs::canonicalize(&dir).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent job directory cannot be resolved",
            )
        })
    }

    /// Remove the baseline directory belonging to one job. The isolated
    /// workspace itself is removed separately through `IsolatedWorkspace`.
    pub fn remove_job_dir(&self, task_id: TaskId) -> Result<(), BoundaryError> {
        let dir = self.root.join(task_id.0.to_string());
        match fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                fs::remove_dir_all(&dir).map_err(|_| {
                    failure(
                        error_codes::SUBAGENT_WORKSPACE_ERROR,
                        "persistent subagent job directory cannot be removed",
                    )
                })?;
                Ok(())
            }
            Ok(_) => Err(failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent job directory is unsafe",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent job directory cannot be inspected",
            )),
        }
    }

    /// Persist the fork-time baseline manifest next to the job workspace.
    pub fn write_baseline(
        &self,
        job_dir: &Path,
        workspace: &IsolatedWorkspace,
    ) -> Result<PathBuf, BoundaryError> {
        let path = job_dir.join(BASELINE_FILE);
        let encoded = serde_json::to_vec(&workspace.base_manifest).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent baseline cannot be encoded",
            )
        })?;
        if encoded.len() as u64 > MAX_BASELINE_BYTES {
            return Err(failure(
                error_codes::SUBAGENT_LIMIT,
                "persistent subagent baseline exceeds its byte limit",
            ));
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|_| {
                failure(
                    error_codes::SUBAGENT_WORKSPACE_ERROR,
                    "persistent subagent baseline cannot be created",
                )
            })?;
        file.write_all(&encoded).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent baseline cannot be written",
            )
        })?;
        file.sync_all().map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent baseline cannot be synchronized",
            )
        })?;
        Ok(path)
    }

    /// Load one persistent job, scoped to its expected parent session when one
    /// is supplied. Foreign jobs are reported as not found.
    pub async fn load_job(
        &self,
        task_id: TaskId,
        parent: Option<SessionId>,
    ) -> Result<(TaskRecord, PersistentSubagentJob), BoundaryError> {
        let task = self.store.load(task_id).await?;
        let job = self.job_of(&task, parent)?;
        Ok((task, job))
    }

    /// Load every persistent job (optionally scoped by parent session).
    pub async fn list_jobs(
        &self,
        parent: Option<SessionId>,
    ) -> Result<Vec<(TaskRecord, PersistentSubagentJob)>, BoundaryError> {
        let mut jobs = Vec::new();
        for task in self.store.list().await? {
            if task.run_kind != TaskRunKind::Subagent {
                continue;
            }
            if let Some(parent) = parent
                && task.session_id != parent
            {
                continue;
            }
            if let Some(job) = task.subagent.clone() {
                jobs.push((task, job));
            }
        }
        Ok(jobs)
    }

    fn job_of(
        &self,
        task: &TaskRecord,
        parent: Option<SessionId>,
    ) -> Result<PersistentSubagentJob, BoundaryError> {
        if task.run_kind != TaskRunKind::Subagent {
            return Err(failure(
                error_codes::SUBAGENT_NOT_FOUND,
                "persistent subagent was not found",
            ));
        }
        if let Some(parent) = parent
            && task.session_id != parent
        {
            return Err(failure(
                error_codes::SUBAGENT_NOT_FOUND,
                "persistent subagent was not found",
            ));
        }
        task.subagent.clone().ok_or_else(|| {
            failure(
                error_codes::SUBAGENT_NOT_FOUND,
                "persistent subagent was not found",
            )
        })
    }

    /// Apply one mutation to a stored job under the record revision guard.
    /// Conflicting revisions are retried a bounded number of times.
    pub async fn update_job(
        &self,
        task_id: TaskId,
        mutate: impl Fn(&mut PersistentSubagentJob) + Send + Sync,
    ) -> Result<(TaskRecord, PersistentSubagentJob), BoundaryError> {
        let mut last = None;
        for _ in 0..4 {
            let mut task = self.store.load(task_id).await?;
            let mut job = self.job_of(&task, None)?;
            mutate(&mut job);
            job.validate()?;
            task.subagent = Some(job.clone());
            task.updated_at = crate::task_queue::timestamp()?;
            let revision = task.revision;
            match self.store.save(task, revision).await {
                Ok(saved) => return Ok((saved, job)),
                Err(error) if error.is_code("task_conflict") => {
                    last = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| {
            failure(
                error_codes::SUBAGENT_ERROR,
                "persistent subagent record cannot be updated",
            )
        }))
    }

    /// Re-open the recorded isolated workspace, verifying every path and the
    /// persisted baseline before any diff or merge trusts it.
    pub fn reopen(
        &self,
        task_id: TaskId,
        job: &PersistentSubagentJob,
    ) -> Result<IsolatedWorkspace, BoundaryError> {
        job.validate()?;
        let parent = fs::canonicalize(&job.parent_workspace).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent parent workspace is unavailable",
            )
        })?;
        if parent != job.parent_workspace {
            return Err(failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent parent workspace identity changed",
            ));
        }
        if job.isolation == SubagentIsolation::ReadOnly {
            return Ok(IsolatedWorkspace {
                kind: WorkspaceIsolationKind::ReadOnly,
                outer_dir: None,
                child_dir: parent.clone(),
                parent_workspace: parent,
                git_common_dir: None,
                base_manifest: BTreeMap::new(),
            });
        }

        let scratch = job.scratch_root.as_ref().ok_or_else(|| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent scratch root is missing",
            )
        })?;
        let scratch = fs::canonicalize(scratch).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent scratch root is unavailable",
            )
        })?;
        let scratch_meta = fs::symlink_metadata(&scratch).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent scratch root cannot be inspected",
            )
        })?;
        if !scratch_meta.is_dir()
            || scratch_meta.file_type().is_symlink()
            || scratch_meta.permissions().mode() & 0o077 != 0
        {
            return Err(failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent scratch root is not a private directory",
            ));
        }
        let expected = self.root.join(task_id.0.to_string());
        let expected = fs::canonicalize(&expected).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent job directory is unavailable",
            )
        })?;
        let child = fs::canonicalize(&job.workspace_root).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent workspace is unavailable",
            )
        })?;
        if !child.starts_with(&scratch) || child == scratch {
            return Err(failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent workspace escaped its scratch directory",
            ));
        }
        let baseline_path = job.baseline_path.as_ref().ok_or_else(|| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent baseline is missing",
            )
        })?;
        let baseline_path = fs::canonicalize(baseline_path).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent baseline is unavailable",
            )
        })?;
        if !baseline_path.starts_with(&expected) {
            return Err(failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent baseline escaped its job directory",
            ));
        }
        let manifest = read_baseline(&baseline_path)?;
        let git_common_dir = match &job.git_common_dir {
            Some(path) => Some(fs::canonicalize(path).map_err(|_| {
                failure(
                    error_codes::SUBAGENT_WORKSPACE_ERROR,
                    "persistent subagent Git metadata is unavailable",
                )
            })?),
            None => None,
        };
        Ok(IsolatedWorkspace {
            kind: job.isolation_kind,
            outer_dir: Some(scratch),
            child_dir: child,
            parent_workspace: parent,
            git_common_dir,
            base_manifest: manifest,
        })
    }
}

fn read_baseline(path: &Path) -> Result<BTreeMap<String, BaseFileEntry>, BoundaryError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        failure(
            error_codes::SUBAGENT_WORKSPACE_ERROR,
            "persistent subagent baseline cannot be inspected",
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(failure(
            error_codes::SUBAGENT_WORKSPACE_ERROR,
            "persistent subagent baseline is unsafe",
        ));
    }
    if metadata.len() > MAX_BASELINE_BYTES {
        return Err(failure(
            error_codes::SUBAGENT_LIMIT,
            "persistent subagent baseline exceeds its byte limit",
        ));
    }
    let bytes = fs::read(path).map_err(|_| {
        failure(
            error_codes::SUBAGENT_WORKSPACE_ERROR,
            "persistent subagent baseline cannot be read",
        )
    })?;
    let manifest: BTreeMap<String, BaseFileEntry> =
        serde_json::from_slice(&bytes).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent baseline is invalid",
            )
        })?;
    for (path, entry) in &manifest {
        validate_relative_workspace_path(path)?;
        if !sha256_hex_valid(&entry.sha256) {
            return Err(failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "persistent subagent baseline digest is invalid",
            ));
        }
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{PermissionMode, WorkMode};
    use crate::task_queue::{TaskRunKind, TaskSeed};

    fn job() -> PersistentSubagentJob {
        PersistentSubagentJob {
            parent_session_id: SessionId::new(),
            parent_workspace: PathBuf::from("/workspace"),
            label: "worker".into(),
            isolation: SubagentIsolation::Isolated,
            isolation_kind: WorkspaceIsolationKind::DirectoryCopy,
            allow_exec: false,
            allowed_paths: vec!["src".into()],
            timeout_seconds: 60,
            depth: 1,
            workspace_root: PathBuf::from("/tmp/sub/ws"),
            scratch_root: Some(PathBuf::from("/tmp/sub")),
            git_common_dir: None,
            baseline_path: Some(PathBuf::from("/tmp/sub/baseline.json")),
            summary: None,
            changed_files: vec![],
            conflicts: vec![],
            merge_receipts: vec![],
            step_traces: vec![],
            usage: None,
            execution_requests: 0,
            tool_calls: 0,
            merged: false,
            discarded: false,
        }
    }

    #[test]
    fn job_validation_enforces_workspace_and_digest_invariants() {
        let valid = job();
        valid.validate().unwrap();

        let mut read_only = job();
        read_only.isolation = SubagentIsolation::ReadOnly;
        assert!(read_only.validate().is_err());

        let mut bad_digest = job();
        bad_digest.changed_files.push(SubagentFileChange {
            path: "src/lib.rs".into(),
            kind: super::super::ChangeKind::Modified,
            base_sha256: Some("not-a-digest".into()),
            new_sha256: None,
            bytes: 1,
            has_conflict: false,
        });
        assert!(bad_digest.validate().is_err());
    }

    #[test]
    fn task_record_requires_subagent_payload_for_subagent_kind() {
        let mut task = TaskRecord::new(TaskSeed {
            session_id: SessionId::new(),
            workspace: PathBuf::from("/workspace"),
            profile: "fixture".into(),
            mode: WorkMode::Build,
            permissions: PermissionMode::Auto,
            input: "child task".into(),
            now: "2026-11-24T00:00:00Z".into(),
        })
        .unwrap();
        assert_eq!(task.run_kind, TaskRunKind::Task);
        task.run_kind = TaskRunKind::Subagent;
        assert!(task.validate().is_err());
        task.subagent = Some(job());
        task.validate().unwrap();
    }
}
