# jj-yak: Implementation Plan

Status: active. Transport architecture decided (§4.3 Path C).
Last updated: 2026-04-25

This document captures the roadmap for getting jj-yak from "scaffold with
stubs" to "usable read/write VCS", along with a review of assumptions
against the current code and external feasibility risks.

## 1. Big picture

The project's goal is a daemon that serves the working copy as a virtual
filesystem (FUSE on Linux, NFS on macOS — see §4.3), backed by storage
that eventually goes to a remote. Three orthogonal layers:

```
┌─────────────────────────────────────────────────────────────────┐
│  Layer A: WC-over-VFS         <── core architectural bet        │
│  Layer B: Backend persistence <── scaling / durability          │
│  Layer C: Remote storage      <── the "yak" in jj-yak           │
└─────────────────────────────────────────────────────────────────┘
```

Recommendation: **do A first**. If WC-over-VFS doesn't work, B and C are
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
    transport: TransportHandle,            // FUSE handle or NFS port; M4
    fs: Arc<JjYakFs>,                      // shared VFS state; M3
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

### M3 — VFS read path

Refactor `daemon/src/vfs.rs` along §4.3:

1. Extract a `JjYakFs` trait shaped like the current `NFSFileSystem` impl
   (which is mostly already the right shape, just renamed).
2. Move the inode/tree state into the trait's owning type. Add a slab of
   `Inode` entries that lazily expands as paths are walked, keyed by
   `fileid3`-equivalent u64. The slab is the canonical state; both
   adapters read from it.
3. Add two adapters:
   - `impl nfsserve::NFSFileSystem` — keep current scaffolding.
   - `impl fuse3::Filesystem` — new. Pulls in `fuse3` as a workspace dep.
4. Implement the read ops on the trait:
   - `lookup(dirid, name)` — walk into a tree by component
   - `getattr(id)` — file/dir mode + size
   - `read(id, offset, count)` — pull file from `Store`
   - `readdir(dirid, ...)` — list tree entries

Once M3 lands you can mount the export — Linux via FUSE, macOS via NFS —
and `ls`/`cat` an empty repo. Won't show anything yet (no commit checked
out), but the transport plumbing is real.

### M4 — `jj yak init` actually mounts

Today `Initialize` just stores a session. It needs to:

1. Bring up the per-mount filesystem instance via `VfsManager` (the
   `Bind` message exists at `vfs_mgr.rs:18-20`; `VfsManagerHandle::bind()`
   exists at `vfs_mgr.rs:26-29`; but `handle()` is never called from
   `main.rs`, so nothing currently sends `Bind`). Per-mount lifecycle
   has to expand to cover both transports.
2. Mount the share at `working_copy_path`:
   - **Linux:** `fuse3` does the mount itself via `fusermount3` —
     no shell out, no root.
   - **macOS:** shell out to `mount_nfs -o port=N,mountport=N,nolocks,vers=3`.
3. Return whatever handle the CLI needs (port for NFS, nothing for FUSE)
   so subsequent RPCs hit the right mount.

This is the **"is this idea even tractable"** milestone — once a mount
survives `init` and basic file ops work, the rest of M5/M6 is mostly
filling in trait methods.

### M5 — `check_out` writes files

`LockedYakWorkingCopy::check_out` is `todo!` at `cli/src/working_copy.rs:262`.
Flow:

1. Get the new tree from `commit.tree()`.
2. Send the tree id (and a list of changed paths) to the daemon via a new
   `CheckOut` RPC.
3. Daemon updates `Mount.root_tree_id` and notifies the VFS that the tree
   changed.
4. Clients see new files:
   - **FUSE:** push invalidations via `notify_inval_inode` /
     `notify_inval_entry` for the changed paths. Kernel re-reads on next
     access.
   - **NFS:** rely on attr-cache TTL (`actimeo=0` mount option) plus
     bumped `mtime`/`ctime` on changed entries. Kernel re-stats on
     access.

After M5, `jj new` populates the WC and `test_init` becomes a real
end-to-end VFS round trip.

### M6 — VFS write path + snapshot

Implement `write` / `create` / `remove` / `mkdir` / `setattr` on the
`JjYakFs` trait. Each mutates an in-memory tree under the `Mount`. The
ops land once on the trait and both adapters dispatch to them.
`snapshot` RPC computes the current `root_tree_id` (or returns it cached
if no writes since last snapshot).

After M6, `jj describe` / `jj st` work end-to-end. **First point at
which jj-yak is a usable VCS.**

## 3. What's deferred

- **Layer B (persistence):** the daemon's `HashMap` `Store` loses state on
  restart. Add `sled` or `redb` after M6.
- **Layer C (remote):** `Initialize.remote` is currently a string that's
  stored and ignored. Make it real after M6.
