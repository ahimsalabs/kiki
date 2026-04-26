//! `JjYakFs` — the read+write trait the per-mount filesystem exposes — and
//! `YakFs`, its concrete implementation backed by [`crate::store::Store`].
//!
//! The trait exists so the NFS and FUSE adapters can share a single
//! tree-walking codebase: `daemon/src/vfs/{nfs_adapter,fuse_adapter}.rs`
//! both wrap an `Arc<dyn JjYakFs>` and translate between the wire
//! protocol's reply types and the domain types defined here.
//!
//! ## Read path (M3) and check-out (M5)
//!
//! `lookup` / `getattr` / `read` / `readdir` / `readlink` walk the
//! per-mount [`Store`] starting from the inode the kernel passed in.
//! `check_out` re-roots the slab at a new tree id (M5).
//!
//! ## Write path (M6)
//!
//! `create_file` / `mkdir` / `symlink` / `write` / `setattr` / `remove`
//! mutate the slab's "dirty" `NodeRef` variants (`DirtyTree`, `DirtyFile`,
//! `DirtySymlink` — see `inode.rs`). The first write touching a path
//! lazily promotes the affected inode from clean to dirty by loading the
//! current content from the store; subsequent writes mutate the in-memory
//! buffer in place. `snapshot` walks the slab, persists every dirty blob
//! into the per-mount [`Store`], and returns the rolled-up root tree id.
//! It also "cleans" the slab by replacing the now-persisted dirty refs
//! with their content-addressed ids, so kernel reads after snapshot
//! continue to work and memory doesn't accumulate stale buffers.
//!
//! Errors are converted to wire types in the adapters (`fs_err_to_nfs`,
//! `fs_err_to_errno`) so the same domain code maps to both protocols
//! without duplicating the match arms.

use std::sync::Arc;

use async_trait::async_trait;

