//! Unified `soyo credentials` administration. Independent of any configured or
//! enabled MCP server: `list`/`revoke`/`rotate`/`grant`/`grant-revoke` operate
//! on the credential store directly; `import`/`login` still bind a named HTTP
//! MCP server because they produce a scope-bound reference. Credential material
//! never appears in arguments or output.
use clap::{Args, Subcommand};
use soyo_core::{
    config::{Config, Output},
    contract::BoundaryError,
    credentials::{
        CredentialBackend, CredentialGrant, CredentialRef, CredentialScope, CredentialValue,
        GrantRunKind, GrantStore, SecretRegistry, SecretValue,
    },
    mcp::config::{HttpMcpSettings, McpTransportSettings},
};
use std::{
    collections::BTreeSet,
    io::{self, IsTerminal},
    sync::Arc,
    time::Duration,
};

#[derive(Subcommand)]
pub enum CredentialsAction {
    /// List credential metadata (reference, scope, kind, revision, revoked).
    List,
    /// Import a bearer token from a named environment variable for an HTTP MCP server.
    Import(ImportArgs),
    /// Permanently revoke a credential reference (also drops its grants).
    Revoke {
        /// Credential reference UUID.
        reference: String,
    },
    /// Rotate a secret credential in place: new value, same reference, revision bump.
    /// OAuth credentials rotate through token refresh or a fresh login instead.
    Rotate {
        /// Credential reference UUID.
        reference: String,
        /// Environment variable holding the new secret value.
        #[arg(long)]
        from_env: String,
    },
    /// Grant a credential reference to workspaces/run-kinds/tools with an optional expiry.
    Grant(GrantArgs),
    /// Drop one grant by id (from `credentials grant-list`).
    GrantRevoke {
        /// Grant UUID.
        grant_id: String,
    },
    /// List configured + persisted grants.
    GrantList,
    /// Interactive OAuth login for an HTTP MCP server (PKCE S256, loopback).
    Login(LoginArgs),
}

#[derive(Args)]
pub struct ImportArgs {
    /// Enabled HTTP MCP server name the credential is scoped to.
    pub(crate) server: String,
    /// Environment variable holding the secret value.
    #[arg(long)]
    pub(crate) from_env: String,
    /// Additional OAuth scope strings (merged with configured `scopes`).
    #[arg(long)]
    pub(crate) scope: Vec<String>,
    /// Explicitly approve this endpoint and scope set without a prompt.
    #[arg(long)]
    pub(crate) approve_scopes: bool,
}

#[derive(Args)]
pub struct GrantArgs {
    /// Credential reference UUID.
    reference: String,
    /// Restrict to these workspace identity ids (`ws_<digest>`); empty = any.
    #[arg(long)]
    workspace: Vec<String>,
    /// Run kinds this grant covers; required, repeatable. One of tool|subagent|profile.
    #[arg(long, required = true)]
    run_kind: Vec<GrantRunKindArg>,
    /// Restrict to these tool names; empty = any tool.
    #[arg(long)]
    tool: Vec<String>,
    /// RFC 3339 UTC expiry (e.g. 2030-01-01T00:00:00Z); omit = never.
    #[arg(long)]
    expires_at: Option<String>,
    /// Server id the grant binds to; defaults to the credential's own scope server.
    #[arg(long)]
    server_id: Option<String>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum GrantRunKindArg {
    Tool,
    Subagent,
    Profile,
}
impl From<GrantRunKindArg> for GrantRunKind {
    fn from(value: GrantRunKindArg) -> Self {
        match value {
            GrantRunKindArg::Tool => Self::Tool,
            GrantRunKindArg::Subagent => Self::Subagent,
            GrantRunKindArg::Profile => Self::Profile,
        }
    }
}

#[derive(Args)]
pub struct LoginArgs {
    /// Enabled HTTP MCP server name.
    server: String,
    #[arg(long)]
    scope: Vec<String>,
    /// Explicitly approve this endpoint and the requested scope set.
    #[arg(long)]
    approve_scopes: bool,
    #[arg(long)]
    client_id: Option<String>,
    #[arg(long, default_value_t = 0)]
    callback_port: u16,
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=600))]
    timeout_seconds: u64,
    /// Open the returned authorization URL using the system URL handler.
    #[arg(long)]
    open_browser: bool,
}

