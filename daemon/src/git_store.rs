//! Content store backed by jj-lib's [`GitBackend`], replacing the
//! BLAKE3/prost/redb content tables with a bare git repo.
//!
//! The op-store tables (views, operations) remain in redb — those are
//! opaque jj-lib data with 64-byte BLAKE2b-512 keys and don't map to
//! git objects.
//!
//! ## Design (see `docs/GIT_CONVERGENCE.md`)
//!
//! - **Reads** bypass `GitBackend`'s internal `Mutex<gix::Repository>`
//!   by calling `git_repo()` which returns a fresh thread-local handle.
//!   gix's ODB uses lock-free `ArcSwap` + atomics for reads.
//!
//! - **Writes** go through `GitBackend` (which holds the mutex) for
//!   commits (extras table, change-id headers), and directly through
//!   gix for blobs/trees (atomic tmp+rename, safe without mutex).
//!
//! - **Op-store** (views, operations) stays in redb as before.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use gix::objs::Exists as _;
use gix::prelude::Write as _;
use jj_lib::backend::{self, Backend as _, CommitId, FileId, SymlinkId, TreeId};
use jj_lib::git_backend::GitBackend;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo_path::RepoPath;
use jj_lib::settings::UserSettings;
use redb::{Database, ReadableTable, TableDefinition};

// ---- Op-store redb tables (unchanged from store.rs) ----

const VIEWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("views_v1");
const OPERATIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("operations_v1");

/// Result of a prefix scan against the operations table.
/// Same shape as jj-lib's `PrefixResolution`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpPrefixResult {
    None,
    Single(Vec<u8>),
    Ambiguous,
}

/// Hex-encode arbitrary bytes. Used by prefix-match scanning.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// Content store backed by a bare git repo (via [`GitBackend`]) plus
/// redb for op-store tables.
///
/// Replaces the previous `Store` which used BLAKE3-addressed redb
/// content tables. IDs are now 20-byte SHA-1 (standard git).
pub struct GitContentStore {
    git_backend: GitBackend,
    op_db: Arc<Database>,
}

impl std::fmt::Debug for GitContentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitContentStore")
            .field("git_repo_path", &self.git_backend.git_repo_path())
            .finish()
    }
}

impl GitContentStore {
    /// Create a new git-backed content store. Initializes a bare git
    /// repo at `store_path/git/` and the extras table at
    /// `store_path/extra/`. Opens (or creates) the redb database at
    /// `redb_path` for op-store tables.
    pub fn init(
        settings: &UserSettings,
        store_path: &Path,
        redb_path: &Path,
    ) -> Result<Self> {
        std::fs::create_dir_all(store_path)
            .with_context(|| format!("creating store dir {}", store_path.display()))?;
        let git_backend = GitBackend::init_internal(settings, store_path)
            .map_err(|e| anyhow!("GitBackend::init_internal: {e}"))?;
        let op_db = Self::open_op_db(redb_path)?;
        Ok(GitContentStore { git_backend, op_db })
    }

    /// Open an existing git-backed content store.
    pub fn load(
        settings: &UserSettings,
        store_path: &Path,
        redb_path: &Path,
    ) -> Result<Self> {
        let git_backend = GitBackend::load(settings, store_path)
            .map_err(|e| anyhow!("GitBackend::load: {e}"))?;
        let op_db = Self::open_op_db(redb_path)?;
        Ok(GitContentStore { git_backend, op_db })
    }

