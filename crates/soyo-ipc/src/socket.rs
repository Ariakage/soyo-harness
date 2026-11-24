//! Helper endpoint resolution (socket path, pid file, lock file, peer identity).

#[cfg(not(unix))]
compile_error!("Soyo helper IPC requires a Unix platform (Linux or macOS)");

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use soyo_core::{
    config::{self, Config, Environment, Overrides, canonicalize_deepest},
    contract::BoundaryError,
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use tokio::net::UnixStream;

use crate::types::DesktopError;

/// Structured helper identity written atomically to `<fp>.pid` (`0600`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperPidRecord {
    pub pid: u32,
    pub started_at_id: u128,
    pub exe_path: String,
    pub fingerprint: String,
}

/// Configuration-scoped endpoint coordinates (`<state_dir>/helper/<fp>.sock`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperTarget {
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub fingerprint: String,
}

impl HelperTarget {
    /// Resolve the target from explicit CLI overrides and caller environment.
    pub fn from_overrides(
        config: Option<&Path>,
        data_dir: Option<&Path>,
        env: &Environment,
        process_cwd: &Path,
    ) -> Result<Self, BoundaryError> {
        let overrides = Overrides {
            config: config.map(Path::to_path_buf),
            data_dir: data_dir.map(Path::to_path_buf),
            ..Default::default()
        };
        let dirs = config::resolve_runtime_dirs(&overrides, env, process_cwd)?;
        Ok(Self::from_paths(
            dirs.config_path,
            dirs.data_dir,
            dirs.state_dir,
        ))
    }

    /// Resolve the target from the current process environment and working directory.
    pub fn from_environment() -> Result<Self, BoundaryError> {
        let env = Environment::capture();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        Self::from_overrides(None, None, &env, &cwd)
    }

    /// Build a target directly from an already-loaded `Config`.
    pub fn from_config(cfg: &Config) -> Self {
        Self::from_paths(
            cfg.config_path.clone(),
            cfg.data_dir.clone(),
            cfg.state_dir.clone(),
        )
    }

    /// Build from resolved paths and compute the canonical configuration fingerprint.
    pub fn from_paths(config_path: PathBuf, data_dir: PathBuf, state_dir: PathBuf) -> Self {
        let fingerprint = config_fingerprint(&config_path, &data_dir);
        Self {
            config_path,
            data_dir,
            state_dir,
            fingerprint,
        }
    }

    /// Directory storing sockets, locks, and pid files (`<state_dir>/helper/`),
    /// or `SOYO_HELPER_DIR` when explicitly overridden.
    pub fn helper_dir(&self) -> PathBuf {
        if let Ok(dir) = std::env::var("SOYO_HELPER_DIR")
            && !dir.trim().is_empty()
        {
            return PathBuf::from(dir);
        }
        self.state_dir.join("helper")
    }

    /// Whether `SOYO_HELPER_DIR` overrode the default `<state_dir>/helper` location.
    pub fn uses_env_helper_dir() -> bool {
        std::env::var("SOYO_HELPER_DIR").is_ok_and(|d| !d.trim().is_empty())
    }

    /// Filesystem path of this configuration's Unix domain socket.
    pub fn socket_path(&self) -> PathBuf {
        self.helper_dir().join(format!("{}.sock", self.fingerprint))
    }

    /// Filesystem path of this configuration's helper pid record (`0600`).
    pub fn pid_path(&self) -> PathBuf {
        self.helper_dir().join(format!("{}.pid", self.fingerprint))
    }

    /// Exclusive flock path coordinating bind and stale-socket reclamation.
    pub fn lock_path(&self) -> PathBuf {
        self.helper_dir().join(format!("{}.lock", self.fingerprint))
    }

