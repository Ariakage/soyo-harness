//! Strict, default-disabled settings for browser credential slots and the
//! local file broker. Every field is host-owned; model input may only name a
//! pre-registered slot alias or artifact reference.

use crate::{
    contract::{BoundaryError, ErrorInfo},
    credentials::CredentialRef,
    network::NormalizedOrigin,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_MAX_DOWNLOAD_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CREDENTIAL_SLOTS: usize = 16;
const MAX_SLOT_USES: u32 = 32;

fn error(code: &str, message: &'static str) -> BoundaryError {
    BoundaryError(
        ErrorInfo {
            code: code.into(),
            message: message.into(),
        },
        None,
        None,
    )
}

fn invalid(message: &'static str) -> BoundaryError {
    error("invalid_config", message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserCredentialPurpose {
    /// Fill one form field without submitting it.
    FormFill,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserCredentialSlot {
    /// Opaque model-visible alias.
    pub alias: String,
    /// Durable host-only reference into the private credential store.
    pub credential_ref: CredentialRef,
    /// Exact canonical origin (scheme, host and explicit port when non-default).
    pub origin: String,
    /// Only `form_fill` is supported.
    pub purpose: BrowserCredentialPurpose,
    /// Number of fill dispatches allowed for this Run.
    pub max_uses: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserCredentialSettings {
    pub enabled: bool,
    /// Private credential store directory. No value is ever read from model
    /// input.
    pub store: Option<PathBuf>,
    pub slots: Vec<BrowserCredentialSlot>,
}

/// Canonical exact-origin rendering used by slot validation and matching.
pub fn exact_origin(value: &str) -> Result<(NormalizedOrigin, String), BoundaryError> {
    let origin =
        NormalizedOrigin::parse(value).map_err(|_| invalid("Browser origin is invalid"))?;
    let scheme = match origin.scheme {
        crate::network::OriginScheme::Http => "http",
        crate::network::OriginScheme::Https => "https",
    };
    let canonical = format!("{scheme}://{}", origin.authority());
    if canonical != value {
        return Err(invalid(
            "Browser origin must be an exact canonical origin without a path",
        ));
    }
    Ok((origin, canonical))
}

/// Credential slots may target HTTPS only. A literal loopback HTTP origin is
/// allowed solely as the explicit local-development exception.
fn credential_origin_allowed(origin: &NormalizedOrigin) -> bool {
    match origin.scheme {
        crate::network::OriginScheme::Https => true,
        crate::network::OriginScheme::Http => {
            origin.host == "localhost"
                || origin
                    .host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }
    }
}

impl BrowserCredentialSettings {
    pub fn validate(&self) -> Result<(), BoundaryError> {
        if !self.enabled {
            if self.store.is_some() || !self.slots.is_empty() {
                return Err(invalid(
                    "Disabled browser credentials must not declare a store or slots",
                ));
            }
            return Ok(());
        }
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| invalid("Enabled browser credentials require a store path"))?;
        if !store.is_absolute()
            || store.as_os_str().is_empty()
            || store.to_str().is_none()
            || store.as_os_str().to_string_lossy().contains('\0')
        {
            return Err(invalid(
                "Browser credential store must be an absolute UTF-8 path",
            ));
        }
        if self.slots.is_empty() || self.slots.len() > MAX_CREDENTIAL_SLOTS {
            return Err(invalid(
                "Browser credential slots are empty or exceed their limit",
            ));
        }
        let mut aliases = std::collections::BTreeSet::new();
        for slot in &self.slots {
            if !valid_alias(&slot.alias) || !aliases.insert(slot.alias.as_str()) {
                return Err(invalid(
                    "Browser credential slot alias is invalid or duplicated",
                ));
            }
            if slot.max_uses == 0 || slot.max_uses > MAX_SLOT_USES {
                return Err(invalid("Browser credential slot max_uses is invalid"));
            }
            let (origin, _) = exact_origin(&slot.origin)?;
            if !credential_origin_allowed(&origin) {
                return Err(invalid(
                    "Browser credential slot origin must be HTTPS except an explicit loopback development origin",
                ));
            }
        }
        Ok(())
    }
}

pub fn valid_alias(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserFileSettings {
    pub enabled: bool,
    pub max_download_bytes: usize,
    pub max_upload_bytes: usize,
}

impl Default for BrowserFileSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_download_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
            max_upload_bytes: DEFAULT_MAX_UPLOAD_BYTES,
        }
    }
}

/// How long a named browser profile's state is kept. This deliberately relaxes
/// the default per-Run scratch guarantee, so the choice and its expiry are
/// always explicit host inputs — never implied and never model-controlled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserProfileRetention {
    /// State persists across Runs until the profile directory is removed by
    /// the operator. This is the only mode that keeps login state.
    #[default]
    Persist,
    /// State expires `seconds` after the profile was created, then the host
    /// removes the whole profile directory before the next launch.
    ExpiresAfter {
        #[serde(rename = "seconds")]
        seconds: u64,
    },
}

/// Opt-in persisted login/profile state (`[browser.profile]`). When enabled the
/// browser profile directory survives across Runs instead of living in the
/// per-Run scratch tree. This is an explicit relaxation of the per-Run scratch
/// guarantee and is OFF by default; see decision D-098.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserProfileSettings {
    pub enabled: bool,
    /// Opaque alias for the named profile. Model input never carries a path.
    pub name: Option<String>,
    /// Host-native absolute directory that owns the persisted profile tree. On
    /// the VM backend this is the remote POSIX root instead; the same field is
    /// reused because a Run selects exactly one backend.
    pub root: Option<PathBuf>,
    /// Remote POSIX root used only by the VM backend. Mutually exclusive with
    /// `root`: exactly one must be set for the selected backend.
    pub remote_root: Option<String>,
    /// Retention scope for the named profile.
    pub retention: BrowserProfileRetention,
}

