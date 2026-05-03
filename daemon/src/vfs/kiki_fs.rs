//! `JjKikiFs` — the read+write trait the per-mount filesystem exposes — and
//! `KikiFs`, its concrete implementation backed by [`crate::git_store::GitContentStore`].
//!
//! The trait exists so the NFS and FUSE adapters can share a single
//! tree-walking codebase: `daemon/src/vfs/{nfs_adapter,fuse_adapter}.rs`
//! both wrap an `Arc<dyn JjKikiFs>` and translate between the wire
//! protocol's reply types and the domain types defined here.
//!
//! ## Read path (M3) and check-out (M5)
//!
//! `lookup` / `getattr` / `read` / `readdir` / `readlink` walk the
//! per-mount [`GitContentStore`] starting from the inode the kernel passed in.
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
//! into the per-mount [`GitContentStore`], and returns the rolled-up root tree id.
//! It also "cleans" the slab by replacing the now-persisted dirty refs
//! with their content-addressed ids, so kernel reads after snapshot
//! continue to work and memory doesn't accumulate stale buffers.
//!
//! Errors are converted to wire types in the adapters (`fs_err_to_nfs`,
//! `fs_err_to_errno`) so the same domain code maps to both protocols
//! without duplicating the match arms.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use jj_lib::gitignore::GitIgnoreFile;
use parking_lot::Mutex;

use crate::{
    git_store::{GitContentStore, GitEntryKind, GitTreeEntry},
    remote::{
        RemoteStore,
        fetch::{self, FetchError},
    },
    ty::Id,
    vfs::inode::{Inode, InodeId, InodeSlab, NodeRef, ROOT_INODE},
};

/// Reserved name for jj's metadata directory at the root of the working
/// copy. Pinned by `KikiFs` outside the content-addressed user tree:
/// created on first access, preserved across `check_out`, and excluded
/// from `snapshot`'s rollup. See `KikiFs::jj_subtree`.
const JJ_DIR: &str = ".jj";

/// Synthesized read-only `.git` gitdir file at the workspace root.
/// Contains `gitdir: <path>\n` so that stock git tools work against
/// the mount. Pinned outside the content-addressed tree, same as `.jj/`.
const GIT_FILE: &str = ".git";

const GITIGNORE_FILE: &str = ".gitignore";

/// Repo-root config listing directories to redirect to local scratch
/// storage (one path per line, repo-root-relative). Redirected dirs
/// become symlinks to a per-mount scratch directory and bypass both
/// the VFS hot path and the snapshot.
const KIKI_REDIRECTIONS_FILE: &str = ".kiki-redirections";

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
    /// Only meaningful for `Regular`. Mirrors `GitEntryKind::File.executable`.
    pub executable: bool,
    /// Modification time as seconds since the Unix epoch. Derived from the
    /// committer timestamp of the checked-out commit. 0 means unknown.
    pub mtime_secs: i64,
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
///
/// Not `Copy` because `StoreError` carries a String description — the
/// Layer-B redb store can fail with multiple distinct messages, and
/// surfacing the underlying chain through tracing is more useful than
/// collapsing every backend failure into a sentinel.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Operation not permitted on this target (e.g. mutation of a
    /// RootFs synthetic directory). Maps to EACCES / NFS3ERR_ACCES.
    PermissionDenied,
    /// Rename across mount/workspace boundaries. Maps to EXDEV /
    /// NFS3ERR_XDEV.
    CrossDevice,
    /// A tree, file, or symlink id present in an inode is missing from
    /// the store. With the GitContentStore this only happens after a
    /// check_out into a tree whose blobs aren't reachable — either a
    /// remote pull failure or an out-of-band db mutation. Adapters map
    /// it to EIO/NFS3ERR_IO.
    StoreMiss,
    /// The store returned an I/O error (git ODB failure, disk full,
    /// etc.). Stringified at the boundary so `FsError: PartialEq + Eq`
    /// still works for tests.
    StoreError(String),
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsError::NotFound => f.write_str("inode or directory entry not found"),
            FsError::NotADirectory => f.write_str("not a directory"),
            FsError::NotAFile => f.write_str("not a regular file"),
            FsError::NotASymlink => f.write_str("not a symlink"),
            FsError::AlreadyExists => f.write_str("entry already exists"),
            FsError::NotEmpty => f.write_str("directory not empty"),
            FsError::PermissionDenied => f.write_str("permission denied"),
            FsError::CrossDevice => f.write_str("cross-device operation"),
            FsError::StoreMiss => f.write_str("missing entry in content store"),
            FsError::StoreError(msg) => write!(f, "store error: {msg}"),
        }
    }
}

impl std::error::Error for FsError {}

/// Wrap an `anyhow::Error` from the store layer as an `FsError` suitable
/// for adapter return paths. Uses the chained formatter so the root cause
/// (e.g. "git ODB: ...") survives the conversion.
fn store_err(e: anyhow::Error) -> FsError {
    FsError::StoreError(format!("{e:#}"))
}

/// Map a [`FetchError`] (from M10 §10.6 read-through) onto an `FsError`.
/// `NotFound` becomes `StoreMiss` so the adapters' `EIO` mapping fires
/// the same way it would for a local-store miss with no remote. Every
/// other variant — DataLoss, decode failure, transport error — collapses
/// to `StoreError(...)` so the chain surfaces in tracing without
/// exposing fetch-specific variants to the wire layer (the kernel only
/// sees `EIO` either way).
fn fetch_err(e: FetchError) -> FsError {
    match e {
        FetchError::NotFound { .. } => FsError::StoreMiss,
        other => FsError::StoreError(format!("{other:#}")),
    }
}

#[async_trait]
pub trait JjKikiFs: Send + Sync + std::fmt::Debug {
    /// Inode id for the root directory. Always `ROOT_INODE` for the
    /// default impl; exposed on the trait so adapters needn't import the
    /// constant directly.
    fn root(&self) -> InodeId {
        ROOT_INODE
    }

    /// Resolve `name` within `parent`. On success the returned inode id
    /// is stable for the lifetime of this `JjKikiFs` instance.
    async fn lookup(&self, parent: InodeId, name: &str) -> Result<InodeId, FsError>;

    async fn getattr(&self, ino: InodeId) -> Result<Attr, FsError>;

    /// Read up to `count` bytes starting at `offset`. Returns `(data, eof)`,
    /// where `eof` is true when the read consumed the rest of the file
    /// (matching `nfsserve::vfs::NFSFileSystem::read`'s contract).
    async fn read(&self, ino: InodeId, offset: u64, count: u32)
    -> Result<(Vec<u8>, bool), FsError>;

    /// Full child listing of a directory. Adapters paginate as required
    /// by their wire protocol; `JjKikiFs` always returns everything in
    /// one shot since per-mount trees are small (a workspace, not a
    /// crawl target).
    async fn readdir(&self, dir: InodeId) -> Result<Vec<DirEntry>, FsError>;

    async fn readlink(&self, ino: InodeId) -> Result<String, FsError>;

    /// Re-root the filesystem at `new_root_tree`. Subsequent reads through
    /// this `JjKikiFs` (and through any kernel mount the adapters expose)
    /// see the new tree.
    ///
    /// `new_root_tree` must already be present in the backing store —
    /// returns `FsError::StoreMiss` otherwise. The daemon's `CheckOut`
    /// RPC handler turns that into `failed_precondition`.
    ///
    /// `commit_mtime_secs`: committer timestamp (seconds since epoch) of
    /// the commit being checked out. Used as mtime/ctime for all files.
    /// Pass 0 to leave the previous value unchanged.
    async fn check_out(&self, new_root_tree: Id, commit_mtime_secs: i64) -> Result<(), FsError>;

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
    /// `jj kk init` fails halfway through populating `.jj/`.
    async fn rename(
        &self,
        parent: InodeId,
        name: &str,
        new_parent: InodeId,
        new_name: &str,
    ) -> Result<(), FsError>;

    /// Walk the slab, persisting every dirty blob into the [`GitContentStore`],
    /// and return the rolled-up root tree id. After a successful `snapshot`,
    /// every previously-dirty inode is replaced with its clean content-
    /// addressed counterpart so the slab doesn't accumulate stale buffers
    /// — but inode ids are preserved so the kernel doesn't see them
    /// change.
    ///
    /// Returns the new root tree id on success.
    async fn snapshot(&self) -> Result<Id, FsError>;
}

/// Concrete `JjKikiFs` backed by a [`GitContentStore`] and the inode slab.
///
/// ## Pinned `.jj/` subtree (M7)
///
/// `.jj/` lives outside the content-addressed user tree managed by the
/// slab's root. The first `mkdir(root, ".jj")` allocates an inode and
/// stashes it in [`jj_subtree`](Self::jj_subtree); subsequent
/// `lookup`/`readdir`/etc. at the root short-circuit through that
/// pointer rather than going through the slab's regular root-children
/// map. `snapshot` walks root's children excluding `.jj`, so the tree
/// id returned to jj-lib never contains daemon-managed metadata.
/// `check_out` runs `swap_root` to re-root the user tree but leaves
/// `jj_subtree` untouched, so daemon-managed state survives a check-out.
///
/// This trades one special-case branch in each per-root op for not
/// having to thread a separate keyspace through the whole adapter
/// stack — the right shape until Layer C makes the storage location of
/// `.jj/` matter (PLAN §10.1 option (a) → (b) migration).
#[derive(Debug)]
pub struct KikiFs {
    store: Arc<GitContentStore>,
    /// M10 §10.6: lazy remote read-through. When set, a local
    /// [`GitContentStore`] miss in [`Self::read_tree`] / [`Self::read_file`] /
    /// [`Self::read_symlink`] falls through to `RemoteStore::get_blob`
    /// and persists the result into `store` so subsequent accesses
    /// hit the cache. `None` preserves pre-M10 behavior (miss surfaces
    /// as `EIO`).
    remote: Option<Arc<dyn RemoteStore>>,
    slab: InodeSlab,
    /// Inode id of the pinned `.jj/` subtree, if jj-lib has created one.
    /// `None` until the first `mkdir(root, ".jj")`. See type-level docs
    /// for why this is separate from the slab's root children.
    jj_subtree: Mutex<Option<InodeId>>,
    /// Inode id of the synthesized `.git` gitdir file. Populated at
    /// construction time. Contains `gitdir: <path>\n` and is read-only.
    git_file: InodeId,
    /// Pre-computed content of the `.git` gitdir file. Kept here for
    /// potential future use (e.g. fast-path reads bypassing the slab);
    /// the authoritative copy lives in the slab as a `DirtyFile`.
    #[allow(dead_code)]
    git_file_content: Vec<u8>,
    /// M10.7: gitignore rules loaded from `.gitignore` files in the
    /// content tree. Updated at `check_out` time and on `.gitignore`
    /// writes (hot-reload). Consulted by `create_file`/`mkdir`/`symlink`
    /// to tag new inodes as ignored.
    ignore_rules: Mutex<Arc<GitIgnoreFile>>,
    /// M10.7: repo-root-relative paths configured as redirections (from
    /// `.kiki-redirections`). `mkdir` for a redirected path creates a
    /// symlink to a scratch directory on real local disk instead of a
    /// `DirtyTree`, bypassing FUSE for all I/O inside.
    redirections: Mutex<Vec<String>>,
    /// Per-mount scratch directory for redirections. Created under
    /// `<storage_dir>/scratch/`. `None` disables redirections (tests).
    scratch_dir: Option<std::path::PathBuf>,
    /// Committer timestamp (seconds since epoch) of the currently checked-out
    /// commit. Used as `mtime`/`ctime` for all files. Updated atomically by
    /// `check_out`. 0 means unknown (epoch).
    commit_mtime: std::sync::atomic::AtomicI64,
}

impl KikiFs {
    /// Build a new mount-side filesystem rooted at `root_tree`.
    ///
    /// The root tree must already be in `store` — `GitContentStore::read_tree`
    /// is called lazily on the first `lookup`/`readdir`. Constructing with
    /// the store's empty tree id (the M1 default for a fresh mount) is
    /// the common case and yields an empty directory.
    ///
    /// `remote = Some(...)` enables M10 §10.6 lazy read-through: a
    /// local-store miss in any of the read paths falls through to the
    /// remote, persists the fetched blob into `store`, and returns
    /// the typed value. `remote = None` preserves pre-M10 behavior —
    /// a miss surfaces as `FsError::StoreMiss` and the kernel sees
    /// `EIO`.
    pub fn new(
        store: Arc<GitContentStore>,
        root_tree: Id,
        remote: Option<Arc<dyn RemoteStore>>,
        scratch_dir: Option<std::path::PathBuf>,
        worktree_gitdir: Option<std::path::PathBuf>,
        commit_mtime_secs: i64,
    ) -> Self {
        let slab = InodeSlab::new(root_tree);
        // If a per-workspace worktree gitdir is provided, the `.git`
        // file points there (own HEAD + index). Otherwise, fall back
        // to the shared bare repo (legacy/ad-hoc mounts).
        let gitdir_target = worktree_gitdir
            .as_deref()
            .unwrap_or_else(|| store.git_repo_path());
        let git_content =
            format!("gitdir: {}\n", gitdir_target.display()).into_bytes();
        let git_file = slab.alloc(
            ROOT_INODE,
            GIT_FILE.to_owned(),
            NodeRef::DirtyFile {
                content: git_content.clone(),
                executable: false,
            },
        );
        // Load .gitignore and .kiki-redirections from the initial tree.
        let ignore_rules = Self::load_gitignore_from_store(&store, &root_tree);
        let redirections = Self::load_redirections_from_store(&store, &root_tree);
        // NOTE: do NOT call slab.attach_child — pinned like .jj/
        KikiFs {
            store,
            remote,
            slab,
            jj_subtree: Mutex::new(None),
            git_file,
            git_file_content: git_content,
            ignore_rules: Mutex::new(ignore_rules),
            redirections: Mutex::new(redirections),
            scratch_dir,
            commit_mtime: std::sync::atomic::AtomicI64::new(commit_mtime_secs),
        }
    }

