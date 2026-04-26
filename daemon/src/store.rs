use std::{collections::HashMap, sync::Arc};

use parking_lot::Mutex;

use crate::ty::*;

/// Stores mount-agnostic information like Trees or Commits. Unaware of filesystem information.
///
/// Fields are private; access goes through `get_*`/`write_*` methods
/// so the in-memory `HashMap` backing can be swapped for `redb` / `sled`
/// in Layer B without rewriting every call site.
#[derive(Clone, Debug)]
pub struct Store {
    commits: Arc<Mutex<HashMap<Id, Commit>>>,
    files: Arc<Mutex<HashMap<Id, File>>>,
    symlinks: Arc<Mutex<HashMap<Id, Symlink>>>,
    trees: Arc<Mutex<HashMap<Id, Tree>>>,
    empty_tree_id: Id,
}

impl Store {
    pub fn new() -> Self {
        let commits = Arc::new(Mutex::new(HashMap::new()));
        let files = Arc::new(Mutex::new(HashMap::new()));
        let symlinks = Arc::new(Mutex::new(HashMap::new()));

        let (empty_tree_id, trees) = {
            let mut trees = HashMap::new();
            let tree = Tree::default();
            let empty_tree_id: Id = tree.get_hash();
            trees.insert(empty_tree_id, tree);
            (empty_tree_id, Arc::new(Mutex::new(trees)))
        };

        Store {
            commits,
            trees,
            files,
            symlinks,
            empty_tree_id,
        }
    }

    pub fn get_empty_tree_id(&self) -> Id {
        self.empty_tree_id
    }

    pub fn get_tree(&self, id: Id) -> Option<Tree> {
        let tree_store = self.trees.lock();
        tree_store.get(&id).cloned()
    }

    // Store methods are sync rather than async: every body just locks an
    // in-memory `parking_lot::Mutex` (no I/O, no real await points). Keeping
    // them async would force every caller into an async context — most
    // painfully, the recursive `JjYakFs::snapshot` walk in `yak_fs.rs`,
    // which would otherwise need `async-recursion` or `Box::pin` to
    // satisfy the borrow checker. Layer B (durable storage) may add real
    // I/O here; switch back to async if/when that lands.
    #[tracing::instrument]
    pub fn write_tree(&self, tree: Tree) -> Id {
        let mut tree_store = self.trees.lock();
        let hash = tree.get_hash();
        tree_store.insert(hash, tree);
        hash
    }

    pub fn get_file(&self, id: Id) -> Option<File> {
        let file_store = self.files.lock();
        file_store.get(&id).cloned()
    }

    pub fn get_commit(&self, id: Id) -> Option<Commit> {
        let commit_store = self.commits.lock();
        commit_store.get(&id).cloned()
    }

    #[tracing::instrument]
    pub fn write_commit(&self, commit: Commit) -> Id {
        let mut commit_store = self.commits.lock();
        let hash = commit.get_hash();
        commit_store.insert(hash, commit);
        hash
    }

    #[tracing::instrument]
    pub fn write_file(&self, file: File) -> Id {
        let mut file_store = self.files.lock();
        let hash = file.get_hash();
        file_store.insert(hash, file);
        hash
    }

    pub fn get_symlink(&self, id: Id) -> Option<Symlink> {
        let symlink_store = self.symlinks.lock();
        symlink_store.get(&id).cloned()
    }

    #[tracing::instrument]
    pub fn write_symlink(&self, symlink: Symlink) -> Id {
        let mut symlink_store = self.symlinks.lock();
        let hash = symlink.get_hash();
        symlink_store.insert(hash, symlink);
        hash
    }
}
