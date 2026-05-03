# Daemon Lifecycle and Auto-Management

Design for making the kiki daemon invisible infrastructure: auto-start,
socket-based discovery, zero-config defaults, and explicit controls for
power users.

## Problem

Today the daemon requires manual startup with a config file:

```bash
./target/debug/daemon --config daemon.toml
```

The CLI requires a matching `grpc_port` in jj config. This is fine for
development but blocks every user-facing workflow:

- `kiki kk clone` can't work without a running daemon.
- `kiki kk workspace new` can't work without a running daemon.
- The WORKSPACES.md vision (managed namespace under `~/.kiki/`) assumes
  the daemon is always available.

The daemon should be invisible. The user types `kiki kk clone <remote>`
(or `jj kk clone` with `alias jj=kiki`) and everything works.

## Design

### Unix domain socket, not TCP port

The CLI-to-daemon channel is a Unix domain socket at a well-known path.
No port allocation, no config file, no collision risk.

TCP stays for `grpc://` remotes only (daemon-to-daemon over the network).
The local channel is always UDS.

Tonic supports UDS natively via `tonic::transport::Endpoint::from_shared`
with a `unix://` scheme.

### Socket path resolution

The CLI resolves the daemon socket in this order:

1. **`KIKI_SOCKET_PATH`** env var (explicit override).
2. **`$XDG_RUNTIME_DIR/kiki/daemon.sock`** (Linux default).
3. **`~/Library/Caches/kiki/daemon.sock`** (macOS default).
4. **`/tmp/kiki-$UID/daemon.sock`** (fallback if XDG_RUNTIME_DIR unset).

If `KIKI_SOCKET_PATH` is set, the CLI uses it and **skips auto-start**
(assumes the user is managing that daemon). If using a default path,
auto-start kicks in.

### Auto-start flow

```
kiki kk <any command needing daemon>
  |
  +-- resolve socket path
  +-- try connect
  |     |
  |     +-- success --> proceed with RPC
  |     +-- ECONNREFUSED / ENOENT --> stale or missing
  |
  +-- clean up stale socket file (if exists)
  +-- spawn: kiki kk daemon run --managed
  |     (same binary, detached, stdout/stderr to log file)
  +-- poll socket for readiness (< 500ms typical)
  +-- connect --> proceed with RPC
```

`--managed` tells the daemon it was auto-started: it writes a PID file
alongside the socket for stale detection, and exits after an idle timeout
(configurable, default: no timeout -- the daemon is cheap to keep alive).

### XDG-conventional layout

**Linux:**

```
~/.config/kiki/
  config.toml              # optional overrides

~/.local/state/kiki/
  mounts/<hash>/
    git/                   # bare git repo (post-convergence)
    extra/                 # jj extras table
    store.redb             # op-store + refs
    mount.toml             # mount metadata

$XDG_RUNTIME_DIR/kiki/
  daemon.sock              # UDS
  daemon.pid               # stale detection
  daemon.log               # log output (auto-managed mode)
```

**macOS:**

```
~/.config/kiki/
  config.toml              # optional overrides

~/Library/Application Support/kiki/
  mounts/<hash>/
    ...                    # same as Linux

~/Library/Caches/kiki/
  daemon.sock
  daemon.pid
  daemon.log
```

### Config file (optional)

`~/.config/kiki/config.toml` is optional. Defaults cover single-user
localhost use.

```toml
# All fields are optional. Shown with defaults.

# Storage directory for per-mount state.
# Default: ~/.local/state/kiki (Linux), ~/Library/Application Support/kiki (macOS)
# storage_dir = "..."

# TCP listener for grpc:// remote access from other daemons.
# Default: disabled (no TCP listener unless a remote needs it).
# grpc_addr = "[::1]:12000"

# NFS port range (macOS only).
# [nfs]
# min_port = 12000
# max_port = 12010
```

The old `daemon.toml` with mandatory `grpc_addr` and `storage_dir` becomes
a power-user / systemd artifact. The normal path requires no config.

The old `grpc_port` jj config setting is no longer needed for local use
(the CLI talks over UDS). It may persist as a fallback for environments
where UDS is unavailable, or be removed entirely.

### `kiki kk daemon` subcommands

```
kiki kk daemon status      # running? pid, uptime, socket path, mount count
kiki kk daemon stop        # graceful shutdown (SIGTERM)
kiki kk daemon restart     # stop + start
kiki kk daemon run         # foreground mode (systemd, debugging, --managed)
kiki kk daemon logs        # tail the log file
```

