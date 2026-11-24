//! End-to-end socket round-trip tests: a real `HelperServer` on a temp
//! `SOYO_HELPER_DIR`, a real `HelperClient`, real NDJSON frames.

#[cfg(test)]
mod tests {
    use crate::server::HelperServer;
    use crate::service::HostService;
    use serde_json::json;
    use soyo_ipc::{HelperClient, helper_pid_path};
    use std::sync::OnceLock;
    use tokio::sync::Mutex;

    /// `SOYO_HELPER_DIR` is process-global; serialize socket tests.
    static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

    async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
        SERIAL.get_or_init(|| Mutex::new(())).lock().await
    }

    fn use_temp_helper_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("helper");
        // SAFETY: tests in this crate set SOYO_HELPER_DIR before any
        // server/client reads it; runs are serialized by `serial()`.
        unsafe {
            std::env::set_var("SOYO_HELPER_DIR", &helper);
        }
        dir
    }

    async fn running_server() -> (tempfile::TempDir, tokio::task::JoinHandle<()>) {
        let dir = use_temp_helper_dir();
        let server = HelperServer::new(HostService::from_environment());
        let handle = tokio::spawn(async move {
            if let Err(err) = server.run().await {
                eprintln!("test helper server error: {}: {}", err.code, err.message);
            }
        });
        // Wait for the socket to accept.
        for _ in 0..100 {
            if HelperClient::connect().await.is_ok() {
                return (dir, handle);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("helper server did not start listening");
    }

    #[tokio::test]
    async fn client_call_round_trip_and_shutdown() {
        let _guard = serial().await;
        let (_dir, handle) = running_server().await;
        let client = HelperClient::connect().await.expect("connect");
        let bootstrap: serde_json::Value = client
            .call_typed("get_bootstrap_state", &json!({}))
            .await
            .expect("bootstrap call");
        assert!(bootstrap.get("version").is_some());
        assert!(bootstrap.get("config_ready").is_some());

        // Unknown method is a clean error frame, not a crash.
        let err = client.call("no_such_method", json!({})).await.unwrap_err();
        assert_eq!(err.code, "unknown_method");

        client.shutdown_daemon().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        // Socket must stop accepting after shutdown.
        assert!(HelperClient::connect().await.is_err());
        assert!(!helper_pid_path().exists());
    }

    fn free_tcp_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// D-113 path (a): the same NDJSON wire contract over loopback TCP — the
    /// transport an `ssh -L` forward delivers traffic over. Published methods
    /// must behave byte-identically to the Unix socket path.
    #[tokio::test]
    async fn tcp_loopback_round_trip_and_shutdown() {
        let _guard = serial().await;
        let _dir = use_temp_helper_dir();
        let port = free_tcp_port();
        let listen = format!("127.0.0.1:{port}");
        let _env = EnvGuard::set(&[("SOYO_HELPER_LISTEN", &format!("tcp:{listen}"))]);
        let server = HelperServer::new(HostService::from_environment());
        let handle = tokio::spawn(async move {
            if let Err(err) = server.run().await {
                eprintln!("test helper server error: {}: {}", err.code, err.message);
            }
        });
        // Wait for the TCP listener to accept.
        let mut client = None;
        for _ in 0..100 {
            if let Ok(c) = HelperClient::connect().await {
                client = Some(c);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let client = client.expect("tcp helper server did not start listening");
        // Both sides resolved the same `tcp:` endpoint (no Unix socket involved).
        assert!(matches!(
            soyo_ipc::HelperTarget::from_environment()
                .unwrap()
                .listen_endpoint()
                .unwrap(),
            soyo_ipc::ListenEndpoint::Tcp(_)
        ));

        // Identity + bootstrap over TCP.
        let info: serde_json::Value = client
            .call_typed("helper_info", &json!({}))
            .await
            .expect("helper_info over tcp");
        assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));

        let bootstrap: serde_json::Value = client
            .call_typed("get_bootstrap_state", &json!({}))
            .await
            .expect("bootstrap over tcp");
        assert!(bootstrap.get("config_ready").is_some());

        // A second method round-trips too (desktop prefs are config-independent,
        // so they work in this fixture).
        let prefs: serde_json::Value = client
            .call_typed("get_desktop_prefs", &json!({}))
            .await
            .expect("get_desktop_prefs over tcp");
        assert!(prefs.is_object(), "{prefs}");

        // Unknown methods still fail as a clean error frame, not a hang.
        let err = client.call("no_such_method", json!({})).await.unwrap_err();
        assert_eq!(err.code, "unknown_method");

        client.shutdown_daemon().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        // The listener must stop accepting after shutdown.
        assert!(HelperClient::connect().await.is_err());
    }

    /// A `tcp:` endpoint only dials: `connect_or_spawn_for` must never run
    /// `spawn_helper_process` (nothing to spawn on the far side of a tunnel).
    #[tokio::test]
    async fn tcp_endpoint_does_not_spawn() {
        let _guard = serial().await;
        let dir = use_temp_helper_dir();
        let marker = dir.path().join("spawned");
        let script = dir.path().join("fake-soyo");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let port = free_tcp_port();
        let _env = EnvGuard::set(&[
            ("SOYO_HELPER_LISTEN", &format!("tcp:127.0.0.1:{port}")),
            ("SOYO_HELPER_BIN", script.to_str().unwrap()),
        ]);

        let target = soyo_ipc::HelperTarget::from_environment().unwrap();
        let err = match HelperClient::connect_or_spawn_for(&target).await {
            Ok(_) => panic!("nothing should listen on the chosen port"),
            Err(err) => err,
        };
        assert_eq!(err.code, "helper_unavailable", "{}", err.message);
        // Give any (incorrectly) spawned process time to touch the marker.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !marker.exists(),
            "tcp endpoints must never spawn a local helper"
        );
    }

    #[tokio::test]
    async fn stale_socket_is_reclaimed_on_bind() {
        let _guard = serial().await;
        let _dir = use_temp_helper_dir();
        let target = soyo_ipc::HelperTarget::from_environment().unwrap();
        soyo_fs::ensure_private_dir(&target.helper_dir(), "helper", "helper").unwrap();
        std::fs::write(target.socket_path(), b"stale").unwrap();
        let (_dir, handle) = running_server().await;
        let client = HelperClient::connect().await.expect("connect");
        client.shutdown_daemon().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn server_exits_when_socket_file_removed() {
        let _guard = serial().await;
        let (_dir, handle) = running_server().await;
        let target = soyo_ipc::HelperTarget::from_environment().unwrap();
        let sock = target.socket_path();
        assert!(sock.exists());
        // Simulate tempdir/cleanup removal: the helper is unreachable forever.
        std::fs::remove_file(&sock).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(
            result.is_ok(),
            "helper server should exit after its socket path is removed"
        );
    }

    #[tokio::test]
    async fn helper_info_reports_process_identity_and_compat() {
        let _guard = serial().await;
        let (_dir, handle) = running_server().await;
        let client = HelperClient::connect().await.expect("connect");

        let info: serde_json::Value = client
            .call_typed("helper_info", &json!({}))
            .await
            .expect("helper_info call");
        assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
        assert!(info["pid"].as_u64().is_some_and(|pid| pid > 0));
        assert!(!info["exe_path"].as_str().unwrap_or_default().is_empty());
        assert_eq!(info["run_active"], false);

        // Same workspace build, indeterminate exe comparison → no mismatch.
        let compat = client.helper_compat().await;
        assert!(!compat.mismatch, "{}", compat.detail);

        client.shutdown_daemon().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("fixtures/remote-tls")
            .join(name)
    }

    fn fixture_cert() -> std::path::PathBuf {
        fixture("ca.pem")
    }

    fn remote_config(port: u16) -> String {
        format!(
            "schema_version = 1\n[remote]\nenabled = true\nbind = \"127.0.0.1:{port}\"\n\
             token_ttl_days = 30\n[remote.tls]\ncert_path = \"{}\"\nkey_path = \"{}\"\n",
            fixture("chain.pem").display(),
            fixture("server-key.pem").display()
        )
    }

    /// D-113 path (b): the same NDJSON wire contract over authenticated
    /// HTTPS loopback, using the fixture CA. Covers accept and reject
    /// (missing/unknown/revoked/expired token) plus the audit trail.
    ///
    /// Configuration is built from a synthetic `Environment` and the token is
    /// passed to `connect_remote` explicitly. The only process-global value
    /// touched is `SOYO_HELPER_DIR`: sibling socket tests leave it pointing at
    /// their deleted temp directories, so it is cleared (and restored on drop)
    /// rather than inherited.
    #[tokio::test]
    async fn remote_https_round_trip_requires_a_valid_bearer_token() {
        use soyo_core::config::{self, Environment, Overrides};
        use soyo_core::credentials::{
            CredentialBackend, CredentialGrant, CredentialStore, GrantRunKind, GrantStore,
            SecretRegistry,
        };
        use std::ffi::OsString;

        let _guard = serial().await;
        let _helper_dir_env = EnvGuard::set(&[("SOYO_HELPER_DIR", "")]);
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let data_dir = dir.path().join("data");
        let port = free_tcp_port();
        std::fs::write(&config_path, remote_config(port)).unwrap();
        let env = Environment::from_pairs([
            ("HOME".into(), OsString::from(dir.path())),
            (
                "XDG_STATE_HOME".into(),
                OsString::from(dir.path().join("state")),
            ),
            (
                "XDG_DATA_HOME".into(),
                OsString::from(dir.path().join("data-xdg")),
            ),
            (
                "XDG_CACHE_HOME".into(),
                OsString::from(dir.path().join("cache")),
            ),
        ]);
        let cfg = config::load(
            Overrides {
                config: Some(config_path.clone()),
                data_dir: Some(data_dir.clone()),
                ..Default::default()
            },
            env.clone(),
            dir.path(),
        )
        .expect("fixture config must load");
        assert!(cfg.remote.enabled);
        let state_dir = cfg.state_dir.clone();
        let target = soyo_ipc::HelperTarget::from_config(&cfg);
        let server = HelperServer::new(HostService::with_config(cfg));
        let (error_tx, mut error_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let handle = tokio::spawn(async move {
            if let Err(err) = server.run().await {
                let _ = error_tx.send(format!("{}: {}", err.code, err.message));
            }
        });
        // `start_remote` binds before the local loop, so a reachable local
        // socket proves the remote listener is up too. The suite runs many
        // tests in parallel, so allow for scheduling delays and surface an
        // early daemon failure instead of timing out opaquely.
        let mut local = None;
        for _ in 0..500 {
            if let Ok(client) = HelperClient::connect_for(&target).await {
                local = Some(client);
                break;
            }
            match error_rx.try_recv() {
                Ok(message) => panic!("helper failed to start: {message}"),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    panic!("helper task exited before binding")
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let local = local.expect("helper did not start listening locally");

        let store = CredentialStore::open_backend(
            CredentialBackend::File,
            &state_dir,
            std::sync::Arc::new(SecretRegistry::default()),
        )
        .unwrap();
        let endpoint_url = format!("https://127.0.0.1:{port}");
        let endpoint = soyo_ipc::RemoteEndpoint::parse(&endpoint_url).unwrap();
        let ca = fixture_cert();
        let issued = store.issue_remote_token(&endpoint_url, 30).await.unwrap();
        let token = issued.token.expose().to_owned();

        let river = |token: &str| {
            let endpoint = endpoint.clone();
            let token = token.to_owned();
            let ca = ca.clone();
            async move { soyo_ipc::remote::connect_remote(&endpoint, &token, Some(ca.as_path())).await }
        };
        let stream = river(&token).await.expect("remote https connect");
        let client = HelperClient::from_stream(stream)
            .await
            .expect("protocol handshake over https");
        let info: serde_json::Value = client
            .call_typed("helper_info", &json!({}))
            .await
            .expect("helper_info over https");
        assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
        let bootstrap: serde_json::Value = client
            .call_typed("get_bootstrap_state", &json!({}))
            .await
            .expect("bootstrap over https");
        assert_eq!(bootstrap["config_ready"], true);
        let prefs: serde_json::Value = client
            .call_typed("get_desktop_prefs", &json!({}))
            .await
            .expect("desktop prefs over https");
        assert!(prefs.is_object(), "{prefs}");
        let err = client.call("no_such_method", json!({})).await.unwrap_err();
        assert_eq!(err.code, "unknown_method");

        // A remote endpoint without a configured token is a configuration
        // error before any network use (`connect_for`'s remote arm).
        {
            let missing_config = dir.path().join("missing-token.toml");
            std::fs::write(
                &missing_config,
                format!(
                    "{}\n[remote.client]\nendpoint = \"{endpoint_url}\"\n\
                     token_env = \"SOYO_F13B_UNSET_TOKEN\"\n",
                    remote_config(port)
                ),
            )
            .unwrap();
            let missing_target = soyo_ipc::HelperTarget::from_overrides(
                Some(&missing_config),
                Some(&data_dir),
                &env,
                dir.path(),
            )
            .unwrap();
            let err = HelperClient::connect_for(&missing_target)
                .await
                .err()
                .expect("missing token");
            assert_eq!(err.code, "invalid_config", "{}", err.message);
        }
        // Unknown token: rejected by the helper.
        {
            let err = river(&format!("{token}-wrong"))
                .await
                .err()
                .expect("wrong token");
            assert_eq!(err.code, "helper_unauthorized", "{}", err.message);
        }
        // Expired grant: digest still matches, grant does not apply.
        let expiring = store.issue_remote_token(&endpoint_url, 1).await.unwrap();
        {
            let grants = GrantStore::open(store.protected_path()).unwrap();
            grants.remove(expiring.grant_id).unwrap();
            grants
                .put(&CredentialGrant {
                    grant_id: uuid::Uuid::now_v7(),
                    reference: expiring.reference,
                    workspaces: Default::default(),
                    run_kinds: [GrantRunKind::Remote].into_iter().collect(),
                    tools: Default::default(),
                    expires_at: Some("2000-01-01T00:00:00Z".into()),
                    server_id: "remote".into(),
                })
                .unwrap();
            let err = river(expiring.token.expose())
                .await
                .err()
                .expect("expired token");
            assert_eq!(err.code, "helper_unauthorized", "{}", err.message);
        }
        // Revoked token: tombstone + material removed, denied like an unknown one.
        store.revoke(issued.reference, &issued.scope).await.unwrap();
        {
            let err = river(&token).await.err().expect("revoked token");
            assert_eq!(err.code, "helper_unauthorized", "{}", err.message);
        }
        // The audit trail lists accepted and denied attempts, never the token.
        let audit = std::fs::read_to_string(store.protected_path().join("audit.log")).unwrap();
        assert!(audit.contains("remote_auth_ok"), "{audit}");
        assert!(audit.contains("remote_auth_denied"), "{audit}");
        assert!(!audit.contains(&token), "{audit}");

        // Shutdown travels over the remote transport like any other frame.
        client.shutdown_daemon().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(HelperClient::connect_for(&target).await.is_err());
        drop(local);
    }

    fn remote_worker_config(port: u16) -> String {
        format!(
            "schema_version = 1\n\
             [defaults]\nprofile = \"test\"\nmode = \"work\"\npermissions = \"auto\"\n\
             [profiles.test]\nprotocol = \"openai-chat-completions\"\n\
             base_url = \"http://127.0.0.1:9/v1\"\nmodel = \"fixture\"\nauth = \"none\"\n\
             context_window_tokens = 64000\nstream = false\n\
             [tasks]\nenabled = true\n\
             [remote]\nenabled = true\nbind = \"127.0.0.1:{port}\"\ntoken_ttl_days = 30\n\
             [remote.tls]\ncert_path = \"{}\"\nkey_path = \"{}\"\n",
            fixture("chain.pem").display(),
            fixture("server-key.pem").display()
        )
    }

    async fn upload_remote_blob(client: &HelperClient, bytes: &[u8]) -> String {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let sha = soyo_core::remote::sha256_hex(bytes);
        let _: serde_json::Value = client
            .call_typed(
                "remote_blob_begin",
                &json!({"sha256": sha, "total_bytes": bytes.len()}),
            )
            .await
            .expect("blob begin");
        let next: u64 = client
            .call_typed(
                "remote_blob_put",
                &json!({"sha256": sha, "offset": 0, "data_b64": STANDARD.encode(bytes)}),
            )
            .await
            .expect("blob put");
        assert_eq!(next, bytes.len() as u64);
        let _: serde_json::Value = client
            .call_typed(
                "remote_blob_finish",
                &json!({"sha256": sha, "total_bytes": bytes.len()}),
            )
            .await
            .expect("blob finish");
        sha
    }

    async fn fetch_remote_blob(client: &HelperClient, sha: &str) -> Vec<u8> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let chunk: serde_json::Value = client
            .call_typed("remote_blob_get", &json!({"sha256": sha, "offset": 0}))
            .await
            .expect("blob get");
        STANDARD
            .decode(chunk["data_b64"].as_str().expect("data_b64"))
            .expect("decode blob")
    }

    /// D-113 path (c): a remote worker registers over the authenticated HTTPS
    /// transport, claims an assigned task, syncs workspace inputs as blobs,
    /// reports a result, and the owning side materializes it only through the
    /// explicit three-way apply. Also covers divergence refusal and the
    /// never-auto-replay lease rules.
    #[tokio::test]
    async fn remote_worker_dispatch_sync_apply_and_lease_rules() {
        use soyo_core::config::{self, Environment, Overrides};
        use soyo_core::credentials::{CredentialBackend, CredentialStore, SecretRegistry};
        use std::ffi::OsString;

        let _guard = serial().await;
        let _helper_dir_env = EnvGuard::set(&[("SOYO_HELPER_DIR", "")]);
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("notes.txt"), b"base\n").unwrap();
        let config_path = dir.path().join("config.toml");
        let data_dir = dir.path().join("data");
        let port = free_tcp_port();
        std::fs::write(&config_path, remote_worker_config(port)).unwrap();
        let env = Environment::from_pairs([
            ("HOME".into(), OsString::from(dir.path())),
            (
                "XDG_STATE_HOME".into(),
                OsString::from(dir.path().join("state")),
            ),
            (
                "XDG_DATA_HOME".into(),
                OsString::from(dir.path().join("data-xdg")),
            ),
            (
                "XDG_CACHE_HOME".into(),
                OsString::from(dir.path().join("cache")),
            ),
        ]);
        let cfg = config::load(
            Overrides {
                config: Some(config_path.clone()),
                data_dir: Some(data_dir.clone()),
                ..Default::default()
            },
            env,
            &workspace,
        )
        .expect("fixture config must load");
        let state_dir = cfg.state_dir.clone();
        let target = soyo_ipc::HelperTarget::from_config(&cfg);
        let server = HelperServer::new(HostService::with_config(cfg));
        let (error_tx, mut error_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let handle = tokio::spawn(async move {
            if let Err(err) = server.run().await {
                let _ = error_tx.send(format!("{}: {}", err.code, err.message));
            }
        });
        let mut local = None;
        for _ in 0..500 {
            if let Ok(client) = HelperClient::connect_for(&target).await {
                local = Some(client);
                break;
            }
            match error_rx.try_recv() {
                Ok(message) => panic!("helper failed to start: {message}"),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    panic!("helper task exited before binding")
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let local = local.expect("helper did not start listening locally");

        let store = CredentialStore::open_backend(
            CredentialBackend::File,
            &state_dir,
            std::sync::Arc::new(SecretRegistry::default()),
        )
        .unwrap();
        let endpoint_url = format!("https://127.0.0.1:{port}");
        let endpoint = soyo_ipc::RemoteEndpoint::parse(&endpoint_url).unwrap();
        let issued = store.issue_remote_token(&endpoint_url, 30).await.unwrap();
        let stream = soyo_ipc::remote::connect_remote(
            &endpoint,
            issued.token.expose(),
            Some(fixture_cert().as_path()),
        )
        .await
        .expect("remote worker transport");
        let worker = HelperClient::from_stream(stream)
            .await
            .expect("worker handshake over https");

        // Malformed registration fails closed; a valid one is listed.
        let bad = json!({
            "schema_version": 1,
            "worker_id": "bad id",
            "arch": "aarch64",
            "sandbox_backends": ["seatbelt"],
            "version": "0.3.0"
        });
        let err = worker
            .call_typed::<_, serde_json::Value>("remote_worker_register", &bad)
            .await
            .expect_err("bad worker id");
        assert_eq!(err.code, "invalid_args", "{}", err.message);
        let info: serde_json::Value = worker
            .call_typed(
                "remote_worker_register",
                &json!({
                    "schema_version": 1,
                    "worker_id": "worker-1",
                    "arch": "aarch64",
                    "sandbox_backends": ["seatbelt"],
                    "version": "0.3.0"
                }),
            )
            .await
            .expect("register worker");
        assert_eq!(info["worker_id"], "worker-1");
        let workers: Vec<serde_json::Value> = worker
            .call_typed("remote_workers", &json!({}))
            .await
            .unwrap();
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0]["arch"], "aarch64");

        // Submit, assign, claim, and check the content-addressed input.
        let task: serde_json::Value = local
            .call_typed("task_submit", &json!({"input": "remote task"}))
            .await
            .expect("task submit");
        let task_id = task["task_id"].as_str().unwrap().to_owned();
        let assigned: serde_json::Value = worker
            .call_typed(
                "remote_task_assign",
                &json!({"task_id": task_id, "worker_id": "worker-1"}),
            )
            .await
            .expect("assign");
        assert_eq!(assigned["status"], "queued");
        let dispatch: serde_json::Value = worker
            .call_typed::<_, Option<serde_json::Value>>(
                "remote_task_claim",
                &json!({"worker_id": "worker-1", "lease_ttl_ms": 60000}),
            )
            .await
            .expect("claim")
            .expect("a dispatched task");
        let attempt = dispatch["attempt"].as_u64().unwrap() as u32;
        let run_id = dispatch["run_id"].as_str().unwrap().to_owned();
        assert_eq!(attempt, 1);
        assert_eq!(dispatch["input"], "remote task");
        assert_eq!(dispatch["permissions"], "auto");
        let entries = dispatch["workspace"].as_array().unwrap();
        let notes = entries
            .iter()
            .find(|entry| entry["path"] == "notes.txt")
            .expect("workspace manifest carries notes.txt");
        let base_sha = notes["sha256"].as_str().unwrap().to_owned();
        assert_eq!(base_sha, soyo_core::remote::sha256_hex(b"base\n"));
        assert_eq!(fetch_remote_blob(&worker, &base_sha).await, b"base\n");

        // Confirm the start, then report progress and a completed result with
        // one modified file uploaded as a blob.
        worker
            .call_typed::<_, serde_json::Value>(
                "remote_task_start",
                &json!({"worker_id": "worker-1", "task_id": task_id, "attempt": attempt}),
            )
            .await
            .expect("start");
        let heartbeat: serde_json::Value = worker
            .call_typed(
                "remote_task_heartbeat",
                &json!({"worker_id": "worker-1", "task_id": task_id, "attempt": attempt}),
            )
            .await
            .expect("heartbeat");
        assert_eq!(heartbeat["renewed"], true);
        assert_eq!(heartbeat["cancel"], false);
        worker
            .call_typed::<_, serde_json::Value>(
                "remote_task_progress",
                &json!({
                    "worker_id": "worker-1",
                    "task_id": task_id,
                    "attempt": attempt,
                    "phase": "running",
                    "detail": "model loop"
                }),
            )
            .await
            .expect("progress");
        let workers: Vec<serde_json::Value> = worker
            .call_typed("remote_workers", &json!({}))
            .await
            .unwrap();
        assert_eq!(workers[0]["progress"]["phase"], "running");

        let new_sha = upload_remote_blob(&worker, b"remote\n").await;
        let finished: serde_json::Value = worker
            .call_typed(
                "remote_task_finish",
                &json!({
                    "schema_version": 1,
                    "worker_id": "worker-1",
                    "task_id": task_id,
                    "attempt": attempt,
                    "run_id": run_id,
                    "status": "completed",
                    "text": "remote output",
                    "persistence_saved": true,
                    "summary": "remote summary",
                    "changed_files": [{
                        "path": "notes.txt",
                        "kind": "modified",
                        "base_sha256": base_sha,
                        "new_sha256": new_sha,
                        "bytes": 7
                    }]
                }),
            )
            .await
            .expect("finish");
        assert_eq!(finished["status"], "completed");
        let shown: serde_json::Value = worker
            .call_typed("remote_task_show", &json!({"task_id": task_id}))
            .await
            .unwrap();
        assert_eq!(shown["result"]["changed_files"][0]["path"], "notes.txt");
        // The result is held back until the explicit apply.
        assert_eq!(
            std::fs::read(workspace.join("notes.txt")).unwrap(),
            b"base\n".as_slice()
        );
        let applied: serde_json::Value = worker
            .call_typed("remote_task_apply", &json!({"task_id": task_id}))
            .await
            .expect("apply");
        assert_eq!(applied["applied"], true, "{applied}");
        assert_eq!(applied["receipts"][0]["operation"], "modified");
        assert_eq!(
            std::fs::read(workspace.join("notes.txt")).unwrap(),
            b"remote\n".as_slice()
        );

        // Divergence: a local edit after dispatch makes apply refuse and write
        // nothing.
        let task: serde_json::Value = local
            .call_typed("task_submit", &json!({"input": "diverging task"}))
            .await
            .unwrap();
        let task_id = task["task_id"].as_str().unwrap().to_owned();
        worker
            .call_typed::<_, serde_json::Value>(
                "remote_task_assign",
                &json!({"task_id": task_id, "worker_id": "worker-1"}),
            )
            .await
            .unwrap();
        let dispatch: serde_json::Value = worker
            .call_typed::<_, Option<serde_json::Value>>(
                "remote_task_claim",
                &json!({"worker_id": "worker-1", "lease_ttl_ms": 60000}),
            )
            .await
            .unwrap()
            .expect("second dispatch");
        let attempt = dispatch["attempt"].as_u64().unwrap() as u32;
        let run_id = dispatch["run_id"].as_str().unwrap().to_owned();
        let base_sha = dispatch["workspace"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["path"] == "notes.txt")
            .unwrap()["sha256"]
            .as_str()
            .unwrap()
            .to_owned();
        worker
            .call_typed::<_, serde_json::Value>(
                "remote_task_start",
                &json!({"worker_id": "worker-1", "task_id": task_id, "attempt": attempt}),
            )
            .await
            .unwrap();
        let new_sha = upload_remote_blob(&worker, b"worker-two\n").await;
        std::fs::write(workspace.join("notes.txt"), b"local-edit\n").unwrap();
        worker
            .call_typed::<_, serde_json::Value>(
                "remote_task_finish",
                &json!({
                    "schema_version": 1,
                    "worker_id": "worker-1",
                    "task_id": task_id,
                    "attempt": attempt,
                    "run_id": run_id,
                    "status": "completed",
                    "persistence_saved": true,
                    "changed_files": [{
                        "path": "notes.txt",
                        "kind": "modified",
                        "base_sha256": base_sha,
                        "new_sha256": new_sha,
                        "bytes": 11
                    }]
                }),
            )
            .await
            .unwrap();
        let refused: serde_json::Value = worker
            .call_typed("remote_task_apply", &json!({"task_id": task_id}))
            .await
            .expect("apply report");
        assert_eq!(refused["applied"], false, "{refused}");
        assert_eq!(refused["conflicts"][0]["path"], "notes.txt");
        assert_eq!(
            std::fs::read(workspace.join("notes.txt")).unwrap(),
            b"local-edit\n".as_slice()
        );

        // Lease rules: an unconfirmed dispatch returns to the queue; a
        // confirmed start never replays automatically.
        let task: serde_json::Value = local
            .call_typed("task_submit", &json!({"input": "lease task"}))
            .await
            .unwrap();
        let task_id = task["task_id"].as_str().unwrap().to_owned();
        worker
            .call_typed::<_, serde_json::Value>(
                "remote_task_assign",
                &json!({"task_id": task_id, "worker_id": "worker-1"}),
            )
            .await
            .unwrap();
        let dispatch: serde_json::Value = worker
            .call_typed::<_, Option<serde_json::Value>>(
                "remote_task_claim",
                &json!({"worker_id": "worker-1", "lease_ttl_ms": 1000}),
            )
            .await
            .unwrap()
            .expect("unstarted dispatch");
        assert_eq!(dispatch["attempt"], 1);
        tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
        let recovered: Vec<serde_json::Value> =
            local.call_typed("task_recover", &json!({})).await.unwrap();
        let record = recovered
            .iter()
            .find(|record| record["task_id"] == task_id.as_str())
            .expect("recovered record");
        assert_eq!(record["status"], "queued", "{record}");
        let dispatch: serde_json::Value = worker
            .call_typed::<_, Option<serde_json::Value>>(
                "remote_task_claim",
                &json!({"worker_id": "worker-1", "lease_ttl_ms": 1000}),
            )
            .await
            .unwrap()
            .expect("requeued dispatch");
        assert_eq!(dispatch["attempt"], 2);
        let attempt = dispatch["attempt"].as_u64().unwrap() as u32;
        worker
            .call_typed::<_, serde_json::Value>(
                "remote_task_start",
                &json!({"worker_id": "worker-1", "task_id": task_id, "attempt": attempt}),
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
        let recovered: Vec<serde_json::Value> =
            local.call_typed("task_recover", &json!({})).await.unwrap();
        let record = recovered
            .iter()
            .find(|record| record["task_id"] == task_id.as_str())
            .expect("recovered record");
        assert_eq!(record["status"], "waiting_unknown", "{record}");
        // Never replayed without an explicit resolution.
        let none: Option<serde_json::Value> = worker
            .call_typed("remote_task_claim", &json!({"worker_id": "worker-1"}))
            .await
            .unwrap();
        assert!(none.is_none());
        let resolved: serde_json::Value = local
            .call_typed("task_resolve", &json!({"task_id": task_id, "retry": true}))
            .await
            .unwrap();
        assert_eq!(resolved["status"], "queued");
        let dispatch: serde_json::Value = worker
            .call_typed::<_, Option<serde_json::Value>>(
                "remote_task_claim",
                &json!({"worker_id": "worker-1"}),
            )
            .await
            .unwrap()
            .expect("explicitly retried dispatch");
        assert_eq!(dispatch["attempt"], 3);

        local.shutdown_daemon().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(HelperClient::connect_for(&target).await.is_err());
    }

    /// Process-global env vars must be restored on drop so other tests in the
    /// same process never observe a leaked configuration.
    struct EnvGuard(Vec<(String, Option<std::ffi::OsString>)>);

    impl EnvGuard {
        fn set(vars: &[(&str, &str)]) -> Self {
            let saved = vars
                .iter()
                .map(|(key, _)| (key.to_string(), std::env::var_os(key)))
                .collect();
            // SAFETY: all env-mutating tests in this crate hold `serial()`.
            unsafe {
                for (key, value) in vars {
                    std::env::set_var(key, value);
                }
            }
            Self(saved)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: same serialization as `set`.
            unsafe {
                for (key, value) in &self.0 {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    fn setup_params() -> soyo_ipc::types::SetupEnvironmentParams {
        soyo_ipc::types::SetupEnvironmentParams {
            profile_name: "openai".into(),
            protocol: "openai-responses".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-test".into(),
            auth: Some("bearer".into()),
            api_key_env: Some("SOYO_SETUP_TEST_KEY".into()),
            api_key: Some("sk-test-123".into()),
            context_window_tokens: Some(64_000),
            max_output_tokens: Some(4_096),
            workspace: None,
            permission_mode: Some(soyo_ipc::types::PermissionModeDto::Manual),
            work_mode: None,
        }
    }

    #[tokio::test]
    async fn setup_environment_writes_config_overlay_and_bootstrap() {
        let _guard = serial().await;
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("helper");
        let state_home = dir.path().join("state");
        let config_path = dir.path().join("config.toml");
        let _env = EnvGuard::set(&[
            ("SOYO_HELPER_DIR", helper.to_str().unwrap()),
            ("XDG_STATE_HOME", state_home.to_str().unwrap()),
            ("SOYO_CONFIG", config_path.to_str().unwrap()),
        ]);
        let service = HostService::from_environment();
        assert!(!service.get_bootstrap_state().await.config_ready);

        let boot = service
            .setup_environment(setup_params())
            .await
            .expect("setup should succeed");

        assert!(boot.config_ready);
        assert_eq!(boot.selected_profile.as_deref(), Some("openai"));

        // config.toml references the variable name, never the secret value.
        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            text.contains("api_key_env = \"SOYO_SETUP_TEST_KEY\""),
            "{text}"
        );
        assert!(text.contains("permissions = \"manual\""), "{text}");
        assert!(!text.contains("sk-test-123"), "{text}");

        // The secret lives only in `<state_dir>/secrets/env` (0700 dir, 0600 file).
        let env_path = state_home.join("soyo").join("secrets").join("env");
        let overlay = std::fs::read_to_string(&env_path).unwrap();
        assert!(
            overlay.contains("SOYO_SETUP_TEST_KEY=sk-test-123"),
            "{overlay}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(env_path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let file_mode = std::fs::metadata(&env_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }

        // Re-running merges into the same document instead of replacing it.
        let mut second = setup_params();
        second.profile_name = "secondary".into();
        second.auth = Some("none".into());
        second.api_key_env = None;
        second.api_key = None;
        second.base_url = "http://localhost:11434/v1".into();
        service
            .setup_environment(second)
            .await
            .expect("second profile merge");
        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(text.contains("[profiles.openai]"), "{text}");
        assert!(text.contains("[profiles.secondary]"), "{text}");

        // A missing workspace directory is created instead of rejected.
        let nested = dir.path().join("nested").join("work");
        let mut third = setup_params();
        third.profile_name = "third".into();
        third.auth = Some("none".into());
        third.api_key_env = None;
        third.api_key = None;
        third.base_url = "http://localhost:11434/v1".into();
        third.workspace = Some(nested.to_str().unwrap().to_owned());
        let boot = service
            .setup_environment(third)
            .await
            .expect("setup creates missing workspace");
        assert!(nested.is_dir());
        assert_eq!(
            boot.workspace,
            nested.canonicalize().unwrap().to_string_lossy().to_string()
        );
    }

    #[tokio::test]
    async fn helper_permissions_and_flock_serialization() {
        let _guard = serial().await;
        let (_dir, handle) = running_server().await;
        let target = soyo_ipc::HelperTarget::from_environment().unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(target.helper_dir())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let sock_mode = std::fs::metadata(target.socket_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let pid_mode = std::fs::metadata(target.pid_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(sock_mode, 0o600);
            assert_eq!(pid_mode, 0o600);
        }

        // Launching a competing server against the same HelperTarget must fail
        // with helper_already_running without disturbing the active server.
        let rival = HelperServer::new(HostService::from_environment());
        let err = rival.run().await.unwrap_err();
        assert_eq!(err.code, "helper_already_running");

        let client = HelperClient::connect()
            .await
            .expect("live helper stays reachable");
        client.shutdown_daemon().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn legacy_env_migrates_once_without_clobbering() {
        let _guard = serial().await;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let legacy_dir = home.join(".soyo");
        let legacy_env = legacy_dir.join("env");
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(&legacy_env, "SOYO_MIGRATE_TEST=migrated-secret\n").unwrap();

        let _env = EnvGuard::set(&[("HOME", home.to_str().unwrap())]);
        crate::envfile::migrate_legacy_env(&state_dir);

        let dest = crate::envfile::env_file_path_for(&state_dir);
        let migrated = std::fs::read_to_string(&dest).unwrap();
        assert!(
            migrated.contains("SOYO_MIGRATE_TEST=migrated-secret"),
            "{migrated}"
        );
        assert!(!legacy_env.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(dest.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let file_mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }

        // Re-introducing a legacy env file later must not overwrite destination.
        std::fs::write(&legacy_env, "SOYO_MIGRATE_TEST=must-not-overwrite\n").unwrap();
        crate::envfile::migrate_legacy_env(&state_dir);
        let after = std::fs::read_to_string(&dest).unwrap();
        assert!(after.contains("migrated-secret"));
        assert!(!after.contains("must-not-overwrite"));
    }

    #[tokio::test]
    async fn setup_environment_rejects_invalid_params_and_restores() {
        let _guard = serial().await;
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let _env = EnvGuard::set(&[
            (
                "SOYO_HELPER_DIR",
                dir.path().join("helper").to_str().unwrap(),
            ),
            ("SOYO_CONFIG", config_path.to_str().unwrap()),
        ]);
        let service = HostService::from_environment();

        // Bearer auth requires an env-var name to reference.
        let mut params = setup_params();
        params.api_key_env = None;
        let err = service.setup_environment(params).await.unwrap_err();
        assert_eq!(err.code, "invalid_args");

        // auth `none` cannot carry key material.
        let mut params = setup_params();
        params.auth = Some("none".into());
        let err = service.setup_environment(params).await.unwrap_err();
        assert_eq!(err.code, "invalid_args");

        // Profile names are restricted to TOML-safe keys.
        let mut params = setup_params();
        params.profile_name = "bad name!".into();
        let err = service.setup_environment(params).await.unwrap_err();
        assert_eq!(err.code, "invalid_args");

        // A config that fails validation leaves no partial file behind.
        let mut params = setup_params();
        params.base_url = "http://example.com".into();
        let err = service.setup_environment(params).await.unwrap_err();
        assert_ne!(err.code, "invalid_args");
        assert!(!config_path.exists());
    }
}
