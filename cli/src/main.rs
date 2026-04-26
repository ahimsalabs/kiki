#![deny(warnings)]

use jj_cli::{
    cli_util::{CliRunner, CommandHelper},
    command_error::{cli_error, internal_error_with_message, user_error_with_message, CommandError},
    ui::Ui,
};
use jj_lib::{
    file_util,
    ref_name::WorkspaceName,
    repo::{ReadonlyRepo, StoreFactories},
    signing::Signer,
    workspace::{WorkingCopyFactories, Workspace, WorkspaceInitError},
};

mod backend;
mod blocking_client;
mod working_copy;

use backend::YakBackend;
use working_copy::{YakWorkingCopy, YakWorkingCopyFactory};

/// Create a new repo in the given directory
/// If the given directory does not exist, it will be created. If no directory
/// is given, the current directory is used.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct InitArgs {
    /// The destination directory
    #[arg(value_hint = clap::ValueHint::Url)]
    remote: String,

    /// The destination directory
    #[arg(default_value = ".", value_hint = clap::ValueHint::DirPath)]
    destination: String,
}

#[derive(Debug, Clone, clap::Subcommand)]
enum YakCommands {
    Init(InitArgs),
    Status,
}

#[derive(Debug, Clone, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
#[command(flatten_help = true)]
struct YakArgs {
    #[command(subcommand)]
    command: YakCommands,
}

#[derive(clap::Parser, Clone, Debug)]
enum YakSubcommand {
    /// Commands for working with the yak daemon
    Yak(YakArgs),
}

fn create_store_factories() -> StoreFactories {
    let mut store_factories = StoreFactories::empty();
    // Register the backend so it can be loaded when the repo is loaded. The name
    // must match `Backend::name()`.
    store_factories.add_backend(
        "yak",
        // The factory closure returns BackendLoadError; map BackendInitError
        // (which is what YakBackend::new produces) into it preserving the
        // underlying error.
        Box::new(|settings, store_path| {
            let backend = YakBackend::new(settings, store_path)
                .map_err(|jj_lib::backend::BackendInitError(e)| {
                    jj_lib::backend::BackendLoadError(e)
                })?;
            Ok(Box::new(backend))
        }),
    );
    store_factories
}

async fn run_yak_command(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    command: YakSubcommand,
) -> Result<(), CommandError> {
    let YakSubcommand::Yak(YakArgs { command }) = command;

    let grpc_port = command_helper
        .settings()
        .get::<usize>("grpc_port")
        .map_err(|e| user_error_with_message("grpc_port not configured in jj config", e))?;
    let client = crate::blocking_client::BlockingJujutsuInterfaceClient::connect(format!(
        "http://[::1]:{grpc_port}"
    ))
    .map_err(|e| {
        user_error_with_message(format!("Failed to connect to yak daemon on port {grpc_port}"), e)
    })?;
    match command {
        YakCommands::Status => {
            let resp = client
                .daemon_status(proto::jj_interface::DaemonStatusReq {})
                .map_err(|e| internal_error_with_message("daemon DaemonStatus RPC failed", e))?;
            ui.request_pager();
            let mut formatter = ui.stdout_formatter();
            for session in resp.into_inner().data {
                writeln!(formatter, "{} - {}", session.path, session.remote)?;
            }
            Ok(())
        }
        YakCommands::Init(args) => {
            if command_helper.global_args().ignore_working_copy {
                return Err(cli_error("--ignore-working-copy is not respected"));
            }
            if command_helper.global_args().at_operation.is_some() {
                return Err(cli_error("--at-op is not respected"));
            }
            let cwd = command_helper.cwd();
            let wc_path = cwd.join(&args.destination);
            let wc_path = file_util::create_or_reuse_dir(&wc_path)
                .and_then(|_| wc_path.canonicalize())
                .map_err(|e| user_error_with_message("Failed to create workspace", e))?;

            let wc_path_str = wc_path.as_os_str().to_str().ok_or_else(|| {
                user_error_with_message(
                    format!("Workspace path is not valid UTF-8: {}", wc_path.display()),
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-UTF-8 path"),
                )
            })?;

            // NOTE: We need to tell the daemon to mount the filesystem BEFORE we
            // initalize the core jj internals or we'll have writes on-disk and on
            // vfs.
            let init_reply = client
                .initialize(proto::jj_interface::InitializeReq {
                    remote: args.remote,
                    path: wc_path_str.to_string(),
                })
                .map_err(|e| internal_error_with_message("daemon Initialize RPC failed", e))?
                .into_inner();
            // The daemon picks the transport per platform (FUSE on Linux,
            // NFS on macOS) and reports it back here. On Linux the mount
            // already happened daemon-side; on macOS we shell out to
            // `mount_nfs`. With `disable_mount = true` (integration tests),
            // the daemon returns `transport = None`.
            attach_transport(init_reply.transport, &wc_path)?;

            Workspace::init_with_factories(
                command_helper.settings(),
                &wc_path,
                &|settings, store_path| {
                    let backend = YakBackend::new(settings, store_path)?;
                    Ok(Box::new(backend))
                },
                Signer::from_settings(command_helper.settings())
                    .map_err(WorkspaceInitError::SignInit)?,
                ReadonlyRepo::default_op_store_initializer(),
                ReadonlyRepo::default_op_heads_store_initializer(),
                ReadonlyRepo::default_index_store_initializer(),
                ReadonlyRepo::default_submodule_store_initializer(),
                // M2: route Workspace::init_with_factories through
                // YakWorkingCopyFactory so the freshly-initialised workspace
                // talks to the daemon (SetCheckoutState) rather than spinning
                // up a local on-disk working copy. The daemon mount is
                // guaranteed to exist here because the Initialize RPC above
                // ran first.
                &YakWorkingCopyFactory {},
                WorkspaceName::DEFAULT.to_owned(),
            )
            .await?;

            let relative_wc_path = file_util::relative_path(cwd, &wc_path);
            writeln!(
                ui.status(),
                "Initialized repo in \"{}\"",
                relative_wc_path.display()
            )?;

            Ok(())
        }
    }
}

