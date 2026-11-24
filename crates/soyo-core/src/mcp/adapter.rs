use super::{McpClient, McpTool, error};
use crate::boundary::{Operation, Tool, ToolContext, ToolDefinition};
use crate::contract::{ToolCall, ToolResult};
use std::sync::Arc;

/// A single remote name and catalogue generation, published by the host registry.
pub struct McpToolAdapter {
    client: Arc<McpClient>,
    tool: McpTool,
    generation: u64,
}

impl McpToolAdapter {
    pub fn new(client: Arc<McpClient>, tool: McpTool, generation: u64) -> Self {
        Self {
            client,
            tool,
            generation,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn server_id(&self) -> &str {
        self.client.server_id()
    }
}

impl Tool for McpToolAdapter {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.tool.name.clone(),
            description: self.tool.description.clone(),
            input_schema: self.tool.input_schema.clone(),
            examples: self.tool.examples.clone(),
        }
    }

    fn execute_captured<'a>(
        &'a self,
        call: &'a ToolCall,
        context: &'a ToolContext,
        cancel: &'a dyn crate::boundary::Cancellation,
        outputs: Option<&'a crate::output_store::OutputStore>,
    ) -> Operation<'a, crate::output_store::CapturedToolResult> {
        Box::pin(async move {
            if call.name != self.tool.name {
                return Err(error(
                    "mcp_unknown_tool",
                    "MCP adapter received a different remote tool name",
                ));
            }
            let arguments = serde_json::from_str(&call.arguments)
                .map_err(|_| error("invalid_args", "MCP arguments are not valid JSON"))?;
            let capture = outputs.map(|store| {
                (
                    store,
                    crate::output_store::OutputScope {
                        session_id: context.run.session_id,
                        run_id: context.run.id,
                        call_id: call.call_id.clone(),
                        stream: crate::output_store::OutputStream::Mcp,
                    },
                )
            });
            let (outcome, outputs, capture_errors) = self
                .client
                .call_tool_captured(&call.name, arguments, self.generation, cancel, capture)
                .await?;
            let result = match outcome {
                super::CallOutcome::Complete(result) => ToolResult {
                    ok: !result.is_error,
                    data: Some(serde_json::to_value(&result).map_err(|_| {
                        error("mcp_protocol", "MCP projected output cannot be encoded")
                    })?),
                    error: result.is_error.then(|| crate::contract::ErrorInfo {
                        code: "mcp_tool_error".into(),
                        message: "MCP tool reported an execution error".into(),
                    }),
                    truncated: result.truncated,
                    media_refs: Vec::new(),
                },
                super::CallOutcome::Task(handle) => ToolResult {
                    ok: true,
                    data: Some(serde_json::json!({
                        "mcp_task": handle,
                        "note": "The server materialized a long-running task. Poll mcp_get_task for status/result; send mcp_cancel_task to request cancellation; mcp_respond_task answers input_required requests. No reply was replayed.",
                    })),
                    error: None,
                    truncated: false,
                    media_refs: Vec::new(),
                },
            };
            Ok(crate::output_store::CapturedToolResult {
                result,
                outputs,
                media: Vec::new(),
                capture_errors,
            })
        })
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _context: &'a ToolContext,
        cancel: &'a dyn crate::boundary::Cancellation,
    ) -> Operation<'a, ToolResult> {
        Box::pin(async move {
            if call.name != self.tool.name {
                return Err(error(
                    "mcp_unknown_tool",
                    "MCP adapter received a different remote tool name",
                ));
            }
            let arguments = serde_json::from_str(&call.arguments)
                .map_err(|_| error("invalid_args", "MCP arguments are not valid JSON"))?;
            let result = self
                .client
                .call_tool(&call.name, arguments, self.generation, cancel)
                .await?;
            Ok(ToolResult {
                ok: !result.is_error,
                data: Some(serde_json::to_value(&result).map_err(|_| {
                    error("mcp_protocol", "MCP projected output cannot be encoded")
                })?),
                error: result.is_error.then(|| crate::contract::ErrorInfo {
                    code: "mcp_tool_error".into(),
                    message: "MCP tool reported an execution error".into(),
                }),
                truncated: result.truncated,
                media_refs: Vec::new(),
            })
        })
    }
}
