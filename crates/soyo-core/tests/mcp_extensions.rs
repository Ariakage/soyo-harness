use serde_json::{Value, json};
use soyo_core::{
    approvals::Preauthorizations,
    boundary::{Cancellation, Model, ModelRequest, ModelStream, ModelUpdate, Operation},
    config::Environment,
    contract::{
        HistoryItem, ModelProtocol, PermissionMode, Run, RunId, RunStatus, Session, SessionId,
        ToolCall, WorkMode,
    },
    credentials::SecretRegistry,
    execution::{BackendKind, SandboxPolicy},
    mcp::{
        McpClient, McpElicitationDecision, McpElicitationHandler, McpElicitationRequest, McpLimits,
        McpPrompt, McpResourceContent,
        config::{
            McpLimitSettings, McpServerSettings, McpSettings, McpTransportSettings,
            StdioMcpSettings,
        },
    },
    memory::MemoryStore,
    permissions::{PermissionConfig, Policy},
    review::ReviewConfig,
    runtime::{AgentRuntime, RuntimeLimits, RuntimeSetup},
    tools::{
        exec::ExecConfig,
        read::{ReadLimits, ReadTools},
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::{Future, pending},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

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
struct ServerCfg {
    modern: bool,
    advertise_resources: bool,
    advertise_subscribe: bool,
    advertise_prompts: bool,
    repeat_cursor: bool,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            modern: true,
            advertise_resources: true,
            advertise_subscribe: true,
            advertise_prompts: true,
            repeat_cursor: false,
        }
    }
}

