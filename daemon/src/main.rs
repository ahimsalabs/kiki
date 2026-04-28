use std::path::PathBuf;

use anyhow::{anyhow, Context};
use serde::Deserialize;
use tonic::transport::Server as GrpcServer;
use tracing::{error, info};

use crate::service::StorageConfig;

mod hash;
mod mount_meta;
mod service;
mod store;
mod ty;
mod vfs;
mod vfs_mgr;

use clap::Parser;
use vfs_mgr::*;

/// JJ Daemon
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Configuration
    #[arg(short, long)]
    config: PathBuf,
}

#[derive(Deserialize, Debug)]
struct Config {
    /// Address the jj CLI connects over
    pub grpc_addr: String,
    /// Per-mount durable storage root directory (M8 / Layer B). Each
    /// mount's redb file lives at
    /// `<storage_dir>/mounts/<hash(working_copy_path)>/store.redb`.
    /// Must be writable; created on demand. Replaces the pre-M8 `cache`
    /// field — old `daemon.toml` files need to rename `cache` to
    /// `storage_dir` (or keep both: `cache` is now ignored).
    pub storage_dir: PathBuf,
    /// Pre-M8 alias for `storage_dir`. Accepted but ignored — kept so old
    /// `daemon.toml` files still parse during migration. Remove once
    /// every consumer has switched.
    #[serde(default)]
    #[allow(dead_code)]
    pub cache: Option<PathBuf>,
    /// NFS configuration
    pub nfs: NfsConfig,
    /// Skip mountpoint validation and the VFS attach in `Initialize`. The
    /// service still tracks per-mount state and answers RPCs; it just
    /// doesn't actually mount a filesystem at `working_copy_path`.
    ///
    /// Default `false`. Set to `true` in integration tests (M4) until the
    /// VFS write path lands at M6 — without it, jj-lib's `.jj/`
    /// scaffolding writes via `Workspace::init_with_factories` would hit
    /// the FUSE/NFS mount and fail with ENOSYS. Real users get the real
    /// mount.
    #[serde(default)]
    pub disable_mount: bool,
}

#[derive(Deserialize, Debug)]
struct NfsConfig {
    /// Minimum of the port range an NFS mount can be served over.
    /// Inclusive. Stored as `u16` because `mount_nfs`'s `port=` option is
    /// a 16-bit TCP port.
    pub min_port: u16,
    /// Maximum of the port range an NFS mount can be served over. Inclusive.
    pub max_port: u16,
}

async fn run_with_config(config: Config) -> Result<(), anyhow::Error> {
    info!("Starting daemon with configuration: {config:#?}");

    let addr = config.grpc_addr.parse()?;

    let mut vfs_mgr = VfsManager::new(VfsManagerConfig {
        min_nfs_port: config.nfs.min_port,
        max_nfs_port: config.nfs.max_port,
    });
    // Hand the gRPC service a handle so `Initialize` can drive the
    // manager. Cloning is cheap (mpsc sender); there's only one consumer
    // today but the type is ready for more. With `disable_mount = true`
    // the service never sees the handle and skips the validate+bind step
    // entirely (test-only path; see `Config.disable_mount`).
    let vfs_handle = (!config.disable_mount).then(|| vfs_mgr.handle());
    if config.disable_mount {
        info!("disable_mount=true: skipping mountpoint validation and VFS attach");
    }

    let storage = StorageConfig::on_disk(config.storage_dir.clone());
    let svc = service::JujutsuService::new(vfs_handle, storage);

    // Re-attach mounts left behind by a previous daemon process.
    // Done before we start the gRPC listener so an early `Initialize`
    // can't race with the rehydrate scan.
    svc.rehydrate()
        .await
        .context("rehydrating persisted mounts")?;

    let reflection_svc = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
        .build()?;

    info!("Serving jj gRPC interface");
    let grpc_fut = GrpcServer::builder()
        .add_service(reflection_svc)
        .add_service(svc.into_server())
        .serve(addr);

    let vfs_fut = vfs_mgr.serve();
    tokio::select! {
        () = vfs_fut => {
            // VfsManager::serve only returns when every handle has been
            // dropped — i.e. the gRPC service is gone too. Treat as a
            // clean shutdown trigger rather than a panic.
            info!("VFS manager exited; shutting down");
            Ok(())
        }
        ret = grpc_fut => {
            // The gRPC server normally runs forever. Returning means
            // the listener died (bind failure, runtime drop, etc.) —
            // surface it as an error with context rather than panicking
            // with the Debug repr of `Result<(), tonic::transport::Error>`.
            match ret {
                Ok(()) => {
                    error!("gRPC server exited unexpectedly without error");
                    Err(anyhow!("gRPC server exited unexpectedly"))
                }
                Err(e) => {
                    error!(error = %e, "gRPC server failed");
                    Err(e).context("gRPC server failed")
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let args = Args::parse();

    let contents = std::fs::read_to_string(&args.config)
        .map_err(|e| anyhow!("Could not read {}: {}", args.config.display(), e))?;

    let config: Config = toml::from_str(&contents)?;

    tracing_log::LogTracer::init()?;

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_target(true)
        .with_env_filter(env_filter)
        .finish();

    // use that subscriber to process traces emitted after this point
    tracing::subscriber::set_global_default(subscriber)?;

    run_with_config(config).await
}
