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
use proto::jj_interface::{CheckOutReq, GetCheckoutStateReq, GetTreeStateReq, SnapshotReq};
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

/// The "all files" sparse pattern. Yak doesn't support sparse
/// checkouts — the daemon always materializes the full tree — so
/// this is the only pattern we ever return.
static FULL_SPARSE: std::sync::LazyLock<Vec<RepoPathBuf>> =
    std::sync::LazyLock::new(|| vec![RepoPathBuf::root()]);

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
        Ok(&FULL_SPARSE)
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

    async fn recover(&mut self, commit: &Commit) -> Result<(), ResetError> {
        // Recovery re-roots the working copy at the given commit,
        // same as reset. The daemon doesn't distinguish between the
        // two operations.
        self.reset(commit).await
    }

    async fn snapshot(
        &mut self,
        _options: &SnapshotOptions,
    ) -> Result<(MergedTree, SnapshotStats), SnapshotError> {
        let tree = self.wc.snapshot_via_daemon()?;
        Ok((tree, SnapshotStats::default()))
    }

    async fn check_out(&mut self, commit: &Commit) -> Result<CheckoutStats, CheckoutError> {
        // Yak only supports unconflicted checkouts today: the daemon's
        // VFS roots at a single tree id, so a Merge<TreeId> with
        // multiple terms has no obvious materialization. Conflict
        // rendering pairs with the conflict UI work — punt for now and
        // surface a clean error rather than picking a side silently.
        let new_tree = commit.tree();
        let resolved_tree_id = new_tree.tree_ids().as_resolved().ok_or_else(|| {
            CheckoutError::Other {
                message: "yak: checking out a conflicted tree is not yet supported".into(),
                err: std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "conflicted MergedTree",
                )
                .into(),
            }
        })?;

        // Stamp the new root tree on the daemon. The daemon validates
        // the tree exists in its per-mount Store (it must — jj-lib
        // wrote it via `Backend::write_tree` before reaching here) and
        // re-roots the VFS so subsequent reads through the mount see
        // the new tree. CheckoutStats stays default until the VFS
        // write path (M6) gives us a real tree-diff to count.
        let path_str = path_to_str(&self.wc.working_copy_path)?.to_string();
        self.wc
            .client
            .check_out(CheckOutReq {
                working_copy_path: path_str,
                new_tree_id: resolved_tree_id.to_bytes(),
            })
            .map_err(|e| CheckoutError::Other {
                message: "daemon CheckOut RPC failed".into(),
                err: e.into(),
            })?;

        // Invalidate the cached MergedTree so subsequent `tree()` calls
        // refetch via GetTreeState. Without this, the OnceCell would
        // still hand back the pre-checkout tree until the next
        // `start_mutation`.
        self.wc.tree_state = OnceCell::new();

        Ok(CheckoutStats::default())
    }

    fn rename_workspace(&mut self, _new_workspace_name: WorkspaceNameBuf) {
        todo!()
    }

    async fn reset(&mut self, commit: &Commit) -> Result<(), ResetError> {
        // `reset` re-roots the working copy at the given commit's tree,
        // discarding any pending mutations. For yak this is the same
        // operation as `check_out`: tell the daemon to swap the VFS
        // root tree.
        let new_tree = commit.tree();
        let resolved_tree_id = new_tree.tree_ids().as_resolved().ok_or_else(|| {
            ResetError::Other {
                message: "yak: resetting to a conflicted tree is not yet supported".into(),
                err: std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "conflicted MergedTree",
                )
                .into(),
            }
        })?;
        let path_str = path_to_str(&self.wc.working_copy_path)?.to_string();
        self.wc
            .client
            .check_out(CheckOutReq {
                working_copy_path: path_str,
                new_tree_id: resolved_tree_id.to_bytes(),
            })
            .map_err(|e| ResetError::Other {
                message: "daemon CheckOut RPC failed during reset".into(),
                err: e.into(),
            })?;
        self.wc.tree_state = OnceCell::new();
        Ok(())
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        Ok(&FULL_SPARSE)
    }

    async fn set_sparse_patterns(
        &mut self,
        _new_sparse_patterns: Vec<RepoPathBuf>,
    ) -> Result<CheckoutStats, CheckoutError> {
        Err(CheckoutError::Other {
            message: "yak: sparse checkouts are not supported".into(),
            err: std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "sparse patterns",
            )
            .into(),
        })
    }

    async fn finish(
        mut self: Box<Self>,
        operation_id: OperationId,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        info!("Finished: {operation_id:?}");
        // Persist the new operation id to the daemon so the next CLI
        // invocation's `GetCheckoutState` returns it. The local-disk
        // working copy writes a `.jj/working_copy/checkout` file at
        // this point; the daemon-backed equivalent is SetCheckoutState.
        // Without this, `WorkingCopy::operation_id()` keeps reporting
        // the pre-mutation op id and `jj log`'s `@` marker stays
        // pinned to the previous WC commit (PLAN §10.2).
        let workspace_name = self.wc.get_checkout_state()?.workspace_name.clone();
        let path_str = path_to_str(&self.wc.working_copy_path)?.to_string();
        self.wc
            .client
            .set_checkout_state(proto::jj_interface::SetCheckoutStateReq {
                working_copy_path: path_str,
                checkout_state: Some(proto::jj_interface::CheckoutState {
                    op_id: operation_id.as_bytes().into(),
                    workspace_id: workspace_name.as_str().as_bytes().to_vec(),
                }),
            })
            .map_err(|e| wc_state_err("daemon SetCheckoutState failed", e))?;
        // Invalidate the cached checkout_state so subsequent reads via
        // `operation_id()` don't keep returning the stale value.
        self.wc.checkout_state = OnceCell::new();
        Ok(Box::new(self.wc))
    }
}
