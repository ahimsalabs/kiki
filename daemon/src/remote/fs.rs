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
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use rand::Rng;

use super::{validate_ref_name, BlobKind, CasOutcome, RemoteStore};

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

    fn blob_path(&self, kind: BlobKind, id: &[u8]) -> PathBuf {
        self.kind_dir(kind).join(hex_bytes(id))
    }

    /// Where refs live. `<root>/refs/`. The sentinel lockfile
    /// (`.lock`) lives at the same level as ref files; ref names
    /// can't start with `/` or contain `..` (validated by
    /// [`validate_ref_name`]) so collision is impossible.
    fn refs_dir(&self) -> PathBuf {
        self.root.join("refs")
    }

    fn ref_path(&self, name: &str) -> PathBuf {
        self.refs_dir().join(name)
    }
}

/// RAII flock guard. Holds an exclusive advisory lock on the wrapped
/// file for as long as the guard is alive. Used by `cas_ref` to
/// arbitrate across multiple processes sharing a `dir://` remote.
///
/// The lock is **advisory** — well-behaved peers using the same flock
/// see the contention; rogue processes that bypass it can still race.
/// All M10 backends go through this code path, so the contract holds
/// for the intended use case (multiple `jj-yak` daemons sharing a
/// remote dir).
struct RefsLock {
    file: fs::File,
}

impl RefsLock {
    /// Acquire an exclusive flock on `<refs_dir>/.lock`, creating the
    /// file if needed. Blocks until acquired; localhost contention is
    /// rare (refs are scarce; CAS holds the lock for one read + one
    /// rename) so we don't spin or backoff.
    fn acquire(refs_dir: &Path) -> Result<Self> {
        fs::create_dir_all(refs_dir)
            .with_context(|| format!("creating {}", refs_dir.display()))?;
        let lock_path = refs_dir.join(".lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        // SAFETY: flock with a valid raw fd. LOCK_EX blocks until
        // exclusive access is acquired; failure (EBADF, EINTR, etc.)
        // surfaces via `last_os_error`.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("flock LOCK_EX on {}", lock_path.display()));
        }
        Ok(RefsLock { file })
    }
}

