//! SSH tunnel management for `kiki+ssh://` remotes.
//!
//! Instead of the old per-connection stdin/stdout framing protocol, this
//! module establishes a persistent
//! SSH tunnel (`ssh -L local.sock:remote.sock host -N`) to the remote
//! daemon's UDS. The local daemon then speaks gRPC over the forwarded
//! socket — same protocol as `grpc://`, same `RemoteStore` trait impl,
//! but transported over SSH with zero TCP ports needed on either end.
//!
//! The tunnel is long-lived (survives across CLI invocations) and owned
//! by the local daemon process. On daemon shutdown or mount removal the
//! SSH child is killed and the local socket cleaned up.
//!
//! ## Flow
//!
//! 1. `ssh user@host kiki kk daemon socket-path` → discover remote socket
//! 2. `ssh user@host kiki kk daemon run --managed` → ensure remote daemon
//! 3. `ssh -L <local>.sock:<remote>.sock user@host -N` → persistent tunnel
//! 4. Connect `GrpcRemoteStore` to `<local>.sock`
//!
//! Steps 1–3 happen once per mount init / rehydrate.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

/// A managed SSH tunnel forwarding a remote daemon's UDS to a local socket.
///
/// Owns the `ssh -L` child process. On drop, the child is killed and the
/// local socket file removed (best-effort).
pub struct SshTunnel {
    /// SSH target: `user@host` or just `host`.
    target: String,
    /// Remote storage path (for logging/debug).
    #[allow(dead_code)] // kept for diagnostics / future health-check logging
    remote_path: String,
    /// Remote daemon socket path (discovered via `kiki kk daemon socket-path`).
    #[allow(dead_code)] // kept for re-establishment on tunnel restart
    remote_socket: String,
    /// Local forwarded socket path.
    local_socket: PathBuf,
    /// The `ssh -L ... -N` child process.
    child: Child,
}

impl std::fmt::Debug for SshTunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshTunnel")
            .field("target", &self.target)
            .field("remote_path", &self.remote_path)
            .field("local_socket", &self.local_socket)
            .field("alive", &self.is_alive())
            .finish()
    }
}

impl SshTunnel {
    /// Establish a new SSH tunnel to a remote daemon.
    ///
    /// - `user`: SSH user (empty string means SSH default).
    /// - `host`: remote hostname.
    /// - `path`: remote storage path (the `/path` from `kiki+ssh://user@host/path`).
    /// - `tunnels_dir`: directory where local forwarded sockets are placed.
    ///
    /// This function:
    /// 1. Discovers the remote daemon socket.
    /// 2. Ensures the remote daemon is running.
    /// 3. Opens a persistent `ssh -L` tunnel.
    ///
    /// Blocks (async) until the tunnel is ready for connections.
    pub async fn establish(
        user: &str,
        host: &str,
        path: &str,
        tunnels_dir: &Path,
    ) -> Result<Self> {
        let target = if user.is_empty() {
            host.to_owned()
        } else {
            format!("{user}@{host}")
        };

        // 1. Discover the remote daemon socket path.
        let remote_socket = discover_remote_socket(&target).await
            .with_context(|| format!("discovering remote socket on {target}"))?;
        info!(%target, %remote_socket, "discovered remote daemon socket");

        // 2. Ensure the remote daemon is running.
        ensure_remote_daemon(&target).await
            .with_context(|| format!("ensuring remote daemon on {target}"))?;

        // 3. Compute deterministic local socket path.
        let tunnel_id = tunnel_socket_name(user, host, path);
        std::fs::create_dir_all(tunnels_dir)
            .with_context(|| format!("creating tunnels dir {}", tunnels_dir.display()))?;
        let local_socket = tunnels_dir.join(format!("{tunnel_id}.sock"));

        // Remove stale socket if present (previous daemon crash).
        if local_socket.exists() {
            std::fs::remove_file(&local_socket).ok();
        }

        // 4. Open persistent tunnel.
        let forward_spec = format!(
            "{}:{}",
            local_socket.display(),
            remote_socket,
        );
        debug!(%target, %forward_spec, "opening SSH tunnel");

        let child = Command::new("ssh")
            .arg("-o").arg("BatchMode=yes")
            .arg("-o").arg("ExitOnForwardFailure=yes")
            .arg("-o").arg("StreamLocalBindUnlink=yes")
            // ServerAliveInterval keeps the tunnel alive and detects
            // dead connections within ~45s (3 missed keepalives).
            .arg("-o").arg("ServerAliveInterval=15")
            .arg("-o").arg("ServerAliveCountMax=3")
            .arg("-L").arg(&forward_spec)
            .arg(&target)
            .arg("-N") // no remote command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit()) // SSH errors visible to user
            .spawn()
            .with_context(|| format!("spawning ssh tunnel to {target}"))?;

        // Wait for the local socket to appear (SSH creates it on bind).
        wait_for_socket(&local_socket, std::time::Duration::from_secs(10)).await
            .with_context(|| format!(
                "tunnel socket {} did not appear within timeout",
                local_socket.display()
            ))?;

        info!(
            %target,
            local = %local_socket.display(),
            remote = %remote_socket,
            pid = child.id().unwrap_or(0),
            "SSH tunnel established"
        );

        Ok(SshTunnel {
            target,
            remote_path: path.to_owned(),
            remote_socket,
            local_socket,
            child,
        })
    }

    /// Path to the local forwarded socket.
    pub fn local_socket(&self) -> &Path {
        &self.local_socket
    }

    /// Check if the SSH child process is still alive.
    pub fn is_alive(&self) -> bool {
        // `id()` returns None if the process has already been awaited/reaped.
        self.child.id().is_some()
    }

