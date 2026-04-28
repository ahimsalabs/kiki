use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use prost::Message as _;
use redb::{Database, ReadableTable, TableDefinition};

use crate::ty::*;

// Per-mount KV schema. Keys are 32-byte content-hash `Id` bytes; values
// are prost-encoded representations of the corresponding wire types.
//
// The `_v1` suffix is intentional: when on-disk encoding changes (proto
// schema break, key derivation change), bump the suffix and add a
// migration step rather than reusing a name. Until then, `redb` only
// surfaces the table if and when callers `open_table` it; absent tables
// are not an error on read.
const COMMITS: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("commits_v1");
const FILES: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("files_v1");
const SYMLINKS: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("symlinks_v1");
const TREES: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("trees_v1");

/// Stores mount-agnostic information like Trees or Commits. Unaware of
/// filesystem information.
///
/// Backed by a [`redb::Database`] (Layer B, PLAN.md §8). Cloning a `Store`
/// only clones the `Arc<Database>`; redb itself is internally synchronized
/// (one writer + many readers per process).
///
/// Methods are sync (`Result`) rather than async: every body just opens
/// a redb transaction (no `.await` points). Keeping them async would
/// force every caller into an async context — most painfully, the
/// recursive `JjYakFs::snapshot` walk in `vfs/yak_fs.rs`, which would
/// otherwise need `async-recursion` or `Box::pin` to satisfy the
/// borrow checker.
#[derive(Clone, Debug)]
pub struct Store {
    db: Arc<Database>,
    empty_tree_id: Id,
}

