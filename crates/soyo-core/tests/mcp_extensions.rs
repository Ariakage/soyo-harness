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
    /// Advertise the SEP-2663 `io.modelcontextprotocol/tasks` extension and
    /// serve the `tasks/*` lifecycle methods below.
    advertise_tasks: bool,
    /// Fixed task id this mock returns for `tools/call` -> CreateTaskResult
    /// and recognizes in `tasks/get`/`tasks/cancel`/`tasks/update`.
    tasks_id: String,
    /// Task status served by `tasks/get` (`working`, `input_required`,
    /// `completed`, `failed`, `cancelled`).
    tasks_status: String,
    /// Hang without replying to `tasks/get` (timeout test).
    tasks_get_hangs: bool,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            modern: true,
            advertise_resources: true,
            advertise_subscribe: true,
            advertise_prompts: true,
            repeat_cursor: false,
            advertise_tasks: false,
            tasks_id: "task-abc".into(),
            tasks_status: "working".into(),
            tasks_get_hangs: false,
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
    spawn_mock_with_options(cfg, limits, None, false).await
}

async fn spawn_mock_with_elicit(
    cfg: ServerCfg,
    limits: McpLimits,
    elicit: Option<Arc<dyn McpElicitationHandler>>,
) -> (McpClient, MockStdio) {
    spawn_mock_with_options(cfg, limits, elicit, false).await
}

async fn spawn_mock_with_options(
    cfg: ServerCfg,
    limits: McpLimits,
    elicit: Option<Arc<dyn McpElicitationHandler>>,
    tasks_enabled: bool,
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
            if cfg.advertise_tasks {
                caps["extensions"] = json!({
                    "io.modelcontextprotocol/tasks": {}
                });
            }
            caps
        };
        let tasks_id = cfg.tasks_id.clone();
        let tasks_status = cfg.tasks_status.clone();
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
                            let tools = if cfg.advertise_tasks {
                                json!([{
                                    "name": "create_report",
                                    "description": "start a long-running report task",
                                    "inputSchema": {"type": "object", "properties": {}}
                                }])
                            } else {
                                json!([])
                            };
                            Some(json!({"jsonrpc":"2.0","id":id,"result":{"tools": tools}}))
                        }
                        "tools/call" if cfg.advertise_tasks => {
                            // Server materializes a SEP-2663 task instead of a final result.
                            Some(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "resultType": "task",
                                    "taskId": tasks_id,
                                    "status": "working",
                                    "createdAt": "2026-11-24T00:00:00Z",
                                    "lastUpdatedAt": "2026-11-24T00:00:00Z",
                                    "ttlMs": 300000,
                                    "pollIntervalMs": 1000
                                }
                            }))
                        }
                        "tasks/get" if cfg.advertise_tasks && !cfg.tasks_get_hangs => {
                            let status = tasks_status.as_str();
                            let mut body = json!({
                                "resultType": "complete",
                                "taskId": req["params"]["taskId"].clone(),
                                "status": status,
                                "createdAt": "2026-11-24T00:00:00Z",
                                "lastUpdatedAt": "2026-11-24T00:00:01Z",
                                "ttlMs": 300000,
                                "pollIntervalMs": 1000
                            });
                            match status {
                                "input_required" => {
                                    body["inputRequests"] = json!({
                                        "elic-1": {
                                            "method": "roots/list"
                                        }
                                    });
                                }
                                "completed" => {
                                    body["result"] = json!({
                                        "content": [{"type": "text", "text": "task finished"}],
                                        "isError": false
                                    });
                                }
                                "failed" => {
                                    body["error"] = json!({
                                        "code": -32603,
                                        "message": "task blew up"
                                    });
                                }
                                _ => {}
                            }
                            Some(json!({"jsonrpc": "2.0", "id": id, "result": body}))
                        }
                        "tasks/get" if cfg.advertise_tasks && cfg.tasks_get_hangs => {
                            // No reply: the client-side timeout is under test.
                            None
                        }
                        "tasks/cancel" | "tasks/update" if cfg.advertise_tasks => {
                            Some(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {"resultType": "complete"}
                            }))
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

    let client = McpClient::connect_stdio_with_options(
        "ext-srv",
        cr,
        cw,
        limits,
        elicit,
        tasks_enabled,
        &Never,
    )
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
                    env_credential_refs: BTreeMap::new(),
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
        .with_mcp_settings(settings, environment, None, registry, None)
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
                    env_credential_refs: BTreeMap::new(),
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
            None,
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

// ---------------------------------------------------------------------------
// SEP-2663 MCP Tasks (opt-in)
// ---------------------------------------------------------------------------

