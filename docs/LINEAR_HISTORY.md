# Linear History & Segment Index

Research notes on incorporating linear-history optimization into kiki.
Covers enforcement, daemon-side indexing, and jj-lib integration.

**Context:** The jj community favors rebase-always workflows, which
produce linear mainline history. Meta's segmented changelog work
(Sapling/Mononoke) shows that linear history enables O(1) ancestor
queries by assigning monotonic integer IDs to commits within segments.

## Enforcement: `linear` ref protection mode

A natural extension of [REF_PROTECTION.md](./REF_PROTECTION.md).
Alongside `immutable` and `append_only`, add a `linear` mode:

```toml
# .kiki/protect.toml
[refs.main]
mode = "linear"   # append_only + every commit in the chain is single-parent
```

When a client calls `cas_ref(main, old_tip, new_tip)`, the daemon:

1. Walks git parents from `new_tip` backward toward `old_tip`.
2. At each commit, asserts `parents.len() == 1`.
3. Fails the CAS if any commit has multiple parents (merge) or if
   `old_tip` is not reachable.

The walk is bounded by the number of new commits (typically small).
The daemon already has `read_commit` → `parents: Vec<CommitId>`.

**Scope:** ~50 lines in the `cas_ref` enforcement path. No proto,
RemoteStore, or storage changes.

## Segment index

### What it is

In a linear history, assign monotonic sequence numbers:

```
main:  C0 ← C1 ← C2 ← C3 ← C4 ← C5
       #0   #1   #2   #3   #4   #5
```

`is_ancestor(C1, C4)` becomes `1 < 4` — O(1).

For branches, store segments:

```
Segment 0: [C0..C5] parent: none       (main)
Segment 1: [C6..C8] parent: seg0@#3    (feature branch off C3)
```

Ancestor queries across segments: O(log segments). In a rebase-always
workflow the main branch is fully linear, so the "universal segment"
covers most history.

### Phase 1: Daemon-side redb table

New table in `GitContentStore`'s redb alongside `operations_v1`:

```
commit_seq_v1:  &[u8; 20] → u64     # commit SHA-1 → sequence number
segments_v1:    u64 → segment_meta   # segment id → (start, end, parent_seg, parent_pos)
```

Written at `write_commit` time. Rebuilt from the git DAG on first mount
(one-time cost; linear trunk makes this fast).

**Unlocks:**
- O(1) `is_ancestor` for ref protection enforcement (no git parent walk)
- Efficient incremental sync: "commits since X" = range `seq(X)+1..HEAD`
- Fast `heads()` computation for push decisions

**Scope:** ~200 lines, no jj-lib coupling, no proto changes.

### Phase 2: Shared via RemoteStore

Store the segment table as a blob (`BlobKind::CommitIndex` or similar)
with a well-known ref (`"__segment_index"`) that CAS-advances when the
trunk advances. Client daemons fetch the index rather than rebuilding.

**Scope:** New `BlobKind` variant (~2 lines in proto), new push step in
commit write path.

### Phase 3: jj-lib IndexStore integration

Plug a custom `IndexStore` into jj-lib so CLI-side operations (`jj log`,
revsets, rebase) benefit from the segment table.

## jj-lib Index trait surface (as of jj-lib 0.40)

We pin jj-lib's version, so trait changes are a migration, not a
surprise. Full surface documented here for future reference.

### Registration

`StoreFactories::add_index_store(name, factory)` in `cli/src/main.rs`.
We already register custom factories for `KikiBackend`, `KikiOpStore`,
and `KikiOpHeadsStore` — same pattern.

The factory type:
```rust
Box<dyn Fn(&UserSettings, &Path) -> Result<Box<dyn IndexStore>, BackendLoadError>>
```

`RepoLoader::new()` also accepts `index_store: Arc<dyn IndexStore>`
directly for cases without file-system discovery.

### IndexStore (3 methods)

```rust
pub trait IndexStore: Any + Send + Sync + Debug {
    fn name(&self) -> &str;
    async fn get_index_at_op(&self, op: &Operation, store: &Arc<Store>)
        -> IndexStoreResult<Box<dyn ReadonlyIndex>>;
    fn write_index(&self, index: Box<dyn MutableIndex>, op: &Operation)
        -> IndexStoreResult<Box<dyn ReadonlyIndex>>;
}
```

### ReadonlyIndex (3 methods)

```rust
pub trait ReadonlyIndex: Any + Send + Sync {
    fn as_index(&self) -> &dyn Index;
    fn change_id_index(&self, heads: &mut dyn Iterator<Item = &CommitId>)
        -> Box<dyn ChangeIdIndex>;
    fn start_modification(&self) -> Box<dyn MutableIndex>;
}
```

### MutableIndex (4 methods)

```rust
pub trait MutableIndex: Any {
    fn as_index(&self) -> &dyn Index;
    fn change_id_index(&self, heads: &mut dyn Iterator<Item = &CommitId>)
        -> Box<dyn ChangeIdIndex + '_>;
    async fn add_commit(&mut self, commit: &Commit) -> IndexResult<()>;
    fn merge_in(&mut self, other: &dyn ReadonlyIndex) -> IndexResult<()>;
}
```

**`merge_in` caveat:** The default implementation downcasts `other` to
its own concrete type. A custom `MutableIndex` must handle receiving a
`ReadonlyIndex` of a potentially different type.

### Index (8 methods)

