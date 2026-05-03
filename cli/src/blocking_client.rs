// `tonic::Status` is ~176 bytes; clippy flags this as `result_large_err` on
// every RPC return. Boxing it would touch every call site in this crate; the
// size cost on the cold error path is not worth that churn while
// `BlockingJujutsuInterfaceClient` is the only consumer of these signatures.
#![allow(clippy::result_large_err)]

use std::sync::{Arc, Mutex};

use proto::jj_interface::{jujutsu_interface_client::JujutsuInterfaceClient, *};
use tokio::runtime::{Builder, Runtime};

type StdError = Box<dyn std::error::Error + Send + Sync + 'static>;
type Result<T, E = StdError> = ::std::result::Result<T, E>;

// The order of the fields in this struct is important. They must be ordered
// such that when `BlockingJujutsuInterfaceClient` is dropped the client is dropped
// before the runtime. Not doing this will result in a deadlock when dropped.
// Rust drops struct fields in declaration order.
#[derive(Debug, Clone)]
pub struct BlockingJujutsuInterfaceClient {
    client: Arc<Mutex<JujutsuInterfaceClient<tonic::transport::Channel>>>,
    rt: Arc<Mutex<Runtime>>,
}

impl BlockingJujutsuInterfaceClient {
    /// Connect to the daemon via a Unix domain socket.
    pub fn connect_uds(path: std::path::PathBuf) -> std::result::Result<Self, StdError> {
        // Quick existence check before spinning up a runtime.
        if !path.exists() {
            return Err(format!("socket not found: {}", path.display()).into());
        }
        let rt = Builder::new_multi_thread().enable_all().build().unwrap();
        let channel = rt.block_on(async {
            let path_clone = path.clone();
            tonic::transport::Endpoint::from_static("http://[::]:50051")
                .connect_with_connector(tower::service_fn(
                    move |_: tonic::transport::Uri| {
                        let path = path_clone.clone();
                        async move { tokio::net::UnixStream::connect(path).await }
                    },
                ))
                .await
        })?;
        let client = Arc::new(Mutex::new(JujutsuInterfaceClient::new(channel)));
        let rt = Arc::new(Mutex::new(rt));
        Ok(Self { client, rt })
    }

