# jj-yak: Implementation Plan

Status: draft, under review
Last updated: 2026-04-25

This document captures the proposed roadmap for getting jj-yak from "scaffold
with stubs" to "usable read/write VCS", along with a review of the plan's
assumptions against the current code and external feasibility risks.

## 1. Big picture

The project's goal is a daemon that serves the working copy over NFS, backed
by storage that eventually goes to a remote. Three orthogonal layers:

```
┌─────────────────────────────────────────────────────────────────┐
│  Layer A: WC-over-NFS         <── core architectural bet        │
│  Layer B: Backend persistence <── scaling / durability          │
│  Layer C: Remote storage      <── the "yak" in jj-yak           │
└─────────────────────────────────────────────────────────────────┘
```

Recommendation: **do A first**. If WC-over-NFS doesn't work, B and C are
wasted effort. If it does, B and C are routine engineering.

## 2. Milestones

Smallest first; each is a meaningful demo.

### M1 — Daemon owns per-mount WC state

Replace the today-global `JujutsuService { sessions: Vec<Session>, store: Store }`
with a per-mount state map keyed by `working_copy_path`:

```rust
struct Mount {
    working_copy_path: PathBuf,
    workspace_id: WorkspaceId,             // matches proto: bytes, not string
    op_id: OperationId,
    root_tree_id: TreeId,
    sparse_patterns: Vec<RepoPathBuf>,
    nfs_port: u16,                         // assigned by VfsManager (M4)
    fs: VirtualFileSystem,                 // NFS server state (M3)
}
```

Plumb this through:
- `Initialize` creates a `Mount` and inserts it into the map.
- `set_checkout_state` / `get_checkout_state` / `get_tree_state` / `snapshot`
  stop being `todo!()` and read/write `Mount` fields.
- The single global `Store` stays global for now; per-mount stores arrive with
  remote backends in Layer C.

Drops the four `todo!()`s in `daemon/src/service.rs` (lines 148, 158, 167, 176).
No NFS work yet — just state. Also worth filling in the unrelated `todo!()` at
`daemon/src/ty.rs:277` (non-File `TreeEntry` variants) before it panics in
production.

