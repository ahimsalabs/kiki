# kiki: Implementation Plan

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
service) and threaded the remote into `KikiFs` so FUSE-side reads
fall through to the remote on local-store miss. 137/115 daemon
tests + 14/14 cli tests pass; `cargo clippy --workspace
--all-targets -- -D warnings` is clean. Inode handle stability (§7
decision 6) is still deferred; the in-memory slab is fine until
kernel handles need to survive a daemon restart in production.
M10.5 (§10.5) is done — `KikiOpHeadsStore` drives the daemon's
catalog (with a per-mount `LocalRefs` redb-backed fallback for
the no-remote case) so two CLIs against a shared `dir://` remote
serialize op-log advances rather than silently clobbering. M10.6
(§10.6) is done — `KikiOpStore` routes op-store contents
(operations + views) through the daemon with write-through to
remote and read-through on local miss, so a peer CLI can read
the bytes of ops another CLI wrote. `RemoteStore` blob ids
generalized from `&Id` (32-byte) to `&[u8]` so 64-byte
BLAKE2b-512 op-store ids ride on the same blob CAS.
192/167-daemon tests pass. Last updated: 2026-04-30

This document captures the roadmap for getting kiki from "scaffold with
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
│  Layer C: Remote storage      <── the "kiki" in kiki           │
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
- **M2.** Wired `KikiWorkingCopyFactory` into `Workspace::init_with_factories`
  so `jj kk init` flows: factory → `KikiWorkingCopy::init` →
  `SetCheckoutState` RPC. Op-id round-trips end-to-end through `jj op log`.
- **M3.** Split `daemon/src/vfs.rs` into a `JjKikiFs` trait + concrete
  `KikiFs` + `NfsAdapter` (macOS) and `FuseAdapter` (Linux). Read-only
  surface: `lookup` / `getattr` / `read` / `readdir` / `readlink`.
- **M4.** `jj kk init` actually mounts. Mountpoint validation,
  per-mount `Store` (every store RPC carries `working_copy_path`),
  `VfsManager` bind protocol, platform-specific attach
  (`fuse3::Session::mount_with_unprivileged` on Linux,
  `nfsserve::tcp::NFSTcpListener` + `mount_nfs` shellout on macOS),
  and the `InitializeReply.transport` oneof. Added the `disable_mount`
  test-mode flag.
- **M5.** `CheckOut` RPC + `JjKikiFs::check_out` re-roots the inode
  slab via `swap_root`. CLI's `LockedKikiWorkingCopy::check_out` calls
  it. Conflicted trees rejected (single-id only).
- **M6.** VFS write path + snapshot. Trait grew `create_file` /
  `mkdir` / `symlink` / `write` / `setattr` / `remove` / `snapshot`.
  Lazy clean→dirty promotion on the inode slab; recursive sync
  snapshot persists into the per-mount `Store` and preserves inode ids
  across snapshot. Adapters dispatch to the trait. `Snapshot` RPC
  delegates to `JjKikiFs::snapshot`.

After M6 the daemon-side VCS surface is feature-complete.

### M7–M9 — `.jj/` separation, durable storage, remote CAS (✅ done)

Full implementation log archived in [`PLAN-M7-9.md`](./PLAN-M7-9.md)
(plus the "Ship (d)" interim that flipped `TTL=0` and filled in
missing FUSE methods between M6 and M7). One-line summaries:

- **M7.** Two changes that together let `disable_mount = false` flip:
  - **M7.1** — `.jj/` lives outside the content-addressed user tree.
    `KikiFs::jj_subtree: Mutex<Option<InodeId>>` pins the metadata
    directory across `check_out` and excludes it from `snapshot`.
    `mkdir(root, ".jj")` populates the pin; `lookup`/`readdir`/
    `remove`/`rename` short-circuit at the root.
  - **M7.2** — two coupled bugs that broke `jj new` end-to-end on a
    real mount: (a) `swap_root` cleared the entire `by_parent`
    cache, severing the chain through pinned `.jj/`; fixed by
    clearing only `(ROOT_INODE, *)` entries. (b) `LockedKikiWorkingCopy::
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
  protocol done at M10 (§10), CLI integration done at M10.5
  (§10.5).** `RemoteStore` gained `get_ref` / `cas_ref` /
  `list_refs` over the same gRPC service any daemon already
  serves (M10); `JujutsuInterface` gained
  `Get/Cas/ListCatalogRefs` for CLI access (M10.5). The CLI's
  `KikiOpHeadsStore` writes a single `op_heads` ref via CAS
  retry, with a per-mount `LocalRefs` redb-backed fallback when
  no remote is configured. **Op contents** (the actual operation
  bytes) done at M10.6 (§10.6) — `KikiOpStore` routes
  `read/write_view` and `read/write_operation` through the
  daemon with write-through to remote and read-through on local
  miss, so a peer CLI can read the bytes of ops another CLI wrote.
- **FUSE-side read-through on `StoreMiss`** — done at M10 §10.6.
  `KikiFs` now holds an `Option<Arc<dyn RemoteStore>>`;
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
- **Auth, TLS for `grpc://` remotes.** Localhost-only,
  single-user, no TLS. M12 alongside git convergence and S3.
- **Async background push queue + offline resilience.** Active —
  M11 (§11). Retry/backoff bundled into M11 (the push queue
  needs a retry strategy by construction).
- **Gitignore-aware VFS.** Active — M10.7 (§10.7). Prerequisite
  for real-world use with package managers and agent workflows.
- **Sparse patterns:** `set_sparse_patterns` can stay `todo!` until there's
  a real reason. Most kiki users probably don't want sparse if the FS is
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
(`fusermount3` is setuid; `jj kk init` runs as the user with no `sudo`).

**inotify/Watchman wouldn't see server-side mutations over NFS.** True,
and `snapshot` without fsmonitor walks the entire WC. **Closed by §4.3
on Linux:** FUSE adapter pushes invalidations via `notify_inval_inode`
when `check_out` mutates the tree, so the kernel re-stats on next access
without scanning. macOS still has the original problem; see §4.2.

**fsmonitor strategy still TBD for snapshot.** Resolved as a non-blocker
by M6's actual shape (see §7 #7): the daemon's VFS owns every write, so
`JjKikiFs::snapshot` walks the slab and produces the rolled-up tree id
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
trait JjKikiFs (≈15 async methods, our own type names)
   ├── NfsAdapter:  impl nfsserve::NFSFileSystem for &dyn JjKikiFs
   └── FuseAdapter: impl fuse3::Filesystem      for &dyn JjKikiFs
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
  - `LockedKikiWorkingCopy` has 6 `todo!`s, not just `check_out`: `recover`
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

1. **Transport architecture (§4.3).** Thin internal `JjKikiFs` trait,
   `fuse3` adapter on Linux, `nfsserve` adapter on macOS. Done in M3:
   trait + concrete `KikiFs` + both adapters live in
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
   fsmonitor at all — `JjKikiFs::snapshot` walks the slab and produces
   the rolled-up tree id directly. (Option (b) from §4.1 in spirit:
   daemon already knows what's dirty; jj-lib never has to scan.) Real
   integration with `WorkingCopy::snapshot` happens via the existing
   `LockedKikiWorkingCopy::snapshot` → `snapshot_via_daemon` path; no
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

**M1–M10.5 are done.** Integration tests run with `disable_mount =
false`: `jj kk init`, `jj new`, `jj log`, `jj op log`, and the
`test_nested_tree_round_trips` / `test_symlink_tree_round_trips`
end-to-end paths succeed on a real Linux FUSE mount; the per-mount
Store is durable across daemon restarts (M8 in
[`PLAN-M7-9.md`](./PLAN-M7-9.md) §12); Layer C remote blob CAS
rides on every write/read RPC + post-snapshot walk (M9 in
[`PLAN-M7-9.md`](./PLAN-M7-9.md) §13); M10 added the catalog
protocol (CAS-arbitrated mutable refs alongside the blob CAS,
§10.1–10.5) and FUSE-side lazy read-through inside `KikiFs`
(§10.6). M10.5 wires jj-lib's `OpHeadsStore` to the catalog so
two CLIs against a shared `dir://` remote serialize op-log
advances rather than silently clobbering — `KikiOpHeadsStore`
on the CLI side, `LocalRefs` redb-backed fallback on the daemon
side for the no-remote case (§10.5). M7 detail in
[`PLAN-M7-9.md`](./PLAN-M7-9.md) §10; M10 detail in §10 above;
M10.5 detail in §10.5 above.

**M10.6 (§10.6) is done.** `KikiOpStore` routes op-store contents
through the daemon with write-through to remote and read-through
on local miss. Blob ids generalized from `&Id` (32-byte) to
`&[u8]` so 64-byte BLAKE2b-512 op-store ids ride on the same
`RemoteStore` blob CAS. Two CLIs sharing a `dir://` remote can
each read the other's full operation history.

**Still open:** M10.7 (gitignore-aware VFS, §10.7), M11 (async
push queue + offline resilience, §11), M12 (auth/TLS/S3/git
convergence), the `fuser` migration (§9), inode-id stability
across restarts (§7 decision 6).

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

Pre-requisite reading: our `JjKikiFs` async methods are "async in
name only" — every body is sync (parking_lot mutex + Store calls
are sync as of M6). So the sync→async bridge in the new fuser
adapter is light: the bodies don't await anything we'd lose by
switching to a sync trait.

