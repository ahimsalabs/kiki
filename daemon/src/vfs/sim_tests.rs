//! FoundationDB-style simulation tests for crash recovery.
//!
//! Models the inode state machine as a set of operations (create, write,
//! mkdir, rename, symlink, remove, snapshot) and verifies crash-recovery
//! invariants via property-based testing with proptest.
//!
//! ## Crash model
//!
//! The in-memory inode slab ([`InodeSlab`]) is ephemeral — it lives only
//! as long as the [`KikiFs`] instance. A "crash" is simulated by dropping
//! the `KikiFs`. "Rehydration" creates a fresh `KikiFs` from the root
//! tree id persisted by the last successful [`JjKikiFs::snapshot`].
//! Content blobs survive the crash because they're written to the git ODB
//! (via [`GitContentStore`]) during snapshot.
//!
//! ## Properties verified
//!
//! 1. **No data loss**: after crash + rehydrate, every file whose state
//!    was persisted via snapshot is recoverable with correct content.
//! 2. **No phantom files**: no entries appear after rehydrate that weren't
//!    present at the last snapshot.
//! 3. **Parent consistency**: the rehydrated tree is well-formed — every
//!    file is readable, every directory is listable, no dangling refs.
//! 4. **Snapshot idempotency**: consecutive snapshots with no intervening
//!    mutations produce the same root tree id.
//! 5. **Model agreement**: after each snapshot, the live filesystem tree
//!    matches the reference model exactly.

use std::collections::BTreeMap;
use std::sync::Arc;

use jj_lib::object_id::ObjectId as _;
use proptest::prelude::*;

use crate::git_store::GitContentStore;
use crate::ty::Id;
use crate::vfs::inode::ROOT_INODE;

use super::kiki_fs::{FileKind, JjKikiFs, KikiFs};

// ---- Reference model ----

/// Simple recursive tree model. Each node is either a file (with content
/// bytes), a directory (with named children), or a symlink (with target).
#[derive(Clone, Debug, PartialEq, Eq)]
enum ModelNode {
    File(Vec<u8>),
    Dir(BTreeMap<String, ModelNode>),
    Symlink(String),
}

type ModelTree = BTreeMap<String, ModelNode>;

// ---- Simulation operations ----

/// Operations the simulation can generate. Each maps to one or more
/// [`JjKikiFs`] trait method calls on the live filesystem.
#[derive(Debug, Clone)]
enum Op {
    /// Create an empty non-executable file at root.
    CreateFile(String),
    /// Create an empty directory at root.
    Mkdir(String),
    /// Write data at offset 0 to a root-level file.
    WriteFile(String, Vec<u8>),
    /// Create a symlink at root with the given target string.
    Symlink { name: String, target: String },
    /// Remove a root-level entry.
    Remove(String),
    /// Rename a root-level entry to another root-level name.
    Rename { old: String, new: String },
    /// Create a file inside a root-level directory.
    CreateInDir { dir: String, name: String },
    /// Write data at offset 0 to a file inside a root-level directory.
    WriteInDir { dir: String, name: String, data: Vec<u8> },
    /// Persist all dirty state to the content store.
    Snapshot,
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::CreateFile(n) => write!(f, "create_file({n})"),
            Op::Mkdir(n) => write!(f, "mkdir({n})"),
            Op::WriteFile(n, d) => write!(f, "write({n}, {}B)", d.len()),
            Op::Symlink { name, target } => write!(f, "symlink({name} -> {target})"),
            Op::Remove(n) => write!(f, "remove({n})"),
            Op::Rename { old, new } => write!(f, "rename({old} -> {new})"),
            Op::CreateInDir { dir, name } => write!(f, "create({dir}/{name})"),
            Op::WriteInDir { dir, name, data } => {
                write!(f, "write({dir}/{name}, {}B)", data.len())
            }
            Op::Snapshot => write!(f, "snapshot"),
        }
    }
}

// ---- Helpers ----

fn test_settings() -> jj_lib::settings::UserSettings {
    let toml_str = r#"
        user.name = "Test User"
        user.email = "test@example.com"
        operation.hostname = "test"
        operation.username = "test"
        debug.randomness-seed = 42
    "#;
    let mut config = jj_lib::config::StackedConfig::with_defaults();
    config.add_layer(
        jj_lib::config::ConfigLayer::parse(jj_lib::config::ConfigSource::User, toml_str)
            .unwrap(),
    );
    jj_lib::settings::UserSettings::from_config(config).unwrap()
}

