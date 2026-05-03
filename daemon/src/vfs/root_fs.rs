//! `RootFs` — the multi-workspace routing layer (M12 §12.3–12.6).
//!
//! A single `RootFs` implements `JjKikiFs` and routes FUSE operations
//! through a synthetic `/<repo>/<workspace>/` namespace into per-workspace
//! `KikiFs` instances. The FUSE adapter takes `Arc<RootFs>` the same way
//! it currently takes `Arc<KikiFs>` — no adapter changes required.
//!
//! ## Inode encoding (§12.4)
//!
//! ```text
//! global_inode = (slot << 40) | local_inode
//! ```
//!
//! Slot 0 is reserved for synthetic directories (root, repo dirs).
//! Slots 1..2^24 are assigned to workspaces. `KikiFs` and `InodeSlab`
//! are unchanged — translation is mechanical bit manipulation at the
//! `RootFs` boundary.
//!
//! ## Lazy hydration (§12.6)
//!
//! Workspace `KikiFs` instances are created on first filesystem access
//! via `get_or_hydrate`, not at daemon startup. Two-phase
//! double-checked locking avoids holding the mutex across async I/O.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tracing::{debug, warn};

use crate::git_store::GitContentStore;
use crate::local_refs::LocalRefs;
use crate::remote::RemoteStore;
use crate::ty::Id;
use crate::vfs::inode::{InodeId, ROOT_INODE};
use crate::vfs::kiki_fs::{Attr, DirEntry, FileKind, FsError, JjKikiFs, KikiFs};

// ── Inode encoding ──────────────────────────────────────────────────

const SLOT_BITS: u32 = 40;
const SLOT_MASK: u64 = (1u64 << SLOT_BITS) - 1;

/// Extract the workspace slot from a global inode.
fn slot_of(ino: InodeId) -> u32 {
    (ino >> SLOT_BITS) as u32
}

/// Strip the slot prefix, returning the per-workspace local inode.
fn to_local(ino: InodeId) -> InodeId {
    ino & SLOT_MASK
}

/// Combine a slot and a local inode into a global inode.
fn to_global(slot: u32, local: InodeId) -> InodeId {
    ((slot as u64) << SLOT_BITS) | local
}

/// Translate an `Attr` from per-workspace local inode space to global.
fn attr_to_global(slot: u32, mut attr: Attr) -> Attr {
    attr.inode = to_global(slot, attr.inode);
    attr
}

// ── Synthetic directory layer ───────────────────────────────────────

/// A synthetic directory (root or repo dir) in slot 0.
#[derive(Debug, Clone)]
struct SyntheticDir {
    #[allow(dead_code)]
    parent: InodeId,
    #[allow(dead_code)]
    name: String,
    children: BTreeMap<String, InodeId>,
}

/// Per-repo metadata held by `RootFs`.
#[derive(Debug)]
struct RepoEntry {
    #[allow(dead_code)]
    url: String,
    /// Synthetic directory inode for this repo (slot 0).
    inode: InodeId,
    /// Shared content store for all workspaces in this repo.
    store: Arc<GitContentStore>,
    /// Optional remote for lazy read-through.
    remote_store: Option<Arc<dyn RemoteStore>>,
    /// Workspace name -> workspace metadata.
    workspaces: HashMap<String, WorkspaceEntry>,
}

/// Per-workspace metadata (may or may not be hydrated).
#[derive(Debug, Clone)]
struct WorkspaceEntry {
    slot: u32,
    state: WorkspaceState,
    root_tree_id: Vec<u8>,
    op_id: Vec<u8>,
    workspace_id: Vec<u8>,
}

/// Workspace lifecycle state mirroring `repo_meta::WorkspaceState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceState {
    Pending,
    Active,
}

/// A hydrated (live) workspace — the `KikiFs` has been instantiated.
#[derive(Debug)]
struct WorkspaceLive {
    #[allow(dead_code)]
    repo_name: String,
    #[allow(dead_code)]
    workspace_name: String,
    fs: Arc<KikiFs>,
    local_refs: Arc<LocalRefs>,
    /// Last HEAD commit id exported via `export_head`. Used to detect
    /// external git HEAD changes (e.g. `git commit` from the mount).
    /// Runtime-only; not persisted.
    last_exported_head: Vec<u8>,
    /// Last bookmarks exported via `export_bookmarks` in WriteView.
    /// `None` until first WriteView; `Some(vec![])` means "exported
    /// an empty set."
    last_exported_bookmarks: Option<Vec<(String, Vec<u8>)>>,
}

// ── Public API types ────────────────────────────────────────────────

/// Returned by [`RootFs::get_workspace`]. Bundles the per-workspace
/// handles the service needs for RPC dispatch.
pub struct WorkspaceHandle {
    pub fs: Arc<KikiFs>,
    pub store: Arc<GitContentStore>,
    pub remote_store: Option<Arc<dyn RemoteStore>>,
    pub local_refs: Arc<LocalRefs>,
}

/// Parameters for [`RootFs::register_workspace`].
pub struct WorkspaceRegistration {
    pub name: String,
    pub slot: u32,
    pub state: WorkspaceState,
    pub root_tree_id: Vec<u8>,
    pub op_id: Vec<u8>,
    pub workspace_id: Vec<u8>,
}

// ── RootFs ──────────────────────────────────────────────────────────

#[derive(Debug)]
struct RootFsInner {
    /// Mount root path (e.g. `/mnt/kiki`).
    #[allow(dead_code)]
    mount_root: PathBuf,
    /// Storage directory for persisted state.
    storage_dir: PathBuf,
    /// Synthetic inode allocator (slot 0, starts at 2; inode 1 = root).
    next_synthetic: InodeId,
    /// Synthetic directory entries: root dir, repo dirs.
    synthetic_dirs: HashMap<InodeId, SyntheticDir>,
    /// Repo name -> repo entry.
    repos: HashMap<String, RepoEntry>,
    /// Slot -> live workspace. Populated lazily by `get_or_hydrate`.
    live: HashMap<u32, WorkspaceLive>,
    /// Next slot to allocate. Monotonic, never reused (§12.2 #3).
    next_slot: u32,
}

/// The multi-workspace routing filesystem.
///
/// Implements `JjKikiFs` by dispatching operations through a synthetic
/// `/<repo>/<workspace>/` namespace. Slot 0 inodes are synthetic
/// directories owned by `RootFs`; slot 1..N inodes delegate to the
/// per-workspace `KikiFs` after inode translation.
pub struct RootFs {
    inner: Mutex<RootFsInner>,
}

impl std::fmt::Debug for RootFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootFs").finish_non_exhaustive()
    }
}

impl RootFs {
    /// Construct a new `RootFs`. The root synthetic directory (inode 1)
    /// is created automatically. Repos and workspaces are registered
    /// afterward via `register_repo` / `register_workspace`.
    pub fn new(mount_root: PathBuf, storage_dir: PathBuf, next_slot: u32) -> Self {
        let mut synthetic_dirs = HashMap::new();
        synthetic_dirs.insert(
            ROOT_INODE,
            SyntheticDir {
                parent: ROOT_INODE,
                name: String::new(),
                children: BTreeMap::new(),
            },
        );
        RootFs {
            inner: Mutex::new(RootFsInner {
                mount_root,
                storage_dir,
                next_synthetic: ROOT_INODE + 1,
                synthetic_dirs,
                repos: HashMap::new(),
                live: HashMap::new(),
                next_slot,
            }),
        }
    }

    // ── Registration (called at startup / from RPCs) ────────────

    /// Register a new repo. Creates a synthetic directory under the
    /// root. If the repo already exists, this is a no-op.
    pub fn register_repo(
        &self,
        name: String,
        url: String,
        store: Arc<GitContentStore>,
        remote_store: Option<Arc<dyn RemoteStore>>,
    ) {
        let mut inner = self.inner.lock();
        if inner.repos.contains_key(&name) {
            return;
        }
        let inode = inner.next_synthetic;
        inner.next_synthetic += 1;
        inner.synthetic_dirs.insert(
            inode,
            SyntheticDir {
                parent: ROOT_INODE,
                name: name.clone(),
                children: BTreeMap::new(),
            },
        );
        // Add to root's children.
        if let Some(root) = inner.synthetic_dirs.get_mut(&ROOT_INODE) {
            root.children.insert(name.clone(), inode);
        }
        inner.repos.insert(
            name,
            RepoEntry {
                url,
                inode,
                store,
                remote_store,
                workspaces: HashMap::new(),
            },
        );
    }

