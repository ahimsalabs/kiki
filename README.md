# kiki

A virtual filesystem for your repos. Built on
[jj](https://jj-vcs.github.io/jj/latest/), stored as git.

```bash
kiki clone git@github.com:myorg/myproject.git
cd /mnt/kiki/myproject/default
vim src/main.rs       # files appear on read, sync on write
```

> **Experimental.** Works end-to-end on Linux and macOS.
> Not yet ready for real projects.

## What is this

A daemon serves your repos through a single mount at `/mnt/kiki/`.
Each repo gets lightweight workspaces — files are fetched lazily on
read and synced to a remote store in the background. No checkout
step, no full clone. Multiple workspaces share a single git object
store, so creating a new workspace is instant.

Same idea as Google's
[CitC](https://abseil.io/resources/swe-book/html/ch16.html#clients_in_the_cloud_citc)
or Meta's
[EdenFS](https://github.com/facebook/sapling/tree/main/eden/fs),
but built on jj and open source.

## jj superset

The `kiki` binary wraps `jj` — every jj command works unchanged.
kiki adds a daemon, a virtual filesystem layer, remote sync, and
managed workspaces.

```bash
kiki log                        # this is jj log
kiki new -m "add feature"       # this is jj new
kiki describe -m "fix auth bug" # this is jj describe
```

Top-level kiki commands handle repo and workspace lifecycle:

```bash
kiki clone git@github.com:org/repo.git  # clone into /mnt/kiki/repo/default
kiki workspace create repo/fix  # new workspace at /mnt/kiki/repo/fix
kiki workspace list repo        # list workspaces
kiki workspace delete repo/fix  # remove a workspace
```

The `kk` subcommand handles other kiki-specific operations
(`kk status`, `kk daemon`). `kk init` remains available for
ad-hoc mounts outside the managed namespace.

## Git-native storage

The content store is a bare git repo — git objects addressed by
SHA-1, managed by jj-lib's `GitBackend`. There is no custom
format and no translation layer.

This means:
- `kiki git push` sends objects to GitHub with zero conversion
- Teammates without kiki just `git clone`
- Every remote type (`dir://`, `s3://`, `kiki+ssh://`, `kiki://`, git forges)
  stores identical bytes — same objects, different transport
- Stock git tools (`git log`, `git blame`) work against mounts
  via a synthesized `.git` pointer

## Architecture

```
kiki (CLI)
  │  gRPC over Unix socket
  ▼
daemon
  ├─ RootFs        /mnt/kiki/<repo>/<workspace>/ namespace
  ├─ GitBackend    bare git repo (content store, shared per repo)
  ├─ RemoteStore   dir:// · s3:// · kiki+ssh:// · kiki:// (grpc)
  └─ VFS           FUSE (Linux) · NFS (macOS)
```

The daemon auto-starts on first command and runs in the
background. A single FUSE mount at `/mnt/kiki/` serves all repos
and workspaces. `kiki kk daemon status` shows what's running.

## Status

**Working:** read/write/snapshot, FUSE and NFS mounts, background
sync, multi-machine sharing via `dir://` / `s3://` / `kiki+ssh://` / `kiki://`,
git push and fetch to GitHub/GitLab, operation log sharing,
`.gitignore`-aware VFS, daemon lifecycle.

**In progress:** [managed workspaces](./docs/M12-WORKSPACES.md)
(`kiki clone`, `kiki workspace`, single-mount RootFs namespace),
async offline push queue.

**Designed:** [code review](./docs/REVIEW.md),
[auth](./docs/AUTH.md),
[ref protection](./docs/REF_PROTECTION.md).

## Build

Requires Rust (stable) and `libfuse3-dev` (Linux) or Xcode
command-line tools (macOS).

```bash
cargo build --release
# binary at target/release/kiki
```

See the **[User Guide](./docs/USER_GUIDE.md)** for a full
walkthrough. The **[design docs](./docs/)** cover the roadmap and
architecture decisions.

## License

TBD
