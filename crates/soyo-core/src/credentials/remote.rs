//! Remote helper bearer tokens (`F-13` D-113 path (b), built on the `F-18`
//! credential layer).
//!
//! A token is an opaque random value shown once at issuance. Only its SHA-256
//! digest is persisted, as the material of a `Secret` credential bound to the
//! remote endpoint scope, together with a `run_kinds = "remote"` grant whose
//! `expires_at` is the token lifetime. Verification hashes the presented value
//! and requires an active record plus an applicable `Remote` grant; revocation
//! goes through the ordinary `CredentialStore::revoke` path (tombstone +
//! material removal + grant removal), so a revoked token stops verifying
//! immediately and no hash is retained.
use super::{
    CredentialGrant, CredentialRef, CredentialScope, CredentialStore, CredentialValue,
    GrantRunKind, GrantStore, SecretValue, backend::MaterialRecord, error, grants::now_rfc3339,
};
use crate::contract::BoundaryError;
use ring::rand::{SecureRandom, SystemRandom};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use uuid::Uuid;

/// Credential server id used by every remote bearer token.
pub const REMOTE_SERVER_ID: &str = "remote";
/// Scope string recorded on a remote token credential.
pub const REMOTE_SCOPE: &str = "remote.execute";
const TOKEN_PREFIX: &str = "soyo_rt_";
const TOKEN_RANDOM_BYTES: usize = 32;
const MAX_TOKEN_CHARS: usize = 256;

/// Result of issuing a remote bearer token. The plaintext value is exposed
/// exactly once, to the issuing CLI, and never persisted.
pub struct IssuedRemoteToken {
    pub reference: CredentialRef,
    pub grant_id: Uuid,
    pub token: SecretValue,
    pub expires_at: String,
    pub scope: CredentialScope,
}