fn failure(code: &str, message: &str) -> BoundaryError {
    super::cli_error(code, message)
}
fn known_credentials(config: &Config) -> Result<Vec<String>, BoundaryError> {
    config
        .secret_env_names()
        .iter()
        .map(|name| config.secret(name).map(str::to_owned))
        .collect()
}
fn ensure_clean_metadata(value: &str, secrets: &[String]) -> Result<(), BoundaryError> {
    if secrets.iter().all(String::is_empty) {
        return Ok(());
    }
    let sensitive = || {
        failure(
            "sensitive_data",
            "credential administration metadata contains a credential",
        )
    };
    let mut view = value.to_owned();
    for _ in 0..8 {
        if soyo_core::redaction::contains_secret(&view, secrets) {
            return Err(sensitive());
        }
        let mut decoded = Vec::with_capacity(view.len());
        let mut cursor = 0;
        let bytes = view.as_bytes();
        let mut changed = false;
        while cursor < bytes.len() {
            if let Some(encoded) = bytes.get(cursor..cursor.saturating_add(3))
                && encoded[0] == b'%'
                && let (Some(high), Some(low)) = (
                    char::from(encoded[1]).to_digit(16),
                    char::from(encoded[2]).to_digit(16),
                )
            {
                decoded.push((high * 16 + low) as u8);
                cursor += 3;
                changed = true;
            } else if bytes[cursor] == b'+' {
                decoded.push(b' ');
                cursor += 1;
                changed = true;
            } else {
                decoded.push(bytes[cursor]);
                cursor += 1;
            }
        }
        if !changed {
            return Ok(());
        }
        view = String::from_utf8_lossy(&decoded).into_owned();
    }
    Err(sensitive())
}
fn http_settings<'a>(config: &'a Config, server: &str) -> Result<&'a HttpMcpSettings, BoundaryError> {
    let configured = config
        .mcp
        .servers
        .get(server)
        .filter(|server| server.enabled)
        .ok_or_else(|| failure("invalid_config", "Enabled MCP server was not found"))?;
    match &configured.transport {
        McpTransportSettings::Http(http) => Ok(http),
        _ => Err(failure(
            "invalid_config",
            "Credential administration requires an HTTP MCP server",
        )),
    }
}
fn open_store(
    config: &Config,
) -> Result<(soyo_core::credentials::CredentialStore, Arc<SecretRegistry>), BoundaryError> {
    let registry = Arc::new(SecretRegistry::default());
    let values = known_credentials(config)?;
    registry.register(&values)?;
    std::fs::create_dir_all(&config.state_dir).map_err(|_| {
        failure(
            "credential_io",
            "Credential parent directory could not be created",
        )
    })?;
    let store = soyo_core::credentials::CredentialStore::open_backend(
        config.credentials.backend,
        &config.state_dir,
        registry.clone(),
    )?;
    Ok((store, registry))
}
fn open_grants(store: &soyo_core::credentials::CredentialStore) -> Result<GrantStore, BoundaryError> {
    GrantStore::open(store.protected_path())
}
/// Persist configured `[[credentials.grants]]` entries plus return them merged
/// with already-stored grants. Config grants get fresh ids on each write —
/// re-running is idempotent because entries are deduplicated on content.
fn materialize_grants(
    config: &Config,
    grants: &GrantStore,
) -> Result<Vec<CredentialGrant>, BoundaryError> {
    let stored = grants.list()?;
    for entry in &config.credentials.grants {
        let reference: CredentialRef = entry
            .reference
            .parse()
            .map_err(|_| failure("invalid_credential_ref", "grant reference is not a UUID"))?;
        // Skip when an identical (reference + filters) grant already exists.
        let duplicate = stored.iter().any(|existing| {
            existing.reference == reference
                && existing.server_id == entry.server_id
                && existing.workspaces == entry.workspaces
                && existing.run_kinds == entry.run_kinds
                && existing.tools == entry.tools
                && existing.expires_at == entry.expires_at
        });
        if !duplicate {
            let grant = entry
                .to_grant(uuid::Uuid::now_v7())
                .map_err(|_| failure("invalid_config", "credential grant is invalid"))?;
            grants.put(&grant)?;
        }
    }
    grants.list()
}
async fn approve(
    scope: &CredentialScope,
    approved: bool,
    secrets: &[String],
    cancel: &dyn soyo_core::boundary::Cancellation,
) -> Result<(), BoundaryError> {
    if approved {
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(failure(
            "needs_input",
            "Explicit approval of the endpoint and scopes is required",
        ));
    }
    let terminal = super::Terminal::open(secrets.to_vec())
        .map_err(|_| failure("needs_input", "A controlling terminal is required"))?;
    let scopes = scope.scopes.iter().cloned().collect::<Vec<_>>().join(" ");
    let prompt = format!(
        "\nMCP server {}\nResource {}\nRequested scopes [{}]\nApprove this authorization? [y/N] ",
        scope.server_id, scope.audience, scopes
    );
    if terminal
        .read_line(&prompt, cancel)
        .await?
        .is_some_and(|answer| matches!(answer.trim(), "y" | "Y" | "yes"))
    {
        Ok(())
    } else {
        Err(failure(
            "permission_denied",
            "Authorization was not approved",
        ))
    }
}
fn scope(
    config: &Config,
    server: &str,
    extra: &[String],
    secrets: &[String],
) -> Result<CredentialScope, BoundaryError> {
    let http = http_settings(config, server)?;
    let requested: BTreeSet<_> = http.scopes.iter().chain(extra).cloned().collect();
    let scope = CredentialScope::for_mcp(server, &http.endpoint, requested)?;
    ensure_clean_metadata(
        &serde_json::json!({"scope":scope,"endpoint":http.endpoint}).to_string(),
        secrets,
    )?;
    Ok(scope)
}
/// Audit one credential lifecycle event into the store's redacted journal:
/// `<credentials>/audit.log`, 0600 append-only, one JSON line per event. The
/// payload carries reference ids and action names only — never values.
fn audit(
    store: &soyo_core::credentials::CredentialStore,
    event: &str,
    reference: Option<CredentialRef>,
    extra: serde_json::Value,
) -> Result<(), BoundaryError> {
    use std::io::Write as _;
    let path = store.protected_path().join("audit.log");
    let mut line = serde_json::json!({
        "type": "credential.event",
        "event": event,
        "at": soyo_core::credentials::now_rfc3339(),
        "reference": reference,
    });
    line.as_object_mut()
        .map(|object| object.extend(extra.as_object().cloned().unwrap_or_default()));
    let encoded = serde_json::to_string(&line)
        .map_err(|_| failure("credential_io", "credential event could not be encoded"))?;
    // Redaction gate: the journal must never carry a known secret.
    let secrets = store.secret_registry().snapshot()?;
    if soyo_core::redaction::contains_secret(&encoded, &secrets) {
        return Err(failure(
            "sensitive_data",
            "credential event payload contained a credential",
        ));
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|_| failure("credential_io", "credential audit log could not be opened"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    writeln!(file, "{encoded}")
        .map_err(|_| failure("credential_io", "credential audit event could not be written"))
}

pub(crate) async fn perform(config: &Config, action: &CredentialsAction) -> Result<serde_json::Value, BoundaryError> {
    let cancel = Arc::new(super::Cancel::default());
    let _listener = super::signals::CancellationListener::start(cancel.clone())
        .map_err(|_| failure("signal_error", "Signal handlers could not be installed"))?;
    let (store, _registry) = open_store(config)?;
    match action {
        CredentialsAction::List => {
            let metadata = store.list().await?;
            let grants = open_grants(&store).and_then(|store| store.list()).unwrap_or_default();
            Ok(serde_json::json!({
                "backend": store.backend_name(),
                "system_backend_supported": CredentialBackend::system_supported(),
                "credentials": metadata,
                "grants": grants,
            }))
        }
        CredentialsAction::Revoke { reference } => {
            let reference: CredentialRef = reference.parse()?;
            let metadata = store
                .list()
                .await?
                .into_iter()
                .find(|record| record.reference == reference)
                .ok_or_else(|| {
                    failure(
                        "invalid_credential_ref",
                        "Credential reference was not found",
                    )
                })?;
            let removed = store.revoke(reference, &metadata.scope).await?;
            audit(
                &store,
                "revoke",
                Some(reference),
                serde_json::json!({"grants_removed": removed}),
            )?;
            Ok(serde_json::json!({
                "reference": reference,
                "revoked": true,
                "grants_removed": removed,
            }))
        }
        CredentialsAction::Rotate { reference, from_env } => {
            let reference: CredentialRef = reference.parse()?;
            let value = SecretValue::new(config.secret(from_env)?.to_owned())?;
            let mut secrets = known_credentials(config)?;
            secrets.push(value.expose().to_owned());
            store.secret_registry().register(&secrets)?;
            let metadata = store
                .list()
                .await?
                .into_iter()
                .find(|record| record.reference == reference)
                .ok_or_else(|| {
                    failure(
                        "invalid_credential_ref",
                        "Credential reference was not found",
                    )
                })?;
            let revision = store.rotate_secret(reference, &metadata.scope, value).await?;
            audit(
                &store,
                "rotate",
                Some(reference),
                serde_json::json!({"revision": revision}),
            )?;
            Ok(serde_json::json!({
                "reference": reference,
                "revision": revision,
            }))
        }
        CredentialsAction::Grant(args) => {
            let reference: CredentialRef = args.reference.parse()?;
            let metadata = store
                .list()
                .await?
                .into_iter()
                .find(|record| record.reference == reference)
                .ok_or_else(|| {
                    failure(
                        "invalid_credential_ref",
                        "Credential reference was not found",
                    )
                })?;
            let grant = CredentialGrant {
                grant_id: uuid::Uuid::now_v7(),
                reference,
                workspaces: args.workspace.iter().cloned().collect(),
                run_kinds: args.run_kind.iter().map(|kind| (*kind).into()).collect(),
                tools: args.tool.iter().cloned().collect(),
                expires_at: args.expires_at.clone(),
                server_id: args
                    .server_id
                    .clone()
                    .unwrap_or_else(|| metadata.scope.server_id.clone()),
            };
            grant
                .validate()
                .map_err(|_| failure("invalid_config", "credential grant is invalid"))?;
            let store_grants = open_grants(&store)?;
            store_grants.put(&grant)?;
            audit(
                &store,
                "grant",
                Some(reference),
                serde_json::json!({"grant_id": grant.grant_id, "server_id": grant.server_id}),
            )?;
            Ok(serde_json::json!({"grant_id": grant.grant_id, "reference": reference}))
        }
        CredentialsAction::GrantRevoke { grant_id } => {
            let grant_id: uuid::Uuid = grant_id
                .parse()
                .map_err(|_| failure("invalid_args", "grant id is not a UUID"))?;
            let removed = open_grants(&store)?.remove(grant_id)?;
            if removed {
                audit(&store, "grant_revoke", None, serde_json::json!({"grant_id": grant_id}))?;
            }
            Ok(serde_json::json!({"grant_id": grant_id, "removed": removed}))
        }
        CredentialsAction::GrantList => {
            let grants = materialize_grants(config, &open_grants(&store)?)?;
            Ok(serde_json::to_value(grants)
                .map_err(|_| failure("output_error", "grants could not be encoded"))?)
        }
        CredentialsAction::Import(args) => {
            let value = SecretValue::new(config.secret(&args.from_env)?.to_owned())?;
            let mut secrets = known_credentials(config)?;
            secrets.push(value.expose().to_owned());
            let scope = scope(config, &args.server, &args.scope, &secrets)?;
            approve(&scope, args.approve_scopes, &secrets, cancel.as_ref()).await?;
            let reference = store.create(scope, CredentialValue::Secret(value)).await?;
            audit(
                &store,
                "import",
                Some(reference),
                serde_json::json!({"server": args.server}),
            )?;
            Ok(serde_json::json!({"reference": reference, "server": args.server}))
        }
        CredentialsAction::Login(args) => {
            let secrets = known_credentials(config)?;
            ensure_clean_metadata(
                &serde_json::json!({"client_id":args.client_id}).to_string(),
                &secrets,
            )?;
            let scope = scope(config, &args.server, &args.scope, &secrets)?;
            approve(&scope, args.approve_scopes, &secrets, cancel.as_ref()).await?;
            let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
            let flow = soyo_core::credentials::McpOAuthLogin::begin(
                store.clone(),
                scope,
                soyo_core::credentials::OAuthLoginOptions {
                    interactive,
                    client_id: args.client_id.clone(),
                    timeout: Duration::from_secs(args.timeout_seconds),
                    callback_port: args.callback_port,
                },
                cancel.as_ref(),
            )
            .await?;
            ensure_clean_metadata(flow.authorization_url(), &secrets)?;
            eprintln!(
                "Open this URL to authorize MCP access\n{}",
                flow.authorization_url()
            );
            if args.open_browser {
                open_browser(flow.authorization_url(), cancel.as_ref()).await?;
            }
            let reference = flow.wait_for_callback(cancel.as_ref()).await?;
            audit(
                &store,
                "login",
                Some(reference),
                serde_json::json!({"server": args.server}),
            )?;
            Ok(serde_json::json!({"reference": reference, "server": args.server}))
        }
    }
}
async fn open_browser(
    url: &str,
    cancel: &dyn soyo_core::boundary::Cancellation,
) -> Result<(), BoundaryError> {
    if cancel.is_cancelled() {
        return Err(failure("cancelled", "Authorization was cancelled"));
    }
    #[cfg(target_os = "macos")]
    let program = "/usr/bin/open";
    #[cfg(target_os = "linux")]
    let program = "/usr/bin/xdg-open";
    let mut child = tokio::process::Command::new(program)
        .arg(url)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| failure("browser_error", "System URL handler could not be started"))?;
    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        result = tokio::time::timeout(Duration::from_secs(10), child.wait()) => result.ok().and_then(Result::ok),
    };
    let Some(status) = status else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(if cancel.is_cancelled() {
            failure("cancelled", "Authorization was cancelled")
        } else {
            failure("browser_error", "System URL handler did not complete")
        });
    };
    if status.success() {
        Ok(())
    } else {
        Err(failure("browser_error", "System URL handler failed"))
    }
}
pub(crate) fn error_exit_code(code: &str) -> u8 {
    match code {
        "cancelled" => 130,
        "needs_input" | "needs_auth" => 4,
        "needs_confirmation" => 3,
        "limit_reached" | "credential_limit" => 5,
        "invalid_config"
        | "invalid_args"
        | "invalid_credential"
        | "invalid_credential_ref"
        | "credential_scope"
        | "credential_grant" => 2,
        _ => 1,
    }
}
pub async fn run(config: &Config, action: &CredentialsAction) -> u8 {
    let result = perform(config, action).await;
    let (data, error, code) = match result {
        Ok(data) => (Some(data), None, 0),
        Err(error) => {
            let code = error_exit_code(&error.0.code);
            (None, Some(error.0), code)
        }
    };
    let mut value = serde_json::json!({"schema_version":1,"data":data,"error":error});
    let secrets = known_credentials(config);
    let safe = secrets.is_ok_and(|secrets| {
        soyo_core::redaction::redact(&mut value, &secrets).is_ok()
            && !soyo_core::redaction::contains_secret(&value.to_string(), &secrets)
    });
    if !safe {
        return super::query_error(
            super::cli_error(
                "runtime_error",
                "Sensitive credential administration details were withheld",
            ),
            Some(config.output),
        );
    }
    if config.output == Output::Jsonl {
        value["type"] = "credentials.result".into();
    }
    println!("{value}");
    code
}
