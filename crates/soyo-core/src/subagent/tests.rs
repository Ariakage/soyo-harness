//! Unit tests for `[subagents]` settings, workspace isolation, 3-way conflict
//! detection, atomic merge, cleanup, and policy derivation (`F-09`).

use super::*;
use crate::{
    contract::{PermissionMode, WorkMode},
    permissions::{Decision, Operation, PermissionConfig, Policy},
};
use std::{fs, path::Path, process::Command};
use tempfile::tempdir;

fn init_git_repo(dir: &Path) {
    for args in [
        &["init"][..],
        &["config", "user.email", "test@example.com"][..],
        &["config", "user.name", "Soyo Test"][..],
    ] {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {:?} failed", args);
    }
    fs::write(dir.join("lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();
    fs::write(dir.join("ignored.log"), "secret-ignored").unwrap();
    fs::write(dir.join(".gitignore"), "ignored.log\n").unwrap();
    for args in [&["add", "."][..], &["commit", "-m", "initial"][..]] {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {:?} failed", args);
    }
}

#[test]
fn settings_default_disabled_and_validate_bounds() {
    let settings = SubagentSettings::default();
    assert!(!settings.enabled);
    assert!(settings.resolve().unwrap().is_none());

    let mut invalid = settings.clone();
    invalid.enabled = false;
    invalid.max_concurrent = Some(0);
    assert!(invalid.validate().is_err());

    let enabled = SubagentSettings {
        enabled: true,
        max_concurrent: Some(2),
        default_timeout_seconds: Some(30),
        max_timeout_seconds: Some(60),
        ..Default::default()
    };
    let resolved = enabled.resolve().unwrap().unwrap();
    assert_eq!(resolved.limits.max_concurrent, 2);
    assert_eq!(resolved.limits.default_timeout.as_secs(), 30);
    assert_eq!(resolved.limits.max_timeout.as_secs(), 60);
}

#[test]
fn disabled_or_plan_mode_definitions_omit_writable_subagent_options() {
    let plan_defs = definitions(WorkMode::Plan, false);
    assert!(!plan_defs.iter().any(|d| d.name == "subagent_merge"));
    let spawn = plan_defs
        .iter()
        .find(|d| d.name == "subagent_spawn")
        .unwrap();
    assert_eq!(
        spawn.input_schema["properties"]["workspace_isolation"]["enum"],
        serde_json::json!(["read_only"])
    );
    assert_eq!(
        spawn.input_schema["properties"]["allow_exec"]["enum"],
        serde_json::json!([false])
    );

    let work_defs = definitions(WorkMode::Work, false);
    assert!(work_defs.iter().any(|d| d.name == "subagent_merge"));
    let ro_work_defs = definitions(WorkMode::Work, true);
    assert!(!ro_work_defs.iter().any(|d| d.name == "subagent_merge"));
}

#[test]
fn git_worktree_overlays_uncommitted_changes_and_merges_with_3way_conflict_check() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path());
    // Uncommitted dirty modification and untracked file in parent workspace
    fs::write(dir.path().join("lib.rs"), "pub fn base() -> i32 { 2 }\n").unwrap();
    fs::write(dir.path().join("draft.txt"), "parent-untracked\n").unwrap();

    let limits = SubagentLimits::default();
    let ws = workspace::IsolatedWorkspace::create_isolated(dir.path(), &[], &[], &limits).unwrap();
    assert_eq!(ws.kind, WorkspaceIsolationKind::GitWorktree);

    assert_eq!(
        fs::read_to_string(ws.child_dir.join("lib.rs")).unwrap(),
        "pub fn base() -> i32 { 2 }\n"
    );
    assert_eq!(
        fs::read_to_string(ws.child_dir.join("draft.txt")).unwrap(),
        "parent-untracked\n"
    );

    // Child modifies `lib.rs` and adds `extra.txt`
    fs::write(ws.child_dir.join("lib.rs"), "pub fn base() -> i32 { 42 }\n").unwrap();
    fs::write(ws.child_dir.join("extra.txt"), "added-by-child\n").unwrap();

    let diff = ws.compute_diff("sub-1", &[], true, &limits).unwrap();
    assert_eq!(diff.changed_files.len(), 2);
    assert!(diff.patch.contains("+pub fn base() -> i32 { 42 }"));

    // Concurrent parent edit on `lib.rs` causes 3-way conflict, aborting ALL writes
    // (including `extra.txt`).
    fs::write(dir.path().join("lib.rs"), "pub fn base() -> i32 { 99 }\n").unwrap();
    let root_canon = fs::canonicalize(dir.path()).unwrap();
    let outcome = ws
        .merge_into_parent(None, std::slice::from_ref(&root_canon), &[], &limits)
        .unwrap();
    match outcome {
        workspace::MergeOutcome::Conflict(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].path, "lib.rs");
        }
        workspace::MergeOutcome::Applied(_) => panic!("expected 3-way conflict"),
    }
    assert!(!dir.path().join("extra.txt").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("lib.rs")).unwrap(),
        "pub fn base() -> i32 { 99 }\n"
    );

    // Selective merge of `extra.txt` only succeeds cleanly
    let applied = ws
        .merge_into_parent(Some(&["extra.txt".into()]), &[root_canon], &[], &limits)
        .unwrap();
    match applied {
        workspace::MergeOutcome::Applied(receipts) => {
            assert_eq!(receipts.len(), 1);
            assert_eq!(receipts[0].path, "extra.txt");
            assert_eq!(receipts[0].operation, "created");
        }
        workspace::MergeOutcome::Conflict(_) => panic!("selective merge should succeed"),
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("extra.txt")).unwrap(),
        "added-by-child\n"
    );

    let outer = ws.outer_dir.clone().unwrap();
    ws.cleanup().unwrap();
    assert!(!outer.exists());
}

