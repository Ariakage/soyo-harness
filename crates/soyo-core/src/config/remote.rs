//! `[remote]` settings for authenticated HTTP(S) helper access (`F-13` D-113
//! path (b)). Absent section = disabled and zero drift: the daemon keeps its
//! Unix socket / loopback TCP endpoint and no token machinery is consulted.
//!
//! `[remote.client]` is the dial side: an endpoint URL, the environment
//! variable naming the bearer token, and an optional explicit root CA for
//! self-signed/test deployments.
use crate::contract::{BoundaryError, ErrorInfo};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub const DEFAULT_TOKEN_TTL_DAYS: u32 = 30;
pub const MAX_TOKEN_TTL_DAYS: u32 = 365;

fn invalid(message: &str) -> BoundaryError {
    BoundaryError(
        ErrorInfo {
            code: "invalid_config".into(),
            message: message.into(),
        },
        None,
        None,
    )
}

/// Explicit TLS material for the remote listener. Both paths must be set;
/// non-loopback binds require TLS.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteTlsSettings {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// Client-side dial settings. Lives in the same `[remote]` table so one
/// schema covers both ends; the daemon ignores `client` and vice versa.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteClientSettings {
    /// `http://` (loopback only) or `https://` endpoint of a remote helper.
    pub endpoint: Option<String>,
    /// Environment variable holding the bearer token. `SOYO_REMOTE_TOKEN`
    /// takes precedence when set.
    pub token_env: Option<String>,
    /// Explicit PEM root CA (self-signed or private CA). Omitted = platform
    /// native roots.
    pub ca_path: Option<PathBuf>,
}

/// `[remote]` section. `enabled` defaults to `false`; when enabled the helper
/// also binds `bind` and accepts authenticated HTTP(S) clients.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteSettings {
    pub enabled: bool,
    /// Listener address (`<ip>:<port>`; port `0` asks the OS). Non-loopback
    /// addresses require `tls`.
    pub bind: Option<String>,
    /// Bearer-token lifetime used by `soyo credentials token issue` when no
    /// explicit `--ttl-days` is given.
    pub token_ttl_days: u32,
    pub tls: Option<RemoteTlsSettings>,
    pub client: RemoteClientSettings,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: None,
            token_ttl_days: DEFAULT_TOKEN_TTL_DAYS,
            tls: None,
            client: RemoteClientSettings::default(),
        }
    }
}

impl RemoteSettings {
    /// Validate the section. Disabled sections are inert apart from shape and
    /// range checks, so a pre-written `[remote]` block changes nothing until
    /// it is enabled.
    pub fn validate(&self) -> Result<(), BoundaryError> {
        if self.token_ttl_days == 0 || self.token_ttl_days > MAX_TOKEN_TTL_DAYS {
            return Err(invalid(
                "remote token_ttl_days must be between 1 and 365 days",
            ));
        }
        if let Some(tls) = &self.tls
            && (tls.cert_path.as_os_str().is_empty() || tls.key_path.as_os_str().is_empty())
        {
            return Err(invalid("remote TLS requires both cert_path and key_path"));
        }
        if let Some(endpoint) = &self.client.endpoint {
            let scheme_ok = endpoint.starts_with("http://") || endpoint.starts_with("https://");
            if !scheme_ok || endpoint.len() > 2048 {
                return Err(invalid(
                    "remote client endpoint must be an http:// or https:// URL",
                ));
            }
        }
        if let Some(name) = &self.client.token_env
            && !crate::config::valid_env_name(name)
        {
            return Err(invalid("remote client token_env is not a valid name"));
        }
        if !self.enabled {
            return Ok(());
        }
        let bind = self
            .bind
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid("remote bind is required when remote is enabled"))?;
        let address = parse_bind(bind)?;
        if !address.ip().is_loopback() && self.tls.is_none() {
            return Err(invalid(
                "remote bind outside loopback requires TLS (cert_path/key_path)",
            ));
        }
        Ok(())
    }

    /// Parsed listener address; `None` when remote is disabled or unset.
    pub fn bind_addr(&self) -> Result<Option<SocketAddr>, BoundaryError> {
        self.bind
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(parse_bind)
            .transpose()
    }
}

fn parse_bind(value: &str) -> Result<SocketAddr, BoundaryError> {
    value
        .parse::<SocketAddr>()
        .map_err(|_| invalid("remote bind must be an IP address and port (e.g. 127.0.0.1:8443)"))
}

/// Lenient reader for `[remote.client]`, mirroring [`crate::config::helper_listen_override`]:
/// client-side endpoint resolution must not require a fully valid configuration
/// file. Returns `None` when the file or section is absent/unreadable.
pub fn remote_client_override(config_path: &Path) -> Option<RemoteClientSettings> {
    #[derive(Deserialize)]
    struct RemoteOnly {
        remote: Option<RemoteSection>,
    }
    #[derive(Deserialize)]
    struct RemoteSection {
        client: Option<RemoteClientSettings>,
    }
    let text = std::fs::read_to_string(config_path).ok()?;
    let doc: RemoteOnly = toml::from_str(&text).ok()?;
    doc.remote.and_then(|section| section.client)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(text: &str) -> RemoteSettings {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn absent_section_is_disabled_zero_drift() {
        let settings = RemoteSettings::default();
        assert!(!settings.enabled);
        assert_eq!(settings.token_ttl_days, DEFAULT_TOKEN_TTL_DAYS);
        assert_eq!(settings.bind_addr().unwrap(), None);
        settings.validate().unwrap();
        // TOML without the section produces the same defaults.
        assert_eq!(remote(""), settings);
    }

    #[test]
    fn non_loopback_bind_requires_tls() {
        let mut settings = remote(
            r#"enabled = true
bind = "0.0.0.0:8443"
"#,
        );
        assert_eq!(settings.validate().unwrap_err().0.code, "invalid_config");
        settings.tls = Some(RemoteTlsSettings {
            cert_path: PathBuf::from("/tmp/cert.pem"),
            key_path: PathBuf::from("/tmp/key.pem"),
        });
        settings.validate().unwrap();
    }

    #[test]
    fn loopback_bind_runs_without_tls_but_needs_an_address() {
        let mut settings = remote(
            r#"enabled = true
bind = "127.0.0.1:7101"
"#,
        );
        settings.validate().unwrap();
        assert!(settings.bind_addr().unwrap().unwrap().ip().is_loopback());
        settings.bind = None;
        assert!(settings.validate().is_err());
        settings.bind = Some("not-an-address".into());
        assert!(settings.validate().is_err());
    }

    #[test]
    fn ttl_and_tls_paths_are_bounded() {
        let mut settings = remote("token_ttl_days = 0");
        assert!(settings.validate().is_err());
        settings.token_ttl_days = MAX_TOKEN_TTL_DAYS + 1;
        assert!(settings.validate().is_err());
        settings.token_ttl_days = MAX_TOKEN_TTL_DAYS;
        assert!(settings.validate().is_ok());
        let settings = remote(
            r#"[tls]
cert_path = "/tmp/cert.pem"
key_path = ""
"#,
        );
        assert!(settings.validate().is_err());
    }

    #[test]
    fn disabled_section_does_not_require_bind() {
        let settings = remote(
            r#"enabled = false
bind = "0.0.0.0:8443"
"#,
        );
        // A disabled non-loopback bind is inert; validation only applies when
        // the listener is actually enabled.
        settings.validate().unwrap();
    }
}
