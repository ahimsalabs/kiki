use std::io::Cursor;
use std::path::Path;
use std::pin::Pin;
use std::time::SystemTime;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use jj_lib::backend::{
    Backend, BackendError, BackendInitError, BackendResult, ChangeId, Commit, CommitId,
    CopyHistory, CopyId, CopyRecord, FileId, MillisSinceEpoch, RelatedCopy, SecureSig, Signature,
    SigningFn, SymlinkId, Timestamp, Tree, TreeId, TreeValue, make_root_commit,
};
use jj_lib::index::Index;
use jj_lib::merge::MergeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::repo_path::{RepoPath, RepoPathBuf, RepoPathComponentBuf};
use jj_lib::settings::UserSettings;
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::blocking_client::BlockingJujutsuInterfaceClient;

const COMMIT_ID_LENGTH: usize = 32;
const CHANGE_ID_LENGTH: usize = 16;

#[derive(Debug)]
pub struct YakBackend {
    client: BlockingJujutsuInterfaceClient,
    root_commit_id: CommitId,
    root_change_id: ChangeId,
    empty_tree_id: TreeId,
}

impl YakBackend {
    pub const fn name() -> &'static str {
        "yak"
    }

    pub fn new(settings: &UserSettings, _store_path: &Path) -> Result<Self, BackendInitError> {
        let root_commit_id = CommitId::from_bytes(&[0; COMMIT_ID_LENGTH]);
        let root_change_id = ChangeId::from_bytes(&[0; CHANGE_ID_LENGTH]);
        let grpc_port = settings.get::<usize>("grpc_port").unwrap();

        let client = crate::blocking_client::BlockingJujutsuInterfaceClient::connect(format!(
            "http://[::1]:{grpc_port}"
        ))
        .unwrap();
        let empty_tree_id =
            TreeId::from_bytes(&client.get_empty_tree_id().unwrap().into_inner().tree_id);

        Ok(YakBackend {
            client,
            root_commit_id,
            root_change_id,
            empty_tree_id,
        })
    }
}

#[async_trait]
impl Backend for YakBackend {
    fn name(&self) -> &str {
        Self::name()
    }

    fn commit_id_length(&self) -> usize {
        COMMIT_ID_LENGTH
    }

    fn change_id_length(&self) -> usize {
        CHANGE_ID_LENGTH
    }

    fn root_commit_id(&self) -> &CommitId {
        &self.root_commit_id
    }

    fn root_change_id(&self) -> &ChangeId {
        &self.root_change_id
    }

    fn empty_tree_id(&self) -> &TreeId {
        &self.empty_tree_id
    }

    fn concurrency(&self) -> usize {
        1
    }

    async fn read_file(
        &self,
        _path: &RepoPath,
        id: &FileId,
    ) -> BackendResult<Pin<Box<dyn AsyncRead + Send>>> {
        let proto = self
            .client
            .read_file(file_id_to_proto(id))
            .unwrap()
            .into_inner();
        let mut decoded = Vec::new();
        zstd::stream::copy_decode(proto.data.as_slice(), &mut decoded)
            .map_err(|e| BackendError::Other(e.into()))?;
        Ok(Box::pin(Cursor::new(decoded)))
    }

    async fn write_file(
        &self,
        _path: &RepoPath,
        contents: &mut (dyn AsyncRead + Send + Unpin),
    ) -> BackendResult<FileId> {
        let mut buf = Vec::new();
        contents
            .read_to_end(&mut buf)
            .await
            .map_err(|e| BackendError::Other(e.into()))?;
        let mut encoded = Vec::new();
        zstd::stream::copy_encode(buf.as_slice(), &mut encoded, 0)
            .map_err(|e| BackendError::Other(e.into()))?;
        let proto = proto::jj_interface::File { data: encoded };
        let id = self.client.write_file(proto).unwrap().into_inner();
        Ok(FileId::new(id.file_id))
    }

    async fn read_symlink(&self, _path: &RepoPath, id: &SymlinkId) -> BackendResult<String> {
        let proto = self
            .client
            .read_symlink(symlink_id_to_proto(id))
            .unwrap()
            .into_inner();
        Ok(proto.target)
    }

    async fn write_symlink(&self, _path: &RepoPath, target: &str) -> BackendResult<SymlinkId> {
        let proto = proto::jj_interface::Symlink {
            target: target.to_string(),
        };
        let id = self.client.write_symlink(proto).unwrap().into_inner();
        Ok(SymlinkId::new(id.symlink_id))
    }

    // Copy tracking is not supported by yak yet.
    async fn read_copy(&self, _id: &CopyId) -> BackendResult<CopyHistory> {
        Err(BackendError::Unsupported(
            "yak backend does not support copy tracking".into(),
        ))
    }

