//! VFS manager — owns transport-specific mount logistics.
//!
//! Production flow at M4: `JujutsuService::initialize` builds a per-mount
//! `Arc<dyn JjYakFs>` and asks the manager to attach it to a kernel mount
//! (Linux: `fuse3` via the `fusermount3` setuid helper; macOS: `nfsserve`
//! over a localhost port that the CLI then `mount_nfs`es). Each `bind`
//! returns both a [`TransportInfo`] for the wire and a [`MountAttachment`]
//! that the service stores on the `Mount` to keep the mount alive.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use crate::vfs::JjYakFs;
#[cfg(target_os = "linux")]
use crate::vfs::FuseAdapter;
#[cfg(target_os = "macos")]
use crate::vfs::NfsAdapter;

/// Configuration for the VFS manager.
///
/// On Linux these fields go unused (FUSE doesn't care about ports);
/// silenced with `cfg_attr` rather than dropped because the config file
/// shape is shared across platforms.
pub struct VfsManagerConfig {
    /// Inclusive lower bound of the localhost-NFS port range used on macOS.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub min_nfs_port: u16,
    /// Inclusive upper bound.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub max_nfs_port: u16,
}

/// Transport the daemon negotiated for one mount. Mirrors the proto
/// `InitializeReply.transport` oneof on the wire side. Only one variant
/// is constructible per build (Linux → Fuse, macOS → Nfs); the unused
/// variant is kept compiled to keep the proto<->internal mapping
/// transparent.
#[derive(Debug, Clone)]
#[allow(dead_code)] // unused-on-this-platform variant; see above
pub enum TransportInfo {
    /// FUSE mount succeeded daemon-side; CLI has nothing to do.
    Fuse,
    /// NFS server bound on `port`; CLI shells out to `mount_nfs`.
    Nfs { port: u16 },
}

/// Holds a mount alive for as long as the `Mount` exists. Dropping it
/// either tears down the FUSE session (Linux) or aborts the NFS server
/// task (macOS). M4 doesn't expose explicit unmount; that happens
/// implicitly when the daemon stops.
///
/// The `MountHandle` field isn't read directly — its `Drop` impl does
/// the work — but it must stay owned by the `Mount` for the lifetime of
/// the kernel mount.
#[derive(Debug)]
pub enum MountAttachment {
    #[cfg(target_os = "linux")]
    Fuse(#[allow(dead_code)] fuse3::raw::MountHandle),
    /// JoinHandle for the spawned NFS server task. Aborted on drop via
    /// the wrapper below.
    #[cfg(target_os = "macos")]
    Nfs(NfsAttachment),
}

/// Wraps a tokio JoinHandle so dropping it aborts the NFS server task.
/// Without this, the spawned task lives until it returns from
/// `handle_forever`, which never happens on its own.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct NfsAttachment {
    handle: tokio::task::JoinHandle<()>,
}

#[cfg(target_os = "macos")]
impl Drop for NfsAttachment {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[derive(thiserror::Error, Debug)]
pub enum BindError {
    #[error("VFS manager has shut down")]
    ManagerGone,

    #[cfg(target_os = "linux")]
    #[error("FUSE mount failed: {0}")]
    FuseMount(std::io::Error),

    #[cfg(target_os = "macos")]
    #[error(
        "NFS bind exhausted port range {min}..={max}: {last_error}"
    )]
    NfsBind {
        min: u16,
        max: u16,
        last_error: std::io::Error,
    },

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[error("no VFS transport available on this platform")]
    UnsupportedPlatform,
}

/// Internal request shape sent over the manager's channel. The response
/// `oneshot` closes if the manager task drops without responding (e.g.
/// shutdown), surfacing as `BindError::ManagerGone` on the caller side.
struct BindRequest {
    working_copy_path: String,
    fs: Arc<dyn JjYakFs>,
    response: oneshot::Sender<Result<(TransportInfo, MountAttachment), BindError>>,
}

pub struct VfsManager {
    /// Read by `bind_nfs` on macOS. On Linux it goes unused but the field
    /// stays so the cross-platform `Config` parsing in `main.rs` doesn't
    /// have to fork.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    config: VfsManagerConfig,
    tx: mpsc::UnboundedSender<BindRequest>,
    rx: mpsc::UnboundedReceiver<BindRequest>,
}

/// Cheap clone — wraps the channel sender. Hand one to each consumer
/// (the gRPC service is the only one today).
#[derive(Clone)]
pub struct VfsManagerHandle {
    tx: mpsc::UnboundedSender<BindRequest>,
}

