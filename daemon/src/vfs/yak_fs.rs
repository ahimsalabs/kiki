//! `JjYakFs` — the read-side trait the per-mount filesystem exposes — and
//! `YakFs`, its concrete implementation backed by [`crate::store::Store`].
//!
//! The trait exists so the NFS and FUSE adapters can share a single
//! tree-walking codebase: `daemon/src/vfs/{nfs_adapter,fuse_adapter}.rs`
//! both wrap an `Arc<dyn JjYakFs>` and translate between the wire
//! protocol's reply types and the domain types defined here.
//!
//! M3 surface: `lookup`, `getattr`, `read`, `readdir`, `readlink`. Mutations
//! (`write`, `create`, `mkdir`, `setattr`, …) land at M5/M6 and are
//! intentionally absent — the adapters return ROFS / ENOSYS for now.
//!
//! Errors are converted to wire types in the adapters (`fs_err_to_nfs`,
//! `fs_err_to_errno`) so the same domain code maps to both protocols
//! without duplicating the match arms.

use std::sync::Arc;

use async_trait::async_trait;

use crate::store::Store;
use crate::ty::{Id, Tree, TreeEntry};
use crate::vfs::inode::{Inode, InodeId, InodeSlab, NodeRef, ROOT_INODE};

/// Kind of filesystem object. Smaller than the NFS `ftype3` and FUSE
/// `FileType` enums; we never serve block/char/socket/fifo nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular,
    Directory,
    Symlink,
}

/// Domain-level attributes. Adapters fill in the rest of the wire-level
/// `fattr3`/`FileAttr` (timestamps, uid/gid, nlink, blksize) themselves so
/// this stays focused on what the store actually knows.
#[derive(Debug, Clone, Copy)]
pub struct Attr {
    pub inode: InodeId,
    pub kind: FileKind,
    /// Byte size: file content length, or 0 for directories. Symlink size
    /// is the target string's byte length, per POSIX.
    pub size: u64,
    /// Only meaningful for `Regular`. Mirrors `TreeEntry::File.executable`.
    pub executable: bool,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub inode: InodeId,
    pub name: String,
    /// Kind is currently consumed only by tests and by the upcoming
    /// FUSE adapter (which needs it for `DirectoryEntry::kind`); the NFS
    /// adapter reaches for it via `getattr` so the same code path
    /// produces both directory listings and stat-after-lookup attrs.
    #[allow(dead_code)]
    pub kind: FileKind,
}

/// Filesystem-level error. Both adapters map this to their own wire error
/// type (`nfsstat3`, `Errno`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    /// Inode not in the slab, or directory entry not found by name.
    NotFound,
    /// A directory operation hit a file or symlink.
    NotADirectory,
    /// A file operation (read) hit a directory or symlink.
    NotAFile,
    /// `readlink` on a non-symlink.
    NotASymlink,
    /// A tree, file, or symlink id present in an inode is missing from
    /// the store. Should be impossible with the current hash-map store;
    /// will become real once Layer B introduces lazy loading.
    StoreMiss,
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            FsError::NotFound => "inode or directory entry not found",
            FsError::NotADirectory => "not a directory",
            FsError::NotAFile => "not a regular file",
            FsError::NotASymlink => "not a symlink",
            FsError::StoreMiss => "missing entry in content store",
        };
        f.write_str(s)
    }
}

impl std::error::Error for FsError {}

#[async_trait]
pub trait JjYakFs: Send + Sync + std::fmt::Debug {
    /// Inode id for the root directory. Always `ROOT_INODE` for the
    /// default impl; exposed on the trait so adapters needn't import the
    /// constant directly.
    fn root(&self) -> InodeId {
        ROOT_INODE
    }

    /// Resolve `name` within `parent`. On success the returned inode id
    /// is stable for the lifetime of this `JjYakFs` instance.
    async fn lookup(&self, parent: InodeId, name: &str) -> Result<InodeId, FsError>;

    async fn getattr(&self, ino: InodeId) -> Result<Attr, FsError>;

