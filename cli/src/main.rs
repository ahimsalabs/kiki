#![deny(warnings)]

use jj_cli::{
    cli_util::{BoxedAsyncCliDispatch, CliRunner, CommandHelper},
    command_error::{
        cli_error, internal_error_with_message, user_error, user_error_with_message, CommandError,
    },
    ui::Ui,
};
use jj_lib::{
    file_util,
    ref_name::WorkspaceName,
    repo::{ReadonlyRepo, Repo, StoreFactories},
    signing::Signer,
    workspace::{WorkingCopyFactories, Workspace, WorkspaceInitError},
};

mod backend;
mod blocking_client;
mod daemon_client;
mod daemon_cmd;
mod doctor;
mod op_heads_store;
mod op_store;
mod working_copy;

use backend::KikiBackend;
use blocking_client::BlockingJujutsuInterfaceClient;
use op_heads_store::KikiOpHeadsStore;
use working_copy::{KikiWorkingCopy, KikiWorkingCopyFactory};

/// Convert a `tonic::Status` into a user-facing `CommandError`, showing only
/// the gRPC message (not the raw code, details, or metadata).
fn rpc_error(context: &str, status: tonic::Status) -> CommandError {
    user_error(format!("{context}: {}", status.message()))
}

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

#[derive(Debug, Clone, clap::Args)]
struct DoctorArgs {
    /// Attempt to repair detected issues
    #[arg(long)]
    fix: bool,
    /// Only check a specific repo
    #[arg(long)]
    repo: Option<String>,
}

#[derive(Debug, Clone, clap::Subcommand)]
enum KikiCommands {
    Init(InitArgs),
    Status,
    /// Check prerequisites and set up the workspace mount root
    Setup,
    /// Daemon lifecycle management
    Daemon(daemon_cmd::DaemonArgs),
    /// Diagnose and repair managed workspace state
    Doctor(DoctorArgs),
}

#[derive(Debug, Clone, clap::Args)]
struct GitArgs {
    #[command(subcommand)]
    command: GitCommands,
}

#[derive(Debug, Clone, clap::Subcommand)]
enum GitCommands {
    /// Manage git remotes
    Remote(GitRemoteArgs),
    /// Push bookmarks to a git remote
    Push(GitPushArgs),
    /// Fetch from a git remote
    Fetch(GitFetchArgs),
}

#[derive(Debug, Clone, clap::Args)]
struct GitRemoteArgs {
    #[command(subcommand)]
    command: GitRemoteCommands,
}

#[derive(Debug, Clone, clap::Subcommand)]
enum GitRemoteCommands {
    /// Add a git remote
    Add(GitRemoteAddArgs),
    /// List git remotes
    List,
}

#[derive(Debug, Clone, clap::Args)]
struct GitRemoteAddArgs {
    /// Remote name
    name: String,
    /// Remote URL
    url: String,
}

#[derive(Debug, Clone, clap::Args)]
struct GitPushArgs {
    /// Remote to push to (defaults to "origin")
    #[arg(long)]
    remote: Option<String>,
    /// Bookmark(s) to push (can be repeated)
    #[arg(long, short, alias = "branch")]
    bookmark: Vec<String>,
    /// Push all bookmarks
    #[arg(long, conflicts_with = "bookmark")]
    all: bool,
}

#[derive(Debug, Clone, clap::Args)]
struct GitFetchArgs {
    /// Remote to fetch from (defaults to "origin")
    #[arg(long)]
    remote: Option<String>,
}

/// Wrapper for re-parsing `kiki git ...` args in the dispatch hook.
#[derive(clap::Parser, Debug)]
#[command(name = "git")]
struct KikiGitCli {
    #[command(subcommand)]
    command: GitCommands,
}

#[derive(Debug, Clone, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
#[command(flatten_help = true)]
struct KikiArgs {
    #[command(subcommand)]
    command: KikiCommands,
}

#[derive(clap::Parser, Clone, Debug)]
enum KikiSubcommand {
    /// Commands for working with the kiki daemon
    Kk(KikiArgs),
    /// Clone a remote repo into the managed workspace namespace
    Clone(CloneCommandArgs),
    /// Manage the kiki-native remote (full-fidelity sync)
    Remote(RemoteCommandArgs),
}

/// Clone a remote repo into the managed namespace (`/mnt/kiki/<name>/default`).
#[derive(clap::Args, Clone, Debug)]
struct CloneCommandArgs {
    /// Clone source URL. Kiki remote (kiki+ssh://, dir://, s3://, kiki://)
    /// or git remote (https://, ssh://, git@host:path).
    #[arg(value_hint = clap::ValueHint::Url)]
    url: String,
    /// Repo name override (default: derived from URL's last path segment)
    #[arg(long)]
    name: Option<String>,
    /// Create a symlink at this path pointing into the managed namespace.
    ///
    /// The repo still lives under /mnt/kiki/<name>/default, but you can
    /// `cd <path>` to work there. All paths are canonicalized before
    /// reaching the daemon, so tools see the real VFS path.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    link: Option<std::path::PathBuf>,
    /// Kiki remote for real-time sync (e.g. s3://bucket/repo, kiki://server:12000).
    /// Only valid when cloning from a git URL.
    #[arg(long, value_hint = clap::ValueHint::Url)]
    remote: Option<String>,
}

/// Classify a clone URL as a kiki-native remote or a git remote.
enum CloneSource {
    /// Kiki-native remote store. The URL is the kiki remote.
    KikiRemote(String),
    /// Git remote. The URL becomes the "origin" git remote.
    Git(String),
}

fn classify_clone_url(url: &str) -> CloneSource {
    // SCP-style: no "://" and contains ":" (e.g. git@github.com:org/repo)
    if !url.contains("://") && url.contains(':') {
        return CloneSource::Git(url.to_string());
    }
    match url.split("://").next().unwrap_or("") {
        "dir" | "s3" | "kiki" | "grpc" | "kiki+ssh" => {
            CloneSource::KikiRemote(url.to_string())
        }
        // Everything else is git: https, http, git, ssh, etc.
        _ => CloneSource::Git(url.to_string()),
    }
}

// ── M13: kiki remote management ─────────────────────────────────────

#[derive(Debug, Clone, clap::Args)]
struct RemoteCommandArgs {
    #[command(subcommand)]
    command: RemoteCommands,
}

#[derive(Debug, Clone, clap::Subcommand)]
enum RemoteCommands {
    /// Attach a kiki remote for full-fidelity sync
    Add(RemoteAddArgs),
    /// Detach the kiki remote
    Remove,
    /// Show the current kiki remote
    Show,
}

#[derive(Debug, Clone, clap::Args)]
struct RemoteAddArgs {
    /// Kiki remote URL (s3://..., kiki://..., kiki+ssh://..., dir://...)
    #[arg(value_hint = clap::ValueHint::Url)]
    url: String,
}

#[derive(Debug, Clone, clap::Args)]
struct WorkspaceCommandArgs {
    #[command(subcommand)]
    command: WorkspaceCommands,
}

#[derive(Debug, Clone, clap::Subcommand)]
enum WorkspaceCommands {
    /// Create a new workspace in a repo
    Create(WorkspaceCreateArgs),
    /// List workspaces in a repo
    List(WorkspaceListArgs),
    /// Delete a workspace from a repo
    Delete(WorkspaceDeleteArgs),
}

