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

#[derive(Clone)]
struct Session {
    remote: String,
    path: String,
}

pub struct JujutsuService {
    store: Store,
    sessions: Arc<Mutex<Vec<Session>>>,
}

impl JujutsuService {
    pub fn new() -> jujutsu_interface_server::JujutsuInterfaceServer<Self> {
        jujutsu_interface_server::JujutsuInterfaceServer::new(JujutsuService {
            store: Store::new(),
            sessions: Arc::new(Mutex::new(vec![])),
        })
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
        let mut sessions = self.sessions.lock().await;
        sessions.push(Session {
            remote: req.remote,
            path: req.path,
        });
        Ok(Response::new(InitializeReply {}))
    }

    #[tracing::instrument(skip(self))]
    async fn daemon_status(
        &self,
        request: Request<DaemonStatusReq>,
    ) -> Result<Response<DaemonStatusReply>, Status> {
        let _req = request.into_inner();
        let sessions = self.sessions.lock().await;
        let data = sessions
            .clone()
            .into_iter()
            .map(|sess| proto::jj_interface::daemon_status_reply::Data {
                path: sess.path,
                remote: sess.remote,
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
        info!("Getting tree state");
        let _req = request.into_inner();
        todo!()
    }

    #[tracing::instrument(skip(self))]
    async fn get_checkout_state(
        &self,
        request: Request<GetCheckoutStateReq>,
    ) -> Result<Response<CheckoutState>, Status> {
        info!("Getting checkout state");
        let _req = request.into_inner();
        todo!()
    }

    #[tracing::instrument(skip(self))]
    async fn set_checkout_state(
        &self,
        request: Request<SetCheckoutStateReq>,
    ) -> Result<Response<SetCheckoutStateReply>, Status> {
        let _req = request.into_inner();
        todo!()
    }

    #[tracing::instrument(skip(self))]
    async fn snapshot(
        &self,
        request: Request<SnapshotReq>,
    ) -> Result<Response<SnapshotReply>, Status> {
        let _req = request.into_inner();
        todo!()
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
        let svc = JujutsuService {
            store: Store::new(),
            sessions: Arc::new(Mutex::new(vec![])),
        };
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
}
