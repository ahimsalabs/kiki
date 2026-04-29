# jj-yak: Implementation Plan

Status: active. Transport architecture decided (§4.3 Path C). M1–M10
done — see [`PLAN-M1-6.md`](./PLAN-M1-6.md) for M1–M6 detail,
[`PLAN-M7-9.md`](./PLAN-M7-9.md) for M7–M9 detail (including the
"Ship (d)" interim that landed between M6 and M7), and §10 below
for M10 detail (mutable pointers + FUSE-side read-through). One-
line state: integration tests run with `disable_mount = false` on
a real Linux FUSE mount; the per-mount `Store` is redb-backed and
rehydrates across daemon restart; the per-mount `RemoteStore`
(parsed from `Initialize.remote`) does write-through +
read-through + post-snapshot push against `dir://` and `grpc://`
backends; M10 added a CAS-arbitrated mutable refs catalog
(`get_ref` / `cas_ref` / `list_refs` on the same `RemoteStore`
service) and threaded the remote into `YakFs` so FUSE-side reads
fall through to the remote on local-store miss. 137/115 daemon
tests + 14/14 cli tests pass; `cargo clippy --workspace
--all-targets -- -D warnings` is clean. Inode handle stability (§7
decision 6) is still deferred; the in-memory slab is fine until
kernel handles need to survive a daemon restart in production.
M10.5 (§10.5) — wiring jj-lib's `OpHeadsStore` to the catalog —
is in progress. Last updated: 2026-04-29

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

- **Mutable pointers (op heads, ref tips) — daemon-to-daemon
  protocol done at M10 (§10).** `RemoteStore` gained
  `get_ref` / `cas_ref` / `list_refs` over the same gRPC service
  any daemon already serves; CAS arbitration matches git's
  ref-update model (§10.1). `dir://` and `grpc://` backends both
  implement them. **CLI op-heads-store integration with the
  catalog is M10.5 (§10.5).** Op contents (the actual operation
  bytes, currently in `.jj/op_store/`) need their own milestone
  — plumbing them through the remote so a peer daemon can read
  the bytes of an op another daemon wrote is independent design
  work; tentatively M10.6.
- **FUSE-side read-through on `StoreMiss`** — done at M10 §10.6.
  `YakFs` now holds an `Option<Arc<dyn RemoteStore>>`;
  `read_tree`/`read_file`/`read_symlink` are async and fall
  through to the remote on local-store miss, sharing the
  verify-round-trip + persist helpers in `remote/fetch.rs` with
  `service.rs`.
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

**M1–M10 are done.** Integration tests run with `disable_mount =
false`: `jj yak init`, `jj new`, `jj log`, `jj op log`, and the
`test_nested_tree_round_trips` / `test_symlink_tree_round_trips`
end-to-end paths succeed on a real Linux FUSE mount; the per-mount
Store is durable across daemon restarts (M8 in
[`PLAN-M7-9.md`](./PLAN-M7-9.md) §12); Layer C remote blob CAS
rides on every write/read RPC + post-snapshot walk (M9 in
[`PLAN-M7-9.md`](./PLAN-M7-9.md) §13); M10 added the catalog
protocol (CAS-arbitrated mutable refs alongside the blob CAS,
§10.1–10.5) and FUSE-side lazy read-through inside `YakFs`
(§10.6). M7 detail in [`PLAN-M7-9.md`](./PLAN-M7-9.md) §10; M10
detail in §10 above.

**Next: M10.5 — wire jj-lib's op-heads-store to the catalog.**
Spec at §10.5. Custom `YakOpHeadsStore` driven by the daemon's
catalog (CLI talks to local daemon over `JujutsuInterface`, daemon
delegates to either the configured remote or a per-mount local
fallback). Single `op_heads` ref, length-prefixed list of op-ids.
Op-store contents are explicitly out of scope — that's M10.6.
Also still open: M11 (auth/TLS/retry/backoff alongside S3), the
`fuser` migration (§9), inode-id stability across restarts (§7
decision 6), async background push queue.

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