    /// Resolve the endpoint clients should dial / the daemon should bind.
    ///
    /// Order: `SOYO_HELPER_LISTEN` environment value, then `[helper] listen` in
    /// the selected configuration file, then the default Unix socket derived
    /// from the configuration fingerprint.
    ///
    /// `SOYO_HELPER_LISTEN` also reaches a spawned `soyo helper` child through
    /// normal environment inheritance, keeping client and daemon on the same
    /// endpoint without extra flags.
    pub fn listen_endpoint(&self) -> Result<ListenEndpoint, DesktopError> {
        if let Ok(raw) = std::env::var("SOYO_HELPER_LISTEN")
            && !raw.trim().is_empty()
        {
            return self.resolve_listen(&raw);
        }
        if let Some(raw) = config::helper_listen_override(&self.config_path) {
            return self.resolve_listen(&raw);
        }
        Ok(ListenEndpoint::Unix(self.socket_path()))
    }

    fn resolve_listen(&self, raw: &str) -> Result<ListenEndpoint, DesktopError> {
        match ListenSpec::parse(raw)? {
            ListenSpec::Unix(None) => Ok(ListenEndpoint::Unix(self.socket_path())),
            ListenSpec::Unix(Some(path)) => Ok(ListenEndpoint::Unix(path)),
            ListenSpec::Tcp(addr) => Ok(ListenEndpoint::Tcp(addr)),
        }
    }

    /// Whether the resolved endpoint is TCP (dial-only: never spawns, never
    /// verifies peer uid). `false` on unresolvable configuration so callers
    /// keep the Unix path and surface the parse error from `connect_for`.
    pub fn is_tcp_endpoint(&self) -> bool {
        matches!(self.listen_endpoint(), Ok(ListenEndpoint::Tcp(_)))
    }
}

/// A configured helper endpoint, ready to bind or dial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenEndpoint {
    /// Unix domain socket path (fingerprint-derived or explicit `unix:<path>`).
    Unix(PathBuf),
    /// Loopback TCP address (`tcp:<host>:<port>`); remote clients reach it
    /// through SSH local forwarding (`ssh -L`).
    Tcp(SocketAddr),
}

/// The endpoint string as configured: `unix`, `unix:<path>`, or
/// `tcp:<host>:<port>` restricted to loopback addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenSpec {
    /// `unix` (no path → fingerprint-derived socket) or `unix:<path>`.
    Unix(Option<PathBuf>),
    /// `tcp:<host>:<port>` already restricted to a loopback `SocketAddr`.
    Tcp(SocketAddr),
}

impl ListenSpec {
    /// Parse an endpoint string; `tcp:` hosts must be a loopback address
    /// (`127.0.0.1`, `::1`, or `localhost`). Anything routable is refused:
    /// remote access is intended to arrive via SSH local forwarding, which
    /// still presents as a loopback connection to the daemon. A bare
    /// `host:port` (the D-113 shorthand) is accepted as `tcp:host:port`.
    pub fn parse(raw: &str) -> Result<Self, DesktopError> {
        let fail = |detail: &str| {
            DesktopError::new(
                "invalid_config",
                format!("invalid helper listen endpoint '{raw}': {detail}"),
            )
        };
        let (scheme, rest) = raw.split_once(':').unwrap_or((raw, ""));
        match scheme {
            "unix" => {
                if rest.is_empty() {
                    Ok(Self::Unix(None))
                } else {
                    Ok(Self::Unix(Some(PathBuf::from(rest))))
                }
            }
            "tcp" => Ok(Self::Tcp(parse_tcp_addr(rest).map_err(|e| fail(&e))?)),
            _ if looks_like_host_port(raw) => {
                Ok(Self::Tcp(parse_tcp_addr(raw).map_err(|e| fail(&e))?))
            }
            _ => Err(fail(
                "scheme must be 'unix' or 'tcp' (or a bare 'host:port')",
            )),
        }
    }
}

/// Recognize the bare `host:port` shorthand without swallowing other schemes
/// (`http:...`) or relative Unix paths.
fn looks_like_host_port(raw: &str) -> bool {
    raw.starts_with('[') || raw.starts_with("localhost:") || raw.parse::<SocketAddr>().is_ok()
}