/// `tasks/get` on a client that did NOT opt in stays fail-closed even when the
/// server advertises the extension — the tool never reaches the wire.
#[tokio::test]
async fn tasks_get_rejects_without_opt_in() {
    let (client, mut srv) = spawn_mock(
        ServerCfg {
            advertise_tasks: true,
            ..ServerCfg::default()
        },
        McpLimits::default(),
    )
    .await;
    assert!(client.server_capabilities().tasks);
    assert!(!client.tasks_enabled());
    let err = client.get_task("task-abc", &Never).await.unwrap_err();
    assert_eq!(err.code(), "mcp_unsupported");
    // The wire saw no tasks/get request.
    while let Ok(req) = srv.requests.try_recv() {
        assert_ne!(req["method"], "tasks/get");
    }
}

/// `tasks/get` on a server that does NOT advertise the extension fails closed
/// even when the client opted in.
#[tokio::test]
async fn tasks_get_rejects_when_server_lacks_capability() {
    let (client, _srv) = spawn_mock_with_options(
        ServerCfg {
            advertise_tasks: false,
            ..ServerCfg::default()
        },
        McpLimits::default(),
        None,
        true,
    )
    .await;
    assert!(!client.server_capabilities().tasks);
    assert!(client.tasks_enabled());
    let err = client.get_task("task-abc", &Never).await.unwrap_err();
    assert_eq!(err.code(), "mcp_unsupported");
}

/// Working / input_required / completed / failed status projection and the
/// `needs_input`/`done`/`failed` run-state mapping.
#[tokio::test]
async fn tasks_get_projects_status_and_input_requests() {
    for (status, run_state) in [
        ("working", "running"),
        ("input_required", "needs_input"),
        ("completed", "done"),
        ("failed", "failed"),
        ("cancelled", "cancelled"),
    ] {
        let (client, _srv) = spawn_mock_with_options(
            ServerCfg {
                advertise_tasks: true,
                tasks_status: status.into(),
                ..ServerCfg::default()
            },
            McpLimits::default(),
            None,
            true,
        )
        .await;
        let proj = client.get_task("task-abc", &Never).await.unwrap();
        assert_eq!(proj.info.status, status, "status for {status}");
        assert_eq!(proj.info.run_state, run_state, "run_state for {status}");
        match status {
            "input_required" => {
                assert_eq!(proj.answerable_keys, vec!["elic-1".to_string()]);
                assert_eq!(proj.info.input_requests.len(), 1);
                assert_eq!(proj.info.input_requests[0].key, "elic-1");
                assert_eq!(proj.info.input_requests[0].method, "roots/list");
            }
            "completed" => {
                assert!(proj.info.result.is_some());
                assert_eq!(proj.info.result.as_ref().unwrap()["isError"], false);
            }
            "failed" => {
                assert!(proj.info.error.is_some());
            }
            _ => {
                assert!(proj.answerable_keys.is_empty());
            }
        }
    }
}

/// `tools/call` returning a `CreateTaskResult` yields a `mcp_task_created`
/// error only when both opt-in and server capability are present; a missing
/// opt-in keeps `outcome_unknown` (fail-closed).
#[tokio::test]
async fn call_tool_task_response_needs_opt_in() {
    // opt-in + advertised server: the task handle surfaces as `mcp_task_created`.
    let (client, _srv) = spawn_mock_with_options(
        ServerCfg {
            advertise_tasks: true,
            ..ServerCfg::default()
        },
        McpLimits::default(),
        None,
        true,
    )
    .await;
    let catalog = client.list_tools(&Never).await.unwrap();
    assert_eq!(catalog.tools.len(), 1);
    let result = client
        .call_tool("create_report", json!({}), catalog.generation, &Never)
        .await
        .unwrap();
    assert!(!result.is_error);
    let handle = &result.structured_content.unwrap()["mcp_task"];
    assert_eq!(handle["taskId"], "task-abc");
    assert_eq!(handle["status"], "working");

    // Server advertises but the client did NOT opt in: still fail-closed.
    let (closed, _srv2) = spawn_mock(
        ServerCfg {
            advertise_tasks: true,
            ..ServerCfg::default()
        },
        McpLimits::default(),
    )
    .await;
    let catalog2 = closed.list_tools(&Never).await.unwrap();
    let err2 = closed
        .call_tool("create_report", json!({}), catalog2.generation, &Never)
        .await
        .unwrap_err();
    assert_eq!(err2.code(), "outcome_unknown", "err2: {err2:?}");
}