struct MockStdio {
    requests: mpsc::Receiver<Value>,
    to_client: mpsc::Sender<Value>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for MockStdio {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_mock(cfg: ServerCfg, limits: McpLimits) -> (McpClient, MockStdio) {
    spawn_mock_with_elicit(cfg, limits, None).await
}

async fn spawn_mock_with_elicit(
    cfg: ServerCfg,
    limits: McpLimits,
    elicit: Option<Arc<dyn McpElicitationHandler>>,
) -> (McpClient, MockStdio) {
    let (client, server) = tokio::io::duplex(2 * 1024 * 1024);
    let (cr, cw) = tokio::io::split(client);
    let (sr, mut sw) = tokio::io::split(server);
    let (req_tx, req_rx) = mpsc::channel(64);
    let (msg_tx, mut msg_rx) = mpsc::channel::<Value>(64);
    let task = tokio::spawn(async move {
        let mut lines = BufReader::new(sr).lines();
        let mut list_sub_id: Option<Value> = None;
        let mut uri_sub_id: Option<Value> = None;
        let server_caps = || {
            let mut caps = json!({"tools": {"listChanged": cfg.modern}});
            if cfg.advertise_resources {
                caps["resources"] = json!({
                    "subscribe": cfg.advertise_subscribe,
                    "listChanged": cfg.modern
                });
            }
            if cfg.advertise_prompts {
                caps["prompts"] = json!({"listChanged": cfg.modern});
            }
            caps
        };
        loop {
            let out: Option<Value> = tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break; };
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let _ = req_tx.send(req.clone()).await;
                    if req.get("id").is_none() {
                        continue;
                    }
                    let id = req["id"].clone();
                    let method = req["method"].as_str().unwrap_or("");
                    match method {
                        "initialize" => {
                            Some(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "protocolVersion": if cfg.modern { "2026-07-28" } else { "2025-11-25" },
                                    "capabilities": server_caps(),
                                    "serverInfo": {"name": "ext-fixture", "version": "1.0"}
                                }
                            }))
                        }
                        "server/discover" => {
                            if !cfg.modern {
                                Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"legacy"}}))
                            } else {
                                Some(json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "resultType": "complete",
                                        "supportedVersions": ["2026-07-28"],
                                        "capabilities": server_caps(),
                                        "ttlMs": 0,
                                        "cacheScope": "private"
                                    }
                                }))
                            }
                        }
                        "subscriptions/listen" => {
                            let requested = req["params"]["notifications"].clone();
                            if requested.get("resourceSubscriptions").is_some() {
                                uri_sub_id = Some(id.clone());
                            } else {
                                list_sub_id = Some(id.clone());
                            }
                            Some(json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/subscriptions/acknowledged",
                                "params": {
                                    "_meta": {"io.modelcontextprotocol/subscriptionId": id},
                                    "notifications": requested
                                }
                            }))
                        }
                        "resources/list" => {
                            let cursor = req["params"]["cursor"].as_str();
                            if cfg.repeat_cursor {
                                Some(json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {"resources": [], "nextCursor": "loop"}
                                }))
                            } else if cursor.is_none() {
                                Some(json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "resources": [{
                                            "uri": "file:///doc/a.txt",
                                            "name": "doc-a",
                                            "title": "Doc A",
                                            "description": "First doc",
                                            "mimeType": "text/plain",
                                            "size": 12
                                        }],
                                        "nextCursor": "p2"
                                    }
                                }))
                            } else {
                                Some(json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "resources": [{
                                            "uri": "file:///doc/b.bin",
                                            "name": "doc-b",
                                            "mimeType": "application/octet-stream"
                                        }]
                                    }
                                }))
                            }
                        }
                        "resources/templates/list" => {
                            Some(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "resourceTemplates": [{
                                        "uriTemplate": "file:///doc/{name}",
                                        "name": "doc-template",
                                        "description": "Parameterized doc",
                                        "mimeType": "text/plain"
                                    }]
                                }
                            }))
                        }
                        "resources/read" => {
                            let uri = req["params"]["uri"].as_str().unwrap_or("");
                            if uri == "file:///doc/huge" {
                                Some(json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "contents": [{
                                            "uri": uri,
                                            "mimeType": "text/plain",
                                            "text": "x".repeat(4096)
                                        }]
                                    }
                                }))
                            } else {
                                Some(json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "contents": [
                                            {"uri": uri, "mimeType": "text/plain", "text": "hello resource"},
                                            {"uri": "file:///doc/b.bin", "mimeType": "application/octet-stream", "blob": "AQID"}
                                        ]
                                    }
                                }))
                            }
                        }
                        "resources/subscribe" | "resources/unsubscribe" => {
                            Some(json!({"jsonrpc": "2.0", "id": id, "result": {}}))
                        }
                        "prompts/list" => {
                            Some(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "prompts": [{
                                        "name": "summarize",
                                        "title": "Summarize",
                                        "description": "Summarize text",
                                        "arguments": [{"name": "topic", "description": "Focus topic", "required": true}]
                                    }]
                                }
                            }))
                        }
                        "prompts/get" => {
                            let topic = req["params"]["arguments"]["topic"].as_str().unwrap_or("default");
                            Some(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "description": "Topic summary prompt",
                                    "messages": [
                                        {"role": "user", "content": {"type": "text", "text": format!("Focus on {topic}")}},
                                        {"role": "assistant", "content": {"type": "image", "data": "AAAA", "mimeType": "image/png"}}
                                    ]
                                }
                            }))
                        }
                        "tools/list" => {
                            Some(json!({"jsonrpc":"2.0","id":id,"result":{"tools":[]}}))
                        }
                        _ => Some(json!({"jsonrpc":"2.0","id":id,"result":{}})),
                    }
                }
                msg = msg_rx.recv() => {
                    match msg {
                        Some(mut msg) => {
                            if cfg.modern {
                                let target_id = if msg["method"] == "notifications/resources/updated" {
                                    uri_sub_id.clone()
                                } else {
                                    list_sub_id.clone()
                                };
                                if let Some(sub_id) = target_id {
                                    let params = msg["params"].as_object_mut();
                                    if let Some(obj) = params {
                                        obj.insert("_meta".into(), json!({"io.modelcontextprotocol/subscriptionId": sub_id}));
                                    } else {
                                        msg["params"] = json!({"_meta": {"io.modelcontextprotocol/subscriptionId": sub_id}});
                                    }
                                }
                            }
                            Some(msg)
                        }
                        None => break,
                    }
                }
            };
            if let Some(payload) = out {
                let mut text = serde_json::to_string(&payload).unwrap();
                text.push('\n');
                if sw.write_all(text.as_bytes()).await.is_err() {
                    break;
                }
                let _ = sw.flush().await;
            }
        }
    });

    let client =
        McpClient::connect_stdio_with_elicitation("ext-srv", cr, cw, limits, elicit, &Never)
            .await
            .unwrap();
    (
        client,
        MockStdio {
            requests: req_rx,
            to_client: msg_tx,
            task,
        },
    )
}

