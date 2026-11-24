//! Controlled parent-child subagent collaboration (`F-09`).
//!
//! Subagents are strictly opt-in (`[subagents] enabled = true`) and execute
//! with non-escalating mode, permission, sandbox, secret, and budget
//! bounds. Code modifications in `isolated` mode target a private Git
//! worktree or bounded directory copy outside the parent workspace and can
//! only enter the parent through conflict-checked `subagent_merge`.

pub mod supervisor;
pub mod workspace;

#[cfg(test)]
mod tests;

use crate::{
    boundary::ToolDefinition,
    contract::{BoundaryError, ErrorInfo, Usage, WorkMode, error_codes},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Component, PathBuf},
    time::Duration,
};

pub const DEFAULT_MAX_DEPTH: usize = 1;
pub const MAX_ALLOWED_DEPTH: usize = 3;
pub const DEFAULT_MAX_CONCURRENT: usize = 4;
pub const MAX_ALLOWED_CONCURRENT: usize = 16;
pub const DEFAULT_MAX_TOTAL: usize = 16;
pub const MAX_ALLOWED_TOTAL: usize = 64;
pub const DEFAULT_MAX_MODEL_REQUESTS: usize = 32;
pub const MAX_ALLOWED_MODEL_REQUESTS: usize = 256;
pub const DEFAULT_MAX_TOOL_CALLS: usize = 128;
pub const MAX_ALLOWED_TOOL_CALLS: usize = 1024;
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 300;
pub const DEFAULT_MAX_TIMEOUT_SECONDS: u64 = 900;
pub const MAX_ALLOWED_TIMEOUT_SECONDS: u64 = 1800;
pub const DEFAULT_MAX_WORKSPACE_FILES: usize = 10_000;
pub const MAX_ALLOWED_WORKSPACE_FILES: usize = 50_000;
pub const DEFAULT_MAX_WORKSPACE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_ALLOWED_WORKSPACE_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_MAX_CHANGED_FILES: usize = 256;
pub const MAX_ALLOWED_CHANGED_FILES: usize = 2048;
pub const DEFAULT_MAX_PATCH_BYTES: usize = 256 * 1024;
pub const MAX_ALLOWED_PATCH_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_MAX_SUMMARY_BYTES: usize = 32 * 1024;
pub const MAX_ALLOWED_SUMMARY_BYTES: usize = 256 * 1024;

const MAX_TASK_BYTES: usize = 32 * 1024;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_LABEL_BYTES: usize = 64;
const MAX_ALLOWED_PATHS: usize = 64;

pub const TOOL_NAMES: [&str; 7] = [
    "subagent_spawn",
    "subagent_status",
    "subagent_wait",
    "subagent_message",
    "subagent_cancel",
    "subagent_diff",
    "subagent_merge",
];

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

/// Host configuration section `[subagents]`; strictly disabled unless `enabled = true`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubagentSettings {
    pub enabled: bool,
    pub max_depth: Option<usize>,
    pub max_concurrent: Option<usize>,
    pub max_total: Option<usize>,
    pub max_model_requests: Option<usize>,
    pub max_tool_calls: Option<usize>,
    pub default_timeout_seconds: Option<u64>,
    pub max_timeout_seconds: Option<u64>,
    pub max_workspace_files: Option<usize>,
    pub max_workspace_bytes: Option<u64>,
    pub max_changed_files: Option<usize>,
    pub max_patch_bytes: Option<usize>,
    pub max_summary_bytes: Option<usize>,
}

/// Validated runtime limits for one enabled subagent supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentLimits {
    /// Maximum delegation-chain depth (`1` disables grandchildren entirely).
    pub max_depth: usize,
    pub max_concurrent: usize,
    pub max_total: usize,
    pub max_model_requests: usize,
    pub max_tool_calls: usize,
    pub default_timeout: Duration,
    pub max_timeout: Duration,
    pub max_workspace_files: usize,
    pub max_workspace_bytes: u64,
    pub max_changed_files: usize,
    pub max_patch_bytes: usize,
    pub max_summary_bytes: usize,
}

