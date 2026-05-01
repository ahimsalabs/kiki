# Git-convergent storage for jj-yak

Design for replacing jj-yak's custom content store (BLAKE3/prost/redb)
with jj-lib's `GitBackend`, making every remote — git forges, cloud
object stores, peer daemons — speak the same content format with zero
translation.

**Supersedes:** `GIT_REMOTE_RESEARCH.md` (shadow repo + object graph
translation approach).

## Problem statement

jj-yak needs to work with multiple remote types: git forges (GitHub,
GitLab) for public collaboration, cloud object stores (S3, GCS, R2)
for private/team storage, and peer daemons (gRPC) for real-time sync.

The current `YakBackend` uses a custom object format (prost-encoded,
BLAKE3-addressed, stored in redb) that has no connection to git's
object model. The original design proposed a shadow git repo with full
object graph translation (blobs, trees, commits) between the two
formats. Research into jj-lib's `GitBackend` internals revealed that
translation is more complex than initially estimated (~640 lines, six
mapping tables, recursive bottom-up tree walks) and creates a permanent
maintenance tax: every future jj feature that touches git objects
(signing, tags, submodules, SHA-256 migration) requires corresponding
translation code. Worse, a custom content format means every remote
type stores different bytes for the same content — S3 would hold
prost-encoded BLAKE3 blobs while GitHub holds git objects, with
translation needed between them.

This document proposes eliminating the translation layer entirely by
converging yak's content store to git's object format, making it the
**universal content-addressed format** across all remote types.

## Architecture

### Current (BLAKE3/prost/redb)

```
CLI (YakBackend)
  │  gRPC proxy — every Backend method is an RPC
  ▼
Daemon
  ├── Store (redb)
  │     commits_v1:  [u8; 32] → prost bytes
  │     trees_v1:    [u8; 32] → prost bytes
  │     files_v1:    [u8; 32] → prost bytes
  │     symlinks_v1: [u8; 32] → prost bytes
  │
  ├── RemoteStore (grpc:// or dir://)
  │     put_blob / get_blob — per-object write-through/read-through
  │
  └── VFS (FUSE/NFS)
        reads from Store, falls through to RemoteStore on miss
```

Content IDs are 32-byte BLAKE3 hashes of `ContentHash`-serialized
structs. Every object is stored as prost-encoded bytes in redb.

### Proposed (GitBackend)

```
CLI (YakBackend)
  │  gRPC proxy — same RPC surface
  ▼
Daemon
  ├── GitBackend (bare git repo per mount)
  │     .git/objects/   — git ODB (SHA-1, gix)
  │     .git/refs/      — managed by jj-lib
  │     extra/          — jj's extras table (change-id, predecessors)
  │
  ├── Op store (redb, unchanged)
  │     views_v1:      [u8] → opaque bytes
  │     operations_v1: [u8] → opaque bytes
  │
  ├── RemoteStore (grpc:// or dir://)
  │     same trait, carries git object bytes instead of prost bytes
  │
  └── VFS (FUSE/NFS)
        reads through GitBackend (gix)
```

Content IDs become 20-byte SHA-1 hashes (standard git). The bare git
repo lives at `<storage_dir>/mounts/<hash>/git/`. jj-lib's `GitBackend`
handles all encoding: commit headers, conflict trees, signatures,
extras table — the daemon never touches git objects directly.

### What stays the same

- **CLI-side `YakBackend`**: still a gRPC proxy. Every `Backend` trait
  method is an RPC to the daemon. The only change is ID sizes (20 bytes
  instead of 32).

- **Daemon RPC surface**: `WriteCommit`, `ReadCommit`, `WriteTree`,
  `ReadTree`, `WriteFile`, `ReadFile`, `WriteSymlink`, `ReadSymlink` —
  same proto messages. The `bytes` fields carry 20-byte IDs instead of
  32-byte IDs. Proto uses `bytes` (variable-length), so no schema change
  needed.

- **Op store**: views and operations stay in redb with 64-byte
  BLAKE2b-512 keys. These are opaque to the daemon and don't touch git.

- **RemoteStore trait**: already uses `&[u8]` for IDs (deliberately
  variable-length). `put_blob` / `get_blob` / `has_blob` work unchanged.
  The blob bytes change from prost-encoded to git object bytes, but the
  trait doesn't care.

- **VFS architecture**: FUSE/NFS adapters dispatch to `JjYakFs` trait.
  The trait methods read content by ID — the backing store changes but
  the interface doesn't.

- **Catalog refs, checkout state, mount metadata**: unchanged.

### What changes

