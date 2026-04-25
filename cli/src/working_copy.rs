use std::cell::OnceCell;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use jj_lib::backend::TreeId;
use jj_lib::commit::Commit;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use jj_lib::store::Store;
use jj_lib::working_copy::{
    CheckoutError, CheckoutStats, LockedWorkingCopy, ResetError, SnapshotError, SnapshotOptions,
    SnapshotStats, WorkingCopy, WorkingCopyFactory, WorkingCopyStateError,
};
use proto::jj_interface::{GetCheckoutStateReq, GetTreeStateReq, SnapshotReq};
use tracing::{info, warn};

use crate::blocking_client::BlockingJujutsuInterfaceClient;

pub struct YakWorkingCopyFactory {}

impl WorkingCopyFactory for YakWorkingCopyFactory {
    fn init_working_copy(
        &self,
        store: Arc<Store>,
        working_copy_path: PathBuf,
        _state_path: PathBuf,
        operation_id: OperationId,
        workspace_name: WorkspaceNameBuf,
        settings: &UserSettings,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        Ok(Box::new(YakWorkingCopy::init(
            store,
            working_copy_path,
            operation_id,
            workspace_name,
            settings,
        )?))
    }

    fn load_working_copy(
        &self,
        store: Arc<Store>,
        working_copy_path: PathBuf,
        _state_path: PathBuf,
        settings: &UserSettings,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        Ok(Box::new(YakWorkingCopy::load(
            store,
            working_copy_path,
            settings,
        )))
    }
}

pub struct YakWorkingCopy {
    store: Arc<Store>,
    working_copy_path: PathBuf,
    client: BlockingJujutsuInterfaceClient,
    /// Only access through get_checkout_state
    checkout_state: OnceCell<CheckoutState>,
    tree_state: OnceCell<MergedTree>,
}

impl YakWorkingCopy {
    pub fn name() -> &'static str {
        "yak"
    }

    fn connect_client(settings: &UserSettings) -> BlockingJujutsuInterfaceClient {
        // Pull the daemon port from user settings (matches YakBackend); the
        // integration test harness assigns a random port per env, so this
        // must not be hardcoded.
        let grpc_port = settings.get::<usize>("grpc_port").unwrap();
        BlockingJujutsuInterfaceClient::connect(format!("http://[::1]:{grpc_port}"))
            .expect("connect to yak daemon")
    }

    fn init(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        operation_id: OperationId,
        workspace_name: WorkspaceNameBuf,
        settings: &UserSettings,
    ) -> Result<Self, WorkingCopyStateError> {
        let client = Self::connect_client(settings);
        client
            .set_checkout_state(proto::jj_interface::SetCheckoutStateReq {
                working_copy_path: working_copy_path.to_str().unwrap().to_string(),
                checkout_state: Some(proto::jj_interface::CheckoutState {
                    op_id: operation_id.as_bytes().into(),
                    workspace_id: workspace_name.as_str().as_bytes().to_vec(),
                }),
            })
            .unwrap();
        Ok(YakWorkingCopy {
            store,
            working_copy_path,
            client,
            checkout_state: OnceCell::new(),
            tree_state: OnceCell::new(),
        })
    }

    fn load(store: Arc<Store>, working_copy_path: PathBuf, settings: &UserSettings) -> Self {
        let client = Self::connect_client(settings);
        YakWorkingCopy {
            store,
            working_copy_path,
            client,
            checkout_state: OnceCell::new(),
            tree_state: OnceCell::new(),
        }
    }
}

/// Working copy state stored in "checkout" file.
#[derive(Clone, Debug)]
struct CheckoutState {
    operation_id: OperationId,
    workspace_name: WorkspaceNameBuf,
}

impl YakWorkingCopy {
    fn get_tree(&self) -> &MergedTree {
        self.tree_state.get_or_init(|| {
            let tree_state = self
                .client
                .get_tree_state(GetTreeStateReq {
                    working_copy_path: self.working_copy_path.to_str().unwrap().to_string(),
                })
                .unwrap()
                .into_inner();
            MergedTree::resolved(self.store.clone(), TreeId::new(tree_state.tree_id))
        })
    }