    /// Register a workspace under a repo. The workspace starts in the
    /// given state; `Pending` workspaces are hidden from readdir/lookup.
    ///
    /// Returns `Err(FsError::NotFound)` if the repo doesn't exist,
    /// `Err(FsError::AlreadyExists)` if the workspace name is taken.
    pub fn register_workspace(
        &self,
        repo: &str,
        reg: WorkspaceRegistration,
    ) -> Result<(), FsError> {
        let mut inner = self.inner.lock();
        // Validate and mutate the repo entry first, then update synthetic dirs.
        let repo_inode = {
            let repo_entry = inner.repos.get_mut(repo).ok_or(FsError::NotFound)?;
            if repo_entry.workspaces.contains_key(&reg.name) {
                return Err(FsError::AlreadyExists);
            }
            let repo_inode = repo_entry.inode;
            repo_entry.workspaces.insert(
                reg.name.clone(),
                WorkspaceEntry {
                    slot: reg.slot,
                    state: reg.state,
                    root_tree_id: reg.root_tree_id,
                    op_id: reg.op_id,
                    workspace_id: reg.workspace_id,
                },
            );
            repo_inode
        };
        // Add the workspace to the repo dir's lookup table regardless of
        // state. This makes the workspace accessible via FUSE lookup (needed
        // by the CLI to write .jj/ during init before finalize). Pending
        // workspaces are filtered out of readdir (§12.10 step 5) so users
        // don't see half-initialized workspaces in `ls`.
        let global_root = to_global(reg.slot, ROOT_INODE);
        if let Some(repo_dir) = inner.synthetic_dirs.get_mut(&repo_inode) {
            repo_dir.children.insert(reg.name, global_root);
        }
        // Ensure next_slot stays ahead.
        if reg.slot >= inner.next_slot {
            inner.next_slot = reg.slot + 1;
        }
        Ok(())
    }

    /// Transition a workspace from pending to active. Makes it visible
    /// in readdir/lookup.
    pub fn finalize_workspace(&self, repo: &str, ws: &str) -> Result<(), FsError> {
        let mut inner = self.inner.lock();
        let repo_inode = inner
            .repos
            .get(repo)
            .ok_or(FsError::NotFound)?
            .inode;
        let repo_entry = inner.repos.get_mut(repo).ok_or(FsError::NotFound)?;
        let ws_entry = repo_entry
            .workspaces
            .get_mut(ws)
            .ok_or(FsError::NotFound)?;
        if ws_entry.state == WorkspaceState::Active {
            return Ok(()); // already finalized
        }
        ws_entry.state = WorkspaceState::Active;
        let global_root = to_global(ws_entry.slot, ROOT_INODE);
        if let Some(repo_dir) = inner.synthetic_dirs.get_mut(&repo_inode) {
            repo_dir.children.insert(ws.to_owned(), global_root);
        }
        Ok(())
    }

    /// Remove a workspace. Drops the live `KikiFs` if hydrated, retires
    /// the slot (never reused), and removes from the synthetic dir.
    pub fn remove_workspace(&self, repo: &str, ws: &str) -> Result<u32, FsError> {
        let mut inner = self.inner.lock();
        let repo_inode = inner
            .repos
            .get(repo)
            .ok_or(FsError::NotFound)?
            .inode;
        let repo_entry = inner.repos.get_mut(repo).ok_or(FsError::NotFound)?;
        let ws_entry = repo_entry
            .workspaces
            .remove(ws)
            .ok_or(FsError::NotFound)?;
        // Remove from repo dir children.
        if let Some(repo_dir) = inner.synthetic_dirs.get_mut(&repo_inode) {
            repo_dir.children.remove(ws);
        }
        // Drop live KikiFs.
        inner.live.remove(&ws_entry.slot);
        Ok(ws_entry.slot)
    }

    /// Allocate the next workspace slot. Monotonic, never reused.
    pub fn alloc_slot(&self) -> u32 {
        let mut inner = self.inner.lock();
        let slot = inner.next_slot;
        inner.next_slot = inner
            .next_slot
            .checked_add(1)
            .expect("workspace slot overflow (>16M workspaces)");
        slot
    }

    /// Current value of `next_slot` (for persisting to `repos.toml`).
    pub fn next_slot(&self) -> u32 {
        self.inner.lock().next_slot
    }

    // ── Workspace lookup (service API) ──────────────────────────

    /// Look up a workspace by (repo, ws) name. Hydrates lazily if
    /// needed (§12.6). Returns the bundle of handles the service
    /// needs for RPC dispatch.
    pub async fn get_workspace(
        &self,
        repo: &str,
        ws: &str,
    ) -> Result<WorkspaceHandle, FsError> {
        // Phase 1: check live map, gather metadata under lock.
        let hydration_info = {
            let inner = self.inner.lock();
            let repo_entry = inner.repos.get(repo).ok_or(FsError::NotFound)?;
            let ws_entry = repo_entry
                .workspaces
                .get(ws)
                .ok_or(FsError::NotFound)?;
            if let Some(live) = inner.live.get(&ws_entry.slot) {
                return Ok(WorkspaceHandle {
                    fs: Arc::clone(&live.fs),
                    store: Arc::clone(&repo_entry.store),
                    remote_store: repo_entry.remote_store.clone(),
                    local_refs: Arc::clone(&live.local_refs),
                });
            }
            // Need to hydrate. Copy what we need before dropping the lock.
            HydrationInfo {
                slot: ws_entry.slot,
                root_tree_id: ws_entry.root_tree_id.clone(),
                store: Arc::clone(&repo_entry.store),
                remote_store: repo_entry.remote_store.clone(),
                repo_name: repo.to_owned(),
                ws_name: ws.to_owned(),
                storage_dir: inner.storage_dir.clone(),
            }
        };

        // Phase 2: hydrate without lock (may involve I/O).
        let (fs, local_refs) = self.hydrate_workspace(&hydration_info)?;

        // Phase 3: re-acquire lock, insert-if-absent.
        let mut inner = self.inner.lock();
        if let Some(live) = inner.live.get(&hydration_info.slot) {
            // Another thread won the race.
            return Ok(WorkspaceHandle {
                fs: Arc::clone(&live.fs),
                store: hydration_info.store,
                remote_store: hydration_info.remote_store,
                local_refs: Arc::clone(&live.local_refs),
            });
        }
        let local_refs = Arc::new(local_refs);
        let fs = Arc::new(fs);
        inner.live.insert(
            hydration_info.slot,
            WorkspaceLive {
                repo_name: hydration_info.repo_name,
                workspace_name: hydration_info.ws_name,
                fs: Arc::clone(&fs),
                local_refs: Arc::clone(&local_refs),
                last_exported_head: Vec::new(),
                last_exported_bookmarks: None,
            },
        );
        Ok(WorkspaceHandle {
            fs,
            store: hydration_info.store,
            remote_store: hydration_info.remote_store,
            local_refs,
        })
    }

    // ── Mutable workspace metadata ──────────────────────────────

    /// Update op_id + workspace_id. Called by SetCheckoutState RPC.
    pub fn set_checkout_state(
        &self,
        repo: &str,
        ws: &str,
        op_id: Vec<u8>,
        workspace_id: Vec<u8>,
    ) -> Result<(), FsError> {
        let mut inner = self.inner.lock();
        let repo_entry = inner.repos.get_mut(repo).ok_or(FsError::NotFound)?;
        let ws_entry = repo_entry
            .workspaces
            .get_mut(ws)
            .ok_or(FsError::NotFound)?;
        ws_entry.op_id = op_id.clone();
        ws_entry.workspace_id = workspace_id.clone();
        // Persist to workspace.toml.
        let cfg = crate::repo_meta::WorkspaceConfig {
            state: match ws_entry.state {
                WorkspaceState::Active => crate::repo_meta::WorkspaceState::Active,
                WorkspaceState::Pending => crate::repo_meta::WorkspaceState::Pending,
            },
            slot: ws_entry.slot,
            op_id,
            workspace_id,
            root_tree_id: ws_entry.root_tree_id.clone(),
        };
        let path = crate::repo_meta::workspace_config_path(
            &inner.storage_dir,
            repo,
            ws,
        );
        if let Err(e) = cfg.write_to(&path) {
            warn!(error = %format!("{e:#}"), "failed to persist workspace.toml");
        }
        Ok(())
    }

    /// Update root_tree_id after CheckOut or Snapshot.
    pub fn set_root_tree_id(
        &self,
        repo: &str,
        ws: &str,
        root_tree_id: Vec<u8>,
    ) -> Result<(), FsError> {
        let mut inner = self.inner.lock();
        let repo_entry = inner.repos.get_mut(repo).ok_or(FsError::NotFound)?;
        let ws_entry = repo_entry
            .workspaces
            .get_mut(ws)
            .ok_or(FsError::NotFound)?;
        ws_entry.root_tree_id = root_tree_id.clone();
        // Persist to workspace.toml.
        let cfg = crate::repo_meta::WorkspaceConfig {
            state: match ws_entry.state {
                WorkspaceState::Active => crate::repo_meta::WorkspaceState::Active,
                WorkspaceState::Pending => crate::repo_meta::WorkspaceState::Pending,
            },
            slot: ws_entry.slot,
            op_id: ws_entry.op_id.clone(),
            workspace_id: ws_entry.workspace_id.clone(),
            root_tree_id,
        };
        let path = crate::repo_meta::workspace_config_path(
            &inner.storage_dir,
            repo,
            ws,
        );
        if let Err(e) = cfg.write_to(&path) {
            warn!(error = %format!("{e:#}"), "failed to persist workspace.toml");
        }
        Ok(())
    }

    // ── Workspace state getters (service dispatch) ──────────────

    /// Get the root_tree_id for a workspace. Returns `Err` if repo
    /// or workspace not found.
    pub fn get_root_tree_id(
        &self,
        repo: &str,
        ws: &str,
    ) -> Result<Vec<u8>, FsError> {
        let inner = self.inner.lock();
        let repo_entry = inner.repos.get(repo).ok_or(FsError::NotFound)?;
        let ws_entry = repo_entry
            .workspaces
            .get(ws)
            .ok_or(FsError::NotFound)?;
        Ok(ws_entry.root_tree_id.clone())
    }

