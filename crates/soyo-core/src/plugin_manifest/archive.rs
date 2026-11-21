//! Bounded archive inspection, SHA-256 / Ed25519 verification, and
//! SSRF-safe HTTPS retrieval for `soyo-plugin` packages (`F-03`).

use super::{
    FileBudget, MANIFEST_NAME, MAX_MANIFEST_BYTES, MAX_PACKAGE_BYTES, MAX_PACKAGE_FILE_BYTES,
    PackageSource, PluginError, Result, SourceKind, VerifiedPackage, invalid,
    package::{finalize_package, make_package_file},
    validate_relative_path,
};
use base64::Engine;
use flate2::read::GzDecoder;
use futures::StreamExt;
use ring::signature;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    net::{IpAddr, SocketAddr},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    time::Duration,
};
use tar::{Archive, EntryType};

pub const MAX_ARCHIVE_BYTES: u64 = MAX_PACKAGE_BYTES;
const MAX_HTTPS_REDIRECTS: usize = 3;
const ARCHIVE_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

fn archive_err(code: &'static str, message: &'static str) -> PluginError {
    PluginError { code, message }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Tar,
    TarGz,
}

impl ArchiveFormat {
    pub fn detect(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        detect_format_name(name)
    }

    pub fn from_bytes_or_url(url: &reqwest::Url, bytes: &[u8]) -> Self {
        if let Some(last) = url.path_segments().and_then(|mut segs| segs.next_back())
            && let Some(fmt) = detect_format_name(last)
        {
            return fmt;
        }
        if bytes.starts_with(&[0x1f, 0x8b]) {
            Self::TarGz
        } else {
            Self::Tar
        }
    }
}

fn detect_format_name(name: &str) -> Option<ArchiveFormat> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        Some(ArchiveFormat::TarGz)
    } else if lower.ends_with(".tar") {
        Some(ArchiveFormat::Tar)
    } else {
        None
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveVerification {
    pub sha256: Option<String>,
    pub ed25519_pubkey: Option<String>,
    pub ed25519_signature: Option<String>,
}

impl ArchiveVerification {
    pub fn validate(&self, require_verification: bool) -> Result<()> {
        if let Some(sha) = &self.sha256 {
            decode_exact_bytes(sha, 32)
                .map_err(|_| invalid("sha256 checksum must be 64 hex characters"))?;
        }
        match (&self.ed25519_pubkey, &self.ed25519_signature) {
            (Some(pk), Some(sig)) => {
                decode_key_or_sig(pk, 32)
                    .map_err(|_| invalid("ed25519 public key must be 32 bytes (hex or base64)"))?;
                decode_key_or_sig(sig, 64)
                    .map_err(|_| invalid("ed25519 signature must be 64 bytes (hex or base64)"))?;
            }
            (None, None) => {}
            _ => {
                return Err(invalid(
                    "both ed25519 public key and signature are required together",
                ));
            }
        }
        if require_verification && self.sha256.is_none() && self.ed25519_signature.is_none() {
            return Err(archive_err(
                "plugin_verification_required",
                "remote HTTPS plugin fetch requires --sha256 or --pubkey with --signature",
            ));
        }
        Ok(())
    }
}

fn decode_exact_bytes(hex_str: &str, expected_len: usize) -> std::result::Result<Vec<u8>, ()> {
    if hex_str.len() != expected_len * 2 || !hex_str.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(());
    }
    let bytes = hex_str.as_bytes();
    let mut out = Vec::with_capacity(expected_len);
    for pair in bytes.as_chunks::<2>().0 {
        let s = std::str::from_utf8(pair).map_err(|_| ())?;
        out.push(u8::from_str_radix(s, 16).map_err(|_| ())?);
    }
    Ok(out)
}

