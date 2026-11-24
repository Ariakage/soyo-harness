//! Read-only projections over a committed session snapshot: timeline replay,
//! failure pinpointing, usage aggregation and per-run reports.
//!
//! Every projection is a pure function of `SessionSnapshot` /
//! `UsageMetricsRecord` data that has already been persisted and redacted at
//! write time. Nothing here constructs a runtime, dispatches a model request,
//! or touches tool execution — replay is strictly observational.
use serde::Serialize;
use serde_json::Value;
use soyo_core::{
    contract::{
        BoundaryError, ErrorInfo, EventPayload, HistoryItem, RunId, RunResult, RunStatus,
        SessionId,
    },
    metrics::RunMetrics,
    sqlite::{RunMetricsRecord, SessionSnapshot, UsageMetricsRecord},
};

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

/// Stable step kinds emitted by the replay projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    UserText,
    AssistantText,
    ModelRequest,
    ToolCall,
    PermissionDecision,
    InputExchange,
    RunBoundary,
    Metric,
}

/// Terminal state of a projected step. `Unknown` is reserved for stored rows
/// whose finish event is absent (interrupted run, older journal formats).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Ok,
    Failed,
    Denied,
    Cancelled,
    Unknown,
}

/// One ordered, bounded entry in a replayed session timeline.
#[derive(Debug, Clone, Serialize)]
pub struct TimelineStep {
    /// Monotonic index assigned by the projection (0-based).
    pub index: usize,
    /// Event sequence when the step comes from the persisted event table;
    /// history-derived steps carry the message ordinal instead.
    pub seq: u64,
    pub kind: StepKind,
    /// Run this step belongs to, when known.
    pub run_id: Option<String>,
    /// One-line summary safe for terminals and JSON output.
    pub label: String,
    /// Input-side payload (prompt, arguments, request metadata). Bounded.
    pub input: Value,
    /// Output-side payload (response text, result, decision). Bounded.
    pub output: Value,
    pub status: StepStatus,
    /// Stable error code when `status` is `failed`/`denied`.
    pub error_code: Option<String>,
}

/// Result of projecting one session snapshot into a replayable timeline.
#[derive(Debug, Clone, Serialize)]
pub struct Timeline {
    pub session_id: String,
    pub steps: Vec<TimelineStep>,
    /// Runs that appear in the projection, in start order.
    pub runs: Vec<String>,
}

/// First-failure pinpoint plus the healthy step that preceded it.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnosis {
    pub session_id: String,
    pub run_id: Option<String>,
    /// Index into `Timeline::steps` of the first step whose status is
    /// `failed`, `denied`, or `cancelled`. `None` when the projection is clean.
    pub failed_step: Option<usize>,
    /// Index of the most recent `ok` step before `failed_step`.
    pub last_ok_step: Option<usize>,
    /// Stable error code of the first failing step.
    pub error_code: Option<String>,
    /// Human-readable summary line (kind + label + code).
    pub summary: String,
    /// Final run result carried by `run.finished`, when the run finished.
    pub run_result: Option<Value>,
    /// Total projected steps considered.
    pub step_count: usize,
}

/// Per-run report bundle combining run metadata, final result, metrics and
/// the failure pinpoint — the payload behind `soyo session report`.
#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub session_id: String,
    pub run_id: String,
    /// Persisted `runs` row (metadata + result + timestamps).
    pub run: Value,
    /// Final `RunResult` when the run finished.
    pub result: Option<RunResult>,
    /// Persisted `RunMetrics` for this run, when journaled.
    pub metrics: Option<RunMetrics>,
    pub timeline: Timeline,
    pub diagnosis: Diagnosis,
}

/// Aggregated usage and cost attribution over persisted run metrics.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageTotals {
    pub runs: u64,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    /// `microunits` per currency; empty when no run reported a cost estimate.
    pub cost: std::collections::BTreeMap<String, u64>,
    /// True when any contributing usage or cost figure was an estimate.
    pub estimated: bool,
}

/// Which dimension one [`UsageGroup`] row aggregates over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDimension {
    Run,
    Profile,
    Day,
}

/// Usage grouped one entry per key (`run`, `profile`, or `day`).
#[derive(Debug, Clone, Serialize)]
pub struct UsageGroup {
    pub dimension: UsageDimension,
    pub key: String,
    pub totals: UsageTotals,
}

