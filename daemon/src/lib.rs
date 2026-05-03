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
use crate::vfs::RootFs;
use crate::vfs::root_fs::{WorkspaceRegistration, WorkspaceState as RootFsWorkspaceState};
use crate::vfs_mgr::*;

pub mod git_ops;
pub mod git_store;
pub mod hash;
pub mod local_refs;
pub mod mount_meta;
pub mod remote;
pub mod repo_meta;
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
    /// Mount root for the managed workspace namespace (M12).
    /// Default: `/mnt/kiki` on Linux.
    pub mount_root: PathBuf,
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
            mount_root: store::paths::default_mount_root(),
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

    // VFS manager — must be serving before any bind() calls.
    let mut vfs_mgr = VfsManager::new(VfsManagerConfig {
        min_nfs_port: config.nfs_min_port,
        max_nfs_port: config.nfs_max_port,
    });
    let vfs_handle = (!config.disable_mount).then(|| vfs_mgr.handle());
    if config.disable_mount {
        info!("disable_mount=true: skipping mountpoint validation and VFS attach");
    }
    // Spawn the VFS manager task so its serve() loop is polling the
    // channel before we send any bind requests (M12 §12.10 step 5).
    let vfs_task = tokio::spawn(async move { vfs_mgr.serve().await });

    // Storage and service.
    let storage = StorageConfig::on_disk(config.storage_dir.clone());
    let mut svc = service::JujutsuService::new(vfs_handle.clone(), storage);
    svc.rehydrate()
        .await
        .context("rehydrating persisted mounts")?;

    // ── M12: managed workspace rehydration ─────────────────────────
    //
    // Read repos.toml, reconstruct RootFs with all registered repos
    // and workspaces, bind the single FUSE mount at mount_root, and
    // hand the RootFs to the service so M12 RPCs work.
    let root_fs = rehydrate_root_fs(&config).context("rehydrating managed workspaces")?;
    let mount_attachment = if !config.disable_mount {
        if let Some(ref handle) = vfs_handle {
            // Ensure mount_root exists. If the parent isn't writable (e.g.
            // /mnt/kiki and /mnt isn't user-owned), skip gracefully — the
            // daemon still works for ad-hoc mounts and gRPC-only access.
            let mount_root_ready = if config.mount_root.exists() {
                true
            } else {
                match std::fs::create_dir_all(&config.mount_root) {
                    Ok(()) => true,
                    Err(e) => {
                        warn!(
                            error = %e,
                            mount_root = %config.mount_root.display(),
                            "cannot create mount root; RootFs FUSE mount skipped"
                        );
                        false
                    }
                }
            };
            if mount_root_ready {
                let mount_path = config.mount_root.to_string_lossy().to_string();
                match handle.bind(mount_path, Arc::clone(&root_fs) as Arc<dyn crate::vfs::JjKikiFs>).await {
                    Ok((_transport, attachment)) => {
                        info!(mount_root = %config.mount_root.display(), "RootFs FUSE mount bound");
                        Some(attachment)
                    }
                    Err(e) => {
                        warn!(
                            error = %format!("{e:#}"),
                            mount_root = %config.mount_root.display(),
                            "failed to bind RootFs FUSE mount; managed workspaces \
                             accessible via gRPC only"
                        );
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        info!("disable_mount=true: skipping RootFs FUSE mount");
        None
    };
    svc.set_root_fs(root_fs, config.mount_root.clone(), mount_attachment);

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

    // Main select loop.
    let result = tokio::select! {
        res = vfs_task => {
            match res {
                Ok(()) => info!("VFS manager exited; shutting down"),
                Err(e) => error!(error = %e, "VFS manager task panicked"),
            }
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

/// Reconstruct the [`RootFs`] from persisted `repos.toml` and per-workspace
/// `workspace.toml` files. Does not bind a FUSE mount — the caller does
/// that. Safe to call on first run (no `repos.toml` → empty `RootFs`).
///
/// Failures for individual repos/workspaces are logged and skipped so that
/// one corrupt workspace doesn't prevent the daemon from starting.
fn rehydrate_root_fs(config: &DaemonConfig) -> anyhow::Result<Arc<RootFs>> {
    use crate::git_store::GitContentStore;
    use crate::repo_meta;

    let repos_path = repo_meta::repos_config_path(&config.storage_dir);
    let repos_cfg = repo_meta::ReposConfig::read_or_default(&repos_path)
        .context("reading repos.toml")?;

    let root_fs = Arc::new(RootFs::new(
        config.mount_root.clone(),
        config.storage_dir.clone(),
        repos_cfg.next_slot,
    ));

    if repos_cfg.repos.is_empty() {
        info!("no managed repos in repos.toml (first run or empty)");
        return Ok(root_fs);
    }

    let settings = crate::service::default_user_settings();

    for (repo_name, repo_entry) in &repos_cfg.repos {
        // Open the shared git store + redb for this repo.
        let git_store_path =
            repo_meta::repo_git_store_path(&config.storage_dir, repo_name);
        let redb_path = repo_meta::repo_redb_path(&config.storage_dir, repo_name);

        let store = match GitContentStore::load(&settings, &git_store_path, &redb_path) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                warn!(
                    repo = %repo_name,
                    error = %format!("{e:#}"),
                    "skipping repo: failed to open git store"
                );
                continue;
            }
        };

        // Parse remote store (dir://, s3://, kiki+ssh:// etc.).
        // SSH remotes are not established at startup — they require an
        // async tunnel handshake. The Clone/WorkspaceCreate RPCs handle
        // SSH setup lazily. For now, only synchronous remote stores
        // (dir://, s3://) are wired up at rehydration.
        let remote_store = match crate::remote::parse(&repo_entry.url) {
            Ok(rs) => rs,
            Err(e) => {
                warn!(
                    repo = %repo_name,
                    url = %repo_entry.url,
                    error = %format!("{e:#}"),
                    "skipping remote store setup (repo still accessible locally)"
                );
                None
            }
        };

        root_fs.register_repo(
            repo_name.clone(),
            repo_entry.url.clone(),
            Arc::clone(&store),
            remote_store,
        );

        // Scan workspaces for this repo.
        let workspaces = match repo_meta::list_workspace_configs(
            &config.storage_dir,
            repo_name,
        ) {
            Ok(ws) => ws,
            Err(e) => {
                warn!(
                    repo = %repo_name,
                    error = %format!("{e:#}"),
                    "failed to list workspaces; repo registered but empty"
                );
                continue;
            }
        };

        for (ws_name, ws_cfg) in workspaces {
            // Clean up stale pending workspaces (CLI crashed before
            // finalize). §12.10: "pending workspaces are either cleaned
            // up automatically or left for manual cleanup."
            if ws_cfg.state == repo_meta::WorkspaceState::Pending {
                info!(
                    repo = %repo_name,
                    workspace = %ws_name,
                    "skipping pending workspace (not finalized)"
                );
                continue;
            }

            let state = match ws_cfg.state {
                repo_meta::WorkspaceState::Active => RootFsWorkspaceState::Active,
                repo_meta::WorkspaceState::Pending => RootFsWorkspaceState::Pending,
            };

            if let Err(e) = root_fs.register_workspace(
                repo_name,
                WorkspaceRegistration {
                    name: ws_name.clone(),
                    slot: ws_cfg.slot,
                    state,
                    root_tree_id: ws_cfg.root_tree_id,
                    op_id: ws_cfg.op_id,
                    workspace_id: ws_cfg.workspace_id,
                },
            ) {
                warn!(
                    repo = %repo_name,
                    workspace = %ws_name,
                    error = %e,
                    "failed to register workspace"
                );
            }
        }

        let ws_count = root_fs
            .list_repos()
            .iter()
            .find(|(n, _, _)| n == repo_name)
            .map(|(_, _, ws)| ws.len())
            .unwrap_or(0);
        info!(
            repo = %repo_name,
            url = %repo_entry.url,
            workspaces = ws_count,
            "rehydrated repo"
        );
    }

    Ok(root_fs)
}