impl Drop for RefsLock {
    fn drop(&mut self) {
        // SAFETY: same fd we acquired the lock on; LOCK_UN never fails
        // in a way we can recover from. Drop releases the lock when
        // the file handle closes anyway, but explicit unlock makes the
        // contract auditable.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Hex-encode arbitrary bytes without pulling in a hex crate dep.
/// Lower case, no separators, matches what the rest of the daemon
/// prints in `tracing::info!` lines so blob paths are greppable.
/// Works for both 32-byte BLAKE3 and 64-byte BLAKE2b-512 ids.
fn hex_bytes(id: &[u8]) -> String {
    let mut s = String::with_capacity(id.len() * 2);
    for b in id {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

#[async_trait]
impl RemoteStore for FsRemoteStore {
    async fn get_blob(&self, kind: BlobKind, id: &[u8]) -> Result<Option<Bytes>> {
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

    async fn put_blob(&self, kind: BlobKind, id: &[u8], bytes: Bytes) -> Result<()> {
        let dir = self.kind_dir(kind);
        let final_path = dir.join(hex_bytes(id));
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

    async fn has_blob(&self, kind: BlobKind, id: &[u8]) -> Result<bool> {
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

    async fn get_ref(&self, name: &str) -> Result<Option<Bytes>> {
        validate_ref_name(name)?;
        let path = self.ref_path(name);
        // No flock here — readers race with writers but the writer's
        // tmp+rename is atomic (POSIX rename(2) on the same FS is one
        // syscall), so a reader either sees the old contents or the
        // new contents, never a torn write.
        let bytes = tokio::task::spawn_blocking(move || -> Result<Option<Bytes>> {
            match fs::read(&path) {
                Ok(b) => Ok(Some(Bytes::from(b))),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).with_context(|| format!("reading ref {}", path.display())),
            }
        })
        .await
        .context("spawn_blocking get_ref")??;
        Ok(bytes)
    }

    async fn cas_ref(
        &self,
        name: &str,
        expected: Option<&Bytes>,
        new: Option<&Bytes>,
    ) -> Result<CasOutcome> {
        validate_ref_name(name)?;
        let refs_dir = self.refs_dir();
        let ref_path = self.ref_path(name);
        // Clone the precondition + new value across the spawn_blocking
        // boundary. `Bytes::clone` is cheap (refcount bump).
        let expected = expected.cloned();
        let new = new.cloned();
        tokio::task::spawn_blocking(move || -> Result<CasOutcome> {
            // RAII lock arbitrates across processes sharing the dir.
            // Held across the read + rename so a peer's CAS that
            // sneaks in between would block here.
            let _guard = RefsLock::acquire(&refs_dir)?;

            // Read current value (if any).
            let current = match fs::read(&ref_path) {
                Ok(b) => Some(Bytes::from(b)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("reading ref {}", ref_path.display()))
                }
            };

            // Precondition check. Note: `Bytes::eq` is content
            // equality, so empty-current vs empty-expected matches as
            // expected; absent-vs-empty does NOT match (different
            // arms of the Option).
            if current != expected {
                return Ok(CasOutcome::Conflict { actual: current });
            }

            // Apply the swap.
            match new {
                Some(value) => {
                    let tmp_name = format!(
                        ".tmp.{:016x}",
                        rand::thread_rng().gen::<u64>()
                    );
                    let tmp_path = refs_dir.join(tmp_name);
                    {
                        use std::io::Write;
                        let mut f = fs::File::create(&tmp_path)
                            .with_context(|| format!("creating {}", tmp_path.display()))?;
                        f.write_all(&value)
                            .with_context(|| format!("writing {}", tmp_path.display()))?;
                        f.sync_all()
                            .with_context(|| format!("fsync {}", tmp_path.display()))?;
                    }
                    if let Err(e) = fs::rename(&tmp_path, &ref_path) {
                        let _ = fs::remove_file(&tmp_path);
                        return Err(e).with_context(|| {
                            format!(
                                "renaming {} -> {}",
                                tmp_path.display(),
                                ref_path.display()
                            )
                        });
                    }
                }
                None => {
                    // Delete. ENOENT here means we observed `current`
                    // as None (no precondition mismatch above), so
                    // there's nothing to remove — treat as success.
                    if let Err(e) = fs::remove_file(&ref_path) {
                        if e.kind() != io::ErrorKind::NotFound {
                            return Err(e).with_context(|| {
                                format!("removing {}", ref_path.display())
                            });
                        }
                    }
                }
            }
            Ok(CasOutcome::Updated)
        })
        .await
        .context("spawn_blocking cas_ref")?
    }

    async fn list_refs(&self) -> Result<Vec<String>> {
        let refs_dir = self.refs_dir();
        let names = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let read_dir = match fs::read_dir(&refs_dir) {
                Ok(it) => it,
                // Refs dir doesn't exist yet — no refs.
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("reading {}", refs_dir.display()))
                }
            };
            let mut out = Vec::new();
            for entry in read_dir {
                let entry = entry.with_context(|| {
                    format!("iterating {}", refs_dir.display())
                })?;
                let name = entry.file_name();
                let name = name.to_str().ok_or_else(|| {
                    anyhow!("non-UTF-8 ref name in {}", refs_dir.display())
                })?;
                // Skip the sentinel lockfile and any in-flight tmp
                // writes. Both are internal bookkeeping — peers must
                // not see them as refs.
                if name == ".lock" || name.starts_with(".tmp.") {
                    continue;
                }
                out.push(name.to_owned());
            }
            // Deterministic order so callers can compare results
            // across runs without re-sorting.
            out.sort();
            Ok(out)
        })
        .await
        .context("spawn_blocking list_refs")??;
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_of(byte: u8) -> [u8; 32] {
        [byte; 32]
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
            root.join("file").join(hex_bytes(&id)).exists(),
            "blob should be at <root>/<kind>/<hex>"
        );
    }

    #[test]
    fn hex_bytes_round_trip_format() {
        let id = [0u8; 32];
        assert_eq!(hex_bytes(&id).len(), 64);
        let id = [0xab; 32];
        assert!(hex_bytes(&id).chars().all(|c| c == 'a' || c == 'b'));
    }

    // M10.6: 64-byte ids round-trip through the blob CAS.
    #[tokio::test]
    async fn put_then_get_64_byte_id_view() {
        let (_dir, s) = make_store();
        let id = [0xcd; 64];
        let data = Bytes::from_static(b"view-data");
        s.put_blob(BlobKind::View, &id, data.clone()).await.unwrap();
        let got = s.get_blob(BlobKind::View, &id).await.unwrap();
        assert_eq!(got.as_deref(), Some(data.as_ref()));
        // has_blob agrees.
        assert!(s.has_blob(BlobKind::View, &id).await.unwrap());
        // Different kind, same id bytes: distinct keyspace.
        assert!(!s.has_blob(BlobKind::Operation, &id).await.unwrap());
    }

    // ---- M10: ref methods ----------------------------------------------

    #[tokio::test]
    async fn ref_missing_returns_none() {
        let (_dir, s) = make_store();
        let got = s.get_ref("op_heads").await.unwrap();
        assert!(got.is_none(), "missing ref must surface as Ok(None)");
    }

    #[tokio::test]
    async fn cas_ref_create_then_read() {
        let (_dir, s) = make_store();
        let v = Bytes::from_static(b"value-0");
        let outcome = s
            .cas_ref("head", None, Some(&v))
            .await
            .expect("cas create");
        assert_eq!(outcome, CasOutcome::Updated);
        let got = s.get_ref("head").await.unwrap().expect("ref should exist");
        assert_eq!(got.as_ref(), v.as_ref());
    }

    #[tokio::test]
    async fn cas_ref_create_only_conflicts_when_present() {
        let (_dir, s) = make_store();
        let v0 = Bytes::from_static(b"v0");
        s.cas_ref("head", None, Some(&v0)).await.unwrap();
        // expected = None means create-only; against an existing ref
        // the CAS must conflict and return the existing value.
        let outcome = s
            .cas_ref("head", None, Some(&Bytes::from_static(b"v1")))
            .await
            .unwrap();
        assert_eq!(
            outcome,
            CasOutcome::Conflict {
                actual: Some(v0),
            }
        );
    }

    #[tokio::test]
    async fn cas_ref_advance_with_correct_expected() {
        let (_dir, s) = make_store();
        let v0 = Bytes::from_static(b"v0");
        let v1 = Bytes::from_static(b"v1");
        s.cas_ref("op_heads", None, Some(&v0)).await.unwrap();
        let outcome = s
            .cas_ref("op_heads", Some(&v0), Some(&v1))
            .await
            .unwrap();
        assert_eq!(outcome, CasOutcome::Updated);
        assert_eq!(s.get_ref("op_heads").await.unwrap(), Some(v1));
    }

    #[tokio::test]
    async fn cas_ref_stale_expected_returns_actual() {
        let (_dir, s) = make_store();
        let v0 = Bytes::from_static(b"v0");
        let v1 = Bytes::from_static(b"v1");
        s.cas_ref("head", None, Some(&v0)).await.unwrap();
        // Stale expected: caller thinks ref is absent, but it's v0.
        // Conflict reply must carry the actual current value so the
        // caller can retry without a follow-up get_ref.
        let stale = Bytes::from_static(b"stale");
        let outcome = s
            .cas_ref("head", Some(&stale), Some(&v1))
            .await
            .unwrap();
        assert_eq!(
            outcome,
            CasOutcome::Conflict {
                actual: Some(v0),
            }
        );
        // And the ref didn't get clobbered.
        assert_eq!(
            s.get_ref("head").await.unwrap(),
            Some(Bytes::from_static(b"v0"))
        );
    }

    #[tokio::test]
    async fn cas_ref_delete() {
        let (_dir, s) = make_store();
        let v0 = Bytes::from_static(b"v0");
        s.cas_ref("transient", None, Some(&v0)).await.unwrap();
        let outcome = s
            .cas_ref("transient", Some(&v0), None)
            .await
            .unwrap();
        assert_eq!(outcome, CasOutcome::Updated);
        assert_eq!(s.get_ref("transient").await.unwrap(), None);
    }

    #[tokio::test]
    async fn cas_ref_delete_with_stale_expected_conflicts() {
        let (_dir, s) = make_store();
        let v0 = Bytes::from_static(b"v0");
        s.cas_ref("transient", None, Some(&v0)).await.unwrap();
        // Try to delete with a stale precondition.
        let stale = Bytes::from_static(b"wrong");
        let outcome = s
            .cas_ref("transient", Some(&stale), None)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            CasOutcome::Conflict {
                actual: Some(v0.clone()),
            }
        );
        // Nothing was deleted.
        assert_eq!(s.get_ref("transient").await.unwrap(), Some(v0));
    }

