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
use proto::jj_interface::{
    CasRefReq, GetBlobReq, GetRefReq, HasBlobReq, ListRefsReq, PutBlobReq,
};
use tonic::transport::Channel;

use super::{BlobKind, CasOutcome, RemoteStore};

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
    async fn get_blob(&self, kind: BlobKind, id: &[u8]) -> Result<Option<Bytes>> {
        let mut client = self.client.clone();
        let resp = client
            .get_blob(GetBlobReq {
                kind: kind.as_proto() as i32,
                id: id.to_vec(),
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

    async fn put_blob(&self, kind: BlobKind, id: &[u8], bytes: Bytes) -> Result<()> {
        let mut client = self.client.clone();
        client
            .put_blob(PutBlobReq {
                kind: kind.as_proto() as i32,
                id: id.to_vec(),
                bytes: bytes.to_vec(),
            })
            .await
            .with_context(|| format!("grpc put_blob {} @ {}", kind.as_str(), self.endpoint))?;
        Ok(())
    }

    async fn has_blob(&self, kind: BlobKind, id: &[u8]) -> Result<bool> {
        let mut client = self.client.clone();
        let resp = client
            .has_blob(HasBlobReq {
                kind: kind.as_proto() as i32,
                id: id.to_vec(),
            })
            .await
            .with_context(|| format!("grpc has_blob {} @ {}", kind.as_str(), self.endpoint))?
            .into_inner();
        Ok(resp.found)
    }

    async fn get_ref(&self, name: &str) -> Result<Option<Bytes>> {
        let mut client = self.client.clone();
        let resp = client
            .get_ref(GetRefReq {
                name: name.to_owned(),
            })
            .await
            .with_context(|| format!("grpc get_ref {name:?} @ {}", self.endpoint))?
            .into_inner();
        Ok(if resp.found {
            Some(Bytes::from(resp.value))
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
        let mut client = self.client.clone();
        // proto3 `optional bytes` is `Option<Vec<u8>>` on the wire;
        // an explicit `None` round-trips as "field absent" (must-not-
        // exist / delete), Some(empty) round-trips as "must equal
        // empty / set to empty".
        let resp = client
            .cas_ref(CasRefReq {
                name: name.to_owned(),
                expected: expected.map(|b| b.to_vec()),
                new: new.map(|b| b.to_vec()),
            })
            .await
            .with_context(|| format!("grpc cas_ref {name:?} @ {}", self.endpoint))?
            .into_inner();
        Ok(if resp.updated {
            CasOutcome::Updated
        } else {
            CasOutcome::Conflict {
                actual: resp.actual.map(Bytes::from),
            }
        })
    }

    async fn list_refs(&self) -> Result<Vec<String>> {
        let mut client = self.client.clone();
        let resp = client
            .list_refs(ListRefsReq {})
            .await
            .with_context(|| format!("grpc list_refs @ {}", self.endpoint))?
            .into_inner();
        Ok(resp.names)
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

        let id = [0xab_u8; 32];
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

        // M10.6: 64-byte View/Operation ids also round-trip.
        let long_id = [0xef_u8; 64];
        for (kind, payload) in [
            (BlobKind::View, b"view-bytes".as_ref()),
            (BlobKind::Operation, b"operation-bytes".as_ref()),
        ] {
            client
                .put_blob(kind, &long_id, Bytes::copy_from_slice(payload))
                .await
                .unwrap_or_else(|e| panic!("put_blob {kind:?}: {e:#}"));
            let got = client
                .get_blob(kind, &long_id)
                .await
                .unwrap_or_else(|e| panic!("get_blob {kind:?}: {e:#}"))
                .expect("blob present");
            assert_eq!(got.as_ref(), payload, "kind {kind:?}");
        }

        // Missing blob → Ok(None), not an error.
        let missing_id = [0_u8; 32];
        let missing = client
            .get_blob(BlobKind::Tree, &missing_id)
            .await
            .expect("get_blob (missing)");
        assert!(missing.is_none());
        assert!(!client.has_blob(BlobKind::Tree, &missing_id).await.unwrap());
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

        let id = [0x77_u8; 32];
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

    /// M10: end-to-end ref CAS over the wire. One client advances the
    /// ref; another observes via get_ref + list_refs. Proves the
    /// proto3 `optional` semantics survive the gRPC round-trip — the
    /// only thing this test catches that the server-side unit tests
    /// don't is mis-encoding `None`-vs-empty across the wire.
    #[tokio::test]
    async fn grpc_ref_cas_round_trip() {
        let (endpoint, _backing) = spawn_server().await;
        let client_a = GrpcRemoteStore::new(&endpoint).unwrap();
        let client_b = GrpcRemoteStore::new(&endpoint).unwrap();

        // Initially no refs.
        assert!(client_a.list_refs().await.unwrap().is_empty());

        // Client A creates `op_heads` → v0.
        let v0 = Bytes::from_static(b"v0");
        assert_eq!(
            client_a.cas_ref("op_heads", None, Some(&v0)).await.unwrap(),
            CasOutcome::Updated
        );

        // Client B observes v0 + the new entry in list_refs.
        assert_eq!(
            client_b.get_ref("op_heads").await.unwrap(),
            Some(v0.clone())
        );
        assert_eq!(client_b.list_refs().await.unwrap(), vec!["op_heads"]);

        // Client B advances v0 → v1 with correct precondition.
        let v1 = Bytes::from_static(b"v1");
        assert_eq!(
            client_b
                .cas_ref("op_heads", Some(&v0), Some(&v1))
                .await
                .unwrap(),
            CasOutcome::Updated
        );

        // Client A's stale CAS (still believes v0) loses, gets v1
        // back as the actual current value so it can retry.
        let v2 = Bytes::from_static(b"v2");
        assert_eq!(
            client_a
                .cas_ref("op_heads", Some(&v0), Some(&v2))
                .await
                .unwrap(),
            CasOutcome::Conflict {
                actual: Some(v1.clone()),
            }
        );
    }

    /// Sanity: put_blob with an empty id is rejected on the server side,
    /// surfaced to the client as an `Err` rather than silently accepted.
    #[tokio::test]
    async fn server_rejects_empty_id() {
        let (endpoint, _backing) = spawn_server().await;
        let client = GrpcRemoteStore::new(&endpoint).unwrap();
        let mut raw = client.client.clone();
        let err = raw
            .put_blob(PutBlobReq {
                kind: BlobKind::File.as_proto() as i32,
                id: vec![],
                bytes: vec![],
            })
            .await
            .expect_err("server must reject empty id");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
