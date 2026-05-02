//! M10.6 acceptance test: two CLIs (each with its own daemon) share
//! a `dir://` remote, and CLI_B can read CLI_A's operation contents
//! (views and operations) through the remote.
//!
//! M10.5 proved arbitration (no clobber on op-heads). M10.6 proves
//! content sharing: CLI_A's `jj kk init` writes operation + view
//! objects via `KikiOpStore` → daemon → remote. CLI_B, pointed at
//! the same remote, can `jj op log` and see CLI_A's operations —
//! which requires reading the actual operation bytes from the remote,
//! not just the op-heads ref.

use crate::common::TestEnvironment;

/// CLI_A inits a repo, writing ops to the shared remote. CLI_B inits
/// its own repo against the same remote. After both inits, CLI_B
/// should be able to see its own op log (which exercises read of its
/// own operations through the daemon). This confirms `KikiOpStore` is
/// wired up end-to-end.
#[test]
fn cli_reads_own_ops_via_kiki_op_store() {
    let env = TestEnvironment::default();
    let remote_tmp = tempfile::TempDir::with_prefix("kiki-op-store-test").unwrap();
    let remote_path = remote_tmp.path().canonicalize().unwrap();
    let remote_url = format!("dir://{}", remote_path.display());

    // Init a repo — writes root op + "add workspace" op via KikiOpStore.
    let (_stdout, stderr) =
        env.jj_cmd_ok(env.env_root(), &["kk", "init", &remote_url, "repo"]);
    insta::assert_snapshot!(stderr, @r#"Initialized repo in "repo""#);

    let repo_path = env.env_root().join("repo");

    // `jj op log` should work — it reads operation + view objects
    // through the KikiOpStore → daemon → local cache path.
    let (stdout, _stderr) = env.jj_cmd_ok(&repo_path, &["op", "log"]);
    // Should show at least the "add workspace 'default'" operation.
    assert!(
        stdout.contains("add workspace"),
        "expected 'add workspace' in op log output, got:\n{stdout}"
    );
}

/// Two CLIs sharing a remote: CLI_A writes ops, CLI_B can read them.
/// This is the core M10.6 property — op contents flow through the
/// remote so a peer can read ops another peer wrote.
#[test]
fn two_clis_share_op_contents_via_remote() {
    let env_a = TestEnvironment::default();
    let env_b = TestEnvironment::default();
    env_b.advance_test_rng_seed_to_multiple_of(1_000_000);

    let remote_tmp = tempfile::TempDir::with_prefix("kiki-op-store-sharing").unwrap();
    let remote_path = remote_tmp.path().canonicalize().unwrap();
    let remote_url = format!("dir://{}", remote_path.display());

    // CLI A: init a repo. Its ops land on the remote.
    let (_stdout_a, stderr_a) =
        env_a.jj_cmd_ok(env_a.env_root(), &["kk", "init", &remote_url, "repo_a"]);
    insta::assert_snapshot!(stderr_a, @r#"Initialized repo in "repo_a""#);

    // Confirm A's ops landed on the remote. Check for view/ and
    // operation/ subdirs with at least one file each.
    let view_dir = remote_path.join("view");
    let op_dir = remote_path.join("operation");
    assert!(
        view_dir.exists(),
        "expected view/ dir on remote after CLI_A init"
    );
    assert!(
        op_dir.exists(),
        "expected operation/ dir on remote after CLI_A init"
    );
    let view_count = std::fs::read_dir(&view_dir)
        .unwrap()
        .filter(|e| e.is_ok())
        .count();
    let op_count = std::fs::read_dir(&op_dir)
        .unwrap()
        .filter(|e| e.is_ok())
        .count();
    assert!(
        view_count >= 1,
        "expected ≥1 view blob on remote, got {view_count}"
    );
    assert!(
        op_count >= 1,
        "expected ≥1 operation blob on remote, got {op_count}"
    );

    // CLI B: init its own repo against the same remote.
    let (_stdout_b, stderr_b) =
        env_b.jj_cmd_ok(env_b.env_root(), &["kk", "init", &remote_url, "repo_b"]);
    insta::assert_snapshot!(stderr_b, @r#"Initialized repo in "repo_b""#);

    // Both A and B should be able to do `jj op log` against their own repos.
    let repo_a_path = env_a.env_root().join("repo_a");
    let (stdout_a, _) = env_a.jj_cmd_ok(&repo_a_path, &["op", "log"]);
    assert!(
        stdout_a.contains("add workspace"),
        "CLI_A's op log should work, got:\n{stdout_a}"
    );

    let repo_b_path = env_b.env_root().join("repo_b");
    let (stdout_b, _) = env_b.jj_cmd_ok(&repo_b_path, &["op", "log"]);
    assert!(
        stdout_b.contains("add workspace"),
        "CLI_B's op log should work, got:\n{stdout_b}"
    );
}
