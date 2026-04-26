# jj-yak: Archive — M1 through M6

This document is the archived implementation log for milestones M1–M6.
The main roadmap lives in [`PLAN.md`](./PLAN.md); this file preserves the
detail behind each milestone (RPCs, file paths, scope numbers, deltas
from the original plan) for spelunking purposes. **Do not extend; new
work goes in `PLAN.md`.**

Status of M1–M6: **all done.** The "ship (d)" interim (TTL=0 + missing
FUSE methods) also landed on top of M6. See `PLAN.md` §9 for that
interim and §10 for the next gate (M7).

## M1 — Daemon owns per-mount WC state

Landed as `daemon: M1 — per-mount state map + WC RPCs`.

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

## M2 — Wire YakWorkingCopyFactory at init

Landed as `cli: M2 — route YakWorkingCopyFactory at workspace init` and
`cli/tests: add op_id round-trip smoke test`.

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

## M3 — VFS read path

Landed as `daemon: M3 — split vfs.rs into JjYakFs trait + NFS adapter`
and `daemon: M3 — add fuse3 dep and FuseAdapter (Linux primary path)`.

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

## M4 — `jj yak init` actually mounts

Landed across `proto: M4 — wrap store RPCs in working_copy_path
envelopes + transport oneof`, `daemon: M4 — per-mount Store +
mountpoint validation + VfsManager wiring`, and `cli: M4 — stamp
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

## M5 — `check_out` writes files

Landed as `daemon: M5 — CheckOut RPC + JjYakFs::check_out` and `cli: M5
— call CheckOut from LockedYakWorkingCopy::check_out`.

End-to-end shape:

1. CLI: `LockedYakWorkingCopy::check_out` (`cli/src/working_copy.rs:339`)
   pulls the resolved root `TreeId` out of `commit.tree()` and sends a
   `CheckOut` RPC.
2. Proto: new `CheckOut(CheckOutReq) returns (CheckOutReply)`. Req carries
   `working_copy_path` + `new_tree_id`; reply is empty (reserved for
   added/updated/removed counts once M6 gives us a real tree-diff).
3. Daemon: `JujutsuService::check_out` clones the per-mount
   `Arc<dyn JjYakFs>` out from under the lock, then calls
   `JjYakFs::check_out(new_tree_id)`. `Mount.root_tree_id` is updated
   only on success so the field never lies about what the kernel sees.
4. `JjYakFs::check_out` (on `YakFs`): validates the tree exists in the
   per-mount `Store` (miss → `failed_precondition` "call WriteTree
   first") and re-roots the inode slab via `InodeSlab::swap_root`.
5. `InodeSlab::swap_root` rewrites `ROOT_INODE`'s `NodeRef::Tree` and
   clears the `(parent, name)` reverse cache. Non-root inode entries
   stay live in `inodes` (orphaned but safe — `next_id` is monotonic so
   the kernel never sees a recycled id). Tradeoff is more inode churn
   per checkout; the slab is small per workspace, so we eat the cost.

**Conflicted trees rejected.** `Commit::tree().tree_ids()` returns
`Merge<TreeId>`. Yak only handles the resolved single-id case today;
multi-term merges return `CheckoutError::Other` ("yak: checking out a
conflicted tree is not yet supported"). Conflict materialization
pairs with the conflict UI work — punted, not a blocker for the next
milestones.

**FUSE invalidation deferred.** The original M5 plan said "push
invalidations via `notify_inval_inode` / `notify_inval_entry` for the
changed paths". `fuse3::raw::Session::get_notify` is `fn` (not `pub
fn`) in fuse3 0.8.1, and `MountHandle` doesn't re-expose it — there's
no public API to push invalidations from outside the crate. Options:

- (a) PR upstream to make `Session::get_notify` `pub`. Easiest fix,
  blocks on review.
- (b) Fork or vendor fuse3.
- (c) Switch to `fuser` (sync) and rebuild the trait surface.

Punt: integration tests use `disable_mount = true`, so the kernel
never sees the mount and stale-attr windows don't matter for testing.
Real users today would see up to 60s (the `TTL` in `fuse_adapter.rs`)
of stale `getattr`/`lookup` after a `check_out`. Decide before turning
`disable_mount = false` (which is M6's job). [**Resolution:** the
"ship (d)" interim flipped TTL to 0 instead, see PLAN.md §9 + §7
decision 9.]

**Tests:** +5 daemon unit tests (39 → 44):
`vfs::yak_fs::check_out_swaps_visible_tree`,
`vfs::yak_fs::check_out_unknown_tree_is_store_miss`,
`vfs::inode::swap_root_updates_root_and_clears_reverse_cache`,
`service::check_out_updates_root_tree_and_validates_input`,
`service::check_out_without_mount_is_not_found`. CLI integration
suite goes 3 passed + 3 ignored → 4 passed + 2 ignored:
`test_repos_are_independent` is unblocked (`jj new` round-trips
through `CheckOut` against per-mount Stores).
`test_nested_tree_round_trips` and `test_symlink_tree_round_trips`
stay ignored on M6 (need the VFS write path to capture on-disk writes
into a tree).

**Scope (actual):** ~430 LoC net across `proto/jj_interface.proto`
(new `CheckOut` rpc + `CheckOutReq`/`Reply`),
`daemon/src/vfs/{inode,yak_fs,mod}.rs` (trait method, `swap_root`,
`FsError` re-export), `daemon/src/service.rs` (per-mount `fs:
Arc<dyn JjYakFs>`, `check_out` handler, +2 unit tests),
`cli/src/{blocking_client,working_copy}.rs` (RPC shim + the actual
implementation), `cli/tests/test_init.rs` (un-ignore one test).

## M6 — VFS write path + snapshot

Landed across `daemon: M6 — VFS write path on JjYakFs`, `daemon: M6 —
wire adapters (FUSE + NFS) to JjYakFs write ops`, and `daemon: M6 —
Snapshot RPC delegates to JjYakFs::snapshot`.

End-to-end shape:

1. **Trait surface** (`daemon/src/vfs/yak_fs.rs`). `JjYakFs` grew
   `create_file` / `mkdir` / `symlink` / `write` / `setattr` / `remove`
   / `snapshot`. Errors gained `AlreadyExists` and `NotEmpty` variants
   for the create-collision and rmdir-non-empty paths. `setattr` only
   honours `size` (truncate) and `executable` (chmod) — uid/gid/atime/
   mtime are silently no-ops because we don't model them on the tree.
   Rename intentionally not implemented (`NFS3ERR_ROFS` until a real
   consumer cares — see `nfs_adapter.rs`). [Rename was added during
   the "ship (d)" interim — see PLAN.md §9.]
2. **Slab dirty variants** (`daemon/src/vfs/inode.rs`). `NodeRef`
   gained `DirtyTree { children: BTreeMap }`, `DirtyFile { content,
   executable }`, `DirtySymlink { target }` next to the existing clean
   variants. New slab helpers: `replace_node`,
   `materialize_dir_for_mutation` (clean→dirty promotion that allocates
   child inodes and reuses already-cached ids), `attach_child` /
   `detach_child` (parent's `(name → child)` map maintenance), and a
   leaner `alloc` that no longer pre-registers `by_parent` (split from
   `attach_child` so the caller can back out without leaving stale
   entries).
3. **Lazy promotion in `YakFs`.** First write touching a path promotes
   that inode from clean to dirty by loading content from the per-mount
   `Store`; subsequent writes mutate the in-memory buffer in place. The
   `child_exists` pre-check on create/mkdir/symlink walks the
   still-clean parent tree without forcing a materialize.
4. **Snapshot is recursive sync** (`YakFs::snapshot_node`). Walks the
   slab from `root`, persists every dirty blob into the per-mount
   `Store`, and replaces dirty refs with their content-addressed
   counterparts. **Inode ids are preserved across snapshot** so the
   kernel never sees them change (no ESTALE). The walk is sync so it
   can recurse without `Box::pin` / `async-recursion` — Store ops were
   converted from spurious-async to plain sync in this milestone (none
   of them ever awaited anything; see §5 below).
5. **Adapter dispatch** (`daemon/src/vfs/{fuse,nfs}_adapter.rs`).
   `fuse3::Filesystem` and `nfsserve::NFSFileSystem` mutating methods
   that previously returned ENOSYS / NFS3ERR_ROFS now delegate into the
   trait. The protocol-specific quirks live in the adapters: FUSE
   `create` returns a stateless `fh = 0`; NFS pulls `mode`/`size` out of
   the `set_*` enums on `sattr3` via small `mode_value`/`size_value`
   helpers and applies any `O_TRUNC`-style size on the create path.
6. **`Snapshot` RPC** (`daemon/src/service.rs`). No longer returns the
   cached `Mount.root_tree_id`; clones the per-mount `Arc<dyn JjYakFs>`
   out from under the lock, calls `JjYakFs::snapshot`, and stamps the
   returned id back on `Mount.root_tree_id`. Mirrors the lock pattern
   `CheckOut` set up at M5.

**Determinism:** `DirtyTree.children` is a `BTreeMap`, so snapshot
iterates entries in name-sorted order and `Tree::ContentHash` produces
the same id regardless of write-insertion order. A unit test
(`snapshot_is_deterministic_under_insertion_order`) pins this.

**Tests:** 67 daemon unit tests pass (44 → 67 — added 6 inode-slab
cases, 12 yak_fs write-path cases, 4 NFS adapter cases, 4 FUSE adapter
cases, 2 service-level Snapshot RPC cases, plus a `fs_for_test` helper
on `JujutsuService`). 8 cli unit tests pass. CLI integration tests
unchanged: 2 still passed + 2 still ignored.

**Why M6 didn't unblock the M5+M6-tagged integration tests:**
`test_nested_tree_round_trips` and `test_symlink_tree_round_trips`
write through `std::fs::create_dir`/`std::fs::write` on the mount path.
With `disable_mount = true` (the integration-test setting since M4),
those calls go to the local disk — *not* through the daemon's VFS — so
the daemon's snapshot has no view of them. Unblocking the tests
requires flipping `disable_mount = false`, which depends on the FUSE
invalidation question (PLAN.md §7 decision 9, resolved-as-non-blocker
via TTL=0 in the "ship (d)" interim). Until then, the trait + RPC
shape is exercised end-to-end via the `fs_for_test` helper in
`service::tests::snapshot_rpc_returns_new_tree_id_after_vfs_write`.

**Scope (actual):** ~1700 LoC net across `daemon/src/store.rs`
(spurious-async removal), `daemon/src/vfs/inode.rs` (dirty NodeRef
variants + materialize/attach/detach), `daemon/src/vfs/yak_fs.rs`
(trait surface + YakFs impl + recursive snapshot + write-path tests),
`daemon/src/vfs/{fuse,nfs}_adapter.rs` (write-method dispatch),
`daemon/src/service.rs` (`Snapshot` RPC body + fs_for_test test
helper).

After M6 the daemon-side VCS surface is feature-complete; flipping
`disable_mount = false` is the last hop before `jj describe` / `jj st`
round-trip end-to-end on a real mount.

## Corrections folded into M1–M6 from code review

These adjustments to the original sketch were applied during the
relevant milestone; listed here so reviewers can spot-check the
historical record.

- **Mount field naming.** Original sketch used `workspace_name:
  WorkspaceNameBuf`. Proto has `workspace_id: bytes`
  (`proto/jj_interface.proto:72-75`). M1's struct uses `workspace_id`
  to avoid a gratuitous rename.
- ~~**Fifth `todo!()`.** `daemon/src/ty.rs:277` panics for non-File
  `TreeEntry` variants. Cheap to fill while in the area for M1; will
  hit it as soon as symlinks or subtrees flow through.~~ Already
  handled by the `TryFrom<proto::jj_interface::TreeValue> for
  TreeEntry` impl at `daemon/src/ty.rs:356` (commit `ba36e622`). All
  four variants — `File`, `TreeId`, `SymlinkId`, `ConflictId` — now
  round-trip with proto-decode errors instead of panics.
- **M2 smoke test.** Original plan said `test_init.rs` is read-only —
  only the first of three tests is. `test_multiple_init` and
  `test_repos_are_independent` already exercise `jj new` and `yak
  status`, so they're a better post-M1 signal.
- **Spurious `async` on `Store` (folded in M6).** `Store::write_*` were
  marked `async` but never awaited anything (their bodies are pure
  `parking_lot::Mutex` locks). Kept misleading the trait-impl
  recursion story in `JjYakFs::snapshot`; converted to plain sync at
  M6 so the recursive `snapshot_node` walk doesn't need `Box::pin` /
  `async-recursion`. Layer B (durable storage) may add real I/O to
  these methods — switch back to async if/when that lands.
- **NodeRef dirty variants (M6).** Original M6 sketch said "Each
  mutates an in-memory tree under the `Mount`." The actual
  representation layered the dirty side onto `NodeRef` itself (clean ⊎
  dirty in the same enum) rather than introducing a separate
  working-tree object. Lazy promotion in `YakFs` (clean → dirty on
  first write touching a path) means we only pay the buffer cost for
  paths the user is actually editing.
