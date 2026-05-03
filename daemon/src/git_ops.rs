//! Git remote operations on the per-mount bare git repo.
//!
//! Post git-convergence, every mount's content store is a bare git repo
//! (managed by [`crate::git_store::GitContentStore`]). This module adds
//! git remote management and push/fetch by driving the `git` subprocess,
//! matching `jj git` behavior.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use gix::objs::Exists as _;

/// Add a named remote to the bare git repo.
pub fn remote_add(git_repo_path: &Path, name: &str, url: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["remote", "add", name, url])
        .current_dir(git_repo_path)
        .output()
        .context("spawning git remote add")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git remote add failed: {}", stderr.trim());
    }
    Ok(())
}

/// List remotes configured in the bare git repo. Returns `(name, url)` pairs.
pub fn remote_list(git_repo_path: &Path) -> Result<Vec<(String, String)>> {
    let output = Command::new("git")
        .args(["remote", "-v"])
        .current_dir(git_repo_path)
        .output()
        .context("spawning git remote -v")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git remote -v failed: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut seen = std::collections::HashSet::new();
    let mut remotes = Vec::new();
    for line in stdout.lines() {
        // Format: "origin\thttps://... (fetch)" or "origin\thttps://... (push)"
        let Some((name, rest)) = line.split_once('\t') else {
            continue;
        };
        if !seen.insert(name.to_owned()) {
            continue;
        }
        // Strip the trailing " (fetch)" or " (push)"
        let url = rest
            .rsplit_once(" (")
            .map(|(u, _)| u)
            .unwrap_or(rest);
        remotes.push((name.to_owned(), url.to_owned()));
    }
    Ok(remotes)
}

/// Push bookmarks to a git remote.
///
/// For each bookmark, sets `refs/heads/<name>` in the bare repo to the
/// given commit OID, then runs `git push <remote> <refspecs>`.
pub fn push(
    git_repo_path: &Path,
    remote: &str,
    bookmarks: &[(String, Vec<u8>)],
) -> Result<()> {
    if bookmarks.is_empty() {
        anyhow::bail!("nothing to push");
    }

    // Set local refs so `git push` has something to push.
    let repo = gix::open(git_repo_path).context("opening git repo for push")?;
    let mut refspecs = Vec::new();
    for (name, oid_bytes) in bookmarks {
        let oid = gix::ObjectId::try_from(oid_bytes.as_slice())
            .map_err(|_| anyhow!("invalid commit id for bookmark '{name}'"))?;
        // Verify the object exists in the ODB.
        if !repo.objects.exists(&oid) {
            anyhow::bail!(
                "commit {} for bookmark '{name}' not found in git ODB",
                oid
            );
        }
        let ref_name = format!("refs/heads/{name}");
        repo.reference(
            ref_name.clone(),
            oid,
            gix::refs::transaction::PreviousValue::Any,
            "kiki git push",
        )
        .with_context(|| format!("setting ref {ref_name}"))?;
        refspecs.push(format!("refs/heads/{name}:refs/heads/{name}"));
    }

    let mut cmd = Command::new("git");
    cmd.arg("push").arg(remote);
    for spec in &refspecs {
        cmd.arg(spec);
    }
    cmd.current_dir(git_repo_path);

    let output = cmd.output().context("spawning git push")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git push failed: {}", stderr.trim());
    }
    Ok(())
}

/// Fetch from a git remote, then read the updated remote-tracking refs.
///
/// Returns `(bookmark_name, commit_id)` pairs for each
/// `refs/remotes/<remote>/<name>` ref.
pub fn fetch(
    git_repo_path: &Path,
    remote: &str,
) -> Result<Vec<(String, Vec<u8>)>> {
    let output = Command::new("git")
        .args(["fetch", remote])
        .current_dir(git_repo_path)
        .output()
        .context("spawning git fetch")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git fetch failed: {}", stderr.trim());
    }

    // Read remote-tracking refs.
    let repo = gix::open(git_repo_path).context("opening git repo after fetch")?;
    let prefix = format!("refs/remotes/{remote}/");
    let mut bookmarks = Vec::new();
    for r in repo.references()?.prefixed(prefix.as_bytes())? {
        let r = r.map_err(|e| anyhow!("iterating refs: {e}"))?;
        let name = r.name().as_bstr();
        let short = match name.strip_prefix(prefix.as_bytes()) {
            Some(s) => s,
            None => continue,
        };
        if short == b"HEAD" {
            continue;
        }
        let oid = r
            .id()
            .object()
            .context("peeling ref to object")?
            .id;
        let short_str = String::from_utf8_lossy(short).into_owned();
        bookmarks.push((short_str, oid.as_bytes().to_vec()));
    }
    Ok(bookmarks)
}

