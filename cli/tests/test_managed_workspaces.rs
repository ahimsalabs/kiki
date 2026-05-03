//! End-to-end tests for M12 managed workspaces.
//!
//! These tests exercise the `kiki clone` and `kiki workspace` CLI commands
//! through a real daemon with FUSE enabled. The daemon mounts a single
//! `RootFs` at a per-test `mount_root`, and the CLI creates/manages
//! workspaces in the `/<repo>/<workspace>/` namespace.
//!
//! **Requires:** `/dev/fuse` and `fusermount3` available. Tests are skipped
//! if FUSE is unavailable or `KIKI_TEST_DISABLE_MOUNT=1` is set.
//!
//! ## Test matrix
//!
//! | Test | Property |
//! |------|----------|
//! | `clone_creates_workspace` | `kiki clone dir://...` creates repo + default workspace |
//! | `clone_derives_name_from_url` | repo name derived from URL path |
//! | `clone_explicit_name` | `--name` overrides derived name |
//! | `workspace_list_after_clone` | `kiki workspace list` shows default |
//! | `workspace_create_and_list` | creating a second workspace shows both |
//! | `workspace_delete` | deleted workspace disappears from list |
//! | `workspace_files_independent` | writes in one workspace don't appear in another |
//! | `jj_status_in_workspace` | `jj status` works inside managed workspace |
//! | `jj_commands_in_workspace` | `jj new`, `jj describe` work normally |
//! | `shared_store_across_workspaces` | committed blob visible from sibling workspace |
//! | `daemon_restart_preserves_state` | repos/workspaces survive daemon restart |

use std::path::{Path, PathBuf};