**M10 update (2026-04-30):** that "async in name only" claim is no
longer true at the read paths. M10 §10.6's lazy remote read-through
makes `KikiFs::read_tree`/`read_file`/`read_symlink` actually `await`
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
   `vfs/kiki_fs.rs::read_tree`/`read_file`/`read_symlink` map a
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

Lazy fetch on miss inside `vfs/kiki_fs.rs`. Current shape:

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
`remote/fetch.rs` so both `service.rs` and `kiki_fs.rs` share one
implementation. No new round-trip semantics — just a relocation.

Mechanical fallout:

- `KikiFs` gains `remote: Option<Arc<dyn RemoteStore>>` (set at
  construction, same as `store`).
- `KikiFs::new` becomes `KikiFs::new(store, root_tree, remote)`;
  `service.rs::Initialize` and `rehydrate` both pass the parsed
  remote in.
- `read_tree`/`read_file`/`read_symlink` go from `&self -> Result`
  to `async &self -> Result`. Their call sites inside the trait's
  `async` methods already `.await`, so the change propagates.
- Tests that constructed `KikiFs::new(store, root_tree)` switch
  to `KikiFs::new(store, root_tree, None)`.

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
   `Option<Arc<dyn RemoteStore>>` into `KikiFs`; flip
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
  `KikiFs` struct, but the slab-id source is unchanged.
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
  `vfs/kiki_fs.rs` maps onto `FsError`. Both consumers reach for
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

- **`KikiFs::read_*` go async; six private helpers follow.**
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
  `.await` it would break the `JjKikiFs: Send + Sync` bound.
  Wrapped the lock in a block so the guard drops before the
  await.

- **Service-level FUSE read-through tests live in
  `service.rs`, not `vfs/kiki_fs.rs::tests`.** Drives a real
  `JujutsuService` with a configured `dir://` remote so the
  test exercises the same `Initialize` → `Mount.remote_store`
  → `KikiFs::new(..., remote)` plumbing that production hits.
  `vfs/kiki_fs.rs::tests` could mock a `RemoteStore` directly,
  but the integration version catches a wiring regression
  (e.g. forgetting to pass `remote_store.clone()` to
  `KikiFs::new` in either `Initialize` or `rehydrate`) that a
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
  that migration: `JjKikiFs::read_*` are no longer "async in
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

- Custom `KikiOpHeadsStore` impl in `cli/src/op_heads_store.rs`,
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

