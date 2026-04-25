use std::collections::HashMap;
use std::sync::Arc;

use proto::jj_interface::*;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use tracing::info;

use crate::{store::Store, ty};

/// Map a fallible proto-decode error onto an `invalid_argument` gRPC status.
/// Use for any conversion that came off the wire — peers that send malformed
/// requests should get a clean error, not crash the daemon.
fn decode_status<E: std::fmt::Display>(context: &str) -> impl FnOnce(E) -> Status + '_ {
    move |e| Status::invalid_argument(format!("{context}: {e}"))
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
/// the daemon needs to answer working-copy RPCs (`Get/SetCheckoutState`,
/// `GetTreeState`, `Snapshot`) for one mount.
///
/// `op_id` and `workspace_id` start empty after `Initialize`; the CLI fills
/// them in via `SetCheckoutState` once `YakWorkingCopy::init` runs (M2 wires
/// that path up). `root_tree_id` defaults to the store's empty tree until a
/// real check-out lands (M5).
///
/// Field types match the proto wire format (`Vec<u8>` for the `bytes` fields,
/// `String` for `working_copy_path`/`remote`) so RPC handlers can copy in/out
/// without conversion. The plan's `RepoPathBuf` for sparse patterns can wait
/// until there's actually something to do with them.
#[derive(Clone, Debug)]
struct Mount {
    /// Canonical working-copy path. Also the map key; stored here too so the
    /// `Mount` is self-describing for `DaemonStatus` listings.
    #[allow(dead_code)]
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
}

pub struct JujutsuService {
    store: Store,
    /// Per-mount state, keyed by `working_copy_path`. Use `tokio::Mutex`
    /// because the RPC handlers are async; contention is minimal (one
    /// per-mount entry, mostly small reads/writes).
    mounts: Arc<Mutex<HashMap<String, Mount>>>,
}

impl JujutsuService {
    pub fn new() -> jujutsu_interface_server::JujutsuInterfaceServer<Self> {
        jujutsu_interface_server::JujutsuInterfaceServer::new(JujutsuService::bare())
    }