fn decode_key_or_sig(raw: &str, expected_len: usize) -> std::result::Result<Vec<u8>, ()> {
    let trimmed = raw.trim();
    if let Ok(bytes) = decode_exact_bytes(trimmed, expected_len) {
        return Ok(bytes);
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .map_err(|_| ())?;
    if decoded.len() == expected_len {
        Ok(decoded)
    } else {
        Err(())
    }
}

/// Compute the lowercase hex SHA-256 checksum of an archive byte slice.
pub fn archive_sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn verify_sha256_hex(bytes: &[u8], expected_hex: &str) -> Result<()> {
    let expected = decode_exact_bytes(expected_hex.trim(), 32)
        .map_err(|_| invalid("sha256 checksum must be 64 hex characters"))?;
    let actual = Sha256::digest(bytes);
    let diff = actual
        .as_slice()
        .iter()
        .zip(expected.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    if diff != 0 {
        return Err(archive_err(
            "plugin_checksum_mismatch",
            "plugin archive SHA-256 checksum verification failed",
        ));
    }
    Ok(())
}

pub fn verify_ed25519_signature(bytes: &[u8], pubkey: &str, signature_text: &str) -> Result<()> {
    let key_bytes = decode_key_or_sig(pubkey, 32)
        .map_err(|_| invalid("ed25519 public key must be 32 bytes (hex or base64)"))?;
    let sig_bytes = decode_key_or_sig(signature_text, 64)
        .map_err(|_| invalid("ed25519 signature must be 64 bytes (hex or base64)"))?;
    let verifier = signature::UnparsedPublicKey::new(&signature::ED25519, &key_bytes);
    verifier.verify(bytes, &sig_bytes).map_err(|_| {
        archive_err(
            "plugin_signature_invalid",
            "plugin archive Ed25519 signature verification failed",
        )
    })
}

pub fn verify_archive_bytes(bytes: &[u8], verification: &ArchiveVerification) -> Result<()> {
    verification.validate(false)?;
    if let Some(expected_sha) = &verification.sha256 {
        verify_sha256_hex(bytes, expected_sha)?;
    }
    if let (Some(pubkey), Some(sig)) = (
        &verification.ed25519_pubkey,
        &verification.ed25519_signature,
    ) {
        verify_ed25519_signature(bytes, pubkey, sig)?;
    }
    Ok(())
}

/// Read, validate, and seal a local `.tar`, `.tar.gz`, or `.tgz` file without
/// extracting anything to disk. Symlinks, hardlinks, device nodes, path
/// traversal, zip bombs, and ambiguous case/Unicode collisions are rejected.
pub fn inspect_archive_path(
    path: &Path,
    verification: &ArchiveVerification,
) -> Result<VerifiedPackage> {
    verification.validate(false)?;
    let format = ArchiveFormat::detect(path).ok_or_else(|| {
        archive_err(
            "plugin_source_unsupported",
            "Only directories and .tar, .tar.gz, or .tgz plugin archives are supported",
        )
    })?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| invalid("archive file cannot be opened safely"))?;
    let meta = file
        .metadata()
        .map_err(|_| invalid("archive metadata cannot be inspected"))?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(invalid(
            "archive file must be a regular non-hardlinked file",
        ));
    }
    if meta.len() > MAX_ARCHIVE_BYTES {
        return Err(invalid(
            "plugin archive exceeds maximum compressed byte limit",
        ));
    }
    let mut raw = Vec::with_capacity(meta.len() as usize);
    (&mut file)
        .take(MAX_ARCHIVE_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|_| invalid("archive file could not be read"))?;
    if raw.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(invalid("plugin archive exceeds maximum byte limit"));
    }
    verify_archive_bytes(&raw, verification)?;
    inspect_archive_bytes(&raw, format)
}

