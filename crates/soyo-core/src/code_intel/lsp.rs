//! Sandboxed LSP 3.17 JSON-RPC 2.0 client over stdio (`F-17`).
//!
//! Launches configured language servers inside `PreparedEnvironment`:
//! - `workspace` is readable (`read_roots`) and never writable (`write_roots` excludes it,
//!   `write_denied_paths` includes it).
//! - Only external `<cache_dir>/lsp/<sha256(workspace)>/<server_id>/` is writable.
//! - `CARGO_TARGET_DIR`, `XDG_CACHE_HOME`, `TMPDIR`, and `HOME` are redirected inside that external
//!   cache directory while read-only `RUSTUP_HOME` / `CARGO_HOME` and configured `read_roots`
//!   remain accessible for toolchains such as `rust-analyzer`.
//! - All LSP paths/URIs returned to the harness pass workspace containment, symlink refusal,
//!   `.git` control-path rejection, and `ReadTools::check_private_path` even in `Bypass` mode.

use super::index::{
    ensure_private_dir_no_symlink, is_allowed_checked_path, read_regular_no_follow, sha256_hex,
    validate_external_cache_dir, validate_workspace_file, workspace_hash,
};
use super::symbols::{IndexedSymbol, SymbolKind, language_id};
use crate::{
    boundary::Cancellation,
    config::{Environment, LspServerSettings, LspSettings},
    contract::{BoundaryError, ErrorInfo},
    execution::{BackendKind, NetworkPolicy, PreparedEnvironment, SandboxPolicy, prepare},
    tools::exec::{
        ExecConfig, ExecPlan, ManagedProcess, environment_digest, resolve_executable_identity,
    },
    tools::read::ReadTools,
};
use diffy::create_patch;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex as AsyncMutex, Notify, oneshot},
    task::JoinHandle,
};

const MAX_LSP_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_LSP_STDERR_BYTES: usize = 64 * 1024;
const MAX_LSP_LOCATIONS: usize = 100;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceLocation {
    pub path: PathBuf,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagnosticEntry {
    pub path: PathBuf,
    pub severity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub message: String,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditSuggestionFile {
    pub path: PathBuf,
    pub base_sha256: String,
    pub unified_diff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditSuggestion {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub files: Vec<EditSuggestionFile>,
}

#[derive(Debug, Clone)]
pub struct LspClientConfig {
    pub workspace: PathBuf,
    pub cache_dir: PathBuf,
    pub sandbox_backend: BackendKind,
    pub settings: LspSettings,
    pub protected_paths: Vec<PathBuf>,
    pub host_env: Environment,
}

/// Percent-encode a local path as an RFC 8089 `file://` URI.
pub(crate) fn path_to_file_uri(path: &Path) -> Result<String, BoundaryError> {
    let raw = path.to_str().ok_or_else(|| {
        failure(
            "invalid_path",
            "non-UTF8 path cannot be encoded as file URI",
        )
    })?;
    let mut encoded = String::from("file://");
    if !raw.starts_with('/') {
        encoded.push('/');
    }
    for byte in raw.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' | b':' => {
                encoded.push(*byte as char)
            }
            other => {
                use std::fmt::Write as _;
                let _ = write!(&mut encoded, "%{other:02X}");
            }
        }
    }
    Ok(encoded)
}

