# Ref Protection

Server-side enforcement of bookmark/ref rules on kiki's `cas_ref` RPC.
No identity or authentication required — these rules apply to all
callers unconditionally.

**Depends on:** nothing (implementable today).
**Depended on by:** [APPROVALS.md](./APPROVALS.md) (`required_reviewers`
needs this enforcement layer).

## Problem

Today, any client that can reach the daemon's gRPC port can advance any
ref to any value. A stray `kiki bookmark set main -r @` from the wrong
machine silently overwrites the team's main bookmark. There's no
mechanism to say "main is append-only" or "release tags are immutable."

## Design

### Policy source

Protection rules live in `.kiki/protect.toml` at the root of the repo
tree. The file is version-controlled alongside the code it protects.

When a client calls `cas_ref(ref, old, new)`, the daemon reads
`.kiki/protect.toml` from the tree at `old` — the **current** tip of the
ref being advanced. Policy is always evaluated from the existing state,
not the incoming commit. This prevents a single push from weakening
protection rules and exploiting the weakness in the same operation.

If `old` is the zero ID (ref doesn't exist yet), no protection applies —
you can always create a new ref. Protection kicks in on subsequent
advances.

If `.kiki/protect.toml` doesn't exist in the tree, all refs are
unprotected (current behavior).

### Fallback: daemon config

For bootstrapping or environments where the repo doesn't yet contain a
policy file, the daemon also supports protection rules in `daemon.toml`:

```toml
# daemon.toml
[refs.protection]
immutable = ["release/*"]
append_only = ["main"]
```

When both sources exist, the daemon merges them: the **union** of all
rules applies. Daemon config cannot weaken in-repo rules — it can only
add restrictions.

### Rule types

#### `immutable`

```toml
[refs."release/*"]
immutable = true
```

The ref cannot be advanced once set. Any `cas_ref` is rejected
unconditionally. The ref can still be created (first write), but once
it has a value, it's frozen.

Use case: release tags, published versions.

#### `append_only`

```toml
[refs."main"]
append_only = true
```

The new value must be a descendant of the old value. The daemon walks the
parent chain of `new` looking for `old`. If `old` is not an ancestor of
`new`, the advance is rejected.

This prevents force-pushes, rebases over main, and history rewrites on
protected branches. Normal forward advances (merges, new commits) are
allowed.

Walk depth is bounded to prevent DoS. If the ancestor check exceeds a
configurable limit (default: 10,000 commits), the advance is rejected
with a specific error.

#### `required_reviewers` (future, needs identity)

```toml
[refs."main"]
append_only = true
required_reviewers = 1
```

The ref advance requires N approval attestations from distinct
identities. See [APPROVALS.md](./APPROVALS.md) for the approval
mechanism.

### Pattern matching

Ref names match against patterns using glob syntax:

- `main` — exact match
- `release/*` — single-level wildcard
- `feature/**` — multi-level wildcard

Patterns are evaluated in order. The first matching rule wins. If no
pattern matches, the ref is unprotected.

### Full example

```toml
# .kiki/protect.toml

[refs."main"]
append_only = true
required_reviewers = 1       # future: needs APPROVALS.md

[refs."release/*"]
immutable = true

[refs."experimental/*"]
# explicitly unprotected — listed for documentation

# Changes to this file on a protected ref require the same approval
# as the ref itself. No special meta-rule needed — the file is part
# of the tree, so it's covered by the ref's protection.
```

## Enforcement point

All enforcement happens inside the daemon's `cas_ref` handler in
`service.rs`. The client (kiki CLI, jj commands, peer daemons) never
evaluates policy — it submits a ref advance and the daemon accepts or
rejects.

This is important: a misbehaving or modified client cannot bypass
protection. The daemon is the sole authority for its own refs.

### Pseudocode

```
fn cas_ref(ref, old, new):
    policy = read_protect_toml(tree_at(old))
    policy.merge(daemon_config.refs.protection)

    rule = policy.match(ref)
    if rule is None:
        return accept()             # unprotected

    if rule.immutable:
        return reject("ref is immutable")

    if rule.append_only:
        if not is_ancestor(old, new, max_depth=10000):
            return reject("not a descendant; ref is append-only")

    if rule.required_reviewers > 0:
        check_approvals(ref, old, new, rule)  # see APPROVALS.md

    return accept()
```

## Error responses

Rejections must be structured and actionable. The gRPC response includes:

```protobuf
message CasRefReply {
  bool accepted = 1;
  RefRejection rejection = 2;  // set when accepted = false
}

message RefRejection {
  string ref_name = 1;
  string rule = 2;              // "immutable", "append_only", "requires_review"
  string reason = 3;            // human-readable explanation
  string hint = 4;              // actionable next step
}
```

Example rejection formatted by the CLI:

```
Error: bookmark advance rejected
  ref: main (append_only)
  reason: commit abc123 is not a descendant of current tip def456
  hint: rebase your change onto main first: kiki rebase -d main
```

## Implementation sequence

1. **Parse `.kiki/protect.toml` from a tree ID.** Add a helper to
   `service.rs` that reads the file from the tree at a given root,
   returns parsed config or empty (no file = no rules).

2. **`immutable` check in `cas_ref`.** Reject unconditionally if the
   matched rule says `immutable = true`. Simplest possible check.

3. **`append_only` ancestry check.** Walk the commit parent chain from
   `new` looking for `old`. Post-git-convergence this is a git object
   graph walk via gix.

4. **Daemon config fallback.** Parse `[refs.protection]` from
   `daemon.toml` and merge with in-repo rules.

5. **Structured error responses.** Extend the `CasRefReply` proto with
   rejection details.

6. **CLI formatting.** Pretty-print rejections with context and hints.

Steps 1–2 are ~50 lines and can land immediately. Step 3 depends on
the daemon being able to walk the commit graph, which lands naturally
with git convergence.
