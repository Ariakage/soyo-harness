//! Independent versioned SQLite storage for queued tasks and dispatch intents.
//!
//! A workspace may host several tasks, so there is no workspace lease table.
//! Dispatch intents are the crash-recovery record: an attempt whose worker
//! lease expires before the outcome is recorded is marked `unknown` and the
//! task waits for an explicit resolution instead of being replayed.

use super::{DispatchState, TaskDispatch, TaskId, TaskRecord, TaskRunSummary, TaskStatus, failure};
use crate::{
    contract::{BoundaryError, ErrorInfo, RunId, RunStatus},
    metrics::RunMetrics,
    redaction,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const DATABASE_NAME: &str = "tasks.sqlite3";
const SCHEMA_VERSION: i64 = 1;
const APPLICATION_ID: i64 = 0x5354_5131;
const MAX_TASK_ROWS: usize = 1024;
const SCHEMA: &str = r#"
CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    workspace TEXT NOT NULL,
    profile TEXT NOT NULL,
    status TEXT NOT NULL,
    revision INTEGER NOT NULL,
    record TEXT NOT NULL
);
CREATE INDEX tasks_status ON tasks(status, revision);
CREATE TABLE task_leases (
    task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    owner TEXT NOT NULL,
    expires_at_ms INTEGER NOT NULL
);
CREATE TABLE task_dispatches (
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    attempt INTEGER NOT NULL,
    run_id TEXT NOT NULL,
    state TEXT NOT NULL,
    PRIMARY KEY(task_id, attempt)
);
CREATE INDEX task_dispatches_state ON task_dispatches(state);
"#;

fn storage(code: &str, message: &str) -> BoundaryError {
    failure(code, message)
}

fn sql_error(_error: rusqlite::Error) -> BoundaryError {
    storage("task_storage", "Task database operation failed")
}

fn io_error() -> BoundaryError {
    storage("task_storage", "Task storage I/O failed")
}

#[derive(Debug, Clone)]
pub struct StoreSettings {
    pub busy_timeout: Duration,
    pub default_lease_ttl_ms: u64,
}

