# jj-yak: Archive — M7 through M9

This document is the archived implementation log for milestones M7–M9
plus the "Ship (d)" interim that landed between M6 and M7. The main
roadmap lives in [`PLAN.md`](./PLAN.md); this file preserves the
detail behind each milestone (decisions, file paths, code-audit
findings, wire-protocol specs) for spelunking purposes. **Do not
extend; new work goes in `PLAN.md`.**

Section numbers are preserved from PLAN.md as it existed at the end
of M9: **§9** is the "Ship (d)" interim, **§10** is M7, **§12** is
M8, **§13** is M9. (There was no §11 outcome — `§11` in the historical
PLAN.md was the still-live fuser-migration plan, which now appears as
PLAN.md §9.) Cross-references inside this file use these historical
numbers; references that point at the live PLAN.md are prefixed with
`PLAN.md`.

## 9. "Ship (d)" outcome (interim, 2026-04-26)

Goal was: flip `TTL: Duration::ZERO` in the FUSE adapter and
`disable_mount = false` in tests, find out the real next blocker.

**What landed (committed):**

- `daemon/src/vfs/fuse_adapter.rs`: `TTL = Duration::ZERO` (was
  `Duration::from_secs(60)`). Comment updated to explain the
  trade-off and point at PLAN.md §7 #9.
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

### 10.3 Post-M7 code audit (landed)

Surface metrics first: ~3,940 lines of production Rust across 21
files (5,903 code lines total minus 1,962 inside `#[cfg(test)]`).
Zero `unsafe`. 88 tests passing, 0 ignored. `cargo clippy
--workspace --all-targets -- -D warnings` is clean.

A targeted audit pass after M7 landed picked up four real bugs and
two trailing clippy warnings:

- **Lock-order inversion in `cli/src/blocking_client.rs:182`.**
  Every method acquired `client` before `rt` — except
  `get_empty_tree_id`, which inverted the order. Latent two-mutex
  deadlock if `BlockingJujutsuInterfaceClient` (`Clone + Send`) is
  ever called concurrently. Fixed: restored the canonical order.
- **`Store` API encapsulation hole.** `daemon/src/store.rs` exposed
  every map (`commits`, `files`, `trees`, `symlinks`) as `pub
  Arc<Mutex<HashMap<…>>>`, and `service.rs:read_commit` reached
  past the API to call `store.commits.lock()` directly. Privatized
  the fields, added `Store::get_commit` mirroring the other
  getters, switched `read_commit` over. Layer B's `redb`/`sled`
  swap will be a one-file change instead of a grep.
- **Silent epoch-zero timestamp in `cli/src/backend.rs`.**
  `signature_from_proto` was `proto.timestamp.unwrap_or_default()`
  — a missing wire timestamp round-tripped as 1970-01-01 instead
  of erroring. The daemon-side `TryFrom` in `daemon/src/ty.rs`
  already returned `Err` for the same case. Fixed: both
  `signature_from_proto` and `commit_from_proto` now return
  `BackendResult` and propagate `BackendError::Other("commit
  proto: …")`; the two `proto.author.unwrap_or_default()` /
  `proto.committer.unwrap_or_default()` substitutions in the same
  function were quietly papering over the same bug class and got
  the same treatment.
- **`panic!("GRPC: {:?}", ret)` in `daemon/src/main.rs:101`.**
  Fired on real server-listener death (bind failure, runtime
  drop) with no operator context. Replaced with `tracing::error!`
  + `anyhow::Error` propagation; `main()` already returns
  `Result`, so the process exits non-zero with a real chain
  instead of a `Debug`-formatted `Result<(), tonic::transport::Error>`.
- **Two clippy warnings** (`field_reassign_with_default` in
  `fuse_adapter.rs:824`, `redundant_pattern_matching` in
  `vfs_mgr.rs:180`). One-line fixes each.

What was checked and intentionally **not** changed:

- The 33 `Mutex::lock().unwrap()` calls in
  `cli/src/blocking_client.rs`. They are CLI-process-lifetime safe
  (`std::sync::Mutex` only poisons on panic, and the CLI dies on
  panic), and there are no fallible operations inside the locked
  regions to poison them in the first place. Document if the
  client is ever embedded in a longer-lived process.
- The 6 `todo!()`s in `cli/src/working_copy.rs` (`recover`,
  `rename_workspace`, `reset`, `sparse_patterns`,
  `set_sparse_patterns`). All cold paths requiring explicit
  uncommon user actions; none on the `jj yak init` / `jj log` /
  `jj diff` hot paths. Tracked in PLAN.md §5.
- `async_trait` on `JjYakFs`. Native `async fn` in traits is
  Rust-1.75+; the indirection is fine until we bump MSRV.
- `YakBackend::working_copy_path()` clones a `String` per RPC.
  Method sugar over `self.working_copy_path.clone()`; the clone
  itself is unavoidable because the value goes into a proto by
  value. Not worth churning every call site.

## 12. M8 outcome — Layer B durable storage (done)

Two pieces had to land before the daemon could survive a restart with
the `Mount` map still meaningful: durable per-mount blobs (12.1) and
durable per-mount metadata (12.2). Both shipped; 99 tests pass, clippy
clean, integration suite still green on a real FUSE mount.

Decisions made up front (see request log 2026-04-28):

- **redb 2.x** over sled or fjall. Pure-Rust, ACID, sync `Database`
  fits the existing `Store` shape; built-in `InMemoryBackend` for
  tests; stable 2.x API with no LSM-style background merges.
- **Configurable storage_dir, default to a daemon-managed dir.**
  `daemon.toml` now requires `storage_dir`; `cache` is parsed via
  `#[serde(default)]` so old configs still load but the field has no
  effect.
- **In scope:** persist Store + persist Mount metadata + rehydrate on
  startup + delete `server/` crate.
- **Out of scope (deferred):** stable inode ids derived from
  `(parent_tree_id, name)` (PLAN.md §7 decision 6). Kernel handles
  still don't survive a daemon restart — applications keeping fds open
  through a restart will see ESTALE. Lands when perf justifies the
  `fuser` migration (PLAN.md §9).

### 12.1 redb-backed Store

Schema is four tables (`commits_v1`, `files_v1`, `symlinks_v1`,
`trees_v1`), keyed by 32-byte content-hash bytes, values prost-
encoded. The `_v1` suffix is intentional — schema breaks bump the
suffix and add a migration step rather than reusing a name. The
empty tree is seeded on first open so callers can still
`get_empty_tree_id` -> `get_tree` without a special case.

Surface changes:

- `Store::new()` → `Store::open(path)` for production,
  `Store::new_in_memory()` for tests. Tests opt into an
  `InMemoryBackend` (no tempdir clutter).
- All Store getters/writers now return `anyhow::Result<…>`. The
  pre-M8 infallible API was a `HashMap` artifact; redb commits, table
  opens, and prost decodes are all real failure points. §10.3 just
  removed `panic!` from the daemon's hot paths — introducing
  new ones would have regressed.
- New `FsError::StoreError(String)` variant alongside `StoreMiss`.
  Both adapters map it to `EIO`/`NFS3ERR_IO` and emit a
  `tracing::warn!` carrying the chained anyhow context before
  collapsing — surfacing the underlying redb message at the trace
  layer is much more useful than a bare errno.
- `JujutsuService::new` now takes a `StorageConfig`
  (`OnDisk { root }` or `InMemory`); `Initialize` opens the per-mount
  Store at `<root>/mounts/<hash(wc_path)>/store.redb` (blake3 of the
  WC path, truncated to 16 hex chars; collisions in this namespace
  would require ~4B unique mounts on one host).
- New `StoreTestExt` trait (`#[cfg(test)]`) preserves test-site
  ergonomics — `store.put_file(...)` / `store.read_tree(id)` instead
  of `.expect("write_file").expect("file present")` everywhere.

### 12.2 Mount metadata + rehydrate