/// Decode a `file://` URI into an OS path without performing filesystem resolution.
pub(crate) fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path_part = if let Some(after_local) = rest.strip_prefix("localhost/") {
        format!("/{after_local}")
    } else if rest.starts_with('/') {
        rest.to_owned()
    } else {
        return None;
    };

    let bytes = path_part.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_nibble(bytes[i + 1]);
            let lo = hex_nibble(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok().map(PathBuf::from)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Convert 1-based `(line, col_utf16)` used by Soyo tools into 0-based LSP Position, clamped to file lines.
fn to_lsp_position(text: &str, line_1: u32, col_1: u32) -> (u32, u32) {
    let line_0 = line_1.saturating_sub(1);
    let target_line = usize::try_from(line_0).unwrap_or(0);
    let col_target = usize::try_from(col_1.saturating_sub(1)).unwrap_or(0);
    if let Some(line_str) = text.lines().nth(target_line) {
        let max_utf16 = line_str.encode_utf16().count();
        return (
            line_0,
            u32::try_from(col_target.min(max_utf16)).unwrap_or(0),
        );
    }
    (line_0, u32::try_from(col_target).unwrap_or(0))
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (idx, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

/// Convert 0-based LSP `(line, character_utf16)` into byte offset inside `text`.
fn lsp_pos_to_byte_offset(
    text: &str,
    starts: &[usize],
    line: u32,
    character: u32,
) -> Option<usize> {
    let line_idx = usize::try_from(line).ok()?;
    let start = *starts.get(line_idx)?;
    let end_with_nl = starts.get(line_idx + 1).copied().unwrap_or(text.len());
    let mut line_slice = &text[start..end_with_nl];
    if let Some(stripped) = line_slice.strip_suffix('\n') {
        line_slice = stripped;
    }
    if let Some(stripped) = line_slice.strip_suffix('\r') {
        line_slice = stripped;
    }
    let target_units = usize::try_from(character).ok()?;
    let mut seen_units = 0usize;
    let mut byte_in_line = 0usize;
    for ch in line_slice.chars() {
        if seen_units >= target_units {
            break;
        }
        seen_units += ch.len_utf16();
        byte_in_line += ch.len_utf8();
    }
    Some(start + byte_in_line)
}

#[derive(Debug, Clone)]
struct OpenDocState {
    version: i32,
    sha256: String,
    uri: String,
}

#[derive(Debug, Default)]
struct ServerSharedState {
    pending: HashMap<i64, oneshot::Sender<Result<Value, BoundaryError>>>,
    diagnostics: HashMap<PathBuf, Vec<DiagnosticEntry>>,
    progress_active: usize,
    diag_notify: Option<Arc<Notify>>,
    read_error: Option<String>,
}

struct LspServerSession {
    server_id: String,
    workspace: PathBuf,
    lsp_cache_dir: PathBuf,
    #[allow(dead_code)]
    environment: PreparedEnvironment,
    process: ManagedProcess,
    stdin: Arc<AsyncMutex<tokio::process::ChildStdin>>,
    reader_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
    stderr_buffer: Arc<StdMutex<Vec<u8>>>,
    next_id: AtomicI64,
    shared: Arc<StdMutex<ServerSharedState>>,
    diag_notify: Arc<Notify>,
    capabilities: Value,
    open_docs: BTreeMap<PathBuf, OpenDocState>,
    open_lru: VecDeque<PathBuf>,
    max_open_documents: usize,
    request_timeout: Duration,
    protected_paths: Vec<PathBuf>,
}

impl LspServerSession {
    async fn start(
        server_id: &str,
        server_cfg: &LspServerSettings,
        client_cfg: &LspClientConfig,
    ) -> Result<Self, BoundaryError> {
        let (canon_ws, norm_cache) =
            validate_external_cache_dir(&client_cfg.workspace, &client_cfg.cache_dir)?;
        let ws_hash = workspace_hash(&canon_ws);
        let lsp_root = norm_cache.join("lsp");
        let lsp_ws_dir = lsp_root.join(&ws_hash);
        let lsp_cache_dir = lsp_ws_dir.join(server_id);
        for dir in [&lsp_root, &lsp_ws_dir, &lsp_cache_dir] {
            ensure_private_dir_no_symlink(dir)?;
        }
        let target_dir = lsp_cache_dir.join("cargo-target");
        let xdg_cache = lsp_cache_dir.join("xdg-cache");
        let home_dir = lsp_cache_dir.join("home");
        let tmp_dir = lsp_cache_dir.join("tmp");
        for dir in [&target_dir, &xdg_cache, &home_dir, &tmp_dir] {
            ensure_private_dir_no_symlink(dir)?;
        }

        let Some(raw_program) = server_cfg.argv.first() else {
            return Err(failure("invalid_config", "LSP server argv cannot be empty"));
        };

        let host_path_os = server_cfg
            .env
            .get("PATH")
            .map(OsString::from)
            .or_else(|| client_cfg.host_env.get_os("PATH").map(|v| v.to_os_string()));
        let resolved = resolve_executable_identity(raw_program, &canon_ws, host_path_os.as_ref())?;

        // Build read_roots for Seatbelt/Bubblewrap without opening workspace for writes.
        let mut read_roots = vec![canon_ws.clone()];
        let mut read_files = Vec::new();
        for dir in [resolved.invoked.parent(), resolved.canonical.parent()]
            .into_iter()
            .flatten()
        {
            if let Ok(c) = fs::canonicalize(dir)
                && !read_roots.contains(&c)
            {
                read_roots.push(c);
            }
        }
        let mut toolchain_dirs: Vec<(&'static str, PathBuf)> = Vec::new();
        let host_home = client_cfg.host_env.get_os("HOME").map(PathBuf::from);
        for (key, default_sub) in [("RUSTUP_HOME", ".rustup"), ("CARGO_HOME", ".cargo")] {
            let candidate_path = server_cfg
                .env
                .get(key)
                .map(PathBuf::from)
                .or_else(|| client_cfg.host_env.get_os(key).map(PathBuf::from))
                .or_else(|| host_home.as_ref().map(|h| h.join(default_sub)));
            if let Some(val) = candidate_path
                && let Ok(c) = fs::canonicalize(val)
                && c.is_dir()
            {
                if !read_roots.contains(&c) {
                    read_roots.push(c.clone());
                }
                toolchain_dirs.push((key, c));
            }
        }
        for root in client_cfg
            .settings
            .read_roots
            .iter()
            .chain(server_cfg.read_roots.iter())
        {
            if let Ok(c) = fs::canonicalize(root)
                && !read_roots.contains(&c)
            {
                read_roots.push(c);
            }
        }
        // Allow helper scripts passed as argv[1..] read-only if they exist as regular files.
        for arg in &server_cfg.argv[1..] {
            let p = Path::new(arg);
            if p.is_absolute()
                && p.is_file()
                && let Ok(c) = fs::canonicalize(p)
                && !c.starts_with(&canon_ws)
                && !read_files.contains(&c)
            {
                read_files.push(c);
            }
        }
        #[cfg(target_os = "macos")]
        {
            let openssl_cnf = Path::new("/private/etc/ssl/openssl.cnf");
            if openssl_cnf.is_file() {
                read_files.push(openssl_cnf.to_path_buf());
            }
        }

        let mut policy = SandboxPolicy::workspace(canon_ws.clone());
        policy.backend = client_cfg.sandbox_backend;
        policy.read_roots = read_roots;
        policy.read_files = read_files;
        // CRITICAL: workspace MUST NOT be in write_roots; ONLY external lsp_cache_dir is writable.
        policy.write_roots = vec![lsp_cache_dir.clone()];
        policy.write_denied_paths = vec![canon_ws.clone()];
        policy.protected_paths = client_cfg.protected_paths.clone();
        policy.network = NetworkPolicy::Deny;
        policy.resources = client_cfg.settings.resources.clone();

        let environment = prepare(&policy)?;

        let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
        for key in ["PATH", "LANG", "LC_ALL"] {
            if let Some(val) = client_cfg.host_env.get_os(key) {
                env.insert(OsString::from(key), val.to_os_string());
            }
        }
        for (key, dir) in &toolchain_dirs {
            env.insert(OsString::from(key), dir.as_os_str().to_os_string());
        }
        for (k, v) in &server_cfg.env {
            env.insert(OsString::from(k), OsString::from(v));
        }
        environment.apply_environment(&mut env)?;
        // Force mutable compiler/server scratch dirs inside the external LSP cache root.
        env.entry(OsString::from("OPENSSL_CONF"))
            .or_insert_with(|| OsString::from("/dev/null"));
        env.insert(
            OsString::from("CARGO_TARGET_DIR"),
            target_dir.as_os_str().to_os_string(),
        );
        env.insert(
            OsString::from("XDG_CACHE_HOME"),
            xdg_cache.as_os_str().to_os_string(),
        );
        env.insert(OsString::from("HOME"), home_dir.as_os_str().to_os_string());
        env.insert(OsString::from("TMPDIR"), tmp_dir.as_os_str().to_os_string());
        env.insert(OsString::from("TEMP"), tmp_dir.as_os_str().to_os_string());
        env.insert(OsString::from("TMP"), tmp_dir.as_os_str().to_os_string());

        let mut argv = Vec::with_capacity(server_cfg.argv.len());
        argv.push(resolved.invoked.to_string_lossy().into_owned());
        for arg in &server_cfg.argv[1..] {
            argv.push(arg.clone());
        }

        let startup_timeout = Duration::from_secs(client_cfg.settings.startup_timeout_seconds);
        let request_timeout = Duration::from_secs(client_cfg.settings.request_timeout_seconds);
        let plan = ExecPlan {
            argv,
            cwd: canon_ws.clone(),
            timeout: startup_timeout,
            environment_digest: environment_digest(&env, Some(&environment)),
        };
        let cwd_dir = cap_std::fs::Dir::open_ambient_dir(&canon_ws, cap_std::ambient_authority())
            .map_err(|err| failure("invalid_path", err.to_string()))?;
        let exec_config = ExecConfig {
            default_timeout: request_timeout,
            max_timeout: startup_timeout.max(request_timeout),
            stdout_max: MAX_LSP_FRAME_BYTES,
            stderr_max: MAX_LSP_STDERR_BYTES,
            execution: Some(environment.clone()),
            environment: Environment::from_pairs([]),
            ..ExecConfig::default()
        };

        let mut process = ManagedProcess::spawn(&plan, cwd_dir, env, &exec_config, true)?;
        let stdin = process
            .take_stdin()
            .ok_or_else(|| failure("lsp_spawn_error", "LSP stdin pipe unavailable"))?;
        let stdout = process
            .take_stdout()
            .ok_or_else(|| failure("lsp_spawn_error", "LSP stdout pipe unavailable"))?;

        let stderr_buffer = Arc::new(StdMutex::new(Vec::<u8>::new()));
        let stderr_task = process.take_stderr().map(|stderr| {
            let buf = Arc::clone(&stderr_buffer);
            tokio::spawn(async move {
                let mut bounded = stderr.take(MAX_LSP_STDERR_BYTES as u64);
                let mut chunk = [0u8; 4096];
                loop {
                    match bounded.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            if let Ok(mut guard) = buf.lock() {
                                let keep = MAX_LSP_STDERR_BYTES.saturating_sub(guard.len());
                                guard.extend_from_slice(&chunk[..read.min(keep)]);
                            }
                        }
                    }
                }
            })
        });

        let diag_notify = Arc::new(Notify::new());
        let shared = Arc::new(StdMutex::new(ServerSharedState {
            diag_notify: Some(Arc::clone(&diag_notify)),
            ..ServerSharedState::default()
        }));
        let stdin_arc = Arc::new(AsyncMutex::new(stdin));

        let reader_shared = Arc::clone(&shared);
        let reader_stdin = Arc::clone(&stdin_arc);
        let reader_ws = canon_ws.clone();
        let reader_protected = client_cfg.protected_paths.clone();
        let max_diag = client_cfg.settings.max_diagnostics_per_file;
        let reader_task = tokio::spawn(async move {
            run_reader_loop(
                stdout,
                reader_stdin,
                reader_shared,
                reader_ws,
                reader_protected,
                max_diag,
            )
            .await;
        });

        let mut session = Self {
            server_id: server_id.to_owned(),
            workspace: canon_ws.clone(),
            lsp_cache_dir,
            environment,
            process,
            stdin: stdin_arc,
            reader_task: Some(reader_task),
            stderr_task,
            stderr_buffer,
            next_id: AtomicI64::new(1),
            shared,
            diag_notify,
            capabilities: Value::Null,
            open_docs: BTreeMap::new(),
            open_lru: VecDeque::new(),
            max_open_documents: client_cfg.settings.max_open_documents,
            request_timeout,
            protected_paths: client_cfg.protected_paths.clone(),
        };

        let init_result = session
            .initialize(server_cfg.initialization_options.clone(), startup_timeout)
            .await;
        if let Err(err) = init_result {
            let _ = session.shutdown().await;
            return Err(err);
        }
        Ok(session)
    }

    async fn initialize(
        &mut self,
        initialization_options: Option<Value>,
        startup_timeout: Duration,
    ) -> Result<(), BoundaryError> {
        let root_uri = path_to_file_uri(&self.workspace)?;
        let ws_name = self
            .workspace
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("workspace");
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "soyo-harness" },
            "rootUri": root_uri,
            "workspaceFolders": [{
                "uri": root_uri,
                "name": ws_name,
            }],
            "initializationOptions": initialization_options.unwrap_or(Value::Null),
            "capabilities": {
                "workspace": {
                    "configuration": true,
                    "workspaceFolders": true,
                    "applyEdit": false,
                    "symbol": { "dynamicRegistration": false },
                    "workspaceEdit": { "documentChanges": true }
                },
                "window": {
                    "workDoneProgress": true
                },
                "textDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "willSave": false,
                        "willSaveWaitUntil": false,
                        "didSave": true
                    },
                    "definition": { "linkSupport": true },
                    "references": {},
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "publishDiagnostics": { "relatedInformation": false },
                    "diagnostic": { "dynamicRegistration": false },
                    "codeAction": {
                        "codeActionLiteralSupport": {
                            "codeActionKind": {
                                "valueSet": ["quickfix", "refactor", "source"]
                            }
                        },
                        "resolveSupport": {
                            "properties": ["edit"]
                        }
                    },
                    "rename": { "prepareSupport": false }
                }
            }
        });

        let result = self
            .request_with_timeout("initialize", params, startup_timeout, &NeverCancel)
            .await
            .map_err(|err| {
                let tail = self.stderr_tail();
                match tail {
                    Some(t) => failure(
                        &err.0.code,
                        format!("{} (server stderr: {t})", err.0.message),
                    ),
                    None => err,
                }
            })?;
        self.capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
        self.notify("initialized", json!({})).await?;
        Ok(())
    }

    fn stderr_tail(&self) -> Option<String> {
        let guard = self.stderr_buffer.lock().ok()?;
        if guard.is_empty() {
            return None;
        }
        let slice = if guard.len() > 2048 {
            &guard[guard.len() - 2048..]
        } else {
            &guard[..]
        };
        let line: String = String::from_utf8_lossy(slice)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        (!line.is_empty()).then_some(line)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), BoundaryError> {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        write_lsp_message(&self.stdin, &payload).await
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: &dyn Cancellation,
    ) -> Result<Value, BoundaryError> {
        self.request_with_timeout(method, params, self.request_timeout, cancel)
            .await
    }

    async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout_dur: Duration,
        cancel: &dyn Cancellation,
    ) -> Result<Value, BoundaryError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self
                .shared
                .lock()
                .map_err(|_| failure("lsp_error", "LSP shared state lock poisoned"))?;
            if let Some(err) = &guard.read_error {
                return Err(failure("lsp_disconnected", err.clone()));
            }
            guard.pending.insert(id, tx);
        }

        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(err) = write_lsp_message(&self.stdin, &payload).await {
            if let Ok(mut guard) = self.shared.lock() {
                guard.pending.remove(&id);
            }
            return Err(err);
        }

        let res = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                let _ = self.notify("$/cancelRequest", json!({ "id": id })).await;
                Err(failure("cancelled", "LSP request cancelled"))
            }
            () = tokio::time::sleep(timeout_dur) => {
                let _ = self.notify("$/cancelRequest", json!({ "id": id })).await;
                Err(failure("lsp_timeout", format!("LSP request `{method}` timed out after {}s", timeout_dur.as_secs())))
            }
            joined = rx => {
                match joined {
                    Ok(inner) => inner,
                    Err(_) => Err(failure("lsp_disconnected", format!("LSP server `{}` closed channel during `{method}`", self.server_id))),
                }
            }
        };

        if res.is_err()
            && let Ok(mut guard) = self.shared.lock()
        {
            guard.pending.remove(&id);
        }
        res
    }

    async fn ensure_document_synced(
        &mut self,
        abs_or_rel: &Path,
        max_file_bytes: usize,
        read_tools: Option<&ReadTools>,
    ) -> Result<(PathBuf, String, i32, String, String), BoundaryError> {
        let (rel, abs) = validate_workspace_file(
            &self.workspace,
            abs_or_rel,
            &self.protected_paths,
            read_tools,
        )?;
        let (bytes, _, _) = read_regular_no_follow(&abs, max_file_bytes)?;
        let text = String::from_utf8(bytes).map_err(|_| {
            failure(
                "invalid_args",
                format!("{} is not valid UTF-8 text", rel.display()),
            )
        })?;
        let digest = sha256_hex(text.as_bytes());
        let lang = language_id(&rel).unwrap_or("plaintext").to_owned();
        let uri = path_to_file_uri(&abs)?;

        if let Some(doc) = self.open_docs.get_mut(&rel) {
            if doc.sha256 != digest {
                doc.version = doc.version.saturating_add(1);
                doc.sha256 = digest.clone();
                let new_ver = doc.version;
                let doc_uri = doc.uri.clone();
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": doc_uri, "version": new_ver },
                        "contentChanges": [{ "text": text }]
                    }),
                )
                .await?;
                self.notify(
                    "textDocument/didSave",
                    json!({
                        "textDocument": { "uri": uri },
                        "text": text
                    }),
                )
                .await?;
            }
            let version = self.open_docs.get(&rel).map(|d| d.version).unwrap_or(1);
            return Ok((rel, uri, version, digest, text));
        }

        // Enforce max_open_documents by closing oldest opened doc cleanly.
        while self.open_docs.len() >= self.max_open_documents {
            let Some(evict) = self.open_lru.pop_front() else {
                break;
            };
            if let Some(old) = self.open_docs.remove(&evict) {
                let _ = self
                    .notify(
                        "textDocument/didClose",
                        json!({ "textDocument": { "uri": old.uri } }),
                    )
                    .await;
            }
        }

        let version = 1;
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": lang,
                    "version": version,
                    "text": text
                }
            }),
        )
        .await?;
        self.open_docs.insert(
            rel.clone(),
            OpenDocState {
                version,
                sha256: digest.clone(),
                uri: uri.clone(),
            },
        );
        self.open_lru.push_back(rel.clone());
        Ok((rel, uri, version, digest, text))
    }

    async fn wait_for_diagnostics_if_needed(
        &self,
        rel: &Path,
        timeout: Duration,
        cancel: &dyn Cancellation,
    ) {
        let deadline = Instant::now() + timeout;
        loop {
            let has_diag_or_idle = {
                let Ok(guard) = self.shared.lock() else {
                    return;
                };
                guard.diagnostics.contains_key(rel)
            };
            if has_diag_or_idle {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(remaining) => return,
                () = self.diag_notify.notified() => {}
            }
        }
    }

    fn supports_pull_diagnostics(&self) -> bool {
        self.capabilities.get("diagnosticProvider").is_some()
            && !self.capabilities["diagnosticProvider"].is_null()
            && self.capabilities["diagnosticProvider"] != Value::Bool(false)
    }

    fn supports_code_action_resolve(&self) -> bool {
        self.capabilities
            .get("codeActionProvider")
            .and_then(|v| v.get("resolveProvider"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    fn parse_locations(&self, val: Value, read_tools: Option<&ReadTools>) -> Vec<SourceLocation> {
        let items: Vec<Value> = match val {
            Value::Null => Vec::new(),
            Value::Array(arr) => arr,
            Value::Object(_) => vec![val],
            _ => Vec::new(),
        };
        let mut out = Vec::new();
        for item in items {
            let Some((uri, range_val)) = extract_uri_and_range(&item) else {
                continue;
            };
            let Some(raw_path) = file_uri_to_path(&uri) else {
                continue;
            };
            let Ok((rel, _abs)) = validate_workspace_file(
                &self.workspace,
                &raw_path,
                &self.protected_paths,
                read_tools,
            ) else {
                continue;
            };
            if let Some((sl, sc, el, ec)) = parse_lsp_range(range_val) {
                out.push(SourceLocation {
                    path: rel,
                    start_line: sl,
                    start_col: sc,
                    end_line: el,
                    end_col: ec,
                    preview: None,
                });
                if out.len() >= MAX_LSP_LOCATIONS {
                    break;
                }
            }
        }
        out
    }

    async fn shutdown(&mut self) {
        let shutdown_fut = self.request_with_timeout(
            "shutdown",
            Value::Null,
            Duration::from_secs(2),
            &NeverCancel,
        );
        let _ = shutdown_fut.await;
        let _ = self.notify("exit", Value::Null).await;
        let _ = self.process.terminate().await;
        if let Some(handle) = self.reader_task.take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_task.take() {
            handle.abort();
        }
    }
}

struct NeverCancel;
impl Cancellation for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::pending())
    }
}