| Component | Before | After |
|---|---|---|
| `ty::Id` | `[u8; 32]` | `[u8; 20]` |
| `COMMIT_ID_LENGTH` | 32 | 20 |
| `commit_id_length()` | 32 | 20 |
| `root_commit_id()` | `[0; 32]` | `[0; 20]` |
| `empty_tree_id()` | BLAKE3 of empty prost `Tree` | `4b825dc642cb6eb9a060e54bf8d69288fbee4904` (well-known) |
| Content store | redb (`commits_v1`, etc.) | `GitBackend` (bare git repo + extras table) |
| Content hashing | BLAKE3 of `ContentHash` stream | SHA-1 of git object format |
| Encoding | prost (protobuf) | git object format (via gix) |
| `store.write_commit()` | BLAKE3 hash + redb insert | `GitBackend::write_commit` (gix + extras) |
| redb tables | 6 content tables + 2 op tables | 2 op tables only |
| `push_reachable_blobs` | walks redb, pushes prost bytes | walks git ODB, pushes git objects |
| `verify_round_trip` | re-BLAKE3-hashes prost bytes | re-SHA1 unnecessary (git ODB is content-addressed) |

## Why this is better

### Universal content format across all remotes

Git object format becomes the single content-addressed format for
every remote type. The same bytes flow everywhere:

```
Content: git objects (SHA-1 content-addressed, via GitBackend)
  │
  ├─── Git forges (GitHub, GitLab)
  │      git push / git fetch — native protocol, zero translation
  │
  ├─── Cloud object stores (S3, GCS, R2)
  │      put_blob(sha1, git_object_bytes) — trivial RemoteStore impl
  │
  ├─── Peer daemons (grpc://)
  │      same put_blob / get_blob — existing RemoteStore, unchanged
  │
  └─── Local shared storage (dir://)
         same put_blob / get_blob — existing RemoteStore, unchanged
```

Every remote stores **identical bytes** for the same content. A blob
written by one peer, pushed to S3, and fetched by another peer is
byte-for-byte the same object that `jj git push` sends to GitHub.
There is no format boundary anywhere in the system.

This means:
- A colleague can clone from S3 or from GitHub — same objects.
- An S3-backed repo can `jj yak git push` to GitHub with zero
  translation — the git objects already exist locally.
- A GitHub-backed repo can add an S3 remote for fast internal sync
  — same bytes, different transport.

With the current BLAKE3/prost format, S3 and GitHub would store
*different bytes* for the same content, requiring translation between
them. Every new remote type would need its own translation layer.
With git objects as the universal format, a new `RemoteStore` impl
is just a transport adapter (~80 lines for S3).

### Zero translation for git push/fetch

The content store *is* a git repo. Push and fetch become:

```rust
// Push
let mut tx = shadow_repo.start_transaction();
git::export_refs(tx.repo_mut())?;
git::push_refs(tx.repo_mut(), subprocess_opts, &remote, &targets, &mut cb, &opts)?;
tx.commit("push").await?;

// Fetch
let mut tx = shadow_repo.start_transaction();
let mut gf = GitFetch::new(tx.repo_mut(), subprocess_opts, &import_opts)?;
gf.fetch(&remote, refspecs, &mut cb, None, None)?;
gf.import_refs().await?;
tx.commit("fetch").await?;
```

No object graph walking. No ID mapping tables. No blob/tree/commit
copiers. No six redb mapping tables. The git objects already exist in
the right format.

### No permanent translation tax

Every future jj feature that touches git objects just works:
- GPG/SSH commit signing
- Git tags
- Submodules (if jj ever supports them)
- SHA-256 migration (jj + git are both working toward this)
- `jj git` CLI interop

### Simpler daemon

The daemon drops ~200 lines of content store code (`store.rs` content
tables, `ty.rs` `ContentHash` impls, `hash.rs` BLAKE3 helpers) and
replaces them with `GitBackend` delegation. The redb database shrinks
to op-store only.

### Every remote is a transport, not a format

With a universal content format, adding a new remote type is purely a
transport concern — no serialization, no translation, no mapping
tables. The `RemoteStore` trait is already transport-agnostic:

```rust
async fn put_blob(&self, kind: BlobKind, id: &[u8], bytes: Bytes) -> Result<()>;
async fn get_blob(&self, kind: BlobKind, id: &[u8]) -> Result<Option<Bytes>>;
async fn has_blob(&self, kind: BlobKind, id: &[u8]) -> Result<bool>;
```

New remote types are ~80-line `RemoteStore` impls:

| Remote | Transport | Implementation |
|---|---|---|
| `dir://path` | Local filesystem | Exists (M9) |
| `grpc://host:port` | gRPC to peer daemon | Exists (M9) |
| `s3://bucket/prefix` | AWS S3 API | `aws_sdk_s3::Client` put/get/head |
| `gcs://bucket/prefix` | Google Cloud Storage | `google_cloud_storage` put/get |
| `r2://bucket/prefix` | Cloudflare R2 (S3-compat) | Same as S3 |

