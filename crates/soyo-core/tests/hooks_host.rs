use serde_json::json;
use soyo_core::{
    approvals::Preauthorizations,
    boundary::{
        Cancellation, Model, ModelRequest, ModelStream, ModelUpdate, Operation, Store, StoreChange,
    },
    compaction::{
        CompactionConfig, EvidenceLevel, Fact, FactStatus, MandatoryItem, MandatoryKind,
        StructuredSummary,
    },
    config::{Environment, HookSettings, HostHookRule},
    context_builder::{ContextBudget, ItemRange},
    contract::{
        ApprovalRequest, HistoryItem, ModelProtocol, PermissionMode, Run, RunId, Session,
        SessionId, ToolCall, WorkMode,
    },
    credentials::SecretRegistry,
    execution::{BackendKind, SandboxPolicy},
    hook_store::{HookStore, HookStoreLimits},
    input::InputHandler,
    memory::MemoryStore,
    permissions::{PermissionConfig, Policy},
    review::ReviewConfig,
    runtime::{AgentRuntime, ApprovalHandler, RuntimeLimits, RuntimeSetup},
    tools::{
        exec::ExecConfig,
        read::{ReadLimits, ReadTools},
    },
};
use std::{
    collections::BTreeMap,
    future::{Future, pending},
    os::unix::fs::PermissionsExt,
    path::Path,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(pending())
    }
}

#[derive(Default)]
struct RecordingStore {
    inner: MemoryStore,
    changes: Mutex<Vec<String>>,
}
impl Store for RecordingStore {
    fn load_session(&self, id: SessionId) -> Operation<'_, Option<Session>> {
        self.inner.load_session(id)
    }
    fn context_checkpoint(
        &self,
        id: SessionId,
    ) -> Operation<'_, Option<soyo_core::context_store::ContextCheckpoint>> {
        self.inner.context_checkpoint(id)
    }
    fn commit<'a>(&'a self, changes: &'a [StoreChange]) -> Operation<'a, ()> {
        Box::pin(async move {
            {
                let mut log = self.changes.lock().unwrap();
                for c in changes {
                    match c {
                        StoreChange::ToolIntent { call, .. } => {
                            log.push(format!("intent:{}", call.name));
                        }
                        StoreChange::ToolFinished {
                            call_id, result, ..
                        } => {
                            log.push(format!("finished:{call_id}:{result:?}"));
                        }
                        StoreChange::Activity { payload, .. } => {
                            log.push(format!("activity:{payload:?}"));
                        }
                        _ => {}
                    }
                }
            }
            self.inner.commit(changes).await
        })
    }
}

