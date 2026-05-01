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

/// 64-char lowercase hex. Matches what the dir-backend on disk uses
/// for blob filenames and what the daemon's `tracing` lines print, so
/// `format!("{id}")` is greppable across logs and on-disk paths.
impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
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

#[cfg(test)]
mod proptests {
    use super::*;
    use jj_lib::content_hash::ContentHash;
    use proptest::prelude::*;

    // ---- Arbitrary strategies ----

    fn arb_id() -> impl Strategy<Value = Id> {
        any::<[u8; 32]>().prop_map(Id)
    }

    fn arb_file() -> impl Strategy<Value = File> {
        any::<Vec<u8>>().prop_map(|content| File { content })
    }

    fn arb_symlink() -> impl Strategy<Value = Symlink> {
        any::<String>().prop_map(|target| Symlink { target })
    }

    fn arb_tree_entry() -> impl Strategy<Value = TreeEntry> {
        prop_oneof![
            (arb_id(), any::<bool>(), any::<Vec<u8>>()).prop_map(|(id, executable, copy_id)| {
                TreeEntry::File {
                    id,
                    executable,
                    copy_id,
                }
            }),
            arb_id().prop_map(TreeEntry::TreeId),
            arb_id().prop_map(TreeEntry::SymlinkId),
            arb_id().prop_map(TreeEntry::ConflictId),
        ]
    }

    fn arb_tree_entry_mapping() -> impl Strategy<Value = TreeEntryMapping> {
        ("[a-z]{1,8}", arb_tree_entry()).prop_map(|(name, entry)| TreeEntryMapping { name, entry })
    }

    fn arb_tree() -> impl Strategy<Value = Tree> {
        prop::collection::vec(arb_tree_entry_mapping(), 0..8)
            .prop_map(|entries| Tree { entries })
    }

    fn arb_commit_timestamp() -> impl Strategy<Value = CommitTimestamp> {
        (any::<i64>(), any::<i32>()).prop_map(|(millis_since_epoch, tz_offset)| CommitTimestamp {
            millis_since_epoch,
            tz_offset,
        })
    }

    fn arb_commit_signature() -> impl Strategy<Value = CommitSignature> {
        (".*", ".*", arb_commit_timestamp()).prop_map(|(name, email, timestamp)| {
            CommitSignature {
                name,
                email,
                timestamp,
            }
        })
    }