    /// Read up to `count` bytes starting at `offset`. Returns `(data, eof)`,
    /// where `eof` is true when the read consumed the rest of the file
    /// (matching `nfsserve::vfs::NFSFileSystem::read`'s contract).
    async fn read(
        &self,
        ino: InodeId,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), FsError>;

    /// Full child listing of a directory. Adapters paginate as required
    /// by their wire protocol; `JjYakFs` always returns everything in
    /// one shot since per-mount trees are small (a workspace, not a
    /// crawl target).
    async fn readdir(&self, dir: InodeId) -> Result<Vec<DirEntry>, FsError>;

    async fn readlink(&self, ino: InodeId) -> Result<String, FsError>;

    /// Re-root the filesystem at `new_root_tree`. Subsequent reads through
    /// this `JjYakFs` (and through any kernel mount the adapters expose)
    /// see the new tree.
    ///
    /// `new_root_tree` must already be present in the backing store —
    /// returns `FsError::StoreMiss` otherwise. The daemon's `CheckOut`
    /// RPC handler turns that into `failed_precondition`.
    async fn check_out(&self, new_root_tree: Id) -> Result<(), FsError>;
}

/// Concrete `JjYakFs` backed by a [`Store`] and the inode slab.
#[derive(Debug)]
pub struct YakFs {
    store: Arc<Store>,
    slab: InodeSlab,
}

impl YakFs {
    /// Build a new mount-side filesystem rooted at `root_tree`.
    ///
    /// The root tree must already be in `store` — `Store::get_tree` is
    /// called lazily on the first `lookup`/`readdir`. Constructing with
    /// the store's empty tree id (the M1 default for a fresh mount) is
    /// the common case and yields an empty directory.
    pub fn new(store: Arc<Store>, root_tree: Id) -> Self {
        YakFs {
            store,
            slab: InodeSlab::new(root_tree),
        }
    }

    fn read_tree(&self, id: Id) -> Result<Tree, FsError> {
        self.store.get_tree(id).ok_or(FsError::StoreMiss)
    }

    /// Map a `TreeEntry` (jj's on-tree type) to a `NodeRef` (our slab type).
    /// Conflict entries are surfaced as opaque files for now — making them
    /// addressable is a real fix that should land alongside conflict
    /// rendering, well after M3.
    fn entry_to_node(entry: &TreeEntry) -> NodeRef {
        match entry {
            TreeEntry::File {
                id, executable, ..
            } => NodeRef::File {
                id: *id,
                executable: *executable,
            },
            TreeEntry::TreeId(id) => NodeRef::Tree(*id),
            TreeEntry::SymlinkId(id) => NodeRef::Symlink(*id),
            TreeEntry::ConflictId(id) => NodeRef::File {
                id: *id,
                executable: false,
            },
        }
    }

    fn entry_kind(entry: &TreeEntry) -> FileKind {
        match entry {
            TreeEntry::TreeId(_) => FileKind::Directory,
            TreeEntry::SymlinkId(_) => FileKind::Symlink,
            TreeEntry::File { .. } | TreeEntry::ConflictId(_) => FileKind::Regular,
        }
    }

    fn dir_tree(&self, inode: &Inode) -> Result<Tree, FsError> {
        match inode.node {
            NodeRef::Tree(id) => self.read_tree(id),
            _ => Err(FsError::NotADirectory),
        }
    }
}

#[async_trait]
impl JjYakFs for YakFs {
    async fn lookup(&self, parent: InodeId, name: &str) -> Result<InodeId, FsError> {
        let parent_inode = self.slab.get(parent).ok_or(FsError::NotFound)?;
        let tree = self.dir_tree(&parent_inode)?;
        let mapping = tree
            .entries
            .iter()
            .find(|m| m.name == name)
            .ok_or(FsError::NotFound)?;
        let node = Self::entry_to_node(&mapping.entry);
        Ok(self.slab.intern_child(parent, name, || node))
    }

