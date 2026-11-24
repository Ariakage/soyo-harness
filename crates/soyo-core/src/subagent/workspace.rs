//! Workspace isolation, diffing, conflict-checked merging, and cleanup for F-09 subagents.

use super::{
    ChangeKind, SubagentConflict, SubagentDiffReport, SubagentFileChange, SubagentLimits,
    SubagentMergeReceipt, WorkspaceIsolationKind, validate_relative_workspace_path,
};
use crate::{
    contract::{BoundaryError, error_codes},
    execution::{NetworkPolicy, SandboxPolicy, enumerate_git_control_paths, normalize_path},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BaseFileEntry {
    pub sha256: String,
    pub bytes: u64,
    pub mode: u32,
}

#[derive(Debug)]
pub struct IsolatedWorkspace {
    pub kind: WorkspaceIsolationKind,
    pub outer_dir: Option<PathBuf>,
    pub child_dir: PathBuf,
    pub parent_workspace: PathBuf,
    pub git_common_dir: Option<PathBuf>,
    pub base_manifest: BTreeMap<String, BaseFileEntry>,
}

#[derive(Debug)]
pub enum MergeOutcome {
    Applied(Vec<SubagentMergeReceipt>),
    Conflict(Vec<SubagentConflict>),
}

fn failure(code: &str, message: impl Into<String>) -> BoundaryError {
    BoundaryError::new(code, message)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_regular_file_bounded(
    path: &Path,
    max_bytes: u64,
) -> Result<(Vec<u8>, BaseFileEntry), BoundaryError> {
    let meta = fs::symlink_metadata(path).map_err(|err| {
        failure(
            error_codes::SUBAGENT_WORKSPACE_ERROR,
            format!("cannot inspect {}: {err}", path.display()),
        )
    })?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(failure(
            error_codes::PATH_DENIED,
            "symlinks and special files are not allowed in subagent workspace operations",
        ));
    }
    if meta.len() > max_bytes {
        return Err(failure(
            error_codes::SUBAGENT_LIMIT,
            "workspace file exceeds subagent byte budget",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(path).map_err(|err| {
        failure(
            error_codes::SUBAGENT_WORKSPACE_ERROR,
            format!("cannot open {}: {err}", path.display()),
        )
    })?;
    let opened_meta = file.metadata().map_err(|err| {
        failure(
            error_codes::SUBAGENT_WORKSPACE_ERROR,
            format!("cannot stat {}: {err}", path.display()),
        )
    })?;
    if !opened_meta.is_file() || (opened_meta.dev(), opened_meta.ino()) != (meta.dev(), meta.ino())
    {
        return Err(failure(
            error_codes::PATH_DENIED,
            "file changed during read",
        ));
    }
    let mut buf = Vec::with_capacity(usize::try_from(opened_meta.len()).unwrap_or(0));
    std::io::Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|err| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                format!("failed reading {}: {err}", path.display()),
            )
        })?;
    if buf.len() as u64 > max_bytes {
        return Err(failure(
            error_codes::SUBAGENT_LIMIT,
            "workspace file exceeds subagent byte budget",
        ));
    }
    let entry = BaseFileEntry {
        sha256: sha256_hex(&buf),
        bytes: buf.len() as u64,
        mode: opened_meta.mode(),
    };
    Ok((buf, entry))
}

fn check_no_symlinks_under(root: &Path, rel: &Path) -> Result<(), BoundaryError> {
    let mut cursor = root.to_path_buf();
    for component in rel.components() {
        cursor.push(component);
        match fs::symlink_metadata(&cursor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(failure(
                        error_codes::PATH_DENIED,
                        "symlink targets are forbidden in subagent operations",
                    ));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(failure(
                    error_codes::SUBAGENT_WORKSPACE_ERROR,
                    format!("cannot inspect {}: {err}", cursor.display()),
                ));
            }
        }
    }
    Ok(())
}

fn path_overlaps_protected(abs: &Path, protected: &[PathBuf]) -> bool {
    protected
        .iter()
        .any(|p| abs.starts_with(p) || p.starts_with(abs))
}

