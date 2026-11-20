//! Read-only agent tool definitions, argument validation, and execution (`F-17`).

use super::{
    CodeIntelState,
    index::lexical_normalize,
    lsp::{DiagnosticEntry, EditSuggestion, EditSuggestionQuery, SourceLocation},
    symbols::SymbolKind,
};
use crate::{
    boundary::{Cancellation, ToolDefinition},
    contract::{BoundaryError, ErrorInfo, ToolCall, ToolResult},
    permissions::Operation,
    tools::read::ReadTools,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const MAX_CALL_ARGUMENT_BYTES: usize = 16 * 1024;
const MAX_OUTPUT_BYTES: usize = 128 * 1024;

pub const CODE_INTEL_TOOL_NAMES: &[&str] = &[
    "code_search_symbols",
    "code_file_outline",
    "code_definition",
    "code_references",
    "code_diagnostics",
    "code_edit_suggestions",
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

pub fn is_code_intel_tool(name: &str) -> bool {
    CODE_INTEL_TOOL_NAMES.contains(&name)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchSymbolsArgs {
    query: String,
    #[serde(default)]
    kind: Option<SymbolKind>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    use_lsp: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePathArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PositionArgs {
    path: String,
    line: u32,
    column: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferencesArgs {
    path: String,
    line: u32,
    column: u32,
    #[serde(default)]
    include_declaration: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditSuggestionsArgs {
    path: String,
    line: u32,
    column: u32,
    #[serde(default)]
    end_line: Option<u32>,
    #[serde(default)]
    end_column: Option<u32>,
    #[serde(default)]
    new_name: Option<String>,
    #[serde(default)]
    only: Vec<String>,
    #[serde(default)]
    expected_file_sha256: Option<String>,
}

pub fn definitions() -> Vec<ToolDefinition> {
    CODE_INTEL_TOOL_NAMES
        .iter()
        .filter_map(|name| definition_for(name))
        .collect()
}

pub fn definition_for(name: &str) -> Option<ToolDefinition> {
    Some(match name {
        "code_search_symbols" => ToolDefinition {
            name: "code_search_symbols".into(),
            description: "Search indexed workspace symbols (functions, structs, traits, classes, headings, sections) using the external incremental project index and optional LSP workspace/symbol fallback.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1, "maxLength": 256 },
                    "kind": {
                        "type": ["string", "null"],
                        "enum": [
                            "module", "struct", "enum", "trait", "interface", "class",
                            "function", "method", "const", "static", "type_alias", "variable", "heading", null
                        ]
                    },
                    "path": { "type": ["string", "null"], "minLength": 1, "maxLength": 1024 },
                    "limit": { "type": ["integer", "null"], "minimum": 1, "maximum": 200 },
                    "use_lsp": { "type": ["boolean", "null"] }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            examples: vec![
                json!({"query": "RoutingCoordinator"}),
                json!({"query": "handle", "kind": "function", "path": "crates/soyo-core/src", "limit": 20, "use_lsp": false}),
            ],
        },
        "code_file_outline" => ToolDefinition {
            name: "code_file_outline".into(),
            description: "List structural symbols and current SHA-256 content version for a workspace file without modifying the workspace.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1, "maxLength": 1024 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            examples: vec![json!({"path": "crates/soyo-core/src/lib.rs"})],
        },
        "code_definition" => ToolDefinition {
            name: "code_definition".into(),
            description: "Resolve LSP go-to-definition locations (1-based line and column) via a sandboxed read-only language server.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1, "maxLength": 1024 },
                    "line": { "type": "integer", "minimum": 1 },
                    "column": { "type": "integer", "minimum": 1 }
                },
                "required": ["path", "line", "column"],
                "additionalProperties": false
            }),
            examples: vec![json!({"path": "src/main.rs", "line": 12, "column": 15})],
        },
        "code_references" => ToolDefinition {
            name: "code_references".into(),
            description: "Find LSP symbol references (1-based line and column) via a sandboxed read-only language server.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1, "maxLength": 1024 },
                    "line": { "type": "integer", "minimum": 1 },
                    "column": { "type": "integer", "minimum": 1 },
                    "include_declaration": { "type": ["boolean", "null"] }
                },
                "required": ["path", "line", "column"],
                "additionalProperties": false
            }),
            examples: vec![json!({"path": "src/lib.rs", "line": 8, "column": 8, "include_declaration": true})],
        },
        "code_diagnostics" => ToolDefinition {
            name: "code_diagnostics".into(),
            description: "Inspect compiler/typechecker diagnostics for a workspace file using the sandboxed LSP session.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1, "maxLength": 1024 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            examples: vec![json!({"path": "src/lib.rs"})],
        },
        "code_edit_suggestions" => ToolDefinition {
            name: "code_edit_suggestions".into(),
            description: "Request read-only LSP refactor/rename/code-action unified diff suggestions with SHA-256 base file verification (STALE_INDEX on stale state). Never writes workspace files.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1, "maxLength": 1024 },
                    "line": { "type": "integer", "minimum": 1 },
                    "column": { "type": "integer", "minimum": 1 },
                    "end_line": { "type": ["integer", "null"], "minimum": 1 },
                    "end_column": { "type": ["integer", "null"], "minimum": 1 },
                    "new_name": { "type": ["string", "null"], "minLength": 1, "maxLength": 256 },
                    "only": {
                        "type": "array",
                        "maxItems": 8,
                        "items": { "type": "string", "minLength": 1, "maxLength": 64 }
                    },
                    "expected_file_sha256": { "type": ["string", "null"], "minLength": 64, "maxLength": 64 }
                },
                "required": ["path", "line", "column"],
                "additionalProperties": false
            }),
            examples: vec![
                json!({"path": "src/lib.rs", "line": 4, "column": 8, "new_name": "renamed_symbol"}),
                json!({"path": "src/lib.rs", "line": 10, "column": 5, "only": ["quickfix"]}),
            ],
        },
        _ => return None,
    })
}

