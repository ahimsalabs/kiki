use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context as _};
use bytes::Bytes;
use proto::jj_interface::*;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::mount_meta::{self, MountMetadata};
use crate::remote::fetch::{self, FetchError};
use crate::remote::{self, BlobKind, RemoteStore};
use crate::store::Store;
use crate::ty;
use crate::ty::TreeEntry;
use crate::vfs::{FsError, JjYakFs, YakFs};
use crate::vfs_mgr::{BindError, MountAttachment, TransportInfo, VfsManagerHandle};

/// Where per-mount redb files live. Production runs are always
/// [`StorageConfig::OnDisk`]; tests can use [`StorageConfig::InMemory`]
/// to skip the filesystem entirely.
#[derive(Clone, Debug)]
pub enum StorageConfig {
    /// Each mount opens a redb file under
    /// `<root>/mounts/<hash(wc_path)>/store.redb`. The directory is
    /// created on demand.
    OnDisk { root: PathBuf },
    /// Each mount gets a fresh `redb::backends::InMemoryBackend`. Used
    /// by `JujutsuService::bare()` and any test that doesn't care about
    /// persistence across `Initialize`.
    ///
    /// `#[allow(dead_code)]` because the production daemon binary never
    /// constructs this variant (only `bare()` does, and that's
    /// `cfg(test)`); declaring it here keeps the prod and test
    /// constructors symmetrical without a `cfg` split on the enum
    /// itself.
    #[allow(dead_code)]
    InMemory,
}

impl StorageConfig {
    pub fn on_disk(root: PathBuf) -> Self {
        StorageConfig::OnDisk { root }
    }

    /// Where the redb file for `working_copy_path` should live, if any.
    /// Returns `None` for the in-memory variant.
    fn store_path_for(&self, working_copy_path: &str) -> Option<PathBuf> {
        match self {
            StorageConfig::OnDisk { root } => {
                Some(mount_meta::store_path(root, working_copy_path))
            }
            StorageConfig::InMemory => None,
        }
    }

    /// Where the `mount.toml` for `working_copy_path` should live, if
    /// any. Returns `None` for the in-memory variant.
    fn meta_path_for(&self, working_copy_path: &str) -> Option<PathBuf> {
        match self {
            StorageConfig::OnDisk { root } => {
                Some(mount_meta::meta_path(root, working_copy_path))
            }
            StorageConfig::InMemory => None,
        }
    }
}

/// Map a fallible proto-decode error onto an `invalid_argument` gRPC status.
/// Use for any conversion that came off the wire — peers that send malformed
/// requests should get a clean error, not crash the daemon. Uses the
/// alternate `{:#}` format so anyhow-style error chains surface their root
/// cause (e.g. "expected 32-byte id, got N bytes") rather than just the
/// outer context.
fn decode_status<E: std::fmt::Display>(context: &str) -> impl FnOnce(E) -> Status + '_ {
    move |e| Status::invalid_argument(format!("{context}: {e:#}"))
}

/// Map a Store-layer `anyhow::Error` (Layer B I/O failures, redb
/// commit/read errors) onto `internal`. Uses the alternate formatter so
/// the chained context survives across the wire.
fn store_status(context: &str) -> impl FnOnce(anyhow::Error) -> Status + '_ {
    move |e| Status::internal(format!("{context}: {e:#}"))
}

/// Map a remote (Layer C / M9) `anyhow::Error` onto `internal`. Same
/// shape as [`store_status`]; kept separate so the wire-side error
/// message says "remote_*: …" and is greppable in tracing output.
fn remote_status(context: &str) -> impl FnOnce(anyhow::Error) -> Status + '_ {
    move |e| Status::internal(format!("{context}: {e:#}"))
}

fn hex(id: &ty::Id) -> String {
    let mut s = String::with_capacity(id.0.len() * 2);
    for b in id.0 {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// Per-mount working-copy state.
///
/// Keyed by `working_copy_path` in `JujutsuService::mounts`. Holds everything
/// the daemon needs to answer working-copy and store RPCs for one mount.
///
/// `op_id` and `workspace_id` start empty after `Initialize`; the CLI fills
/// them in via `SetCheckoutState` once `YakWorkingCopy::init` runs (M2).
/// `root_tree_id` defaults to the store's empty tree until a real check-out
/// lands (M5).
///
/// The `store` is per-mount so two `Mount`s at different remotes can never
/// see each other's content-addressed blobs (decided at M4 per
/// `docs/PLAN.md` §7). The `attachment` keeps the FUSE/NFS mount alive for
/// as long as the `Mount` exists; dropping it tears the mount down.
struct Mount {
    /// Canonical working-copy path. Also the map key; stored here too so the
    /// `Mount` is self-describing for `DaemonStatus` listings.
    working_copy_path: String,
    /// Carried from `Initialize.remote`; surfaced via `DaemonStatus`.
    /// Also persisted to `mount.toml` so a daemon restart can rebuild
    /// [`Self::remote_store`] by re-parsing.
    remote: String,
    /// Parsed Layer-C handle (M9). `None` when `remote` was empty or
    /// the parser produced no concrete backend (e.g. blank string,
    /// pre-Initialize). When `Some`, write RPCs push every newly-stored
    /// blob through it (write-through), read RPCs fall back to it on a
    /// local miss (read-through), and `Snapshot` walks the rolled-up
    /// tree post-VFS-snapshot to push any blobs the remote doesn't have.
    /// Plan §13.2.
    remote_store: Option<Arc<dyn RemoteStore>>,
    /// Last operation id pushed by the CLI via `SetCheckoutState`.
    /// Empty until first set.
    op_id: Vec<u8>,
    /// Workspace identifier as bytes (matches proto). Empty until first set.
    workspace_id: Vec<u8>,
    /// Currently checked-out root tree. Initialized to the store's empty
    /// tree id; updated by `CheckOut` (M5) and `Snapshot` (M6).
    root_tree_id: Vec<u8>,
    /// Per-mount keyspace.
    store: Arc<Store>,
    /// Per-mount local-fallback catalog (M10.5, PLAN.md §10.5). Always
    /// present, regardless of whether `remote_store` is `Some`. The
    /// catalog RPC handlers prefer `remote_store`'s ref methods when a
    /// remote is configured (so two daemons sharing a remote serialize
    /// against each other), and fall back to `local_refs` otherwise
    /// (so the catalog API works uniformly in the single-daemon case).
    /// Backed by a redb table inside the same per-mount `store.redb`
    /// file the blob `store` already owns — sharing the `Arc<Database>`
    /// keeps everything on a single fsync per mutating txn.
    local_refs: Arc<crate::local_refs::LocalRefs>,
    /// Per-mount filesystem. The same `Arc` is handed to the VFS bind so
    /// kernel I/O hits this object; we keep a clone here so RPCs that
    /// mutate the mount (today: `CheckOut`) can drive it directly.
    fs: Arc<dyn JjYakFs>,
    /// Holds the kernel mount alive. `None` only in the test path where
    /// `JujutsuService::bare` constructed a service without a
    /// `VfsManagerHandle`. Production `Initialize` always populates it.
    #[allow(dead_code)] // dropped on Mount drop, never read directly
    attachment: Option<MountAttachment>,
    /// Where the persisted `mount.toml` lives (M8 / Layer B). `None`
    /// for `StorageConfig::InMemory`. Mutating RPCs
    /// (`SetCheckoutState`, `CheckOut`, `Snapshot`) re-write this file
    /// so a daemon restart sees current state.
    meta_path: Option<PathBuf>,
}

impl Mount {
    /// Snapshot the persistable subset of this mount's state.
    fn metadata(&self) -> MountMetadata {
        MountMetadata {
            working_copy_path: self.working_copy_path.clone(),
            remote: self.remote.clone(),
            op_id: self.op_id.clone(),
            workspace_id: self.workspace_id.clone(),
            root_tree_id: self.root_tree_id.clone(),
        }
    }

    /// Persist the current metadata to disk if the mount is backed by
    /// a real `<storage_dir>` (not the in-memory test variant). Logs
    /// failures rather than propagating them — the in-memory state is
    /// still authoritative; a transient write failure shouldn't fail
    /// the RPC. Hard failures will resurface on the next restart's
    /// rehydrate scan.
    fn persist_metadata(&self) {
        if let Some(path) = &self.meta_path {
            if let Err(e) = self.metadata().write_to(path) {
                tracing::error!(
                    path = %path.display(),
                    error = %format!("{e:#}"),
                    "failed to persist mount metadata"
                );
            }
        }
    }
}

pub struct JujutsuService {
    /// Per-mount state, keyed by `working_copy_path`. Use `tokio::Mutex`
    /// because the RPC handlers are async; contention is minimal (one
    /// per-mount entry, mostly small reads/writes).
    mounts: Arc<Mutex<HashMap<String, Mount>>>,
    /// `None` only in `bare()` test mode. Production `Initialize`
    /// requires it.
    vfs_handle: Option<VfsManagerHandle>,
    /// Where per-mount redb files live. M8 / Layer B.
    storage: StorageConfig,
}

impl JujutsuService {
    /// Production constructor: with `Some(vfs_handle)`, `Initialize`
    /// validates mountpoints and attaches a real FUSE/NFS mount.
    /// `None` is the integration-test path (see `Config.disable_mount`):
    /// per-mount state is still tracked and store/WC RPCs work, but
    /// nothing is mounted at `working_copy_path`.
    ///
    /// `storage` decides where per-mount Stores live. Production passes
    /// `StorageConfig::OnDisk { root: <storage_dir from daemon.toml> }`;
    /// tests that don't care about durability use
    /// `StorageConfig::InMemory`.
    ///
    /// Returns the bare `JujutsuService`. Call [`Self::rehydrate`] to
    /// re-attach mounts left behind by a previous daemon process, then
    /// [`Self::into_server`] to wrap it for tonic.
    pub fn new(vfs_handle: Option<VfsManagerHandle>, storage: StorageConfig) -> Self {
        JujutsuService {
            mounts: Arc::new(Mutex::new(HashMap::new())),
            vfs_handle,
            storage,
        }
    }

    /// Wrap the service in the tonic-generated server type. Consumes
    /// `self` because the server takes the service by value.
    pub fn into_server(self) -> jujutsu_interface_server::JujutsuInterfaceServer<Self> {
        jujutsu_interface_server::JujutsuInterfaceServer::new(self)
    }

    /// Re-attach every mount described by a `mount.toml` under
    /// `<storage_dir>/mounts/`. Run once at startup, before the gRPC
    /// listener accepts connections — otherwise an early `Initialize`
    /// could race with the rehydrate scan.
    ///
    /// Failures for a single mount are logged and skipped; a corrupt
    /// mount.toml or missing redb shouldn't take the daemon down. The
    /// operator can prune the broken subdir manually.
    pub async fn rehydrate(&self) -> anyhow::Result<()> {
        let root = match &self.storage {
            StorageConfig::OnDisk { root } => root.clone(),
            StorageConfig::InMemory => return Ok(()),
        };
        let entries = mount_meta::list_persisted(&root)?;
        if entries.is_empty() {
            return Ok(());
        }
        info!(count = entries.len(), "rehydrating mounts from {}", root.display());
        for (mount_dir, meta) in entries {
            if let Err(e) = self.rehydrate_one(&meta).await {
                tracing::error!(
                    path = %meta.working_copy_path,
                    dir = %mount_dir.display(),
                    error = %format!("{e:#}"),
                    "failed to rehydrate mount; skipping"
                );
            }
        }
        Ok(())
    }

    async fn rehydrate_one(&self, meta: &MountMetadata) -> anyhow::Result<()> {
        let store = Arc::new(open_store_for(&self.storage, &meta.working_copy_path)?);
        // M10.5: always materialize a local-fallback catalog handle.
        // It shares the per-mount redb Database, so no extra file or
        // fsync; on this rehydrate path the table was created on the
        // previous daemon's first cas_ref (or is implicitly empty).
        let local_refs = Arc::new(crate::local_refs::LocalRefs::new(store.database()));
        let root_tree_id: ty::Id = if meta.root_tree_id.is_empty() {
            store.get_empty_tree_id()
        } else {
            meta.root_tree_id
                .clone()
                .try_into()
                .context("decoding stored root_tree_id")?
        };
        // Re-parse the remote URL on rehydrate. A bad URL on disk is a
        // bug — but fail soft (anyhow::Error here is logged + the mount
        // is skipped by `rehydrate`) rather than panicking, so the
        // operator can prune the broken mount dir.
        let remote_store = remote::parse(&meta.remote)
            .with_context(|| format!("parsing remote URL {:?}", meta.remote))?;
        // M10 §10.6: hand the remote into `YakFs` too, so FUSE-side
        // reads on a `StoreMiss` fall through to the remote the same
        // way M9's RPC-layer reads already do.
        let fs: Arc<dyn JjYakFs> =
            Arc::new(YakFs::new(store.clone(), root_tree_id, remote_store.clone()));

        // The previous daemon's FUSE/NFS mount went away when its
        // process exited (kernel drops the mount). On restart the path
        // is no longer a mountpoint, so `validate_mountpoint` should
        // accept it as long as no one repopulated the dir in between.
        let attachment = if let Some(vfs) = &self.vfs_handle {
            validate_mountpoint(&meta.working_copy_path).map_err(|e| {
                anyhow::anyhow!("mountpoint no longer valid for rehydrate: {e}")
            })?;
            let (_transport, attachment) = vfs
                .bind(meta.working_copy_path.clone(), fs.clone())
                .await
                .map_err(|e| anyhow::anyhow!("vfs bind failed: {e}"))?;
            Some(attachment)
        } else {
            None
        };

        let meta_path = self.storage.meta_path_for(&meta.working_copy_path);
        let mut mounts = self.mounts.lock().await;
        mounts.insert(
            meta.working_copy_path.clone(),
            Mount {
                working_copy_path: meta.working_copy_path.clone(),
                remote: meta.remote.clone(),
                remote_store,
                op_id: meta.op_id.clone(),
                workspace_id: meta.workspace_id.clone(),
                root_tree_id: root_tree_id.0.to_vec(),
                store,
                local_refs,
                fs,
                attachment,
                meta_path,
            },
        );
        info!(path = %meta.working_copy_path, "rehydrated mount");
        Ok(())
    }

    /// Bare service without the gRPC server wrapping or any VFS attach.
    /// Used by tests that drive the trait methods directly: `Initialize`
    /// skips both mountpoint validation and VFS bind, so callers can use
    /// arbitrary string paths (`/tmp/repo`, `/never/initialized`) without
    /// creating real directories or spinning up a real VFS manager.
    /// Always uses in-memory storage so tests don't litter the disk.
    #[cfg(test)]
    fn bare() -> Self {
        JujutsuService {
            mounts: Arc::new(Mutex::new(HashMap::new())),
            vfs_handle: None,
            storage: StorageConfig::InMemory,
        }
    }

    /// Test-only access to a mount's per-mount `JjYakFs`. Used to drive
    /// VFS write ops directly from service-level tests without going
    /// through a real FUSE/NFS adapter — e.g. to seed a dirty file, then
    /// confirm the `Snapshot` RPC turns it into a real tree id.
    #[cfg(test)]
    async fn fs_for_test(&self, path: &str) -> Option<Arc<dyn JjYakFs>> {
        let mounts = self.mounts.lock().await;
        mounts.get(path).map(|m| m.fs.clone())
    }
}

/// Mountpoint validation errors. Mapped onto `FailedPrecondition` so the
/// CLI surfaces a clean user-facing message rather than a generic
/// internal error.
#[derive(thiserror::Error, Debug)]
enum MountpointError {
    #[error("path does not exist: {0}")]
    Missing(String),
    #[error("path is not a directory: {0}")]
    NotADirectory(String),
    #[error("directory is not empty: {0}")]
    NotEmpty(String),
    #[error("path is already a mountpoint: {0}")]
    AlreadyMounted(String),
    #[error("failed to inspect {0}: {1}")]
    Io(String, std::io::Error),
}

/// Reject anything that would either lose user data or shadow an existing
/// mount. Conservative on purpose — we never auto-unmount stale mounts
/// (would mask daemon-restart bugs that Layer B is supposed to fix).
fn validate_mountpoint(path: &str) -> Result<(), MountpointError> {
    let p = Path::new(path);
    let meta = std::fs::metadata(p)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => MountpointError::Missing(path.to_owned()),
            _ => MountpointError::Io(path.to_owned(), e),
        })?;
    if !meta.is_dir() {
        return Err(MountpointError::NotADirectory(path.to_owned()));
    }
    let mut entries = std::fs::read_dir(p).map_err(|e| MountpointError::Io(path.to_owned(), e))?;
    if entries.next().is_some() {
        return Err(MountpointError::NotEmpty(path.to_owned()));
    }
    if is_mountpoint(p).map_err(|e| MountpointError::Io(path.to_owned(), e))? {
        return Err(MountpointError::AlreadyMounted(path.to_owned()));
    }
    Ok(())
}