struct SequenceModel {
    turns: Mutex<Vec<Vec<HistoryItem>>>,
}
struct OneShot(Option<ModelUpdate>);
impl ModelStream for OneShot {
    fn next<'a>(&'a mut self, _: &'a dyn Cancellation) -> Operation<'a, Option<ModelUpdate>> {
        Box::pin(async move { Ok(self.0.take()) })
    }
}
impl Model for SequenceModel {
    fn start<'a>(
        &'a self,
        request: &'a ModelRequest,
        _: &'a dyn Cancellation,
    ) -> Operation<'a, Box<dyn ModelStream>> {
        Box::pin(async move {
            if matches!(
                request.purpose,
                soyo_core::contract::ModelPurpose::Compaction
            ) {
                let HistoryItem::UserText(text) = &request.history[0] else {
                    panic!("expected summary data carrier");
                };
                let data: serde_json::Value = serde_json::from_str(text).unwrap();
                let range: ItemRange =
                    serde_json::from_value(data["assigned_source_range"].clone()).unwrap();
                let mut summary = StructuredSummary {
                    schema_version: 1,
                    covered_range: range,
                    objective: vec![],
                    constraints: vec![],
                    decisions: vec![],
                    completed: vec![],
                    remaining: vec![],
                    uncertainties: vec![],
                    required_ids: vec![],
                    references: vec![],
                };
                let mut source_ids = Vec::new();
                for unit in data["source_units"].as_array().unwrap() {
                    for source in unit.as_array().unwrap() {
                        if source["role"] == "user" {
                            source_ids.push(source["source_id"].as_str().unwrap().into());
                        }
                    }
                }
                if !source_ids.is_empty() {
                    summary.objective.push(Fact {
                        text: "User task active".into(),
                        source_ids,
                        evidence: EvidenceLevel::UserRequest,
                        status: FactStatus::Active,
                    });
                }
                let required: Vec<MandatoryItem> =
                    serde_json::from_value(data["required_items"].clone()).unwrap();
                for item in required {
                    let fact = Fact {
                        text: item.text,
                        source_ids: item.source_ids,
                        evidence: EvidenceLevel::UserRequest,
                        status: item.status,
                    };
                    match item.kind {
                        MandatoryKind::Objective => summary.objective.push(fact),
                        MandatoryKind::Constraint => summary.constraints.push(fact),
                        MandatoryKind::Remaining => summary.remaining.push(fact),
                        MandatoryKind::Recovery => summary.uncertainties.push(fact),
                    }
                    summary.required_ids.push(item.id);
                }
                return Ok(Box::new(OneShot(Some(ModelUpdate::Finished {
                    items: vec![HistoryItem::AssistantText(
                        serde_json::to_string(&summary).unwrap(),
                    )],
                    usage: None,
                }))) as Box<dyn ModelStream>);
            }
            let mut turns = self.turns.lock().unwrap();
            let items = if turns.is_empty() {
                vec![HistoryItem::AssistantText("done".into())]
            } else {
                turns.remove(0)
            };
            Ok(
                Box::new(OneShot(Some(ModelUpdate::Finished { items, usage: None })))
                    as Box<dyn ModelStream>,
            )
        })
    }
}

struct FixedInput;
impl InputHandler for FixedInput {
    fn ask<'a>(
        &'a self,
        _: &'a soyo_core::input::InputRequest,
        _: &'a dyn Cancellation,
    ) -> Operation<'a, Option<String>> {
        Box::pin(async move { Ok(Some("yes".into())) })
    }
}

struct CountApproval {
    count: AtomicUsize,
    allow: bool,
}
impl ApprovalHandler for CountApproval {
    fn confirm<'a>(
        &'a self,
        _: &'a ApprovalRequest,
        _: &'a dyn Cancellation,
    ) -> Operation<'a, Option<bool>> {
        Box::pin(async move {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(Some(self.allow))
        })
    }
}

fn write_hook_script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn build_runtime(
    workspace: &Path,
    state_dir: &Path,
    session: &Session,
    permissions: PermissionMode,
    model: Arc<SequenceModel>,
    confirmation: Option<Arc<dyn ApprovalHandler>>,
    hooks: HookSettings,
) -> (AgentRuntime, Arc<RecordingStore>) {
    let environment = Environment::from_pairs(std::env::vars_os());
    let store = Arc::new(RecordingStore::default());
    let registry = Arc::new(SecretRegistry::default());
    let exec = ExecConfig {
        environment: environment.clone(),
        ..ExecConfig::default()
    };
    let setup = RuntimeSetup {
        run: Run {
            id: RunId::new(),
            session_id: session.id,
            work_mode: session.work_mode,
            permissions,
        },
        model,
        store: store.clone(),
        policy: Arc::new(Policy::new(workspace, PermissionConfig::default()).unwrap()),
        files: Arc::new(
            ReadTools::new(
                workspace,
                &[],
                permissions == PermissionMode::Bypass,
                ReadLimits::default(),
            )
            .unwrap(),
        ),
        cancellation: Arc::new(Never),
        review: ReviewConfig::new("fixture".into()),
        exec,
        preauthorizations: Preauthorizations::default(),
        known_secrets: Vec::new(),
        summary: "host-hooks-fixture".into(),
        confirmation,
        limits: RuntimeLimits {
            active_time: Duration::from_secs(30),
            model_connect: Duration::from_secs(3),
            model_idle: Duration::from_secs(3),
            model_total: Duration::from_secs(3),
            tool_timeout: Duration::from_secs(5),
            ..RuntimeLimits::default()
        },
    };
    let mut sandbox = SandboxPolicy::workspace(workspace.into());
    sandbox.backend = BackendKind::Host;
    sandbox.protected_paths.clear();
    std::fs::create_dir_all(state_dir).unwrap();
    std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let journal = Arc::new(
        HookStore::open(
            &state_dir.join("journal"),
            registry.clone(),
            HookStoreLimits::default(),
        )
        .unwrap(),
    );
    let runtime = AgentRuntime::new(setup)
        .unwrap()
        .with_secret_registry(registry)
        .unwrap()
        .with_execution_settings(sandbox, None)
        .unwrap()
        .with_host_hooks(hooks, environment, journal)
        .unwrap()
        .with_input_handler(Arc::new(FixedInput));
    (runtime, store)
}

