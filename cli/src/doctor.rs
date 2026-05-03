//! `kiki kk doctor` — diagnose and repair managed workspace state.
//!
//! Reads the storage directory directly (repos.toml, workspace.toml,
//! git stores) and reports issues. With `--fix`, repairs what it can
//! by reconnecting to the daemon and re-running initialization steps.

use std::path::Path;

use jj_cli::ui::Ui;

use kiki_daemon::repo_meta;

// ── Check result types ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warning,
    Error,
}

#[derive(Debug)]
pub struct Check {
    pub label: String,
    pub severity: Severity,
    pub detail: String,
    /// Can `--fix` repair this?
    pub fixable: bool,
}

impl Check {
    fn ok(label: impl Into<String>) -> Self {
        Check {
            label: label.into(),
            severity: Severity::Ok,
            detail: String::new(),
            fixable: false,
        }
    }

    fn warn(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Check {
            label: label.into(),
            severity: Severity::Warning,
            detail: detail.into(),
            fixable: false,
        }
    }

    fn error(
        label: impl Into<String>,
        detail: impl Into<String>,
        fixable: bool,
    ) -> Self {
        Check {
            label: label.into(),
            severity: Severity::Error,
            detail: detail.into(),
            fixable,
        }
    }
}

// ── Per-workspace diagnostics ──────────────────────────────────────

fn check_workspace(
    storage_dir: &Path,
    repo_name: &str,
    ws_name: &str,
    ws_cfg: &repo_meta::WorkspaceConfig,
    git_repo_path: &Path,
) -> Vec<Check> {
    let mut checks = Vec::new();

    // 1. op_id / workspace_id populated?
    if ws_cfg.op_id.is_empty() || ws_cfg.workspace_id.is_empty() {
        checks.push(Check::error(
            "jj state",
            format!(
                "op_id or workspace_id is empty — jj workspace was never \
                 initialized. This workspace is a zombie: files may be \
                 visible via FUSE but no version control operations work. \
                 Run `kiki kk doctor --fix` to re-initialize, or delete \
                 and re-clone."
            ),
            true,
        ));
    } else {
        checks.push(Check::ok("jj state"));
    }

    // 2. root_tree_id references a valid git object?
    if ws_cfg.root_tree_id.is_empty() {
        checks.push(Check::warn(
            "root tree",
            "root_tree_id is empty (workspace has no checked-out content)",
        ));
    } else {
        let hex: String = ws_cfg.root_tree_id.iter().map(|b| format!("{b:02x}")).collect();
        // Try to verify the object exists in the git store.
        let result = std::process::Command::new("git")
            .args(["cat-file", "-t", &hex])
            .env("GIT_DIR", git_repo_path)
            .output();
        match result {
            Ok(output) if output.status.success() => {
                let obj_type = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if obj_type == "tree" {
                    checks.push(Check::ok("root tree"));
                } else {
                    checks.push(Check::error(
                        "root tree",
                        format!("root_tree_id {hex} is a {obj_type}, not a tree"),
                        false,
                    ));
                }
            }
            Ok(_) => {
                checks.push(Check::warn(
                    "root tree",
                    format!("root_tree_id {hex} not found in git store (may be in remote)"),
                ));
            }
            Err(e) => {
                checks.push(Check::warn(
                    "root tree",
                    format!("could not verify root_tree_id: {e}"),
                ));
            }
        }
    }

    // 3. Git worktree exists?
    let wt_dir = repo_meta::workspace_worktree_gitdir(storage_dir, repo_name, ws_name);
    let head_file = wt_dir.join("HEAD");
    let commondir_file = wt_dir.join("commondir");
    if !wt_dir.exists() {
        checks.push(Check::error(
            "git worktree",
            format!("worktree gitdir missing at {}", wt_dir.display()),
            true,
        ));
    } else if !head_file.exists() || !commondir_file.exists() {
        checks.push(Check::error(
            "git worktree",
            format!(
                "worktree gitdir at {} is incomplete (missing HEAD or commondir)",
                wt_dir.display()
            ),
            true,
        ));
    } else {
        checks.push(Check::ok("git worktree"));
    }

    // 4. Workspace state.
    match ws_cfg.state {
        repo_meta::WorkspaceState::Pending => {
            checks.push(Check::warn(
                "workspace state",
                "state is 'pending' (not yet finalized)",
            ));
        }
        repo_meta::WorkspaceState::Active => {
            checks.push(Check::ok("workspace state"));
        }
    }

    checks
}

