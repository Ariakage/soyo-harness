//! End-to-end tests for F-17 (External SQLite Project Index & Sandboxed LSP Code Intelligence).

use soyo_core::{
    boundary::{Cancellation, Model, ModelRequest, ModelStream, ModelUpdate, Operation, Store},
    code_intel::{CodeIntelConfig, CodeIntelState, LspClientConfig, LspManager, SymbolKind},
    config::{CodeIntelSettings, Environment, LspServerSettings, LspSettings},
    contract::{
        ApprovalRequest, HistoryItem, ModelProtocol, PermissionMode, Run, RunId, RunStatus,
        Session, SessionId, ToolCall, ToolResult, WorkMode,
    },
    execution::BackendKind,
    memory::MemoryStore,
    permissions::{PermissionConfig, Policy},
    prompt,
    review::ReviewConfig,
    runtime::{AgentRuntime, ApprovalHandler, RuntimeLimits, RuntimeSetup},
    tools::{
        exec::ExecConfig,
        read::{ReadLimits, ReadTools},
    },
};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::TempDir;

#[cfg(target_os = "macos")]
const BOOTSTRAP_C: &str = r###"#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int fill(int fd, void *target, size_t remaining) {
    char *position = target;
    while (remaining > 0) {
        ssize_t count = read(fd, position, remaining);
        if (count <= 0) return -1;
        position += count;
        remaining -= (size_t)count;
    }
    return 0;
}

static int number(int fd, uint32_t *value) {
    unsigned char bytes[4];
    if (fill(fd, bytes, sizeof(bytes))) return -1;
    *value = ((uint32_t)bytes[0] << 24) | ((uint32_t)bytes[1] << 16)
           | ((uint32_t)bytes[2] << 8) | bytes[3];
    return 0;
}

int main(int argc, char **argv) {
    int fd = -1;
    int separator = -1;
    if (argc < 6 || strcmp(argv[1], "__soyo-sandbox-launch")) return 64;
    for (int index = 2; index < argc; index++) {
        if (!strcmp(argv[index], "--env-fd") && index + 1 < argc) {
            fd = atoi(argv[++index]);
            continue;
        }
        if (!strcmp(argv[index], "--")) {
            separator = index;
            break;
        }
        return 64;
    }
    if (fd < 3 || separator < 0 || separator + 1 >= argc) return 64;
    char magic[8];
    uint32_t count = 0;
    if (fill(fd, magic, sizeof(magic)) || memcmp(magic, "SOYOENV1", 8)
        || number(fd, &count) || count > 1048576) return 74;
    char **environment = calloc((size_t)count + 1, sizeof(char *));
    if (!environment) return 74;
    size_t total = 12;
    const size_t limit = 8 * 1024 * 1024;
    for (uint32_t index = 0; index < count; index++) {
        uint32_t key_length;
        uint32_t value_length;
        if (number(fd, &key_length) || number(fd, &value_length)
            || key_length == 0 || key_length > limit || value_length > limit
            || total + 8 + key_length + value_length > limit) return 74;
        char *entry = malloc((size_t)key_length + value_length + 2);
        if (!entry) return 74;
        if (fill(fd, entry, key_length)
            || fill(fd, entry + key_length + 1, value_length)) return 74;
        entry[key_length] = '=';
        entry[key_length + value_length + 1] = 0;
        environment[index] = entry;
        total += 8 + key_length + value_length;
    }
    char extra;
    if (read(fd, &extra, 1) != 0) return 74;
    close(fd);
    execve(argv[separator + 1], argv + separator + 1, environment);
    return errno == 0 ? 74 : errno;
}
"###;

fn prepare_test_sandbox_backend() -> BackendKind {
    static BACKEND: std::sync::OnceLock<(Option<TempDir>, BackendKind)> =
        std::sync::OnceLock::new();
    let (_, backend) = BACKEND.get_or_init(|| {
        if let Some(path) = std::env::var_os("SOYO_TEST_EXECUTABLE")
            .or_else(|| std::env::var_os("SAYA_TEST_EXECUTABLE"))
        {
            soyo_core::execution::pin_launcher_image_from(Path::new(&path))
                .expect("pin explicit SOYO_TEST_EXECUTABLE");
            return (None, BackendKind::Auto);
        }
        #[cfg(target_os = "macos")]
        if Path::new("/usr/bin/cc").exists() {
            let dir = tempfile::tempdir().expect("bootstrap dir");
            let src = dir.path().join("bootstrap.c");
            let img = dir.path().join("bootstrap");
            fs::write(&src, BOOTSTRAP_C).unwrap();
            let status = std::process::Command::new("/usr/bin/cc")
                .args(["-Wall", "-Wextra", "-Werror"])
                .arg(&src)
                .arg("-o")
                .arg(&img)
                .status()
                .unwrap();
            if status.success() {
                soyo_core::execution::pin_launcher_image_from(&img).expect("pin bootstrap image");
                return (Some(dir), BackendKind::Auto);
            }
        }
        (None, BackendKind::Host)
    });
    *backend
}

struct TestCancel;
impl Cancellation for TestCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::pending())
    }
}

struct ScriptStream {
    items: VecDeque<ModelUpdate>,
}
impl ModelStream for ScriptStream {
    fn next<'a>(&'a mut self, _cancel: &'a dyn Cancellation) -> Operation<'a, Option<ModelUpdate>> {
        Box::pin(async move { Ok(self.items.pop_front()) })
    }
}