Each per-mount directory now holds `mount.toml` next to `store.redb`,
carrying everything `Mount` previously kept only in memory:
`working_copy_path`, `remote`, `op_id`, `workspace_id`,
`root_tree_id`. TOML rather than a redb table: trivial to inspect,
zero coupling to the content-addressed store, atomic writes via
`<file>.tmp` + rename. Bytes are hex-encoded since TOML has no
native byte type.

Persist points:

- `Initialize` writes the initial file. Failure here is fatal
  (`Status::internal`) — the mount has not been registered yet, and
  half-state would be worse than a clean error.
- `SetCheckoutState`, `Snapshot`, `CheckOut` re-write on relevant
  mutations. Failure here is logged at `error` level but doesn't
  fail the RPC; the in-memory state is still authoritative, and a
  transient write failure shouldn't surface as `jj log` failing.

Rehydrate:

- `JujutsuService::rehydrate` runs once at startup, *before* the
  gRPC listener accepts connections. Otherwise an early `Initialize`
  could race with the rehydrate scan.
- Per-mount failures (corrupt TOML, missing redb, mountpoint no
  longer empty) are logged and the mount is skipped. Bringing the
  daemon down on one bad subdir is worse than letting the operator
  clean up.
- Sort by `working_copy_path` so `DaemonStatus` is deterministic
  across restarts.
- Surface change: `JujutsuService::new` now returns `Self`; add
  `into_server(self)` for the wrapped form `main.rs` needs after
  rehydrate.

Test coverage: `persisted_mount_rehydrates_after_restart` drops a
service after writing checkout state, builds a fresh one over the
same `storage_dir`, and confirms `GetCheckoutState` and
`DaemonStatus` both see the rehydrated mount; `mount_meta::tests`
covers TOML round-trip, hex parser, and `list_persisted` skipping
unreadable entries; `Store::open_persists_across_reopen` covers the
redb durability primitive itself.

### 12.3 What `unwrap()` audit picked up alongside

- `Store` API now `Result`-returning across the board (above).
- New `store_status` helper in `service.rs` mirrors the existing
  `decode_status`: maps Layer-B errors to `Status::internal` with
  the chained `{:#}` formatter so the root cause survives the wire.

Still un-touched: the 33 `Mutex::lock().unwrap()` in
`cli/src/blocking_client.rs`. CLI-process-lifetime safe, tracked in
PLAN.md §5.

### 12.4 Hygiene capstone

Deleted the empty `server/` workspace member (3-line
`Hello, world!` `main.rs`, no dependencies, never wired up). PLAN.md
§3 had flagged it for deletion since M1.

## 13. M9 — Layer C: remote blob CAS (done)

M9 wires `Initialize.remote` from a passive string on `mount.toml`
into a real outbound + read-through path. Scope is deliberately
narrow: **content-addressed blobs only**. Mutable pointers (op
heads, ref tips) are explicitly out of scope — they ride on
top of CAS but need their own arbitration story (§13.5) and that
story doesn't fit cleanly inside one milestone. M9 outcome in
§13.9.

### 13.1 Trait shape

The local `Store` is jj-typed (`get_tree`/`write_file`/...) because
it round-trips prost messages. The remote doesn't need to know
about jj types — it's a content-addressed blob store. Blob IDs are
already 32-byte hashes; values are already prost-encoded `bytes`.
So the trait is byte-typed:

```rust
#[async_trait]
trait RemoteStore: Send + Sync {
    async fn get_blob(&self, kind: BlobKind, id: &Id) -> Result<Option<Bytes>>;
    async fn put_blob(&self, kind: BlobKind, id: &Id, bytes: Bytes) -> Result<()>;
    async fn has_blob(&self, kind: BlobKind, id: &Id) -> Result<bool>;
}

enum BlobKind { Tree, File, Symlink, Commit }
```

`BlobKind` is on the trait — not implicit in the bytes — because
the wire-side storage can route by table the same way redb does
locally, and because a content hash collision across kinds is
benign-but-confusing without it. Keeping it on the trait also lets
backends that prefer one big keyspace (S3 prefix, IPLD) flatten it
themselves.

