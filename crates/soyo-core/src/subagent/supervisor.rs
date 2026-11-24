//! Parent-supervised child subagent orchestration (`F-09`).

use super::{
    CancelArgs, DiffArgs, MergeArgs, MessageArgs, PersistentSubagentJob, SpawnArgs, StatusArgs,
    SubagentConfig, SubagentDiffReport, SubagentFileChange, SubagentIsolation, SubagentLimits,
    SubagentMergeReceipt, SubagentPersistence, SubagentSnapshot, SubagentState, SubagentStepTrace,
    WaitArgs,
    workspace::{IsolatedWorkspace, MergeOutcome},
};
use crate::{
    approvals::Preauthorizations,
    boundary::{Cancellation, Model, Store, StoreChange},
    compaction::CompactionConfig,
    context_builder::ContextBudget,
    contract::{
        ApprovalRequest, Artifact, BoundaryError, ErrorInfo, PermissionMode, Run, RunStatus,
        Session, SessionId, ToolResult, Usage, WorkMode, error_codes,
    },
    credentials::SecretRegistry,
    metrics::{Metrics, RunMetrics},
    permissions::Policy,
    review::{ReviewConfig, SharedBudget},
    runtime::{
        AgentRuntime, ApprovalHandler, RuntimeLimits, RuntimeSetup, services::RuntimeServices,
    },
    task_queue::{TaskId, TaskRecord, TaskSeed, TaskStatus, timestamp},
    tools::{
        exec::ExecConfig,
        read::{ReadLimits, ReadTools},
    },
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Notify, task::JoinHandle};
use uuid::Uuid;

const MAX_TRACE_ENTRIES: usize = 256;
const SHUTDOWN_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

struct ChildRuntimeSpec<'a> {
    id: &'a str,
    label: &'a str,
    child_mode: WorkMode,
    isolation: SubagentIsolation,
    allow_exec: bool,
    allowed_rel_paths: &'a [PathBuf],
    workspace: &'a IsolatedWorkspace,
    parent_perm: PermissionMode,
    prior_requests: u64,
    prior_tools: u64,
    timeout: Duration,
    cancel: Arc<SubagentCancel>,
    store: Arc<SubagentStore>,
    session_id: SessionId,
    subagent_config: Option<SubagentConfig>,
}

fn failure(code: &str, message: impl Into<String>) -> BoundaryError {
    BoundaryError::new(code, message)
}

fn sanitize_line(input: &str, max_bytes: usize) -> String {
    let cleaned: String = input
        .chars()
        .map(|ch| {
            if matches!(ch, '\n' | '\r' | '\t') || crate::text::is_terminal_control(ch) {
                ' '
            } else {
                ch
            }
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.len() <= max_bytes {
        return trimmed.to_owned();
    }
    let mut end = max_bytes;
    while !trimmed.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}...", &trimmed[..end])
}

fn summarize_tool_arguments(raw: &str) -> String {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw) else {
        return sanitize_line(raw, 80);
    };
    for key in ["path", "command", "pattern", "query", "url", "subagent_id"] {
        if let Some(value) = map.get(key) {
            if let Some(text) = value.as_str() {
                return sanitize_line(text, 80);
            }
            if let Some(arr) = value.as_array() {
                let joined = arr
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                if !joined.is_empty() {
                    return sanitize_line(&joined, 80);
                }
            }
        }
    }
    String::new()
}

fn truncate_summary(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}...", &text[..end])
}

fn is_plan_or_read_only(mode: WorkMode, read_only: bool) -> bool {
    read_only || mode == WorkMode::Plan
}

fn validate_non_escalation(
    parent_mode: WorkMode,
    parent_read_only: bool,
    requested_mode: Option<WorkMode>,
    requested_isolation: Option<SubagentIsolation>,
    allow_exec: bool,
) -> Result<(WorkMode, SubagentIsolation), BoundaryError> {
    let parent_restricted = is_plan_or_read_only(parent_mode, parent_read_only);
    let default_isolation = if parent_restricted {
        SubagentIsolation::ReadOnly
    } else {
        SubagentIsolation::Isolated
    };
    let isolation = requested_isolation.unwrap_or(default_isolation);
    let child_mode = requested_mode.unwrap_or(parent_mode);

    if parent_mode == WorkMode::Plan && child_mode != WorkMode::Plan {
        return Err(failure(
            error_codes::MODE_DENIED,
            "PLAN mode cannot delegate to non-PLAN subagents",
        ));
    }
    if parent_restricted && isolation == SubagentIsolation::Isolated {
        return Err(failure(
            error_codes::MODE_DENIED,
            "read-only / PLAN parent cannot spawn isolated writable subagents",
        ));
    }
    if parent_restricted && allow_exec {
        return Err(failure(
            error_codes::MODE_DENIED,
            "read-only / PLAN parent cannot enable exec for subagents",
        ));
    }
    if child_mode == WorkMode::Plan && isolation == SubagentIsolation::Isolated {
        return Err(failure(
            error_codes::MODE_DENIED,
            "PLAN subagents must use read_only isolation",
        ));
    }
    if child_mode == WorkMode::Plan && allow_exec {
        return Err(failure(
            error_codes::MODE_DENIED,
            "PLAN subagents cannot enable exec",
        ));
    }
    Ok((child_mode, isolation))
}

/// Inputs for persisting a child subagent when no live parent runtime exists.
/// The Goal worker uses this path so a delegated Goal step (`D-112`) reuses the
/// same validation, workspace and TaskStore record as `subagent_spawn`.
pub struct PersistentSpawnRequest<'a> {
    pub parent_session_id: SessionId,
    pub parent_workspace: &'a Path,
    pub parent_profile: &'a str,
    pub parent_mode: WorkMode,
    pub parent_perm: PermissionMode,
    pub parent_read_only: bool,
    pub parent_policy: &'a Policy,
    pub limits: &'a SubagentLimits,
    /// Host-only paths that must never enter the isolated child workspace.
    pub extra_protected: &'a [PathBuf],
    /// Delegation depth of the parent runtime; the child is stored one deeper.
    pub parent_depth: usize,
    pub args: &'a SpawnArgs,
}

/// Create and persist one queued persistent-subagent job. The caller remains
/// responsible for driving it through `run_subagent_once`.
pub async fn spawn_persistent_job(
    persistence: &SubagentPersistence,
    request: PersistentSpawnRequest<'_>,
) -> Result<SubagentSnapshot, BoundaryError> {
    let PersistentSpawnRequest {
        parent_session_id,
        parent_workspace,
        parent_profile,
        parent_mode,
        parent_perm,
        parent_read_only,
        parent_policy,
        limits,
        extra_protected,
        parent_depth,
        args,
    } = request;
    let (child_mode, isolation) = validate_non_escalation(
        parent_mode,
        parent_read_only,
        args.mode,
        args.isolation,
        args.allow_exec,
    )?;
    if args.timeout > limits.max_timeout {
        return Err(failure(
            error_codes::SUBAGENT_LIMIT,
            "requested subagent timeout exceeds configured limit",
        ));
    }

    let mut protected = parent_policy.protected_paths()?;
    for path in extra_protected {
        protected.push(crate::execution::normalize_path(path)?);
    }
    protected.sort();
    protected.dedup();
    let write_roots = parent_policy.roots().to_vec();
    let task_id = TaskId::new();
    let job_dir = persistence.allocate_job_dir(task_id)?;

    let workspace = match isolation {
        SubagentIsolation::ReadOnly => IsolatedWorkspace::read_only(parent_workspace),
        SubagentIsolation::Isolated => {
            IsolatedWorkspace::create_isolated(parent_workspace, &protected, &write_roots, limits)
        }
    };
    let workspace = match workspace {
        Ok(workspace) => workspace,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&job_dir);
            return Err(error);
        }
    };
    let baseline = if isolation == SubagentIsolation::Isolated {
        match persistence.write_baseline(&job_dir, &workspace) {
            Ok(path) => Some(path),
            Err(error) => {
                let _ = workspace.cleanup();
                let _ = persistence.remove_job_dir(task_id);
                return Err(error);
            }
        }
    } else {
        None
    };

    let allowed_paths = args
        .allowed_paths
        .iter()
        .filter_map(|path| path.to_str().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    let job = PersistentSubagentJob {
        parent_session_id,
        parent_workspace: workspace.parent_workspace.clone(),
        label: args.label.clone(),
        isolation,
        isolation_kind: workspace.kind,
        allow_exec: args.allow_exec,
        allowed_paths: allowed_paths.clone(),
        timeout_seconds: args.timeout.as_secs(),
        depth: parent_depth.saturating_add(1),
        workspace_root: workspace.child_dir.clone(),
        scratch_root: workspace.outer_dir.clone(),
        git_common_dir: workspace.git_common_dir.clone(),
        baseline_path: baseline,
        summary: None,
        changed_files: Vec::new(),
        conflicts: Vec::new(),
        merge_receipts: Vec::new(),
        step_traces: Vec::new(),
        usage: None,
        execution_requests: 0,
        tool_calls: 0,
        merged: false,
        discarded: false,
    };
    let record = TaskRecord::new_subagent_with_id(
        TaskSeed {
            session_id: parent_session_id,
            workspace: workspace.parent_workspace.clone(),
            profile: parent_profile.to_owned(),
            mode: child_mode,
            permissions: parent_perm,
            input: args.task.clone(),
            now: timestamp()?,
        },
        task_id,
        job,
    )?;
    let record = match persistence.store().create(record).await {
        Ok(record) => record,
        Err(error) => {
            let _ = workspace.cleanup();
            let _ = persistence.remove_job_dir(task_id);
            return Err(error);
        }
    };
    // Cleanup of a completed-but-unmerged persistent workspace is an explicit
    // operator action (`soyo subagent cancel --discard` or a merge with
    // `discard_after`), so nothing is registered in this run's in-memory child
    // table or run-scoped counters.
    Ok(SubagentSnapshot {
        id: record.id.0.to_string(),
        label: args.label.clone(),
        state: SubagentState::Running,
        work_mode: child_mode,
        isolation,
        isolation_kind: workspace.kind,
        allow_exec: args.allow_exec,
        allowed_paths,
        summary: None,
        error: None,
        usage: None,
        execution_requests: 0,
        tool_calls: 0,
        step_traces: Vec::new(),
        changed_files: Vec::new(),
        merge_receipts: Vec::new(),
        persistent: true,
    })
}