impl BrowserProfileSettings {
    /// Validate the persisted-profile settings for the selected backend.
    ///
    /// A persistent `userDataDir` forces Playwright into persistent-context
    /// mode, which disables the private isolated-stdio processInfo and
    /// masked-capture channels. The VM backend still verifies the renderer
    /// sandbox through its remote `/proc` scan, so persisted profiles are
    /// offered only there; the native worker would lose its only sandbox
    /// evidence channel and is rejected fail-closed.
    pub fn validate(&self, vm_enabled: bool, remote_enabled: bool) -> Result<(), BoundaryError> {
        if !self.enabled {
            if self.name.is_some()
                || self.root.is_some()
                || self.remote_root.is_some()
                || self.retention != BrowserProfileRetention::Persist
            {
                return Err(invalid(
                    "Disabled browser profile must not declare a name, root or retention",
                ));
            }
            return Ok(());
        }
        let name = self
            .name
            .as_deref()
            .ok_or_else(|| invalid("Enabled browser profile requires a name"))?;
        if !valid_alias(name) {
            return Err(invalid("Browser profile name is invalid"));
        }
        if let BrowserProfileRetention::ExpiresAfter { seconds } = self.retention
            && !(60..=31_536_000).contains(&seconds)
        {
            return Err(invalid(
                "Browser profile expiry must be between 60 seconds and one year",
            ));
        }
        if !vm_enabled || remote_enabled {
            return Err(invalid(
                "Browser profile persistence requires the dedicated VM backend",
            ));
        }
        if self.root.is_some() {
            return Err(invalid(
                "Browser profile on the VM backend uses remote_root, not a host path",
            ));
        }
        let root = self
            .remote_root
            .as_deref()
            .ok_or_else(|| invalid("Enabled browser profile requires a remote root"))?;
        if !root.starts_with('/')
            || root == "/"
            || root.len() > 4096
            || root.chars().any(char::is_control)
            || root.split('/').any(|c| c == "..")
        {
            return Err(invalid(
                "Browser profile remote root must be a safe absolute POSIX path",
            ));
        }
        Ok(())
    }

    /// Resolved remote user-data directory for the VM launch. `None` when the
    /// feature is off so the launch is byte-for-byte unchanged.
    pub fn remote_user_data_dir(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let name = self.name.as_deref()?;
        let root = self.remote_root.as_deref()?;
        Some(format!("{}/{}", root.trim_end_matches('/'), name))
    }
}

