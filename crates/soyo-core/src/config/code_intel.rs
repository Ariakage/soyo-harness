//! Configuration for local project symbol indexing and sandboxed LSP code intelligence (`F-17`).

use super::{Environment, invalid, valid_env_name};
use crate::{contract::BoundaryError, execution::ResourceLimits, registry::valid_server_id};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const DEFAULT_INDEX_MAX_CONTEXT_BYTES: usize = 8 * 1024;
pub const MAX_INDEX_CONTEXT_BYTES: usize = 64 * 1024;
pub const DEFAULT_INDEX_MAX_FILES: usize = 10_000;
pub const MAX_INDEX_FILES: usize = 100_000;
pub const DEFAULT_INDEX_MAX_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_INDEX_FILE_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_INDEX_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_INDEX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_INDEX_MAX_SYMBOLS_PER_FILE: usize = 512;
pub const MAX_INDEX_SYMBOLS_PER_FILE: usize = 4096;
pub const DEFAULT_INDEX_MAX_QUERY_RESULTS: usize = 50;
pub const MAX_INDEX_QUERY_RESULTS: usize = 200;

pub const DEFAULT_LSP_REQUEST_TIMEOUT_SECONDS: u64 = 15;
pub const MAX_LSP_TIMEOUT_SECONDS: u64 = 120;
pub const DEFAULT_LSP_STARTUP_TIMEOUT_SECONDS: u64 = 30;
pub const DEFAULT_LSP_MAX_DIAGNOSTICS_PER_FILE: usize = 100;
pub const MAX_LSP_DIAGNOSTICS_PER_FILE: usize = 500;
pub const DEFAULT_LSP_MAX_OPEN_DOCUMENTS: usize = 64;
pub const MAX_LSP_OPEN_DOCUMENTS: usize = 256;
pub const DEFAULT_LSP_MAX_EDIT_FILES: usize = 32;
pub const MAX_LSP_EDIT_FILES: usize = 128;

fn yes() -> bool {
    true
}

fn valid_path(path: &Path) -> bool {
    let raw = path.as_os_str();
    !raw.is_empty() && raw.len() <= 4096 && !raw.as_encoded_bytes().contains(&0)
}

/// Host-controlled project indexing and LSP settings (`[code_intel]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodeIntelSettings {
    /// Disabled by default to keep existing sessions and tool schemas unchanged.
    pub enabled: bool,
    /// Inject retrieved/fresh symbol and diagnostic summaries on demand as low-trust context.
    #[serde(default = "yes")]
    pub inject_context: bool,
    pub max_context_bytes: usize,
    pub max_files: usize,
    pub max_file_bytes: usize,
    pub max_total_index_bytes: u64,
    pub max_symbols_per_file: usize,
    pub max_query_results: usize,
    pub lsp: LspSettings,
}

impl Default for CodeIntelSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            inject_context: true,
            max_context_bytes: DEFAULT_INDEX_MAX_CONTEXT_BYTES,
            max_files: DEFAULT_INDEX_MAX_FILES,
            max_file_bytes: DEFAULT_INDEX_MAX_FILE_BYTES,
            max_total_index_bytes: DEFAULT_INDEX_MAX_TOTAL_BYTES,
            max_symbols_per_file: DEFAULT_INDEX_MAX_SYMBOLS_PER_FILE,
            max_query_results: DEFAULT_INDEX_MAX_QUERY_RESULTS,
            lsp: LspSettings::default(),
        }
    }
}

/// Sandboxed LSP client limits and explicitly configured language servers (`[code_intel.lsp]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LspSettings {
    pub enabled: bool,
    pub request_timeout_seconds: u64,
    pub startup_timeout_seconds: u64,
    pub max_diagnostics_per_file: usize,
    pub max_open_documents: usize,
    pub max_edit_files: usize,
    pub resources: ResourceLimits,
    pub read_roots: Vec<PathBuf>,
    pub servers: BTreeMap<String, LspServerSettings>,
}

impl Default for LspSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            request_timeout_seconds: DEFAULT_LSP_REQUEST_TIMEOUT_SECONDS,
            startup_timeout_seconds: DEFAULT_LSP_STARTUP_TIMEOUT_SECONDS,
            max_diagnostics_per_file: DEFAULT_LSP_MAX_DIAGNOSTICS_PER_FILE,
            max_open_documents: DEFAULT_LSP_MAX_OPEN_DOCUMENTS,
            max_edit_files: DEFAULT_LSP_MAX_EDIT_FILES,
            resources: ResourceLimits::default(),
            read_roots: Vec::new(),
            servers: BTreeMap::new(),
        }
    }
}

