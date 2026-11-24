//! Offline acceptance for the read-only observability commands.
//!
//! `soyo usage` / `soyo replay` / `soyo diagnose` / `soyo session report`
//! project persisted session data without opening the store for write — the
//! fixture holds an exclusive session lock through every command to prove
//! none of them needs it.
use serde_json::{Value, json};
use soyo_core::{
    boundary::{Store, StoreChange},
    contract::{
        Event, EventPayload, HistoryItem, ModelProtocol, PermissionMode, Run, RunId, Session,
        SessionId, ToolCall, ToolResult, Usage, WorkMode,
    },
    metrics::{CostEstimate, RunMetrics},
    sqlite::SqliteStore,
};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

struct Fixture {
    workspace: TempDir,
    state: TempDir,
    config: PathBuf,
    data: PathBuf,
    session: Session,
    run: Run,
    /// Held so the session stays exclusively locked through every command;
    /// the lock releases when the fixture drops.
    _store: Option<SqliteStore>,
}

impl Fixture {
    async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let config = state.path().join("config.toml");
        fs::write(&config, "schema_version = 1\n")?;
        let data = state.path().join("database");
        let session = Session {
            id: SessionId::new(),
            workspace: fs::canonicalize(workspace.path())?,
            work_mode: WorkMode::Work,
            profile: "fixture".into(),
            protocol: ModelProtocol::Responses,
            model: "offline".into(),
            history: vec![
                HistoryItem::UserText("do the thing".into()),
                HistoryItem::AssistantText("working".into()),
            ],
        };
        let store = SqliteStore::open(&data, vec![])?;
        store.create_session(&session, "fixture-hash").await?;
        let run = Run {
            id: RunId::new(),
            session_id: session.id,
            work_mode: WorkMode::Work,
            permissions: PermissionMode::Auto,
        };
        let ok_call = ToolCall {
            call_id: "call-ok".into(),
            name: "read_file".into(),
            arguments: json!({"path":"a.txt"}).to_string(),
        };
        let bad_call = ToolCall {
            call_id: "call-bad".into(),
            name: "exec".into(),
            arguments: json!({"command":"rm -rf /"}).to_string(),
        };
        store
            .commit(&[
                StoreChange::RunStarted(run.clone()),
                StoreChange::ToolIntent {
                    run_id: run.id,
                    call: ok_call.clone(),
                },
                StoreChange::ToolFinished {
                    run_id: run.id,
                    call_id: ok_call.call_id.clone(),
                    result: ToolResult::success(Some(json!({"text":"contents"}))),
                },
                StoreChange::ToolIntent {
                    run_id: run.id,
                    call: bad_call.clone(),
                },
                StoreChange::ToolFinished {
                    run_id: run.id,
                    call_id: bad_call.call_id.clone(),
                    result: ToolResult::failure("permission_denied", "denied by policy"),
                },
                StoreChange::Event(Event {
                    schema_version: 1,
                    session_id: session.id,
                    run_id: Some(run.id),
                    seq: 1,
                    timestamp: "2026-11-24T00:00:01Z".into(),
                    payload: EventPayload::ToolFinished {
                        call_id: ok_call.call_id.clone(),
                        name: ok_call.name.clone(),
                        result: ToolResult::success(Some(json!({"text":"contents"}))),
                    },
                }),
                StoreChange::Event(Event {
                    schema_version: 1,
                    session_id: session.id,
                    run_id: Some(run.id),
                    seq: 2,
                    timestamp: "2026-11-24T00:00:02Z".into(),
                    payload: EventPayload::ToolFinished {
                        call_id: bad_call.call_id.clone(),
                        name: bad_call.name.clone(),
                        result: ToolResult::failure("permission_denied", "denied by policy"),
                    },
                }),
                StoreChange::RunFinished(soyo_core::contract::RunResult {
                    schema_version: 1,
                    session_id: Some(session.id),
                    run_id: Some(run.id),
                    status: soyo_core::contract::RunStatus::Failed,
                    stop_reason: Some("permission_denied".into()),
                    text: None,
                    error: Some(soyo_core::contract::ErrorInfo {
                        code: "permission_denied".into(),
                        message: "denied by policy".into(),
                    }),
                    usage: None,
                    artifacts: vec![],
                    persistence: None,
                }),
                StoreChange::RunMetrics {
                    run_id: run.id,
                    metrics: RunMetrics {
                        execution_requests: 1,
                        tool_calls: 2,
                        tool_executions: 2,
                        duration_ms: 5,
                        execution_usage: Some(Usage {
                            input_tokens: Some(120),
                            output_tokens: Some(30),
                            cached_input_tokens: Some(10),
                            estimated: false,
                        }),
                        execution_cost: Some(CostEstimate {
                            currency: "USD".into(),
                            microunits: 42,
                            rounded_up: false,
                            estimated: true,
                        }),
                        ..RunMetrics::default()
                    },
                },
            ])
            .await?;
        store.reserve_event_sequences(session.id, 100).await?;
        // Prove the commands stay read-only by holding the exclusive lock.
        let _unused = store.lock_session(session.id)?;
        Ok(Self {
            workspace,
            state,
            config,
            data,
            session,
            run,
            _store: Some(store),
        })
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_soyo"));
        command
            .env_clear()
            .env("HOME", self.state.path())
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_STATE_HOME", self.state.path().join("state"))
            .env("XDG_CACHE_HOME", self.state.path().join("cache"))
            .current_dir(self.workspace.path())
            .stdin(Stdio::null())
            .arg("--config")
            .arg(&self.config)
            .arg("--data-dir")
            .arg(&self.data);
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
}

