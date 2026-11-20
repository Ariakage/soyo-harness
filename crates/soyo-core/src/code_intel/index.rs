//! External SQLite incremental workspace symbol index (`F-17`).
//!
//! Stored strictly outside `<workspace>` at `<cache_dir>/index/<sha256(workspace)>/project-index.sqlite3`.
//! Respects `.gitignore`/`.ignore`, `ProtectedPaths`, `.git` repository control paths, and
//! refuses symlinks on both workspace files and cache DB paths.

use super::symbols::{IndexedSymbol, SymbolKind, extract_symbols, language_id};
use crate::{
    config::CodeIntelSettings,
    contract::{BoundaryError, ErrorInfo},
    permissions::is_repository_control_path,
    tools::read::ReadTools,
};
use ignore::WalkBuilder;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Read,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

const SCHEMA_VERSION: &str = "1";
const DB_FILE_NAME: &str = "project-index.sqlite3";

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

fn sql_error(err: rusqlite::Error) -> BoundaryError {
    failure(
        "code_intel_index_error",
        format!("project index SQLite error: {err}"),
    )
}

fn io_error(context: &str, err: std::io::Error) -> BoundaryError {
    failure("code_intel_index_error", format!("{context}: {err}"))
}

/// Normalize a path lexically without touching the filesystem.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(Component::ParentDir);
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Canonicalize an existing ancestor + trailing missing components so non-overlapping checks
/// cannot be bypassed by `..` or symlinks on existing parent dirs.
pub(crate) fn normalize_with_existing_ancestor(path: &Path) -> Result<PathBuf, BoundaryError> {
    let lexical = lexical_normalize(path);
    let mut missing = Vec::new();
    let mut cursor = lexical.as_path();
    loop {
        if cursor.exists() {
            let canon = fs::canonicalize(cursor).map_err(|err| {
                failure(
                    "invalid_config",
                    format!("cannot canonicalize path {}: {err}", cursor.display()),
                )
            })?;
            let mut result = canon;
            for part in missing.into_iter().rev() {
                result.push(part);
            }
            return Ok(result);
        }
        let Some(name) = cursor.file_name() else {
            return Ok(lexical);
        };
        missing.push(name.to_os_string());
        let Some(parent) = cursor.parent() else {
            return Ok(lexical);
        };
        cursor = parent;
    }
}

/// Validate that `cache_dir` lies strictly outside `workspace` (no overlap in either direction).
pub fn validate_external_cache_dir(
    workspace: &Path,
    cache_dir: &Path,
) -> Result<(PathBuf, PathBuf), BoundaryError> {
    if !workspace.is_dir() {
        return Err(failure(
            "invalid_path",
            format!("workspace {} is not a directory", workspace.display()),
        ));
    }
    if cache_dir.as_os_str().is_empty() {
        return Err(failure(
            "invalid_config",
            "code_intel cache_dir must not be empty",
        ));
    }
    let canon_ws = fs::canonicalize(workspace).map_err(|err| {
        failure(
            "invalid_path",
            format!(
                "cannot canonicalize workspace {}: {err}",
                workspace.display()
            ),
        )
    })?;
    let abs_cache = if cache_dir.is_absolute() {
        cache_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|err| failure("invalid_config", format!("cannot resolve cache_dir: {err}")))?
            .join(cache_dir)
    };
    let norm_cache = normalize_with_existing_ancestor(&abs_cache)?;
    if norm_cache == canon_ws
        || norm_cache.starts_with(&canon_ws)
        || canon_ws.starts_with(&norm_cache)
    {
        return Err(failure(
            "invalid_config",
            "code_intel cache_dir must be external to and non-overlapping with workspace",
        ));
    }
    Ok((canon_ws, norm_cache))
}

/// Stable SHA-256 hex key identifying a canonical workspace path.
pub(crate) fn workspace_hash(canonical_workspace: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_workspace.as_os_str().as_encoded_bytes());
    hex_encode(&hasher.finalize())
}

/// SHA-256 hex digest of raw bytes.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

