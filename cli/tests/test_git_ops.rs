//! Integration tests for `kiki git` operations (push, fetch, remote).
//!
//! `test_git_*` tests use `file://` bare repos and run in normal CI — no
//! external deps.
//!
//! `test_git_fetch_and_push_round_trip` (Gitea variant) is gated behind
//! `KIKI_TEST_GITEA=1`. To run:
//!   docker run -d --name gitea-test -p 3000:3000 \
//!     -e GITEA__security__INSTALL_LOCK=true gitea/gitea:latest
//!   docker exec --user git gitea-test gitea admin user create \
//!     --admin --username testuser --password testpass123 \
//!     --email test@test.com --must-change-password=false
//!   KIKI_TEST_GITEA=1 cargo test --package kiki --test runner test_git_ops

use std::process::Command;

use crate::common::TestEnvironment;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a bare git repo at `<parent_dir>/<name>.git` and return its
/// `file://` URL. Optionally seed it with branches via `seed_fn`.
fn create_bare_repo(
    parent_dir: &std::path::Path,
    name: &str,
    seed_fn: impl FnOnce(&std::path::Path),
) -> String {
    let upstream = parent_dir.join(format!("{name}.git"));
    let out = Command::new("git")
        .args(["init", "--bare"])
        .arg(&upstream)
        .output()
        .expect("git init --bare");
    assert!(out.status.success(), "git init --bare failed");

    // Clone into a workdir so the seed function can commit + push.
    let workdir = parent_dir.join(format!("{name}-workdir"));
    let out = Command::new("git")
        .args(["clone"])
        .arg(&upstream)
        .arg(&workdir)
        .output()
        .expect("git clone");
    assert!(out.status.success(), "git clone failed");

    // Configure author.
    for (k, v) in [("user.name", "Upstream User"), ("user.email", "up@test.com")] {
        Command::new("git")
            .args(["config", k, v])
            .current_dir(&workdir)
            .output()
            .unwrap();
    }

    seed_fn(&workdir);

    format!("file://{}", upstream.display())
}

/// Seed: single commit on `main` with README.md.
fn seed_single_branch(workdir: &std::path::Path) {
    std::fs::write(workdir.join("README.md"), "# upstream\n").unwrap();
    git_add_commit_push(workdir, "Initial commit", &["README.md"], "main");
}

/// Seed: two branches (`main` and `feature`) each with a commit.
fn seed_two_branches(workdir: &std::path::Path) {
    std::fs::write(workdir.join("README.md"), "# upstream\n").unwrap();
    git_add_commit_push(workdir, "Initial commit", &["README.md"], "main");

    // Create `feature` branch from main.
    run_git(workdir, &["checkout", "-b", "feature"]);
    std::fs::write(workdir.join("feature.txt"), "feature work\n").unwrap();
    git_add_commit_push(workdir, "Add feature", &["feature.txt"], "feature");
}

