use super::*;
use crate::{
    contract::{PermissionMode, RunStatus, Usage},
    metrics::RunMetrics,
};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

fn record() -> TaskRecord {
    TaskRecord::new(TaskSeed {
        session_id: SessionId::new(),
        workspace: PathBuf::from("/workspace"),
        profile: "fixture".into(),
        mode: WorkMode::Build,
        permissions: PermissionMode::Auto,
        input: "Deliver a verified API".into(),
        now: "2026-11-24T00:00:00Z".into(),
    })
    .unwrap()
}

fn summary(run_id: RunId, now: &str) -> TaskRunSummary {
    TaskRunSummary {
        run_id,
        status: RunStatus::Completed,
        stop_reason: None,
        text: Some("task completed".into()),
        error_code: None,
        persistence_saved: true,
        finished_at: now.into(),
    }
}

fn failed_summary(run_id: RunId, now: &str, error_code: &str, saved: bool) -> TaskRunSummary {
    TaskRunSummary {
        run_id,
        status: RunStatus::Failed,
        stop_reason: Some("model_failure".into()),
        text: None,
        error_code: Some(error_code.into()),
        persistence_saved: saved,
        finished_at: now.into(),
    }
}

fn metrics() -> RunMetrics {
    RunMetrics {
        execution_requests: 2,
        review_requests: 1,
        compaction_requests: 0,
        tool_calls: 3,
        tool_executions: 3,
        duration_ms: 2_500,
        execution_usage: Some(Usage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            cached_input_tokens: None,
            estimated: false,
        }),
        review_usage: None,
        compaction_usage: None,
        execution_cost: None,
        review_cost: None,
        compaction_cost: None,
        by_profile: Default::default(),
        route_decisions: vec![],
    }
}

#[tokio::test]
async fn store_keeps_private_revisioned_tasks_and_exclusive_leases() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("workspace")).unwrap();
    let store = TaskStore::open(&root.path().join("data"), vec![]).unwrap();
    let mut task = record();
    task.workspace = fs::canonicalize(root.path().join("workspace")).unwrap();
    let mut task = store.create(task).await.unwrap();
    let loaded = store.load(task.id).await.unwrap();
    assert_eq!(loaded, task);
    assert_eq!(store.list().await.unwrap().len(), 1);
    assert_eq!(
        fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(store.path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    let owner = "worker-a".to_owned();
    store
        .acquire_lease(task.id, owner.clone(), 1_000, Some(10_000))
        .await
        .unwrap();
    assert!(
        store
            .acquire_lease(task.id, "worker-b".into(), 2_000, Some(10_000))
            .await
            .is_err()
    );
    // The same owner may re-acquire while its lease is valid.
    store
        .acquire_lease(task.id, owner.clone(), 2_000, Some(10_000))
        .await
        .unwrap();

    task.input = "Updated input".into();
    task.updated_at = "2026-11-24T00:03:00Z".into();
    let stale = task.revision;
    task = store.save(task, stale).await.unwrap();
    assert_eq!(task.revision, stale + 1);
    let mut conflicting = task.clone();
    conflicting.updated_at = "2026-11-24T00:04:00Z".into();
    assert!(store.save(conflicting, stale).await.is_err());
}

#[tokio::test]
async fn several_tasks_may_share_one_workspace() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("workspace")).unwrap();
    let store = TaskStore::open(&root.path().join("data"), vec![]).unwrap();
    let workspace = fs::canonicalize(root.path().join("workspace")).unwrap();
    let mut ids = Vec::new();
    for index in 0..2 {
        let mut task = record();
        task.workspace = workspace.clone();
        task.input = format!("Task {index}");
        ids.push(store.create(task).await.unwrap().id);
    }
    store
        .acquire_lease(ids[0], "worker-a".into(), 1_000, Some(60_000))
        .await
        .unwrap();
    // A second task in the same workspace is leaseable by another worker.
    store
        .acquire_lease(ids[1], "worker-b".into(), 1_000, Some(60_000))
        .await
        .unwrap();
}

