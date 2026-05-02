//! Core ID type for the daemon's content-addressed store.
//!
//! After git convergence, IDs are 20-byte SHA-1 (standard git) instead
//! of the previous 32-byte BLAKE3. Content types (`Commit`, `Tree`,
//! `File`, `Symlink`) and their `ContentHash` impls have been removed —
//! the daemon now works with jj-lib's `backend::*` types via
//! [`crate::git_store::GitContentStore`].

use anyhow::{anyhow, Context};

/// 20-byte SHA-1 content-addressed ID (standard git object ID).
///
/// Previously `[u8; 32]` (BLAKE3). Changed to `[u8; 20]` as part of
/// the git convergence — see `docs/GIT_CONVERGENCE.md`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Id(pub [u8; 20]);

impl From<Id> for Vec<u8> {
    fn from(id: Id) -> Self {
        id.0.to_vec()
    }
}

/// 40-char lowercase hex. Matches git's object ID format.
impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

// Proto-to-Id conversions are fallible because the wire format is `bytes` of
// arbitrary length while `Id` is a fixed-size 20-byte hash. RPC handlers map
// the resulting error to `Status::invalid_argument`.
impl TryFrom<Vec<u8>> for Id {
    type Error = anyhow::Error;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        let actual_len = value.len();
        let arr: [u8; 20] = value
            .try_into()
            .map_err(|_| anyhow!("expected 20-byte id, got {} bytes", actual_len))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_try_from_wrong_length_errors() {
        let err = Id::try_from(vec![0u8; 19]).expect_err("expected length error");
        assert!(err.to_string().contains("19 bytes"), "got: {err}");
    }

    #[test]
    fn id_try_from_correct_length_succeeds() {
        let id: Id = vec![0xab; 20].try_into().expect("20 bytes should fit");
        assert_eq!(id.0[0], 0xab);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn arb_id() -> impl Strategy<Value = Id> {
        any::<[u8; 20]>().prop_map(Id)
    }

    proptest! {
        #[test]
        fn id_try_from_rejects_wrong_length(len in 0..128usize) {
            prop_assume!(len != 20);
            let v = vec![0u8; len];
            prop_assert!(Id::try_from(v).is_err());
        }

        #[test]
        fn id_try_from_accepts_20_bytes(bytes in any::<[u8; 20]>()) {
            let v = bytes.to_vec();
            let id = Id::try_from(v).unwrap();
            prop_assert_eq!(id.0, bytes);
        }

        #[test]
        fn id_display_is_40_hex_chars(id in arb_id()) {
            let s = format!("{id}");
            prop_assert_eq!(s.len(), 40);
            prop_assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        }

        #[test]
        fn id_vec_u8_roundtrip(bytes in any::<[u8; 20]>()) {
            let id = Id(bytes);
            let v: Vec<u8> = id.into();
            let id2 = Id::try_from(v).unwrap();
            prop_assert_eq!(id, id2);
        }
    }
}