/// Returns true if FUSE is available and managed-workspace e2e tests can run.
fn fuse_available() -> bool {
    if std::env::var("KIKI_TEST_DISABLE_MOUNT").is_ok() {
        return false;
    }
    if !Path::new("/dev/fuse").exists() {
        return false;
    }
    // Check fusermount3 is on PATH.
    std::process::Command::new("fusermount3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Extended TestEnvironment that writes kiki config before daemon startup.
/// This ensures the daemon reads mount_root on first start.
struct ManagedTestEnvironment {
    _temp_dir: tempfile::TempDir,
    env_root: PathBuf,
    home_dir: PathBuf,
    mount_root: PathBuf,
    cli_config_path: PathBuf,
    env_vars: std::collections::HashMap<String, String>,
    config_file_number: std::cell::RefCell<i64>,
    command_number: std::cell::RefCell<i64>,
    daemon_child: std::process::Child,
    daemon_dir: PathBuf,
    socket_path: PathBuf,
}

impl Drop for ManagedTestEnvironment {
    fn drop(&mut self) {
        let _ = self.daemon_child.kill();
    }
}

impl ManagedTestEnvironment {
    /// Create a test env with managed workspace support.
    /// Writes kiki config.toml BEFORE spawning the daemon so mount_root is set.
    fn new() -> Self {
        let tmp_dir = tempfile::TempDir::with_prefix("kiki-m12-test").unwrap();
        let env_root = tmp_dir.path().canonicalize().unwrap();

        let home_dir = env_root.join("home");
        std::fs::create_dir(&home_dir).unwrap();
        let config_dir = env_root.join("config");
        std::fs::create_dir(&config_dir).unwrap();
        let daemon_dir = env_root.join("daemon");
        std::fs::create_dir(&daemon_dir).unwrap();

        // Create mount_root directory for the RootFs FUSE mount.
        let mount_root = env_root.join("mnt");
        std::fs::create_dir(&mount_root).unwrap();

        // Write kiki config.toml with mount_root BEFORE starting daemon.
        let kiki_config_dir = home_dir.join(".config/kiki");
        std::fs::create_dir_all(&kiki_config_dir).unwrap();
        let config_content = format!(
            "mount_root = {:?}\n",
            mount_root.to_str().unwrap()
        );
        std::fs::write(kiki_config_dir.join("config.toml"), &config_content).unwrap();

        // Spawn daemon with FUSE enabled (no --disable-mount).
        let socket_path = env_root.join("daemon.sock");
        let kiki = assert_cmd::cargo::cargo_bin("kiki");
        let mut command = std::process::Command::new(kiki);
        command.args([
            "kk", "daemon", "run",
            "--storage-dir", daemon_dir.to_str().unwrap(),
        ]);
        command.env("HOME", home_dir.to_str().unwrap());
        command.env("KIKI_SOCKET_PATH", socket_path.to_str().unwrap());
        // Suppress daemon output to avoid noise.
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::null());
        let daemon_child = command
            .spawn()
            .expect("Failed to start daemon for managed workspace test");

        // Wait for daemon socket.
        for _ in 0..100 {
            if socket_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            socket_path.exists(),
            "daemon failed to start (socket not found after 5s)"
        );

        // Wait for FUSE mount to appear. The daemon binds the mount
        // asynchronously after accepting the socket. Give it time.
        for _ in 0..100 {
            // A FUSE mount makes the directory a mountpoint. We can check
            // by seeing if it has a different device ID from its parent.
            if is_mountpoint(&mount_root) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if !is_mountpoint(&mount_root) {
            // If mount didn't succeed, the daemon may have failed.
            // This can happen if fusermount3 isn't available.
            panic!(
                "FUSE mount at {} did not appear within 10s. \
                 fusermount3 may not be available.",
                mount_root.display()
            );
        }

        let env_vars = std::collections::HashMap::new();
        let env = Self {
            _temp_dir: tmp_dir,
            env_root,
            home_dir,
            mount_root,
            cli_config_path: config_dir,
            env_vars,
            config_file_number: std::cell::RefCell::new(0),
            command_number: std::cell::RefCell::new(0),
            daemon_child,
            daemon_dir,
            socket_path,
        };
        env.add_config(
            r#"
[template-aliases]
'format_time_range(time_range)' = 'time_range.start() ++ " - " ++ time_range.end()'
        "#,
        );
        env
    }

    pub fn kiki_cmd(&self, current_dir: &Path, args: &[&str]) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::cargo_bin("kiki").unwrap();
        cmd.current_dir(current_dir);
        cmd.args(args);
        cmd.env_clear();
        cmd.env("COLUMNS", "100");
        for (key, value) in &self.env_vars {
            cmd.env(key, value);
        }
        cmd.env("RUST_BACKTRACE", "1");
        cmd.env("HOME", self.home_dir.to_str().unwrap());
        cmd.env("JJ_CONFIG", self.cli_config_path.to_str().unwrap());
        cmd.env("JJ_USER", "Test User");
        cmd.env("JJ_EMAIL", "test.user@example.com");
        cmd.env("JJ_OP_HOSTNAME", "host.example.com");
        cmd.env("JJ_OP_USERNAME", "test-username");
        cmd.env("JJ_TZ_OFFSET_MINS", "660");
        cmd.env("KIKI_SOCKET_PATH", self.socket_path.to_str().unwrap());

        let mut command_number = self.command_number.borrow_mut();
        *command_number += 1;
        cmd.env("JJ_RANDOMNESS_SEED", command_number.to_string());
        let timestamp =
            chrono::DateTime::parse_from_rfc3339("2001-02-03T04:05:06+07:00").unwrap();
        let timestamp =
            timestamp + chrono::Duration::try_seconds(*command_number).unwrap();
        cmd.env("JJ_TIMESTAMP", timestamp.to_rfc3339());
        cmd.env("JJ_OP_TIMESTAMP", timestamp.to_rfc3339());
        cmd
    }

    pub fn kiki_cmd_ok(&self, current_dir: &Path, args: &[&str]) -> (String, String) {
        let assert = self.kiki_cmd(current_dir, args).assert().success();
        let stdout = self.normalize_output(&get_stdout_string(&assert));
        let stderr = self.normalize_output(&get_stderr_string(&assert));
        (stdout, stderr)
    }

    #[allow(dead_code)]
    pub fn kiki_cmd_failure(&self, current_dir: &Path, args: &[&str]) -> String {
        let assert = self.kiki_cmd(current_dir, args).assert().code(1).stdout("");
        self.normalize_output(&get_stderr_string(&assert))
    }

    pub fn add_config(&self, content: &str) {
        let mut config_file_number = self.config_file_number.borrow_mut();
        *config_file_number += 1;
        let n = *config_file_number;
        std::fs::write(
            self.cli_config_path.join(format!("config{n:04}.toml")),
            content,
        )
        .unwrap();
    }

    pub fn mount_root(&self) -> &Path {
        &self.mount_root
    }

    pub fn env_root(&self) -> &Path {
        &self.env_root
    }

    #[allow(dead_code)]
    pub fn daemon_dir(&self) -> &Path {
        &self.daemon_dir
    }

    /// Stop the daemon process. Used for restart tests.
    pub fn stop_daemon(&mut self) {
        let _ = self.daemon_child.kill();
        let _ = self.daemon_child.wait();
        // Explicitly unmount — killing the daemon leaves a stale mount.
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", self.mount_root.to_str().unwrap()])
            .status();
        // Wait for mount to be cleaned up.
        for _ in 0..50 {
            if !is_mountpoint(&self.mount_root) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// Start a fresh daemon process. Used for restart tests.
    pub fn start_daemon(&mut self) {
        let kiki = assert_cmd::cargo::cargo_bin("kiki");
        let mut command = std::process::Command::new(kiki);
        command.args([
            "kk", "daemon", "run",
            "--storage-dir", self.daemon_dir.to_str().unwrap(),
        ]);
        command.env("HOME", self.home_dir.to_str().unwrap());
        command.env("KIKI_SOCKET_PATH", self.socket_path.to_str().unwrap());
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::null());
        self.daemon_child = command
            .spawn()
            .expect("Failed to restart daemon");

        // Wait for socket.
        for _ in 0..100 {
            if self.socket_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // Wait for FUSE mount.
        for _ in 0..100 {
            if is_mountpoint(&self.mount_root) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(
            is_mountpoint(&self.mount_root),
            "FUSE mount did not reappear after daemon restart"
        );
    }

    fn normalize_output(&self, text: &str) -> String {
        let text = text.replace("jj.exe", "jj");
        let regex = regex::Regex::new(&format!(
            r"{}(\S+)",
            regex::escape(&self.env_root.display().to_string())
        ))
        .unwrap();
        regex
            .replace_all(&text, |caps: &regex::Captures| {
                format!("$TEST_ENV{}", caps[1].replace('\\', "/"))
            })
            .to_string()
    }
}

/// Check if a path is a mountpoint by comparing device IDs with parent.
fn is_mountpoint(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return true; // root is always a mountpoint
    };
    let Ok(parent_meta) = std::fs::metadata(parent) else {
        return false;
    };
    meta.dev() != parent_meta.dev()
}

fn get_stdout_string(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn get_stderr_string(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

/// Create a `dir://` remote with an initial empty commit for cloning.
fn create_dir_remote() -> tempfile::TempDir {
    let tmp = tempfile::TempDir::with_prefix("kiki-remote").unwrap();
    // dir:// remotes just need to exist as a directory. The daemon's
    // FsRemoteStore creates it on first write. For clone tests the
    // remote can start empty — the Clone RPC creates the local store.
    tmp
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[test]
fn clone_creates_workspace() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let env = ManagedTestEnvironment::new();
    let remote = create_dir_remote();
    let remote_url = format!("dir://{}", remote.path().canonicalize().unwrap().display());

    let (stdout, stderr) = env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &remote_url, "--name", "myrepo"],
    );
    // Clone output goes to stderr (status messages) or stdout.
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("myrepo") || combined.contains("default"),
        "clone output should mention repo/workspace; got stdout={stdout:?} stderr={stderr:?}"
    );

    // The workspace should be accessible via the FUSE mount.
    let ws_path = env.mount_root().join("myrepo/default");
    assert!(ws_path.exists(), "workspace path should exist via FUSE");
    assert!(ws_path.join(".jj").exists(), "workspace should have .jj/");
}

#[test]
fn clone_derives_name_from_url() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let env = ManagedTestEnvironment::new();
    // Create a remote dir with a meaningful name for derivation.
    let remote_dir = env.env_root().join("my-project");
    std::fs::create_dir(&remote_dir).unwrap();
    let remote_url = format!("dir://{}", remote_dir.canonicalize().unwrap().display());

    env.kiki_cmd_ok(env.env_root(), &["clone", &remote_url]);

    let ws_path = env.mount_root().join("my-project/default");
    assert!(ws_path.exists(), "derived name 'my-project' should appear");
}

#[test]
fn clone_explicit_name() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let env = ManagedTestEnvironment::new();
    let remote = create_dir_remote();
    let remote_url = format!("dir://{}", remote.path().canonicalize().unwrap().display());

    env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &remote_url, "--name", "custom-name"],
    );

    let ws_path = env.mount_root().join("custom-name/default");
    assert!(ws_path.exists());
}