#[tokio::test]
async fn task_store_rejects_configured_credentials_without_publishing() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("workspace")).unwrap();
    let store =
        TaskStore::open(&root.path().join("data"), vec!["task-secret-token".into()]).unwrap();
    let mut task = record();
    task.workspace = fs::canonicalize(root.path().join("workspace")).unwrap();
    task.input = "Use task-secret-token without persisting it".into();
    let error = store.create(task).await.unwrap_err();
    assert_eq!(error.0.code, "storage_sensitive_data");
    assert!(store.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn lease_expiry_and_renewal_control_ownership() {
    let root = tempfile::tempdir().unwrap();
    let store = TaskStore::open(&root.path().join("data"), vec![]).unwrap();
    let task = store.create(record()).await.unwrap();
    let mut lease = store
        .acquire_lease(task.id, "worker-a".into(), 1_000, Some(10_000))
        .await
        .unwrap();
    assert!(
        store
            .acquire_lease(task.id, "worker-b".into(), 2_000, Some(10_000))
            .await
            .is_err()
    );
    store
        .renew_lease(&mut lease, 2_000, Some(10_000))
        .await
        .unwrap();
    assert_eq!(lease.expires_at_ms, 12_000);
    assert!(
        store
            .acquire_lease(task.id, "worker-b".into(), 11_000, Some(10_000))
            .await
            .is_err()
    );
    // After expiry another owner can take over.
    store
        .acquire_lease(task.id, "worker-b".into(), 12_000, Some(10_000))
        .await
        .unwrap();
    // The stale owner can no longer renew.
    assert!(
        store
            .renew_lease(&mut lease, 12_500, Some(10_000))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn begin_and_finish_attempt_record_terminal_outcomes() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("workspace")).unwrap();
    let store = TaskStore::open(&root.path().join("data"), vec![]).unwrap();
    let mut task = record();
    task.workspace = fs::canonicalize(root.path().join("workspace")).unwrap();
    let task = store.create(task).await.unwrap();
    let lease = store
        .acquire_lease(task.id, "worker".into(), 1_000, Some(60_000))
        .await
        .unwrap();
    let run_id = RunId::new();
    let dispatch = store
        .begin_attempt(&lease, run_id, 2_000, "2026-11-24T00:03:00Z".into())
        .await
        .unwrap();
    assert_eq!(dispatch.attempt, 1);
    store.mark_running(&dispatch, &lease, 3_000).await.unwrap();
    assert_eq!(
        store.load(task.id).await.unwrap().status,
        TaskStatus::Running
    );
    // A second attempt cannot begin while one is in flight.
    assert!(
        store
            .begin_attempt(&lease, RunId::new(), 3_500, "2026-11-24T00:03:30Z".into())
            .await
            .is_err()
    );
    let finished = store
        .finish_attempt(
            &lease,
            &dispatch,
            summary(dispatch.run_id, "2026-11-24T00:04:00Z"),
            &metrics(),
            "2026-11-24T00:04:01Z".into(),
        )
        .await
        .unwrap();
    assert_eq!(finished.status, TaskStatus::Completed);
    assert_eq!(
        finished.last_result.as_ref().map(|result| result.run_id),
        Some(dispatch.run_id)
    );

    // A failed run records a terminal failure.
    let mut second = record();
    second.workspace = task.workspace.clone();
    let second = store.create(second).await.unwrap();
    let lease = store
        .acquire_lease(second.id, "worker".into(), 5_000, Some(60_000))
        .await
        .unwrap();
    let dispatch = store
        .begin_attempt(&lease, RunId::new(), 5_500, "2026-11-24T00:05:00Z".into())
        .await
        .unwrap();
    let failed = store
        .finish_attempt(
            &lease,
            &dispatch,
            failed_summary(
                dispatch.run_id,
                "2026-11-24T00:06:00Z",
                "model_failure",
                true,
            ),
            &metrics(),
            "2026-11-24T00:06:01Z".into(),
        )
        .await
        .unwrap();
    assert_eq!(failed.status, TaskStatus::Failed);
    assert!(failed.failure.is_some());
}

#[tokio::test]
async fn unpersisted_or_cancelled_runs_wait_for_resolution_without_replay() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("workspace")).unwrap();
    let store = TaskStore::open(&root.path().join("data"), vec![]).unwrap();
    for (index, result) in [
        TaskRunSummary {
            run_id: RunId::new(),
            status: RunStatus::Completed,
            stop_reason: None,
            text: None,
            error_code: None,
            persistence_saved: false,
            finished_at: "2026-11-24T01:00:00Z".into(),
        },
        TaskRunSummary {
            run_id: RunId::new(),
            status: RunStatus::Cancelled,
            stop_reason: Some("cancelled".into()),
            text: None,
            error_code: Some("cancelled".into()),
            persistence_saved: true,
            finished_at: "2026-11-24T01:10:00Z".into(),
        },
        failed_summary(
            RunId::new(),
            "2026-11-24T01:20:00Z",
            "tool_outcome_unknown",
            true,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut task = record();
        task.workspace = fs::canonicalize(root.path().join("workspace")).unwrap();
        let task = store.create(task).await.unwrap();
        let lease = store
            .acquire_lease(task.id, format!("worker-{index}"), 1_000, Some(60_000))
            .await
            .unwrap();
        let dispatch = store
            .begin_attempt(
                &lease,
                result.run_id,
                2_000,
                format!("2026-11-24T01:0{index}:00Z"),
            )
            .await
            .unwrap();
        store.mark_running(&dispatch, &lease, 2_001).await.unwrap();
        let finished = store
            .finish_attempt(
                &lease,
                &dispatch,
                result,
                &metrics(),
                format!("2026-11-24T01:3{index}:00Z"),
            )
            .await
            .unwrap();
        assert_eq!(finished.status, TaskStatus::WaitingUnknown);
        assert_eq!(
            finished
                .failure
                .as_ref()
                .map(|failure| failure.code.as_str()),
            Some("task_outcome_unknown")
        );
        // Waiting tasks are never dispatched again automatically.
        assert!(
            store
                .begin_attempt(&lease, RunId::new(), 3_000, "2026-11-24T01:40:00Z".into())
                .await
                .is_err()
        );
        store.release_lease(&lease).await.unwrap();
    }
}

