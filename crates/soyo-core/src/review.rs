//! Independent model review for operations selected by the permission policy.
//! This component never executes tools and does not grant persistent approvals.

use crate::{
    boundary::{Cancellation, Model, ModelRequest, ModelUpdate},
    contract::{BoundaryError, ErrorInfo, HistoryItem, ModelPurpose, PermissionMode, WorkMode},
    metrics::Metrics,
    permissions::{Decision, Operation, Policy},
    provider::routing::{ModelRole, RouteDecisionRecord, RouteRequirements},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::{Instant, sleep};

fn failure(code: &str, message: &str) -> BoundaryError {
    BoundaryError(
        ErrorInfo {
            code: code.into(),
            message: message.into(),
        },
        None,
        None,
    )
}

struct BudgetState {
    count: usize,
    started: Instant,
    paused_at: Option<Instant>,
    paused: Duration,
    self_paused: bool,
    child_pause_depth: usize,
}
/// Shared per-run model request and active-time budget. Execution and review
/// callers must use the same instance; each attempt consumes one request.
#[derive(Clone)]
pub struct SharedBudget {
    state: Arc<Mutex<BudgetState>>,
    max_requests: usize,
    active_limit: Duration,
    parent: Option<Arc<SharedBudget>>,
}
impl SharedBudget {
    /// Start timing when a newly constructed runtime actually begins its run.
    pub(crate) fn start_run(&self) -> Result<(), BoundaryError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("budget_error", "budget lock poisoned"))?;
        if state.count != 0 || state.self_paused || state.child_pause_depth != 0 {
            return Err(failure("budget_error", "budget has already been used"));
        }
        state.started = Instant::now();
        state.paused = Duration::ZERO;
        Ok(())
    }
    pub fn new(max_requests: usize, active_limit: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(BudgetState {
                count: 0,
                started: Instant::now(),
                paused_at: None,
                paused: Duration::ZERO,
                self_paused: false,
                child_pause_depth: 0,
            })),
            max_requests,
            active_limit,
            parent: None,
        }
    }
    /// Create a bounded child budget linked to `parent`. Every request
    /// consumed from the child also consumes from `parent`, and remaining
    /// active time never exceeds `parent.remaining()`.
    pub fn child(parent: &Self, max_requests: usize, active_limit: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(BudgetState {
                count: 0,
                started: Instant::now(),
                paused_at: None,
                paused: Duration::ZERO,
                self_paused: false,
                child_pause_depth: 0,
            })),
            max_requests,
            active_limit,
            parent: Some(Arc::new(parent.clone())),
        }
    }
    /// Consume one request, returning a limit error when exhausted.
    pub fn consume(&self) -> Result<(), BoundaryError> {
        {
            let mut s = self
                .state
                .lock()
                .map_err(|_| failure("budget_error", "budget lock poisoned"))?;
            if s.self_paused {
                return Err(failure(
                    "budget_error",
                    "model request while active time is paused",
                ));
            }
            let now = s.paused_at.unwrap_or_else(Instant::now);
            if s.count >= self.max_requests
                || now.duration_since(s.started).saturating_sub(s.paused) >= self.active_limit
            {
                return Err(failure(
                    "limit_reached",
                    "model request or active time budget exceeded",
                ));
            }
            s.count += 1;
        }
        if let Some(parent) = &self.parent
            && let Err(error) = parent.consume()
        {
            if let Ok(mut s) = self.state.lock() {
                s.count = s.count.saturating_sub(1);
            }
            return Err(error);
        }
        Ok(())
    }
    pub fn used(&self) -> Result<usize, BoundaryError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| failure("budget_error", "budget lock poisoned"))?
            .count)
    }
    pub fn remaining(&self) -> Result<Duration, BoundaryError> {
        let local = {
            let s = self
                .state
                .lock()
                .map_err(|_| failure("budget_error", "budget lock poisoned"))?;
            let now = s.paused_at.unwrap_or_else(Instant::now);
            self.active_limit
                .saturating_sub(now.duration_since(s.started).saturating_sub(s.paused))
        };
        match &self.parent {
            Some(parent) => Ok(local.min(parent.remaining()?)),
            None => Ok(local),
        }
    }
    /// Pause active time during a human confirmation or clarification.
    pub fn pause(&self) -> Result<(), BoundaryError> {
        let propagate = {
            let mut s = self
                .state
                .lock()
                .map_err(|_| failure("budget_error", "budget lock poisoned"))?;
            let was_paused = s.self_paused || s.child_pause_depth > 0;
            s.self_paused = true;
            if !was_paused {
                s.paused_at = Some(Instant::now());
                true
            } else {
                false
            }
        };
        if propagate
            && let Some(parent) = &self.parent
            && let Err(error) = parent.pause_child()
        {
            if let Ok(mut s) = self.state.lock() {
                s.self_paused = false;
                if s.child_pause_depth == 0 {
                    s.paused_at = None;
                }
            }
            return Err(error);
        }
        Ok(())
    }
    /// Resume active time after human input.
    pub fn resume(&self) -> Result<(), BoundaryError> {
        let propagate = {
            let mut s = self
                .state
                .lock()
                .map_err(|_| failure("budget_error", "budget lock poisoned"))?;
            if !s.self_paused {
                return Ok(());
            }
            s.self_paused = false;
            if s.child_pause_depth == 0 {
                if let Some(since) = s.paused_at.take() {
                    s.paused += since.elapsed();
                }
                true
            } else {
                false
            }
        };
        if propagate && let Some(parent) = &self.parent {
            parent.resume_child()?;
        }
        Ok(())
    }
    /// Reference-counted clock pause used when a child subagent awaits human input.
    pub fn pause_child(&self) -> Result<(), BoundaryError> {
        let propagate = {
            let mut s = self
                .state
                .lock()
                .map_err(|_| failure("budget_error", "budget lock poisoned"))?;
            let was_paused = s.self_paused || s.child_pause_depth > 0;
            s.child_pause_depth = s.child_pause_depth.saturating_add(1);
            if !was_paused {
                s.paused_at = Some(Instant::now());
                true
            } else {
                false
            }
        };
        if propagate
            && let Some(parent) = &self.parent
            && let Err(error) = parent.pause_child()
        {
            if let Ok(mut s) = self.state.lock() {
                s.child_pause_depth = s.child_pause_depth.saturating_sub(1);
                if !s.self_paused && s.child_pause_depth == 0 {
                    s.paused_at = None;
                }
            }
            return Err(error);
        }
        Ok(())
    }
    /// Release one child pause reference on the active-time clock.
    pub fn resume_child(&self) -> Result<(), BoundaryError> {
        let propagate = {
            let mut s = self
                .state
                .lock()
                .map_err(|_| failure("budget_error", "budget lock poisoned"))?;
            if s.child_pause_depth == 0 {
                return Ok(());
            }
            s.child_pause_depth -= 1;
            if !s.self_paused && s.child_pause_depth == 0 {
                if let Some(since) = s.paused_at.take() {
                    s.paused += since.elapsed();
                }
                true
            } else {
                false
            }
        };
        if propagate && let Some(parent) = &self.parent {
            parent.resume_child()?;
        }
        Ok(())
    }
}
impl Default for SharedBudget {
    fn default() -> Self {
        Self::new(64, Duration::from_secs(1800))
    }
}

