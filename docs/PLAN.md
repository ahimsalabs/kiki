# jj-yak: Implementation Plan

Status: active. Transport architecture decided (§4.3 Path C). M1–M9
done — see [`PLAN-M1-6.md`](./PLAN-M1-6.md) for M1–M6 detail and
[`PLAN-M7-9.md`](./PLAN-M7-9.md) for M7–M9 detail (including the
"Ship (d)" interim that landed between M6 and M7). M10 spec at §10;
in flight. One-line state: integration tests run with `disable_mount
= false` on a real Linux FUSE mount, the per-mount `Store` is
redb-backed and rehydrates across daemon restart, and the per-mount
`RemoteStore` (parsed from `Initialize.remote`) does write-through +
read-through + post-snapshot push against `dir://` and `grpc://`
backends. 115/115 daemon tests + 14/14 cli tests pass; `cargo clippy
--workspace --all-targets -- -D warnings` is clean. Inode handle
stability (§7 decision 6) is still deferred; the in-memory slab is
fine until kernel handles need to survive a daemon restart in
production. Last updated: 2026-04-30

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

### M7–M9 — `.jj/` separation, durable storage, remote CAS (✅ done)

Full implementation log archived in [`PLAN-M7-9.md`](./PLAN-M7-9.md)
(plus the "Ship (d)" interim that flipped `TTL=0` and filled in
missing FUSE methods between M6 and M7). One-line summaries:

- **M7.** Two changes that together let `disable_mount = false` flip:
  - **M7.1** — `.jj/` lives outside the content-addressed user tree.
    `YakFs::jj_subtree: Mutex<Option<InodeId>>` pins the metadata
    directory across `check_out` and excludes it from `snapshot`.
    `mkdir(root, ".jj")` populates the pin; `lookup`/`readdir`/
    `remove`/`rename` short-circuit at the root.
  - **M7.2** — two coupled bugs that broke `jj new` end-to-end on a
    real mount: (a) `swap_root` cleared the entire `by_parent`
    cache, severing the chain through pinned `.jj/`; fixed by
    clearing only `(ROOT_INODE, *)` entries. (b) `LockedYakWorkingCopy::
    finish` didn't propagate the new `operation_id` back to the
    daemon; fixed by sending `SetCheckoutState` in `finish`.
  - Capstone: `cli/tests/common/mod.rs` flipped to `disable_mount =
    false`, M7-gated `test_nested_tree_round_trips` and
    `test_symlink_tree_round_trips` un-ignored. Detail in
    [`PLAN-M7-9.md`](./PLAN-M7-9.md) §10.

- **M8.** Layer B done — per-mount durable storage. redb-backed
  `Store` (one file per mount under
  `<storage_dir>/mounts/<hash(wc_path)>/store.redb`); same sync API
  as the M1–M7 `HashMap` impl, but methods now return
  `anyhow::Result<…>` so I/O failures don't panic. Mount metadata
  (`working_copy_path`, `remote`, `op_id`, `workspace_id`,
  `root_tree_id`) persists in `mount.toml` next to the store.
  `JujutsuService::rehydrate` runs at daemon startup before the
  gRPC listener accepts connections, re-binding every persisted
  mount. `server/` crate deleted as a hygiene capstone. Detail in
  [`PLAN-M7-9.md`](./PLAN-M7-9.md) §12.

- **M9.** Layer C done — per-mount remote blob CAS.
  `Initialize.remote` parses to a `RemoteStore` impl (`dir://` or
  `grpc://`); composition lives in `service.rs` so `Store` stays
  sync. Write-through on every write RPC, read-through on local
  miss (with `verify_round_trip` to defend against a corrupt peer),
  and a post-snapshot reachability walk that pushes any blobs the
  remote doesn't already have. Every daemon binary also serves the
  matching `RemoteStore` gRPC service on its existing listener so
  peer daemons can use it as a remote. Mutable pointers (op heads,
  ref tips) explicitly out of scope — they need their own
  arbitration story (M10). Detail in [`PLAN-M7-9.md`](./PLAN-M7-9.md)
  §13.

