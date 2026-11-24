//! Opt-in `MediaKind::File` text extraction (`F-11`). Disabled by default;
//! extracted text travels as bounded TEXT content, never model media.

use crate::contract::{BoundaryError, ErrorInfo};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which converter produces the extracted text.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileTextConverter {
    /// Prefer a resolved `markitdown` binary; fall back to the builtin PDF extractor.
    #[default]
    Auto,
    /// Require the external `markitdown` binary.
    Markitdown,
    /// Use only the pure-Rust builtin extractor.
    Builtin,
}

/// `[file_text]` host configuration; `enabled` defaults to `false` so File
/// artifacts stay host-side evidence unless the user opts in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileTextSettings {
    pub enabled: bool,
    pub converter: FileTextConverter,
    /// Optional absolute path to a `markitdown` executable. `None` resolves
    /// `markitdown` on the captured `PATH` at extraction time.
    pub markitdown_path: Option<PathBuf>,
    /// Maximum extracted text retained per file, in bytes.
    pub max_text_bytes: usize,
    /// Maximum File artifacts expanded per model request.
    pub max_files_per_message: usize,
    /// Wall-clock cap for one external conversion.
    pub timeout_seconds: u64,
    /// Total size bound for the content-addressed extraction cache.
    pub cache_max_bytes: u64,
}
impl Default for FileTextSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            converter: FileTextConverter::Auto,
            markitdown_path: None,
            max_text_bytes: 256 * 1024,
            max_files_per_message: 4,
            timeout_seconds: 30,
            cache_max_bytes: 512 * 1024 * 1024,
        }
    }
}
impl FileTextSettings {
    /// Reject zero or over-range limits before any extraction is attempted.
    pub fn validate(&self) -> Result<(), BoundaryError> {
        if self.max_text_bytes == 0
            || self.max_text_bytes > 2 * 1024 * 1024
            || self.max_files_per_message == 0
            || self.max_files_per_message > 8
            || self.timeout_seconds == 0
            || self.timeout_seconds > 120
            || self.cache_max_bytes == 0
        {
            return Err(failure(
                "invalid_config",
                "file_text limits are outside their supported range",
            ));
        }
        if let Some(path) = &self.markitdown_path
            && !path.is_absolute()
        {
            return Err(failure(
                "invalid_config",
                "file_text markitdown_path must be an absolute path",
            ));
        }
        Ok(())
    }
}

fn failure(code: &str, message: impl Into<String>) -> BoundaryError {
    BoundaryError(
        ErrorInfo {
            code: code.into(),
            message: message.into(),
        },
        None,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_disabled_and_bounded() {
        let settings = FileTextSettings::default();
        assert!(!settings.enabled);
        assert_eq!(settings.converter, FileTextConverter::Auto);
        settings.validate().unwrap();
    }

    #[test]
    fn out_of_range_limits_are_rejected() {
        for bad in [
            FileTextSettings {
                max_text_bytes: 0,
                ..Default::default()
            },
            FileTextSettings {
                max_text_bytes: 4 * 1024 * 1024,
                ..Default::default()
            },
            FileTextSettings {
                max_files_per_message: 0,
                ..Default::default()
            },
            FileTextSettings {
                timeout_seconds: 0,
                ..Default::default()
            },
            FileTextSettings {
                timeout_seconds: 3600,
                ..Default::default()
            },
            FileTextSettings {
                markitdown_path: Some("relative/markitdown".into()),
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }
}