/// Runtime-controlled review settings; credentials must never be placed in a
/// request. `known_secrets` are redacted from the data and model reason.
#[derive(Clone)]
pub struct ReviewConfig {
    pub execution_profile: String,
    pub review_profile: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
    /// Zero disables only the total cap; connect, idle and active budgets remain.
    pub total_timeout: Duration,
    pub known_secrets: Vec<String>,
    pub retry_jitter_max: Duration,
}
impl ReviewConfig {
    pub fn new(execution_profile: String) -> Self {
        Self {
            execution_profile,
            review_profile: None,
            max_output_tokens: None,
            connect_timeout: Duration::from_secs(300),
            idle_timeout: Duration::from_secs(900),
            total_timeout: Duration::ZERO,
            known_secrets: vec![],
            retry_jitter_max: Duration::from_millis(250),
        }
    }
}
/// Where a decision was made. HumanFallback means a user confirmation is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecisionSource {
    Rule,
    Model,
    HumanFallback,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOutcome {
    pub decision: Decision,
    pub reason: String,
    pub source: ReviewDecisionSource,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelDecision {
    decision: ModelChoice,
    reason: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ModelChoice {
    Allow,
    Ask,
    Deny,
}

fn jitter(max: Duration) -> Duration {
    let millis = max.as_millis().min(u128::from(u64::MAX - 1)) as u64;
    if millis == 0 {
        Duration::ZERO
    } else {
        Duration::from_millis((uuid::Uuid::now_v7().as_u128() as u64) % (millis + 1))
    }
}
async fn bounded<T>(
    future: impl Future<Output = Result<T, BoundaryError>>,
    cancel: &dyn Cancellation,
    budget: &SharedBudget,
    duration: Duration,
) -> Result<T, BoundaryError> {
    if cancel.is_cancelled() {
        return Err(failure("cancelled", "review cancelled"));
    }
    let remaining = budget.remaining()?;
    if remaining.is_zero() {
        return Err(failure("limit_reached", "active time budget exceeded"));
    }
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(failure("cancelled", "review cancelled")),
        _ = sleep(remaining) => Err(failure("limit_reached", "active time budget exceeded")),
        _ = sleep(duration) => Err(failure("model_timeout", "review request timed out")),
        result = future => result,
    }
}

/// Data for a single independent review request. Tool arguments are data, not instructions.
pub struct ReviewInput<'a> {
    pub mode: WorkMode,
    pub permissions: PermissionMode,
    pub operation: &'a Operation,
    pub arguments: Value,
    pub goal_summary: &'a str,
}