fn git_add_commit_push(workdir: &std::path::Path, msg: &str, files: &[&str], branch: &str) {
    for f in files {
        run_git(workdir, &["add", f]);
    }
    let _ = run_git(workdir, &["branch", "-M", branch]);
    let out = Command::new("git")
        .args(["commit", "-m", msg])
        .current_dir(workdir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git commit failed: {}", String::from_utf8_lossy(&out.stderr));
    let out = Command::new("git")
        .args(["push", "origin", branch])
        .current_dir(workdir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git push failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn run_git(workdir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(workdir)
        .output()
        .unwrap()
}

/// Push an additional commit to an existing bare repo's branch.
fn push_extra_commit(
    parent_dir: &std::path::Path,
    workdir_name: &str,
    branch: &str,
    filename: &str,
    content: &str,
    msg: &str,
) {
    let workdir = parent_dir.join(workdir_name);
    run_git(&workdir, &["checkout", branch]);
    std::fs::write(workdir.join(filename), content).unwrap();
    git_add_commit_push(&workdir, msg, &[filename], branch);
}

/// Init a kiki workspace and add a remote. Returns the repo path.
fn init_with_remote(
    test_env: &TestEnvironment,
    repo_name: &str,
    remote_name: &str,
    remote_url: &str,
) -> std::path::PathBuf {
    let (_, stderr) =
        test_env.jj_cmd_ok(test_env.env_root(), &["kk", "init", "", repo_name]);
    assert!(stderr.contains("Initialized repo"), "init: {stderr}");
    let repo_path = test_env.env_root().join(repo_name);
    test_env.jj_cmd_ok(
        &repo_path,
        &["kk", "git", "remote", "add", remote_name, remote_url],
    );
    repo_path
}

// ---------------------------------------------------------------------------
// Tests: fetch
// ---------------------------------------------------------------------------

/// Full round-trip: init → remote add → fetch → bookmarks → push → re-fetch.
#[test]
fn test_git_fetch_and_push_local() {
    let test_env = TestEnvironment::default();
    let url = create_bare_repo(test_env.env_root(), "upstream", seed_single_branch);
    let repo = init_with_remote(&test_env, "repo", "origin", &url);

    // List remotes.
    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["kk", "git", "remote", "list"]);
    assert!(stdout.contains("origin"), "remote list: {stdout}");

    // Fetch.
    let (stdout, _) =
        test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "origin"]);
    assert!(stdout.contains("origin/main"), "fetch: {stdout}");

    // Verify bookmarks.
    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["bookmark", "list", "--all"]);
    assert!(stdout.contains("main"), "bookmarks: {stdout}");
    assert!(stdout.contains("@origin"), "bookmarks: {stdout}");

    // Verify tree content is readable.
    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["file", "list", "-r", "main"]);
    assert!(stdout.contains("README.md"), "file list: {stdout}");

    // Commit on top of fetched main, push back.
    test_env.jj_cmd_ok(&repo, &["new", "main"]);
    test_env.jj_cmd_ok(&repo, &["describe", "-m", "kiki commit"]);
    test_env.jj_cmd_ok(&repo, &["new"]);
    test_env.jj_cmd_ok(&repo, &["bookmark", "set", "main", "-r", "@-"]);
    let (_, stderr) = test_env.jj_cmd_ok(
        &repo,
        &["kk", "git", "push", "--remote", "origin", "--bookmark", "main"],
    );
    assert!(stderr.contains("Done"), "push: {stderr}");

    // Re-fetch: should succeed (idempotent — pushed commit is now on remote).
    let (stdout, stderr) =
        test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "origin"]);
    // The main ref still points to the same commit we just pushed.
    assert!(stdout.contains("origin/main"), "re-fetch: {stdout}");
    assert!(stderr.contains("Fetching"), "re-fetch: {stderr}");
}

/// Fetch from an empty bare repo (no branches at all).
#[test]
fn test_git_fetch_empty_repo() {
    let test_env = TestEnvironment::default();
    // Seed function does nothing → bare repo has zero refs.
    let url = create_bare_repo(test_env.env_root(), "empty", |_workdir| {});
    let repo = init_with_remote(&test_env, "repo", "origin", &url);

    let (_, stderr) =
        test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "origin"]);
    assert!(
        stderr.contains("Nothing new"),
        "expected 'Nothing new' for empty repo, stderr: {stderr}"
    );

    // No bookmarks should exist.
    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["bookmark", "list", "--all"]);
    assert!(
        !stdout.contains("main"),
        "no bookmarks expected, got: {stdout}"
    );
}

/// Fetch from a repo with two branches — both should appear.
#[test]
fn test_git_fetch_multiple_bookmarks() {
    let test_env = TestEnvironment::default();
    let url = create_bare_repo(test_env.env_root(), "multi", seed_two_branches);
    let repo = init_with_remote(&test_env, "repo", "origin", &url);

    let (stdout, _) =
        test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "origin"]);
    assert!(stdout.contains("origin/main"), "fetch: {stdout}");
    assert!(stdout.contains("origin/feature"), "fetch: {stdout}");

    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["bookmark", "list", "--all"]);
    assert!(stdout.contains("main"), "bookmarks: {stdout}");
    assert!(stdout.contains("feature"), "bookmarks: {stdout}");

    // Both trees should be readable.
    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["file", "list", "-r", "main"]);
    assert!(stdout.contains("README.md"), "main files: {stdout}");
    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["file", "list", "-r", "feature"]);
    assert!(stdout.contains("feature.txt"), "feature files: {stdout}");
}

