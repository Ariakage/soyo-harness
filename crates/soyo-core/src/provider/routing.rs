//! Capability-based model routing, role assignment, cost budgets, circuit-breaker
//! health tracking, and explicit opaque-history migration (`F-16`).

use crate::{
    config::{
        MediaCapabilities, OpaqueHistoryPolicy, Pricing, Profile, ProfileCapabilities, Protocol,
        RoutingSettings,
    },
    contract::{
        BoundaryError, ErrorInfo, HistoryItem, ModelProtocol, ModelPurpose, WorkMode, error_codes,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::{Duration, Instant},
};

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

pub fn model_protocol(protocol: Protocol) -> ModelProtocol {
    match protocol {
        Protocol::ChatCompletions => ModelProtocol::ChatCompletions,
        Protocol::Responses => ModelProtocol::Responses,
        Protocol::AnthropicMessages => ModelProtocol::AnthropicMessages,
    }
}

/// Functional role used when selecting a model profile (`F-16`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    Execution,
    Planning,
    Review,
    Compaction,
}

impl ModelRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Execution => "execution",
            Self::Planning => "planning",
            Self::Review => "review",
            Self::Compaction => "compaction",
        }
    }

    /// Derive the effective routing role from request purpose and work mode.
    pub fn from_request(purpose: ModelPurpose, work_mode: WorkMode) -> Self {
        match purpose {
            ModelPurpose::Review => Self::Review,
            ModelPurpose::Compaction => Self::Compaction,
            ModelPurpose::Execution => match work_mode {
                WorkMode::Plan => Self::Planning,
                _ => Self::Execution,
            },
        }
    }
}

/// Credential-free capability and pricing metadata for one registered profile.
#[derive(Debug, Clone)]
pub struct ProfileCandidateMeta {
    pub name: String,
    pub protocol: ModelProtocol,
    pub model: String,
    pub context_window_tokens: u64,
    pub max_output_tokens: u32,
    pub multimodal: MediaCapabilities,
    pub capabilities: ProfileCapabilities,
    pub pricing: Option<Pricing>,
}

impl ProfileCandidateMeta {
    pub fn from_profile(name: impl Into<String>, profile: &Profile) -> Self {
        let mut capabilities = profile.capabilities.clone();
        if profile.thinking_effort.is_some() {
            capabilities.reasoning = true;
        }
        Self {
            name: name.into(),
            protocol: model_protocol(profile.protocol),
            model: profile.model.clone(),
            context_window_tokens: profile.context_window_tokens,
            max_output_tokens: profile.max_output_tokens,
            multimodal: profile.multimodal,
            capabilities,
            pricing: profile.pricing.clone(),
        }
    }
}

/// Request requirements evaluated against configured candidates.
#[derive(Debug, Clone)]
pub struct RouteRequirements {
    pub role: ModelRole,
    pub purpose: ModelPurpose,
    pub work_mode: WorkMode,
    pub session_profile: String,
    pub review_profile: Option<String>,
    pub tools_required: bool,
    pub required_media: MediaCapabilities,
    pub estimated_input_tokens: u64,
    pub extra_safety_tokens: u64,
    pub task_type: Option<String>,
    pub prefer_reasoning: bool,
}

/// Reason a candidate profile was excluded from serving a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedCandidate {
    pub profile: String,
    pub reason: String,
    pub detail: String,
}

/// Record of explicit opaque-protocol history handling during cross-profile switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMigrationInfo {
    pub policy: String,
    pub stripped_opaque_items: usize,
    pub from_protocols: Vec<ModelProtocol>,
    pub to_protocol: ModelProtocol,
}

impl HistoryMigrationInfo {
    pub fn summary(&self) -> String {
        format!(
            "{}:stripped_{}_to_{}",
            self.policy,
            self.stripped_opaque_items,
            protocol_label(self.to_protocol)
        )
    }
}

