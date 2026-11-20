//! Per-run model and tool observations. Missing provider usage stays unknown.
use crate::{
    boundary::{
        Cancellation, Model, ModelRequest, ModelStream, ModelUpdate, Operation, StoreChange,
    },
    config::Pricing,
    contract::{Artifact, BoundaryError, ModelProtocol, ModelPurpose, Session, ToolResult, Usage},
    provider::routing::RouteDecisionRecord,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Instant,
};
use tracing::Instrument;

/// Per-profile request, token, and cost attribution within one Run (`F-16`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileAttribution {
    pub model: String,
    pub protocol: Option<ModelProtocol>,
    pub execution_requests: u64,
    pub review_requests: u64,
    pub compaction_requests: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostEstimate>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunMetrics {
    pub execution_requests: u64,
    pub review_requests: u64,
    #[serde(default)]
    pub compaction_requests: u64,
    pub tool_calls: u64,
    pub tool_executions: u64,
    pub duration_ms: u64,
    pub execution_usage: Option<Usage>,
    pub review_usage: Option<Usage>,
    #[serde(default)]
    pub compaction_usage: Option<Usage>,
    pub execution_cost: Option<CostEstimate>,
    pub review_cost: Option<CostEstimate>,
    #[serde(default)]
    pub compaction_cost: Option<CostEstimate>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_profile: BTreeMap<String, ProfileAttribution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route_decisions: Vec<RouteDecisionRecord>,
}
/// An estimate in millionths of the explicitly configured currency; it is not a bill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CostEstimate {
    pub currency: String,
    pub microunits: u64,
    pub rounded_up: bool,
    pub estimated: bool,
}
#[derive(Default)]
struct Totals {
    requests: u64,
    completed: u64,
    input: u64,
    output: u64,
    cached_input: u64,
    has_cached_input: bool,
    input_unknown: bool,
    output_unknown: bool,
    cached_input_unknown: bool,
    estimated: bool,
}
impl Totals {
    fn finish(&mut self, usage: Option<Usage>) {
        self.completed = self.completed.saturating_add(1);
        if let Some(usage) = usage {
            self.estimated |= usage.estimated;
            match usage.input_tokens {
                Some(n) => match self.input.checked_add(n) {
                    Some(sum) => self.input = sum,
                    None => self.input_unknown = true,
                },
                None => self.input_unknown = true,
            }
            match usage.output_tokens {
                Some(n) => match self.output.checked_add(n) {
                    Some(sum) => self.output = sum,
                    None => self.output_unknown = true,
                },
                None => self.output_unknown = true,
            }
            match usage.cached_input_tokens {
                Some(n) => {
                    self.has_cached_input = true;
                    match self.cached_input.checked_add(n) {
                        Some(sum) => self.cached_input = sum,
                        None => self.cached_input_unknown = true,
                    }
                }
                None => self.cached_input_unknown = true,
            }
        } else {
            self.input_unknown = true;
            self.output_unknown = true;
            self.cached_input_unknown = true;
        }
    }
    fn merge(&mut self, requests: u64, usage: Option<&Usage>) {
        if requests == 0 && usage.is_none() {
            return;
        }
        self.requests = self.requests.saturating_add(requests);
        let Some(usage) = usage else {
            self.input_unknown = true;
            self.output_unknown = true;
            self.cached_input_unknown = true;
            self.estimated = true;
            return;
        };
        self.completed = self.completed.saturating_add(requests);
        self.estimated |= usage.estimated;
        match usage.input_tokens {
            Some(n) => match self.input.checked_add(n) {
                Some(sum) => self.input = sum,
                None => self.input_unknown = true,
            },
            None => self.input_unknown = true,
        }
        match usage.output_tokens {
            Some(n) => match self.output.checked_add(n) {
                Some(sum) => self.output = sum,
                None => self.output_unknown = true,
            },
            None => self.output_unknown = true,
        }
        match usage.cached_input_tokens {
            Some(n) => {
                self.has_cached_input = true;
                match self.cached_input.checked_add(n) {
                    Some(sum) => self.cached_input = sum,
                    None => self.cached_input_unknown = true,
                }
            }
            None => self.cached_input_unknown = true,
        }
    }
    fn usage(&self) -> Option<Usage> {
        if self.requests == 0 {
            return None;
        }
        let incomplete = self.completed != self.requests;
        Some(Usage {
            input_tokens: if self.input_unknown || incomplete {
                None
            } else {
                Some(self.input)
            },
            output_tokens: if self.output_unknown || incomplete {
                None
            } else {
                Some(self.output)
            },
            cached_input_tokens: if !self.has_cached_input
                || self.cached_input_unknown
                || incomplete
            {
                None
            } else {
                Some(self.cached_input)
            },
            estimated: self.estimated || self.input_unknown || self.output_unknown || incomplete,
        })
    }
    /// Lower-bound known tokens from completed requests even if a sibling attempt failed.
    fn known_usage(&self) -> Option<Usage> {
        if self.completed == 0 {
            return None;
        }
        Some(Usage {
            input_tokens: Some(self.input),
            output_tokens: Some(self.output),
            cached_input_tokens: self.has_cached_input.then_some(self.cached_input),
            estimated: self.estimated || self.completed != self.requests,
        })
    }
}
#[derive(Clone)]
pub struct ProfilePricingMeta {
    pub model: String,
    pub protocol: ModelProtocol,
    pub pricing: Option<Pricing>,
}

