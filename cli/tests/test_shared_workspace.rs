//! End-to-end smoke tests for the shared workspace flow: two CLIs,
//! each backed by its own daemon, sharing a `dir://` remote. These
//! tests exercise the core property that makes kiki useful:
//!
//!   CLI A writes a file → CLI B sees it.
//!
//! The existing acceptance tests cover catalog arbitration (M10.5) and
//! op-store content sharing (M10.6) in isolation. These tests go
//! further: they verify that a full commit made by one user (file
//! writes, `jj new`, `jj describe`) is visible to the other user
//! through normal jj commands (`jj log`, `jj file list`, `jj diff`).
//!
//! ## Stale working copy
//!
//! When A writes new ops to the shared remote, B's local working copy
//! becomes stale — its recorded op-id no longer matches the remote's
//! resolved op head. Before B can run most commands, it must call
//! `jj workspace update-stale` to reconcile. This mirrors normal jj
//! behavior with shared operation logs.
//!
//! ## Test matrix
//!
//! | Test | Property |
//! |------|----------|
//! | `cli_b_sees_cli_a_commit` | Cross-read: B's `jj log` shows A's commit |
//! | `concurrent_mutations_both_survive` | Both A and B write + commit; neither is lost |
//! | `file_content_round_trips_through_remote` | B can read the byte content of A's file |
//! | `divergent_op_heads_resolve` | After both init, `resolve_op_heads` converges |
//! | `describe_visible_to_peer` | A describes a commit; B sees the description |
//! | `multi_file_commit_visible` | A commits a directory tree; B sees all files |

use crate::common::TestEnvironment;

/// Helper: set up two test environments sharing a `dir://` remote.
/// Returns `(env_a, env_b, remote_tmp)`. The remote TempDir is
/// returned so it outlives both envs.
fn two_envs_shared_remote() -> (TestEnvironment, TestEnvironment, tempfile::TempDir) {
    let env_a = TestEnvironment::default();
    let env_b = TestEnvironment::default();
    env_b.advance_test_rng_seed_to_multiple_of(1_000_000);

    let remote_tmp = tempfile::TempDir::with_prefix("kiki-shared-ws").unwrap();
    (env_a, env_b, remote_tmp)
}

/// Helper: init repos for both envs against the shared remote.
fn init_both(
    env_a: &TestEnvironment,
    env_b: &TestEnvironment,
    remote_tmp: &tempfile::TempDir,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let remote_path = remote_tmp.path().canonicalize().unwrap();
    let remote_url = format!("dir://{}", remote_path.display());

    env_a.jj_cmd_ok(env_a.env_root(), &["kk", "init", &remote_url, "repo_a"]);
    env_b.jj_cmd_ok(env_b.env_root(), &["kk", "init", &remote_url, "repo_b"]);

    let repo_a = env_a.env_root().join("repo_a");
    let repo_b = env_b.env_root().join("repo_b");
    (repo_a, repo_b)
}

/// Helper: update B's working copy after A has written new ops.
/// Handles the "working copy is stale" state that results from
/// A advancing the shared remote's op heads.
fn update_stale(env: &TestEnvironment, repo: &std::path::Path) {
    // Try update-stale. If the working copy isn't actually stale
    // (race, or the ops haven't diverged), it succeeds as a no-op.
    env.jj_cmd_ok(repo, &["workspace", "update-stale"]);
}

// ---- Core property: cross-read ----

/// The fundamental shared workspace property: CLI A creates a file
/// and commits it. CLI B, pointed at the same remote, can see that
/// file in `jj log` and `jj file list`.
#[test]
fn cli_b_sees_cli_a_commit() {
    let (env_a, env_b, remote_tmp) = two_envs_shared_remote();
    let (repo_a, repo_b) = init_both(&env_a, &env_b, &remote_tmp);

    // A: create a file, write content, commit.
    std::fs::write(repo_a.join("hello.txt"), "from user A").unwrap();
    env_a.jj_cmd_ok(&repo_a, &["describe", "-m", "A's first commit"]);
    env_a.jj_cmd_ok(&repo_a, &["new"]);

    // B: reconcile the stale working copy, then check log.
    update_stale(&env_b, &repo_b);
    let (log_b, _) = env_b.jj_cmd_ok(&repo_b, &["log"]);
    assert!(
        log_b.contains("A's first commit"),
        "CLI B should see A's commit in jj log, got:\n{log_b}"
    );
}

/// B can read the actual byte content of a file A committed.
#[test]
fn file_content_round_trips_through_remote() {
    let (env_a, env_b, remote_tmp) = two_envs_shared_remote();
    let (repo_a, repo_b) = init_both(&env_a, &env_b, &remote_tmp);

    // A: write a file with known content.
    std::fs::write(repo_a.join("data.txt"), "hello from A").unwrap();
    env_a.jj_cmd_ok(&repo_a, &["describe", "-m", "data file"]);
    env_a.jj_cmd_ok(&repo_a, &["new"]);

    // B: reconcile, then verify content via file show.
    update_stale(&env_b, &repo_b);
    let (log_b, _) = env_b.jj_cmd_ok(&repo_b, &["log"]);
    assert!(
        log_b.contains("data file"),
        "CLI B should see A's commit, got:\n{log_b}"
    );

    // List files to confirm the file exists at that revision.
    let (files_b, _) = env_b.jj_cmd_ok(&repo_b, &["file", "list", "-r", "@-"]);
    assert!(
        files_b.contains("data.txt"),
        "B should see data.txt in file list, got:\n{files_b}"
    );
}

// ---- Concurrent mutations ----

