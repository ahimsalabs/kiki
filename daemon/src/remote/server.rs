//! Daemon-side gRPC server for `service RemoteStore` (PLAN.md §13.6).
//!
//! Wraps any [`RemoteStore`] backend (filesystem, in-memory, future
//! S3...) behind the unary RPCs declared in
//! `proto/jj_interface.proto`. Same daemon binary serves both
//! `JujutsuInterface` and `RemoteStore` on the same gRPC listener, so
//! peer daemons can use each other as remotes with no auxiliary
//! transport.
//!
//! Auth/permissions: none in M9. Same trust model as the existing
//! `JujutsuInterface` server — single-user, localhost. TLS + auth land
//! in M11 alongside S3.

use std::sync::Arc;

use proto::jj_interface::remote_store_server::{
    RemoteStore as RemoteStoreServerTrait, RemoteStoreServer,
};
use proto::jj_interface::{
    BlobKind as ProtoBlobKind, CasRefReply, CasRefReq, GetBlobReply, GetBlobReq, GetRefReply,
    GetRefReq, HasBlobReply, HasBlobReq, ListRefsReply, ListRefsReq, PutBlobReply, PutBlobReq,
};
use tonic::{Request, Response, Status};

use super::{BlobKind, RemoteStore};
use crate::ty::Id;

/// Adapter from the `proto::remote_store_server::RemoteStore` trait to
/// any [`crate::remote::RemoteStore`] backend.
///
/// Cheap to clone: backend is `Arc`-shared. The server's listener
/// thread fans request handling out across the runtime; backend
/// implementations must be `Send + Sync` (the trait already requires
/// it) and tolerate concurrent calls. `FsRemoteStore`'s `tokio::
/// task::spawn_blocking` shims handle this trivially; in-memory
/// backends should rely on their internal `parking_lot::Mutex` or
/// equivalent.
pub struct RemoteStoreService {
    backend: Arc<dyn RemoteStore>,
}

impl RemoteStoreService {
    pub fn new(backend: Arc<dyn RemoteStore>) -> Self {
        RemoteStoreService { backend }
    }

    /// Wrap in the tonic-generated server type.
    pub fn into_server(self) -> RemoteStoreServer<Self> {
        RemoteStoreServer::new(self)
    }
}

/// Decode the `(kind, id)` pair sent on the wire into typed values.
/// `BlobKind::Unspecified` (the protobuf3 default) is rejected — silent
/// fallback would route blobs to a "default" table that doesn't exist.
///
/// The `Result<_, Status>` shape on these helpers is intentionally
/// imbalanced (Status is 176 bytes; the Ok side is small). The lint
/// can't help here — we want the callers to `?`-propagate without
/// boxing — so it's allow-listed at the helper level rather than
/// papered over with an actual `Box<Status>`.
#[allow(clippy::result_large_err)]
fn decode_kind(kind: i32) -> Result<BlobKind, Status> {
    let proto = ProtoBlobKind::try_from(kind)
        .map_err(|e| Status::invalid_argument(format!("invalid BlobKind: {e}")))?;
    BlobKind::from_proto(proto).ok_or_else(|| {
        Status::invalid_argument("BlobKind unspecified (zero) is not a valid kind")
    })
}

#[allow(clippy::result_large_err)]
fn decode_id(id: Vec<u8>) -> Result<Id, Status> {
    id.try_into()
        .map_err(|e| Status::invalid_argument(format!("blob id: {e:#}")))
}

#[tonic::async_trait]
impl RemoteStoreServerTrait for RemoteStoreService {
    #[tracing::instrument(skip(self, request), fields(endpoint = "RemoteStore.GetBlob"))]
    async fn get_blob(
        &self,
        request: Request<GetBlobReq>,
    ) -> Result<Response<GetBlobReply>, Status> {
        let req = request.into_inner();
        let kind = decode_kind(req.kind)?;
        let id = decode_id(req.id)?;
        let bytes = self
            .backend
            .get_blob(kind, &id)
            .await
            .map_err(|e| Status::internal(format!("backend get_blob: {e:#}")))?;
        // `found` distinguishes "remote doesn't have it" from "remote
        // has an empty blob"; preserve the distinction across the wire.
        Ok(Response::new(match bytes {
            Some(b) => GetBlobReply {
                found: true,
                bytes: b.to_vec(),
            },
            None => GetBlobReply {
                found: false,
                bytes: Vec::new(),
            },
        }))
    }

    #[tracing::instrument(skip(self, request), fields(endpoint = "RemoteStore.PutBlob"))]
    async fn put_blob(
        &self,
        request: Request<PutBlobReq>,
    ) -> Result<Response<PutBlobReply>, Status> {
        let req = request.into_inner();
        let kind = decode_kind(req.kind)?;
        let id = decode_id(req.id)?;
        self.backend
            .put_blob(kind, &id, bytes::Bytes::from(req.bytes))
            .await
            .map_err(|e| Status::internal(format!("backend put_blob: {e:#}")))?;
        Ok(Response::new(PutBlobReply {}))
    }

