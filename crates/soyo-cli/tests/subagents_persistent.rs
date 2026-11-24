//! End-to-end CLI tests for persistent subagents (`F-09`, `D-110`).

#[path = "../../soyo-core/tests/fixtures/http_support/mod.rs"]
mod fixture;

use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct CliSandbox {
    workspace: TempDir,
    state: TempDir,
    config: PathBuf,
}

impl CliSandbox {
    fn new(url: &str) -> Self {
        let workspace = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let config = state.path().join("config.toml");
        fs::write(
            &config,
            format!(
                r#"schema_version = 1
[defaults]
profile = "test"
mode = "code"
permissions = "bypass"
[profiles.test]
protocol = "openai-chat-completions"
base_url = "{url}"
model = "fixture"
auth = "none"
context_window_tokens = 128000
stream = false
[subagents]
enabled = true
[tasks]
enabled = true
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

    fn run_json(&self, prompt: &str) -> Value {
        let output = self
            .command()
            .args(["--output", "json", "run", prompt])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stderr={} stdout={}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn subagent(&self, args: &[&str]) -> Output {
        let mut command = self.command();
        command.args(["--output", "json", "subagent"]);
        command.args(args);
        command.output().unwrap()
    }

    fn data_dir(&self) -> PathBuf {
        self.state.path().join("data/soyo")
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
                "function": { "name": name, "arguments": args.to_string() }
            })
        })
        .collect();
    json!({
        "choices": [{
            "index": 0,
            "finish_reason": "tool_calls",
            "message": { "role": "assistant", "content": null, "tool_calls": tool_calls }
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
            && let Ok(value) = serde_json::from_str::<Value>(content)
        {
            map.insert(id.to_owned(), value);
        }
    }
    map
}

fn persistent_spawn_body(tools: &std::collections::HashMap<String, Value>) -> String {
    if !tools.contains_key("p_spawn") {
        calls_response(vec![(
            "p_spawn",
            "subagent_spawn",
            json!({
                "label": "persistent-worker",
                "task": "PERSISTENT_WORKER edit persist_out.txt",
                "mode": "code",
                "workspace_isolation": "isolated",
                "persistent": true,
                "background": true
            }),
        )])
    } else {
        let id = tools["p_spawn"]["data"]["id"].as_str().unwrap();
        text_response(&format!("spawned:{id}"))
    }
}

fn parse_spawned_id(result: &Value) -> String {
    result["text"]
        .as_str()
        .unwrap()
        .strip_prefix("spawned:")
        .unwrap()
        .to_owned()
}

#[test]
fn persistent_subagent_survives_parent_run_and_merges() {
    let job_id = Arc::new(std::sync::Mutex::new(String::new()));
    let shared_id = job_id.clone();
    let (url, _requests) = fixture::server_dynamic(8, move |_, req| {
        let tools = tool_message_map(req);
        let body = if has_user_message_containing(req, "PERSISTENT_WORKER") {
            if !tools.contains_key("w_write") {
                calls_response(vec![(
                    "w_write",
                    "write_file",
                    json!({
                        "path": "persist_out.txt",
                        "content": "from-persistent-child\n",
                        "expected_version": Value::Null
                    }),
                )])
            } else {
                text_response("persistent child completed")
            }
        } else if has_user_message_containing(req, "MERGE_PERSISTENT") {
            if !tools.contains_key("p_merge") {
                let id = shared_id.lock().unwrap().clone();
                calls_response(vec![(
                    "p_merge",
                    "subagent_merge",
                    json!({"subagent_id": id, "discard_after": true}),
                )])
            } else {
                text_response("merged by parent")
            }
        } else {
            persistent_spawn_body(&tools)
        };
        ("application/json", body)
    })
    .unwrap();

    let sandbox = CliSandbox::new(&url);
    init_git(sandbox.workspace.path());

    let parent = sandbox.run_json("MAIN_PERSISTENT");
    assert_eq!(parent["status"], "completed");
    let id = parse_spawned_id(&parent);
    *job_id.lock().unwrap() = id.clone();
    let parent_session = parent["session_id"].as_str().unwrap().to_owned();

    let list = sandbox.subagent(&["list"]);
    assert!(
        list.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list: Value = serde_json::from_slice(&list.stdout).unwrap();
    let jobs = list["data"]["subagents"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["id"], id);
    assert_eq!(jobs[0]["persistent"], true);

    let continued = sandbox.subagent(&["continue", &id]);
    assert!(
        continued.status.success(),
        "stderr={} stdout={}",
        String::from_utf8_lossy(&continued.stderr),
        String::from_utf8_lossy(&continued.stdout)
    );
    let continued: Value = serde_json::from_slice(&continued.stdout).unwrap();
    assert_eq!(continued["data"]["task"]["status"], "completed");
    assert_eq!(continued["data"]["run"]["status"], "completed");

    let status = sandbox.subagent(&["status", &id]);
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["data"]["subagent"]["state"], "completed");
    assert_eq!(
        status["data"]["subagent"]["summary"],
        "persistent child completed"
    );
    assert!(
        status["data"]["subagent"]["changed_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["path"] == "persist_out.txt")
    );

    // The parent session resumes after its run ended and merges the persistent
    // child through the model-facing tool path.
    let merged = sandbox
        .command()
        .args([
            "--output",
            "json",
            "run",
            "--session",
            &parent_session,
            "MERGE_PERSISTENT",
        ])
        .output()
        .unwrap();
    assert!(
        merged.status.success(),
        "stderr={} stdout={}",
        String::from_utf8_lossy(&merged.stderr),
        String::from_utf8_lossy(&merged.stdout)
    );
    let merged: Value = serde_json::from_slice(&merged.stdout).unwrap();
    assert_eq!(merged["text"], "merged by parent");
    assert_eq!(
        fs::read_to_string(sandbox.workspace.path().join("persist_out.txt")).unwrap(),
        "from-persistent-child\n"
    );
    let status = sandbox.subagent(&["status", &id]);
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["data"]["subagent"]["state"], "merged");
    assert!(!sandbox.data_dir().join("subagents").join(&id).exists());

    // Completed jobs are never replayed.
    let replay = sandbox.subagent(&["continue", &id]);
    assert!(!replay.status.success());
}

#[test]
fn persistent_merge_conflict_against_moved_parent_fails_closed() {
    let (url, _requests) = fixture::server_dynamic(8, |_, req| {
        let tools = tool_message_map(req);
        let body = if has_user_message_containing(req, "PERSISTENT_WORKER") {
            if !tools.contains_key("w_write") {
                calls_response(vec![(
                    "w_write",
                    "write_file",
                    json!({
                        "path": "persist_out.txt",
                        "content": "from-persistent-child\n",
                        "expected_version": Value::Null
                    }),
                )])
            } else {
                text_response("persistent child completed")
            }
        } else {
            persistent_spawn_body(&tools)
        };
        ("application/json", body)
    })
    .unwrap();

    let sandbox = CliSandbox::new(&url);
    init_git(sandbox.workspace.path());
    let parent = sandbox.run_json("MAIN_PERSISTENT");
    let id = parse_spawned_id(&parent);

    let continued = sandbox.subagent(&["continue", &id]);
    assert!(continued.status.success());

    // The parent moved after the child forked from (and completed against)
    // the old snapshot.
    fs::write(
        sandbox.workspace.path().join("persist_out.txt"),
        "parent changed this\n",
    )
    .unwrap();
    let merge = sandbox.subagent(&["merge", &id]);
    assert!(!merge.status.success());
    let merge: Value = serde_json::from_slice(&merge.stdout).unwrap();
    let conflicts = merge["data"]["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["path"], "persist_out.txt");
    assert_eq!(
        fs::read_to_string(sandbox.workspace.path().join("persist_out.txt")).unwrap(),
        "parent changed this\n"
    );
}

#[test]
fn cancelling_a_running_persistent_job_stops_the_worker() {
    let reached = Arc::new(AtomicBool::new(false));
    let reached_flag = reached.clone();
    let (url, _requests) = fixture::server_dynamic(4, move |_, req| {
        let tools = tool_message_map(req);
        let body = if has_user_message_containing(req, "PERSISTENT_WORKER") {
            reached_flag.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_secs(30));
            text_response("late persistent child completion")
        } else {
            persistent_spawn_body(&tools)
        };
        ("application/json", body)
    })
    .unwrap();

    let sandbox = CliSandbox::new(&url);
    init_git(sandbox.workspace.path());
    let parent = sandbox.run_json("MAIN_PERSISTENT");
    let id = parse_spawned_id(&parent);

    let worker = sandbox
        .command()
        .args(["--output", "json", "subagent", "continue", &id])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !reached.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "worker never reached the model");
        std::thread::sleep(Duration::from_millis(50));
    }

    let started = Instant::now();
    let cancel = sandbox.subagent(&["cancel", &id]);
    assert!(
        cancel.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&cancel.stderr)
    );
    let output = worker.wait_with_output().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "worker ignored cancellation"
    );
    assert!(!output.status.success());

    let status = sandbox.subagent(&["status", &id]);
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["data"]["subagent"]["state"], "cancelled");
}

#[test]
fn cancelling_a_queued_persistent_job_prevents_execution() {
    let (url, _requests) = fixture::server_dynamic(4, |_, req| {
        let tools = tool_message_map(req);
        ("application/json", persistent_spawn_body(&tools))
    })
    .unwrap();

    let sandbox = CliSandbox::new(&url);
    init_git(sandbox.workspace.path());
    let parent = sandbox.run_json("MAIN_PERSISTENT");
    let id = parse_spawned_id(&parent);

    let cancel = sandbox.subagent(&["cancel", &id, "--discard"]);
    assert!(
        cancel.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&cancel.stderr)
    );
    let cancel: Value = serde_json::from_slice(&cancel.stdout).unwrap();
    assert_eq!(cancel["data"]["subagent"]["state"], "discarded");

    let continued = sandbox.subagent(&["continue", &id]);
    assert!(!continued.status.success());
}
