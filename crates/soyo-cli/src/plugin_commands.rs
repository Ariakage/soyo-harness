//! Explicit local plugin administration. The host owns printing and exit codes.
//! All filesystem work runs on a blocking worker. No command starts plugin code.
use clap::{Subcommand, ValueEnum};
use serde_json::{Value, json};
use soyo_core::{
    config::Config,
    contract::{BoundaryError, ErrorInfo},
    credentials::SecretRegistry,
    plugin_manifest::{
        ArchiveFormat, ArchiveVerification, MANIFEST_NAME, PackageSource, SourceKind,
        VerifiedPackage, fetch_https_archive, inspect_archive_path, inspect_package,
        parse_manifest_with_secrets,
    },
    plugin_store::{
        EnableGrant, InstanceState, LeaseRecord, PluginScope, PluginStore, PluginSummary,
        StoreLimits,
    },
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

const PAGE_SIZE: usize = 20;
const SCOPE_PREVIEW: usize = 4;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, ValueEnum)]
pub enum PluginScopeOption {
    Workspace,
    User,
}

#[derive(Clone, Subcommand)]
pub enum PluginAction {
    /// Validate and seal a local directory, archive, or verified HTTPS archive without installing it.
    Validate {
        #[arg(value_name = "PATH", conflicts_with = "url")]
        path: Option<PathBuf>,
        #[arg(long, requires = "verify", value_name = "HTTPS_URL")]
        url: Option<String>,
        #[arg(long, group = "verify")]
        sha256: Option<String>,
        #[arg(long)]
        pubkey: Option<String>,
        #[arg(long, group = "verify", requires = "pubkey")]
        signature: Option<String>,
    },
    /// Install a local directory, archive, or verified HTTPS package in the disabled state.
    Install {
        #[arg(long, conflicts_with = "url")]
        path: Option<PathBuf>,
        #[arg(long, requires = "verify", value_name = "HTTPS_URL")]
        url: Option<String>,
        #[arg(long, group = "verify")]
        sha256: Option<String>,
        #[arg(long)]
        pubkey: Option<String>,
        #[arg(long, group = "verify", requires = "pubkey")]
        signature: Option<String>,
    },
    /// List recorded package identities, requested ceilings, scopes, and leases.
    List {
        #[arg(long, default_value_t = 1)]
        page: u32,
        #[arg(long, default_value_t = PAGE_SIZE)]
        page_size: usize,
    },
    /// Revalidate installed bytes and inspect their manifest and fixed lock.
    Inspect {
        plugin: String,
        #[arg(long)]
        digest: Option<String>,
    },
    /// Explicitly enable this digest with its complete declared capability ceiling.
    Enable {
        plugin: String,
        #[arg(long)]
        digest: String,
        #[arg(long, value_enum, default_value = "workspace")]
        scope: PluginScopeOption,
    },
    /// Stop new scheduling in this scope and request owned lease cleanup.
    Disable {
        plugin: String,
        #[arg(long, value_enum, default_value = "workspace")]
        scope: PluginScopeOption,
    },
    /// Select new local or verified remote content using the expected current digest. Enable it separately.
    Update {
        plugin: String,
        #[arg(long, conflicts_with = "url")]
        path: Option<PathBuf>,
        #[arg(long, requires = "verify", value_name = "HTTPS_URL")]
        url: Option<String>,
        #[arg(long, group = "verify")]
        sha256: Option<String>,
        #[arg(long)]
        pubkey: Option<String>,
        #[arg(long, group = "verify", requires = "pubkey")]
        signature: Option<String>,
        #[arg(long)]
        expected_digest: String,
    },
    /// Remove an exact digest after its fixed dependencies and leases permit cleanup.
    Uninstall {
        plugin: String,
        #[arg(long)]
        digest: String,
    },
    /// Observe orphaned leases and finish safe removals without replaying workers.
    Recover {
        #[arg(long, default_value_t = 1)]
        page: u32,
        #[arg(long, default_value_t = PAGE_SIZE)]
        page_size: usize,
    },
}
impl PluginAction {
    fn name(&self) -> &'static str {
        match self {
            Self::Validate { .. } => "validate",
            Self::Install { .. } => "install",
            Self::List { .. } => "list",
            Self::Inspect { .. } => "inspect",
            Self::Enable { .. } => "enable",
            Self::Disable { .. } => "disable",
            Self::Update { .. } => "update",
            Self::Uninstall { .. } => "uninstall",
            Self::Recover { .. } => "recover",
        }
    }
}
fn failure(code: &'static str, message: &'static str) -> BoundaryError {
    BoundaryError(
        ErrorInfo {
            code: code.into(),
            message: message.into(),
        },
        None,
        None,
    )
}
fn features() -> BTreeSet<String> {
    [
        "plugin-manifest-v1",
        "skill-directory-v1",
        "mcp-stdio-v1",
        "mcp-http-v1",
        "hook-events-v1",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}
fn check_id(id: &str) -> Result<(), BoundaryError> {
    if id.len() > 128
        || !id.contains('.')
        || !id.split('.').all(|label| {
            label.split('-').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            })
        })
    {
        return Err(failure("invalid_args", "Plugin ID is invalid"));
    }
    Ok(())
}
fn check_digest(digest: &str) -> Result<(), BoundaryError> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(failure(
            "invalid_args",
            "Plugin digest must be a complete lowercase SHA-256 identity",
        ));
    }
    Ok(())
}
fn known_registry(config: &Config) -> Result<Arc<SecretRegistry>, BoundaryError> {
    let values = config
        .secret_env_names()
        .iter()
        .map(|name| config.secret(name).map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()?;
    let registry = Arc::new(SecretRegistry::default());
    registry.register(&values)?;
    Ok(registry)
}
fn safe_output(mut value: Value, secrets: &SecretRegistry) -> Result<Value, BoundaryError> {
    let known = secrets.snapshot()?;
    let before = serde_json::to_vec(&value).map_err(|_| {
        failure(
            "output_error",
            "Plugin administration output cannot be encoded",
        )
    })?;
    if before.len() > MAX_OUTPUT_BYTES {
        return Err(failure(
            "plugin_output_limit",
            "Plugin action output exceeds its limit; inspect current state using a smaller page",
        ));
    }
    soyo_core::redaction::redact(&mut value, &known)?;
    let encoded = serde_json::to_string(&value).map_err(|_| {
        failure(
            "output_error",
            "Plugin administration output cannot be encoded",
        )
    })?;
    if encoded.len() > MAX_OUTPUT_BYTES || soyo_core::redaction::contains_secret(&encoded, &known) {
        return Err(failure(
            "sensitive_data",
            "Plugin administration output was withheld",
        ));
    }
    Ok(value)
}
fn ensure_clean(value: &Value, secrets: &SecretRegistry) -> Result<(), BoundaryError> {
    if soyo_core::redaction::contains_secret(&value.to_string(), &secrets.snapshot()?) {
        return Err(failure(
            "sensitive_data",
            "Plugin administration metadata contains protected data",
        ));
    }
    Ok(())
}
fn verification_from(
    sha256: &Option<String>,
    pubkey: &Option<String>,
    signature: &Option<String>,
) -> ArchiveVerification {
    ArchiveVerification {
        sha256: sha256.clone(),
        ed25519_pubkey: pubkey.clone(),
        ed25519_signature: signature.clone(),
    }
}

fn check_manifest_secrets(
    package: &VerifiedPackage,
    secrets: &SecretRegistry,
) -> Result<(), BoundaryError> {
    let manifest = package
        .file(MANIFEST_NAME)
        .ok_or_else(|| failure("invalid_plugin", "Sealed package manifest is missing"))?;
    let manifest = std::str::from_utf8(manifest.bytes())
        .map_err(|_| failure("invalid_plugin", "Sealed package manifest is not UTF-8"))?;
    let snapshot = secrets.snapshot()?;
    if soyo_core::redaction::contains_secret(manifest, &snapshot) {
        return Err(failure(
            "sensitive_data",
            "Plugin manifest contains protected data",
        ));
    }
    parse_manifest_with_secrets(manifest, &snapshot)?;
    Ok(())
}

fn input_support_label(kind: &SourceKind) -> &'static str {
    match kind {
        SourceKind::LocalDirectory => "local_directory",
        SourceKind::Archive => "archive",
        SourceKind::Https => "https_archive",
    }
}

