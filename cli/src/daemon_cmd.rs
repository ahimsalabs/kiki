//! `kiki kk daemon` subcommands.

use std::path::PathBuf;

use jj_cli::command_error::{user_error, CommandError};

/// Arguments for `kiki kk daemon`.
#[derive(Debug, Clone, clap::Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommands,
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum DaemonCommands {
    /// Run the daemon in the foreground
    Run(DaemonRunArgs),
    /// Show daemon status (PID, uptime, socket path, mounts)
    Status,
    /// Stop the running daemon (sends SIGTERM)
    Stop,
    /// Print the resolved socket path and exit
    SocketPath,
    /// Tail the daemon log file
    Logs(DaemonLogsArgs),
}

#[derive(Debug, Clone, clap::Args)]
pub struct DaemonRunArgs {
    /// Indicates this daemon was auto-started by the CLI (writes PID file,
    /// logs to file instead of stderr)
    #[arg(long)]
    pub managed: bool,

    /// Path to optional config file (legacy / power-user)
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// TCP gRPC address for remote access (e.g. "[::1]:12000")
    #[arg(long)]
    pub grpc_addr: Option<String>,

    /// Storage directory override
    #[arg(long)]
    pub storage_dir: Option<PathBuf>,

    /// Skip FUSE/NFS mount (test mode)
    #[arg(long)]
    pub disable_mount: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub struct DaemonLogsArgs {
    /// Number of lines to show (default: 50)
    #[arg(short = 'n', long, default_value = "50")]
    pub lines: usize,

    /// Follow the log (like tail -f)
    #[arg(short, long)]
    pub follow: bool,
}

/// Optional TOML config file format for `~/.config/kiki/config.toml`.
#[derive(serde::Deserialize, Debug, Default)]
pub struct KikiConfig {
    /// TCP gRPC address for remote daemon access.
    pub grpc_addr: Option<String>,
    /// Override storage directory.
    pub storage_dir: Option<PathBuf>,
    /// Mount root for the managed workspace namespace (M12).
    /// Default: `/mnt/kiki` on Linux, `~/kiki` on macOS.
    pub mount_root: Option<PathBuf>,
    /// Skip VFS mount (test/debug mode).
    #[serde(default)]
    pub disable_mount: bool,
    /// NFS configuration (macOS).
    #[serde(default)]
    pub nfs: Option<NfsConfigToml>,
}

#[derive(serde::Deserialize, Debug)]
pub struct NfsConfigToml {
    pub min_port: Option<u16>,
    pub max_port: Option<u16>,
}

/// Read the configured `mount_root` from `config.toml`, falling back to
/// the platform default. Used by CLI commands that need to construct or
/// display managed workspace paths.
#[allow(dead_code)]
pub fn configured_mount_root() -> std::path::PathBuf {
    let config_path = store::paths::config_path();
    if config_path.exists()
        && let Ok(contents) = std::fs::read_to_string(&config_path)
        && let Ok(cfg) = toml::from_str::<KikiConfig>(&contents)
        && let Some(root) = cfg.mount_root
    {
        return root;
    }
    store::paths::default_mount_root()
}

/// Execute a daemon subcommand. Returns Ok(true) if the command was handled
/// (caller should exit), Ok(false) if it should fall through to jj dispatch.
pub fn dispatch_daemon(args: &DaemonArgs) -> Result<(), CommandError> {
    match &args.command {
        DaemonCommands::Run(run_args) => run_daemon_foreground(run_args),
        DaemonCommands::Status => show_status(),
        DaemonCommands::Stop => stop_daemon(),
        DaemonCommands::SocketPath => {
            println!("{}", store::paths::socket_path().display());
            Ok(())
        }
        DaemonCommands::Logs(logs_args) => show_logs(logs_args),
    }
}

fn apply_file_config(config: &mut kiki_daemon::DaemonConfig, file_config: KikiConfig) {
    if let Some(addr) = file_config.grpc_addr {
        config.grpc_addr = Some(addr);
    }
    if let Some(dir) = file_config.storage_dir {
        config.storage_dir = dir;
    }
    if let Some(root) = file_config.mount_root {
        config.mount_root = root;
    }
    if file_config.disable_mount {
        config.disable_mount = true;
    }
    if let Some(nfs) = file_config.nfs {
        if let Some(min) = nfs.min_port {
            config.nfs_min_port = min;
        }
        if let Some(max) = nfs.max_port {
            config.nfs_max_port = max;
        }
    }
}

fn run_daemon_foreground(args: &DaemonRunArgs) -> Result<(), CommandError> {
    // Build DaemonConfig from defaults + optional config file + CLI flags.
    let mut config = kiki_daemon::DaemonConfig::with_defaults();
    config.managed = args.managed;

    // Layer config file if it exists.
    if let Some(ref config_path) = args.config {
        let contents = std::fs::read_to_string(config_path).map_err(|e| {
            user_error(format!("failed to read config {}: {e}", config_path.display()))
        })?;
        let file_config: KikiConfig = toml::from_str(&contents).map_err(|e| {
            user_error(format!("failed to parse config {}: {e}", config_path.display()))
        })?;
        apply_file_config(&mut config, file_config);
    } else {
        // Try default config path.
        let default_config = store::paths::config_path();
        if default_config.exists()
            && let Ok(contents) = std::fs::read_to_string(&default_config)
            && let Ok(file_config) = toml::from_str::<KikiConfig>(&contents)
        {
            apply_file_config(&mut config, file_config);
        }
    }

    // CLI flag overrides.
    if let Some(ref addr) = args.grpc_addr {
        config.grpc_addr = Some(addr.clone());
    }
    if let Some(ref dir) = args.storage_dir {
        config.storage_dir = dir.clone();
    }
    if args.disable_mount {
        config.disable_mount = true;
    }

    // Set up tracing.
    tracing_log::LogTracer::init().ok();
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
    tracing::subscriber::set_global_default(subscriber).ok();

    // Run the daemon (blocking).
    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| user_error(format!("failed to create tokio runtime: {e}")))?;
    rt.block_on(kiki_daemon::run_daemon(config))
        .map_err(|e| user_error(format!("daemon error: {e}")))?;

