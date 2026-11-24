//! MCP SEP-2663 long-running tasks (`io.modelcontextprotocol/tasks`, `F-02`).
//!
//! Tasks are opt-in: the client only advertises the extension and accepts
//! task-shaped responses when `[mcp.tasks] enabled = true`. A server that
//! returns `CreateTaskResult` without the opt-in stays on the existing
//! fail-closed `outcome_unknown` path.

use super::{McpClient, bounded, error, service_error};
use crate::boundary::Cancellation;
use crate::contract::BoundaryError;
use rmcp::model::{CancelTaskParams, GetTaskParams, TaskStatus, UpdateTaskParams};
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_TASK_ID_BYTES: usize = 256;
const MAX_STATUS_MESSAGE_BYTES: usize = 2048;
const MAX_INPUT_REQUESTS: usize = 64;

/// Host-facing projection of one server task snapshot (`tasks/get` result).
///
/// `status` mirrors SEP-2663 terminal/working states; `run_state` maps them
/// onto the host's Run semantics so callers never interpret wire values.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTaskInfo {
    pub task_id: String,
    /// One of `working` | `input_required` | `completed` | `failed` | `cancelled`.
    pub status: String,
    /// Host projection: `running` | `needs_input` | `done` | `failed` |
    /// `cancelled` | `unknown`. Terminal-but-unparsable states fold into
    /// `unknown` so nothing claims an outcome the server did not confirm.
    pub run_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    /// Outstanding in-task server→client requests keyed by their server id.
    /// Only present when `status == "input_required"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_requests: Vec<McpTaskInputRequest>,
    /// Terminal `result` object verbatim from `tasks/get` (`completed` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Terminal `error` object verbatim from `tasks/get` (`failed` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    /// Suggested server poll interval in milliseconds, if advertised.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_interval_ms: Option<u64>,
    /// Server-declared TTL in milliseconds (`None` = unlimited), if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
}

/// One outstanding server→client request surfaced inside an input-required task.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTaskInputRequest {
    pub key: String,
    /// Method name (`sampling/createMessage` / `elicitation/create` /
    /// `roots/list`). Untrusted server data.
    pub method: String,
}

/// Projection outcome of a `tasks/get` call: the info the model sees plus the
/// subset of input-request keys the caller may answer via `mcp_respond_task`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTaskProjection {
    pub info: McpTaskInfo,
    /// Input-request keys still answerable (bounded to `MAX_INPUT_REQUESTS`).
    pub answerable_keys: Vec<String>,
}

/// Result of `tools/call` when the server materialized a SEP-2663 task
/// instead of a final result. Surfaced only when the opt-in and the server
/// capability are both present; otherwise the call stays fail-closed.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolTaskHandle {
    /// Server-assigned task identifier for `tasks/get`/`tasks/cancel`.
    pub task_id: String,
    /// Seed status at creation (`working` by spec).
    pub status: String,
    /// Suggested server poll interval in milliseconds, if advertised.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_interval_ms: Option<u64>,
}

fn check_task_id(value: &str) -> Result<String, BoundaryError> {
    if value.is_empty()
        || value.len() > MAX_TASK_ID_BYTES
        || value.chars().any(crate::text::is_terminal_control)
    {
        return Err(error(
            "invalid_args",
            "MCP task identifier is empty, oversized, or invalid",
        ));
    }
    Ok(value.to_owned())
}

fn status_to_run_state(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Working => "running",
        TaskStatus::InputRequired => "needs_input",
        TaskStatus::Completed => "done",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
        _ => "unknown",
    }
}

fn status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Working => "working",
        TaskStatus::InputRequired => "input_required",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
        _ => "unknown",
    }
}

fn input_request_method(request: &rmcp::model::InputRequest) -> &'static str {
    match request {
        rmcp::model::InputRequest::CreateMessage(_) => "sampling/createMessage",
        rmcp::model::InputRequest::Elicitation(_) => "elicitation/create",
        rmcp::model::InputRequest::ListRoots(_) => "roots/list",
        _ => "unknown",
    }
}

