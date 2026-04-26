//! `fuse3::raw::Filesystem` adapter over [`JjYakFs`].
//!
//! Linux primary path. The kernel mount is wired up at M4
//! (`Session::mount_with_unprivileged` from the `fuse3` crate). M5 added
//! the check-out write path; M6 fills in the rest of the mutation
//! surface (`create`, `mkdir`, `symlink`, `write`, `setattr`,
//! `unlink`/`rmdir`).
//!
//! Notes on the fuse3 trait:
//!
//! - `lookup`/`getattr` carry a `Request` (uid/gid/pid of the caller).
//!   We ignore it for now — single-user mount, no permission checks.
//! - `readdir` returns a `ReplyDirectory<DirEntryStream<'_>>` where the
//!   stream is an associated type. The simplest concrete stream is
//!   `futures::stream::Iter<vec::IntoIter<…>>` since `JjYakFs::readdir`
//!   already returns the full listing.
//! - The `.` and `..` entries are added by the adapter, not by `JjYakFs`,
//!   because their inode numbers are protocol-specific.
//! - `create` and `open` return a stateless `fh = 0`. The kernel echoes
//!   it back on subsequent reads/writes; we don't track per-handle
//!   state.

#![allow(dead_code)]

use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fuse3::raw::reply::{
    DirectoryEntry, DirectoryEntryPlus, FileAttr, ReplyAttr, ReplyCreated, ReplyData,
    ReplyDirectory, ReplyDirectoryPlus, ReplyEntry, ReplyInit, ReplyOpen, ReplyWrite,
};
use fuse3::raw::{Filesystem, Request};
use fuse3::{Errno, FileType, Inode, Result as FuseResult, SetAttr, Timestamp};
use futures::stream;

use crate::vfs::yak_fs::{Attr, FileKind, FsError, JjYakFs};
use crate::vfs::ROOT_INODE;

#[derive(Debug)]
pub struct FuseAdapter {
    inner: Arc<dyn JjYakFs>,
}

impl FuseAdapter {
    pub fn new(inner: Arc<dyn JjYakFs>) -> Self {
        Self { inner }
    }
}

/// Cache TTL for entry/attr replies. Set to zero so the kernel
/// revalidates every `getattr`/`lookup` over the FUSE channel. The
/// "right" answer is to push `notify_inval_inode` / `notify_inval_entry`
/// from the daemon when `check_out` rewrites a tree, but `fuse3 0.9`'s
/// `Session::get_notify` is private *and* `mount_with_unprivileged`
/// consumes the `Session`, so there's no public path from a
/// `MountHandle` to a `Notify` today (see `docs/PLAN.md` §7 #9).
///
/// Localhost FUSE round-trip is sub-100µs and editor workloads issue
/// O(20) syscalls per file open, so revalidation cost is dominated by
/// daemon dispatch — acceptable until we migrate to `fuser` (whose
/// `Session::notifier()` is public and `Notifier` is `Clone`). At that
/// point this constant goes back up and `JujutsuService::check_out`
/// drives invalidation explicitly.
const TTL: Duration = Duration::ZERO;

fn fs_err_to_errno(e: FsError) -> Errno {
    let raw: i32 = match e {
        FsError::NotFound => libc::ENOENT,
        FsError::NotADirectory => libc::ENOTDIR,
        FsError::NotAFile => libc::EISDIR,
        FsError::NotASymlink => libc::EINVAL,
        FsError::AlreadyExists => libc::EEXIST,
        FsError::NotEmpty => libc::ENOTEMPTY,
        FsError::StoreMiss => libc::EIO,
    };
    raw.into()
}

fn name_to_str(name: &OsStr) -> Result<&str, Errno> {
    // jj's tree-entry names are `String`; non-UTF-8 components can't
    // address anything in the tree. Surface as ENOENT so the kernel
    // doesn't keep retrying on the same path.
    name.to_str().ok_or_else(|| Errno::from(libc::ENOENT))
}