## 3. What's deferred

- **Mutable pointers (op heads, ref tips).** Layer C blobs are content-
  addressed and idempotent across pushes; mutable pointers are not.
  Two daemons over the same `dir://` blob store can sync content but
  not op-log linearity. **M10 owns the catalog protocol — spec at
  §10, in flight.** See [`PLAN-M7-9.md`](./PLAN-M7-9.md) §13.5 for
  the M9 boundary the spec rides on top of.
- **FUSE-side read-through on `StoreMiss`.** `vfs/yak_fs.rs`'s
  `lookup`/`read`/`readdir` paths still map `StoreMiss` to `EIO`. M9
  integrated at `service.rs` rather than `Store`
  ([`PLAN-M7-9.md`](./PLAN-M7-9.md) §13.2 decision), so yak_fs.rs
  doesn't see the remote without the orchestration cost duplicated
  there or `Store` upgraded to async. **M10 §10.6 lands the lazy
  fetch on miss inside `YakFs`** (see §10.6 for why lazy beat
  warm-on-CheckOut).
- **CLI op-store / op-heads-store integration with the catalog.**
  M10's catalog RPCs are daemon-to-daemon. Plumbing them into a
  custom jj-lib `OpHeadsStore` so two CLIs against a shared
  remote actually serialize op-log advances is its own milestone
  (probably M10.5). M10 §10.9 lists this as explicit out-of-scope.
- **Inode handle stability across restarts (§7 decision 6):** the
  in-memory slab still uses monotonic `next_id`. Layer B persists the
  Store but kernel handles still don't survive a daemon restart;
  applications keeping fds open across the restart will see ESTALE.
  Land alongside the `fuser` migration (§9). The slab API is the
  right shape; just swap the id source from monotonic `next_id` to a
  derived `(parent_tree_id, name)` hash.
- **Auth, TLS, retry/backoff for `grpc://` remotes.** Localhost-only,
  single-user, no TLS. M11 alongside S3.
- **Async background push queue.** M9's `Snapshot` blocks until every
  newly-written blob lands on the remote. Fine for `dir://` and
  localhost gRPC, will hurt with a real network remote. The current
  sync code path is the right shape for the queue: `Store::write_*`
  returns the same `(Id, Bytes)` either way; the queue just batches
  `put_blob` instead of inlining them. M10/M11.
- **Sparse patterns:** `set_sparse_patterns` can stay `todo!` until there's
  a real reason. Most yak users probably don't want sparse if the FS is
  already lazy.

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
  perf matters; see §9.)
- **macOS path: keep nfsserve.** macOS mounts `nfs://localhost:N` cleanly
  with `mount_nfs -o port=N,mountport=N,nolocks,vers=3` and no kext.
  macFUSE is increasingly hostile to install on Apple Silicon and ships
  with kext-signing requirements; FUSE-T avoids the kext but trades it
  for a license problem.
- **`fuser` vs `fuse3`:** chose `fuse3` for M3. `fuser` uses sync reply
  callbacks and explicit file handles; `fuse3` is async and value-returning,
  which lines up cleanly with `nfsserve::NFSFileSystem`. Less impedance
  mismatch in the shared trait. (Migration to `fuser` is on the table —
  see §9.)

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
  don't forget. Partial pass landed in [`PLAN-M7-9.md`](./PLAN-M7-9.md)
  §10.3 (`signature_from_proto`, `commit_from_proto`, daemon's
  `panic!("GRPC: …")` shutdown path); the remaining 33 `Mutex::lock().unwrap()`
  in `cli/src/blocking_client.rs` are CLI-process-lifetime safe and not
  in scope.

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
   write commits to a tree. **Implementation lands alongside the
   `fuser` migration** (§9); the M3 slab API is the right shape, just
   swap the id source.

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
     — see §9.
   Until then, real users (and once M7 lands, integration tests)
   see correct semantics with a small per-syscall daemon-dispatch
   tax.

## 8. Recommended starting point