/// Inspect in-memory `.tar` or `.tar.gz` bytes strictly within quota limits.
pub fn inspect_archive_bytes(
    archive_bytes: &[u8],
    format: ArchiveFormat,
) -> Result<VerifiedPackage> {
    if archive_bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(invalid("plugin archive exceeds maximum byte limit"));
    }
    let owned_tar: Vec<u8>;
    let tar_slice: &[u8] = match format {
        ArchiveFormat::Tar => archive_bytes,
        ArchiveFormat::TarGz => {
            let decoder = GzDecoder::new(archive_bytes);
            let mut bounded = decoder.take(MAX_ARCHIVE_BYTES + 1);
            let mut out = Vec::new();
            bounded
                .read_to_end(&mut out)
                .map_err(|_| invalid("gzip plugin archive is invalid or truncated"))?;
            if out.len() as u64 > MAX_ARCHIVE_BYTES {
                return Err(invalid("decompressed archive exceeds maximum byte limit"));
            }
            owned_tar = out;
            &owned_tar
        }
    };

    let mut archive = Archive::new(tar_slice);
    let mut budget = FileBudget::default();
    let mut files = Vec::new();
    let entries = archive
        .entries()
        .map_err(|_| invalid("tar archive headers could not be parsed"))?;

    for entry_result in entries {
        let mut entry = entry_result.map_err(|_| invalid("tar archive entry header is corrupt"))?;
        let entry_type = entry.header().entry_type();
        let normalized = {
            let raw_path = entry
                .path()
                .map_err(|_| invalid("archive entry path is not valid UTF-8"))?;
            let path_str = raw_path
                .to_str()
                .ok_or_else(|| invalid("archive entry path is not UTF-8"))?;
            normalize_archive_path(path_str, entry_type == EntryType::Directory)?.to_owned()
        };

        if normalized.is_empty() {
            if entry_type == EntryType::Directory {
                continue;
            }
            return Err(invalid("archive root entry must be a directory"));
        }
        validate_relative_path(&normalized)?;

        match entry_type {
            EntryType::Directory => {
                budget.reserve_directory(&normalized)?;
            }
            EntryType::Regular => {
                let declared_size = entry
                    .header()
                    .size()
                    .map_err(|_| invalid("archive entry size is invalid"))?;
                if declared_size > MAX_PACKAGE_FILE_BYTES
                    || (normalized == MANIFEST_NAME && declared_size > MAX_MANIFEST_BYTES as u64)
                {
                    return Err(invalid("archive file exceeds its byte limit"));
                }
                budget.reserve_file(&normalized, declared_size)?;
                let mut content = Vec::with_capacity(declared_size as usize);
                (&mut entry)
                    .take(MAX_PACKAGE_FILE_BYTES + 1)
                    .read_to_end(&mut content)
                    .map_err(|_| invalid("archive file payload could not be read"))?;
                if content.len() as u64 != declared_size {
                    return Err(invalid("archive file size does not match header"));
                }
                files.push(make_package_file(normalized, content));
            }
            _ => {
                return Err(invalid(
                    "archive symlinks, hardlinks, and special files are forbidden",
                ));
            }
        }
    }

    finalize_package(files, &budget)
}

fn normalize_archive_path(raw: &str, is_dir: bool) -> Result<&str> {
    if raw.contains('\\') || raw.contains('\0') {
        return Err(invalid("archive path contains forbidden separators"));
    }
    let mut trimmed = raw;
    if let Some(rest) = trimmed.strip_prefix("./") {
        trimmed = rest;
    } else if trimmed == "." || trimmed == "./" {
        return Ok("");
    }
    if is_dir {
        trimmed = trimmed.trim_end_matches('/');
        if trimmed == "." {
            return Ok("");
        }
    }
    Ok(trimmed)
}