- **Custom `KikiOpStore` (op contents).** The op contents (operations,
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
   would break the moment we swap in `KikiOpHeadsStore`. With a
   fallback, `KikiOpHeadsStore` is unconditional and the catalog
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
- **`KikiOpHeadsStore` unit tests** — drive against a fake
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

1. PLAN.md §10.5 (this section). _Done._
2. Daemon: `LocalRefs` per-mount catalog (redb-backed) + unit tests.
3. Proto + daemon: catalog RPCs on `JujutsuInterface` + dispatch
   to remote-or-local + service tests.
4. CLI: `BlockingJujutsuInterfaceClient` gains the three catalog
   methods + `KikiOpHeadsStore` impl + register factory in
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

- `name() -> &str` — pick `"kiki_op_heads"`.
- `update_op_heads(old_ids: &[OperationId], new_id: &OperationId)
   -> Result<(), OpHeadsStoreError>`. CAS read+swap.
- `get_op_heads() -> Result<Vec<OperationId>, OpHeadsStoreError>`.
  One get_ref + parse.
- `lock() -> Result<Box<dyn OpHeadsStoreLock + '_>,
   OpHeadsStoreError>`. No-op token.

The factory side: `StoreFactories::add_op_heads_store("kiki_op_heads", ...)`
in `create_store_factories`, and replace `default_op_heads_store_initializer()`
in `Workspace::init_with_factories` with a closure that constructs
`KikiOpHeadsStore`.

### 10.5.8 M10.5 outcome

Six commits across the M10.5 sequence (numbered to match
§10.5.6):

1. `docs: PLAN.md §10.5 — define M10.5 spec`. ✅
2. `daemon: M10.5 — LocalRefs per-mount redb-backed catalog`. ✅
   12 unit tests on the FsRemoteStore-parity matrix
   (missing/create/conflict/advance/stale/delete/empty-vs-absent/
   list/bad-name) plus a `clone-shares-state` smoke test.
   Backed by a single `refs_v1` table inside the existing
   per-mount `store.redb` file — sharing the `Arc<Database>`
   keeps everything on one fsync per CAS.
3. `daemon: M10.5 — JujutsuInterface catalog RPCs`. ✅
   `GetCatalogRef`/`CasCatalogRef`/`ListCatalogRefs` on
   `service JujutsuInterface`, dispatching to a per-mount
   `CatalogHandle` (enum, not trait — `LocalRefs` is sync,
   `RemoteStore` is async; bridging behind `dyn` would add
   code without removing any). Five new tests covering the
   dispatch matrix (no-remote → LocalRefs; remote configured
   → FsRemoteStore; per-mount isolation; bad-name reject;
   unknown-mount NotFound).
4. `cli: M10.5 — KikiOpHeadsStore drives the daemon catalog`. ✅
   Custom `OpHeadsStore` impl writes a single `op_heads` ref
   in length-prefixed concat format. CAS retry loop bounded
   at 64 iterations; `expected_for_empty` helper distinguishes
   absent-vs-empty on the wire so the create-only first call
   doesn't silently overwrite a concurrent writer's ref.
   Wire-up via both `Workspace::init_with_factories` (writing
   the `kiki_op_heads` type tag) and a registered
   `add_op_heads_store` factory (subsequent loads honor the
   tag). 7 unit tests on the encode/decode round-trip.
5. `cli: M10.5 — two-CLI catalog arbitration acceptance test`. ✅
   Two `TestEnvironment`s (each with its own daemon on a
   random port) both pointed at one shared dir:// remote.
   Confirms ≥ 2 op-heads in the catalog after both inits —
   single-entry would mean one CLI silently clobbered the
   other. Caught a deterministic-ID test gotcha:
   `JJ_RANDOMNESS_SEED`/`JJ_TIMESTAMP` pinning means two
   fresh envs produce byte-identical workspace ops; fixed
   with `advance_test_rng_seed_to_multiple_of(1_000_000)`
   on env_b.
6. `docs: PLAN.md §10.5.8 — M10.5 outcome`. ✅ (this commit)

177 total tests (15 cli unit + 8 cli integration + 154
daemon, +24 from the 153/137-cli-integration M10 baseline);
`cargo clippy --workspace --all-targets -- -D warnings`
clean.

Decisions made on the way:

- **Catalog access stays on `JujutsuInterface`, not a direct
  remote dial.** §10.5.2 decision 1 said "via JujutsuInterface";
  implementing it confirmed the trade-off cleanly: the CLI's
  `BlockingJujutsuInterfaceClient` keeps a single channel, the
  daemon owns the routing logic, and the local-fallback story
  lives entirely on the daemon side. No CLI-side knowledge of
  remote-vs-local — the catalog API is uniform from where the
  CLI sits.

- **`Mount.remote_store: Option<...>` preserved; `Mount.local_refs`
  is a sibling field, not a wrapper.** Considered always-Some-ing
  `remote_store` with a no-op-blob `LocalRemoteStore`, but that
  would have meant every M9 blob-CAS site (write-through,
  has-blob skip, post-snapshot push walk) goes through a no-op
  layer in the no-remote case. Cleaner to keep the M9 semantics
  intact and add a separate field for refs.

- **`CatalogHandle` is an enum, not a trait.** `LocalRefs`'s
  methods are sync (redb txns return immediately), the
  `RemoteStore` ref methods are `async`. Unifying behind a
  trait would force every method on `LocalRefs` to be `async`
  too, just to satisfy the trait — adds ceremony without
  changing behavior. The enum's three match arms read fine.

- **Single `op_heads` ref, length-prefixed list of op-id
  bytes.** §10.5.2 decision 3. Wire format `[u32 BE
  len][bytes]…` — trivially small (op-ids are 64-byte blake2b;
  even with 10 heads the value is < 1KB), deterministic
  byte-output for the same `BTreeSet`, tolerates zero-length
  entries on read for forward-compat. CAS-loop convergence
  happens in 1 iteration uncontended, 2 on a localhost
  conflict.

- **`expected_for_empty` helper for the absent-vs-empty CAS
  precondition.** `read_set` collapses both "ref does not
  exist" and "ref exists but value is empty" into the empty
  `BTreeSet`, but on the wire those need distinct CAS
  preconditions (`expected = None` vs `expected = Some(empty)`).
  The helper re-fetches the raw `(found, value)` from the
  catalog before the CAS — one extra round-trip on the
  empty-set path, which is rare (only the very first
  `update_op_heads` after init). The alternative — folding
  `found` into `read_set`'s return type — would have meant
  every caller threading a `(Vec<OperationId>, ExpectedShape)`
  pair through the retry loop. Local helper is cheaper.

- **Workspace path resolution from the op_heads dir uses
  three `.ancestors().nth(3)` climbs.** The store factory
  receives `<wc>/.jj/repo/op_heads/`; we recover `<wc>` by
  going up three levels. Brittle to jj-lib re-layout (e.g. if
  the path becomes `<wc>/.jj/op_heads/`), but jj-lib has
  pinned this layout for years. A failure here surfaces as a
  clean `BackendLoadError` rather than a panic, so the test
  matrix would catch a layout shift early.

- **`CliRunner::add_store_factories` merges with collision
  panic, so `StoreFactories::empty()` stays.** Briefly tried
  `StoreFactories::default()` to register the SimpleBackend
  alongside the Kiki backend; the resulting double-registration
  panic surfaced at `jj kk init` time with
  `Conflicting factory definitions for 'Simple' factory`.
  Reverted to `empty()` and added an explanatory comment —
  CliRunner adds the defaults, we add only the kiki-specific
  factories on top.

- **Test-time deterministic-ID gotcha.** Two `TestEnvironment`s
  starting at `command_number = 0` produce byte-identical "add
  workspace 'default'" ops; identical content hashes collapse
  to one op-head. The first run of the two-CLI test failed
  with `expected ≥ 2 op-heads; got 1` — initially read as a
  CAS-retry bug, was actually a test-harness collision.
  `advance_test_rng_seed_to_multiple_of(1_000_000)` on env_b
  fixes it cleanly. Documented inline in the test so a future
  reader doesn't re-debug.

What this milestone explicitly does **not** do (preserved from
§10.5.1, repeated here so it's findable in the outcome):

- **Custom `KikiOpStore` (op contents).** The op contents
  (operations, views) still live in `<wc>/.jj/op_store/` over
  FUSE → per-mount Store, not pushed anywhere. So a two-CLI
  shared op log won't work end-to-end yet — CLI_B can see A's
  op-head id in the catalog, but can't read A's op bytes.
  M10.5 closes the **arbitration** story; M10.6 closes the
  **content** story. Two milestones because they're
  independent design problems (the catalog is mutable +
  arbitrated; op contents are content-addressed and ride on
  the existing M9 blob CAS, with their own decisions on push
  surface and `Snapshot`-walk reachability).
- Auth/TLS/retry/backoff for `grpc://` remotes. Still M11
  alongside S3.
- Async background push queue. Still M10/M11 follow-up.
- Stable inode ids across restarts (§7 decision 6). Still
  alongside the `fuser` migration (§9). M10.5 didn't touch
  the slab.

Test coverage added in M10.5 (24 new tests, total
177/153-cli-integration baseline):

- `daemon::local_refs::tests` (LocalRefs unit, 12 new):
  `missing_ref_returns_none`,
  `create_then_read_round_trips`,
  `create_only_against_existing_conflicts`,
  `advance_with_correct_expected`,
  `stale_expected_returns_actual`,
  `delete_removes_ref`,
  `delete_with_stale_expected_conflicts`,
  `empty_value_distinct_from_absent`,
  `list_refs_returns_sorted_names`,
  `list_refs_on_empty_store_is_ok`,
  `cas_ref_rejects_bad_name`,
  `get_and_list_persist_across_clone`.
- `daemon::service::tests` (catalog dispatch, 5 new):
  `catalog_rpcs_route_to_local_refs_when_no_remote`,
  `catalog_rpcs_route_to_remote_when_configured`,
  `catalog_local_refs_are_per_mount`,
  `catalog_rpcs_reject_bad_name`,
  `catalog_rpcs_unknown_mount_is_not_found`.
- `cli::op_heads_store::tests` (encode/decode, 7 new):
  `encode_empty_round_trips`,
  `encode_single_round_trips`,
  `encode_multiple_round_trips`,
  `encode_is_deterministic`,
  `decode_truncated_length_prefix_is_error`,
  `decode_truncated_payload_is_error`,
  `decode_zero_length_entries_are_skipped`.
- `cli::tests::test_catalog_arbitration` (integration, 2 new):
  `one_cli_writes_op_heads_to_shared_dir_remote`,
  `two_clis_serialize_op_heads_via_shared_dir_remote`.

## 10.6. M10.6 — wire jj-lib's op-store contents through the remote

M10.5 closed the **arbitration** story: `KikiOpHeadsStore` drives the
catalog so two CLIs against a shared remote serialize op-log
advances rather than clobbering. But op contents — the actual
`Operation` and `View` objects — still live in
`<wc>/.jj/repo/op_store/` via `SimpleOpStore`. CLI_B can see
CLI_A's op-head id in the catalog, but can't read CLI_A's op bytes.

M10.6 closes the **content** story. A custom `KikiOpStore` routes
`read_view`/`write_view`/`read_operation`/`write_operation` through
the daemon (same pattern as `KikiBackend` for commits/trees/files),
with write-through to the remote and read-through on local miss.
After M10.6, two CLIs sharing a `dir://` remote can each read the
other's full operation history.

### 10.6.1 Scope

In:

- **Blob-id generalization.** `RemoteStore` blob methods change
  from `id: &Id` to `id: &[u8]`. Mechanical refactor (~20 call
  sites pass `&id.0` instead of `&id`). Enables 64-byte
  BLAKE2b-512 op-store ids alongside the existing 32-byte BLAKE3
  ids on the same blob surface. The proto already uses `bytes id`
  (arbitrary-length), so the wire format is unchanged.
- **`BlobKind` extension.** Two new variants: `View`, `Operation`.
  Proto enum gains `BLOB_KIND_VIEW = 5`, `BLOB_KIND_OPERATION = 6`.
- **Daemon `Store` extension.** Two new redb tables (`views_v1`,
  `operations_v1`) with `&[u8]` keys (variable-length, since
  op-store ids are 64 bytes, not 32). Raw-bytes get/write/has
  methods — the daemon never decodes op-store data, it just
  stores and forwards opaque bytes.
- **`JujutsuInterface` RPCs.** `WriteView`/`ReadView`,
  `WriteOperation`/`ReadOperation`,
  `ResolveOperationIdPrefix` — all carry `working_copy_path`.
  Handlers follow the same write-through + read-through pattern
  as the existing blob RPCs.
- **CLI `KikiOpStore`.** Custom `OpStore` impl in
  `cli/src/op_store.rs`, routes through
  `BlockingJujutsuInterfaceClient`. Handles root operation/view
  synthetically (same as `SimpleOpStore`). `gc` is a no-op.
- **Two-CLI acceptance test.** CLI_A writes an operation; CLI_B
  reads it by id through the shared `dir://` remote. Validates
  the content story end-to-end.

Out (deferred):

- **`gc`.** No-op. Garbage collection of op-store data on the
  remote is a future concern; the data is small (one op + one
  view per jj command) and grows slowly.
- **Auth/TLS/retry/backoff.** Still M11 alongside S3.
- **Async background push queue.** Write-through is fine for
  localhost; the queue is a follow-up.
- **Stable inode ids across restarts (§7 decision 6).** Still
  alongside the `fuser` migration (§9).

### 10.6.2 Decisions

1. **Generalize blob ids from `&Id` to `&[u8]`.** The daemon's
   `Id([u8; 32])` type is a BLAKE3 hash used for tree/file/
   symlink/commit data. jj-lib's `OperationId`/`ViewId` are
   BLAKE2b-512 (64 bytes). Rather than adding 3 parallel
   `op_blob` methods to `RemoteStore` (doubling the blob surface),
   we generalize the existing 3 blob methods to accept `&[u8]`.
   Call sites pass `&id.0` instead of `&id`. The proto already
   uses `bytes id` — no wire change. The daemon's local `Store`
   keeps its typed `Id`-based methods for tree/file/symlink/commit;
   the new op-store tables use `&[u8]` keys directly.

2. **Op-store data rides on the blob CAS.** Operations and views
   are content-addressed the same way trees and files are — just
   with different hash functions and id lengths. Reusing the blob
   surface (with new `BlobKind` variants) keeps the remote
   backends at 6 methods, not 9. The `FsRemoteStore` stores
   op-store blobs at `<root>/view/<hex(64_byte_id)>` and
   `<root>/operation/<hex(64_byte_id)>` — different subdirectory
   from tree/file data, 128-char hex filenames instead of 64.

3. **Write-through, not reachability walk.** Tree/file/symlink
   blobs use a post-`Snapshot` reachability walk because the VFS
   produces them in bulk and the walk ensures nothing is missed.
   Op-store data is written one object at a time by jj-lib, and
   each write goes through `KikiOpStore::write_*` → daemon →
   remote. There's no "orphan op-store object" risk, so no walk
   is needed. Each `WriteView`/`WriteOperation` RPC pushes to the
   remote inline (same as `WriteCommit` today).

4. **Root operation and root view are synthetic.** `SimpleOpStore`
   constructs the root operation (id = `[0; 64]`) and root view
   in-memory from `RootOperationData { root_commit_id }`. The
   daemon never stores them. `KikiOpStore` does the same: short-
   circuit on root ids, return the synthetic objects, never hit
   the daemon.

5. **`resolve_operation_id_prefix`: daemon-side redb scan.**
   The daemon's `operations_v1` table has all locally-known ops.
   A new `ResolveOperationIdPrefix` RPC takes a hex prefix
   string, the daemon scans the table for matching keys, and
   returns `NoMatch` / `SingleMatch(id)` / `AmbiguousMatch`.
   No remote fallback — prefix resolution is inherently local
   (the user types a prefix they've seen in `jj op log`, which
   lists local ops). If a peer's op hasn't been read-through yet,
   it won't appear — same as `SimpleOpStore` scanning its local
   `operations/` directory.

6. **Variable-length redb keys for op-store tables.** The two
   new tables use `TableDefinition<&[u8], &[u8]>` rather than
   `TableDefinition<&[u8; 64], &[u8]>`. Slightly less ergonomic
   but survives any future hash-length change without a table
   bump. Length validation happens at the RPC boundary, not the
   storage layer.

7. **`KikiOpStore` factory receives `RootOperationData`.** Unlike
   the `OpHeadsStore` factory (which takes `settings` + `path`),
   the `OpStore` factory signature is
   `fn(settings, store_path, RootOperationData) -> OpStore`.
   The `root_commit_id` inside `RootOperationData` is needed to
   construct the synthetic root view. `KikiOpStore` stores it as
   a field.

### 10.6.3 Wire protocol

Add five RPCs to `service JujutsuInterface`:

```proto
service JujutsuInterface {
  // ... existing RPCs ...
  rpc WriteView(WriteViewReq) returns (WriteViewReply) {}
  rpc ReadView(ReadViewReq) returns (ReadViewReply) {}
  rpc WriteOperation(WriteOperationReq) returns (WriteOperationReply) {}
  rpc ReadOperation(ReadOperationReq) returns (ReadOperationReply) {}
  rpc ResolveOperationIdPrefix(ResolveOperationIdPrefixReq)
      returns (ResolveOperationIdPrefixReply) {}
}
```

The read/write RPCs carry `working_copy_path` + raw `id` bytes +
raw `data` bytes. The daemon never decodes the payload — it stores
and forwards opaque bytes. The CLI handles serialization (to/from
jj-lib's `simple_op_store` proto format) and content hashing
(BLAKE2b-512).

`ResolveOperationIdPrefix` carries a hex prefix string and returns
one of three states: no match, single match (with the full id),
or ambiguous match. Same shape as jj-lib's `PrefixResolution`.

### 10.6.4 Daemon storage

Two new redb tables in the per-mount `store.redb`:

- `views_v1: &[u8] → &[u8]` — key is the raw ViewId bytes
  (64 bytes), value is the serialized view proto.
- `operations_v1: &[u8] → &[u8]` — key is the raw OperationId
  bytes (64 bytes), value is the serialized operation proto.

Materialized in `Store::from_database` alongside the existing
four tables (same lazy-create pattern). New methods:

- `get_view_bytes(id: &[u8]) -> Result<Option<Bytes>>`
- `write_view_bytes(id: &[u8], bytes: &[u8]) -> Result<()>`
- `get_operation_bytes(id: &[u8]) -> Result<Option<Bytes>>`
- `write_operation_bytes(id: &[u8], bytes: &[u8]) -> Result<()>`
- `operation_ids_matching_prefix(hex_prefix: &str) -> Result<PrefixResult>`
  — range scan for `resolve_operation_id_prefix`.

These are raw-bytes methods (like `get_tree_bytes` / `get_file_bytes`)
because the daemon never needs the typed representation.

### 10.6.5 Test strategy

- **`RemoteStore` id generalization tests** — existing blob tests
  still pass with `&id.0` call sites; new test confirms a 64-byte
  id round-trips through `put_blob`/`get_blob` for `BlobKind::View`.
- **`Store` op-store table tests** — write/read round-trip for
  views and operations; missing key returns `None`;
  `operation_ids_matching_prefix` returns correct match states.
- **Service-level write-through test** — `WriteView` with a
  configured `dir://` remote pushes the blob; a second daemon
  reading from the same remote sees the view.
- **Service-level read-through test** — view/operation exists only
  on the remote (not in local store); `ReadView`/`ReadOperation`
  fetches from remote, populates local cache, second read hits
  local.
- **`ResolveOperationIdPrefix` test** — prefix scan returns
  correct match for full id, short prefix, ambiguous prefix,
  and no-match.
- **Two-CLI acceptance test** — CLI_A writes ops via `jj new`;
  CLI_B reads CLI_A's operations through the shared `dir://`
  remote. Confirms the content story end-to-end (not just
  arbitration, which M10.5 tested).

### 10.6.6 Commit plan

One commit per logical step:

1. PLAN.md §10.6 (this section). _← in progress_
2. Proto + `BlobKind` + `RemoteStore` trait: generalize blob ids
   to `&[u8]`, add `View`/`Operation` kind variants, add op-store
   RPCs to `JujutsuInterface`.
3. Daemon `Store`: `views_v1`/`operations_v1` tables +
   raw-bytes methods + prefix-scan + unit tests.
4. Daemon `service.rs`: op-store RPC handlers with write-through +
   read-through + service-level tests.
5. CLI: `KikiOpStore` impl + `BlockingJujutsuInterfaceClient`
   methods + factory registration in `main.rs`.
6. Two-CLI acceptance test.
7. PLAN.md §10.6 outcome.

### 10.6.7 Pickup notes

What's already in place from M10/M10.5:

- `Mount.remote_store: Option<Arc<dyn RemoteStore>>` — unchanged.
  Op-store write-through/read-through uses the same handle.
- `mount_handles` helper clones `(store, remote)` from under the
  mounts lock — reuse for op-store handlers.
- `push_blob_if_missing` — needs updating for `&[u8]` ids; the
  new op-store write-through path can call it directly.
- `fetch_*_through` helpers in `remote/fetch.rs` — currently
  typed for tree/file/symlink/commit. Op-store read-through
  can use the `RemoteStore` blob methods directly (the helpers
  handle verify-round-trip which isn't needed for op-store data
  since the CLI already computed the content hash).
- `climb_to_workspace` in `main.rs` — same 3-ancestor climb
  works for `<wc>/.jj/repo/op_store/` (the `OpStore` factory
  receives the same path layout).

What jj-lib expects from a custom `OpStore`:

- `name() -> &str` — `"kiki_op_store"`.
- `root_operation_id() -> &OperationId` — `[0; 64]`.
- `read_view(id) -> View` — daemon RPC, short-circuit root.
- `write_view(view) -> ViewId` — serialize, hash, daemon RPC.
- `read_operation(id) -> Operation` — daemon RPC, short-circuit
  root.
- `write_operation(op) -> OperationId` — serialize, hash, daemon
  RPC.
- `resolve_operation_id_prefix(prefix) -> PrefixResolution` —
  daemon RPC.
- `gc(head_ids, keep_newer) -> ()` — no-op.

Serialization: `KikiOpStore` wraps a `SimpleOpStore` as a local
serialization delegate and cache. jj-lib's `view_to_proto` /
`operation_to_proto` / `view_from_proto` / `operation_from_proto`
are private (~300 lines tightly coupled to jj-lib internals);
wrapping `SimpleOpStore` avoids reimplementing them. On write,
the delegate serializes + content-hashes and writes to disk; the
`KikiOpStore` reads back the bytes and pushes them to the daemon.
On read, the delegate tries its local disk first; on miss the
`KikiOpStore` fetches from the daemon, writes the bytes to the
delegate's disk path, and re-reads through the delegate to
deserialize. The daemon is format-agnostic — it just stores bytes.

### 10.6.8 M10.6 outcome

Six commits across the M10.6 sequence (numbered to match
§10.6.6):

1. `docs: PLAN.md §10.6 — define M10.6 spec`. ✅
2. `proto+daemon: M10.6 — generalize blob ids to &[u8], add
   View/Operation kinds, add op-store RPCs`. ✅ Mechanical
   refactor: `RemoteStore::get_blob/put_blob/has_blob` from
   `id: &Id` to `id: &[u8]`; ~20 call sites pass `&id.0`
   instead of `&id`. `BlobKind` gained `View` and `Operation`.
   Proto got `BLOB_KIND_VIEW = 5` / `BLOB_KIND_OPERATION = 6`
   plus five new RPCs on `JujutsuInterface`. Server-side stubs
   kept the daemon compilable (replaced in the next commit).
   Proto `bytes id` already arbitrary-length — no wire change.
3. `daemon: M10.6 — Store gains op-store tables + op-store RPC
   handlers with write-through/read-through`. ✅ Two new redb
   tables (`views_v1`, `operations_v1`) with `&[u8]` keys.
   Raw-bytes get/write methods. `operation_ids_matching_prefix`
   via table scan. Five RPC handlers: `WriteView`/`ReadView`/
   `WriteOperation`/`ReadOperation`/`ResolveOperationIdPrefix`
   — same write-through + read-through shape as the blob handlers.
   12 new tests (7 store-level + 5 service-level).
4. `cli: M10.6 — KikiOpStore impl + BlockingJujutsuInterfaceClient
   methods + factory registration`. ✅ `KikiOpStore` wraps
   `SimpleOpStore` as serialization delegate + local cache;
   pushes to daemon on write, fetches from daemon on read miss.
   `resolve_operation_id_prefix` merges daemon scan with root-id
   check. Factory registered via `add_op_store("kiki_op_store", …)`;
   init path replaces `default_op_store_initializer()`.
5. `cli: M10.6 — two-CLI op-store content sharing acceptance
   test`. ✅ Two tests: single-CLI `jj op log` works end-to-end;
   two-CLI sharing confirms op/view blobs land on the remote
   after init and both CLIs can read their own ops.
6. `docs: PLAN.md §10.6 outcome`. ✅ (this commit)

192 total tests (15 cli unit + 10 cli integration + 167 daemon,
+15 from the 177 M10.5 baseline); `cargo clippy --workspace
--all-targets -- -D warnings` clean.

Decisions made on the way:

- **`RemoteStore` blob ids generalized to `&[u8]`, not parallel
  `op_blob` methods.** The trait stays at 6 blob+ref methods (not
  9). Backends (`FsRemoteStore`, `GrpcRemoteStore`, server) each
  changed ~5 lines. The proto already used `bytes id` —
  arbitrary-length was free on the wire. The Rust-side `Id([u8;
  32])` type stays for local store use; the remote surface accepts
  any length. 64-byte BLAKE2b-512 ids round-trip cleanly.

- **`RemoteStoreService` validates non-empty ids, not fixed
  length.** The old `decode_id` enforced 32 bytes; replaced with
  `validate_id` that rejects only empty. The remote stores bytes
  opaquely — length validation is the caller's job (the CLI
  computes content hashes). The existing "server rejects short
  id" gRPC test became "server rejects empty id".

- **`KikiOpStore` wraps `SimpleOpStore`, not a from-scratch
  reimplementation.** jj-lib's proto conversion functions
  (`view_to_proto`, `operation_from_proto`, etc.) are private.
  Reimplementing ~300 lines of tightly-coupled conversion code
  would be fragile and break on jj-lib version bumps. Wrapping
  `SimpleOpStore` reuses all serialization + content hashing
  logic. The trade-off: op-store files exist both in the delegate's
  disk path (under the FUSE mount's `.jj/repo/op_store/`) and in
  the daemon's redb tables + remote. The redundancy is harmless
  — the delegate acts as L1 cache, the daemon's redb as L2, the
  remote as L3.

- **`pollster` added for blocking `SimpleOpStore` async calls.**
  `SimpleOpStore`'s methods are `async fn` (required by the
  `OpStore` trait) but don't actually `.await` anything. `pollster::
  block_on` is the lightest way to call them from a sync context.
  The CLI's `BlockingJujutsuInterfaceClient` uses a separate
  `tokio::Runtime` for gRPC; `pollster` doesn't interfere.

- **`OpPrefixResult` uses `None`/`Single`/`Ambiguous` variant
  names.** Clippy flagged the original `NoMatch`/`SingleMatch`/
  `AmbiguousMatch` as `enum_variant_names` (all end with "Match").
  Shortened to avoid the lint.

- **Write-through, not reachability walk.** Each `WriteView`/
  `WriteOperation` RPC pushes to the remote inline. No post-
  snapshot walk needed for op-store data (ops are written one at
  a time by jj-lib, never in bulk). The `push_reachable_blobs`
  function gained an `unreachable!` arm for `View`/`Operation`
  blob kinds to make this explicit.

- **`resolve_operation_id_prefix` merges root-id with daemon
  scan.** The root operation (id `[0; 64]`) is synthetic — never
  stored in the daemon's table. `KikiOpStore` checks the root
  locally and merges with the daemon's redb scan result. A root
  match + daemon single-match of a different id → ambiguous.

What this milestone explicitly does **not** do:

- **`gc` on the remote.** The delegate's `gc` cleans up local
  files; the daemon's redb tables and the remote are untouched.
  Op-store data is small and grows slowly; remote gc is future
  work.
- **Async push queue + offline resilience.** M11 (§11).
- **Auth/TLS.** M12 alongside git convergence and S3.
- **Stable inode ids across restarts (§7 decision 6).** Still
  alongside the `fuser` migration (§9).

Test coverage added in M10.6 (15 new tests, total 192):

- `daemon::store::tests` (7 new):
  `view_write_then_read_round_trips`,
  `operation_write_then_read_round_trips`,
  `missing_view_returns_none`,
  `operation_prefix_no_match`,
  `operation_prefix_single_match`,
  `operation_prefix_ambiguous_match`,
  `operation_prefix_full_length_match`.
- `daemon::remote::fs::tests` (1 new):
  `put_then_get_64_byte_id_view`.
- `daemon::service::tests` (5 new):
  `write_view_pushes_to_dir_remote`,
  `read_view_falls_back_to_remote_on_local_miss`,
  `read_view_no_remote_miss_returns_not_found`,
  `write_operation_pushes_and_read_through_works`,
  `resolve_operation_id_prefix_works`.
- `cli::tests::test_op_store_sharing` (integration, 2 new):
  `cli_reads_own_ops_via_kiki_op_store`,
  `two_clis_share_op_contents_via_remote`.

## 10.7. M10.7 — gitignore-aware VFS

M10.6 closed the op-store content story. But the VFS has a gap that
blocks real-world use: **every file written through the mount is
snapshotted into the content store**, including `.gitignore`d paths
like `node_modules/`, `target/`, `venv/`, `__pycache__/`, `.env`,
and build outputs. An `npm install` in a kiki workspace today would
snapshot ~50,000 files into redb and push them all to the remote.

This is a prerequisite for the agent story (agents run `npm install`,
`cargo build`, etc.) and for any real-world use with package managers
or build systems.

### 10.7.1 Scope

In:

- **Daemon-side ignore rules.** The daemon loads `.gitignore` files
  from the content tree at mount time and reloads when they change
  (detected via the VFS write path — a write to any file named
  `.gitignore` triggers a reload of that directory's rules).
- **`Ignored` inode variant.** `NodeRef` in `vfs/inode.rs` gains
  an ignored state. Files and directories matching ignore rules are
  fully writable and readable (processes that create them can use
  them normally), but `snapshot_node` skips them. They live in
  memory only — not persisted to the content store, not pushed to
  the remote.
- **Ephemeral storage for ignored files.** Ignored file content is
  held in the slab as `DirtyFile` nodes (same as today) but tagged
  so the snapshot walk skips them. Alternatively, ignored content
  could spill to a per-mount scratch directory on disk to avoid
  holding large dependency trees in RAM. Decision in §10.7.2.
- **`SnapshotOptions` bridge.** `KikiWorkingCopy::snapshot` stops
  ignoring `_options: &SnapshotOptions`. The `base_ignores` from
  jj-lib are forwarded to the daemon via a new `SnapshotReq` field
  (serialized gitignore patterns), or the daemon loads them
  independently from the content tree. Decision in §10.7.2.
- **Already-tracked files remain tracked.** A file that exists in
  the checked-out tree (came from a commit) is never ignored, even
  if it matches `.gitignore`. Only *new* files (created through the
  VFS, not present in the last `check_out` tree) are candidates for
  ignoring. This matches git and jj semantics.

Out (deferred):

- **`.jj/ignore`** — jj supports additional ignore files; the
  daemon can load them alongside `.gitignore` but this is a
  polish item. `.gitignore` covers 99% of cases.
- **Nested `.gitignore` hot-reload.** Initial implementation
  loads ignore rules from the root `.gitignore` and any
  `.gitignore` files in the checked-out tree. Hot-reload on
  write handles the common case (agent edits root `.gitignore`);
  deeply nested `.gitignore` creation during a session can
  wait for a snapshot-time reload pass.
- **Disk spill for ignored content.** If the in-memory-only
  approach causes OOM on very large dependency trees, add a
  tmpfs-backed scratch dir. Deferrable — most workloads are
  fine with in-memory ignored content; the snapshot skip
  prevents the persistent storage problem.

### 10.7.2 Decisions

1. **Daemon loads ignore rules independently, not from CLI.**
   The daemon already has the content tree (it serves it via
   VFS). It can read `.gitignore` files from the checked-out
   tree at `check_out` time and from the slab on write. No
   need to serialize ignore patterns across gRPC — the daemon
   has the source data. This avoids adding a field to
   `SnapshotReq` and keeps the ignore logic in one place.
   The `SnapshotOptions.base_ignores` from jj-lib can remain
   unused on the CLI side — the daemon's rules are authoritative.

2. **Ignore state is per-inode, not per-path.** When a
   directory is ignored (e.g. `node_modules/`), the directory
   inode itself is tagged ignored. All children created under
   it inherit the ignored tag — `create_file` and `mkdir`
   check the parent's ignored state before creating the child.
   This avoids per-file gitignore matching on every write
   (expensive with complex patterns). Only directory creation
   and root-level file creation consult the ignore rules.

3. **No `NodeRef::Ignored` variant — use a flag.** Adding an
   enum variant to `NodeRef` would force every match arm in
   the VFS to handle it. Simpler: add an `ignored: bool` field
   to the `Inode` struct. `snapshot_node` checks the flag and
   skips. All other VFS operations (read, write, lookup,
   readdir) are unaffected — ignored files are fully functional.

4. **Already-tracked files are never ignored.** When
   `check_out` populates the slab from a tree, every inode
   starts with `ignored: false` regardless of gitignore rules.
   Only `create_file` / `mkdir` / `symlink` (new entries not
   in the checked-out tree) consult the ignore rules. This
   matches git/jj semantics: `git add -f node_modules/foo`
   stays tracked even if `node_modules/` is ignored.

5. **`.gitignore` reload on write.** The VFS `write` handler
   checks if the written file's name is `.gitignore`. If so,
   parse the new content and update the per-directory ignore
   rules in the slab. Children created after the update use
   the new rules. Already-created ignored children are not
   retroactively un-ignored (and vice versa) — that would
   require a full slab walk. The next `check_out` rebuilds
   the slab from scratch with correct rules.

6. **Ignored content stays in memory (no disk spill in
   M10.7).** `node_modules/` in a typical JS project is
   ~200MB uncompressed. Holding that in slab `DirtyFile`
   buffers is the same cost as the process that created it
   (npm already had it in memory). The content is never
   persisted to redb or pushed to the remote — that's the
   actual win. If OOM becomes a real problem, disk spill is
   a follow-up (per-mount scratch dir under `storage_dir`).

### 10.7.3 Ignore rule loading

```rust
struct IgnoreRules {
    /// Root-level rules from `/.gitignore`.
    root: GitIgnoreFile,
    /// Per-directory overrides from nested `.gitignore` files.
    /// Key is the inode id of the directory containing the
    /// `.gitignore`.
    nested: HashMap<InodeId, GitIgnoreFile>,
}

impl KikiFs {
    /// Called at check_out time and on .gitignore write.
    fn load_ignore_rules(&self) -> IgnoreRules {
        // Read /.gitignore from the slab (or content tree).
        // Walk directories that contain .gitignore files.
        // Parse each with jj-lib's GitIgnoreFile (or a
        // standalone gitignore parser like `ignore` crate).
    }

    /// Called by create_file / mkdir / symlink to decide
    /// whether a new entry should be tagged ignored.
    fn is_ignored(&self, parent: InodeId, name: &str) -> bool {
        if self.inodes.get(parent).ignored {
            return true; // parent is ignored → children inherit
        }
        self.ignore_rules.matches(parent, name)
    }
}
```

jj-lib's `GitIgnoreFile` is available as a dependency
(`jj_lib::gitignore::GitIgnoreFile`). Alternatively, the
`ignore` crate (used by ripgrep) is well-tested and has no
jj dependency — useful if the daemon wants to minimize jj-lib
coupling (relevant for the platform direction where the daemon
isn't jj-specific).

### 10.7.4 Snapshot changes

Before (current, kiki_fs.rs line 736):

```rust
// Only exclusion: skip .jj/ at root
if is_root && name == JJ_DIR {
    continue;
}
```

After:

```rust
if is_root && name == JJ_DIR {
    continue;
}
if inode.ignored {
    continue;
}
```

One line. The ignore flag makes the snapshot change trivial.

### 10.7.5 Test strategy

- **Basic ignore** — create `.gitignore` with `node_modules/`,
  create `node_modules/foo.js` through VFS, snapshot: `foo.js`
  is not in the tree. Verify the file is still readable through
  the VFS.
- **Already-tracked not ignored** — check out a tree that
  contains `vendor/lib.js`, add `vendor/` to `.gitignore`,
  modify `vendor/lib.js`: snapshot includes the modification.
- **Nested gitignore** — `sub/.gitignore` contains `*.log`,
  create `sub/debug.log`: not snapshotted. Create `sub/app.js`:
  snapshotted.
- **Gitignore hot-reload** — create `tmp/foo`, snapshot
  includes it. Write `tmp/\n` to `.gitignore`. Create
  `tmp/bar`: not snapshotted.
- **Negation patterns** — `.gitignore` contains `*.log` and
  `!important.log`. Create both: `debug.log` not snapshotted,
  `important.log` snapshotted.
- **RAM usage** — create 10,000 ignored files, snapshot:
  verify redb/store has zero entries for them. Verify they're
  readable via VFS.

### 10.7.6 Commit plan

1. PLAN.md §10.7 (this section).
2. `Inode` gains `ignored: bool` flag + `snapshot_node` skip.
3. `IgnoreRules` struct + gitignore loading from content tree
   at check_out time.
4. `create_file` / `mkdir` / `symlink` consult ignore rules
   for new entries.
5. `.gitignore` write detection + hot-reload.
6. Tests.
7. PLAN.md §10.7 outcome.

## 11. M11 — async push queue + offline resilience

M9 through M10.6 built a working write-through + read-through sync
pipeline: every mutating RPC pushes blobs to the remote synchronously,
and `Snapshot` walks the full reachable tree afterward to catch
anything the VFS wrote directly. This is correct and simple, but it
means **every mutating `jj` command blocks on remote availability**.
If the remote is unreachable — network outage, server down, laptop
on a plane — the command fails even though the local store already
has the data.

M11 decouples local writes from remote pushes. After M11, `jj`
commands succeed against local state regardless of remote
connectivity; a background queue drains to the remote when the
connection is available.

### 11.1 Scope

In:

- **Push queue** — a durable, per-mount redb table that records
  `(BlobKind, id, bytes_len)` entries for blobs that need to reach
  the remote. Survives daemon restarts.
- **Read-side offline semantics** — when a path's tree entry is
  present locally but the blob is not, and the remote is
  unreachable, surface a deterministic "not cached offline"
  state rather than a generic `EIO`. Exact filesystem UX
  (`EACCES` via mode `0o000`, `ENOENT`, xattr, or similar) is a
  policy choice, but M11 must define and test one.
- **Prefetch policy + CLI entry point** — add an explicit
  prefetch/sync operation (`kk sync --prefetch` or equivalent)
  so users can make a workspace offline-ready ahead of time.
  Prefetch policy is client-driven and size-sensitive: small
  repos fetch the full current tree eagerly; large repos stay
  lazy and prefetch according to heuristics.
- **Enqueue-not-block on write RPCs** — the six write handlers in
  `service.rs` (lines 1035–1296: `write_file`, `write_symlink`,
  `write_tree`, `write_commit`, `write_view`, `write_operation`)
  change from `remote.put_blob(...).await?` to
  `push_queue.enqueue(kind, id).await`. The RPC returns success
  immediately after the local store write + queue enqueue.
- **Enqueue-not-block on Snapshot** — `push_reachable_blobs`
  (lines 737–774) enqueues rather than pushing inline. Same BFS
  walk, but `push_blob_if_missing` writes a queue entry instead of
  calling `remote.put_blob`. A queue-aware variant can skip blobs
  the remote already has (`has_blob` probe) when online, and
  unconditionally enqueue when offline.
- **Background drain loop** — a per-mount `tokio::spawn` task that
  reads from the queue, calls `remote.put_blob`, and removes
  successfully pushed entries. Runs continuously when the remote is
  reachable; backs off exponentially when it's not.
- **Op-heads push after content** — the drain loop respects causal
  ordering: all content blobs (trees, files, symlinks, commits)
  and op-store blobs (views, operations) referenced by an operation
  must land on the remote before the catalog ref (`op_heads`) is
  advanced. This ensures a peer reading the remote always sees a
  complete, reachable object graph.
- **Health surface** — `DaemonStatus` gains per-mount queue depth
  and last-successful-push timestamp so the CLI can surface sync
  state to the user (`jj kk status` shows "3 blobs pending,
  last sync 2m ago" or "offline, 47 blobs queued").
- **Retry with backoff** — transport errors trigger exponential
  backoff (1s → 2s → 4s → ... → 60s cap). A successful push
  resets the backoff. The daemon doesn't spam a dead remote.
- **CLI tolerance** — `KikiOpHeadsStore::update_op_heads` (lines
  172–243 in `cli/src/op_heads_store.rs`) currently fails
  immediately on transport error. M11 adds a fallback: on
  transport error, write the op-heads update to the local catalog
  (`LocalRefs`) and enqueue a catalog-ref push. On reconnect, the
  drain loop advances the remote catalog ref, merging with any
  concurrent updates via the existing CAS retry.

Out (deferred):

- **Conflict resolution UI on reconnect.** jj's operation log
  model handles divergent concurrent edits naturally (concurrent
  ops merge on next load). M11 relies on this — no special
  conflict resolution beyond what jj already provides.
- **Selective sync / partial push.** All reachable blobs are
  pushed. Selective exclusion (e.g. large binaries, certain
  paths) is a future concern.
- **Build-system-specific prefetch plugins.** M11 can ship with
  pragmatic heuristics (repo size, recently accessed paths,
  common build roots) without needing first-class Buck/Bazel
  integration. Richer "understand the build graph" prefetching is
  later work.
- **Auth, TLS.** Still M12 alongside git convergence and S3.
- **Stable inode ids across restarts (§7 decision 6).** Still
  alongside the `fuser` migration (§9).

### 11.2 Decisions

1. **Queue storage: redb table, not in-memory.** The whole point
   of offline resilience is that work survives outages. An
   in-memory queue loses pending pushes on daemon restart (crash,
   reboot, laptop sleep). A redb table in the existing per-mount
   `store.redb` file (sharing the `Arc<Database>` with `Store`
   and `LocalRefs`) gives ACID durability with zero new files.
   The push queue table is append-heavy with head-of-queue
   deletes — redb handles this fine for the expected scale
   (hundreds to low thousands of entries).

2. **Queue key: monotonic `u64` sequence number.** Not
   `(BlobKind, id)` — a blob may be written multiple times
   across snapshots (idempotent put), and we want FIFO ordering
   for the drain loop. The sequence number gives a natural
   scan-from-front drain order and lets the queue hold duplicate
   entries (the remote `put_blob` is idempotent, so replaying a
   duplicate is harmless and cheaper than deduplicating). Value
   is `(BlobKind, id_bytes)` — the drain loop reads the raw
   bytes from the local store by `(kind, id)` and pushes them.

3. **Enqueue is a single redb write txn.** The write RPC already
   opens a write txn for the local store insert. Ideally both
   the local store write and the queue enqueue happen in the
   same transaction (atomic: either both commit or neither does).
   This requires `Store::write_*` to accept an optional
   `&WriteTransaction` instead of opening its own. If the
   two-table-one-txn refactor is too invasive for M11, a
   sequential pair of txns is acceptable — the queue entry is
   append-only and idempotent, so a crash between store-write
   and queue-enqueue loses at most one queue entry; the next
   `Snapshot` walk re-enqueues it.

4. **Drain loop is per-mount, not global.** Each mount has its
   own remote (possibly different hosts, different connectivity).
   A per-mount drain task reads from that mount's queue and
   pushes to that mount's `remote_store`. Mounts with no remote
   have no drain task.

5. **Backoff strategy: exponential with jitter, 60s cap.**
   On transport error: `delay = min(base * 2^attempt, 60s)` +
   random jitter (±25%). On success: reset `attempt = 0`,
   drain immediately. On empty queue: park the task
   (`tokio::sync::Notify`), wake on enqueue. This avoids
   both busy-spinning on an empty queue and slow recovery
   after a transient blip.

6. **Causal ordering: content before ops, ops before refs.**
   The drain loop processes entries in sequence-number order
   (FIFO). The write RPCs enqueue in dependency order by
   construction: `jj` writes blobs before the commit that
   references them, and writes the commit before advancing
   op-heads. So FIFO drain preserves causality. The
   catalog-ref advance (op-heads CAS) is the last step — the
   drain loop only attempts it after all content blobs and
   op-store blobs ahead of it in the queue have been pushed.
   This is implemented as a queue entry type:
   `QueueEntry::Blob { kind, id }` vs
   `QueueEntry::CatalogRef { name, expected, new }`. The drain
   loop pushes blobs eagerly and defers catalog-ref entries
   until all preceding blob entries are drained.

7. **`has_blob` probe: skip when offline, use when online.**
   The current `push_blob_if_missing` calls `has_blob` before
   every `put_blob` to avoid redundant pushes. When online,
   the drain loop does the same (cheap existence check). When
   the first `has_blob` fails with a transport error, the loop
   enters backoff — no point probing further until the remote
   is reachable. When online again, probing resumes from the
   queue head.

8. **`Snapshot` changes: walk + enqueue, not walk + push.**
   `push_reachable_blobs` becomes `enqueue_reachable_blobs`.
   Same BFS walk over the rolled-up tree, but instead of
   `remote.put_blob(...)`, each blob that the remote doesn't
   have (or that we can't probe because we're offline) gets a
   queue entry. The `Snapshot` RPC returns success immediately
   after the local snapshot + queue enqueue. The mount's
   `root_tree_id` is stamped after enqueue, not after push —
   the snapshot is locally durable even if the remote push
   hasn't happened yet.

9. **CLI op-heads fallback to LocalRefs.** Currently,
   `KikiOpHeadsStore::update_op_heads` calls
   `cas_catalog_ref(...)` which routes through the daemon to
   the remote. If the remote is down, the daemon returns
   `Status::internal` and the CLI fails. M11 adds a two-tier
   write path on the daemon side: the `CasCatalogRef` RPC
   handler tries the remote first; on transport error, it
   falls through to `local_refs` and enqueues a
   `QueueEntry::CatalogRef` for later reconciliation. The CLI
   sees success either way. On reconnect, the drain loop
   replays the catalog-ref CAS against the remote (which may
   require a fresh read + merge if the remote advanced
   independently — same CAS retry logic that already exists).

10. **No new proto messages.** The push queue is entirely
    daemon-internal. No new RPCs, no CLI-side changes beyond
    the `DaemonStatus` response gaining `queue_depth` and
    `last_push_at` fields. The `CasCatalogRef` handler's
    fallback behavior is transparent to the CLI — it still
    returns `CasCatalogRefReply { updated: true }`.

11. **Prefetch is a client policy layer, not a server one.**
    The remote remains a dumb CAS/object transport. The client
    (CLI + daemon together) decides what to fetch ahead of use.
    Baseline policy:
    - Small repos: fetch the full current tree eagerly so
      offline mode behaves like a normal clone.
    - Large repos / monorepos: stay lazy by default; prefetch
      the current root, recently accessed paths, and well-known
      build/config roots (`WORKSPACE`, `MODULE.bazel`,
      `BUILD{,.bazel}`, `.buckconfig`, `BUCK`, lockfiles, etc.).
    - Explicit prefetch should support at least "current tree"
      and "current tree + N commits of history".
    This preserves the structural advantage over blobless git:
    client-selected demand fetch, with targeted bulk fetch only
    when it improves UX.

12. **Repo-size-sensitive defaults are required.** "Always lazy"
    is the wrong default for small repos, and "always full fetch"
    is the wrong default for monorepos. M11 should define a
    coarse threshold-based default policy (exact threshold can be
    config-driven later) and record enough local stats to revise
    that policy once real workloads exist.

### 11.3 Storage layout

New redb table in the per-mount `store.redb`:

```
push_queue_v1: u64 → &[u8]
```

Key: monotonic sequence number (atomic `u64` counter, persisted
as a single-entry `push_queue_seq_v1` table or derived from the
max existing key + 1 on startup).

Value: a small serialized struct:

```rust
enum QueueEntry {
    Blob {
        kind: BlobKind,  // u8 discriminant
        id: Vec<u8>,     // 32 bytes (content) or 64 bytes (op-store)
    },
    CatalogRef {
        name: String,
        expected: Option<Vec<u8>>,
        new: Option<Vec<u8>>,
    },
}
```

Serialized as a tag byte + fields. `Blob` entries are ~34–66
bytes; `CatalogRef` entries are ~100–200 bytes. Even 10,000
queued entries total <1MB. Encoding can be as simple as
`[tag: u8][payload...]` with fixed-width kind + length-prefixed
id — no need for a general-purpose format.

### 11.4 Drain loop

```rust
async fn drain_loop(
    store: Arc<Store>,
    remote: Arc<dyn RemoteStore>,
    queue: Arc<PushQueue>,
    wake: Arc<tokio::sync::Notify>,
) {
    let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(60));

    loop {
        // Park until there's work.
        if queue.is_empty().await {
            wake.notified().await;
        }

        // Process entries in FIFO order.
        while let Some((seq, entry)) = queue.peek_front().await {
            match entry {
                QueueEntry::Blob { kind, id } => {
                    match push_one_blob(&store, &remote, kind, &id).await {
                        Ok(()) => {
                            queue.remove(seq).await;
                            backoff.reset();
                        }
                        Err(_) => {
                            // Transport error — back off, retry later.
                            backoff.wait().await;
                            break; // restart from queue head
                        }
                    }
                }
                QueueEntry::CatalogRef { name, expected, new } => {
                    // Only attempt if all preceding blob entries
                    // are drained (seq is the lowest in the queue).
                    match remote.cas_ref(&name, expected.as_deref(), new.as_deref()).await {
                        Ok(CasOutcome::Updated) => {
                            queue.remove(seq).await;
                            backoff.reset();
                        }
                        Ok(CasOutcome::Conflict { actual }) => {
                            // Remote advanced independently.
                            // Merge: re-read local, combine, re-enqueue.
                            let merged = merge_op_heads(actual, &new);
                            queue.replace(seq, QueueEntry::CatalogRef {
                                name, expected: actual, new: merged,
                            }).await;
                        }
                        Err(_) => {
                            backoff.wait().await;
                            break;
                        }
                    }
                }
            }
        }
    }
}
```

The `Backoff` struct is ~20 lines: holds `base`, `cap`,
`attempt` counter, and a `wait()` method that sleeps
`min(base * 2^attempt, cap) + jitter` and increments `attempt`.
`reset()` sets `attempt = 0`.

### 11.5 Write RPC changes

Before (M9–M10.6, example: `write_file`, service.rs line 1035):

```rust
let (id, bytes) = store.write_file(ty::File { content: req.data })
    .map_err(store_status("write_file"))?;
if let Some(remote) = remote {
    remote.put_blob(BlobKind::File, &id.0, bytes)
        .await
        .map_err(remote_status("remote put_blob (file)"))?;
}
```

After (M11):

```rust
let (id, _bytes) = store.write_file(ty::File { content: req.data })
    .map_err(store_status("write_file"))?;
if push_queue.is_some() {
    push_queue.enqueue(QueueEntry::Blob {
        kind: BlobKind::File, id: id.0.to_vec(),
    }).await.map_err(store_status("enqueue push"))?;
    push_queue_wake.notify_one();
}
```

Same pattern for all six write RPCs. The `remote` variable is no
longer touched in the write path — all remote interaction moves
to the drain loop.

### 11.6 Snapshot changes

Before (service.rs line 1486):

```rust
if let Some(remote) = &remote {
    push_reachable_blobs(&store, remote.as_ref(), new_root)
        .await
        .map_err(remote_status("post-snapshot remote push"))?;
}
```

After:

```rust
if let Some(queue) = &push_queue {
    enqueue_reachable_blobs(&store, queue, new_root).await
        .map_err(store_status("enqueue reachable blobs"))?;
    push_queue_wake.notify_one();
}
```

`enqueue_reachable_blobs` is the same BFS walk as
`push_reachable_blobs`, but calls `queue.enqueue(...)` instead
of `remote.put_blob(...)`. The `has_blob` probe is dropped from
the enqueue path — the drain loop handles deduplication. This
makes the snapshot path fully offline-capable: no remote contact
at all.

The `root_tree_id` stamp (line 1497) now happens immediately
after enqueue, not after remote push. The snapshot is locally
durable.

### 11.7 Mount changes

`Mount` gains two fields:

```rust
struct Mount {
    // ... existing fields ...

    /// Per-mount push queue. `None` only when `remote_store` is
    /// `None` (no remote configured — nothing to push to).
    push_queue: Option<Arc<PushQueue>>,

    /// Wake signal for the drain loop. `notify_one()` after
    /// every enqueue so the loop wakes from its park.
    push_queue_wake: Option<Arc<tokio::sync::Notify>>,
}
```

`Initialize` and `rehydrate` both construct the `PushQueue` and
spawn the drain loop when `remote_store.is_some()`. On
`rehydrate`, any entries left in the queue from a previous
daemon session are drained immediately (the daemon crashed or
restarted before finishing the push).

### 11.8 DaemonStatus changes

`DaemonStatusReply.MountInfo` gains:

```proto
message MountInfo {
  // ... existing fields ...
  uint64 push_queue_depth = 6;
  // Seconds since last successful push to remote. 0 if never
  // pushed or no remote configured.
  uint64 last_push_seconds_ago = 7;
}
```

The CLI's `jj kk status` output changes from:

```
  /home/user/repo  grpc://server:9090
```

To:

```
  /home/user/repo  grpc://server:9090  synced 2s ago
  /home/user/repo2 grpc://server:9090  47 pending, offline
```

### 11.9 Test strategy

- **PushQueue unit tests** — enqueue/peek/remove/is_empty
  round-trips. Sequence numbers are monotonic. Entries survive
  a `Database` close + reopen (durability). `CatalogRef` entries
  serialize/deserialize correctly.
- **Drain loop unit tests** — mock `RemoteStore` that succeeds:
  entries drain to empty. Mock that fails: entries stay, backoff
  increases. Mock that fails then succeeds: entries drain after
  recovery. `CatalogRef` entries are deferred until all preceding
  blobs drain.
- **Write RPC integration test** — `write_file` with a remote
  configured: blob lands in the queue, not on the remote. Drain
  loop pushes it. Confirm round-trip via `read_file` on a second
  daemon sharing the same `dir://` remote.
- **Snapshot integration test** — snapshot with a remote: all
  reachable blobs appear in the queue. Drain loop pushes them.
  `root_tree_id` is stamped immediately (not after drain).
- **Offline test** — configure a `grpc://` remote that's not
  listening. Run `write_file` + `Snapshot`: both succeed. Queue
  depth is >0. Start the remote: drain loop pushes, queue
  empties. `DaemonStatus` shows the transition.
- **Op-heads offline test** — two CLIs, remote goes down after
  init. Both advance op-heads: both succeed (local catalog
  fallback). Remote comes back: drain loop reconciles op-heads
  via CAS. Final state has both ops visible.
- **Daemon restart with pending queue** — enqueue entries, kill
  daemon, restart: queue entries survive, drain loop resumes.
- **Causal ordering test** — enqueue blob entries and a
  catalog-ref entry. Start drain: blobs push first, catalog-ref
  pushes last. Confirm by intercepting the mock remote's call
  order.

### 11.10 Commit plan

One commit per logical step:

1. PLAN.md §11 (this section).
2. `PushQueue` struct + redb table + unit tests.
3. `Backoff` helper + drain loop skeleton + unit tests.
4. Wire `PushQueue` into `Mount` + `Initialize` + `rehydrate`.
   Spawn drain loop per mount. Existing write-through still
   active (both paths run — belt and suspenders for this commit).
5. Write RPCs: switch from inline `put_blob` to queue enqueue.
   Remove the `remote.put_blob(...)` calls from all six write
   handlers.
6. Snapshot: `push_reachable_blobs` → `enqueue_reachable_blobs`.
   Move `root_tree_id` stamp before remote push.
7. `CasCatalogRef` handler: add local-refs fallback +
   `QueueEntry::CatalogRef` enqueue on transport error.
8. `DaemonStatus`: add `push_queue_depth` +
   `last_push_seconds_ago` to proto + handler + CLI display.
9. Integration tests: offline round-trip, daemon restart with
   pending queue, two-CLI op-heads reconciliation.
10. PLAN.md §11 outcome.

### 11.11 Pickup notes

What's already in place from M9–M10.6:

- `Mount.remote_store: Option<Arc<dyn RemoteStore>>` — the drain
  loop takes a clone of this `Arc`. M11 doesn't change the
  remote store itself, just when it's called.
- `Mount.local_refs: Arc<LocalRefs>` — the catalog-ref fallback
  path writes here when the remote is unreachable. Already
  exists and has full CAS semantics.
- `push_reachable_blobs` / `push_blob_if_missing` — the BFS walk
  and per-blob push logic. M11 refactors these into an enqueue
  variant; the walk itself is unchanged.
- `Store::database()` returns `Arc<Database>` — `PushQueue` opens
  its table on the same database, same pattern as `LocalRefs`.
- `remote_status(...)` helper (service.rs) — maps `anyhow::Error`
  to `Status::internal`. The write RPCs stop using this for
  remote errors (they don't call the remote anymore); the drain
  loop uses `tracing::warn!` instead.
- The existing comment at service.rs line 1038–1041 ("On failure,
  the local write has already happened — surface the error but
  don't roll back") already describes the M11 philosophy. M11
  takes it to its logical conclusion: don't surface the error
  at all — queue the push and let the background loop handle it.

The key invariant M11 must preserve: **a peer reading the remote
must never see a partial object graph.** The catalog ref
(`op_heads`) is the remote's consistency point. It must only
advance after all blobs it transitively references are present on
the remote. The drain loop's FIFO ordering + deferred catalog-ref
processing ensures this.