fn extract_fenced_json(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let Some(newline) = rest.find('\n') else {
        return trimmed;
    };
    let lang = rest[..newline].trim();
    if !lang.is_empty() && !lang.eq_ignore_ascii_case("json") {
        return trimmed;
    }
    let body = &rest[newline + 1..];
    let Some(inner) = body.strip_suffix("```") else {
        return trimmed;
    };
    inner.trim()
}

/// Evaluate the policy, then only when required call a separate, tool-free model
/// request. Malformed/failed reviews fall back to `Ask`; cancellation and budget
/// exhaustion are returned as errors and must stop dispatch.
pub async fn review_operation(
    policy: &Policy,
    input: ReviewInput<'_>,
    model: &dyn Model,
    budget: &SharedBudget,
    config: &ReviewConfig,
    cancel: &dyn Cancellation,
) -> Result<ReviewOutcome, BoundaryError> {
    let (outcome, _) =
        review_operation_routed(policy, input, model, budget, config, cancel, None).await?;
    Ok(outcome)
}

/// Variant used internally by [`ApprovalManager`] that also returns per-attempt
/// model route decisions (`ModelRouted` audit records) when opt-in routing is
/// enabled (`F-16`).
pub(crate) async fn review_operation_routed(
    policy: &Policy,
    input: ReviewInput<'_>,
    model: &dyn Model,
    budget: &SharedBudget,
    config: &ReviewConfig,
    cancel: &dyn Cancellation,
    metrics: Option<&Metrics>,
) -> Result<(ReviewOutcome, Vec<RouteDecisionRecord>), BoundaryError> {
    if cancel.is_cancelled() {
        return Err(failure("cancelled", "review cancelled"));
    }
    if config
        .max_output_tokens
        .is_some_and(|tokens| tokens == 0 || tokens > crate::config::MAX_OUTPUT_TOKENS)
    {
        return Err(failure(
            "invalid_config",
            "review output token limit must be positive",
        ));
    }
    let initial = policy.evaluate_with_arguments(
        input.mode,
        input.permissions,
        input.operation,
        &input.arguments,
    );
    if initial.decision != Decision::ModelReview {
        return Ok((
            ReviewOutcome {
                decision: initial.decision,
                reason: initial.reason.into(),
                source: ReviewDecisionSource::Rule,
            },
            Vec::new(),
        ));
    }
    let normalized = input.operation.review_arguments(input.arguments)?;
    // If an operation contains a known secret, do not ask the model to approve
    // an incomplete redacted view; fall back to a human decision.
    let encoded = serde_json::to_string(&normalized)
        .map_err(|_| failure("invalid_args", "cannot encode review arguments"))?;
    if crate::redaction::contains_secret(&encoded, &config.known_secrets) {
        return Ok((
            ReviewOutcome {
                decision: Decision::Ask,
                reason: "review_sensitive_data".into(),
                source: ReviewDecisionSource::HumanFallback,
            },
            Vec::new(),
        ));
    }
    let mut data = json!({"tool": input.operation.tool_name(), "arguments": normalized, "work_mode": input.mode, "permissions": input.permissions, "goal_summary": input.goal_summary});
    if crate::redaction::redact(&mut data, &config.known_secrets).is_err() {
        return Ok((
            ReviewOutcome {
                decision: Decision::Ask,
                reason: "review_sensitive_data".into(),
                source: ReviewDecisionSource::HumanFallback,
            },
            Vec::new(),
        ));
    }
    let instructions = crate::prompt::review_instructions();
    let payload_history = vec![HistoryItem::UserText(data.to_string())];
    let routing = model.model_routing().filter(|r| r.is_enabled());
    let mut route_decisions = Vec::new();

    if let Some(coordinator) = routing {
        let probe = ModelRequest {
            request_id: "probe".into(),
            purpose: ModelPurpose::Review,
            profile: String::new(),
            instructions: instructions.into(),
            dynamic_instructions: None,
            max_output_tokens: config.max_output_tokens,
            history: payload_history.clone(),
            media: Default::default(),
            tools: vec![],
        };
        let estimated_input_tokens = crate::context_builder::conservative_request_tokens(&probe)?;
        let reqs = RouteRequirements {
            role: ModelRole::Review,
            purpose: ModelPurpose::Review,
            work_mode: input.mode,
            session_profile: config.execution_profile.clone(),
            review_profile: config.review_profile.clone(),
            tools_required: false,
            required_media: Default::default(),
            estimated_input_tokens,
            extra_safety_tokens: u64::from(config.max_output_tokens.unwrap_or(512)),
            task_type: Some("review".into()),
            prefer_reasoning: false,
        };
        let mut attempted = BTreeSet::new();
        let mut fallback_from: Option<String> = None;
        let mut same_profile_attempts = 0usize;

        for _step in 0..8usize {
            if cancel.is_cancelled() {
                return Err(failure("cancelled", "review cancelled"));
            }
            let spent = metrics.map(Metrics::spent_by_currency).unwrap_or_default();
            let plan = coordinator.plan(&reqs, &[], &spent);
            let cand = plan
                .eligible
                .iter()
                .find(|c| !attempted.contains(&c.profile))
                .or_else(|| {
                    if same_profile_attempts < 2 {
                        plan.eligible.first()
                    } else {
                        None
                    }
                });
            let Some(cand) = cand.cloned() else {
                return Ok((
                    ReviewOutcome {
                        decision: Decision::Ask,
                        reason: "review_unavailable".into(),
                        source: ReviewDecisionSource::HumanFallback,
                    },
                    route_decisions,
                ));
            };
            attempted.insert(cand.profile.clone());
            let request_id = uuid::Uuid::now_v7().to_string();
            let decision_record = RouteDecisionRecord {
                request_id: request_id.clone(),
                purpose: ModelPurpose::Review,
                role: ModelRole::Review.as_str().to_owned(),
                profile: cand.profile.clone(),
                model: cand.model.clone(),
                protocol: cand.protocol,
                reason: if fallback_from.is_some() {
                    "failover".into()
                } else {
                    cand.source.to_owned()
                },
                fallback_from: fallback_from.clone(),
                excluded: plan.excluded.clone(),
                migration: None,
            };
            if let Some(m) = metrics {
                m.record_route(decision_record.clone());
            }
            route_decisions.push(decision_record);

            budget.consume()?;
            let request = ModelRequest {
                request_id,
                purpose: ModelPurpose::Review,
                profile: cand.profile.clone(),
                instructions: instructions.into(),
                dynamic_instructions: None,
                max_output_tokens: config.max_output_tokens,
                history: payload_history.clone(),
                media: Default::default(),
                tools: vec![],
            };
            let started = Instant::now();
            let mut received = false;
            let result = run_review_attempt(
                model,
                &request,
                cancel,
                budget,
                config,
                started,
                &mut received,
            )
            .await;
            match result {
                Ok(parsed) => {
                    coordinator.health().record_success(&cand.profile);
                    return Ok((interpret_review_response(parsed, config), route_decisions));
                }
                Err(error) => {
                    if matches!(
                        error.0.code.as_str(),
                        "cancelled" | "limit_reached" | "budget_error"
                    ) {
                        return Err(error);
                    }
                    let retryable = error.is_transient_model_error();
                    if retryable {
                        coordinator.health().record_transient_failure(
                            &cand.profile,
                            error.code(),
                            error.retry_after(),
                            coordinator.settings(),
                        );
                    }
                    if !retryable || received {
                        return Ok((
                            ReviewOutcome {
                                decision: Decision::Ask,
                                reason: "review_unavailable".into(),
                                source: ReviewDecisionSource::HumanFallback,
                            },
                            route_decisions,
                        ));
                    }
                    fallback_from = Some(cand.profile.clone());
                    let refreshed = coordinator.plan(&reqs, &[], &spent);
                    let has_other = refreshed
                        .eligible
                        .iter()
                        .any(|c| !attempted.contains(&c.profile));
                    if !has_other {
                        if same_profile_attempts >= 2 || refreshed.eligible.is_empty() {
                            return Ok((
                                ReviewOutcome {
                                    decision: Decision::Ask,
                                    reason: "review_unavailable".into(),
                                    source: ReviewDecisionSource::HumanFallback,
                                },
                                route_decisions,
                            ));
                        }
                        let delay = (Duration::from_secs(1 << same_profile_attempts)
                            + jitter(config.retry_jitter_max))
                        .max(error.1.unwrap_or_default().min(Duration::from_secs(30)));
                        same_profile_attempts += 1;
                        bounded(
                            async {
                                sleep(delay).await;
                                Ok(())
                            },
                            cancel,
                            budget,
                            budget.remaining()?.saturating_add(Duration::from_secs(1)),
                        )
                        .await?;
                    }
                }
            }
        }
        return Ok((
            ReviewOutcome {
                decision: Decision::Ask,
                reason: "review_unavailable".into(),
                source: ReviewDecisionSource::HumanFallback,
            },
            route_decisions,
        ));
    }

    let profile = config
        .review_profile
        .as_ref()
        .unwrap_or(&config.execution_profile);
    for attempt in 0..=2usize {
        if cancel.is_cancelled() {
            return Err(failure("cancelled", "review cancelled"));
        }
        budget.consume()?;
        let request = ModelRequest {
            request_id: uuid::Uuid::now_v7().to_string(),
            purpose: ModelPurpose::Review,
            profile: profile.clone(),
            instructions: instructions.into(),
            dynamic_instructions: None,
            max_output_tokens: config.max_output_tokens,
            history: payload_history.clone(),
            media: Default::default(),
            tools: vec![],
        };
        let started = Instant::now();
        let mut received = false;
        let result = run_review_attempt(
            model,
            &request,
            cancel,
            budget,
            config,
            started,
            &mut received,
        )
        .await;
        match result {
            Ok(parsed) => return Ok((interpret_review_response(parsed, config), route_decisions)),
            Err(error) => {
                if matches!(
                    error.0.code.as_str(),
                    "cancelled" | "limit_reached" | "budget_error"
                ) {
                    return Err(error);
                }
                let retryable = matches!(
                    error.0.code.as_str(),
                    "transient_connection"
                        | "rate_limited"
                        | "server_error"
                        | "model_timeout"
                        | "model_connect_timeout"
                );
                if !retryable || received || attempt == 2 {
                    return Ok((
                        ReviewOutcome {
                            decision: Decision::Ask,
                            reason: "review_unavailable".into(),
                            source: ReviewDecisionSource::HumanFallback,
                        },
                        route_decisions,
                    ));
                }
                let delay = (Duration::from_secs(1 << attempt) + jitter(config.retry_jitter_max))
                    .max(error.1.unwrap_or_default().min(Duration::from_secs(30)));
                bounded(
                    async {
                        sleep(delay).await;
                        Ok(())
                    },
                    cancel,
                    budget,
                    budget.remaining()?.saturating_add(Duration::from_secs(1)),
                )
                .await?;
            }
        }
    }
    Ok((
        ReviewOutcome {
            decision: Decision::Ask,
            reason: "review_unavailable".into(),
            source: ReviewDecisionSource::HumanFallback,
        },
        route_decisions,
    ))
}