/// Mountpoint detection without parsing /proc/mounts or getmntinfo:
/// compare the device id of the path with that of its parent. A different
/// device id means the path is the root of some other filesystem — i.e.
/// already mounted.
///
/// Edge case: if `path` is a filesystem root (no parent), treat it as
/// already mounted. We don't expect anyone to `jj yak init /` but the
/// conservative answer keeps the validator from accepting it.
fn is_mountpoint(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let path_meta = std::fs::metadata(path)?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => return Ok(true),
    };
    let parent_meta = std::fs::metadata(parent)?;
    Ok(path_meta.dev() != parent_meta.dev())
}

fn mountpoint_status(e: MountpointError) -> Status {
    Status::failed_precondition(e.to_string())
}

fn bind_status(e: BindError) -> Status {
    Status::internal(format!("VFS bind failed: {e}"))
}

/// Convert an internal `TransportInfo` into the proto oneof.
fn transport_to_proto(info: TransportInfo) -> initialize_reply::Transport {
    match info {
        TransportInfo::Fuse => initialize_reply::Transport::Fuse(FuseTransport {}),
        TransportInfo::Nfs { port } => {
            initialize_reply::Transport::Nfs(NfsTransport {
                port: port as u32,
            })
        }
    }
}

/// Open a per-mount [`Store`] according to the daemon's `StorageConfig`.
/// On disk: redb file at `<root>/mounts/<hash(wc_path)>/store.redb`,
/// reused if it already exists (Layer B durability — second `Initialize`
/// rehydrates instead of starting empty). In-memory: fresh
/// `InMemoryBackend` per call.
fn open_store_for(storage: &StorageConfig, working_copy_path: &str) -> anyhow::Result<Store> {
    match storage.store_path_for(working_copy_path) {
        Some(path) => Store::open(&path),
        None => Ok(Store::new_in_memory()),
    }
}

/// Look up a mount by `working_copy_path` and clone its store handle so
/// the lock can be released before doing real work. All store RPCs use
/// this — the lock is short, the RPC body is long.
async fn store_for(
    mounts: &Arc<Mutex<HashMap<String, Mount>>>,
    path: &str,
) -> Result<Arc<Store>, Status> {
    let (store, _remote) = mount_handles(mounts, path).await?;
    Ok(store)
}

/// Variant of [`store_for`] that also clones the per-mount
/// `RemoteStore` (M9). Returned as `(local, remote)` so handlers can
/// destructure cleanly. `remote` is `None` when no remote is
/// configured for the mount; handlers fall back to the existing
/// `not_found` path in that case.
async fn mount_handles(
    mounts: &Arc<Mutex<HashMap<String, Mount>>>,
    path: &str,
) -> Result<(Arc<Store>, Option<Arc<dyn RemoteStore>>), Status> {
    let guard = mounts.lock().await;
    guard
        .get(path)
        .map(|m| (m.store.clone(), m.remote_store.clone()))
        .ok_or_else(|| Status::not_found(format!("no mount at {path}")))
}

/// Per-mount catalog handle (M10.5). When the mount has a remote
/// configured, catalog ops route through that remote's ref methods —
/// so two daemons sharing one remote serialize against each other.
/// Otherwise they route through the per-mount [`LocalRefs`] (single
/// daemon, no cross-process arbitration needed).
///
/// The two arms are exposed as a small enum rather than a `dyn
/// Catalog` trait object: `LocalRefs`'s methods are sync, the
/// `RemoteStore` ref methods are async, and adding a unifying trait
/// just to bridge that asymmetry would add code without trimming any.
#[derive(Clone)]
enum CatalogHandle {
    Remote(Arc<dyn RemoteStore>),
    Local(Arc<crate::local_refs::LocalRefs>),
}

impl CatalogHandle {
    async fn get_ref(&self, name: &str) -> anyhow::Result<Option<Bytes>> {
        match self {
            CatalogHandle::Remote(r) => r.get_ref(name).await,
            CatalogHandle::Local(l) => l.get_ref(name),
        }
    }

    async fn cas_ref(
        &self,
        name: &str,
        expected: Option<&Bytes>,
        new: Option<&Bytes>,
    ) -> anyhow::Result<remote::CasOutcome> {
        match self {
            CatalogHandle::Remote(r) => r.cas_ref(name, expected, new).await,
            CatalogHandle::Local(l) => l.cas_ref(name, expected, new),
        }
    }

    async fn list_refs(&self) -> anyhow::Result<Vec<String>> {
        match self {
            CatalogHandle::Remote(r) => r.list_refs().await,
            CatalogHandle::Local(l) => l.list_refs(),
        }
    }
}

/// Resolve the catalog handle for a mount: prefer the configured
/// `RemoteStore` if any, otherwise the per-mount `LocalRefs`. Errors
/// with `not_found` when the mount itself doesn't exist (same shape
/// as [`mount_handles`]).
#[allow(clippy::result_large_err)]
async fn catalog_for(
    mounts: &Arc<Mutex<HashMap<String, Mount>>>,
    path: &str,
) -> Result<CatalogHandle, Status> {
    let guard = mounts.lock().await;
    let m = guard
        .get(path)
        .ok_or_else(|| Status::not_found(format!("no mount at {path}")))?;
    Ok(match &m.remote_store {
        Some(r) => CatalogHandle::Remote(r.clone()),
        None => CatalogHandle::Local(m.local_refs.clone()),
    })
}

