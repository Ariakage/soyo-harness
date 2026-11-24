//! Goal lifecycle RPC surface: thin adapters over the shared
//! `soyo_app::goal` workflows plus daemon-owned continuation jobs.
//!
//! Semantics mirror the CLI and TUI exactly: planning/verification are
//! single awaited model calls, `continue` executes one step, `start` stamps
//! explicit authorization and runs the finite auto loop as a background job
//! keyed by goal id. Goal jobs are daemon-scoped — they outlive the client
//! connection that submitted them (durable records keep lease and dispatch
//! evidence) and are stopped only via `goal_pause`/`goal_cancel`.

use super::peripheral::{require_config_clone, require_idle_config_clone};
use super::service::HostService;
use soyo_app::goal::{self, GoalRunContext};
use soyo_app::interaction::RunCancellation;
use soyo_core::{
    boundary::Cancellation,
    config::Config,
    contract::PermissionMode,
    goal::{
        GoalBudget, GoalId, GoalPhase, GoalPlan, GoalRecord, GoalStatus, GoalStore, GoalWaitReason,
        StepId,
    },
    runtime::PersonalContext,
};
use soyo_ipc::redaction;
use soyo_ipc::types::{
    DesktopError, GoalAuthorizationDto, GoalBudgetDto, GoalCriterionDto, GoalDto, GoalJobDto,
    GoalReportDto, GoalStepDto, GoalStepVerificationDto, GoalUsageDto, GoalVerifierDto,
};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::task::JoinHandle;

pub struct GoalJob {
    pub action: &'static str,
    pub cancel: Arc<RunCancellation>,
    pub state: Arc<StdMutex<GoalJobState>>,
    pub join: JoinHandle<()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalJobState {
    Running,
    Succeeded,
    Failed(String),
}

fn failure(code: &str, message: impl Into<String>) -> DesktopError {
    DesktopError::new(code, message.into())
}

fn parse_goal_id(raw: &str) -> Result<GoalId, DesktopError> {
    raw.parse::<GoalId>()
        .map_err(|_| failure("invalid_args", "Goal id is invalid"))
}

fn parse_step_id(raw: &str) -> Result<StepId, DesktopError> {
    raw.parse::<StepId>()
        .map_err(|_| failure("invalid_args", "Goal step id is invalid"))
}

fn goal_store(config: &Config) -> Result<GoalStore, DesktopError> {
    goal::open_goal_store(config).map_err(|err| failure(err.code(), err.message().to_owned()))
}

fn verification_kind(spec: &soyo_core::goal::VerificationSpec) -> &'static str {
    match spec {
        soyo_core::goal::VerificationSpec::Manual => "manual",
        soyo_core::goal::VerificationSpec::FileExists { .. } => "file_exists",
        soyo_core::goal::VerificationSpec::FileSha256 { .. } => "file_sha256",
    }
}

fn wait_reason_text(reason: &GoalWaitReason) -> String {
    match reason {
        GoalWaitReason::PlanConfirmation => "plan_confirmation".to_owned(),
        GoalWaitReason::Verification => "verification".to_owned(),
        GoalWaitReason::UnknownSideEffect { step_id } => {
            format!("unknown_side_effect:{}", step_id.0)
        }
        GoalWaitReason::ExternalInput => "external_input".to_owned(),
        GoalWaitReason::Authorization => "authorization".to_owned(),
    }
}

fn status_text(status: GoalStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{status:?}").to_lowercase())
}

fn phase_text(phase: GoalPhase) -> String {
    serde_json::to_value(phase)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{phase:?}").to_lowercase())
}

fn step_status_text(status: soyo_core::goal::StepStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{status:?}").to_lowercase())
}

fn map_verifier(verifier: &soyo_core::goal::StepVerifier) -> GoalVerifierDto {
    GoalVerifierDto {
        argv: verifier.argv.clone(),
        cwd: verifier.cwd.clone(),
        timeout_seconds: verifier.timeout_seconds,
        max_output_bytes: verifier.max_output_bytes,
    }
}