async fn write_lsp_message(
    stdin: &Arc<AsyncMutex<tokio::process::ChildStdin>>,
    payload: &Value,
) -> Result<(), BoundaryError> {
    let body =
        serde_json::to_vec(payload).map_err(|err| failure("lsp_encode_error", err.to_string()))?;
    if body.len() > MAX_LSP_FRAME_BYTES {
        return Err(failure(
            "lsp_encode_error",
            "LSP request exceeds frame limit",
        ));
    }
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut guard = stdin.lock().await;
    guard
        .write_all(header.as_bytes())
        .await
        .map_err(|err| failure("lsp_io_error", format!("LSP write failed: {err}")))?;
    guard
        .write_all(&body)
        .await
        .map_err(|err| failure("lsp_io_error", format!("LSP write failed: {err}")))?;
    guard
        .flush()
        .await
        .map_err(|err| failure("lsp_io_error", format!("LSP flush failed: {err}")))?;
    Ok(())
}

async fn read_lsp_frame<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<Vec<u8>>, String> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|err| format!("LSP header read failed: {err}"))?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, val)) = trimmed.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            let parsed = val
                .trim()
                .parse::<usize>()
                .map_err(|_| "invalid LSP Content-Length header".to_owned())?;
            if parsed > MAX_LSP_FRAME_BYTES {
                return Err("LSP message exceeds maximum frame size".to_owned());
            }
            content_length = Some(parsed);
        }
    }
    let len = content_length.ok_or_else(|| "missing LSP Content-Length header".to_owned())?;
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|err| format!("LSP body read failed: {err}"))?;
    Ok(Some(body))
}

