//! Runtime integration tests for F-09 controlled parent-child subagents.

use serde_json::{Value, json};
use soyo_core::{
    approvals::Preauthorizations,
    boundary::{
        Cancellation, Model, ModelRequest, ModelStream, ModelUpdate, Operation, Store, StoreChange,
    },
    contract::{
        ApprovalRequest, BoundaryError, HistoryItem, ModelProtocol, PermissionMode, Run, RunId,
        RunStatus, Session, SessionId, ToolCall, ToolResult, Usage, WorkMode, error_codes,
    },
    credentials::SecretRegistry,
    memory::MemoryStore,
    metrics::RunMetrics,
    permissions::{PermissionConfig, Policy},
    review::ReviewConfig,
    runtime::{AgentRuntime, ApprovalHandler, RuntimeLimits, RuntimeSetup},
    subagent::SubagentSettings,
    tools::{
        exec::ExecConfig,
        read::{ReadLimits, ReadTools},
    },
};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    future::{Future, pending},
    path::Path,
    pin::Pin,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tempfile::tempdir;
use tokio::sync::Notify;

struct NeverCancel;
impl Cancellation for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(pending())
    }
}

struct FlagCancel {
    cancelled: AtomicBool,
    notify: Notify,
}
impl FlagCancel {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }
    fn trigger(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}
impl Cancellation for FlagCancel {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            loop {
                if self.is_cancelled() {
                    return;
                }
                let notified = self.notify.notified();
                if self.is_cancelled() {
                    return;
                }
                notified.await;
            }
        })
    }
}

struct VecStream(VecDeque<ModelUpdate>);
impl ModelStream for VecStream {
    fn next<'a>(&'a mut self, cancel: &'a dyn Cancellation) -> Operation<'a, Option<ModelUpdate>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(BoundaryError::new(error_codes::CANCELLED, "cancelled"));
            }
            Ok(self.0.pop_front())
        })
    }
}

struct RecordingStore {
    inner: MemoryStore,
    changes: Mutex<Vec<StoreChange>>,
}
impl RecordingStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::default(),
            changes: Mutex::new(Vec::new()),
        }
    }
    fn recorded_metrics(&self) -> Option<RunMetrics> {
        self.changes.lock().unwrap().iter().rev().find_map(|c| {
            if let StoreChange::RunMetrics { metrics, .. } = c {
                Some(metrics.clone())
            } else {
                None
            }
        })
    }
}
impl Store for RecordingStore {
    fn load_session(&self, id: SessionId) -> Operation<'_, Option<Session>> {
        self.inner.load_session(id)
    }
    fn commit<'a>(&'a self, changes: &'a [StoreChange]) -> Operation<'a, ()> {
        Box::pin(async move {
            self.inner.commit(changes).await?;
            self.changes.lock().unwrap().extend(changes.iter().cloned());
            Ok(())
        })
    }
}

struct RecordingApproval {
    approve: bool,
    seen: Mutex<Vec<ApprovalRequest>>,
}
impl RecordingApproval {
    fn allow_all() -> Self {
        Self {
            approve: true,
            seen: Mutex::new(Vec::new()),
        }
    }
    fn summaries(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|req| req.operation_summary.clone())
            .collect()
    }
}
impl ApprovalHandler for RecordingApproval {
    fn confirm<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        _cancel: &'a dyn Cancellation,
    ) -> Operation<'a, Option<bool>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(request.clone());
            Ok(Some(self.approve))
        })
    }
}

type TurnHandler =
    Box<dyn Fn(&ModelRequest, usize) -> Result<Vec<ModelUpdate>, BoundaryError> + Send + Sync>;

struct RouterModel {
    handler: TurnHandler,
    counts: Mutex<HashMap<String, usize>>,
    total_calls: AtomicUsize,
}
impl RouterModel {
    fn new(
        handler: impl Fn(&ModelRequest, usize) -> Result<Vec<ModelUpdate>, BoundaryError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            handler: Box::new(handler),
            counts: Mutex::new(HashMap::new()),
            total_calls: AtomicUsize::new(0),
        }
    }
}
fn first_user_task(history: &[HistoryItem]) -> &str {
    history
        .iter()
        .find_map(|item| match item {
            HistoryItem::UserText(text) if !text.contains("soyo_runtime_context_v1") => {
                Some(text.as_str())
            }
            _ => None,
        })
        .unwrap_or("")
}

impl Model for RouterModel {
    fn start<'a>(
        &'a self,
        request: &'a ModelRequest,
        cancel: &'a dyn Cancellation,
    ) -> Operation<'a, Box<dyn ModelStream>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(BoundaryError::new(error_codes::CANCELLED, "cancelled"));
            }
            self.total_calls.fetch_add(1, Ordering::AcqRel);
            let key = first_user_task(&request.history).to_owned();
            let turn = {
                let mut map = self.counts.lock().unwrap();
                let entry = map.entry(key).or_insert(0);
                let current = *entry;
                *entry += 1;
                current
            };
            let updates = (self.handler)(request, turn)?;
            Ok(Box::new(VecStream(updates.into())) as Box<dyn ModelStream>)
        })
    }
}

fn version_of(bytes: &[u8]) -> &'static str {
    match bytes {
        b"base-a\n" => "264050fd065aaf6e4dcce0580794bb2275cb23429e3fbc54b81309b1ea65aca5",
        b"dirty-b\n" => "6c411f683746fbc4c7341966911ca131913f5fda4ddde746cf4b9ca80ea7360a",
        b"base-1\n" => "6cb61606b89b97a092f09f3b2c3fe5e0a72b25c01d3c6a889055ad01fcb70ccd",
        b"base-2\n" => "c55c20764575e0e37e1243f2ac0685bc1c0cf28b17602ecab9cbbe78da725cc8",
        b"initial" => "ac1b5c0961a7269b6a053ee64276ed0e20a7f48aefb9f67519539d23aaf10149",
        other => panic!("unregistered test version input: {other:?}"),
    }
}

fn finish_tool_calls(
    calls: Vec<(&str, &str, Value)>,
    in_tok: u64,
    out_tok: u64,
) -> Vec<ModelUpdate> {
    let items = calls
        .into_iter()
        .map(|(id, name, args)| {
            HistoryItem::ToolCall(ToolCall {
                call_id: id.into(),
                name: name.into(),
                arguments: args.to_string(),
            })
        })
        .collect();
    vec![ModelUpdate::Finished {
        items,
        usage: Some(Usage {
            input_tokens: Some(in_tok),
            output_tokens: Some(out_tok),
            cached_input_tokens: None,
            estimated: false,
        }),
    }]
}

fn finish_text(text: &str, in_tok: u64, out_tok: u64) -> Vec<ModelUpdate> {
    vec![
        ModelUpdate::TextDelta(text.into()),
        ModelUpdate::Finished {
            items: vec![HistoryItem::AssistantText(text.into())],
            usage: Some(Usage {
                input_tokens: Some(in_tok),
                output_tokens: Some(out_tok),
                cached_input_tokens: None,
                estimated: false,
            }),
        },
    ]
}

fn find_tool_result<'a>(session: &'a Session, call_id: &str) -> &'a ToolResult {
    session
        .history
        .iter()
        .find_map(|item| match item {
            HistoryItem::ToolResult {
                call_id: id,
                result,
            } if id == call_id => Some(result),
            _ => None,
        })
        .expect("missing tool result in session history")
}

fn build_session(workspace: &Path, mode: WorkMode, task: &str) -> Session {
    Session {
        id: SessionId::new(),
        workspace: workspace.to_path_buf(),
        work_mode: mode,
        profile: "test".into(),
        protocol: ModelProtocol::ChatCompletions,
        model: "fixture".into(),
        history: vec![HistoryItem::UserText(task.into())],
    }
}

struct TestRunSpec<'a> {
    workspace: &'a Path,
    mode: WorkMode,
    permissions: PermissionMode,
    perm_cfg: PermissionConfig,
    model: Arc<dyn Model>,
    store: Arc<dyn Store>,
    cancel: Arc<dyn Cancellation>,
    confirmation: Option<Arc<dyn ApprovalHandler>>,
    settings: SubagentSettings,
    limits: RuntimeLimits,
    known_secrets: Vec<String>,
    task: &'a str,
}