fn local_package(
    path: &Path,
    verification: &ArchiveVerification,
    config: &Config,
    secrets: &SecretRegistry,
) -> Result<(VerifiedPackage, PackageSource), BoundaryError> {
    let text = path
        .to_str()
        .filter(|text| {
            !text.is_empty()
                && text.len() <= 4096
                && !text.chars().any(soyo_core::text::is_terminal_control)
        })
        .ok_or_else(|| {
            failure(
                "plugin_source_unsupported",
                "Only bounded local directory or archive package paths are supported",
            )
        })?;
    if text.contains("://") || text.starts_with("git@") {
        return Err(failure(
            "plugin_source_unsupported",
            "Remote URLs must be supplied with --url and cryptographic verification",
        ));
    }
    let absolute = config.cwd.join(path);
    let metadata = std::fs::symlink_metadata(&absolute).map_err(|_| {
        failure(
            "plugin_source_unsupported",
            "Local package path is unavailable",
        )
    })?;
    let (package, kind) = if metadata.is_dir() {
        if verification.sha256.is_some()
            || verification.ed25519_pubkey.is_some()
            || verification.ed25519_signature.is_some()
        {
            return Err(failure(
                "invalid_args",
                "Checksum and signature flags require a plugin archive input",
            ));
        }
        (inspect_package(&absolute)?, SourceKind::LocalDirectory)
    } else if metadata.is_file() {
        if ArchiveFormat::detect(&absolute).is_none() {
            return Err(failure(
                "plugin_source_unsupported",
                "Only local directories and .tar, .tar.gz, or .tgz plugin archives are supported",
            ));
        }
        (
            inspect_archive_path(&absolute, verification)?,
            SourceKind::Archive,
        )
    } else {
        return Err(failure(
            "plugin_source_unsupported",
            "Only local directories and regular archive files are supported",
        ));
    };
    check_manifest_secrets(&package, secrets)?;
    let canonical = std::fs::canonicalize(&absolute).map_err(|_| {
        failure(
            "plugin_source_unsupported",
            "Local package provenance cannot be resolved",
        )
    })?;
    let source = PackageSource {
        kind,
        reference: canonical
            .to_str()
            .ok_or_else(|| {
                failure(
                    "plugin_source_unsupported",
                    "Local package provenance is not UTF-8",
                )
            })?
            .into(),
    };
    source.validate()?;
    ensure_clean(&json!({"source":source}), secrets)?;
    Ok((package, source))
}
fn open_store(config: &Config, secrets: Arc<SecretRegistry>) -> Result<PluginStore, BoundaryError> {
    soyo_app::session::check_roots(config, &config.cwd, config.mode).map_err(|_| {
        failure(
            "plan_read_only",
            "PLAN plugin storage must be outside the workspace",
        )
    })?;
    std::fs::create_dir_all(&config.data_dir)
        .map_err(|_| failure("plugin_io", "Plugin data directory cannot be created"))?;
    PluginStore::open(
        config.data_dir.join("plugins"),
        secrets,
        StoreLimits::default(),
    )
}
fn store_exists(config: &Config) -> Result<bool, BoundaryError> {
    match std::fs::symlink_metadata(config.data_dir.join("plugins")) {
        Ok(_) => Ok(true),
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(failure("plugin_io", "Plugin store cannot be inspected")),
    }
}
fn scope(config: &Config, selected: PluginScopeOption) -> Result<PluginScope, BoundaryError> {
    match selected {
        PluginScopeOption::User => Ok(PluginScope::User),
        PluginScopeOption::Workspace => PluginScope::workspace(&config.cwd),
    }
}
fn range(page: u32, size: usize, count: usize) -> Result<std::ops::Range<usize>, BoundaryError> {
    if size == 0 || size > PAGE_SIZE {
        return Err(failure(
            "invalid_args",
            "Plugin page size must be between one and twenty",
        ));
    }
    let start = page
        .checked_sub(1)
        .and_then(|page| (page as usize).checked_mul(size))
        .ok_or_else(|| {
            failure(
                "invalid_args",
                "Plugin page must be a positive bounded integer",
            )
        })?;
    Ok(start.min(count)..start.saturating_add(size).min(count))
}
fn health(leases: &[LeaseRecord]) -> Value {
    let count = |state| leases.iter().filter(|lease| lease.state == state).count();
    json!({"running":count(InstanceState::Running),"stop_requested":count(InstanceState::StopRequested),
        "unknown":count(InstanceState::Unknown),"cleaned":count(InstanceState::Cleaned)})
}
fn summary(summary: &PluginSummary) -> Value {
    let scopes: Vec<_> = summary
        .enabled_scopes
        .iter()
        .filter(|scope| scope.enabled)
        .take(SCOPE_PREVIEW)
        .collect();
    let enabled = summary
        .enabled_scopes
        .iter()
        .filter(|scope| scope.enabled)
        .count();
    json!({"plugin_id":summary.plugin_id,"version":summary.version,"digest":summary.digest,"source":summary.source,
        "capability_fingerprint":summary.capability_fingerprint,"requested_capabilities":summary.requested_capabilities,
        "installation_state":summary.state,"selected":summary.selected,"source_conflict":summary.source_conflict,
        "enabled_scopes":scopes,"enabled_scope_count":enabled,"enabled_scopes_omitted":enabled.saturating_sub(scopes.len()),
        "disabled_scope_count":summary.enabled_scopes.iter().filter(|scope| !scope.enabled).count(),
        "lease_health":health(&summary.instances),"cleanup_pending":summary.instances.iter().any(|lease| matches!(lease.state,InstanceState::StopRequested|InstanceState::Unknown))})
}
fn lease(lease: &LeaseRecord) -> Value {
    json!({"lease_id":lease.lease_id,"plugin_id":lease.plugin_id,"digest":lease.digest,"dependency_digests":lease.dependency_digests,
        "scope":lease.scope,"enable_scope":lease.enable_scope,"session_id":lease.session_id,"run_id":lease.run_id,"state":lease.state})
}
fn pending(leases: &[LeaseRecord], page: u32, size: usize) -> Result<Value, BoundaryError> {
    let slice = range(page, size, leases.len())?;
    Ok(
        json!({"total":leases.len(),"page":page,"page_size":size,"items":leases[slice].iter().map(lease).collect::<Vec<_>>(),"lease_health":health(leases)}),
    )
}
fn selected_digest(
    store: &PluginStore,
    plugin: &str,
    digest: Option<&str>,
) -> Result<String, BoundaryError> {
    check_id(plugin)?;
    if let Some(digest) = digest {
        check_digest(digest)?;
        return Ok(digest.into());
    }
    store
        .list()?
        .into_iter()
        .find(|summary| summary.plugin_id == plugin && summary.selected)
        .map(|summary| summary.digest)
        .ok_or_else(|| {
            failure(
                "plugin_not_found",
                "Selected plugin was not found; specify an installed digest",
            )
        })
}
fn inspect_id(
    store: &PluginStore,
    plugin: &str,
    digest: &str,
) -> Result<soyo_core::plugin_store::PluginInspection, BoundaryError> {
    check_id(plugin)?;
    check_digest(digest)?;
    let inspection = store.inspect(digest)?;
    if inspection.summary.plugin_id != plugin {
        return Err(failure(
            "plugin_identity",
            "Plugin ID and content digest do not match",
        ));
    }
    Ok(inspection)
}
fn compatibility(manifest: &soyo_core::plugin_manifest::Manifest) -> Result<Value, BoundaryError> {
    let report = manifest.compatibility_report(env!("CARGO_PKG_VERSION"), &features())?;
    Ok(
        json!({"host_version":env!("CARGO_PKG_VERSION"),"host_features":features(),"compatible":report.is_compatible(),
        "version_matches":report.version_matches,"missing_features":report.missing_features}),
    )
}

