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
//! ## Clean vs. dirty (M6)
//!
//! [`NodeRef`] grew three "dirty" variants (`DirtyTree`, `DirtyFile`,
//! `DirtySymlink`) at M6 to support the VFS write path. The split mirrors
//! jj's content-addressed model: clean nodes point at an `Id` in the
//! per-mount [`GitContentStore`](crate::git_store::GitContentStore) and
//! are immutable; dirty nodes hold the in-memory representation that VFS
//! writes mutate. A dirty node is "promoted" back to a clean reference on
//! [`crate::vfs::JjKikiFs::snapshot`], which writes the in-memory blob
//! into the store and updates the slab to point at the resulting id.
//!
//! Promotion in the other direction — clean → dirty — happens lazily on
//! the first write touching a path. `materialize_dir_for_mutation` and
//! `materialize_file_for_mutation` on [`InodeSlab`] do that work; the
//! `KikiFs` impl in `kiki_fs.rs` orchestrates which to call when.
//!
//! Concurrency: a single mutex guards both maps. The slab is on the hot
//! path for `lookup`/`getattr`, but contention is bounded by the number
//! of in-flight kernel calls (typically tiny on a localhost mount), and
//! the alternative — split locks across `inodes` and `by_parent` — opens
//! a window where one is updated before the other.

use std::collections::{BTreeMap, HashMap};

use parking_lot::Mutex;

use crate::ty::Id;

/// Inode id type. `u64` so it widens cleanly to both `nfsserve::nfs::fileid3`
/// and `fuse3::Inode`.
pub type InodeId = u64;

/// Inode id of the root directory. Both NFSv3 and FUSE treat 0 as
/// reserved and conventionally use 1 for the root.
pub const ROOT_INODE: InodeId = 1;

/// What an inode points at.
///
/// "Clean" variants (`Tree`, `File`, `Symlink`) carry a content-addressed
/// [`Id`] resolvable through the per-mount `Store`; they are immutable
/// references. "Dirty" variants hold the in-memory representation that
/// the VFS write path mutates between snapshots.
///
/// `DirtyTree`'s `children` is a `BTreeMap` rather than a `HashMap` so
/// snapshot iteration order is deterministic — `Tree::get_hash` hashes
/// entries in declaration order, and we want two distinct daemon runs
/// that ended up with the same logical contents to produce identical
/// tree ids. (Even if jj-lib doesn't strictly require it today, "two
/// equivalent trees produce equal hashes" is a property worth keeping.)
#[derive(Clone, Debug)]
pub enum NodeRef {
    /// Clean tree (directory). The `Id` resolves via `Store::get_tree`.
    Tree(Id),
    /// Modified directory. `children` is the live name → child-inode map;
    /// kept as `BTreeMap` so iteration order matches name order, which is
    /// what we hash trees in at snapshot time.
    DirtyTree { children: BTreeMap<String, InodeId> },
    /// Clean regular file. `executable` mirrors the bit jj's
    /// `TreeEntry::File` carries on disk; surfaced as the unix exec bit on
    /// getattr.
    File { id: Id, executable: bool },
    /// Modified file. `content` is the live byte buffer the VFS reads and
    /// writes; `executable` carries forward from the clean state (or
    /// defaults to `false` for newly-created files) until `setattr` flips
    /// it.
    DirtyFile { content: Vec<u8>, executable: bool },
    /// Clean symlink. The target is in the daemon's symlink store.
    Symlink(Id),
    /// Modified symlink. Symlink targets are short — no need for `Arc` to
    /// avoid the clone on snapshot.
    DirtySymlink { target: String },
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
    ///
    /// Mostly redundant with `DirtyTree.children` once a directory has
    /// been materialized for mutation, but cheap to keep in sync and
    /// covers the still-clean-`Tree` case where the parent's children
    /// haven't been promoted into the slab yet.
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

