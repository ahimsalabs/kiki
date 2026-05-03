//! M12 managed-workspace storage layout.
//!
//! Replaces the flat `mounts/wc-<hash>/` layout with a per-repo
//! grouped structure:
//!
//! ```text
//! <storage_dir>/
//!   repos.toml                          # repo registry + slot allocator
//!   repos/
//!     <repo_name>/
//!       git_store/                      # shared bare git repo
//!         git/
//!           worktrees/
//!             <workspace_name>/         # per-workspace git worktree gitdir
//!               HEAD                    #   own HEAD (detached)
//!               index                   #   own staging area
//!               commondir               #   → "../.." (shared objects)
//!       store.redb                      # shared redb (op-store, LocalRefs)
//!       workspaces/
//!         <workspace_name>/
//!           workspace.toml              # per-workspace metadata
//!           scratch/                    # per-workspace redirections
//! ```
//!
//! The old `mount_meta.rs` layout remains available for ad-hoc mounts
//! via `kiki kk init`. This module is additive.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ── repos.toml ──────────────────────────────────────────────────────

/// Top-level repo registry persisted at `<storage_dir>/repos.toml`.
///
/// Maps repo names to URLs and tracks the monotonic workspace-slot
/// allocator. Slot ids are never reused (M12 §12.2 #3): a deleted
/// workspace's slot is permanently retired.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReposConfig {
    /// Next workspace slot to allocate. Starts at 1; slot 0 is
    /// reserved for RootFs synthetic directories.
    #[serde(default = "default_next_slot")]
    pub next_slot: u32,
    /// Repo entries keyed by name.
    #[serde(default)]
    pub repos: std::collections::BTreeMap<String, RepoEntry>,
}

fn default_next_slot() -> u32 {
    1
}

impl Default for ReposConfig {
    fn default() -> Self {
        ReposConfig {
            next_slot: 1,
            repos: std::collections::BTreeMap::new(),
        }
    }
}

/// A single repo entry in `repos.toml`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RepoEntry {
    /// Remote URL (e.g. `kiki+ssh://server/repo`, `dir:///path`).
    pub url: String,
}

impl ReposConfig {
    /// Allocate the next workspace slot. Monotonic, never reused.
    pub fn alloc_slot(&mut self) -> u32 {
        let slot = self.next_slot;
        self.next_slot = self
            .next_slot
            .checked_add(1)
            .expect("workspace slot overflow (>16M workspaces)");
        slot
    }

    /// Read `repos.toml` from `path`, or return `Default` if the file
    /// doesn't exist yet (first run).
    pub fn read_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self = toml::from_str(&body)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    /// Atomically write `repos.toml` to `path` (tmp + rename).
    pub fn write_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self).context("serializing repos.toml")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| {
            format!("renaming {} -> {}", tmp.display(), path.display())
        })?;
        Ok(())
    }
}

// ── workspace.toml ──────────────────────────────────────────────────

/// Workspace lifecycle state.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceState {
    /// Created by daemon, awaiting CLI jj initialization.
    Pending,
    /// Fully initialized and visible in the namespace.
    Active,
}

/// Per-workspace metadata persisted at
/// `<storage_dir>/repos/<repo>/workspaces/<ws>/workspace.toml`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceConfig {
    /// Lifecycle state (pending until CLI finalizes).
    pub state: WorkspaceState,
    /// Global inode slot for this workspace. Stable across restarts;
    /// monotonic, never reused.
    pub slot: u32,
    /// Last operation id pushed by the CLI via `SetCheckoutState`.
    #[serde(default, with = "hex_bytes")]
    pub op_id: Vec<u8>,
    /// jj workspace identifier (opaque bytes).
    #[serde(default, with = "hex_bytes")]
    pub workspace_id: Vec<u8>,
    /// Currently checked-out root tree id.
    #[serde(default, with = "hex_bytes")]
    pub root_tree_id: Vec<u8>,
    /// Committer timestamp (millis since epoch) of the currently checked-out
    /// commit. Persisted so that file mtimes survive daemon restarts.
    #[serde(default)]
    pub commit_mtime_millis: i64,
}

