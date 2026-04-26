use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use proto::jj_interface::*;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::store::Store;
use crate::ty;
use crate::vfs::{FsError, JjYakFs, YakFs};
use crate::vfs_mgr::{BindError, MountAttachment, TransportInfo, VfsManagerHandle};

/// Map a fallible proto-decode error onto an `invalid_argument` gRPC status.
/// Use for any conversion that came off the wire — peers that send malformed
/// requests should get a clean error, not crash the daemon. Uses the
/// alternate `{:#}` format so anyhow-style error chains surface their root
/// cause (e.g. "expected 32-byte id, got N bytes") rather than just the
/// outer context.
fn decode_status<E: std::fmt::Display>(context: &str) -> impl FnOnce(E) -> Status + '_ {
    move |e| Status::invalid_argument(format!("{context}: {e:#}"))
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
    /// Carried from `Initialize.remote`; surfaced via `DaemonStatus`. Will
    /// become meaningful once Layer C lands.
    remote: String,
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
    /// Per-mount filesystem. The same `Arc` is handed to the VFS bind so
    /// kernel I/O hits this object; we keep a clone here so RPCs that
    /// mutate the mount (today: `CheckOut`) can drive it directly.
    fs: Arc<dyn JjYakFs>,
    /// Holds the kernel mount alive. `None` only in the test path where
    /// `JujutsuService::bare` constructed a service without a
    /// `VfsManagerHandle`. Production `Initialize` always populates it.
    #[allow(dead_code)] // dropped on Mount drop, never read directly
    attachment: Option<MountAttachment>,
}

pub struct JujutsuService {
    /// Per-mount state, keyed by `working_copy_path`. Use `tokio::Mutex`
    /// because the RPC handlers are async; contention is minimal (one
    /// per-mount entry, mostly small reads/writes).
    mounts: Arc<Mutex<HashMap<String, Mount>>>,
    /// `None` only in `bare()` test mode. Production `Initialize`
    /// requires it.
    vfs_handle: Option<VfsManagerHandle>,
}

impl JujutsuService {
    /// Production constructor: with `Some(vfs_handle)`, `Initialize`
    /// validates mountpoints and attaches a real FUSE/NFS mount.
    /// `None` is the integration-test path (see `Config.disable_mount`):
    /// per-mount state is still tracked and store/WC RPCs work, but
    /// nothing is mounted at `working_copy_path`.
    pub fn new(
        vfs_handle: Option<VfsManagerHandle>,
    ) -> jujutsu_interface_server::JujutsuInterfaceServer<Self> {
        jujutsu_interface_server::JujutsuInterfaceServer::new(JujutsuService {
            mounts: Arc::new(Mutex::new(HashMap::new())),
            vfs_handle,
        })
    }