Git forges are the exception — they speak git's pack protocol, not
per-object CAS. But since the local store *is* a git repo, `jj git
push` / `jj git fetch` handles that natively via jj-lib.

A future optimization for the CAS remotes: git's pack protocol with
delta compression is more efficient for bulk transfer than per-object
put/get. The daemon could generate pack files locally and upload them
as single S3 objects. This is optional — per-object CAS works
correctly and is simpler to implement first.

## Detailed design

### Daemon-side storage

Each mount gets a bare git repo instead of redb content tables:

```
storage_dir/mounts/<hash>/
├── git/                    # bare git repo (replaces store.redb content tables)
│   ├── objects/            # git ODB
│   ├── refs/               # managed by jj-lib
│   └── config              # git config (remotes, etc.)
├── extra/                  # jj's extras table (change-id, predecessors)
│   ├── heads/
│   └── <segment files>
├── store.redb              # op-store only (views_v1, operations_v1, refs_v1)
└── mount.toml              # mount metadata (unchanged)
```

The `git/` directory and `extra/` directory are managed by jj-lib's
`GitBackend`. The daemon creates them via:

```rust
let git_backend = GitBackend::init_internal(&settings, &store_path)?;
```

This creates `<store_path>/git/` (bare repo) and `<store_path>/extra/`
(metadata table). The daemon wraps this `GitBackend` and delegates all
content read/write operations to it.

### Daemon content operations

Replace the current `Store` (redb content tables + BLAKE3 hashing) with
`GitBackend` delegation:

```rust
// Current: daemon/src/store.rs
impl Store {
    fn write_commit(&self, commit: ty::Commit) -> Result<(Id, Bytes)> {
        let bytes = commit.encode_to_vec();
        let id = Id(*blake3(&commit).as_bytes());
        // insert into redb commits_v1 table
        Ok((id, Bytes::from(bytes)))
    }
}

// Proposed: daemon/src/git_store.rs
impl GitContentStore {
    fn write_commit(&self, commit: backend::Commit) -> Result<(CommitId, backend::Commit)> {
        // GitBackend handles: git object encoding, extras table,
        // change-id header, no-GC ref, collision avoidance
        self.git_backend.write_commit(commit, None)
    }
}
```

The `ty::Commit`, `ty::Tree`, `ty::File`, `ty::Symlink` types and
their `ContentHash` impls become unnecessary. The daemon works with
jj-lib's `backend::Commit`, `backend::Tree`, etc. directly.

### Proto wire format

The proto messages (`jj_interface.proto`) use `bytes` for IDs and
content — variable-length, no schema change needed. The actual bytes
on the wire change (20-byte IDs instead of 32-byte, git-format content
instead of prost), but the proto schema is identical.

The `service.rs` RPC handlers change from:
1. Decode proto → `ty::*` → `store.write_*()` → return BLAKE3 ID

To:
1. Decode proto → `backend::*` → `git_backend.write_*()` → return SHA-1 ID

The proto-to-backend conversion functions (`commit_to_proto`,
`commit_from_proto`, `tree_to_proto`, `tree_from_proto`) are adapted
to work with jj-lib's `backend::*` types. These are similar to the
existing conversions but map to/from jj-lib's types instead of the
daemon's `ty::*` types.

### `ty::Id` migration

```rust
// Current
pub struct Id(pub [u8; 32]);

// Proposed
pub struct Id(pub [u8; 20]);
```

All `TryFrom<Vec<u8>>` conversions change from expecting 32 bytes to
20 bytes. The redb content tables (`commits_v1` through `symlinks_v1`)
are removed entirely — no migration needed, just stop creating them.

The `refs_v1` table key is `&str` and value is `&[u8]` — no ID size
assumption. The `views_v1` and `operations_v1` tables use `&[u8]` keys
(64-byte BLAKE2b-512) — unchanged.

### VFS reads

The VFS currently reads from redb via `store.get_tree(id)` etc. With
GitBackend, reads go through `git_backend.read_tree(path, id)` (which
calls gix to read git objects). The `JjYakFs` trait methods are already
async and return typed values — only the backing implementation changes.

**Concurrency**: GitBackend's internal `Mutex<gix::Repository>` and
`concurrency() = 1` appear to be a problem for FUSE, but the mutex
is a micro-optimization for single-threaded use, not a safety
requirement. gix's underlying `Store` uses `ArcSwap` + atomics and
is explicitly designed for "lock-free reading for perfect scaling
across all cores." The YakBackend wrapper overrides all read methods
to use `self.inner.git_repo()` (which returns a fresh thread-local
`gix::Repository` from the `ThreadSafeRepository` base — cost is a
few `Arc` clones, microseconds, no disk I/O) and returns
`concurrency() > 1`. Writes keep the mutex. See "Concurrent VFS
reads" section below for details.