pub async fn fetch_https_archive(
    raw_url: &str,
    verification: &ArchiveVerification,
) -> Result<(VerifiedPackage, PackageSource)> {
    verification.validate(true)?;
    if raw_url.len() > 2048 || raw_url.chars().any(crate::text::is_terminal_control) {
        return Err(invalid(
            "plugin HTTPS URL exceeds bounds or contains control chars",
        ));
    }
    let mut current_url = validate_https_url(raw_url)?;
    let initial_reference = current_url.as_str().to_owned();

    let (final_url, bytes) = tokio::time::timeout(ARCHIVE_FETCH_TIMEOUT, async {
        for hop in 0..=MAX_HTTPS_REDIRECTS {
            let host = current_url
                .host_str()
                .ok_or_else(|| invalid("plugin HTTPS URL must contain a host"))?
                .to_owned();
            let port = current_url
                .port_or_known_default()
                .ok_or_else(|| invalid("plugin HTTPS URL port is unknown"))?;
            let resolved = resolve_checked_address(&host, port).await?;
            let mut builder = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(ARCHIVE_FETCH_TIMEOUT)
                .pool_max_idle_per_host(0)
                .user_agent(concat!("soyo-plugin/", env!("CARGO_PKG_VERSION")));
            if host.parse::<IpAddr>().is_err() {
                builder = builder.resolve(&host, SocketAddr::new(resolved, port));
            }
            let client = builder.build().map_err(|_| {
                archive_err("plugin_fetch_failed", "HTTPS plugin client build failed")
            })?;
            let response =
                client.get(current_url.clone()).send().await.map_err(|_| {
                    archive_err("plugin_fetch_failed", "HTTPS plugin download failed")
                })?;
            if response.status().is_redirection() {
                if hop >= MAX_HTTPS_REDIRECTS {
                    return Err(archive_err(
                        "plugin_fetch_failed",
                        "HTTPS plugin download exceeded maximum redirects",
                    ));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        archive_err("plugin_fetch_failed", "HTTPS redirect is missing Location")
                    })?;
                let next = current_url.join(location).map_err(|_| {
                    archive_err("plugin_fetch_failed", "HTTPS redirect Location is invalid")
                })?;
                current_url = validate_https_url(next.as_str())?;
                continue;
            }
            if !response.status().is_success() {
                return Err(archive_err(
                    "plugin_fetch_failed",
                    "HTTPS plugin server returned an error status",
                ));
            }
            if let Some(len) = response.content_length()
                && len > MAX_ARCHIVE_BYTES
            {
                return Err(invalid("remote plugin archive exceeds maximum byte limit"));
            }
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| {
                    archive_err(
                        "plugin_fetch_failed",
                        "HTTPS plugin stream failed mid-transfer",
                    )
                })?;
                if (body.len() + chunk.len()) as u64 > MAX_ARCHIVE_BYTES {
                    return Err(invalid("remote plugin archive exceeds maximum byte limit"));
                }
                body.extend_from_slice(&chunk);
            }
            return Ok((current_url, body));
        }
        Err(archive_err(
            "plugin_fetch_failed",
            "HTTPS plugin download exceeded maximum redirects",
        ))
    })
    .await
    .map_err(|_| archive_err("plugin_fetch_failed", "HTTPS plugin download timed out"))??;

    verify_archive_bytes(&bytes, verification)?;
    let format = ArchiveFormat::from_bytes_or_url(&final_url, &bytes);
    let package = inspect_archive_bytes(&bytes, format)?;
    let source = PackageSource {
        kind: SourceKind::Https,
        reference: initial_reference,
    };
    source.validate()?;
    Ok((package, source))
}

fn validate_https_url(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).map_err(|_| invalid("plugin source is not a valid URL"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        return Err(archive_err(
            "plugin_source_unsupported",
            "Remote plugin downloads require HTTPS without embedded credentials or fragments",
        ));
    }
    Ok(url)
}