#[derive(Debug, Clone, clap::Args)]
struct WorkspaceCreateArgs {
    /// Workspace name, or repo/workspace.
    ///
    /// If a bare name like `fix-auth` is given, the repo is inferred
    /// from the current directory (you must be inside a managed
    /// workspace under mount_root). Use `myrepo/fix-auth` to be
    /// explicit.
    name: String,
    // TODO(M12): --revision flag for checkout target override
}

#[derive(Debug, Clone, clap::Args)]
struct WorkspaceListArgs {
    /// Repo name (omit to infer from cwd, or list all repos)
    repo: Option<String>,
}

#[derive(Debug, Clone, clap::Args)]
struct WorkspaceDeleteArgs {
    /// Workspace name, or repo/workspace.
    ///
    /// If a bare name like `fix-auth` is given, the repo is inferred
    /// from the current directory.
    name: String,
}

fn create_store_factories() -> StoreFactories {
    // Start empty: `CliRunner::add_store_factories` merges these on top
    // of jj-cli's own defaults, and `merge_factories_map` panics on
    // collisions. We only register the kiki-specific factories here.
    let mut store_factories = StoreFactories::empty();
    // Register the backend so it can be loaded when the repo is loaded. The name
    // must match `Backend::name()`.
    store_factories.add_backend(
        "kiki",
        // The factory closure returns BackendLoadError; map BackendInitError
        // (which is what KikiBackend::new produces) into it preserving the
        // underlying error.
        Box::new(|settings, store_path| {
            let backend = KikiBackend::new(settings, store_path)
                .map_err(|jj_lib::backend::BackendInitError(e)| {
                    jj_lib::backend::BackendLoadError(e)
                })?;
            Ok(Box::new(backend))
        }),
    );
    // M10.5: register the KikiOpHeadsStore factory so subsequent loads
    // pick up the catalog-driven impl. The corresponding initializer
    // wired into `Workspace::init_with_factories` writes the
    // `kiki_op_heads` type tag to disk; this loader honors it.
    //
    // The `repo_dir` we receive is `<jj_root>/op_heads/`. We need the
    // workspace path for the daemon RPCs — climb two levels up
    // (`<wc>/.jj/repo/op_heads/`) to recover it.
    store_factories.add_op_heads_store(
        op_heads_store::KikiOpHeadsStore::name(),
        Box::new(|settings, store_path| {
            let working_copy_path = climb_to_workspace(store_path)
                .map_err(|e| jj_lib::backend::BackendLoadError(e.into()))?;
            let client = connect_daemon(settings)
                .map_err(jj_lib::backend::BackendLoadError)?;
            Ok(Box::new(KikiOpHeadsStore::new(client, working_copy_path)))
        }),
    );
    // M10.6: register the KikiOpStore factory so subsequent loads
    // pick up the daemon-routed impl. The `store_path` is
    // `<wc>/.jj/repo/op_store/`; climb 3 levels to recover `<wc>`.
    store_factories.add_op_store(
        op_store::KikiOpStore::name(),
        Box::new(|settings, store_path, root_data| {
            let working_copy_path = climb_to_workspace(store_path)
                .map_err(|e| jj_lib::backend::BackendLoadError(e.into()))?;
            let client = connect_daemon(settings)
                .map_err(jj_lib::backend::BackendLoadError)?;
            Ok(Box::new(op_store::KikiOpStore::load(
                store_path,
                root_data,
                client,
                working_copy_path,
            )))
        }),
    );
    store_factories
}

/// Resolve the workspace path from an `op_heads` store directory.
/// The path is `<wc>/.jj/repo/op_heads/`; we climb three components
/// up to recover `<wc>` and re-canonicalize so the daemon-side
/// `working_copy_path` lookup matches the one stamped at `Initialize`.
fn climb_to_workspace(op_heads_path: &std::path::Path) -> std::io::Result<String> {
    let workspace = op_heads_path
        .ancestors()
        .nth(3)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "op_heads path {} has no .../wc/.jj/repo/op_heads ancestor chain",
                    op_heads_path.display()
                ),
            )
        })?
        .canonicalize()?;
    workspace.into_os_string().into_string().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("workspace path is not valid UTF-8: {e:?}"),
        )
    })
}

/// Connect to the daemon described by user settings. Same shape as
/// `KikiWorkingCopy::connect_client` (cli/src/working_copy.rs:96) — we
/// duplicate the few lines rather than expose a public helper because
/// the error types differ (`BackendLoadError` here, `WorkingCopyStateError`
/// there).
fn connect_daemon(
    settings: &jj_lib::settings::UserSettings,
) -> Result<BlockingJujutsuInterfaceClient, Box<dyn std::error::Error + Send + Sync>> {
    daemon_client::connect_or_start(settings)
}

/// Check if the current workspace uses the kiki backend.
fn is_kiki_backend(command_helper: &CommandHelper) -> bool {
    let Ok(loader) = command_helper.workspace_loader() else {
        return false;
    };
    let type_path = loader.repo_path().join("store").join("type");
    std::fs::read_to_string(type_path)
        .map(|s| s.trim() == "kiki")
        .unwrap_or(false)
}

/// Dispatch hook: detects external git changes (HEAD and bookmark refs) and
/// imports them into jj's graph before the normal command runs.
/// Runs before `kiki_git_dispatch_hook` so the import lands before snapshot.
async fn kiki_git_import_hook(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    old_dispatch: BoxedAsyncCliDispatch<'_>,
) -> Result<(), CommandError> {
    if is_kiki_backend(command_helper) {
        match detect_git_changes(command_helper) {
            Ok(None) => {} // No changes detected — nothing to do.
            Ok(Some(changes)) => {
                // Transaction phase: failures here are fatal (could leave
                // partial state).
                apply_git_imports(ui, command_helper, changes).await?;
            }
            Err(e) => {
                // Detection phase failure (daemon unreachable, RPC error,
                // mount doesn't have git colocated). Non-fatal: warn and
                // let the command proceed.
                writeln!(ui.warning_default(), "git import failed: {e:?}")?;
            }
        }
    }
    old_dispatch.call(ui, command_helper).await
}

/// Detected git changes ready to be applied as a jj transaction.
struct GitChanges {
    new_head_commit_id: Vec<u8>,
    bookmark_changes: Vec<proto::jj_interface::GitBookmarkChange>,
}

/// Detection phase: query the daemon for external git changes.
///
/// Returns `Ok(None)` if no changes detected or not in a workspace.
/// Returns `Err` for daemon/RPC failures (safe to swallow as a warning).
fn detect_git_changes(
    command_helper: &CommandHelper,
) -> Result<Option<GitChanges>, CommandError> {
    let wc_path = match workspace_path(command_helper) {
        Ok(p) => p,
        Err(_) => return Ok(None), // Not in a workspace — nothing to import.
    };
    let settings = command_helper.settings();
    let client = daemon_client::connect_or_start(settings)
        .map_err(|e| internal_error_with_message("connecting to daemon for git import", e))?;

    let reply = client
        .git_detect_head_change(proto::jj_interface::GitDetectHeadChangeReq {
            working_copy_path: wc_path.clone(),
        })
        .map_err(|e| internal_error_with_message("GitDetectHeadChange RPC", e))?
        .into_inner();

    let has_head_change = !reply.new_head_commit_id.is_empty();
    let has_bookmark_changes = !reply.bookmark_changes.is_empty();

    if !has_head_change && !has_bookmark_changes {
        return Ok(None);
    }

    Ok(Some(GitChanges {
        new_head_commit_id: reply.new_head_commit_id,
        bookmark_changes: reply.bookmark_changes,
    }))
}