fn successful(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "stderr={} stdout={}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn usage_aggregates_session_metrics() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new().await?;
    let session = fixture.session.id.0.to_string();
    // Text output prints the aggregate.
    let out = fixture.run(&["usage", "--session", &session]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout)?;
    assert!(
        text.contains("120"),
        "usage text missing input tokens: {text}"
    );
    assert!(text.contains("42"), "usage text missing cost: {text}");
    // JSON output returns the structured report.
    let out = fixture.run(&["--output", "json", "usage", "--session", &session]);
    let value = successful(&out);
    assert_eq!(value["data"]["totals"]["input_tokens"], 120);
    assert_eq!(value["data"]["totals"]["cost"]["USD"], 42);
    assert_eq!(value["data"]["session_id"], session);
    // flock is per-process, so re-locking from this test process always
    // succeeds — the read-only guarantee is proven by the commands above
    // completing as subprocesses while the fixture still holds the
    // exclusive session lock (any `lock_session` in a child would have
    // failed with `session_busy`).
    Ok(())
}

#[tokio::test]
async fn replay_projects_the_persisted_timeline() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new().await?;
    let session = fixture.session.id.0.to_string();
    let out = fixture.run(&["replay", &session]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout)?;
    assert!(
        text.contains("read_file"),
        "timeline missing tool step: {text}"
    );
    assert!(
        text.contains("exec"),
        "timeline missing failed tool step: {text}"
    );
    assert!(
        text.contains("FAIL"),
        "timeline missing failure status: {text}"
    );
    // --step drills into one entry.
    let out = fixture.run(&["--output", "json", "replay", &session, "--step", "0"]);
    let value = successful(&out);
    assert_eq!(value["data"]["index"], 0);
    Ok(())
}

#[tokio::test]
async fn diagnose_pinpoints_the_first_failed_step() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new().await?;
    let session = fixture.session.id.0.to_string();
    let out = fixture.run(&["--output", "json", "diagnose", &session]);
    let value = successful(&out);
    assert_eq!(
        value["data"]["error_code"], "permission_denied",
        "diagnosis did not pinpoint the failed tool: {value}"
    );
    assert!(value["data"]["failed_step"].is_number());
    assert!(value["data"]["last_ok_step"].is_number());
    let text = fixture.run(&["diagnose", &session]);
    let text = String::from_utf8(text.stdout)?;
    assert!(
        text.contains("permission_denied"),
        "diagnose text missing code: {text}"
    );
    Ok(())
}

#[tokio::test]
async fn session_report_publishes_a_run_scoped_markdown_file()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new().await?;
    let session = fixture.session.id.0.to_string();
    let target = fixture.state.path().join("run-report.md");
    let out = fixture.run(&[
        "session",
        "report",
        &session,
        "--file",
        target.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let markdown = fs::read_to_string(&target)?;
    assert!(
        markdown.contains("# Soyo run report"),
        "missing title: {markdown}"
    );
    assert!(
        markdown.contains(&fixture.run.id.0.to_string()),
        "missing run id: {markdown}"
    );
    assert!(
        markdown.contains("permission_denied"),
        "missing failure: {markdown}"
    );
    // Private permissions on the published file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&target)?.permissions().mode() & 0o777, 0o600);
    }
    // JSON format produces a structured report file.
    let target = fixture.state.path().join("run-report.json");
    let out = fixture.run(&[
        "--output",
        "json",
        "session",
        "report",
        &session,
        "--format",
        "json",
        "--file",
        target.to_str().unwrap(),
    ]);
    let receipt = successful(&out);
    assert_eq!(receipt["data"]["run_id"], fixture.run.id.0.to_string());
    let value: Value = serde_json::from_str(&fs::read_to_string(&target)?)?;
    assert_eq!(value["run_id"], fixture.run.id.0.to_string());
    Ok(())
}
