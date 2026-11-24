//! Scoped grants binding a `CredentialRef` to workspace identity, run kind, tool
//! names and expiry. Grants are recorded separately from the URL-bound
//! `CredentialScope`; a resolution requires both the exact scope match and an
//! applicable grant. Grant state lives in the same private directory as the
//! credential store (`grants/`), one file per grant.
use super::{CredentialRef, error};
use crate::contract::BoundaryError;
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::DirBuilder;
use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

const MAX_GRANT_BYTES: usize = 16 * 1024;
const MAX_GRANTS: usize = 512;
const MAX_TOOLS: usize = 64;
const MAX_WORKSPACES: usize = 64;

/// The run/task kind a grant applies to. `Tool` covers direct tool dispatch and
/// MCP server connections; `Subagent` covers spawned child agents; `Profile`
/// covers model profile API-key indirection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantRunKind {
    Tool,
    Subagent,
    Profile,
}

/// A durable authorization grant bound to one credential reference. All filters
/// are conjunctive: a request must satisfy the workspace set, the run kind, the
/// tool name set and the expiry. An empty `workspaces` or `tools` set means
/// "any"; `workspaces` values are `WorkspaceIdentity` ids (`ws_<digest>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialGrant {
    pub grant_id: Uuid,
    pub reference: CredentialRef,
    /// Empty = any workspace. Otherwise the workspace identity ids allowed.
    #[serde(default)]
    pub workspaces: BTreeSet<String>,
    /// Run kinds this grant applies to. Empty set is invalid at validate time.
    pub run_kinds: BTreeSet<GrantRunKind>,
    /// Empty = any tool name. Otherwise the exact tool names allowed.
    #[serde(default)]
    pub tools: BTreeSet<String>,
    /// RFC 3339 UTC expiry; `None` never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Server id this grant is bound to (usually the scope `server_id`).
    pub server_id: String,
}

impl CredentialGrant {
    pub fn validate(&self) -> Result<(), BoundaryError> {
        if self.run_kinds.is_empty()
            || self.run_kinds.len() > 3
            || self.workspaces.len() > MAX_WORKSPACES
            || self.tools.len() > MAX_TOOLS
            || self.workspaces.iter().any(|id| {
                crate::global_memory::WorkspaceIdentity { id: id.clone() }
                    .validate()
                    .is_err()
            })
            || self.tools.iter().any(|name| {
                name.is_empty()
                    || name.len() > 128
                    || name
                        .bytes()
                        .any(|b| !(b.is_ascii_alphanumeric() || b"_.-".contains(&b)))
            })
            || self.server_id.is_empty()
            || !crate::registry::valid_server_id(&self.server_id)
        {
            return Err(error("credential_grant", "Credential grant is invalid"));
        }
        if let Some(expires) = &self.expires_at {
            // RFC 3339 parse check; compare lexically later against now().
            if parse_rfc3339(expires).is_none() {
                return Err(error(
                    "credential_grant",
                    "Credential grant expiry must be RFC 3339 UTC",
                ));
            }
        }
        Ok(())
    }

    /// True when this grant authorizes `kind`, `tool` and `workspace` at `now`.
    /// `workspace` is `None` for grants that do not bind a workspace (e.g. a
    /// profile key used by the host before a run workspace is known) — those
    /// resolve only when the grant has an empty workspace set.
    pub fn applies(
        &self,
        kind: GrantRunKind,
        tool: Option<&str>,
        workspace: Option<&str>,
        now: &str,
    ) -> bool {
        if !self.run_kinds.contains(&kind) {
            return false;
        }
        if let Some(expires) = &self.expires_at
            && expires.as_str() <= now
        {
            return false;
        }
        if !self.tools.is_empty() {
            match tool {
                Some(name) if self.tools.contains(name) => {}
                _ => return false,
            }
        }
        if !self.workspaces.is_empty() {
            match workspace {
                Some(id) if self.workspaces.contains(id) => {}
                _ => return false,
            }
        }
        true
    }
}

/// File-backed grant registry in `<credentials>/grants/`. Shares the store's
/// private root. Entries are small; we rewrite the directory atomically per
/// grant and keep no extra index file.
#[derive(Clone)]
pub struct GrantStore {
    dir: Arc<Dir>,
}

impl std::fmt::Debug for GrantStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GrantStore([PRIVATE])")
    }
}