async fn run_reader_loop(
    stdout: tokio::process::ChildStdout,
    stdin: Arc<AsyncMutex<tokio::process::ChildStdin>>,
    shared: Arc<StdMutex<ServerSharedState>>,
    workspace: PathBuf,
    protected_paths: Vec<PathBuf>,
    max_diagnostics_per_file: usize,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_lsp_frame(&mut reader).await {
            Ok(Some(body)) => {
                let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                let id_opt = msg.get("id").filter(|v| !v.is_null()).cloned();
                let method_opt = msg.get("method").and_then(Value::as_str);

                match (id_opt, method_opt) {
                    // Response to our outgoing request
                    (Some(id_val), None) => {
                        let Some(req_id) = id_val.as_i64() else {
                            continue;
                        };
                        let outcome = if let Some(err_obj) = msg.get("error") {
                            let msg_txt = err_obj
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("LSP server error");
                            let code = err_obj
                                .get("code")
                                .and_then(Value::as_i64)
                                .unwrap_or(-32000);
                            Err(failure(
                                "lsp_server_error",
                                format!("LSP error ({code}): {msg_txt}"),
                            ))
                        } else {
                            Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                        };
                        if let Ok(mut guard) = shared.lock()
                            && let Some(tx) = guard.pending.remove(&req_id)
                        {
                            let _ = tx.send(outcome);
                        }
                    }
                    // Server -> client request: respond immediately so server never deadlocks.
                    (Some(req_id), Some(method)) => {
                        let response_result = match method {
                            "workspace/configuration" => {
                                let count = msg
                                    .get("params")
                                    .and_then(|p| p.get("items"))
                                    .and_then(Value::as_array)
                                    .map(|a| a.len())
                                    .unwrap_or(0);
                                Value::Array(vec![Value::Null; count])
                            }
                            "workspace/workspaceFolders" => {
                                let uri = path_to_file_uri(&workspace).unwrap_or_default();
                                let name = workspace
                                    .file_name()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("workspace");
                                json!([{ "uri": uri, "name": name }])
                            }
                            "workspace/applyEdit" => {
                                json!({ "applied": false, "failureReason": "workspace edits are disabled in read-only LSP mode" })
                            }
                            // client/registerCapability, window/workDoneProgress/create, etc.
                            _ => Value::Null,
                        };
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "result": response_result,
                        });
                        let _ = write_lsp_message(&stdin, &resp).await;
                    }
                    // Server -> client notification
                    (None, Some(method)) => {
                        if method == "textDocument/publishDiagnostics"
                            && let Some(params) = msg.get("params")
                            && let Some(uri) = params.get("uri").and_then(Value::as_str)
                            && let Some(raw_path) = file_uri_to_path(uri)
                            && let Ok((rel, _)) = validate_workspace_file(
                                &workspace,
                                &raw_path,
                                &protected_paths,
                                None,
                            )
                        {
                            let diags = parse_diagnostic_array(
                                &rel,
                                params.get("diagnostics").and_then(Value::as_array),
                                max_diagnostics_per_file,
                            );
                            if let Ok(mut guard) = shared.lock() {
                                guard.diagnostics.insert(rel, diags);
                                if let Some(notify) = &guard.diag_notify {
                                    notify.notify_waiters();
                                }
                            }
                        } else if method == "$/progress"
                            && let Some(kind) = msg
                                .get("params")
                                .and_then(|p| p.get("value"))
                                .and_then(|v| v.get("kind"))
                                .and_then(Value::as_str)
                            && let Ok(mut guard) = shared.lock()
                        {
                            match kind {
                                "begin" => {
                                    guard.progress_active = guard.progress_active.saturating_add(1)
                                }
                                "end" => {
                                    guard.progress_active = guard.progress_active.saturating_sub(1);
                                    if let Some(notify) = &guard.diag_notify {
                                        notify.notify_waiters();
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    (None, None) => {}
                }
            }
            Ok(None) => {
                drain_pending_with_error(&shared, "LSP server closed stdout");
                break;
            }
            Err(err) => {
                drain_pending_with_error(&shared, &err);
                break;
            }
        }
    }
}

fn drain_pending_with_error(shared: &Arc<StdMutex<ServerSharedState>>, message: &str) {
    if let Ok(mut guard) = shared.lock() {
        guard.read_error = Some(message.to_owned());
        let pending = std::mem::take(&mut guard.pending);
        for (_, tx) in pending {
            let _ = tx.send(Err(failure("lsp_disconnected", message.to_owned())));
        }
        if let Some(notify) = &guard.diag_notify {
            notify.notify_waiters();
        }
    }
}

fn parse_lsp_range(val: &Value) -> Option<(u32, u32, u32, u32)> {
    let start = val.get("start")?;
    let end = val.get("end")?;
    let sl = u32::try_from(start.get("line")?.as_u64()?).ok()? + 1;
    let sc = u32::try_from(start.get("character")?.as_u64()?).ok()? + 1;
    let el = u32::try_from(end.get("line")?.as_u64()?).ok()? + 1;
    let ec = u32::try_from(end.get("character")?.as_u64()?).ok()? + 1;
    Some((sl, sc, el, ec))
}

fn extract_uri_and_range(val: &Value) -> Option<(String, &Value)> {
    if let (Some(uri), Some(range)) = (val.get("uri").and_then(Value::as_str), val.get("range")) {
        return Some((uri.to_owned(), range));
    }
    if let (Some(uri), Some(range)) = (
        val.get("targetUri").and_then(Value::as_str),
        val.get("targetSelectionRange")
            .or_else(|| val.get("targetRange")),
    ) {
        return Some((uri.to_owned(), range));
    }
    None
}

fn parse_diagnostic_severity(raw: Option<u64>) -> &'static str {
    match raw {
        Some(1) => "error",
        Some(2) => "warning",
        Some(3) => "information",
        Some(4) => "hint",
        _ => "unknown",
    }
}

fn parse_diagnostic_array(
    rel: &Path,
    items: Option<&Vec<Value>>,
    limit: usize,
) -> Vec<DiagnosticEntry> {
    let Some(items) = items else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items.iter().take(limit) {
        let Some(range) = item.get("range").and_then(parse_lsp_range) else {
            continue;
        };
        let message = item
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if message.is_empty() {
            continue;
        }
        let code = match item.get("code") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        };
        let source = item
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_owned);
        out.push(DiagnosticEntry {
            path: rel.to_path_buf(),
            severity: parse_diagnostic_severity(item.get("severity").and_then(Value::as_u64))
                .to_owned(),
            code,
            source,
            message,
            start_line: range.0,
            start_col: range.1,
            end_line: range.2,
            end_col: range.3,
        });
    }
    out
}