/// Aggregated usage report — the payload behind `soyo usage`.
#[derive(Debug, Clone, Serialize)]
pub struct UsageReport {
    /// Session filter that was applied, if any.
    pub session_id: Option<String>,
    /// Lower `runs.started_at` bound that was applied, if any.
    pub since_unixepoch: Option<i64>,
    pub totals: UsageTotals,
    pub groups: Vec<UsageGroup>,
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_owned();
    out.push('…');
    out
}

const STEP_FIELD: usize = 4096;
const DETAIL_FIELD: usize = 65_536;

fn bounded(value: Value, limit: usize) -> Value {
    match value {
        Value::String(text) => Value::String(truncate_text(&text, limit)),
        Value::Array(items) => items
            .into_iter()
            .map(|item| bounded(item, limit))
            .collect::<Vec<_>>()
            .into(),
        Value::Object(map) => map
            .into_iter()
            .map(|(key, value)| (key, bounded(value, limit)))
            .collect::<serde_json::Map<_, _>>()
            .into(),
        other => other,
    }
}

fn event_step_kind(payload: &EventPayload) -> StepKind {
    match payload {
        EventPayload::ModelStarted { .. }
        | EventPayload::ModelFinished { .. }
        | EventPayload::ModelRouted { .. }
        | EventPayload::ModelTextDelta { .. } => StepKind::ModelRequest,
        EventPayload::ToolRequested { .. }
        | EventPayload::ToolStarted { .. }
        | EventPayload::ToolFinished { .. } => StepKind::ToolCall,
        EventPayload::PermissionReviewed { .. }
        | EventPayload::PermissionRequested { .. }
        | EventPayload::PermissionResolved { .. } => StepKind::PermissionDecision,
        EventPayload::InputRequested { .. } | EventPayload::InputReceived { .. } => {
            StepKind::InputExchange
        }
        _ => StepKind::RunBoundary,
    }
}

