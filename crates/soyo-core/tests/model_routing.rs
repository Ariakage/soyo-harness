use soyo_core::{
    approvals::Preauthorizations,
    boundary::{
        Cancellation, Model, ModelRequest, ModelStream, ModelUpdate, Operation, Store, StoreChange,
    },
    config::{
        MediaCapabilities, OpaqueHistoryPolicy, Pricing, ProfileCapabilities, RoleRoutingConfig,
        RoutingSettings,
    },
    contract::{
        BoundaryError, ErrorInfo, EventPayload, HistoryItem, ModelProtocol, PermissionMode, Run,
        RunId, RunStatus, Session, SessionId, ToolCall, Usage, WorkMode, error_codes,
    },
    media,
    metrics::RunMetrics,
    output_store::{OutputLimits, OutputStore},
    permissions::{PermissionConfig, Policy},
    provider::{
        router::ProfileRouter,
        routing::{ProfileCandidateMeta, ProfileHealthTracker, RoutingCoordinator},
    },
    review::ReviewConfig,
    runtime::{AgentRuntime, RuntimeLimits, RuntimeSetup},
    sqlite::SqliteStore,
    tools::{
        exec::ExecConfig,
        read::{ReadLimits, ReadTools},
    },
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::{Future, pending},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::tempdir;

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(pending())
    }
}

#[derive(Clone)]
enum MockStep {
    Complete {
        items: Vec<HistoryItem>,
        usage: Option<Usage>,
    },
    PartialThenFail {
        delta: String,
        code: &'static str,
    },
    Fail {
        code: &'static str,
        message: &'static str,
        retry_after: Option<Duration>,
    },
}

struct MockStream(VecDeque<Result<ModelUpdate, BoundaryError>>);
impl ModelStream for MockStream {
    fn next<'a>(&'a mut self, _: &'a dyn Cancellation) -> Operation<'a, Option<ModelUpdate>> {
        Box::pin(async move {
            match self.0.pop_front() {
                Some(Ok(update)) => Ok(Some(update)),
                Some(Err(err)) => Err(err),
                None => Ok(None),
            }
        })
    }
}

struct ScriptedProfileModel {
    steps: Mutex<VecDeque<MockStep>>,
    seen: Mutex<Vec<ModelRequest>>,
}

impl ScriptedProfileModel {
    fn new(steps: Vec<MockStep>) -> Self {
        Self {
            steps: Mutex::new(VecDeque::from(steps)),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl Model for ScriptedProfileModel {
    fn start<'a>(
        &'a self,
        request: &'a ModelRequest,
        _: &'a dyn Cancellation,
    ) -> Operation<'a, Box<dyn ModelStream>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(request.clone());
            let step = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(MockStep::Complete {
                    items: vec![HistoryItem::AssistantText("fallback default".into())],
                    usage: None,
                });
            match step {
                MockStep::Complete { items, usage } => {
                    let mut q = VecDeque::new();
                    q.push_back(Ok(ModelUpdate::Finished { items, usage }));
                    Ok(Box::new(MockStream(q)) as Box<dyn ModelStream>)
                }
                MockStep::PartialThenFail { delta, code } => {
                    let mut q = VecDeque::new();
                    q.push_back(Ok(ModelUpdate::TextDelta(delta)));
                    q.push_back(Err(BoundaryError::new(code, "stream failed mid-response")));
                    Ok(Box::new(MockStream(q)) as Box<dyn ModelStream>)
                }
                MockStep::Fail {
                    code,
                    message,
                    retry_after,
                } => Err(BoundaryError(
                    ErrorInfo {
                        code: code.into(),
                        message: message.into(),
                    },
                    retry_after,
                    None,
                )),
            }
        })
    }
}

struct RecordingStore {
    inner: SqliteStore,
    changes: Mutex<Vec<StoreChange>>,
}

impl RecordingStore {
    fn open(dir: &std::path::Path) -> Arc<Self> {
        Arc::new(Self {
            inner: SqliteStore::open(dir, vec![]).unwrap(),
            changes: Mutex::new(Vec::new()),
        })
    }

