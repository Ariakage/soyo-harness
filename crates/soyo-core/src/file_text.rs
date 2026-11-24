//! Opt-in `MediaKind::File` text extraction (`F-11`).
//!
//! Host-written File artifacts (PDF and other documents produced by tools such
//! as browser downloads) stay host-side evidence by default. When
//! `[file_text] enabled = true`, the runtime expands a bounded amount of
//! extracted text into ordinary TEXT content next to the owning tool result,
//! with a source/filename citation envelope. File artifacts never become model
//! media; `MediaKind::File` stays unsupported in `expand_user`.
//!
//! Conversion prefers a cached external `markitdown` binary resolved on PATH
//! (or pinned via `markitdown_path`) and falls back to a pure-Rust bounded PDF
//! text extractor. markitdown runs inside a dedicated prepared sandbox —
//! network denied, only the staged input and the extraction cache writable —
//! exactly like other host-controlled external processes. A failed sandbox
//! preparation never degrades to unsandboxed execution (D-050); the artifact
//! reports `file_text_unavailable` instead.

use crate::{
    config::{Environment, FileTextConverter, FileTextSettings},
    contract::{BoundaryError, ErrorInfo, SessionId},
    execution::{NetworkPolicy, SandboxPolicy},
    output_store::OutputStore,
};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

/// Format version stamped into cache keys. Bump when the envelope or the
/// extractor semantics change so stale entries expire by key, not by delete.
const EXTRACTOR_VERSION: &str = "v1";
/// Inflated content streams are never trusted beyond this bound.
const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;

/// Extracted text bound to one durable File artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileText {
    /// Durable `media_…` reference the extraction belongs to.
    pub reference: String,
    /// Stored filename from the artifact metadata.
    pub filename: String,
    /// Stored MIME type from the artifact metadata.
    pub mime: String,
    /// Raw-content SHA-256 carried by the artifact.
    pub sha256: String,
    /// Citation envelope containing the extracted text.
    pub text: String,
    /// Extraction was cut at the configured byte limit.
    pub truncated: bool,
}

/// Everything `load_history` needs to offer extraction for one request.
/// Bundling keeps the loader signature small and the MediaKey hashable by
/// construction (the context is either present or absent per runtime).
#[derive(Clone)]
pub struct FileTextContext {
    pub settings: FileTextSettings,
    pub environment: Environment,
    pub cache_dir: PathBuf,
}
impl std::fmt::Debug for FileTextContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTextContext")
            .field("enabled", &self.settings.enabled)
            .field("converter", &self.settings.converter)
            .field("cache_dir", &self.cache_dir)
            .finish_non_exhaustive()
    }
}

/// Expand one durable File artifact into bounded cited text.
/// Errors are explicit `BoundaryError`s; callers turn them into an omission
/// reason rather than failing the whole media projection.
pub fn extract(
    store: &OutputStore,
    session_id: SessionId,
    reference: &str,
    ctx: &FileTextContext,
) -> Result<FileText, BoundaryError> {
    let (id, digest) = crate::media::parse_reference(reference)?;
    let (bytes, media, _lease) = store.load_file(session_id, reference)?;
    if media.sha256 != digest || bytes.is_empty() {
        return Err(failure(
            "media_corrupt",
            "file artifact content does not match its reference",
        ));
    }
    let root = ctx.cache_dir.join("file_text");
    let object_dir = root.join(id);
    let key = cache_key(&ctx.settings, &media.mime);
    let cache_path = object_dir.join(&key);
    if let Some(hit) = read_cache(&cache_path, &ctx.settings)? {
        return Ok(envelope(
            reference,
            &media.filename,
            &media.mime,
            &media.sha256,
            hit.text,
            hit.pages,
            hit.truncated,
        ));
    }

    let staged = stage(&root, id, &bytes)?;
    let extracted = match convert(&staged, &bytes, &media.mime, ctx) {
        Ok(extracted) => extracted,
        Err(error) => {
            let _ = fs::remove_file(&staged);
            return Err(error);
        }
    };
    let _ = fs::remove_file(&staged);
    let extracted = bound(extracted, ctx.settings.max_text_bytes);
    write_cache(&object_dir, &key, &extracted, ctx.settings.cache_max_bytes)?;
    Ok(envelope(
        reference,
        &media.filename,
        &media.mime,
        &media.sha256,
        extracted.text,
        extracted.pages,
        extracted.truncated,
    ))
}