#[derive(Default)]
struct ProfileState {
    model: String,
    protocol: Option<ModelProtocol>,
    execution: Totals,
    review: Totals,
    compaction: Totals,
    pricing: Option<Pricing>,
}
impl ProfileState {
    fn totals_mut(&mut self, purpose: ModelPurpose) -> &mut Totals {
        match purpose {
            ModelPurpose::Execution => &mut self.execution,
            ModelPurpose::Review => &mut self.review,
            ModelPurpose::Compaction => &mut self.compaction,
        }
    }
    fn combined_usage(&self) -> Option<Usage> {
        let e = self.execution.usage();
        let r = self.review.usage();
        let c = self.compaction.usage();
        combine_usages([&e, &r, &c].into_iter().flatten())
    }
    fn combined_known_usage(&self) -> Option<Usage> {
        let e = self.execution.known_usage();
        let r = self.review.known_usage();
        let c = self.compaction.known_usage();
        combine_usages([&e, &r, &c].into_iter().flatten())
    }
}
fn combine_usages<'a>(items: impl Iterator<Item = &'a Usage>) -> Option<Usage> {
    let usages: Vec<&Usage> = items.collect();
    if usages.is_empty() {
        return None;
    }
    let sum = |field: fn(&Usage) -> Option<u64>| {
        usages
            .iter()
            .try_fold(0u64, |total, usage| total.checked_add(field(usage)?))
    };
    let input_tokens = sum(|u| u.input_tokens);
    let output_tokens = sum(|u| u.output_tokens);
    let cached_input_tokens = sum(|u| u.cached_input_tokens);
    Some(Usage {
        input_tokens,
        output_tokens,
        cached_input_tokens,
        estimated: usages.iter().any(|u| u.estimated)
            || input_tokens.is_none()
            || output_tokens.is_none(),
    })
}