struct ScriptModel {
    responses: Mutex<VecDeque<Vec<ModelUpdate>>>,
    seen_requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptModel {
    fn new(responses: Vec<Vec<ModelUpdate>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            seen_requests: Mutex::new(Vec::new()),
        }
    }
}

impl Model for ScriptModel {
    fn start<'a>(
        &'a self,
        request: &'a ModelRequest,
        _cancel: &'a dyn Cancellation,
    ) -> Operation<'a, Box<dyn ModelStream>> {
        Box::pin(async move {
            self.seen_requests.lock().unwrap().push(request.clone());
            let mut guard = self.responses.lock().unwrap();
            let updates = guard.pop_front().unwrap_or_else(|| {
                vec![ModelUpdate::Finished {
                    items: vec![HistoryItem::AssistantText("done".into())],
                    usage: None,
                }]
            });
            Ok(Box::new(ScriptStream {
                items: updates.into(),
            }) as Box<dyn ModelStream>)
        })
    }
}

struct StaticApprover(Option<bool>);
impl ApprovalHandler for StaticApprover {
    fn confirm<'a>(
        &'a self,
        _request: &'a ApprovalRequest,
        _cancel: &'a dyn Cancellation,
    ) -> Operation<'a, Option<bool>> {
        Box::pin(async move { Ok(self.0) })
    }
}

fn finish_turn(items: Vec<HistoryItem>) -> Vec<ModelUpdate> {
    vec![ModelUpdate::Finished { items, usage: None }]
}

fn default_review(profile: &str) -> ReviewConfig {
    ReviewConfig::new(profile.into())
}

fn base_setup(
    ws: &Path,
    model: Arc<dyn Model>,
    store: Arc<dyn Store>,
    work_mode: WorkMode,
    permissions: PermissionMode,
    approver: Option<Arc<dyn ApprovalHandler>>,
) -> (RuntimeSetup, Session) {
    let session = Session {
        id: SessionId::new(),
        workspace: ws.to_path_buf(),
        work_mode,
        profile: "default".into(),
        protocol: ModelProtocol::Responses,
        model: "fake-model".into(),
        history: vec![HistoryItem::UserText("inspect workspace".into())],
    };
    let run = Run {
        id: RunId::new(),
        session_id: session.id,
        work_mode,
        permissions,
    };
    let perm_cfg = PermissionConfig::default();
    let policy = Arc::new(Policy::new(ws, perm_cfg).unwrap());
    let files = Arc::new(
        ReadTools::new(
            ws,
            &[],
            permissions == PermissionMode::Bypass,
            ReadLimits::default(),
        )
        .unwrap(),
    );
    let setup = RuntimeSetup {
        run,
        model,
        store,
        policy,
        files,
        cancellation: Arc::new(TestCancel),
        review: default_review("default"),
        exec: ExecConfig::default(),
        preauthorizations: Default::default(),
        known_secrets: Vec::new(),
        summary: "code intel test".into(),
        confirmation: approver,
        limits: RuntimeLimits::default(),
    };
    (setup, session)
}

#[tokio::test]
async fn disabled_code_intel_keeps_tool_surface_and_prompt_unchanged() {
    let ws = TempDir::new().unwrap();
    fs::write(ws.path().join("lib.rs"), "pub fn alpha() {}\n").unwrap();

    let model = Arc::new(ScriptModel::new(vec![finish_turn(vec![
        HistoryItem::AssistantText("ok".into()),
    ])]));
    let store = Arc::new(MemoryStore::default());
    let (setup, mut session) = base_setup(
        ws.path(),
        model.clone(),
        store,
        WorkMode::Work,
        PermissionMode::Auto,
        None,
    );
    let mut rt = AgentRuntime::new(setup).unwrap();
    let result = rt.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);

    let reqs = model.seen_requests.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    assert!(!reqs[0].tools.iter().any(|t| t.name.starts_with("code_")));
    assert!(!reqs[0].instructions.contains("code_search_symbols"));

    let prompt_without =
        prompt::execution(WorkMode::Work, PermissionMode::Auto, &reqs[0].tools).unwrap();
    assert!(!prompt_without.section_ids.contains(&"code_intel"));
}