    Ok(())
}

fn show_status() -> Result<(), CommandError> {
    let socket = store::paths::socket_path();
    let pid_path = store::paths::pid_path();

    println!("Socket: {}", socket.display());
    println!("PID file: {}", pid_path.display());

    // Check PID file.
    match std::fs::read_to_string(&pid_path) {
        Ok(contents) => {
            let pid_str = contents.trim();
            println!("PID: {pid_str}");
            if let Ok(pid) = pid_str.parse::<u32>() {
                let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
                if alive {
                    println!("Status: running");
                    // Try to connect and get mount count.
                    if let Ok(client) =
                        crate::blocking_client::BlockingJujutsuInterfaceClient::connect_uds(
                            socket.clone(),
                        )
                        && let Ok(resp) = client.daemon_status(
                            proto::jj_interface::DaemonStatusReq {},
                        )
                    {
                        let status = resp.into_inner();
                        println!("Mounts: {}", status.data.len());
                    }
                } else {
                    println!("Status: stale (process not running)");
                }
            }
        }
        Err(_) => {
            if socket.exists() {
                println!("Status: socket exists but no PID file (unmanaged?)");
            } else {
                println!("Status: not running");
            }
        }
    }

    Ok(())
}

fn stop_daemon() -> Result<(), CommandError> {
    let pid_path = store::paths::pid_path();
    let contents = std::fs::read_to_string(&pid_path).map_err(|_| {
        user_error("no PID file found; daemon may not be running (or was started externally)")
    })?;
    let pid: i32 = contents.trim().parse().map_err(|_| {
        user_error(format!("PID file contains invalid content: {:?}", contents.trim()))
    })?;

    // Send SIGTERM.
    let ret = unsafe { libc::kill(pid, libc::SIGTERM) };
    if ret == 0 {
        println!("Sent SIGTERM to daemon (PID {pid})");
        // Wait briefly for cleanup.
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let alive = unsafe { libc::kill(pid, 0) == 0 };
            if !alive {
                println!("Daemon stopped");
                return Ok(());
            }
        }
        println!("Daemon still running after 2s; may need `kill -9 {pid}`");
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            println!("Process {pid} not found; cleaning up stale PID file");
            let _ = std::fs::remove_file(&pid_path);
            let _ = std::fs::remove_file(store::paths::socket_path());
        } else {
            return Err(user_error(format!("failed to send SIGTERM to {pid}: {err}")));
        }
    }
    Ok(())
}

fn show_logs(args: &DaemonLogsArgs) -> Result<(), CommandError> {
    let log_path = store::paths::log_path();
    if !log_path.exists() {
        return Err(user_error(format!(
            "log file not found at {}",
            log_path.display()
        )));
    }

    if args.follow {
        // Simple tail -f equivalent.
        use std::io::{BufRead, BufReader, Seek, SeekFrom};
        let file = std::fs::File::open(&log_path).map_err(|e| {
            user_error(format!("failed to open log: {e}"))
        })?;
        let mut reader = BufReader::new(file);
        // Seek to end.
        reader.seek(SeekFrom::End(0)).ok();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => std::thread::sleep(std::time::Duration::from_millis(100)),
                Ok(_) => print!("{line}"),
                Err(_) => break,
            }
        }
    } else {
        // Read last N lines.
        let contents = std::fs::read_to_string(&log_path).map_err(|e| {
            user_error(format!("failed to read log: {e}"))
        })?;
        let lines: Vec<&str> = contents.lines().collect();
        let start = lines.len().saturating_sub(args.lines);
        for line in &lines[start..] {
            println!("{line}");
        }
    }
    Ok(())
}