/// Parent configuration snapshot shared lazily with the subagent supervisor so
/// runtime builder ordering (`with_secret_registry`, `with_context_settings`, etc.)
/// does not break inheritance.
#[derive(Clone)]
pub(crate) struct ParentRuntimeTemplate {
    pub raw_model: Arc<dyn Model>,
    pub policy: Arc<Policy>,
    pub read_limits: ReadLimits,
    pub review: ReviewConfig,
    pub exec: ExecConfig,
    pub known_secrets: Vec<String>,
    pub secret_source: Arc<RwLock<Option<Arc<SecretRegistry>>>>,
    pub context_settings: Arc<RwLock<Option<(ContextBudget, bool, CompactionConfig)>>>,
    pub routing: Option<Arc<crate::provider::routing::RoutingCoordinator>>,
    pub confirmation: Option<Arc<dyn ApprovalHandler>>,
    pub limits: RuntimeLimits,
    pub parent_budget: SharedBudget,
    pub parent_metrics: Arc<Metrics>,
    pub parent_services: Arc<RuntimeServices>,
    pub parent_cancel: Arc<dyn Cancellation>,
}

/// In-memory session and step-trace store for one child run. Also used by the
/// persistent-subagent worker, which has no parent runtime to inherit a store
/// from.
pub struct SubagentStore {
    session: Mutex<Option<Session>>,
    traces: Mutex<Vec<SubagentStepTrace>>,
}

impl SubagentStore {
    pub fn new(session: Session, prior_traces: Vec<SubagentStepTrace>) -> Self {
        Self {
            session: Mutex::new(Some(session)),
            traces: Mutex::new(prior_traces),
        }
    }

    pub fn take_traces(&self) -> Vec<SubagentStepTrace> {
        self.traces
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

impl Store for SubagentStore {
    fn load_session<'a>(
        &'a self,
        id: SessionId,
    ) -> crate::boundary::Operation<'a, Option<Session>> {
        Box::pin(async move {
            let guard = self
                .session
                .lock()
                .map_err(|_| failure(error_codes::STORAGE_ERROR, "subagent store poisoned"))?;
            Ok(guard.as_ref().filter(|s| s.id == id).cloned())
        })
    }

    fn commit<'a>(&'a self, changes: &'a [StoreChange]) -> crate::boundary::Operation<'a, ()> {
        Box::pin(async move {
            let mut traces = self
                .traces
                .lock()
                .map_err(|_| failure(error_codes::STORAGE_ERROR, "subagent trace lock poisoned"))?;
            let mut session = self.session.lock().map_err(|_| {
                failure(error_codes::STORAGE_ERROR, "subagent session lock poisoned")
            })?;
            for change in changes {
                match change {
                    StoreChange::Session(updated) => {
                        *session = Some(updated.clone());
                    }
                    StoreChange::History { session_id, item } => {
                        if let Some(current) = session.as_mut()
                            && current.id == *session_id
                        {
                            current.history.push(item.clone());
                        }
                    }
                    StoreChange::ToolIntent { call, .. } => {
                        let detail = summarize_tool_arguments(&call.arguments);
                        if let Some(existing) = traces
                            .iter_mut()
                            .find(|entry| entry.call_id == call.call_id)
                        {
                            existing.detail = detail;
                            existing.status = "running".into();
                        } else if traces.len() < MAX_TRACE_ENTRIES {
                            traces.push(SubagentStepTrace {
                                call_id: call.call_id.clone(),
                                tool: call.name.clone(),
                                detail,
                                status: "running".into(),
                            });
                        }
                    }
                    StoreChange::ToolFinished {
                        call_id, result, ..
                    } => {
                        let status = if result.ok {
                            "ok".to_owned()
                        } else {
                            result
                                .error
                                .as_ref()
                                .map_or_else(|| "error".into(), |err| err.code.clone())
                        };
                        if let Some(existing) =
                            traces.iter_mut().find(|entry| entry.call_id == *call_id)
                        {
                            existing.status = status;
                        }
                    }
                    _ => {}
                }
            }
            Ok(())
        })
    }
}

struct SubagentApprovalHandler {
    inner: Arc<dyn ApprovalHandler>,
    prefix: String,
}

impl ApprovalHandler for SubagentApprovalHandler {
    fn confirm<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        cancel: &'a dyn Cancellation,
    ) -> crate::boundary::Operation<'a, Option<bool>> {
        Box::pin(async move {
            let mut prefixed = request.clone();
            prefixed.operation_summary = format!("{} {}", self.prefix, request.operation_summary);
            self.inner.confirm(&prefixed, cancel).await
        })
    }
}

struct SubagentCancel {
    parent: Arc<dyn Cancellation>,
    local: AtomicBool,
    deadline: tokio::time::Instant,
    notify: Notify,
}

impl SubagentCancel {
    fn new(parent: Arc<dyn Cancellation>, timeout: Duration) -> Self {
        Self {
            parent,
            local: AtomicBool::new(false),
            deadline: tokio::time::Instant::now() + timeout,
            notify: Notify::new(),
        }
    }

    fn trigger_local(&self) {
        self.local.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_local_cancelled(&self) -> bool {
        self.local.load(Ordering::Acquire)
    }

    fn is_deadline_elapsed(&self) -> bool {
        tokio::time::Instant::now() >= self.deadline
    }
}

impl Cancellation for SubagentCancel {
    fn is_cancelled(&self) -> bool {
        self.parent.is_cancelled() || self.is_local_cancelled() || self.is_deadline_elapsed()
    }

    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            loop {
                if self.is_cancelled() {
                    return;
                }
                let notified = self.notify.notified();
                if self.is_cancelled() {
                    return;
                }
                tokio::select! {
                    biased;
                    () = self.parent.cancelled() => return,
                    () = notified => {},
                    () = tokio::time::sleep_until(self.deadline) => return,
                }
            }
        })
    }
}

struct ChildEntry {
    id: String,
    label: String,
    state: SubagentState,
    work_mode: WorkMode,
    isolation: SubagentIsolation,
    allow_exec: bool,
    allowed_paths: Vec<String>,
    allowed_rel_paths: Vec<PathBuf>,
    workspace: IsolatedWorkspace,
    session: Session,
    summary: Option<String>,
    error: Option<ErrorInfo>,
    usage: Option<Usage>,
    execution_requests: u64,
    tool_calls: u64,
    step_traces: Vec<SubagentStepTrace>,
    live_store: Option<Arc<SubagentStore>>,
    merge_receipts: Vec<SubagentMergeReceipt>,
    cancel: Option<Arc<SubagentCancel>>,
    handle: Option<JoinHandle<()>>,
    done_notify: Arc<Notify>,
    cleaned_up: bool,
}