/// Opt-in remote/CDP browser attach (`[browser.remote]`). The pinned worker
/// still runs inside the dedicated VM and stays identity-verified; only the
/// browser it drives changes from a managed Chromium to an explicit CDP
/// endpoint. That endpoint cannot satisfy the isolated-stdio + processInfo +
/// captureScreenshot private-channel contract, so masked screenshots,
/// downloads and uploads fail closed and are never claimed.
///
/// The remote browser lives outside the controlled boundary: the host cannot
/// inject the broker proxy into it, so only the endpoint's own loopback reach
/// is allowed. The endpoint must be a loopback literal on the VM side
/// (`ws://127.0.0.1:PORT` or `ws://[::1]:PORT`) and a matching
/// `[browser.dev_service]` forward must carry that exact port through the
/// controlled channel. Mutually exclusive with `[browser.profile]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserRemoteSettings {
    pub enabled: bool,
    /// Explicit CDP WebSocket endpoint on the VM's loopback. Never a model
    /// input and never resolved from a discovery URL.
    pub endpoint: Option<String>,
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
}

impl BrowserRemoteSettings {
    pub fn validate(&self) -> Result<(), BoundaryError> {
        if !(1_000..=120_000).contains(&self.connect_timeout_ms)
            || !(1_000..=600_000).contains(&self.request_timeout_ms)
        {
            return Err(invalid("Browser remote limits are invalid"));
        }
        if !self.enabled {
            if self.endpoint.is_some() {
                return Err(invalid(
                    "Disabled browser remote must not declare an endpoint",
                ));
            }
            return Ok(());
        }
        let endpoint = self
            .endpoint
            .as_deref()
            .ok_or_else(|| invalid("Enabled browser remote requires an endpoint"))?;
        let (host, port) = parse_remote_endpoint(endpoint)?;
        if !loopback_literal(&host) {
            return Err(invalid(
                "Browser remote endpoint must be a loopback literal on the VM side",
            ));
        }
        if port == 0 || port == crate::browser::network::VM_PROXY_PORT {
            return Err(invalid(
                "Browser remote endpoint port is invalid or collides with the in-VM proxy",
            ));
        }
        Ok(())
    }

    /// Parsed `(host, port)` of the configured endpoint. `None` when the
    /// feature is off so launch inputs stay byte-for-byte unchanged.
    pub fn endpoint_host_port(&self) -> Option<(String, u16)> {
        if !self.enabled {
            return None;
        }
        self.endpoint
            .as_deref()
            .and_then(|endpoint| parse_remote_endpoint(endpoint).ok())
    }

    /// The exact `ws://host:port` CDP origin passed to the worker. `None` when
    /// the feature is off. The worker sees the VM-side loopback address; the
    /// matching dev-service forward is what carries it to the host.
    pub fn cdp_endpoint(&self) -> Option<String> {
        let (host, port) = self.endpoint_host_port()?;
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        Some(format!("ws://{host}:{port}"))
    }
}

/// Parse a CDP WebSocket endpoint into its `(host, port)` without rewriting it.
/// Only `ws://` or `wss://` is accepted; credentials, a path, a query or a
/// fragment all fail closed because the endpoint is dialed verbatim.
fn parse_remote_endpoint(value: &str) -> Result<(String, u16), BoundaryError> {
    if value.is_empty()
        || value.len() > 2048
        || value.chars().any(char::is_control)
        || value.contains('\0')
    {
        return Err(invalid("Browser remote endpoint is invalid"));
    }
    let url =
        reqwest::Url::parse(value).map_err(|_| invalid("Browser remote endpoint is invalid"))?;
    if !matches!(url.scheme(), "ws" | "wss")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid(
            "Browser remote endpoint must be a ws/wss URL without credentials",
        ));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(invalid(
            "Browser remote endpoint must not carry a path, query or fragment",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| invalid("Browser remote endpoint host is missing"))?
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| invalid("Browser remote endpoint port is missing"))?;
    Ok((host, port))
}