**Performance**: gix reads loose git objects with a single file open +
zlib decompress. For packed objects, gix uses mmap'd pack files with
an index for O(log n) lookup. This is comparable to redb B-tree
lookup for typical VFS access patterns. FUSE/NFS kernel overhead
dominates either way.

### RemoteStore sync

The `RemoteStore` trait is ID-length-agnostic (`id: &[u8]`). Content
sync continues to work via `put_blob` / `get_blob`:

```rust
// Push a git object to the remote
let git_object_bytes = read_git_object(&git_odb, &sha1_id)?;
remote.put_blob(BlobKind::Commit, &sha1_id, git_object_bytes).await?;

// Fetch a git object from the remote
let bytes = remote.get_blob(BlobKind::Commit, &sha1_id).await?;
write_git_object(&git_odb, &sha1_id, bytes)?;
```

The `verify_round_trip` helper in `remote/fetch.rs` currently re-hashes
fetched bytes with BLAKE3 to verify integrity. With git objects, this
changes to SHA-1 verification (or can be dropped — git's ODB is
content-addressed by SHA-1, so a correct write implies a correct hash).

The `push_reachable_blobs` post-snapshot walk changes from iterating
redb tables to walking the git ODB (via gix's object iteration or
`refs/jj/keep/` refs).

#### Extras table replication

GitBackend stores change-id and predecessors in `<store>/extra/` — a
custom stacked-table format, not git objects. For daemon-to-daemon
sync, the extras must also be replicated. Options:

1. **Add `BlobKind::Extra`** — cleanest, fits the existing CAS model.
   The key is the commit SHA-1; the value is the extras protobuf bytes.
   On `write_commit`, the daemon does `put_blob(Extra, sha1, extras)`.
   On `read_commit` miss + remote fetch, the daemon also fetches the
   extras entry.

2. **Rely on the git commit header** — GitBackend stores `change-id`
   in a git commit header *and* in the extras table. On read, the
   header is preferred. `import_head_commits()` can rebuild extras
   from git commits. However, **predecessors only live in extras** —
   they'd be lost without explicit replication.

**Recommendation**: Option 1. Add `BlobKind::Extra`. Predecessors are
important for jj's evolution log; losing them silently is not
acceptable. The extras blobs are small (a few hundred bytes each) and
only exist for commits, so the overhead is negligible.

#### BlobKind changes

Consider collapsing `BlobKind::File` and `BlobKind::Symlink` into a
single `BlobKind::Blob` — git doesn't distinguish between files and
symlinks at the object level (both are blobs; the mode bits live on
the tree entry). This simplifies the enum and avoids the question of
which kind to use when syncing a git blob.

Updated enum:
```rust
enum BlobKind {
    Blob,       // git blob (files + symlinks)
    Tree,       // git tree
    Commit,     // git commit
    Extra,      // jj extras protobuf (new)
    View,       // jj view (unchanged)
    Operation,  // jj operation (unchanged)
}
```

### Git remote operations (the whole point)

With the content store being a real git repo, git push/fetch is
straightforward. The daemon needs a jj `Workspace` or `ReadonlyRepo`
wrapping the `GitBackend` to get a `MutableRepo` for jj-lib's git
APIs.

```rust
// One-time init (when first git remote is added, or at mount creation)
let (workspace, repo) = Workspace::init_internal_git(&settings, &jj_repo_path).await?;

// Subsequent opens
let repo = RepoLoader::init_from_file_system(&settings, &jj_repo_path, &StoreFactories::default())?
    .load_at_head().await?;
```

The jj repo metadata (`.jj/repo/` with op_store, op_heads, index)
lives alongside the git repo. This is purely local bookkeeping for
jj-lib's transaction model.

#### Push

```rust
let mut tx = repo.start_transaction();
let mut_repo = tx.repo_mut();

// Set bookmark targets from yak's catalog refs
for (name, commit_id) in yak_bookmarks {
    mut_repo.set_local_bookmark_target(&name, RefTarget::normal(commit_id));
}

// Push (internally exports refs to git, then runs git push subprocess)
let stats = git::push_refs(mut_repo, subprocess_opts, &remote, &targets, &mut cb, &opts)?;

tx.commit("push to origin").await?;
```

#### Fetch

```rust
let mut tx = repo.start_transaction();
let mut_repo = tx.repo_mut();

let mut gf = GitFetch::new(mut_repo, subprocess_opts, &import_opts)?;
gf.fetch(&remote_name, refspecs, &mut cb, None, None)?;
gf.import_refs().await?;

// Sync imported bookmarks back to yak's catalog refs
for (name, target) in tx.repo().view().local_bookmarks() {
    yak_catalog.cas_ref(&name, expected, new).await?;
}

tx.commit("fetch from origin").await?;
```

#### Auth

