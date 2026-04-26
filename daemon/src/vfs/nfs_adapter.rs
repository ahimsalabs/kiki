//! `nfsserve::vfs::NFSFileSystem` adapter over [`JjYakFs`].
//!
//! Read ops dispatch into the trait; write ops dispatch into the M6
//! mutating surface (`create` / `mkdir` / `symlink` / `write` / `setattr`
//! / `remove`). This is the macOS primary path — Linux uses
//! `vfs::fuse_adapter` once mount lands at M4 (see `docs/PLAN.md` §4.3).
//!
//! NFS-specific quirks worth flagging:
//!
//! - `sattr3` carries each field inside a small "set or not" enum
//!   (`set_mode3`, `set_size3`, …). The helpers at the bottom of this
//!   module collapse those to `Option<T>` so the trait surface stays
//!   protocol-agnostic.
//! - `rename` dispatches into `JjYakFs::rename`. jj-lib uses the standard
//!   atomic-write-via-temp-then-rename pattern for index segments,
//!   opheads, and refs; without rename, `jj yak init` fails halfway.

use std::sync::Arc;

use async_trait::async_trait;
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, mode3, nfspath3, nfsstat3, nfstime3, sattr3, set_gid3,
    set_mode3, set_size3, set_uid3, specdata3,
};
use nfsserve::vfs::{
    DirEntry as NfsDirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities,
};

use crate::vfs::yak_fs::{Attr, FileKind, FsError, JjYakFs};

#[derive(Debug)]
pub struct NfsAdapter {
    inner: Arc<dyn JjYakFs>,
}

impl NfsAdapter {
    pub fn new(inner: Arc<dyn JjYakFs>) -> Self {
        Self { inner }
    }
}

fn fs_err_to_nfs(e: FsError) -> nfsstat3 {
    match e {
        FsError::NotFound => nfsstat3::NFS3ERR_NOENT,
        FsError::NotADirectory => nfsstat3::NFS3ERR_NOTDIR,
        // ISDIR is the closest fit for "asked to read a directory" — NFS
        // doesn't have a direct equivalent of EINVAL for "kind mismatch".
        FsError::NotAFile => nfsstat3::NFS3ERR_ISDIR,
        FsError::NotASymlink => nfsstat3::NFS3ERR_INVAL,
        FsError::AlreadyExists => nfsstat3::NFS3ERR_EXIST,
        FsError::NotEmpty => nfsstat3::NFS3ERR_NOTEMPTY,
        FsError::StoreMiss => nfsstat3::NFS3ERR_IO,
    }
}

fn name_to_string(name: &filename3) -> Result<String, nfsstat3> {
    std::str::from_utf8(name.as_ref())
        .map(str::to_owned)
        // Path components must be UTF-8: jj's tree entries store their
        // names as `String`. A non-UTF-8 path component can't address
        // anything in the tree, so report INVAL rather than NOENT — the
        // distinction matters on macOS where `Finder` retries after NOENT.
        .map_err(|_| nfsstat3::NFS3ERR_INVAL)
}

fn to_fattr3(a: Attr) -> fattr3 {
    let (ftype, mode) = match a.kind {
        FileKind::Regular => (
            ftype3::NF3REG,
            if a.executable { 0o755 } else { 0o644 } as mode3,
        ),
        FileKind::Directory => (ftype3::NF3DIR, 0o755 as mode3),
        FileKind::Symlink => (ftype3::NF3LNK, 0o777 as mode3),
    };
    fattr3 {
        ftype,
        mode,
        // Single hard link — jj's WC isn't multi-rooted.
        nlink: 1,
        // Owner is whoever is running the daemon. Real uid/gid mapping
        // arrives whenever multi-user write is on the table; not before.
        uid: 0,
        gid: 0,
        size: a.size,
        used: a.size,
        rdev: specdata3::default(),
        fsid: 0,
        fileid: a.inode,
        // Times stay zeroed until M5/M6 — the VFS hasn't seen any
        // mutations yet, and the macOS NFS client mostly cares about
        // ctime/mtime to decide whether to revalidate. With `actimeo=0`
        // (planned mount option, §4.2) it revalidates anyway.
        atime: nfstime3::default(),
        mtime: nfstime3::default(),
        ctime: nfstime3::default(),
    }
}

