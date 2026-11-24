//! `soyo remote`: owner-side remote worker administration (`D-117`).
//!
//! These commands talk to the owning helper over its resolved endpoint, so
//! they also work from another machine when `SOYO_HELPER_LISTEN` or
//! `[remote.client]` selects the authenticated HTTP(S) transport.

use super::{Cli, helper_commands};
use clap::Subcommand;
use serde_json::{Value, json};
use soyo_core::{
    config::{Config, Output},
    contract::BoundaryError,
};
use soyo_ipc::{HelperTarget, types::DesktopError};

#[derive(Debug, Clone, Subcommand)]
pub enum RemoteAction {
    /// List remote workers registered with the helper.
    Workers,
    /// Assign a queued task to a remote worker id.
    Assign { task_id: String, worker_id: String },
    /// Clear a queued task's remote worker assignment.
    Unassign { task_id: String },
    /// Show a task and the worker result held back from the workspace.
    Show { task_id: String },
    /// Apply a completed worker result through the three-way check.
    Apply {
        task_id: String,
        /// Only apply these workspace-relative paths (repeatable).
        #[arg(long = "path")]
        paths: Vec<String>,
    },
}

fn from_desktop(error: DesktopError) -> BoundaryError {
    BoundaryError::new(error.code, error.message)
}

fn print_value(config: &Config, value: &Value, text: impl FnOnce()) -> Result<(), BoundaryError> {
    match config.output {
        Output::Text => {
            text();
            Ok(())
        }
        Output::Json | Output::Jsonl => {
            println!("{value}");
            Ok(())
        }
    }
}

pub async fn run(action: &RemoteAction, _cli: &Cli, config: &Config) -> Result<u8, BoundaryError> {
    let target = HelperTarget::from_config(config);
    let client = helper_commands::connect_checked_for(&target, true)
        .await
        .map_err(from_desktop)?;
    match action {
        RemoteAction::Workers => {
            let workers: Vec<Value> = client
                .call_typed("remote_workers", &json!({}))
                .await
                .map_err(from_desktop)?;
            let value = json!({"schema_version": 1, "data": {"workers": workers}});
            print_value(config, &value, || {
                if workers.is_empty() {
                    println!("No remote workers registered.");
                    return;
                }
                for worker in &workers {
                    let progress = worker
                        .get("progress")
                        .and_then(|progress| {
                            Some(format!(
                                " · task {} {}",
                                progress.get("task_id")?.as_str()?,
                                progress.get("phase")?.as_str()?
                            ))
                        })
                        .unwrap_or_default();
                    println!(
                        "{} · {} · {} · v{} · last seen {}{}",
                        worker
                            .get("worker_id")
                            .and_then(Value::as_str)
                            .unwrap_or("?"),
                        worker.get("arch").and_then(Value::as_str).unwrap_or("?"),
                        worker
                            .get("sandbox_backends")
                            .and_then(Value::as_array)
                            .map(|backends| backends
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(","))
                            .filter(|text| !text.is_empty())
                            .unwrap_or_else(|| "no native sandbox".into()),
                        worker.get("version").and_then(Value::as_str).unwrap_or("?"),
                        worker
                            .get("last_seen_at")
                            .and_then(Value::as_str)
                            .unwrap_or("?"),
                        progress
                    );
                }
            })?;
            Ok(0)
        }
        RemoteAction::Assign { task_id, worker_id } => {
            let task: Value = client
                .call_typed(
                    "remote_task_assign",
                    &json!({"task_id": task_id, "worker_id": worker_id}),
                )
                .await
                .map_err(from_desktop)?;
            let value = json!({"schema_version": 1, "data": {"task": task}});
            print_value(config, &value, || {
                println!("Task {task_id} assigned to worker {worker_id}.");
            })?;
            Ok(0)
        }
        RemoteAction::Unassign { task_id } => {
            let task: Value = client
                .call_typed(
                    "remote_task_assign",
                    &json!({"task_id": task_id, "worker_id": Value::Null}),
                )
                .await
                .map_err(from_desktop)?;
            let value = json!({"schema_version": 1, "data": {"task": task}});
            print_value(config, &value, || {
                println!("Task {task_id} has no remote worker assignment.");
            })?;
            Ok(0)
        }
        RemoteAction::Show { task_id } => {
            let show: Value = client
                .call_typed("remote_task_show", &json!({"task_id": task_id}))
                .await
                .map_err(from_desktop)?;
            let value = json!({"schema_version": 1, "data": show});
            print_value(config, &value, || {
                println!(
                    "Task {task_id} · {}",
                    show["task"]["status"].as_str().unwrap_or("?")
                );
                match show.get("result").and_then(Value::as_object) {
                    Some(_) => {
                        let changed = show["result"]["changed_files"]
                            .as_array()
                            .map(Vec::len)
                            .unwrap_or(0);
                        println!(
                            "Worker result: {} changed file(s), received {}",
                            changed,
                            show["result"]["received_at"].as_str().unwrap_or("?")
                        );
                        if let Some(summary) = show["result"]["summary"].as_str() {
                            println!("{summary}");
                        }
                        for change in show["result"]["changed_files"]
                            .as_array()
                            .into_iter()
                            .flatten()
                        {
                            println!(
                                "  {:<8} {}",
                                change["kind"].as_str().unwrap_or("?"),
                                change["path"].as_str().unwrap_or("?")
                            );
                        }
                    }
                    None => println!("No remote result is held for this task."),
                }
            })?;
            Ok(0)
        }
        RemoteAction::Apply { task_id, paths } => {
            let params = if paths.is_empty() {
                json!({"task_id": task_id})
            } else {
                json!({"task_id": task_id, "paths": paths})
            };
            let report: Value = client
                .call_typed("remote_task_apply", &params)
                .await
                .map_err(from_desktop)?;
            let applied = report["applied"].as_bool().unwrap_or(false);
            let value = json!({"schema_version": 1, "data": report});
            print_value(config, &value, || {
                if applied {
                    for receipt in report["receipts"].as_array().into_iter().flatten() {
                        println!(
                            "  {:<9} {}",
                            receipt["operation"].as_str().unwrap_or("?"),
                            receipt["path"].as_str().unwrap_or("?")
                        );
                    }
                } else {
                    println!("Apply refused; nothing was written:");
                    for conflict in report["conflicts"].as_array().into_iter().flatten() {
                        println!(
                            "  conflict {} — {}",
                            conflict["path"].as_str().unwrap_or("?"),
                            conflict["reason"].as_str().unwrap_or("local divergence")
                        );
                    }
                }
            })?;
            Ok(if applied { 0 } else { 2 })
        }
    }
}