Git auth (SSH keys, credential helpers, tokens) is handled by the git
subprocess that jj-lib spawns via `GitSubprocessOptions`. The daemon
runs as the user, so the user's SSH agent and git credential config
apply naturally. No special handling needed.

### CLI commands

```bash
# Add a git remote to an existing yak repo
jj yak git remote add origin https://github.com/user/repo.git

# Push bookmarks to GitHub
jj yak git push --remote origin
jj yak git push --remote origin --bookmark main

# Fetch from GitHub
jj yak git fetch --remote origin

# List git remotes
jj yak git remote list
```

The `jj yak git` subcommand tree mirrors `jj git` as closely as
possible.

### Multi-remote workflow

A typical workflow uses a cloud object store for real-time team sync
and a git forge for public collaboration:

```bash
# Init with S3 as the primary remote (real-time sync, private)
jj yak init --remote s3://company-bucket/repos/my-project

# Work normally — writes flow through to S3 via RemoteStore
jj new -m "add feature"
# ... edit files ...

# A teammate syncs from the same S3 bucket
jj yak init --remote s3://company-bucket/repos/my-project ~/teammate-copy

# When ready, push to GitHub for a PR (batch, public)
jj yak git remote add origin git@github.com:company/my-project.git
jj yak git push --remote origin --bookmark main

# External contributor clones from GitHub
jj git clone git@github.com:company/my-project.git

# Both paths produce identical content — same git objects everywhere
```

The S3 remote and GitHub remote hold the same content-addressed git
objects. The only difference is the transport: S3 uses per-object CAS
via the `RemoteStore` trait; GitHub uses git's pack protocol via
jj-lib's `push_refs` / `GitFetch` APIs.

### Serving git protocol (future)

Since the content store is a standard bare git repo, the daemon can
serve the git smart HTTP protocol directly — making it a git server
that plain `git` clients can clone from and push to:

```
                    ┌──────────────────────────┐
                    │       yak daemon         │
                    │                          │
 jj/yak clients ──►│  gRPC  (yak protocol)    │
                    │    ├─ VFS / FUSE         │
                    │    ├─ real-time sync      │
                    │    ├─ op-log              │
                    │    └─ catalog refs        │
                    │                          │
 git clients ─────►│  HTTP  (git protocol)    │──── bare git repo
                    │    ├─ git clone / pull    │    (shared content
                    │    └─ git push            │     store)
                    │                          │
 S3 / cloud ◄─────►│  RemoteStore             │
                    │    └─ put/get blob        │
                    └──────────────────────────┘
```

A commit written by a jj client via gRPC is immediately visible to a
`git clone` from the same daemon. All three client types see the same
content.

#### In-process protocol, no hooks

The daemon handles the git smart HTTP protocol **in-process** via
`gix-protocol` and `gix-pack` — no `git receive-pack` subprocess, no
filesystem-executable hook scripts. This eliminates:

- **Hook injection**: no shell scripts in the storage directory that
  could be modified by an attacker with write access.
- **Subprocess spawning on inbound connections**: no fork/exec on
  every git client request.
- **Opaque error handling**: no "hook exited with code 1" — the
  daemon controls the entire receive path and can return structured
  errors.

The inbound push flow:

```
git client ──HTTP──► daemon
                       1. Speaks git smart HTTP protocol (in-process)
                       2. Receives pack data, writes objects via gix
                       3. Validates ref updates (reject force-push, etc.)
                       4. Updates refs in the bare repo via gix
                       5. Calls import_refs → updates jj's view
                       6. Replicates new objects to RemoteStore (S3, peers)
                       7. Updates catalog refs for jj clients
```

The daemon has full control over what pushes are accepted — it can
enforce policies (no force-push to main, require signed commits, etc.)
without relying on git hooks.

#### HTTP endpoints

The git smart HTTP protocol requires three endpoints:

```
GET  /info/refs?service=git-upload-pack      # ref advertisement (fetch)
GET  /info/refs?service=git-receive-pack     # ref advertisement (push)
POST /git-upload-pack                         # pack negotiation + send
POST /git-receive-pack                        # receive pack + update refs
```

These can be served alongside the existing gRPC listener (different
port, or HTTP/gRPC multiplexing via content-type detection). The
implementation uses gix for pack generation (`upload-pack`) and pack
ingestion (`receive-pack`), with the daemon's own authorization and
policy enforcement layer in between.

#### Why this only works with git-convergent storage

With a custom BLAKE3/prost content format, serving git protocol would
require real-time object graph translation on every pack negotiation —
effectively impossible at interactive speeds for non-trivial repos.
With git objects as the store, the bare repo is already a git server
waiting to be exposed.

## Migration

### Existing yak repos

This is a breaking change to the on-disk format. Existing yak repos
(with 32-byte BLAKE3 IDs in redb) are incompatible. Since jj-yak is
pre-release, this is acceptable.

