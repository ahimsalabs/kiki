//! Layer C — remote blob CAS (PLAN.md §13).
//!
//! Byte-typed content-addressed store. The local
//! [`crate::git_store::GitContentStore`] stays jj-typed (`get_tree`,
//! `write_file`, ...) because it round-trips through jj-lib's
//! `GitBackend`. A *remote* doesn't need to know about jj types at all
//! — it's just bytes keyed by `(BlobKind, Id)`. Decoupling at the byte
//! boundary means the wire protocol survives schema evolution and lets
//! every backend (fs, gRPC, S3, ...) implement the same three methods.
//!
//! Backends ship in [`fs`] (filesystem, `dir://` scheme), [`grpc`]
//! (peer daemon, `kiki://` / `grpc://` scheme), and [`ssh`]
//! (SSH transport, `ssh://` scheme) — see PLAN.md §13.3.

use std::sync::Arc;

use anyhow::{anyhow, Result};

pub use store::{BlobKind, CasOutcome, RemoteStore, validate_ref_name};

pub mod fetch;
pub mod fs;
pub mod grpc;
pub mod server;
pub mod ssh;

#[cfg(test)]
mod sync_sim_tests;

/// Parse `Initialize.remote` into an optional [`RemoteStore`] handle.
///
/// Supported schemes:
/// - `""` → `Ok(None)` (no remote; local-only operation).
/// - `dir:///abs/path` → filesystem-backed remote.
/// - `kiki://host:port` or `grpc://host:port` → peer daemon gRPC.
/// - `ssh://user@host/abs/path` → SSH transport (no daemon on server).
pub fn parse(remote: &str) -> Result<Option<Arc<dyn RemoteStore>>> {
    if remote.is_empty() {
        return Ok(None);
    }
    let (scheme, rest) = remote
        .split_once("://")
        .ok_or_else(|| anyhow!(
            "remote {remote:?} has no scheme; expected dir://…, kiki://…, ssh://…, or grpc://…"
        ))?;
    match scheme {
        "dir" => {
            if !rest.starts_with('/') {
                return Err(anyhow!(
                    "dir:// remote must use absolute path (got dir://{rest})"
                ));
            }
            Ok(Some(Arc::new(fs::FsRemoteStore::new(rest.into()))
                as Arc<dyn RemoteStore>))
        }
        "kiki" | "grpc" => {
            if rest.is_empty() {
                return Err(anyhow!(
                    "{scheme}:// remote requires host:port (got empty endpoint)"
                ));
            }
            Ok(Some(Arc::new(grpc::GrpcRemoteStore::new(rest)?)
                as Arc<dyn RemoteStore>))
        }
        "ssh" => {
            // ssh://user@host/path — authority is user@host, path is
            // absolute on the remote. Also supports ssh://host/path
            // (no explicit user — SSH uses its default).
            let (authority, path) = rest.split_once('/').ok_or_else(|| {
                anyhow!("ssh:// remote requires host/path (got ssh://{rest})")
            })?;
            let path = format!("/{path}");
            if authority.is_empty() {
                return Err(anyhow!(
                    "ssh:// remote requires host (got empty authority)"
                ));
            }
            let (user, host) = match authority.split_once('@') {
                Some((u, h)) => (u.to_owned(), h.to_owned()),
                None => (String::new(), authority.to_owned()),
            };
            if host.is_empty() {
                return Err(anyhow!(
                    "ssh:// remote requires non-empty host"
                ));
            }
            Ok(Some(Arc::new(ssh::SshRemoteStore::new(user, host, path))
                as Arc<dyn RemoteStore>))
        }
        other => Err(anyhow!("unsupported remote scheme {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse("").unwrap().is_none());
    }

    #[test]
    fn parse_dir_absolute_ok() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("dir://{}", dir.path().display());
        let remote = parse(&url).unwrap().expect("dir:// returns Some");
        // Smoke test: a freshly-rooted remote answers `has_blob` with
        // `false` and not an error.
        let id = [0u8; 32];
        let rt = tokio::runtime::Runtime::new().unwrap();
        let has = rt.block_on(remote.has_blob(BlobKind::Tree, &id)).unwrap();
        assert!(!has);
    }

    #[test]
    fn parse_dir_relative_rejected() {
        let err = parse("dir://relative/path").unwrap_err();
        assert!(err.to_string().contains("absolute"), "got: {err}");
    }

    #[test]
    fn parse_no_scheme_rejected() {
        let err = parse("not-a-url").unwrap_err();
        assert!(err.to_string().contains("no scheme"), "got: {err}");
    }

    // `connect_lazy` itself just constructs a `Channel` future, but
    // tonic touches the tokio executor on construction — so the test
    // needs a runtime even though no RPC fires.
    #[tokio::test]
    async fn parse_grpc_returns_some() {
        // grpc:// is parsed lazily — `connect_lazy` defers the actual
        // TCP connect to the first RPC, so an unreachable peer here is
        // not an error at parse time. We just confirm the parser
        // produces a `Some(...)`.
        let remote = parse("grpc://127.0.0.1:9999")
            .unwrap()
            .expect("grpc:// returns Some");
        assert!(format!("{remote:?}").contains("GrpcRemoteStore"));
    }

    #[test]
    fn parse_grpc_empty_endpoint_rejected() {
        let err = parse("grpc://").unwrap_err();
        assert!(err.to_string().contains("host:port"), "got: {err}");
    }

    #[tokio::test]
    async fn parse_kiki_scheme() {
        let remote = parse("kiki://127.0.0.1:8080")
            .unwrap()
            .expect("kiki:// returns Some");
        assert!(format!("{remote:?}").contains("GrpcRemoteStore"));
    }

    #[test]
    fn parse_kiki_empty_endpoint_rejected() {
        let err = parse("kiki://").unwrap_err();
        assert!(err.to_string().contains("host:port"), "got: {err}");
    }

    #[test]
    fn parse_ssh_with_user() {
        let remote = parse("ssh://cbro@myserver.com/data/kiki-store")
            .unwrap()
            .expect("ssh:// returns Some");
        let dbg = format!("{remote:?}");
        assert!(dbg.contains("SshRemoteStore"), "got: {dbg}");
        assert!(dbg.contains("ssh://cbro@myserver.com/data/kiki-store"), "got: {dbg}");
    }

    #[test]
    fn parse_ssh_without_user() {
        let remote = parse("ssh://myserver/data/store")
            .unwrap()
            .expect("ssh:// without user returns Some");
        let dbg = format!("{remote:?}");
        assert!(dbg.contains("SshRemoteStore"), "got: {dbg}");
    }

    #[test]
    fn parse_ssh_no_path_rejected() {
        let err = parse("ssh://user@host").unwrap_err();
        assert!(err.to_string().contains("host/path"), "got: {err}");
    }

    #[test]
    fn parse_ssh_empty_authority_rejected() {
        let err = parse("ssh:///path").unwrap_err();
        assert!(err.to_string().contains("host"), "got: {err}");
    }

    #[test]
    fn parse_unknown_scheme_rejected() {
        let err = parse("s3://bucket/prefix").unwrap_err();
        assert!(err.to_string().contains("unsupported"), "got: {err}");
    }
}
