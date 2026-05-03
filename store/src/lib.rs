#![deny(warnings)]

//! Shared storage primitives for kiki.
//!
//! The `RemoteStore` trait, `BlobKind` enum, and `FsRemoteStore`
//! implementation live here so both the daemon and CLI crates can
//! use them without circular dependencies.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;

pub mod fs;
pub mod paths;

/// Which content-addressed table the (id, bytes) pair belongs to.
/// Mirrors the object kinds in the daemon's `GitContentStore` (git
/// blobs, trees, commits plus redb views/operations).
///
/// Carrying the kind through the trait — not implicit in the bytes —
/// lets backends route by table the same way the local store does
/// and avoids confusing-but-benign content-hash collisions across
/// kinds (two different blob types happening to hash to the same id).
///
/// M10.6: `View` and `Operation` added for op-store data. These use
/// 64-byte BLAKE2b-512 ids (vs 20-byte SHA-1 for git objects); the
/// trait's blob methods accept `&[u8]` to accommodate both.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BlobKind {
    Tree,
    Blob,
    Commit,
    View,
    Operation,
    Extra,
}

impl BlobKind {
    /// Stable string tag used by file-backed and on-the-wire layouts.
    /// Adding a kind requires bumping the table layout *or* mapping the
    /// new kind onto an existing tag — never silently reusing one.
    pub fn as_str(self) -> &'static str {
        match self {
            BlobKind::Tree => "tree",
            BlobKind::Blob => "blob",
            BlobKind::Commit => "commit",
            BlobKind::View => "view",
            BlobKind::Operation => "operation",
            BlobKind::Extra => "extra",
        }
    }

    /// Wire-side enum value (proto `BlobKind`). Kept here so callers
    /// don't reach into the proto crate for a constant.
    pub fn as_proto(self) -> proto::jj_interface::BlobKind {
        match self {
            BlobKind::Tree => proto::jj_interface::BlobKind::Tree,
            BlobKind::Blob => proto::jj_interface::BlobKind::Blob,
            BlobKind::Commit => proto::jj_interface::BlobKind::Commit,
            BlobKind::View => proto::jj_interface::BlobKind::View,
            BlobKind::Operation => proto::jj_interface::BlobKind::Operation,
            BlobKind::Extra => proto::jj_interface::BlobKind::Extra,
        }
    }

    /// Decode the wire-side enum. Returns `None` for the
    /// `BLOB_KIND_UNSPECIFIED` zero value — protobuf3 enums always
    /// admit zero, and an unspecified kind is invalid input rather than
    /// a partition we should silently absorb.
    pub fn from_proto(p: proto::jj_interface::BlobKind) -> Option<Self> {
        use proto::jj_interface::BlobKind as P;
        match p {
            P::Unspecified => None,
            P::Tree => Some(BlobKind::Tree),
            P::Blob => Some(BlobKind::Blob),
            P::Commit => Some(BlobKind::Commit),
            P::View => Some(BlobKind::View),
            P::Operation => Some(BlobKind::Operation),
            P::Extra => Some(BlobKind::Extra),
        }
    }
}

impl std::fmt::Display for BlobKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of a [`RemoteStore::cas_ref`] call.
///
/// `Updated` — the precondition matched; the swap was applied.
/// `Conflict { actual }` — the precondition did not match; `actual`
/// is the value the server saw (`None` if the ref does not exist),
/// which the caller should retry against rather than re-read via a
/// follow-up `get_ref` (saves a round-trip on the conflict path).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CasOutcome {
    Updated,
    Conflict { actual: Option<Bytes> },
}

/// Validate a ref name. Refs live in a flat namespace under
/// `<remote>/refs/<name>`; we forbid characters that would either
/// escape the directory or break path-style backends.
///
/// Rejected: empty, NUL, `/`, exact matches `.` and `..`. The
/// trailing-`.lock` suffix is *not* reserved — the FS backend uses
/// a single `.lock` sentinel for the whole namespace, not per-ref
/// lockfiles, so a ref literally named `something.lock` is fine.
pub fn validate_ref_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("ref name must not be empty"));
    }
    if name.contains('\0') {
        return Err(anyhow!("ref name {name:?} contains NUL"));
    }
    if name.contains('/') {
        return Err(anyhow!("ref name {name:?} contains '/' (refs are flat)"));
    }
    if name == "." || name == ".." {
        return Err(anyhow!("ref name {name:?} is a reserved path component"));
    }
    Ok(())
}