impl ChildEntry {
    fn snapshot(&self, protected_paths: &[PathBuf], limits: &SubagentLimits) -> SubagentSnapshot {
        let step_traces = if self.state == SubagentState::Running {
            self.live_store
                .as_ref()
                .map_or_else(|| self.step_traces.clone(), |store| store.take_traces())
        } else {
            self.step_traces.clone()
        };
        let changed_files = if self.state != SubagentState::Running
            && self.isolation == SubagentIsolation::Isolated
            && !self.cleaned_up
        {
            self.workspace
                .compute_diff(&self.id, protected_paths, false, limits)
                .map(|report| report.changed_files)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        SubagentSnapshot {
            id: self.id.clone(),
            label: self.label.clone(),
            state: self.state,
            work_mode: self.work_mode,
            isolation: self.isolation,
            isolation_kind: self.workspace.kind,
            allow_exec: self.allow_exec,
            allowed_paths: self.allowed_paths.clone(),
            summary: self.summary.clone(),
            error: self.error.clone(),
            usage: self.usage.clone(),
            execution_requests: self.execution_requests,
            tool_calls: self.tool_calls,
            step_traces,
            changed_files,
            merge_receipts: self.merge_receipts.clone(),
            persistent: false,
        }
    }
}

/// Prepared target set and SHA-256 digest checked twice around `subagent_merge`.
pub struct PreparedSubagentMerge {
    pub targets: Vec<PathBuf>,
    pub binding: String,
    pub review: Value,
}

/// Spawn accounting shared by every supervisor on one delegation chain so
/// `max_concurrent`/`max_total` aggregate to the top-level parent run rather
/// than giving each level its own fresh allowance.
#[derive(Debug, Default)]
pub(crate) struct SubtreeCounters {
    total_spawned: AtomicUsize,
    running: AtomicUsize,
}

/// One chain-shared running slot. Releasing on drop means cancelled, aborted,
/// or panicking child tasks cannot leak concurrency budget.
struct RunningSlot(Arc<SubtreeCounters>);

impl Drop for RunningSlot {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Parent-run coordinator for bounded child subagent execution.
pub struct SubagentSupervisor {
    config: SubagentConfig,
    template: ParentRuntimeTemplate,
    counters: Arc<SubtreeCounters>,
    children: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<ChildEntry>>>>,
    spawn_order: Mutex<Vec<String>>,
    next_id: AtomicUsize,
    any_done: Notify,
    persistence: RwLock<Option<SubagentPersistence>>,
    owner_session: RwLock<Option<SessionId>>,
}

impl SubagentSupervisor {
    pub(crate) fn new(config: SubagentConfig, template: ParentRuntimeTemplate) -> Self {
        Self::with_counters(config, template, Arc::new(SubtreeCounters::default()))
    }

    /// Attach this supervisor to shared subtree counters (used for deeper
    /// levels of the delegation chain so `max_concurrent`/`max_total` stay
    /// aggregated at the top-level run).
    pub(crate) fn with_counters(
        config: SubagentConfig,
        template: ParentRuntimeTemplate,
        counters: Arc<SubtreeCounters>,
    ) -> Self {
        Self {
            config,
            template,
            counters,
            children: Mutex::new(BTreeMap::new()),
            spawn_order: Mutex::new(Vec::new()),
            next_id: AtomicUsize::new(1),
            any_done: Notify::new(),
            persistence: RwLock::new(None),
            owner_session: RwLock::new(None),
        }
    }

    /// Attach durable storage for `persistent = true` spawns and for reading or
    /// merging jobs from earlier runs.
    pub fn set_persistence(&self, persistence: Option<SubagentPersistence>) {
        if let Ok(mut slot) = self.persistence.write() {
            *slot = persistence;
        }
    }

    pub fn persistence(&self) -> Option<SubagentPersistence> {
        self.persistence.read().ok().and_then(|guard| guard.clone())
    }

    /// Bind this supervisor to the session that owns the current run. Persistent
    /// jobs from other sessions are reported as not found.
    pub fn set_owner_session(&self, session_id: SessionId) {
        if let Ok(mut slot) = self.owner_session.write() {
            *slot = Some(session_id);
        }
    }

    fn owner_session(&self) -> Option<SessionId> {
        self.owner_session.read().ok().and_then(|guard| *guard)
    }

    fn persistent_task_id(&self, id: &str) -> Option<TaskId> {
        id.parse::<uuid::Uuid>().ok().map(TaskId)
    }

    async fn persistent_entry(
        &self,
        id: &str,
    ) -> Result<Option<(TaskRecord, PersistentSubagentJob)>, BoundaryError> {
        let Some(persistence) = self.persistence() else {
            return Ok(None);
        };
        let Some(task_id) = self.persistent_task_id(id) else {
            return Ok(None);
        };
        match persistence.load_job(task_id, self.owner_session()).await {
            Ok(entry) => Ok(Some(entry)),
            Err(error) if error.is_code(error_codes::SUBAGENT_NOT_FOUND) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Release a `max_total` spawn reservation whose child never started.
    fn release_spawn_total(&self) {
        self.counters.total_spawned.fetch_sub(1, Ordering::AcqRel);
    }

    /// Reserve one chain-shared running slot.
    fn reserve_running(&self) -> Result<RunningSlot, BoundaryError> {
        self.counters
            .running
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < self.config.limits.max_concurrent).then_some(used + 1)
            })
            .map(|_| RunningSlot(self.counters.clone()))
            .map_err(|_| {
                failure(
                    error_codes::SUBAGENT_LIMIT,
                    "maximum concurrent subagents reached",
                )
            })
    }

    /// Reserve one running slot and one `max_total` spawn on the shared
    /// delegation-chain counters. The returned slot must be carried by the
    /// child's run task so it is released exactly when the child stops.
    fn reserve_spawn(&self) -> Result<RunningSlot, BoundaryError> {
        let slot = self.reserve_running()?;
        if self
            .counters
            .total_spawned
            .try_update(Ordering::AcqRel, Ordering::Acquire, |total| {
                (total < self.config.limits.max_total).then_some(total + 1)
            })
            .is_err()
        {
            // Dropping `slot` returns the running reservation.
            return Err(failure(
                error_codes::SUBAGENT_LIMIT,
                "maximum total subagents for this run reached",
            ));
        }
        Ok(slot)
    }

    /// Shared running/total counters passed to a child's own supervisor so
    /// concurrency and total-spawn limits account the whole delegation chain.
    fn child_counters(&self) -> Arc<SubtreeCounters> {
        self.counters.clone()
    }

    pub fn config(&self) -> &SubagentConfig {
        &self.config
    }

    /// Delegation depth (hops from the top-level run) of the runtime that owns
    /// this supervisor: `0` at the top-level run, `1` for its children, and so
    /// on up to `max_depth - 1`.
    pub fn depth(&self) -> usize {
        self.config.depth()
    }