fn run_with_session(spec: TestRunSpec<'_>) -> (AgentRuntime, Session) {
    let session = build_session(spec.workspace, spec.mode, spec.task);
    let policy = Arc::new(Policy::new(spec.workspace, spec.perm_cfg).unwrap());
    let files =
        Arc::new(ReadTools::new(spec.workspace, &[], false, ReadLimits::default()).unwrap());
    let run = Run {
        id: RunId::new(),
        session_id: session.id,
        work_mode: spec.mode,
        permissions: spec.permissions,
    };
    let setup = RuntimeSetup {
        run,
        model: spec.model,
        store: spec.store,
        policy,
        files,
        cancellation: spec.cancel,
        review: ReviewConfig::new("test".into()),
        exec: ExecConfig::default(),
        preauthorizations: Preauthorizations::default(),
        known_secrets: spec.known_secrets,
        summary: "subagent integration test".into(),
        confirmation: spec.confirmation,
        limits: spec.limits,
    };
    let runtime = if spec.mode == WorkMode::Plan {
        AgentRuntime::new_read_only(setup).unwrap()
    } else {
        AgentRuntime::new(setup).unwrap()
    };
    let runtime = runtime
        .with_subagents(spec.settings.resolve().unwrap())
        .unwrap();
    (runtime, session)
}

fn init_git(dir: &Path) {
    for args in [
        &["init"][..],
        &["config", "user.email", "test@example.com"][..],
        &["config", "user.name", "Soyo Test"][..],
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
    fs::write(dir.join("a.txt"), "base-a\n").unwrap();
    fs::write(dir.join("b.txt"), "base-b\n").unwrap();
    for args in [&["add", "."][..], &["commit", "-m", "init"][..]] {
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

#[tokio::test]
async fn parallel_git_worktree_edits_diff_merge_and_teardown_cleanup() {
    let dir = tempdir().unwrap();
    init_git(dir.path());
    // Uncommitted dirty edit in parent must propagate into worktree base.
    fs::write(dir.path().join("b.txt"), "dirty-b\n").unwrap();

    let alpha_id = Arc::new(Mutex::new(String::new()));
    let beta_id = Arc::new(Mutex::new(String::new()));
    let alpha_id_m = alpha_id.clone();
    let beta_id_m = beta_id.clone();

    let model = Arc::new(RouterModel::new(move |req, turn| {
        let first = first_user_task(&req.history);
        if first == "TASK_ALPHA" {
            return match turn {
                0 => Ok(finish_tool_calls(
                    vec![(
                        "a1",
                        "write_file",
                        json!({
                            "path": "a.txt",
                            "content": "alpha-a\n",
                            "expected_version": version_of(b"base-a\n")
                        }),
                    )],
                    10,
                    5,
                )),
                _ => Ok(finish_text("alpha updated a.txt", 10, 5)),
            };
        }
        if first == "TASK_BETA" {
            return match turn {
                0 => Ok(finish_tool_calls(
                    vec![(
                        "b1",
                        "write_file",
                        json!({
                            "path": "b.txt",
                            "content": "beta-b\n",
                            "expected_version": version_of(b"dirty-b\n")
                        }),
                    )],
                    12,
                    6,
                )),
                _ => Ok(finish_text("beta updated b.txt", 12, 6)),
            };
        }

        match turn {
            0 => Ok(finish_tool_calls(
                vec![
                    (
                        "p1",
                        "subagent_spawn",
                        json!({
                            "label": "alpha",
                            "task": "TASK_ALPHA",
                            "mode": "code",
                            "workspace_isolation": "isolated",
                            "background": true
                        }),
                    ),
                    (
                        "p2",
                        "subagent_spawn",
                        json!({
                            "label": "beta",
                            "task": "TASK_BETA",
                            "mode": "code",
                            "workspace_isolation": "isolated",
                            "background": true
                        }),
                    ),
                ],
                20,
                10,
            )),
            1 => {
                // Capture IDs from ToolResult items in history
                let mut ids = Vec::new();
                for item in &req.history {
                    if let HistoryItem::ToolResult { call_id, result } = item
                        && (call_id == "p1" || call_id == "p2")
                    {
                        ids.push(
                            result.data.as_ref().unwrap()["id"]
                                .as_str()
                                .unwrap()
                                .to_owned(),
                        );
                    }
                }
                *alpha_id_m.lock().unwrap() = ids[0].clone();
                *beta_id_m.lock().unwrap() = ids[1].clone();
                Ok(finish_tool_calls(
                    vec![(
                        "p3",
                        "subagent_wait",
                        json!({"subagent_ids": ids, "wait_all": true, "timeout_seconds": 30}),
                    )],
                    20,
                    10,
                ))
            }
            2 => {
                let a = alpha_id_m.lock().unwrap().clone();
                let b = beta_id_m.lock().unwrap().clone();
                Ok(finish_tool_calls(
                    vec![
                        (
                            "p4",
                            "subagent_diff",
                            json!({"subagent_id": a, "include_patch": true}),
                        ),
                        (
                            "p5",
                            "subagent_diff",
                            json!({"subagent_id": b, "include_patch": true}),
                        ),
                    ],
                    20,
                    10,
                ))
            }
            3 => {
                let a = alpha_id_m.lock().unwrap().clone();
                let b = beta_id_m.lock().unwrap().clone();
                Ok(finish_tool_calls(
                    vec![
                        (
                            "p6",
                            "subagent_merge",
                            json!({"subagent_id": a, "discard_after": true}),
                        ),
                        (
                            "p7",
                            "subagent_merge",
                            json!({"subagent_id": b, "discard_after": true}),
                        ),
                    ],
                    20,
                    10,
                ))
            }
            _ => Ok(finish_text("both subagents merged", 20, 10)),
        }
    }));

    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        ..Default::default()
    };
    let (mut runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Code,
        permissions: PermissionMode::Bypass,
        perm_cfg: PermissionConfig::default(),
        model,
        store: store.clone(),
        cancel: Arc::new(NeverCancel),
        confirmation: None,
        settings,
        limits: RuntimeLimits::default(),
        known_secrets: Vec::new(),
        task: "PARENT_MAIN",
    });

    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha-a\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("b.txt")).unwrap(),
        "beta-b\n"
    );

    let diff_a = find_tool_result(&session, "p4");
    assert!(diff_a.ok);
    assert_eq!(
        diff_a.data.as_ref().unwrap()["isolation_kind"],
        "git_worktree"
    );
    assert!(
        diff_a.data.as_ref().unwrap()["patch"]
            .as_str()
            .unwrap()
            .contains("+alpha-a")
    );

    // Git worktrees must be completely cleaned up after discard_after + shutdown.
    let wt_out = Command::new("git")
        .current_dir(dir.path())
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let wt_text = String::from_utf8_lossy(&wt_out.stdout);
    assert_eq!(wt_text.matches("worktree ").count(), 1, "{wt_text}");

    // Artifacts were recorded on parent RunMetrics / RunResult.
    assert!(
        result
            .artifacts
            .iter()
            .any(|a| a.path == Path::new("a.txt"))
    );
    assert!(
        result
            .artifacts
            .iter()
            .any(|a| a.path == Path::new("b.txt"))
    );
    let _ = store.recorded_metrics().unwrap();
}