fn new_session(workspace: &Path, work_mode: WorkMode) -> Session {
    Session {
        id: SessionId::new(),
        workspace: workspace.into(),
        work_mode,
        profile: "fixture".into(),
        protocol: ModelProtocol::ChatCompletions,
        model: "fixture".into(),
        history: Vec::new(),
    }
}

#[tokio::test]
async fn host_hooks_fire_extended_events_with_filters_and_veto() {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let workspace = base.join("workspace");
    let state_dir = base.join("state");
    std::fs::create_dir_all(&workspace).unwrap();

    let log = workspace.join("events.log");
    let obs = base.join("obs.sh");
    write_hook_script(
        &obs,
        &format!(
            "#!/bin/sh\nIFS= read -r payload || :\nprintf '%s\\n' \"$1\" >> \"{}\"\nprintf '{{\"annotations\":[]}}'\n",
            log.display()
        ),
    );
    let tool_script = base.join("only-todo.sh");
    write_hook_script(
        &tool_script,
        &format!(
            "#!/bin/sh\nIFS= read -r payload || :\nprintf 'tool-todo\\n' >> \"{}\"\nprintf '{{\"annotations\":[]}}'\n",
            log.display()
        ),
    );

    let hooks = HookSettings {
        enabled: true,
        rules: vec![
            HostHookRule {
                id: "m-before".into(),
                event: "model.before_request".into(),
                kind: "observer".into(),
                argv: vec![obs.to_string_lossy().into(), "model.before_request".into()],
                tools: Vec::new(),
                modes: vec![WorkMode::Work],
                timeout_ms: 2000,
                env: BTreeMap::new(),
            },
            HostHookRule {
                id: "m-finished".into(),
                event: "model.finished".into(),
                kind: "observer".into(),
                argv: vec![obs.to_string_lossy().into(), "model.finished".into()],
                tools: Vec::new(),
                modes: Vec::new(),
                timeout_ms: 2000,
                env: BTreeMap::new(),
            },
            HostHookRule {
                id: "in-req".into(),
                event: "input.required".into(),
                kind: "observer".into(),
                argv: vec![obs.to_string_lossy().into(), "input.required".into()],
                tools: Vec::new(),
                modes: Vec::new(),
                timeout_ms: 2000,
                env: BTreeMap::new(),
            },
            HostHookRule {
                id: "only-todo-tool".into(),
                event: "tool.before_dispatch".into(),
                kind: "observer".into(),
                argv: vec![tool_script.to_string_lossy().into()],
                tools: vec!["todo_write".into()],
                modes: Vec::new(),
                timeout_ms: 2000,
                env: BTreeMap::new(),
            },
        ],
    };

    let model = Arc::new(SequenceModel {
        turns: Mutex::new(vec![
            vec![HistoryItem::ToolCall(ToolCall {
                call_id: "c-ask".into(),
                name: "ask_user".into(),
                arguments: json!({"question": "Proceed?"}).to_string(),
            })],
            vec![HistoryItem::ToolCall(ToolCall {
                call_id: "c-todo".into(),
                name: "todo_write".into(),
                arguments: json!({"todos": [{"id": "t1", "content": "check hooks", "status": "completed"}]}).to_string(),
            })],
            vec![HistoryItem::AssistantText("all good".into())],
        ]),
    });

    let mut session = new_session(&workspace, WorkMode::Work);
    let (mut runtime, store1) = build_runtime(
        &workspace,
        &state_dir,
        &session,
        PermissionMode::Bypass,
        model,
        None,
        hooks,
    );
    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(
        result.status,
        soyo_core::contract::RunStatus::Completed,
        "unexpected result: {result:?}"
    );
    assert_eq!(result.text.as_deref(), Some("all good"));

    let recorded = std::fs::read_to_string(&log).unwrap_or_else(|e| {
        let log_events = store1.changes.lock().unwrap().join("\n");
        panic!(
            "log missing at {}: {e}\nstore changes:\n{log_events}",
            log.display()
        )
    });
    assert!(recorded.contains("model.before_request"), "{recorded}");
    assert!(recorded.contains("model.finished"), "{recorded}");
    assert!(recorded.contains("input.required"), "{recorded}");
    assert!(recorded.contains("tool-todo"), "{recorded}");

    // Test model.before_request check veto fails closed with hook_denied.
    let deny = base.join("deny.sh");
    write_hook_script(
        &deny,
        "#!/bin/sh\nIFS= read -r _ || :\nprintf '{\"decision\":\"deny\"}'\n",
    );
    let veto_hooks = HookSettings {
        enabled: true,
        rules: vec![HostHookRule {
            id: "veto-model".into(),
            event: "model.before_request".into(),
            kind: "check".into(),
            argv: vec![deny.to_string_lossy().into()],
            tools: Vec::new(),
            modes: Vec::new(),
            timeout_ms: 2000,
            env: BTreeMap::new(),
        }],
    };
    let mut session2 = new_session(&workspace, WorkMode::Work);
    let (mut runtime2, _) = build_runtime(
        &workspace,
        base.join("state2").as_path(),
        &session2,
        PermissionMode::Bypass,
        Arc::new(SequenceModel {
            turns: Mutex::new(vec![]),
        }),
        None,
        veto_hooks,
    );
    let res2 = runtime2.run(&mut session2).await.unwrap();
    assert_eq!(res2.status, soyo_core::contract::RunStatus::Failed);
    assert_eq!(
        res2.error.as_ref().map(|e| e.code.as_str()),
        Some("hook_denied"),
        "expected hook_denied, got {res2:?}"
    );

    // Disabled config executes zero hooks even with rules present.
    let marker = workspace.join("disabled.marker");
    let touch = base.join("touch.sh");
    write_hook_script(
        &touch,
        &format!("#!/bin/sh\ntouch \"{}\"\n", marker.display()),
    );
    let disabled_hooks = HookSettings {
        enabled: false,
        rules: vec![HostHookRule {
            id: "disabled-rule".into(),
            event: "run.started".into(),
            kind: "observer".into(),
            argv: vec![touch.to_string_lossy().into()],
            tools: Vec::new(),
            modes: Vec::new(),
            timeout_ms: 2000,
            env: BTreeMap::new(),
        }],
    };
    let mut session3 = new_session(&workspace, WorkMode::Work);
    let (mut runtime3, _) = build_runtime(
        &workspace,
        base.join("state3").as_path(),
        &session3,
        PermissionMode::Bypass,
        Arc::new(SequenceModel {
            turns: Mutex::new(vec![]),
        }),
        None,
        disabled_hooks,
    );
    runtime3.run(&mut session3).await.unwrap();
    assert!(!marker.exists());
}

