//! Inode slab — the canonical state behind both the NFS and FUSE adapters.
//!
//! Both `nfsserve::vfs::NFSFileSystem` and `fuse3::raw::Filesystem` are
//! inode-keyed (`fileid3` / `Inode`, both `u64`) and assume the server can
//! resolve an opaque integer back to a path. The slab is that resolution:
//! every directory entry the kernel touches via `lookup` is interned so a
//! later `getattr`/`read`/`readdir` on the same id resolves to the same
//! tree-store reference.
//!
//! Allocation is monotonic. Reusing inode numbers across the daemon's
//! lifetime would cause kernel-side `ESTALE` (NFS) or stale-handle
//! confusion (FUSE) when a previously-cached id is recycled for a new
//! object. Stable-across-restart is a separate problem owned by Layer B.
//!
//! The slab is currently unbounded — it grows with the breadth of paths
//! the kernel walks. M3 doesn't need eviction; if/when it does, the
//! eviction policy has to coordinate with the FUSE `forget` op.
//!
//! Concurrency: a single mutex guards both maps. The slab is on the hot
//! path for `lookup`/`getattr`, but contention is bounded by the number
//! of in-flight kernel calls (typically tiny on a localhost mount), and
//! the alternative — split locks across `inodes` and `by_parent` — opens
//! a window where one is updated before the other.

use std::collections::HashMap;

use parking_lot::Mutex;

use crate::ty::Id;

/// Inode id type. `u64` so it widens cleanly to both `nfsserve::nfs::fileid3`
/// and `fuse3::Inode`.
pub type InodeId = u64;

/// Inode id of the root directory. Both NFSv3 and FUSE treat 0 as
/// reserved and conventionally use 1 for the root.
pub const ROOT_INODE: InodeId = 1;

/// What an inode points at in the content store.
#[derive(Clone, Copy, Debug)]
pub enum NodeRef {
    /// Tree (directory). The `Id` resolves via `Store::get_tree`.
    Tree(Id),
    /// Regular file. `executable` mirrors the bit jj's `TreeEntry::File`
    /// carries on disk; surfaced as the unix exec bit on getattr.
    File { id: Id, executable: bool },
    /// Symlink. The target is in the daemon's symlink store.
    Symlink(Id),
}

#[derive(Clone, Debug)]
pub struct Inode {
    /// Parent inode. The root inode is its own parent. Currently unused
    /// in the read path but stored so future work (symlink resolution,
    /// `readdirplus` `..` entries, path-tracing in error messages) has
    /// what it needs without re-walking the tree.
    #[allow(dead_code)]
    pub parent: InodeId,
    /// Component name within the parent directory. Empty for the root.
    /// Same justification as `parent`.
    #[allow(dead_code)]
    pub name: String,
    pub node: NodeRef,
}

#[derive(Debug, Default)]
struct SlabInner {
    /// Next id to allocate. Monotonic; never reused.
    next_id: InodeId,
    inodes: HashMap<InodeId, Inode>,
    /// `(parent, name) -> id` reverse map. Populated on every successful
    /// `lookup`, so the kernel sees stable ids across calls.
    by_parent: HashMap<(InodeId, String), InodeId>,
}

#[derive(Debug)]
pub struct InodeSlab {
    inner: Mutex<SlabInner>,
}

impl InodeSlab {
    pub fn new(root_tree: Id) -> Self {
        let mut inodes = HashMap::new();
        inodes.insert(
            ROOT_INODE,
            Inode {
                parent: ROOT_INODE,
                name: String::new(),
                node: NodeRef::Tree(root_tree),
            },
        );
        InodeSlab {
            inner: Mutex::new(SlabInner {
                next_id: ROOT_INODE + 1,
                inodes,
                by_parent: HashMap::new(),
            }),
        }
    }

    pub fn get(&self, id: InodeId) -> Option<Inode> {
        self.inner.lock().inodes.get(&id).cloned()
    }

    /// Resolve a child by name under `parent`, allocating an inode id if
    /// this `(parent, name)` pair hasn't been seen before. `make_node` is
    /// invoked only on cache miss so callers don't pay store lookups
    /// they don't need.
    ///
    /// On a hit we return the existing id without invoking `make_node`,
    /// even if the underlying tree entry has changed since interning. The
    /// content-addressed tree store means the data the inode points at
    /// is immutable for as long as the tree id is unchanged; once tree
    /// rewrites land (M5), we'll have to invalidate the slab on
    /// checkout — tracked separately.
    pub fn intern_child(
        &self,
        parent: InodeId,
        name: &str,
        make_node: impl FnOnce() -> NodeRef,
    ) -> InodeId {
        let mut inner = self.inner.lock();
        let key = (parent, name.to_owned());
        if let Some(&id) = inner.by_parent.get(&key) {
            return id;
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let node = make_node();
        inner.inodes.insert(
            id,
            Inode {
                parent,
                name: name.to_owned(),
                node,
            },
        );
        inner.by_parent.insert(key, id);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> Id {
        Id([byte; 32])
    }

    #[test]
    fn root_is_present_and_is_a_tree() {
        let slab = InodeSlab::new(id(0xaa));
        let root = slab.get(ROOT_INODE).expect("root present");
        assert!(matches!(root.node, NodeRef::Tree(_)));
        assert_eq!(root.parent, ROOT_INODE);
    }

    #[test]
    fn intern_child_is_idempotent() {
        let slab = InodeSlab::new(id(0));
        let a = slab.intern_child(ROOT_INODE, "foo", || NodeRef::File {
            id: id(1),
            executable: false,
        });
        let b = slab.intern_child(ROOT_INODE, "foo", || NodeRef::File {
            id: id(2),
            executable: true,
        });
        assert_eq!(a, b, "second intern should return the same id");
        // The make_node closure on the second call must not be observed —
        // the file id we read back must be the original (0x01), not the
        // one the second closure would have produced (0x02).
        let inode = slab.get(a).expect("interned inode present");
        assert!(matches!(inode.node, NodeRef::File { id: i, .. } if i == id(1)));
    }

    #[test]
    fn ids_are_monotonic_and_unique() {
        let slab = InodeSlab::new(id(0));
        let a = slab.intern_child(ROOT_INODE, "a", || NodeRef::Tree(id(1)));
        let b = slab.intern_child(ROOT_INODE, "b", || NodeRef::Tree(id(2)));
        let c = slab.intern_child(ROOT_INODE, "c", || NodeRef::Tree(id(3)));
        assert_eq!(b, a + 1);
        assert_eq!(c, b + 1);
        assert!(a > ROOT_INODE);
    }
}