    #[tracing::instrument(skip(self, request), fields(endpoint = "RemoteStore.HasBlob"))]
    async fn has_blob(
        &self,
        request: Request<HasBlobReq>,
    ) -> Result<Response<HasBlobReply>, Status> {
        let req = request.into_inner();
        let kind = decode_kind(req.kind)?;
        let id = decode_id(req.id)?;
        let found = self
            .backend
            .has_blob(kind, &id)
            .await
            .map_err(|e| Status::internal(format!("backend has_blob: {e:#}")))?;
        Ok(Response::new(HasBlobReply { found }))
    }

    #[tracing::instrument(skip(self, request), fields(endpoint = "RemoteStore.GetRef"))]
    async fn get_ref(
        &self,
        request: Request<GetRefReq>,
    ) -> Result<Response<GetRefReply>, Status> {
        let req = request.into_inner();
        // Defensive validation at the wire boundary even though the
        // backend re-checks. Keeps the server-side error a clean
        // `invalid_argument` rather than letting an internal-error
        // shape leak out for malformed names.
        super::validate_ref_name(&req.name)
            .map_err(|e| Status::invalid_argument(format!("ref name: {e:#}")))?;
        let value = self
            .backend
            .get_ref(&req.name)
            .await
            .map_err(|e| Status::internal(format!("backend get_ref: {e:#}")))?;
        Ok(Response::new(match value {
            Some(b) => GetRefReply {
                found: true,
                value: b.to_vec(),
            },
            None => GetRefReply {
                found: false,
                value: Vec::new(),
            },
        }))
    }

    #[tracing::instrument(skip(self, request), fields(endpoint = "RemoteStore.CasRef"))]
    async fn cas_ref(
        &self,
        request: Request<CasRefReq>,
    ) -> Result<Response<CasRefReply>, Status> {
        let req = request.into_inner();
        super::validate_ref_name(&req.name)
            .map_err(|e| Status::invalid_argument(format!("ref name: {e:#}")))?;
        // proto3 `optional bytes` decodes to `Option<Vec<u8>>`; lift to
        // `Option<Bytes>` so the trait sees the same semantics on both
        // sides of the wire.
        let expected = req.expected.map(bytes::Bytes::from);
        let new = req.new.map(bytes::Bytes::from);
        let outcome = self
            .backend
            .cas_ref(&req.name, expected.as_ref(), new.as_ref())
            .await
            .map_err(|e| Status::internal(format!("backend cas_ref: {e:#}")))?;
        Ok(Response::new(match outcome {
            super::CasOutcome::Updated => CasRefReply {
                updated: true,
                actual: None,
            },
            super::CasOutcome::Conflict { actual } => CasRefReply {
                updated: false,
                actual: actual.map(|b| b.to_vec()),
            },
        }))
    }

