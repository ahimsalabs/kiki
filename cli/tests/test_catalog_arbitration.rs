//! M10.5 acceptance test: two CLIs (each with its own daemon)
//! share a `dir://` remote and serialize op-heads advances through
//! the catalog rather than silently clobbering.
//!
//! Each `jj yak init` issues two `update_op_heads` calls against the
//! catalog: first to register the root_operation_id, then again
//! during `init_working_copy` to advance from root_op → "add
//! workspace 'default'" op. With a shared remote, the second CLI's
//! create-only CAS conflicts with the first CLI's already-written
//! ref; the `YakOpHeadsStore` retry loop reads the actual value and
//! re-CASes against it, ending with a catalog that contains *both*
//! workspace ops as divergent heads. Divergence is fine — that's
//! what `resolve_op_heads` is for; the M10.5 property under test is
//! just "no clobber."
//!
//! Op-store contents (the actual operation bytes for A's
//! "add workspace" op) still live only in CLI_A's daemon's local
//! op_store on disk. Reading them from CLI_B is M10.6 territory; we
//! deliberately don't `jj log` after init here.

use std::path::Path;

use crate::common::TestEnvironment;

/// Decode the wire format `YakOpHeadsStore` writes:
/// `[u32 BE len][bytes]` repeated until end of buffer.
/// Returns the decoded op-id byte vectors.
fn decode_op_heads(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        assert!(buf.len() - i >= 4, "truncated length prefix");
        let len = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        assert!(buf.len() - i >= len, "truncated payload");
        if len > 0 {
            out.push(buf[i..i + len].to_vec());
        }
        i += len;
    }
    out
}

/// Read the `op_heads` ref from the shared `dir://` remote on
/// disk. `FsRemoteStore` writes refs at `<root>/refs/<name>`.
fn read_remote_op_heads(remote_dir: &Path) -> Vec<Vec<u8>> {
    let path = remote_dir.join("refs").join("op_heads");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "expected op_heads ref at {} to exist on shared remote: {e}",
            path.display()
        )
    });
    decode_op_heads(&bytes)
}

/// Two CLIs, each with their own daemon, init repos pointed at one
/// shared `dir://` remote. The expected end state on the remote is a
/// single `op_heads` ref containing both daemons' workspace op ids
/// (plus possibly the root_op_id from earlier writes) — proving the
/// CAS arbitration converged rather than one daemon silently
/// overwriting the other.
#[test]
fn two_clis_serialize_op_heads_via_shared_dir_remote() {
    let env_a = TestEnvironment::default();
    let env_b = TestEnvironment::default();

    // The TestEnvironment harness pins JJ_RANDOMNESS_SEED + JJ_TIMESTAMP
    // for reproducibility. Two separate envs starting at command_number
    // = 0 would produce *byte-identical* "add workspace 'default'" ops,
    // and identical content hashes to identical op-ids — which would
    // collapse to a single op-head in the catalog regardless of whether
    // the CAS retry loop worked correctly. Advance env_b's RNG to a
    // disjoint range so its workspace op hashes to a different id.
    env_b.advance_test_rng_seed_to_multiple_of(1_000_000);

    // Shared remote dir lives outside either env so neither's `Drop`
    // tears it down prematurely. Canonicalize so the daemon's
    // `working_copy_path` lookup matches across the two daemons.
    let remote_tmp = tempfile::TempDir::with_prefix("yak-shared-remote").unwrap();
    let remote_path = remote_tmp.path().canonicalize().unwrap();
    let remote_url = format!("dir://{}", remote_path.display());

    // CLI A goes first: empty catalog → catalog has {A's workspace op}.
    let (_stdout_a, stderr_a) =
        env_a.jj_cmd_ok(env_a.env_root(), &["yak", "init", &remote_url, "repo_a"]);
    insta::assert_snapshot!(stderr_a, @r#"Initialized repo in "repo_a""#);

    // CLI B comes second: catalog already has A's op-head. Each
    // update_op_heads in CLI_B's init runs a CAS — the create-only
    // first call must conflict and retry; the second call (advancing
    // from root_op_id) starts from the merged-heads state.
    //
    // The contract under test is *that init succeeds at all*. Any
    // bug in the retry loop or absent-vs-empty wire shape would
    // either spin past MAX_RETRIES, clobber A's head silently, or
    // surface a daemon-side error.
    let (_stdout_b, stderr_b) =
        env_b.jj_cmd_ok(env_b.env_root(), &["yak", "init", &remote_url, "repo_b"]);
    insta::assert_snapshot!(stderr_b, @r#"Initialized repo in "repo_b""#);

    // Inspect the shared remote directly. The format matches
    // `cli/src/op_heads_store.rs::encode`. We expect ≥ 2 entries —
    // the two workspace ops, possibly plus root_op_id depending on
    // when each CLI's tx.commit ran relative to A's two writes.
    let op_heads = read_remote_op_heads(&remote_path);
    assert!(
        op_heads.len() >= 2,
        "expected ≥ 2 op-heads in catalog after two CLIs init; \
         got {} — a single entry means one CLI silently clobbered \
         the other's op-head",
        op_heads.len()
    );

    // Each entry should be a valid jj-lib op-id (64 bytes for
    // blake2b). Anything other than 64 bytes here would mean we
    // serialized garbage, or the wire format drifted from the
    // YakOpHeadsStore encoder.
    for (i, id) in op_heads.iter().enumerate() {
        assert_eq!(
            id.len(),
            64,
            "op-head[{i}] has unexpected length {} (expected 64)",
            id.len()
        );
    }

    // All entries must be unique — no duplicates from a confused
    // retry-and-re-add path.
    let mut sorted = op_heads.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), op_heads.len(), "duplicate op-heads in catalog");
}

/// Single-CLI baseline: one daemon, one CLI, one shared `dir://`
/// remote. This isolates "the catalog write path works at all"
/// from the multi-daemon arbitration story above. If this one
/// fails the previous test's failure was probably a basic catalog
/// regression, not a CAS retry-loop bug.
#[test]
fn one_cli_writes_op_heads_to_shared_dir_remote() {
    let env = TestEnvironment::default();
    let remote_tmp = tempfile::TempDir::with_prefix("yak-shared-remote-single").unwrap();
    let remote_path = remote_tmp.path().canonicalize().unwrap();
    let remote_url = format!("dir://{}", remote_path.display());

    let (_stdout, stderr) =
        env.jj_cmd_ok(env.env_root(), &["yak", "init", &remote_url, "repo"]);
    insta::assert_snapshot!(stderr, @r#"Initialized repo in "repo""#);

    let op_heads = read_remote_op_heads(&remote_path);
    assert_eq!(
        op_heads.len(),
        1,
        "expected exactly one op-head from a single CLI init; got {op_heads:?}"
    );
    assert_eq!(op_heads[0].len(), 64, "op-head id must be 64-byte blake2b");
}