fn event_step(
    payload: &EventPayload,
) -> Option<(String, Value, Value, StepStatus, Option<String>)> {
    use EventPayload as E;
    let (label, input, output, status, code) = match payload {
        E::SessionCreated { profile, .. } | E::SessionResumed { profile, .. } => (
            "session".into(),
            Value::Null,
            serde_json::json!({"profile":profile}),
            StepStatus::Ok,
            None,
        ),
        E::RunStarted {
            work_mode,
            permissions,
            profile,
        } => (
            "run started".into(),
            serde_json::json!({"work_mode":work_mode,"permissions":permissions,"profile":profile}),
            Value::Null,
            StepStatus::Ok,
            None,
        ),
        E::ModeChangeRequested { from, to } => (
            "mode change requested".into(),
            serde_json::json!({"from":from,"to":to}),
            Value::Null,
            StepStatus::Ok,
            None,
        ),
        E::ModeChanged { from, to } => (
            "mode changed".into(),
            serde_json::json!({"from":from,"to":to}),
            Value::Null,
            StepStatus::Ok,
            None,
        ),
        E::ModelRouted {
            request_id,
            profile,
            model,
            reason,
            ..
        } => (
            "model routed".into(),
            serde_json::json!({"request_id":request_id,"profile":profile,"model":model}),
            serde_json::json!({"reason":reason}),
            StepStatus::Ok,
            None,
        ),
        E::ModelStarted {
            request_id,
            purpose,
            profile,
        } => (
            "model request".into(),
            serde_json::json!({"request_id":request_id,"purpose":purpose,"profile":profile}),
            Value::Null,
            StepStatus::Ok,
            None,
        ),
        E::ModelTextDelta { text, .. } => (
            "model text delta".into(),
            Value::Null,
            serde_json::json!({"text":truncate_text(text, STEP_FIELD)}),
            StepStatus::Ok,
            None,
        ),
        E::ModelFinished {
            request_id,
            purpose,
            ok,
            usage,
            error,
        } => {
            let (status, code) = if *ok {
                (StepStatus::Ok, None)
            } else {
                (
                    StepStatus::Failed,
                    error.as_ref().map(|error| error.code.clone()),
                )
            };
            (
                "model finished".into(),
                serde_json::json!({"request_id":request_id,"purpose":purpose}),
                serde_json::json!({"usage":usage,"error":error}),
                status,
                code,
            )
        }
        E::ToolRequested {
            call_id,
            name,
            args,
        } => (
            format!("tool {name}"),
            serde_json::json!({"call_id":call_id,"name":name,"args":args}),
            Value::Null,
            StepStatus::Ok,
            None,
        ),
        E::ToolStarted { call_id, name } => (
            format!("tool {name} started"),
            serde_json::json!({"call_id":call_id,"name":name}),
            Value::Null,
            StepStatus::Ok,
            None,
        ),
        E::ToolFinished {
            call_id,
            name,
            result,
        } => {
            let (status, code) = if result.ok {
                (StepStatus::Ok, None)
            } else if result.error_code() == Some(soyo_core::contract::error_codes::CANCELLED) {
                (
                    StepStatus::Cancelled,
                    result.error_code().map(str::to_owned),
                )
            } else {
                (
                    StepStatus::Failed,
                    result.error.as_ref().map(|error| error.code.clone()),
                )
            };
            (
                format!("tool {name} finished"),
                serde_json::json!({"call_id":call_id,"name":name}),
                serde_json::json!({"result":result}),
                status,
                code,
            )
        }
        E::PermissionReviewed {
            call_id,
            decision,
            source,
            reason,
        } => (
            "permission reviewed".into(),
            serde_json::json!({"call_id":call_id,"source":source}),
            serde_json::json!({"decision":decision,"reason":reason}),
            StepStatus::Ok,
            None,
        ),
        E::PermissionRequested {
            approval_id,
            call_id,
            operation_summary,
            work_mode,
        } => (
            "permission requested".into(),
            serde_json::json!({"approval_id":approval_id,"call_id":call_id,"work_mode":work_mode}),
            serde_json::json!({"operation_summary":operation_summary}),
            StepStatus::Ok,
            None,
        ),
        E::PermissionResolved {
            approval_id,
            call_id,
            decision,
            source,
        } => {
            let denied = decision == "denied";
            (
                "permission resolved".into(),
                serde_json::json!({"approval_id":approval_id,"call_id":call_id,"source":source}),
                serde_json::json!({"decision":decision}),
                if denied {
                    StepStatus::Denied
                } else {
                    StepStatus::Ok
                },
                denied.then(|| "permission_denied".to_owned()),
            )
        }
        E::InputRequested {
            input_id,
            question,
            options,
        } => (
            "input requested".into(),
            serde_json::json!({"input_id":input_id}),
            serde_json::json!({"question":question,"options":options}),
            StepStatus::Ok,
            None,
        ),
        E::InputReceived { input_id, answer } => (
            "input received".into(),
            serde_json::json!({"input_id":input_id}),
            serde_json::json!({"answer":truncate_text(answer, STEP_FIELD)}),
            StepStatus::Ok,
            None,
        ),
        E::McpStatus { servers } => (
            "mcp status".into(),
            Value::Null,
            serde_json::json!({"servers":servers}),
            StepStatus::Ok,
            None,
        ),
        E::RunFinished(result) => {
            let (status, code) = match result.status {
                RunStatus::Completed => (StepStatus::Ok, None),
                RunStatus::Cancelled => (
                    StepStatus::Cancelled,
                    Some(
                        result
                            .stop_reason
                            .clone()
                            .unwrap_or_else(|| "cancelled".into()),
                    ),
                ),
                RunStatus::Failed
                | RunStatus::NeedsConfirmation
                | RunStatus::NeedsInput
                | RunStatus::LimitReached => (
                    StepStatus::Failed,
                    Some(
                        result
                            .stop_reason
                            .clone()
                            .or_else(|| result.error.as_ref().map(|error| error.code.clone()))
                            .unwrap_or_else(|| "failed".into()),
                    ),
                ),
            };
            (
                "run finished".into(),
                Value::Null,
                serde_json::to_value(result)
                    .unwrap_or_else(|_| serde_json::json!({"status":"unavailable"})),
                status,
                code,
            )
        }
    };
    Some((label, input, output, status, code))
}

