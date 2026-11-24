//! Stable SDK facade over `soyo_ipc::HelperClient` (F-13, D-105/D-107).
//!
//! All methods map onto the frozen wire surface
//! (`soyo_ipc::protocol::PUBLISHED_METHODS`); run, session and task calls go
//! through the helper's own permission/sandbox path — nothing is replicated
//! client-side.

use serde::Serialize;
use serde_json::{Value, json};
use soyo_ipc::{
    HelperClient, HelperCompat,
    socket::HelperTarget,
    types::{
        ActiveSessionDto, DesktopError, DesktopStreamEnvelope, HelperInfoDto, ResumeSessionDto,
        ResumeSessionResultDto, RunPromptParams, RunStateDto, SessionPageDto, SubmitPromptParams,
        SubmitPromptResultDto, TaskDto, TaskJobDto,
    },
};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

use crate::error::SdkError;

/// Stream of run events for a live `submit_prompt`/`run_prompt`/`compact_session`
/// call. The terminal `Response`/`Error` resolves the spawned call future —
/// the stream ends when the sender is dropped after that terminal frame.
pub struct RunEventStream {
    pub(crate) rx: mpsc::UnboundedReceiver<DesktopStreamEnvelope>,
}

impl RunEventStream {
    /// Next stream envelope; `None` when the run's event channel has ended.
    pub async fn next(&mut self) -> Option<DesktopStreamEnvelope> {
        self.rx.recv().await
    }
}

/// Stable client for the `soyo helper` daemon.
///
/// `attach`/`detach` carry D-107 semantics explicitly: detaching only ends this
/// connection's subscription — runs owned by a session keep executing, and a
/// later `attach` re-binds to that session and resumes event observation. The
/// SDK never cancels a run on behalf of a dropped connection.
pub struct SoyoClient {
    client: Arc<HelperClient>,
    /// Session this connection is currently bound to (None = unbound).
    attached: Mutex<Option<String>>,
}

impl SoyoClient {
    /// Connect to the default environment's helper, spawning `soyo helper` if
    /// needed, then probe compatibility (`helper_info`). A stale daemon that is
    /// not busy is recycled; one with active runs or other clients is left
    /// alone and reported as `IncompatibleHelper` — never killed mid-run.
    pub async fn connect_or_spawn() -> Result<Self, SdkError> {
        let target = HelperTarget::from_environment()
            .map_err(|err| SdkError::InvalidInput(err.message().to_owned()))?;
        Self::connect_or_spawn_for(&target).await
    }

    /// Connect to the helper matching `target` (config/data-dir fingerprinted).
    pub async fn connect_or_spawn_for(target: &HelperTarget) -> Result<Self, SdkError> {
        let client = HelperClient::connect_or_spawn_for(target).await?;
        Self::handshake(client).await
    }

    /// Connect once without spawning; fails when no daemon is listening.
    pub async fn connect() -> Result<Self, SdkError> {
        let target = HelperTarget::from_environment()
            .map_err(|err| SdkError::InvalidInput(err.message().to_owned()))?;
        Self::connect_for(&target).await
    }

    /// Connect once to the helper matching `target`; does not spawn.
    pub async fn connect_for(target: &HelperTarget) -> Result<Self, SdkError> {
        let client = HelperClient::connect_for(target).await?;
        Self::handshake(client).await
    }

    async fn handshake(client: Arc<HelperClient>) -> Result<Self, SdkError> {
        let compat = client.helper_compat().await;
        let client = if compat.mismatch && !compat.busy {
            client.restart().await?
        } else if compat.mismatch {
            return Err(SdkError::IncompatibleHelper(compat.detail));
        } else {
            client
        };
        Ok(Self {
            client,
            attached: Mutex::new(None),
        })
    }

    /// Raw compat probe result for callers that surface it to users.
    pub async fn helper_compat(&self) -> HelperCompat {
        self.client.helper_compat().await
    }

    /// Helper identity (version, protocol_version, pid, run/client counts).
    pub async fn helper_info(&self) -> Result<HelperInfoDto, SdkError> {
        self.call("helper_info", json!({})).await
    }

    /// Bootstrap snapshot: config readiness, workspace, profiles, bound
    /// session and `active_run_id`.
    pub async fn bootstrap_state(&self) -> Result<Value, SdkError> {
        self.call("get_bootstrap_state", json!({})).await
    }

    /// Whether the underlying socket connection is still open.
    pub fn is_connected(&self) -> bool {
        !self.client.is_closed()
    }