fn select_scratch_outer_dir(
    parent_workspace: &Path,
    protected_paths: &[PathBuf],
    write_roots: &[PathBuf],
) -> Result<PathBuf, BoundaryError> {
    let mut last_error: Option<std::io::Error> = None;
    for raw_base in [std::env::temp_dir(), PathBuf::from("/var/tmp")] {
        let Ok(base) = fs::canonicalize(&raw_base) else {
            continue;
        };
        if !base.is_dir() {
            continue;
        }
        for _ in 0..4 {
            let dir_name = format!("soyo-sub-{}", Uuid::now_v7().simple());
            let candidate = base.join(&dir_name);
            if candidate.starts_with(parent_workspace)
                || parent_workspace.starts_with(&candidate)
                || write_roots
                    .iter()
                    .any(|root| candidate.starts_with(root) || root.starts_with(&candidate))
                || path_overlaps_protected(&candidate, protected_paths)
            {
                continue;
            }
            match fs::DirBuilder::new().mode(0o700).create(&candidate) {
                Ok(()) => {
                    return fs::canonicalize(&candidate).map_err(|err| {
                        failure(
                            error_codes::SUBAGENT_WORKSPACE_ERROR,
                            format!("cannot resolve subagent scratch dir: {err}"),
                        )
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_error = Some(err);
                    continue;
                }
                Err(err) => {
                    last_error = Some(err);
                    break;
                }
            }
        }
    }
    let detail = last_error.map(|err| format!(": {err}")).unwrap_or_default();
    Err(failure(
        error_codes::SUBAGENT_WORKSPACE_ERROR,
        format!(
            "cannot allocate private subagent scratch directory outside protected/writable roots{detail}"
        ),
    ))
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<std::process::Output, BoundaryError> {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                format!("git execution failed: {err}"),
            )
        })
}

fn detect_git_repo(parent_workspace: &Path) -> Option<PathBuf> {
    let inside = run_git(parent_workspace, &["rev-parse", "--is-inside-work-tree"]).ok()?;
    if !inside.status.success() || String::from_utf8_lossy(&inside.stdout).trim() != "true" {
        return None;
    }
    let head = run_git(parent_workspace, &["rev-parse", "--verify", "HEAD"]).ok()?;
    if !head.status.success() {
        return None;
    }
    let toplevel = run_git(parent_workspace, &["rev-parse", "--show-toplevel"]).ok()?;
    if !toplevel.status.success() {
        return None;
    }
    let toplevel_path = fs::canonicalize(String::from_utf8_lossy(&toplevel.stdout).trim()).ok()?;
    if toplevel_path != parent_workspace {
        return None;
    }
    let common = run_git(parent_workspace, &["rev-parse", "--git-common-dir"]).ok()?;
    if !common.status.success() {
        return None;
    }
    let raw_common = PathBuf::from(String::from_utf8_lossy(&common.stdout).trim());
    let abs_common = if raw_common.is_absolute() {
        raw_common
    } else {
        parent_workspace.join(raw_common)
    };
    fs::canonicalize(abs_common).ok()
}

fn copy_file_preserving_mode(src: &Path, dst: &Path, mode: u32) -> Result<(), BoundaryError> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                format!("cannot create parent dir {}: {err}", parent.display()),
            )
        })?;
    }
    let mut in_file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(src)
        .map_err(|err| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                format!("cannot read {}: {err}", src.display()),
            )
        })?;
    let mut out_file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dst)
        .map_err(|err| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                format!("cannot write {}: {err}", dst.display()),
            )
        })?;
    std::io::copy(&mut in_file, &mut out_file).map_err(|err| {
        failure(
            error_codes::SUBAGENT_WORKSPACE_ERROR,
            format!("cannot copy {}: {err}", src.display()),
        )
    })?;
    let _ = out_file.set_permissions(fs::Permissions::from_mode(mode & 0o777));
    Ok(())
}