#[tokio::test]
async fn resources_templates_prompts_pagination_and_bounds() {
    let limits = McpLimits {
        max_result_bytes: 512,
        ..McpLimits::default()
    };
    let (client, _srv) = spawn_mock(ServerCfg::default(), limits).await;
    let caps = client.server_capabilities();
    assert!(caps.resources);
    assert!(caps.resources_subscribe);
    assert!(caps.prompts);

    let catalog = client.list_resources(&Never).await.unwrap();
    assert_eq!(catalog.resources.len(), 2);
    assert_eq!(catalog.resources[0].uri, "file:///doc/a.txt");
    assert_eq!(catalog.resources[1].uri, "file:///doc/b.bin");

    let templates = client.list_resource_templates(&Never).await.unwrap();
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].uri_template, "file:///doc/{name}");

    let read = client
        .read_resource("file:///doc/a.txt", &Never)
        .await
        .unwrap();
    assert!(!read.truncated);
    assert_eq!(read.contents.len(), 2);
    assert!(
        matches!(&read.contents[0], McpResourceContent::Text { text, .. } if text == "hello resource")
    );
    assert!(matches!(
        &read.contents[1],
        McpResourceContent::Blob {
            encoded_bytes: 4,
            ..
        }
    ));

    let huge = client
        .read_resource("file:///doc/huge", &Never)
        .await
        .unwrap();
    assert!(huge.truncated);

    let prompts = client.list_prompts(&Never).await.unwrap();
    assert_eq!(prompts.prompts.len(), 1);
    assert_eq!(
        prompts.prompts[0],
        McpPrompt {
            name: "summarize".into(),
            title: Some("Summarize".into()),
            description: Some("Summarize text".into()),
            arguments: vec![soyo_core::mcp::McpPromptArgument {
                name: "topic".into(),
                title: None,
                description: Some("Focus topic".into()),
                required: true,
            }],
        }
    );

    let resolved = client
        .get_prompt(
            "summarize",
            Some(BTreeMap::from([("topic".into(), "rust".into())])),
            &Never,
        )
        .await
        .unwrap();
    assert_eq!(resolved.messages.len(), 2);
    assert_eq!(resolved.messages[0].role, "user");
    assert_eq!(resolved.messages[1].role, "assistant");

    // Repeating cursor must fail cleanly.
    let (bad_client, _bad) = spawn_mock(
        ServerCfg {
            repeat_cursor: true,
            ..ServerCfg::default()
        },
        McpLimits::default(),
    )
    .await;
    let err = bad_client.list_resources(&Never).await.unwrap_err();
    assert_eq!(err.code(), "mcp_protocol");
}

#[tokio::test]
async fn subscriptions_and_notifications_work_in_both_modern_and_legacy() {
    for modern in [true, false] {
        let (client, mut srv) = spawn_mock(
            ServerCfg {
                modern,
                ..ServerCfg::default()
            },
            McpLimits::default(),
        )
        .await;
        client
            .subscribe_resource("file:///doc/a.txt", &Never)
            .await
            .unwrap();
        // Idempotent repeat does not error or duplicate.
        client
            .subscribe_resource("file:///doc/a.txt", &Never)
            .await
            .unwrap();

        let res_gen_before = client.resource_generation();
        let prompt_gen_before = client.prompt_generation();

        srv.to_client
            .send(json!({
                "jsonrpc": "2.0",
                "method": "notifications/resources/updated",
                "params": {"uri": "file:///doc/a.txt"}
            }))
            .await
            .unwrap();
        srv.to_client
            .send(json!({
                "jsonrpc": "2.0",
                "method": "notifications/resources/list_changed"
            }))
            .await
            .unwrap();
        srv.to_client
            .send(json!({
                "jsonrpc": "2.0",
                "method": "notifications/prompts/list_changed"
            }))
            .await
            .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while client.peek_updated_resources().is_empty()
            || client.resource_generation() == res_gen_before
            || client.prompt_generation() == prompt_gen_before
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for notifications (modern={modern})"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            client.take_updated_resources(),
            vec!["file:///doc/a.txt".to_string()]
        );
        assert!(client.take_updated_resources().is_empty());

        client
            .unsubscribe_resource("file:///doc/a.txt", &Never)
            .await
            .unwrap();
        while srv.requests.try_recv().is_ok() {}
    }
}