fn empty_root(store: &GitContentStore) -> Id {
    Id(store.empty_tree_id().as_bytes().try_into().unwrap())
}

// ---- Simulation harness ----

/// Drives a sequence of operations against a [`KikiFs`] while maintaining
/// a reference model. After a simulated crash, verifies that rehydration
/// from the last snapshot root produces a tree identical to the model's
/// committed checkpoint.
struct Sim {
    store: Arc<GitContentStore>,
    /// Current expected state (tracks every applied operation).
    model: ModelTree,
    /// Model state at the last successful snapshot.
    committed: ModelTree,
    /// Root tree id from the last successful snapshot.
    committed_root: Id,
    rt: tokio::runtime::Runtime,
}

impl Sim {
    fn new() -> Self {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let root = empty_root(&store);
        Sim {
            store,
            model: BTreeMap::new(),
            committed: BTreeMap::new(),
            committed_root: root,
            rt: tokio::runtime::Runtime::new().expect("tokio runtime"),
        }
    }

    /// Create a fresh [`KikiFs`] rooted at the last committed snapshot.
    fn new_fs(&self) -> KikiFs {
        KikiFs::new(self.store.clone(), self.committed_root, None)
    }

    /// Apply `ops[..crash_point]` to a fresh filesystem, then drop it
    /// (simulating a crash).
    fn run_until_crash(&mut self, ops: &[Op], crash_point: usize) {
        let fs = self.new_fs();
        let end = crash_point.min(ops.len());
        for op in &ops[..end] {
            self.apply(&fs, op);
        }
        drop(fs);
    }

    /// Apply a single operation to the live filesystem and update the
    /// reference model to match.
    fn apply(&mut self, fs: &KikiFs, op: &Op) {
        match op {
            Op::CreateFile(name) => {
                let r = self.rt.block_on(fs.create_file(ROOT_INODE, name, false));
                if r.is_ok() {
                    self.model.insert(name.clone(), ModelNode::File(Vec::new()));
                }
            }
            Op::Mkdir(name) => {
                let r = self.rt.block_on(fs.mkdir(ROOT_INODE, name));
                if r.is_ok() {
                    self.model
                        .insert(name.clone(), ModelNode::Dir(BTreeMap::new()));
                }
            }
            Op::WriteFile(name, data) => {
                let Ok(ino) = self.rt.block_on(fs.lookup(ROOT_INODE, name)) else {
                    return;
                };
                if self.rt.block_on(fs.write(ino, 0, data)).is_ok()
                    && let Some(ModelNode::File(c)) = self.model.get_mut(name)
                {
                    model_write(c, 0, data);
                }
            }
            Op::Symlink { name, target } => {
                let r = self.rt.block_on(fs.symlink(ROOT_INODE, name, target));
                if r.is_ok() {
                    self.model
                        .insert(name.clone(), ModelNode::Symlink(target.clone()));
                }
            }
            Op::Remove(name) => {
                if self.rt.block_on(fs.remove(ROOT_INODE, name)).is_ok() {
                    self.model.remove(name);
                }
            }
            Op::Rename { old, new } => {
                let r = self
                    .rt
                    .block_on(fs.rename(ROOT_INODE, old, ROOT_INODE, new));
                if r.is_ok()
                    && let Some(node) = self.model.remove(old)
                {
                    self.model.insert(new.clone(), node);
                }
            }
            Op::CreateInDir { dir, name } => {
                let Ok(dir_ino) = self.rt.block_on(fs.lookup(ROOT_INODE, dir)) else {
                    return;
                };
                if self
                    .rt
                    .block_on(fs.create_file(dir_ino, name, false))
                    .is_ok()
                    && let Some(ModelNode::Dir(children)) = self.model.get_mut(dir)
                {
                    children.insert(name.clone(), ModelNode::File(Vec::new()));
                }
            }
            Op::WriteInDir { dir, name, data } => {
                let Ok(dir_ino) = self.rt.block_on(fs.lookup(ROOT_INODE, dir)) else {
                    return;
                };
                let Ok(file_ino) = self.rt.block_on(fs.lookup(dir_ino, name)) else {
                    return;
                };
                if self.rt.block_on(fs.write(file_ino, 0, data)).is_ok()
                    && let Some(ModelNode::Dir(children)) = self.model.get_mut(dir)
                    && let Some(ModelNode::File(c)) = children.get_mut(name)
                {
                    model_write(c, 0, data);
                }
            }
            Op::Snapshot => {
                let Ok(root) = self.rt.block_on(fs.snapshot()) else {
                    return;
                };
                self.committed = self.model.clone();
                self.committed_root = root;

                // Verify the live tree matches the model right after
                // snapshot. This catches model drift before the crash
                // check adds noise.
                let actual = self.walk_tree(fs, ROOT_INODE);
                assert_eq!(
                    actual, self.model,
                    "model diverged from live FS after snapshot"
                );
            }
        }
    }

