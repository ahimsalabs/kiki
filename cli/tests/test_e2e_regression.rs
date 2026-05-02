//! Regression tests for the three bugs found during the first e2e
//! smoke test (2026-05-01):
//!
//! 1. zstd compression mismatch in KikiBackend (read_file/write_file)
//! 2. Missing ancestor-dirty propagation after snapshot
//! 3. Dirty nodes clobbered by tree materialization
//!
//! These tests exercise the full CLI → daemon → FUSE → jj pipeline.
//! The `common::TestEnvironment` harness starts a real daemon with
//! NFS mounts, so files created via `std::fs::write` go through the
//! VFS layer and are visible to `jj status` / `jj diff`.

use crate::common::TestEnvironment;

/// Bug 1+3 combined: create a file via FUSE, `jj new`, modify the
/// file in-place, and verify `jj diff` shows the modification.
///
/// This exercises the raw-bytes read/write path (Bug 1 — no zstd)
/// and ensures that writing to an existing tree child after check-out
/// survives materialization (Bug 3).
#[test]
fn test_modify_file_after_new() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_ok(test_env.env_root(), &["kk", "init", "", "repo"]);
    let repo_path = test_env.env_root().join("repo");

    // Create a file via FUSE.
    std::fs::write(repo_path.join("hello.txt"), "original content").unwrap();

    // Commit the current state.
    test_env.jj_cmd_ok(&repo_path, &["new"]);

    // File should still be readable after check-out.
    let content = std::fs::read_to_string(repo_path.join("hello.txt")).unwrap();
    assert_eq!(content, "original content");

    // Modify in-place. This goes through FUSE write with O_TRUNC.
    std::fs::write(repo_path.join("hello.txt"), "modified content").unwrap();

    // jj diff should show the modification.
    let stdout = test_env.jj_cmd_success(&repo_path, &["diff", "--summary"]);
    insta::assert_snapshot!(stdout, @r"
    M hello.txt
    ");

    // jj diff (full) should show the content change.
    let stdout = test_env.jj_cmd_success(&repo_path, &["diff", "--git"]);
    assert!(
        stdout.contains("-original content") && stdout.contains("+modified content"),
        "git diff should show old and new content, got:\n{stdout}"
    );
}

/// Bug 2: delete a file after `jj new` and verify `jj diff` shows
/// the deletion.
///
/// Without ancestor-dirty propagation, the snapshot after the removal
/// would return the same root tree id as the parent commit, making
/// the deletion invisible.
#[test]
fn test_delete_file_after_new() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_ok(test_env.env_root(), &["kk", "init", "", "repo"]);
    let repo_path = test_env.env_root().join("repo");

    // Create two files so the directory isn't empty after deletion.
    std::fs::write(repo_path.join("keep.txt"), "keeper").unwrap();
    std::fs::write(repo_path.join("remove.txt"), "will be removed").unwrap();

    test_env.jj_cmd_ok(&repo_path, &["new"]);

    // Delete one file.
    std::fs::remove_file(repo_path.join("remove.txt")).unwrap();

    // jj diff should show the deletion.
    let stdout = test_env.jj_cmd_success(&repo_path, &["diff", "--summary"]);
    insta::assert_snapshot!(stdout, @r"
    D remove.txt
    ");

    // The remaining file should still be listed.
    let stdout = test_env.jj_cmd_success(&repo_path, &["file", "list"]);
    insta::assert_snapshot!(stdout, @r"
    keep.txt
    ");
}

/// Bug 2 (deep variant): modify a file inside a nested directory
/// after `jj new`. Exercises ancestor-dirty propagation across
/// multiple levels (root → dir → subdir → file).
#[test]
fn test_modify_deep_file_after_new() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_ok(test_env.env_root(), &["kk", "init", "", "repo"]);
    let repo_path = test_env.env_root().join("repo");

    // Create nested structure via FUSE.
    let deep = repo_path.join("src").join("pkg");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("main.go"), "package main\n").unwrap();

    test_env.jj_cmd_ok(&repo_path, &["new"]);

    // Modify the deeply nested file.
    std::fs::write(deep.join("main.go"), "package main\n\nfunc main() {}\n").unwrap();

    let stdout = test_env.jj_cmd_success(&repo_path, &["diff", "--summary"]);
    insta::assert_snapshot!(stdout, @r"
    M src/pkg/main.go
    ");
}