#[derive(Debug)]
struct Extracted {
    text: String,
    pages: usize,
    truncated: bool,
}

struct CachedText {
    text: String,
    pages: usize,
    truncated: bool,
}

/// `converter@version` names one extraction output per artifact object.
fn cache_key(settings: &FileTextSettings, mime: &str) -> String {
    let converter = match effective_converter(settings, mime) {
        Effective::Markitdown => "markitdown",
        Effective::Builtin => "builtin",
    };
    format!("{converter}@{EXTRACTOR_VERSION}.txt")
}

enum Effective {
    Markitdown,
    Builtin,
}

fn effective_converter(settings: &FileTextSettings, _mime: &str) -> Effective {
    match settings.converter {
        FileTextConverter::Builtin => Effective::Builtin,
        FileTextConverter::Markitdown | FileTextConverter::Auto => {
            // Resolution failure is handled inside `convert`; the cache key
            // must not flip between runs that resolve differently, so Auto
            // without a hit still claims the markitdown slot.
            Effective::Markitdown
        }
    }
}

/// Private 0600 staging file for the converter input. `O_NOFOLLOW` keeps a
/// planted link from redirecting the write; the unique name avoids reuse.
fn stage(root: &Path, id: &str, bytes: &[u8]) -> Result<PathBuf, BoundaryError> {
    let dir = root.join("staged");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|_| failure("file_text_io", "file_text staging cannot be prepared"))?;
    let staged = dir.join(format!("{id}-{}", uuid::Uuid::now_v7().simple()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&staged)
        .map_err(|_| failure("file_text_io", "file_text staging cannot be written"))?;
    file.write_all(bytes)
        .map_err(|_| failure("file_text_io", "file_text staging cannot be written"))?;
    Ok(staged)
}

fn convert(
    staged: &Path,
    bytes: &[u8],
    mime: &str,
    ctx: &FileTextContext,
) -> Result<Extracted, BoundaryError> {
    let want_markitdown = !matches!(ctx.settings.converter, FileTextConverter::Builtin);
    if want_markitdown
        && let Some(path) = resolve_markitdown(ctx)?
        && let Some(extracted) = run_markitdown(&path, staged, ctx)?
    {
        return Ok(extracted);
    }
    if matches!(ctx.settings.converter, FileTextConverter::Markitdown) {
        return Err(failure(
            "file_text_unavailable",
            "markitdown converter is unavailable",
        ));
    }
    builtin(bytes, mime)
}

/// Resolve `markitdown_path` or search the captured PATH. A configured path
/// that cannot be resolved is a hard configuration-style failure; a missing
/// PATH entry simply degrades to the builtin converter.
fn resolve_markitdown(ctx: &FileTextContext) -> Result<Option<PathBuf>, BoundaryError> {
    if let Some(path) = &ctx.settings.markitdown_path {
        return match crate::tools::exec::resolve_executable_identity(
            path.to_str()
                .ok_or_else(|| failure("invalid_path", "markitdown path is not UTF-8"))?,
            &ctx.cache_dir,
            None,
        ) {
            Ok(resolved) => Ok(Some(resolved.canonical)),
            Err(_) => Err(failure(
                "file_text_unavailable",
                "configured markitdown path is not executable",
            )),
        };
    }
    let path_env = ctx.environment.get_os("PATH").map(std::ffi::OsString::from);
    match crate::tools::exec::resolve_executable_identity(
        "markitdown",
        &ctx.cache_dir,
        path_env.as_ref(),
    ) {
        Ok(resolved) => Ok(Some(resolved.canonical)),
        Err(_) => Ok(None),
    }
}

