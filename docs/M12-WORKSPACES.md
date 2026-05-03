# M12: Managed Workspaces (RootFs)

**Status: ✅ done.** All phases implemented, 14 e2e tests passing
with real FUSE, `root_tree_id` persistence across daemon restarts
verified. Git clone materializes content immediately via
`initial_tree_id`. Last updated: 2026-05-03.

Spec for M12. Implements the managed-workspace model from
[`WORKSPACES.md`](./WORKSPACES.md) (Option B: single mount,
`RootFs` routing). The milestone index in [`PLAN.md`](./PLAN.md)
links here. Depends on daemon lifecycle (landed), git convergence
(landed), and M10.7 gitignore-aware VFS (landed). Does not depend
on M11 (push queue) — workspaces are useful before offline
resilience.

## 12. M12 — managed workspaces

A single FUSE mount at a well-known path (`/mnt/kiki/` default)
serves all repos and all workspaces. The daemon presents a `RootFs`
layer that routes filesystem operations through synthetic namespace
directories into per-workspace `KikiFs` instances. Workspace
creation is cheap: each workspace shares a per-repo git object store
and owns only lightweight per-workspace state (op heads, checkout
pointer, dirty inodes). Workspaces hydrate lazily on first
filesystem access.

The namespace:

```
/mnt/kiki/                          # FUSE mount root
  monorepo/                         # repo dir (synthetic)
    default/                        # workspace → KikiFs
    fix-auth/                       # workspace → KikiFs
  dotfiles/                         # repo dir (synthetic)
    default/                        # workspace → KikiFs
```

### 12.1 Scope

In:

- `RootFs` struct implementing `JjKikiFs`, managing the
  `/<repo>/<workspace>/` namespace and dispatching into per-workspace
  `KikiFs` instances.
- Boundary remapping for global inodes: `RootFs` translates between
  per-workspace local inode space and a global inode space using a
  24:40 slot encoding. `KikiFs` and `InodeSlab` are unchanged.
- Restructured storage layout: per-repo shared `git_store/`,
  per-workspace lightweight state.
- Repo registry: `repos.toml` at the storage root, mapping names to
  URLs and workspace slot allocations.
- Proto RPCs: `Clone`, `WorkspaceCreate`, `WorkspaceList`,
  `WorkspaceDelete`, `RepoList`.
- CLI commands: `kiki clone <url> [--name <name>]`, `kiki workspace
  create/list/delete`.
- Single-mount daemon startup: daemon creates and binds one FUSE
  mount at the configured `mount_root`.
- Lazy hydration: workspace `KikiFs` instantiated on first `lookup`
  into a workspace directory, not at daemon start.
- Rehydration: daemon restart re-reads `repos.toml` and
  `workspace.toml` files, re-populates `RootFs` routing table,
  re-binds the single FUSE mount.
- jj workspace integration: each kiki workspace maps to a jj
  workspace within a shared repo. `kiki clone` calls
  `Workspace::init_with_factories` for the default workspace;
  `kiki workspace create` calls the jj workspace-add flow.

Out (deferred):

- **macOS NFS `RootFs`** — this spec covers Linux FUSE only. macOS
  NFS adapter for `RootFs` is a follow-up. Per-workspace NFS mounts
  (Option A from [`WORKSPACES.md`](./WORKSPACES.md)) remain
  available on macOS as a bridge.
- **Remote catalog discovery** — `readdir` on a repo dir shows only
  locally-known workspaces. Remote-advertised repo/workspace lists
  are a follow-up.
- **Agent workspace GC** — automated cleanup of stale temporary
  workspaces. Tracked in [`WORKSPACES.md`](./WORKSPACES.md).
- **Server-side workspace awareness** — the server sees operations
  and refs but not workspace names. Workspace identity is
  client-side.
- **Cross-workspace invalidation** — `check_out` in workspace A does
  not proactively invalidate workspace B's VFS cache. Each workspace
  revalidates independently (TTL=0).
- **Graceful restart / fd-passing** — one mount makes restart more
  disruptive; the takeover protocol from PLAN.md §10.2 becomes
  critical but ships separately.
- **Multi-repo server remotes** — a remote entry that discovers and
  lists multiple repos on a server. Each remote points to one repo
  for M12.

### 12.2 Decisions

1. **Option B: single `RootFs` mount, not many mounts.** The CitC
   model requires `cd /mnt/kiki/myrepo/foo/` to work without any
   prior mount step for that workspace. A single FUSE mount at a
   well-known path achieves this. Option A (many mounts,
   CLI-managed namespace with real directories) gives a weaker UX:
   you must explicitly create a workspace before you can access it,
   and `ls /mnt/kiki/myrepo/` requires real-directory management
   outside the daemon. Option B subsumes A — the daemon owns the
   entire namespace.

2. **Each named remote is one repo.** A remote entry in config
   points to a single repo, not a server with many repos. The remote
   name becomes the top-level directory in the namespace:
   `[repos.monorepo] url = "kiki+ssh://server/repos/monorepo"` →
   `/mnt/kiki/monorepo/`. Multi-repo server discovery would add a
   nesting level (`/<server>/<repo>/`) that can land later without
   breaking the two-level `/<repo>/<workspace>/` shape.

