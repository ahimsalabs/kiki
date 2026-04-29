//! gRPC-backed `RemoteStore` (`grpc://host:port` scheme, PLAN.md §13.3).
//!
//! Tonic client wrapper that turns the byte-typed [`RemoteStore`] trait
//! into the unary RPCs declared in `proto/jj_interface.proto`'s
//! `service RemoteStore`. Any daemon serves the matching server side
//! ([`super::server::RemoteStoreService`]) on its existing gRPC
//! listener, so the same daemon binary can be both a client and a
//! peer/server with no protocol fork.
//!
//! Connection model: `connect_lazy` — the channel doesn't actually dial
//! until the first RPC. Keeps `remote::parse(...)` synchronous and
//! defers transport failures to the call site (where we have a real
//! tracing context). Same trust assumptions as the existing
//! `JujutsuInterface` listener: single-user, localhost. TLS + auth are
//! M11 alongside S3.

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use proto::jj_interface::remote_store_client::RemoteStoreClient;
use proto::jj_interface::{GetBlobReq, HasBlobReq, PutBlobReq};
use tonic::transport::Channel;

use super::{BlobKind, RemoteStore};
use crate::ty::Id;

/// Tonic-backed `RemoteStore` reachable at `grpc://<endpoint>`.
///
/// Cloning the wrapper is cheap (`tonic::transport::Channel` is `Clone`
/// and shares the underlying connection pool); each method clones the
/// client locally so calls don't serialize behind a single mutable
/// reference. This is the pattern tonic's docs recommend.
#[derive(Clone, Debug)]
pub struct GrpcRemoteStore {
    client: RemoteStoreClient<Channel>,
    // Kept around for `Debug` / tracing context. The channel itself is
    // already opaque; storing the user-supplied endpoint string makes
    // failure messages greppable.
    endpoint: String,
}

impl GrpcRemoteStore {
    /// Build a lazily-connected client for `grpc://<endpoint>`.
    /// `endpoint` is the part after the scheme (e.g. `"127.0.0.1:8080"`,
    /// `"[::1]:9999"`). The channel does not dial here; the first RPC
    /// triggers `connect_lazy`'s actual TCP connect.
    ///
    /// Returns `Err` only for a syntactically-malformed endpoint. A
    /// reachable-but-unresponsive peer surfaces as an `Err` from the
    /// first `get_blob` / `put_blob` / `has_blob` call instead.
    pub fn new(endpoint: &str) -> Result<Self> {
        // Tonic requires an `http://` prefix on the URL it parses; we
        // hide that here so the user-facing scheme stays `grpc://`.
        let url = format!("http://{endpoint}");
        let channel = Channel::from_shared(url.clone())
            .with_context(|| format!("parsing grpc remote endpoint {url:?}"))?
            .connect_lazy();
        Ok(GrpcRemoteStore {
            client: RemoteStoreClient::new(channel),
            endpoint: endpoint.to_owned(),
        })
    }
}

#[async_trait]
impl RemoteStore for GrpcRemoteStore {
    async fn get_blob(&self, kind: BlobKind, id: &Id) -> Result<Option<Bytes>> {
        let mut client = self.client.clone();
        let resp = client
            .get_blob(GetBlobReq {
                kind: kind.as_proto() as i32,
                id: id.0.to_vec(),
            })
            .await
            .with_context(|| format!("grpc get_blob {} @ {}", kind.as_str(), self.endpoint))?
            .into_inner();
        // `found = false` means "remote doesn't have it"; the `bytes`
        // field is meaningless in that case (and may be empty).
        Ok(if resp.found {
            Some(Bytes::from(resp.bytes))
        } else {
            None
        })
    }

    async fn put_blob(&self, kind: BlobKind, id: &Id, bytes: Bytes) -> Result<()> {
        let mut client = self.client.clone();
        client
            .put_blob(PutBlobReq {
                kind: kind.as_proto() as i32,
                id: id.0.to_vec(),
                bytes: bytes.to_vec(),
            })
            .await
            .with_context(|| format!("grpc put_blob {} @ {}", kind.as_str(), self.endpoint))?;
        Ok(())
    }