fn parse_tcp_addr(host_port: &str) -> Result<SocketAddr, String> {
    let (host, port) = host_port
        .rsplit_once(':')
        .ok_or_else(|| "expected <host>:<port>".to_owned())?;
    let port: u16 = port
        .parse()
        .map_err(|_| "port must be a number between 0 and 65535".to_owned())?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let ip: IpAddr = match host {
        "localhost" => Ipv4Addr::LOCALHOST.into(),
        _ => host
            .parse::<IpAddr>()
            .map_err(|_| "host must be a loopback IP or 'localhost'".to_owned())?,
    };
    if !is_loopback(ip) {
        return Err("host must be a loopback address (127.0.0.1 or ::1); \
             remote access is reached via `ssh -L` forwarding"
            .to_owned());
    }
    Ok(SocketAddr::new(ip, port))
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Deterministic short fingerprint of `(canonical(config_path), canonical(data_dir))`.
/// Uses 10 lowercase hex chars (40 bits) so full paths fit within macOS 103-byte `sun_path`.
pub fn config_fingerprint(config_path: &Path, data_dir: &Path) -> String {
    let canon_cfg = canonicalize_deepest(config_path);
    let canon_data = canonicalize_deepest(data_dir);
    let mut hasher = Sha256::new();
    hasher.update(b"soyo-helper-v1\0");
    hasher.update(canon_cfg.as_os_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(canon_data.as_os_str().as_bytes());
    let digest = hasher.finalize();
    format!(
        "{:02x}{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3], digest[4]
    )
}

/// Default configuration's helper pid path.
pub fn helper_pid_path() -> PathBuf {
    HelperTarget::from_environment()
        .map(|target| target.pid_path())
        .unwrap_or_else(|_| helper_dir().join("default.pid"))
}

/// Base helper runtime directory (default target `<state_dir>/helper`, or `SOYO_HELPER_DIR`).
pub fn helper_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SOYO_HELPER_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    HelperTarget::from_environment()
        .map(|target| target.helper_dir())
        .unwrap_or_else(|_| PathBuf::from(".").join(".soyo-helper"))
}

/// Verify the connected Unix socket peer belongs to the current effective UID.
pub fn verify_peer_euid(stream: &UnixStream) -> Result<(), DesktopError> {
    let cred = stream.peer_cred().map_err(|err| {
        DesktopError::new(
            "helper_unauthorized",
            format!("Cannot verify IPC peer credentials: {err}"),
        )
    })?;
    // SAFETY: `geteuid` takes no pointers and retains no ownership.
    let expected_uid = unsafe { libc::geteuid() };
    if cred.uid() != expected_uid {
        return Err(DesktopError::new(
            "helper_unauthorized",
            "IPC peer UID does not match current user",
        ));
    }
    Ok(())
}

/// OS process start timestamp token used to defeat PID reuse on fallback `soyo kill`.
#[cfg(target_os = "linux")]
pub fn process_started_id(pid: u32) -> Option<u128> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    fields.get(19)?.parse().ok()
}