#[async_trait]
impl NFSFileSystem for NfsAdapter {
    fn root_dir(&self) -> fileid3 {
        self.inner.root()
    }

    fn capabilities(&self) -> VFSCapabilities {
        // Advertise read-write so the kernel will issue write RPCs once
        // M5/M6 implement them; meanwhile each mutation returns ROFS,
        // which `mount_nfs` users will see as "Read-only file system".
        VFSCapabilities::ReadWrite
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = name_to_string(filename)?;
        self.inner.lookup(dirid, &name).await.map_err(fs_err_to_nfs)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let attr = self.inner.getattr(id).await.map_err(fs_err_to_nfs)?;
        Ok(to_fattr3(attr))
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        self.inner
            .read(id, offset, count)
            .await
            .map_err(fs_err_to_nfs)
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let entries = self.inner.readdir(dirid).await.map_err(fs_err_to_nfs)?;

        // Pagination: nfsserve passes `start_after = 0` for the first
        // page, then echoes back the last fileid we returned. Skip until
        // we've seen that id; if it isn't present (kernel asked about
        // a stale cookie), fall through and return nothing left.
        let mut iter = entries.iter();
        if start_after != 0 {
            let mut found = false;
            for e in iter.by_ref() {
                if e.inode == start_after {
                    found = true;
                    break;
                }
            }
            if !found {
                return Ok(ReadDirResult {
                    entries: Vec::new(),
                    end: true,
                });
            }
        }

        let mut out = Vec::with_capacity(max_entries);
        let mut end = true;
        for e in iter {
            if out.len() >= max_entries {
                end = false;
                break;
            }
            // `getattr` reuses the same kind/size logic as the inherent
            // op so directory listings and stat-after-lookup are
            // attribute-consistent.
            let attr = self.inner.getattr(e.inode).await.map_err(fs_err_to_nfs)?;
            out.push(NfsDirEntry {
                fileid: e.inode,
                name: e.name.as_bytes().into(),
                attr: to_fattr3(attr),
            });
        }
        Ok(ReadDirResult { entries: out, end })
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let target = self.inner.readlink(id).await.map_err(fs_err_to_nfs)?;
        Ok(target.into_bytes().into())
    }