#[test]
fn directory_copy_fallback_isolated_merge_and_rejects_symlinks_and_git_paths() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("notes.txt"), "v1\n").unwrap();
    let limits = SubagentLimits::default();
    let ws = workspace::IsolatedWorkspace::create_isolated(dir.path(), &[], &[], &limits).unwrap();
    assert_eq!(ws.kind, WorkspaceIsolationKind::DirectoryCopy);

    fs::write(ws.child_dir.join("notes.txt"), "v2\n").unwrap();
    let root_canon = fs::canonicalize(dir.path()).unwrap();
    let applied = ws
        .merge_into_parent(None, std::slice::from_ref(&root_canon), &[], &limits)
        .unwrap();
    assert!(matches!(applied, workspace::MergeOutcome::Applied(_)));
    assert_eq!(
        fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
        "v2\n"
    );

    // Reject path targeting `.git` or escaping workspace
    assert!(validate_relative_workspace_path(".git/config").is_err());
    assert!(validate_relative_workspace_path("../escape").is_err());

    // Symlink in parent workspace is rejected on merge
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.path().join("notes.txt"), dir.path().join("link.txt"))
            .unwrap();
        fs::write(ws.child_dir.join("link.txt"), "hijack").unwrap();
        let err = ws
            .merge_into_parent(Some(&["link.txt".into()]), &[root_canon], &[], &limits)
            .unwrap_err();
        assert_eq!(err.code(), error_codes::PATH_DENIED);
    }

    let outer = ws.outer_dir.clone().unwrap();
    ws.cleanup().unwrap();
    assert!(!outer.exists());
}

#[test]
fn child_policy_remaps_globs_enforces_bypass_roots_and_denies_exec() {
    let parent_dir = tempdir().unwrap();
    let child_dir = tempdir().unwrap();
    fs::create_dir_all(parent_dir.path().join("src")).unwrap();
    fs::create_dir_all(parent_dir.path().join("secrets")).unwrap();
    fs::create_dir_all(child_dir.path().join("src")).unwrap();
    fs::create_dir_all(child_dir.path().join("secrets")).unwrap();
    fs::create_dir_all(child_dir.path().join("other")).unwrap();

    let cfg_toml = format!(
        r#"
[[rules]]
effect = "deny"
path_glob = "{}/secrets/**"

[[rules]]
effect = "ask"
path_glob = "{}/src/**"
"#,
        parent_dir.path().display(),
        parent_dir.path().display()
    );
    let perm_cfg = PermissionConfig::from_toml(&cfg_toml).unwrap();
    let parent_policy = Policy::new(parent_dir.path(), perm_cfg).unwrap();

    let child_policy = parent_policy
        .derive_for_subagent(child_dir.path(), true, &[PathBuf::from("src")], &[])
        .unwrap()
        .with_exec_denied();

    // Parent deny rule remapped onto child workspace still denies even under Bypass.
    let secret_op = Operation::file(
        child_dir.path(),
        "read_file",
        &child_dir.path().join("secrets/key.txt"),
    )
    .unwrap();
    assert_eq!(
        child_policy
            .evaluate(WorkMode::Work, PermissionMode::Bypass, &secret_op)
            .decision,
        Decision::Deny
    );

    // Narrowed `allowed_paths = ["src"]` blocks `other/file.txt` even under Bypass.
    let other_op = Operation::file(
        child_dir.path(),
        "read_file",
        &child_dir.path().join("other/file.txt"),
    )
    .unwrap();
    assert_eq!(
        child_policy
            .evaluate(WorkMode::Work, PermissionMode::Bypass, &other_op)
            .decision,
        Decision::Deny
    );

    // Remapped ask rule on `src/**` triggers Ask in Auto mode.
    let src_op = Operation::file(
        child_dir.path(),
        "write_file",
        &child_dir.path().join("src/main.rs"),
    )
    .unwrap();
    assert_eq!(
        child_policy
            .evaluate(WorkMode::Work, PermissionMode::Auto, &src_op)
            .decision,
        Decision::Ask
    );

    // `with_exec_denied` denies exec even under Bypass.
    let exec_op = Operation::exec(&crate::tools::exec::ExecPlan {
        argv: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
        cwd: child_dir.path().to_path_buf(),
        timeout: std::time::Duration::from_secs(5),
        environment_digest: "0".repeat(64),
    })
    .unwrap();
    assert_eq!(
        child_policy
            .evaluate(WorkMode::Work, PermissionMode::Bypass, &exec_op)
            .decision,
        Decision::Deny
    );
}