impl Default for SubagentLimits {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            max_total: DEFAULT_MAX_TOTAL,
            max_model_requests: DEFAULT_MAX_MODEL_REQUESTS,
            max_tool_calls: DEFAULT_MAX_TOOL_CALLS,
            default_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECONDS),
            max_timeout: Duration::from_secs(DEFAULT_MAX_TIMEOUT_SECONDS),
            max_workspace_files: DEFAULT_MAX_WORKSPACE_FILES,
            max_workspace_bytes: DEFAULT_MAX_WORKSPACE_BYTES,
            max_changed_files: DEFAULT_MAX_CHANGED_FILES,
            max_patch_bytes: DEFAULT_MAX_PATCH_BYTES,
            max_summary_bytes: DEFAULT_MAX_SUMMARY_BYTES,
        }
    }
}

impl SubagentLimits {
    /// Reject zero, inverted, or over-range limits.
    pub fn validate(&self) -> Result<(), BoundaryError> {
        if self.max_depth == 0
            || self.max_depth > MAX_ALLOWED_DEPTH
            || self.max_concurrent == 0
            || self.max_concurrent > MAX_ALLOWED_CONCURRENT
            || self.max_total == 0
            || self.max_total > MAX_ALLOWED_TOTAL
            || self.max_concurrent > self.max_total
            || self.max_model_requests == 0
            || self.max_model_requests > MAX_ALLOWED_MODEL_REQUESTS
            || self.max_tool_calls == 0
            || self.max_tool_calls > MAX_ALLOWED_TOOL_CALLS
            || self.default_timeout.is_zero()
            || self.max_timeout.is_zero()
            || self.default_timeout > self.max_timeout
            || self.max_timeout > Duration::from_secs(MAX_ALLOWED_TIMEOUT_SECONDS)
            || self.max_workspace_files == 0
            || self.max_workspace_files > MAX_ALLOWED_WORKSPACE_FILES
            || self.max_workspace_bytes < 1024
            || self.max_workspace_bytes > MAX_ALLOWED_WORKSPACE_BYTES
            || self.max_changed_files == 0
            || self.max_changed_files > MAX_ALLOWED_CHANGED_FILES
            || self.max_patch_bytes < 1024
            || self.max_patch_bytes > MAX_ALLOWED_PATCH_BYTES
            || self.max_summary_bytes < 512
            || self.max_summary_bytes > MAX_ALLOWED_SUMMARY_BYTES
        {
            return Err(failure(
                error_codes::INVALID_CONFIG,
                "subagent limits are outside their supported range",
            ));
        }
        Ok(())
    }
}

/// Resolved configuration passed to `AgentRuntime::with_subagents`.
///
/// `depth` counts delegation hops from the top-level run (`0` for the session
/// runtime, `1` for its direct children, ...). A runtime whose depth is below
/// `limits.max_depth` builds a supervisor so its children can delegate
/// further; a runtime at `depth == max_depth` gets none, so it sees no
/// `subagent_*` tools and any fabricated spawn fails closed as `unknown_tool`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentConfig {
    pub limits: SubagentLimits,
    depth: usize,
}

impl SubagentConfig {
    /// Top-level resolved configuration (the session runtime sits at depth 0).
    pub fn new(limits: SubagentLimits) -> Self {
        Self { limits, depth: 0 }
    }

    /// Configuration for the supervisor of a runtime one hop deeper. Returns
    /// `None` when the child would sit at `max_depth` and therefore must not
    /// delegate further.
    pub fn for_child(&self) -> Option<Self> {
        let child_depth = self.depth + 1;
        (child_depth < self.limits.max_depth).then(|| Self {
            limits: self.limits.clone(),
            depth: child_depth,
        })
    }

    /// Delegation depth (hops from the top-level run) of the runtime that owns
    /// this supervisor.
    pub fn depth(&self) -> usize {
        self.depth
    }
}

impl SubagentSettings {
    /// Validate explicit bounds even when `enabled = false`.
    pub fn validate(&self) -> Result<(), BoundaryError> {
        self.build_limits()?.validate()
    }

    /// Resolve effective configuration (`Ok(None)` when disabled).
    pub fn resolve(&self) -> Result<Option<SubagentConfig>, BoundaryError> {
        let limits = self.build_limits()?;
        limits.validate()?;
        if !self.enabled {
            return Ok(None);
        }
        Ok(Some(SubagentConfig::new(limits)))
    }