impl std::fmt::Debug for IssuedRemoteToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IssuedRemoteToken")
            .field("reference", &self.reference)
            .field("grant_id", &self.grant_id)
            .field("token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

impl CredentialScope {
    /// Scope of a remote token credential: the helper endpoint it is meant for.
    pub fn for_remote(endpoint: &str) -> Result<Self, BoundaryError> {
        let url = reqwest::Url::parse(endpoint)
            .map_err(|_| error("credential_scope", "Remote endpoint URL is invalid"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(error(
                "credential_scope",
                "Remote endpoint URL contains unsupported components",
            ));
        }
        let scope = Self {
            server_id: REMOTE_SERVER_ID.into(),
            origin: url.origin().ascii_serialization(),
            audience: url.to_string(),
            scopes: [REMOTE_SCOPE.to_owned()].into_iter().collect(),
        };
        scope.validate()?;
        Ok(scope)
    }
}

impl CredentialStore {
    /// Issue a bearer token for `endpoint` that stays valid for `ttl_days`.
    ///
    /// Stores the digest as a `Secret` credential record plus a `Remote` grant
    /// with the matching expiry, writes a redacted audit event, and returns the
    /// plaintext token for one-time display.
    pub async fn issue_remote_token(
        &self,
        endpoint: &str,
        ttl_days: u32,
    ) -> Result<IssuedRemoteToken, BoundaryError> {
        if ttl_days == 0 || ttl_days > crate::config::MAX_TOKEN_TTL_DAYS {
            return Err(error(
                "invalid_credential",
                "Remote token lifetime must be between 1 and 365 days",
            ));
        }
        let token = SecretValue::new(generate_token()?)?;
        let digest = token_digest(token.expose());
        // Keep the plaintext in the redaction registry of the issuing process;
        // it is never written anywhere.
        self.inner()
            .secrets
            .register(&[token.expose().to_owned()])?;
        let scope = CredentialScope::for_remote(endpoint)?;
        let expires_at = super::grants::rfc3339_in_days(ttl_days);
        let reference = self
            .create(
                scope.clone(),
                CredentialValue::Secret(SecretValue::new(digest)?),
            )
            .await?;
        let grant = CredentialGrant {
            grant_id: Uuid::now_v7(),
            reference,
            workspaces: BTreeSet::new(),
            run_kinds: [GrantRunKind::Remote].into_iter().collect(),
            tools: BTreeSet::new(),
            expires_at: Some(expires_at.clone()),
            server_id: REMOTE_SERVER_ID.into(),
        };
        let grant_id = grant.grant_id;
        if let Err(err) = GrantStore::open(self.inner().index.root()).and_then(|s| s.put(&grant)) {
            // Do not leave an orphan credential behind when the grant cannot be
            // persisted: without a grant the token could never verify.
            let _ = self.revoke(reference, &scope).await;
            return Err(err);
        }
        self.audit(
            "remote_token_issue",
            Some(reference),
            &[
                ("grant_id", serde_json::json!(grant_id)),
                ("endpoint", serde_json::json!(endpoint)),
                ("expires_at", serde_json::json!(expires_at)),
            ],
        )?;
        Ok(IssuedRemoteToken {
            reference,
            grant_id,
            token,
            expires_at,
            scope,
        })
    }

    /// Verify a presented bearer token.
    ///
    /// Fails closed: an unknown value is `helper_unauthorized`, a matching
    /// record without an applicable (unexpired) `Remote` grant is
    /// `credential_grant_denied`. Revocation removes both the digest and the
    /// grant, so a revoked token has no match and fails as unauthorized — the
    /// credential audit log records the denial either way.
    pub fn verify_remote_token(&self, token: &str) -> Result<CredentialRef, BoundaryError> {
        if token.is_empty() || token.len() > MAX_TOKEN_CHARS || token.chars().any(char::is_control)
        {
            return Err(error("helper_unauthorized", "Invalid bearer token"));
        }
        let digest = token_digest(token);
        let mut matched = None;
        for metadata in self.inner().index.list_metadata()? {
            let material = match self.inner().backend.get(metadata.reference) {
                Ok(material) => material,
                Err(err) if err.0.code == "credential_not_found" => continue,
                Err(err) => return Err(err),
            };
            let MaterialRecord::Secret { value } = material else {
                continue;
            };
            if constant_time_eq(value.as_bytes(), digest.as_bytes()) {
                matched = Some(metadata.reference);
                break;
            }
        }
        let Some(reference) = matched else {
            return Err(error("helper_unauthorized", "Unknown bearer token"));
        };
        // Only a value that matches a stored digest enters the redaction set.
        // Registering arbitrary presented values would let an unauthenticated
        // caller fill the bounded registry and deny later verifications.
        self.inner().secrets.register(&[token.to_owned()])?;
        let grants = GrantStore::open(self.inner().index.root())?;
        if !grants.check(reference, GrantRunKind::Remote, None, None, &now_rfc3339())? {
            return Err(error(
                "credential_grant_denied",
                "Bearer token has no applicable remote grant (expired or revoked)",
            ));
        }
        Ok(reference)
    }

    /// Append one redacted `credential.event` line to `<credentials>/audit.log`
    /// (`0600`, append-only). Payloads carry ids, action names and non-secret
    /// metadata only; the write is refused if it would contain a known secret.
    pub fn audit(
        &self,
        event: &str,
        reference: Option<CredentialRef>,
        extra: &[(&str, serde_json::Value)],
    ) -> Result<(), BoundaryError> {
        if event.is_empty()
            || event.len() > 128
            || event
                .bytes()
                .any(|b| !(b.is_ascii_alphanumeric() || b"_.-".contains(&b)))
        {
            return Err(error("credential_io", "Credential event name is invalid"));
        }
        let mut line = serde_json::json!({
            "type": "credential.event",
            "event": event,
            "at": now_rfc3339(),
            "reference": reference,
        });
        if let Some(object) = line.as_object_mut() {
            for (key, value) in extra {
                object.insert((*key).to_owned(), value.clone());
            }
        }
        let encoded = serde_json::to_string(&line)
            .map_err(|_| error("credential_io", "credential event could not be encoded"))?;
        let secrets = self.inner().secrets.snapshot()?;
        if crate::redaction::contains_secret(&encoded, &secrets) {
            return Err(error(
                "sensitive_data",
                "credential event payload contained a credential",
            ));
        }
        append_audit_line(&self.inner().index.root().join("audit.log"), &encoded)
    }
}

fn append_audit_line(path: &Path, encoded: &str) -> Result<(), BoundaryError> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| error("credential_io", "credential audit log could not be opened"))?;
    let metadata = file
        .metadata()
        .map_err(|_| error("credential_io", "credential audit log is unavailable"))?;
    use std::os::unix::fs::MetadataExt;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(error(
            "credential_permissions",
            "credential audit log must be an owned regular file",
        ));
    }
    writeln!(file, "{encoded}").map_err(|_| {
        error(
            "credential_io",
            "credential audit event could not be written",
        )
    })
}

fn generate_token() -> Result<String, BoundaryError> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut bytes = [0u8; TOKEN_RANDOM_BYTES];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| error("credential_io", "Token randomness is unavailable"))?;
    Ok(format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

fn token_digest(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Lengths are equal by construction (both fixed-size hex digests); loop over
/// the whole value so comparison time does not depend on the first mismatch.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_stable_and_constant_time_compare_exact() {
        assert_eq!(token_digest("abc").len(), 64);
        assert_eq!(token_digest("abc"), token_digest("abc"));
        assert_ne!(token_digest("abc"), token_digest("abd"));
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"diff"));
        assert!(!constant_time_eq(b"short", b"longer"));
    }

    #[test]
    fn generated_tokens_are_prefixed_and_unique() {
        let first = generate_token().unwrap();
        let second = generate_token().unwrap();
        assert!(first.starts_with(TOKEN_PREFIX));
        assert_eq!(first.len(), TOKEN_PREFIX.len() + 43);
        assert_ne!(first, second);
    }

    #[test]
    fn remote_scope_binds_the_endpoint() {
        let scope = CredentialScope::for_remote("https://helper.example:8443").unwrap();
        assert_eq!(scope.server_id, REMOTE_SERVER_ID);
        assert_eq!(scope.audience, "https://helper.example:8443/");
        assert!(scope.scopes.contains(REMOTE_SCOPE));
        assert!(CredentialScope::for_remote("ftp://helper.example").is_err());
        assert!(CredentialScope::for_remote("https://user:pw@helper.example").is_err());
    }
}