#[tokio::test]
async fn external_sqlite_index_enforces_cache_separation_security_and_incremental_refresh() {
    let ws = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();

    // Overlapping cache dirs must be rejected unconditionally.
    let inside_ws = ws.path().join(".soyo-index-cache");
    assert!(
        CodeIntelState::new(CodeIntelConfig {
            workspace: ws.path().to_path_buf(),
            cache_dir: inside_ws,
            sandbox_backend: BackendKind::Auto,
            settings: CodeIntelSettings {
                enabled: true,
                ..CodeIntelSettings::default()
            },
            protected_paths: Vec::new(),
            host_env: Environment::default(),
        })
        .is_err()
    );
    assert!(
        CodeIntelState::new(CodeIntelConfig {
            workspace: ws.path().to_path_buf(),
            cache_dir: ws.path().parent().unwrap().to_path_buf(),
            sandbox_backend: BackendKind::Auto,
            settings: CodeIntelSettings {
                enabled: true,
                ..CodeIntelSettings::default()
            },
            protected_paths: Vec::new(),
            host_env: Environment::default(),
        })
        .is_err()
    );

    // Populate workspace with gitignore, .git, protected secrets dir, and symlink escape.
    fs::write(ws.path().join(".gitignore"), "ignored_gen/\n*.ignored.rs\n").unwrap();
    fs::create_dir_all(ws.path().join("src")).unwrap();
    fs::create_dir_all(ws.path().join("ignored_gen")).unwrap();
    fs::create_dir_all(ws.path().join(".git")).unwrap();
    fs::create_dir_all(ws.path().join("secrets")).unwrap();

    fs::write(
        ws.path().join("src/main.rs"),
        "pub struct Engine;\npub fn boot_sequence() -> i32 { 1 }\n",
    )
    .unwrap();
    fs::write(
        ws.path().join("src/helper.ts"),
        "export interface Config { ok: boolean }\nexport function computeHash() { return 42; }\n",
    )
    .unwrap();
    fs::write(
        ws.path().join("ignored_gen/secret_ignored.rs"),
        "pub fn ignored_symbol() {}\n",
    )
    .unwrap();
    fs::write(
        ws.path().join(".git/config"),
        "[core]\nrepositoryformatversion = 0\n",
    )
    .unwrap();
    fs::write(
        ws.path().join(".git/hooks.rs"),
        "pub fn git_control_symbol() {}\n",
    )
    .unwrap();
    let protected_file = ws.path().join("secrets/api.rs");
    fs::write(&protected_file, "pub fn protected_token_secret() {}\n").unwrap();

    let outside_dir = TempDir::new().unwrap();
    let outside_file = outside_dir.path().join("escape.rs");
    fs::write(&outside_file, "pub fn escaped_symlink_symbol() {}\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside_file, ws.path().join("src/escape.rs")).unwrap();

    // Even in Bypass permission mode, ProtectedPaths and .git remain invisible.
    let read_tools = ReadTools::new(ws.path(), &[], true, ReadLimits::default()).unwrap();
    read_tools
        .protect_paths(vec![ws.path().join("secrets")])
        .unwrap();

    let state = CodeIntelState::new(CodeIntelConfig {
        workspace: ws.path().to_path_buf(),
        cache_dir: cache.path().to_path_buf(),
        sandbox_backend: BackendKind::Auto,
        settings: CodeIntelSettings {
            enabled: true,
            inject_context: true,
            max_context_bytes: 512,
            ..CodeIntelSettings::default()
        },
        protected_paths: vec![ws.path().join("secrets")],
        host_env: Environment::default(),
    })
    .unwrap();

    let status = state.build_index(Some(&read_tools)).await.unwrap();
    let canon_cache = fs::canonicalize(cache.path()).unwrap();
    let canon_ws = fs::canonicalize(ws.path()).unwrap();
    assert!(status.db_path.starts_with(&canon_cache));
    assert!(!status.db_path.starts_with(&canon_ws));
    assert_eq!(status.indexed_files, 2);

    assert_eq!(
        state
            .search_symbols(
                "boot_sequence",
                Some(SymbolKind::Function),
                None,
                None,
                Some(&read_tools)
            )
            .await
            .unwrap()
            .matches
            .len(),
        1
    );
    assert_eq!(
        state
            .search_symbols("computeHash", None, None, None, Some(&read_tools))
            .await
            .unwrap()
            .matches
            .len(),
        1
    );
    assert!(
        state
            .search_symbols("ignored_symbol", None, None, None, Some(&read_tools))
            .await
            .unwrap()
            .matches
            .is_empty()
    );
    assert!(
        state
            .search_symbols("git_control_symbol", None, None, None, Some(&read_tools))
            .await
            .unwrap()
            .matches
            .is_empty()
    );
    assert!(
        state
            .search_symbols(
                "protected_token_secret",
                None,
                None,
                None,
                Some(&read_tools)
            )
            .await
            .unwrap()
            .matches
            .is_empty()
    );
    assert!(
        state
            .search_symbols(
                "escaped_symlink_symbol",
                None,
                None,
                None,
                Some(&read_tools)
            )
            .await
            .unwrap()
            .matches
            .is_empty()
    );

    assert!(
        state
            .file_outline(Path::new("secrets/api.rs"), Some(&read_tools))
            .await
            .is_err()
    );
    assert!(
        state
            .file_outline(Path::new(".git/hooks.rs"), Some(&read_tools))
            .await
            .is_err()
    );
    #[cfg(unix)]
    assert!(
        state
            .file_outline(Path::new("src/escape.rs"), Some(&read_tools))
            .await
            .is_err()
    );

    let (digest_before, summary_before) = state
        .context_summary(Some(&read_tools))
        .await
        .unwrap()
        .unwrap();
    assert!(summary_before.len() <= 512);
    assert!(summary_before.contains("boot_sequence"));

    // Incremental update & deletion + invalidation.
    fs::write(
        ws.path().join("src/main.rs"),
        "pub struct Engine;\npub fn boot_v2() -> i32 { 2 }\n",
    )
    .unwrap();
    state.invalidate_path(Path::new("src/main.rs")).await;
    assert!(
        state
            .search_symbols("boot_sequence", None, None, None, Some(&read_tools))
            .await
            .unwrap()
            .matches
            .is_empty()
    );
    assert_eq!(
        state
            .search_symbols("boot_v2", None, None, None, Some(&read_tools))
            .await
            .unwrap()
            .matches
            .len(),
        1
    );
    let (digest_after, summary_after) = state
        .context_summary(Some(&read_tools))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(digest_before, digest_after);
    assert!(summary_after.contains("boot_v2"));
}

const MOCK_LSP_PY: &str = r#"import json, os, sys
from urllib.parse import unquote, urlparse

def read_msg():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        s = line.decode("ascii").strip()
        if not s:
            break
        k, v = s.split(":", 1)
        headers[k.strip().lower()] = v.strip()
    n = int(headers["content-length"])
    return json.loads(sys.stdin.buffer.read(n).decode("utf-8"))

def send_msg(obj):
    raw = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(raw)}\r\n\r\n".encode("ascii") + raw)
    sys.stdout.buffer.flush()