fn check_raw_arg_bounds(call: &ToolCall) -> Result<(), BoundaryError> {
    if call.arguments.len() > MAX_CALL_ARGUMENT_BYTES {
        return Err(failure("invalid_args", "tool arguments exceed size limit"));
    }
    Ok(())
}

fn parse_args<T: for<'de> Deserialize<'de>>(call: &ToolCall) -> Result<T, BoundaryError> {
    check_raw_arg_bounds(call)?;
    serde_json::from_str::<T>(&call.arguments).map_err(|err| {
        failure(
            "invalid_args",
            format!("invalid {} arguments: {err}", call.name),
        )
    })
}

fn resolve_abs_candidate(workspace: &Path, raw: &str) -> Result<PathBuf, BoundaryError> {
    if raw.is_empty() || raw.contains('\0') {
        return Err(failure(
            "invalid_args",
            "path must be a non-empty valid path",
        ));
    }
    let candidate = Path::new(raw);
    let abs = if candidate.is_absolute() {
        lexical_normalize(candidate)
    } else {
        lexical_normalize(&workspace.join(candidate))
    };
    if !abs.is_absolute() {
        return Err(failure("invalid_path", "target path must be absolute"));
    }
    Ok(abs)
}

/// Validate a `code_*` call and build a host-managed read-only [`Operation`] for policy & approval gates.
pub fn validate_call(workspace: &Path, call: &ToolCall) -> Result<Operation, BoundaryError> {
    let ws = if workspace.is_absolute() {
        lexical_normalize(workspace)
    } else {
        std::fs::canonicalize(workspace)
            .map_err(|err| failure("invalid_path", format!("cannot resolve workspace: {err}")))?
    };

    let (paths, binding, review) = match call.name.as_str() {
        "code_search_symbols" => {
            let args: SearchSymbolsArgs = parse_args(call)?;
            if args.query.trim().is_empty() {
                return Err(failure("invalid_args", "query must not be empty"));
            }
            let target = match args.path.as_deref() {
                Some(p) => resolve_abs_candidate(&ws, p)?,
                None => ws.clone(),
            };
            (
                vec![target],
                format!("search:{}", args.query.trim()),
                json!({
                    "tool": call.name,
                    "query": args.query,
                    "path": args.path,
                    "kind": args.kind,
                }),
            )
        }
        "code_file_outline" => {
            let args: FilePathArgs = parse_args(call)?;
            let target = resolve_abs_candidate(&ws, &args.path)?;
            (
                vec![target],
                format!("outline:{}", args.path),
                json!({ "tool": call.name, "path": args.path }),
            )
        }
        "code_definition" => {
            let args: PositionArgs = parse_args(call)?;
            if args.line == 0 || args.column == 0 {
                return Err(failure("invalid_args", "line and column must be >= 1"));
            }
            let target = resolve_abs_candidate(&ws, &args.path)?;
            (
                vec![target],
                format!("definition:{}:{}:{}", args.path, args.line, args.column),
                json!({ "tool": call.name, "path": args.path, "line": args.line, "column": args.column }),
            )
        }
        "code_references" => {
            let args: ReferencesArgs = parse_args(call)?;
            if args.line == 0 || args.column == 0 {
                return Err(failure("invalid_args", "line and column must be >= 1"));
            }
            let target = resolve_abs_candidate(&ws, &args.path)?;
            (
                vec![target],
                format!("references:{}:{}:{}", args.path, args.line, args.column),
                json!({ "tool": call.name, "path": args.path, "line": args.line, "column": args.column }),
            )
        }
        "code_diagnostics" => {
            let args: FilePathArgs = parse_args(call)?;
            let target = resolve_abs_candidate(&ws, &args.path)?;
            (
                vec![target],
                format!("diagnostics:{}", args.path),
                json!({ "tool": call.name, "path": args.path }),
            )
        }
        "code_edit_suggestions" => {
            let args: EditSuggestionsArgs = parse_args(call)?;
            if args.line == 0 || args.column == 0 {
                return Err(failure("invalid_args", "line and column must be >= 1"));
            }
            if args
                .new_name
                .as_deref()
                .is_some_and(|n| n.trim().is_empty())
            {
                return Err(failure("invalid_args", "new_name must not be empty"));
            }
            let target = resolve_abs_candidate(&ws, &args.path)?;
            (
                vec![target],
                format!(
                    "edit_suggestions:{}:{}:{}",
                    args.path, args.line, args.column
                ),
                json!({
                    "tool": call.name,
                    "path": args.path,
                    "line": args.line,
                    "column": args.column,
                    "new_name": args.new_name,
                }),
            )
        }
        _ => {
            return Err(failure(
                "unknown_tool",
                format!("unknown code intelligence tool {}", call.name),
            ));
        }
    };

    let bound_key = if binding.len() > 120 {
        format!("sha256:{}", super::index::sha256_hex(binding.as_bytes()))
    } else {
        binding
    };
    Operation::code_intel(&call.name, paths, bound_key, review)
}

