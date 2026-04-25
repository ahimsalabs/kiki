use crate::common::TestEnvironment;

#[test]
fn test_init() {
    let test_env = TestEnvironment::default();
    let (stdout, stderr) =
        test_env.jj_cmd_ok(test_env.env_root(), &["yak", "init", "localhost", "repo"]);
    insta::assert_snapshot!(stdout, @"");
    insta::assert_snapshot!(stderr, @r#"Initialized repo in "repo""#);
    let repo_path = test_env.env_root().join("repo");

    let stdout = test_env.jj_cmd_success(&repo_path, &["log"]);
    insta::assert_snapshot!(stdout, @r"
    @  qpvuntsm test.user@example.com 2001-02-03 08:05:07 b4e46adb
    │  (empty) (no description set)
    ◆  zzzzzzzz root() 00000000
    ");
}

/// Round-trip the operation id through the daemon: `jj yak init` makes
/// `YakWorkingCopy::init` push the workspace op id via `SetCheckoutState`,
/// and subsequent commands fetch it back via `GetCheckoutState` whenever
/// they need the workspace's current operation. `jj op log` exercises that
/// fetch — the `current_operation` keyword resolves to whichever op id the
/// workspace reports, which for `YakWorkingCopy` is the daemon's cached
/// value. If the round-trip drops bytes (e.g. workspace_id<->op_id swapped
/// in the proto, hex/decode mismatch) the `@` marker either attaches to
/// the wrong op or no op at all, and this assertion fails.
#[test]
fn test_op_id_round_trip() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_ok(test_env.env_root(), &["yak", "init", "localhost", "repo"]);
    let repo_path = test_env.env_root().join("repo");

    let stdout = test_env.jj_cmd_success(
        &repo_path,
        &[
            "op",
            "log",
            "--no-graph",
            "-T",
            r#"if(current_operation, "@", " ") ++ " " ++ id.short() ++ " " ++ description.first_line() ++ "\n""#,
        ],
    );
    insta::assert_snapshot!(stdout, @r"
    @ e69ffce1f5bb add workspace 'default'
      000000000000
    ");
}

#[test]
fn test_multiple_init() {
    let test_env = TestEnvironment::default();
    let (stdout, stderr) =
        test_env.jj_cmd_ok(test_env.env_root(), &["yak", "init", "localhost", "repo1"]);
    insta::assert_snapshot!(stdout, @"");
    insta::assert_snapshot!(stderr, @r#"Initialized repo in "repo1""#);
    let repo1_path = test_env.env_root().join("repo1");

    let (stdout, stderr) =
        test_env.jj_cmd_ok(test_env.env_root(), &["yak", "init", "localhost", "repo2"]);
    insta::assert_snapshot!(stdout, @"");
    insta::assert_snapshot!(stderr, @r#"Initialized repo in "repo2""#);
    let repo2_path = test_env.env_root().join("repo2");

    let stdout = test_env.jj_cmd_success(&repo1_path, &["log"]);
    insta::assert_snapshot!(stdout, @r"
    @  qpvuntsm test.user@example.com 2001-02-03 08:05:07 b4e46adb
    │  (empty) (no description set)
    ◆  zzzzzzzz root() 00000000
    ");

    let stdout = test_env.jj_cmd_success(&repo2_path, &["log"]);
    insta::assert_snapshot!(stdout, @r"
    @  rlvkpnrz test.user@example.com 2001-02-03 08:05:08 029ed36b
    │  (empty) (no description set)
    ◆  zzzzzzzz root() 00000000
    ");

    let stdout = test_env.jj_cmd_success(&repo2_path, &["yak", "status"]);
    insta::assert_snapshot!(stdout, @r"
    $TEST_ENV/repo1 - localhost
    $TEST_ENV/repo2 - localhost
    ");
}

// `jj new` calls into `LockedYakWorkingCopy::check_out`, which is `todo!`
// until M5 (`cli/src/working_copy.rs`). Was green pre-M2 only because
// `LocalWorkingCopyFactory` was masking the gap. §6 of docs/PLAN.md will
// re-home this kind of coverage in `test_workingcopy.rs` once the WC path
// is real.
#[ignore = "needs M5: LockedYakWorkingCopy::check_out"]
#[test]
fn test_repos_are_independent() {
    let test_env = TestEnvironment::default();
    let (stdout, stderr) =
        test_env.jj_cmd_ok(test_env.env_root(), &["yak", "init", "localhost", "repo1"]);
    insta::assert_snapshot!(stdout, @"");
    insta::assert_snapshot!(stderr, @r#"Initialized repo in "repo1""#);
    let repo1_path = test_env.env_root().join("repo1");

    let (stdout, stderr) =
        test_env.jj_cmd_ok(test_env.env_root(), &["yak", "init", "localhost", "repo2"]);
    insta::assert_snapshot!(stdout, @"");
    insta::assert_snapshot!(stderr, @r#"Initialized repo in "repo2""#);
    let repo2_path = test_env.env_root().join("repo2");

    let stdout = test_env.jj_cmd_success(&repo1_path, &["log"]);
    insta::assert_snapshot!(stdout, @r"
    @  qpvuntsm test.user@example.com 2001-02-03 08:05:07 b4e46adb
    │  (empty) (no description set)
    ◆  zzzzzzzz root() 00000000
    ");

    let stdout = test_env.jj_cmd_success(&repo2_path, &["log"]);
    insta::assert_snapshot!(stdout, @r"
    @  rlvkpnrz test.user@example.com 2001-02-03 08:05:08 029ed36b
    │  (empty) (no description set)
    ◆  zzzzzzzz root() 00000000
    ");

    test_env.jj_cmd_ok(&repo1_path, &["new"]);
    let stdout = test_env.jj_cmd_success(&repo1_path, &["log"]);
    insta::assert_snapshot!(stdout, @r"
    @  mzvwutvl test.user@example.com 2001-02-03 08:05:11 bada728f
    │  (empty) (no description set)
    ○  qpvuntsm test.user@example.com 2001-02-03 08:05:07 b4e46adb
    │  (empty) (no description set)
    ◆  zzzzzzzz root() 00000000
    ");

    let stdout = test_env.jj_cmd_success(&repo2_path, &["yak", "status"]);
    insta::assert_snapshot!(stdout, @r"
    $TEST_ENV/repo1 - localhost
    $TEST_ENV/repo2 - localhost
    ");
}

// Needs the VFS write path (M6) to capture the on-disk write into a tree,
// plus `check_out` (M5) for `jj new`. Will move to `test_workingcopy.rs`
// per §6 of docs/PLAN.md once those land.
#[ignore = "needs M5+M6: VFS write path + check_out"]
#[test]
fn test_nested_tree_round_trips() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_ok(test_env.env_root(), &["yak", "init", "localhost", "repo"]);
    let repo_path = test_env.env_root().join("repo");
    let dir_path = repo_path.join("dir");
    std::fs::create_dir(&dir_path).unwrap();
    std::fs::write(dir_path.join("file"), "content").unwrap();

    test_env.jj_cmd_ok(&repo_path, &["new"]);
    let stdout = test_env.jj_cmd_success(&repo_path, &["file", "list", "-r", "@-"]);
    insta::assert_snapshot!(stdout, @r"
    dir/file
    ");
}

// Same dependency as `test_nested_tree_round_trips` — needs M5+M6.
#[ignore = "needs M5+M6: VFS write path + check_out"]
#[cfg(unix)]
#[test]
fn test_symlink_tree_round_trips() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_ok(test_env.env_root(), &["yak", "init", "localhost", "repo"]);
    let repo_path = test_env.env_root().join("repo");
    std::os::unix::fs::symlink("target", repo_path.join("link")).unwrap();

    test_env.jj_cmd_ok(&repo_path, &["new"]);
    let stdout = test_env.jj_cmd_success(&repo_path, &["file", "list", "-r", "@-"]);
    insta::assert_snapshot!(stdout, @r"
    link
    ");
}