    /// Bare service without the gRPC server wrapping or any VFS attach.
    /// Used by tests that drive the trait methods directly: `Initialize`
    /// skips both mountpoint validation and VFS bind, so callers can use
    /// arbitrary string paths (`/tmp/repo`, `/never/initialized`) without
    /// creating real directories or spinning up a real VFS manager.
    #[cfg(test)]
    fn bare() -> Self {
        JujutsuService {
            mounts: Arc::new(Mutex::new(HashMap::new())),
            vfs_handle: None,
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

/// Look up a mount by `working_copy_path` and clone its store handle so
/// the lock can be released before doing real work. All store RPCs use
/// this — the lock is short, the RPC body is long.
async fn store_for(
    mounts: &Arc<Mutex<HashMap<String, Mount>>>,
    path: &str,
) -> Result<Arc<Store>, Status> {
    let guard = mounts.lock().await;
    guard
        .get(path)
        .map(|m| m.store.clone())
        .ok_or_else(|| Status::not_found(format!("no mount at {path}")))
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

        let store = Arc::new(Store::new());
        let root_tree_id = store.get_empty_tree_id();

        // Build the per-mount FS up front so we can hand the same `Arc`
        // to both the VFS bind (for kernel I/O) and the `Mount` (for
        // RPCs like `CheckOut` that mutate mount-side state). Even on
        // the disable_mount test path the `Mount` keeps an `fs` so
        // mutating RPCs work end-to-end without the kernel.
        let fs: Arc<dyn JjYakFs> = Arc::new(YakFs::new(store.clone(), root_tree_id));

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
        mounts.insert(
            req.path.clone(),
            Mount {
                working_copy_path: req.path,
                remote: req.remote,
                op_id: Vec::new(),
                workspace_id: Vec::new(),
                root_tree_id: root_tree_id.0.to_vec(),
                store,
                fs,
                attachment,
            },
        );

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
        let store = store_for(&self.mounts, &req.working_copy_path).await?;
        let file_id = store.write_file(ty::File { content: req.data }).into();
        Ok(Response::new(FileId { file_id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_file(
        &self,
        request: Request<ReadFileReq>,
    ) -> Result<Response<File>, Status> {
        let req = request.into_inner();
        let store = store_for(&self.mounts, &req.working_copy_path).await?;
        let file_id: ty::Id = FileId { file_id: req.file_id }
            .try_into()
            .map_err(decode_status("file id"))?;
        let file = store
            .get_file(file_id)
            .ok_or_else(|| Status::not_found(format!("file {} not found", hex(&file_id))))?;
        Ok(Response::new(file.as_proto()))
    }

    #[tracing::instrument(skip(self))]
    async fn write_symlink(
        &self,
        request: Request<WriteSymlinkReq>,
    ) -> Result<Response<SymlinkId>, Status> {
        let req = request.into_inner();
        let store = store_for(&self.mounts, &req.working_copy_path).await?;
        let symlink = ty::Symlink { target: req.target };
        let symlink_id = store.write_symlink(symlink).into();
        Ok(Response::new(SymlinkId { symlink_id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_symlink(
        &self,
        request: Request<ReadSymlinkReq>,
    ) -> Result<Response<Symlink>, Status> {
        let req = request.into_inner();
        let store = store_for(&self.mounts, &req.working_copy_path).await?;
        let symlink_id: ty::Id = SymlinkId { symlink_id: req.symlink_id }
            .try_into()
            .map_err(decode_status("symlink id"))?;
        let symlink = store
            .get_symlink(symlink_id)
            .ok_or_else(|| Status::not_found(format!("symlink {} not found", hex(&symlink_id))))?;
        Ok(Response::new(symlink.as_proto()))
    }

    #[tracing::instrument(skip(self))]
    async fn write_tree(
        &self,
        request: Request<WriteTreeReq>,
    ) -> Result<Response<TreeId>, Status> {
        let req = request.into_inner();
        let store = store_for(&self.mounts, &req.working_copy_path).await?;
        let tree_proto = req
            .tree
            .ok_or_else(|| Status::invalid_argument("WriteTreeReq.tree is required"))?;
        let tree: ty::Tree = tree_proto.try_into().map_err(decode_status("tree"))?;
        let tree_id = store.write_tree(tree).into();
        Ok(Response::new(TreeId { tree_id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_tree(
        &self,
        request: Request<ReadTreeReq>,
    ) -> Result<Response<Tree>, Status> {
        let req = request.into_inner();
        let store = store_for(&self.mounts, &req.working_copy_path).await?;
        let tree_id: ty::Id = TreeId { tree_id: req.tree_id }
            .try_into()
            .map_err(decode_status("tree id"))?;
        let tree = store
            .get_tree(tree_id)
            .ok_or_else(|| Status::not_found(format!("tree {} not found", hex(&tree_id))))?;
        Ok(Response::new(tree.as_proto()))
    }

    #[tracing::instrument(skip(self))]
    async fn write_commit(
        &self,
        request: Request<WriteCommitReq>,
    ) -> Result<Response<CommitId>, Status> {
        let req = request.into_inner();
        let store = store_for(&self.mounts, &req.working_copy_path).await?;
        let commit_proto = req
            .commit
            .ok_or_else(|| Status::invalid_argument("WriteCommitReq.commit is required"))?;
        if commit_proto.parents.is_empty() {
            return Err(Status::internal("Cannot write a commit with no parents"));
        }
        let commit: ty::Commit = commit_proto.try_into().map_err(decode_status("commit"))?;
        let commit_id = store.write_commit(commit).into();
        Ok(Response::new(CommitId { commit_id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_commit(
        &self,
        request: Request<ReadCommitReq>,
    ) -> Result<Response<Commit>, Status> {
        let req = request.into_inner();
        let store = store_for(&self.mounts, &req.working_copy_path).await?;
        let commit_id: ty::Id = CommitId { commit_id: req.commit_id }
            .try_into()
            .map_err(decode_status("commit id"))?;
        let commit = store
            .get_commit(commit_id)
            .ok_or_else(|| Status::not_found(format!("commit {} not found", hex(&commit_id))))?;
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
        Ok(Response::new(SetCheckoutStateReply {}))
    }

    #[tracing::instrument(skip(self))]
    async fn snapshot(
        &self,
        request: Request<SnapshotReq>,
    ) -> Result<Response<SnapshotReply>, Status> {
        let req = request.into_inner();

        // Clone the per-mount FS handle out from under the lock so the
        // (potentially I/O-heavy) snapshot walk doesn't hold it. Same
        // pattern as `check_out`.
        let fs = {
            let mounts = self.mounts.lock().await;
            let mount = mounts.get(&req.working_copy_path).ok_or_else(|| {
                Status::not_found(format!("no mount at {}", req.working_copy_path))
            })?;
            mount.fs.clone()
        };

        let new_root = fs.snapshot().await.map_err(|e| {
            Status::internal(format!(
                "snapshot failed for {}: {e}",
                req.working_copy_path
            ))
        })?;
        info!(path = %req.working_copy_path, tree_id = %hex(&new_root), "Snapshot");

        // Stamp the new root tree id back on the Mount so subsequent
        // `GetTreeState`/`Snapshot` reads agree with what the VFS just
        // produced. We do this *after* the snapshot succeeds — a
        // half-snapshotted Mount.root_tree_id would lie about what the
        // kernel sees through `fs`.
        let mut mounts = self.mounts.lock().await;
        if let Some(mount) = mounts.get_mut(&req.working_copy_path) {
            mount.root_tree_id = new_root.0.to_vec();
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

    use super::*;

    /// Initialize a mount via the test path (no VFS bind). Returns the
    /// path used so the caller can pass it back into store/WC RPCs.
    async fn init_mount(svc: &JujutsuService, path: &str) {
        svc.initialize(Request::new(InitializeReq {
            path: path.to_owned(),
            remote: "localhost".into(),
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
        for (path, remote) in [("/tmp/a", "remote-a"), ("/tmp/b", "remote-b")] {
            svc.initialize(Request::new(InitializeReq {
                path: path.into(),
                remote: remote.into(),
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
                remote: "localhost".into(),
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
}
