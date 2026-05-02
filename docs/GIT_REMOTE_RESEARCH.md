# Git Remote Support for kiki

Research and design notes for adding GitHub/git remote support to kiki.

## Problem statement

kiki uses a custom `KikiBackend` with its own object format (prost-encoded,
BLAKE3-addressed, stored in redb). It has no connection to git's object model.
This means kiki repos can't push to or fetch from GitHub or any git remote.

The goal is to enable a workflow where:

- **`grpc://`** remains the primary real-time sync channel for local and
  internal development (write-through, op-log sync, peer-to-peer).
- **Git remotes** (GitHub, GitLab, etc.) are a batch sync target for public
  open-source contributions — same UX as `jj git push` / `jj git fetch`.

## Architecture: shadow git repo

Rather than reimplementing git object translation, we maintain a **shadow git
repo** per mount and use jj-lib's existing public API (`git::push_refs`,
`GitFetch`, `import_refs`, `export_refs`) to handle all git protocol and object
translation.

```
daemon storage_dir/mounts/<hash>/
├── store.redb          # existing kiki store (primary, BLAKE3)
├── git/                # bare git repo (shadow, SHA-1)         ← NEW
└── git-jj/             # jj metadata for the shadow git repo   ← NEW
    └── .jj/repo/
        ├── store/      # points at git/ via GitBackend
        ├── op_store/   # SimpleOpStore (local-only, never synced)
        ├── op_heads/   # SimpleOpHeadsStore
        └── index/      # DefaultIndexStore
```

### Why a shadow repo works

jj-lib's `GitBackend` implements the `Backend` trait. When you call
`Store::write_commit` on a `GitBackend`-backed repo, it automatically:

- Encodes the change-id as a git commit header (`change-id`)
- Encodes predecessors into the extras protobuf side-table
- Encodes conflict trees as `.jjconflict-base-N` / `.jjconflict-side-N` subtrees
  plus a `jj:trees` commit header
- Converts jj `Signature` to git signature format
- Produces a git object with a SHA-1 ID
- Creates a `refs/jj/keep/<hex>` no-GC ref
- Handles hash-collision avoidance (adjusts committer timestamp if SHA-1 collides)

And `Store::get_commit` does the reverse. All the translation logic is inside
jj-lib — we never touch git objects directly.

### Why not implement `RemoteStore` for git

The `RemoteStore` trait is designed for real-time write-through — every
`put_blob` blocks the gRPC RPC. That works for `grpc://localhost:12000` or
`dir:///tmp/shared`. It would be terrible for `https://github.com/user/repo`
on every file write. Batch push/fetch on demand is the right model for
network git remotes.

### The shadow repo's operation log

The shadow repo **requires** a jj operation log (`SimpleOpStore`,
`SimpleOpHeadsStore`, `DefaultIndexStore`) because jj-lib's transaction model
demands it — you can't obtain a `MutableRepo` without a full `ReadonlyRepo`,
and every API we need (`push_refs`, `GitFetch`, `export_refs`, `import_refs`)
takes `&mut MutableRepo`.

However, this op log is **purely local bookkeeping**. It is never synced to
the git remote or to the kiki remote. Operations only flow through the native
`grpc://` / `dir://` channel. The shadow repo's op log tracks its own state
transitions (each push/fetch) and nothing else. This matches standard jj
behavior — `jj git push` has never synced the operation log.

## Detailed design

### Full object graph translation (critical finding)

The design's central challenge is **not** just commit ID rewriting — it's
translating the entire object graph. `GitBackend::write_commit` expects
every referenced tree and blob to **already exist** in the git object store
with valid 20-byte SHA-1 IDs. A kiki `TreeId` is 32 bytes (BLAKE3) and will
fail `validate_git_object_id`.

The translation is a recursive walk:

```
blob (file/symlink)  →  read from kiki redb
                     →  write to git ODB via Store::write_file
                     →  get SHA-1 FileId, record mapping

tree (bottom-up)     →  read from kiki redb
                     →  rewrite every child entry ID (file→SHA-1, subtree→SHA-1)
                     →  write to git ODB via Store::write_tree
                     →  get SHA-1 TreeId, record mapping

commit               →  rewrite parents, predecessors, root_tree IDs
                     →  call Store::write_commit
                     →  get SHA-1 CommitId, record mapping
```