#[test]
fn workspace_list_after_clone() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let env = ManagedTestEnvironment::new();
    let remote = create_dir_remote();
    let remote_url = format!("dir://{}", remote.path().canonicalize().unwrap().display());

    env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &remote_url, "--name", "myrepo"],
    );

    // Run workspace list from inside the workspace (is_kiki_backend check).
    let ws_path = env.mount_root().join("myrepo/default");
    let (stdout, _) = env.kiki_cmd_ok(&ws_path, &["workspace", "list"]);
    assert!(stdout.contains("default"), "workspace list should show 'default'");
}

#[test]
fn workspace_create_and_list() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let env = ManagedTestEnvironment::new();
    let remote = create_dir_remote();
    let remote_url = format!("dir://{}", remote.path().canonicalize().unwrap().display());

    env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &remote_url, "--name", "myrepo"],
    );

    // Create a second workspace from inside the default workspace.
    let default_path = env.mount_root().join("myrepo/default");
    let (stdout, stderr) = env.kiki_cmd_ok(
        &default_path,
        &["workspace", "add", "myrepo/feature"],
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("feature") || combined.contains("myrepo"),
        "create output should reference the workspace; got: {combined}"
    );

    // Both workspaces should appear in the FUSE namespace.
    let feature_path = env.mount_root().join("myrepo/feature");
    assert!(default_path.exists());
    assert!(feature_path.exists());
    assert!(feature_path.join(".jj").exists());

    // List should show both.
    let (stdout, _) = env.kiki_cmd_ok(&default_path, &["workspace", "list"]);
    assert!(stdout.contains("default"));
    assert!(stdout.contains("feature"));
}

