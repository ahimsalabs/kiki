use std::cell::OnceCell;
use std::path::{Path, PathBuf};
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

fn wc_state_err(
    message: impl Into<String>,
    err: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> WorkingCopyStateError {
    WorkingCopyStateError {
        message: message.into(),
        err: err.into(),
    }
}

/// Working-copy paths must be UTF-8 because they cross the proto boundary as
/// `string`. Non-UTF-8 paths (possible on Linux) need to be rejected up front
/// instead of `unwrap()`ing inside RPC builders.
fn path_to_str(path: &Path) -> Result<&str, WorkingCopyStateError> {
    path.to_str().ok_or_else(|| {
        wc_state_err(
            format!("working copy path is not valid UTF-8: {}", path.display()),
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-UTF-8 path"),
        )
    })
}

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
        )?))
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

    fn connect_client(
        settings: &UserSettings,
    ) -> Result<BlockingJujutsuInterfaceClient, WorkingCopyStateError> {
        // Pull the daemon port from user settings (matches YakBackend); the
        // integration test harness assigns a random port per env, so this
        // must not be hardcoded.
        let grpc_port = settings
            .get::<usize>("grpc_port")
            .map_err(|e| wc_state_err("grpc_port not configured", e))?;
        BlockingJujutsuInterfaceClient::connect(format!("http://[::1]:{grpc_port}"))
            .map_err(|e| wc_state_err("failed to connect to yak daemon", e))
    }

    fn init(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        operation_id: OperationId,
        workspace_name: WorkspaceNameBuf,
        settings: &UserSettings,
    ) -> Result<Self, WorkingCopyStateError> {
        let client = Self::connect_client(settings)?;
        let path_str = path_to_str(&working_copy_path)?.to_string();
        client
            .set_checkout_state(proto::jj_interface::SetCheckoutStateReq {
                working_copy_path: path_str,
                checkout_state: Some(proto::jj_interface::CheckoutState {
                    op_id: operation_id.as_bytes().into(),
                    workspace_id: workspace_name.as_str().as_bytes().to_vec(),
                }),
            })
            .map_err(|e| wc_state_err("daemon SetCheckoutState failed", e))?;
        Ok(YakWorkingCopy {
            store,
            working_copy_path,
            client,
            checkout_state: OnceCell::new(),
            tree_state: OnceCell::new(),
        })
    }

    fn load(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        settings: &UserSettings,
    ) -> Result<Self, WorkingCopyStateError> {
        // Reject non-UTF-8 paths up front so subsequent RPC builders don't
        // need to handle them.
        let _ = path_to_str(&working_copy_path)?;
        let client = Self::connect_client(settings)?;
        Ok(YakWorkingCopy {
            store,
            working_copy_path,
            client,
            checkout_state: OnceCell::new(),
            tree_state: OnceCell::new(),
        })
    }
}

/// Working copy state stored in "checkout" file.
#[derive(Clone, Debug)]
struct CheckoutState {
    operation_id: OperationId,
    workspace_name: WorkspaceNameBuf,
}

impl YakWorkingCopy {
    fn get_tree(&self) -> Result<&MergedTree, WorkingCopyStateError> {
        // `OnceCell::get_or_try_init` is unstable, so we manually populate the
        // cell. Single-threaded, so no race window.
        if self.tree_state.get().is_none() {
            let path_str = path_to_str(&self.working_copy_path)?.to_string();
            let tree_state = self
                .client
                .get_tree_state(GetTreeStateReq {
                    working_copy_path: path_str,
                })
                .map_err(|e| wc_state_err("daemon GetTreeState failed", e))?
                .into_inner();
            let tree =
                MergedTree::resolved(self.store.clone(), TreeId::new(tree_state.tree_id));
            // Discard the Err that would mean another caller raced us; can't
            // happen here (single-threaded), but it would be harmless either
            // way (both load identical state).
            let _ = self.tree_state.set(tree);
        }
        Ok(self
            .tree_state
            .get()
            .expect("tree_state populated above"))
    }

    /// Trait-required `workspace_name`/`operation_id` are infallible; if the
    /// daemon RPC underneath fails we have no choice but to panic. Callers
    /// that can return errors should call this helper instead.
    fn get_checkout_state(&self) -> Result<&CheckoutState, WorkingCopyStateError> {
        if self.checkout_state.get().is_none() {
            let path_str = path_to_str(&self.working_copy_path)?.to_string();
            let checkout_state = self
                .client
                .get_checkout_state(GetCheckoutStateReq {
                    working_copy_path: path_str,
                })
                .map_err(|e| wc_state_err("daemon GetCheckoutState failed", e))?
                .into_inner();
            let workspace_name = std::str::from_utf8(&checkout_state.workspace_id)
                .map_err(|e| {
                    wc_state_err("daemon returned non-UTF-8 workspace name", e)
                })?
                .to_string();
            let _ = self.checkout_state.set(CheckoutState {
                operation_id: OperationId::new(checkout_state.op_id),
                workspace_name: WorkspaceNameBuf::from(workspace_name),
            });
        }
        Ok(self
            .checkout_state
            .get()
            .expect("checkout_state populated above"))
    }

    fn get_working_copy_lock(&self) -> DaemonLock {
        DaemonLock::new()
    }

    fn snapshot_via_daemon(&mut self) -> Result<MergedTree, SnapshotError> {
        // path_to_str returns WorkingCopyStateError, which converts to
        // SnapshotError via `#[from]`.
        let path_str = path_to_str(&self.working_copy_path)?.to_string();
        let tree_state = self
            .client
            .snapshot(SnapshotReq {
                working_copy_path: path_str,
            })
            .map_err(|e| SnapshotError::Other {
                message: "daemon Snapshot RPC failed".into(),
                err: e.into(),
            })?
            .into_inner();
        Ok(MergedTree::resolved(
            self.store.clone(),
            TreeId::new(tree_state.tree_id),
        ))
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
        // Trait-required infallible accessor; the daemon RPC underneath can
        // fail. Eagerly hydrate via `start_mutation` / `tree()` to avoid
        // panicking here. If we end up here without a populated cache, that's
        // a bug in the load path.
        &self
            .get_checkout_state()
            .expect("checkout state must be loaded before workspace_name()")
            .workspace_name
    }

    fn operation_id(&self) -> &OperationId {
        &self
            .get_checkout_state()
            .expect("checkout state must be loaded before operation_id()")
            .operation_id
    }

    fn tree(&self) -> Result<&MergedTree, WorkingCopyStateError> {
        self.get_tree()
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
        // Hydrate both lazy caches up front so the infallible accessors on
        // WorkingCopy / LockedWorkingCopy can borrow from them without
        // surprise panics.
        let old_operation_id = wc.get_checkout_state()?.operation_id.clone();
        let _ = wc.get_tree()?;
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
        let tree = self.wc.snapshot_via_daemon()?;
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
