//! Offline CLI integration tests for `soyo doctor` (`F-15`).

use serde_json::Value;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::{Command, Stdio},
};
use tempfile::TempDir;

struct DoctorSandbox {
    workspace: TempDir,
    home: TempDir,
    config: std::path::PathBuf,
}

impl DoctorSandbox {
    fn new(extra_toml: &str) -> Self {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_dir = home.path().join("cfg");
        fs::create_dir_all(&config_dir).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let config = config_dir.join("config.toml");
        fs::write(
            &config,
            format!(
                r#"schema_version = 1
[defaults]
profile = "primary"
permissions = "auto"

[profiles.primary]
protocol = "openai-responses"
base_url = "https://127.0.0.1:9/v1"
model = "gpt-5.4"
auth = "bearer"
api_key_env = "DOCTOR_TEST_SECRET"
context_window_tokens = 128000
max_output_tokens = 8192

{extra_toml}
"#
            ),
        )
        .unwrap();
        Self {
            workspace,
            home,
            config,
        }
    }

    fn command(&self, include_config: bool) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_soyo"));
        cmd.env_clear()
            .env("HOME", self.home.path())
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env("XDG_STATE_HOME", self.home.path().join("state"))
            .env("XDG_CACHE_HOME", self.home.path().join("cache"))
            .env("XDG_CONFIG_HOME", self.home.path().join("xdg-config"))
            .env("DOCTOR_TEST_SECRET", "sk-doctor-must-never-leak-999")
            .current_dir(self.workspace.path())
            .stdin(Stdio::null());
        if include_config {
            cmd.arg("--config").arg(&self.config);
        }
        cmd
    }
}

fn find_check<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["data"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == id)
        .unwrap_or_else(|| panic!("missing doctor check '{id}' in {report}"))
}

#[test]
fn doctor_json_and_strict_succeed_on_clean_macos_host_without_leaking_secrets_or_creating_dirs() {
    let sb = DoctorSandbox::new("");

    let json_out = sb
        .command(true)
        .args(["--output", "json", "doctor", "--strict"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&json_out.stdout);
    let stderr = String::from_utf8_lossy(&json_out.stderr);
    assert!(
        json_out.status.success(),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(!stdout.contains("sk-doctor-must-never-leak-999"));
    assert!(!stderr.contains("sk-doctor-must-never-leak-999"));

    let parsed: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["kind"], "doctor");
    assert_eq!(parsed["data"]["summary"]["passed"], true);
    assert_eq!(find_check(&parsed, "config")["status"], "ok");
    assert_eq!(find_check(&parsed, "profiles")["status"], "ok");
    #[cfg(target_os = "macos")]
    assert_eq!(find_check(&parsed, "native_sandbox")["status"], "ok");

    let matrix = parsed["data"]["platform_matrix"].as_array().unwrap();
    assert!(
        matrix
            .iter()
            .any(|row| row["target"] == "windows" && row["status"] == "unsupported")
    );

    // Offline read-only guarantee: doctor must not create absent storage directories.
    assert!(!sb.home.path().join("state").exists());
    assert!(!sb.home.path().join("data").exists());

    // Text mode also succeeds cleanly and omits secrets.
    let text_out = sb.command(true).args(["doctor"]).output().unwrap();
    let text_stdout = String::from_utf8_lossy(&text_out.stdout);
    assert!(text_out.status.success(), "stdout={text_stdout}");
    assert!(text_stdout.contains("Platform Support Matrix:"));
    assert!(text_stdout.contains("windows"));
    assert!(!text_stdout.contains("sk-doctor-must-never-leak-999"));
}

#[test]
fn doctor_handles_missing_default_config_and_enforces_strict_exit_code() {
    let sb = DoctorSandbox::new("");

    let relaxed = sb
        .command(false)
        .args(["--output", "json", "doctor"])
        .output()
        .unwrap();
    assert!(
        relaxed.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&relaxed.stderr)
    );
    let parsed: Value = serde_json::from_slice(&relaxed.stdout).unwrap();
    assert_eq!(find_check(&parsed, "config")["status"], "warn");
    assert_eq!(parsed["data"]["summary"]["passed"], true);

    let strict = sb
        .command(false)
        .args(["--output", "json", "doctor", "--strict"])
        .output()
        .unwrap();
    assert_eq!(strict.status.code(), Some(1));
    let strict_parsed: Value = serde_json::from_slice(&strict.stdout).unwrap();
    assert_eq!(strict_parsed["data"]["summary"]["passed"], false);
}

#[test]
fn doctor_fails_on_permissive_or_symlinked_private_credentials_dir() {
    let sb = DoctorSandbox::new("");
    let state_soyo = sb.home.path().join("state/soyo");
    let creds = state_soyo.join("credentials");
    fs::create_dir_all(&creds).unwrap();
    fs::set_permissions(&state_soyo, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&creds, fs::Permissions::from_mode(0o777)).unwrap();

    let permissive_out = sb
        .command(true)
        .args(["--output", "json", "doctor"])
        .output()
        .unwrap();
    assert_eq!(permissive_out.status.code(), Some(1));
    let parsed: Value = serde_json::from_slice(&permissive_out.stdout).unwrap();
    assert_eq!(find_check(&parsed, "dir_credentials")["status"], "fail");

    // Symlink credentials dir must also fail.
    fs::remove_dir(&creds).unwrap();
    let decoy = sb.home.path().join("decoy");
    fs::create_dir(&decoy).unwrap();
    symlink(Path::new(&decoy), &creds).unwrap();

    let symlink_out = sb
        .command(true)
        .args(["--output", "json", "doctor"])
        .output()
        .unwrap();
    assert_eq!(symlink_out.status.code(), Some(1));
    let symlink_parsed: Value = serde_json::from_slice(&symlink_out.stdout).unwrap();
    assert_eq!(
        find_check(&symlink_parsed, "dir_credentials")["status"],
        "fail"
    );
}

#[test]
fn doctor_warns_when_enabled_lsp_binary_is_missing_and_fails_only_under_strict() {
    let sb = DoctorSandbox::new(
        r#"[code_intel]
enabled = true
[code_intel.lsp]
enabled = true
[code_intel.lsp.servers.rust]
enabled = true
argv = ["definitely-missing-rust-analyzer-bin"]
languages = ["rust"]
extensions = ["rs"]
"#,
    );

    let normal = sb
        .command(true)
        .args(["--output", "json", "doctor"])
        .output()
        .unwrap();
    assert!(normal.status.success());
    let parsed: Value = serde_json::from_slice(&normal.stdout).unwrap();
    assert_eq!(find_check(&parsed, "code_intel")["status"], "warn");

    let strict = sb
        .command(true)
        .args(["--output", "json", "doctor", "--strict"])
        .output()
        .unwrap();
    assert_eq!(strict.status.code(), Some(1));
}