    fn open_op_db(redb_path: &Path) -> Result<Arc<Database>> {
        if let Some(parent) = redb_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating op-store dir {}", parent.display()))?;
        }
        let db = Database::create(redb_path)
            .with_context(|| format!("opening redb op-store at {}", redb_path.display()))?;
        // Materialize the op-store tables on first open.
        let txn = db.begin_write().context("redb begin_write")?;
        {
            let _views = txn.open_table(VIEWS).context("open views table")?;
            let _ops = txn
                .open_table(OPERATIONS)
                .context("open operations table")?;
        }
        txn.commit().context("redb commit (materialize op tables)")?;
        Ok(Arc::new(db))
    }

    /// Test-only constructor: in-memory redb for op-store, temp-dir for git.
    #[cfg(test)]
    pub fn new_in_memory(settings: &UserSettings) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store_path = tmp.path().join("store");
        std::fs::create_dir_all(&store_path).expect("create store dir");
        // We leak the TempDir so the git repo survives the constructor.
        // Tests are short-lived; this is acceptable.
        let tmp = Box::leak(Box::new(tmp));
        let _ = tmp;
        let git_backend = GitBackend::init_internal(settings, &store_path)
            .expect("GitBackend::init_internal in test");
        let op_db = redb::Builder::new()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .expect("in-memory redb");
        let txn = op_db.begin_write().unwrap();
        {
            let _v = txn.open_table(VIEWS).unwrap();
            let _o = txn.open_table(OPERATIONS).unwrap();
        }
        txn.commit().unwrap();
        GitContentStore {
            git_backend,
            op_db: Arc::new(op_db),
        }
    }

    // ---- ID constants ----

    /// The empty tree ID (well-known git SHA-1).
    pub fn empty_tree_id(&self) -> &TreeId {
        self.git_backend.empty_tree_id()
    }

    /// The root commit ID (all-zeros, 20 bytes).
    pub fn root_commit_id(&self) -> &CommitId {
        self.git_backend.root_commit_id()
    }

    /// Path to the bare git repo.
    pub fn git_repo_path(&self) -> &Path {
        self.git_backend.git_repo_path()
    }

    /// Hand out the redb database handle for sibling modules
    /// (e.g. [`crate::local_refs::LocalRefs`]).
    pub fn database(&self) -> Arc<Database> {
        self.op_db.clone()
    }

    // ---- Content reads (concurrent, bypasses GitBackend mutex) ----

    /// Read a file blob from the git ODB. Returns the raw file content.
    pub fn read_file(&self, id: &[u8]) -> Result<Option<Vec<u8>>> {
        let oid = match gix::ObjectId::try_from(id) {
            Ok(oid) => oid,
            Err(_) => return Err(anyhow!("invalid git object id ({} bytes)", id.len())),
        };
        let repo = self.git_backend.git_repo();
        match repo.find_object(oid) {
            Ok(obj) => {
                let mut blob = obj
                    .try_into_blob()
                    .map_err(|e| anyhow!("object is not a blob: {e}"))?;
                Ok(Some(blob.take_data()))
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(anyhow!("read_file: {e}")),
        }
    }

    /// Read a symlink target from the git ODB (stored as a blob).
    pub fn read_symlink(&self, id: &[u8]) -> Result<Option<String>> {
        match self.read_file(id)? {
            Some(bytes) => {
                let target = String::from_utf8(bytes)
                    .context("symlink target is not valid UTF-8")?;
                Ok(Some(target))
            }
            None => Ok(None),
        }
    }

    /// Read a tree from the git ODB. Returns entries as
    /// `(name, kind, id)` tuples in git-sorted order.
    pub fn read_tree(&self, id: &[u8]) -> Result<Option<Vec<GitTreeEntry>>> {
        // Short-circuit the well-known empty tree.
        if id == self.git_backend.empty_tree_id().as_bytes() {
            return Ok(Some(Vec::new()));
        }
        let oid = match gix::ObjectId::try_from(id) {
            Ok(oid) => oid,
            Err(_) => return Err(anyhow!("invalid git object id ({} bytes)", id.len())),
        };
        let repo = self.git_backend.git_repo();
        let git_tree = match repo.find_object(oid) {
            Ok(obj) => match obj.try_into_tree() {
                Ok(t) => t,
                Err(e) => return Err(anyhow!("object is not a tree: {e}")),
            },
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(anyhow!("read_tree: {e}")),
        };
        let entries = git_tree
            .iter()
            .map(|entry_r| {
                let entry = entry_r.context("iterating tree entries")?;
                let name = std::str::from_utf8(entry.filename())
                    .context("tree entry name not UTF-8")?
                    .to_owned();
                let kind = match entry.mode().kind() {
                    gix::object::tree::EntryKind::Tree => GitEntryKind::Tree,
                    gix::object::tree::EntryKind::Blob => GitEntryKind::File { executable: false },
                    gix::object::tree::EntryKind::BlobExecutable => {
                        GitEntryKind::File { executable: true }
                    }
                    gix::object::tree::EntryKind::Link => GitEntryKind::Symlink,
                    gix::object::tree::EntryKind::Commit => GitEntryKind::Submodule,
                };
                let id = entry.oid().as_bytes().to_vec();
                Ok(GitTreeEntry { name, kind, id })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(entries))
    }

    /// Read a commit via GitBackend (handles extras table for
    /// change-id and predecessors).
    pub fn read_commit(&self, id: &[u8]) -> Result<Option<backend::Commit>> {
        let commit_id = CommitId::from_bytes(id);
        match pollster::block_on(self.git_backend.read_commit(&commit_id)) {
            Ok(commit) => Ok(Some(commit)),
            Err(backend::BackendError::ObjectNotFound { .. }) => Ok(None),
            Err(e) => Err(anyhow!("read_commit: {e}")),
        }
    }

    // ---- Content writes ----

    /// Write a file blob to the git ODB. Returns the 20-byte SHA-1 id.
    pub fn write_file(&self, content: &[u8]) -> Result<Vec<u8>> {
        let file_id = pollster::block_on(
            self.git_backend
                .write_file(RepoPath::root(), &mut &content[..]),
        )
        .map_err(|e| anyhow!("write_file: {e}"))?;
        Ok(file_id.to_bytes())
    }

    /// Write a symlink target as a git blob. Returns the 20-byte SHA-1 id.
    pub fn write_symlink(&self, target: &str) -> Result<Vec<u8>> {
        let sym_id =
            pollster::block_on(self.git_backend.write_symlink(RepoPath::root(), target))
                .map_err(|e| anyhow!("write_symlink: {e}"))?;
        Ok(sym_id.to_bytes())
    }

    /// Write a tree to the git ODB. Returns the 20-byte SHA-1 id.
    pub fn write_tree(&self, entries: &[GitTreeEntry]) -> Result<Vec<u8>> {
        let jj_tree = self.entries_to_jj_tree(entries);
        let tree_id =
            pollster::block_on(self.git_backend.write_tree(RepoPath::root(), &jj_tree))
                .map_err(|e| anyhow!("write_tree: {e}"))?;
        Ok(tree_id.to_bytes())
    }

    /// Write a commit via GitBackend (handles extras table, change-id
    /// headers). Returns `(commit_id, stored_commit)`.
    pub fn write_commit(
        &self,
        commit: backend::Commit,
    ) -> Result<(Vec<u8>, backend::Commit)> {
        let (id, stored) =
            pollster::block_on(self.git_backend.write_commit(commit, None))
                .map_err(|e| anyhow!("write_commit: {e}"))?;
        Ok((id.to_bytes(), stored))
    }

    // ---- Raw git object bytes (for RemoteStore sync) ----

    /// Read raw git object bytes by SHA-1 id. Returns the decompressed
    /// object content (without the git header). Used by RemoteStore
    /// `put_blob` for replication.
    pub fn read_git_object_bytes(&self, id: &[u8]) -> Result<Option<(GitObjectKind, Vec<u8>)>> {
        let oid = gix::ObjectId::try_from(id)
            .map_err(|_| anyhow!("invalid git object id ({} bytes)", id.len()))?;
        let repo = self.git_backend.git_repo();
        match repo.find_object(oid) {
            Ok(obj) => {
                let kind = match obj.kind {
                    gix::object::Kind::Blob => GitObjectKind::Blob,
                    gix::object::Kind::Tree => GitObjectKind::Tree,
                    gix::object::Kind::Commit => GitObjectKind::Commit,
                    gix::object::Kind::Tag => GitObjectKind::Tag,
                };
                Ok(Some((kind, obj.detach().data)))
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(anyhow!("read_git_object_bytes: {e}")),
        }
    }

    /// Write raw git object bytes into the ODB. The caller provides the
    /// kind and raw content; gix computes the SHA-1 and writes the
    /// object. Returns the 20-byte id.
    pub fn write_git_object_bytes(
        &self,
        kind: GitObjectKind,
        data: &[u8],
    ) -> Result<Vec<u8>> {
        let gix_kind = match kind {
            GitObjectKind::Blob => gix::objs::Kind::Blob,
            GitObjectKind::Tree => gix::objs::Kind::Tree,
            GitObjectKind::Commit => gix::objs::Kind::Commit,
            GitObjectKind::Tag => gix::objs::Kind::Tag,
        };
        let repo = self.git_backend.git_repo();
        let oid = repo
            .objects
            .write_buf(gix_kind, data)
            .map_err(|e| anyhow!("write_git_object_bytes: {e}"))?;
        Ok(oid.as_bytes().to_vec())
    }

    /// Check whether a git object exists in the ODB.
    pub fn has_git_object(&self, id: &[u8]) -> Result<bool> {
        let oid = gix::ObjectId::try_from(id)
            .map_err(|_| anyhow!("invalid git object id ({} bytes)", id.len()))?;
        let repo = self.git_backend.git_repo();
        Ok(repo.objects.exists(&oid))
    }

    // ---- Extras table (for RemoteStore replication) ----

    /// Read the extras table entry for a commit. Returns the raw
    /// protobuf bytes (change-id + predecessors). Used by RemoteStore
    /// to replicate extras alongside git objects.
    pub fn read_extras(&self, commit_id: &[u8]) -> Result<Option<Vec<u8>>> {
        // The extras table is managed by GitBackend's internal
        // TableStore. We can read it by reading the commit (which
        // populates extras) and then re-serializing the extras fields.
        // However, for raw replication we want the exact bytes.
        // For now, we round-trip through read_commit + serialize.
        //
        // TODO: direct TableStore access when jj-lib exposes it.
        let commit = match self.read_commit(commit_id)? {
            Some(c) => c,
            None => return Ok(None),
        };
        let extras = serialize_extras_for_replication(&commit);
        Ok(Some(extras))
    }

    // ---- Op-store tables (redb, unchanged) ----

    /// Read a view blob by its raw id bytes.
    pub fn get_view_bytes(&self, id: &[u8]) -> Result<Option<Bytes>> {
        self.read_raw_varkey(VIEWS, id)
    }

    /// Write a view blob at the caller-provided id.
    pub fn write_view_bytes(&self, id: &[u8], bytes: &[u8]) -> Result<()> {
        self.write_raw_varkey(VIEWS, id, bytes)
    }

    /// Read an operation blob by its raw id bytes.
    pub fn get_operation_bytes(&self, id: &[u8]) -> Result<Option<Bytes>> {
        self.read_raw_varkey(OPERATIONS, id)
    }

    /// Write an operation blob at the caller-provided id.
    pub fn write_operation_bytes(&self, id: &[u8], bytes: &[u8]) -> Result<()> {
        self.write_raw_varkey(OPERATIONS, id, bytes)
    }

    /// Prefix scan over the operations table.
    pub fn operation_ids_matching_prefix(&self, hex_prefix: &str) -> Result<OpPrefixResult> {
        let txn = self.op_db.begin_read().context("redb begin_read")?;
        let tbl = txn
            .open_table(OPERATIONS)
            .context("open operations table")?;
        let mut matched: Option<Vec<u8>> = None;
        for entry in tbl.iter().context("iterate operations table")? {
            let (key, _value) = entry.context("operations table entry")?;
            let key_bytes = key.value();
            let key_hex = hex_encode(key_bytes);
            if key_hex.starts_with(hex_prefix) {
                if matched.is_some() {
                    return Ok(OpPrefixResult::Ambiguous);
                }
                matched = Some(key_bytes.to_vec());
            }
        }
        Ok(match matched {
            Some(id) => OpPrefixResult::Single(id),
            None => OpPrefixResult::None,
        })
    }

    // ---- Internal helpers ----

    fn read_raw_varkey(
        &self,
        table: TableDefinition<'_, &'static [u8], &'static [u8]>,
        id: &[u8],
    ) -> Result<Option<Bytes>> {
        let txn = self.op_db.begin_read().context("redb begin_read")?;
        let tbl = txn.open_table(table).context("open table for read")?;
        let raw = tbl.get(id).context("redb get")?;
        Ok(raw.map(|slot| Bytes::copy_from_slice(slot.value())))
    }

    fn write_raw_varkey(
        &self,
        table: TableDefinition<'_, &'static [u8], &'static [u8]>,
        id: &[u8],
        bytes: &[u8],
    ) -> Result<()> {
        let txn = self.op_db.begin_write().context("redb begin_write")?;
        {
            let mut tbl = txn.open_table(table).context("open table for write")?;
            tbl.insert(id, bytes).context("redb insert")?;
        }
        txn.commit().context("redb commit")?;
        Ok(())
    }

    /// Convert our `GitTreeEntry` list to jj-lib's `backend::Tree`.
    fn entries_to_jj_tree(&self, entries: &[GitTreeEntry]) -> backend::Tree {
        use jj_lib::backend::TreeValue;
        use jj_lib::repo_path::RepoPathComponentBuf;

        let jj_entries: Vec<(RepoPathComponentBuf, TreeValue)> = entries
            .iter()
            .map(|e| {
                let name = RepoPathComponentBuf::new(&e.name).unwrap();
                let value = match e.kind {
                    GitEntryKind::File { executable } => TreeValue::File {
                        id: FileId::from_bytes(&e.id),
                        executable,
                        copy_id: backend::CopyId::placeholder(),
                    },
                    GitEntryKind::Tree => TreeValue::Tree(TreeId::from_bytes(&e.id)),
                    GitEntryKind::Symlink => TreeValue::Symlink(SymlinkId::from_bytes(&e.id)),
                    GitEntryKind::Submodule => {
                        TreeValue::GitSubmodule(CommitId::from_bytes(&e.id))
                    }
                };
                (name, value)
            })
            .collect();
        backend::Tree::from_sorted_entries(jj_entries)
    }
}

/// Git object kind for raw object byte transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitObjectKind {
    Blob,
    Tree,
    Commit,
    Tag,
}

/// A single tree entry as seen by the daemon. Simpler than jj-lib's
/// `TreeValue` — no merge/conflict handling at this layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTreeEntry {
    pub name: String,
    pub kind: GitEntryKind,
    pub id: Vec<u8>,
}

