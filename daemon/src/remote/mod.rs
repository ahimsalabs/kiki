//! Layer C — remote blob CAS (PLAN.md §13).
//!
//! Byte-typed content-addressed store. The local
//! [`crate::git_store::GitContentStore`] stays jj-typed (`get_tree`,
//! `write_file`, ...) because it round-trips through jj-lib's
//! `GitBackend`. A *remote* doesn't need to know about jj types at all
//! — it's just bytes keyed by `(BlobKind, Id)`. Decoupling at the byte
//! boundary means the wire protocol survives schema evolution and lets
//! every backend (fs, gRPC, S3, ...) implement the same three methods.
//!
//! Backends ship in [`fs`] (filesystem, `dir://` scheme) and [`grpc`]
//! (peer daemon, `grpc://` scheme) — see PLAN.md §13.3. Two impls is the
//! magic number for trait extraction; with one impl the trait is shaped
//! by what's easy, not what's needed.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;

pub mod fetch;
pub mod fs;
pub mod grpc;
pub mod server;

#[cfg(test)]
mod sync_sim_tests;

/// Which content-addressed table the (id, bytes) pair belongs to.
/// Mirrors the object kinds in
/// [`crate::git_store::GitContentStore`] (git blobs, trees, commits
/// plus redb views/operations).
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
/// single handle. M10 §10.1 design rationale: backends that ship
/// (`FsRemoteStore`, `GrpcRemoteStore`) and every plausible future
/// one (S3, redb-on-shared-NFS) want both surfaces against the same
/// underlying storage.
///
/// Blob implementations must be:
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
///
/// Ref implementations must be:
///
/// - **Atomic on `cas_ref`.** Two concurrent CASes against the same
///   ref must serialize: one wins (`Updated`), the other loses
///   (`Conflict`). Multi-process backends (`dir://` shared between
///   daemons) need cross-process arbitration, not just an in-process
///   mutex; `FsRemoteStore` uses `flock` on a sentinel file for this.
/// - **Honest about absence on `get_ref`.** `Ok(None)` means "ref
///   does not exist" — same shape as `get_blob`.
/// - **Symmetric absent/present semantics.** `expected = None` is
///   "must not currently exist" (create-only); `new = None` is
///   "delete the ref". An empty `Bytes` value (`Some(Bytes::new())`)
///   is a *valid* value distinct from absent — backends must not
///   conflate them.
#[async_trait]
pub trait RemoteStore: Send + Sync + std::fmt::Debug {
    /// Fetch a blob. `Ok(None)` if the remote does not have it.
    ///
    /// `id` is variable-length: 32 bytes for tree/file/symlink/commit
    /// (BLAKE3), 64 bytes for view/operation (BLAKE2b-512). Backends
    /// must not assume a fixed id length.
    async fn get_blob(&self, kind: BlobKind, id: &[u8]) -> Result<Option<Bytes>>;

    /// Push a blob. Idempotent: byte-identical puts under the same
    /// `(kind, id)` are no-ops on the remote.
    async fn put_blob(&self, kind: BlobKind, id: &[u8], bytes: Bytes) -> Result<()>;

    /// Cheap existence probe. Equivalent in semantics to
    /// `get_blob(...).map(|o| o.is_some())` but backends should
    /// implement it without transferring the body when possible.
    async fn has_blob(&self, kind: BlobKind, id: &[u8]) -> Result<bool>;

    /// Read a ref's current value. `Ok(None)` if the ref does not
    /// exist. `name` must satisfy [`validate_ref_name`]; backends
    /// should re-validate defensively.
    async fn get_ref(&self, name: &str) -> Result<Option<Bytes>>;

    /// Compare-and-swap a ref atomically. See [`CasOutcome`] for the
    /// return shape and the trait-level docs for the absent/present
    /// semantics on `expected`/`new`.
    async fn cas_ref(
        &self,
        name: &str,
        expected: Option<&Bytes>,
        new: Option<&Bytes>,
    ) -> Result<CasOutcome>;

    /// List every ref name. Non-paginated by design: refs are scarce
    /// (op heads, branch tips, not arbitrary catalog data).
    async fn list_refs(&self) -> Result<Vec<String>>;
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
        let id = [0u8; 32];
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
    fn ref_name_validation() {
        // Accepted: arbitrary UTF-8 short names with embedded dots,
        // including names ending in `.lock` (we use a single
        // namespace-wide sentinel, not per-ref lockfiles).
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
        // Sanity: the helpers in `service.rs` compare CasOutcome via
        // `==`, so the derived equality must round-trip the
        // `Conflict { actual: Option<Bytes> }` payload — including
        // the empty-bytes-vs-absent distinction.
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
