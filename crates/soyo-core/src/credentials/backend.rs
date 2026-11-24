//! Pluggable credential material backends. The `file` backend keeps records
//! under the private state directory; the opt-in `system` backend stores secret
//! material in the macOS Keychain while its metadata index remains file-backed,
//! so scope/grant/revocation semantics are identical across backends. A backend
//! that cannot satisfy a request fails closed — it never silently downgrades to
//! plaintext.

use super::{
    CredentialRef, CredentialScope, CredentialValue, ResolvedCredential, SecretRegistry, error,
};
use crate::contract::{BoundaryError, ErrorInfo};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

/// `[credentials] backend` selection. `file` (default) keeps zero drift;
/// `system` selects the platform system credential store (macOS Keychain).
/// Other platforms report `credential_backend_unavailable`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialBackend {
    #[default]
    File,
    System,
}

impl CredentialBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::System => "system",
        }
    }
    /// Is a `system` backend buildable on this target? Compile-time check only;
    /// runtime availability (keychain reachable) is verified at open.
    pub fn system_supported() -> bool {
        cfg!(all(target_os = "macos", feature = "security-framework"))
    }
}

/// One material record a backend persists, keyed by `CredentialRef`.
/// Metadata (scope, kind, revision, revocation, OAuth resource) is *not* part of
/// the material contract — it lives in the file index so list/grant/scope
/// checks never depend on backend query capabilities.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MaterialRecord {
    Secret { value: String },
    OAuth { credentials: Box<rmcp::transport::auth::StoredCredentials> },
}

impl MaterialRecord {
    pub(crate) fn from_public(value: &CredentialValue) -> Result<Self, BoundaryError> {
        Ok(match value {
            CredentialValue::Secret(value) => Self::Secret {
                value: value.expose().into(),
            },
            CredentialValue::OAuth(credentials) => Self::OAuth {
                credentials: credentials.clone(),
            },
        })
    }
    pub(crate) fn into_public(self) -> Result<CredentialValue, BoundaryError> {
        match self {
            Self::Secret { value } => {
                super::SecretValue::new(value).map(CredentialValue::Secret)
            }
            Self::OAuth { credentials } => Ok(CredentialValue::OAuth(credentials)),
        }
    }
}

/// The backend contract: put/get/remove opaque material by reference.
/// Implementations must be `Send + Sync` and fail closed.
pub(crate) trait MaterialBackend: Send + Sync {
    /// Persist `record` under `reference`. Fresh references are new items;
    /// existing ones are atomically replaced (rotation bumps the metadata
    /// revision, not this call).
    fn put(&self, reference: CredentialRef, record: &MaterialRecord) -> Result<(), BoundaryError>;
    /// Fetch material; `credential_not_found` on absence.
    fn get(&self, reference: CredentialRef) -> Result<MaterialRecord, BoundaryError>;
    /// Drop material while keeping the metadata tombstone (revoke). Missing is
    /// a no-op success so revoke is idempotent.
    fn remove(&self, reference: CredentialRef) -> Result<(), BoundaryError>;
    /// Backend identifier for diagnostics.
    fn name(&self) -> &'static str;
}

pub(crate) struct Inner {
    pub(crate) backend: Box<dyn MaterialBackend>,
    pub(crate) index: super::file::IndexDir,
    pub(crate) secrets: Arc<SecretRegistry>,
}

/// Wrap a `MaterialBackend` in the metadata index that tracks scope, kind,
/// revision, revocation and OAuth resource. This is the object the rest of the
/// crate resolves through; `FileCredentialStore` is its file-backed instance.
#[derive(Clone)]
pub struct CredentialStore {
    inner: Arc<Inner>,
}

impl fmt::Debug for CredentialStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CredentialStore([PRIVATE])")
    }
}

impl CredentialStore {
    /// Open the default file backend at `path` — the historical
    /// `FileCredentialStore::open` signature.
    pub fn open(path: impl AsRef<Path>, secrets: Arc<SecretRegistry>) -> Result<Self, BoundaryError> {
        Self::open_file(path, secrets)
    }

    /// Open a store: `backend` material store + `index` metadata directory.
    /// Only the `system` backend path uses this; `file` opens through `open_file`.
    #[allow(dead_code)]
    pub(crate) fn open_with(
        backend: Box<dyn MaterialBackend>,
        index_root: &Path,
        secrets: Arc<SecretRegistry>,
    ) -> Result<Self, BoundaryError> {
        let index = super::file::IndexDir::open(index_root, secrets.clone())?;
        Ok(Self {
            inner: Arc::new(Inner {
                backend,
                index,
                secrets,
            }),
        })
    }

    /// Default file-backend open — the previous `FileCredentialStore::open`.
    pub fn open_file(
        path: impl AsRef<Path>,
        secrets: Arc<SecretRegistry>,
    ) -> Result<Self, BoundaryError> {
        let index = super::file::IndexDir::open(path.as_ref(), secrets.clone())?;
        let backend = super::file::FileMaterialBackend::new(index.clone());
        Ok(Self {
            inner: Arc::new(Inner {
                backend: Box::new(backend),
                index,
                secrets,
            }),
        })
    }

