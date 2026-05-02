# Naming

Pin the naming decisions. These match the current code.

## Names

| Context | Name | Notes |
|---------|------|-------|
| Project / crate | **kiki** | `cargo install kiki`. The name you search for, tell people about, put in a README. |
| Binary | **kiki** | `cli/Cargo.toml` `name = "kiki"`. Installed as `kiki` on PATH. |
| kiki subcommand | **kk** | `kiki kk init`, `kiki kk status`. Kiki-specific operations live under the `kk` subcommand. |
| Standard jj commands | `kiki log`, `kiki new`, etc. | All jj commands work at the top level. |
| Recommended alias | `alias jj=kiki` | `kiki` is a strict superset of `jj`. |
| Daemon binary | `daemon` (current) | Separate crate, to be merged into `kiki daemon run`. |
| Daemon subcommands | `kiki kk daemon run/status/stop` | Post-merge. Power user / debugging. |
| Env var (socket) | `KIKI_SOCKET_PATH` | Escape hatch for non-default daemon socket. |
| Env var (log) | `RUST_LOG=kiki=info` | Standard tracing filter. |
| Config file | `~/.config/kiki/config.toml` | Optional. |
| Storage dir | `~/.local/state/kiki/` | XDG default. |
| systemd unit | `kiki.service` | `ExecStart=kiki kk daemon run`. |
| launchd label | `id.kiki.daemon` | macOS service. |

## Current state

The code already reflects these names:

- `cli/Cargo.toml`: `name = "kiki"` -- binary is `kiki`.
- `main.rs`: `CliRunner::init().name("kiki")` -- app name in `--help`.
- Clap enum: `KikiSubcommand::Kk(KikiArgs)` -- the `kk` subcommand.
- Sub-subcommands: `KikiCommands::Init`, `KikiCommands::Status`.
- Invocation: `kiki kk init <remote> [dest]`, `kiki kk status`.

With `alias jj=kiki`, users type `jj kk init`, `jj log`, `jj new`,
etc. Every standard jj command works because `kiki` *is* jj with
extra backends wired in.

## Rationale

**Why `kiki` for both the project and the binary:**

The binary name matches the project name. `cargo install kiki`
installs `kiki`. Simple, no confusion. Four keystrokes is fine --
with `alias jj=kiki` you type `jj` anyway.

**Why `kk` as the subcommand, not a separate binary:**

- jj has no plugin/extension system. The binary links jj-lib and
  registers custom store factories at compile time.
- `kk` is a short, memorable namespace for kiki-specific operations
  that don't collide with jj's built-in commands.
- The alphabetic progression jj -> kk is legible to the target
  audience.

**Why `alias jj=kiki`:**

`kiki` is a strict superset of `jj`. Users who don't use the daemon
features never notice. Users who do get `jj kk init`, `jj kk status`,
and eventually `jj kk clone`, `jj kk workspace new`, etc.

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
is to merge it into the `kiki` binary as `kiki kk daemon run`. This
gives a single binary to install, with the daemon as a subcommand
mode. See [DAEMON_LIFECYCLE.md](./DAEMON_LIFECYCLE.md).