    async fn has_blob(&self, kind: BlobKind, id: &Id) -> Result<bool> {
        let mut client = self.client.clone();
        let resp = client
            .has_blob(HasBlobReq {
                kind: kind.as_proto() as i32,
                id: id.0.to_vec(),
            })
            .await
            .with_context(|| format!("grpc has_blob {} @ {}", kind.as_str(), self.endpoint))?
            .into_inner();
        Ok(resp.found)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::Server as GrpcServer;

    use super::super::fs::FsRemoteStore;
    use super::super::server::RemoteStoreService;
    use super::*;

    /// Spawn the M9 `RemoteStoreServer` on an ephemeral localhost port.
    /// Returns `(endpoint, backing_dir, server_join)` where dropping
    /// `server_join` does *not* stop the server (it runs until the
    /// runtime drops); tests rely on `tempfile::TempDir` cleanup +
    /// process exit instead.
    async fn spawn_server() -> (String, tempfile::TempDir) {
        let backing = tempfile::tempdir().expect("backing tempdir");
        let backend: Arc<dyn RemoteStore> =
            Arc::new(FsRemoteStore::new(backing.path().to_owned()));
        let service = RemoteStoreService::new(backend).into_server();

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral bind");
        let addr = listener.local_addr().expect("local_addr");

        tokio::spawn(async move {
            GrpcServer::builder()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .expect("RemoteStore tonic server")
        });

        (addr.to_string(), backing)
    }

    /// End-to-end gRPC round-trip: spin up a real tonic server with an
    /// FsRemoteStore backend, point a `GrpcRemoteStore` client at it,
    /// and exercise put → has → get on every blob kind.
    #[tokio::test]
    async fn grpc_round_trip_against_real_server() {
        let (endpoint, _backing) = spawn_server().await;
        let client = GrpcRemoteStore::new(&endpoint).expect("connect_lazy");

        let id = Id([0xab; 32]);
        // put_blob across all kinds to confirm the BlobKind enum
        // round-trips through the wire correctly.
        for (kind, payload) in [
            (BlobKind::Tree, b"tree-bytes".as_ref()),
            (BlobKind::File, b"file-bytes".as_ref()),
            (BlobKind::Symlink, b"symlink-bytes".as_ref()),
            (BlobKind::Commit, b"commit-bytes".as_ref()),
        ] {
            client
                .put_blob(kind, &id, Bytes::copy_from_slice(payload))
                .await
                .unwrap_or_else(|e| panic!("put_blob {kind:?}: {e:#}"));
            assert!(
                client.has_blob(kind, &id).await.unwrap(),
                "has_blob({kind:?}) should be true after put"
            );
            let got = client
                .get_blob(kind, &id)
                .await
                .unwrap_or_else(|e| panic!("get_blob {kind:?}: {e:#}"))
                .expect("blob present");
            assert_eq!(got.as_ref(), payload, "kind {kind:?}");
        }

        // Missing blob → Ok(None), not an error.
        let missing = client
            .get_blob(BlobKind::Tree, &Id([0; 32]))
            .await
            .expect("get_blob (missing)");
        assert!(missing.is_none());
        assert!(!client.has_blob(BlobKind::Tree, &Id([0; 32])).await.unwrap());
    }

    /// Two `JujutsuService`-style users sharing a `grpc://` remote.
    /// Service A pushes via write-through, service B reads via
    /// read-through — the byte-typed trait is honest under a real
    /// network transport.
    #[tokio::test]
    async fn two_clients_share_blobs_via_grpc_server() {
        let (endpoint, _backing) = spawn_server().await;
        let client_a = GrpcRemoteStore::new(&endpoint).unwrap();
        let client_b = GrpcRemoteStore::new(&endpoint).unwrap();

        let id = Id([0x77; 32]);
        client_a
            .put_blob(BlobKind::File, &id, Bytes::from_static(b"shared"))
            .await
            .unwrap();
        let got = client_b
            .get_blob(BlobKind::File, &id)
            .await
            .unwrap()
            .expect("client B sees blob written by client A");
        assert_eq!(got.as_ref(), b"shared");
    }

    /// Sanity: put_blob with a malformed id (≠32 bytes) is rejected on
    /// the server side, surfaced to the client as an `Err` rather than
    /// silently accepted. Catches a regression where the server forgets
    /// to validate id length.
    #[tokio::test]
    async fn server_rejects_short_id() {
        let (endpoint, _backing) = spawn_server().await;
        // Bypass the wrapper to send a hand-crafted bad request.
        let client = GrpcRemoteStore::new(&endpoint).unwrap();
        let mut raw = client.client.clone();
        let err = raw
            .put_blob(PutBlobReq {
                kind: BlobKind::File.as_proto() as i32,
                id: vec![0u8; 16], // wrong length
                bytes: vec![],
            })
            .await
            .expect_err("server must reject short id");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