    /// Select a backend per `[credentials] backend` config. `System` is an
    /// explicit opt-in; when unavailable the error is explicit, never a silent
    /// file fallback.
    pub fn open_backend(
        backend: CredentialBackend,
        state_dir: &Path,
        secrets: Arc<SecretRegistry>,
    ) -> Result<Self, BoundaryError> {
        let credentials_dir = state_dir.join("credentials");
        match backend {
            CredentialBackend::File => Self::open_file(&credentials_dir, secrets),
            CredentialBackend::System => Self::open_system(&credentials_dir, secrets),
        }
    }

    #[cfg(all(target_os = "macos", feature = "security-framework"))]
    fn open_system(index_root: &Path, secrets: Arc<SecretRegistry>) -> Result<Self, BoundaryError> {
        let backend = super::system::SystemMaterialBackend::new()?;
        Self::open_with(Box::new(backend), index_root, secrets)
    }
    #[cfg(not(all(target_os = "macos", feature = "security-framework")))]
    fn open_system(
        _index_root: &Path,
        _secrets: Arc<SecretRegistry>,
    ) -> Result<Self, BoundaryError> {
        Err(error(
            "credential_backend_unavailable",
            "System credential backend is not supported on this platform",
        ))
    }

    pub fn protected_path(&self) -> &Path {
        self.inner.index.root()
    }
    pub fn secret_registry(&self) -> Arc<SecretRegistry> {
        self.inner.secrets.clone()
    }
    pub fn backend_name(&self) -> &'static str {
        self.inner.backend.name()
    }

    pub async fn create(
        &self,
        scope: CredentialScope,
        value: CredentialValue,
    ) -> Result<CredentialRef, BoundaryError> {
        self.create_record(scope, value, None).await
    }
    pub async fn create_oauth(
        &self,
        scope: CredentialScope,
        credentials: rmcp::transport::auth::StoredCredentials,
        resource: String,
    ) -> Result<CredentialRef, BoundaryError> {
        let resource = scope.validate_oauth_resource(&resource)?;
        self.create_record(scope, CredentialValue::oauth(credentials), Some(resource))
            .await
    }
    async fn create_record(
        &self,
        scope: CredentialScope,
        value: CredentialValue,
        resource: Option<String>,
    ) -> Result<CredentialRef, BoundaryError> {
        scope.validate()?;
        value.validate_scope(&scope)?;
        self.inner.secrets.register(&value.secrets()?)?;
        self.inner.index.check_metadata_secrets(&scope)?;
        let record = MaterialRecord::from_public(&value)?;
        let inner = self.inner.clone();
        let is_file = inner.backend.name() == "file";
        tokio::task::spawn_blocking(move || {
            if is_file {
                // File backend: metadata + inline material in one atomic record.
                inner
                    .index
                    .create_with_material(&scope, value, resource.as_deref())
            } else {
                let metadata = inner
                    .index
                    .create_metadata(&scope, &value, resource.as_deref())?;
                inner.backend.put(metadata.reference, &record)?;
                Ok(metadata.reference)
            }
        })
        .await
        .map_err(|_| error("credential_io", "Credential worker did not complete"))?
    }

    pub async fn resolve(
        &self,
        reference: CredentialRef,
        scope: &CredentialScope,
    ) -> Result<ResolvedCredential, BoundaryError> {
        scope.validate()?;
        let scope = scope.clone();
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || Self::resolve_inner(&inner, reference, &scope))
            .await
            .map_err(|_| error("credential_io", "Credential worker did not complete"))?
    }

    /// Blocking resolve for callers already on a blocking thread (e.g. inside
    /// `spawn_blocking`). Identical semantics to `resolve`.
    pub fn resolve_sync(
        &self,
        reference: CredentialRef,
        scope: &CredentialScope,
    ) -> Result<ResolvedCredential, BoundaryError> {
        scope.validate()?;
        Self::resolve_inner(&self.inner, reference, scope)
    }

    fn resolve_inner(
        inner: &Arc<Inner>,
        reference: CredentialRef,
        scope: &CredentialScope,
    ) -> Result<ResolvedCredential, BoundaryError> {
        let record = inner.index.read_metadata(reference, scope)?;
        if record.metadata.revoked {
            return Err(error(
                "credential_revoked",
                "Credential reference has been revoked",
            ));
        }
        let material = inner.backend.get(reference)?;
        let value = material.into_public()?;
        value.validate_scope(scope)?;
        inner.secrets.register(&value.secrets()?)?;
        Ok(ResolvedCredential {
            metadata: record.metadata,
            value,
        })
    }

    /// Permanently revoke one reference: material removed, tombstone kept, any
    /// grants for it dropped. Old SDK refreshes cannot recreate the material.
    /// Returns how many grants were dropped with the revocation.
    pub async fn revoke(
        &self,
        reference: CredentialRef,
        scope: &CredentialScope,
    ) -> Result<usize, BoundaryError> {
        scope.validate()?;
        let scope = scope.clone();
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            inner.index.revoke_metadata(reference, &scope)?;
            // Material removal follows the durable tombstone.
            inner.backend.remove(reference)?;
            // A revoked reference can never resolve again, so its grants are
            // dead policy — drop them in the same critical section. A missing
            // grants directory (older stores) is not an error.
            let removed = match super::GrantStore::open(inner.index.root()) {
                Ok(grants) => grants.remove_for(reference)?,
                Err(_) => 0,
            };
            Ok(removed)
        })
        .await
        .map_err(|_| error("credential_io", "Credential worker did not complete"))?
    }

    /// Rotate a secret (non-OAuth) reference: replace material with a new value
    /// and bump the metadata revision. The reference stays stable — callers and
    /// `credential_ref` config keep working, while the *next* `resolve` yields
    /// the new secret and any still-cached old value fails to match (the old
    /// value remains registered in `SecretRegistry` for redaction).
    pub async fn rotate_secret(
        &self,
        reference: CredentialRef,
        scope: &CredentialScope,
        value: super::SecretValue,
    ) -> Result<u64, BoundaryError> {
        scope.validate()?;
        let scope = scope.clone();
        let inner = self.inner.clone();
        let new_value = CredentialValue::Secret(value);
        new_value.validate_scope(&scope)?;
        inner.secrets.register(&new_value.secrets()?)?;
        tokio::task::spawn_blocking(move || {
            let record = inner.index.read_metadata(reference, &scope)?;
            if record.metadata.revoked {
                return Err(error(
                    "credential_revoked",
                    "Credential reference has been revoked",
                ));
            }
            if record.metadata.kind != super::CredentialKind::Secret {
                return Err(error(
                    "credential_kind",
                    "Only secret credentials support in-place rotation; OAuth credentials rotate through refresh or a new login",
                ));
            }
            inner
                .backend
                .put(reference, &MaterialRecord::from_public(&new_value)?)?;
            inner.index.bump_revision(reference, &scope)
        })
        .await
        .map_err(|_| error("credential_io", "Credential worker did not complete"))?
    }

    pub async fn list(&self) -> Result<Vec<super::CredentialMetadata>, BoundaryError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.index.list_metadata())
            .await
            .map_err(|_| error("credential_io", "Credential worker did not complete"))?
    }

    /// Bind the MCP SDK store to exactly one active OAuth reference and scope.
    pub fn oauth(
        &self,
        reference: CredentialRef,
        scope: CredentialScope,
    ) -> Result<super::McpCredentialStore, BoundaryError> {
        scope.validate()?;
        self.inner.index.check_metadata_secrets(&scope)?;
        Ok(super::McpCredentialStore {
            store: self.clone(),
            reference,
            scope,
        })
    }

    /// Scoped-grant resolve: requires both the exact scope match and an
    /// applicable grant for `kind`/`tool`/`workspace` at `now`.
    pub async fn resolve_granted(
        &self,
        reference: CredentialRef,
        scope: &CredentialScope,
        grants: &super::GrantStore,
        kind: super::GrantRunKind,
        tool: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<ResolvedCredential, BoundaryError> {
        let now = super::grants::now_rfc3339();
        if !grants.check(reference, kind, tool, workspace, &now)? {
            return Err(error(
                "credential_grant_denied",
                "No applicable credential grant for this workspace, task kind, or tool",
            ));
        }
        self.resolve(reference, scope).await
    }

    /// Blocking variant of `resolve_granted` for callers already on a blocking
    /// thread (launch planning inside `spawn_blocking`-compatible paths).
    pub fn resolve_granted_sync(
        &self,
        reference: CredentialRef,
        scope: &CredentialScope,
        grants: &super::GrantStore,
        kind: super::GrantRunKind,
        tool: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<ResolvedCredential, BoundaryError> {
        scope.validate()?;
        let now = super::grants::now_rfc3339();
        if !grants.check(reference, kind, tool, workspace, &now)? {
            return Err(error(
                "credential_grant_denied",
                "No applicable credential grant for this workspace, task kind, or tool",
            ));
        }
        Self::resolve_inner(&self.inner, reference, scope)
    }

    pub(crate) fn inner(&self) -> &Arc<Inner> {
        &self.inner
    }
}

/// Compatibility alias: `FileCredentialStore` is the store with the file
/// backend selected; the type is the same under the generic name.
pub type FileCredentialStore = CredentialStore;

pub(crate) fn worker_error() -> BoundaryError {
    BoundaryError(
        ErrorInfo {
            code: "credential_io".into(),
            message: "Credential worker did not complete".into(),
        },
        None,
        None,
    )
}