fn history_steps(
    snapshot: &SessionSnapshot,
    run_filter: &Option<String>,
    steps: &mut Vec<TimelineStep>,
) {
    use std::collections::HashMap;
    // Map (call_id,name) → run so tool history can join its run when the
    // persisted tool_calls row names one.
    let mut call_runs: HashMap<String, String> = HashMap::new();
    for call in &snapshot.tool_calls {
        if let (Some(id), Some(run)) = (
            call["call_id"].as_str().map(str::to_owned),
            call["run_id"].as_str().map(str::to_owned),
        ) {
            call_runs.insert(id, run);
        }
    }
    for (ordinal, item) in snapshot.session.history.iter().enumerate() {
        let seq = ordinal as u64;
        let (kind, label, input, output, status, code, run) = match item {
            HistoryItem::UserText(text) => (
                StepKind::UserText,
                "user".to_owned(),
                serde_json::json!({"text":display_text(text, STEP_FIELD)}),
                Value::Null,
                StepStatus::Ok,
                None,
                None,
            ),
            HistoryItem::AssistantText(text) => (
                StepKind::AssistantText,
                "assistant".to_owned(),
                Value::Null,
                serde_json::json!({"text":display_text(text, STEP_FIELD)}),
                StepStatus::Ok,
                None,
                None,
            ),
            HistoryItem::AssistantPartial(text) => (
                StepKind::AssistantText,
                "assistant (partial)".to_owned(),
                Value::Null,
                serde_json::json!({"text":display_text(text, STEP_FIELD),"partial":true}),
                StepStatus::Ok,
                None,
                None,
            ),
            HistoryItem::ToolCall(call) => {
                let run = call_runs.get(&call.call_id).cloned();
                (
                    StepKind::ToolCall,
                    format!("tool {}", call.name),
                    serde_json::json!({"call_id":call.call_id,"name":call.name,"arguments":call.arguments}),
                    Value::Null,
                    StepStatus::Ok,
                    None,
                    run,
                )
            }
            HistoryItem::ToolResult { call_id, result } => {
                let run = call_runs.get(call_id).cloned();
                let (status, code) = if result.ok {
                    (StepStatus::Ok, None)
                } else {
                    (
                        StepStatus::Failed,
                        result.error.as_ref().map(|error| error.code.clone()),
                    )
                };
                (
                    StepKind::ToolCall,
                    format!("tool result {call_id}"),
                    serde_json::json!({"call_id":call_id}),
                    serde_json::json!({"result":result}),
                    status,
                    code,
                    run,
                )
            }
            HistoryItem::ProviderItem { .. } => continue,
        };
        if let Some(wanted) = run_filter
            && let Some(run) = &run
            && run != wanted
        {
            continue;
        }
        steps.push(TimelineStep {
            index: usize::MAX,
            seq,
            kind,
            run_id: run,
            label,
            input: bounded(input, STEP_FIELD),
            output: bounded(output, DETAIL_FIELD),
            status,
            error_code: code,
        });
    }
}

/// Project a committed snapshot into a replayable, ordered timeline.
///
/// `run_filter` limits the projection to a single run's persisted events and
/// tool rows; session-level user/assistant text has no run association and is
/// always retained for context. The projection never mutates the snapshot.
pub fn project_timeline(
    snapshot: &SessionSnapshot,
    run_filter: Option<&str>,
) -> Result<Timeline, BoundaryError> {
    let session_id = snapshot.session.id.0.to_string();
    let run_filter = run_filter.map(str::to_owned);
    let mut steps = Vec::new();
    // Persisted events carry schema-v1 ordering (seq) — primary timeline.
    for raw in &snapshot.events {
        let event: soyo_core::contract::Event = serde_json::from_value(raw.clone())
            .map_err(|_| error("storage_corrupt", "invalid replay event"))?;
        let run = event.run_id.map(|run| run.0.to_string());
        if let Some(wanted) = &run_filter
            && let Some(run) = &run
            && run != wanted
        {
            continue;
        }
        let Some((label, input, output, status, code)) = event_step(&event.payload) else {
            continue;
        };
        steps.push(TimelineStep {
            index: usize::MAX,
            seq: event.seq,
            kind: event_step_kind(&event.payload),
            run_id: run,
            label,
            input: bounded(input, STEP_FIELD),
            output: bounded(output, DETAIL_FIELD),
            status,
            error_code: code,
        });
    }
    // History items (user/assistant text, tool call/result rows without a
    // matching persisted event) fill in the conversation surface.
    history_steps(snapshot, &run_filter, &mut steps);
    // Ordering: events first by seq, then history ordinals interleaved so
    // user/assistant text lands around the calls it describes.
    steps.sort_by_key(|step| step.seq);
    // Runs in start order for the boundary view.
    let mut runs: Vec<String> = Vec::new();
    for run in &snapshot.runs {
        if let Some(id) = run["run_id"].as_str() {
            runs.push(id.to_owned());
        }
    }
    for (index, step) in steps.iter_mut().enumerate() {
        step.index = index;
    }
    Ok(Timeline {
        session_id,
        steps,
        runs,
    })
}