    /// Load `.gitignore` rules from the root of the content tree.
    ///
    /// Reads only the root-level `.gitignore` — nested `.gitignore`
    /// files are picked up via the hot-reload path when they are
    /// written through the VFS, or can be extended here for known
    /// subdirectories.
    fn load_gitignore_from_store(
        store: &GitContentStore,
        tree_id: &Id,
    ) -> Arc<GitIgnoreFile> {
        let base = GitIgnoreFile::empty();
        let tree = match store.read_tree(&tree_id.0) {
            Ok(Some(t)) => t,
            _ => return base,
        };
        for entry in &tree {
            if entry.name == GITIGNORE_FILE
                && matches!(entry.kind, GitEntryKind::File { .. })
                && let Ok(id) = <[u8; 20]>::try_from(entry.id.as_slice())
                && let Ok(Some(content)) = store.read_file(&id)
            {
                return base
                    .chain("", Path::new(GITIGNORE_FILE), &content)
                    .unwrap_or(base);
            }
        }
        base
    }

    /// Load `.kiki-redirections` from the root of the content tree.
    /// Returns a list of repo-root-relative paths to redirect to
    /// scratch storage.
    fn load_redirections_from_store(
        store: &GitContentStore,
        tree_id: &Id,
    ) -> Vec<String> {
        let tree = match store.read_tree(&tree_id.0) {
            Ok(Some(t)) => t,
            _ => return Vec::new(),
        };
        for entry in &tree {
            if entry.name == KIKI_REDIRECTIONS_FILE
                && matches!(entry.kind, GitEntryKind::File { .. })
                && let Ok(id) = <[u8; 20]>::try_from(entry.id.as_slice())
                && let Ok(Some(content)) = store.read_file(&id)
            {
                return Self::parse_redirections(&content);
            }
        }
        Vec::new()
    }

    /// Parse `.kiki-redirections`: one path per line, `#` comments,
    /// blank lines skipped.
    fn parse_redirections(content: &[u8]) -> Vec<String> {
        let text = String::from_utf8_lossy(content);
        text.lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.trim_end_matches('/').to_owned())
            .collect()
    }

    /// Build a repo-root-relative path string for a child being
    /// created under `parent` with the given `name`. Walks parent
    /// pointers in the slab to reconstruct the full path.
    fn repo_path_str(&self, parent: InodeId, name: &str) -> String {
        let mut components = vec![name.to_owned()];
        let mut current = parent;
        loop {
            if current == self.root() {
                break;
            }
            let inode = match self.slab.get(current) {
                Some(i) => i,
                None => break,
            };
            if inode.parent == current {
                break; // reached root (self-referencing)
            }
            components.push(inode.name.clone());
            current = inode.parent;
        }
        components.reverse();
        components.join("/")
    }

    /// Check whether a new entry at `(parent, name)` should be tagged
    /// as ignored. Fast path: parent is ignored → child inherits.
    /// Slow path: consult gitignore rules with the full repo-relative
    /// path.
    fn is_ignored(&self, parent: InodeId, name: &str, is_dir: bool) -> bool {
        // Fast path: parent is ignored → child inherits.
        if self.slab.get(parent).is_some_and(|i| i.ignored) {
            return true;
        }
        // Consult gitignore rules. Trailing `/` signals "is directory"
        // to jj-lib 0.40's `GitIgnoreFile::matches`.
        let path = self.repo_path_str(parent, name);
        if path.is_empty() {
            return false;
        }
        let match_path = if is_dir {
            format!("{path}/")
        } else {
            path
        };
        let rules = self.ignore_rules.lock();
        rules.matches(&match_path)
    }

    /// Hot-reload ignore or redirection rules from a `.gitignore` or
    /// `.kiki-redirections` write. Called from the `write` handler when
    /// the written file's name matches.
    fn reload_rules(&self, file_name: &str, content: &[u8]) {
        if file_name == GITIGNORE_FILE {
            // Re-chain from the empty base with the new content.
            // TODO: handle nested .gitignore files by tracking the
            // prefix from the parent chain.
            let base = GitIgnoreFile::empty();
            let rules = base
                .chain("", Path::new(GITIGNORE_FILE), content)
                .unwrap_or(base);
            *self.ignore_rules.lock() = rules;
        } else if file_name == KIKI_REDIRECTIONS_FILE {
            *self.redirections.lock() = Self::parse_redirections(content);
        }
    }

    /// Check whether `name` under root is a configured redirection.
    fn is_redirection(&self, parent: InodeId, name: &str) -> bool {
        if parent != self.root() {
            return false;
        }
        let redirections = self.redirections.lock();
        redirections.iter().any(|r| r == name)
    }

    /// Returns the pinned `.jj/` inode if it exists.
    fn jj_subtree(&self) -> Option<InodeId> {
        *self.jj_subtree.lock()
    }

    /// True when `(parent, name)` addresses the pinned `.jj/` slot —
    /// either currently populated or about to be by a `mkdir`.
    fn is_jj_root(&self, parent: InodeId, name: &str) -> bool {
        parent == self.root() && name == JJ_DIR
    }

    /// True when `(parent, name)` addresses the synthesized `.git` file.
    fn is_git_file(&self, parent: InodeId, name: &str) -> bool {
        parent == self.root() && name == GIT_FILE
    }

    /// Read a tree blob. Local store first; on miss with a configured
    /// remote (M10 §10.6), fall through to `RemoteStore::get_blob`,
    /// verify the bytes round-trip to the requested id, persist into
    /// the local store, and return. On miss with no remote, surfaces
    /// `FsError::StoreMiss` (mapped to `EIO` by the adapters).
    async fn read_tree(&self, id: Id) -> Result<Vec<GitTreeEntry>, FsError> {
        match self.store.read_tree(&id.0) {
            Ok(Some(t)) => return Ok(t),
            Ok(None) => {}
            Err(e) => return Err(store_err(e)),
        }
        let Some(remote) = &self.remote else {
            return Err(FsError::StoreMiss);
        };
        fetch::fetch_tree(&self.store, remote.as_ref(), &id.0)
            .await
            .map_err(fetch_err)
    }

    async fn read_file(&self, id: Id) -> Result<Vec<u8>, FsError> {
        match self.store.read_file(&id.0) {
            Ok(Some(f)) => return Ok(f),
            Ok(None) => {}
            Err(e) => return Err(store_err(e)),
        }
        let Some(remote) = &self.remote else {
            return Err(FsError::StoreMiss);
        };
        fetch::fetch_file(&self.store, remote.as_ref(), &id.0)
            .await
            .map_err(fetch_err)
    }

    async fn read_symlink(&self, id: Id) -> Result<String, FsError> {
        match self.store.read_symlink(&id.0) {
            Ok(Some(s)) => return Ok(s),
            Ok(None) => {}
            Err(e) => return Err(store_err(e)),
        }
        let Some(remote) = &self.remote else {
            return Err(FsError::StoreMiss);
        };
        fetch::fetch_symlink(&self.store, remote.as_ref(), &id.0)
            .await
            .map_err(fetch_err)
    }

    /// Map a `GitTreeEntry` to a `NodeRef` (our slab type).
    /// Submodule entries are surfaced as non-executable files for now.
    fn entry_to_node(entry: &GitTreeEntry) -> NodeRef {
        match entry.kind {
            GitEntryKind::File { executable } => NodeRef::File {
                id: Id(entry.id.as_slice().try_into().expect("20-byte id")),
                executable,
            },
            GitEntryKind::Tree => {
                NodeRef::Tree(Id(entry.id.as_slice().try_into().expect("20-byte id")))
            }
            GitEntryKind::Symlink => {
                NodeRef::Symlink(Id(entry.id.as_slice().try_into().expect("20-byte id")))
            }
            GitEntryKind::Submodule => NodeRef::File {
                id: Id(entry.id.as_slice().try_into().expect("20-byte id")),
                executable: false,
            },
        }
    }

    fn entry_kind(entry: &GitTreeEntry) -> FileKind {
        match entry.kind {
            GitEntryKind::Tree => FileKind::Directory,
            GitEntryKind::Symlink => FileKind::Symlink,
            GitEntryKind::File { .. } | GitEntryKind::Submodule => FileKind::Regular,
        }
    }

    /// Read the directory tree backing `inode`. Only valid for the clean
    /// `NodeRef::Tree` variant — dirty trees own their children directly,
    /// so callers walking a `DirtyTree` should iterate its `children`
    /// map instead.
    ///
    /// Async since M10 §10.6: the underlying [`Self::read_tree`] may
    /// fall through to a remote fetch on local-store miss.
    async fn dir_tree(&self, inode: &Inode) -> Result<Vec<GitTreeEntry>, FsError> {
        match inode.node {
            NodeRef::Tree(id) => self.read_tree(id).await,
            _ => Err(FsError::NotADirectory),
        }
    }

    /// Promote `dir` from clean `Tree` into `DirtyTree`, then walk up
    /// through parent pointers and dirty every ancestor directory too.
    ///
    /// The ancestor walk is necessary so that `snapshot` — which
    /// short-circuits on clean `Tree` nodes — can discover that a
    /// descendant was mutated. Without it, a mutation deep in the
    /// tree after a snapshot has cleaned the root would be invisible
    /// to the next snapshot.
    ///
    /// The walk stops early when it reaches a node that is already
    /// `DirtyTree` (all its ancestors must also be dirty from when
    /// it was first dirtied). It also stops at the `.jj` boundary:
    /// mutations inside the pinned `.jj/` subtree should not dirty
    /// the user tree root (`.jj` is excluded from `snapshot`).
    ///
    /// Idempotent: calling on an already-dirty tree is a no-op.
    ///
    /// Async since M10 §10.6 (see [`Self::dir_tree`]).
    async fn ensure_dirty_tree(&self, dir: InodeId) -> Result<(), FsError> {
        // Check if `dir` is inside the pinned `.jj/` subtree.
        // If so, materialize just `dir` itself (for local mutations
        // like creating op-store files) but don't propagate upward.
        let inside_jj = if let Some(jj_ino) = self.jj_subtree() {
            let mut probe = dir;
            loop {
                if probe == jj_ino {
                    break true;
                }
                let inode = self.slab.get(probe).ok_or(FsError::NotFound)?;
                if inode.parent == probe {
                    break false; // reached root without hitting .jj
                }
                probe = inode.parent;
            }
        } else {
            false
        };

        // Materialize `dir` itself.
        self.materialize_single_tree(dir).await?;

        if inside_jj {
            return Ok(());
        }

        // Walk up through parent pointers, dirtying each ancestor.
        // Stop when we reach an already-dirty node (its ancestors
        // are guaranteed dirty from the last time it was materialized)
        // or when we reach the root.
        let mut current = dir;
        loop {
            let inode = self.slab.get(current).ok_or(FsError::NotFound)?;
            let parent = inode.parent;
            if parent == current {
                // Root is its own parent — done.
                break;
            }
            let parent_inode = self.slab.get(parent).ok_or(FsError::NotFound)?;
            if matches!(parent_inode.node, NodeRef::DirtyTree { .. }) {
                break; // already dirty — all ancestors above are too
            }
            drop(parent_inode);
            self.materialize_single_tree(parent).await?;
            current = parent;
        }
        Ok(())
    }

    /// Promote a single directory inode from clean `Tree` to
    /// `DirtyTree`. Does NOT propagate to ancestors; that's
    /// `ensure_dirty_tree`'s job. Idempotent on `DirtyTree`.
    async fn materialize_single_tree(&self, dir: InodeId) -> Result<(), FsError> {
        let inode = self.slab.get(dir).ok_or(FsError::NotFound)?;
        match inode.node {
            NodeRef::DirtyTree { .. } => Ok(()),
            NodeRef::Tree(id) => {
                let tree = self.read_tree(id).await?;
                let entries: Vec<(String, NodeRef)> = tree
                    .iter()
                    .map(|e| (e.name.clone(), Self::entry_to_node(e)))
                    .collect();
                self.slab
                    .materialize_dir_for_mutation(dir, move || entries.into_iter())
                    .ok_or(FsError::NotADirectory)?;
                Ok(())
            }
            _ => Err(FsError::NotADirectory),
        }
    }

    /// Promote a clean `File` inode into `DirtyFile` by loading its
    /// content from the store. No-op for already-dirty files. Returns
    /// `NotAFile` for trees and symlinks.
    async fn ensure_dirty_file(&self, ino: InodeId) -> Result<(), FsError> {
        let inode = self.slab.get(ino).ok_or(FsError::NotFound)?;
        match inode.node {
            NodeRef::DirtyFile { .. } => Ok(()),
            NodeRef::File { id, executable } => {
                let content = self.read_file(id).await?;
                self.slab.replace_node(
                    ino,
                    NodeRef::DirtyFile {
                        content,
                        executable,
                    },
                );
                Ok(())
            }
            _ => Err(FsError::NotAFile),
        }
    }

    /// Detach a slab-attached child of `parent` by `name`, enforcing
    /// the rmdir-empty rule. Shared between the trait's async `remove`
    /// and the pinned-`.jj/` fall-through.
    ///
    /// Async since M10 §10.6: the empty-check on a still-clean child
    /// directory may need to fetch the tree blob from the remote.
    async fn remove_from_slab(&self, parent: InodeId, name: &str) -> Result<(), FsError> {
        // Materialize the parent so we can detach by name. If the named
        // child is itself a directory, refuse on a non-empty body —
        // mirrors POSIX `rmdir`.
        self.ensure_dirty_tree(parent).await?;

        // Peek at the child to enforce the "directory must be empty" rule
        // before we detach. Look up via the just-materialized
        // `DirtyTree.children` instead of `lookup` — `lookup` is async
        // and we'd still need the same data.
        let parent_inode = self.slab.get(parent).ok_or(FsError::NotFound)?;
        let child_id = match parent_inode.node {
            NodeRef::DirtyTree { ref children } => {
                children.get(name).copied().ok_or(FsError::NotFound)?
            }
            // Parent was just materialized; anything else is a bug.
            _ => return Err(FsError::NotADirectory),
        };
        let child = self.slab.get(child_id).ok_or(FsError::NotFound)?;
        // Pull the tree id out under the match so we can drop the
        // `Inode` ref before awaiting on `read_tree` (Inode contains
        // a non-`Send` parking_lot guard via `slab.get`).
        let clean_child_tree_id = match &child.node {
            NodeRef::DirtyTree { children } if !children.is_empty() => {
                return Err(FsError::NotEmpty);
            }
            NodeRef::Tree(id) => Some(*id),
            _ => None,
        };
        drop(child);
        if let Some(id) = clean_child_tree_id {
            // Empty check on a still-clean child directory: peek the
            // tree (possibly via remote read-through) without
            // materializing.
            let tree = self.read_tree(id).await?;
            if !tree.is_empty() {
                return Err(FsError::NotEmpty);
            }
        }

        self.slab
            .detach_child(parent, name)
            .ok_or(FsError::NotFound)?;
        Ok(())
    }

    /// Look up a child by name in a (possibly clean) directory inode.
    /// Used by the write path's "does this name already exist?" check.
    ///
    /// Async since M10 §10.6 (clean trees may need a remote fetch).
    async fn child_exists(&self, parent: InodeId, name: &str) -> Result<bool, FsError> {
        // Pinned `.jj/` shadows the user tree at the same name — and is
        // visible whether or not the underlying root is dirty.
        if self.is_jj_root(parent, name) && self.jj_subtree().is_some() {
            return Ok(true);
        }
        // Synthesized `.git` always exists at the root.
        if self.is_git_file(parent, name) {
            return Ok(true);
        }
        let parent_inode = self.slab.get(parent).ok_or(FsError::NotFound)?;
        match parent_inode.node {
            NodeRef::DirtyTree { children } => Ok(children.contains_key(name)),
            NodeRef::Tree(id) => {
                let tree = self.read_tree(id).await?;
                Ok(tree.iter().any(|e| e.name == name))
            }
            _ => Err(FsError::NotADirectory),
        }
    }

    /// Inflate an inode into the `Attr` shape both adapters consume.
    /// Same logic as `getattr`'s body, exposed as a helper so the write
    /// ops can return an `Attr` for the just-created/just-modified inode
    /// without round-tripping through the trait method.
    ///
    /// Async since M10 §10.6: a still-clean file/symlink whose blob
    /// isn't local needs a remote fetch to compute its size.
    async fn attr_for(&self, ino: InodeId) -> Result<Attr, FsError> {
        let inode = self.slab.get(ino).ok_or(FsError::NotFound)?;
        let mtime = self
            .commit_mtime
            .load(std::sync::atomic::Ordering::Relaxed);
        match inode.node {
            NodeRef::Tree(_) | NodeRef::DirtyTree { .. } => Ok(Attr {
                inode: ino,
                kind: FileKind::Directory,
                size: 0,
                executable: false,
                mtime_secs: mtime,
            }),
            NodeRef::File { id, executable } => {
                let content = self.read_file(id).await?;
                Ok(Attr {
                    inode: ino,
                    kind: FileKind::Regular,
                    size: content.len() as u64,
                    executable,
                    mtime_secs: mtime,
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
                mtime_secs: mtime,
            }),
            NodeRef::Symlink(id) => {
                let target = self.read_symlink(id).await?;
                Ok(Attr {
                    inode: ino,
                    kind: FileKind::Symlink,
                    size: target.len() as u64,
                    executable: false,
                    mtime_secs: mtime,
                })
            }
            NodeRef::DirtySymlink { ref target } => Ok(Attr {
                inode: ino,
                kind: FileKind::Symlink,
                size: target.len() as u64,
                executable: false,
                mtime_secs: mtime,
            }),
        }
    }

    /// Recursive snapshot: walk the inode at `ino`, persist any dirty
    /// content into the store, replace the slab entry with the clean
    /// counterpart, and return the resulting `Id`.
    ///
    /// Sync (no `.await`) so it can recurse without `Box::pin` /
    /// `async-recursion`. Store ops are sync since M6.
    ///
    /// At the root inode, an entry named `.jj` is excluded from the
    /// emitted tree — daemon-managed metadata never appears in the
    /// content-addressed user tree (PLAN §10.1). The pinned subtree
    /// itself is cleaned separately by [`Self::snapshot`].
    fn snapshot_node(&self, ino: InodeId) -> Result<Id, FsError> {
        let inode = self.slab.get(ino).ok_or(FsError::NotFound)?;
        match inode.node {
            // Clean refs: nothing to do, just hand back the id.
            NodeRef::Tree(id) | NodeRef::File { id, .. } | NodeRef::Symlink(id) => Ok(id),

            NodeRef::DirtyFile {
                content,
                executable,
            } => {
                let id_bytes = self.store.write_file(&content).map_err(store_err)?;
                let id = Id(id_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| store_err(anyhow::anyhow!("bad id len")))?);
                self.slab
                    .replace_node(ino, NodeRef::File { id, executable });
                Ok(id)
            }

            NodeRef::DirtySymlink { target } => {
                let id_bytes = self.store.write_symlink(&target).map_err(store_err)?;
                let id = Id(id_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| store_err(anyhow::anyhow!("bad id len")))?);
                self.slab.replace_node(ino, NodeRef::Symlink(id));
                Ok(id)
            }

            NodeRef::DirtyTree { children } => {
                // Recurse first, gather the canonical entries, then write
                // the tree. BTreeMap iteration is name-sorted, which is
                // also the order git stores tree entries — so two
                // equivalent dirty trees produce identical tree ids.
                let is_root = ino == self.root();
                let mut entries = Vec::with_capacity(children.len());
                for (name, child_id) in children {
                    // Defensive: with M7's `mkdir` short-circuit, `.jj`
                    // never lands in root.children — but legacy slabs
                    // (e.g. a tree checked out from a pre-M7 snapshot
                    // that contained `.jj/`) might have one anyway.
                    if is_root && (name == JJ_DIR || name == GIT_FILE) {
                        continue;
                    }
                    let child = self.slab.get(child_id).ok_or(FsError::NotFound)?;
                    // M10.7: skip gitignored inodes. They remain fully
                    // functional in the VFS but are never persisted to the
                    // content store or pushed to a remote.
                    if child.ignored {
                        continue;
                    }
                    let (kind, child_content_id) = match child.node {
                        NodeRef::Tree(id) => (GitEntryKind::Tree, id),
                        NodeRef::DirtyTree { .. } => {
                            (GitEntryKind::Tree, self.snapshot_node(child_id)?)
                        }
                        NodeRef::File { id, executable } => (GitEntryKind::File { executable }, id),
                        NodeRef::DirtyFile { executable, .. } => {
                            let id = self.snapshot_node(child_id)?;
                            (GitEntryKind::File { executable }, id)
                        }
                        NodeRef::Symlink(id) => (GitEntryKind::Symlink, id),
                        NodeRef::DirtySymlink { .. } => {
                            (GitEntryKind::Symlink, self.snapshot_node(child_id)?)
                        }
                    };
                    entries.push(GitTreeEntry {
                        name,
                        kind,
                        id: child_content_id.0.to_vec(),
                    });
                }
                let id_bytes = self.store.write_tree(&entries).map_err(store_err)?;
                let id = Id(id_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| store_err(anyhow::anyhow!("bad id len")))?);
                self.slab.replace_node(ino, NodeRef::Tree(id));
                Ok(id)
            }
        }
    }
}

