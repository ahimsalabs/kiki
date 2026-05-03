# M14: Blobless git clone with on-demand fetch

Spec for making `kiki clone` of git remotes fast by default — fetch
commits + trees eagerly, blobs on demand, with background backfill
to full.

Depends on: M13 (git clone, landed), M11 (push queue, active — shares
the background-task pattern).

## Problem statement

`kiki clone git@github.com:org/repo.git` currently runs a full
`git fetch origin` — every blob in the repo. For kubernetes-scale
repos (~1GB+), this makes clone unusable. The kiki-native remote
path already has lazy read-through for blobs, but git clones get
no equivalent.

Git's partial clone protocol (`--filter=blob:none`) solves exactly
this: fetch commits and trees eagerly, configure the remote as a
"promisor," and fetch individual blobs on demand when accessed.

kiki's VFS already has the architecture for on-demand fetch (the
`read_file` / `read_tree` / `read_symlink` fallback path in
`kiki_fs.rs`). The missing piece is a `RemoteStore`-compatible
adapter that fetches from a git remote instead of a kiki/S3 remote.

## Clone modes

Every kiki repo has a **fetch mode** persisted in its config:

| Mode | Eager fetch | Lazy | Use case |
|------|-------------|------|----------|
| `full` | commits + trees + blobs | nothing | small repos, offline |
| `blobless` | commits + trees | blobs | default for git clones |
| `treeless` | commits only | trees + blobs | very large repos |
| `minimal` | refs only | everything | monorepos, fastest clone |

The mode controls:
1. What filter is passed to `git fetch` on clone and subsequent fetches.
2. Whether the `GitRemoteFetcher` is wired as the fallback in `KikiFs`.
3. Whether a backfill task is spawned.

### Mode progression

Modes progress toward `full` over time via background backfill:

```
minimal → treeless → blobless → full
```

Each transition fires when the backfill cursor reaches the end
of the graph for that object type. The mode is a high-water mark —
once `full`, no further background work is needed.

### Configuration

Per-repo, persisted in `workspace.toml` (or a new `repo.toml`
alongside `repos.toml`):

```toml
[fetch]
mode = "blobless"       # current mode
target = "full"         # what backfill is working toward
```

If `target` is omitted or equals `mode`, no backfill runs.
Setting `target = "blobless"` on a `minimal` repo backfills
trees but not blobs.

## CLI changes

```bash
# Clone with explicit mode
kiki clone --fetch=blobless git@github.com:org/repo.git
kiki clone --fetch=full git@github.com:org/repo.git
kiki clone --fetch=treeless git@github.com:org/repo.git
kiki clone --fetch=minimal git@github.com:org/repo.git

# Default: blobless (with background backfill to full)
kiki clone git@github.com:org/repo.git

# Change mode on existing repo
kiki repo set-fetch-mode blobless    # evicts nothing, just stops backfill
kiki repo set-fetch-mode full        # triggers immediate full fetch

# Check status
kiki repo info
#   fetch mode: blobless (backfill: 73% → full)
```

### Default selection

The default mode for `kiki clone <git-url>` is `blobless` with
`target = full`. This gives:
- Fast clone (commits + trees only, typically <10% of repo size)
- Instant `ls`, `jj log`, `git log`, blame (trees are local)
- First file read triggers a single-blob fetch (~instant for
  typical files)
- Background backfill silently fills the rest; small repos reach
  `full` within seconds/minutes

Future: server-side hint via a well-known ref
(`refs/kiki/clone-hint`) or HTTP response header. Repos can
advertise their recommended mode. Not required for M14.

Future: heuristic based on fetch pack size — if the blobless
fetch is under some threshold, just do a full fetch instead.

## Architecture: `GitRemoteFetcher`

A new struct implementing a subset of the fetch pattern (not the
full `RemoteStore` trait — git remotes don't support `put_blob`,
`cas_ref`, etc.):

```rust
/// Fetches git objects on demand from a configured promisor remote.
///
/// Uses `git cat-file --batch` in long-running mode for efficient
/// single-object fetches. Git's promisor-remote mechanism handles
/// the actual network fetch transparently — if the object isn't
/// in the local ODB, git fetches it from the promisor remote.
#[derive(Debug)]
struct GitRemoteFetcher {
    git_repo_path: PathBuf,
    /// Long-running `git cat-file --batch` subprocess.
    /// Fed object IDs, returns object content.
    cat_file: Mutex<CatFileProcess>,
}
```

