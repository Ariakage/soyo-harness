//! Read-only observability modal (`F-12`): usage, replay, diagnose and run
//! report over the active session's committed snapshot.
//!
//! Reuses the memory/checkpoint modal shell. Unlike checkpoint administration
//! nothing here stages or writes: every command projects persisted data through
//! [`soyo_app::observe`] and never dispatches a model request or tool call.
use super::*;
use crate::{i18n::observe as text, view::MemoryPanel};
use soyo_app::observe::{self, Diagnosis, StepStatus, Timeline, UsageReport};
use soyo_core::sqlite::{SessionSnapshot, SnapshotLimits, SqliteStore};

enum Task {
    Usage,
    Replay {
        run: Option<String>,
        step: Option<usize>,
    },
    Diagnose {
        run: Option<String>,
    },
    Report {
        run: Option<String>,
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

/// A snapshot of the active session, read through the immutable WAL snapshot
/// on the local backend and through `read_session_snapshot` on the remote one.
async fn session_snapshot(
    config: &Config,
    remote: Option<(Arc<HelperClient>, String)>,
    session: Option<soyo_core::contract::SessionId>,
    cancel: Arc<RunCancellation>,
) -> std::result::Result<SessionSnapshot, BoundaryError> {
    if let Some((client, session_id)) = remote {
        return backend::remote_session_snapshot(&client, &session_id, cancel)
            .await
            .map_err(|error| match error.downcast::<BoundaryError>() {
                Ok(error) => *error,
                Err(error) => failure("helper_protocol", error.to_string()),
            });
    }
    let session_id = match session {
        Some(id) => id,
        None => soyo_app::checkpoint_admin::resolve_session_id(config, None).await?,
    };
    SqliteStore::read_snapshot(
        &config.data_dir,
        session_id,
        SnapshotLimits::default(),
        cancel,
    )
    .await
}

fn format_cost(totals: &observe::UsageTotals) -> String {
    if totals.cost.is_empty() {
        return "—".into();
    }
    totals
        .cost
        .iter()
        .map(|(currency, micro)| {
            format!(
                "{currency} {:.6}{}",
                *micro as f64 / 1_000_000.0,
                if totals.estimated { "~" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn usage_lines(report: &UsageReport) -> Vec<String> {
    let mut lines = vec![
        text::USAGE_HEADER.into(),
        format!(
            "runs {} · requests {} · {} in / {} out tokens ({} cached)",
            report.totals.runs,
            report.totals.requests,
            report.totals.input_tokens,
            report.totals.output_tokens,
            report.totals.cached_input_tokens,
        ),
        format!("cost {}", format_cost(&report.totals)),
        String::new(),
    ];
    if report.groups.is_empty() {
        lines.push(text::USAGE_EMPTY.into());
        return lines;
    }
    for group in &report.groups {
        let dimension = match group.dimension {
            observe::UsageDimension::Run => "run",
            observe::UsageDimension::Profile => "profile",
            observe::UsageDimension::Day => "day",
        };
        lines.push(format!(
            "[{dimension}] {} — {} req, {}+{} tok, {}",
            group.key,
            group.totals.requests,
            group.totals.input_tokens,
            group.totals.output_tokens,
            format_cost(&group.totals),
        ));
    }
    lines
}

fn status_label(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Ok => "ok",
        StepStatus::Failed => "FAIL",
        StepStatus::Denied => "DENIED",
        StepStatus::Cancelled => "CANCEL",
        StepStatus::Unknown => "?",
    }
}

fn timeline_lines(timeline: &Timeline) -> Vec<String> {
    let mut lines = vec![
        format!(
            "Session {} · {} step(s) · {} run(s)",
            timeline.session_id,
            timeline.steps.len(),
            timeline.runs.len()
        ),
        text::REPLAY_STEP_HINT.into(),
        String::new(),
    ];
    for step in &timeline.steps {
        lines.push(format!(
            "{:>5}  {:<9} {:<18} {}{}",
            step.index,
            status_label(step.status),
            format!("{:?}", step.kind),
            step.label,
            step.error_code
                .as_ref()
                .map(|code| format!(" [{code}]"))
                .unwrap_or_default(),
        ));
    }
    lines
}

fn step_lines(timeline: &Timeline, index: usize) -> Vec<String> {
    let Some(step) = timeline.steps.get(index) else {
        return vec![text::step_missing(index)];
    };
    let mut lines = vec![
        format!(
            "Step {} · {} · {}",
            step.index,
            step.label,
            step.error_code.as_deref().unwrap_or("ok")
        ),
        String::new(),
        text::STEP_INPUT.into(),
    ];
    lines.extend(
        serde_json::to_string_pretty(&step.input)
            .unwrap_or_else(|_| step.input.to_string())
            .lines()
            .map(str::to_owned),
    );
    lines.push(String::new());
    lines.push(text::STEP_OUTPUT.into());
    lines.extend(
        serde_json::to_string_pretty(&step.output)
            .unwrap_or_else(|_| step.output.to_string())
            .lines()
            .map(str::to_owned),
    );
    lines
}

fn diagnose_lines(diagnosis: &Diagnosis) -> Vec<String> {
    let mut lines = vec![
        text::DIAGNOSE_HEADER.into(),
        format!("session {}", diagnosis.session_id),
    ];
    if let Some(run) = &diagnosis.run_id {
        lines.push(format!("run {run}"));
    }
    lines.push(String::new());
    lines.push(diagnosis.summary.clone());
    if let (Some(failed), Some(ok)) = (diagnosis.failed_step, diagnosis.last_ok_step) {
        lines.push(text::diagnose_steps(ok, failed));
    }
    if let Some(result) = &diagnosis.run_result {
        lines.push(format!(
            "run result {}",
            result["status"].as_str().unwrap_or("unavailable")
        ));
    }
    lines
}

fn report_lines(report: &observe::RunReport) -> Vec<String> {
    let mut lines = vec![
        text::REPORT_HEADER.into(),
        format!("session {}", report.session_id),
        format!("run {}", report.run_id),
        format!("steps {}", report.diagnosis.step_count),
        format!(
            "failure {}",
            report.diagnosis.error_code.as_deref().unwrap_or("none")
        ),
        String::new(),
    ];
    lines.push(text::REPORT_DIAGNOSIS.into());
    lines.push(report.diagnosis.summary.clone());
    match observe::report_markdown(report) {
        Ok(bytes) => {
            lines.push(String::new());
            lines.push(text::REPORT_MARKDOWN.into());
            lines.extend(String::from_utf8_lossy(&bytes).lines().map(str::to_owned));
        }
        Err(error) => lines.push(format!("report error: {}", error.0.code)),
    }
    lines
}

impl App {
    fn observe_settings(&self) -> String {
        text::settings(self.prepared.is_some())
    }
    fn observe_lines(&mut self, lines: Vec<String>) {
        let lines = lines.into_iter().map(|line| self.safe(&line)).collect();
        let settings = self.safe(&self.observe_settings());
        if let Some(panel) = &mut self.state.observe_panel {
            panel.lines = lines;
            panel.scroll = 0;
            panel.settings = settings;
        }
    }
    pub(super) async fn open_observe(&mut self, verb: &str, argument: &str) -> Result<()> {
        if self.pending.is_some() {
            return Err(text::PENDING_REQUEST.into());
        }
        if self.ui_job.is_some() {
            return Err(text::UI_ACTIVE_WAIT.into());
        }
        self.state.command_palette = None;
        self.state.completion.dismiss();
        self.state.observe_panel = Some(MemoryPanel {
            settings: self.observe_settings(),
            ..Default::default()
        });
        let command = if argument.is_empty() {
            verb.to_owned()
        } else {
            format!("{verb} {argument}")
        };
        self.observe_command(&command).await
    }
    fn observe_worker(&mut self, task: Task) -> Result<()> {
        if self.ui_job.is_some() {
            return Err(text::UI_ACTIVE_CANCEL.into());
        }
        let cancel = Arc::new(RunCancellation::new());
        let config = self.config.clone();
        let session = self.prepared.as_ref().map(|prepared| prepared.session().id);
        let remote = match self.prepared.as_ref() {
            Some(SessionBackend::Remote(remote)) => {
                Some((remote.client.clone(), remote.session.id.0.to_string()))
            }
            _ => None,
        };
        let checking = cancel.clone();
        let join = tokio::spawn(async move {
            let snapshot = session_snapshot(&config, remote, session, checking.clone())
                .await
                .map_err(|error| {
                    if checking.is_cancelled() {
                        failure("cancelled", text::QUERY_CANCELLED)
                    } else {
                        error
                    }
                })?;
            if checking.is_cancelled() {
                return Err(failure("cancelled", text::QUERY_CANCELLED));
            }
            // The projection is a pure fold over the bounded snapshot; running
            // it inline keeps the read snapshot's lifetime local.
            let lines = observe_task(&snapshot, task)?;
            Ok(services::UiResult::Observe(lines))
        });
        self.begin_ui(cancel, join);
        if let Some(panel) = &mut self.state.observe_panel {
            panel.busy = true;
        }
        Ok(())
    }
    pub(super) async fn observe_command(&mut self, command: &str) -> Result<()> {
        let command = command.trim();
        let (verb, argument) = command.split_once(' ').unwrap_or((command, ""));
        let argument = argument.trim();
        if matches!(verb, "close" | "exit") {
            if let Some(job) = &self.ui_job {
                job.cancel.cancel();
            }
            self.state.observe_panel = None;
            return Ok(());
        }
        if self.ui_job.is_some() {
            return Err(text::OPERATION_ACTIVE.into());
        }
        let mut parts = argument.split_whitespace();
        let task = match verb {
            "help" => {
                self.observe_lines(
                    text::HELP_LINES
                        .iter()
                        .copied()
                        .map(str::to_owned)
                        .collect(),
                );
                return Ok(());
            }
            "usage" | "u" => Task::Usage,
            "replay" | "timeline" => Task::Replay {
                run: parts.next().map(str::to_owned),
                step: None,
            },
            "step" => {
                let index = parts.next().ok_or(text::STEP_USAGE)?;
                let index = index.parse::<usize>().map_err(|_| text::STEP_USAGE)?;
                Task::Replay {
                    run: None,
                    step: Some(index),
                }
            }
            "diagnose" | "diag" => Task::Diagnose {
                run: parts.next().map(str::to_owned),
            },
            "report" => Task::Report {
                run: parts.next().map(str::to_owned),
            },
            "" => Task::Replay {
                run: None,
                step: None,
            },
            _ => return Err(text::UNKNOWN_COMMAND.into()),
        };
        self.observe_worker(task)
    }
    pub(super) fn finish_observe(&mut self, lines: Vec<String>, cancelled: bool) -> Result<()> {
        if let Some(panel) = &mut self.state.observe_panel {
            panel.busy = false;
        }
        if self.state.observe_panel.is_some() {
            self.observe_lines(if cancelled {
                vec![text::QUERY_CANCELLED.into()]
            } else {
                lines
            });
        }
        Ok(())
    }
    pub(super) async fn observe_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.kind != KeyEventKind::Press && matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if let Some(job) = &self.ui_job {
                        job.cancel.cancel();
                    } else if let Some(panel) = &mut self.state.observe_panel {
                        panel.input.clear();
                    }
                    return Ok(false);
                }
                KeyCode::Char('d') => {
                    self.observe_command("close").await?;
                    return Ok(false);
                }
                KeyCode::Char('p') => return Ok(false),
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => self.observe_command("close").await?,
            KeyCode::PageUp => {
                if let Some(panel) = &mut self.state.observe_panel {
                    panel.scroll = panel.scroll.saturating_sub(panel.page_rows);
                }
            }
            KeyCode::PageDown => {
                if let Some(panel) = &mut self.state.observe_panel {
                    panel.scroll = panel.scroll.saturating_add(panel.page_rows);
                }
            }
            KeyCode::Enter => {
                let command = self
                    .state
                    .observe_panel
                    .as_ref()
                    .map(|panel| panel.input.text().to_owned())
                    .unwrap_or_default();
                let result = self.observe_command(&command).await;
                if result.is_ok()
                    && let Some(panel) = &mut self.state.observe_panel
                {
                    panel.input.clear();
                }
                if let Err(error) = result {
                    let message = self.safe(&error.to_string());
                    if let Some(panel) = &mut self.state.observe_panel {
                        panel.lines.insert(0, text::error(&message));
                        panel.scroll = 0;
                        panel.lines.truncate(4096);
                    }
                }
            }
            _ => {
                if let Some(panel) = &mut self.state.observe_panel {
                    panel.input.handle_key(key);
                }
            }
        }
        Ok(false)
    }
}

/// Projection dispatch over an already-read snapshot. Pure: no IO, no model or
/// tool dispatch. Returns the rendered lines for the modal.
fn observe_task(
    snapshot: &SessionSnapshot,
    task: Task,
) -> std::result::Result<Vec<String>, BoundaryError> {
    match task {
        Task::Usage => {
            let records = observe::metrics_records(snapshot);
            let report = observe::usage_for_session(snapshot.session.id, records);
            Ok(usage_lines(&report))
        }
        Task::Replay { run, step } => {
            let timeline = observe::project_timeline(snapshot, run.as_deref())?;
            Ok(match step {
                Some(index) => step_lines(&timeline, index),
                None => timeline_lines(&timeline),
            })
        }
        Task::Diagnose { run } => {
            let timeline = observe::project_timeline(snapshot, run.as_deref())?;
            let result = snapshot
                .runs
                .iter()
                .find(|row| match run.as_deref() {
                    Some(wanted) => row["run_id"].as_str() == Some(wanted),
                    None => !row["finished_at"].is_null(),
                })
                .map(|row| row["result"].clone())
                .filter(|result| !result.is_null());
            let mut diagnosis = observe::diagnose(&timeline, result);
            diagnosis.run_id = run;
            Ok(diagnose_lines(&diagnosis))
        }
        Task::Report { run } => {
            let run_id = match run {
                Some(run) => run,
                None => snapshot
                    .runs
                    .iter()
                    .rev()
                    .find(|row| !row["finished_at"].is_null())
                    .or_else(|| snapshot.runs.last())
                    .and_then(|row| row["run_id"].as_str())
                    .map(str::to_owned)
                    .ok_or_else(|| failure("run_not_found", text::NO_RUN))?,
            };
            let report = observe::run_report(snapshot, &run_id)?;
            Ok(report_lines(&report))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use soyo_core::{
        contract::{
            Event, EventPayload, HistoryItem, ModelProtocol, Session, SessionId, ToolResult,
        },
        sqlite::SessionSnapshot,
    };

    fn snapshot() -> SessionSnapshot {
        let session = Session {
            id: SessionId::new(),
            workspace: std::path::PathBuf::from("/tmp/ws"),
            work_mode: soyo_core::contract::WorkMode::Work,
            profile: "p".into(),
            protocol: ModelProtocol::Responses,
            model: "m".into(),
            history: vec![HistoryItem::UserText("hello".into())],
        };
        SessionSnapshot {
            created_at: "2026-11-24T00:00:00Z".into(),
            source_database_schema: 2,
            last_change_seq: 2,
            last_event_seq: 2,
            session,
            runs: vec![json!({"run_id":"r1","result":null,"started_at":1,"finished_at":null})],
            tool_calls: vec![],
            approval_requests: vec![],
            events: vec![
                serde_json::to_value(Event {
                    schema_version: 1,
                    session_id: SessionId::new(),
                    run_id: None,
                    seq: 1,
                    timestamp: "2026-11-24T00:00:01Z".into(),
                    payload: EventPayload::ToolFinished {
                        call_id: "c1".into(),
                        name: "exec".into(),
                        result: ToolResult::failure("tool_error", "boom"),
                    },
                })
                .unwrap(),
            ],
            changes: vec![],
            metrics: vec![],
            project_context: vec![],
            background_jobs: vec![],
        }
    }

    #[test]
    fn replay_renders_the_failed_step_and_diagnose_pinpoints_it() {
        let snapshot = snapshot();
        let lines = observe_task(
            &snapshot,
            Task::Replay {
                run: None,
                step: None,
            },
        )
        .unwrap();
        let text = lines.join("\n");
        assert!(text.contains("exec"), "{text}");
        assert!(text.contains("FAIL"), "{text}");
        let lines = observe_task(&snapshot, Task::Diagnose { run: None }).unwrap();
        assert!(lines.join("\n").contains("tool_error"));
    }

    #[test]
    fn usage_without_metrics_reports_the_empty_state() {
        let snapshot = snapshot();
        let lines = observe_task(&snapshot, Task::Usage).unwrap();
        assert!(lines.join("\n").contains("runs 0"));
    }

    #[test]
    fn report_without_a_run_reports_not_found() {
        let mut snapshot = snapshot();
        snapshot.runs = vec![];
        let error = observe_task(&snapshot, Task::Report { run: None }).unwrap_err();
        assert_eq!(error.0.code, "run_not_found");
    }
}