/// OS process start timestamp token used to defeat PID reuse on fallback `soyo kill`.
#[cfg(target_os = "macos")]
pub fn process_started_id(pid: u32) -> Option<u128> {
    let pid_i32 = i32::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // SAFETY: `proc_pidinfo` initializes `proc_bsdinfo` when return equals struct size.
    let size = unsafe {
        libc::proc_pidinfo(
            pid_i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    if size != std::mem::size_of::<libc::proc_bsdinfo>() as i32 {
        return None;
    }
    // SAFETY: complete buffer initialized above.
    let info = unsafe { info.assume_init() };
    Some((u128::from(info.pbi_start_tvsec) << 64) | u128::from(info.pbi_start_tvusec))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_started_id(_pid: u32) -> Option<u128> {
    None
}

/// Live executable path for `pid` on Linux/macOS to confirm process identity before signaling.
#[cfg(target_os = "linux")]
pub fn process_exe_path(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// Live executable path for `pid` on Linux/macOS to confirm process identity before signaling.
#[cfg(target_os = "macos")]
pub fn process_exe_path(pid: u32) -> Option<PathBuf> {
    let pid_i32 = i32::try_from(pid).ok()?;
    let max_len = usize::try_from(libc::PROC_PIDPATHINFO_MAXSIZE).ok()?;
    let mut buf = vec![0u8; max_len];
    // SAFETY: `proc_pidpath` writes up to `buf.len()` bytes into `buf` and returns written byte count.
    let len = unsafe {
        libc::proc_pidpath(
            pid_i32,
            buf.as_mut_ptr().cast(),
            u32::try_from(buf.len()).ok()?,
        )
    };
    if len <= 0 {
        return None;
    }
    let slice = &buf[..usize::try_from(len).ok()?];
    let os = std::ffi::OsStr::from_bytes(slice);
    Some(PathBuf::from(os))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_exe_path(_pid: u32) -> Option<PathBuf> {
    None
}

/// Convenience helper for accessing the current environment's endpoint path.
pub struct HelperEndpoint;

impl HelperEndpoint {
    pub fn socket_path() -> Option<PathBuf> {
        HelperTarget::from_environment()
            .ok()
            .map(|target| target.socket_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    fn listen_spec_unix_forms() {
        assert_eq!(ListenSpec::parse("unix").unwrap(), ListenSpec::Unix(None));
        assert_eq!(ListenSpec::parse("unix:").unwrap(), ListenSpec::Unix(None));
        assert_eq!(
            ListenSpec::parse("unix:/tmp/x.sock").unwrap(),
            ListenSpec::Unix(Some(PathBuf::from("/tmp/x.sock")))
        );
        // Relative socket paths pass through unchanged; the daemon anchors them.
        assert_eq!(
            ListenSpec::parse("unix:rel/x.sock").unwrap(),
            ListenSpec::Unix(Some(PathBuf::from("rel/x.sock")))
        );
    }

    #[test]
    fn listen_spec_tcp_loopback_forms() {
        assert_eq!(
            ListenSpec::parse("tcp:127.0.0.1:7100").unwrap(),
            ListenSpec::Tcp(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 7100))
        );
        assert_eq!(
            ListenSpec::parse("tcp:[::1]:7100").unwrap(),
            ListenSpec::Tcp(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 7100))
        );
        assert_eq!(
            ListenSpec::parse("tcp:::1:7100").unwrap(),
            ListenSpec::Tcp(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 7100))
        );
        assert_eq!(
            ListenSpec::parse("tcp:localhost:0").unwrap(),
            ListenSpec::Tcp(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
        );
        // Bare `host:port` shorthand (the D-113 `--listen 127.0.0.1:PORT` form).
        assert_eq!(
            ListenSpec::parse("127.0.0.1:7100").unwrap(),
            ListenSpec::Tcp(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 7100))
        );
        assert_eq!(
            ListenSpec::parse("localhost:7100").unwrap(),
            ListenSpec::Tcp(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 7100))
        );
        assert_eq!(
            ListenSpec::parse("[::1]:7100").unwrap(),
            ListenSpec::Tcp(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 7100))
        );
    }

    #[test]
    fn listen_spec_tcp_rejects_non_loopback() {
        for raw in [
            "tcp:0.0.0.0:7100",
            "tcp:::7100",
            "tcp:192.168.1.5:80",
            "tcp:8.8.8.8:443",
            "tcp:example.com:80",
            "tcp:::ffff:8.8.8.8:80",
            "0.0.0.0:7100",
            "192.168.1.5:80",
        ] {
            let err = ListenSpec::parse(raw).unwrap_err();
            assert_eq!(err.code, "invalid_config", "{raw}");
        }
    }

    #[test]
    fn listen_spec_rejects_malformed() {
        for raw in [
            "",
            "tcp:",
            "tcp:127.0.0.1",
            "tcp:127.0.0.1:abc",
            "tcp:127.0.0.1:99999",
            "http:127.0.0.1:80",
            "example.com:80",
        ] {
            assert!(ListenSpec::parse(raw).is_err(), "{raw}");
        }
    }
}