#[tokio::test]
async fn directory_copy_three_way_conflict_selective_merge_and_discard() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f1.txt"), "base-1\n").unwrap();
    fs::write(dir.path().join("f2.txt"), "base-2\n").unwrap();
    let parent_dir = dir.path().to_path_buf();
    let child_id = Arc::new(Mutex::new(String::new()));
    let child_id_m = child_id.clone();

    let model = Arc::new(RouterModel::new(move |req, turn| {
        let first = first_user_task(&req.history);
        if first == "CHILD_EDITS" {
            return match turn {
                0 => Ok(finish_tool_calls(
                    vec![
                        (
                            "c1",
                            "write_file",
                            json!({
                                "path": "f1.txt",
                                "content": "child-1\n",
                                "expected_version": version_of(b"base-1\n")
                            }),
                        ),
                        (
                            "c2",
                            "write_file",
                            json!({
                                "path": "f2.txt",
                                "content": "child-2\n",
                                "expected_version": version_of(b"base-2\n")
                            }),
                        ),
                    ],
                    10,
                    5,
                )),
                _ => Ok(finish_text("edited both", 10, 5)),
            };
        }
        match turn {
            0 => Ok(finish_tool_calls(
                vec![(
                    "p1",
                    "subagent_spawn",
                    json!({
                        "label": "dir-worker",
                        "task": "CHILD_EDITS",
                        "workspace_isolation": "isolated",
                        "background": false
                    }),
                )],
                10,
                5,
            )),
            1 => {
                // Inject a concurrent parent modification to f1.txt right before merge!
                let id = find_id_in_history(&req.history, "p1");
                *child_id_m.lock().unwrap() = id.clone();
                fs::write(parent_dir.join("f1.txt"), "parent-conflict-1\n").unwrap();
                Ok(finish_tool_calls(
                    vec![("p2", "subagent_merge", json!({"subagent_id": id}))],
                    10,
                    5,
                ))
            }
            2 => {
                let id = child_id_m.lock().unwrap().clone();
                Ok(finish_tool_calls(
                    vec![(
                        "p3",
                        "subagent_merge",
                        json!({"subagent_id": id, "paths": ["f2.txt"], "discard_after": false}),
                    )],
                    10,
                    5,
                ))
            }
            3 => {
                let id = child_id_m.lock().unwrap().clone();
                Ok(finish_tool_calls(
                    vec![(
                        "p4",
                        "subagent_cancel",
                        json!({"subagent_id": id, "discard": true}),
                    )],
                    10,
                    5,
                ))
            }
            _ => Ok(finish_text("handled conflict and selective merge", 10, 5)),
        }
    }));

    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        ..Default::default()
    };
    let (mut runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Code,
        permissions: PermissionMode::Bypass,
        perm_cfg: PermissionConfig::default(),
        model,
        store,
        cancel: Arc::new(NeverCancel),
        confirmation: None,
        settings,
        limits: RuntimeLimits::default(),
        known_secrets: Vec::new(),
        task: "PARENT",
    });

    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);

    let spawn_res = find_tool_result(&session, "p1");
    assert_eq!(
        spawn_res.data.as_ref().unwrap()["isolation_kind"],
        "directory_copy"
    );
    let conflict_res = find_tool_result(&session, "p2");
    assert!(!conflict_res.ok);
    assert_eq!(
        conflict_res.error.as_ref().unwrap().code,
        error_codes::SUBAGENT_CONFLICT
    );
    assert_eq!(
        conflict_res.data.as_ref().unwrap()["conflicts"][0]["path"],
        "f1.txt"
    );
    // Selective merge of f2.txt succeeded while f1.txt preserved parent edits.
    assert!(find_tool_result(&session, "p3").ok);
    assert_eq!(
        fs::read_to_string(dir.path().join("f1.txt")).unwrap(),
        "parent-conflict-1\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("f2.txt")).unwrap(),
        "child-2\n"
    );
    assert_eq!(
        find_tool_result(&session, "p4").data.as_ref().unwrap()["state"],
        "discarded"
    );
}

fn find_id_in_history(history: &[HistoryItem], target_call_id: &str) -> String {
    let found = history.iter().find_map(|item| match item {
        HistoryItem::ToolResult { call_id, result } if call_id == target_call_id => Some(result),
        _ => None,
    });
    match found {
        Some(res) if res.ok => res
            .data
            .as_ref()
            .and_then(|d| d["id"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| panic!("missing id in ok result: {res:?}")),
        other => panic!("expected ok tool result for {target_call_id}, got: {other:?}"),
    }
}

#[tokio::test]
async fn plan_mode_enforces_read_only_and_rejects_mutating_delegation() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("readme.md"), "plan target\n").unwrap();

    let model = Arc::new(RouterModel::new(|req, turn| {
        let first = first_user_task(&req.history);
        if first == "RO_CHILD" {
            // Confirm child sees NO write tools and NO subagent_* tools (max_depth = 1).
            assert!(!req.tools.iter().any(|t| t.name == "write_file"));
            assert!(!req.tools.iter().any(|t| t.name.starts_with("subagent_")));
            return Ok(finish_text("audited readme", 10, 5));
        }
        match turn {
            0 => Ok(finish_tool_calls(
                vec![
                    (
                        "p_bad",
                        "subagent_spawn",
                        json!({
                            "label": "bad",
                            "task": "RO_CHILD",
                            "workspace_isolation": "isolated"
                        }),
                    ),
                    (
                        "p_bad_exec",
                        "subagent_spawn",
                        json!({
                            "label": "bad-exec",
                            "task": "RO_CHILD",
                            "workspace_isolation": "read_only",
                            "allow_exec": true
                        }),
                    ),
                    (
                        "p_good",
                        "subagent_spawn",
                        json!({
                            "label": "good-ro",
                            "task": "RO_CHILD",
                            "workspace_isolation": "read_only",
                            "background": false
                        }),
                    ),
                    (
                        "p_bad_merge",
                        "subagent_merge",
                        json!({"subagent_id": "sub-1-000000"}),
                    ),
                ],
                10,
                5,
            )),
            _ => Ok(finish_text("plan complete", 10, 5)),
        }
    }));

    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        ..Default::default()
    };
    let (mut runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Plan,
        permissions: PermissionMode::Auto,
        perm_cfg: PermissionConfig::default(),
        model,
        store,
        cancel: Arc::new(NeverCancel),
        confirmation: None,
        settings,
        limits: RuntimeLimits::default(),
        known_secrets: Vec::new(),
        task: "PLAN_PARENT",
    });
    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(
        find_tool_result(&session, "p_bad")
            .error
            .as_ref()
            .unwrap()
            .code,
        error_codes::MODE_DENIED
    );
    assert_eq!(
        find_tool_result(&session, "p_bad_exec")
            .error
            .as_ref()
            .unwrap()
            .code,
        error_codes::MODE_DENIED
    );
    assert!(find_tool_result(&session, "p_good").ok);
    assert_eq!(
        find_tool_result(&session, "p_bad_merge")
            .error
            .as_ref()
            .unwrap()
            .code,
        error_codes::DURABILITY_REQUIRED
    );

    // Also verify Policy::evaluate blocks subagent_merge with MODE_DENIED in Plan mode
    // even when durability (`AgentRuntime::new`) is present.
    let model2 = Arc::new(RouterModel::new(|_req, turn| match turn {
        0 => Ok(finish_tool_calls(
            vec![(
                "pm",
                "subagent_merge",
                json!({"subagent_id": "sub-1-000000"}),
            )],
            10,
            5,
        )),
        _ => Ok(finish_text("done", 10, 5)),
    }));
    let mut session2 = build_session(dir.path(), WorkMode::Plan, "PLAN_POLICY_CHECK");
    let setup2 = RuntimeSetup {
        run: Run {
            id: RunId::new(),
            session_id: session2.id,
            work_mode: WorkMode::Plan,
            permissions: PermissionMode::Auto,
        },
        model: model2,
        store: Arc::new(RecordingStore::new()),
        policy: Arc::new(Policy::new(dir.path(), PermissionConfig::default()).unwrap()),
        files: Arc::new(ReadTools::new(dir.path(), &[], false, ReadLimits::default()).unwrap()),
        cancellation: Arc::new(NeverCancel),
        review: ReviewConfig::new("test".into()),
        exec: ExecConfig::default(),
        preauthorizations: Preauthorizations::default(),
        known_secrets: Vec::new(),
        summary: "plan policy check".into(),
        confirmation: None,
        limits: RuntimeLimits::default(),
    };
    let settings2 = SubagentSettings {
        enabled: true,
        ..Default::default()
    };
    let mut rt2 = AgentRuntime::new(setup2)
        .unwrap()
        .with_subagents(settings2.resolve().unwrap())
        .unwrap();
    let res2 = rt2.run(&mut session2).await.unwrap();
    assert_eq!(res2.status, RunStatus::Completed);
    let pm_err = find_tool_result(&session2, "pm").error.as_ref().unwrap();
    assert_eq!(pm_err.code, error_codes::PERMISSION_DENIED, "{pm_err:?}");
}