    #[tracing::instrument(skip(self, _request), fields(endpoint = "RemoteStore.ListRefs"))]
    async fn list_refs(
        &self,
        _request: Request<ListRefsReq>,
    ) -> Result<Response<ListRefsReply>, Status> {
        let names = self
            .backend
            .list_refs()
            .await
            .map_err(|e| Status::internal(format!("backend list_refs: {e:#}")))?;
        Ok(Response::new(ListRefsReply { names }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::fs::FsRemoteStore;
    use super::*;
    use bytes::Bytes;

    fn id_of(b: u8) -> Id {
        Id([b; 32])
    }

    fn service_with_tempdir() -> (tempfile::TempDir, RemoteStoreService) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FsRemoteStore::new(dir.path().to_owned())) as Arc<dyn RemoteStore>;
        (dir, RemoteStoreService::new(backend))
    }

    #[tokio::test]
    async fn put_then_get_round_trip() {
        let (_dir, svc) = service_with_tempdir();
        let id = id_of(0x42);
        svc.put_blob(Request::new(PutBlobReq {
            kind: ProtoBlobKind::File as i32,
            id: id.0.to_vec(),
            bytes: b"server-side-hello".to_vec(),
        }))
        .await
        .expect("put_blob");

        let resp = svc
            .get_blob(Request::new(GetBlobReq {
                kind: ProtoBlobKind::File as i32,
                id: id.0.to_vec(),
            }))
            .await
            .expect("get_blob")
            .into_inner();
        assert!(resp.found);
        assert_eq!(resp.bytes, b"server-side-hello");
    }

    #[tokio::test]
    async fn missing_blob_returns_found_false() {
        let (_dir, svc) = service_with_tempdir();
        let resp = svc
            .get_blob(Request::new(GetBlobReq {
                kind: ProtoBlobKind::Tree as i32,
                id: id_of(0).0.to_vec(),
            }))
            .await
            .expect("get_blob")
            .into_inner();
        assert!(!resp.found);
        assert!(resp.bytes.is_empty());
    }

    #[tokio::test]
    async fn unspecified_kind_rejected() {
        let (_dir, svc) = service_with_tempdir();
        let err = svc
            .get_blob(Request::new(GetBlobReq {
                kind: ProtoBlobKind::Unspecified as i32,
                id: id_of(1).0.to_vec(),
            }))
            .await
            .expect_err("unspecified kind must error");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn short_id_rejected() {
        let (_dir, svc) = service_with_tempdir();
        let err = svc
            .has_blob(Request::new(HasBlobReq {
                kind: ProtoBlobKind::Tree as i32,
                id: vec![0u8; 16], // wrong length
            }))
            .await
            .expect_err("short id must error");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn has_blob_tracks_state() {
        let (_dir, svc) = service_with_tempdir();
        let id = id_of(7);
        let probe = || async {
            svc.has_blob(Request::new(HasBlobReq {
                kind: ProtoBlobKind::Symlink as i32,
                id: id.0.to_vec(),
            }))
            .await
            .unwrap()
            .into_inner()
            .found
        };
        assert!(!probe().await);
        svc.put_blob(Request::new(PutBlobReq {
            kind: ProtoBlobKind::Symlink as i32,
            id: id.0.to_vec(),
            bytes: Bytes::from_static(b"x").to_vec(),
        }))
        .await
        .unwrap();
        assert!(probe().await);
    }

    // ---- M10: ref RPCs --------------------------------------------------

    #[tokio::test]
    async fn ref_round_trip() {
        let (_dir, svc) = service_with_tempdir();
        // Missing ref → found=false.
        let resp = svc
            .get_ref(Request::new(GetRefReq {
                name: "head".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!resp.found);

        // CAS create-only.
        let cas = svc
            .cas_ref(Request::new(CasRefReq {
                name: "head".into(),
                expected: None,
                new: Some(b"v0".to_vec()),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(cas.updated);
        assert!(cas.actual.is_none());

        // GetRef now sees v0.
        let resp = svc
            .get_ref(Request::new(GetRefReq {
                name: "head".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.found);
        assert_eq!(resp.value, b"v0");
    }

    #[tokio::test]
    async fn cas_ref_conflict_carries_actual() {
        let (_dir, svc) = service_with_tempdir();
        svc.cas_ref(Request::new(CasRefReq {
            name: "op_heads".into(),
            expected: None,
            new: Some(b"v0".to_vec()),
        }))
        .await
        .unwrap();
        // Stale precondition: caller expects empty bytes, but server
        // has v0. The reply must carry v0 so the caller can retry
        // without a follow-up GetRef round-trip.
        let cas = svc
            .cas_ref(Request::new(CasRefReq {
                name: "op_heads".into(),
                expected: Some(Vec::new()),
                new: Some(b"v1".to_vec()),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!cas.updated);
        assert_eq!(cas.actual.as_deref(), Some(b"v0".as_ref()));
    }

    #[tokio::test]
    async fn cas_ref_create_only_against_existing_conflicts() {
        // proto3 `optional` distinguishes "field absent" from "field
        // present but empty". The server side must preserve that
        // distinction: `expected = None` (field absent) must conflict
        // when the ref exists, even with empty value.
        let (_dir, svc) = service_with_tempdir();
        svc.cas_ref(Request::new(CasRefReq {
            name: "e".into(),
            expected: None,
            new: Some(Vec::new()), // present-but-empty value
        }))
        .await
        .unwrap();
        let cas = svc
            .cas_ref(Request::new(CasRefReq {
                name: "e".into(),
                expected: None, // create-only
                new: Some(b"x".to_vec()),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!cas.updated);
        // `actual` is Some(empty bytes) — distinct from None.
        assert_eq!(cas.actual.as_deref(), Some(b"".as_ref()));
    }

    #[tokio::test]
    async fn list_refs_round_trip() {
        let (_dir, svc) = service_with_tempdir();
        for name in ["zeta", "alpha", "head"] {
            svc.cas_ref(Request::new(CasRefReq {
                name: name.into(),
                expected: None,
                new: Some(b"v".to_vec()),
            }))
            .await
            .unwrap();
        }
        let resp = svc
            .list_refs(Request::new(ListRefsReq {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.names, vec!["alpha", "head", "zeta"]);
    }

    #[tokio::test]
    async fn ref_rpcs_reject_bad_name() {
        let (_dir, svc) = service_with_tempdir();
        // The server validates names defensively at the wire boundary
        // — backends do too, but the wire-side error should be
        // invalid_argument, not internal.
        for bad in ["", "a/b", "..", "a\0b"] {
            let err = svc
                .get_ref(Request::new(GetRefReq {
                    name: bad.into(),
                }))
                .await
                .expect_err(bad);
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            let err = svc
                .cas_ref(Request::new(CasRefReq {
                    name: bad.into(),
                    expected: None,
                    new: Some(b"x".to_vec()),
                }))
                .await
                .expect_err(bad);
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }
    }
}