Migration path: `jj yak init` creates a new repo with git-backed
storage. Existing repos can be abandoned or migrated with a one-time
export/import tool (low priority).

### Daemon startup

The daemon's `rehydrate` logic changes: instead of opening redb content
tables, it opens the bare git repo via `GitBackend::load`. The
`mount.toml` format adds a version field so the daemon can detect and
reject old-format mounts with a clear error.

## Concurrent VFS reads

GitBackend's `concurrency()` returns 1 and all reads go through a global
`Mutex<gix::Repository>`. This looks like a dealbreaker for FUSE, but
it's a performance optimization, not a correctness requirement.

### Why the mutex exists

The comment in `git_backend.rs` (lines 170-173): "it's cheaper to
cache the thread-local instance behind a mutex than creating one for
each backend method call." In jj's normal CLI workflow (single-threaded),
reusing one `gix::Repository` avoids repeated `to_thread_local()` calls.

### Why concurrent reads are safe

gix's object database (`gix_odb::Store`) is explicitly designed for
concurrent access:

- **Lock-free reads**: uses `ArcSwap` for the slot-map index and atomics
  for load state. Documentation states: "lock-free reading for perfect
  scaling across all cores."
- **Per-handle state**: each `gix::Repository` (from `to_thread_local()`)
  gets its own `RefCell<zlib::Inflate>` decompressor and pack cache.
  No shared mutable state between handles during reads.
- **Mmap'd pack files**: under `max-performance-safe` (enabled in jj's
  Cargo.toml), pack files are mmap'd. Concurrent mmap reads are safe —
  the kernel handles page-level coherency.
- **Loose objects**: plain files, read-only. Multiple concurrent
  `open()` + `read()` calls are safe on all POSIX systems.
- **Writes are atomic**: loose objects use `tempfile` + `rename(2)`.
  A reader either sees the complete object or doesn't see it at all.

### The wrapper pattern

```rust
struct YakGitStore {
    inner: GitBackend,  // owns the ThreadSafeRepository + mutex
}

impl YakGitStore {
    /// Thread-safe concurrent read — bypasses inner's mutex
    fn read_file(&self, path: &RepoPath, id: &FileId) -> BackendResult<Vec<u8>> {
        let repo = self.inner.git_repo();  // fresh thread-local, no mutex
        let oid = validate_git_object_id(id)?;
        let blob = repo.find_object(oid)?.try_into_blob()?;
        Ok(blob.take_data())
    }

    fn concurrency(&self) -> usize {
        // Enable jj-lib's parallel read pipelines
        num_cpus::get().min(16)
    }
}
```

**Override these read methods** to use `self.inner.git_repo()`:
- `read_file` — blob lookup
- `read_symlink` — blob lookup
- `read_tree` — tree object parse
- `read_commit` — commit parse + extras table lookup
  (the extras `Mutex<Option<Arc<ReadonlyTable>>>` is separate from the
  ODB mutex and held only briefly to clone an `Arc`)

**Keep the mutex for writes** — `write_file`, `write_tree`,
`write_commit` legitimately need serialization for ODB writes and
extras table updates.

### Cost of `to_thread_local()`

Each call does: 2 `Arc` clones, 1 atomic increment, a
`collect_snapshot()` that iterates loaded pack indices and clones their
`Arc<IndexFile>` handles. No disk I/O, no config parsing. Cost is
O(number of loaded pack files) — microseconds for a typical repo.

Can be optimized further with `thread_local!` caching if profiling
shows it matters. Unlikely to be necessary given FUSE kernel overhead
dominates.

### Precedent

`SecretBackend` in jj-lib wraps `GitBackend` by value and delegates
all `Backend` trait methods. It demonstrates the pattern is stable and
supported. It does not override concurrency (returns 1), but the trait
allows any value.

## What this gives up

### BLAKE3

BLAKE3 is faster than SHA-1 and has 256-bit collision resistance vs
SHA-1's effectively broken 160-bit. In practice:

- **Speed**: content hashing is not the bottleneck. I/O, git protocol
  negotiation, and network latency dominate push/fetch. For local
  writes, gix's SHA-1 is fast enough.
- **Collision resistance**: git is migrating to SHA-256. jj already has
  infrastructure for this (`git.write-change-id-header`). When git
  completes the transition, yak benefits automatically.

### redb as content store

redb is a good embedded database: single file, ACID transactions,
zero-copy reads. git's ODB is a directory tree of loose objects +
pack files. For VFS random-read workloads:

- **Loose objects**: one file open + zlib decompress per read. Slightly
  more overhead than redb B-tree lookup, but FUSE/NFS kernel overhead
  dominates.
- **Pack files**: gix maintains an index for O(log n) lookup.
  Comparable to redb for read-heavy workloads.