fn project_detailed(task: rmcp::model::DetailedTask) -> McpTaskProjection {
    let rmcp::model::DetailedTask { task, payload, .. } = task;
    let mut info = McpTaskInfo {
        task_id: task.task_id,
        status: status_label(task.status).to_owned(),
        run_state: status_to_run_state(task.status).to_owned(),
        status_message: task.status_message.filter(|m| {
            !m.is_empty()
                && m.len() <= MAX_STATUS_MESSAGE_BYTES
                && !m.chars().any(crate::text::is_terminal_control)
        }),
        input_requests: Vec::new(),
        result: None,
        error: None,
        poll_interval_ms: task.poll_interval_ms,
        ttl_ms: task.ttl_ms,
    };
    let mut answerable = Vec::new();
    match payload {
        rmcp::model::TaskPayload::Working | rmcp::model::TaskPayload::Cancelled => {}
        rmcp::model::TaskPayload::InputRequired { input_requests } => {
            for (key, request) in input_requests.iter().take(MAX_INPUT_REQUESTS) {
                info.input_requests.push(McpTaskInputRequest {
                    key: key.clone(),
                    method: input_request_method(request).to_owned(),
                });
                answerable.push(key.clone());
            }
        }
        rmcp::model::TaskPayload::Completed { result } => {
            info.result = Some(Value::Object(result));
        }
        rmcp::model::TaskPayload::Failed { error: failure } => {
            info.error = Some(Value::Object(failure));
        }
        _ => {
            // Unknown future payload variant: surface the status label but do
            // not claim a terminal outcome the spec does not define.
            info.run_state = "unknown".into();
        }
    }
    // `DetailedTask` keeps `task.status` in agreement with `payload` by
    // construction, so an unparsed terminal variant only ever lands in the
    // catch-all above with `run_state = "unknown"`.
    McpTaskProjection {
        info,
        answerable_keys: answerable,
    }
}

impl McpClient {
    /// `true` when this connection opted into SEP-2663 tasks at initialize.
    pub fn tasks_enabled(&self) -> bool {
        self.tasks_enabled
    }

    /// `tasks/get`: poll the current state of one task.
    ///
    /// Fails closed: requires the client opt-in AND a server that advertised
    /// the tasks extension during capability negotiation.
    pub async fn get_task(
        &self,
        task_id: &str,
        cancel: &dyn Cancellation,
    ) -> Result<McpTaskProjection, BoundaryError> {
        let task_id = check_task_id(task_id)?;
        bounded(
            self.get_task_inner(task_id),
            self.limits.request_timeout,
            cancel,
        )
        .await?
    }

    async fn get_task_inner(&self, task_id: String) -> Result<McpTaskProjection, BoundaryError> {
        self.ensure_connected()?;
        if !self.tasks_enabled || !self.server_capabilities().tasks {
            return Err(error(
                "mcp_unsupported",
                "MCP server does not advertise the tasks extension, or tasks are disabled",
            ));
        }
        let result = self
            .service
            .get_task(GetTaskParams::new(task_id))
            .await
            .map_err(service_error)?;
        Ok(project_detailed(result.task))
    }

    /// `tasks/cancel`: signal cooperative cancellation. The acknowledgement
    /// does not guarantee the task stopped — poll `get_task` to observe the
    /// terminal `cancelled` state.
    pub async fn cancel_task(
        &self,
        task_id: &str,
        cancel: &dyn Cancellation,
    ) -> Result<(), BoundaryError> {
        let task_id = check_task_id(task_id)?;
        bounded(
            self.cancel_task_inner(task_id),
            self.limits.request_timeout,
            cancel,
        )
        .await?
    }

    async fn cancel_task_inner(&self, task_id: String) -> Result<(), BoundaryError> {
        self.ensure_connected()?;
        if !self.tasks_enabled || !self.server_capabilities().tasks {
            return Err(error(
                "mcp_unsupported",
                "MCP server does not advertise the tasks extension, or tasks are disabled",
            ));
        }
        self.service
            .cancel_task(CancelTaskParams::new(task_id))
            .await
            .map_err(service_error)
    }

    /// `tasks/update`: deliver responses to outstanding in-task input
    /// requests previously surfaced by `get_task`. Only keys the server listed
    /// as outstanding may be answered — the host never invents request ids.
    pub async fn respond_task(
        &self,
        task_id: &str,
        input_responses: BTreeMap<String, Value>,
        cancel: &dyn Cancellation,
    ) -> Result<(), BoundaryError> {
        let task_id = check_task_id(task_id)?;
        if input_responses.is_empty() {
            return Err(error(
                "invalid_args",
                "MCP task update requires at least one input response",
            ));
        }
        if input_responses.len() > MAX_INPUT_REQUESTS {
            return Err(error(
                "invalid_args",
                "MCP task update response count exceeds its limit",
            ));
        }
        bounded(
            self.respond_task_inner(task_id, input_responses),
            self.limits.request_timeout,
            cancel,
        )
        .await?
    }