fn resolve_sync_source(
    path: Option<&PathBuf>,
    url: Option<&str>,
    verification: &ArchiveVerification,
    prefetched: Option<(VerifiedPackage, PackageSource)>,
    config: &Config,
    secrets: &SecretRegistry,
) -> Result<(VerifiedPackage, PackageSource), BoundaryError> {
    if let Some(resolved) = prefetched {
        return Ok(resolved);
    }
    if path.is_some() && url.is_some() {
        return Err(failure(
            "invalid_args",
            "Specify either a local path or --url, not both",
        ));
    }
    if let Some(path) = path {
        return local_package(path, verification, config, secrets);
    }
    Err(failure(
        "invalid_args",
        "Plugin package source (--path or verified --url) is required",
    ))
}

fn perform(
    action: &PluginAction,
    config: &Config,
    secrets: Arc<SecretRegistry>,
    prefetched: Option<(VerifiedPackage, PackageSource)>,
) -> Result<Value, BoundaryError> {
    match action {
        PluginAction::Inspect { plugin, digest } => {
            check_id(plugin)?;
            if let Some(digest) = digest {
                check_digest(digest)?;
            }
        }
        PluginAction::Enable { plugin, digest, .. }
        | PluginAction::Uninstall { plugin, digest } => {
            check_id(plugin)?;
            check_digest(digest)?;
        }
        PluginAction::Disable { plugin, .. } => check_id(plugin)?,
        PluginAction::Update {
            plugin,
            expected_digest,
            ..
        } => {
            check_id(plugin)?;
            check_digest(expected_digest)?;
        }
        _ => {}
    }
    let data = match action {
        PluginAction::Validate {
            path,
            url,
            sha256,
            pubkey,
            signature,
        } => {
            let verification = verification_from(sha256, pubkey, signature);
            let (package, source) = resolve_sync_source(
                path.as_ref(),
                url.as_deref(),
                &verification,
                prefetched,
                config,
                &secrets,
            )?;
            let support = input_support_label(&source.kind);
            json!({"valid":true,"source":source,"digest":package.digest(),"capability_fingerprint":package.capability_fingerprint(),
                "manifest":package.manifest(),"file_count":package.files().len(),"bytes":package.files().iter().map(|file| file.bytes().len()).sum::<usize>(),
                "compatibility":compatibility(package.manifest())?,"input_support":support})
        }
        PluginAction::Install {
            path,
            url,
            sha256,
            pubkey,
            signature,
        } => {
            let verification = verification_from(sha256, pubkey, signature);
            let (package, source) = resolve_sync_source(
                path.as_ref(),
                url.as_deref(),
                &verification,
                prefetched,
                config,
                &secrets,
            )?;
            let support = input_support_label(&source.kind);
            let store = open_store(config, secrets.clone())?;
            let installed = store.install(&package, source)?;
            json!({"package":summary(&installed),"compatibility":compatibility(package.manifest())?,"input_support":support})
        }
        PluginAction::Update {
            plugin,
            path,
            url,
            sha256,
            pubkey,
            signature,
            expected_digest,
        } => {
            check_id(plugin)?;
            check_digest(expected_digest)?;
            let verification = verification_from(sha256, pubkey, signature);
            let (package, source) = resolve_sync_source(
                path.as_ref(),
                url.as_deref(),
                &verification,
                prefetched,
                config,
                &secrets,
            )?;
            let support = input_support_label(&source.kind);
            if package.manifest().plugin.id != *plugin {
                return Err(failure(
                    "plugin_identity",
                    "Update package has a different plugin ID",
                ));
            }
            if !store_exists(config)? {
                return Err(failure("plugin_not_found", "Plugin store does not exist"));
            }
            let store = open_store(config, secrets.clone())?;
            let updated = store.update(expected_digest, &package, source)?;
            json!({"package":summary(&updated),"compatibility":compatibility(package.manifest())?,"input_support":support})
        }
        PluginAction::List { page, page_size } => {
            range(*page, *page_size, 0)?;
            let items = if store_exists(config)? {
                open_store(config, secrets.clone())?.list()?
            } else {
                Vec::new()
            };
            let slice = range(*page, *page_size, items.len())?;
            json!({"total":items.len(),"page":page,"page_size":page_size,"items":items[slice].iter().map(summary).collect::<Vec<_>>()})
        }
        PluginAction::Recover { page, page_size } => {
            range(*page, *page_size, 0)?;
            let instances = if store_exists(config)? {
                open_store(config, secrets.clone())?.recover()?
            } else {
                Vec::new()
            };
            json!({"cleanup_pending":pending(&instances,*page,*page_size)?,"automatic_replay":false})
        }
        action => {
            if !store_exists(config)? {
                return Err(failure("plugin_not_found", "Plugin store does not exist"));
            }
            let store = open_store(config, secrets.clone())?;
            match action {
                PluginAction::Inspect { plugin, digest } => {
                    let digest = selected_digest(&store, plugin, digest.as_deref())?;
                    let inspection = inspect_id(&store, plugin, &digest)?;
                    json!({"package":summary(&inspection.summary),"compatibility":compatibility(&inspection.manifest)?,"manifest":inspection.manifest,"lock":inspection.lock})
                }
                PluginAction::Enable {
                    plugin,
                    digest,
                    scope: selected,
                } => {
                    let inspection = inspect_id(&store, plugin, digest)?;
                    let enabled = store.enable(
                        digest,
                        scope(config, *selected)?,
                        EnableGrant::requested(&inspection.manifest),
                        env!("CARGO_PKG_VERSION"),
                        &features(),
                    )?;
                    json!({"enabled":enabled,"requested_capabilities":inspection.manifest.capabilities,"authorization":"current_policy_checked_per_operation"})
                }
                PluginAction::Disable {
                    plugin,
                    scope: selected,
                } => {
                    check_id(plugin)?;
                    let changed = store.disable(plugin, Some(&scope(config, *selected)?))?;
                    json!({"plugin_id":plugin,"scheduling_disabled":true,"cleanup_pending":pending(&changed.cleanup_pending,1,PAGE_SIZE)?})
                }
                PluginAction::Uninstall { plugin, digest } => {
                    check_id(plugin)?;
                    check_digest(digest)?;
                    if !store
                        .list()?
                        .iter()
                        .any(|summary| summary.plugin_id == *plugin && summary.digest == *digest)
                    {
                        return Err(failure(
                            "plugin_identity",
                            "Installed plugin ID and digest do not match",
                        ));
                    }
                    let changed = store.uninstall(digest)?;
                    json!({"plugin_id":plugin,"digest":digest,"removed":changed.removed,"cleanup_pending":pending(&changed.cleanup_pending,1,PAGE_SIZE)?})
                }
                _ => {
                    return Err(failure(
                        "invalid_args",
                        "Unsupported plugin administration action",
                    ));
                }
            }
        }
    };
    safe_output(
        json!({"schema_version":1,"kind":"plugin","action":action.name(),"data":data}),
        &secrets,
    )
}

