//! `soyo mcp-server`: expose the helper daemon as an MCP server over stdio
//! (D-108). This is a thin adapter — every tool call maps onto a published
//! `soyo-ipc` method (`run_prompt` / `get_run_state` / `read_session_snapshot`
//! / `cancel_run`), so run spawning, permissions and sandboxing stay inside
//! `HostService`. The server embeds no runtime and never spawns a run
//! in-process.
//!
//! Boundary notes:
//! - `run` is spawn-and-return: it streams the published `run_prompt` events
//!   just long enough to learn the `session_id`/`run_id`, then keeps draining
//!   in the background (D-107 — dropping the stream never cancels the run).
//! - `status`/`cancel` only need the caller's helper connection; mutating
//!   tools are annotated `readOnlyHint: false`.

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerConfig,
        Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use soyo_ipc::{
    HelperClient,
    types::{DesktopError, DesktopStreamEnvelope, RunPromptParams, SubmitPromptResultDto},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

/// How long `run` waits for the run's first schema event (which carries the
/// assigned `run_id`) before reporting the spawn attempt failed.
const RUN_STARTED_TIMEOUT: Duration = Duration::from_secs(30);

/// MCP tool names this server publishes. Frozen wire surface (D-106/D-108):
/// every name maps 1:1 onto a helper method.
const TOOL_RUN: &str = "run";
const TOOL_STATUS: &str = "status";
const TOOL_CANCEL: &str = "cancel";

/// Arguments for the `run` tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct RunToolArgs {
    prompt: String,
    /// Resume this session instead of creating a new one.
    session_id: Option<String>,
    /// Workspace override (`--cwd` equivalent).
    cwd: Option<String>,
    /// Named model profile.
    profile: Option<String>,
    /// Work mode: plan | build | code | work.
    mode: Option<String>,
    /// Permission mode: auto | bypass | manual.
    permissions: Option<String>,
    /// Accept a config change when resuming.
    accept_config_change: Option<bool>,
}

/// Arguments for the `status` tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct StatusToolArgs {
    /// Session to inspect.
    session_id: String,
    /// Optional run inside the session; defaults to the newest run row.
    run_id: Option<String>,
}

/// Arguments for the `cancel` tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct CancelToolArgs {
    /// Run id returned by `run`. Omit to cancel the caller's own active run.
    run_id: Option<String>,
}

/// The server: a lazily connected `HelperClient` plus the shared tool schema.
pub struct SoyoMcpServer {
    client: Arc<HelperClient>,
}

impl SoyoMcpServer {
    pub fn new(client: Arc<HelperClient>) -> Self {
        Self { client }
    }
}

impl ServerHandler for SoyoMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2025_11_25)
            .with_server_info(Implementation::new("soyo", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Soyo agent runtime: `run` spawns a run in a session, `status` \
                 reports run/session state, `cancel` stops a run. Every call \
                 executes inside the helper daemon's existing permission and \
                 sandbox path — this surface cannot bypass it.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tool_catalog()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_catalog().into_iter().find(|tool| tool.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = Value::Object(request.arguments.unwrap_or_default());
        let result = match request.name.as_ref() {
            TOOL_RUN => self.tool_run(args).await,
            TOOL_STATUS => self.tool_status(args).await,
            TOOL_CANCEL => self.tool_cancel(args).await,
            other => Err(McpError::invalid_params(
                format!("unknown tool '{other}'"),
                None,
            )),
        };
        result.map(CallToolResponse::from)
    }
}