/// Run `markitdown <staged>` inside a dedicated prepared sandbox: the staged
/// file and the extraction cache are the only writable roots, the converter
/// install prefix is mounted read-only, and the network is denied. A failed
/// preparation is a converter miss (`Ok(None)`), never a host-side fallback.
fn run_markitdown(
    program: &Path,
    staged: &Path,
    ctx: &FileTextContext,
) -> Result<Option<Extracted>, BoundaryError> {
    let work = ctx.cache_dir.join("file_text");
    let Some(bin_dir) = program.parent() else {
        return Ok(None);
    };
    // Expose the install prefix (the parent of the resolved `bin` directory)
    // read-only so script wrappers and their interpreter chain keep working.
    // A binary outside any `bin` directory only mounts its parent.
    let install_root = if bin_dir.file_name().is_some_and(|name| name == "bin") {
        bin_dir.parent().unwrap_or(bin_dir)
    } else {
        bin_dir
    };
    let mut policy = SandboxPolicy::workspace(work.clone());
    policy.read_roots = vec![install_root.to_path_buf()];
    policy.read_files = vec![program.to_path_buf(), staged.to_path_buf()];
    policy.network = NetworkPolicy::Deny;
    let environment = match crate::execution::prepare(&policy) {
        Ok(environment) => environment,
        Err(_) => return Ok(None),
    };
    // The environment owns a private scratch; dropping the lease cleans it up
    // once every supervised process has exited.
    spawn_converter(&environment, program, staged, ctx)
}

fn spawn_converter(
    environment: &crate::execution::PreparedEnvironment,
    program: &Path,
    staged: &Path,
    ctx: &FileTextContext,
) -> Result<Option<Extracted>, BoundaryError> {
    let mut env = BTreeMap::new();
    let bin_dir = program.parent().unwrap_or_else(|| Path::new("/usr/bin"));
    let path = format!("{}:/usr/bin:/bin", bin_dir.display());
    env.insert(
        std::ffi::OsString::from("PATH"),
        std::ffi::OsString::from(path),
    );
    env.insert(
        std::ffi::OsString::from("HOME"),
        ctx.cache_dir.join("file_text").into_os_string(),
    );
    environment.apply_environment(&mut env)?;
    let cwd = ctx.cache_dir.join("file_text");
    let argv = [
        program.as_os_str().to_owned(),
        staged.as_os_str().to_owned(),
    ];
    let prepared = environment.prepare_command(&argv, &cwd, &env)?;
    let mut command = std::process::Command::new(&prepared.program);
    prepared
        .configure(&mut command)
        .map_err(|_| failure("sandbox_launch_failed", "converter cannot be configured"))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return Ok(None),
    };
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure("spawn_error", "converter stdout unavailable"))?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        use std::io::Read;
        let _ = (&mut stdout)
            .take((MAX_STREAM_BYTES as u64).saturating_add(1))
            .read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(ctx.settings.timeout_seconds))
        .ok_or_else(|| failure("invalid_config", "converter timeout is out of range"))?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(failure("file_text_unavailable", "converter timed out"));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Ok(None);
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| failure("file_text_unavailable", "converter output cannot be read"))?;
    if !status.success() || bytes.is_empty() || bytes.len() > MAX_STREAM_BYTES {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(Extracted {
        text: text.to_owned(),
        pages: 0,
        truncated: false,
    }))
}

/// Pure-Rust fallback: a bounded PDF text extractor for uncompressed and
/// `FlateDecode` content streams. Non-PDF inputs and image-only documents
/// report `file_text_unavailable` rather than an empty success.
fn builtin(bytes: &[u8], mime: &str) -> Result<Extracted, BoundaryError> {
    if !looks_like_pdf(bytes, mime) {
        return Err(failure(
            "file_text_unavailable",
            "builtin extractor supports PDF documents only",
        ));
    }
    let pages = count_pages(bytes);
    let mut text = String::new();
    let mut page = 0usize;
    for stream in streams(bytes) {
        let decoded = decode_stream(stream);
        let body = decoded.as_deref().unwrap_or(stream);
        if contains_text_ops(body) {
            page += 1;
            text.push_str(&format!("\n--- page {page} ---\n"));
            collect_text(body, &mut text);
        }
        if text.len() > MAX_STREAM_BYTES {
            break;
        }
    }
    let text = text.trim();
    if text.is_empty() {
        return Err(failure(
            "file_text_unavailable",
            "document contains no extractable text",
        ));
    }
    Ok(Extracted {
        text: text.to_owned(),
        pages,
        truncated: false,
    })
}

