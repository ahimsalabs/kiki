//! Read-through helpers shared between [`crate::service`] and
//! [`crate::vfs::yak_fs`].
//!
//! Both consumers face the same shape: "local store missed; if a
//! remote is configured, fetch the blob, verify it round-trips to
//! the requested id (defending against a corrupt peer), persist it
//! locally so the next access hits the cache, and return the typed
//! value." The two consumers differ only in error mapping —
//! `service.rs` surfaces gRPC `Status` codes; `yak_fs.rs` surfaces
//! `FsError`. Splitting the typed [`FetchError`] out lets each
//! consumer pattern-match on the variant they care about (typically
//! `NotFound` and `DataLoss`) and collapse the rest to "internal".
//!
//! Why a typed error rather than `anyhow::Error` with substring
//! matching: `Status::data_loss` (corrupt peer) is observably
//! distinct from `Status::internal` (transport/storage issue), and
//! we want both call sites to honour the distinction without
//! re-implementing the verify-round-trip dance.

use bytes::Bytes;
use prost::Message as _;
use thiserror::Error;

use super::{BlobKind, RemoteStore};
use crate::store::Store;
use crate::ty;

/// Why a read-through fetch failed.
///
/// `NotFound` is the "absent" branch of the upstream `Result<Option<_>>`
/// that backends return — separated from the `Err` axis so callers can
/// surface it as `not_found`/`StoreMiss` cleanly.
#[derive(Debug, Error)]
pub enum FetchError {
    /// Remote does not have the blob (`get_blob` returned `Ok(None)`).
    #[error("remote does not have {kind} {id}")]
    NotFound { kind: BlobKind, id: ty::Id },
    /// Remote returned bytes whose content hash does not match the
    /// id we asked for — almost certainly a remote-store bug, but
    /// surfacing it as a distinct variant lets callers reach for
    /// `Status::data_loss` (or equivalent) instead of silently
    /// poisoning the local store.
    #[error(
        "remote returned {kind} bytes that hash to {got} but we asked for {requested}"
    )]
    DataLoss {
        kind: BlobKind,
        requested: ty::Id,
        got: ty::Id,
    },
    /// Failed to decode the prost message off the wire.
    #[error("decoding {kind} blob: {source}")]
    Decode {
        kind: BlobKind,
        #[source]
        source: prost::DecodeError,
    },
    /// Decode succeeded but the typed-value conversion (e.g. `Tree
    /// TryFrom`) rejected the contents.
    #[error("decoding {kind} value: {source:#}")]
    DecodeValue {
        kind: BlobKind,
        #[source]
        source: anyhow::Error,
    },
    /// Persisting the fetched blob into the local cache failed
    /// (redb commit, I/O error). The remote did its part; we
    /// couldn't write the result.
    #[error("local cache write ({kind}): {source:#}")]
    LocalWrite {
        kind: BlobKind,
        #[source]
        source: anyhow::Error,
    },
    /// `RemoteStore::get_blob` returned an `Err` (transport, backend
    /// failure). Distinct from `NotFound`, which is `Ok(None)`.
    #[error("remote get_blob ({kind}): {source:#}")]
    Remote {
        kind: BlobKind,
        #[source]
        source: anyhow::Error,
    },
}

/// `BlobKind` doesn't implement `Display`; this lets the
/// `#[error(...)]` macros render it as a stable lowercase tag the
/// same way the existing `tracing` lines and dir-backend layout do.
/// Lives next to the variant fields for proximity. (`ty::Id`'s
/// matching `Display` impl lives in `ty.rs`, near the type.)
impl std::fmt::Display for BlobKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Fetch a blob's raw bytes from the remote, returning a typed
/// `Err(FetchError::NotFound)` on `Ok(None)` so the caller doesn't
/// have to flatten the nested option.
async fn fetch_bytes(
    remote: &dyn RemoteStore,
    kind: BlobKind,
    id: ty::Id,
) -> Result<Bytes, FetchError> {
    match remote.get_blob(kind, &id.0).await {
        Ok(Some(b)) => Ok(b),
        Ok(None) => Err(FetchError::NotFound { kind, id }),
        Err(source) => Err(FetchError::Remote { kind, source }),
    }
}

