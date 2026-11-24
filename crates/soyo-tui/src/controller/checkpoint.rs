//! Explicit per-session checkpoint administration (`F-05`), kept separate
//! from model tool permissions. Reuses the memory modal: the same line list,
//! scroll, input and operation-confirm interaction drives list/diff/capture/
//! restore/recover on the local checkpoint store.
use super::*;
use crate::{i18n::checkpoint as text, view::MemoryPanel};
use serde_json::Value;
use soyo_app::checkpoint_admin::{
    CheckpointAction, CheckpointAdmin, PreparedCheckpointAction, resolve_session_id,
};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) enum CheckpointUiResult {
    View(Value),
    Review(Box<PreparedCheckpointAction>),
    Outcome(Value),
}

enum Task {
    Query(QueryKind),
    Prepare(PrepareKind),
    Confirm(Box<PreparedCheckpointAction>),
}
enum QueryKind {
    List(usize),
    Diff { id: String, sha256: String },
    Recover,
}
enum PrepareKind {
    Create {
        paths: Vec<String>,
        label: String,
    },
    Accept {
        id: String,
        sha256: String,
        paths: Vec<String>,
        label: String,
    },
    Restore {
        id: String,
        sha256: String,
        paths: Vec<String>,
        label: String,
    },
}

fn failure(code: &str, message: impl Into<String>) -> BoundaryError {
    BoundaryError(
        soyo_core::contract::ErrorInfo {
            code: code.into(),
            message: message.into(),
        },
        None,
        None,
    )
}
fn parse_token(id: &str, sha256: &str) -> Result<soyo_core::checkpoint::CheckpointToken> {
    if id.len() != 36
        || sha256.len() != 64
        || !sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(failure(
            "invalid_args",
            "checkpoint identity must be a lowercase UUID and SHA-256 digest",
        )
        .into());
    }
    Ok(soyo_core::checkpoint::CheckpointToken {
        id: id.to_owned(),
        sha256: sha256.to_owned(),
    })
}