/// Editor atomic-save pattern: write(tmp) → rename(tmp, real).
///
/// Many editors save files by writing to a temporary file and then
/// atomically renaming it over the original. This exercises that
/// pattern through the FUSE layer.
#[test]
fn test_editor_atomic_save() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_ok(test_env.env_root(), &["kk", "init", "", "repo"]);
    let repo_path = test_env.env_root().join("repo");

    // Create original file.
    std::fs::write(repo_path.join("config.toml"), "[original]\nkey = 1\n").unwrap();

    test_env.jj_cmd_ok(&repo_path, &["new"]);

    // Simulate editor save: write to tmp, rename over original.
    let tmp_path = repo_path.join("config.toml.tmp");
    std::fs::write(&tmp_path, "[modified]\nkey = 2\n").unwrap();
    std::fs::rename(&tmp_path, repo_path.join("config.toml")).unwrap();

    // The tmp file should be gone.
    assert!(!tmp_path.exists());

    // jj diff should show only the modification, not an add+delete.
    let stdout = test_env.jj_cmd_success(&repo_path, &["diff", "--summary"]);
    insta::assert_snapshot!(stdout, @r"
    M config.toml
    ");

    // Verify content is the new version.
    let content = std::fs::read_to_string(repo_path.join("config.toml")).unwrap();
    assert_eq!(content, "[modified]\nkey = 2\n");
}

/// Cross-directory rename followed by modification: rename a file
/// from one directory to another, then modify it. The modification
/// must be visible in `jj diff`.
///
/// Regression test for the parent-pointer bug: `rename` must update
/// the child's parent field so that subsequent writes dirty the
/// correct ancestor chain.
#[test]
fn test_cross_dir_rename_then_modify() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_ok(test_env.env_root(), &["kk", "init", "", "repo"]);
    let repo_path = test_env.env_root().join("repo");

    // Create /src/main.rs and /dst/.
    std::fs::create_dir(repo_path.join("src")).unwrap();
    std::fs::create_dir(repo_path.join("dst")).unwrap();
    std::fs::write(repo_path.join("src").join("main.rs"), "v1").unwrap();

    test_env.jj_cmd_ok(&repo_path, &["new"]);

    // Move src/main.rs → dst/main.rs.
    std::fs::rename(
        repo_path.join("src").join("main.rs"),
        repo_path.join("dst").join("main.rs"),
    )
    .unwrap();

    // Modify the renamed file.
    std::fs::write(repo_path.join("dst").join("main.rs"), "v2").unwrap();

    let stdout = test_env.jj_cmd_success(&repo_path, &["diff", "--summary"]);
    // Should show the file was deleted from src and added/modified in dst.
    assert!(
        stdout.contains("dst/main.rs"),
        "diff should mention dst/main.rs, got:\n{stdout}"
    );
}

/// `jj restore` reverts the working copy to the parent commit's tree.
/// Exercises the snapshot + check_out path end-to-end.
#[test]
fn test_jj_restore() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_ok(test_env.env_root(), &["kk", "init", "", "repo"]);
    let repo_path = test_env.env_root().join("repo");

    // Create a file and commit.
    std::fs::write(repo_path.join("file.txt"), "original").unwrap();
    test_env.jj_cmd_ok(&repo_path, &["new"]);

    // Modify the file.
    std::fs::write(repo_path.join("file.txt"), "modified").unwrap();

    test_env.jj_cmd_ok(&repo_path, &["restore"]);

    // After restore, the file should be back to original.
    let content = std::fs::read_to_string(repo_path.join("file.txt")).unwrap();
    assert_eq!(content, "original", "restore should revert the file");
}

/// `jj squash` combines the current commit into its parent.
/// Exercises tree merging through the backend.
#[test]
fn test_jj_squash() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_ok(test_env.env_root(), &["kk", "init", "", "repo"]);
    let repo_path = test_env.env_root().join("repo");

    // Create a file.
    std::fs::write(repo_path.join("a.txt"), "content a").unwrap();
    test_env.jj_cmd_ok(&repo_path, &["new"]);

    // Create another file in the new commit.
    std::fs::write(repo_path.join("b.txt"), "content b").unwrap();

    // Squash: merge current into parent.
    test_env.jj_cmd_ok(&repo_path, &["squash"]);

    // Both files should be in the parent commit now.
    let stdout = test_env.jj_cmd_success(&repo_path, &["file", "list", "-r", "@-"]);
    assert!(stdout.contains("a.txt"), "squashed commit should have a.txt");
    assert!(stdout.contains("b.txt"), "squashed commit should have b.txt");

    // Working copy should still have both files visible.
    assert!(repo_path.join("a.txt").exists());
    assert!(repo_path.join("b.txt").exists());
}

/// `jj abandon` drops the current commit and moves to the parent.
/// This exercises the working copy reset path.
#[test]
fn test_jj_abandon() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_ok(test_env.env_root(), &["kk", "init", "", "repo"]);
    let repo_path = test_env.env_root().join("repo");

    // Create a file and commit.
    std::fs::write(repo_path.join("keep.txt"), "kept").unwrap();
    test_env.jj_cmd_ok(&repo_path, &["new"]);

    // Create another file in the child commit.
    std::fs::write(repo_path.join("abandon.txt"), "abandoned").unwrap();
    test_env.jj_cmd_ok(&repo_path, &["new"]);

    // Abandon the middle commit (the parent of @).
    // This should not panic even if reset is a todo!().
    test_env.jj_cmd_ok(&repo_path, &["abandon", "@-"]);

    // The kept file should still be visible (it's in the grandparent).
    let content = std::fs::read_to_string(repo_path.join("keep.txt")).unwrap();
    assert_eq!(content, "kept");
}