    /// Update the `NodeRef` of an existing inode in place. Used by the
    /// VFS write path: file writes update `DirtyFile.content`, snapshot
    /// promotes dirty back to clean, etc. Returns `false` if `id` isn't
    /// in the slab — the caller should then surface `FsError::NotFound`.
    pub fn replace_node(&self, id: InodeId, node: NodeRef) -> bool {
        let mut inner = self.inner.lock();
        if let Some(inode) = inner.inodes.get_mut(&id) {
            inode.node = node;
            true
        } else {
            false
        }
    }

    /// Re-root the slab at `new_root_tree`.
    ///
    /// Updates the root inode's `NodeRef::Tree` and clears only the
    /// `(ROOT_INODE, name)` entries from the reverse cache so subsequent
    /// `lookup` calls under the root see the new tree's children. Sub-tree
    /// entries `(non-root-inode, name)` survive: they're either reachable
    /// through the pinned `.jj/` subtree (M7) — in which case we *want*
    /// stable inode ids so daemon-managed state survives `check_out` —
    /// or they're orphaned, which is harmless because nothing reaches
    /// them from the new root.
    ///
    /// Existing non-root inode entries in `inodes` are retained so the
    /// kernel doesn't immediately ESTALE on cached ids; they're orphaned
    /// but `next_id` keeps moving forward so nothing reuses their numbers.
    ///
    /// This is the M5 contract refined by M7. Pushing kernel-side
    /// invalidation (so the kernel re-`lookup`s rather than reusing its
    /// own cached entries) needs fuse3 to expose `Session::get_notify`
    /// publicly — tracked in `docs/PLAN.md` §7 #9.
    pub fn swap_root(&self, new_root_tree: Id) {
        let mut inner = self.inner.lock();
        // Replace the root inode's NodeRef in place; keep parent/name as the
        // root's self-reference + empty name.
        inner.inodes.insert(
            ROOT_INODE,
            Inode {
                parent: ROOT_INODE,
                name: String::new(),
                node: NodeRef::Tree(new_root_tree),
            },
        );
        // Drop only (ROOT_INODE, *) mappings. Pre-M7 behaviour was to
        // clear the whole cache, which inadvertently severed the chain
        // through `.jj/`'s pinned subtree: a subsequent lookup of
        // `.jj/repo` would re-walk the (now-stale) snapshotted user tree
        // and miss writes that happened between the last snapshot and
        // this swap. Surgical clearing keeps the user-tree story right
        // (root re-resolves freshly) while leaving daemon-managed state
        // reachable.
        inner.by_parent.retain(|(parent, _), _| *parent != ROOT_INODE);
    }

    /// Resolve a child by name under `parent`, allocating an inode id if
    /// this `(parent, name)` pair hasn't been seen before. `make_node` is
    /// invoked only on cache miss so callers don't pay store lookups
    /// they don't need.
    ///
    /// On a hit we return the existing id without invoking `make_node`,
    /// even if the underlying tree entry has changed since interning. The
    /// content-addressed tree store means the data the inode points at
    /// is immutable for as long as the tree id is unchanged; the slab is
    /// invalidated on `check_out` (which clears the cache) and on
    /// `materialize_dir_for_mutation` (which authoritatively re-populates
    /// from the tree).
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

    /// Allocate a new inode under `parent` with the given `name` and
    /// initial `node`. The new inode is *not* attached to the parent's
    /// `(parent, name)` reverse map — that's `attach_child`'s job.
    /// Splitting alloc from attach lets the caller back out cleanly if
    /// the attach is going to fail (e.g. the parent isn't a `DirtyTree`)
    /// without leaving a stale `by_parent` entry pointing at a
    /// not-yet-attached child.
    pub fn alloc(&self, parent: InodeId, name: String, node: NodeRef) -> InodeId {
        let mut inner = self.inner.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        inner
            .inodes
            .insert(id, Inode { parent, name, node });
        id
    }

