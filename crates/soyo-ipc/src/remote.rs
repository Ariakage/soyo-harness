//! Authenticated HTTP(S) helper transport (`F-13` D-113 path (b)).
//!
//! One connection is one long-lived `POST /rpc`: the request body carries the
//! NDJSON client frames and the response body carries the server frames, both
//! chunked. Authentication is `Authorization: Bearer <token>` where the token
//! was issued by the `F-18` credential layer (`soyo credentials token issue`)
//! and verifies against a stored digest plus a `run_kinds = ["remote"]` grant.
//!
//! Loopback endpoints may run plain HTTP for local testing; any non-loopback
//! bind requires TLS. On the client an explicit root CA (`ca_path`,
//! `SOYO_REMOTE_CA`) covers self-signed deployments, otherwise the platform
//! native roots apply.
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use soyo_core::credentials::CredentialStore;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::http::{
    HttpTunnel, MAX_HEAD_BYTES, parse_request_line, parse_status_line, read_head,
    write_error_response, write_tunnel_request, write_tunnel_response,
};
use crate::types::DesktopError;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);
const RPC_PATH: &str = "/rpc";

/// Duplex byte stream carrying the NDJSON tunnel (TLS or plain loopback TCP).
pub trait RemoteIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> RemoteIo for T {}

/// Boxed remote byte stream.
pub type RemoteStream = Box<dyn RemoteIo>;

fn invalid(message: impl Into<String>) -> DesktopError {
    DesktopError::new("invalid_config", message.into())
}

/// Scheme selected by a remote endpoint URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteScheme {
    Http,
    Https,
}

/// Parsed remote helper endpoint (`http://` loopback or `https://` anywhere).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEndpoint {
    pub scheme: RemoteScheme,
    pub host: String,
    pub port: u16,
    authority: String,
}

impl RemoteEndpoint {
    /// Parse `http://<host>[:<port>]` or `https://<host>[:<port>]`.
    ///
    /// `http://` is accepted only for loopback hosts: plaintext is a local
    /// testing path, never a remote-auth boundary.
    pub fn parse(raw: &str) -> Result<Self, DesktopError> {
        let fail = |detail: &str| invalid(format!("invalid remote endpoint '{raw}': {detail}"));
        let (scheme, rest) = if let Some(rest) = raw.strip_prefix("https://") {
            (RemoteScheme::Https, rest)
        } else if let Some(rest) = raw.strip_prefix("http://") {
            (RemoteScheme::Http, rest)
        } else {
            return Err(fail("scheme must be 'http' or 'https'"));
        };
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, path),
            None => (rest, ""),
        };
        if !path.is_empty() {
            return Err(fail("path is not supported; use the bare origin"));
        }
        if authority.is_empty()
            || authority.len() > 1024
            || authority.contains('@')
            || authority.contains('?')
            || authority.contains('#')
            || authority
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte == b' ')
        {
            return Err(fail("authority is invalid"));
        }
        let (host, port) = split_host_port(authority, scheme).map_err(|detail| fail(&detail))?;
        if scheme == RemoteScheme::Http && !host_is_loopback(&host) {
            return Err(fail(
                "plain http is allowed only for loopback hosts; use https",
            ));
        }
        Ok(Self {
            scheme,
            host,
            port,
            authority: authority.to_owned(),
        })
    }

    /// `host:port` as sent in the `Host` header.
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// Canonical URL used in messages and credential scopes.
    pub fn url(&self) -> String {
        let scheme = match self.scheme {
            RemoteScheme::Http => "http",
            RemoteScheme::Https => "https",
        };
        format!("{scheme}://{}", self.authority)
    }
}

fn split_host_port(authority: &str, scheme: RemoteScheme) -> Result<(String, u16), String> {
    let default_port = match scheme {
        RemoteScheme::Http => 80,
        RemoteScheme::Https => 443,
    };
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| "IPv6 host must be bracketed".to_owned())?;
        let port = match tail {
            "" => default_port,
            tail => {
                let port = tail
                    .strip_prefix(':')
                    .ok_or_else(|| "unexpected characters after IPv6 host".to_owned())?;
                parse_port(port)?
            }
        };
        (host.to_owned(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => {
                (host.to_owned(), parse_port(port)?)
            }
            _ => (authority.to_owned(), default_port),
        }
    };
    if host.is_empty() || host.len() > 255 {
        return Err("host is invalid".into());
    }
    let host_ok = host.parse::<IpAddr>().is_ok()
        || host.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-' || byte == b'_'
        });
    if !host_ok {
        return Err("host is invalid".into());
    }
    Ok((host, port))
}