/// Manages sandboxed LSP server sessions for one workspace.
pub struct LspManager {
    config: LspClientConfig,
    max_file_bytes: usize,
    sessions: HashMap<String, LspServerSession>,
}

impl LspManager {
    pub fn new(config: LspClientConfig, max_file_bytes: usize) -> Self {
        Self {
            config,
            max_file_bytes,
            sessions: HashMap::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.settings.enabled
    }

    pub fn select_server_for_path(&self, rel_or_abs: &Path) -> Option<(String, LspServerSettings)> {
        if !self.config.settings.enabled {
            return None;
        }
        let ext = rel_or_abs
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase());
        let lang = language_id(rel_or_abs);

        for (id, srv) in &self.config.settings.servers {
            if !srv.enabled {
                continue;
            }
            let ext_match = ext.as_ref().is_some_and(|e| {
                srv.extensions
                    .iter()
                    .any(|cand| cand.trim_start_matches('.').eq_ignore_ascii_case(e))
            });
            let lang_match = lang.is_some_and(|l| {
                srv.languages
                    .iter()
                    .any(|cand| cand.eq_ignore_ascii_case(l))
            });
            let marker_match = srv.root_markers.is_empty()
                || srv
                    .root_markers
                    .iter()
                    .any(|m| self.config.workspace.join(m).exists());
            if marker_match && (ext_match || lang_match) {
                return Some((id.clone(), srv.clone()));
            }
        }
        None
    }