### Why `git cat-file --batch`?

When a git repo is configured as a partial clone (has a promisor
remote), `git cat-file -p <oid>` transparently fetches missing
objects from the promisor remote. This is git's built-in lazy
fetch mechanism — we don't need to implement the pack protocol
ourselves.

A long-running `--batch` process avoids subprocess spawn overhead
per blob. Feed it `<oid>\n`, read back the object. Git handles
the promisor fetch internally.

### Fallback: batch fetch

For pathological access patterns (user runs `find . -exec cat`),
single-object fetch is inefficient. Git supports batch promisor
fetch via `git fetch origin <oid1> <oid2> ...`. The fetcher can
detect "many sequential misses" and switch to batch mode:

```rust
impl GitRemoteFetcher {
    /// Single-object fetch via cat-file (promisor handles network).
    async fn get_blob(&self, id: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Batch prefetch — hint that these OIDs will be needed soon.
    /// Fetches them in one pack negotiation round-trip.
    async fn prefetch(&self, ids: &[&[u8]]) -> Result<()>;
}
```

### Integration with KikiFs

The `KikiFs` already has `remote: Option<Arc<dyn RemoteStore>>`.
For git-cloned repos in blobless/treeless/minimal mode, we wire
a `GitRemoteFetcher` into this slot.

But `GitRemoteFetcher` doesn't implement full `RemoteStore` (no
write side). Two options:

**Option A: Wrapper that implements `RemoteStore`**

```rust
impl RemoteStore for GitRemoteFetcherAdapter {
    async fn get_blob(&self, kind: BlobKind, id: &[u8]) -> Result<Option<Bytes>> {
        // Delegate to git cat-file
        self.inner.get_blob(id).await.map(|opt| opt.map(Bytes::from))
    }
    async fn put_blob(&self, ..) -> Result<()> {
        // No-op or error — git remote is read-only from kiki's perspective
        Ok(())
    }
    // ... other methods return errors or no-ops
}
```

**Option B: Separate trait for read-only remotes**

```rust
trait BlobFetcher: Send + Sync + Debug {
    async fn get_blob(&self, kind: BlobKind, id: &[u8]) -> Result<Option<Bytes>>;
}

// KikiFs field becomes:
fetcher: Option<Arc<dyn BlobFetcher>>,
```

