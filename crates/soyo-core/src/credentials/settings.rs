//! `[credentials]` configuration section: backend selection plus host-authored
//! scoped grants (`[[credentials.grants]]`). Defaults keep the private file
//! backend and grant nothing — credential behavior is unchanged without config.
use super::{CredentialBackend, CredentialGrant, GrantRunKind};
use crate::contract::BoundaryError;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::str::FromStr;

fn invalid(message: &str) -> BoundaryError {
    crate::contract::BoundaryError(
        crate::contract::ErrorInfo {
            code: "invalid_config".into(),
            message: message.into(),
        },
        None,
        None,
    )
}

/// Raw TOML-facing settings. `backend` defaults to `file`; `grants` is an
/// explicit list of host-authored `CredentialGrant` records.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CredentialSettings {
    pub backend: CredentialBackend,
    #[serde(default)]
    pub grants: Vec<GrantConfig>,
}

/// Host-authored grant entry. `reference`/`server_id` required; filters empty
/// by default (any workspace / any tool), `run_kinds` required (no default —
/// the kind must be chosen explicitly), `expires_at` optional RFC 3339 UTC.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantConfig {
    pub reference: String,
    pub server_id: String,
    #[serde(default)]
    pub workspaces: BTreeSet<String>,
    pub run_kinds: BTreeSet<GrantRunKind>,
    #[serde(default)]
    pub tools: BTreeSet<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

impl CredentialSettings {
    pub fn validate(&self) -> Result<(), BoundaryError> {
        if self.backend == CredentialBackend::System && !CredentialBackend::system_supported() {
            // Fail closed at config-load time too; `open_backend` re-checks at
            // store open so a config written elsewhere still errors explicitly.
            return Err(invalid(
                "credentials backend `system` is not supported on this platform",
            ));
        }
        let mut refs = BTreeSet::new();
        for grant in &self.grants {
            grant.validate()?;
            // Each grant line produces one persisted grant; identical
            // (reference, server_id, filters) duplicates are pointless and
            // likely a config error.
            if !refs.insert((
                grant.reference.clone(),
                grant.server_id.clone(),
                grant.workspaces.clone(),
                grant.tools.clone(),
                grant.expires_at.clone(),
            )) {
                return Err(invalid("duplicate credential grant"));
            }
        }
        Ok(())
    }
}

impl GrantConfig {
    pub fn validate(&self) -> Result<(), BoundaryError> {
        self.to_grant(uuid::Uuid::nil()).map(|_| ())
    }

    /// Materialize to a `CredentialGrant` with a caller-chosen id. Config
    /// grants get a fresh v7 id at materialization — they're host-authored
    /// policy, not durable store records, so there's no stable id to preserve.
    pub fn to_grant(&self, grant_id: uuid::Uuid) -> Result<CredentialGrant, BoundaryError> {
        let reference = super::CredentialRef::from_str(&self.reference)
            .map_err(|_| invalid("credential grant reference is not a valid UUID"))?;
        let grant = CredentialGrant {
            grant_id,
            reference,
            workspaces: self.workspaces.clone(),
            run_kinds: self.run_kinds.clone(),
            tools: self.tools.clone(),
            expires_at: self.expires_at.clone(),
            server_id: self.server_id.clone(),
        };
        grant
            .validate()
            .map_err(|_| invalid("credential grant is invalid"))?;
        Ok(grant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_file_backend_and_no_grants() {
        let settings: CredentialSettings = toml::from_str("").unwrap();
        assert_eq!(settings.backend, CredentialBackend::File);
        assert!(settings.grants.is_empty());
        settings.validate().unwrap();
    }

    #[test]
    fn system_backend_fails_closed_where_unsupported() {
        let settings: CredentialSettings = toml::from_str(r#"backend = "system""#).unwrap();
        if CredentialBackend::system_supported() {
            assert!(settings.validate().is_ok());
        } else {
            assert!(settings.validate().is_err());
        }
    }

    #[test]
    fn grant_requires_run_kinds_and_valid_reference() {
        let bad: Result<CredentialSettings, _> = toml::from_str(
            r#"[[grants]]
reference = "not-a-uuid"
server_id = "srv"
run_kinds = ["tool"]
"#,
        );
        let settings = bad.unwrap();
        assert!(settings.validate().is_err());

        // `run_kinds` is a required field; omitting it fails at parse, and an
        // explicit empty set fails validation.
        assert!(
            toml::from_str::<CredentialSettings>(
                r#"[[grants]]
reference = "00000000-0000-0000-0000-000000000001"
server_id = "srv"
"#,
            )
            .is_err()
        );
        let empty_kinds: CredentialSettings = toml::from_str(
            r#"[[grants]]
reference = "00000000-0000-0000-0000-000000000001"
server_id = "srv"
run_kinds = []
"#,
        )
        .unwrap();
        assert!(empty_kinds.validate().is_err());
    }
}