    async fn get_or_start_session(
        &mut self,
        server_id: &str,
        server_cfg: &LspServerSettings,
    ) -> Result<&mut LspServerSession, BoundaryError> {
        if let Some(existing) = self.sessions.get_mut(server_id) {
            let dead = existing.process.has_exited().unwrap_or(true)
                || existing
                    .shared
                    .lock()
                    .map(|g| g.read_error.is_some())
                    .unwrap_or(true);
            if dead {
                if let Some(mut stale) = self.sessions.remove(server_id) {
                    stale.shutdown().await;
                }
            } else {
                return Ok(self
                    .sessions
                    .get_mut(server_id)
                    .expect("session just verified"));
            }
        }

        let session = LspServerSession::start(server_id, server_cfg, &self.config).await?;
        self.sessions.insert(server_id.to_owned(), session);
        Ok(self.sessions.get_mut(server_id).expect("session inserted"))
    }

    async fn session_for_path(
        &mut self,
        target: &Path,
    ) -> Result<&mut LspServerSession, BoundaryError> {
        if !self.config.settings.enabled {
            return Err(failure("lsp_disabled", "LSP integration is disabled"));
        }
        let (server_id, server_cfg) = self.select_server_for_path(target).ok_or_else(|| {
            failure(
                "lsp_unavailable",
                format!("no enabled LSP server matches {}", target.display()),
            )
        })?;
        self.get_or_start_session(&server_id, &server_cfg).await
    }

    /// Notify active LSP sessions about a file invalidated on disk (closes doc if deleted or
    /// syncs on next query). Never returns an error if no server is active.
    pub async fn invalidate_path(&mut self, abs_or_rel: &Path) {
        let rel = if abs_or_rel.is_absolute() {
            abs_or_rel
                .strip_prefix(&self.config.workspace)
                .unwrap_or(abs_or_rel)
                .to_path_buf()
        } else {
            abs_or_rel.to_path_buf()
        };
        for session in self.sessions.values_mut() {
            if !session.workspace.join(&rel).exists()
                && let Some(old) = session.open_docs.remove(&rel)
            {
                let _ = session
                    .notify(
                        "textDocument/didClose",
                        json!({ "textDocument": { "uri": old.uri } }),
                    )
                    .await;
            }
        }
    }

