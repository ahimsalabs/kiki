# Code Review, Approvals, and Ref Protection

Design for kiki's native review system: approvals, OWNERS, ref
protection rules, and the `land` operation. Built on jj's change-
centric data model rather than the branch/PR metaphor.

**Depends on:** identity envelope (`AUTH.md`), git convergence.

**Companion doc:** [`AUTH.md`](./AUTH.md) — identity, authentication,
daemon-to-daemon trust.

## Core idea

The unit of review is the **change**, not a branch. jj already gives
changes a stable identity (change ID) that survives rebases,
descriptions, and amendments. kiki adds:

- A declared **destination** (where you intend to land)
- **Approvals** (signed assertions that a change is ready)
- **Protection rules** (what's required before a ref advances)
- **Ownership** (who must approve changes to which paths)

No branches, no PRs, no force-push/rebase-invalidation dance.

## Data model

All review data is stored as content-addressed blobs in the CAS,
referenced by refs. This means review state **syncs between daemons**
through the existing `RemoteStore` replication — no separate database.

### Submissions

A submission declares "I want this change to land on this ref."

```
Submission {
  change_id: ChangeId,
  commit_id: CommitId,        // head at time of submit
  destination: String,        // "main", "release/v2"
  author: String,             // identity envelope (email)
  timestamp: DateTime,
  signature: Bytes,
}
```

Stored at: `refs/submissions/<change_id>`

When the author revises their change, they re-submit (CAS the
submission ref to a new blob with the updated commit_id). Old
submission blobs remain in the CAS for audit.

### Approvals

An approval is a signed assertion: "I reviewed this specific
revision of this change and it's good."

```
Approval {
  change_id: ChangeId,
  commit_id: CommitId,        // the specific revision reviewed
  approver: String,           // identity envelope (email)
  attestor: String,           // who signed (daemon or self)
  path_snapshots: Vec<(PathBuf, ContentHash)>,
                              // content hashes of paths the approver
                              // owns at approval time — enables O(1)
                              // staleness checks without tree diffs
  timestamp: DateTime,
  signature: Bytes,
}
```

Stored at: `refs/approvals/<change_id>/<approver>`

Multiple approvers each get their own ref. Re-approving (after a
revision) overwrites the ref via CAS.

**Scoping:** Approval blobs are repo-wide — `cas_ref` enforcement
must see all of them regardless of caller. But storage is repo-wide,
queries are scoped. See [Query scoping](#query-scoping) below.

### Ref namespace

```
refs/
  main                                → commit
  release/v2                          → commit
  submissions/<change_id>             → submission blob
  approvals/<change_id>/<identity>    → approval blob
```

Clean, flat, queryable. `list_refs("submissions/")` shows all
pending work. `list_refs("approvals/kkmpxuqz/")` shows all
approvals for a change.

## Protection rules

### `.kiki/protect.toml`

Lives in the repo root, version-controlled, syncs like code.
Evaluated from the **current tip of the target ref** (not from
the incoming commit — you can't weaken your own guardrails in the
same change that exploits the weakness).

```toml
[refs."main"]
append_only = true           # new value must descend from old
required_reviewers = 1       # N approvals required to land

[refs."release/*"]
immutable = true             # no writes once set

[refs."experimental/*"]
# no restrictions
```

#### Rule types

| Rule | Meaning | Identity needed? |
|------|---------|-----------------|
| `immutable` | Ref cannot be advanced after initial set | No |
| `append_only` | New value must be a descendant of old | No |
| `required_reviewers = N` | N valid approvals needed to land | Yes |

`immutable` and `append_only` work without identity — they apply
to everyone equally. `required_reviewers` requires the identity
and approval infrastructure from `AUTH.md`.

### Changes to `protect.toml`

Changes to protection rules on a protected ref require the same
(or stricter) approval as the ref itself. A `[meta]` section
allows specifying who can modify the protection config:

```toml
[meta]
owners = ["group:admin"]
# Changes to this file require approval from an admin
```

If no `[meta]` section, changes to `protect.toml` follow the same
rules as any other path (i.e., OWNERS for the `.kiki/` directory).

## Path ownership (OWNERS)

### File format

```toml
# .kiki/owners/OWNERS (repo-wide default)
owners = ["group:backend"]

# .kiki/owners/src/crypto/OWNERS
owners = ["alice@co.com", "bob@co.com"]

# .kiki/owners/src/api/OWNERS
owners = ["group:backend", "carol@co.com"]
```

### Resolution

When a protected ref requires reviewers, the daemon:

1. Diffs the trees at `old` (current tip) and `new` (proposed)
2. For each changed path, walks up the directory tree looking for
   the nearest OWNERS file
3. Collects the set of required owners for each changed path
4. Checks that at least one valid approval exists from an owner of
   each path

Example: a commit changes `src/crypto/hmac.rs` and `src/api/auth.rs`.
- `src/crypto/hmac.rs` → nearest OWNERS is `src/crypto/OWNERS`
  → requires alice or bob
- `src/api/auth.rs` → nearest OWNERS is `src/api/OWNERS`
  → requires group:backend or carol

If Bob approved the commit, that satisfies `src/crypto/` (Bob is
listed). If Bob is also in group:backend, it satisfies `src/api/`
too. One approval can cover multiple paths if the approver is an
owner of all changed paths.

### OWNERS resolution uses the target ref's tree

OWNERS files are read from `old` (the ref being merged INTO), not
from the incoming commit. This prevents an attacker from modifying
OWNERS to add themselves and landing in the same change.

## Approval freshness and carry-forward

When Alice revises her change after Bob approved:

- The change ID stays the same
- The commit ID changes (new revision = new hash)
- Bob's approval blob references the old commit ID

**Question:** is Bob's approval still valid?

**Rule: an approval carries forward if the approver's owned paths
are byte-identical between the approved commit and the current
commit.**

The daemon diffs the tree at `approved_commit_id` against the tree
at `current_commit_id`, scoped to the paths the approver owns. If
none of those paths changed, the approval carries.

### Scenarios

| Scenario | Approval carries? |
|----------|------------------|
| Mechanical rebase (no conflicts, no owned-path changes) | Yes |
| Alice fixes a typo in a file Bob doesn't own | Yes |
| Alice modifies a file Bob owns | No — Bob must re-review |
| Alice rebases and resolves a conflict in Bob's file | No |

### Showing staleness

```bash
kiki kk approvals kkmpxuqz
#  bob@co.com  approved v2 (abc123)  ✓ carries to v3 (def456)
#  carol@co.com  approved v1 (789fed)  ✗ stale — src/api/auth.rs changed
```

```bash
# Bob can see what changed since his last approval:
kiki diff -r kkmpxuqz --since-approval bob@co.com
```

## The CLI

### Submit a change for review

```bash
kiki kk submit -d main
```

Registers the current change as targeting `main`. Creates (or
updates) the submission ref.

Options:
- `-d <ref>` — destination ref (required)
- `--draft` — mark as not ready for review (informational)

### View pending submissions

```bash
kiki kk pending
#  kkmpxuqz  alice@co.com  "fix auth bug"        → main  (awaiting review)
#  zztnwpqr  carol@co.com  "add metrics"         → main  (1/1 approved)
#  nnvvwxyz  dave@co.com   "update deps"         → main  (draft)
```

### Approve a change

```bash
kiki kk approve kkmpxuqz
```

Creates a signed approval blob for the current commit of the
specified change.

### View approvals

```bash
kiki kk approvals kkmpxuqz
#  bob@co.com    approved abc123 (v2)  2h ago   ✓ current
#  carol@co.com  approved 789fed (v1)  1d ago   ✗ stale
```

### See what changed since an approval

```bash
kiki diff -r kkmpxuqz --since-approval
# Shows diff between approved commit and current commit
# Scoped to the caller's owned paths if --mine flag is passed
```

### Land a change

```bash
kiki kk land kkmpxuqz
```

The merge button equivalent. Mechanically:

1. Resolve the current commit for the change
2. Verify it's submitted with a destination
3. Read `protect.toml` from the destination ref's current tip
4. If `append_only`: verify new commit descends from current tip
   (rebase onto tip if needed, or reject)
5. If `required_reviewers`: check approval coverage (see below)
6. Diff trees, resolve OWNERS, verify approvals cover all paths
7. `cas_ref(destination, old_tip, new_commit)` — advance the ref
8. Update submission status to `landed`

If the destination advanced since submit, `land` can optionally
auto-rebase before landing (configurable):

```toml
# .kiki/protect.toml
[refs."main"]
auto_rebase_on_land = true   # rebase onto tip before landing
```

### Withdraw a submission

```bash
kiki kk withdraw kkmpxuqz
```

Removes the submission ref. Approvals remain in the CAS (they're
immutable signed assertions) but become orphaned.

## Full workflow example

```bash
# Alice starts work
kiki new -m "fix auth token expiry"
# ... edits src/api/auth.rs and src/api/middleware.rs ...

# Alice submits for review
kiki kk submit -d main

# Bob sees it in pending
kiki kk pending
#  kkmpxuqz  alice  "fix auth token expiry"  → main  (awaiting review)

# Bob reviews
kiki diff -r kkmpxuqz
# Looks good, but wants a small change

# Alice revises (normal jj workflow)
kiki describe -m "fix auth token expiry (handle refresh case)"
# commit_id changes, change_id stays, predecessor chain tracks it

# Bob reviews the delta
kiki diff -r kkmpxuqz --since-approval bob@co.com
# (shows nothing — Bob hasn't approved yet, shows full diff)

# Actually, Bob just reviews the current state
kiki diff -r kkmpxuqz
# Looks good now

# Bob approves
kiki kk approve kkmpxuqz

# Alice lands
kiki kk land kkmpxuqz
# daemon: checks protect.toml (main requires 1 reviewer)
#         diffs trees: src/api/auth.rs and src/api/middleware.rs changed
#         resolves OWNERS: src/api/OWNERS lists group:backend
#         checks bob@co.com is in group:backend
#         bob's approval is for current commit_id ✓
#         cas_ref("main", old, new) → success
# Output: "Landed kkmpxuqz on main"
```

## Stacked changes

jj naturally supports stacked changes (A depends on B depends on
C). The review system handles this:

```bash
# Alice has a stack: A → B → C (C is newest)
kiki kk submit -d main   # submits C (the tip)
# But A and B need review too

# Submit the whole stack
kiki kk submit -d main -r 'kkmpxuqz::'  # submit change and descendants
```

When A lands:
1. `cas_ref("main", old, A)` advances main
2. B and C auto-rebase onto new main (jj does this in the op log)
3. B's submission is still active, now targeting the new main tip
4. Approvals on B carry if owned paths didn't change in the rebase

Landing is sequential: A lands, then B can land, then C. Each
`land` is an independent `cas_ref` with its own protection checks.

## Garbage collection

Over time, `refs/submissions/*` and `refs/approvals/*` accumulate.
Cleanup policy:

- **Landed changes:** submission and approval refs can be pruned
  after N days. The blobs remain in CAS until normal blob GC
  runs (unreferenced blobs get cleaned up).
- **Withdrawn changes:** refs removed immediately by `withdraw`.
  Blobs GC'd normally.
- **Stale submissions:** changes that haven't been updated in
  N days could be flagged or auto-withdrawn (configurable).

```toml
# .kiki/protect.toml
[gc]
prune_landed_after_days = 30
auto_withdraw_stale_days = 90
```

## Interaction with jj's immutable_heads()

jj has a client-side concept of immutable revisions (configured
via `immutable_heads()` revset in jj config). This prevents the
user from rewriting commits that have "landed."

kiki's protection is **server-side** — it prevents unauthorized
ref advances regardless of client config. The two are complementary:

- `immutable_heads()` — client-side guardrail, prevents accidental
  rewrites of landed history (UX convenience)
- `protect.toml` — server-side enforcement, prevents unauthorized
  ref advances (security boundary)

Recommendation: configure `immutable_heads()` in the repo's
jj config to match `protect.toml` rules. This gives users fast
client-side feedback ("you can't rewrite this") without waiting
for a server rejection.

## Query scoping

In a monorepo with many teams, `list_refs("approvals/")` and
`kiki kk pending` would return thousands of entries, mostly
irrelevant. Storage is repo-wide (enforcement needs full
visibility), but every query surface is scoped.

Three scoping mechanisms, layered:

### OWNERS-derived (implicit default)

The daemon knows the caller's identity and which OWNERS groups they
belong to. Default query behavior: show changes that touch paths
the caller owns.

```bash
kiki kk pending
# Only shows submissions where changed paths overlap with my OWNERS scope
```

### Path prefix (explicit)

For browsing or cross-team visibility:

```bash
kiki kk pending --scope "src/payments/**"
kiki kk approvals xyz --scope "src/api/**"
```

### Workspace-derived (automatic)

If the workspace has a declared scope (see
[`WORKSPACES.md`](./WORKSPACES.md)), use it as the default filter.
A workspace scoped to `src/payments/` automatically filters queries
to that subtree.

```toml
# workspace config or mount metadata
[scope]
paths = ["src/payments/**"]
```

`kiki kk pending` from that workspace implicitly means
`kiki kk pending --scope "src/payments/**"`. The `--scope` flag
overrides the workspace default.

### Implementation

`ListApprovals` and `ListSubmissions` RPCs each gain an optional
`repeated string scope` field (path patterns). The daemon diffs
each change's tree against the scope and skips non-overlapping
entries. When no explicit scope is provided, the daemon falls back
to the OWNERS-derived scope for the authenticated caller, then to
the workspace scope if set.

Enforcement at `cas_ref` time is always unscoped — full visibility.

## Open questions

1. **Review comments.** This design covers approvals (yes/no) but
   not inline comments or threaded discussions. Those could be
   another blob type (`refs/comments/<change_id>/<identity>/<seq>`)
   but the UX for viewing them in a terminal is unclear. Maybe
   this is where a web UI layer makes sense.

2. **Requested reviewers.** Should the submitter be able to request
   specific reviewers? Probably yes — as metadata on the submission
   blob. The daemon could notify (webhook, email) requested
   reviewers.

3. **Auto-approve / trust levels.** Some changes (typo fixes,
   dependency bumps) might not need human review. Could add a
   `trivial = true` flag or let OWNERS files specify paths that
   are auto-approved.

4. **CI integration.** Protection rules could require CI to pass
   before landing. This would mean the daemon checks for a
   CI-status blob/ref in addition to human approvals. Shape TBD.

5. **Merge vs. rebase landing.** When landing, should the change be
   rebased onto the tip (linear history) or merged? jj supports
   both. Probably configurable per-ref in `protect.toml`.

6. **Partial approval.** If a commit touches files owned by two
   different teams, can one team approve their portion? Current
   design says yes — each owner's approval covers their paths
   independently.
