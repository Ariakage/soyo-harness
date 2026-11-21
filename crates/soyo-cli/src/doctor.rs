//! Offline host, sandbox, storage, configuration, and capability diagnostics (`soyo doctor`, `F-15`).

use serde::Serialize;
use serde_json::Value;
use soyo_core::{
    config::{self, Config, Environment, Output, Overrides, RuntimeDirs},
    contract::BoundaryError,
    execution::{self, BackendKind, HostEnvironmentProbe},
    redaction,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Unsupported,
}

impl CheckStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
            Self::Unsupported => "UNSUPPORTED",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub id: String,
    pub category: &'static str,
    pub status: CheckStatus,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlatformSupportEntry {
    pub target: &'static str,
    pub status: &'static str,
    pub scope: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorSummary {
    pub ok: usize,
    pub warn: usize,
    pub fail: usize,
    pub unsupported: usize,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub version: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
    pub strict: bool,
    pub sandbox: HostEnvironmentProbe,
    pub checks: Vec<DoctorCheck>,
    pub platform_matrix: Vec<PlatformSupportEntry>,
    pub summary: DoctorSummary,
}

pub fn run(
    overrides: Overrides,
    env: Environment,
    output_override: Option<Output>,
    config_explicit: bool,
    strict: bool,
) -> u8 {
    let Ok(process_cwd) = std::env::current_dir() else {
        eprintln!("cannot determine current working directory");
        return 2;
    };
    let runtime_dirs = match config::resolve_runtime_dirs(&overrides, &env, &process_cwd) {
        Ok(dirs) => Some(dirs),
        Err(err) => {
            eprintln!("invalid runtime directory resolution: {}", err.0.message);
            None
        }
    };
    let config_result = config::load(overrides, env.clone(), &process_cwd);

    let output = match (&config_result, output_override) {
        (_, Some(explicit)) => explicit,
        (Ok(cfg), None) => cfg.output,
        (Err(_), None) => Output::Text,
    };

    let report = build_report(
        config_result.as_ref(),
        runtime_dirs.as_ref(),
        &env,
        config_explicit,
        strict,
    );

    let secrets: Vec<String> = match &config_result {
        Ok(cfg) => collect_secrets(cfg).unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    let mut payload = serde_json::json!({
        "schema_version": 1,
        "kind": "doctor",
        "action": "check",
        "data": report,
    });
    if output == Output::Jsonl {
        payload["type"] = serde_json::json!("doctor.result");
    }
    if redaction::redact(&mut payload, &secrets).is_err() {
        eprintln!("unable to sanitize doctor output");
        return 1;
    }

    let exit_code = if report.summary.passed { 0 } else { 1 };
    let write_res = match output {
        Output::Json | Output::Jsonl => writeln!(io::stdout().lock(), "{payload}"),
        Output::Text => {
            let text = render_text(&payload);
            let sanitized = crate::terminal::redacted_text(&text, &secrets);
            writeln!(io::stdout().lock(), "{sanitized}")
        }
    };

    if write_res.is_err() {
        return 1;
    }
    exit_code
}

fn collect_secrets(config: &Config) -> Result<Vec<String>, BoundaryError> {
    config
        .secret_env_names()
        .iter()
        .map(|name| config.secret(name).map(str::to_owned))
        .collect()
}

fn build_report(
    config_result: Result<&Config, &BoundaryError>,
    runtime_dirs: Option<&RuntimeDirs>,
    env: &Environment,
    config_explicit: bool,
    strict: bool,
) -> DoctorReport {
    let env_pairs = env.export_pairs();
    let configured_backend = config_result
        .map(|cfg| cfg.sandbox.backend)
        .unwrap_or(BackendKind::Auto);
    let sandbox_probe = execution::probe_host_environment(configured_backend);

    let mut checks = Vec::new();
    checks.push(sandbox_check(&sandbox_probe));
    checks.push(config_check(config_result, runtime_dirs, config_explicit));

    if let Ok(cfg) = config_result {
        checks.push(profiles_check(cfg));
    }

    append_directory_checks(&mut checks, config_result.ok(), runtime_dirs, &env_pairs);
    checks.push(git_check(&env_pairs));

    if let Ok(cfg) = config_result {
        checks.push(js_repl_check(cfg, &env_pairs));
        checks.push(code_intel_check(cfg, &env_pairs));
        checks.push(extensions_check(cfg));
    }

    let ok = checks
        .iter()
        .filter(|check| check.status == CheckStatus::Ok)
        .count();
    let warn = checks
        .iter()
        .filter(|check| check.status == CheckStatus::Warn)
        .count();
    let fail = checks
        .iter()
        .filter(|check| check.status == CheckStatus::Fail)
        .count();
    let unsupported = checks
        .iter()
        .filter(|check| check.status == CheckStatus::Unsupported)
        .count();
    let passed = if strict {
        fail == 0 && warn == 0 && unsupported == 0
    } else {
        fail == 0
    };

    DoctorReport {
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        strict,
        sandbox: sandbox_probe,
        checks,
        platform_matrix: platform_support_matrix(),
        summary: DoctorSummary {
            ok,
            warn,
            fail,
            unsupported,
            passed,
        },
    }
}

fn sandbox_check(probe: &HostEnvironmentProbe) -> DoctorCheck {
    let status = if probe.configured_backend == BackendKind::Host {
        CheckStatus::Warn
    } else if probe.native_sandbox_available {
        CheckStatus::Ok
    } else if probe.resolved_backend.is_none() {
        CheckStatus::Unsupported
    } else {
        CheckStatus::Fail
    };
    DoctorCheck {
        id: "native_sandbox".into(),
        category: "sandbox",
        status,
        summary: probe.summary.clone(),
        detail: probe.blocker.clone(),
    }
}

fn config_check(
    config_result: Result<&Config, &BoundaryError>,
    runtime_dirs: Option<&RuntimeDirs>,
    config_explicit: bool,
) -> DoctorCheck {
    match config_result {
        Ok(cfg) => DoctorCheck {
            id: "config".into(),
            category: "config",
            status: CheckStatus::Ok,
            summary: format!("valid config at {}", cfg.config_path.display()),
            detail: None,
        },
        Err(err) => {
            let path = runtime_dirs.map(|dirs| dirs.config_path.as_path());
            let missing_default = !config_explicit && path.is_some_and(|p| !p.exists());
            if missing_default {
                DoctorCheck {
                    id: "config".into(),
                    category: "config",
                    status: CheckStatus::Warn,
                    summary: format!(
                        "default config file does not exist yet at {}",
                        path.map(|p| p.display().to_string()).unwrap_or_default()
                    ),
                    detail: Some("run `soyo config init` or pass `--config PATH`".into()),
                }
            } else {
                DoctorCheck {
                    id: "config".into(),
                    category: "config",
                    status: CheckStatus::Fail,
                    summary: format!("configuration invalid ({})", err.0.code),
                    detail: Some(err.0.message.clone()),
                }
            }
        }
    }
}

fn profiles_check(config: &Config) -> DoctorCheck {
    let profile_count = config.profiles.len();
    let selected = config.profile.as_deref().unwrap_or("none");
    let routing_note = if config.routing.enabled {
        "routing enabled"
    } else {
        "routing disabled"
    };
    let status = if profile_count == 0 {
        CheckStatus::Warn
    } else {
        CheckStatus::Ok
    };
    DoctorCheck {
        id: "profiles".into(),
        category: "config",
        status,
        summary: format!(
            "{profile_count} profile(s) configured (active={selected}, {routing_note})"
        ),
        detail: (profile_count == 0).then_some(
            "no model profiles configured; model execution will require at least one profile"
                .into(),
        ),
    }
}

fn append_directory_checks(
    checks: &mut Vec<DoctorCheck>,
    config: Option<&Config>,
    runtime_dirs: Option<&RuntimeDirs>,
    env_pairs: &BTreeMap<String, String>,
) {
    let (config_dir, data_dir, state_dir, cache_dir) = if let Some(cfg) = config {
        (
            cfg.config_path.parent().map(Path::to_path_buf),
            Some(cfg.data_dir.clone()),
            Some(cfg.state_dir.clone()),
            Some(cfg.cache_dir.clone()),
        )
    } else if let Some(dirs) = runtime_dirs {
        (
            dirs.config_path.parent().map(Path::to_path_buf),
            Some(dirs.data_dir.clone()),
            Some(dirs.state_dir.clone()),
            None,
        )
    } else {
        (None, None, None, None)
    };

    for (id, path_opt, strict_mode) in [
        ("dir_config", config_dir, false),
        ("dir_data", data_dir, false),
        ("dir_state", state_dir.clone(), false),
        ("dir_cache", cache_dir, false),
        (
            "dir_credentials",
            state_dir.as_ref().map(|d| d.join("credentials")),
            true,
        ),
        (
            "dir_secrets",
            state_dir.as_ref().map(|d| d.join("secrets")),
            true,
        ),
    ] {
        if let Some(path) = path_opt {
            checks.push(inspect_dir(id, &path, strict_mode));
        }
    }

    if let Some(home) = env_pairs.get("HOME").filter(|h| !h.is_empty()) {
        let home_path = Path::new(home);
        let new_dir = home_path.join(".soyo");
        let legacy_dir = home_path.join(".saya");
        let mut status = CheckStatus::Ok;
        let summary;
        let mut detail = None;
        if new_dir.exists() || fs::symlink_metadata(&new_dir).is_ok() {
            let check = inspect_dir("home_soyo", &new_dir, false);
            status = check.status;
            summary = format!("primary home store at {}", new_dir.display());
            detail = check.detail;
        } else if legacy_dir.exists() || fs::symlink_metadata(&legacy_dir).is_ok() {
            let check = inspect_dir("home_saya", &legacy_dir, false);
            status = check.status;
            summary = format!(
                "legacy fallback store at {} (.soyo absent)",
                legacy_dir.display()
            );
            detail = check.detail;
        } else {
            summary = format!(
                "no ~/.soyo or ~/.saya store created yet under {}",
                home_path.display()
            );
        }
        checks.push(DoctorCheck {
            id: "home_store".into(),
            category: "storage",
            status,
            summary,
            detail,
        });
    }
}

fn inspect_dir(id: &str, path: &Path, require_0700: bool) -> DoctorCheck {
    match fs::symlink_metadata(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => DoctorCheck {
            id: id.into(),
            category: "storage",
            status: CheckStatus::Ok,
            summary: format!("{} is absent (created on demand with 0700)", path.display()),
            detail: None,
        },
        Err(err) => DoctorCheck {
            id: id.into(),
            category: "storage",
            status: CheckStatus::Fail,
            summary: format!("cannot inspect {}", path.display()),
            detail: Some(err.to_string()),
        },
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return DoctorCheck {
                    id: id.into(),
                    category: "storage",
                    status: CheckStatus::Fail,
                    summary: format!("{} must not be a symlink", path.display()),
                    detail: None,
                };
            }
            if !meta.is_dir() {
                return DoctorCheck {
                    id: id.into(),
                    category: "storage",
                    status: CheckStatus::Fail,
                    summary: format!("{} exists and is not a directory", path.display()),
                    detail: None,
                };
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let mode = meta.permissions().mode() & 0o777;
                // SAFETY: geteuid takes no arguments and has no side effects.
                let euid = unsafe { libc::geteuid() };
                if meta.uid() != euid {
                    return DoctorCheck {
                        id: id.into(),
                        category: "storage",
                        status: CheckStatus::Fail,
                        summary: format!("{} is not owned by the current user", path.display()),
                        detail: Some(format!("owner uid={} expected euid={euid}", meta.uid())),
                    };
                }
                if require_0700 && (mode & 0o077 != 0) {
                    return DoctorCheck {
                        id: id.into(),
                        category: "storage",
                        status: CheckStatus::Fail,
                        summary: format!(
                            "{} permissions {mode:04o} are too permissive (expected 0700)",
                            path.display()
                        ),
                        detail: Some("restrict permissions with chmod 0700".into()),
                    };
                }
                if mode & 0o022 != 0 {
                    return DoctorCheck {
                        id: id.into(),
                        category: "storage",
                        status: CheckStatus::Fail,
                        summary: format!(
                            "{} permissions {mode:04o} allow group/world writes",
                            path.display()
                        ),
                        detail: Some("restrict permissions with chmod 0700".into()),
                    };
                }
                DoctorCheck {
                    id: id.into(),
                    category: "storage",
                    status: CheckStatus::Ok,
                    summary: format!("{} exists (mode {mode:04o}, owner ok)", path.display()),
                    detail: None,
                }
            }
            #[cfg(not(unix))]
            DoctorCheck {
                id: id.into(),
                category: "storage",
                status: CheckStatus::Ok,
                summary: format!(
                    "{} exists (non-POSIX ACL check unimplemented)",
                    path.display()
                ),
                detail: None,
            }
        }
    }
}