/// Selected ordered candidate plan plus explainable exclusions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePlan {
    pub role: ModelRole,
    pub eligible: Vec<RoutedCandidate>,
    pub excluded: Vec<ExcludedCandidate>,
    pub requires_new_session: bool,
    pub requires_history_migration: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedCandidate {
    pub profile: String,
    pub model: String,
    pub protocol: ModelProtocol,
    pub source: &'static str,
    pub estimated_cost_microunits: Option<u64>,
    pub migration: Option<HistoryMigrationInfo>,
}

/// Auditable per-request route decision persisted into metrics and events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteDecisionRecord {
    pub request_id: String,
    pub purpose: ModelPurpose,
    pub role: String,
    pub profile: String,
    pub model: String,
    pub protocol: ModelProtocol,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_from: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<ExcludedCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<String>,
}

/// Snapshot of runtime circuit-breaker state for one profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileHealthStatus {
    pub healthy: bool,
    pub consecutive_failures: u32,
    pub cooldown_remaining_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_code: Option<String>,
}

struct HealthEntry {
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
    last_error_code: Option<String>,
}

/// Thread-safe circuit breaker for transient provider failures and rate limits.
#[derive(Default)]
pub struct ProfileHealthTracker {
    entries: Mutex<BTreeMap<String, HealthEntry>>,
}

impl ProfileHealthTracker {
    pub fn record_success(&self, profile: &str) {
        if let Ok(mut guard) = self.entries.lock() {
            guard.remove(profile);
        }
    }

    pub fn record_transient_failure(
        &self,
        profile: &str,
        error_code: &str,
        retry_after: Option<Duration>,
        settings: &RoutingSettings,
    ) {
        let Ok(mut guard) = self.entries.lock() else {
            return;
        };
        let entry = guard.entry(profile.to_owned()).or_insert(HealthEntry {
            consecutive_failures: 0,
            cooldown_until: None,
            last_error_code: None,
        });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.last_error_code = Some(error_code.to_owned());
        let threshold = settings.circuit_breaker_failures.max(1);
        let base_cooldown = Duration::from_secs(settings.cooldown_seconds.max(1));
        if entry.consecutive_failures >= threshold
            || (error_code == error_codes::RATE_LIMITED && retry_after.is_some())
        {
            let duration = retry_after
                .map(|delay| delay.min(Duration::from_secs(3600)).max(base_cooldown))
                .unwrap_or(base_cooldown);
            entry.cooldown_until = Some(Instant::now() + duration);
        }
    }

    pub fn cooldown_remaining(&self, profile: &str) -> Option<(u32, Duration, Option<String>)> {
        let mut guard = self.entries.lock().ok()?;
        let entry = guard.get_mut(profile)?;
        let until = entry.cooldown_until?;
        let now = Instant::now();
        if until <= now {
            entry.cooldown_until = None;
            return None;
        }
        Some((
            entry.consecutive_failures,
            until.saturating_duration_since(now),
            entry.last_error_code.clone(),
        ))
    }

    pub fn snapshot(
        &self,
        profiles: impl IntoIterator<Item = String>,
    ) -> BTreeMap<String, ProfileHealthStatus> {
        let mut out = BTreeMap::new();
        let guard = self.entries.lock().ok();
        let now = Instant::now();
        for name in profiles {
            let status = guard
                .as_ref()
                .and_then(|map| map.get(&name))
                .map(|entry| {
                    let rem = entry
                        .cooldown_until
                        .filter(|until| *until > now)
                        .map_or(0, |until| {
                            until.saturating_duration_since(now).as_secs().max(1)
                        });
                    ProfileHealthStatus {
                        healthy: rem == 0,
                        consecutive_failures: entry.consecutive_failures,
                        cooldown_remaining_seconds: rem,
                        last_error_code: entry.last_error_code.clone(),
                    }
                })
                .unwrap_or(ProfileHealthStatus {
                    healthy: true,
                    consecutive_failures: 0,
                    cooldown_remaining_seconds: 0,
                    last_error_code: None,
                });
            out.insert(name, status);
        }
        out
    }
}