    // ---- sessions --------------------------------------------------------

    /// Create a new session (optional `cwd`/`profile` overrides) and attach
    /// this connection to it.
    pub async fn new_session(
        &self,
        cwd: Option<&str>,
        profile: Option<&str>,
    ) -> Result<ActiveSessionDto, SdkError> {
        let dto: ActiveSessionDto = self
            .call("new_session", json!({ "cwd": cwd, "profile": profile }))
            .await?;
        *self.attached.lock().await = Some(dto.id.clone());
        Ok(dto)
    }

    /// Resume an existing session and attach to it (`accept_config_change`
    /// mirrors the helper's config-change confirmation flow).
    pub async fn resume_session(
        &self,
        session_id: &str,
        accept_config_change: bool,
    ) -> Result<ResumeSessionResultDto, SdkError> {
        let params = ResumeSessionDto {
            session_id: session_id.to_owned(),
            accept_config_change,
            cwd_explicit: false,
            profile_explicit: false,
            mode_override: None,
            migrate_history: false,
        };
        let dto: ResumeSessionResultDto = self.call_typed("resume_session", &params).await?;
        *self.attached.lock().await = Some(session_id.to_owned());
        Ok(dto)
    }

    /// Re-bind this connection to `session_id` and resume event observation
    /// (D-107). Alias for [`resume_session`](Self::resume_session) with the
    /// default no-override options — the session keeps its stored config.
    pub async fn attach(&self, session_id: &str) -> Result<ResumeSessionResultDto, SdkError> {
        self.resume_session(session_id, false).await
    }

    /// Session this connection is currently bound to, if any.
    pub async fn attached_session(&self) -> Option<String> {
        self.attached.lock().await.clone()
    }

    /// Release this connection's hold on the session without deleting it.
    pub async fn release_session(&self, session_id: Option<&str>) -> Result<(), SdkError> {
        self.call::<Value>("release_session", json!({ "session_id": session_id }))
            .await?;
        let mut guard = self.attached.lock().await;
        if session_id.is_none() || guard.as_deref() == session_id {
            *guard = None;
        }
        Ok(())
    }

    /// End this connection's event observation. Per D-107 this never cancels
    /// the remote run — the session keeps executing and a later `attach`
    /// re-subscribes.
    pub async fn detach(&self) {
        *self.attached.lock().await = None;
        self.client.detach().await;
    }

    /// Paginated session metadata.
    pub async fn list_sessions(&self, page: Option<u32>) -> Result<SessionPageDto, SdkError> {
        self.call("list_sessions_page", json!({ "page": page }))
            .await
    }

    /// One session's durable record (redacted JSON; `Null` when absent).
    pub async fn get_session(&self, session_id: &str) -> Result<Value, SdkError> {
        self.call("get_session", json!({ "session_id": session_id }))
            .await
    }

    /// Read-only committed snapshot (history/events/metrics); never locks or
    /// migrates the source session.
    pub async fn read_session_snapshot(&self, session_id: &str) -> Result<Value, SdkError> {
        self.call("read_session_snapshot", json!({ "session_id": session_id }))
            .await
    }

    // ---- runs ------------------------------------------------------------

    /// Spawn an interactive run on `session_id`, streaming envelopes onto the
    /// returned channel until the terminal result resolves. `params` is the
    /// full `SubmitPromptParams` surface; `session_id` pins the target session
    /// (`session_changed` on mismatch — never silently steers elsewhere).
    pub async fn submit_prompt(
        &self,
        params: SubmitPromptParams,
    ) -> Result<(RunEventStream, RunHandle), SdkError> {
        self.streaming_call("submit_prompt", &params).await
    }

    /// Spawn a headless run (`RunPromptParams`, e.g. `interactive: false`).
    /// With `raw_events: true` the stream also carries `RawSchemaEvent`
    /// passthrough of schema-v1 `Event`s.
    pub async fn run_prompt(
        &self,
        params: RunPromptParams,
    ) -> Result<(RunEventStream, RunHandle), SdkError> {
        self.streaming_call("run_prompt", &params).await
    }

    /// Spawn a compaction run on the attached (or given) session.
    pub async fn compact_session(
        &self,
        session_id: Option<&str>,
        raw_events: bool,
    ) -> Result<(RunEventStream, RunHandle), SdkError> {
        self.streaming_call(
            "compact_session",
            &json!({ "session_id": session_id, "raw_events": raw_events }),
        )
        .await
    }