#[test]
fn workspace_delete() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let env = ManagedTestEnvironment::new();
    let remote = create_dir_remote();
    let remote_url = format!("dir://{}", remote.path().canonicalize().unwrap().display());

    env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &remote_url, "--name", "myrepo"],
    );
    let default_path = env.mount_root().join("myrepo/default");
    env.kiki_cmd_ok(
        &default_path,
        &["workspace", "add", "myrepo/ephemeral"],
    );

    // Delete the workspace.
    env.kiki_cmd_ok(
        &default_path,
        &["workspace", "forget", "myrepo/ephemeral"],
    );

    // Should no longer appear in FUSE namespace.
    let ws_path = env.mount_root().join("myrepo/ephemeral");
    assert!(!ws_path.exists(), "deleted workspace should disappear from FUSE");

    // Should not appear in list.
    let (stdout, _) = env.kiki_cmd_ok(&default_path, &["workspace", "list"]);
    assert!(!stdout.contains("ephemeral"));
    assert!(stdout.contains("default"));
}

#[test]
fn workspace_files_independent() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let env = ManagedTestEnvironment::new();
    let remote = create_dir_remote();
    let remote_url = format!("dir://{}", remote.path().canonicalize().unwrap().display());

    env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &remote_url, "--name", "myrepo"],
    );
    let default_path = env.mount_root().join("myrepo/default");
    env.kiki_cmd_ok(
        &default_path,
        &["workspace", "add", "myrepo/branch-a"],
    );

    let branch_a_path = env.mount_root().join("myrepo/branch-a");

    // Write a file in the default workspace.
    std::fs::write(default_path.join("hello.txt"), "from default\n").unwrap();

    // The file should NOT appear in branch-a (dirty state is per-workspace).
    assert!(
        !branch_a_path.join("hello.txt").exists(),
        "dirty file in default should not appear in branch-a"
    );
}

