//! Daemon connection with auto-start.
//!
//! The CLI connects to the daemon via UDS. If the socket is missing or stale,
//! and `KIKI_SOCKET_PATH` is not set, the CLI auto-starts a managed daemon.

use std::path::Path;
use std::time::Duration;

use crate::blocking_client::BlockingJujutsuInterfaceClient;

type ConnectError = Box<dyn std::error::Error + Send + Sync>;

/// Connect to the daemon, auto-starting if necessary.
///
/// Resolution order:
/// 1. Try UDS at the resolved socket path.
/// 2. If that fails and `KIKI_SOCKET_PATH` is set → error (user manages daemon).
/// 3. Otherwise → clean stale socket, spawn `kiki kk daemon run --managed`, poll for readiness.
pub fn connect_or_start(
    _settings: &jj_lib::settings::UserSettings,
) -> Result<BlockingJujutsuInterfaceClient, ConnectError> {
    let socket = store::paths::socket_path();

    // Try connecting to existing daemon.
    if socket.exists() {
        match BlockingJujutsuInterfaceClient::connect_uds(socket.clone()) {
            Ok(client) => return Ok(client),
            Err(_) => {
                // Socket exists but can't connect — stale.
                tracing::debug!("socket exists but connection failed; treating as stale");
            }
        }
    }

    // If KIKI_SOCKET_PATH is explicitly set, don't auto-start — the user
    // owns the daemon lifecycle.
    if store::paths::is_explicit_socket() {
        return Err(format!(
            "daemon not reachable at KIKI_SOCKET_PATH={}",
            socket.display()
        )
        .into());
    }

    // Clean stale socket and PID file.
    clean_stale(&socket);

    // Auto-start the daemon.
    spawn_managed_daemon()?;

    // Poll for readiness (up to 3 seconds).
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        if let Ok(client) = BlockingJujutsuInterfaceClient::connect_uds(socket.clone()) {
            return Ok(client);
        }
    }

    Err(format!(
        "daemon did not become ready at {} within 3 seconds after auto-start",
        socket.display()
    )
    .into())
}

/// Remove stale socket and PID file if the PID is no longer alive.
fn clean_stale(socket: &Path) {
    let pid_path = store::paths::pid_path();
    if let Ok(contents) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = contents.trim().parse::<u32>() {
            // Check if the process is alive.
            let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
            if !alive {
                tracing::debug!(pid, "stale PID file; cleaning up");
                let _ = std::fs::remove_file(socket);
                let _ = std::fs::remove_file(&pid_path);
            }
        }
    } else {
        // No PID file but socket exists — remove the socket.
        let _ = std::fs::remove_file(socket);
    }
}

/// Spawn `kiki kk daemon run --managed` as a detached background process.
fn spawn_managed_daemon() -> Result<(), ConnectError> {
    let kiki_bin = std::env::current_exe()
        .map_err(|e| format!("failed to determine kiki binary path: {e}"))?;

    // Ensure runtime directory exists.
    let runtime_dir = store::paths::runtime_dir();
    std::fs::create_dir_all(&runtime_dir)
        .map_err(|e| format!("failed to create runtime dir {}: {e}", runtime_dir.display()))?;

    // Redirect stdout/stderr to log file.
    let log_path = store::paths::log_path();
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("failed to open log file {}: {e}", log_path.display()))?;
    let log_err = log_file
        .try_clone()
        .map_err(|e| format!("failed to clone log file handle: {e}"))?;

    use std::process::{Command, Stdio};
    let _child = Command::new(kiki_bin)
        .args(["kk", "daemon", "run", "--managed"])
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_err)
        .spawn()
        .map_err(|e| format!("failed to spawn managed daemon: {e}"))?;

    // Don't wait for the child — it's a long-lived background process.
    // The spawned process is detached by virtue of not being waited on,
    // and it won't be killed when this process exits because we don't
    // hold the Child (it's dropped here but that doesn't kill on Unix).
    Ok(())
}