/// Application phase: apply detected git changes as a jj transaction.
///
/// Failures here are fatal — a partially-committed transaction could
/// leave the repo in an inconsistent state.
async fn apply_git_imports(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    changes: GitChanges,
) -> Result<(), CommandError> {
    use jj_lib::object_id::ObjectId as _;

    let has_head_change = !changes.new_head_commit_id.is_empty();
    let has_bookmark_changes = !changes.bookmark_changes.is_empty();

    // Load workspace without triggering snapshot (we need to import first).
    let mut ws_helper = command_helper.workspace_helper_no_snapshot(ui)?;

    let workspace_name = ws_helper.workspace_name().to_owned();
    {
        let mut tx = ws_helper.start_transaction();
        let store = tx.repo().store().clone();

        // Import external HEAD change (e.g. `git commit` from the mount).
        if has_head_change {
            let new_head_id =
                jj_lib::backend::CommitId::from_bytes(&changes.new_head_commit_id);
            writeln!(
                ui.status(),
                "Importing external git commit: {}",
                new_head_id.hex()
            )?;

            let new_commit = store
                .get_commit(&new_head_id)
                .map_err(|e| internal_error_with_message("reading imported commit", e))?;

            tx.repo_mut()
                .add_head(&new_commit)
                .await
                .map_err(|e| {
                    internal_error_with_message("adding imported commit as head", e)
                })?;

            tx.repo_mut()
                .check_out(workspace_name, &new_commit)
                .await
                .map_err(|e| {
                    internal_error_with_message("checking out imported commit", e)
                })?;
        }

        // Import external bookmark changes (e.g. `git fetch` or `git branch`
        // from the mount).
        if has_bookmark_changes {
            use jj_lib::op_store::RefTarget;
            use jj_lib::ref_name::RefName;

            for change in &changes.bookmark_changes {
                let bookmark_name = RefName::new(&change.name);
                if change.commit_id.is_empty() {
                    // Bookmark was deleted externally.
                    writeln!(
                        ui.status(),
                        "Importing deleted git bookmark: {}",
                        change.name
                    )?;
                    tx.repo_mut().set_local_bookmark_target(
                        bookmark_name,
                        RefTarget::absent(),
                    );
                } else {
                    // Bookmark was added or changed externally.
                    let commit_id =
                        jj_lib::backend::CommitId::from_bytes(&change.commit_id);
                    writeln!(
                        ui.status(),
                        "Importing external git bookmark: {} -> {}",
                        change.name,
                        commit_id.hex()
                    )?;

                    // Read the commit through the store (auto-creates extras).
                    let commit = store.get_commit(&commit_id).map_err(|e| {
                        internal_error_with_message(
                            format!(
                                "reading commit for imported bookmark '{}'",
                                change.name
                            ),
                            e,
                        )
                    })?;

                    // Make the commit visible in jj log.
                    tx.repo_mut().add_head(&commit).await.map_err(|e| {
                        internal_error_with_message(
                            format!(
                                "adding head for imported bookmark '{}'",
                                change.name
                            ),
                            e,
                        )
                    })?;

                    tx.repo_mut().set_local_bookmark_target(
                        bookmark_name,
                        RefTarget::normal(commit_id),
                    );
                }
            }
        }

        let description = if has_head_change && has_bookmark_changes {
            "import git head and bookmarks"
        } else if has_head_change {
            "import git head"
        } else {
            "import git bookmarks"
        };
        tx.finish(ui, description).await?;
    }

    if has_head_change {
        writeln!(
            ui.status(),
            "Reset the working copy parent to the new Git HEAD."
        )?;
    }

    Ok(())
}

/// Dispatch hook: intercepts `kiki git push/fetch/remote` on kiki-backend repos,
/// routing them through the daemon instead of jj's built-in git commands (which
/// fail with `UnexpectedGitBackendError` on non-GitBackend repos).
async fn kiki_git_dispatch_hook(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    old_dispatch: BoxedAsyncCliDispatch<'_>,
) -> Result<(), CommandError> {
    if let Some(("git", git_matches)) = command_helper.matches().subcommand()
        && is_kiki_backend(command_helper)
    {
        match git_matches.subcommand_name() {
            Some("push") | Some("fetch") | Some("remote") => {
                return dispatch_kiki_git(ui, command_helper).await;
            }
            Some(other) => {
                return Err(user_error(format!(
                    "'git {other}' is not supported on kiki-backend repositories\n\
                     Supported: git push, git fetch, git remote"
                )));
            }
            None => {
                // `kiki git` with no subcommand — fall through to jj's help.
            }
        }
    }
    old_dispatch.call(ui, command_helper).await
}

/// Dispatch hook for `kiki workspace` — intercepts `jj workspace`
/// subcommands and routes `create`/`list`/`delete` through the daemon's
/// managed-workspace RPCs instead of jj's built-in workspace management.
/// Mirrors the `kiki_git_dispatch_hook` pattern (§12.2 #7).
async fn kiki_workspace_dispatch_hook(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    old_dispatch: BoxedAsyncCliDispatch<'_>,
) -> Result<(), CommandError> {
    if let Some(("workspace", ws_matches)) = command_helper.matches().subcommand()
        && is_kiki_backend(command_helper)
    {
        match ws_matches.subcommand_name() {
            Some("create" | "add") | Some("list") | Some("delete" | "forget") => {
                return dispatch_kiki_workspace(ui, command_helper).await;
            }
            _ => {
                // Other workspace subcommands (e.g. `workspace root`) fall
                // through to jj's built-in handling.
            }
        }
    }
    old_dispatch.call(ui, command_helper).await
}

/// Re-parse workspace args from the raw command line and dispatch to
/// kiki's managed-workspace implementation.
async fn dispatch_kiki_workspace(
    ui: &mut Ui,
    command_helper: &CommandHelper,
) -> Result<(), CommandError> {
    use clap::Parser as _;

    let string_args = command_helper.string_args();
    let ws_pos = string_args
        .iter()
        .position(|s| s == "workspace")
        .ok_or_else(|| cli_error("internal: 'workspace' not found in command args"))?;
    let ws_args = &string_args[ws_pos..];

    // Map jj workspace subcommand names to kiki's naming:
    // `jj workspace add` → `kiki workspace create`
    // `jj workspace forget` → `kiki workspace delete`
    let mapped: Vec<String> = ws_args
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if i == 1 {
                match s.as_str() {
                    "add" => "create".to_owned(),
                    "forget" => "delete".to_owned(),
                    other => other.to_owned(),
                }
            } else {
                s.clone()
            }
        })
        .collect();

    let parsed = KikiWorkspaceCli::try_parse_from(&mapped).map_err(|e| {
        user_error(format!(
            "on kiki-backend repos, workspace management goes through the daemon:\n\n  \
             workspace create [repo/]<name>\n  \
             workspace list [<repo>]\n  \
             workspace delete [repo/]<name>\n\n\
             {e}"
        ))
    })?;

    run_workspace_command(ui, command_helper, WorkspaceCommandArgs { command: parsed.command })
        .await
}

/// Top-level clap parser for re-parsing workspace args from raw strings.
#[derive(clap::Parser, Debug)]
#[command(name = "workspace")]
struct KikiWorkspaceCli {
    #[command(subcommand)]
    command: WorkspaceCommands,
}