    fn get_checkout_state(&self) -> &CheckoutState {
        self.checkout_state.get_or_init(|| {
            let checkout_state = self
                .client
                .get_checkout_state(GetCheckoutStateReq {
                    working_copy_path: self.working_copy_path.to_str().unwrap().to_string(),
                })
                .unwrap()
                .into_inner();
            let workspace_name = std::str::from_utf8(&checkout_state.workspace_id)
                .expect("daemon returned non-UTF-8 workspace name")
                .to_string();
            CheckoutState {
                operation_id: OperationId::new(checkout_state.op_id),
                workspace_name: WorkspaceNameBuf::from(workspace_name),
            }
        })
    }

    fn get_working_copy_lock(&self) -> DaemonLock {
        DaemonLock::new()
    }

    fn snapshot_via_daemon(&mut self) -> MergedTree {
        let tree_state = self
            .client
            .snapshot(SnapshotReq {
                working_copy_path: self.working_copy_path.to_str().unwrap().to_string(),
            })
            .unwrap()
            .into_inner();
        MergedTree::resolved(self.store.clone(), TreeId::new(tree_state.tree_id))
    }
}

/// Distributed lock. The daemon hold the lock since all work
/// is done in it.
struct DaemonLock {}
impl DaemonLock {
    pub fn new() -> Self {
        warn!("DaemonLock is unimplemented. No locking currently done.");
        DaemonLock {}
    }
}

impl WorkingCopy for YakWorkingCopy {
    fn name(&self) -> &str {
        Self::name()
    }

    fn workspace_name(&self) -> &WorkspaceName {
        &self.get_checkout_state().workspace_name
    }

    fn operation_id(&self) -> &OperationId {
        &self.get_checkout_state().operation_id
    }

    fn tree(&self) -> Result<&MergedTree, WorkingCopyStateError> {
        Ok(self.get_tree())
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        todo!()
    }

    fn start_mutation(&self) -> Result<Box<dyn LockedWorkingCopy>, WorkingCopyStateError> {
        info!("Starting mutation");
        let lock = self.get_working_copy_lock();
        let wc = YakWorkingCopy {
            client: self.client.clone(),
            store: self.store.clone(),
            working_copy_path: self.working_copy_path.clone(),
            checkout_state: OnceCell::new(),
            tree_state: OnceCell::new(),
        };
        let old_operation_id = wc.operation_id().clone();
        // Force the tree to be loaded so old_tree() can return a borrow.
        let _ = wc.tree()?;
        Ok(Box::new(LockedYakWorkingCopy {
            wc,
            lock,
            old_operation_id,
        }))
    }
}

struct LockedYakWorkingCopy {
    wc: YakWorkingCopy,
    #[allow(dead_code)]
    lock: DaemonLock,
    old_operation_id: OperationId,
}

#[async_trait]
impl LockedWorkingCopy for LockedYakWorkingCopy {
    fn old_operation_id(&self) -> &OperationId {
        &self.old_operation_id
    }

    fn old_tree(&self) -> &MergedTree {
        // tree was forced to load in start_mutation.
        self.wc
            .tree_state
            .get()
            .expect("old_tree called before tree was loaded in start_mutation")
    }

    async fn recover(&mut self, _commit: &Commit) -> Result<(), ResetError> {
        todo!()
    }

    async fn snapshot(
        &mut self,
        _options: &SnapshotOptions,
    ) -> Result<(MergedTree, SnapshotStats), SnapshotError> {
        let tree = self.wc.snapshot_via_daemon();
        Ok((tree, SnapshotStats::default()))
    }

    async fn check_out(&mut self, commit: &Commit) -> Result<CheckoutStats, CheckoutError> {
        let _new_tree = commit.tree();
        todo!()
    }

    fn rename_workspace(&mut self, _new_workspace_name: WorkspaceNameBuf) {
        todo!()
    }

    async fn reset(&mut self, _commit: &Commit) -> Result<(), ResetError> {
        todo!()
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        todo!()
    }

    async fn set_sparse_patterns(
        &mut self,
        _new_sparse_patterns: Vec<RepoPathBuf>,
    ) -> Result<CheckoutStats, CheckoutError> {
        todo!()
    }

    async fn finish(
        self: Box<Self>,
        operation_id: OperationId,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        info!("Finished: {operation_id:?}");
        Ok(Box::new(self.wc))
    }
}
