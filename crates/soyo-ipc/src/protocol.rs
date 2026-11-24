//! Wire protocol: newline-delimited JSON frames over a local socket.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bump when frame shapes or method semantics change incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Methods the SDK (`soyo-sdk`) and other external consumers may rely on.
///
/// This is the **frozen subset** of the wire contract (D-106): names, request
/// params, and result DTOs for these methods follow the same rule as
/// [`PROTOCOL_VERSION`] — an incompatible change requires a version bump.
/// All other dispatch arms in `soyo-host` remain internal implementation
/// details and may change without notice.
///
/// Grouped by lifecycle surface:
/// - helper handshake: `helper_info`, `get_bootstrap_state`
/// - session lifecycle: `new_session`, `resume_session`, `release_session`,
///   `list_sessions_page`, `get_session`, `read_session_snapshot`
/// - run lifecycle: `submit_prompt` (interactive spawn), `run_prompt`
///   (headless spawn with raw schema-v1 event passthrough), `cancel_run`,
///   `get_run_state`
/// - task queries: `task_list`, `task_jobs`, `task_status`
pub const PUBLISHED_METHODS: &[&str] = &[
    "get_bootstrap_state",
    "helper_info",
    "new_session",
    "resume_session",
    "release_session",
    "list_sessions_page",
    "get_session",
    "read_session_snapshot",
    "submit_prompt",
    "run_prompt",
    "compact_session",
    "cancel_run",
    "get_run_state",
    "task_list",
    "task_jobs",
    "task_status",
];

/// Frames sent from a client to the helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Invoke a service method. `params` is the JSON object of named arguments.
    Request {
        id: u64,
        method: String,
        params: Value,
    },
    /// Best-effort cancel of an in-flight streaming request.
    CancelRequest { id: u64 },
    /// Ask the helper daemon to shut down gracefully.
    Shutdown,
}

/// Frames sent from the helper to a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerFrame {
    /// First frame on every connection.
    Hello { version: u32, pid: u32 },
    /// Stream item for a streaming `Request` (serialized `DesktopStreamEnvelope`).
    Event { id: u64, data: Value },
    /// Terminal success for `id`.
    Response { id: u64, result: Value },
    /// Terminal failure for `id`.
    Error {
        id: u64,
        code: String,
        message: String,
    },
    /// Sent before the daemon exits (response to `Shutdown` or signal).
    Bye,
}

#[cfg(test)]
mod tests {
    use super::PUBLISHED_METHODS;

    /// The frozen surface must stay small and intentional; bumping it is a
    /// contract decision (D-106), not a drive-by edit. Keep the count aligned
    /// with the table in `docs/sdk.md`.
    #[test]
    fn published_method_set_is_stable_and_unique() {
        assert_eq!(PUBLISHED_METHODS.len(), 16);
        let mut sorted = PUBLISHED_METHODS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), PUBLISHED_METHODS.len());
        for name in PUBLISHED_METHODS {
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
                "method name must be snake_case: {name}"
            );
        }
    }
}