/// Locate the first failing step and the healthy step that preceded it.
pub fn diagnose(timeline: &Timeline, run_result: Option<Value>) -> Diagnosis {
    let failed_step = timeline.steps.iter().position(|step| {
        matches!(
            step.status,
            StepStatus::Failed | StepStatus::Denied | StepStatus::Cancelled
        )
    });
    let last_ok_step = failed_step.and_then(|failed| {
        timeline.steps[..failed]
            .iter()
            .rposition(|step| step.status == StepStatus::Ok)
    });
    let error_code = failed_step.and_then(|index| timeline.steps[index].error_code.clone());
    let summary = match failed_step {
        Some(index) => {
            let step = &timeline.steps[index];
            format!(
                "first failure at step {index} ({:?} {}): {}",
                step.kind,
                step.label,
                step.error_code.as_deref().unwrap_or("unknown")
            )
        }
        None => format!("no failure across {} projected steps", timeline.steps.len()),
    };
    Diagnosis {
        session_id: timeline.session_id.clone(),
        run_id: None,
        failed_step,
        last_ok_step,
        error_code,
        summary,
        run_result,
        step_count: timeline.steps.len(),
    }
}

/// Assemble a run-scoped report from a snapshot.
///
/// The run must exist in the snapshot; its persisted metrics, timeline slice
/// and first-failure pinpoint are bundled without mutating the source.
pub fn run_report(snapshot: &SessionSnapshot, run_id: &str) -> Result<RunReport, BoundaryError> {
    let session_id = snapshot.session.id.0.to_string();
    let run = snapshot
        .runs
        .iter()
        .find(|row| row["run_id"].as_str() == Some(run_id))
        .cloned()
        .ok_or_else(|| error("run_not_found", "run not found in session snapshot"))?;
    let result: Option<RunResult> = if run["result"].is_null() {
        None
    } else {
        Some(
            serde_json::from_value(run["result"].clone())
                .map_err(|_| error("storage_corrupt", "invalid run result"))?,
        )
    };
    let metrics: Option<RunMetrics> = snapshot
        .metrics
        .iter()
        .find(|row| row["run_id"].as_str() == Some(run_id))
        .and_then(|row| serde_json::from_value(row["data"].clone()).ok());
    let timeline = project_timeline(snapshot, Some(run_id))?;
    let mut diagnosis = diagnose(
        &timeline,
        result
            .as_ref()
            .and_then(|result| serde_json::to_value(result).ok()),
    );
    diagnosis.run_id = Some(run_id.to_owned());
    Ok(RunReport {
        session_id,
        run_id: run_id.to_owned(),
        run,
        result,
        metrics,
        timeline,
        diagnosis,
    })
}

fn fold_usage_record(totals: &mut UsageTotals, metrics: &RunMetrics) {
    totals.runs = totals.runs.saturating_add(1);
    totals.requests = totals
        .requests
        .saturating_add(metrics.execution_requests)
        .saturating_add(metrics.review_requests)
        .saturating_add(metrics.compaction_requests);
    let usages = [
        metrics.execution_usage.as_ref(),
        metrics.review_usage.as_ref(),
        metrics.compaction_usage.as_ref(),
    ];
    for usage in usages.into_iter().flatten() {
        totals.input_tokens = totals
            .input_tokens
            .saturating_add(usage.input_tokens.unwrap_or(0));
        totals.output_tokens = totals
            .output_tokens
            .saturating_add(usage.output_tokens.unwrap_or(0));
        totals.cached_input_tokens = totals
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens.unwrap_or(0));
        totals.estimated |= usage.estimated;
    }
    for cost in [
        metrics.execution_cost.as_ref(),
        metrics.review_cost.as_ref(),
        metrics.compaction_cost.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let slot = totals.cost.entry(cost.currency.clone()).or_insert(0);
        *slot = slot.saturating_add(cost.microunits);
        totals.estimated |= cost.estimated;
    }
}

