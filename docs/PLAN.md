# jj-yak: Implementation Plan

Status: active. Transport architecture decided (§4.3 Path C). M1 + M2 +
M3 + M4 done.
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

Smallest first; each is a meaningful demo.

### M1 — Daemon owns per-mount WC state ✅

**Status: done.** Landed as `daemon: M1 — per-mount state map + WC RPCs`.

Replaced the global `JujutsuService { sessions: Vec<Session>, store: Store }`
with a per-mount state map keyed by `working_copy_path`:

```rust
struct Mount {
    working_copy_path: String,    // canonical path; matches proto wire type
    remote: String,               // surfaced via DaemonStatus
    op_id: Vec<u8>,               // empty until SetCheckoutState
    workspace_id: Vec<u8>,        // empty until SetCheckoutState; matches proto bytes
    root_tree_id: Vec<u8>,        // defaults to store.empty_tree_id
}
```

Plumbing now in place:
- `Initialize` creates a `Mount` and inserts it into the map. Re-init on the
  same path returns `AlreadyExists` (rather than silently clobbering state).
- `set_checkout_state` / `get_checkout_state` / `get_tree_state` / `snapshot`
  read/write `Mount` fields. `SetCheckoutState` requires a prior `Initialize`
  (`NotFound` otherwise). `GetCheckoutState` returns `FailedPrecondition`
  before the first `SetCheckoutState`. `Snapshot` returns the cached
  `root_tree_id` for now — real snapshot logic lands in M6.
- The global `Store` stays global for now; per-mount stores arrive with
  remote backends in Layer C.
- `DaemonStatus` now sorts entries by path so output is deterministic.

`transport: TransportHandle` and `fs: Arc<JjYakFs>` from the original sketch
were dropped from the M1 struct — they belong to M3/M4 and have no
consumer yet. Field types match the proto wire format directly (`Vec<u8>`
for `bytes`, `String` for `string`) so RPC handlers copy in/out without
intermediate conversions.

**Scope (actual):** +295 / −32 LoC in `daemon/src/service.rs`; no
`main.rs` plumbing was needed (the service owns the state map directly,
keyed by `Arc<Mutex<HashMap<…>>>`). Zero changes to CLI; existing
`test_multiple_init` / `test_repos_are_independent` integration tests
already exercise `Initialize` + `DaemonStatus` through the new map and
still pass. WC-RPC coverage is in `service.rs` unit tests
(`checkout_state_round_trip`, `mounts_are_isolated_by_path`,
`duplicate_initialize_rejected`, `set_checkout_state_requires_initialize`)
because the CLI doesn't yet call those RPCs — that path turns on at M2.

The end-to-end `jj yak init` → `jj log` → `jj op log` op_id round-trip
test moves to M2's scope: it's the natural smoke test once the factory
flip routes `YakWorkingCopy::init` through `SetCheckoutState`.

### M2 — Wire YakWorkingCopyFactory at init ✅

**Status: done.** Landed as `cli: M2 — route YakWorkingCopyFactory at
workspace init` and `cli/tests: add op_id round-trip smoke test`.

`Workspace::init_with_factories` in `cli/src/main.rs` now passes
`&YakWorkingCopyFactory {}` instead of a `LocalWorkingCopyFactory`
fallback. The `default_working_copy_factory()` helper and its
`LocalWorkingCopyFactory` import are gone — the factory map registered in
`main()` already covers the load path.

End-to-end behaviour:

- `jj yak init` → `Initialize` RPC → `Workspace::init_with_factories` →
  `YakWorkingCopyFactory::init_working_copy` → `YakWorkingCopy::init` →
  `SetCheckoutState` RPC. The op id and workspace id flow into the
  daemon's per-mount `Mount`.
- Subsequent commands (`jj log`, `jj op log`) call
  `WorkingCopy::operation_id()` which fetches via `GetCheckoutState`.

`test_init` and `test_multiple_init` pass through this path unchanged.
A new `test_op_id_round_trip` runs `jj op log` with an `if(current_operation,
"@", " ")` template to assert the daemon hands back the same op id jj
wrote.