Order matters: blobs first, then trees bottom-up (leaves before parents),
then commits bottom-up (parents before children). Each layer depends on
the mapping established by the layer below.

### ID mapping tables

Six redb tables provide bidirectional mapping for three object types:

```rust
// BLAKE3 (kiki) → SHA-1 (git)
const GIT_COMMIT_MAP:  TableDefinition<&[u8; 32], &[u8; 20]> = TableDefinition::new("git_commit_map_v1");
const GIT_TREE_MAP:    TableDefinition<&[u8; 32], &[u8; 20]> = TableDefinition::new("git_tree_map_v1");
const GIT_BLOB_MAP:    TableDefinition<&[u8; 32], &[u8; 20]> = TableDefinition::new("git_blob_map_v1");

// SHA-1 (git) → BLAKE3 (kiki), for the fetch direction
const GIT_COMMIT_MAP_REV: TableDefinition<&[u8; 20], &[u8; 32]> = TableDefinition::new("git_commit_map_rev_v1");
const GIT_TREE_MAP_REV:   TableDefinition<&[u8; 20], &[u8; 32]> = TableDefinition::new("git_tree_map_rev_v1");
const GIT_BLOB_MAP_REV:   TableDefinition<&[u8; 20], &[u8; 32]> = TableDefinition::new("git_blob_map_rev_v1");
```

Alternative: a single unified table with an object-type discriminator byte
prefix. Six tables is simpler and avoids key encoding overhead.

Seeded with the root commit mapping on first push or fetch
(`[0u8; 32]` ↔ `[0u8; 20]`). The root commit is handled specially by
`GitBackend` — it drops root-commit parents (making the git commit
parentless) on write, and injects `root_commit_id` on read.

The mapping is rebuildable: walk all git objects, re-hash content as
BLAKE3 via the kiki store, and reconstruct.

### Push flow (`cli kk git push`)

```
1. CLI sends GitPush RPC to daemon (with remote name, bookmark patterns)

2. Daemon opens/loads shadow repo, starts transaction → MutableRepo

3. Walk kiki commit graph — ancestors of bookmarked heads, bottom-up:

   For each commit not already in the mapping table:

   a. Copy blobs:
      - Read kiki root tree, recurse into subtrees
      - For each file/symlink entry not in blob mapping:
        read content from kiki store, write via shadow Store::write_file
      - Record BLAKE3 ↔ SHA-1 blob mapping

   b. Copy trees (bottom-up, leaves first):
      - Read kiki tree entries, translate child IDs via mapping
      - Write via shadow Store::write_tree
      - Record BLAKE3 ↔ SHA-1 tree mapping

   c. Copy commit:
      - Read backend::Commit from kiki redb
      - Rewrite parent/predecessor CommitIds: BLAKE3 → SHA-1
      - Rewrite root_tree Merge<TreeId>: BLAKE3 → SHA-1
      - Write via shadow Store::write_commit
        (GitBackend handles change-id header, extras protobuf,
         conflict encoding, no-GC ref — all automatically)
      - Record BLAKE3 ↔ SHA-1 commit mapping

4. Set bookmarks in MutableRepo:
   mut_repo.set_local_bookmark_target(name, target)

5. push_refs(mut_repo, subprocess_options, remote, &targets, &callback, &options)
   — internally calls export_refs_to_git (no need to call export_refs first)
   — runs git push subprocess (SSH/HTTPS auth from user's environment)
   — updates mut_repo's remote-tracking bookmark state on success

6. Commit transaction: tx.commit("push to <remote>")
```

### Fetch flow (`cli kk git fetch`)

