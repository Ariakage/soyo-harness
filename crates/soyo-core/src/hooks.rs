//! Controlled v1 process hooks. The host supplies mandatory execution and
//! durable journal adapters. Hook output never grants the parent operation.
use crate::{
    boundary::{Cancellation, Operation},
    contract::{BoundaryError, ErrorInfo, Run},
    credentials::SecretRegistry,
    plugin_manifest::HookEntry,
    plugin_store::PluginLease,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    sync::{Arc, atomic::AtomicUsize},
    time::Duration,
};

mod engine;
mod output;
#[cfg(test)]
mod tests;

fn error(code: &str, message: &str) -> BoundaryError {
    BoundaryError(
        ErrorInfo {
            code: code.into(),
            message: message.into(),
        },
        None,
        None,
    )
}
fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn identity(text: &str) -> bool {
    !text.is_empty() && text.len() <= 256 && !text.chars().any(crate::text::is_terminal_control)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookEvent {
    #[serde(rename = "run.started")]
    RunStarted,
    #[serde(rename = "run.finished")]
    RunFinished,
    #[serde(rename = "tool.before_dispatch")]
    ToolBeforeDispatch,
    #[serde(rename = "tool.finished")]
    ToolFinished,
    #[serde(rename = "model.before_request")]
    ModelBeforeRequest,
    #[serde(rename = "model.finished")]
    ModelFinished,
    #[serde(rename = "input.required")]
    InputRequired,
    #[serde(rename = "compaction.finished")]
    CompactionFinished,
}
impl HookEvent {
    pub(crate) fn parse(text: &str) -> Result<Self, BoundaryError> {
        match text {
            "run.started" => Ok(Self::RunStarted),
            "run.finished" => Ok(Self::RunFinished),
            "tool.before_dispatch" => Ok(Self::ToolBeforeDispatch),
            "tool.finished" => Ok(Self::ToolFinished),
            "model.before_request" => Ok(Self::ModelBeforeRequest),
            "model.finished" => Ok(Self::ModelFinished),
            "input.required" => Ok(Self::InputRequired),
            "compaction.finished" => Ok(Self::CompactionFinished),
            _ => Err(error("hook_config", "Hook event is unsupported")),
        }
    }

    pub(crate) fn allows_check(self) -> bool {
        matches!(self, Self::ToolBeforeDispatch | Self::ModelBeforeRequest)
    }

    pub(crate) fn is_tool_event(self) -> bool {
        matches!(self, Self::ToolBeforeDispatch | Self::ToolFinished)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    Observer,
    Check,
}
impl HookKind {
    pub(crate) fn parse(text: &str) -> Result<Self, BoundaryError> {
        match text {
            "observer" => Ok(Self::Observer),
            "check" => Ok(Self::Check),
            _ => Err(error("hook_config", "Hook kind is unsupported")),
        }
    }
}

#[derive(Clone)]
pub struct HostHookBinding {
    pub rule: crate::config::HostHookRule,
    pub plugin_id: String,
    pub package_digest: String,
    pub lease_id: String,
    pub enable_revision: u64,
    pub session_id: crate::contract::SessionId,
    pub run_id: crate::contract::RunId,
}

#[derive(Clone)]
enum HookSource {
    Plugin(Arc<PluginLease>),
    Host(Arc<HostHookBinding>),
}

/// A pinned, enabled package or host entry. The executor rechecks revocation,
/// current policy, lease contents and permissions immediately before execution.
#[derive(Clone)]
pub struct EnabledHook {
    source: HookSource,
    entry: HookEntry,
    event: HookEvent,
    kind: HookKind,
    entry_sha256: String,
}
impl std::fmt::Debug for EnabledHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("EnabledHook([PRIVATE])")
    }
}
impl EnabledHook {
    pub fn from_lease(lease: Arc<PluginLease>, entry_id: &str) -> Result<Self, BoundaryError> {
        let entry = lease
            .package()
            .manifest()
            .hooks
            .iter()
            .find(|entry| entry.id == entry_id)
            .ok_or_else(|| error("hook_config", "Hook entry is not in the leased package"))?
            .clone();
        let event = HookEvent::parse(&entry.event)?;
        let kind = HookKind::parse(&entry.kind)?;
        if (kind == HookKind::Check && !event.allows_check())
            || entry.timeout_ms == 0
            || entry.timeout_ms > 5000
            || !lease.record().grant.capabilities.contains("process.exec")
            || !lease
                .record()
                .grant
                .capabilities
                .contains(if kind == HookKind::Check {
                    "hook.check"
                } else {
                    "hook.observe"
                })
        {
            return Err(error(
                "hook_config",
                "Hook entry is outside its enabled capability ceiling",
            ));
        }
        let entry_sha256 = digest(&output::encode(&entry, 256 * 1024)?);
        Ok(Self {
            source: HookSource::Plugin(lease),
            entry,
            event,
            kind,
            entry_sha256,
        })
    }