/// Content-addressed remote blob store + mutable refs catalog.
///
/// Two surfaces on one trait so every Arc-wielding consumer holds a
/// single handle. Backends that ship (`FsRemoteStore`,
/// `GrpcRemoteStore`) and every plausible future one (S3,
/// redb-on-shared-NFS) want both surfaces against the same underlying
/// storage.
#[async_trait]
pub trait RemoteStore: Send + Sync + std::fmt::Debug {
    /// Fetch a blob. `Ok(None)` if the remote does not have it.
    async fn get_blob(&self, kind: BlobKind, id: &[u8]) -> Result<Option<Bytes>>;

    /// Push a blob. Idempotent: byte-identical puts under the same
    /// `(kind, id)` are no-ops on the remote.
    async fn put_blob(&self, kind: BlobKind, id: &[u8], bytes: Bytes) -> Result<()>;

    /// Cheap existence probe.
    async fn has_blob(&self, kind: BlobKind, id: &[u8]) -> Result<bool>;

    /// Read a ref's current value. `Ok(None)` if the ref does not
    /// exist.
    async fn get_ref(&self, name: &str) -> Result<Option<Bytes>>;

    /// Compare-and-swap a ref atomically. See [`CasOutcome`].
    async fn cas_ref(
        &self,
        name: &str,
        expected: Option<&Bytes>,
        new: Option<&Bytes>,
    ) -> Result<CasOutcome>;

    /// List every ref name.
    async fn list_refs(&self) -> Result<Vec<String>>;
}

/// Parse a remote URL into an optional [`RemoteStore`] handle.
///
/// This is the daemon-side parser. The `store` crate provides
/// `FsRemoteStore` directly; other schemes (`grpc://`, `kiki+ssh://`,
/// `kiki://`) are handled by the daemon's extended `parse()`.
pub fn parse_dir(remote: &str) -> Result<Option<Arc<dyn RemoteStore>>> {
    if remote.is_empty() {
        return Ok(None);
    }
    let (scheme, rest) = remote
        .split_once("://")
        .ok_or_else(|| anyhow!("remote {remote:?} has no scheme; expected dir://…, s3://…, kiki+ssh://…, or kiki://…"))?;
    match scheme {
        "dir" => {
            if !rest.starts_with('/') {
                return Err(anyhow!(
                    "dir:// remote must use absolute path (got dir://{rest})"
                ));
            }
            Ok(Some(
                Arc::new(fs::FsRemoteStore::new(rest.into())) as Arc<dyn RemoteStore>
            ))
        }
        _ => Err(anyhow!("store::parse_dir only handles dir://; got scheme {scheme:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_name_validation() {
        for name in ["op_heads", "head", "branch.lock", "a.b.c", "🦀"] {
            validate_ref_name(name).unwrap_or_else(|e| panic!("expected {name:?} ok: {e}"));
        }
        for (name, snippet) in [
            ("", "must not be empty"),
            ("a/b", "'/'"),
            ("..", "reserved"),
            (".", "reserved"),
            ("a\0b", "NUL"),
        ] {
            let err = validate_ref_name(name).expect_err(name);
            assert!(
                err.to_string().contains(snippet),
                "expected {name:?} error to contain {snippet:?}, got: {err}"
            );
        }
    }

    #[test]
    fn cas_outcome_eq() {
        assert_eq!(CasOutcome::Updated, CasOutcome::Updated);
        assert_eq!(
            CasOutcome::Conflict {
                actual: Some(Bytes::from_static(b"x")),
            },
            CasOutcome::Conflict {
                actual: Some(Bytes::from_static(b"x")),
            }
        );
        assert_ne!(
            CasOutcome::Conflict { actual: None },
            CasOutcome::Conflict {
                actual: Some(Bytes::new()),
            }
        );
    }

    #[test]
    fn blob_kind_proto_round_trip() {
        for k in [
            BlobKind::Tree,
            BlobKind::Blob,
            BlobKind::Commit,
            BlobKind::View,
            BlobKind::Operation,
            BlobKind::Extra,
        ] {
            assert_eq!(BlobKind::from_proto(k.as_proto()), Some(k));
        }
        assert_eq!(
            BlobKind::from_proto(proto::jj_interface::BlobKind::Unspecified),
            None
        );
    }
}