```rust
pub trait Index: Send + Sync {
    fn shortest_unique_commit_id_prefix_len(&self, commit_id: &CommitId) -> IndexResult<usize>;
    fn resolve_commit_id_prefix(&self, prefix: &HexPrefix)
        -> IndexResult<PrefixResolution<CommitId>>;
    fn has_id(&self, commit_id: &CommitId) -> IndexResult<bool>;
    fn is_ancestor(&self, ancestor_id: &CommitId, descendant_id: &CommitId) -> IndexResult<bool>;
    fn common_ancestors(&self, set1: &[CommitId], set2: &[CommitId]) -> IndexResult<Vec<CommitId>>;
    fn all_heads_for_gc(&self) -> IndexResult<Box<dyn Iterator<Item = CommitId> + '_>>;
    fn heads(&self, candidates: &mut dyn Iterator<Item = &CommitId>) -> IndexResult<Vec<CommitId>>;
    fn evaluate_revset(&self, expression: &ResolvedExpression, store: &Arc<Store>)
        -> Result<Box<dyn Revset + '_>, RevsetEvaluationError>;
}
```

### ChangeIdIndex (2 methods)

```rust
pub trait ChangeIdIndex: Send + Sync {
    fn resolve_prefix(&self, prefix: &HexPrefix)
        -> IndexResult<PrefixResolution<ResolvedChangeTargets>>;
    fn shortest_unique_prefix_len(&self, change_id: &ChangeId) -> IndexResult<usize>;
}
```

### Revset (8 methods)

```rust
pub trait Revset: fmt::Debug {
    fn iter(&self) -> Box<dyn Iterator<Item = Result<CommitId, RevsetEvaluationError>> + '_>;
    fn stream(&self) -> LocalBoxStream<'_, Result<CommitId, RevsetEvaluationError>>;
    fn commit_change_ids(&self)
        -> Box<dyn Iterator<Item = Result<(CommitId, ChangeId), RevsetEvaluationError>> + '_>;
    fn iter_graph(&self)
        -> Box<dyn Iterator<Item = Result<GraphNode<CommitId>, RevsetEvaluationError>> + '_>;
    fn stream_graph(&self)
        -> LocalBoxStream<'_, Result<GraphNode<CommitId>, RevsetEvaluationError>>;
    fn is_empty(&self) -> bool;
    fn count_estimate(&self) -> Result<(usize, Option<usize>), RevsetEvaluationError>;
    fn containing_fn(&self) -> Box<RevsetContainingFn<'_>>;
}
```

## The `evaluate_revset` wall

**Critical constraint:** `CompositeIndex`, `AsCompositeIndex`, and the
revset engine internals are all `pub(super)` in `jj_lib::default_index`.
A custom `Index` cannot call into the default revset engine.

This means a custom `IndexStore` must either:

1. **Wrap `DefaultIndexStore`** — delegate `evaluate_revset` to the
   inner `DefaultReadonlyIndex`, intercept `is_ancestor` / `heads` /
   `common_ancestors` with the segment table for direct callers.

2. **Implement a full revset engine** — ~3-5k lines of graph algorithm
   code. What Meta built for Sapling. Not justified until repo scale
   demands it.

### The wrapping pattern

```rust
struct KikiReadonlyIndex {
    inner: Box<dyn ReadonlyIndex>,  // DefaultReadonlyIndex
    segments: Arc<SegmentTable>,
}

impl Index for KikiReadonlyIndex {
    fn is_ancestor(&self, a: &CommitId, b: &CommitId) -> IndexResult<bool> {
        // O(1) via segment table for linear history
        if let Some(result) = self.segments.is_ancestor(a, b) {
            return Ok(result);
        }
        // Fall back to default DAG walk
        self.inner.as_index().is_ancestor(a, b)
    }

    fn evaluate_revset(&self, expr, store) -> Result<Box<dyn Revset + '_>, _> {
        // Must delegate — revset engine is not public
        self.inner.as_index().evaluate_revset(expr, store)
    }
    // ... other methods similarly wrap with segment fast-path
}
```

**What this gives you:** Fast `is_ancestor`, `common_ancestors`, `heads`
for callers going through the `Index` trait (rebase, merge resolution,
some `jj log` paths).

**What it doesn't give you:** The revset engine calls `CompositeIndex`
methods directly, not through the `Index` trait, so revset expressions
like `x::y` don't benefit from the segment table.

## Recommended build order

| Phase | What | Depends on | Effort |
|-------|------|------------|--------|
| 1 | `linear` ref protection mode | REF_PROTECTION.md | ~50 lines |
| 2 | Daemon-side segment table (redb) | Git convergence | ~200 lines |
| 3 | Shared segment index via RemoteStore | Phase 2 | ~100 lines |
| 4 | jj-lib IndexStore wrapper | Phase 2, stable jj-lib pin | ~400 lines |
| 5 | Custom revset engine | Phase 4, scale demands it | ~3-5k lines |

Phases 1-3 are daemon-only, no jj-lib coupling. Phase 4 is the wrapping
pattern. Phase 5 is only worth doing if repos hit a scale where the
default index is a bottleneck or if jj-lib opens up `CompositeIndex`.

## Cloudflare Workers compatibility

A segment index makes the Workers deployment more viable. With a segment
table, the team server answers ancestry queries without walking objects
in R2. The index itself is small (a few KB for thousands of commits) and
fits in Workers KV or a Durable Object. R2 round-trips are only needed
for actual content.

## References

- Meta's segmented changelog: presented at Git Merge, used in
  Sapling/Mononoke for O(1) ancestor queries on linear history
- jj-lib source: `jj_lib::index` (public traits), `jj_lib::default_index`
  (implementation, `CompositeIndex` is `pub(super)`)
- jj-lib pinned at 0.40.0 — trait surface documented above is from that
  version
