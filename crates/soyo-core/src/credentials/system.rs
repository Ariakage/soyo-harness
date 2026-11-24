//! macOS Keychain material backend (opt-in `[credentials] backend = "system"`).
//!
//! Secret material is stored as a generic-password item under a fixed service
//! name keyed by the credential reference UUID; metadata stays in the file
//! index (`IndexDir`), so revocation tombstones and scope checks behave exactly
//! like the file backend. Any Keychain failure is an explicit error — this
//! backend never falls back to plaintext.
//!
//! Item attributes: `service = "soyo.credentials"`, `account = <ref uuid>`,
//! `label = "Soyo credential <ref>"`, `data = JSON MaterialRecord`. The JSON
//! shape keeps OAuth `StoredCredentials` round-trippable without a second
//! serialisation contract.
#![cfg(all(target_os = "macos", feature = "security-framework"))]

use super::{
    CredentialRef,
    backend::{MaterialBackend, MaterialRecord},
    error,
};
use crate::contract::BoundaryError;
use security_framework::item::{
    ItemAddOptions, ItemAddValue, ItemClass, ItemSearchOptions, ItemUpdateOptions, ItemUpdateValue,
    Limit, SearchResult,
};
use std::sync::Mutex;

const SERVICE: &str = "soyo.credentials";

/// Serialise SecItem calls — the file keychain has no transaction model, and a
/// stale delete/add race is worse than a short mutex. Calls are quick.
pub(crate) struct SystemMaterialBackend {
    lock: Mutex<()>,
}

impl SystemMaterialBackend {
    pub(crate) fn new() -> Result<Self, BoundaryError> {
        // Probe the default keychain once so an unusable keychain fails at open
        // rather than first use. `ItemSearchOptions` with limit 0 performs a
        // metadata-only query; a refusal/error maps to explicit unavailable.
        let mut probe = ItemSearchOptions::new();
        probe.class(ItemClass::generic_password());
        probe.limit(Limit::Max(0));
        probe.search().map_err(|_| {
            error(
                "credential_backend_unavailable",
                "System credential backend (macOS Keychain) is unavailable",
            )
        })?;
        Ok(Self {
            lock: Mutex::new(()),
        })
    }

    fn backend_error(&self) -> BoundaryError {
        error(
            "credential_backend",
            "System credential backend operation failed",
        )
    }

    fn search(account: &str) -> ItemSearchOptions {
        let mut options = ItemSearchOptions::new();
        options.class(ItemClass::generic_password());
        options.service(SERVICE);
        options.account(account);
        options
    }
}

impl MaterialBackend for SystemMaterialBackend {
    fn name(&self) -> &'static str {
        "system"
    }

    fn put(&self, reference: CredentialRef, record: &MaterialRecord) -> Result<(), BoundaryError> {
        let bytes = serde_json::to_vec(record)
            .map_err(|_| error("credential_io", "Credential record could not be encoded"))?;
        let account = reference.to_string();
        let _guard = self.lock.lock().map_err(|_| self.backend_error())?;
        let mut probe = Self::search(&account);
        probe.limit(Limit::Max(1));
        let exists = !probe.search().map_err(|_| self.backend_error())?.is_empty();
        if exists {
            let mut update = ItemUpdateOptions::new();
            update.set_value(ItemUpdateValue::Data(
                core_foundation::data::CFData::from_buffer(&bytes),
            ));
            security_framework::item::update_item(&Self::search(&account), &update)
                .map_err(|_| self.backend_error())?;
        } else {
            let mut add = ItemAddOptions::new(ItemAddValue::Data {
                class: ItemClass::generic_password(),
                data: core_foundation::data::CFData::from_buffer(&bytes),
            });
            add.set_service(SERVICE);
            add.set_account_name(&account);
            add.set_label(format!("Soyo credential {account}"));
            add.add().map_err(|_| self.backend_error())?;
        }
        Ok(())
    }

    fn get(&self, reference: CredentialRef) -> Result<MaterialRecord, BoundaryError> {
        let account = reference.to_string();
        let _guard = self.lock.lock().map_err(|_| self.backend_error())?;
        let mut search = Self::search(&account);
        search.load_data(true);
        search.limit(Limit::Max(1));
        let results = search.search().map_err(|_| self.backend_error())?;
        let item = results
            .into_iter()
            .next()
            .ok_or_else(|| error("credential_not_found", "Credential reference was not found"))?;
        let SearchResult::Data(bytes) = item else {
            return Err(self.backend_error());
        };
        serde_json::from_slice(&bytes)
            .map_err(|_| error("credential_corrupt", "Credential record is malformed"))
    }

    fn remove(&self, reference: CredentialRef) -> Result<(), BoundaryError> {
        let account = reference.to_string();
        let _guard = self.lock.lock().map_err(|_| self.backend_error())?;
        // Not-found is a no-op so revoke stays idempotent.
        match security_framework::passwords::delete_generic_password(SERVICE, &account) {
            Ok(()) => Ok(()),
            Err(err) if err.code() == security_framework_sys::base::errSecItemNotFound => Ok(()),
            Err(_) => Err(self.backend_error()),
        }
    }
}
