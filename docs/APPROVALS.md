# Change-Scoped Approvals

A review and approval system for kiki, built on jj's change IDs.
Approvals track the **change**, not the commit — they survive rebases
automatically and go stale only when the reviewed content actually
changes.

**Depends on:** [REF_PROTECTION.md](./REF_PROTECTION.md) (enforcement
layer), authentication (identity of the approver), git convergence
(commit graph walking for OWNERS resolution).

## Problem

When `required_reviewers = 1` is set on a protected ref, the daemon
needs to answer: "has a qualified person reviewed this change?" That
requires three things kiki doesn't have today:

1. **An approval primitive** — a way for someone to say "I reviewed this."
2. **Ownership rules** — who counts as a qualified reviewer for which
   paths.
3. **Staleness detection** — knowing when a previous approval no longer
   covers the current state of the change.

GitHub solves this with PR approvals, CODEOWNERS, and "dismiss stale
reviews." But GitHub's model is commit-scoped: a force push (including
a harmless rebase) invalidates all approvals. Teams either re-review
identical code or disable stale-review dismissal and lose safety.

jj's change IDs fix this. A change ID is stable across rebases,
amends, and rewrites. kiki can scope approvals to the change ID and
detect staleness by content, not by commit identity.

## Design

### Approval storage

The team daemon maintains an approvals table in its redb store:

```
approvals:
  (change_id, approver_identity) → ApprovalRecord
```

```rust
struct ApprovalRecord {
    /// Commit ID at the time of approval (for staleness checks).
    approved_commit: CommitId,
    /// Content hashes of paths the approver is an owner of,
    /// at the time of approval.
    path_snapshots: Vec<(PathBuf, ContentHash)>,
    /// When the approval was recorded.
    timestamp: DateTime,
}
```

Approvals are local to the daemon — they don't replicate to remotes.
The team daemon is the approval authority, just as GitHub is the
approval authority for GitHub PRs. This is intentional: approval is a
social/organizational fact, not a content fact.

### Approval RPC

```protobuf
service JujutsuInterface {
  // ... existing RPCs ...

  // Record an approval for a change.
  rpc Approve(ApproveReq) returns (ApproveReply);

  // List approvals for a change.
  rpc ListApprovals(ListApprovalsReq) returns (ListApprovalsReply);
}

message ApproveReq {
  string working_copy_path = 1;
  // Change ID (hex string). The daemon resolves this to the current
  // commit ID at approval time.
  string change_id = 2;
  // Identity is derived from the authenticated connection (mTLS,
  // signed request, etc.), not from this message.
}

message ApproveReply {
  // The commit ID the approval was recorded against.
  bytes approved_commit = 1;
}

message ListApprovalsReq {
  string working_copy_path = 1;
  string change_id = 2;
}

message ListApprovalsReply {
  repeated Approval approvals = 1;
}

message Approval {
  string approver = 1;
  bytes approved_commit = 2;
  string timestamp = 3;
  bool stale = 4;           // daemon pre-computes staleness
  string stale_reason = 5;  // which paths changed, if stale
}
```

### CLI surface

```bash
# Approve a change (by change ID)
kiki kk approve xzy

# List approvals (shows staleness)
kiki kk approvals xzy
# alice@co.com  approved 2h ago  (current)
# bob@co.com    approved 1d ago  (stale: src/api/auth.rs changed)

# Approve with explicit commit pinning (for automation/CI)
kiki kk approve xzy --commit abc123
```

### OWNERS files

Path-level ownership follows the CODEOWNERS pattern: per-directory
files that declare who must approve changes to that subtree.

```
.kiki/
  protect.toml           # ref protection rules
  OWNERS                 # repo-wide default owners (optional)

src/
  crypto/
    OWNERS               # owners for src/crypto/**
  api/
    OWNERS               # owners for src/api/**
```

#### OWNERS format

```toml
# src/crypto/OWNERS
owners = [
  "alice@co.com",
  "bob@co.com",
]

# Optional: require ALL owners, not just one.
# Default is "any" (at least one owner must approve).
require = "any"    # or "all"
```

#### Resolution

When the daemon evaluates `cas_ref("main", old, new)` with
`required_reviewers = 1`:

1. **Diff trees.** Compute the set of changed paths between `old` and
   `new`.

2. **Walk OWNERS.** For each changed path, walk up the directory tree
   in `old`'s tree (the **target ref's current state**) looking for an
   `OWNERS` file. The nearest ancestor OWNERS file applies. If no
   OWNERS file exists for a path, any authenticated user's approval
   satisfies the requirement.

3. **Check approvals.** For each OWNERS-covered path group, check the
   approvals table for the change ID being advanced. An approval
   satisfies if:
   - The approver is listed in the OWNERS file, AND
   - The approval is **not stale** for the paths that approver owns
     (see staleness below).