use crate::store::Store;
use crate::ty::{File, Id, Symlink, Tree, TreeEntry, TreeEntryMapping};
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
    /// A file operation (read/write) hit a directory or symlink.
    NotAFile,
    /// `readlink` on a non-symlink.
    NotASymlink,
    /// `create` / `mkdir` / `symlink` collided with an existing entry.
    AlreadyExists,
    /// `rmdir` on a directory that still has children.
    NotEmpty,
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
            FsError::AlreadyExists => "entry already exists",
            FsError::NotEmpty => "directory not empty",
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

    // ----- M6: write path ------------------------------------------------

    /// Create a new regular file under `parent` with the given `name`,
    /// initially empty. Returns the new inode + attrs.
    ///
    /// Errors: `NotFound` if `parent` doesn't exist; `NotADirectory` if
    /// `parent` isn't a directory; `AlreadyExists` if `name` is already
    /// taken.
    async fn create_file(
        &self,
        parent: InodeId,
        name: &str,
        executable: bool,
    ) -> Result<(InodeId, Attr), FsError>;

    /// Create a new directory under `parent`. Returns the new inode + attrs.
    async fn mkdir(&self, parent: InodeId, name: &str) -> Result<(InodeId, Attr), FsError>;

    /// Create a new symlink under `parent` pointing at `target`. Returns
    /// the new inode + attrs.
    async fn symlink(
        &self,
        parent: InodeId,
        name: &str,
        target: &str,
    ) -> Result<(InodeId, Attr), FsError>;

    /// Write `data` at `offset` into the file `ino`. Returns the number of
    /// bytes written (always `data.len()` on success — short writes aren't
    /// surfaced today; the in-memory buffer can always grow).
    ///
    /// Files are promoted from `NodeRef::File` (clean) to
    /// `NodeRef::DirtyFile` (in-memory buffer) on the first write — the
    /// existing content is loaded from the store, then `data` is spliced
    /// in at `offset`. Subsequent writes mutate the buffer in place.
    async fn write(&self, ino: InodeId, offset: u64, data: &[u8]) -> Result<u32, FsError>;

    /// Update file attributes. Today only the executable bit and the
    /// truncation length are honoured; everything else (uid/gid/atime/mtime)
    /// is silently ignored because we don't model it in the tree. Pass
    /// `None` for fields you don't want to change.
    ///
    /// Truncation is the most common reason this gets called from the
    /// kernel — `open(O_TRUNC)` and `truncate(2)` both arrive here. We
    /// shrink or zero-extend the dirty file's buffer to `size`.
    async fn setattr(
        &self,
        ino: InodeId,
        size: Option<u64>,
        executable: Option<bool>,
    ) -> Result<Attr, FsError>;

    /// Remove a directory entry from `parent`. Works for files, symlinks,
    /// and empty directories; non-empty directories return
    /// `FsError::NotEmpty`.
    ///
    /// The detached child's inode stays live in the slab so already-issued
    /// kernel handles don't immediately go stale; it just becomes
    /// unreachable through the parent.
    async fn remove(&self, parent: InodeId, name: &str) -> Result<(), FsError>;

    /// Rename a file or directory. POSIX semantics: if `new_name` already
    /// exists at `new_parent`, it is replaced atomically. The orphaned
    /// inode (if any) is left in the slab — same reasoning as `remove`.
    ///
    /// Required for jj-lib's atomic-write-via-temp-then-rename pattern
    /// (used by index segments, opheads, refs, etc.). Without this,
    /// `jj yak init` fails halfway through populating `.jj/`.
    async fn rename(
        &self,
        parent: InodeId,
        name: &str,
        new_parent: InodeId,
        new_name: &str,
    ) -> Result<(), FsError>;

    /// Walk the slab, persisting every dirty blob into the [`Store`], and
    /// return the rolled-up root tree id. After a successful `snapshot`,
    /// every previously-dirty inode is replaced with its clean content-
    /// addressed counterpart so the slab doesn't accumulate stale buffers
    /// — but inode ids are preserved so the kernel doesn't see them
    /// change.
    ///
    /// Returns the new root tree id on success.
    async fn snapshot(&self) -> Result<Id, FsError>;
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

    /// Read the directory tree backing `inode`. Only valid for the clean
    /// `NodeRef::Tree` variant — dirty trees own their children directly,
    /// so callers walking a `DirtyTree` should iterate its `children`
    /// map instead.
    fn dir_tree(&self, inode: &Inode) -> Result<Tree, FsError> {
        match inode.node {
            NodeRef::Tree(id) => self.read_tree(id),
            _ => Err(FsError::NotADirectory),
        }
    }

    /// Promote `parent` from clean `Tree` into `DirtyTree`. Idempotent —
    /// already-dirty parents are left as-is.
    fn ensure_dirty_tree(&self, parent: InodeId) -> Result<(), FsError> {
        let parent_inode = self.slab.get(parent).ok_or(FsError::NotFound)?;
        match parent_inode.node {
            NodeRef::DirtyTree { .. } => Ok(()),
            NodeRef::Tree(id) => {
                let tree = self.read_tree(id)?;
                let entries: Vec<(String, NodeRef)> = tree
                    .entries
                    .into_iter()
                    .map(|m| (m.name, Self::entry_to_node(&m.entry)))
                    .collect();
                // `materialize_dir_for_mutation` runs the loader closure
                // under the slab lock; pre-resolve the tree above so the
                // closure stays trivial.
                self.slab
                    .materialize_dir_for_mutation(parent, move || entries.into_iter())
                    .ok_or(FsError::NotADirectory)?;
                Ok(())
            }
            _ => Err(FsError::NotADirectory),
        }
    }

    /// Promote a clean `File` inode into `DirtyFile` by loading its
    /// content from the store. No-op for already-dirty files. Returns
    /// `NotAFile` for trees and symlinks.
    fn ensure_dirty_file(&self, ino: InodeId) -> Result<(), FsError> {
        let inode = self.slab.get(ino).ok_or(FsError::NotFound)?;
        match inode.node {
            NodeRef::DirtyFile { .. } => Ok(()),
            NodeRef::File { id, executable } => {
                let file = self.store.get_file(id).ok_or(FsError::StoreMiss)?;
                self.slab.replace_node(
                    ino,
                    NodeRef::DirtyFile {
                        content: file.content,
                        executable,
                    },
                );
                Ok(())
            }
            _ => Err(FsError::NotAFile),
        }
    }

    /// Look up a child by name in a (possibly clean) directory inode.
    /// Used by the write path's "does this name already exist?" check.
    fn child_exists(&self, parent: InodeId, name: &str) -> Result<bool, FsError> {
        let parent_inode = self.slab.get(parent).ok_or(FsError::NotFound)?;
        match parent_inode.node {
            NodeRef::DirtyTree { children } => Ok(children.contains_key(name)),
            NodeRef::Tree(id) => {
                let tree = self.read_tree(id)?;
                Ok(tree.entries.iter().any(|m| m.name == name))
            }
            _ => Err(FsError::NotADirectory),
        }
    }

    /// Inflate an inode into the `Attr` shape both adapters consume.
    /// Same logic as `getattr`'s body, exposed as a helper so the write
    /// ops can return an `Attr` for the just-created/just-modified inode
    /// without round-tripping through the trait method.
    fn attr_for(&self, ino: InodeId) -> Result<Attr, FsError> {
        let inode = self.slab.get(ino).ok_or(FsError::NotFound)?;
        match inode.node {
            NodeRef::Tree(_) | NodeRef::DirtyTree { .. } => Ok(Attr {
                inode: ino,
                kind: FileKind::Directory,
                size: 0,
                executable: false,
            }),
            NodeRef::File { id, executable } => {
                let file = self.store.get_file(id).ok_or(FsError::StoreMiss)?;
                Ok(Attr {
                    inode: ino,
                    kind: FileKind::Regular,
                    size: file.content.len() as u64,
                    executable,
                })
            }
            NodeRef::DirtyFile {
                ref content,
                executable,
            } => Ok(Attr {
                inode: ino,
                kind: FileKind::Regular,
                size: content.len() as u64,
                executable,
            }),
            NodeRef::Symlink(id) => {
                let symlink = self.store.get_symlink(id).ok_or(FsError::StoreMiss)?;
                Ok(Attr {
                    inode: ino,
                    kind: FileKind::Symlink,
                    size: symlink.target.len() as u64,
                    executable: false,
                })
            }
            NodeRef::DirtySymlink { ref target } => Ok(Attr {
                inode: ino,
                kind: FileKind::Symlink,
                size: target.len() as u64,
                executable: false,
            }),
        }
    }

    /// Recursive snapshot: walk the inode at `ino`, persist any dirty
    /// content into the store, replace the slab entry with the clean
    /// counterpart, and return the resulting `Id`.
    ///
    /// Sync (no `.await`) so it can recurse without `Box::pin` /
    /// `async-recursion`. Store ops are sync since M6.
    fn snapshot_node(&self, ino: InodeId) -> Result<Id, FsError> {
        let inode = self.slab.get(ino).ok_or(FsError::NotFound)?;
        match inode.node {
            // Clean refs: nothing to do, just hand back the id.
            NodeRef::Tree(id) | NodeRef::File { id, .. } | NodeRef::Symlink(id) => Ok(id),

            NodeRef::DirtyFile {
                content,
                executable,
            } => {
                let id = self.store.write_file(File { content });
                self.slab.replace_node(ino, NodeRef::File { id, executable });
                Ok(id)
            }

            NodeRef::DirtySymlink { target } => {
                let id = self.store.write_symlink(Symlink { target });
                self.slab.replace_node(ino, NodeRef::Symlink(id));
                Ok(id)
            }

            NodeRef::DirtyTree { children } => {
                // Recurse first, gather the canonical entries, then write
                // the tree. BTreeMap iteration is name-sorted, which is
                // also the order we hash entries in (see Tree::ContentHash
                // derivation in ty.rs) — so two equivalent dirty trees
                // produce identical tree ids.
                let mut entries = Vec::with_capacity(children.len());
                for (name, child_id) in children {
                    let child = self.slab.get(child_id).ok_or(FsError::NotFound)?;
                    let entry = match child.node {
                        NodeRef::Tree(id) => TreeEntry::TreeId(id),
                        NodeRef::DirtyTree { .. } => {
                            TreeEntry::TreeId(self.snapshot_node(child_id)?)
                        }
                        NodeRef::File { id, executable } => TreeEntry::File {
                            id,
                            executable,
                            copy_id: Vec::new(),
                        },
                        NodeRef::DirtyFile { executable, .. } => {
                            let id = self.snapshot_node(child_id)?;
                            TreeEntry::File {
                                id,
                                executable,
                                copy_id: Vec::new(),
                            }
                        }
                        NodeRef::Symlink(id) => TreeEntry::SymlinkId(id),
                        NodeRef::DirtySymlink { .. } => {
                            TreeEntry::SymlinkId(self.snapshot_node(child_id)?)
                        }
                    };
                    entries.push(TreeEntryMapping { name, entry });
                }
                let id = self.store.write_tree(Tree { entries });
                self.slab.replace_node(ino, NodeRef::Tree(id));
                Ok(id)
            }
        }
    }
}