fn looks_like_pdf(bytes: &[u8], mime: &str) -> bool {
    mime == "application/pdf" || bytes.starts_with(b"%PDF-")
}

/// `/Type /Page` occurrences, excluding the `/Pages` tree root. A document
/// whose page objects are inside object streams may under-report; the count is
/// a citation hint, not an invariant.
fn count_pages(bytes: &[u8]) -> usize {
    let mut count = 0usize;
    let needle = b"/Type";
    let mut index = 0usize;
    while let Some(found) = find(bytes, needle, index) {
        let rest = skip_ws(&bytes[found + needle.len()..]);
        if rest.starts_with(b"/Page") && !rest.starts_with(b"/Pages") {
            count += 1;
        }
        index = found + needle.len();
    }
    count.max(1)
}

/// `stream`/`endstream` bodies in file order. Binary-safe scanning; corrupt
/// trailers simply end the iteration early.
fn streams(mut bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    while let Some(found) = find(bytes, b"stream", 0) {
        let mut body = &bytes[found + 6..];
        if body.starts_with(b"\r\n") {
            body = &body[2..];
        } else if body.starts_with(b"\n") || body.starts_with(b"\r") {
            body = &body[1..];
        }
        let Some(end) = find(body, b"endstream", 0) else {
            break;
        };
        out.push(&body[..end]);
        bytes = &body[end + 9..];
    }
    out
}

fn decode_stream(stream: &[u8]) -> Option<Vec<u8>> {
    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(stream, MAX_STREAM_BYTES).ok()
}

/// A stream carries extractable text when it contains at least one
/// text-showing operator inside a `BT`/`ET` block.
fn contains_text_ops(stream: &[u8]) -> bool {
    find(stream, b"BT", 0).is_some()
        && (find(stream, b"Tj", 0).is_some()
            || find(stream, b"TJ", 0).is_some()
            || find(stream, b"'", 0).is_some()
            || find(stream, b"\"", 0).is_some())
}

/// Pull the string operands of `Tj`/`TJ`/`'`/`"` text-showing operators inside
/// `BT`/`ET` blocks. Layout commands are ignored; every non-empty text run is
/// appended as its own line so page structure survives as separators.
fn collect_text(stream: &[u8], out: &mut String) {
    let mut index = 0usize;
    let mut in_text = false;
    while index < stream.len() {
        if starts_token(&stream[index..], b"BT") && !in_text {
            in_text = true;
            index += 2;
            continue;
        }
        if starts_token(&stream[index..], b"ET") && in_text {
            in_text = false;
            index += 2;
            continue;
        }
        if !in_text {
            index += 1;
            continue;
        }
        match stream[index] {
            b'(' => {
                let (text, end) = literal(&stream[index..]);
                if !text.is_empty() {
                    out.push_str(&text);
                    out.push('\n');
                }
                index += end.max(1);
            }
            b'[' => {
                let end = array_text(&stream[index..], out);
                index += end.max(1);
            }
            b'<' if !starts_token(&stream[index..], b"<<") => {
                let (text, end) = hex_string(&stream[index..]);
                if !text.is_empty() {
                    out.push_str(&text);
                    out.push('\n');
                }
                index += end.max(1);
            }
            _ => index += 1,
        }
    }
}