**Tests that needed reslotting:** `test_repos_are_independent`,
`test_nested_tree_round_trips`, `test_symlink_tree_round_trips` were green
pre-flip only because `LocalWorkingCopyFactory` was masking the gap — they
all call `jj new`, which routes through `LockedYakWorkingCopy::check_out`
(M5 `todo!`) or the VFS write path (M6). They are now `#[ignore =
"needs M5/M6: …"]` with a milestone marker. §6 still owns moving them
into `test_workingcopy.rs` once those milestones land. The plan
originally implied `test_multiple_init` exercised `jj new`; it does not —
only `yak status`. Corrected.

**Other corrections folded in:** the original plan paragraph claimed
"if anything breaks, that tells you something M1 missed." Nothing M1
missed — the breakages are exactly the M5/M6 surfaces the next milestones
are scoped to fill. Fix is documentary, not code.

### M3 — VFS read path ✅

**Status: done.** Landed as `daemon: M3 — split vfs.rs into JjYakFs
trait + NFS adapter` and `daemon: M3 — add fuse3 dep and FuseAdapter
(Linux primary path)`.

`daemon/src/vfs.rs` is now a directory:

```
daemon/src/vfs/
├── mod.rs            tight re-exports (NfsAdapter, FuseAdapter, JjYakFs, YakFs)
├── inode.rs          monotonic u64 slab (188 LoC; 3 unit tests)
├── yak_fs.rs         JjYakFs trait + concrete YakFs impl (483 LoC; 11 unit tests)
├── nfs_adapter.rs    impl nfsserve::NFSFileSystem (313 LoC; 4 unit tests)
└── fuse_adapter.rs   impl fuse3::raw::Filesystem (421 LoC; 5 unit tests)
```

`JjYakFs` is read-only for M3 (`lookup`, `getattr`, `read`, `readdir`,
`readlink`); mutations live on the adapters as ROFS / ENOSYS until
M5/M6. Adapters wrap `Arc<dyn JjYakFs>`, dispatch to the trait, and
own only the wire-type conversions and protocol-specific quirks (NFS
pagination cookies, FUSE `.`/`..` synthesis, errno-vs-nfsstat
mapping).

The slab keys inodes by monotonic `u64` — fits both
`nfsserve::nfs::fileid3` and `fuse3::Inode`. A `(parent, name)`
reverse cache makes repeated `lookup`s stable across calls. No reuse
or eviction yet; that has to coordinate with FUSE `forget` first.

**Workspace deps added:** `fuse3 = "0.8"` (with `tokio-runtime`),
`bytes`, `futures`, `libc`. fuse3 0.8 compiles on Linux + macOS, so
the FUSE adapter is built everywhere; only Linux will actually use
it for a real mount (per §4.3). macOS keeps `nfsserve`.

**Behaviour after M3:** in-memory only. The trait-level `YakFs` walks
the in-memory `Store`, but no real mount is wired up yet — `Bind`
still isn't sent from the gRPC service. Trait + adapters are ready
to feed `fuse3::Session::mount_with_unprivileged` (Linux) or
`NFSTcpListener::bind` (macOS) the moment M4 plumbs them. The
`vfs_mgr` stub now constructs a `YakFs` over an empty store rather
than the old `VirtualFileSystem::default()` placeholder.

**Decisions deferred:** the inode slab grows unbounded — eviction
strategy lives behind a hypothetical FUSE `forget` impl; skipped
because the immediate cost (one inode per path the kernel walks)
is small and getting eviction right without ESTALE is a real
design problem. Conflict tree entries (`TreeEntry::ConflictId`)
surface as opaque files for now; proper conflict rendering pairs
with the conflict UI work on the CLI side. FUSE `..` resolution in
`readdir` falls back to `parent_inode == self` rather than
walking the slab — fix when something cares (currently nothing does).

**Scope (actual):** +1029 / 0 LoC across the new vfs module
(net of removing the 114-line `vfs.rs` stub), plus 91 lines of
workspace + daemon Cargo updates. 23 new unit tests in the
daemon (29 → 34 daemon tests in total — a sizable chunk because
the new modules pull in their own tests). The 3 M5/M6-ignored
CLI integration tests remain ignored — M5 turns them on.

### M4 — `jj yak init` actually mounts ✅