    // ------------------------------------------------------------------
    // Write side (M6) — dispatches into JjYakFs.
    // ------------------------------------------------------------------

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        self.inner
            .write(id, offset, data)
            .await
            .map_err(fs_err_to_nfs)?;
        // NFS expects the post-write attrs back so the client doesn't
        // need to round-trip a separate getattr.
        let attr = self.inner.getattr(id).await.map_err(fs_err_to_nfs)?;
        Ok(to_fattr3(attr))
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = name_to_string(filename)?;
        let executable = mode_executable(&attr);
        let (id, a) = self
            .inner
            .create_file(dirid, &name, executable)
            .await
            .map_err(fs_err_to_nfs)?;
        // NFS create can also carry a size — usually used for truncation
        // of an O_TRUNC create. Apply it inline so the client doesn't
        // need a separate setattr.
        let a = if let Some(size) = size_value(&attr) {
            self.inner
                .setattr(id, Some(size), None)
                .await
                .map_err(fs_err_to_nfs)?
        } else {
            a
        };
        Ok((id, to_fattr3(a)))
    }

    /// Exclusive create succeeds iff the name doesn't exist. Our
    /// `create_file` is already exclusive (returns `AlreadyExists` on
    /// collision), so this is a thin wrapper.
    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let name = name_to_string(filename)?;
        let (id, _) = self
            .inner
            .create_file(dirid, &name, false)
            .await
            .map_err(fs_err_to_nfs)?;
        Ok(id)
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let executable = mode_value(&setattr).map(|m| (m & 0o111) != 0);
        let size = size_value(&setattr);
        // uid/gid/atime/mtime are silently ignored — see module doc
        // (no on-tree representation for them).
        let attr = self
            .inner
            .setattr(id, size, executable)
            .await
            .map_err(fs_err_to_nfs)?;
        Ok(to_fattr3(attr))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let name = name_to_string(filename)?;
        self.inner
            .remove(dirid, &name)
            .await
            .map_err(fs_err_to_nfs)
    }

    /// Rename. Required for jj-lib's atomic-write-via-temp-then-rename
    /// pattern (index segments, opheads, refs). The adapter just
    /// translates filenames to UTF-8 and dispatches to `JjYakFs::rename`,
    /// which holds the POSIX semantics. Returns `NFS3ERR_INVAL` for
    /// non-UTF-8 names — same convention as `lookup`.
    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let from = name_to_string(from_filename)?;
        let to = name_to_string(to_filename)?;
        self.inner
            .rename(from_dirid, &from, to_dirid, &to)
            .await
            .map_err(fs_err_to_nfs)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = name_to_string(dirname)?;
        let (id, attr) = self
            .inner
            .mkdir(dirid, &name)
            .await
            .map_err(fs_err_to_nfs)?;
        Ok((id, to_fattr3(attr)))
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = name_to_string(linkname)?;
        let target = std::str::from_utf8(symlink.as_ref())
            .map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let (id, attr) = self
            .inner
            .symlink(dirid, &name, target)
            .await
            .map_err(fs_err_to_nfs)?;
        Ok((id, to_fattr3(attr)))
    }
}

// ---- sattr3 helpers ----------------------------------------------------

/// Pull a numeric mode out of an sattr3, if the caller set one.
fn mode_value(attr: &sattr3) -> Option<mode3> {
    match attr.mode {
        set_mode3::mode(m) => Some(m),
        set_mode3::Void => None,
    }
}

/// Pull a target file size out of an sattr3, if the caller set one.
fn size_value(attr: &sattr3) -> Option<u64> {
    match attr.size {
        set_size3::size(s) => Some(s),
        set_size3::Void => None,
    }
}

/// Whether the (optionally-set) mode bits make this an executable file.
/// "No mode set" means "not executable" for create — matches the FUSE
/// adapter's interpretation.
fn mode_executable(attr: &sattr3) -> bool {
    mode_value(attr).map(|m| (m & 0o111) != 0).unwrap_or(false)
}