/// Fetch when the local bookmark has diverged from remote — local
/// should NOT be clobbered.
#[test]
fn test_git_fetch_diverged_local_bookmark() {
    let test_env = TestEnvironment::default();
    let url = create_bare_repo(test_env.env_root(), "upstream", seed_single_branch);
    let repo = init_with_remote(&test_env, "repo", "origin", &url);

    // First fetch — sets local `main` to the upstream commit.
    test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "origin"]);

    // Advance local `main` with a local-only commit.
    test_env.jj_cmd_ok(&repo, &["new", "main"]);
    test_env.jj_cmd_ok(&repo, &["describe", "-m", "local-only commit"]);
    test_env.jj_cmd_ok(&repo, &["new"]);
    test_env.jj_cmd_ok(&repo, &["bookmark", "set", "main", "-r", "@-"]);

    // Record what local main points to.
    let (log_before, _) = test_env.jj_cmd_ok(&repo, &["log", "-r", "main", "--no-graph", "-T", "commit_id.short(12)"]);
    let local_main_before = log_before.trim().to_string();

    // Meanwhile, push a new commit to the upstream (simulating another
    // collaborator advancing remote main).
    push_extra_commit(
        test_env.env_root(),
        "upstream-workdir",
        "main",
        "other.txt",
        "from collaborator\n",
        "Collaborator commit",
    );

    // Second fetch — remote main advanced, but local main diverged.
    let (stdout, _) =
        test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "origin"]);
    assert!(stdout.contains("origin/main"), "fetch: {stdout}");

    // Local main should be unchanged (diverged — not fast-forwarded).
    let (log_after, _) = test_env.jj_cmd_ok(&repo, &["log", "-r", "main", "--no-graph", "-T", "commit_id.short(12)"]);
    let local_main_after = log_after.trim().to_string();
    assert_eq!(
        local_main_before, local_main_after,
        "local main should not be clobbered on divergence"
    );

    // Remote-tracking bookmark should have advanced, showing divergence.
    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["bookmark", "list", "--all"]);
    // jj shows "ahead by N commits, behind by N commits" when local
    // and remote have diverged.
    assert!(
        stdout.contains("ahead by") && stdout.contains("behind by"),
        "main should show ahead/behind divergence, got: {stdout}"
    );
}

/// Fetch when upstream advances main — local should be fast-forwarded.
#[test]
fn test_git_fetch_fast_forward_local_bookmark() {
    let test_env = TestEnvironment::default();
    let url = create_bare_repo(test_env.env_root(), "upstream", seed_single_branch);
    let repo = init_with_remote(&test_env, "repo", "origin", &url);

    // First fetch.
    test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "origin"]);

    // Record local main.
    let (log_before, _) = test_env.jj_cmd_ok(&repo, &["log", "-r", "main", "--no-graph", "-T", "commit_id.short(12)"]);
    let local_main_before = log_before.trim().to_string();

    // Push a new commit upstream (fast-forward scenario: local main
    // hasn't moved, so it's strictly behind remote).
    push_extra_commit(
        test_env.env_root(),
        "upstream-workdir",
        "main",
        "new.txt",
        "new content\n",
        "Second upstream commit",
    );

    // Second fetch.
    let (stdout, _) =
        test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "origin"]);
    assert!(stdout.contains("origin/main"), "fetch: {stdout}");

    // Local main should have been fast-forwarded.
    let (log_after, _) = test_env.jj_cmd_ok(&repo, &["log", "-r", "main", "--no-graph", "-T", "commit_id.short(12)"]);
    let local_main_after = log_after.trim().to_string();
    assert_ne!(
        local_main_before, local_main_after,
        "local main should have advanced (fast-forward)"
    );

    // Verify new file is visible.
    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["file", "list", "-r", "main"]);
    assert!(stdout.contains("new.txt"), "files: {stdout}");
}

// ---------------------------------------------------------------------------
// Tests: push
// ---------------------------------------------------------------------------