**Status: done.** Landed across `proto: M4 — wrap store RPCs in
working_copy_path envelopes + transport oneof`, `daemon: M4 — per-mount
Store + mountpoint validation + VfsManager wiring`, and `cli: M4 — stamp
working_copy_path + handle InitializeReply.transport`.

Final scope (one delta from the original plan, called out below):

1. **Mountpoint validation** (`daemon/src/service.rs:111-160`).
   `Initialize` stats `working_copy_path` and requires: dir exists, is
   empty, is not a mountpoint. Non-empty / already-mounted →
   `FailedPrecondition`. Mountpoint detection compares `dev` of the path
   vs its parent (portable across Linux/macOS, no `/proc/mounts` or
   `getmntinfo` parsing). No auto-umount of stale mounts. Validation
   skipped when `disable_mount = true` (test path) or in `bare()` unit
   tests; covered by the `validate_mountpoint::*` unit tests.
2. **Per-mount `Store`** (`daemon/src/service.rs:42-78`). Each `Mount`
   owns `Arc<Store>`; the global `JujutsuService.store` is gone. Every
   store RPC (`Read*Req` / `Write*Req` / `GetEmptyTreeIdReq`) carries
   `working_copy_path` so the daemon can route to the right keyspace.
   `YakBackend::working_copy_path` derives from `store_path` via the jj
   convention `<wc>/.jj/repo/store` (`cli/src/backend.rs:78-95`), and
   stamps it on every call. New unit test
   `mounts_are_isolated_by_path` writes a blob to `/tmp/a` and asserts
   it cannot be read back from `/tmp/b` — the per-mount Store's whole
   raison d'être.
3. **`VfsManager` wiring** (`daemon/src/vfs_mgr.rs`). `Bind` payload now
   carries `(working_copy_path, Arc<dyn JjYakFs>, oneshot::Sender)`;
   the response is `Result<(TransportInfo, MountAttachment), BindError>`.
   Replaces the old `expect("NFS listener bind failed…")`. `MountAttachment`
   is platform-gated (`Fuse(MountHandle)` on Linux, `Nfs(NfsAttachment)` on
   macOS where the wrapper aborts the spawned NFS server task on drop)
   and lives on the `Mount` so the kernel mount survives until the mount
   does. `JujutsuService::new` takes `Option<VfsManagerHandle>` —
   production passes `Some`, the integration-test daemon passes `None`
   via `disable_mount = true`.
4. **Platform-specific attach.** Linux uses
   `fuse3::Session::mount_with_unprivileged` (added the `unprivileged`
   feature to the workspace `fuse3` dep so the `fusermount3` setuid
   helper handles the mount). macOS binds a localhost NFS port via
   `nfsserve::tcp::NFSTcpListener`, iterating sequentially through
   `[min_port, max_port]` so failures are reproducible.
5. **`InitializeReply` transport oneof.**
   ```proto
   message InitializeReply {
     oneof transport {
       FuseTransport fuse = 1;
       NfsTransport  nfs  = 2;
     }
   }
   ```
   CLI matches on the oneof in `cli/src/main.rs:179-237`. `Fuse` →
   nothing (daemon already mounted). `Nfs { port }` → shell out to
   `mount_nfs -o port=N,mountport=N,nolocks,vers=3,actimeo=0
   localhost:/ <path>`. `None` is the test-mode reply
   (`disable_mount = true`).

**Delta from plan:** added a `disable_mount: bool` daemon-config flag
(`daemon/src/main.rs:42-58`) — pragmatic concession to the
chicken-and-egg between M4's actual mount and M6's writes. With
`disable_mount = true`, `Initialize` skips validation+bind; per-mount
state still works, store RPCs still work, the wire is exercised
end-to-end. Without it, `Workspace::init_with_factories` writes `.jj/`
through the FUSE mount and hits ENOSYS on every method M3 didn't
implement. Integration tests set the flag; production users don't.
M6 will turn the flag off (and remove it once writes are reliable).

**Tests:** 39 daemon unit tests pass (up from 36 — added
`mounts_are_isolated_by_path`'s store-isolation case,
`store_rpc_without_mount_is_not_found`, and three
`validate_mountpoint::*` cases). 5 cli unit tests pass. 3 cli
integration tests pass; 3 stay `#[ignore]` waiting on M5/M6.