fn map_goal(goal: &GoalRecord, running_action: Option<String>, secrets: &[String]) -> GoalDto {
    GoalDto {
        goal_id: goal.id.0.to_string(),
        session_id: goal.session_id.0.to_string(),
        objective: goal.objective.clone(),
        workspace: goal.workspace.display().to_string(),
        profile: goal.profile.clone(),
        mode: format!("{:?}", goal.mode).to_lowercase(),
        permissions: format!("{:?}", goal.permissions).to_lowercase(),
        status: status_text(goal.status),
        phase: phase_text(goal.phase),
        wait_reason: goal.wait_reason.as_ref().map(wait_reason_text),
        current_step: goal.current_step.map(|step| step.0.to_string()),
        acceptance: goal
            .acceptance
            .iter()
            .map(|criterion| GoalCriterionDto {
                id: criterion.id.clone(),
                text: criterion.text.clone(),
                verification: verification_kind(&criterion.verification).to_owned(),
                verified: goal.criterion_verified(&criterion.id),
            })
            .collect(),
        steps: goal
            .steps
            .iter()
            .map(|step| GoalStepDto {
                step_id: step.id.0.to_string(),
                ordinal: step.ordinal,
                title: step.title.clone(),
                instruction: step.instruction.clone(),
                mode: format!("{:?}", step.mode).to_lowercase(),
                status: step_status_text(step.status),
                depends_on: step.depends_on.iter().map(|id| id.0.to_string()).collect(),
                attempts: step.attempts,
                run_id: step.run_id.map(|run| run.0.to_string()),
                verifier: step.verifier.as_ref().map(map_verifier),
                last_verification: step.last_verification.as_ref().map(|verification| {
                    GoalStepVerificationDto {
                        status: format!("{:?}", verification.status).to_lowercase(),
                        attempt: verification.attempt,
                        exit_code: verification.exit_code,
                        duration_ms: verification.duration_ms,
                        error: verification
                            .error_message
                            .as_ref()
                            .map(|message| redaction::redact_str(message, secrets)),
                        stdout_excerpt: redaction::redact_str(
                            &verification.stdout_excerpt,
                            secrets,
                        ),
                        stderr_excerpt: redaction::redact_str(
                            &verification.stderr_excerpt,
                            secrets,
                        ),
                    }
                }),
                last_status: step
                    .last_result
                    .as_ref()
                    .map(|result| format!("{:?}", result.status).to_lowercase()),
                last_error: step.last_result.as_ref().and_then(|result| {
                    result.error_code.as_ref().map(|code| {
                        let detail = result.stop_reason.as_deref().unwrap_or_default();
                        redaction::redact_str(&format!("{code}{detail}"), secrets)
                    })
                }),
            })
            .collect(),
        budget: GoalBudgetDto {
            max_runs: goal.budget.max_runs,
            max_model_requests: goal.budget.max_model_requests,
            max_tool_calls: goal.budget.max_tool_calls,
            max_active_seconds: goal.budget.max_active_seconds,
            max_tokens: goal.budget.max_tokens,
            max_planning_attempts: goal.budget.max_planning_attempts,
            max_verification_attempts: goal.budget.max_verification_attempts,
            max_replans: goal.budget.max_replans,
        },
        usage: GoalUsageDto {
            runs: goal.usage.runs,
            model_requests: goal.usage.model_requests,
            tool_calls: goal.usage.tool_calls,
            active_seconds: goal.usage.active_seconds,
            input_tokens: goal.usage.input_tokens,
            output_tokens: goal.usage.output_tokens,
            token_usage_unknown: goal.usage.token_usage_unknown,
            planning_attempts: goal.usage.planning_attempts,
            verification_attempts: goal.usage.verification_attempts,
            replans: goal.usage.replans,
        },
        authorization: goal
            .authorization
            .as_ref()
            .map(|auth| GoalAuthorizationDto {
                auto: auth.auto,
                mode: format!("{:?}", auth.mode).to_lowercase(),
                permissions: format!("{:?}", auth.permissions).to_lowercase(),
                authorized_at: auth.authorized_at.clone(),
            }),
        verifiers_confirmed: goal.verifiers_confirmed(),
        failure: goal.failure.as_ref().map(|error| {
            redaction::redact_str(&format!("{}: {}", error.code, error.message), secrets)
        }),
        running_action,
        created_at: goal.created_at.clone(),
        updated_at: goal.updated_at.clone(),
    }
}

fn goal_context(config: &Config, cancel: Arc<RunCancellation>) -> GoalRunContext {
    GoalRunContext {
        permissions: config.permissions_mode,
        confirmation: None,
        input: None,
        approvals: None,
        personal_context: PersonalContext::default(),
        anthropic_prompt_cache: false,
        accept_config_change: false,
        cancellation: cancel,
    }
}