impl App {
    fn checkpoint_settings(&self) -> String {
        text::settings(self.checkpoint_session().is_some(), &self.state.mode)
    }
    fn checkpoint_lines(&mut self, lines: Vec<String>) {
        let lines = lines.into_iter().map(|line| self.safe(&line)).collect();
        let settings = self.safe(&self.checkpoint_settings());
        if let Some(panel) = &mut self.state.checkpoint_panel {
            panel.lines = lines;
            panel.scroll = 0;
            panel.settings = settings;
        }
    }
    /// The session checkpoints bind to: the active prepared session, else the
    /// newest stored session for this workspace (resolved on the worker).
    fn checkpoint_session(&self) -> Option<soyo_core::contract::SessionId> {
        self.prepared.as_ref().map(|prepared| prepared.session().id)
    }
    pub(super) async fn open_checkpoint(&mut self, argument: &str) -> Result<()> {
        if self.pending.is_some() {
            return Err(text::PENDING_REQUEST.into());
        }
        if self.ui_job.is_some() {
            return Err(text::UI_ACTIVE_WAIT.into());
        }
        self.checkpoint_pending = None;
        self.checkpoint_next = None;
        self.state.command_palette = None;
        self.state.completion.dismiss();
        self.state.checkpoint_panel = Some(MemoryPanel {
            settings: self.checkpoint_settings(),
            ..Default::default()
        });
        self.checkpoint_command(if argument.is_empty() {
            "list"
        } else {
            argument
        })
        .await?;
        Ok(())
    }
    fn checkpoint_worker(&mut self, task: Task) -> Result<()> {
        if self.ui_job.is_some() {
            return Err(text::UI_ACTIVE_CANCEL.into());
        }
        let cancel = Arc::new(RunCancellation::new());
        let config = self.config.clone();
        let secrets = self.secret_registry.clone();
        let session = self.checkpoint_session();
        let checking = cancel.clone();
        let flag = Arc::new(AtomicBool::new(false));
        let stopping = flag.clone();
        let runtime = tokio::runtime::Handle::current();
        let mut worker = tokio::task::spawn_blocking(move || {
            let session_id = match session {
                Some(id) => id,
                None => runtime.block_on(resolve_session_id(&config, None))?,
            };
            let result = match task {
                Task::Query(QueryKind::List(page)) => {
                    let admin = CheckpointAdmin::open(&config, secrets, session_id)?;
                    CheckpointUiResult::View(admin.list(page, &flag)?)
                }
                Task::Query(QueryKind::Diff { id, sha256 }) => {
                    let admin = CheckpointAdmin::open(&config, secrets, session_id)?;
                    let token = parse_token(&id, &sha256).map_err(|error| {
                        match error.downcast::<BoundaryError>() {
                            Ok(error) => *error,
                            Err(error) => failure("invalid_args", error.to_string()),
                        }
                    })?;
                    CheckpointUiResult::View(admin.diff_view(&token, &flag)?)
                }
                Task::Query(QueryKind::Recover) => {
                    let admin = CheckpointAdmin::open_locked(&config, secrets, session_id)?;
                    let recovered = admin.recover(&flag)?;
                    CheckpointUiResult::View(serde_json::json!({
                        "session_id": admin.session_id().0.to_string(),
                        "recovered": recovered.len(),
                        "outcomes": recovered,
                    }))
                }
                Task::Prepare(kind) => {
                    let admin = CheckpointAdmin::open_locked(&config, secrets, session_id)?;
                    let prepared = match kind {
                        PrepareKind::Create { paths, label } => admin.prepare(
                            CheckpointAction::Create { paths: &paths },
                            &label,
                            &flag,
                        )?,
                        PrepareKind::Accept {
                            id,
                            sha256,
                            paths,
                            label,
                        } => {
                            let token = parse_token(&id, &sha256).map_err(|error| {
                                match error.downcast::<BoundaryError>() {
                                    Ok(error) => *error,
                                    Err(error) => failure("invalid_args", error.to_string()),
                                }
                            })?;
                            admin.prepare(
                                CheckpointAction::Accept {
                                    checkpoint: &token,
                                    paths: &paths,
                                },
                                &label,
                                &flag,
                            )?
                        }
                        PrepareKind::Restore {
                            id,
                            sha256,
                            paths,
                            label,
                        } => {
                            let token = parse_token(&id, &sha256).map_err(|error| {
                                match error.downcast::<BoundaryError>() {
                                    Ok(error) => *error,
                                    Err(error) => failure("invalid_args", error.to_string()),
                                }
                            })?;
                            admin.prepare(
                                CheckpointAction::Restore {
                                    checkpoint: &token,
                                    paths: &paths,
                                },
                                &label,
                                &flag,
                            )?
                        }
                    };
                    CheckpointUiResult::Review(Box::new(prepared))
                }
                Task::Confirm(prepared) => {
                    let admin = CheckpointAdmin::open_locked(&config, secrets, session_id)?;
                    // The TUI confirms the exact staged operation id; the path
                    // gate still runs so a compromised review cannot write
                    // outside the session workspace or protected storage.
                    let workspace = admin.workspace_root().clone();
                    let protected = admin.protected_roots();
                    CheckpointUiResult::Outcome(admin.confirm(
                        *prepared,
                        &flag,
                        move |operation| {
                            let target = workspace.join(&operation.path);
                            let resolved = std::fs::canonicalize(&target).unwrap_or(target);
                            if !resolved.starts_with(&workspace)
                                || protected.iter().any(|root| resolved.starts_with(root))
                            {
                                return Err(failure(
                                    "path_denied",
                                    "Restore target leaves the session workspace",
                                ));
                            }
                            Ok(())
                        },
                    )?)
                }
            };
            Ok(services::UiResult::Checkpoint(result))
        });
        let join = tokio::spawn(async move {
            let result = tokio::select! {
                result=&mut worker=>result,
                _=checking.cancelled()=>{stopping.store(true,Ordering::SeqCst);worker.await},
            };
            result.map_err(|_| failure("ui_worker_failed", text::WORKER_FAILED))?
        });
        self.begin_ui(cancel, join);
        if let Some(panel) = &mut self.state.checkpoint_panel {
            panel.busy = true;
        }
        Ok(())
    }
    pub(super) async fn checkpoint_command(&mut self, command: &str) -> Result<()> {
        if command.len() > soyo_app::checkpoint_admin::MAX_VIEW_BYTES {
            return Err(text::COMMAND_TOO_LONG.into());
        }
        let command = command.trim();
        let (verb, argument) = command.split_once(' ').unwrap_or((command, ""));
        if matches!(verb, "close" | "exit") {
            if let Some(job) = &self.ui_job {
                job.cancel.cancel();
            }
            self.checkpoint_pending = None;
            self.checkpoint_next = None;
            self.state.checkpoint_panel = None;
            return Ok(());
        }
        if self.ui_job.is_some() {
            return Err(text::OPERATION_ACTIVE.into());
        }
        match verb {
            "help" => self.checkpoint_lines(
                text::HELP_LINES
                    .iter()
                    .copied()
                    .map(str::to_owned)
                    .collect(),
            ),
            "reject" => {
                self.checkpoint_pending = None;
                if let Some(panel) = &mut self.state.checkpoint_panel {
                    panel.confirmation = None;
                }
                self.checkpoint_lines(vec![text::PENDING_DISCARDED.into()]);
            }
            "review" if argument.is_empty() => {
                let data = self
                    .checkpoint_pending
                    .as_ref()
                    .ok_or(text::PREPARE_BEFORE_REVIEW)?
                    .review()
                    .clone();
                self.checkpoint_lines(json_lines(&data)?);
            }
            "confirm" => {
                if self.active.is_some() {
                    return Err(text::PUBLICATION_IDLE.into());
                }
                let prepared = self
                    .checkpoint_pending
                    .as_ref()
                    .ok_or(text::PREPARE_BEFORE_CONFIRM)?;
                if argument.trim() != prepared.operation_id() {
                    return Err(text::CONFIRMATION_MISMATCH.into());
                }
                let prepared = self.checkpoint_pending.take().ok_or(text::REVIEW_EXPIRED)?;
                if let Some(panel) = &mut self.state.checkpoint_panel {
                    panel.confirmation = None;
                }
                self.checkpoint_worker(Task::Confirm(Box::new(prepared)))?;
            }
            "create" | "accept" | "restore" => {
                if self.active.is_some() {
                    return Err(text::CHANGE_IDLE.into());
                }
                self.checkpoint_pending = None;
                if let Some(panel) = &mut self.state.checkpoint_panel {
                    panel.confirmation = None;
                }
                self.checkpoint_worker(Task::Prepare(parse_prepare(verb, argument)?))?;
            }
            "diff" => {
                self.checkpoint_worker(Task::Query(parse_diff(argument)?))?;
            }
            "recover" => {
                self.checkpoint_worker(Task::Query(QueryKind::Recover))?;
            }
            "next" => {
                let command = self.checkpoint_next.clone().ok_or(text::NO_NEXT_PAGE)?;
                let page = command.parse::<usize>().map_err(|_| text::NO_NEXT_PAGE)?;
                self.checkpoint_worker(Task::Query(QueryKind::List(page)))?;
            }
            "" | "list" => {
                let page = argument.trim().parse::<usize>().unwrap_or(1);
                self.checkpoint_worker(Task::Query(QueryKind::List(page)))?;
            }
            _ => return Err(text::UNKNOWN_COMMAND.into()),
        }
        Ok(())
    }
    pub(super) fn finish_checkpoint(
        &mut self,
        result: CheckpointUiResult,
        cancelled: bool,
    ) -> Result<()> {
        if let Some(panel) = &mut self.state.checkpoint_panel {
            panel.busy = false;
        }
        match result {
            CheckpointUiResult::View(data) if !cancelled => {
                self.checkpoint_next = data["next_page"].as_u64().map(|page| page.to_string());
                self.checkpoint_lines(json_lines(&data)?);
            }
            CheckpointUiResult::Review(prepared)
                if !cancelled && self.state.checkpoint_panel.is_some() =>
            {
                let mut lines = json_lines(prepared.review())?;
                let confirmation = format!("confirm {}", prepared.operation_id());
                lines.push(String::new());
                lines.push(confirmation.clone());
                self.checkpoint_lines(lines);
                if let Some(panel) = &mut self.state.checkpoint_panel {
                    panel.confirmation = Some(confirmation);
                }
                self.checkpoint_pending = Some(*prepared);
            }
            CheckpointUiResult::Outcome(data) => {
                self.checkpoint_next = None;
                self.checkpoint_pending = None;
                let lines = json_lines(&data)?;
                if self.state.checkpoint_panel.is_some() {
                    self.checkpoint_lines(lines);
                } else {
                    self.notice(&text::outcome(&self.safe(&data.to_string())));
                }
                if data["status"] == "unknown" {
                    self.notice(text::OUTCOME_UNKNOWN);
                } else if cancelled {
                    self.notice(text::FINISHED_DURING_CANCEL);
                }
            }
            _ => self.checkpoint_lines(vec![text::QUERY_CANCELLED.into()]),
        }
        Ok(())
    }
    pub(super) async fn checkpoint_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.kind != KeyEventKind::Press && matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if let Some(job) = &self.ui_job {
                        job.cancel.cancel();
                    } else {
                        self.checkpoint_pending = None;
                        if let Some(panel) = &mut self.state.checkpoint_panel {
                            panel.confirmation = None;
                            panel.input.clear();
                        }
                    }
                    return Ok(false);
                }
                KeyCode::Char('d') => {
                    self.checkpoint_command("close").await?;
                    return Ok(false);
                }
                KeyCode::Char('p') => return Ok(false),
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => self.checkpoint_command("close").await?,
            KeyCode::PageUp => {
                if let Some(panel) = &mut self.state.checkpoint_panel {
                    panel.scroll = panel.scroll.saturating_sub(panel.page_rows);
                }
            }
            KeyCode::PageDown => {
                if let Some(panel) = &mut self.state.checkpoint_panel {
                    panel.scroll = panel.scroll.saturating_add(panel.page_rows);
                }
            }
            KeyCode::Enter => {
                let command = self
                    .state
                    .checkpoint_panel
                    .as_ref()
                    .map(|panel| panel.input.text().to_owned())
                    .unwrap_or_default();
                let result = self.checkpoint_command(&command).await;
                if result.is_ok()
                    && let Some(panel) = &mut self.state.checkpoint_panel
                {
                    panel.input.clear();
                }
                if let Err(error) = result {
                    let message = self.safe(&error.to_string());
                    if let Some(panel) = &mut self.state.checkpoint_panel {
                        panel.lines.insert(0, text::error(&message));
                        panel.scroll = 0;
                        panel.lines.truncate(4096);
                    }
                }
            }
            _ => {
                if let Some(panel) = &mut self.state.checkpoint_panel {
                    panel.input.handle_key(key);
                }
            }
        }
        Ok(false)
    }
}