// ── Per-repo diagnostics ───────────────────────────────────────────

fn check_repo(
    storage_dir: &Path,
    repo_name: &str,
) -> Vec<(String, Vec<Check>)> {
    let mut sections = Vec::new();

    // Repo-level checks.
    let mut repo_checks = Vec::new();
    let git_store_path = repo_meta::repo_git_store_path(storage_dir, repo_name);
    let git_repo_path = git_store_path.join("git");

    // 1. Git store exists?
    if !git_repo_path.exists() {
        repo_checks.push(Check::error(
            "git store",
            format!("git store missing at {}", git_repo_path.display()),
            false,
        ));
        sections.push((repo_name.to_string(), repo_checks));
        return sections;
    }

    // Verify git store is readable.
    let result = std::process::Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .env("GIT_DIR", &git_repo_path)
        .output();
    match result {
        Ok(output) if output.status.success() => {
            repo_checks.push(Check::ok("git store"));
        }
        _ => {
            repo_checks.push(Check::error(
                "git store",
                format!("git store at {} is not a valid git repository", git_repo_path.display()),
                false,
            ));
            sections.push((repo_name.to_string(), repo_checks));
            return sections;
        }
    }

    // 2. redb exists?
    let redb_path = repo_meta::repo_redb_path(storage_dir, repo_name);
    if !redb_path.exists() {
        repo_checks.push(Check::warn(
            "redb store",
            format!("store.redb missing at {}", redb_path.display()),
        ));
    } else {
        repo_checks.push(Check::ok("redb store"));
    }

    // 3. Local branch refs?
    let result = std::process::Command::new("git")
        .args(["for-each-ref", "--format=%(refname)", "refs/heads/"])
        .env("GIT_DIR", &git_repo_path)
        .output();
    let has_local_branches = match result {
        Ok(output) if output.status.success() => {
            let refs = String::from_utf8_lossy(&output.stdout);
            !refs.trim().is_empty()
        }
        _ => false,
    };

    let has_remote_branches = {
        let result = std::process::Command::new("git")
            .args(["for-each-ref", "--format=%(refname)", "refs/remotes/"])
            .env("GIT_DIR", &git_repo_path)
            .output();
        match result {
            Ok(output) if output.status.success() => {
                let refs = String::from_utf8_lossy(&output.stdout);
                !refs.trim().is_empty()
            }
            _ => false,
        }
    };

    if !has_local_branches && has_remote_branches {
        repo_checks.push(Check::warn(
            "local branches",
            "no local branch refs (refs/heads/*) but remote tracking refs exist. \
             This may indicate incomplete clone initialization.",
        ));
    } else {
        repo_checks.push(Check::ok("local branches"));
    }

    sections.push((repo_name.to_string(), repo_checks));

    // Per-workspace checks.
    match repo_meta::list_workspace_configs(storage_dir, repo_name) {
        Ok(workspaces) => {
            if workspaces.is_empty() {
                sections.push((
                    format!("{repo_name} (workspaces)"),
                    vec![Check::warn("workspaces", "no workspaces found")],
                ));
            }
            for (ws_name, ws_cfg) in &workspaces {
                let ws_checks = check_workspace(
                    storage_dir,
                    repo_name,
                    ws_name,
                    ws_cfg,
                    &git_repo_path,
                );
                sections.push((format!("{repo_name}/{ws_name}"), ws_checks));
            }
        }
        Err(e) => {
            sections.push((
                format!("{repo_name} (workspaces)"),
                vec![Check::error(
                    "workspaces",
                    format!("failed to list workspaces: {e:#}"),
                    false,
                )],
            ));
        }
    }

    sections
}

// ── Stale mount detection ──────────────────────────────────────────