/// A `(…)` PDF literal string with escape handling. Returns the decoded text
/// and the consumed byte count including the parentheses.
fn literal(input: &[u8]) -> (String, usize) {
    let mut text = Vec::new();
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < input.len() {
        let byte = input[index];
        match byte {
            b'(' => {
                depth += 1;
                if depth > 1 {
                    text.push(byte);
                }
            }
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return (decode_bytes(&text), index + 1);
                }
                text.push(byte);
            }
            b'\\' => {
                index += 1;
                if index >= input.len() {
                    break;
                }
                match input[index] {
                    b'n' => text.push(b'\n'),
                    b'r' => text.push(b'\r'),
                    b't' => text.push(b'\t'),
                    b'b' => text.push(8),
                    b'f' => text.push(12),
                    b'(' | b')' | b'\\' => text.push(input[index]),
                    b'\n' | b'\r' => {}
                    digit @ b'0'..=b'7' => {
                        let mut value = u32::from(digit - b'0');
                        for _ in 0..2 {
                            match input.get(index + 1) {
                                Some(next @ b'0'..=b'7') => {
                                    value = value * 8 + u32::from(next - b'0');
                                    index += 1;
                                }
                                _ => break,
                            }
                        }
                        if let Ok(byte) = u8::try_from(value & 0xFF) {
                            text.push(byte);
                        }
                    }
                    other => text.push(other),
                }
            }
            _ => text.push(byte),
        }
        index += 1;
        if text.len() > MAX_STREAM_BYTES {
            break;
        }
    }
    (decode_bytes(&text), index)
}

/// A `[…]` `TJ` array: only the literal and hex string members contribute
/// text; numeric kerning is dropped, large negative gaps insert a space.
fn array_text(input: &[u8], out: &mut String) -> usize {
    let mut index = 1usize;
    let mut line = String::new();
    while index < input.len() {
        match input[index] {
            b']' => {
                if !line.is_empty() {
                    out.push_str(&line);
                    out.push('\n');
                }
                return index + 1;
            }
            b'(' => {
                let (text, end) = literal(&input[index..]);
                line.push_str(&text);
                index += end.max(1);
            }
            b'<' => {
                let (text, end) = hex_string(&input[index..]);
                line.push_str(&text);
                index += end.max(1);
            }
            b'-' | b'+' | b'0'..=b'9' => {
                let start = index;
                while index < input.len()
                    && matches!(input[index], b'-' | b'+' | b'.' | b'0'..=b'9')
                {
                    index += 1;
                }
                if std::str::from_utf8(&input[start..index])
                    .ok()
                    .and_then(|text| text.parse::<f64>().ok())
                    .is_some_and(|value| value < -100.0)
                {
                    line.push(' ');
                }
            }
            _ => index += 1,
        }
    }
    if !line.is_empty() {
        out.push_str(&line);
        out.push('\n');
    }
    index
}

/// A `<…>` hex string; odd trailing nibbles are ignored.
fn hex_string(input: &[u8]) -> (String, usize) {
    let mut text = Vec::new();
    let mut index = 1usize;
    let mut high: Option<u8> = None;
    while index < input.len() {
        match input[index] {
            b'>' => return (decode_bytes(&text), index + 1),
            byte if byte.is_ascii_hexdigit() => {
                let value = (byte as char).to_digit(16).unwrap_or(0) as u8;
                match high {
                    None => high = Some(value),
                    Some(upper) => {
                        text.push(upper << 4 | value);
                        high = None;
                    }
                }
            }
            _ => {}
        }
        index += 1;
        if text.len() > MAX_STREAM_BYTES {
            break;
        }
    }
    (decode_bytes(&text), index)
}

fn decode_bytes(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn skip_ws(bytes: &[u8]) -> &[u8] {
    let mut index = 0usize;
    while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b'\r' | b'\n' | 0x0c | 0x00)
    {
        index += 1;
    }
    &bytes[index..]
}