/// Compare-and-fail: the local store re-hashes whatever we write,
/// so we can detect a corrupt peer's id↔bytes mismatch by checking
/// the returned id against the one we asked for.
fn verify_round_trip(
    kind: BlobKind,
    requested: ty::Id,
    got: ty::Id,
) -> Result<(), FetchError> {
    if requested != got {
        return Err(FetchError::DataLoss {
            kind,
            requested,
            got,
        });
    }
    Ok(())
}

/// Fetch a tree blob through the remote and persist it locally.
pub async fn fetch_tree(
    store: &Store,
    remote: &dyn RemoteStore,
    id: ty::Id,
) -> Result<ty::Tree, FetchError> {
    let kind = BlobKind::Tree;
    let bytes = fetch_bytes(remote, kind, id).await?;
    let proto = proto::jj_interface::Tree::decode(bytes.as_ref())
        .map_err(|source| FetchError::Decode { kind, source })?;
    let tree: ty::Tree = proto
        .try_into()
        .map_err(|source| FetchError::DecodeValue { kind, source })?;
    let (got, _bytes) = store
        .write_tree(tree.clone())
        .map_err(|source| FetchError::LocalWrite { kind, source })?;
    verify_round_trip(kind, id, got)?;
    Ok(tree)
}

/// Fetch a file blob through the remote and persist it locally.
pub async fn fetch_file(
    store: &Store,
    remote: &dyn RemoteStore,
    id: ty::Id,
) -> Result<ty::File, FetchError> {
    let kind = BlobKind::File;
    let bytes = fetch_bytes(remote, kind, id).await?;
    let proto = proto::jj_interface::File::decode(bytes.as_ref())
        .map_err(|source| FetchError::Decode { kind, source })?;
    let file = ty::File::from(proto);
    let (got, _bytes) = store
        .write_file(file.clone())
        .map_err(|source| FetchError::LocalWrite { kind, source })?;
    verify_round_trip(kind, id, got)?;
    Ok(file)
}

/// Fetch a symlink blob through the remote and persist it locally.
pub async fn fetch_symlink(
    store: &Store,
    remote: &dyn RemoteStore,
    id: ty::Id,
) -> Result<ty::Symlink, FetchError> {
    let kind = BlobKind::Symlink;
    let bytes = fetch_bytes(remote, kind, id).await?;
    let proto = proto::jj_interface::Symlink::decode(bytes.as_ref())
        .map_err(|source| FetchError::Decode { kind, source })?;
    let symlink = ty::Symlink::from(proto);
    let (got, _bytes) = store
        .write_symlink(symlink.clone())
        .map_err(|source| FetchError::LocalWrite { kind, source })?;
    verify_round_trip(kind, id, got)?;
    Ok(symlink)
}

/// Fetch a commit blob through the remote and persist it locally.
pub async fn fetch_commit(
    store: &Store,
    remote: &dyn RemoteStore,
    id: ty::Id,
) -> Result<ty::Commit, FetchError> {
    let kind = BlobKind::Commit;
    let bytes = fetch_bytes(remote, kind, id).await?;
    let proto = proto::jj_interface::Commit::decode(bytes.as_ref())
        .map_err(|source| FetchError::Decode { kind, source })?;
    let commit: ty::Commit = proto
        .try_into()
        .map_err(|source| FetchError::DecodeValue { kind, source })?;
    let (got, _bytes) = store
        .write_commit(commit.clone())
        .map_err(|source| FetchError::LocalWrite { kind, source })?;
    verify_round_trip(kind, id, got)?;
    Ok(commit)
}