async fn resolve_remote(
    action: &PluginAction,
    secrets: &SecretRegistry,
) -> Result<Option<(VerifiedPackage, PackageSource)>, BoundaryError> {
    let (path, url, sha256, pubkey, signature) = match action {
        PluginAction::Validate {
            path,
            url,
            sha256,
            pubkey,
            signature,
        }
        | PluginAction::Install {
            path,
            url,
            sha256,
            pubkey,
            signature,
        }
        | PluginAction::Update {
            path,
            url,
            sha256,
            pubkey,
            signature,
            ..
        } => (path, url, sha256, pubkey, signature),
        _ => return Ok(None),
    };
    let Some(url) = url else {
        return Ok(None);
    };
    if path.is_some() {
        return Err(failure(
            "invalid_args",
            "Specify either a local path or --url, not both",
        ));
    }
    let verification = verification_from(sha256, pubkey, signature);
    let (package, source) = fetch_https_archive(url, &verification).await?;
    check_manifest_secrets(&package, secrets)?;
    ensure_clean(&json!({"source": &source}), secrets)?;
    Ok(Some((package, source)))
}

/// Return one bounded JSON value. The caller selects text, JSON, or a single
/// `plugin.result` JSONL envelope and maps BoundaryError to the shared exit codes.
pub async fn run(action: &PluginAction, config: &Config) -> Result<Value, BoundaryError> {
    let action = action.clone();
    let config = config.clone();
    let secrets = known_registry(&config)?;
    let prefetched = resolve_remote(&action, &secrets).await?;
    tokio::task::spawn_blocking(move || perform(&action, &config, secrets, prefetched))
        .await
        .map_err(|_| failure("plugin_worker", "Plugin administration worker failed"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use soyo_core::{
        config::{Environment, Overrides},
        contract::{RunId, SessionId, WorkMode},
        plugin_store::CleanupOutcome,
    };
    use std::{fs, os::unix::fs::PermissionsExt};

    const MANIFEST: &str = r#"schema_version=1
[plugin]
id="io.soyo.test"
version="1.0.0"
description="An offline local test package."
license="MIT"
[compatibility]
requires_soyo=">=0.2.0, <1.0.0"
requires_features=["plugin-manifest-v1"]
[capabilities]
requested=["workspace.read"]
[[resources]]
id="guide"
path="guide.md"
media_type="text/markdown"
"#;
    struct Fixture {
        root: tempfile::TempDir,
        config: Config,
        package: PathBuf,
    }
    impl Fixture {
        fn new(secret: Option<&str>) -> Self {
            let root = tempfile::tempdir().unwrap();
            let config_path = root.path().join("config.toml");
            let mut text = "schema_version=1\n".to_owned();
            let mut values = vec![("HOME".into(), root.path().as_os_str().to_owned())];
            if let Some(secret) = secret {
                text.push_str("[profiles.fixture]\nprotocol=\"openai-chat-completions\"\nbase_url=\"https://example.invalid/v1\"\nmodel=\"fixture\"\nauth=\"bearer\"\napi_key_env=\"FIXTURE_KEY\"\ncontext_window_tokens=128000\n");
                values.push(("FIXTURE_KEY".into(), secret.into()));
            }
            fs::write(&config_path, text).unwrap();
            let config = soyo_core::config::load(
                Overrides {
                    config: Some(config_path),
                    ..Default::default()
                },
                Environment::from_pairs(values),
                root.path(),
            )
            .unwrap();
            let package = root.path().join("source");
            fs::create_dir(&package).unwrap();
            fs::write(package.join(MANIFEST_NAME), MANIFEST).unwrap();
            fs::write(package.join("guide.md"), "package guide").unwrap();
            Self {
                root,
                config,
                package,
            }
        }
        async fn install(&self) -> String {
            run(&install_action(self.package.clone()), &self.config)
                .await
                .unwrap()["data"]["package"]["digest"]
                .as_str()
                .unwrap()
                .into()
        }
    }
    fn validate_action(path: PathBuf) -> PluginAction {
        PluginAction::Validate {
            path: Some(path),
            url: None,
            sha256: None,
            pubkey: None,
            signature: None,
        }
    }
    fn install_action(path: PathBuf) -> PluginAction {
        PluginAction::Install {
            path: Some(path),
            url: None,
            sha256: None,
            pubkey: None,
            signature: None,
        }
    }
    fn update_action(plugin: &str, path: PathBuf, expected_digest: String) -> PluginAction {
        PluginAction::Update {
            plugin: plugin.into(),
            path: Some(path),
            url: None,
            sha256: None,
            pubkey: None,
            signature: None,
            expected_digest,
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fn writable(path: &Path) {
                let Ok(metadata) = fs::symlink_metadata(path) else {
                    return;
                };
                if metadata.is_dir() {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
                    if let Ok(entries) = fs::read_dir(path) {
                        for entry in entries.flatten() {
                            writable(&entry.path());
                        }
                    }
                }
            }
            writable(self.root.path());
        }
    }
    fn list(page: u32) -> PluginAction {
        PluginAction::List {
            page,
            page_size: PAGE_SIZE,
        }
    }

    #[tokio::test]
    async fn validation_and_empty_queries_do_not_create_a_store_or_execute_code() {
        let fixture = Fixture::new(None);
        let validated = run(&validate_action(fixture.package.clone()), &fixture.config)
            .await
            .unwrap();
        assert_eq!(validated["data"]["valid"], true);
        assert_eq!(validated["data"]["compatibility"]["compatible"], true);
        assert!(!fixture.config.data_dir.exists());
        assert_eq!(
            run(&list(1), &fixture.config).await.unwrap()["data"]["total"],
            0
        );
        assert_eq!(
            run(
                &PluginAction::Recover {
                    page: 1,
                    page_size: PAGE_SIZE
                },
                &fixture.config
            )
            .await
            .unwrap()["data"]["cleanup_pending"]["total"],
            0
        );
        assert!(!fixture.config.data_dir.exists());
        for path in [
            PathBuf::from("https://example.invalid/package"),
            fixture.package.join("guide.md"),
        ] {
            assert_eq!(
                run(&install_action(path), &fixture.config)
                    .await
                    .unwrap_err()
                    .0
                    .code,
                "plugin_source_unsupported"
            );
        }
    }
    #[tokio::test]
    async fn install_inspect_enable_and_scoped_disable_keep_exact_identity_and_ceiling() {
        let fixture = Fixture::new(None);
        let digest = fixture.install().await;
        let listed = run(&list(1), &fixture.config).await.unwrap();
        assert_eq!(listed["data"]["items"][0]["enabled_scope_count"], 0);
        assert_eq!(
            listed["data"]["items"][0]["requested_capabilities"]["requested"][0],
            "workspace.read"
        );
        let inspected = run(
            &PluginAction::Inspect {
                plugin: "io.soyo.test".into(),
                digest: None,
            },
            &fixture.config,
        )
        .await
        .unwrap();
        assert_eq!(inspected["data"]["package"]["digest"], digest);
        let enabled = run(
            &PluginAction::Enable {
                plugin: "io.soyo.test".into(),
                digest: digest.clone(),
                scope: PluginScopeOption::Workspace,
            },
            &fixture.config,
        )
        .await
        .unwrap();
        assert_eq!(enabled["data"]["enabled"]["scope"]["kind"], "workspace");
        assert_eq!(
            enabled["data"]["enabled"]["grant"]["capabilities"][0],
            "workspace.read"
        );
        let disabled = run(
            &PluginAction::Disable {
                plugin: "io.soyo.test".into(),
                scope: PluginScopeOption::Workspace,
            },
            &fixture.config,
        )
        .await
        .unwrap();
        assert_eq!(disabled["data"]["scheduling_disabled"], true);
        assert_eq!(disabled["data"]["cleanup_pending"]["total"], 0);
    }
    #[tokio::test]
    async fn cli_enable_accepts_hook_packages_with_the_runtime_feature_and_explicit_ceiling() {
        #[derive(Parser)]
        struct Arguments {
            #[command(subcommand)]
            action: PluginAction,
        }
        let fixture = Fixture::new(None);
        let manifest = MANIFEST
            .replace(
                "requires_features=[\"plugin-manifest-v1\"]",
                "requires_features=[\"plugin-manifest-v1\",\"hook-events-v1\"]",
            )
            .replace(
                "requested=[\"workspace.read\"]",
                "requested=[\"workspace.read\",\"process.exec\",\"hook.observe\"]",
            );
        fs::write(
            fixture.package.join(MANIFEST_NAME),
            format!("{manifest}\n[[hooks]]\nid=\"started\"\nevent=\"run.started\"\nkind=\"observer\"\nprogram=\"shell\"\nargs=[\"@plugin/hook.sh\"]\ntimeout_ms=5000\n"),
        ).unwrap();
        fs::write(fixture.package.join("hook.sh"), "#!/bin/sh\nexit 93\n").unwrap();
        let validated = run(&validate_action(fixture.package.clone()), &fixture.config)
            .await
            .unwrap();
        assert_eq!(validated["data"]["compatibility"]["compatible"], true);
        assert!(
            validated["data"]["compatibility"]["host_features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "hook-events-v1")
        );
        let digest = fixture.install().await;
        let action =
            Arguments::try_parse_from(["plugin", "enable", "io.soyo.test", "--digest", &digest])
                .unwrap()
                .action;
        let enabled = run(&action, &fixture.config).await.unwrap();
        assert_eq!(enabled["data"]["enabled"]["scope"]["kind"], "workspace");
        let granted = enabled["data"]["enabled"]["grant"]["capabilities"]
            .as_array()
            .unwrap();
        for capability in ["workspace.read", "process.exec", "hook.observe"] {
            assert!(granted.iter().any(|value| value == capability));
        }
        assert_eq!(granted.len(), 3);
        let store = open_store(&fixture.config, known_registry(&fixture.config).unwrap()).unwrap();
        let installed = store.inspect(&digest).unwrap();
        assert_eq!(installed.manifest.hooks.len(), 1);
        assert!(installed.summary.instances.is_empty());
        assert!(!features().contains("browser-managed-v1"));
    }
    #[tokio::test]
    async fn update_uses_compare_and_swap_and_uninstall_reports_owned_cleanup_pending() {
        let fixture = Fixture::new(None);
        let old = fixture.install().await;
        fs::write(fixture.package.join("guide.md"), "new guide").unwrap();
        let updated = run(
            &update_action("io.soyo.test", fixture.package.clone(), old.clone()),
            &fixture.config,
        )
        .await
        .unwrap();
        let digest = updated["data"]["package"]["digest"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(digest, old);
        assert_eq!(updated["data"]["package"]["enabled_scope_count"], 0);
        assert_eq!(
            run(
                &update_action("io.soyo.test", fixture.package.clone(), old),
                &fixture.config
            )
            .await
            .unwrap_err()
            .0
            .code,
            "plugin_stale"
        );
        run(
            &PluginAction::Enable {
                plugin: "io.soyo.test".into(),
                digest: digest.clone(),
                scope: PluginScopeOption::Workspace,
            },
            &fixture.config,
        )
        .await
        .unwrap();
        let store = open_store(&fixture.config, known_registry(&fixture.config).unwrap()).unwrap();
        let lease = store
            .acquire(
                "io.soyo.test",
                &fixture.config.cwd,
                SessionId::new(),
                RunId::new(),
                env!("CARGO_PKG_VERSION"),
                &features(),
            )
            .unwrap();
        let removed = run(
            &PluginAction::Uninstall {
                plugin: "io.soyo.test".into(),
                digest: digest.clone(),
            },
            &fixture.config,
        )
        .await
        .unwrap();
        assert_eq!(removed["data"]["removed"], false);
        assert_eq!(
            removed["data"]["cleanup_pending"]["items"][0]["state"],
            "stop_requested"
        );
        assert!(lease.check_active().is_err());
        lease.finish(CleanupOutcome::Cleaned).unwrap();
        run(
            &PluginAction::Recover {
                page: 1,
                page_size: PAGE_SIZE,
            },
            &fixture.config,
        )
        .await
        .unwrap();
        assert!(
            store
                .list()
                .unwrap()
                .iter()
                .all(|package| package.digest != digest)
        );
    }
    #[tokio::test]
    async fn configured_secrets_are_rejected_before_installation_and_never_returned() {
        let secret = "SYNTHETIC-PLUGIN-ADMIN-SECRET";
        let fixture = Fixture::new(Some(secret));
        fs::write(
            fixture.package.join(MANIFEST_NAME),
            MANIFEST.replace("An offline local test package.", secret),
        )
        .unwrap();
        let error = run(&install_action(fixture.package.clone()), &fixture.config)
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "sensitive_data");
        assert!(!error.to_string().contains(secret));
        assert!(!fixture.config.data_dir.exists());
        let registry = known_registry(&fixture.config).unwrap();
        let output = safe_output(
            json!({"nested":format!("prefix {secret} suffix")}),
            &registry,
        )
        .unwrap();
        assert!(!output.to_string().contains(secret));
    }
    #[tokio::test]
    async fn invalid_pages_and_plan_storage_overlap_fail_before_creating_directories() {
        let mut fixture = Fixture::new(None);
        assert_eq!(
            run(&list(0), &fixture.config).await.unwrap_err().0.code,
            "invalid_args"
        );
        assert!(
            run(
                &PluginAction::Recover {
                    page: 1,
                    page_size: 0
                },
                &fixture.config
            )
            .await
            .is_err()
        );
        assert!(!fixture.config.data_dir.exists());
        fixture.config.mode = WorkMode::Plan;
        assert!(
            run(&validate_action(fixture.package.clone()), &fixture.config)
                .await
                .is_ok()
        );
        assert_eq!(
            run(&install_action(fixture.package.clone()), &fixture.config)
                .await
                .unwrap_err()
                .0
                .code,
            "plan_read_only"
        );
        assert!(!fixture.config.data_dir.exists());
    }
    #[test]
    fn command_parser_requires_explicit_digests_and_defaults_to_workspace_scope() {
        #[derive(Parser)]
        struct Arguments {
            #[command(subcommand)]
            action: PluginAction,
        }
        let digest = "a".repeat(64);
        let parsed =
            Arguments::try_parse_from(["plugin", "enable", "io.soyo.test", "--digest", &digest])
                .unwrap();
        assert!(matches!(
            parsed.action,
            PluginAction::Enable {
                scope: PluginScopeOption::Workspace,
                ..
            }
        ));
        assert!(Arguments::try_parse_from(["plugin", "enable", "io.soyo.test"]).is_err());
        assert!(Arguments::try_parse_from(["plugin", "uninstall", "io.soyo.test"]).is_err());
        assert!(
            Arguments::try_parse_from([
                "plugin",
                "install",
                "--url",
                "https://example.invalid/package"
            ])
            .is_err()
        );
        assert!(
            Arguments::try_parse_from([
                "plugin",
                "install",
                "--url",
                "https://example.invalid/package",
                "--sha256",
                &digest
            ])
            .is_ok()
        );
        assert!(features().contains("hook-events-v1"));
        assert!(!features().contains("browser-managed-v1"));
    }
    #[tokio::test]
    async fn archive_validate_install_and_update_verify_sha256_and_preserve_tree_digest() {
        let fixture = Fixture::new(None);
        let dir_validated = run(&validate_action(fixture.package.clone()), &fixture.config)
            .await
            .unwrap();
        let expected_tree_digest = dir_validated["data"]["digest"].as_str().unwrap().to_owned();

        let archive_path = fixture.root.path().join("plugin.tar.gz");
        let output = std::process::Command::new("tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("-czf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&fixture.package)
            .arg(".")
            .output()
            .unwrap();
        assert!(output.status.success(), "tar failed: {output:?}");
        let raw_archive = fs::read(&archive_path).unwrap();
        let archive_sha = soyo_core::plugin_manifest::archive_sha256_hex(&raw_archive);

        // Validate archive directly with sha256 check.
        let validated_archive = run(
            &PluginAction::Validate {
                path: Some(archive_path.clone()),
                url: None,
                sha256: Some(archive_sha.clone()),
                pubkey: None,
                signature: None,
            },
            &fixture.config,
        )
        .await
        .unwrap();
        assert_eq!(validated_archive["data"]["digest"], expected_tree_digest);

        // Tampered sha256 fails closed before opening store.
        let bad_sha = "0".repeat(64);
        assert_eq!(
            run(
                &PluginAction::Install {
                    path: Some(archive_path.clone()),
                    url: None,
                    sha256: Some(bad_sha),
                    pubkey: None,
                    signature: None,
                },
                &fixture.config
            )
            .await
            .unwrap_err()
            .0
            .code,
            "plugin_checksum_mismatch"
        );
        assert!(!fixture.config.data_dir.exists());

        // Matching sha256 installs archive and produces identical tree digest.
        let installed = run(
            &PluginAction::Install {
                path: Some(archive_path.clone()),
                url: None,
                sha256: Some(archive_sha),
                pubkey: None,
                signature: None,
            },
            &fixture.config,
        )
        .await
        .unwrap();
        assert_eq!(installed["data"]["package"]["digest"], expected_tree_digest);
        assert_eq!(installed["data"]["package"]["source"]["kind"], "archive");
        assert_eq!(installed["data"]["input_support"], "archive");

        // Update from an updated archive preserves expected_digest CAS semantics.
        let v2_dir = fixture.root.path().join("source-v2");
        fs::create_dir(&v2_dir).unwrap();
        fs::write(
            v2_dir.join(MANIFEST_NAME),
            MANIFEST.replace("version=\"1.0.0\"", "version=\"1.1.0\""),
        )
        .unwrap();
        fs::write(v2_dir.join("guide.md"), "updated guide").unwrap();
        let v2_archive = fixture.root.path().join("plugin-v2.tar.gz");
        let output_v2 = std::process::Command::new("tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("-czf")
            .arg(&v2_archive)
            .arg("-C")
            .arg(&v2_dir)
            .arg(".")
            .output()
            .unwrap();
        assert!(output_v2.status.success(), "tar v2 failed: {output_v2:?}");
        let v2_sha =
            soyo_core::plugin_manifest::archive_sha256_hex(&fs::read(&v2_archive).unwrap());
        let updated = run(
            &PluginAction::Update {
                plugin: "io.soyo.test".into(),
                path: Some(v2_archive),
                url: None,
                sha256: Some(v2_sha),
                pubkey: None,
                signature: None,
                expected_digest: expected_tree_digest,
            },
            &fixture.config,
        )
        .await
        .unwrap();
        assert_eq!(updated["data"]["package"]["version"], "1.1.0");
        assert_eq!(updated["data"]["package"]["source"]["kind"], "archive");
        assert_eq!(updated["data"]["input_support"], "archive");
    }
}
