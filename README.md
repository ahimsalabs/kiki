# kiki

A virtual filesystem for your repos. Built on
[jj](https://jj-vcs.github.io/jj/latest/), stored as git.

```bash
kiki kk init ssh://devbox/repos/myproject ~/work/myproject
cd ~/work/myproject
vim src/main.rs       # files appear on read, sync on write
```

> **Experimental.** Works end-to-end on Linux and macOS.
> Not yet ready for real projects.

## What is this

A daemon serves your repo as a mount point — FUSE on Linux, NFS
on macOS. Files are fetched lazily on read and synced to a remote
store in the background. No checkout step, no full clone.

Same idea as Google's
[CitC](https://abseil.io/resources/swe-book/html/ch16.html#clients_in_the_cloud_citc)
or Meta's
[EdenFS](https://github.com/facebook/sapling/tree/main/eden/fs),
but built on jj and open source.

## jj superset

The `kiki` binary wraps `jj` — every jj command works unchanged.
kiki adds a daemon, a virtual filesystem layer, and remote sync.

```bash
kiki log                        # this is jj log
kiki new -m "add feature"       # this is jj new
kiki describe -m "fix auth bug" # this is jj describe
```

The `kk` subcommand handles kiki-specific operations that would
collide with jj builtins (`kk init`, `kk status`, `kk daemon`).

## Git-native storage

The content store is a bare git repo — git objects addressed by
SHA-1, managed by jj-lib's `GitBackend`. There is no custom
format and no translation layer.

This means:
- `kiki git push` sends objects to GitHub with zero conversion
- Teammates without kiki just `git clone`
- Every remote type (`dir://`, `s3://`, `ssh://`, `kiki://`, git forges)
  stores identical bytes — same objects, different transport
- Stock git tools (`git log`, `git blame`) work against mounts
  via a synthesized `.git` pointer

## Architecture

```
kiki (CLI)
  │  gRPC over Unix socket
  ▼
daemon
  ├─ GitBackend    bare git repo (content store)
  ├─ RemoteStore   dir:// · s3:// · ssh:// · kiki:// (grpc)
  └─ VFS           FUSE (Linux) · NFS (macOS)
```

The daemon auto-starts on first command and runs in the
background. It manages the mount, the local object cache, and
background sync. `kiki kk daemon status` shows what's running.

## Status

**Working:** read/write/snapshot, FUSE and NFS mounts, background
sync, multi-machine sharing via `dir://` / `s3://` / `ssh://` / `kiki://`,
git push and fetch to GitHub/GitLab, operation log sharing.

**In progress:** `.gitignore`-aware VFS, async offline push queue,
daemon lifecycle (launchd/systemd auto-start).

**Designed:** [managed workspaces](./docs/WORKSPACES.md),
[code review](./docs/REVIEW.md),
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