docs = {}
root_uri = None
marker_dir = os.environ["HOME"]

# Verify write-restriction: workspace write must fail under Seatbelt/Bwrap; external cache write succeeds.
ws_path = os.environ["MOCK_LSP_WS"]
outside_dir = os.environ["MOCK_LSP_FORBIDDEN_DIR"]
try:
    with open(os.path.join(ws_path, "forbidden_lsp_write.txt"), "w") as f:
        f.write("oops")
    ws_write_blocked = False
except Exception:
    ws_write_blocked = True

try:
    with open(os.path.join(outside_dir, "forbidden_outside_write.txt"), "w") as f:
        f.write("oops")
    outside_write_blocked = False
except Exception:
    outside_write_blocked = True

with open(os.path.join(marker_dir, "probe.json"), "w") as f:
    json.dump({
        "pid": os.getpid(),
        "ws_write_blocked": ws_write_blocked,
        "outside_write_blocked": outside_write_blocked,
        "cargo_target_dir": os.environ.get("CARGO_TARGET_DIR", ""),
        "xdg_cache_home": os.environ.get("XDG_CACHE_HOME", ""),
        "tmpdir": os.environ.get("TMPDIR", ""),
        "home": os.environ.get("HOME", ""),
    }, f)

while True:
    msg = read_msg()
    if msg is None:
        break
    method = msg.get("method")
    req_id = msg.get("id")
    params = msg.get("params") or {}
    if method == "initialize":
        root_uri = params.get("rootUri")
        # Send server->client configuration request before replying to verify async request handling.
        send_msg({"jsonrpc": "2.0", "id": 9901, "method": "workspace/configuration", "params": {"items": [{"section": "rust-analyzer"}]}})
        send_msg({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "capabilities": {
                    "textDocumentSync": 1,
                    "definitionProvider": True,
                    "referencesProvider": True,
                    "workspaceSymbolProvider": True,
                    "diagnosticProvider": {"interFileDependencies": False, "workspaceDiagnostics": False},
                    "codeActionProvider": True,
                    "renameProvider": True,
                }
            }
        })
    elif req_id is not None and method is None:
        # Ignore responses to server->client requests
        continue
    elif method == "initialized":
        continue
    elif method == "textDocument/didOpen":
        td = params["textDocument"]
        docs[td["uri"]] = {"version": td["version"], "text": td["text"]}
        send_msg({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": td["uri"],
                "diagnostics": [{
                    "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 5}},
                    "severity": 2,
                    "code": "W001",
                    "source": "mock-lsp",
                    "message": "sample diagnostic warning"
                }]
            }
        })
    elif method == "textDocument/didChange":
        td = params["textDocument"]
        docs[td["uri"]] = {"version": td["version"], "text": params["contentChanges"][0]["text"]}
        with open(os.path.join(marker_dir, "did_change.txt"), "a") as f:
            f.write(str(td["version"]) + "\n")
    elif method == "textDocument/definition":
        uri = params["textDocument"]["uri"]
        bad_git_uri = f"file://{ws_path}/.git/HEAD"
        secret_uri = f"file://{ws_path}/secrets/key.rs"
        send_msg({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": [
                {"uri": uri, "range": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 11}}},
                {"uri": bad_git_uri, "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}},
                {"uri": secret_uri, "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}},
            ]
        })
    elif method == "textDocument/references":
        uri = params["textDocument"]["uri"]
        send_msg({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": [
                {"uri": uri, "range": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 11}}},
                {"uri": uri, "range": {"start": {"line": 1, "character": 16}, "end": {"line": 1, "character": 20}}},
            ]
        })
    elif method == "workspace/symbol":
        main_uri = f"file://{ws_path}/src/lib.rs"
        send_msg({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": [{
                "name": "lspOnlySymbol",
                "kind": 12,
                "location": {"uri": main_uri, "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 12}}}
            }]
        })
    elif method == "textDocument/diagnostic":
        send_msg({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "kind": "full",
                "items": [{
                    "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 8}},
                    "severity": 1,
                    "code": "E042",
                    "source": "mock-lsp",
                    "message": "pull diagnostic issue"
                }]
            }
        })
    elif method == "textDocument/rename":
        uri = params["textDocument"]["uri"]
        ver = docs[uri]["version"]
        new_name = params["newName"]
        send_msg({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "documentChanges": [{
                    "textDocument": {"uri": uri, "version": ver},
                    "edits": [
                        {"range": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 10}}, "newText": new_name},
                        {"range": {"start": {"line": 1, "character": 16}, "end": {"line": 1, "character": 19}}, "newText": new_name},
                    ]
                }]
            }
        })
    elif method == "shutdown":
        with open(os.path.join(marker_dir, "shutdown.txt"), "w") as f:
            f.write("clean\n")
        send_msg({"jsonrpc": "2.0", "id": req_id, "result": None})
    elif method == "exit":
        break
"#;