    fn build_limits(&self) -> Result<SubagentLimits, BoundaryError> {
        let defaults = SubagentLimits::default();
        Ok(SubagentLimits {
            max_depth: self.max_depth.unwrap_or(defaults.max_depth),
            max_concurrent: self.max_concurrent.unwrap_or(defaults.max_concurrent),
            max_total: self.max_total.unwrap_or(defaults.max_total),
            max_model_requests: self
                .max_model_requests
                .unwrap_or(defaults.max_model_requests),
            max_tool_calls: self.max_tool_calls.unwrap_or(defaults.max_tool_calls),
            default_timeout: Duration::from_secs(
                self.default_timeout_seconds
                    .unwrap_or(defaults.default_timeout.as_secs()),
            ),
            max_timeout: Duration::from_secs(
                self.max_timeout_seconds
                    .unwrap_or(defaults.max_timeout.as_secs()),
            ),
            max_workspace_files: self
                .max_workspace_files
                .unwrap_or(defaults.max_workspace_files),
            max_workspace_bytes: self
                .max_workspace_bytes
                .unwrap_or(defaults.max_workspace_bytes),
            max_changed_files: self.max_changed_files.unwrap_or(defaults.max_changed_files),
            max_patch_bytes: self.max_patch_bytes.unwrap_or(defaults.max_patch_bytes),
            max_summary_bytes: self.max_summary_bytes.unwrap_or(defaults.max_summary_bytes),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentIsolation {
    ReadOnly,
    Isolated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceIsolationKind {
    ReadOnly,
    GitWorktree,
    DirectoryCopy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    Running,
    Completed,
    Failed,
    Cancelled,
    LimitReached,
    NeedsConfirmation,
    NeedsInput,
    Merged,
    Discarded,
}

impl SubagentState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::LimitReached => "limit_reached",
            Self::NeedsConfirmation => "needs_confirmation",
            Self::NeedsInput => "needs_input",
            Self::Merged => "merged",
            Self::Discarded => "discarded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentStepTrace {
    pub call_id: String,
    pub tool: String,
    pub detail: String,
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentFileChange {
    pub path: String,
    pub kind: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_sha256: Option<String>,
    pub bytes: u64,
    pub has_conflict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentConflict {
    pub path: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentMergeReceipt {
    pub path: String,
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_parent_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_parent_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentDiffReport {
    pub subagent_id: String,
    pub isolation_kind: WorkspaceIsolationKind,
    pub changed_files: Vec<SubagentFileChange>,
    pub patch: String,
    pub patch_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSnapshot {
    pub id: String,
    pub label: String,
    pub state: SubagentState,
    pub work_mode: WorkMode,
    pub isolation: SubagentIsolation,
    pub isolation_kind: WorkspaceIsolationKind,
    pub allow_exec: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub execution_requests: u64,
    pub tool_calls: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub step_traces: Vec<SubagentStepTrace>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<SubagentFileChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merge_receipts: Vec<SubagentMergeReceipt>,
}

#[derive(Debug, Clone)]
pub struct SpawnArgs {
    pub label: String,
    pub task: String,
    pub mode: Option<WorkMode>,
    pub isolation: Option<SubagentIsolation>,
    pub allow_exec: bool,
    pub allowed_paths: Vec<PathBuf>,
    pub background: bool,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct StatusArgs {
    pub subagent_id: String,
}

#[derive(Debug, Clone)]
pub struct WaitArgs {
    pub subagent_ids: Vec<String>,
    pub timeout: Option<Duration>,
    pub wait_all: bool,
}

#[derive(Debug, Clone)]
pub struct MessageArgs {
    pub subagent_id: String,
    pub message: String,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct CancelArgs {
    pub subagent_id: String,
    pub discard: bool,
}

#[derive(Debug, Clone)]
pub struct DiffArgs {
    pub subagent_id: String,
    pub include_patch: bool,
}

#[derive(Debug, Clone)]
pub struct MergeArgs {
    pub subagent_id: String,
    pub paths: Option<Vec<String>>,
    pub discard_after: bool,
}

fn parse_json_object(raw: &str) -> Result<serde_json::Map<String, Value>, BoundaryError> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| failure(error_codes::INVALID_ARGS, "subagent arguments must be JSON"))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(failure(
            error_codes::INVALID_ARGS,
            "subagent arguments must be an object",
        )),
    }
}

fn reject_unknown_keys(
    map: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), BoundaryError> {
    if let Some(unexpected) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(failure(
            error_codes::INVALID_ARGS,
            format!("unexpected argument field `{unexpected}`"),
        ));
    }
    Ok(())
}

fn required_string(
    map: &serde_json::Map<String, Value>,
    key: &str,
    max_bytes: usize,
) -> Result<String, BoundaryError> {
    let value = map
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| failure(error_codes::INVALID_ARGS, format!("`{key}` is required")))?;
    if value.trim().is_empty() || value.len() > max_bytes {
        return Err(failure(
            error_codes::INVALID_ARGS,
            format!("`{key}` must be non-empty and within {max_bytes} bytes"),
        ));
    }
    Ok(value.to_owned())
}

fn validate_subagent_id(id: &str) -> Result<String, BoundaryError> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(failure(
            error_codes::INVALID_ARGS,
            "subagent_id must be a bounded ASCII identifier",
        ));
    }
    Ok(id.to_owned())
}

fn validate_label(label: &str) -> Result<String, BoundaryError> {
    if label.trim().is_empty()
        || label.len() > MAX_LABEL_BYTES
        || label.chars().any(crate::text::is_terminal_control)
    {
        return Err(failure(
            error_codes::INVALID_ARGS,
            "label must be non-empty, <= 64 bytes, and free of control characters",
        ));
    }
    Ok(label.trim().to_owned())
}

pub fn validate_relative_workspace_path(raw: &str) -> Result<PathBuf, BoundaryError> {
    if raw.is_empty()
        || raw.len() > 1024
        || raw.contains('\\')
        || raw.contains('\0')
        || raw.chars().any(crate::text::is_terminal_control)
    {
        return Err(failure(
            error_codes::INVALID_PATH,
            "path must be a clean workspace-relative path",
        ));
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        return Err(failure(
            error_codes::INVALID_PATH,
            "path must be relative to the workspace",
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                if part == ".git" {
                    return Err(failure(
                        error_codes::PATH_DENIED,
                        "repository control paths cannot be targeted",
                    ));
                }
                normalized.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(failure(
                    error_codes::INVALID_PATH,
                    "path must not traverse parent directories",
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(failure(
            error_codes::INVALID_PATH,
            "path must reference a file or subdirectory",
        ));
    }
    Ok(normalized)
}

fn parse_timeout(
    map: &serde_json::Map<String, Value>,
    limits: &SubagentLimits,
) -> Result<Duration, BoundaryError> {
    match map.get("timeout_seconds") {
        None => Ok(limits.default_timeout),
        Some(value) => {
            let seconds = value.as_u64().ok_or_else(|| {
                failure(
                    error_codes::INVALID_ARGS,
                    "timeout_seconds must be a positive integer",
                )
            })?;
            let duration = Duration::from_secs(seconds);
            if duration.is_zero() || duration > limits.max_timeout {
                return Err(failure(
                    error_codes::INVALID_ARGS,
                    "timeout_seconds is outside configured limits",
                ));
            }
            Ok(duration)
        }
    }
}

pub fn parse_spawn(raw: &str, limits: &SubagentLimits) -> Result<SpawnArgs, BoundaryError> {
    let map = parse_json_object(raw)?;
    reject_unknown_keys(
        &map,
        &[
            "label",
            "task",
            "mode",
            "workspace_isolation",
            "allow_exec",
            "allowed_paths",
            "background",
            "timeout_seconds",
        ],
    )?;
    let label = validate_label(&required_string(&map, "label", MAX_LABEL_BYTES)?)?;
    let task = required_string(&map, "task", MAX_TASK_BYTES)?;
    let mode = match map.get("mode") {
        None => None,
        Some(value) => {
            let raw_mode = value
                .as_str()
                .ok_or_else(|| failure(error_codes::INVALID_ARGS, "mode must be a string"))?;
            Some(match raw_mode {
                "plan" => WorkMode::Plan,
                "code" => WorkMode::Code,
                "build" => WorkMode::Build,
                "work" => WorkMode::Work,
                _ => {
                    return Err(failure(
                        error_codes::INVALID_ARGS,
                        "mode must be one of plan, code, build, work",
                    ));
                }
            })
        }
    };
    let isolation = match map.get("workspace_isolation") {
        None => None,
        Some(value) => {
            let raw_iso = value.as_str().ok_or_else(|| {
                failure(
                    error_codes::INVALID_ARGS,
                    "workspace_isolation must be a string",
                )
            })?;
            Some(match raw_iso {
                "read_only" => SubagentIsolation::ReadOnly,
                "isolated" => SubagentIsolation::Isolated,
                _ => {
                    return Err(failure(
                        error_codes::INVALID_ARGS,
                        "workspace_isolation must be read_only or isolated",
                    ));
                }
            })
        }
    };
    let allow_exec = match map.get("allow_exec") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| failure(error_codes::INVALID_ARGS, "allow_exec must be a boolean"))?,
    };
    let allowed_paths = match map.get("allowed_paths") {
        None => Vec::new(),
        Some(value) => {
            let array = value.as_array().ok_or_else(|| {
                failure(error_codes::INVALID_ARGS, "allowed_paths must be an array")
            })?;
            if array.len() > MAX_ALLOWED_PATHS {
                return Err(failure(
                    error_codes::INVALID_ARGS,
                    "allowed_paths exceeds maximum entry count",
                ));
            }
            let mut out = Vec::with_capacity(array.len());
            for item in array {
                let raw_path = item.as_str().ok_or_else(|| {
                    failure(
                        error_codes::INVALID_ARGS,
                        "allowed_paths entries must be strings",
                    )
                })?;
                let normalized = validate_relative_workspace_path(raw_path)?;
                if !out.contains(&normalized) {
                    out.push(normalized);
                }
            }
            out
        }
    };
    let background = match map.get("background") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| failure(error_codes::INVALID_ARGS, "background must be a boolean"))?,
    };
    let timeout = parse_timeout(&map, limits)?;
    Ok(SpawnArgs {
        label,
        task,
        mode,
        isolation,
        allow_exec,
        allowed_paths,
        background,
        timeout,
    })
}

pub fn parse_status(raw: &str) -> Result<StatusArgs, BoundaryError> {
    let map = parse_json_object(raw)?;
    reject_unknown_keys(&map, &["subagent_id"])?;
    let subagent_id = validate_subagent_id(&required_string(&map, "subagent_id", 64)?)?;
    Ok(StatusArgs { subagent_id })
}

pub fn parse_wait(raw: &str, limits: &SubagentLimits) -> Result<WaitArgs, BoundaryError> {
    let map = parse_json_object(raw)?;
    reject_unknown_keys(&map, &["subagent_ids", "timeout_seconds", "wait_all"])?;
    let array = map
        .get("subagent_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| failure(error_codes::INVALID_ARGS, "subagent_ids array is required"))?;
    if array.is_empty() || array.len() > MAX_ALLOWED_CONCURRENT {
        return Err(failure(
            error_codes::INVALID_ARGS,
            "subagent_ids must contain 1..=16 entries",
        ));
    }
    let mut seen = BTreeSet::new();
    let mut subagent_ids = Vec::with_capacity(array.len());
    for item in array {
        let raw_id = item
            .as_str()
            .ok_or_else(|| failure(error_codes::INVALID_ARGS, "subagent_ids must be strings"))?;
        let id = validate_subagent_id(raw_id)?;
        if seen.insert(id.clone()) {
            subagent_ids.push(id);
        }
    }
    let timeout = if map.contains_key("timeout_seconds") {
        Some(parse_timeout(&map, limits)?)
    } else {
        None
    };
    let wait_all = match map.get("wait_all") {
        None => true,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| failure(error_codes::INVALID_ARGS, "wait_all must be a boolean"))?,
    };
    Ok(WaitArgs {
        subagent_ids,
        timeout,
        wait_all,
    })
}

pub fn parse_message(raw: &str, limits: &SubagentLimits) -> Result<MessageArgs, BoundaryError> {
    let map = parse_json_object(raw)?;
    reject_unknown_keys(&map, &["subagent_id", "message", "timeout_seconds"])?;
    let subagent_id = validate_subagent_id(&required_string(&map, "subagent_id", 64)?)?;
    let message = required_string(&map, "message", MAX_MESSAGE_BYTES)?;
    let timeout = parse_timeout(&map, limits)?;
    Ok(MessageArgs {
        subagent_id,
        message,
        timeout,
    })
}

pub fn parse_cancel(raw: &str) -> Result<CancelArgs, BoundaryError> {
    let map = parse_json_object(raw)?;
    reject_unknown_keys(&map, &["subagent_id", "discard"])?;
    let subagent_id = validate_subagent_id(&required_string(&map, "subagent_id", 64)?)?;
    let discard = match map.get("discard") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| failure(error_codes::INVALID_ARGS, "discard must be a boolean"))?,
    };
    Ok(CancelArgs {
        subagent_id,
        discard,
    })
}

pub fn parse_diff(raw: &str) -> Result<DiffArgs, BoundaryError> {
    let map = parse_json_object(raw)?;
    reject_unknown_keys(&map, &["subagent_id", "include_patch"])?;
    let subagent_id = validate_subagent_id(&required_string(&map, "subagent_id", 64)?)?;
    let include_patch = match map.get("include_patch") {
        None => true,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| failure(error_codes::INVALID_ARGS, "include_patch must be a boolean"))?,
    };
    Ok(DiffArgs {
        subagent_id,
        include_patch,
    })
}

pub fn parse_merge(raw: &str) -> Result<MergeArgs, BoundaryError> {
    let map = parse_json_object(raw)?;
    reject_unknown_keys(&map, &["subagent_id", "paths", "discard_after"])?;
    let subagent_id = validate_subagent_id(&required_string(&map, "subagent_id", 64)?)?;
    let paths = match map.get("paths") {
        None => None,
        Some(value) => {
            let array = value
                .as_array()
                .ok_or_else(|| failure(error_codes::INVALID_ARGS, "paths must be an array"))?;
            if array.is_empty() || array.len() > MAX_ALLOWED_CHANGED_FILES {
                return Err(failure(
                    error_codes::INVALID_ARGS,
                    "paths array must be non-empty and within change limits",
                ));
            }
            let mut out = Vec::with_capacity(array.len());
            let mut seen = BTreeSet::new();
            for item in array {
                let raw_path = item.as_str().ok_or_else(|| {
                    failure(error_codes::INVALID_ARGS, "paths entries must be strings")
                })?;
                let normalized = validate_relative_workspace_path(raw_path)?;
                let key = normalized.to_string_lossy().into_owned();
                if seen.insert(key.clone()) {
                    out.push(key);
                }
            }
            Some(out)
        }
    };
    let discard_after = match map.get("discard_after") {
        None => true,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| failure(error_codes::INVALID_ARGS, "discard_after must be a boolean"))?,
    };
    Ok(MergeArgs {
        subagent_id,
        paths,
        discard_after,
    })
}

