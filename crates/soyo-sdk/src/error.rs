//! SDK error surface: a small stable enum mapping `soyo_ipc` wire errors.

use soyo_ipc::types::DesktopError;
use std::fmt;

/// Top-level SDK error. `Helper` wraps the helper's structured wire error;
/// the other variants cover client-side conditions the wire never sees.
#[derive(Debug)]
pub enum SdkError {
    /// Helper returned a structured `DesktopError` (code + message preserved).
    Helper(DesktopError),
    /// Connection to the helper could not be established or was lost.
    Disconnected(String),
    /// The running helper is a different version/protocol than this SDK and
    /// cannot be recycled while it is busy (run or other clients active).
    IncompatibleHelper(String),
    /// Caller-supplied input failed client-side validation before any RPC.
    InvalidInput(String),
}

impl SdkError {
    /// Wire or client error code (stable machine-readable discriminator).
    pub fn code(&self) -> &str {
        match self {
            Self::Helper(err) => err.code.as_str(),
            Self::Disconnected(_) => "helper_disconnected",
            Self::IncompatibleHelper(_) => "helper_incompatible",
            Self::InvalidInput(_) => "invalid_input",
        }
    }

    /// Human-readable message.
    pub fn message(&self) -> &str {
        match self {
            Self::Helper(err) => err.message.as_str(),
            Self::Disconnected(msg) | Self::IncompatibleHelper(msg) | Self::InvalidInput(msg) => {
                msg.as_str()
            }
        }
    }

    /// True when the underlying condition is a lost/closed helper connection —
    /// callers may retry `connect`/`attach` without cancelling the remote run.
    pub fn is_disconnect(&self) -> bool {
        match self {
            Self::Disconnected(_) => true,
            Self::Helper(err) => matches!(
                err.code.as_str(),
                "helper_disconnected" | "helper_io" | "helper_shutdown" | "client_detached"
            ),
            _ => false,
        }
    }
}

impl fmt::Display for SdkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for SdkError {}

impl From<DesktopError> for SdkError {
    fn from(err: DesktopError) -> Self {
        if matches!(
            err.code.as_str(),
            "helper_disconnected" | "helper_io" | "helper_shutdown" | "helper_unavailable"
        ) {
            Self::Disconnected(err.message)
        } else {
            Self::Helper(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_codes_map_to_disconnected_variant() {
        for code in [
            "helper_disconnected",
            "helper_io",
            "helper_shutdown",
            "helper_unavailable",
        ] {
            let err = SdkError::from(DesktopError::new(code, "x"));
            assert!(err.is_disconnect(), "{code}");
            assert!(matches!(err, SdkError::Disconnected(_)));
        }
    }

    #[test]
    fn domain_codes_stay_helper_errors() {
        let err = SdkError::from(DesktopError::new("session_changed", "x"));
        assert!(!err.is_disconnect());
        assert_eq!(err.code(), "session_changed");
        assert!(matches!(err, SdkError::Helper(_)));
    }
}