/// Write HEAD as a detached head pointing at `commit_oid` in the bare git repo.
pub fn export_head(git_repo_path: &Path, commit_oid: &[u8]) -> Result<()> {
    let repo = gix::open(git_repo_path).context("opening git repo for export_head")?;
    let oid = gix::ObjectId::try_from(commit_oid)
        .map_err(|_| anyhow!("invalid commit id for export_head"))?;
    if !repo.objects.exists(&oid) {
        anyhow::bail!("commit {} not found in git ODB", oid);
    }
    repo.reference(
        "HEAD",
        oid,
        gix::refs::transaction::PreviousValue::Any,
        "kiki export head",
    )
    .context("setting HEAD")?;
    Ok(())
}

/// Write `refs/heads/<name>` for each bookmark in the bare git repo.
///
/// This is the ref-setting part extracted from `push()` — without the
/// subsequent `git push` subprocess call.
pub fn export_bookmarks(git_repo_path: &Path, bookmarks: &[(String, Vec<u8>)]) -> Result<()> {
    let repo = gix::open(git_repo_path).context("opening git repo for export_bookmarks")?;
    for (name, oid_bytes) in bookmarks {
        let oid = gix::ObjectId::try_from(oid_bytes.as_slice())
            .map_err(|_| anyhow!("invalid commit id for bookmark '{name}'"))?;
        if !repo.objects.exists(&oid) {
            anyhow::bail!(
                "commit {} for bookmark '{name}' not found in git ODB",
                oid
            );
        }
        let ref_name = format!("refs/heads/{name}");
        repo.reference(
            ref_name.clone(),
            oid,
            gix::refs::transaction::PreviousValue::Any,
            "kiki export bookmark",
        )
        .with_context(|| format!("setting ref {ref_name}"))?;
    }
    Ok(())
}

/// Reset the git index to match a tree object in the bare repo.
///
/// Rebuilds `<git_repo>/index` so that `git status`, `git diff`, and
/// `git add` work correctly when run from the FUSE mount. The index
/// should match the "committed" tree (the checkout target), so `git
/// status` shows the same diff as `jj diff`.
pub fn reset_index(git_repo_path: &Path, tree_oid: &[u8]) -> Result<()> {
    let repo = gix::open(git_repo_path).context("opening git repo for reset_index")?;
    let oid = gix::ObjectId::try_from(tree_oid)
        .map_err(|_| anyhow!("invalid tree id for reset_index"))?;
    let mut index = if oid.is_null() || !repo.objects.exists(&oid) {
        // Empty or missing tree → empty index.
        gix::index::File::from_state(
            gix::index::State::new(repo.object_hash()),
            repo.index_path(),
        )
    } else {
        repo.index_from_tree(&oid)
            .context("building index from tree")?
    };
    index
        .write(gix::index::write::Options::default())
        .context("writing index")?;
    Ok(())
}