impl Default for StoreSettings {
    fn default() -> Self {
        Self {
            busy_timeout: Duration::from_secs(5),
            default_lease_ttl_ms: 60_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLease {
    pub task_id: TaskId,
    pub owner: String,
    pub expires_at_ms: u64,
}

#[derive(Clone)]
pub struct TaskStore {
    connection: Arc<Mutex<Connection>>,
    path: PathBuf,
    secrets: Arc<Vec<String>>,
    settings: StoreSettings,
}

fn id(value: impl std::fmt::Display) -> String {
    value.to_string()
}

fn encode(record: &TaskRecord, secrets: &[String]) -> Result<String, BoundaryError> {
    record.validate()?;
    let mut value = serde_json::to_value(record)
        .map_err(|_| storage("task_storage", "Task record cannot be encoded"))?;
    let original = value.clone();
    redaction::redact(&mut value, secrets)
        .map_err(|_| storage("storage_sensitive_data", "Task contains protected data"))?;
    if value != original {
        return Err(storage(
            "storage_sensitive_data",
            "Task contains a configured credential",
        ));
    }
    let encoded = serde_json::to_string(&value)
        .map_err(|_| storage("task_storage", "Task record cannot be encoded"))?;
    if redaction::contains_secret(&encoded, secrets) {
        return Err(storage(
            "storage_sensitive_data",
            "Task contains a configured credential",
        ));
    }
    Ok(encoded)
}

fn decode(raw: &str) -> Result<TaskRecord, BoundaryError> {
    let record: TaskRecord =
        serde_json::from_str(raw).map_err(|_| storage("task_corrupt", "Task record is invalid"))?;
    record
        .validate()
        .map_err(|_| storage("task_corrupt", "Task record violates its invariants"))?;
    Ok(record)
}

fn load_connection(connection: &Connection, task_id: TaskId) -> Result<TaskRecord, BoundaryError> {
    let raw: String = connection
        .query_row(
            "SELECT record FROM tasks WHERE id=?1",
            [id(task_id.0)],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?
        .ok_or_else(|| storage("task_not_found", "Task was not found"))?;
    decode(&raw)
}

fn write_connection(
    connection: &Connection,
    record: &TaskRecord,
    secrets: &[String],
    expected_revision: Option<u64>,
) -> Result<(), BoundaryError> {
    let raw = encode(record, secrets)?;
    let status = serde_json::to_value(record.status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| storage("task_storage", "Task status cannot be encoded"))?;
    let changed = connection
        .execute(
            "UPDATE tasks SET status=?2,revision=?3,record=?4 WHERE id=?1 AND (?5 IS NULL OR revision=?5)",
            params![
                id(record.id.0),
                status,
                i64::try_from(record.revision).unwrap_or(i64::MAX),
                raw,
                expected_revision.map(|value| i64::try_from(value).unwrap_or(i64::MAX))
            ],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(storage("task_not_found", "Task was not found"));
    }
    Ok(())
}

fn active_lease(
    connection: &Connection,
    task_id: TaskId,
    owner: &str,
    now_ms: u64,
) -> Result<(), BoundaryError> {
    let lease: Option<(String, u64)> = connection
        .query_row(
            "SELECT owner,expires_at_ms FROM task_leases WHERE task_id=?1",
            [id(task_id.0)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    match lease {
        Some((current, until)) if current == owner && until > now_ms => Ok(()),
        Some(_) => Err(storage("task_leased", "Task is owned by another worker")),
        None => Err(storage("task_lease_lost", "Task worker lease is missing")),
    }
}

fn dispatch_state_label(state: DispatchState) -> &'static str {
    match state {
        DispatchState::Intent => "intent",
        DispatchState::Running => "running",
        DispatchState::Completed => "completed",
        DispatchState::Failed => "failed",
        DispatchState::Unknown => "unknown",
    }
}

impl TaskStore {
    /// Open the private task queue database. This performs only local schema work.
    pub fn open(data_dir: &Path, secrets: Vec<String>) -> Result<Self, BoundaryError> {
        Self::open_with_settings(data_dir, secrets, StoreSettings::default())
    }

    pub fn open_with_settings(
        data_dir: &Path,
        secrets: Vec<String>,
        settings: StoreSettings,
    ) -> Result<Self, BoundaryError> {
        if settings.busy_timeout.is_zero() || settings.default_lease_ttl_ms == 0 {
            return Err(storage(
                "invalid_task_store",
                "Task storage timeouts must be positive",
            ));
        }
        let root = data_dir.join("tasks");
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
            .map_err(|_| io_error())?;
        let root = fs::canonicalize(&root).map_err(|_| io_error())?;
        let metadata = fs::symlink_metadata(&root).map_err(|_| io_error())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(storage("task_storage", "Task directory is unsafe"));
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(|_| io_error())?;
        let path = root.join(DATABASE_NAME);
        let created = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => {
                drop(file);
                true
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(_) => return Err(io_error()),
        };
        let metadata = fs::symlink_metadata(&path).map_err(|_| io_error())?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(storage("task_storage", "Task database is unsafe"));
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|_| io_error())?;
        let mut connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(sql_error)?;
        connection
            .busy_timeout(settings.busy_timeout)
            .map_err(sql_error)?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(sql_error)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sql_error)?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(sql_error)?;
        let application: i64 = connection
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .map_err(sql_error)?;
        if created {
            if version != 0 || application != 0 {
                return Err(storage("task_corrupt", "new Task database is not empty"));
            }
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            transaction.execute_batch(SCHEMA).map_err(sql_error)?;
            transaction
                .pragma_update(None, "application_id", APPLICATION_ID)
                .map_err(sql_error)?;
            transaction
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(sql_error)?;
            transaction.commit().map_err(sql_error)?;
        } else if application != APPLICATION_ID || version != SCHEMA_VERSION {
            return Err(storage(
                if version > SCHEMA_VERSION {
                    "task_storage_version"
                } else {
                    "task_corrupt"
                },
                "Task database version is unsupported",
            ));
        }
        let journal: String = connection
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .map_err(sql_error)?;
        if !journal.eq_ignore_ascii_case("wal") {
            return Err(storage("task_storage", "Task storage WAL is unavailable"));
        }
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path,
            secrets: Arc::new(secrets),
            settings,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn create(&self, record: TaskRecord) -> Result<TaskRecord, BoundaryError> {
        let connection = self.connection.clone();
        let secrets = self.secrets.clone();
        tokio::task::spawn_blocking(move || {
            let connection = connection
                .lock()
                .map_err(|_| storage("task_storage", "Task database lock poisoned"))?;
            let raw = encode(&record, &secrets)?;
            let status = serde_json::to_value(record.status)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .ok_or_else(|| storage("task_storage", "Task status cannot be encoded"))?;
            connection
                .execute(
                    "INSERT INTO tasks(id,session_id,workspace,profile,status,revision,record) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        id(record.id.0),
                        id(record.session_id.0),
                        record.workspace.to_string_lossy(),
                        record.profile,
                        status,
                        i64::try_from(record.revision).unwrap_or(i64::MAX),
                        raw
                    ],
                )
                .map_err(|_| storage("task_exists", "Task already exists"))?;
            Ok(record)
        })
        .await
        .map_err(|_| storage("task_storage", "Task storage worker failed"))?
    }

    pub async fn load(&self, task_id: TaskId) -> Result<TaskRecord, BoundaryError> {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let connection = connection
                .lock()
                .map_err(|_| storage("task_storage", "Task database lock poisoned"))?;
            load_connection(&connection, task_id)
        })
        .await
        .map_err(|_| storage("task_storage", "Task storage worker failed"))?
    }

    pub async fn list(&self) -> Result<Vec<TaskRecord>, BoundaryError> {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let connection = connection
                .lock()
                .map_err(|_| storage("task_storage", "Task database lock poisoned"))?;
            let mut statement = connection
                .prepare("SELECT record FROM tasks ORDER BY revision DESC,id LIMIT ?1")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([i64::try_from(MAX_TASK_ROWS).unwrap_or(i64::MAX)], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(sql_error)?;
            let mut tasks = Vec::new();
            for row in rows {
                tasks.push(decode(&row.map_err(sql_error)?)?);
            }
            Ok(tasks)
        })
        .await
        .map_err(|_| storage("task_storage", "Task storage worker failed"))?
    }