    pub fn daemon_status(
        &self,
        request: impl tonic::IntoRequest<DaemonStatusReq>,
    ) -> Result<tonic::Response<DaemonStatusReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.daemon_status(request))
    }

    pub fn get_tree_state(
        &self,
        request: impl tonic::IntoRequest<GetTreeStateReq>,
    ) -> Result<tonic::Response<GetTreeStateReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.get_tree_state(request))
    }

    pub fn initialize(
        &self,
        request: impl tonic::IntoRequest<InitializeReq>,
    ) -> Result<tonic::Response<InitializeReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.initialize(request))
    }

    pub fn set_checkout_state(
        &self,
        request: impl tonic::IntoRequest<SetCheckoutStateReq>,
    ) -> Result<tonic::Response<SetCheckoutStateReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.set_checkout_state(request))
    }

    pub fn get_checkout_state(
        &self,
        request: impl tonic::IntoRequest<GetCheckoutStateReq>,
    ) -> Result<tonic::Response<CheckoutState>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.get_checkout_state(request))
    }

    pub fn snapshot(
        &self,
        request: impl tonic::IntoRequest<SnapshotReq>,
    ) -> Result<tonic::Response<SnapshotReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.snapshot(request))
    }

    pub fn check_out(
        &self,
        request: impl tonic::IntoRequest<CheckOutReq>,
    ) -> Result<tonic::Response<CheckOutReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.check_out(request))
    }

    // Store RPCs all carry `working_copy_path` on the request side now (see
    // proto/jj_interface.proto). KikiBackend stamps it from its own
    // `working_copy_path` field on every call.

    pub fn write_commit(
        &self,
        request: impl tonic::IntoRequest<WriteCommitReq>,
    ) -> Result<tonic::Response<CommitId>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.write_commit(request))
    }

    pub fn read_commit(
        &self,
        request: impl tonic::IntoRequest<ReadCommitReq>,
    ) -> Result<tonic::Response<Commit>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.read_commit(request))
    }

    pub fn write_file(
        &self,
        request: impl tonic::IntoRequest<WriteFileReq>,
    ) -> Result<tonic::Response<FileId>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.write_file(request))
    }

    pub fn read_file(
        &self,
        request: impl tonic::IntoRequest<ReadFileReq>,
    ) -> Result<tonic::Response<File>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.read_file(request))
    }

    pub fn write_tree(
        &self,
        request: impl tonic::IntoRequest<WriteTreeReq>,
    ) -> Result<tonic::Response<TreeId>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.write_tree(request))
    }

    pub fn read_tree(
        &self,
        request: impl tonic::IntoRequest<ReadTreeReq>,
    ) -> Result<tonic::Response<Tree>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.read_tree(request))
    }

    pub fn write_symlink(
        &self,
        request: impl tonic::IntoRequest<WriteSymlinkReq>,
    ) -> Result<tonic::Response<SymlinkId>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.write_symlink(request))
    }

    pub fn read_symlink(
        &self,
        request: impl tonic::IntoRequest<ReadSymlinkReq>,
    ) -> Result<tonic::Response<Symlink>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.read_symlink(request))
    }

    pub fn get_empty_tree_id(
        &self,
        working_copy_path: String,
    ) -> Result<tonic::Response<TreeId>, tonic::Status> {
        // Acquire `client` before `rt` to match every other RPC method —
        // single ordering avoids the latent two-mutex deadlock that
        // would surface if the client is ever called concurrently
        // (`BlockingJujutsuInterfaceClient` is `Clone + Send`).
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.get_empty_tree_id(GetEmptyTreeIdReq { working_copy_path }))
    }

    // ---- M10.5: catalog (mutable refs) ------------------------------
    //
    // The CLI talks to the local daemon's `JujutsuInterface` for every
    // RPC; the daemon dispatches per-mount to either the configured
    // remote's catalog or the local-fallback `LocalRefs` (PLAN.md
    // §10.5.2 decision 1). `KikiOpHeadsStore` (cli/src/op_heads_store.rs)
    // is the sole consumer today.

    pub fn get_catalog_ref(
        &self,
        request: impl tonic::IntoRequest<GetCatalogRefReq>,
    ) -> Result<tonic::Response<GetCatalogRefReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.get_catalog_ref(request))
    }

    pub fn cas_catalog_ref(
        &self,
        request: impl tonic::IntoRequest<CasCatalogRefReq>,
    ) -> Result<tonic::Response<CasCatalogRefReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.cas_catalog_ref(request))
    }

    // Unused by the only M10.5 consumer (`KikiOpHeadsStore`) since the
    // single-`op_heads`-ref keying doesn't need enumeration. Kept on
    // the client surface so a future branch-tip / multi-ref consumer
    // doesn't bounce back through the proto crate to add it.
    #[allow(dead_code)]
    pub fn list_catalog_refs(
        &self,
        request: impl tonic::IntoRequest<ListCatalogRefsReq>,
    ) -> Result<tonic::Response<ListCatalogRefsReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.list_catalog_refs(request))
    }

    // ---- M10.6: op-store RPCs -----------------------------------------
    //
    // The daemon stores/forwards opaque bytes; serialization and content
    // hashing happen on the CLI side (KikiOpStore in op_store.rs).

    pub fn write_view(
        &self,
        request: impl tonic::IntoRequest<WriteViewReq>,
    ) -> Result<tonic::Response<WriteViewReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.write_view(request))
    }

    pub fn read_view(
        &self,
        request: impl tonic::IntoRequest<ReadViewReq>,
    ) -> Result<tonic::Response<ReadViewReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.read_view(request))
    }

    pub fn write_operation(
        &self,
        request: impl tonic::IntoRequest<WriteOperationReq>,
    ) -> Result<tonic::Response<WriteOperationReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.write_operation(request))
    }

    pub fn read_operation(
        &self,
        request: impl tonic::IntoRequest<ReadOperationReq>,
    ) -> Result<tonic::Response<ReadOperationReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.read_operation(request))
    }

    pub fn resolve_operation_id_prefix(
        &self,
        request: impl tonic::IntoRequest<ResolveOperationIdPrefixReq>,
    ) -> Result<tonic::Response<ResolveOperationIdPrefixReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.resolve_operation_id_prefix(request))
    }

    // ---- Git remote operations ----------------------------------------

    pub fn git_remote_add(
        &self,
        request: impl tonic::IntoRequest<GitRemoteAddReq>,
    ) -> Result<tonic::Response<GitRemoteAddReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.git_remote_add(request))
    }

    pub fn git_remote_list(
        &self,
        request: impl tonic::IntoRequest<GitRemoteListReq>,
    ) -> Result<tonic::Response<GitRemoteListReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.git_remote_list(request))
    }

    pub fn git_push(
        &self,
        request: impl tonic::IntoRequest<GitPushReq>,
    ) -> Result<tonic::Response<GitPushReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.git_push(request))
    }

    pub fn git_fetch(
        &self,
        request: impl tonic::IntoRequest<GitFetchReq>,
    ) -> Result<tonic::Response<GitFetchReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.git_fetch(request))
    }

    pub fn git_detect_head_change(
        &self,
        request: impl tonic::IntoRequest<GitDetectHeadChangeReq>,
    ) -> Result<tonic::Response<GitDetectHeadChangeReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.git_detect_head_change(request))
    }

    // ---- M13: first-class git clone ------------------------------------

    pub fn git_clone(
        &self,
        request: impl tonic::IntoRequest<GitCloneReq>,
    ) -> Result<tonic::Response<GitCloneReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.git_clone(request))
    }

    // ---- M13: kiki remote management ----------------------------------

    pub fn remote_add(
        &self,
        request: impl tonic::IntoRequest<RemoteAddReq>,
    ) -> Result<tonic::Response<RemoteAddReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.remote_add(request))
    }

    pub fn remote_remove(
        &self,
        request: impl tonic::IntoRequest<RemoteRemoveReq>,
    ) -> Result<tonic::Response<RemoteRemoveReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.remote_remove(request))
    }

    pub fn remote_show(
        &self,
        request: impl tonic::IntoRequest<RemoteShowReq>,
    ) -> Result<tonic::Response<RemoteShowReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.remote_show(request))
    }

    // ---- M12: managed-workspace RPCs ---------------------------------

    /// Calls the proto `Clone` RPC. Named `clone_repo` to avoid
    /// collision with the `Clone` trait method.
    pub fn clone_repo(
        &self,
        request: impl tonic::IntoRequest<CloneReq>,
    ) -> Result<tonic::Response<CloneReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        // The generated tonic method is `clone(&mut self, ...)` which
        // shadows `Clone::clone` on the MutexGuard. Explicitly deref
        // to the concrete type so the inherent method wins.
        let c: &mut JujutsuInterfaceClient<tonic::transport::Channel> = &mut client;
        rt.block_on(c.clone(request))
    }

    pub fn repo_list(
        &self,
        request: impl tonic::IntoRequest<RepoListReq>,
    ) -> Result<tonic::Response<RepoListReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.repo_list(request))
    }

    pub fn workspace_create(
        &self,
        request: impl tonic::IntoRequest<WorkspaceCreateReq>,
    ) -> Result<tonic::Response<WorkspaceCreateReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.workspace_create(request))
    }

    pub fn workspace_finalize(
        &self,
        request: impl tonic::IntoRequest<WorkspaceFinalizeReq>,
    ) -> Result<tonic::Response<WorkspaceFinalizeReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.workspace_finalize(request))
    }

    pub fn workspace_list(
        &self,
        request: impl tonic::IntoRequest<WorkspaceListReq>,
    ) -> Result<tonic::Response<WorkspaceListReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.workspace_list(request))
    }

    pub fn workspace_delete(
        &self,
        request: impl tonic::IntoRequest<WorkspaceDeleteReq>,
    ) -> Result<tonic::Response<WorkspaceDeleteReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.workspace_delete(request))
    }

    pub fn repo_delete(
        &self,
        request: impl tonic::IntoRequest<RepoDeleteReq>,
    ) -> Result<tonic::Response<RepoDeleteReply>, tonic::Status> {
        let mut client = self.client.lock().unwrap();
        let rt = self.rt.lock().unwrap();
        rt.block_on(client.repo_delete(request))
    }
}