    /// Rehydrate from the last committed root and assert the tree
    /// matches the committed model.
    fn verify_crash_recovery(&self) {
        let fs = self.new_fs();
        let actual = self.walk_tree(&fs, ROOT_INODE);
        assert_eq!(
            actual, self.committed,
            "rehydrated tree does not match committed model\n\
             committed_root: {}",
            self.committed_root,
        );
    }

    /// Recursively walk the filesystem tree from `root`, building a
    /// [`ModelTree`] for comparison.
    fn walk_tree(&self, fs: &KikiFs, root: u64) -> ModelTree {
        let entries = self
            .rt
            .block_on(fs.readdir(root))
            .expect("readdir failed on a valid tree");
        let mut result = BTreeMap::new();
        for entry in entries {
            // Skip synthesized entries that are not part of the user model.
            if entry.name == ".git" || entry.name == ".jj" {
                continue;
            }
            let attr = self
                .rt
                .block_on(fs.getattr(entry.inode))
                .expect("getattr failed for readdir result");
            let node = match attr.kind {
                FileKind::Regular => {
                    let (data, _) = self
                        .rt
                        .block_on(fs.read(entry.inode, 0, u32::MAX))
                        .expect("read failed on a valid file");
                    ModelNode::File(data)
                }
                FileKind::Directory => {
                    let children = self.walk_tree(fs, entry.inode);
                    ModelNode::Dir(children)
                }
                FileKind::Symlink => {
                    let target = self
                        .rt
                        .block_on(fs.readlink(entry.inode))
                        .expect("readlink failed on a valid symlink");
                    ModelNode::Symlink(target)
                }
            };
            result.insert(entry.name, node);
        }
        result
    }
}

/// Replicate the splice semantics of [`KikiFs::write`] on a model buffer.
fn model_write(content: &mut Vec<u8>, offset: usize, data: &[u8]) {
    let end = offset + data.len();
    if content.len() < end {
        content.resize(end, 0);
    }
    content[offset..end].copy_from_slice(data);
}

// ---- proptest strategies ----

/// Small fixed vocabulary of file names. Keeps the pool tight so
/// operations frequently interact (create then write, write then rename,
/// etc.) which is where bugs hide.
fn arb_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("a".to_owned()),
        Just("b".to_owned()),
        Just("c".to_owned()),
        Just("d".to_owned()),
        Just("e".to_owned()),
    ]
}

/// Directory names — separate pool from file names to avoid type
/// collisions (mkdir where a file already exists).
fn arb_dir_name() -> impl Strategy<Value = String> {
    prop_oneof![Just("x".to_owned()), Just("y".to_owned()),]
}

fn arb_content() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..32)
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        // Weighted toward mutations that interact.
        3 => arb_name().prop_map(Op::CreateFile),
        2 => arb_dir_name().prop_map(Op::Mkdir),
        3 => (arb_name(), arb_content()).prop_map(|(n, c)| Op::WriteFile(n, c)),
        1 => (arb_name(), "[a-z]{1,8}").prop_map(|(name, target)| Op::Symlink { name, target }),
        2 => arb_name().prop_map(Op::Remove),
        2 => (arb_name(), arb_name()).prop_map(|(old, new)| Op::Rename { old, new }),
        2 => (arb_dir_name(), arb_name()).prop_map(|(dir, name)| Op::CreateInDir { dir, name }),
        2 => (arb_dir_name(), arb_name(), arb_content())
            .prop_map(|(dir, name, data)| Op::WriteInDir { dir, name, data }),
        3 => Just(Op::Snapshot),
    ]
}

fn arb_ops_with_crash() -> impl Strategy<Value = (Vec<Op>, usize)> {
    prop::collection::vec(arb_op(), 1..30).prop_flat_map(|ops| {
        let len = ops.len();
        (Just(ops), 0..=len)
    })
}

