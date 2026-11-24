//! File-backed metadata index and material backend.
//!
//! `IndexDir` owns the private 0700 root plus one `<ref>.json` metadata record
//! per credential (scope, kind, revision, revocation, OAuth resource). The
//! default `FileMaterialBackend` keeps material inline in the same record; a
//! `system` backend stores material in the OS credential store while reusing
//! this index for metadata — so scope checks, grants, revocation and listing
//! are identical across backends.

use super::{
    CredentialKind, CredentialMetadata, CredentialRef, CredentialScope, CredentialValue,
    SecretRegistry, SecretValue,
    backend::{MaterialBackend, MaterialRecord},
    error,
};
use crate::contract::BoundaryError;
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use rmcp::transport::auth::StoredCredentials;
use serde::{Deserialize, Serialize};
use std::fs::{DirBuilder, File};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

const MAX_RECORD_BYTES: usize = 1024 * 1024;
const MAX_RECORDS: usize = 256;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    version: u8,
    pub metadata: CredentialMetadata,
    /// File backend only: material stored inline. `None` on a revoked tombstone
    /// and on every record when a non-file backend owns the material.
    material: Option<MaterialRecord>,
}

/// Private root directory + metadata records. Shared by every backend because
/// scope/grant/revoke/list never depend on backend query capabilities.
/// `cap_std::fs::Dir` is not `Clone`; clones share the handle via `Arc`.
#[derive(Clone)]
pub(crate) struct IndexDir {
    inner: Arc<IndexDirInner>,
}

pub(crate) struct IndexDirInner {
    pub(super) dir: Dir,
    pub(super) secrets: Arc<SecretRegistry>,
    path: PathBuf,
}

impl std::fmt::Debug for IndexDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IndexDir([PRIVATE])")
    }
}

