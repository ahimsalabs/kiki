# kiki: Implementation Plan

Status: active. Transport architecture decided (§4.3 Path C). M1–M10.7
done — see milestone index below for per-milestone detail. 330 tests
(250 daemon + 17 CLI + 41 integration + 22 store) pass; `cargo clippy
--workspace --all-targets -- -D warnings` is clean. SSH remote
transport (`kiki+ssh://`) landed. **M10.7 (gitignore-aware VFS +
redirections) landed:** `.gitignore` rules loaded at checkout, new
files tagged `ignored` at creation, `snapshot_node` skips ignored
inodes. `.kiki-redirections` file redirects configured dirs
(`node_modules/`, `target/`, etc.) to local scratch storage via
symlinks — all I/O bypasses FUSE entirely. Hot-reload on `.gitignore`
and `.kiki-redirections` writes. **M12 (managed workspaces) active:**
single `RootFs` FUSE mount at `/mnt/kiki/`, per-repo shared git
store, cheap workspace creation, lazy hydration. Next up: M12
implementation, M11 (async push queue). Last updated: 2026-05-02

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

| Milestone | Status | Detail |
|-----------|--------|--------|
| **M1–M6** — Layer A foundation (daemon WC state, VFS trait, mount, checkout, write+snapshot) | ✅ done | [`PLAN-M1-6.md`](./PLAN-M1-6.md) |
| **M7–M9** — `.jj/` separation, durable storage, remote blob CAS | ✅ done | [`PLAN-M7-9.md`](./PLAN-M7-9.md) |
| **M10/M10.5/M10.6** — mutable pointers (CAS-arbitrated catalog), op-heads store, op-store contents, FUSE read-through | ✅ done | [`PLAN-M10.md`](./PLAN-M10.md) |
| **SSH remote** — `kiki+ssh://` transport, `store` crate, `kiki kk serve`, `kiki://` scheme | ✅ done | — |
| **M10.7** — gitignore-aware VFS + redirections | ✅ done | [`M10.7-GITIGNORE.md`](./M10.7-GITIGNORE.md) |
| **Git convergence** — replace custom content store with jj-lib `GitBackend` | ✅ done | [`GIT_CONVERGENCE.md`](./GIT_CONVERGENCE.md) |
| **Daemon lifecycle** — auto-start, launchd/systemd integration | ✅ done | [`DAEMON_LIFECYCLE.md`](./DAEMON_LIFECYCLE.md) |
| **M11** — async push queue + offline resilience | active | [`M11-PUSH-QUEUE.md`](./M11-PUSH-QUEUE.md) |
| **M12 — Workspaces** — single RootFs mount, managed namespace, multi-workspace orchestration | active | [`M12-WORKSPACES.md`](./M12-WORKSPACES.md), [`WORKSPACES.md`](./WORKSPACES.md) |
| **M13 — Git clone & dual-remote model** — first-class git URLs, `kiki remote`, `kiki+ssh://` rename | active | [`M13-GIT-CLONE.md`](./M13-GIT-CLONE.md) |
| **Linear history & segment index** — `linear` ref protection, O(1) ancestor queries | future | [`LINEAR_HISTORY.md`](./LINEAR_HISTORY.md) |
| **Inode GC** — evict unused inodes from memory (critical for macOS NFS) | future | §10 |
| **Graceful restart / takeover** — fd-passing so daemon upgrades don't unmount | future | §10 |
| **fsmonitor / fsnotify** — emit file-change events for IDE and build-tool integration | future | §10 |
| **Observability** — per-operation latency histograms, cache hit rates, queue depths | future | §10 |
| **Ref protection** — server-side enforcement of bookmark/ref rules | future | [`REF_PROTECTION.md`](./REF_PROTECTION.md) |
| **Code review** — submissions, approvals, OWNERS, `land` operation | future | [`REVIEW.md`](./REVIEW.md) |
| **Authentication** — identity, auth mechanisms, authorization | future | [`AUTH.md`](./AUTH.md) |
| **Approvals** — change-scoped approval storage, staleness detection | future | [`APPROVALS.md`](./APPROVALS.md) |

## 3. What's deferred