/// Ensure a directory chain starting at `base` exists with mode `0700` and no symlinks on leaf.
pub(crate) fn ensure_private_dir_no_symlink(dir: &Path) -> Result<(), BoundaryError> {
    // Reject symlink on the target directory or its existing immediate ancestors up to creation.
    if let Ok(meta) = fs::symlink_metadata(dir) {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(failure(
                "path_denied",
                format!(
                    "cache directory {} must be a real non-symlink directory",
                    dir.display()
                ),
            ));
        }
    } else {
        fs::create_dir_all(dir).map_err(|err| io_error("cannot create cache directory", err))?;
    }
    let meta =
        fs::symlink_metadata(dir).map_err(|err| io_error("cannot inspect cache directory", err))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(failure(
            "path_denied",
            format!("cache directory {} cannot be a symlink", dir.display()),
        ));
    }
    #[cfg(unix)]
    {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|err| io_error("cannot set cache directory permissions", err))?;
    }
    Ok(())
}

/// Check that no path component inside `workspace..=abs_path` is a symlink and verify `abs_path`
/// resolves inside `workspace`, passes `.git` repository-control protection, `protected_paths`,
/// and `ReadTools::check_private_path` (even in `Bypass` permission mode).
pub(crate) fn validate_workspace_file(
    canonical_workspace: &Path,
    abs_or_rel: &Path,
    protected_paths: &[PathBuf],
    read_tools: Option<&ReadTools>,
) -> Result<(PathBuf, PathBuf), BoundaryError> {
    let candidate = if abs_or_rel.is_absolute() {
        lexical_normalize(abs_or_rel)
    } else {
        lexical_normalize(&canonical_workspace.join(abs_or_rel))
    };
    let rel = candidate.strip_prefix(canonical_workspace).map_err(|_| {
        failure(
            "path_denied",
            format!("path {} is outside workspace", abs_or_rel.display()),
        )
    })?;
    if rel.as_os_str().is_empty() {
        return Err(failure(
            "invalid_path",
            "target path must be a workspace file",
        ));
    }

    // Walk components incrementally to refuse any symlink hop inside workspace.
    let mut probe = canonical_workspace.to_path_buf();
    for component in rel.components() {
        let Component::Normal(part) = component else {
            return Err(failure("path_denied", "invalid workspace path component"));
        };
        probe.push(part);
        let meta = fs::symlink_metadata(&probe).map_err(|err| {
            failure(
                "invalid_path",
                format!("cannot inspect {}: {err}", probe.display()),
            )
        })?;
        if meta.file_type().is_symlink() {
            return Err(failure(
                "path_denied",
                format!("symlink path refused: {}", probe.display()),
            ));
        }
    }

    if !is_allowed_checked_path(canonical_workspace, &candidate, protected_paths, read_tools) {
        return Err(failure(
            "path_denied",
            format!(
                "path {} is protected or repository control data",
                rel.display()
            ),
        ));
    }
    Ok((rel.to_path_buf(), candidate))
}

pub(crate) fn is_allowed_checked_path(
    canonical_workspace: &Path,
    abs_path: &Path,
    protected_paths: &[PathBuf],
    read_tools: Option<&ReadTools>,
) -> bool {
    if is_repository_control_path(abs_path) {
        return false;
    }
    let lexical = lexical_normalize(abs_path);
    if is_repository_control_path(&lexical) {
        return false;
    }
    if let Ok(resolved) = fs::canonicalize(abs_path) {
        if !resolved.starts_with(canonical_workspace) || is_repository_control_path(&resolved) {
            return false;
        }
        for protected in protected_paths {
            let norm_p = normalize_with_existing_ancestor(protected)
                .unwrap_or_else(|_| lexical_normalize(protected));
            if resolved.starts_with(&norm_p) || lexical.starts_with(&norm_p) {
                return false;
            }
        }
    } else {
        return false;
    }
    if let Some(rt) = read_tools
        && rt.check_private_path(abs_path).is_err()
    {
        return false;
    }
    true
}

/// Read a bounded regular workspace file without following symlinks on the final component.
pub(crate) fn read_regular_no_follow(
    path: &Path,
    max_bytes: usize,
) -> Result<(Vec<u8>, u64, i128), BoundaryError> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    opts.custom_flags(libc::O_NOFOLLOW);

    let mut file = opts.open(path).map_err(|err| {
        failure(
            "path_denied",
            format!("cannot safely open file {}: {err}", path.display()),
        )
    })?;
    let meta = file
        .metadata()
        .map_err(|err| io_error("cannot stat file", err))?;
    if !meta.is_file() {
        return Err(failure(
            "invalid_path",
            format!("{} is not a regular file", path.display()),
        ));
    }
    let size = meta.len();
    if size > u64::try_from(max_bytes).unwrap_or(u64::MAX) {
        return Err(failure(
            "file_too_large",
            format!(
                "{} exceeds index max_file_bytes ({max_bytes})",
                path.display()
            ),
        ));
    }
    let mut buf = Vec::with_capacity(usize::try_from(size).unwrap_or(0).min(max_bytes));
    let mut limited = (&mut file).take(u64::try_from(max_bytes).unwrap_or(u64::MAX) + 1);
    limited
        .read_to_end(&mut buf)
        .map_err(|err| io_error("cannot read workspace file", err))?;
    if buf.len() > max_bytes {
        return Err(failure(
            "file_too_large",
            format!(
                "{} exceeds index max_file_bytes ({max_bytes})",
                path.display()
            ),
        ));
    }
    Ok((buf, size, mtime_ns(&meta)))
}