pub fn protocol_label(protocol: ModelProtocol) -> &'static str {
    match protocol {
        ModelProtocol::ChatCompletions => "openai-chat-completions",
        ModelProtocol::Responses => "openai-responses",
        ModelProtocol::AnthropicMessages => "anthropic-messages",
    }
}

fn is_endpoint_opaque_item(protocol: ModelProtocol, value: &Value) -> bool {
    match protocol {
        ModelProtocol::Responses => {
            value.get("type").and_then(Value::as_str) == Some("reasoning")
                || value.get("encrypted_content").is_some()
        }
        ModelProtocol::AnthropicMessages => matches!(
            value.get("type").and_then(Value::as_str),
            Some("thinking" | "redacted_thinking")
        ),
        ModelProtocol::ChatCompletions => value.get("reasoning_content").is_some(),
    }
}

/// Inspect whether `history` contains opaque or cross-protocol provider items
/// relative to `(target_profile, target_protocol)`.
pub fn inspect_incompatible_opaque_items(
    history: &[HistoryItem],
    target_profile: &str,
    target_protocol: ModelProtocol,
    source_profile: &str,
) -> (usize, Vec<ModelProtocol>) {
    let mut count = 0usize;
    let mut unique = Vec::new();
    let cross_profile = !source_profile.is_empty() && source_profile != target_profile;
    for item in history {
        if let HistoryItem::ProviderItem { protocol, value } = item
            && (*protocol != target_protocol
                || (cross_profile && is_endpoint_opaque_item(*protocol, value)))
        {
            count += 1;
            if !unique.contains(protocol) {
                unique.push(*protocol);
            }
        }
    }
    (count, unique)
}

/// Strip incompatible or endpoint-bound opaque `ProviderItem`s while preserving every
/// semantic conversation turn and executed `ToolCall`/`ToolResult` verbatim.
/// Never drops tool results or permits re-executing completed tools.
pub fn project_compatible_history(
    history: &[HistoryItem],
    target_profile: &str,
    target_protocol: ModelProtocol,
    source_profile: &str,
    policy: OpaqueHistoryPolicy,
) -> Result<(Vec<HistoryItem>, Option<HistoryMigrationInfo>), BoundaryError> {
    let (count, from_protocols) =
        inspect_incompatible_opaque_items(history, target_profile, target_protocol, source_profile);
    if count == 0 {
        return Ok((history.to_vec(), None));
    }
    match policy {
        OpaqueHistoryPolicy::Reject => Err(failure(
            error_codes::HISTORY_MIGRATION_REQUIRED,
            format!(
                "history contains {count} opaque provider items incompatible with profile '{target_profile}'"
            ),
        )),
        OpaqueHistoryPolicy::RequireNewSession => Err(failure(
            error_codes::NEW_SESSION_REQUIRED,
            format!(
                "profile '{target_profile}' requires a new session because history has {count} opaque provider items"
            ),
        )),
        OpaqueHistoryPolicy::MigrateCompatible => {
            let mut migrated = Vec::with_capacity(history.len().saturating_sub(count));
            for item in history {
                if let HistoryItem::ProviderItem { protocol, .. } = item
                    && (*protocol != target_protocol || target_profile != source_profile)
                {
                    // Also drop companion wrapper ProviderItems from another protocol so
                    // semantic AssistantText/ToolCall items serialize cleanly without mismatches.
                    continue;
                }
                migrated.push(item.clone());
            }
            Ok((
                migrated,
                Some(HistoryMigrationInfo {
                    policy: "migrate_compatible".into(),
                    stripped_opaque_items: count,
                    from_protocols,
                    to_protocol: target_protocol,
                }),
            ))
        }
    }
}