Option A is less invasive (KikiFs doesn't change). Option B is
cleaner but touches more code. **Go with A** — a thin adapter
that panics on write methods (they should never be called; git
remotes are pull-only).

## Background task infrastructure

### `FetchPool` — cooperative priority

```rust
/// Shared per-endpoint concurrency budget.
///
/// Foreground (on-demand VFS reads) always gets permits.
/// Background (backfill, prefetch) only runs when there's spare
/// capacity AND no foreground pressure. This gives natural
/// priority without explicit scheduling.
pub struct FetchPool {
    sem: Arc<Semaphore>,
    /// Number of foreground fetches currently waiting for a permit.
    /// When > 0, background tasks stop acquiring.
    pressure: AtomicU32,
}

impl FetchPool {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(max_concurrent)),
            pressure: AtomicU32::new(0),
        }
    }

    /// Foreground: acquire a permit, waiting if necessary.
    /// Increments pressure while waiting so background backs off.
    pub async fn fg(&self) -> OwnedSemaphorePermit {
        self.pressure.fetch_add(1, Ordering::Relaxed);
        let permit = self.sem.clone().acquire_owned().await.unwrap();
        self.pressure.fetch_sub(1, Ordering::Relaxed);
        permit
    }

    /// Background: try to acquire without waiting.
    /// Returns None if budget is full OR foreground is starving.
    pub fn bg_try(&self) -> Option<OwnedSemaphorePermit> {
        if self.pressure.load(Ordering::Relaxed) > 0 {
            return None;
        }
        self.sem.clone().try_acquire_owned().ok()
    }
}
```

### Task lifecycle

```rust
enum TaskKind { Backfill, PushQueue, GitFetch, Gc }

struct BgTask {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

/// Owned by the daemon. Manages all per-repo background work.
struct BgTasks {
    tasks: HashMap<(String, TaskKind), BgTask>,
}

impl BgTasks {
    fn spawn(&mut self, repo: &str, kind: TaskKind, fut: impl Future);
    fn cancel(&mut self, repo: &str, kind: TaskKind);
    fn cancel_repo(&mut self, repo: &str);  // all tasks for a repo
    async fn shutdown_all(&mut self);        // graceful daemon shutdown
}
```

### Backfill cursor (persistence)

New redb table in the per-repo store:

```
backfill_state_v1: () → BackfillState (single entry)
```

```rust
struct BackfillState {
    /// Commits whose trees have been fully walked and all blobs
    /// confirmed present. The cursor is the frontier — commits
    /// reachable from tips that haven't been processed yet.
    processed_commits: HashSet<[u8; 20]>,  // compacted on write
    /// Total blobs fetched by backfill (progress reporting).
    blobs_fetched: u64,
    /// Total bytes fetched by backfill.
    bytes_fetched: u64,
}
```

On startup, for each repo with `mode != target`:
1. Load `BackfillState` from redb (or start fresh).
2. Spawn a `Backfill` task with the repo's `FetchPool`.

### Backfill algorithm

```
for each tip commit (from refs/remotes/origin/*):
    walk commit graph (BFS from tips, skip processed_commits):
        for each commit:
            if mode is treeless/minimal:
                fetch commit's root tree (recursive)
            for each blob OID in the tree:
                if not in local ODB:
                    acquire bg permit (back off if fg pressure)
                    fetch via GitRemoteFetcher
            mark commit as processed
            persist cursor every N commits (batch persistence)

when all tips processed:
    if mode was minimal → set mode to treeless, restart for trees
    if mode was treeless → set mode to blobless, restart for blobs
    if mode was blobless → set mode to full, stop
```

### Concurrency budget defaults

| Remote | Max concurrent | Rationale |
|--------|---------------|-----------|
| github.com | 8 | GitHub's pack endpoint handles parallel well |
| Other forges | 4 | Conservative default |
| Local (file://) | 1 | No network, just ODB reads |

Configurable per-remote in a future pass. Hardcoded defaults for M14.

## Wire protocol changes

### `GitCloneReq` extension

```protobuf
message GitCloneReq {
    string git_url = 1;
    string name = 2;
    string kiki_remote = 3;
    FetchMode fetch_mode = 4;      // NEW
    FetchMode backfill_target = 5;  // NEW (default: FULL)
}

enum FetchMode {
    FETCH_MODE_UNSPECIFIED = 0;  // daemon picks default (blobless)
    FULL = 1;
    BLOBLESS = 2;
    TREELESS = 3;
    MINIMAL = 4;
}
```

### New RPC: `SetFetchMode`

```protobuf
rpc SetFetchMode(SetFetchModeReq) returns (SetFetchModeReply) {}

message SetFetchModeReq {
    string repo = 1;
    FetchMode mode = 2;
    FetchMode target = 3;  // 0 = same as mode (no backfill)
}
```

### `RepoInfo` extension

```protobuf
message RepoInfoReply {
    // ... existing fields ...
    FetchMode fetch_mode = 10;
    FetchMode backfill_target = 11;
    float backfill_progress = 12;  // 0.0–1.0
}
```

## Daemon-side flow changes

### `git_clone` handler (updated)

Change in step 4 (the `git fetch` call):

```rust
// Before (M13):
let bookmarks = crate::git_ops::fetch(&git_path, "origin")?;

// After (M14):
let filter = match req.fetch_mode {
    FetchMode::Full | FetchMode::Unspecified => None,
    FetchMode::Blobless => Some("blob:none"),
    FetchMode::Treeless => Some("tree:0"),
    FetchMode::Minimal => Some("blob:none"),  // minimal still needs refs
};
let bookmarks = crate::git_ops::fetch_filtered(&git_path, "origin", filter)?;
```

New after registration:

```rust
// Wire up GitRemoteFetcher if mode != full
if effective_mode != FetchMode::Full {
    let fetcher = GitRemoteFetcher::new(git_path.clone())?;
    root_fs.set_fetcher(&name, Arc::new(fetcher));
}

// Spawn backfill if target > mode
if target != effective_mode {
    self.bg_tasks.spawn_backfill(&name, &store, &pool);
}
```

### `git_ops` changes

```rust
/// Fetch with an optional filter (partial clone).
pub fn fetch_filtered(
    git_repo_path: &Path,
    remote: &str,
    filter: Option<&str>,
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut cmd = Command::new("git");
    cmd.args(["fetch", remote]);
    if let Some(f) = filter {
        cmd.args(["--filter", f]);
    }
    cmd.current_dir(git_repo_path);
    // ... rest same as existing fetch()
}
```

## `kiki git fetch` changes

Subsequent fetches respect the repo's mode:

```rust
// In the GitFetch handler:
let filter = repo_config.fetch_mode.to_git_filter();
git_ops::fetch_filtered(&git_path, remote, filter)?;
```

This ensures `kiki git fetch` doesn't accidentally download all
blobs on a blobless repo.

## Interaction with existing systems

### kiki-native remote (dual-remote repos)

If a repo has BOTH a git origin (blobless) AND a kiki remote:
- VFS read miss → try `GitRemoteFetcher` first (cheaper, same
  origin), fall back to kiki `RemoteStore`
- Write-through still goes to kiki remote (unchanged)
- Backfill fetches from git origin (saves kiki remote bandwidth)

### jj operations

jj-lib's `GitBackend` reads from the ODB. If it hits a missing
blob (because blobless), it will error. The VFS intercepts reads
before jj sees them, so this is fine for file content. But jj
operations that walk the tree directly (e.g., `jj diff` computing
a diff) need the blobs.

Resolution: the CLI commands that drive jj operations (`describe`,
`new`, `diff`, `log`) go through the daemon's `CheckOut` /
`Snapshot` / `Diff` RPCs. These read via `KikiFs` (which has the
fetcher fallback). Pure jj-lib reads that bypass the VFS (if any)
would need a `Backend` wrapper — investigate during implementation.

### Offline mode

If the git remote is unreachable, on-demand fetch fails. The VFS
surfaces this as `EIO` (same as a kiki remote being down). The
user sees files they haven't accessed as unreadable until
connectivity returns.

Mitigation: the backfill progressively makes this less likely.
A repo that's been idle for a while approaches `full` and is
effectively offline-safe.

## Implementation sequence

1. **`FetchPool` + `BgTasks`** — shared infra in `daemon/src/bg/`.
   No functional change, just the primitives. Unit tests for
   priority behavior.

2. **`git_ops::fetch_filtered`** — add `--filter` support to the
   existing fetch function. Backward compatible (None = full).

3. **`GitRemoteFetcher`** — long-running `git cat-file --batch`
   wrapper. Integration test: partial-clone a repo, fetch a
   missing blob.

4. **Wire `GitRemoteFetcher` into `KikiFs`** — adapter implementing
   `RemoteStore`, registered for repos with mode != full.

5. **`GitCloneReq` extension + CLI `--fetch` flag** — pass mode
   through, default to blobless.

6. **Backfill task** — cursor, commit-graph walk, blob check,
   fetch loop, mode transition on completion. Persistence in redb.

7. **`kiki repo set-fetch-mode` + `kiki repo info`** — mode
   management CLI.

8. **`kiki git fetch` respects mode** — don't accidentally
   full-fetch on a blobless repo.

9. **Progress reporting** — `backfill_progress` in `RepoInfo`,
   shown in `kiki repo info` and daemon status.

## Open questions

1. **`git cat-file --batch` vs `gix`** — gix may handle promisor
   fetches natively (it supports partial clone repos). If so, we
   can skip the subprocess and use gix's `find_object()` which
   will trigger a promisor fetch on miss. Needs investigation.
   Subprocess is the safe fallback.

2. **Tree prefetch on checkout** — when the user checks out a new
   commit (blobless mode), we know the full tree. Should we
   batch-prefetch all blobs in the tree? Probably yes for small
   trees, no for monorepo-sized trees. Threshold TBD.

3. **`minimal` mode and commit fetch** — `--filter=tree:0` still
   fetches all commits. True `minimal` (refs only) would need
   `--filter=tree:0 --filter=blob:none --depth=1` or similar.
   Git's protocol supports this but it's exotic. Defer until
   there's a real use case beyond the table symmetry.

4. **Eviction** — this spec only adds (fetch + cache). Eviction
   (remove cached blobs to free space) is a separate concern.
   Deferred — most users want to trend toward full, not away
   from it. Eviction matters for CI/ephemeral environments.

5. **Multiple git remotes** — if a repo has `origin` and
   `upstream`, which is the promisor? Probably just `origin`
   (the clone source). `git fetch upstream` would also be
   filtered if the repo mode says so.