// uid/gid setters are accepted on the wire but ignored end-to-end.
// Keep the imports referenced so dead-code detection doesn't strip them.
const _: fn() = || {
    let _ = std::mem::size_of::<set_uid3>();
    let _ = std::mem::size_of::<set_gid3>();
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::store::Store;
    use crate::ty::{File, Tree, TreeEntry, TreeEntryMapping};
    use crate::vfs::yak_fs::YakFs;

    use super::*;

    /// Build the same tiny tree the YakFs tests use, scaled down. Verifies
    /// the NFS adapter passes through to the trait without losing fidelity
    /// (kind, size, exec bit) and that attribute conversion is consistent.
    fn build_adapter() -> NfsAdapter {
        let store = Arc::new(Store::new());
        let hello_id = store.write_file(File {
            content: b"hi".to_vec(),
        });
        let root = Tree {
            entries: vec![TreeEntryMapping {
                name: "hello.txt".into(),
                entry: TreeEntry::File {
                    id: hello_id,
                    executable: false,
                    copy_id: Vec::new(),
                },
            }],
        };
        let root_id = store.write_tree(root);
        let yak: Arc<dyn JjYakFs> = Arc::new(YakFs::new(store, root_id));
        NfsAdapter::new(yak)
    }

    #[tokio::test]
    async fn root_attrs_resolve() {
        let nfs = build_adapter();
        let attr = nfs.getattr(nfs.root_dir()).await.expect("getattr");
        assert!(matches!(attr.ftype, ftype3::NF3DIR));
    }

    #[tokio::test]
    async fn lookup_then_read_round_trips_through_adapter() {
        let nfs = build_adapter();
        let id = nfs
            .lookup(nfs.root_dir(), &b"hello.txt"[..].into())
            .await
            .expect("lookup");
        let attr = nfs.getattr(id).await.expect("getattr");
        assert!(matches!(attr.ftype, ftype3::NF3REG));
        assert_eq!(attr.size, 2);
        let (data, eof) = nfs.read(id, 0, 16).await.expect("read");
        assert_eq!(data, b"hi");
        assert!(eof);
    }

    #[tokio::test]
    async fn readdir_returns_entries_with_attrs() {
        let nfs = build_adapter();
        let result = nfs.readdir(nfs.root_dir(), 0, 100).await.expect("readdir");
        assert!(result.end);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].name.as_ref(), b"hello.txt");
        assert!(matches!(result.entries[0].attr.ftype, ftype3::NF3REG));
    }

    /// `write` against a directory inode surfaces NFS3ERR_ISDIR (mapped
    /// from `FsError::NotAFile`). Smoke-test that the M6 dispatch
    /// reaches `JjYakFs::write` rather than short-circuiting at the
    /// adapter level.
    #[tokio::test]
    async fn write_on_directory_is_isdir() {
        let nfs = build_adapter();
        let err = nfs
            .write(nfs.root_dir(), 0, b"x")
            .await
            .expect_err("write to a directory must fail");
        assert!(matches!(err, nfsstat3::NFS3ERR_ISDIR), "got {err:?}");
    }

    /// `create` + `write` round-trip via the NFS adapter: the new file is
    /// addressable, its content is readable, and `getattr` reports the
    /// post-write size.
    #[tokio::test]
    async fn create_then_write_then_read_round_trips() {
        let nfs = build_adapter();
        let attr = nfs
            .create(
                nfs.root_dir(),
                &b"new.txt"[..].into(),
                sattr3::default(),
            )
            .await
            .expect("create");
        let id = attr.0;
        let post_attr = nfs.write(id, 0, b"hello").await.expect("write");
        assert_eq!(post_attr.size, 5);
        let (data, eof) = nfs.read(id, 0, 1024).await.expect("read");
        assert_eq!(data, b"hello");
        assert!(eof);
    }

    /// `mkdir` + `lookup` round-trip via the NFS adapter.
    #[tokio::test]
    async fn mkdir_then_lookup_round_trips() {
        let nfs = build_adapter();
        let (id, attr) = nfs
            .mkdir(nfs.root_dir(), &b"sub"[..].into())
            .await
            .expect("mkdir");
        assert!(matches!(attr.ftype, ftype3::NF3DIR));
        let looked = nfs
            .lookup(nfs.root_dir(), &b"sub"[..].into())
            .await
            .expect("lookup");
        assert_eq!(looked, id);
    }

    /// `symlink` reports `NF3LNK` on getattr and the target survives a
    /// readlink round-trip.
    #[tokio::test]
    async fn symlink_then_readlink_round_trips() {
        let nfs = build_adapter();
        let (id, attr) = nfs
            .symlink(
                nfs.root_dir(),
                &b"link"[..].into(),
                &b"target"[..].into(),
                &sattr3::default(),
            )
            .await
            .expect("symlink");
        assert!(matches!(attr.ftype, ftype3::NF3LNK));
        let target = nfs.readlink(id).await.expect("readlink");
        assert_eq!(target.as_ref(), b"target");
    }
}