With `alias jj=kiki`, these become `jj kk daemon status`, etc.

Most users never type any of these. They exist for:

- Debugging ("is the daemon running?")
- Service management (systemd unit pointing at `kiki kk daemon run`)
- CI/testing (explicit lifecycle control)

### Stale detection

On connect failure:

1. Check PID file. If PID is not running, remove socket + PID file.
2. If socket file exists but connect fails, remove it (kernel doesn't
   clean up UDS on crash).
3. Proceed with auto-start.

### Logging

Auto-managed mode logs to `daemon.log` in the runtime directory.
Log rotation is out of scope for now (the log is small -- one line per
RPC at info level). `RUST_LOG` is respected.

`kiki kk daemon run` (foreground) logs to stderr as today.

### systemd integration

For users who prefer explicit service management:

```ini
# ~/.config/systemd/user/kiki.service
[Unit]
Description=kiki daemon

[Service]
ExecStart=%h/.cargo/bin/kiki kk daemon run
Restart=on-failure

[Install]
WantedBy=default.target
```

This coexists with auto-start: if the systemd unit is running, the CLI
finds the socket and connects. If not, auto-start kicks in.

### launchd integration (macOS)

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>id.kiki.daemon</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/kiki</string>
    <string>kk</string>
    <string>daemon</string>
    <string>run</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
</dict>
</plist>
```

## `KIKI_SOCKET_PATH` use cases

The env var is the escape hatch for non-default configurations:

- **Testing:** each `TestEnvironment` sets its own socket path in a
  temp directory. No port conflicts, no cleanup issues.
- **Multiple daemons:** run a "work" and "personal" daemon with
  different storage roots and different socket paths.
- **CI:** ephemeral socket in `$TMPDIR` with explicit lifecycle.
- **Containers:** predictable socket path for volume-mounted UDS.

When `KIKI_SOCKET_PATH` is set, the CLI:
- Uses that path for all daemon communication.
- Skips auto-start (the user owns the daemon lifecycle).
- Errors clearly if the socket is not connectable.

## SSH remote access (`kiki+ssh://`)

### Goal

Match git's SSH UX: a single URL, no manual tunnels, no port config.

```bash
kiki kk init kiki+ssh://user@my-server/myproject ~/work/myproject
```

The user never thinks about ports, sockets, or tunnels.

### Mechanism: UDS forwarding over SSH

OpenSSH 6.7+ supports Unix socket forwarding (`-L local.sock:remote.sock`).
The CLI uses this to bridge to the remote daemon's UDS — no TCP port
needed on either end.

Full flow when the CLI sees an `kiki+ssh://` URL:

```
1. ssh user@host kiki kk daemon socket-path
   → prints /run/user/1000/kiki/daemon.sock
   (resolves via the same KIKI_SOCKET_PATH / XDG order,
    respects the remote user's environment)

2. ssh user@host kiki kk daemon run --managed
   (auto-start if not already running — same as local auto-start)

3. ssh -L /tmp/kiki-<hash>.sock:<remote-socket> user@host -N
   (background, managed by CLI, cleaned up on disconnect)

4. CLI connects to /tmp/kiki-<hash>.sock — standard gRPC over UDS
```

Steps 1–3 happen once, transparently. The local CLI talks to the
forwarded socket as if the daemon were local.

### `kiki kk daemon socket-path`

New subcommand. Prints the resolved socket path and exits. Used by
the SSH flow to discover the remote socket without hardcoding paths.

```bash
$ kiki kk daemon socket-path
/run/user/1000/kiki/daemon.sock

$ KIKI_SOCKET_PATH=/custom/path.sock kiki kk daemon socket-path
/custom/path.sock
```

### Non-standard socket paths

If the remote user has `KIKI_SOCKET_PATH` set (in their shell profile,
`.bashrc`, etc.), `socket-path` returns it. The local CLI doesn't need
to know — it asks the remote and uses whatever comes back.

### SSH connection lifecycle

The forwarded SSH connection is per-session. Options:

- **Per-command:** open tunnel, do RPC, close tunnel. Simple but slow
  (SSH handshake per command).
- **Persistent background:** open tunnel on first command, keep alive,
  reuse for subsequent commands. Clean up on `kiki kk daemon stop` or
  process exit. Preferred — amortizes the SSH handshake.