fn overlay_uncommitted_git_changes(
    parent_workspace: &Path,
    child_dir: &Path,
    protected_paths: &[PathBuf],
    limits: &SubagentLimits,
) -> Result<(), BoundaryError> {
    let status = run_git(
        parent_workspace,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    if !status.status.success() {
        return Err(failure(
            error_codes::SUBAGENT_WORKSPACE_ERROR,
            "failed inspecting uncommitted git workspace changes",
        ));
    }
    let mut bytes_remaining = limits.max_workspace_bytes;
    let mut fields = status.stdout.split(|b| *b == 0);
    while let Some(raw_entry) = fields.next() {
        if raw_entry.is_empty() {
            continue;
        }
        if raw_entry.len() < 4 || raw_entry[2] != b' ' {
            continue;
        }
        let code_x = raw_entry[0];
        let code_y = raw_entry[1];
        let rel_str = String::from_utf8_lossy(&raw_entry[3..]).into_owned();
        if code_x == b'R' {
            if let Some(raw_old) = fields.next() {
                let old_str = String::from_utf8_lossy(raw_old);
                if let Ok(old_rel) = validate_relative_workspace_path(&old_str) {
                    let old_parent = parent_workspace.join(&old_rel);
                    let old_child = child_dir.join(&old_rel);
                    if !old_parent.exists() && old_child.exists() {
                        let _ = fs::remove_file(&old_child);
                    }
                }
            }
        } else if code_x == b'C' {
            let _ = fields.next();
        }
        let Ok(rel) = validate_relative_workspace_path(&rel_str) else {
            continue;
        };
        let parent_target = parent_workspace.join(&rel);
        if path_overlaps_protected(&parent_target, protected_paths) {
            continue;
        }
        let child_target = child_dir.join(&rel);
        if matches!(code_x, b'D') || matches!(code_y, b'D') {
            if !parent_target.exists() && child_target.exists() {
                let _ = fs::remove_file(&child_target);
            }
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&parent_target) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            continue;
        }
        if meta.len() > bytes_remaining {
            return Err(failure(
                error_codes::SUBAGENT_LIMIT,
                "uncommitted parent changes exceed subagent workspace byte limit",
            ));
        }
        bytes_remaining = bytes_remaining.saturating_sub(meta.len());
        copy_file_preserving_mode(&parent_target, &child_target, meta.mode())?;
    }
    Ok(())
}

fn scan_workspace_manifest(
    root: &Path,
    protected_paths: &[PathBuf],
    respect_gitignore: bool,
    limits: &SubagentLimits,
) -> Result<BTreeMap<String, BaseFileEntry>, BoundaryError> {
    let mut manifest = BTreeMap::new();
    let mut total_bytes: u64 = 0;

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .follow_links(false)
        .parents(false)
        .git_ignore(respect_gitignore)
        .git_global(false)
        .git_exclude(false)
        .filter_entry(|entry| entry.file_name() != ".git");

    for item in builder.build() {
        let entry = item.map_err(|err| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                format!("workspace scan error: {err}"),
            )
        })?;
        let path = entry.path();
        if path == root {
            continue;
        }
        let meta = fs::symlink_metadata(path).map_err(|err| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                format!("cannot stat {}: {err}", path.display()),
            )
        })?;
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            continue;
        }
        let rel = path.strip_prefix(root).map_err(|_| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                "scanned path escaped workspace root",
            )
        })?;
        let rel_str = rel.to_str().ok_or_else(|| {
            failure(
                error_codes::INVALID_PATH,
                "workspace contains non-UTF-8 file path",
            )
        })?;
        if validate_relative_workspace_path(rel_str).is_err() {
            continue;
        }
        if path_overlaps_protected(path, protected_paths) {
            continue;
        }
        if manifest.len() >= limits.max_workspace_files {
            return Err(failure(
                error_codes::SUBAGENT_LIMIT,
                "workspace file count exceeds subagent limit",
            ));
        }
        let remaining = limits.max_workspace_bytes.saturating_sub(total_bytes);
        let (_, info) = read_regular_file_bounded(path, remaining)?;
        total_bytes = total_bytes.saturating_add(info.bytes);
        manifest.insert(rel_str.to_owned(), info);
    }

    Ok(manifest)
}

fn copy_directory_workspace(
    parent_workspace: &Path,
    child_dir: &Path,
    protected_paths: &[PathBuf],
    limits: &SubagentLimits,
) -> Result<BTreeMap<String, BaseFileEntry>, BoundaryError> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(child_dir)
        .map_err(|err| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                format!("cannot create child workspace dir: {err}"),
            )
        })?;
    let manifest = scan_workspace_manifest(parent_workspace, protected_paths, false, limits)?;
    for (rel, entry) in &manifest {
        let src = parent_workspace.join(rel);
        let dst = child_dir.join(rel);
        copy_file_preserving_mode(&src, &dst, entry.mode)?;
    }
    Ok(manifest)
}