#[tokio::test]
async fn security_globs_bypass_containment_approvals_max_depth_and_secret_redaction() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("secrets")).unwrap();
    fs::create_dir_all(dir.path().join("guarded")).unwrap();
    fs::create_dir_all(dir.path().join("other")).unwrap();
    fs::write(dir.path().join("secrets/api.key"), "top-secret-file").unwrap();
    fs::write(dir.path().join("guarded/work.txt"), "initial").unwrap();
    fs::write(dir.path().join("other/sibling.txt"), "sibling").unwrap();

    let cfg_toml = format!(
        r#"
[[rules]]
effect = "deny"
path_glob = "{}/secrets/**"

[[rules]]
effect = "ask"
path_glob = "{}/guarded/**"
"#,
        dir.path().display(),
        dir.path().display()
    );
    let perm_cfg = PermissionConfig::from_toml(&cfg_toml).unwrap();

    let build_sec_model = |saw_flag: Arc<AtomicBool>| {
        Arc::new(RouterModel::new(move |req, turn| {
            let first = first_user_task(&req.history);
            if first == "SEC_CHILD" {
                if req.tools.iter().any(|t| t.name.starts_with("subagent_")) {
                    saw_flag.store(true, Ordering::Release);
                }
                return match turn {
                    0 => Ok(finish_tool_calls(
                        vec![
                            ("c_sec", "read_file", json!({"path": "secrets/api.key"})),
                            ("c_oth", "read_file", json!({"path": "other/sibling.txt"})),
                            (
                                "c_sub",
                                "subagent_spawn",
                                json!({"label": "nested", "task": "boom"}),
                            ),
                            ("c_exe", "exec", json!({"argv": ["echo", "hi"]})),
                            (
                                "c_w",
                                "write_file",
                                json!({
                                    "path": "guarded/work.txt",
                                    "content": "updated",
                                    "expected_version": version_of(b"initial")
                                }),
                            ),
                        ],
                        10,
                        5,
                    )),
                    _ => Ok(finish_text(
                        "leaked SUPER_STATIC_SECRET and DYNAMIC_MCP_SECRET",
                        10,
                        5,
                    )),
                };
            }
            match turn {
                0 => Ok(finish_tool_calls(
                    vec![(
                        "p1",
                        "subagent_spawn",
                        json!({
                            "label": "sec-worker",
                            "task": "SEC_CHILD",
                            "workspace_isolation": "isolated",
                            "allowed_paths": ["guarded"],
                            "allow_exec": false,
                            "background": false
                        }),
                    )],
                    10,
                    5,
                )),
                _ => Ok(finish_text("done", 10, 5)),
            }
        }))
    };

    // 1) Auto mode: child inherits `ask` rule on guarded/** with `[subagent:...]` approval prefix,
    // remapped deny on secrets/**, `max_depth = 1`, `exec` disabled, and redacts secrets.
    let child_saw_subagent_tool = Arc::new(AtomicBool::new(false));
    let model = build_sec_model(child_saw_subagent_tool.clone());

    let approval = Arc::new(RecordingApproval::allow_all());
    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        ..Default::default()
    };
    let (runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Code,
        permissions: PermissionMode::Auto,
        perm_cfg: perm_cfg.clone(),
        model: model.clone(),
        store: store.clone(),
        cancel: Arc::new(NeverCancel),
        confirmation: Some(approval.clone()),
        settings: settings.clone(),
        limits: RuntimeLimits::default(),
        known_secrets: vec!["SUPER_STATIC_SECRET".into()],
        task: "PARENT_SEC",
    });
    let registry = Arc::new(SecretRegistry::default());
    registry.register(&["DYNAMIC_MCP_SECRET".into()]).unwrap();
    let mut runtime = runtime.with_secret_registry(registry).unwrap();

    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert!(!child_saw_subagent_tool.load(Ordering::Acquire));

    let spawn = find_tool_result(&session, "p1");
    assert!(spawn.ok);
    let data = spawn.data.as_ref().unwrap();
    let summary = data["summary"].as_str().unwrap();
    assert!(!summary.contains("SUPER_STATIC_SECRET"), "{summary}");
    assert!(!summary.contains("DYNAMIC_MCP_SECRET"), "{summary}");
    assert!(summary.contains("[REDACTED]"), "{summary}");

    let traces = data["step_traces"].as_array().unwrap();
    let find_status = |call_id: &str| {
        traces.iter().find(|t| t["call_id"] == call_id).unwrap()["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(find_status("c_sec"), "permission_denied");
    assert_eq!(find_status("c_oth"), "permission_denied");
    assert_eq!(find_status("c_sub"), "unknown_tool");
    assert_eq!(find_status("c_exe"), "unknown_tool");
    assert_eq!(find_status("c_w"), "ok");

    assert!(
        approval
            .summaries()
            .iter()
            .any(|s| s.starts_with("[subagent:") && s.contains("sec-worker"))
    );

    // 2) Bypass mode: child MUST STILL be contained inside `allowed_paths` & denied `secrets/**` & `exec`!
    let store2 = Arc::new(RecordingStore::new());
    let model2 = build_sec_model(Arc::new(AtomicBool::new(false)));
    let (mut bypass_rt, mut bypass_sess) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Code,
        permissions: PermissionMode::Bypass,
        perm_cfg,
        model: model2,
        store: store2,
        cancel: Arc::new(NeverCancel),
        confirmation: None,
        settings,
        limits: RuntimeLimits::default(),
        known_secrets: Vec::new(),
        task: "PARENT_BYPASS",
    });
    let bypass_res = bypass_rt.run(&mut bypass_sess).await.unwrap();
    assert_eq!(bypass_res.status, RunStatus::Completed);
    let bypass_spawn = find_tool_result(&bypass_sess, "p1");
    let bypass_traces = bypass_spawn.data.as_ref().unwrap()["step_traces"]
        .as_array()
        .unwrap();
    let bypass_status = |call_id: &str| {
        bypass_traces
            .iter()
            .find(|t| t["call_id"] == call_id)
            .unwrap()["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(bypass_status("c_sec"), "permission_denied");
    assert_eq!(bypass_status("c_oth"), "permission_denied");
    assert_eq!(bypass_status("c_exe"), "unknown_tool");
    assert_eq!(bypass_status("c_w"), "ok");
}

#[tokio::test]
async fn shared_budget_tool_cap_metrics_rollup_and_followup_message() {
    let dir = tempdir().unwrap();
    let child_id = Arc::new(Mutex::new(String::new()));
    let child_id_m = child_id.clone();

    let model = Arc::new(RouterModel::new(move |req, turn| {
        let first = first_user_task(&req.history);
        if first == "MULTI_CHILD" {
            return match turn {
                0 => Ok(finish_tool_calls(
                    vec![(
                        "c1",
                        "write_file",
                        json!({"path": "out.txt", "content": "turn1\n", "expected_version": Value::Null}),
                    )],
                    100,
                    20,
                )),
                1 => Ok(finish_text("turn 1 done", 50, 10)),
                2 => Ok(finish_text("turn 2 follow-up done", 40, 15)),
                _ => Ok(finish_text("unexpected extra turn", 1, 1)),
            };
        }
        match turn {
            0 => Ok(finish_tool_calls(
                vec![(
                    "p1",
                    "subagent_spawn",
                    json!({
                        "label": "multi",
                        "task": "MULTI_CHILD",
                        "workspace_isolation": "isolated",
                        "background": false
                    }),
                )],
                200,
                30,
            )),
            1 => {
                let id = find_id_in_history(&req.history, "p1");
                *child_id_m.lock().unwrap() = id.clone();
                Ok(finish_tool_calls(
                    vec![(
                        "p2",
                        "subagent_message",
                        json!({"subagent_id": id, "message": "follow up check"}),
                    )],
                    150,
                    25,
                ))
            }
            _ => Ok(finish_text("parent complete", 100, 15)),
        }
    }));

    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        ..Default::default()
    };
    let limits = RuntimeLimits {
        model_requests: 10,
        tool_calls: 10,
        ..RuntimeLimits::default()
    };
    let (mut runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Code,
        permissions: PermissionMode::Bypass,
        perm_cfg: PermissionConfig::default(),
        model,
        store: store.clone(),
        cancel: Arc::new(NeverCancel),
        confirmation: None,
        settings,
        limits,
        known_secrets: Vec::new(),
        task: "PARENT_MULTI",
    });

    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    // Parent: 3 requests (200+150+100 = 450 in, 30+25+15 = 70 out)
    // Child:  3 requests (100+50+40  = 190 in, 20+10+15 = 45 out)
    // Single-source roll-up sum = 640 in, 115 out, 6 execution requests total!
    let usage = result.usage.unwrap();
    assert_eq!(usage.input_tokens, Some(640));
    assert_eq!(usage.output_tokens, Some(115));
    let metrics = store.recorded_metrics().unwrap();
    assert_eq!(metrics.execution_requests, 6);
    assert_eq!(metrics.tool_calls, 3);
}