**M1–M9 are done.** Integration tests run with `disable_mount = false`:
`jj yak init`, `jj new`, `jj log`, `jj op log`, and the
`test_nested_tree_round_trips` / `test_symlink_tree_round_trips`
end-to-end paths succeed on a real Linux FUSE mount, the per-mount
Store is durable across daemon restarts (M8 in
[`PLAN-M7-9.md`](./PLAN-M7-9.md) §12), and Layer C remote blob CAS
rides on every write/read RPC + post-snapshot walk (M9 in
[`PLAN-M7-9.md`](./PLAN-M7-9.md) §13). M7 detail in
[`PLAN-M7-9.md`](./PLAN-M7-9.md) §10.

**In flight: M10 — mutable pointers + concurrency arbitration
(§10).** CAS-arbitrated catalog RPCs alongside the existing
`RemoteStore` service (§10.1–10.5), plus FUSE-side lazy
read-through on `StoreMiss` inside `YakFs` (§10.6). Both pieces
ride on top of the M9 read/write/snapshot path. Out of scope for
M10: CLI op-store integration (M10.5), lock-based arbitration
(CAS is enough; §10.1), local-fallback catalog (§10.2). Commit
plan at §10.8.

**Hygiene still pending:**

- **Inode handle stability across restarts** (§7 decision 6).
  Layer B persists the Store but kernel handles still don't survive
  a daemon restart. Land alongside the `fuser` migration (§9). The
  slab API is the right shape; just swap the id source from
  monotonic `next_id` to a derived `(parent_tree_id, name)` hash.
- **`unwrap()` audit.** Partially done in
  [`PLAN-M7-9.md`](./PLAN-M7-9.md) §10.3 and §12. The remaining 33
  `Mutex::lock().unwrap()` in `cli/src/blocking_client.rs` are
  CLI-process-lifetime safe and tracked separately.
- **Tracing for the CLI.** Daemon already has it; CLI gets `RUST_LOG`
  setup that helps debug snapshot/checkout RPC traffic.

## 9. fuser migration (deferred until perf matters)

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

**M10 update (2026-04-30):** that "async in name only" claim is no
longer true at the read paths. M10 §10.6's lazy remote read-through
makes `YakFs::read_tree`/`read_file`/`read_symlink` actually `await`
the `RemoteStore` on local miss. The fuser migration cost goes up
slightly: the sync fuser callback bodies need a runtime handle for
the post-M10 read paths the same way their write paths already
need it. Still small relative to the rest of the migration.

Land alongside Layer B if possible (we'll already be touching
per-Mount state for stable inode ids).

## 10. M10 — mutable pointers + concurrency arbitration

M10 owns two pieces that the M9 outcome left as explicit non-goals
([`PLAN-M7-9.md`](./PLAN-M7-9.md) §13.5):

1. **Catalog protocol** — a CAS-arbitrated mutable name → bytes
   map alongside the existing content-addressed blob store, so two
   daemons over a shared remote can serialize "what's the latest
   op_id" without each silently overwriting the other.
2. **FUSE-side remote read-through on `StoreMiss`.** Today
   `vfs/yak_fs.rs::read_tree`/`read_file`/`read_symlink` map a
   local-store miss straight onto `EIO`. M9 wired the remote into
   `service.rs` for the RPC layer; M10 threads it into the FS
   layer too, so clone-style flows (where the kernel asks the
   mount for blobs only the remote has) work end-to-end.

Wiring jj-lib's actual `OpHeadsStore` / `OpStore` to use the new
catalog RPCs is **out of scope** — that needs a custom store impl
on the CLI side and is a sizeable enough chunk to warrant its own
milestone (probably M10.5). M10 lays the wire and the trait; the
CLI integration rides on top later.

### 10.1 Trait shape

The catalog is "blobs but mutable". It can't share `RemoteStore`'s
content-addressed contract — different keyspace, different
arbitration story — so we extend the trait by adding ref methods
rather than overload `(BlobKind, Id) → bytes`:

```rust
#[async_trait]
trait RemoteStore: Send + Sync + Debug {
    // ... existing M9 methods (get_blob/put_blob/has_blob) ...

    /// Read a ref. `Ok(None)` when the ref does not exist.
    async fn get_ref(&self, name: &str) -> Result<Option<Bytes>>;

    /// Compare-and-swap. If the current value matches `expected`,
    /// install `new` and return `CasOutcome::Updated`. Otherwise
    /// return `CasOutcome::Conflict { actual }` so the caller can
    /// retry against the real current value.
    ///
    /// `expected = None` means "must not exist" (create-only).
    /// `new = None` means "delete".
    async fn cas_ref(
        &self,
        name: &str,
        expected: Option<&Bytes>,
        new: Option<&Bytes>,
    ) -> Result<CasOutcome>;

    /// List every ref name. Refs are scarce (op heads, branch tips,
    /// not arbitrary catalog data), so non-paginated is fine.
    async fn list_refs(&self) -> Result<Vec<String>>;
}

enum CasOutcome { Updated, Conflict { actual: Option<Bytes> } }
```

**Why one trait instead of a sibling `RemoteRefs`:** every backend
that ships in M10 (and every plausible future one — S3, redb-on-a-
shared-nfs, …) wants both surfaces against the same underlying
storage. Two traits would force every Arc-wielding consumer to
hold two `Arc<dyn …>` and route by purpose; one trait keeps the
parse/init/handover paths in `service.rs` single-handle. The
`Debug + Send + Sync` bounds already match.

**Why CAS, not lock-based.** Decided up-front (PLAN.md design Q,
2026-04-30): CAS is lock-free, has no lease state machine, and
matches git's ref-update model in spirit. A CAS loser retries; a
CAS winner advances. There is no expiry/fencing/crash-recovery
path to design because there's nothing held across calls.
Lock-based arbitration becomes interesting if a future workflow
needs to hold a ref across multiple round-trips without races
(e.g. "lock op_heads for the duration of a 5-RPC dance"); we'd
add it as a layer above CAS at that point, not by replacing the
primitive.

**Why `Option<&Bytes>` for `expected`/`new`:** distinguishes "ref
must not exist" from "ref must equal empty bytes", and "delete the
ref" from "set ref to empty". Empty bytes is a valid value (e.g.
the empty op id); we don't get to conflate it with absent.

**Names are UTF-8 strings, not arbitrary bytes.** Simpler and
matches what jj-lib uses internally for op-store keys. If a future
caller needs binary names, hex-encoding at the call site is fine.

### 10.2 Composition

Same shape as M9 §13.2: orchestrate at `service.rs`, keep `Store`
sync. The catalog RPCs in `JujutsuService` (if we decide to expose
any — see §10.5 scope note below) take a `working_copy_path` and
delegate to `Mount.remote_store.cas_ref(...)`. Backends that don't
have a remote configured surface "no catalog available" as
`Status::failed_precondition` — same shape as M9's "no remote
configured" miss path.

Open scope question: should the catalog also have a "local
fallback" (i.e. when no remote is configured, store refs in the
per-mount redb)? **Not in M10.** Mount metadata's `op_id` field
already plays that role for the single-daemon case. The catalog
exists *because* of the multi-daemon case; a one-daemon shortcut
would just be a redundant API. (If we later want "always go
through the catalog API regardless of remote configuration," we
add an in-memory or redb-backed `RemoteStore` impl and point
`mount.remote_store` at it.)

### 10.3 Backends

Both M9 backends gain ref methods. Two impls is still the magic
number for trait extraction; with one, the trait shape gets
warped by what's easy.

