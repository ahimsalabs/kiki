use std::path::PathBuf;

use anyhow::anyhow;
use serde::Deserialize;
use tonic::transport::Server as GrpcServer;
use tracing::info;

mod hash;
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
    /// Local cache. Required in the config file but not yet consumed by the
    /// daemon — kept here so existing `daemon.toml` files continue to parse
    /// and so the field shows up in the `Debug` log line at startup.
    #[allow(dead_code)]
    pub cache: PathBuf,
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

    let jj_svc = service::JujutsuService::new(vfs_handle);

    let reflection_svc = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
        .build()?;

    info!("Serving jj gRPC interface");
    let grpc_fut = GrpcServer::builder()
        .add_service(reflection_svc)
        .add_service(jj_svc)
        .serve(addr);

    let vfs_fut = vfs_mgr.serve();
    tokio::select! {
        () = vfs_fut => {
            // VfsManager::serve only returns when every handle has been
            // dropped — i.e. the gRPC service is gone too. Treat as a
            // clean shutdown trigger rather than a panic.
            info!("VFS manager exited; shutting down");
        }
        ret = grpc_fut => {
            panic!("GRPC: {:?}", ret );
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let args = Args::parse();

    let contents = std::fs::read_to_string(&args.config)
        .map_err(|e| anyhow!("Could not read {}: {}", args.config.display(), e))?;

    let config: Config = toml::from_str(&contents)?;

    tracing_log::LogTracer::init()?;

    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_target(false)
        .finish();

    // use that subscriber to process traces emitted after this point
    tracing::subscriber::set_global_default(subscriber)?;

    run_with_config(config).await
}
