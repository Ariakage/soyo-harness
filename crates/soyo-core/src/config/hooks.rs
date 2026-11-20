//! Host-configured automation hook rules (`[hooks]`, `F-04`).
//!
//! Disabled by default (`enabled = false`) to preserve zero-drift baseline runs.
//! Every configured rule executes inside the host sandbox (`PreparedEnvironment`),
//! passes through the durable `HookJournal`, and never grants permissions beyond `Policy`.

use super::{Environment, invalid, valid_env_name};
use crate::{
    contract::{BoundaryError, WorkMode},
    hooks::{HookEvent, HookKind},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_HOST_HOOK_RULES: usize = 32;
pub const MAX_HOST_HOOK_ARGS: usize = 64;
pub const MAX_HOST_HOOK_TIMEOUT_MS: u32 = 5_000;
pub const DEFAULT_HOST_HOOK_TIMEOUT_MS: u32 = 5_000;

fn default_hook_kind() -> String {
    "observer".into()
}

fn default_hook_timeout_ms() -> u32 {
    DEFAULT_HOST_HOOK_TIMEOUT_MS
}

fn valid_rule_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn has_inline_credential_flag(arg: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    [
        "--api-key",
        "--api_key",
        "--token",
        "--password",
        "--secret",
    ]
    .iter()
    .any(|flag| {
        lower == *flag
            || lower
                .strip_prefix(flag)
                .is_some_and(|suffix| suffix.starts_with('='))
    }) || lower.contains("authorization:")
        || lower.contains("bearer ")
}

/// One user-configured host hook rule under `[[hooks.rules]]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostHookRule {
    pub id: String,
    pub event: String,
    #[serde(default = "default_hook_kind")]
    pub kind: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub modes: Vec<WorkMode>,
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u32,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Host automation hook settings (`[hooks]`).
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HookSettings {
    /// Disabled by default to preserve MVP and plugin-only behavior unless configured.
    pub enabled: bool,
    pub rules: Vec<HostHookRule>,
}

impl HookSettings {
    /// Validate rule schemas, event kinds, tool/mode filters, and env names offline.
    pub fn validate(&self, env: &Environment) -> Result<(), BoundaryError> {
        if self.rules.len() > MAX_HOST_HOOK_RULES {
            return Err(invalid("host hook rule count exceeds supported maximum"));
        }
        let mut ids = BTreeSet::new();
        for rule in &self.rules {
            if !valid_rule_id(&rule.id) || !ids.insert(rule.id.as_str()) {
                return Err(invalid("host hook rule id is invalid or duplicated"));
            }
            let event = HookEvent::parse(&rule.event)
                .map_err(|_| invalid("host hook event is unsupported"))?;
            let kind = HookKind::parse(&rule.kind)
                .map_err(|_| invalid("host hook kind must be observer or check"))?;
            if kind == HookKind::Check && !event.allows_check() {
                return Err(invalid(
                    "check hooks are supported only for tool.before_dispatch and model.before_request",
                ));
            }
            if rule.timeout_ms == 0 || rule.timeout_ms > MAX_HOST_HOOK_TIMEOUT_MS {
                return Err(invalid("host hook timeout_ms must be between 1 and 5000"));
            }
            if rule.argv.is_empty()
                || rule.argv[0].trim().is_empty()
                || rule.argv.len() > MAX_HOST_HOOK_ARGS
                || rule.argv.iter().map(String::len).sum::<usize>() > 64 * 1024
                || rule.argv.iter().any(|arg| {
                    arg.contains('\0')
                        || arg.chars().any(crate::text::is_terminal_control)
                        || has_inline_credential_flag(arg)
                })
            {
                return Err(invalid(
                    "host hook argv is invalid or contains unsafe flags",
                ));
            }
            if rule.tools.len() > 32
                || rule.tools.iter().any(|name| {
                    name.trim().is_empty()
                        || name.len() > 128
                        || name.chars().any(crate::text::is_terminal_control)
                })
            {
                return Err(invalid("host hook tools filter is invalid"));
            }
            if !rule.tools.is_empty() && !event.is_tool_event() {
                return Err(invalid(
                    "host hook tools filter applies only to tool lifecycle events",
                ));
            }
            if rule.modes.len() > 4 {
                return Err(invalid("host hook modes filter exceeds four modes"));
            }
            if rule.env.len() > 16
                || rule.env.iter().any(|(dest, src)| {
                    !valid_env_name(dest) || !valid_env_name(src) || env.get_os(src).is_none()
                })
            {
                return Err(invalid("host hook env references are invalid or missing"));
            }
        }
        Ok(())
    }
}