fn create_isolated_at(
    parent_canon: &Path,
    outer_dir: &Path,
    protected_paths: &[PathBuf],
    limits: &SubagentLimits,
) -> Result<IsolatedWorkspace, BoundaryError> {
    let child_dir = outer_dir.join("ws");
    let mut created_worktree = false;
    let setup = (|| -> Result<IsolatedWorkspace, BoundaryError> {
        if let Some(git_common_dir) = detect_git_repo(parent_canon) {
            let child_str = child_dir.to_str().ok_or_else(|| {
                failure(
                    error_codes::INVALID_PATH,
                    "subagent worktree path is not UTF-8",
                )
            })?;
            let add = run_git(
                parent_canon,
                &["worktree", "add", "--detach", child_str, "HEAD"],
            )?;
            if add.status.success() {
                created_worktree = true;
                let child_canon = fs::canonicalize(&child_dir).map_err(|err| {
                    failure(
                        error_codes::SUBAGENT_WORKSPACE_ERROR,
                        format!("cannot canonicalize worktree: {err}"),
                    )
                })?;
                overlay_uncommitted_git_changes(
                    parent_canon,
                    &child_canon,
                    protected_paths,
                    limits,
                )?;
                let base_manifest =
                    scan_workspace_manifest(&child_canon, protected_paths, true, limits)?;
                return Ok(IsolatedWorkspace {
                    kind: WorkspaceIsolationKind::GitWorktree,
                    outer_dir: Some(outer_dir.to_path_buf()),
                    child_dir: child_canon,
                    parent_workspace: parent_canon.to_path_buf(),
                    git_common_dir: Some(git_common_dir),
                    base_manifest,
                });
            }
        }
        let base_manifest =
            copy_directory_workspace(parent_canon, &child_dir, protected_paths, limits)?;
        let child_canon = fs::canonicalize(&child_dir).map_err(|err| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                format!("cannot canonicalize workspace copy: {err}"),
            )
        })?;
        Ok(IsolatedWorkspace {
            kind: WorkspaceIsolationKind::DirectoryCopy,
            outer_dir: Some(outer_dir.to_path_buf()),
            child_dir: child_canon,
            parent_workspace: parent_canon.to_path_buf(),
            git_common_dir: None,
            base_manifest,
        })
    })();

    if setup.is_err() {
        let _ = cleanup_paths(
            if created_worktree {
                WorkspaceIsolationKind::GitWorktree
            } else {
                WorkspaceIsolationKind::DirectoryCopy
            },
            parent_canon,
            Some(outer_dir),
            &child_dir,
        );
    }
    setup
}

impl IsolatedWorkspace {
    pub fn read_only(parent_workspace: &Path) -> Result<Self, BoundaryError> {
        let canonical = fs::canonicalize(parent_workspace).map_err(|err| {
            failure(
                error_codes::INVALID_WORKSPACE,
                format!("parent workspace unavailable: {err}"),
            )
        })?;
        Ok(Self {
            kind: WorkspaceIsolationKind::ReadOnly,
            outer_dir: None,
            child_dir: canonical.clone(),
            parent_workspace: canonical,
            git_common_dir: None,
            base_manifest: BTreeMap::new(),
        })
    }

    pub fn create_isolated(
        parent_workspace: &Path,
        protected_paths: &[PathBuf],
        write_roots: &[PathBuf],
        limits: &SubagentLimits,
    ) -> Result<Self, BoundaryError> {
        let parent_canon = fs::canonicalize(parent_workspace).map_err(|err| {
            failure(
                error_codes::INVALID_WORKSPACE,
                format!("parent workspace unavailable: {err}"),
            )
        })?;
        let outer_dir = select_scratch_outer_dir(&parent_canon, protected_paths, write_roots)?;
        create_isolated_at(&parent_canon, &outer_dir, protected_paths, limits)
    }