impl WorkspaceConfig {
    /// Atomically write to `path` (tmp + rename).
    pub fn write_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let body =
            toml::to_string_pretty(self).context("serializing workspace.toml")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| {
            format!("renaming {} -> {}", tmp.display(), path.display())
        })?;
        Ok(())
    }

    /// Read from `path`.
    pub fn read_from(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self = toml::from_str(&body)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }
}

// ── Path helpers ────────────────────────────────────────────────────

/// `<storage_dir>/repos.toml`
pub fn repos_config_path(storage_dir: &Path) -> PathBuf {
    storage_dir.join("repos.toml")
}

/// `<storage_dir>/repos/<repo_name>/`
pub fn repo_dir(storage_dir: &Path, repo_name: &str) -> PathBuf {
    storage_dir.join("repos").join(repo_name)
}

/// `<storage_dir>/repos/<repo_name>/git_store/`
pub fn repo_git_store_path(storage_dir: &Path, repo_name: &str) -> PathBuf {
    repo_dir(storage_dir, repo_name).join("git_store")
}

/// `<storage_dir>/repos/<repo_name>/store.redb`
pub fn repo_redb_path(storage_dir: &Path, repo_name: &str) -> PathBuf {
    repo_dir(storage_dir, repo_name).join("store.redb")
}

/// `<storage_dir>/repos/<repo_name>/workspaces/<ws_name>/`
pub fn workspace_dir(
    storage_dir: &Path,
    repo_name: &str,
    ws_name: &str,
) -> PathBuf {
    repo_dir(storage_dir, repo_name)
        .join("workspaces")
        .join(ws_name)
}

/// `<storage_dir>/repos/<repo_name>/workspaces/<ws_name>/workspace.toml`
pub fn workspace_config_path(
    storage_dir: &Path,
    repo_name: &str,
    ws_name: &str,
) -> PathBuf {
    workspace_dir(storage_dir, repo_name, ws_name).join("workspace.toml")
}

/// Per-workspace git worktree gitdir inside the shared bare repo.
///
/// `<storage_dir>/repos/<repo_name>/git_store/git/worktrees/<ws_name>/`
///
/// This directory holds the workspace's own HEAD, index, and a
/// `commondir` file pointing back to the shared bare repo. It enables
/// stock `git add`/`git commit` per workspace without index collisions.
pub fn workspace_worktree_gitdir(
    storage_dir: &Path,
    repo_name: &str,
    ws_name: &str,
) -> PathBuf {
    repo_git_store_path(storage_dir, repo_name)
        .join("git")
        .join("worktrees")
        .join(ws_name)
}

/// `<storage_dir>/repos/<repo_name>/workspaces/<ws_name>/scratch/`
pub fn workspace_scratch_dir(
    storage_dir: &Path,
    repo_name: &str,
    ws_name: &str,
) -> PathBuf {
    workspace_dir(storage_dir, repo_name, ws_name).join("scratch")
}