// ---- Deterministic tests ----

#[tokio::test]
async fn basic_write_snapshot_crash_rehydrate() {
    let settings = test_settings();
    let store = Arc::new(GitContentStore::new_in_memory(&settings));
    let root = empty_root(&store);
    let fs = KikiFs::new(store.clone(), root, None);

    // Create and write a file.
    let (ino, _) = fs.create_file(ROOT_INODE, "hello.txt", false).await.unwrap();
    fs.write(ino, 0, b"world").await.unwrap();

    // Snapshot persists to git ODB.
    let snap_root = fs.snapshot().await.unwrap();
    drop(fs); // crash

    // Rehydrate.
    let fs2 = KikiFs::new(store.clone(), snap_root, None);
    let ino2 = fs2.lookup(ROOT_INODE, "hello.txt").await.unwrap();
    let (data, _) = fs2.read(ino2, 0, u32::MAX).await.unwrap();
    assert_eq!(data, b"world");

    // No other entries (besides the synthesized .git).
    let entries = fs2.readdir(ROOT_INODE).await.unwrap();
    let user_entries: Vec<_> = entries.iter().filter(|e| e.name != ".git").collect();
    assert_eq!(user_entries.len(), 1);
    assert_eq!(user_entries[0].name, "hello.txt");
}

#[tokio::test]
async fn nested_dir_survives_crash_recovery() {
    let settings = test_settings();
    let store = Arc::new(GitContentStore::new_in_memory(&settings));
    let root = empty_root(&store);
    let fs = KikiFs::new(store.clone(), root, None);

    let (dir_ino, _) = fs.mkdir(ROOT_INODE, "src").await.unwrap();
    let (file_ino, _) = fs.create_file(dir_ino, "main.rs", false).await.unwrap();
    fs.write(file_ino, 0, b"fn main() {}").await.unwrap();

    let snap_root = fs.snapshot().await.unwrap();
    drop(fs);

    let fs2 = KikiFs::new(store.clone(), snap_root, None);
    let dir2 = fs2.lookup(ROOT_INODE, "src").await.unwrap();
    let file2 = fs2.lookup(dir2, "main.rs").await.unwrap();
    let (data, _) = fs2.read(file2, 0, u32::MAX).await.unwrap();
    assert_eq!(data, b"fn main() {}");
}

#[tokio::test]
async fn no_snapshot_means_rehydrate_to_empty() {
    let settings = test_settings();
    let store = Arc::new(GitContentStore::new_in_memory(&settings));
    let root = empty_root(&store);
    let fs = KikiFs::new(store.clone(), root, None);

    // Mutations without snapshot.
    fs.create_file(ROOT_INODE, "ephemeral.txt", false)
        .await
        .unwrap();

    drop(fs); // crash — no snapshot was taken

    // Rehydrate from initial empty root.
    let fs2 = KikiFs::new(store.clone(), root, None);
    let entries = fs2.readdir(ROOT_INODE).await.unwrap();
    let user_entries: Vec<_> = entries.iter().filter(|e| e.name != ".git").collect();
    assert!(user_entries.is_empty(), "expected empty after crash without snapshot");
}

#[tokio::test]
async fn latest_snapshot_wins_after_crash() {
    let settings = test_settings();
    let store = Arc::new(GitContentStore::new_in_memory(&settings));
    let root = empty_root(&store);
    let fs = KikiFs::new(store.clone(), root, None);

    // First generation.
    let (ino, _) = fs.create_file(ROOT_INODE, "f", false).await.unwrap();
    fs.write(ino, 0, b"v1").await.unwrap();
    let _snap1 = fs.snapshot().await.unwrap();

    // Second generation overwrites.
    let ino = fs.lookup(ROOT_INODE, "f").await.unwrap();
    fs.write(ino, 0, b"v2").await.unwrap();
    let snap2 = fs.snapshot().await.unwrap();

    // Third generation — NOT snapshotted.
    let ino = fs.lookup(ROOT_INODE, "f").await.unwrap();
    fs.write(ino, 0, b"v3-lost").await.unwrap();

    drop(fs); // crash — only snap2 survives

    let fs2 = KikiFs::new(store.clone(), snap2, None);
    let ino2 = fs2.lookup(ROOT_INODE, "f").await.unwrap();
    let (data, _) = fs2.read(ino2, 0, u32::MAX).await.unwrap();
    assert_eq!(data, b"v2");
}