**Why byte-typed beats Store-mirror:**

- Three methods × N backends instead of twelve. Smaller surface
  for every new backend.
- Decouples the remote protocol from prost schema evolution. Proto
  v2 lands → daemon-to-daemon RPC stays the same; only the daemon
  cares about decoding.
- Idempotent by construction: byte-identical puts under the same
  `(kind, id)` are no-ops. Two mounts pushing the same blob is
  benign — no coordination needed for blobs (the PLAN.md §7 #8
  concurrency question reduces to mutable-pointer arbitration,
  which is deferred).

### 13.2 Composition: `service.rs` orchestrates; `Store` stays sync

**Decision (2026-04-28):** the integration site is `service.rs`, not
`Store`. The original spec (a draft of this section) had `Store::
open_with_remote` wrap a `RemoteStore` directly, but `Store` is
sync by design (§12.1 — methods open a redb transaction
without `.await` so `JjYakFs::snapshot_node` can recurse without
`Box::pin`). Composing an async `RemoteStore` inside a sync
`Store` would force one of: (a) make `Store` async (ripples
through ~10 sync helpers in `vfs/yak_fs.rs` and ~30 call sites);
(b) hide a dedicated tokio runtime inside `Store` to bridge
sync→async (adds threads per remote-equipped mount and a non-obvious
re-entry hazard); (c) push integration up to a layer that's
already async. Option (c) — service.rs — has the smallest blast
radius and the cleanest seam; the remote becomes an "RPC layer"
concern that doesn't touch storage internals. The cost is: the
FUSE-side store-miss path in `vfs/yak_fs.rs` doesn't get
read-through automatically (see §13.5 — deferred to M10).

`Mount` (in `service.rs`) gains
`remote: Option<Arc<dyn RemoteStore>>`. RPC handlers do:

- **write-through (in `WriteFile`/`WriteTree`/`WriteSymlink`/
  `WriteCommit` handlers).** After `store.write_*` succeeds, if
  `remote` is `Some`, capture the same prost-encoded bytes the
  local store just wrote and `put_blob` them to the remote.
  Synchronous: the RPC blocks until durable (§13.4). On `put_blob`
  failure the local write has already happened; return the error
  but don't roll back. Idempotent puts + the next snapshot retry
  cover transient remote failures (M9 doesn't track
  already-pushed state).
- **read-through (in `ReadFile`/`ReadTree`/`ReadSymlink`/
  `ReadCommit` handlers).** Local hit returns fast. On local
  miss *and* `remote.is_some()`: `get_blob`, decode, persist to
  local store via `store.write_*` (gives back the same id by
  construction), return. On local miss with no remote: existing
  `not_found` path.
- **post-snapshot push (in `Snapshot` handler).** Blobs written
  through `JjYakFs::snapshot_node` bypass the RPC handlers, so
  `service.rs::Snapshot`, after `fs.snapshot().await` returns
  the new root, walks the tree from that id and pushes every
  reachable blob whose `has_blob` says the remote doesn't have
  it. Walk re-uses local `Store::get_*`, so it's cheap.
- **bytes encoding.** The encoding is exactly what redb stores
  today: `prost::Message::encode_to_vec()` on `*::as_proto()`.
  Push reuses the same buffer (`Bytes::clone`); no re-encoding.

### 13.3 Backends

Two impls in M9. Two is the magic number for trait extraction —
with one impl the trait is shaped by what's easy, not what's
needed.

- **`FsRemoteStore`** (`dir://` scheme). Blobs at
  `<root>/<kind>/<hex(id)>`. Atomic put: write to
  `<root>/<kind>/.tmp.<rand>`, fsync, rename. `has_blob` is a
  `metadata().is_ok()` probe. No locking; concurrent identical-puts
  race on rename and the loser gets `EEXIST` (treated as success).
  Stays as a permanent test fixture and "shared NFS dir between two
  hosts" tool.
