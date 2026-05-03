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
//! - **Writes** of files, trees, symlinks, and commits go through
//!   `GitBackend` (which holds the mutex). Raw git object byte writes
//!   (`write_git_object_bytes`, used by RemoteStore sync) bypass the
//!   mutex and write directly through gix (atomic tmp+rename, safe
//!   without mutex).
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

/// SHA-1 hash length (20 bytes). Used as the key size for the extras
/// stacked table.
const HASH_LENGTH: usize = 20;

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
    /// Holds the TempDir for in-memory test stores so it isn't dropped
    /// (and the git repo directory deleted) while the store is alive.
    #[cfg(test)]
    _tmp: Option<tempfile::TempDir>,
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
        Ok(GitContentStore {
            git_backend,
            op_db,
            #[cfg(test)]
            _tmp: None,
        })
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
        Ok(GitContentStore {
            git_backend,
            op_db,
            #[cfg(test)]
            _tmp: None,
        })
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
            _tmp: Some(tmp),
        }
    }

    // ---- ID constants ----

    /// The empty tree ID (well-known git SHA-1).
    pub fn empty_tree_id(&self) -> &TreeId {
        self.git_backend.empty_tree_id()
    }

    /// The root commit ID (all-zeros, 20 bytes).
    #[allow(dead_code)] // used by future git push/fetch RPCs
    pub fn root_commit_id(&self) -> &CommitId {
        self.git_backend.root_commit_id()
    }

    /// Path to the bare git repo.
    #[allow(dead_code)] // used by future git push/fetch RPCs and tests
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
        let jj_tree = self.entries_to_jj_tree(entries)
            .context("building jj tree from entries")?;
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
    #[allow(dead_code)] // used by future remote sync and tests
    pub fn has_git_object(&self, id: &[u8]) -> Result<bool> {
        let oid = gix::ObjectId::try_from(id)
            .map_err(|_| anyhow!("invalid git object id ({} bytes)", id.len()))?;
        let repo = self.git_backend.git_repo();
        Ok(repo.objects.exists(&oid))
    }

    // ---- Extras table (for RemoteStore replication) ----

    /// Write extras (change-id + predecessors) for a commit into the
    /// extras table. Used by the fetch layer to persist extras received
    /// from a remote.
    ///
    /// **Cache note:** This opens a fresh `TableStore`, bypassing
    /// `GitBackend`'s internal cached `ReadonlyTable`. A subsequent
    /// `read_commit` will miss that cache and fall through to
    /// `import_head_commits`, which re-reads the table from disk and
    /// finds the entry (no data loss). The unnecessary import walk is a
    /// performance cost; a future optimization would expose
    /// `GitBackend`'s `TableStore` or add `invalidate_extras_cache()`.
    pub fn write_extras(&self, commit_id: &[u8], extras_bytes: &[u8]) -> Result<()> {
        let extra_dir = self
            .git_backend
            .git_repo_path()
            .parent()
            .ok_or_else(|| anyhow!("git repo path has no parent"))?
            .join("extra");
        let table_store =
            jj_lib::stacked_table::TableStore::load(extra_dir, HASH_LENGTH);
        let (table, _lock) = table_store
            .get_head_locked()
            .map_err(|e| anyhow!("read extras table head: {e}"))?;
        let mut mut_table = table.start_mutation();
        mut_table.add_entry(commit_id.to_vec(), extras_bytes.to_vec());
        table_store
            .save_table(mut_table)
            .map_err(|e| anyhow!("save extras table: {e}"))?;
        Ok(())
    }

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
    ///
    /// Returns an error if any entry has an invalid name (empty or
    /// containing `/`). Entries are sorted by name before building the
    /// tree to satisfy `from_sorted_entries`'s invariant — proto message
    /// order is not guaranteed to be sorted.
    fn entries_to_jj_tree(&self, entries: &[GitTreeEntry]) -> Result<backend::Tree> {
        use jj_lib::backend::TreeValue;
        use jj_lib::repo_path::RepoPathComponentBuf;

        let mut jj_entries: Vec<(RepoPathComponentBuf, TreeValue)> = entries
            .iter()
            .map(|e| {
                let name = RepoPathComponentBuf::new(&e.name)
                    .map_err(|_| anyhow!("invalid tree entry name: {:?}", e.name))?;
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
                Ok((name, value))
            })
            .collect::<Result<Vec<_>>>()?;
        // Sort by name to satisfy from_sorted_entries's debug_assert.
        // Proto message order is not guaranteed to be sorted.
        jj_entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(backend::Tree::from_sorted_entries(jj_entries))
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
/// replication. Uses jj-lib's internal `git_store::Commit` proto
/// format so that the bytes written to the extras table are directly
/// consumable by `GitBackend::read_commit`.
fn serialize_extras_for_replication(commit: &backend::Commit) -> Vec<u8> {
    use prost::Message;

    let mut proto = jj_lib::protos::git_store::Commit {
        change_id: commit.change_id.to_bytes(),
        ..Default::default()
    };
    proto.uses_tree_conflict_format = true;
    for predecessor in &commit.predecessors {
        proto.predecessors.push(predecessor.to_bytes());
    }
    proto.encode_to_vec()
}

/// Check if a gix error is a "not found" error.
fn is_not_found(err: &gix::object::find::existing::Error) -> bool {
    matches!(
        err,
        gix::object::find::existing::Error::NotFound { .. }
    )
}

// ---- Proto ↔ git-store type conversions ----
//
// These replace the old ty::* ↔ proto conversions. The proto wire format
// is unchanged (same messages, same field semantics), but the daemon-side
// types are now GitTreeEntry/GitEntryKind (for trees) and
// backend::Commit (for commits, via jj-lib).

/// Convert a proto `Tree` message to a list of `GitTreeEntry`.
pub fn tree_from_proto(
    proto: proto::jj_interface::Tree,
) -> anyhow::Result<Vec<GitTreeEntry>> {
    proto
        .entries
        .into_iter()
        .map(|e| {
            let proto_val = e
                .value
                .ok_or_else(|| anyhow!("tree entry {:?} missing value oneof", e.name))?;
            let value = proto_val
                .value
                .ok_or_else(|| anyhow!("TreeValue missing value oneof for {:?}", e.name))?;
            use proto::jj_interface::tree_value::Value;
            let (kind, id) = match value {
                Value::TreeId(id) => (GitEntryKind::Tree, id),
                Value::SymlinkId(id) => (GitEntryKind::Symlink, id),
                Value::ConflictId(id) => {
                    // Conflicts are surfaced as opaque blobs on the wire.
                    // The git store doesn't have a native conflict type;
                    // treat as a non-executable file for storage.
                    (GitEntryKind::File { executable: false }, id)
                }
                Value::File(f) => (
                    GitEntryKind::File {
                        executable: f.executable,
                    },
                    f.id,
                ),
            };
            Ok(GitTreeEntry {
                name: e.name,
                kind,
                id,
            })
        })
        .collect()
}

/// Convert a list of `GitTreeEntry` to a proto `Tree` message.
pub fn tree_to_proto(entries: &[GitTreeEntry]) -> proto::jj_interface::Tree {
    let proto_entries = entries
        .iter()
        .map(|e| {
            let value = match e.kind {
                GitEntryKind::File { executable } => {
                    proto::jj_interface::tree_value::Value::File(
                        proto::jj_interface::tree_value::File {
                            id: e.id.clone(),
                            executable,
                            copy_id: Vec::new(),
                        },
                    )
                }
                GitEntryKind::Tree => {
                    proto::jj_interface::tree_value::Value::TreeId(e.id.clone())
                }
                GitEntryKind::Symlink => {
                    proto::jj_interface::tree_value::Value::SymlinkId(e.id.clone())
                }
                GitEntryKind::Submodule => {
                    // Lossy: submodule entries are mapped to TreeId on the
                    // wire. A round-trip through proto will come back as a
                    // tree, not a submodule. Acceptable because kiki doesn't
                    // create submodule entries — they only appear if a
                    // pre-existing repo contains them, and we surface them
                    // read-only.
                    proto::jj_interface::tree_value::Value::TreeId(e.id.clone())
                }
            };
            proto::jj_interface::tree::Entry {
                name: e.name.clone(),
                value: Some(proto::jj_interface::TreeValue {
                    value: Some(value),
                }),
            }
        })
        .collect();
    proto::jj_interface::Tree {
        entries: proto_entries,
    }
}

/// Convert a proto `Commit` message to a jj-lib `backend::Commit`.
pub fn commit_from_proto(
    proto: proto::jj_interface::Commit,
) -> anyhow::Result<backend::Commit> {
    use jj_lib::backend::{ChangeId, Signature, Timestamp};
    use jj_lib::merge::Merge;

    let parents = proto
        .parents
        .into_iter()
        .map(|p| backend::CommitId::from_bytes(&p))
        .collect();
    let predecessors = proto
        .predecessors
        .into_iter()
        .map(|p| backend::CommitId::from_bytes(&p))
        .collect();

    // root_tree: the proto carries a repeated bytes field. With
    // uses_tree_conflict_format, the entries alternate removes/adds
    // (Merge encoding). Without it, it's a single legacy tree id.
    let root_tree: Merge<TreeId> = if proto.uses_tree_conflict_format {
        let tree_ids: Vec<TreeId> = proto
            .root_tree
            .into_iter()
            .map(|b| TreeId::from_bytes(&b))
            .collect();
        anyhow::ensure!(
            tree_ids.len() % 2 != 0,
            "root_tree merge must have an odd number of terms, got {}",
            tree_ids.len()
        );
        Merge::from_vec(tree_ids)
    } else {
        let id = proto
            .root_tree
            .into_iter()
            .next()
            .unwrap_or_default();
        Merge::resolved(TreeId::from_bytes(&id))
    };

    let change_id = ChangeId::new(proto.change_id);

    let author = proto
        .author
        .map(signature_from_proto)
        .transpose()
        .context("author")?
        .unwrap_or_else(|| Signature {
            name: String::new(),
            email: String::new(),
            timestamp: Timestamp {
                timestamp: jj_lib::backend::MillisSinceEpoch(0),
                tz_offset: 0,
            },
        });

    let committer = proto
        .committer
        .map(signature_from_proto)
        .transpose()
        .context("committer")?
        .unwrap_or_else(|| Signature {
            name: String::new(),
            email: String::new(),
            timestamp: Timestamp {
                timestamp: jj_lib::backend::MillisSinceEpoch(0),
                tz_offset: 0,
            },
        });

    // conflict_labels: same Merge encoding as root_tree (alternating
    // removes/adds). Empty vec → resolved empty string (the default).
    let conflict_labels: Merge<String> = if proto.conflict_labels.is_empty() {
        Merge::resolved(String::new())
    } else {
        anyhow::ensure!(
            proto.conflict_labels.len() % 2 != 0,
            "conflict_labels merge must have an odd number of terms, got {}",
            proto.conflict_labels.len()
        );
        Merge::from_vec(proto.conflict_labels)
    };

    // secure_sig: the proto carries only the raw signature bytes.
    // The signed data is the git commit object itself, which the
    // caller can reconstruct from the git ODB when needed. We store
    // it with an empty `data` field — GitBackend::read_commit
    // repopulates `data` from the git object on read.
    let secure_sig = proto.secure_sig.map(|sig| backend::SecureSig {
        data: Vec::new(),
        sig,
    });

    Ok(backend::Commit {
        parents,
        predecessors,
        root_tree,
        conflict_labels,
        change_id,
        description: proto.description,
        author,
        committer,
        secure_sig,
    })
}

/// Convert a jj-lib `backend::Commit` to a proto `Commit` message.
pub fn commit_to_proto(commit: &backend::Commit) -> proto::jj_interface::Commit {
    use jj_lib::object_id::ObjectId;

    let root_tree: Vec<Vec<u8>> = commit
        .root_tree
        .iter()
        .map(|id| id.to_bytes())
        .collect();
    // A resolved merge has a single term — encode as the legacy format
    // (uses_tree_conflict_format = false) for maximal compat. Multi-term
    // merges set the flag so the reader knows the alternation pattern.
    let uses_tree_conflict_format = root_tree.len() != 1;

    // conflict_labels: same Merge encoding as root_tree. Omit when
    // resolved to empty string (the common case) for wire compat.
    let conflict_labels: Vec<String> = if commit.conflict_labels.is_resolved() {
        Vec::new()
    } else {
        commit.conflict_labels.iter().cloned().collect()
    };

    proto::jj_interface::Commit {
        parents: commit.parents.iter().map(|id| id.to_bytes()).collect(),
        predecessors: commit
            .predecessors
            .iter()
            .map(|id| id.to_bytes())
            .collect(),
        root_tree,
        conflict_labels,
        uses_tree_conflict_format,
        change_id: commit.change_id.to_bytes(),
        description: commit.description.clone(),
        author: Some(signature_to_proto(&commit.author)),
        committer: Some(signature_to_proto(&commit.committer)),
        secure_sig: commit.secure_sig.as_ref().map(|s| s.sig.clone()),
    }
}

fn signature_from_proto(
    proto: proto::jj_interface::commit::Signature,
) -> anyhow::Result<jj_lib::backend::Signature> {
    let ts = proto
        .timestamp
        .ok_or_else(|| anyhow!("Signature missing required timestamp"))?;
    Ok(jj_lib::backend::Signature {
        name: proto.name,
        email: proto.email,
        timestamp: jj_lib::backend::Timestamp {
            timestamp: jj_lib::backend::MillisSinceEpoch(ts.millis_since_epoch),
            tz_offset: ts.tz_offset,
        },
    })
}

fn signature_to_proto(
    sig: &jj_lib::backend::Signature,
) -> proto::jj_interface::commit::Signature {
    proto::jj_interface::commit::Signature {
        name: sig.name.clone(),
        email: sig.email.clone(),
        timestamp: Some(proto::jj_interface::commit::Timestamp {
            millis_since_epoch: sig.timestamp.timestamp.0,
            tz_offset: sig.timestamp.tz_offset,
        }),
    }
}

#[cfg(test)]
/// Well-known SHA-1 of the empty tree in git.
const EMPTY_TREE_ID: [u8; 20] = [
    0x4b, 0x82, 0x5d, 0xc6, 0x42, 0xcb, 0x6e, 0xb9, 0xa0, 0x60,
    0xe5, 0x4b, 0xf8, 0xd6, 0x92, 0x88, 0xfb, 0xee, 0x49, 0x04,
];

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

    // ---- write_extras / read_extras round-trip ----

    #[test]
    fn write_and_read_extras_round_trip() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);

        // Write a commit first (extras are keyed by commit id).
        let commit = make_test_commit(&settings, &[0xaa; 16]);
        let (commit_id, stored) = store.write_commit(commit).expect("write_commit");

        // Read extras back via the round-trip path.
        let extras = store.read_extras(&commit_id).expect("read_extras");
        assert!(extras.is_some(), "extras should be present for a written commit");

        // The extras should contain the change-id and predecessors.
        // Verify by deserializing with prost.
        use prost::Message;
        let decoded = jj_lib::protos::git_store::Commit::decode(
            extras.unwrap().as_slice(),
        )
        .expect("decode extras proto");
        assert_eq!(decoded.change_id, stored.change_id.to_bytes());
    }

    // ---- write_commit / read_commit direct round-trip ----

    #[test]
    fn write_and_read_commit_round_trip() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);

        let commit = make_test_commit(&settings, &[0xbb; 16]);
        let (commit_id, stored) = store.write_commit(commit).expect("write_commit");
        assert_eq!(commit_id.len(), 20);

        let read_back = store
            .read_commit(&commit_id)
            .expect("read_commit")
            .expect("commit should be present");

        // Core fields must match.
        assert_eq!(read_back.change_id, stored.change_id);
        assert_eq!(read_back.description, stored.description);
        assert_eq!(read_back.author.name, stored.author.name);
        assert_eq!(read_back.committer.email, stored.committer.email);
        assert_eq!(read_back.root_tree, stored.root_tree);
    }

    // ---- entries_to_jj_tree rejects invalid names ----

    #[test]
    fn entries_to_jj_tree_rejects_empty_name() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);
        let entries = vec![GitTreeEntry {
            name: "".into(),
            kind: GitEntryKind::File { executable: false },
            id: vec![0; 20],
        }];
        assert!(store.write_tree(&entries).is_err());
    }

    #[test]
    fn entries_to_jj_tree_rejects_slash_in_name() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);
        let entries = vec![GitTreeEntry {
            name: "a/b".into(),
            kind: GitEntryKind::File { executable: false },
            id: vec![0; 20],
        }];
        assert!(store.write_tree(&entries).is_err());
    }

    // ---- operation_ids_matching_prefix coverage ----

    #[test]
    fn operation_prefix_no_match() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);
        assert_eq!(
            store.operation_ids_matching_prefix("deadbeef").unwrap(),
            OpPrefixResult::None,
        );
    }

    #[test]
    fn operation_prefix_single_match() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);

        let id = [0xab; 64];
        store.write_operation_bytes(&id, b"op1").unwrap();
        let result = store.operation_ids_matching_prefix("ab").unwrap();
        assert_eq!(result, OpPrefixResult::Single(id.to_vec()));
    }

    #[test]
    fn operation_prefix_full_length_match() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);

        let id = [0xab; 64];
        store.write_operation_bytes(&id, b"op1").unwrap();
        let full_hex = hex_encode(&id);
        let result = store.operation_ids_matching_prefix(&full_hex).unwrap();
        assert_eq!(result, OpPrefixResult::Single(id.to_vec()));
    }

    #[test]
    fn operation_prefix_ambiguous() {
        let settings = test_settings();
        let store = GitContentStore::new_in_memory(&settings);

        // Two ids that share the same prefix.
        let mut id1 = [0xab; 64];
        let mut id2 = [0xab; 64];
        id1[63] = 0x01;
        id2[63] = 0x02;
        store.write_operation_bytes(&id1, b"op1").unwrap();
        store.write_operation_bytes(&id2, b"op2").unwrap();
        assert_eq!(
            store.operation_ids_matching_prefix("ab").unwrap(),
            OpPrefixResult::Ambiguous,
        );
    }

    /// Helper: build a minimal `backend::Commit` for testing.
    fn make_test_commit(
        settings: &UserSettings,
        change_id_bytes: &[u8],
    ) -> backend::Commit {
        use jj_lib::backend::{ChangeId, Signature, Timestamp};
        use jj_lib::merge::Merge;

        let _ = settings; // settings used for store, not commit construction
        backend::Commit {
            parents: vec![CommitId::from_bytes(&[0; 20])],
            predecessors: vec![],
            root_tree: Merge::resolved(TreeId::from_bytes(
                &EMPTY_TREE_ID,
            )),
            conflict_labels: Merge::resolved(String::new()),
            change_id: ChangeId::new(change_id_bytes.to_vec()),
            description: "test commit".to_string(),
            author: Signature {
                name: "Test".to_string(),
                email: "test@test.com".to_string(),
                timestamp: Timestamp {
                    timestamp: jj_lib::backend::MillisSinceEpoch(1_700_000_000_000),
                    tz_offset: 0,
                },
            },
            committer: Signature {
                name: "Test".to_string(),
                email: "test@test.com".to_string(),
                timestamp: Timestamp {
                    timestamp: jj_lib::backend::MillisSinceEpoch(1_700_000_000_000),
                    tz_offset: 0,
                },
            },
            secure_sig: None,
        }
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use jj_lib::backend::{self, ChangeId, CommitId, Signature, Timestamp, TreeId};
    use jj_lib::merge::Merge;
    use jj_lib::settings::UserSettings;
    use proptest::prelude::*;

    fn test_settings() -> UserSettings {
        let toml_str = r#"
            user.name = "Test User"
            user.email = "test@example.com"
            operation.hostname = "test"
            operation.username = "test"
            debug.randomness-seed = 42
        "#;
        let mut config = jj_lib::config::StackedConfig::with_defaults();
        config.add_layer(
            jj_lib::config::ConfigLayer::parse(
                jj_lib::config::ConfigSource::User,
                toml_str,
            )
            .unwrap(),
        );
        UserSettings::from_config(config).unwrap()
    }

    // ---- Strategies ----

    fn arb_20_bytes() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(any::<u8>(), 20)
    }

    /// Valid tree entry names: non-empty, no '/' or NUL.
    fn arb_entry_name() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_.][a-zA-Z0-9_.\\-]{0,15}"
    }

    fn arb_entry_kind() -> impl Strategy<Value = GitEntryKind> {
        prop_oneof![
            Just(GitEntryKind::File { executable: false }),
            Just(GitEntryKind::File { executable: true }),
            Just(GitEntryKind::Tree),
            Just(GitEntryKind::Symlink),
        ]
    }

    fn arb_tree_entry() -> impl Strategy<Value = GitTreeEntry> {
        (arb_entry_name(), arb_entry_kind(), arb_20_bytes()).prop_map(
            |(name, kind, id)| GitTreeEntry { name, kind, id },
        )
    }

    /// Strategy for a vec of tree entries with unique names (git trees
    /// don't allow duplicate names).
    fn arb_tree_entries() -> impl Strategy<Value = Vec<GitTreeEntry>> {
        proptest::collection::vec(arb_tree_entry(), 0..8).prop_map(|mut entries| {
            // Deduplicate by name, keeping the first occurrence.
            let mut seen = std::collections::HashSet::new();
            entries.retain(|e| seen.insert(e.name.clone()));
            entries
        })
    }

    fn arb_signature() -> impl Strategy<Value = Signature> {
        (
            "[a-zA-Z ]{1,20}",
            "[a-z]+@[a-z]+\\.[a-z]{2,4}",
            any::<i64>(),
            -720i32..720i32,
        )
            .prop_map(|(name, email, millis, tz)| Signature {
                name,
                email,
                timestamp: Timestamp {
                    timestamp: jj_lib::backend::MillisSinceEpoch(millis),
                    tz_offset: tz,
                },
            })
    }

    fn arb_commit() -> impl Strategy<Value = backend::Commit> {
        (
            proptest::collection::vec(arb_20_bytes(), 1..4), // parents
            proptest::collection::vec(arb_20_bytes(), 0..3), // predecessors
            arb_20_bytes(),                                  // root_tree id
            proptest::collection::vec(any::<u8>(), 16),      // change_id
            ".*",                                            // description
            arb_signature(),
            arb_signature(),
            proptest::option::of(proptest::collection::vec(any::<u8>(), 1..64)), // secure_sig
        )
            .prop_map(
                |(parents, predecessors, tree_id, change_id, description, author, committer, sig)| {
                    backend::Commit {
                        parents: parents.into_iter().map(|b| CommitId::from_bytes(&b)).collect(),
                        predecessors: predecessors
                            .into_iter()
                            .map(|b| CommitId::from_bytes(&b))
                            .collect(),
                        root_tree: Merge::resolved(TreeId::from_bytes(&tree_id)),
                        conflict_labels: Merge::resolved(String::new()),
                        change_id: ChangeId::new(change_id),
                        description,
                        author,
                        committer,
                        secure_sig: sig.map(|s| backend::SecureSig {
                            data: Vec::new(),
                            sig: s,
                        }),
                    }
                },
            )
    }

    // ---- tree_from_proto / tree_to_proto round-trip ----

    proptest! {
        /// Encoding tree entries to proto and decoding back preserves all
        /// fields except Submodule (which is lossy — see tree_to_proto doc).
        #[test]
        fn tree_proto_round_trip(entries in arb_tree_entries()) {
            let proto_tree = tree_to_proto(&entries);
            let decoded = tree_from_proto(proto_tree).expect("tree_from_proto");

            // Build expected: same entries but Submodule becomes Tree (lossy).
            let expected: Vec<GitTreeEntry> = entries
                .into_iter()
                .map(|mut e| {
                    if e.kind == GitEntryKind::Submodule {
                        e.kind = GitEntryKind::Tree;
                    }
                    e
                })
                .collect();
            prop_assert_eq!(decoded, expected);
        }
    }

    // ---- commit_from_proto / commit_to_proto round-trip ----

    proptest! {
        /// Proto round-trip for commits preserves core fields.
        #[test]
        fn commit_proto_round_trip(commit in arb_commit()) {
            let proto_commit = commit_to_proto(&commit);
            let decoded = commit_from_proto(proto_commit).expect("commit_from_proto");

            prop_assert_eq!(&decoded.parents, &commit.parents);
            prop_assert_eq!(&decoded.predecessors, &commit.predecessors);
            prop_assert_eq!(&decoded.root_tree, &commit.root_tree);
            prop_assert_eq!(&decoded.change_id, &commit.change_id);
            prop_assert_eq!(&decoded.description, &commit.description);
            prop_assert_eq!(&decoded.author, &commit.author);
            prop_assert_eq!(&decoded.committer, &commit.committer);

            // conflict_labels: resolved empty → resolved empty.
            prop_assert_eq!(&decoded.conflict_labels, &commit.conflict_labels);

            // secure_sig: round-trips the sig bytes (data is empty on
            // decode since it's reconstructed from the git object).
            match (&decoded.secure_sig, &commit.secure_sig) {
                (Some(d), Some(c)) => prop_assert_eq!(&d.sig, &c.sig),
                (None, None) => {}
                (d, c) => prop_assert!(false, "secure_sig mismatch: decoded={d:?}, original={c:?}"),
            }
        }
    }

    // ---- commit_to_proto conflict_labels round-trip ----

    proptest! {
        /// Non-trivial conflict labels (multi-term merge) survive proto
        /// round-trip.
        #[test]
        fn commit_proto_conflict_labels_round_trip(
            label_a in "[a-z]{1,8}",
            label_b in "[a-z]{1,8}",
            label_c in "[a-z]{1,8}",
        ) {
            // 3-way merge: [remove, add, add] → Merge::from_vec with 3 terms.
            let labels = Merge::from_vec(vec![label_a.clone(), label_b.clone(), label_c.clone()]);
            let tree_a = TreeId::from_bytes(&[1; 20]);
            let tree_b = TreeId::from_bytes(&[2; 20]);
            let tree_c = TreeId::from_bytes(&[3; 20]);
            let commit = backend::Commit {
                parents: vec![CommitId::from_bytes(&[0; 20])],
                predecessors: vec![],
                root_tree: Merge::from_vec(vec![tree_a, tree_b, tree_c]),
                conflict_labels: labels.clone(),
                change_id: ChangeId::new(vec![0xcc; 16]),
                description: String::new(),
                author: Signature {
                    name: "T".into(),
                    email: "t@t".into(),
                    timestamp: Timestamp {
                        timestamp: jj_lib::backend::MillisSinceEpoch(0),
                        tz_offset: 0,
                    },
                },
                committer: Signature {
                    name: "T".into(),
                    email: "t@t".into(),
                    timestamp: Timestamp {
                        timestamp: jj_lib::backend::MillisSinceEpoch(0),
                        tz_offset: 0,
                    },
                },
                secure_sig: None,
            };
            let proto_commit = commit_to_proto(&commit);
            let decoded = commit_from_proto(proto_commit).expect("commit_from_proto");
            prop_assert_eq!(&decoded.conflict_labels, &labels);
        }
    }

    // ---- serialize_extras_for_replication round-trip ----

    proptest! {
        /// Extras serialized for replication can be decoded back by jj-lib's
        /// internal proto format.
        #[test]
        fn serialize_extras_round_trip(
            change_id_bytes in proptest::collection::vec(any::<u8>(), 16),
            predecessor_ids in proptest::collection::vec(arb_20_bytes(), 0..4),
        ) {
            use prost::Message;

            let commit = backend::Commit {
                parents: vec![CommitId::from_bytes(&[0; 20])],
                predecessors: predecessor_ids
                    .iter()
                    .map(|b| CommitId::from_bytes(b))
                    .collect(),
                root_tree: Merge::resolved(TreeId::from_bytes(&[0; 20])),
                conflict_labels: Merge::resolved(String::new()),
                change_id: ChangeId::new(change_id_bytes.clone()),
                description: String::new(),
                author: Signature {
                    name: String::new(),
                    email: String::new(),
                    timestamp: Timestamp {
                        timestamp: jj_lib::backend::MillisSinceEpoch(0),
                        tz_offset: 0,
                    },
                },
                committer: Signature {
                    name: String::new(),
                    email: String::new(),
                    timestamp: Timestamp {
                        timestamp: jj_lib::backend::MillisSinceEpoch(0),
                        tz_offset: 0,
                    },
                },
                secure_sig: None,
            };

            let bytes = serialize_extras_for_replication(&commit);

            // Decode using the same proto type jj-lib uses internally.
            let decoded = jj_lib::protos::git_store::Commit::decode(bytes.as_slice())
                .expect("decode extras proto");

            prop_assert_eq!(&decoded.change_id, &change_id_bytes);
            prop_assert_eq!(decoded.predecessors.len(), predecessor_ids.len());
            for (got, expected) in decoded.predecessors.iter().zip(&predecessor_ids) {
                prop_assert_eq!(got, expected);
            }
            prop_assert!(decoded.uses_tree_conflict_format);
        }
    }

    // ---- write_extras / read_extras through store ----

    proptest! {
        /// Extras written directly and read back via read_extras preserve
        /// the change-id and predecessor set.
        #[test]
        fn write_read_extras_through_store(
            change_id_bytes in proptest::collection::vec(any::<u8>(), 16),
        ) {
            use prost::Message;

            let settings = test_settings();
            let store = GitContentStore::new_in_memory(&settings);

            // Build and write a commit.
            let commit = backend::Commit {
                parents: vec![CommitId::from_bytes(&[0; 20])],
                predecessors: vec![],
                root_tree: Merge::resolved(TreeId::from_bytes(
                    &EMPTY_TREE_ID,
                )),
                conflict_labels: Merge::resolved(String::new()),
                change_id: ChangeId::new(change_id_bytes.clone()),
                description: "prop test".into(),
                author: Signature {
                    name: "T".into(),
                    email: "t@t".into(),
                    timestamp: Timestamp {
                        timestamp: jj_lib::backend::MillisSinceEpoch(1_700_000_000_000),
                        tz_offset: 0,
                    },
                },
                committer: Signature {
                    name: "T".into(),
                    email: "t@t".into(),
                    timestamp: Timestamp {
                        timestamp: jj_lib::backend::MillisSinceEpoch(1_700_000_000_000),
                        tz_offset: 0,
                    },
                },
                secure_sig: None,
            };
            let (commit_id, _stored) = store.write_commit(commit).expect("write_commit");

            // Read extras.
            let extras_bytes = store
                .read_extras(&commit_id)
                .expect("read_extras")
                .expect("extras present");

            let decoded = jj_lib::protos::git_store::Commit::decode(extras_bytes.as_slice())
                .expect("decode extras");
            // The change_id from write_commit may differ from what we
            // passed in (GitBackend assigns one), but it must be non-empty.
            prop_assert!(!decoded.change_id.is_empty());
        }
    }

    // ---- write_commit / read_commit round-trip ----

    proptest! {
        /// Writing and reading a commit preserves key fields.
        /// Uses names/descriptions that survive git's whitespace
        /// normalization (no leading/trailing whitespace, no bare
        /// whitespace-only strings).
        #[test]
        fn write_read_commit_round_trip(
            description in "[a-zA-Z0-9 ]{0,50}",
            author_name in "[a-zA-Z][a-zA-Z ]{0,18}[a-zA-Z]",
            author_email in "[a-z]+@[a-z]+\\.[a-z]{2,4}",
        ) {
            let settings = test_settings();
            let store = GitContentStore::new_in_memory(&settings);

            let commit = backend::Commit {
                parents: vec![CommitId::from_bytes(&[0; 20])],
                predecessors: vec![],
                root_tree: Merge::resolved(TreeId::from_bytes(
                    &EMPTY_TREE_ID,
                )),
                conflict_labels: Merge::resolved(String::new()),
                change_id: ChangeId::new(vec![0xdd; 16]),
                description: description.clone(),
                author: Signature {
                    name: author_name.clone(),
                    email: author_email.clone(),
                    timestamp: Timestamp {
                        timestamp: jj_lib::backend::MillisSinceEpoch(1_700_000_000_000),
                        tz_offset: 60,
                    },
                },
                committer: Signature {
                    name: "Committer".into(),
                    email: "c@c.com".into(),
                    timestamp: Timestamp {
                        timestamp: jj_lib::backend::MillisSinceEpoch(1_700_000_000_000),
                        tz_offset: -120,
                    },
                },
                secure_sig: None,
            };

            let (commit_id, stored) = store.write_commit(commit).expect("write_commit");
            let read_back = store
                .read_commit(&commit_id)
                .expect("read_commit")
                .expect("present");

            prop_assert_eq!(&read_back.description, &stored.description);
            prop_assert_eq!(&read_back.author.name, &stored.author.name);
            prop_assert_eq!(&read_back.author.email, &stored.author.email);
            prop_assert_eq!(&read_back.root_tree, &stored.root_tree);
        }
    }

    // ---- commit_from_proto rejects even-length merge vecs ----

    /// Regression: an even-length `root_tree` repeated field triggers
    /// `Merge::from_vec`'s assert (`values.len() % 2 != 0`), panicking
    /// the daemon. `commit_from_proto` must return `Err`, not panic.
    #[test]
    fn commit_from_proto_rejects_even_length_root_tree() {
        let proto = proto::jj_interface::Commit {
            parents: vec![vec![0; 20]],
            root_tree: vec![vec![1; 20], vec![2; 20]], // even: 2 entries
            uses_tree_conflict_format: true,
            change_id: vec![0xaa; 16],
            ..Default::default()
        };
        let result = commit_from_proto(proto);
        assert!(
            result.is_err(),
            "even-length root_tree should be rejected, not panic"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("odd"),
            "error should mention odd-number requirement, got: {msg}"
        );
    }

    /// Same regression for `conflict_labels`: even-length vec must be
    /// rejected gracefully.
    #[test]
    fn commit_from_proto_rejects_even_length_conflict_labels() {
        let proto = proto::jj_interface::Commit {
            parents: vec![vec![0; 20]],
            root_tree: vec![vec![1; 20]], // valid: single resolved tree
            uses_tree_conflict_format: false,
            conflict_labels: vec!["a".into(), "b".into()], // even: 2 entries
            change_id: vec![0xbb; 16],
            ..Default::default()
        };
        let result = commit_from_proto(proto);
        assert!(
            result.is_err(),
            "even-length conflict_labels should be rejected, not panic"
        );
    }

    proptest! {
        /// Property: any even-length root_tree vec must be rejected.
        #[test]
        fn commit_from_proto_even_root_tree_never_panics(
            n in 1usize..5,
        ) {
            let even_count = n * 2;
            let tree_ids: Vec<Vec<u8>> = (0..even_count)
                .map(|i| vec![i as u8; 20])
                .collect();
            let proto = proto::jj_interface::Commit {
                parents: vec![vec![0; 20]],
                root_tree: tree_ids,
                uses_tree_conflict_format: true,
                change_id: vec![0xcc; 16],
                ..Default::default()
            };
            prop_assert!(commit_from_proto(proto).is_err());
        }
    }

    // ---- operation_ids_matching_prefix property ----

    proptest! {
        /// Writing N distinct operations and querying by a shared prefix
        /// always returns Ambiguous when N > 1 and the prefix is shared.
        #[test]
        fn operation_prefix_property(
            suffix_a in any::<u8>(),
            suffix_b in any::<u8>(),
        ) {
            prop_assume!(suffix_a != suffix_b);

            let settings = test_settings();
            let store = GitContentStore::new_in_memory(&settings);

            let mut id_a = [0xcd; 64];
            let mut id_b = [0xcd; 64];
            id_a[63] = suffix_a;
            id_b[63] = suffix_b;

            store.write_operation_bytes(&id_a, b"a").unwrap();
            store.write_operation_bytes(&id_b, b"b").unwrap();

            // The shared prefix "cd" matches both.
            prop_assert_eq!(
                store.operation_ids_matching_prefix("cd").unwrap(),
                OpPrefixResult::Ambiguous,
            );

            // Full hex of id_a matches exactly one.
            let full_hex_a = hex_encode(&id_a);
            prop_assert_eq!(
                store.operation_ids_matching_prefix(&full_hex_a).unwrap(),
                OpPrefixResult::Single(id_a.to_vec()),
            );
        }
    }
}