    /// SSH target string (for diagnostics).
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Kill the tunnel and clean up.
    pub async fn shutdown(&mut self) {
        // Send SIGTERM to the ssh process.
        if let Err(e) = self.child.kill().await
            && e.kind() != std::io::ErrorKind::InvalidInput
        {
            warn!(error = %e, target = %self.target, "failed to kill SSH tunnel");
        }
        // Remove local socket.
        if self.local_socket.exists()
            && let Err(e) = std::fs::remove_file(&self.local_socket)
        {
            warn!(
                error = %e,
                path = %self.local_socket.display(),
                "failed to remove tunnel socket"
            );
        }
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        // Best-effort synchronous cleanup. The child process gets SIGKILL
        // on drop via tokio's Child impl, and we clean the socket file.
        if self.local_socket.exists() {
            std::fs::remove_file(&self.local_socket).ok();
        }
        // Note: tokio::process::Child sends SIGKILL on drop when the
        // process is still running. This is acceptable for SSH tunnels.
    }
}

/// Discover the remote daemon socket by running
/// `ssh target kiki kk daemon socket-path`.
async fn discover_remote_socket(target: &str) -> Result<String> {
    let output = Command::new("ssh")
        .arg("-o").arg("BatchMode=yes")
        .arg(target)
        .arg("kiki")
        .arg("kk")
        .arg("daemon")
        .arg("socket-path")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("running ssh for socket-path discovery")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "remote `kiki kk daemon socket-path` failed (exit {}): {}",
            output.status,
            stderr.trim()
        ));
    }

    let socket = String::from_utf8(output.stdout)
        .context("remote socket-path output is not valid UTF-8")?
        .trim()
        .to_owned();

    if socket.is_empty() {
        return Err(anyhow!("remote `kiki kk daemon socket-path` returned empty output"));
    }

    Ok(socket)
}

/// Ensure the remote daemon is running by invoking
/// `ssh target kiki kk daemon run --managed` (auto-start).
///
/// This is fire-and-forget: the remote daemon detaches from the SSH
/// session. We just need it to be alive by the time we open the tunnel.
/// If it's already running, `run --managed` exits quickly (socket
/// already bound → error or the daemon simply starts a second instance
/// which discovers the socket is taken and exits).
///
/// A better approach (TODO): run `kiki kk daemon status` first and only
/// start if not running.
async fn ensure_remote_daemon(target: &str) -> Result<()> {
    // First check if the daemon is already reachable by testing
    // if the socket-path discovery worked (it did, if we got here).
    // Try a lightweight status check.
    let status_output = Command::new("ssh")
        .arg("-o").arg("BatchMode=yes")
        .arg(target)
        .arg("kiki")
        .arg("kk")
        .arg("daemon")
        .arg("status")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("checking remote daemon status")?;

    let stdout = String::from_utf8_lossy(&status_output.stdout);
    if stdout.contains("running") {
        debug!(%target, "remote daemon already running");
        return Ok(());
    }

    // Daemon not running — start it.
    info!(%target, "starting remote daemon");
    let start_output = Command::new("ssh")
        .arg("-o").arg("BatchMode=yes")
        .arg(target)
        // Use nohup + background so the daemon outlives the SSH session.
        // The remote shell detaches it.
        .arg("nohup")
        .arg("kiki")
        .arg("kk")
        .arg("daemon")
        .arg("run")
        .arg("--managed")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("starting remote daemon via SSH")?;

    if !start_output.status.success() {
        let stderr = String::from_utf8_lossy(&start_output.stderr);
        // Non-zero exit might mean "already running" — not fatal.
        warn!(
            %target,
            exit = %start_output.status,
            stderr = %stderr.trim(),
            "remote daemon start returned non-zero (may already be running)"
        );
    }

    // Give the remote daemon a moment to bind its socket.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    Ok(())
}

/// Wait for a socket file to appear on disk.
async fn wait_for_socket(path: &Path, timeout: std::time::Duration) -> Result<()> {
    let start = std::time::Instant::now();
    let interval = std::time::Duration::from_millis(50);

    loop {
        if path.exists() {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(anyhow!(
                "socket {} not created within {:?}",
                path.display(),
                timeout
            ));
        }
        tokio::time::sleep(interval).await;
    }
}

/// Compute a deterministic, filesystem-safe socket name for a tunnel.
///
/// Uses blake3 to hash the (user, host, path) tuple into a short hex
/// string. This ensures the socket path is stable across daemon restarts
/// (so rehydrate finds the same file) and collision-free.
fn tunnel_socket_name(user: &str, host: &str, path: &str) -> String {
    use std::fmt::Write;
    let input = format!("kiki+ssh://{user}@{host}{path}");
    let hash = blake3::hash(input.as_bytes());
    let mut s = String::with_capacity(3 + 16);
    s.push_str("tun-");
    for b in &hash.as_bytes()[..8] {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_socket_name_is_deterministic() {
        let a = tunnel_socket_name("cbro", "myhost.com", "/data/store");
        let b = tunnel_socket_name("cbro", "myhost.com", "/data/store");
        assert_eq!(a, b);
    }

    #[test]
    fn tunnel_socket_name_differs_by_path() {
        let a = tunnel_socket_name("cbro", "host", "/path/a");
        let b = tunnel_socket_name("cbro", "host", "/path/b");
        assert_ne!(a, b);
    }

    #[test]
    fn tunnel_socket_name_format() {
        let name = tunnel_socket_name("user", "host", "/path");
        assert!(name.starts_with("tun-"), "got: {name}");
        assert_eq!(name.len(), 4 + 16, "got: {name}"); // "tun-" + 16 hex chars
        assert!(name[4..].chars().all(|c| c.is_ascii_hexdigit()));
    }
}
