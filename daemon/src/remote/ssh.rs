//! SSH-backed `RemoteStore` (`ssh://user@host/path` scheme).
//!
//! Git-style SSH transport: the daemon spawns
//! `ssh user@host kiki kk serve /path` and speaks the
//! length-prefixed protobuf framing protocol (see `store::framing`)
//! over the child's stdin/stdout. No daemon needed on the remote —
//! just the `kiki` binary and a directory.
//!
//! Connection model: lazy. The SSH child is spawned on first RPC,
//! not at construction. This keeps `remote::parse()` synchronous
//! (matching `GrpcRemoteStore::new`'s `connect_lazy` pattern) and
//! defers SSH failures to the call site.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use tokio::process::{Child, ChildStdin, ChildStdout};

use store::framing::{read_frame, write_frame};
use store::{BlobKind, CasOutcome, RemoteStore};

/// SSH-backed `RemoteStore` reachable at `ssh://user@host/path`.
pub struct SshRemoteStore {
    user: String,
    host: String,
    path: String,
    state: tokio::sync::Mutex<Option<SshState>>,
}

struct SshState {
    _child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    next_id: u64,
}

impl std::fmt::Debug for SshRemoteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshRemoteStore")
            .field("url", &format!("ssh://{}@{}{}", self.user, self.host, self.path))
            .finish()
    }
}

impl SshRemoteStore {
    /// Create a new (lazily-connected) SSH remote store.
    ///
    /// The SSH child process is not spawned until the first RPC call.
    pub fn new(user: String, host: String, path: String) -> Self {
        SshRemoteStore {
            user,
            host,
            path,
            state: tokio::sync::Mutex::new(None),
        }
    }

    fn url(&self) -> String {
        format!("ssh://{}@{}{}", self.user, self.host, self.path)
    }

    /// Spawn the SSH child process if not already running.
    async fn ensure_connected<'a>(
        state: &'a mut Option<SshState>,
        user: &str,
        host: &str,
        path: &str,
    ) -> Result<&'a mut SshState> {
        if state.is_none() {
            let target = if user.is_empty() {
                host.to_owned()
            } else {
                format!("{user}@{host}")
            };
            let mut child = tokio::process::Command::new("ssh")
                // BatchMode prevents interactive password prompts.
                // Keys must be pre-configured (agent, config, etc.).
                .arg("-o")
                .arg("BatchMode=yes")
                .arg(&target)
                .arg("kiki")
                .arg("kk")
                .arg("serve")
                .arg(path)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                // Let SSH errors (auth failures, host not found, etc.)
                // flow to the user's terminal.
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .with_context(|| format!("spawning ssh to {target}"))?;

            let stdin = child.stdin.take().expect("stdin piped");
            let stdout = child.stdout.take().expect("stdout piped");

            *state = Some(SshState {
                _child: child,
                stdin,
                stdout,
                next_id: 1,
            });
        }
        Ok(state.as_mut().unwrap())
    }
}

/// Send a `StoreRequest` and read back the `StoreResponse`, extracting
/// the expected `oneof` variant or surfacing `StoreError` as an `Err`.
macro_rules! ssh_rpc {
    ($self:expr, $variant:ident, $req_payload:expr, $resp_variant:ident) => {{
        let url = $self.url();
        let mut guard = $self.state.lock().await;
        let conn = Self::ensure_connected(&mut guard, &$self.user, &$self.host, &$self.path)
            .await
            .with_context(|| format!("connecting to {url}"))?;

        let id = conn.next_id;
        conn.next_id += 1;

        let request = proto::jj_interface::StoreRequest {
            id,
            request: Some(proto::jj_interface::store_request::Request::$variant(
                $req_payload,
            )),
        };
        write_frame(&mut conn.stdin, &request)
            .await
            .with_context(|| format!("writing {} to {url}", stringify!($variant)))?;

        let response: proto::jj_interface::StoreResponse =
            read_frame(&mut conn.stdout)
                .await
                .with_context(|| format!("reading {} response from {url}", stringify!($variant)))?
                .ok_or_else(|| anyhow!("SSH connection closed unexpectedly for {url}"))?;

        if response.id != id {
            return Err(anyhow!(
                "response id mismatch: expected {id}, got {}",
                response.id
            ));
        }

        match response.response {
            Some(proto::jj_interface::store_response::Response::$resp_variant(reply)) => {
                Ok(reply)
            }
            Some(proto::jj_interface::store_response::Response::Error(e)) => {
                Err(anyhow!("remote error from {url}: {}", e.message))
            }
            other => Err(anyhow!(
                "unexpected response variant from {url}: {other:?}"
            )),
        }
    }};
}

#[async_trait]
impl RemoteStore for SshRemoteStore {
    async fn get_blob(&self, kind: BlobKind, id: &[u8]) -> Result<Option<Bytes>> {
        let reply = ssh_rpc!(
            self,
            GetBlob,
            proto::jj_interface::GetBlobReq {
                kind: kind.as_proto() as i32,
                id: id.to_vec(),
            },
            GetBlob
        )?;
        Ok(if reply.found {
            Some(Bytes::from(reply.bytes))
        } else {
            None
        })
    }

    async fn put_blob(&self, kind: BlobKind, id: &[u8], bytes: Bytes) -> Result<()> {
        ssh_rpc!(
            self,
            PutBlob,
            proto::jj_interface::PutBlobReq {
                kind: kind.as_proto() as i32,
                id: id.to_vec(),
                bytes: bytes.to_vec(),
            },
            PutBlob
        )?;
        Ok(())
    }

    async fn has_blob(&self, kind: BlobKind, id: &[u8]) -> Result<bool> {
        let reply = ssh_rpc!(
            self,
            HasBlob,
            proto::jj_interface::HasBlobReq {
                kind: kind.as_proto() as i32,
                id: id.to_vec(),
            },
            HasBlob
        )?;
        Ok(reply.found)
    }

    async fn get_ref(&self, name: &str) -> Result<Option<Bytes>> {
        let reply = ssh_rpc!(
            self,
            GetRef,
            proto::jj_interface::GetRefReq {
                name: name.to_owned(),
            },
            GetRef
        )?;
        Ok(if reply.found {
            Some(Bytes::from(reply.value))
        } else {
            None
        })
    }

    async fn cas_ref(
        &self,
        name: &str,
        expected: Option<&Bytes>,
        new: Option<&Bytes>,
    ) -> Result<CasOutcome> {
        let reply = ssh_rpc!(
            self,
            CasRef,
            proto::jj_interface::CasRefReq {
                name: name.to_owned(),
                expected: expected.map(|b| b.to_vec()),
                new: new.map(|b| b.to_vec()),
            },
            CasRef
        )?;
        Ok(if reply.updated {
            CasOutcome::Updated
        } else {
            CasOutcome::Conflict {
                actual: reply.actual.map(Bytes::from),
            }
        })
    }

    async fn list_refs(&self) -> Result<Vec<String>> {
        let reply = ssh_rpc!(
            self,
            ListRefs,
            proto::jj_interface::ListRefsReq {},
            ListRefs
        )?;
        Ok(reply.names)
    }
}

impl Drop for SshRemoteStore {
    fn drop(&mut self) {
        // Best-effort cleanup: drop the inner Option<SshState> to signal
        // EOF on stdin to the remote serve process. The child
        // process is reaped when `_child` drops.
        // tokio::sync::Mutex doesn't have a sync get_mut in older versions,
        // so we just let the Mutex drop naturally — it owns the SshState
        // and will drop it (closing stdin → EOF → remote exits).
    }
}