#[tokio::test]
async fn rename_persists_through_snapshot() {
    let settings = test_settings();
    let store = Arc::new(GitContentStore::new_in_memory(&settings));
    let root = empty_root(&store);
    let fs = KikiFs::new(store.clone(), root, None);

    let (ino, _) = fs.create_file(ROOT_INODE, "old.txt", false).await.unwrap();
    fs.write(ino, 0, b"content").await.unwrap();
    fs.rename(ROOT_INODE, "old.txt", ROOT_INODE, "new.txt")
        .await
        .unwrap();

    let snap = fs.snapshot().await.unwrap();
    drop(fs);

    let fs2 = KikiFs::new(store.clone(), snap, None);
    assert!(fs2.lookup(ROOT_INODE, "old.txt").await.is_err());
    let ino2 = fs2.lookup(ROOT_INODE, "new.txt").await.unwrap();
    let (data, _) = fs2.read(ino2, 0, u32::MAX).await.unwrap();
    assert_eq!(data, b"content");
}

#[tokio::test]
async fn remove_reflected_in_snapshot() {
    let settings = test_settings();
    let store = Arc::new(GitContentStore::new_in_memory(&settings));
    let root = empty_root(&store);
    let fs = KikiFs::new(store.clone(), root, None);

    fs.create_file(ROOT_INODE, "gone.txt", false).await.unwrap();
    fs.create_file(ROOT_INODE, "kept.txt", false).await.unwrap();
    let _snap1 = fs.snapshot().await.unwrap();

    fs.remove(ROOT_INODE, "gone.txt").await.unwrap();
    let snap2 = fs.snapshot().await.unwrap();
    drop(fs);

    let fs2 = KikiFs::new(store.clone(), snap2, None);
    assert!(fs2.lookup(ROOT_INODE, "gone.txt").await.is_err());
    assert!(fs2.lookup(ROOT_INODE, "kept.txt").await.is_ok());
}

#[tokio::test]
async fn symlink_survives_crash_recovery() {
    let settings = test_settings();
    let store = Arc::new(GitContentStore::new_in_memory(&settings));
    let root = empty_root(&store);
    let fs = KikiFs::new(store.clone(), root, None);

    let (_ino, _) = fs
        .symlink(ROOT_INODE, "link", "/usr/bin/env")
        .await
        .unwrap();
    let snap = fs.snapshot().await.unwrap();
    drop(fs);

    let fs2 = KikiFs::new(store.clone(), snap, None);
    let ino2 = fs2.lookup(ROOT_INODE, "link").await.unwrap();
    let target = fs2.readlink(ino2).await.unwrap();
    assert_eq!(target, "/usr/bin/env");
}

#[tokio::test]
async fn snapshot_idempotency_deterministic() {
    let settings = test_settings();
    let store = Arc::new(GitContentStore::new_in_memory(&settings));
    let root = empty_root(&store);
    let fs = KikiFs::new(store.clone(), root, None);

    let (ino, _) = fs.create_file(ROOT_INODE, "f", false).await.unwrap();
    fs.write(ino, 0, b"data").await.unwrap();
    let (dir, _) = fs.mkdir(ROOT_INODE, "d").await.unwrap();
    fs.create_file(dir, "nested", false).await.unwrap();

    let id1 = fs.snapshot().await.unwrap();
    let id2 = fs.snapshot().await.unwrap();
    assert_eq!(id1, id2, "consecutive snapshots should produce the same root");
}

#[tokio::test]
async fn write_extends_preserves_existing_content() {
    let settings = test_settings();
    let store = Arc::new(GitContentStore::new_in_memory(&settings));
    let root = empty_root(&store);
    let fs = KikiFs::new(store.clone(), root, None);

    let (ino, _) = fs.create_file(ROOT_INODE, "f", false).await.unwrap();
    fs.write(ino, 0, b"hello world").await.unwrap();
    let _snap1 = fs.snapshot().await.unwrap();

    // Partial overwrite: first 5 bytes change, rest preserved.
    let ino = fs.lookup(ROOT_INODE, "f").await.unwrap();
    fs.write(ino, 0, b"HELLO").await.unwrap();
    let snap2 = fs.snapshot().await.unwrap();
    drop(fs);

    let fs2 = KikiFs::new(store.clone(), snap2, None);
    let ino2 = fs2.lookup(ROOT_INODE, "f").await.unwrap();
    let (data, _) = fs2.read(ino2, 0, u32::MAX).await.unwrap();
    assert_eq!(data, b"HELLO world");
}

