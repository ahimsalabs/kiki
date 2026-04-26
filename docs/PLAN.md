# jj-yak: Implementation Plan

Status: active. Transport architecture decided (§4.3 Path C). M1–M7
done — see [`PLAN-M1-6.md`](./PLAN-M1-6.md) for M1–M6 detail and §10
below for the M7 outcome. **Integration tests now run with
`disable_mount = false`**: `jj yak init` + `jj new` round-trip
through a real Linux FUSE mount, `.jj/` is excluded from snapshots,
and the working-copy `@` advances correctly across mutations.
The next milestone is **Layer B — durable storage** (§8). Until that
lands the daemon's per-mount `HashMap` `Store` loses everything on
restart.
Last updated: 2026-04-26

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

### M1–M6 — Layer A foundation (✅ done)

Full implementation log archived in [`PLAN-M1-6.md`](./PLAN-M1-6.md).
One-line summaries:

- **M1.** Daemon owns per-mount WC state — replaced global
  `JujutsuService { sessions, store }` with a path-keyed `Mount` map
  exercising the WC RPCs (`Initialize`, `Set/GetCheckoutState`,
  `GetTreeState`, `Snapshot`).
- **M2.** Wired `YakWorkingCopyFactory` into `Workspace::init_with_factories`
  so `jj yak init` flows: factory → `YakWorkingCopy::init` →
  `SetCheckoutState` RPC. Op-id round-trips end-to-end through `jj op log`.
- **M3.** Split `daemon/src/vfs.rs` into a `JjYakFs` trait + concrete
  `YakFs` + `NfsAdapter` (macOS) and `FuseAdapter` (Linux). Read-only
  surface: `lookup` / `getattr` / `read` / `readdir` / `readlink`.
- **M4.** `jj yak init` actually mounts. Mountpoint validation,
  per-mount `Store` (every store RPC carries `working_copy_path`),
  `VfsManager` bind protocol, platform-specific attach
  (`fuse3::Session::mount_with_unprivileged` on Linux,
  `nfsserve::tcp::NFSTcpListener` + `mount_nfs` shellout on macOS),
  and the `InitializeReply.transport` oneof. Added the `disable_mount`
  test-mode flag.
- **M5.** `CheckOut` RPC + `JjYakFs::check_out` re-roots the inode
  slab via `swap_root`. CLI's `LockedYakWorkingCopy::check_out` calls
  it. Conflicted trees rejected (single-id only).
- **M6.** VFS write path + snapshot. Trait grew `create_file` /
  `mkdir` / `symlink` / `write` / `setattr` / `remove` / `snapshot`.
  Lazy clean→dirty promotion on the inode slab; recursive sync
  snapshot persists into the per-mount `Store` and preserves inode ids
  across snapshot. Adapters dispatch to the trait. `Snapshot` RPC
  delegates to `JjYakFs::snapshot`.

After M6 the daemon-side VCS surface is feature-complete.

- **M7.** Two changes that together let `disable_mount = false` flip:
  - **M7.1** — `.jj/` lives outside the content-addressed user tree.
    `YakFs::jj_subtree: Mutex<Option<InodeId>>` pins the metadata
    directory across `check_out` and excludes it from `snapshot`.
    `mkdir(root, ".jj")` populates the pin; `lookup`/`readdir`/
    `remove`/`rename` short-circuit at the root. Pinned subtree's
    dirty buffers are also cleaned on snapshot (memory bound).
  - **M7.2** — two coupled bugs that broke `jj new` end-to-end on a
    real mount: (a) `swap_root` cleared the entire `by_parent`
    cache, severing the chain through pinned `.jj/` so writes that
    happened between snapshot and check_out got orphaned; fixed by
    clearing only `(ROOT_INODE, *)` entries. (b) `LockedYakWorkingCopy::
    finish` didn't propagate the new `operation_id` back to the
    daemon, so subsequent `WorkingCopy::operation_id()` reads kept
    returning the pre-mutation op and `jj log`'s `@` marker stayed
    pinned to the old WC commit. Fixed by sending `SetCheckoutState`
    in `finish`.
  - Capstone: `cli/tests/common/mod.rs` flipped to `disable_mount =
    false`, M7-gated `test_nested_tree_round_trips` and
    `test_symlink_tree_round_trips` un-ignored.

## 3. What's deferred

- **Layer B (persistence):** the daemon's `HashMap` `Store` loses state on
  restart. Add `sled` or `redb` next.