    async fn write_copy(&self, _contents: &CopyHistory) -> BackendResult<CopyId> {
        Err(BackendError::Unsupported(
            "yak backend does not support copy tracking".into(),
        ))
    }

    async fn get_related_copies(&self, _copy_id: &CopyId) -> BackendResult<Vec<RelatedCopy>> {
        Err(BackendError::Unsupported(
            "yak backend does not support copy tracking".into(),
        ))
    }

    #[tracing::instrument]
    async fn read_tree(&self, _path: &RepoPath, id: &TreeId) -> BackendResult<Tree> {
        let proto = self
            .client
            .read_tree(tree_id_to_proto(id))
            .unwrap()
            .into_inner();
        Ok(tree_from_proto(proto))
    }

    #[tracing::instrument]
    async fn write_tree(&self, _path: &RepoPath, tree: &Tree) -> BackendResult<TreeId> {
        let proto = tree_to_proto(tree);
        let id = self.client.write_tree(proto).unwrap().into_inner();
        Ok(TreeId::new(id.tree_id))
    }

    #[tracing::instrument]
    async fn read_commit(&self, id: &CommitId) -> BackendResult<Commit> {
        if *id == self.root_commit_id {
            return Ok(make_root_commit(
                self.root_change_id().clone(),
                self.empty_tree_id.clone(),
            ));
        }
        let proto = self
            .client
            .read_commit(commit_id_to_proto(id))
            .unwrap()
            .into_inner();
        Ok(commit_from_proto(proto))
    }

    #[tracing::instrument(skip(sign_with))]
    async fn write_commit(
        &self,
        commit: Commit,
        sign_with: Option<&mut SigningFn>,
    ) -> BackendResult<(CommitId, Commit)> {
        assert!(commit.secure_sig.is_none(), "commit.secure_sig was set");
        assert!(sign_with.is_none(), "sign_with was set");

        if commit.parents.is_empty() {
            return Err(BackendError::Other(
                "Cannot write a commit with no parents".into(),
            ));
        }
        let proto = commit_to_proto(&commit);
        let id = self.client.write_commit(proto).unwrap().into_inner();
        Ok((CommitId::new(id.commit_id), commit))
    }

    fn gc(&self, _index: &dyn Index, _keep_newer: SystemTime) -> BackendResult<()> {
        Ok(())
    }

    #[tracing::instrument]
    fn get_copy_records(
        &self,
        _paths: Option<&[RepoPathBuf]>,
        _root: &CommitId,
        _head: &CommitId,
    ) -> BackendResult<BoxStream<'_, BackendResult<CopyRecord>>> {
        Ok(Box::pin(stream::empty()))
    }
}

// ---------- proto conversions ----------

pub fn file_id_to_proto(file_id: &FileId) -> proto::jj_interface::FileId {
    proto::jj_interface::FileId {
        file_id: file_id.to_bytes(),
    }
}

pub fn commit_id_to_proto(commit_id: &CommitId) -> proto::jj_interface::CommitId {
    proto::jj_interface::CommitId {
        commit_id: commit_id.to_bytes(),
    }
}

pub fn tree_id_to_proto(tree_id: &TreeId) -> proto::jj_interface::TreeId {
    proto::jj_interface::TreeId {
        tree_id: tree_id.to_bytes(),
    }
}

pub fn symlink_id_to_proto(symlink_id: &SymlinkId) -> proto::jj_interface::SymlinkId {
    proto::jj_interface::SymlinkId {
        symlink_id: symlink_id.to_bytes(),
    }
}

pub fn commit_to_proto(commit: &Commit) -> proto::jj_interface::Commit {
    let mut proto = proto::jj_interface::Commit::default();
    for parent in &commit.parents {
        proto.parents.push(parent.to_bytes());
    }
    for predecessor in &commit.predecessors {
        proto.predecessors.push(predecessor.to_bytes());
    }
    // Commit::root_tree is now Merge<TreeId>; serialize as a flat list of bytes.
    // The "uses_tree_conflict_format" field is now redundant (everything is a
    // merge) but we keep it set for forward-compatibility with older readers.
    proto.uses_tree_conflict_format = true;
    proto.root_tree = commit.root_tree.iter().map(|id| id.to_bytes()).collect();
    proto.change_id = commit.change_id.to_bytes();
    proto.description = commit.description.clone();
    proto.author = Some(signature_to_proto(&commit.author));
    proto.committer = Some(signature_to_proto(&commit.committer));
    proto
}