```
1. CLI sends GitFetch RPC to daemon

2. Daemon opens/loads shadow repo, starts transaction → MutableRepo

3. Fetch from git remote:
   a. GitFetch::new(mut_repo, subprocess_options, &import_options)
   b. git_fetch.fetch(remote, refspecs, &mut callback, None, None)
      — runs git fetch subprocess, updates git's refs/remotes/
      — does NOT update jj's view yet
   c. git_fetch.import_refs().await
      — diffs git refs against jj's view
      — imports commit objects into jj's backend
      — updates remote bookmark tracking state
      — merges into local bookmarks

4. Walk new commits in shadow repo (ones not yet in mapping table):

   For each new commit (reverse of push, git→kiki):

   a. Copy blobs (git→kiki):
      - Read git blobs via shadow Store::read_file
      - Write to kiki redb store
      - Record SHA-1 ↔ BLAKE3 blob mapping

   b. Copy trees bottom-up (git→kiki):
      - Read git tree entries, translate child IDs via mapping
      - Write to kiki redb store
      - Record SHA-1 ↔ BLAKE3 tree mapping

   c. Copy commit (git→kiki):
      - Read backend::Commit via shadow Store::get_commit
        (GitBackend handles git→jj translation: change-id from header,
         predecessors from extras, conflicts from jj:trees header)
      - Rewrite parent/predecessor/root_tree IDs: SHA-1 → BLAKE3
      - Write to kiki redb store
      - Record SHA-1 ↔ BLAKE3 commit mapping

5. Sync bookmarks/tags from shadow repo's view into kiki's catalog refs

6. Commit transaction: tx.commit("fetch from <remote>")
```

### Deduplication and incremental sync

Subsequent pushes/fetches only process objects not already in the mapping
table. The mapping table serves as the "already synced" set. For trees and
blobs, this means checking the mapping before copying — shared subtrees
across commits are copied once.

For the initial push of a large repo, the full graph walk could be slow.
The incremental approach handles this naturally after the first sync.

### Shadow repo initialization

One-time setup when a git remote is first configured:

```rust
// Option A: Full workspace (includes working copy — not needed but harmless)
let (workspace, repo) = Workspace::init_internal_git(&settings, &shadow_path).await?;

// Option B: Repo only, no workspace (cleaner for headless shadow use)
let repo = ReadonlyRepo::init(
    &settings,
    &repo_dir,
    &|settings, store_path| Ok(Box::new(GitBackend::init_internal(settings, store_path)?)),
    Signer::from_settings(&settings)?,
    ReadonlyRepo::default_op_store_initializer(),
    ReadonlyRepo::default_op_heads_store_initializer(),
    ReadonlyRepo::default_index_store_initializer(),
    ReadonlyRepo::default_submodule_store_initializer(),
).await?;
```

Option B is preferred — creates `repo_dir/store/git/` (bare git repo) plus
jj metadata, no working copy directory.

Reopening on subsequent operations:

```rust
let repo = ReadonlyRepo::loader(&settings, &repo_dir, &StoreFactories::default())?
    .load_at_head()?;
let mut tx = repo.start_transaction();
let mut_repo = tx.repo_mut();
```

### Remote management

Remotes are stored in the shadow git repo's config file (standard git format):

```rust
git::add_remote(
    tx.repo_mut(),
    &remote_name,       // e.g. "origin"
    "https://github.com/user/repo.git",
    None,               // push_url (optional)
    gix::remote::fetch::Tags::Included,
    &StringExpression::glob("*"),  // bookmark pattern
)?;
tx.commit("add remote").await?;
```

### Ref and bookmark mapping

Kiki's `RemoteStore` ref namespace is flat (no `/` allowed by
`validate_ref_name`). Git refs are hierarchical. The shadow repo handles this
internally — jj-lib's `export_refs` / `import_refs` manage the
`refs/heads/<bookmark>` ↔ jj bookmark mapping.

`export_refs` syncs jj's bookmark/tag state → git ref namespace by diffing
`mut_repo.view()` against remembered git ref state. It writes
`refs/heads/<bookmark>` for each tracked bookmark.

`import_refs` does the reverse: reads all git refs from the bare repo and
brings them into jj's view, updating remote bookmark tracking state.

For pushing, `push_refs` calls `export_refs_to_git` internally — the caller
does not need to call `export_refs` separately.

## jj-lib API reference (v0.40)

### Push API