3. **Boundary remapping for global inodes (24:40 slot encoding).**
   Each workspace is assigned a slot `S` (1-indexed, 24 bits →
   16M slots). `RootFs` translates between global and local inode
   spaces at every trait-method boundary:
   `global = (S << 40) | local`, `local = global & ((1<<40)-1)`.
   Slot 0 is reserved for `RootFs` synthetic directories (inodes
   1..1023). `KikiFs` and `InodeSlab` are completely unchanged —
   they keep using `ROOT_INODE = 1` and monotonic allocation
   internally. The translation is mechanical bit manipulation in
   `RootFs`. **Slots are monotonic and never reused.** A deleted
   workspace's slot is permanently retired. With 24 bits (16M
   slots), exhaustion is not a practical concern — and reuse is
   unsafe because the kernel dcache and any process with open fds
   may still hold global inodes from the deleted workspace. If a
   recycled slot's inodes happen to alias into the new workspace,
   stale handles would silently dispatch into wrong files instead
   of failing with ESTALE. Monotonic allocation avoids this
   entirely. The derived-hash scheme from PLAN.md §7 decision 6
   (restart stability + global uniqueness collapsed into one) is
   the eventual target; boundary remapping decouples the two
   concerns and ships faster.

4. **Shared git object store per repo, direct reference.** All
   workspaces within a repo reference the same `git_store/` bare
   repo directory. No git alternates indirection. `git gc` affecting
   all workspaces simultaneously is acceptable — the daemon manages
   the store and can coordinate. Alternates add object-lookup and
   pack-file-sharing complexity that isn't justified at this scale.

5. **Repo registry in `repos.toml`, per-workspace state in
   `workspace.toml`.** The daemon's repo list is a single TOML file
   at `<storage_dir>/repos.toml`. Per-workspace mutable state
   (`workspace.toml`, scratch dir) lives under each workspace
   directory. The shared `store.redb` (op-store, `LocalRefs`) lives
   at the repo level. This separates "what repos exist" (global,
   small) from "per-workspace mutable state"
   (local, updated on every mutating RPC).

6. **`/mnt/kiki/` as default mount root, configurable.** Requires
   one-time `sudo mkdir -p /mnt/kiki && sudo chown $USER /mnt/kiki`
   on first use. Acceptable for a developer tool. Alternative roots
   like `~/kiki/` work via `mount_root` in `config.toml`. The daemon
   creates the mount directory if it doesn't exist and the parent is
   writable.

7. **Top-level CLI commands for workspace management.** `kiki clone`
   and `kiki workspace` are top-level, not under the `kk` jj-
   passthrough subcommand. Workspace management is a first-class kiki
   concept. The `kk` subcommand remains for jj-passthrough operations
   and daemon management. Exact naming deferred — proto and service
   changes are independent of CLI surface.

### 12.3 `RootFs` design

New file: `daemon/src/vfs/root_fs.rs`.

`RootFs` implements `JjKikiFs`. The FUSE adapter takes
`Arc<RootFs>` the same way it currently takes `Arc<KikiFs>` — no
changes to `FuseAdapter` (`daemon/src/vfs/fuse_adapter.rs`).

```rust
pub struct RootFs {
    inner: Mutex<RootFsInner>,
}

struct RootFsInner {
    mount_root: PathBuf,
    /// Synthetic inode allocator (slot 0, starts at 2; inode 1 = root).
    next_synthetic: InodeId,
    /// Synthetic directory entries: root, repo dirs.
    synthetic_dirs: HashMap<InodeId, SyntheticDir>,
    /// Repo name → RepoEntry.
    repos: HashMap<String, RepoEntry>,
    /// Slot → live workspace KikiFs. Populated lazily.
    live: HashMap<u32, WorkspaceLive>,
    /// Next slot to allocate. Monotonic, never reused (§12.2 #3).
    next_slot: u32,  // starts at 1
}

struct SyntheticDir {
    parent: InodeId,
    name: String,
    children: BTreeMap<String, InodeId>,
}

struct RepoEntry {
    name: String,
    url: String,
    inode: InodeId,                  // synthetic dir inode
    store: Arc<GitContentStore>,     // shared
    remote_store: Option<Arc<dyn RemoteStore>>,
    workspaces: HashMap<String, WorkspaceEntry>,
}

struct WorkspaceEntry {
    name: String,
    slot: u32,
    root_tree_id: Vec<u8>,
    op_id: Vec<u8>,
    workspace_id: Vec<u8>,
    // KikiFs is in RootFsInner.live[slot] when hydrated
}

struct WorkspaceLive {
    repo_name: String,
    workspace_name: String,
    fs: Arc<KikiFs>,
    local_refs: Arc<LocalRefs>,
}
```

### 12.4 Inode dispatch

Every `JjKikiFs` method on `RootFs` begins with inode dispatch:

```rust
const SLOT_BITS: u32 = 40;
const SLOT_MASK: u64 = (1u64 << SLOT_BITS) - 1;

fn slot_of(ino: InodeId) -> u32 {
    (ino >> SLOT_BITS) as u32
}

fn to_local(ino: InodeId) -> InodeId {
    ino & SLOT_MASK
}

fn to_global(slot: u32, local: InodeId) -> InodeId {
    ((slot as u64) << SLOT_BITS) | local
}
```