fn mtime_ns(meta: &fs::Metadata) -> i128 {
    #[cfg(unix)]
    {
        (meta.mtime() as i128) * 1_000_000_000 + (meta.mtime_nsec() as i128)
    }
    #[cfg(not(unix))]
    {
        meta.modified()
            .ok()
            .and_then(|ts| ts.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexStatus {
    pub workspace_root: PathBuf,
    pub db_path: PathBuf,
    pub indexed_files: usize,
    pub symbol_count: usize,
    pub total_indexed_bytes: u64,
    pub db_bytes: u64,
    pub last_indexed_ms: Option<u64>,
    pub truncated: bool,
    pub dirty_paths: usize,
    pub dirty_all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymbolSearchResult {
    pub query: String,
    pub indexed_files: usize,
    pub truncated: bool,
    pub matches: Vec<IndexedSymbol>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileOutlineResult {
    pub path: PathBuf,
    pub language: Option<String>,
    pub file_sha256: String,
    pub symbols: Vec<IndexedSymbol>,
}

#[derive(Debug, Clone)]
struct ExistingFileRow {
    size: i64,
    mtime_ns: i128,
    sha256: String,
}

/// Incremental external SQLite index for a single workspace.
pub struct WorkspaceIndex {
    workspace: PathBuf,
    db_path: PathBuf,
    settings: CodeIntelSettings,
    protected_paths: Vec<PathBuf>,
    dirty_paths: BTreeSet<PathBuf>,
    dirty_all: bool,
}

impl WorkspaceIndex {
    /// Open/prepare an external index handle (validating `cache_dir` lies strictly outside
    /// `workspace`). Call [`WorkspaceIndex::build_or_refresh`] to populate the SQLite database.
    pub fn new(
        workspace: &Path,
        cache_dir: &Path,
        settings: CodeIntelSettings,
        protected_paths: Vec<PathBuf>,
    ) -> Result<Self, BoundaryError> {
        let (canon_ws, norm_cache) = validate_external_cache_dir(workspace, cache_dir)?;
        let ws_hash = workspace_hash(&canon_ws);
        let index_dir = norm_cache.join("index").join(&ws_hash);
        let db_path = index_dir.join(DB_FILE_NAME);
        Ok(Self {
            workspace: canon_ws,
            db_path,
            settings,
            protected_paths,
            dirty_paths: BTreeSet::new(),
            dirty_all: true,
        })
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn invalidate_path(&mut self, path: &Path) {
        let candidate = if path.is_absolute() {
            lexical_normalize(path)
        } else {
            lexical_normalize(&self.workspace.join(path))
        };
        if let Ok(rel) = candidate.strip_prefix(&self.workspace)
            && !rel.as_os_str().is_empty()
        {
            self.dirty_paths.insert(rel.to_path_buf());
        } else {
            self.dirty_all = true;
        }
    }

    pub fn invalidate_all(&mut self) {
        self.dirty_all = true;
    }

    fn open_db(&self, create: bool) -> Result<Connection, BoundaryError> {
        let parent = self
            .db_path
            .parent()
            .ok_or_else(|| failure("invalid_path", "index db path has no parent"))?;
        if create {
            if let Some(root_index) = parent.parent() {
                ensure_private_dir_no_symlink(root_index)?;
            }
            ensure_private_dir_no_symlink(parent)?;
        }
        if let Ok(meta) = fs::symlink_metadata(&self.db_path) {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(failure(
                    "path_denied",
                    format!(
                        "index database {} must be a regular non-symlink file",
                        self.db_path.display()
                    ),
                ));
            }
        } else if !create {
            return Err(failure(
                "code_intel_index_missing",
                format!(
                    "index database {} does not exist yet",
                    self.db_path.display()
                ),
            ));
        }

        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        if create {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        let conn = Connection::open_with_flags(&self.db_path, flags).map_err(sql_error)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(sql_error)?;
        #[cfg(unix)]
        if self.db_path.exists() {
            fs::set_permissions(&self.db_path, fs::Permissions::from_mode(0o600))
                .map_err(|err| io_error("cannot set index database permissions", err))?;
        }
        Self::init_schema(&conn)?;
        Ok(conn)
    }

    fn init_schema(conn: &Connection) -> Result<(), BoundaryError> {
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS files (
                rel_path TEXT PRIMARY KEY,
                language TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime_ns TEXT NOT NULL,
                sha256 TEXT NOT NULL,
                symbol_count INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS symbols (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                rel_path TEXT NOT NULL,
                name TEXT NOT NULL,
                name_lower TEXT NOT NULL,
                container TEXT,
                kind TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                start_col INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                end_col INTEGER NOT NULL,
                signature TEXT,
                language TEXT NOT NULL,
                file_sha256 TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_symbols_name_lower ON symbols(name_lower);
            CREATE INDEX IF NOT EXISTS idx_symbols_rel_path ON symbols(rel_path);
            "#,
        )
        .map_err(sql_error)?;
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        match version.as_deref() {
            None => {
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION],
                )
                .map_err(sql_error)?;
            }
            Some(SCHEMA_VERSION) => {}
            Some(other) => {
                return Err(failure(
                    "code_intel_index_version",
                    format!("unsupported project index schema version {other}"),
                ));
            }
        }
        Ok(())
    }

    /// Incrementally build or refresh the workspace index inside `<cache_dir>/index/<ws_hash>/`.
    pub fn build_or_refresh(
        &mut self,
        read_tools: Option<&ReadTools>,
    ) -> Result<IndexStatus, BoundaryError> {
        let mut conn = self.open_db(true)?;
        let mut existing: BTreeMap<String, ExistingFileRow> = BTreeMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT rel_path, size, mtime_ns, sha256 FROM files")
                .map_err(sql_error)?;
            let rows = stmt
                .query_map([], |row| {
                    let rel_path: String = row.get(0)?;
                    let size: i64 = row.get(1)?;
                    let mtime_raw: String = row.get(2)?;
                    let sha256: String = row.get(3)?;
                    Ok((rel_path, size, mtime_raw, sha256))
                })
                .map_err(sql_error)?;
            for item in rows {
                let (rel_path, size, mtime_raw, sha256) = item.map_err(sql_error)?;
                let mtime_ns = mtime_raw.parse::<i128>().unwrap_or(0);
                existing.insert(
                    rel_path,
                    ExistingFileRow {
                        size,
                        mtime_ns,
                        sha256,
                    },
                );
            }
        }

        let mut walker = WalkBuilder::new(&self.workspace);
        walker
            .follow_links(false)
            .standard_filters(true)
            .sort_by_file_name(|a, b| a.cmp(b))
            .filter_entry(|entry| {
                if let Some(name) = entry.file_name().to_str()
                    && name == ".git"
                {
                    return false;
                }
                true
            });

        let mut seen = BTreeSet::new();
        let mut pending_updates: Vec<(String, String, i64, String, String, Vec<IndexedSymbol>)> =
            Vec::new();
        let mut metadata_only_updates: Vec<(String, i64, String)> = Vec::new();
        let mut total_indexed_bytes = 0u64;
        let mut counted_files = 0usize;
        let mut truncated = false;

        for entry_res in walker.build() {
            let Ok(entry) = entry_res else {
                continue;
            };
            let abs_path = entry.path();
            if entry.file_type().is_some_and(|ft| ft.is_symlink()) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(abs_path) else {
                continue;
            };
            if meta.file_type().is_symlink() || !meta.is_file() {
                continue;
            }
            if !is_allowed_checked_path(
                &self.workspace,
                abs_path,
                &self.protected_paths,
                read_tools,
            ) {
                continue;
            }
            let Ok(rel) = abs_path.strip_prefix(&self.workspace) else {
                continue;
            };
            let Some(language) = language_id(rel) else {
                continue;
            };
            if counted_files >= self.settings.max_files {
                truncated = true;
                break;
            }
            let file_size = meta.len();
            if file_size > u64::try_from(self.settings.max_file_bytes).unwrap_or(u64::MAX) {
                continue;
            }
            if total_indexed_bytes.saturating_add(file_size) > self.settings.max_total_index_bytes {
                truncated = true;
                break;
            }

            let rel_str = rel.to_string_lossy().into_owned();
            seen.insert(rel_str.clone());
            counted_files += 1;
            total_indexed_bytes = total_indexed_bytes.saturating_add(file_size);

            let cur_size = i64::try_from(file_size).unwrap_or(i64::MAX);
            let cur_mtime = mtime_ns(&meta);
            let forced_dirty = self.dirty_paths.contains(rel);

            if let Some(prev) = existing.get(&rel_str)
                && !forced_dirty
                && prev.size == cur_size
                && prev.mtime_ns == cur_mtime
                && cur_mtime != 0
            {
                continue;
            }

            let Ok((bytes, _, exact_mtime)) =
                read_regular_no_follow(abs_path, self.settings.max_file_bytes)
            else {
                continue;
            };
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            let digest = sha256_hex(text.as_bytes());
            if let Some(prev) = existing.get(&rel_str)
                && prev.sha256 == digest
                && !forced_dirty
            {
                metadata_only_updates.push((rel_str, cur_size, exact_mtime.to_string()));
                continue;
            }

            let syms = extract_symbols(
                rel,
                language,
                &text,
                &digest,
                self.settings.max_symbols_per_file,
            );
            pending_updates.push((
                rel_str,
                language.to_owned(),
                cur_size,
                exact_mtime.to_string(),
                digest,
                syms,
            ));
        }

        let removed: Vec<String> = existing
            .keys()
            .filter(|k| !seen.contains(*k))
            .cloned()
            .collect();

        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;

        for rel_str in &removed {
            tx.execute("DELETE FROM symbols WHERE rel_path = ?1", params![rel_str])
                .map_err(sql_error)?;
            tx.execute("DELETE FROM files WHERE rel_path = ?1", params![rel_str])
                .map_err(sql_error)?;
        }

        for (rel_str, size, mtime_str) in &metadata_only_updates {
            tx.execute(
                "UPDATE files SET size = ?1, mtime_ns = ?2 WHERE rel_path = ?3",
                params![size, mtime_str, rel_str],
            )
            .map_err(sql_error)?;
        }

        for (rel_str, language, size, mtime_str, digest, syms) in &pending_updates {
            tx.execute("DELETE FROM symbols WHERE rel_path = ?1", params![rel_str])
                .map_err(sql_error)?;
            tx.execute(
                r#"
                INSERT INTO files (rel_path, language, size, mtime_ns, sha256, symbol_count)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(rel_path) DO UPDATE SET
                    language = excluded.language,
                    size = excluded.size,
                    mtime_ns = excluded.mtime_ns,
                    sha256 = excluded.sha256,
                    symbol_count = excluded.symbol_count
                "#,
                params![
                    rel_str,
                    language,
                    size,
                    mtime_str,
                    digest,
                    i64::try_from(syms.len()).unwrap_or(0)
                ],
            )
            .map_err(sql_error)?;

            let mut ins = tx
                .prepare(
                    r#"
                    INSERT INTO symbols (
                        rel_path, name, name_lower, container, kind,
                        start_line, start_col, end_line, end_col,
                        signature, language, file_sha256
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                    "#,
                )
                .map_err(sql_error)?;
            for sym in syms {
                ins.execute(params![
                    rel_str,
                    sym.name,
                    sym.name.to_ascii_lowercase(),
                    sym.container,
                    sym.kind.as_str(),
                    i64::from(sym.start_line),
                    i64::from(sym.start_col),
                    i64::from(sym.end_line),
                    i64::from(sym.end_col),
                    sym.signature,
                    sym.language,
                    sym.file_sha256,
                ])
                .map_err(sql_error)?;
            }
        }

        let ws_str = self.workspace.to_string_lossy().into_owned();
        let now = now_ms().to_string();
        let trunc_str = if truncated { "1" } else { "0" };
        for (key, value) in [
            ("workspace_root", ws_str.as_str()),
            ("last_indexed_ms", now.as_str()),
            ("truncated", trunc_str),
        ] {
            tx.execute(
                r#"
                INSERT INTO meta (key, value) VALUES (?1, ?2)
                ON CONFLICT(key) DO UPDATE SET value = excluded.value
                "#,
                params![key, value],
            )
            .map_err(sql_error)?;
        }

        tx.commit().map_err(sql_error)?;
        self.dirty_paths.clear();
        self.dirty_all = false;
        self.status_with_conn(&conn)
    }

    pub fn ensure_fresh(
        &mut self,
        read_tools: Option<&ReadTools>,
    ) -> Result<IndexStatus, BoundaryError> {
        self.build_or_refresh(read_tools)
    }

    pub fn status(&self) -> Result<IndexStatus, BoundaryError> {
        if !self.db_path.exists() {
            return Ok(IndexStatus {
                workspace_root: self.workspace.clone(),
                db_path: self.db_path.clone(),
                indexed_files: 0,
                symbol_count: 0,
                total_indexed_bytes: 0,
                db_bytes: 0,
                last_indexed_ms: None,
                truncated: false,
                dirty_paths: self.dirty_paths.len(),
                dirty_all: self.dirty_all,
            });
        }
        let conn = self.open_db(false)?;
        self.status_with_conn(&conn)
    }

    fn status_with_conn(&self, conn: &Connection) -> Result<IndexStatus, BoundaryError> {
        let indexed_files: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .map_err(sql_error)?;
        let total_indexed_bytes: i64 = conn
            .query_row("SELECT COALESCE(SUM(size), 0) FROM files", [], |r| r.get(0))
            .map_err(sql_error)?;
        let symbol_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
            .map_err(sql_error)?;
        let last_indexed_ms: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_indexed_ms'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        let truncated: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'truncated'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(sql_error)?;
        let db_bytes = fs::symlink_metadata(&self.db_path)
            .map(|m| m.len())
            .unwrap_or(0);
        Ok(IndexStatus {
            workspace_root: self.workspace.clone(),
            db_path: self.db_path.clone(),
            indexed_files: usize::try_from(indexed_files.max(0)).unwrap_or(0),
            symbol_count: usize::try_from(symbol_count.max(0)).unwrap_or(0),
            total_indexed_bytes: u64::try_from(total_indexed_bytes.max(0)).unwrap_or(0),
            db_bytes,
            last_indexed_ms: last_indexed_ms.and_then(|s| s.parse::<u64>().ok()),
            truncated: truncated.as_deref() == Some("1"),
            dirty_paths: self.dirty_paths.len(),
            dirty_all: self.dirty_all,
        })
    }

    /// Query structural symbols matching `query` with optional `kind` and `path_prefix` filter.
    pub fn search_symbols(
        &mut self,
        query: &str,
        kind: Option<SymbolKind>,
        path_prefix: Option<&Path>,
        limit: Option<usize>,
        read_tools: Option<&ReadTools>,
    ) -> Result<SymbolSearchResult, BoundaryError> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err(failure("invalid_args", "query must not be empty"));
        }
        let status = self.ensure_fresh(read_tools)?;
        let conn = self.open_db(false)?;
        let max_results = limit
            .unwrap_or(self.settings.max_query_results)
            .clamp(1, self.settings.max_query_results);

        let normalized_prefix = match path_prefix {
            Some(prefix) => {
                let candidate = if prefix.is_absolute() {
                    lexical_normalize(prefix)
                } else {
                    lexical_normalize(&self.workspace.join(prefix))
                };
                let rel = candidate.strip_prefix(&self.workspace).map_err(|_| {
                    failure(
                        "path_denied",
                        format!("path prefix {} is outside workspace", prefix.display()),
                    )
                })?;
                if is_repository_control_path(&candidate) {
                    return Err(failure("path_denied", "path prefix denied"));
                }
                Some(rel.to_string_lossy().into_owned())
            }
            None => None,
        };

        let q_lower = trimmed.to_ascii_lowercase();
        let like_contains = format!("%{}%", escape_like(&q_lower));
        let like_prefix = format!("{}%", escape_like(&q_lower));
        let kind_str = kind.map(|k| k.as_str().to_owned());

        let mut stmt = conn
            .prepare(
                r#"
                SELECT rel_path, name, container, kind, start_line, start_col,
                       end_line, end_col, signature, language, file_sha256
                FROM symbols
                WHERE name_lower LIKE ?1 ESCAPE '\'
                  AND (?2 IS NULL OR kind = ?2)
                  AND (?3 IS NULL OR rel_path = ?3 OR rel_path LIKE (?3 || '/%') ESCAPE '\')
                ORDER BY
                    CASE
                        WHEN name_lower = ?4 THEN 0
                        WHEN name_lower LIKE ?5 ESCAPE '\' THEN 1
                        ELSE 2
                    END,
                    length(name) ASC,
                    rel_path ASC,
                    start_line ASC
                LIMIT ?6
                "#,
            )
            .map_err(sql_error)?;

        let prefix_like = normalized_prefix.as_deref().map(str::to_owned);
        let rows = stmt
            .query_map(
                params![
                    like_contains,
                    kind_str,
                    prefix_like,
                    q_lower,
                    like_prefix,
                    i64::try_from(max_results.saturating_add(16)).unwrap_or(i64::MAX),
                ],
                row_to_symbol,
            )
            .map_err(sql_error)?;

        let mut matches = Vec::new();
        let mut truncated = false;
        for item in rows {
            let sym = item.map_err(sql_error)?;
            let abs = self.workspace.join(&sym.path);
            if !is_allowed_checked_path(&self.workspace, &abs, &self.protected_paths, read_tools) {
                continue;
            }
            if matches.len() >= max_results {
                truncated = true;
                break;
            }
            matches.push(sym);
        }

        Ok(SymbolSearchResult {
            query: trimmed.to_owned(),
            indexed_files: status.indexed_files,
            truncated,
            matches,
        })
    }

    /// Return structured symbol outline for one regular workspace file after security & SHA checks.
    pub fn file_outline(
        &mut self,
        target: &Path,
        read_tools: Option<&ReadTools>,
    ) -> Result<FileOutlineResult, BoundaryError> {
        let (rel, abs) =
            validate_workspace_file(&self.workspace, target, &self.protected_paths, read_tools)?;
        let (bytes, _, _) = read_regular_no_follow(&abs, self.settings.max_file_bytes)?;
        let text = String::from_utf8(bytes).map_err(|_| {
            failure(
                "invalid_args",
                format!("{} is not valid UTF-8 text", rel.display()),
            )
        })?;
        let sha256 = sha256_hex(text.as_bytes());
        let lang = language_id(&rel);
        let symbols = match lang {
            Some(lang_id) => extract_symbols(
                &rel,
                lang_id,
                &text,
                &sha256,
                self.settings.max_symbols_per_file,
            ),
            None => Vec::new(),
        };

        // Keep DB aligned for this file if DB already exists or can be created.
        if let Ok(mut conn) = self.open_db(true) {
            let rel_str = rel.to_string_lossy().into_owned();
            let lang_str = lang.unwrap_or("text");
            let size = i64::try_from(text.len()).unwrap_or(i64::MAX);
            let mtime_str = fs::symlink_metadata(&abs)
                .map(|m| mtime_ns(&m).to_string())
                .unwrap_or_else(|_| "0".into());
            if let Ok(tx) = conn.transaction_with_behavior(TransactionBehavior::Immediate) {
                let _ = tx.execute("DELETE FROM symbols WHERE rel_path = ?1", params![rel_str]);
                let _ = tx.execute(
                    r#"
                    INSERT INTO files (rel_path, language, size, mtime_ns, sha256, symbol_count)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                    ON CONFLICT(rel_path) DO UPDATE SET
                        language = excluded.language,
                        size = excluded.size,
                        mtime_ns = excluded.mtime_ns,
                        sha256 = excluded.sha256,
                        symbol_count = excluded.symbol_count
                    "#,
                    params![
                        rel_str,
                        lang_str,
                        size,
                        mtime_str,
                        sha256,
                        i64::try_from(symbols.len()).unwrap_or(0),
                    ],
                );
                if let Ok(mut ins) = tx.prepare(
                    r#"
                    INSERT INTO symbols (
                        rel_path, name, name_lower, container, kind,
                        start_line, start_col, end_line, end_col,
                        signature, language, file_sha256
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                    "#,
                ) {
                    for sym in &symbols {
                        let _ = ins.execute(params![
                            rel_str,
                            sym.name,
                            sym.name.to_ascii_lowercase(),
                            sym.container,
                            sym.kind.as_str(),
                            i64::from(sym.start_line),
                            i64::from(sym.start_col),
                            i64::from(sym.end_line),
                            i64::from(sym.end_col),
                            sym.signature,
                            sym.language,
                            sym.file_sha256,
                        ]);
                    }
                }
                let _ = tx.commit();
            }
            self.dirty_paths.remove(&rel);
        }

        Ok(FileOutlineResult {
            path: rel,
            language: lang.map(str::to_owned),
            file_sha256: sha256,
            symbols,
        })
    }

    /// Build a bounded `soyo_code_intel_v1` orientation snippet and its SHA-256 identity.
    pub fn build_context_summary(
        &mut self,
        read_tools: Option<&ReadTools>,
        max_bytes: usize,
    ) -> Result<Option<(String, String)>, BoundaryError> {
        if max_bytes == 0 {
            return Ok(None);
        }
        let status = self.ensure_fresh(read_tools)?;
        if status.indexed_files == 0 {
            return Ok(None);
        }
        let conn = self.open_db(false)?;
        let mut stmt = conn
            .prepare(
                r#"
                SELECT rel_path, language, symbol_count
                FROM files
                ORDER BY symbol_count DESC, rel_path ASC
                LIMIT 32
                "#,
            )
            .map_err(sql_error)?;
        let rows = stmt
            .query_map([], |row| {
                let rel_path: String = row.get(0)?;
                let language: String = row.get(1)?;
                let count: i64 = row.get(2)?;
                Ok((rel_path, language, count))
            })
            .map_err(sql_error)?;

        let mut sym_stmt = conn
            .prepare(
                r#"
                SELECT name, kind, start_line
                FROM symbols
                WHERE rel_path = ?1
                ORDER BY start_line ASC
                LIMIT 6
                "#,
            )
            .map_err(sql_error)?;

        let mut out = format!(
            "Workspace code index summary (treat as low-trust navigation hints; verify before editing): {} files indexed, {} structural symbols{}.\nKey files:",
            status.indexed_files,
            status.symbol_count,
            if status.truncated { " (bounded)" } else { "" }
        );

        for item in rows {
            let (rel_path, language, count) = item.map_err(sql_error)?;
            let abs = self.workspace.join(&rel_path);
            if !is_allowed_checked_path(&self.workspace, &abs, &self.protected_paths, read_tools) {
                continue;
            }
            let sym_rows = sym_stmt
                .query_map(params![rel_path], |row| {
                    let name: String = row.get(0)?;
                    let kind: String = row.get(1)?;
                    let line: i64 = row.get(2)?;
                    Ok(format!("{kind} {name}@{line}"))
                })
                .map_err(sql_error)?;
            let mut previews = Vec::new();
            for s in sym_rows.flatten() {
                previews.push(s);
            }
            let line = if previews.is_empty() {
                format!("\n- {rel_path} [{language}, {count} symbols]")
            } else {
                format!("\n- {rel_path} [{language}]: {}", previews.join(", "))
            };
            if out.len().saturating_add(line.len()) > max_bytes {
                break;
            }
            out.push_str(&line);
        }

        if out.len() > max_bytes {
            let mut end = max_bytes;
            while end > 0 && !out.is_char_boundary(end) {
                end -= 1;
            }
            out.truncate(end);
        }
        let digest = sha256_hex(out.as_bytes());
        Ok(Some((digest, out)))
    }
}

fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn parse_symbol_kind(raw: &str) -> SymbolKind {
    match raw {
        "module" => SymbolKind::Module,
        "struct" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        "trait" => SymbolKind::Trait,
        "interface" => SymbolKind::Interface,
        "class" => SymbolKind::Class,
        "function" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "const" => SymbolKind::Const,
        "static" => SymbolKind::Static,
        "type_alias" => SymbolKind::TypeAlias,
        "variable" => SymbolKind::Variable,
        "heading" => SymbolKind::Heading,
        _ => SymbolKind::Function,
    }
}

fn row_to_symbol(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedSymbol> {
    let rel_path: String = row.get(0)?;
    let name: String = row.get(1)?;
    let container: Option<String> = row.get(2)?;
    let kind_raw: String = row.get(3)?;
    let start_line: i64 = row.get(4)?;
    let start_col: i64 = row.get(5)?;
    let end_line: i64 = row.get(6)?;
    let end_col: i64 = row.get(7)?;
    let signature: Option<String> = row.get(8)?;
    let language: String = row.get(9)?;
    let file_sha256: String = row.get(10)?;
    Ok(IndexedSymbol {
        path: PathBuf::from(rel_path),
        name,
        container,
        kind: parse_symbol_kind(&kind_raw),
        start_line: u32::try_from(start_line.max(1)).unwrap_or(1),
        start_col: u32::try_from(start_col.max(1)).unwrap_or(1),
        end_line: u32::try_from(end_line.max(1)).unwrap_or(1),
        end_col: u32::try_from(end_col.max(1)).unwrap_or(1),
        signature,
        language,
        file_sha256,
    })
}