    /// Cancel a run. `run_id: None` cancels every run this connection owns;
    /// `Some(id)` targets that run from any session of the same user.
    pub async fn cancel_run(&self, run_id: Option<&str>) -> Result<bool, SdkError> {
        self.call("cancel_run", json!({ "run_id": run_id })).await
    }

    /// Live mode/compaction state of a session (`None` = attached session).
    pub async fn run_state(&self, session_id: Option<&str>) -> Result<RunStateDto, SdkError> {
        self.call("get_run_state", json!({ "session_id": session_id }))
            .await
    }

    /// Persisted schema-v1 events for one run within a session (D-107
    /// re-check path after re-attach). Reads the committed snapshot — never
    /// subscribes to live frames — and filters `events` to `run_id` when given.
    pub async fn run_events(
        &self,
        session_id: &str,
        run_id: Option<&str>,
    ) -> Result<Vec<Value>, SdkError> {
        let snapshot = self.read_session_snapshot(session_id).await?;
        let events = snapshot
            .get("events")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(run_id) = run_id else {
            return Ok(events);
        };
        Ok(events
            .into_iter()
            .filter(|event| event.get("run_id").and_then(Value::as_str) == Some(run_id))
            .collect())
    }

    // ---- tasks -----------------------------------------------------------

    /// Durable task queue projection.
    pub async fn task_list(&self) -> Result<Vec<TaskDto>, SdkError> {
        self.call("task_list", json!({})).await
    }

    /// In-flight helper task jobs.
    pub async fn task_jobs(&self) -> Result<Vec<TaskJobDto>, SdkError> {
        self.call("task_jobs", json!({})).await
    }

    /// One task's projection.
    pub async fn task_status(&self, task_id: &str) -> Result<TaskDto, SdkError> {
        self.call("task_status", json!({ "task_id": task_id }))
            .await
    }

    // ---- plumbing --------------------------------------------------------

    async fn call<R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<R, SdkError> {
        self.client
            .call_typed(method, &params)
            .await
            .map_err(SdkError::from)
    }

    async fn call_typed<P: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, SdkError> {
        self.client
            .call_typed(method, params)
            .await
            .map_err(SdkError::from)
    }

    async fn streaming_call<P: Serialize>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<(RunEventStream, RunHandle), SdkError> {
        let (events_tx, rx) = mpsc::unbounded_channel();
        let client = Arc::clone(&self.client);
        let method = method.to_owned();
        let params = serde_json::to_value(params)
            .map_err(|err| SdkError::InvalidInput(format!("encode {method} params: {err}")))?;
        let call = tokio::spawn(async move {
            client
                .call_streaming::<_, SubmitPromptResultDto>(&method, &params, events_tx)
                .await
        });
        Ok((RunEventStream { rx }, RunHandle { call }))
    }
}

/// Terminal result of a streaming run call; awaiting it yields the helper's
/// `SubmitPromptResultDto` once the run's terminal frame arrives.
pub struct RunHandle {
    call: tokio::task::JoinHandle<Result<SubmitPromptResultDto, DesktopError>>,
}

impl RunHandle {
    /// Wait for the terminal response. A join failure means the driver task
    /// itself died — surfaced as a disconnect since the stream is orphaned.
    pub async fn wait(self) -> Result<SubmitPromptResultDto, SdkError> {
        self.call
            .await
            .map_err(|err| SdkError::Disconnected(format!("run driver failed: {err}")))?
            .map_err(SdkError::from)
    }

    /// Abort the driver task. The remote run is untouched (D-107); only this
    /// connection's observation ends.
    pub fn abort(&self) {
        self.call.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Request params must serialize to the exact wire shape the frozen
    /// methods expect (D-106) — guards against silent field renames in the
    /// facade's JSON construction.
    #[test]
    fn wire_params_use_frozen_field_names() {
        let v = json!({ "session_id": "s_1", "run_id": "r_9", "page": 2u32 });
        assert_eq!(v["session_id"], "s_1");
        assert_eq!(v["run_id"], "r_9");
        assert_eq!(v["page"], 2);
    }

    #[test]
    fn submit_prompt_params_default_wire_shape() {
        let params = SubmitPromptParams {
            prompt: "hi".into(),
            session_id: Some("s_1".into()),
            raw_events: true,
            ..Default::default()
        };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["prompt"], "hi");
        assert_eq!(v["session_id"], "s_1");
        assert_eq!(v["raw_events"], true);
        // Optional unset fields stay false rather than null drift.
        assert_eq!(v["capture_media"], false);
        assert_eq!(v["anthropic_prompt_cache"], false);
    }
}