fn parse_port(port: &str) -> Result<u16, String> {
    match port.parse::<u16>() {
        Ok(0) | Err(_) => Err("port must be between 1 and 65535".into()),
        Ok(port) => Ok(port),
    }
}

fn host_is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Path of a request; only the RPC endpoint is served.
pub fn is_rpc_path(target: &str) -> bool {
    target == RPC_PATH
}

/// One accepted connection from [`RemoteServer::accept`].
pub struct RemoteAccept {
    pub peer: SocketAddr,
    /// `Some` only after a successful bearer-token handshake; the byte stream
    /// is already the NDJSON tunnel.
    pub stream: Option<RemoteStream>,
}

/// HTTP(S) listener for the helper's remote transport.
pub struct RemoteServer {
    listener: TcpListener,
    acceptor: Option<TlsAcceptor>,
    address: SocketAddr,
    scheme: RemoteScheme,
}

impl RemoteServer {
    /// Bind `bind` with optional TLS material. Non-loopback binds require TLS.
    pub async fn bind(
        bind: SocketAddr,
        tls: Option<(PathBuf, PathBuf)>,
    ) -> Result<Self, DesktopError> {
        if !bind.ip().is_loopback() && tls.is_none() {
            return Err(invalid(
                "refusing a non-loopback remote bind without TLS; configure [remote.tls]",
            ));
        }
        let scheme = if tls.is_some() {
            RemoteScheme::Https
        } else {
            RemoteScheme::Http
        };
        let acceptor = tls
            .map(|(cert, key)| server_acceptor(&cert, &key))
            .transpose()?;
        let listener = TcpListener::bind(bind).await.map_err(|err| {
            DesktopError::new(
                "helper_bind",
                format!("cannot bind remote listener at {bind}: {err}"),
            )
        })?;
        let address = listener
            .local_addr()
            .map_err(|err| DesktopError::new("helper_bind", format!("remote local addr: {err}")))?;
        Ok(Self {
            listener,
            acceptor,
            address,
            scheme,
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn scheme(&self) -> RemoteScheme {
        self.scheme
    }

    /// Accept and authenticate one remote connection.
    ///
    /// Rejections (TLS failure, malformed head, missing/unknown/expired token)
    /// are answered with an HTTP error when possible, audited, and returned as
    /// `stream: None`; socket-level failures surface as `Err`.
    pub async fn accept(&self, store: &CredentialStore) -> io::Result<RemoteAccept> {
        let (tcp, peer) = self.listener.accept().await?;
        let _ = tcp.set_nodelay(true);
        let transport: RemoteStream = match &self.acceptor {
            Some(acceptor) => {
                let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp));
                match handshake.await {
                    Ok(Ok(stream)) => Box::new(stream),
                    Ok(Err(_)) | Err(_) => {
                        audit(
                            store,
                            "remote_tls_denied",
                            None,
                            vec![("peer", serde_json::json!(peer.to_string()))],
                        )
                        .await;
                        return Ok(RemoteAccept { peer, stream: None });
                    }
                }
            }
            None => Box::new(tcp),
        };
        let mut reader = BufReader::new(transport);
        let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut reader, MAX_HEAD_BYTES))
            .await
        {
            Ok(Ok(head)) => head,
            _ => {
                audit(
                    store,
                    "remote_auth_denied",
                    None,
                    vec![
                        ("peer", serde_json::json!(peer.to_string())),
                        ("reason", serde_json::json!("bad_request")),
                    ],
                )
                .await;
                let _ = write_error_response(reader.get_mut(), "400 Bad Request", &[]).await;
                return Ok(RemoteAccept { peer, stream: None });
            }
        };
        let request_ok = matches!(
            parse_request_line(&head),
            Ok((method, target)) if method == "POST" && is_rpc_path(target)
        );
        if !request_ok {
            audit(
                store,
                "remote_auth_denied",
                None,
                vec![
                    ("peer", serde_json::json!(peer.to_string())),
                    ("reason", serde_json::json!("bad_request")),
                ],
            )
            .await;
            let _ = write_error_response(reader.get_mut(), "404 Not Found", &[]).await;
            return Ok(RemoteAccept { peer, stream: None });
        }
        let Some(token) = bearer_token(&head).map(str::to_owned) else {
            audit(
                store,
                "remote_auth_denied",
                None,
                vec![
                    ("peer", serde_json::json!(peer.to_string())),
                    ("reason", serde_json::json!("missing_token")),
                ],
            )
            .await;
            let _ = write_error_response(
                reader.get_mut(),
                "401 Unauthorized",
                &[("WWW-Authenticate", "Bearer realm=\"soyo-helper\"")],
            )
            .await;
            return Ok(RemoteAccept { peer, stream: None });
        };
        // Token verification touches the private credential store; run it on a
        // blocking worker and audit the decision either way.
        let store_for_verify = store.clone();
        let verified =
            match tokio::task::spawn_blocking(move || store_for_verify.verify_remote_token(&token))
                .await
            {
                Ok(Ok(reference)) => Ok(reference),
                Ok(Err(err)) => Err(err.0.code),
                Err(_) => Err("credential_io".to_owned()),
            };
        match verified {
            Ok(reference) => {
                audit(
                    store,
                    "remote_auth_ok",
                    Some(reference),
                    vec![("peer", serde_json::json!(peer.to_string()))],
                )
                .await;
                if write_tunnel_response(reader.get_mut()).await.is_err() {
                    return Ok(RemoteAccept { peer, stream: None });
                }
                Ok(RemoteAccept {
                    peer,
                    stream: Some(Box::new(HttpTunnel::new(reader))),
                })
            }
            Err(err) => {
                audit(
                    store,
                    "remote_auth_denied",
                    None,
                    vec![
                        ("peer", serde_json::json!(peer.to_string())),
                        ("reason", serde_json::json!(err)),
                    ],
                )
                .await;
                let _ = write_error_response(
                    reader.get_mut(),
                    "401 Unauthorized",
                    &[("WWW-Authenticate", "Bearer realm=\"soyo-helper\"")],
                )
                .await;
                Ok(RemoteAccept { peer, stream: None })
            }
        }
    }
}