impl IndexDir {
    /// Open an owned private directory. Existing permissive directories and
    /// symlinks are refused.
    pub(in crate::credentials) fn open(
        path: &Path,
        secrets: Arc<SecretRegistry>,
    ) -> Result<Self, BoundaryError> {
        match DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error_value) if error_value.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => {
                return Err(error(
                    "credential_io",
                    "Credential directory could not be created",
                ));
            }
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| {
                error(
                    "credential_permissions",
                    "Credential root is unavailable or is a symlink",
                )
            })?;
        let metadata = file.metadata().map_err(|_| {
            error(
                "credential_io",
                "Credential directory metadata is unavailable",
            )
        })?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(error(
                "credential_permissions",
                "Credential directory must be owned by this user with mode 0700",
            ));
        }
        let canonical = std::fs::canonicalize(path).map_err(|_| {
            error(
                "credential_io",
                "Credential directory path cannot be resolved",
            )
        })?;
        let named = std::fs::metadata(&canonical).map_err(|_| {
            error(
                "credential_io",
                "Credential directory path metadata is unavailable",
            )
        })?;
        if named.dev() != metadata.dev() || named.ino() != metadata.ino() {
            return Err(error(
                "credential_permissions",
                "Credential directory changed while it was opened",
            ));
        }
        Ok(Self {
            inner: Arc::new(IndexDirInner {
                dir: Dir::from_std_file(file),
                secrets,
                path: canonical,
            }),
        })
    }

    pub(in crate::credentials) fn root(&self) -> &Path {
        &self.inner.path
    }

    fn references(&self) -> Result<Vec<CredentialRef>, BoundaryError> {
        let mut references = Vec::new();
        for entry in self
            .inner
            .dir
            .entries()
            .map_err(|_| error("credential_io", "Credential directory cannot be listed"))?
        {
            let name = entry
                .map_err(|_| error("credential_io", "Credential directory entry is unavailable"))?
                .file_name();
            let Some(name) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            let reference = name
                .parse::<CredentialRef>()
                .map_err(|_| error("credential_corrupt", "Credential filename is invalid"))?;
            references.push(reference);
            if references.len() > MAX_RECORDS {
                return Err(error(
                    "credential_limit",
                    "Credential directory exceeds its record capacity",
                ));
            }
        }
        references.sort();
        Ok(references)
    }

    pub(super) fn data_lock(&self, reference: CredentialRef) -> Result<FileLock, BoundaryError> {
        FileLock::acquire(&self.inner.dir, &format!("{reference}.lock"))
    }
    pub(super) fn refresh_lock(&self, reference: CredentialRef) -> Result<FileLock, BoundaryError> {
        FileLock::acquire(&self.inner.dir, &format!("{reference}.refresh.lock"))
    }

    pub(super) fn read_record(&self, reference: CredentialRef) -> Result<Record, BoundaryError> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let mut file = self
            .inner
            .dir
            .open_with(format!("{reference}.json"), &options)
            .map_err(|error_value| {
                if error_value.kind() == std::io::ErrorKind::NotFound {
                    error("credential_not_found", "Credential reference was not found")
                } else {
                    error(
                        "credential_permissions",
                        "Credential file is unavailable or is a symlink",
                    )
                }
            })?;
        private_file(&file)?;
        if file
            .metadata()
            .map_err(|_| error("credential_io", "Credential file metadata is unavailable"))?
            .len()
            > MAX_RECORD_BYTES as u64
        {
            return Err(error(
                "credential_corrupt",
                "Credential record exceeds its size limit",
            ));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_RECORD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| error("credential_io", "Credential record could not be read"))?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(error(
                "credential_corrupt",
                "Credential record exceeds its size limit",
            ));
        }
        let record: Record = serde_json::from_slice(&bytes)
            .map_err(|_| error("credential_corrupt", "Credential record is malformed"))?;
        record.metadata.scope.validate()?;
        if record.version != 1
            || record.metadata.reference != reference
            || record.metadata.revision == 0
            // File backend keeps `revoked == material.is_none()`; a non-file
            // backend writes metadata-only records, so the material flag is
            // authoritative only when material is present.
            || (record.metadata.revoked && record.material.is_some())
        {
            return Err(error(
                "credential_corrupt",
                "Credential record identity or state is invalid",
            ));
        }
        match (&record.metadata.kind, &record.material) {
            (_, None)
            | (CredentialKind::Secret, Some(MaterialRecord::Secret { .. }))
            | (CredentialKind::OAuth, Some(MaterialRecord::OAuth { .. })) => {}
            _ => {
                return Err(error(
                    "credential_corrupt",
                    "Credential material and metadata kinds do not match",
                ));
            }
        }
        if let Some(material) = &record.material {
            let values = match material {
                MaterialRecord::Secret { value } => {
                    CredentialValue::Secret(SecretValue::new(value.clone())?).secrets()?
                }
                MaterialRecord::OAuth { credentials } => {
                    let value = CredentialValue::OAuth(credentials.clone());
                    value.validate_scope(&record.metadata.scope)?;
                    value.secrets()?
                }
            };
            self.inner.secrets.register(&values)?;
        }
        self.check_record_metadata(&record.metadata)?;
        Ok(record)
    }

    /// Metadata read for a scope-bound resolve: lock, scope equality.
    pub(in crate::credentials) fn read_metadata(
        &self,
        reference: CredentialRef,
        scope: &CredentialScope,
    ) -> Result<Record, BoundaryError> {
        let _lock = self.data_lock(reference)?;
        let record = self.read_record(reference)?;
        check_scope(&record, scope)?;
        Ok(record)
    }

    /// Create the metadata record for a fresh reference (material written by
    /// the caller via the backend afterwards). Returns the new metadata.
    pub(in crate::credentials) fn create_metadata(
        &self,
        scope: &CredentialScope,
        value: &CredentialValue,
        resource: Option<&str>,
    ) -> Result<CredentialMetadata, BoundaryError> {
        let _index = FileLock::acquire(&self.inner.dir, "index.lock")?;
        if self.references()?.len() >= MAX_RECORDS {
            return Err(error(
                "credential_limit",
                "Credential record capacity has been reached",
            ));
        }
        let reference = CredentialRef(Uuid::now_v7());
        let kind = match value {
            CredentialValue::Secret(_) => CredentialKind::Secret,
            CredentialValue::OAuth(_) => CredentialKind::OAuth,
        };
        let oauth_resource = if kind == CredentialKind::OAuth {
            Some(resource.unwrap_or(scope.audience.as_str()).to_owned())
        } else {
            None
        };
        let metadata = CredentialMetadata {
            reference,
            scope: scope.clone(),
            kind,
            revision: 1,
            revoked: false,
            oauth_resource,
        };
        let record = Record {
            version: 1,
            metadata,
            material: None,
        };
        self.check_record_metadata(&record.metadata)?;
        self.write_record(&record)?;
        Ok(record.metadata)
    }

    /// File-backend create: metadata + inline material in one atomic record.
    pub(in crate::credentials) fn create_with_material(
        &self,
        scope: &CredentialScope,
        value: CredentialValue,
        resource: Option<&str>,
    ) -> Result<CredentialRef, BoundaryError> {
        let _index = FileLock::acquire(&self.inner.dir, "index.lock")?;
        if self.references()?.len() >= MAX_RECORDS {
            return Err(error(
                "credential_limit",
                "Credential record capacity has been reached",
            ));
        }
        let reference = CredentialRef(Uuid::now_v7());
        let kind = match &value {
            CredentialValue::Secret(_) => CredentialKind::Secret,
            CredentialValue::OAuth(_) => CredentialKind::OAuth,
        };
        let oauth_resource = if kind == CredentialKind::OAuth {
            Some(resource.unwrap_or(scope.audience.as_str()).to_owned())
        } else {
            None
        };
        let record = Record {
            version: 1,
            metadata: CredentialMetadata {
                reference,
                scope: scope.clone(),
                kind,
                revision: 1,
                revoked: false,
                oauth_resource,
            },
            material: Some(MaterialRecord::from_public(&value)?),
        };
        self.check_record_metadata(&record.metadata)?;
        self.write_record(&record)?;
        Ok(reference)
    }

    /// Mark the record revoked (drop any inline material) and bump the
    /// revision. Idempotent: re-revoking only rewrites when state changed.
    pub(in crate::credentials) fn revoke_metadata(
        &self,
        reference: CredentialRef,
        scope: &CredentialScope,
    ) -> Result<(), BoundaryError> {
        let _refresh = self.refresh_lock(reference)?;
        let _data = self.data_lock(reference)?;
        let mut record = self.read_record(reference)?;
        check_scope(&record, scope)?;
        if let Some(material) = record.material.take() {
            self.inner
                .secrets
                .register(&material.into_public()?.secrets()?)?;
        }
        if !record.metadata.revoked {
            record.metadata.revoked = true;
            record.metadata.revision = next_revision(record.metadata.revision)?;
        }
        self.write_record(&record)?;
        Ok(())
    }

    /// Bump the revision for a rotate. Scope must match; record must be active.
    pub(in crate::credentials) fn bump_revision(
        &self,
        reference: CredentialRef,
        scope: &CredentialScope,
    ) -> Result<u64, BoundaryError> {
        let _data = self.data_lock(reference)?;
        let mut record = self.read_record(reference)?;
        check_scope(&record, scope)?;
        record.metadata.revision = next_revision(record.metadata.revision)?;
        self.write_record(&record)?;
        Ok(record.metadata.revision)
    }

    /// List only identities and scope metadata, including tombstones.
    pub(in crate::credentials) fn list_metadata(
        &self,
    ) -> Result<Vec<CredentialMetadata>, BoundaryError> {
        let _index = FileLock::acquire(&self.inner.dir, "index.lock")?;
        let mut result = Vec::new();
        for reference in self.references()? {
            let _lock = self.data_lock(reference)?;
            result.push(self.read_record(reference)?.metadata);
        }
        // Later records can reveal values that also occur in earlier metadata.
        for metadata in &result {
            self.check_record_metadata(metadata)?;
        }
        Ok(result)
    }

    fn check_record_metadata(&self, metadata: &CredentialMetadata) -> Result<(), BoundaryError> {
        self.check_metadata_secrets(&metadata.scope)?;
        if let Some(resource) = &metadata.oauth_resource {
            if metadata.kind != CredentialKind::OAuth {
                return Err(error(
                    "credential_corrupt",
                    "Bearer credential contains OAuth resource metadata",
                ));
            }
            metadata.scope.validate_oauth_resource(resource)?;
        }
        let encoded = serde_json::to_string(metadata)
            .map_err(|_| error("credential_scope", "Credential metadata cannot be encoded"))?;
        if crate::redaction::contains_secret(&encoded, &self.inner.secrets.snapshot()?) {
            return Err(error(
                "credential_scope",
                "Credential metadata contains a known credential value",
            ));
        }
        Ok(())
    }

    pub(in crate::credentials) fn check_metadata_secrets(
        &self,
        scope: &CredentialScope,
    ) -> Result<(), BoundaryError> {
        let encoded = serde_json::to_string(scope)
            .map_err(|_| error("credential_scope", "Credential scope cannot be encoded"))?;
        if crate::redaction::contains_secret(&encoded, &self.inner.secrets.snapshot()?) {
            return Err(error(
                "credential_scope",
                "Credential scope metadata contains a known credential value",
            ));
        }
        Ok(())
    }

    fn write_record(&self, record: &Record) -> Result<(), BoundaryError> {
        let bytes = serde_json::to_vec(record)
            .map_err(|_| error("credential_io", "Credential record could not be encoded"))?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(error(
                "credential_limit",
                "Credential record exceeds its size limit",
            ));
        }
        let temporary = format!(".tmp-{}", Uuid::now_v7());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let outcome = (|| {
            let mut file = self
                .inner
                .dir
                .open_with(&temporary, &options)
                .map_err(|_| {
                    error(
                        "credential_io",
                        "Private credential temporary file could not be created",
                    )
                })?;
            private_file(&file)?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| error("credential_io", "Credential record could not be saved"))?;
            self.inner
                .dir
                .rename(
                    &temporary,
                    &self.inner.dir,
                    format!("{}.json", record.metadata.reference),
                )
                .map_err(|_| error("credential_io", "Credential record replacement failed"))?;
            self.sync_directory()?;
            Ok(())
        })();
        if outcome.is_err() {
            let _ = self.inner.dir.remove_file(&temporary);
        }
        outcome
    }

    fn sync_directory(&self) -> Result<(), BoundaryError> {
        self.inner
            .dir
            .try_clone()
            .map_err(|_| error("credential_io", "Credential directory could not be cloned"))?
            .into_std_file()
            .sync_all()
            .map_err(|_| {
                error(
                    "credential_io",
                    "Credential commit durability could not be confirmed",
                )
            })
    }

    /// Write material into an existing active record (OAuth refresh path for
    /// the file backend).
    pub(super) fn write_inline_material(
        &self,
        reference: CredentialRef,
        material: MaterialRecord,
        bump: bool,
    ) -> Result<(), BoundaryError> {
        let mut record = self.read_record(reference)?;
        record.material = Some(material);
        if bump {
            record.metadata.revision = next_revision(record.metadata.revision)?;
        }
        self.write_record(&record)
    }

    // ---- OAuth refresh paths (file backend inline material) ----------------

    pub(super) fn load_oauth(
        &self,
        reference: CredentialRef,
        scope: &CredentialScope,
    ) -> Result<Option<StoredCredentials>, BoundaryError> {
        let _lock = self.data_lock(reference)?;
        let record = self.read_record(reference)?;
        check_scope(&record, scope)?;
        if record.metadata.revoked {
            return Ok(None);
        }
        match record.material {
            Some(MaterialRecord::OAuth { credentials }) => {
                self.inner
                    .secrets
                    .register(&CredentialValue::OAuth(credentials.clone()).secrets()?)?;
                Ok(Some(*credentials))
            }
            _ => Err(error(
                "credential_kind",
                "Credential reference does not contain OAuth material",
            )),
        }
    }

    pub(super) fn save_oauth(
        &self,
        reference: CredentialRef,
        scope: &CredentialScope,
        credentials: StoredCredentials,
    ) -> Result<(), BoundaryError> {
        let _lock = self.data_lock(reference)?;
        let mut record = self.read_record(reference)?;
        check_scope(&record, scope)?;
        if record.metadata.revoked {
            return Err(error(
                "credential_revoked",
                "Revoked credentials cannot be restored by a refresh",
            ));
        }
        let Some(MaterialRecord::OAuth {
            credentials: old, ..
        }) = record.material.as_ref()
        else {
            return Err(error(
                "credential_kind",
                "Credential reference does not contain OAuth material",
            ));
        };
        if (!old.client_id.is_empty() && old.client_id != credentials.client_id)
            || old
                .issuer
                .as_ref()
                .is_some_and(|issuer| Some(issuer) != credentials.issuer.as_ref())
        {
            return Err(error(
                "credential_scope",
                "OAuth client or issuer differs from the bound credential",
            ));
        }
        let mut known = CredentialValue::OAuth(old.clone()).secrets()?;
        known.extend(CredentialValue::oauth(credentials.clone()).secrets()?);
        self.inner.secrets.register(&known)?;
        CredentialValue::oauth(credentials.clone()).validate_scope(scope)?;
        self.check_record_metadata(&record.metadata)?;
        record.material = Some(MaterialRecord::OAuth {
            credentials: Box::new(credentials),
        });
        record.metadata.revision = next_revision(record.metadata.revision)?;
        self.write_record(&record)
    }

    pub(super) fn reject_oauth(
        &self,
        reference: CredentialRef,
        scope: &CredentialScope,
        rejected: &StoredCredentials,
    ) -> Result<(), BoundaryError> {
        let _lock = self.data_lock(reference)?;
        let mut record = self.read_record(reference)?;
        check_scope(&record, scope)?;
        if record.metadata.revoked {
            return self.write_record(&record);
        }
        let Some(MaterialRecord::OAuth {
            credentials: current,
            ..
        }) = record.material.as_ref()
        else {
            return Err(error(
                "credential_kind",
                "Credential reference does not contain OAuth material",
            ));
        };
        let encoded_current = serde_json::to_vec(current).map_err(|_| {
            error(
                "credential_corrupt",
                "Stored OAuth credential cannot be compared",
            )
        })?;
        let encoded_rejected = serde_json::to_vec(rejected).map_err(|_| {
            error(
                "credential_corrupt",
                "Rejected OAuth credential cannot be compared",
            )
        })?;
        if encoded_current != encoded_rejected {
            return Ok(());
        }
        self.inner
            .secrets
            .register(&CredentialValue::OAuth(current.clone()).secrets()?)?;
        record.material = None;
        record.metadata.revoked = true;
        record.metadata.revision = next_revision(record.metadata.revision)?;
        self.write_record(&record)
    }
}