- **Mutable pointers + op-store — done (M10/M10.5/M10.6).**
  Full detail in [`PLAN-M10.md`](./PLAN-M10.md). CAS-arbitrated
  catalog, `KikiOpHeadsStore`, `KikiOpStore` with write-through
  + read-through, FUSE-side lazy fetch on miss — all landed.
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
  [`M11-PUSH-QUEUE.md`](./M11-PUSH-QUEUE.md). Retry/backoff
  bundled into M11 (the push queue needs a retry strategy by
  construction).
- **Gitignore-aware VFS.** Active —
  [`M10.7-GITIGNORE.md`](./M10.7-GITIGNORE.md). Prerequisite
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

**M1–M10.6 are done.** Integration tests run with `disable_mount =
false` on a real Linux FUSE mount. Layers A (VFS), B (durable
storage), and C (remote blob CAS + catalog + op-store) are
complete. Two CLIs sharing a `dir://` remote can each read the
other's full operation history. Full history: [`PLAN-M1-6.md`](./PLAN-M1-6.md),
[`PLAN-M7-9.md`](./PLAN-M7-9.md), [`PLAN-M10.md`](./PLAN-M10.md).

**Next up:**

- **M10.7** — gitignore-aware VFS
  ([`M10.7-GITIGNORE.md`](./M10.7-GITIGNORE.md)). Prerequisite
  for real-world use with package managers and agent workflows.
- **Daemon lifecycle** — auto-start, launchd/systemd integration
  ([`DAEMON_LIFECYCLE.md`](./DAEMON_LIFECYCLE.md)).
- **Git convergence** — replace custom content store with jj-lib
  `GitBackend` ([`GIT_CONVERGENCE.md`](./GIT_CONVERGENCE.md)).

**Still open:** M11 (async push queue + offline resilience,
[`M11-PUSH-QUEUE.md`](./M11-PUSH-QUEUE.md)), M12 (auth/TLS/S3),
the `fuser` migration (§9), inode-id stability across restarts
(§7 decision 6), and the EdenFS-informed items (§10).

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
longer true at the read paths. M10's lazy remote read-through (see
[`PLAN-M10.md`](./PLAN-M10.md) §10.6)
makes `KikiFs::read_tree`/`read_file`/`read_symlink` actually `await`
the `RemoteStore` on local miss. The fuser migration cost goes up
slightly: the sync fuser callback bodies need a runtime handle for
the post-M10 read paths the same way their write paths already
need it. Still small relative to the rest of the migration.

Land alongside Layer B if possible (we'll already be touching
per-Mount state for stable inode ids).

## 10. EdenFS-informed work items

Informed by reviewing Meta's EdenFS design docs
(`facebook/sapling/eden/fs/docs/`). These are engineering facts
and architectural patterns learned from EdenFS's production
experience with VFS-served source control working copies.

### 10.1 Inode GC (memory management)

**Problem:** The `InodeSlab` grows without bound. On macOS NFS this
is especially bad — NFSv3 has no `FORGET` message (unlike FUSE),
so the daemon never learns that the kernel no longer needs an
inode. EdenFS documents this explicitly: "inode count grows
unboundedly" on NFS, mitigated by background GC with access-time
cutoffs (default 6 hours).

**Design sketch:** Periodic GC task per mount. Walk the slab,
evict non-dirty inodes whose last access time exceeds a cutoff.
On FUSE (Linux), the kernel sends `FUSE_FORGET` naturally, so GC
is less critical but still useful for memory pressure. On NFS
(macOS), GC is the only mechanism.

Reference: ghfs blob cache uses sampled-LRU (power-of-K-choices,
K=5) with platform-specific atime extraction
(`syscall.Stat_t.Atim` on Linux, `Atimespec` on Darwin). The
same atime-based eviction pattern applies to inodes — track last
access in the `Inode` struct, sample K candidates, evict the
oldest.

### 10.2 Graceful restart / takeover

**Problem:** Daemon restart (upgrade, config change, crash
recovery) unmounts everything, breaking every open terminal, IDE,
and build process.

**Mechanism:** fd-passing over a Unix domain socket via
`sendmsg`/`recvmsg` with `SCM_RIGHTS`. Old daemon sends new
daemon:
- FUSE `/dev/fuse` fd (Linux) — mount stays alive
- NFS TCP listener socket fd (macOS) — client connection
  migrates
- UDS listener fd — new daemon picks up CLI connections
- Serialized mount state (inode slab, dirty state, op-id)

EdenFS confirms the NFS client (kernel) tolerates server-side
socket ownership change. Their takeover protocol is
capability-based (version 7) with ping verification, chunked
state transfer, and UID validation.