#[tokio::test]
async fn recover_unknown_marks_expired_dispatches_without_replaying() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("workspace")).unwrap();
    let store = TaskStore::open(&root.path().join("data"), vec![]).unwrap();
    let mut task = record();
    task.workspace = fs::canonicalize(root.path().join("workspace")).unwrap();
    let task = store.create(task).await.unwrap();
    let lease = store
        .acquire_lease(task.id, "worker".into(), 1_000, Some(60_000))
        .await
        .unwrap();
    let run_id = RunId::new();
    let dispatch = store
        .begin_attempt(&lease, run_id, 2_000, "2026-11-24T02:00:00Z".into())
        .await
        .unwrap();
    store.mark_running(&dispatch, &lease, 3_000).await.unwrap();
    // While the lease is valid nothing is recovered.
    assert!(
        store
            .recover_unknown(4_000, "2026-11-24T02:04:00Z".into())
            .await
            .unwrap()
            .is_empty()
    );
    // Once the lease expires the in-flight attempt is marked unknown.
    let recovered = store
        .recover_unknown(u64::MAX, "2026-11-24T02:05:00Z".into())
        .await
        .unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].status, TaskStatus::WaitingUnknown);
    // Recovery never requeues: the task cannot be dispatched again.
    assert!(
        store
            .begin_attempt(
                &lease,
                RunId::new(),
                u64::MAX,
                "2026-11-24T02:06:00Z".into()
            )
            .await
            .is_err()
    );
    // Explicit retry resolution returns the task to the queue.
    let resolved = store
        .resolve_unknown(task.id, true, "2026-11-24T02:07:00Z".into())
        .await
        .unwrap();
    assert_eq!(resolved.status, TaskStatus::Queued);
    assert!(resolved.failure.is_none());
    // The crashed worker's lease row survives recovery; a new owner can only
    // take over once it has actually expired (expires_at_ms = 61_000).
    let lease = store
        .acquire_lease(task.id, "worker-b".into(), 62_000, Some(60_000))
        .await
        .unwrap();
    let retry = store
        .begin_attempt(&lease, RunId::new(), 62_500, "2026-11-24T02:08:00Z".into())
        .await
        .unwrap();
    assert_eq!(retry.attempt, 2);

    // A non-retry resolution blocks the task for external input.
    let mut other = record();
    other.workspace = task.workspace.clone();
    let other = store.create(other).await.unwrap();
    let lease = store
        .acquire_lease(other.id, "worker".into(), 1_000, Some(5_000))
        .await
        .unwrap();
    let dispatch = store
        .begin_attempt(&lease, RunId::new(), 1_500, "2026-11-24T02:09:00Z".into())
        .await
        .unwrap();
    store.mark_running(&dispatch, &lease, 1_600).await.unwrap();
    store
        .recover_unknown(u64::MAX, "2026-11-24T02:10:00Z".into())
        .await
        .unwrap();
    let blocked = store
        .resolve_unknown(other.id, false, "2026-11-24T02:11:00Z".into())
        .await
        .unwrap();
    assert_eq!(blocked.status, TaskStatus::Blocked);
}