pub(crate) async fn execute(
    state: &CodeIntelState,
    call: &ToolCall,
    read_tools: Option<&ReadTools>,
    known_secrets: &[String],
    cancel: &dyn Cancellation,
) -> Result<ToolResult, BoundaryError> {
    let mut truncated = false;
    let data = match call.name.as_str() {
        "code_search_symbols" => {
            let args: SearchSymbolsArgs = parse_args(call)?;
            let prefix = args.path.as_deref().map(PathBuf::from);
            let mut res = {
                let mut index = state.index.lock().await;
                index.search_symbols(
                    &args.query,
                    args.kind,
                    prefix.as_deref(),
                    args.limit,
                    read_tools,
                )?
            };
            truncated = res.truncated;

            let want_lsp = args.use_lsp.unwrap_or(false)
                || (args.use_lsp != Some(false) && res.matches.is_empty());
            if want_lsp {
                let mut lsp = state.lsp.lock().await;
                if lsp.is_enabled() {
                    let max_res = state.settings().max_query_results;
                    let limit = args.limit.unwrap_or(max_res).clamp(1, max_res);
                    if let Ok(extra) = lsp
                        .workspace_symbols(&args.query, limit, read_tools, cancel)
                        .await
                    {
                        for sym in extra {
                            if let Some(kind) = args.kind
                                && sym.kind != kind
                            {
                                continue;
                            }
                            if !res.matches.iter().any(|m| {
                                m.path == sym.path
                                    && m.name == sym.name
                                    && m.start_line == sym.start_line
                            }) {
                                if res.matches.len() >= limit {
                                    truncated = true;
                                    break;
                                }
                                res.matches.push(sym);
                            }
                        }
                    }
                }
            }
            serde_json::to_value(res).map_err(|e| failure("encode_error", e.to_string()))?
        }
        "code_file_outline" => {
            let args: FilePathArgs = parse_args(call)?;
            let mut index = state.index.lock().await;
            let outline = index.file_outline(Path::new(&args.path), read_tools)?;
            serde_json::to_value(outline).map_err(|e| failure("encode_error", e.to_string()))?
        }
        "code_definition" => {
            let args: PositionArgs = parse_args(call)?;
            let mut lsp = state.lsp.lock().await;
            let locations: Vec<SourceLocation> = lsp
                .definition(
                    Path::new(&args.path),
                    args.line,
                    args.column,
                    read_tools,
                    cancel,
                )
                .await?;
            json!({
                "path": args.path,
                "line": args.line,
                "column": args.column,
                "locations": locations,
            })
        }
        "code_references" => {
            let args: ReferencesArgs = parse_args(call)?;
            let mut lsp = state.lsp.lock().await;
            let locations: Vec<SourceLocation> = lsp
                .references(
                    Path::new(&args.path),
                    args.line,
                    args.column,
                    args.include_declaration.unwrap_or(true),
                    read_tools,
                    cancel,
                )
                .await?;
            json!({
                "path": args.path,
                "line": args.line,
                "column": args.column,
                "locations": locations,
            })
        }
        "code_diagnostics" => {
            let args: FilePathArgs = parse_args(call)?;
            let mut lsp = state.lsp.lock().await;
            let diagnostics: Vec<DiagnosticEntry> = lsp
                .diagnostics(Path::new(&args.path), read_tools, cancel)
                .await?;
            json!({
                "path": args.path,
                "diagnostics": diagnostics,
            })
        }
        "code_edit_suggestions" => {
            let args: EditSuggestionsArgs = parse_args(call)?;
            let mut lsp = state.lsp.lock().await;
            let suggestions: Vec<EditSuggestion> = lsp
                .edit_suggestions(
                    EditSuggestionQuery {
                        file: Path::new(&args.path),
                        line: args.line,
                        column: args.column,
                        end_line: args.end_line,
                        end_column: args.end_column,
                        new_name: args.new_name.as_deref(),
                        only_kinds: &args.only,
                        expected_file_sha256: args.expected_file_sha256.as_deref(),
                    },
                    read_tools,
                    cancel,
                )
                .await?;
            json!({
                "path": args.path,
                "read_only": true,
                "suggestions": suggestions,
            })
        }
        _ => {
            return Err(failure(
                "unknown_tool",
                format!("unknown code intelligence tool {}", call.name),
            ));
        }
    };

    let (bounded_data, size_truncated) = bound_json_payload(data, MAX_OUTPUT_BYTES)?;
    let raw_result = ToolResult {
        ok: true,
        data: Some(bounded_data),
        error: None,
        truncated: truncated || size_truncated,
        media_refs: Vec::new(),
    };
    crate::redaction::tool_result(&raw_result, known_secrets)
}

fn bound_json_payload(mut data: Value, max_bytes: usize) -> Result<(Value, bool), BoundaryError> {
    let encoded = serde_json::to_vec(&data).map_err(|e| failure("encode_error", e.to_string()))?;
    if encoded.len() <= max_bytes {
        return Ok((data, false));
    }
    for array_key in ["suggestions", "locations", "diagnostics", "matches"] {
        loop {
            let can_shrink = data
                .get(array_key)
                .and_then(Value::as_array)
                .is_some_and(|arr| arr.len() > 1);
            if !can_shrink {
                break;
            }
            if let Some(arr) = data.get_mut(array_key).and_then(Value::as_array_mut) {
                arr.pop();
            }
            if serde_json::to_vec(&data)
                .map(|b| b.len() <= max_bytes)
                .unwrap_or(false)
            {
                if let Some(obj) = data.as_object_mut() {
                    obj.insert("truncated".into(), Value::Bool(true));
                }
                return Ok((data, true));
            }
        }
    }
    let truncated_str: String = serde_json::to_string(&data)
        .unwrap_or_default()
        .chars()
        .take(max_bytes / 2)
        .collect();
    Ok((
        json!({
            "truncated": true,
            "excerpt": truncated_str,
        }),
        true,
    ))
}