#[async_trait]
impl JjYakFs for YakFs {
    async fn lookup(&self, parent: InodeId, name: &str) -> Result<InodeId, FsError> {
        let parent_inode = self.slab.get(parent).ok_or(FsError::NotFound)?;
        match parent_inode.node {
            NodeRef::DirtyTree { children } => {
                children.get(name).copied().ok_or(FsError::NotFound)
            }
            NodeRef::Tree(id) => {
                let tree = self.read_tree(id)?;
                let mapping = tree
                    .entries
                    .iter()
                    .find(|m| m.name == name)
                    .ok_or(FsError::NotFound)?;
                let node = Self::entry_to_node(&mapping.entry);
                Ok(self.slab.intern_child(parent, name, || node))
            }
            _ => Err(FsError::NotADirectory),
        }
    }

    async fn getattr(&self, ino: InodeId) -> Result<Attr, FsError> {
        self.attr_for(ino)
    }

    async fn read(
        &self,
        ino: InodeId,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), FsError> {
        let inode = self.slab.get(ino).ok_or(FsError::NotFound)?;
        // Dirty files read straight out of the in-memory buffer; clean
        // files round-trip through the store. Same off-end semantics in
        // both cases (NFS allows reads past EOF; FUSE just gets a short
        // reply).
        let content_owned: Option<Vec<u8>>;
        let content: &[u8] = match inode.node {
            NodeRef::DirtyFile { ref content, .. } => content,
            NodeRef::File { id, .. } => {
                let file = self.store.get_file(id).ok_or(FsError::StoreMiss)?;
                content_owned = Some(file.content);
                content_owned.as_deref().expect("just set")
            }
            _ => return Err(FsError::NotAFile),
        };
        let len = content.len() as u64;
        if offset >= len {
            return Ok((Vec::new(), true));
        }
        let end = (offset + count as u64).min(len);
        let data = content[offset as usize..end as usize].to_vec();
        let eof = end == len;
        Ok((data, eof))
    }