    /// Get the checkout state (op_id, workspace_id) for a workspace.
    pub fn get_checkout_state(
        &self,
        repo: &str,
        ws: &str,
    ) -> Result<(Vec<u8>, Vec<u8>), FsError> {
        let inner = self.inner.lock();
        let repo_entry = inner.repos.get(repo).ok_or(FsError::NotFound)?;
        let ws_entry = repo_entry
            .workspaces
            .get(ws)
            .ok_or(FsError::NotFound)?;
        Ok((ws_entry.op_id.clone(), ws_entry.workspace_id.clone()))
    }

    /// Get the last exported HEAD commit id (runtime-only, not persisted).
    /// Returns empty vec if workspace is not hydrated or no HEAD exported.
    pub fn get_last_exported_head(
        &self,
        repo: &str,
        ws: &str,
    ) -> Vec<u8> {
        let inner = self.inner.lock();
        let Some(repo_entry) = inner.repos.get(repo) else { return Vec::new() };
        let Some(ws_entry) = repo_entry.workspaces.get(ws) else { return Vec::new() };
        inner.live.get(&ws_entry.slot)
            .map(|l| l.last_exported_head.clone())
            .unwrap_or_default()
    }

    /// Set the last exported HEAD commit id on a hydrated workspace.
    pub fn set_last_exported_head(
        &self,
        repo: &str,
        ws: &str,
        head: Vec<u8>,
    ) {
        let mut inner = self.inner.lock();
        let slot = match inner.repos.get(repo).and_then(|r| r.workspaces.get(ws)) {
            Some(ws_entry) => ws_entry.slot,
            None => return,
        };
        if let Some(live) = inner.live.get_mut(&slot) {
            live.last_exported_head = head;
        }
    }

    /// Get the last exported bookmarks (runtime-only).
    pub fn get_last_exported_bookmarks(
        &self,
        repo: &str,
        ws: &str,
    ) -> Option<Vec<(String, Vec<u8>)>> {
        let inner = self.inner.lock();
        let repo_entry = inner.repos.get(repo)?;
        let ws_entry = repo_entry.workspaces.get(ws)?;
        inner.live.get(&ws_entry.slot)
            .and_then(|l| l.last_exported_bookmarks.clone())
    }

    /// Set the last exported bookmarks on a hydrated workspace.
    pub fn set_last_exported_bookmarks(
        &self,
        repo: &str,
        ws: &str,
        bookmarks: Vec<(String, Vec<u8>)>,
    ) {
        let mut inner = self.inner.lock();
        let slot = match inner.repos.get(repo).and_then(|r| r.workspaces.get(ws)) {
            Some(ws_entry) => ws_entry.slot,
            None => return,
        };
        if let Some(live) = inner.live.get_mut(&slot) {
            live.last_exported_bookmarks = Some(bookmarks);
        }
    }

    // ── Query helpers ───────────────────────────────────────────

    /// List all repos and their workspaces.
    pub fn list_repos(&self) -> Vec<(String, String, Vec<String>)> {
        let inner = self.inner.lock();
        inner
            .repos
            .iter()
            .map(|(name, entry)| {
                let ws_names: Vec<String> = entry
                    .workspaces
                    .iter()
                    .filter(|(_, ws)| ws.state == WorkspaceState::Active)
                    .map(|(n, _)| n.clone())
                    .collect();
                (name.clone(), entry.url.clone(), ws_names)
            })
            .collect()
    }

    /// Get the store for a repo.
    pub fn repo_store(&self, repo: &str) -> Option<Arc<GitContentStore>> {
        let inner = self.inner.lock();
        inner.repos.get(repo).map(|r| Arc::clone(&r.store))
    }

    /// Get the remote store for a repo.
    pub fn repo_remote_store(
        &self,
        repo: &str,
    ) -> Option<Option<Arc<dyn RemoteStore>>> {
        let inner = self.inner.lock();
        inner.repos.get(repo).map(|r| r.remote_store.clone())
    }

    /// Get the kiki remote URL for a repo. `None` = repo not found.
    pub fn repo_url(&self, repo: &str) -> Option<String> {
        let inner = self.inner.lock();
        inner.repos.get(repo).map(|r| r.url.clone())
    }

    /// Set or clear the kiki remote store and URL on an existing repo.
    /// Returns `false` if the repo is not registered.
    pub fn set_repo_remote(
        &self,
        repo: &str,
        url: String,
        remote_store: Option<Arc<dyn RemoteStore>>,
    ) -> bool {
        let mut inner = self.inner.lock();
        if let Some(entry) = inner.repos.get_mut(repo) {
            entry.url = url;
            entry.remote_store = remote_store;
            true
        } else {
            false
        }
    }

    // ── Internal ────────────────────────────────────────────────

    /// Hydrate a workspace (phase 2, no lock held). Creates a `KikiFs`
    /// and `LocalRefs` from the persisted metadata.
    fn hydrate_workspace(
        &self,
        info: &HydrationInfo,
    ) -> Result<(KikiFs, LocalRefs), FsError> {
        use jj_lib::object_id::ObjectId as _;

        let root_tree = if info.root_tree_id.len() == 20 {
            Id(info.root_tree_id.clone().try_into().unwrap())
        } else {
            // Empty or invalid root_tree_id -> empty tree.
            Id(info.store.empty_tree_id().as_bytes().try_into().expect("20-byte tree id"))
        };

        let scratch_dir = crate::repo_meta::workspace_scratch_dir(
            &info.storage_dir,
            &info.repo_name,
            &info.ws_name,
        );

        // Per-workspace worktree gitdir for colocated git support.
        // If the worktree dir doesn't exist yet (pre-existing workspace
        // from before this feature), init it now.
        let git_repo_path = info.store.git_repo_path().to_path_buf();
        let wt_gitdir = crate::repo_meta::workspace_worktree_gitdir(
            &info.storage_dir,
            &info.repo_name,
            &info.ws_name,
        );
        if !wt_gitdir.exists() {
            if let Err(e) = crate::git_ops::init_worktree(&git_repo_path, &info.ws_name) {
                tracing::warn!(
                    repo = %info.repo_name,
                    ws = %info.ws_name,
                    error = %format!("{e:#}"),
                    "failed to init worktree gitdir (git colocation disabled)"
                );
            }
        }

        let fs = KikiFs::new(
            Arc::clone(&info.store),
            root_tree,
            info.remote_store.clone(),
            Some(scratch_dir),
            if wt_gitdir.exists() { Some(wt_gitdir) } else { None },
        );
        let local_refs = LocalRefs::new(info.store.database());

        debug!(
            repo = %info.repo_name,
            ws = %info.ws_name,
            slot = info.slot,
            "hydrated workspace KikiFs"
        );

        Ok((fs, local_refs))
    }