    async fn getattr(&self, ino: InodeId) -> Result<Attr, FsError> {
        let inode = self.slab.get(ino).ok_or(FsError::NotFound)?;
        let attr = match inode.node {
            NodeRef::Tree(_) => Attr {
                inode: ino,
                kind: FileKind::Directory,
                // Directory size is meaningless on both NFS and FUSE; the
                // kernel ignores it for `NF3DIR` / `FileType::Directory`
                // and tools like `ls -l` show the link count instead.
                size: 0,
                executable: false,
            },
            NodeRef::File { id, executable } => {
                let file = self.store.get_file(id).ok_or(FsError::StoreMiss)?;
                Attr {
                    inode: ino,
                    kind: FileKind::Regular,
                    size: file.content.len() as u64,
                    executable,
                }
            }
            NodeRef::Symlink(id) => {
                let symlink = self.store.get_symlink(id).ok_or(FsError::StoreMiss)?;
                Attr {
                    inode: ino,
                    kind: FileKind::Symlink,
                    size: symlink.target.len() as u64,
                    executable: false,
                }
            }
        };
        Ok(attr)
    }

    async fn read(
        &self,
        ino: InodeId,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), FsError> {
        let inode = self.slab.get(ino).ok_or(FsError::NotFound)?;
        let id = match inode.node {
            NodeRef::File { id, .. } => id,
            _ => return Err(FsError::NotAFile),
        };
        let file = self.store.get_file(id).ok_or(FsError::StoreMiss)?;
        let len = file.content.len() as u64;
        // Read past the end is legal (NFS spec; FUSE just gets shorter
        // data). Return empty + EOF for the offset-past-EOF case.
        if offset >= len {
            return Ok((Vec::new(), true));
        }
        let end = (offset + count as u64).min(len);
        let data = file.content[offset as usize..end as usize].to_vec();
        let eof = end == len;
        Ok((data, eof))
    }

    async fn readdir(&self, dir: InodeId) -> Result<Vec<DirEntry>, FsError> {
        let inode = self.slab.get(dir).ok_or(FsError::NotFound)?;
        let tree = self.dir_tree(&inode)?;
        let mut out = Vec::with_capacity(tree.entries.len());
        for mapping in &tree.entries {
            let kind = Self::entry_kind(&mapping.entry);
            let node = Self::entry_to_node(&mapping.entry);
            // Intern even if the kernel never looked the name up directly,
            // so subsequent `getattr` against the returned id resolves.
            let id = self.slab.intern_child(dir, &mapping.name, || node);
            out.push(DirEntry {
                inode: id,
                name: mapping.name.clone(),
                kind,
            });
        }
        Ok(out)
    }

    async fn readlink(&self, ino: InodeId) -> Result<String, FsError> {
        let inode = self.slab.get(ino).ok_or(FsError::NotFound)?;
        match inode.node {
            NodeRef::Symlink(id) => self
                .store
                .get_symlink(id)
                .ok_or(FsError::StoreMiss)
                .map(|s| s.target),
            _ => Err(FsError::NotASymlink),
        }
    }

