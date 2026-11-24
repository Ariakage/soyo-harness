//! Durable cross-run task queue records and state validation.
//!
//! The task queue persists executable work items across process restarts.
//! Dispatch intents are recorded before a Run starts so a worker crash is
//! recoverable: an uncertain outcome is marked `unknown` and never replayed
//! automatically; only an explicit resolution requeues the task.

mod store;

pub use store::{StoreSettings, TaskLease, TaskStore};

use crate::{
    contract::{BoundaryError, ErrorInfo, PermissionMode, RunId, SessionId, WorkMode},
    text,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

pub const TASK_SCHEMA_VERSION: u32 = 1;
pub const MAX_TASK_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_TASK_RESULT_BYTES: usize = 64 * 1024;

/// UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) used by durable task records.
pub fn timestamp() -> Result<String, BoundaryError> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| failure("clock_error", "system clock is before epoch"))?
        .as_secs();
    let seconds = libc::time_t::try_from(seconds)
        .map_err(|_| failure("clock_error", "system clock is out of range"))?;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: gmtime_r writes a valid tm to the provided pointer on success.
    let ptr = unsafe { libc::gmtime_r(&seconds, tm.as_mut_ptr()) };
    if ptr.is_null() {
        return Err(failure("clock_error", "cannot read UTC clock"));
    }
    let tm = unsafe { tm.assume_init() };
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    ))
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

fn valid_text(value: &str, limit: usize) -> bool {
    !value.trim().is_empty()
        && value.len() <= limit
        && !value.chars().any(text::is_terminal_control)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::str::FromStr for $name {
            type Err = BoundaryError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                value
                    .parse::<Uuid>()
                    .map(Self)
                    .map_err(|_| failure("invalid_task_id", "Task identifier is invalid"))
            }
        }
    };
}

id_type!(TaskId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Paused,
    Blocked,
    Completed,
    Failed,
    Cancelled,
    WaitingUnknown,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// What a durable task row represents. Ordinary tasks drive one top-level Run;
/// `subagent` rows drive one persistent child subagent (`D-110`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunKind {
    #[default]
    Task,
    Subagent,
}

impl TaskRunKind {
    pub fn is_task(&self) -> bool {
        *self == Self::Task
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    Intent,
    Running,
    Completed,
    Failed,
    Unknown,
    /// A remote dispatch whose claimant never confirmed the Run started; the
    /// task went back to `queued` without replaying any side effect.
    Expired,
}

/// Persisted summary of the last finished attempt; model text is data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRunSummary {
    pub run_id: RunId,
    pub status: crate::contract::RunStatus,
    pub stop_reason: Option<String>,
    pub text: Option<String>,
    pub error_code: Option<String>,
    pub persistence_saved: bool,
    pub finished_at: String,
}

impl TaskRunSummary {
    pub fn from_result(
        result: &crate::contract::RunResult,
        finished_at: String,
    ) -> Result<Self, BoundaryError> {
        let run_id = result
            .run_id
            .ok_or_else(|| failure("invalid_task_run", "Task Run result has no Run ID"))?;
        Ok(Self {
            run_id,
            status: result.status,
            stop_reason: result.stop_reason.as_ref().map(|value| {
                let mut value = value.clone();
                value.truncate(1024);
                while !value.is_char_boundary(value.len()) {
                    value.pop();
                }
                value
            }),
            text: result.text.as_ref().map(|value| {
                let mut value = value.clone();
                value.truncate(MAX_TASK_RESULT_BYTES);
                while !value.is_char_boundary(value.len()) {
                    value.pop();
                }
                value
            }),
            error_code: result.error.as_ref().map(|error| error.code.clone()),
            persistence_saved: result
                .persistence
                .as_ref()
                .is_some_and(|persistence| persistence.saved),
            finished_at,
        })
    }