/// Fold per-run metrics into per-profile attribution rows.
fn profile_groups(
    record: &RunMetricsRecord,
    groups: &mut std::collections::BTreeMap<String, UsageTotals>,
) {
    for (name, attribution) in &record.metrics.by_profile {
        let group = groups.entry(name.clone()).or_default();
        group.runs = group.runs.saturating_add(1);
        group.requests = group
            .requests
            .saturating_add(attribution.execution_requests)
            .saturating_add(attribution.review_requests)
            .saturating_add(attribution.compaction_requests);
        if let Some(usage) = &attribution.usage {
            group.input_tokens = group
                .input_tokens
                .saturating_add(usage.input_tokens.unwrap_or(0));
            group.output_tokens = group
                .output_tokens
                .saturating_add(usage.output_tokens.unwrap_or(0));
            group.cached_input_tokens = group
                .cached_input_tokens
                .saturating_add(usage.cached_input_tokens.unwrap_or(0));
            group.estimated |= usage.estimated;
        }
        if let Some(cost) = &attribution.cost {
            let slot = group.cost.entry(cost.currency.clone()).or_insert(0);
            *slot = slot.saturating_add(cost.microunits);
            group.estimated |= cost.estimated;
        }
    }
}

/// Aggregate per-session run metrics into a usage report grouped by run and
/// profile.
pub fn usage_for_session(session_id: SessionId, records: Vec<RunMetricsRecord>) -> UsageReport {
    let mut totals = UsageTotals::default();
    let mut run_groups = Vec::new();
    let mut profile_groups_map: std::collections::BTreeMap<String, UsageTotals> =
        std::collections::BTreeMap::new();
    for record in &records {
        let mut run_totals = UsageTotals::default();
        fold_usage_record(&mut run_totals, &record.metrics);
        fold_usage_record(&mut totals, &record.metrics);
        run_groups.push(UsageGroup {
            dimension: UsageDimension::Run,
            key: record.run_id.0.to_string(),
            totals: run_totals,
        });
        profile_groups(record, &mut profile_groups_map);
    }
    let mut groups = run_groups;
    groups.extend(
        profile_groups_map
            .into_iter()
            .map(|(key, totals)| UsageGroup {
                dimension: UsageDimension::Profile,
                key,
                totals,
            }),
    );
    UsageReport {
        session_id: Some(session_id.0.to_string()),
        since_unixepoch: None,
        totals,
        groups,
    }
}

/// Aggregate cross-session usage records, grouped by UTC day (day_start is the
/// unix timestamp at 00:00 UTC under the supplied offset) and by run.
pub fn usage_since(
    records: Vec<UsageMetricsRecord>,
    since_unixepoch: i64,
    utc_offset_minutes: i32,
) -> UsageReport {
    const DAY: i64 = 86_400;
    let offset_secs = i64::from(utc_offset_minutes) * 60;
    let day_start = |ts: i64| -> i64 {
        let shifted = ts.saturating_add(offset_secs);
        (shifted - shifted.rem_euclid(DAY)) - offset_secs
    };
    let mut totals = UsageTotals::default();
    let mut run_groups = Vec::new();
    let mut day_groups: std::collections::BTreeMap<i64, UsageTotals> =
        std::collections::BTreeMap::new();
    for record in &records {
        let mut run_totals = UsageTotals::default();
        fold_usage_record(&mut run_totals, &record.metrics);
        fold_usage_record(&mut totals, &record.metrics);
        run_groups.push(UsageGroup {
            dimension: UsageDimension::Run,
            key: format!("{}:{}", record.session_id.0, record.run_id.0),
            totals: run_totals,
        });
        let day = day_groups.entry(day_start(record.started_at)).or_default();
        fold_usage_record(day, &record.metrics);
    }
    let mut groups = run_groups;
    groups.extend(day_groups.into_iter().map(|(day, totals)| UsageGroup {
        dimension: UsageDimension::Day,
        key: format!("day:{day}"),
        totals,
    }));
    UsageReport {
        session_id: None,
        since_unixepoch: Some(since_unixepoch),
        totals,
        groups,
    }
}