fn file_kind_to_fuse(kind: FileKind) -> FileType {
    match kind {
        FileKind::Regular => FileType::RegularFile,
        FileKind::Directory => FileType::Directory,
        FileKind::Symlink => FileType::Symlink,
    }
}

fn to_file_attr(a: Attr) -> FileAttr {
    let perm: u16 = match a.kind {
        FileKind::Directory => 0o755,
        FileKind::Symlink => 0o777,
        FileKind::Regular if a.executable => 0o755,
        FileKind::Regular => 0o644,
    };
    FileAttr {
        ino: a.inode,
        size: a.size,
        // Block count: rounded up to 512-byte sectors. POSIX expects this
        // for `du` to work; kernel computes nothing from it on our path.
        blocks: a.size.div_ceil(512),
        atime: Timestamp::new(0, 0),
        mtime: Timestamp::new(0, 0),
        ctime: Timestamp::new(0, 0),
        kind: file_kind_to_fuse(a.kind),
        perm,
        nlink: 1,
        // Owner is whoever runs the daemon. Real uid/gid mapping is on
        // the table only for multi-user setups, which jj-yak isn't.
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
    }
}

/// Concrete stream type used for `readdir`. `JjYakFs::readdir` already
/// returns everything in one shot, so we wrap it in a `stream::iter`.
type DirStream =
    stream::Iter<std::vec::IntoIter<FuseResult<DirectoryEntry>>>;

impl Filesystem for FuseAdapter {
    async fn init(&self, _req: Request) -> FuseResult<ReplyInit> {
        // 1 MiB max write — matches what nfsserve advertises and what
        // most kernels negotiate by default. M6 may want to raise this
        // once the write path is in place.
        Ok(ReplyInit {
            max_write: NonZeroU32::new(1 << 20).expect("nonzero"),
        })
    }

    async fn destroy(&self, _req: Request) {
        // No state on shutdown. The session driver flushes the kernel
        // queue; nothing for us to do.
    }

    async fn lookup(
        &self,
        _req: Request,
        parent: Inode,
        name: &OsStr,
    ) -> FuseResult<ReplyEntry> {
        let name = name_to_str(name)?;
        let id = self
            .inner
            .lookup(parent, name)
            .await
            .map_err(fs_err_to_errno)?;
        let attr = self.inner.getattr(id).await.map_err(fs_err_to_errno)?;
        Ok(ReplyEntry {
            ttl: TTL,
            attr: to_file_attr(attr),
            // Generation is only meaningful with inode reuse, which we
            // don't do (see `inode.rs`). 0 is the canonical "no
            // generation" value.
            generation: 0,
        })
    }

    async fn getattr(
        &self,
        _req: Request,
        inode: Inode,
        _fh: Option<u64>,
        _flags: u32,
    ) -> FuseResult<ReplyAttr> {
        let attr = self.inner.getattr(inode).await.map_err(fs_err_to_errno)?;
        Ok(ReplyAttr {
            ttl: TTL,
            attr: to_file_attr(attr),
        })
    }

    async fn read(
        &self,
        _req: Request,
        inode: Inode,
        _fh: u64,
        offset: u64,
        size: u32,
    ) -> FuseResult<ReplyData> {
        let (data, _eof) = self
            .inner
            .read(inode, offset, size)
            .await
            .map_err(fs_err_to_errno)?;
        // FUSE doesn't have an explicit EOF flag — the kernel infers it
        // from the returned length. Drop the bool.
        Ok(ReplyData {
            data: Bytes::from(data),
        })
    }

    async fn open(
        &self,
        _req: Request,
        _inode: Inode,
        _flags: u32,
    ) -> FuseResult<ReplyOpen> {
        // Stateless I/O: we don't allocate file handles. A `0` fh tells
        // the kernel to expect that on subsequent read/release calls.
        Ok(ReplyOpen { fh: 0, flags: 0 })
    }

    async fn opendir(
        &self,
        _req: Request,
        _inode: Inode,
        _flags: u32,
    ) -> FuseResult<ReplyOpen> {
        // Same stateless story for directories.
        Ok(ReplyOpen { fh: 0, flags: 0 })
    }