#[derive(Default)]
struct State {
    started: Option<Instant>,
    execution: Totals,
    review: Totals,
    compaction: Totals,
    tools: u64,
    executions: u64,
    calls: HashMap<String, String>,
    artifacts: Vec<Artifact>,
    execution_pricing: Option<Pricing>,
    review_pricing: Option<Pricing>,
    profile_catalog: BTreeMap<String, ProfilePricingMeta>,
    by_profile: BTreeMap<String, ProfileState>,
    route_decisions: Vec<RouteDecisionRecord>,
}
#[derive(Default)]
pub(crate) struct Metrics {
    state: Mutex<State>,
}
impl Metrics {
    pub fn pricing(&self, execution: Option<Pricing>, review: Option<Pricing>) {
        if let Ok(mut state) = self.state.lock() {
            state.execution_pricing = execution;
            state.review_pricing = review;
        }
    }
    pub fn register_profiles(&self, profiles: BTreeMap<String, ProfilePricingMeta>) {
        if let Ok(mut state) = self.state.lock() {
            state.profile_catalog = profiles;
        }
    }
    pub fn record_route(&self, decision: RouteDecisionRecord) {
        if let Ok(mut state) = self.state.lock()
            && state.route_decisions.len() < 512
        {
            state.route_decisions.push(decision);
        }
    }
    pub fn spent_by_currency(&self) -> BTreeMap<String, u64> {
        let mut out: BTreeMap<String, u64> = BTreeMap::new();
        let Ok(state) = self.state.lock() else {
            return out;
        };
        if !state.by_profile.is_empty() {
            for entry in state.by_profile.values() {
                if let Some(cost) = estimate(
                    entry.pricing.as_ref(),
                    entry.combined_known_usage().as_ref(),
                ) {
                    let slot = out.entry(cost.currency).or_insert(0);
                    *slot = slot.saturating_add(cost.microunits);
                }
            }
            return out;
        }
        if let Some(cost) = estimate(
            state.execution_pricing.as_ref(),
            state.execution.known_usage().as_ref(),
        ) {
            let slot = out.entry(cost.currency).or_insert(0);
            *slot = slot.saturating_add(cost.microunits);
        }
        if let Some(cost) = estimate(
            state.execution_pricing.as_ref(),
            state.compaction.known_usage().as_ref(),
        ) {
            let slot = out.entry(cost.currency).or_insert(0);
            *slot = slot.saturating_add(cost.microunits);
        }
        if let Some(cost) = estimate(
            state.review_pricing.as_ref(),
            state.review.known_usage().as_ref(),
        ) {
            let slot = out.entry(cost.currency).or_insert(0);
            *slot = slot.saturating_add(cost.microunits);
        }
        out
    }
    pub fn start(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.started = Some(Instant::now());
        }
    }
    pub fn tool_call(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.tools = state.tools.saturating_add(1);
        }
    }
    /// Roll a finished child subagent's model requests, token usage, tool counts
    /// and merged workspace artifacts into the parent run's metrics once.
    pub fn merge_child(&self, child: &RunMetrics, artifacts: &[Artifact]) {
        if let Ok(mut state) = self.state.lock() {
            state
                .execution
                .merge(child.execution_requests, child.execution_usage.as_ref());
            state
                .review
                .merge(child.review_requests, child.review_usage.as_ref());
            state
                .compaction
                .merge(child.compaction_requests, child.compaction_usage.as_ref());
            state.tools = state.tools.saturating_add(child.tool_calls);
            state.executions = state.executions.saturating_add(child.tool_executions);
            state.artifacts.extend(artifacts.iter().cloned());
            for (name, attr) in &child.by_profile {
                let fallback_pricing = state
                    .profile_catalog
                    .get(name)
                    .and_then(|m| m.pricing.clone());
                let entry = state.by_profile.entry(name.clone()).or_default();
                if entry.model.is_empty() {
                    entry.model = attr.model.clone();
                }
                entry.protocol = entry.protocol.or(attr.protocol);
                if entry.pricing.is_none() {
                    entry.pricing = fallback_pricing;
                }
                if attr.execution_requests > 0 {
                    entry
                        .execution
                        .merge(attr.execution_requests, attr.usage.as_ref());
                    entry.review.merge(attr.review_requests, None);
                    entry.compaction.merge(attr.compaction_requests, None);
                } else if attr.review_requests > 0 {
                    entry
                        .review
                        .merge(attr.review_requests, attr.usage.as_ref());
                    entry.compaction.merge(attr.compaction_requests, None);
                } else {
                    entry
                        .compaction
                        .merge(attr.compaction_requests, attr.usage.as_ref());
                }
            }
            for rec in &child.route_decisions {
                if state.route_decisions.len() >= 512 {
                    break;
                }
                state.route_decisions.push(rec.clone());
            }
        }
    }
    fn request_for_profile(&self, purpose: ModelPurpose, profile: &str) {
        if let Ok(mut state) = self.state.lock() {
            let totals = match purpose {
                ModelPurpose::Execution => &mut state.execution,
                ModelPurpose::Review => &mut state.review,
                ModelPurpose::Compaction => &mut state.compaction,
            };
            totals.requests = totals.requests.saturating_add(1);
            if !profile.is_empty() {
                let meta = state.profile_catalog.get(profile).cloned();
                let fallback_pricing = match purpose {
                    ModelPurpose::Review => state.review_pricing.clone(),
                    ModelPurpose::Execution | ModelPurpose::Compaction => {
                        state.execution_pricing.clone()
                    }
                };
                let entry = state.by_profile.entry(profile.to_owned()).or_default();
                if let Some(meta) = meta {
                    if entry.model.is_empty() {
                        entry.model = meta.model;
                    }
                    entry.protocol.get_or_insert(meta.protocol);
                    if entry.pricing.is_none() {
                        entry.pricing = meta.pricing.or(fallback_pricing);
                    }
                } else if entry.pricing.is_none() {
                    entry.pricing = fallback_pricing;
                }
                entry.totals_mut(purpose).requests =
                    entry.totals_mut(purpose).requests.saturating_add(1);
            }
        }
    }
    fn finish_for_profile(&self, purpose: ModelPurpose, profile: &str, usage: Option<Usage>) {
        if let Ok(mut state) = self.state.lock() {
            match purpose {
                ModelPurpose::Execution => &mut state.execution,
                ModelPurpose::Review => &mut state.review,
                ModelPurpose::Compaction => &mut state.compaction,
            }
            .finish(usage.clone());
            if !profile.is_empty()
                && let Some(entry) = state.by_profile.get_mut(profile)
            {
                entry.totals_mut(purpose).finish(usage);
            }
        }
    }
    pub fn observe(&self, changes: &[StoreChange]) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        for change in changes {
            match change {
                StoreChange::ToolIntent { call, .. } => {
                    state.calls.insert(call.call_id.clone(), call.name.clone());
                    if !changes.iter().any(|c|matches!(c,StoreChange::ToolFinished {call_id,..} if call_id==&call.call_id)) {state.executions=state.executions.saturating_add(1);}
                }
                StoreChange::ToolFinished {
                    call_id, result, ..
                } if result.ok
                    && state
                        .calls
                        .get(call_id)
                        .is_some_and(|name| crate::registry::is_local_write_tool(name))
                    && let Some(artifact) = artifact(result) =>
                {
                    state.artifacts.push(artifact);
                }
                _ => {}
            }
        }
    }
    pub fn snapshot(&self) -> Option<(RunMetrics, Option<Usage>, Vec<Artifact>)> {
        let state = self.state.lock().ok()?;
        let duration = state
            .started
            .map(|start| start.elapsed().as_millis())
            .unwrap_or(0);
        let execution = state.execution.usage();
        let review = state.review.usage();
        let compaction = state.compaction.usage();
        let total = combine_usages([&execution, &review, &compaction].into_iter().flatten());
        let by_profile: BTreeMap<String, ProfileAttribution> = state
            .by_profile
            .iter()
            .map(|(name, entry)| {
                let usage = entry.combined_usage();
                let cost = estimate(entry.pricing.as_ref(), usage.as_ref());
                (
                    name.clone(),
                    ProfileAttribution {
                        model: entry.model.clone(),
                        protocol: entry.protocol,
                        execution_requests: entry.execution.requests,
                        review_requests: entry.review.requests,
                        compaction_requests: entry.compaction.requests,
                        usage,
                        cost,
                    },
                )
            })
            .collect();
        let purpose_cost = |select: fn(&ProfileState) -> &Totals,
                            fallback: Option<&Pricing>,
                            usage: Option<&Usage>| {
            if state.by_profile.is_empty() {
                return estimate(fallback, usage);
            }
            let items: Vec<Option<CostEstimate>> = state
                .by_profile
                .values()
                .filter(|entry| select(entry).requests > 0)
                .map(|entry| {
                    let pricing = entry.pricing.as_ref().or(fallback);
                    estimate(pricing, select(entry).usage().as_ref())
                })
                .collect();
            if items.is_empty() {
                return estimate(fallback, usage);
            }
            sum_costs(items.into_iter())
        };
        let execution_cost = purpose_cost(
            |entry| &entry.execution,
            state.execution_pricing.as_ref(),
            execution.as_ref(),
        );
        let review_cost = purpose_cost(
            |entry| &entry.review,
            state.review_pricing.as_ref(),
            review.as_ref(),
        );
        let compaction_cost = purpose_cost(
            |entry| &entry.compaction,
            state.execution_pricing.as_ref(),
            compaction.as_ref(),
        );
        Some((
            RunMetrics {
                execution_requests: state.execution.requests,
                review_requests: state.review.requests,
                compaction_requests: state.compaction.requests,
                tool_calls: state.tools,
                tool_executions: state.executions,
                duration_ms: u64::try_from(duration).unwrap_or(u64::MAX),
                execution_usage: execution,
                review_usage: review,
                compaction_usage: compaction,
                compaction_cost,
                execution_cost,
                review_cost,
                by_profile,
                route_decisions: state.route_decisions.clone(),
            },
            total,
            state.artifacts.clone(),
        ))
    }
}
fn sum_costs(items: impl Iterator<Item = Option<CostEstimate>>) -> Option<CostEstimate> {
    let mut currency: Option<String> = None;
    let mut microunits = 0u64;
    let mut rounded = false;
    let mut count = 0usize;
    for item in items {
        let cost = item?;
        if let Some(existing) = &currency {
            if existing != &cost.currency {
                return None;
            }
        } else {
            currency = Some(cost.currency.clone());
        }
        microunits = microunits.checked_add(cost.microunits)?;
        rounded |= cost.rounded_up;
        count += 1;
    }
    (count > 0).then(|| CostEstimate {
        currency: currency.unwrap_or_default(),
        microunits,
        rounded_up: rounded,
        estimated: true,
    })
}
fn estimate(pricing: Option<&Pricing>, usage: Option<&Usage>) -> Option<CostEstimate> {
    let pricing = pricing?;
    let usage = usage?;
    let input = u128::from(usage.input_tokens?)
        .checked_mul(u128::from(pricing.input_microunits_per_million))?;
    let output = u128::from(usage.output_tokens?)
        .checked_mul(u128::from(pricing.output_microunits_per_million))?;
    let numerator = input.checked_add(output)?;
    let rounded_up = numerator % 1_000_000 != 0;
    let amount = (numerator / 1_000_000).checked_add(u128::from(rounded_up))?;
    Some(CostEstimate {
        currency: pricing.currency.clone(),
        microunits: u64::try_from(amount).ok()?,
        rounded_up,
        estimated: true,
    })
}
fn artifact(result: &ToolResult) -> Option<Artifact> {
    let data = result.data.as_ref()?;
    let path = data.get("path")?.as_str()?;
    let created = data.get("created")?.as_bool()?;
    Some(Artifact {
        path: path.into(),
        operation: if created { "created" } else { "modified" }.into(),
    })
}

