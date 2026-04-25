use anyhow::{anyhow, Context};
use jj_lib_proc_macros::ContentHash;

use crate::hash::blake3;

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Id(pub [u8; 32]);

impl jj_lib::content_hash::ContentHash for Id {
    fn hash(&self, state: &mut impl digest::Update) {
        for x in self.0 {
            x.hash(state);
        }
    }
}

impl From<Id> for Vec<u8> {
    fn from(id: Id) -> Self {
        id.0.to_vec()
    }
}

// Proto-to-Id conversions are fallible because the wire format is `bytes` of
// arbitrary length while `Id` is a fixed-size 32-byte hash. Previously these
// were `From` impls that panicked on length mismatch — corrupt or misrouted
// bytes from a peer would then crash the daemon. RPC handlers map the
// resulting error to `Status::invalid_argument`.
impl TryFrom<Vec<u8>> for Id {
    type Error = anyhow::Error;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        let actual_len = value.len();
        let arr: [u8; 32] = value
            .try_into()
            .map_err(|_| anyhow!("expected 32-byte id, got {} bytes", actual_len))?;
        Ok(Id(arr))
    }
}

impl TryFrom<proto::jj_interface::FileId> for Id {
    type Error = anyhow::Error;

    fn try_from(proto: proto::jj_interface::FileId) -> Result<Self, Self::Error> {
        proto.file_id.try_into().context("FileId")
    }
}

impl TryFrom<proto::jj_interface::CommitId> for Id {
    type Error = anyhow::Error;

    fn try_from(proto: proto::jj_interface::CommitId) -> Result<Self, Self::Error> {
        proto.commit_id.try_into().context("CommitId")
    }
}

impl TryFrom<proto::jj_interface::SymlinkId> for Id {
    type Error = anyhow::Error;

    fn try_from(proto: proto::jj_interface::SymlinkId) -> Result<Self, Self::Error> {
        proto.symlink_id.try_into().context("SymlinkId")
    }
}

impl TryFrom<proto::jj_interface::TreeId> for Id {
    type Error = anyhow::Error;

    fn try_from(proto: proto::jj_interface::TreeId) -> Result<Self, Self::Error> {
        proto.tree_id.try_into().context("TreeId")
    }
}

#[derive(Clone, Debug, Default, ContentHash)]
pub struct Symlink {
    // TODO: maybe represent as PathBuf
    pub target: String,
}

impl Symlink {
    pub fn get_hash(&self) -> Id {
        Id(*blake3(self).as_bytes())
    }

    pub fn as_proto(&self) -> proto::jj_interface::Symlink {
        proto::jj_interface::Symlink {
            target: self.target.clone(),
        }
    }
}

impl From<proto::jj_interface::Symlink> for Symlink {
    fn from(proto: proto::jj_interface::Symlink) -> Self {
        Symlink {
            target: proto.target,
        }
    }
}

#[derive(Clone, Debug, Default, ContentHash)]
pub struct CommitTimestamp {
    millis_since_epoch: i64,
    tz_offset: i32,
}
impl CommitTimestamp {
    pub fn as_proto(&self) -> proto::jj_interface::commit::Timestamp {
        proto::jj_interface::commit::Timestamp {
            millis_since_epoch: self.millis_since_epoch,
            tz_offset: self.tz_offset,
        }
    }
}

impl CommitSignature {
    pub fn as_proto(&self) -> proto::jj_interface::commit::Signature {
        proto::jj_interface::commit::Signature {
            name: self.name.clone(),
            email: self.email.clone(),
            timestamp: Some(self.timestamp.as_proto()),
        }
    }
}

impl From<proto::jj_interface::commit::Timestamp> for CommitTimestamp {
    fn from(proto: proto::jj_interface::commit::Timestamp) -> Self {
        CommitTimestamp {
            millis_since_epoch: proto.millis_since_epoch,
            tz_offset: proto.tz_offset,
        }
    }
}

impl TryFrom<proto::jj_interface::commit::Signature> for CommitSignature {
    type Error = anyhow::Error;

    fn try_from(proto: proto::jj_interface::commit::Signature) -> Result<Self, Self::Error> {
        // The wire schema marks timestamp as `optional`, but jj's data model
        // requires every signature to carry one. A missing timestamp means
        // either a corrupted entry or a peer running with a stale schema; in
        // either case a hard crash is worse than surfacing the failure.
        let timestamp = proto
            .timestamp
            .ok_or_else(|| anyhow!("CommitSignature missing required timestamp"))?
            .into();
        Ok(CommitSignature {
            name: proto.name,
            email: proto.email,
            timestamp,
        })
    }
}