```rust
// High-level: push bookmarks + update jj view
pub fn push_refs(
    mut_repo: &mut MutableRepo,
    subprocess_options: GitSubprocessOptions,
    remote: &RemoteName,
    targets: &GitPushRefTargets,
    callback: &mut dyn GitSubprocessCallback,
    options: &GitPushOptions,
) -> Result<GitPushStats, GitPushError>

pub struct GitPushRefTargets {
    pub bookmarks: Vec<(RefNameBuf, Diff<Option<CommitId>>)>,
    pub tags: Vec<(RefNameBuf, Diff<Option<CommitId>>)>,
}

pub struct GitPushStats {
    pub pushed: Vec<GitRefNameBuf>,
    pub rejected: Vec<(GitRefNameBuf, Option<String>)>,
    pub remote_rejected: Vec<(GitRefNameBuf, Option<String>)>,
    pub unexported_bookmarks: Vec<(RemoteRefSymbolBuf, FailedRefExportReason)>,
}

// Low-level: raw git push, no jj state updates
pub fn push_updates(
    repo: &dyn Repo,
    subprocess_options: GitSubprocessOptions,
    remote_name: &RemoteName,
    updates: &[GitRefUpdate],
    callback: &mut dyn GitSubprocessCallback,
    options: &GitPushOptions,
) -> Result<GitPushStats, GitPushError>
```

`push_refs` internally calls `export_refs_to_git` and then `push_updates`.
The `targets` are `(bookmark_name, Diff { before, after })` where `before`
is the expected current remote state and `after` is the new target.

### Fetch API

```rust
pub struct GitFetch<'a> { /* ... */ }

impl<'a> GitFetch<'a> {
    pub fn new(
        mut_repo: &'a mut MutableRepo,
        subprocess_options: GitSubprocessOptions,
        import_options: &'a GitImportOptions,
    ) -> Result<Self, UnexpectedGitBackendError>

    pub fn fetch(
        &mut self,
        remote_name: &RemoteName,
        expanded_refspecs: ExpandedFetchRefSpecs,
        callback: &mut dyn GitSubprocessCallback,
        depth: Option<NonZeroU32>,
        fetch_tags_override: Option<FetchTagsOverride>,
    ) -> Result<(), GitFetchError>

    pub async fn import_refs(&mut self) -> Result<GitImportStats, GitImportError>
}

pub struct GitImportOptions {
    pub auto_local_bookmark: bool,
    pub abandon_unreachable_commits: bool,
    pub remote_auto_track_bookmarks: HashMap<RemoteNameBuf, StringMatcher>,
}
```

`fetch()` runs `git fetch` subprocess — updates git's `refs/remotes/` but
does NOT touch jj's view. `import_refs()` diffs git refs against jj's view,
imports commit objects, updates remote bookmark tracking state, merges into
local bookmarks. Transaction must be committed afterward.

### Export/Import refs

```rust
// jj bookmarks → git refs
pub fn export_refs(mut_repo: &mut MutableRepo) -> Result<GitExportStats, GitExportError>

// git refs → jj bookmarks
pub async fn import_refs(
    mut_repo: &mut MutableRepo,
    options: &GitImportOptions,
) -> Result<GitImportStats, GitImportError>
```

### Subprocess and callback

```rust
pub struct GitSubprocessOptions {
    pub executable_path: PathBuf,
    pub environment: HashMap<OsString, OsString>,
}

pub trait GitSubprocessCallback {
    fn needs_progress(&self) -> bool;
    fn progress(&mut self, progress: &GitProgress) -> io::Result<()>;
    fn local_sideband(&mut self, message: &[u8], term: Option<GitSidebandLineTerminator>) -> io::Result<()>;
    fn remote_sideband(&mut self, message: &[u8], term: Option<GitSidebandLineTerminator>) -> io::Result<()>;
}
```

Git subprocess requires git ≥ 2.41.0. Auth is handled by the user's git
config (SSH agent, credential helpers). No libgit2 dependency.

### Remote management

```rust
pub fn add_remote(
    mut_repo: &mut MutableRepo,
    remote_name: &RemoteName,
    url: &str,
    push_url: Option<&str>,
    fetch_tags: gix::remote::fetch::Tags,
    bookmark_expr: &StringExpression,
) -> Result<(), GitRemoteManagementError>

pub fn get_all_remote_names(store: &Store) -> Result<Vec<RemoteNameBuf>, UnexpectedGitBackendError>
pub fn remove_remote(mut_repo: &mut MutableRepo, ...) -> Result<(), GitRemoteManagementError>
pub fn rename_remote(mut_repo: &mut MutableRepo, ...) -> Result<(), GitRemoteManagementError>
pub fn set_remote_urls(mut_repo: &mut MutableRepo, ...) -> Result<(), GitRemoteManagementError>
```

### Not needed (handled internally by GitBackend)