/// Opt-in localhost dev-service forwarding (`[browser.dev_service]`). Each
/// entry maps an exact loopback host/port on the execution side to the same
/// host/port on the host through a controlled channel. Entries are exact only;
/// wildcards and hostnames other than loopback literals are refused.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserDevServiceForward {
    /// Opaque alias the operator uses to identify this forward.
    pub alias: String,
    /// Loopback host literal the page connects to (`127.0.0.1` or `::1`).
    pub host: String,
    /// Exact TCP port on both the execution side and the host.
    pub port: u16,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserDevServiceSettings {
    pub enabled: bool,
    /// Exact loopback forward entries. Bounded; duplicates are refused.
    pub forwards: Vec<BrowserDevServiceForward>,
}

impl Default for BrowserRemoteSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            connect_timeout_ms: 20_000,
            request_timeout_ms: 120_000,
        }
    }
}

pub const MAX_DEV_SERVICE_FORWARDS: usize = 16;

/// How the managed worker attaches to a browser for one Run on the dedicated
/// VM backend. `Isolated` is the default per-Run scratch managed Chromium and
/// is the only mode that satisfies the private-channel contract. The other two
/// are explicit opt-in relaxations that degrade facts fail-closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserAttachMode {
    /// Managed Chromium with a per-Run scratch profile (default).
    Isolated,
    /// Managed Chromium with a persisted `userDataDir` under a named profile.
    PersistentProfile {
        /// Remote absolute path the browser uses as its user-data directory.
        user_data_dir: String,
        /// Retention applied to the profile's creation stamp.
        retention: BrowserProfileRetention,
    },
    /// Attach to an explicit CDP endpoint reachable on the VM loopback via a
    /// dev-service forward. The browser is not managed or sandbox-verified.
    RemoteCdp {
        /// `ws://host:port` endpoint dialed verbatim by the worker.
        endpoint: String,
    },
}

impl BrowserAttachMode {
    /// Whether the private `isolated-stdio` processInfo + captureScreenshot
    /// channels are available for this mode. Only the isolated managed launch
    /// keeps them; both relaxations degrade to fail-closed facts.
    pub fn private_channels(&self) -> bool {
        matches!(self, BrowserAttachMode::Isolated)
    }
}

fn loopback_literal(value: &str) -> bool {
    let bare = value
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(value);
    !bare.is_empty()
        && bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

impl BrowserDevServiceSettings {
    pub fn validate(&self, vm_enabled: bool, network_enabled: bool) -> Result<(), BoundaryError> {
        if !self.enabled {
            if !self.forwards.is_empty() {
                return Err(invalid(
                    "Disabled browser dev_service must not declare forwards",
                ));
            }
            return Ok(());
        }
        if self.forwards.is_empty() || self.forwards.len() > MAX_DEV_SERVICE_FORWARDS {
            return Err(invalid(
                "Browser dev_service requires between one and sixteen forward entries",
            ));
        }
        // The dev-service listener rides the controlled channel, which exists
        // only on the dedicated VM backend (reverse forwards + netns socat).
        // The native worker's environment has no forwarding channel at all, so
        // dev_service is refused fail-closed there rather than silently
        // inert. The browser network policy is still required so page traffic
        // to the service can be authorized explicitly.
        if !vm_enabled {
            return Err(invalid(
                "Browser dev_service requires the dedicated VM backend",
            ));
        }
        if !network_enabled {
            return Err(invalid(
                "Browser dev_service requires the browser network policy",
            ));
        }
        let mut endpoints = std::collections::BTreeSet::new();
        let mut aliases = std::collections::BTreeSet::new();
        for forward in &self.forwards {
            if !valid_alias(&forward.alias) || !aliases.insert(forward.alias.as_str()) {
                return Err(invalid(
                    "Browser dev_service forward alias is invalid or duplicated",
                ));
            }
            if !loopback_literal(&forward.host) {
                return Err(invalid(
                    "Browser dev_service host must be a loopback literal",
                ));
            }
            if forward.port == 0 {
                return Err(invalid("Browser dev_service port must be nonzero"));
            }
            // A forward may not collide with the in-VM broker proxy port.
            if forward.port == crate::browser::network::VM_PROXY_PORT {
                return Err(invalid(
                    "Browser dev_service port collides with the in-VM proxy",
                ));
            }
            if !endpoints.insert((forward.host.clone(), forward.port)) {
                return Err(invalid(
                    "Browser dev_service forward endpoints must be unique",
                ));
            }
        }
        Ok(())
    }

    /// The forward that carries an exact `(host, port)` on the VM loopback, or
    /// `None`. Exact endpoints only; a wildcard never matches.
    pub fn forward_for(&self, host: &str, port: u16) -> Option<&BrowserDevServiceForward> {
        if !self.enabled {
            return None;
        }
        let bare = host
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .unwrap_or(host);
        let Ok(target) = bare.parse::<std::net::IpAddr>() else {
            return None;
        };
        self.forwards.iter().find(|forward| {
            forward.port == port
                && forward
                    .host
                    .strip_prefix('[')
                    .and_then(|inner| inner.strip_suffix(']'))
                    .unwrap_or(forward.host.as_str())
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|candidate| candidate == target)
        })
    }
}