#[tokio::test]
async fn elicitation_declines_headless_validates_url_and_forwards_when_attached() {
    // Headless client declines server elicitation automatically.
    let (headless, _srv) = spawn_mock_with_elicit(
        ServerCfg {
            modern: false,
            ..ServerCfg::default()
        },
        McpLimits::default(),
        None,
    )
    .await;
    // Trigger bidirectional check via duplex? Here we directly connect raw server below to assert wire reply!
    drop(headless);

    struct AcceptHandler(Arc<AtomicUsize>);
    impl McpElicitationHandler for AcceptHandler {
        fn elicit<'a>(
            &'a self,
            request: &'a McpElicitationRequest,
            _: &'a dyn Cancellation,
        ) -> Operation<'a, McpElicitationDecision> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                match request {
                    McpElicitationRequest::Form { .. } => Ok(McpElicitationDecision::Accept {
                        content: Some(json!({"confirm": true})),
                    }),
                    McpElicitationRequest::Url { .. } => Ok(McpElicitationDecision::Cancel),
                }
            })
        }
    }

    let handler_calls = Arc::new(AtomicUsize::new(0));
    let (client, mut srv) = spawn_mock_with_elicit(
        ServerCfg {
            modern: false,
            ..ServerCfg::default()
        },
        McpLimits::default(),
        Some(Arc::new(AcceptHandler(handler_calls.clone()))),
    )
    .await;

    let (client2, mut srv2) = spawn_mock_with_elicit(
        ServerCfg {
            modern: false,
            ..ServerCfg::default()
        },
        McpLimits::default(),
        None,
    )
    .await;

    // Drain initial handshake requests.
    tokio::time::sleep(Duration::from_millis(50)).await;
    while srv.requests.try_recv().is_ok() {}
    while srv2.requests.try_recv().is_ok() {}

    // 1. Unsafe http:// URL must fail closed without invoking handler.
    srv.to_client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 901,
            "method": "elicitation/create",
            "params": {
                "mode": "url",
                "message": "open http",
                "url": "http://insecure.example/auth",
                "elicitationId": "e1"
            }
        }))
        .await
        .unwrap();
    // 2. Valid form elicitation invokes attached handler and returns accept.
    srv.to_client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 902,
            "method": "elicitation/create",
            "params": {
                "mode": "form",
                "message": "Need confirmation",
                "requestedSchema": {"type": "object", "properties": {"confirm": {"type": "boolean"}}}
            }
        }))
        .await
        .unwrap();
    // 3. Valid https:// URL invokes attached handler and returns cancel.
    srv.to_client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 903,
            "method": "elicitation/create",
            "params": {
                "mode": "url",
                "message": "Complete OAuth",
                "url": "https://example.com/oauth",
                "elicitationId": "e2"
            }
        }))
        .await
        .unwrap();

    // Also test headless decline.
    srv2.to_client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 904,
            "method": "elicitation/create",
            "params": {
                "mode": "form",
                "message": "Headless check",
                "requestedSchema": {"type": "object", "properties": {"x": {"type": "string"}}}
            }
        }))
        .await
        .unwrap();

    let mut replies = BTreeMap::new();
    for _ in 0..3 {
        let msg = tokio::time::timeout(Duration::from_secs(2), srv.requests.recv())
            .await
            .unwrap()
            .unwrap();
        if let Some(id) = msg.get("id").and_then(Value::as_i64) {
            replies.insert(id, msg["result"]["action"].as_str().unwrap().to_string());
        }
    }
    let headless_reply = tokio::time::timeout(Duration::from_secs(2), srv2.requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replies.get(&901).map(String::as_str), Some("decline"));
    assert_eq!(replies.get(&902).map(String::as_str), Some("accept"));
    assert_eq!(replies.get(&903).map(String::as_str), Some("cancel"));
    assert_eq!(headless_reply["result"]["action"].as_str(), Some("decline"));
    assert_eq!(handler_calls.load(Ordering::SeqCst), 2);
    drop(client);
    drop(client2);
}

