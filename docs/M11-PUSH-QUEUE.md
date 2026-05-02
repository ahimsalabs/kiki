# M11 — async push queue + offline resilience

Active spec for M11. Extracted from [`PLAN.md`](./PLAN.md) for
navigability; the milestone index in PLAN.md links here.

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
- **Stable inode ids across restarts (PLAN.md §7 decision 6).** Still
  alongside the `fuser` migration (PLAN.md §9).

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