impl SoyoMcpServer {
    /// `run`: spawn a run through the published `run_prompt` streaming method.
    /// The terminal `SubmitPromptResultDto` only resolves when the run ends,
    /// so the assigned ids come from the first raw schema event instead.
    async fn tool_run(&self, args: Value) -> Result<CallToolResult, McpError> {
        let parsed: RunToolArgs = parse_args(args)?;
        if parsed.prompt.trim().is_empty() {
            return Ok(tool_error_text("prompt cannot be empty"));
        }
        let mode = match parsed.mode.as_deref().map(parse_work_mode) {
            Some(Ok(mode)) => Some(mode),
            Some(Err(err)) => return Ok(tool_error_text(err)),
            None => None,
        };
        let permissions = match parsed.permissions.as_deref().map(parse_permission_mode) {
            Some(Ok(mode)) => Some(mode),
            Some(Err(err)) => return Ok(tool_error_text(err)),
            None => None,
        };
        let params = RunPromptParams {
            prompt: parsed.prompt,
            session_id: parsed.session_id,
            accept_config_change: parsed.accept_config_change.unwrap_or(false),
            cwd: parsed.cwd,
            profile: parsed.profile,
            mode,
            permissions,
            ..Default::default()
        };

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let client = Arc::clone(&self.client);
        let call = tokio::spawn(async move {
            client
                .call_streaming::<_, SubmitPromptResultDto>("run_prompt", &params, events_tx)
                .await
        });

        // Wait for the first schema event carrying the run identity. The run
        // keeps streaming after we return; draining continues on a detached
        // task so the helper's channel never fills while the caller only
        // wanted the spawn acknowledgement (D-107: detach != cancel).
        let identity = wait_for_run_identity(&mut events_rx, RUN_STARTED_TIMEOUT).await;
        tokio::spawn(async move {
            // Drain remaining events; the terminal DTO is dropped on purpose.
            while events_rx.recv().await.is_some() {}
            let _ = call.await;
        });

        match identity {
            Ok((session_id, run_id)) => Ok(CallToolResult::structured(json!({
                "action": "started",
                "session_id": session_id,
                "run_id": run_id,
            }))),
            Err(err) => Ok(tool_error_text(err)),
        }
    }

    /// `status`: project live run state plus the persisted run row for the
    /// requested session. Read-only; never opens the session for write.
    async fn tool_status(&self, args: Value) -> Result<CallToolResult, McpError> {
        let parsed: StatusToolArgs = parse_args(args)?;
        let session_id = parsed.session_id.as_str();

        let run_state = match self
            .client
            .call("get_run_state", json!({ "session_id": session_id }))
            .await
        {
            Ok(value) => value,
            Err(err) => return Ok(helper_error(&err)),
        };
        let snapshot = match self
            .client
            .call("read_session_snapshot", json!({ "session_id": session_id }))
            .await
        {
            Ok(value) => value,
            Err(err) => return Ok(helper_error(&err)),
        };

        Ok(CallToolResult::structured(project_status(
            &run_state,
            &snapshot,
            parsed.run_id.as_deref(),
        )))
    }

    /// `cancel`: forward to `cancel_run`. An explicit `run_id` targets that
    /// run; without one the helper cancels the caller-connection's active run.
    async fn tool_cancel(&self, args: Value) -> Result<CallToolResult, McpError> {
        let parsed: CancelToolArgs = parse_args(args)?;
        let result = self
            .client
            .call("cancel_run", json!({ "run_id": parsed.run_id }))
            .await;
        match result {
            Ok(value) => Ok(CallToolResult::structured(json!({
                "cancelled": value.as_bool().unwrap_or(false),
            }))),
            Err(err) => Ok(helper_error(&err)),
        }
    }
}

/// Serve the handler on stdin/stdout until the client disconnects.
pub async fn serve_stdio(client: Arc<HelperClient>) -> Result<(), String> {
    let server = SoyoMcpServer::new(client)
        .serve(rmcp::transport::io::stdio_pipes())
        .await
        .map_err(|err| format!("mcp server init failed: {err}"))?;
    server
        .waiting()
        .await
        .map_err(|err| format!("mcp server task failed: {err}"))?;
    Ok(())
}

fn tool_catalog() -> Vec<Tool> {
    let mut run = Tool::new(
        TOOL_RUN,
        "Spawn a run in a new or existing session. Returns the session_id \
         and run_id immediately; the run continues in the background \
         (disconnecting does not cancel it). Poll `status` for progress.",
        schema(json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "prompt": { "type": "string", "description": "Prompt text for the run" },
                "session_id": { "type": "string", "description": "Resume this session instead of creating a new one" },
                "cwd": { "type": "string", "description": "Workspace path override" },
                "profile": { "type": "string", "description": "Named model profile" },
                "mode": { "type": "string", "enum": ["plan", "build", "code", "work"] },
                "permissions": { "type": "string", "enum": ["auto", "bypass", "manual"] },
                "accept_config_change": { "type": "boolean" },
            },
            "additionalProperties": false,
        })),
    );
    run.title = Some("Spawn a run".into());
    run.annotations = Some(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(false)
            .idempotent(false)
            .open_world(true),
    );

    let mut status = Tool::new(
        TOOL_STATUS,
        "Report the live run state and the persisted result for a \
         session (and optionally one run). Read-only.",
        schema(json!({
            "type": "object",
            "required": ["session_id"],
            "properties": {
                "session_id": { "type": "string" },
                "run_id": { "type": "string" },
            },
            "additionalProperties": false,
        })),
    );
    status.title = Some("Query run/session status".into());
    status.annotations = Some(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    );

    let mut cancel = Tool::new(
        TOOL_CANCEL,
        "Cancel a run by id. Without run_id, cancels the caller's \
         active run. Cancellation is cooperative; poll `status` for \
         the terminal state.",
        schema(json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string" },
            },
            "additionalProperties": false,
        })),
    );
    cancel.title = Some("Cancel a run".into());
    cancel.annotations = Some(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(true)
            .idempotent(true)
            .open_world(false),
    );

    vec![run, status, cancel]
}