#[tokio::test]
async fn shared_parent_request_budget_stops_overspending_subagents() {
    let dir = tempdir().unwrap();
    let model = Arc::new(RouterModel::new(|req, turn| {
        let first = first_user_task(&req.history);
        if first == "HUNGRY_CHILD" {
            return match turn {
                0 => Ok(finish_tool_calls(
                    vec![("c1", "list_dir", json!({"path": "."}))],
                    10,
                    5,
                )),
                _ => Ok(finish_text("should not fit in parent budget", 10, 5)),
            };
        }
        Ok(finish_tool_calls(
            vec![(
                "p1",
                "subagent_spawn",
                json!({"label": "hungry", "task": "HUNGRY_CHILD", "workspace_isolation": "read_only"}),
            )],
            10,
            5,
        ))
    }));

    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        ..Default::default()
    };
    // Parent only gets 2 model requests total: 1 for parent spawn, 1 for child -> 2nd child turn hits limit_reached!
    let limits = RuntimeLimits {
        model_requests: 2,
        ..RuntimeLimits::default()
    };
    let (mut runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Work,
        permissions: PermissionMode::Bypass,
        perm_cfg: PermissionConfig::default(),
        model,
        store,
        cancel: Arc::new(NeverCancel),
        confirmation: None,
        settings,
        limits,
        known_secrets: Vec::new(),
        task: "PARENT_BUDGET",
    });

    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::LimitReached);
    let spawn = find_tool_result(&session, "p1");
    assert_eq!(spawn.data.as_ref().unwrap()["state"], "limit_reached");
}

#[tokio::test]
async fn parent_cancel_terminates_running_subagent_and_cleans_workspace() {
    let dir = tempdir().unwrap();
    init_git(dir.path());
    let cancel = Arc::new(FlagCancel::new());
    let cancel_trigger = cancel.clone();

    struct BlockingModel {
        started: Notify,
    }
    impl Model for BlockingModel {
        fn start<'a>(
            &'a self,
            request: &'a ModelRequest,
            cancel: &'a dyn Cancellation,
        ) -> Operation<'a, Box<dyn ModelStream>> {
            Box::pin(async move {
                let is_child = request
                    .history
                    .iter()
                    .any(|item| matches!(item, HistoryItem::UserText(t) if t == "BLOCKING_CHILD"));
                if !is_child {
                    return Ok(Box::new(VecStream(
                        finish_tool_calls(
                            vec![(
                                "p1",
                                "subagent_spawn",
                                json!({"label": "slow", "task": "BLOCKING_CHILD", "workspace_isolation": "isolated"}),
                            )],
                            10,
                            5,
                        )
                        .into(),
                    )) as Box<dyn ModelStream>);
                }
                self.started.notify_one();
                cancel.cancelled().await;
                Err(BoundaryError::new(error_codes::CANCELLED, "cancelled"))
            })
        }
    }

    let model = Arc::new(BlockingModel {
        started: Notify::new(),
    });
    let model_wait = model.clone();
    tokio::spawn(async move {
        model_wait.started.notified().await;
        cancel_trigger.trigger();
    });

    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        ..Default::default()
    };
    let (mut runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Code,
        permissions: PermissionMode::Bypass,
        perm_cfg: PermissionConfig::default(),
        model,
        store,
        cancel,
        confirmation: None,
        settings,
        limits: RuntimeLimits::default(),
        known_secrets: Vec::new(),
        task: "PARENT_CANCEL",
    });

    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Cancelled);

    let wt_out = Command::new("git")
        .current_dir(dir.path())
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let wt_text = String::from_utf8_lossy(&wt_out.stdout);
    assert_eq!(wt_text.matches("worktree ").count(), 1, "{wt_text}");
    let worktrees_dir = dir.path().join(".git/worktrees");
    assert!(!worktrees_dir.exists() || fs::read_dir(&worktrees_dir).unwrap().next().is_none());
}

#[tokio::test]
async fn subagent_timeout_transitions_to_limit_reached() {
    let dir = tempdir().unwrap();

    struct TimeoutModel;
    impl Model for TimeoutModel {
        fn start<'a>(
            &'a self,
            request: &'a ModelRequest,
            cancel: &'a dyn Cancellation,
        ) -> Operation<'a, Box<dyn ModelStream>> {
            Box::pin(async move {
                let is_child = request
                    .history
                    .iter()
                    .any(|item| matches!(item, HistoryItem::UserText(t) if t == "TIMEOUT_CHILD"));
                if is_child {
                    cancel.cancelled().await;
                    return Err(BoundaryError::new(
                        error_codes::CANCELLED,
                        "child timed out",
                    ));
                }
                let has_result = request
                    .history
                    .iter()
                    .any(|item| matches!(item, HistoryItem::ToolResult { .. }));
                if !has_result {
                    Ok(Box::new(VecStream(
                        finish_tool_calls(
                            vec![(
                                "p1",
                                "subagent_spawn",
                                json!({
                                    "label": "timeout-worker",
                                    "task": "TIMEOUT_CHILD",
                                    "workspace_isolation": "read_only",
                                    "timeout_seconds": 1,
                                    "background": false
                                }),
                            )],
                            10,
                            5,
                        )
                        .into(),
                    )) as Box<dyn ModelStream>)
                } else {
                    Ok(Box::new(VecStream(
                        finish_text("observed child timeout", 10, 5).into(),
                    )) as Box<dyn ModelStream>)
                }
            })
        }
    }

    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        ..Default::default()
    };
    let (mut runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Work,
        permissions: PermissionMode::Bypass,
        perm_cfg: PermissionConfig::default(),
        model: Arc::new(TimeoutModel),
        store,
        cancel: Arc::new(NeverCancel),
        confirmation: None,
        settings,
        limits: RuntimeLimits::default(),
        known_secrets: Vec::new(),
        task: "PARENT_TIMEOUT",
    });

    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    let spawn = find_tool_result(&session, "p1");
    assert_eq!(spawn.data.as_ref().unwrap()["state"], "limit_reached");
}