async fn audit(
    store: &CredentialStore,
    event: &'static str,
    reference: Option<soyo_core::credentials::CredentialRef>,
    extra: Vec<(&'static str, serde_json::Value)>,
) {
    let store = store.clone();
    let result = tokio::task::spawn_blocking(move || store.audit(event, reference, &extra)).await;
    if !matches!(result, Ok(Ok(()))) {
        eprintln!("soyo helper: remote auth audit event could not be written ({event})");
    }
}

fn bearer_token(head: &crate::http::HttpHead) -> Option<&str> {
    let value = head.header("authorization")?;
    let (scheme, token) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        let token = token.trim();
        if !token.is_empty() { Some(token) } else { None }
    } else {
        None
    }
}

fn server_acceptor(cert_path: &Path, key_path: &Path) -> Result<TlsAcceptor, DesktopError> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .map_err(|_| invalid("remote TLS certificate file is unreadable"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid("remote TLS certificate file is malformed"))?;
    if certs.is_empty() {
        return Err(invalid(
            "remote TLS certificate file contains no certificates",
        ));
    }
    let key = PrivateKeyDer::from_pem_file(key_path)
        .map_err(|_| invalid("remote TLS key file is unreadable or malformed"))?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| invalid("remote TLS protocol versions are unavailable"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|_| invalid("remote TLS certificate and key do not match"))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn client_roots(ca_path: Option<&Path>) -> Result<rustls::RootCertStore, DesktopError> {
    let mut roots = rustls::RootCertStore::empty();
    match ca_path {
        Some(path) => {
            let certs = CertificateDer::pem_file_iter(path)
                .map_err(|_| invalid("remote CA file is unreadable"))?;
            for cert in certs {
                let cert = cert.map_err(|_| invalid("remote CA file is malformed"))?;
                roots
                    .add(cert)
                    .map_err(|_| invalid("remote CA file contains an invalid certificate"))?;
            }
            if roots.is_empty() {
                return Err(invalid("remote CA file contains no certificates"));
            }
        }
        None => {
            let native = rustls_native_certs::load_native_certs();
            for cert in native.certs {
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                return Err(DesktopError::new(
                    "helper_unavailable",
                    "no platform root certificates are available for the remote helper",
                ));
            }
        }
    }
    Ok(roots)
}

/// Dial a remote helper, authenticate, and return the NDJSON tunnel.
///
/// The returned stream begins after the `Hello`-bearing response head, so
/// [`crate::HelperClient`] performs its normal protocol handshake over it.
pub async fn connect_remote(
    endpoint: &RemoteEndpoint,
    token: &str,
    ca_path: Option<&Path>,
) -> Result<RemoteStream, DesktopError> {
    if token.is_empty() || token.len() > 256 || token.chars().any(char::is_control) {
        return Err(invalid("remote bearer token is invalid"));
    }
    let unavailable = |err: std::io::Error| {
        DesktopError::new(
            "helper_unavailable",
            format!(
                "cannot connect to remote helper at {}: {err}",
                endpoint.url()
            ),
        )
    };
    let tcp = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
    )
    .await
    .map_err(|_| {
        DesktopError::new(
            "helper_unavailable",
            format!(
                "connecting to remote helper at {} timed out",
                endpoint.url()
            ),
        )
    })?
    .map_err(unavailable)?;
    let _ = tcp.set_nodelay(true);
    let transport: RemoteStream = match endpoint.scheme {
        RemoteScheme::Http => Box::new(tcp),
        RemoteScheme::Https => {
            let roots = client_roots(ca_path)?;
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|_| invalid("remote TLS protocol versions are unavailable"))?
                .with_root_certificates(roots)
                .with_no_client_auth();
            let connector = TlsConnector::from(Arc::new(config));
            let server_name = ServerName::try_from(endpoint.host.clone())
                .map_err(|_| invalid("remote endpoint host is not a valid TLS server name"))?;
            let stream =
                tokio::time::timeout(HANDSHAKE_TIMEOUT, connector.connect(server_name, tcp))
                    .await
                    .map_err(|_| {
                        DesktopError::new(
                            "helper_unavailable",
                            format!("TLS handshake with {} timed out", endpoint.url()),
                        )
                    })?
                    .map_err(unavailable)?;
            Box::new(stream)
        }
    };
    let mut reader = BufReader::new(transport);
    write_tunnel_request(reader.get_mut(), endpoint.authority(), token)
        .await
        .map_err(unavailable)?;
    let head =
        match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut reader, MAX_HEAD_BYTES)).await {
            Ok(Ok(head)) => head,
            Ok(Err(err)) => {
                return Err(DesktopError::new(
                    "helper_protocol",
                    format!("remote helper response head is invalid: {err}"),
                ));
            }
            Err(_) => {
                return Err(DesktopError::new(
                    "helper_protocol",
                    "remote helper did not answer the HTTP handshake",
                ));
            }
        };
    let status = parse_status_line(&head).map_err(|_| {
        DesktopError::new("helper_protocol", "remote helper status line is invalid")
    })?;
    if status == 401 || status == 403 {
        return Err(DesktopError::new(
            "helper_unauthorized",
            format!(
                "remote helper at {} rejected the bearer token",
                endpoint.url()
            ),
        ));
    }
    if status != 200 {
        return Err(DesktopError::new(
            "helper_unavailable",
            format!("remote helper at {} answered HTTP {status}", endpoint.url()),
        ));
    }
    let chunked = head.header("transfer-encoding").is_some_and(|value| {
        value
            .split(',')
            .any(|part| part.trim().eq_ignore_ascii_case("chunked"))
    });
    if !chunked {
        return Err(DesktopError::new(
            "helper_protocol",
            "remote helper response is not chunked; a proxy may be rewriting the tunnel",
        ));
    }
    Ok(Box::new(HttpTunnel::new(reader)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_and_loopback_http() {
        let https = RemoteEndpoint::parse("https://helper.example:8443").unwrap();
        assert_eq!(https.scheme, RemoteScheme::Https);
        assert_eq!(https.host, "helper.example");
        assert_eq!(https.port, 8443);
        assert_eq!(https.authority(), "helper.example:8443");
        assert_eq!(https.url(), "https://helper.example:8443");

        let default = RemoteEndpoint::parse("https://helper.example").unwrap();
        assert_eq!(default.port, 443);

        let v6 = RemoteEndpoint::parse("https://[::1]:9443").unwrap();
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.port, 9443);

        for raw in [
            "http://127.0.0.1:7100",
            "http://localhost:7100",
            "http://[::1]:7100",
        ] {
            let endpoint = RemoteEndpoint::parse(raw).unwrap();
            assert_eq!(endpoint.scheme, RemoteScheme::Http);
            assert!(host_is_loopback(&endpoint.host));
        }
        // A trailing slash is the same origin.
        assert!(RemoteEndpoint::parse("https://helper.example:8443/").is_ok());
    }

    #[test]
    fn rejects_plain_http_off_loopback_and_malformed_urls() {
        for raw in [
            "http://helper.example:7100",
            "http://192.168.1.5:7100",
            "ftp://127.0.0.1:7100",
            "https://",
            "https://user:pw@helper.example:8443",
            "https://helper.example:8443/rpc",
            "https://helper.example:8443?x=1",
            "https://helper.example:0",
            "https://helper.example:8443#f",
            "tcp:127.0.0.1:7100",
            "",
        ] {
            assert!(RemoteEndpoint::parse(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        let head =
            crate::http::parse_head(b"POST /rpc HTTP/1.1\r\nAuthorization: bearer abc\r\n\r\n")
                .unwrap();
        assert_eq!(bearer_token(&head), Some("abc"));
        let head =
            crate::http::parse_head(b"POST /rpc HTTP/1.1\r\nAuthorization: Basic abc\r\n\r\n")
                .unwrap();
        assert_eq!(bearer_token(&head), None);
    }
}