- **Sparse patterns:** `set_sparse_patterns` can stay `todo!` until there's
  a real reason. Most yak users probably don't want sparse if the FS is
  already lazy.
- **Cleanup of `server/` crate:** `server/src/main.rs` is 3 lines (just
  `Hello, world!`). Delete it next time you're in `Cargo.toml`.

## 4. Areas of concern

These are the risks the original sketch glossed over. The §4.3
architecture decision routes around the worst ones; the rest are still
live and listed here so they don't get forgotten.

### 4.1 Risks the architecture closes

For the record, since the original sketch worried about these:

**Mounting NFS on Linux would have required root.** `mount(2)` on Linux
is gated on `CAP_SYS_ADMIN`; nfsserve's server runs unprivileged but the
kernel NFS *client* doesn't. **Closed by §4.3:** Linux uses FUSE
(`fusermount3` is setuid; `jj yak init` runs as the user with no `sudo`).

**inotify/Watchman wouldn't see server-side mutations over NFS.** True,
and `snapshot` without fsmonitor walks the entire WC. **Closed by §4.3
on Linux:** FUSE adapter pushes invalidations via `notify_inval_inode`
when `check_out` mutates the tree, so the kernel re-stats on next access
without scanning. macOS still has the original problem; see §4.2.

**fsmonitor strategy still TBD for snapshot.** Even with FUSE
invalidation, snapshot needs to know which paths the *client* (editors,
build tools) wrote since the last revision. Options:

- (a) Stamp `mtime`/`ctime` and rely on jj's stat-based scan (still O(tree)).
- (b) Bypass fsmonitor and feed jj a precomputed dirty set out-of-band
  (the daemon already knows what was written via the FUSE/NFS write path).
  Requires upstream jj cooperation or a wrapper.
- (c) Run watchman *inside* the daemon against the backing store.

(b) is most aligned with how jj-yak already mediates the WC. Decide
before M6 — affects the snapshot RPC's contract.

### 4.2 Live risks (mostly macOS NFS)

**Cache coherency on the macOS NFS path.** Linux NFS attribute cache
(`acdirmin/acdirmax`, default 30–60s) means `stat()` on the client may
return stale data for up to a minute *after* the daemon updates.
`nfsserve` has no client-side invalidation channel, so on macOS we live
with the polling model. Mitigation:

- Mount with `actimeo=0` (or `noac`) — every access revalidates.
  Localhost perf hit is small.
- Bump `mtime`/`ctime`/change-attr on every daemon-side mutation.
- The xetdata blog/README assume read-mostly workloads; we don't.

(Linux FUSE path doesn't have this problem — kernel respects the
explicit invalidation we push.)

**macOS quirks.** Apple's `mount_nfs` works against a custom port via
`-o port=N,mountport=N` and requires `nolocks`. nfsserve serves MOUNT3
statelessly, which is enough. macOS Big Sur+ has periodically broken
loopback NFS and sometimes wants `resvport` (reserved source port < 1024
→ root client-side). Expect intermittent macOS-version-specific bugs.

**nfsserve maturity gaps (macOS only after §4.3).**

- NFSv3 only — no v4 → no delegations, no callbacks, no compounds.
- No locking (NLM not implemented; mount with `nolock`). Editors and jj's
  own index lockfile fall back to local emulation. Single-client this is
  fine, but the kernel may still reject some operations.
