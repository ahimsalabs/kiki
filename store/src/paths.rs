//! Shared path resolution used by both the daemon and CLI.
//!
//! Two independent roots:
//!
//! - **`KIKI_HOME`** — state directory (stores, config, daemon runtime).
//!   Default: `$XDG_DATA_HOME/kiki` or `~/.local/share/kiki`.
//! - **`KIKI_MOUNT`** — FUSE mount point (repos/workspaces appear here).
//!   Default: `~/kiki`.
//!
//! ```text
//! ~/.local/share/kiki/          # KIKI_HOME — state directory
//! ~/.local/share/kiki/config.toml
//! ~/.local/share/kiki/store/    # git stores, redb, workspace metadata
//! ~/.local/share/kiki/daemon.sock
//! ~/.local/share/kiki/daemon.pid
//! ~/.local/share/kiki/daemon.log
//! ~/.local/share/kiki/tunnels/
//! ~/kiki/                        # KIKI_MOUNT — FUSE mount (repos/workspaces)
//! ```
//!
//! The FUSE mount point must be empty before mounting. State never lives
//! inside the mount — they are completely separate directories.
//!
//! Directory creation is the caller's responsibility.

use std::path::PathBuf;

/// Resolve `KIKI_HOME` — the state directory for all kiki data.
///
/// Resolution order:
/// 1. `KIKI_HOME` env var
/// 2. `$XDG_DATA_HOME/kiki`
/// 3. `~/.local/share/kiki`
pub fn kiki_home() -> PathBuf {
    if let Ok(p) = std::env::var("KIKI_HOME") {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(p).join("kiki");
    }
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join(".local/share/kiki")
}

/// Resolve the FUSE mount point where repos/workspaces appear.
///
/// Resolution order:
/// 1. `KIKI_MOUNT` env var
/// 2. `~/kiki`
pub fn mount_root() -> PathBuf {
    if let Ok(p) = std::env::var("KIKI_MOUNT") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join("kiki")
}

/// State directory (alias for `kiki_home()`).
///
/// Contains store data, daemon socket/PID/log, tunnels, and config.
pub fn state_dir() -> PathBuf {
    kiki_home()
}

/// Returns `true` if the `KIKI_SOCKET_PATH` env var is set.
///
/// When the user explicitly manages the daemon socket location, the CLI
/// should skip auto-start logic and connect directly.
pub fn is_explicit_socket() -> bool {
    std::env::var("KIKI_SOCKET_PATH").is_ok()
}

/// Resolve the daemon socket path.
///
/// Resolution order:
/// 1. `KIKI_SOCKET_PATH` env var (explicit override, disables auto-start)
/// 2. `$KIKI_HOME/daemon.sock`
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("KIKI_SOCKET_PATH") {
        return PathBuf::from(p);
    }
    state_dir().join("daemon.sock")
}

/// PID file path.
///
/// When `KIKI_SOCKET_PATH` is set, the PID file is a sibling of the
/// socket (preserving test isolation). Otherwise: `$KIKI_HOME/daemon.pid`.
pub fn pid_path() -> PathBuf {
    runtime_dir().join("daemon.pid")
}

/// Log file path.
///
/// When `KIKI_SOCKET_PATH` is set, the log file is a sibling of the
/// socket. Otherwise: `$KIKI_HOME/daemon.log`.
pub fn log_path() -> PathBuf {
    runtime_dir().join("daemon.log")
}

/// Runtime directory for ephemeral state (socket, PID, log, tunnels).
///
/// When `KIKI_SOCKET_PATH` is set, this is the parent directory of the
/// socket — preserving the existing contract that PID, log, and tunnels
/// live alongside the socket for test isolation. Otherwise returns
/// [`state_dir()`].
pub fn runtime_dir() -> PathBuf {
    if let Ok(p) = std::env::var("KIKI_SOCKET_PATH") {
        let path = PathBuf::from(p);
        return path.parent().map(|p| p.to_path_buf()).unwrap_or(path);
    }
    state_dir()
}

/// Storage directory for per-mount durable state: `$KIKI_HOME/store/`.
pub fn default_storage_dir() -> PathBuf {
    kiki_home().join("store")
}

/// Config file path: `$KIKI_HOME/config.toml`.
pub fn config_path() -> PathBuf {
    kiki_home().join("config.toml")
}

/// Default mount root (FUSE mount point): resolved from `KIKI_MOUNT`.
///
/// The FUSE mount is bound here. Repos appear as
/// `$KIKI_MOUNT/<repo>/<workspace>/`. The directory must be empty
/// before mounting.
pub fn default_mount_root() -> PathBuf {
    mount_root()
}