| Function | What it does | Why not needed |
|----------|--------------|----------------|
| `commit_from_git_without_root_parent` | git→Commit deserialization | `Store::get_commit` calls it |
| `write_tree_conflict` | Conflict→git subtree encoding | `Store::write_commit` calls it |
| `signature_to_git` / `signature_from_git` | Signature conversion | `GitBackend` trait methods |
| `serialize_extras` / `deserialize_extras` | Protobuf sidecar | Handled internally |

## Conflict round-tripping (resolved)

### How conflicts are represented

jj stores conflicts as `Merge<TreeId>` — a `SmallVec` with alternating
add/remove terms: `[add0, remove0, add1, remove1, add2, ...]`. A simple
3-way conflict has 3 values `[side0, base0, side1]`. Resolved commits have
a single value `[tree_id]`.

### GitBackend conflict encoding

For conflicted commits, `GitBackend::write_commit` automatically:

1. Writes a synthetic git tree with `.jjconflict-base-N` (pointing to
   remove trees) and `.jjconflict-side-N` (pointing to add trees) entries,
   plus a `JJ-CONFLICT-README` blob and entries from side-0 for editor
   compatibility.
2. Writes a `jj:trees` commit header with all term IDs in hex
   (space-separated, internal order: `add0 remove0 add1 ...`).
3. Writes a `jj:conflict-labels` commit header.

### GitBackend conflict decoding

`extract_root_tree_from_commit` reads the `jj:trees` commit header — **not**
the `.jjconflict-*` subtree entries. The subtrees exist only for GC-prevention
and editor compatibility. The header is the authoritative source.

### Round-trip is exact

jj's test suite (`write_tree_conflicts`) asserts `write_commit` → `read_commit`
produces identical `Merge<TreeId>`. No normalization or canonicalization happens.

### KikiBackend compatibility

KikiBackend uses the same `Merge<TreeId>` type with the same proto wire
format (`repeated bytes root_tree` in alternating order, with
`uses_tree_conflict_format = true`). The only translation needed is
BLAKE3 → SHA-1 ID substitution. Structure, ordering, and semantics are
identical.

All tree IDs in the `Merge` must be valid git SHA-1 IDs pointing to
existing git objects before `write_commit` is called. The
`write_tree_conflict` function calls `find_tree(conflict.first())` which
will fail if the tree doesn't exist.

## GitBackend internals relevant to translation

### What `write_commit` handles automatically

When you call `GitBackend::write_commit(contents)`, it:

1. Validates/encodes `root_tree` (resolved: `validate_git_object_id`;
   conflicted: `write_tree_conflict`)
2. Writes the git commit object via gix
3. Serializes `change_id` + `predecessors` into a protobuf stored in
   `<store>/extra/` (a custom stacked-table format, not redb)
4. Creates a `refs/jj/keep/<hex>` no-GC ref
5. Handles hash-collision avoidance (adjusts committer timestamp if
   SHA-1 collides with a different change-id)

### What it does NOT handle

- **Does not write trees or blobs.** Tree/blob objects must already exist
  in the git object store. `write_tree` and `write_file` are completely
  separate methods that must be called independently, bottom-up.
- **Does not recurse into the tree graph.** The `root_tree` IDs are
  embedded directly into the git commit object as raw bytes.

### Root commit

Kiki root commit: `[0u8; 32]`. Git root commit: `[0u8; 20]`.
`GitBackend::write_commit` silently drops root-commit parents (making the
git commit parentless). `read_commit` injects `root_commit_id` for
parentless commits. Mapping `[0u8; 32]` ↔ `[0u8; 20]` is all that's needed.

### Change-id

Stored in two places automatically: a `change-id` git commit header
(reverse-hex encoded) and in the extras protobuf. On read, the header is
preferred; falls back to extras or synthesizes from commit hash.

### Predecessors

Git has no concept of predecessors. They're stored only in the extras
protobuf, never in git commit headers. Handled transparently by
`write_commit` / `read_commit`.

## CLI command design

### `jj kk git push` / `jj kk git fetch`

```bash
# Add a git remote to an existing kiki repo
jj kk git remote add origin https://github.com/user/repo.git

# Push bookmarks to GitHub
jj kk git push --remote origin
jj kk git push --remote origin --bookmark main

# Fetch from GitHub
jj kk git fetch --remote origin

# List git remotes
jj kk git remote list
```