- No symlink/hardlink creation (TODO in upstream README).
- No auth/permission enforcement: any localhost process that finds the port
  can read/write the tree (issue #38, open).
- Last release 0.10.2 (Apr 2024). Small-team project.

### 4.3 Transport: adopted architecture

Decided. Punch line: **no off-the-shelf NFS↔FUSE bridge is worth taking
on.** Survey of options that ruled them out:

- **FUSE-T** (macOS) goes the wrong direction (FUSE → NFS) and ships under
  a non-commercial-only license. Its NFS server is not a reusable library.
- **fuse-nfs** consumes NFS, doesn't expose it. Wrong direction.
- **NFS-Ganesha** has the right plugin architecture (FSAL) but is hundreds
  of kloc of C with no Rust bindings; wildly disproportionate for a
  per-user, single-mount daemon.
- **9P** doesn't help — Linux already has FUSE; macOS has no 9P client.

The actually-useful finding: **`fuse3::Filesystem` (the Rust crate) and
`nfsserve::NFSFileSystem` have nearly identical trait shapes** — both
async, both `Result`-returning, both inode-keyed, ~15 ops each. A thin
shared trait shaped like the existing `NFSFileSystem` impl in
`daemon/src/vfs.rs` covers both. Concretely:

```
trait JjYakFs (≈15 async methods, our own type names)
   ├── NfsAdapter:  impl nfsserve::NFSFileSystem for &dyn JjYakFs
   └── FuseAdapter: impl fuse3::Filesystem      for &dyn JjYakFs
```

**Adopted:**

- **Linux primary path: `fuse3` crate.** Rootless mount via the bundled
  `fusermount3` setuid helper. Real client-side invalidation via FUSE
  notify ops (`notify_inval_inode`, `notify_inval_entry`) — closes the
  fsmonitor problem in §4.1 by giving us a real way to push tree changes.
  No port games, no `actimeo=0`.
- **macOS path: keep nfsserve.** macOS mounts `nfs://localhost:N` cleanly
  with `mount_nfs -o port=N,mountport=N,nolocks,vers=3` and no kext.
  macFUSE is increasingly hostile to install on Apple Silicon and ships
  with kext-signing requirements; FUSE-T avoids the kext but trades it
  for a license problem.
- **`fuser` vs `fuse3`:** prefer `fuse3`. `fuser` uses sync reply
  callbacks and explicit file handles; `fuse3` is async and value-returning,
  which lines up cleanly with `nfsserve::NFSFileSystem`. Less impedance
  mismatch in the shared trait.

**Why this is still "minimal abstractions":** the existing
`daemon/src/vfs.rs` is *already* shaped like the proposed shared trait —
it just happens to be named `NFSFileSystem`. The refactor is mostly
renaming a trait and adding a second `impl` block. Estimated glue:
~200-line trait file, ~300–500-line nfsserve adapter (attr conversions),
~600–1000-line fuse3 adapter (more because of `open`/`release` lifecycle).
The tree/inode/store model — the actually-interesting code — is written
once.

**License surface:** `fuse3` is MIT, `nfsserve` is BSD/Apache, system
`libfuse3` is LGPL. No restrictions on distribution.

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
- **FUSE on Linux (§4.3).** Original plan deferred FUSE to "decide later
  when M3 reveals what's clunky." Promoted, decided, and adopted as the
  Linux primary path. macOS keeps NFS.
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
- **jj-lib 0.40 metadata round-trip.** `cli/src/backend.rs` now preserves
  `conflict_labels` and tree-entry `copy_id` (commit `ba36e622`). Still
  TODO: drop `predecessors` from `commit_to_proto` if/when jj-lib
  upstream removes the field — track and bump to proto v2 when it
  happens.
- **Tracing for the CLI.** Daemon has it; CLI doesn't. `RUST_LOG=cli=info,jj_lib=info`
  would help during M3/M4. Note the comment about CliRunner initializing
  late — any pre-CliRunner setup needs to use `eprintln!`.
- **`unwrap()` everywhere** — acceptable now (failures are programmer
  errors during dev) but should map to `BackendError::Other` /
  `WorkingCopyStateError::Other` before any user touches it. Track so we
  don't forget.
- **Delete `server/` crate** — 30 seconds, do it next time in `Cargo.toml`.

## 7. Decisions

### Decided

1. **Transport architecture (§4.3).** Thin internal `JjYakFs` trait,
   `fuse3` adapter on Linux, `nfsserve` adapter on macOS. Existing
   `daemon/src/vfs.rs` already approximates the trait. M3 does the
   extraction.
2. **Linux mount privilege.** Falls out of (1): `fusermount3` setuid
   helper handles rootless mount; no `sudo` flow needed.

### Still open

3. **fsmonitor strategy.** Blocks M6 (snapshot RPC contract). See §4.1.
   Leaning toward (b) — daemon feeds jj a precomputed dirty set
   out-of-band, since the FUSE/NFS write path already knows what was
   written. Decide while building M6.
4. **Inode handle stability across daemon restarts.** Blocks Layer B
   design. NFSv3 file handles must survive restart or all clients see
   ESTALE; FUSE has the same constraint via `generation`. Persist the
   inode slab (sled/redb) or regenerate deterministically from a
   content-addressed tree? Decide alongside Layer B.
5. **Concurrency model.** Multiple `Mount`s, single `Store`. If two
   mounts point at the same remote (Layer C), how do snapshot/checkout
   serialize? Deferrable past M6.

## 8. Recommended starting point

**M1 is the right first step.** Self-contained, mechanical, unblocks
everything else. Concrete scope:

- `daemon/src/service.rs`: ~150 lines (state map + 4 RPC bodies).
- `daemon/src/main.rs`: ~40 lines to thread state through.
- `cli/`: 0 changes (calls these RPCs already; they'll start working).
- One new test in `cli/tests/` exercising `jj yak init` → `jj log` →
  `jj op log` to round-trip op_id.

M3 (trait extraction + FUSE adapter) can start as soon as M1–M2 are
green. Add `fuse3` to `Cargo.toml` as part of M3; the existing
`nfsserve` dep stays.
