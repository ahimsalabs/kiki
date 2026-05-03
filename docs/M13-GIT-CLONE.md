# M13: First-class git clone and the dual-remote model

Spec for making git URLs work as first-class clone sources, introducing
`kiki remote` as the kiki-native remote management surface, and
renaming `ssh://` to `kiki+ssh://` to free the scheme for git.

Depends on: git convergence (landed), M12 managed workspaces (active).

## Problem statement

kiki currently requires a kiki-native remote store (`dir://`, `s3://`,
`kiki+ssh://`, `kiki://`) to clone a repo. Working with GitHub or any
git forge is a two-step afterthought: clone from a kiki remote, then
`kiki git remote add origin git@github.com:...`. This is backwards —
most developers start from a git URL, and the kiki remote (if any) is
infrastructure they add later.

After git convergence, the content store is a real bare git repo.
There is no technical reason `kiki clone git@github.com:org/repo.git`
can't work directly.

## The dual-remote model

Every kiki repo has two orthogonal remote axes:

```
kiki remote (zero or one)         git remotes (zero or more)
├── s3://team-bucket/repo         ├── origin → github.com
├── kiki://team-server:12000      ├── upstream → github.com/fork
├── kiki+ssh://user@host/store    └── ...
├── dir:///shared/store
└── (none = local-only)
```

**Kiki remote** — the full-fidelity collaboration surface. Automatic
write-through / read-through. Carries git objects, jj operations,
views, catalog refs, extras (change-ids, predecessors), and git remote
metadata. Two kiki users sharing a kiki remote see each other's full
operation history, commit evolution, and workspace states. One per repo.

**Git remotes** — lossy publication channels for the git ecosystem.
Explicit push/fetch. Carry commits and refs only — no jj operations,
no change-ids, no predecessors, no workspace state. Named, multiple
per repo. Managed via `kiki git remote add/list/remove`.

The kiki remote is infrastructure; git remotes are where you publish to
the wider world. For a team that's all-kiki, you may never need a git
remote. For open-source work, the git remote (GitHub) is the
publication layer and the kiki remote is optional team infrastructure.

### Stock tool interop

The VFS already synthesizes a read-only `.git` gitdir file at every
workspace root, pointing at the bare git repo. Stock `git log`,
`git diff`, `git blame`, `jj log` all work read-only against any kiki
workspace. Writes must go through the kiki daemon (via the `kiki`
CLI) because the daemon owns the op store, extras table, and ref
management.

## Scheme rename: `ssh://` → `kiki+ssh://`

### Motivation

`ssh://` is universally understood as "git over SSH" by developers.
kiki currently uses it for kiki-native SSH remotes (tunnel to a
remote daemon). This creates ambiguity when adding git URL support
to `kiki clone`.

Renaming to `kiki+ssh://` follows the `svn+ssh://` precedent — reads
as "kiki protocol over SSH." It frees `ssh://` for its standard
meaning (git over SSH).

### Detection rule

After the rename, URL classification is unambiguous:

| URL pattern | Type | Handler |
|---|---|---|
| `dir://...` | kiki remote | `remote::parse()` |
| `s3://...` | kiki remote | `remote::establish_s3_remote()` |
| `kiki://...`, `grpc://...` | kiki remote | `remote::parse()` |
| `kiki+ssh://...` | kiki remote | `remote::establish_ssh_remote()` |
| `https://...`, `http://...` | git clone | `git fetch` subprocess |
| `git://...` | git clone | `git fetch` subprocess |
| `ssh://...` | git clone | `git fetch` subprocess |
| `git@host:path` (SCP-style) | git clone | `git fetch` subprocess |

The mental model: **kiki-specific schemes have `kiki` in them or are
cloud/filesystem storage. Everything else is git.**

### Blast radius

Three functional code points:

1. `daemon/src/remote/mod.rs:70` — scheme match arm `"ssh"` →
   `"kiki+ssh"`
2. `daemon/src/remote/mod.rs:91` — `strip_prefix("ssh://")` →
   `strip_prefix("kiki+ssh://")`
3. `daemon/src/remote/tunnel.rs:341` — `format!("ssh://...")` →
   `format!("kiki+ssh://...")`

Plus ~15 test strings, ~30 comment/doc-string updates, and docs
(USER_GUIDE.md, PLAN.md, M12-WORKSPACES.md, etc.). Mechanical,
no logic changes.

## `kiki clone` for git URLs

### CLI changes

`CloneCommandArgs` gains an optional `--remote` flag:

```rust
struct CloneCommandArgs {
    /// Clone source URL. Kiki remote (dir://, s3://, kiki://, kiki+ssh://)
    /// or git remote (https://, ssh://, git://, git@host:path).
    url: String,
    /// Repo name override (default: derived from URL).
    #[arg(long)]
    name: Option<String>,
    /// Symlink to create pointing at the workspace.
    #[arg(long)]
    link: Option<PathBuf>,
    /// Kiki remote for real-time sync (e.g. s3://bucket/repo,
    /// kiki://server:12000). Attaches a kiki-native remote alongside
    /// the git origin.
    #[arg(long)]
    remote: Option<String>,
}
```

### URL classification

A helper function classifies the clone URL:

```rust
enum CloneSource {
    /// Kiki-native remote store. The URL is the kiki remote; no git
    /// origin unless added later.
    KikiRemote(String),
    /// Git remote. The URL becomes the `origin` git remote; no kiki
    /// remote unless --remote is specified.
    Git(String),
}

fn classify_clone_url(url: &str) -> CloneSource {
    // SCP-style: no "://" and contains ":"  (e.g. git@github.com:org/repo)
    if !url.contains("://") && url.contains(':') {
        return CloneSource::Git(url.to_string());
    }
    match url.split("://").next().unwrap_or("") {
        "dir" | "s3" | "kiki" | "grpc" | "kiki+ssh" => {
            CloneSource::KikiRemote(url.to_string())
        }
        // Everything else is git: https, http, git, ssh, etc.
        _ => CloneSource::Git(url.to_string()),
    }
}
```

### Wire protocol

New RPC alongside the existing `Clone`:

```protobuf
rpc GitClone(GitCloneReq) returns (GitCloneReply) {}

message GitCloneReq {
    string git_url = 1;        // git remote URL (becomes "origin")
    string name = 2;           // repo name (empty = derive from URL)
    string kiki_remote = 3;    // optional kiki remote URL (empty = none)
}
message GitCloneReply {
    string workspace_path = 1;
    repeated GitFetchedBookmark bookmarks = 2;  // imported refs
    string default_branch = 3;  // e.g. "main"
}
```

A separate RPC (rather than extending `CloneReq`) because the flows
are substantially different. `Clone` establishes a kiki remote and
optionally fetches initial state from it. `GitClone` runs `git fetch`
from a forge and optionally attaches a kiki remote.

### Daemon-side flow (`service.rs`)

`git_clone` handler:

1. Derive or validate repo name. Same `derive_repo_name` /
   `validate_name` as existing `clone`.
2. Create repo directory, init `GitContentStore` + `store.redb`.
3. Add git remote: `git_ops::remote_add(git_path, "origin", &git_url)`.
4. Fetch: `git_ops::fetch(git_path, "origin")` → returns bookmark
   list.
5. Detect default branch:
   - Run `git symbolic-ref refs/remotes/origin/HEAD` (populated by
     `git fetch` when the remote advertises HEAD).
   - Strip `refs/remotes/origin/` prefix → `"main"`, `"master"`, etc.
   - Fallback: first of `main`, `master`, or first bookmark
     alphabetically.
6. If `kiki_remote` is non-empty: establish and register the kiki
   remote store (same logic as existing `clone` — SSH tunnel, S3,
   etc.).
7. If a kiki remote was attached: push all fetched git objects to it
   via `push_reachable_blobs`. Also store git remote metadata (see
   §Git remote metadata replication).
8. Register repo + default workspace with `RootFs`.
9. Persist `repos.toml`.
10. Return workspace path, bookmarks, and default branch name.

### CLI-side flow (`main.rs`)

`run_clone_command` for `CloneSource::Git`:

1. Connect to daemon.
2. Call `GitClone` RPC.
3. `Workspace::init_with_factories(...)` at `workspace_path` — same
   factory setup as today.
4. Import bookmarks into jj view:
   - Set local bookmarks from `reply.bookmarks` (as remote-tracking
     bookmarks for `origin`).
   - Check out the default branch commit.
5. Create symlink if `--link` specified.
6. Print workspace path and hint.

### Existing `kiki clone <kiki-url>` behavior