/// Push --all with multiple local bookmarks.
#[test]
fn test_git_push_all_multiple_bookmarks() {
    let test_env = TestEnvironment::default();
    let url = create_bare_repo(test_env.env_root(), "upstream", seed_single_branch);
    let repo = init_with_remote(&test_env, "repo", "origin", &url);

    // Fetch to get main.
    test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "origin"]);

    // Create a second bookmark `dev` on a new commit.
    test_env.jj_cmd_ok(&repo, &["new", "main"]);
    test_env.jj_cmd_ok(&repo, &["describe", "-m", "dev work"]);
    test_env.jj_cmd_ok(&repo, &["bookmark", "create", "dev", "-r", "@"]);
    test_env.jj_cmd_ok(&repo, &["new"]);

    // Push --all.
    let (_, stderr) =
        test_env.jj_cmd_ok(&repo, &["kk", "git", "push", "--remote", "origin", "--all"]);
    // Should mention both bookmarks.
    assert!(stderr.contains("main"), "push --all stderr: {stderr}");
    assert!(stderr.contains("dev"), "push --all stderr: {stderr}");
    assert!(stderr.contains("Done"), "push --all stderr: {stderr}");
}

/// Push with no --bookmark and no --all should fail with a clear error.
#[test]
fn test_git_push_no_bookmark_arg() {
    let test_env = TestEnvironment::default();
    let url = create_bare_repo(test_env.env_root(), "upstream", seed_single_branch);
    let repo = init_with_remote(&test_env, "repo", "origin", &url);

    test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "origin"]);

    let stderr = test_env.jj_cmd_cli_error(
        &repo,
        &["kk", "git", "push", "--remote", "origin"],
    );
    assert!(
        stderr.contains("no bookmarks specified"),
        "expected 'no bookmarks specified' error, got: {stderr}"
    );
}

/// Push a non-fast-forward should fail with git's rejection message.
#[test]
fn test_git_push_non_fast_forward() {
    let test_env = TestEnvironment::default();
    let url = create_bare_repo(test_env.env_root(), "upstream", seed_single_branch);
    let repo = init_with_remote(&test_env, "repo", "origin", &url);

    // Fetch to get main.
    test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "origin"]);

    // Push a new commit upstream so remote is ahead.
    push_extra_commit(
        test_env.env_root(),
        "upstream-workdir",
        "main",
        "ahead.txt",
        "remote is ahead\n",
        "Remote-only commit",
    );

    // Create a local commit that doesn't descend from the new upstream
    // commit (divergent). Set main to it.
    test_env.jj_cmd_ok(&repo, &["new", "main"]);
    test_env.jj_cmd_ok(&repo, &["describe", "-m", "local divergent commit"]);
    test_env.jj_cmd_ok(&repo, &["new"]);
    test_env.jj_cmd_ok(&repo, &["bookmark", "set", "main", "-r", "@-"]);

    // Push should fail (non-fast-forward).
    let stderr = test_env.jj_cmd_internal_error(
        &repo,
        &["kk", "git", "push", "--remote", "origin", "--bookmark", "main"],
    );
    assert!(
        stderr.contains("rejected") || stderr.contains("non-fast-forward"),
        "expected rejection, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Tests: remotes
// ---------------------------------------------------------------------------

/// Multiple remotes: add two, list both, fetch from each independently.
#[test]
fn test_git_multiple_remotes() {
    let test_env = TestEnvironment::default();
    let url_a = create_bare_repo(test_env.env_root(), "remote-a", seed_single_branch);
    let url_b = create_bare_repo(test_env.env_root(), "remote-b", |workdir| {
        std::fs::write(workdir.join("B.txt"), "from remote B\n").unwrap();
        git_add_commit_push(workdir, "B initial", &["B.txt"], "main");
    });

    let (_, stderr) =
        test_env.jj_cmd_ok(test_env.env_root(), &["kk", "init", "", "repo"]);
    assert!(stderr.contains("Initialized repo"));
    let repo = test_env.env_root().join("repo");
    test_env.jj_cmd_ok(&repo, &["kk", "git", "remote", "add", "alpha", &url_a]);
    test_env.jj_cmd_ok(&repo, &["kk", "git", "remote", "add", "beta", &url_b]);

    // List should show both.
    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["kk", "git", "remote", "list"]);
    assert!(stdout.contains("alpha"), "list: {stdout}");
    assert!(stdout.contains("beta"), "list: {stdout}");

    // Fetch from alpha.
    let (stdout, _) =
        test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "alpha"]);
    assert!(stdout.contains("alpha/main"), "fetch alpha: {stdout}");

    // Fetch from beta.
    let (stdout, _) =
        test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "beta"]);
    assert!(stdout.contains("beta/main"), "fetch beta: {stdout}");

    // Both remote-tracking refs should be visible.
    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["bookmark", "list", "--all"]);
    assert!(stdout.contains("@alpha"), "bookmarks: {stdout}");
    assert!(stdout.contains("@beta"), "bookmarks: {stdout}");
}

