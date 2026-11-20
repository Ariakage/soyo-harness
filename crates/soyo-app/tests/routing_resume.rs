use serde_json::json;
use soyo_app::session::{self, ResumeOptions};
use soyo_core::{
    boundary::Store,
    config::{Environment, Overrides},
    contract::{HistoryItem, ModelProtocol, ToolCall, ToolResult},
};
use std::ffi::OsString;

#[tokio::test]
async fn cross_profile_resume_rejects_without_mutation_and_fork_migrated_preserves_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("workspace");
    std::fs::create_dir(&ws).unwrap();
    let cfg_path = tmp.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        r#"schema_version = 1
[defaults]
profile = "resp"
[profiles.resp]
protocol = "openai-responses"
base_url = "http://127.0.0.1:9/v1"
model = "resp-1"
auth = "none"
context_window_tokens = 128000
stream = false

[profiles.anth]
protocol = "anthropic-messages"
base_url = "http://127.0.0.1:9"
model = "anth-1"
auth = "none"
context_window_tokens = 200000
stream = false
"#,
    )
    .unwrap();

    let env =
        Environment::from_pairs([(OsString::from("HOME"), tmp.path().as_os_str().to_owned())]);
    let cfg_resp = soyo_core::config::load(
        Overrides {
            config: Some(cfg_path.clone()),
            ..Default::default()
        },
        env.clone(),
        &ws,
    )
    .unwrap();

    let mut original = soyo_app::new_session(&cfg_resp).unwrap();
    original.history = vec![
        HistoryItem::UserText("inspect bug".into()),
        HistoryItem::ProviderItem {
            protocol: ModelProtocol::Responses,
            value: json!({"type":"reasoning","encrypted_content":"opaque_blob"}),
        },
        HistoryItem::ToolCall(ToolCall {
            call_id: "c1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"src/lib.rs"}"#.into(),
        }),
        HistoryItem::ToolResult {
            call_id: "c1".into(),
            result: ToolResult::success(Some(json!({"content":"fn main(){}"}))),
        },
        HistoryItem::AssistantText("found it".into()),
    ];
    let orig_id = original.id;
    let orig_history = original.history.clone();
    let prepared = session::create(&cfg_resp, original).await.unwrap();
    drop(prepared);

    let cfg_anth = soyo_core::config::load(
        Overrides {
            config: Some(cfg_path),
            profile: Some("anth".into()),
            ..Default::default()
        },
        env,
        &ws,
    )
    .unwrap();

    // Direct cross-profile resume rejects cleanly.
    let err = match session::resume(
        &cfg_anth,
        &orig_id.0.to_string(),
        ResumeOptions {
            accept_config_change: true,
            cwd_explicit: false,
            profile_explicit: true,
            mode_override: None,
        },
    )
    .await
    {
        Ok(_) => panic!("direct cross-profile resume must fail"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("session profile differs"));

    // Explicit migrated fork creates a new session, strips opaque ProviderItem, retains tools,
    // and leaves original session history untouched.
    let (forked, info) = session::fork_migrated(
        &cfg_anth,
        &orig_id.0.to_string(),
        ResumeOptions {
            accept_config_change: true,
            cwd_explicit: false,
            profile_explicit: true,
            mode_override: None,
        },
    )
    .await
    .unwrap();
    assert_ne!(forked.session.id, orig_id);
    assert_eq!(forked.session.profile, "anth");
    assert_eq!(forked.session.protocol, ModelProtocol::AnthropicMessages);
    assert_eq!(info.unwrap().stripped_opaque_items, 1);
    assert!(
        !forked
            .session
            .history
            .iter()
            .any(|i| matches!(i, HistoryItem::ProviderItem { .. }))
    );
    assert!(forked.session.history.iter().any(
        |i| matches!(i, HistoryItem::ToolResult { call_id, result } if call_id == "c1" && result.ok)
    ));

    // Verify original session in SQLite was NEVER rewritten.
    let orig_reloaded = forked.store.load_session(orig_id).await.unwrap().unwrap();
    assert_eq!(orig_reloaded.history, orig_history);
    assert_eq!(orig_reloaded.profile, "resp");
}