    fn current_secret_registry(&self) -> Option<Arc<SecretRegistry>> {
        self.template
            .secret_source
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn sanitize_text(&self, text: &str) -> String {
        let mut secrets = self.template.known_secrets.clone();
        if let Some(registry) = self.current_secret_registry()
            && let Ok(dynamic) = registry.snapshot()
        {
            secrets.extend(dynamic);
        }
        let mut value = json!(text);
        let _ = crate::redaction::redact(&mut value, &secrets);
        value
            .as_str()
            .map_or_else(|| "[redacted]".into(), ToOwned::to_owned)
    }

    fn sanitize_snapshot(&self, mut snap: SubagentSnapshot) -> SubagentSnapshot {
        if let Some(summary) = snap.summary {
            let clean = self.sanitize_text(&summary);
            snap.summary = Some(truncate_summary(
                &clean,
                self.config.limits.max_summary_bytes,
            ));
        }
        if let Some(err) = snap.error.as_mut() {
            err.message = self.sanitize_text(&err.message);
        }
        for trace in &mut snap.step_traces {
            trace.detail = self.sanitize_text(&trace.detail);
            trace.status = self.sanitize_text(&trace.status);
        }
        snap
    }

    fn get_child_mutex(
        &self,
        id: &str,
    ) -> Result<Arc<tokio::sync::Mutex<ChildEntry>>, BoundaryError> {
        let guard = self
            .children
            .lock()
            .map_err(|_| failure(error_codes::SUBAGENT_ERROR, "subagent registry poisoned"))?;
        guard.get(id).cloned().ok_or_else(|| {
            failure(
                error_codes::SUBAGENT_NOT_FOUND,
                format!("unknown subagent `{id}`"),
            )
        })
    }

    fn list_children(&self) -> Result<Vec<Arc<tokio::sync::Mutex<ChildEntry>>>, BoundaryError> {
        let order = self
            .spawn_order
            .lock()
            .map_err(|_| failure(error_codes::SUBAGENT_ERROR, "subagent order lock poisoned"))?
            .clone();
        let guard = self
            .children
            .lock()
            .map_err(|_| failure(error_codes::SUBAGENT_ERROR, "subagent registry poisoned"))?;
        Ok(order
            .iter()
            .filter_map(|id| guard.get(id).cloned())
            .collect())
    }

    fn build_child_runtime(
        &self,
        spec: ChildRuntimeSpec<'_>,
    ) -> Result<AgentRuntime, BoundaryError> {
        let ChildRuntimeSpec {
            id,
            label,
            child_mode,
            isolation,
            allow_exec,
            allowed_rel_paths,
            workspace,
            parent_perm,
            prior_requests,
            prior_tools,
            timeout,
            cancel,
            store,
            session_id,
            subagent_config,
        } = spec;
        let mut extra_protected = self.template.policy.protected_paths()?;
        if let Some(outer) = &workspace.outer_dir {
            // Keep sibling scratch dirs outside the child's allowed scope; the
            // child workspace itself is `workspace.child_dir`, not `outer_dir`.
            let _ = outer;
        }
        let sandbox_snapshot = self.template.parent_services.execution_snapshot()?;
        if let Some((parent_sandbox, _)) = &sandbox_snapshot {
            for path in &parent_sandbox.protected_paths {
                if !extra_protected.contains(path) {
                    extra_protected.push(path.clone());
                }
            }
        }

        let mut child_policy = self.template.policy.derive_for_subagent(
            &workspace.child_dir,
            isolation == SubagentIsolation::Isolated,
            allowed_rel_paths,
            &extra_protected,
        )?;
        if !allow_exec {
            child_policy = child_policy.with_exec_denied();
        }
        let child_policy = Arc::new(child_policy);

        let files = Arc::new(
            ReadTools::new_restricted(
                &workspace.child_dir,
                child_policy.roots(),
                self.template.read_limits.clone(),
            )
            .map_err(|err| {
                failure(
                    error_codes::SUBAGENT_WORKSPACE_ERROR,
                    format!("cannot configure child read roots: {err}"),
                )
            })?,
        );

        let approval_handler: Option<Arc<dyn ApprovalHandler>> =
            self.template.confirmation.as_ref().map(|inner| {
                Arc::new(SubagentApprovalHandler {
                    inner: inner.clone(),
                    prefix: format!("[subagent:{id}:{label}]"),
                }) as Arc<dyn ApprovalHandler>
            });

        let remaining_requests = self
            .config
            .limits
            .max_model_requests
            .saturating_sub(usize::try_from(prior_requests).unwrap_or(usize::MAX))
            .min(self.template.limits.model_requests)
            .max(1);
        let remaining_tools = self
            .config
            .limits
            .max_tool_calls
            .saturating_sub(usize::try_from(prior_tools).unwrap_or(usize::MAX))
            .min(self.template.limits.tool_calls);

        let mut child_limits = self.template.limits.clone();
        child_limits.model_requests = remaining_requests;
        child_limits.tool_calls = remaining_tools;
        child_limits.active_time = child_limits.active_time.min(timeout);

        let mut child_exec = self.template.exec.clone();
        child_exec.execution = None;

        let setup = RuntimeSetup {
            run: Run {
                id: crate::contract::RunId::new(),
                session_id,
                work_mode: child_mode,
                permissions: parent_perm,
            },
            model: self.template.raw_model.clone(),
            store,
            policy: child_policy,
            files,
            cancellation: cancel,
            review: self.template.review.clone(),
            exec: child_exec,
            preauthorizations: Preauthorizations::default(),
            known_secrets: self.template.known_secrets.clone(),
            summary: format!("Subagent `{label}` ({id})"),
            confirmation: approval_handler,
            limits: child_limits,
        };

        let mut runtime = if isolation == SubagentIsolation::ReadOnly {
            AgentRuntime::new_read_only(setup)?
        } else {
            AgentRuntime::new(setup)?
        };
        if !allow_exec {
            runtime = runtime.with_exec_disabled();
        }
        let child_budget = SharedBudget::child(
            &self.template.parent_budget,
            remaining_requests,
            timeout.min(self.template.limits.active_time),
        );
        runtime = runtime.with_shared_budget(child_budget);
        if let Some(routing) = &self.template.routing {
            runtime = runtime.with_routing(routing.clone());
        }
        runtime = runtime.with_parent_tool_budget(
            self.template.parent_services.tool_budget_handle(),
            self.template.limits.tool_calls,
        )?;
        // `max_depth` permitting, let the child coordinate its own bounded
        // descendants. Limits, budgets, and cancellation keep aggregating to
        // the top-level parent via the same template handles.
        if let Some(child_config) = subagent_config {
            runtime =
                runtime.with_subagents_with_counters(Some(child_config), self.child_counters())?;
        }
        if let Some(registry) = self.current_secret_registry() {
            runtime = runtime.with_secret_registry(registry)?;
        }
        if let Ok(guard) = self.template.context_settings.read()
            && let Some((budget, automatic, compaction)) = guard.as_ref()
        {
            runtime = runtime.with_context_settings(*budget, *automatic, compaction.clone())?;
        }
        if allow_exec && let Some((parent_sandbox, parent_broker)) = sandbox_snapshot {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let child_sandbox =
                workspace.derive_child_sandbox(&parent_sandbox, allow_exec, home.as_deref());
            let mut child_broker = parent_broker;
            if let Some(broker) = child_broker.as_mut() {
                // Allocate an isolated per-child Unix socket in headless/Linux sandboxes.
                broker.unix_socket = None;
            }
            runtime = runtime.with_execution_settings(child_sandbox, child_broker)?;
        }
        Ok(runtime)
    }

    fn run_child_turn(
        self: Arc<Self>,
        entry_arc: Arc<tokio::sync::Mutex<ChildEntry>>,
        mut runtime: AgentRuntime,
        cancel: Arc<SubagentCancel>,
        store: Arc<SubagentStore>,
        slot: RunningSlot,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            // Carried for the whole turn so the chain-shared running slot is
            // released on completion, cancellation, abort, or panic.
            let _slot = slot;
            let mut child_session = {
                let guard = entry_arc.lock().await;
                guard.session.clone()
            };
            let runtime_handle = runtime.handle();
            let run_outcome = runtime.run(&mut child_session).await;
            let _ = runtime_handle.shutdown_background().await;
            let (child_metrics, _, _) = runtime
                .metrics_snapshot()
                .unwrap_or_else(|| (RunMetrics::default(), None, Vec::new()));
            self.template
                .parent_metrics
                .merge_child(&child_metrics, &[]);

            let traces = store.take_traces();
            let mut guard = entry_arc.lock().await;
            guard.live_store = None;
            guard.cancel = None;
            guard.handle = None;
            guard.step_traces = traces;
            guard.execution_requests = guard
                .execution_requests
                .saturating_add(child_metrics.execution_requests);
            guard.tool_calls = guard.tool_calls.saturating_add(child_metrics.tool_calls);
            guard.session = child_session;
            guard.usage = match (&guard.usage, &child_metrics.execution_usage) {
                (None, other) => other.clone(),
                (Some(a), Some(b)) => Some(merge_usage(a, b)),
                (Some(a), None) => Some(a.clone()),
            };

            let timed_out = cancel.is_deadline_elapsed();
            let local_cancelled = cancel.is_local_cancelled();
            let parent_cancelled = self.template.parent_cancel.is_cancelled();

            match run_outcome {
                Ok(result) => {
                    guard.summary = result.text;
                    guard.error = result.error;
                    guard.state = match result.status {
                        RunStatus::Completed => SubagentState::Completed,
                        RunStatus::Cancelled => {
                            if timed_out && !local_cancelled && !parent_cancelled {
                                if guard.error.is_none() {
                                    guard.error = Some(ErrorInfo {
                                        code: error_codes::TOOL_TIMEOUT.into(),
                                        message: "subagent execution timed out".into(),
                                    });
                                }
                                SubagentState::LimitReached
                            } else {
                                SubagentState::Cancelled
                            }
                        }
                        RunStatus::LimitReached => SubagentState::LimitReached,
                        RunStatus::NeedsConfirmation => SubagentState::NeedsConfirmation,
                        RunStatus::NeedsInput => SubagentState::NeedsInput,
                        RunStatus::Failed => SubagentState::Failed,
                    };
                }
                Err(err) => {
                    if local_cancelled || parent_cancelled {
                        guard.state = SubagentState::Cancelled;
                    } else if err.0.code == error_codes::LIMIT_REACHED
                        || err.0.code == error_codes::TOOL_TIMEOUT
                        || err.0.code == error_codes::MODEL_TIMEOUT
                        || timed_out
                    {
                        guard.state = SubagentState::LimitReached;
                    } else {
                        guard.state = SubagentState::Failed;
                    }
                    guard.error = Some(err.0);
                }
            }
            let notify = guard.done_notify.clone();
            drop(guard);
            // `_slot` frees this child's chain-shared running reservation as
            // the task ends; `message()` re-acquires one for reruns.
            self.any_done.notify_waiters();
            notify.notify_waiters();
        })
    }