    /// Promote a `Tree(Id)` inode into `DirtyTree`, allocating child
    /// inodes for every entry in the tree.
    ///
    /// Returns the new child map (a clone of what was just stored on the
    /// inode), so the caller can mutate it without re-locking. Callers
    /// must persist their mutations back via `replace_node` /
    /// `attach_child` / `detach_child` before reads — the slab keeps the
    /// authoritative copy on the inode.
    ///
    /// On a parent that's already `DirtyTree`, returns the existing
    /// children unchanged. On any other variant, returns `None` so the
    /// caller can map to `FsError::NotADirectory`.
    ///
    /// `child_for_entry` builds the `NodeRef` for each tree entry. The
    /// closure form keeps the [`crate::ty::TreeEntry`] → [`NodeRef`]
    /// mapping in `kiki_fs.rs` (where it belongs) rather than dragging
    /// every `TreeEntry` variant into this module.
    pub fn materialize_dir_for_mutation<F, I>(
        &self,
        dir: InodeId,
        load_entries: F,
    ) -> Option<BTreeMap<String, InodeId>>
    where
        F: FnOnce() -> I,
        I: IntoIterator<Item = (String, NodeRef)>,
    {
        let mut inner = self.inner.lock();
        let inode = inner.inodes.get(&dir)?.clone();
        match inode.node {
            NodeRef::DirtyTree { children } => Some(children),
            NodeRef::Tree(_) => {
                // Materialize: allocate or reuse a child inode for each
                // tree entry, populate `by_parent`, then store the
                // resulting `DirtyTree` on `dir`. Reusing existing ids
                // (via `by_parent`) keeps already-cached kernel handles
                // valid; only newly-seen names allocate fresh ids.
                let mut children: BTreeMap<String, InodeId> = BTreeMap::new();
                for (name, child_node) in load_entries() {
                    let key = (dir, name.clone());
                    let child_id = if let Some(&existing) = inner.by_parent.get(&key) {
                        // Refresh the child's NodeRef from the tree —
                        // unless the child has already been promoted to a
                        // dirty variant (DirtyFile, DirtyTree,
                        // DirtySymlink). Overwriting a dirty node with
                        // the clean version from the stored tree would
                        // silently discard in-flight writes.
                        if let Some(c) = inner.inodes.get_mut(&existing) {
                            let already_dirty = matches!(
                                c.node,
                                NodeRef::DirtyFile { .. }
                                    | NodeRef::DirtyTree { .. }
                                    | NodeRef::DirtySymlink { .. }
                            );
                            if !already_dirty {
                                c.node = child_node;
                            }
                        }
                        existing
                    } else {
                        let id = inner.next_id;
                        inner.next_id += 1;
                        inner.inodes.insert(
                            id,
                            Inode {
                                parent: dir,
                                name: name.clone(),
                                node: child_node,
                            },
                        );
                        inner.by_parent.insert(key, id);
                        id
                    };
                    children.insert(name, child_id);
                }
                inner.inodes.insert(
                    dir,
                    Inode {
                        parent: inode.parent,
                        name: inode.name,
                        node: NodeRef::DirtyTree {
                            children: children.clone(),
                        },
                    },
                );
                Some(children)
            }
            _ => None,
        }
    }

    /// Add a child to an already-`DirtyTree` parent. Returns `false` if
    /// `parent` isn't a `DirtyTree` (caller forgot to materialize) or if
    /// `name` already exists. Caller is responsible for first allocating
    /// the child inode (via `alloc`) and not calling this for an existing
    /// name (the FS write ops check for `EEXIST` before this point).
    pub fn attach_child(&self, parent: InodeId, name: String, child: InodeId) -> bool {
        let mut inner = self.inner.lock();
        let Some(parent_inode) = inner.inodes.get_mut(&parent) else {
            return false;
        };
        let NodeRef::DirtyTree { children } = &mut parent_inode.node else {
            return false;
        };
        if children.contains_key(&name) {
            return false;
        }
        children.insert(name.clone(), child);
        inner.by_parent.insert((parent, name), child);
        true
    }