const RUNTIME_MCP_SCRIPT: &str = r#"#!/usr/bin/env python3
import json, sys

CAPS = {
    "tools": {"listChanged": False},
    "resources": {"subscribe": True, "listChanged": False},
    "prompts": {"listChanged": False},
}

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    if "id" not in req:
        continue
    rid = req["id"]
    m = req.get("method", "")
    if m == "initialize":
        res = {
            "protocolVersion": "2026-07-28",
            "capabilities": CAPS,
            "serverInfo": {"name": "py-ext", "version": "1.0"}
        }
    elif m == "server/discover":
        res = {
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": CAPS,
            "ttlMs": 0,
            "cacheScope": "private"
        }
    elif m == "tools/list":
        res = {"resultType": "complete", "tools": [], "ttlMs": 0, "cacheScope": "private"}
    elif m == "resources/list":
        res = {"resources": [{"uri": "memo://secret", "name": "secret-memo", "mimeType": "text/plain"}]}
    elif m == "resources/templates/list":
        res = {"resourceTemplates": []}
    elif m == "resources/read":
        res = {"contents": [{"uri": "memo://secret", "mimeType": "text/plain", "text": "contains secret-value-xyz"}]}
    elif m == "prompts/list":
        res = {"prompts": [{"name": "hello", "arguments": [{"name": "who", "required": False}]}]}
    elif m == "prompts/get":
        who = req.get("params", {}).get("arguments", {}).get("who", "world")
        res = {"messages": [{"role": "user", "content": {"type": "text", "text": f"hi {who} secret-value-xyz"}}]}
    else:
        res = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "result": res}) + "\n")
    sys.stdout.flush()
"#;

struct CapModel {
    seen_tools: Mutex<Vec<Vec<String>>>,
    calls: AtomicUsize,
}
struct OneShot(Option<ModelUpdate>);
impl ModelStream for OneShot {
    fn next<'a>(&'a mut self, _: &'a dyn Cancellation) -> Operation<'a, Option<ModelUpdate>> {
        Box::pin(async move { Ok(self.0.take()) })
    }
}
impl Model for CapModel {
    fn start<'a>(
        &'a self,
        request: &'a ModelRequest,
        _: &'a dyn Cancellation,
    ) -> Operation<'a, Box<dyn ModelStream>> {
        Box::pin(async move {
            if request.purpose != soyo_core::contract::ModelPurpose::Execution {
                return Ok(Box::new(OneShot(Some(ModelUpdate::Finished {
                    items: vec![HistoryItem::AssistantText(
                        r#"{"decision":"ask","reason":"fixture escalates stdio start to ApproveAll"}"#.into(),
                    )],
                    usage: None,
                }))) as Box<dyn ModelStream>);
            }
            let names: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();
            self.seen_tools.lock().unwrap().push(names);
            let step = self.calls.fetch_add(1, Ordering::SeqCst);
            let items = match step {
                0 => vec![HistoryItem::ToolCall(ToolCall {
                    call_id: "c1".into(),
                    name: "mcp_list_resources".into(),
                    arguments: "{}".into(),
                })],
                1 => vec![HistoryItem::ToolCall(ToolCall {
                    call_id: "c2".into(),
                    name: "mcp_read_resource".into(),
                    arguments: json!({"server": "ext", "uri": "memo://secret"}).to_string(),
                })],
                2 => vec![HistoryItem::ToolCall(ToolCall {
                    call_id: "c3".into(),
                    name: "mcp_subscribe_resource".into(),
                    arguments: json!({"server": "ext", "uri": "memo://secret"}).to_string(),
                })],
                3 => vec![HistoryItem::ToolCall(ToolCall {
                    call_id: "c4".into(),
                    name: "mcp_list_prompts".into(),
                    arguments: "{}".into(),
                })],
                4 => vec![HistoryItem::ToolCall(ToolCall {
                    call_id: "c5".into(),
                    name: "mcp_get_prompt".into(),
                    arguments:
                        json!({"server": "ext", "name": "hello", "arguments": {"who": "soyo"}})
                            .to_string(),
                })],
                _ => vec![HistoryItem::AssistantText("done".into())],
            };
            Ok(
                Box::new(OneShot(Some(ModelUpdate::Finished { items, usage: None })))
                    as Box<dyn ModelStream>,
            )
        })
    }
}