/// Finalize whatever transport the daemon chose. On Linux (Fuse) the
/// daemon already attached the mount; on macOS (Nfs) we shell out to
/// `mount_nfs`. `None` is the test-mode reply (`disable_mount = true`):
/// nothing to do.
fn attach_transport(
    transport: Option<proto::jj_interface::initialize_reply::Transport>,
    wc_path: &std::path::Path,
) -> Result<(), CommandError> {
    use proto::jj_interface::initialize_reply::Transport;
    match transport {
        None => Ok(()),
        Some(Transport::Fuse(_)) => Ok(()),
        Some(Transport::Nfs(nfs)) => mount_nfs_localhost(wc_path, nfs.port),
    }
}

#[cfg(target_os = "macos")]
fn mount_nfs_localhost(wc_path: &std::path::Path, port: u32) -> Result<(), CommandError> {
    let port_arg = format!(
        "port={port},mountport={port},nolocks,vers=3,actimeo=0"
    );
    let status = std::process::Command::new("mount_nfs")
        .arg("-o")
        .arg(&port_arg)
        .arg("localhost:/")
        .arg(wc_path)
        .status()
        .map_err(|e| {
            internal_error_with_message("failed to spawn mount_nfs", e)
        })?;
    if !status.success() {
        return Err(internal_error_with_message(
            format!(
                "mount_nfs exited with status {status} (port={port}, path={})",
                wc_path.display()
            ),
            std::io::Error::new(std::io::ErrorKind::Other, "mount_nfs failed"),
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn mount_nfs_localhost(_wc_path: &std::path::Path, _port: u32) -> Result<(), CommandError> {
    // The daemon should never return Nfs on a non-macOS host (vfs_mgr
    // gates `bind_nfs` behind `cfg(target_os = "macos")`), but if it
    // somehow does, surface it cleanly rather than silently no-op'ing.
    Err(internal_error_with_message(
        "daemon returned NFS transport on non-macOS host",
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "NFS transport on non-macOS",
        ),
    ))
}

fn main() -> std::process::ExitCode {
    let mut working_copy_factories = WorkingCopyFactories::new();
    working_copy_factories.insert(
        YakWorkingCopy::name().to_owned(),
        Box::new(YakWorkingCopyFactory {}),
    );
    // NOTE: logging before this point will not work since it is
    // initialized by CliRunner.
    CliRunner::init()
        .add_store_factories(create_store_factories())
        .add_working_copy_factories(working_copy_factories)
        .add_subcommand(run_yak_command)
        .run()
        .into()
}