    /// Spawn a bounded child subagent under strict parent constraints.
    pub async fn spawn(
        self: &Arc<Self>,
        parent_session: &Session,
        parent_mode: WorkMode,
        parent_perm: PermissionMode,
        parent_read_only: bool,
        args: &SpawnArgs,
    ) -> Result<SubagentSnapshot, BoundaryError> {
        if args.persistent {
            return self
                .spawn_persistent(
                    parent_session,
                    parent_mode,
                    parent_perm,
                    parent_read_only,
                    args,
                )
                .await;
        }
        let (child_mode, isolation) = validate_non_escalation(
            parent_mode,
            parent_read_only,
            args.mode,
            args.isolation,
            args.allow_exec,
        )?;
        if args.timeout > self.config.limits.max_timeout {
            return Err(failure(
                error_codes::SUBAGENT_LIMIT,
                "requested subagent timeout exceeds configured limit",
            ));
        }
        // `max_total` and `max_concurrent` account every descendant on the
        // delegation chain, not only this supervisor's direct children. The
        // returned slot is handed to the child's run task.
        let running_slot = self.reserve_spawn()?;
        let spawn_index = self.next_id.fetch_add(1, Ordering::Relaxed);

        let short = Uuid::now_v7().simple().to_string();
        let id = format!("sub-{spawn_index}-{}", &short[..6]);
        let protected = match self.template.policy.protected_paths() {
            Ok(paths) => paths,
            Err(err) => {
                self.release_spawn_total();
                return Err(err);
            }
        };
        let write_roots = self.template.policy.roots().to_vec();

        let workspace_res = match isolation {
            SubagentIsolation::ReadOnly => IsolatedWorkspace::read_only(&parent_session.workspace),
            SubagentIsolation::Isolated => IsolatedWorkspace::create_isolated(
                &parent_session.workspace,
                &protected,
                &write_roots,
                &self.config.limits,
            ),
        };
        let workspace = match workspace_res {
            Ok(ws) => ws,
            Err(err) => {
                self.release_spawn_total();
                return Err(err);
            }
        };

        let child_session = Session {
            id: SessionId::new(),
            workspace: workspace.child_dir.clone(),
            work_mode: child_mode,
            profile: parent_session.profile.clone(),
            protocol: parent_session.protocol,
            model: parent_session.model.clone(),
            history: vec![crate::contract::HistoryItem::UserText(args.task.clone())],
        };
        let cancel = Arc::new(SubagentCancel::new(
            self.template.parent_cancel.clone(),
            args.timeout,
        ));
        let store = Arc::new(SubagentStore::new(child_session.clone(), Vec::new()));

        let runtime = match self.build_child_runtime(ChildRuntimeSpec {
            id: &id,
            label: &args.label,
            child_mode,
            isolation,
            allow_exec: args.allow_exec,
            allowed_rel_paths: &args.allowed_paths,
            workspace: &workspace,
            parent_perm,
            prior_requests: 0,
            prior_tools: 0,
            timeout: args.timeout,
            cancel: cancel.clone(),
            store: store.clone(),
            session_id: child_session.id,
            subagent_config: self.config.for_child(),
        }) {
            Ok(runtime) => runtime,
            Err(err) => {
                self.release_spawn_total();
                let _ = workspace.cleanup();
                return Err(err);
            }
        };

        let allowed_paths = args
            .allowed_paths
            .iter()
            .filter_map(|p| p.to_str().map(ToOwned::to_owned))
            .collect();
        let done_notify = Arc::new(Notify::new());
        let entry_arc = Arc::new(tokio::sync::Mutex::new(ChildEntry {
            id: id.clone(),
            label: args.label.clone(),
            state: SubagentState::Running,
            work_mode: child_mode,
            isolation,
            allow_exec: args.allow_exec,
            allowed_paths,
            allowed_rel_paths: args.allowed_paths.clone(),
            workspace,
            session: child_session,
            summary: None,
            error: None,
            usage: None,
            execution_requests: 0,
            tool_calls: 0,
            step_traces: Vec::new(),
            live_store: Some(store.clone()),
            merge_receipts: Vec::new(),
            cancel: Some(cancel.clone()),
            handle: None,
            done_notify: done_notify.clone(),
            cleaned_up: false,
        }));

        {
            let mut map = self
                .children
                .lock()
                .map_err(|_| failure(error_codes::SUBAGENT_ERROR, "subagent registry poisoned"))?;
            map.insert(id.clone(), entry_arc.clone());
            self.spawn_order
                .lock()
                .map_err(|_| failure(error_codes::SUBAGENT_ERROR, "subagent order lock poisoned"))?
                .push(id.clone());
        }

        let supervisor = Arc::clone(self);
        let task_entry = entry_arc.clone();
        let join = tokio::spawn(supervisor.run_child_turn(
            task_entry,
            runtime,
            cancel,
            store,
            running_slot,
        ));

        {
            let mut guard = entry_arc.lock().await;
            if guard.state == SubagentState::Running {
                guard.handle = Some(join);
            }
        }

        if args.background {
            let guard = entry_arc.lock().await;
            return Ok(self.sanitize_snapshot(guard.snapshot(&protected, &self.config.limits)));
        }

        self.wait_for_entry(&entry_arc, None, self.template.parent_cancel.as_ref())
            .await?;
        let guard = entry_arc.lock().await;
        Ok(self.sanitize_snapshot(guard.snapshot(&protected, &self.config.limits)))
    }

    /// Persist the child job in the task store and return immediately. The
    /// parent run does not own or wait for this child; a background worker
    /// resumes it through the F-08 lease/dispatch machinery.
    async fn spawn_persistent(
        self: &Arc<Self>,
        parent_session: &Session,
        parent_mode: WorkMode,
        parent_perm: PermissionMode,
        parent_read_only: bool,
        args: &SpawnArgs,
    ) -> Result<SubagentSnapshot, BoundaryError> {
        let persistence = self.persistence().ok_or_else(|| {
            failure(
                "task_queue_disabled",
                "persistent subagents require the task queue; set [tasks] enabled = true",
            )
        })?;
        spawn_persistent_job(
            &persistence,
            PersistentSpawnRequest {
                parent_session_id: parent_session.id,
                parent_workspace: &parent_session.workspace,
                parent_profile: &parent_session.profile,
                parent_mode,
                parent_perm,
                parent_read_only,
                parent_policy: &self.template.policy,
                limits: &self.config.limits,
                extra_protected: &[],
                parent_depth: self.config.depth(),
                args,
            },
        )
        .await
    }

    /// Return current status and step traces for one subagent (or all when `subagent_id` is absent).
    pub async fn status(&self, args: &StatusArgs) -> Result<Value, BoundaryError> {
        let protected = self.template.policy.protected_paths()?;
        if !args.subagent_id.is_empty() {
            if let Ok(entry) = self.get_child_mutex(&args.subagent_id) {
                let guard = entry.lock().await;
                let snap = self.sanitize_snapshot(guard.snapshot(&protected, &self.config.limits));
                return serde_json::to_value(snap)
                    .map_err(|err| failure(error_codes::SUBAGENT_ERROR, err.to_string()));
            }
            if let Some((task, job)) = self.persistent_entry(&args.subagent_id).await? {
                let snap = self.sanitize_snapshot(job.snapshot(&task));
                return serde_json::to_value(snap)
                    .map_err(|err| failure(error_codes::SUBAGENT_ERROR, err.to_string()));
            }
            return Err(failure(
                error_codes::SUBAGENT_NOT_FOUND,
                format!("unknown subagent `{}`", args.subagent_id),
            ));
        }
        let entries = self.list_children()?;
        let mut list = Vec::with_capacity(entries.len());
        for entry in entries {
            let guard = entry.lock().await;
            list.push(self.sanitize_snapshot(guard.snapshot(&protected, &self.config.limits)));
        }
        if let Some(persistence) = self.persistence() {
            for (task, job) in persistence.list_jobs(self.owner_session()).await? {
                list.push(self.sanitize_snapshot(job.snapshot(&task)));
            }
        }
        serde_json::to_value(json!({ "subagents": list }))
            .map_err(|err| failure(error_codes::SUBAGENT_ERROR, err.to_string()))
    }

    async fn wait_for_entry(
        &self,
        entry_arc: &Arc<tokio::sync::Mutex<ChildEntry>>,
        deadline: Option<tokio::time::Instant>,
        cancel: &dyn Cancellation,
    ) -> Result<(), BoundaryError> {
        let notify = {
            let guard = entry_arc.lock().await;
            if guard.state.is_terminal() {
                return Ok(());
            }
            guard.done_notify.clone()
        };
        loop {
            let notified = notify.notified();
            {
                let guard = entry_arc.lock().await;
                if guard.state.is_terminal() {
                    return Ok(());
                }
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return Err(failure(error_codes::CANCELLED, "subagent wait cancelled"));
                }
                () = async {
                    match deadline {
                        Some(dl) => tokio::time::sleep_until(dl).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    return Ok(());
                }
                () = notified => {}
            }
        }
    }