/// Entry kind within a git tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitEntryKind {
    File { executable: bool },
    Tree,
    Symlink,
    Submodule,
}

/// Serialize the extras fields (change-id + predecessors) for
/// replication. Uses a minimal protobuf-compatible encoding.
///
/// This mirrors the format in jj-lib's `git_store.proto` Commit
/// message: field 4 = change_id (bytes), field 2 = predecessors
/// (repeated bytes).
fn serialize_extras_for_replication(commit: &backend::Commit) -> Vec<u8> {
    use prost::Message;

    // Re-use the proto module's Commit message for encoding, but only
    // populate the extras-relevant fields.
    let extras_proto = proto::jj_interface::Commit {
        change_id: commit.change_id.to_bytes(),
        predecessors: commit
            .predecessors
            .iter()
            .map(|id| id.to_bytes())
            .collect(),
        ..Default::default()
    };
    extras_proto.encode_to_vec()
}

/// Check if a gix error is a "not found" error.
fn is_not_found(err: &gix::object::find::existing::Error) -> bool {
    matches!(
        err,
        gix::object::find::existing::Error::NotFound { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use jj_lib::settings::UserSettings;

    fn test_settings() -> UserSettings {
        let toml_str = r#"
            user.name = "Test User"
            user.email = "test@example.com"
            operation.hostname = "test"
            operation.username = "test"
            debug.randomness-seed = 42
        "#;
        let mut config = jj_lib::config::StackedConfig::with_defaults();
        config.add_layer(jj_lib::config::ConfigLayer::parse(
            jj_lib::config::ConfigSource::User,
            toml_str,
        ).unwrap());
        UserSettings::from_config(config).unwrap()
    }

    #[test]
    fn init_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store");
        let redb_path = tmp.path().join("ops.redb");
        let settings = test_settings();

        let store = GitContentStore::init(&settings, &store_path, &redb_path)
            .expect("init");

        // Verify the git repo was created.
        assert!(store_path.join("git").exists());
        assert!(store_path.join("extra").exists());

        // Verify the empty tree id is the well-known value.
        assert_eq!(
            store.empty_tree_id().hex(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );

        drop(store);

        // Re-open.
        let _store2 = GitContentStore::load(&settings, &store_path, &redb_path)
            .expect("load");
    }

    #[test]
    fn write_and_read_file() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);

        let content = b"hello, git convergence!";
        let id = store.write_file(content).expect("write_file");
        assert_eq!(id.len(), 20, "SHA-1 id should be 20 bytes");

        let data = store.read_file(&id).expect("read_file").expect("present");
        assert_eq!(data, content);
    }

    #[test]
    fn write_and_read_symlink() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);

        let target = "/usr/bin/env";
        let id = store.write_symlink(target).expect("write_symlink");
        assert_eq!(id.len(), 20);

        let got = store
            .read_symlink(&id)
            .expect("read_symlink")
            .expect("present");
        assert_eq!(got, target);
    }

    #[test]
    fn write_and_read_tree() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);

        // Write two files.
        let hello_id = store.write_file(b"hello").unwrap();
        let world_id = store.write_file(b"world").unwrap();

        // Write a tree containing those files.
        let entries = vec![
            GitTreeEntry {
                name: "hello.txt".into(),
                kind: GitEntryKind::File { executable: false },
                id: hello_id.clone(),
            },
            GitTreeEntry {
                name: "world.txt".into(),
                kind: GitEntryKind::File { executable: true },
                id: world_id.clone(),
            },
        ];
        let tree_id = store.write_tree(&entries).expect("write_tree");
        assert_eq!(tree_id.len(), 20);

        // Read it back.
        let got = store
            .read_tree(&tree_id)
            .expect("read_tree")
            .expect("present");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "hello.txt");
        assert_eq!(got[0].id, hello_id);
        assert_eq!(got[1].name, "world.txt");
        assert_eq!(got[1].id, world_id);
    }

    #[test]
    fn empty_tree_reads_empty() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);

        let empty_id = store.empty_tree_id().as_bytes();
        let entries = store
            .read_tree(empty_id)
            .expect("read_tree")
            .expect("present");
        assert!(entries.is_empty());
    }

    #[test]
    fn missing_object_returns_none() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);

        let bogus = [0xff; 20];
        assert!(store.read_file(&bogus).expect("read").is_none());
        assert!(store.read_symlink(&bogus).expect("read").is_none());
        assert!(store.read_tree(&bogus).expect("read").is_none());
    }

    #[test]
    fn op_store_roundtrip() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);

        let id = [0xab; 64];
        let data = b"view-bytes";
        store.write_view_bytes(&id, data).expect("write");
        let got = store
            .get_view_bytes(&id)
            .expect("read")
            .expect("present");
        assert_eq!(got.as_ref(), data);
    }

    #[test]
    fn raw_git_object_roundtrip() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);

        let content = b"raw blob content";
        let id = store
            .write_git_object_bytes(GitObjectKind::Blob, content)
            .expect("write");
        assert_eq!(id.len(), 20);

        let (kind, data) = store
            .read_git_object_bytes(&id)
            .expect("read")
            .expect("present");
        assert_eq!(kind, GitObjectKind::Blob);
        assert_eq!(data, content);

        assert!(store.has_git_object(&id).expect("has"));
        assert!(!store.has_git_object(&[0xff; 20]).expect("has bogus"));
    }
}