async fn run_review_attempt(
    model: &dyn Model,
    request: &ModelRequest,
    cancel: &dyn Cancellation,
    budget: &SharedBudget,
    config: &ReviewConfig,
    started: Instant,
    received: &mut bool,
) -> Result<Option<String>, BoundaryError> {
    let mut stream = bounded(
        model.start(request, cancel),
        cancel,
        budget,
        if config.total_timeout.is_zero() {
            config.connect_timeout
        } else {
            config.connect_timeout.min(config.total_timeout)
        },
    )
    .await?;
    loop {
        let remaining = if config.total_timeout.is_zero() {
            config.idle_timeout
        } else {
            config
                .idle_timeout
                .min(config.total_timeout.saturating_sub(started.elapsed()))
        };
        match bounded(
            stream.next(cancel),
            cancel,
            budget,
            config.idle_timeout.min(remaining),
        )
        .await?
        {
            Some(ModelUpdate::TextDelta(_)) => {
                *received = true;
            }
            Some(ModelUpdate::Progress | ModelUpdate::ThinkingDelta { .. }) => *received = true,
            Some(ModelUpdate::Finished { items, .. }) => {
                let mut text = String::new();
                for item in items {
                    match item {
                        HistoryItem::AssistantText(s) => {
                            if text.len().saturating_add(s.len()) > 64 * 1024 {
                                return Ok(None);
                            }
                            text.push_str(&s);
                        }
                        HistoryItem::ProviderItem { .. } => {}
                        _ => return Ok(None),
                    }
                }
                return Ok(Some(text));
            }
            Some(ModelUpdate::OutputLimitTruncated { .. }) | None => return Ok(None),
        }
    }
}

