//! Opt-in Project Index & Sandboxed LSP Code Intelligence (`F-17`).
//!
//! Disabled by default (`[code_intel] enabled = false`, `[code_intel.lsp] enabled = false`).
//! When enabled:
//! - Layer 1 builds and incrementally maintains an external SQLite symbol index strictly outside
//!   `<workspace>` at `<cache_dir>/index/<sha256(workspace)>/project-index.sqlite3`.
//! - Layer 2 optionally spawns configured LSP 3.17 servers inside a read-only workspace sandbox
//!   (`PreparedEnvironment`) with only `<cache_dir>/lsp/<sha256(workspace)>/<server_id>/` writable.

pub mod index;
pub mod lsp;
pub mod symbols;
pub mod tools;

pub use index::{
    FileOutlineResult, IndexStatus, SymbolSearchResult, WorkspaceIndex, validate_external_cache_dir,
};
pub use lsp::{
    DiagnosticEntry, EditSuggestion, EditSuggestionFile, LspClientConfig, LspManager,
    SourceLocation,
};
pub use symbols::{IndexedSymbol, SymbolKind};
pub use tools::{
    CODE_INTEL_TOOL_NAMES, definition_for, definitions, is_code_intel_tool, validate_call,
};

use crate::{
    boundary::Cancellation,
    config::{CodeIntelSettings, Environment},
    contract::{BoundaryError, ToolCall, ToolResult},
    execution::BackendKind,
    tools::read::ReadTools,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex as AsyncMutex;

/// Runtime configuration for initializing workspace code intelligence.
#[derive(Debug, Clone)]
pub struct CodeIntelConfig {
    pub workspace: PathBuf,
    pub cache_dir: PathBuf,
    pub sandbox_backend: BackendKind,
    pub settings: CodeIntelSettings,
    pub protected_paths: Vec<PathBuf>,
    pub host_env: Environment,
}

/// Shared runtime handle owning the incremental project index and sandboxed LSP sessions.
#[derive(Clone)]
pub struct CodeIntelState {
    settings: CodeIntelSettings,
    pub(crate) index: Arc<AsyncMutex<WorkspaceIndex>>,
    pub(crate) lsp: Arc<AsyncMutex<LspManager>>,
}

impl CodeIntelState {
    /// Construct and validate an external index + sandboxed LSP handle.
    pub fn new(config: CodeIntelConfig) -> Result<Self, BoundaryError> {
        let index = WorkspaceIndex::new(
            &config.workspace,
            &config.cache_dir,
            config.settings.clone(),
            config.protected_paths.clone(),
        )?;
        let lsp = LspManager::new(
            LspClientConfig {
                workspace: config.workspace,
                cache_dir: config.cache_dir,
                sandbox_backend: config.sandbox_backend,
                settings: config.settings.lsp.clone(),
                protected_paths: config.protected_paths,
                host_env: config.host_env,
            },
            config.settings.max_file_bytes,
        );
        Ok(Self {
            settings: config.settings,
            index: Arc::new(AsyncMutex::new(index)),
            lsp: Arc::new(AsyncMutex::new(lsp)),
        })
    }

    pub fn settings(&self) -> &CodeIntelSettings {
        &self.settings
    }

    pub fn is_enabled(&self) -> bool {
        self.settings.enabled
    }

    pub async fn invalidate_path(&self, path: &Path) {
        self.index.lock().await.invalidate_path(path);
        self.lsp.lock().await.invalidate_path(path).await;
    }

    pub async fn invalidate_all(&self) {
        self.index.lock().await.invalidate_all();
    }

    pub async fn build_index(
        &self,
        read_tools: Option<&ReadTools>,
    ) -> Result<IndexStatus, BoundaryError> {
        self.index.lock().await.build_or_refresh(read_tools)
    }

    pub async fn status(&self) -> Result<IndexStatus, BoundaryError> {
        self.index.lock().await.status()
    }

    pub async fn search_symbols(
        &self,
        query: &str,
        kind: Option<SymbolKind>,
        path_prefix: Option<&Path>,
        limit: Option<usize>,
        read_tools: Option<&ReadTools>,
    ) -> Result<SymbolSearchResult, BoundaryError> {
        self.index
            .lock()
            .await
            .search_symbols(query, kind, path_prefix, limit, read_tools)
    }

    pub async fn file_outline(
        &self,
        path: &Path,
        read_tools: Option<&ReadTools>,
    ) -> Result<FileOutlineResult, BoundaryError> {
        self.index.lock().await.file_outline(path, read_tools)
    }

    pub async fn execute(
        &self,
        call: &ToolCall,
        read_tools: Option<&ReadTools>,
        known_secrets: &[String],
        cancel: &dyn Cancellation,
    ) -> Result<ToolResult, BoundaryError> {
        tools::execute(self, call, read_tools, known_secrets, cancel).await
    }

    /// Build a bounded `soyo_code_intel_v1` summary (`(sha256, content)`) for optional history
    /// injection when both `enabled` and `inject_context` are true.
    pub async fn context_summary(
        &self,
        read_tools: Option<&ReadTools>,
    ) -> Result<Option<(String, String)>, BoundaryError> {
        if !self.settings.enabled || !self.settings.inject_context {
            return Ok(None);
        }
        self.index
            .lock()
            .await
            .build_context_summary(read_tools, self.settings.max_context_bytes)
    }

    /// Expose active sandboxed LSP external cache directories (diagnostic / test inspection).
    pub async fn active_lsp_cache_dirs(&self) -> Vec<PathBuf> {
        self.lsp.lock().await.active_lsp_cache_dirs()
    }

    /// Shut down all running sandboxed LSP server processes cleanly.
    pub async fn shutdown(&self) {
        self.lsp.lock().await.shutdown_all().await;
    }
}
