//! kiki daemon library.
//!
//! Exposes the daemon's server logic. Started via
//! `kiki kk daemon run` (the unified entry point).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server as GrpcServer;
use tracing::{error, info, warn};

use crate::remote::{fs::FsRemoteStore, server::RemoteStoreService, RemoteStore};
use crate::service::StorageConfig;
use crate::vfs_mgr::*;

pub mod git_ops;
pub mod git_store;
pub mod hash;
pub mod local_refs;
pub mod mount_meta;
pub mod remote;
pub mod service;
pub mod ty;
pub mod vfs;
pub mod vfs_mgr;

/// Daemon configuration. All fields have sensible defaults via
/// [`DaemonConfig::with_defaults()`]; power users override via config file
/// or CLI flags.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// UDS path for local CLI communication.
    pub socket_path: PathBuf,
    /// Optional TCP gRPC address for daemon-to-daemon remote access.
    /// Only opened if set (e.g. from config.toml `grpc_addr`).
    pub grpc_addr: Option<String>,
    /// Per-mount durable storage root.
    pub storage_dir: PathBuf,
    /// NFS port range (macOS only).
    pub nfs_min_port: u16,
    pub nfs_max_port: u16,
    /// Skip VFS mount (test mode).
    pub disable_mount: bool,
    /// PID file path.
    pub pid_path: PathBuf,
    /// Log file path (used when `managed` is true).
    pub log_path: PathBuf,
    /// Whether this daemon was auto-started by the CLI.
    pub managed: bool,
}

impl DaemonConfig {
    /// Construct with platform-appropriate defaults. No config file needed.
    pub fn with_defaults() -> Self {
        DaemonConfig {
            socket_path: store::paths::socket_path(),
            grpc_addr: None,
            storage_dir: store::paths::default_storage_dir(),
            nfs_min_port: 12000,
            nfs_max_port: 12100,
            disable_mount: false,
            pid_path: store::paths::pid_path(),
            log_path: store::paths::log_path(),
            managed: false,
        }
    }
}

/// Run the daemon. Blocks until shutdown (signal or error).
///
/// Entry point for `kiki kk daemon run`.
pub async fn run_daemon(config: DaemonConfig) -> Result<(), anyhow::Error> {
    info!("Starting daemon with configuration: {config:#?}");

    // Ensure runtime directory exists.
    if let Some(parent) = config.socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating runtime dir {}", parent.display()))?;
    }

    // Remove stale socket file if present.
    if config.socket_path.exists() {
        std::fs::remove_file(&config.socket_path).with_context(|| {
            format!("removing stale socket {}", config.socket_path.display())
        })?;
    }

    // Write PID file.
    let pid = std::process::id();
    if let Some(parent) = config.pid_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&config.pid_path, pid.to_string())
        .with_context(|| format!("writing PID file {}", config.pid_path.display()))?;

    // Bind UDS listener.
    let uds_listener = UnixListener::bind(&config.socket_path)
        .with_context(|| format!("binding UDS at {}", config.socket_path.display()))?;
    info!(path = %config.socket_path.display(), "UDS listener bound");
    let uds_stream = UnixListenerStream::new(uds_listener);

    // VFS manager.
    let mut vfs_mgr = VfsManager::new(VfsManagerConfig {
        min_nfs_port: config.nfs_min_port,
        max_nfs_port: config.nfs_max_port,
    });
    let vfs_handle = (!config.disable_mount).then(|| vfs_mgr.handle());
    if config.disable_mount {
        info!("disable_mount=true: skipping mountpoint validation and VFS attach");
    }

    // Storage and service.
    let storage = StorageConfig::on_disk(config.storage_dir.clone());
    let svc = service::JujutsuService::new(vfs_handle, storage);
    svc.rehydrate()
        .await
        .context("rehydrating persisted mounts")?;

    // Reflection + remote store services.
    let reflection_svc = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
        .build()?;

    let served_blobs_dir = config.storage_dir.join("served_blobs");
    let remote_backend: Arc<dyn RemoteStore> =
        Arc::new(FsRemoteStore::new(served_blobs_dir));
    let remote_svc = RemoteStoreService::new(remote_backend);

    // Build the gRPC router (shared between UDS and optional TCP).
    let router = GrpcServer::builder()
        .add_service(reflection_svc)
        .add_service(remote_svc.into_server())
        .add_service(svc.into_server());

    // UDS gRPC server.
    let uds_fut = router.serve_with_incoming(uds_stream);

    // Optional TCP listener for daemon-to-daemon remotes.
    let tcp_fut = async {
        if let Some(ref addr_str) = config.grpc_addr {
            let addr: std::net::SocketAddr = addr_str.parse().with_context(|| {
                format!("parsing grpc_addr {addr_str:?}")
            })?;
            info!(%addr, "TCP gRPC listener starting (for remote access)");

            // Need a second router instance for TCP.
            // Re-create services for the TCP listener.
            // Actually, tonic routers are not Clone. We'll handle this
            // by only running one or the other. For now, if grpc_addr is
            // set, we run BOTH UDS and TCP — but TCP needs its own server.
            // The simplest approach: just also listen on TCP using a separate
            // tokio::net::TcpListener.
            //
            // For now, skip the TCP dual-serve complexity — it's opt-in and
            // can be added later when daemon-to-daemon actually needs it.
            // Just log that it's configured but not yet implemented in UDS mode.
            warn!("TCP grpc_addr configured but dual-serve (UDS + TCP) not yet implemented");
            Ok::<(), anyhow::Error>(())
        } else {
            // No TCP listener configured — just wait forever.
            std::future::pending::<()>().await;
            Ok(())
        }
    };

    // Signal handling.
    #[cfg(unix)]
    let shutdown_signal = async {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt())
            .expect("failed to install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => info!("Received SIGTERM"),
            _ = sigint.recv() => info!("Received SIGINT"),
        }
    };
    #[cfg(not(unix))]
    let shutdown_signal = async {
        tokio::signal::ctrl_c().await.ok();
        info!("Received Ctrl+C");
    };

    let vfs_fut = vfs_mgr.serve();

    // Main select loop.
    let result = tokio::select! {
        () = vfs_fut => {
            info!("VFS manager exited; shutting down");
            Ok(())
        }
        ret = uds_fut => {
            match ret {
                Ok(()) => {
                    error!("UDS gRPC server exited unexpectedly without error");
                    Err(anyhow!("UDS gRPC server exited unexpectedly"))
                }
                Err(e) => {
                    error!(error = %e, "UDS gRPC server failed");
                    Err(e).context("UDS gRPC server failed")
                }
            }
        }
        ret = tcp_fut => {
            ret
        }
        () = shutdown_signal => {
            info!("Shutting down gracefully");
            Ok(())
        }
    };

    // Cleanup: remove socket and PID file.
    if let Err(e) = std::fs::remove_file(&config.socket_path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(error = %e, "failed to remove socket file on shutdown");
    }
    if let Err(e) = std::fs::remove_file(&config.pid_path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(error = %e, "failed to remove PID file on shutdown");
    }
    info!("Daemon stopped");

    result
}