#[async_trait]
impl JjKikiFs for KikiFs {
    async fn lookup(&self, parent: InodeId, name: &str) -> Result<InodeId, FsError> {
        // Synthesized `.git` gitdir file at the workspace root.
        if self.is_git_file(parent, name) {
            return Ok(self.git_file);
        }
        // Pinned `.jj/` shadows whatever the user tree has at the same
        // name. If unpinned we fall through; legacy trees that contain
        // a real `.jj` entry (snapshots taken before M7) still resolve
        // until something writes through the daemon and pins it.
        if self.is_jj_root(parent, name)
            && let Some(jj_ino) = self.jj_subtree()
        {
            return Ok(jj_ino);
        }
        let parent_inode = self.slab.get(parent).ok_or(FsError::NotFound)?;
        match parent_inode.node {
            NodeRef::DirtyTree { children } => children.get(name).copied().ok_or(FsError::NotFound),
            NodeRef::Tree(id) => {
                let tree = self.read_tree(id).await?;
                let entry = tree
                    .iter()
                    .find(|e| e.name == name)
                    .ok_or(FsError::NotFound)?;
                let node = Self::entry_to_node(entry);
                Ok(self.slab.intern_child(parent, name, || node))
            }
            _ => Err(FsError::NotADirectory),
        }
    }