/// The file `MaterialBackend`: material is inline in the index record, so
/// `put`/`get`/`remove` map to record reads/writes under the data lock.
pub(crate) struct FileMaterialBackend {
    index: IndexDir,
}

impl FileMaterialBackend {
    pub(crate) fn new(index: IndexDir) -> Self {
        Self { index }
    }
}

impl MaterialBackend for FileMaterialBackend {
    fn name(&self) -> &'static str {
        "file"
    }
    fn put(&self, reference: CredentialRef, record: &MaterialRecord) -> Result<(), BoundaryError> {
        let _lock = self.index.data_lock(reference)?;
        self.index
            .write_inline_material(reference, record.clone(), false)
    }
    fn get(&self, reference: CredentialRef) -> Result<MaterialRecord, BoundaryError> {
        let _lock = self.index.data_lock(reference)?;
        let record = self.index.read_record(reference)?;
        record.material.clone().ok_or_else(|| {
            error(
                "credential_not_found",
                "Credential reference has no material",
            )
        })
    }
    fn remove(&self, reference: CredentialRef) -> Result<(), BoundaryError> {
        let _lock = self.index.data_lock(reference)?;
        let mut record = self.index.read_record(reference)?;
        if record.material.take().is_some() {
            self.index.write_record(&record)?;
        }
        Ok(())
    }
}

