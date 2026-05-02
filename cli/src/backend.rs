use std::{io::Cursor, path::Path, pin::Pin, time::SystemTime};

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use jj_lib::{
    backend::{
        make_root_commit, Backend, BackendError, BackendInitError, BackendResult, ChangeId, Commit,
        CommitId, CopyHistory, CopyId, CopyRecord, FileId, MillisSinceEpoch, RelatedCopy,
        SecureSig, Signature, SigningFn, SymlinkId, Timestamp, Tree, TreeId, TreeValue,
    },
    conflict_labels::ConflictLabels,
    index::Index,
    merge::MergeBuilder,
    object_id::ObjectId,
    repo_path::{RepoPath, RepoPathBuf, RepoPathComponentBuf},
    settings::UserSettings,
};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::blocking_client::BlockingJujutsuInterfaceClient;

const COMMIT_ID_LENGTH: usize = 20;
const CHANGE_ID_LENGTH: usize = 16;

#[derive(Debug)]
pub struct KikiBackend {
    client: BlockingJujutsuInterfaceClient,
    /// Stamped on every store RPC so the daemon can route to the right
    /// per-mount Store. Derived from `store_path` per the jj convention
    /// (`<wc>/.jj/repo/store`); see `derive_working_copy_path`.
    working_copy_path: String,
    root_commit_id: CommitId,
    root_change_id: ChangeId,
    empty_tree_id: TreeId,
}

impl KikiBackend {
    pub const fn name() -> &'static str {
        "kiki"
    }

    pub fn new(settings: &UserSettings, store_path: &Path) -> Result<Self, BackendInitError> {
        let root_commit_id = CommitId::from_bytes(&[0; COMMIT_ID_LENGTH]);
        let root_change_id = ChangeId::from_bytes(&[0; CHANGE_ID_LENGTH]);

        let working_copy_path = derive_working_copy_path(store_path)
            .map_err(BackendInitError)?;

        let client = crate::daemon_client::connect_or_start(settings)
            .map_err(BackendInitError)?;
        let empty_tree_id = TreeId::from_bytes(
            &client
                .get_empty_tree_id(working_copy_path.clone())
                .map_err(|e| BackendInitError(e.into()))?
                .into_inner()
                .tree_id,
        );

        Ok(KikiBackend {
            client,
            working_copy_path,
            root_commit_id,
            root_change_id,
            empty_tree_id,
        })
    }

    fn working_copy_path(&self) -> String {
        self.working_copy_path.clone()
    }
}

/// Reverse the jj convention `store_path == <wc>/.jj/repo/store` to recover
/// the workspace root. Brittle by design — if jj-lib ever reorganizes the
/// store layout this needs to be revisited (and ideally replaced with a
/// stamp file the backend writes at init time, similar to what
/// `GitBackend::init_colocated` does).
///
/// Always canonicalizes the result. The CLI canonicalizes `wc_path` before
/// sending `Initialize`, so the daemon's per-mount key is canonical. If
/// jj-lib ever hands us a non-canonical `store_path` (relative, symlinked,
/// or containing `..`) the un-canonicalized stamp would miss every Mount
/// and surface as a confusing `NotFound` on every store RPC. Canonicalizing
/// here costs one extra `realpath(2)` per backend init and removes that
/// failure mode.
fn derive_working_copy_path(
    store_path: &Path,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // store_path is `<wc>/.jj/repo/store`; nth(3) climbs four ancestors
    // (store, repo, .jj, wc) — `Path::ancestors` includes `self`.
    let wc = store_path.ancestors().nth(3).ok_or_else(|| {
        format!(
            "store_path {:?} is too shallow to derive workspace root",
            store_path
        )
    })?;
    let canonical = wc.canonicalize().map_err(|e| {
        format!(
            "failed to canonicalize workspace path {}: {e}",
            wc.display()
        )
    })?;
    let s = canonical.to_str().ok_or_else(|| {
        format!(
            "workspace path is not valid UTF-8: {}",
            canonical.display()
        )
    })?;
    Ok(s.to_owned())
}