fn schema(value: Value) -> Arc<rmcp::model::JsonObject> {
    match value {
        Value::Object(map) => Arc::new(map),
        _ => unreachable!("tool schema must be a JSON object"),
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, McpError> {
    serde_json::from_value(args)
        .map_err(|err| McpError::invalid_params(format!("invalid arguments: {err}"), None))
}

fn parse_work_mode(raw: &str) -> Result<soyo_ipc::types::WorkModeDto, String> {
    match raw {
        "plan" => Ok(soyo_ipc::types::WorkModeDto::Plan),
        "build" => Ok(soyo_ipc::types::WorkModeDto::Build),
        "code" => Ok(soyo_ipc::types::WorkModeDto::Code),
        "work" => Ok(soyo_ipc::types::WorkModeDto::Work),
        other => Err(format!(
            "invalid mode '{other}' (expected plan|build|code|work)"
        )),
    }
}

fn parse_permission_mode(raw: &str) -> Result<soyo_ipc::types::PermissionModeDto, String> {
    match raw {
        "auto" => Ok(soyo_ipc::types::PermissionModeDto::Auto),
        "bypass" => Ok(soyo_ipc::types::PermissionModeDto::Bypass),
        "manual" => Ok(soyo_ipc::types::PermissionModeDto::Manual),
        other => Err(format!(
            "invalid permissions '{other}' (expected auto|bypass|manual)"
        )),
    }
}

/// Wait for the first streamed event that identifies the spawned run.
async fn wait_for_run_identity(
    events: &mut mpsc::UnboundedReceiver<DesktopStreamEnvelope>,
    timeout: Duration,
) -> Result<(String, String), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("timed out waiting for the run to start".into());
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(DesktopStreamEnvelope::RawSchemaEvent { event })) => {
                if let (Some(session), Some(run)) = (
                    event.get("session_id").and_then(Value::as_str),
                    event.get("run_id").and_then(Value::as_str),
                ) {
                    return Ok((session.to_owned(), run.to_owned()));
                }
                // Keep waiting — control envelopes arrive before the first
                // run-scoped schema event.
            }
            Ok(Some(DesktopStreamEnvelope::RunCompleted { status, error, .. })) => {
                let detail = error
                    .map(|err| format!("{}: {}", err.code, err.message))
                    .unwrap_or_else(|| format!("run ended with status {status}"));
                return Err(detail);
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err("helper closed the run stream before it started".into());
            }
            Err(_) => return Err("timed out waiting for the run to start".into()),
        }
    }
}