// ---- Layer C / M9 read-through helpers --------------------------------
//
// On local-store miss with a configured remote: delegate to
// `crate::remote::fetch`, which fetches the bytes, decodes, verifies
// round-trip (defending against a corrupt peer), and persists locally
// so subsequent reads hit the cache. On miss with no remote, return
// `not_found` — preserves pre-M9 behavior.
//
// M10 §10.6 factored the fetch+verify+persist dance out of this module
// so `vfs/yak_fs.rs` can share the implementation. The wrapper here
// translates the typed `FetchError` onto gRPC `Status` codes — keeping
// `data_loss` distinct from generic `internal` is a real wire-side
// signal (corrupt peer, not a transient I/O hiccup).
//
// Ok=() vs Err=Status is intentionally imbalanced — Status is large
// (~176 bytes); the call sites all `?`-propagate without boxing.
#[allow(clippy::result_large_err)]
fn fetch_status(err: FetchError) -> Status {
    match err {
        FetchError::NotFound { kind, id } => Status::not_found(format!(
            "{kind} {id} not found locally or on remote"
        )),
        FetchError::DataLoss { .. } => Status::data_loss(format!("{err:#}")),
        FetchError::Decode { .. } | FetchError::DecodeValue { .. } => {
            Status::invalid_argument(format!("{err:#}"))
        }
        FetchError::LocalWrite { .. } | FetchError::Remote { .. } => {
            Status::internal(format!("{err:#}"))
        }
    }
}

#[allow(clippy::result_large_err)]
async fn fetch_file_through_remote(
    store: &Store,
    remote: Option<&dyn RemoteStore>,
    id: ty::Id,
) -> Result<ty::File, Status> {
    let remote = remote
        .ok_or_else(|| Status::not_found(format!("file {id} not found")))?;
    fetch::fetch_file(store, remote, id).await.map_err(fetch_status)
}

#[allow(clippy::result_large_err)]
async fn fetch_symlink_through_remote(
    store: &Store,
    remote: Option<&dyn RemoteStore>,
    id: ty::Id,
) -> Result<ty::Symlink, Status> {
    let remote = remote
        .ok_or_else(|| Status::not_found(format!("symlink {id} not found")))?;
    fetch::fetch_symlink(store, remote, id)
        .await
        .map_err(fetch_status)
}

#[allow(clippy::result_large_err)]
async fn fetch_tree_through_remote(
    store: &Store,
    remote: Option<&dyn RemoteStore>,
    id: ty::Id,
) -> Result<ty::Tree, Status> {
    let remote = remote
        .ok_or_else(|| Status::not_found(format!("tree {id} not found")))?;
    fetch::fetch_tree(store, remote, id).await.map_err(fetch_status)
}

#[allow(clippy::result_large_err)]
async fn fetch_commit_through_remote(
    store: &Store,
    remote: Option<&dyn RemoteStore>,
    id: ty::Id,
) -> Result<ty::Commit, Status> {
    let remote = remote
        .ok_or_else(|| Status::not_found(format!("commit {id} not found")))?;
    fetch::fetch_commit(store, remote, id)
        .await
        .map_err(fetch_status)
}

// ---- Layer C / M9 post-snapshot push ---------------------------------
//
// After `JjYakFs::snapshot` produces the new root, walk every reachable
// blob and push the ones the remote doesn't already have. Idempotent
// across snapshots: `has_blob` short-circuits the second-and-later visit
// for unchanged subtrees.
//
// Iterative (not recursive) so we don't need `Box::pin` /
// `async-recursion` to satisfy the borrow checker on async recursion.
// Dedupe by tree id within a single walk to avoid re-checking shared
// subtrees in the same snapshot.

/// Walk reachable blobs from `root_tree_id` in `store` and push any the
/// `remote` is missing. Tree ids are deduped per walk; file/symlink ids
/// rely on `has_blob` being cheap (the `dir://` impl is a `metadata()`
/// probe; the gRPC server is a single map lookup).
async fn push_reachable_blobs(
    store: &Store,
    remote: &dyn RemoteStore,
    root_tree_id: ty::Id,
) -> anyhow::Result<()> {
    let mut seen_trees: HashSet<ty::Id> = HashSet::new();
    let mut tree_stack: Vec<ty::Id> = vec![root_tree_id];

    while let Some(tree_id) = tree_stack.pop() {
        if !seen_trees.insert(tree_id) {
            continue;
        }
        // Push the tree blob itself.
        push_blob_if_missing(store, remote, BlobKind::Tree, tree_id).await?;

        let tree = store
            .get_tree(tree_id)?
            .ok_or_else(|| anyhow!("tree {} missing locally during push walk", hex(&tree_id)))?;
        for entry in tree.entries {
            match entry.entry {
                TreeEntry::TreeId(id) => tree_stack.push(id),
                TreeEntry::File { id, .. } => {
                    push_blob_if_missing(store, remote, BlobKind::File, id).await?;
                }
                TreeEntry::SymlinkId(id) => {
                    push_blob_if_missing(store, remote, BlobKind::Symlink, id).await?;
                }
                TreeEntry::ConflictId(_) => {
                    // Not reachable from a `JjYakFs`-produced snapshot
                    // tree (the FS only emits TreeId/File/SymlinkId).
                    // Conflict objects, when they appear, ride on the
                    // commit-side write-through path instead. Skip.
                }
            }
        }
    }
    Ok(())
}

/// Cheap-existence-probe-then-push for one blob. Reuses the same
/// `Bytes` buffer that lives in redb (via `Store::get_*_bytes`) so the
/// post-snapshot walk doesn't re-encode anything.
async fn push_blob_if_missing(
    store: &Store,
    remote: &dyn RemoteStore,
    kind: BlobKind,
    id: ty::Id,
) -> anyhow::Result<()> {
    if remote.has_blob(kind, &id).await? {
        return Ok(());
    }
    let bytes: Bytes = match kind {
        BlobKind::Tree => store.get_tree_bytes(id)?,
        BlobKind::File => store.get_file_bytes(id)?,
        BlobKind::Symlink => store.get_symlink_bytes(id)?,
        BlobKind::Commit => store.get_commit_bytes(id)?,
    }
    .ok_or_else(|| {
        anyhow!(
            "local store missing {} {} during push walk",
            kind.as_str(),
            hex(&id)
        )
    })?;
    remote.put_blob(kind, &id, bytes).await?;
    Ok(())
}

#[tonic::async_trait]
impl jujutsu_interface_server::JujutsuInterface for JujutsuService {
    #[tracing::instrument(skip(self))]
    async fn initialize(
        &self,
        request: Request<InitializeReq>,
    ) -> Result<Response<InitializeReply>, Status> {
        let req = request.into_inner();
        info!(
            "Initializing a new repo at {} for {}",
            &req.path, &req.remote
        );

        // Cheap pre-check: surface the duplicate before doing any I/O.
        // Re-checked after validation/bind under the same lock to close
        // the obvious race window.
        {
            let mounts = self.mounts.lock().await;
            if mounts.contains_key(&req.path) {
                return Err(Status::already_exists(format!(
                    "mount already initialized at {}",
                    req.path
                )));
            }
        }

        let store = Arc::new(open_store_for(&self.storage, &req.path).map_err(|e| {
            Status::internal(format!("opening store for {}: {e:#}", req.path))
        })?);
        // M10.5: always materialize a local-fallback catalog. See the
        // note in `rehydrate_one`; same shape here.
        let local_refs = Arc::new(crate::local_refs::LocalRefs::new(store.database()));
        let root_tree_id = store.get_empty_tree_id();

        // M9: parse `remote` into an optional `RemoteStore`. Bad URL =
        // `invalid_argument` so the user gets a clean message rather
        // than a generic internal error or — worse — a silent drop into
        // "no remote configured". Empty string is still `Ok(None)`.
        let remote_store = remote::parse(&req.remote)
            .map_err(|e| Status::invalid_argument(format!("remote: {e:#}")))?;

        // Build the per-mount FS up front so we can hand the same `Arc`
        // to both the VFS bind (for kernel I/O) and the `Mount` (for
        // RPCs like `CheckOut` that mutate mount-side state). Even on
        // the disable_mount test path the `Mount` keeps an `fs` so
        // mutating RPCs work end-to-end without the kernel.
        //
        // M10 §10.6: the remote is threaded into `YakFs` too — kernel
        // reads on local-store miss fall through the same way the M9
        // RPC layer already does at `service.rs`.
        let fs: Arc<dyn JjYakFs> =
            Arc::new(YakFs::new(store.clone(), root_tree_id, remote_store.clone()));

        // Production path validates and binds; test path skips both so
        // unit tests can use arbitrary string paths.
        let (transport, attachment) = if let Some(vfs) = &self.vfs_handle {
            validate_mountpoint(&req.path).map_err(mountpoint_status)?;
            let (transport, attachment) = vfs
                .bind(req.path.clone(), fs.clone())
                .await
                .map_err(bind_status)?;
            (Some(transport), Some(attachment))
        } else {
            (None, None)
        };

        let mut mounts = self.mounts.lock().await;
        if mounts.contains_key(&req.path) {
            // Race: someone else inserted while we were validating/binding.
            // The mount we just attached will be torn down when
            // `attachment` drops at the end of this scope.
            warn!(path = %req.path, "race during Initialize — discarding our bind");
            return Err(Status::already_exists(format!(
                "mount already initialized at {}",
                req.path
            )));
        }
        let meta_path = self.storage.meta_path_for(&req.path);
        let mount = Mount {
            working_copy_path: req.path.clone(),
            remote: req.remote,
            remote_store,
            op_id: Vec::new(),
            workspace_id: Vec::new(),
            root_tree_id: root_tree_id.0.to_vec(),
            store,
            local_refs,
            fs,
            attachment,
            meta_path,
        };
        // Persist initial metadata so `rehydrate` can re-attach the
        // mount across daemon restarts even before the CLI's first
        // `SetCheckoutState`. Failure here is fatal — the mount has
        // not been registered yet and we'd rather report the error
        // than have a half-persisted state.
        if let Some(path) = &mount.meta_path {
            mount.metadata().write_to(path).map_err(|e| {
                Status::internal(format!(
                    "writing initial mount.toml for {}: {e:#}",
                    req.path
                ))
            })?;
        }
        mounts.insert(req.path.clone(), mount);

        let reply = InitializeReply {
            transport: transport.map(transport_to_proto),
        };
        Ok(Response::new(reply))
    }

