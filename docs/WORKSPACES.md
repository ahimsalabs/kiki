# Workspaces and Managed Namespace

Design notes for a CitC/EdenFS-style user experience on top of jj-yak:
many repos and many workspaces, presented under one managed namespace,
with lazy hydration and shared content storage.

This document is intentionally separate from [`PLAN.md`](./PLAN.md).
`PLAN.md` tracks the implementation sequence for the current daemon,
remote, and VFS milestones. This document captures a larger UX and
mount-topology direction that likely lands as its own milestone after
git convergence, M10.7, and M11.

## Goal

Make jj-yak feel like a managed workspace system rather than a
collection of one-off mounts:

```text
~/.yak/
  git@github.com/
    repo1/
      default/
      fix-auth/
    repo2/
      default/
  s3://company/
    monorepo/
      default/
      agent-task-1234/
```

The key properties:

- One user-facing namespace for all repos.
- Cheap workspace creation within a repo.
- Shared clean content across workspaces.
- Lazy file and tree fetching.
- Offline operation for cached and locally-written content.
- A path toward colocated git/jj metadata and better stock-tool
  compatibility after git convergence.

## Why this is separate

Today's architecture is effectively "one mount per repo". That is
enough for the current milestones and is a good place to get the core
VFS, sync, and git-convergent storage right.

The managed-namespace model adds a second layer of complexity:

- Routing multiple repos through one mount.
- Global inode identity.
- Repo discovery in the filesystem namespace.
- Lazy repo hydration on first access.
- Workspace lifecycle commands above the current `jj yak init` flow.

That deserves its own design doc instead of being buried in milestone
notes.

## User model

At the UX level, users should think in terms of:

- A managed root such as `~/.yak/` or `~/.kiki/mnt/`.
- Repos addressed by remote + repo name.
- Multiple named workspaces per repo.
- Workspace operations that are cheaper than cloning.

Illustrative commands:

```bash
kk clone s3://company/repos/api
# -> mounts or materializes ~/.yak/company/api/default/

kk workspace new fix-auth
# -> creates ~/.yak/company/api/fix-auth/

kk workspace list
kk workspace delete fix-auth

kk sync --prefetch
kk sync --prefetch --depth 50
```

The exact CLI name is not important here. The important part is that
workspace management becomes a top-level concept rather than an
incidental consequence of `jj yak init`.

## Architecture options

There are two plausible presentation models.

### Option A: many mounts, managed by CLI

Each workspace remains its own `YakFs` mount, but the CLI presents them
under a standardized namespace and manages creation/deletion.

Pros:

- Minimal change to the current daemon architecture.
- Reuses existing per-mount storage, metadata, and mount lifecycle.
- Lower risk in the short term.

Cons:

- One kernel mount per workspace.
- macOS NFS port management stays awkward.
- Discoverability is weaker.

### Option B: one top-level mount with a `RootFs`

Introduce a new filesystem layer above `YakFs`:

```rust
struct RootFs {
    repos: HashMap<RepoKey, Arc<RepoEntry>>,
}
```

`RootFs` owns the visible namespace and delegates operations into
workspace-specific `YakFs` instances.

Pros:

- One kernel mount for the whole namespace.
- Better fit for CitC/EdenFS-style workflows.
- Natural place for repo discovery and lazy hydration.

Cons:

- Requires global inode strategy.
- Requires path routing and lifecycle logic above `YakFs`.
- More moving parts around mount recovery and cache invalidation.

## Recommended sequencing

Short term: start with Option A for the first managed-workspace UX.

Long term: keep Option B as the architectural destination if the
single-mount namespace proves worth the extra complexity.

Reasoning:

- The hard technical work today is content storage, sync, git
  convergence, ignore handling, and offline behavior.
- Workspace UX can land earlier without blocking on a `RootFs`.
- A standardized on-disk namespace under many mounts still gives most
  of the user-visible benefit.

## `RootFs` model

If jj-yak graduates to the single-mount model, `RootFs` would be the
top-level router.

Responsibilities:

- Present remote/org/repo/workspace directories.
- Resolve a path prefix to the correct repo/workspace instance.
- Lazily instantiate or rehydrate the underlying `YakFs`.
- Maintain global inode identity for the mount.
- Surface repo discovery without requiring an explicit init step.

Non-responsibilities:

- Blob storage semantics.
- Commit/tree encoding.
- Remote arbitration.

Those stay in the existing store/remote layers.

## Global inode strategy

One mount means one inode namespace. Per-repo `YakFs` instances cannot
all expose `ROOT_INODE = 1` directly to the kernel.

Two broad approaches:

### 1. Range partitioning

Assign each repo or workspace a reserved inode range:

- workspace 0: `1 .. 2^48`
- workspace 1: `2^48 .. 2 * 2^48`
- ...

Pros:

- Simple mental model.
- Per-workspace `YakFs` can remain mostly unchanged if the range is
  threaded in at construction time.

Cons:

- Requires planning for workspace lifecycle and reuse of ranges.
- Feels artificial, though `u64` space is effectively unlimited here.

### 2. Boundary remapping

Keep local inode identities inside `YakFs`, and let `RootFs` remap them
to globally unique inode ids at the boundary.

Pros:

- Less invasive to `YakFs` internals.
- Global policy lives in one place.

Cons:

- Every inode-carrying operation crosses a translation layer.
- More bookkeeping for file handles, lookups, and invalidations.

Either approach is viable. Boundary remapping is cleaner if the goal is
to avoid reopening `YakFs` internals; range partitioning is cleaner if
we want the per-workspace FS to be directly mountable for debugging.

## Lazy repo hydration

This is the biggest UX win beyond the namespace itself.

Instead of eagerly initializing every repo/workspace:

- `lookup("company")` lists organizations/remotes.
- `lookup("company/api")` resolves repo metadata.
- First access to `default/` or another workspace triggers hydration.

Hydration can mean:

- Load existing local metadata if the repo/workspace is already known.
- Or create local metadata/store state on first access.
- Or fetch enough remote metadata to present the initial tree.

The important constraint is that "open the namespace" must stay cheap.
Listing `~/.yak/` cannot require mounting hundreds of repos eagerly.

## Workspace economics

Cheap workspaces depend on shared clean storage.

Desired properties:

- One repo's clean object store exists once locally.
- Multiple workspaces share those clean objects.
- Each workspace owns only mutable state:
  checkout pointer, op heads, dirty inodes, ignored materialized files,
  and any local metadata needed for offline recovery.

After git convergence, the natural shape is:

- Shared bare git repo or shared git object directory for the repo.
- Per-workspace checkout/VFS state.
- Per-workspace `.git`/`.jj` view presented through the mount.

This is the core reason workspace creation can be cheap: a new
workspace is mostly new pointers and dirty state, not a new full clone.

## Prefetch and offline behavior

Managed workspaces make prefetch policy more important, not less.

Baseline policy:

- Small repos: fetch the full current tree eagerly.
- Large repos: stay lazy by default.
- Explicit `sync --prefetch`: fetch the full current tree.
- Optional history prefetch: fetch the last N commits of history.

Monorepo heuristics should be client-side:

- Recent access patterns.
- Build/config roots such as `WORKSPACE`, `MODULE.bazel`,
  `BUILD`, `BUILD.bazel`, `BUCK`, `.buckconfig`, lockfiles.
- Possibly configured hot paths per repo.

The remote should stay a dumb object transport. The client decides what
to prefetch and when.

## Discovery

A managed namespace implies repo discovery.

Possible sources:

- Explicit local config mapping remotes to repo lists.
- Remote-side catalog service.
- Enumerating a shared object-store prefix plus metadata.
- Git forge APIs after git convergence.

This is intentionally unresolved. Discovery should not be entangled
with the core blob transport protocol.

## Stock tool compatibility

The long-term attractive idea is that a workspace could look enough
like a normal colocated repo that stock tools work:

- `git status`
- `jj log`
- editors, language servers, and build tools

After git convergence, this becomes much more plausible because the
daemon's backing store is real git content, not a custom translation
format.

But "plausible" is not "free":

- `jj` still expects repo metadata and backend semantics, not just
  visible files.
- `gix` and similar libraries do direct filesystem I/O and may notice
  behavioral differences through FUSE/NFS.
- Locking and filesystem edge cases matter.

So this should be treated as a design goal and evaluation topic, not as
an assumption baked into earlier milestones.

## Open questions

- Is the first managed-workspace UX many mounts under a namespace, or
  should the project skip directly to a `RootFs`?
- What is the repo discovery source of truth?
- Should workspaces share one bare repo directly, or use alternates?
- What inode strategy is simpler in practice once invalidation and file
  handles are included?
- How much build-system awareness is worth adding before real users
  force the issue?
- How should agent-created temporary workspaces be garbage-collected?

## Suggested implementation order

1. Ship git convergence, M10.7, and M11.
2. Add managed workspace CLI and standardized namespace layout with the
   existing one-mount-per-workspace model.
3. Add shared clean-object storage across workspaces of one repo.
4. Add explicit prefetch commands and repo-size-sensitive defaults.
5. Evaluate whether single-mount `RootFs` is worth the added kernel/VFS
   complexity.
6. If yes, build `RootFs` as a later milestone rather than folding it
   into the earlier workspace UX work.