impl GrantStore {
    /// Open or create the `grants` subdirectory under an existing private
    /// credential root. `root` is the already-validated canonical path.
    pub fn open(root: &Path) -> Result<Self, BoundaryError> {
        let path = root.join("grants");
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => {
                return Err(error(
                    "credential_io",
                    "Grant directory could not be created",
                ));
            }
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .map_err(|_| {
                error(
                    "credential_permissions",
                    "Grant directory is unavailable or a symlink",
                )
            })?;
        let metadata = file
            .metadata()
            .map_err(|_| error("credential_io", "Grant directory metadata is unavailable"))?;
        use std::os::unix::fs::MetadataExt;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(error(
                "credential_permissions",
                "Grant directory must be owned by this user with mode 0700",
            ));
        }
        Ok(Self {
            dir: Arc::new(Dir::from_std_file(file)),
        })
    }

    fn read_grant(&self, name: &str) -> Result<Option<CredentialGrant>, BoundaryError> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let mut file = match self.dir.open_with(name, &options) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(error(
                    "credential_permissions",
                    "Grant file is unavailable or is a symlink",
                ));
            }
        };
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_GRANT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| error("credential_io", "Grant file could not be read"))?;
        if bytes.len() > MAX_GRANT_BYTES {
            return Err(error(
                "credential_limit",
                "Grant file exceeds its size limit",
            ));
        }
        let grant: CredentialGrant = serde_json::from_slice(&bytes)
            .map_err(|_| error("credential_corrupt", "Grant record is malformed"))?;
        grant.validate()?;
        Ok(Some(grant))
    }

    fn write_grant(&self, grant: &CredentialGrant) -> Result<(), BoundaryError> {
        grant.validate()?;
        let bytes = serde_json::to_vec(grant)
            .map_err(|_| error("credential_io", "Grant record could not be encoded"))?;
        let temporary = format!(".tmp-{}", Uuid::now_v7());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let outcome = (|| {
            let mut file = self
                .dir
                .open_with(&temporary, &options)
                .map_err(|_| error("credential_io", "Grant temporary file could not be created"))?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| error("credential_io", "Grant record could not be saved"))?;
            self.dir
                .rename(&temporary, &self.dir, format!("{}.json", grant.grant_id))
                .map_err(|_| error("credential_io", "Grant record replacement failed"))?;
            self.dir
                .try_clone()
                .map_err(|_| error("credential_io", "Grant directory could not be cloned"))?
                .into_std_file()
                .sync_all()
                .map_err(|_| {
                    error(
                        "credential_io",
                        "Grant commit durability could not be confirmed",
                    )
                })
        })();
        if outcome.is_err() {
            let _ = self.dir.remove_file(&temporary);
        }
        outcome
    }

    /// Insert a grant. The caller chooses the id; a colliding existing file is
    /// replaced atomically (rename-over), so repeated `grant` calls with the same
    /// id are idempotent.
    pub fn put(&self, grant: &CredentialGrant) -> Result<(), BoundaryError> {
        let existing = self.list_raw()?;
        if existing.len() >= MAX_GRANTS && !existing.iter().any(|g| g.grant_id == grant.grant_id) {
            return Err(error(
                "credential_limit",
                "Credential grant capacity has been reached",
            ));
        }
        self.write_grant(grant)
    }

    /// Remove one grant by id. Returns false when no such grant exists.
    pub fn remove(&self, grant_id: Uuid) -> Result<bool, BoundaryError> {
        let name = format!("{grant_id}.json");
        match self.dir.remove_file(&name) {
            Ok(()) => {
                let _ = self.dir.try_clone().map(|f| f.into_std_file().sync_all());
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(error("credential_io", "Grant removal failed")),
        }
    }

    /// Remove every grant bound to a credential reference (used on revoke).
    /// Returns how many grants were removed.
    pub fn remove_for(&self, reference: CredentialRef) -> Result<usize, BoundaryError> {
        let mut removed = 0;
        for grant in self.list_raw()? {
            if grant.reference == reference {
                let name = format!("{}.json", grant.grant_id);
                match self.dir.remove_file(&name) {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err(error("credential_io", "Grant removal failed")),
                }
            }
        }
        if removed > 0 {
            let _ = self.dir.try_clone().map(|f| f.into_std_file().sync_all());
        }
        Ok(removed)
    }

    /// List all stored grants, validating each record.
    pub fn list(&self) -> Result<Vec<CredentialGrant>, BoundaryError> {
        let mut grants = self.list_raw()?;
        grants.sort_by(|a, b| a.grant_id.cmp(&b.grant_id));
        Ok(grants)
    }

    fn list_raw(&self) -> Result<Vec<CredentialGrant>, BoundaryError> {
        let mut result = Vec::new();
        for entry in self
            .dir
            .entries()
            .map_err(|_| error("credential_io", "Grant directory cannot be listed"))?
        {
            let name = entry
                .map_err(|_| error("credential_io", "Grant directory entry is unavailable"))?
                .file_name();
            let Some(name) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            if name.parse::<Uuid>().is_err() {
                continue;
            }
            if let Some(grant) = self.read_grant(&format!("{name}.json"))? {
                result.push(grant);
                if result.len() > MAX_GRANTS {
                    return Err(error(
                        "credential_limit",
                        "Grant directory exceeds its capacity",
                    ));
                }
            }
        }
        Ok(result)
    }

    /// Does an applicable grant exist for this reference + invocation context?
    pub fn check(
        &self,
        reference: CredentialRef,
        kind: GrantRunKind,
        tool: Option<&str>,
        workspace: Option<&str>,
        now: &str,
    ) -> Result<bool, BoundaryError> {
        for grant in self.list_raw()? {
            if grant.reference == reference && grant.applies(kind, tool, workspace, now) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn parse_rfc3339(value: &str) -> Option<u64> {
    // Accept `YYYY-MM-DDTHH:MM:SSZ` or with `+HH:MM`/`-HH:MM`. Compare the
    // canonical form lexically after normalizing to UTC is overkill; instead we
    // require callers to store UTC `…Z` and only accept that spelling here.
    let bytes = value.as_bytes();
    if bytes.len() != 20 || !value.ends_with('Z') {
        return None;
    }
    // Basic shape: digits and separators.
    let digit = |i: usize| bytes.get(i).is_some_and(u8::is_ascii_digit);
    let sep = |i: usize, c: u8| bytes.get(i) == Some(&c);
    if !(digit(0)
        && digit(1)
        && digit(2)
        && digit(3)
        && sep(4, b'-')
        && digit(5)
        && digit(6)
        && sep(7, b'-')
        && digit(8)
        && digit(9)
        && sep(10, b'T')
        && digit(11)
        && digit(12)
        && sep(13, b':')
        && digit(14)
        && digit(15)
        && sep(16, b':')
        && digit(17)
        && digit(18))
    {
        return None;
    }
    Some(0)
}

/// Current UTC as `YYYY-MM-DDTHH:MM:SSZ` (lexically ordered for `applies`).
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_utc(secs)
}

fn format_unix_utc(secs: u64) -> String {
    // Civil-from-days (Howard Hinnant's algorithm), no external dep.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference() -> CredentialRef {
        "00000000-0000-0000-0000-000000000001".parse().unwrap()
    }

    fn grant(kinds: &[GrantRunKind], tools: &[&str], workspaces: &[&str]) -> CredentialGrant {
        CredentialGrant {
            grant_id: Uuid::now_v7(),
            reference: reference(),
            workspaces: workspaces.iter().map(|s| s.to_string()).collect(),
            run_kinds: kinds.iter().copied().collect(),
            tools: tools.iter().map(|s| s.to_string()).collect(),
            expires_at: None,
            server_id: "srv".into(),
        }
    }

    #[test]
    fn grant_applies_requires_kind_tool_workspace_and_expiry() {
        let g = grant(
            &[GrantRunKind::Tool],
            &["exec"],
            &["ws_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"],
        );
        let ws = "ws_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(g.applies(
            GrantRunKind::Tool,
            Some("exec"),
            Some(ws),
            "2099-01-01T00:00:00Z"
        ));
        assert!(!g.applies(
            GrantRunKind::Subagent,
            Some("exec"),
            Some(ws),
            "2099-01-01T00:00:00Z"
        ));
        assert!(!g.applies(
            GrantRunKind::Tool,
            Some("read"),
            Some(ws),
            "2099-01-01T00:00:00Z"
        ));
        assert!(!g.applies(
            GrantRunKind::Tool,
            Some("exec"),
            Some("ws_other"),
            "2099-01-01T00:00:00Z"
        ));
        assert!(!g.applies(
            GrantRunKind::Tool,
            Some("exec"),
            None,
            "2099-01-01T00:00:00Z"
        ));
    }

    #[test]
    fn expired_grant_never_applies() {
        let mut g = grant(&[GrantRunKind::Tool], &[], &[]);
        g.expires_at = Some("2020-01-01T00:00:00Z".into());
        assert!(!g.applies(
            GrantRunKind::Tool,
            Some("exec"),
            None,
            "2020-01-01T00:00:00Z"
        ));
        assert!(!g.applies(
            GrantRunKind::Tool,
            Some("exec"),
            None,
            "2021-01-01T00:00:00Z"
        ));
    }

    #[test]
    fn grant_validation_rejects_bad_fields() {
        let mut g = grant(&[], &[], &[]);
        assert!(g.validate().is_err()); // empty run_kinds
        g.run_kinds.insert(GrantRunKind::Tool);
        g.server_id = "bad id!".into();
        assert!(g.validate().is_err());
        g.server_id = "srv".into();
        g.expires_at = Some("not-a-date".into());
        assert!(g.validate().is_err());
        g.expires_at = Some("2030-05-05T12:00:00Z".into());
        assert!(g.validate().is_ok());
    }

    #[test]
    fn store_round_trip_and_grant_check() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cred");
        std::fs::create_dir(&root).unwrap();
        let store = GrantStore::open(&root).unwrap();
        let mut g = grant(&[GrantRunKind::Tool], &["exec"], &[]);
        g.reference = reference();
        store.put(&g).unwrap();
        let now = now_rfc3339();
        assert!(
            store
                .check(reference(), GrantRunKind::Tool, Some("exec"), None, &now)
                .unwrap()
        );
        assert!(
            !store
                .check(
                    reference(),
                    GrantRunKind::Subagent,
                    Some("exec"),
                    None,
                    &now
                )
                .unwrap()
        );
        assert!(
            !store
                .check(reference(), GrantRunKind::Tool, Some("read"), None, &now)
                .unwrap()
        );
        // remove_for clears grants on revoke.
        assert_eq!(store.remove_for(reference()).unwrap(), 1);
        assert!(
            !store
                .check(reference(), GrantRunKind::Tool, Some("exec"), None, &now)
                .unwrap()
        );
    }
}
