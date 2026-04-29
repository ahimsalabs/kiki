//! Filesystem-backed `RemoteStore` (`dir://` scheme, PLAN.md §13.3).
//!
//! Blobs land at `<root>/<kind>/<hex(id)>`. Atomic put: write to a
//! per-call `<root>/<kind>/.tmp.<rand>`, fsync the file, rename into
//! place. Two pushers racing on identical content collide on the
//! rename; the loser gets a benign overwrite (rename is atomic and
//! the bytes are by-construction equal).
//!
//! Useful as:
//! - the trait's reference impl (zero deps, easy to read);
//! - a permanent test fixture (every CI run exercises a real backend);
//! - a "shared NFS dir between two hosts" tool that actually works
//!   today.

use std::fs;
use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use rand::Rng;

use super::{BlobKind, RemoteStore};
use crate::ty::Id;

/// Filesystem-backed blob CAS rooted at `root`.
///
/// `root` is created on first write if missing. Per-kind subdirs are
/// also created lazily. We don't `fsync(root)` after rename; on power
/// loss the worst case is a missing blob that re-uploads on next push.
#[derive(Debug)]
pub struct FsRemoteStore {
    root: PathBuf,
}

impl FsRemoteStore {
    pub fn new(root: PathBuf) -> Self {
        FsRemoteStore { root }
    }

    fn kind_dir(&self, kind: BlobKind) -> PathBuf {
        self.root.join(kind.as_str())
    }

    fn blob_path(&self, kind: BlobKind, id: &Id) -> PathBuf {
        self.kind_dir(kind).join(hex_id(id))
    }
}