    pub fn compute_diff(
        &self,
        subagent_id: &str,
        protected_paths: &[PathBuf],
        include_patch: bool,
        limits: &SubagentLimits,
    ) -> Result<SubagentDiffReport, BoundaryError> {
        if self.kind == WorkspaceIsolationKind::ReadOnly {
            return Ok(SubagentDiffReport {
                subagent_id: subagent_id.to_owned(),
                isolation_kind: self.kind,
                changed_files: Vec::new(),
                patch: String::new(),
                patch_truncated: false,
            });
        }
        let respect_gitignore = self.kind == WorkspaceIsolationKind::GitWorktree;
        let current_manifest =
            scan_workspace_manifest(&self.child_dir, protected_paths, respect_gitignore, limits)?;
        let mut all_paths = BTreeSet::new();
        all_paths.extend(self.base_manifest.keys().cloned());
        all_paths.extend(current_manifest.keys().cloned());

        let mut changes = Vec::new();
        let mut patch = String::new();
        let mut patch_truncated = false;

        for rel in all_paths {
            let base = self.base_manifest.get(&rel);
            let curr = current_manifest.get(&rel);
            let kind = match (base, curr) {
                (None, Some(_)) => ChangeKind::Added,
                (Some(_), None) => ChangeKind::Deleted,
                (Some(b), Some(c)) if b.sha256 != c.sha256 => ChangeKind::Modified,
                _ => continue,
            };
            if changes.len() >= limits.max_changed_files {
                return Err(failure(
                    error_codes::SUBAGENT_LIMIT,
                    "subagent modified file count exceeds limit",
                ));
            }
            let base_sha = base.map(|b| b.sha256.clone());
            let sub_sha = curr.map(|c| c.sha256.clone());
            let parent_sha = inspect_parent_sha256(
                &self.parent_workspace,
                Path::new(&rel),
                limits.max_workspace_bytes,
            )?;
            let has_conflict = parent_sha != sub_sha && parent_sha != base_sha;
            let bytes = curr.map_or(0, |c| c.bytes);

            if include_patch && !patch_truncated {
                let piece = render_file_patch(
                    &rel,
                    kind,
                    &self.parent_workspace,
                    &self.child_dir,
                    limits.max_workspace_bytes,
                );
                if patch.len().saturating_add(piece.len()) > limits.max_patch_bytes {
                    let remaining = limits.max_patch_bytes.saturating_sub(patch.len());
                    let mut end = remaining.min(piece.len());
                    while !piece.is_char_boundary(end) {
                        end = end.saturating_sub(1);
                    }
                    patch.push_str(&piece[..end]);
                    patch_truncated = true;
                } else {
                    patch.push_str(&piece);
                }
            }

            changes.push(SubagentFileChange {
                path: rel,
                kind,
                base_sha256: base_sha,
                new_sha256: sub_sha,
                bytes,
                has_conflict,
            });
        }

        Ok(SubagentDiffReport {
            subagent_id: subagent_id.to_owned(),
            isolation_kind: self.kind,
            changed_files: changes,
            patch,
            patch_truncated,
        })
    }

    /// Resolve all target workspace paths that `merge` will touch so the
    /// dispatcher can bind them into `Operation::subagent_merge` before running.
    pub fn resolve_merge_targets(
        &self,
        selected: Option<&[String]>,
        protected_paths: &[PathBuf],
        limits: &SubagentLimits,
    ) -> Result<Vec<PathBuf>, BoundaryError> {
        if self.kind == WorkspaceIsolationKind::ReadOnly {
            return Err(failure(
                error_codes::INVALID_ARGS,
                "read_only subagents have no isolated changes to merge",
            ));
        }
        let diff = self.compute_diff("merge", protected_paths, false, limits)?;
        let selected_set: Option<BTreeSet<&str>> =
            selected.map(|items| items.iter().map(String::as_str).collect());
        let mut targets = Vec::new();
        for change in &diff.changed_files {
            if let Some(filter) = &selected_set
                && !filter.contains(change.path.as_str())
            {
                continue;
            }
            let rel = validate_relative_workspace_path(&change.path)?;
            check_no_symlinks_under(&self.parent_workspace, &rel)?;
            let abs = self.parent_workspace.join(&rel);
            if path_overlaps_protected(&abs, protected_paths) {
                return Err(failure(
                    error_codes::PATH_DENIED,
                    "merge target addresses protected host storage",
                ));
            }
            targets.push(abs);
        }
        if let Some(filter) = &selected_set {
            let found: BTreeSet<&str> =
                diff.changed_files.iter().map(|c| c.path.as_str()).collect();
            if let Some(missing) = filter.iter().find(|p| !found.contains(*p)) {
                return Err(failure(
                    error_codes::INVALID_ARGS,
                    format!("path `{missing}` was not changed by this subagent"),
                ));
            }
        }
        if targets.is_empty() {
            return Err(failure(
                error_codes::INVALID_ARGS,
                "no changed files selected for subagent_merge",
            ));
        }
        Ok(targets)
    }