fn check_scope(record: &Record, scope: &CredentialScope) -> Result<(), BoundaryError> {
    if &record.metadata.scope != scope {
        return Err(error(
            "credential_scope",
            "Credential reference is bound to a different server, audience, or scope",
        ));
    }
    Ok(())
}
fn next_revision(revision: u64) -> Result<u64, BoundaryError> {
    revision.checked_add(1).ok_or_else(|| {
        error(
            "credential_limit",
            "Credential revision capacity has been reached",
        )
    })
}

fn private_file(file: &cap_std::fs::File) -> Result<(), BoundaryError> {
    use cap_std::fs::MetadataExt as _;
    let metadata = file
        .metadata()
        .map_err(|_| error("credential_io", "Credential file metadata is unavailable"))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(error(
            "credential_permissions",
            "Credential files must be private, owned regular files without hard links",
        ));
    }
    Ok(())
}

pub(super) struct FileLock {
    file: File,
}
impl FileLock {
    fn acquire(dir: &Dir, name: &str) -> Result<Self, BoundaryError> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let file = dir.open_with(name, &options).map_err(|_| {
            error(
                "credential_permissions",
                "Credential lock file could not be opened",
            )
        })?;
        private_file(&file)?;
        let file = file.into_std();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // SAFETY: the owned descriptor stays live through this call and guard lifetime.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self { file });
            }
            let cause = std::io::Error::last_os_error();
            if cause.kind() != std::io::ErrorKind::WouldBlock
                && cause.kind() != std::io::ErrorKind::Interrupted
            {
                return Err(error("credential_io", "Credential lock acquisition failed"));
            }
            if Instant::now() >= deadline {
                return Err(error(
                    "credential_busy",
                    "Credential lock acquisition timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor belongs to this live guard.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}