**Scope:** ~150 lines new in `service.rs`, ~40 in `main.rs` to thread state,
0 changes to CLI (it already calls these RPCs, they'll start working).
One new test in `cli/tests/` doing `jj yak init` → `jj log` → `jj op log` to
exercise op_id round-trip.

### M2 — Wire YakWorkingCopyFactory at init

`cli/src/main.rs:138` is `&*default_working_copy_factory()`. The `// NOTE`
comment block at 135–138 explains why. With M1 done, flip it to
`&YakWorkingCopyFactory {}`. Integration tests will start hitting
`YakWorkingCopy::init` → `set_checkout_state` RPC. `test_init` (read-only)
should still pass; `test_multiple_init` and `test_repos_are_independent` do
call `jj new` and `yak status`, so they exercise more state — they're a
better post-M1 smoke test.

If anything breaks, that tells you something M1 missed.

### M3 — NFS read path

Implement these methods in `daemon/src/vfs.rs` (currently all return
`NFS3ERR_NOTSUPP`):

- `lookup(dirid, name)` — walk into a tree by component
- `getattr(id)` — file/dir mode + size
- `read(id, offset, count)` — pull file from `Store`
- `readdir(dirid, ...)` — list tree entries

Each resolves a `fileid3` (NFS's u64 inode-equivalent) to a tree path or
file id. Need a stable `fileid3 ↔ (TreeId, RepoPath)` mapping inside
`VirtualFileSystem` — standard approach is a slab of `Inode` entries that
lazily expands as paths are walked.

After M3 you can mount the export from a separate terminal and `ls`/`cat` an
empty repo. Won't show anything yet — no commit checked out.

### M4 — `jj yak init` actually mounts

Today `Initialize` just stores a session. It needs to:

1. Spawn an NFS server on a port (call into `VfsManager` — the `Bind` message
   exists at `vfs_mgr.rs:18-20`, the `VfsManagerHandle::bind()` wrapper
   exists at `vfs_mgr.rs:26-29`, but `handle()` is never called from
   `main.rs`, so nothing currently sends `Bind`).
2. Mount the NFS share at `working_copy_path` (shell out to `mount.nfs` —
   nfsserve has no built-in helper).
3. Return the port to the CLI so the WC factory can talk to the right server.

This is the **"is this idea even tractable"** milestone. See §4 for risks.

### M5 — `check_out` writes files

`LockedYakWorkingCopy::check_out` is `todo!` at `cli/src/working_copy.rs:262`.
Flow:

1. Get the new tree from `commit.tree()`.
2. Send the tree id (and a list of changed paths) to the daemon via a new
   `CheckOut` RPC.
3. Daemon updates `Mount.root_tree_id` and notifies the `VirtualFileSystem`
   that the tree changed.
4. NFS clients see new files on next `readdir`/`lookup`.

After M5, `jj new` populates the WC and `test_init` becomes a real end-to-end
NFS round trip.

### M6 — NFS write path + snapshot

Implement `write` / `create` / `remove` / `mkdir` / `setattr` in `vfs.rs`.
Each mutates an in-memory tree under the `Mount`. `snapshot` RPC computes
the current `root_tree_id` (or returns it cached if no writes since last
snapshot).

After M6, `jj describe` / `jj st` work over NFS. **First point at which
jj-yak is a usable VCS.**

## 3. What's deferred

- **Layer B (persistence):** the daemon's `HashMap` `Store` loses state on
  restart. Add `sled` or `redb` after M6.
- **Layer C (remote):** `Initialize.remote` is currently a string that's
  stored and ignored. Make it real after M6.
- **NFS-over-UNIX-socket / FUSE alternative:** see §4 — this may need to move
  earlier, not later.
- **Sparse patterns:** `set_sparse_patterns` can stay `todo!` until there's
  a real reason. Most yak users probably don't want sparse if the FS is
  already lazy.
- **Cleanup of `server/` crate:** `server/src/main.rs` is 3 lines (just
  `Hello, world!`). Delete it next time you're in `Cargo.toml`.

## 4. Areas of concern

The original plan mentions some of these (privileges, Watchman) in passing.
Investigation suggests they are **larger risks than the plan suggests** and
deserve explicit decisions before M3.

### 4.1 P0 — Likely blockers

**Mounting NFS on Linux requires root.** The plan asserts "nfsserve claims
it works unprivileged via TCP on a high port. Verify." Verified: nfsserve's
*server* side runs unprivileged, but `mount(2)` on Linux is gated on
`CAP_SYS_ADMIN`. The kernel NFS *client* needs root to attach the mount.
There is no clean rootless path on stock Linux:

- `/etc/fstab` with `user` option → admin one-time setup, doesn't fit
  `jj yak init`.
- Setuid `mount.nfs` → only honors fstab.
- User namespaces → NFS isn't in the user-ns mountable filesystem list.

**Implication:** `jj yak init` on Linux needs `sudo`, OR a setuid helper
shipped with jj-yak, OR a fundamentally different transport (FUSE, see below).
This needs an explicit decision before M4.

**inotify/Watchman do not see server-side mutations over NFS.** The plan
says "Watchman won't see writes via NFS — disable in the WC config." That's
correct, but the cost is bigger than the plan implies: jj's `snapshot`
without fsmonitor walks the entire WC. For large repos this dominates
command latency. Mitigation options:

- (a) Stamp `mtime`/`ctime` and rely on jj's stat-based scan (still O(tree)).
- (b) Bypass fsmonitor and feed jj a precomputed dirty set out-of-band
  (requires upstream jj changes or a wrapper).
- (c) Run watchman *inside* the daemon against the backing store and
  pretend its events came from the mount.

Worth deciding before M6 — this affects the snapshot RPC's contract.

### 4.2 P1 — Significant friction

**Cache coherency.** Linux NFS attribute cache (`acdirmin/acdirmax`,
default 30–60s) means `stat()` on the client may return stale data for up
to a minute *after* the daemon updates. nfsserve has no client-side
invalidation channel. Mitigation:

- Mount with `actimeo=0` (or `noac`) to force every access to revalidate.
  Localhost perf hit is small.
- Bump `mtime`/`ctime`/change-attr on every daemon-side mutation.
- The xetdata blog/README assume read-mostly workloads; we don't.

The plan does not currently mention attribute caching. It should.

**macOS quirks.** Apple's `mount_nfs` works against a custom port via
`-o port=N,mountport=N` and requires `nolocks`. nfsserve serves MOUNT3
statelessly which is enough for both Linux and macOS clients. macOS Big Sur+
has periodically broken loopback NFS and sometimes wants `resvport`
(reserved source port < 1024 → root client-side). FUSE-T uses this
exact approach in production, so it's viable, but expect intermittent
macOS-version-specific bugs.

**nfsserve maturity gaps.**

- NFSv3 only — no v4 → no delegations, no callbacks, no compounds.
- No locking (NLM not implemented; mount with `nolock`). Editors and jj's
  own index lockfile fall back to local emulation. Single-client this is
  fine, but the kernel may still reject some operations.
- No symlink/hardlink creation (TODO in upstream README).
- No auth/permission enforcement: any localhost process that finds the port
  can read/write the tree (issue #38, open).
- Last release 0.10.2 (Apr 2024). Small-team project.

### 4.3 The FUSE question

For a Linux-first project, **FUSE is probably the better fit**:

- Rootless mount via `fusermount3` (setuid helper bundled with `fuse3`).
- Real client-side invalidation (`notify_inval_inode`, `notify_store`).
- No port games, no `actimeo=0` workaround.
- macOS story is uglier (macFUSE kext is increasingly hostile to install;
  FUSE-T is itself NFS-loopback under the hood).

Recommended architectural option to discuss: **build the VFS abstraction so
the transport is swappable**. Ship FUSE on Linux first (rootless, real
invalidation), nfsserve on macOS as a second adapter. The current
`VirtualFileSystem` already implements `nfsserve::NFSFileSystem` directly,
so this would mean splitting it — the tree/inode model is transport-agnostic;
only the trait impl would differ.

Decision needed before M3.

## 5. Corrections folded in from code review

These adjustments to the original sketch are already applied above; listed
here so reviewers can spot-check.

- **Mount field naming.** Original sketch used `workspace_name: WorkspaceNameBuf`.
  Proto has `workspace_id: bytes` (`proto/jj_interface.proto:72-75`). M1's
  struct uses `workspace_id` to avoid a gratuitous rename.
- **Fifth `todo!()`.** `daemon/src/ty.rs:277` panics for non-File `TreeEntry`
  variants. Cheap to fill while in the area for M1; will hit it as soon as
  symlinks or subtrees flow through.
- **M2 smoke test.** Original plan said `test_init.rs` is read-only — only
  the first of three tests is. `test_multiple_init` and
  `test_repos_are_independent` already exercise `jj new` and `yak status`,
  so they're a better post-M1 signal.
- **Attribute caching (§4.2).** Original plan mentioned mount privileges and
  Watchman but not NFS attribute caching. Added — `actimeo=0` + ctime
  stamping is mandatory for a mutable WC.
- **FUSE alternative (§4.3).** Original plan deferred FUSE to "decide later
  when M3 reveals what's clunky." Promoted to a pre-M3 decision because the
  Linux mount-privilege answer (§4.1) probably forces it anyway.
- **Other ambient findings worth noting.**
  - `LockedYakWorkingCopy` has 6 `todo!`s, not just `check_out`: `recover`
    (251), `rename_workspace` (268), `reset` (272), `sparse_patterns` (276),
    `set_sparse_patterns` (280).
  - jj-lib pinned to 0.40 (`Cargo.toml:22`). Predecessors-deletion TODO is
    upstream lore; verify against jj-lib's CHANGELOG before pre-emptively
    versioning the proto.
  - No `Taskfile.yml`. Adding one is on the hygiene list (§6).

## 6. Cross-cutting hygiene

Worth doing in passing, not blocking:

- **Taskfile.yml** with `task build`, `task test`, `task lint`, `task daemon`
  (runs daemon with `daemon.toml`), `task tdd` (cargo watch).
- **Tests for the WC path** — once M2 lands, copy `cli/tests/test_init.rs`
  into `test_workingcopy.rs` exercising `jj st`, `jj new`, `jj describe -m foo`,
  `jj st`.
- **Drop predecessors from `commit_to_proto`** if/when jj-lib upstream
  removes the field — track in a TODO; will need a proto v2.
- **Tracing for the CLI.** Daemon has it; CLI doesn't. `RUST_LOG=cli=info,jj_lib=info`
  would help during M3/M4. Note the comment about CliRunner initializing
  late — any pre-CliRunner setup needs to use `eprintln!`.
- **`unwrap()` everywhere** — acceptable now (failures are programmer
  errors during dev) but should map to `BackendError::Other` /
  `WorkingCopyStateError::Other` before any user touches it. Track so we
  don't forget.
- **Delete `server/` crate** — 30 seconds, do it next time in `Cargo.toml`.

## 7. Decisions that gate later milestones

Listed in the order they have to be made. Each blocks specific milestones;
M1–M2 don't depend on any of them, so work can start now.



1. **Transport: NFS-only, FUSE-only, or a swappable abstraction?** Blocks
   M3. Currently under research — see `docs/transport-research.md` (TBD).
   The original plan implicitly commits to NFS; Linux mount privilege
   (decision 2) likely forces FUSE on Linux regardless.
2. **Mount privilege on Linux.** Blocks M4. If staying with NFS: setuid
   helper, `sudo` prompt, or admin-managed fstab. If FUSE: `fusermount3`'s
   setuid helper handles it. Falls out of decision 1.
3. **fsmonitor strategy.** Blocks M6 (snapshot RPC contract). Options:
   (a) disable fsmonitor and accept O(tree) snapshots; (b) run watchman
   inside the daemon against the backing store; (c) add a side-channel
   "dirty set" RPC and feed jj a precomputed working-copy delta. (c) is
   the most aligned with how jj-yak already mediates the WC, but requires
   upstream jj cooperation or a wrapper.
4. **Inode handle stability across daemon restarts.** Blocks Layer B
   design. NFSv3 file handles must survive restart or all clients see
   ESTALE; FUSE has the same constraint via `generation`. Persist the
   inode slab (sled/redb) or regenerate deterministically from a
   content-addressed tree?
5. **Concurrency model.** Multiple `Mount`s, single `Store`. If two mounts
   point at the same remote (Layer C), how do snapshot/checkout serialize?
   Deferrable past M6.

## 8. Recommended starting point

**M1 is the right first step.** Self-contained, mechanical, unblocks
everything else. Concrete scope:

- `daemon/src/service.rs`: ~150 lines (state map + 4 RPC bodies).
- `daemon/src/main.rs`: ~40 lines to thread state through.
- `cli/`: 0 changes (calls these RPCs already; they'll start working).
- One new test in `cli/tests/` exercising `jj yak init` → `jj log` →
  `jj op log` to round-trip op_id.

In parallel with M1, **make a decision on §7.1 (transport) and §7.2 (mount
privilege)** before starting M3/M4 — those choices can invalidate large
chunks of M3+.