    #[tracing::instrument(skip(self))]
    async fn daemon_status(
        &self,
        _request: Request<DaemonStatusReq>,
    ) -> Result<Response<DaemonStatusReply>, Status> {
        let mounts = self.mounts.lock().await;
        // Sort by path so output is deterministic — `yak status` is
        // user-facing and `HashMap` iteration order is not.
        let mut data: Vec<_> = mounts
            .values()
            .map(|m| proto::jj_interface::daemon_status_reply::Data {
                path: m.working_copy_path.clone(),
                remote: m.remote.clone(),
            })
            .collect();
        data.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Response::new(DaemonStatusReply { data }))
    }

    // ---- M10.5: per-mount catalog (mutable refs) -------------------
    //
    // CLI-facing wrappers around the per-mount catalog handle. The
    // dispatch (remote vs local) lives in `catalog_for`; these
    // handlers just thread the bytes across the wire and re-validate
    // ref names defensively (same pattern as the M10
    // `RemoteStoreService` handlers).

    #[tracing::instrument(skip(self))]
    async fn get_catalog_ref(
        &self,
        request: Request<GetCatalogRefReq>,
    ) -> Result<Response<GetCatalogRefReply>, Status> {
        let req = request.into_inner();
        remote::validate_ref_name(&req.name)
            .map_err(|e| Status::invalid_argument(format!("ref name: {e:#}")))?;
        let catalog = catalog_for(&self.mounts, &req.working_copy_path).await?;
        let value = catalog
            .get_ref(&req.name)
            .await
            .map_err(remote_status("catalog get_ref"))?;
        Ok(Response::new(match value {
            Some(b) => GetCatalogRefReply {
                found: true,
                value: b.to_vec(),
            },
            None => GetCatalogRefReply {
                found: false,
                value: Vec::new(),
            },
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn cas_catalog_ref(
        &self,
        request: Request<CasCatalogRefReq>,
    ) -> Result<Response<CasCatalogRefReply>, Status> {
        let req = request.into_inner();
        remote::validate_ref_name(&req.name)
            .map_err(|e| Status::invalid_argument(format!("ref name: {e:#}")))?;
        let catalog = catalog_for(&self.mounts, &req.working_copy_path).await?;
        // proto3 `optional bytes` decodes to `Option<Vec<u8>>`; lift
        // to `Option<Bytes>` so the trait sees the same
        // absent-vs-empty distinction as the wire.
        let expected = req.expected.map(Bytes::from);
        let new = req.new.map(Bytes::from);
        let outcome = catalog
            .cas_ref(&req.name, expected.as_ref(), new.as_ref())
            .await
            .map_err(remote_status("catalog cas_ref"))?;
        Ok(Response::new(match outcome {
            remote::CasOutcome::Updated => CasCatalogRefReply {
                updated: true,
                actual: None,
            },
            remote::CasOutcome::Conflict { actual } => CasCatalogRefReply {
                updated: false,
                actual: actual.map(|b| b.to_vec()),
            },
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn list_catalog_refs(
        &self,
        request: Request<ListCatalogRefsReq>,
    ) -> Result<Response<ListCatalogRefsReply>, Status> {
        let req = request.into_inner();
        let catalog = catalog_for(&self.mounts, &req.working_copy_path).await?;
        let names = catalog
            .list_refs()
            .await
            .map_err(remote_status("catalog list_refs"))?;
        Ok(Response::new(ListCatalogRefsReply { names }))
    }

    #[tracing::instrument(skip(self))]
    async fn get_empty_tree_id(
        &self,
        request: Request<GetEmptyTreeIdReq>,
    ) -> Result<Response<TreeId>, Status> {
        let req = request.into_inner();
        let store = store_for(&self.mounts, &req.working_copy_path).await?;
        Ok(Response::new(TreeId {
            tree_id: store.get_empty_tree_id().into(),
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn write_file(
        &self,
        request: Request<WriteFileReq>,
    ) -> Result<Response<FileId>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let (id, bytes) = store
            .write_file(ty::File { content: req.data })
            .map_err(store_status("write_file"))?;
        // Write-through: push to the remote synchronously (M9 §13.4).
        // On failure, the local write has already happened — surface
        // the error but don't roll back; idempotent puts + the next
        // snapshot's walk cover transient remote failures.
        if let Some(remote) = remote {
            remote
                .put_blob(BlobKind::File, &id, bytes)
                .await
                .map_err(remote_status("remote put_blob (file)"))?;
        }
        let file_id = id.into();
        Ok(Response::new(FileId { file_id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_file(
        &self,
        request: Request<ReadFileReq>,
    ) -> Result<Response<File>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let file_id: ty::Id = FileId { file_id: req.file_id }
            .try_into()
            .map_err(decode_status("file id"))?;
        let file = match store.get_file(file_id).map_err(store_status("get_file"))? {
            Some(f) => f,
            None => fetch_file_through_remote(&store, remote.as_deref(), file_id).await?,
        };
        Ok(Response::new(file.as_proto()))
    }

    #[tracing::instrument(skip(self))]
    async fn write_symlink(
        &self,
        request: Request<WriteSymlinkReq>,
    ) -> Result<Response<SymlinkId>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let symlink = ty::Symlink { target: req.target };
        let (id, bytes) = store
            .write_symlink(symlink)
            .map_err(store_status("write_symlink"))?;
        if let Some(remote) = remote {
            remote
                .put_blob(BlobKind::Symlink, &id, bytes)
                .await
                .map_err(remote_status("remote put_blob (symlink)"))?;
        }
        let symlink_id = id.into();
        Ok(Response::new(SymlinkId { symlink_id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_symlink(
        &self,
        request: Request<ReadSymlinkReq>,
    ) -> Result<Response<Symlink>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let symlink_id: ty::Id = SymlinkId { symlink_id: req.symlink_id }
            .try_into()
            .map_err(decode_status("symlink id"))?;
        let symlink = match store
            .get_symlink(symlink_id)
            .map_err(store_status("get_symlink"))?
        {
            Some(s) => s,
            None => fetch_symlink_through_remote(&store, remote.as_deref(), symlink_id).await?,
        };
        Ok(Response::new(symlink.as_proto()))
    }

    #[tracing::instrument(skip(self))]
    async fn write_tree(
        &self,
        request: Request<WriteTreeReq>,
    ) -> Result<Response<TreeId>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let tree_proto = req
            .tree
            .ok_or_else(|| Status::invalid_argument("WriteTreeReq.tree is required"))?;
        let tree: ty::Tree = tree_proto.try_into().map_err(decode_status("tree"))?;
        let (id, bytes) = store
            .write_tree(tree)
            .map_err(store_status("write_tree"))?;
        if let Some(remote) = remote {
            remote
                .put_blob(BlobKind::Tree, &id, bytes)
                .await
                .map_err(remote_status("remote put_blob (tree)"))?;
        }
        let tree_id = id.into();
        Ok(Response::new(TreeId { tree_id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_tree(
        &self,
        request: Request<ReadTreeReq>,
    ) -> Result<Response<Tree>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let tree_id: ty::Id = TreeId { tree_id: req.tree_id }
            .try_into()
            .map_err(decode_status("tree id"))?;
        let tree = match store.get_tree(tree_id).map_err(store_status("get_tree"))? {
            Some(t) => t,
            None => fetch_tree_through_remote(&store, remote.as_deref(), tree_id).await?,
        };
        Ok(Response::new(tree.as_proto()))
    }

    #[tracing::instrument(skip(self))]
    async fn write_commit(
        &self,
        request: Request<WriteCommitReq>,
    ) -> Result<Response<CommitId>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let commit_proto = req
            .commit
            .ok_or_else(|| Status::invalid_argument("WriteCommitReq.commit is required"))?;
        if commit_proto.parents.is_empty() {
            return Err(Status::internal("Cannot write a commit with no parents"));
        }
        let commit: ty::Commit = commit_proto.try_into().map_err(decode_status("commit"))?;
        let (id, bytes) = store
            .write_commit(commit)
            .map_err(store_status("write_commit"))?;
        if let Some(remote) = remote {
            remote
                .put_blob(BlobKind::Commit, &id, bytes)
                .await
                .map_err(remote_status("remote put_blob (commit)"))?;
        }
        let commit_id = id.into();
        Ok(Response::new(CommitId { commit_id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_commit(
        &self,
        request: Request<ReadCommitReq>,
    ) -> Result<Response<Commit>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let commit_id: ty::Id = CommitId { commit_id: req.commit_id }
            .try_into()
            .map_err(decode_status("commit id"))?;
        let commit = match store
            .get_commit(commit_id)
            .map_err(store_status("get_commit"))?
        {
            Some(c) => c,
            None => fetch_commit_through_remote(&store, remote.as_deref(), commit_id).await?,
        };
        Ok(Response::new(commit.as_proto()))
    }

    #[tracing::instrument(skip(self))]
    async fn get_tree_state(
        &self,
        request: Request<GetTreeStateReq>,
    ) -> Result<Response<GetTreeStateReply>, Status> {
        let req = request.into_inner();
        let mounts = self.mounts.lock().await;
        let mount = mounts.get(&req.working_copy_path).ok_or_else(|| {
            Status::not_found(format!("no mount at {}", req.working_copy_path))
        })?;
        Ok(Response::new(GetTreeStateReply {
            tree_id: mount.root_tree_id.clone(),
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn get_checkout_state(
        &self,
        request: Request<GetCheckoutStateReq>,
    ) -> Result<Response<CheckoutState>, Status> {
        let req = request.into_inner();
        let mounts = self.mounts.lock().await;
        let mount = mounts.get(&req.working_copy_path).ok_or_else(|| {
            Status::not_found(format!("no mount at {}", req.working_copy_path))
        })?;
        // `op_id`/`workspace_id` start empty after `Initialize`. Surfacing
        // that as `failed_precondition` keeps the contract crisp: the CLI
        // must call `SetCheckoutState` first (which it does inside
        // `YakWorkingCopy::init`).
        if mount.op_id.is_empty() && mount.workspace_id.is_empty() {
            return Err(Status::failed_precondition(format!(
                "checkout state not yet set for {}",
                req.working_copy_path
            )));
        }
        Ok(Response::new(CheckoutState {
            op_id: mount.op_id.clone(),
            workspace_id: mount.workspace_id.clone(),
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn set_checkout_state(
        &self,
        request: Request<SetCheckoutStateReq>,
    ) -> Result<Response<SetCheckoutStateReply>, Status> {
        let req = request.into_inner();
        info!(path = %req.working_copy_path, "SetCheckoutState");
        let checkout = req.checkout_state.ok_or_else(|| {
            Status::invalid_argument("SetCheckoutStateReq.checkout_state is required")
        })?;
        let mut mounts = self.mounts.lock().await;
        let mount = mounts.get_mut(&req.working_copy_path).ok_or_else(|| {
            // Unlike `get_*` which only read, this RPC mutates state. We
            // refuse to lazily create a Mount because that would mask CLI
            // bugs (forgotten `Initialize`).
            Status::not_found(format!(
                "no mount at {} (call Initialize first)",
                req.working_copy_path
            ))
        })?;
        mount.op_id = checkout.op_id;
        mount.workspace_id = checkout.workspace_id;
        mount.persist_metadata();
        Ok(Response::new(SetCheckoutStateReply {}))
    }

    #[tracing::instrument(skip(self))]
    async fn snapshot(
        &self,
        request: Request<SnapshotReq>,
    ) -> Result<Response<SnapshotReply>, Status> {
        let req = request.into_inner();

        // Clone fs / store / remote handles out from under the lock so
        // the (potentially I/O-heavy) snapshot walk + post-walk remote
        // push don't hold it. Same pattern as `check_out`.
        let (fs, store, remote) = {
            let mounts = self.mounts.lock().await;
            let mount = mounts.get(&req.working_copy_path).ok_or_else(|| {
                Status::not_found(format!("no mount at {}", req.working_copy_path))
            })?;
            (mount.fs.clone(), mount.store.clone(), mount.remote_store.clone())
        };

        let new_root = fs.snapshot().await.map_err(|e| {
            Status::internal(format!(
                "snapshot failed for {}: {e}",
                req.working_copy_path
            ))
        })?;
        info!(path = %req.working_copy_path, tree_id = %hex(&new_root), "Snapshot");

        // M9 §13.2: blobs written through `JjYakFs::snapshot_node`
        // bypass the RPC handlers, so we walk the rolled-up tree and
        // push every reachable blob the remote doesn't already have.
        // Synchronous push: `Snapshot` blocks until durable. The walk
        // is cheap on a clean (no-change) snapshot — `has_blob` returns
        // true for every reachable blob and we put_blob nothing.
        if let Some(remote) = &remote {
            push_reachable_blobs(&store, remote.as_ref(), new_root)
                .await
                .map_err(remote_status("post-snapshot remote push"))?;
        }

        // Stamp the new root tree id back on the Mount so subsequent
        // `GetTreeState`/`Snapshot` reads agree with what the VFS just
        // produced. Done *after* the snapshot + remote push succeeds —
        // we'd rather report failure than declare a snapshot durable
        // that didn't fully push to the remote.
        let mut mounts = self.mounts.lock().await;
        if let Some(mount) = mounts.get_mut(&req.working_copy_path) {
            mount.root_tree_id = new_root.0.to_vec();
            mount.persist_metadata();
        } else {
            // Mount disappeared between the two locks; surface as
            // not_found rather than internal-error so a transient race
            // (no explicit Unmount today, but future-proof) is debuggable.
            return Err(Status::not_found(format!(
                "mount at {} disappeared during snapshot",
                req.working_copy_path
            )));
        }

        Ok(Response::new(SnapshotReply {
            tree_id: new_root.0.to_vec(),
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn check_out(
        &self,
        request: Request<CheckOutReq>,
    ) -> Result<Response<CheckOutReply>, Status> {
        let req = request.into_inner();
        let new_tree_id: ty::Id = TreeId {
            tree_id: req.new_tree_id.clone(),
        }
        .try_into()
        .map_err(decode_status("tree id"))?;
        info!(path = %req.working_copy_path, tree_id = %hex(&new_tree_id), "CheckOut");

        // Clone the per-mount fs handle out from under the lock so the
        // (potentially I/O-heavy) `JjYakFs::check_out` call doesn't hold
        // it. Mirrors how `store_for` works.
        let fs = {
            let mounts = self.mounts.lock().await;
            let mount = mounts.get(&req.working_copy_path).ok_or_else(|| {
                Status::not_found(format!("no mount at {}", req.working_copy_path))
            })?;
            mount.fs.clone()
        };

        // The VFS swap validates the tree exists in the store; map the
        // miss onto failed_precondition so the caller gets a crisp
        // "write the tree first" signal rather than internal-error noise.
        fs.check_out(new_tree_id).await.map_err(|e| match e {
            FsError::StoreMiss => Status::failed_precondition(format!(
                "tree {} not in store; call WriteTree first",
                hex(&new_tree_id)
            )),
            other => Status::internal(format!("check_out failed: {other}")),
        })?;

        // Stamp the new root tree id on the Mount under the lock. We do
        // this *after* the swap succeeds: if the swap fails the mount's
        // declared `root_tree_id` should still match what the kernel
        // sees through `fs`.
        let mut mounts = self.mounts.lock().await;
        if let Some(mount) = mounts.get_mut(&req.working_copy_path) {
            mount.root_tree_id = req.new_tree_id;
            mount.persist_metadata();
        } else {
            // Mount disappeared between the two locks. Extremely
            // unlikely (no Unmount RPC exists yet) but we should not
            // pretend success.
            return Err(Status::not_found(format!(
                "mount at {} disappeared during check_out",
                req.working_copy_path
            )));
        }

        Ok(Response::new(CheckOutReply {}))
    }
}

#[cfg(test)]
mod tests {
    const COMMIT_ID_LENGTH: usize = 32;
    const CHANGE_ID_LENGTH: usize = 16;

    use assert_matches::assert_matches;
    use proto::jj_interface::jujutsu_interface_server::JujutsuInterface;
    // Several tests decode raw blob bytes off the dir:// remote to
    // confirm write-through actually pushed the prost-encoded payload.
    // The trait import lives here (test-only) rather than at the
    // module top level — production code uses the typed
    // `Store::get_*_bytes` helpers and never touches `prost::decode`
    // directly.
    use prost::Message as _;

    use super::*;
    use crate::vfs::FileKind;

    /// Initialize a mount via the test path (no VFS bind). Returns the
    /// path used so the caller can pass it back into store/WC RPCs.
    /// `remote` defaults to empty (no Layer C / M9 backend); tests that
    /// want to exercise write-through/read-through call
    /// [`init_mount_with_remote`] instead.
    async fn init_mount(svc: &JujutsuService, path: &str) {
        init_mount_with_remote(svc, path, "").await
    }

    async fn init_mount_with_remote(svc: &JujutsuService, path: &str, remote: &str) {
        svc.initialize(Request::new(InitializeReq {
            path: path.to_owned(),
            remote: remote.to_owned(),
        }))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn write_commit_parents() {
        let svc = JujutsuService::bare();
        let path = "/tmp/repo".to_string();
        init_mount(&svc, &path).await;

        // No parents
        let mut commit = Commit {
            parents: vec![],
            ..Default::default()
        };

        assert_matches!(
            svc.write_commit(Request::new(WriteCommitReq {
                working_copy_path: path.clone(),
                commit: Some(commit.clone()),
            }))
            .await,
            Err(status) if status.message().contains("no parents")
        );

        // Only root commit as parent
        commit.parents = vec![vec![0; CHANGE_ID_LENGTH]];
        let first_id = svc
            .write_commit(Request::new(WriteCommitReq {
                working_copy_path: path.clone(),
                commit: Some(commit.clone()),
            }))
            .await
            .unwrap()
            .into_inner();
        let first_commit = svc
            .read_commit(Request::new(ReadCommitReq {
                working_copy_path: path.clone(),
                commit_id: first_id.commit_id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(first_commit, commit);

        // Only non-root commit as parent
        commit.parents = vec![first_id.commit_id.clone()];
        let second_id = svc
            .write_commit(Request::new(WriteCommitReq {
                working_copy_path: path.clone(),
                commit: Some(commit.clone()),
            }))
            .await
            .unwrap()
            .into_inner();
        let second_commit = svc
            .read_commit(Request::new(ReadCommitReq {
                working_copy_path: path.clone(),
                commit_id: second_id.commit_id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(second_commit, commit);

        // Merge commit
        commit.parents = vec![first_id.commit_id.clone(), second_id.commit_id.clone()];
        let merge_id = svc
            .write_commit(Request::new(WriteCommitReq {
                working_copy_path: path.clone(),
                commit: Some(commit.clone()),
            }))
            .await
            .unwrap()
            .into_inner();
        let merge_commit = svc
            .read_commit(Request::new(ReadCommitReq {
                working_copy_path: path.clone(),
                commit_id: merge_id.commit_id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(merge_commit, commit);

        commit.parents = vec![first_id.commit_id, vec![0; COMMIT_ID_LENGTH]];
        let root_merge_id = svc
            .write_commit(Request::new(WriteCommitReq {
                working_copy_path: path.clone(),
                commit: Some(commit.clone()),
            }))
            .await
            .unwrap()
            .into_inner();
        let root_merge_commit = svc
            .read_commit(Request::new(ReadCommitReq {
                working_copy_path: path,
                commit_id: root_merge_id.commit_id,
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(root_merge_commit, commit);
    }

    /// Walk through the lifecycle exercised by `jj yak init` followed by
    /// `YakWorkingCopy::init` (M2): Initialize → SetCheckoutState →
    /// GetCheckoutState → GetTreeState → Snapshot. Catches plumbing
    /// regressions in the per-mount state map.
    #[tokio::test]
    async fn checkout_state_round_trip() {
        let svc = JujutsuService::bare();
        let path = "/tmp/repo".to_string();
        init_mount(&svc, &path).await;

        // Before SetCheckoutState, the checkout state is unset and the RPC
        // surfaces that as failed_precondition.
        let err = svc
            .get_checkout_state(Request::new(GetCheckoutStateReq {
                working_copy_path: path.clone(),
            }))
            .await
            .expect_err("expected failed_precondition before SetCheckoutState");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);

        // GetTreeState before any check-out returns the empty tree id —
        // this is what lets `YakWorkingCopy::tree()` succeed on a fresh
        // mount.
        let empty = svc
            .get_empty_tree_id(Request::new(GetEmptyTreeIdReq {
                working_copy_path: path.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        let initial_tree = svc
            .get_tree_state(Request::new(GetTreeStateReq {
                working_copy_path: path.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(initial_tree.tree_id, empty.tree_id);

        // Push a checkout state and read it back.
        let op_id = vec![0xab; 32];
        let workspace_id = b"default".to_vec();
        svc.set_checkout_state(Request::new(SetCheckoutStateReq {
            working_copy_path: path.clone(),
            checkout_state: Some(CheckoutState {
                op_id: op_id.clone(),
                workspace_id: workspace_id.clone(),
            }),
        }))
        .await
        .unwrap();

        let state = svc
            .get_checkout_state(Request::new(GetCheckoutStateReq {
                working_copy_path: path.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(state.op_id, op_id);
        assert_eq!(state.workspace_id, workspace_id);

        // Snapshot of an unmodified mount returns the existing root
        // (empty here since nothing has been written through the VFS).
        // M6 made Snapshot drive `JjYakFs::snapshot`, but that walk is a
        // no-op on a clean mount.
        let snap = svc
            .snapshot(Request::new(SnapshotReq {
                working_copy_path: path,
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(snap.tree_id, empty.tree_id);
    }

    /// Mounts must be isolated by `working_copy_path`; mutating one must
    /// not bleed into another. Mirrors `test_repos_are_independent` at the
    /// CLI level. Per-mount Stores (M4) make this stricter: a blob written
    /// to `/tmp/a` cannot even be read back from `/tmp/b`.
    #[tokio::test]
    async fn mounts_are_isolated_by_path() {
        let svc = JujutsuService::bare();
        // Two mounts, no remotes. Pre-M9 these used arbitrary
        // remote-string labels; M9 makes the field a parseable URL so
        // empty (no remote) is the right neutral value here.
        for path in ["/tmp/a", "/tmp/b"] {
            svc.initialize(Request::new(InitializeReq {
                path: path.into(),
                remote: "".into(),
            }))
            .await
            .unwrap();
        }

        svc.set_checkout_state(Request::new(SetCheckoutStateReq {
            working_copy_path: "/tmp/a".into(),
            checkout_state: Some(CheckoutState {
                op_id: vec![1; 32],
                workspace_id: b"alpha".to_vec(),
            }),
        }))
        .await
        .unwrap();

        let a = svc
            .get_checkout_state(Request::new(GetCheckoutStateReq {
                working_copy_path: "/tmp/a".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(a.workspace_id, b"alpha");

        // /tmp/b's checkout was never set — must not see /tmp/a's value.
        let err = svc
            .get_checkout_state(Request::new(GetCheckoutStateReq {
                working_copy_path: "/tmp/b".into(),
            }))
            .await
            .expect_err("expected failed_precondition");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);

        // Per-mount Stores: write a file to /tmp/a, read it back; confirm
        // /tmp/b's store cannot see it.
        let written = svc
            .write_file(Request::new(WriteFileReq {
                working_copy_path: "/tmp/a".into(),
                data: b"hello-from-a".to_vec(),
            }))
            .await
            .unwrap()
            .into_inner();
        let from_a = svc
            .read_file(Request::new(ReadFileReq {
                working_copy_path: "/tmp/a".into(),
                file_id: written.file_id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(from_a.data, b"hello-from-a");
        let cross_read = svc
            .read_file(Request::new(ReadFileReq {
                working_copy_path: "/tmp/b".into(),
                file_id: written.file_id,
            }))
            .await
            .expect_err("expected per-mount-store isolation");
        assert_eq!(cross_read.code(), tonic::Code::NotFound);

        // DaemonStatus surfaces both mounts in a deterministic order.
        let status = svc
            .daemon_status(Request::new(DaemonStatusReq {}))
            .await
            .unwrap()
            .into_inner();
        let paths: Vec<_> = status.data.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["/tmp/a", "/tmp/b"]);
    }

    #[tokio::test]
    async fn duplicate_initialize_rejected() {
        let svc = JujutsuService::bare();
        let req = || {
            Request::new(InitializeReq {
                path: "/tmp/repo".into(),
                remote: "".into(),
            })
        };
        svc.initialize(req()).await.unwrap();
        let err = svc
            .initialize(req())
            .await
            .expect_err("expected already_exists");
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
    }

    #[tokio::test]
    async fn set_checkout_state_requires_initialize() {
        let svc = JujutsuService::bare();
        let err = svc
            .set_checkout_state(Request::new(SetCheckoutStateReq {
                working_copy_path: "/never/initialized".into(),
                checkout_state: Some(CheckoutState {
                    op_id: vec![0; 32],
                    workspace_id: b"default".to_vec(),
                }),
            }))
            .await
            .expect_err("expected not_found");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    /// CheckOut updates `Mount.root_tree_id` and the per-mount FS so
    /// subsequent `GetTreeState` / `Snapshot` reads see the new root,
    /// and rejects unknown tree ids cleanly. This is the wire-side
    /// contract `LockedYakWorkingCopy::check_out` depends on.
    #[tokio::test]
    async fn check_out_updates_root_tree_and_validates_input() {
        let svc = JujutsuService::bare();
        let path = "/tmp/repo".to_string();
        init_mount(&svc, &path).await;

        // Build a non-empty tree and write it to the per-mount store.
        let file_id = svc
            .write_file(Request::new(WriteFileReq {
                working_copy_path: path.clone(),
                data: b"hello".to_vec(),
            }))
            .await
            .unwrap()
            .into_inner();
        let tree_id = svc
            .write_tree(Request::new(WriteTreeReq {
                working_copy_path: path.clone(),
                tree: Some(Tree {
                    entries: vec![tree::Entry {
                        name: "hello.txt".into(),
                        value: Some(TreeValue {
                            value: Some(tree_value::Value::File(tree_value::File {
                                id: file_id.file_id.clone(),
                                executable: false,
                                copy_id: Vec::new(),
                            })),
                        }),
                    }],
                }),
            }))
            .await
            .unwrap()
            .into_inner();

        // Check out the new tree.
        svc.check_out(Request::new(CheckOutReq {
            working_copy_path: path.clone(),
            new_tree_id: tree_id.tree_id.clone(),
        }))
        .await
        .expect("check_out");

        // GetTreeState now returns the new tree id.
        let after = svc
            .get_tree_state(Request::new(GetTreeStateReq {
                working_copy_path: path.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(after.tree_id, tree_id.tree_id);

        // Snapshot reflects the same updated root tree id (still a stub
        // until M6, but the mount-side state must agree).
        let snap = svc
            .snapshot(Request::new(SnapshotReq {
                working_copy_path: path.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(snap.tree_id, tree_id.tree_id);

        // Checking out an unknown tree id rejects with failed_precondition,
        // and leaves `root_tree_id` as it was after the successful swap.
        let bogus = vec![0xee; 32];
        let err = svc
            .check_out(Request::new(CheckOutReq {
                working_copy_path: path.clone(),
                new_tree_id: bogus,
            }))
            .await
            .expect_err("expected failed_precondition for unknown tree");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        let still = svc
            .get_tree_state(Request::new(GetTreeStateReq {
                working_copy_path: path,
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(still.tree_id, tree_id.tree_id);
    }

    /// CheckOut against a missing mount must NotFound, mirroring the
    /// other store/WC RPCs. Catches a typo in the lookup path.
    #[tokio::test]
    async fn check_out_without_mount_is_not_found() {
        let svc = JujutsuService::bare();
        let err = svc
            .check_out(Request::new(CheckOutReq {
                working_copy_path: "/never/initialized".into(),
                new_tree_id: vec![0; 32],
            }))
            .await
            .expect_err("expected not_found");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    /// Store RPCs require an initialized mount.
    #[tokio::test]
    async fn store_rpc_without_mount_is_not_found() {
        let svc = JujutsuService::bare();
        let err = svc
            .write_file(Request::new(WriteFileReq {
                working_copy_path: "/never/initialized".into(),
                data: b"hi".to_vec(),
            }))
            .await
            .expect_err("expected not_found");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    /// End-to-end M6 Snapshot RPC: drive a write through the per-mount
    /// `JjYakFs`, then call `Snapshot` and confirm the daemon both
    /// returns the new root tree id and stamps it on `Mount.root_tree_id`
    /// (so subsequent `GetTreeState` agrees).
    #[tokio::test]
    async fn snapshot_rpc_returns_new_tree_id_after_vfs_write() {
        let svc = JujutsuService::bare();
        let path = "/tmp/repo".to_string();
        init_mount(&svc, &path).await;

        // Initial root tree is the empty tree.
        let empty = svc
            .get_empty_tree_id(Request::new(GetEmptyTreeIdReq {
                working_copy_path: path.clone(),
            }))
            .await
            .unwrap()
            .into_inner();

        // Drive a write through the per-mount fs.
        let fs = svc
            .fs_for_test(&path)
            .await
            .expect("mount initialised above");
        let (file_ino, _) = fs
            .create_file(fs.root(), "hello.txt", false)
            .await
            .expect("create_file");
        fs.write(file_ino, 0, b"hello").await.unwrap();

        // Snapshot picks up the write and produces a fresh tree id.
        let snap = svc
            .snapshot(Request::new(SnapshotReq {
                working_copy_path: path.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_ne!(snap.tree_id, empty.tree_id);

        // GetTreeState reflects the same id (Mount.root_tree_id was
        // stamped on the success path).
        let after = svc
            .get_tree_state(Request::new(GetTreeStateReq {
                working_copy_path: path,
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(after.tree_id, snap.tree_id);
    }

    /// Snapshot against a missing mount must NotFound, mirroring CheckOut.
    #[tokio::test]
    async fn snapshot_without_mount_is_not_found() {
        let svc = JujutsuService::bare();
        let err = svc
            .snapshot(Request::new(SnapshotReq {
                working_copy_path: "/never/initialized".into(),
            }))
            .await
            .expect_err("expected not_found");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    /// Mountpoint validation is exercised on its own (the full
    /// `Initialize` path skips it under `bare`); these cases test the
    /// helper directly so we don't have to spin up a real `VfsManager`.
    mod validate_mountpoint {
        use super::super::{validate_mountpoint, MountpointError};

        #[test]
        fn rejects_missing() {
            let err =
                validate_mountpoint("/definitely/does/not/exist/jjyak").expect_err("missing");
            assert_matches::assert_matches!(err, MountpointError::Missing(_));
        }

        #[test]
        fn rejects_file() {
            let f = tempfile::NamedTempFile::new().unwrap();
            let path = f.path().to_str().unwrap().to_owned();
            let err = validate_mountpoint(&path).expect_err("not a dir");
            assert_matches::assert_matches!(err, MountpointError::NotADirectory(_));
        }

        #[test]
        fn rejects_non_empty_directory() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("squatter"), b"data").unwrap();
            let path = dir.path().to_str().unwrap().to_owned();
            let err = validate_mountpoint(&path).expect_err("not empty");
            assert_matches::assert_matches!(err, MountpointError::NotEmpty(_));
        }

        #[test]
        fn accepts_empty_directory() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().to_str().unwrap().to_owned();
            validate_mountpoint(&path).expect("empty dir on same fs");
        }
    }

    /// `Initialize` writes `mount.toml`; `SetCheckoutState` updates it;
    /// rehydrate on a fresh service rebuilds the in-memory `Mount` from
    /// the on-disk metadata + redb store (M8 / Layer B). Uses a real
    /// `dir://` remote so the rehydrate path also exercises M9's
    /// `remote::parse` → `Mount.remote_store` round-trip.
    #[tokio::test]
    async fn persisted_mount_rehydrates_after_restart() {
        let storage_dir = tempfile::tempdir().expect("storage tempdir");
        let remote_dir = tempfile::tempdir().expect("remote tempdir");
        let mount_path = "/tmp/yak-rehydrate-test";
        let remote_url = format!("dir://{}", remote_dir.path().display());

        // Phase 1: initialize and stamp some checkout state into the
        // first daemon. No VFS bind (vfs_handle = None) so we can use
        // arbitrary string paths without creating real directories.
        let svc1 = JujutsuService::new(
            None,
            StorageConfig::on_disk(storage_dir.path().to_owned()),
        );
        svc1.initialize(Request::new(InitializeReq {
            path: mount_path.into(),
            remote: remote_url.clone(),
        }))
        .await
        .expect("initialize");

        let op_id = vec![0xab, 0xcd, 0xef];
        let workspace_id = b"default".to_vec();
        svc1.set_checkout_state(Request::new(SetCheckoutStateReq {
            working_copy_path: mount_path.into(),
            checkout_state: Some(CheckoutState {
                op_id: op_id.clone(),
                workspace_id: workspace_id.clone(),
            }),
        }))
        .await
        .expect("set_checkout_state");

        // Confirm mount.toml exists with expected content.
        let meta_path = mount_meta::meta_path(storage_dir.path(), mount_path);
        let on_disk = MountMetadata::read_from(&meta_path).expect("read mount.toml");
        assert_eq!(on_disk.working_copy_path, mount_path);
        assert_eq!(on_disk.remote, remote_url);
        assert_eq!(on_disk.op_id, op_id);
        assert_eq!(on_disk.workspace_id, workspace_id);

        // Drop svc1 — simulates daemon restart.
        drop(svc1);

        // Phase 2: fresh service over the same storage_dir. Rehydrate
        // re-attaches the mount; subsequent `GetCheckoutState` returns
        // the values we wrote earlier.
        let svc2 = JujutsuService::new(
            None,
            StorageConfig::on_disk(storage_dir.path().to_owned()),
        );
        svc2.rehydrate().await.expect("rehydrate");

        let got = svc2
            .get_checkout_state(Request::new(GetCheckoutStateReq {
                working_copy_path: mount_path.into(),
            }))
            .await
            .expect("get_checkout_state")
            .into_inner();
        assert_eq!(got.op_id, op_id);
        assert_eq!(got.workspace_id, workspace_id);

        // DaemonStatus should also list the rehydrated mount with the
        // correct remote, proving Mount.remote round-tripped.
        let status = svc2
            .daemon_status(Request::new(DaemonStatusReq {}))
            .await
            .expect("daemon_status")
            .into_inner();
        assert_eq!(status.data.len(), 1);
        assert_eq!(status.data[0].path, mount_path);
        assert_eq!(status.data[0].remote, remote_url);
    }

    /// Rehydrating an empty `storage_dir` is a no-op (the
    /// `<storage_dir>/mounts/` directory may not exist yet on a fresh
    /// install). It must not error.
    #[tokio::test]
    async fn rehydrate_with_no_mounts_is_noop() {
        let storage_dir = tempfile::tempdir().expect("storage tempdir");
        let svc = JujutsuService::new(
            None,
            StorageConfig::on_disk(storage_dir.path().to_owned()),
        );
        svc.rehydrate().await.expect("rehydrate empty dir");
    }

    // ---- M9 / Layer C: remote blob CAS service-level integration ----

    /// Initialize ergonomics: an unparseable remote URL surfaces as
    /// `invalid_argument`. Empty string is still accepted as "no
    /// remote" (back-compat with pre-M9 mounts and tests).
    #[tokio::test]
    async fn initialize_rejects_unparseable_remote() {
        let svc = JujutsuService::bare();
        let err = svc
            .initialize(Request::new(InitializeReq {
                path: "/tmp/repo".into(),
                remote: "localhost".into(),
            }))
            .await
            .expect_err("expected invalid_argument for unparseable URL");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            err.message().contains("no scheme"),
            "expected scheme error, got: {}",
            err.message()
        );
    }

    /// `WriteFile` with a `dir://` remote pushes the prost-encoded blob
    /// into the remote synchronously (M9 §13.4 — write-through). The
    /// blob lands at `<remote>/file/<hex(id)>`.
    #[tokio::test]
    async fn write_file_pushes_to_dir_remote() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir");
        let svc = JujutsuService::bare();
        let path = "/tmp/repo".to_string();
        init_mount_with_remote(
            &svc,
            &path,
            &format!("dir://{}", remote_dir.path().display()),
        )
        .await;

        let written = svc
            .write_file(Request::new(WriteFileReq {
                working_copy_path: path,
                data: b"hello-remote".to_vec(),
            }))
            .await
            .expect("write_file")
            .into_inner();

        // Hex-encode the 32-byte id the way the dir backend does it.
        let mut hex_id = String::with_capacity(64);
        for b in &written.file_id {
            use std::fmt::Write;
            let _ = write!(&mut hex_id, "{b:02x}");
        }
        let blob_path = remote_dir.path().join("file").join(&hex_id);
        let bytes = std::fs::read(&blob_path).expect("blob landed on remote");
        let proto = proto::jj_interface::File::decode(bytes.as_slice())
            .expect("decoding remote file blob");
        assert_eq!(proto.data, b"hello-remote");
    }

    /// `ReadFile` falls back to the remote on local miss. The fetched
    /// bytes are persisted to the local store so a second `ReadFile`
    /// hits the cache (no remote round-trip).
    #[tokio::test]
    async fn read_file_falls_back_to_remote_on_local_miss() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir");
        let remote_url = format!("dir://{}", remote_dir.path().display());

        // Service A writes the blob — its local store + the shared
        // remote both end up with it.
        let svc_a = JujutsuService::bare();
        init_mount_with_remote(&svc_a, "/tmp/a", &remote_url).await;
        let written = svc_a
            .write_file(Request::new(WriteFileReq {
                working_copy_path: "/tmp/a".into(),
                data: b"shared-content".to_vec(),
            }))
            .await
            .expect("write_file")
            .into_inner();

        // Service B was never told about the blob. Its local store is
        // empty, but it shares the remote — `ReadFile` should fetch
        // through the remote.
        let svc_b = JujutsuService::bare();
        init_mount_with_remote(&svc_b, "/tmp/b", &remote_url).await;
        let got = svc_b
            .read_file(Request::new(ReadFileReq {
                working_copy_path: "/tmp/b".into(),
                file_id: written.file_id.clone(),
            }))
            .await
            .expect("read_file via read-through")
            .into_inner();
        assert_eq!(got.data, b"shared-content");

        // Confirm the read-through populated B's local store: a second
        // read against a remote-less service B' over the same storage
        // dir would still hit. We sidestep "same storage dir" here by
        // checking that the same id round-trips through B's local
        // `read_file` directly via `mount_handles`.
        let (store_b, _remote_b) = mount_handles(&svc_b.mounts, "/tmp/b").await.unwrap();
        let id: ty::Id = proto::jj_interface::FileId {
            file_id: written.file_id,
        }
        .try_into()
        .unwrap();
        let cached = store_b.get_file(id).expect("get_file (after read-through)");
        assert!(
            cached.is_some(),
            "read-through should have populated the local store"
        );
    }

    /// `ReadFile` with no remote and a local miss returns NotFound,
    /// preserving pre-M9 behavior.
    #[tokio::test]
    async fn read_file_local_miss_no_remote_is_not_found() {
        let svc = JujutsuService::bare();
        let path = "/tmp/repo".to_string();
        init_mount(&svc, &path).await;
        let err = svc
            .read_file(Request::new(ReadFileReq {
                working_copy_path: path,
                file_id: vec![0xff; 32],
            }))
            .await
            .expect_err("expected not_found");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    /// Snapshot pushes every reachable blob from the new root tree to
    /// the remote (M9 §13.2 post-snapshot walk). Drives a write through
    /// the per-mount `JjYakFs`, calls `Snapshot`, and verifies the file
    /// blob *and* the rolled-up tree blob both land on the remote.
    #[tokio::test]
    async fn snapshot_pushes_reachable_blobs_to_remote() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir");
        let svc = JujutsuService::bare();
        let path = "/tmp/repo".to_string();
        init_mount_with_remote(
            &svc,
            &path,
            &format!("dir://{}", remote_dir.path().display()),
        )
        .await;

        // Drive a write through the FS (bypasses the RPC layer, so
        // write-through doesn't run — exercises only the post-snapshot
        // walk).
        let fs = svc.fs_for_test(&path).await.expect("mount initialised");
        let (file_ino, _) = fs
            .create_file(fs.root(), "hello.txt", false)
            .await
            .expect("create_file");
        fs.write(file_ino, 0, b"snapshot-content").await.unwrap();

        let snap = svc
            .snapshot(Request::new(SnapshotReq {
                working_copy_path: path,
            }))
            .await
            .expect("snapshot")
            .into_inner();
        assert_eq!(snap.tree_id.len(), 32);

        // The new root tree blob is on the remote.
        let mut tree_hex = String::with_capacity(64);
        for b in &snap.tree_id {
            use std::fmt::Write;
            let _ = write!(&mut tree_hex, "{b:02x}");
        }
        let tree_blob = remote_dir.path().join("tree").join(&tree_hex);
        assert!(tree_blob.exists(), "tree blob {tree_hex} should be on remote");

        // The file blob is also on the remote (the walk recursed into
        // the tree's File entry).
        let file_dir = remote_dir.path().join("file");
        let entries: Vec<_> = std::fs::read_dir(&file_dir)
            .expect("file kind dir exists")
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with(".tmp"))
            .collect();
        assert_eq!(entries.len(), 1, "expected one file blob on remote");
        let file_bytes =
            std::fs::read(entries[0].path()).expect("read remote file blob");
        let file_proto = proto::jj_interface::File::decode(file_bytes.as_slice())
            .expect("decoding remote file blob");
        assert_eq!(file_proto.data, b"snapshot-content");
    }

    /// End-to-end M9 §13.7 gRPC analogue: two `JujutsuService`
    /// instances sharing a `grpc://` remote backed by a third
    /// `RemoteStoreService` over a real tonic listener. Service A
    /// writes a file (write-through pushes to the gRPC peer); service
    /// B issues `read_file` for the same id, hits its empty local
    /// store, and falls back through the gRPC remote to fetch the
    /// content. Proves the byte-typed trait is honest under a real
    /// network transport at the RPC layer above it.
    #[tokio::test]
    async fn two_services_share_blobs_via_grpc_remote() {
        use std::sync::Arc;

        use tokio::net::TcpListener;
        use tokio_stream::wrappers::TcpListenerStream;
        use tonic::transport::Server as GrpcServer;

        use crate::remote::fs::FsRemoteStore;
        use crate::remote::server::RemoteStoreService;

        // Spawn a `RemoteStoreServer` on an ephemeral port, backed by
        // an FsRemoteStore tempdir so this test doesn't shell out to
        // anything else.
        let backing = tempfile::tempdir().unwrap();
        let backend: Arc<dyn RemoteStore> =
            Arc::new(FsRemoteStore::new(backing.path().to_owned()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            GrpcServer::builder()
                .add_service(RemoteStoreService::new(backend).into_server())
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .expect("RemoteStore tonic server")
        });
        let remote_url = format!("grpc://{addr}");

        // Service A: writes the blob.
        let svc_a = JujutsuService::bare();
        init_mount_with_remote(&svc_a, "/tmp/a", &remote_url).await;
        let written = svc_a
            .write_file(Request::new(WriteFileReq {
                working_copy_path: "/tmp/a".into(),
                data: b"grpc-shared".to_vec(),
            }))
            .await
            .expect("write_file via grpc remote")
            .into_inner();

        // Service B: never saw the blob locally, but shares the remote.
        // ReadFile must round-trip through the gRPC peer.
        let svc_b = JujutsuService::bare();
        init_mount_with_remote(&svc_b, "/tmp/b", &remote_url).await;
        let got = svc_b
            .read_file(Request::new(ReadFileReq {
                working_copy_path: "/tmp/b".into(),
                file_id: written.file_id.clone(),
            }))
            .await
            .expect("read_file via grpc read-through")
            .into_inner();
        assert_eq!(got.data, b"grpc-shared");
    }

    /// Snapshot is idempotent across remote pushes: a second snapshot
    /// of an unchanged mount must not error (the walk hits `has_blob`
    /// for every entry and skips the put), and the remote contents
    /// stay byte-identical. Catches regressions in the dedupe set.
    #[tokio::test]
    async fn snapshot_is_idempotent_against_remote() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir");
        let svc = JujutsuService::bare();
        let path = "/tmp/repo".to_string();
        init_mount_with_remote(
            &svc,
            &path,
            &format!("dir://{}", remote_dir.path().display()),
        )
        .await;

        let fs = svc.fs_for_test(&path).await.unwrap();
        let (ino, _) = fs.create_file(fs.root(), "f.txt", false).await.unwrap();
        fs.write(ino, 0, b"x").await.unwrap();
        let first = svc
            .snapshot(Request::new(SnapshotReq {
                working_copy_path: path.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        let second = svc
            .snapshot(Request::new(SnapshotReq {
                working_copy_path: path,
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            first.tree_id, second.tree_id,
            "clean re-snapshot should produce the same tree id"
        );
    }

    // ---- M10 §10.6: FUSE-side remote read-through ---------------------

    /// End-to-end FUSE-side read-through: service A populates a remote
    /// (write-through + post-snapshot push); service B initialised
    /// with the same remote but an empty local store can `lookup` /
    /// `read` through B's `JjYakFs` and get the right content via
    /// the lazy remote fetch in `vfs/yak_fs.rs`.
    ///
    /// This is the M10 analog of M9's
    /// `read_file_falls_back_to_remote_on_local_miss`, but exercising
    /// the FS layer rather than the RPC layer. Pre-M10 this test
    /// would fail at `lookup` / `getattr` / `read` with EIO because
    /// `vfs/yak_fs.rs::read_*` mapped `StoreMiss` straight to that;
    /// now those helpers fall through to `RemoteStore::get_blob`.
    #[tokio::test]
    async fn fuse_layer_reads_through_remote_on_local_miss() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir");
        let remote_url = format!("dir://{}", remote_dir.path().display());

        // Service A writes a file + builds a tree containing it,
        // then snapshots so the remote sees both the file blob and
        // the rolled-up tree blob.
        let svc_a = JujutsuService::bare();
        init_mount_with_remote(&svc_a, "/tmp/a", &remote_url).await;
        let fs_a = svc_a.fs_for_test("/tmp/a").await.expect("svc_a mount");
        let (file_ino_a, _) = fs_a
            .create_file(fs_a.root(), "shared.txt", false)
            .await
            .expect("create_file");
        fs_a.write(file_ino_a, 0, b"read-through-content")
            .await
            .unwrap();
        let snap_a = svc_a
            .snapshot(Request::new(SnapshotReq {
                working_copy_path: "/tmp/a".into(),
            }))
            .await
            .expect("snapshot")
            .into_inner();
        let tree_id: ty::Id = TreeId {
            tree_id: snap_a.tree_id.clone(),
        }
        .try_into()
        .unwrap();

        // Service B: separate storage_dir (so the tree+file blobs
        // genuinely aren't local), shares the same remote. Tell it
        // to check out A's tree id; B's `JjYakFs::check_out` calls
        // `read_tree` which must fall through to the remote.
        let svc_b = JujutsuService::bare();
        init_mount_with_remote(&svc_b, "/tmp/b", &remote_url).await;
        svc_b
            .check_out(Request::new(CheckOutReq {
                working_copy_path: "/tmp/b".into(),
                new_tree_id: tree_id.0.to_vec(),
            }))
            .await
            .expect("check_out via remote read-through");

        // Now drive a kernel-style walk through B's FS: lookup +
        // getattr + read. Each one would hit StoreMiss without M10
        // §10.6 because B's local store didn't have the underlying
        // blobs at init time.
        let fs_b = svc_b.fs_for_test("/tmp/b").await.expect("svc_b mount");
        let file_ino_b = fs_b
            .lookup(fs_b.root(), "shared.txt")
            .await
            .expect("lookup via remote");
        let attr = fs_b
            .getattr(file_ino_b)
            .await
            .expect("getattr via remote");
        assert_eq!(attr.size, b"read-through-content".len() as u64);
        let (data, eof) = fs_b
            .read(file_ino_b, 0, 1024)
            .await
            .expect("read via remote");
        assert!(eof);
        assert_eq!(data, b"read-through-content");
    }

    /// Negative case: with no remote configured, a `StoreMiss`
    /// against `JjYakFs::check_out` still surfaces as the expected
    /// failed_precondition (tree not in store), preserving pre-M10
    /// behavior for mounts without a Layer-C remote.
    #[tokio::test]
    async fn fuse_layer_store_miss_no_remote_is_failed_precondition() {
        let svc = JujutsuService::bare();
        init_mount(&svc, "/tmp/repo").await;
        // A fabricated tree id that nothing wrote — and no remote
        // to fetch it from.
        let bogus = vec![0xaa; 32];
        let err = svc
            .check_out(Request::new(CheckOutReq {
                working_copy_path: "/tmp/repo".into(),
                new_tree_id: bogus,
            }))
            .await
            .expect_err("expected failed_precondition");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    /// FUSE-side read-through also primes the local cache: a second
    /// `lookup` / `read` after the first must not depend on the
    /// remote being reachable (we don't simulate a "remote going
    /// down" condition here, but proving the local store now contains
    /// the blob is the structural guarantee).
    #[tokio::test]
    async fn fuse_layer_read_through_populates_local_cache() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir");
        let remote_url = format!("dir://{}", remote_dir.path().display());

        // Service A populates the remote.
        let svc_a = JujutsuService::bare();
        init_mount_with_remote(&svc_a, "/tmp/a", &remote_url).await;
        let fs_a = svc_a.fs_for_test("/tmp/a").await.unwrap();
        let (ino, _) = fs_a.create_file(fs_a.root(), "f.txt", false).await.unwrap();
        fs_a.write(ino, 0, b"cached").await.unwrap();
        let snap = svc_a
            .snapshot(Request::new(SnapshotReq {
                working_copy_path: "/tmp/a".into(),
            }))
            .await
            .unwrap()
            .into_inner();

        // Service B: fetch via read-through.
        let svc_b = JujutsuService::bare();
        init_mount_with_remote(&svc_b, "/tmp/b", &remote_url).await;
        svc_b
            .check_out(Request::new(CheckOutReq {
                working_copy_path: "/tmp/b".into(),
                new_tree_id: snap.tree_id.clone(),
            }))
            .await
            .unwrap();
        let fs_b = svc_b.fs_for_test("/tmp/b").await.unwrap();
        let f_ino = fs_b.lookup(fs_b.root(), "f.txt").await.unwrap();
        let _ = fs_b.read(f_ino, 0, 1024).await.unwrap();

        // After the read-through, B's local store now has the file
        // and tree blobs. We probe the file blob directly — the
        // tree blob was already populated by `check_out`'s
        // validate-via-`read_tree`.
        let (store_b, _remote_b) = mount_handles(&svc_b.mounts, "/tmp/b").await.unwrap();
        let listing = fs_b.readdir(fs_b.root()).await.unwrap();
        let entry = listing.iter().find(|e| e.name == "f.txt").unwrap();
        // file id is whatever the lookup just resolved to — but we
        // don't have a clean way to extract it without going through
        // the slab. Instead, walk the tree blob from the local
        // store and confirm the file id resolves via store_b too.
        let tree_id: ty::Id = TreeId {
            tree_id: snap.tree_id,
        }
        .try_into()
        .unwrap();
        let tree = store_b
            .get_tree(tree_id)
            .expect("tree in local store after read-through")
            .unwrap();
        let file_entry = tree
            .entries
            .iter()
            .find(|m| m.name == "f.txt")
            .expect("file in tree");
        let file_id = match file_entry.entry {
            TreeEntry::File { id, .. } => id,
            _ => panic!("expected file entry"),
        };
        assert!(
            store_b
                .get_file(file_id)
                .expect("get_file in local cache")
                .is_some(),
            "file blob should be cached locally after read-through"
        );
        // Sanity: entry inode matches what readdir saw.
        assert!(matches!(entry.kind, FileKind::Regular));
    }

    // ---- M10.5: catalog RPC dispatch ----------------------------

    /// No-remote mount: catalog RPCs route through `LocalRefs`, so
    /// the create/get/cas/list cycle works against per-mount redb.
    /// This is the single-daemon flow that all current tests exercise
    /// (every existing `init_mount` call passes `remote = ""`).
    #[tokio::test]
    async fn catalog_rpcs_route_to_local_refs_when_no_remote() {
        let svc = JujutsuService::bare();
        let path = "/tmp/repo".to_string();
        init_mount(&svc, &path).await;

        // Create-only: ref must not exist before this call.
        let cas = svc
            .cas_catalog_ref(Request::new(CasCatalogRefReq {
                working_copy_path: path.clone(),
                name: "op_heads".into(),
                expected: None,
                new: Some(b"v0".to_vec()),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(cas.updated);
        assert!(cas.actual.is_none());

        // Read it back: hits LocalRefs.
        let got = svc
            .get_catalog_ref(Request::new(GetCatalogRefReq {
                working_copy_path: path.clone(),
                name: "op_heads".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(got.found);
        assert_eq!(got.value, b"v0");

        // Stale CAS: precondition mismatch returns the current value.
        let cas = svc
            .cas_catalog_ref(Request::new(CasCatalogRefReq {
                working_copy_path: path.clone(),
                name: "op_heads".into(),
                expected: Some(b"WRONG".to_vec()),
                new: Some(b"v1".to_vec()),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!cas.updated);
        assert_eq!(cas.actual.as_deref(), Some(b"v0".as_ref()));

        // List sees the one ref.
        let list = svc
            .list_catalog_refs(Request::new(ListCatalogRefsReq {
                working_copy_path: path,
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(list.names, vec!["op_heads".to_string()]);
    }

    /// With a `dir://` remote configured, catalog RPCs route through
    /// the remote's ref methods, not the local fallback. Two daemons
    /// pointed at the same `dir://` therefore see each other's writes
    /// — the multi-daemon arbitration property M10 set up.
    #[tokio::test]
    async fn catalog_rpcs_route_to_remote_when_configured() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir");
        let remote_url = format!("dir://{}", remote_dir.path().display());

        let svc_a = JujutsuService::bare();
        init_mount_with_remote(&svc_a, "/tmp/a", &remote_url).await;
        let svc_b = JujutsuService::bare();
        init_mount_with_remote(&svc_b, "/tmp/b", &remote_url).await;

        // A creates op_heads.
        let cas = svc_a
            .cas_catalog_ref(Request::new(CasCatalogRefReq {
                working_copy_path: "/tmp/a".into(),
                name: "op_heads".into(),
                expected: None,
                new: Some(b"from-a".to_vec()),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(cas.updated);

        // B sees A's value through the same remote.
        let got = svc_b
            .get_catalog_ref(Request::new(GetCatalogRefReq {
                working_copy_path: "/tmp/b".into(),
                name: "op_heads".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(got.found, "B should see A's ref through the shared remote");
        assert_eq!(got.value, b"from-a");

        // B's create-only must conflict — the remote already has the ref.
        let cas = svc_b
            .cas_catalog_ref(Request::new(CasCatalogRefReq {
                working_copy_path: "/tmp/b".into(),
                name: "op_heads".into(),
                expected: None,
                new: Some(b"from-b".to_vec()),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!cas.updated);
        assert_eq!(cas.actual.as_deref(), Some(b"from-a".as_ref()));
    }

    /// Two no-remote mounts on the same service have independent
    /// `LocalRefs` (per-mount redb) — A's writes are invisible to B.
    /// This verifies the local fallback is per-mount, not global.
    #[tokio::test]
    async fn catalog_local_refs_are_per_mount() {
        let svc = JujutsuService::bare();
        init_mount(&svc, "/tmp/a").await;
        init_mount(&svc, "/tmp/b").await;

        svc.cas_catalog_ref(Request::new(CasCatalogRefReq {
            working_copy_path: "/tmp/a".into(),
            name: "op_heads".into(),
            expected: None,
            new: Some(b"only-on-a".to_vec()),
        }))
        .await
        .unwrap();

        // B's catalog must not see A's local ref.
        let got = svc
            .get_catalog_ref(Request::new(GetCatalogRefReq {
                working_copy_path: "/tmp/b".into(),
                name: "op_heads".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!got.found);
    }

    /// Bad ref names are rejected with `invalid_argument` at the wire
    /// boundary — same shape as the `RemoteStoreService` ref RPCs.
    #[tokio::test]
    async fn catalog_rpcs_reject_bad_name() {
        let svc = JujutsuService::bare();
        init_mount(&svc, "/tmp/a").await;
        for bad in ["", "a/b", "..", "a\0b"] {
            let err = svc
                .get_catalog_ref(Request::new(GetCatalogRefReq {
                    working_copy_path: "/tmp/a".into(),
                    name: bad.into(),
                }))
                .await
                .expect_err(bad);
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            let err = svc
                .cas_catalog_ref(Request::new(CasCatalogRefReq {
                    working_copy_path: "/tmp/a".into(),
                    name: bad.into(),
                    expected: None,
                    new: Some(b"x".to_vec()),
                }))
                .await
                .expect_err(bad);
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }
    }

    /// Catalog RPCs against an unknown mount surface as `not_found`
    /// — same as every other per-mount RPC.
    #[tokio::test]
    async fn catalog_rpcs_unknown_mount_is_not_found() {
        let svc = JujutsuService::bare();
        let err = svc
            .get_catalog_ref(Request::new(GetCatalogRefReq {
                working_copy_path: "/never/initialized".into(),
                name: "op_heads".into(),
            }))
            .await
            .expect_err("must error");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }
}
