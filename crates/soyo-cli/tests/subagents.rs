//! End-to-end CLI tests for `F-09` subagents under the platform native sandbox.

#[path = "../../soyo-core/tests/fixtures/http_support/mod.rs"]
mod fixture;

use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

struct CliSandbox {
    workspace: TempDir,
    state: TempDir,
    config: PathBuf,
}

impl CliSandbox {
    fn new(url: &str, permissions: &str, extra_toml: &str) -> Self {
        let workspace = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let config = state.path().join("config.toml");
        fs::write(
            &config,
            format!(
                r#"schema_version = 1
[defaults]
profile = "test"
permissions = "{permissions}"
[profiles.test]
protocol = "openai-chat-completions"
base_url = "{url}"
model = "fixture"
auth = "none"
context_window_tokens = 128000
stream = false
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

    fn run_json(&self, mode: &str, output_mode: &str, prompt: &str) -> Output {
        self.command()
            .args(["--mode", mode, "--output", output_mode, "run", prompt])
            .output()
            .unwrap()
    }
}

fn init_git(dir: &Path) {
    for args in [
        &["init"][..],
        &["config", "user.email", "test@example.com"][..],
        &["config", "user.name", "Soyo CLI Test"][..],
    ] {
        assert!(
            Command::new("git")
                .current_dir(dir)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(dir.join("README.md"), "seed repo\n").unwrap();
    for args in [&["add", "."][..], &["commit", "-m", "initial"][..]] {
        assert!(
            Command::new("git")
                .current_dir(dir)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
}

fn text_response(text: &str) -> String {
    json!({
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": { "role": "assistant", "content": text }
        }]
    })
    .to_string()
}

fn calls_response(calls: Vec<(&str, &str, Value)>) -> String {
    let tool_calls: Vec<_> = calls
        .into_iter()
        .map(|(id, name, args)| {
            json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": args.to_string(),
                }
            })
        })
        .collect();
    json!({
        "choices": [{
            "index": 0,
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": tool_calls
            }
        }]
    })
    .to_string()
}

fn has_user_message_containing(req: &Value, needle: &str) -> bool {
    req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .any(|text| !text.contains("soyo_runtime_context_v1") && text.contains(needle))
}

fn tool_message_map(req: &Value) -> std::collections::HashMap<String, Value> {
    let mut map = std::collections::HashMap::new();
    for msg in req["messages"].as_array().unwrap() {
        if msg["role"] == "tool"
            && let (Some(id), Some(content)) =
                (msg["tool_call_id"].as_str(), msg["content"].as_str())
            && let Ok(val) = serde_json::from_str::<Value>(content)
        {
            map.insert(id.to_owned(), val);
        }
    }
    map
}

#[test]
fn native_sandbox_cli_runs_parallel_git_worktree_exec_diff_merge_and_cleans_up() {
    let (url, _requests) = fixture::server_dynamic(12, |_, req| {
        let tools_map = tool_message_map(req);

        let body = if has_user_message_containing(req, "WORKER_EXEC") {
            if !tools_map.contains_key("w_exec_1") {
                calls_response(vec![(
                    "w_exec_1",
                    "exec",
                    json!({"argv": ["/bin/sh", "-c", "printf 'from-sandboxed-exec\\n' > exec_out.txt"]}),
                )])
            } else {
                let exit_code = tools_map["w_exec_1"]["data"]["exit_code"]
                    .as_i64()
                    .unwrap_or(-1);
                text_response(&format!("exec exit {exit_code}"))
            }
        } else if has_user_message_containing(req, "WORKER_EDIT") {
            if !tools_map.contains_key("w_edit_1") {
                calls_response(vec![(
                    "w_edit_1",
                    "write_file",
                    json!({"path": "edit_out.txt", "content": "from-child-edit\n", "expected_version": Value::Null}),
                )])
            } else {
                text_response("edited edit_out.txt")
            }
        } else {
            // Parent orchestration turn sequence
            if !tools_map.contains_key("p_spawn_exec") {
                calls_response(vec![
                    (
                        "p_spawn_exec",
                        "subagent_spawn",
                        json!({
                            "label": "exec-worker",
                            "task": "WORKER_EXEC",
                            "mode": "code",
                            "workspace_isolation": "isolated",
                            "allow_exec": true,
                            "background": true
                        }),
                    ),
                    (
                        "p_spawn_edit",
                        "subagent_spawn",
                        json!({
                            "label": "edit-worker",
                            "task": "WORKER_EDIT",
                            "mode": "code",
                            "workspace_isolation": "isolated",
                            "allow_exec": false,
                            "background": true
                        }),
                    ),
                ])
            } else if !tools_map.contains_key("p_wait") {
                let id_a = tools_map["p_spawn_exec"]["data"]["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let id_b = tools_map["p_spawn_edit"]["data"]["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                calls_response(vec![(
                    "p_wait",
                    "subagent_wait",
                    json!({"subagent_ids": [id_a, id_b], "wait_all": true, "timeout_seconds": 30}),
                )])
            } else if !tools_map.contains_key("p_diff_a") {
                let id_a = tools_map["p_spawn_exec"]["data"]["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let id_b = tools_map["p_spawn_edit"]["data"]["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                calls_response(vec![
                    ("p_diff_a", "subagent_diff", json!({"subagent_id": id_a, "include_patch": true})),
                    ("p_diff_b", "subagent_diff", json!({"subagent_id": id_b, "include_patch": true})),
                ])
            } else if !tools_map.contains_key("p_merge_a") {
                let id_a = tools_map["p_spawn_exec"]["data"]["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let id_b = tools_map["p_spawn_edit"]["data"]["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                calls_response(vec![
                    ("p_merge_a", "subagent_merge", json!({"subagent_id": id_a, "discard_after": true})),
                    ("p_merge_b", "subagent_merge", json!({"subagent_id": id_b, "discard_after": true})),
                ])
            } else {
                text_response("parallel subagents merged under native sandbox")
            }
        };
        ("application/json", body)
    })
    .unwrap();

    let sandbox = CliSandbox::new(&url, "bypass", "\n[subagents]\nenabled = true\n");
    init_git(sandbox.workspace.path());

    let output = sandbox.run_json("code", "jsonl", "MAIN_PARENT_RUN");
    assert!(
        output.status.success(),
        "status={:?}\nstderr={}\nstdout={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let last_line = stdout.lines().last().unwrap();
    let final_event: Value = serde_json::from_str(last_line).unwrap();
    assert_eq!(final_event["data"]["status"], "completed");

    assert_eq!(
        fs::read_to_string(sandbox.workspace.path().join("exec_out.txt")).unwrap(),
        "from-sandboxed-exec\n"
    );
    assert_eq!(
        fs::read_to_string(sandbox.workspace.path().join("edit_out.txt")).unwrap(),
        "from-child-edit\n"
    );

    let wt_out = Command::new("git")
        .current_dir(sandbox.workspace.path())
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let wt_text = String::from_utf8_lossy(&wt_out.stdout);
    assert_eq!(wt_text.matches("worktree ").count(), 1, "{wt_text}");
    let worktrees_dir = sandbox.workspace.path().join(".git/worktrees");
    assert!(!worktrees_dir.exists() || fs::read_dir(&worktrees_dir).unwrap().next().is_none());
}

#[test]
fn cli_disabled_subagents_rejects_fabricated_spawn_without_prompt_change() {
    let saw_subagent_prompt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_flag = saw_subagent_prompt.clone();
    let (url, _requests) = fixture::server_dynamic(4, move |turn, req| {
        let system = req["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "system")
            .filter_map(|m| m["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if system.contains("Subagent orchestration") {
            saw_flag.store(true, std::sync::atomic::Ordering::Release);
        }
        let body = if turn == 0 {
            calls_response(vec![(
                "fab_1",
                "subagent_spawn",
                json!({"label": "nope", "task": "should fail"}),
            )])
        } else {
            let map = tool_message_map(req);
            let err_code = map["fab_1"]["error"]["code"].as_str().unwrap_or("");
            text_response(&format!("rejected:{err_code}"))
        };
        ("application/json", body)
    })
    .unwrap();

    let sandbox = CliSandbox::new(&url, "bypass", "");
    let output = sandbox.run_json("work", "json", "try fabricated subagent");
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["text"], "rejected:unknown_tool");
    assert!(!saw_subagent_prompt.load(std::sync::atomic::Ordering::Acquire));
}
