//! Explicit model routing, role assignment, cost budgets, and failover policy (`F-16`).

use super::{Profile, invalid};
use crate::contract::BoundaryError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const DEFAULT_CIRCUIT_BREAKER_FAILURES: u32 = 2;
pub const MAX_CIRCUIT_BREAKER_FAILURES: u32 = 16;
pub const DEFAULT_COOLDOWN_SECONDS: u64 = 60;
pub const MAX_COOLDOWN_SECONDS: u64 = 3600;
const MAX_CANDIDATES: usize = 16;

fn yes() -> bool {
    true
}

/// Optional model capability metadata declared on `[profiles.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileCapabilities {
    #[serde(default = "yes")]
    pub tools: bool,
    pub reasoning: bool,
    pub task_types: BTreeSet<String>,
}

impl Default for ProfileCapabilities {
    fn default() -> Self {
        Self {
            tools: true,
            reasoning: false,
            task_types: BTreeSet::new(),
        }
    }
}

impl ProfileCapabilities {
    /// Returns true when the profile carries only legacy implicit capabilities
    /// (`tools = true`, `reasoning = false`, no `task_types` filter).
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Validate task-type tag counts and ASCII bounds.
    pub fn validate(&self) -> Result<(), BoundaryError> {
        if self.task_types.len() > 32
            || self.task_types.iter().any(|tag| {
                tag.is_empty()
                    || tag.len() > 32
                    || !tag.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
                    })
            })
        {
            return Err(invalid("profile task_types capabilities are invalid"));
        }
        Ok(())
    }
}

/// Explicit handling when failover or cross-profile switching encounters
/// opaque provider protocol state (`HistoryItem::ProviderItem`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpaqueHistoryPolicy {
    /// Fail closed rather than stripping or reusing incompatible opaque items.
    #[default]
    Reject,
    /// Strip incompatible/endpoint-bound opaque `ProviderItem`s while retaining
    /// all semantic conversation and executed tool call/result records.
    MigrateCompatible,
    /// Require starting or forking a new compatible session rather than mutating in place.
    RequireNewSession,
}

/// Ordered candidate profiles for each runtime role and work mode.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoleRoutingConfig {
    pub execution: Vec<String>,
    pub planning: Vec<String>,
    pub review: Vec<String>,
    pub compaction: Vec<String>,
    pub plan: Vec<String>,
    pub build: Vec<String>,
    pub code: Vec<String>,
    pub work: Vec<String>,
}

/// Host-controlled model routing and bounded failover configuration (`[routing]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingSettings {
    /// Disabled by default to preserve MVP single-profile session behavior.
    pub enabled: bool,
    pub roles: RoleRoutingConfig,
    pub fallback_profiles: Vec<String>,
    pub prefer_lowest_cost: bool,
    pub max_run_cost_microunits: Option<u64>,
    pub cost_currency: Option<String>,
    pub require_pricing_for_budget: bool,
    pub circuit_breaker_failures: u32,
    pub cooldown_seconds: u64,
    pub opaque_history: OpaqueHistoryPolicy,
}

impl Default for RoutingSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            roles: RoleRoutingConfig::default(),
            fallback_profiles: Vec::new(),
            prefer_lowest_cost: false,
            max_run_cost_microunits: None,
            cost_currency: None,
            require_pricing_for_budget: false,
            circuit_breaker_failures: DEFAULT_CIRCUIT_BREAKER_FAILURES,
            cooldown_seconds: DEFAULT_COOLDOWN_SECONDS,
            opaque_history: OpaqueHistoryPolicy::default(),
        }
    }
}

fn validate_candidate_list(
    list: &[String],
    profiles: &BTreeMap<String, Profile>,
) -> Result<(), BoundaryError> {
    if list.len() > MAX_CANDIDATES {
        return Err(invalid("routing candidate list exceeds its limit"));
    }
    let mut seen = BTreeSet::new();
    for name in list {
        if name.trim().is_empty() || !seen.insert(name.as_str()) {
            return Err(invalid(
                "routing candidate list contains empty or duplicate profile",
            ));
        }
        if !profiles.contains_key(name) {
            return Err(invalid("routing references an unknown profile"));
        }
    }
    Ok(())
}

impl RoutingSettings {
    /// Validate syntax, bounds, and profile references without contacting any provider.
    pub fn validate(&self, profiles: &BTreeMap<String, Profile>) -> Result<(), BoundaryError> {
        if self.circuit_breaker_failures == 0
            || self.circuit_breaker_failures > MAX_CIRCUIT_BREAKER_FAILURES
            || self.cooldown_seconds == 0
            || self.cooldown_seconds > MAX_COOLDOWN_SECONDS
        {
            return Err(invalid("routing circuit breaker bounds are invalid"));
        }
        if self.max_run_cost_microunits == Some(0) {
            return Err(invalid("routing max_run_cost_microunits must be positive"));
        }
        if let Some(currency) = &self.cost_currency
            && (currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()))
        {
            return Err(invalid(
                "routing cost_currency must be a three-letter uppercase code",
            ));
        }
        for list in [
            &self.roles.execution,
            &self.roles.planning,
            &self.roles.review,
            &self.roles.compaction,
            &self.roles.plan,
            &self.roles.build,
            &self.roles.code,
            &self.roles.work,
            &self.fallback_profiles,
        ] {
            validate_candidate_list(list, profiles)?;
        }
        Ok(())
    }
}