**Rust crates:** `sendfd` for fd-passing, or raw
`libc::sendmsg`. The serialization payload can use the existing
protobuf types.

**Prerequisite:** Inode handle stability (§7 decision 6) — the
new daemon must produce the same inode numbers as the old one,
otherwise the kernel's dcache is invalid.

### 10.3 Durability invariants and testing

**Problem:** If the daemon crashes between promoting a child
inode to dirty and updating the parent's tree entry in redb,
rehydrate may see inconsistent state. EdenFS documents strict
ordering: "materialized children's overlay data must be written
before the parent records them as materialized" and the reverse
for dematerialization.

**Properties to verify:**
1. No data loss: every dirty inode persisted to redb before crash
   is recoverable after rehydrate
2. No phantom files: no files appear that weren't written
3. Parent consistency: dirty child implies dirty parent chain
4. Ordering: crash between child persist and parent update
   produces a recoverable (not corrupt) state

**Testing approach:** Property-based simulation using `proptest`
or `bolero`. Model the state machine as:
```
enum Op {
    CreateFile(path),
    Write(path, data),
    Mkdir(path),
    Rename(from, to),
    Remove(path),
    Snapshot,
    Crash,              // truncate ops at arbitrary point
    Rehydrate,          // reconstruct state from redb
}
```
Generate random `Vec<Op>` with `Crash` inserted at arbitrary
positions. Assert: `run(ops_before_crash); rehydrate()` produces
a state consistent with the properties above. This tests the
`Store` + `InodeSlab` layer directly — no FUSE/NFS needed, so
it's fast.

Secondary: property-test the takeover serialization:
`forall(state: MountState): deserialize(serialize(state)) == state`.

### 10.4 fsmonitor / fsnotify

**Problem:** The daemon owns every write, so *snapshot* doesn't
need fsmonitor. But external tools (VS Code, IntelliJ, Watchman,
build systems) expect file-change notifications. On FUSE (Linux),
inotify works naturally through the kernel VFS layer. On NFS
(macOS), FSEvents may or may not fire reliably for NFS mounts.

EdenFS solves this with a **Journal** — a log of modifying
filesystem operations, exposed via `getFilesChangedSince()` Thrift
API, consumed by Watchman.

**Design options:**
- **(a)** Expose a `FilesChangedSince(token) -> Vec<ChangedFile>`
  gRPC endpoint. The daemon already knows every mutation — log
  them in a ring buffer with monotonic sequence numbers.
- **(b)** Implement jj-lib's fsmonitor integration point to return
  the dirty set from the slab directly (the daemon already knows).
- **(c)** On macOS NFS, if FSEvents don't fire for NFS mounts,
  the Journal API becomes the only mechanism for IDE integration.

### 10.5 Observability

**Problem:** No per-operation metrics. When something is slow, no
way to distinguish local redb latency from remote fetch latency
from VFS overhead.

EdenFS tracks counters and duration histograms at every layer:
VFS ops (per FUSE/NFS/ProjFS command), object store (memory cache
vs backing store hit rates), overlay ops, backing store (queue
time vs fetch time), cache eviction rates.

**Minimum viable instrumentation:**
- Per-VFS-op latency (read, write, lookup, readdir, getattr) via
  `tracing` spans or `metrics` crate histograms
- Cache hit/miss rates: local redb hit vs remote fetch
- Push queue depth + drain rate (M11)
- Inode slab size (loaded count, dirty count, memory estimate)
- Expose via `DaemonStatus` RPC and/or Prometheus endpoint

### 10.6 macOS NFS hardening

Known issues from EdenFS's macOS NFS experience to track:

- **Hung mounts on daemon death.** Unlike FUSE (returns ENOTCONN),
  NFS mounts become unresponsive. Mitigation: launchd/systemd
  watchdog, or document `umount -f` in troubleshooting.
- **`.nfs-xxxx` temporary files.** Created when a file is removed
  while another process has it open. Filter from snapshot.
- **No xattr support (NFSv3).** Xcode and Finder use xattrs.
  Document as a known limitation.
- **readdir returns names only.** No file types → n+1 LOOKUP
  round-trips per directory listing. Consider `readdirplus` if
  nfsserve supports it.
- **Case-insensitivity.** macOS NFS mounts are case-insensitive
  by default. May cause silent name collisions in case-sensitive
  repos.

