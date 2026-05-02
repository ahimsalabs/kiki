//! Shared socket/path resolution used by both the daemon and CLI.
//!
//! The daemon uses these paths to decide where to listen; the CLI uses
//! them to decide where to connect. All functions return paths only —
//! directory creation is the caller's responsibility.

use std::path::PathBuf;

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
/// 1. `KIKI_SOCKET_PATH` env var (explicit override)
/// 2. `$XDG_RUNTIME_DIR/kiki/daemon.sock` (Linux default)
/// 3. `~/Library/Caches/kiki/daemon.sock` (macOS default)
/// 4. `/tmp/kiki-{uid}/daemon.sock` (fallback if `XDG_RUNTIME_DIR` is unset on Linux)
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("KIKI_SOCKET_PATH") {
        return PathBuf::from(p);
    }
    runtime_dir().join("daemon.sock")
}

/// PID file path — sibling of [`socket_path()`]: same directory, `daemon.pid`.
pub fn pid_path() -> PathBuf {
    runtime_dir().join("daemon.pid")
}

/// Log file path — sibling of [`socket_path()`]: same directory, `daemon.log`.
pub fn log_path() -> PathBuf {
    runtime_dir().join("daemon.log")
}

/// Runtime directory containing the socket, PID file, and log.
///
/// Resolution order (when `KIKI_SOCKET_PATH` is not set):
/// - Linux: `$XDG_RUNTIME_DIR/kiki`, falling back to `/tmp/kiki-{uid}`
/// - macOS: `~/Library/Caches/kiki`
/// - Other: treated as Linux
pub fn runtime_dir() -> PathBuf {
    // If KIKI_SOCKET_PATH is set, derive the directory from it.
    if let Ok(p) = std::env::var("KIKI_SOCKET_PATH") {
        let path = PathBuf::from(p);
        return path.parent().map(|p| p.to_path_buf()).unwrap_or(path);
    }

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").expect("HOME must be set");
        PathBuf::from(home).join("Library/Caches/kiki")
    }

    #[cfg(not(target_os = "macos"))]
    {
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            PathBuf::from(xdg).join("kiki")
        } else {
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/kiki-{uid}"))
        }
    }
}

/// Default storage directory for per-mount state.
///
/// - Linux: `~/.local/state/kiki`
/// - macOS: `~/Library/Application Support/kiki`
pub fn default_storage_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    let home = PathBuf::from(home);

    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/kiki")
    }

    #[cfg(not(target_os = "macos"))]
    {
        home.join(".local/state/kiki")
    }
}

/// Optional config file path: `~/.config/kiki/config.toml`.
pub fn config_path() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join(".config/kiki/config.toml")
}
