//! Read-through helpers shared between [`crate::service`] and
//! [`crate::vfs::kiki_fs`].
//!
//! After git convergence, content objects are raw git objects (blobs,
//! trees, commits). The fetch pattern is:
//! 1. Local miss on `GitContentStore::read_*`
//! 2. `RemoteStore::get_blob` → raw git object bytes
//! 3. `GitContentStore::write_git_object_bytes` → persist into ODB
//! 4. Re-read from the now-populated ODB
//!
//! Op-store blobs (views, operations) are handled directly in
//! `service.rs` — they don't flow through the git ODB.

use anyhow::anyhow;
use bytes::Bytes;
use thiserror::Error;

use super::{BlobKind, RemoteStore};
use crate::fi::buggify;
use crate::git_store::{GitContentStore, GitObjectKind};

/// Why a read-through fetch failed.
///
/// `NotFound` is the "absent" branch of the upstream `Result<Option<_>>`
/// that backends return — separated from the `Err` axis so callers can
/// surface it as `not_found`/`StoreMiss` cleanly.
#[derive(Debug, Error)]
pub enum FetchError {
    /// Remote does not have the blob (`get_blob` returned `Ok(None)`).
    #[error("remote does not have {kind} {id}")]
    NotFound { kind: BlobKind, id: String },
    /// Remote returned bytes whose content hash does not match the
    /// id we asked for.
    #[error(
        "remote returned {kind} bytes that hash to {got} but we asked for {requested}"
    )]
    DataLoss {
        kind: BlobKind,
        requested: String,
        got: String,
    },
    /// Persisting the fetched blob into the local cache failed.
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

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// Map a `BlobKind` to the corresponding `GitObjectKind`.
fn blob_kind_to_git(kind: BlobKind) -> Option<GitObjectKind> {
    match kind {
        BlobKind::Tree => Some(GitObjectKind::Tree),
        BlobKind::Blob => Some(GitObjectKind::Blob),
        BlobKind::Commit => Some(GitObjectKind::Commit),
        BlobKind::View | BlobKind::Operation | BlobKind::Extra => None,
    }
}

/// Fetch a blob's raw bytes from the remote.
async fn fetch_bytes(
    remote: &dyn RemoteStore,
    kind: BlobKind,
    id: &[u8],
) -> Result<Bytes, FetchError> {
    match remote.get_blob(kind, id).await {
        Ok(Some(b)) => Ok(b),
        Ok(None) => Err(FetchError::NotFound {
            kind,
            id: hex(id),
        }),
        Err(source) => Err(FetchError::Remote { kind, source }),
    }
}

/// Fetch a git object from the remote and write it into the local ODB.
/// Returns the written object id (which should match `requested_id`).
pub async fn fetch_git_object(
    store: &GitContentStore,
    remote: &dyn RemoteStore,
    kind: BlobKind,
    requested_id: &[u8],
) -> Result<(), FetchError> {
    let git_kind = blob_kind_to_git(kind).ok_or_else(|| FetchError::Remote {
        kind,
        source: anyhow!("BlobKind::{kind} is not a git object"),
    })?;
    let bytes = fetch_bytes(remote, kind, requested_id).await?;

    // BUGGIFY(fetch-local-write): Inject failure after successful remote
    // fetch but before local ODB write. Tests that the VFS correctly
    // retries a failed fetch on next read (no stale cache entry).
    if buggify!(50) {
        return Err(FetchError::LocalWrite {
            kind,
            source: anyhow!("[BUGGIFY] simulated local write failure after fetch"),
        });
    }

    let got_id = store
        .write_git_object_bytes(git_kind, &bytes)
        .map_err(|source| FetchError::LocalWrite { kind, source })?;
    if got_id != requested_id {
        return Err(FetchError::DataLoss {
            kind,
            requested: hex(requested_id),
            got: hex(&got_id),
        });
    }
    Ok(())
}

/// Fetch a file blob through the remote and persist into the git ODB.
/// Returns the raw file content.
pub async fn fetch_file(
    store: &GitContentStore,
    remote: &dyn RemoteStore,
    id: &[u8],
) -> Result<Vec<u8>, FetchError> {
    fetch_git_object(store, remote, BlobKind::Blob, id).await?;
    store
        .read_file(id)
        .map_err(|source| FetchError::LocalWrite {
            kind: BlobKind::Blob,
            source,
        })?
        .ok_or_else(|| FetchError::NotFound {
            kind: BlobKind::Blob,
            id: hex(id),
        })
}

/// Fetch a symlink blob through the remote and persist into the git ODB.
/// Returns the symlink target string.
pub async fn fetch_symlink(
    store: &GitContentStore,
    remote: &dyn RemoteStore,
    id: &[u8],
) -> Result<String, FetchError> {
    // Symlinks are git blobs — fetch as Blob kind.
    fetch_git_object(store, remote, BlobKind::Blob, id).await?;
    store
        .read_symlink(id)
        .map_err(|source| FetchError::LocalWrite {
            kind: BlobKind::Blob,
            source,
        })?
        .ok_or_else(|| FetchError::NotFound {
            kind: BlobKind::Blob,
            id: hex(id),
        })
}

/// Fetch a tree object through the remote and persist into the git ODB.
/// Returns the tree entries.
pub async fn fetch_tree(
    store: &GitContentStore,
    remote: &dyn RemoteStore,
    id: &[u8],
) -> Result<Vec<crate::git_store::GitTreeEntry>, FetchError> {
    fetch_git_object(store, remote, BlobKind::Tree, id).await?;
    store
        .read_tree(id)
        .map_err(|source| FetchError::LocalWrite {
            kind: BlobKind::Tree,
            source,
        })?
        .ok_or_else(|| FetchError::NotFound {
            kind: BlobKind::Tree,
            id: hex(id),
        })
}

/// Fetch extras (change-id + predecessors) from the remote and write
/// into the local extras table.
pub async fn fetch_extras(
    store: &GitContentStore,
    remote: &dyn RemoteStore,
    commit_id: &[u8],
) -> Result<(), FetchError> {
    let bytes = fetch_bytes(remote, BlobKind::Extra, commit_id).await?;
    store
        .write_extras(commit_id, &bytes)
        .map_err(|source| FetchError::LocalWrite {
            kind: BlobKind::Extra,
            source,
        })?;
    Ok(())
}

/// Fetch a commit object through the remote and persist into the git ODB.
/// Returns the commit.
pub async fn fetch_commit(
    store: &GitContentStore,
    remote: &dyn RemoteStore,
    id: &[u8],
) -> Result<jj_lib::backend::Commit, FetchError> {
    fetch_git_object(store, remote, BlobKind::Commit, id).await?;
    // Best-effort extras fetch: treat NotFound as OK for backwards
    // compat with remotes that predate extras replication, but
    // propagate other errors.
    match fetch_extras(store, remote, id).await {
        Ok(()) => {}
        Err(FetchError::NotFound { .. }) => {
            tracing::warn!(
                commit = %hex(id),
                "remote has no extras for commit (pre-extras remote?)"
            );
        }
        Err(e) => return Err(e),
    }
    store
        .read_commit(id)
        .map_err(|source| FetchError::LocalWrite {
            kind: BlobKind::Commit,
            source,
        })?
        .ok_or_else(|| FetchError::NotFound {
            kind: BlobKind::Commit,
            id: hex(id),
        })
}
