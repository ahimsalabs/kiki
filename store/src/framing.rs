//! Length-prefixed protobuf framing for the stdin/stdout store
//! protocol (SSH transport).
//!
//! Frame format: `[4-byte big-endian length][protobuf bytes]`.
//! The protocol is sequential: client sends one `StoreRequest`, waits
//! for one `StoreResponse`, repeats. The `id` field is echoed back
//! for future pipelining but is not required for correctness today.

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{BlobKind, CasOutcome, RemoteStore};

/// Safety limit: reject frames larger than 64 MiB to protect against
/// malformed input on the pipe.
const MAX_FRAME_SIZE: u32 = 64 * 1024 * 1024;

/// Write a length-prefixed protobuf message.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: &impl Message,
) -> Result<()> {
    let len = msg.encoded_len() as u32;
    writer
        .write_all(&len.to_be_bytes())
        .await
        .context("writing frame length")?;
    let mut buf = Vec::with_capacity(len as usize);
    msg.encode(&mut buf).context("encoding protobuf")?;
    writer
        .write_all(&buf)
        .await
        .context("writing frame body")?;
    writer.flush().await.context("flushing frame")?;
    Ok(())
}

/// Read a length-prefixed protobuf message. Returns `None` on clean
/// EOF (peer closed the pipe).
pub async fn read_frame<R: AsyncReadExt + Unpin, M: Message + Default>(
    reader: &mut R,
) -> Result<Option<M>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("reading frame length"),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_SIZE {
        return Err(anyhow!(
            "frame size {len} exceeds maximum {MAX_FRAME_SIZE}"
        ));
    }
    let mut buf = vec![0u8; len as usize];
    reader
        .read_exact(&mut buf)
        .await
        .context("reading frame body")?;
    let msg = M::decode(&buf[..]).context("decoding protobuf")?;
    Ok(Some(msg))
}

/// Serve loop: read `StoreRequest` messages from `reader`, dispatch
/// to the given `RemoteStore`, write `StoreResponse` messages to
/// `writer`. Returns when the reader hits EOF (client disconnected).
pub async fn serve_store_loop<R, W>(
    mut reader: R,
    mut writer: W,
    store: &dyn RemoteStore,
) -> Result<()>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    use proto::jj_interface::store_request::Request;
    use proto::jj_interface::store_response::Response;
    use proto::jj_interface::*;

    loop {
        let req: StoreRequest = match read_frame(&mut reader).await? {
            Some(r) => r,
            None => return Ok(()), // clean EOF
        };

        let id = req.id;
        let response = match req.request {
            Some(Request::GetBlob(r)) => {
                match dispatch_get_blob(store, r).await {
                    Ok(reply) => Response::GetBlob(reply),
                    Err(e) => Response::Error(StoreError { message: format!("{e:#}") }),
                }
            }
            Some(Request::PutBlob(r)) => {
                match dispatch_put_blob(store, r).await {
                    Ok(reply) => Response::PutBlob(reply),
                    Err(e) => Response::Error(StoreError { message: format!("{e:#}") }),
                }
            }
            Some(Request::HasBlob(r)) => {
                match dispatch_has_blob(store, r).await {
                    Ok(reply) => Response::HasBlob(reply),
                    Err(e) => Response::Error(StoreError { message: format!("{e:#}") }),
                }
            }
            Some(Request::GetRef(r)) => {
                match dispatch_get_ref(store, r).await {
                    Ok(reply) => Response::GetRef(reply),
                    Err(e) => Response::Error(StoreError { message: format!("{e:#}") }),
                }
            }
            Some(Request::CasRef(r)) => {
                match dispatch_cas_ref(store, r).await {
                    Ok(reply) => Response::CasRef(reply),
                    Err(e) => Response::Error(StoreError { message: format!("{e:#}") }),
                }
            }
            Some(Request::ListRefs(r)) => {
                match dispatch_list_refs(store, r).await {
                    Ok(reply) => Response::ListRefs(reply),
                    Err(e) => Response::Error(StoreError { message: format!("{e:#}") }),
                }
            }
            None => {
                Response::Error(StoreError {
                    message: "empty request (no oneof variant set)".into(),
                })
            }
        };

        let resp = StoreResponse {
            id,
            response: Some(response),
        };
        write_frame(&mut writer, &resp).await?;
    }
}

// ---- dispatch helpers ----
// These mirror the gRPC server adapter in daemon/src/remote/server.rs
// but without tonic types.

fn decode_kind(kind: i32) -> Result<BlobKind> {
    let proto = proto::jj_interface::BlobKind::try_from(kind)
        .map_err(|e| anyhow!("invalid BlobKind: {e}"))?;
    BlobKind::from_proto(proto)
        .ok_or_else(|| anyhow!("BlobKind unspecified (zero) is not a valid kind"))
}

fn validate_id(id: &[u8]) -> Result<()> {
    if id.is_empty() {
        return Err(anyhow!("blob id must not be empty"));
    }
    Ok(())
}