    async fn check_out(&self, new_root_tree: Id) -> Result<(), FsError> {
        // Validate the target tree is present before we touch the slab —
        // otherwise we'd swap the root to an unreadable id and surface
        // every subsequent lookup as `StoreMiss`.
        let _ = self.read_tree(new_root_tree)?;
        self.slab.swap_root(new_root_tree);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::store::Store;
    use crate::ty::{File, Symlink, Tree, TreeEntry, TreeEntryMapping};

    use super::*;

    /// Build a small synthetic repo on a fresh store:
    /// ```text
    /// /
    /// ├── hello.txt          file "hi\n"
    /// ├── bin/
    /// │   └── tool           executable file "x"
    /// └── link               symlink -> "hello.txt"
    /// ```
    /// Returns `(store, root_tree_id)`.
    async fn build_synthetic_tree() -> (Arc<Store>, Id) {
        let store = Arc::new(Store::new());
        let hello_id = store
            .write_file(File {
                content: b"hi\n".to_vec(),
            })
            .await;
        let tool_id = store
            .write_file(File {
                content: b"x".to_vec(),
            })
            .await;
        let link_id = store
            .write_symlink(Symlink {
                target: "hello.txt".into(),
            })
            .await;
        let bin_tree = Tree {
            entries: vec![TreeEntryMapping {
                name: "tool".into(),
                entry: TreeEntry::File {
                    id: tool_id,
                    executable: true,
                    copy_id: Vec::new(),
                },
            }],
        };
        let bin_id = store.write_tree(bin_tree).await;
        let root = Tree {
            entries: vec![
                TreeEntryMapping {
                    name: "bin".into(),
                    entry: TreeEntry::TreeId(bin_id),
                },
                TreeEntryMapping {
                    name: "hello.txt".into(),
                    entry: TreeEntry::File {
                        id: hello_id,
                        executable: false,
                        copy_id: Vec::new(),
                    },
                },
                TreeEntryMapping {
                    name: "link".into(),
                    entry: TreeEntry::SymlinkId(link_id),
                },
            ],
        };
        let root_id = store.write_tree(root).await;
        (store, root_id)
    }

    #[tokio::test]
    async fn empty_repo_has_only_root() {
        let store = Arc::new(Store::new());
        let fs = YakFs::new(store.clone(), store.get_empty_tree_id());
        let entries = fs.readdir(fs.root()).await.expect("readdir empty root");
        assert!(entries.is_empty(), "got {entries:?}");
        let attr = fs.getattr(fs.root()).await.expect("getattr root");
        assert_eq!(attr.kind, FileKind::Directory);
    }

    #[tokio::test]
    async fn lookup_finds_top_level_file() {
        let (store, root) = build_synthetic_tree().await;
        let fs = YakFs::new(store, root);
        let id = fs.lookup(fs.root(), "hello.txt").await.expect("lookup");
        let attr = fs.getattr(id).await.expect("getattr");
        assert_eq!(attr.kind, FileKind::Regular);
        assert_eq!(attr.size, 3);
        assert!(!attr.executable);
    }

    #[tokio::test]
    async fn lookup_traverses_subdirectory() {
        let (store, root) = build_synthetic_tree().await;
        let fs = YakFs::new(store, root);
        let bin_id = fs.lookup(fs.root(), "bin").await.expect("bin");
        let bin_attr = fs.getattr(bin_id).await.expect("getattr bin");
        assert_eq!(bin_attr.kind, FileKind::Directory);
        let tool_id = fs.lookup(bin_id, "tool").await.expect("tool");
        let tool_attr = fs.getattr(tool_id).await.expect("getattr tool");
        assert_eq!(tool_attr.kind, FileKind::Regular);
        assert!(tool_attr.executable);
    }

    #[tokio::test]
    async fn lookup_is_idempotent() {
        let (store, root) = build_synthetic_tree().await;
        let fs = YakFs::new(store, root);
        let a = fs.lookup(fs.root(), "hello.txt").await.unwrap();
        let b = fs.lookup(fs.root(), "hello.txt").await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn read_returns_file_content_with_eof_flag() {
        let (store, root) = build_synthetic_tree().await;
        let fs = YakFs::new(store, root);
        let id = fs.lookup(fs.root(), "hello.txt").await.unwrap();

        // Whole file in one shot.
        let (data, eof) = fs.read(id, 0, 1024).await.unwrap();
        assert_eq!(data, b"hi\n");
        assert!(eof);

        // Partial read, no EOF yet.
        let (data, eof) = fs.read(id, 0, 1).await.unwrap();
        assert_eq!(data, b"h");
        assert!(!eof);

        // Read past EOF returns empty + EOF.
        let (data, eof) = fs.read(id, 99, 1024).await.unwrap();
        assert!(data.is_empty());
        assert!(eof);
    }

    #[tokio::test]
    async fn readdir_lists_all_top_level_entries_with_kind() {
        let (store, root) = build_synthetic_tree().await;
        let fs = YakFs::new(store, root);
        let mut entries = fs.readdir(fs.root()).await.unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["bin", "hello.txt", "link"]);
        let kinds: Vec<_> = entries.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![FileKind::Directory, FileKind::Regular, FileKind::Symlink]
        );
    }