#[test]
fn jj_status_in_workspace() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let env = ManagedTestEnvironment::new();
    let remote = create_dir_remote();
    let remote_url = format!("dir://{}", remote.path().canonicalize().unwrap().display());

    env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &remote_url, "--name", "myrepo"],
    );

    let ws_path = env.mount_root().join("myrepo/default");

    // `jj status` (via kiki kk passthrough) should work.
    let (stdout, _) = env.kiki_cmd_ok(&ws_path, &["st"]);
    // Should show the working copy with no changes (clean after init).
    assert!(
        stdout.contains("Working copy")
            || stdout.contains("working copy")
            || stdout.is_empty()
            || stdout.contains("The working copy has no parent commit"),
        "jj status should succeed; got: {stdout}"
    );
}

#[test]
fn jj_commands_in_workspace() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let env = ManagedTestEnvironment::new();
    let remote = create_dir_remote();
    let remote_url = format!("dir://{}", remote.path().canonicalize().unwrap().display());

    env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &remote_url, "--name", "myrepo"],
    );

    let ws_path = env.mount_root().join("myrepo/default");

    // Write a file.
    std::fs::write(ws_path.join("README.md"), "# Hello\n").unwrap();

    // `jj new` to create a new commit.
    env.kiki_cmd_ok(&ws_path, &["new"]);

    // `jj describe` on the parent.
    env.kiki_cmd_ok(&ws_path, &["describe", "@-", "-m", "add readme"]);

    // `jj log` should show the description.
    let (stdout, _) = env.kiki_cmd_ok(&ws_path, &["log", "--no-graph", "-r", "@-"]);
    assert!(
        stdout.contains("add readme"),
        "jj log should show the description; got: {stdout}"
    );
}

#[test]
fn shared_store_across_workspaces() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let env = ManagedTestEnvironment::new();
    let remote = create_dir_remote();
    let remote_url = format!("dir://{}", remote.path().canonicalize().unwrap().display());

    env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &remote_url, "--name", "myrepo"],
    );
    let default_path = env.mount_root().join("myrepo/default");
    env.kiki_cmd_ok(
        &default_path,
        &["workspace", "add", "myrepo/ws2"],
    );

    // Write and commit a file in the default workspace.
    std::fs::write(default_path.join("data.txt"), "shared content\n").unwrap();
    env.kiki_cmd_ok(&default_path, &["new"]);
    env.kiki_cmd_ok(&default_path, &["describe", "@-", "-m", "add data"]);

    // From ws2, we should be able to see the commit in the log (shared store).
    let ws2_path = env.mount_root().join("myrepo/ws2");
    let (stdout, _) = env.kiki_cmd_ok(&ws2_path, &["log", "--no-graph"]);
    assert!(
        stdout.contains("add data"),
        "ws2 should see commits from default workspace (shared store); got: {stdout}"
    );
}