    /// Update the `parent` and `name` fields of an existing inode.
    /// Used by `rename` to keep the ancestor-walk in
    /// `ensure_dirty_tree` correct after a cross-directory move.
    /// Returns `false` if `child` isn't in the slab.
    pub fn reparent(&self, child: InodeId, new_parent: InodeId, new_name: String) -> bool {
        let mut inner = self.inner.lock();
        if let Some(inode) = inner.inodes.get_mut(&child) {
            inode.parent = new_parent;
            inode.name = new_name;
            true
        } else {
            false
        }
    }

    /// Remove a child from a `DirtyTree` parent. Returns the detached
    /// child id on success; `None` if the parent isn't a `DirtyTree` or
    /// the name doesn't exist.
    ///
    /// We don't delete the child inode itself — non-root inodes stay live
    /// in `inodes` for the same reason as `swap_root` (kernel may still
    /// have stale handles; orphaning is safe because `next_id` is
    /// monotonic).
    pub fn detach_child(&self, parent: InodeId, name: &str) -> Option<InodeId> {
        let mut inner = self.inner.lock();
        let parent_inode = inner.inodes.get_mut(&parent)?;
        let NodeRef::DirtyTree { children } = &mut parent_inode.node else {
            return None;
        };
        let child = children.remove(name)?;
        inner.by_parent.remove(&(parent, name.to_owned()));
        Some(child)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> Id {
        Id([byte; 20])
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

    /// `swap_root` rewrites the root inode and clears `(ROOT, *)`
    /// reverse-cache entries so subsequent `intern_child` calls under
    /// the root allocate fresh ids. Sub-tree entries are retained.
    /// Older non-root ids stay live in `inodes` (orphaned but safe).
    #[test]
    fn swap_root_updates_root_and_clears_root_level_cache() {
        let slab = InodeSlab::new(id(1));
        let old_a = slab.intern_child(ROOT_INODE, "a", || NodeRef::File {
            id: id(2),
            executable: false,
        });

        slab.swap_root(id(99));

        // Root now points at the new tree id.
        let root = slab.get(ROOT_INODE).expect("root present");
        assert!(matches!(root.node, NodeRef::Tree(t) if t == id(99)));

        // Re-interning "a" allocates a fresh id (the (ROOT, "a") entry
        // was cleared); monotonic ordering still holds.
        let new_a = slab.intern_child(ROOT_INODE, "a", || NodeRef::File {
            id: id(3),
            executable: false,
        });
        assert!(new_a > old_a, "expected monotonic id");
    }

    /// `swap_root` preserves sub-tree `(non-root, name)` cache entries.
    /// The pinned `.jj/` subtree (M7) relies on this to keep its
    /// descendants reachable across `check_out`: the kernel-cached
    /// children of any directory inside `.jj/` must continue to resolve
    /// to the same inode ids after the user tree changes.
    #[test]
    fn swap_root_preserves_subtree_cache() {
        let slab = InodeSlab::new(id(1));
        // Set up: root → "dir" → "file". The "dir" inode lives outside
        // the cleared cache.
        let dir = slab.intern_child(ROOT_INODE, "dir", || NodeRef::Tree(id(2)));
        let file_under_dir = slab.intern_child(dir, "file", || NodeRef::File {
            id: id(3),
            executable: false,
        });

        slab.swap_root(id(99));

        // (ROOT, "dir") is gone — re-allocates.
        let new_dir = slab.intern_child(ROOT_INODE, "dir", || NodeRef::Tree(id(4)));
        assert!(new_dir > dir, "(ROOT, dir) entry was dropped");

        // (dir, "file") survived — re-interning returns the original id
        // and the loader closure must NOT be observed.
        let same_file = slab.intern_child(dir, "file", || {
            panic!("subtree cache should have hit, not allocated")
        });
        assert_eq!(same_file, file_under_dir);
    }

    /// Materializing a clean `Tree` inode swaps it to `DirtyTree` and
    /// allocates child inodes for each entry. The child ids must be
    /// reused for already-cached `(parent, name)` pairs so the kernel
    /// doesn't see them change mid-flight.
    #[test]
    fn materialize_dir_promotes_tree_and_reuses_existing_children() {
        let slab = InodeSlab::new(id(1));
        // Pretend the kernel did a `lookup("a")` before the parent was
        // materialized — the child gets an inode via `intern_child`.
        let cached_a = slab.intern_child(ROOT_INODE, "a", || NodeRef::File {
            id: id(2),
            executable: false,
        });

        let children = slab
            .materialize_dir_for_mutation(ROOT_INODE, || {
                vec![
                    (
                        "a".to_owned(),
                        NodeRef::File {
                            id: id(2),
                            executable: false,
                        },
                    ),
                    (
                        "b".to_owned(),
                        NodeRef::File {
                            id: id(3),
                            executable: false,
                        },
                    ),
                ]
            })
            .expect("materialize");
        assert_eq!(children["a"], cached_a, "must reuse the kernel-known id");
        assert!(children["b"] > cached_a, "new entries get fresh ids");
        // Root is now DirtyTree.
        let root = slab.get(ROOT_INODE).expect("root present");
        assert!(matches!(root.node, NodeRef::DirtyTree { .. }));
    }

    /// Materializing a directory that's already dirty is a no-op:
    /// returns the existing children, doesn't allocate.
    #[test]
    fn materialize_dir_is_idempotent() {
        let slab = InodeSlab::new(id(1));
        slab.replace_node(
            ROOT_INODE,
            NodeRef::DirtyTree {
                children: BTreeMap::from([("x".to_owned(), 42)]),
            },
        );
        let children = slab
            .materialize_dir_for_mutation(ROOT_INODE, || -> Vec<(String, NodeRef)> {
                panic!("loader must not run for an already-dirty tree");
            })
            .expect("materialize");
        assert_eq!(children["x"], 42);
    }

    /// `attach_child` plus `detach_child` round-trip: the child is
    /// addressable by `(parent, name)` after attach, gone after detach.
    /// The child *inode* survives detach (orphaned, see module docs).
    #[test]
    fn attach_then_detach_round_trips() {
        let slab = InodeSlab::new(id(1));
        slab.replace_node(
            ROOT_INODE,
            NodeRef::DirtyTree {
                children: BTreeMap::new(),
            },
        );
        let child = slab.alloc(
            ROOT_INODE,
            "new.txt".to_owned(),
            NodeRef::DirtyFile {
                content: b"hi".to_vec(),
                executable: false,
            },
        );
        assert!(
            slab.attach_child(ROOT_INODE, "new.txt".to_owned(), child),
            "attach must succeed under an empty DirtyTree"
        );
        // Now `lookup`-equivalents resolve via `by_parent`.
        assert_eq!(
            slab.intern_child(ROOT_INODE, "new.txt", || panic!("must hit cache")),
            child,
        );
        let detached = slab.detach_child(ROOT_INODE, "new.txt").expect("detach");
        assert_eq!(detached, child);
        assert!(
            slab.detach_child(ROOT_INODE, "new.txt").is_none(),
            "second detach is None"
        );
        // Child inode still resolvable directly (orphaned but safe).
        assert!(slab.get(child).is_some());

        // A duplicate attach is rejected without corrupting state.
        let other = slab.alloc(
            ROOT_INODE,
            "x.txt".to_owned(),
            NodeRef::DirtyFile {
                content: Vec::new(),
                executable: false,
            },
        );
        assert!(slab.attach_child(ROOT_INODE, "x.txt".to_owned(), other));
        let dup = slab.alloc(
            ROOT_INODE,
            "x.txt".to_owned(),
            NodeRef::DirtyFile {
                content: Vec::new(),
                executable: false,
            },
        );
        assert!(
            !slab.attach_child(ROOT_INODE, "x.txt".to_owned(), dup),
            "duplicate attach must fail"
        );
    }

    /// Regression test for Bug 3 (dirty nodes clobbered by materialization).
    ///
    /// If a child has already been promoted to a dirty variant
    /// (e.g. `DirtyFile` via a prior `write`), re-materializing the
    /// parent must NOT overwrite that child's `NodeRef` with the clean
    /// version from the stored tree. Before the fix, the second
    /// `materialize_dir_for_mutation` would unconditionally refresh
    /// every existing child's `NodeRef`, silently discarding writes.
    #[test]
    fn materialize_preserves_dirty_children() {
        let slab = InodeSlab::new(id(1));

        // Step 1: intern "a" as a clean File — simulates lookup of an
        // existing tree entry that the kernel has cached.
        let child_a = slab.intern_child(ROOT_INODE, "a", || NodeRef::File {
            id: id(0x10),
            executable: false,
        });

        // Step 2: promote "a" to DirtyFile — simulates `ensure_dirty_file`
        // after a `write()` call.
        slab.replace_node(
            child_a,
            NodeRef::DirtyFile {
                content: b"modified".to_vec(),
                executable: false,
            },
        );

        // Step 3: materialize the parent. The loader provides the
        // original clean entry for "a" (as the store would). The fix
        // skips the refresh for "a" because it's already dirty.
        let children = slab
            .materialize_dir_for_mutation(ROOT_INODE, || {
                vec![
                    (
                        "a".to_owned(),
                        NodeRef::File {
                            id: id(0x10),
                            executable: false,
                        },
                    ),
                    (
                        "b".to_owned(),
                        NodeRef::File {
                            id: id(0x20),
                            executable: false,
                        },
                    ),
                ]
            })
            .expect("materialize");

        // "a" should still use the same inode id.
        assert_eq!(children["a"], child_a, "must reuse the kernel-known id");

        // Crucially: "a" must still be DirtyFile with the modified content,
        // not reverted to the clean File{id: 0x10}.
        let inode_a = slab.get(child_a).expect("child a present");
        assert!(
            matches!(inode_a.node, NodeRef::DirtyFile { ref content, .. } if content == b"modified"),
            "dirty child must survive materialization, got {:?}",
            inode_a.node,
        );

        // "b" should be the clean version from the loader.
        let child_b = children["b"];
        let inode_b = slab.get(child_b).expect("child b present");
        let expected_b = id(0x20);
        assert!(
            matches!(inode_b.node, NodeRef::File { id: file_id, .. } if file_id == expected_b),
            "clean child should be refreshed normally",
        );
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn id(byte: u8) -> Id {
        Id([byte; 20])
    }

    fn arb_id() -> impl Strategy<Value = Id> {
        any::<[u8; 20]>().prop_map(Id)
    }

    /// Strategy for valid child names (non-empty, no '/' or NUL).
    fn arb_child_name() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9_.]{0,7}"
    }

    // ---- intern_child idempotency ----

    proptest! {
        /// Interning the same (parent, name) pair N times always returns
        /// the same id, and the original make_node result is the one stored.
        #[test]
        fn intern_child_idempotent(
            root_id in arb_id(),
            name in arb_child_name(),
            child_id in arb_id(),
            n in 2..8usize,
        ) {
            let slab = InodeSlab::new(root_id);
            let first = slab.intern_child(ROOT_INODE, &name, || NodeRef::File {
                id: child_id,
                executable: false,
            });
            for _ in 1..n {
                let again = slab.intern_child(ROOT_INODE, &name, || NodeRef::File {
                    id: Id([0xff; 20]), // different — must not be observed
                    executable: true,
                });
                prop_assert_eq!(first, again);
            }
            // Verify the stored node is the first one, not any later closure.
            let inode = slab.get(first).unwrap();
            match inode.node {
                NodeRef::File { id: stored_id, executable } => {
                    prop_assert_eq!(stored_id, child_id);
                    prop_assert!(!executable);
                }
                _ => prop_assert!(false, "expected File, got {:?}", inode.node),
            }
        }
    }

    // ---- alloc monotonicity ----

    proptest! {
        /// Allocating N children produces strictly increasing inode ids.
        #[test]
        fn alloc_ids_are_monotonic(n in 2..32usize) {
            let slab = InodeSlab::new(id(0));
            let mut prev = 0u64;
            for i in 0..n {
                let child = slab.alloc(
                    ROOT_INODE,
                    format!("child_{i}"),
                    NodeRef::File {
                        id: id(i as u8),
                        executable: false,
                    },
                );
                prop_assert!(child > prev, "id {child} must be > {prev}");
                prev = child;
            }
        }
    }

    // ---- attach / detach round-trip ----

    proptest! {
        /// After attach, the child is reachable via intern_child.
        /// After detach, the child inode persists but name lookup fails.
        #[test]
        fn attach_detach_roundtrip(name in arb_child_name(), content in prop::collection::vec(any::<u8>(), 0..64)) {
            let slab = InodeSlab::new(id(1));
            slab.replace_node(ROOT_INODE, NodeRef::DirtyTree { children: BTreeMap::new() });

            let child = slab.alloc(
                ROOT_INODE,
                name.clone(),
                NodeRef::DirtyFile { content: content.clone(), executable: false },
            );
            prop_assert!(slab.attach_child(ROOT_INODE, name.clone(), child));

            // Reachable via cache.
            let looked_up = slab.intern_child(ROOT_INODE, &name, || panic!("should hit cache"));
            prop_assert_eq!(looked_up, child);

            // Detach.
            let detached = slab.detach_child(ROOT_INODE, &name);
            prop_assert_eq!(detached, Some(child));

            // Name lookup now misses (allocates fresh).
            let fresh = slab.intern_child(ROOT_INODE, &name, || NodeRef::File {
                id: id(0xff),
                executable: false,
            });
            prop_assert!(fresh > child, "fresh id after detach must be > old");

            // But the orphaned child inode is still in the slab.
            prop_assert!(slab.get(child).is_some(), "orphaned child must survive detach");
        }
    }

    // ---- duplicate attach is rejected ----

    proptest! {
        #[test]
        fn duplicate_attach_rejected(name in arb_child_name()) {
            let slab = InodeSlab::new(id(1));
            slab.replace_node(ROOT_INODE, NodeRef::DirtyTree { children: BTreeMap::new() });

            let c1 = slab.alloc(ROOT_INODE, name.clone(), NodeRef::DirtyFile {
                content: Vec::new(),
                executable: false,
            });
            let c2 = slab.alloc(ROOT_INODE, name.clone(), NodeRef::DirtyFile {
                content: Vec::new(),
                executable: false,
            });
            prop_assert!(slab.attach_child(ROOT_INODE, name.clone(), c1));
            prop_assert!(!slab.attach_child(ROOT_INODE, name.clone(), c2),
                "duplicate attach must fail");
        }
    }

    // ---- swap_root clears only root-level cache ----

    proptest! {
        /// After swap_root, (ROOT, *) entries are gone but (sub, *) survive.
        #[test]
        fn swap_root_selective_clear(
            n_root_children in 1..6usize,
            n_sub_children in 1..6usize,
        ) {
            let slab = InodeSlab::new(id(1));
            let mut root_children = Vec::new();
            for i in 0..n_root_children {
                let name = format!("r{i}");
                let child = slab.intern_child(ROOT_INODE, &name, || NodeRef::File {
                    id: id(i as u8 + 10),
                    executable: false,
                });
                root_children.push((name, child));
            }

            // Create a subtree and intern children under it.
            let sub = slab.intern_child(ROOT_INODE, "sub", || NodeRef::Tree(id(0x50)));
            let mut sub_children = Vec::new();
            for i in 0..n_sub_children {
                let name = format!("s{i}");
                let child = slab.intern_child(sub, &name, || NodeRef::File {
                    id: id(i as u8 + 20),
                    executable: false,
                });
                sub_children.push((name, child));
            }

            slab.swap_root(id(0x99));

            // Root-level re-intern should allocate fresh ids.
            for (name, old_id) in &root_children {
                let new_id = slab.intern_child(ROOT_INODE, name, || NodeRef::File {
                    id: id(0xff),
                    executable: false,
                });
                prop_assert!(new_id > *old_id, "root child {name} must get fresh id");
            }
            // "sub" entry under root is also gone.
            let new_sub = slab.intern_child(ROOT_INODE, "sub", || NodeRef::Tree(id(0x51)));
            prop_assert!(new_sub > sub, "sub entry under root must get fresh id");

            // Sub-tree children must survive (cache hit, no allocation).
            for (name, old_id) in &sub_children {
                let same = slab.intern_child(sub, name, || {
                    panic!("sub-tree cache for {name} should have survived swap_root")
                });
                prop_assert_eq!(same, *old_id);
            }
        }
    }

    // ---- materialize preserves dirty children (generalized Bug 3 regression) ----

    proptest! {
        /// Create N children, mark a random subset as dirty, then
        /// materialize the parent. Dirty children must keep their
        /// in-memory content; clean children get the loader's NodeRef.
        #[test]
        fn materialize_preserves_arbitrary_dirty_subset(
            n in 1..8usize,
            dirty_mask in any::<u8>(),
        ) {
            let slab = InodeSlab::new(id(1));
            let mut names_and_ids = Vec::new();
            let mut dirty_set = std::collections::HashSet::new();

            for i in 0..n {
                let name = format!("f{i}");
                let child = slab.intern_child(ROOT_INODE, &name, || NodeRef::File {
                    id: id(i as u8 + 10),
                    executable: false,
                });
                // Use one bit of dirty_mask per child to decide.
                if (dirty_mask >> (i % 8)) & 1 == 1 {
                    slab.replace_node(child, NodeRef::DirtyFile {
                        content: format!("dirty_{i}").into_bytes(),
                        executable: false,
                    });
                    dirty_set.insert(i);
                }
                names_and_ids.push((name, child, i));
            }

            // Materialize parent — loader provides the original clean refs.
            let children = slab.materialize_dir_for_mutation(ROOT_INODE, || {
                (0..n).map(|i| {
                    (
                        format!("f{i}"),
                        NodeRef::File {
                            id: id(i as u8 + 10),
                            executable: false,
                        },
                    )
                })
            }).expect("materialize");

            for (name, old_id, i) in &names_and_ids {
                let child_id = children[name];
                prop_assert_eq!(child_id, *old_id,
                    "must reuse existing id for {}", name);

                let inode = slab.get(child_id).unwrap();
                if dirty_set.contains(i) {
                    let expected = format!("dirty_{i}").into_bytes();
                    match &inode.node {
                        NodeRef::DirtyFile { content, .. } => {
                            prop_assert_eq!(content, &expected,
                                "dirty child {} content must survive", name);
                        }
                        other => prop_assert!(false,
                            "dirty child {} should be DirtyFile, got {:?}", name, other),
                    }
                } else {
                    match &inode.node {
                        NodeRef::File { id: file_id, .. } => {
                            prop_assert_eq!(*file_id, id(*i as u8 + 10),
                                "clean child {} should have loader's id", name);
                        }
                        other => prop_assert!(false,
                            "clean child {} should be File, got {:?}", name, other),
                    }
                }
            }
        }
    }
}
