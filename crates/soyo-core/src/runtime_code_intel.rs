//! AgentRuntime wiring for opt-in Project Index & Sandboxed LSP Code Intelligence (`F-17`).

use super::*;
use crate::{
    approvals::{Authorization, AuthorizationInput, OneShotPermit},
    boundary::ToolContext,
    code_intel::{CodeIntelConfig, CodeIntelState, index::validate_external_cache_dir},
    contract::{
        ApprovalSource, BoundaryError, ErrorInfo, EventPayload, HistoryItem, Session, ToolCall,
        ToolResult, error_codes,
    },
    dispatch::DispatchReview,
};
use std::path::{Path, PathBuf};

fn code_intel_failed(code: &str, message: &str) -> ToolResult {
    ToolResult {
        ok: false,
        data: None,
        error: Some(ErrorInfo {
            code: code.into(),
            message: message.into(),
        }),
        truncated: false,
        media_refs: Vec::new(),
    }
}

struct CodeIntelLease<'a>(&'a OneShotPermit);
impl Drop for CodeIntelLease<'_> {
    fn drop(&mut self) {
        let _ = self.0.finish_managed();
    }
}

impl AgentRuntime {
    /// Enable opt-in workspace code intelligence (`[code_intel] enabled = true`).
    /// Rejects cache directories overlapping `<workspace>` and protects external index/LSP
    /// directories from generic workspace file tools.
    pub fn with_code_intel(mut self, config: CodeIntelConfig) -> Result<Self, BoundaryError> {
        let (_canon_ws, norm_cache) =
            validate_external_cache_dir(&config.workspace, &config.cache_dir)?;
        let ws_hash = {
            use sha2::{Digest, Sha256};
            use std::fmt::Write as _;
            let mut hasher = Sha256::new();
            hasher.update(_canon_ws.as_os_str().as_encoded_bytes());
            let mut hex = String::with_capacity(64);
            for b in hasher.finalize() {
                let _ = write!(&mut hex, "{b:02x}");
            }
            hex
        };
        self.dispatcher.protect_host_paths(vec![
            norm_cache.join("index").join(&ws_hash),
            norm_cache.join("lsp").join(&ws_hash),
        ])?;
        let state = Arc::new(CodeIntelState::new(config)?);
        self.code_intel = Some(state);
        Ok(self)
    }

    pub(super) async fn code_intel_context_envelope(
        &self,
    ) -> Result<Option<String>, BoundaryError> {
        let Some(state) = &self.code_intel else {
            return Ok(None);
        };
        let files = Some(self.dispatcher.files().as_ref());
        let Some((digest, summary)) = state.context_summary(files).await? else {
            return Ok(None);
        };
        Ok(Some(
            serde_json::json!({
                "soyo_code_intel_v1": {
                    "sha256": digest,
                    "summary": summary,
                }
            })
            .to_string(),
        ))
    }