#[test]
fn daemon_restart_preserves_state() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let mut env = ManagedTestEnvironment::new();
    let remote = create_dir_remote();
    let remote_url = format!("dir://{}", remote.path().canonicalize().unwrap().display());

    env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &remote_url, "--name", "myrepo"],
    );
    let ws_path = env.mount_root().join("myrepo/default");
    env.kiki_cmd_ok(
        &ws_path,
        &["workspace", "add", "myrepo/persist-test"],
    );
    // Restart daemon.
    env.stop_daemon();

    // Remove stale socket so the new daemon can bind.
    let _ = std::fs::remove_file(env.env_root().join("daemon.sock"));

    env.start_daemon();

    // After restart, the workspaces should reappear via FUSE.
    let ws_path = env.mount_root().join("myrepo/default");
    let ws2_path = env.mount_root().join("myrepo/persist-test");
    assert!(ws_path.exists(), "default workspace should reappear after restart");
    assert!(ws2_path.exists(), "persist-test workspace should reappear after restart");

    // The repo dir should show both workspaces in a directory listing.
    let repo_dir = env.mount_root().join("myrepo");
    let entries: Vec<String> = std::fs::read_dir(&repo_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(entries.contains(&"default".to_string()));
    assert!(entries.contains(&"persist-test".to_string()));
}

/// After committing a file (which triggers Snapshot+CheckOut, persisting
/// root_tree_id to workspace.toml), a daemon restart should preserve the
/// file content because the KikiFs is rehydrated with the stored tree.
#[test]
fn daemon_restart_preserves_file_content() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let mut env = ManagedTestEnvironment::new();
    let remote = create_dir_remote();
    let remote_url = format!("dir://{}", remote.path().canonicalize().unwrap().display());

    env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &remote_url, "--name", "myrepo"],
    );
    let ws_path = env.mount_root().join("myrepo/default");

    // Write a file and commit it. `jj new` triggers:
    //   1. Snapshot (captures hello.txt, persists root_tree_id of snapshotted tree)
    //   2. CheckOut (new commit's tree, persists root_tree_id of @)
    // After jj new, @ is a fresh empty commit and @- has hello.txt.
    // But the tree of @ at this point includes hello.txt because jj's
    // working copy semantics: the file stays in the working copy.
    std::fs::write(ws_path.join("hello.txt"), "persist me\n").unwrap();
    env.kiki_cmd_ok(&ws_path, &["new"]);

    // Verify the file is visible (it's in @-'s tree but also in working copy).
    let content = std::fs::read_to_string(ws_path.join("hello.txt")).unwrap();
    assert_eq!(content, "persist me\n", "file should exist before restart");

    // Restart the daemon.
    env.stop_daemon();
    let _ = std::fs::remove_file(env.env_root().join("daemon.sock"));
    env.start_daemon();

    // After restart, the workspace should still have hello.txt because
    // root_tree_id was persisted by the CheckOut during `jj new`.
    let ws_path = env.mount_root().join("myrepo/default");
    assert!(ws_path.exists(), "workspace should reappear after restart");
    let content = std::fs::read_to_string(ws_path.join("hello.txt")).unwrap();
    assert_eq!(
        content, "persist me\n",
        "file content should survive daemon restart (root_tree_id persisted)"
    );
}

/// Create a local bare git repo with content for git clone tests.
fn create_git_repo_with_content() -> tempfile::TempDir {
    let tmp = tempfile::TempDir::with_prefix("kiki-git-remote").unwrap();
    let repo_path = tmp.path().join("repo.git");

    // Init bare repo, add a file via a temporary worktree.
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();

    let run = |args: &[&str], dir: &Path| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };

    run(&["init", "--bare", repo_path.to_str().unwrap()], tmp.path());
    run(&["clone", repo_path.to_str().unwrap(), work_dir.to_str().unwrap()], tmp.path());
    std::fs::write(work_dir.join("README.md"), "# Hello from git\n").unwrap();
    run(&["add", "README.md"], &work_dir);
    run(&["-c", "user.name=Test", "-c", "user.email=test@test", "commit", "-m", "initial"], &work_dir);
    run(&["push", "origin", "main"], &work_dir);

    tmp
}