    /// Wait for all or any listed subagents until completion or timeout.
    pub async fn wait(
        &self,
        args: &WaitArgs,
        cancel: &dyn Cancellation,
    ) -> Result<Value, BoundaryError> {
        let protected = self.template.policy.protected_paths()?;
        let mut persistent_snapshots = Vec::new();
        let entries: Vec<Arc<tokio::sync::Mutex<ChildEntry>>> = if args.subagent_ids.is_empty() {
            let mut running = Vec::new();
            for entry in self.list_children()? {
                let guard = entry.lock().await;
                if guard.state == SubagentState::Running || args.wait_all {
                    running.push(entry.clone());
                }
            }
            if let Some(persistence) = self.persistence() {
                for (task, job) in persistence.list_jobs(self.owner_session()).await? {
                    persistent_snapshots.push(self.sanitize_snapshot(job.snapshot(&task)));
                }
            }
            if running.is_empty() {
                self.list_children()?
            } else {
                running
            }
        } else {
            let mut out = Vec::new();
            for id in &args.subagent_ids {
                match self.get_child_mutex(id) {
                    Ok(entry) => out.push(entry),
                    Err(_) => match self.persistent_entry(id).await? {
                        Some((task, job)) => {
                            persistent_snapshots.push(self.sanitize_snapshot(job.snapshot(&task)));
                        }
                        None => {
                            return Err(failure(
                                error_codes::SUBAGENT_NOT_FOUND,
                                format!("unknown subagent `{id}`"),
                            ));
                        }
                    },
                }
            }
            out
        };

        let deadline = args.timeout.map(|d| tokio::time::Instant::now() + d);
        loop {
            let mut snapshots = persistent_snapshots.clone();
            let mut any_done = false;
            let mut all_done = true;
            for snapshot in &snapshots {
                if snapshot.state.is_terminal() {
                    any_done = true;
                } else {
                    all_done = false;
                }
            }
            let mut all_entries_done = true;
            for entry in &entries {
                let guard = entry.lock().await;
                if guard.state.is_terminal() {
                    any_done = true;
                } else {
                    all_done = false;
                    all_entries_done = false;
                }
                snapshots
                    .push(self.sanitize_snapshot(guard.snapshot(&protected, &self.config.limits)));
            }
            let condition_met = if entries.is_empty() && persistent_snapshots.is_empty() {
                true
            } else if args.wait_all {
                all_done
            } else {
                any_done
            };
            if condition_met {
                return serde_json::to_value(json!({ "completed": true, "subagents": snapshots }))
                    .map_err(|err| failure(error_codes::SUBAGENT_ERROR, err.to_string()));
            }
            if let Some(dl) = deadline
                && tokio::time::Instant::now() >= dl
            {
                return serde_json::to_value(json!({ "completed": false, "subagents": snapshots }))
                    .map_err(|err| failure(error_codes::SUBAGENT_ERROR, err.to_string()));
            }
            // Persistent jobs are driven by an external worker; there is no
            // in-process notifier to await, so report their current state
            // instead of blocking this run indefinitely.
            if entries.is_empty() || all_entries_done {
                return serde_json::to_value(json!({ "completed": false, "subagents": snapshots }))
                    .map_err(|err| failure(error_codes::SUBAGENT_ERROR, err.to_string()));
            }

            let notified = self.any_done.notified();
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return Err(failure(error_codes::CANCELLED, "subagent_wait cancelled"));
                }
                () = async {
                    match deadline {
                        Some(dl) => tokio::time::sleep_until(dl).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {}
                () = notified => {}
            }
        }
    }

    /// Send a follow-up prompt turn to an existing non-running, non-discarded child.
    pub async fn message(
        self: &Arc<Self>,
        parent_mode: WorkMode,
        parent_perm: PermissionMode,
        parent_read_only: bool,
        args: &MessageArgs,
    ) -> Result<SubagentSnapshot, BoundaryError> {
        let entry_arc = self.get_child_mutex(&args.subagent_id)?;
        let protected = self.template.policy.protected_paths()?;
        if args.timeout > self.config.limits.max_timeout {
            return Err(failure(
                error_codes::SUBAGENT_LIMIT,
                "requested timeout exceeds configured subagent limit",
            ));
        }

        let (runtime, cancel, store, running_slot) = {
            let mut guard = entry_arc.lock().await;
            if guard.state == SubagentState::Running {
                return Err(failure(
                    error_codes::SUBAGENT_CONFLICT,
                    "subagent is currently running",
                ));
            }
            if matches!(
                guard.state,
                SubagentState::Merged | SubagentState::Discarded
            ) || guard.cleaned_up
            {
                return Err(failure(
                    error_codes::SUBAGENT_CONFLICT,
                    "subagent workspace is already merged or discarded",
                ));
            }
            let (child_mode, isolation) = validate_non_escalation(
                parent_mode,
                parent_read_only,
                Some(guard.work_mode),
                Some(guard.isolation),
                guard.allow_exec,
            )?;
            if guard.execution_requests as usize >= self.config.limits.max_model_requests
                || guard.tool_calls as usize >= self.config.limits.max_tool_calls
            {
                return Err(failure(
                    error_codes::SUBAGENT_LIMIT,
                    "subagent per-child request or tool call budget is exhausted",
                ));
            }
            // Reserve a chain-shared running slot before the child re-enters
            // `Running`; the slot travels with the child's run task and is
            // released when it stops.
            let running_slot = self.reserve_running()?;
            guard
                .session
                .history
                .push(crate::contract::HistoryItem::UserText(args.message.clone()));
            let cancel = Arc::new(SubagentCancel::new(
                self.template.parent_cancel.clone(),
                args.timeout,
            ));
            let store = Arc::new(SubagentStore::new(
                guard.session.clone(),
                guard.step_traces.clone(),
            ));
            let runtime = match self.build_child_runtime(ChildRuntimeSpec {
                id: &guard.id,
                label: &guard.label,
                child_mode,
                isolation,
                allow_exec: guard.allow_exec,
                allowed_rel_paths: &guard.allowed_rel_paths,
                workspace: &guard.workspace,
                parent_perm,
                prior_requests: guard.execution_requests,
                prior_tools: guard.tool_calls,
                timeout: args.timeout,
                cancel: cancel.clone(),
                store: store.clone(),
                session_id: guard.session.id,
                subagent_config: self.config.for_child(),
            }) {
                Ok(runtime) => runtime,
                Err(err) => return Err(err),
            };
            guard.state = SubagentState::Running;
            guard.error = None;
            guard.live_store = Some(store.clone());
            guard.cancel = Some(cancel.clone());
            (runtime, cancel, store, running_slot)
        };

        let supervisor = Arc::clone(self);
        let task_entry = entry_arc.clone();
        let join = tokio::spawn(supervisor.run_child_turn(
            task_entry,
            runtime,
            cancel,
            store,
            running_slot,
        ));
        {
            let mut guard = entry_arc.lock().await;
            if guard.state == SubagentState::Running {
                guard.handle = Some(join);
            }
        }
        self.wait_for_entry(&entry_arc, None, self.template.parent_cancel.as_ref())
            .await?;
        let guard = entry_arc.lock().await;
        Ok(self.sanitize_snapshot(guard.snapshot(&protected, &self.config.limits)))
    }

    /// Cancel a running subagent and optionally discard its isolated workspace.
    pub async fn cancel(&self, args: &CancelArgs) -> Result<SubagentSnapshot, BoundaryError> {
        let protected = self.template.policy.protected_paths()?;
        if let Ok(entry_arc) = self.get_child_mutex(&args.subagent_id) {
            let join = {
                let mut guard = entry_arc.lock().await;
                if let Some(cancel) = &guard.cancel {
                    cancel.trigger_local();
                }
                guard.handle.take()
            };
            if let Some(mut handle) = join {
                let _ = tokio::time::timeout(SHUTDOWN_WAIT_TIMEOUT, &mut handle).await;
            }
            let mut guard = entry_arc.lock().await;
            if guard.state == SubagentState::Running {
                guard.state = SubagentState::Cancelled;
                guard.done_notify.notify_waiters();
                self.any_done.notify_waiters();
            }
            if args.discard && !guard.cleaned_up {
                guard.workspace.cleanup()?;
                guard.cleaned_up = true;
                guard.state = SubagentState::Discarded;
            }
            return Ok(self.sanitize_snapshot(guard.snapshot(&protected, &self.config.limits)));
        }

        let Some((task, job)) = self.persistent_entry(&args.subagent_id).await? else {
            return Err(failure(
                error_codes::SUBAGENT_NOT_FOUND,
                format!("unknown subagent `{}`", args.subagent_id),
            ));
        };
        let persistence = self.persistence().ok_or_else(|| {
            failure(
                error_codes::SUBAGENT_ERROR,
                "persistent subagent storage is unavailable",
            )
        })?;
        if !task.status.is_terminal() {
            persistence
                .store()
                .transition(task.id, TaskStatus::Cancelled, timestamp()?)
                .await?;
        }
        if args.discard && !job.discarded {
            if let Ok(workspace) = persistence.reopen(task.id, &job) {
                workspace.cleanup()?;
            } else if let Some(scratch) = &job.scratch_root {
                let _ = std::fs::remove_dir_all(scratch);
            }
            let _ = persistence.remove_job_dir(task.id);
            let (task, job) = persistence
                .update_job(task.id, |job| {
                    job.discarded = true;
                })
                .await?;
            return Ok(self.sanitize_snapshot(job.snapshot(&task)));
        }
        let (task, job) = persistence.load_job(task.id, self.owner_session()).await?;
        Ok(self.sanitize_snapshot(job.snapshot(&task)))
    }