    pub(super) async fn invalidate_code_intel_for_call(
        &self,
        call: &ToolCall,
        result: &ToolResult,
    ) {
        let Some(state) = &self.code_intel else {
            return;
        };
        if !result.ok && call.name != "exec" {
            return;
        }
        match call.name.as_str() {
            "write_file" | "edit_file" => {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments)
                    && let Some(path) = val.get("path").and_then(serde_json::Value::as_str)
                {
                    state.invalidate_path(Path::new(path)).await;
                } else {
                    state.invalidate_all().await;
                }
            }
            "apply_patch" | "subagent_merge" | "checkpoint_restore" | "exec" => {
                state.invalidate_all().await;
            }
            _ => {}
        }
    }

    pub(super) async fn execute_parallel_code_intel_on_store(
        &self,
        call: &ToolCall,
        _context: &ToolContext,
        permit: Arc<OneShotPermit>,
        cancel: &CallCancel,
        store: Arc<dyn Store>,
    ) -> Result<ToolResult, BoundaryError> {
        let _lease = CodeIntelLease(permit.as_ref());
        let Some(state) = self.code_intel.clone() else {
            let result = code_intel_failed("unknown_tool", "Code intelligence is disabled");
            commit_code_intel_buffered(&store, self.run.id, self.run.session_id, call, &result)
                .await?;
            return Ok(result);
        };
        if cancel.is_cancelled() {
            return Err(BoundaryError::new(
                error_codes::CANCELLED,
                "tool cancelled before dispatch",
            ));
        }
        let files = Some(self.dispatcher.files().as_ref());
        let mut secrets = self.setup_known_secrets.clone();
        secrets.extend(secrets::snapshot(&self.secret_source)?);
        let outcome = state.execute(call, files, &secrets, cancel).await;
        if cancel.is_cancelled() {
            return Err(BoundaryError::new(
                error_codes::CANCELLED,
                "code intelligence call cancelled",
            ));
        }
        let result = match outcome {
            Ok(res) => res,
            Err(err) => code_intel_failed(&err.0.code, &err.0.message),
        };
        commit_code_intel_buffered(&store, self.run.id, self.run.session_id, call, &result).await?;
        Ok(result)
    }

    pub(super) async fn execute_code_intel(
        &self,
        session: &mut Session,
        call: &ToolCall,
        context: &ToolContext,
        progress: &mut CallProgress,
    ) -> Result<ToolResult, BoundaryError> {
        let Some(state) = self.code_intel.clone() else {
            return self
                .save_local(
                    session,
                    call,
                    code_intel_failed("unknown_tool", "Code intelligence is unavailable"),
                )
                .await;
        };

        let operation = match crate::code_intel::validate_call(&context.workspace, call) {
            Ok(op) => op,
            Err(err) => {
                return self
                    .save_local(
                        session,
                        call,
                        code_intel_failed(&err.0.code, &err.0.message),
                    )
                    .await;
            }
        };

        self.check()?;
        let authorization = self
            .dispatcher
            .authorize_managed(
                AuthorizationInput {
                    operation: &operation,
                    call,
                    context,
                    summary: &self.summary,
                    interactive: self.confirmation.is_some(),
                },
                DispatchReview {
                    model: self.model.as_ref(),
                    budget: &self.budget,
                    config: &self.review,
                },
            )
            .await?;
        let authorization = match authorization {
            Authorization::Pending(request) => match self.confirm(&request).await? {
                Some(answer) => {
                    self.store
                        .commit(&[StoreChange::Activity {
                            run_id: self.run.id,
                            payload: EventPayload::PermissionResolved {
                                approval_id: request.approval_id.clone(),
                                call_id: call.call_id.clone(),
                                decision: if answer { "allow" } else { "deny" }.into(),
                                source: ApprovalSource::User,
                            },
                        }])
                        .await?;
                    self.dispatcher.resolve(&request.approval_id, answer)?
                }
                None => {
                    return self
                        .save_local(
                            session,
                            call,
                            code_intel_failed(
                                error_codes::NEEDS_CONFIRMATION,
                                "Code intelligence confirmation is unavailable",
                            ),
                        )
                        .await;
                }
            },
            other => other,
        };
        let permit = match authorization {
            Authorization::Permit(permit) => permit,
            Authorization::Denied(_) => {
                return self
                    .save_local(
                        session,
                        call,
                        code_intel_failed(
                            error_codes::PERMISSION_DENIED,
                            "Code intelligence operation was denied",
                        ),
                    )
                    .await;
            }
            Authorization::Stale => {
                return self
                    .save_local(
                        session,
                        call,
                        code_intel_failed(
                            error_codes::STALE_APPROVAL,
                            "Code intelligence authorization changed",
                        ),
                    )
                    .await;
            }
            _ => {
                return self
                    .save_local(
                        session,
                        call,
                        code_intel_failed(
                            error_codes::NEEDS_CONFIRMATION,
                            "Code intelligence confirmation is required",
                        ),
                    )
                    .await;
            }
        };
        let _lease = CodeIntelLease(&permit);
        self.check()?;
        self.store
            .commit(&[StoreChange::ToolIntent {
                run_id: self.run.id,
                call: call.clone(),
            }])
            .await?;
        *progress = CallProgress::Dispatched;

        let files = Some(self.dispatcher.files().as_ref());
        let mut secrets = self.setup_known_secrets.clone();
        secrets.extend(secrets::snapshot(&self.secret_source)?);
        let cancel = CallCancel::new(self.cancel.clone());
        let result = match state.execute(call, files, &secrets, &cancel).await {
            Ok(res) => res,
            Err(err) => {
                if cancel.is_cancelled() {
                    return Err(BoundaryError::new(
                        error_codes::CANCELLED,
                        "code intelligence call cancelled",
                    ));
                }
                code_intel_failed(&err.0.code, &err.0.message)
            }
        };
        cancel.stop();

        let item = HistoryItem::ToolResult {
            call_id: call.call_id.clone(),
            result: result.clone(),
        };
        if let Err(mut failure) = self
            .store
            .commit(&[
                StoreChange::ToolFinished {
                    run_id: self.run.id,
                    call_id: call.call_id.clone(),
                    result: result.clone(),
                },
                StoreChange::History {
                    session_id: session.id,
                    item: item.clone(),
                },
            ])
            .await
        {
            failure.2 = Some(Box::new(result));
            return Err(failure);
        }
        session.history.push(item);
        Ok(result)
    }
}

async fn commit_code_intel_buffered(
    store: &Arc<dyn Store>,
    run_id: crate::contract::RunId,
    session_id: crate::contract::SessionId,
    call: &ToolCall,
    result: &ToolResult,
) -> Result<(), BoundaryError> {
    store
        .commit(&[
            StoreChange::ToolIntent {
                run_id,
                call: call.clone(),
            },
            StoreChange::ToolFinished {
                run_id,
                call_id: call.call_id.clone(),
                result: result.clone(),
            },
            StoreChange::History {
                session_id,
                item: HistoryItem::ToolResult {
                    call_id: call.call_id.clone(),
                    result: result.clone(),
                },
            },
        ])
        .await
}

/// Normalize path helper exported for tests if needed.
#[allow(dead_code)]
fn _unused(_p: PathBuf) {}