- **`GrpcRemoteStore`** (`grpc://host:port` scheme). Tonic client
  against the new `RemoteStore` service (§13.6). The same daemon
  binary serves the `RemoteStore` service on its existing gRPC
  listener, so any daemon can act as the remote for another. No
  new auth design — same trust assumptions as the existing
  `JujutsuInterface` (single-user, localhost). TLS + auth land in
  M11 alongside S3.

URL parsing in `daemon/src/remote/mod.rs::parse(remote: &str) ->
Result<Option<Arc<dyn RemoteStore>>>`. Empty string = `None`
(current behavior preserved). Unknown scheme =
`Status::invalid_argument` at `Initialize`.

### 13.4 Push timing: synchronous on Snapshot

`Snapshot` blocks until every newly-written blob lands on the
remote. Pros: deterministic, easy to test, matches the "WC commit
is durable" mental model. Cons: ties RPC latency to remote latency
— fine for `dir://` and localhost gRPC, will hurt with a real
network remote.

Async background queue (with restart-survivable state) is the
M10/M11 follow-up. The current sync code path is the right shape
for the queue: `Store::write_file` returns the same `Id` either
way, and the queue just batches `put_blob` calls instead of
inlining them.

### 13.5 Out of scope (explicit)

- **Mutable pointers (op heads, ref tips).** Pushing the bytes of
  `.jj/op_heads/heads/<id>` works today via §10.1's
  pinned-`.jj`-walk-into-Store path, and those file blobs would
  flow to the remote naturally under M9. But the *catalog* — "what
  is the latest op_id for this remote?" — has no home in CAS, and
  M9 doesn't add one. Two daemons sharing a `dir://` blob store
  can sync content but not op-log linearity.