    pub fn from_host_rule(
        rule: &crate::config::HostHookRule,
        run: &Run,
        enable_revision: u64,
    ) -> Result<Self, BoundaryError> {
        let event = HookEvent::parse(&rule.event)?;
        let kind = HookKind::parse(&rule.kind)?;
        if (kind == HookKind::Check && !event.allows_check())
            || rule.timeout_ms == 0
            || rule.timeout_ms > 5000
            || rule.argv.is_empty()
            || enable_revision == 0
        {
            return Err(error("hook_config", "Host hook rule is invalid"));
        }
        let entry = HookEntry {
            id: rule.id.clone(),
            event: rule.event.clone(),
            kind: rule.kind.clone(),
            program: rule.argv[0].clone(),
            args: rule.argv[1..].to_vec(),
            timeout_ms: rule.timeout_ms,
        };
        let entry_sha256 = digest(&output::encode(&entry, 256 * 1024)?);
        let package_digest = digest(&output::encode(rule, 256 * 1024)?);
        let binding = Arc::new(HostHookBinding {
            rule: rule.clone(),
            plugin_id: "host.config".into(),
            package_digest,
            lease_id: format!("host:{}:{}", run.session_id.0, rule.id),
            enable_revision,
            session_id: run.session_id,
            run_id: run.id,
        });
        Ok(Self {
            source: HookSource::Host(binding),
            entry,
            event,
            kind,
            entry_sha256,
        })
    }