    /// Conflict-check every target file, then atomically write changes into the
    /// parent workspace. Zero files are modified if any conflict is detected.
    pub fn merge_into_parent(
        &self,
        selected: Option<&[String]>,
        allowed_write_roots: &[PathBuf],
        protected_paths: &[PathBuf],
        limits: &SubagentLimits,
    ) -> Result<MergeOutcome, BoundaryError> {
        if self.kind == WorkspaceIsolationKind::ReadOnly {
            return Err(failure(
                error_codes::INVALID_ARGS,
                "read_only subagents cannot merge changes",
            ));
        }
        let parent_canon = fs::canonicalize(&self.parent_workspace).map_err(|err| {
            failure(
                error_codes::SUBAGENT_WORKSPACE_ERROR,
                format!("parent workspace unavailable: {err}"),
            )
        })?;
        if parent_canon != self.parent_workspace {
            return Err(failure(
                error_codes::CONFLICT,
                "parent workspace identity changed before merge",
            ));
        }
        let diff = self.compute_diff("merge", protected_paths, false, limits)?;
        let selected_set: Option<BTreeSet<&str>> =
            selected.map(|items| items.iter().map(String::as_str).collect());

        struct PlannedMerge {
            rel: String,
            rel_path: PathBuf,
            kind: ChangeKind,
            parent_abs: PathBuf,
            pre_parent_sha256: Option<String>,
            subagent_sha256: Option<String>,
            bytes: Option<Vec<u8>>,
            mode: u32,
        }

        let mut plan = Vec::new();
        let mut conflicts = Vec::new();

        for change in diff.changed_files {
            if let Some(filter) = &selected_set
                && !filter.contains(change.path.as_str())
            {
                continue;
            }
            let rel_path = validate_relative_workspace_path(&change.path)?;
            check_no_symlinks_under(&self.parent_workspace, &rel_path)?;
            let parent_abs = self.parent_workspace.join(&rel_path);
            if path_overlaps_protected(&parent_abs, protected_paths)
                || !allowed_write_roots
                    .iter()
                    .any(|root| parent_abs.starts_with(root))
            {
                return Err(failure(
                    error_codes::PATH_DENIED,
                    "merge target is outside allowed parent write roots or hits protected storage",
                ));
            }
            let pre_parent_sha = inspect_parent_sha256(
                &self.parent_workspace,
                &rel_path,
                limits.max_workspace_bytes,
            )?;
            let sub_sha = change.new_sha256.clone();
            let base_sha = change.base_sha256.clone();
            if pre_parent_sha != sub_sha && pre_parent_sha != base_sha {
                conflicts.push(SubagentConflict {
                    path: change.path,
                    reason: "parent file changed since subagent fork".into(),
                    base_sha256: base_sha,
                    parent_sha256: pre_parent_sha,
                    subagent_sha256: sub_sha,
                });
                continue;
            }
            let (bytes, mode) = match change.kind {
                ChangeKind::Added | ChangeKind::Modified => {
                    let child_file = self.child_dir.join(&rel_path);
                    let (data, entry) =
                        read_regular_file_bounded(&child_file, limits.max_workspace_bytes)?;
                    (Some(data), entry.mode)
                }
                ChangeKind::Deleted => (None, 0o644),
            };
            plan.push(PlannedMerge {
                rel: change.path,
                rel_path,
                kind: change.kind,
                parent_abs,
                pre_parent_sha256: pre_parent_sha,
                subagent_sha256: sub_sha,
                bytes,
                mode,
            });
        }

        if let Some(filter) = &selected_set {
            let planned_paths: BTreeSet<&str> = plan
                .iter()
                .map(|p| p.rel.as_str())
                .chain(conflicts.iter().map(|c| c.path.as_str()))
                .collect();
            if let Some(missing) = filter.iter().find(|p| !planned_paths.contains(*p)) {
                return Err(failure(
                    error_codes::INVALID_ARGS,
                    format!("path `{missing}` was not changed by this subagent"),
                ));
            }
        }
        if !conflicts.is_empty() {
            return Ok(MergeOutcome::Conflict(conflicts));
        }

        let mut receipts = Vec::with_capacity(plan.len());
        for item in plan {
            check_no_symlinks_under(&self.parent_workspace, &item.rel_path)?;
            if item.pre_parent_sha256 == item.subagent_sha256 {
                receipts.push(SubagentMergeReceipt {
                    path: item.rel,
                    operation: "unchanged".into(),
                    pre_parent_sha256: item.pre_parent_sha256,
                    post_parent_sha256: item.subagent_sha256,
                });
                continue;
            }
            match item.kind {
                ChangeKind::Added | ChangeKind::Modified => {
                    let data = item.bytes.as_deref().unwrap_or(&[]);
                    atomic_write_file(&item.parent_abs, data, item.mode)?;
                    let (_, verified) =
                        read_regular_file_bounded(&item.parent_abs, limits.max_workspace_bytes)?;
                    if Some(verified.sha256.as_str()) != item.subagent_sha256.as_deref() {
                        return Err(failure(
                            error_codes::CONFLICT,
                            format!("post-merge digest verification failed for {}", item.rel),
                        ));
                    }
                    receipts.push(SubagentMergeReceipt {
                        path: item.rel,
                        operation: if item.kind == ChangeKind::Added {
                            "created"
                        } else {
                            "modified"
                        }
                        .into(),
                        pre_parent_sha256: item.pre_parent_sha256,
                        post_parent_sha256: Some(verified.sha256),
                    });
                }
                ChangeKind::Deleted => {
                    match fs::symlink_metadata(&item.parent_abs) {
                        Ok(meta) => {
                            if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                                return Err(failure(
                                    error_codes::PATH_DENIED,
                                    "cannot delete non-regular or symlinked target",
                                ));
                            }
                            fs::remove_file(&item.parent_abs).map_err(|err| {
                                failure(
                                    error_codes::SUBAGENT_WORKSPACE_ERROR,
                                    format!("cannot delete {}: {err}", item.rel),
                                )
                            })?;
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => {
                            return Err(failure(
                                error_codes::SUBAGENT_WORKSPACE_ERROR,
                                format!("cannot stat {}: {err}", item.rel),
                            ));
                        }
                    }
                    receipts.push(SubagentMergeReceipt {
                        path: item.rel,
                        operation: "deleted".into(),
                        pre_parent_sha256: item.pre_parent_sha256,
                        post_parent_sha256: None,
                    });
                }
            }
        }
        Ok(MergeOutcome::Applied(receipts))
    }

