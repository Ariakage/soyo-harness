//! Read-only observability commands: `soyo usage`, `soyo replay`,
//! `soyo diagnose`, `soyo session report`. Each projects persisted session
//! data without constructing a runtime, dispatching a model request, or
//! executing a tool — every answer comes from `read_snapshot` or the
//! persisted metrics store.
use super::*;
use clap::Args;
use soyo_app::observe::{self, Diagnosis, StepStatus, Timeline, UsageReport, UsageTotals};
use soyo_core::{
    config::{Config, Output},
    contract::{BoundaryError, RunId, SessionId},
    sqlite::{SnapshotLimits, SqliteStore},
};
use std::{io::Write, path::PathBuf, sync::Arc};

#[derive(Args, Clone)]
pub struct UsageArgs {
    /// Restrict the aggregation to one session's persisted run metrics.
    #[arg(long)]
    session: Option<String>,
    /// Include runs started within the last N days (UTC today included).
    #[arg(long)]
    since: Option<u32>,
    /// Group rows by run, profile, or UTC day.
    #[arg(long, value_enum, default_value = "run")]
    by: UsageGroupBy,
}
#[derive(Clone, Copy, ValueEnum)]
enum UsageGroupBy {
    Run,
    Profile,
    Day,
}

#[derive(Args, Clone)]
pub struct ReplayArgs {
    /// Persisted session identifier.
    session_id: String,
    /// Restrict the timeline to one run.
    #[arg(long)]
    run: Option<String>,
    /// Show the full input/output detail for step N (0-based index).
    #[arg(long)]
    step: Option<usize>,
}

#[derive(Args, Clone)]
pub struct DiagnoseArgs {
    /// Persisted session identifier.
    session_id: String,
    /// Diagnose a single run inside the session.
    #[arg(long)]
    run: Option<String>,
}

#[derive(Args, Clone)]
pub struct ReportArgs {
    /// Persisted session identifier.
    session_id: String,
    /// Run inside the session to report on. Required when more than one run
    /// is persisted; defaults to the most recent finished run otherwise.
    #[arg(long)]
    run: Option<String>,
    #[arg(long, value_enum, default_value = "markdown")]
    format: ReportFormat,
    /// Destination file; parent must exist. Exclusive create, mode 0600.
    #[arg(long)]
    file: PathBuf,
}
#[derive(Clone, Copy, ValueEnum)]
enum ReportFormat {
    Markdown,
    Json,
}

fn parse_session(raw: &str) -> Result<SessionId, BoundaryError> {
    serde_json::from_value(serde_json::json!(raw))
        .map_err(|_| cli_error("invalid_session_id", "invalid session ID"))
}

fn parse_run(raw: &str) -> Result<RunId, BoundaryError> {
    serde_json::from_value(serde_json::json!(raw))
        .map_err(|_| cli_error("invalid_run_id", "invalid run ID"))
}

async fn snapshot(
    config: &Config,
    session_id: SessionId,
    cancel: Arc<Cancel>,
) -> Result<soyo_core::sqlite::SessionSnapshot, BoundaryError> {
    SqliteStore::read_snapshot(
        &config.data_dir,
        session_id,
        SnapshotLimits::default(),
        cancel,
    )
    .await
}

