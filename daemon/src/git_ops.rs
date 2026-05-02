//! Git remote operations on the per-mount bare git repo.
//!
//! Post git-convergence, every mount's content store is a bare git repo
//! (managed by [`crate::git_store::GitContentStore`]). This module adds
//! git remote management and push/fetch by driving the `git` subprocess,
//! matching `jj git` behavior.

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
}