/// Merge the live `RunStateDto` and the persisted snapshot into one answer.
fn project_status(run_state: &Value, snapshot: &Value, run_id: Option<&str>) -> Value {
    let active_run = run_state
        .get("active_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let runs = snapshot
        .get("runs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let selected = match run_id {
        Some(want) => runs
            .iter()
            .find(|row| row.get("run_id").and_then(Value::as_str) == Some(want)),
        None => runs.last(),
    };
    let mut out = Map::new();
    out.insert("session_id".into(), snapshot_session_id(snapshot));
    out.insert("active_run".into(), Value::Bool(active_run));
    if let Some(state) = run_state.get("current").cloned() {
        out.insert("work_mode".into(), state);
    }
    if run_id.is_some() && selected.is_none() {
        out.insert("run_status".into(), Value::Null);
        out.insert("error".into(), json!("run not found in session"));
    } else if let Some(row) = selected {
        if let Some(id) = row.get("run_id").cloned() {
            out.insert("run_id".into(), id);
        }
        match row.get("result") {
            Some(result) if !result.is_null() => {
                out.insert("run_status".into(), result["status"].clone());
                if let Some(err) = result.get("error").cloned() {
                    out.insert("error".into(), err);
                }
            }
            _ => {
                out.insert(
                    "run_status".into(),
                    if active_run {
                        json!("running")
                    } else {
                        Value::Null
                    },
                );
            }
        }
    }
    Value::Object(out)
}

fn snapshot_session_id(snapshot: &Value) -> Value {
    snapshot
        .get("session")
        .and_then(|session| session.get("id").cloned())
        .unwrap_or(Value::Null)
}

fn tool_error_text(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

fn helper_error(err: &DesktopError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "{}: {}",
        err.code, err.message
    ))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use soyo_ipc::types::DesktopStreamEnvelope;

    #[test]
    fn catalog_has_three_tools_with_mutating_annotations() {
        let tools = tool_catalog();
        assert_eq!(tools.len(), 3);
        let by_name = |name: &str| tools.iter().find(|t| t.name == name).expect(name);
        let run = by_name(TOOL_RUN);
        assert_eq!(
            run.annotations.as_ref().unwrap().read_only_hint,
            Some(false)
        );
        let status = by_name(TOOL_STATUS);
        assert_eq!(
            status.annotations.as_ref().unwrap().read_only_hint,
            Some(true)
        );
        assert_eq!(
            status.annotations.as_ref().unwrap().idempotent_hint,
            Some(true)
        );
        let cancel = by_name(TOOL_CANCEL);
        assert_eq!(
            cancel.annotations.as_ref().unwrap().destructive_hint,
            Some(true)
        );
        for tool in &tools {
            assert_eq!(tool.input_schema["type"], "object");
            assert_eq!(tool.input_schema["additionalProperties"], false);
        }
    }

    #[test]
    fn run_args_reject_unknown_mode_and_permissions() {
        assert!(parse_work_mode("plan").is_ok());
        assert!(parse_work_mode("Plan").is_err());
        assert!(parse_permission_mode("auto").is_ok());
        assert!(parse_permission_mode("sudo").is_err());
    }

    #[test]
    fn status_projects_run_result_and_error() {
        let run_state = json!({ "current": "work", "active_run": false });
        let snapshot = json!({
            "session": { "id": "s-1" },
            "runs": [
                { "run_id": "r-1", "result": { "status": "completed" } },
                { "run_id": "r-2", "result": { "status": "failed", "error": {"code":"boom"} } },
            ],
        });
        let out = project_status(&run_state, &snapshot, Some("r-2"));
        assert_eq!(out["run_status"], "failed");
        assert_eq!(out["run_id"], "r-2");
        assert_eq!(out["error"]["code"], "boom");
    }

    #[test]
    fn status_defaults_to_last_run_and_running_flag() {
        let run_state = json!({ "current": "work", "active_run": true });
        let snapshot = json!({
            "session": { "id": "s-1" },
            "runs": [{ "run_id": "r-1", "result": Value::Null }],
        });
        let out = project_status(&run_state, &snapshot, None);
        assert_eq!(out["run_id"], "r-1");
        assert_eq!(out["run_status"], "running");
        assert_eq!(out["active_run"], true);
    }

    #[test]
    fn status_marks_missing_run() {
        let run_state = json!({ "current": "work", "active_run": false });
        let snapshot = json!({ "session": { "id": "s-1" }, "runs": [] });
        let out = project_status(&run_state, &snapshot, Some("nope"));
        assert_eq!(out["error"], "run not found in session");
    }

    #[tokio::test]
    async fn identity_comes_from_first_run_scoped_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(DesktopStreamEnvelope::RawSchemaEvent {
            event: json!({"session_id": "s-1", "type": "session.created", "data": {}}),
        })
        .unwrap();
        tx.send(DesktopStreamEnvelope::RawSchemaEvent {
            event: json!({"session_id": "s-1", "run_id": "r-9", "type": "run.started", "data": {}}),
        })
        .unwrap();
        let got = wait_for_run_identity(&mut rx, Duration::from_secs(1)).await;
        assert_eq!(got, Ok(("s-1".into(), "r-9".into())));
    }

    #[tokio::test]
    async fn identity_fails_on_run_completed_before_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(DesktopStreamEnvelope::RunCompleted {
            status: "failed".into(),
            summary_line: String::new(),
            todos: Vec::new(),
            error: Some(DesktopError::new("invalid_config", "no profile")),
            needs_input_question: None,
            compaction_pending: None,
        })
        .unwrap();
        let got = wait_for_run_identity(&mut rx, Duration::from_secs(1)).await;
        assert_eq!(got, Err("invalid_config: no profile".into()));
    }

    #[tokio::test]
    async fn identity_times_out_on_silent_stream() {
        let (_tx, mut rx) = mpsc::unbounded_channel();
        let got = wait_for_run_identity(&mut rx, Duration::from_millis(10)).await;
        assert_eq!(got, Err("timed out waiting for the run to start".into()));
    }
}