struct ApproveAll;
impl soyo_core::runtime::ApprovalHandler for ApproveAll {
    fn confirm<'a>(
        &'a self,
        _: &'a soyo_core::contract::ApprovalRequest,
        _: &'a dyn Cancellation,
    ) -> Operation<'a, Option<bool>> {
        Box::pin(async move { Ok(Some(true)) })
    }
}

#[tokio::test]
async fn runtime_exposes_and_redacts_mcp_resources_and_prompts_only_when_advertised() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(dir.path()).unwrap();
    let script = workspace.join("mcp_ext.py");
    std::fs::write(&script, RUNTIME_MCP_SCRIPT).unwrap();

    let environment = Environment::from_pairs(std::env::vars_os());
    let store = Arc::new(MemoryStore::default());
    let registry = Arc::new(SecretRegistry::default());
    registry.register(&["secret-value-xyz".into()]).unwrap();
    let session = Session {
        id: SessionId::new(),
        workspace: workspace.clone(),
        work_mode: WorkMode::Work,
        profile: "fixture".into(),
        protocol: ModelProtocol::ChatCompletions,
        model: "fixture".into(),
        history: Vec::new(),
    };
    let model = Arc::new(CapModel {
        seen_tools: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let setup = RuntimeSetup {
        run: Run {
            id: RunId::new(),
            session_id: session.id,
            work_mode: WorkMode::Work,
            permissions: PermissionMode::Auto,
        },
        model: model.clone(),
        store,
        policy: Arc::new(Policy::new(&workspace, PermissionConfig::default()).unwrap()),
        files: Arc::new(ReadTools::new(&workspace, &[], false, ReadLimits::default()).unwrap()),
        cancellation: Arc::new(Never),
        review: ReviewConfig::new("fixture".into()),
        exec: ExecConfig {
            environment: environment.clone(),
            ..ExecConfig::default()
        },
        preauthorizations: Preauthorizations::default(),
        known_secrets: vec!["secret-value-xyz".into()],
        summary: "mcp-ext-test".into(),
        confirmation: Some(Arc::new(ApproveAll)),
        limits: RuntimeLimits {
            active_time: Duration::from_secs(30),
            model_connect: Duration::from_secs(3),
            model_idle: Duration::from_secs(3),
            model_total: Duration::from_secs(3),
            tool_timeout: Duration::from_secs(5),
            ..RuntimeLimits::default()
        },
    };
    let settings = McpSettings {
        servers: BTreeMap::from([(
            "ext".into(),
            McpServerSettings {
                enabled: true,
                transport: McpTransportSettings::Stdio(StdioMcpSettings {
                    argv: vec![
                        "python3".into(),
                        "-u".into(),
                        script.to_string_lossy().into(),
                    ],
                    cwd: None,
                    env: BTreeMap::new(),
                }),
                read_only_tools: BTreeSet::new(),
                eager_tools: BTreeSet::new(),
                tool_examples: BTreeMap::new(),
                limits: McpLimitSettings::default(),
            },
        )]),
        ..McpSettings::default()
    };
    let mut sandbox = SandboxPolicy::workspace(workspace.clone());
    sandbox.backend = BackendKind::Host;
    sandbox.protected_paths.clear();

    let mut runtime = AgentRuntime::new(setup)
        .unwrap()
        .with_execution_settings(sandbox, None)
        .unwrap()
        .with_mcp_settings(settings, environment, None, registry)
        .unwrap();
    let mut session_mut = session;
    let res = runtime.run(&mut session_mut).await.unwrap();
    assert_eq!(res.status, RunStatus::Completed, "result: {res:?}");

    let tools = model.seen_tools.lock().unwrap()[0].clone();
    for expected in [
        "mcp_list_resources",
        "mcp_read_resource",
        "mcp_subscribe_resource",
        "mcp_list_prompts",
        "mcp_get_prompt",
    ] {
        assert!(
            tools.iter().any(|t| t == expected),
            "missing {expected} in {tools:?}"
        );
    }

    let serialized_history = serde_json::to_string(&session_mut.history).unwrap();
    assert!(
        !serialized_history.contains("secret-value-xyz"),
        "secret leaked into history"
    );
    assert!(serialized_history.contains("hi soyo"));
    assert!(
        serialized_history.contains("hello resource") || serialized_history.contains("contains ")
    );

    // Zero drift: a server with tools only registers none of the 5 mcp_* capability tools.
    let tools_only_script = workspace.join("mcp_tools_only.py");
    std::fs::write(
        &tools_only_script,
        RUNTIME_MCP_SCRIPT.replace(
            r#""resources": {"subscribe": True, "listChanged": False},
    "prompts": {"listChanged": False},"#,
            "",
        ),
    )
    .unwrap();
    let model2 = Arc::new(CapModel {
        seen_tools: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(5),
    });
    let mut session2 = Session {
        id: SessionId::new(),
        workspace: workspace.clone(),
        work_mode: WorkMode::Work,
        profile: "fixture".into(),
        protocol: ModelProtocol::ChatCompletions,
        model: "fixture".into(),
        history: Vec::new(),
    };
    let setup2 = RuntimeSetup {
        run: Run {
            id: RunId::new(),
            session_id: session2.id,
            work_mode: WorkMode::Work,
            permissions: PermissionMode::Auto,
        },
        model: model2.clone(),
        store: Arc::new(MemoryStore::default()),
        policy: Arc::new(Policy::new(&workspace, PermissionConfig::default()).unwrap()),
        files: Arc::new(ReadTools::new(&workspace, &[], false, ReadLimits::default()).unwrap()),
        cancellation: Arc::new(Never),
        review: ReviewConfig::new("fixture".into()),
        exec: ExecConfig {
            environment: Environment::from_pairs(std::env::vars_os()),
            ..ExecConfig::default()
        },
        preauthorizations: Preauthorizations::default(),
        known_secrets: Vec::new(),
        summary: "mcp-tools-only".into(),
        confirmation: Some(Arc::new(ApproveAll)),
        limits: RuntimeLimits {
            active_time: Duration::from_secs(30),
            model_connect: Duration::from_secs(3),
            model_idle: Duration::from_secs(3),
            model_total: Duration::from_secs(3),
            tool_timeout: Duration::from_secs(5),
            ..RuntimeLimits::default()
        },
    };
    let settings2 = McpSettings {
        servers: BTreeMap::from([(
            "plain".into(),
            McpServerSettings {
                enabled: true,
                transport: McpTransportSettings::Stdio(StdioMcpSettings {
                    argv: vec![
                        "python3".into(),
                        "-u".into(),
                        tools_only_script.to_string_lossy().into(),
                    ],
                    cwd: None,
                    env: BTreeMap::new(),
                }),
                read_only_tools: BTreeSet::new(),
                eager_tools: BTreeSet::new(),
                tool_examples: BTreeMap::new(),
                limits: McpLimitSettings::default(),
            },
        )]),
        ..McpSettings::default()
    };
    let mut sandbox2 = SandboxPolicy::workspace(workspace);
    sandbox2.backend = BackendKind::Host;
    sandbox2.protected_paths.clear();
    let mut runtime2 = AgentRuntime::new(setup2)
        .unwrap()
        .with_execution_settings(sandbox2, None)
        .unwrap()
        .with_mcp_settings(
            settings2,
            Environment::from_pairs(std::env::vars_os()),
            None,
            Arc::new(SecretRegistry::default()),
        )
        .unwrap();
    runtime2.run(&mut session2).await.unwrap();
    let plain_tools = &model2.seen_tools.lock().unwrap()[0];
    for absent in [
        "mcp_list_resources",
        "mcp_read_resource",
        "mcp_subscribe_resource",
        "mcp_list_prompts",
        "mcp_get_prompt",
    ] {
        assert!(
            !plain_tools.iter().any(|t| t == absent),
            "unexpected {absent} in {plain_tools:?}"
        );
    }
}