fn find_executable(program: &str, env_pairs: &BTreeMap<String, String>) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if candidate.is_absolute() || program.contains('/') {
        return is_executable_file(candidate).then(|| candidate.to_path_buf());
    }
    let path_var = env_pairs.get("PATH")?;
    for dir in std::env::split_paths(path_var) {
        let joined = dir.join(program);
        if is_executable_file(&joined) {
            return Some(joined);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn git_check(env_pairs: &BTreeMap<String, String>) -> DoctorCheck {
    let Some(git_bin) = find_executable("git", env_pairs) else {
        return DoctorCheck {
            id: "git".into(),
            category: "tooling",
            status: CheckStatus::Warn,
            summary: "git executable not found on PATH".into(),
            detail: Some("subagent git worktree isolation will fall back to directory copy".into()),
        };
    };
    let mut cmd = Command::new(&git_bin);
    cmd.env_clear();
    if let Some(path) = env_pairs.get("PATH") {
        cmd.env("PATH", path);
    }
    match cmd.arg("--version").output() {
        Ok(out) if out.status.success() => {
            let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            DoctorCheck {
                id: "git".into(),
                category: "tooling",
                status: CheckStatus::Ok,
                summary: format!("{line} ({})", git_bin.display()),
                detail: None,
            }
        }
        _ => DoctorCheck {
            id: "git".into(),
            category: "tooling",
            status: CheckStatus::Warn,
            summary: format!("git executable failed at {}", git_bin.display()),
            detail: None,
        },
    }
}

fn js_repl_check(config: &Config, env_pairs: &BTreeMap<String, String>) -> DoctorCheck {
    if !config.js_repl.enabled {
        return DoctorCheck {
            id: "js_repl".into(),
            category: "optional",
            status: CheckStatus::Ok,
            summary: "js_repl is disabled by default (opt-in F-21 QuickJS worker ready)".into(),
            detail: None,
        };
    }
    match config.js_repl.resolve() {
        Ok(Some(_)) => {
            if let Some(node_path) = &config.js_repl.node.path
                && find_executable(&node_path.to_string_lossy(), env_pairs).is_none()
            {
                return DoctorCheck {
                    id: "js_repl".into(),
                    category: "optional",
                    status: CheckStatus::Fail,
                    summary: format!(
                        "configured js_repl node binary is not executable: {}",
                        node_path.display()
                    ),
                    detail: None,
                };
            }
            DoctorCheck {
                id: "js_repl".into(),
                category: "optional",
                status: CheckStatus::Ok,
                summary: "js_repl enabled and configuration validated".into(),
                detail: None,
            }
        }
        Ok(None) => DoctorCheck {
            id: "js_repl".into(),
            category: "optional",
            status: CheckStatus::Ok,
            summary: "js_repl disabled".into(),
            detail: None,
        },
        Err(err) => DoctorCheck {
            id: "js_repl".into(),
            category: "optional",
            status: CheckStatus::Fail,
            summary: format!("js_repl config invalid: {}", err.0.message),
            detail: None,
        },
    }
}

fn code_intel_check(config: &Config, env_pairs: &BTreeMap<String, String>) -> DoctorCheck {
    if !config.code_intel.enabled {
        return DoctorCheck {
            id: "code_intel".into(),
            category: "optional",
            status: CheckStatus::Ok,
            summary: "code_intel is disabled by default (opt-in F-17 index/LSP)".into(),
            detail: None,
        };
    }
    if !config.code_intel.lsp.enabled {
        return DoctorCheck {
            id: "code_intel".into(),
            category: "optional",
            status: CheckStatus::Ok,
            summary: "code_intel Layer 1 SQLite symbol index enabled; LSP Layer 2 disabled".into(),
            detail: None,
        };
    }
    let mut missing = Vec::new();
    let mut active = 0usize;
    for (id, server) in &config.code_intel.lsp.servers {
        if !server.enabled {
            continue;
        }
        active += 1;
        let Some(program) = server.argv.first() else {
            missing.push(format!("{id} (empty argv)"));
            continue;
        };
        let resolved = find_executable(program, env_pairs);
        let usable = match resolved {
            None => false,
            Some(ref path) if program.ends_with("rust-analyzer") => {
                // Verify rust-analyzer is actually runnable under active rustup toolchain.
                let mut cmd = Command::new(path);
                cmd.env_clear();
                for key in [
                    "PATH",
                    "HOME",
                    "RUSTUP_HOME",
                    "CARGO_HOME",
                    "RUSTUP_TOOLCHAIN",
                ] {
                    if let Some(val) = env_pairs.get(key) {
                        cmd.env(key, val);
                    }
                }
                cmd.arg("--version")
                    .output()
                    .is_ok_and(|out| out.status.success())
            }
            Some(_) => true,
        };
        if !usable {
            missing.push(format!("{id} ({program})"));
        }
    }
    if missing.is_empty() {
        DoctorCheck {
            id: "code_intel".into(),
            category: "optional",
            status: CheckStatus::Ok,
            summary: format!("code_intel + LSP enabled ({active} configured server(s) runnable)"),
            detail: None,
        }
    } else {
        DoctorCheck {
            id: "code_intel".into(),
            category: "optional",
            status: CheckStatus::Warn,
            summary: format!(
                "code_intel LSP enabled, unavailable server(s): {}",
                missing.join(", ")
            ),
            detail: Some(
                "ensure configured LSP binaries exist and run `rustup component add rust-analyzer` if using rust-analyzer".into(),
            ),
        }
    }
}

fn extensions_check(config: &Config) -> DoctorCheck {
    let mcp_enabled = config
        .mcp
        .servers
        .values()
        .filter(|server| server.enabled)
        .count();
    let host_hooks_note = if config.hooks.enabled {
        format!("enabled ({} rule(s))", config.hooks.rules.len())
    } else {
        "disabled".into()
    };
    let plugins_note = if config.plugins.enabled {
        "enabled"
    } else {
        "disabled"
    };
    DoctorCheck {
        id: "extensions".into(),
        category: "extensions",
        status: CheckStatus::Ok,
        summary: format!(
            "mcp={mcp_enabled} server(s), plugins={plugins_note}, host_hooks={host_hooks_note}"
        ),
        detail: None,
    }
}

fn platform_support_matrix() -> Vec<PlatformSupportEntry> {
    vec![
        PlatformSupportEntry {
            target: "macos-aarch64",
            status: "supported",
            scope: "Seatbelt sandbox (process cleanup best_effort); CLI/TUI/Desktop; Colima VM browser",
        },
        PlatformSupportEntry {
            target: "macos-x86_64",
            status: "supported",
            scope: "Seatbelt sandbox (process cleanup best_effort); CLI/TUI/Desktop",
        },
        PlatformSupportEntry {
            target: "linux-x86_64",
            status: "supported",
            scope: "Bubblewrap + Seccomp + socat sandbox; CLI/TUI/Desktop; headless browser worker (no prebuilt masked headless_shell)",
        },
        PlatformSupportEntry {
            target: "linux-aarch64",
            status: "supported",
            scope: "Bubblewrap + Seccomp + socat sandbox; CLI/TUI/Desktop; linux-arm64 custom masked headless_shell target",
        },
        PlatformSupportEntry {
            target: "windows",
            status: "unsupported",
            scope: "Blocker (F-01/F-15): native AppContainer/Job sandbox and POSIX 0700 private-dir DACL parity unimplemented",
        },
    ]
}

fn render_text(payload: &Value) -> String {
    let data = &payload["data"];
    let mut lines = Vec::new();
    let version = data["version"].as_str().unwrap_or("unknown");
    let os = data["os"].as_str().unwrap_or("unknown");
    let arch = data["arch"].as_str().unwrap_or("unknown");
    let strict = data["strict"].as_bool().unwrap_or(false);
    let passed = data["summary"]["passed"].as_bool().unwrap_or(false);
    lines.push(format!(
        "Soyo Doctor v{version} ({os}/{arch}, strict={strict}) — {}",
        if passed { "PASS" } else { "ISSUES FOUND" }
    ));

    if let Some(checks) = data["checks"].as_array() {
        lines.push(String::new());
        lines.push("Checks:".into());
        for check in checks {
            let status = match check["status"].as_str().unwrap_or("warn") {
                "ok" => CheckStatus::Ok,
                "warn" => CheckStatus::Warn,
                "fail" => CheckStatus::Fail,
                "unsupported" => CheckStatus::Unsupported,
                _ => CheckStatus::Warn,
            };
            let id = check["id"].as_str().unwrap_or("check");
            let summary = check["summary"].as_str().unwrap_or("");
            lines.push(format!("  [{:<11}] {id}: {summary}", status.label()));
            if let Some(detail) = check["detail"].as_str().filter(|d| !d.is_empty()) {
                lines.push(format!("                detail: {detail}"));
            }
        }
    }

    if let Some(matrix) = data["platform_matrix"].as_array() {
        lines.push(String::new());
        lines.push("Platform Support Matrix:".into());
        for row in matrix {
            let target = row["target"].as_str().unwrap_or("");
            let status = row["status"].as_str().unwrap_or("");
            let scope = row["scope"].as_str().unwrap_or("");
            lines.push(format!("  {target:<14} [{status:<11}] {scope}"));
        }
    }

    lines.join("\n")
}