async fn resolve_checked_address(host: &str, port: u16) -> Result<IpAddr> {
    let clean_host = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = clean_host.parse::<IpAddr>() {
        if !crate::network::is_public_address(ip) {
            return Err(archive_err(
                "plugin_fetch_failed",
                "Remote plugin host resolves to a non-public IP address",
            ));
        }
        return Ok(ip);
    }
    let addrs = tokio::net::lookup_host((clean_host, port))
        .await
        .map_err(|_| {
            archive_err(
                "plugin_fetch_failed",
                "Failed to resolve remote plugin host",
            )
        })?;
    let mut chosen = None;
    for addr in addrs {
        let ip = addr.ip();
        if !crate::network::is_public_address(ip) {
            return Err(archive_err(
                "plugin_fetch_failed",
                "Remote plugin host resolved to a non-public IP address",
            ));
        }
        chosen.get_or_insert(ip);
    }
    chosen.ok_or_else(|| {
        archive_err(
            "plugin_fetch_failed",
            "Remote plugin DNS returned no addresses",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use ring::{
        rand::SystemRandom,
        signature::{Ed25519KeyPair, KeyPair},
    };
    use std::io::Write;
    use tar::{Builder, Header};

    const MANIFEST: &str = r#"schema_version = 1
[plugin]
id = "io.soyo.archive-test"
version = "1.0.0"
description = "Archive verification test fixture."
license = "MIT"
[compatibility]
requires_soyo = ">=0.2.0, <1.0.0"
requires_features = ["plugin-manifest-v1"]
[capabilities]
requested = ["workspace.read"]
[[resources]]
id = "readme"
path = "docs/readme.txt"
media_type = "text/plain"
"#;

    fn build_tar_gz(entries: &[(&str, EntryType, &[u8], Option<&str>)]) -> Vec<u8> {
        let mut tar_buf = Vec::new();
        {
            let mut builder = Builder::new(&mut tar_buf);
            for (path, kind, body, link) in entries {
                let mut header = Header::new_gnu();
                header.set_entry_type(*kind);
                header.set_mode(if *kind == EntryType::Directory {
                    0o755
                } else {
                    0o644
                });
                header.set_size(body.len() as u64);
                if let Some(target) = link {
                    header.set_link_name(target).unwrap();
                }
                header.set_cksum();
                builder.append_data(&mut header, path, *body).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar_buf).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn tar_gz_archive_matches_directory_digest_and_verifies_ed25519_and_sha256() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(MANIFEST_NAME), MANIFEST).unwrap();
        std::fs::create_dir(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/readme.txt"), b"hello archive").unwrap();
        let from_dir = crate::plugin_manifest::inspect_package(dir.path()).unwrap();

        let archive_bytes = build_tar_gz(&[
            ("./", EntryType::Directory, b"", None),
            (
                "./soyo-plugin.toml",
                EntryType::Regular,
                MANIFEST.as_bytes(),
                None,
            ),
            ("./docs/", EntryType::Directory, b"", None),
            (
                "./docs/readme.txt",
                EntryType::Regular,
                b"hello archive",
                None,
            ),
        ]);
        let from_archive = inspect_archive_bytes(&archive_bytes, ArchiveFormat::TarGz).unwrap();
        assert_eq!(from_archive.digest(), from_dir.digest());
        assert_eq!(
            from_archive.capability_fingerprint(),
            from_dir.capability_fingerprint()
        );

        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let keypair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let pub_hex: String = keypair
            .public_key()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let sig_b64 =
            base64::engine::general_purpose::STANDARD.encode(keypair.sign(&archive_bytes).as_ref());
        let sha_hex: String = Sha256::digest(&archive_bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let verified = ArchiveVerification {
            sha256: Some(sha_hex.clone()),
            ed25519_pubkey: Some(pub_hex.clone()),
            ed25519_signature: Some(sig_b64.clone()),
        };
        verify_archive_bytes(&archive_bytes, &verified).unwrap();

        let mut tampered = archive_bytes.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert_eq!(
            verify_sha256_hex(&tampered, &sha_hex).unwrap_err().code,
            "plugin_checksum_mismatch"
        );
        assert_eq!(
            verify_ed25519_signature(&tampered, &pub_hex, &sig_b64)
                .unwrap_err()
                .code,
            "plugin_signature_invalid"
        );
    }

    #[test]
    fn archive_rejects_symlinks_hardlinks_and_traversal() {
        let symlink_tar = build_tar_gz(&[
            (
                "soyo-plugin.toml",
                EntryType::Regular,
                MANIFEST.as_bytes(),
                None,
            ),
            ("evil-link", EntryType::Symlink, b"", Some("/etc/passwd")),
        ]);
        assert_eq!(
            inspect_archive_bytes(&symlink_tar, ArchiveFormat::TarGz)
                .unwrap_err()
                .code,
            "invalid_plugin"
        );

        let hardlink_tar = build_tar_gz(&[
            (
                "soyo-plugin.toml",
                EntryType::Regular,
                MANIFEST.as_bytes(),
                None,
            ),
            ("evil-hard", EntryType::Link, b"", Some("soyo-plugin.toml")),
        ]);
        assert_eq!(
            inspect_archive_bytes(&hardlink_tar, ArchiveFormat::TarGz)
                .unwrap_err()
                .code,
            "invalid_plugin"
        );
    }

    #[tokio::test]
    async fn https_fetch_requires_verification_and_blocks_private_addresses() {
        let empty = ArchiveVerification::default();
        assert_eq!(
            fetch_https_archive("https://example.invalid/plugin.tar.gz", &empty)
                .await
                .unwrap_err()
                .code,
            "plugin_verification_required"
        );

        let sha = ArchiveVerification {
            sha256: Some("a".repeat(64)),
            ed25519_pubkey: None,
            ed25519_signature: None,
        };
        assert_eq!(
            fetch_https_archive("http://example.invalid/plugin.tar.gz", &sha)
                .await
                .unwrap_err()
                .code,
            "plugin_source_unsupported"
        );
        assert_eq!(
            fetch_https_archive("https://127.0.0.1/plugin.tar.gz", &sha)
                .await
                .unwrap_err()
                .code,
            "plugin_fetch_failed"
        );
        assert_eq!(
            fetch_https_archive("https://169.254.169.254/plugin.tar.gz", &sha)
                .await
                .unwrap_err()
                .code,
            "plugin_fetch_failed"
        );
    }
}