/// `tasks/cancel` and `tasks/update` reach the wire and are acknowledged.
#[tokio::test]
async fn tasks_cancel_and_update_complete() {
    let (client, mut srv) = spawn_mock_with_options(
        ServerCfg {
            advertise_tasks: true,
            ..ServerCfg::default()
        },
        McpLimits::default(),
        None,
        true,
    )
    .await;
    client.cancel_task("task-abc", &Never).await.unwrap();
    client
        .respond_task(
            "task-abc",
            BTreeMap::from([(
                "elic-1".into(),
                json!({"action": "accept", "content": {"name": "soyo"}}),
            )]),
            &Never,
        )
        .await
        .unwrap();
    let mut saw_cancel = false;
    let mut saw_update = false;
    while let Ok(req) = srv.requests.try_recv() {
        match req["method"].as_str() {
            Some("tasks/cancel") => saw_cancel = true,
            Some("tasks/update") => {
                saw_update = true;
                assert_eq!(req["params"]["taskId"], "task-abc");
                assert!(req["params"]["inputResponses"]["elic-1"].is_object());
            }
            _ => {}
        }
    }
    assert!(saw_cancel && saw_update);

    // An empty response set is rejected before any wire call.
    let err = client
        .respond_task("task-abc", BTreeMap::new(), &Never)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_args");
}

/// A hanging `tasks/get` maps to `mcp_timeout`, not an unbounded wait.
#[tokio::test]
async fn tasks_get_timeout_is_bounded() {
    let (client, _srv) = spawn_mock_with_options(
        ServerCfg {
            advertise_tasks: true,
            tasks_get_hangs: true,
            ..ServerCfg::default()
        },
        McpLimits {
            request_timeout: Duration::from_millis(50),
            ..McpLimits::default()
        },
        None,
        true,
    )
    .await;
    let err = client.get_task("task-abc", &Never).await.unwrap_err();
    assert_eq!(err.code(), "mcp_timeout");
}

/// `mcp_get_task`/`mcp_cancel_task`/`mcp_respond_task` are absent from the
/// tool directory unless both opt-in and server advertisement are present;
/// when present they register exactly once.
#[tokio::test]
async fn runtime_task_tools_only_register_when_opt_in_and_advertised() {
    // (a) opt-in + advertised => tools registered
    let dir = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(dir.path()).unwrap();
    let script = workspace.join("mcp_tasks.py");
    std::fs::write(&script, RUNTIME_MCP_TASKS_SCRIPT).unwrap();
    for (tasks_enabled, tasks_advertised, expect_registered) in [
        (true, true, true),
        (true, false, false),
        (false, true, false),
    ] {
        let script_text = RUNTIME_MCP_TASKS_SCRIPT.replace(
            "TASKS_ADVERTISED = True",
            &format!(
                "TASKS_ADVERTISED = {}",
                if tasks_advertised { "True" } else { "False" }
            ),
        );
        std::fs::write(&script, script_text).unwrap();
        let model = Arc::new(CapModel {
            seen_tools: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(usize::MAX - 1),
        });
        let session = Session {
            id: SessionId::new(),
            workspace: workspace.clone(),
            work_mode: WorkMode::Work,
            profile: "fixture".into(),
            protocol: ModelProtocol::ChatCompletions,
            model: "fixture".into(),
            history: Vec::new(),
        };
        let setup = RuntimeSetup {
            run: Run {
                id: RunId::new(),
                session_id: session.id,
                work_mode: WorkMode::Work,
                permissions: PermissionMode::Auto,
            },
            model: model.clone(),
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
            summary: "mcp-tasks-test".into(),
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
            tasks: soyo_core::mcp::config::McpTasksSettings {
                enabled: tasks_enabled,
            },
            servers: BTreeMap::from([(
                "tasksrv".into(),
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
                        env_credential_refs: BTreeMap::new(),
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
            .with_mcp_settings(
                settings,
                Environment::from_pairs(std::env::vars_os()),
                None,
                Arc::new(SecretRegistry::default()),
                None,
            )
            .unwrap();
        let mut session_mut = session;
        runtime.run(&mut session_mut).await.unwrap();
        let seen = &model.seen_tools.lock().unwrap()[0];
        for name in ["mcp_get_task", "mcp_cancel_task", "mcp_respond_task"] {
            assert_eq!(
                seen.iter().any(|t| t == name),
                expect_registered,
                "opt_in={tasks_enabled} advertised={tasks_advertised} tool={name} seen={seen:?}"
            );
        }
    }
}

const RUNTIME_MCP_TASKS_SCRIPT: &str = r#"#!/usr/bin/env python3
import json, sys

TASKS_ADVERTISED = True

CAPS = {"tools": {"listChanged": False}}
if TASKS_ADVERTISED:
    CAPS["extensions"] = {"io.modelcontextprotocol/tasks": {}}

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
            "serverInfo": {"name": "py-tasks", "version": "1.0"}
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
    else:
        res = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "result": res}) + "\n")
    sys.stdout.flush()
"#;