/// Re-parse git args and dispatch to kiki's daemon-backed implementation.
async fn dispatch_kiki_git(
    ui: &mut Ui,
    command_helper: &CommandHelper,
) -> Result<(), CommandError> {
    use clap::Parser as _;

    let string_args = command_helper.string_args();
    let git_pos = string_args
        .iter()
        .position(|s| s == "git")
        .ok_or_else(|| cli_error("internal: 'git' not found in command args"))?;
    let git_args = &string_args[git_pos..];

    let parsed = KikiGitCli::try_parse_from(git_args).map_err(|e| {
        user_error(format!(
            "on kiki-backend repos, only a subset of 'jj git' flags are supported:\n\n  \
             git push [--remote <REMOTE>] [--bookmark <NAME>]... [--all]\n  \
             git fetch [--remote <REMOTE>]\n  \
             git remote add <NAME> <URL>\n  \
             git remote list\n\n\
             {e}"
        ))
    })?;

    let client = daemon_client::connect_or_start(command_helper.settings())
        .map_err(|e| user_error_with_message("failed to connect to kiki daemon", e))?;

    run_git_command(
        ui,
        command_helper,
        &client,
        GitArgs {
            command: parsed.command,
        },
    )
    .await
}

async fn run_kk_command(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    command: KikiSubcommand,
) -> Result<(), CommandError> {
    match command {
        KikiSubcommand::Clone(args) => {
            return run_clone_command(ui, command_helper, args).await;
        }
        KikiSubcommand::Remote(args) => {
            return run_remote_command(ui, command_helper, args).await;
        }
        KikiSubcommand::Kk(_) => {} // fall through to existing dispatch below
    }

    let KikiSubcommand::Kk(KikiArgs { command }) = command else {
        unreachable!()
    };

    // setup runs standalone — no daemon needed.
    if let KikiCommands::Setup = command {
        return run_setup(ui);
    }

    // doctor runs standalone — reads storage state directly.
    if let KikiCommands::Doctor(ref args) = command {
        return doctor::run_doctor(ui, args.fix, args.repo.as_deref());
    }

    // daemon subcommands run standalone — they manage the daemon lifecycle.
    if let KikiCommands::Daemon(ref args) = command {
        daemon_cmd::dispatch_daemon(args)?;
        return Ok(());
    }

    let client = daemon_client::connect_or_start(command_helper.settings())
        .map_err(|e| user_error_with_message("failed to connect to kiki daemon", e))?;
    match command {
        KikiCommands::Status => {
            let resp = client
                .daemon_status(proto::jj_interface::DaemonStatusReq {})
                .map_err(|e| internal_error_with_message("daemon DaemonStatus RPC failed", e))?;
            ui.request_pager();
            let mut formatter = ui.stdout_formatter();
            for session in resp.into_inner().data {
                // M9 made `remote` actually mean "URL of the configured
                // RemoteStore" (empty = no remote). Drop the ` - <remote>`
                // tail when there's nothing to show, instead of emitting
                // a dangling `path - ` with trailing whitespace.
                if session.remote.is_empty() {
                    writeln!(formatter, "{}", session.path)?;
                } else {
                    writeln!(formatter, "{} - {}", session.path, session.remote)?;
                }
            }
            Ok(())
        }
        KikiCommands::Init(args) => {
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

            // M10.5: replace the default `OpHeadsStore` initializer
            // with one that constructs a `KikiOpHeadsStore` driving
            // the daemon's catalog. The store dir argument is
            // `<wc>/.jj/repo/op_heads/`; the daemon already routes
            // by `working_copy_path`, which we have here.
            let wc_path_for_op_heads = wc_path_str.to_string();
            let kiki_op_heads_initializer = move |settings: &jj_lib::settings::UserSettings,
                                                 _store_path: &std::path::Path|
                  -> Result<
                Box<dyn jj_lib::op_heads_store::OpHeadsStore>,
                jj_lib::backend::BackendInitError,
            > {
                let client = connect_daemon(settings).map_err(|e| {
                    jj_lib::backend::BackendInitError(e.to_string().into())
                })?;
                Ok(Box::new(KikiOpHeadsStore::new(
                    client,
                    wc_path_for_op_heads.clone(),
                )))
            };

            // M10.6: replace the default `OpStore` initializer with
            // one that constructs a `KikiOpStore` routing through the
            // daemon (write-through to remote, read-through on miss).
            let wc_path_for_op_store = wc_path_str.to_string();
            let kiki_op_store_initializer = move |settings: &jj_lib::settings::UserSettings,
                                                 store_path: &std::path::Path,
                                                 root_data: jj_lib::op_store::RootOperationData|
                  -> Result<
                Box<dyn jj_lib::op_store::OpStore>,
                jj_lib::backend::BackendInitError,
            > {
                let client = connect_daemon(settings).map_err(|e| {
                    jj_lib::backend::BackendInitError(e.to_string().into())
                })?;
                let store = op_store::KikiOpStore::init(
                    store_path,
                    root_data,
                    client,
                    wc_path_for_op_store.clone(),
                )
                .map_err(|e| jj_lib::backend::BackendInitError(e.into()))?;
                Ok(Box::new(store))
            };

            Workspace::init_with_factories(
                command_helper.settings(),
                &wc_path,
                &|settings, store_path| {
                    let backend = KikiBackend::new(settings, store_path)?;
                    Ok(Box::new(backend))
                },
                Signer::from_settings(command_helper.settings())
                    .map_err(WorkspaceInitError::SignInit)?,
                &kiki_op_store_initializer,
                &kiki_op_heads_initializer,
                ReadonlyRepo::default_index_store_initializer(),
                ReadonlyRepo::default_submodule_store_initializer(),
                // M2: route Workspace::init_with_factories through
                // KikiWorkingCopyFactory so the freshly-initialised workspace
                // talks to the daemon (SetCheckoutState) rather than spinning
                // up a local on-disk working copy. The daemon mount is
                // guaranteed to exist here because the Initialize RPC above
                // ran first.
                &KikiWorkingCopyFactory {},
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
        KikiCommands::Daemon(_) | KikiCommands::Setup | KikiCommands::Doctor(_) => {
            // Handled above before daemon connection.
            unreachable!()
        }
    }
}

async fn run_git_command(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    client: &BlockingJujutsuInterfaceClient,
    git_args: GitArgs,
) -> Result<(), CommandError> {
    match git_args.command {
        GitCommands::Remote(remote_args) => match remote_args.command {
            GitRemoteCommands::Add(add_args) => {
                let wc_path = workspace_path(command_helper)?;
                client
                    .git_remote_add(proto::jj_interface::GitRemoteAddReq {
                        working_copy_path: wc_path,
                        name: add_args.name,
                        url: add_args.url,
                    })
                    .map_err(|e| {
                        internal_error_with_message("daemon GitRemoteAdd RPC failed", e)
                    })?;
                Ok(())
            }
            GitRemoteCommands::List => {
                let wc_path = workspace_path(command_helper)?;
                let resp = client
                    .git_remote_list(proto::jj_interface::GitRemoteListReq {
                        working_copy_path: wc_path,
                    })
                    .map_err(|e| {
                        internal_error_with_message("daemon GitRemoteList RPC failed", e)
                    })?;
                let mut formatter = ui.stdout_formatter();
                for remote in resp.into_inner().remotes {
                    writeln!(formatter, "{}\t{}", remote.name, remote.url)?;
                }
                Ok(())
            }
        },
        GitCommands::Push(push_args) => {
            let remote = push_args
                .remote
                .unwrap_or_else(|| "origin".to_string());
            let wc_path = workspace_path(command_helper)?;
            let workspace_helper = command_helper.workspace_helper(ui)?;
            let repo = workspace_helper.repo();
            let view = repo.view();

            use jj_lib::object_id::ObjectId as _;
            use jj_lib::ref_name::RefName;

            let bookmarks: Vec<proto::jj_interface::GitPushBookmark> = if push_args.all {
                view.local_bookmarks()
                    .filter_map(|(name, target)| {
                        target.as_normal().map(|id| proto::jj_interface::GitPushBookmark {
                            name: name.as_str().to_owned(),
                            commit_id: id.to_bytes(),
                        })
                    })
                    .collect()
            } else if push_args.bookmark.is_empty() {
                return Err(cli_error(
                    "no bookmarks specified; use --bookmark <name> or --all",
                ));
            } else {
                let mut result = Vec::new();
                for name in &push_args.bookmark {
                    let ref_name = RefName::new(name.as_str());
                    let target = view.get_local_bookmark(ref_name);
                    if target.is_absent() {
                        return Err(cli_error(format!("bookmark '{name}' not found")));
                    }
                    let commit_id = target.as_normal().ok_or_else(|| {
                        cli_error(format!("bookmark '{name}' is conflicted"))
                    })?;
                    result.push(proto::jj_interface::GitPushBookmark {
                        name: name.clone(),
                        commit_id: commit_id.to_bytes(),
                    });
                }
                result
            };

            if bookmarks.is_empty() {
                writeln!(ui.status(), "Nothing to push.")?;
                return Ok(());
            }

            let count = bookmarks.len();
            let names: Vec<&str> = bookmarks.iter().map(|b| b.name.as_str()).collect();
            writeln!(
                ui.status(),
                "Pushing {} bookmark(s) to {}: {}",
                count,
                remote,
                names.join(", ")
            )?;

            client
                .git_push(proto::jj_interface::GitPushReq {
                    working_copy_path: wc_path,
                    remote,
                    bookmarks,
                })
                .map_err(|e| internal_error_with_message("daemon GitPush RPC failed", e))?;

            writeln!(ui.status(), "Done.")?;
            Ok(())
        }
        GitCommands::Fetch(fetch_args) => {
            let remote = fetch_args
                .remote
                .unwrap_or_else(|| "origin".to_string());
            let wc_path = workspace_path(command_helper)?;
            writeln!(ui.status(), "Fetching from {}...", remote)?;

            let resp = client
                .git_fetch(proto::jj_interface::GitFetchReq {
                    working_copy_path: wc_path,
                    remote: remote.clone(),
                })
                .map_err(|e| internal_error_with_message("daemon GitFetch RPC failed", e))?;

            let fetched = resp.into_inner().bookmarks;
            if fetched.is_empty() {
                writeln!(ui.status(), "Nothing new from remote.")?;
                return Ok(());
            }

            // Print fetched bookmarks.
            {
                let mut formatter = ui.stdout_formatter();
                for b in &fetched {
                    let hex: String =
                        b.commit_id.iter().map(|byte| format!("{byte:02x}")).collect();
                    let short = &hex[..12.min(hex.len())];
                    writeln!(formatter, "  {}/{}: {short}", remote, b.name)?;
                }
            }

            // Update jj's View: index fetched commits, set remote-tracking
            // bookmarks so `jj log` shows e.g. `main@origin`, and
            // fast-forward local bookmarks where possible.
            use jj_lib::backend::CommitId;
            use jj_lib::op_store::{RemoteRef, RemoteRefState, RefTarget};
            use jj_lib::ref_name::{RefName, RemoteName};

            let mut workspace_command = command_helper.workspace_helper(ui)?;
            let mut tx = workspace_command.start_transaction();

            // Index the fetched commits so jj's index knows about them.
            // The commits live in the daemon's git ODB (accessible via
            // KikiBackend); add_head walks ancestors and indexes them.
            let store = tx.repo().store().clone();
            for b in &fetched {
                let commit_id = CommitId::new(b.commit_id.clone());
                let commit = store.get_commit(&commit_id).map_err(|e| {
                    internal_error_with_message(
                        format!("fetched commit for '{}' not readable", b.name),
                        e,
                    )
                })?;
                tx.repo_mut().add_head(&commit).await.map_err(|e| {
                    internal_error_with_message(
                        format!("failed to index fetched commit for '{}'", b.name),
                        e,
                    )
                })?;
            }

            // Set remote-tracking bookmarks.
            let remote_name = RemoteName::new(remote.as_str());
            for b in &fetched {
                let bookmark_name = RefName::new(b.name.as_str());
                let commit_id = CommitId::new(b.commit_id.clone());
                let remote_ref = RemoteRef {
                    target: RefTarget::normal(commit_id),
                    state: RemoteRefState::Tracked,
                };
                tx.repo_mut().set_remote_bookmark(
                    bookmark_name.to_remote_symbol(remote_name),
                    remote_ref,
                );
            }

            // For tracked remote bookmarks, also update local bookmarks
            // when the local target is absent (new bookmark) or is an
            // ancestor of the fetched commit (fast-forward). Skip
            // conflicting updates — the user can resolve manually.
            for b in &fetched {
                let bookmark_name = RefName::new(b.name.as_str());
                let commit_id = CommitId::new(b.commit_id.clone());
                let local_target = tx.repo().view().get_local_bookmark(bookmark_name);
                if local_target.is_absent() {
                    // New bookmark — create local tracking it.
                    tx.repo_mut().set_local_bookmark_target(
                        bookmark_name,
                        RefTarget::normal(commit_id),
                    );
                } else if let Some(local_id) = local_target.as_normal() {
                    // Check if this is a fast-forward (local is ancestor of fetched).
                    let index = tx.repo().index();
                    if index.is_ancestor(local_id, &commit_id).unwrap_or(false) {
                        tx.repo_mut().set_local_bookmark_target(
                            bookmark_name,
                            RefTarget::normal(commit_id),
                        );
                    }
                    // else: diverged — leave local as-is, user resolves.
                }
            }

            tx.finish(
                ui,
                format!("fetch from git remote(s) {}", remote),
            )
            .await?;

            Ok(())
        }
    }
}

/// Resolve the canonical working-copy path for the current workspace.
/// Walks up from cwd looking for `.jj/`, matching jj's workspace discovery.
fn workspace_path(command_helper: &CommandHelper) -> Result<String, CommandError> {
    // Prefer the workspace loader's root (handles -R correctly).
    if let Ok(loader) = command_helper.workspace_loader() {
        let root = loader.workspace_root();
        return root
            .to_str()
            .map(|s| s.to_owned())
            .ok_or_else(|| cli_error("workspace path is not valid UTF-8"));
    }

    // Fallback: walk up from cwd looking for .jj/.
    let cwd = command_helper.cwd();
    let mut dir = cwd.to_path_buf();
    loop {
        if dir.join(".jj").is_dir() {
            break;
        }
        if !dir.pop() {
            return Err(cli_error(
                "not in a jj workspace (no .jj/ directory found)",
            ));
        }
    }
    let wc_path = dir.canonicalize().map_err(|e| {
        user_error_with_message("failed to resolve workspace path", e)
    })?;
    wc_path
        .to_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| cli_error("workspace path is not valid UTF-8"))
}

/// Infer the repo name from the current working directory by stripping
/// `mount_root` and taking the first path component. Returns `None` if
/// cwd is not under `mount_root`.
///
/// Example: cwd = `/mnt/kiki/monorepo/default/src`, mount_root = `/mnt/kiki`
/// → `Some("monorepo")`.
fn infer_repo_from_cwd(command_helper: &CommandHelper) -> Option<String> {
    let mount_root = daemon_cmd::configured_mount_root();
    let cwd = command_helper.cwd();
    // Canonicalize cwd to resolve symlinks (e.g. ~/src/bar → /mnt/kiki/bar/default).
    let canonical = cwd.canonicalize().ok()?;
    let rel = canonical.strip_prefix(&mount_root).ok()?;
    rel.components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .map(|s| s.to_owned())
}

/// Parse a workspace specifier into `(repo, workspace)`.
///
/// Accepts two forms:
/// - `fix-auth` — bare name, repo inferred from cwd
/// - `myrepo/fix-auth` — explicit repo/workspace
///
/// Returns a user-facing error if a bare name is given but the repo
/// can't be inferred.
fn parse_workspace_spec(
    name: &str,
    command_helper: &CommandHelper,
) -> Result<(String, String), CommandError> {
    if let Some((repo, ws)) = name.split_once('/') {
        if repo.is_empty() || ws.is_empty() {
            return Err(user_error(format!(
                "invalid workspace specifier {name:?}: \
                 use 'workspace-name' or 'repo/workspace-name'"
            )));
        }
        Ok((repo.to_owned(), ws.to_owned()))
    } else {
        let repo = infer_repo_from_cwd(command_helper).ok_or_else(|| {
            user_error(format!(
                "cannot infer repo from current directory; \
                 use '{name}' from inside a managed workspace, \
                 or specify 'repo/{name}' explicitly"
            ))
        })?;
        Ok((repo, name.to_owned()))
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

// ── M12: managed-workspace CLI commands ──────────────────────────────

/// `kiki kk setup`
///
/// Check prerequisites and create the workspace mount root directory.
/// Runs without a daemon connection — safe to call before anything else.
fn run_setup(ui: &mut Ui) -> Result<(), CommandError> {
    let mount_root = daemon_cmd::configured_mount_root();

    writeln!(ui.status(), "kiki setup")?;
    writeln!(ui.status())?;

    // ── 1. Check fusermount3 (Linux only) ──────────────────────────
    #[cfg(target_os = "linux")]
    {
        write!(ui.status(), "  fusermount3 ... ")?;
        match std::process::Command::new("which")
            .arg("fusermount3")
            .output()
        {
            Ok(output) if output.status.success() => {
                let path = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .to_string();
                writeln!(ui.status(), "ok ({})", path)?;
            }
            _ => {
                writeln!(ui.status(), "MISSING")?;
                writeln!(
                    ui.warning_default(),
                    "fusermount3 not found. Install the fuse3 package:\n  \
                     sudo apt install fuse3   # Debian/Ubuntu\n  \
                     sudo dnf install fuse3   # Fedora\n  \
                     sudo pacman -S fuse3     # Arch",
                )?;
            }
        }
    }

    // ── 2. Create mount root ───────────────────────────────────────
    write!(ui.status(), "  mount root ({}) ... ", mount_root.display())?;
    let mount_root_ok = if mount_root.exists() {
        // Check we can write to it.
        let probe = mount_root.join(".kiki-setup-probe");
        match std::fs::File::create(&probe) {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                writeln!(ui.status(), "ok")?;
                true
            }
            Err(e) => {
                writeln!(ui.status(), "NOT WRITABLE")?;
                writeln!(
                    ui.warning_default(),
                    "Mount root exists but is not writable: {e}\n  \
                     Fix with: sudo chown $USER {}",
                    mount_root.display(),
                )?;
                false
            }
        }
    } else {
        // Try to create it directly. AlreadyExists is fine — can happen
        // when a stale FUSE mountpoint makes `exists()` return false but
        // the directory inode is still present.
        match std::fs::create_dir_all(&mount_root) {
            Ok(()) => {
                writeln!(ui.status(), "created")?;
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                writeln!(ui.status(), "ok (already exists)")?;
                true
            }
            Err(e) => {
                writeln!(ui.status(), "FAILED")?;
                writeln!(
                    ui.warning_default(),
                    "Cannot create {}: {e}\n  \
                     Create it manually:\n    \
                     sudo mkdir -p {} && sudo chown $USER {}",
                    mount_root.display(),
                    mount_root.display(),
                    mount_root.display(),
                )?;
                false
            }
        }
    };

    // ── 3. Summary ─────────────────────────────────────────────────
    writeln!(ui.status())?;
    if mount_root_ok {
        writeln!(
            ui.status(),
            "Ready. Clone a repo to get started:\n\n  \
             kiki clone <url>\n  \
             cd {}/{{repo}}/default",
            mount_root.display(),
        )?;
    } else {
        writeln!(
            ui.status(),
            "Fix the issues above, then run `kiki kk setup` again."
        )?;
    }

    Ok(())
}

/// `kiki clone <url> [--name <name>]`
///
/// Clones a remote repo into the managed namespace. Creates the repo
/// entry + default workspace via the daemon's Clone or GitClone RPC, then
/// initializes the jj workspace at the returned path.
async fn run_clone_command(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    args: CloneCommandArgs,
) -> Result<(), CommandError> {
    let client = daemon_client::connect_or_start(command_helper.settings())
        .map_err(|e| user_error_with_message("failed to connect to kiki daemon", e))?;

    let (workspace_path, initial_tree_id) = match classify_clone_url(&args.url) {
        CloneSource::KikiRemote(url) => {
            if args.remote.is_some() {
                return Err(user_error(
                    "--remote is only valid when cloning from a git URL; \
                     the clone URL is already a kiki remote",
                ));
            }
            writeln!(ui.status(), "Cloning {}...", url)?;
            let reply = client
                .clone_repo(proto::jj_interface::CloneReq {
                    url,
                    name: args.name.clone().unwrap_or_default(),
                })
                .map_err(|e| rpc_error("Clone failed", e))?
                .into_inner();
            (reply.workspace_path, reply.initial_tree_id)
        }
        CloneSource::Git(git_url) => {
            writeln!(ui.status(), "Cloning from git: {}...", git_url)?;
            let reply = client
                .git_clone(proto::jj_interface::GitCloneReq {
                    git_url: git_url.clone(),
                    name: args.name.clone().unwrap_or_default(),
                    kiki_remote: args.remote.clone().unwrap_or_default(),
                })
                .map_err(|e| rpc_error("Clone failed", e))?
                .into_inner();
            if !reply.default_branch.is_empty() {
                writeln!(
                    ui.status(),
                    "Fetched {} bookmark(s), default branch: {}",
                    reply.bookmarks.len(),
                    reply.default_branch,
                )?;
            }
            (reply.workspace_path, reply.initial_tree_id)
        }
    };

    let wc_path = std::path::PathBuf::from(&workspace_path);

    // Derive repo name from workspace path (~/kiki/<repo>/<workspace>)
    // for rollback in case init fails.
    let repo_name = wc_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_owned());

    // Initialize the jj workspace at the managed path. This is the same
    // flow as `kiki kk init` (§12.11): create the .jj/ metadata through
    // the VFS, using daemon-backed store factories.
    if let Err(e) = init_jj_workspace(command_helper, &wc_path, &workspace_path).await {
        // Rollback: delete the partially-created repo so we don't leave
        // a zombie (registered, files visible, but no jj metadata).
        if let Some(ref name) = repo_name {
            let _ = client.repo_delete(proto::jj_interface::RepoDeleteReq {
                name: name.clone(),
            });
        }
        return Err(e);
    }

    // If the clone resolved an initial tree (e.g. default branch tip for
    // git clone), issue a CheckOut to materialize it and persist the
    // root_tree_id to workspace.toml. Without this, the workspace starts
    // with an empty tree until the user manually checks out a commit, and
    // a daemon restart before that would lose any content reference.
    if !initial_tree_id.is_empty() {
        if let Err(e) = client
            .check_out(proto::jj_interface::CheckOutReq {
                working_copy_path: workspace_path.clone(),
                new_tree_id: initial_tree_id,
            })
        {
            // Rollback on CheckOut failure too.
            if let Some(ref name) = repo_name {
                let _ = client.repo_delete(proto::jj_interface::RepoDeleteReq {
                    name: name.clone(),
                });
            }
            return Err(user_error_with_message("initial CheckOut failed", e));
        }
    }

    // Create symlink if --link was requested.
    if let Some(link_path) = &args.link {
        create_link(link_path, &wc_path)?;
    }

    let display_path = args
        .link
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| workspace_path.clone());

    writeln!(
        ui.status(),
        "Cloned into {} (workspace: default)",
        workspace_path,
    )?;
    if args.link.is_some() {
        writeln!(
            ui.hint_default(),
            "cd {} to start working (symlink -> {})",
            display_path,
            workspace_path,
        )?;
    } else {
        writeln!(
            ui.hint_default(),
            "cd {} to start working",
            display_path,
        )?;
    }

    Ok(())
}