    pub async fn definition(
        &mut self,
        file: &Path,
        line: u32,
        column: u32,
        read_tools: Option<&ReadTools>,
        cancel: &dyn Cancellation,
    ) -> Result<Vec<SourceLocation>, BoundaryError> {
        let max_bytes = self.max_file_bytes;
        let session = self.session_for_path(file).await?;
        let (_rel, uri, _ver, _sha, text) = session
            .ensure_document_synced(file, max_bytes, read_tools)
            .await?;
        let (lsp_line, lsp_col) = to_lsp_position(&text, line, column);
        let result = session
            .request(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": lsp_line, "character": lsp_col }
                }),
                cancel,
            )
            .await?;
        Ok(session.parse_locations(result, read_tools))
    }

    pub async fn references(
        &mut self,
        file: &Path,
        line: u32,
        column: u32,
        include_declaration: bool,
        read_tools: Option<&ReadTools>,
        cancel: &dyn Cancellation,
    ) -> Result<Vec<SourceLocation>, BoundaryError> {
        let max_bytes = self.max_file_bytes;
        let session = self.session_for_path(file).await?;
        let (_rel, uri, _ver, _sha, text) = session
            .ensure_document_synced(file, max_bytes, read_tools)
            .await?;
        let (lsp_line, lsp_col) = to_lsp_position(&text, line, column);
        let result = session
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": lsp_line, "character": lsp_col },
                    "context": { "includeDeclaration": include_declaration }
                }),
                cancel,
            )
            .await?;
        Ok(session.parse_locations(result, read_tools))
    }

    pub async fn workspace_symbols(
        &mut self,
        query: &str,
        limit: usize,
        read_tools: Option<&ReadTools>,
        cancel: &dyn Cancellation,
    ) -> Result<Vec<IndexedSymbol>, BoundaryError> {
        if !self.config.settings.enabled {
            return Ok(Vec::new());
        }
        let server_ids: Vec<(String, LspServerSettings)> = self
            .config
            .settings
            .servers
            .iter()
            .filter(|(_, s)| s.enabled)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut results = Vec::new();
        for (id, cfg) in server_ids {
            let Ok(session) = self.get_or_start_session(&id, &cfg).await else {
                continue;
            };
            let Ok(val) = session
                .request("workspace/symbol", json!({ "query": query }), cancel)
                .await
            else {
                continue;
            };
            let Some(arr) = val.as_array() else {
                continue;
            };
            for item in arr {
                let Some(name) = item.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let Some(loc) = item.get("location") else {
                    continue;
                };
                let Some((uri, range_val)) = extract_uri_and_range(loc) else {
                    continue;
                };
                let Some(raw_path) = file_uri_to_path(&uri) else {
                    continue;
                };
                let Ok((rel, abs)) = validate_workspace_file(
                    &self.config.workspace,
                    &raw_path,
                    &self.config.protected_paths,
                    read_tools,
                ) else {
                    continue;
                };
                let Some((sl, sc, el, ec)) = parse_lsp_range(range_val) else {
                    continue;
                };
                let sha = read_regular_no_follow(&abs, self.max_file_bytes)
                    .map(|(b, _, _)| sha256_hex(&b))
                    .unwrap_or_default();
                let kind = item
                    .get("kind")
                    .and_then(Value::as_u64)
                    .map(SymbolKind::from_lsp_kind)
                    .unwrap_or(SymbolKind::Function);
                let container = item
                    .get("containerName")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                results.push(IndexedSymbol {
                    language: language_id(&rel).unwrap_or("unknown").to_owned(),
                    path: rel,
                    name: name.to_owned(),
                    container,
                    kind,
                    start_line: sl,
                    start_col: sc,
                    end_line: el,
                    end_col: ec,
                    signature: None,
                    file_sha256: sha,
                });
                if results.len() >= limit {
                    return Ok(results);
                }
            }
        }
        Ok(results)
    }

    pub async fn diagnostics(
        &mut self,
        file: &Path,
        read_tools: Option<&ReadTools>,
        cancel: &dyn Cancellation,
    ) -> Result<Vec<DiagnosticEntry>, BoundaryError> {
        let max_bytes = self.max_file_bytes;
        let max_diag = self.config.settings.max_diagnostics_per_file;
        let wait_timeout = Duration::from_secs(self.config.settings.request_timeout_seconds)
            .min(Duration::from_secs(3));
        let session = self.session_for_path(file).await?;
        let (rel, uri, _ver, _sha, _text) = session
            .ensure_document_synced(file, max_bytes, read_tools)
            .await?;

        if session.supports_pull_diagnostics()
            && let Ok(val) = session
                .request(
                    "textDocument/diagnostic",
                    json!({ "textDocument": { "uri": uri } }),
                    cancel,
                )
                .await
            && let Some(items) = val.get("items").and_then(Value::as_array)
        {
            return Ok(parse_diagnostic_array(&rel, Some(items), max_diag));
        }

        session
            .wait_for_diagnostics_if_needed(&rel, wait_timeout, cancel)
            .await;

        let diags = session
            .shared
            .lock()
            .map_err(|_| failure("lsp_error", "LSP diagnostic state unavailable"))?
            .diagnostics
            .get(&rel)
            .cloned()
            .unwrap_or_default();
        Ok(diags)
    }

    /// Collect read-only unified diff suggestions from LSP `rename` or `codeAction`, verifying
    /// exact SHA-256 base versions (`STALE_INDEX` on concurrent edits) and NEVER modifying workspace files.
    pub async fn edit_suggestions(
        &mut self,
        query: EditSuggestionQuery<'_>,
        read_tools: Option<&ReadTools>,
        cancel: &dyn Cancellation,
    ) -> Result<Vec<EditSuggestion>, BoundaryError> {
        let max_bytes = self.max_file_bytes;
        let max_edit_files = self.config.settings.max_edit_files;
        let workspace = self.config.workspace.clone();
        let protected = self.config.protected_paths.clone();

        let session = self.session_for_path(query.file).await?;
        let (rel, uri, _ver, initial_sha, text) = session
            .ensure_document_synced(query.file, max_bytes, read_tools)
            .await?;

        if let Some(expected) = query.expected_file_sha256
            && expected != initial_sha
        {
            return Err(failure(
                "STALE_INDEX",
                format!(
                    "target {} changed on disk (expected sha256 {expected}, found {initial_sha})",
                    rel.display()
                ),
            ));
        }

        let (sl, sc) = to_lsp_position(&text, query.line, query.column);
        let (el, ec) = to_lsp_position(
            &text,
            query.end_line.unwrap_or(query.line),
            query.end_column.unwrap_or(query.column),
        );

        let mut raw_edits: Vec<(String, Option<String>, Value)> = Vec::new();
        if let Some(rename_to) = query.new_name {
            let trimmed = rename_to.trim();
            if trimmed.is_empty() {
                return Err(failure("invalid_args", "new_name must not be empty"));
            }
            let ws_edit = session
                .request(
                    "textDocument/rename",
                    json!({
                        "textDocument": { "uri": uri },
                        "position": { "line": sl, "character": sc },
                        "newName": trimmed,
                    }),
                    cancel,
                )
                .await?;
            if !ws_edit.is_null() {
                raw_edits.push((
                    format!("Rename symbol to `{trimmed}`"),
                    Some("refactor.rename".into()),
                    ws_edit,
                ));
            }
        } else {
            // Pull or cached diagnostics overlapping this range for codeAction context.
            let diagnostics_val = if session.supports_pull_diagnostics() {
                session
                    .request(
                        "textDocument/diagnostic",
                        json!({ "textDocument": { "uri": uri } }),
                        cancel,
                    )
                    .await
                    .ok()
                    .and_then(|v| v.get("items").cloned())
                    .unwrap_or_else(|| json!([]))
            } else {
                Value::Array(Vec::new())
            };

            let mut context_obj = json!({ "diagnostics": diagnostics_val });
            if !query.only_kinds.is_empty() {
                context_obj["only"] = json!(query.only_kinds);
            }
            let actions_val = session
                .request(
                    "textDocument/codeAction",
                    json!({
                        "textDocument": { "uri": uri },
                        "range": {
                            "start": { "line": sl, "character": sc },
                            "end": { "line": el, "character": ec }
                        },
                        "context": context_obj,
                    }),
                    cancel,
                )
                .await?;
            if let Some(arr) = actions_val.as_array() {
                for action in arr {
                    let title = action
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("Suggested edit")
                        .to_owned();
                    let kind = action
                        .get("kind")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let resolved = if action.get("edit").is_some_and(|e| !e.is_null()) {
                        action.clone()
                    } else if session.supports_code_action_resolve() {
                        session
                            .request("codeAction/resolve", action.clone(), cancel)
                            .await
                            .unwrap_or_else(|_| action.clone())
                    } else {
                        action.clone()
                    };
                    if let Some(edit) = resolved.get("edit").filter(|e| !e.is_null()) {
                        raw_edits.push((title, kind, edit.clone()));
                    }
                }
            }
        }

        // Snapshot open document versions/hashes from session for concurrency validation.
        let open_hashes: BTreeMap<PathBuf, (i32, String)> = session
            .open_docs
            .iter()
            .map(|(p, doc)| (p.clone(), (doc.version, doc.sha256.clone())))
            .collect();

        // Also re-check target file hash after the async LSP roundtrip.
        let (_target_rel, target_abs) =
            validate_workspace_file(&workspace, &rel, &protected, read_tools)?;
        let (recheck_bytes, _, _) = read_regular_no_follow(&target_abs, max_bytes)?;
        let recheck_sha = sha256_hex(&recheck_bytes);
        if recheck_sha != initial_sha {
            return Err(failure(
                "STALE_INDEX",
                format!(
                    "file {} was modified on disk while computing edit suggestions",
                    rel.display()
                ),
            ));
        }

        let mut suggestions = Vec::new();
        for (title, kind, ws_edit) in raw_edits {
            let file_edit_map = collect_workspace_text_edits(&ws_edit, max_edit_files)?;
            if file_edit_map.is_empty() {
                continue;
            }
            let mut preview_files = Vec::new();
            let mut blocked = false;
            for UriTextEdits {
                uri: raw_uri,
                expected_version: expected_ver,
                edits: text_edits,
            } in file_edit_map
            {
                let Some(raw_path) = file_uri_to_path(&raw_uri) else {
                    blocked = true;
                    break;
                };
                let Ok((edit_rel, edit_abs)) =
                    validate_workspace_file(&workspace, &raw_path, &protected, read_tools)
                else {
                    blocked = true;
                    break;
                };
                if !is_allowed_checked_path(&workspace, &edit_abs, &protected, read_tools) {
                    blocked = true;
                    break;
                }
                let (disk_bytes, _, _) = read_regular_no_follow(&edit_abs, max_bytes)?;
                let original = String::from_utf8(disk_bytes).map_err(|_| {
                    failure(
                        "invalid_args",
                        format!("{} is not valid UTF-8 text", edit_rel.display()),
                    )
                })?;
                let disk_sha = sha256_hex(original.as_bytes());
                if let Some((opened_ver, opened_sha)) = open_hashes.get(&edit_rel)
                    && (opened_sha != &disk_sha || expected_ver.is_some_and(|v| v != *opened_ver))
                {
                    return Err(failure(
                        "STALE_INDEX",
                        format!(
                            "edit suggestion targets modified file {} (disk sha256 {disk_sha})",
                            edit_rel.display()
                        ),
                    ));
                }
                let updated = apply_lsp_text_edits(&original, &text_edits)?;
                if updated == original {
                    continue;
                }
                let diff = create_patch(&original, &updated).to_string();
                preview_files.push(EditSuggestionFile {
                    path: edit_rel,
                    base_sha256: disk_sha,
                    unified_diff: diff,
                });
            }
            if !blocked && !preview_files.is_empty() {
                suggestions.push(EditSuggestion {
                    title,
                    kind,
                    files: preview_files,
                });
            }
        }

        Ok(suggestions)
    }

    pub async fn shutdown_all(&mut self) {
        let keys: Vec<String> = self.sessions.keys().cloned().collect();
        for key in keys {
            if let Some(mut session) = self.sessions.remove(&key) {
                session.shutdown().await;
            }
        }
    }

    /// Expose cached external workspace/cache path metadata for diagnostics/tests.
    pub fn active_lsp_cache_dirs(&self) -> Vec<PathBuf> {
        self.sessions
            .values()
            .map(|s| s.lsp_cache_dir.clone())
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EditSuggestionQuery<'a> {
    pub file: &'a Path,
    pub line: u32,
    pub column: u32,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
    pub new_name: Option<&'a str>,
    pub only_kinds: &'a [String],
    pub expected_file_sha256: Option<&'a str>,
}