    fn validate(&self) -> Result<(), BoundaryError> {
        if self
            .text
            .as_ref()
            .is_some_and(|value| value.len() > MAX_TASK_RESULT_BYTES)
            || self
                .stop_reason
                .as_ref()
                .is_some_and(|value| value.len() > 1024)
            || self.finished_at.trim().is_empty()
            || self.finished_at.len() > 128
        {
            return Err(failure(
                "invalid_task",
                "Task Run summary exceeds its storage limit",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub schema_version: u32,
    pub id: TaskId,
    pub session_id: SessionId,
    pub workspace: PathBuf,
    pub profile: String,
    pub mode: WorkMode,
    pub permissions: PermissionMode,
    pub input: String,
    /// Task rows are ordinary durable tasks unless explicitly persisted as a
    /// child subagent (`D-110`). The field is omitted for ordinary tasks so
    /// existing records keep their exact encoding.
    #[serde(default, skip_serializing_if = "TaskRunKind::is_task")]
    pub run_kind: TaskRunKind,
    /// Bound child configuration and last outcome for `run_kind = subagent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<crate::subagent::PersistentSubagentJob>,
    /// Stable id of the remote worker this task is dispatched to (`D-117`).
    /// Ordinary local tasks leave it unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_worker: Option<String>,
    /// Last worker-reported result, held back from the owning workspace until
    /// an explicit `soyo remote apply`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_result: Option<crate::remote::RemoteTaskResult>,
    pub status: TaskStatus,
    pub revision: u64,
    pub failure: Option<ErrorInfo>,
    pub created_at: String,
    pub updated_at: String,
    pub last_run_id: Option<RunId>,
    pub last_result: Option<TaskRunSummary>,
}

pub struct TaskSeed {
    pub session_id: SessionId,
    pub workspace: PathBuf,
    pub profile: String,
    pub mode: WorkMode,
    pub permissions: PermissionMode,
    pub input: String,
    pub now: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskDispatch {
    pub task_id: TaskId,
    pub attempt: u32,
    pub run_id: RunId,
}

impl TaskRecord {
    pub fn new(seed: TaskSeed) -> Result<Self, BoundaryError> {
        let record = Self {
            schema_version: TASK_SCHEMA_VERSION,
            id: TaskId::new(),
            session_id: seed.session_id,
            workspace: seed.workspace,
            profile: seed.profile,
            mode: seed.mode,
            permissions: seed.permissions,
            input: seed.input,
            run_kind: TaskRunKind::Task,
            subagent: None,
            remote_worker: None,
            remote_result: None,
            status: TaskStatus::Queued,
            revision: 0,
            failure: None,
            created_at: seed.now.clone(),
            updated_at: seed.now,
            last_run_id: None,
            last_result: None,
        };
        record.validate()?;
        Ok(record)
    }

    /// Create a durable child subagent job (`D-110`).
    pub fn new_subagent(
        seed: TaskSeed,
        job: crate::subagent::PersistentSubagentJob,
    ) -> Result<Self, BoundaryError> {
        let mut record = Self::new(seed)?;
        record.run_kind = TaskRunKind::Subagent;
        record.subagent = Some(job);
        record.validate()?;
        Ok(record)
    }

    /// Create a persistent subagent record with a pre-allocated identifier so
    /// its scratch directory can be addressed before the record exists.
    pub fn new_subagent_with_id(
        seed: TaskSeed,
        id: TaskId,
        job: crate::subagent::PersistentSubagentJob,
    ) -> Result<Self, BoundaryError> {
        let mut record = Self::new_subagent(seed, job)?;
        record.id = id;
        Ok(record)
    }

    pub fn validate(&self) -> Result<(), BoundaryError> {
        if self.schema_version != TASK_SCHEMA_VERSION
            || !valid_text(&self.input, MAX_TASK_INPUT_BYTES)
            || !self.workspace.is_absolute()
            || !valid_identifier(&self.profile)
            || self.created_at.trim().is_empty()
            || self.created_at.len() > 128
            || self.updated_at.trim().is_empty()
            || self.updated_at.len() > 128
        {
            return Err(failure(
                "invalid_task",
                "Task schema, input, workspace, profile or timestamps are invalid",
            ));
        }
        match (self.run_kind, &self.subagent) {
            (TaskRunKind::Task, None) => {}
            (TaskRunKind::Subagent, Some(job)) => job.validate()?,
            (TaskRunKind::Task, Some(_)) => {
                return Err(failure(
                    "invalid_task",
                    "an ordinary Task cannot carry a persistent subagent job",
                ));
            }
            (TaskRunKind::Subagent, None) => {
                return Err(failure(
                    "invalid_task",
                    "a persistent subagent Task requires its job payload",
                ));
            }
        }
        if let Some(worker) = &self.remote_worker
            && !crate::remote::valid_worker_id(worker)
        {
            return Err(failure(
                "invalid_task",
                "Task remote worker identifier is invalid",
            ));
        }
        if let Some(remote) = &self.remote_result {
            remote.validate(&crate::remote::RemoteSyncLimits::default())?;
            if self.remote_worker.as_deref() != Some(remote.worker_id.as_str()) {
                return Err(failure(
                    "invalid_task",
                    "Task remote result does not belong to its assigned worker",
                ));
            }
            if self.last_run_id != Some(remote.run_id) {
                return Err(failure(
                    "invalid_task",
                    "Task remote result does not match its last Run",
                ));
            }
        }
        if let Some(result) = &self.last_result {
            result.validate()?;
            if self.last_run_id != Some(result.run_id) {
                return Err(failure(
                    "invalid_task",
                    "Task Run and result identity differ",
                ));
            }
        }
        if self.status == TaskStatus::Running && self.last_run_id.is_none() {
            return Err(failure(
                "invalid_task",
                "a running Task requires its current Run ID",
            ));
        }
        if self.status == TaskStatus::Completed && self.failure.is_some() {
            return Err(failure(
                "invalid_task",
                "a completed Task cannot carry a failure",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