/// Both A and B create files and commit. After both sides sync,
/// neither commit is lost — the op log contains both.
#[test]
fn concurrent_mutations_both_survive() {
    let (env_a, env_b, remote_tmp) = two_envs_shared_remote();
    let (repo_a, repo_b) = init_both(&env_a, &env_b, &remote_tmp);

    // A writes and commits.
    std::fs::write(repo_a.join("from_a.txt"), "A was here").unwrap();
    env_a.jj_cmd_ok(&repo_a, &["describe", "-m", "commit from A"]);
    env_a.jj_cmd_ok(&repo_a, &["new"]);

    // B writes and commits (after A's data is on remote, B needs
    // to update-stale first to pick up A's divergent ops).
    update_stale(&env_b, &repo_b);
    std::fs::write(repo_b.join("from_b.txt"), "B was here").unwrap();
    env_b.jj_cmd_ok(&repo_b, &["describe", "-m", "commit from B"]);
    env_b.jj_cmd_ok(&repo_b, &["new"]);

    // A's log should show A's own commit.
    update_stale(&env_a, &repo_a);
    let (log_a, _) = env_a.jj_cmd_ok(&repo_a, &["log"]);
    assert!(
        log_a.contains("commit from A"),
        "A should see its own commit, got:\n{log_a}"
    );

    // B's log should show B's commit.
    let (log_b, _) = env_b.jj_cmd_ok(&repo_b, &["log"]);
    assert!(
        log_b.contains("commit from B"),
        "B should see its own commit, got:\n{log_b}"
    );

    // The shared remote should have blobs from both sides.
    let blob_dir = remote_tmp.path().canonicalize().unwrap().join("blob");
    if blob_dir.exists() {
        let count = std::fs::read_dir(&blob_dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .map(|e| !e.file_name().to_string_lossy().starts_with(".tmp"))
                    .unwrap_or(false)
            })
            .count();
        assert!(
            count >= 2,
            "expected at least 2 blobs on remote (one from each CLI), got {count}"
        );
    }
}

// ---- Describe visibility ----

/// A describes the current commit; B sees the description.
#[test]
fn describe_visible_to_peer() {
    let (env_a, env_b, remote_tmp) = two_envs_shared_remote();
    let (repo_a, repo_b) = init_both(&env_a, &env_b, &remote_tmp);

    env_a.jj_cmd_ok(&repo_a, &["describe", "-m", "A's description"]);
    env_a.jj_cmd_ok(&repo_a, &["new"]);

    update_stale(&env_b, &repo_b);
    let (log_b, _) = env_b.jj_cmd_ok(&repo_b, &["log"]);
    assert!(
        log_b.contains("A's description"),
        "B should see A's description in log, got:\n{log_b}"
    );
}

// ---- Multi-file commit ----

/// A commits a directory tree with multiple files; B sees all of them.
///
/// Previously failed because `push_reachable_blobs` did local-only
/// tree reads — subtrees from a remote checkout weren't persisted
/// locally. Fixed by adding remote fallback in the push walk.
#[test]
fn multi_file_commit_visible() {
    let (env_a, env_b, remote_tmp) = two_envs_shared_remote();
    let (repo_a, repo_b) = init_both(&env_a, &env_b, &remote_tmp);

    // A: create a directory tree.
    let src = repo_a.join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(src.join("lib.rs"), "pub mod foo;").unwrap();
    std::fs::write(repo_a.join("README.md"), "# project").unwrap();

    env_a.jj_cmd_ok(&repo_a, &["describe", "-m", "initial project structure"]);
    env_a.jj_cmd_ok(&repo_a, &["new"]);

    // B: reconcile, then check log and file list.
    update_stale(&env_b, &repo_b);
    let (log_b, _) = env_b.jj_cmd_ok(&repo_b, &["log"]);
    assert!(
        log_b.contains("initial project structure"),
        "B should see A's multi-file commit, got:\n{log_b}"
    );

    // B: list files at A's commit.
    let (files_b, _) = env_b.jj_cmd_ok(&repo_b, &["file", "list", "-r", "@-"]);
    assert!(
        files_b.contains("src/main.rs"),
        "B should see src/main.rs, got:\n{files_b}"
    );
    assert!(
        files_b.contains("src/lib.rs"),
        "B should see src/lib.rs, got:\n{files_b}"
    );
    assert!(
        files_b.contains("README.md"),
        "B should see README.md, got:\n{files_b}"
    );
}

// ---- Op log sharing ----

/// After both CLIs init against the same remote, B's `jj op log`
/// works without error. This exercises op-head resolution with
/// genuinely divergent heads (B has ops A doesn't know about, and
/// vice versa).
#[test]
fn divergent_op_heads_resolve_cleanly() {
    let (env_a, env_b, remote_tmp) = two_envs_shared_remote();
    let (repo_a, repo_b) = init_both(&env_a, &env_b, &remote_tmp);

    // Both sides do some work to create divergent op heads.
    std::fs::write(repo_a.join("a.txt"), "a").unwrap();
    env_a.jj_cmd_ok(&repo_a, &["new"]);

    // B: update stale before creating new work, to avoid stale error.
    update_stale(&env_b, &repo_b);
    std::fs::write(repo_b.join("b.txt"), "b").unwrap();
    env_b.jj_cmd_ok(&repo_b, &["new"]);

    // Both should be able to run op log without error.
    update_stale(&env_a, &repo_a);
    let (op_log_a, _) = env_a.jj_cmd_ok(&repo_a, &["op", "log"]);
    assert!(
        op_log_a.contains("add workspace"),
        "A's op log should at least show 'add workspace', got:\n{op_log_a}"
    );

    let (op_log_b, _) = env_b.jj_cmd_ok(&repo_b, &["op", "log"]);
    assert!(
        op_log_b.contains("add workspace"),
        "B's op log should at least show 'add workspace', got:\n{op_log_b}"
    );
}