### 10.10 M10 outcome

Five commits across the M10 sequence (numbered to match §10.8):

1. `docs: PLAN.md §10 — define M10 spec`. ✅
2. `proto: M10 — ref RPCs (GetRef/CasRef/ListRefs) on service
   RemoteStore`. ✅ Server-side stubs (`Status::unimplemented`)
   kept the daemon-bin compilable on its own; the next commit
   replaced them with real delegations.
3. `daemon: M10 — RemoteStore trait gains ref methods + backends +
   tests`. ✅ Combined the originally-planned commits 3+4 (trait +
   FsRemoteStore + server delegate + GrpcRemoteStore client +
   matching tests) into one logical "the catalog now exists at
   the daemon-to-daemon RPC layer" landing. The split would have
   needed throwaway intermediate stubs without independent
   verification value.
4. `daemon: M10 — FUSE-side remote read-through on StoreMiss`. ✅
5. `docs: PLAN.md §10.10 — M10 outcome`. ✅ (this commit)

137/115 daemon tests (+22 from M9 baseline of 115) + 14/14 cli
tests pass; `cargo clippy --workspace --all-targets -- -D
warnings` is clean.

Decisions made on the way:

- **One trait, not two (`RemoteStore` extended rather than a
  sibling `RemoteRefs`).** Every backend that lands now or in
  the foreseeable future (fs, grpc, eventually S3, redb-on-NFS)
  wants both surfaces against the same underlying storage.
  Two traits would force every Arc-wielding consumer to hold
  two `Arc<dyn ...>` and route by purpose. The trait grew from
  3 methods to 6; still small.

- **Cross-process advisory locking via `libc::flock`, not an
  in-process Mutex.** `dir://` is the explicit "two daemons
  share an NFS dir" backend (M9 §13.3); a `parking_lot::Mutex`
  would only catch same-process races. Implemented as a RAII
  `RefsLock` guard in `remote/fs.rs` — acquires `LOCK_EX` on
  `<refs>/.lock`, releases on drop. Held across the read +
  rename, dropped before the result returns. No new dep.

- **Sentinel lockfile is namespace-wide, not per-ref.** Refs
  are scarce (op heads, branch tips — not arbitrary catalog
  data), and CAS holds the lock for one read + one rename
  (~microseconds). Per-ref lockfiles would multiply inode
  churn for a contention pattern that doesn't exist.

- **proto3 `optional` for the CAS preconditions.** The trait's
  `Option<&Bytes>` distinguishes "absent" (must-not-exist /
  delete) from `Some(empty)` (must-equal-empty / set-to-empty).
  `optional bytes` is the only proto3 way to round-trip that
  distinction across the wire. The corresponding tests
  (`cas_ref_empty_value_distinct_from_absent`,
  `cas_ref_create_only_against_existing_conflicts`) catch the
  "backend conflates them" regression class up front.

- **Server-side stubs in the proto-only commit, not a single
  consolidated commit.** Adding RPCs to `service RemoteStore`
  breaks the existing `RemoteStoreService` impl block (it no
  longer implements every trait method). Rather than collapse
  proto + trait extension + impl into one giant commit, the
  proto commit ships compile-clean with three
  `Err(Status::unimplemented(...))` stubs that the next commit
  replaces. Five extra throwaway lines vs. proper isolated
  commits — worth it.

- **Factor read-through helpers into `remote/fetch.rs`.** §10.6
  said factor; we did. The helpers return a typed `FetchError`
  (NotFound / DataLoss / Decode / DecodeValue / LocalWrite /
  Remote), so `service.rs` maps onto gRPC `Status` codes and
  `vfs/yak_fs.rs` maps onto `FsError`. Both consumers reach for
  the variant they care about (typically NotFound and DataLoss)
  and collapse the rest. No more `verify_round_trip` duplicated
  across modules.

