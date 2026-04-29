//! Layer C — remote blob CAS (PLAN.md §13).
//!
//! Byte-typed content-addressed store. The local [`crate::store::Store`]
//! stays jj-typed (`get_tree`, `write_file`, ...) because it round-trips
//! prost messages. A *remote* doesn't need to know about jj types at all
//! — it's just bytes keyed by `(BlobKind, Id)`. Decoupling at the byte
//! boundary means the wire protocol survives prost schema evolution and
//! lets every backend (fs, gRPC, S3, ...) implement the same three
//! methods.
//!
//! Backends ship in [`fs`] (filesystem, `dir://` scheme) and [`grpc`]
//! (peer daemon, `grpc://` scheme) — see PLAN.md §13.3. Two impls is the
//! magic number for trait extraction; with one impl the trait is shaped
//! by what's easy, not what's needed.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;

use crate::ty::Id;

pub mod fs;
pub mod grpc;
pub mod server;

/// Which content-addressed table the (id, bytes) pair belongs to.
/// Mirrors the four redb tables in [`crate::store::Store`]
/// (`commits_v1`, `files_v1`, `symlinks_v1`, `trees_v1`).
///
/// Carrying the kind through the trait — not implicit in the bytes —
/// lets backends route by table the same way redb does locally and
/// avoids confusing-but-benign content-hash collisions across kinds
/// (two different blob types happening to hash to the same `Id`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BlobKind {
    Tree,
    File,
    Symlink,
    Commit,
}

impl BlobKind {
    /// Stable string tag used by file-backed and on-the-wire layouts.
    /// Adding a kind requires bumping the table layout *or* mapping the
    /// new kind onto an existing tag — never silently reusing one.
    pub fn as_str(self) -> &'static str {
        match self {
            BlobKind::Tree => "tree",
            BlobKind::File => "file",
            BlobKind::Symlink => "symlink",
            BlobKind::Commit => "commit",
        }
    }

    /// Wire-side enum value (proto `BlobKind`). Kept here so callers
    /// don't reach into the proto crate for a constant.
    pub fn as_proto(self) -> proto::jj_interface::BlobKind {
        match self {
            BlobKind::Tree => proto::jj_interface::BlobKind::Tree,
            BlobKind::File => proto::jj_interface::BlobKind::File,
            BlobKind::Symlink => proto::jj_interface::BlobKind::Symlink,
            BlobKind::Commit => proto::jj_interface::BlobKind::Commit,
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
            P::File => Some(BlobKind::File),
            P::Symlink => Some(BlobKind::Symlink),
            P::Commit => Some(BlobKind::Commit),
        }
    }
}

/// Content-addressed remote blob store.
///
/// Implementations must be:
///
/// - **Idempotent on `put_blob`.** Two byte-identical puts under the
///   same `(kind, id)` are equivalent to one. Backends that race on
///   write must collapse losers to `Ok(())`.
/// - **Honest about absence on `get_blob`.** `Ok(None)` means "not
///   here" — distinct from `Err(_)` which means "we don't know,
///   transport/backend failure".
/// - **Cheap on `has_blob`.** Used by the daemon's write-through path
///   to skip a redundant `put_blob` when the remote already has the
///   bytes. Backends that can't answer cheaply should still answer
///   correctly; "stat-then-write" is fine.
#[async_trait]
pub trait RemoteStore: Send + Sync + std::fmt::Debug {
    /// Fetch a blob. `Ok(None)` if the remote does not have it.
    async fn get_blob(&self, kind: BlobKind, id: &Id) -> Result<Option<Bytes>>;

    /// Push a blob. Idempotent: byte-identical puts under the same
    /// `(kind, id)` are no-ops on the remote.
    async fn put_blob(&self, kind: BlobKind, id: &Id, bytes: Bytes) -> Result<()>;

    /// Cheap existence probe. Equivalent in semantics to
    /// `get_blob(...).map(|o| o.is_some())` but backends should
    /// implement it without transferring the body when possible.
    async fn has_blob(&self, kind: BlobKind, id: &Id) -> Result<bool>;
}