    async fn respond_task_inner(
        &self,
        task_id: String,
        input_responses: BTreeMap<String, Value>,
    ) -> Result<(), BoundaryError> {
        self.ensure_connected()?;
        if !self.tasks_enabled || !self.server_capabilities().tasks {
            return Err(error(
                "mcp_unsupported",
                "MCP server does not advertise the tasks extension, or tasks are disabled",
            ));
        }
        self.service
            .update_task(UpdateTaskParams::new(task_id, input_responses))
            .await
            .map_err(service_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{DetailedTask, InputRequest, InputRequests, Task, TaskPayload};
    use serde_json::json;

    fn base_task(status: TaskStatus) -> Task {
        Task::new(
            "task-1",
            status,
            "2026-11-24T00:00:00Z",
            "2026-11-24T00:00:01Z",
        )
        .with_ttl_ms(60_000)
        .with_poll_interval_ms(2_000)
    }

    #[test]
    fn projection_maps_each_status_to_run_state() {
        for (status, run_state) in [
            (TaskStatus::Working, "running"),
            (TaskStatus::InputRequired, "needs_input"),
            (TaskStatus::Completed, "done"),
            (TaskStatus::Failed, "failed"),
            (TaskStatus::Cancelled, "cancelled"),
        ] {
            let payload = match status {
                TaskStatus::Working => TaskPayload::Working,
                TaskStatus::InputRequired => TaskPayload::InputRequired {
                    input_requests: InputRequests::new(),
                },
                TaskStatus::Completed => TaskPayload::Completed {
                    result: serde_json::Map::from_iter([("isError".to_owned(), json!(false))]),
                },
                TaskStatus::Failed => TaskPayload::Failed {
                    error: serde_json::Map::from_iter([("code".to_owned(), json!(-32603))]),
                },
                TaskStatus::Cancelled => TaskPayload::Cancelled,
                _ => unreachable!("non-exhaustive status"),
            };
            let proj = project_detailed(DetailedTask::new(base_task(status), payload));
            assert_eq!(proj.info.run_state, run_state);
            assert_eq!(proj.info.poll_interval_ms, Some(2_000));
            assert_eq!(proj.info.ttl_ms, Some(60_000));
        }
    }

    #[test]
    fn projection_caps_input_requests_and_strips_control_chars() {
        let mut input_requests = InputRequests::new();
        for index in 0..(MAX_INPUT_REQUESTS + 8) {
            let request: InputRequest = serde_json::from_value(json!({
                "method": "roots/list",
                "params": {}
            }))
            .unwrap_or_else(|_| {
                // `roots/list` is the lightest InputRequest variant to build.
                // Fall back to a form elicitation if the wire shape changes.
                serde_json::from_value(json!({
                    "method": "elicitation/create",
                    "params": {
                        "mode": "form",
                        "message": format!("q{index}"),
                        "requestedSchema": {"type": "object", "properties": {}}
                    }
                }))
                .expect("test input request must deserialize")
            });
            input_requests.insert(format!("key-{index:03}"), request);
        }
        let mut task = base_task(TaskStatus::InputRequired);
        task.status_message = Some("bad\u{7}message".into());
        let proj = project_detailed(DetailedTask::new(
            task,
            TaskPayload::InputRequired { input_requests },
        ));
        assert_eq!(proj.info.input_requests.len(), MAX_INPUT_REQUESTS);
        assert_eq!(proj.answerable_keys.len(), MAX_INPUT_REQUESTS);
        assert_eq!(proj.info.input_requests[0].method, "roots/list");
        // Control characters in the status message are stripped entirely.
        assert_eq!(proj.info.status_message, None);
    }

    #[test]
    fn projection_completed_and_failed_carry_terminal_payloads() {
        let completed = project_detailed(DetailedTask::new(
            base_task(TaskStatus::Completed),
            TaskPayload::Completed {
                result: serde_json::Map::from_iter([("ok".to_owned(), json!(true))]),
            },
        ));
        assert_eq!(completed.info.result.as_ref().unwrap()["ok"], true);
        let failed = project_detailed(DetailedTask::new(
            base_task(TaskStatus::Failed),
            TaskPayload::Failed {
                error: serde_json::Map::from_iter([("code".to_owned(), json!(-32603))]),
            },
        ));
        assert_eq!(failed.info.error.as_ref().unwrap()["code"], -32603);
    }

    #[test]
    fn task_id_validation_rejects_oversized_or_control() {
        for bad in ["", &"x".repeat(MAX_TASK_ID_BYTES + 1), "bad\u{7}id"] {
            assert!(check_task_id(bad).is_err());
        }
        assert!(check_task_id("task-abc").is_ok());
    }
}