- **Layer C (remote):** `Initialize.remote` is currently a string that's
  stored and ignored. Make it real after Layer B.
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

**fsmonitor strategy still TBD for snapshot.** Resolved as a non-blocker
by M6's actual shape (see §7 #7): the daemon's VFS owns every write, so
`JjYakFs::snapshot` walks the slab and produces the rolled-up tree id
directly without jj-lib ever scanning.

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

(Linux FUSE path doesn't have this problem — we run with `TTL =
Duration::ZERO`, so the kernel revalidates over the FUSE channel on
every `getattr`/`lookup`. See §7 #9.)

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
  No port games, no `actimeo=0`. (In practice we run with `TTL=0` instead
  of pushing invalidations — see §7 #9 — and may migrate to `fuser` once
  perf matters; see §11.)
- **macOS path: keep nfsserve.** macOS mounts `nfs://localhost:N` cleanly
  with `mount_nfs -o port=N,mountport=N,nolocks,vers=3` and no kext.
  macFUSE is increasingly hostile to install on Apple Silicon and ships
  with kext-signing requirements; FUSE-T avoids the kext but trades it
  for a license problem.
- **`fuser` vs `fuse3`:** chose `fuse3` for M3. `fuser` uses sync reply
  callbacks and explicit file handles; `fuse3` is async and value-returning,
  which lines up cleanly with `nfsserve::NFSFileSystem`. Less impedance
  mismatch in the shared trait. (Migration to `fuser` is on the table —
  see §11.)

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

## 5. Corrections folded in from code review (still-live)

These adjustments are still relevant for the next milestones. (M1–M6
specific corrections are archived in `PLAN-M1-6.md`.)

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
- **Tests for the WC path** — copy `cli/tests/test_init.rs` into
  `test_workingcopy.rs` exercising `jj st`, `jj new`, `jj describe -m foo`,
  `jj st`. Now unblocked (M7 flipped `disable_mount = false`); the
  existing `test_nested_tree_round_trips` / `test_symlink_tree_round_trips`
  give a starting template.
- **jj-lib 0.40 metadata round-trip.** `cli/src/backend.rs` now preserves
  `conflict_labels` and tree-entry `copy_id` (commit `ba36e622`). Still
  TODO: drop `predecessors` from `commit_to_proto` if/when jj-lib
  upstream removes the field — track and bump to proto v2 when it
  happens.
- **Tracing for the CLI.** Daemon has it; CLI doesn't. `RUST_LOG=cli=info,jj_lib=info`
  would help during M4 mount debugging. Note the comment about CliRunner
  initializing late — any pre-CliRunner setup needs to use `eprintln!`.
- **`unwrap()` everywhere** — acceptable now (failures are programmer
  errors during dev) but should map to `BackendError::Other` /
  `WorkingCopyStateError::Other` before any user touches it. Track so we
  don't forget.
- **Delete `server/` crate** — 30 seconds, do it next time in `Cargo.toml`.

## 7. Decisions

### Decided

1. **Transport architecture (§4.3).** Thin internal `JjYakFs` trait,
   `fuse3` adapter on Linux, `nfsserve` adapter on macOS. Done in M3:
   trait + concrete `YakFs` + both adapters live in
   `daemon/src/vfs/`.
2. **Linux mount privilege.** Falls out of (1): `fusermount3` setuid
   helper handles rootless mount; no `sudo` flow needed.
3. **Mountpoint policy (M4).** `Initialize` requires `working_copy_path`
   to exist, be a directory, be empty, and not already be a mountpoint.
   Non-empty or already-mounted → `FailedPrecondition`. No auto-umount
   of stale mounts: a stale mount almost always means the previous
   daemon crashed, and silent recovery would mask the bug Layer B is
   meant to fix. CLI's `create_or_reuse_dir` becomes create-only.
4. **Per-mount `Store` (M4).** Each `Mount` owns an `Arc<Store>`; the
   global `JujutsuService.store` goes away. Every store RPC gains a
   `working_copy_path` field — wire-schema change, but the CLI already
   knows the workspace so the stamp is free. Done now rather than at
   Layer C so two mounts at different remotes cannot see each other's
   blobs through the content-addressed keyspace.
5. **`InitializeReply` transport shape (M4).** Polymorphic `oneof
   transport { FuseTransport fuse = 1; NfsTransport nfs = 2; }`. CLI
   dispatches on the variant: `Fuse` → mount already done by daemon;
   `Nfs { port }` → CLI shells out to `mount_nfs`. Daemon never runs
   `mount_nfs` itself (would require root). Leaves room for future
   transports without sentinels.
6. **Inode handle stability across daemon restarts.** Derive
   deterministically from `(parent_tree_id, name)` via a stable hash
   truncated to `u64`. The slab becomes a cache, not a source of
   truth — restart-safe, no persistence dependency. Collisions in a
   `u64` namespace are handled by chaining on collide (revisit if any
   real workload hits one). Writes-in-flight need a temporary id space
   the kernel won't ESTALE on; reserve the high bit (or a high range)
   for transient ids that get rewritten to the derived id once the
   write commits to a tree. **Implementation lands alongside Layer B**
   (the current monotonic slab is fine until restarts matter); the
   M3 slab API is the right shape, just swap the id source.

### Still open

7. **fsmonitor strategy.** Resolved as a non-blocker by M6's actual
   shape. The daemon's VFS owns every write, so snapshot doesn't need
   fsmonitor at all — `JjYakFs::snapshot` walks the slab and produces
   the rolled-up tree id directly. (Option (b) from §4.1 in spirit:
   daemon already knows what's dirty; jj-lib never has to scan.) Real
   integration with `WorkingCopy::snapshot` happens via the existing
   `LockedYakWorkingCopy::snapshot` → `snapshot_via_daemon` path; no
   jj-lib hook override required.
8. **Concurrency model.** Multiple `Mount`s, each now with its own
   `Store` (decision 4). If two mounts point at the same remote
   (Layer C), how do snapshot/checkout serialize against the shared
   remote? Deferrable past M7 — local mounts are independent until
   Layer C couples them.
9. **FUSE invalidation API. (Resolved as a non-blocker via TTL=0.)**
   `fuse3 0.9.0`'s `Session::get_notify` is still private, and worse
   than the original PLAN suggested: `mount_with_unprivileged` *consumes*
   the `Session`, so even a "make `get_notify` pub" upstream PR isn't
   enough — we'd need `MountHandle::notifier()` exposed too, and `Notify`
   itself has one-shot async methods (`pub async fn invalid_inode(mut
   self, …)`), so `MountHandle` would need to vend a constructor each
   call. A real upstream patch is structural, not minutes-of-code.
   We sidestepped this by setting `TTL: Duration::ZERO` in
   `daemon/src/vfs/fuse_adapter.rs`. The kernel revalidates every
   `getattr`/`lookup` over the FUSE channel; localhost round-trip is
   sub-100µs, editor workloads issue O(20) syscalls per file open, and
   `cat`/`ls` after a daemon-side checkout sees the correct attrs
   immediately. Options for the eventual proper fix:
   - **(a) Upstream PR to expose `Notify` via `MountHandle`.** Larger
     surface than originally thought (see above). Days-to-weeks of
     review.
   - **(b) Fork/vendor `fuse3`.** Cuts the dependency chain but
     `Notify`'s one-shot consume-self API is awkward forever.
   - **(c) Switch to `fuser`.** Sync trait surface, but `fuser`'s
     `Notifier` is `Clone`, public, and used in production by
     mountpoint-s3 (AWS). The right long-term move once perf matters
     — see §11.
   Until then, real users (and the once-M7 lands, integration tests)
   see correct semantics with a small per-syscall daemon-dispatch
   tax.

## 8. Recommended starting point

**M1–M7 are done.** Integration tests run with `disable_mount = false`:
`jj yak init`, `jj new`, `jj log`, `jj op log`, and the
`test_nested_tree_round_trips` / `test_symlink_tree_round_trips`
end-to-end paths succeed on a real Linux FUSE mount. M7 detail in §10.

**Next: Layer B — durable storage.** The per-mount `HashMap<Id, …>`
`Store` loses everything on daemon restart (M6's
snapshot-cleans-the-slab pattern means even an in-flight write is
durable across `Snapshot` calls but not across daemon restarts).
Pick `redb` or `sled`, swap the `HashMap` impls behind the same
sync API, and the Mount-id derivation in §7 decision 6 becomes
necessary so kernel handles survive restart.

**Hygiene to fold in around Layer B:**

- **Inode handle stability across restarts** (§7 decision 6).
  Implementation lands alongside Layer B; the slab API is already the
  right shape, just swap the id source from monotonic `next_id` to a
  derived `(parent_tree_id, name)` hash.
- **`unwrap()` audit.** Acceptable while the daemon is single-developer,
  but tighter error-mapping should land before any user touches the
  daemon (`BackendError::Other` / `WorkingCopyStateError::Other`
  passthrough at every boundary).
- **Delete `server/` crate.** Three lines of "Hello, world!"; just
  remove from `Cargo.toml`.
- **Tracing for the CLI.** Daemon already has it; CLI gets `RUST_LOG`
  setup that helps debug snapshot/checkout RPC traffic.

## 9. "Ship (d)" outcome (interim, 2026-04-26)

Goal was: flip `TTL: Duration::ZERO` in the FUSE adapter and
`disable_mount = false` in tests, find out the real next blocker.

**What landed (committed):**

- `daemon/src/vfs/fuse_adapter.rs`: `TTL = Duration::ZERO` (was
  `Duration::from_secs(60)`). Comment updated to explain the
  trade-off and point at §7 #9.
- **Missing FUSE methods filled in.** The fuse3 trait defaults to
  `ENOSYS` for many ops; we needed working implementations or stubs
  for: `flush`, `fsync`, `fsyncdir`, `release`, `releasedir`,
  `readdirplus`, `rename`. All but `rename` are no-op-style stubs
  (`Ok(())`) — we have no per-fh state and durability lives at
  Layer B, so flush/fsync/release have nothing to do.
  `readdirplus` is a real impl (the kernel falls back to `readdir`
  imperfectly when readdirplus returns ENOSYS), looking like
  `readdir` + `getattr` per entry.
- **`JjYakFs::rename`** (new trait method) + impl on `YakFs`. POSIX
  semantics: existing destination atomic-replace, empty-dir-rmdir
  for dir-over-dir, type-match guards. Required because jj-lib's
  index/op-heads writes use the standard
  atomic-write-via-`.tmpXXXX`-then-rename pattern. Plumbed through
  both adapters: FUSE (`OsStr`→`&str`) and NFS (`filename3`→`&str`).
- `daemon/src/main.rs`: `tracing-subscriber` now uses `EnvFilter`,
  so `RUST_LOG` actually controls log filtering. Indispensable for
  debugging the FUSE op flow against a real mount.
- `Cargo.toml`: enabled the `env-filter` feature on
  `tracing-subscriber`.

**What did *not* land:**

- `cli/tests/common/mod.rs:disable_mount = false`. Tried it; saw
  the real next gate. Reverted to `true`. The two M5+M6-tagged
  tests are still `#[ignore]`, now with a "needs M7" reason
  pointing at §10.

**Test status:** 67 daemon unit + 8 cli unit + 4 cli integration
passing, 2 ignored (still). No regressions.

## 10. M7 outcome — `.jj/` separation + op_id propagation (done)

Two issues had to be fixed before `disable_mount = false` could flip.
Both landed; integration tests now run end-to-end on a real FUSE mount.

### 10.1 `.jj/` separation (M7.1, landed)

**Problem:** `JjYakFs::snapshot` walked the slab from `ROOT_INODE`
and included every child — including `.jj/`, which jj-lib creates
inside the mount during `jj yak init`. So the tree returned to
jj-lib for the WC commit contained `.jj/repo/index/segments/…`,
`.jj/working_copy/…`, etc. as if they were user content. `jj log`
showed `(empty)` flipping off because the WC commit had ~14
`.jj/…` entries.

**Adopted: option (a) refined** — pin `.jj/` outside the slab's
root children. `YakFs` grew `jj_subtree: Mutex<Option<InodeId>>`;
`mkdir(root, ".jj")` populates it; `lookup`/`readdir`/`remove`/
`rename` short-circuit on `(ROOT_INODE, ".jj")`; `child_exists`
treats the pin as occupied; `snapshot_node`'s root iteration
defensively skips `.jj` (it isn't in `root.children` to begin with
under this design, but legacy slabs from pre-M7 snapshots could
have had one). `snapshot()` also walks the pinned subtree to clean
its dirty buffers (memory bound) but discards the resulting tree id.

**Survival across `check_out`:** the pinned inode is held outside
the slab's child maps, so `swap_root` doesn't disturb it. Pre-M7
trees that contain `.jj/` resolve via the user-tree fall-through
until something pins our own; jj-lib's `mkdir(".jj")` creates one
on first use, after which the pin shadows the legacy entry.

The cleaner option (b) — two-keyspace storage with `.jj/` outside
the per-mount Store entirely — is on the table for Layer C, when
the user tree's storage location starts to differ from the
daemon-managed metadata's.

### 10.2 Stale `@-` after `jj new` (M7.2, landed)

Two coupled bugs surfaced once §10.1 was in place and the M7-gated
tests started running.

**Bug A — `swap_root` cleared the entire `by_parent` cache.**
`InodeSlab::swap_root` (called by `JjYakFs::check_out`) used to
do `inner.by_parent.clear()`, which inadvertently severed the
chain through the pinned `.jj/`: a subsequent lookup of
`.jj/repo/op_heads/heads` re-walked the (now-stale) snapshotted
user tree and missed any writes that had happened between the
last snapshot and the `check_out`. Result: jj-lib's freshly-written
op file (`a297f8…`) was attached to inode 11 in the slab but the
next CLI invocation re-resolved the path through fresh inode ids
that pointed at the snapshotted `.jj/` tree without it.

**Fix:** `swap_root` now only drops `(ROOT_INODE, name)` entries
from `by_parent`. Sub-tree entries `(non-root-inode, name)`
survive — they're either reachable through the pinned `.jj/` (in
which case we *want* stable inode ids) or orphaned (harmless).
Test: `swap_root_preserves_subtree_cache` in `inode.rs`.

**Bug B — `LockedYakWorkingCopy::finish` didn't propagate the
new operation_id back to the daemon.** The local-disk working
copy writes `.jj/working_copy/checkout` at this point; the
daemon-backed equivalent is `SetCheckoutState`. Without it, the
daemon's stored `op_id` stayed at the pre-mutation value, and the
next CLI's `WorkingCopy::operation_id()` returned the old op,
which made jj-lib resolve `@` to the pre-`jj new` WC commit.
Visible as: `jj new` printed "Working copy now at: <new>" but
`jj log` next ran showed `@` still on the old commit.

**Fix:** `finish` now sends `SetCheckoutStateReq` with the new
`operation_id`, then invalidates the cached `checkout_state`
`OnceCell`. Code in `cli/src/working_copy.rs:LockedYakWorkingCopy::finish`.

**Diagnostic that found the bugs:** `RUST_LOG=daemon=info` plus
explicit `info!` in the snapshot/check_out/SetCheckoutState
handlers (kept in the daemon as low-volume tracing — three
mutating RPCs per CLI invocation), with FUSE-side traces added
temporarily during the investigation. The smoking gun was
comparing FUSE write traces (jj-lib *did* create the new op file)
against a `readdir` of the same path post-CheckOut (different
inode id, stale tree).

## 11. fuser migration (deferred until perf matters)

The user is leaning toward switching from `fuse3` to `fuser`
once we exit the "(d) interim" mode. fuser's advantages, confirmed
by reading sources:

- `Session::notifier()` and `BackgroundSession::notifier()` are
  public.
- `Notifier` is `Clone`, methods take `&self`, and have proper
  semantics (`inval_entry` swallows ENOENT for already-evicted
  entries).
- Used in production by mountpoint-s3 (AWS).
- Drops the structural problem with fuse3's
  `mount_with_unprivileged` consuming `Session`.

Cost (reconfirmed against actual sources):

- ~700–900 LoC in `daemon/src/vfs/fuse_adapter.rs` (sync trait
  surface, ~25 callbacks, spawn-and-reply pattern via captured
  `tokio::runtime::Handle` to avoid serializing kernel requests
  behind fuser's single-threaded loop).
- ~50–100 LoC in `daemon/src/vfs_mgr.rs` (`spawn_mount2` returns
  `BackgroundSession`; capture `notifier()` for the per-Mount
  state).
- ~30 LoC in `daemon/src/service.rs` (wire `notifier` into
  `JujutsuService::check_out` so we push `inval_inode(ROOT_INODE,
  0, 0)` + per-child `inval_entry` after the swap).

Pre-requisite reading: our `JjYakFs` async methods are "async in
name only" — every body is sync (parking_lot mutex + Store calls
are sync as of M6). So the sync→async bridge in the new fuser
adapter is light: the bodies don't await anything we'd lose by
switching to a sync trait.

Land alongside Layer B if possible (we'll already be touching
per-Mount state for stable inode ids).