    /// Inspect uncommitted isolated workspace diff for a finished child.
    pub async fn diff(&self, args: &DiffArgs) -> Result<SubagentDiffReport, BoundaryError> {
        let protected = self.template.policy.protected_paths()?;
        if let Ok(entry_arc) = self.get_child_mutex(&args.subagent_id) {
            let guard = entry_arc.lock().await;
            if guard.state == SubagentState::Running {
                return Err(failure(
                    error_codes::SUBAGENT_CONFLICT,
                    "cannot inspect diff while subagent is still running",
                ));
            }
            if guard.state == SubagentState::Discarded || guard.cleaned_up {
                return Err(failure(
                    error_codes::SUBAGENT_CONFLICT,
                    "subagent workspace was discarded",
                ));
            }
            if guard.isolation == SubagentIsolation::ReadOnly {
                return Err(failure(
                    error_codes::INVALID_ARGS,
                    "read_only subagents have no isolated workspace diff",
                ));
            }
            return guard.workspace.compute_diff(
                &guard.id,
                &protected,
                args.include_patch,
                &self.config.limits,
            );
        }
        let Some((task, job)) = self.persistent_entry(&args.subagent_id).await? else {
            return Err(failure(
                error_codes::SUBAGENT_NOT_FOUND,
                format!("unknown subagent `{}`", args.subagent_id),
            ));
        };
        if job.state(&task) == SubagentState::Running {
            return Err(failure(
                error_codes::SUBAGENT_CONFLICT,
                "cannot inspect diff while the persistent subagent job is queued or running",
            ));
        }
        if job.discarded {
            return Err(failure(
                error_codes::SUBAGENT_CONFLICT,
                "subagent workspace was discarded",
            ));
        }
        if job.isolation == SubagentIsolation::ReadOnly {
            return Err(failure(
                error_codes::INVALID_ARGS,
                "read_only subagents have no isolated workspace diff",
            ));
        }
        let persistence = self.persistence().ok_or_else(|| {
            failure(
                error_codes::SUBAGENT_ERROR,
                "persistent subagent storage is unavailable",
            )
        })?;
        let workspace = persistence.reopen(task.id, &job)?;
        workspace.compute_diff(
            &args.subagent_id,
            &protected,
            args.include_patch,
            &self.config.limits,
        )
    }

    /// Compute canonical targets and approval binding digest for `subagent_merge`.
    pub async fn prepare_merge(
        &self,
        args: &MergeArgs,
    ) -> Result<PreparedSubagentMerge, BoundaryError> {
        let protected = self.template.policy.protected_paths()?;
        if let Ok(entry_arc) = self.get_child_mutex(&args.subagent_id) {
            let guard = entry_arc.try_lock().map_err(|_| {
                failure(error_codes::SUBAGENT_CONFLICT, "subagent is currently busy")
            })?;
            return prepare_merge_locked(&guard, args, &protected, &self.config.limits);
        }
        let Some((task, job)) = self.persistent_entry(&args.subagent_id).await? else {
            return Err(failure(
                error_codes::SUBAGENT_NOT_FOUND,
                format!("unknown subagent `{}`", args.subagent_id),
            ));
        };
        let persistence = self.persistence().ok_or_else(|| {
            failure(
                error_codes::SUBAGENT_ERROR,
                "persistent subagent storage is unavailable",
            )
        })?;
        prepare_persistent_merge(
            &persistence,
            &task,
            &job,
            args,
            &protected,
            &self.config.limits,
        )
    }

    /// Re-verify approval binding, run 3-way conflict check, and atomically merge
    /// selected isolated files into the parent workspace.
    pub async fn merge(
        &self,
        args: &MergeArgs,
        expected_binding: &str,
        allowed_write_roots: &[PathBuf],
        protected_paths: &[PathBuf],
    ) -> Result<ToolResult, BoundaryError> {
        if let Ok(entry_arc) = self.get_child_mutex(&args.subagent_id) {
            let mut guard = entry_arc.lock().await;
            let current = prepare_merge_locked(&guard, args, protected_paths, &self.config.limits)?;
            if current.binding != expected_binding {
                return Err(failure(
                    error_codes::CONFLICT,
                    "subagent changes drifted after merge authorization",
                ));
            }
            return match guard.workspace.merge_into_parent(
                args.paths.as_deref(),
                allowed_write_roots,
                protected_paths,
                &self.config.limits,
            )? {
                MergeOutcome::Conflict(conflicts) => {
                    let data = serde_json::json!({
                        "subagent_id": guard.id,
                        "conflicts": conflicts,
                    });
                    Ok(ToolResult {
                        ok: false,
                        data: Some(data),
                        error: Some(ErrorInfo {
                            code: error_codes::SUBAGENT_CONFLICT.into(),
                            message: "parent workspace modified since subagent fork; merge aborted with zero writes"
                                .into(),
                        }),
                        truncated: false,
                        media_refs: Vec::new(),
                    })
                }
                MergeOutcome::Applied(receipts) => {
                    let artifacts = merge_artifacts(&receipts);
                    if !artifacts.is_empty() {
                        self.template
                            .parent_metrics
                            .merge_child(&RunMetrics::default(), &artifacts);
                    }
                    guard.merge_receipts.extend(receipts);
                    if args.discard_after {
                        guard.workspace.cleanup()?;
                        guard.cleaned_up = true;
                        guard.state = SubagentState::Merged;
                    } else {
                        let remaining = guard
                            .workspace
                            .compute_diff(&guard.id, protected_paths, false, &self.config.limits)?
                            .changed_files
                            .into_iter()
                            .any(|c| c.new_sha256 != inspect_matching_parent(&guard.workspace, &c));
                        if !remaining {
                            guard.state = SubagentState::Merged;
                        }
                    }
                    let snap = self
                        .sanitize_snapshot(guard.snapshot(protected_paths, &self.config.limits));
                    let data = serde_json::to_value(snap)
                        .map_err(|err| failure(error_codes::SUBAGENT_ERROR, err.to_string()))?;
                    Ok(ToolResult::success(Some(data)))
                }
            };
        }

        let Some((task, job)) = self.persistent_entry(&args.subagent_id).await? else {
            return Err(failure(
                error_codes::SUBAGENT_NOT_FOUND,
                format!("unknown subagent `{}`", args.subagent_id),
            ));
        };
        let persistence = self.persistence().ok_or_else(|| {
            failure(
                error_codes::SUBAGENT_ERROR,
                "persistent subagent storage is unavailable",
            )
        })?;
        let current = prepare_persistent_merge(
            &persistence,
            &task,
            &job,
            args,
            protected_paths,
            &self.config.limits,
        )?;
        if current.binding != expected_binding {
            return Err(failure(
                error_codes::CONFLICT,
                "persistent subagent changes drifted after merge authorization",
            ));
        }
        let workspace = persistence.reopen(task.id, &job)?;
        match workspace.merge_into_parent(
            args.paths.as_deref(),
            allowed_write_roots,
            protected_paths,
            &self.config.limits,
        )? {
            MergeOutcome::Conflict(conflicts) => {
                let stored = conflicts.clone();
                persistence
                    .update_job(task.id, |job| {
                        job.conflicts = stored.clone();
                    })
                    .await?;
                let data = serde_json::json!({
                    "subagent_id": args.subagent_id,
                    "conflicts": conflicts,
                });
                Ok(ToolResult {
                    ok: false,
                    data: Some(data),
                    error: Some(ErrorInfo {
                        code: error_codes::SUBAGENT_CONFLICT.into(),
                        message: "parent workspace modified since persistent subagent fork; merge aborted with zero writes"
                            .into(),
                    }),
                    truncated: false,
                    media_refs: Vec::new(),
                })
            }
            MergeOutcome::Applied(receipts) => {
                let artifacts = merge_artifacts(&receipts);
                if !artifacts.is_empty() {
                    self.template
                        .parent_metrics
                        .merge_child(&RunMetrics::default(), &artifacts);
                }
                if args.discard_after {
                    workspace.cleanup()?;
                    let _ = persistence.remove_job_dir(task.id);
                }
                let stored = receipts.clone();
                let discarded = args.discard_after;
                let (task, job) = persistence
                    .update_job(task.id, |job| {
                        job.merge_receipts.extend(stored.clone());
                        job.conflicts.clear();
                        job.merged = true;
                        if discarded {
                            job.discarded = true;
                        }
                    })
                    .await?;
                let snap = self.sanitize_snapshot(job.snapshot(&task));
                let data = serde_json::to_value(snap)
                    .map_err(|err| failure(error_codes::SUBAGENT_ERROR, err.to_string()))?;
                Ok(ToolResult::success(Some(data)))
            }
        }
    }