    pub fn plugin_lease(&self) -> Option<&PluginLease> {
        match &self.source {
            HookSource::Plugin(lease) => Some(lease),
            HookSource::Host(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn plugin_lease_arc(&self) -> Option<Arc<PluginLease>> {
        match &self.source {
            HookSource::Plugin(lease) => Some(Arc::clone(lease)),
            HookSource::Host(_) => None,
        }
    }

    pub fn host_binding(&self) -> Option<&HostHookBinding> {
        match &self.source {
            HookSource::Plugin(_) => None,
            HookSource::Host(binding) => Some(binding),
        }
    }

    pub(crate) fn plugin_id(&self) -> &str {
        match &self.source {
            HookSource::Plugin(lease) => &lease.record().plugin_id,
            HookSource::Host(binding) => &binding.plugin_id,
        }
    }

    pub(crate) fn package_digest(&self) -> &str {
        match &self.source {
            HookSource::Plugin(lease) => &lease.record().digest,
            HookSource::Host(binding) => &binding.package_digest,
        }
    }

    pub(crate) fn lease_id(&self) -> &str {
        match &self.source {
            HookSource::Plugin(lease) => &lease.record().lease_id,
            HookSource::Host(binding) => &binding.lease_id,
        }
    }

    pub(crate) fn enable_revision(&self) -> u64 {
        match &self.source {
            HookSource::Plugin(lease) => lease.record().enable_revision,
            HookSource::Host(binding) => binding.enable_revision,
        }
    }

    pub(crate) fn run_scope(&self) -> (crate::contract::SessionId, crate::contract::RunId) {
        match &self.source {
            HookSource::Plugin(lease) => (lease.record().session_id, lease.record().run_id),
            HookSource::Host(binding) => (binding.session_id, binding.run_id),
        }
    }

    pub(crate) fn matches_context(&self, context: &HookContext) -> bool {
        if self.event != context.event {
            return false;
        }
        let HookSource::Host(binding) = &self.source else {
            return true;
        };
        if !binding.rule.modes.is_empty() && !binding.rule.modes.contains(&context.run.work_mode) {
            return false;
        }
        if !binding.rule.tools.is_empty() {
            let Some(tool_name) = context.data.get("tool").and_then(Value::as_str) else {
                return false;
            };
            if !binding
                .rule
                .tools
                .iter()
                .any(|allowed| allowed == tool_name)
            {
                return false;
            }
        }
        true
    }

    pub fn entry(&self) -> &HookEntry {
        &self.entry
    }
    pub fn event(&self) -> HookEvent {
        self.event
    }
    pub fn kind(&self) -> HookKind {
        self.kind
    }
    fn key(&self) -> String {
        digest(
            format!(
                "{}\0{}\0{}",
                self.plugin_id(),
                self.package_digest(),
                self.entry.id
            )
            .as_bytes(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookAncestor {
    pub hook_key: String,
    pub invocation_id: String,
}

/// Host metadata only. Event IDs and ancestry cannot come from hook output.
#[derive(Clone)]
pub struct HookContext {
    pub event_id: String,
    pub root_event_id: String,
    pub event: HookEvent,
    pub run: Run,
    pub parent_call_id: Option<String>,
    pub depth: u8,
    pub ancestry: Vec<HookAncestor>,
    pub data: Value,
}
impl std::fmt::Debug for HookContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HookContext([PRIVATE])")
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HookLimits {
    pub max_hooks_per_event: usize,
    pub max_invocations_per_run: usize,
    pub max_depth: u8,
    pub max_payload_bytes: usize,
    pub max_output_bytes: usize,
    pub cleanup_timeout: Duration,
}
impl Default for HookLimits {
    fn default() -> Self {
        Self {
            max_hooks_per_event: 32,
            max_invocations_per_run: 256,
            max_depth: 4,
            max_payload_bytes: 32 * 1024,
            max_output_bytes: 16 * 1024,
            cleanup_timeout: Duration::from_secs(5),
        }
    }
}
impl HookLimits {
    fn validate(&self) -> Result<(), BoundaryError> {
        if self.max_hooks_per_event == 0
            || self.max_hooks_per_event > 128
            || self.max_invocations_per_run == 0
            || self.max_invocations_per_run > 4096
            || self.max_depth == 0
            || self.max_depth > 8
            || self.max_payload_bytes == 0
            || self.max_payload_bytes > 256 * 1024
            || self.max_output_bytes == 0
            || self.max_output_bytes > 64 * 1024
            || self.cleanup_timeout.is_zero()
            || self.cleanup_timeout > Duration::from_secs(30)
        {
            return Err(error("hook_config", "Hook budgets are invalid"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookIntent {
    pub invocation_id: String,
    pub fingerprint: String,
    pub event_id: String,
    pub root_event_id: String,
    pub event: HookEvent,
    pub kind: HookKind,
    pub run: Run,
    pub parent_call_id: Option<String>,
    pub depth: u8,
    pub plugin_id: String,
    pub package_digest: String,
    pub entry_id: String,
    pub entry_sha256: String,
    pub lease_id: String,
    pub enable_revision: u64,
    pub payload_sha256: String,
    pub timeout_ms: u32,
    pub output_max_bytes: usize,
    pub run_invocation_limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupState {
    Cleaned,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookExecutionState {
    NotStarted,
    Started,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookStatus {
    Succeeded,
    Denied,
    Failed,
    Cancelled,
    TimedOut,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookDecision {
    Pass,
    Deny,
    Abstain,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationLevel {
    Info,
    Warning,
    Error,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookAnnotation {
    pub level: AnnotationLevel,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookRecord {
    pub intent: HookIntent,
    pub status: HookStatus,
    pub execution: HookExecutionState,
    pub cleanup: CleanupState,
    pub decision: Option<HookDecision>,
    pub annotations: Vec<HookAnnotation>,
    pub error: Option<ErrorInfo>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDiagnostic {
    pub invocation_id: String,
    pub event_id: String,
    pub code: String,
    pub message: String,
}

pub enum HookClaim {
    Execute,
    Completed(Box<HookRecord>),
    Running,
    Unknown,
    Conflict,
    BudgetExhausted,
}

/// Atomic claim and result transitions. Adapters must compare fingerprints,
/// treat pre-existing Running/Unknown as non-replayable, and validate current
/// secrets again at their durable write boundary. A fresh claim must enforce
/// run_invocation_limit atomically across engines and restarts, returning
/// BudgetExhausted without recording a running operation. No default exists.
pub trait HookJournal: Send + Sync {
    fn claim<'a>(&'a self, intent: &'a HookIntent) -> Operation<'a, HookClaim>;
    fn finish<'a>(&'a self, record: &'a HookRecord) -> Operation<'a, ()>;
    fn diagnostic<'a>(&'a self, diagnostic: &'a HookDiagnostic) -> Operation<'a, ()>;
}

pub struct HookExecutionRequest {
    pub intent: HookIntent,
    pub hook: EnabledHook,
    pub payload: Value,
    pub next_depth: u8,
    pub ancestry: Vec<HookAncestor>,
}
pub enum HookBegin {
    Started(Box<dyn HookExecution>),
    NotStarted {
        denied: bool,
    },
    /// The host retains resource ownership until independently reconciled.
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}
pub struct HookProcessOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub truncated: bool,
}

/// A wait future can be dropped while this handle continues to own the process.
/// stop must await actual cleanup and retain ownership on Unknown, including
/// when its future is dropped at the cleanup deadline.
pub trait HookExecution: Send {
    fn wait<'a>(&'a mut self) -> Operation<'a, HookProcessOutput>;
    fn stop<'a>(&'a mut self, reason: StopReason) -> Operation<'a, CleanupState>;
}
/// begin performs current policy/Manual Approval/PLAN/native-exec checks. It is
/// cancellable and returns only after disclosing ownership or definite no-start.
/// Human waiting pauses the host's shared budget. The execution timeout starts
/// after Started returns. Errors are conservatively treated as Unknown.
pub trait HookHostExecutor: Send + Sync {
    fn begin<'a>(
        &'a self,
        request: &'a HookExecutionRequest,
        cancel: &'a dyn Cancellation,
    ) -> Operation<'a, HookBegin>;
}

#[derive(Debug, Default)]
pub struct HookBatchOutcome {
    pub records: Vec<HookRecord>,
    pub diagnostics: Vec<HookDiagnostic>,
    pub blocked: bool,
    pub cancelled: bool,
    pub cleanup_pending: bool,
}

pub struct HookEngine {
    run: Run,
    limits: HookLimits,
    secrets: Arc<SecretRegistry>,
    executor: Arc<dyn HookHostExecutor>,
    journal: Arc<dyn HookJournal>,
    invocations: AtomicUsize,
}
impl HookEngine {
    pub fn new(
        run: Run,
        limits: HookLimits,
        secrets: Arc<SecretRegistry>,
        executor: Arc<dyn HookHostExecutor>,
        journal: Arc<dyn HookJournal>,
    ) -> Result<Self, BoundaryError> {
        limits.validate()?;
        Ok(Self {
            run,
            limits,
            secrets,
            executor,
            journal,
            invocations: AtomicUsize::new(0),
        })
    }
    /// Results are durable before return. `blocked == false` only clears the
    /// hook veto. The caller still authorizes the parent operation independently.
    pub async fn fire(
        &self,
        context: &HookContext,
        hooks: &[EnabledHook],
        cancel: &dyn Cancellation,
    ) -> Result<HookBatchOutcome, BoundaryError> {
        self.fire_inner(context, hooks, cancel)
            .await
            .map_err(|failure| output::safe_error(failure, &self.secrets))
    }
}