The `jj kk git` subcommand tree mirrors `jj git` as closely as possible.

### Why not `jj git push` directly

`jj git push` calls `git::get_git_backend(store)` which downcasts to
`GitBackend`. Since kiki repos use `KikiBackend`, the downcast fails with
`UnexpectedGitBackendError`. There is no extension point in jj-cli to
intercept this.

### Future: upstream `jj git push` support

The right long-term fix is an upstream contribution to jj: a trait like
`GitSyncable` that any backend can implement, replacing the hard downcast
to `GitBackend`. A working kiki implementation would strengthen that
proposal with a concrete use case.

## Scope estimate

| Component | Est. lines | Location |
|-----------|-----------|----------|
| Shadow git repo init/load | ~50 | `daemon/src/git.rs` |
| ID mapping tables (6 redb tables, 3 object types) | ~120 | `daemon/src/store.rs` |
| Blob/symlink copier | ~40 | `daemon/src/git.rs` |
| Tree translator (recursive, bottom-up) | ~60 | `daemon/src/git.rs` |
| Commit translator (ID rewriting) | ~60 | `daemon/src/git.rs` |
| Graph walker (push direction) | ~50 | `daemon/src/git.rs` |
| Graph walker (fetch direction) | ~50 | `daemon/src/git.rs` |
| `GitPush` / `GitFetch` daemon RPCs | ~100 | `daemon/src/service.rs` |
| Proto definitions for new RPCs | ~30 | `proto/jj_interface.proto` |
| CLI commands (`kk git push/fetch/remote`) | ~80 | `cli/src/main.rs` |
| **Total** | **~640** | |

The distribution of complexity shifts toward the object graph translator
(~260 lines) compared to the original estimate of ~150 for "commit graph
walk + ID rewriting." The increase is mechanical — translating blobs and
trees is straightforward but requires careful bottom-up ordering.

## Open questions

1. **Shadow repo location.** Should it live in the daemon's `storage_dir`
   (alongside `store.redb`) or in the working copy (e.g., `.jj/git/`)?
   The daemon's storage dir is cleaner — the shadow repo is an implementation
   detail, not user-facing.

2. **Multiple git remotes.** The shadow git repo can have multiple git remotes
   (origin, upstream, etc.). This works naturally with gix / git config.

3. ~~**Conflict round-tripping.**~~ **Resolved.** KikiBackend and GitBackend
   use the same `Merge<TreeId>` type. The `jj:trees` commit header preserves
   exact conflict structure. Only ID substitution (BLAKE3→SHA-1) is needed.
   Round-trip test exists in jj's test suite.

4. **Change-id stability.** When pushing a kiki commit to git and fetching it
   back, the change-id must survive the round trip. jj stores it as a commit
   header, which `GitBackend` preserves. Should be fine, but needs testing.

5. **Large repos.** The initial push walks the full object graph (blobs,
   trees, commits) to populate the shadow repo. For repos with long history,
   this could be slow. The mapping table provides incremental sync — only
   new objects are copied on subsequent pushes. Consider parallelizing
   blob writes (they're independent).

6. **Auth.** Git auth (SSH keys, credential helpers, tokens) is handled by
   the git subprocess that jj-lib spawns. The daemon runs as the user, so
   the user's SSH agent and git credential config apply naturally. No special
   handling needed.

7. **Bookmarks.** Kiki's bookmark model needs to be compatible with jj's for
   `export_refs` to work. Need to verify how bookmarks are represented in
   kiki's catalog refs vs. jj's `View`. The push flow uses
   `mut_repo.set_local_bookmark_target()` to set bookmarks before pushing,
   and `push_refs` handles `export_refs_to_git` internally.

8. **Tree deduplication across commits.** Commits that share subtrees (common
   in most repos) should not re-copy objects already in the mapping table.
   The mapping lookup before each write handles this, but the recursive tree
   walk needs to short-circuit when it hits a mapped tree ID.

9. **Unified vs. per-type mapping tables.** Six tables (fwd + rev × 3 types)
   vs. a single table with type-discriminator prefix key. Six tables is
   simpler and avoids encoding overhead; a unified table keeps the redb
   schema smaller. Decide during implementation.
