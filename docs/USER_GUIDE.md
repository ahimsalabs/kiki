# kiki User Guide

kiki is an experimental remote backend for [jj](https://jj-vcs.github.io/jj/latest/).
It serves the working copy as a virtual filesystem (FUSE on Linux, NFS on macOS)
backed by a daemon that handles storage, caching, and remote synchronization.

> **Status:** experimental. The core workflow (init, edit, commit, sync between
> peers) works end-to-end on Linux. macOS support is present but less tested.
> Not yet suitable for production repositories.

## Architecture overview

```mermaid
graph LR
    CLI["kiki<br/>(jj superset)"] -- "gRPC (UDS)" --> Daemon
    Daemon -- "dir:// / s3:// / ssh:// / kiki://" --> Remote["Remote Store"]
    Daemon -- mount --> VFS["Working copy<br/>(FUSE / NFS)"]
```

- **kiki** (`kiki`): A jj superset binary that talks to the local daemon over
  a Unix domain socket. Stores no persistent data itself. All standard jj
  commands (`kiki log`, `kiki new`, `kiki describe`, `kiki diff`, etc.) work
  normally. The `kk` subcommand provides kiki-specific operations.
- **Daemon**: Long-lived process on the local machine, auto-started on first
  command. Mounts repos as virtual filesystems, manages a durable per-mount
  store (redb), and optionally syncs blobs and operation state to a remote.
  No manual configuration needed.
- **Remote** (`dir://`, `s3://`, `ssh://`, or `kiki://`): Content-addressed
  blob store with compare-and-swap mutable refs. `s3://` remotes use the AWS
  SDK default credential chain. `ssh://` remotes need only the `kiki` binary on
  the server — the local daemon manages a persistent SSH tunnel to the remote
  daemon. `kiki://` remotes connect to a running daemon on another machine
  (e.g., over Tailscale).

## Prerequisites

- **Linux:** `fusermount3` (usually provided by the `fuse3` package). It ships
  as a setuid binary on most distros, so no `sudo` is needed to mount.
- **macOS:** `mount_nfs` (ships with macOS). No extra packages required, but
  loopback NFS has occasional version-specific quirks.
- **Rust toolchain:** edition 2024 (nightly or stable 1.85+).
- **jj** 0.40.x (jj-lib 0.40 is the pinned dependency).

## Building

```bash
cargo build --workspace            # debug build
cargo build --workspace --release  # release build
```

The workspace produces one binary:

| Binary | Location (debug)      | Description                          |
|--------|-----------------------|--------------------------------------|
| `kiki` | `target/debug/kiki`   | jj superset + daemon (unified binary)|

## Configuration

### Zero-config defaults

kiki requires **no configuration** for local use. The daemon is auto-started
when you run your first `kiki` command. It communicates over a Unix domain
socket at a platform-appropriate path:

| Platform | Socket path |
|----------|-------------|
| Linux    | `$XDG_RUNTIME_DIR/kiki/daemon.sock` (or `/tmp/kiki-$UID/daemon.sock`) |
| macOS    | `~/Library/Caches/kiki/daemon.sock` |

Storage lives at `~/.local/state/kiki` (Linux) or
`~/Library/Application Support/kiki` (macOS).

### Optional config file

For power users, `~/.config/kiki/config.toml` can override defaults:

```toml
# All fields are optional.

# TCP listener for daemon-to-daemon remote access (kiki:// scheme).
# Default: disabled.
# grpc_addr = "[::1]:12000"

# Override storage directory.
# storage_dir = "/path/to/storage"

# NFS port range (macOS only).
# [nfs]
# min_port = 12000
# max_port = 12010
```

### Environment overrides

| Variable | Effect |
|----------|--------|
| `KIKI_SOCKET_PATH` | Override socket location. Disables auto-start (user manages daemon). |
| `RUST_LOG` | Control daemon log verbosity (e.g., `info`, `debug`). |

## Getting started

### 1. Initialize a repository

The daemon starts automatically on your first command — no manual setup.

```bash
kiki kk init <remote> [destination]
```

**`<remote>`** is the remote store URL. Supported schemes:

| Scheme    | Example                          | Description |
|-----------|----------------------------------|-------------|
| `dir://`  | `dir:///tmp/kiki-remote`         | Filesystem-backed remote. Good for local testing and single-machine use. |
| `s3://`   | `s3://my-bucket/repos/project`   | S3-backed remote. Uses AWS SDK credentials and bucket permissions. |
| `ssh://`  | `ssh://user@host/data/store`     | SSH transport. Needs only the `kiki` binary on the server. The local daemon manages a persistent SSH tunnel. |
| `kiki://` | `kiki://myserver:12000`          | Another kiki daemon's gRPC endpoint. Enables peer-to-peer sync (e.g., over Tailscale). |
| `grpc://` | `grpc://[::1]:12000`             | Alias for `kiki://`. |
| (empty)   | `""`                             | No remote. Local-only operation with redb-backed storage. |

**`[destination]`** is the directory to create the repo in (default: `.`).

Examples:

```bash
# Local-only repo (no remote)
kiki kk init "" my-project

# Repo backed by a filesystem remote
kiki kk init "dir:///shared/kiki-store" my-project

# Repo backed by S3
kiki kk init "s3://my-bucket/repos/my-project" my-project

# Repo syncing over SSH (no daemon needed on the server)
kiki kk init "ssh://user@myserver/data/kiki-store" my-project

# Repo syncing to another daemon (e.g., over Tailscale)
kiki kk init "kiki://myserver:12000" my-project
```

On Linux, `kiki kk init` tells the daemon to FUSE-mount the working copy at the
destination directory. On macOS, the CLI shells out to `mount_nfs` after the
daemon sets up the NFS server.

### 3. Use standard jj commands

Once initialized, all standard jj commands work via the `kiki` binary:

```bash
cd my-project

# Create files (writes go through the VFS to the daemon)
mkdir src
echo 'fn main() {}' > src/main.rs

# Check status
kiki st

# Create a new change
kiki new

# View history
kiki log

# Describe the current change
kiki describe -m "add main.rs"

# View operation log
kiki op log

# List files at a revision
kiki file list -r @-

# Diff
kiki diff
```

The daemon snapshots the working copy automatically on each kiki command, just
like regular jj. The difference is that snapshots happen in the daemon's
in-memory inode slab and persist to the redb store, rather than scanning the
filesystem.

### 4. Daemon management

The daemon is invisible in normal use. For debugging:

```bash
kiki kk daemon status       # PID, socket path, mount count
kiki kk daemon logs         # tail the log file
kiki kk daemon logs -f      # follow (like tail -f)
kiki kk daemon stop         # graceful shutdown
kiki kk daemon socket-path  # print the resolved socket path
```

To see all mounted repositories:

```bash
kiki kk status
```

## Multi-user / multi-machine sync

When two CLIs point at the same remote (e.g., an `s3://` bucket prefix, a shared
`ssh://` server, a `dir://` path, or a `kiki://` peer), kiki serializes
operation-log advances via compare-and-swap on the remote's mutable ref catalog.
This means:

- **Blob sync:** Every write is pushed to the remote immediately
  (write-through). Reads fall through to the remote on local cache miss
  (read-through with verification).
- **Operation sync:** Operation and view data route through the daemon with
  write-through/read-through semantics. A peer CLI can read the full operation
  history that another CLI wrote.
- **Op-head arbitration:** The `op_heads` ref uses CAS retry so concurrent
  `kiki new` from two machines won't silently clobber each other's op head.

### Example: two machines sharing a dir:// remote

```bash
# Machine A
kiki kk init "dir:///shared/remote" project

# Machine B
kiki kk init "dir:///shared/remote" project

# Both machines see each other's commits and operations
```

### Example: two machines sharing an s3:// remote

Configure AWS credentials using the standard AWS SDK mechanisms, such as
`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`, `~/.aws/credentials`, SSO, ECS,
or instance metadata. Both machines must have permission to read, write, list,
and conditionally update objects under the same bucket prefix.

```bash
# Machine A
kiki kk init "s3://my-bucket/repos/project" project

# Machine B
kiki kk init "s3://my-bucket/repos/project" project

# Both machines see each other's commits and operations
```

### Example: two machines sharing an ssh:// remote

No daemon needed on the server. Each machine SSHes to the server and
reads/writes the shared store directory directly:

```bash
# Machine A
kiki kk init "ssh://user@server/data/remote" project

# Machine B
kiki kk init "ssh://user@server/data/remote" project

# Both machines see each other's commits and operations
```

### Example: peer-to-peer via kiki:// (Tailscale, LAN)

Every daemon also serves the `RemoteStore` gRPC service, so any daemon can act
as the remote for another. Use `kiki://` (or the `grpc://` alias). Requires
`grpc_addr` in the server's `~/.config/kiki/config.toml`:

```bash
# Machine A: enable TCP listener in ~/.config/kiki/config.toml
#   grpc_addr = "0.0.0.0:12000"

# Machine B: use Machine A as the remote (e.g., over Tailscale)
kiki kk init "kiki://machine-a:12000" project
```

## Working with GitHub

After git convergence (in progress), kiki repos store content as git
objects. You can add GitHub as a git remote and push/fetch using
standard git protocol.

### Setup

```bash
# Initialize a local repo
kiki kk init "" my-project
cd my-project

# Create a GitHub repo (or use an existing one)
# Then add it as a remote:
kiki git remote add origin git@github.com:yourorg/my-project.git
```

### Push to GitHub

```bash
# Work normally
kiki new -m "add feature"
mkdir src && echo 'fn main() {}' > src/main.rs
kiki describe -m "initial commit"

# Push to GitHub
kiki git push --remote origin --bookmark main
```

### Fetch from GitHub

```bash
# Pull changes (e.g., merged PRs, teammate pushes)
kiki git fetch --remote origin

# See what came in
kiki log
```

### Collaborating with non-kiki users

Your teammates don't need kiki. They use plain git:

```bash
git clone git@github.com:yourorg/my-project.git
cd my-project
# normal git workflow — commit, push, PR, etc.
```

You fetch their work into your kiki workspace with `kiki git fetch`.

## Syncing over SSH

Use an `ssh://` URL to sync with a remote machine. Only the `kiki` binary
needs to be on the server.

```bash
kiki kk init ssh://user@my-server/data/myproject ~/work/myproject
cd ~/work/myproject

# Work normally — syncs to the server over SSH
kiki new -m "fix bug"
vim src/auth.rs
```

A teammate runs the same command. Both of you see each other's changes
through the shared store on the server.

### How it works

On `kiki kk init ssh://...`, the local daemon:

1. **Discovers** the remote socket: `ssh user@host kiki kk daemon socket-path`
2. **Starts** the remote daemon if not running: `ssh user@host kiki kk daemon run --managed`
3. **Opens a persistent tunnel**: `ssh -L local.sock:remote.sock user@host -N`
4. **Connects** a gRPC client to the forwarded socket

The tunnel stays alive for the mount's lifetime — subsequent CLI commands
reuse it with zero SSH handshake cost. The local daemon manages the tunnel
process and cleans up on shutdown.

The full gRPC protocol runs over the tunnel, giving access to all
`RemoteStore` operations (blob CAS + mutable refs). Multiple local
daemons sharing the same remote serialize ref updates via compare-and-swap.

### Prerequisites on the server

1. `kiki` binary in `$PATH`
2. SSH access with key-based auth (BatchMode — no interactive prompts)
3. The remote daemon auto-starts and manages its own storage

## Known limitations

- **Linux-primary.** FUSE on Linux is the well-tested path. macOS NFS works but
  has cache-coherency caveats (mitigated by mounting with `actimeo=0`) and
  occasional Apple-version-specific quirks.
- **No auth or TLS on kiki:// (gRPC).** The daemon listens on localhost only
  by default. For `kiki://` remotes on a LAN or Tailscale, the network
  provides the trust boundary. Don't expose the gRPC port to untrusted
  networks. `ssh://` remotes inherit SSH's authentication and encryption.
- **S3-compatible backend requirements.** `s3://` remotes rely on conditional
  object writes/deletes for ref compare-and-swap. AWS S3 supports this; S3-like
  services must support `If-Match` / `If-None-Match` on object writes and
  deletes to be safe for concurrent writers.
- **No sparse patterns.** `set_sparse_patterns` is unimplemented. With a
  lazy VFS this is less important than for on-disk working copies.
- **Daemon restart drops kernel file handles.** The inode slab is in-memory;
  applications with open file descriptors across a daemon restart will see
  ESTALE. Mount state and store data are preserved (redb), so re-running
  commands after restart works fine.
- **Synchronous remote push.** `Snapshot` blocks until all new blobs land on
  the remote. Fine for localhost and `dir://`; will need an async push queue
  for higher-latency network remotes.
- **Some jj commands are unimplemented.** `recover`, `rename_workspace`,
  `reset`, and sparse-patterns operations will panic with `todo!`.

## Troubleshooting

**"daemon not reachable"**
If `KIKI_SOCKET_PATH` is set, the CLI won't auto-start the daemon.
Check with `kiki kk daemon status`. Otherwise the daemon auto-starts —
check `kiki kk daemon logs` for errors.

**"mount failed" / FUSE errors on init**
Check that `fusermount3` is installed and setuid. On most Linux distros:
```bash
which fusermount3
ls -la $(which fusermount3)  # should show the setuid bit
```

**Stale mount after daemon crash**
If the daemon crashes and leaves a stale FUSE mount, unmount it manually:
```bash
fusermount3 -u /path/to/repo
```
Then restart the daemon (`kiki kk daemon run`). It rehydrates persisted
mounts on startup.

**Verbose logging**
```bash
RUST_LOG=debug kiki kk daemon run
# Or check the auto-managed daemon's log:
kiki kk daemon logs -f
```

**SSH tunnel issues**
If an `ssh://` remote fails to connect:
```bash
# Test SSH connectivity manually
ssh user@host kiki kk daemon socket-path
ssh user@host kiki kk daemon status
```
The local daemon's log (`kiki kk daemon logs`) shows tunnel establishment
details and errors.

## Running tests

```bash
cargo test --workspace
```

Integration tests spin up a temporary daemon per test (using `KIKI_SOCKET_PATH`
for isolation), exercising the full FUSE path. They require `fusermount3` to be
available. Set `KIKI_TEST_DISABLE_MOUNT=1` to skip FUSE in environments where
it's unavailable.