For operations that take an `InodeId`:

```rust
async fn lookup(&self, parent: InodeId, name: &str)
    -> Result<InodeId, FsError>
{
    let slot = slot_of(parent);
    if slot == 0 {
        self.synthetic_lookup(parent, name).await
    } else {
        let fs = self.get_or_hydrate(slot).await?;
        let local = fs.lookup(to_local(parent), name).await?;
        Ok(to_global(slot, local))
    }
}
```

`synthetic_lookup` on a repo dir returns
`to_global(workspace.slot, ROOT_INODE)` — the workspace's root
inode in global space. Subsequent FUSE operations on that inode
dispatch to the workspace's `KikiFs` via the slot.

**Translation rule: every `InodeId` crossing the `RootFs` ↔
`KikiFs` boundary must be converted.** This includes:

- `InodeId` arguments to delegated calls (parent, ino) →
  `to_local` before calling `KikiFs`.
- `InodeId` return values from `lookup` → `to_global`.
- `DirEntry.inode` in every entry returned by `readdir` →
  `to_global`.
- **`Attr.inode`** in every `Attr` returned by `getattr`,
  `create_file`, `mkdir`, `symlink`, and `setattr` → `to_global`.
  The FUSE adapter uses `Attr.inode` directly to build
  `FileAttr.ino` (`fuse_adapter.rs:103–112`); returning a local
  inode here would cause the kernel to cache a wrong inode number.

For `rename`, if the source and destination slots differ, return a
new `FsError::CrossDevice` (maps to `EXDEV`).

All mutation operations on synthetic directories — `create_file`,
`mkdir`, `symlink`, `write`, `setattr`, `remove`, and `rename` —
return `FsError::PermissionDenied` (maps to `EACCES`). This
includes attempts to `rmdir` a workspace or repo directory, or to
`rename` one. The namespace is managed exclusively via kiki CLI
commands (`kiki clone`, `kiki workspace create/delete`), which go
through gRPC RPCs. FUSE-level mutations are rejected so that `rm
-rf /mnt/kiki/myrepo` doesn't accidentally destroy state.

`check_out` and `snapshot` are never called through the FUSE
adapter — they're called by the gRPC service directly on the
per-workspace `KikiFs`. `RootFs` implements them as
`Err(FsError::PermissionDenied)` (unreachable in practice).

### 12.5 `FsError` additions

Two new variants in `daemon/src/vfs/kiki_fs.rs`:

```rust
pub enum FsError {
    // ... existing variants ...
    PermissionDenied,   // → libc::EACCES
    CrossDevice,        // → libc::EXDEV
}
```

And the corresponding arms in `fs_err_to_errno`
(`daemon/src/vfs/fuse_adapter.rs:76`) and in `fs_err_to_nfs` in
the NFS adapter (`daemon/src/vfs/nfs_adapter.rs`). Even though
`RootFs` is Linux-FUSE-only for M12, both adapters have
exhaustive matches on `FsError` and must compile.
`PermissionDenied` → `NFS3ERR_ACCES`, `CrossDevice` →
`NFS3ERR_XDEV`.

### 12.6 Lazy hydration

`RootFs` registers workspace slots at startup (from persisted
config) but does not instantiate `KikiFs` until first access.

The `get_or_hydrate(slot)` method uses two-phase hydration to
avoid holding a lock across I/O:

1. **Phase 1 (under lock):** check `live[slot]`. If present,
   clone the `Arc<KikiFs>` and return. If absent, copy the
   `WorkspaceEntry` metadata (slot, root_tree_id, repo name)
   and the shared `Arc<GitContentStore>` / remote refs out of
   the lock. Drop the lock.