The persistent approach stores the SSH PID and local socket path in
a file alongside the mount metadata (`mount.toml`), so subsequent
CLI invocations reconnect to the existing tunnel.

### Comparison with git

| | git | kiki (kiki+ssh://) |
|---|---|---|
| Remote process | `git-upload-pack` (stdin/stdout) | daemon (UDS, auto-started) |
| Port required | No | No |
| Protocol over SSH | git pack protocol | UDS forwarding → gRPC |
| Auth | SSH key | SSH key |
| Encryption | SSH | SSH |

The key difference: git runs a short-lived process per operation;
kiki connects to a long-lived daemon. UDS forwarding bridges this
cleanly — the daemon stays running, SSH provides the tunnel.

### Scope (phase 1: OpenSSH)

~200 lines in the CLI: URL parsing, `ssh` subprocess management,
socket-path discovery, tunnel lifecycle. No daemon changes needed —
the daemon doesn't know or care that the UDS connection came through
SSH.

Requires `kiki` to be installed on the remote and the user to have
shell access.

### Future: built-in SSH server (phase 2)

The daemon embeds its own SSH server (via `russh` crate), eliminating
the dependency on the remote having `kiki` in `$PATH` or the user
having shell access.

**What changes:**
- The daemon listens on a single port for SSH connections
- SSH keys map to kiki identities (ties into [`AUTH.md`](./AUTH.md))
- The SSH server only speaks the kiki protocol — no shell, no exec
- `kiki+ssh://` URLs connect to the built-in server instead of OpenSSH
- Same port can serve git smart HTTP (post-convergence) and the
  kiki gRPC protocol — one port, three protocols, discriminated
  by the initial bytes

**What stays the same:**
- The `kiki+ssh://` URL scheme and CLI UX
- SSH key-based auth
- The daemon's internal architecture (the SSH server is just another
  listener feeding into the same gRPC service)

**Why this matters:**
- GitHub, GitLab, Gitea all run custom SSH servers for the same
  reasons — no shell access, custom auth, single-port deployment
- A hosted kiki instance (e.g., on a VPS or Fly.io) shouldn't
  require system-level SSH config
- Phase 1 (OpenSSH forwarding) ships first and covers the
  self-hosted / dev-machine case; phase 2 covers the hosted /
  team-server case

## Relationship to other docs

| Doc | Connection |
|-----|------------|
| [NAMING.md](./NAMING.md) | Binary is `kiki`, kiki-specific commands live under the `kk` subcommand. |
| [WORKSPACES.md](./WORKSPACES.md) | The managed namespace (`~/.kiki/`) requires an always-available daemon. Auto-start makes `kiki kk clone` and `kiki kk workspace new` seamless. |
| [GIT_CONVERGENCE.md](./GIT_CONVERGENCE.md) | Post-convergence, the daemon manages bare git repos. `kiki kk clone git@github.com:user/repo` auto-starts daemon, inits GitBackend, mounts workspace. `git gc` runs inside the daemon. |
| [PLAN.md](./PLAN.md) | Milestone index and architectural reference. |
| [M10.7-GITIGNORE.md](./M10.7-GITIGNORE.md) | Gitignore-aware VFS — daemon-side. Auto-management means the daemon is always there. |
| [M11-PUSH-QUEUE.md](./M11-PUSH-QUEUE.md) | Async push queue + offline resilience — daemon-side. |

## Open questions

1. **Idle timeout.** Should an auto-started daemon exit after N minutes
   of inactivity? Pro: clean up resources on laptops. Con: next command
   pays startup cost. Leaning toward no timeout -- the daemon is cheap
   (< 10MB RSS idle). The user can `kiki kk daemon stop` explicitly.

2. **TCP listener in auto-managed mode.** Should the auto-started daemon
   also open a TCP port for `grpc://` remote access? Leaning no --
   TCP is opt-in via `config.toml` or `kiki kk daemon run --grpc-addr`.
   Auto-started daemons serve only the local UDS.

3. **Single daemon vs. per-user multi-daemon.** The default is one
   daemon per user. `KIKI_SOCKET_PATH` allows multiple, but the default
   path resolution always points to one socket. This is intentional --
   one daemon can manage many mounts cheaply.

4. **Upgrade story.** When the user installs a new `kiki` binary, should
   the CLI detect a version mismatch with the running daemon and
   restart it? Probably yes -- `kiki kk daemon status` shows the daemon
   version, and the CLI auto-restarts on mismatch during auto-start.