fn commit_from_proto(mut proto: proto::jj_interface::Commit) -> Commit {
    // Note: .take() sets secure_sig to None before encoding, mirroring the
    // approach in jj's GitBackend.
    let secure_sig = proto.secure_sig.take().map(|sig| SecureSig {
        data: proto.encode_to_vec(),
        sig,
    });

    let parents = proto.parents.into_iter().map(CommitId::new).collect();
    let predecessors = proto.predecessors.into_iter().map(CommitId::new).collect();
    let merge_builder: MergeBuilder<TreeId> =
        proto.root_tree.into_iter().map(TreeId::new).collect();
    let root_tree = merge_builder.build();
    let change_id = ChangeId::new(proto.change_id);
    Commit {
        parents,
        predecessors,
        root_tree,
        // Yak doesn't track conflict labels (yet); hand jj a resolved-empty merge.
        conflict_labels: jj_lib::merge::Merge::resolved(String::new()),
        change_id,
        description: proto.description,
        author: signature_from_proto(proto.author.unwrap_or_default()),
        committer: signature_from_proto(proto.committer.unwrap_or_default()),
        secure_sig,
    }
}

fn signature_to_proto(signature: &Signature) -> proto::jj_interface::commit::Signature {
    proto::jj_interface::commit::Signature {
        name: signature.name.clone(),
        email: signature.email.clone(),
        timestamp: Some(proto::jj_interface::commit::Timestamp {
            millis_since_epoch: signature.timestamp.timestamp.0,
            tz_offset: signature.timestamp.tz_offset,
        }),
    }
}

fn signature_from_proto(proto: proto::jj_interface::commit::Signature) -> Signature {
    let timestamp = proto.timestamp.unwrap_or_default();
    Signature {
        name: proto.name,
        email: proto.email,
        timestamp: Timestamp {
            timestamp: MillisSinceEpoch(timestamp.millis_since_epoch),
            tz_offset: timestamp.tz_offset,
        },
    }
}

fn tree_to_proto(tree: &Tree) -> proto::jj_interface::Tree {
    let mut proto = proto::jj_interface::Tree::default();
    for entry in tree.entries() {
        proto.entries.push(proto::jj_interface::tree::Entry {
            name: entry.name().as_internal_str().to_owned(),
            value: Some(tree_value_to_proto(entry.value())),
        });
    }
    proto
}

fn tree_value_to_proto(value: &TreeValue) -> proto::jj_interface::TreeValue {
    let mut proto = proto::jj_interface::TreeValue::default();
    match value {
        // Yak doesn't track copy histories yet; copy_id is dropped on the wire
        // and rehydrated as a placeholder on read.
        TreeValue::File {
            id,
            executable,
            copy_id: _,
        } => {
            proto.value = Some(proto::jj_interface::tree_value::Value::File(
                proto::jj_interface::tree_value::File {
                    id: id.to_bytes(),
                    executable: *executable,
                },
            ));
        }
        TreeValue::Symlink(id) => {
            proto.value = Some(proto::jj_interface::tree_value::Value::SymlinkId(
                id.to_bytes(),
            ));
        }
        TreeValue::GitSubmodule(_id) => {
            panic!("cannot store git submodules");
        }
        TreeValue::Tree(id) => {
            proto.value = Some(proto::jj_interface::tree_value::Value::TreeId(
                id.to_bytes(),
            ));
        }
    }
    proto
}

fn tree_from_proto(proto: proto::jj_interface::Tree) -> Tree {
    // Tree entries must be sorted by name for `Tree::from_sorted_entries`.
    // The daemon emits entries in insertion order, so we sort here defensively.
    let mut entries: Vec<(RepoPathComponentBuf, TreeValue)> = proto
        .entries
        .into_iter()
        .map(|e| {
            (
                RepoPathComponentBuf::new(e.name).expect("invalid path component from daemon"),
                tree_value_from_proto(e.value.unwrap()),
            )
        })
        .collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    Tree::from_sorted_entries(entries)
}

fn tree_value_from_proto(proto: proto::jj_interface::TreeValue) -> TreeValue {
    match proto.value.unwrap() {
        proto::jj_interface::tree_value::Value::TreeId(id) => TreeValue::Tree(TreeId::new(id)),
        proto::jj_interface::tree_value::Value::File(proto::jj_interface::tree_value::File {
            id,
            executable,
        }) => TreeValue::File {
            id: FileId::new(id),
            executable,
            copy_id: CopyId::placeholder(),
        },
        proto::jj_interface::tree_value::Value::SymlinkId(id) => {
            TreeValue::Symlink(SymlinkId::new(id))
        }
        // jj-lib 0.40 dropped TreeValue::Conflict from the trait surface; the
        // wire type still carries it for legacy data, but yak should never
        // produce it.
        proto::jj_interface::tree_value::Value::ConflictId(_) => {
            panic!("yak backend: stored conflict_id no longer supported by jj")
        }
    }
}