- **`FsRemoteStore`** (`dir://`). Refs at `<root>/refs/<name>`.
  CAS dance:
    1. Acquire an exclusive flock on `<root>/refs/.lock` (one
       lockfile for the whole refs namespace — concurrent CAS on
       different refs is rare enough that namespace-wide locking
       beats per-ref lockfile bookkeeping).
    2. Read current value from `<root>/refs/<name>` (`Ok(None)` if
       the file doesn't exist).
    3. Compare to `expected`. Mismatch → release lock, return
       `Conflict { actual }`.
    4. Match → write `new` to `<root>/refs/.tmp.<rand>`, fsync,
       rename into place (or `unlink` if `new = None`). Release
       lock.

  The flock dance lives in `tokio::task::spawn_blocking` so the
  async runtime stays unblocked; same pattern as M9's blob
  put_blob. Names are validated against `/`, NUL, and `..` so
  callers can't escape the refs subdir.

- **`GrpcRemoteStore`** (`grpc://host:port`). Same trait method
  set, but calls translate 1:1 to the new tonic RPCs. Tonic's
  `optional bytes` is an `Option<Vec<u8>>` on the Rust side, so
  the wire-side CAS preconditions round-trip cleanly.

### 10.4 Wire protocol

Add three RPCs to the existing `service RemoteStore` in
`proto/jj_interface.proto`:

```proto
service RemoteStore {
  // existing M9 RPCs ...
  rpc GetRef(GetRefReq) returns (GetRefReply) {}
  rpc CasRef(CasRefReq) returns (CasRefReply) {}
  rpc ListRefs(ListRefsReq) returns (ListRefsReply) {}
}

message GetRefReq { string name = 1; }
message GetRefReply {
  // false = ref does not exist; bytes meaningless when found=false
  bool found = 1;
  bytes value = 2;
}

message CasRefReq {
  string name = 1;
  // proto3 `optional` so absent-vs-empty is distinguishable on
  // the wire (matches the trait's Option<&Bytes>).
  optional bytes expected = 2;
  optional bytes new = 3;
}
message CasRefReply {
  // true = swap applied. false = conflict; `actual` is the value
  // the server saw, which the caller should retry against (or
  // surface).
  bool updated = 1;
  optional bytes actual = 2;
}

message ListRefsReq {}
message ListRefsReply { repeated string names = 1; }
```

Same `RemoteStore` service so peer daemons get refs for free —
the always-on M9 server (`main.rs::RemoteStoreService`) extends
to ref RPCs without a second listener.

### 10.5 Scope at the `JujutsuService` layer

M10 does **not** add catalog-facing RPCs to `JujutsuInterface`.
The CLI doesn't use them yet (per scope: jj-lib op-store
integration is M10.5). The proto-side RPCs land on `RemoteStore`
only — that's the daemon-to-daemon channel.

`Mount.remote_store: Option<Arc<dyn RemoteStore>>` already
carries the new methods through trait extension; tests that want
to exercise refs do so against `Mount.remote_store` directly
(same pattern as the M9 `mount_handles` test helper).

### 10.6 FUSE-side remote read-through on `StoreMiss`

Lazy fetch on miss inside `vfs/yak_fs.rs`. Current shape:

```rust
fn read_tree(&self, id: Id) -> Result<Tree, FsError> {
    match self.store.get_tree(id) {
        Ok(Some(t)) => Ok(t),
        Ok(None) => Err(FsError::StoreMiss),
        Err(e) => Err(store_err(e)),
    }
}
```

Becomes:

```rust
async fn read_tree(&self, id: Id) -> Result<Tree, FsError> {
    if let Some(t) = self.store.get_tree(id).map_err(store_err)? {
        return Ok(t);
    }
    if let Some(remote) = &self.remote {
        return fetch_tree_through(&self.store, remote.as_ref(), id)
            .await
            .map_err(store_err);
    }
    Err(FsError::StoreMiss)
}
```

Same shape for `read_file` and `read_symlink`. The
`fetch_*_through` helpers (verify-round-trip + persist locally)
already exist in `service.rs` — M10 factors them into
`remote/fetch.rs` so both `service.rs` and `yak_fs.rs` share one
implementation. No new round-trip semantics — just a relocation.

Mechanical fallout:

- `YakFs` gains `remote: Option<Arc<dyn RemoteStore>>` (set at
  construction, same as `store`).