- **Writes**: git loose object writes are atomic (tmp + rename), same
  as redb. Pack file repacking is a background operation.

Net: no meaningful performance regression for yak's workload.

### Custom serialization

prost encoding is compact and well-understood, but git's object format
is equally so — and it's the universal interchange format. Dropping
prost for content objects simplifies the stack (one fewer dependency
for the content path).

## Scaling

### Small to large repos (Linux kernel, Chromium scale)

Git's ODB handles this. It's literally what git was built for. The
Linux kernel has ~1M commits and ~80K files; Chromium has ~100K files.
gix reads pack files via mmap'd indexes with O(log n) lookup.
Performance is comparable to redb for yak's VFS workload, where
FUSE/NFS kernel overhead dominates object store access time.

### Huge monorepos (Google, Meta scale)

Neither git ODB nor redb scales to this level without lazy object
fetching. At millions of files and millions of commits, no local store
can hold the full repo. The scaling architecture is always:

```
Hot set (local cache)  ←  fetch on miss  ←  Cold set (remote)
```

This is what Google does internally with jj (cloud backend + caching
daemon), what Microsoft does with VFS for Git, and what Meta does with
Sapling/EdenFS.

**Yak already has this pattern.** The daemon's `RemoteStore`
read-through fetches objects on local cache miss:

```
VFS read → local store miss → RemoteStore.get_blob() → cache locally
```

This works identically whether the local store is redb or git ODB,
and whether the remote is `grpc://`, `dir://`, or `s3://`. The
convergence decision doesn't change the scaling story.

jj-lib's `Backend` trait was designed for this future:

```rust
/// An estimate of how many concurrent requests this backend handles
/// well. A local backend like the Git backend (until it supports
/// partial clones) may want to set this to 1. A cloud-backed backend
/// may want to set it to 100 or so.
fn concurrency(&self) -> usize;
```

At Google/Meta scale, you'd implement a custom `Backend` that returns
`concurrency() = 100` and fetches objects from a cloud store on demand.
The content format at that layer could be anything — it's behind the
`Backend` trait boundary. Git objects would be one option (compatible
with GitHub); a custom format optimized for the cloud store would be
another. That's a separate project from the current work.

**The convergence decision should be made on the translation-tax and
remote-universality arguments, not the scaling argument.** Scaling is
orthogonal — both stores need the same lazy-fetch architecture for
true monorepo support, and yak already has the foundation for it.

## Alternatives considered

### A. Shadow git repo with full object graph translation

The original design in `GIT_REMOTE_RESEARCH.md`. Maintain a separate
bare git repo alongside the yak redb store. On push, walk the full
object graph (blobs, trees, commits) bottom-up, translating IDs from
BLAKE3 (32-byte) to SHA-1 (20-byte) and writing to the shadow repo.
On fetch, reverse the process.

**Why rejected:**

- **~640 lines of translation code** including recursive tree walkers,
  six redb mapping tables (fwd + rev for commits, trees, blobs), and
  careful bottom-up ordering.
- **Permanent maintenance tax**: every jj feature that touches git
  objects needs corresponding translation code.
- **Doubled storage**: every content object exists in both redb
  (BLAKE3/prost) and git ODB (SHA-1/git format).
- **Initial push is O(repo)**: the first push walks the entire object
  graph. Subsequent pushes are incremental via mapping tables, but the
  cold start is expensive.
- **Complexity is in the wrong place**: jj-lib already has a
  production-quality git backend. Maintaining a parallel content store
  and translating between them is fighting the platform.

### B. `git fast-import` for batch translation

Generate a `git fast-import` stream from yak commits and pipe to
`git fast-import` in a shadow repo. Avoids per-object gix writes.

**Why rejected:**

- Still requires the shadow repo and mapping tables.
- jj-lib's extras table (change-id, predecessors) is not populated by
  fast-import — would need manual population via a private API
  (`stacked_table` format is `pub(crate)`).
- Saves some mechanical complexity in the push direction but doesn't
  help with fetch.
- Doesn't address the fundamental problem: two content stores with
  different formats.

### C. Dual-write (write to both stores simultaneously)

On every `write_commit` / `write_tree` / `write_file`, write to both
the yak redb store and a git repo. The git repo is always up-to-date,
so push is cheap.

**Why rejected:**

- Doubles write cost for every operation, even when git push is never
  used.
- Still need the mapping tables for ID cross-referencing.
- Complex consistency — what if one write succeeds and the other fails?
- Doesn't simplify the codebase; adds a second write path.

### D. Keep custom format, translate per-remote

Keep BLAKE3/prost/redb as the canonical store. Implement translation
for each remote type that needs a different format: git translation
for GitHub, raw prost bytes for S3, etc.

**Why rejected:**