impl Store {
    /// Open or create a redb-backed Store at `path`. Parent directories
    /// are created if missing. The empty tree is seeded on first open.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("creating store directory {}", parent.display())
            })?;
        }
        let db = Database::create(path)
            .with_context(|| format!("opening redb store at {}", path.display()))?;
        Self::from_database(db)
    }

    /// Test-only constructor: open a Store backed by an in-memory redb.
    /// Loses everything when dropped, but otherwise behaves identically
    /// to a file-backed Store.
    pub fn new_in_memory() -> Self {
        let db = redb::Builder::new()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .expect("redb in-memory backend cannot fail to open");
        Self::from_database(db).expect("redb in-memory store seed cannot fail")
    }

    fn from_database(db: Database) -> Result<Self> {
        let empty_tree = Tree::default();
        let empty_tree_id = empty_tree.get_hash();
        let empty_proto = empty_tree.as_proto().encode_to_vec();

        // Seed the empty tree if it isn't there yet. Also forces the
        // four tables to exist so subsequent read txns don't see
        // `TableError::TableDoesNotExist` for an empty store.
        let txn = db.begin_write().context("redb begin_write")?;
        {
            let mut commits = txn.open_table(COMMITS).context("open commits table")?;
            let mut files = txn.open_table(FILES).context("open files table")?;
            let mut symlinks = txn
                .open_table(SYMLINKS)
                .context("open symlinks table")?;
            let mut trees = txn.open_table(TREES).context("open trees table")?;
            if trees
                .get(&empty_tree_id.0)
                .context("read empty tree")?
                .is_none()
            {
                trees
                    .insert(&empty_tree_id.0, empty_proto.as_slice())
                    .context("seed empty tree")?;
            }
            // Suppress "unused" warnings — the open_table calls above have
            // the side effect of materializing each table.
            let _ = (&mut commits, &mut files, &mut symlinks);
        }
        txn.commit().context("redb commit (seed empty tree)")?;

        Ok(Store {
            db: Arc::new(db),
            empty_tree_id,
        })
    }

    pub fn get_empty_tree_id(&self) -> Id {
        self.empty_tree_id
    }

    pub fn get_tree(&self, id: Id) -> Result<Option<Tree>> {
        self.read_value(TREES, id, |bytes| {
            let proto = proto::jj_interface::Tree::decode(bytes)
                .context("decoding stored tree proto")?;
            Tree::try_from(proto).context("converting stored tree proto")
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn write_tree(&self, tree: Tree) -> Result<Id> {
        let hash = tree.get_hash();
        let bytes = tree.as_proto().encode_to_vec();
        self.write_value(TREES, hash, &bytes)?;
        Ok(hash)
    }

    pub fn get_file(&self, id: Id) -> Result<Option<File>> {
        self.read_value(FILES, id, |bytes| {
            let proto = proto::jj_interface::File::decode(bytes)
                .context("decoding stored file proto")?;
            Ok(File::from(proto))
        })
    }

    #[tracing::instrument(skip(self, file), fields(len = file.content.len()))]
    pub fn write_file(&self, file: File) -> Result<Id> {
        let hash = file.get_hash();
        let bytes = file.as_proto().encode_to_vec();
        self.write_value(FILES, hash, &bytes)?;
        Ok(hash)
    }

    pub fn get_symlink(&self, id: Id) -> Result<Option<Symlink>> {
        self.read_value(SYMLINKS, id, |bytes| {
            let proto = proto::jj_interface::Symlink::decode(bytes)
                .context("decoding stored symlink proto")?;
            Ok(Symlink::from(proto))
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn write_symlink(&self, symlink: Symlink) -> Result<Id> {
        let hash = symlink.get_hash();
        let bytes = symlink.as_proto().encode_to_vec();
        self.write_value(SYMLINKS, hash, &bytes)?;
        Ok(hash)
    }

    pub fn get_commit(&self, id: Id) -> Result<Option<Commit>> {
        self.read_value(COMMITS, id, |bytes| {
            let proto = proto::jj_interface::Commit::decode(bytes)
                .context("decoding stored commit proto")?;
            Commit::try_from(proto).context("converting stored commit proto")
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn write_commit(&self, commit: Commit) -> Result<Id> {
        let hash = commit.get_hash();
        let bytes = commit.as_proto().encode_to_vec();
        self.write_value(COMMITS, hash, &bytes)?;
        Ok(hash)
    }

    fn read_value<T>(
        &self,
        table: TableDefinition<'_, &'static [u8; 32], &'static [u8]>,
        id: Id,
        decode: impl FnOnce(&[u8]) -> Result<T>,
    ) -> Result<Option<T>> {
        let txn = self.db.begin_read().context("redb begin_read")?;
        let tbl = txn.open_table(table).context("open table for read")?;
        let raw = tbl.get(&id.0).context("redb get")?;
        match raw {
            Some(slot) => decode(slot.value()).map(Some),
            None => Ok(None),
        }
    }

    fn write_value(
        &self,
        table: TableDefinition<'_, &'static [u8; 32], &'static [u8]>,
        id: Id,
        bytes: &[u8],
    ) -> Result<()> {
        let txn = self.db.begin_write().context("redb begin_write")?;
        {
            let mut tbl = txn.open_table(table).context("open table for write")?;
            tbl.insert(&id.0, bytes).context("redb insert")?;
        }
        txn.commit().context("redb commit")?;
        Ok(())
    }
}

/// Test-only extension methods that `.expect()` away the `Result`
/// wrapper from every Store call. Lets test code keep reading like the
/// pre-M8 infallible API. Production code must use the fallible base
/// methods directly so real I/O failures aren't swallowed.
#[cfg(test)]
pub trait StoreTestExt {
    fn read_tree(&self, id: Id) -> Tree;
    fn read_file(&self, id: Id) -> File;
    fn read_symlink(&self, id: Id) -> Symlink;
    fn put_tree(&self, tree: Tree) -> Id;
    fn put_file(&self, file: File) -> Id;
    fn put_symlink(&self, symlink: Symlink) -> Id;
}

#[cfg(test)]
impl StoreTestExt for Store {
    fn read_tree(&self, id: Id) -> Tree {
        self.get_tree(id)
            .expect("get_tree")
            .expect("tree present in store")
    }
    fn read_file(&self, id: Id) -> File {
        self.get_file(id)
            .expect("get_file")
            .expect("file present in store")
    }
    fn read_symlink(&self, id: Id) -> Symlink {
        self.get_symlink(id)
            .expect("get_symlink")
            .expect("symlink present in store")
    }
    fn put_tree(&self, tree: Tree) -> Id {
        self.write_tree(tree).expect("write_tree")
    }
    fn put_file(&self, file: File) -> Id {
        self.write_file(file).expect("write_file")
    }
    fn put_symlink(&self, symlink: Symlink) -> Id {
        self.write_symlink(symlink).expect("write_symlink")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_then_get_empty_tree() {
        let store = Store::new_in_memory();
        let empty_id = store.get_empty_tree_id();
        let tree = store
            .get_tree(empty_id)
            .expect("get_tree")
            .expect("empty tree seeded");
        assert!(tree.entries.is_empty(), "seeded empty tree has no entries");
    }

    #[test]
    fn write_then_read_file_roundtrip() {
        let store = Store::new_in_memory();
        let file = File {
            content: b"hello world".to_vec(),
        };
        let id = store.write_file(file.clone()).expect("write_file");
        let got = store
            .get_file(id)
            .expect("get_file")
            .expect("file present");
        assert_eq!(got.content, file.content);
    }

    #[test]
    fn missing_tree_returns_none() {
        let store = Store::new_in_memory();
        let bogus = Id([0xff; 32]);
        let got = store.get_tree(bogus).expect("get_tree (missing)");
        assert!(got.is_none(), "non-existent tree should be None");
    }

    #[test]
    fn open_persists_across_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("store.redb");

        let written_id = {
            let store = Store::open(&path).expect("open #1");
            store
                .write_file(File {
                    content: b"persistent".to_vec(),
                })
                .expect("write_file")
        };

        let store2 = Store::open(&path).expect("open #2");
        let got = store2
            .get_file(written_id)
            .expect("get_file #2")
            .expect("file persisted");
        assert_eq!(got.content, b"persistent");
    }
}