/// Build the model-facing definitions for `F-09` subagent tools.
/// In `Plan` or read-only runtimes, `subagent_merge` is omitted and
/// `subagent_spawn` constrains mode/isolation/exec to read-only inspection.
pub fn definitions(mode: WorkMode, read_only: bool) -> Vec<ToolDefinition> {
    let plan_only = mode == WorkMode::Plan || read_only;
    let spawn_mode_enum = if plan_only {
        json!(["plan"])
    } else {
        json!(["plan", "code", "build", "work"])
    };
    let isolation_enum = if plan_only {
        json!(["read_only"])
    } else {
        json!(["read_only", "isolated"])
    };
    let exec_schema = if plan_only {
        json!({
            "type": "boolean",
            "enum": [false],
            "description": "Command execution is disabled in PLAN/read-only mode."
        })
    } else {
        json!({
            "type": "boolean",
            "description": "Allow exec inside this subagent (requires parent authorization; defaults to false)."
        })
    };

    let spawn_examples = if plan_only {
        vec![json!({
            "label": "audit-restore",
            "task": "Read crates/soyo-app/src/session.rs and list all session resume validation checks.",
            "mode": "plan",
            "workspace_isolation": "read_only",
            "background": false
        })]
    } else {
        vec![
            json!({
                "label": "inspect-auth",
                "task": "Audit crates/soyo-core/src/approvals.rs for permit consumption edge cases.",
                "mode": "plan",
                "workspace_isolation": "read_only",
                "background": true
            }),
            json!({
                "label": "patch-parser",
                "task": "Implement and verify UTF-8 line splitting inside src/parser.rs.",
                "mode": "code",
                "workspace_isolation": "isolated",
                "allowed_paths": ["src/parser.rs"],
                "background": true
            }),
        ]
    };

    let mut defs = vec![
        ToolDefinition {
            name: "subagent_spawn".into(),
            description: "Spawn a bounded child subagent with an isolated context, shared parent budgets, and non-escalating permissions. Use workspace_isolation=\"isolated\" for parallel code edits inside a private worktree/copy, or \"read_only\" for parallel exploration.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "label": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_LABEL_BYTES,
                        "description": "Short human-readable identifier for this subagent."
                    },
                    "task": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_TASK_BYTES,
                        "description": "Self-contained objective, scope, and verification expectation for the child."
                    },
                    "mode": {
                        "type": "string",
                        "enum": spawn_mode_enum,
                        "description": "Child work mode (cannot exceed parent mode)."
                    },
                    "workspace_isolation": {
                        "type": "string",
                        "enum": isolation_enum,
                        "description": "read_only inspects without writing; isolated works in a separate worktree or workspace copy."
                    },
                    "allow_exec": exec_schema,
                    "allowed_paths": {
                        "type": "array",
                        "maxItems": MAX_ALLOWED_PATHS,
                        "items": {"type": "string"},
                        "description": "Optional workspace-relative files or directories constraining the child."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Return immediately with a running subagent_id for parallel execution; wait later with subagent_wait."
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_ALLOWED_TIMEOUT_SECONDS,
                        "description": "Optional wall-clock timeout for this subagent."
                    }
                },
                "required": ["label", "task"],
                "additionalProperties": false
            }),
            examples: spawn_examples,
        },
        ToolDefinition {
            name: "subagent_status".into(),
            description: "Inspect a subagent's current status, step trace, usage, and changed files.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subagent_id": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 64
                    }
                },
                "required": ["subagent_id"],
                "additionalProperties": false
            }),
            examples: vec![json!({"subagent_id": "sub_01"})],
        },
        ToolDefinition {
            name: "subagent_wait".into(),
            description: "Wait for one or more background subagents to reach a terminal state.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subagent_ids": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_ALLOWED_CONCURRENT,
                        "items": {"type": "string", "minLength": 1, "maxLength": 64}
                    },
                    "wait_all": {
                        "type": "boolean",
                        "description": "When true (default), waits for all listed subagents; when false, returns as soon as any finishes."
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_ALLOWED_TIMEOUT_SECONDS
                    }
                },
                "required": ["subagent_ids"],
                "additionalProperties": false
            }),
            examples: vec![json!({"subagent_ids": ["sub_01", "sub_02"], "wait_all": true})],
        },
        ToolDefinition {
            name: "subagent_message".into(),
            description: "Send steering or a bounded follow-up instruction to an active or unmerged subagent.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subagent_id": {"type": "string", "minLength": 1, "maxLength": 64},
                    "message": {"type": "string", "minLength": 1, "maxLength": MAX_MESSAGE_BYTES},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": MAX_ALLOWED_TIMEOUT_SECONDS}
                },
                "required": ["subagent_id", "message"],
                "additionalProperties": false
            }),
            examples: vec![json!({
                "subagent_id": "sub_01",
                "message": "Also check error handling for empty inputs."
            })],
        },
        ToolDefinition {
            name: "subagent_cancel".into(),
            description: "Cancel a running subagent and optionally discard its isolated workspace.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subagent_id": {"type": "string", "minLength": 1, "maxLength": 64},
                    "discard": {
                        "type": "boolean",
                        "description": "When true, immediately remove any isolated workspace after stopping."
                    }
                },
                "required": ["subagent_id"],
                "additionalProperties": false
            }),
            examples: vec![json!({"subagent_id": "sub_01", "discard": true})],
        },
        ToolDefinition {
            name: "subagent_diff".into(),
            description: "Inspect changed files, parent conflict flags, and bounded unified diff from an isolated subagent.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subagent_id": {"type": "string", "minLength": 1, "maxLength": 64},
                    "include_patch": {"type": "boolean"}
                },
                "required": ["subagent_id"],
                "additionalProperties": false
            }),
            examples: vec![json!({"subagent_id": "sub_01", "include_patch": true})],
        },
    ];

    if !plan_only {
        defs.push(ToolDefinition {
            name: "subagent_merge".into(),
            description: "Conflict-check and atomically apply changed files from a completed isolated subagent into the parent workspace. Fails closed with conflict details if the parent modified a target file since fork.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subagent_id": {"type": "string", "minLength": 1, "maxLength": 64},
                    "paths": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_ALLOWED_CHANGED_FILES,
                        "items": {"type": "string"},
                        "description": "Optional subset of workspace-relative changed paths to merge."
                    },
                    "discard_after": {
                        "type": "boolean",
                        "description": "Clean up the isolated workspace after merging (defaults to true)."
                    }
                },
                "required": ["subagent_id"],
                "additionalProperties": false
            }),
            examples: vec![json!({
                "subagent_id": "sub_01",
                "paths": ["src/parser.rs"],
                "discard_after": true
            })],
        });
    }

    defs
}