impl BrowserFileSettings {
    pub fn validate(&self) -> Result<(), BoundaryError> {
        if self.max_download_bytes == 0
            || self.max_download_bytes > crate::media::MAX_FILE_BYTES as usize
            || self.max_upload_bytes == 0
            || self.max_upload_bytes > crate::media::MAX_FILE_BYTES as usize
        {
            return Err(invalid("Browser file broker limits are invalid"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn settings(origin: &str) -> BrowserCredentialSettings {
        BrowserCredentialSettings {
            enabled: true,
            store: Some(std::env::temp_dir().join("soyo-browser-credentials")),
            slots: vec![BrowserCredentialSlot {
                alias: "login".into(),
                credential_ref: CredentialRef::from_str("018f4f7e-7a3c-7c4e-9f5a-123456789abc")
                    .unwrap(),
                origin: origin.into(),
                purpose: BrowserCredentialPurpose::FormFill,
                max_uses: 1,
            }],
        }
    }

    #[test]
    fn credential_slots_require_https_except_loopback_development() {
        assert!(settings("https://login.example").validate().is_ok());
        assert!(settings("http://localhost:8080").validate().is_ok());
        assert!(settings("http://127.0.0.1:8080").validate().is_ok());
        assert!(settings("http://[::1]:8080").validate().is_ok());
        let error = settings("http://login.example").validate().unwrap_err();
        assert_eq!(error.0.code, "invalid_config");
        let error = settings("https://login.example/path")
            .validate()
            .unwrap_err();
        assert_eq!(error.0.code, "invalid_config");
    }

    // ------------------------------------------------------------------
    // Opt-in F-10 increments: profile, remote attach, dev_service.
    // Every increment is OFF by default and fails closed.
    // ------------------------------------------------------------------

    fn base_vm() -> crate::browser::vm::BrowserVmSettings {
        // A syntactically valid enabled VM block. Existence/permission checks
        // are the separate preflight step, not validate().
        crate::browser::vm::BrowserVmSettings {
            enabled: true,
            colima: Some(std::path::PathBuf::from("/opt/homebrew/bin/colima")),
            ssh: Some(std::path::PathBuf::from("/usr/bin/ssh")),
            profile: Some("soyo".into()),
            host: Some("127.0.0.1".into()),
            user: Some("soyo".into()),
            port: Some(49222),
            identity_file: Some(std::path::PathBuf::from("/tmp/soyo-vm/id_ed25519")),
            known_hosts: Some(std::path::PathBuf::from("/tmp/soyo-vm/known_hosts")),
            node: Some("/opt/soyo/node/bin/node".into()),
            worker_root: Some("/opt/soyo/worker".into()),
            chromium: Some("/opt/soyo/chromium/headless_shell".into()),
            identity_manifest: Some("/opt/soyo/identity.json".into()),
            identity_sha256: Some("a".repeat(64)),
            remote_temp_root: Some("/tmp/soyo".into()),
            ..Default::default()
        }
    }

    fn full() -> crate::browser::BrowserSettings {
        crate::browser::BrowserSettings::default()
    }

    #[test]
    fn all_increments_off_is_zero_drift() {
        let settings = full();
        // Defaults are the pre-increment surface exactly: managed isolated
        // attach, no remote, no profile, no forwards.
        assert_eq!(settings.attach_mode(), BrowserAttachMode::Isolated);
        assert!(settings.remote.endpoint_host_port().is_none());
        assert!(settings.remote.cdp_endpoint().is_none());
        assert!(settings.profile.remote_user_data_dir().is_none());
        assert!(
            settings
                .dev_service
                .forward_for("127.0.0.1", 3000)
                .is_none()
        );
        // A disabled BrowserSettings validates the same as before.
        settings.validate().unwrap();
    }

    #[test]
    fn profile_requires_vm_and_remote_root() {
        // Profile on the native worker fails closed: the private channel is
        // the only sandbox evidence there, and a persistent context drops it.
        let mut s = full();
        s.profile = BrowserProfileSettings {
            enabled: true,
            name: Some("work".into()),
            root: None,
            remote_root: Some("/home/soyo/profiles".into()),
            retention: BrowserProfileRetention::Persist,
        };
        s.worker.enabled = true;
        assert!(s.validate().is_err(), "profile on native worker must fail");

        // On the VM backend it resolves to a remote user-data dir.
        let mut s = full();
        s.vm = base_vm();
        s.network.enabled = true;
        s.network.public_web = true;
        s.profile = BrowserProfileSettings {
            enabled: true,
            name: Some("work".into()),
            root: None,
            remote_root: Some("/home/soyo/profiles".into()),
            retention: BrowserProfileRetention::Persist,
        };
        s.validate().unwrap();
        assert_eq!(
            s.attach_mode(),
            BrowserAttachMode::PersistentProfile {
                user_data_dir: "/home/soyo/profiles/work".into(),
                retention: BrowserProfileRetention::Persist,
            }
        );
    }

    #[test]
    fn profile_and_remote_are_mutually_exclusive() {
        let mut s = full();
        s.vm = base_vm();
        s.network.enabled = true;
        s.network.public_web = true;
        s.dev_service = BrowserDevServiceSettings {
            enabled: true,
            forwards: vec![BrowserDevServiceForward {
                alias: "cdp".into(),
                host: "127.0.0.1".into(),
                port: 9222,
            }],
        };
        s.remote = BrowserRemoteSettings {
            enabled: true,
            endpoint: Some("ws://127.0.0.1:9222".into()),
            connect_timeout_ms: 20_000,
            request_timeout_ms: 120_000,
        };
        s.profile = BrowserProfileSettings {
            enabled: true,
            name: Some("work".into()),
            root: None,
            remote_root: Some("/home/soyo/profiles".into()),
            retention: BrowserProfileRetention::Persist,
        };
        assert!(
            s.validate().is_err(),
            "remote attach and profile persistence must be mutually exclusive"
        );
    }

    #[test]
    fn remote_requires_matching_dev_service_forward() {
        // A remote endpoint with no matching forward cannot be reached from the
        // VM's private netns, so it fails closed.
        let mut s = full();
        s.vm = base_vm();
        s.network.enabled = true;
        s.network.public_web = true;
        s.remote = BrowserRemoteSettings {
            enabled: true,
            endpoint: Some("ws://127.0.0.1:9222".into()),
            connect_timeout_ms: 20_000,
            request_timeout_ms: 120_000,
        };
        // No dev_service at all → the endpoint is unreachable → fail closed.
        assert!(s.validate().is_err(), "remote without a forward must fail");

        // A forward on a different port still does not carry the endpoint.
        s.dev_service = BrowserDevServiceSettings {
            enabled: true,
            forwards: vec![BrowserDevServiceForward {
                alias: "other".into(),
                host: "127.0.0.1".into(),
                port: 3000,
            }],
        };
        assert!(
            s.validate().is_err(),
            "remote without the exact forward port must fail"
        );

        // The exact forward carries it.
        s.dev_service = BrowserDevServiceSettings {
            enabled: true,
            forwards: vec![BrowserDevServiceForward {
                alias: "cdp".into(),
                host: "127.0.0.1".into(),
                port: 9222,
            }],
        };
        s.validate().unwrap();
        assert_eq!(
            s.attach_mode(),
            BrowserAttachMode::RemoteCdp {
                endpoint: "ws://127.0.0.1:9222".into()
            }
        );
    }

    #[test]
    fn remote_endpoint_rejects_non_loopback_and_credentials() {
        for endpoint in [
            "ws://example.com:9222",
            "ws://192.168.1.5:9222",
            "ws://user:pw@127.0.0.1:9222",
            "http://127.0.0.1:9222",
            "ws://127.0.0.1:9222/json",
            "ws://127.0.0.1:9222?x=1",
        ] {
            let remote = BrowserRemoteSettings {
                enabled: true,
                endpoint: Some(endpoint.into()),
                connect_timeout_ms: 20_000,
                request_timeout_ms: 120_000,
            };
            assert!(
                remote.validate().is_err(),
                "endpoint {endpoint} must be rejected"
            );
        }
        // Loopback literals are accepted.
        for endpoint in [
            "ws://127.0.0.1:9222",
            "ws://[::1]:9222",
            "wss://127.0.0.1:9222",
        ] {
            let remote = BrowserRemoteSettings {
                enabled: true,
                endpoint: Some(endpoint.into()),
                connect_timeout_ms: 20_000,
                request_timeout_ms: 120_000,
            };
            remote
                .validate()
                .unwrap_or_else(|_| panic!("endpoint {endpoint} must be accepted"));
        }
    }

    #[test]
    fn dev_service_requires_vm_and_exact_loopback() {
        // dev_service on the native worker fails closed: there is no forward
        // channel there.
        let mut s = full();
        s.worker.enabled = true;
        s.network.enabled = true;
        s.network.public_web = true;
        s.dev_service = BrowserDevServiceSettings {
            enabled: true,
            forwards: vec![BrowserDevServiceForward {
                alias: "web".into(),
                host: "127.0.0.1".into(),
                port: 3000,
            }],
        };
        assert!(
            s.validate().is_err(),
            "dev_service on the native worker must fail"
        );

        // Non-loopback hosts and wildcards are refused at the entry level.
        for host in ["localhost", "0.0.0.0", "example.com", "10.0.0.5"] {
            let dev = BrowserDevServiceSettings {
                enabled: true,
                forwards: vec![BrowserDevServiceForward {
                    alias: "web".into(),
                    host: host.into(),
                    port: 3000,
                }],
            };
            assert!(
                dev.validate(true, true).is_err(),
                "dev_service host {host} must be rejected"
            );
        }
        // The in-VM proxy port may not be re-bound.
        let dev = BrowserDevServiceSettings {
            enabled: true,
            forwards: vec![BrowserDevServiceForward {
                alias: "web".into(),
                host: "127.0.0.1".into(),
                port: crate::browser::network::VM_PROXY_PORT,
            }],
        };
        assert!(dev.validate(true, true).is_err());
    }

    #[test]
    fn dev_service_forward_lookup_is_exact_not_wildcard() {
        let dev = BrowserDevServiceSettings {
            enabled: true,
            forwards: vec![
                BrowserDevServiceForward {
                    alias: "web".into(),
                    host: "127.0.0.1".into(),
                    port: 3000,
                },
                BrowserDevServiceForward {
                    alias: "v6".into(),
                    host: "::1".into(),
                    port: 4000,
                },
            ],
        };
        // Exact host+port matches; anything else returns None — never a prefix,
        // a wildcard or a different port on the same host.
        assert!(dev.forward_for("127.0.0.1", 3000).is_some());
        assert!(dev.forward_for("[::1]", 4000).is_some());
        assert!(dev.forward_for("::1", 4000).is_some());
        assert!(dev.forward_for("127.0.0.1", 3001).is_none());
        assert!(dev.forward_for("127.0.0.2", 3000).is_none());
        assert!(dev.forward_for("localhost", 3000).is_none());
    }

    #[test]
    fn masked_capture_engine_only_claimed_on_supported_target() {
        assert!(crate::browser::worker::masked_capture_engine_supported(
            "aarch64-unknown-linux-gnu"
        ));
        for unsupported in [
            "x86_64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-pc-windows-msvc",
        ] {
            assert!(
                !crate::browser::worker::masked_capture_engine_supported(unsupported),
                "{unsupported} must never claim the patched capture engine"
            );
        }
    }
}