- `YakFs::new` becomes `YakFs::new(store, root_tree, remote)`;
  `service.rs::Initialize` and `rehydrate` both pass the parsed
  remote in.
- `read_tree`/`read_file`/`read_symlink` go from `&self -> Result`
  to `async &self -> Result`. Their call sites inside the trait's
  `async` methods already `.await`, so the change propagates.
- Tests that constructed `YakFs::new(store, root_tree)` switch
  to `YakFs::new(store, root_tree, None)`.

**Why lazy, not warm-on-CheckOut.** A clone-style workflow
typically opens a handful of files in a multi-thousand-file tree;
warming the whole tree would page in O(MB) of blob data the user
never touches. Lazy pays exactly for what the kernel asks for.
The downside — first access latency — is bounded by the remote
RTT, which on a localhost peer is sub-millisecond. If a future
workload pathologically opens every file in the tree we revisit;
hybrid (warm + lazy) is a one-flag change at that point.

**Error surfacing.** Remote-fetch failures (transport error,
data-loss on hash mismatch) collapse to `FsError::StoreError`
and propagate as `EIO` to the kernel. Same as today's M9 RPC-
layer fetch — the user gets a real error rather than a silent
hang.

### 10.7 Test strategy

- **Backend unit tests (FsRemoteStore)** — get/cas/list, CAS hit
  path, CAS conflict path returns the actual current value, CAS
  with `expected = None` succeeds only when the ref doesn't
  exist, CAS with `new = None` deletes, name validation rejects
  `/` and `..`.
- **Server unit tests (RemoteStoreService)** — ref RPCs round-
  trip; `optional` field present-vs-absent decoded correctly.
- **gRPC end-to-end** — two `GrpcRemoteStore` clients sharing a
  server: one CASes from None→`v0`; the other observes via
  `get_ref`; the loser's CAS sees `Conflict { actual: v0 }`.
- **FUSE-side read-through** — a service-level test analogous to
  M9's `read_file_falls_back_to_remote_on_local_miss` but
  exercising the FS path: drive a `lookup`/`read` against an
  inode whose tree/file blob exists only on the remote; confirm
  it returns the right bytes and that the second access hits the
  cached local blob.
- **Negative case** — `StoreMiss` with no remote still surfaces
  as `EIO` (preserves pre-M10 behavior for tests that don't
  configure a remote).

### 10.8 Commit plan

One commit per task:

1. PLAN.md §10 (this section). _← in progress_
2. Proto: add `GetRef` / `CasRef` / `ListRefs` RPCs + messages.
3. `RemoteStore` trait gains `get_ref` / `cas_ref` / `list_refs`;
   `FsRemoteStore` impl + unit tests.
4. `RemoteStoreService` server impl + unit tests; `GrpcRemoteStore`
   client impl + the gRPC end-to-end test.
5. FUSE-side remote read-through: extract shared
   `fetch_*_through` helpers into `remote/fetch.rs`; thread
   `Option<Arc<dyn RemoteStore>>` into `YakFs`; flip
   `read_tree`/`read_file`/`read_symlink` to async; update call
   sites; add a service-level read-through-via-FS test.
6. PLAN.md §10.9 — M10 outcome.

### 10.9 Out of scope (explicit)

- **CLI integration with jj-lib's op store / op-heads store.**
  Needs a custom impl that drives the new catalog RPCs from
  `cli/src/`. Sizeable; M10.5.
- **Lock-based arbitration / leases.** §10.1 above. CAS is the
  primitive; leases land if/when a real workflow needs them.
- **Local-fallback catalog when no remote is configured.**
  §10.2. The single-daemon case already has `mount.toml`'s
  `op_id`.
- **Auth, TLS, retry/backoff, streaming.** Still M11 alongside
  S3.
- **Stable inode ids across restarts (PLAN §7 decision 6).**
  Still deferred; the M10 read-through change touches the same
  `YakFs` struct, but the slab-id source is unchanged.
- **Async background push queue.** Still M10/M11 follow-up;
  current `Snapshot` blocks on remote.