#[tokio::test]
async fn switching_parent_to_plan_cancels_running_mutating_subagent() {
    let dir = tempdir().unwrap();
    let child_started = Arc::new(Notify::new());
    let child_started_m = child_started.clone();
    let plan_requested = Arc::new(Notify::new());
    let plan_requested_m = plan_requested.clone();
    let child_cancelled = Arc::new(AtomicBool::new(false));
    let child_cancelled_m = child_cancelled.clone();

    struct CancelDropGuard<'a> {
        cancel: &'a dyn Cancellation,
        flag: &'a AtomicBool,
    }
    impl Drop for CancelDropGuard<'_> {
        fn drop(&mut self) {
            if self.cancel.is_cancelled() {
                self.flag.store(true, Ordering::Release);
            }
        }
    }

    struct PlanSwitchModel {
        child_started: Arc<Notify>,
        plan_requested: Arc<Notify>,
        child_cancelled: Arc<AtomicBool>,
    }
    impl Model for PlanSwitchModel {
        fn start<'a>(
            &'a self,
            request: &'a ModelRequest,
            cancel: &'a dyn Cancellation,
        ) -> Operation<'a, Box<dyn ModelStream>> {
            Box::pin(async move {
                let is_child = request
                    .history
                    .iter()
                    .any(|item| matches!(item, HistoryItem::UserText(t) if t == "MUTATING_BG"));
                if is_child {
                    let _drop_guard = CancelDropGuard {
                        cancel,
                        flag: &self.child_cancelled,
                    };
                    self.child_started.notify_waiters();
                    cancel.cancelled().await;
                    self.child_cancelled.store(true, Ordering::Release);
                    return Err(BoundaryError::new(error_codes::CANCELLED, "plan cancelled"));
                }
                let spawned_id = request.history.iter().find_map(|item| match item {
                    HistoryItem::ToolResult { call_id, result } if call_id == "p1" => result
                        .data
                        .as_ref()
                        .and_then(|d| d["id"].as_str())
                        .map(str::to_owned),
                    _ => None,
                });
                let has_status = request.history.iter().any(|item| {
                    matches!(item, HistoryItem::ToolResult { call_id, .. } if call_id == "p3")
                });
                let has_probe = request.history.iter().any(|item| {
                    matches!(item, HistoryItem::ToolResult { call_id, .. } if call_id == "p2")
                });
                let Some(sub_id) = spawned_id else {
                    return Ok(Box::new(VecStream(
                        finish_tool_calls(
                            vec![(
                                "p1",
                                "subagent_spawn",
                                json!({
                                    "label": "bg-mutator",
                                    "task": "MUTATING_BG",
                                    "workspace_isolation": "isolated",
                                    "background": true
                                }),
                            )],
                            10,
                            5,
                        )
                        .into(),
                    )) as Box<dyn ModelStream>);
                };
                if !has_probe {
                    self.plan_requested.notified().await;
                    Ok(Box::new(VecStream(
                        finish_tool_calls(vec![("p2", "list_dir", json!({"path": "."}))], 10, 5)
                            .into(),
                    )) as Box<dyn ModelStream>)
                } else if !has_status {
                    Ok(Box::new(VecStream(
                        finish_tool_calls(
                            vec![("p3", "subagent_status", json!({"subagent_id": sub_id}))],
                            10,
                            5,
                        )
                        .into(),
                    )) as Box<dyn ModelStream>)
                } else {
                    Ok(Box::new(VecStream(
                        finish_text("verified plan cancelled mutating child", 10, 5).into(),
                    )) as Box<dyn ModelStream>)
                }
            })
        }
    }

    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        ..Default::default()
    };
    let (mut runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Code,
        permissions: PermissionMode::Bypass,
        perm_cfg: PermissionConfig::default(),
        model: Arc::new(PlanSwitchModel {
            child_started: child_started_m,
            plan_requested: plan_requested_m,
            child_cancelled: child_cancelled_m,
        }),
        store,
        cancel: Arc::new(NeverCancel),
        confirmation: None,
        settings,
        limits: RuntimeLimits::default(),
        known_secrets: Vec::new(),
        task: "PARENT_PLAN_SWITCH",
    });

    let handle = runtime.handle();
    let mode_check = handle.clone();
    tokio::spawn(async move {
        child_started.notified().await;
        let _ = handle.request_mode(WorkMode::Plan);
        plan_requested.notify_waiters();
    });

    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert!(child_cancelled.load(Ordering::Acquire));
    assert_eq!(mode_check.mode().unwrap().current, WorkMode::Plan);
    let status_res = find_tool_result(&session, "p3");
    assert!(status_res.ok, "{status_res:?}");
    assert_eq!(status_res.data.as_ref().unwrap()["state"], "cancelled");
}

// ------------------------- D-109 multi-level delegation -------------------------

#[test]
fn max_depth_setting_defaults_to_one_and_rejects_out_of_range() {
    let settings = SubagentSettings {
        enabled: true,
        ..Default::default()
    };
    let resolved = settings.resolve().unwrap().unwrap();
    assert_eq!(resolved.limits.max_depth, 1);
    assert_eq!(resolved.depth(), 0);
    // `max_depth = 1` means exactly one hop: children get no supervisor.
    assert!(resolved.for_child().is_none());

    for bad in [0usize, 4, usize::MAX] {
        let mut invalid = SubagentSettings {
            enabled: true,
            ..Default::default()
        };
        invalid.max_depth = Some(bad);
        assert!(invalid.validate().is_err(), "max_depth {bad} must fail");
    }
    let deep = SubagentSettings {
        enabled: true,
        max_depth: Some(3),
        ..Default::default()
    };
    let resolved = deep.resolve().unwrap().unwrap();
    let child = resolved.for_child().expect("hop 1 supervisor allowed");
    assert_eq!(child.depth(), 1);
    let grandchild = child.for_child().expect("hop 2 supervisor allowed");
    assert_eq!(grandchild.depth(), 2);
    assert!(
        grandchild.for_child().is_none(),
        "a runtime at max_depth must not delegate further"
    );
}

/// `max_depth = 3` allows a three-hop chain (parent → child → grandchild →
/// great-grandchild). Runtimes at hops 1 and 2 see `subagent_*` tools; the
/// runtime at hop 3 (`== max_depth`) gets none, and a fabricated
/// `subagent_spawn` there fails closed as `unknown_tool`.
#[tokio::test]
async fn three_hop_chain_allowed_and_max_depth_runtime_fails_closed() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("note.txt"), "leaf\n").unwrap();

    let child_saw_tools = Arc::new(AtomicBool::new(false));
    let grand_saw_tools = Arc::new(AtomicBool::new(false));
    let great_saw_tools = Arc::new(AtomicBool::new(false));
    let ghost_error = Arc::new(Mutex::new(None::<String>));
    let child_saw = child_saw_tools.clone();
    let grand_saw = grand_saw_tools.clone();
    let great_saw = great_saw_tools.clone();
    let ghost = ghost_error.clone();

    let model = Arc::new(RouterModel::new(move |req, turn| {
        let first = first_user_task(&req.history);
        let sees_tools = req.tools.iter().any(|t| t.name.starts_with("subagent_"));
        match first {
            "GREAT_TASK" => {
                // Hop 3 == max_depth: no supervisor, so no subagent tools.
                great_saw.store(sees_tools, Ordering::Release);
                match turn {
                    0 => Ok(finish_tool_calls(
                        vec![(
                            "ghost",
                            "subagent_spawn",
                            json!({"label": "ghost", "task": "GHOST_TASK"}),
                        )],
                        10,
                        5,
                    )),
                    1 => {
                        // Fabricated spawn must fail closed at dispatch.
                        let code = req.history.iter().find_map(|item| match item {
                            HistoryItem::ToolResult { call_id, result } if call_id == "ghost" => {
                                result.error.as_ref().map(|err| err.code.clone())
                            }
                            _ => None,
                        });
                        *ghost.lock().unwrap() = code;
                        Ok(finish_text("great-grandchild done", 10, 5))
                    }
                    _ => Ok(finish_text("great-grandchild done", 10, 5)),
                }
            }
            "GRAND_TASK" => {
                grand_saw.store(sees_tools, Ordering::Release);
                match turn {
                    0 => Ok(finish_tool_calls(
                        vec![(
                            "gg1",
                            "subagent_spawn",
                            json!({
                                "label": "great-grandchild",
                                "task": "GREAT_TASK",
                                "mode": "plan",
                                "workspace_isolation": "read_only",
                                "background": false
                            }),
                        )],
                        10,
                        5,
                    )),
                    _ => Ok(finish_text("grandchild done", 10, 5)),
                }
            }
            "CHILD_TASK" => {
                child_saw.store(sees_tools, Ordering::Release);
                match turn {
                    0 => Ok(finish_tool_calls(
                        vec![(
                            "gc1",
                            "subagent_spawn",
                            json!({
                                "label": "grandchild",
                                "task": "GRAND_TASK",
                                "mode": "plan",
                                "workspace_isolation": "read_only",
                                "background": false
                            }),
                        )],
                        10,
                        5,
                    )),
                    _ => Ok(finish_text("child done", 10, 5)),
                }
            }
            _ => match turn {
                0 => Ok(finish_tool_calls(
                    vec![(
                        "p1",
                        "subagent_spawn",
                        json!({
                            "label": "child",
                            "task": "CHILD_TASK",
                            "mode": "plan",
                            "workspace_isolation": "read_only",
                            "background": false
                        }),
                    )],
                    10,
                    5,
                )),
                _ => Ok(finish_text("parent done", 10, 5)),
            },
        }
    }));

    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        max_depth: Some(3),
        ..Default::default()
    };
    let (mut runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Code,
        permissions: PermissionMode::Bypass,
        perm_cfg: PermissionConfig::default(),
        model,
        store,
        cancel: Arc::new(NeverCancel),
        confirmation: None,
        settings,
        limits: RuntimeLimits::default(),
        known_secrets: Vec::new(),
        task: "PARENT_DEPTH",
    });

    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert!(child_saw_tools.load(Ordering::Acquire), "hop 1 lacks tools");
    assert!(grand_saw_tools.load(Ordering::Acquire), "hop 2 lacks tools");
    assert!(
        !great_saw_tools.load(Ordering::Acquire),
        "runtime at max_depth still exposed subagent tools"
    );
    assert_eq!(
        ghost_error.lock().unwrap().as_deref(),
        Some(error_codes::UNKNOWN_TOOL)
    );
    let spawn = find_tool_result(&session, "p1");
    assert!(spawn.ok, "{spawn:?}");
    assert_eq!(spawn.data.as_ref().unwrap()["state"], "completed");
}

