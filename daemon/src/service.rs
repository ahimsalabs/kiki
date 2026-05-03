use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context as _};
use bytes::Bytes;
use jj_lib::backend::CommitId as JjCommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{OpStore as _, OperationId, RootOperationData, ViewId};
use jj_lib::simple_op_store::SimpleOpStore;
use proto::jj_interface::*;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::git_store::{self, GitContentStore, GitEntryKind, OpPrefixResult};
use crate::mount_meta::{self, MountMetadata};
use crate::remote::fetch::{self, FetchError};
use crate::remote::{self, BlobKind, RemoteStore};
use crate::ty;
use crate::vfs::{FsError, JjKikiFs, KikiFs};
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

    /// Where the redb op-store file for `working_copy_path` should live,
    /// if any. Returns `None` for the in-memory variant.
    fn redb_path_for(&self, working_copy_path: &str) -> Option<PathBuf> {
        match self {
            StorageConfig::OnDisk { root } => {
                Some(mount_meta::store_path(root, working_copy_path))
            }
            StorageConfig::InMemory => None,
        }
    }

    /// Where the git-backed content store directory for
    /// `working_copy_path` should live (`<root>/mounts/<hash>/git_store/`).
    /// Returns `None` for the in-memory variant.
    fn git_store_path_for(&self, working_copy_path: &str) -> Option<PathBuf> {
        match self {
            StorageConfig::OnDisk { root } => {
                Some(mount_meta::mount_dir(root, working_copy_path).join("git_store"))
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

    /// Scratch directory for VFS redirections (M10.7). Lives under
    /// `<root>/mounts/<hash>/scratch/`. Returns `None` for in-memory.
    fn scratch_dir_for(&self, working_copy_path: &str) -> Option<PathBuf> {
        match self {
            StorageConfig::OnDisk { root } => {
                Some(mount_meta::mount_dir(root, working_copy_path).join("scratch"))
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

fn metadata_status(context: &str) -> impl FnOnce(anyhow::Error) -> Status + '_ {
    move |e| Status::internal(format!("{context}: {e:#}"))
}

fn persist_metadata_snapshot(
    meta_path: &Option<PathBuf>,
    metadata: &MountMetadata,
) -> anyhow::Result<()> {
    if let Some(path) = meta_path {
        metadata
            .write_to(path)
            .with_context(|| format!("persisting {}", path.display()))?;
    }
    Ok(())
}

/// Hex-encode arbitrary bytes for display.
fn hex_bytes(id: &[u8]) -> String {
    let mut s = String::with_capacity(id.len() * 2);
    for b in id {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

#[derive(Clone, Copy)]
enum OpStoreBlobKind {
    View,
    Operation,
}

impl OpStoreBlobKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Operation => "operation",
        }
    }
}

fn op_store_root_data() -> RootOperationData {
    RootOperationData {
        root_commit_id: JjCommitId::from_bytes(&[0; 20]),
    }
}

async fn validate_op_store_blob_id(
    kind: OpStoreBlobKind,
    requested_id: &[u8],
    bytes: &[u8],
) -> anyhow::Result<()> {
    if requested_id.len() != 64 {
        anyhow::bail!(
            "{} id must be 64 bytes, got {}",
            kind.as_str(),
            requested_id.len()
        );
    }
    if requested_id.iter().all(|b| *b == 0) {
        anyhow::bail!(
            "{} id is the reserved all-zero root id and must not be stored",
            kind.as_str()
        );
    }

    let scratch = TempDir::new().context("creating temp op-store scratch dir")?;
    let store = SimpleOpStore::init(scratch.path(), op_store_root_data())
        .with_context(|| format!("initializing temp {} store", kind.as_str()))?;
    match kind {
        OpStoreBlobKind::View => {
            let requested_id = ViewId::new(requested_id.to_vec());
            std::fs::write(scratch.path().join("views").join(requested_id.hex()), bytes)
                .context("writing temp view blob")?;
            let view = store
                .read_view(&requested_id)
                .await
                .map_err(|e| anyhow!("decoding view blob: {e}"))?;
            let canonical_id = store
                .write_view(&view)
                .await
                .map_err(|e| anyhow!("rehashing view blob: {e}"))?;
            if canonical_id.as_bytes() != requested_id.as_bytes() {
                anyhow::bail!(
                    "view id/bytes mismatch: requested {} but canonical id is {}",
                    requested_id.hex(),
                    canonical_id.hex()
                );
            }
        }
        OpStoreBlobKind::Operation => {
            let requested_id = OperationId::new(requested_id.to_vec());
            std::fs::write(
                scratch.path().join("operations").join(requested_id.hex()),
                bytes,
            )
            .context("writing temp operation blob")?;
            let operation = store
                .read_operation(&requested_id)
                .await
                .map_err(|e| anyhow!("decoding operation blob: {e}"))?;
            let canonical_id = store
                .write_operation(&operation)
                .await
                .map_err(|e| anyhow!("rehashing operation blob: {e}"))?;
            if canonical_id.as_bytes() != requested_id.as_bytes() {
                anyhow::bail!(
                    "operation id/bytes mismatch: requested {} but canonical id is {}",
                    requested_id.hex(),
                    canonical_id.hex()
                );
            }
        }
    }
    Ok(())
}

/// Extract resolved (non-conflicting) local bookmarks from raw View proto
/// bytes (jj-lib `simple_op_store::View` format).
///
/// Returns `(bookmark_name, commit_id_bytes)` pairs. Conflicted or absent
/// targets are silently skipped. Decode failures return an empty vec (the
/// caller treats this as best-effort).
fn bookmarks_from_view_bytes(data: &[u8]) -> Vec<(String, Vec<u8>)> {
    use jj_lib::protos::simple_op_store as op_proto;
    use prost014::Message as _;

    let Ok(view) = op_proto::View::decode(data) else {
        return Vec::new();
    };

    view.bookmarks
        .into_iter()
        .filter_map(|b| {
            let id = resolved_commit_id(b.local_target?)?;
            Some((b.name, id))
        })
        .collect()
}

/// Extract a single commit id from a non-conflicting RefTarget proto.
///
/// kiki only produces the modern `Conflict` format (via jj-lib's
/// `SimpleOpStore`), so we only handle that variant. Returns `None`
/// for absent or conflicted targets.
fn resolved_commit_id(
    target: jj_lib::protos::simple_op_store::RefTarget,
) -> Option<Vec<u8>> {
    use jj_lib::protos::simple_op_store::ref_target::Value;

    match target.value? {
        Value::Conflict(c) if c.removes.is_empty() && c.adds.len() == 1 => {
            c.adds.into_iter().next()?.value
        }
        _ => None,
    }
}

/// Per-mount working-copy state.
///
/// Keyed by `working_copy_path` in `JujutsuService::mounts`. Holds everything
/// the daemon needs to answer working-copy and store RPCs for one mount.
///
/// `op_id` and `workspace_id` start empty after `Initialize`; the CLI fills
/// them in via `SetCheckoutState` once `KikiWorkingCopy::init` runs (M2).
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
    store: Arc<GitContentStore>,
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
    fs: Arc<dyn JjKikiFs>,
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
    /// SSH tunnel for `ssh://` remotes. Kept alive for the mount's
    /// lifetime — dropping it kills the SSH process and removes the
    /// forwarded socket. `None` for non-SSH remotes.
    #[allow(dead_code)] // dropped on Mount drop, never read directly
    _ssh_tunnel: Option<remote::tunnel::SshTunnel>,
    /// Last HEAD commit id exported via `export_head`. Used to detect
    /// external git HEAD changes (e.g. `git commit` from the mount).
    /// Empty until the first `WriteCommit` exports HEAD.
    last_exported_head: Vec<u8>,
    /// Last bookmarks exported via `export_bookmarks` in the `WriteView`
    /// handler. Used to detect external ref changes (e.g. `git fetch` or
    /// `git branch` from the mount). `None` until the first `WriteView`;
    /// `Some(vec![])` means "exported an empty set" (any new refs are
    /// external additions).
    last_exported_bookmarks: Option<Vec<(String, Vec<u8>)>>,
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
    /// Directory for SSH tunnel forwarded sockets. Lives in the runtime
    /// dir alongside daemon.sock/pid/log. `None` in test mode.
    tunnels_dir: Option<PathBuf>,
}

impl JujutsuService {
    /// Production constructor: with `Some(vfs_handle)`, `Initialize`
    /// validates mountpoints and attaches a real FUSE/NFS mount.
    /// `None` is the integration-test path (see `Config.disable_mount`):
    /// per-mount state is still tracked and store/WC RPCs work, but
    /// nothing is mounted at `working_copy_path`.
    ///
    /// `storage` decides where per-mount Stores live. Production passes
    /// `StorageConfig::OnDisk { root: <storage_dir> }`;
    /// tests that don't care about durability use
    /// `StorageConfig::InMemory`.
    ///
    /// Returns the bare `JujutsuService`. Call [`Self::rehydrate`] to
    /// re-attach mounts left behind by a previous daemon process, then
    /// [`Self::into_server`] to wrap it for tonic.
    pub fn new(vfs_handle: Option<VfsManagerHandle>, storage: StorageConfig) -> Self {
        let tunnels_dir = Some(store::paths::runtime_dir().join("tunnels"));
        JujutsuService {
            mounts: Arc::new(Mutex::new(HashMap::new())),
            vfs_handle,
            storage,
            tunnels_dir,
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
            ty::Id(store.empty_tree_id().as_bytes().try_into().expect("20-byte tree id"))
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
        //
        // For `ssh://` URLs, re-establish the tunnel (the previous
        // daemon's SSH child is dead).
        let (remote_store, ssh_tunnel) = if remote::parse_ssh_url(&meta.remote)
            .with_context(|| format!("parsing remote URL {:?}", meta.remote))?
            .is_some()
        {
            if let Some(tunnels_dir) = &self.tunnels_dir {
                let (store, tunnel) = remote::establish_ssh_remote(&meta.remote, tunnels_dir)
                    .await
                    .with_context(|| format!(
                        "re-establishing SSH tunnel for {:?}",
                        meta.remote
                    ))?;
                (Some(store), Some(tunnel))
            } else {
                // Test mode (no tunnels_dir) — ssh:// will fail here,
                // which is correct since tests can't establish SSH tunnels.
                let store = remote::parse(&meta.remote)
                    .with_context(|| format!("parsing remote URL {:?}", meta.remote))?;
                (store, None)
            }
        } else {
            let store = remote::parse(&meta.remote)
                .with_context(|| format!("parsing remote URL {:?}", meta.remote))?;
            (store, None)
        };
        // M10 §10.6: hand the remote into `KikiFs` too, so FUSE-side
        // reads on a `StoreMiss` fall through to the remote the same
        // way M9's RPC-layer reads already do.
        let scratch_dir = self.storage.scratch_dir_for(&meta.working_copy_path);
        let fs: Arc<dyn JjKikiFs> =
            Arc::new(KikiFs::new(store.clone(), root_tree_id, remote_store.clone(), scratch_dir));

        // On Linux (FUSE), the kernel tears down the mount when the
        // daemon's process exits — the path is usually a clean empty dir
        // by the time we rehydrate.
        //
        // On macOS (NFS), the kernel mount survives the daemon's death
        // and becomes a hung mount. The stale mount can manifest as any
        // of several validate_mountpoint errors (NotEmpty if the mount
        // is still functional, Io if the NFS server is dead and reads
        // return EIO, or AlreadyMounted if the dir happened to be
        // empty). Rather than pattern-matching on the error, check
        // is_mountpoint up front — if the path is still a mountpoint
        // and we're rehydrating (so we *know* it was ours), tear it
        // down before validating.
        let attachment = if let Some(vfs) = &self.vfs_handle {
            if is_mountpoint(Path::new(&meta.working_copy_path)).unwrap_or(false) {
                warn!(
                    path = %meta.working_copy_path,
                    "stale mount detected during rehydrate; force-unmounting"
                );
                force_unmount(&meta.working_copy_path).with_context(|| {
                    format!(
                        "force-unmounting stale mount at {} — \
                         try `umount -f {}` manually",
                        meta.working_copy_path, meta.working_copy_path,
                    )
                })?;
            }
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
                _ssh_tunnel: ssh_tunnel,
                last_exported_head: Vec::new(),
                last_exported_bookmarks: None,
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
    pub(crate) fn bare() -> Self {
        JujutsuService {
            mounts: Arc::new(Mutex::new(HashMap::new())),
            vfs_handle: None,
            storage: StorageConfig::InMemory,
            tunnels_dir: None,
        }
    }

    /// Test-only access to a mount's per-mount `JjKikiFs`. Used to drive
    /// VFS write ops directly from service-level tests without going
    /// through a real FUSE/NFS adapter — e.g. to seed a dirty file, then
    /// confirm the `Snapshot` RPC turns it into a real tree id.
    #[cfg(test)]
    pub(crate) async fn fs_for_test(&self, path: &str) -> Option<Arc<dyn JjKikiFs>> {
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
/// mount. Conservative for `Initialize` (fresh mounts) — a stale mount
/// from another process is a legitimate conflict. During rehydration,
/// callers handle `AlreadyMounted` by force-unmounting (we *know* the
/// mount was ours).
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
/// already mounted. We don't expect anyone to `jj kk init /` but the
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

/// Force-unmount a stale mountpoint left behind by a previous daemon
/// that crashed or was killed without clean shutdown.
///
/// On macOS, NFS mounts survive the daemon's death (the kernel NFS client
/// keeps trying to reach the dead NFS server). `umount -f` is the only
/// reliable way to tear them down.
///
/// On Linux, FUSE mounts normally disappear when the daemon process exits
/// (the kernel closes the `/dev/fuse` fd), but `fusermount3 -u` handles
/// the rare case where a mount lingers (e.g. lazy unmount, external FUSE
/// mount at the same path).
///
/// Only called during rehydration where we *know* the path was previously
/// managed by kiki — never during `Initialize` where an existing mount
/// might belong to something else.
fn force_unmount(path: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let output = std::process::Command::new("umount")
        .arg("-f")
        .arg(path)
        .output()
        .context("running umount -f")?;

    #[cfg(not(target_os = "macos"))]
    let output = std::process::Command::new("fusermount3")
        .arg("-uz") // lazy unmount: detach immediately even if busy
        .arg(path)
        .output()
        .context("running fusermount3 -uz")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "force unmount of {path} failed (exit {}): {}",
            output.status,
            stderr.trim()
        );
    }
    info!(path, "force-unmounted stale mountpoint");
    Ok(())
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

/// Minimal [`UserSettings`] for the daemon. Content writes through
/// `GitBackend` require a plausible user/operation identity.
fn default_user_settings() -> jj_lib::settings::UserSettings {
    let toml_str = r#"
        user.name = "kiki daemon"
        user.email = "daemon@localhost"
        operation.hostname = "localhost"
        operation.username = "daemon"
    "#;
    let mut config = jj_lib::config::StackedConfig::with_defaults();
    config.add_layer(
        jj_lib::config::ConfigLayer::parse(jj_lib::config::ConfigSource::User, toml_str)
            .unwrap(),
    );
    jj_lib::settings::UserSettings::from_config(config).unwrap()
}

/// Open a per-mount [`GitContentStore`] according to the daemon's
/// `StorageConfig`. On disk: git store at
/// `<root>/mounts/<hash(wc_path)>/git_store/`, redb at
/// `<root>/mounts/<hash(wc_path)>/store.redb`, reused if they already
/// exist (Layer B durability). In-memory: temp-dir git repo + in-memory
/// redb per call.
fn open_store_for(
    storage: &StorageConfig,
    working_copy_path: &str,
) -> anyhow::Result<GitContentStore> {
    let settings = default_user_settings();
    match (
        storage.git_store_path_for(working_copy_path),
        storage.redb_path_for(working_copy_path),
    ) {
        (Some(git_store_path), Some(redb_path)) => {
            if git_store_path.join("git").exists() {
                GitContentStore::load(&settings, &git_store_path, &redb_path)
            } else {
                GitContentStore::init(&settings, &git_store_path, &redb_path)
            }
        }
        #[cfg(test)]
        _ => Ok(GitContentStore::new_in_memory(&settings)),
        #[cfg(not(test))]
        _ => Err(anyhow!("in-memory storage not supported in production")),
    }
}

/// Look up a mount by `working_copy_path` and clone its store handle so
/// the lock can be released before doing real work. All store RPCs use
/// this — the lock is short, the RPC body is long.
async fn store_for(
    mounts: &Arc<Mutex<HashMap<String, Mount>>>,
    path: &str,
) -> Result<Arc<GitContentStore>, Status> {
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
) -> Result<(Arc<GitContentStore>, Option<Arc<dyn RemoteStore>>), Status> {
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
// so `vfs/kiki_fs.rs` can share the implementation. The wrapper here
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
        FetchError::LocalWrite { .. } | FetchError::Remote { .. } => {
            Status::internal(format!("{err:#}"))
        }
    }
}

#[allow(clippy::result_large_err)]
async fn fetch_file_through_remote(
    store: &GitContentStore,
    remote: Option<&dyn RemoteStore>,
    id: &[u8],
) -> Result<Vec<u8>, Status> {
    let remote = remote
        .ok_or_else(|| Status::not_found(format!("file {} not found", hex_bytes(id))))?;
    fetch::fetch_file(store, remote, id).await.map_err(fetch_status)
}

#[allow(clippy::result_large_err)]
async fn fetch_symlink_through_remote(
    store: &GitContentStore,
    remote: Option<&dyn RemoteStore>,
    id: &[u8],
) -> Result<String, Status> {
    let remote = remote
        .ok_or_else(|| Status::not_found(format!("symlink {} not found", hex_bytes(id))))?;
    fetch::fetch_symlink(store, remote, id)
        .await
        .map_err(fetch_status)
}

#[allow(clippy::result_large_err)]
async fn fetch_tree_through_remote(
    store: &GitContentStore,
    remote: Option<&dyn RemoteStore>,
    id: &[u8],
) -> Result<Vec<git_store::GitTreeEntry>, Status> {
    let remote = remote
        .ok_or_else(|| Status::not_found(format!("tree {} not found", hex_bytes(id))))?;
    fetch::fetch_tree(store, remote, id).await.map_err(fetch_status)
}

#[allow(clippy::result_large_err)]
async fn fetch_commit_through_remote(
    store: &GitContentStore,
    remote: Option<&dyn RemoteStore>,
    id: &[u8],
) -> Result<jj_lib::backend::Commit, Status> {
    let remote = remote
        .ok_or_else(|| Status::not_found(format!("commit {} not found", hex_bytes(id))))?;
    fetch::fetch_commit(store, remote, id)
        .await
        .map_err(fetch_status)
}

// ---- Layer C / M9 post-snapshot push ---------------------------------
//
// After `JjKikiFs::snapshot` produces the new root, walk every reachable
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
    store: &GitContentStore,
    remote: &dyn RemoteStore,
    root_tree_id: ty::Id,
) -> anyhow::Result<()> {
    let mut seen_trees: HashSet<Vec<u8>> = HashSet::new();
    let mut tree_stack: Vec<Vec<u8>> = vec![root_tree_id.0.to_vec()];

    while let Some(tree_id) = tree_stack.pop() {
        if !seen_trees.insert(tree_id.clone()) {
            continue;
        }
        // Push the tree blob itself.
        push_git_object_if_missing(store, remote, BlobKind::Tree, &tree_id).await?;

        let entries = match store.read_tree(&tree_id)? {
            Some(e) => e,
            None => {
                // The tree blob isn't in the local git ODB yet — this
                // happens when the tree was checked out from a remote
                // (check_out only fetches the root tree lazily; subtrees
                // stay as NodeRef::Tree references in the slab until the
                // kernel walks into them). Fetch + persist now so the
                // push walk can continue.
                fetch::fetch_tree(store, remote, &tree_id).await.map_err(|e| {
                    anyhow!("tree {} missing locally and remote fetch failed: {e}", hex_bytes(&tree_id))
                })?
            }
        };
        for entry in entries {
            match entry.kind {
                GitEntryKind::Tree => tree_stack.push(entry.id),
                GitEntryKind::File { .. } => {
                    push_git_object_if_missing(store, remote, BlobKind::Blob, &entry.id).await?;
                }
                GitEntryKind::Symlink => {
                    push_git_object_if_missing(store, remote, BlobKind::Blob, &entry.id).await?;
                }
                GitEntryKind::Submodule => {
                    // Submodules are not reachable from a
                    // `JjKikiFs`-produced snapshot tree. Skip.
                }
            }
        }
    }
    Ok(())
}

/// Cheap-existence-probe-then-push for one git object. Reads the raw
/// object bytes from the local git ODB and pushes to the remote.
async fn push_git_object_if_missing(
    store: &GitContentStore,
    remote: &dyn RemoteStore,
    kind: BlobKind,
    id: &[u8],
) -> anyhow::Result<()> {
    if remote.has_blob(kind, id).await? {
        return Ok(());
    }
    let (_git_kind, data) = store
        .read_git_object_bytes(id)?
        .ok_or_else(|| {
            anyhow!(
                "local store missing {} {} during push walk",
                kind.as_str(),
                hex_bytes(id)
            )
        })?;
    remote.put_blob(kind, id, Bytes::from(data)).await?;
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
        let root_tree_id = ty::Id(store.empty_tree_id().as_bytes().try_into().expect("20-byte tree id"));

        // M9: parse `remote` into an optional `RemoteStore`. Bad URL =
        // `invalid_argument` so the user gets a clean message rather
        // than a generic internal error or — worse — a silent drop into
        // "no remote configured". Empty string is still `Ok(None)`.
        //
        // For `ssh://` URLs, establish a persistent tunnel to the remote
        // daemon and connect via GrpcRemoteStore over the forwarded UDS.
        let (remote_store, ssh_tunnel) = if remote::parse_ssh_url(&req.remote)
            .map_err(|e| Status::invalid_argument(format!("remote: {e:#}")))?
            .is_some()
        {
            let tunnels_dir = self.tunnels_dir.as_deref().ok_or_else(|| {
                Status::internal("SSH tunnels not available in test mode")
            })?;
            let (store, tunnel) = remote::establish_ssh_remote(&req.remote, tunnels_dir)
                .await
                .map_err(|e| Status::internal(format!("SSH tunnel: {e:#}")))?;
            (Some(store), Some(tunnel))
        } else {
            let store = remote::parse(&req.remote)
                .map_err(|e| Status::invalid_argument(format!("remote: {e:#}")))?;
            (store, None)
        };

        // Build the per-mount FS up front so we can hand the same `Arc`
        // to both the VFS bind (for kernel I/O) and the `Mount` (for
        // RPCs like `CheckOut` that mutate mount-side state). Even on
        // the disable_mount test path the `Mount` keeps an `fs` so
        // mutating RPCs work end-to-end without the kernel.
        //
        // M10 §10.6: the remote is threaded into `KikiFs` too — kernel
        // reads on local-store miss fall through the same way the M9
        // RPC layer already does at `service.rs`.
        let scratch_dir = self.storage.scratch_dir_for(&req.path);
        let fs: Arc<dyn JjKikiFs> =
            Arc::new(KikiFs::new(store.clone(), root_tree_id, remote_store.clone(), scratch_dir));

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
            _ssh_tunnel: ssh_tunnel,
            last_exported_head: Vec::new(),
            last_exported_bookmarks: None,
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
        // Sort by path so output is deterministic — `kk status` is
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
            tree_id: store.empty_tree_id().as_bytes().to_vec(),
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn write_file(
        &self,
        request: Request<WriteFileReq>,
    ) -> Result<Response<FileId>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let id = store
            .write_file(&req.data)
            .map_err(store_status("write_file"))?;
        // Write-through: push the raw git blob to the remote
        // synchronously (M9 §13.4). On failure, the local write has
        // already happened — surface the error but don't roll back;
        // idempotent puts + the next snapshot's walk cover transient
        // remote failures.
        if let Some(remote) = remote {
            let (_kind, obj_bytes) = store
                .read_git_object_bytes(&id)
                .map_err(store_status("read git blob for file push"))?
                .ok_or_else(|| Status::internal("just-written file not found"))?;
            remote
                .put_blob(BlobKind::Blob, &id, Bytes::from(obj_bytes))
                .await
                .map_err(remote_status("remote put_blob (file)"))?;
        }
        Ok(Response::new(FileId { file_id: id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_file(
        &self,
        request: Request<ReadFileReq>,
    ) -> Result<Response<File>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let id: ty::Id = FileId { file_id: req.file_id }
            .try_into()
            .map_err(decode_status("file id"))?;
        let content = match store.read_file(&id.0).map_err(store_status("read_file"))? {
            Some(c) => c,
            None => fetch_file_through_remote(&store, remote.as_deref(), &id.0).await?,
        };
        Ok(Response::new(proto::jj_interface::File { data: content }))
    }

    #[tracing::instrument(skip(self))]
    async fn write_symlink(
        &self,
        request: Request<WriteSymlinkReq>,
    ) -> Result<Response<SymlinkId>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let id = store
            .write_symlink(&req.target)
            .map_err(store_status("write_symlink"))?;
        if let Some(remote) = remote {
            let (_kind, obj_bytes) = store
                .read_git_object_bytes(&id)
                .map_err(store_status("read git blob for symlink push"))?
                .ok_or_else(|| Status::internal("just-written symlink not found"))?;
            remote
                .put_blob(BlobKind::Blob, &id, Bytes::from(obj_bytes))
                .await
                .map_err(remote_status("remote put_blob (symlink)"))?;
        }
        Ok(Response::new(SymlinkId { symlink_id: id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_symlink(
        &self,
        request: Request<ReadSymlinkReq>,
    ) -> Result<Response<Symlink>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let id: ty::Id = SymlinkId { symlink_id: req.symlink_id }
            .try_into()
            .map_err(decode_status("symlink id"))?;
        let target = match store
            .read_symlink(&id.0)
            .map_err(store_status("read_symlink"))?
        {
            Some(t) => t,
            None => fetch_symlink_through_remote(&store, remote.as_deref(), &id.0).await?,
        };
        Ok(Response::new(proto::jj_interface::Symlink { target }))
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
        let entries = git_store::tree_from_proto(tree_proto).map_err(decode_status("tree"))?;
        let id = store
            .write_tree(&entries)
            .map_err(store_status("write_tree"))?;
        if let Some(remote) = remote {
            let (_kind, obj_bytes) = store
                .read_git_object_bytes(&id)
                .map_err(store_status("read git blob for tree push"))?
                .ok_or_else(|| Status::internal("just-written tree not found"))?;
            remote
                .put_blob(BlobKind::Tree, &id, Bytes::from(obj_bytes))
                .await
                .map_err(remote_status("remote put_blob (tree)"))?;
        }
        Ok(Response::new(TreeId { tree_id: id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_tree(
        &self,
        request: Request<ReadTreeReq>,
    ) -> Result<Response<Tree>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let id: ty::Id = TreeId { tree_id: req.tree_id }
            .try_into()
            .map_err(decode_status("tree id"))?;
        let entries = match store.read_tree(&id.0).map_err(store_status("read_tree"))? {
            Some(e) => e,
            None => fetch_tree_through_remote(&store, remote.as_deref(), &id.0).await?,
        };
        Ok(Response::new(git_store::tree_to_proto(&entries)))
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
        let commit = git_store::commit_from_proto(commit_proto).map_err(decode_status("commit"))?;
        let (id, _stored) = store
            .write_commit(commit)
            .map_err(store_status("write_commit"))?;
        if let Some(remote) = remote {
            let (_kind, obj_bytes) = store
                .read_git_object_bytes(&id)
                .map_err(store_status("read git blob for commit push"))?
                .ok_or_else(|| Status::internal("just-written commit not found"))?;
            remote
                .put_blob(BlobKind::Commit, &id, Bytes::from(obj_bytes))
                .await
                .map_err(remote_status("remote put_blob (commit)"))?;
            // Push extras (change-id + predecessors)
            if let Some(extras_bytes) = store
                .read_extras(&id)
                .map_err(store_status("read extras for commit push"))?
            {
                remote
                    .put_blob(BlobKind::Extra, &id, Bytes::from(extras_bytes))
                    .await
                    .map_err(remote_status("remote put_blob (extra)"))?;
            }
        }

        // Colocation: update HEAD in the bare git repo so stock `git log`
        // shows the latest state. Best-effort — a failure here doesn't
        // invalidate the commit itself.
        let git_path = store.git_repo_path().to_path_buf();
        if let Err(e) = crate::git_ops::export_head(&git_path, &id) {
            tracing::warn!("export_head after WriteCommit: {e:#}");
        } else {
            // Track what we exported so Snapshot can detect external changes.
            let mut mounts = self.mounts.lock().await;
            if let Some(mount) = mounts.get_mut(&req.working_copy_path) {
                mount.last_exported_head = id.clone();
            }
        }

        Ok(Response::new(CommitId { commit_id: id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_commit(
        &self,
        request: Request<ReadCommitReq>,
    ) -> Result<Response<Commit>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let id: ty::Id = CommitId { commit_id: req.commit_id }
            .try_into()
            .map_err(decode_status("commit id"))?;
        let commit = match store
            .read_commit(&id.0)
            .map_err(store_status("read_commit"))?
        {
            Some(c) => c,
            None => fetch_commit_through_remote(&store, remote.as_deref(), &id.0).await?,
        };
        Ok(Response::new(git_store::commit_to_proto(&commit)))
    }

    // ---- M10.6: op-store RPCs ----------------------------------------
    //
    // The daemon stores and forwards opaque bytes; serialization and
    // content-hashing happen on the CLI side (KikiOpStore). Write-
    // through pushes to the remote inline; read-through falls back to
    // the remote on local miss (same shape as the blob handlers above).

    #[tracing::instrument(skip(self))]
    async fn write_view(
        &self,
        request: Request<WriteViewReq>,
    ) -> Result<Response<WriteViewReply>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        validate_op_store_blob_id(OpStoreBlobKind::View, &req.view_id, &req.data)
            .await
            .map_err(decode_status("view blob"))?;

        // Decode bookmarks before data is potentially moved to remote.
        let bookmarks = bookmarks_from_view_bytes(&req.data);

        store
            .write_view_bytes(&req.view_id, &req.data)
            .map_err(store_status("write_view"))?;
        if let Some(remote) = remote {
            remote
                .put_blob(BlobKind::View, &req.view_id, Bytes::from(req.data))
                .await
                .map_err(remote_status("remote put_blob (view)"))?;
        }

        // Best-effort: export bookmarks to git refs and clean up stale
        // refs so stock git tools see branches (e.g. `git log --all`,
        // `git push`).
        let git_path = store.git_repo_path().to_path_buf();
        if !bookmarks.is_empty()
            && let Err(e) = crate::git_ops::export_bookmarks(&git_path, &bookmarks)
        {
            warn!("export_bookmarks after WriteView: {e:#}");
        }
        // Delete refs/heads/* for bookmarks removed from the View.
        {
            let active: HashSet<&str> =
                bookmarks.iter().map(|(n, _)| n.as_str()).collect();
            if let Err(e) = crate::git_ops::cleanup_stale_refs(&git_path, &active) {
                warn!("cleanup_stale_refs after WriteView: {e:#}");
            }
        }

        // Track what we exported so git_detect_head_change can detect
        // external ref changes (e.g. `git fetch` from the mount).
        {
            let mut mounts = self.mounts.lock().await;
            if let Some(mount) = mounts.get_mut(&req.working_copy_path) {
                mount.last_exported_bookmarks = Some(bookmarks);
            }
        }

        Ok(Response::new(WriteViewReply {}))
    }

    #[tracing::instrument(skip(self))]
    async fn read_view(
        &self,
        request: Request<ReadViewReq>,
    ) -> Result<Response<ReadViewReply>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        if req.view_id.len() != 64 {
            return Err(Status::invalid_argument(format!(
                "view_id must be 64 bytes, got {}",
                req.view_id.len()
            )));
        }
        // Local hit.
        if let Some(bytes) = store
            .get_view_bytes(&req.view_id)
            .map_err(store_status("get_view"))?
        {
            return Ok(Response::new(ReadViewReply {
                found: true,
                data: bytes.to_vec(),
            }));
        }
        // Read-through from remote.
        if let Some(remote) = remote
            && let Some(bytes) = remote
                .get_blob(BlobKind::View, &req.view_id)
                .await
                .map_err(remote_status("remote get_blob (view)"))?
        {
            validate_op_store_blob_id(OpStoreBlobKind::View, &req.view_id, bytes.as_ref())
                .await
                .map_err(|e| Status::data_loss(format!("remote view blob: {e:#}")))?;
            // Populate local cache.
            store
                .write_view_bytes(&req.view_id, &bytes)
                .map_err(store_status("cache write_view"))?;
            return Ok(Response::new(ReadViewReply {
                found: true,
                data: bytes.to_vec(),
            }));
        }
        Ok(Response::new(ReadViewReply {
            found: false,
            data: Vec::new(),
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn write_operation(
        &self,
        request: Request<WriteOperationReq>,
    ) -> Result<Response<WriteOperationReply>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        validate_op_store_blob_id(OpStoreBlobKind::Operation, &req.operation_id, &req.data)
            .await
            .map_err(decode_status("operation blob"))?;
        store
            .write_operation_bytes(&req.operation_id, &req.data)
            .map_err(store_status("write_operation"))?;
        if let Some(remote) = remote {
            remote
                .put_blob(
                    BlobKind::Operation,
                    &req.operation_id,
                    Bytes::from(req.data),
                )
                .await
                .map_err(remote_status("remote put_blob (operation)"))?;
        }
        Ok(Response::new(WriteOperationReply {}))
    }

    #[tracing::instrument(skip(self))]
    async fn read_operation(
        &self,
        request: Request<ReadOperationReq>,
    ) -> Result<Response<ReadOperationReply>, Status> {
        let req = request.into_inner();
        let (store, remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        if req.operation_id.len() != 64 {
            return Err(Status::invalid_argument(format!(
                "operation_id must be 64 bytes, got {}",
                req.operation_id.len()
            )));
        }
        // Local hit.
        if let Some(bytes) = store
            .get_operation_bytes(&req.operation_id)
            .map_err(store_status("get_operation"))?
        {
            return Ok(Response::new(ReadOperationReply {
                found: true,
                data: bytes.to_vec(),
            }));
        }
        // Read-through from remote.
        if let Some(remote) = remote
            && let Some(bytes) = remote
                .get_blob(BlobKind::Operation, &req.operation_id)
                .await
                .map_err(remote_status("remote get_blob (operation)"))?
        {
            validate_op_store_blob_id(
                OpStoreBlobKind::Operation,
                &req.operation_id,
                bytes.as_ref(),
            )
            .await
            .map_err(|e| Status::data_loss(format!("remote operation blob: {e:#}")))?;
            store
                .write_operation_bytes(&req.operation_id, &bytes)
                .map_err(store_status("cache write_operation"))?;
            return Ok(Response::new(ReadOperationReply {
                found: true,
                data: bytes.to_vec(),
            }));
        }
        Ok(Response::new(ReadOperationReply {
            found: false,
            data: Vec::new(),
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn resolve_operation_id_prefix(
        &self,
        request: Request<ResolveOperationIdPrefixReq>,
    ) -> Result<Response<ResolveOperationIdPrefixReply>, Status> {
        let req = request.into_inner();
        let (store, _remote) = mount_handles(&self.mounts, &req.working_copy_path).await?;
        let result = store
            .operation_ids_matching_prefix(&req.hex_prefix)
            .map_err(store_status("operation_ids_matching_prefix"))?;
        Ok(Response::new(match result {
            OpPrefixResult::None => ResolveOperationIdPrefixReply {
                resolution: 0,
                full_id: Vec::new(),
            },
            OpPrefixResult::Single(id) => ResolveOperationIdPrefixReply {
                resolution: 1,
                full_id: id,
            },
            OpPrefixResult::Ambiguous => ResolveOperationIdPrefixReply {
                resolution: 2,
                full_id: Vec::new(),
            },
        }))
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
        // `KikiWorkingCopy::init`).
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
        let new_metadata = MountMetadata {
            working_copy_path: mount.working_copy_path.clone(),
            remote: mount.remote.clone(),
            op_id: checkout.op_id.clone(),
            workspace_id: checkout.workspace_id.clone(),
            root_tree_id: mount.root_tree_id.clone(),
        };
        persist_metadata_snapshot(&mount.meta_path, &new_metadata)
            .map_err(metadata_status("persist checkout state"))?;
        mount.op_id = checkout.op_id;
        mount.workspace_id = checkout.workspace_id;
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
        let (fs, store, remote, last_exported_head) = {
            let mounts = self.mounts.lock().await;
            let mount = mounts.get(&req.working_copy_path).ok_or_else(|| {
                Status::not_found(format!("no mount at {}", req.working_copy_path))
            })?;
            (
                mount.fs.clone(),
                mount.store.clone(),
                mount.remote_store.clone(),
                mount.last_exported_head.clone(),
            )
        };

        let new_root = fs.snapshot().await.map_err(|e| {
            Status::internal(format!(
                "snapshot failed for {}: {e}",
                req.working_copy_path
            ))
        })?;
        info!(path = %req.working_copy_path, tree_id = %hex_bytes(&new_root.0), "Snapshot");

        // M9 §13.2: blobs written through `JjKikiFs::snapshot_node`
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
            let new_root_tree_id = new_root.0.to_vec();
            let new_metadata = MountMetadata {
                working_copy_path: mount.working_copy_path.clone(),
                remote: mount.remote.clone(),
                op_id: mount.op_id.clone(),
                workspace_id: mount.workspace_id.clone(),
                root_tree_id: new_root_tree_id.clone(),
            };
            persist_metadata_snapshot(&mount.meta_path, &new_metadata)
                .map_err(metadata_status("persist snapshot metadata"))?;
            mount.root_tree_id = new_root_tree_id;
        } else {
            // Mount disappeared between the two locks; surface as
            // not_found rather than internal-error so a transient race
            // (no explicit Unmount today, but future-proof) is debuggable.
            return Err(Status::not_found(format!(
                "mount at {} disappeared during snapshot",
                req.working_copy_path
            )));
        }

        // Detect external git HEAD changes (e.g. `git commit` from the
        // mount). If HEAD differs from what we last exported, surface the
        // new commit id so the CLI can import it.
        let external_git_head = if !last_exported_head.is_empty() {
            let git_path = store.git_repo_path().to_path_buf();
            match crate::git_ops::read_head(&git_path) {
                Ok(Some(current_head)) if current_head != last_exported_head => {
                    info!(
                        path = %req.working_copy_path,
                        new_head = %hex_bytes(&current_head),
                        "detected external git HEAD change"
                    );
                    current_head
                }
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };

        Ok(Response::new(SnapshotReply {
            tree_id: new_root.0.to_vec(),
            external_git_head,
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
        info!(path = %req.working_copy_path, tree_id = %hex_bytes(&new_tree_id.0), "CheckOut");

        // Clone the per-mount fs + store handles out from under the lock
        // so the (potentially I/O-heavy) `JjKikiFs::check_out` call
        // doesn't hold it. Mirrors how `store_for` works.
        let (fs, store) = {
            let mounts = self.mounts.lock().await;
            let mount = mounts.get(&req.working_copy_path).ok_or_else(|| {
                Status::not_found(format!("no mount at {}", req.working_copy_path))
            })?;
            (mount.fs.clone(), mount.store.clone())
        };

        // The VFS swap validates the tree exists in the store; map the
        // miss onto failed_precondition so the caller gets a crisp
        // "write the tree first" signal rather than internal-error noise.
        fs.check_out(new_tree_id).await.map_err(|e| match e {
            FsError::StoreMiss => Status::failed_precondition(format!(
                "tree {} not in store; call WriteTree first",
                hex_bytes(&new_tree_id.0)
            )),
            other => Status::internal(format!("check_out failed: {other}")),
        })?;

        // Stamp the new root tree id on the Mount under the lock. We do
        // this *after* the swap succeeds: if the swap fails the mount's
        // declared `root_tree_id` should still match what the kernel
        // sees through `fs`.
        let mut mounts = self.mounts.lock().await;
        if let Some(mount) = mounts.get_mut(&req.working_copy_path) {
            let new_root_tree_id = req.new_tree_id;
            let new_metadata = MountMetadata {
                working_copy_path: mount.working_copy_path.clone(),
                remote: mount.remote.clone(),
                op_id: mount.op_id.clone(),
                workspace_id: mount.workspace_id.clone(),
                root_tree_id: new_root_tree_id.clone(),
            };
            persist_metadata_snapshot(&mount.meta_path, &new_metadata)
                .map_err(metadata_status("persist checkout metadata"))?;
            mount.root_tree_id = new_root_tree_id;
        } else {
            // Mount disappeared between the two locks. Extremely
            // unlikely (no Unmount RPC exists yet) but we should not
            // pretend success.
            return Err(Status::not_found(format!(
                "mount at {} disappeared during check_out",
                req.working_copy_path
            )));
        }

        // Best-effort: rebuild the git index to match the checked-out
        // tree so `git status`/`git diff` from the mount show the same
        // changes as `jj diff`.
        let git_path = store.git_repo_path().to_path_buf();
        if let Err(e) = crate::git_ops::reset_index(&git_path, &new_tree_id.0) {
            warn!("reset_index after CheckOut: {e:#}");
        }

        Ok(Response::new(CheckOutReply {}))
    }

    // ---- Git remote operations ----------------------------------------

    #[tracing::instrument(skip(self))]
    async fn git_remote_add(
        &self,
        request: Request<GitRemoteAddReq>,
    ) -> Result<Response<GitRemoteAddReply>, Status> {
        let req = request.into_inner();
        let store = store_for(&self.mounts, &req.working_copy_path).await?;
        let git_path = store.git_repo_path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            crate::git_ops::remote_add(&git_path, &req.name, &req.url)
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking: {e}")))?
        .map_err(|e| Status::internal(format!("git remote add: {e:#}")))?;
        Ok(Response::new(GitRemoteAddReply {}))
    }

    #[tracing::instrument(skip(self))]
    async fn git_remote_list(
        &self,
        request: Request<GitRemoteListReq>,
    ) -> Result<Response<GitRemoteListReply>, Status> {
        let req = request.into_inner();
        let store = store_for(&self.mounts, &req.working_copy_path).await?;
        let git_path = store.git_repo_path().to_path_buf();
        let remotes = tokio::task::spawn_blocking(move || {
            crate::git_ops::remote_list(&git_path)
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking: {e}")))?
        .map_err(|e| Status::internal(format!("git remote list: {e:#}")))?;
        Ok(Response::new(GitRemoteListReply {
            remotes: remotes
                .into_iter()
                .map(|(name, url)| git_remote_list_reply::Remote { name, url })
                .collect(),
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn git_push(
        &self,
        request: Request<GitPushReq>,
    ) -> Result<Response<GitPushReply>, Status> {
        let req = request.into_inner();
        let store = store_for(&self.mounts, &req.working_copy_path).await?;
        let git_path = store.git_repo_path().to_path_buf();
        let bookmarks: Vec<(String, Vec<u8>)> = req
            .bookmarks
            .into_iter()
            .map(|b| (b.name, b.commit_id))
            .collect();
        let remote = req.remote;
        tokio::task::spawn_blocking(move || {
            crate::git_ops::push(&git_path, &remote, &bookmarks)
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking: {e}")))?
        .map_err(|e| Status::internal(format!("git push: {e:#}")))?;
        Ok(Response::new(GitPushReply {}))
    }

    #[tracing::instrument(skip(self))]
    async fn git_fetch(
        &self,
        request: Request<GitFetchReq>,
    ) -> Result<Response<GitFetchReply>, Status> {
        let req = request.into_inner();
        let store = store_for(&self.mounts, &req.working_copy_path).await?;
        let git_path = store.git_repo_path().to_path_buf();
        let remote = req.remote;
        let bookmarks = tokio::task::spawn_blocking(move || {
            crate::git_ops::fetch(&git_path, &remote)
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking: {e}")))?
        .map_err(|e| Status::internal(format!("git fetch: {e:#}")))?;
        Ok(Response::new(GitFetchReply {
            bookmarks: bookmarks
                .into_iter()
                .map(|(name, commit_id)| GitFetchedBookmark { name, commit_id })
                .collect(),
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn git_detect_head_change(
        &self,
        request: Request<GitDetectHeadChangeReq>,
    ) -> Result<Response<GitDetectHeadChangeReply>, Status> {
        let req = request.into_inner();
        let (store, last_exported_head, last_exported_bookmarks) = {
            let mounts = self.mounts.lock().await;
            let mount = mounts.get(&req.working_copy_path).ok_or_else(|| {
                Status::not_found(format!("no mount at {}", req.working_copy_path))
            })?;
            (
                mount.store.clone(),
                mount.last_exported_head.clone(),
                mount.last_exported_bookmarks.clone(),
            )
        };

        let git_path = store.git_repo_path().to_path_buf();

        let new_head = if !last_exported_head.is_empty() {
            match crate::git_ops::read_head(&git_path) {
                Ok(Some(current_head)) if current_head != last_exported_head => current_head,
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };

        // Detect external bookmark changes: compare current refs/heads/*
        // against what we last exported in WriteView. `None` means no
        // WriteView has happened yet — skip detection.
        let bookmark_changes = if let Some(ref exported_bookmarks) = last_exported_bookmarks {
            let exported: HashMap<&str, &[u8]> = exported_bookmarks
                .iter()
                .map(|(n, id)| (n.as_str(), id.as_slice()))
                .collect();
            let current_refs = crate::git_ops::read_local_refs(&git_path)
                .unwrap_or_default();
            let current: HashMap<&str, &[u8]> = current_refs
                .iter()
                .map(|(n, id)| (n.as_str(), id.as_slice()))
                .collect();

            let mut changes = Vec::new();
            // Added or changed bookmarks.
            for (name, cur_id) in &current {
                match exported.get(name) {
                    Some(exp_id) if *exp_id == *cur_id => {} // unchanged
                    _ => {
                        changes.push(GitBookmarkChange {
                            name: name.to_string(),
                            commit_id: cur_id.to_vec(),
                        });
                    }
                }
            }
            // Deleted bookmarks.
            for name in exported.keys() {
                if !current.contains_key(name) {
                    changes.push(GitBookmarkChange {
                        name: name.to_string(),
                        commit_id: Vec::new(), // empty = deleted
                    });
                }
            }
            changes
        } else {
            Vec::new()
        };

        Ok(Response::new(GitDetectHeadChangeReply {
            new_head_commit_id: new_head,
            bookmark_changes,
        }))
    }

    // ── M12 managed-workspace stubs (§12.8) ─────────────────────────
    //
    // Proto RPCs landed; real implementations follow in the service-
    // changes commit (§12.14 step 6). These return Unimplemented so
    // the build stays green while the proto is wired in.

    async fn clone(
        &self,
        _request: Request<CloneReq>,
    ) -> Result<Response<CloneReply>, Status> {
        Err(Status::unimplemented("Clone not yet implemented (M12)"))
    }

    async fn repo_list(
        &self,
        _request: Request<RepoListReq>,
    ) -> Result<Response<RepoListReply>, Status> {
        Err(Status::unimplemented("RepoList not yet implemented (M12)"))
    }

    async fn workspace_create(
        &self,
        _request: Request<WorkspaceCreateReq>,
    ) -> Result<Response<WorkspaceCreateReply>, Status> {
        Err(Status::unimplemented(
            "WorkspaceCreate not yet implemented (M12)",
        ))
    }

    async fn workspace_finalize(
        &self,
        _request: Request<WorkspaceFinalizeReq>,
    ) -> Result<Response<WorkspaceFinalizeReply>, Status> {
        Err(Status::unimplemented(
            "WorkspaceFinalize not yet implemented (M12)",
        ))
    }

    async fn workspace_list(
        &self,
        _request: Request<WorkspaceListReq>,
    ) -> Result<Response<WorkspaceListReply>, Status> {
        Err(Status::unimplemented(
            "WorkspaceList not yet implemented (M12)",
        ))
    }

    async fn workspace_delete(
        &self,
        _request: Request<WorkspaceDeleteReq>,
    ) -> Result<Response<WorkspaceDeleteReply>, Status> {
        Err(Status::unimplemented(
            "WorkspaceDelete not yet implemented (M12)",
        ))
    }
}

#[cfg(test)]
mod tests {
    const COMMIT_ID_LENGTH: usize = 20;

    use assert_matches::assert_matches;
    use proto::jj_interface::jujutsu_interface_server::JujutsuInterface;
    use proptest::prelude::*;

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

    async fn make_view_blob() -> (Vec<u8>, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let store = SimpleOpStore::init(dir.path(), op_store_root_data()).unwrap();
        let view = jj_lib::op_store::View::make_root(JjCommitId::new(vec![1]));
        let view_id = store.write_view(&view).await.unwrap();
        let bytes = std::fs::read(dir.path().join("views").join(view_id.hex())).unwrap();
        (view_id.as_bytes().to_vec(), bytes)
    }

    async fn make_operation_blob() -> (Vec<u8>, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let store = SimpleOpStore::init(dir.path(), op_store_root_data()).unwrap();
        let view = jj_lib::op_store::View::make_root(JjCommitId::new(vec![1]));
        let view_id = store.write_view(&view).await.unwrap();
        let mut operation = jj_lib::op_store::Operation::make_root(view_id);
        operation.parents = vec![OperationId::from_bytes(&[0; 64])];
        operation.metadata.description = "non-root".into();
        let op_id = store.write_operation(&operation).await.unwrap();
        let bytes = std::fs::read(dir.path().join("operations").join(op_id.hex())).unwrap();
        (op_id.as_bytes().to_vec(), bytes)
    }

    proptest! {
        #[test]
        fn wrong_view_id_never_validates(idx in 0usize..64, delta in any::<u8>()) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let (mut id, bytes) = make_view_blob().await;
                let tweak = if delta == 0 { 1 } else { delta };
                id[idx] ^= tweak;
                prop_assert!(
                    validate_op_store_blob_id(OpStoreBlobKind::View, &id, &bytes)
                        .await
                        .is_err()
                );
                Ok(())
            })?;
        }

        #[test]
        fn wrong_operation_id_never_validates(idx in 0usize..64, delta in any::<u8>()) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let (mut id, bytes) = make_operation_blob().await;
                let tweak = if delta == 0 { 1 } else { delta };
                id[idx] ^= tweak;
                prop_assert!(
                    validate_op_store_blob_id(OpStoreBlobKind::Operation, &id, &bytes)
                        .await
                        .is_err()
                );
                Ok(())
            })?;
        }
    }

    #[tokio::test]
    async fn write_commit_parents() {
        let svc = JujutsuService::bare();
        let path = "/tmp/repo".to_string();
        init_mount(&svc, &path).await;

        // Get the empty tree id so we can supply a valid root_tree.
        let empty = svc
            .get_empty_tree_id(Request::new(GetEmptyTreeIdReq {
                working_copy_path: path.clone(),
            }))
            .await
            .unwrap()
            .into_inner();

        // Helper: build a Commit proto with the given parents/description
        // and a valid empty root_tree.
        let make_commit = |parents: Vec<Vec<u8>>, desc: &str| -> Commit {
            Commit {
                parents,
                root_tree: vec![empty.tree_id.clone()],
                description: desc.into(),
                ..Default::default()
            }
        };

        // No parents
        assert_matches!(
            svc.write_commit(Request::new(WriteCommitReq {
                working_copy_path: path.clone(),
                commit: Some(Commit {
                    parents: vec![],
                    ..Default::default()
                }),
            }))
            .await,
            Err(status) if status.message().contains("no parents")
        );

        // Only root commit as parent (use 20-byte root commit id)
        let root_parent = vec![0; COMMIT_ID_LENGTH];
        let first_id = svc
            .write_commit(Request::new(WriteCommitReq {
                working_copy_path: path.clone(),
                commit: Some(make_commit(vec![root_parent.clone()], "first")),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(first_id.commit_id.len(), COMMIT_ID_LENGTH);
        let first_commit = svc
            .read_commit(Request::new(ReadCommitReq {
                working_copy_path: path.clone(),
                commit_id: first_id.commit_id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        // GitBackend assigns a change_id on write, so we check key
        // fields rather than full proto equality.
        assert_eq!(first_commit.parents, vec![root_parent.clone()]);
        assert_eq!(first_commit.description, "first");

        // Only non-root commit as parent
        let second_id = svc
            .write_commit(Request::new(WriteCommitReq {
                working_copy_path: path.clone(),
                commit: Some(make_commit(
                    vec![first_id.commit_id.clone()],
                    "second",
                )),
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
        assert_eq!(second_commit.parents, vec![first_id.commit_id.clone()]);
        assert_eq!(second_commit.description, "second");

        // Merge commit
        let merge_id = svc
            .write_commit(Request::new(WriteCommitReq {
                working_copy_path: path.clone(),
                commit: Some(make_commit(
                    vec![first_id.commit_id.clone(), second_id.commit_id.clone()],
                    "merge",
                )),
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
        assert_eq!(
            merge_commit.parents,
            vec![first_id.commit_id.clone(), second_id.commit_id.clone()]
        );
        assert_eq!(merge_commit.description, "merge");

        // Note: GitBackend does not support merge commits with the root
        // commit as a parent. That case (previously tested here) is now
        // a backend-level constraint.
    }

    /// Walk through the lifecycle exercised by `jj kk init` followed by
    /// `KikiWorkingCopy::init` (M2): Initialize → SetCheckoutState →
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
        // this is what lets `KikiWorkingCopy::tree()` succeed on a fresh
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
        // M6 made Snapshot drive `JjKikiFs::snapshot`, but that walk is a
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
    /// contract `LockedKikiWorkingCopy::check_out` depends on.
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
        let bogus = vec![0xee; 20];
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
                new_tree_id: vec![0; 20],
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
    /// `JjKikiFs`, then call `Snapshot` and confirm the daemon both
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
                validate_mountpoint("/definitely/does/not/exist/jjkiki").expect_err("missing");
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
        let mount_path = "/tmp/kiki-rehydrate-test";
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

    #[tokio::test]
    async fn set_checkout_state_returns_error_when_metadata_persist_fails() {
        let storage_dir = tempfile::tempdir().expect("storage tempdir");
        let svc = JujutsuService::new(
            None,
            StorageConfig::on_disk(storage_dir.path().to_owned()),
        );
        let path = "/tmp/persist-fail".to_string();
        init_mount(&svc, &path).await;

        let poison = storage_dir.path().join("not-a-dir");
        std::fs::write(&poison, b"x").unwrap();
        let mut mounts = svc.mounts.lock().await;
        let mount = mounts.get_mut(&path).unwrap();
        mount.meta_path = Some(poison.join("mount.toml"));
        drop(mounts);

        let err = svc
            .set_checkout_state(Request::new(SetCheckoutStateReq {
                working_copy_path: path.clone(),
                checkout_state: Some(CheckoutState {
                    op_id: vec![1, 2, 3],
                    workspace_id: b"default".to_vec(),
                }),
            }))
            .await
            .expect_err("persist failure must fail the RPC");
        assert_eq!(err.code(), tonic::Code::Internal);

        let err = svc
            .get_checkout_state(Request::new(GetCheckoutStateReq {
                working_copy_path: path,
            }))
            .await
            .expect_err("in-memory state must stay unchanged on persist failure");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
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

    /// `WriteFile` with a `dir://` remote pushes the raw git blob
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

        // Hex-encode the 20-byte id the way the dir backend does it.
        let hex_id = hex_bytes(&written.file_id);
        let blob_path = remote_dir.path().join("blob").join(&hex_id);
        let bytes = std::fs::read(&blob_path).expect("blob landed on remote");
        // The remote now holds raw git blob bytes (just the file content).
        assert_eq!(bytes, b"hello-remote");
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
        let cached = store_b.read_file(&id.0).expect("read_file (after read-through)");
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
                file_id: vec![0xff; 20],
            }))
            .await
            .expect_err("expected not_found");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    /// Snapshot pushes every reachable blob from the new root tree to
    /// the remote (M9 §13.2 post-snapshot walk). Drives a write through
    /// the per-mount `JjKikiFs`, calls `Snapshot`, and verifies the file
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
        assert_eq!(snap.tree_id.len(), 20);

        // The new root tree blob is on the remote.
        let tree_hex = hex_bytes(&snap.tree_id);
        let tree_blob = remote_dir.path().join("tree").join(&tree_hex);
        assert!(tree_blob.exists(), "tree blob {tree_hex} should be on remote");

        // The file blob is also on the remote (the walk recursed into
        // the tree's File entry).
        let file_dir = remote_dir.path().join("blob");
        let entries: Vec<_> = std::fs::read_dir(&file_dir)
            .expect("file kind dir exists")
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with(".tmp"))
            .collect();
        assert_eq!(entries.len(), 1, "expected one file blob on remote");
        // The remote now holds raw git blob bytes (just the file content).
        let file_bytes =
            std::fs::read(entries[0].path()).expect("read remote file blob");
        assert_eq!(file_bytes, b"snapshot-content");
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
    /// `read` through B's `JjKikiFs` and get the right content via
    /// the lazy remote fetch in `vfs/kiki_fs.rs`.
    ///
    /// This is the M10 analog of M9's
    /// `read_file_falls_back_to_remote_on_local_miss`, but exercising
    /// the FS layer rather than the RPC layer. Pre-M10 this test
    /// would fail at `lookup` / `getattr` / `read` with EIO because
    /// `vfs/kiki_fs.rs::read_*` mapped `StoreMiss` straight to that;
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
        // to check out A's tree id; B's `JjKikiFs::check_out` calls
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
    /// against `JjKikiFs::check_out` still surfaces as the expected
    /// failed_precondition (tree not in store), preserving pre-M10
    /// behavior for mounts without a Layer-C remote.
    #[tokio::test]
    async fn fuse_layer_store_miss_no_remote_is_failed_precondition() {
        let svc = JujutsuService::bare();
        init_mount(&svc, "/tmp/repo").await;
        // A fabricated tree id that nothing wrote — and no remote
        // to fetch it from.
        let bogus = vec![0xaa; 20];
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
        let tree_entries = store_b
            .read_tree(&tree_id.0)
            .expect("tree in local store after read-through")
            .unwrap();
        let file_entry = tree_entries
            .iter()
            .find(|e| e.name == "f.txt")
            .expect("file in tree");
        let file_id = match file_entry.kind {
            GitEntryKind::File { .. } => &file_entry.id,
            _ => panic!("expected file entry"),
        };
        assert!(
            store_b
                .read_file(file_id)
                .expect("read_file in local cache")
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

    // ---- M10.6: op-store RPCs -----------------------------------------

    #[tokio::test]
    async fn write_view_rejects_mismatched_id_bytes() {
        let svc = JujutsuService::bare();
        init_mount(&svc, "/tmp/m106badview").await;
        let (_actual_id, bytes) = make_view_blob().await;
        let err = svc
            .write_view(Request::new(WriteViewReq {
                working_copy_path: "/tmp/m106badview".into(),
                view_id: vec![0x55; 64],
                data: bytes,
            }))
            .await
            .expect_err("mismatched view id must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn read_view_rejects_corrupt_remote_blob() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir");
        let remote_url = format!("dir://{}", remote_dir.path().display());
        let svc = JujutsuService::bare();
        init_mount_with_remote(&svc, "/tmp/m106badviewremote", &remote_url).await;

        let (_actual_id, bytes) = make_view_blob().await;
        let wrong_id = vec![0x66; 64];
        let mut hex = String::with_capacity(128);
        for b in &wrong_id {
            use std::fmt::Write;
            let _ = write!(&mut hex, "{b:02x}");
        }
        std::fs::create_dir_all(remote_dir.path().join("view")).unwrap();
        std::fs::write(remote_dir.path().join("view").join(hex), bytes).unwrap();

        let err = svc
            .read_view(Request::new(ReadViewReq {
                working_copy_path: "/tmp/m106badviewremote".into(),
                view_id: wrong_id,
            }))
            .await
            .expect_err("corrupt remote view blob must be rejected");
        assert_eq!(err.code(), tonic::Code::DataLoss);
    }

    #[tokio::test]
    async fn write_operation_rejects_mismatched_id_bytes() {
        let svc = JujutsuService::bare();
        init_mount(&svc, "/tmp/m106badop").await;
        let (_actual_id, bytes) = make_operation_blob().await;
        let err = svc
            .write_operation(Request::new(WriteOperationReq {
                working_copy_path: "/tmp/m106badop".into(),
                operation_id: vec![0x77; 64],
                data: bytes,
            }))
            .await
            .expect_err("mismatched operation id must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn read_operation_rejects_corrupt_remote_blob() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir");
        let remote_url = format!("dir://{}", remote_dir.path().display());
        let svc = JujutsuService::bare();
        init_mount_with_remote(&svc, "/tmp/m106badopremote", &remote_url).await;

        let (_actual_id, bytes) = make_operation_blob().await;
        let wrong_id = vec![0x88; 64];
        let mut hex = String::with_capacity(128);
        for b in &wrong_id {
            use std::fmt::Write;
            let _ = write!(&mut hex, "{b:02x}");
        }
        std::fs::create_dir_all(remote_dir.path().join("operation")).unwrap();
        std::fs::write(remote_dir.path().join("operation").join(hex), bytes).unwrap();

        let err = svc
            .read_operation(Request::new(ReadOperationReq {
                working_copy_path: "/tmp/m106badopremote".into(),
                operation_id: wrong_id,
            }))
            .await
            .expect_err("corrupt remote operation blob must be rejected");
        assert_eq!(err.code(), tonic::Code::DataLoss);
    }

    #[tokio::test]
    async fn write_view_pushes_to_dir_remote() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir");
        let svc = JujutsuService::bare();
        let path = "/tmp/m106a".to_string();
        init_mount_with_remote(
            &svc,
            &path,
            &format!("dir://{}", remote_dir.path().display()),
        )
        .await;

        let (view_id, view_data) = make_view_blob().await;
        svc.write_view(Request::new(WriteViewReq {
            working_copy_path: path.clone(),
            view_id: view_id.clone(),
            data: view_data.clone(),
        }))
        .await
        .expect("write_view");

        // Confirm blob landed on the remote.
        let mut hex = String::with_capacity(128);
        for b in &view_id {
            use std::fmt::Write;
            let _ = write!(&mut hex, "{b:02x}");
        }
        let blob_path = remote_dir.path().join("view").join(&hex);
        let remote_bytes = std::fs::read(&blob_path).expect("view blob on remote");
        assert_eq!(remote_bytes, view_data);
    }

    #[tokio::test]
    async fn read_view_falls_back_to_remote_on_local_miss() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir");
        let remote_url = format!("dir://{}", remote_dir.path().display());

        // Service A writes a view.
        let svc_a = JujutsuService::bare();
        init_mount_with_remote(&svc_a, "/tmp/m106b_a", &remote_url).await;
        let (view_id, view_data) = make_view_blob().await;
        svc_a
            .write_view(Request::new(WriteViewReq {
                working_copy_path: "/tmp/m106b_a".into(),
                view_id: view_id.clone(),
                data: view_data.clone(),
            }))
            .await
            .expect("write_view");

        // Service B's local store is empty — read should fall through
        // to the shared remote.
        let svc_b = JujutsuService::bare();
        init_mount_with_remote(&svc_b, "/tmp/m106b_b", &remote_url).await;
        let got = svc_b
            .read_view(Request::new(ReadViewReq {
                working_copy_path: "/tmp/m106b_b".into(),
                view_id: view_id.clone(),
            }))
            .await
            .expect("read_view via remote")
            .into_inner();
        assert!(got.found);
        assert_eq!(got.data, view_data);

        // Confirm local cache was populated.
        let (store_b, _) = mount_handles(&svc_b.mounts, "/tmp/m106b_b")
            .await
            .unwrap();
        let cached = store_b
            .get_view_bytes(&view_id)
            .expect("get_view (after read-through)");
        assert!(cached.is_some(), "read-through should populate local cache");
    }

    #[tokio::test]
    async fn read_view_no_remote_miss_returns_not_found() {
        let svc = JujutsuService::bare();
        init_mount(&svc, "/tmp/m106c").await;
        let got = svc
            .read_view(Request::new(ReadViewReq {
                working_copy_path: "/tmp/m106c".into(),
                view_id: vec![0xee; 64],
            }))
            .await
            .expect("read_view (miss)")
            .into_inner();
        assert!(!got.found);
    }

    #[tokio::test]
    async fn write_operation_pushes_and_read_through_works() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir");
        let remote_url = format!("dir://{}", remote_dir.path().display());

        // A writes an operation.
        let svc_a = JujutsuService::bare();
        init_mount_with_remote(&svc_a, "/tmp/m106d_a", &remote_url).await;
        let (op_id, op_data) = make_operation_blob().await;
        svc_a
            .write_operation(Request::new(WriteOperationReq {
                working_copy_path: "/tmp/m106d_a".into(),
                operation_id: op_id.clone(),
                data: op_data.clone(),
            }))
            .await
            .expect("write_operation");

        // B reads it through the remote.
        let svc_b = JujutsuService::bare();
        init_mount_with_remote(&svc_b, "/tmp/m106d_b", &remote_url).await;
        let got = svc_b
            .read_operation(Request::new(ReadOperationReq {
                working_copy_path: "/tmp/m106d_b".into(),
                operation_id: op_id.clone(),
            }))
            .await
            .expect("read_operation via remote")
            .into_inner();
        assert!(got.found);
        assert_eq!(got.data, op_data);
    }

    #[tokio::test]
    async fn resolve_operation_id_prefix_works() {
        let svc = JujutsuService::bare();
        init_mount(&svc, "/tmp/m106e").await;

        let (op_id, op_data) = make_operation_blob().await;
        svc.write_operation(Request::new(WriteOperationReq {
            working_copy_path: "/tmp/m106e".into(),
            operation_id: op_id.clone(),
            data: op_data,
        }))
        .await
        .expect("write_operation");

        let mut prefix = String::with_capacity(8);
        for b in &op_id[..4] {
            use std::fmt::Write;
            let _ = write!(&mut prefix, "{b:02x}");
        }
        let got = svc
            .resolve_operation_id_prefix(Request::new(
                ResolveOperationIdPrefixReq {
                    working_copy_path: "/tmp/m106e".into(),
                    hex_prefix: prefix,
                },
            ))
            .await
            .expect("resolve prefix")
            .into_inner();
        assert_eq!(got.resolution, 1); // single match
        assert_eq!(got.full_id, op_id);

        // Bogus prefix should not match.
        let got = svc
            .resolve_operation_id_prefix(Request::new(
                ResolveOperationIdPrefixReq {
                    working_copy_path: "/tmp/m106e".into(),
                    hex_prefix: "ff00".into(),
                },
            ))
            .await
            .expect("resolve prefix (miss)")
            .into_inner();
        assert_eq!(got.resolution, 0); // no match
    }

    /// After WriteView exports bookmarks, external ref changes
    /// (add/change/delete) are detected by git_detect_head_change.
    #[tokio::test]
    async fn git_detect_bookmark_changes() {
        let svc = JujutsuService::bare();
        let path = "/tmp/bookmark_detect";
        init_mount(&svc, path).await;

        // Write a commit so we have a valid OID for bookmarks.
        let empty = svc
            .get_empty_tree_id(Request::new(GetEmptyTreeIdReq {
                working_copy_path: path.into(),
            }))
            .await
            .unwrap()
            .into_inner();

        let root_parent = vec![0; COMMIT_ID_LENGTH];
        let commit_id = svc
            .write_commit(Request::new(WriteCommitReq {
                working_copy_path: path.into(),
                commit: Some(Commit {
                    parents: vec![root_parent],
                    root_tree: vec![empty.tree_id.clone()],
                    description: "test".into(),
                    ..Default::default()
                }),
            }))
            .await
            .unwrap()
            .into_inner()
            .commit_id;

        // WriteView stamps last_exported_bookmarks (the root view has
        // no bookmarks, so it's Some(vec![])).
        let (view_id, view_data) = make_view_blob().await;
        svc.write_view(Request::new(WriteViewReq {
            working_copy_path: path.into(),
            view_id,
            data: view_data,
        }))
        .await
        .expect("write_view");

        // Before any external change, detect should find nothing.
        let reply = svc
            .git_detect_head_change(Request::new(GitDetectHeadChangeReq {
                working_copy_path: path.into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(
            reply.bookmark_changes.is_empty(),
            "no changes expected before external modification"
        );

        // Get the git repo path to manipulate refs directly.
        let git_path = {
            let mounts = svc.mounts.lock().await;
            let mount = mounts.get(path).unwrap();
            mount.store.git_repo_path().to_path_buf()
        };

        // --- Test addition: add a ref externally ---
        crate::git_ops::export_bookmarks(
            &git_path,
            &[("external-branch".to_string(), commit_id.clone())],
        )
        .unwrap();

        let reply = svc
            .git_detect_head_change(Request::new(GitDetectHeadChangeReq {
                working_copy_path: path.into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reply.bookmark_changes.len(), 1);
        assert_eq!(reply.bookmark_changes[0].name, "external-branch");
        assert_eq!(reply.bookmark_changes[0].commit_id, commit_id);

        // --- Test deletion: simulate the daemon exporting a bookmark,
        // then an external tool deleting it ---
        // Directly stamp the mount's last_exported_bookmarks as if
        // WriteView had exported "external-branch".
        {
            let mut mounts = svc.mounts.lock().await;
            let mount = mounts.get_mut(path).unwrap();
            mount.last_exported_bookmarks =
                Some(vec![("external-branch".to_string(), commit_id.clone())]);
        }

        // Now the ref exists and matches — no changes.
        let reply = svc
            .git_detect_head_change(Request::new(GitDetectHeadChangeReq {
                working_copy_path: path.into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(
            reply.bookmark_changes.is_empty(),
            "no changes when refs match exported bookmarks"
        );

        // Delete it externally.
        let keep_none: HashSet<&str> = HashSet::new();
        crate::git_ops::cleanup_stale_refs(&git_path, &keep_none).unwrap();

        let reply = svc
            .git_detect_head_change(Request::new(GitDetectHeadChangeReq {
                working_copy_path: path.into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reply.bookmark_changes.len(), 1);
        assert_eq!(reply.bookmark_changes[0].name, "external-branch");
        assert!(
            reply.bookmark_changes[0].commit_id.is_empty(),
            "deleted bookmark should have empty commit_id"
        );

        // --- Before first WriteView, detection is skipped ---
        let svc2 = JujutsuService::bare();
        let path2 = "/tmp/bookmark_detect_2";
        init_mount(&svc2, path2).await;
        let reply = svc2
            .git_detect_head_change(Request::new(GitDetectHeadChangeReq {
                working_copy_path: path2.into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(
            reply.bookmark_changes.is_empty(),
            "no detection before first WriteView"
        );
    }

    // ---- Stale mount / force_unmount tests ----

    #[test]
    fn force_unmount_on_non_mountpoint_returns_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().unwrap();
        let result = force_unmount(path);
        assert!(result.is_err(), "force_unmount should fail on a plain directory");
    }

    #[test]
    fn is_mountpoint_detects_real_mountpoints() {
        // /proc is always a separate filesystem on Linux.
        #[cfg(target_os = "linux")]
        {
            let result = is_mountpoint(Path::new("/proc"));
            assert!(matches!(result, Ok(true)), "/proc should be a mountpoint");
        }

        // A tempdir is on the same device as its parent.
        let dir = tempfile::tempdir().expect("tempdir");
        let result = is_mountpoint(dir.path());
        assert!(
            matches!(result, Ok(false)),
            "tempdir should NOT be a mountpoint"
        );
    }

    #[test]
    fn validate_mountpoint_rejects_non_empty_mountpoint() {
        // /proc is a mountpoint and non-empty — validate catches it at
        // the emptiness check (before the dev-id check). Both are
        // valid rejection reasons.
        #[cfg(target_os = "linux")]
        {
            let result = validate_mountpoint("/proc");
            assert!(result.is_err(), "/proc should not pass mountpoint validation");
        }
    }

    /// Full integration test: create a mount, simulate a stale FUSE mount
    /// left behind by a crashed daemon, then verify rehydration detects and
    /// force-unmounts it before re-binding.
    ///
    /// Requires /dev/fuse — skipped if unavailable (CI without FUSE).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rehydrate_force_unmounts_stale_fuse_mount() {
        use crate::vfs_mgr::{VfsManager, VfsManagerConfig};

        if !Path::new("/dev/fuse").exists() {
            eprintln!("skipping: /dev/fuse not available");
            return;
        }

        let storage_dir = tempfile::tempdir().expect("storage tempdir");
        let mount_dir = tempfile::tempdir().expect("mount tempdir");
        let mount_path = mount_dir.path().to_str().unwrap().to_owned();

        // Phase 1: use a bare service (no VFS) to initialize and persist
        // the mount metadata + redb store.
        let svc1 = JujutsuService::new(
            None,
            StorageConfig::on_disk(storage_dir.path().to_owned()),
        );
        svc1.initialize(Request::new(InitializeReq {
            path: mount_path.clone(),
            remote: String::new(),
        }))
        .await
        .expect("initialize");
        drop(svc1);

        // Phase 2: create a FUSE mount at mount_path to simulate a stale
        // mount left behind by a crashed daemon. The VFS manager creates
        // a real kernel mount; we `mem::forget` the attachment so it
        // stays alive after we lose the handle.
        let mut vfs = VfsManager::new(VfsManagerConfig {
            min_nfs_port: 0,
            max_nfs_port: 0,
        });
        let vfs_handle = vfs.handle();

        // The VFS manager needs to run as a task so bind() can complete.
        let vfs_task = tokio::spawn(async move { vfs.serve().await });

        // Create a minimal KikiFs for the stale mount — content doesn't
        // matter, we just need a real FUSE session at the mountpoint.
        let settings = {
            let mut config = jj_lib::config::StackedConfig::with_defaults();
            config.add_layer(
                jj_lib::config::ConfigLayer::parse(
                    jj_lib::config::ConfigSource::User,
                    "user.name = \"Test\"\nuser.email = \"t@t\"\noperation.hostname = \"t\"\noperation.username = \"t\"",
                ).unwrap(),
            );
            jj_lib::settings::UserSettings::from_config(config).unwrap()
        };
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let root = ty::Id(store.empty_tree_id().as_bytes().try_into().unwrap());
        let fs: Arc<dyn JjKikiFs> = Arc::new(KikiFs::new(store, root, None, None));
        let (_transport, attachment) = vfs_handle
            .bind(mount_path.clone(), fs)
            .await
            .expect("bind FUSE for stale mount");

        // Confirm the path is now a mountpoint.
        assert!(
            is_mountpoint(Path::new(&mount_path)).unwrap(),
            "mount_path should be a mountpoint after FUSE bind"
        );

        // Leak the attachment so the FUSE mount survives — simulating a
        // daemon crash where the mount handle was never dropped cleanly.
        std::mem::forget(attachment);

        // Phase 3: a new service with a VFS handle rehydrates from the
        // same storage dir. It should detect the stale mount, call
        // fusermount3 -u, and re-bind successfully.
        let mut vfs2 = VfsManager::new(VfsManagerConfig {
            min_nfs_port: 0,
            max_nfs_port: 0,
        });
        let vfs_handle2 = vfs2.handle();
        let vfs_task2 = tokio::spawn(async move { vfs2.serve().await });

        let svc2 = JujutsuService::new(
            Some(vfs_handle2),
            StorageConfig::on_disk(storage_dir.path().to_owned()),
        );
        svc2.rehydrate().await.expect(
            "rehydrate should succeed after force-unmounting stale mount"
        );

        // Verify the mount was rehydrated: DaemonStatus should list it.
        let status = svc2
            .daemon_status(Request::new(DaemonStatusReq {}))
            .await
            .expect("daemon_status")
            .into_inner();
        assert_eq!(status.data.len(), 1, "one mount should be rehydrated");
        assert_eq!(status.data[0].path, mount_path);

        // Clean up: dropping svc2 drops the new mount attachment,
        // which unmounts the re-bound FUSE mount.
        drop(svc2);
        drop(vfs_handle);
        vfs_task.abort();
        vfs_task2.abort();
    }
}