- **`impl Display for Id` in `ty.rs`.** New typed `FetchError`
  variants want `{kind} {id}` rendering in `#[error(...)]`;
  `Id` had no `Display`. Added a 5-line impl next to the type
  definition (proper home). `service.rs`'s private `hex()`
  helper stays — still used by the post-snapshot push walk and
  `tracing::info!` macros that want a stable `%`-renderer. A
  separate cleanup pass could collapse the two; out of M10
  scope.

- **`YakFs::read_*` go async; six private helpers follow.**
  Mechanical propagation: `read_tree`, `read_file`,
  `read_symlink` await `RemoteStore::get_blob` on miss; their
  callers (`dir_tree`, `ensure_dirty_tree`, `ensure_dirty_file`,
  `remove_from_slab`, `child_exists`, `attr_for`) become async.
  `snapshot_node` stays sync — it only writes, never hits the
  read path. Two match-by-ref patterns (in `remove`'s pinned-
  `.jj/` empty-check and `rename`'s dst-empty-check) needed
  restructuring so the `Inode` handle drops before the
  `read_tree(...).await`; both pulled out a small enum so the
  match is sync-only and the await is sequential.

- **`read()`'s `NodeRef::File` arm pre-fetches the owned `Vec`.**
  The pre-M10 body matched `inode.node` by ref to get a `&[u8]`
  slice into `DirtyFile`'s buffer. With `read_file` now async,
  matching by ref and awaiting inside the arm is awkward;
  pre-fetching the file content into an `Option<Vec<u8>>`
  before the borrow-pinning match is the smaller diff.

- **`mkdir`'s pinned-`.jj/` arm releases the
  `parking_lot::Mutex` before awaiting `attr_for`.**
  `parking_lot::MutexGuard` is `!Send`; held across an
  `.await` it would break the `JjYakFs: Send + Sync` bound.
  Wrapped the lock in a block so the guard drops before the
  await.

- **Service-level FUSE read-through tests live in
  `service.rs`, not `vfs/yak_fs.rs::tests`.** Drives a real
  `JujutsuService` with a configured `dir://` remote so the
  test exercises the same `Initialize` → `Mount.remote_store`
  → `YakFs::new(..., remote)` plumbing that production hits.
  `vfs/yak_fs.rs::tests` could mock a `RemoteStore` directly,
  but the integration version catches a wiring regression
  (e.g. forgetting to pass `remote_store.clone()` to
  `YakFs::new` in either `Initialize` or `rehydrate`) that a
  unit test wouldn't.

- **Re-export `FileKind` under `cfg(test)`.** Production code
  doesn't need it at the `crate::vfs::*` surface; only the new
  service-level read-through test does. Gating the re-export
  keeps `clippy --all-targets -- -D warnings` clean without an
  `#[allow(unused_imports)]`.

What this milestone explicitly does **not** do (preserved from
§10.9, repeated here so it's findable in the outcome):

- CLI integration with jj-lib's op-store / op-heads store. Needs
  a custom store impl driving the new catalog RPCs; sizeable
  enough to warrant its own milestone. **M10.5.**
- Lock-based arbitration / leases. CAS is the primitive; leases
  if/when a real workflow needs them.
- Local-fallback catalog (catalog-API even without a remote).
  Single-daemon case already covered by `mount.toml`'s `op_id`.
- Auth, TLS, retry/backoff, streaming for large blobs/refs.
  M11 alongside S3.
- Stable inode ids across restarts (§7 decision 6). With the
  `fuser` migration (§9). M10 did slightly raise the cost of
  that migration: `JjYakFs::read_*` are no longer "async in
  name only" — they genuinely await on miss, so the fuser
  adapter's sync→async bridge has more to do at the read paths
  than the §9 estimate accounted for.

Test coverage added in M10 (22 tests, total 137/115):

- `daemon::remote::tests` (validation / outcome equality):
  `ref_name_validation` (accepted + rejected cases),
  `cas_outcome_eq` (Some(empty) ≠ None on Conflict).
- `daemon::remote::fs::tests` (FS backend, 11 new):
  `ref_missing_returns_none`,
  `cas_ref_create_then_read`,
  `cas_ref_create_only_conflicts_when_present`,
  `cas_ref_advance_with_correct_expected`,
  `cas_ref_stale_expected_returns_actual`,
  `cas_ref_delete`,
  `cas_ref_delete_with_stale_expected_conflicts`,
  `cas_ref_empty_value_distinct_from_absent`,
  `list_refs_returns_sorted_names_and_hides_internals`,
  `list_refs_on_empty_store_is_ok`,
  `cas_ref_rejects_bad_name`.
- `daemon::remote::server::tests` (RemoteStoreService, 5 new):
  `ref_round_trip`,
  `cas_ref_conflict_carries_actual`,
  `cas_ref_create_only_against_existing_conflicts`,
  `list_refs_round_trip`,
  `ref_rpcs_reject_bad_name`.
- `daemon::remote::grpc::tests`: `grpc_ref_cas_round_trip`
  end-to-end against a real tonic listener.
- `daemon::service::tests` M10 cases:
  `fuse_layer_reads_through_remote_on_local_miss`,
  `fuse_layer_store_miss_no_remote_is_failed_precondition`,
  `fuse_layer_read_through_populates_local_cache`.

## 10.5. M10.5 — wire jj-lib's op-heads store to the catalog

M10 (§10) shipped the catalog protocol — `get_ref`/`cas_ref`/
`list_refs` on `service RemoteStore`, with CAS arbitration matching
git's ref-update model. Two daemons over a shared `dir://` remote
can already serialize ref updates against each other. What's still
missing is the CLI side: jj-lib's `Workspace::init_with_factories`
in `cli/src/main.rs` still uses `ReadonlyRepo::default_op_heads_store_initializer`,
which produces a `SimpleOpHeadsStore` — empty files at
`<repo>/op_heads/heads/<hex(op_id)>`. With M7's pinned-`.jj/`
subtree those writes go through FUSE into the per-mount `Store`,
so they're content-addressed but **not** catalog-arbitrated. Two
CLIs against a shared remote can sync blobs (M9) but silently
clobber each other's op-log advances.

M10.5's job is plumbing the catalog into jj-lib's `OpHeadsStore`
trait, so that "advance the latest op_id" goes through `cas_ref`
instead of the local filesystem.

### 10.5.1 Scope

In:

- Custom `YakOpHeadsStore` impl in `cli/src/op_heads_store.rs`,
  driven by the daemon's catalog RPCs.
- `JujutsuInterface` gains the three catalog RPCs (CLI never
  dials a remote directly — every CLI traffic still goes to the
  local daemon, which delegates to its `Mount.remote_store` or
  the local fallback).
- A per-mount **local-fallback** catalog backed by a refs table
  inside the existing per-mount redb file. Used when no remote
  URL is configured, so the catalog API works uniformly. §10.2's
  "no local-fallback in M10" explicitly anticipated this as the
  M10.5 follow-up.
- Two-CLI integration test against a shared `dir://` remote.

Out (deferred):

- **Custom `YakOpStore` (op contents).** The op contents (operations,
  views) still live in `.jj/op_store/` over FUSE → per-mount Store,
  not pushed anywhere. So a two-CLI shared op log won't work
  end-to-end yet — CLI_B can't read the bytes of operations CLI_A
  wrote. M10.5 closes the **arbitration** story (who-wins on
  concurrent op-log advance); M10.6 closes the **content** story
  (CLI_B can actually fetch CLI_A's op bytes). Two milestones
  because they're independent design problems with their own
  trade-offs.
- Auth/TLS/retry. Still M11 alongside S3.
- Async background push queue.
- Inode stability across restarts (§7 decision 6).

### 10.5.2 Decisions

1. **Catalog access from CLI: via JujutsuInterface, not direct.**
   The CLI's `BlockingJujutsuInterfaceClient` already has a
   single channel to the local daemon for every other RPC (blob
   IO, snapshot, checkout, status). Adding `GetRef`/`CasRef`/
   `ListRefs` to `JujutsuInterface` keeps CLI traffic single-
   handle. The daemon already owns the per-mount `RemoteStore`
   (and now the local fallback); routing the catalog through it
   is symmetric with the rest. The alternative — CLI dials the
   `dir://`/`grpc://` remote directly — would duplicate the URL
   parser, force the CLI to know about backend authentication
   later, and split "the daemon is the source of truth for the
   mount" into "...except for refs."

2. **Local fallback when no remote configured.** Without a
   fallback, every existing test (which passes `remote = ""`)
   would break the moment we swap in `YakOpHeadsStore`. With a
   fallback, `YakOpHeadsStore` is unconditional and the catalog
   API is uniform. §10.2 said "we'd add an in-memory or
   redb-backed `RemoteStore` impl and point `mount.remote_store`
   at it" — that's exactly what M10.5 does, except we keep
   `mount.remote_store: Option<...>` (so the M9 blob-CAS no-op
   semantics for "no remote" stay unchanged) and add a separate
   `mount.local_refs: Arc<LocalRefs>` for the catalog. Routing
   logic: catalog RPC handlers prefer `mount.remote_store`'s ref
   methods if Some, otherwise hand off to `mount.local_refs`.

3. **Single 'op_heads' ref, not per-head refs.** One ref keyed
   `op_heads`, value = concatenated 32-byte (or whatever
   length) op-id bytes — length-prefixed so we can mix lengths
   if jj-lib ever changes op-id width. `update_op_heads(old=[…],
   new=…)` becomes a single CAS read+swap; the loser sees
   `Conflict { actual: <real heads list> }` and resolves in one
   round-trip. Per-head refs (one ref per head, mirroring
   `simple_op_heads_store.rs`'s file-per-head shape) would make
   `update_op_heads` non-atomic across the multi-step write+
   delete and force `list_refs` to filter by prefix. Both are
   safe — `resolve_op_heads` merges divergent heads on next load
   either way — but single-ref uses CAS the way CAS was meant to
   be used (the heads-set is the unit of arbitration), and the
   serialized list is tiny (32B × handful of heads).

4. **Op-heads ref naming: `op_heads` (no workspace suffix).**
   The jj-lib `OpHeadsStore` trait is repo-scoped, not
   workspace-scoped (`update_op_heads` takes no workspace
   argument). Op-heads belong to the repo. If a future repo
   ever wanted multiple op-head namespaces under one remote,
   we'd add a prefix; for now, one repo per remote.

5. **Wire format: length-prefixed concat of op-id bytes.**
   `[u32 len_be][len bytes][u32 len_be][len bytes]…`. Empty
   value (`Bytes::new()`) means "no heads" — distinct from
   "ref does not exist" (which is also "no heads" but
   pre-initialization). Trivially round-trips; ~10 LoC of
   serialize/parse in `cli/src/op_heads_store.rs`.

6. **Locking.** `OpHeadsStore::lock` returns a no-op token. The
   trait doc says "the lock is not needed for correctness"; the
   M10 CAS protocol gives us per-update arbitration without a
   distinct lock primitive. If we later need a real lock (e.g.
   to hold ref state across multiple round-trips for a complex
   resolve), we'd add it as a layer above CAS — not by replacing
   it. (Same logic as §10.1's "why CAS, not lock-based.")

### 10.5.3 Wire protocol

Add three RPCs to `service JujutsuInterface` in
`proto/jj_interface.proto`. They are the same shape as the M10
`RemoteStore` ref RPCs but carry `working_copy_path` so the daemon
can route to the per-mount catalog handle:

```proto
service JujutsuInterface {
  // ... existing RPCs ...
  rpc GetCatalogRef(GetCatalogRefReq) returns (GetCatalogRefReply) {}
  rpc CasCatalogRef(CasCatalogRefReq) returns (CasCatalogRefReply) {}
  rpc ListCatalogRefs(ListCatalogRefsReq) returns (ListCatalogRefsReply) {}
}
```

The `GetCatalogRef`/`CasCatalogRef`/`ListCatalogRefs` names disambiguate
from the `RemoteStore` service's same-named RPCs — proto3 allows
the conflict (different services) but it's clearer in code-gen
output.

Same `Option<Bytes>` semantics on `expected`/`new` (proto3
`optional`) as the M10 `CasRef` — distinguishes absent-vs-empty.

### 10.5.4 Storage layout

`LocalRefs` opens a single redb table `refs_v1` inside the
per-mount `store.redb` (the same file the per-mount `Store`
already owns). Key: ref name as `&str`. Value: ref bytes.
Acquisition: `Store::open` returns the existing `Database`;
`LocalRefs::new(db.clone())` opens/creates the table on first
use. CAS atomicity comes from `redb`'s `WriteTransaction`
serialization — the whole CAS check + apply runs inside one
transaction.

### 10.5.5 Test strategy

- **`LocalRefs` unit tests** — get/cas/list, conflict path
  carries actual, create-only against absent succeeds, delete,
  empty-vs-absent distinction. Mirrors the FsRemoteStore
  ref-method tests.
- **Service-level catalog dispatch tests** — drive a service
  with `remote = ""` (so `Mount.remote_store = None`) and
  confirm catalog RPCs hit `LocalRefs`; drive a service with
  `remote = dir:///…` and confirm they hit the FsRemoteStore.
- **`YakOpHeadsStore` unit tests** — drive against a fake
  `BlockingJujutsuInterfaceClient` (or a real daemon in
  test env), exercise update_op_heads/get_op_heads/serialize/
  deserialize.
- **Two-CLI acceptance test** — two daemons sharing one
  `dir:///<tmp>` remote; CLI_A advances op-heads; CLI_B
  advances op-heads concurrently; one wins, the other sees
  Conflict and retries; final state has both op-heads merged
  (or one fast-forwarded). The test validates only the
  arbitration property (no clobber); reading CLI_A's op
  contents from CLI_B is M10.6.

### 10.5.6 Commit plan

One commit per logical step:

1. PLAN.md §10.5 (this section). _← in progress_
2. Daemon: `LocalRefs` per-mount catalog (redb-backed) + unit tests.
3. Proto + daemon: catalog RPCs on `JujutsuInterface` + dispatch
   to remote-or-local + service tests.
4. CLI: `BlockingJujutsuInterfaceClient` gains the three catalog
   methods + `YakOpHeadsStore` impl + register factory in
   `Workspace::init_with_factories`.
5. Two-CLI acceptance test.
6. PLAN.md §10.5 outcome.

### 10.5.7 Pickup notes

The current M10 description in `service.rs` and `remote/mod.rs`
already gives us:

- `Mount.remote_store: Option<Arc<dyn RemoteStore>>`. M10.5 keeps
  the `Option`; the local-fallback work happens via a sibling
  field, not by always-Some-ing remote_store.
- `validate_ref_name`. Reuse for all catalog RPC handlers,
  including the local-fallback path.
- `CasOutcome { Updated | Conflict { actual: Option<Bytes> } }`.
  The same enum threads through the new RPCs.

What jj-lib expects from a custom `OpHeadsStore`:

- `name() -> &str` — pick `"yak_op_heads"`.
- `update_op_heads(old_ids: &[OperationId], new_id: &OperationId)
   -> Result<(), OpHeadsStoreError>`. CAS read+swap.
- `get_op_heads() -> Result<Vec<OperationId>, OpHeadsStoreError>`.
  One get_ref + parse.
- `lock() -> Result<Box<dyn OpHeadsStoreLock + '_>,
   OpHeadsStoreError>`. No-op token.

The factory side: `StoreFactories::add_op_heads_store("yak_op_heads", ...)`
in `create_store_factories`, and replace `default_op_heads_store_initializer()`
in `Workspace::init_with_factories` with a closure that constructs
`YakOpHeadsStore`.