fn parse_prepare(verb: &str, argument: &str) -> Result<PrepareKind> {
    // create PATH... | accept ID SHA256 PATH... | restore ID SHA256 PATH...
    let mut parts = argument.split_whitespace();
    let (id, sha256) = if verb == "create" {
        (String::new(), String::new())
    } else {
        let id = parts.next().ok_or(text::PREPARE_USAGE)?.to_owned();
        let sha256 = parts.next().ok_or(text::PREPARE_USAGE)?.to_owned();
        (id, sha256)
    };
    let paths = parts.map(str::to_owned).collect::<Vec<_>>();
    if paths.is_empty() {
        return Err(text::PREPARE_USAGE.into());
    }
    let label = format!("tui-{verb}");
    match verb {
        "create" => Ok(PrepareKind::Create { paths, label }),
        "accept" => Ok(PrepareKind::Accept {
            id,
            sha256,
            paths,
            label,
        }),
        "restore" => Ok(PrepareKind::Restore {
            id,
            sha256,
            paths,
            label,
        }),
        _ => Err(text::UNKNOWN_COMMAND.into()),
    }
}
fn parse_diff(argument: &str) -> Result<QueryKind> {
    let mut parts = argument.split_whitespace();
    let id = parts.next().ok_or(text::DIFF_USAGE)?.to_owned();
    let sha256 = parts.next().ok_or(text::DIFF_USAGE)?.to_owned();
    Ok(QueryKind::Diff { id, sha256 })
}
fn json_lines(value: &Value) -> Result<Vec<String>> {
    let text = serde_json::to_string_pretty(value)?;
    if text.len() > 128 * 1024 {
        return Err(text::PRESENTATION_TOO_LONG.into());
    }
    Ok(text.lines().map(str::to_owned).collect())
}