    type DirEntryStream<'a>
        = DirStream
    where
        Self: 'a;

    async fn readdir(
        &self,
        _req: Request,
        parent: Inode,
        _fh: u64,
        offset: i64,
    ) -> FuseResult<ReplyDirectory<Self::DirEntryStream<'_>>> {
        let entries = self
            .inner
            .readdir(parent)
            .await
            .map_err(fs_err_to_errno)?;

        // The kernel expects `.` and `..` as the first two entries on a
        // fresh `readdir`. They're protocol-specific (FUSE includes them;
        // NFS3's `READDIR` does not), so we add them here rather than in
        // `JjYakFs`. Parent inode for `..` falls back to the parent's id
        // if known; the root's `..` points at itself.
        let parent_inode = if parent == ROOT_INODE {
            ROOT_INODE
        } else {
            // Best-effort: getattr on parent isn't available, so we
            // approximate by using the same inode. Real `..` resolution
            // would need the slab to expose `inodes.get(parent).parent`;
            // wire that up if/when something cares.
            parent
        };

        let mut out: Vec<FuseResult<DirectoryEntry>> = Vec::with_capacity(entries.len() + 2);
        out.push(Ok(DirectoryEntry {
            inode: parent,
            kind: FileType::Directory,
            name: OsString::from("."),
            offset: 1,
        }));
        out.push(Ok(DirectoryEntry {
            inode: parent_inode,
            kind: FileType::Directory,
            name: OsString::from(".."),
            offset: 2,
        }));
        // Real entries start at offset 3. `offset` here is the directory
        // cookie from the kernel — it echoes back the last entry's
        // `offset` and asks us to start *after* it. We translate by
        // skipping the first `offset` items in the assembled list.
        for (i, e) in entries.into_iter().enumerate() {
            let next_offset: i64 = (i as i64) + 3;
            out.push(Ok(DirectoryEntry {
                inode: e.inode,
                kind: file_kind_to_fuse(e.kind),
                name: OsString::from(e.name),
                offset: next_offset,
            }));
        }

        // Apply the cookie: drop entries whose offset is <= the kernel's
        // last-seen offset. `offset == 0` means "first call".
        let skip = if offset <= 0 { 0 } else { offset as usize };
        let remaining: Vec<FuseResult<DirectoryEntry>> = out.into_iter().skip(skip).collect();