/// Render a run report as bounded Markdown.
///
/// The format mirrors the session export's escaping rules: text stays inside
/// dynamically sized fences so untrusted content cannot break the document.
pub fn report_markdown(report: &RunReport) -> Result<Vec<u8>, BoundaryError> {
    let mut out = String::new();
    out.push_str("# Soyo run report\n\n");
    out.push_str(&format!("- Session: `{}`\n", report.session_id));
    out.push_str(&format!("- Run: `{}`\n", report.run_id));
    out.push_str(&format!(
        "- Steps: {} | Failure: {}\n\n",
        report.diagnosis.step_count,
        report.diagnosis.error_code.as_deref().unwrap_or("none")
    ));
    out.push_str("## Run\n\n```json\n");
    out.push_str(
        &serde_json::to_string_pretty(&report.run)
            .map_err(|_| error("export_error", "cannot encode run"))?,
    );
    out.push_str("\n```\n\n");
    if let Some(result) = &report.result {
        out.push_str("## Result\n\n```json\n");
        out.push_str(
            &serde_json::to_string_pretty(result)
                .map_err(|_| error("export_error", "cannot encode result"))?,
        );
        out.push_str("\n```\n\n");
    }
    if let Some(metrics) = &report.metrics {
        out.push_str("## Metrics\n\n```json\n");
        out.push_str(
            &serde_json::to_string_pretty(metrics)
                .map_err(|_| error("export_error", "cannot encode metrics"))?,
        );
        out.push_str("\n```\n\n");
    }
    out.push_str("## Diagnosis\n\n");
    out.push_str(&report.diagnosis.summary);
    out.push_str("\n\n");
    out.push_str("## Timeline\n\n");
    for step in &report.timeline.steps {
        let status = match step.status {
            StepStatus::Ok => "ok",
            StepStatus::Failed => "failed",
            StepStatus::Denied => "denied",
            StepStatus::Cancelled => "cancelled",
            StepStatus::Unknown => "unknown",
        };
        out.push_str(&format!(
            "- **{}** `{}` {} ({})\n",
            step.index,
            step.label,
            status,
            step.error_code.as_deref().unwrap_or("—")
        ));
    }
    Ok(out.into_bytes())
}

/// Human-readable text for `crate::media::display`, kept inside observe so the
/// projection does not depend on the media module's internals.
fn display_text(text: &str, limit: usize) -> String {
    truncate_text(&soyo_core::media::display(text), limit)
}

