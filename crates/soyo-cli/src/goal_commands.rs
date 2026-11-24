//! CLI adapter for durable Goal planning, bounded continuation and reports.

use super::{Cancel, Cli, Terminal, personal_context, safe_output, signals};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};
use soyo_app::goal::{GoalRunContext, configured_auto_goal, continue_once, now_ms};
use soyo_core::{
    approvals::Preauthorizations,
    boundary::Cancellation,
    config::{Config, Output},
    contract::{BoundaryError, ErrorInfo, PermissionMode},
    goal::{
        GoalBudget, GoalId, GoalPhase, GoalPlan, GoalRecord, GoalStatus, GoalStore, GoalWaitReason,
        StepId, StepVerifier,
    },
};
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

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

fn start_signals(cancel: Arc<Cancel>) -> Result<signals::CancellationListener, BoundaryError> {
    signals::CancellationListener::start(cancel)
        .map_err(|_| failure("signal_error", "Goal signal handling is unavailable"))
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum GoalReportFormat {
    Json,
    Markdown,
}

#[derive(Debug, Clone, Args)]
pub struct GoalBudgetArgs {
    #[arg(long)]
    pub max_runs: Option<u32>,
    #[arg(long)]
    pub max_model_requests: Option<u64>,
    #[arg(long)]
    pub max_tool_calls: Option<u64>,
    #[arg(long)]
    pub max_active_seconds: Option<u64>,
    #[arg(long)]
    pub max_tokens: Option<u64>,
    #[arg(long)]
    pub max_planning_attempts: Option<u32>,
    #[arg(long)]
    pub max_verification_attempts: Option<u32>,
    #[arg(long)]
    pub max_replans: Option<u32>,
}

impl GoalBudgetArgs {
    fn resolve(&self) -> GoalBudget {
        let defaults = GoalBudget::default();
        GoalBudget {
            max_runs: self.max_runs.unwrap_or(defaults.max_runs),
            max_model_requests: self
                .max_model_requests
                .unwrap_or(defaults.max_model_requests),
            max_tool_calls: self.max_tool_calls.unwrap_or(defaults.max_tool_calls),
            max_active_seconds: self
                .max_active_seconds
                .unwrap_or(defaults.max_active_seconds),
            max_tokens: self.max_tokens.unwrap_or(defaults.max_tokens),
            max_planning_attempts: self
                .max_planning_attempts
                .unwrap_or(defaults.max_planning_attempts),
            max_verification_attempts: self
                .max_verification_attempts
                .unwrap_or(defaults.max_verification_attempts),
            max_replans: self.max_replans.unwrap_or(defaults.max_replans),
        }
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum GoalAction {
    /// Save a new Goal and its dedicated session.
    Create {
        objective: String,
        /// Repeat to seed immutable acceptance criteria.
        #[arg(long = "accept")]
        acceptance: Vec<String>,
        #[command(flatten)]
        budget: GoalBudgetArgs,
    },
    /// Generate a structured plan and save it for explicit confirmation.
    Plan {
        goal_id: String,
        /// Use a caller-reviewed GoalPlan JSON file instead of a model call.
        #[arg(long)]
        plan_file: Option<PathBuf>,
    },
    /// Confirm the current plan and make its first dependency-ready step runnable.
    Confirm { goal_id: String },
    /// Attach or clear a structured host verifier command on one Goal step (requires confirm).
    SetVerifier {
        goal_id: String,
        step_id: String,
        /// Optional workspace-relative working directory for the verifier command.
        #[arg(long)]
        cwd: Option<String>,
        /// Timeout in seconds (1-600, default 120).
        #[arg(long)]
        timeout_seconds: Option<u64>,
        /// Bounded stdout/stderr excerpt cap in bytes (256-32768, default 8192).
        #[arg(long)]
        max_output_bytes: Option<usize>,
        /// Clear any configured step verifier instead of setting one.
        #[arg(long)]
        clear: bool,
        /// Direct argv tokens for the verifier executable (after `--`).
        #[arg(last = true)]
        argv: Vec<String>,
    },
    /// List saved Goals.
    List,
    /// Show one complete Goal record.
    Status { goal_id: String },
    /// Execute exactly one dependency-ready step.
    Continue {
        goal_id: String,
        #[arg(long)]
        approval_file: Option<PathBuf>,
        #[arg(long)]
        accept_config_change: bool,
    },
    /// Explicitly start finite automatic continuation for one Goal.
    Start {
        goal_id: String,
        #[arg(long)]
        auto: bool,
        /// Persist D-111 auto-advance: the worker keeps scheduling the next
        /// pending step after each passed step while every guard holds.
        #[arg(long)]
        chain: bool,
        #[arg(long)]
        approval_file: Option<PathBuf>,
        #[arg(long)]
        accept_config_change: bool,
    },
    /// Stop scheduling new steps after the current local Run has ended.
    Pause { goal_id: String },
    /// Return a paused or blocked Goal to its ready state.
    Resume { goal_id: String },
    /// Cancel a Goal and stop automatic scheduling.
    Cancel { goal_id: String },
    /// Verify one acceptance criterion after all steps complete.
    Verify {
        goal_id: String,
        criterion: String,
        /// Required for manual evidence; file criteria use host verification only.
        #[arg(long)]
        summary: Option<String>,
    },
    /// Resolve an unknown step outcome. Retry is never implicit.
    Resolve {
        goal_id: String,
        step_id: String,
        #[arg(long)]
        retry: bool,
    },
    /// Apply one bounded replan to a known failed step.
    Replan { goal_id: String },
    /// Render a standalone Goal report.
    Report {
        goal_id: String,
        #[arg(long, value_enum, default_value_t = GoalReportFormat::Json)]
        format: GoalReportFormat,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Resume explicitly authorized automatic Goals from durable state.
    Worker {
        /// Perform one recovery/scheduling pass and exit.
        #[arg(long)]
        once: bool,
    },
}

fn parse_goal(raw: &str) -> Result<GoalId, BoundaryError> {
    raw.parse()
}

fn parse_step(raw: &str) -> Result<StepId, BoundaryError> {
    raw.parse()
}

fn secrets(config: &Config) -> Result<Vec<String>, BoundaryError> {
    config
        .secret_env_names()
        .iter()
        .map(|name| config.secret(name).map(str::to_owned))
        .collect()
}

fn store(config: &Config) -> Result<GoalStore, BoundaryError> {
    soyo_app::goal::open_goal_store(config)
}

fn print_value(config: &Config, value: &Value, text: impl FnOnce()) {
    match config.output {
        Output::Text => text(),
        Output::Json => println!("{value}"),
        Output::Jsonl => println!("{value}"),
    }
}

fn goal_text(goal: &GoalRecord) {
    println!("Goal {}", goal.id.0);
    println!("Status: {:?} · Phase: {:?}", goal.status, goal.phase);
    println!("{}", goal.objective);
    if let Some(reason) = &goal.wait_reason {
        println!("Waiting: {reason:?}");
    }
    println!(
        "Budget: {}/{} runs · {}/{} requests · {}/{} tools",
        goal.usage.runs,
        goal.budget.max_runs,
        goal.usage.model_requests,
        goal.budget.max_model_requests,
        goal.usage.tool_calls,
        goal.budget.max_tool_calls
    );
    if goal.auto_chain {
        let completed = goal
            .steps
            .iter()
            .filter(|step| step.status == soyo_core::goal::StepStatus::Completed)
            .count();
        let next = goal.next_pending_step().map(|step| step.ordinal);
        match next {
            Some(ordinal) => println!(
                "Chain: on · {}/{} steps done · next step {ordinal}",
                completed,
                goal.steps.len()
            ),
            None => println!("Chain: on · {}/{} steps done", completed, goal.steps.len()),
        }
    }
    for criterion in &goal.acceptance {
        println!(
            "[{}] {}: {}",
            if goal.criterion_verified(&criterion.id) {
                "x"
            } else {
                " "
            },
            criterion.id,
            criterion.text
        );
    }
    let verifiers_confirmed = goal.verifiers_confirmed();
    for step in &goal.steps {
        println!(
            "{}. [{}] {:?} · {} · {} attempt(s) · {}",
            step.ordinal,
            step.id.0,
            step.status,
            step.title,
            step.attempts,
            step.run_id
                .map(|run| run.0.to_string())
                .unwrap_or_else(|| "no Run".into())
        );
        if let Some(verifier) = &step.verifier {
            println!(
                "   Verifier ({}): {:?} (timeout {}s{})",
                if verifiers_confirmed {
                    "confirmed"
                } else {
                    "unconfirmed - run `goal confirm`"
                },
                verifier.argv,
                verifier.timeout_seconds,
                verifier
                    .cwd
                    .as_deref()
                    .map(|cwd| format!(", cwd {cwd}"))
                    .unwrap_or_default()
            );
        }
        if let Some(verification) = &step.last_verification {
            println!(
                "   Last host verification: {:?} · exit {} · {}ms",
                verification.status,
                verification
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "—".into()),
                verification.duration_ms
            );
            if let Some(message) = &verification.error_message {
                println!("   Host verification note: {message}");
            }
            if !verification.stdout_excerpt.trim().is_empty() {
                println!(
                    "   Host verification stdout: {}",
                    verification.stdout_excerpt.trim()
                );
            }
            if !verification.stderr_excerpt.trim().is_empty() {
                println!(
                    "   Host verification stderr: {}",
                    verification.stderr_excerpt.trim()
                );
            }
        }
    }
}

fn bounded_file(path: &Path) -> Result<String, BoundaryError> {
    soyo_fs::read_bounded_utf8_file(path, 1024 * 1024, "invalid_goal_plan", "Goal plan file")
}

async fn create_goal(
    objective: &str,
    acceptance: &[String],
    budget: &GoalBudgetArgs,
    config: &Config,
) -> Result<Value, BoundaryError> {
    let goal =
        soyo_app::goal::create_goal(config, objective, acceptance.to_vec(), budget.resolve())
            .await?;
    Ok(json!({"schema_version":1,"data":{"goal":goal}}))
}

async fn plan(
    goal_id: GoalId,
    plan_file: Option<&PathBuf>,
    config: &Config,
) -> Result<Value, BoundaryError> {
    let supplied = plan_file
        .map(|path| GoalPlan::parse(&bounded_file(path)?))
        .transpose()?;
    let cancel = Arc::new(Cancel::new());
    let _signals = start_signals(cancel.clone())?;
    let goal =
        soyo_app::goal::run_goal_planning(config, goal_id, supplied, cancel.as_ref()).await?;
    Ok(json!({"schema_version":1,"data":{"goal":goal,"confirmation_required":true}}))
}

async fn confirm(goal_id: GoalId, config: &Config) -> Result<Value, BoundaryError> {
    let goal = soyo_app::goal::confirm_goal_plan(config, goal_id).await?;
    Ok(json!({"schema_version":1,"data":{"goal":goal}}))
}

#[allow(clippy::too_many_arguments)]
async fn set_verifier(
    goal_id: GoalId,
    step_id: StepId,
    cwd: Option<String>,
    timeout_seconds: Option<u64>,
    max_output_bytes: Option<usize>,
    clear: bool,
    argv: Vec<String>,
    config: &Config,
) -> Result<Value, BoundaryError> {
    let verifier = if clear {
        if !argv.is_empty()
            || cwd.is_some()
            || timeout_seconds.is_some()
            || max_output_bytes.is_some()
        {
            return Err(failure(
                "invalid_goal",
                "--clear cannot be combined with verifier command arguments or options",
            ));
        }
        None
    } else {
        if argv.is_empty() {
            return Err(failure(
                "invalid_goal",
                "Provide verifier argv after `--` or pass --clear",
            ));
        }
        let defaults = soyo_core::goal::DEFAULT_VERIFIER_TIMEOUT_SECONDS;
        let verifier = StepVerifier {
            argv,
            cwd,
            timeout_seconds: timeout_seconds.unwrap_or(defaults),
            max_output_bytes: max_output_bytes
                .unwrap_or(soyo_core::goal::DEFAULT_VERIFIER_OUTPUT_BYTES),
        };
        verifier.validate()?;
        Some(verifier)
    };
    let goal = soyo_app::goal::assign_step_verifier(config, goal_id, step_id, verifier).await?;
    Ok(json!({"schema_version":1,"data":{"goal":goal,"confirmation_required":true}}))
}

fn terminal(config: &Config) -> Result<Option<Arc<Terminal>>, BoundaryError> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Terminal::open(secrets(config)?)
            .map(Some)
            .map(|terminal| terminal.map(Arc::new))
            .map_err(|_| failure("terminal_error", "Goal terminal is unavailable"))
    } else {
        Ok(None)
    }
}

fn run_context(
    cli: &Cli,
    config: &Config,
    approval_file: Option<&PathBuf>,
    cancellation: Arc<Cancel>,
    interactive: bool,
    accept_config_change: bool,
) -> Result<GoalRunContext, BoundaryError> {
    let terminal = if interactive { terminal(config)? } else { None };
    let approvals = approval_file
        .map(|path| {
            Preauthorizations::from_file(path, &config.cwd)
                .map_err(|_| failure("invalid_approval_file", "Goal approval file is invalid"))
        })
        .transpose()?;
    Ok(GoalRunContext {
        permissions: config.permissions_mode,
        confirmation: terminal
            .as_ref()
            .map(|terminal| terminal.clone() as Arc<dyn soyo_core::runtime::ApprovalHandler>),
        input: terminal.map(|terminal| terminal as Arc<dyn soyo_core::input::InputHandler>),
        approvals,
        personal_context: personal_context(cli, config)
            .map_err(|_| failure("personal_context", "Goal personal context is invalid"))?,
        anthropic_prompt_cache: cli.anthropic_prompt_cache,
        accept_config_change,
        cancellation,
    })
}

async fn continue_goal(
    goal_id: GoalId,
    cli: &Cli,
    config: &Config,
    approval_file: Option<&PathBuf>,
    accept_config_change: bool,
) -> Result<Value, BoundaryError> {
    let store = store(config)?;
    if cli.mode.is_some() {
        let mut goal = store.load(goal_id).await?;
        if goal.mode != config.mode {
            goal.mode = config.mode;
            for step in &mut goal.steps {
                if step.status == soyo_core::goal::StepStatus::Pending {
                    step.mode = config.mode;
                }
            }
            let revision = goal.revision;
            store.save(goal, revision).await?;
        }
    }
    let cancel = Arc::new(Cancel::new());
    let _signals = start_signals(cancel.clone())?;
    let context = run_context(
        cli,
        config,
        approval_file,
        cancel,
        true,
        accept_config_change,
    )?;
    let outcome = match continue_once(config, &store, goal_id, context).await {
        Ok(outcome) => outcome,
        Err(error) if error.0.code == "goal_budget" => {
            let goal = store.load(goal_id).await?;
            return Ok(json!({"schema_version":1,"data":{"goal":goal}}));
        }
        Err(error) => return Err(error),
    };
    Ok(json!({
        "schema_version":1,
        "data":{"goal":outcome.goal,"run":outcome.result}
    }))
}

async fn auto_loop(
    mut goal: GoalRecord,
    cli: &Cli,
    config: &Config,
    approval_file: Option<&PathBuf>,
    interactive: bool,
    accept_config_change: bool,
    cancel: Arc<Cancel>,
) -> Result<(GoalRecord, Vec<Value>), BoundaryError> {
    let store = store(config)?;
    let mut runs = Vec::new();
    let mut first_run_approval = approval_file.cloned();
    loop {
        {
            // The fingerprint reads module overrides through SQLite; keep the
            // bounded storage work off this current_thread executor.
            let owned = config.clone();
            let record = goal.clone();
            soyo_app::offload_boundary(move || configured_auto_goal(&owned, &record)).await?;
        }
        if goal.status != GoalStatus::Ready {
            break;
        }
        if goal.budget_exhausted() {
            goal.status = GoalStatus::LimitReached;
            goal.phase = GoalPhase::Finished;
            goal.updated_at = soyo_app::events::timestamp()?;
            let revision = goal.revision;
            goal = store.save(goal, revision).await?;
            break;
        }
        let context = run_context(
            cli,
            config,
            first_run_approval.as_ref(),
            cancel.clone(),
            interactive,
            accept_config_change,
        )?;
        first_run_approval = None;
        let outcome = match continue_once(config, &store, goal.id, context).await {
            Ok(outcome) => outcome,
            Err(error) if error.0.code == "goal_budget" => {
                goal = store.load(goal.id).await?;
                break;
            }
            Err(error) => return Err(error),
        };
        runs.push(serde_json::to_value(&outcome.result).unwrap_or(Value::Null));
        goal = outcome.goal;
        if cancel.is_cancelled() {
            break;
        }
        if goal.status == GoalStatus::Blocked
            && goal.wait_reason == Some(GoalWaitReason::ExternalInput)
        {
            goal = store
                .replan_failed_step(goal.id, soyo_app::events::timestamp()?)
                .await?;
        }
        if !matches!(goal.status, GoalStatus::Ready) {
            break;
        }
    }
    Ok((goal, runs))
}

async fn start(
    goal_id: GoalId,
    auto: bool,
    chain: bool,
    cli: &Cli,
    config: &Config,
    approval_file: Option<&PathBuf>,
    accept_config_change: bool,
) -> Result<Value, BoundaryError> {
    if !auto {
        return Err(failure(
            "invalid_goal_start",
            "Use --auto to explicitly authorize finite automatic continuation",
        ));
    }
    let chain = chain || config.goal.auto_chain;
    let store = store(config)?;
    let mut goal = store.load(goal_id).await?;
    goal.mode = config.mode;
    for step in &mut goal.steps {
        if step.status == soyo_core::goal::StepStatus::Pending {
            step.mode = config.mode;
        }
    }
    soyo_app::goal::authorize_auto_goal(config, &mut goal, chain).await?;
    let revision = goal.revision;
    goal = store.save(goal, revision).await?;
    let cancel = Arc::new(Cancel::new());
    let _signals = start_signals(cancel.clone())?;
    let (goal, runs) = auto_loop(
        goal,
        cli,
        config,
        approval_file,
        true,
        accept_config_change,
        cancel,
    )
    .await?;
    Ok(json!({"schema_version":1,"data":{"goal":goal,"runs":runs}}))
}

async fn worker_once(
    cli: &Cli,
    config: &Config,
    cancel: Arc<Cancel>,
) -> Result<Value, BoundaryError> {
    let store = store(config)?;
    let recovered = store
        .recover_unknown(soyo_app::events::timestamp()?, now_ms())
        .await?;
    let goals = store.list().await?;
    let mut continued = Vec::new();
    for goal in goals {
        // D-111: only an explicitly chained Goal auto-resumes. A plain
        // `start --auto` burst is finite and never restarted by the worker.
        if goal.status != GoalStatus::Ready
            || goal.authorization.is_none()
            || !(goal.auto_chain || config.goal.auto_chain)
        {
            continue;
        }
        let configured = {
            let owned = config.clone();
            let record = goal.clone();
            soyo_app::offload_boundary(move || configured_auto_goal(&owned, &record))
                .await
                .is_ok()
        };
        if config.permissions_mode == PermissionMode::Manual || !configured {
            let blocked = store
                .transition(
                    goal.id,
                    GoalStatus::Blocked,
                    GoalPhase::Executing,
                    Some(soyo_core::goal::GoalWaitReason::Authorization),
                    soyo_app::events::timestamp()?,
                )
                .await?;
            continued.push(json!({"goal":blocked,"runs":[]}));
            continue;
        }
        let (goal, runs) = auto_loop(goal, cli, config, None, false, false, cancel.clone()).await?;
        continued.push(json!({"goal":goal,"runs":runs}));
    }
    Ok(json!({
        "schema_version":1,
        "data":{"recovered_unknown":recovered,"continued":continued}
    }))
}

async fn report(
    goal_id: GoalId,
    format: GoalReportFormat,
    file: Option<&PathBuf>,
    config: &Config,
) -> Result<Value, BoundaryError> {
    let goal = store(config)?.load(goal_id).await?;
    let bytes = soyo_app::goal::report_goal(
        config,
        goal_id,
        matches!(format, GoalReportFormat::Markdown),
    )
    .await?;
    if let Some(target) = file {
        let cancel = Arc::new(Cancel::new());
        let target = if target.is_absolute() {
            target.clone()
        } else {
            config.cwd.join(target)
        };
        let receipt = soyo_app::export::publish_session(
            &target,
            &bytes,
            &config.cwd,
            &goal.workspace,
            config.mode,
            cancel.as_ref(),
        )?;
        Ok(json!({"schema_version":1,"data":{"goal_id":goal.id,"receipt":receipt}}))
    } else {
        if io::stdout().lock().write_all(&bytes).is_err() {
            return Err(failure("goal_report", "Goal report cannot be written"));
        }
        Ok(Value::Null)
    }
}

async fn control(
    action: GoalAction,
    config: &Config,
    transition: GoalStatus,
    phase: GoalPhase,
    reason: Option<soyo_core::goal::GoalWaitReason>,
) -> Result<Value, BoundaryError> {
    let goal_id = match action {
        GoalAction::Pause { goal_id }
        | GoalAction::Resume { goal_id }
        | GoalAction::Cancel { goal_id } => parse_goal(&goal_id)?,
        _ => {
            return Err(failure(
                "invalid_goal_action",
                "Goal control action is invalid",
            ));
        }
    };
    let goal = soyo_app::goal::transition_goal(config, goal_id, transition, phase, reason).await?;
    Ok(json!({"schema_version":1,"data":{"goal":goal}}))
}

pub async fn run(action: &GoalAction, cli: &Cli, config: &Config) -> Result<u8, BoundaryError> {
    let value = match action {
        GoalAction::Create {
            objective,
            acceptance,
            budget,
        } => create_goal(objective, acceptance, budget, config).await?,
        GoalAction::Plan { goal_id, plan_file } => {
            plan(parse_goal(goal_id)?, plan_file.as_ref(), config).await?
        }
        GoalAction::Confirm { goal_id } => confirm(parse_goal(goal_id)?, config).await?,
        GoalAction::SetVerifier {
            goal_id,
            step_id,
            cwd,
            timeout_seconds,
            max_output_bytes,
            clear,
            argv,
        } => {
            set_verifier(
                parse_goal(goal_id)?,
                parse_step(step_id)?,
                cwd.clone(),
                *timeout_seconds,
                *max_output_bytes,
                *clear,
                argv.clone(),
                config,
            )
            .await?
        }
        GoalAction::List => {
            let goals = soyo_app::goal::list_goals(config).await?;
            let value = json!({"schema_version":1,"data":{"goals":goals.clone()}});
            print_value(config, &value, || {
                for goal in goals {
                    println!(
                        "{} {:?} {}",
                        goal.id.0,
                        goal.status,
                        goal.objective.chars().take(80).collect::<String>()
                    );
                }
            });
            return Ok(0);
        }
        GoalAction::Status { goal_id } => {
            let goal = soyo_app::goal::load_goal(config, parse_goal(goal_id)?).await?;
            let value = json!({"schema_version":1,"data":{"goal":goal.clone()}});
            print_value(config, &value, || goal_text(&goal));
            return Ok(0);
        }
        GoalAction::Continue {
            goal_id,
            approval_file,
            accept_config_change,
        } => {
            continue_goal(
                parse_goal(goal_id)?,
                cli,
                config,
                approval_file.as_ref(),
                *accept_config_change,
            )
            .await?
        }
        GoalAction::Start {
            goal_id,
            auto,
            chain,
            approval_file,
            accept_config_change,
        } => {
            start(
                parse_goal(goal_id)?,
                *auto,
                *chain,
                cli,
                config,
                approval_file.as_ref(),
                *accept_config_change,
            )
            .await?
        }
        GoalAction::Pause { .. } => {
            control(
                action.clone(),
                config,
                GoalStatus::Paused,
                GoalPhase::Executing,
                None,
            )
            .await?
        }
        GoalAction::Resume { .. } => {
            control(
                action.clone(),
                config,
                GoalStatus::Ready,
                GoalPhase::Executing,
                None,
            )
            .await?
        }
        GoalAction::Cancel { .. } => {
            control(
                action.clone(),
                config,
                GoalStatus::Cancelled,
                GoalPhase::Finished,
                None,
            )
            .await?
        }
        GoalAction::Verify {
            goal_id,
            criterion,
            summary,
        } => {
            let cancel = Arc::new(Cancel::new());
            let _signals = start_signals(cancel.clone())?;
            let goal = soyo_app::goal::verify_goal_criterion(
                config,
                parse_goal(goal_id)?,
                criterion,
                summary.clone(),
                cancel.as_ref(),
            )
            .await?;
            json!({"schema_version":1,"data":{"goal":goal}})
        }
        GoalAction::Resolve {
            goal_id,
            step_id,
            retry,
        } => {
            let goal = soyo_app::goal::resolve_goal_step(
                config,
                parse_goal(goal_id)?,
                parse_step(step_id)?,
                *retry,
            )
            .await?;
            json!({"schema_version":1,"data":{"goal":goal}})
        }
        GoalAction::Replan { goal_id } => {
            let goal = soyo_app::goal::replan_goal(config, parse_goal(goal_id)?).await?;
            json!({"schema_version":1,"data":{"goal":goal}})
        }
        GoalAction::Report {
            goal_id,
            format,
            file,
        } => {
            let value = report(parse_goal(goal_id)?, *format, file.as_ref(), config).await?;
            if value.is_null() {
                return Ok(0);
            }
            value
        }
        GoalAction::Worker { once } => {
            let cancel = Arc::new(Cancel::new());
            let _signals = start_signals(cancel.clone())?;
            if *once {
                worker_once(cli, config, cancel).await?
            } else {
                let mut latest = json!({"schema_version":1,"data":{"goals":[]}});
                loop {
                    latest = worker_once(cli, config, cancel.clone()).await?;
                    if cancel.is_cancelled() {
                        break latest;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    };
    let value = value;
    print_value(config, &value, || {
        if let Some(goal) = value
            .get("data")
            .and_then(|data| data.get("goal"))
            .and_then(|goal| serde_json::from_value::<GoalRecord>(goal.clone()).ok())
        {
            goal_text(&goal);
        }
        if let Some(text) = value
            .get("data")
            .and_then(|data| data.get("run"))
            .and_then(|run| run.get("text"))
            .and_then(Value::as_str)
        {
            println!("\n{}", safe_output(text, config));
        }
    });
    let run_failed = value
        .get("data")
        .and_then(|data| data.get("run"))
        .and_then(|run| run.get("status"))
        .and_then(Value::as_str)
        .is_some_and(|status| status != "completed");
    let goal_status = value
        .get("data")
        .and_then(|data| data.get("goal"))
        .and_then(|goal| goal.get("status"))
        .and_then(Value::as_str);
    Ok(match goal_status {
        Some("cancelled") if !matches!(action, GoalAction::Cancel { .. }) => 130,
        Some("cancelled") => 0,
        Some("limit_reached") => 5,
        Some("failed" | "blocked") => 1,
        _ => u8::from(run_failed),
    })
}
