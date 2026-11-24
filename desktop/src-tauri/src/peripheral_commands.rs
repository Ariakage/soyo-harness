//! Tauri command wrappers for peripheral desktop operations.
//! Every call forwards to the `soyo helper` daemon via `DesktopState`.

use crate::{
    client_state::DesktopState,
    types::{
        BackgroundJobDto, BootstrapStateDto, CheckpointConfirmResultDto, CheckpointDiffDto,
        CheckpointListDto, CheckpointRecoverDto, CredentialRecordDto, DesktopError,
        DesktopPrefsDto, ExperimentalSettingsDto, ExportFormatDto, ExportResultDto, ExportScopeDto,
        ExtensionItemDto, GlobalPromptDto, GoalBudgetDto, GoalDto, GoalJobDto, GoalReportDto,
        McpServerDto, MemoryConfirmResultDto, MemoryQueryResultDto, ModuleStatusDto,
        PreparedCheckpointReviewDto, PreparedMemoryReviewDto, SandboxBackendDto,
        StagedAttachmentDto, TaskDto, TaskJobDto, UpdateDesktopPrefsParams,
    },
};
use tauri::State;

#[tauri::command]
#[specta::specta]
pub async fn set_workspace(
    state: State<'_, DesktopState>,
    path: String,
) -> Result<BootstrapStateDto, DesktopError> {
    state.set_workspace(&path).await
}

#[tauri::command]
#[specta::specta]
pub async fn select_profile(
    state: State<'_, DesktopState>,
    profile: String,
) -> Result<BootstrapStateDto, DesktopError> {
    state.select_profile(&profile).await
}

#[tauri::command]
#[specta::specta]
pub async fn set_thinking_effort(
    state: State<'_, DesktopState>,
    effort: Option<String>,
) -> Result<BootstrapStateDto, DesktopError> {
    state.set_thinking_effort(effort).await
}

#[tauri::command]
#[specta::specta]
pub async fn set_sandbox_backend(
    state: State<'_, DesktopState>,
    backend: SandboxBackendDto,
) -> Result<BootstrapStateDto, DesktopError> {
    state.set_sandbox_backend(backend).await
}

#[tauri::command]
#[specta::specta]
pub async fn set_autocompact(
    state: State<'_, DesktopState>,
    enabled: bool,
    trigger_tokens: Option<f64>,
) -> Result<BootstrapStateDto, DesktopError> {
    let parsed_tokens = match trigger_tokens {
        Some(value) => {
            if !value.is_finite() || value <= 0.0 || value > 9_007_199_254_740_991.0 {
                return Err(DesktopError::new(
                    "invalid_args",
                    "Auto-compact trigger tokens must be a valid positive integer",
                ));
            }
            Some(value.trunc() as u64)
        }
        None => None,
    };
    state.set_autocompact(enabled, parsed_tokens).await
}

#[tauri::command]
#[specta::specta]
pub async fn list_extensions(
    state: State<'_, DesktopState>,
) -> Result<Vec<ExtensionItemDto>, DesktopError> {
    state.list_extensions().await
}

#[tauri::command]
#[specta::specta]
pub async fn list_jobs(
    state: State<'_, DesktopState>,
) -> Result<Vec<BackgroundJobDto>, DesktopError> {
    state.list_jobs().await
}