    pub fn cleanup(&self) -> Result<(), BoundaryError> {
        if self.kind == WorkspaceIsolationKind::ReadOnly {
            return Ok(());
        }
        cleanup_paths(
            self.kind,
            &self.parent_workspace,
            self.outer_dir.as_deref(),
            &self.child_dir,
        )
    }

    /// Derive a non-escalating `SandboxPolicy` for the child subagent.
    pub fn derive_child_sandbox(
        &self,
        parent: &SandboxPolicy,
        allow_exec: bool,
        home: Option<&Path>,
    ) -> SandboxPolicy {
        let mut policy = parent.clone();
        policy.browser = None;
        policy.read_files = Vec::new();
        if self.kind == WorkspaceIsolationKind::ReadOnly {
            policy.write_roots = Vec::new();
            return policy;
        }
        let allow_writes = allow_exec || self.kind != WorkspaceIsolationKind::ReadOnly;
        policy.workspace = self.child_dir.clone();
        policy.write_roots = if allow_writes {
            vec![self.child_dir.clone()]
        } else {
            Vec::new()
        };
        // Strip parent workspace and writable roots so the child cannot inspect
        // sibling parent files via `exec`, while preserving system/toolchain roots.
        policy.read_roots.retain(|root| {
            *root != self.parent_workspace
                && !parent.write_roots.iter().any(|w| root.starts_with(w))
        });
        if !policy.read_roots.contains(&self.child_dir) {
            policy.read_roots.push(self.child_dir.clone());
        }
        if let Some(common_dir) = &self.git_common_dir
            && allow_exec
            && !policy.read_roots.contains(common_dir)
        {
            policy.read_roots.push(common_dir.clone());
        }
        let git_control = enumerate_git_control_paths(std::slice::from_ref(&self.child_dir), home);
        policy.write_denied_paths = git_control.write_denied;
        // Network strictly stays <= parent policy (Proxy or Deny).
        policy.network = match &parent.network {
            NetworkPolicy::Deny => NetworkPolicy::Deny,
            NetworkPolicy::Proxy(ep) => NetworkPolicy::Proxy(ep.clone()),
            NetworkPolicy::LoopbackPort { .. } => NetworkPolicy::Deny,
        };
        policy
    }
}