#[derive(Clone, Debug, Default, ContentHash)]
pub struct CommitSignature {
    name: String,
    email: String,
    timestamp: CommitTimestamp,
}

#[derive(Clone, Debug, Default)]
pub struct Commit {
    pub parents: Vec<Vec<u8>>,
    pub predecessors: Vec<Vec<u8>>,
    pub root_tree: Vec<Vec<u8>>,
    pub conflict_labels: Vec<String>,
    pub uses_tree_conflict_format: bool,
    pub change_id: Vec<u8>,
    pub description: String,
    pub author: Option<CommitSignature>,
    pub committer: Option<CommitSignature>,
}

impl jj_lib::content_hash::ContentHash for Commit {
    fn hash(&self, state: &mut impl digest::Update) {
        self.parents.hash(state);
        self.predecessors.hash(state);
        self.root_tree.hash(state);
        if !self.conflict_labels.is_empty() {
            self.conflict_labels.hash(state);
        }
        self.uses_tree_conflict_format.hash(state);
        self.change_id.hash(state);
        self.description.hash(state);
        self.author.hash(state);
        self.committer.hash(state);
    }
}

impl Commit {
    pub fn get_hash(&self) -> Id {
        Id(*blake3(self).as_bytes())
    }

    pub fn as_proto(&self) -> proto::jj_interface::Commit {
        proto::jj_interface::Commit {
            parents: self.parents.clone(),
            predecessors: self.predecessors.clone(),
            root_tree: self.root_tree.clone(),
            conflict_labels: self.conflict_labels.clone(),
            uses_tree_conflict_format: self.uses_tree_conflict_format,
            change_id: self.change_id.clone(),
            description: self.description.clone(),
            author: self.author.as_ref().map(CommitSignature::as_proto),
            committer: self.committer.as_ref().map(CommitSignature::as_proto),
            ..Default::default()
        }
    }
}

impl TryFrom<proto::jj_interface::Commit> for Commit {
    type Error = anyhow::Error;

    fn try_from(proto: proto::jj_interface::Commit) -> Result<Self, Self::Error> {
        // `Option::map` won't compose with a fallible conversion, so unfold
        // the author/committer signatures by hand.
        let author = proto
            .author
            .map(CommitSignature::try_from)
            .transpose()
            .context("author")?;
        let committer = proto
            .committer
            .map(CommitSignature::try_from)
            .transpose()
            .context("committer")?;
        Ok(Commit {
            parents: proto.parents,
            predecessors: proto.predecessors,
            root_tree: proto.root_tree,
            conflict_labels: proto.conflict_labels,
            uses_tree_conflict_format: proto.uses_tree_conflict_format,
            change_id: proto.change_id,
            description: proto.description,
            author,
            committer,
        })
    }
}

#[derive(Clone, Debug, Default, ContentHash)]
pub struct File {
    pub content: Vec<u8>,
}

impl File {
    pub fn get_hash(&self) -> Id {
        Id(*blake3(self).as_bytes())
    }

    pub fn as_proto(&self) -> proto::jj_interface::File {
        proto::jj_interface::File {
            data: self.content.clone(),
        }
    }
}

impl From<proto::jj_interface::File> for File {
    fn from(proto: proto::jj_interface::File) -> Self {
        File {
            content: proto.data,
        }
    }
}

#[derive(Clone, Debug, Default, ContentHash)]
pub struct Tree {
    pub entries: Vec<TreeEntryMapping>,
}

#[derive(Clone, Debug, ContentHash)]
pub struct TreeEntryMapping {
    pub name: String,
    pub entry: TreeEntry,
}

impl Tree {
    pub fn get_hash(&self) -> Id {
        Id(*blake3(self).as_bytes())
    }

    pub fn as_proto(&self) -> proto::jj_interface::Tree {
        let mut proto = proto::jj_interface::Tree::default();
        for entry in &self.entries {
            proto.entries.push(proto::jj_interface::tree::Entry {
                name: entry.name.clone(),
                value: Some(entry.entry.as_proto()),
            });
        }
        proto
    }
}

impl TryFrom<proto::jj_interface::Tree> for Tree {
    type Error = anyhow::Error;

    fn try_from(proto: proto::jj_interface::Tree) -> Result<Self, Self::Error> {
        let mut tree = Tree::default();
        for proto_entry in proto.entries {
            let proto_val = proto_entry
                .value
                .ok_or_else(|| anyhow!("tree entry {:?} missing value oneof", proto_entry.name))?;
            let entry: TreeEntry = proto_val
                .try_into()
                .with_context(|| format!("decoding tree entry {:?}", proto_entry.name))?;
            tree.entries.push(TreeEntryMapping {
                name: proto_entry.name,
                entry,
            });
        }
        Ok(tree)
    }
}