    pub async fn save(
        &self,
        mut record: TaskRecord,
        expected_revision: u64,
    ) -> Result<TaskRecord, BoundaryError> {
        record.revision = expected_revision.saturating_add(1);
        record.validate()?;
        let connection = self.connection.clone();
        let secrets = self.secrets.clone();
        tokio::task::spawn_blocking(move || {
            let connection = connection
                .lock()
                .map_err(|_| storage("task_storage", "Task database lock poisoned"))?;
            write_connection(&connection, &record, &secrets, Some(expected_revision))
                .map_err(|_| storage("task_conflict", "Task revision changed before save"))?;
            Ok(record)
        })
        .await
        .map_err(|_| storage("task_storage", "Task storage worker failed"))?
    }

    pub async fn acquire_lease(
        &self,
        task_id: TaskId,
        owner: String,
        now_ms: u64,
        ttl_ms: Option<u64>,
    ) -> Result<TaskLease, BoundaryError> {
        if owner.is_empty()
            || owner.len() > 128
            || owner.chars().any(crate::text::is_terminal_control)
        {
            return Err(storage("invalid_task_lease", "Task owner is invalid"));
        }
        let ttl = ttl_ms.unwrap_or(self.settings.default_lease_ttl_ms);
        if ttl == 0 {
            return Err(storage("invalid_task_lease", "Task lease must be positive"));
        }
        let expires = now_ms.saturating_add(ttl);
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| storage("task_storage", "Task database lock poisoned"))?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            load_connection(&transaction, task_id)?;
            let current: Option<(String, u64)> = transaction
                .query_row(
                    "SELECT owner,expires_at_ms FROM task_leases WHERE task_id=?1",
                    [id(task_id.0)],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(sql_error)?;
            if let Some((current, until)) = current
                && current != owner
                && until > now_ms
            {
                return Err(storage("task_leased", "Task is owned by another worker"));
            }
            transaction
                .execute(
                    "INSERT INTO task_leases(task_id,owner,expires_at_ms) VALUES(?1,?2,?3) ON CONFLICT(task_id) DO UPDATE SET owner=excluded.owner,expires_at_ms=excluded.expires_at_ms",
                    params![id(task_id.0), owner.clone(), i64::try_from(expires).unwrap_or(i64::MAX)],
                )
                .map_err(sql_error)?;
            transaction.commit().map_err(sql_error)?;
            Ok(TaskLease {
                task_id,
                owner,
                expires_at_ms: expires,
            })
        })
        .await
        .map_err(|_| storage("task_storage", "Task storage worker failed"))?
    }

    pub async fn release_lease(&self, lease: &TaskLease) -> Result<(), BoundaryError> {
        let connection = self.connection.clone();
        let task_id = lease.task_id;
        let owner = lease.owner.clone();
        tokio::task::spawn_blocking(move || {
            let connection = connection
                .lock()
                .map_err(|_| storage("task_storage", "Task database lock poisoned"))?;
            connection
                .execute(
                    "DELETE FROM task_leases WHERE task_id=?1 AND owner=?2",
                    params![id(task_id.0), owner],
                )
                .map_err(sql_error)?;
            Ok(())
        })
        .await
        .map_err(|_| storage("task_storage", "Task storage worker failed"))?
    }

    pub async fn renew_lease(
        &self,
        lease: &mut TaskLease,
        now_ms: u64,
        ttl_ms: Option<u64>,
    ) -> Result<(), BoundaryError> {
        let ttl = ttl_ms.unwrap_or(self.settings.default_lease_ttl_ms);
        if ttl == 0 {
            return Err(storage("invalid_task_lease", "Task lease must be positive"));
        }
        let expires = now_ms.saturating_add(ttl);
        let connection = self.connection.clone();
        let task_id = lease.task_id;
        let owner = lease.owner.clone();
        tokio::task::spawn_blocking(move || {
            let connection = connection
                .lock()
                .map_err(|_| storage("task_storage", "Task database lock poisoned"))?;
            let changed = connection
                .execute(
                    "UPDATE task_leases SET expires_at_ms=?3 WHERE task_id=?1 AND owner=?2",
                    params![
                        id(task_id.0),
                        owner.clone(),
                        i64::try_from(expires).unwrap_or(i64::MAX)
                    ],
                )
                .map_err(sql_error)?;
            if changed != 1 {
                return Err(storage("task_lease_lost", "Task worker lease was lost"));
            }
            Ok(())
        })
        .await
        .map_err(|_| storage("task_storage", "Task storage worker failed"))??;
        lease.expires_at_ms = expires;
        Ok(())
    }

    /// Atomically check the worker lease, require a queued task, mark it running
    /// and record the dispatch intent for the new attempt in one transaction.
    pub async fn begin_attempt(
        &self,
        lease: &TaskLease,
        run_id: RunId,
        now_ms: u64,
        now: String,
    ) -> Result<TaskDispatch, BoundaryError> {
        let connection = self.connection.clone();
        let secrets = self.secrets.clone();
        let task_id = lease.task_id;
        let owner = lease.owner.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| storage("task_storage", "Task database lock poisoned"))?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            active_lease(&transaction, task_id, &owner, now_ms)?;
            let mut record = load_connection(&transaction, task_id)?;
            if record.status != TaskStatus::Queued {
                return Err(storage(
                    "invalid_task_state",
                    "Task is not queued for another attempt",
                ));
            }
            let prior: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM task_dispatches WHERE task_id=?1",
                    [id(task_id.0)],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            let attempt = u32::try_from(prior)
                .unwrap_or(u32::MAX)
                .saturating_add(1);
            record.status = TaskStatus::Running;
            record.last_run_id = Some(run_id);
            record.updated_at = now;
            record.revision = record.revision.saturating_add(1);
            record.validate()?;
            write_connection(&transaction, &record, &secrets, None)?;
            transaction
                .execute(
                    "INSERT INTO task_dispatches(task_id,attempt,run_id,state) VALUES(?1,?2,?3,'intent')",
                    params![id(task_id.0), i64::from(attempt), id(run_id.0)],
                )
                .map_err(sql_error)?;
            transaction.commit().map_err(sql_error)?;
            Ok(TaskDispatch {
                task_id,
                attempt,
                run_id,
            })
        })
        .await
        .map_err(|_| storage("task_storage", "Task storage worker failed"))?
    }

    /// Mark a recorded dispatch intent as actively running.
    pub async fn mark_running(
        &self,
        dispatch: &TaskDispatch,
        lease: &TaskLease,
        now_ms: u64,
    ) -> Result<(), BoundaryError> {
        let connection = self.connection.clone();
        let task_id = lease.task_id;
        let owner = lease.owner.clone();
        let attempt = dispatch.attempt;
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| storage("task_storage", "Task database lock poisoned"))?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            active_lease(&transaction, task_id, &owner, now_ms)?;
            let changed = transaction
                .execute(
                    "UPDATE task_dispatches SET state='running' WHERE task_id=?1 AND attempt=?2 AND state='intent'",
                    params![id(task_id.0), i64::from(attempt)],
                )
                .map_err(sql_error)?;
            if changed != 1 {
                return Err(storage(
                    "task_dispatch_conflict",
                    "Task dispatch changed before start",
                ));
            }
            transaction.commit().map_err(sql_error)
        })
        .await
        .map_err(|_| storage("task_storage", "Task storage worker failed"))?
    }

    /// Record the terminal outcome of an attempt inside one transaction.
    /// A run whose status is cancelled or whose persistence was not saved is
    /// treated as an unknown side effect: the task waits for resolution and
    /// the dispatch is marked `unknown` rather than replayed.
    pub async fn finish_attempt(
        &self,
        lease: &TaskLease,
        dispatch: &TaskDispatch,
        summary: TaskRunSummary,
        metrics: &RunMetrics,
        now: String,
    ) -> Result<TaskRecord, BoundaryError> {
        summary.validate()?;
        let connection = self.connection.clone();
        let secrets = self.secrets.clone();
        let metrics = metrics.clone();
        let task_id = lease.task_id;
        let owner = lease.owner.clone();
        let attempt = dispatch.attempt;
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| storage("task_storage", "Task database lock poisoned"))?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let current: Option<String> = transaction
                .query_row(
                    "SELECT owner FROM task_leases WHERE task_id=?1",
                    [id(task_id.0)],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            if current.as_deref() != Some(owner.as_str()) {
                return Err(storage("task_lease_lost", "Task worker lease was lost"));
            }
            let mut record = load_connection(&transaction, task_id)?;
            if !matches!(
                record.status,
                TaskStatus::Running | TaskStatus::Paused | TaskStatus::Cancelled
            ) || record.last_run_id != Some(summary.run_id)
            {
                return Err(storage(
                    "task_dispatch_conflict",
                    "Task is not waiting for this Run result",
                ));
            }
            let run_completed = summary.status == RunStatus::Completed && summary.persistence_saved;
            let requested_status = record.status;
            let mut dispatch_state = if run_completed {
                DispatchState::Completed
            } else {
                DispatchState::Failed
            };
            record.last_result = Some(summary.clone());
            record.updated_at = now;
            match summary.status {
                RunStatus::Completed if summary.persistence_saved => {
                    record.status = TaskStatus::Completed;
                    record.failure = None;
                }
                RunStatus::Completed => {
                    dispatch_state = DispatchState::Unknown;
                    record.status = TaskStatus::WaitingUnknown;
                    record.failure = Some(ErrorInfo {
                        code: "task_outcome_unknown".into(),
                        message: "the Run completed but its outcome was not persisted".into(),
                    });
                }
                RunStatus::Cancelled => {
                    dispatch_state = DispatchState::Unknown;
                    record.status = TaskStatus::WaitingUnknown;
                    record.failure = Some(ErrorInfo {
                        code: "task_outcome_unknown".into(),
                        message: "the Run was cancelled before its outcome was recorded".into(),
                    });
                }
                RunStatus::Failed | RunStatus::NeedsConfirmation | RunStatus::NeedsInput => {
                    record.status = TaskStatus::Failed;
                    record.failure = Some(ErrorInfo {
                        code: summary
                            .error_code
                            .clone()
                            .unwrap_or_else(|| "task_run_failed".into()),
                        message: summary
                            .stop_reason
                            .clone()
                            .unwrap_or_else(|| "the task Run failed".into()),
                    });
                }
                RunStatus::LimitReached => {
                    record.status = TaskStatus::Failed;
                    record.failure = Some(ErrorInfo {
                        code: "task_budget".into(),
                        message: summary
                            .stop_reason
                            .clone()
                            .unwrap_or_else(|| "the task Run reached its limits".into()),
                    });
                }
            }
            if let Some(error_code) = summary.error_code.as_deref()
                && error_code.contains("unknown")
                && record.status == TaskStatus::Failed
            {
                dispatch_state = DispatchState::Unknown;
                record.status = TaskStatus::WaitingUnknown;
                record.failure = Some(ErrorInfo {
                    code: "task_outcome_unknown".into(),
                    message: summary
                        .stop_reason
                        .clone()
                        .unwrap_or_else(|| "the Run outcome is unknown".into()),
                });
            }
            if requested_status == TaskStatus::Cancelled {
                record.status = TaskStatus::Cancelled;
                record.failure = None;
            } else if requested_status == TaskStatus::Paused {
                record.status = TaskStatus::Paused;
            }
            let _observed_requests = metrics
                .execution_requests
                .saturating_add(metrics.review_requests)
                .saturating_add(metrics.compaction_requests);
            record.revision = record.revision.saturating_add(1);
            record.validate()?;
            write_connection(&transaction, &record, &secrets, None)?;
            transaction
                .execute(
                    "UPDATE task_dispatches SET state=?3 WHERE task_id=?1 AND attempt=?2",
                    params![
                        id(task_id.0),
                        i64::from(attempt),
                        dispatch_state_label(dispatch_state),
                    ],
                )
                .map_err(sql_error)?;
            transaction.commit().map_err(sql_error)?;
            Ok(record)
        })
        .await
        .map_err(|_| storage("task_storage", "Task storage worker failed"))?
    }

    /// Operator-driven status transition. Pause is allowed from Queued, Running
    /// or Blocked; resume only requeues a Paused task; cancel is allowed from
    /// any non-terminal state. Unknown outcomes are never requeued here.
    pub async fn transition(
        &self,
        task_id: TaskId,
        target: TaskStatus,
        now: String,
    ) -> Result<TaskRecord, BoundaryError> {
        let connection = self.connection.clone();
        let secrets = self.secrets.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| storage("task_storage", "Task database lock poisoned"))?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let mut record = load_connection(&transaction, task_id)?;
            let allowed = match target {
                TaskStatus::Paused => matches!(
                    record.status,
                    TaskStatus::Queued | TaskStatus::Running | TaskStatus::Blocked
                ),
                TaskStatus::Queued => record.status == TaskStatus::Paused,
                TaskStatus::Cancelled => !record.status.is_terminal(),
                _ => false,
            };
            if !allowed {
                return Err(storage(
                    "invalid_task_state",
                    "requested Task transition is not allowed from the current state",
                ));
            }
            record.status = target;
            if target == TaskStatus::Queued {
                record.failure = None;
                record.last_run_id = None;
            }
            record.updated_at = now;
            record.revision = record.revision.saturating_add(1);
            record.validate()?;
            write_connection(&transaction, &record, &secrets, None)?;
            transaction.commit().map_err(sql_error)?;
            Ok(record)
        })
        .await
        .map_err(|_| storage("task_storage", "Task storage worker failed"))?
    }

    /// Mark in-flight dispatches whose worker lease expired as `unknown` and
    /// move their tasks to `waiting_unknown`. Never replays a side effect:
    /// requeue requires an explicit `resolve_unknown` or `transition` call.
    pub async fn recover_unknown(
        &self,
        now_ms: u64,
        now: String,
    ) -> Result<Vec<TaskRecord>, BoundaryError> {
        let connection = self.connection.clone();
        let secrets = self.secrets.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| storage("task_storage", "Task database lock poisoned"))?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let mut statement = transaction
                .prepare(
                    "SELECT d.task_id,d.attempt,d.run_id FROM task_dispatches d LEFT JOIN task_leases l ON l.task_id=d.task_id WHERE d.state IN ('intent','running') AND (l.task_id IS NULL OR l.expires_at_ms<=?1) ORDER BY d.task_id",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map([i64::try_from(now_ms).unwrap_or(i64::MAX)], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            drop(statement);
            let mut recovered = Vec::new();
            for (task_id, attempt, run_id) in rows {
                transaction
                    .execute(
                        "UPDATE task_dispatches SET state='unknown' WHERE task_id=?1 AND attempt=?2",
                        params![task_id, attempt],
                    )
                    .map_err(sql_error)?;
                let task_id = task_id
                    .parse::<uuid::Uuid>()
                    .map(TaskId)
                    .map_err(|_| storage("task_corrupt", "Task ID is invalid"))?;
                let run_id = run_id
                    .parse::<uuid::Uuid>()
                    .map(RunId)
                    .map_err(|_| storage("task_corrupt", "Task Run ID is invalid"))?;
                let mut record = load_connection(&transaction, task_id)?;
                if record.status.is_terminal() {
                    continue;
                }
                let requested_status = record.status;
                record.last_run_id = Some(run_id);
                record.status = match requested_status {
                    TaskStatus::Cancelled => TaskStatus::Cancelled,
                    TaskStatus::Paused => TaskStatus::Paused,
                    _ => TaskStatus::WaitingUnknown,
                };
                if record.status == TaskStatus::WaitingUnknown {
                    record.failure = Some(ErrorInfo {
                        code: "task_outcome_unknown".into(),
                        message: "worker stopped before the Run outcome was recorded".into(),
                    });
                }
                record.updated_at = now.clone();
                record.revision = record.revision.saturating_add(1);
                record.validate()?;
                write_connection(&transaction, &record, &secrets, None)?;
                recovered.push(record);
            }
            transaction.commit().map_err(sql_error)?;
            Ok(recovered)
        })
        .await
        .map_err(|_| storage("task_storage", "Task storage worker failed"))?
    }

    /// Resolve a task waiting on an unknown Run outcome. `retry` requeues the
    /// task for a fresh attempt; otherwise the task is blocked for external
    /// input. Only explicit resolution requeues — never automatic replay.
    pub async fn resolve_unknown(
        &self,
        task_id: TaskId,
        retry: bool,
        now: String,
    ) -> Result<TaskRecord, BoundaryError> {
        let connection = self.connection.clone();
        let secrets = self.secrets.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| storage("task_storage", "Task database lock poisoned"))?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let mut record = load_connection(&transaction, task_id)?;
            if record.status != TaskStatus::WaitingUnknown {
                return Err(storage(
                    "invalid_task_state",
                    "Task is not waiting for an unknown Run outcome",
                ));
            }
            if retry {
                record.status = TaskStatus::Queued;
                record.failure = None;
                record.last_run_id = None;
            } else {
                record.status = TaskStatus::Blocked;
            }
            record.updated_at = now;
            record.revision = record.revision.saturating_add(1);
            record.validate()?;
            write_connection(&transaction, &record, &secrets, None)?;
            transaction.commit().map_err(sql_error)?;
            Ok(record)
        })
        .await
        .map_err(|_| storage("task_storage", "Task storage worker failed"))?
    }
}