    async fn readdir(&self, dir: InodeId) -> Result<Vec<DirEntry>, FsError> {
        let inode = self.slab.get(dir).ok_or(FsError::NotFound)?;
        match inode.node {
            NodeRef::DirtyTree { children } => {
                // For a dirty tree we already have authoritative
                // (name -> inode) entries; just classify each one.
                let mut out = Vec::with_capacity(children.len());
                for (name, child_id) in children {
                    let child = self.slab.get(child_id).ok_or(FsError::NotFound)?;
                    let kind = match child.node {
                        NodeRef::Tree(_) | NodeRef::DirtyTree { .. } => FileKind::Directory,
                        NodeRef::File { .. } | NodeRef::DirtyFile { .. } => FileKind::Regular,
                        NodeRef::Symlink(_) | NodeRef::DirtySymlink { .. } => FileKind::Symlink,
                    };
                    out.push(DirEntry {
                        inode: child_id,
                        name,
                        kind,
                    });
                }
                Ok(out)
            }
            NodeRef::Tree(_) => {
                let tree = self.dir_tree(&inode)?;
                let mut out = Vec::with_capacity(tree.entries.len());
                for mapping in &tree.entries {
                    let kind = Self::entry_kind(&mapping.entry);
                    let node = Self::entry_to_node(&mapping.entry);
                    let id = self.slab.intern_child(dir, &mapping.name, || node);
                    out.push(DirEntry {
                        inode: id,
                        name: mapping.name.clone(),
                        kind,
                    });
                }
                Ok(out)
            }
            _ => Err(FsError::NotADirectory),
        }
    }