/// Initialize a jj workspace at a managed namespace path using
/// daemon-backed store factories.
async fn init_jj_workspace(
    command_helper: &CommandHelper,
    wc_path: &std::path::Path,
    wc_path_str: &str,
) -> Result<(), CommandError> {
    let wc_path_for_op_heads = wc_path_str.to_string();
    let kiki_op_heads_initializer = move |settings: &jj_lib::settings::UserSettings,
                                         _store_path: &std::path::Path|
          -> Result<
        Box<dyn jj_lib::op_heads_store::OpHeadsStore>,
        jj_lib::backend::BackendInitError,
    > {
        let client = connect_daemon(settings).map_err(|e| {
            jj_lib::backend::BackendInitError(e.to_string().into())
        })?;
        Ok(Box::new(KikiOpHeadsStore::new(
            client,
            wc_path_for_op_heads.clone(),
        )))
    };

    let wc_path_for_op_store = wc_path_str.to_string();
    let kiki_op_store_initializer = move |settings: &jj_lib::settings::UserSettings,
                                         store_path: &std::path::Path,
                                         root_data: jj_lib::op_store::RootOperationData|
          -> Result<
        Box<dyn jj_lib::op_store::OpStore>,
        jj_lib::backend::BackendInitError,
    > {
        let client = connect_daemon(settings).map_err(|e| {
            jj_lib::backend::BackendInitError(e.to_string().into())
        })?;
        let store = op_store::KikiOpStore::init(
            store_path,
            root_data,
            client,
            wc_path_for_op_store.clone(),
        )
        .map_err(|e| jj_lib::backend::BackendInitError(e.into()))?;
        Ok(Box::new(store))
    };

    Workspace::init_with_factories(
        command_helper.settings(),
        wc_path,
        &|settings, store_path| {
            let backend = KikiBackend::new(settings, store_path)?;
            Ok(Box::new(backend))
        },
        Signer::from_settings(command_helper.settings())
            .map_err(WorkspaceInitError::SignInit)?,
        &kiki_op_store_initializer,
        &kiki_op_heads_initializer,
        ReadonlyRepo::default_index_store_initializer(),
        ReadonlyRepo::default_submodule_store_initializer(),
        &KikiWorkingCopyFactory {},
        WorkspaceName::DEFAULT.to_owned(),
    )
    .await?;
    Ok(())
}