/// Hex-encode a 32-byte id without pulling in a hex crate dep. Lower
/// case, no separators, matches what the rest of the daemon prints in
/// `tracing::info!` lines so blob paths are greppable.
fn hex_id(id: &Id) -> String {
    let mut s = String::with_capacity(64);
    for b in id.0 {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

#[async_trait]
impl RemoteStore for FsRemoteStore {
    async fn get_blob(&self, kind: BlobKind, id: &Id) -> Result<Option<Bytes>> {
        let path = self.blob_path(kind, id);
        // Sync I/O on a tokio task. The daemon serves a single FS-rooted
        // remote per mount and blobs are small (jj commits/trees);
        // spawning is cheaper than the ceremony of `tokio::fs` here.
        let bytes = tokio::task::spawn_blocking(move || -> Result<Option<Bytes>> {
            match fs::read(&path) {
                Ok(b) => Ok(Some(Bytes::from(b))),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
            }
        })
        .await
        .context("spawn_blocking get_blob")??;
        Ok(bytes)
    }

    async fn put_blob(&self, kind: BlobKind, id: &Id, bytes: Bytes) -> Result<()> {
        let dir = self.kind_dir(kind);
        let final_path = dir.join(hex_id(id));
        tokio::task::spawn_blocking(move || -> Result<()> {
            // Idempotency fast path: if the bytes are already there,
            // skip the tmp+fsync+rename dance. Saves an inode churn
            // every snapshot for unchanged blobs.
            if final_path.exists() {
                return Ok(());
            }
            fs::create_dir_all(&dir)
                .with_context(|| format!("creating {}", dir.display()))?;

            let tmp_name = format!(
                ".tmp.{:016x}",
                rand::thread_rng().gen::<u64>()
            );
            let tmp_path = dir.join(tmp_name);

            // Scope so the file handle drops before rename.
            {
                use std::io::Write;
                let mut f = fs::File::create(&tmp_path)
                    .with_context(|| format!("creating {}", tmp_path.display()))?;
                f.write_all(&bytes)
                    .with_context(|| format!("writing {}", tmp_path.display()))?;
                f.sync_all()
                    .with_context(|| format!("fsync {}", tmp_path.display()))?;
            }

            // Atomic rename. Two concurrent puts of byte-identical
            // content race here; whichever rename wins, the resulting
            // file has the bytes we wanted (CAS invariant). Map ENOENT
            // / leftover tmp cleanup to anyhow context.
            match fs::rename(&tmp_path, &final_path) {
                Ok(()) => Ok(()),
                Err(e) => {
                    // Best-effort cleanup of the tmp file so we don't
                    // leak inode space on failure paths.
                    let _ = fs::remove_file(&tmp_path);
                    Err(e).with_context(|| {
                        format!(
                            "renaming {} -> {}",
                            tmp_path.display(),
                            final_path.display()
                        )
                    })
                }
            }
        })
        .await
        .context("spawn_blocking put_blob")??;
        Ok(())
    }

    async fn has_blob(&self, kind: BlobKind, id: &Id) -> Result<bool> {
        let path = self.blob_path(kind, id);
        let exists = tokio::task::spawn_blocking(move || -> Result<bool> {
            match fs::metadata(&path) {
                Ok(_) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
            }
        })
        .await
        .context("spawn_blocking has_blob")??;
        Ok(exists)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_of(byte: u8) -> Id {
        Id([byte; 32])
    }

    fn make_store() -> (tempfile::TempDir, FsRemoteStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = FsRemoteStore::new(dir.path().to_owned());
        (dir, s)
    }

    #[tokio::test]
    async fn put_then_get_round_trip() {
        let (_dir, s) = make_store();
        let id = id_of(0xab);
        let bytes = Bytes::from_static(b"hello blob");
        s.put_blob(BlobKind::File, &id, bytes.clone()).await.unwrap();
        let got = s.get_blob(BlobKind::File, &id).await.unwrap();
        assert_eq!(got.as_deref(), Some(bytes.as_ref()));
    }

    #[tokio::test]
    async fn missing_returns_none_not_err() {
        let (_dir, s) = make_store();
        let got = s.get_blob(BlobKind::Tree, &id_of(0)).await.unwrap();
        assert!(got.is_none(), "missing blob must surface as Ok(None)");
    }

    #[tokio::test]
    async fn has_blob_tracks_state() {
        let (_dir, s) = make_store();
        let id = id_of(7);
        assert!(!s.has_blob(BlobKind::Symlink, &id).await.unwrap());
        s.put_blob(BlobKind::Symlink, &id, Bytes::from_static(b"x"))
            .await
            .unwrap();
        assert!(s.has_blob(BlobKind::Symlink, &id).await.unwrap());
        // Different kind, same id: distinct keyspace.
        assert!(!s.has_blob(BlobKind::Tree, &id).await.unwrap());
    }

    #[tokio::test]
    async fn idempotent_put_does_not_error() {
        let (_dir, s) = make_store();
        let id = id_of(1);
        let b = Bytes::from_static(b"same bytes");
        s.put_blob(BlobKind::Commit, &id, b.clone()).await.unwrap();
        // Second put with same content is a no-op.
        s.put_blob(BlobKind::Commit, &id, b.clone()).await.unwrap();
        assert_eq!(
            s.get_blob(BlobKind::Commit, &id).await.unwrap().as_deref(),
            Some(b.as_ref())
        );
    }

    #[tokio::test]
    async fn kinds_are_partitioned() {
        let (_dir, s) = make_store();
        let id = id_of(0xff);
        s.put_blob(BlobKind::File, &id, Bytes::from_static(b"file-bytes"))
            .await
            .unwrap();
        s.put_blob(BlobKind::Tree, &id, Bytes::from_static(b"tree-bytes"))
            .await
            .unwrap();
        assert_eq!(
            s.get_blob(BlobKind::File, &id).await.unwrap().as_deref(),
            Some(b"file-bytes".as_ref())
        );
        assert_eq!(
            s.get_blob(BlobKind::Tree, &id).await.unwrap().as_deref(),
            Some(b"tree-bytes".as_ref())
        );
    }

    #[tokio::test]
    async fn put_creates_root_lazily() {
        // Point at a path under tempdir that doesn't exist yet.
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("does/not/exist/yet");
        let s = FsRemoteStore::new(root.clone());
        let id = id_of(2);
        s.put_blob(BlobKind::File, &id, Bytes::from_static(b"x"))
            .await
            .expect("put creates root + kind dir");
        assert!(
            root.join("file").join(hex_id(&id)).exists(),
            "blob should be at <root>/<kind>/<hex>"
        );
    }

    #[test]
    fn hex_id_round_trip_format() {
        let id = Id([0; 32]);
        assert_eq!(hex_id(&id).len(), 64);
        let id = Id([0xab; 32]);
        assert!(hex_id(&id).chars().all(|c| c == 'a' || c == 'b'));
    }
}
