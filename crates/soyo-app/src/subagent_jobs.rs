//! Background execution and operator commands for persistent subagents
//! (`F-09`, `D-110`).
//!
//! A persistent child is a `run_kind = "subagent"` task row. This module
//! assembles the same bounded child runtime the in-process supervisor builds,
//! drives exactly one attempt under a durable lease, and stores the child's
//! summary, changed-file manifest, and conflict checklist on the task record.
//! Unknown outcomes wait for explicit resolution and are never replayed.

use crate::{
    configure_host_sandbox, model,
    task_queue::{TaskRunContext, TaskRunOutcome},
};
use soyo_core::{
    contract::{BoundaryError, ErrorInfo, ModelProtocol, Run, RunId, Session, SessionId},
    credentials::SecretRegistry,
    permissions::Policy,
    provider::router::ProfileRouter,
    review::ReviewConfig,
    runtime::{AgentRuntime, RuntimeLimits, RuntimeSetup},
    subagent::{
        PersistentSubagentJob, SubagentConfig, SubagentIsolation, SubagentPersistence,
        SubagentSnapshot, supervisor::SubagentStore, workspace::MergeOutcome,
    },
    task_queue::{
        StoreSettings, TaskId, TaskRecord, TaskRunKind, TaskRunSummary, TaskStatus, TaskStore,
        timestamp,
    },
    tools::{
        exec::ExecConfig,
        read::{ReadLimits, ReadTools},
    },
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

fn failure(code: &str, message: &str) -> BoundaryError {
    BoundaryError(
        ErrorInfo {
            code: code.into(),
            message: message.into(),
        },
        None,
        None,
    )
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn task_secrets(config: &soyo_core::config::Config) -> Result<Vec<String>, BoundaryError> {
    config
        .secret_env_names()
        .iter()
        .map(|name| config.secret(name).map(str::to_owned))
        .collect()
}

/// Open persistent-subagent storage. The F-08 queue is the durability layer, so
/// `[tasks] enabled = true` is required.
pub fn open_subagent_persistence(
    config: &soyo_core::config::Config,
) -> Result<SubagentPersistence, BoundaryError> {
    if !config.tasks.enabled {
        return Err(failure(
            "task_queue_disabled",
            "persistent subagents require the task queue; set [tasks] enabled = true",
        ));
    }
    let store = TaskStore::open_with_settings(
        &config.data_dir,
        task_secrets(config)?,
        StoreSettings::default(),
    )?;
    SubagentPersistence::new(store, config.data_dir.join("subagents"))
}

fn subagent_limits(
    config: &soyo_core::config::Config,
) -> Result<soyo_core::subagent::SubagentLimits, BoundaryError> {
    config
        .subagents
        .resolve()?
        .map(|resolved| resolved.limits)
        .ok_or_else(|| failure("subagents_disabled", "subagents are not enabled"))
}

fn parse_task_id(raw: &str) -> Result<TaskId, BoundaryError> {
    raw.parse()
}

/// Load one persistent job by its stable subagent id (the task id string).
pub async fn load_job(
    config: &soyo_core::config::Config,
    raw: &str,
) -> Result<(TaskRecord, PersistentSubagentJob), BoundaryError> {
    let persistence = open_subagent_persistence(config)?;
    persistence
        .load_job(parse_task_id(raw)?, None)
        .await
        .map_err(|error| {
            if error.is_code(soyo_core::contract::error_codes::SUBAGENT_NOT_FOUND) {
                failure("subagent_not_found", "persistent subagent was not found")
            } else {
                error
            }
        })
}

/// List every persistent subagent job.
pub async fn list_jobs(
    config: &soyo_core::config::Config,
) -> Result<Vec<(TaskRecord, PersistentSubagentJob)>, BoundaryError> {
    open_subagent_persistence(config)?.list_jobs(None).await
}

/// Current snapshot projection for one persistent job.
pub async fn status(
    config: &soyo_core::config::Config,
    raw: &str,
) -> Result<SubagentSnapshot, BoundaryError> {
    let (task, job) = load_job(config, raw).await?;
    Ok(job.snapshot(&task))
}

/// Compute the changed-file manifest and bounded patch for a persistent job.
pub async fn diff(
    config: &soyo_core::config::Config,
    raw: &str,
    include_patch: bool,
) -> Result<soyo_core::subagent::SubagentDiffReport, BoundaryError> {
    let persistence = open_subagent_persistence(config)?;
    let (task, job) = load_job(config, raw).await?;
    if job.state(&task) == soyo_core::subagent::SubagentState::Running {
        return Err(failure(
            "subagent_conflict",
            "cannot inspect diff while the persistent subagent job is queued or running",
        ));
    }
    if job.discarded {
        return Err(failure(
            "subagent_conflict",
            "subagent workspace was discarded",
        ));
    }
    if job.isolation == SubagentIsolation::ReadOnly {
        return Err(failure(
            "invalid_args",
            "read_only subagents have no isolated workspace diff",
        ));
    }
    let limits = subagent_limits(config)?;
    let policy = Policy::new(&job.parent_workspace, config.permissions.clone())?;
    let protected = policy.protected_paths()?;
    let workspace = persistence.reopen(task.id, &job)?;
    workspace.compute_diff(raw, &protected, include_patch, &limits)
}

/// Result of one operator-driven merge.
pub struct MergeReport {
    pub task: TaskRecord,
    pub snapshot: SubagentSnapshot,
    pub receipts: Vec<soyo_core::subagent::SubagentMergeReceipt>,
    pub conflicts: Vec<soyo_core::subagent::SubagentConflict>,
}

/// Merge a completed persistent child into the current parent workspace with
/// the same three-way SHA-256 conflict detection used by `subagent_merge`.
/// Conflicts produce zero writes and are stored on the job.
pub async fn merge(
    config: &soyo_core::config::Config,
    raw: &str,
    paths: Option<Vec<String>>,
    discard_after: bool,
) -> Result<MergeReport, BoundaryError> {
    let persistence = open_subagent_persistence(config)?;
    let (task, job) = load_job(config, raw).await?;
    if !matches!(
        task.status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    ) {
        return Err(failure(
            "subagent_conflict",
            "cannot merge a persistent subagent that is not in a terminal state",
        ));
    }
    if job.discarded {
        return Err(failure(
            "subagent_conflict",
            "subagent workspace was discarded",
        ));
    }
    if job.isolation == SubagentIsolation::ReadOnly {
        return Err(failure(
            "invalid_args",
            "read_only subagents have no isolated changes to merge",
        ));
    }
    let limits = subagent_limits(config)?;
    let policy = Policy::new(&job.parent_workspace, config.permissions.clone())?;
    let protected = policy.protected_paths()?;
    let roots = policy.roots().to_vec();
    let workspace = persistence.reopen(task.id, &job)?;
    match workspace.merge_into_parent(paths.as_deref(), &roots, &protected, &limits)? {
        MergeOutcome::Conflict(conflicts) => {
            let stored = conflicts.clone();
            let (task, job) = persistence
                .update_job(task.id, |job| {
                    job.conflicts = stored.clone();
                })
                .await?;
            Ok(MergeReport {
                snapshot: job.snapshot(&task),
                task,
                receipts: Vec::new(),
                conflicts,
            })
        }
        MergeOutcome::Applied(receipts) => {
            if discard_after {
                workspace.cleanup()?;
                let _ = persistence.remove_job_dir(task.id);
            }
            let stored = receipts.clone();
            let (task, job) = persistence
                .update_job(task.id, |job| {
                    job.merge_receipts.extend(stored.clone());
                    job.conflicts.clear();
                    job.merged = true;
                    if discard_after {
                        job.discarded = true;
                    }
                })
                .await?;
            Ok(MergeReport {
                snapshot: job.snapshot(&task),
                task,
                receipts,
                conflicts: Vec::new(),
            })
        }
    }
}

/// Cancel a queued or running persistent job and optionally remove its isolated
/// workspace. A running worker observes the cancellation through its control
/// poll and stops before the next model or tool boundary.
pub async fn cancel(
    config: &soyo_core::config::Config,
    raw: &str,
    discard: bool,
) -> Result<SubagentSnapshot, BoundaryError> {
    let persistence = open_subagent_persistence(config)?;
    let (task, job) = load_job(config, raw).await?;
    if !task.status.is_terminal() {
        persistence
            .store()
            .transition(task.id, TaskStatus::Cancelled, timestamp()?)
            .await?;
    }
    if discard && !job.discarded {
        if let Ok(workspace) = persistence.reopen(task.id, &job) {
            workspace.cleanup()?;
        } else if let Some(scratch) = &job.scratch_root {
            let _ = std::fs::remove_dir_all(scratch);
        }
        let _ = persistence.remove_job_dir(task.id);
        let (task, job) = persistence
            .update_job(task.id, |job| {
                job.discarded = true;
            })
            .await?;
        return Ok(job.snapshot(&task));
    }
    let (task, job) = persistence.load_job(task.id, None).await?;
    Ok(job.snapshot(&task))
}

/// Resolve a persistent job waiting on an unknown outcome. `retry` explicitly
/// requeues it; otherwise it is blocked for external input. Automatic replay is
/// never performed.
pub async fn resolve(
    config: &soyo_core::config::Config,
    raw: &str,
    retry: bool,
) -> Result<SubagentSnapshot, BoundaryError> {
    let persistence = open_subagent_persistence(config)?;
    let task_id = parse_task_id(raw)?;
    let (task, _) = persistence.load_job(task_id, None).await?;
    let task = persistence
        .store()
        .resolve_unknown(task.id, retry, timestamp()?)
        .await?;
    let job = task.subagent.clone().ok_or_else(|| {
        failure(
            "subagent_not_found",
            "persistent subagent job payload is missing",
        )
    })?;
    Ok(job.snapshot(&task))
}

/// Recover in-flight subagent jobs whose worker lease expired. They become
/// `waiting_unknown` and are never replayed automatically.
pub async fn recover(
    config: &soyo_core::config::Config,
) -> Result<Vec<SubagentSnapshot>, BoundaryError> {
    let recovered = crate::task_queue::recover_tasks(config).await?;
    Ok(recovered
        .into_iter()
        .filter(|task| task.run_kind == TaskRunKind::Subagent)
        .filter_map(|task| task.subagent.as_ref().map(|job| job.snapshot(&task)))
        .collect())
}

fn child_session(
    config: &soyo_core::config::Config,
    task: &TaskRecord,
    job: &PersistentSubagentJob,
) -> Result<Session, BoundaryError> {
    let selected = config
        .profiles
        .get(&task.profile)
        .ok_or_else(|| failure("invalid_task", "persistent subagent profile is missing"))?;
    let protocol = match selected.protocol {
        soyo_core::config::Protocol::ChatCompletions => ModelProtocol::ChatCompletions,
        soyo_core::config::Protocol::Responses => ModelProtocol::Responses,
        soyo_core::config::Protocol::AnthropicMessages => ModelProtocol::AnthropicMessages,
    };
    Ok(Session {
        id: SessionId::new(),
        workspace: job.workspace_root.clone(),
        work_mode: task.mode,
        profile: task.profile.clone(),
        protocol,
        model: selected.model.clone(),
        history: vec![soyo_core::contract::HistoryItem::UserText(
            task.input.clone(),
        )],
    })
}

struct ChildRuntimeRequest<'a> {
    task: &'a TaskRecord,
    job: &'a PersistentSubagentJob,
    workspace: &'a soyo_core::subagent::workspace::IsolatedWorkspace,
    persistence: &'a SubagentPersistence,
    session: &'a Session,
    run_id: RunId,
    child_store: Arc<SubagentStore>,
    context: &'a mut TaskRunContext,
}

fn build_child_runtime(
    config: &soyo_core::config::Config,
    request: ChildRuntimeRequest<'_>,
) -> Result<AgentRuntime, BoundaryError> {
    let ChildRuntimeRequest {
        task,
        job,
        workspace,
        persistence,
        session,
        run_id,
        child_store,
        context,
    } = request;
    let selected = config
        .profiles
        .get(&task.profile)
        .ok_or_else(|| failure("invalid_task", "persistent subagent profile is missing"))?;
    let limits_cfg = &config.limits;
    let known_secrets = task_secrets(config)?;
    let registry = Arc::new(SecretRegistry::default());
    registry.register(&known_secrets)?;

    let models = config
        .profiles
        .iter()
        .map(|(name, profile)| {
            Ok((
                name.clone(),
                model(
                    config,
                    name,
                    profile,
                    profile.prompt_cache
                        || (context.anthropic_prompt_cache && name == &task.profile),
                )?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, BoundaryError>>()?;
    let routing = crate::routing_coordinator(config, None);
    let router =
        Arc::new(ProfileRouter::new(task.profile.clone(), models)?.with_routing(routing.clone()));

    let parent_policy = Policy::new(&job.parent_workspace, config.permissions.clone())?;
    let (mut sandbox, broker) = config
        .sandbox
        .resolve_with_environment(&job.parent_workspace, config.environment())?;
    configure_host_sandbox(config, &job.parent_workspace, &mut sandbox)
        .map_err(|error| failure("sandbox_prepare_failed", &error.to_string()))?;
    let mut extra_protected = parent_policy.protected_paths()?;
    for path in &sandbox.protected_paths {
        if !extra_protected.contains(path) {
            extra_protected.push(path.clone());
        }
    }
    let allowed_rel_paths = job
        .allowed_paths
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let isolated = job.isolation == SubagentIsolation::Isolated;
    let mut child_policy = parent_policy.derive_for_subagent(
        &job.workspace_root,
        isolated,
        &allowed_rel_paths,
        &extra_protected,
    )?;
    if !job.allow_exec {
        child_policy = child_policy.with_exec_denied();
    }
    let child_policy = Arc::new(child_policy);
    let files = Arc::new(
        ReadTools::new_restricted(
            &job.workspace_root,
            child_policy.roots(),
            ReadLimits {
                read_return_bytes: limits_cfg.read_return_max_bytes,
                file_bytes: limits_cfg.read_file_max_bytes,
                search_results: limits_cfg.search_max_results,
                search_bytes: limits_cfg.search_return_max_bytes,
                list_entries: limits_cfg.list_max_entries,
                list_bytes: limits_cfg.list_return_max_bytes,
            },
        )
        .map_err(|error| {
            failure(
                "subagent_workspace_error",
                &format!("cannot configure persistent child read roots: {error}"),
            )
        })?,
    );

    let subagent_limits = subagent_limits(config)?;
    let timeout = Duration::from_secs(job.timeout_seconds);
    let mut limits = RuntimeLimits {
        model_requests: limits_cfg
            .max_model_requests
            .min(subagent_limits.max_model_requests),
        tool_calls: limits_cfg
            .max_tool_calls
            .min(subagent_limits.max_tool_calls),
        active_time: Duration::from_secs(limits_cfg.active_run_timeout_seconds).min(timeout),
        model_connect: Duration::from_secs(limits_cfg.model_connect_timeout_seconds),
        model_idle: Duration::from_secs(limits_cfg.model_idle_timeout_seconds),
        model_total: Duration::from_secs(limits_cfg.model_timeout_seconds),
        tool_timeout: Duration::from_secs(limits_cfg.command_timeout_seconds),
        background_running: limits_cfg.max_background_running,
        background_jobs: limits_cfg.max_background_jobs,
        write_max_bytes: limits_cfg.write_max_bytes,
        ..Default::default()
    };
    limits.model_requests = limits.model_requests.max(1);
    limits.tool_calls = limits.tool_calls.max(1);
    let mut review = ReviewConfig::new(task.profile.clone());
    review.review_profile = config.review.profile.clone();
    review.max_output_tokens = config.review.max_output_tokens;
    review.connect_timeout = limits.model_connect;
    review.idle_timeout = limits.model_idle;
    review.total_timeout = limits.model_total;
    review.known_secrets = known_secrets.clone();
    let exec = ExecConfig {
        execution: None,
        environment: config.environment().clone(),
        default_timeout: Duration::from_secs(limits_cfg.command_timeout_seconds),
        max_timeout: Duration::from_secs(limits_cfg.max_command_timeout_seconds),
        stdout_max: limits_cfg.command_stdout_max_bytes,
        stderr_max: limits_cfg.command_stderr_max_bytes,
        inherit_extra: config.commands.inherit_env.clone(),
        secret_env_names: config.secret_env_names(),
    };

    let setup = RuntimeSetup {
        run: Run {
            id: run_id,
            session_id: session.id,
            work_mode: task.mode,
            permissions: task.permissions,
        },
        model: router,
        store: child_store,
        policy: child_policy,
        files,
        cancellation: context.cancellation.clone(),
        review,
        exec,
        preauthorizations: context.approvals.take().unwrap_or_default(),
        known_secrets,
        summary: format!("Persistent subagent `{}` ({})", job.label, task.id.0),
        confirmation: context.confirmation.clone(),
        limits: limits.clone(),
    };
    let mut runtime = if isolated {
        AgentRuntime::new(setup)?
    } else {
        AgentRuntime::new_read_only(setup)?
    };
    if !job.allow_exec {
        runtime = runtime.with_exec_disabled();
    }
    runtime = runtime.with_routing(routing);
    let compact = soyo_core::compaction::CompactionConfig {
        summary_output_tokens: config
            .context
            .summary_max_output_tokens
            .unwrap_or(selected.max_output_tokens),
        max_model_calls: config.context.max_compaction_calls,
        connect_timeout: limits.model_connect.min(Duration::from_secs(3600)),
        idle_timeout: limits.model_idle.min(Duration::from_secs(3600)),
        request_timeout: if limits.model_total.is_zero() {
            Duration::from_secs(3600)
        } else {
            limits.model_total.min(Duration::from_secs(3600))
        },
        operation_timeout: Duration::from_secs(config.context.compaction_timeout_seconds)
            .min(limits.active_time),
        retry_jitter_max: limits.retry_jitter_max,
        ..Default::default()
    };
    runtime = runtime.with_context_settings(
        config.context.budget(selected),
        config.context.auto_compact,
        compact,
    )?;
    // A persistent child may itself delegate when the stored depth leaves room;
    // its limits and cancellation remain bounded by this worker run.
    let child_supervisor = if job.depth < subagent_limits.max_depth {
        Some(SubagentConfig::at_depth(subagent_limits.clone(), job.depth))
    } else {
        None
    };
    runtime = runtime.with_subagents(child_supervisor)?;
    runtime = runtime.with_subagent_persistence(Some(persistence.clone()));
    runtime = runtime.with_secret_registry(registry)?;
    runtime = runtime.with_pricing(
        selected.pricing.clone(),
        config
            .profiles
            .get(config.review.profile.as_ref().unwrap_or(&task.profile))
            .and_then(|profile| profile.pricing.clone()),
    );
    if job.allow_exec {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let child_sandbox = workspace.derive_child_sandbox(&sandbox, true, home.as_deref());
        runtime = runtime.with_execution_settings(child_sandbox, broker)?;
    }
    if let Some(input) = &context.input {
        runtime = runtime.with_input_handler(input.clone());
    }
    runtime = runtime.with_personal_context(context.personal_context.clone());
    Ok(runtime)
}

/// Drive exactly one queued persistent-subagent attempt under a durable lease.
pub async fn run_subagent_once(
    config: &soyo_core::config::Config,
    task_id: TaskId,
    mut context: TaskRunContext,
) -> Result<TaskRunOutcome, BoundaryError> {
    let persistence = open_subagent_persistence(config)?;
    let store = persistence.store().clone();
    let owner = format!("subagent-{}", now_ms());
    let ttl = config
        .limits
        .active_run_timeout_seconds
        .saturating_add(60)
        .max(60)
        .saturating_mul(1000);
    let lease = store
        .acquire_lease(task_id, owner, now_ms(), Some(ttl))
        .await?;
    let result = async {
        let task = store.load(task_id).await?;
        if task.run_kind != TaskRunKind::Subagent {
            return Err(failure(
                "invalid_task_state",
                "the durable task is not a persistent subagent job",
            ));
        }
        if task.status != TaskStatus::Queued {
            return Err(failure(
                "invalid_task_state",
                "persistent subagent must be queued before running an attempt",
            ));
        }
        let job = task.subagent.clone().ok_or_else(|| {
            failure(
                "subagent_not_found",
                "persistent subagent job payload is missing",
            )
        })?;
        let run_id = RunId::new();
        let dispatch = store
            .begin_attempt(&lease, run_id, now_ms(), timestamp()?)
            .await?;
        store.mark_running(&dispatch, &lease, now_ms()).await?;
        let workspace = persistence.reopen(task.id, &job)?;
        let session = child_session(config, &task, &job)?;
        let child_store = Arc::new(SubagentStore::new(session.clone(), Vec::new()));
        let mut runtime = build_child_runtime(
            config,
            ChildRuntimeRequest {
                task: &task,
                job: &job,
                workspace: &workspace,
                persistence: &persistence,
                session: &session,
                run_id,
                child_store: child_store.clone(),
                context: &mut context,
            },
        )?;

        let heartbeat_store = store.clone();
        let mut heartbeat_lease = lease.clone();
        let heartbeat_ttl = lease.expires_at_ms.saturating_sub(now_ms()).max(1);
        let heartbeat_wait = Duration::from_millis((heartbeat_ttl / 3).clamp(1_000, 30_000));
        let heartbeat_error = Arc::new(std::sync::Mutex::new(None::<BoundaryError>));
        let heartbeat_failure = heartbeat_error.clone();
        let heartbeat_cancellation = context.cancellation.clone();
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::time::sleep(heartbeat_wait).await;
                if let Err(error) = heartbeat_store
                    .renew_lease(&mut heartbeat_lease, now_ms(), Some(heartbeat_ttl))
                    .await
                {
                    heartbeat_cancellation.cancel();
                    if let Ok(mut failure) = heartbeat_failure.lock() {
                        *failure = Some(error);
                    }
                    break;
                }
            }
        });
        let control_store = store.clone();
        let control_cancellation = context.cancellation.clone();
        let control_task_id = task.id;
        let control = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                match control_store.load(control_task_id).await {
                    Ok(current)
                        if matches!(current.status, TaskStatus::Paused | TaskStatus::Cancelled) =>
                    {
                        control_cancellation.cancel();
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        control_cancellation.cancel();
                        break;
                    }
                }
            }
        });

        let mut session = session;
        let outcome = runtime.run(&mut session).await;
        let metrics = runtime
            .metrics_snapshot()
            .map(|(metrics, _, _)| metrics)
            .unwrap_or_default();
        heartbeat.abort();
        control.abort();
        let _ = heartbeat.await;
        let _ = control.await;
        if let Some(error) = heartbeat_error
            .lock()
            .ok()
            .and_then(|mut failure| failure.take())
        {
            return Err(error);
        }
        let result = outcome?;

        let id = task.id.0.to_string();
        // A manifest failure must not erase a completed Run's outcome; the
        // operator can re-run `subagent diff` to see the underlying limit
        // error.
        let (changed_files, conflicts) =
            if job.isolation == SubagentIsolation::Isolated && !job.discarded {
                let policy = Policy::new(&job.parent_workspace, config.permissions.clone())?;
                let protected = policy.protected_paths()?;
                let limits = subagent_limits(config)?;
                match workspace.compute_diff(&id, &protected, false, &limits) {
                    Ok(report) => {
                        let conflicts = report
                            .changed_files
                            .iter()
                            .filter(|change| change.has_conflict)
                            .map(|change| soyo_core::subagent::SubagentConflict {
                                path: change.path.clone(),
                                reason: "parent file changed since subagent fork".into(),
                                base_sha256: change.base_sha256.clone(),
                                parent_sha256: None,
                                subagent_sha256: change.new_sha256.clone(),
                            })
                            .collect();
                        (report.changed_files, conflicts)
                    }
                    Err(_) => (Vec::new(), Vec::new()),
                }
            } else {
                (Vec::new(), Vec::new())
            };

        let mut updated = job.clone();
        updated.summary = result.text.clone();
        updated.changed_files = changed_files;
        updated.conflicts = conflicts;
        updated.step_traces = child_store.take_traces();
        updated.execution_requests = metrics.execution_requests;
        updated.tool_calls = metrics.tool_calls;
        updated.usage = metrics.execution_usage.clone();
        let summary = TaskRunSummary::from_result(&result, timestamp()?)?;
        store
            .finish_subagent_attempt(&lease, &dispatch, summary, &metrics, updated, timestamp()?)
            .await
            .map(|task| TaskRunOutcome {
                task,
                result,
                streamed: false,
            })
    }
    .await;
    let release = store.release_lease(&lease).await;
    match (result, release) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}