/// Create a symlink from `link_path` to `target`.
fn create_link(
    link_path: &std::path::Path,
    target: &std::path::Path,
) -> Result<(), CommandError> {
    if link_path.exists() {
        return Err(user_error(format!(
            "link target already exists: {}",
            link_path.display()
        )));
    }
    if let Some(parent) = link_path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            user_error_with_message(
                format!("creating parent directory {}", parent.display()),
                e,
            )
        })?;
    }
    std::os::unix::fs::symlink(target, link_path).map_err(|e| {
        user_error_with_message(
            format!(
                "creating symlink {} -> {}",
                link_path.display(),
                target.display()
            ),
            e,
        )
    })?;
    Ok(())
}

// ── M13: kiki remote CLI commands ───────────────────────────────────

/// `kiki remote add/remove/show`
///
/// Manages the kiki-native remote for the current repo. Repo is inferred
/// from cwd (you must be inside a managed workspace).
async fn run_remote_command(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    args: RemoteCommandArgs,
) -> Result<(), CommandError> {
    let client = daemon_client::connect_or_start(command_helper.settings())
        .map_err(|e| user_error_with_message("failed to connect to kiki daemon", e))?;

    let repo = infer_repo_from_cwd(command_helper).ok_or_else(|| {
        user_error(
            "cannot infer repo from current directory; \
             run this command from inside a managed workspace under /mnt/kiki/<repo>/",
        )
    })?;

    match args.command {
        RemoteCommands::Add(add_args) => {
            writeln!(
                ui.status(),
                "Attaching kiki remote {} to {}...",
                add_args.url,
                repo,
            )?;
            client
                .remote_add(proto::jj_interface::RemoteAddReq {
                    repo: repo.clone(),
                    url: add_args.url.clone(),
                })
                .map_err(|e| rpc_error("Failed to add remote", e))?;
            writeln!(
                ui.status(),
                "Done. Kiki remote attached: {}",
                add_args.url,
            )?;
            Ok(())
        }
        RemoteCommands::Remove => {
            client
                .remote_remove(proto::jj_interface::RemoteRemoveReq {
                    repo: repo.clone(),
                })
                .map_err(|e| rpc_error("Failed to remove remote", e))?;
            writeln!(ui.status(), "Kiki remote detached from {}.", repo)?;
            Ok(())
        }
        RemoteCommands::Show => {
            let reply = client
                .remote_show(proto::jj_interface::RemoteShowReq {
                    repo: repo.clone(),
                })
                .map_err(|e| rpc_error("Failed to show remote", e))?
                .into_inner();
            if reply.url.is_empty() {
                writeln!(ui.stdout_formatter(), "(none)")?;
            } else {
                writeln!(ui.stdout_formatter(), "{}", reply.url)?;
            }
            Ok(())
        }
    }
}

