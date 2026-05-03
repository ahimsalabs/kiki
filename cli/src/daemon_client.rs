//! Daemon connection with auto-start and mount-health verification.
//!
//! The CLI connects to the daemon via UDS. If the socket is missing or stale,
//! and `KIKI_SOCKET_PATH` is not set, the CLI auto-starts a managed daemon.
//! After connecting, a `DaemonStatus` RPC verifies that the workspace mount
//! is active. If the mount is dead, the daemon is restarted once.

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
/// 4. Verify mount health via `DaemonStatus` RPC.
/// 5. If mount is dead on a managed daemon → stop, re-spawn, verify once more.
pub fn connect_or_start(
    _settings: &jj_lib::settings::UserSettings,
) -> Result<BlockingJujutsuInterfaceClient, ConnectError> {
    let client = try_connect_or_start()?;

    match check_mount_health(&client) {
        Ok(true) => return Ok(client),
        Ok(false) => {
            if store::paths::is_explicit_socket() {
                return Err(
                    "daemon is alive but the workspace mount is not active; \
                     restart the daemon or run `kiki kk doctor` to diagnose"
                        .into(),
                );
            }
            tracing::warn!("daemon is alive but mount is not active; restarting");
            drop(client);
            stop_running_daemon()?;

            let client = try_connect_or_start()?;
            match check_mount_health(&client) {
                Ok(true) => Ok(client),
                Ok(false) => Err(
                    "daemon restarted but mount is still not active; \
                     run `kiki kk setup` to check prerequisites \
                     or `kiki kk daemon logs` to see what failed"
                        .into(),
                ),
                Err(e) => Err(e),
            }
        }
        Err(e) => {
            tracing::warn!("DaemonStatus RPC failed after connect: {e}");
            Err(e)
        }
    }
}

/// Returns `Ok(true)` if the workspace mount is active, `Ok(false)` if the
/// daemon is alive but the mount failed.
fn check_mount_health(client: &BlockingJujutsuInterfaceClient) -> Result<bool, ConnectError> {
    let resp = client
        .daemon_status(proto::jj_interface::DaemonStatusReq {})
        .map_err(|e| -> ConnectError { format!("DaemonStatus RPC failed: {e}").into() })?;
    Ok(resp.into_inner().mount_active)
}

/// Connect to an existing daemon or auto-start one. Does NOT verify mount
/// health — the caller is responsible for that.
fn try_connect_or_start() -> Result<BlockingJujutsuInterfaceClient, ConnectError> {
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

    // Poll for readiness (up to 5 seconds).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        if let Ok(client) = BlockingJujutsuInterfaceClient::connect_uds(socket.clone()) {
            return Ok(client);
        }
    }

    Err(format!(
        "daemon did not become ready at {} within 5 seconds after auto-start; \
         check `kiki kk daemon logs` for details",
        socket.display()
    )
    .into())
}

/// Stop a managed daemon by sending SIGTERM and waiting for it to exit.
fn stop_running_daemon() -> Result<(), ConnectError> {
    let pid_path = store::paths::pid_path();
    let socket = store::paths::socket_path();

    let contents = match std::fs::read_to_string(&pid_path) {
        Ok(c) => c,
        Err(_) => {
            // No PID file — clean up socket and return.
            let _ = std::fs::remove_file(&socket);
            return Ok(());
        }
    };
    let pid: i32 = match contents.trim().parse() {
        Ok(p) => p,
        Err(_) => {
            let _ = std::fs::remove_file(&pid_path);
            let _ = std::fs::remove_file(&socket);
            return Ok(());
        }
    };

    // Send SIGTERM.
    let ret = unsafe { libc::kill(pid, libc::SIGTERM) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            // Already dead — clean up.
            let _ = std::fs::remove_file(&pid_path);
            let _ = std::fs::remove_file(&socket);
            return Ok(());
        }
        return Err(format!("failed to send SIGTERM to daemon (PID {pid}): {err}").into());
    }

    // Wait up to 3 seconds for the process to exit.
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(50));
        let alive = unsafe { libc::kill(pid, 0) == 0 };
        if !alive {
            let _ = std::fs::remove_file(&pid_path);
            let _ = std::fs::remove_file(&socket);
            return Ok(());
        }
    }

    Err(format!("daemon (PID {pid}) did not exit within 3 seconds after SIGTERM").into())
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