- Every remote type that isn't "raw prost bytes" needs a translation
  layer. GitHub needs BLAKE3→SHA-1 object graph translation (~640
  lines). A future format-aware remote would need its own translator.
- S3 and gRPC remotes would store prost bytes that no other tool can
  read. The content is opaque to everything outside yak.
- The format boundary multiplies: N remote types × M content types =
  N×M translation paths. With git objects as the universal format,
  all remote types store the same bytes — the only special case is
  GitHub (which speaks git protocol instead of per-object CAS).
- Yak becomes a walled garden. Git objects are the universal
  interchange format — every tool, service, and hosting provider
  understands them. A custom format means yak content only lives
  in yak.

## Scope estimate

| Component | Change | Est. |
|---|---|---|
| `ty::Id`: `[u8; 32]` → `[u8; 20]` | mechanical | ~20 lines |
| Remove `store.rs` content tables + BLAKE3 hashing | deletion | -200 lines |
| Remove `ty.rs` ContentHash impls | deletion | -100 lines |
| Remove `hash.rs` BLAKE3 helpers | deletion | -30 lines |
| New `git_store.rs`: GitBackend wrapper + init/load | new | ~120 lines |
| Concurrent read overrides (read_file/tree/commit/symlink) | new | ~80 lines |
| `service.rs`: adapt RPC handlers to GitBackend | modify | ~150 lines |
| Proto conversions: `backend::*` ↔ proto | modify | ~100 lines |
| `remote/fetch.rs`: adapt verify/sync to git objects | modify | ~50 lines |
| `remote/fetch.rs`: extras replication (`BlobKind::Extra`) | new | ~40 lines |
| VFS reads: adapt to GitBackend | modify | ~30 lines |
| Git push/fetch RPCs + CLI commands | new | ~200 lines |
| **Net change** | | **~+460 lines** |

The shadow repo approach estimated ~640 lines of *new* code on top of
the existing store. This approach is ~460 lines net (including
deletions), removes the six mapping tables entirely, and produces a
simpler final architecture.

## Open questions

### Resolved

1. ~~**FUSE concurrency.**~~ GitBackend's mutex is bypassable via
   `git_repo()` (thread-local repos). gix's ODB is designed for
   lock-free concurrent reads. The wrapper overrides read methods
   and returns `concurrency() > 1`. See "Concurrent VFS reads" above.

2. ~~**Extras table replication.**~~ Add `BlobKind::Extra` to the
   `RemoteStore` CAS. Extras are small and commit-only. Predecessors
   cannot be recovered from git objects alone, so explicit replication
   is required. See "Extras table replication" above.

3. ~~**Peer daemon interop.**~~ Breaking change, acceptable for
   pre-release. No existing yak users.

4. ~~**gRPC content sync format.**~~ Send decompressed git object
   content via the existing `put_blob` / `get_blob` API. The receiver
   writes it into git ODB via gix (which handles compression). Git
   pack protocol is a future optimization, not needed for correctness.

### Open

5. **jj repo metadata for git push/fetch.** The `GitBackend` needs a
   jj repo wrapper (op_store, op_heads, index) to get a `MutableRepo`
   for `push_refs` / `GitFetch`. Two options:
   - Use `ReadonlyRepo::init` with default store initializers — creates
     the minimal repo metadata alongside the git repo. This is local
     bookkeeping, never synced. Cleanest option.
   - Use `Workspace::init_internal_git` — creates a full workspace
     including working copy type. Unnecessary but harmless.
   Decision: use `ReadonlyRepo::init`. The jj metadata lives at
   `<storage_dir>/mounts/<hash>/jj-repo/` (sibling to `git/`).

6. **Pack file management.** git's ODB accumulates loose objects.
   Options:
   - Run `git gc --auto` periodically from the daemon
   - Let the user run it manually
   - Hook into jj's `gc()` Backend method (GitBackend runs `git gc`)
   Not urgent — loose objects work fine for small-to-medium repos.

7. **Bookmark model bridging.** Yak's catalog refs are flat strings
   (`op_heads`, custom refs). jj's bookmarks are structured
   (`local_bookmarks`, `remote_bookmarks` with tracking state). The
   push/fetch RPCs need to bridge this:
   - On push: read yak catalog refs → set `local_bookmark_target` in
     `MutableRepo` → `push_refs` handles export + push
   - On fetch: `import_refs` updates jj's View → read View's
     `local_bookmarks()` → write back to yak catalog refs via `cas_ref`
   The bridging is straightforward but the exact ref naming convention
   needs design.

8. **StoreFactories registration.** The CLI-side `YakBackend` needs
   to register with `StoreFactories` under a custom name (e.g. `"yak"`)
   so `Workspace::load` can find it. The `store/type` file contains
   this name. The daemon-side `GitBackend` uses the standard `"git"`
   type — the daemon doesn't go through `StoreFactories` at all.
