//! End-to-end test for `soyo remote-worker` (`F-13` D-113 path (c), `D-117`):
//! a real helper over authenticated HTTPS, a real worker process, a synthetic
//! model endpoint, and the explicit apply back into the owning workspace.

#[path = "../../soyo-core/tests/fixtures/http_support/mod.rs"]
mod fixture;

use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn response(text: &str) -> String {
    serde_json::to_string(&json!({
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": { "role": "assistant", "content": text }
        }]
    }))
    .unwrap()
}

fn call_response() -> String {
    serde_json::to_string(&json!({
        "choices": [{
            "index": 0,
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "write_file",
                        "arguments": json!({
                            "path": "result.txt",
                            "content": "done by worker\n",
                            "expected_version": null
                        }).to_string()
                    }
                }]
            }
        }]
    }))
    .unwrap()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures/remote-tls")
        .join(name)
}

fn profile(url: &str) -> String {
    format!(
        "[defaults]\nprofile = \"test\"\nmode = \"code\"\npermissions = \"bypass\"\n\
         [profiles.test]\nprotocol = \"openai-chat-completions\"\nbase_url = \"{url}\"\n\
         model = \"fixture\"\nauth = \"none\"\ncontext_window_tokens = 64000\nstream = false\n"
    )
}

struct Sandbox {
    root: TempDir,
    owner_workspace: PathBuf,
    config: PathBuf,
    data: PathBuf,
    helper_dir: PathBuf,
}

impl Sandbox {
    fn new(model_url: &str, port: u16) -> Self {
        let root = TempDir::new().unwrap();
        let owner_workspace = root.path().join("owner");
        fs::create_dir(&owner_workspace).unwrap();
        fs::write(owner_workspace.join("input.txt"), "seed\n").unwrap();
        let config = root.path().join("config.toml");
        fs::write(
            &config,
            format!(
                "schema_version = 1\n{}[tasks]\nenabled = true\n\
                 [remote]\nenabled = true\nbind = \"127.0.0.1:{port}\"\ntoken_ttl_days = 30\n\
                 [remote.tls]\ncert_path = \"{}\"\nkey_path = \"{}\"\n",
                profile(model_url),
                fixture("chain.pem").display(),
                fixture("server-key.pem").display()
            ),
        )
        .unwrap();
        let data = root.path().join("owner-data");
        let helper_dir = root.path().join("helper");
        // SOYO_HELPER_DIR suppresses the helper's usual state-dir creation;
        // prepare the XDG layout the credential store expects.
        for dir in [
            root.path().join("xdg-state/soyo"),
            root.path().join("xdg-data/soyo"),
            root.path().join("xdg-cache/soyo"),
        ] {
            fs::create_dir_all(&dir).unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self {
            root,
            owner_workspace,
            config,
            data,
            helper_dir,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_soyo"));
        self.apply_env(&mut command);
        command
            .arg("--config")
            .arg(&self.config)
            .arg("--data-dir")
            .arg(&self.data)
            .arg("--output")
            .arg("json")
            .stdin(Stdio::null());
        command
    }

    fn apply_env(&self, command: &mut Command) {
        command
            .env_clear()
            .env("HOME", self.root.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("XDG_DATA_HOME", self.root.path().join("xdg-data"))
            .env("XDG_STATE_HOME", self.root.path().join("xdg-state"))
            .env("XDG_CACHE_HOME", self.root.path().join("xdg-cache"))
            .env("SOYO_HELPER_DIR", &self.helper_dir);
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .current_dir(&self.owner_workspace)
            .args(args)
            .output()
            .unwrap()
    }

    fn run_json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "args={args:?} stderr={} stdout={}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
            panic!(
                "args={args:?} is not JSON ({err}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }
}

struct KillOnDrop(Option<Child>, Vec<Child>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        for child in self.0.iter_mut().chain(self.1.iter_mut()) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn remote_worker_executes_and_applies_a_dispatched_task() {
    let (model_url, _requests) = fixture::server(vec![
        ("application/json", call_response()),
        ("application/json", response("done")),
        ("application/json", response("done")),
    ])
    .expect("model fixture");
    let port = free_port();
    let sandbox = Sandbox::new(&model_url, port);

    let helper = sandbox
        .command()
        .arg("helper")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir(sandbox.root.path())
        .spawn()
        .expect("spawn helper");
    let mut children = KillOnDrop(Some(helper), Vec::new());

    // Wait for the helper to answer over its Unix socket.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let output = sandbox.run(&["remote", "workers"]);
        if output.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "helper did not start: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    let issued = sandbox.run_json(&["credentials", "token", "issue"]);
    let token = issued["data"]["token"]
        .as_str()
        .expect("issued token")
        .to_owned();
    let submitted = sandbox.run_json(&["task", "submit", "write the result file"]);
    let task_id = submitted["data"]["task"]["id"]
        .as_str()
        .expect("task id")
        .to_owned();
    sandbox.run_json(&["remote", "assign", &task_id, "build-01"]);

    let worker_config = sandbox.root.path().join("worker.toml");
    fs::write(
        &worker_config,
        format!("schema_version = 1\n{}", profile(&model_url)),
    )
    .unwrap();
    let worker_workspace = sandbox.root.path().join("worker-ws");
    let worker_log = fs::File::create(sandbox.root.path().join("worker.log")).unwrap();
    let mut worker_command = Command::new(env!("CARGO_BIN_EXE_soyo"));
    sandbox.apply_env(&mut worker_command);
    let worker = worker_command
        .arg("--config")
        .arg(&worker_config)
        .arg("--data-dir")
        .arg(sandbox.root.path().join("worker-data"))
        .args(["remote-worker", "--endpoint"])
        .arg(format!("https://127.0.0.1:{port}"))
        .arg("--workspace")
        .arg(&worker_workspace)
        .args(["--worker-id", "build-01", "--poll-seconds", "1"])
        .env("SOYO_REMOTE_TOKEN", &token)
        .env("SOYO_REMOTE_CA", fixture("ca.pem"))
        .current_dir(sandbox.root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(worker_log))
        .spawn()
        .expect("spawn worker");
    children.1.push(worker);

    let deadline = Instant::now() + Duration::from_secs(90);
    let show = loop {
        let output = sandbox.run(&["remote", "show", &task_id]);
        if output.status.success()
            && let Ok(show) = serde_json::from_slice::<Value>(&output.stdout)
            && show["data"]["task"]["status"] == "completed"
        {
            break show;
        }
        if Instant::now() > deadline {
            let log =
                fs::read_to_string(sandbox.root.path().join("worker.log")).unwrap_or_default();
            panic!(
                "task did not complete; last show: {}\nworker log:\n{log}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    assert_eq!(
        show["data"]["result"]["changed_files"][0]["path"],
        "result.txt"
    );
    // Nothing is written to the owning workspace before the explicit apply.
    assert!(!sandbox.owner_workspace.join("result.txt").exists());

    let applied = sandbox.run(&["remote", "apply", &task_id]);
    assert!(
        applied.status.success(),
        "apply failed: {} {}",
        String::from_utf8_lossy(&applied.stderr),
        String::from_utf8_lossy(&applied.stdout)
    );
    assert_eq!(
        fs::read(sandbox.owner_workspace.join("result.txt")).unwrap(),
        b"done by worker\n"
    );

    let _ = sandbox.run(&["kill"]);
    for child in children.0.iter_mut().chain(children.1.iter_mut()) {
        let _ = child.kill();
        let _ = child.wait();
    }
    children.0 = None;
    children.1.clear();
}