/// Depth-2 child narrows `allowed_paths` to `src/`; a grandchild spawned under
/// it must be contained inside the *child's* allowed subtree — the child's own
/// `deny` remapping and root enforcement cannot be widened back out.
#[tokio::test]
async fn allowed_paths_tighten_down_the_chain() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::create_dir_all(dir.path().join("other")).unwrap();
    fs::write(dir.path().join("src/ok.rs"), "src\n").unwrap();
    fs::write(dir.path().join("other/sibling.txt"), "sibling\n").unwrap();

    let probe_error = Arc::new(Mutex::new(None::<String>));
    let probe = probe_error.clone();
    let model = Arc::new(RouterModel::new(move |req, turn| {
        let first = first_user_task(&req.history);
        match first {
            "GRAND_PROBE" => match turn {
                // Grandchild (read_only): it must be denied reading the
                // sibling tree that sits outside its parent's allowed_paths.
                0 => Ok(finish_tool_calls(
                    vec![("g_probe", "read_file", json!({"path": "other/sibling.txt"}))],
                    10,
                    5,
                )),
                1 => {
                    let code = req.history.iter().find_map(|item| match item {
                        HistoryItem::ToolResult { call_id, result } if call_id == "g_probe" => {
                            result.error.as_ref().map(|err| err.code.clone())
                        }
                        _ => None,
                    });
                    *probe.lock().unwrap() = code;
                    Ok(finish_text("grandchild probed", 10, 5))
                }
                _ => Ok(finish_text("grandchild probed", 10, 5)),
            },
            "CHILD_NARROW" => match turn {
                0 => Ok(finish_tool_calls(
                    vec![(
                        "c_spawn",
                        "subagent_spawn",
                        json!({
                            "label": "gc",
                            "task": "GRAND_PROBE",
                            "mode": "plan",
                            "workspace_isolation": "read_only",
                            "background": false
                        }),
                    )],
                    10,
                    5,
                )),
                _ => Ok(finish_text("child narrowed", 10, 5)),
            },
            _ => match turn {
                0 => Ok(finish_tool_calls(
                    vec![(
                        "p1",
                        "subagent_spawn",
                        json!({
                            "label": "child",
                            "task": "CHILD_NARROW",
                            "mode": "code",
                            "workspace_isolation": "isolated",
                            "allowed_paths": ["src"],
                            "background": false
                        }),
                    )],
                    10,
                    5,
                )),
                _ => Ok(finish_text("parent done", 10, 5)),
            },
        }
    }));

    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        max_depth: Some(3),
        ..Default::default()
    };
    let (mut runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Code,
        permissions: PermissionMode::Bypass,
        perm_cfg: PermissionConfig::default(),
        model,
        store,
        cancel: Arc::new(NeverCancel),
        confirmation: None,
        settings,
        limits: RuntimeLimits::default(),
        known_secrets: Vec::new(),
        task: "PARENT_TIGHTEN",
    });

    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    let spawn = find_tool_result(&session, "p1");
    assert!(spawn.ok, "{spawn:?}");
    // The child's step trace shows the grandchild spawn succeeded; the
    // grandchild's own `read_file` outside the child's `allowed_paths` was
    // still denied, so narrowing survives one more level down.
    let traces = spawn.data.as_ref().unwrap()["step_traces"]
        .as_array()
        .unwrap();
    let gc_spawn = traces
        .iter()
        .find(|t| t["call_id"] == "c_spawn")
        .expect("missing grandchild spawn trace");
    assert_eq!(gc_spawn["status"], "ok", "{gc_spawn:?}");
    assert_eq!(
        probe_error.lock().unwrap().as_deref(),
        Some(error_codes::PERMISSION_DENIED),
        "grandchild escaped its parent's allowed_paths"
    );
}

/// `max_total` and `max_concurrent` aggregate to the top-level run: a depth-2
/// supervisor cannot fan out past the same `max_total` allowance.
#[tokio::test]
async fn max_total_and_concurrent_share_one_budget_across_depths() {
    let dir = tempdir().unwrap();

    let model = Arc::new(RouterModel::new(|req, turn| {
        let first = first_user_task(&req.history);
        match first {
            "GRAND" => Ok(finish_text("grandchild ok", 10, 5)),
            "CHILD_FANOUT" => match turn {
                0 => Ok(finish_tool_calls(
                    vec![
                        (
                            "c1",
                            "subagent_spawn",
                            json!({"label": "g1", "task": "GRAND", "mode": "plan", "workspace_isolation": "read_only"}),
                        ),
                        (
                            "c2",
                            "subagent_spawn",
                            json!({"label": "g2", "task": "GRAND", "mode": "plan", "workspace_isolation": "read_only"}),
                        ),
                    ],
                    10,
                    5,
                )),
                _ => Ok(finish_text("child done", 10, 5)),
            },
            _ => match turn {
                0 => Ok(finish_tool_calls(
                    vec![(
                        "p1",
                        "subagent_spawn",
                        json!({"label": "child", "task": "CHILD_FANOUT", "mode": "code", "workspace_isolation": "read_only"}),
                    )],
                    10,
                    5,
                )),
                _ => Ok(finish_text("parent done", 10, 5)),
            },
        }
    }));

    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        max_depth: Some(2),
        // Only 2 spawns allowed across the WHOLE chain: the parent uses one
        // for the child and the child one for `g1`, so `g2` must be rejected.
        max_concurrent: Some(2),
        max_total: Some(2),
        ..Default::default()
    };
    let (mut runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Code,
        permissions: PermissionMode::Bypass,
        perm_cfg: PermissionConfig::default(),
        model,
        store,
        cancel: Arc::new(NeverCancel),
        confirmation: None,
        settings,
        limits: RuntimeLimits::default(),
        known_secrets: Vec::new(),
        task: "PARENT_BUDGET_TOTAL",
    });

    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    let spawn = find_tool_result(&session, "p1");
    assert!(spawn.ok, "{spawn:?}");
    let traces = spawn.data.as_ref().unwrap()["step_traces"]
        .as_array()
        .unwrap();
    let trace_status = |id: &str| {
        traces
            .iter()
            .find(|t| t["call_id"] == id)
            .map(|t| t["status"].as_str().unwrap().to_owned())
    };
    // The first grandchild spawn consumes the remaining chain slot; the
    // second is rejected with the shared `subagent_limit` even though this
    // supervisor has never spawned anything itself.
    assert_eq!(trace_status("c1").as_deref(), Some("ok"), "{traces:?}");
    assert_eq!(
        trace_status("c2").as_deref(),
        Some(error_codes::SUBAGENT_LIMIT),
        "{traces:?}"
    );
}