    /// Get a live `KikiFs` for a slot, or hydrate it. Used by the
    /// `JjKikiFs` impl for workspace-dispatched operations.
    async fn get_or_hydrate_by_slot(
        &self,
        slot: u32,
    ) -> Result<Arc<KikiFs>, FsError> {
        // Phase 1: check under lock.
        let hydration_info = {
            let inner = self.inner.lock();
            if let Some(live) = inner.live.get(&slot) {
                return Ok(Arc::clone(&live.fs));
            }
            // Find the workspace entry for this slot.
            let mut found = None;
            for (repo_name, repo_entry) in &inner.repos {
                for (ws_name, ws_entry) in &repo_entry.workspaces {
                    if ws_entry.slot == slot {
                        found = Some(HydrationInfo {
                            slot,
                            root_tree_id: ws_entry.root_tree_id.clone(),
                            store: Arc::clone(&repo_entry.store),
                            remote_store: repo_entry.remote_store.clone(),
                            repo_name: repo_name.clone(),
                            ws_name: ws_name.clone(),
                            storage_dir: inner.storage_dir.clone(),
                        });
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            found.ok_or(FsError::NotFound)?
        };

        // Phase 2: hydrate without lock.
        let (fs, local_refs) = self.hydrate_workspace(&hydration_info)?;

        // Phase 3: re-acquire lock, insert-if-absent.
        let mut inner = self.inner.lock();
        if let Some(live) = inner.live.get(&hydration_info.slot) {
            return Ok(Arc::clone(&live.fs));
        }
        let fs = Arc::new(fs);
        inner.live.insert(
            hydration_info.slot,
            WorkspaceLive {
                repo_name: hydration_info.repo_name,
                workspace_name: hydration_info.ws_name,
                fs: Arc::clone(&fs),
                local_refs: Arc::new(local_refs),
                last_exported_head: Vec::new(),
                last_exported_bookmarks: None,
            },
        );
        Ok(fs)
    }

    /// Synthetic lookup: resolve a name within a synthetic dir (slot 0).
    fn synthetic_lookup(&self, parent: InodeId, name: &str) -> Result<InodeId, FsError> {
        let inner = self.inner.lock();
        let dir = inner
            .synthetic_dirs
            .get(&parent)
            .ok_or(FsError::NotFound)?;
        dir.children.get(name).copied().ok_or(FsError::NotFound)
    }

    /// Synthetic getattr: attributes for a synthetic directory.
    fn synthetic_getattr(&self, ino: InodeId) -> Result<Attr, FsError> {
        let inner = self.inner.lock();
        if inner.synthetic_dirs.contains_key(&ino) {
            Ok(Attr {
                inode: ino,
                kind: FileKind::Directory,
                size: 0,
                executable: false,
            })
        } else {
            Err(FsError::NotFound)
        }
    }

    /// Synthetic readdir: list children of a synthetic directory.
    /// Pending workspaces are filtered out (visible via lookup but hidden
    /// from directory listing until finalized).
    fn synthetic_readdir(&self, dir: InodeId) -> Result<Vec<DirEntry>, FsError> {
        let inner = self.inner.lock();
        let synth = inner
            .synthetic_dirs
            .get(&dir)
            .ok_or(FsError::NotFound)?;
        let entries = synth
            .children
            .iter()
            .filter(|(name, inode)| {
                // For repo dirs, filter out pending workspaces.
                // A workspace child has a non-zero slot.
                let ino = **inode;
                if slot_of(ino) == 0 {
                    return true; // Not a workspace entry (e.g. root → repo).
                }
                let slot = slot_of(ino);
                // Check if there's a pending workspace in this slot.
                for repo in inner.repos.values() {
                    if let Some(ws) = repo.workspaces.get(name.as_str())
                        && ws.slot == slot
                        && ws.state == WorkspaceState::Pending
                    {
                        return false;
                    }
                }
                true
            })
            .map(|(name, &inode)| DirEntry {
                inode,
                name: name.clone(),
                kind: FileKind::Directory,
            })
            .collect();
        Ok(entries)
    }
}

/// Metadata copied out of the lock for phase-2 hydration.
struct HydrationInfo {
    slot: u32,
    root_tree_id: Vec<u8>,
    store: Arc<GitContentStore>,
    remote_store: Option<Arc<dyn RemoteStore>>,
    repo_name: String,
    ws_name: String,
    storage_dir: PathBuf,
}

// ── JjKikiFs implementation ─────────────────────────────────────────

#[async_trait]
impl JjKikiFs for RootFs {
    fn root(&self) -> InodeId {
        ROOT_INODE
    }

    async fn lookup(&self, parent: InodeId, name: &str) -> Result<InodeId, FsError> {
        let slot = slot_of(parent);
        if slot == 0 {
            self.synthetic_lookup(parent, name)
        } else {
            let fs = self.get_or_hydrate_by_slot(slot).await?;
            let local = fs.lookup(to_local(parent), name).await?;
            Ok(to_global(slot, local))
        }
    }

    async fn getattr(&self, ino: InodeId) -> Result<Attr, FsError> {
        let slot = slot_of(ino);
        if slot == 0 {
            // Could be a synthetic dir OR the root of a workspace
            // (workspace roots have inode = to_global(slot, ROOT_INODE),
            // but ROOT_INODE = 1 is in slot 0 range only for actual
            // synthetic dirs). Slot 0 means it's synthetic.
            self.synthetic_getattr(ino)
        } else {
            let fs = self.get_or_hydrate_by_slot(slot).await?;
            let attr = fs.getattr(to_local(ino)).await?;
            Ok(attr_to_global(slot, attr))
        }
    }

    async fn read(
        &self,
        ino: InodeId,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), FsError> {
        let slot = slot_of(ino);
        if slot == 0 {
            // Can't read a synthetic directory.
            Err(FsError::NotAFile)
        } else {
            let fs = self.get_or_hydrate_by_slot(slot).await?;
            fs.read(to_local(ino), offset, count).await
        }
    }

    async fn readdir(&self, dir: InodeId) -> Result<Vec<DirEntry>, FsError> {
        let slot = slot_of(dir);
        if slot == 0 {
            self.synthetic_readdir(dir)
        } else {
            let fs = self.get_or_hydrate_by_slot(slot).await?;
            let entries = fs.readdir(to_local(dir)).await?;
            Ok(entries
                .into_iter()
                .map(|mut e| {
                    e.inode = to_global(slot, e.inode);
                    e
                })
                .collect())
        }
    }

    async fn readlink(&self, ino: InodeId) -> Result<String, FsError> {
        let slot = slot_of(ino);
        if slot == 0 {
            Err(FsError::NotASymlink)
        } else {
            let fs = self.get_or_hydrate_by_slot(slot).await?;
            fs.readlink(to_local(ino)).await
        }
    }

    async fn check_out(&self, _new_root_tree: Id) -> Result<(), FsError> {
        // check_out is called directly on per-workspace KikiFs by the
        // service, never through the FUSE adapter on RootFs.
        Err(FsError::PermissionDenied)
    }

    // ── Write path: synthetic dirs reject mutations ─────────────

    async fn create_file(
        &self,
        parent: InodeId,
        name: &str,
        executable: bool,
    ) -> Result<(InodeId, Attr), FsError> {
        let slot = slot_of(parent);
        if slot == 0 {
            Err(FsError::PermissionDenied)
        } else {
            let fs = self.get_or_hydrate_by_slot(slot).await?;
            let (id, attr) = fs.create_file(to_local(parent), name, executable).await?;
            Ok((to_global(slot, id), attr_to_global(slot, attr)))
        }
    }

    async fn mkdir(
        &self,
        parent: InodeId,
        name: &str,
    ) -> Result<(InodeId, Attr), FsError> {
        let slot = slot_of(parent);
        if slot == 0 {
            Err(FsError::PermissionDenied)
        } else {
            let fs = self.get_or_hydrate_by_slot(slot).await?;
            let (id, attr) = fs.mkdir(to_local(parent), name).await?;
            Ok((to_global(slot, id), attr_to_global(slot, attr)))
        }
    }

    async fn symlink(
        &self,
        parent: InodeId,
        name: &str,
        target: &str,
    ) -> Result<(InodeId, Attr), FsError> {
        let slot = slot_of(parent);
        if slot == 0 {
            Err(FsError::PermissionDenied)
        } else {
            let fs = self.get_or_hydrate_by_slot(slot).await?;
            let (id, attr) = fs.symlink(to_local(parent), name, target).await?;
            Ok((to_global(slot, id), attr_to_global(slot, attr)))
        }
    }

    async fn write(
        &self,
        ino: InodeId,
        offset: u64,
        data: &[u8],
    ) -> Result<u32, FsError> {
        let slot = slot_of(ino);
        if slot == 0 {
            Err(FsError::PermissionDenied)
        } else {
            let fs = self.get_or_hydrate_by_slot(slot).await?;
            fs.write(to_local(ino), offset, data).await
        }
    }

    async fn setattr(
        &self,
        ino: InodeId,
        size: Option<u64>,
        executable: Option<bool>,
    ) -> Result<Attr, FsError> {
        let slot = slot_of(ino);
        if slot == 0 {
            Err(FsError::PermissionDenied)
        } else {
            let fs = self.get_or_hydrate_by_slot(slot).await?;
            let attr = fs.setattr(to_local(ino), size, executable).await?;
            Ok(attr_to_global(slot, attr))
        }
    }

    async fn remove(&self, parent: InodeId, name: &str) -> Result<(), FsError> {
        let slot = slot_of(parent);
        if slot == 0 {
            Err(FsError::PermissionDenied)
        } else {
            let fs = self.get_or_hydrate_by_slot(slot).await?;
            fs.remove(to_local(parent), name).await
        }
    }

    async fn rename(
        &self,
        parent: InodeId,
        name: &str,
        new_parent: InodeId,
        new_name: &str,
    ) -> Result<(), FsError> {
        let src_slot = slot_of(parent);
        let dst_slot = slot_of(new_parent);
        // Reject cross-workspace rename.
        if src_slot != dst_slot {
            return Err(FsError::CrossDevice);
        }
        if src_slot == 0 {
            return Err(FsError::PermissionDenied);
        }
        let fs = self.get_or_hydrate_by_slot(src_slot).await?;
        fs.rename(to_local(parent), name, to_local(new_parent), new_name)
            .await
    }

    async fn snapshot(&self) -> Result<Id, FsError> {
        // snapshot is called directly on per-workspace KikiFs by the
        // service, never through the FUSE adapter on RootFs.
        Err(FsError::PermissionDenied)
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use jj_lib::object_id::ObjectId as _;
    use proptest::prelude::*;

    // ── Inode encoding ──────────────────────────────────────────

    #[test]
    fn slot_zero_root_inode() {
        assert_eq!(slot_of(ROOT_INODE), 0);
        assert_eq!(to_local(ROOT_INODE), ROOT_INODE);
    }

    #[test]
    fn to_global_to_local_round_trip() {
        let slot = 42u32;
        let local = 1234u64;
        let global = to_global(slot, local);
        assert_eq!(slot_of(global), slot);
        assert_eq!(to_local(global), local);
    }

    #[test]
    fn slot_zero_synthetic_inodes_stay_in_slot_zero() {
        for ino in 1..1024u64 {
            assert_eq!(slot_of(ino), 0, "inode {ino} should be in slot 0");
        }
    }

    #[test]
    fn workspace_root_inode_is_in_correct_slot() {
        let slot = 1u32;
        let global = to_global(slot, ROOT_INODE);
        assert_eq!(slot_of(global), 1);
        assert_eq!(to_local(global), ROOT_INODE);
    }

    #[test]
    fn max_slot_round_trips() {
        // 24-bit slot max = (1 << 24) - 1 = 16_777_215
        let max_slot = (1u32 << 24) - 1;
        let local = (1u64 << 40) - 1; // max local inode
        let global = to_global(max_slot, local);
        assert_eq!(slot_of(global), max_slot);
        assert_eq!(to_local(global), local);
    }

    proptest! {
        #[test]
        fn inode_encoding_round_trip(
            slot in 0u32..(1u32 << 24),
            local in 0u64..(1u64 << 40),
        ) {
            let global = to_global(slot, local);
            prop_assert_eq!(slot_of(global), slot);
            prop_assert_eq!(to_local(global), local);
        }

        #[test]
        fn different_slots_produce_different_globals(
            slot_a in 1u32..(1u32 << 24),
            slot_b in 1u32..(1u32 << 24),
            local in 1u64..(1u64 << 40),
        ) {
            prop_assume!(slot_a != slot_b);
            let ga = to_global(slot_a, local);
            let gb = to_global(slot_b, local);
            prop_assert_ne!(ga, gb);
        }
    }

    // ── Synthetic directory layer ───────────────────────────────

    #[test]
    fn empty_root_readdir() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let entries = fs.synthetic_readdir(ROOT_INODE).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn register_repo_shows_in_readdir() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let store = test_store();
        fs.register_repo(
            "monorepo".into(),
            "kiki+ssh://server/mono".into(),
            store,
            None,
        );
        let entries = fs.synthetic_readdir(ROOT_INODE).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "monorepo");
        assert_eq!(entries[0].kind, FileKind::Directory);
        // Repo dir inode should be in slot 0.
        assert_eq!(slot_of(entries[0].inode), 0);
    }

    #[test]
    fn register_workspace_active_shows_in_repo_dir() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let store = test_store();
        fs.register_repo("repo".into(), "dir:///tmp/r".into(), Arc::clone(&store), None);
        fs.register_workspace(
            "repo",
            ws_reg("default", 1, WorkspaceState::Active, vec![0; 20], b"default".to_vec()),
        )
        .unwrap();

        // Lookup repo dir.
        let repo_ino = fs.synthetic_lookup(ROOT_INODE, "repo").unwrap();
        let entries = fs.synthetic_readdir(repo_ino).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "default");
        // Workspace root inode should be to_global(slot=1, ROOT_INODE).
        assert_eq!(entries[0].inode, to_global(1, ROOT_INODE));
        assert_eq!(slot_of(entries[0].inode), 1);
    }