/// `kiki clone <git-url>` should immediately show the default branch's
/// content after clone (initial_tree_id from GitClone RPC + CheckOut).
#[test]
fn git_clone_shows_content_immediately() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let git_remote = create_git_repo_with_content();
    let git_url = format!("file://{}", git_remote.path().join("repo.git").display());

    let env = ManagedTestEnvironment::new();
    let (stdout, stderr) = env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &git_url, "--name", "gitrepo"],
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("default branch: main"),
        "should report default branch; got: {combined}"
    );

    // The workspace should immediately have the file from git.
    let ws_path = env.mount_root().join("gitrepo/default");
    assert!(ws_path.exists(), "workspace should exist after clone");
    let content = std::fs::read_to_string(ws_path.join("README.md")).unwrap();
    assert_eq!(
        content, "# Hello from git\n",
        "git clone should materialize default branch content immediately"
    );
}

/// Content materialized by git clone initial_tree_id should persist across
/// daemon restart.
#[test]
fn git_clone_content_persists_across_restart() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let git_remote = create_git_repo_with_content();
    let git_url = format!("file://{}", git_remote.path().join("repo.git").display());

    let mut env = ManagedTestEnvironment::new();
    env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &git_url, "--name", "gitrepo"],
    );
    let ws_path = env.mount_root().join("gitrepo/default");
    // Verify content exists before restart.
    let content = std::fs::read_to_string(ws_path.join("README.md")).unwrap();
    assert_eq!(content, "# Hello from git\n");

    // Restart daemon.
    env.stop_daemon();
    let _ = std::fs::remove_file(env.env_root().join("daemon.sock"));
    env.start_daemon();

    // Content should survive restart.
    let ws_path = env.mount_root().join("gitrepo/default");
    assert!(ws_path.exists(), "workspace should reappear after restart");
    let content = std::fs::read_to_string(ws_path.join("README.md")).unwrap();
    assert_eq!(
        content, "# Hello from git\n",
        "git clone content should survive daemon restart (initial_tree_id persisted)"
    );
}

/// `git commit` inside a kiki mount should be visible in `kiki log`
/// via the git import dispatch hook (colocated workflow).
#[test]
fn git_commit_visible_in_kiki_log() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let git_remote = create_git_repo_with_content();
    let git_url = format!("file://{}", git_remote.path().join("repo.git").display());

    let env = ManagedTestEnvironment::new();
    env.kiki_cmd_ok(
        env.env_root(),
        &["clone", &git_url, "--name", "coloc"],
    );
    let ws_path = env.mount_root().join("coloc/default");

    // Establish the HEAD export baseline: any kiki command that triggers a
    // snapshot will call WriteCommit → export_head, seeding
    // `last_exported_head`. The clone already does this, but run an
    // explicit `jj log` to be sure the import hook has run at least once.
    env.kiki_cmd_ok(&ws_path, &["log", "--no-graph", "--limit", "1"]);

    // Write a file and commit it via stock git (not jj/kiki).
    // The FUSE mount has different ownership, so git's safe.directory
    // check must be relaxed.
    std::fs::write(ws_path.join("colocated.txt"), "git was here\n").unwrap();

    let git_add = std::process::Command::new("git")
        .args(["-c", "safe.directory=*", "add", "colocated.txt"])
        .current_dir(&ws_path)
        .output()
        .expect("git add");
    assert!(
        git_add.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&git_add.stderr)
    );

    let git_commit = std::process::Command::new("git")
        .args([
            "-c", "safe.directory=*",
            "-c", "user.name=Test User",
            "-c", "user.email=test@test.com",
            "commit", "-m", "colocated commit",
        ])
        .current_dir(&ws_path)
        .output()
        .expect("git commit");
    assert!(
        git_commit.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&git_commit.stderr)
    );

    // Now `kiki log` should see the commit via the import hook.
    let (stdout, _) = env.kiki_cmd_ok(&ws_path, &["log", "--no-graph"]);
    assert!(
        stdout.contains("colocated commit"),
        "kiki log should show the git commit; got: {stdout}"
    );
}