#[tokio::test]
async fn transition_pauses_resumes_and_cancels_conservatively() {
    let root = tempfile::tempdir().unwrap();
    let store = TaskStore::open(&root.path().join("data"), vec![]).unwrap();
    let task = store.create(record()).await.unwrap();
    let paused = store
        .transition(task.id, TaskStatus::Paused, "2026-11-24T03:00:00Z".into())
        .await
        .unwrap();
    assert_eq!(paused.status, TaskStatus::Paused);
    // Paused tasks cannot be dispatched.
    let lease = store
        .acquire_lease(task.id, "worker".into(), 1_000, Some(60_000))
        .await
        .unwrap();
    assert!(
        store
            .begin_attempt(&lease, RunId::new(), 1_500, "2026-11-24T03:01:00Z".into())
            .await
            .is_err()
    );
    let resumed = store
        .transition(task.id, TaskStatus::Queued, "2026-11-24T03:02:00Z".into())
        .await
        .unwrap();
    assert_eq!(resumed.status, TaskStatus::Queued);
    store
        .begin_attempt(&lease, RunId::new(), 1_600, "2026-11-24T03:03:00Z".into())
        .await
        .unwrap();
    // Resume is only allowed from Paused.
    assert!(
        store
            .transition(task.id, TaskStatus::Queued, "2026-11-24T03:04:00Z".into())
            .await
            .is_err()
    );
    let cancelled = store
        .transition(
            task.id,
            TaskStatus::Cancelled,
            "2026-11-24T03:05:00Z".into(),
        )
        .await
        .unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    // Terminal tasks cannot transition further.
    assert!(
        store
            .transition(task.id, TaskStatus::Paused, "2026-11-24T03:06:00Z".into())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn restart_preserves_state_and_marks_in_flight_unknown() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("workspace")).unwrap();
    let data_dir = root.path().join("data");
    let workspace = fs::canonicalize(root.path().join("workspace")).unwrap();
    let (task_id, run_id) = {
        let store = TaskStore::open(&data_dir, vec![]).unwrap();
        let mut queued = record();
        queued.workspace = workspace.clone();
        let _queued = store.create(queued).await.unwrap();
        let mut running = record();
        running.workspace = workspace.clone();
        let running = store.create(running).await.unwrap();
        let lease = store
            .acquire_lease(running.id, "worker".into(), 1_000, Some(60_000))
            .await
            .unwrap();
        let run_id = RunId::new();
        let dispatch = store
            .begin_attempt(&lease, run_id, 2_000, "2026-11-24T04:00:00Z".into())
            .await
            .unwrap();
        store.mark_running(&dispatch, &lease, 3_000).await.unwrap();
        (running.id, run_id)
    };
    // Reopen the store: queued tasks stay queued and the in-flight attempt is
    // recovered as unknown rather than replayed.
    let store = TaskStore::open(&data_dir, vec![]).unwrap();
    let tasks = store.list().await.unwrap();
    assert_eq!(tasks.len(), 2);
    let recovered = store
        .recover_unknown(u64::MAX, "2026-11-24T04:05:00Z".into())
        .await
        .unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].id, task_id);
    assert_eq!(recovered[0].status, TaskStatus::WaitingUnknown);
    assert_eq!(recovered[0].last_run_id, Some(run_id));
    let reloaded = store.load(task_id).await.unwrap();
    assert_eq!(reloaded.status, TaskStatus::WaitingUnknown);
}

#[test]
fn task_records_enforce_schema_and_terminal_invariants() {
    let task = record();
    assert_eq!(task.status, TaskStatus::Queued);

    let mut invalid = task.clone();
    invalid.input = "   ".into();
    assert!(invalid.validate().is_err());
    let mut invalid = task.clone();
    invalid.workspace = PathBuf::from("relative/path");
    assert!(invalid.validate().is_err());
    let mut invalid = task.clone();
    invalid.status = TaskStatus::Running;
    assert!(invalid.validate().is_err());
    let mut invalid = task.clone();
    invalid.status = TaskStatus::Completed;
    invalid.failure = Some(ErrorInfo {
        code: "x".into(),
        message: "y".into(),
    });
    assert!(invalid.validate().is_err());
    let mut invalid = task.clone();
    invalid.last_result = Some(summary(RunId::new(), "2026-11-24T05:00:00Z"));
    assert!(invalid.validate().is_err());
}