#[tokio::test]
async fn sandboxed_mock_lsp_end_to_end_tools_diff_stale_and_permissions() {
    let Some(python_path) = [
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
        "/Library/Developer/CommandLineTools/usr/bin/python3",
        "/usr/bin/python3",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.exists()) else {
        eprintln!("SKIP mock LSP test: python3 not found");
        return;
    };
    let ws_tmp = TempDir::new().unwrap();
    let cache_tmp = TempDir::new().unwrap();
    let script_tmp = TempDir::new().unwrap();
    let ws = fs::canonicalize(ws_tmp.path()).unwrap();
    let cache = fs::canonicalize(cache_tmp.path()).unwrap();
    let script_dir = fs::canonicalize(script_tmp.path()).unwrap();
    let mut py_read_roots = vec![script_dir.clone()];
    for homebrew_root in ["/opt/homebrew", "/usr/local/opt", "/usr/local/Cellar"] {
        let p = Path::new(homebrew_root);
        if p.exists() {
            py_read_roots.push(p.to_path_buf());
        }
    }
    let marker_dir = cache.join("lsp-probe");
    fs::create_dir_all(&marker_dir).unwrap();

    fs::create_dir_all(ws.join("src")).unwrap();
    fs::create_dir_all(ws.join(".git")).unwrap();
    fs::create_dir_all(ws.join("secrets")).unwrap();
    fs::write(ws.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(ws.join("secrets/key.rs"), "pub fn secret() {}\n").unwrap();
    fs::write(
        ws.join("src/lib.rs"),
        "pub fn foo() -> i32 { 1 }\npub fn bar() -> i32 { foo() }\n",
    )
    .unwrap();

    let mock_script = script_dir.join("mock_lsp.py");
    fs::write(&mock_script, MOCK_LSP_PY).unwrap();

    let mut servers = BTreeMap::new();
    servers.insert(
        "mock-rs".to_owned(),
        LspServerSettings {
            enabled: true,
            argv: vec![
                python_path.to_string_lossy().into_owned(),
                mock_script.to_string_lossy().into_owned(),
            ],
            languages: vec!["rust".into()],
            extensions: vec!["rs".into()],
            root_markers: Vec::new(),
            env: BTreeMap::from([
                ("MOCK_LSP_WS".into(), ws.to_string_lossy().into_owned()),
                (
                    "MOCK_LSP_FORBIDDEN_DIR".into(),
                    marker_dir.to_string_lossy().into_owned(),
                ),
            ]),
            read_roots: py_read_roots,
            initialization_options: None,
        },
    );

    let backend = prepare_test_sandbox_backend();
    let code_intel_cfg = CodeIntelConfig {
        workspace: ws.clone(),
        cache_dir: cache.clone(),
        sandbox_backend: backend,
        settings: CodeIntelSettings {
            enabled: true,
            inject_context: true,
            lsp: LspSettings {
                enabled: true,
                request_timeout_seconds: 10,
                startup_timeout_seconds: 10,
                servers,
                ..LspSettings::default()
            },
            ..CodeIntelSettings::default()
        },
        protected_paths: vec![ws.join("secrets")],
        host_env: Environment::capture(),
    };

    // Scripted multi-turn agent run in PLAN mode exercising all 6 code_* tools + write invalidation.
    let model = Arc::new(ScriptModel::new(vec![
        finish_turn(vec![
            HistoryItem::ToolCall(ToolCall {
                call_id: "c1".into(),
                name: "code_search_symbols".into(),
                arguments: serde_json::json!({"query": "lspOnlySymbol", "use_lsp": true}).to_string(),
            }),
            HistoryItem::ToolCall(ToolCall {
                call_id: "c2".into(),
                name: "code_file_outline".into(),
                arguments: serde_json::json!({"path": "src/lib.rs"}).to_string(),
            }),
        ]),
        finish_turn(vec![
            HistoryItem::ToolCall(ToolCall {
                call_id: "c3".into(),
                name: "code_definition".into(),
                arguments: serde_json::json!({"path": "src/lib.rs", "line": 2, "column": 17}).to_string(),
            }),
            HistoryItem::ToolCall(ToolCall {
                call_id: "c4".into(),
                name: "code_references".into(),
                arguments: serde_json::json!({"path": "src/lib.rs", "line": 1, "column": 8}).to_string(),
            }),
            HistoryItem::ToolCall(ToolCall {
                call_id: "c5".into(),
                name: "code_diagnostics".into(),
                arguments: serde_json::json!({"path": "src/lib.rs"}).to_string(),
            }),
        ]),
        finish_turn(vec![
            HistoryItem::ToolCall(ToolCall {
                call_id: "c6".into(),
                name: "code_edit_suggestions".into(),
                arguments: serde_json::json!({
                    "path": "src/lib.rs",
                    "line": 1,
                    "column": 8,
                    "new_name": "renamed_fn"
                })
                .to_string(),
            }),
        ]),
        finish_turn(vec![
            HistoryItem::ToolCall(ToolCall {
                call_id: "c7".into(),
                name: "code_edit_suggestions".into(),
                arguments: serde_json::json!({
                    "path": "src/lib.rs",
                    "line": 1,
                    "column": 8,
                    "new_name": "renamed_stale",
                    "expected_file_sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                })
                .to_string(),
            }),
        ]),
        finish_turn(vec![HistoryItem::AssistantText("complete".into())]),
    ]));

    let store = Arc::new(MemoryStore::default());
    let (setup, mut session) = base_setup(
        &ws,
        model.clone(),
        store.clone(),
        WorkMode::Plan,
        PermissionMode::Auto,
        None,
    );
    let rt = AgentRuntime::new(setup)
        .unwrap()
        .with_private_paths(vec![ws.join("secrets")])
        .unwrap()
        .with_code_intel(code_intel_cfg.clone())
        .unwrap();
    let mut rt = rt;
    let res = rt.run(&mut session).await.unwrap();
    assert_eq!(res.status, RunStatus::Completed);

    // Check soyo_code_intel_v1 low-trust header injected and prompt includes code_intel section.
    {
        let reqs = model.seen_requests.lock().unwrap();
        assert!(reqs[0].instructions.contains("code_edit_suggestions"));
        assert!(reqs[0].history.iter().any(|h| matches!(
            h,
            HistoryItem::UserText(t) if t.contains("soyo_code_intel_v1") && t.contains("sha256")
        )));
    }

    // Inspect recorded tool results.
    let results: Vec<(String, ToolResult)> = session
        .history
        .iter()
        .filter_map(|h| match h {
            HistoryItem::ToolResult { call_id, result } => Some((call_id.clone(), result.clone())),
            _ => None,
        })
        .collect();
    let by_id: BTreeMap<String, ToolResult> = results.into_iter().collect();

    // c1: lsp workspace/symbol fallback returned lspOnlySymbol
    let c1 = &by_id["c1"];
    assert!(c1.ok, "{c1:?}");
    assert!(
        c1.data
            .as_ref()
            .unwrap()
            .to_string()
            .contains("lspOnlySymbol")
    );

    // c2: outline has foo & bar
    let c2 = &by_id["c2"];
    assert!(c2.ok, "{c2:?}");
    let c2_str = c2.data.as_ref().unwrap().to_string();
    assert!(c2_str.contains("foo") && c2_str.contains("bar"));

    // c3: definition stripped .git/HEAD and secrets/key.rs, kept src/lib.rs only!
    let c3 = &by_id["c3"];
    assert!(c3.ok, "{c3:?}");
    let c3_locs = c3.data.as_ref().unwrap()["locations"].as_array().unwrap();
    assert_eq!(c3_locs.len(), 1);
    assert_eq!(c3_locs[0]["path"], "src/lib.rs");

    // c4: 2 references
    let c4 = &by_id["c4"];
    assert!(c4.ok, "{c4:?}");
    assert_eq!(
        c4.data.as_ref().unwrap()["locations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // c5: diagnostics returned pull warning
    let c5 = &by_id["c5"];
    assert!(c5.ok, "{c5:?}");
    assert_eq!(c5.data.as_ref().unwrap()["diagnostics"][0]["code"], "E042");

    // c6: unified diff preview generated WITHOUT touching disk!
    let c6 = &by_id["c6"];
    assert!(c6.ok, "{c6:?}");
    let sug = &c6.data.as_ref().unwrap()["suggestions"][0]["files"][0];
    assert_eq!(sug["path"], "src/lib.rs");
    let diff = sug["unified_diff"].as_str().unwrap();
    assert!(
        diff.contains("-pub fn foo()") && diff.contains("+pub fn renamed_fn()"),
        "{diff}"
    );
    // Confirm original workspace file was still untouched after c6 (note c7 later simulates concurrent mutation).
    assert!(
        !fs::read_to_string(ws.join("src/lib.rs"))
            .unwrap()
            .contains("renamed_fn")
    );

    // c7: stale expected_file_sha256 caught -> STALE_INDEX!
    let c7 = &by_id["c7"];
    assert!(!c7.ok, "{c7:?}");
    assert_eq!(c7.error.as_ref().unwrap().code, "STALE_INDEX");

    // Check mock LSP probe output: external env dirs + write isolation + clean shutdown!
    let lsp_home_dir = PathBuf::from(&cache)
        .join("lsp")
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path().join("mock-rs").join("home"))
        .find(|p| p.join("probe.json").exists())
        .expect("sandboxed mock LSP probe.json inside external lsp cache");
    let probe: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(lsp_home_dir.join("probe.json")).unwrap())
            .unwrap();
    let expected_lsp_dir = fs::canonicalize(cache.join("lsp")).unwrap();
    for env_key in ["cargo_target_dir", "xdg_cache_home", "home"] {
        let p = PathBuf::from(probe[env_key].as_str().unwrap());
        assert!(
            p.starts_with(&expected_lsp_dir),
            "{env_key}={p:?} not in {expected_lsp_dir:?}"
        );
    }
    let tmp_p = PathBuf::from(probe["tmpdir"].as_str().unwrap());
    assert!(
        !tmp_p.as_os_str().is_empty() && !tmp_p.starts_with(&ws),
        "tmpdir={tmp_p:?}"
    );
    if backend != BackendKind::Host {
        assert_eq!(probe["ws_write_blocked"].as_bool(), Some(true), "{probe:?}");
        assert_eq!(
            probe["outside_write_blocked"].as_bool(),
            Some(true),
            "{probe:?}"
        );
    }
    assert!(!ws.join("forbidden_lsp_write.txt").exists());
    assert!(!marker_dir.join("forbidden_outside_write.txt").exists());
    assert!(lsp_home_dir.join("shutdown.txt").exists());

    // Permission checks: Manual asks and Deny rejects.
    let deny_model = Arc::new(ScriptModel::new(vec![
        finish_turn(vec![HistoryItem::ToolCall(ToolCall {
            call_id: "m1".into(),
            name: "code_file_outline".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}).to_string(),
        })]),
        finish_turn(vec![HistoryItem::AssistantText("denied".into())]),
    ]));
    let deny_store = Arc::new(MemoryStore::default());
    let (mut manual_setup, mut manual_session) = base_setup(
        &ws,
        deny_model,
        deny_store,
        WorkMode::Work,
        PermissionMode::Manual,
        Some(Arc::new(StaticApprover(Some(false)))),
    );
    manual_setup.limits.model_requests = 4;
    let mut manual_rt = AgentRuntime::new(manual_setup)
        .unwrap()
        .with_code_intel(code_intel_cfg)
        .unwrap();
    let _ = manual_rt.run(&mut manual_session).await.unwrap();
    let denied = manual_session.history.iter().find_map(|h| match h {
        HistoryItem::ToolResult { call_id, result } if call_id == "m1" => Some(result),
        _ => None,
    });
    assert_eq!(
        denied.unwrap().error.as_ref().unwrap().code,
        "permission_denied"
    );
}

#[tokio::test]
async fn write_tool_automatically_invalidates_index_and_syncs_lsp() {
    let Some(python_path) = [
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
        "/Library/Developer/CommandLineTools/usr/bin/python3",
        "/usr/bin/python3",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.exists()) else {
        eprintln!("SKIP write invalidation LSP test: python3 not found");
        return;
    };

    let ws_tmp = TempDir::new().unwrap();
    let cache_tmp = TempDir::new().unwrap();
    let script_tmp = TempDir::new().unwrap();
    let ws = fs::canonicalize(ws_tmp.path()).unwrap();
    let cache = fs::canonicalize(cache_tmp.path()).unwrap();
    let script_dir = fs::canonicalize(script_tmp.path()).unwrap();
    let marker_dir = cache.join("forbidden-dir");
    fs::create_dir_all(&marker_dir).unwrap();
    fs::create_dir_all(ws.join("src")).unwrap();
    let initial_src = "pub fn initial_symbol() -> i32 { 1 }\n";
    fs::write(ws.join("src/lib.rs"), initial_src).unwrap();
    let initial_version = ReadTools::new(&ws, &[], false, ReadLimits::default())
        .unwrap()
        .read_file(Path::new("src/lib.rs"), 1, 10)
        .unwrap()
        .version;

    let mock_script = script_dir.join("mock_lsp.py");
    fs::write(&mock_script, MOCK_LSP_PY).unwrap();
    let mut py_read_roots = vec![script_dir.clone()];
    for homebrew_root in ["/opt/homebrew", "/usr/local/opt", "/usr/local/Cellar"] {
        if Path::new(homebrew_root).exists() {
            py_read_roots.push(PathBuf::from(homebrew_root));
        }
    }

    let mut servers = BTreeMap::new();
    servers.insert(
        "mock-rs".to_owned(),
        LspServerSettings {
            enabled: true,
            argv: vec![
                python_path.to_string_lossy().into_owned(),
                mock_script.to_string_lossy().into_owned(),
            ],
            languages: vec!["rust".into()],
            extensions: vec!["rs".into()],
            root_markers: Vec::new(),
            env: BTreeMap::from([
                ("MOCK_LSP_WS".into(), ws.to_string_lossy().into_owned()),
                (
                    "MOCK_LSP_FORBIDDEN_DIR".into(),
                    marker_dir.to_string_lossy().into_owned(),
                ),
            ]),
            read_roots: py_read_roots,
            initialization_options: None,
        },
    );

    let backend = prepare_test_sandbox_backend();
    let code_intel_cfg = CodeIntelConfig {
        workspace: ws.clone(),
        cache_dir: cache.clone(),
        sandbox_backend: backend,
        settings: CodeIntelSettings {
            enabled: true,
            lsp: LspSettings {
                enabled: true,
                request_timeout_seconds: 10,
                startup_timeout_seconds: 10,
                servers,
                ..LspSettings::default()
            },
            ..CodeIntelSettings::default()
        },
        protected_paths: Vec::new(),
        host_env: Environment::capture(),
    };

    let model = Arc::new(ScriptModel::new(vec![
        finish_turn(vec![HistoryItem::ToolCall(ToolCall {
            call_id: "step1_diag".into(),
            name: "code_diagnostics".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}).to_string(),
        })]),
        finish_turn(vec![HistoryItem::ToolCall(ToolCall {
            call_id: "step2_outline".into(),
            name: "code_file_outline".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}).to_string(),
        })]),
        finish_turn(vec![HistoryItem::ToolCall(ToolCall {
            call_id: "step3_edit".into(),
            name: "edit_file".into(),
            arguments: serde_json::json!({
                "path": "src/lib.rs",
                "old_string": "pub fn initial_symbol() -> i32 { 1 }",
                "new_string": "pub fn initial_symbol() -> i32 { 1 }\npub fn added_after_write() -> i32 { 2 }",
                "expected_version": initial_version
            })
            .to_string(),
        })]),
        finish_turn(vec![
            HistoryItem::ToolCall(ToolCall {
                call_id: "step4_search".into(),
                name: "code_search_symbols".into(),
                arguments: serde_json::json!({"query": "added_after_write"}).to_string(),
            }),
            HistoryItem::ToolCall(ToolCall {
                call_id: "step5_refs".into(),
                name: "code_references".into(),
                arguments: serde_json::json!({"path": "src/lib.rs", "line": 2, "column": 8}).to_string(),
            }),
        ]),
        finish_turn(vec![HistoryItem::AssistantText("done".into())]),
    ]));
    let store = Arc::new(MemoryStore::default());
    let (setup, mut session) = base_setup(
        &ws,
        model,
        store,
        WorkMode::Code,
        PermissionMode::Auto,
        None,
    );
    let mut rt = AgentRuntime::new(setup)
        .unwrap()
        .with_code_intel(code_intel_cfg)
        .unwrap();
    let _ = rt.run(&mut session).await.unwrap();

    let by_id: BTreeMap<String, ToolResult> = session
        .history
        .iter()
        .filter_map(|h| match h {
            HistoryItem::ToolResult { call_id, result } => Some((call_id.clone(), result.clone())),
            _ => None,
        })
        .collect();
    assert!(by_id["step1_diag"].ok, "{:?}", by_id["step1_diag"]);
    assert!(by_id["step2_outline"].ok, "{:?}", by_id["step2_outline"]);
    assert!(by_id["step3_edit"].ok, "{:?}", by_id["step3_edit"]);
    assert!(by_id["step4_search"].ok, "{:?}", by_id["step4_search"]);
    assert_eq!(
        by_id["step4_search"].data.as_ref().unwrap()["matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(by_id["step5_refs"].ok, "{:?}", by_id["step5_refs"]);
    let lsp_home_dir = PathBuf::from(&cache)
        .join("lsp")
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path().join("mock-rs").join("home"))
        .find(|p| p.join("probe.json").exists())
        .expect("probe.json");
    assert!(
        lsp_home_dir.join("did_change.txt").exists(),
        "expected LSP didChange sync after edit_file"
    );
}