Unchanged. `CloneSource::KikiRemote` dispatches to the existing
`Clone` RPC. The `--remote` flag is rejected for kiki-remote clones
(the URL IS the kiki remote; you don't attach a second one).

### Combined clone: `kiki clone <git-url> --remote <kiki-url>`

Passes both URLs to the `GitClone` RPC. The daemon:

1. Creates the repo with the git origin.
2. `git fetch origin` — populates git objects.
3. Establishes the kiki remote.
4. Pushes all fetched objects to the kiki remote.
5. Stores git remote metadata on the kiki remote.

Result: the repo has both a git origin (explicit push/fetch) and a
kiki remote (automatic write-through/read-through). This is the
"team server" setup in a single command.

## `kiki remote` — kiki-native remote management

### Commands

```bash
# Attach a kiki remote to the current repo (inferred from cwd)
kiki remote add s3://team-bucket/project

# Remove the kiki remote
kiki remote remove

# Show the current kiki remote
kiki remote show
```

Repo is inferred from the working directory, same as every other kiki
command — the CLI sees you're in `/mnt/kiki/<repo>/<workspace>/` and
resolves the repo name.

### Wire protocol

```protobuf
rpc RemoteAdd(RemoteAddReq) returns (RemoteAddReply) {}
rpc RemoteRemove(RemoteRemoveReq) returns (RemoteRemoveReply) {}
rpc RemoteShow(RemoteShowReq) returns (RemoteShowReply) {}

message RemoteAddReq {
    string repo = 1;        // repo name
    string url = 2;         // kiki remote URL
}
message RemoteAddReply {}

message RemoteRemoveReq {
    string repo = 1;
}
message RemoteRemoveReply {}

message RemoteShowReq {
    string repo = 1;
}
message RemoteShowReply {
    string url = 1;         // empty = no kiki remote
}
```

### `RemoteAdd` flow

1. Validate: repo exists, no kiki remote already attached.
2. Establish the remote store (SSH tunnel, S3, gRPC, etc.).
3. Push all existing git objects to the kiki remote via
   `push_reachable_blobs`.
4. Push jj operations, views, catalog refs.
5. Store git remote metadata on the kiki remote (see below).
6. Update `RepoEntry.url` in `repos.toml`.
7. Register the `RemoteStore` with `RootFs` so subsequent writes
   flow through.

### `RemoteRemove` flow

1. Deregister the `RemoteStore` from `RootFs`.
2. Drop SSH tunnel if present.
3. Clear `RepoEntry.url` in `repos.toml`.
4. Repo continues to work local-only. Git remotes unaffected.

## Git remote metadata replication

### What gets stored

When a kiki remote is attached, the daemon stores git remote
configuration as repo-level metadata on the kiki remote. This is a
small blob:

```json
{
  "git_remotes": [
    {"name": "origin", "url": "git@github.com:org/repo.git"},
    {"name": "upstream", "url": "git@github.com:other/repo.git"}
  ]
}
```

Stored as a catalog ref (mutable ref in the `RemoteStore`), e.g.
`refs/kiki/git_remotes`. Updated on `kiki git remote add/remove`.

### Seeded on clone

When `kiki clone kiki://server:12000 --name repo` fetches from a
kiki remote, the daemon reads `refs/kiki/git_remotes` from the
remote. If present, it calls `git_ops::remote_add` for each entry
to configure the git remotes on the local bare repo. This is a
one-time seed — subsequent changes to git remotes are local and
are NOT pushed back unless the user explicitly has a kiki remote
attached (in which case `kiki git remote add` updates the ref).

### Propagation rules

- `kiki git remote add/remove` on a repo WITH a kiki remote →
  updates the local git config AND writes `refs/kiki/git_remotes`
  to the kiki remote.
- `kiki git remote add/remove` on a repo WITHOUT a kiki remote →
  updates local git config only.
- `kiki remote add` on a repo with existing git remotes → pushes
  current git remote config to the newly attached kiki remote.
- `kiki clone` from a kiki remote → seeds git remotes from the
  remote's `refs/kiki/git_remotes`. After that, local config.

This means: the first person to add a git remote and attach a kiki
remote "publishes" the git remote config. Everyone who subsequently
clones from that kiki remote gets the git remotes for free. After
the initial seed, each machine's git remote config is independent.

## End-to-end workflows

### Solo developer, GitHub-primary

```bash
kiki clone git@github.com:myorg/myproject.git
cd /mnt/kiki/myproject/default

# Work normally
mkdir src && echo 'fn main() {}' > src/main.rs
kiki describe -m "initial commit"
kiki new

# Push to GitHub (own credentials)
kiki git push --remote origin --bookmark main

# Fetch teammate's changes
kiki git fetch
kiki log
```

No kiki remote. Local-only storage. Git push/fetch for collaboration.
This is the "better git/jj client" story.

### Team with kiki server

```bash
# Dev A: set up the project
kiki clone git@github.com:myorg/myproject.git \
    --remote kiki://team-server:12000
cd /mnt/kiki/myproject/default

# Dev A: work — writes flow to kiki remote automatically
echo 'fn main() {}' > src/main.rs
kiki describe -m "initial commit"

# Dev A: push to GitHub when ready (own creds)
kiki git push --remote origin --bookmark main
```

```bash
# Dev B: join the project
kiki clone kiki://team-server:12000 --name myproject
cd /mnt/kiki/myproject/default

# git origin is already configured (seeded from kiki remote)
kiki git remote list
#   origin  git@github.com:myorg/myproject.git

# Dev B sees Dev A's full operation history, change-ids, etc.
kiki log

# Dev B: push to GitHub (own creds)
kiki git push --remote origin --bookmark feature-x
```

### Adding a kiki remote to an existing repo

```bash
cd /mnt/kiki/myproject/default
kiki remote show
#   (none)

kiki remote add s3://team-bucket/myproject
#   Pushing objects to s3://team-bucket/myproject...
#   Done. 142 objects, 3 operations synced.

kiki remote show
#   s3://team-bucket/myproject
```

## Changes to `repos.toml`

`RepoEntry` gains an optional `git_origin` field to distinguish
repos cloned from git vs. kiki:

```toml
next_slot = 3

[repos.myproject]
url = "kiki://team-server:12000"            # kiki remote (may be empty)
git_origin = "git@github.com:org/repo.git"  # informational, the source
                                            # of truth is the git config
                                            # in git_store/

[repos.localproject]
url = ""                                    # no kiki remote
```

The `git_origin` field is informational — the authoritative git
remote config lives in `git_store/config` (managed by `git remote
add/remove`). It exists so `kiki remote show` and status displays
can show the git origin without opening the git repo.

## Implementation sequence

1. **`kiki+ssh://` rename.** Mechanical find-and-replace across
   `remote/mod.rs`, `tunnel.rs`, `service.rs`, `repo_meta.rs`,
   `store/src/lib.rs`, `cli/src/main.rs`, proto comments, tests,
   and docs. No logic changes.

2. **URL classification.** `classify_clone_url()` function in the
   CLI. Unit tests for all URL patterns.

3. **`GitClone` RPC + daemon handler.** New proto message, handler
   in `service.rs` that does `git remote add` + `git fetch` +
   optional kiki remote attach. Reuses `git_ops::remote_add`,
   `git_ops::fetch`, existing `GitContentStore` init, existing
   `RootFs` registration.

4. **CLI `kiki clone` dispatch.** `run_clone_command` branches on
   `classify_clone_url()`. `Git` variant calls `GitClone` RPC,
   imports bookmarks into jj view, checks out default branch.
   `KikiRemote` variant calls existing `Clone` RPC (unchanged).

5. **`kiki remote add/remove/show` CLI + RPCs.** New
   `KikiSubcommand::Remote` variant. Three RPCs. `RemoteAdd`
   handler establishes remote store, pushes existing objects,
   updates `repos.toml`.

6. **Git remote metadata replication.** Store/read
   `refs/kiki/git_remotes` on the kiki remote. Seed on clone.
   Update on `kiki git remote add/remove` when a kiki remote is
   attached.

7. **`--remote` flag on `kiki clone`.** Passes to `GitCloneReq`.
   Combined flow in daemon handler.

8. **Doc updates.** USER_GUIDE.md "Working with GitHub" section
   simplified. PLAN.md milestone table updated (git convergence →
   done, add M13).

9. **PLAN.md update.** Mark git convergence and daemon lifecycle as
   done. Add M13 row.

## Open questions

1. **Default branch detection.** `git fetch` populates
   `refs/remotes/origin/HEAD` as a symbolic ref when the remote
   advertises it. If not present, fall back to `main` → `master` →
   first alphabetically. Should we also support
   `git ls-remote --symref <url> HEAD` before fetch for early
   detection? Probably not worth the extra network round-trip.

2. **Multiple kiki remotes.** This spec limits to one kiki remote
   per repo. Is there a use case for multiple? (e.g. S3 for
   durability + kiki:// for low-latency peer sync). Probably not
   for M13 — the ref arbitration model assumes a single authoritative
   remote. Supporting multiple would require conflict resolution
   between remotes. Defer.

3. **`kiki remote add` with existing data.** When attaching a kiki
   remote to a repo that already has content, the daemon must push
   all existing objects, operations, and refs. For large repos this
   could take a while. Should there be progress reporting? Probably
   yes — the CLI should stream progress from the daemon.

4. **Naming: `kiki remote` vs `kiki store`.** "remote" parallels
   `git remote` and implies network access, which is accurate for
   `s3://`, `kiki://`, `kiki+ssh://`. For `dir://` (local
   filesystem) it's a stretch, but `dir://` remotes are mainly for
   testing. "remote" is the right default.

5. **What happens to `kiki clone <kiki-url>` repos that want a git
   remote later?** Works today: `kiki git remote add origin <url>`.
   With M13, `kiki git remote add` also writes the git remote
   metadata to the kiki remote (if attached), so subsequent clones
   from the same kiki remote will inherit it. No new mechanism
   needed.
