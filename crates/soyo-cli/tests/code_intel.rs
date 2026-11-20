//! CLI integration tests for F-16 (`soyo routing explain`) and F-17 (`soyo index build|query|status`).

use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
};
use tempfile::TempDir;

struct CliSandbox {
    workspace: TempDir,
    state: TempDir,
    config: PathBuf,
}

impl CliSandbox {
    fn new(extra_toml: &str) -> Self {
        let workspace = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let config = state.path().join("config.toml");
        fs::write(
            &config,
            format!(
                r#"schema_version = 1
[defaults]
profile = "primary"
permissions = "auto"

[profiles.primary]
protocol = "openai-responses"
base_url = "https://127.0.0.1:9/v1"
model = "gpt-5.4"
auth = "none"
context_window_tokens = 256000
max_output_tokens = 16000
[profiles.primary.capabilities]
tools = true

[profiles.fast_plan]
protocol = "anthropic-messages"
base_url = "https://127.0.0.1:9"
model = "claude-sonnet-4-6"
auth = "none"
context_window_tokens = 200000
max_output_tokens = 8192
[profiles.fast_plan.capabilities]
tools = true

{extra_toml}
"#
            ),
        )
        .unwrap();
        Self {
            workspace,
            state,
            config,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_soyo"));
        command
            .env_clear()
            .env("HOME", self.state.path())
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_DATA_HOME", self.state.path().join("data"))
            .env("XDG_STATE_HOME", self.state.path().join("state"))
            .env("XDG_CACHE_HOME", self.state.path().join("cache"))
            .current_dir(self.workspace.path())
            .arg("--config")
            .arg(&self.config)
            .stdin(Stdio::null());
        command
    }
}

#[test]
fn cli_routing_explain_reports_candidates_and_capabilities() {
    let sb = CliSandbox::new(
        r#"[routing]
enabled = true
opaque_history = "migrate_compatible"
[routing.roles]
planning = ["fast_plan", "primary"]
"#,
    );

    let out = sb
        .command()
        .args([
            "--output", "json", "routing", "explain", "--role", "planning", "--tools",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(parsed["enabled"], true);
    assert_eq!(parsed["role"], "planning");
    assert_eq!(parsed["selected"], "fast_plan");
}

#[test]
fn cli_index_build_query_status_stores_db_externally_and_excludes_secrets() {
    let sb = CliSandbox::new(
        r#"[code_intel]
enabled = true
"#,
    );

    fs::create_dir_all(sb.workspace.path().join("src")).unwrap();
    fs::create_dir_all(sb.workspace.path().join(".git")).unwrap();
    fs::write(sb.workspace.path().join(".gitignore"), "ignored_dir/\n").unwrap();
    fs::create_dir_all(sb.workspace.path().join("ignored_dir")).unwrap();
    fs::write(
        sb.workspace.path().join("ignored_dir/skip.rs"),
        "pub fn secret_ignored_fn() {}\n",
    )
    .unwrap();
    fs::write(
        sb.workspace.path().join(".git/internal.rs"),
        "pub fn git_secret_fn() {}\n",
    )
    .unwrap();
    fs::write(
        sb.workspace.path().join("src/engine.rs"),
        "pub struct IndexEngine;\npub fn boot_external_index() -> bool { true }\n",
    )
    .unwrap();

    let build = sb
        .command()
        .args(["--output", "json", "index", "build"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    let built: Value = serde_json::from_slice(&build.stdout).unwrap();
    let db_path = PathBuf::from(built["db_path"].as_str().unwrap());
    let cache_dir = fs::canonicalize(sb.state.path().join("cache")).unwrap();
    let canon_ws = fs::canonicalize(sb.workspace.path()).unwrap();
    assert!(
        db_path.starts_with(&cache_dir),
        "{db_path:?} not in {cache_dir:?}"
    );
    assert!(!db_path.starts_with(&canon_ws));
    assert_eq!(built["indexed_files"], 1);

    let query = sb
        .command()
        .args(["--output", "json", "index", "query", "boot_external_index"])
        .output()
        .unwrap();
    assert!(
        query.status.success(),
        "{}",
        String::from_utf8_lossy(&query.stderr)
    );
    let queried: Value = serde_json::from_slice(&query.stdout).unwrap();
    assert_eq!(queried["matches"].as_array().unwrap().len(), 1);
    assert_eq!(queried["matches"][0]["path"], "src/engine.rs");

    let status = sb
        .command()
        .args(["--output", "json", "index", "status"])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let st: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(st["indexed_files"], 1);
}
