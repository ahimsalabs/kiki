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
        // Only active workspaces appear in the repo dir's children.
        if reg.state == WorkspaceState::Active {
            let global_root = to_global(reg.slot, ROOT_INODE);
            if let Some(repo_dir) = inner.synthetic_dirs.get_mut(&repo_inode) {
                repo_dir.children.insert(reg.name, global_root);
            }
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
            if ws_entry.state == WorkspaceState::Pending {
                return Err(FsError::NotFound);
            }
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

        let fs = KikiFs::new(
            Arc::clone(&info.store),
            root_tree,
            info.remote_store.clone(),
            Some(scratch_dir),
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
    fn synthetic_readdir(&self, dir: InodeId) -> Result<Vec<DirEntry>, FsError> {
        let inner = self.inner.lock();
        let synth = inner
            .synthetic_dirs
            .get(&dir)
            .ok_or(FsError::NotFound)?;
        let entries = synth
            .children
            .iter()
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
            "ssh://server/mono".into(),
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
    fn pending_workspace_hidden_from_readdir_and_lookup() {
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
        assert!(entries.is_empty(), "pending workspace should be hidden");

        // Lookup should also fail.
        let result = fs.synthetic_lookup(repo_ino, "wip");
        assert_eq!(result, Err(FsError::NotFound));
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
}