/// Adding a remote with a duplicate name should fail.
#[test]
fn test_git_remote_add_duplicate() {
    let test_env = TestEnvironment::default();
    let url = create_bare_repo(test_env.env_root(), "upstream", seed_single_branch);
    let repo = init_with_remote(&test_env, "repo", "origin", &url);

    // Adding the same remote name again should error.
    let stderr = test_env.jj_cmd_internal_error(
        &repo,
        &["kk", "git", "remote", "add", "origin", "file:///other"],
    );
    assert!(
        stderr.contains("already exists") || stderr.contains("remote add"),
        "expected duplicate error, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Tests: `kiki git` dispatch hook (no `kk` prefix)
// ---------------------------------------------------------------------------

/// Full round-trip via `kiki git` (not `kiki kk git`): verifies the dispatch
/// hook intercepts on kiki-backend repos and routes through the daemon.
#[test]
fn test_git_dispatch_hook_round_trip() {
    let test_env = TestEnvironment::default();
    let url = create_bare_repo(test_env.env_root(), "upstream", seed_single_branch);

    // Init via `kk` (required — init is not on the git path).
    let (_, stderr) =
        test_env.jj_cmd_ok(test_env.env_root(), &["kk", "init", "", "repo"]);
    assert!(stderr.contains("Initialized repo"), "init: {stderr}");
    let repo = test_env.env_root().join("repo");

    // `git remote add` (dispatch hook path).
    test_env.jj_cmd_ok(&repo, &["git", "remote", "add", "origin", &url]);

    // `git remote list` (dispatch hook path).
    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["git", "remote", "list"]);
    assert!(stdout.contains("origin"), "remote list: {stdout}");

    // `git fetch` (dispatch hook path).
    let (stdout, _) =
        test_env.jj_cmd_ok(&repo, &["git", "fetch", "--remote", "origin"]);
    assert!(stdout.contains("origin/main"), "fetch: {stdout}");

    // Verify bookmarks.
    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["bookmark", "list", "--all"]);
    assert!(stdout.contains("main"), "bookmarks: {stdout}");
    assert!(stdout.contains("@origin"), "bookmarks: {stdout}");

    // Commit on top of fetched main, push back via `git push`.
    test_env.jj_cmd_ok(&repo, &["new", "main"]);
    test_env.jj_cmd_ok(&repo, &["describe", "-m", "dispatch hook commit"]);
    test_env.jj_cmd_ok(&repo, &["new"]);
    test_env.jj_cmd_ok(&repo, &["bookmark", "set", "main", "-r", "@-"]);
    let (_, stderr) = test_env.jj_cmd_ok(
        &repo,
        &["git", "push", "--remote", "origin", "--bookmark", "main"],
    );
    assert!(stderr.contains("Done"), "push: {stderr}");
}

/// `kiki git fetch` with no --remote should default to "origin".
#[test]
fn test_git_dispatch_hook_fetch_default_remote() {
    let test_env = TestEnvironment::default();
    let url = create_bare_repo(test_env.env_root(), "upstream", seed_single_branch);
    let repo = init_with_remote(&test_env, "repo", "origin", &url);

    // Fetch with no --remote flag — should default to "origin".
    let (stdout, _) = test_env.jj_cmd_ok(&repo, &["git", "fetch"]);
    assert!(stdout.contains("origin/main"), "fetch: {stdout}");
}