#[tokio::test]
async fn host_hooks_respect_plan_and_manual_approval_and_fire_compaction_finished() {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let workspace = base.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let ran = workspace.join("plan-or-manual.log");
    let script = base.join("record.sh");
    write_hook_script(
        &script,
        &format!(
            "#!/bin/sh\nIFS= read -r _ || :\nprintf '%s\\n' \"$1\" >> \"{}\"\nprintf '{{\"annotations\":[]}}'\n",
            ran.display()
        ),
    );

    // WorkMode::Plan never starts observer hooks.
    let plan_hooks = HookSettings {
        enabled: true,
        rules: vec![HostHookRule {
            id: "obs-plan".into(),
            event: "run.started".into(),
            kind: "observer".into(),
            argv: vec![script.to_string_lossy().into(), "plan".into()],
            tools: Vec::new(),
            modes: Vec::new(),
            timeout_ms: 2000,
            env: BTreeMap::new(),
        }],
    };
    let mut plan_session = new_session(&workspace, WorkMode::Plan);
    let (mut plan_runtime, _) = build_runtime(
        &workspace,
        base.join("state-plan").as_path(),
        &plan_session,
        PermissionMode::Bypass,
        Arc::new(SequenceModel {
            turns: Mutex::new(vec![]),
        }),
        None,
        plan_hooks,
    );
    plan_runtime.run(&mut plan_session).await.unwrap();
    assert!(!ran.exists(), "PLAN must not execute observer hook scripts");

    // PermissionMode::Manual requires explicit approval before running host hook.
    let approval = Arc::new(CountApproval {
        count: AtomicUsize::new(0),
        allow: true,
    });
    let manual_hooks = HookSettings {
        enabled: true,
        rules: vec![HostHookRule {
            id: "obs-manual".into(),
            event: "run.started".into(),
            kind: "observer".into(),
            argv: vec![script.to_string_lossy().into(), "manual".into()],
            tools: Vec::new(),
            modes: Vec::new(),
            timeout_ms: 2000,
            env: BTreeMap::new(),
        }],
    };
    let mut manual_session = new_session(&workspace, WorkMode::Work);
    let (mut manual_runtime, _) = build_runtime(
        &workspace,
        base.join("state-manual").as_path(),
        &manual_session,
        PermissionMode::Manual,
        Arc::new(SequenceModel {
            turns: Mutex::new(vec![]),
        }),
        Some(approval.clone()),
        manual_hooks,
    );
    let manual_res = manual_runtime.run(&mut manual_session).await.unwrap();
    assert_eq!(
        manual_res.status,
        soyo_core::contract::RunStatus::Completed,
        "manual run result: {manual_res:?}"
    );
    assert_eq!(approval.count.load(Ordering::SeqCst), 1);

    // CompactionFinished fires when compaction triggers inside a run.
    let compact_hooks = HookSettings {
        enabled: true,
        rules: vec![HostHookRule {
            id: "on-compact".into(),
            event: "compaction.finished".into(),
            kind: "observer".into(),
            argv: vec![
                script.to_string_lossy().into(),
                "compaction.finished".into(),
            ],
            tools: Vec::new(),
            modes: Vec::new(),
            timeout_ms: 2000,
            env: BTreeMap::new(),
        }],
    };
    let mut compact_session = new_session(&workspace, WorkMode::Work);
    compact_session.history = (0..6)
        .flat_map(|i| {
            [
                HistoryItem::UserText(format!("prior message {i} {}", "x".repeat(200))),
                HistoryItem::AssistantText(format!("prior reply {i} {}", "y".repeat(200))),
            ]
        })
        .collect();
    let (compact_runtime, compact_store) = build_runtime(
        &workspace,
        base.join("state-compact").as_path(),
        &compact_session,
        PermissionMode::Bypass,
        Arc::new(SequenceModel {
            turns: Mutex::new(vec![]),
        }),
        None,
        compact_hooks,
    );
    let mut compact_runtime = compact_runtime
        .with_context_settings(
            ContextBudget::new(128_000, 2048, 12_801),
            true,
            CompactionConfig {
                summary_output_tokens: 200,
                max_model_calls: 2,
                operation_timeout: Duration::from_secs(5),
                ..CompactionConfig::default()
            },
        )
        .unwrap();
    compact_runtime.handle().request_compaction().unwrap();
    let compact_res = compact_runtime.run(&mut compact_session).await.unwrap();
    assert_eq!(
        compact_res.status,
        soyo_core::contract::RunStatus::Completed,
        "compact run result: {compact_res:?}"
    );
    let content = std::fs::read_to_string(&ran).unwrap_or_else(|e| {
        let log_events = compact_store.changes.lock().unwrap().join("\n");
        panic!(
            "ran log missing at {}: {e}\ncompact store changes:\n{log_events}",
            ran.display()
        )
    });
    assert!(content.contains("manual"), "{content}");
    assert!(content.contains("compaction.finished"), "{content}");
}