/// Delete any `refs/heads/*` that are not in the active bookmarks list.
///
/// Called after `export_bookmarks` to remove refs for bookmarks that
/// were deleted from the jj View.
pub fn cleanup_stale_refs(
    git_repo_path: &Path,
    active_bookmark_names: &HashSet<&str>,
) -> Result<()> {
    let current_refs = read_local_refs(git_repo_path)?;
    for (name, _) in &current_refs {
        if !active_bookmark_names.contains(name.as_str()) {
            let ref_name = format!("refs/heads/{name}");
            let output = Command::new("git")
                .args(["update-ref", "-d", &ref_name])
                .current_dir(git_repo_path)
                .output()
                .with_context(|| format!("deleting stale ref {ref_name}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!("failed to delete stale ref {ref_name}: {}", stderr.trim());
            }
        }
    }
    Ok(())
}

/// Read the current HEAD commit from the bare git repo.
/// Returns `None` if HEAD is unborn or symbolic (not detached).
pub fn read_head(git_repo_path: &Path) -> Result<Option<Vec<u8>>> {
    let repo = gix::open(git_repo_path).context("opening git repo for read_head")?;
    let head = repo.head().context("reading HEAD")?;
    match head.kind {
        gix::head::Kind::Detached { target, .. } => Ok(Some(target.as_bytes().to_vec())),
        _ => Ok(None),
    }
}

/// Read all `refs/heads/*` from the bare git repo.
/// Returns `(bookmark_name, commit_oid_bytes)` pairs.
pub fn read_local_refs(git_repo_path: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let repo = gix::open(git_repo_path).context("opening git repo for read_local_refs")?;
    let prefix = "refs/heads/";
    let mut bookmarks = Vec::new();
    for r in repo.references()?.prefixed(prefix.as_bytes())? {
        let r = r.map_err(|e| anyhow!("iterating refs: {e}"))?;
        let name = r.name().as_bstr();
        let short = match name.strip_prefix(prefix.as_bytes()) {
            Some(s) => s,
            None => continue,
        };
        let oid = r
            .id()
            .object()
            .context("peeling ref to object")?
            .id;
        let short_str = String::from_utf8_lossy(short).into_owned();
        bookmarks.push((short_str, oid.as_bytes().to_vec()));
    }
    Ok(bookmarks)
}

/// Detect the default branch after a `git fetch`.
///
/// Checks `refs/remotes/<remote>/HEAD` (set by `git fetch` when the
/// remote advertises HEAD). Falls back to `main`, then `master`, then
/// the first remote-tracking branch alphabetically.
pub fn detect_default_branch(git_repo_path: &Path, remote: &str) -> Result<Option<String>> {
    let repo = gix::open(git_repo_path).context("opening git repo for default branch")?;
    let prefix = format!("refs/remotes/{remote}/");

    // Try refs/remotes/<remote>/HEAD (symbolic ref → refs/remotes/<remote>/<branch>).
    let head_ref_name = format!("refs/remotes/{remote}/HEAD");
    if let Ok(head_ref) = repo.find_reference(&head_ref_name) {
        if let Some(target_name) = head_ref.inner.target.try_name() {
            let target = target_name.as_bstr().to_string();
            if let Some(branch) = target.strip_prefix(&prefix) {
                return Ok(Some(branch.to_owned()));
            }
        }
    }

    // Fallback: check for well-known branch names in remote-tracking refs.
    let mut branches = Vec::new();
    if let Ok(refs) = repo.references() {
        if let Ok(iter) = refs.prefixed(prefix.as_bytes()) {
            for r in iter {
                let Ok(r) = r else { continue };
                let name = r.name().as_bstr().to_string();
                if let Some(branch) = name.strip_prefix(&prefix) {
                    if branch != "HEAD" {
                        branches.push(branch.to_owned());
                    }
                }
            }
        }
    }

    for preferred in ["main", "master"] {
        if branches.iter().any(|b| b == preferred) {
            return Ok(Some(preferred.to_owned()));
        }
    }

    // Take the first one alphabetically.
    branches.sort();
    Ok(branches.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_bare_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("test.git");
        let output = Command::new("git")
            .args(["init", "--bare"])
            .arg(&git_dir)
            .output()
            .unwrap();
        assert!(output.status.success(), "git init --bare failed");
        (tmp, git_dir)
    }

    #[test]
    fn remote_add_and_list() {
        let (_tmp, git_dir) = init_bare_repo();
        remote_add(&git_dir, "origin", "https://github.com/test/repo.git").unwrap();

        let remotes = remote_list(&git_dir).unwrap();
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].0, "origin");
        assert_eq!(remotes[0].1, "https://github.com/test/repo.git");
    }

    #[test]
    fn remote_add_duplicate_fails() {
        let (_tmp, git_dir) = init_bare_repo();
        remote_add(&git_dir, "origin", "https://example.com/a.git").unwrap();
        let err = remote_add(&git_dir, "origin", "https://example.com/b.git");
        assert!(err.is_err());
    }

    #[test]
    fn push_empty_bookmarks_fails() {
        let (_tmp, git_dir) = init_bare_repo();
        let err = push(&git_dir, "origin", &[]);
        assert!(err.is_err());
    }

    #[test]
    fn push_missing_commit_fails() {
        let (_tmp, git_dir) = init_bare_repo();
        remote_add(&git_dir, "origin", "https://example.com/a.git").unwrap();
        let bogus_oid = vec![0xaa; 20];
        let err = push(&git_dir, "origin", &[("main".into(), bogus_oid)]);
        assert!(err.is_err());
        assert!(
            err.unwrap_err().to_string().contains("not found"),
            "should mention missing commit"
        );
    }

    /// Create a commit in a bare repo using git plumbing commands.
    /// Returns the 20-byte raw OID of the commit.
    fn create_test_commit(git_dir: &Path) -> Vec<u8> {
        // Create a blob.
        let blob = Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(git_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            blob.stdin.as_ref().unwrap().write_all(b"hello\n").unwrap();
        }
        let blob_out = blob.wait_with_output().unwrap();
        assert!(blob_out.status.success());
        let blob_hex = String::from_utf8(blob_out.stdout).unwrap();
        let blob_hex = blob_hex.trim();

        // Create a tree containing that blob.
        let tree_input = format!("100644 blob {blob_hex}\tfile.txt\n");
        let tree = Command::new("git")
            .args(["mktree"])
            .current_dir(git_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            tree.stdin
                .as_ref()
                .unwrap()
                .write_all(tree_input.as_bytes())
                .unwrap();
        }
        let tree_out = tree.wait_with_output().unwrap();
        assert!(tree_out.status.success());
        let tree_hex = String::from_utf8(tree_out.stdout).unwrap();
        let tree_hex = tree_hex.trim();

        // Create a commit pointing at that tree.
        let commit = Command::new("git")
            .args(["commit-tree", tree_hex, "-m", "test commit"])
            .current_dir(git_dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test")
            .output()
            .unwrap();
        assert!(commit.status.success());
        let commit_hex = String::from_utf8(commit.stdout).unwrap();
        let commit_hex = commit_hex.trim();

        // Parse the hex OID into raw bytes via gix.
        let oid = gix::ObjectId::from_hex(commit_hex.as_bytes()).unwrap();
        oid.as_bytes().to_vec()
    }

    #[test]
    fn export_head_round_trip() {
        let (_tmp, git_dir) = init_bare_repo();
        let commit_oid = create_test_commit(&git_dir);

        export_head(&git_dir, &commit_oid).unwrap();
        let read_back = read_head(&git_dir).unwrap();
        assert_eq!(read_back, Some(commit_oid));
    }

    #[test]
    fn export_bookmarks_round_trip() {
        let (_tmp, git_dir) = init_bare_repo();
        let commit_oid = create_test_commit(&git_dir);

        let bookmarks = vec![("my-branch".to_owned(), commit_oid.clone())];
        export_bookmarks(&git_dir, &bookmarks).unwrap();

        let refs = read_local_refs(&git_dir).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, "my-branch");
        assert_eq!(refs[0].1, commit_oid);
    }

    #[test]
    fn cleanup_stale_refs_deletes_old_bookmarks() {
        let (_tmp, git_dir) = init_bare_repo();
        let commit_oid = create_test_commit(&git_dir);

        // Export two bookmarks.
        let bookmarks = vec![
            ("keep".to_owned(), commit_oid.clone()),
            ("delete-me".to_owned(), commit_oid.clone()),
        ];
        export_bookmarks(&git_dir, &bookmarks).unwrap();
        assert_eq!(read_local_refs(&git_dir).unwrap().len(), 2);

        // Cleanup with only "keep" active.
        let active: HashSet<&str> = ["keep"].into_iter().collect();
        cleanup_stale_refs(&git_dir, &active).unwrap();

        let refs = read_local_refs(&git_dir).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, "keep");
    }

    #[test]
    fn reset_index_populates_index_from_tree() {
        let (_tmp, git_dir) = init_bare_repo();
        let commit_oid = create_test_commit(&git_dir);

        // Read the commit's tree id.
        let commit_gix = gix::ObjectId::try_from(commit_oid.as_slice()).unwrap();
        let output = Command::new("git")
            .args(["cat-file", "-p", &commit_gix.to_string()])
            .current_dir(&git_dir)
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let tree_hex = stdout
            .lines()
            .find(|l| l.starts_with("tree "))
            .unwrap()
            .strip_prefix("tree ")
            .unwrap()
            .trim();
        let tree_oid = gix::ObjectId::from_hex(tree_hex.as_bytes()).unwrap();

        // Reset index to the tree.
        reset_index(&git_dir, tree_oid.as_bytes()).unwrap();

        // Verify git sees entries in the index.
        let output = Command::new("git")
            .args(["ls-files", "--cached"])
            .current_dir(&git_dir)
            .output()
            .unwrap();
        assert!(output.status.success());
        let files = String::from_utf8(output.stdout).unwrap();
        assert!(
            files.contains("file.txt"),
            "index should contain file.txt, got: {files}"
        );
    }
}