/// Explicit configuration for one sandboxed stdio LSP server (`[code_intel.lsp.servers.<id>]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LspServerSettings {
    pub enabled: bool,
    pub argv: Vec<String>,
    pub languages: Vec<String>,
    pub extensions: Vec<String>,
    pub root_markers: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub read_roots: Vec<PathBuf>,
    pub initialization_options: Option<Value>,
}

impl CodeIntelSettings {
    /// Validate limits, server schemas, paths, and env reference names offline.
    pub fn validate(&self, env: &Environment) -> Result<(), BoundaryError> {
        if self.max_context_bytes < 512
            || self.max_context_bytes > MAX_INDEX_CONTEXT_BYTES
            || self.max_files == 0
            || self.max_files > MAX_INDEX_FILES
            || self.max_file_bytes < 1024
            || self.max_file_bytes > MAX_INDEX_FILE_BYTES
            || self.max_total_index_bytes < 65_536
            || self.max_total_index_bytes > MAX_INDEX_TOTAL_BYTES
            || self.max_symbols_per_file == 0
            || self.max_symbols_per_file > MAX_INDEX_SYMBOLS_PER_FILE
            || self.max_query_results == 0
            || self.max_query_results > MAX_INDEX_QUERY_RESULTS
        {
            return Err(invalid("code_intel indexing limits are invalid"));
        }
        self.lsp.validate(self.enabled, env)
    }
}

impl LspSettings {
    fn validate(&self, code_intel_enabled: bool, env: &Environment) -> Result<(), BoundaryError> {
        if self.request_timeout_seconds == 0
            || self.request_timeout_seconds > MAX_LSP_TIMEOUT_SECONDS
            || self.startup_timeout_seconds == 0
            || self.startup_timeout_seconds > MAX_LSP_TIMEOUT_SECONDS
            || self.max_diagnostics_per_file == 0
            || self.max_diagnostics_per_file > MAX_LSP_DIAGNOSTICS_PER_FILE
            || self.max_open_documents == 0
            || self.max_open_documents > MAX_LSP_OPEN_DOCUMENTS
            || self.max_edit_files == 0
            || self.max_edit_files > MAX_LSP_EDIT_FILES
        {
            return Err(invalid("code_intel.lsp limits are invalid"));
        }
        self.resources
            .validate()
            .map_err(|_| invalid("code_intel.lsp resource limits are invalid"))?;
        if self.read_roots.len() > 64 || self.read_roots.iter().any(|p| !valid_path(p)) {
            return Err(invalid("code_intel.lsp read_roots are invalid"));
        }
        if self.servers.len() > 16 {
            return Err(invalid("too many configured code_intel.lsp servers"));
        }
        for (id, server) in &self.servers {
            if !valid_server_id(id) {
                return Err(invalid("code_intel.lsp server id is invalid"));
            }
            server.validate(code_intel_enabled && self.enabled && server.enabled, env)?;
        }
        Ok(())
    }
}

impl LspServerSettings {
    fn validate(&self, active: bool, env: &Environment) -> Result<(), BoundaryError> {
        if active && self.argv.is_empty() {
            return Err(invalid("enabled LSP server requires explicit argv"));
        }
        if self.argv.len() > 32
            || self
                .argv
                .iter()
                .any(|arg| arg.is_empty() || arg.len() > 4096 || arg.contains('\0'))
        {
            return Err(invalid("code_intel.lsp server argv is invalid"));
        }
        if self.languages.len() > 16
            || self.languages.iter().any(|lang| {
                lang.is_empty()
                    || lang.len() > 32
                    || !lang
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
        {
            return Err(invalid("code_intel.lsp server languages are invalid"));
        }
        if self.extensions.len() > 32
            || self.extensions.iter().any(|ext| {
                ext.is_empty()
                    || ext.len() > 32
                    || !ext
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
        {
            return Err(invalid("code_intel.lsp server extensions are invalid"));
        }
        if self.root_markers.len() > 16
            || self
                .root_markers
                .iter()
                .any(|m| m.is_empty() || m.len() > 128 || m.contains('\0'))
        {
            return Err(invalid("code_intel.lsp server root_markers are invalid"));
        }
        if self.read_roots.len() > 64 || self.read_roots.iter().any(|p| !valid_path(p)) {
            return Err(invalid("code_intel.lsp server read_roots are invalid"));
        }
        if self.env.len() > 32 {
            return Err(invalid("too many code_intel.lsp environment references"));
        }
        for (var_name, ref_name) in &self.env {
            if !valid_env_name(var_name) || !valid_env_name(ref_name) {
                return Err(invalid("code_intel.lsp environment mapping is invalid"));
            }
            if active && env.reference(ref_name)?.contains('\0') {
                return Err(invalid(
                    "code_intel.lsp environment reference contains invalid value",
                ));
            }
        }
        Ok(())
    }
}