/// List all workspace configs under a repo. Returns
/// `(workspace_name, WorkspaceConfig)` pairs sorted by name.
pub fn list_workspace_configs(
    storage_dir: &Path,
    repo_name: &str,
) -> Result<Vec<(String, WorkspaceConfig)>> {
    let ws_parent = repo_dir(storage_dir, repo_name).join("workspaces");
    if !ws_parent.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&ws_parent)
        .with_context(|| format!("reading {}", ws_parent.display()))?
    {
        let entry = entry.context("reading workspace dir entry")?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let ws_name = match dir.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let cfg_path = dir.join("workspace.toml");
        if !cfg_path.exists() {
            continue;
        }
        match WorkspaceConfig::read_from(&cfg_path) {
            Ok(cfg) => out.push((ws_name, cfg)),
            Err(e) => {
                tracing::warn!(
                    path = %cfg_path.display(),
                    error = %format!("{e:#}"),
                    "skipping unreadable workspace.toml"
                );
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Validate a repo or workspace name for use as a directory component.
/// Rejects empty, `.`, `..`, names containing `/` or `\`, and names
/// starting with `.`.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("name must not be empty");
    }
    if name == "." || name == ".." {
        anyhow::bail!("name must not be '.' or '..'");
    }
    if name.starts_with('.') {
        anyhow::bail!("name must not start with '.'");
    }
    if name.contains('/') || name.contains('\\') {
        anyhow::bail!("name must not contain path separators");
    }
    Ok(())
}

// ── hex_bytes serde helper ──────────────────────────────────────────

/// Hex-encode/decode `Vec<u8>` for serde. Empty vec → empty string.
/// Same implementation as `mount_meta::hex_bytes` — duplicated here
/// to avoid coupling the two modules.
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(&mut out, "{b:02x}");
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s: String = String::deserialize(d)?;
        if s.is_empty() {
            return Ok(Vec::new());
        }
        if !s.len().is_multiple_of(2) {
            return Err(serde::de::Error::custom(
                "hex string length must be even",
            ));
        }
        let mut out = Vec::with_capacity(s.len() / 2);
        let bytes = s.as_bytes();
        for i in (0..bytes.len()).step_by(2) {
            let hi = decode_nibble(bytes[i])
                .ok_or_else(|| serde::de::Error::custom("invalid hex digit"))?;
            let lo = decode_nibble(bytes[i + 1])
                .ok_or_else(|| serde::de::Error::custom("invalid hex digit"))?;
            out.push((hi << 4) | lo);
        }
        Ok(out)
    }

    fn decode_nibble(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ReposConfig ──

    #[test]
    fn repos_config_default_is_empty() {
        let cfg = ReposConfig::default();
        assert_eq!(cfg.next_slot, 1);
        assert!(cfg.repos.is_empty());
    }

    #[test]
    fn repos_config_alloc_slot_is_monotonic() {
        let mut cfg = ReposConfig::default();
        assert_eq!(cfg.alloc_slot(), 1);
        assert_eq!(cfg.alloc_slot(), 2);
        assert_eq!(cfg.alloc_slot(), 3);
        assert_eq!(cfg.next_slot, 4);
    }

    #[test]
    fn repos_config_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.toml");

        let mut cfg = ReposConfig {
            next_slot: 5,
            ..Default::default()
        };
        cfg.repos.insert(
            "myrepo".into(),
            RepoEntry {
                url: "kiki+ssh://server/myrepo".into(),
            },
        );
        cfg.write_to(&path).unwrap();
        let back = ReposConfig::read_or_default(&path).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn repos_config_read_missing_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.toml");
        let cfg = ReposConfig::read_or_default(&path).unwrap();
        assert_eq!(cfg, ReposConfig::default());
    }

    // ── WorkspaceConfig ──

    fn sample_workspace() -> WorkspaceConfig {
        WorkspaceConfig {
            state: WorkspaceState::Active,
            slot: 1,
            op_id: vec![0xab, 0xcd],
            workspace_id: b"default".to_vec(),
            root_tree_id: vec![0xff; 20],
            commit_mtime_millis: 1_700_000_000_000,
        }
    }

    #[test]
    fn workspace_config_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.toml");
        let ws = sample_workspace();
        ws.write_to(&path).unwrap();
        let back = WorkspaceConfig::read_from(&path).unwrap();
        assert_eq!(ws, back);
    }

    #[test]
    fn workspace_config_pending_state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.toml");
        let ws = WorkspaceConfig {
            state: WorkspaceState::Pending,
            slot: 42,
            op_id: Vec::new(),
            workspace_id: Vec::new(),
            root_tree_id: Vec::new(),
            commit_mtime_millis: 0,
        };
        ws.write_to(&path).unwrap();
        let back = WorkspaceConfig::read_from(&path).unwrap();
        assert_eq!(ws, back);
    }

    // ── list_workspace_configs ──

    #[test]
    fn list_workspace_configs_finds_all() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();

        // Create two workspaces.
        let ws1 = sample_workspace();
        let p1 = workspace_config_path(storage, "repo", "alpha");
        ws1.write_to(&p1).unwrap();

        let mut ws2 = sample_workspace();
        ws2.slot = 2;
        let p2 = workspace_config_path(storage, "repo", "beta");
        ws2.write_to(&p2).unwrap();

        let list = list_workspace_configs(storage, "repo").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].0, "alpha");
        assert_eq!(list[1].0, "beta");
        assert_eq!(list[0].1.slot, 1);
        assert_eq!(list[1].1.slot, 2);
    }

    #[test]
    fn list_workspace_configs_empty_when_no_dir() {
        let dir = tempfile::tempdir().unwrap();
        let list = list_workspace_configs(dir.path(), "nonexistent").unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn list_workspace_configs_skips_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();

        // Good workspace.
        let ws = sample_workspace();
        ws.write_to(&workspace_config_path(storage, "repo", "good"))
            .unwrap();

        // Bad workspace: unreadable toml.
        let bad_dir = workspace_dir(storage, "repo", "bad");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("workspace.toml"), "not valid toml ===")
            .unwrap();

        let list = list_workspace_configs(storage, "repo").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "good");
    }

    // ── Path helpers ──

    #[test]
    fn path_helpers_produce_expected_layout() {
        let s = Path::new("/data/kiki");
        assert_eq!(repos_config_path(s), PathBuf::from("/data/kiki/repos.toml"));
        assert_eq!(
            repo_dir(s, "mono"),
            PathBuf::from("/data/kiki/repos/mono")
        );
        assert_eq!(
            repo_git_store_path(s, "mono"),
            PathBuf::from("/data/kiki/repos/mono/git_store")
        );
        assert_eq!(
            repo_redb_path(s, "mono"),
            PathBuf::from("/data/kiki/repos/mono/store.redb")
        );
        assert_eq!(
            workspace_dir(s, "mono", "default"),
            PathBuf::from("/data/kiki/repos/mono/workspaces/default")
        );
        assert_eq!(
            workspace_config_path(s, "mono", "default"),
            PathBuf::from("/data/kiki/repos/mono/workspaces/default/workspace.toml")
        );
        assert_eq!(
            workspace_scratch_dir(s, "mono", "default"),
            PathBuf::from("/data/kiki/repos/mono/workspaces/default/scratch")
        );
        assert_eq!(
            workspace_worktree_gitdir(s, "mono", "default"),
            PathBuf::from("/data/kiki/repos/mono/git_store/git/worktrees/default")
        );
    }

    // ── validate_name ──

    #[test]
    fn validate_name_accepts_good_names() {
        assert!(validate_name("default").is_ok());
        assert!(validate_name("fix-auth").is_ok());
        assert!(validate_name("my_repo_2").is_ok());
        assert!(validate_name("UPPER").is_ok());
    }

    #[test]
    fn validate_name_rejects_bad_names() {
        assert!(validate_name("").is_err());
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name(".hidden").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn repos_config_toml_roundtrip(
            next_slot in 1..1000u32,
            name in "[a-z][a-z0-9_-]{0,15}",
            url in "[a-z]{3,8}://[a-z0-9.]{1,20}/[a-z0-9]{1,10}",
        ) {
            let mut cfg = ReposConfig {
                next_slot,
                ..Default::default()
            };
            cfg.repos.insert(name, RepoEntry { url });

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("repos.toml");
            cfg.write_to(&path).unwrap();
            let back = ReposConfig::read_or_default(&path).unwrap();
            prop_assert_eq!(&cfg, &back);
        }

        #[test]
        fn workspace_config_toml_roundtrip(
            slot in 1..10000u32,
            op_id in prop::collection::vec(any::<u8>(), 0..64),
            workspace_id in prop::collection::vec(any::<u8>(), 0..32),
            root_tree_id in prop::collection::vec(any::<u8>(), 0..32),
            pending in any::<bool>(),
            commit_mtime_millis in any::<i64>(),
        ) {
            let ws = WorkspaceConfig {
                state: if pending { WorkspaceState::Pending } else { WorkspaceState::Active },
                slot,
                op_id,
                workspace_id,
                root_tree_id,
                commit_mtime_millis,
            };
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("workspace.toml");
            ws.write_to(&path).unwrap();
            let back = WorkspaceConfig::read_from(&path).unwrap();
            prop_assert_eq!(&ws, &back);
        }

        #[test]
        fn alloc_slot_never_reuses(count in 1..100usize) {
            let mut cfg = ReposConfig::default();
            let mut seen = std::collections::HashSet::new();
            for _ in 0..count {
                let slot = cfg.alloc_slot();
                prop_assert!(seen.insert(slot), "slot {slot} was reused");
            }
        }
    }
}