    /// Bare service without the gRPC server wrapping. Used by tests that
    /// drive the trait methods directly.
    fn bare() -> Self {
        JujutsuService {
            store: Store::new(),
            mounts: Arc::new(Mutex::new(HashMap::new())),
        }
    }
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
        let mut mounts = self.mounts.lock().await;
        // Reject re-init of the same path: if a Mount already exists we'd
        // silently clobber `op_id`/`workspace_id`/`root_tree_id` and the
        // CLI's view of the world would diverge from the daemon's. Better
        // to surface the collision.
        if mounts.contains_key(&req.path) {
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
                root_tree_id: self.store.get_empty_tree_id().0.to_vec(),
            },
        );
        Ok(Response::new(InitializeReply {}))
    }

    #[tracing::instrument(skip(self))]
    async fn daemon_status(
        &self,
        request: Request<DaemonStatusReq>,
    ) -> Result<Response<DaemonStatusReply>, Status> {
        let _req = request.into_inner();
        let mounts = self.mounts.lock().await;
        // Sort by path so output is deterministic — `yak status` is
        // user-facing and `HashMap` iteration order is not.
        let mut entries: Vec<_> = mounts.values().cloned().collect();
        entries.sort_by(|a, b| a.working_copy_path.cmp(&b.working_copy_path));
        let data = entries
            .into_iter()
            .map(|mnt| proto::jj_interface::daemon_status_reply::Data {
                path: mnt.working_copy_path,
                remote: mnt.remote,
            })
            .collect();
        Ok(Response::new(DaemonStatusReply { data }))
    }

    #[tracing::instrument(skip(self))]
    async fn get_empty_tree_id(
        &self,
        _request: Request<GetEmptyTreeIdReq>,
    ) -> Result<Response<TreeId>, Status> {
        Ok(Response::new(TreeId {
            tree_id: self.store.get_empty_tree_id().into(),
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn write_file(&self, request: Request<File>) -> Result<Response<FileId>, Status> {
        let file = request.into_inner();
        let file_id = self.store.write_file(file.into()).await.into();
        Ok(Response::new(FileId { file_id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_file(&self, request: Request<FileId>) -> Result<Response<File>, Status> {
        let file_id: ty::Id = request
            .into_inner()
            .try_into()
            .map_err(decode_status("file id"))?;
        let file = self
            .store
            .get_file(file_id)
            .ok_or_else(|| Status::not_found(format!("file {} not found", hex(&file_id))))?;
        Ok(Response::new(file.as_proto()))
    }

    #[tracing::instrument(skip(self))]
    async fn write_symlink(
        &self,
        request: Request<Symlink>,
    ) -> Result<Response<SymlinkId>, Status> {
        let symlink = request.into_inner();
        let symlink_id = self.store.write_symlink(symlink.into()).await.into();
        Ok(Response::new(SymlinkId { symlink_id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_symlink(&self, request: Request<SymlinkId>) -> Result<Response<Symlink>, Status> {
        let symlink_id: ty::Id = request
            .into_inner()
            .try_into()
            .map_err(decode_status("symlink id"))?;
        let symlink = self
            .store
            .get_symlink(symlink_id)
            .ok_or_else(|| Status::not_found(format!("symlink {} not found", hex(&symlink_id))))?;
        Ok(Response::new(symlink.as_proto()))
    }

    #[tracing::instrument(skip(self))]
    async fn write_tree(&self, request: Request<Tree>) -> Result<Response<TreeId>, Status> {
        let tree: ty::Tree = request
            .into_inner()
            .try_into()
            .map_err(decode_status("tree"))?;
        let tree_id = self.store.write_tree(tree).await.into();
        Ok(Response::new(TreeId { tree_id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_tree(&self, request: Request<TreeId>) -> Result<Response<Tree>, Status> {
        let tree_id: ty::Id = request
            .into_inner()
            .try_into()
            .map_err(decode_status("tree id"))?;
        let tree = self
            .store
            .get_tree(tree_id)
            .ok_or_else(|| Status::not_found(format!("tree {} not found", hex(&tree_id))))?;
        Ok(Response::new(tree.as_proto()))
    }

    #[tracing::instrument(skip(self))]
    async fn write_commit(&self, request: Request<Commit>) -> Result<Response<CommitId>, Status> {
        let commit_proto = request.into_inner();
        if commit_proto.parents.is_empty() {
            return Err(Status::internal("Cannot write a commit with no parents"));
        }
        let commit: ty::Commit = commit_proto.try_into().map_err(decode_status("commit"))?;
        let commit_id = self.store.write_commit(commit).await.into();
        Ok(Response::new(CommitId { commit_id }))
    }

    #[tracing::instrument(skip(self))]
    async fn read_commit(&self, request: Request<CommitId>) -> Result<Response<Commit>, Status> {
        let commit_id: ty::Id = request
            .into_inner()
            .try_into()
            .map_err(decode_status("commit id"))?;
        let commits = self.store.commits.lock();
        let commit = commits
            .get(&commit_id)
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
        // M1 stub: there is no VFS write path yet, so a "snapshot" is just
        // the currently-checked-out tree id. M6 will replace this with a
        // real snapshot computed from the VFS write log.
        let mounts = self.mounts.lock().await;
        let mount = mounts.get(&req.working_copy_path).ok_or_else(|| {
            Status::not_found(format!("no mount at {}", req.working_copy_path))
        })?;
        Ok(Response::new(SnapshotReply {
            tree_id: mount.root_tree_id.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    const COMMIT_ID_LENGTH: usize = 32;
    const CHANGE_ID_LENGTH: usize = 16;

    use assert_matches::assert_matches;
    use proto::jj_interface::jujutsu_interface_server::JujutsuInterface;

    use super::*;

    #[tokio::test]
    async fn write_commit_parents() {
        let svc = JujutsuService::bare();
        // No parents
        let mut commit = Commit {
            parents: vec![],
            ..Default::default()
        };

        assert_matches!(
            svc.write_commit(Request::new(commit.clone())).await,
            Err(status) if status.message().contains("no parents")
        );

        // Only root commit as parent
        commit.parents = vec![vec![0; CHANGE_ID_LENGTH]];
        let first_id = svc
            .write_commit(Request::new(commit.clone()))
            .await
            .unwrap()
            .into_inner();
        let first_commit = svc
            .read_commit(Request::new(first_id.clone()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(first_commit, commit);

        // Only non-root commit as parent
        commit.parents = vec![first_id.clone().commit_id];
        let second_id = svc
            .write_commit(Request::new(commit.clone()))
            .await
            .unwrap()
            .into_inner();
        let second_commit = svc
            .read_commit(Request::new(second_id.clone()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(second_commit, commit);

        // Merge commit
        commit.parents = vec![first_id.clone().commit_id, second_id.commit_id];
        let merge_id = svc
            .write_commit(Request::new(commit.clone()))
            .await
            .unwrap()
            .into_inner();
        let merge_commit = svc
            .read_commit(Request::new(merge_id.clone()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(merge_commit, commit);

        commit.parents = vec![first_id.commit_id, vec![0; COMMIT_ID_LENGTH]];
        let root_merge_id = svc
            .write_commit(Request::new(commit.clone()))
            .await
            .unwrap()
            .into_inner();
        let root_merge_commit = svc
            .read_commit(Request::new(root_merge_id.clone()))
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

        svc.initialize(Request::new(InitializeReq {
            path: path.clone(),
            remote: "localhost".into(),
        }))
        .await
        .unwrap();

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
            .get_empty_tree_id(Request::new(GetEmptyTreeIdReq {}))
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

        // Snapshot returns whatever the current root tree is (still the
        // empty tree until M5/M6 change it).
        let snap = svc
            .snapshot(Request::new(SnapshotReq {
                working_copy_path: path.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(snap.tree_id, empty.tree_id);
    }

    /// Mounts must be isolated by `working_copy_path`; mutating one must
    /// not bleed into another. Mirrors `test_repos_are_independent` at the
    /// CLI level.
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
}