/// Check whether a path is a stale FUSE/NFS mount. A stale mount is one
/// where `stat` fails with "Transport endpoint is not connected" (ENOTCONN,
/// errno 107 on Linux) or "Stale file handle" (ESTALE). These indicate a
/// dead FUSE/NFS daemon left behind a mount that the kernel can't service.
fn is_stale_mount(path: &Path) -> Option<String> {
    match std::fs::metadata(path) {
        Ok(_) => None, // accessible — not stale
        Err(e) => {
            let raw = e.raw_os_error();
            // ENOTCONN (107): "Transport endpoint is not connected"
            // ESTALE (116): "Stale file handle"
            if raw == Some(107) || raw == Some(116) {
                Some(format!("{e}"))
            } else {
                None
            }
        }
    }
}

/// Check whether a path is a live mount (different device from parent).
fn is_mountpoint(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(path_meta) = std::fs::metadata(path) else {
            return false;
        };
        let Some(parent) = path.parent() else {
            return false;
        };
        let Ok(parent_meta) = std::fs::metadata(parent) else {
            return false;
        };
        path_meta.dev() != parent_meta.dev()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

fn check_stale_mounts(
    ui: &mut Ui,
    storage_dir: &Path,
    fix: bool,
    total_errors: &mut u32,
    total_warnings: &mut u32,
    _total_fixed: &mut u32,
) -> Result<(), jj_cli::command_error::CommandError> {
    // 1. Check the managed mount root (~/kiki/).
    let mount_root = crate::daemon_cmd::configured_mount_root();
    if mount_root.exists() || is_stale_mount(&mount_root).is_some() {
        if let Some(err_msg) = is_stale_mount(&mount_root) {
            writeln!(
                ui.status(),
                "  mount root ({}):",
                mount_root.display()
            )?;
            writeln!(
                ui.status(),
                "    STALE MOUNT — {err_msg}"
            )?;
            writeln!(
                ui.status(),
                "    The managed mount root has a stale FUSE/NFS mount from a \
                 crashed daemon."
            )?;
            if fix {
                write!(ui.status(), "    unmounting... ")?;
                match force_unmount(&mount_root) {
                    Ok(()) => {
                        writeln!(ui.status(), "ok")?;
                        // Don't count as "fixed" since they'll need to restart the daemon
                    }
                    Err(e) => {
                        writeln!(ui.status(), "FAILED: {e}")?;
                        writeln!(
                            ui.status(),
                            "    Try manually: fusermount3 -u {}",
                            mount_root.display()
                        )?;
                    }
                }
            } else {
                writeln!(
                    ui.status(),
                    "    Fix with: fusermount3 -u {}",
                    mount_root.display()
                )?;
            }
            *total_errors += 1;
            writeln!(ui.status())?;
        } else if is_mountpoint(&mount_root) {
            writeln!(ui.status(), "  mount root ({}): ok (mounted)", mount_root.display())?;
        } else {
            writeln!(ui.status(), "  mount root ({}): ok (not mounted)", mount_root.display())?;
        }
    }

    // 2. Check legacy ad-hoc mounts.
    let mounts_dir = storage_dir.join("mounts");
    if mounts_dir.exists() {
        if let Ok(entries) = kiki_daemon::mount_meta::list_persisted(storage_dir) {
            for (_mount_dir, meta) in &entries {
                let mount_path = Path::new(&meta.working_copy_path);
                if let Some(err_msg) = is_stale_mount(mount_path) {
                    writeln!(
                        ui.status(),
                        "  legacy mount ({}):",
                        meta.working_copy_path
                    )?;
                    writeln!(
                        ui.status(),
                        "    STALE MOUNT — {err_msg}"
                    )?;
                    if fix {
                        write!(ui.status(), "    unmounting... ")?;
                        match force_unmount(mount_path) {
                            Ok(()) => writeln!(ui.status(), "ok")?,
                            Err(e) => {
                                writeln!(ui.status(), "FAILED: {e}")?;
                                writeln!(
                                    ui.status(),
                                    "    Try manually: fusermount3 -u {}",
                                    meta.working_copy_path
                                )?;
                            }
                        }
                    } else {
                        writeln!(
                            ui.status(),
                            "    Fix with: fusermount3 -u {}",
                            meta.working_copy_path
                        )?;
                    }
                    *total_errors += 1;
                } else if mount_path.exists() && is_mountpoint(mount_path) {
                    // Active mount — check if it's healthy by trying to readdir.
                    if let Err(e) = std::fs::read_dir(mount_path) {
                        writeln!(
                            ui.status(),
                            "  legacy mount ({}):",
                            meta.working_copy_path
                        )?;
                        writeln!(
                            ui.status(),
                            "    WARN — mount exists but readdir failed: {e}"
                        )?;
                        *total_warnings += 1;
                    }
                } else if mount_path.exists() && !is_mountpoint(mount_path) {
                    // Path exists but is not a mount — leaked metadata from a
                    // previous daemon session. The mount was cleaned up but the
                    // metadata in mounts/ was not.
                    writeln!(
                        ui.status(),
                        "  legacy mount ({}):",
                        meta.working_copy_path
                    )?;
                    writeln!(
                        ui.status(),
                        "    WARN — orphaned mount metadata (path exists but is not \
                         mounted). Stale state in {}",
                        _mount_dir.display()
                    )?;
                    if fix {
                        write!(ui.status(), "    removing orphaned metadata... ")?;
                        match std::fs::remove_dir_all(_mount_dir) {
                            Ok(()) => writeln!(ui.status(), "ok")?,
                            Err(e) => writeln!(ui.status(), "FAILED: {e}")?,
                        }
                    }
                    *total_warnings += 1;
                } else if !mount_path.exists() {
                    // Mount metadata exists but the mountpoint directory doesn't.
                    writeln!(
                        ui.status(),
                        "  legacy mount ({}):",
                        meta.working_copy_path
                    )?;
                    writeln!(
                        ui.status(),
                        "    WARN — orphaned mount metadata (mountpoint is gone). \
                         Stale state in {}",
                        _mount_dir.display()
                    )?;
                    if fix {
                        write!(ui.status(), "    removing orphaned metadata... ")?;
                        match std::fs::remove_dir_all(_mount_dir) {
                            Ok(()) => writeln!(ui.status(), "ok")?,
                            Err(e) => writeln!(ui.status(), "FAILED: {e}")?,
                        }
                    }
                    *total_warnings += 1;
                }
            }
        }
    }

    Ok(())
}

fn force_unmount(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let output = std::process::Command::new("umount")
        .arg("-f")
        .arg(path)
        .output()
        .map_err(|e| format!("running umount -f: {e}"))?;

    #[cfg(not(target_os = "macos"))]
    let output = std::process::Command::new("fusermount3")
        .arg("-u")
        .arg(path)
        .output()
        .map_err(|e| format!("running fusermount3 -u: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

// ── Fix routines ───────────────────────────────────────────────────

fn fix_git_worktree(
    storage_dir: &Path,
    repo_name: &str,
    ws_name: &str,
) -> Result<(), String> {
    let git_store_path = repo_meta::repo_git_store_path(storage_dir, repo_name);
    let git_path = git_store_path.join("git");
    kiki_daemon::git_ops::init_worktree(&git_path, ws_name)
        .map(|_| ())
        .map_err(|e| format!("init_worktree: {e:#}"))
}

// ── Entry point ────────────────────────────────────────────────────

pub fn run_doctor(
    ui: &mut Ui,
    fix: bool,
    filter_repo: Option<&str>,
) -> Result<(), jj_cli::command_error::CommandError> {
    use jj_cli::command_error::user_error;

    let storage_dir = store::paths::default_storage_dir();
    let repos_path = repo_meta::repos_config_path(&storage_dir);

    writeln!(ui.status(), "kiki doctor")?;
    writeln!(ui.status(), "  storage: {}", storage_dir.display())?;
    writeln!(ui.status())?;

    let mut total_errors = 0u32;
    let mut total_warnings = 0u32;
    let mut total_fixed = 0u32;

    // ── Check for stale mounts before anything else ──────────────
    check_stale_mounts(ui, &storage_dir, fix, &mut total_errors, &mut total_warnings, &mut total_fixed)?;

    let repos_cfg = repo_meta::ReposConfig::read_or_default(&repos_path)
        .map_err(|e| user_error(format!("failed to read repos.toml: {e:#}")))?;

    if repos_cfg.repos.is_empty() && total_errors == 0 && total_warnings == 0 {
        writeln!(ui.status(), "  No repos registered.")?;
        return Ok(());
    }

    for (repo_name, _entry) in &repos_cfg.repos {
        if let Some(filter) = filter_repo {
            if repo_name != filter {
                continue;
            }
        }

        let sections = check_repo(&storage_dir, repo_name);

        for (section_name, checks) in &sections {
            let has_issues = checks.iter().any(|c| c.severity != Severity::Ok);
            if !has_issues {
                writeln!(ui.status(), "  {section_name}: ok")?;
                continue;
            }

            writeln!(ui.status(), "  {section_name}:")?;
            for check in checks {
                let marker = match (check.severity, check.fixable) {
                    (Severity::Ok, _) => "  ok",
                    (Severity::Warning, _) => "  WARN",
                    (Severity::Error, true) => "  ERROR (fixable)",
                    (Severity::Error, false) => "  ERROR",
                };
                writeln!(
                    ui.status(),
                    "    {}: {} {}",
                    check.label,
                    marker,
                    if check.detail.is_empty() {
                        String::new()
                    } else {
                        format!("— {}", check.detail)
                    }
                )?;
                match check.severity {
                    Severity::Warning => total_warnings += 1,
                    Severity::Error => total_errors += 1,
                    Severity::Ok => {}
                }
            }
        }

        // ── Attempt fixes ──────────────────────────────────────────
        if fix {
            // Collect workspace configs for fix attempts.
            if let Ok(workspaces) = repo_meta::list_workspace_configs(&storage_dir, repo_name) {
                for (ws_name, ws_cfg) in &workspaces {
                    // Fix: missing git worktree.
                    let wt_dir = repo_meta::workspace_worktree_gitdir(
                        &storage_dir, repo_name, ws_name,
                    );
                    if !wt_dir.exists() || !wt_dir.join("HEAD").exists() {
                        write!(
                            ui.status(),
                            "    fixing git worktree for {repo_name}/{ws_name}... "
                        )?;
                        match fix_git_worktree(&storage_dir, repo_name, ws_name) {
                            Ok(()) => {
                                writeln!(ui.status(), "ok")?;
                                total_fixed += 1;
                            }
                            Err(e) => {
                                writeln!(ui.status(), "FAILED: {e}")?;
                            }
                        }
                    }

                    // Fix: empty op_id (zombie workspace).
                    if ws_cfg.op_id.is_empty() || ws_cfg.workspace_id.is_empty() {
                        writeln!(
                            ui.status(),
                            "    {repo_name}/{ws_name}: jj state is uninitialized"
                        )?;
                        writeln!(
                            ui.status(),
                            "    To repair, run with the daemon running:"
                        )?;
                        writeln!(
                            ui.status(),
                            "      kiki kk doctor --fix"
                        )?;
                        writeln!(
                            ui.status(),
                            "    Or delete and re-clone:"
                        )?;
                        writeln!(
                            ui.status(),
                            "      kiki workspace delete {repo_name}/{ws_name}"
                        )?;
                        // NOTE: Full jj re-init requires a daemon connection
                        // and the init_jj_workspace flow from main.rs. This
                        // is a filesystem-level tool, so we can only fix
                        // filesystem-level issues. The jj re-init needs the
                        // daemon-backed store factories. We report the issue
                        // and guide the user.
                        //
                        // In the future, this could call init_jj_workspace
                        // via a daemon RPC.
                    }
                }
            }
        }

        writeln!(ui.status())?;
    }

    // Summary.
    if total_errors == 0 && total_warnings == 0 {
        writeln!(ui.status(), "All checks passed.")?;
    } else {
        writeln!(
            ui.status(),
            "Found {} error(s), {} warning(s).",
            total_errors,
            total_warnings
        )?;
        if fix && total_fixed > 0 {
            writeln!(ui.status(), "Fixed {} issue(s).", total_fixed)?;
        }
        if total_errors > 0 && !fix {
            writeln!(
                ui.status(),
                "Run `kiki kk doctor --fix` to attempt repairs."
            )?;
        }
    }

    Ok(())
}
