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
mod op_heads_store;
mod op_store;
mod working_copy;

use backend::KikiBackend;
use blocking_client::BlockingJujutsuInterfaceClient;
use op_heads_store::KikiOpHeadsStore;
use working_copy::{KikiWorkingCopy, KikiWorkingCopyFactory};

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
enum KikiCommands {
    Init(InitArgs),
    Status,
    /// Git remote operations (push, fetch, remote management)
    Git(GitArgs),
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
    let grpc_port = settings
        .get::<usize>("grpc_port")
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    BlockingJujutsuInterfaceClient::connect(format!("http://[::1]:{grpc_port}"))
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
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

/// Dispatch hook: intercepts `kiki git push/fetch/remote` on kiki-backend repos,
/// routing them through the daemon instead of jj's built-in git commands (which
/// fail with `UnexpectedGitBackendError` on non-GitBackend repos).
async fn kiki_git_dispatch_hook(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    old_dispatch: BoxedAsyncCliDispatch<'_>,
) -> Result<(), CommandError> {
    if let Some(("git", git_matches)) = command_helper.matches().subcommand() {
        if is_kiki_backend(command_helper) {
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
    }
    old_dispatch.call(ui, command_helper).await
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

    let grpc_port = command_helper
        .settings()
        .get::<usize>("grpc_port")
        .map_err(|e| user_error_with_message("grpc_port not configured in jj config", e))?;
    let client = BlockingJujutsuInterfaceClient::connect(format!("http://[::1]:{grpc_port}"))
        .map_err(|e| {
            user_error_with_message(
                format!("Failed to connect to kiki daemon on port {grpc_port}"),
                e,
            )
        })?;

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
    let KikiSubcommand::Kk(KikiArgs { command }) = command;

    let grpc_port = command_helper
        .settings()
        .get::<usize>("grpc_port")
        .map_err(|e| user_error_with_message("grpc_port not configured in jj config", e))?;
    let client = crate::blocking_client::BlockingJujutsuInterfaceClient::connect(format!(
        "http://[::1]:{grpc_port}"
    ))
    .map_err(|e| {
        user_error_with_message(format!("Failed to connect to kiki daemon on port {grpc_port}"), e)
    })?;
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
        KikiCommands::Git(git_args) => {
            run_git_command(ui, command_helper, &client, git_args).await
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
        .add_dispatch_hook(kiki_git_dispatch_hook)
        .run()
        .into()
}