#[tauri::command]
#[specta::specta]
pub async fn cancel_background_job(
    state: State<'_, DesktopState>,
    job_id: String,
) -> Result<bool, DesktopError> {
    state.cancel_background_job(&job_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn list_modules(
    state: State<'_, DesktopState>,
) -> Result<Vec<ModuleStatusDto>, DesktopError> {
    state.list_modules().await
}

#[tauri::command]
#[specta::specta]
pub async fn set_module_enabled(
    state: State<'_, DesktopState>,
    module: String,
    enabled: bool,
) -> Result<Vec<ModuleStatusDto>, DesktopError> {
    state.set_module_enabled(&module, enabled).await
}

#[tauri::command]
#[specta::specta]
pub async fn list_mcp_servers(
    state: State<'_, DesktopState>,
) -> Result<Vec<McpServerDto>, DesktopError> {
    state.list_mcp_servers().await
}

#[tauri::command]
#[specta::specta]
pub async fn toggle_mcp_server(
    state: State<'_, DesktopState>,
    name: String,
    enabled: bool,
) -> Result<Vec<McpServerDto>, DesktopError> {
    state.toggle_mcp_server(&name, enabled).await
}

#[tauri::command]
#[specta::specta]
pub async fn list_credentials(
    state: State<'_, DesktopState>,
) -> Result<Vec<CredentialRecordDto>, DesktopError> {
    state.list_credentials().await
}

#[tauri::command]
#[specta::specta]
pub async fn import_credential(
    state: State<'_, DesktopState>,
    server: String,
    env_var: String,
    scopes: Vec<String>,
    approve_scopes: bool,
) -> Result<CredentialRecordDto, DesktopError> {
    state
        .import_credential(&server, &env_var, scopes, approve_scopes)
        .await
}

#[tauri::command]
#[specta::specta]
pub async fn revoke_credential(
    state: State<'_, DesktopState>,
    reference: String,
) -> Result<bool, DesktopError> {
    state.revoke_credential(&reference).await
}

#[tauri::command]
#[specta::specta]
pub async fn query_memory(
    state: State<'_, DesktopState>,
    command: String,
) -> Result<MemoryQueryResultDto, DesktopError> {
    state.query_memory(&command).await
}

#[tauri::command]
#[specta::specta]
pub async fn prepare_memory_action(
    state: State<'_, DesktopState>,
    command: String,
) -> Result<PreparedMemoryReviewDto, DesktopError> {
    state.prepare_memory_action(&command).await
}

#[tauri::command]
#[specta::specta]
pub async fn confirm_memory_action(
    state: State<'_, DesktopState>,
    action_id: String,
) -> Result<MemoryConfirmResultDto, DesktopError> {
    state.confirm_memory_action(&action_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn checkpoint_list(
    state: State<'_, DesktopState>,
    session_id: Option<String>,
    page: f64,
) -> Result<CheckpointListDto, DesktopError> {
    if !page.is_finite() || page < 1.0 || page > 9_007_199_254_740_991.0 {
        return Err(DesktopError::new(
            "invalid_args",
            "Checkpoint page must be a valid positive integer",
        ));
    }
    state
        .checkpoint_list(session_id.as_deref(), page as usize)
        .await
}

#[tauri::command]
#[specta::specta]
pub async fn checkpoint_diff(
    state: State<'_, DesktopState>,
    session_id: Option<String>,
    checkpoint_id: String,
    sha256: String,
) -> Result<CheckpointDiffDto, DesktopError> {
    state
        .checkpoint_diff(session_id.as_deref(), &checkpoint_id, &sha256)
        .await
}

#[tauri::command]
#[specta::specta]
pub async fn checkpoint_recover(
    state: State<'_, DesktopState>,
    session_id: Option<String>,
) -> Result<CheckpointRecoverDto, DesktopError> {
    state.checkpoint_recover(session_id.as_deref()).await
}

#[tauri::command]
#[specta::specta]
pub async fn prepare_checkpoint_action(
    state: State<'_, DesktopState>,
    session_id: Option<String>,
    action: String,
    paths: Vec<String>,
    checkpoint_id: Option<String>,
    sha256: Option<String>,
    label: Option<String>,
) -> Result<PreparedCheckpointReviewDto, DesktopError> {
    state
        .prepare_checkpoint_action(
            session_id.as_deref(),
            &action,
            paths,
            checkpoint_id.as_deref(),
            sha256.as_deref(),
            label.as_deref().unwrap_or("desktop"),
        )
        .await
}

#[tauri::command]
#[specta::specta]
pub async fn confirm_checkpoint_action(
    state: State<'_, DesktopState>,
    action_id: String,
    approved_paths: Option<Vec<String>>,
) -> Result<CheckpointConfirmResultDto, DesktopError> {
    state
        .confirm_checkpoint_action(&action_id, approved_paths)
        .await
}

#[tauri::command]
#[specta::specta]
pub async fn export_session(
    state: State<'_, DesktopState>,
    session_id: Option<String>,
    target_path: String,
    format: ExportFormatDto,
    scope: ExportScopeDto,
    include_tools: bool,
) -> Result<ExportResultDto, DesktopError> {
    state
        .export_session(session_id, &target_path, format, scope, include_tools)
        .await
}

#[tauri::command]
#[specta::specta]
pub async fn stage_attachments(
    state: State<'_, DesktopState>,
    paths: Vec<String>,
) -> Result<Vec<StagedAttachmentDto>, DesktopError> {
    state.stage_attachments(paths).await
}

#[tauri::command]
#[specta::specta]
pub async fn stage_pasted_attachments(
    state: State<'_, DesktopState>,
    items: Vec<crate::client_state::PastedAttachmentItem>,
) -> Result<Vec<StagedAttachmentDto>, DesktopError> {
    state.stage_pasted_attachments(items).await
}
#[tauri::command]
#[specta::specta]
pub async fn get_user_name(state: State<'_, DesktopState>) -> Result<Option<String>, DesktopError> {
    state.get_user_name().await
}

#[tauri::command]
#[specta::specta]
pub async fn get_desktop_prefs(
    state: State<'_, DesktopState>,
) -> Result<DesktopPrefsDto, DesktopError> {
    state.get_desktop_prefs().await
}

#[tauri::command]
#[specta::specta]
pub async fn set_nickname(
    state: State<'_, DesktopState>,
    nickname: Option<String>,
) -> Result<DesktopPrefsDto, DesktopError> {
    state.set_nickname(nickname).await
}

#[tauri::command]
#[specta::specta]
pub async fn set_avatar(
    state: State<'_, DesktopState>,
    source_path: Option<String>,
) -> Result<DesktopPrefsDto, DesktopError> {
    state.set_avatar(source_path).await
}

#[tauri::command]
#[specta::specta]
pub async fn update_desktop_prefs(
    state: State<'_, DesktopState>,
    params: UpdateDesktopPrefsParams,
) -> Result<DesktopPrefsDto, DesktopError> {
    state.update_desktop_prefs(params).await
}

#[tauri::command]
#[specta::specta]
pub async fn get_global_prompt(
    state: State<'_, DesktopState>,
) -> Result<GlobalPromptDto, DesktopError> {
    state.get_global_prompt().await
}

#[tauri::command]
#[specta::specta]
pub async fn set_global_prompt(
    state: State<'_, DesktopState>,
    content: String,
) -> Result<GlobalPromptDto, DesktopError> {
    state.set_global_prompt(content).await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_list(state: State<'_, DesktopState>) -> Result<Vec<GoalDto>, DesktopError> {
    state.goal_list().await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_jobs(state: State<'_, DesktopState>) -> Result<Vec<GoalJobDto>, DesktopError> {
    state.goal_jobs().await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_status(
    state: State<'_, DesktopState>,
    goal_id: String,
) -> Result<GoalDto, DesktopError> {
    state.goal_status(&goal_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_create(
    state: State<'_, DesktopState>,
    objective: String,
    acceptance: Vec<String>,
    budget: Option<GoalBudgetDto>,
) -> Result<GoalDto, DesktopError> {
    state.goal_create(&objective, acceptance, budget).await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_plan(
    state: State<'_, DesktopState>,
    goal_id: String,
    plan_json: Option<String>,
) -> Result<GoalDto, DesktopError> {
    state.goal_plan(&goal_id, plan_json.as_deref()).await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_confirm(
    state: State<'_, DesktopState>,
    goal_id: String,
) -> Result<GoalDto, DesktopError> {
    state.goal_confirm(&goal_id).await
}

#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
pub async fn goal_set_verifier(
    state: State<'_, DesktopState>,
    goal_id: String,
    step_id: String,
    argv: Vec<String>,
    cwd: Option<String>,
    timeout_seconds: Option<f64>,
    max_output_bytes: Option<f64>,
    clear: bool,
) -> Result<GoalDto, DesktopError> {
    state
        .goal_set_verifier(
            &goal_id,
            &step_id,
            argv,
            cwd,
            timeout_seconds,
            max_output_bytes,
            clear,
        )
        .await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_pause(
    state: State<'_, DesktopState>,
    goal_id: String,
) -> Result<GoalDto, DesktopError> {
    state.goal_pause(&goal_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_resume(
    state: State<'_, DesktopState>,
    goal_id: String,
) -> Result<GoalDto, DesktopError> {
    state.goal_resume(&goal_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_cancel(
    state: State<'_, DesktopState>,
    goal_id: String,
) -> Result<GoalDto, DesktopError> {
    state.goal_cancel(&goal_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_verify(
    state: State<'_, DesktopState>,
    goal_id: String,
    criterion: String,
    summary: Option<String>,
) -> Result<GoalDto, DesktopError> {
    state.goal_verify(&goal_id, &criterion, summary).await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_resolve(
    state: State<'_, DesktopState>,
    goal_id: String,
    step_id: String,
    retry: bool,
) -> Result<GoalDto, DesktopError> {
    state.goal_resolve(&goal_id, &step_id, retry).await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_replan(
    state: State<'_, DesktopState>,
    goal_id: String,
) -> Result<GoalDto, DesktopError> {
    state.goal_replan(&goal_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_report(
    state: State<'_, DesktopState>,
    goal_id: String,
    format: String,
) -> Result<GoalReportDto, DesktopError> {
    state.goal_report(&goal_id, &format).await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_continue(
    state: State<'_, DesktopState>,
    goal_id: String,
) -> Result<GoalDto, DesktopError> {
    state.goal_continue(&goal_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn goal_start(
    state: State<'_, DesktopState>,
    goal_id: String,
) -> Result<GoalDto, DesktopError> {
    state.goal_start(&goal_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn task_list(state: State<'_, DesktopState>) -> Result<Vec<TaskDto>, DesktopError> {
    state.task_list().await
}

#[tauri::command]
#[specta::specta]
pub async fn task_jobs(state: State<'_, DesktopState>) -> Result<Vec<TaskJobDto>, DesktopError> {
    state.task_jobs().await
}

#[tauri::command]
#[specta::specta]
pub async fn task_status(
    state: State<'_, DesktopState>,
    task_id: String,
) -> Result<TaskDto, DesktopError> {
    state.task_status(&task_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn task_submit(
    state: State<'_, DesktopState>,
    input: String,
) -> Result<TaskDto, DesktopError> {
    state.task_submit(&input).await
}

#[tauri::command]
#[specta::specta]
pub async fn task_pause(
    state: State<'_, DesktopState>,
    task_id: String,
) -> Result<TaskDto, DesktopError> {
    state.task_pause(&task_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn task_resume(
    state: State<'_, DesktopState>,
    task_id: String,
) -> Result<TaskDto, DesktopError> {
    state.task_resume(&task_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn task_cancel(
    state: State<'_, DesktopState>,
    task_id: String,
) -> Result<TaskDto, DesktopError> {
    state.task_cancel(&task_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn task_resolve(
    state: State<'_, DesktopState>,
    task_id: String,
    retry: bool,
) -> Result<TaskDto, DesktopError> {
    state.task_resolve(&task_id, retry).await
}

#[tauri::command]
#[specta::specta]
pub async fn task_recover(state: State<'_, DesktopState>) -> Result<Vec<TaskDto>, DesktopError> {
    state.task_recover().await
}

#[tauri::command]
#[specta::specta]
pub async fn task_continue(
    state: State<'_, DesktopState>,
    task_id: String,
) -> Result<TaskDto, DesktopError> {
    state.task_continue(&task_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn get_experimental_settings(
    state: State<'_, DesktopState>,
) -> Result<ExperimentalSettingsDto, DesktopError> {
    state.get_experimental_settings().await
}

#[tauri::command]
#[specta::specta]
pub async fn set_js_repl_enabled(
    state: State<'_, DesktopState>,
    enabled: bool,
) -> Result<ExperimentalSettingsDto, DesktopError> {
    state.set_js_repl_enabled(enabled).await
}

#[tauri::command]
#[specta::specta]
pub async fn set_subagents_enabled(
    state: State<'_, DesktopState>,
    enabled: bool,
) -> Result<ExperimentalSettingsDto, DesktopError> {
    state.set_subagents_enabled(enabled).await
}

#[tauri::command]
#[specta::specta]
pub async fn set_routing_enabled(
    state: State<'_, DesktopState>,
    enabled: bool,
) -> Result<ExperimentalSettingsDto, DesktopError> {
    state.set_routing_enabled(enabled).await
}

#[tauri::command]
#[specta::specta]
pub async fn set_code_intel_enabled(
    state: State<'_, DesktopState>,
    enabled: bool,
) -> Result<ExperimentalSettingsDto, DesktopError> {
    state.set_code_intel_enabled(enabled).await
}