    async fn readlink(&self, ino: InodeId) -> Result<String, FsError> {
        let inode = self.slab.get(ino).ok_or(FsError::NotFound)?;
        match inode.node {
            NodeRef::Symlink(id) => self
                .store
                .get_symlink(id)
                .ok_or(FsError::StoreMiss)
                .map(|s| s.target),
            NodeRef::DirtySymlink { target } => Ok(target),
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

    async fn create_file(
        &self,
        parent: InodeId,
        name: &str,
        executable: bool,
    ) -> Result<(InodeId, Attr), FsError> {
        if self.child_exists(parent, name)? {
            return Err(FsError::AlreadyExists);
        }
        self.ensure_dirty_tree(parent)?;
        let child = self.slab.alloc(
            parent,
            name.to_owned(),
            NodeRef::DirtyFile {
                content: Vec::new(),
                executable,
            },
        );
        if !self.slab.attach_child(parent, name.to_owned(), child) {
            // Concurrent creator beat us. Race is unlikely in practice
            // (one CLI per mount, single-threaded write path), but
            // surface it cleanly rather than corrupting state.
            return Err(FsError::AlreadyExists);
        }
        let attr = self.attr_for(child)?;
        Ok((child, attr))
    }

    async fn mkdir(&self, parent: InodeId, name: &str) -> Result<(InodeId, Attr), FsError> {
        if self.child_exists(parent, name)? {
            return Err(FsError::AlreadyExists);
        }
        self.ensure_dirty_tree(parent)?;
        let child = self.slab.alloc(
            parent,
            name.to_owned(),
            NodeRef::DirtyTree {
                children: std::collections::BTreeMap::new(),
            },
        );
        if !self.slab.attach_child(parent, name.to_owned(), child) {
            return Err(FsError::AlreadyExists);
        }
        let attr = self.attr_for(child)?;
        Ok((child, attr))
    }

    async fn symlink(
        &self,
        parent: InodeId,
        name: &str,
        target: &str,
    ) -> Result<(InodeId, Attr), FsError> {
        if self.child_exists(parent, name)? {
            return Err(FsError::AlreadyExists);
        }
        self.ensure_dirty_tree(parent)?;
        let child = self.slab.alloc(
            parent,
            name.to_owned(),
            NodeRef::DirtySymlink {
                target: target.to_owned(),
            },
        );
        if !self.slab.attach_child(parent, name.to_owned(), child) {
            return Err(FsError::AlreadyExists);
        }
        let attr = self.attr_for(child)?;
        Ok((child, attr))
    }

    async fn write(&self, ino: InodeId, offset: u64, data: &[u8]) -> Result<u32, FsError> {
        self.ensure_dirty_file(ino)?;
        let mut inode = self.slab.get(ino).ok_or(FsError::NotFound)?;
        let NodeRef::DirtyFile {
            ref mut content,
            executable,
        } = inode.node
        else {
            // ensure_dirty_file just succeeded so this shouldn't fire,
            // but the type system can't see that across the slab
            // boundary.
            return Err(FsError::NotAFile);
        };
        // Splice `data` in at `offset`, growing the buffer (zero-padded) if
        // the write starts past the current end.
        let end = offset as usize + data.len();
        if content.len() < end {
            content.resize(end, 0);
        }
        content[offset as usize..end].copy_from_slice(data);
        let new = NodeRef::DirtyFile {
            content: std::mem::take(content),
            executable,
        };
        self.slab.replace_node(ino, new);
        Ok(data.len() as u32)
    }

    async fn setattr(
        &self,
        ino: InodeId,
        size: Option<u64>,
        executable: Option<bool>,
    ) -> Result<Attr, FsError> {
        // Both fields touch a file — make sure we're working with a
        // dirty buffer.
        if size.is_some() || executable.is_some() {
            self.ensure_dirty_file(ino)?;
        } else {
            // No-op setattr (e.g. atime-only) just returns current attrs.
            return self.attr_for(ino);
        }

        let mut inode = self.slab.get(ino).ok_or(FsError::NotFound)?;
        let NodeRef::DirtyFile {
            ref mut content,
            executable: ref mut exec_bit,
        } = inode.node
        else {
            return Err(FsError::NotAFile);
        };
        if let Some(new_size) = size {
            content.resize(new_size as usize, 0);
        }
        if let Some(new_exec) = executable {
            *exec_bit = new_exec;
        }
        let new = NodeRef::DirtyFile {
            content: std::mem::take(content),
            executable: *exec_bit,
        };
        self.slab.replace_node(ino, new);
        self.attr_for(ino)
    }

    async fn remove(&self, parent: InodeId, name: &str) -> Result<(), FsError> {
        // Materialize the parent so we can detach by name. If the named
        // child is itself a directory, refuse on a non-empty body —
        // mirrors POSIX `rmdir`.
        self.ensure_dirty_tree(parent)?;

        // Peek at the child to enforce the "directory must be empty" rule
        // before we detach. Look up via the just-materialized
        // `DirtyTree.children` instead of `lookup` — `lookup` is async
        // and we'd still need the same data.
        let parent_inode = self.slab.get(parent).ok_or(FsError::NotFound)?;
        let child_id = match parent_inode.node {
            NodeRef::DirtyTree { ref children } => children
                .get(name)
                .copied()
                .ok_or(FsError::NotFound)?,
            // Parent was just materialized; anything else is a bug.
            _ => return Err(FsError::NotADirectory),
        };
        let child = self.slab.get(child_id).ok_or(FsError::NotFound)?;
        match &child.node {
            NodeRef::DirtyTree { children } if !children.is_empty() => {
                return Err(FsError::NotEmpty)
            }
            NodeRef::Tree(id) => {
                // Empty check on a still-clean child directory: peek the
                // tree without materializing.
                let tree = self.read_tree(*id)?;
                if !tree.entries.is_empty() {
                    return Err(FsError::NotEmpty);
                }
            }
            _ => {}
        }

        self.slab
            .detach_child(parent, name)
            .ok_or(FsError::NotFound)?;
        Ok(())
    }

    async fn rename(
        &self,
        parent: InodeId,
        name: &str,
        new_parent: InodeId,
        new_name: &str,
    ) -> Result<(), FsError> {
        // Both parents need to be DirtyTree so we can mutate their
        // children maps. They might be the same inode (same-directory
        // rename — the common case for jj-lib's tmpfile→final swap).
        // Materialize both; same-parent is a no-op the second time.
        self.ensure_dirty_tree(parent)?;
        if new_parent != parent {
            self.ensure_dirty_tree(new_parent)?;
        }

        // POSIX rename rules we honour:
        // - Source must exist (ENOENT otherwise).
        // - If destination exists:
        //     * source is dir, dest is dir → dest must be empty.
        //     * source is non-dir, dest is dir → EISDIR.
        //     * source is dir, dest is non-dir → ENOTDIR.
        //     * both non-dir → silently replace (POSIX guarantee).
        // - Source and destination resolving to the same inode is a
        //   no-op (POSIX: "If the old and new arguments resolve to
        //   either the same existing directory entry or different
        //   directory entries for the same existing file, rename()
        //   shall return successfully and perform no other action.").
        let src_inode_id = {
            let parent_inode = self.slab.get(parent).ok_or(FsError::NotFound)?;
            match parent_inode.node {
                NodeRef::DirtyTree { ref children } => children
                    .get(name)
                    .copied()
                    .ok_or(FsError::NotFound)?,
                _ => return Err(FsError::NotADirectory),
            }
        };

        // Look up destination if any.
        let dst_inode_id_opt: Option<InodeId> = {
            let np_inode = self.slab.get(new_parent).ok_or(FsError::NotFound)?;
            match np_inode.node {
                NodeRef::DirtyTree { ref children } => children.get(new_name).copied(),
                _ => return Err(FsError::NotADirectory),
            }
        };

        if let Some(dst_id) = dst_inode_id_opt {
            if dst_id == src_inode_id {
                // Same path resolves to same inode — POSIX no-op.
                return Ok(());
            }
            let src = self.slab.get(src_inode_id).ok_or(FsError::NotFound)?;
            let dst = self.slab.get(dst_id).ok_or(FsError::NotFound)?;
            let src_is_dir =
                matches!(src.node, NodeRef::Tree(_) | NodeRef::DirtyTree { .. });
            let dst_is_dir =
                matches!(dst.node, NodeRef::Tree(_) | NodeRef::DirtyTree { .. });
            match (src_is_dir, dst_is_dir) {
                (true, false) => return Err(FsError::NotADirectory),
                (false, true) => return Err(FsError::NotAFile),
                (true, true) => {
                    // Empty-check the destination directory before
                    // clobbering. Mirrors `remove`'s rmdir-empty rule.
                    let empty = match &dst.node {
                        NodeRef::DirtyTree { children } => children.is_empty(),
                        NodeRef::Tree(id) => self.read_tree(*id)?.entries.is_empty(),
                        _ => true,
                    };
                    if !empty {
                        return Err(FsError::NotEmpty);
                    }
                }
                (false, false) => {
                    // Both files (or symlinks) — POSIX silently replaces.
                }
            }
            // Detach the existing destination. The orphaned inode
            // stays live in the slab; same reasoning as `remove`.
            self.slab.detach_child(new_parent, new_name);
        }

        // Detach source from old parent and attach under new name.
        // detach_child returns the same id we already looked up, so the
        // result is redundant; we still call it to actually unlink.
        self.slab
            .detach_child(parent, name)
            .ok_or(FsError::NotFound)?;
        if !self
            .slab
            .attach_child(new_parent, new_name.to_owned(), src_inode_id)
        {
            // Should not happen: we just materialized new_parent and
            // detached any colliding entry. If it does, our slab
            // invariants are broken.
            return Err(FsError::AlreadyExists);
        }
        Ok(())
    }

    async fn snapshot(&self) -> Result<Id, FsError> {
        self.snapshot_node(self.root())
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
    fn build_synthetic_tree() -> (Arc<Store>, Id) {
        let store = Arc::new(Store::new());
        let hello_id = store.write_file(File {
            content: b"hi\n".to_vec(),
        });
        let tool_id = store.write_file(File {
            content: b"x".to_vec(),
        });
        let link_id = store.write_symlink(Symlink {
            target: "hello.txt".into(),
        });
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
        let bin_id = store.write_tree(bin_tree);
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
        let root_id = store.write_tree(root);
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
        let (store, root) = build_synthetic_tree();
        let fs = YakFs::new(store, root);
        let id = fs.lookup(fs.root(), "hello.txt").await.expect("lookup");
        let attr = fs.getattr(id).await.expect("getattr");
        assert_eq!(attr.kind, FileKind::Regular);
        assert_eq!(attr.size, 3);
        assert!(!attr.executable);
    }

    #[tokio::test]
    async fn lookup_traverses_subdirectory() {
        let (store, root) = build_synthetic_tree();
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
        let (store, root) = build_synthetic_tree();
        let fs = YakFs::new(store, root);
        let a = fs.lookup(fs.root(), "hello.txt").await.unwrap();
        let b = fs.lookup(fs.root(), "hello.txt").await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn read_returns_file_content_with_eof_flag() {
        let (store, root) = build_synthetic_tree();
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
        let (store, root) = build_synthetic_tree();
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
        let (store, root) = build_synthetic_tree();
        let fs = YakFs::new(store, root);
        let id = fs.lookup(fs.root(), "link").await.unwrap();
        assert_eq!(fs.readlink(id).await.unwrap(), "hello.txt");
    }

    #[tokio::test]
    async fn lookup_unknown_name_is_not_found() {
        let (store, root) = build_synthetic_tree();
        let fs = YakFs::new(store, root);
        let err = fs.lookup(fs.root(), "missing").await.unwrap_err();
        assert_eq!(err, FsError::NotFound);
    }

    #[tokio::test]
    async fn read_on_directory_is_not_a_file() {
        let (store, root) = build_synthetic_tree();
        let fs = YakFs::new(store, root);
        let err = fs.read(fs.root(), 0, 16).await.unwrap_err();
        assert_eq!(err, FsError::NotAFile);
    }

    #[tokio::test]
    async fn readdir_on_file_is_not_a_directory() {
        let (store, root) = build_synthetic_tree();
        let fs = YakFs::new(store, root);
        let id = fs.lookup(fs.root(), "hello.txt").await.unwrap();
        let err = fs.readdir(id).await.unwrap_err();
        assert_eq!(err, FsError::NotADirectory);
    }

    #[tokio::test]
    async fn getattr_on_unknown_inode_is_not_found() {
        let (store, root) = build_synthetic_tree();
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
        let a_id = store.write_file(File {
            content: b"a-content".to_vec(),
        });
        let b_id = store.write_file(File {
            content: b"b-content".to_vec(),
        });
        let tree_a = store.write_tree(Tree {
            entries: vec![TreeEntryMapping {
                name: "only-in-a.txt".into(),
                entry: TreeEntry::File {
                    id: a_id,
                    executable: false,
                    copy_id: Vec::new(),
                },
            }],
        });
        let tree_b = store.write_tree(Tree {
            entries: vec![TreeEntryMapping {
                name: "only-in-b.txt".into(),
                entry: TreeEntry::File {
                    id: b_id,
                    executable: false,
                    copy_id: Vec::new(),
                },
            }],
        });

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
        let (store, root) = build_synthetic_tree();
        let fs = YakFs::new(store, root);
        let err = fs.check_out(Id([0xff; 32])).await.unwrap_err();
        assert_eq!(err, FsError::StoreMiss);
        // Original tree is still visible.
        fs.lookup(fs.root(), "hello.txt").await.expect("still A");
    }

    // ----- M6 write-path tests -----------------------------------------

    /// `create_file` + `write` + `read` round-trip on an empty repo.
    /// Snapshot afterward produces a tree that contains the new file at
    /// the right content. The slab's clean-up after snapshot keeps the
    /// inode resolvable via the same id, so post-snapshot reads still
    /// hit the right content.
    #[tokio::test]
    async fn create_write_read_round_trips() {
        let store = Arc::new(Store::new());
        let fs = YakFs::new(store.clone(), store.get_empty_tree_id());
        let (file_ino, attr) = fs
            .create_file(fs.root(), "hello.txt", false)
            .await
            .expect("create_file");
        assert_eq!(attr.kind, FileKind::Regular);
        assert_eq!(attr.size, 0);

        let n = fs.write(file_ino, 0, b"hello world").await.unwrap();
        assert_eq!(n, 11);

        // Read back via the trait.
        let (data, eof) = fs.read(file_ino, 0, 1024).await.unwrap();
        assert_eq!(data, b"hello world");
        assert!(eof);

        // Snapshot persists the dirty buffer; the returned root tree id
        // resolves through the per-mount store.
        let new_root = fs.snapshot().await.expect("snapshot");
        assert_ne!(new_root, store.get_empty_tree_id());
        let tree = store.get_tree(new_root).expect("tree in store");
        let entry = tree
            .entries
            .iter()
            .find(|m| m.name == "hello.txt")
            .expect("hello.txt entry");
        assert!(matches!(entry.entry, TreeEntry::File { .. }));

        // After snapshot the inode survives + still reads correctly
        // (the slab cleaned up dirty content, but the file is now
        // backed by the store).
        let (data, _) = fs.read(file_ino, 0, 1024).await.unwrap();
        assert_eq!(data, b"hello world");
    }

    /// `mkdir` + create-file-inside + snapshot produces a nested tree
    /// matching the standard test_nested_tree_round_trips fixture.
    #[tokio::test]
    async fn mkdir_then_create_inside_round_trips() {
        let store = Arc::new(Store::new());
        let fs = YakFs::new(store.clone(), store.get_empty_tree_id());

        let (dir_ino, _) = fs.mkdir(fs.root(), "dir").await.expect("mkdir");
        let (file_ino, _) = fs
            .create_file(dir_ino, "file", false)
            .await
            .expect("create_file");
        fs.write(file_ino, 0, b"content").await.unwrap();

        let new_root = fs.snapshot().await.expect("snapshot");

        // Walk the tree: root → "dir" → "file".
        let root_tree = store.get_tree(new_root).expect("root in store");
        let dir_entry = root_tree
            .entries
            .iter()
            .find(|m| m.name == "dir")
            .expect("dir entry");
        let dir_id = match &dir_entry.entry {
            TreeEntry::TreeId(id) => *id,
            other => panic!("expected dir to be a Tree, got {other:?}"),
        };
        let dir_tree = store.get_tree(dir_id).expect("dir in store");
        let file_entry = dir_tree
            .entries
            .iter()
            .find(|m| m.name == "file")
            .expect("file entry");
        let file_id = match &file_entry.entry {
            TreeEntry::File { id, .. } => *id,
            other => panic!("expected file to be a File, got {other:?}"),
        };
        let f = store.get_file(file_id).expect("file in store");
        assert_eq!(f.content, b"content");
    }

    /// `symlink` + snapshot produces a SymlinkId entry whose target is
    /// readable through the store.
    #[tokio::test]
    async fn symlink_round_trips() {
        let store = Arc::new(Store::new());
        let fs = YakFs::new(store.clone(), store.get_empty_tree_id());
        let (link_ino, attr) = fs
            .symlink(fs.root(), "link", "target")
            .await
            .expect("symlink");
        assert_eq!(attr.kind, FileKind::Symlink);
        assert_eq!(fs.readlink(link_ino).await.unwrap(), "target");

        let new_root = fs.snapshot().await.expect("snapshot");
        let root_tree = store.get_tree(new_root).expect("root in store");
        let link_entry = root_tree
            .entries
            .iter()
            .find(|m| m.name == "link")
            .expect("link entry");
        let symlink_id = match &link_entry.entry {
            TreeEntry::SymlinkId(id) => *id,
            other => panic!("expected SymlinkId, got {other:?}"),
        };
        let sym = store.get_symlink(symlink_id).expect("symlink in store");
        assert_eq!(sym.target, "target");

        // After snapshot the symlink reads back through the clean path
        // (NodeRef::Symlink → store).
        assert_eq!(fs.readlink(link_ino).await.unwrap(), "target");
    }

    /// Duplicate `create_file` on the same name returns AlreadyExists.
    #[tokio::test]
    async fn create_collision_is_already_exists() {
        let store = Arc::new(Store::new());
        let fs = YakFs::new(store.clone(), store.get_empty_tree_id());
        fs.create_file(fs.root(), "a", false).await.unwrap();
        let err = fs
            .create_file(fs.root(), "a", false)
            .await
            .expect_err("duplicate must fail");
        assert_eq!(err, FsError::AlreadyExists);
    }

    /// `setattr(size=0)` truncates a file; the content shrinks and reads
    /// past the new EOF return empty.
    #[tokio::test]
    async fn setattr_truncates_file() {
        let store = Arc::new(Store::new());
        let fs = YakFs::new(store.clone(), store.get_empty_tree_id());
        let (ino, _) = fs.create_file(fs.root(), "f", false).await.unwrap();
        fs.write(ino, 0, b"hello").await.unwrap();

        let attr = fs
            .setattr(ino, Some(0), None)
            .await
            .expect("setattr truncate");
        assert_eq!(attr.size, 0);

        let (data, eof) = fs.read(ino, 0, 1024).await.unwrap();
        assert!(data.is_empty());
        assert!(eof);
    }

    /// `setattr(executable=true)` flips the exec bit without disturbing
    /// content, and the new bit survives a snapshot round-trip.
    #[tokio::test]
    async fn setattr_chmod_preserves_content() {
        let store = Arc::new(Store::new());
        let fs = YakFs::new(store.clone(), store.get_empty_tree_id());
        let (ino, _) = fs.create_file(fs.root(), "f", false).await.unwrap();
        fs.write(ino, 0, b"hello").await.unwrap();

        let attr = fs.setattr(ino, None, Some(true)).await.expect("chmod");
        assert!(attr.executable);

        let new_root = fs.snapshot().await.expect("snapshot");
        let tree = store.get_tree(new_root).unwrap();
        let entry = tree.entries.iter().find(|m| m.name == "f").unwrap();
        assert!(matches!(
            entry.entry,
            TreeEntry::File { executable: true, .. }
        ));
    }

    /// `remove` of a file detaches it; subsequent lookup is NotFound.
    /// The detached inode is gone from the parent, but its id stays
    /// monotonic (non-reused).
    #[tokio::test]
    async fn remove_file_then_lookup_is_not_found() {
        let store = Arc::new(Store::new());
        let fs = YakFs::new(store.clone(), store.get_empty_tree_id());
        fs.create_file(fs.root(), "f", false).await.unwrap();
        fs.remove(fs.root(), "f").await.expect("remove");
        let err = fs.lookup(fs.root(), "f").await.unwrap_err();
        assert_eq!(err, FsError::NotFound);
    }

    /// `remove` of a non-empty directory returns NotEmpty. The directory
    /// stays attached after the failure.
    #[tokio::test]
    async fn remove_non_empty_directory_is_not_empty() {
        let store = Arc::new(Store::new());
        let fs = YakFs::new(store.clone(), store.get_empty_tree_id());
        let (dir, _) = fs.mkdir(fs.root(), "dir").await.unwrap();
        fs.create_file(dir, "f", false).await.unwrap();

        let err = fs.remove(fs.root(), "dir").await.unwrap_err();
        assert_eq!(err, FsError::NotEmpty);
        // Still resolvable.
        fs.lookup(fs.root(), "dir").await.expect("dir survives");
    }

    /// `snapshot` of an unmodified tree is a no-op: returns the existing
    /// root tree id, doesn't write anything new to the store.
    #[tokio::test]
    async fn snapshot_clean_returns_existing_root() {
        let (store, root) = build_synthetic_tree();
        let fs = YakFs::new(store.clone(), root);
        let id = fs.snapshot().await.expect("snapshot");
        assert_eq!(id, root);
    }

    /// Two distinct dirty edits that produce structurally identical
    /// trees yield identical snapshot ids — the BTreeMap-based name
    /// ordering keeps content hashing stable.
    #[tokio::test]
    async fn snapshot_is_deterministic_under_insertion_order() {
        let store_a = Arc::new(Store::new());
        let fs_a = YakFs::new(store_a.clone(), store_a.get_empty_tree_id());
        fs_a.create_file(fs_a.root(), "a", false).await.unwrap();
        fs_a.create_file(fs_a.root(), "b", false).await.unwrap();
        let id_a = fs_a.snapshot().await.unwrap();

        let store_b = Arc::new(Store::new());
        let fs_b = YakFs::new(store_b.clone(), store_b.get_empty_tree_id());
        // Inserted in the opposite order — final tree should hash the same.
        fs_b.create_file(fs_b.root(), "b", false).await.unwrap();
        fs_b.create_file(fs_b.root(), "a", false).await.unwrap();
        let id_b = fs_b.snapshot().await.unwrap();

        assert_eq!(id_a, id_b);
    }

    /// After snapshot, the slab's dirty entries become clean, but the
    /// kernel-visible inode ids are preserved. Ensures the kernel
    /// doesn't ESTALE on cached handles after the daemon-side snapshot.
    #[tokio::test]
    async fn snapshot_preserves_inode_ids() {
        let store = Arc::new(Store::new());
        let fs = YakFs::new(store.clone(), store.get_empty_tree_id());
        let (ino, _) = fs.create_file(fs.root(), "f", false).await.unwrap();
        fs.write(ino, 0, b"x").await.unwrap();
        fs.snapshot().await.unwrap();
        // Same inode id still resolves and reads.
        let (data, _) = fs.read(ino, 0, 1024).await.unwrap();
        assert_eq!(data, b"x");
    }
}
