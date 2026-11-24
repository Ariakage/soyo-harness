use super::{CredentialRef, CredentialScope, CredentialStore, backend::worker_error};
use rmcp::transport::auth::{
    AuthError, CredentialRefreshGuard, CredentialStore as SdkCredentialStore, StoredCredentials,
};

/// Resource-scoped SDK store. Refreshes use a separate cross-process lock from record I/O.
#[derive(Clone)]
pub struct McpCredentialStore {
    pub(super) store: CredentialStore,
    pub(super) reference: CredentialRef,
    pub(super) scope: CredentialScope,
}

impl std::fmt::Debug for McpCredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpCredentialStore")
            .field("reference", &self.reference)
            .finish()
    }
}
impl McpCredentialStore {
    pub(crate) async fn oauth_resource(&self) -> Result<String, crate::contract::BoundaryError> {
        let resolved = self.store.resolve(self.reference, &self.scope).await?;
        Ok(resolved
            .metadata
            .oauth_resource
            .unwrap_or_else(|| self.scope.audience.clone()))
    }
}

fn auth_error(error: crate::contract::BoundaryError) -> AuthError {
    AuthError::CredentialStoreError(error.0.code)
}

#[async_trait::async_trait]
impl SdkCredentialStore for McpCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let reference = self.reference;
        let scope = self.scope.clone();
        let inner = self.store.inner().clone();
        tokio::task::spawn_blocking(move || inner.index.load_oauth(reference, &scope))
            .await
            .map_err(|_| auth_error(worker_error()))?
            .map_err(auth_error)
    }
    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let reference = self.reference;
        let scope = self.scope.clone();
        let inner = self.store.inner().clone();
        tokio::task::spawn_blocking(move || inner.index.save_oauth(reference, &scope, credentials))
            .await
            .map_err(|_| auth_error(worker_error()))?
            .map_err(auth_error)
    }
    async fn clear(&self) -> Result<(), AuthError> {
        self.store
            .revoke(self.reference, &self.scope)
            .await
            .map_err(auth_error)
            .map(|_| ())
    }
    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        let reference = self.reference;
        let inner = self.store.inner().clone();
        tokio::task::spawn_blocking(move || inner.index.refresh_lock(reference))
            .await
            .map_err(|_| auth_error(worker_error()))?
            .map(|guard| Some(CredentialRefreshGuard::new(guard)))
            .map_err(auth_error)
    }
    async fn on_refresh_token_rejected(
        &self,
        credentials: &StoredCredentials,
    ) -> Result<(), AuthError> {
        let reference = self.reference;
        let scope = self.scope.clone();
        let credentials = credentials.clone();
        let inner = self.store.inner().clone();
        tokio::task::spawn_blocking(move || {
            inner.index.reject_oauth(reference, &scope, &credentials)
        })
        .await
        .map_err(|_| auth_error(worker_error()))?
        .map_err(auth_error)
    }
}