impl VfsManagerHandle {
    /// Attach `fs` at `working_copy_path`. Awaits the manager's response —
    /// the bind itself is not synchronous (FUSE mount + NFS bind both do
    /// I/O), but per-mount it's a one-shot.
    pub async fn bind(
        &self,
        working_copy_path: String,
        fs: Arc<dyn JjYakFs>,
    ) -> Result<(TransportInfo, MountAttachment), BindError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(BindRequest {
                working_copy_path,
                fs,
                response: response_tx,
            })
            .map_err(|_| BindError::ManagerGone)?;
        response_rx.await.map_err(|_| BindError::ManagerGone)?
    }
}

impl VfsManager {
    pub fn new(config: VfsManagerConfig) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        VfsManager { config, tx, rx }
    }

    pub fn handle(&self) -> VfsManagerHandle {
        VfsManagerHandle {
            tx: self.tx.clone(),
        }
    }

    /// Drive the manager. Runs until the channel closes (no more handles).
    pub async fn serve(&mut self) {
        while let Some(req) = self.rx.recv().await {
            // Decompose so we can move `response` separately from the rest.
            let BindRequest {
                working_copy_path,
                fs,
                response,
            } = req;
            let result = self.handle_bind(&working_copy_path, fs).await;
            // Receiver may have been dropped if `Initialize` was cancelled
            // mid-flight; that's harmless, but log so we don't leak a
            // mounted FS we have no way to refer to.
            if let Err(_) = response.send(result) {
                warn!(path = %working_copy_path, "VfsManager bind result discarded — receiver dropped");
            }
        }
    }

    async fn handle_bind(
        &self,
        working_copy_path: &str,
        fs: Arc<dyn JjYakFs>,
    ) -> Result<(TransportInfo, MountAttachment), BindError> {
        #[cfg(target_os = "linux")]
        {
            self.bind_fuse(working_copy_path, fs).await
        }
        #[cfg(target_os = "macos")]
        {
            self.bind_nfs(working_copy_path, fs).await
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (working_copy_path, fs);
            Err(BindError::UnsupportedPlatform)
        }
    }

    #[cfg(target_os = "linux")]
    async fn bind_fuse(
        &self,
        working_copy_path: &str,
        fs: Arc<dyn JjYakFs>,
    ) -> Result<(TransportInfo, MountAttachment), BindError> {
        use fuse3::raw::Session;
        use fuse3::MountOptions;

        let adapter = FuseAdapter::new(fs);
        let mut opts = MountOptions::default();
        // `fs_name` shows up in /proc/self/mountinfo; pick something
        // identifiable so users (and us during debugging) can tell which
        // mount belongs to yak.
        opts.fs_name("yak").read_only(false);
        let session = Session::new(opts);
        let path = Path::new(working_copy_path);
        let handle = session
            .mount_with_unprivileged(adapter, path)
            .await
            .map_err(BindError::FuseMount)?;
        info!(path = %working_copy_path, "FUSE mount established");
        Ok((TransportInfo::Fuse, MountAttachment::Fuse(handle)))
    }

    #[cfg(target_os = "macos")]
    async fn bind_nfs(
        &self,
        working_copy_path: &str,
        fs: Arc<dyn JjYakFs>,
    ) -> Result<(TransportInfo, MountAttachment), BindError> {
        use nfsserve::tcp::{NFSTcp, NFSTcpListener};

        let min = self.config.min_nfs_port;
        let max = self.config.max_nfs_port;
        let mut last_error: Option<std::io::Error> = None;

        // Iterate sequentially over the configured range. Random selection
        // would be marginally faster on average but makes failure modes
        // (port conflicts, exhaustion) harder to reason about.
        for port in min..=max {
            let adapter = NfsAdapter::new(fs.clone());
            match NFSTcpListener::bind(&format!("127.0.0.1:{port}"), adapter).await {
                Ok(listener) => {
                    let handle = tokio::spawn(async move {
                        if let Err(e) = listener.handle_forever().await {
                            warn!(error = ?e, "NFS server exited with error");
                        }
                    });
                    info!(
                        path = %working_copy_path,
                        port,
                        "NFS server bound on localhost"
                    );
                    return Ok((
                        TransportInfo::Nfs { port },
                        MountAttachment::Nfs(NfsAttachment { handle }),
                    ));
                }
                Err(e) => {
                    last_error = Some(e);
                }
            }
        }
        Err(BindError::NfsBind {
            min,
            max,
            last_error: last_error.unwrap_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "empty NFS port range",
                )
            }),
        })
    }
}