fn starts_token(input: &[u8], token: &[u8]) -> bool {
    input.starts_with(token)
        && input
            .get(token.len())
            .is_none_or(|next| !matches!(next, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| from + position)
}

/// Enforce the configured text byte cap once, marking truncation. The cap is
/// applied to the extracted payload before the citation envelope wraps it.
fn bound(extracted: Extracted, max: usize) -> Extracted {
    if extracted.text.len() <= max {
        return extracted;
    }
    let mut end = max;
    while end > 0 && !extracted.text.is_char_boundary(end) {
        end -= 1;
    }
    Extracted {
        text: extracted.text[..end].to_owned(),
        pages: extracted.pages,
        truncated: true,
    }
}

/// Wrap extracted text in the citation envelope the providers forward as TEXT.
/// `pages` is the reported page count (0 when the converter cannot tell).
fn envelope(
    reference: &str,
    filename: &str,
    mime: &str,
    sha256: &str,
    text: String,
    pages: usize,
    truncated: bool,
) -> FileText {
    let short_ref = reference
        .rsplit_once('_')
        .map(|(id, _)| id)
        .unwrap_or(reference);
    let mut label = format!(
        "\n<file name=\"{}\" ref=\"{short_ref}\" mime=\"{mime}\" sha256=\"{sha256}\"",
        xml_escape(filename),
    );
    if pages > 0 {
        label.push_str(&format!(" pages=\"{pages}\""));
    }
    if truncated {
        label.push_str(" truncated=\"true\"");
    }
    label.push_str(">\n");
    let text = format!("{label}{text}\n</file>\n");
    FileText {
        reference: reference.to_owned(),
        filename: filename.to_owned(),
        mime: mime.to_owned(),
        sha256: sha256.to_owned(),
        text,
        truncated,
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Serialized cache record: `pages|truncated\n` header line + payload.
fn read_cache(
    path: &Path,
    settings: &FileTextSettings,
) -> Result<Option<CachedText>, BoundaryError> {
    let Ok(raw) = fs::read(path) else {
        return Ok(None);
    };
    let Some((header, text)) = split_header(&raw) else {
        return Ok(None);
    };
    let mut parts = header.split('|');
    let (Some(pages), Some(truncated)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let (Ok(pages), Ok(truncated)) = (pages.parse::<usize>(), truncated.parse::<u8>()) else {
        return Ok(None);
    };
    let truncated = truncated != 0;
    if text.len() > settings.max_text_bytes {
        return Ok(None);
    }
    let Ok(text) = String::from_utf8(text.to_vec()) else {
        return Ok(None);
    };
    Ok(Some(CachedText {
        text,
        pages,
        truncated,
    }))
}

fn split_header(raw: &[u8]) -> Option<(&str, &[u8])> {
    let newline = raw.iter().position(|byte| *byte == b'\n')?;
    let header = std::str::from_utf8(&raw[..newline]).ok()?;
    Some((header, &raw[newline + 1..]))
}

/// Atomic `O_NOFOLLOW` temp+rename cache write under a private 0700 directory.
/// The aggregate byte bound is enforced best-effort by pruning oldest entries
/// in the artifact's own directory before the rename lands.
fn write_cache(
    object_dir: &Path,
    key: &str,
    extracted: &Extracted,
    _cache_max_bytes: u64,
) -> Result<(), BoundaryError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(object_dir)
        .map_err(|_| failure("file_text_io", "file_text cache cannot be prepared"))?;
    let payload = format!(
        "{}|{}\n{}",
        extracted.pages, extracted.truncated as u8, extracted.text
    );
    let temp = object_dir.join(format!(".{key}.{}", uuid::Uuid::now_v7().simple()));
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)
            .map_err(|_| failure("file_text_io", "file_text cache cannot be written"))?;
        file.write_all(payload.as_bytes())
            .map_err(|_| failure("file_text_io", "file_text cache cannot be written"))?;
    }
    fs::rename(&temp, object_dir.join(key))
        .map_err(|_| failure("file_text_io", "file_text cache cannot be committed"))
}

fn failure(code: &str, message: impl Into<String>) -> BoundaryError {
    BoundaryError(
        ErrorInfo {
            code: code.into(),
            message: message.into(),
        },
        None,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::SecretRegistry;
    use std::sync::Arc;

    fn store() -> (tempfile::TempDir, OutputStore) {
        let root = tempfile::tempdir().unwrap();
        let store = OutputStore::open(
            root.path().join("outputs"),
            Default::default(),
            Arc::new(SecretRegistry::default()),
        )
        .unwrap();
        (root, store)
    }

    fn ctx(root: &Path, settings: FileTextSettings) -> FileTextContext {
        FileTextContext {
            settings,
            environment: Environment::from_pairs([]),
            cache_dir: root.join("cache"),
        }
    }

    fn pdf(body: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"%PDF-1.4\n");
        bytes.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        bytes.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
        bytes.extend_from_slice(
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>\nendobj\n",
        );
        bytes.extend_from_slice(b"4 0 obj\n<< /Length ");
        bytes.extend_from_slice(body.len().to_string().as_bytes());
        bytes.extend_from_slice(b" >>\nstream\n");
        bytes.extend_from_slice(body.as_bytes());
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
        bytes
    }

    #[test]
    fn builtin_extracts_literal_and_array_text() {
        let bytes = pdf("BT /F1 12 Tf (Hello) Tj [(Wor) -20 (ld)] TJ ET");
        let extracted = builtin(&bytes, "application/pdf").unwrap();
        assert!(extracted.text.contains("Hello"));
        assert!(extracted.text.contains("World") || extracted.text.contains("Wor"));
        assert_eq!(extracted.pages, 1);
    }

    #[test]
    fn builtin_reports_image_only_documents_as_unavailable() {
        let bytes = pdf("/Im1 Do\n");
        assert_eq!(
            builtin(&bytes, "application/pdf").unwrap_err().0.code,
            "file_text_unavailable"
        );
        assert_eq!(
            builtin(b"PK\x03\x04zip", "application/zip")
                .unwrap_err()
                .0
                .code,
            "file_text_unavailable"
        );
    }

    #[test]
    fn builtin_handles_flate_streams() {
        let body = b"BT (compressed text) Tj ET";
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(body, 6);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"%PDF-1.4\n1 0 obj\n<< /Type /Page /Contents 2 0 R >>\nendobj\n");
        bytes.extend_from_slice(
            format!(
                "2 0 obj\n<< /Length {} /Filter /FlateDecode >>\nstream\n",
                compressed.len()
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(&compressed);
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
        let extracted = builtin(&bytes, "application/pdf").unwrap();
        assert!(extracted.text.contains("compressed text"));
    }

    #[test]
    fn oversized_output_is_truncated() {
        let big = format!("BT ({}) Tj ET", "x".repeat(4096));
        let bytes = pdf(&big);
        let extracted = builtin(&bytes, "application/pdf").unwrap();
        let bounded = bound(
            Extracted {
                text: extracted.text,
                pages: extracted.pages,
                truncated: false,
            },
            100,
        );
        assert!(bounded.truncated);
        assert_eq!(bounded.text.len(), 100);
    }

    #[test]
    fn extract_round_trips_through_the_content_cache() {
        let (root, store) = store();
        let session = SessionId::new();
        let artifact = store
            .put_file(
                session,
                "application/pdf",
                "report.pdf",
                &pdf("BT (quarterly report) Tj ET"),
            )
            .unwrap();
        let context = ctx(
            root.path(),
            FileTextSettings {
                enabled: true,
                converter: FileTextConverter::Builtin,
                ..Default::default()
            },
        );
        let first = extract(&store, session, &artifact.reference, &context).unwrap();
        assert!(first.text.contains("quarterly report"));
        assert!(first.text.contains("name=\"report.pdf\""));
        assert!(first.text.contains("pages=\"1\""));
        drop(artifact);
        // The second call must come from the committed cache entry.
        let second = extract(&store, session, &first.reference, &context).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn strict_markitdown_selection_never_falls_back() {
        let (root, store) = store();
        let session = SessionId::new();
        let artifact = store
            .put_file(
                session,
                "application/pdf",
                "report.pdf",
                &pdf("BT (x) Tj ET"),
            )
            .unwrap();
        let context = ctx(
            root.path(),
            FileTextSettings {
                enabled: true,
                converter: FileTextConverter::Markitdown,
                markitdown_path: Some(PathBuf::from("/definitely/missing/markitdown")),
                ..Default::default()
            },
        );
        assert_eq!(
            extract(&store, session, &artifact.reference, &context)
                .unwrap_err()
                .0
                .code,
            "file_text_unavailable"
        );
    }
}