fn format_cost(totals: &UsageTotals) -> String {
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

fn usage_text(report: &UsageReport) -> String {
    let mut out = String::new();
    let scope = report
        .session_id
        .as_deref()
        .map(|id| format!("session {id}"))
        .unwrap_or_else(|| "all sessions".into());
    out.push_str(&format!(
        "Usage for {scope}: {} run(s), {} request(s), {} in / {} out tokens ({} cached)\n",
        report.totals.runs,
        report.totals.requests,
        report.totals.input_tokens,
        report.totals.output_tokens,
        report.totals.cached_input_tokens,
    ));
    out.push_str(&format!("Cost: {}\n", format_cost(&report.totals)));
    for group in &report.groups {
        out.push_str(&format!(
            "  {} — {} req, {}+{} tok, {}\n",
            group.key,
            group.totals.requests,
            group.totals.input_tokens,
            group.totals.output_tokens,
            format_cost(&group.totals),
        ));
    }
    out
}

fn replay_text(timeline: &Timeline, step: Option<usize>) -> String {
    let mut out = String::new();
    if let Some(index) = step {
        let Some(step) = timeline.steps.get(index) else {
            return format!(
                "Step {index} does not exist ({} steps)\n",
                timeline.steps.len()
            );
        };
        out.push_str(&format!(
            "Step {} · {} · {}\n\nInput:\n{}\n\nOutput:\n{}\n",
            step.index,
            step.label,
            step.error_code.as_deref().unwrap_or("ok"),
            serde_json::to_string_pretty(&step.input).unwrap_or_else(|_| step.input.to_string()),
            serde_json::to_string_pretty(&step.output).unwrap_or_else(|_| step.output.to_string()),
        ));
        return out;
    }
    out.push_str(&format!(
        "Session {} · {} step(s) · {} run(s)\n\n",
        timeline.session_id,
        timeline.steps.len(),
        timeline.runs.len()
    ));
    for step in &timeline.steps {
        let status = match step.status {
            StepStatus::Ok => "ok",
            StepStatus::Failed => "FAIL",
            StepStatus::Denied => "DENIED",
            StepStatus::Cancelled => "CANCEL",
            StepStatus::Unknown => "?",
        };
        out.push_str(&format!(
            "{:>5}  {:<7} {:<18} {}{}\n",
            step.index,
            format!("{:?}", step.kind),
            status,
            step.label,
            step.error_code
                .as_ref()
                .map(|code| format!(" [{code}]"))
                .unwrap_or_default(),
        ));
    }
    out
}

fn diagnose_text(diagnosis: &Diagnosis) -> String {
    let mut out = String::new();
    out.push_str(&format!("Session {}\n", diagnosis.session_id));
    if let Some(run) = &diagnosis.run_id {
        out.push_str(&format!("Run {}\n", run));
    }
    out.push_str(&diagnosis.summary);
    out.push('\n');
    if let (Some(failed), Some(ok)) = (diagnosis.failed_step, diagnosis.last_ok_step) {
        out.push_str(&format!(
            "Last healthy step: {ok}; first failure: {failed}\n"
        ));
    }
    if let Some(result) = &diagnosis.run_result {
        out.push_str(&format!(
            "Run result: {}\n",
            result["status"].as_str().unwrap_or("unavailable")
        ));
    }
    out
}

async fn run_usage(
    config: &Config,
    args: &UsageArgs,
    cancel: Arc<Cancel>,
) -> Result<UsageReport, BoundaryError> {
    if let Some(raw) = &args.session {
        let session_id = parse_session(raw)?;
        // The per-session metrics path reads through the immutable snapshot so
        // a read-only query never opens the store for write.
        let snapshot = snapshot(config, session_id, cancel.clone()).await?;
        let mut records = Vec::new();
        for row in &snapshot.metrics {
            if let (Some(run), Ok(metrics)) = (
                row["run_id"].as_str(),
                serde_json::from_value::<soyo_core::metrics::RunMetrics>(row["data"].clone()),
            ) {
                let run_id: RunId = serde_json::from_value(serde_json::json!(run))
                    .map_err(|_| cli_error("storage_corrupt", "invalid metrics run ID"))?;
                records.push(soyo_core::sqlite::RunMetricsRecord { run_id, metrics });
            }
        }
        let mut report = observe::usage_for_session(session_id, records);
        report.groups.retain(|group| {
            matches!(
                (args.by, group.dimension),
                (UsageGroupBy::Run, observe::UsageDimension::Run)
                    | (UsageGroupBy::Profile, observe::UsageDimension::Profile)
            ) || matches!(args.by, UsageGroupBy::Day)
        });
        return Ok(report);
    }
    // Cross-session aggregation goes through the shared store (read-only
    // SELECT against the live database). `--since` bounds the range by
    // `runs.started_at`.
    let since_days = args.since.unwrap_or(30).clamp(1, 365);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let since = now - i64::from(since_days) * 86_400;
    let store = {
        let owned = config.clone();
        soyo_app::offload(move || session::open(&owned))
            .await
            .map_err(|error| match error.downcast::<BoundaryError>() {
                Ok(error) => *error,
                Err(error) => cli_error("storage_error", &error.to_string()),
            })?
    };
    let records = store.usage_metrics_since(since).await?;
    let mut report = observe::usage_since(records, since, 0);
    report.groups.retain(|group| {
        matches!(
            (args.by, group.dimension),
            (UsageGroupBy::Run, observe::UsageDimension::Run)
                | (UsageGroupBy::Profile, observe::UsageDimension::Profile)
                | (UsageGroupBy::Day, observe::UsageDimension::Day)
        )
    });
    Ok(report)
}

async fn run_replay(
    config: &Config,
    args: &ReplayArgs,
    cancel: Arc<Cancel>,
) -> Result<Timeline, BoundaryError> {
    let session_id = parse_session(&args.session_id)?;
    let snapshot = snapshot(config, session_id, cancel).await?;
    let run = args
        .run
        .as_deref()
        .map(parse_run)
        .transpose()?
        .map(|run| run.0.to_string());
    let timeline = observe::project_timeline(&snapshot, run.as_deref())?;
    Ok(timeline)
}

async fn run_diagnose(
    config: &Config,
    args: &DiagnoseArgs,
    cancel: Arc<Cancel>,
) -> Result<Diagnosis, BoundaryError> {
    let session_id = parse_session(&args.session_id)?;
    let snapshot = snapshot(config, session_id, cancel).await?;
    let run = args
        .run
        .as_deref()
        .map(parse_run)
        .transpose()?
        .map(|run| run.0.to_string());
    let timeline = observe::project_timeline(&snapshot, run.as_deref())?;
    let result = snapshot
        .runs
        .iter()
        .find(|row| {
            let Some(wanted) = run.as_deref() else {
                return !row["finished_at"].is_null();
            };
            row["run_id"].as_str() == Some(wanted)
        })
        .map(|row| row["result"].clone())
        .filter(|result| !result.is_null());
    let mut diagnosis = observe::diagnose(&timeline, result);
    diagnosis.run_id = run;
    Ok(diagnosis)
}

async fn run_report(
    config: &Config,
    args: &ReportArgs,
    cancel: Arc<Cancel>,
) -> Result<serde_json::Value, BoundaryError> {
    let session_id = parse_session(&args.session_id)?;
    let snapshot = snapshot(config, session_id, cancel.clone()).await?;
    // Resolve the run: explicit `--run`, else the most recently finished run,
    // else the only persisted run.
    let run_id = if let Some(raw) = &args.run {
        parse_run(raw)?.0.to_string()
    } else {
        snapshot
            .runs
            .iter()
            .rev()
            .find(|row| !row["finished_at"].is_null())
            .or_else(|| snapshot.runs.last())
            .and_then(|row| row["run_id"].as_str())
            .map(str::to_owned)
            .ok_or_else(|| cli_error("run_not_found", "no run is persisted in this session"))?
    };
    let report = observe::run_report(&snapshot, &run_id)?;
    let bytes = match args.format {
        ReportFormat::Markdown => observe::report_markdown(&report)?,
        ReportFormat::Json => serde_json::to_vec_pretty(&report)
            .map_err(|_| cli_error("export_error", "cannot encode run report"))?,
    };
    let workspace = config.cwd.clone();
    let mode = config.mode;
    let target = if args.file.is_absolute() {
        args.file.clone()
    } else {
        workspace.join(&args.file)
    };
    let session_workspace = snapshot.session.workspace.clone();
    let cancel2 = cancel.clone();
    let receipt = tokio::task::spawn_blocking(move || {
        if cancel2.is_cancelled() {
            return Err(cli_error("cancelled", "report cancelled"));
        }
        soyo_app::export::publish_session(
            &target,
            &bytes,
            &workspace,
            &session_workspace,
            mode,
            cancel2.as_ref(),
        )
    })
    .await
    .map_err(|_| cli_error("export_worker_failed", "report worker failed"))??;
    let mut value = receipt;
    value["session_id"] = serde_json::json!(session_id.0.to_string());
    value["run_id"] = serde_json::json!(run_id);
    value["format"] = match args.format {
        ReportFormat::Markdown => "markdown",
        ReportFormat::Json => "json",
    }
    .into();
    value["steps"] = serde_json::json!(report.timeline.steps.len());
    Ok(value)
}

/// Dispatch one observability command. Returns the JSON payload for machine
/// output or the rendered text for terminal output.
pub async fn run(
    config: &Config,
    command: &ObserveCommand,
    cancel: Arc<Cancel>,
) -> Result<(serde_json::Value, String), BoundaryError> {
    match command {
        ObserveCommand::Usage(args) => {
            let report = run_usage(config, args, cancel).await?;
            let value = serde_json::to_value(&report)
                .map_err(|_| cli_error("serialization_error", "cannot encode usage report"))?;
            Ok((value, usage_text(&report)))
        }
        ObserveCommand::Replay(args) => {
            let timeline = run_replay(config, args, cancel).await?;
            let text = replay_text(&timeline, args.step);
            let mut value = serde_json::to_value(&timeline)
                .map_err(|_| cli_error("serialization_error", "cannot encode timeline"))?;
            if let Some(index) = args.step {
                // `--step` narrows the JSON payload to the requested step.
                value = serde_json::to_value(
                    timeline
                        .steps
                        .get(index)
                        .ok_or_else(|| cli_error("invalid_step", "step index out of range"))?,
                )
                .map_err(|_| cli_error("serialization_error", "cannot encode step"))?;
            }
            Ok((value, text))
        }
        ObserveCommand::Diagnose(args) => {
            let diagnosis = run_diagnose(config, args, cancel).await?;
            let value = serde_json::to_value(&diagnosis)
                .map_err(|_| cli_error("serialization_error", "cannot encode diagnosis"))?;
            Ok((value, diagnose_text(&diagnosis)))
        }
        ObserveCommand::Report(args) => {
            let receipt = run_report(config, args, cancel).await?;
            let text = format!(
                "Wrote {} ({} bytes, sha256 {})",
                receipt["path"].as_str().unwrap_or("report"),
                receipt["bytes"],
                receipt["sha256"].as_str().unwrap_or("")
            );
            Ok((receipt, text))
        }
    }
}

/// Top-level observability subcommands.
pub enum ObserveCommand {
    Usage(UsageArgs),
    Replay(ReplayArgs),
    Diagnose(DiagnoseArgs),
    Report(ReportArgs),
}

/// Emit the observe result on stdout (or the error on stderr/JSON), honouring
/// `--output text|json|jsonl`.
pub fn emit(
    config: &Config,
    outcome: Result<(serde_json::Value, String), BoundaryError>,
    jsonl_type: &str,
) -> u8 {
    let (data, text) = match outcome {
        Ok(pair) => pair,
        Err(error) => {
            let code = if error.0.code == "cancelled" { 130 } else { 2 };
            match config.output {
                Output::Json | Output::Jsonl => {
                    let mut value =
                        serde_json::json!({"schema_version":1,"data":null,"error":error.0});
                    if config.output == Output::Jsonl {
                        value["type"] = "observe.failed".into();
                    }
                    println!("{value}");
                }
                Output::Text => {
                    eprintln!(
                        "soyo: {}: {}",
                        safe_output(&error.0.code, config),
                        safe_output(&error.0.message, config)
                    );
                }
            }
            return code;
        }
    };
    let rendered = match config.output {
        Output::Json => serde_json::json!({"schema_version":1,"data":data}).to_string(),
        Output::Jsonl => serde_json::json!({
            "schema_version":1,
            "type":jsonl_type,
            "data":data,
        })
        .to_string(),
        Output::Text => text,
    };
    match writeln!(io::stdout().lock(), "{}", safe_terminal_text(&rendered)) {
        Ok(()) => 0,
        Err(_) => fail("cannot write observe result"),
    }
}