    fn arb_commit() -> impl Strategy<Value = Commit> {
        (
            prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..4),
            prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..4),
            prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..4),
            prop::collection::vec(".*", 0..4),
            any::<bool>(),
            prop::collection::vec(any::<u8>(), 0..64),
            ".*",
            prop::option::of(arb_commit_signature()),
            prop::option::of(arb_commit_signature()),
        )
            .prop_map(
                |(
                    parents,
                    predecessors,
                    root_tree,
                    conflict_labels,
                    uses_tree_conflict_format,
                    change_id,
                    description,
                    author,
                    committer,
                )| {
                    Commit {
                        parents,
                        predecessors,
                        root_tree,
                        conflict_labels,
                        uses_tree_conflict_format,
                        change_id,
                        description,
                        author,
                        committer,
                    }
                },
            )
    }

    // ---- Id invariants ----

    proptest! {
        #[test]
        fn id_try_from_rejects_wrong_length(len in 0..128usize) {
            prop_assume!(len != 32);
            let v = vec![0u8; len];
            prop_assert!(Id::try_from(v).is_err());
        }

        #[test]
        fn id_try_from_accepts_32_bytes(bytes in any::<[u8; 32]>()) {
            let v = bytes.to_vec();
            let id = Id::try_from(v).unwrap();
            prop_assert_eq!(id.0, bytes);
        }

        #[test]
        fn id_display_is_64_hex_chars(id in arb_id()) {
            let s = format!("{id}");
            prop_assert_eq!(s.len(), 64);
            prop_assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        }

        #[test]
        fn id_vec_u8_roundtrip(bytes in any::<[u8; 32]>()) {
            let id = Id(bytes);
            let v: Vec<u8> = id.into();
            let id2 = Id::try_from(v).unwrap();
            prop_assert_eq!(id, id2);
        }
    }

    // ---- File round-trip ----

    proptest! {
        #[test]
        fn file_proto_roundtrip(file in arb_file()) {
            let proto = file.as_proto();
            let back = File::from(proto);
            prop_assert_eq!(file.content, back.content);
        }

        #[test]
        fn file_hash_is_deterministic(file in arb_file()) {
            let h1 = file.get_hash();
            let h2 = file.get_hash();
            prop_assert_eq!(h1, h2);
        }
    }

    // ---- Symlink round-trip ----

    proptest! {
        #[test]
        fn symlink_proto_roundtrip(sym in arb_symlink()) {
            let proto = sym.as_proto();
            let back = Symlink::from(proto);
            prop_assert_eq!(sym.target, back.target);
        }

        #[test]
        fn symlink_hash_is_deterministic(sym in arb_symlink()) {
            let h1 = sym.get_hash();
            let h2 = sym.get_hash();
            prop_assert_eq!(h1, h2);
        }
    }

    // ---- Tree round-trip ----

    proptest! {
        #[test]
        fn tree_proto_roundtrip(tree in arb_tree()) {
            let proto = tree.as_proto();
            let back = Tree::try_from(proto).unwrap();
            prop_assert_eq!(tree.entries.len(), back.entries.len());
            for (orig, decoded) in tree.entries.iter().zip(back.entries.iter()) {
                prop_assert_eq!(&orig.name, &decoded.name);
                // Compare the proto representation for deep equality
                // (TreeEntry doesn't derive PartialEq).
                let orig_proto = orig.entry.as_proto();
                let back_proto = decoded.entry.as_proto();
                prop_assert_eq!(format!("{orig_proto:?}"), format!("{back_proto:?}"));
            }
        }

        #[test]
        fn tree_hash_is_deterministic(tree in arb_tree()) {
            let h1 = tree.get_hash();
            let h2 = tree.get_hash();
            prop_assert_eq!(h1, h2);
        }
    }

    // ---- TreeEntry discriminant isolation ----
    // Two entries of different variants with the same Id must produce
    // different content hashes. This is load-bearing for store integrity.

    proptest! {
        #[test]
        fn tree_entry_discriminant_isolation(id in arb_id()) {
            let file_entry = TreeEntry::File {
                id,
                executable: false,
                copy_id: Vec::new(),
            };
            let tree_entry = TreeEntry::TreeId(id);
            let symlink_entry = TreeEntry::SymlinkId(id);
            let conflict_entry = TreeEntry::ConflictId(id);

            let hash = |entry: &TreeEntry| -> blake3::Hash {
                let mut hasher = blake3::Hasher::new();
                entry.hash(&mut hasher);
                hasher.finalize()
            };

            let h_file = hash(&file_entry);
            let h_tree = hash(&tree_entry);
            let h_sym = hash(&symlink_entry);
            let h_conflict = hash(&conflict_entry);

            // All four must be distinct.
            let hashes = [h_file, h_tree, h_sym, h_conflict];
            for i in 0..hashes.len() {
                for j in (i + 1)..hashes.len() {
                    prop_assert_ne!(hashes[i], hashes[j],
                        "discriminant collision between variant {} and {}", i, j);
                }
            }
        }

        /// Non-empty copy_id must produce a different hash than empty.
        #[test]
        fn file_entry_copy_id_matters(id in arb_id(), copy_id in prop::collection::vec(any::<u8>(), 1..32)) {
            let with_copy = TreeEntry::File {
                id,
                executable: false,
                copy_id: copy_id.clone(),
            };
            let without_copy = TreeEntry::File {
                id,
                executable: false,
                copy_id: Vec::new(),
            };

            let hash = |entry: &TreeEntry| -> blake3::Hash {
                let mut hasher = blake3::Hasher::new();
                entry.hash(&mut hasher);
                hasher.finalize()
            };

            prop_assert_ne!(hash(&with_copy), hash(&without_copy),
                "non-empty copy_id should produce a different hash");
        }
    }

    // ---- Commit round-trip ----

    proptest! {
        #[test]
        fn commit_proto_roundtrip(commit in arb_commit()) {
            let proto = commit.as_proto();
            let back = Commit::try_from(proto).unwrap();
            prop_assert_eq!(&commit.parents, &back.parents);
            prop_assert_eq!(&commit.predecessors, &back.predecessors);
            prop_assert_eq!(&commit.root_tree, &back.root_tree);
            prop_assert_eq!(&commit.conflict_labels, &back.conflict_labels);
            prop_assert_eq!(commit.uses_tree_conflict_format, back.uses_tree_conflict_format);
            prop_assert_eq!(&commit.change_id, &back.change_id);
            prop_assert_eq!(&commit.description, &back.description);
            // Verify author/committer presence round-trips.
            prop_assert_eq!(commit.author.is_some(), back.author.is_some());
            prop_assert_eq!(commit.committer.is_some(), back.committer.is_some());
        }

        #[test]
        fn commit_hash_is_deterministic(commit in arb_commit()) {
            let h1 = commit.get_hash();
            let h2 = commit.get_hash();
            prop_assert_eq!(h1, h2);
        }
    }

    // ---- CommitSignature round-trip ----

    proptest! {
        #[test]
        fn commit_signature_roundtrip(sig in arb_commit_signature()) {
            let proto = sig.as_proto();
            let back = CommitSignature::try_from(proto).unwrap();
            prop_assert_eq!(&sig.name, &back.name);
            prop_assert_eq!(&sig.email, &back.email);
            prop_assert_eq!(sig.timestamp.millis_since_epoch, back.timestamp.millis_since_epoch);
            prop_assert_eq!(sig.timestamp.tz_offset, back.timestamp.tz_offset);
        }
    }
}