fn inspect_parent_sha256(
    parent_workspace: &Path,
    rel: &Path,
    max_bytes: u64,
) -> Result<Option<String>, BoundaryError> {
    check_no_symlinks_under(parent_workspace, rel)?;
    let abs = parent_workspace.join(rel);
    match fs::symlink_metadata(&abs) {
        Ok(_) => {
            let (_, entry) = read_regular_file_bounded(&abs, max_bytes)?;
            Ok(Some(entry.sha256))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(failure(
            error_codes::SUBAGENT_WORKSPACE_ERROR,
            format!("cannot inspect {}: {err}", abs.display()),
        )),
    }
}

fn render_file_patch(
    rel: &str,
    kind: ChangeKind,
    parent_workspace: &Path,
    child_dir: &Path,
    max_bytes: u64,
) -> String {
    let read_text = |root: &Path| -> String {
        let target = root.join(rel);
        let Ok((bytes, _)) = read_regular_file_bounded(&target, max_bytes.min(512 * 1024)) else {
            return String::new();
        };
        if bytes.contains(&0) {
            return "[binary content omitted]\n".into();
        }
        String::from_utf8(bytes).unwrap_or_else(|_| "[non-UTF-8 content omitted]\n".into())
    };
    let old = match kind {
        ChangeKind::Added => String::new(),
        ChangeKind::Modified | ChangeKind::Deleted => read_text(parent_workspace),
    };
    let new = match kind {
        ChangeKind::Deleted => String::new(),
        ChangeKind::Added | ChangeKind::Modified => read_text(child_dir),
    };
    let patch = diffy::create_patch(&old, &new);
    format!("--- a/{rel}\n+++ b/{rel}\n{patch}")
}

fn atomic_write_file(target: &Path, bytes: &[u8], mode: u32) -> Result<(), BoundaryError> {
    let parent = target.parent().ok_or_else(|| {
        failure(
            error_codes::INVALID_PATH,
            "target file has no parent directory",
        )
    })?;
    fs::create_dir_all(parent).map_err(|err| {
        failure(
            error_codes::SUBAGENT_WORKSPACE_ERROR,
            format!("cannot create parent directories: {err}"),
        )
    })?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| failure(error_codes::INVALID_PATH, "invalid target filename"))?;
    let tmp = parent.join(format!(".{name}.soyo-merge-{}", Uuid::now_v7().simple()));
    let write_res = (|| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .mode(mode & 0o777)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, target)?;
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(failure(
            error_codes::SUBAGENT_WORKSPACE_ERROR,
            format!("atomic merge write failed for {}: {err}", target.display()),
        ));
    }
    Ok(())
}

fn cleanup_paths(
    kind: WorkspaceIsolationKind,
    parent_workspace: &Path,
    outer_dir: Option<&Path>,
    child_dir: &Path,
) -> Result<(), BoundaryError> {
    if kind == WorkspaceIsolationKind::GitWorktree
        && child_dir.exists()
        && let Some(child_str) = child_dir.to_str()
    {
        let _ = run_git(
            parent_workspace,
            &["worktree", "remove", "--force", child_str],
        );
        let _ = run_git(parent_workspace, &["worktree", "prune"]);
    }
    if child_dir.exists() {
        fs::remove_dir_all(child_dir).map_err(|err| {
            failure(
                error_codes::OUTCOME_UNKNOWN,
                format!("subagent workspace cleanup failed: {err}"),
            )
        })?;
    }
    if let Some(outer) = outer_dir
        && outer.exists()
    {
        fs::remove_dir_all(outer).map_err(|err| {
            failure(
                error_codes::OUTCOME_UNKNOWN,
                format!("subagent scratch cleanup failed: {err}"),
            )
        })?;
    }
    if child_dir.exists() || outer_dir.is_some_and(Path::exists) {
        return Err(failure(
            error_codes::OUTCOME_UNKNOWN,
            "subagent workspace cleanup could not verify removal",
        ));
    }
    let _ = normalize_path(parent_workspace);
    Ok(())
}