#[tokio::test]
async fn real_rust_analyzer_inside_native_sandbox_resolves_definition_and_keeps_workspace_clean() {
    let ra_bin = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .chain(std::iter::once(PathBuf::from("/Users/ariakage/.cargo/bin")))
        .map(|dir| dir.join("rust-analyzer"))
        .find(|p| p.exists());
    let Some(ra_path) = ra_bin else {
        eprintln!("SKIP real rust-analyzer test: binary not found");
        return;
    };

    let ws_tmp = TempDir::new().unwrap();
    let cache_tmp = TempDir::new().unwrap();
    let ws = fs::canonicalize(ws_tmp.path()).unwrap();
    let cache = fs::canonicalize(cache_tmp.path()).unwrap();
    let usable = std::process::Command::new(&ra_path)
        .current_dir(&ws)
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !usable {
        eprintln!(
            "SKIP real rust-analyzer test: {} is not an installed rust-analyzer component",
            ra_path.display()
        );
        return;
    }

    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname = \"ra_probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        ws.join("src/lib.rs"),
        "pub fn add_nums(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn double_num(x: i32) -> i32 {\n    add_nums(x, x)\n}\n",
    )
    .unwrap();

    let mut servers = BTreeMap::new();
    servers.insert(
        "rust-analyzer".into(),
        LspServerSettings {
            enabled: true,
            argv: vec![ra_path.to_string_lossy().into_owned()],
            languages: vec!["rust".into()],
            extensions: vec!["rs".into()],
            root_markers: vec!["Cargo.toml".into()],
            env: BTreeMap::from([("CARGO_NET_OFFLINE".into(), "true".into())]),
            read_roots: Vec::new(),
            initialization_options: Some(serde_json::json!({
                "cargo": { "sysroot": "discover" },
                "checkOnSave": false
            })),
        },
    );

    let backend = prepare_test_sandbox_backend();
    let mut lsp = LspManager::new(
        LspClientConfig {
            workspace: ws.clone(),
            cache_dir: cache.clone(),
            sandbox_backend: backend,
            settings: LspSettings {
                enabled: true,
                request_timeout_seconds: 30,
                startup_timeout_seconds: 30,
                servers,
                ..LspSettings::default()
            },
            protected_paths: Vec::new(),
            host_env: Environment::capture(),
        },
        1024 * 1024,
    );

    let cancel = TestCancel;
    // Wait briefly / retry definition as rust-analyzer indexes sysroot on first startup.
    let mut resolved = Vec::new();
    for attempt in 0..20 {
        match lsp
            .definition(Path::new("src/lib.rs"), 6, 7, None, &cancel)
            .await
        {
            Ok(locs) if !locs.is_empty() => {
                resolved = locs;
                break;
            }
            Ok(_) if attempt < 19 => tokio::time::sleep(Duration::from_millis(500)).await,
            Ok(_) => break,
            Err(err) => {
                lsp.shutdown_all().await;
                panic!("real rust-analyzer definition failed: {err:?}");
            }
        }
    }

    assert!(
        resolved
            .iter()
            .any(|l| l.path == Path::new("src/lib.rs") && l.start_line == 1),
        "expected definition at src/lib.rs:1, got {resolved:?}"
    );

    let refs = lsp
        .references(Path::new("src/lib.rs"), 1, 9, true, None, &cancel)
        .await
        .unwrap();
    assert!(
        refs.iter()
            .any(|l| l.path == Path::new("src/lib.rs") && l.start_line == 6),
        "expected reference at src/lib.rs:6, got {refs:?}"
    );

    lsp.shutdown_all().await;
    // Sandbox guarantee: rust-analyzer & cargo metadata must NOT write `target/` inside workspace!
    assert!(
        !ws.join("target").exists(),
        "rust-analyzer wrote target/ inside workspace!"
    );
}