    async fn getattr(&self, ino: InodeId) -> Result<Attr, FsError> {
        self.attr_for(ino).await
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
        // Pre-fetch the file content if the inode is a clean
        // `NodeRef::File` so we can drop the slab-held inode before
        // the slice borrow below; this also moves the (possibly
        // remote-fetching) `read_file` outside the match-by-ref.
        let owned_content: Option<Vec<u8>> = if let NodeRef::File { id, .. } = inode.node {
            Some(self.read_file(id).await?)
        } else {
            None
        };
        let content: &[u8] = match (&inode.node, owned_content.as_deref()) {
            (NodeRef::DirtyFile { content, .. }, _) => content,
            (NodeRef::File { .. }, Some(slice)) => slice,
            (NodeRef::File { .. }, None) => unreachable!("owned_content set above"),
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
        // Splice the pinned `.jj/` and synthesized `.git` into the root
        // listing. Any same-named entry in the user tree is shadowed
        // (matches `lookup`).
        let is_root = dir == self.root();
        let pinned_jj = if is_root {
            self.jj_subtree()
        } else {
            None
        };
        match inode.node {
            NodeRef::DirtyTree { children } => {
                // For a dirty tree we already have authoritative
                // (name -> inode) entries; just classify each one.
                let mut out = Vec::with_capacity(children.len() + pinned_jj.is_some() as usize);
                for (name, child_id) in children {
                    if pinned_jj.is_some() && name == JJ_DIR {
                        // pinned shadows; emit once below
                        continue;
                    }
                    // Shadow any user-tree `.git` entry with the synthesized one.
                    if is_root && name == GIT_FILE {
                        continue;
                    }
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
                if let Some(jj_ino) = pinned_jj {
                    out.push(DirEntry {
                        inode: jj_ino,
                        name: JJ_DIR.to_owned(),
                        kind: FileKind::Directory,
                    });
                }
                if is_root {
                    out.push(DirEntry {
                        inode: self.git_file,
                        name: GIT_FILE.to_owned(),
                        kind: FileKind::Regular,
                    });
                }
                Ok(out)
            }
            NodeRef::Tree(_) => {
                let tree = self.dir_tree(&inode).await?;
                let mut out = Vec::with_capacity(tree.len() + pinned_jj.is_some() as usize);
                for entry in &tree {
                    if pinned_jj.is_some() && entry.name == JJ_DIR {
                        continue;
                    }
                    // Shadow any user-tree `.git` entry with the synthesized one.
                    if is_root && entry.name == GIT_FILE {
                        continue;
                    }
                    let kind = Self::entry_kind(entry);
                    let node = Self::entry_to_node(entry);
                    let id = self.slab.intern_child(dir, &entry.name, || node);
                    out.push(DirEntry {
                        inode: id,
                        name: entry.name.clone(),
                        kind,
                    });
                }
                if let Some(jj_ino) = pinned_jj {
                    out.push(DirEntry {
                        inode: jj_ino,
                        name: JJ_DIR.to_owned(),
                        kind: FileKind::Directory,
                    });
                }
                if is_root {
                    out.push(DirEntry {
                        inode: self.git_file,
                        name: GIT_FILE.to_owned(),
                        kind: FileKind::Regular,
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
            NodeRef::Symlink(id) => self.read_symlink(id).await,
            NodeRef::DirtySymlink { target } => Ok(target),
            _ => Err(FsError::NotASymlink),
        }
    }

    async fn check_out(&self, new_root_tree: Id, commit_mtime_secs: i64) -> Result<(), FsError> {
        // Validate the target tree is present before we touch the slab —
        // otherwise we'd swap the root to an unreadable id and surface
        // every subsequent lookup as `StoreMiss`. M10 §10.6: the
        // validate-via-`read_tree` path also primes the local cache
        // from the remote in clone-style flows where the new root
        // isn't yet local.
        let _ = self.read_tree(new_root_tree).await?;
        self.slab.swap_root(new_root_tree);
        if commit_mtime_secs != 0 {
            self.commit_mtime
                .store(commit_mtime_secs, std::sync::atomic::Ordering::Relaxed);
        }
        // M10.7: reload ignore rules and redirections from the new tree.
        *self.ignore_rules.lock() =
            Self::load_gitignore_from_store(&self.store, &new_root_tree);
        *self.redirections.lock() =
            Self::load_redirections_from_store(&self.store, &new_root_tree);
        Ok(())
    }

    async fn create_file(
        &self,
        parent: InodeId,
        name: &str,
        executable: bool,
    ) -> Result<(InodeId, Attr), FsError> {
        if self.child_exists(parent, name).await? {
            return Err(FsError::AlreadyExists);
        }
        self.ensure_dirty_tree(parent).await?;
        let ignored = self.is_ignored(parent, name, false);
        let child = self.slab.alloc_with_ignored(
            parent,
            name.to_owned(),
            NodeRef::DirtyFile {
                content: Vec::new(),
                executable,
            },
            ignored,
        );
        if !self.slab.attach_child(parent, name.to_owned(), child) {
            return Err(FsError::AlreadyExists);
        }
        let attr = self.attr_for(child).await?;
        Ok((child, attr))
    }

    async fn mkdir(&self, parent: InodeId, name: &str) -> Result<(InodeId, Attr), FsError> {
        // Pinned `.jj/` slot — bypass the regular slab attachment so the
        // directory survives `swap_root` and stays out of `snapshot`.
        if self.is_jj_root(parent, name) {
            // Hold `pinned` across the synchronous slab op; release
            // before awaiting `attr_for`.
            let child = {
                let mut pinned = self.jj_subtree.lock();
                if pinned.is_some() {
                    return Err(FsError::AlreadyExists);
                }
                // Validate the parent is a directory we *could* attach
                // into, even though we don't actually attach. This
                // matches the error contract callers see for non-pinned
                // mkdirs.
                let parent_inode = self.slab.get(parent).ok_or(FsError::NotFound)?;
                match parent_inode.node {
                    NodeRef::Tree(_) | NodeRef::DirtyTree { .. } => {}
                    _ => return Err(FsError::NotADirectory),
                }
                let child = self.slab.alloc(
                    parent,
                    name.to_owned(),
                    NodeRef::DirtyTree {
                        children: std::collections::BTreeMap::new(),
                    },
                );
                *pinned = Some(child);
                child
            };
            let attr = self.attr_for(child).await?;
            return Ok((child, attr));
        }
        if self.child_exists(parent, name).await? {
            return Err(FsError::AlreadyExists);
        }
        self.ensure_dirty_tree(parent).await?;
        // M10.7: redirection → symlink to scratch dir on real local
        // disk. All I/O inside the directory bypasses FUSE entirely.
        if self.is_redirection(parent, name)
            && let Some(scratch) = &self.scratch_dir
        {
            let target_dir = scratch.join(name);
            std::fs::create_dir_all(&target_dir).map_err(|e| {
                FsError::StoreError(format!("creating scratch dir: {e}"))
            })?;
            let child = self.slab.alloc_with_ignored(
                parent,
                name.to_owned(),
                NodeRef::DirtySymlink {
                    target: target_dir.to_string_lossy().into_owned(),
                },
                true, // always ignored
            );
            if !self.slab.attach_child(parent, name.to_owned(), child) {
                return Err(FsError::AlreadyExists);
            }
            let attr = self.attr_for(child).await?;
            return Ok((child, attr));
        }
        let ignored = self.is_ignored(parent, name, true);
        let child = self.slab.alloc_with_ignored(
            parent,
            name.to_owned(),
            NodeRef::DirtyTree {
                children: std::collections::BTreeMap::new(),
            },
            ignored,
        );
        if !self.slab.attach_child(parent, name.to_owned(), child) {
            return Err(FsError::AlreadyExists);
        }
        let attr = self.attr_for(child).await?;
        Ok((child, attr))
    }

    async fn symlink(
        &self,
        parent: InodeId,
        name: &str,
        target: &str,
    ) -> Result<(InodeId, Attr), FsError> {
        if self.child_exists(parent, name).await? {
            return Err(FsError::AlreadyExists);
        }
        self.ensure_dirty_tree(parent).await?;
        let ignored = self.is_ignored(parent, name, false);
        let child = self.slab.alloc_with_ignored(
            parent,
            name.to_owned(),
            NodeRef::DirtySymlink {
                target: target.to_owned(),
            },
            ignored,
        );
        if !self.slab.attach_child(parent, name.to_owned(), child) {
            return Err(FsError::AlreadyExists);
        }
        let attr = self.attr_for(child).await?;
        Ok((child, attr))
    }

    async fn write(&self, ino: InodeId, offset: u64, data: &[u8]) -> Result<u32, FsError> {
        if ino == self.git_file {
            return Err(FsError::StoreError(
                "read-only synthesized .git file".into(),
            ));
        }
        self.ensure_dirty_file(ino).await?;
        // Dirty the file's parent and ancestors so snapshot can
        // discover the modification (ensure_dirty_tree propagates).
        let parent = self.slab.get(ino).ok_or(FsError::NotFound)?.parent;
        self.ensure_dirty_tree(parent).await?;
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
        let file_name = inode.name.clone();
        let new = NodeRef::DirtyFile {
            content: std::mem::take(content),
            executable,
        };
        self.slab.replace_node(ino, new);
        // M10.7: hot-reload gitignore / redirection rules when the
        // written file is `.gitignore` or `.kiki-redirections`.
        if file_name == GITIGNORE_FILE || file_name == KIKI_REDIRECTIONS_FILE {
            // Re-read the complete buffer from the slab (the splice
            // above may have been a partial write).
            if let Some(updated) = self.slab.get(ino)
                && let NodeRef::DirtyFile { ref content, .. } = updated.node
            {
                self.reload_rules(&file_name, content);
            }
        }
        Ok(data.len() as u32)
    }

    async fn setattr(
        &self,
        ino: InodeId,
        size: Option<u64>,
        executable: Option<bool>,
    ) -> Result<Attr, FsError> {
        if ino == self.git_file {
            return Err(FsError::StoreError(
                "read-only synthesized .git file".into(),
            ));
        }
        // Both fields touch a file — make sure we're working with a
        // dirty buffer, and dirty ancestors so snapshot can find us.
        if size.is_some() || executable.is_some() {
            self.ensure_dirty_file(ino).await?;
            // Dirty the file's parent and ancestors so snapshot can
            // discover the modification.
            let parent = self.slab.get(ino).ok_or(FsError::NotFound)?.parent;
            self.ensure_dirty_tree(parent).await?;
        } else {
            // No-op setattr (e.g. atime-only) just returns current attrs.
            return self.attr_for(ino).await;
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
        self.attr_for(ino).await
    }

    async fn remove(&self, parent: InodeId, name: &str) -> Result<(), FsError> {
        if self.is_git_file(parent, name) {
            return Err(FsError::StoreError(
                "cannot remove synthesized .git file".into(),
            ));
        }
        // Pinned `.jj/` removal: rmdir-empty guard, then clear the pin.
        // The slab still owns the inode (orphaned, same as `detach_child`)
        // so any cached kernel handle stays resolvable.
        if self.is_jj_root(parent, name) {
            // Pull the pinned id out without holding the parking_lot
            // guard across the empty-check await. We re-acquire to
            // clear the pin once we've confirmed it's safe to drop.
            let pinned_jj = *self.jj_subtree.lock();
            let Some(jj_ino) = pinned_jj else {
                // Fall through: maybe a legacy `.jj` lives in the user tree.
                return self.remove_from_slab(parent, name).await;
            };
            let inode = self.slab.get(jj_ino).ok_or(FsError::NotFound)?;
            // Extract just the bits we need before the inode goes
            // out of scope so we can await `read_tree` without
            // borrowing the inode across the await.
            enum PinShape {
                EmptyDirty,
                CleanTree(Id),
                NonEmptyDirty,
                NotADir,
            }
            let shape = match &inode.node {
                NodeRef::DirtyTree { children } if children.is_empty() => PinShape::EmptyDirty,
                NodeRef::DirtyTree { .. } => PinShape::NonEmptyDirty,
                NodeRef::Tree(id) => PinShape::CleanTree(*id),
                _ => PinShape::NotADir,
            };
            drop(inode);
            match shape {
                PinShape::NonEmptyDirty => return Err(FsError::NotEmpty),
                PinShape::NotADir => return Err(FsError::NotADirectory),
                PinShape::CleanTree(id) => {
                    let tree = self.read_tree(id).await?;
                    if !tree.is_empty() {
                        return Err(FsError::NotEmpty);
                    }
                }
                PinShape::EmptyDirty => {}
            }
            // Re-check that nobody re-pinned a different `.jj/` while
            // we were awaiting (no current code path does, but keep the
            // invariant locally enforced).
            let mut pinned = self.jj_subtree.lock();
            if *pinned == Some(jj_ino) {
                *pinned = None;
            }
            return Ok(());
        }
        self.remove_from_slab(parent, name).await
    }

    async fn rename(
        &self,
        parent: InodeId,
        name: &str,
        new_parent: InodeId,
        new_name: &str,
    ) -> Result<(), FsError> {
        // Refuse renames that would touch the pinned `.jj/` slot. The
        // pin lives outside the slab's child maps so it can't simply be
        // detached and reattached the way regular entries are. jj-lib
        // never renames `.jj` itself, so blocking this is correct in
        // practice and keeps the slab/pin invariants simple.
        if self.is_jj_root(parent, name)
            || self.is_jj_root(new_parent, new_name)
            || self.is_git_file(parent, name)
            || self.is_git_file(new_parent, new_name)
        {
            return Err(FsError::AlreadyExists);
        }
        // Both parents need to be DirtyTree so we can mutate their
        // children maps. They might be the same inode (same-directory
        // rename — the common case for jj-lib's tmpfile→final swap).
        // Materialize both; same-parent is a no-op the second time.
        self.ensure_dirty_tree(parent).await?;
        if new_parent != parent {
            self.ensure_dirty_tree(new_parent).await?;
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
                NodeRef::DirtyTree { ref children } => {
                    children.get(name).copied().ok_or(FsError::NotFound)?
                }
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
            // Pull the type/empty info out of the slab synchronously,
            // then drop the inode handles before any potential await.
            // Tree id is captured for the clean-dir empty-check below.
            #[derive(PartialEq)]
            enum DirShape {
                Dir,
                NonDir,
            }
            let (src_shape, dst_shape, dst_clean_tree_id, dst_dirty_empty) = {
                let src = self.slab.get(src_inode_id).ok_or(FsError::NotFound)?;
                let dst = self.slab.get(dst_id).ok_or(FsError::NotFound)?;
                let src_shape = match src.node {
                    NodeRef::Tree(_) | NodeRef::DirtyTree { .. } => DirShape::Dir,
                    _ => DirShape::NonDir,
                };
                let (dst_shape, dst_clean_tree_id, dst_dirty_empty) = match &dst.node {
                    NodeRef::Tree(id) => (DirShape::Dir, Some(*id), None),
                    NodeRef::DirtyTree { children } => {
                        (DirShape::Dir, None, Some(children.is_empty()))
                    }
                    _ => (DirShape::NonDir, None, None),
                };
                (src_shape, dst_shape, dst_clean_tree_id, dst_dirty_empty)
            };
            match (src_shape == DirShape::Dir, dst_shape == DirShape::Dir) {
                (true, false) => return Err(FsError::NotADirectory),
                (false, true) => return Err(FsError::NotAFile),
                (true, true) => {
                    // Empty-check the destination directory before
                    // clobbering. Mirrors `remove`'s rmdir-empty rule.
                    // Clean dst may need a remote fetch on M10
                    // §10.6's read-through path.
                    let empty = if let Some(id) = dst_clean_tree_id {
                        self.read_tree(id).await?.is_empty()
                    } else {
                        dst_dirty_empty.expect("dirty branch sets dst_dirty_empty")
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
        // Update the child's parent/name so that subsequent
        // `ensure_dirty_tree` ancestor-walks propagate through the
        // *new* parent chain. Without this, writes to the renamed
        // file after a snapshot would dirty the old parent's
        // ancestors instead of the new parent's, and snapshot would
        // miss the modification.
        self.slab
            .reparent(src_inode_id, new_parent, new_name.to_owned());
        Ok(())
    }

    async fn snapshot(&self) -> Result<Id, FsError> {
        let user_root = self.snapshot_node(self.root())?;
        // Clean the pinned `.jj/` subtree's dirty buffers too so memory
        // doesn't accumulate across snapshots. The result tree id is
        // discarded — `.jj/` lives outside the user-tree rollup. Any
        // failure here is surfaced to the caller; we'd rather refuse a
        // snapshot than leave the slab in a half-clean state.
        if let Some(jj_ino) = self.jj_subtree() {
            let _ = self.snapshot_node(jj_ino)?;
        }
        Ok(user_root)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use jj_lib::object_id::ObjectId as _;

    use super::*;
    use crate::{
        git_store::{GitContentStore, GitEntryKind, GitTreeEntry},
        ty::Id,
    };

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

    /// Helper: convert a Vec<u8> id from the store into our Id type.
    fn vec_to_id(v: &[u8]) -> Id {
        Id(v.try_into().expect("20-byte id"))
    }

    /// Helper: get the empty tree Id from a store.
    fn empty_tree_id(store: &GitContentStore) -> Id {
        Id(store.empty_tree_id().as_bytes().try_into().unwrap())
    }

    /// Build a small synthetic repo on a fresh store:
    /// ```text
    /// /
    /// ├── hello.txt          file "hi\n"
    /// ├── bin/
    /// │   └── tool           executable file "x"
    /// └── link               symlink -> "hello.txt"
    /// ```
    /// Returns `(store, root_tree_id)`.
    fn build_synthetic_tree() -> (Arc<GitContentStore>, Id) {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let hello_id = store.write_file(b"hi\n").unwrap();
        let tool_id = store.write_file(b"x").unwrap();
        let link_id = store.write_symlink("hello.txt").unwrap();
        let bin_entries = vec![GitTreeEntry {
            name: "tool".into(),
            kind: GitEntryKind::File { executable: true },
            id: tool_id,
        }];
        let bin_id = store.write_tree(&bin_entries).unwrap();
        let root_entries = vec![
            GitTreeEntry {
                name: "bin".into(),
                kind: GitEntryKind::Tree,
                id: bin_id,
            },
            GitTreeEntry {
                name: "hello.txt".into(),
                kind: GitEntryKind::File { executable: false },
                id: hello_id,
            },
            GitTreeEntry {
                name: "link".into(),
                kind: GitEntryKind::Symlink,
                id: link_id,
            },
        ];
        let root_id_bytes = store.write_tree(&root_entries).unwrap();
        let root_id = vec_to_id(&root_id_bytes);
        (store, root_id)
    }

    #[tokio::test]
    async fn empty_repo_has_only_root() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);
        let entries = fs.readdir(fs.root()).await.expect("readdir empty root");
        // The only entry in an empty repo is the synthesized `.git`.
        assert_eq!(entries.len(), 1, "got {entries:?}");
        assert_eq!(entries[0].name, ".git");
        assert_eq!(entries[0].kind, FileKind::Regular);
        let attr = fs.getattr(fs.root()).await.expect("getattr root");
        assert_eq!(attr.kind, FileKind::Directory);
    }

    #[tokio::test]
    async fn lookup_finds_top_level_file() {
        let (store, root) = build_synthetic_tree();
        let fs = KikiFs::new(store, root, None, None, None, 0);
        let id = fs.lookup(fs.root(), "hello.txt").await.expect("lookup");
        let attr = fs.getattr(id).await.expect("getattr");
        assert_eq!(attr.kind, FileKind::Regular);
        assert_eq!(attr.size, 3);
        assert!(!attr.executable);
    }

    #[tokio::test]
    async fn lookup_traverses_subdirectory() {
        let (store, root) = build_synthetic_tree();
        let fs = KikiFs::new(store, root, None, None, None, 0);
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
        let fs = KikiFs::new(store, root, None, None, None, 0);
        let a = fs.lookup(fs.root(), "hello.txt").await.unwrap();
        let b = fs.lookup(fs.root(), "hello.txt").await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn read_returns_file_content_with_eof_flag() {
        let (store, root) = build_synthetic_tree();
        let fs = KikiFs::new(store, root, None, None, None, 0);
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
        let fs = KikiFs::new(store, root, None, None, None, 0);
        let mut entries = fs.readdir(fs.root()).await.unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec![".git", "bin", "hello.txt", "link"]);
        let kinds: Vec<_> = entries.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                FileKind::Regular,
                FileKind::Directory,
                FileKind::Regular,
                FileKind::Symlink
            ]
        );
    }

    #[tokio::test]
    async fn readlink_returns_target() {
        let (store, root) = build_synthetic_tree();
        let fs = KikiFs::new(store, root, None, None, None, 0);
        let id = fs.lookup(fs.root(), "link").await.unwrap();
        assert_eq!(fs.readlink(id).await.unwrap(), "hello.txt");
    }

    #[tokio::test]
    async fn lookup_unknown_name_is_not_found() {
        let (store, root) = build_synthetic_tree();
        let fs = KikiFs::new(store, root, None, None, None, 0);
        let err = fs.lookup(fs.root(), "missing").await.unwrap_err();
        assert_eq!(err, FsError::NotFound);
    }

    #[tokio::test]
    async fn read_on_directory_is_not_a_file() {
        let (store, root) = build_synthetic_tree();
        let fs = KikiFs::new(store, root, None, None, None, 0);
        let err = fs.read(fs.root(), 0, 16).await.unwrap_err();
        assert_eq!(err, FsError::NotAFile);
    }

    #[tokio::test]
    async fn readdir_on_file_is_not_a_directory() {
        let (store, root) = build_synthetic_tree();
        let fs = KikiFs::new(store, root, None, None, None, 0);
        let id = fs.lookup(fs.root(), "hello.txt").await.unwrap();
        let err = fs.readdir(id).await.unwrap_err();
        assert_eq!(err, FsError::NotADirectory);
    }

    #[tokio::test]
    async fn getattr_on_unknown_inode_is_not_found() {
        let (store, root) = build_synthetic_tree();
        let fs = KikiFs::new(store, root, None, None, None, 0);
        let err = fs.getattr(99_999).await.unwrap_err();
        assert_eq!(err, FsError::NotFound);
    }

    /// `check_out` swaps the visible root tree. Lookups after the swap
    /// must see the new tree's children and not the old's.
    #[tokio::test]
    async fn check_out_swaps_visible_tree() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        // Build two distinct one-file root trees.
        let a_content_id = store.write_file(b"a-content").unwrap();
        let b_content_id = store.write_file(b"b-content").unwrap();
        let tree_a_bytes = store
            .write_tree(&[GitTreeEntry {
                name: "only-in-a.txt".into(),
                kind: GitEntryKind::File { executable: false },
                id: a_content_id,
            }])
            .unwrap();
        let tree_a = vec_to_id(&tree_a_bytes);
        let tree_b_bytes = store
            .write_tree(&[GitTreeEntry {
                name: "only-in-b.txt".into(),
                kind: GitEntryKind::File { executable: false },
                id: b_content_id,
            }])
            .unwrap();
        let tree_b = vec_to_id(&tree_b_bytes);

        let fs = KikiFs::new(store, tree_a, None, None, None, 0);
        // Tree A is visible.
        fs.lookup(fs.root(), "only-in-a.txt").await.expect("A");
        assert_eq!(
            fs.lookup(fs.root(), "only-in-b.txt").await.unwrap_err(),
            FsError::NotFound
        );

        // Swap to tree B.
        fs.check_out(tree_b, 0).await.expect("check_out");
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
        let fs = KikiFs::new(store, root, None, None, None, 0);
        let err = fs.check_out(Id([0xff; 20]), 0).await.unwrap_err();
        assert_eq!(err, FsError::StoreMiss);
        // Original tree is still visible.
        fs.lookup(fs.root(), "hello.txt").await.expect("still A");
    }

    // ----- M6 write-path tests -----------------------------------------

    /// Helper: read a tree from the store and return it as Vec<GitTreeEntry>.
    fn read_tree(store: &GitContentStore, id: Id) -> Vec<GitTreeEntry> {
        store
            .read_tree(&id.0)
            .expect("read_tree")
            .expect("tree present")
    }

    /// Helper: read a file from the store and return its content.
    fn read_file_content(store: &GitContentStore, id: Id) -> Vec<u8> {
        store
            .read_file(&id.0)
            .expect("read_file")
            .expect("file present")
    }

    /// Helper: read a symlink from the store and return the target.
    fn read_symlink_target(store: &GitContentStore, id: Id) -> String {
        store
            .read_symlink(&id.0)
            .expect("read_symlink")
            .expect("symlink present")
    }

    /// Helper: find a tree entry by name.
    fn find_entry<'a>(entries: &'a [GitTreeEntry], name: &str) -> &'a GitTreeEntry {
        entries.iter().find(|e| e.name == name).expect(name)
    }

    /// Helper: extract the Id from a tree entry.
    fn entry_id(entry: &GitTreeEntry) -> Id {
        vec_to_id(&entry.id)
    }

    /// `create_file` + `write` + `read` round-trip on an empty repo.
    /// Snapshot afterward produces a tree that contains the new file at
    /// the right content. The slab's clean-up after snapshot keeps the
    /// inode resolvable via the same id, so post-snapshot reads still
    /// hit the right content.
    #[tokio::test]
    async fn create_write_read_round_trips() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);
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
        assert_ne!(new_root, empty_tree_id(&store));
        let tree = read_tree(&store, new_root);
        let entry = find_entry(&tree, "hello.txt");
        assert!(matches!(entry.kind, GitEntryKind::File { .. }));

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
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        let (dir_ino, _) = fs.mkdir(fs.root(), "dir").await.expect("mkdir");
        let (file_ino, _) = fs
            .create_file(dir_ino, "file", false)
            .await
            .expect("create_file");
        fs.write(file_ino, 0, b"content").await.unwrap();

        let new_root = fs.snapshot().await.expect("snapshot");

        // Walk the tree: root -> "dir" -> "file".
        let root_tree = read_tree(&store, new_root);
        let dir_entry = find_entry(&root_tree, "dir");
        assert!(matches!(dir_entry.kind, GitEntryKind::Tree));
        let dir_id = entry_id(dir_entry);
        let dir_tree = read_tree(&store, dir_id);
        let file_entry = find_entry(&dir_tree, "file");
        assert!(matches!(file_entry.kind, GitEntryKind::File { .. }));
        let file_id = entry_id(file_entry);
        let content = read_file_content(&store, file_id);
        assert_eq!(content, b"content");
    }

    /// `symlink` + snapshot produces a Symlink entry whose target is
    /// readable through the store.
    #[tokio::test]
    async fn symlink_round_trips() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);
        let (link_ino, attr) = fs
            .symlink(fs.root(), "link", "target")
            .await
            .expect("symlink");
        assert_eq!(attr.kind, FileKind::Symlink);
        assert_eq!(fs.readlink(link_ino).await.unwrap(), "target");

        let new_root = fs.snapshot().await.expect("snapshot");
        let root_tree = read_tree(&store, new_root);
        let link_entry = find_entry(&root_tree, "link");
        assert!(matches!(link_entry.kind, GitEntryKind::Symlink));
        let symlink_id = entry_id(link_entry);
        let target = read_symlink_target(&store, symlink_id);
        assert_eq!(target, "target");

        // After snapshot the symlink reads back through the clean path
        // (NodeRef::Symlink -> store).
        assert_eq!(fs.readlink(link_ino).await.unwrap(), "target");
    }

    /// Duplicate `create_file` on the same name returns AlreadyExists.
    #[tokio::test]
    async fn create_collision_is_already_exists() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);
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
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);
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
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);
        let (ino, _) = fs.create_file(fs.root(), "f", false).await.unwrap();
        fs.write(ino, 0, b"hello").await.unwrap();

        let attr = fs.setattr(ino, None, Some(true)).await.expect("chmod");
        assert!(attr.executable);

        let new_root = fs.snapshot().await.expect("snapshot");
        let tree = read_tree(&store, new_root);
        let entry = find_entry(&tree, "f");
        assert!(matches!(
            entry.kind,
            GitEntryKind::File { executable: true }
        ));
    }

    /// `remove` of a file detaches it; subsequent lookup is NotFound.
    /// The detached inode is gone from the parent, but its id stays
    /// monotonic (non-reused).
    #[tokio::test]
    async fn remove_file_then_lookup_is_not_found() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);
        fs.create_file(fs.root(), "f", false).await.unwrap();
        fs.remove(fs.root(), "f").await.expect("remove");
        let err = fs.lookup(fs.root(), "f").await.unwrap_err();
        assert_eq!(err, FsError::NotFound);
    }

    /// `remove` of a non-empty directory returns NotEmpty. The directory
    /// stays attached after the failure.
    #[tokio::test]
    async fn remove_non_empty_directory_is_not_empty() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);
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
        let fs = KikiFs::new(store.clone(), root, None, None, None, 0);
        let id = fs.snapshot().await.expect("snapshot");
        assert_eq!(id, root);
    }

    /// Two distinct dirty edits that produce structurally identical
    /// trees yield identical snapshot ids — the BTreeMap-based name
    /// ordering keeps content hashing stable.
    #[tokio::test]
    async fn snapshot_is_deterministic_under_insertion_order() {
        let settings = test_settings();
        let store_a = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs_a = KikiFs::new(store_a.clone(), empty_tree_id(&store_a), None, None, None, 0);
        fs_a.create_file(fs_a.root(), "a", false).await.unwrap();
        fs_a.create_file(fs_a.root(), "b", false).await.unwrap();
        let id_a = fs_a.snapshot().await.unwrap();

        let store_b = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs_b = KikiFs::new(store_b.clone(), empty_tree_id(&store_b), None, None, None, 0);
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
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);
        let (ino, _) = fs.create_file(fs.root(), "f", false).await.unwrap();
        fs.write(ino, 0, b"x").await.unwrap();
        fs.snapshot().await.unwrap();
        // Same inode id still resolves and reads.
        let (data, _) = fs.read(ino, 0, 1024).await.unwrap();
        assert_eq!(data, b"x");
    }

    // ----- M7.1 pinned `.jj/` tests ------------------------------------

    /// `.jj/` created via `mkdir(root, ".jj")` is visible through
    /// lookup/readdir but does NOT appear in the rolled-up snapshot tree.
    #[tokio::test]
    async fn jj_dir_excluded_from_snapshot() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        // Create `.jj/` and a file inside it (matches what jj-lib does
        // during `init_with_factories`).
        let (jj_ino, attr) = fs.mkdir(fs.root(), ".jj").await.expect("mkdir .jj");
        assert_eq!(attr.kind, FileKind::Directory);
        let (jj_file, _) = fs
            .create_file(jj_ino, "config.toml", false)
            .await
            .expect("create_file");
        fs.write(jj_file, 0, b"[ui]\n").await.unwrap();

        // Also create a real user file at the root.
        let (user_file, _) = fs
            .create_file(fs.root(), "README", false)
            .await
            .expect("create_file");
        fs.write(user_file, 0, b"hi").await.unwrap();

        // `.git`, `.jj/`, and `README` are all visible through readdir.
        let mut listing = fs.readdir(fs.root()).await.unwrap();
        listing.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = listing.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec![".git", ".jj", "README"]);

        // Lookup of `.jj` returns the pinned inode.
        assert_eq!(fs.lookup(fs.root(), ".jj").await.unwrap(), jj_ino);

        // Snapshot rolls up the user tree and excludes `.jj/`.
        let new_root = fs.snapshot().await.expect("snapshot");
        let tree = read_tree(&store, new_root);
        let names: Vec<_> = tree.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["README"], "snapshot must not contain .jj");

        // Post-snapshot: `.jj/` is still visible and its file still reads.
        assert_eq!(fs.lookup(fs.root(), ".jj").await.unwrap(), jj_ino);
        let (data, _) = fs.read(jj_file, 0, 1024).await.unwrap();
        assert_eq!(data, b"[ui]\n");
    }

    /// Pinned `.jj/` must survive a `check_out` to a different user
    /// tree. The user tree changes; the daemon-managed metadata stays.
    #[tokio::test]
    async fn jj_dir_survives_check_out() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        // Set up a `.jj/` with some content.
        let (jj_ino, _) = fs.mkdir(fs.root(), ".jj").await.unwrap();
        let (jj_file, _) = fs.create_file(jj_ino, "marker", false).await.unwrap();
        fs.write(jj_file, 0, b"present").await.unwrap();

        // Build a user tree separately and check it out.
        let only_a_id = store.write_file(b"a").unwrap();
        let tree_a_bytes = store
            .write_tree(&[GitTreeEntry {
                name: "only-a.txt".into(),
                kind: GitEntryKind::File { executable: false },
                id: only_a_id,
            }])
            .unwrap();
        let tree_a = vec_to_id(&tree_a_bytes);
        fs.check_out(tree_a, 0).await.expect("check_out");

        // After check_out: user content reflects tree_a; `.jj/` still
        // resolves to the same pinned inode and its content is intact.
        fs.lookup(fs.root(), "only-a.txt")
            .await
            .expect("user content visible");
        assert_eq!(fs.lookup(fs.root(), ".jj").await.unwrap(), jj_ino);
        let (data, _) = fs.read(jj_file, 0, 1024).await.unwrap();
        assert_eq!(data, b"present");

        // readdir at root mixes the user tree with the pin and `.git`.
        let mut listing = fs.readdir(fs.root()).await.unwrap();
        listing.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = listing.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec![".git", ".jj", "only-a.txt"]);
    }

    /// `mkdir(root, ".jj")` twice fails AlreadyExists. Same goes for
    /// `create_file` / `symlink` against the pinned name.
    #[tokio::test]
    async fn jj_dir_pin_is_unique() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);
        fs.mkdir(fs.root(), ".jj").await.expect("mkdir .jj");
        let err = fs.mkdir(fs.root(), ".jj").await.expect_err("dup mkdir");
        assert_eq!(err, FsError::AlreadyExists);
        let err = fs
            .create_file(fs.root(), ".jj", false)
            .await
            .expect_err("file collides with pin");
        assert_eq!(err, FsError::AlreadyExists);
    }

    /// Pre-existing `.jj` in a checked-out tree (legacy snapshot) is
    /// visible until something pins our own — at which point the pin
    /// shadows it. Drives the migration story for snapshots taken
    /// before M7.
    #[tokio::test]
    async fn pre_existing_jj_is_shadowed_by_pin() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        // Build a synthetic legacy tree containing `.jj` as a file.
        let legacy_content_id = store.write_file(b"legacy").unwrap();
        let legacy_tree_bytes = store
            .write_tree(&[GitTreeEntry {
                name: ".jj".into(),
                kind: GitEntryKind::File { executable: false },
                id: legacy_content_id,
            }])
            .unwrap();
        let legacy_tree = vec_to_id(&legacy_tree_bytes);
        let fs = KikiFs::new(store.clone(), legacy_tree, None, None, None, 0);

        // Without a pin, lookup of `.jj` falls through to the user tree
        // and returns the legacy file.
        let legacy_ino = fs.lookup(fs.root(), ".jj").await.expect("legacy lookup");
        let attr = fs.getattr(legacy_ino).await.unwrap();
        assert_eq!(attr.kind, FileKind::Regular);

        // Pinning a new `.jj/` shadows the legacy entry.
        let (jj_ino, _) = fs.mkdir(fs.root(), ".jj").await.expect("mkdir pin");
        assert_eq!(fs.lookup(fs.root(), ".jj").await.unwrap(), jj_ino);
    }

    /// `remove(root, ".jj")` clears the pin when the directory is empty
    /// and refuses with NotEmpty when it isn't.
    #[tokio::test]
    async fn jj_dir_remove_respects_rmdir_empty() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);
        let (jj_ino, _) = fs.mkdir(fs.root(), ".jj").await.unwrap();
        fs.create_file(jj_ino, "f", false).await.unwrap();
        let err = fs.remove(fs.root(), ".jj").await.unwrap_err();
        assert_eq!(err, FsError::NotEmpty);

        fs.remove(jj_ino, "f").await.expect("remove inner");
        fs.remove(fs.root(), ".jj").await.expect("remove pin");
        // Pin cleared: lookup is NotFound (no legacy fall-through here).
        let err = fs.lookup(fs.root(), ".jj").await.unwrap_err();
        assert_eq!(err, FsError::NotFound);
    }

    /// Snapshot cleans dirty buffers inside the pinned subtree even
    /// though the resulting tree id is discarded — keeps memory bounded
    /// across many `.jj/` writes.
    #[tokio::test]
    async fn jj_dir_dirty_buffers_cleaned_on_snapshot() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);
        let (jj_ino, _) = fs.mkdir(fs.root(), ".jj").await.unwrap();
        let (file_ino, _) = fs.create_file(jj_ino, "config", false).await.unwrap();
        fs.write(file_ino, 0, b"settings").await.unwrap();

        // Snapshot returns the empty user tree (no `.jj/` rolled up).
        let new_root = fs.snapshot().await.unwrap();
        assert_eq!(new_root, empty_tree_id(&store));

        // The pinned subtree's slab entry is now clean (NodeRef::Tree
        // resp. NodeRef::File). We assert this indirectly by checking
        // the file is reachable through the store after snapshot —
        // possible only if the dirty buffer was persisted.
        let inode = fs.slab.get(file_ino).expect("inode survives");
        match inode.node {
            NodeRef::File { id, .. } => {
                let content = read_file_content(&store, id);
                assert_eq!(content, b"settings");
            }
            other => panic!("expected clean File after snapshot, got {other:?}"),
        }
    }

    // ----- Synthesized `.git` gitdir file tests ------------------------------

    /// The synthesized `.git` file is visible via lookup and readdir,
    /// contains the expected `gitdir:` content, and is excluded from
    /// snapshot output.
    #[tokio::test]
    async fn git_file_lookup_read_content() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let expected =
            format!("gitdir: {}\n", store.git_repo_path().display());
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        // lookup resolves the synthesized `.git` inode.
        let git_ino = fs.lookup(fs.root(), ".git").await.expect("lookup .git");

        // getattr reports it as a regular file with the right size.
        let attr = fs.getattr(git_ino).await.expect("getattr .git");
        assert_eq!(attr.kind, FileKind::Regular);
        assert_eq!(attr.size, expected.len() as u64);
        assert!(!attr.executable);

        // read returns the full `gitdir:` content.
        let (data, eof) = fs.read(git_ino, 0, 4096).await.expect("read .git");
        assert_eq!(String::from_utf8(data).unwrap(), expected);
        assert!(eof);

        // readdir includes `.git`.
        let listing = fs.readdir(fs.root()).await.unwrap();
        assert!(
            listing.iter().any(|e| e.name == ".git"),
            "readdir must include .git"
        );

        // snapshot must NOT include `.git` in the rolled-up tree.
        let snap = fs.snapshot().await.unwrap();
        let tree = read_tree(&store, snap);
        assert!(
            !tree.iter().any(|e| e.name == ".git"),
            "snapshot must not contain .git"
        );
    }

    /// Writing to the synthesized `.git` file is rejected.
    #[tokio::test]
    async fn git_file_is_read_only() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);
        let git_ino = fs.lookup(fs.root(), ".git").await.unwrap();

        let err = fs.write(git_ino, 0, b"nope").await.unwrap_err();
        assert!(matches!(err, FsError::StoreError(_)));

        let err = fs.setattr(git_ino, Some(0), None).await.unwrap_err();
        assert!(matches!(err, FsError::StoreError(_)));
    }

    /// Removing or renaming the synthesized `.git` file is rejected.
    #[tokio::test]
    async fn git_file_cannot_be_removed_or_renamed() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        let err = fs.remove(fs.root(), ".git").await.unwrap_err();
        assert!(matches!(err, FsError::StoreError(_)));

        // Creating a file named `.git` collides with the synthesized one.
        let err = fs
            .create_file(fs.root(), ".git", false)
            .await
            .unwrap_err();
        assert_eq!(err, FsError::AlreadyExists);

        // Renaming something to `.git` is rejected.
        let (f, _) = fs.create_file(fs.root(), "tmp", false).await.unwrap();
        let _ = f; // suppress unused warning
        let err = fs
            .rename(fs.root(), "tmp", fs.root(), ".git")
            .await
            .unwrap_err();
        assert_eq!(err, FsError::AlreadyExists);
    }

    // ----- Regression tests for e2e bugs (2026-05-01) ----------------------

    /// Regression test for Bug 2: ancestor-dirty propagation.
    ///
    /// After `snapshot` cleans the tree, a subsequent write deep in the
    /// tree must dirty all ancestor directories so the next `snapshot`
    /// can discover the mutation. Before the fix, `snapshot` would
    /// short-circuit on the clean root and return the stale tree id.
    #[tokio::test]
    async fn write_after_snapshot_deep_in_tree_is_visible() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        // Build /dir/subdir/file with initial content.
        let (dir_ino, _) = fs.mkdir(fs.root(), "dir").await.unwrap();
        let (subdir_ino, _) = fs.mkdir(dir_ino, "subdir").await.unwrap();
        let (file_ino, _) = fs.create_file(subdir_ino, "file", false).await.unwrap();
        fs.write(file_ino, 0, b"original").await.unwrap();

        // First snapshot — cleans the entire tree.
        let snap1 = fs.snapshot().await.unwrap();
        assert_ne!(snap1, empty_tree_id(&store));

        // Modify the deep file after snapshot has cleaned everything.
        // This is the exact scenario Bug 2 broke: the root is clean
        // Tree, and the write needs to dirty root -> dir -> subdir.
        fs.write(file_ino, 0, b"modified").await.unwrap();

        // Second snapshot must pick up the modification.
        let snap2 = fs.snapshot().await.unwrap();
        assert_ne!(snap2, snap1, "snapshot must detect the deep write");

        // Walk the tree to verify the content is "modified".
        let root_tree = read_tree(&store, snap2);
        let dir_entry = find_entry(&root_tree, "dir");
        let dir_id = entry_id(dir_entry);
        let dir_tree = read_tree(&store, dir_id);
        let subdir_entry = find_entry(&dir_tree, "subdir");
        let subdir_id = entry_id(subdir_entry);
        let subdir_tree = read_tree(&store, subdir_id);
        let file_entry = find_entry(&subdir_tree, "file");
        let file_id = entry_id(file_entry);
        let content = read_file_content(&store, file_id);
        assert_eq!(content, b"modified");
    }

    /// Regression test for Bug 2 (variant): `remove` after snapshot.
    ///
    /// Same root cause — `remove` calls `ensure_dirty_tree` on the
    /// parent, which must propagate up to all ancestors. Without
    /// propagation, the removal is invisible to the next snapshot.
    #[tokio::test]
    async fn remove_after_snapshot_deep_in_tree_is_visible() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        let (dir_ino, _) = fs.mkdir(fs.root(), "dir").await.unwrap();
        let (file_ino, _) = fs.create_file(dir_ino, "inner", false).await.unwrap();
        fs.write(file_ino, 0, b"data").await.unwrap();

        let snap1 = fs.snapshot().await.unwrap();

        // Remove the file after snapshot cleaned everything.
        fs.remove(dir_ino, "inner").await.unwrap();

        let snap2 = fs.snapshot().await.unwrap();
        assert_ne!(snap2, snap1, "snapshot must detect the removal");

        // The dir tree should now be empty.
        let root_tree = read_tree(&store, snap2);
        let dir_entry = find_entry(&root_tree, "dir");
        let dir_id = entry_id(dir_entry);
        let dir_tree = read_tree(&store, dir_id);
        assert!(
            dir_tree.is_empty(),
            "dir should be empty after removing its only child"
        );
    }

    /// Regression test for Bug 2 (variant): `setattr` (truncate) after
    /// snapshot must also propagate dirty state to ancestors.
    #[tokio::test]
    async fn setattr_after_snapshot_is_visible() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        let (dir_ino, _) = fs.mkdir(fs.root(), "dir").await.unwrap();
        let (file_ino, _) = fs.create_file(dir_ino, "f", false).await.unwrap();
        fs.write(file_ino, 0, b"hello").await.unwrap();

        let snap1 = fs.snapshot().await.unwrap();

        // Truncate the file after snapshot.
        fs.setattr(file_ino, Some(0), None).await.unwrap();

        let snap2 = fs.snapshot().await.unwrap();
        assert_ne!(snap2, snap1, "snapshot must detect the truncation");

        // Verify the file is now empty.
        let root_tree = read_tree(&store, snap2);
        let dir_entry = find_entry(&root_tree, "dir");
        let dir_id = entry_id(dir_entry);
        let dir_tree = read_tree(&store, dir_id);
        let file_entry = find_entry(&dir_tree, "f");
        let file_id = entry_id(file_entry);
        let content = read_file_content(&store, file_id);
        assert!(content.is_empty());
    }

    /// Regression test for Bug 3: writing to a child of an existing
    /// (store-backed) tree, then snapshotting, must capture the write.
    ///
    /// The write calls `ensure_dirty_file(ino)` (promoting the clean
    /// `File` to `DirtyFile`) and then `ensure_dirty_tree(parent)`
    /// (materializing the parent). Before the fix, materialization
    /// would refresh *all* existing children from the store, clobbering
    /// the just-promoted DirtyFile with its clean predecessor.
    #[tokio::test]
    async fn write_to_existing_tree_child_survives_materialization() {
        let (store, root_id) = build_synthetic_tree();
        let fs = KikiFs::new(store.clone(), root_id, None, None, None, 0);

        // Look up the existing "hello.txt" (clean File from the store).
        let hello_ino = fs.lookup(fs.root(), "hello.txt").await.unwrap();

        // Write new content. Internally this does:
        //   ensure_dirty_file(hello_ino) -> DirtyFile
        //   ensure_dirty_tree(root)      -> materialize root
        // Before the fix, materializing root would overwrite hello_ino
        // back to its clean File variant.
        fs.write(hello_ino, 0, b"new content").await.unwrap();

        // Read back — must see the new content, not the original "hi\n".
        let (data, _) = fs.read(hello_ino, 0, 1024).await.unwrap();
        assert_eq!(data, b"new content");

        // Snapshot must capture the modified content.
        let snap = fs.snapshot().await.unwrap();
        assert_ne!(snap, root_id);
        let root_tree = read_tree(&store, snap);
        let entry = find_entry(&root_tree, "hello.txt");
        let file_id = entry_id(entry);
        let content = read_file_content(&store, file_id);
        assert_eq!(content, b"new content");
    }

    // ----- Editor atomic-save pattern test ---------------------------------

    /// Editors typically save via write(tmp) -> rename(tmp, real). This
    /// exercises that pattern: create "foo.rs" with original content,
    /// snapshot, then simulate an editor save by writing to "foo.rs.tmp"
    /// and renaming over "foo.rs". The final snapshot must contain only
    /// "foo.rs" with the new content.
    #[tokio::test]
    async fn rename_atomic_save_pattern() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        // Original file.
        let (orig_ino, _) = fs.create_file(fs.root(), "foo.rs", false).await.unwrap();
        fs.write(orig_ino, 0, b"fn main() {}").await.unwrap();
        let snap1 = fs.snapshot().await.unwrap();

        // Editor writes to a tmp file.
        let (tmp_ino, _) = fs
            .create_file(fs.root(), "foo.rs.tmp", false)
            .await
            .unwrap();
        fs.write(tmp_ino, 0, b"fn main() { println!(\"hello\"); }")
            .await
            .unwrap();

        // Atomic rename: foo.rs.tmp -> foo.rs (overwrites original).
        fs.rename(fs.root(), "foo.rs.tmp", fs.root(), "foo.rs")
            .await
            .unwrap();

        // "foo.rs.tmp" should be gone.
        let err = fs.lookup(fs.root(), "foo.rs.tmp").await.unwrap_err();
        assert_eq!(err, FsError::NotFound);

        // "foo.rs" should have the new content.
        let new_ino = fs.lookup(fs.root(), "foo.rs").await.unwrap();
        let (data, _) = fs.read(new_ino, 0, 4096).await.unwrap();
        assert_eq!(data, b"fn main() { println!(\"hello\"); }");

        // Snapshot captures the rename.
        let snap2 = fs.snapshot().await.unwrap();
        assert_ne!(snap2, snap1);
        let tree = read_tree(&store, snap2);
        // Only "foo.rs" should be present (no "foo.rs.tmp").
        let names: Vec<&str> = tree.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["foo.rs"]);
        let file_id = entry_id(&tree[0]);
        let content = read_file_content(&store, file_id);
        assert_eq!(content, b"fn main() { println!(\"hello\"); }");
    }

    /// Bug: rename doesn't update the child inode's `parent` pointer.
    ///
    /// After renaming `/a/file` to `/b/file`, the file's `Inode.parent`
    /// still points at `/a/`. A subsequent write after snapshot dirties
    /// the wrong ancestor chain (through `/a/` instead of `/b/`), so
    /// snapshot misses the modification.
    #[tokio::test]
    async fn write_to_cross_dir_renamed_file_after_snapshot() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        // Create /a/file and /b/.
        let (a_ino, _) = fs.mkdir(fs.root(), "a").await.unwrap();
        let (b_ino, _) = fs.mkdir(fs.root(), "b").await.unwrap();
        let (file_ino, _) = fs.create_file(a_ino, "file", false).await.unwrap();
        fs.write(file_ino, 0, b"v1").await.unwrap();

        // Snapshot — cleans everything.
        let snap1 = fs.snapshot().await.unwrap();

        // Rename /a/file -> /b/file.
        fs.rename(a_ino, "file", b_ino, "file").await.unwrap();

        // Snapshot — captures the rename.
        let snap2 = fs.snapshot().await.unwrap();
        assert_ne!(snap2, snap1);

        // Now write to the renamed file. This is the bug trigger:
        // write() reads inode.parent to dirty ancestors. If parent
        // still points at /a/ (stale), the dirty propagation goes
        // up through /a/ instead of /b/, and /b/ stays clean.
        fs.write(file_ino, 0, b"v2").await.unwrap();

        let snap3 = fs.snapshot().await.unwrap();
        assert_ne!(snap3, snap2, "snapshot must detect write to renamed file");

        // Verify /b/file has "v2".
        let root_tree = read_tree(&store, snap3);
        let b_entry = find_entry(&root_tree, "b");
        let b_id = entry_id(b_entry);
        let b_tree = read_tree(&store, b_id);
        let file_entry = find_entry(&b_tree, "file");
        let file_id = entry_id(file_entry);
        let content = read_file_content(&store, file_id);
        assert_eq!(content, b"v2");
    }

    /// After a cross-directory rename, creating a new file at the old
    /// name must succeed and must not reuse the moved inode. This
    /// verifies that the `by_parent` reverse map is correctly cleaned
    /// after rename (detach_child removes the old entry).
    #[tokio::test]
    async fn create_at_old_name_after_rename() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        let (a_ino, _) = fs.mkdir(fs.root(), "a").await.unwrap();
        let (b_ino, _) = fs.mkdir(fs.root(), "b").await.unwrap();
        let (old_file, _) = fs.create_file(a_ino, "f", false).await.unwrap();
        fs.write(old_file, 0, b"old").await.unwrap();

        // Rename /a/f -> /b/f.
        fs.rename(a_ino, "f", b_ino, "f").await.unwrap();

        // Create a new file at the old name /a/f.
        let (new_file, _) = fs.create_file(a_ino, "f", false).await.unwrap();
        assert_ne!(
            new_file, old_file,
            "new file must get a distinct inode from the renamed one"
        );
        fs.write(new_file, 0, b"new").await.unwrap();

        // Both files should be readable with correct content.
        let (data, _) = fs.read(old_file, 0, 1024).await.unwrap();
        assert_eq!(data, b"old");
        let (data, _) = fs.read(new_file, 0, 1024).await.unwrap();
        assert_eq!(data, b"new");

        // Snapshot should capture both.
        let snap = fs.snapshot().await.unwrap();
        let root_tree = read_tree(&store, snap);
        assert_eq!(root_tree.len(), 2, "root should have a/ and b/");
    }

    /// Same-directory rename (the common editor case) followed by write.
    /// Even though old_parent == new_parent, the child's name field
    /// should be updated so that readdir/debug output stays consistent.
    #[tokio::test]
    async fn same_dir_rename_then_write() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        let (file_ino, _) = fs.create_file(fs.root(), "old_name", false).await.unwrap();
        fs.write(file_ino, 0, b"content").await.unwrap();
        let snap1 = fs.snapshot().await.unwrap();

        // Rename in the same directory.
        fs.rename(fs.root(), "old_name", fs.root(), "new_name")
            .await
            .unwrap();

        // Write to the renamed file.
        fs.write(file_ino, 0, b"updated").await.unwrap();

        let snap2 = fs.snapshot().await.unwrap();
        assert_ne!(snap2, snap1);

        let tree = read_tree(&store, snap2);
        let names: Vec<&str> = tree.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["new_name"]);
        let file_id = entry_id(&tree[0]);
        let content = read_file_content(&store, file_id);
        assert_eq!(content, b"updated");
    }

    /// Variant: editor atomic-save inside a subdirectory. Exercises the
    /// ancestor-dirty propagation together with rename.
    #[tokio::test]
    async fn rename_atomic_save_in_subdirectory() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        let (src_ino, _) = fs.mkdir(fs.root(), "src").await.unwrap();
        let (orig_ino, _) = fs.create_file(src_ino, "lib.rs", false).await.unwrap();
        fs.write(orig_ino, 0, b"// v1").await.unwrap();
        let snap1 = fs.snapshot().await.unwrap();

        // Editor writes tmp and renames inside the subdirectory.
        let (tmp_ino, _) = fs.create_file(src_ino, ".lib.rs.swp", false).await.unwrap();
        fs.write(tmp_ino, 0, b"// v2").await.unwrap();
        fs.rename(src_ino, ".lib.rs.swp", src_ino, "lib.rs")
            .await
            .unwrap();

        // Snapshot must pick up the change despite the root being clean
        // after snap1.
        let snap2 = fs.snapshot().await.unwrap();
        assert_ne!(snap2, snap1, "rename in subdir must dirty ancestors");

        // Walk tree: root -> src -> lib.rs
        let root_tree = read_tree(&store, snap2);
        let src_entry = find_entry(&root_tree, "src");
        let src_id = entry_id(src_entry);
        let src_tree = read_tree(&store, src_id);
        let names: Vec<&str> = src_tree.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["lib.rs"], "swap file should not persist");
        let file_id = entry_id(&src_tree[0]);
        let content = read_file_content(&store, file_id);
        assert_eq!(content, b"// v2");
    }

    // ----- Multi-cycle and check_out tests ---------------------------------

    /// Three snapshot cycles with modifications between each.
    /// Verifies the snapshot->clean->modify->snapshot cycle is reliable
    /// when repeated.
    #[tokio::test]
    async fn three_snapshot_cycles() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        let (file_ino, _) = fs.create_file(fs.root(), "counter", false).await.unwrap();
        fs.write(file_ino, 0, b"1").await.unwrap();
        let snap1 = fs.snapshot().await.unwrap();

        fs.write(file_ino, 0, b"2").await.unwrap();
        let snap2 = fs.snapshot().await.unwrap();
        assert_ne!(snap2, snap1);

        fs.write(file_ino, 0, b"3").await.unwrap();
        let snap3 = fs.snapshot().await.unwrap();
        assert_ne!(snap3, snap2);
        assert_ne!(snap3, snap1);

        // All three snapshots should have distinct content.
        let tree3 = read_tree(&store, snap3);
        let entry = find_entry(&tree3, "counter");
        let fid = entry_id(entry);
        let content = read_file_content(&store, fid);
        assert_eq!(content, b"3");
    }

    /// Modify a file, check_out to a different tree, then modify
    /// again. The second modification must be against the new tree,
    /// not the old one.
    #[tokio::test]
    async fn modify_after_check_out_to_different_tree() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let fs = KikiFs::new(store.clone(), empty_tree_id(&store), None, None, None, 0);

        // Build tree A: /file with "aaa".
        let (file_ino, _) = fs.create_file(fs.root(), "file", false).await.unwrap();
        fs.write(file_ino, 0, b"aaa").await.unwrap();
        let _tree_a = fs.snapshot().await.unwrap();

        // Build tree B externally: /file with "bbb", /extra with "xxx".
        let file_b_id = store.write_file(b"bbb").unwrap();
        let extra_id = store.write_file(b"xxx").unwrap();
        let tree_b_bytes = store
            .write_tree(&[
                GitTreeEntry {
                    name: "extra".into(),
                    kind: GitEntryKind::File { executable: false },
                    id: extra_id,
                },
                GitTreeEntry {
                    name: "file".into(),
                    kind: GitEntryKind::File { executable: false },
                    id: file_b_id,
                },
            ])
            .unwrap();
        let tree_b_id = vec_to_id(&tree_b_bytes);

        // Check out to tree B.
        fs.check_out(tree_b_id, 0).await.unwrap();

        // The old file_ino is orphaned — look up fresh.
        let new_file_ino = fs.lookup(fs.root(), "file").await.unwrap();
        let (data, _) = fs.read(new_file_ino, 0, 1024).await.unwrap();
        assert_eq!(data, b"bbb");

        // extra should be visible.
        let extra_ino = fs.lookup(fs.root(), "extra").await.unwrap();
        let (data, _) = fs.read(extra_ino, 0, 1024).await.unwrap();
        assert_eq!(data, b"xxx");

        // Modify /file after check_out.
        fs.write(new_file_ino, 0, b"ccc").await.unwrap();
        let snap = fs.snapshot().await.unwrap();
        assert_ne!(snap, tree_b_id);

        // Snapshot should have /file="ccc" and /extra="xxx".
        let tree = read_tree(&store, snap);
        assert_eq!(tree.len(), 2);
        let file_entry = find_entry(&tree, "file");
        let fid = entry_id(file_entry);
        let content = read_file_content(&store, fid);
        assert_eq!(content, b"ccc");
    }

    /// Create a file inside a subdirectory of a pre-existing tree.
    /// This exercises materialization of a clean subtree for mutation.
    #[tokio::test]
    async fn create_file_in_existing_subdirectory() {
        let (store, root_id) = build_synthetic_tree();
        let fs = KikiFs::new(store.clone(), root_id, None, None, None, 0);

        // Look up the existing "bin/" directory.
        let bin_ino = fs.lookup(fs.root(), "bin").await.unwrap();

        // Create a new file inside bin/ alongside the existing "tool".
        let (new_file, _) = fs.create_file(bin_ino, "helper", false).await.unwrap();
        fs.write(new_file, 0, b"helper script").await.unwrap();

        // Existing "tool" should still be readable.
        let tool_ino = fs.lookup(bin_ino, "tool").await.unwrap();
        let (data, _) = fs.read(tool_ino, 0, 1024).await.unwrap();
        assert_eq!(data, b"x");

        // Snapshot should capture the new file alongside the old one.
        let snap = fs.snapshot().await.unwrap();
        assert_ne!(snap, root_id);
        let root_tree = read_tree(&store, snap);
        let bin_entry = find_entry(&root_tree, "bin");
        let bin_id = entry_id(bin_entry);
        let bin_tree = read_tree(&store, bin_id);
        let names: Vec<&str> = bin_tree.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["helper", "tool"]);
    }

    /// Delete a file from a pre-existing (store-backed) subtree after
    /// snapshot. Verifies ancestor-dirty propagation through
    /// materialized trees.
    #[tokio::test]
    async fn delete_from_existing_subtree_after_snapshot() {
        let (store, root_id) = build_synthetic_tree();
        let fs = KikiFs::new(store.clone(), root_id, None, None, None, 0);

        // Snapshot should be a no-op on a clean tree.
        let snap1 = fs.snapshot().await.unwrap();
        assert_eq!(snap1, root_id);

        // Delete "hello.txt" from the root.
        fs.remove(fs.root(), "hello.txt").await.unwrap();

        let snap2 = fs.snapshot().await.unwrap();
        assert_ne!(snap2, root_id, "snapshot must detect deletion");

        let tree = read_tree(&store, snap2);
        let names: Vec<&str> = tree.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["bin", "link"], "hello.txt should be gone");
    }

    // ---- M10.7: gitignore + redirections tests ----

    /// Build a store + root tree that contains a `.gitignore` with the
    /// given content. Returns `(store, root_tree_id)`.
    fn build_tree_with_gitignore(gitignore_content: &str) -> (Arc<GitContentStore>, Id) {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let gi_id = store.write_file(gitignore_content.as_bytes()).unwrap();
        let entries = vec![GitTreeEntry {
            name: ".gitignore".into(),
            kind: GitEntryKind::File { executable: false },
            id: gi_id,
        }];
        let root_id = store.write_tree(&entries).unwrap();
        (store, vec_to_id(&root_id))
    }

    #[tokio::test]
    async fn ignored_file_skipped_in_snapshot() {
        let (store, root) = build_tree_with_gitignore("*.log\nnode_modules/\n");
        let fs = KikiFs::new(store.clone(), root, None, None, None, 0);
        // Create an ignored file at root level.
        fs.create_file(fs.root(), "debug.log", false)
            .await
            .unwrap();
        // Create a tracked file too.
        fs.create_file(fs.root(), "app.js", false).await.unwrap();
        fs.write(
            fs.lookup(fs.root(), "app.js").await.unwrap(),
            0,
            b"hello",
        )
        .await
        .unwrap();
        let snap = fs.snapshot().await.unwrap();
        let tree = read_tree(&store, snap);
        let names: Vec<&str> = tree.iter().map(|e| e.name.as_str()).collect();
        // .gitignore is from the checked-out tree; app.js was created.
        // debug.log is ignored → not in the tree.
        assert!(names.contains(&".gitignore"));
        assert!(names.contains(&"app.js"));
        assert!(!names.contains(&"debug.log"), "ignored file should not be in snapshot");
    }

    #[tokio::test]
    async fn ignored_file_still_readable() {
        let (store, root) = build_tree_with_gitignore("*.log\n");
        let fs = KikiFs::new(store, root, None, None, None, 0);
        fs.create_file(fs.root(), "debug.log", false)
            .await
            .unwrap();
        let ino = fs.lookup(fs.root(), "debug.log").await.unwrap();
        fs.write(ino, 0, b"some log data").await.unwrap();
        // The file is ignored but still readable through the VFS.
        let (data, _) = fs.read(ino, 0, 1024).await.unwrap();
        assert_eq!(data, b"some log data");
    }

    #[tokio::test]
    async fn ignored_dir_children_inherit() {
        let (store, root) = build_tree_with_gitignore("node_modules/\n");
        let fs = KikiFs::new(store.clone(), root, None, None, None, 0);
        fs.mkdir(fs.root(), "node_modules").await.unwrap();
        let nm = fs.lookup(fs.root(), "node_modules").await.unwrap();
        // Create a file inside node_modules.
        fs.create_file(nm, "lodash.js", false).await.unwrap();
        // Create a nested subdir.
        fs.mkdir(nm, "express").await.unwrap();
        let express = fs.lookup(nm, "express").await.unwrap();
        fs.create_file(express, "index.js", false).await.unwrap();
        let snap = fs.snapshot().await.unwrap();
        let tree = read_tree(&store, snap);
        let names: Vec<&str> = tree.iter().map(|e| e.name.as_str()).collect();
        // node_modules and everything inside it should be excluded.
        assert!(!names.contains(&"node_modules"), "ignored dir should not be in snapshot");
    }

    #[tokio::test]
    async fn already_tracked_file_not_ignored() {
        // Build a tree that contains `vendor/lib.js`.
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let lib_id = store.write_file(b"lib").unwrap();
        let vendor_entries = vec![GitTreeEntry {
            name: "lib.js".into(),
            kind: GitEntryKind::File { executable: false },
            id: lib_id,
        }];
        let vendor_tree = store.write_tree(&vendor_entries).unwrap();
        // Root tree with vendor/ and .gitignore that ignores vendor/.
        let gi_id = store.write_file(b"vendor/\n").unwrap();
        let root_entries = vec![
            GitTreeEntry {
                name: ".gitignore".into(),
                kind: GitEntryKind::File { executable: false },
                id: gi_id,
            },
            GitTreeEntry {
                name: "vendor".into(),
                kind: GitEntryKind::Tree,
                id: vendor_tree,
            },
        ];
        let root_id = store.write_tree(&root_entries).unwrap();
        let root = vec_to_id(&root_id);

        let fs = KikiFs::new(store.clone(), root, None, None, None, 0);
        // vendor/ comes from check_out → ignored: false (already tracked).
        let vendor = fs.lookup(fs.root(), "vendor").await.unwrap();
        let lib = fs.lookup(vendor, "lib.js").await.unwrap();
        // Modify the tracked file.
        fs.write(lib, 0, b"modified lib").await.unwrap();
        let snap = fs.snapshot().await.unwrap();
        let tree = read_tree(&store, snap);
        let names: Vec<&str> = tree.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"vendor"), "already-tracked dir must remain in snapshot");
    }

    #[tokio::test]
    async fn gitignore_hot_reload() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        let empty = empty_tree_id(&store);
        let fs = KikiFs::new(store.clone(), empty, None, None, None, 0);

        // Create a file that will become ignored after .gitignore write.
        fs.create_file(fs.root(), "tmp.log", false).await.unwrap();
        let snap1 = fs.snapshot().await.unwrap();
        let tree1 = read_tree(&store, snap1);
        assert!(
            tree1.iter().any(|e| e.name == "tmp.log"),
            "before .gitignore, tmp.log should be in snapshot"
        );

        // Write a .gitignore that ignores *.log.
        fs.create_file(fs.root(), ".gitignore", false)
            .await
            .unwrap();
        let gi = fs.lookup(fs.root(), ".gitignore").await.unwrap();
        fs.write(gi, 0, b"*.log\n").await.unwrap();

        // Create another .log file — should now be ignored.
        fs.create_file(fs.root(), "other.log", false)
            .await
            .unwrap();

        let snap2 = fs.snapshot().await.unwrap();
        let tree2 = read_tree(&store, snap2);
        let names: Vec<&str> = tree2.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&".gitignore"));
        // tmp.log was created before the .gitignore → not ignored
        // (already in the slab with ignored: false).
        assert!(names.contains(&"tmp.log"));
        // other.log was created after the .gitignore → ignored.
        assert!(!names.contains(&"other.log"), "post-gitignore file should be ignored");
    }

    #[tokio::test]
    async fn negation_pattern() {
        let (store, root) = build_tree_with_gitignore("*.log\n!important.log\n");
        let fs = KikiFs::new(store.clone(), root, None, None, None, 0);
        fs.create_file(fs.root(), "debug.log", false)
            .await
            .unwrap();
        fs.create_file(fs.root(), "important.log", false)
            .await
            .unwrap();
        let snap = fs.snapshot().await.unwrap();
        let tree = read_tree(&store, snap);
        let names: Vec<&str> = tree.iter().map(|e| e.name.as_str()).collect();
        assert!(!names.contains(&"debug.log"), "debug.log should be ignored");
        assert!(names.contains(&"important.log"), "!important.log negation should keep it");
    }

    #[tokio::test]
    async fn redirections_creates_symlink() {
        let settings = test_settings();
        let store = Arc::new(GitContentStore::new_in_memory(&settings));
        // Build a tree with .kiki-redirections.
        let redir_id = store.write_file(b"node_modules\n").unwrap();
        let entries = vec![GitTreeEntry {
            name: KIKI_REDIRECTIONS_FILE.into(),
            kind: GitEntryKind::File { executable: false },
            id: redir_id,
        }];
        let root_id = store.write_tree(&entries).unwrap();
        let root = vec_to_id(&root_id);

        let scratch = tempfile::tempdir().expect("scratch tmpdir");
        let fs = KikiFs::new(store.clone(), root, None, Some(scratch.path().to_owned()), None, 0);

        // mkdir("node_modules") should create a symlink, not a dir.
        fs.mkdir(fs.root(), "node_modules").await.unwrap();
        let ino = fs.lookup(fs.root(), "node_modules").await.unwrap();
        let attr = fs.getattr(ino).await.unwrap();
        assert_eq!(attr.kind, FileKind::Symlink, "redirected dir should be a symlink");

        // The symlink target should point into the scratch dir.
        let target = fs.readlink(ino).await.unwrap();
        assert!(
            target.starts_with(scratch.path().to_str().unwrap()),
            "symlink should point into scratch dir, got: {target}"
        );

        // The scratch dir should actually exist on disk.
        let target_path = std::path::Path::new(&target);
        assert!(target_path.is_dir(), "scratch dir should exist on disk");

        // Snapshot should not include the redirected entry.
        let snap = fs.snapshot().await.unwrap();
        let tree = read_tree(&store, snap);
        let names: Vec<&str> = tree.iter().map(|e| e.name.as_str()).collect();
        assert!(!names.contains(&"node_modules"), "redirected dir should not be in snapshot");
    }

    #[tokio::test]
    async fn parse_redirections_format() {
        let content = b"# build outputs\ntarget\n\nnode_modules/\n  .venv  \n";
        let result = KikiFs::parse_redirections(content);
        assert_eq!(result, vec!["target", "node_modules", ".venv"]);
    }
}