impl HostService {
    fn running_goal_action(&self, goal_id: &str) -> Option<String> {
        self.goal_jobs.lock().ok().and_then(|jobs| {
            jobs.get(goal_id).and_then(|job| {
                job.state.lock().ok().and_then(|state| match &*state {
                    GoalJobState::Running => Some(job.action.to_owned()),
                    _ => None,
                })
            })
        })
    }

    fn map_with_job(&self, goal: &GoalRecord, secrets: &[String]) -> GoalDto {
        map_goal(
            goal,
            self.running_goal_action(&goal.id.0.to_string()),
            secrets,
        )
    }

    /// Spawn one daemon-owned Goal job. A goal admits a single running job at
    /// a time; `goal_busy` is returned when another action is in flight.
    fn spawn_goal_job(
        &self,
        goal_key: String,
        action: &'static str,
        task: impl std::future::Future<Output = Result<GoalRecord, DesktopError>> + Send + 'static,
    ) -> Result<(), DesktopError> {
        let mut jobs = self
            .goal_jobs
            .lock()
            .map_err(|_| failure("goal_storage", "Goal job registry is unavailable"))?;
        if jobs
            .get(&goal_key)
            .and_then(|job| {
                job.state
                    .lock()
                    .ok()
                    .map(|state| *state == GoalJobState::Running)
            })
            .unwrap_or(false)
        {
            return Err(failure(
                "goal_busy",
                "Another Goal action is already running for this Goal",
            ));
        }
        jobs.retain(|_, job| {
            job.state
                .lock()
                .map(|state| *state == GoalJobState::Running)
                .unwrap_or(false)
        });
        let cancel = Arc::new(RunCancellation::new());
        let state = Arc::new(StdMutex::new(GoalJobState::Running));
        let job_state = state.clone();
        let join = tokio::spawn(async move {
            let outcome = task.await;
            if let Ok(mut guard) = job_state.lock() {
                *guard = match outcome {
                    Ok(_) => GoalJobState::Succeeded,
                    Err(error) => {
                        GoalJobState::Failed(format!("{}: {}", error.code, error.message))
                    }
                };
            }
        });
        jobs.insert(
            goal_key,
            GoalJob {
                action,
                cancel,
                state,
                join,
            },
        );
        Ok(())
    }

    fn cancel_goal_job(&self, goal_key: &str) {
        if let Ok(jobs) = self.goal_jobs.lock()
            && let Some(job) = jobs.get(goal_key)
        {
            job.cancel.cancel();
        }
    }

    fn goal_state_of(&self, job_state: &StdMutex<GoalJobState>) -> (String, Option<String>) {
        match job_state.lock().map(|state| state.clone()) {
            Ok(GoalJobState::Running) => ("running".into(), None),
            Ok(GoalJobState::Succeeded) => ("succeeded".into(), None),
            Ok(GoalJobState::Failed(error)) => ("failed".into(), Some(error)),
            Err(_) => ("unknown".into(), None),
        }
    }

    pub async fn goal_jobs(&self) -> Result<Vec<GoalJobDto>, DesktopError> {
        let jobs = self
            .goal_jobs
            .lock()
            .map_err(|_| failure("goal_storage", "Goal job registry is unavailable"))?;
        Ok(jobs
            .iter()
            .map(|(goal_id, job)| {
                let (state, error) = self.goal_state_of(&job.state);
                GoalJobDto {
                    goal_id: goal_id.clone(),
                    action: job.action.to_owned(),
                    state,
                    error: error.map(|text| redaction::redact_str(&text, &self.current_secrets())),
                }
            })
            .collect())
    }

    pub async fn goal_list(&self) -> Result<Vec<GoalDto>, DesktopError> {
        let config = require_config_clone(self).await?;
        let secrets = self.current_secrets();
        let goals = goal::list_goals(&config)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(goals
            .iter()
            .map(|goal| self.map_with_job(goal, &secrets))
            .collect())
    }