pub(crate) struct MeasuredModel {
    pub inner: Arc<dyn Model>,
    pub metrics: Arc<Metrics>,
}
struct Completion {
    metrics: Arc<Metrics>,
    purpose: ModelPurpose,
    profile: String,
    done: bool,
    span: tracing::Span,
    started: Instant,
    status: &'static str,
    error_code: Option<String>,
}
impl Drop for Completion {
    fn drop(&mut self) {
        if !self.done {
            self.metrics
                .finish_for_profile(self.purpose, &self.profile, None);
        }
        self.span.in_scope(|| {
            tracing::info!(target: "soyo::model", event = "model.finished", status = self.status,
                error_code = self.error_code.as_deref().unwrap_or(""), duration_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
        });
    }
}
struct MeasuredStream {
    inner: Box<dyn ModelStream>,
    completion: Completion,
}
impl Model for MeasuredModel {
    fn check_start(&self, request: &ModelRequest) -> Result<(), BoundaryError> {
        self.inner.check_start(request)
    }
    fn request_counter(&self) -> Option<&dyn crate::context_builder::RequestCounter> {
        self.inner.request_counter()
    }

    fn validate_session(&self, session: &Session) -> Result<(), BoundaryError> {
        self.inner.validate_session(session)
    }
    fn model_routing(&self) -> Option<&crate::provider::routing::RoutingCoordinator> {
        self.inner.model_routing()
    }
    fn start<'a>(
        &'a self,
        request: &'a ModelRequest,
        cancel: &'a dyn Cancellation,
    ) -> Operation<'a, Box<dyn ModelStream>> {
        Box::pin(async move {
            self.metrics
                .request_for_profile(request.purpose, &request.profile);
            let purpose = match request.purpose {
                ModelPurpose::Execution => "execution",
                ModelPurpose::Review => "review",
                ModelPurpose::Compaction => "compaction",
            };
            let span = tracing::info_span!(target: "soyo::model", "model", request_id = request.request_id.as_str(), profile = request.profile.as_str(), purpose);
            span.in_scope(|| tracing::info!(target: "soyo::model", event = "model.started"));
            let mut completion = Completion {
                metrics: self.metrics.clone(),
                purpose: request.purpose,
                profile: request.profile.clone(),
                done: false,
                span: span.clone(),
                started: Instant::now(),
                status: "abandoned",
                error_code: None,
            };
            let inner = self
                .inner
                .start(request, cancel)
                .instrument(span)
                .await
                .inspect_err(|error| {
                    completion.status = "failed";
                    completion.error_code = Some(error.0.code.clone());
                })?;
            Ok(Box::new(MeasuredStream { inner, completion }) as Box<dyn ModelStream>)
        })
    }
}
impl ModelStream for MeasuredStream {
    fn next<'a>(&'a mut self, cancel: &'a dyn Cancellation) -> Operation<'a, Option<ModelUpdate>> {
        Box::pin(async move {
            let next = self
                .inner
                .next(cancel)
                .instrument(self.completion.span.clone())
                .await
                .inspect_err(|error| {
                    self.completion.status = "failed";
                    self.completion.error_code = Some(error.0.code.clone());
                })?;
            if let Some(
                ModelUpdate::Finished { usage, .. }
                | ModelUpdate::OutputLimitTruncated { usage, .. },
            ) = &next
                && !self.completion.done
            {
                self.completion.metrics.finish_for_profile(
                    self.completion.purpose,
                    &self.completion.profile,
                    usage.clone(),
                );
                self.completion.done = true;
                self.completion.status = "completed";
            }
            Ok(next)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pricing_uses_explicit_rates_and_never_guesses_missing_usage() {
        let pricing = Pricing {
            currency: "USD".into(),
            input_microunits_per_million: 1,
            output_microunits_per_million: 1,
        };
        let usage = Usage {
            input_tokens: Some(1),
            output_tokens: Some(1),
            cached_input_tokens: None,
            estimated: false,
        };
        let cost = estimate(Some(&pricing), Some(&usage)).unwrap();
        assert_eq!(cost.microunits, 1);
        assert!(cost.rounded_up);
        assert!(cost.estimated);
        assert!(estimate(None, Some(&usage)).is_none());
        assert!(
            estimate(
                Some(&pricing),
                Some(&Usage {
                    input_tokens: None,
                    ..usage.clone()
                })
            )
            .is_none()
        );
        let large = Pricing {
            input_microunits_per_million: u64::MAX,
            output_microunits_per_million: u64::MAX,
            ..pricing
        };
        assert!(
            estimate(
                Some(&large),
                Some(&Usage {
                    input_tokens: Some(u64::MAX),
                    output_tokens: Some(u64::MAX),
                    cached_input_tokens: None,
                    estimated: false
                })
            )
            .is_none()
        );
    }

    #[test]
    fn cached_usage_aggregates_only_when_every_completed_request_reports_it() {
        let mut totals = Totals {
            requests: 2,
            ..Totals::default()
        };
        totals.finish(Some(Usage {
            input_tokens: Some(100),
            output_tokens: Some(10),
            cached_input_tokens: Some(40),
            estimated: false,
        }));
        totals.finish(Some(Usage {
            input_tokens: Some(200),
            output_tokens: Some(20),
            cached_input_tokens: Some(150),
            estimated: false,
        }));
        let usage = totals.usage().unwrap();
        assert_eq!(usage.input_tokens, Some(300));
        assert_eq!(usage.output_tokens, Some(30));
        assert_eq!(usage.cached_input_tokens, Some(190));
        assert!(!usage.estimated);

        let mut incomplete = Totals {
            requests: 1,
            ..Totals::default()
        };
        incomplete.finish(Some(Usage {
            input_tokens: Some(100),
            output_tokens: Some(10),
            cached_input_tokens: None,
            estimated: false,
        }));
        let usage = incomplete.usage().unwrap();
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.cached_input_tokens, None);
        assert!(!usage.estimated);
    }

    #[test]
    fn merge_child_aggregates_usage_and_propagates_unknowns() {
        let parent = Metrics::default();
        parent.request_for_profile(ModelPurpose::Execution, "");
        parent.finish_for_profile(
            ModelPurpose::Execution,
            "",
            Some(Usage {
                input_tokens: Some(100),
                output_tokens: Some(25),
                cached_input_tokens: Some(10),
                estimated: false,
            }),
        );
        parent.tool_call();

        let child = RunMetrics {
            execution_requests: 2,
            review_requests: 1,
            compaction_requests: 0,
            tool_calls: 3,
            tool_executions: 2,
            duration_ms: 50,
            execution_usage: Some(Usage {
                input_tokens: Some(200),
                output_tokens: Some(40),
                cached_input_tokens: Some(30),
                estimated: false,
            }),
            review_usage: Some(Usage {
                input_tokens: None,
                output_tokens: Some(5),
                cached_input_tokens: None,
                estimated: true,
            }),
            compaction_usage: None,
            execution_cost: None,
            review_cost: None,
            compaction_cost: None,
            ..RunMetrics::default()
        };
        let artifacts = vec![Artifact {
            path: "src/lib.rs".into(),
            operation: "modified".into(),
        }];
        parent.merge_child(&child, &artifacts);

        let (metrics, total, merged_artifacts) = parent.snapshot().unwrap();
        assert_eq!(metrics.execution_requests, 3);
        assert_eq!(metrics.review_requests, 1);
        assert_eq!(metrics.tool_calls, 4);
        assert_eq!(metrics.tool_executions, 2);
        assert_eq!(
            metrics.execution_usage,
            Some(Usage {
                input_tokens: Some(300),
                output_tokens: Some(65),
                cached_input_tokens: Some(40),
                estimated: false,
            })
        );
        let total = total.unwrap();
        assert_eq!(total.input_tokens, None);
        assert_eq!(total.output_tokens, Some(70));
        assert!(total.estimated);
        assert_eq!(merged_artifacts, artifacts);
    }
}