#[derive(Clone, Debug)]
pub enum TreeEntry {
    File {
        id: Id,
        executable: bool,
        copy_id: Vec<u8>,
    },
    TreeId(Id),
    SymlinkId(Id),
    ConflictId(Id),
}

impl jj_lib::content_hash::ContentHash for TreeEntry {
    fn hash(&self, state: &mut impl digest::Update) {
        match self {
            TreeEntry::File {
                id,
                executable,
                copy_id,
            } => {
                0_u32.hash(state);
                id.hash(state);
                executable.hash(state);
                if !copy_id.is_empty() {
                    copy_id.hash(state);
                }
            }
            TreeEntry::TreeId(id) => {
                1_u32.hash(state);
                id.hash(state);
            }
            TreeEntry::SymlinkId(id) => {
                2_u32.hash(state);
                id.hash(state);
            }
            TreeEntry::ConflictId(id) => {
                3_u32.hash(state);
                id.hash(state);
            }
        }
    }
}

impl TryFrom<proto::jj_interface::TreeValue> for TreeEntry {
    type Error = anyhow::Error;

    fn try_from(proto: proto::jj_interface::TreeValue) -> Result<Self, Self::Error> {
        let value = proto
            .value
            .ok_or_else(|| anyhow!("TreeValue missing value oneof"))?;
        use proto::jj_interface::tree_value::Value::*;
        Ok(match value {
            TreeId(id) => TreeEntry::TreeId(id.try_into().context("tree id")?),
            SymlinkId(id) => TreeEntry::SymlinkId(id.try_into().context("symlink id")?),
            ConflictId(id) => TreeEntry::ConflictId(id.try_into().context("conflict id")?),
            File(file) => TreeEntry::File {
                id: file.id.try_into().context("file id")?,
                executable: file.executable,
                copy_id: file.copy_id,
            },
        })
    }
}

impl TreeEntry {
    pub fn as_proto(&self) -> proto::jj_interface::TreeValue {
        let value = match self {
            TreeEntry::File {
                id,
                executable,
                copy_id,
            } => proto::jj_interface::tree_value::Value::File(
                proto::jj_interface::tree_value::File {
                    id: id.0.to_vec(),
                    executable: *executable,
                    copy_id: copy_id.clone(),
                },
            ),
            TreeEntry::TreeId(id) => proto::jj_interface::tree_value::Value::TreeId(id.0.to_vec()),
            TreeEntry::SymlinkId(id) => {
                proto::jj_interface::tree_value::Value::SymlinkId(id.0.to_vec())
            }
            TreeEntry::ConflictId(id) => {
                proto::jj_interface::tree_value::Value::ConflictId(id.0.to_vec())
            }
        };
        proto::jj_interface::TreeValue { value: Some(value) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_try_from_wrong_length_errors() {
        let err = Id::try_from(vec![0u8; 31]).expect_err("expected length error");
        assert!(err.to_string().contains("31 bytes"), "got: {err}");
    }

    #[test]
    fn id_try_from_correct_length_succeeds() {
        let id: Id = vec![0xab; 32].try_into().expect("32 bytes should fit");
        assert_eq!(id.0[0], 0xab);
    }

    #[test]
    fn commit_signature_missing_timestamp_errors() {
        let proto = proto::jj_interface::commit::Signature {
            name: "n".into(),
            email: "e".into(),
            timestamp: None,
        };
        let err = CommitSignature::try_from(proto).expect_err("expected missing-timestamp error");
        assert!(err.to_string().contains("timestamp"), "got: {err}");
    }

    #[test]
    fn tree_value_missing_oneof_errors() {
        let proto = proto::jj_interface::TreeValue { value: None };
        let err = TreeEntry::try_from(proto).expect_err("expected missing-oneof error");
        assert!(err.to_string().contains("missing"), "got: {err}");
    }

    #[test]
    fn tree_entry_with_short_id_errors() {
        let proto = proto::jj_interface::TreeValue {
            value: Some(proto::jj_interface::tree_value::Value::TreeId(vec![1; 16])),
        };
        let err = TreeEntry::try_from(proto).expect_err("expected short-id error");
        // anyhow's Display only shows the top-level context; use the
        // alternate formatter to walk the source chain.
        let chained = format!("{err:#}");
        assert!(chained.contains("32-byte id"), "got: {chained}");
    }

    #[test]
    fn tree_from_proto_missing_entry_value_errors() {
        let proto = proto::jj_interface::Tree {
            entries: vec![proto::jj_interface::tree::Entry {
                name: "foo".into(),
                value: None,
            }],
        };
        let err = Tree::try_from(proto).expect_err("expected missing-value error");
        assert!(err.to_string().contains("foo"), "got: {err}");
    }
}