async fn dispatch_get_blob(
    store: &dyn RemoteStore,
    req: proto::jj_interface::GetBlobReq,
) -> Result<proto::jj_interface::GetBlobReply> {
    let kind = decode_kind(req.kind)?;
    validate_id(&req.id)?;
    let bytes = store.get_blob(kind, &req.id).await?;
    Ok(match bytes {
        Some(b) => proto::jj_interface::GetBlobReply {
            found: true,
            bytes: b.to_vec(),
        },
        None => proto::jj_interface::GetBlobReply {
            found: false,
            bytes: Vec::new(),
        },
    })
}

async fn dispatch_put_blob(
    store: &dyn RemoteStore,
    req: proto::jj_interface::PutBlobReq,
) -> Result<proto::jj_interface::PutBlobReply> {
    let kind = decode_kind(req.kind)?;
    validate_id(&req.id)?;
    store
        .put_blob(kind, &req.id, Bytes::from(req.bytes))
        .await?;
    Ok(proto::jj_interface::PutBlobReply {})
}

async fn dispatch_has_blob(
    store: &dyn RemoteStore,
    req: proto::jj_interface::HasBlobReq,
) -> Result<proto::jj_interface::HasBlobReply> {
    let kind = decode_kind(req.kind)?;
    validate_id(&req.id)?;
    let found = store.has_blob(kind, &req.id).await?;
    Ok(proto::jj_interface::HasBlobReply { found })
}

async fn dispatch_get_ref(
    store: &dyn RemoteStore,
    req: proto::jj_interface::GetRefReq,
) -> Result<proto::jj_interface::GetRefReply> {
    crate::validate_ref_name(&req.name)?;
    let value = store.get_ref(&req.name).await?;
    Ok(match value {
        Some(b) => proto::jj_interface::GetRefReply {
            found: true,
            value: b.to_vec(),
        },
        None => proto::jj_interface::GetRefReply {
            found: false,
            value: Vec::new(),
        },
    })
}

async fn dispatch_cas_ref(
    store: &dyn RemoteStore,
    req: proto::jj_interface::CasRefReq,
) -> Result<proto::jj_interface::CasRefReply> {
    crate::validate_ref_name(&req.name)?;
    let expected = req.expected.map(Bytes::from);
    let new = req.new.map(Bytes::from);
    let outcome = store
        .cas_ref(&req.name, expected.as_ref(), new.as_ref())
        .await?;
    Ok(match outcome {
        CasOutcome::Updated => proto::jj_interface::CasRefReply {
            updated: true,
            actual: None,
        },
        CasOutcome::Conflict { actual } => proto::jj_interface::CasRefReply {
            updated: false,
            actual: actual.map(|b| b.to_vec()),
        },
    })
}

async fn dispatch_list_refs(
    store: &dyn RemoteStore,
    _req: proto::jj_interface::ListRefsReq,
) -> Result<proto::jj_interface::ListRefsReply> {
    let names = store.list_refs().await?;
    Ok(proto::jj_interface::ListRefsReply { names })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FsRemoteStore;

    #[tokio::test]
    async fn frame_round_trip() {
        use proto::jj_interface::*;

        let req = StoreRequest {
            id: 42,
            request: Some(store_request::Request::HasBlob(HasBlobReq {
                kind: proto::jj_interface::BlobKind::Blob as i32,
                id: vec![0xab; 32],
            })),
        };

        let mut buf = Vec::new();
        write_frame(&mut buf, &req).await.unwrap();

        let mut cursor = &buf[..];
        let decoded: StoreRequest = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(decoded.id, 42);
    }

    #[tokio::test]
    async fn eof_returns_none() {
        let empty: &[u8] = &[];
        let result: Option<proto::jj_interface::StoreRequest> =
            read_frame(&mut &*empty).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn serve_loop_blob_round_trip() {
        use proto::jj_interface::*;

        let dir = tempfile::tempdir().unwrap();
        let store = FsRemoteStore::new(dir.path().to_owned());

        // Build requests: put then get.
        let id = vec![0x42u8; 32];
        let put_req = StoreRequest {
            id: 1,
            request: Some(store_request::Request::PutBlob(PutBlobReq {
                kind: proto::jj_interface::BlobKind::Blob as i32,
                id: id.clone(),
                bytes: b"hello-ssh".to_vec(),
            })),
        };
        let get_req = StoreRequest {
            id: 2,
            request: Some(store_request::Request::GetBlob(GetBlobReq {
                kind: proto::jj_interface::BlobKind::Blob as i32,
                id: id.clone(),
            })),
        };

        // Serialize requests into a pipe buffer.
        let mut input = Vec::new();
        write_frame(&mut input, &put_req).await.unwrap();
        write_frame(&mut input, &get_req).await.unwrap();

        // Run the serve loop.
        let mut output = Vec::new();
        serve_store_loop(&input[..], &mut output, &store)
            .await
            .unwrap();

        // Decode responses.
        let mut cursor = &output[..];
        let resp1: StoreResponse = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(resp1.id, 1);
        assert!(matches!(
            resp1.response,
            Some(store_response::Response::PutBlob(_))
        ));

        let resp2: StoreResponse = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(resp2.id, 2);
        match resp2.response {
            Some(store_response::Response::GetBlob(reply)) => {
                assert!(reply.found);
                assert_eq!(reply.bytes, b"hello-ssh");
            }
            other => panic!("expected GetBlob response, got {other:?}"),
        }
    }
}