2. **Phase 2 (no lock, async-safe):** open the shared repo-level
   `store.redb` for `LocalRefs` (if not already cached on the
   `RepoEntry`). Call `KikiFs::new(store,
   root_tree_id, remote, scratch_dir)`. This may involve I/O
   (redb open, and for `kiki+ssh://` repos a network round-trip to
   fetch the root tree if it isn't cached locally).
3. **Phase 3 (re-acquire lock):** check `live[slot]` again — a
   concurrent hydration may have raced and won. If still absent,
   insert the new `Arc<KikiFs>` and return it. If already
   present (another thread won the race), discard the duplicate
   and return the existing one.

This is the standard double-checked-locking pattern. The inner
mutex is `parking_lot::Mutex` (matches `InodeSlab`). Phases 1
and 3 are fast map operations under the lock; phase 2 is
arbitrarily slow but lock-free.

The latency of first access depends on the tree — a local
`dir://` repo hydrates in microseconds; an `kiki+ssh://` repo may
take a network round-trip. This blocks the FUSE syscall (e.g.,
`stat` or `readdir`) for the calling process, which is
acceptable for a one-time cost on interactive `cd`.

### 12.7 Storage layout

Before (M10.7, per-mount flat):

```
~/.local/state/kiki/
  mounts/
    wc-<blake3hex>/
      mount.toml
      store.redb
      git_store/
      scratch/
```

After (M12, per-repo grouped):

```
~/.local/state/kiki/
  repos.toml                           # repo registry
  repos/
    monorepo/                          # repo name
      git_store/                       # shared bare git repo
      store.redb                       # shared redb (op-store, LocalRefs)
      workspaces/
        default/
          workspace.toml               # per-workspace metadata
          scratch/                     # per-workspace redirections
        fix-auth/
          workspace.toml
          scratch/
    dotfiles/
      git_store/
      store.redb
      workspaces/
        default/
          workspace.toml
```

**`store.redb` is per-repo, not per-workspace.** Today
`GitContentStore::init` (`daemon/src/git_store.rs:91`) opens the
redb file as the op-store database. jj's operation log, views,
and `LocalRefs` catalog live in this database. If each workspace
had its own redb, jj operations (commits, workspace additions)
would be invisible across workspaces — `jj workspace list` in
workspace B wouldn't see workspace A. A shared repo-level redb
means all workspaces see the same operation history, the same
views, and the same catalog refs. This matches jj's native model
where all workspaces in a repo share one op-store.

Per-workspace state is limited to `workspace.toml` (checkout
pointer, slot, root_tree_id) and the `scratch/` directory for
redirections. Everything content-addressed or operation-level is
shared.

#### `repos.toml`

```toml
# Auto-managed by the daemon. Safe to edit when daemon is stopped.
# Repo names must be valid directory components (no slashes, no
# leading dots, no reserved names).

[repos.monorepo]
url = "kiki+ssh://server/repos/monorepo"

[repos.dotfiles]
url = "dir:///home/cbro/repos/dotfiles"
```

#### `workspace.toml`

Replaces `mount.toml`. Same shape as `MountMetadata` minus the
`working_copy_path` field (now derived from
`<mount_root>/<repo>/<workspace>`), plus a `slot` field:

```toml
slot = 1
remote = "kiki+ssh://server/repos/monorepo"
op_id = "abcdef..."
workspace_id = "64656661756c74"
root_tree_id = "ff..."
```

The `slot` field is the workspace's global inode slot, persisted so
it's stable across daemon restarts. This matters because the kernel
dcache holds inodes from a previous session — if a workspace's slot
changes, every cached inode for that workspace becomes stale. Slots
are allocated from `next_slot` (monotonic, never reused — §12.2 #3).
The allocator state is persisted in `repos.toml`:

```toml
next_slot = 3

[repos.monorepo]
url = "kiki+ssh://server/repos/monorepo"
# ...
```

#### Migration from M10.7 layout

Existing `mounts/` directories are not automatically migrated. A
`kiki migrate` command (or startup-time migration) reads each
`mount.toml`, extracts the remote URL, derives a repo name, creates
the new directory structure, and moves the `git_store/` and
`store.redb`. The old `mounts/` directory is left in place as a
backup until manual cleanup.

Migration is optional for M12 — the old per-mount model remains
functional for ad-hoc mounts via `kiki kk init`. The managed
workspace model is additive.

### 12.8 Wire protocol

New RPCs added to the `JujutsuInterface` service in
`proto/jj_interface.proto`:

```proto
// Repo lifecycle
rpc Clone(CloneReq) returns (CloneReply);
rpc RepoList(RepoListReq) returns (RepoListReply);

// Workspace lifecycle
rpc WorkspaceCreate(WorkspaceCreateReq) returns (WorkspaceCreateReply);
rpc WorkspaceFinalize(WorkspaceFinalizeReq) returns (WorkspaceFinalizeReply);
rpc WorkspaceList(WorkspaceListReq) returns (WorkspaceListReply);
rpc WorkspaceDelete(WorkspaceDeleteReq) returns (WorkspaceDeleteReply);

message CloneReq {
    string url = 1;             // remote URL
    string name = 2;            // repo name (empty = derive from URL)
}
message CloneReply {
    string workspace_path = 1;  // e.g. "/mnt/kiki/monorepo/default"
    bytes initial_tree_id = 2;  // root tree for initial checkout (may be empty)
}

message RepoListReq {}
message RepoListReply {
    repeated RepoInfo repos = 1;
}
message RepoInfo {
    string name = 1;
    string url = 2;
    repeated string workspaces = 3;
}

message WorkspaceCreateReq {
    string repo = 1;            // repo name
    string workspace = 2;       // workspace name
    // No source_workspace or revision: the daemon handles slot
    // allocation + VFS registration only. The CLI owns jj workspace
    // initialization (commit resolution, checkout target) in step 8
    // of the WorkspaceCreate flow (§12.10). This avoids duplicating
    // jj-lib commit-resolution logic in the daemon.
}
message WorkspaceCreateReply {
    string workspace_path = 1;
}

message WorkspaceFinalizeReq {
    string repo = 1;
    string workspace = 2;
}
message WorkspaceFinalizeReply {}

message WorkspaceListReq {
    string repo = 1;            // repo name
}
message WorkspaceListReply {
    repeated WorkspaceInfo workspaces = 1;
}
message WorkspaceInfo {
    string name = 1;
    string path = 2;
}

message WorkspaceDeleteReq {
    string repo = 1;
    string workspace = 2;
}
message WorkspaceDeleteReply {}
```

Existing store/working-copy RPCs continue to use
`working_copy_path` as the routing key. The path is now the managed
namespace path: `/mnt/kiki/monorepo/default`. The service resolves
this to `(repo, workspace)` by stripping `mount_root` and splitting.

### 12.9 Service changes

`JujutsuService` (`daemon/src/service.rs`) gains:

- `root_fs: Arc<RootFs>` — the single filesystem for the FUSE mount
  **and the single owner of all repo/workspace state.** The service
  does not maintain a parallel registry.

**`RootFs` is the single source of truth.** The service queries it
for workspace state via methods on `RootFs`:

```rust
impl RootFs {
    // ── Lookup ──

    /// Look up the KikiFs + store for a workspace by (repo, ws) name.
    /// Hydrates the workspace lazily if needed (§12.6).
    pub async fn get_workspace(
        &self, repo: &str, ws: &str,
    ) -> Result<WorkspaceHandle, FsError>;

    // ── Lifecycle ──

    /// Register a new repo. Called by Clone RPC handler.
    pub fn register_repo(&self, name: String, entry: RepoEntry);

    /// Register a new workspace. Called by WorkspaceCreate RPC handler.
    pub fn register_workspace(
        &self, repo: &str, ws: String, entry: WorkspaceEntry,
    ) -> Result<(), FsError>;

    /// Remove a workspace. Called by WorkspaceDelete RPC handler.
    pub fn remove_workspace(
        &self, repo: &str, ws: &str,
    ) -> Result<(), FsError>;

    // ── Mutable workspace metadata ──
    //
    // These replace the direct field mutations that today's service
    // code does on the Mount struct (e.g. service.rs:1741–1752 for
    // SetCheckoutState, service.rs:1798–1815 for Snapshot).

    /// Update op_id + workspace_id. Called by SetCheckoutState RPC.
    /// Persists to workspace.toml atomically.
    pub fn set_checkout_state(
        &self, repo: &str, ws: &str,
        op_id: Vec<u8>, workspace_id: Vec<u8>,
    ) -> Result<(), FsError>;

    /// Update root_tree_id after CheckOut or Snapshot.
    /// Persists to workspace.toml atomically.
    pub fn set_root_tree_id(
        &self, repo: &str, ws: &str,
        root_tree_id: Vec<u8>,
    ) -> Result<(), FsError>;
}

/// Returned by get_workspace — bundles the per-workspace handles
/// the service needs for RPC dispatch.
pub struct WorkspaceHandle {
    pub fs: Arc<KikiFs>,
    pub store: Arc<GitContentStore>,
    pub remote_store: Option<Arc<dyn RemoteStore>>,
    pub local_refs: Arc<LocalRefs>,
}
```

The metadata-mutating methods update both the in-memory
`WorkspaceEntry` and the on-disk `workspace.toml` atomically
(same tmp+rename pattern as today's `MountMetadata::write_to`).
This ensures daemon restart rehydrates from current state, not
stale metadata.

This avoids dual-lock state that could diverge or deadlock. The
service's `resolve_workspace` helper calls
`root_fs.get_workspace(repo, ws)`, which handles hydration
internally.

The existing `mounts: HashMap<String, Mount>` is removed. The
`Mount` struct's fields distribute into `RepoEntry` (shared store,
remote, SSH tunnel) and `WorkspaceEntry` / `WorkspaceLive`
(per-workspace fs, local_refs, op_id, workspace_id, root_tree_id,
meta_path) — all owned by `RootFs`.

RPC handlers that need a workspace (most store and working-copy
RPCs) resolve `working_copy_path` → `(repo, workspace)`:

```rust
fn resolve_workspace(
    &self,
    working_copy_path: &str,
) -> Result<(String, String), Status> {
    let wc = Path::new(working_copy_path);
    let rel = wc
        .strip_prefix(&self.mount_root)
        .map_err(|_| Status::not_found("path outside mount root"))?;
    let mut components = rel.components();
    let repo = components
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .ok_or_else(|| Status::invalid_argument("missing repo component"))?;
    let ws = components
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .ok_or_else(|| Status::invalid_argument("missing workspace component"))?;
    // Trailing components are ignored — the first two identify
    // the workspace. A path like /mnt/kiki/repo/ws/src/main.rs
    // routes to (repo, ws). This matters because jj and git
    // commands send the cwd as working_copy_path, which is
    // typically a subpath within the workspace.
    Ok((repo.to_string(), ws.to_string()))
}
```

Uses `Path::strip_prefix` (component-level, not byte-level) to
avoid string-prefix mismatches on sibling directories.

The `store_for` and `mount_handles` helpers
(`daemon/src/service.rs:768,781`) change from a flat `HashMap` lookup
to `resolve_workspace` → repo lookup → workspace lookup.

### 12.10 Mount lifecycle

#### Daemon startup

1. Read `repos.toml` (or create empty if first run).
2. Scan `repos/<name>/workspaces/<ws>/workspace.toml` for each repo.
3. Construct `RootFs` with the repo and workspace registrations (no
   `KikiFs` instances yet — all lazy).
4. Create or validate `mount_root` directory.
5. **Spawn the `VfsManager` task** (its `serve()` loop must be
   polling the channel before any `bind` request is sent —
   otherwise `VfsManagerHandle::bind` blocks waiting for a reply
   that nobody is processing). This matches the current
   `run_daemon()` structure where `VfsManager::start()` returns the
   handle before the gRPC server runs, but the bind call below
   must happen *after* the VFS manager task is spawned.
6. Bind FUSE mount at `mount_root` via `VfsManagerHandle::bind`,
   passing `Arc<RootFs>` as the filesystem.
7. Store the returned `MountAttachment` in `JujutsuService` (see
   below). This RAII value keeps the FUSE mount alive; dropping it
   unmounts.
8. Ready to serve RPCs.

**`MountAttachment` ownership.** Today each `Mount` owns its
`attachment: Option<MountAttachment>`. M12 removes per-workspace
`Mount` structs, so the single `MountAttachment` for the RootFs
FUSE mount moves to `JujutsuService`:

```rust
pub struct JujutsuService {
    root_fs: Arc<RootFs>,
    /// Keeps the single FUSE mount alive for the daemon's lifetime.
    /// Dropped on daemon shutdown, which unmounts.
    _mount_attachment: Option<MountAttachment>,
    // ...
}
```

The `_mount_attachment` is set once during startup (step 7) and
never touched again — it exists solely for its `Drop` impl.

The VFS manager binds exactly one mount for the daemon's lifetime,
replacing the current per-`Initialize` binding pattern.

#### `Clone` flow

1. CLI calls `Clone { url, name }`.
2. Service validates name (no duplicates, valid dir component).
3. Create `repos/<name>/` directory.
4. Open shared `GitContentStore` at `repos/<name>/git_store/` and
   shared `store.redb` at `repos/<name>/store.redb`.
5. Establish remote connection (SSH tunnel if `kiki+ssh://`, etc.).
6. Fetch initial content from remote if reachable (root tree).
7. Allocate workspace slot for `default`.
8. Create `repos/<name>/workspaces/default/` with `workspace.toml`.
9. Register repo + workspace with `RootFs` (so `ls /mnt/kiki/`
   shows the new repo, `ls /mnt/kiki/<name>/` shows `default`).
10. Persist `repos.toml`.
11. Return `CloneReply { workspace_path, initial_tree_id }`.
12. CLI calls `Workspace::init_with_factories` at `workspace_path`
    (same as current `kiki kk init` post-Initialize flow).
13. If `initial_tree_id` is non-empty, CLI issues `CheckOut` RPC
    with that tree — materializes content in the FUSE mount and
    persists `root_tree_id` to `workspace.toml`. This ensures the
    workspace survives daemon restarts with correct content.

#### `WorkspaceCreate` flow

1. CLI calls `WorkspaceCreate { repo, workspace }`.
2. Daemon validates: repo exists, workspace name doesn't collide.
3. Daemon allocates workspace slot (monotonic `next_slot++`).
4. Daemon creates per-workspace directory + `workspace.toml`
   with `state = "pending"`.
5. Daemon registers with `RootFs` in **pending state** — the
   workspace exists in the routing table but `readdir` on the
   repo dir does not include it, and `lookup` for its name
   returns `ENOENT`. This prevents users or tools from accessing
   a half-initialized workspace.
6. Daemon persists `repos.toml` (updated `next_slot`),
   `workspace.toml`.
7. Daemon returns `WorkspaceCreateReply { workspace_path }`.
8. **CLI owns jj workspace initialization.** The CLI resolves the
   checkout target (default: parents of the source workspace's
   working-copy commit, matching `jj workspace add` behavior;
   override via `--revision` flag). Then it calls jj-lib's
   workspace-add flow at `workspace_path`: new `WorkspaceName`,
   shared repo backend, checkout at the resolved commit. The
   daemon does not need `source_workspace` or `revision` — it
   handles slot allocation and VFS registration only.
9. CLI calls `WorkspaceFinalize { repo, workspace }` RPC.
   Daemon transitions workspace from `pending` to `active` —
   it now appears in `readdir` and `lookup` succeeds.

If the CLI crashes between steps 7 and 9, the workspace remains
in `pending` state. On daemon restart, pending workspaces are
either cleaned up automatically (delete the workspace directory
and retire the slot) or left for manual cleanup via `kiki
workspace delete`. The `workspace.toml` `state` field
distinguishes the two:

```toml
state = "pending"   # or "active"
slot = 3
# ...
```

#### `WorkspaceDelete` flow

1. CLI calls `WorkspaceDelete { repo, workspace }`.
2. Service validates: workspace exists, is not the only workspace
   (can't delete `default` if it's the last one — or can we?).
3. If workspace is hydrated: drop the `KikiFs`, deregister from
   `RootFs.live`. In-flight FUSE ops on that workspace's inodes
   will get `ENOENT` after this point; the kernel will clean up.
4. Retire the slot (monotonic, never reused — §12.2 #3).
5. Remove per-workspace directory.
6. Persist `repos.toml`.

### 12.11 jj workspace integration

Each kiki workspace maps 1:1 to a jj workspace in a shared jj repo.

**`kiki clone` → jj repo init.** The CLI calls
`Workspace::init_with_factories` with the same factory closures as
today's `kiki kk init` (lines 544–648 of `cli/src/main.rs`):
`KikiBackend`, `KikiOpStore`, `KikiOpHeadsStore`,
`KikiWorkingCopyFactory`. The workspace path is
`/mnt/kiki/<repo>/default/`. jj-lib writes `.jj/` metadata through
the VFS. `WorkspaceName::DEFAULT` is used for the first workspace.

**`kiki workspace create` → jj workspace add.** The daemon handles
slot allocation and VFS registration (steps 1–7 of §12.10
`WorkspaceCreate` flow). The CLI then owns jj workspace
initialization (step 8): resolve the checkout target, call jj-lib's
workspace-add flow at the returned `workspace_path`. The default
checkout target is the parents of the source workspace's
working-copy commit, matching `jj workspace add` behavior
(`jj-vcs/jj` `cli/src/commands/workspace/add.rs:179–196`). An
explicit `--revision` CLI flag overrides this. The store factories
are the same — `KikiBackend` etc., all routing through the daemon
via gRPC. The workspace name in jj matches the kiki workspace name
for sanity.

**`jj workspace list` inside a kiki workspace.** Works because jj
sees the `.jj/` metadata through the VFS and queries the shared
repo. The workspace list comes from jj's internal tracking, which is
kept in sync by the fact that each `kiki workspace create` also
creates a jj workspace.

**`jj` commands inside a workspace.** `jj status`, `jj log`,
`jj diff`, `jj new`, `jj describe` all work as today — the
workspace is a normal jj working copy, just FUSE-served. The store
factories discover the daemon connection by climbing from
`.jj/repo/op_heads/` to the workspace root (existing logic in
`create_store_factories` at `cli/src/main.rs`), which resolves to
`/mnt/kiki/<repo>/<workspace>/`.

### 12.12 Configuration

`~/.config/kiki/config.toml` gains:

```toml
# Mount root for the managed namespace.
# Default: /mnt/kiki
# mount_root = "/mnt/kiki"

# Already existing fields:
# storage_dir = "..."
# grpc_addr = "..."
# [nfs]
# min_port = ...
```

The `mount_root` is read by the daemon at startup and by the CLI
for path construction. The CLI reads it to build workspace paths
for display and for `working_copy_path` routing in RPCs.

### 12.13 Test strategy

- **Synthetic readdir** — mount `RootFs` with two repos, three
  workspaces. Assert `readdir(root)` returns repo names.
  `readdir(repo_inode)` returns workspace names. Inode numbers
  are in the correct slot ranges.

- **Workspace delegation** — `lookup(repo, "default")` returns a
  global inode in slot S. `readdir` on that inode returns the
  workspace's file listing (delegates to `KikiFs`). File content
  reads return correct data.

- **Lazy hydration** — register a workspace without hydrating.
  Assert `readdir(repo)` shows the workspace name. First
  `lookup(repo, ws_name)` triggers hydration. Subsequent lookups
  are fast (no re-hydration).

- **Inode translation round-trip** — property test:
  `to_local(to_global(slot, local)) == local` and
  `slot_of(to_global(slot, local)) == slot` for all valid slot/local
  pairs. Verify slot 0 inodes are never produced by workspace
  dispatch.

- **Cross-workspace rename** — attempt `rename` with source in
  workspace A and destination in workspace B. Assert `EXDEV`.

- **Synthetic dir mutation rejection** — attempt `create_file`,
  `write`, `mkdir`, `symlink`, `remove`, `rmdir`, and `rename` on
  the root dir, repo dirs, and workspace root entries. Assert
  `EACCES` for all. Verify `rm -rf /mnt/kiki/myrepo` fails without
  damaging state.

- **Clone end-to-end** — `kiki clone dir:///tmp/test-repo`. Assert
  repo appears in `repos.toml`, `ls /mnt/kiki/<name>/` shows
  `default`, `jj status` inside the workspace works.

- **Workspace create end-to-end** — after clone, `kiki workspace
  create <repo>/fix-auth`. Assert workspace appears in readdir,
  `jj status` works inside it, files are readable, writes are
  independent of the `default` workspace's dirty state.

- **Workspace delete** — create then delete a workspace. Assert it
  disappears from readdir, its storage directory is removed, and the
  repo's other workspaces are unaffected. Create another workspace
  afterward and assert it gets a *new* slot (higher than the deleted
  one), confirming slots are retired, not reused (§12.2 #3).

- **Shared store** — write a file in workspace A, snapshot. Read the
  same file's blob from workspace B (via the shared git store).
  Assert identical content. Verify only one `git_store/` exists per
  repo.

- **Daemon restart (structure)** — clone, create workspace. Stop
  daemon. Restart. Assert both workspaces are visible in readdir.

- **Daemon restart (file content)** — clone, write+commit files
  (triggers Snapshot+CheckOut, persisting root_tree_id). Restart.
  Assert file content survives (KikiFs rehydrated from workspace.toml).

- **Git clone content persistence** — git clone, verify content is
  immediately visible (initial_tree_id CheckOut). Restart daemon,
  verify content survives.

- **Concurrent workspace access** — two tasks reading/writing in
  different workspaces of the same repo simultaneously. Assert no
  deadlocks, no cross-contamination of dirty state.

### 12.14 Commit plan

1. `docs/M12-WORKSPACES.md` (this spec).
2. `FsError` additions (`PermissionDenied`, `CrossDevice`) +
   `fs_err_to_errno` mapping in `fuse_adapter.rs` + `fs_err_to_nfs`
   mapping in `nfs_adapter.rs`.
3. Storage layout: `RepoConfig`, `WorkspaceConfig` types, directory
   helpers in `mount_meta.rs` (or new `repo_meta.rs`), `repos.toml`
   read/write, `workspace.toml` read/write.
4. `RootFs` struct: synthetic dir layer, inode dispatch
   (`slot_of`/`to_local`/`to_global`), `JjKikiFs` impl with
   delegation to per-workspace `KikiFs`, lazy hydration via
   `get_or_hydrate`. Unit tests for inode translation and synthetic
   readdir.
5. Proto: `Clone`, `WorkspaceCreate/List/Delete`, `RepoList` RPCs.
6. Service: repo + workspace management in `JujutsuService`,
   `resolve_workspace` routing, single-mount lifecycle via `RootFs`,
   rehydration from `repos.toml` + `workspace.toml`. Replace
   `mounts: HashMap<String, Mount>` with repo/workspace model.
7. CLI: `kiki clone`, `kiki workspace create/list/delete` commands,
   jj workspace initialization flows.
8. Integration tests: clone, workspace create/delete, shared store,
   daemon restart.
9. PLAN.md §12 outcome.

### 12.15 Pickup notes

What is already in place from prior milestones:

- **`JjKikiFs` trait** (`daemon/src/vfs/kiki_fs.rs:174`): the
  async trait that `RootFs` will implement. All 15 methods plus
  `root()`.
- **`KikiFs`** (`daemon/src/vfs/kiki_fs.rs:317`): the per-workspace
  filesystem. Unchanged by this milestone — `RootFs` wraps it.
- **`FuseAdapter`** (`daemon/src/vfs/fuse_adapter.rs:43`): takes
  `Arc<dyn JjKikiFs>`. No changes needed — it will receive
  `Arc<RootFs>` which impls `JjKikiFs`.
- **`InodeSlab`** (`daemon/src/vfs/inode.rs:130`): per-workspace
  inode allocator. Unchanged — uses `ROOT_INODE = 1` and monotonic
  `next_id` internally. `RootFs` remaps at the boundary.
- **`VfsManager`** (`daemon/src/vfs_mgr.rs`): creates FUSE mounts.
  Currently called per-`Initialize`; will be called once at startup
  for `RootFs`. The `bind` signature
  (`bind(path, Arc<dyn JjKikiFs>)`) works as-is.
- **`GitContentStore`**: per-mount today, becomes per-repo. The
  constructor and API don't change; just the storage path moves from
  `mounts/wc-<hash>/git_store/` to `repos/<name>/git_store/`.
- **`MountMetadata`** (`daemon/src/mount_meta.rs`): current
  per-mount persistence. Superseded by `repos.toml` +
  `workspace.toml` but the serialization patterns (hex_bytes,
  atomic write) carry over.
- **`Workspace::init_with_factories`**: the jj-lib workspace init
  flow called by `kiki kk init` (`cli/src/main.rs:544`). Reused
  by `kiki clone` for the default workspace.
- **`create_store_factories`** (`cli/src/main.rs`): discovers the
  daemon connection from the workspace path. Works unchanged with
  managed namespace paths.
- **Auto-start** (`cli/src/daemon_client.rs`): `connect_or_start()`
  spawns the daemon if needed. The `kiki clone` command uses the
  same flow.

### 12.16 Open questions

1. **Workspace checkout default.** Resolved: the CLI owns commit
   resolution (§12.10 step 8, §12.11). Default: parents of the
   source workspace's working-copy commit, matching `jj workspace
   add`. Override via `--revision` CLI flag. The daemon proto does
   not carry source/revision — the daemon handles only slot
   allocation and VFS registration.

2. **Ad-hoc mounts alongside managed namespace.** Should `kiki kk
   init` continue to work for one-off mounts outside `/mnt/kiki/`?
   Probably yes for backward compat and testing, but it becomes a
   secondary path.

3. **Repo deletion.** `kiki workspace delete` removes a workspace.
   Should there be a `kiki repo remove` that removes the entire
   repo entry (all workspaces, shared store)? Probably, but it's a
   destructive operation that deserves a confirmation prompt.

4. **macOS bridge.** macOS users can't use `RootFs` (no FUSE) until
   a macOS FUSE adapter lands or until `RootFs` gets an NFS adapter.
   In the interim, should `kiki clone` on macOS fall back to
   per-workspace NFS mounts at the standard namespace paths? This
   gives the same path layout without the single-mount routing.