fn interpret_review_response(parsed: Option<String>, config: &ReviewConfig) -> ReviewOutcome {
    let Some(text) = parsed else {
        return ReviewOutcome {
            decision: Decision::Ask,
            reason: "review_invalid".into(),
            source: ReviewDecisionSource::HumanFallback,
        };
    };
    let candidate = extract_fenced_json(&text);
    if let Ok(parsed) = serde_json::from_str::<ModelDecision>(candidate)
        && !parsed.reason.trim().is_empty()
        && parsed.reason.chars().count() <= 512
    {
        let mut reason = Value::String(parsed.reason);
        if crate::redaction::redact(&mut reason, &config.known_secrets).is_err() {
            return ReviewOutcome {
                decision: Decision::Ask,
                reason: "review_sensitive_data".into(),
                source: ReviewDecisionSource::HumanFallback,
            };
        }
        let reason = reason.as_str().unwrap_or_default().to_owned();
        let decision = match parsed.decision {
            ModelChoice::Allow => Decision::Allow,
            ModelChoice::Ask => Decision::Ask,
            ModelChoice::Deny => Decision::Deny,
        };
        return ReviewOutcome {
            decision,
            reason,
            source: ReviewDecisionSource::Model,
        };
    }
    ReviewOutcome {
        decision: Decision::Ask,
        reason: "review_invalid".into(),
        source: ReviewDecisionSource::HumanFallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn child_budget_chains_requests_and_clamps_remaining_time() {
        let parent = SharedBudget::new(2, Duration::from_secs(10));
        parent.start_run().unwrap();
        tokio::time::advance(Duration::from_secs(3)).await;

        let child_a = SharedBudget::child(&parent, 5, Duration::from_secs(20));
        child_a.start_run().unwrap();
        assert_eq!(child_a.remaining().unwrap(), Duration::from_secs(7));

        let child_b = SharedBudget::child(&parent, 5, Duration::from_secs(4));
        child_b.start_run().unwrap();
        assert_eq!(child_b.remaining().unwrap(), Duration::from_secs(4));

        child_a.consume().unwrap();
        assert_eq!(child_a.used().unwrap(), 1);
        assert_eq!(parent.used().unwrap(), 1);

        child_b.consume().unwrap();
        assert_eq!(child_b.used().unwrap(), 1);
        assert_eq!(parent.used().unwrap(), 2);

        // Parent exhausted: child_a must fail and NOT increment its local count.
        let err = child_a.consume().unwrap_err();
        assert_eq!(err.0.code, "limit_reached");
        assert_eq!(child_a.used().unwrap(), 1);
        assert_eq!(parent.used().unwrap(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn child_pause_is_refcounted_and_allows_sibling_requests() {
        let parent = SharedBudget::new(4, Duration::from_secs(10));
        parent.start_run().unwrap();
        let child_a = SharedBudget::child(&parent, 4, Duration::from_secs(10));
        let child_b = SharedBudget::child(&parent, 4, Duration::from_secs(100));

        tokio::time::advance(Duration::from_secs(2)).await;
        // child_a awaits approval -> self-paused, parent clock paused via refcount
        child_a.pause().unwrap();
        assert_eq!(child_a.consume().unwrap_err().0.code, "budget_error");

        // Another child starts waiting too -> parent pause depth becomes 2
        parent.pause_child().unwrap();
        tokio::time::advance(Duration::from_secs(50)).await;

        // Sibling child_b can still consume while parent clock is paused by child waits
        child_b.consume().unwrap();
        assert_eq!(parent.remaining().unwrap(), Duration::from_secs(8));

        // First release keeps parent clock paused because depth is still 1
        child_a.resume().unwrap();
        tokio::time::advance(Duration::from_secs(20)).await;
        assert_eq!(parent.remaining().unwrap(), Duration::from_secs(8));

        // Final release unpauses parent clock
        parent.resume_child().unwrap();
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(parent.remaining().unwrap(), Duration::from_secs(5));
    }
}