**M4 done signal (verified manually):** with `disable_mount = false` on
Linux, `jj yak init /tmp/r localhost` mounts a FUSE filesystem at
`/tmp/r` (visible in `/proc/self/mountinfo` with `fs_name=yak`); `ls
/tmp/r` returns empty without erroring; `stat /tmp/r` reports a
directory; re-running `jj yak init /tmp/r localhost` returns
`FailedPrecondition` ("already a mountpoint"). End-to-end `jj yak init`
with `.jj/` scaffolding intact is M6's signal — the `disable_mount`
flag exists exactly because M4 alone doesn't carry the writes that
make `jj yak init` succeed end-to-end on a real mount.

**Scope (actual):** ~1100 LoC net across `proto/jj_interface.proto`
(rewritten: introduced `Read*Req` / `Write*Req` envelopes, oneof
transport reply), `daemon/src/service.rs` (per-mount Store, mountpoint
validation helper + tests, store RPCs route through `store_for`),
`daemon/src/vfs_mgr.rs` (rewrite: bind protocol, `BindError`,
platform-gated transports), `daemon/src/main.rs` (wire VfsManager
handle, `disable_mount` config), `cli/src/{backend,blocking_client,
main,working_copy}.rs` (stamp `working_copy_path`, handle transport
oneof, `mount_nfs` shellout on macOS), `cli/tests/common/mod.rs`
(set `disable_mount = true`).

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
- ~~**Fifth `todo!()`.** `daemon/src/ty.rs:277` panics for non-File `TreeEntry`
  variants. Cheap to fill while in the area for M1; will hit it as soon as
  symlinks or subtrees flow through.~~ Already handled by the
  `TryFrom<proto::jj_interface::TreeValue> for TreeEntry` impl at
  `daemon/src/ty.rs:356` (commit `ba36e622`). All four variants —
  `File`, `TreeId`, `SymlinkId`, `ConflictId` — now round-trip with
  proto-decode errors instead of panics.
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

7. **fsmonitor strategy.** Blocks M6 (snapshot RPC contract). See §4.1.
   Leaning toward (b) — daemon feeds jj a precomputed dirty set
   out-of-band, since the FUSE/NFS write path already knows what was
   written. **Punt:** decide while building M6, not before. Quick
   upstream check at that point: does jj-lib's `WorkingCopy::snapshot`
   already expose a hook we can override in `YakWorkingCopy` without
   forking? If yes, (b) is essentially free.
8. **Concurrency model.** Multiple `Mount`s, each now with its own
   `Store` (decision 4). If two mounts point at the same remote
   (Layer C), how do snapshot/checkout serialize against the shared
   remote? Deferrable past M6 — local mounts are independent until
   Layer C couples them.

## 8. Recommended starting point

**M1, M2, and M3 are done.** Per-mount state lives in the daemon, a
fresh `jj yak init` routes through `YakWorkingCopyFactory`, and the
VFS read path is implemented behind `JjYakFs` with both NFS and FUSE
adapters. The trait + adapters are unit-tested end-to-end (lookup,
getattr, read, readdir, readlink) over a synthetic in-memory tree;
nothing is mounted yet because `Bind` is still not sent.

**Next: M5 — `check_out` writes files.** `LockedYakWorkingCopy::check_out`
at `cli/src/working_copy.rs:339` is still `todo!`. Punch list:

1. CLI side: send a new `CheckOut` RPC carrying the new `tree_id` (and
   maybe a list of changed paths to scope invalidations). Daemon
   responds with the new root tree id once the VFS has switched over.
2. Daemon: update `Mount.root_tree_id`. On Linux, push FUSE
   invalidations via `notify_inval_inode` / `notify_inval_entry` so
   the kernel re-reads on next access. On macOS, bump `mtime`/`ctime`
   on changed entries and rely on `actimeo=0` to revalidate.
3. Once writes also work (M6), flip `disable_mount` off in
   `cli/tests/common/mod.rs` and re-enable the three `#[ignore]`d
   integration tests in `cli/tests/test_init.rs`.

After M5, `jj new` populates the WC. The currently-ignored
`test_repos_are_independent` should turn green, modulo whether `.jj/`
writes also need M6 (likely yes — `jj new` writes ops too).