/// `kiki git push --all` via dispatch hook.
#[test]
fn test_git_dispatch_hook_push_all() {
    let test_env = TestEnvironment::default();
    let url = create_bare_repo(test_env.env_root(), "upstream", seed_single_branch);
    let repo = init_with_remote(&test_env, "repo", "origin", &url);

    test_env.jj_cmd_ok(&repo, &["git", "fetch"]);

    // Create a second bookmark.
    test_env.jj_cmd_ok(&repo, &["new", "main"]);
    test_env.jj_cmd_ok(&repo, &["describe", "-m", "dev work"]);
    test_env.jj_cmd_ok(&repo, &["bookmark", "create", "dev", "-r", "@"]);
    test_env.jj_cmd_ok(&repo, &["new"]);

    let (_, stderr) =
        test_env.jj_cmd_ok(&repo, &["git", "push", "--remote", "origin", "--all"]);
    assert!(stderr.contains("main"), "push --all stderr: {stderr}");
    assert!(stderr.contains("dev"), "push --all stderr: {stderr}");
    assert!(stderr.contains("Done"), "push --all stderr: {stderr}");
}

/// `kiki git push -b` short flag (matching jj's interface).
#[test]
fn test_git_dispatch_hook_push_short_flag() {
    let test_env = TestEnvironment::default();
    let url = create_bare_repo(test_env.env_root(), "upstream", seed_single_branch);
    let repo = init_with_remote(&test_env, "repo", "origin", &url);

    test_env.jj_cmd_ok(&repo, &["git", "fetch"]);

    test_env.jj_cmd_ok(&repo, &["new", "main"]);
    test_env.jj_cmd_ok(&repo, &["describe", "-m", "short flag commit"]);
    test_env.jj_cmd_ok(&repo, &["new"]);
    test_env.jj_cmd_ok(&repo, &["bookmark", "set", "main", "-r", "@-"]);

    // Use -b instead of --bookmark.
    let (_, stderr) = test_env.jj_cmd_ok(
        &repo,
        &["git", "push", "--remote", "origin", "-b", "main"],
    );
    assert!(stderr.contains("Done"), "push -b: {stderr}");
}

// ---------------------------------------------------------------------------
// Tests: Gitea (gated)
// ---------------------------------------------------------------------------

fn gitea_available() -> bool {
    std::env::var("KIKI_TEST_GITEA").is_ok()
}

fn gitea_url() -> String {
    std::env::var("KIKI_TEST_GITEA_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_string())
}

/// Create a fresh Gitea repo via the API. Returns the clone URL with
/// embedded credentials.
fn create_gitea_repo(name: &str) -> String {
    let base = gitea_url();
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(format!("{base}/api/v1/user/repos"))
        .basic_auth("testuser", Some("testpass123"))
        .json(&serde_json::json!({
            "name": name,
            "auto_init": true,
            "default_branch": "main"
        }))
        .send()
        .expect("Gitea API request failed");
    assert!(
        resp.status().is_success(),
        "Failed to create repo '{name}': {}",
        resp.text().unwrap_or_default()
    );
    format!("http://testuser:testpass123@localhost:3000/testuser/{name}.git")
}

fn delete_gitea_repo(name: &str) {
    let base = gitea_url();
    let client = reqwest::blocking::Client::new();
    let _ = client
        .delete(format!("{base}/api/v1/repos/testuser/{name}"))
        .basic_auth("testuser", Some("testpass123"))
        .send();
}

#[test]
fn test_git_fetch_and_push_round_trip() {
    if !gitea_available() {
        eprintln!("skipping: set KIKI_TEST_GITEA=1 to run git ops tests");
        return;
    }

    let repo_name = format!("kiki-test-{}", std::process::id());
    let clone_url = create_gitea_repo(&repo_name);
    let _cleanup = scopeguard::guard(repo_name.clone(), |name| {
        delete_gitea_repo(&name);
    });

    let test_env = TestEnvironment::default();
    let repo = init_with_remote(&test_env, "repo", "origin", &clone_url);

    let (stdout, _) =
        test_env.jj_cmd_ok(&repo, &["kk", "git", "fetch", "--remote", "origin"]);
    assert!(stdout.contains("origin/main"), "fetch: {stdout}");

    test_env.jj_cmd_ok(&repo, &["new", "main"]);
    test_env.jj_cmd_ok(&repo, &["describe", "-m", "test commit from kiki"]);
    test_env.jj_cmd_ok(&repo, &["new"]);
    test_env.jj_cmd_ok(&repo, &["bookmark", "set", "main", "-r", "@-"]);

    let (_, stderr) = test_env.jj_cmd_ok(
        &repo,
        &["kk", "git", "push", "--remote", "origin", "--bookmark", "main"],
    );
    assert!(stderr.contains("Done"), "push: {stderr}");
}
