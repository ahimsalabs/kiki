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

use anyhow::{anyhow, Context, Result};

pub use store::{BlobKind, CasOutcome, RemoteStore, validate_ref_name};

pub mod fetch;
pub mod fs;
pub mod grpc;
pub mod server;
pub mod tunnel;

#[cfg(test)]
mod sync_sim_tests;

/// Parse `Initialize.remote` into an optional [`RemoteStore`] handle.
///
/// Supported schemes:
/// - `""` → `Ok(None)` (no remote; local-only operation).
/// - `dir:///abs/path` → filesystem-backed remote.
/// - `kiki://host:port` or `grpc://host:port` → peer daemon gRPC.
///
/// `ssh://` URLs are NOT handled here — they require async tunnel
/// establishment. Use [`establish_ssh_remote()`] for SSH remotes. This
/// function returns `Err` for `ssh://` to prevent accidental misuse.
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
            Err(anyhow!(
                "ssh:// remotes require async tunnel establishment; \
                 use establish_ssh_remote() instead of parse()"
            ))
        }
        other => Err(anyhow!("unsupported remote scheme {other:?}")),
    }
}

/// Parse an `ssh://` URL into its (user, host, path) components.
///
/// Returns `None` if the URL is not an `ssh://` URL.
/// Returns `Err` for malformed `ssh://` URLs.
pub fn parse_ssh_url(remote: &str) -> Result<Option<(String, String, String)>> {
    let Some(rest) = remote.strip_prefix("ssh://") else {
        return Ok(None);
    };
    let (authority, path) = rest.split_once('/').ok_or_else(|| {
        anyhow!("ssh:// remote requires host/path (got ssh://{rest})")
    })?;
    let path = format!("/{path}");
    if authority.is_empty() {
        return Err(anyhow!("ssh:// remote requires host (got empty authority)"));
    }
    let (user, host) = match authority.split_once('@') {
        Some((u, h)) => (u.to_owned(), h.to_owned()),
        None => (String::new(), authority.to_owned()),
    };
    if host.is_empty() {
        return Err(anyhow!("ssh:// remote requires non-empty host"));
    }
    Ok(Some((user, host, path)))
}

/// Establish an SSH tunnel and return a `RemoteStore` connected through it.
///
/// This is the async counterpart to `parse()` for `ssh://` URLs. It:
/// 1. Parses the URL into (user, host, path).
/// 2. Establishes a persistent SSH tunnel to the remote daemon.
/// 3. Connects a `GrpcRemoteStore` to the forwarded local socket.
///
/// The returned `SshTunnel` must be kept alive for the duration of the
/// mount — dropping it kills the SSH process and removes the socket.
///
/// `tunnels_dir` is typically `<runtime_dir>/tunnels/`.
pub async fn establish_ssh_remote(
    remote: &str,
    tunnels_dir: &std::path::Path,
) -> Result<(Arc<dyn RemoteStore>, tunnel::SshTunnel)> {
    let (user, host, path) = parse_ssh_url(remote)?
        .ok_or_else(|| anyhow!("not an ssh:// URL: {remote:?}"))?;

    let tun = tunnel::SshTunnel::establish(&user, &host, &path, tunnels_dir).await?;

    let store = grpc::GrpcRemoteStore::new_uds(tun.local_socket()).await
        .with_context(|| format!(
            "connecting GrpcRemoteStore to tunnel socket {}",
            tun.local_socket().display()
        ))?;

    Ok((Arc::new(store) as Arc<dyn RemoteStore>, tun))
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
    fn parse_rejects_ssh_scheme() {
        // ssh:// is handled by the async establish_ssh_remote() path,
        // not the synchronous parse(). parse() must reject it.
        let err = parse("ssh://cbro@myserver.com/data/store").unwrap_err();
        assert!(err.to_string().contains("establish_ssh_remote"), "got: {err}");
    }

    #[test]
    fn parse_ssh_url_with_user() {
        let (user, host, path) = parse_ssh_url("ssh://cbro@myserver.com/data/kiki-store")
            .unwrap()
            .expect("ssh:// returns Some");
        assert_eq!(user, "cbro");
        assert_eq!(host, "myserver.com");
        assert_eq!(path, "/data/kiki-store");
    }

    #[test]
    fn parse_ssh_url_without_user() {
        let (user, host, path) = parse_ssh_url("ssh://myserver/data/store")
            .unwrap()
            .expect("ssh:// without user returns Some");
        assert_eq!(user, "");
        assert_eq!(host, "myserver");
        assert_eq!(path, "/data/store");
    }

    #[test]
    fn parse_ssh_url_no_path_rejected() {
        let err = parse_ssh_url("ssh://user@host").unwrap_err();
        assert!(err.to_string().contains("host/path"), "got: {err}");
    }

    #[test]
    fn parse_ssh_url_empty_authority_rejected() {
        let err = parse_ssh_url("ssh:///path").unwrap_err();
        assert!(err.to_string().contains("host"), "got: {err}");
    }

    #[test]
    fn parse_ssh_url_returns_none_for_non_ssh() {
        assert!(parse_ssh_url("grpc://host:1234").unwrap().is_none());
        assert!(parse_ssh_url("dir:///path").unwrap().is_none());
        assert!(parse_ssh_url("").unwrap().is_none());
    }

    #[test]
    fn parse_unknown_scheme_rejected() {
        let err = parse("s3://bucket/prefix").unwrap_err();
        assert!(err.to_string().contains("unsupported"), "got: {err}");
    }
}