- **Concurrency arbitration across mounts (PLAN.md §7 #8).**
  Single-mount-per-remote remains the documented assumption. Two
  mounts at the same remote pushing the same blob is benign
  (idempotent CAS); two mounts pushing competing op-log heads is
  undefined and won't be defined until M10's mutable-pointer
  protocol.
- **Auth, TLS, retry/backoff.** Localhost-only, single-user, no
  TLS. Land alongside S3 (M11+).
- **Stable inode ids across restarts (PLAN.md §7 decision 6).**
  Still deferred; lands with the `fuser` migration (PLAN.md §9).
- **FUSE-side read-through on `StoreMiss`.** The `lookup` /
  `read` / `readdir` paths in `vfs/yak_fs.rs` continue to map
  `StoreMiss` to `EIO`. The §13.2 decision (orchestrate at
  `service.rs`) means yak_fs.rs doesn't see the remote without
  duplicating fetch logic. The M9 demo — two daemons sharing
  blobs via the gRPC store RPCs — is fully covered without it.
  M10 is the right milestone to thread the remote into yak_fs.rs
  (clone-style workflows where a checked-out tree's blobs aren't
  all local) once we know whether to take the orchestration cost
  there or upgrade `Store` to async.

### 13.6 Wire protocol (gRPC backend)

New `service RemoteStore` in `proto/jj_interface.proto`:

```
service RemoteStore {
  rpc GetBlob(GetBlobReq) returns (GetBlobReply) {}
  rpc PutBlob(PutBlobReq) returns (PutBlobReply) {}
  rpc HasBlob(HasBlobReq) returns (HasBlobReply) {}
}

enum BlobKind { TREE = 0; FILE = 1; SYMLINK = 2; COMMIT = 3; }
message GetBlobReq  { BlobKind kind = 1; bytes id = 2; }
message GetBlobReply { bool found = 1; bytes bytes = 2; }
message PutBlobReq  { BlobKind kind = 1; bytes id = 2; bytes bytes = 3; }
message PutBlobReply {}
message HasBlobReq  { BlobKind kind = 1; bytes id = 2; }
message HasBlobReply { bool found = 1; }
```

Unary RPCs in M9. Streaming put/get for large blobs is the obvious
follow-up but not in scope; jj's typical blob sizes are well under
the default 4 MiB tonic message cap.

### 13.7 Test strategy

- **Unit** — every Store method exercised against
  `FsRemoteStore` over a `tempdir()`: write-through populates remote,
  read-through on local miss populates local cache.
- **Integration** — two `JujutsuService` instances over distinct
  `storage_dir`s sharing a `dir://` remote. Service A writes a
  file; service B issues `read_file` for the same id and gets the
  content via read-through. Confirms the abstraction works
  end-to-end and that two daemons over the same remote see each
  other's blobs (modulo the §13.5 mutable-pointer caveat).
- **gRPC backend** — analogous integration test where the "remote"
  is service B's `RemoteStore` server. Proves the byte-typed trait
  is honest under a real network transport.

### 13.8 Commit plan

One commit per task — actual landings:

1. PLAN.md §13 (this section). ✅
2. Proto: `service RemoteStore` + bindings. ✅
3. `RemoteStore` trait + `FsRemoteStore` + URL parser + unit tests. ✅
4. PLAN.md §13.2 amendment — composition site moved from `Store` to
   `service.rs`. ✅ (extra commit; rationale in §13.2)
5. `Store::write_*` → `(Id, Bytes)` so the service handlers can reuse
   the buffer for remote push. ✅ (split out of #6 for cleanliness;
   one mechanical refactor commit, no behaviour change)
6. Service-layer composition: `Mount.remote_store` field + write-
   through, read-through, post-snapshot push + `Initialize.remote`
   URL parse + unit tests + the §13.7 fs-fake integration test
   (two services sharing a `dir://` remote). ✅ (steps 4+5+7 from
   the original draft, combined since the wiring is mutually
   coupled)
7. `GrpcRemoteStore` client + `RemoteStoreService` daemon-side server
   + always-on peer service in `main.rs` + tonic integration tests
   (in-process server over `127.0.0.1:0`) + the §13.7 gRPC analogue
   test (two services sharing a `grpc://` remote). ✅
8. PLAN.md M9 outcome (§13.9). ✅ (this commit)

### 13.9 M9 outcome

Eight commits across the M9 sequence (numbered above), including
the §13.2 amendment. 115/115 daemon tests + 14/14 cli tests pass;
`cargo clippy --workspace --all-targets -- -D warnings` is clean.

Decisions made on the way:

- **Composition at `service.rs`, not `Store` (§13.2 amendment).** The
  draft originally had `Store::open_with_remote` wrap a `RemoteStore`
  directly, but `Store` is sync by design (§12.1 — methods open
  a redb txn without `.await` so `JjYakFs::snapshot_node` can recurse
  without `Box::pin`). Composing an async `RemoteStore` inside a sync
  `Store` would force one of: make `Store` async (ripples through
  ~10 sync helpers + ~30 call sites), hide a tokio runtime inside
  `Store` (re-entry hazard, threads-per-mount), or push integration
  up. The third option won — the remote becomes an "RPC layer"
  concern that doesn't touch storage internals. The cost: FUSE-side
  read-through on `StoreMiss` doesn't get the remote automatically
  (deferred to M10).

- **`Store::write_*` returns `(Id, Bytes)`.** §13.2 calls for
  buffer reuse on push ("Push reuses the same buffer (`Bytes::clone`);
  no re-encoding"). Three mechanical options to make that work:
  expose pre-encoded bytes externally, take pre-encoded bytes
  externally, or return them from the existing call. The third is
  smallest-diff: snapshot_node ignores the bytes (`let (id, _) =
  ...`), service handlers reuse them. Re-decoding on the reverse
  read-through path is allowed (the cold path), avoided on the hot
  push path.

- **Pre-encoded bytes on the post-snapshot walk too.** Added
  `Store::get_*_bytes(id) -> Result<Option<Bytes>>` for tree, file,
  symlink, commit. The walk runs `has_blob` per reachable blob and,
  on miss, reads raw bytes from redb (no decode) and `put_blob`s
  them. Walking the entire reachable tree on every snapshot is
  wasteful but correct — `has_blob` short-circuits unchanged
  subtrees on the second-and-later snapshot. The async background
  queue (M10/M11) would batch these.

- **`verify_round_trip` on read-through.** When the remote returns
  bytes, we re-hash them locally before persisting. A hash mismatch
  surfaces as `Status::data_loss` rather than silently poisoning the
  local store. Cheap (already computing hash to write locally) and
  closes a real corrupt-peer attack surface.

- **`grpc://` server is always-on, backed by
  `<storage_dir>/served_blobs/`.** Every daemon registers
  `RemoteStoreService` on its existing tonic listener (§13.6's
  "any daemon can act as the remote for another"). The served-blobs
  dir is separate from per-mount redb stores so M4's cross-mount
  keyspace isolation invariants hold on the served side too. No
  config knob — auth/TLS would need one, but those are M11.

- **`connect_lazy` for the `grpc://` client.** Keeps `remote::parse`
  synchronous and defers transport failures to the first RPC (where
  there's a real tracing context). A `parse_grpc_returns_some` test
  needs `#[tokio::test]` even though no RPC fires — tonic touches
  the executor on `Channel` construction.

- **`BlobKind::Unspecified` rejected on the wire.** protobuf3 enums
  always admit zero; `BLOB_KIND_UNSPECIFIED` represents
  "missing/unset" rather than a partition. The server's `decode_kind`
  surfaces it as `invalid_argument` so a peer sending the zero
  value gets a clean error rather than a silent route to a "default"
  table that doesn't exist.

- **Pre-M9 placeholder strings break the parse.** Existing CLI
  tests + service tests passed `remote: "localhost"` as a free-form
  label. M9's strict `dir://…|grpc://…|""` parse rejects that. CLI
  tests switched to `""` (back-compat path); service tests use a
  real `dir://<tempdir>` URL where they exercise the rehydrate
  round-trip. The `yak status` formatter now drops `- <remote>`
  when `remote` is empty (was `path - ` with trailing whitespace).

What this milestone explicitly does **not** do (preserved from
§13.5, repeated here so it's findable in the outcome):

- Mutable pointers (op heads, ref tips). Two daemons over the same
  blob store can sync content but not op-log linearity. M10.
- Concurrency arbitration across mounts (PLAN.md §7 #8). Single-
  mount-per-remote remains the documented assumption.
- Auth, TLS, retry/backoff. M11 alongside S3.
- Stable inode ids across restarts (PLAN.md §7 decision 6). With the
  `fuser` migration (PLAN.md §9).
- FUSE-side read-through on `StoreMiss`. The §13.2 decision means
  yak_fs.rs doesn't see the remote without duplicating the
  orchestration cost. M10 is the right milestone to thread it
  through (clone-style flows are when the kernel actually asks for
  blobs the local store doesn't have).

Test coverage added in M9:

- `daemon::remote::tests` (parser): empty/dir/relative/no-scheme/
  grpc-empty/grpc/unknown-scheme + BlobKind proto round-trip.
- `daemon::remote::fs::tests`: every method + idempotent put +
  per-kind keyspace partition + lazy root creation.
- `daemon::remote::server::tests`: round-trip put→get + missing
  blob → `found=false` + unspecified kind rejected + short id
  rejected + has_blob tracks state.
- `daemon::remote::grpc::tests`: end-to-end against a real tonic
  listener over `127.0.0.1:0` — round-trips every BlobKind, two
  clients sharing a server see each other's blobs, server rejects
  short id even when the client happens to bypass its own
  validation.
- `daemon::service::tests` M9 cases:
  `initialize_rejects_unparseable_remote`,
  `write_file_pushes_to_dir_remote`,
  `read_file_falls_back_to_remote_on_local_miss`,
  `read_file_local_miss_no_remote_is_not_found`,
  `snapshot_pushes_reachable_blobs_to_remote`,
  `snapshot_is_idempotent_against_remote`,
  `two_services_share_blobs_via_grpc_remote`.
- `persisted_mount_rehydrates_after_restart` upgraded to use a
  real `dir://<tempdir>` URL so rehydrate exercises the
  `remote::parse` → `Mount.remote_store` round-trip too.