/// Parse `Initialize.remote` into an optional [`RemoteStore`] handle.
///
/// - `""` → `Ok(None)` (no remote configured; preserves pre-M9 behavior).
/// - `dir:///abs/path` → [`fs::FsRemoteStore`] rooted at the path.
/// - Anything else → `Err` with the unsupported scheme. Surfaced by
///   `Initialize` as `Status::invalid_argument` so the user gets a
///   clean message rather than a generic internal error.
pub fn parse(remote: &str) -> Result<Option<Arc<dyn RemoteStore>>> {
    if remote.is_empty() {
        return Ok(None);
    }
    // Hand-rolled scheme split rather than pulling in a `url` crate
    // dep: the surface (two schemes for now) is too small to justify.
    let (scheme, rest) = remote
        .split_once("://")
        .ok_or_else(|| anyhow!("remote {remote:?} has no scheme; expected dir://… or grpc://…"))?;
    match scheme {
        "dir" => {
            // `dir:///abs/path` — three slashes preserve POSIX absolute
            // path semantics. `dir://relative` is rejected; a remote
            // pointing at a relative dir is too easy to misroute when
            // the daemon's CWD changes.
            if !rest.starts_with('/') {
                return Err(anyhow!(
                    "dir:// remote must use absolute path (got dir://{rest})"
                ));
            }
            Ok(Some(Arc::new(fs::FsRemoteStore::new(rest.into()))
                as Arc<dyn RemoteStore>))
        }
        "grpc" => {
            // `grpc://host:port` — `host:port` only, no path component.
            // Tonic will reject malformed authorities at first dial; we
            // also pre-flight here so an obviously-bad URL fails at
            // `Initialize` time rather than the first `put_blob`.
            if rest.is_empty() {
                return Err(anyhow!(
                    "grpc:// remote requires host:port (got empty endpoint)"
                ));
            }
            Ok(Some(Arc::new(grpc::GrpcRemoteStore::new(rest)?)
                as Arc<dyn RemoteStore>))
        }
        other => Err(anyhow!("unsupported remote scheme {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse("").unwrap().is_none());
    }

    #[test]
    fn parse_dir_absolute_ok() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("dir://{}", dir.path().display());
        let remote = parse(&url).unwrap().expect("dir:// returns Some");
        // Smoke test: a freshly-rooted remote answers `has_blob` with
        // `false` and not an error.
        let id = Id([0u8; 32]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let has = rt.block_on(remote.has_blob(BlobKind::Tree, &id)).unwrap();
        assert!(!has);
    }

    #[test]
    fn parse_dir_relative_rejected() {
        let err = parse("dir://relative/path").unwrap_err();
        assert!(err.to_string().contains("absolute"), "got: {err}");
    }

    #[test]
    fn parse_no_scheme_rejected() {
        let err = parse("not-a-url").unwrap_err();
        assert!(err.to_string().contains("no scheme"), "got: {err}");
    }

    // `connect_lazy` itself just constructs a `Channel` future, but
    // tonic touches the tokio executor on construction — so the test
    // needs a runtime even though no RPC fires.
    #[tokio::test]
    async fn parse_grpc_returns_some() {
        // grpc:// is parsed lazily — `connect_lazy` defers the actual
        // TCP connect to the first RPC, so an unreachable peer here is
        // not an error at parse time. We just confirm the parser
        // produces a `Some(...)`.
        let remote = parse("grpc://127.0.0.1:9999")
            .unwrap()
            .expect("grpc:// returns Some");
        assert!(format!("{remote:?}").contains("GrpcRemoteStore"));
    }

    #[test]
    fn parse_grpc_empty_endpoint_rejected() {
        let err = parse("grpc://").unwrap_err();
        assert!(err.to_string().contains("host:port"), "got: {err}");
    }

    #[test]
    fn parse_unknown_scheme_rejected() {
        let err = parse("s3://bucket/prefix").unwrap_err();
        assert!(err.to_string().contains("unsupported"), "got: {err}");
    }

    #[test]
    fn blob_kind_proto_round_trip() {
        for k in [
            BlobKind::Tree,
            BlobKind::File,
            BlobKind::Symlink,
            BlobKind::Commit,
        ] {
            assert_eq!(BlobKind::from_proto(k.as_proto()), Some(k));
        }
        assert_eq!(
            BlobKind::from_proto(proto::jj_interface::BlobKind::Unspecified),
            None
        );
    }
}