/// `kiki workspace create/list/delete`
async fn run_workspace_command(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    args: WorkspaceCommandArgs,
) -> Result<(), CommandError> {
    let client = daemon_client::connect_or_start(command_helper.settings())
        .map_err(|e| user_error_with_message("failed to connect to kiki daemon", e))?;

    match args.command {
        WorkspaceCommands::Create(create_args) => {
            run_workspace_create(ui, command_helper, &client, create_args).await
        }
        WorkspaceCommands::List(list_args) => {
            run_workspace_list(ui, command_helper, &client, list_args)
        }
        WorkspaceCommands::Delete(delete_args) => {
            run_workspace_delete(ui, command_helper, &client, delete_args)
        }
    }
}

/// `kiki workspace create [repo/]<workspace>`
///
/// 1. WorkspaceCreate RPC (daemon allocates slot, pending state)
/// 2. jj workspace init at the returned path
/// 3. WorkspaceFinalize RPC (transition to active)
async fn run_workspace_create(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    client: &BlockingJujutsuInterfaceClient,
    args: WorkspaceCreateArgs,
) -> Result<(), CommandError> {
    let (repo, workspace) = parse_workspace_spec(&args.name, command_helper)?;

    // Step 1: WorkspaceCreate RPC (pending state).
    let reply = client
        .workspace_create(proto::jj_interface::WorkspaceCreateReq {
            repo: repo.clone(),
            workspace: workspace.clone(),
        })
        .map_err(|e| rpc_error("Failed to create workspace", e))?
        .into_inner();

    let wc_path = std::path::PathBuf::from(&reply.workspace_path);
    let wc_path_str = &reply.workspace_path;

    // Step 2: jj workspace init at the managed path.
    // Same factory setup as clone, but using a workspace-specific name.
    let wc_path_for_op_heads = wc_path_str.to_string();
    let kiki_op_heads_initializer = move |settings: &jj_lib::settings::UserSettings,
                                         _store_path: &std::path::Path|
          -> Result<
        Box<dyn jj_lib::op_heads_store::OpHeadsStore>,
        jj_lib::backend::BackendInitError,
    > {
        let client = connect_daemon(settings).map_err(|e| {
            jj_lib::backend::BackendInitError(e.to_string().into())
        })?;
        Ok(Box::new(KikiOpHeadsStore::new(
            client,
            wc_path_for_op_heads.clone(),
        )))
    };

    let wc_path_for_op_store = wc_path_str.to_string();
    let kiki_op_store_initializer = move |settings: &jj_lib::settings::UserSettings,
                                         store_path: &std::path::Path,
                                         root_data: jj_lib::op_store::RootOperationData|
          -> Result<
        Box<dyn jj_lib::op_store::OpStore>,
        jj_lib::backend::BackendInitError,
    > {
        let client = connect_daemon(settings).map_err(|e| {
            jj_lib::backend::BackendInitError(e.to_string().into())
        })?;
        let store = op_store::KikiOpStore::init(
            store_path,
            root_data,
            client,
            wc_path_for_op_store.clone(),
        )
        .map_err(|e| jj_lib::backend::BackendInitError(e.into()))?;
        Ok(Box::new(store))
    };

    let ws_name = WorkspaceName::new(&workspace);

    let init_result = Workspace::init_with_factories(
        command_helper.settings(),
        &wc_path,
        &|settings, store_path| {
            let backend = KikiBackend::new(settings, store_path)?;
            Ok(Box::new(backend))
        },
        Signer::from_settings(command_helper.settings())
            .map_err(WorkspaceInitError::SignInit)?,
        &kiki_op_store_initializer,
        &kiki_op_heads_initializer,
        ReadonlyRepo::default_index_store_initializer(),
        ReadonlyRepo::default_submodule_store_initializer(),
        &KikiWorkingCopyFactory {},
        ws_name.to_owned(),
    )
    .await;

    // If jj init fails, the workspace stays in pending state. The daemon
    // will clean it up on restart or the user can `kiki workspace delete`.
    if let Err(e) = init_result {
        // Best-effort cleanup: delete the pending workspace.
        let _ = client.workspace_delete(proto::jj_interface::WorkspaceDeleteReq {
            repo: repo.clone(),
            workspace: workspace.clone(),
        });
        return Err(e.into());
    }

    // Step 3: Finalize — transition from pending to active.
    client
        .workspace_finalize(proto::jj_interface::WorkspaceFinalizeReq {
            repo: repo.clone(),
            workspace: workspace.clone(),
        })
        .map_err(|e| rpc_error("Failed to finalize workspace", e))?;

    writeln!(
        ui.status(),
        "Created workspace {}/{} at {}",
        repo,
        workspace,
        reply.workspace_path,
    )?;
    writeln!(
        ui.hint_default(),
        "cd {} to start working",
        reply.workspace_path,
    )?;

    Ok(())
}