fn work_mode_tags(mode: WorkMode, role: ModelRole, custom: Option<&str>) -> Vec<String> {
    let mut tags = vec![
        match mode {
            WorkMode::Plan => "plan",
            WorkMode::Build => "build",
            WorkMode::Code => "code",
            WorkMode::Work => "work",
        }
        .to_owned(),
        role.as_str().to_owned(),
    ];
    if mode == WorkMode::Code {
        tags.push("coding".into());
    }
    if role == ModelRole::Planning {
        tags.push("plan".into());
    }
    if let Some(tag) = custom {
        tags.push(tag.to_owned());
    }
    tags
}

fn estimate_min_cost_microunits(
    pricing: Option<&Pricing>,
    input_tokens: u64,
    output_tokens: u32,
) -> Option<u64> {
    let pricing = pricing?;
    let in_num =
        u128::from(input_tokens).checked_mul(u128::from(pricing.input_microunits_per_million))?;
    // Include at most 1 output token minimum + small baseline to compare rates fairly when sorting.
    let out_sample = u64::from(output_tokens).clamp(1, 256);
    let out_num =
        u128::from(out_sample).checked_mul(u128::from(pricing.output_microunits_per_million))?;
    let total = in_num.checked_add(out_num)?;
    let amount = (total / 1_000_000).checked_add(u128::from(total % 1_000_000 != 0))?;
    u64::try_from(amount).ok()
}

/// Stateful per-runtime coordinator for multi-profile routing and health failover.
pub struct RoutingCoordinator {
    settings: RoutingSettings,
    candidates: BTreeMap<String, ProfileCandidateMeta>,
    health: std::sync::Arc<ProfileHealthTracker>,
}

impl RoutingCoordinator {
    pub fn new(
        settings: RoutingSettings,
        candidates: BTreeMap<String, ProfileCandidateMeta>,
        health: std::sync::Arc<ProfileHealthTracker>,
    ) -> Self {
        Self {
            settings,
            candidates,
            health,
        }
    }

    pub fn disabled(candidates: BTreeMap<String, ProfileCandidateMeta>) -> Self {
        Self::new(
            RoutingSettings::default(),
            candidates,
            std::sync::Arc::new(ProfileHealthTracker::default()),
        )
    }

    pub fn is_enabled(&self) -> bool {
        self.settings.enabled
    }

    pub fn settings(&self) -> &RoutingSettings {
        &self.settings
    }

    pub fn health(&self) -> &ProfileHealthTracker {
        &self.health
    }

    pub fn candidate_meta(&self, profile: &str) -> Option<&ProfileCandidateMeta> {
        self.candidates.get(profile)
    }

    pub fn pricing_meta(&self) -> BTreeMap<String, crate::metrics::ProfilePricingMeta> {
        self.candidates
            .iter()
            .map(|(name, meta)| {
                (
                    name.clone(),
                    crate::metrics::ProfilePricingMeta {
                        model: meta.model.clone(),
                        protocol: meta.protocol,
                        pricing: meta.pricing.clone(),
                    },
                )
            })
            .collect()
    }