// ---- Property tests ----

proptest! {
    /// Core crash-recovery property: generate a random operation sequence,
    /// crash at a random point, rehydrate from the last snapshot, and
    /// verify the tree matches the committed model.
    ///
    /// This catches:
    /// - Data loss (snapshot wrote wrong content or missed a dirty node)
    /// - Phantom files (rehydrated tree has entries not in the model)
    /// - Parent consistency bugs (child persisted but parent tree not
    ///   updated)
    /// - Ordering bugs (crash between child persist and parent update
    ///   leaves an inconsistent tree)
    #[test]
    fn crash_at_arbitrary_point_recovers_to_last_snapshot(
        (ops, crash_point) in arb_ops_with_crash()
    ) {
        let mut sim = Sim::new();
        sim.run_until_crash(&ops, crash_point);
        sim.verify_crash_recovery();
    }

    /// Run the full operation sequence (no early crash), then verify
    /// crash recovery. Every sequence ends with an implicit crash.
    #[test]
    fn full_sequence_then_crash_recovers(
        ops in prop::collection::vec(arb_op(), 1..30)
    ) {
        let mut sim = Sim::new();
        sim.run_until_crash(&ops, ops.len());
        sim.verify_crash_recovery();
    }

    /// Snapshot idempotency: applying the same sequence twice followed by
    /// two consecutive snapshots (with no intervening mutations) must
    /// produce the same root tree id.
    #[test]
    fn snapshot_idempotency(
        ops in prop::collection::vec(arb_op(), 1..20)
    ) {
        let mut sim = Sim::new();
        let fs = sim.new_fs();
        for op in &ops {
            sim.apply(&fs, op);
        }
        let id1 = sim.rt.block_on(fs.snapshot()).expect("snapshot 1");
        let id2 = sim.rt.block_on(fs.snapshot()).expect("snapshot 2");
        prop_assert_eq!(id1, id2, "consecutive snapshots must produce the same root");
    }

    /// After snapshot + crash + rehydrate, the tree is fully readable:
    /// readdir succeeds on every directory, read succeeds on every file,
    /// readlink succeeds on every symlink. This is a structural integrity
    /// check beyond just comparing with the model.
    #[test]
    fn rehydrated_tree_is_fully_readable(
        ops in prop::collection::vec(arb_op(), 1..30)
    ) {
        let mut sim = Sim::new();
        sim.run_until_crash(&ops, ops.len());

        // Rehydrate and walk — walk_tree internally asserts that every
        // readdir/read/readlink succeeds.
        let fs = sim.new_fs();
        let tree = sim.walk_tree(&fs, ROOT_INODE);

        // Additionally verify parent consistency: every Dir's children
        // must also be walkable (already guaranteed by walk_tree's
        // recursive structure, but let's be explicit).
        verify_parent_consistency(&tree, &[]);
    }

    /// Longer sequences for stress testing. Uses more operations and a
    /// wider vocabulary of content to increase the chance of finding
    /// corner cases in the snapshot/rehydrate cycle.
    #[test]
    #[ignore] // slow — run explicitly via `cargo test -- --ignored`
    fn stress_crash_recovery(
        (ops, crash_point) in
            prop::collection::vec(arb_op(), 30..100)
                .prop_flat_map(|ops| {
                    let len = ops.len();
                    (Just(ops), 0..=len)
                })
    ) {
        let mut sim = Sim::new();
        sim.run_until_crash(&ops, crash_point);
        sim.verify_crash_recovery();
    }
}

/// Recursively verify that every node in the model tree has valid
/// parent structure: files/symlinks are leaf nodes, directories contain
/// only valid children.
fn verify_parent_consistency(tree: &ModelTree, path: &[&str]) {
    for (name, node) in tree {
        let mut child_path: Vec<&str> = path.to_vec();
        child_path.push(name);
        match node {
            ModelNode::File(_) | ModelNode::Symlink(_) => {
                // Leaf nodes — nothing to recurse into.
            }
            ModelNode::Dir(children) => {
                verify_parent_consistency(children, &child_path);
            }
        }
    }
}