/// A grandchild's model requests charge against the same top-level
/// `SharedBudget` the parent uses — spawning a deeper level must not hand out
/// a fresh allowance.
#[tokio::test]
async fn grandchild_model_requests_count_against_top_level_budget() {
    let dir = tempdir().unwrap();

    let model = Arc::new(RouterModel::new(|req, turn| {
        let first = first_user_task(&req.history);
        match first {
            "GRAND_HUNGRY" => match turn {
                // Depth-3 child needs two model turns (tool call then text).
                0 => Ok(finish_tool_calls(
                    vec![("g1", "list_dir", json!({"path": "."}))],
                    10,
                    5,
                )),
                _ => Ok(finish_text("grandchild would overspend", 10, 5)),
            },
            "CHILD_HUNGRY" => match turn {
                0 => Ok(finish_tool_calls(
                    vec![(
                        "c1",
                        "subagent_spawn",
                        json!({"label": "gc", "task": "GRAND_HUNGRY", "mode": "plan", "workspace_isolation": "read_only"}),
                    )],
                    10,
                    5,
                )),
                _ => Ok(finish_text("child done", 10, 5)),
            },
            _ => Ok(finish_tool_calls(
                vec![(
                    "p1",
                    "subagent_spawn",
                    json!({"label": "child", "task": "CHILD_HUNGRY", "mode": "plan", "workspace_isolation": "read_only"}),
                )],
                10,
                5,
            )),
        }
    }));

    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        max_depth: Some(3),
        ..Default::default()
    };
    // Budget = 3 model requests across the WHOLE chain: parent spawn (1),
    // child's grandchild spawn (1), grandchild's first turn (1) — the
    // grandchild's second request must trip `limit_reached`.
    let limits = RuntimeLimits {
        model_requests: 3,
        ..RuntimeLimits::default()
    };
    let (mut runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Code,
        permissions: PermissionMode::Bypass,
        perm_cfg: PermissionConfig::default(),
        model,
        store,
        cancel: Arc::new(NeverCancel),
        confirmation: None,
        settings,
        limits,
        known_secrets: Vec::new(),
        task: "PARENT_SHARED_BUDGET",
    });

    let result = runtime.run(&mut session).await.unwrap();
    // The chain overspends the shared budget; the parent sees `limit_reached`.
    assert_eq!(result.status, RunStatus::LimitReached);
}

/// Switching the top-level parent to `PLAN` must cancel the WHOLE subtree —
/// including a running grandchild two levels down — not only direct children.
#[tokio::test]
async fn plan_transition_cancels_full_subtree() {
    let dir = tempdir().unwrap();
    let grandchild_started = Arc::new(Notify::new());
    let grandchild_cancelled = Arc::new(AtomicBool::new(false));
    let child_cancelled = Arc::new(AtomicBool::new(false));
    let plan_requested = Arc::new(Notify::new());
    let grandchild_started_m = grandchild_started.clone();
    let grandchild_cancelled_m = grandchild_cancelled.clone();
    let child_cancelled_m = child_cancelled.clone();
    let plan_requested_m = plan_requested.clone();

    struct CancelDropGuard<'a> {
        cancel: &'a dyn Cancellation,
        flag: &'a AtomicBool,
    }
    impl Drop for CancelDropGuard<'_> {
        fn drop(&mut self) {
            if self.cancel.is_cancelled() {
                self.flag.store(true, Ordering::Release);
            }
        }
    }

    struct TreeModel {
        grandchild_started: Arc<Notify>,
        grandchild_cancelled: Arc<AtomicBool>,
        child_cancelled: Arc<AtomicBool>,
        plan_requested: Arc<Notify>,
    }
    impl Model for TreeModel {
        fn start<'a>(
            &'a self,
            request: &'a ModelRequest,
            cancel: &'a dyn Cancellation,
        ) -> Operation<'a, Box<dyn ModelStream>> {
            Box::pin(async move {
                let first = first_user_task(&request.history);
                if first == "GRAND_MUTATING" {
                    // Hop 2 (grandchild): block until the chain cancels us.
                    let _guard = CancelDropGuard {
                        cancel,
                        flag: &self.grandchild_cancelled,
                    };
                    self.grandchild_started.notify_one();
                    cancel.cancelled().await;
                    return Err(BoundaryError::new(error_codes::CANCELLED, "plan cancelled"));
                }
                if first == "CHILD_SPAWNER" {
                    // Hop 1 (child): spawn the grandchild in the background,
                    // then block until the chain cancels us.
                    let _guard = CancelDropGuard {
                        cancel,
                        flag: &self.child_cancelled,
                    };
                    let spawned = request.history.iter().any(|item| {
                        matches!(item, HistoryItem::ToolResult { call_id, .. } if call_id == "c1")
                    });
                    if !spawned {
                        return Ok(Box::new(VecStream(
                            finish_tool_calls(
                                vec![(
                                    "c1",
                                    "subagent_spawn",
                                    json!({
                                        "label": "grandchild",
                                        "task": "GRAND_MUTATING",
                                        "workspace_isolation": "isolated",
                                        "background": true
                                    }),
                                )],
                                10,
                                5,
                            )
                            .into(),
                        )) as Box<dyn ModelStream>);
                    }
                    cancel.cancelled().await;
                    return Err(BoundaryError::new(error_codes::CANCELLED, "plan cancelled"));
                }
                // Top-level parent: spawn the chain root in the background,
                // then wait for the external PLAN request, then finish.
                let spawned = request.history.iter().any(|item| {
                    matches!(item, HistoryItem::ToolResult { call_id, .. } if call_id == "p1")
                });
                if !spawned {
                    return Ok(Box::new(VecStream(
                        finish_tool_calls(
                            vec![(
                                "p1",
                                "subagent_spawn",
                                json!({
                                    "label": "top-child",
                                    "task": "CHILD_SPAWNER",
                                    "workspace_isolation": "isolated",
                                    "background": true
                                }),
                            )],
                            10,
                            5,
                        )
                        .into(),
                    )) as Box<dyn ModelStream>);
                }
                self.plan_requested.notified().await;
                Ok(Box::new(VecStream(
                    finish_text("verified subtree cancelled", 10, 5).into(),
                )) as Box<dyn ModelStream>)
            })
        }
    }

    let model = Arc::new(TreeModel {
        grandchild_started: grandchild_started_m,
        grandchild_cancelled: grandchild_cancelled_m,
        child_cancelled: child_cancelled_m,
        plan_requested: plan_requested_m,
    });

    let store = Arc::new(RecordingStore::new());
    let settings = SubagentSettings {
        enabled: true,
        max_depth: Some(3),
        ..Default::default()
    };
    let (mut runtime, mut session) = run_with_session(TestRunSpec {
        workspace: dir.path(),
        mode: WorkMode::Code,
        permissions: PermissionMode::Bypass,
        perm_cfg: PermissionConfig::default(),
        model,
        store,
        cancel: Arc::new(NeverCancel),
        confirmation: None,
        settings,
        limits: RuntimeLimits::default(),
        known_secrets: Vec::new(),
        task: "PARENT_PLAN_TREE",
    });

    let handle = runtime.handle();
    let mode_check = handle.clone();
    tokio::spawn(async move {
        grandchild_started.notified().await;
        let _ = handle.request_mode(WorkMode::Plan);
        plan_requested.notify_waiters();
    });

    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert!(
        child_cancelled.load(Ordering::Acquire),
        "child was not cancelled"
    );
    assert!(
        grandchild_cancelled.load(Ordering::Acquire),
        "grandchild two levels down was not cancelled by the PLAN transition"
    );
    assert_eq!(mode_check.mode().unwrap().current, WorkMode::Plan);
}