        Ok(ReplyDirectory {
            entries: stream::iter(remaining),
        })
    }

    async fn readlink(&self, _req: Request, inode: Inode) -> FuseResult<ReplyData> {
        let target = self
            .inner
            .readlink(inode)
            .await
            .map_err(fs_err_to_errno)?;
        Ok(ReplyData {
            data: Bytes::from(target.into_bytes()),
        })
    }

    // ----- M6 write surface --------------------------------------------

    /// Combined create-and-open. The kernel calls this for
    /// `open(O_CREAT|O_WRONLY)` and similar. We synthesize the new file
    /// honouring the executable bit from `mode`, then return a stateless
    /// `fh = 0` so subsequent writes round-trip through `write`.
    async fn create(
        &self,
        _req: Request,
        parent: Inode,
        name: &OsStr,
        mode: u32,
        _flags: u32,
    ) -> FuseResult<ReplyCreated> {
        let name = name_to_str(name)?;
        // The full mode includes file-type bits we don't model. Mask down
        // to the perm bits and treat any-execute as "executable".
        let executable = (mode & 0o111) != 0;
        // Inode id rides in the returned `attr` (`FileAttr.ino`); the
        // `ReplyCreated` doesn't carry a separate inode field.
        let (_ino, attr) = self
            .inner
            .create_file(parent, name, executable)
            .await
            .map_err(fs_err_to_errno)?;
        Ok(ReplyCreated {
            ttl: TTL,
            attr: to_file_attr(attr),
            generation: 0,
            fh: 0,
            flags: 0,
        })
    }

    /// File-or-fifo create. The kernel falls back to `mknod` + `open` on
    /// older kernels that don't support `create`. We only model regular
    /// files; the device fields are ignored.
    async fn mknod(
        &self,
        _req: Request,
        parent: Inode,
        name: &OsStr,
        mode: u32,
        _rdev: u32,
    ) -> FuseResult<ReplyEntry> {
        let name = name_to_str(name)?;
        let executable = (mode & 0o111) != 0;
        let (_ino, attr) = self
            .inner
            .create_file(parent, name, executable)
            .await
            .map_err(fs_err_to_errno)?;
        Ok(ReplyEntry {
            ttl: TTL,
            attr: to_file_attr(attr),
            generation: 0,
        })
    }

    async fn mkdir(
        &self,
        _req: Request,
        parent: Inode,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
    ) -> FuseResult<ReplyEntry> {
        let name = name_to_str(name)?;
        let (_ino, attr) = self
            .inner
            .mkdir(parent, name)
            .await
            .map_err(fs_err_to_errno)?;
        Ok(ReplyEntry {
            ttl: TTL,
            attr: to_file_attr(attr),
            generation: 0,
        })
    }

    async fn symlink(
        &self,
        _req: Request,
        parent: Inode,
        name: &OsStr,
        link: &OsStr,
    ) -> FuseResult<ReplyEntry> {
        let name = name_to_str(name)?;
        let target = name_to_str(link)?;
        let (_ino, attr) = self
            .inner
            .symlink(parent, name, target)
            .await
            .map_err(fs_err_to_errno)?;
        Ok(ReplyEntry {
            ttl: TTL,
            attr: to_file_attr(attr),
            generation: 0,
        })
    }

    async fn unlink(&self, _req: Request, parent: Inode, name: &OsStr) -> FuseResult<()> {
        let name = name_to_str(name)?;
        self.inner
            .remove(parent, name)
            .await
            .map_err(fs_err_to_errno)
    }

    async fn rmdir(&self, _req: Request, parent: Inode, name: &OsStr) -> FuseResult<()> {
        let name = name_to_str(name)?;
        self.inner
            .remove(parent, name)
            .await
            .map_err(fs_err_to_errno)
    }

    async fn write(
        &self,
        _req: Request,
        inode: Inode,
        _fh: u64,
        offset: u64,
        data: &[u8],
        _write_flags: u32,
        _flags: u32,
    ) -> FuseResult<ReplyWrite> {
        let written = self
            .inner
            .write(inode, offset, data)
            .await
            .map_err(fs_err_to_errno)?;
        Ok(ReplyWrite { written })
    }

    /// Maps `SetAttr.size` (truncate) and `SetAttr.mode` (chmod —
    /// derived as "any execute bit"). Other fields (uid/gid/atime/mtime)
    /// are silently no-ops because we don't model them.
    async fn setattr(
        &self,
        _req: Request,
        inode: Inode,
        _fh: Option<u64>,
        set_attr: SetAttr,
    ) -> FuseResult<ReplyAttr> {
        let executable = set_attr.mode.map(|m| (m & 0o111) != 0);
        let attr = self
            .inner
            .setattr(inode, set_attr.size, executable)
            .await
            .map_err(fs_err_to_errno)?;
        Ok(ReplyAttr {
            ttl: TTL,
            attr: to_file_attr(attr),
        })
    }

    /// Rename. jj-lib relies on the standard atomic-write pattern
    /// (write to `.tmpXXXX`, then `rename` to the real name). Without
    /// this, `jj yak init` fails the first time it tries to persist an
    /// index segment / opheads file. POSIX semantics live on `JjYakFs`;
    /// the adapter just translates names and dispatches.
    async fn rename(
        &self,
        _req: Request,
        parent: Inode,
        name: &OsStr,
        new_parent: Inode,
        new_name: &OsStr,
    ) -> FuseResult<()> {
        let name = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        let new_name = new_name
            .to_str()
            .ok_or_else(|| Errno::from(libc::EINVAL))?;
        self.inner
            .rename(parent, name, new_parent, new_name)
            .await
            .map_err(fs_err_to_errno)
    }

    /// `flush` is called on every `close(2)` of a file the kernel has
    /// open. Since `write` already commits to the in-memory buffer
    /// synchronously, there's no buffered state to drain — but the
    /// fuse3 trait defaults to `ENOSYS`, which jj-lib (and other
    /// callers) propagate as a hard error. Return `Ok(())`.
    async fn flush(
        &self,
        _req: Request,
        _inode: Inode,
        _fh: u64,
        _lock_owner: u64,
    ) -> FuseResult<()> {
        Ok(())
    }

    /// `fsync` semantics are "make this durable". jj-lib `fsync`s its
    /// index segment files after writing them. Yak's durability story
    /// lives at Layer B (a real backing store); until then the in-memory
    /// `Store` is "durable enough" within a daemon lifetime, so this is
    /// a no-op rather than `ENOSYS`. Same reasoning as `flush`.
    async fn fsync(
        &self,
        _req: Request,
        _inode: Inode,
        _fh: u64,
        _datasync: bool,
    ) -> FuseResult<()> {
        Ok(())
    }

    /// `fsyncdir` is the directory variant of `fsync`. Same reasoning.
    async fn fsyncdir(
        &self,
        _req: Request,
        _inode: Inode,
        _fh: u64,
        _datasync: bool,
    ) -> FuseResult<()> {
        Ok(())
    }

    /// `release` is the close half of `open`. We hand out `fh = 0` from
    /// `open`/`create` and don't track per-handle state, so there's
    /// nothing to release.
    async fn release(
        &self,
        _req: Request,
        _inode: Inode,
        _fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
    ) -> FuseResult<()> {
        Ok(())
    }

    /// `releasedir` is the close half of `opendir`. Same reasoning as
    /// `release` — no per-handle state to clean up.
    async fn releasedir(
        &self,
        _req: Request,
        _inode: Inode,
        _fh: u64,
        _flags: u32,
    ) -> FuseResult<()> {
        Ok(())
    }

    type DirEntryPlusStream<'a>
        = stream::Iter<std::vec::IntoIter<FuseResult<DirectoryEntryPlus>>>
    where
        Self: 'a;

    /// `readdirplus` is the kernel's preferred readdir on Linux >= 3.9;
    /// fuse3 unconditionally advertises `FUSE_DO_READDIRPLUS` during
    /// `init`, and there's no mount-option to opt out. If our impl
    /// returns `ENOSYS`, the kernel doesn't always fall back to plain
    /// `readdir` (depends on `FUSE_READDIRPLUS_AUTO` heuristics) — `ls`
    /// just sees `getdents64 -> ENOSYS` and gives up. So we have to
    /// implement it.
    ///
    /// Semantically this is `readdir` + `getattr` on every entry; the
    /// reply lets the kernel cache attrs without follow-up `lookup`s.
    /// We `getattr` per entry, which is fine over the in-memory store
    /// but worth revisiting when the FS grows real I/O at Layer B.
    /// `_lock_owner` is unused — single-user mount, no posix locks.
    async fn readdirplus(
        &self,
        _req: Request,
        parent: Inode,
        _fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> FuseResult<ReplyDirectoryPlus<Self::DirEntryPlusStream<'_>>> {
        let entries = self
            .inner
            .readdir(parent)
            .await
            .map_err(fs_err_to_errno)?;

        // Same `..` approximation as `readdir`. The slab doesn't give
        // us a quick hop to the real grandparent; if a caller ever
        // needs an accurate `..` here, plumb it through `JjYakFs`.
        let parent_inode = if parent == ROOT_INODE { ROOT_INODE } else { parent };

        let parent_attr = self
            .inner
            .getattr(parent)
            .await
            .map_err(fs_err_to_errno)?;
        let parent_file_attr = to_file_attr(parent_attr);

        let mut out: Vec<FuseResult<DirectoryEntryPlus>> = Vec::with_capacity(entries.len() + 2);
        out.push(Ok(DirectoryEntryPlus {
            inode: parent,
            generation: 0,
            kind: FileType::Directory,
            name: OsString::from("."),
            offset: 1,
            attr: parent_file_attr,
            entry_ttl: TTL,
            attr_ttl: TTL,
        }));
        out.push(Ok(DirectoryEntryPlus {
            inode: parent_inode,
            generation: 0,
            kind: FileType::Directory,
            name: OsString::from(".."),
            offset: 2,
            attr: parent_file_attr,
            entry_ttl: TTL,
            attr_ttl: TTL,
        }));

        for (i, e) in entries.into_iter().enumerate() {
            // getattr per entry — necessary for the "Plus" half of the
            // reply. Errors here mean the entry vanished between
            // readdir and getattr (race we're not really exposed to on
            // a single-threaded VFS, but be defensive).
            let attr = match self.inner.getattr(e.inode).await {
                Ok(a) => to_file_attr(a),
                Err(err) => {
                    out.push(Err(fs_err_to_errno(err)));
                    continue;
                }
            };
            let next_offset: i64 = (i as i64) + 3;
            out.push(Ok(DirectoryEntryPlus {
                inode: e.inode,
                generation: 0,
                kind: file_kind_to_fuse(e.kind),
                name: OsString::from(e.name),
                offset: next_offset,
                attr,
                entry_ttl: TTL,
                attr_ttl: TTL,
            }));
        }

        let skip = offset as usize;
        let remaining: Vec<FuseResult<DirectoryEntryPlus>> = out.into_iter().skip(skip).collect();

        Ok(ReplyDirectoryPlus {
            entries: stream::iter(remaining.into_iter()),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::store::Store;
    use crate::ty::{File, Tree, TreeEntry, TreeEntryMapping};
    use crate::vfs::yak_fs::YakFs;

    use super::*;

    /// Minimal `Request` for tests. `unique`/`uid`/`gid`/`pid` are all
    /// caller-identity fields we don't care about.
    fn req() -> Request {
        Request {
            unique: 1,
            uid: 0,
            gid: 0,
            pid: 0,
        }
    }

    fn build_adapter() -> FuseAdapter {
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
        FuseAdapter::new(yak)
    }

    #[tokio::test]
    async fn lookup_then_read_round_trips_through_adapter() {
        let fuse = build_adapter();
        let entry = fuse
            .lookup(req(), ROOT_INODE, OsStr::new("hello.txt"))
            .await
            .expect("lookup");
        assert_eq!(entry.attr.kind, FileType::RegularFile);
        assert_eq!(entry.attr.size, 2);
        let data = fuse
            // (request, inode, fh, offset, size). fh is unused (stateless I/O).
            .read(req(), entry.attr.ino, 0, 0, 16)
            .await
            .expect("read");
        assert_eq!(data.data.as_ref(), b"hi");
    }

    #[tokio::test]
    async fn getattr_root_is_directory() {
        let fuse = build_adapter();
        let attr = fuse.getattr(req(), ROOT_INODE, None, 0).await.expect("getattr");
        assert_eq!(attr.attr.kind, FileType::Directory);
    }

    #[tokio::test]
    async fn readdir_includes_dot_and_dotdot_then_real_entries() {
        use futures::StreamExt;
        let fuse = build_adapter();
        let reply = fuse.readdir(req(), ROOT_INODE, 0, 0).await.expect("readdir");
        let entries: Vec<DirectoryEntry> = reply
            .entries
            .filter_map(|r| async move { r.ok() })
            .collect()
            .await;
        let names: Vec<_> = entries.iter().map(|e| e.name.to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec![".".to_string(), "..".to_string(), "hello.txt".to_string()]);
        // `.` and `..` are directories; the file entry must be a regular file.
        assert_eq!(entries[0].kind, FileType::Directory);
        assert_eq!(entries[1].kind, FileType::Directory);
        assert_eq!(entries[2].kind, FileType::RegularFile);
    }

    #[tokio::test]
    async fn readdir_offset_resumes_after_dotdot() {
        use futures::StreamExt;
        let fuse = build_adapter();
        // offset=2 means "I already saw entries with offsets 1 and 2"
        // (i.e. `.` and `..`). Should return only the real entry.
        let reply = fuse.readdir(req(), ROOT_INODE, 0, 2).await.expect("readdir");
        let entries: Vec<DirectoryEntry> = reply
            .entries
            .filter_map(|r| async move { r.ok() })
            .collect()
            .await;
        let names: Vec<_> = entries.iter().map(|e| e.name.to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["hello.txt".to_string()]);
    }

    #[tokio::test]
    async fn lookup_missing_returns_enoent() {
        let fuse = build_adapter();
        let err = fuse
            .lookup(req(), ROOT_INODE, OsStr::new("missing"))
            .await
            .expect_err("expected ENOENT");
        // fuse3's `Errno::into<i32>` returns the *negated* errno because
        // that's the value FUSE wants on the wire (kernel convention:
        // negative errno = error). The unsigned magnitude is what we
        // care about identifying.
        let raw: i32 = err.into();
        assert_eq!(raw.unsigned_abs() as i32, libc::ENOENT);
    }

    /// `create` + `write` + `read` round-trips through the FUSE
    /// adapter's mode/flag plumbing. Smoke-tests that the M6 dispatch
    /// reaches `JjYakFs` rather than short-circuiting at the adapter
    /// surface.
    #[tokio::test]
    async fn create_then_write_then_read_round_trips() {
        let fuse = build_adapter();
        let created = fuse
            .create(req(), ROOT_INODE, OsStr::new("new.txt"), 0o644, 0)
            .await
            .expect("create");
        let written = fuse
            .write(
                req(),
                created.attr.ino,
                0,
                0,
                b"hello",
                0,
                0,
            )
            .await
            .expect("write");
        assert_eq!(written.written, 5);
        let data = fuse
            .read(req(), created.attr.ino, 0, 0, 1024)
            .await
            .expect("read");
        assert_eq!(data.data.as_ref(), b"hello");
    }

    /// `mkdir` + `lookup` round-trip via the FUSE adapter, confirming
    /// the new entry is addressable through the kernel-facing API.
    #[tokio::test]
    async fn mkdir_then_lookup_round_trips() {
        let fuse = build_adapter();
        let created = fuse
            .mkdir(req(), ROOT_INODE, OsStr::new("sub"), 0o755, 0)
            .await
            .expect("mkdir");
        assert_eq!(created.attr.kind, FileType::Directory);
        let looked = fuse
            .lookup(req(), ROOT_INODE, OsStr::new("sub"))
            .await
            .expect("lookup");
        assert_eq!(looked.attr.ino, created.attr.ino);
    }

    /// `setattr(size=0)` truncates a file end-to-end through the FUSE
    /// adapter. Editor `O_TRUNC` opens land here.
    #[tokio::test]
    async fn setattr_truncates_via_adapter() {
        let fuse = build_adapter();
        // `hello.txt` from build_adapter has 2 bytes of "hi". Truncate.
        let entry = fuse
            .lookup(req(), ROOT_INODE, OsStr::new("hello.txt"))
            .await
            .unwrap();
        let sa = SetAttr {
            size: Some(0),
            ..SetAttr::default()
        };
        let after = fuse
            .setattr(req(), entry.attr.ino, None, sa)
            .await
            .expect("setattr");
        assert_eq!(after.attr.size, 0);
    }

    /// `unlink` removes a file; subsequent lookup returns ENOENT.
    #[tokio::test]
    async fn unlink_then_lookup_is_enoent() {
        let fuse = build_adapter();
        fuse.unlink(req(), ROOT_INODE, OsStr::new("hello.txt"))
            .await
            .expect("unlink");
        let err = fuse
            .lookup(req(), ROOT_INODE, OsStr::new("hello.txt"))
            .await
            .expect_err("ENOENT");
        let raw: i32 = err.into();
        assert_eq!(raw.unsigned_abs() as i32, libc::ENOENT);
    }
}