    #[tokio::test]
    async fn readlink_returns_target() {
        let (store, root) = build_synthetic_tree().await;
        let fs = YakFs::new(store, root);
        let id = fs.lookup(fs.root(), "link").await.unwrap();
        assert_eq!(fs.readlink(id).await.unwrap(), "hello.txt");
    }

    #[tokio::test]
    async fn lookup_unknown_name_is_not_found() {
        let (store, root) = build_synthetic_tree().await;
        let fs = YakFs::new(store, root);
        let err = fs.lookup(fs.root(), "missing").await.unwrap_err();
        assert_eq!(err, FsError::NotFound);
    }

    #[tokio::test]
    async fn read_on_directory_is_not_a_file() {
        let (store, root) = build_synthetic_tree().await;
        let fs = YakFs::new(store, root);
        let err = fs.read(fs.root(), 0, 16).await.unwrap_err();
        assert_eq!(err, FsError::NotAFile);
    }

    #[tokio::test]
    async fn readdir_on_file_is_not_a_directory() {
        let (store, root) = build_synthetic_tree().await;
        let fs = YakFs::new(store, root);
        let id = fs.lookup(fs.root(), "hello.txt").await.unwrap();
        let err = fs.readdir(id).await.unwrap_err();
        assert_eq!(err, FsError::NotADirectory);
    }

    #[tokio::test]
    async fn getattr_on_unknown_inode_is_not_found() {
        let (store, root) = build_synthetic_tree().await;
        let fs = YakFs::new(store, root);
        let err = fs.getattr(99_999).await.unwrap_err();
        assert_eq!(err, FsError::NotFound);
    }

    /// `check_out` swaps the visible root tree. Lookups after the swap
    /// must see the new tree's children and not the old's.
    #[tokio::test]
    async fn check_out_swaps_visible_tree() {
        let store = Arc::new(Store::new());
        // Build two distinct one-file root trees.
        let a_id = store
            .write_file(File {
                content: b"a-content".to_vec(),
            })
            .await;
        let b_id = store
            .write_file(File {
                content: b"b-content".to_vec(),
            })
            .await;
        let tree_a = store
            .write_tree(Tree {
                entries: vec![TreeEntryMapping {
                    name: "only-in-a.txt".into(),
                    entry: TreeEntry::File {
                        id: a_id,
                        executable: false,
                        copy_id: Vec::new(),
                    },
                }],
            })
            .await;
        let tree_b = store
            .write_tree(Tree {
                entries: vec![TreeEntryMapping {
                    name: "only-in-b.txt".into(),
                    entry: TreeEntry::File {
                        id: b_id,
                        executable: false,
                        copy_id: Vec::new(),
                    },
                }],
            })
            .await;

        let fs = YakFs::new(store, tree_a);
        // Tree A is visible.
        fs.lookup(fs.root(), "only-in-a.txt").await.expect("A");
        assert_eq!(
            fs.lookup(fs.root(), "only-in-b.txt").await.unwrap_err(),
            FsError::NotFound
        );

        // Swap to tree B.
        fs.check_out(tree_b).await.expect("check_out");
        fs.lookup(fs.root(), "only-in-b.txt").await.expect("B");
        assert_eq!(
            fs.lookup(fs.root(), "only-in-a.txt").await.unwrap_err(),
            FsError::NotFound
        );
    }

    /// Checking out a tree id that isn't in the store must surface a
    /// `StoreMiss` so the daemon can refuse the RPC cleanly instead of
    /// quietly swapping to an unreadable root.
    #[tokio::test]
    async fn check_out_unknown_tree_is_store_miss() {
        let (store, root) = build_synthetic_tree().await;
        let fs = YakFs::new(store, root);
        let err = fs.check_out(Id([0xff; 32])).await.unwrap_err();
        assert_eq!(err, FsError::StoreMiss);
        // Original tree is still visible.
        fs.lookup(fs.root(), "hello.txt").await.expect("still A");
    }
}