    #[test]
    fn pending_workspace_hidden_from_readdir_but_accessible_via_lookup() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let store = test_store();
        fs.register_repo("repo".into(), "dir:///tmp/r".into(), Arc::clone(&store), None);
        fs.register_workspace(
            "repo",
            ws_reg("wip", 1, WorkspaceState::Pending, vec![0; 20], vec![]),
        )
        .unwrap();

        let repo_ino = fs.synthetic_lookup(ROOT_INODE, "repo").unwrap();
        let entries = fs.synthetic_readdir(repo_ino).unwrap();
        assert!(entries.is_empty(), "pending workspace should be hidden from readdir");

        // Lookup should succeed (needed for CLI to write .jj/ during init).
        let result = fs.synthetic_lookup(repo_ino, "wip");
        assert_eq!(result, Ok(to_global(1, ROOT_INODE)));
    }

    #[test]
    fn finalize_workspace_makes_visible() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let store = test_store();
        fs.register_repo("repo".into(), "dir:///tmp/r".into(), Arc::clone(&store), None);
        fs.register_workspace(
            "repo",
            ws_reg("wip", 1, WorkspaceState::Pending, vec![0; 20], vec![]),
        )
        .unwrap();

        // Finalize.
        fs.finalize_workspace("repo", "wip").unwrap();

        let repo_ino = fs.synthetic_lookup(ROOT_INODE, "repo").unwrap();
        let entries = fs.synthetic_readdir(repo_ino).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "wip");
    }

    #[test]
    fn remove_workspace_retires_slot() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let store = test_store();
        fs.register_repo("repo".into(), "dir:///tmp/r".into(), Arc::clone(&store), None);
        fs.register_workspace(
            "repo",
            ws_reg("temp", 1, WorkspaceState::Active, vec![0; 20], vec![]),
        )
        .unwrap();

        let deleted_slot = fs.remove_workspace("repo", "temp").unwrap();
        assert_eq!(deleted_slot, 1);

        // Repo dir should be empty now.
        let repo_ino = fs.synthetic_lookup(ROOT_INODE, "repo").unwrap();
        let entries = fs.synthetic_readdir(repo_ino).unwrap();
        assert!(entries.is_empty());

        // Next slot should be higher (monotonic, never reused).
        let next = fs.alloc_slot();
        assert!(next > deleted_slot, "slot must not be reused");
    }

    #[test]
    fn multiple_repos_and_workspaces() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let store1 = test_store();
        let store2 = test_store();
        fs.register_repo("alpha".into(), "dir:///a".into(), store1, None);
        fs.register_repo("beta".into(), "dir:///b".into(), store2, None);

        // Workspaces in alpha.
        fs.register_workspace(
            "alpha",
            ws_reg("default", 1, WorkspaceState::Active, vec![0; 20], b"default".to_vec()),
        ).unwrap();
        fs.register_workspace(
            "alpha",
            ws_reg("fix", 2, WorkspaceState::Active, vec![0; 20], b"fix".to_vec()),
        ).unwrap();

        // Workspace in beta.
        fs.register_workspace(
            "beta",
            ws_reg("default", 3, WorkspaceState::Active, vec![0; 20], b"default".to_vec()),
        ).unwrap();

        // Root has two repos.
        let root_entries = fs.synthetic_readdir(ROOT_INODE).unwrap();
        assert_eq!(root_entries.len(), 2);
        let names: Vec<&str> = root_entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));

        // Alpha has two workspaces.
        let alpha_ino = fs.synthetic_lookup(ROOT_INODE, "alpha").unwrap();
        let alpha_ws = fs.synthetic_readdir(alpha_ino).unwrap();
        assert_eq!(alpha_ws.len(), 2);

        // Beta has one workspace.
        let beta_ino = fs.synthetic_lookup(ROOT_INODE, "beta").unwrap();
        let beta_ws = fs.synthetic_readdir(beta_ino).unwrap();
        assert_eq!(beta_ws.len(), 1);

        // All workspace inodes are in different slots.
        let mut slots: Vec<u32> = vec![];
        for e in alpha_ws.iter().chain(beta_ws.iter()) {
            let s = slot_of(e.inode);
            assert!(!slots.contains(&s), "slot {s} already used");
            slots.push(s);
        }
    }

    #[test]
    fn duplicate_repo_register_is_noop() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let store = test_store();
        fs.register_repo("repo".into(), "url1".into(), Arc::clone(&store), None);
        fs.register_repo("repo".into(), "url2".into(), store, None);

        let entries = fs.synthetic_readdir(ROOT_INODE).unwrap();
        assert_eq!(entries.len(), 1, "duplicate register should be noop");
    }

    #[test]
    fn duplicate_workspace_returns_already_exists() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let store = test_store();
        fs.register_repo("repo".into(), "url".into(), Arc::clone(&store), None);
        fs.register_workspace(
            "repo",
            ws_reg("ws", 1, WorkspaceState::Active, vec![0; 20], vec![]),
        ).unwrap();
        let err = fs.register_workspace(
            "repo",
            ws_reg("ws", 2, WorkspaceState::Active, vec![0; 20], vec![]),
        );
        assert_eq!(err, Err(FsError::AlreadyExists));
    }

    #[test]
    fn workspace_in_nonexistent_repo_returns_not_found() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let err = fs.register_workspace(
            "nope",
            ws_reg("ws", 1, WorkspaceState::Active, vec![0; 20], vec![]),
        );
        assert_eq!(err, Err(FsError::NotFound));
    }

    #[test]
    fn synthetic_getattr_root() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let attr = fs.synthetic_getattr(ROOT_INODE).unwrap();
        assert_eq!(attr.inode, ROOT_INODE);
        assert_eq!(attr.kind, FileKind::Directory);
    }

    #[test]
    fn synthetic_getattr_repo_dir() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let store = test_store();
        fs.register_repo("repo".into(), "url".into(), store, None);
        let ino = fs.synthetic_lookup(ROOT_INODE, "repo").unwrap();
        let attr = fs.synthetic_getattr(ino).unwrap();
        assert_eq!(attr.inode, ino);
        assert_eq!(attr.kind, FileKind::Directory);
    }

    #[test]
    fn synthetic_lookup_not_found() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        assert_eq!(
            fs.synthetic_lookup(ROOT_INODE, "nope"),
            Err(FsError::NotFound)
        );
    }

    #[test]
    fn alloc_slot_is_monotonic() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let s1 = fs.alloc_slot();
        let s2 = fs.alloc_slot();
        let s3 = fs.alloc_slot();
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(s3, 3);
    }

    // ── JjKikiFs async trait tests ──────────────────────────────

    #[tokio::test]
    async fn jjkikifs_lookup_root_not_found() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let result = fs.lookup(ROOT_INODE, "nonexistent").await;
        assert_eq!(result, Err(FsError::NotFound));
    }

    #[tokio::test]
    async fn jjkikifs_getattr_root() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let attr = fs.getattr(ROOT_INODE).await.unwrap();
        assert_eq!(attr.inode, ROOT_INODE);
        assert_eq!(attr.kind, FileKind::Directory);
    }

    #[tokio::test]
    async fn jjkikifs_readdir_root_with_repos() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let store = test_store();
        fs.register_repo("myrepo".into(), "url".into(), store, None);
        let entries = fs.readdir(ROOT_INODE).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "myrepo");
    }

    #[tokio::test]
    async fn jjkikifs_read_synthetic_returns_not_a_file() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        assert_eq!(
            fs.read(ROOT_INODE, 0, 100).await,
            Err(FsError::NotAFile)
        );
    }

    #[tokio::test]
    async fn jjkikifs_readlink_synthetic_returns_not_a_symlink() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        assert_eq!(
            fs.readlink(ROOT_INODE).await,
            Err(FsError::NotASymlink)
        );
    }

    #[tokio::test]
    async fn jjkikifs_mutation_on_synthetic_returns_permission_denied() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );

        // create_file
        assert_eq!(
            fs.create_file(ROOT_INODE, "foo", false).await.unwrap_err(),
            FsError::PermissionDenied,
        );
        // mkdir
        assert_eq!(
            fs.mkdir(ROOT_INODE, "bar").await.unwrap_err(),
            FsError::PermissionDenied,
        );
        // symlink
        assert_eq!(
            fs.symlink(ROOT_INODE, "lnk", "/target").await.unwrap_err(),
            FsError::PermissionDenied,
        );
        // write
        assert_eq!(
            fs.write(ROOT_INODE, 0, b"data").await,
            Err(FsError::PermissionDenied),
        );
        // setattr
        assert_eq!(
            fs.setattr(ROOT_INODE, Some(0), None).await.unwrap_err(),
            FsError::PermissionDenied,
        );
        // remove
        assert_eq!(
            fs.remove(ROOT_INODE, "x").await,
            Err(FsError::PermissionDenied),
        );
        // rename (within synthetic)
        assert_eq!(
            fs.rename(ROOT_INODE, "a", ROOT_INODE, "b").await,
            Err(FsError::PermissionDenied),
        );
    }

    #[tokio::test]
    async fn jjkikifs_cross_workspace_rename_returns_cross_device() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        let src_ino = to_global(1, 100);
        let dst_ino = to_global(2, 200);
        // This will fail with CrossDevice before trying to hydrate
        // (different slots).
        assert_eq!(
            fs.rename(src_ino, "a", dst_ino, "b").await,
            Err(FsError::CrossDevice),
        );
    }

    #[tokio::test]
    async fn jjkikifs_check_out_and_snapshot_return_permission_denied() {
        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            PathBuf::from("/tmp/storage"),
            1,
        );
        assert_eq!(
            fs.check_out(Id([0; 20])).await,
            Err(FsError::PermissionDenied),
        );
        assert_eq!(fs.snapshot().await, Err(FsError::PermissionDenied));
    }

    // ── Integration test with real KikiFs ───────────────────────

    #[tokio::test]
    async fn workspace_delegation_through_root_fs() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();

        // Create a GitContentStore.
        let settings = test_settings();
        let store_path = storage.join("repos/testrepo/git_store");
        let redb_path = storage.join("repos/testrepo/store.redb");
        let store = Arc::new(GitContentStore::init(&settings, &store_path, &redb_path).unwrap());
        let empty_tree = Id(store.empty_tree_id().as_bytes().try_into().unwrap());

        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            storage.to_path_buf(),
            1,
        );
        fs.register_repo("testrepo".into(), "dir:///test".into(), Arc::clone(&store), None);
        fs.register_workspace(
            "testrepo",
            ws_reg("default", 1, WorkspaceState::Active, empty_tree.0.to_vec(), b"default".to_vec()),
        )
        .unwrap();

        // Lookup repo.
        let repo_ino = fs.lookup(ROOT_INODE, "testrepo").await.unwrap();
        assert_eq!(slot_of(repo_ino), 0);

        // Lookup workspace -> workspace root inode.
        let ws_root = fs.lookup(repo_ino, "default").await.unwrap();
        assert_eq!(slot_of(ws_root), 1);
        assert_eq!(to_local(ws_root), ROOT_INODE);

        // getattr on workspace root.
        let attr = fs.getattr(ws_root).await.unwrap();
        assert_eq!(attr.kind, FileKind::Directory);
        assert_eq!(slot_of(attr.inode), 1);

        // readdir on workspace root — empty tree should have .git
        // (synthesized by KikiFs).
        let entries = fs.readdir(ws_root).await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&".git"), "expected .git in workspace root, got: {names:?}");
        // All entry inodes should be in slot 1.
        for e in &entries {
            assert_eq!(slot_of(e.inode), 1, "entry {} should be in slot 1", e.name);
        }
    }

    #[tokio::test]
    async fn lazy_hydration_deferred() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();

        let settings = test_settings();
        let store_path = storage.join("repos/repo/git_store");
        let redb_path = storage.join("repos/repo/store.redb");
        let store = Arc::new(GitContentStore::init(&settings, &store_path, &redb_path).unwrap());
        let empty_tree = Id(store.empty_tree_id().as_bytes().try_into().unwrap());

        let root_fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            storage.to_path_buf(),
            1,
        );
        root_fs.register_repo("repo".into(), "dir:///test".into(), Arc::clone(&store), None);
        root_fs.register_workspace(
            "repo",
            ws_reg("ws1", 1, WorkspaceState::Active, empty_tree.0.to_vec(), b"ws1".to_vec()),
        )
        .unwrap();

        // Before any workspace access, live map should be empty.
        {
            let inner = root_fs.inner.lock();
            assert!(inner.live.is_empty(), "no workspace should be hydrated yet");
        }

        // readdir on repo dir shows workspace names (no hydration needed).
        let repo_ino = root_fs.lookup(ROOT_INODE, "repo").await.unwrap();
        let entries = root_fs.readdir(repo_ino).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "ws1");
        {
            let inner = root_fs.inner.lock();
            assert!(inner.live.is_empty(), "readdir on repo dir should not hydrate");
        }

        // Now access the workspace (triggers hydration).
        let ws_root = root_fs.lookup(repo_ino, "ws1").await.unwrap();
        let _entries = root_fs.readdir(ws_root).await.unwrap();
        {
            let inner = root_fs.inner.lock();
            assert!(inner.live.contains_key(&1), "workspace should be hydrated now");
        }
    }

    // ── Test helpers ────────────────────────────────────────────

    fn test_settings() -> jj_lib::settings::UserSettings {
        let toml_str = r#"
            user.name = "Test User"
            user.email = "test@example.com"
            operation.hostname = "test"
            operation.username = "test"
            debug.randomness-seed = 42
        "#;
        let mut config = jj_lib::config::StackedConfig::with_defaults();
        config.add_layer(
            jj_lib::config::ConfigLayer::parse(
                jj_lib::config::ConfigSource::User,
                toml_str,
            )
            .unwrap(),
        );
        jj_lib::settings::UserSettings::from_config(config).unwrap()
    }

    fn test_store() -> Arc<GitContentStore> {
        let settings = test_settings();
        Arc::new(GitContentStore::new_in_memory(&settings))
    }

    /// Shorthand for building a WorkspaceRegistration in tests.
    fn ws_reg(
        name: &str,
        slot: u32,
        state: WorkspaceState,
        root_tree_id: Vec<u8>,
        workspace_id: Vec<u8>,
    ) -> WorkspaceRegistration {
        WorkspaceRegistration {
            name: name.into(),
            slot,
            state,
            root_tree_id,
            op_id: vec![],
            workspace_id,
        }
    }

    /// Build a RootFs with a real on-disk store and one active workspace.
    /// Returns `(RootFs, store, tempdir_guard)`.
    fn setup_one_workspace(
        repo_name: &str,
        ws_name: &str,
        slot: u32,
    ) -> (RootFs, Arc<GitContentStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();
        let settings = test_settings();
        let store_path = storage.join(format!("repos/{repo_name}/git_store"));
        let redb_path = storage.join(format!("repos/{repo_name}/store.redb"));
        let store = Arc::new(
            GitContentStore::init(&settings, &store_path, &redb_path).unwrap(),
        );
        let empty_tree = store.empty_tree_id().as_bytes().to_vec();

        // Create scratch dir for the workspace.
        let scratch = crate::repo_meta::workspace_scratch_dir(storage, repo_name, ws_name);
        std::fs::create_dir_all(&scratch).unwrap();

        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            storage.to_path_buf(),
            slot,
        );
        fs.register_repo(
            repo_name.into(),
            format!("dir:///test/{repo_name}"),
            Arc::clone(&store),
            None,
        );
        fs.register_workspace(
            repo_name,
            ws_reg(ws_name, slot, WorkspaceState::Active, empty_tree, ws_name.as_bytes().to_vec()),
        )
        .unwrap();

        (fs, store, dir)
    }

    /// Build a RootFs with one repo and two active workspaces.
    fn setup_two_workspaces(
        repo_name: &str,
    ) -> (RootFs, Arc<GitContentStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();
        let settings = test_settings();
        let store_path = storage.join(format!("repos/{repo_name}/git_store"));
        let redb_path = storage.join(format!("repos/{repo_name}/store.redb"));
        let store = Arc::new(
            GitContentStore::init(&settings, &store_path, &redb_path).unwrap(),
        );
        let empty_tree = store.empty_tree_id().as_bytes().to_vec();

        for ws in ["default", "feature"] {
            let scratch = crate::repo_meta::workspace_scratch_dir(storage, repo_name, ws);
            std::fs::create_dir_all(&scratch).unwrap();
        }

        let fs = RootFs::new(
            PathBuf::from("/mnt/kiki"),
            storage.to_path_buf(),
            1,
        );
        fs.register_repo(
            repo_name.into(),
            format!("dir:///test/{repo_name}"),
            Arc::clone(&store),
            None,
        );
        fs.register_workspace(
            repo_name,
            ws_reg("default", 1, WorkspaceState::Active, empty_tree.clone(), b"default".to_vec()),
        )
        .unwrap();
        fs.register_workspace(
            repo_name,
            ws_reg("feature", 2, WorkspaceState::Active, empty_tree, b"feature".to_vec()),
        )
        .unwrap();

        (fs, store, dir)
    }

    // ── Write-path delegation ──────────────────────────────────────

    #[tokio::test]
    async fn write_and_read_through_rootfs() {
        let (fs, _store, _dir) = setup_one_workspace("repo", "default", 1);

        // Navigate to workspace root.
        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();
        let ws_root = fs.lookup(repo_ino, "default").await.unwrap();
        assert_eq!(slot_of(ws_root), 1);

        // Create a file.
        let (file_ino, attr) = fs.create_file(ws_root, "hello.txt", false).await.unwrap();
        assert_eq!(slot_of(file_ino), 1, "file inode must be in workspace slot");
        assert_eq!(slot_of(attr.inode), 1, "attr inode must be in workspace slot");
        assert_eq!(attr.kind, FileKind::Regular);

        // Write data.
        let written = fs.write(file_ino, 0, b"hello world").await.unwrap();
        assert_eq!(written, 11);

        // Read it back.
        let (data, eof) = fs.read(file_ino, 0, 4096).await.unwrap();
        assert_eq!(data, b"hello world");
        assert!(eof);

        // getattr should reflect the size.
        let attr = fs.getattr(file_ino).await.unwrap();
        assert_eq!(attr.size, 11);
        assert_eq!(slot_of(attr.inode), 1);
    }

    #[tokio::test]
    async fn mkdir_and_create_file_inside() {
        let (fs, _store, _dir) = setup_one_workspace("repo", "ws", 1);

        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();
        let ws_root = fs.lookup(repo_ino, "ws").await.unwrap();

        // mkdir src/
        let (dir_ino, dir_attr) = fs.mkdir(ws_root, "src").await.unwrap();
        assert_eq!(slot_of(dir_ino), 1);
        assert_eq!(dir_attr.kind, FileKind::Directory);

        // Create file inside src/
        let (file_ino, _) = fs.create_file(dir_ino, "main.rs", false).await.unwrap();
        assert_eq!(slot_of(file_ino), 1);
        fs.write(file_ino, 0, b"fn main() {}").await.unwrap();

        // readdir on src/ should include main.rs
        let entries = fs.readdir(dir_ino).await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"main.rs"));
        for e in &entries {
            assert_eq!(slot_of(e.inode), 1, "entry {} inode in wrong slot", e.name);
        }
    }

    #[tokio::test]
    async fn symlink_through_rootfs() {
        let (fs, _store, _dir) = setup_one_workspace("repo", "ws", 1);

        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();
        let ws_root = fs.lookup(repo_ino, "ws").await.unwrap();

        let (link_ino, attr) = fs.symlink(ws_root, "link", "/target/path").await.unwrap();
        assert_eq!(slot_of(link_ino), 1);
        assert_eq!(attr.kind, FileKind::Symlink);

        let target = fs.readlink(link_ino).await.unwrap();
        assert_eq!(target, "/target/path");
    }

    #[tokio::test]
    async fn remove_through_rootfs() {
        let (fs, _store, _dir) = setup_one_workspace("repo", "ws", 1);

        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();
        let ws_root = fs.lookup(repo_ino, "ws").await.unwrap();

        fs.create_file(ws_root, "doomed.txt", false).await.unwrap();
        assert!(fs.lookup(ws_root, "doomed.txt").await.is_ok());

        fs.remove(ws_root, "doomed.txt").await.unwrap();
        assert_eq!(
            fs.lookup(ws_root, "doomed.txt").await,
            Err(FsError::NotFound)
        );
    }

    #[tokio::test]
    async fn rename_within_workspace() {
        let (fs, _store, _dir) = setup_one_workspace("repo", "ws", 1);

        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();
        let ws_root = fs.lookup(repo_ino, "ws").await.unwrap();

        let (file_ino, _) = fs.create_file(ws_root, "old.txt", false).await.unwrap();
        fs.write(file_ino, 0, b"data").await.unwrap();

        fs.rename(ws_root, "old.txt", ws_root, "new.txt").await.unwrap();

        assert_eq!(fs.lookup(ws_root, "old.txt").await, Err(FsError::NotFound));
        let new_ino = fs.lookup(ws_root, "new.txt").await.unwrap();
        let (data, _) = fs.read(new_ino, 0, 4096).await.unwrap();
        assert_eq!(data, b"data");
    }

    #[tokio::test]
    async fn setattr_through_rootfs() {
        let (fs, _store, _dir) = setup_one_workspace("repo", "ws", 1);

        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();
        let ws_root = fs.lookup(repo_ino, "ws").await.unwrap();

        let (file_ino, _) = fs.create_file(ws_root, "f.txt", false).await.unwrap();
        fs.write(file_ino, 0, b"12345").await.unwrap();

        // Truncate via setattr.
        let attr = fs.setattr(file_ino, Some(3), None).await.unwrap();
        assert_eq!(attr.size, 3);
        assert_eq!(slot_of(attr.inode), 1);

        // Set executable.
        let attr = fs.setattr(file_ino, None, Some(true)).await.unwrap();
        assert!(attr.executable);
    }

    // ── Workspace isolation ────────────────────────────────────────

    #[tokio::test]
    async fn workspace_isolation_dirty_state() {
        let (fs, _store, _dir) = setup_two_workspaces("repo");

        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();

        // Write a file in workspace "default".
        let ws_default = fs.lookup(repo_ino, "default").await.unwrap();
        let (file_ino, _) = fs.create_file(ws_default, "only-in-default.txt", false)
            .await.unwrap();
        fs.write(file_ino, 0, b"default data").await.unwrap();

        // Workspace "feature" should not see it.
        let ws_feature = fs.lookup(repo_ino, "feature").await.unwrap();
        assert_eq!(
            fs.lookup(ws_feature, "only-in-default.txt").await,
            Err(FsError::NotFound),
            "dirty state in 'default' must not leak to 'feature'"
        );

        // Write a different file in "feature".
        let (feat_file, _) = fs.create_file(ws_feature, "feature-only.txt", false)
            .await.unwrap();
        fs.write(feat_file, 0, b"feature data").await.unwrap();

        // "default" should not see it.
        assert_eq!(
            fs.lookup(ws_default, "feature-only.txt").await,
            Err(FsError::NotFound),
            "dirty state in 'feature' must not leak to 'default'"
        );

        // Each workspace reads its own file correctly.
        let (data, _) = fs.read(file_ino, 0, 4096).await.unwrap();
        assert_eq!(data, b"default data");
        let (data, _) = fs.read(feat_file, 0, 4096).await.unwrap();
        assert_eq!(data, b"feature data");
    }

    #[tokio::test]
    async fn workspace_inodes_dont_collide() {
        let (fs, _store, _dir) = setup_two_workspaces("repo");

        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();
        let ws_a = fs.lookup(repo_ino, "default").await.unwrap();
        let ws_b = fs.lookup(repo_ino, "feature").await.unwrap();

        // Create files with the same name in both workspaces.
        let (ino_a, _) = fs.create_file(ws_a, "same.txt", false).await.unwrap();
        let (ino_b, _) = fs.create_file(ws_b, "same.txt", false).await.unwrap();

        // Global inodes must differ (different slots).
        assert_ne!(ino_a, ino_b, "same filename in different workspaces must have different global inodes");
        assert_ne!(slot_of(ino_a), slot_of(ino_b));

        // Write different content to each.
        fs.write(ino_a, 0, b"aaa").await.unwrap();
        fs.write(ino_b, 0, b"bbb").await.unwrap();

        // Read back — each file has independent content.
        let (a_data, _) = fs.read(ino_a, 0, 4096).await.unwrap();
        let (b_data, _) = fs.read(ino_b, 0, 4096).await.unwrap();
        assert_eq!(a_data, b"aaa");
        assert_eq!(b_data, b"bbb");
    }

    // ── Shared store ───────────────────────────────────────────────

    #[tokio::test]
    async fn shared_store_across_workspaces() {
        let (fs, _store, _dir) = setup_two_workspaces("repo");

        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();

        // Write and snapshot in workspace "default".
        let ws_default = fs.lookup(repo_ino, "default").await.unwrap();
        let default_kikifs = {
            let inner = fs.inner.lock();
            // Trigger hydration first.
            drop(inner);
            // Access via get_workspace to get the KikiFs handle.
            let handle = fs.get_workspace("repo", "default").await.unwrap();
            handle.fs
        };
        let (file_ino, _) = fs.create_file(ws_default, "shared.txt", false)
            .await.unwrap();
        fs.write(file_ino, 0, b"shared content").await.unwrap();

        // Snapshot persists to the shared git store.
        let root_tree = default_kikifs.snapshot().await.unwrap();

        // The shared store should have the blob.
        // Check out the same tree in workspace "feature".
        let feature_kikifs = {
            let handle = fs.get_workspace("repo", "feature").await.unwrap();
            handle.fs
        };
        feature_kikifs.check_out(root_tree).await.unwrap();

        // Now read the file through RootFs in "feature".
        let ws_feature = fs.lookup(repo_ino, "feature").await.unwrap();
        let feat_file = fs.lookup(ws_feature, "shared.txt").await.unwrap();
        let (data, _) = fs.read(feat_file, 0, 4096).await.unwrap();
        assert_eq!(data, b"shared content", "shared store should serve the same blob");

        // Confirm only one git_store exists (store Arc identity).
        let handle_a = fs.get_workspace("repo", "default").await.unwrap();
        let handle_b = fs.get_workspace("repo", "feature").await.unwrap();
        assert!(
            Arc::ptr_eq(&handle_a.store, &handle_b.store),
            "both workspaces must share the same GitContentStore"
        );
    }

    // ── Concurrent workspace access ────────────────────────────────

    #[tokio::test]
    async fn concurrent_workspace_access() {
        let (fs, _store, _dir) = setup_two_workspaces("repo");
        let fs = Arc::new(fs);

        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();
        let ws_a = fs.lookup(repo_ino, "default").await.unwrap();
        let ws_b = fs.lookup(repo_ino, "feature").await.unwrap();

        // Spawn two tasks that write concurrently to different workspaces.
        let fs_a = Arc::clone(&fs);
        let task_a = tokio::spawn(async move {
            for i in 0..10 {
                let name = format!("file_a_{i}.txt");
                let (ino, _) = fs_a.create_file(ws_a, &name, false).await.unwrap();
                let data = format!("data_a_{i}");
                fs_a.write(ino, 0, data.as_bytes()).await.unwrap();
            }
        });

        let fs_b = Arc::clone(&fs);
        let task_b = tokio::spawn(async move {
            for i in 0..10 {
                let name = format!("file_b_{i}.txt");
                let (ino, _) = fs_b.create_file(ws_b, &name, false).await.unwrap();
                let data = format!("data_b_{i}");
                fs_b.write(ino, 0, data.as_bytes()).await.unwrap();
            }
        });

        // Both tasks should complete without deadlock or panic.
        let (ra, rb) = tokio::join!(task_a, task_b);
        ra.unwrap();
        rb.unwrap();

        // Verify files in each workspace.
        let entries_a = fs.readdir(ws_a).await.unwrap();
        let names_a: Vec<&str> = entries_a.iter().map(|e| e.name.as_str()).collect();
        for i in 0..10 {
            let name = format!("file_a_{i}.txt");
            assert!(names_a.contains(&name.as_str()), "missing {name} in workspace A");
        }
        // No workspace B files in A.
        assert!(
            !names_a.iter().any(|n| n.starts_with("file_b_")),
            "workspace B files leaked into A"
        );

        let entries_b = fs.readdir(ws_b).await.unwrap();
        let names_b: Vec<&str> = entries_b.iter().map(|e| e.name.as_str()).collect();
        for i in 0..10 {
            let name = format!("file_b_{i}.txt");
            assert!(names_b.contains(&name.as_str()), "missing {name} in workspace B");
        }
        assert!(
            !names_b.iter().any(|n| n.starts_with("file_a_")),
            "workspace A files leaked into B"
        );
    }

    // ── Workspace delete + slot monotonicity (end-to-end) ──────────

    #[tokio::test]
    async fn delete_workspace_with_files_and_new_slot_is_higher() {
        let (fs, store, _dir) = setup_two_workspaces("repo");

        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();
        let ws_feat = fs.lookup(repo_ino, "feature").await.unwrap();

        // Write files into "feature" to ensure it's hydrated.
        let (file_ino, _) = fs.create_file(ws_feat, "doomed.txt", false).await.unwrap();
        fs.write(file_ino, 0, b"will be deleted").await.unwrap();

        // Delete workspace "feature" (slot 2).
        let deleted_slot = fs.remove_workspace("repo", "feature").unwrap();
        assert_eq!(deleted_slot, 2);

        // Verify "feature" is gone from readdir.
        let entries = fs.readdir(repo_ino).await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(!names.contains(&"feature"), "deleted workspace should not appear");
        assert!(names.contains(&"default"), "other workspace should remain");

        // Accessing deleted workspace inode returns error.
        assert!(fs.readdir(ws_feat).await.is_err());

        // Create a new workspace — its slot must be > deleted_slot.
        let scratch = crate::repo_meta::workspace_scratch_dir(
            _dir.path(), "repo", "new-ws",
        );
        std::fs::create_dir_all(&scratch).unwrap();
        let new_slot = fs.alloc_slot();
        assert!(new_slot > deleted_slot, "new slot {new_slot} must be higher than deleted {deleted_slot}");

        fs.register_workspace(
            "repo",
            ws_reg("new-ws", new_slot, WorkspaceState::Active, store.empty_tree_id().as_bytes().to_vec(), b"new-ws".to_vec()),
        ).unwrap();

        let entries = fs.readdir(repo_ino).await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"new-ws"));
        assert!(names.contains(&"default"));
        assert_eq!(entries.len(), 2);
    }

    // ── Mutation on repo-dir synthetic (deeper than root) ──────────

    #[tokio::test]
    async fn mutation_on_repo_dir_returns_permission_denied() {
        let (fs, _store, _dir) = setup_one_workspace("repo", "ws", 1);

        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();
        assert_eq!(slot_of(repo_ino), 0);

        // All mutations on the repo dir should be rejected.
        assert_eq!(fs.create_file(repo_ino, "x", false).await.unwrap_err(), FsError::PermissionDenied);
        assert_eq!(fs.mkdir(repo_ino, "x").await.unwrap_err(), FsError::PermissionDenied);
        assert_eq!(fs.symlink(repo_ino, "x", "/t").await.unwrap_err(), FsError::PermissionDenied);
        assert_eq!(fs.write(repo_ino, 0, b"x").await, Err(FsError::PermissionDenied));
        assert_eq!(fs.setattr(repo_ino, Some(0), None).await.unwrap_err(), FsError::PermissionDenied);
        assert_eq!(fs.remove(repo_ino, "x").await, Err(FsError::PermissionDenied));
        assert_eq!(fs.rename(repo_ino, "a", repo_ino, "b").await, Err(FsError::PermissionDenied));
        assert_eq!(fs.read(repo_ino, 0, 100).await, Err(FsError::NotAFile));
        assert_eq!(fs.readlink(repo_ino).await, Err(FsError::NotASymlink));
    }

    // ── Cross-workspace rename with real workspaces ────────────────

    #[tokio::test]
    async fn cross_workspace_rename_returns_exdev() {
        let (fs, _store, _dir) = setup_two_workspaces("repo");

        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();
        let ws_a = fs.lookup(repo_ino, "default").await.unwrap();
        let ws_b = fs.lookup(repo_ino, "feature").await.unwrap();

        // Create a file in workspace A.
        let (file_ino, _) = fs.create_file(ws_a, "moveme.txt", false).await.unwrap();
        fs.write(file_ino, 0, b"test").await.unwrap();

        // Attempt to rename to workspace B root.
        assert_eq!(
            fs.rename(ws_a, "moveme.txt", ws_b, "moved.txt").await,
            Err(FsError::CrossDevice),
            "rename across workspaces must return EXDEV"
        );

        // File still exists in A.
        assert!(fs.lookup(ws_a, "moveme.txt").await.is_ok());
    }

    // ── Concurrent hydration race ──────────────────────────────────

    #[tokio::test]
    async fn concurrent_hydration_race() {
        let (fs, _store, _dir) = setup_one_workspace("repo", "ws", 1);
        let fs = Arc::new(fs);

        let repo_ino = fs.lookup(ROOT_INODE, "repo").await.unwrap();

        // Spawn multiple tasks that all trigger hydration simultaneously.
        let mut handles = Vec::new();
        for _ in 0..5 {
            let fs = Arc::clone(&fs);
            handles.push(tokio::spawn(async move {
                let ws_root = fs.lookup(repo_ino, "ws").await.unwrap();
                let entries = fs.readdir(ws_root).await.unwrap();
                (ws_root, entries.len())
            }));
        }

        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }

        // All tasks should have gotten the same workspace root inode.
        let expected_root = results[0].0;
        for (root, _) in &results {
            assert_eq!(*root, expected_root, "all tasks should see the same workspace root");
        }

        // Only one KikiFs should have been created (double-checked locking).
        let inner = fs.inner.lock();
        assert_eq!(inner.live.len(), 1, "only one KikiFs should exist despite concurrent hydration");
    }
}
