//! End-to-end Goal step execution + StepVerifier test with `[subagents] enabled = true`.

#[path = "../../soyo-core/tests/fixtures/http_support/mod.rs"]
mod fixture;

use serde_json::{Value, json};
use soyo_app::{
    goal::{
        GoalRunContext, assign_step_verifier, confirm_goal_plan, continue_once, create_goal,
        open_goal_store, run_goal_planning,
    },
    interaction::RunCancellation,
};
use soyo_core::{
    boundary::Cancellation,
    config::{Environment, Overrides},
    contract::PermissionMode,
    goal::{GoalBudget, GoalPlan, PlannedStep, StepId, StepVerificationStatus, StepVerifier},
    runtime::PersonalContext,
};
use std::{
    ffi::OsString,
    fs,
    future::{Future, pending},
    path::Path,
    pin::Pin,
    sync::Arc,
};

struct NeverCancel;
impl Cancellation for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(pending())
    }
}

fn load_goal_config(root: &Path, url: &str) -> soyo_core::config::Config {
    let config_path = root.join("config.toml");
    fs::write(
        &config_path,
        format!(
            r#"schema_version = 1
[defaults]
profile = "chat"
mode = "work"
permissions = "bypass"
output = "text"
[sandbox]
backend = "host"
[subagents]
enabled = true
[profiles.chat]
protocol = "openai-chat-completions"
base_url = "{url}"
model = "example"
auth = "none"
context_window_tokens = 100000
stream = false
"#
        ),
    )
    .unwrap();
    soyo_core::config::load(
        Overrides {
            config: Some(config_path),
            data_dir: Some(root.join("data")),
            ..Default::default()
        },
        Environment::from_pairs([
            (OsString::from("HOME"), root.as_os_str().to_owned()),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ]),
        root,
    )
    .unwrap()
}

fn test_exe() -> String {
    if Path::new("/bin/test").exists() {
        "/bin/test".into()
    } else {
        "/usr/bin/test".into()
    }
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
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": tool_calls
            }
        }]
    })
    .to_string()
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

#[tokio::test]
async fn goal_step_delegates_to_subagent_merges_and_passes_step_verifier() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();

    let (url, _requests) = fixture::server_dynamic(6, |_, req| {
        let tools_map = tool_message_map(req);

        let body = if has_user_message_containing(req, "GOAL_SUB_TASK") {
            if !tools_map.contains_key("c1") {
                calls_response(vec![(
                    "c1",
                    "write_file",
                    json!({
                        "path": "deliverable.txt",
                        "content": "verified-by-subagent\n",
                        "expected_version": Value::Null
                    }),
                )])
            } else {
                text_response("deliverable created inside child workspace")
            }
        } else if !tools_map.contains_key("g_spawn") {
            calls_response(vec![(
                "g_spawn",
                "subagent_spawn",
                json!({
                    "label": "goal-worker",
                    "task": "GOAL_SUB_TASK",
                    "workspace_isolation": "isolated",
                    "background": false
                }),
            )])
        } else if !tools_map.contains_key("g_merge") {
            let id = tools_map["g_spawn"]["data"]["id"]
                .as_str()
                .unwrap()
                .to_owned();
            calls_response(vec![(
                "g_merge",
                "subagent_merge",
                json!({"subagent_id": id, "discard_after": true}),
            )])
        } else {
            text_response("step complete via subagent merge")
        };
        ("application/json", body)
    })
    .unwrap();

    let cfg = load_goal_config(&workspace, &url);
    let plan_cancel = NeverCancel;
    let created = create_goal(
        &cfg,
        "Deliver verified artifact via subagent",
        vec!["deliverable.txt exists in workspace".into()],
        GoalBudget::default(),
    )
    .await
    .unwrap();

    let step_id = StepId::new();
    let plan = GoalPlan {
        steps: vec![PlannedStep {
            id: step_id,
            title: "Delegate deliverable creation to isolated subagent".into(),
            instruction: "Create deliverable.txt via isolated subagent and merge it.".into(),
            depends_on: vec![],
            verifier: None,
            delegate: None,
        }],
        acceptance: created.acceptance.clone(),
    };
    run_goal_planning(&cfg, created.id, Some(plan), &plan_cancel)
        .await
        .unwrap();
    assign_step_verifier(
        &cfg,
        created.id,
        step_id,
        Some(StepVerifier {
            argv: vec![test_exe(), "-f".into(), "deliverable.txt".into()],
            cwd: None,
            timeout_seconds: 15,
            max_output_bytes: 1024,
        }),
    )
    .await
    .unwrap();
    confirm_goal_plan(&cfg, created.id).await.unwrap();

    let store = open_goal_store(&cfg).unwrap();
    let outcome = continue_once(
        &cfg,
        &store,
        created.id,
        GoalRunContext {
            permissions: PermissionMode::Bypass,
            confirmation: None,
            input: None,
            approvals: None,
            personal_context: PersonalContext::default(),
            anthropic_prompt_cache: false,
            accept_config_change: false,
            cancellation: Arc::new(RunCancellation::new()),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        outcome.goal.steps[0]
            .last_verification
            .as_ref()
            .map(|v| v.status),
        Some(StepVerificationStatus::Passed)
    );
    assert_eq!(
        fs::read_to_string(workspace.join("deliverable.txt")).unwrap(),
        "verified-by-subagent\n"
    );
}
