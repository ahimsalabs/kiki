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
    pub fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
    where
        D: TryInto<tonic::transport::Endpoint>,
        D::Error: Into<StdError>,
    {
        let rt = Builder::new_multi_thread().enable_all().build().unwrap();
        let client = Arc::new(Mutex::new(
            rt.block_on(JujutsuInterfaceClient::connect(dst))?,
        ));
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
    // proto/jj_interface.proto). YakBackend stamps it from its own
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
    // §10.5.2 decision 1). `YakOpHeadsStore` (cli/src/op_heads_store.rs)
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

    // Unused by the only M10.5 consumer (`YakOpHeadsStore`) since the
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
}