    pub async fn goal_status(&self, goal_id: &str) -> Result<GoalDto, DesktopError> {
        let config = require_config_clone(self).await?;
        let secrets = self.current_secrets();
        let goal = goal::load_goal(&config, parse_goal_id(goal_id)?)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_with_job(&goal, &secrets))
    }

    pub async fn goal_create(
        &self,
        objective: &str,
        acceptance: Vec<String>,
        budget: Option<GoalBudgetDto>,
    ) -> Result<GoalDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        let budget = budget
            .map(|dto| GoalBudget {
                max_runs: dto.max_runs,
                max_model_requests: dto.max_model_requests,
                max_tool_calls: dto.max_tool_calls,
                max_active_seconds: dto.max_active_seconds,
                max_tokens: dto.max_tokens,
                max_planning_attempts: dto.max_planning_attempts,
                max_verification_attempts: dto.max_verification_attempts,
                max_replans: dto.max_replans,
            })
            .unwrap_or_default();
        let goal = goal::create_goal(&config, objective, acceptance, budget)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_with_job(&goal, &secrets))
    }

    /// Run one planning pass; `plan` carries a caller-reviewed document and
    /// skips the model call entirely (the `--plan-file` equivalent).
    pub async fn goal_plan(
        &self,
        goal_id: &str,
        plan: Option<GoalPlan>,
    ) -> Result<GoalDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        let cancel = RunCancellation::new();
        let goal = goal::run_goal_planning(&config, parse_goal_id(goal_id)?, plan, &cancel)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_with_job(&goal, &secrets))
    }

    pub async fn goal_confirm(&self, goal_id: &str) -> Result<GoalDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        let goal = goal::confirm_goal_plan(&config, parse_goal_id(goal_id)?)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_with_job(&goal, &secrets))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn goal_set_verifier(
        &self,
        goal_id: &str,
        step_id: &str,
        argv: Vec<String>,
        cwd: Option<String>,
        timeout_seconds: Option<u64>,
        max_output_bytes: Option<u64>,
        clear: bool,
    ) -> Result<GoalDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        let verifier = if clear {
            if !argv.is_empty()
                || cwd.is_some()
                || timeout_seconds.is_some()
                || max_output_bytes.is_some()
            {
                return Err(failure(
                    "invalid_goal",
                    "clear cannot be combined with verifier arguments",
                ));
            }
            None
        } else {
            if argv.is_empty() {
                return Err(failure(
                    "invalid_goal",
                    "Provide verifier argv or request a clear",
                ));
            }
            Some(soyo_core::goal::StepVerifier {
                argv,
                cwd,
                timeout_seconds: timeout_seconds
                    .unwrap_or(soyo_core::goal::DEFAULT_VERIFIER_TIMEOUT_SECONDS),
                max_output_bytes: max_output_bytes
                    .map(|bytes| usize::try_from(bytes).unwrap_or(usize::MAX))
                    .unwrap_or(soyo_core::goal::DEFAULT_VERIFIER_OUTPUT_BYTES),
            })
        };
        let goal = goal::assign_step_verifier(
            &config,
            parse_goal_id(goal_id)?,
            parse_step_id(step_id)?,
            verifier,
        )
        .await
        .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_with_job(&goal, &secrets))
    }

    async fn goal_transition(
        &self,
        goal_id: &str,
        status: GoalStatus,
        phase: GoalPhase,
        reason: Option<GoalWaitReason>,
        stop_job: bool,
    ) -> Result<GoalDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        let key = parse_goal_id(goal_id)?;
        if stop_job {
            self.cancel_goal_job(&key.0.to_string());
        }
        let goal = goal::transition_goal(&config, key, status, phase, reason)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_with_job(&goal, &secrets))
    }

    pub async fn goal_pause(&self, goal_id: &str) -> Result<GoalDto, DesktopError> {
        self.goal_transition(
            goal_id,
            GoalStatus::Paused,
            GoalPhase::Executing,
            None,
            true,
        )
        .await
    }

    pub async fn goal_resume(&self, goal_id: &str) -> Result<GoalDto, DesktopError> {
        self.goal_transition(
            goal_id,
            GoalStatus::Ready,
            GoalPhase::Executing,
            None,
            false,
        )
        .await
    }

    pub async fn goal_cancel(&self, goal_id: &str) -> Result<GoalDto, DesktopError> {
        self.goal_transition(
            goal_id,
            GoalStatus::Cancelled,
            GoalPhase::Finished,
            None,
            true,
        )
        .await
    }

    pub async fn goal_verify(
        &self,
        goal_id: &str,
        criterion: &str,
        summary: Option<String>,
    ) -> Result<GoalDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        let cancel = RunCancellation::new();
        let goal = goal::verify_goal_criterion(
            &config,
            parse_goal_id(goal_id)?,
            criterion,
            summary,
            &cancel,
        )
        .await
        .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_with_job(&goal, &secrets))
    }

    pub async fn goal_resolve(
        &self,
        goal_id: &str,
        step_id: &str,
        retry: bool,
    ) -> Result<GoalDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        let goal = goal::resolve_goal_step(
            &config,
            parse_goal_id(goal_id)?,
            parse_step_id(step_id)?,
            retry,
        )
        .await
        .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_with_job(&goal, &secrets))
    }

    pub async fn goal_replan(&self, goal_id: &str) -> Result<GoalDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        let goal = goal::replan_goal(&config, parse_goal_id(goal_id)?)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_with_job(&goal, &secrets))
    }

    pub async fn goal_report(
        &self,
        goal_id: &str,
        markdown: bool,
    ) -> Result<GoalReportDto, DesktopError> {
        let config = require_config_clone(self).await?;
        let secrets = self.current_secrets();
        let bytes = goal::report_goal(&config, parse_goal_id(goal_id)?, markdown)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        let content = String::from_utf8(bytes)
            .map_err(|_| failure("goal_report", "Goal report is not valid UTF-8"))?;
        Ok(GoalReportDto {
            goal_id: goal_id.to_owned(),
            format: if markdown { "markdown" } else { "json" }.to_owned(),
            content,
        })
    }

    /// Execute exactly one dependency-ready step as a daemon job.
    pub async fn goal_continue(&self, goal_id: &str) -> Result<GoalDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        let key = parse_goal_id(goal_id)?;
        let goal_key = key.0.to_string();
        let cancel = Arc::new(RunCancellation::new());
        let job_config = config.clone();
        let job_cancel = cancel.clone();
        self.spawn_goal_job(goal_key, "continue", async move {
            let store = goal_store(&job_config)?;
            match goal::continue_once(
                &job_config,
                &store,
                key,
                goal_context(&job_config, job_cancel),
            )
            .await
            {
                Ok(outcome) => Ok(outcome.goal),
                Err(error) if error.is_code("goal_budget") => store
                    .load(key)
                    .await
                    .map_err(|err| failure(err.code(), err.message().to_owned())),
                Err(error) => Err(failure(error.code(), error.message().to_owned())),
            }
        })?;
        let goal = goal::load_goal(&config, key)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        Ok(self.map_with_job(&goal, &secrets))
    }

    /// Authorize and launch finite automatic continuation as a daemon job.
    /// `[goal] auto_chain` enables the D-111 persisted chain host-wide. The
    /// caller-facing goal transitions to `running` once the job takes its
    /// first step; stopping requires `goal_pause` or `goal_cancel`.
    pub async fn goal_start(&self, goal_id: &str) -> Result<GoalDto, DesktopError> {
        let config = require_idle_config_clone(self).await?;
        let secrets = self.current_secrets();
        if config.permissions_mode == PermissionMode::Manual {
            return Err(failure(
                "goal_authorization",
                "finite automatic continuation requires auto or bypass permissions",
            ));
        }
        let key = parse_goal_id(goal_id)?;
        let goal_key = key.0.to_string();
        let store = goal_store(&config)?;
        let mut goal = store
            .load(key)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        goal::authorize_auto_goal(&config, &mut goal, config.goal.auto_chain)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        let revision = goal.revision;
        goal = store
            .save(goal, revision)
            .await
            .map_err(|err| DesktopError::from_boundary(&err, &secrets))?;
        let cancel = Arc::new(RunCancellation::new());
        let job_cancel = cancel.clone();
        let job_config = config.clone();
        let job_goal = goal.clone();
        self.spawn_goal_job(goal_key, "start", async move {
            let cancelled = {
                let flag = job_cancel.clone();
                move || flag.is_cancelled()
            };
            let make_context = {
                let config = job_config.clone();
                let cancel = job_cancel.clone();
                move || goal_context(&config, cancel.clone())
            };
            match goal::auto_continue_goal(&job_config, job_goal, &make_context, &cancelled).await {
                Ok((goal, _)) => Ok(goal),
                Err(error) => Err(failure(error.code(), error.message().to_owned())),
            }
        })?;
        Ok(self.map_with_job(&goal, &secrets))
    }
}