struct UriTextEdits {
    uri: String,
    expected_version: Option<i32>,
    edits: Vec<LspTextEdit>,
}

#[derive(Debug, Clone)]
struct LspTextEdit {
    start_line: u32,
    start_char: u32,
    end_line: u32,
    end_char: u32,
    new_text: String,
}

fn collect_workspace_text_edits(
    ws_edit: &Value,
    max_edit_files: usize,
) -> Result<Vec<UriTextEdits>, BoundaryError> {
    let mut by_uri: BTreeMap<String, (Option<i32>, Vec<LspTextEdit>)> = BTreeMap::new();

    if let Some(doc_changes) = ws_edit.get("documentChanges").and_then(Value::as_array) {
        for item in doc_changes {
            if item.get("kind").is_some() {
                return Err(failure(
                    "unsupported_edit",
                    "resource create/rename/delete operations are not supported in edit suggestions",
                ));
            }
            let Some(td) = item.get("textDocument") else {
                continue;
            };
            let Some(uri) = td.get("uri").and_then(Value::as_str) else {
                continue;
            };
            let ver = td
                .get("version")
                .and_then(Value::as_i64)
                .and_then(|v| i32::try_from(v).ok());
            let Some(edits) = item.get("edits").and_then(Value::as_array) else {
                continue;
            };
            let entry = by_uri
                .entry(uri.to_owned())
                .or_insert_with(|| (ver, Vec::new()));
            for edit in edits {
                if let Some(parsed) = parse_text_edit(edit) {
                    entry.1.push(parsed);
                }
            }
        }
    } else if let Some(changes) = ws_edit.get("changes").and_then(Value::as_object) {
        for (uri, edits_val) in changes {
            let Some(edits) = edits_val.as_array() else {
                continue;
            };
            let entry = by_uri
                .entry(uri.clone())
                .or_insert_with(|| (None, Vec::new()));
            for edit in edits {
                if let Some(parsed) = parse_text_edit(edit) {
                    entry.1.push(parsed);
                }
            }
        }
    }

    if by_uri.len() > max_edit_files {
        return Err(failure(
            "edit_too_large",
            format!(
                "edit suggestion touches {} files, exceeding limit {max_edit_files}",
                by_uri.len()
            ),
        ));
    }

    Ok(by_uri
        .into_iter()
        .map(|(uri, (expected_version, edits))| UriTextEdits {
            uri,
            expected_version,
            edits,
        })
        .collect())
}

fn parse_text_edit(val: &Value) -> Option<LspTextEdit> {
    let range = val.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    Some(LspTextEdit {
        start_line: u32::try_from(start.get("line")?.as_u64()?).ok()?,
        start_char: u32::try_from(start.get("character")?.as_u64()?).ok()?,
        end_line: u32::try_from(end.get("line")?.as_u64()?).ok()?,
        end_char: u32::try_from(end.get("character")?.as_u64()?).ok()?,
        new_text: val.get("newText")?.as_str()?.to_owned(),
    })
}

fn apply_lsp_text_edits(original: &str, edits: &[LspTextEdit]) -> Result<String, BoundaryError> {
    let starts = line_starts(original);
    let mut byte_edits = Vec::with_capacity(edits.len());
    for edit in edits {
        let start = lsp_pos_to_byte_offset(original, &starts, edit.start_line, edit.start_char)
            .ok_or_else(|| failure("invalid_edit", "LSP edit start position out of range"))?;
        let end = lsp_pos_to_byte_offset(original, &starts, edit.end_line, edit.end_char)
            .ok_or_else(|| failure("invalid_edit", "LSP edit end position out of range"))?;
        if end < start {
            return Err(failure("invalid_edit", "LSP edit has inverted range"));
        }
        byte_edits.push((start, end, &edit.new_text));
    }
    byte_edits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    let mut result = original.to_owned();
    let mut prev_start = usize::MAX;
    for (start, end, new_text) in byte_edits {
        if end > prev_start {
            return Err(failure(
                "invalid_edit",
                "LSP returned overlapping text edits",
            ));
        }
        result.replace_range(start..end, new_text);
        prev_start = start;
    }
    Ok(result)
}