4. **Accept or reject.** If every changed path with an OWNERS file has
   at least one non-stale owner approval (or `required_reviewers`
   non-stale approvals from any authenticated user for uncovered
   paths), accept. Otherwise reject with details.

### Staleness detection

This is the core advantage over commit-scoped approvals.

When the daemon checks whether Bob's approval of change `xyz` is still
valid:

1. Look up Bob's `ApprovalRecord` for change `xyz`. It contains
   `approved_commit` and `path_snapshots` — the content hashes of
   Bob's OWNERS paths at approval time.

2. Resolve change `xyz` to its **current** commit ID. (The change may
   have been rebased since Bob approved.)

3. Read the content of Bob's OWNERS paths in the current commit's tree.

4. Compare content hashes.
   - **All paths unchanged** → approval is current. Bob reviewed this
     exact content.
   - **Any path changed** → approval is stale. Bob needs to re-review.

This means:

| Scenario | Approval status |
|----------|----------------|
| Alice rebases, no content changes | **Current** — rebase doesn't invalidate |
| Alice rebases and edits unrelated files | **Current** — Bob's paths unchanged |
| Alice edits a file Bob owns | **Stale** — content Bob reviewed changed |
| Alice adds a new file in Bob's directory | **Stale** — new path under Bob's OWNERS |

### Self-approval

An author cannot approve their own change for OWNERS purposes. The
daemon checks that the approver identity differs from the commit
author. (If the change has multiple commits with different authors,
the approver must differ from the author of the most recent commit.)

This is configurable:

```toml
# .kiki/protect.toml
[refs."main"]
required_reviewers = 1
allow_self_approve = false  # default
```

### Edge cases

**Merge commits.** When `new` is a merge, the diff is computed against
`old` (the current tip), not against individual parents. All changed
paths need owner coverage.

**OWNERS file changes.** Changes to OWNERS files themselves are
governed by the OWNERS file one level up, or the repo-root
`.kiki/OWNERS`. If no parent OWNERS file exists, the ref's
`required_reviewers` count applies (any N authenticated users).

**Deleted OWNERS files.** Evaluated from `old`'s tree (current tip).
A commit that deletes an OWNERS file still needs approval from the
owners defined in that file — you can't delete your own gatekeepers.

**Change ID conflicts.** jj change IDs are random 128-bit values;
collisions are not a practical concern.

**Multiple changes in one bookmark advance.** If `new` is several
commits ahead of `old`, each intermediate change ID that introduced
path modifications under an OWNERS file must have approvals. The
daemon walks the commit chain from `old` to `new`, collects all
change IDs, and checks each one.

## Error responses

Structured, actionable rejections:

```
Error: bookmark advance rejected
  ref: main (append_only, requires 1 codeowner)
  changed paths needing approval:
    src/api/auth.rs
      owners: alice@co.com, bob@co.com
      approvals: bob@co.com (stale — file changed since approval)
    src/api/routes.rs
      owners: alice@co.com, bob@co.com
      approvals: (none)
  hint: ask a codeowner to review and run: kiki kk approve xyz
```

## Identity model

Approvals require knowing who's calling. The daemon needs
authentication before this feature lands. Likely options:

- **mTLS on the gRPC connection.** Client certificates identify the
  user. The daemon extracts the identity from the certificate's
  subject. Simple, proven, works for team servers.
- **Signed requests.** Each RPC carries a signature from the user's
  private key. The daemon verifies against a trust store. More
  flexible but more complex.
- **External identity provider.** OIDC/OAuth token in the gRPC
  metadata. The daemon validates the token. Integrates with existing
  SSO.

The approval system is agnostic to the identity mechanism — it only
needs a verified string identity (email, username) per request. The
identity layer is a separate design concern.

## Implementation sequence

1. **`Approve` and `ListApprovals` RPCs.** Wire up the redb table,
   record approvals keyed by change ID. Requires identity on the
   connection (even a simple shared-secret-per-user is enough to
   start).

2. **OWNERS file parsing.** Read and parse OWNERS files from a tree
   ID. Walk-up resolution from a path to its governing OWNERS file.

3. **Staleness detection.** On `ListApprovals`, compute staleness by
   comparing path content hashes between the approved commit and the
   current commit for the change.

4. **Wire into `cas_ref`.** When a protected ref has
   `required_reviewers > 0`, run the full check: diff paths, resolve
   OWNERS, verify approvals, check staleness.

5. **CLI formatting.** `kiki kk approve`, `kiki kk approvals`, and
   rich rejection messages from `bookmark set`.

6. **Self-approval check.** Verify approver != author.

Steps 1–3 can be developed and tested independently of ref protection.
Step 4 connects the two systems.