    /// Cancel and await all running isolated/mutating children when entering `PLAN`.
    pub async fn cancel_mutating(&self) -> Result<(), BoundaryError> {
        let Ok(entries) = self.list_children() else {
            return Ok(());
        };
        let mut handles = Vec::new();
        for entry in &entries {
            let mut guard = entry.lock().await;
            if guard.state == SubagentState::Running
                && (guard.isolation == SubagentIsolation::Isolated || guard.allow_exec)
            {
                if let Some(cancel) = &guard.cancel {
                    cancel.trigger_local();
                }
                if let Some(handle) = guard.handle.take() {
                    handles.push((entry.clone(), handle));
                }
            }
        }
        let mut first_error = None;
        for (entry, mut handle) in handles {
            if tokio::time::timeout(SHUTDOWN_WAIT_TIMEOUT, &mut handle)
                .await
                .is_err()
            {
                handle.abort();
                first_error.get_or_insert_with(|| {
                    failure(
                        error_codes::OUTCOME_UNKNOWN,
                        "mutating subagent did not stop before PLAN transition",
                    )
                });
            }
            let mut guard = entry.lock().await;
            if guard.state == SubagentState::Running {
                guard.state = SubagentState::Cancelled;
                guard.done_notify.notify_waiters();
            }
        }
        // Persistent mutating children run under a separate worker. Mark their
        // durable jobs cancelled in the same transition so the worker's control
        // poll stops them; their isolated workspaces are kept for inspection.
        if let Some(persistence) = self.persistence() {
            let now = timestamp()?;
            if let Ok(jobs) = persistence.list_jobs(self.owner_session()).await {
                for (task, job) in jobs {
                    if matches!(
                        task.status,
                        TaskStatus::Queued | TaskStatus::Running | TaskStatus::Paused
                    ) && (job.isolation == SubagentIsolation::Isolated || job.allow_exec)
                    {
                        match persistence
                            .store()
                            .transition(task.id, TaskStatus::Cancelled, now.clone())
                            .await
                        {
                            Ok(_) => {}
                            Err(error)
                                if error.is_code("invalid_task_state")
                                    || error.is_code(error_codes::SUBAGENT_NOT_FOUND) => {}
                            Err(error) => {
                                first_error.get_or_insert(error);
                            }
                        }
                    }
                }
            }
        }
        self.any_done.notify_waiters();
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Cancel all active children and remove every uncleaned isolated workspace.
    pub async fn shutdown(&self) -> Result<(), BoundaryError> {
        let entries = match self.list_children() {
            Ok(list) => list,
            Err(err) => return Err(err),
        };
        let mut handles = Vec::new();
        for entry in &entries {
            let mut guard = entry.lock().await;
            if let Some(cancel) = &guard.cancel {
                cancel.trigger_local();
            }
            if let Some(handle) = guard.handle.take() {
                handles.push((entry.clone(), handle));
            }
        }
        let mut first_error = None;
        for (entry, mut handle) in handles {
            if tokio::time::timeout(SHUTDOWN_WAIT_TIMEOUT, &mut handle)
                .await
                .is_err()
            {
                handle.abort();
                first_error.get_or_insert_with(|| {
                    failure(
                        error_codes::OUTCOME_UNKNOWN,
                        "subagent did not terminate before shutdown timeout",
                    )
                });
            }
            let mut guard = entry.lock().await;
            if guard.state == SubagentState::Running {
                guard.state = SubagentState::Cancelled;
                guard.done_notify.notify_waiters();
            }
        }
        for entry in entries {
            let mut guard = entry.lock().await;
            if !guard.cleaned_up
                && guard.isolation == SubagentIsolation::Isolated
                && let Err(err) = guard.workspace.cleanup()
            {
                first_error.get_or_insert(err);
            } else if guard.isolation == SubagentIsolation::Isolated {
                guard.cleaned_up = true;
            }
        }
        self.any_done.notify_waiters();
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

fn inspect_matching_parent(
    workspace: &IsolatedWorkspace,
    change: &SubagentFileChange,
) -> Option<String> {
    let parent_file = workspace.parent_workspace.join(&change.path);
    match std::fs::symlink_metadata(&parent_file) {
        Ok(meta) if meta.file_type().is_file() => {
            let bytes = std::fs::read(&parent_file).ok()?;
            let digest: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            Some(digest)
        }
        _ => None,
    }
}

fn prepare_merge_locked(
    entry: &ChildEntry,
    args: &MergeArgs,
    protected_paths: &[PathBuf],
    limits: &SubagentLimits,
) -> Result<PreparedSubagentMerge, BoundaryError> {
    if entry.state == SubagentState::Running {
        return Err(failure(
            error_codes::SUBAGENT_CONFLICT,
            "cannot merge while subagent is still running",
        ));
    }
    if matches!(
        entry.state,
        SubagentState::Merged | SubagentState::Discarded
    ) || entry.cleaned_up
    {
        return Err(failure(
            error_codes::SUBAGENT_CONFLICT,
            "subagent workspace has already been merged or discarded",
        ));
    }
    if entry.isolation == SubagentIsolation::ReadOnly {
        return Err(failure(
            error_codes::INVALID_ARGS,
            "read_only subagents have no isolated changes to merge",
        ));
    }

    let targets =
        entry
            .workspace
            .resolve_merge_targets(args.paths.as_deref(), protected_paths, limits)?;
    let diff = entry
        .workspace
        .compute_diff(&entry.id, protected_paths, false, limits)?;
    Ok(build_prepared_merge(
        &entry.id,
        &entry.label,
        entry.workspace.kind,
        targets,
        &diff,
        args,
    ))
}

fn prepare_persistent_merge(
    persistence: &SubagentPersistence,
    task: &TaskRecord,
    job: &PersistentSubagentJob,
    args: &MergeArgs,
    protected_paths: &[PathBuf],
    limits: &SubagentLimits,
) -> Result<PreparedSubagentMerge, BoundaryError> {
    if !matches!(
        task.status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    ) {
        return Err(failure(
            error_codes::SUBAGENT_CONFLICT,
            "cannot merge a persistent subagent that is not in a terminal state",
        ));
    }
    if job.discarded {
        return Err(failure(
            error_codes::SUBAGENT_CONFLICT,
            "persistent subagent workspace was discarded",
        ));
    }
    if job.isolation == SubagentIsolation::ReadOnly {
        return Err(failure(
            error_codes::INVALID_ARGS,
            "read_only subagents have no isolated changes to merge",
        ));
    }
    let id = task.id.0.to_string();
    let workspace = persistence.reopen(task.id, job)?;
    let targets =
        workspace.resolve_merge_targets(args.paths.as_deref(), protected_paths, limits)?;
    let diff = workspace.compute_diff(&id, protected_paths, false, limits)?;
    Ok(build_prepared_merge(
        &id,
        &job.label,
        job.isolation_kind,
        targets,
        &diff,
        args,
    ))
}

fn build_prepared_merge(
    id: &str,
    label: &str,
    isolation_kind: super::WorkspaceIsolationKind,
    targets: Vec<PathBuf>,
    diff: &SubagentDiffReport,
    args: &MergeArgs,
) -> PreparedSubagentMerge {
    let selected_set: Option<BTreeSet<&str>> = args
        .paths
        .as_ref()
        .map(|items| items.iter().map(String::as_str).collect());

    let mut hash = Sha256::new();
    hash.update(b"saya-subagent-merge-v1\0");
    hash.update(id.as_bytes());
    hash.update([u8::from(args.discard_after)]);
    let mut file_reviews = Vec::new();

    for change in &diff.changed_files {
        if let Some(filter) = &selected_set
            && !filter.contains(change.path.as_str())
        {
            continue;
        }
        hash.update(change.path.as_bytes());
        hash.update([0]);
        hash.update(format!("{:?}", change.kind).as_bytes());
        hash.update([0]);
        hash.update(change.base_sha256.as_deref().unwrap_or("").as_bytes());
        hash.update([0]);
        hash.update(change.new_sha256.as_deref().unwrap_or("").as_bytes());
        hash.update([0]);
        file_reviews.push(json!({
            "path": change.path,
            "kind": change.kind,
            "has_conflict": change.has_conflict,
        }));
    }

    let binding = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    PreparedSubagentMerge {
        targets,
        binding,
        review: json!({
            "subagent_id": id,
            "label": label,
            "isolation": isolation_kind,
            "discard_after": args.discard_after,
            "files": file_reviews,
        }),
    }
}

fn merge_artifacts(receipts: &[SubagentMergeReceipt]) -> Vec<Artifact> {
    receipts
        .iter()
        .filter(|receipt| receipt.operation != "unchanged")
        .map(|receipt| Artifact {
            path: PathBuf::from(&receipt.path),
            operation: receipt.operation.clone(),
        })
        .collect()
}

fn merge_usage(a: &Usage, b: &Usage) -> Usage {
    Usage {
        input_tokens: match (a.input_tokens, b.input_tokens) {
            (Some(x), Some(y)) => x.checked_add(y),
            _ => None,
        },
        output_tokens: match (a.output_tokens, b.output_tokens) {
            (Some(x), Some(y)) => x.checked_add(y),
            _ => None,
        },
        cached_input_tokens: match (a.cached_input_tokens, b.cached_input_tokens) {
            (Some(x), Some(y)) => x.checked_add(y),
            (None, None) => None,
            _ => None,
        },
        estimated: a.estimated || b.estimated,
    }
}