    fn candidate_chain(&self, req: &RouteRequirements) -> Vec<(String, &'static str)> {
        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        let mut push = |name: &str, source: &'static str| {
            if !name.is_empty() && seen.insert(name.to_owned()) {
                chain.push((name.to_owned(), source));
            }
        };
        if !self.settings.enabled {
            let primary = match req.role {
                ModelRole::Review => req
                    .review_profile
                    .as_deref()
                    .unwrap_or(&req.session_profile),
                _ => &req.session_profile,
            };
            push(primary, "session_default");
            return chain;
        }
        match req.role {
            ModelRole::Review => {
                if let Some(review) = &req.review_profile {
                    push(review, "review_config");
                }
                for name in &self.settings.roles.review {
                    push(name, "role_review");
                }
                push(&req.session_profile, "session_default");
            }
            ModelRole::Compaction => {
                for name in &self.settings.roles.compaction {
                    push(name, "role_compaction");
                }
                push(&req.session_profile, "session_default");
            }
            ModelRole::Planning => {
                for name in &self.settings.roles.plan {
                    push(name, "mode_plan");
                }
                for name in &self.settings.roles.planning {
                    push(name, "role_planning");
                }
                for name in &self.settings.roles.execution {
                    push(name, "role_execution");
                }
                push(&req.session_profile, "session_default");
            }
            ModelRole::Execution => {
                let mode_list = match req.work_mode {
                    WorkMode::Code => &self.settings.roles.code,
                    WorkMode::Build => &self.settings.roles.build,
                    WorkMode::Work => &self.settings.roles.work,
                    WorkMode::Plan => &self.settings.roles.plan,
                };
                for name in mode_list {
                    push(name, "mode_role");
                }
                for name in &self.settings.roles.execution {
                    push(name, "role_execution");
                }
                push(&req.session_profile, "session_default");
            }
        }
        for fallback in &self.settings.fallback_profiles {
            push(fallback, "fallback");
        }
        chain
    }

    /// Deterministically evaluate and rank candidates with explainable exclusion reasons.
    pub fn plan(
        &self,
        req: &RouteRequirements,
        history: &[HistoryItem],
        spent_by_currency: &BTreeMap<String, u64>,
    ) -> RoutePlan {
        self.plan_with_source(req, history, spent_by_currency, None)
    }

    /// Evaluate and rank candidates while optionally specifying the profile that
    /// authored existing opaque `ProviderItem`s in `history`.
    pub fn plan_with_source(
        &self,
        req: &RouteRequirements,
        history: &[HistoryItem],
        spent_by_currency: &BTreeMap<String, u64>,
        history_source_profile: Option<&str>,
    ) -> RoutePlan {
        let opaque_source = history_source_profile.unwrap_or(&req.session_profile);
        let raw_chain = self.candidate_chain(req);
        if !self.settings.enabled {
            let mut eligible = Vec::new();
            let mut excluded = Vec::new();
            for (name, source) in raw_chain {
                if let Some(meta) = self.candidates.get(&name) {
                    eligible.push(RoutedCandidate {
                        profile: meta.name.clone(),
                        model: meta.model.clone(),
                        protocol: meta.protocol,
                        source,
                        estimated_cost_microunits: estimate_min_cost_microunits(
                            meta.pricing.as_ref(),
                            req.estimated_input_tokens,
                            meta.max_output_tokens,
                        ),
                        migration: None,
                    });
                } else {
                    excluded.push(ExcludedCandidate {
                        profile: name,
                        reason: "unknown_profile".into(),
                        detail: "profile is not registered".into(),
                    });
                }
            }
            return RoutePlan {
                role: req.role,
                eligible,
                excluded,
                requires_new_session: false,
                requires_history_migration: false,
            };
        }

        let tags = work_mode_tags(req.work_mode, req.role, req.task_type.as_deref());
        let mut eligible = Vec::new();
        let mut excluded = Vec::new();
        let mut requires_new_session = false;
        let mut requires_history_migration = false;

        for (name, source) in raw_chain {
            let Some(meta) = self.candidates.get(&name) else {
                excluded.push(ExcludedCandidate {
                    profile: name,
                    reason: "unknown_profile".into(),
                    detail: "profile is not registered".into(),
                });
                continue;
            };
            if req.tools_required && !meta.capabilities.tools {
                excluded.push(ExcludedCandidate {
                    profile: name,
                    reason: "tools_unsupported".into(),
                    detail: "request supplies tools but profile disables tool calling".into(),
                });
                continue;
            }
            if req.required_media.image && !meta.multimodal.image {
                excluded.push(ExcludedCandidate {
                    profile: name,
                    reason: "missing_image_capability".into(),
                    detail: "request includes images but profile multimodal.image is false".into(),
                });
                continue;
            }
            if req.required_media.audio && !meta.multimodal.audio {
                excluded.push(ExcludedCandidate {
                    profile: name,
                    reason: "missing_audio_capability".into(),
                    detail: "request includes audio but profile multimodal.audio is false".into(),
                });
                continue;
            }
            if req.required_media.video && !meta.multimodal.video {
                excluded.push(ExcludedCandidate {
                    profile: name,
                    reason: "missing_video_capability".into(),
                    detail: "request includes video but profile multimodal.video is false".into(),
                });
                continue;
            }
            if req.prefer_reasoning && !meta.capabilities.reasoning {
                excluded.push(ExcludedCandidate {
                    profile: name,
                    reason: "missing_reasoning_capability".into(),
                    detail: "request prefers reasoning capability not enabled on profile".into(),
                });
                continue;
            }
            if !meta.capabilities.task_types.is_empty()
                && !tags
                    .iter()
                    .any(|tag| meta.capabilities.task_types.contains(tag))
            {
                excluded.push(ExcludedCandidate {
                    profile: name,
                    reason: "task_type_mismatch".into(),
                    detail: "profile task_types filter excludes this task role/mode".into(),
                });
                continue;
            }
            let safety = meta
                .context_window_tokens
                .div_ceil(10)
                .saturating_add(1)
                .saturating_add(req.extra_safety_tokens);
            let usable = meta.context_window_tokens.saturating_sub(safety);
            if req.estimated_input_tokens.saturating_add(1) > usable {
                excluded.push(ExcludedCandidate {
                    profile: name,
                    reason: "context_window_insufficient".into(),
                    detail: format!(
                        "estimated {} tokens exceeds usable context capacity {usable}",
                        req.estimated_input_tokens
                    ),
                });
                continue;
            }
            let est_cost = estimate_min_cost_microunits(
                meta.pricing.as_ref(),
                req.estimated_input_tokens,
                meta.max_output_tokens,
            );
            if let Some(cap) = self.settings.max_run_cost_microunits {
                match (&meta.pricing, &self.settings.cost_currency) {
                    (None, _) if self.settings.require_pricing_for_budget => {
                        excluded.push(ExcludedCandidate {
                            profile: name,
                            reason: "pricing_required".into(),
                            detail: "run cost budget requires explicit profile pricing".into(),
                        });
                        continue;
                    }
                    (Some(pricing), Some(expected)) if &pricing.currency != expected => {
                        excluded.push(ExcludedCandidate {
                            profile: name,
                            reason: "currency_mismatch".into(),
                            detail: format!(
                                "profile pricing currency {} differs from routing currency {expected}",
                                pricing.currency
                            ),
                        });
                        continue;
                    }
                    (Some(pricing), _) => {
                        let spent = spent_by_currency
                            .get(&pricing.currency)
                            .copied()
                            .unwrap_or(0);
                        let min_in = estimate_min_cost_microunits(
                            Some(pricing),
                            req.estimated_input_tokens,
                            1,
                        )
                        .unwrap_or(u64::MAX);
                        if spent >= cap || spent.saturating_add(min_in) > cap {
                            excluded.push(ExcludedCandidate {
                                profile: name,
                                reason: "cost_budget_exceeded".into(),
                                detail: format!(
                                    "spent {spent} + est {min_in} microunits {} exceeds cap {cap}",
                                    pricing.currency
                                ),
                            });
                            continue;
                        }
                    }
                    (None, _) => {
                        let any_exceeded = self
                            .settings
                            .cost_currency
                            .as_ref()
                            .and_then(|c| spent_by_currency.get(c).copied())
                            .unwrap_or_else(|| spent_by_currency.values().copied().sum());
                        if any_exceeded >= cap {
                            excluded.push(ExcludedCandidate {
                                profile: name,
                                reason: "cost_budget_exceeded".into(),
                                detail: format!(
                                    "accumulated spend {any_exceeded} reached cap {cap}"
                                ),
                            });
                            continue;
                        }
                    }
                }
            }

            let migration = match project_compatible_history(
                history,
                &meta.name,
                meta.protocol,
                opaque_source,
                self.settings.opaque_history,
            ) {
                Ok((_, info)) => info,
                Err(err) => {
                    if err.is_code(error_codes::NEW_SESSION_REQUIRED) {
                        requires_new_session = true;
                    } else {
                        requires_history_migration = true;
                    }
                    excluded.push(ExcludedCandidate {
                        profile: name,
                        reason: err.code().to_owned(),
                        detail: err.message().to_owned(),
                    });
                    continue;
                }
            };

            if let Some((failures, rem, last_err)) = self.health.cooldown_remaining(&name) {
                excluded.push(ExcludedCandidate {
                    profile: name,
                    reason: "circuit_open".into(),
                    detail: format!(
                        "{failures} consecutive failures ({}), cooldown {}s remaining",
                        last_err.unwrap_or_else(|| "transient_error".into()),
                        rem.as_secs().max(1)
                    ),
                });
                continue;
            }

            eligible.push(RoutedCandidate {
                profile: meta.name.clone(),
                model: meta.model.clone(),
                protocol: meta.protocol,
                source,
                estimated_cost_microunits: est_cost,
                migration,
            });
        }

        if self.settings.prefer_lowest_cost && eligible.len() > 1 {
            eligible.sort_by_key(|cand| cand.estimated_cost_microunits.unwrap_or(u64::MAX));
        }

        RoutePlan {
            role: req.role,
            eligible,
            excluded,
            requires_new_session,
            requires_history_migration,
        }
    }

    /// Return the error to raise when `plan.eligible` is empty.
    pub fn empty_plan_error(&self, plan: &RoutePlan) -> BoundaryError {
        if plan
            .excluded
            .iter()
            .any(|item| item.reason == "cost_budget_exceeded")
            && !plan
                .excluded
                .iter()
                .all(|item| item.reason == "circuit_open")
            && plan
                .excluded
                .iter()
                .filter(|item| item.reason == "cost_budget_exceeded")
                .count()
                == plan.excluded.len()
        {
            return failure(
                error_codes::COST_BUDGET_EXCEEDED,
                "all candidate profiles exceed the configured run cost budget",
            );
        }
        if plan.requires_new_session {
            return failure(
                error_codes::NEW_SESSION_REQUIRED,
                "candidate profiles require a new session due to opaque protocol history",
            );
        }
        if plan.requires_history_migration {
            return failure(
                error_codes::HISTORY_MIGRATION_REQUIRED,
                "candidate profiles cannot replay opaque protocol history without compatible migration",
            );
        }
        let summary = plan
            .excluded
            .iter()
            .map(|e| format!("{}: {}", e.profile, e.reason))
            .collect::<Vec<_>>()
            .join("; ");
        failure(
            error_codes::MODEL_UNAVAILABLE,
            if summary.is_empty() {
                "no eligible model profile configured".to_owned()
            } else {
                format!("no eligible model profile available ({summary})")
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{ToolCall, ToolResult};

    fn meta(
        name: &str,
        protocol: ModelProtocol,
        image: bool,
        tools: bool,
        rate: u64,
    ) -> ProfileCandidateMeta {
        ProfileCandidateMeta {
            name: name.into(),
            protocol,
            model: format!("{name}-model"),
            context_window_tokens: 128_000,
            max_output_tokens: 8192,
            multimodal: MediaCapabilities {
                image,
                audio: false,
                video: false,
            },
            capabilities: ProfileCapabilities {
                tools,
                reasoning: false,
                task_types: BTreeSet::new(),
            },
            pricing: Some(Pricing {
                currency: "USD".into(),
                input_microunits_per_million: rate,
                output_microunits_per_million: rate * 2,
            }),
        }
    }

    #[test]
    fn capability_and_context_mismatches_are_excluded_with_reasons() {
        let mut settings = RoutingSettings {
            enabled: true,
            ..Default::default()
        };
        settings.roles.execution = vec![
            "no_tools".into(),
            "no_img".into(),
            "small_ctx".into(),
            "good".into(),
        ];
        let mut small = meta("small_ctx", ModelProtocol::ChatCompletions, true, true, 100);
        small.context_window_tokens = 1000;
        let candidates = BTreeMap::from([
            (
                "no_tools".into(),
                meta("no_tools", ModelProtocol::ChatCompletions, true, false, 100),
            ),
            (
                "no_img".into(),
                meta("no_img", ModelProtocol::ChatCompletions, false, true, 100),
            ),
            ("small_ctx".into(), small),
            (
                "good".into(),
                meta("good", ModelProtocol::Responses, true, true, 200),
            ),
        ]);
        let coordinator = RoutingCoordinator::new(settings, candidates, Default::default());
        let req = RouteRequirements {
            role: ModelRole::Execution,
            purpose: ModelPurpose::Execution,
            work_mode: WorkMode::Code,
            session_profile: "good".into(),
            review_profile: None,
            tools_required: true,
            required_media: MediaCapabilities {
                image: true,
                audio: false,
                video: false,
            },
            estimated_input_tokens: 1500,
            extra_safety_tokens: 0,
            task_type: None,
            prefer_reasoning: false,
        };
        let plan = coordinator.plan(&req, &[], &BTreeMap::new());
        assert_eq!(plan.eligible.len(), 1);
        assert_eq!(plan.eligible[0].profile, "good");
        assert_eq!(plan.excluded.len(), 3);
        assert_eq!(plan.excluded[0].reason, "tools_unsupported");
        assert_eq!(plan.excluded[1].reason, "missing_image_capability");
        assert_eq!(plan.excluded[2].reason, "context_window_insufficient");
    }

    #[test]
    fn opaque_history_rejects_vs_migrates_without_dropping_tools() {
        let history = vec![
            HistoryItem::UserText("fix bug".into()),
            HistoryItem::ProviderItem {
                protocol: ModelProtocol::Responses,
                value: serde_json::json!({"type":"reasoning","encrypted_content":"opaque"}),
            },
            HistoryItem::ToolCall(ToolCall {
                call_id: "c1".into(),
                name: "edit_file".into(),
                arguments: "{}".into(),
            }),
            HistoryItem::ToolResult {
                call_id: "c1".into(),
                result: ToolResult::success(Some(serde_json::json!({"ok":true}))),
            },
        ];

        let err = project_compatible_history(
            &history,
            "backup",
            ModelProtocol::AnthropicMessages,
            "primary",
            OpaqueHistoryPolicy::Reject,
        )
        .unwrap_err();
        assert_eq!(err.code(), error_codes::HISTORY_MIGRATION_REQUIRED);

        let err = project_compatible_history(
            &history,
            "backup",
            ModelProtocol::AnthropicMessages,
            "primary",
            OpaqueHistoryPolicy::RequireNewSession,
        )
        .unwrap_err();
        assert_eq!(err.code(), error_codes::NEW_SESSION_REQUIRED);

        let (migrated, info) = project_compatible_history(
            &history,
            "backup",
            ModelProtocol::AnthropicMessages,
            "primary",
            OpaqueHistoryPolicy::MigrateCompatible,
        )
        .unwrap();
        assert_eq!(info.unwrap().stripped_opaque_items, 1);
        assert!(matches!(migrated[0], HistoryItem::UserText(_)));
        assert!(matches!(migrated[1], HistoryItem::ToolCall(_)));
        assert!(matches!(migrated[2], HistoryItem::ToolResult { .. }));
    }
}