    #[tokio::test]
    async fn cas_ref_empty_value_distinct_from_absent() {
        // Critical: the trait's contract says Some(empty) ≠ None.
        // A backend that conflates them would silently accept a stale
        // CAS — this test catches the regression.
        let (_dir, s) = make_store();
        let empty = Bytes::new();
        // Set ref to empty bytes.
        s.cas_ref("e", None, Some(&empty)).await.unwrap();
        assert_eq!(s.get_ref("e").await.unwrap(), Some(empty.clone()));

        // expected = None should now conflict (ref exists, even though
        // its value is empty).
        let outcome = s
            .cas_ref("e", None, Some(&Bytes::from_static(b"x")))
            .await
            .unwrap();
        assert_eq!(
            outcome,
            CasOutcome::Conflict {
                actual: Some(empty.clone()),
            }
        );
        // expected = Some(empty) must succeed.
        let outcome = s
            .cas_ref("e", Some(&empty), Some(&Bytes::from_static(b"x")))
            .await
            .unwrap();
        assert_eq!(outcome, CasOutcome::Updated);
    }

    #[tokio::test]
    async fn list_refs_returns_sorted_names_and_hides_internals() {
        let (_dir, s) = make_store();
        for name in ["zeta", "alpha", "head"] {
            s.cas_ref(name, None, Some(&Bytes::from_static(b"v")))
                .await
                .unwrap();
        }
        let names = s.list_refs().await.unwrap();
        assert_eq!(names, vec!["alpha", "head", "zeta"]);

        // Sentinel lockfile must not leak as a ref.
        // The flock dance creates `.lock` on first cas_ref, so it's
        // already there — list_refs above should have skipped it.
        // Belt-and-braces: drop a fake `.tmp.<rand>` and confirm it's
        // hidden too.
        let parent = s.refs_dir();
        std::fs::write(parent.join(".tmp.1234567890abcdef"), b"x").unwrap();
        let names_again = s.list_refs().await.unwrap();
        assert_eq!(names_again, vec!["alpha", "head", "zeta"]);
    }

    #[tokio::test]
    async fn list_refs_on_empty_store_is_ok() {
        let (_dir, s) = make_store();
        // refs_dir doesn't exist yet — list_refs must not error.
        assert!(s.list_refs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cas_ref_rejects_bad_name() {
        let (_dir, s) = make_store();
        let err = s
            .cas_ref("a/b", None, Some(&Bytes::from_static(b"x")))
            .await
            .expect_err("must reject names with '/'");
        assert!(err.to_string().contains("'/'"), "got: {err}");
    }
}