/// Parse the snapshot's persisted `run_metrics` rows into usage records.
///
/// Shared by every surface that aggregates per-session usage (CLI, TUI,
/// desktop over IPC, eval) so the `run_id`/`data` row shape is decoded once.
/// Malformed rows are skipped: the projection is observational, never a gate.
pub fn metrics_records(snapshot: &SessionSnapshot) -> Vec<RunMetricsRecord> {
    let mut records = Vec::new();
    for row in &snapshot.metrics {
        if let (Some(run), Ok(metrics)) = (
            row["run_id"].as_str(),
            serde_json::from_value::<RunMetrics>(row["data"].clone()),
        ) && let Ok(run_id) = serde_json::from_value::<RunId>(serde_json::json!(run))
        {
            records.push(RunMetricsRecord { run_id, metrics });
        }
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use soyo_core::{
        contract::{
            Event, EventPayload, HistoryItem, ModelProtocol, Session, ToolCall, ToolResult,
            WorkMode,
        },
        sqlite::SessionSnapshot,
    };

    fn base_snapshot() -> SessionSnapshot {
        let session = Session {
            id: SessionId::new(),
            workspace: std::path::PathBuf::from("/tmp/ws"),
            work_mode: WorkMode::Work,
            profile: "p".into(),
            protocol: ModelProtocol::Responses,
            model: "m".into(),
            history: vec![
                HistoryItem::UserText("hello".into()),
                HistoryItem::AssistantText("hi".into()),
            ],
        };
        SessionSnapshot {
            created_at: "2026-11-24T00:00:00Z".into(),
            source_database_schema: 2,
            last_change_seq: 1,
            last_event_seq: 1,
            session,
            runs: vec![
                json!({"run_id":"r1","run":{},"result":null,"started_at":1,"finished_at":null}),
            ],
            tool_calls: vec![],
            approval_requests: vec![],
            events: vec![],
            changes: vec![],
            metrics: vec![],
            project_context: vec![],
            background_jobs: vec![],
        }
    }

    #[test]
    fn timeline_projects_user_assistant_and_tool_event() {
        let mut snapshot = base_snapshot();
        let run_id = soyo_core::contract::RunId::new();
        snapshot.runs = vec![
            json!({"run_id":run_id.0.to_string(),"run":{},"result":null,"started_at":1,"finished_at":null}),
        ];
        snapshot.events = vec![
            serde_json::to_value(Event {
                schema_version: 1,
                session_id: snapshot.session.id,
                run_id: Some(run_id),
                seq: 1,
                timestamp: "2026-11-24T00:00:01Z".into(),
                payload: EventPayload::ToolFinished {
                    call_id: "c1".into(),
                    name: "read_file".into(),
                    result: ToolResult::success(Some(json!({"text":"body"}))),
                },
            })
            .unwrap(),
        ];
        let timeline = project_timeline(&snapshot, None).unwrap();
        assert!(timeline.steps.len() >= 3);
        assert_eq!(timeline.steps[0].index, 0);
        assert!(
            timeline
                .steps
                .iter()
                .any(|step| step.label.contains("read_file"))
        );
    }

    #[test]
    fn diagnose_finds_first_failed_step_and_last_ok() {
        let mut snapshot = base_snapshot();
        let run_id = soyo_core::contract::RunId::new();
        snapshot.events = vec![
            serde_json::to_value(Event {
                schema_version: 1,
                session_id: snapshot.session.id,
                run_id: Some(run_id),
                seq: 1,
                timestamp: "2026-11-24T00:00:01Z".into(),
                payload: EventPayload::ToolFinished {
                    call_id: "ok".into(),
                    name: "read_file".into(),
                    result: ToolResult::success(None),
                },
            })
            .unwrap(),
            serde_json::to_value(Event {
                schema_version: 1,
                session_id: snapshot.session.id,
                run_id: Some(run_id),
                seq: 2,
                timestamp: "2026-11-24T00:00:02Z".into(),
                payload: EventPayload::ToolFinished {
                    call_id: "bad".into(),
                    name: "exec".into(),
                    result: ToolResult::failure("tool_error", "boom"),
                },
            })
            .unwrap(),
        ];
        let timeline = project_timeline(&snapshot, None).unwrap();
        let diagnosis = diagnose(&timeline, None);
        let failed = diagnosis.failed_step.unwrap();
        assert_eq!(
            timeline.steps[failed].error_code.as_deref(),
            Some("tool_error")
        );
        assert!(diagnosis.last_ok_step.unwrap() < failed);
        assert!(diagnosis.summary.contains("tool_error"));
    }

    #[test]
    fn usage_aggregates_requests_tokens_and_cost() {
        let session_id = SessionId::new();
        let metrics = RunMetrics {
            execution_requests: 2,
            execution_usage: Some(soyo_core::contract::Usage {
                input_tokens: Some(100),
                output_tokens: Some(40),
                cached_input_tokens: Some(10),
                estimated: false,
            }),
            execution_cost: Some(soyo_core::metrics::CostEstimate {
                currency: "USD".into(),
                microunits: 5,
                rounded_up: true,
                estimated: true,
            }),
            ..RunMetrics::default()
        };
        let report = usage_for_session(
            session_id,
            vec![RunMetricsRecord {
                run_id: soyo_core::contract::RunId::new(),
                metrics,
            }],
        );
        assert_eq!(report.totals.requests, 2);
        assert_eq!(report.totals.input_tokens, 100);
        assert_eq!(report.totals.cost["USD"], 5);
        assert!(!report.totals.estimated || report.totals.estimated);
        assert_eq!(report.groups.len(), 1);
    }

    #[test]
    fn run_report_requires_an_existing_run() {
        let snapshot = base_snapshot();
        assert!(run_report(&snapshot, "missing").is_err());
    }

    #[test]
    fn tool_call_history_projects_arguments_and_result() {
        let mut snapshot = base_snapshot();
        let call = ToolCall {
            call_id: "c1".into(),
            name: "exec".into(),
            arguments: json!({"command":"ls"}).to_string(),
        };
        snapshot
            .session
            .history
            .push(HistoryItem::ToolCall(call.clone()));
        snapshot.session.history.push(HistoryItem::ToolResult {
            call_id: call.call_id.clone(),
            result: ToolResult::failure("tool_timeout", "timed out"),
        });
        let timeline = project_timeline(&snapshot, None).unwrap();
        let tool_steps: Vec<_> = timeline
            .steps
            .iter()
            .filter(|step| step.kind == StepKind::ToolCall)
            .collect();
        assert_eq!(tool_steps.len(), 2);
        assert_eq!(tool_steps[1].status, StepStatus::Failed);
        assert_eq!(tool_steps[1].error_code.as_deref(), Some("tool_timeout"));
        let diagnosis = diagnose(&timeline, None);
        assert!(diagnosis.failed_step.is_some());
    }
}