/// `kiki workspace list [<repo>]`
fn run_workspace_list(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    client: &BlockingJujutsuInterfaceClient,
    args: WorkspaceListArgs,
) -> Result<(), CommandError> {
    // Infer repo from cwd if not given explicitly.
    let repo = args.repo.or_else(|| infer_repo_from_cwd(command_helper));
    if let Some(repo) = repo {
        // List workspaces in a specific repo.
        let reply = client
            .workspace_list(proto::jj_interface::WorkspaceListReq {
                repo: repo.clone(),
            })
            .map_err(|e| rpc_error("Failed to list workspaces", e))?
            .into_inner();

        if reply.workspaces.is_empty() {
            writeln!(ui.status(), "No workspaces in repo {repo:?}")?;
        } else {
            let mut formatter = ui.stdout_formatter();
            for ws in &reply.workspaces {
                writeln!(formatter, "{}\t{}", ws.name, ws.path)?;
            }
        }
    } else {
        // List all repos and their workspaces.
        let reply = client
            .repo_list(proto::jj_interface::RepoListReq {})
            .map_err(|e| rpc_error("Failed to list repos", e))?
            .into_inner();

        if reply.repos.is_empty() {
            writeln!(ui.status(), "No repos registered. Use `kiki clone` to add one.")?;
        } else {
            let mut formatter = ui.stdout_formatter();
            for repo in &reply.repos {
                writeln!(formatter, "{} ({})", repo.name, repo.url)?;
                for ws in &repo.workspaces {
                    writeln!(formatter, "  {ws}")?;
                }
            }
        }
    }
    Ok(())
}

/// `kiki workspace delete [repo/]<workspace>`
fn run_workspace_delete(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    client: &BlockingJujutsuInterfaceClient,
    args: WorkspaceDeleteArgs,
) -> Result<(), CommandError> {
    let (repo, workspace) = parse_workspace_spec(&args.name, command_helper)?;

    client
        .workspace_delete(proto::jj_interface::WorkspaceDeleteReq {
            repo: repo.clone(),
            workspace: workspace.clone(),
        })
        .map_err(|e| rpc_error("Failed to delete workspace", e))?;

    writeln!(
        ui.status(),
        "Deleted workspace {}/{}",
        repo,
        workspace,
    )?;
    Ok(())
}

fn main() -> std::process::ExitCode {
    let mut working_copy_factories = WorkingCopyFactories::new();
    working_copy_factories.insert(
        KikiWorkingCopy::name().to_owned(),
        Box::new(KikiWorkingCopyFactory {}),
    );
    // NOTE: logging before this point will not work since it is
    // initialized by CliRunner.
    CliRunner::init()
        .name("kiki")
        .about("Experimental jj remote backend")
        .add_store_factories(create_store_factories())
        .add_working_copy_factories(working_copy_factories)
        .add_subcommand(run_kk_command)
        .add_dispatch_hook(kiki_git_import_hook)
        .add_dispatch_hook(kiki_git_dispatch_hook)
        .add_dispatch_hook(kiki_workspace_dispatch_hook)
        .run()
        .into()
}