    fn routed_events(&self) -> Vec<EventPayload> {
        self.changes
            .lock()
            .unwrap()
            .iter()
            .filter_map(|change| match change {
                StoreChange::Activity { payload, .. }
                    if matches!(payload, EventPayload::ModelRouted { .. }) =>
                {
                    Some(payload.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn metrics(&self) -> Option<RunMetrics> {
        self.changes
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|change| match change {
                StoreChange::RunMetrics { metrics, .. } => Some(metrics.clone()),
                _ => None,
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

fn meta(
    name: &str,
    protocol: ModelProtocol,
    window: u64,
    tools: bool,
    image: bool,
    reasoning: bool,
    rate_in: u64,
) -> ProfileCandidateMeta {
    ProfileCandidateMeta {
        name: name.into(),
        protocol,
        model: format!("{name}-model"),
        context_window_tokens: window,
        max_output_tokens: 4096,
        multimodal: MediaCapabilities {
            image,
            audio: false,
            video: false,
        },
        capabilities: ProfileCapabilities {
            tools,
            reasoning,
            task_types: BTreeSet::new(),
        },
        pricing: Some(Pricing {
            currency: "USD".into(),
            input_microunits_per_million: rate_in,
            output_microunits_per_million: rate_in * 2,
        }),
    }
}

async fn setup_session(store: &RecordingStore, _workspace: &std::path::Path, session: &Session) {
    store.inner.create_session(session, "fp").await.unwrap();
}

fn build_test_runtime(
    workspace: &std::path::Path,
    store: Arc<dyn Store>,
    router: Arc<dyn Model>,
    session: &Session,
    review_profile: Option<&str>,
) -> AgentRuntime {
    let policy = Arc::new(Policy::new(workspace, PermissionConfig::default()).unwrap());
    let files = Arc::new(ReadTools::new(workspace, &[], false, ReadLimits::default()).unwrap());
    let mut review = ReviewConfig::new(session.profile.clone());
    review.review_profile = review_profile.map(str::to_owned);
    review.retry_jitter_max = Duration::ZERO;
    AgentRuntime::new(RuntimeSetup {
        run: Run {
            id: RunId::new(),
            session_id: session.id,
            work_mode: session.work_mode,
            permissions: PermissionMode::Bypass,
        },
        model: router,
        store,
        policy,
        files,
        cancellation: Arc::new(Never),
        review,
        exec: ExecConfig::default(),
        preauthorizations: Preauthorizations::default(),
        known_secrets: vec![],
        summary: "model routing test".into(),
        confirmation: None,
        limits: RuntimeLimits {
            retry_jitter_max: Duration::ZERO,
            ..Default::default()
        },
    })
    .unwrap()
}

#[tokio::test]
async fn disabled_routing_preserves_zero_drift_and_never_switches_profile() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = std::fs::canonicalize(&workspace).unwrap();
    let store = RecordingStore::open(&tmp.path().join("data"));

    let primary = Arc::new(ScriptedProfileModel::new(vec![
        MockStep::Fail {
            code: error_codes::RATE_LIMITED,
            message: "429",
            retry_after: Some(Duration::ZERO),
        },
        MockStep::Fail {
            code: error_codes::RATE_LIMITED,
            message: "429",
            retry_after: Some(Duration::ZERO),
        },
        MockStep::Fail {
            code: error_codes::RATE_LIMITED,
            message: "429",
            retry_after: Some(Duration::ZERO),
        },
    ]));
    let backup = Arc::new(ScriptedProfileModel::new(vec![MockStep::Complete {
        items: vec![HistoryItem::AssistantText("backup answered".into())],
        usage: None,
    }]));

    let coordinator = Arc::new(RoutingCoordinator::disabled(BTreeMap::from([
        (
            "primary".into(),
            meta(
                "primary",
                ModelProtocol::Responses,
                128_000,
                true,
                false,
                false,
                100,
            ),
        ),
        (
            "backup".into(),
            meta(
                "backup",
                ModelProtocol::ChatCompletions,
                128_000,
                true,
                false,
                false,
                50,
            ),
        ),
    ])));

    let router = Arc::new(
        ProfileRouter::new(
            "primary".into(),
            BTreeMap::from([
                ("primary".into(), primary.clone() as Arc<dyn Model>),
                ("backup".into(), backup.clone() as Arc<dyn Model>),
            ]),
        )
        .unwrap()
        .with_routing(coordinator),
    );

    let mut session = Session {
        id: SessionId::new(),
        workspace: workspace.clone(),
        work_mode: WorkMode::Code,
        profile: "primary".into(),
        protocol: ModelProtocol::Responses,
        model: "primary-model".into(),
        history: vec![HistoryItem::UserText("hello".into())],
    };
    setup_session(&store, &workspace, &session).await;

    let mut runtime = build_test_runtime(&workspace, store.clone(), router, &session, None);
    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().code, error_codes::RATE_LIMITED);
    assert_eq!(primary.requests().len(), 3);
    assert!(backup.requests().is_empty());
    assert!(store.routed_events().is_empty());
}

#[tokio::test]
async fn role_separation_capability_filtering_and_audit_events() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = std::fs::canonicalize(&workspace).unwrap();
    let img_path = workspace.join("shot.png");
    std::fs::write(&img_path, b"\x89PNG\r\n\x1a\n").unwrap();
    let store = RecordingStore::open(&tmp.path().join("data"));
    let output_store = Arc::new(
        OutputStore::open(
            tmp.path().join("outputs"),
            OutputLimits::default(),
            Default::default(),
        )
        .unwrap(),
    );

    let fast = Arc::new(ScriptedProfileModel::new(vec![]));
    let planner = Arc::new(ScriptedProfileModel::new(vec![MockStep::Complete {
        items: vec![HistoryItem::AssistantText("planned cleanly".into())],
        usage: Some(Usage {
            input_tokens: Some(120),
            output_tokens: Some(30),
            cached_input_tokens: None,
            estimated: false,
        }),
    }]));
    let vision = Arc::new(ScriptedProfileModel::new(vec![MockStep::Complete {
        items: vec![HistoryItem::AssistantText("vision inspected image".into())],
        usage: Some(Usage {
            input_tokens: Some(250),
            output_tokens: Some(40),
            cached_input_tokens: None,
            estimated: false,
        }),
    }]));

    let settings = RoutingSettings {
        enabled: true,
        roles: RoleRoutingConfig {
            execution: vec!["fast".into(), "vision".into()],
            planning: vec!["planner".into()],
            plan: vec!["planner".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    let candidates = BTreeMap::from([
        (
            "fast".into(),
            meta(
                "fast",
                ModelProtocol::ChatCompletions,
                128_000,
                true,
                false,
                false,
                100,
            ),
        ),
        (
            "planner".into(),
            meta(
                "planner",
                ModelProtocol::Responses,
                128_000,
                true,
                false,
                true,
                500,
            ),
        ),
        (
            "vision".into(),
            meta(
                "vision",
                ModelProtocol::Responses,
                128_000,
                true,
                true,
                false,
                300,
            ),
        ),
    ]);
    let coordinator = Arc::new(RoutingCoordinator::new(
        settings,
        candidates,
        Default::default(),
    ));
    let router = Arc::new(
        ProfileRouter::new(
            "fast".into(),
            BTreeMap::from([
                ("fast".into(), fast.clone() as Arc<dyn Model>),
                ("planner".into(), planner.clone() as Arc<dyn Model>),
                ("vision".into(), vision.clone() as Arc<dyn Model>),
            ]),
        )
        .unwrap()
        .with_routing(coordinator),
    );

    // 1. PLAN mode selects `planner` via `Planning` role.
    let mut plan_session = Session {
        id: SessionId::new(),
        workspace: workspace.clone(),
        work_mode: WorkMode::Plan,
        profile: "fast".into(),
        protocol: ModelProtocol::ChatCompletions,
        model: "fast-model".into(),
        history: vec![HistoryItem::UserText("design architecture".into())],
    };
    setup_session(&store, &workspace, &plan_session).await;
    let mut runtime = build_test_runtime(
        &workspace,
        store.clone(),
        router.clone(),
        &plan_session,
        None,
    );
    let result = runtime.run(&mut plan_session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(planner.requests().len(), 1);
    assert!(fast.requests().is_empty());

    // 2. CODE mode with image excludes `fast` (`missing_image_capability`) and selects `vision`.
    let sid = SessionId::new();
    let capture = media::capture_user(
        &format!("check [Image 1](<{}>)", img_path.display()),
        MediaCapabilities {
            image: true,
            audio: false,
            video: false,
        },
        &output_store,
        sid,
    )
    .unwrap();
    let mut code_session = Session {
        id: sid,
        workspace: workspace.clone(),
        work_mode: WorkMode::Code,
        profile: "fast".into(),
        protocol: ModelProtocol::ChatCompletions,
        model: "fast-model".into(),
        history: vec![HistoryItem::UserText(capture.text)],
    };
    setup_session(&store, &workspace, &code_session).await;
    let mut runtime2 = build_test_runtime(&workspace, store.clone(), router, &code_session, None)
        .with_media_store(
            output_store,
            MediaCapabilities {
                image: false,
                audio: false,
                video: false,
            },
        );
    let result2 = runtime2.run(&mut code_session).await.unwrap();
    assert_eq!(result2.status, RunStatus::Completed);
    assert_eq!(vision.requests().len(), 1);
    assert!(!vision.requests()[0].media.is_empty());

    let metrics_role = store.metrics().unwrap();
    assert_eq!(metrics_role.by_profile["vision"].execution_requests, 1);
    assert!(
        metrics_role.by_profile["vision"]
            .cost
            .as_ref()
            .unwrap()
            .microunits
            > 0
    );

    let routed = store.routed_events();
    assert_eq!(routed.len(), 2);
    let EventPayload::ModelRouted {
        profile, excluded, ..
    } = &routed[1]
    else {
        panic!("expected ModelRouted");
    };
    assert_eq!(profile, "vision");
    assert_eq!(excluded, &vec!["fast:missing_image_capability".to_owned()]);
}

#[tokio::test]
async fn failover_preserves_executed_tools_tracks_per_profile_cost_and_stops_on_partial() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = std::fs::canonicalize(&workspace).unwrap();
    std::fs::write(workspace.join("notes.txt"), "hello world").unwrap();
    let store = RecordingStore::open(&tmp.path().join("data"));

    // Turn 1: primary issues a real `read_file` tool call and completes turn 1.
    // Turn 2: primary fails with transient 429 BEFORE streaming any bytes.
    // Failover routes turn 2 to `backup` cleanly, receiving the executed tool result
    // without re-executing `read_file`.
    let primary = Arc::new(ScriptedProfileModel::new(vec![
        MockStep::Complete {
            items: vec![HistoryItem::ToolCall(ToolCall {
                call_id: "call_read_1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"notes.txt","start_line":1,"line_count":10}"#.into(),
            })],
            usage: Some(Usage {
                input_tokens: Some(1_000_000),
                output_tokens: Some(0),
                cached_input_tokens: None,
                estimated: false,
            }),
        },
        MockStep::Fail {
            code: error_codes::RATE_LIMITED,
            message: "429 transient",
            retry_after: Some(Duration::from_secs(60)),
        },
        // Used in second run below: partial then transient error -> must NOT fail over!
        MockStep::PartialThenFail {
            delta: "partial text ".into(),
            code: error_codes::TRANSIENT_CONNECTION,
        },
    ]));
    let backup = Arc::new(ScriptedProfileModel::new(vec![MockStep::Complete {
        items: vec![HistoryItem::AssistantText("done after failover".into())],
        usage: Some(Usage {
            input_tokens: Some(500_000),
            output_tokens: Some(500_000),
            cached_input_tokens: None,
            estimated: false,
        }),
    }]));

    let settings = RoutingSettings {
        enabled: true,
        fallback_profiles: vec!["backup".into()],
        circuit_breaker_failures: 1,
        cooldown_seconds: 60,
        opaque_history: OpaqueHistoryPolicy::MigrateCompatible,
        ..Default::default()
    };
    let health = Arc::new(ProfileHealthTracker::default());
    let coordinator = Arc::new(RoutingCoordinator::new(
        settings,
        BTreeMap::from([
            (
                "primary".into(),
                meta(
                    "primary",
                    ModelProtocol::ChatCompletions,
                    128_000,
                    true,
                    false,
                    false,
                    1000,
                ),
            ),
            (
                "backup".into(),
                meta(
                    "backup",
                    ModelProtocol::AnthropicMessages,
                    128_000,
                    true,
                    false,
                    false,
                    200,
                ),
            ),
        ]),
        health.clone(),
    ));
    let router = Arc::new(
        ProfileRouter::new(
            "primary".into(),
            BTreeMap::from([
                ("primary".into(), primary.clone() as Arc<dyn Model>),
                ("backup".into(), backup.clone() as Arc<dyn Model>),
            ]),
        )
        .unwrap()
        .with_routing(coordinator),
    );

    let mut session = Session {
        id: SessionId::new(),
        workspace: workspace.clone(),
        work_mode: WorkMode::Code,
        profile: "primary".into(),
        protocol: ModelProtocol::ChatCompletions,
        model: "primary-model".into(),
        history: vec![HistoryItem::UserText("read notes and reply".into())],
    };
    setup_session(&store, &workspace, &session).await;
    let mut runtime = build_test_runtime(&workspace, store.clone(), router.clone(), &session, None);
    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.text.as_deref(), Some("done after failover"));

    // Verify backup saw the completed read_file result verbatim and only ran once (no tool replay).
    let backup_reqs = backup.requests();
    assert_eq!(backup_reqs.len(), 1);
    assert!(backup_reqs[0]
        .history
        .iter()
        .any(|item| matches!(item, HistoryItem::ToolResult { call_id, result } if call_id == "call_read_1" && result.ok)));

    // Verify per-profile usage/cost attribution and ModelRouted fallback metadata.
    let metrics = store.metrics().unwrap();
    assert_eq!(metrics.tool_executions, 1);
    assert_eq!(metrics.by_profile["primary"].execution_requests, 2);
    // primary had an incomplete failed request -> conservative cost is None rather than guessing.
    assert!(metrics.by_profile["primary"].cost.is_none());
    assert_eq!(metrics.by_profile["backup"].execution_requests, 1);
    assert_eq!(
        metrics.by_profile["backup"]
            .cost
            .as_ref()
            .unwrap()
            .microunits,
        300
    );
    let routed = store.routed_events();
    assert_eq!(routed.len(), 3);
    assert!(matches!(
        &routed[2],
        EventPayload::ModelRouted {
            profile,
            fallback_from: Some(from),
            ..
        } if profile == "backup" && from == "primary"
    ));

    // Cooldown must skip `primary` on next turn cleanly!
    assert!(health.cooldown_remaining("primary").is_some());

    // Now test `!received` rule: clear cooldown, let primary stream partial text then fail.
    // Runtime MUST NOT silently replay onto backup.
    health.record_success("primary");
    let mut partial_session = Session {
        id: SessionId::new(),
        workspace: workspace.clone(),
        work_mode: WorkMode::Code,
        profile: "primary".into(),
        protocol: ModelProtocol::ChatCompletions,
        model: "primary-model".into(),
        history: vec![HistoryItem::UserText("stream half".into())],
    };
    setup_session(&store, &workspace, &partial_session).await;
    let mut runtime2 =
        build_test_runtime(&workspace, store.clone(), router, &partial_session, None);
    let res2 = runtime2.run(&mut partial_session).await.unwrap();
    assert_eq!(res2.status, RunStatus::Failed);
    assert_eq!(res2.error.unwrap().code, error_codes::TRANSIENT_CONNECTION);
    assert_eq!(backup.requests().len(), 1);
    assert!(
        partial_session
            .history
            .iter()
            .any(|item| matches!(item, HistoryItem::AssistantPartial(t) if t == "partial text "))
    );
}

#[tokio::test]
async fn opaque_history_policy_and_cost_budget_gating_fail_closed() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = std::fs::canonicalize(&workspace).unwrap();
    let store = RecordingStore::open(&tmp.path().join("data"));

    let history_with_opaque = vec![
        HistoryItem::UserText("step 1".into()),
        HistoryItem::ProviderItem {
            protocol: ModelProtocol::Responses,
            value: serde_json::json!({"type":"reasoning","encrypted_content":"secret_state"}),
        },
        HistoryItem::ToolCall(ToolCall {
            call_id: "c1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"a"}"#.into(),
        }),
        HistoryItem::ToolResult {
            call_id: "c1".into(),
            result: soyo_core::contract::ToolResult::success(Some(serde_json::json!({"ok":true}))),
        },
        HistoryItem::UserText("continue".into()),
    ];

    for (policy, expected_code) in [
        (
            OpaqueHistoryPolicy::Reject,
            error_codes::HISTORY_MIGRATION_REQUIRED,
        ),
        (
            OpaqueHistoryPolicy::RequireNewSession,
            error_codes::NEW_SESSION_REQUIRED,
        ),
    ] {
        let primary = Arc::new(ScriptedProfileModel::new(vec![MockStep::Fail {
            code: error_codes::RATE_LIMITED,
            message: "primary unavailable",
            retry_after: Some(Duration::from_secs(60)),
        }]));
        let backup = Arc::new(ScriptedProfileModel::new(vec![]));
        let mut settings = RoutingSettings {
            enabled: true,
            fallback_profiles: vec!["backup".into()],
            opaque_history: policy,
            circuit_breaker_failures: 1,
            cooldown_seconds: 60,
            ..Default::default()
        };
        settings.roles.execution = vec!["backup".into()];
        let health = Arc::new(ProfileHealthTracker::default());
        health.record_transient_failure(
            "primary",
            error_codes::RATE_LIMITED,
            Some(Duration::from_secs(60)),
            &settings,
        );
        let coordinator = Arc::new(RoutingCoordinator::new(
            settings,
            BTreeMap::from([
                (
                    "primary".into(),
                    meta(
                        "primary",
                        ModelProtocol::Responses,
                        128_000,
                        true,
                        false,
                        false,
                        100,
                    ),
                ),
                (
                    "backup".into(),
                    meta(
                        "backup",
                        ModelProtocol::AnthropicMessages,
                        128_000,
                        true,
                        false,
                        false,
                        100,
                    ),
                ),
            ]),
            health,
        ));
        let router = Arc::new(
            ProfileRouter::new(
                "primary".into(),
                BTreeMap::from([
                    ("primary".into(), primary as Arc<dyn Model>),
                    ("backup".into(), backup.clone() as Arc<dyn Model>),
                ]),
            )
            .unwrap()
            .with_routing(coordinator),
        );
        let mut session = Session {
            id: SessionId::new(),
            workspace: workspace.clone(),
            work_mode: WorkMode::Code,
            profile: "primary".into(),
            protocol: ModelProtocol::Responses,
            model: "primary-model".into(),
            history: history_with_opaque.clone(),
        };
        setup_session(&store, &workspace, &session).await;
        let mut runtime = build_test_runtime(&workspace, store.clone(), router, &session, None);
        let res = runtime.run(&mut session).await.unwrap();
        assert_eq!(res.status, RunStatus::Failed);
        assert_eq!(res.error.unwrap().code, expected_code);
        assert!(backup.requests().is_empty());
    }

    // MigrateCompatible strips opaque ProviderItem while keeping ToolCall + ToolResult verbatim
    // AND enforcing max_run_cost_microunits.
    let primary_fail = Arc::new(ScriptedProfileModel::new(vec![]));
    let backup = Arc::new(ScriptedProfileModel::new(vec![MockStep::Complete {
        items: vec![HistoryItem::AssistantText("migrated ok".into())],
        usage: None,
    }]));
    let settings = RoutingSettings {
        enabled: true,
        opaque_history: OpaqueHistoryPolicy::MigrateCompatible,
        max_run_cost_microunits: Some(10),
        cost_currency: Some("USD".into()),
        circuit_breaker_failures: 1,
        cooldown_seconds: 60,
        roles: RoleRoutingConfig {
            execution: vec!["expensive".into(), "backup".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    let health = Arc::new(ProfileHealthTracker::default());
    health.record_transient_failure(
        "primary",
        error_codes::RATE_LIMITED,
        Some(Duration::from_secs(60)),
        &settings,
    );
    let coordinator = Arc::new(RoutingCoordinator::new(
        settings,
        BTreeMap::from([
            (
                "primary".into(),
                meta(
                    "primary",
                    ModelProtocol::Responses,
                    128_000,
                    true,
                    false,
                    false,
                    100,
                ),
            ),
            (
                "expensive".into(),
                meta(
                    "expensive",
                    ModelProtocol::AnthropicMessages,
                    128_000,
                    true,
                    false,
                    false,
                    10_000_000,
                ),
            ),
            (
                "backup".into(),
                meta(
                    "backup",
                    ModelProtocol::AnthropicMessages,
                    128_000,
                    true,
                    false,
                    false,
                    100,
                ),
            ),
        ]),
        health,
    ));
    let router = Arc::new(
        ProfileRouter::new(
            "primary".into(),
            BTreeMap::from([
                ("primary".into(), primary_fail as Arc<dyn Model>),
                (
                    "expensive".into(),
                    Arc::new(ScriptedProfileModel::new(vec![])) as Arc<dyn Model>,
                ),
                ("backup".into(), backup.clone() as Arc<dyn Model>),
            ]),
        )
        .unwrap()
        .with_routing(coordinator),
    );
    let mut session = Session {
        id: SessionId::new(),
        workspace: workspace.clone(),
        work_mode: WorkMode::Code,
        profile: "primary".into(),
        protocol: ModelProtocol::Responses,
        model: "primary-model".into(),
        history: history_with_opaque,
    };
    setup_session(&store, &workspace, &session).await;
    let mut runtime = build_test_runtime(&workspace, store.clone(), router, &session, None);
    let result = runtime.run(&mut session).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    let sent_history = &backup.requests()[0].history;
    assert!(
        !sent_history
            .iter()
            .any(|item| matches!(item, HistoryItem::ProviderItem { .. }))
    );
    assert!(sent_history
        .iter()
        .any(|item| matches!(item, HistoryItem::ToolResult { call_id, result } if call_id == "c1" && result.ok)));
}