#[async_trait]
impl Backend for KikiBackend {
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
        path: &RepoPath,
        id: &FileId,
    ) -> BackendResult<Pin<Box<dyn AsyncRead + Send>>> {
        let proto = self
            .client
            .read_file(proto::jj_interface::ReadFileReq {
                working_copy_path: self.working_copy_path(),
                file_id: id.to_bytes(),
            })
            .map_err(|e| read_file_err(path, id, e))?
            .into_inner();
        Ok(Box::pin(Cursor::new(proto.data)))
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
            .map_err(|e| write_err("file", e))?;
        let id = self
            .client
            .write_file(proto::jj_interface::WriteFileReq {
                working_copy_path: self.working_copy_path(),
                data: buf,
            })
            .map_err(|e| write_err("file", e))?
            .into_inner();
        Ok(FileId::new(id.file_id))
    }

    async fn read_symlink(&self, _path: &RepoPath, id: &SymlinkId) -> BackendResult<String> {
        let proto = self
            .client
            .read_symlink(proto::jj_interface::ReadSymlinkReq {
                working_copy_path: self.working_copy_path(),
                symlink_id: id.to_bytes(),
            })
            .map_err(|e| read_object_err("symlink", id, e))?
            .into_inner();
        Ok(proto.target)
    }

    async fn write_symlink(&self, _path: &RepoPath, target: &str) -> BackendResult<SymlinkId> {
        let id = self
            .client
            .write_symlink(proto::jj_interface::WriteSymlinkReq {
                working_copy_path: self.working_copy_path(),
                target: target.to_string(),
            })
            .map_err(|e| write_err("symlink", e))?
            .into_inner();
        Ok(SymlinkId::new(id.symlink_id))
    }

    // Copy tracking is not supported by kiki yet.
    async fn read_copy(&self, _id: &CopyId) -> BackendResult<CopyHistory> {
        Err(BackendError::Unsupported(
            "kiki backend does not support copy tracking".into(),
        ))
    }

    async fn write_copy(&self, _contents: &CopyHistory) -> BackendResult<CopyId> {
        Err(BackendError::Unsupported(
            "kiki backend does not support copy tracking".into(),
        ))
    }

    async fn get_related_copies(&self, _copy_id: &CopyId) -> BackendResult<Vec<RelatedCopy>> {
        Err(BackendError::Unsupported(
            "kiki backend does not support copy tracking".into(),
        ))
    }

    #[tracing::instrument]
    async fn read_tree(&self, _path: &RepoPath, id: &TreeId) -> BackendResult<Tree> {
        let proto = self
            .client
            .read_tree(proto::jj_interface::ReadTreeReq {
                working_copy_path: self.working_copy_path(),
                tree_id: id.to_bytes(),
            })
            .map_err(|e| read_object_err("tree", id, e))?
            .into_inner();
        tree_from_proto(proto)
    }

    #[tracing::instrument]
    async fn write_tree(&self, _path: &RepoPath, tree: &Tree) -> BackendResult<TreeId> {
        let proto = tree_to_proto(tree)?;
        let id = self
            .client
            .write_tree(proto::jj_interface::WriteTreeReq {
                working_copy_path: self.working_copy_path(),
                tree: Some(proto),
            })
            .map_err(|e| write_err("tree", e))?
            .into_inner();
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
            .read_commit(proto::jj_interface::ReadCommitReq {
                working_copy_path: self.working_copy_path(),
                commit_id: id.to_bytes(),
            })
            .map_err(|e| read_object_err("commit", id, e))?
            .into_inner();
        // Decode errors from the proto are themselves a "read commit"
        // failure — wrap them with the commit id so the user sees which
        // commit was malformed.
        commit_from_proto(proto).map_err(|e| read_object_err("commit", id, e))
    }

    #[tracing::instrument(skip(sign_with))]
    async fn write_commit(
        &self,
        commit: Commit,
        sign_with: Option<&mut SigningFn>,
    ) -> BackendResult<(CommitId, Commit)> {
        // Kiki does not yet support signing commits. Both invariants below would
        // be programmer errors at the call site rather than user-facing failures.
        assert!(commit.secure_sig.is_none(), "commit.secure_sig was set");
        assert!(sign_with.is_none(), "sign_with was set");

        if commit.parents.is_empty() {
            return Err(BackendError::Other(
                "Cannot write a commit with no parents".into(),
            ));
        }
        let proto = commit_to_proto(&commit);
        let id = self
            .client
            .write_commit(proto::jj_interface::WriteCommitReq {
                working_copy_path: self.working_copy_path(),
                commit: Some(proto),
            })
            .map_err(|e| write_err("commit", e))?
            .into_inner();
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

// Helpers for mapping internal failures (RPC, decompression, decode) onto
// jj's BackendError variants. The closures are tiny but show up at every RPC
// site, so the helpers cut down on noise without hiding the variant in use.
fn read_object_err<I: ObjectId>(
    object_type: &str,
    id: &I,
    source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> BackendError {
    BackendError::ReadObject {
        object_type: object_type.to_owned(),
        hash: id.hex(),
        source: source.into(),
    }
}

fn read_file_err(
    path: &RepoPath,
    id: &FileId,
    source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> BackendError {
    BackendError::ReadFile {
        path: path.to_owned(),
        id: id.clone(),
        source: source.into(),
    }
}

fn write_err(
    object_type: &'static str,
    source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> BackendError {
    BackendError::WriteObject {
        object_type,
        source: source.into(),
    }
}

// ---------- proto conversions ----------
//
// The pre-M4 wrapper helpers (`file_id_to_proto` etc.) are gone — IDs are
// passed inline as `bytes` on the new `Read*Req`/`Write*Req` types, so
// `id.to_bytes()` at the call site is enough.

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
    if !commit.conflict_labels.is_resolved() {
        proto.conflict_labels = commit.conflict_labels.as_slice().to_owned();
    }
    proto.change_id = commit.change_id.to_bytes();
    proto.description = commit.description.clone();
    proto.author = Some(signature_to_proto(&commit.author));
    proto.committer = Some(signature_to_proto(&commit.committer));
    proto
}

fn commit_from_proto(mut proto: proto::jj_interface::Commit) -> BackendResult<Commit> {
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
    let conflict_labels = ConflictLabels::from_vec(proto.conflict_labels);
    let change_id = ChangeId::new(proto.change_id);
    let author = proto
        .author
        .ok_or_else(|| commit_decode_err("missing author"))
        .and_then(signature_from_proto)?;
    let committer = proto
        .committer
        .ok_or_else(|| commit_decode_err("missing committer"))
        .and_then(signature_from_proto)?;
    Ok(Commit {
        parents,
        predecessors,
        root_tree,
        conflict_labels: conflict_labels.into_merge(),
        change_id,
        description: proto.description,
        author,
        committer,
        secure_sig,
    })
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

fn signature_from_proto(
    proto: proto::jj_interface::commit::Signature,
) -> BackendResult<Signature> {
    // Mirror the daemon-side `TryFrom` in `daemon/src/ty.rs`: a missing
    // timestamp is a malformed wire message, not an epoch-zero default.
    // Silently substituting `Default::default()` here would round-trip
    // commits as if they were authored on 1970-01-01.
    let timestamp = proto
        .timestamp
        .ok_or_else(|| commit_decode_err("missing signature timestamp"))?;
    Ok(Signature {
        name: proto.name,
        email: proto.email,
        timestamp: Timestamp {
            timestamp: MillisSinceEpoch(timestamp.millis_since_epoch),
            tz_offset: timestamp.tz_offset,
        },
    })
}

/// Build a `BackendError::Other` for malformed commit-proto fields.
/// We don't know the commit id at this layer (decode happens before
/// `read_commit` can wrap the error with `read_object_err`), so the
/// message has to stand on its own.
fn commit_decode_err(what: &'static str) -> BackendError {
    BackendError::Other(format!("commit proto: {what}").into())
}

fn tree_to_proto(tree: &Tree) -> BackendResult<proto::jj_interface::Tree> {
    let mut proto = proto::jj_interface::Tree::default();
    for entry in tree.entries() {
        proto.entries.push(proto::jj_interface::tree::Entry {
            name: entry.name().as_internal_str().to_owned(),
            value: Some(tree_value_to_proto(entry.value())?),
        });
    }
    Ok(proto)
}

fn tree_value_to_proto(value: &TreeValue) -> BackendResult<proto::jj_interface::TreeValue> {
    let value = match value {
        // Kiki stores copy ids on tree entries, but the backend's copy-history
        // APIs still report Unsupported until the daemon has real copy objects.
        TreeValue::File {
            id,
            executable,
            copy_id,
        } => proto::jj_interface::tree_value::Value::File(proto::jj_interface::tree_value::File {
            id: id.to_bytes(),
            executable: *executable,
            copy_id: copy_id.to_bytes(),
        }),
        TreeValue::Symlink(id) => proto::jj_interface::tree_value::Value::SymlinkId(id.to_bytes()),
        TreeValue::Tree(id) => proto::jj_interface::tree_value::Value::TreeId(id.to_bytes()),
        TreeValue::GitSubmodule(_) => {
            return Err(BackendError::Unsupported(
                "kiki backend does not support git submodules".into(),
            ));
        }
    };
    Ok(proto::jj_interface::TreeValue { value: Some(value) })
}

fn tree_from_proto(proto: proto::jj_interface::Tree) -> BackendResult<Tree> {
    // Tree entries must be sorted by name for `Tree::from_sorted_entries`
    // (which only debug_asserts the invariant — release builds would silently
    // corrupt via the binary search in `Tree::entry`). The daemon emits
    // entries in insertion order, so we sort here defensively.
    let mut entries: Vec<(RepoPathComponentBuf, TreeValue)> = proto
        .entries
        .into_iter()
        .map(|e| {
            let name = RepoPathComponentBuf::new(e.name)
                .map_err(|e| BackendError::Other(e.into()))?;
            let raw = e.value.ok_or_else(|| {
                BackendError::Other("daemon returned tree entry with no value".into())
            })?;
            let value = tree_value_from_proto(raw)?;
            Ok::<_, BackendError>((name, value))
        })
        .collect::<Result<_, _>>()?;
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(Tree::from_sorted_entries(entries))
}

fn tree_value_from_proto(proto: proto::jj_interface::TreeValue) -> BackendResult<TreeValue> {
    let raw = proto.value.ok_or_else(|| {
        BackendError::Other("daemon returned TreeValue with empty oneof".into())
    })?;
    Ok(match raw {
        proto::jj_interface::tree_value::Value::TreeId(id) => TreeValue::Tree(TreeId::new(id)),
        proto::jj_interface::tree_value::Value::File(proto::jj_interface::tree_value::File {
            id,
            executable,
            copy_id,
        }) => TreeValue::File {
            id: FileId::new(id),
            executable,
            copy_id: CopyId::new(copy_id),
        },
        proto::jj_interface::tree_value::Value::SymlinkId(id) => {
            TreeValue::Symlink(SymlinkId::new(id))
        }
        // jj-lib 0.40 dropped TreeValue::Conflict from the trait surface; the
        // wire type still carries it for legacy data, but kiki should never
        // round-trip it. Surface this as Unsupported instead of crashing so
        // existing daemon data containing legacy conflict objects fails the
        // single read rather than aborting the CLI.
        proto::jj_interface::tree_value::Value::ConflictId(_) => {
            return Err(BackendError::Unsupported(
                "kiki backend: stored conflict_id is no longer supported by jj-lib 0.40".into(),
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use jj_lib::merge::Merge;

    use super::*;

    fn test_signature() -> Signature {
        Signature {
            name: "Test User".to_string(),
            email: "test.user@example.com".to_string(),
            timestamp: Timestamp {
                timestamp: MillisSinceEpoch(0),
                tz_offset: 0,
            },
        }
    }

    #[test]
    fn commit_conflict_labels_round_trip() {
        let root_tree = Merge::from_vec(vec![
            TreeId::new(vec![1; 20]),
            TreeId::new(vec![2; 20]),
            TreeId::new(vec![3; 20]),
        ]);
        let conflict_labels = Merge::from_vec(vec![
            "left".to_string(),
            "base".to_string(),
            "right".to_string(),
        ]);
        let commit = Commit {
            parents: vec![CommitId::new(vec![4; 20])],
            predecessors: vec![],
            root_tree: root_tree.clone(),
            conflict_labels: conflict_labels.clone(),
            change_id: ChangeId::new(vec![5; 16]),
            description: "conflicted".to_string(),
            author: test_signature(),
            committer: test_signature(),
            secure_sig: None,
        };

        let proto = commit_to_proto(&commit);
        assert_eq!(
            proto.conflict_labels,
            vec!["left".to_string(), "base".to_string(), "right".to_string()]
        );
        let round_tripped = commit_from_proto(proto).expect("round-trip decode");
        assert_eq!(round_tripped.root_tree, root_tree);
        assert_eq!(round_tripped.conflict_labels, conflict_labels);
    }

    #[test]
    fn file_copy_id_round_trip() {
        let copy_id = CopyId::new(vec![9, 8, 7]);
        let value = TreeValue::File {
            id: FileId::new(vec![1; 20]),
            executable: true,
            copy_id: copy_id.clone(),
        };

        let proto = tree_value_to_proto(&value).expect("encode");
        assert!(matches!(
            proto.value.as_ref().unwrap(),
            proto::jj_interface::tree_value::Value::File(file) if file.copy_id == copy_id.to_bytes()
        ));
        assert_eq!(tree_value_from_proto(proto).expect("decode"), value);
    }

    #[test]
    fn tree_value_unknown_oneof_is_other_error() {
        // Force a TreeValue with an empty oneof; this models a daemon that
        // emits an entry with no value set.
        let proto = proto::jj_interface::TreeValue { value: None };
        let err = tree_value_from_proto(proto).expect_err("expected decode error");
        assert!(matches!(err, BackendError::Other(_)), "got {err:?}");
    }

    #[test]
    fn tree_value_legacy_conflict_id_is_unsupported() {
        let proto = proto::jj_interface::TreeValue {
            value: Some(proto::jj_interface::tree_value::Value::ConflictId(vec![1; 20])),
        };
        let err = tree_value_from_proto(proto).expect_err("expected decode error");
        assert!(matches!(err, BackendError::Unsupported(_)), "got {err:?}");
    }

    /// derive_working_copy_path must return a canonical path so the stamp
    /// matches the daemon's `Initialize`-keyed Mount even when jj-lib hands
    /// us a symlinked store_path. Without canonicalization here, every
    /// store RPC would hit `NotFound`.
    #[test]
    fn derive_working_copy_path_canonicalizes_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let wc = dir.path().join("wc");
        let store = wc.join(".jj").join("repo").join("store");
        std::fs::create_dir_all(&store).unwrap();

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&wc, &link).unwrap();
        let symlinked_store = link.join(".jj").join("repo").join("store");

        let derived = derive_working_copy_path(&symlinked_store).expect("derive");
        let canonical_wc = wc.canonicalize().unwrap();
        assert_eq!(derived, canonical_wc.to_str().unwrap());
    }

    #[test]
    fn derive_working_copy_path_rejects_too_shallow() {
        // Two segments — not enough ancestors to reach the wc root.
        let store_path = Path::new(".jj/store");
        let err = derive_working_copy_path(store_path).expect_err("too shallow");
        assert!(
            err.to_string().contains("too shallow"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn derive_working_copy_path_rejects_missing_wc() {
        // Well-formed shape but the parent dirs don't exist on disk;
        // canonicalize() surfaces the missing path instead of silently
        // returning a relative or stale stamp.
        let store_path = Path::new("/definitely/does/not/exist/wc/.jj/repo/store");
        let err = derive_working_copy_path(store_path).expect_err("nonexistent wc");
        assert!(
            err.to_string().contains("canonicalize"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn tree_value_git_submodule_is_unsupported() {
        use jj_lib::backend::CommitId;
        let value = TreeValue::GitSubmodule(CommitId::new(vec![1; 20]));
        let err = tree_value_to_proto(&value).expect_err("expected encode error");
        assert!(matches!(err, BackendError::Unsupported(_)), "got {err:?}");
    }
}
