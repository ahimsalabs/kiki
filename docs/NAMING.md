# Naming

Pin the naming decisions. These match the current code.

## Names

| Context | Name | Notes |
|---------|------|-------|
| Project / crate | **kiki** | `cargo install kiki`. The name you search for, tell people about, put in a README. |
| Binary | **kiki** | `cli/Cargo.toml` `name = "kiki"`. Installed as `kiki` on PATH. |
| Standard jj commands | `kiki log`, `kiki new`, etc. | All jj commands work at the top level. |
| Git operations | `kiki git push`, `kiki git fetch`, etc. | On kiki repos, a dispatch hook routes to the daemon. On plain jj repos, falls through to jj's built-in git. |
| kiki-only commands | `kiki kk init`, `kiki kk status` | `kk` is only used for commands that collide with jj builtins. |
| Recommended alias | `alias jj=kiki` | `kiki` is a strict superset of `jj`. |
| Daemon binary | `daemon` (current) | Separate crate, to be merged into `kiki daemon run`. |
| Env var (socket) | `KIKI_SOCKET_PATH` | Escape hatch for non-default daemon socket. |
| Env var (log) | `RUST_LOG=kiki=info` | Standard tracing filter. |
| Config file | `~/.config/kiki/config.toml` | Optional. |
| Storage dir | `~/.local/state/kiki/` | XDG default. |
| systemd unit | `kiki.service` | `ExecStart=kiki daemon run` (post-merge). |
| launchd label | `id.kiki.daemon` | macOS service. |

## Current state

The code already reflects these names:

- `cli/Cargo.toml`: `name = "kiki"` -- binary is `kiki`.
- `main.rs`: `CliRunner::init().name("kiki")` -- app name in `--help`.
- `kiki git push/fetch/remote` -- dispatch hook detects kiki-backend
  repos and routes through the daemon. On non-kiki repos, falls
  through to jj's native git commands.
- `kiki kk init`, `kiki kk status` -- the `kk` subcommand is used
  only for commands that would collide with jj builtins.

With `alias jj=kiki`, users type `jj git push`, `jj kk init`,
`jj log`, `jj new`, etc. Every standard jj command works because
`kiki` *is* jj with extra backends wired in.

## Rationale

**Why `kiki` for both the project and the binary:**

The binary name matches the project name. `cargo install kiki`
installs `kiki`. Simple, no confusion. Four keystrokes is fine --
with `alias jj=kiki` you type `jj` anyway.

**Why git operations are top-level (no `kk`):**

`kiki git push` uses an `add_dispatch_hook` on jj's `CliRunner` to
intercept the `git` subcommand. When the workspace uses the kiki
backend (detected by reading `.jj/repo/store/type`), it routes
push/fetch/remote through the daemon. For non-kiki repos, the hook
falls through to jj's built-in git commands. This means `kiki git`
"just works" regardless of backend.

**Why `kk` still exists for `init` and `status`:**

- `kiki init` would collide with `jj init` (different args, different
  semantics -- kiki init takes a remote URL).
- `kiki status` would collide with `jj status` (kiki shows daemon
  sessions, not working-copy changes).

Future kiki commands that don't collide with jj builtins (e.g.
`approve`, `submit`, `land`) can go top-level without `kk`.

**Why `alias jj=kiki`:**

`kiki` is a strict superset of `jj`. Users who don't use the daemon
features never notice. Users who do get `jj kk init`, `jj git push`,
etc.

## Packaging

| Channel | Install command | Binary |
|---------|----------------|--------|
| crates.io | `cargo install kiki` | `kiki` |
| GitHub Releases | download from releases page | `kiki` |
| Homebrew | `brew install kiki` | `kiki` |
| Nix flake | `nix run github:broady/kiki` | `kiki` |
| AUR | `yay -S kiki` | `kiki` |

## Future: single binary

The daemon is currently a separate crate/binary (`daemon`). The plan
is to merge it into the `kiki` binary as `kiki daemon run`. This
gives a single binary to install, with the daemon as a subcommand
mode. See [DAEMON_LIFECYCLE.md](./DAEMON_LIFECYCLE.md).
