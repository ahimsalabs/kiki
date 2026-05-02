//! End-to-end test for `kiki kk serve <path>`.
//!
//! Spawns the real `kiki` binary with stdin/stdout piped, then exercises
//! the length-prefixed protobuf framing protocol: PutBlob, GetBlob,
//! HasBlob, CasRef, GetRef, and ListRefs.

use std::path::PathBuf;

use store::framing::{read_frame, write_frame};
use tokio::process::Command;

/// Locate the `kiki` binary. It lives next to the test binary in
/// `target/debug/` (or `target/release/` under `--release`).
fn kiki_bin() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    // test binary is e.g. target/debug/deps/e2e_serve-<hash>
    path.pop(); // remove the binary name
    if path.ends_with("deps") {
        path.pop(); // step out of deps/
    }
    path.push("kiki");
    assert!(
        path.exists(),
        "kiki binary not found at {}; run `cargo build -p kiki` first",
        path.display()
    );
    path
}

/// Helper: build a StoreRequest wrapping a PutBlob.
fn put_blob_request(
    id_num: u64,
    kind: i32,
    blob_id: Vec<u8>,
    data: Vec<u8>,
) -> proto::jj_interface::StoreRequest {
    use proto::jj_interface::*;
    StoreRequest {
        id: id_num,
        request: Some(store_request::Request::PutBlob(PutBlobReq {
            kind,
            id: blob_id,
            bytes: data,
        })),
    }
}

/// Helper: build a StoreRequest wrapping a GetBlob.
fn get_blob_request(
    id_num: u64,
    kind: i32,
    blob_id: Vec<u8>,
) -> proto::jj_interface::StoreRequest {
    use proto::jj_interface::*;
    StoreRequest {
        id: id_num,
        request: Some(store_request::Request::GetBlob(GetBlobReq {
            kind,
            id: blob_id,
        })),
    }
}

/// Helper: build a StoreRequest wrapping a HasBlob.
fn has_blob_request(
    id_num: u64,
    kind: i32,
    blob_id: Vec<u8>,
) -> proto::jj_interface::StoreRequest {
    use proto::jj_interface::*;
    StoreRequest {
        id: id_num,
        request: Some(store_request::Request::HasBlob(HasBlobReq {
            kind,
            id: blob_id,
        })),
    }
}

/// Helper: build a StoreRequest wrapping a CasRef (create).
fn cas_ref_create_request(
    id_num: u64,
    name: &str,
    value: Vec<u8>,
) -> proto::jj_interface::StoreRequest {
    use proto::jj_interface::*;
    StoreRequest {
        id: id_num,
        request: Some(store_request::Request::CasRef(CasRefReq {
            name: name.to_owned(),
            expected: None,        // create-only: must not exist
            new: Some(value),      // set to this value
        })),
    }
}

/// Helper: build a StoreRequest wrapping a GetRef.
fn get_ref_request(
    id_num: u64,
    name: &str,
) -> proto::jj_interface::StoreRequest {
    use proto::jj_interface::*;
    StoreRequest {
        id: id_num,
        request: Some(store_request::Request::GetRef(GetRefReq {
            name: name.to_owned(),
        })),
    }
}

/// Helper: build a StoreRequest wrapping a ListRefs.
fn list_refs_request(id_num: u64) -> proto::jj_interface::StoreRequest {
    use proto::jj_interface::*;
    StoreRequest {
        id: id_num,
        request: Some(store_request::Request::ListRefs(ListRefsReq {})),
    }
}

#[tokio::test]
async fn e2e_blob_round_trip() {
    use proto::jj_interface::*;

    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().to_str().unwrap();

    let mut child = Command::new(kiki_bin())
        .args(["kk", "serve", store_path])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn kiki kk serve");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    let blob_id = vec![0xAB_u8; 32];
    let blob_data = b"hello from e2e test".to_vec();

    // 1. PutBlob
    let put_req = put_blob_request(1, BlobKind::Blob as i32, blob_id.clone(), blob_data.clone());
    write_frame(&mut stdin, &put_req).await.unwrap();

    let resp1: StoreResponse = read_frame(&mut stdout)
        .await
        .unwrap()
        .expect("expected PutBlob response, got EOF");
    assert_eq!(resp1.id, 1);
    assert!(
        matches!(resp1.response, Some(store_response::Response::PutBlob(_))),
        "expected PutBlob response, got {:?}",
        resp1.response
    );

    // 2. GetBlob — should return the data we just put
    let get_req = get_blob_request(2, BlobKind::Blob as i32, blob_id.clone());
    write_frame(&mut stdin, &get_req).await.unwrap();

    let resp2: StoreResponse = read_frame(&mut stdout)
        .await
        .unwrap()
        .expect("expected GetBlob response, got EOF");
    assert_eq!(resp2.id, 2);
    match resp2.response {
        Some(store_response::Response::GetBlob(reply)) => {
            assert!(reply.found, "GetBlob should find the blob");
            assert_eq!(reply.bytes, blob_data, "GetBlob data mismatch");
        }
        other => panic!("expected GetBlob response, got {other:?}"),
    }

    // 3. HasBlob — should be true
    let has_req = has_blob_request(3, BlobKind::Blob as i32, blob_id.clone());
    write_frame(&mut stdin, &has_req).await.unwrap();

    let resp3: StoreResponse = read_frame(&mut stdout)
        .await
        .unwrap()
        .expect("expected HasBlob response, got EOF");
    assert_eq!(resp3.id, 3);
    match resp3.response {
        Some(store_response::Response::HasBlob(reply)) => {
            assert!(reply.found, "HasBlob should be true after PutBlob");
        }
        other => panic!("expected HasBlob response, got {other:?}"),
    }

    // 4. HasBlob for a missing blob — should be false
    let missing_id = vec![0x00_u8; 32];
    let has_missing = has_blob_request(4, BlobKind::Blob as i32, missing_id);
    write_frame(&mut stdin, &has_missing).await.unwrap();

    let resp4: StoreResponse = read_frame(&mut stdout)
        .await
        .unwrap()
        .expect("expected HasBlob response, got EOF");
    assert_eq!(resp4.id, 4);
    match resp4.response {
        Some(store_response::Response::HasBlob(reply)) => {
            assert!(!reply.found, "HasBlob should be false for missing blob");
        }
        other => panic!("expected HasBlob response, got {other:?}"),
    }

    // 5. GetBlob for missing blob — found=false
    let get_missing = get_blob_request(5, BlobKind::Tree as i32, vec![0x01; 32]);
    write_frame(&mut stdin, &get_missing).await.unwrap();

    let resp5: StoreResponse = read_frame(&mut stdout)
        .await
        .unwrap()
        .expect("expected GetBlob response, got EOF");
    assert_eq!(resp5.id, 5);
    match resp5.response {
        Some(store_response::Response::GetBlob(reply)) => {
            assert!(!reply.found, "GetBlob for missing blob should have found=false");
        }
        other => panic!("expected GetBlob response, got {other:?}"),
    }

    // Close stdin to signal EOF; the serve loop should exit cleanly.
    drop(stdin);
    let status = child.wait().await.unwrap();
    assert!(status.success(), "kiki kk serve exited with: {status}");
}

#[tokio::test]
async fn e2e_refs_round_trip() {
    use proto::jj_interface::*;

    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().to_str().unwrap();

    let mut child = Command::new(kiki_bin())
        .args(["kk", "serve", store_path])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn kiki kk serve");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    // 1. GetRef on a non-existent ref — found=false
    let req1 = get_ref_request(1, "op_heads");
    write_frame(&mut stdin, &req1).await.unwrap();

    let resp1: StoreResponse = read_frame(&mut stdout)
        .await
        .unwrap()
        .expect("expected GetRef response");
    assert_eq!(resp1.id, 1);
    match resp1.response {
        Some(store_response::Response::GetRef(reply)) => {
            assert!(!reply.found, "GetRef should be false for missing ref");
        }
        other => panic!("expected GetRef, got {other:?}"),
    }

    // 2. CasRef create — expected=None, new=Some(value)
    let ref_value = b"op-id-bytes-1234".to_vec();
    let req2 = cas_ref_create_request(2, "op_heads", ref_value.clone());
    write_frame(&mut stdin, &req2).await.unwrap();

    let resp2: StoreResponse = read_frame(&mut stdout)
        .await
        .unwrap()
        .expect("expected CasRef response");
    assert_eq!(resp2.id, 2);
    match resp2.response {
        Some(store_response::Response::CasRef(reply)) => {
            assert!(reply.updated, "CasRef create should succeed");
        }
        other => panic!("expected CasRef, got {other:?}"),
    }

    // 3. GetRef — should now find the ref
    let req3 = get_ref_request(3, "op_heads");
    write_frame(&mut stdin, &req3).await.unwrap();

    let resp3: StoreResponse = read_frame(&mut stdout)
        .await
        .unwrap()
        .expect("expected GetRef response");
    assert_eq!(resp3.id, 3);
    match resp3.response {
        Some(store_response::Response::GetRef(reply)) => {
            assert!(reply.found, "GetRef should find the ref after CasRef create");
            assert_eq!(reply.value, ref_value, "GetRef value mismatch");
        }
        other => panic!("expected GetRef, got {other:?}"),
    }

    // 4. CasRef create-only conflict — ref already exists
    let req4 = cas_ref_create_request(4, "op_heads", b"new-value".to_vec());
    write_frame(&mut stdin, &req4).await.unwrap();

    let resp4: StoreResponse = read_frame(&mut stdout)
        .await
        .unwrap()
        .expect("expected CasRef response");
    assert_eq!(resp4.id, 4);
    match resp4.response {
        Some(store_response::Response::CasRef(reply)) => {
            assert!(!reply.updated, "CasRef should conflict when ref exists");
            assert_eq!(
                reply.actual.as_deref(),
                Some(ref_value.as_slice()),
                "conflict should return actual value"
            );
        }
        other => panic!("expected CasRef, got {other:?}"),
    }

    // 5. Create a second ref for ListRefs
    let req5 = cas_ref_create_request(5, "head", b"head-value".to_vec());
    write_frame(&mut stdin, &req5).await.unwrap();

    let resp5: StoreResponse = read_frame(&mut stdout)
        .await
        .unwrap()
        .expect("expected CasRef response");
    assert_eq!(resp5.id, 5);
    match resp5.response {
        Some(store_response::Response::CasRef(reply)) => {
            assert!(reply.updated, "CasRef create for 'head' should succeed");
        }
        other => panic!("expected CasRef, got {other:?}"),
    }

    // 6. ListRefs — should return both refs sorted
    let req6 = list_refs_request(6);
    write_frame(&mut stdin, &req6).await.unwrap();

    let resp6: StoreResponse = read_frame(&mut stdout)
        .await
        .unwrap()
        .expect("expected ListRefs response");
    assert_eq!(resp6.id, 6);
    match resp6.response {
        Some(store_response::Response::ListRefs(reply)) => {
            assert_eq!(
                reply.names,
                vec!["head", "op_heads"],
                "ListRefs should return sorted ref names"
            );
        }
        other => panic!("expected ListRefs, got {other:?}"),
    }

    drop(stdin);
    let status = child.wait().await.unwrap();
    assert!(status.success(), "kiki kk serve exited with: {status}");
}

#[tokio::test]
async fn e2e_multiple_blob_kinds() {
    use proto::jj_interface::*;

    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().to_str().unwrap();

    let mut child = Command::new(kiki_bin())
        .args(["kk", "serve", store_path])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn kiki kk serve");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    let blob_id = vec![0xCC_u8; 32];

    // Put as Blob kind
    let req1 = put_blob_request(1, BlobKind::Blob as i32, blob_id.clone(), b"blob-data".to_vec());
    write_frame(&mut stdin, &req1).await.unwrap();
    let _: StoreResponse = read_frame(&mut stdout).await.unwrap().unwrap();

    // Put same id as Tree kind with different data
    let req2 = put_blob_request(2, BlobKind::Tree as i32, blob_id.clone(), b"tree-data".to_vec());
    write_frame(&mut stdin, &req2).await.unwrap();
    let _: StoreResponse = read_frame(&mut stdout).await.unwrap().unwrap();

    // Get Blob kind — should return blob-data
    let req3 = get_blob_request(3, BlobKind::Blob as i32, blob_id.clone());
    write_frame(&mut stdin, &req3).await.unwrap();
    let resp3: StoreResponse = read_frame(&mut stdout).await.unwrap().unwrap();
    match resp3.response {
        Some(store_response::Response::GetBlob(reply)) => {
            assert!(reply.found);
            assert_eq!(reply.bytes, b"blob-data");
        }
        other => panic!("expected GetBlob, got {other:?}"),
    }

    // Get Tree kind — should return tree-data (distinct keyspace)
    let req4 = get_blob_request(4, BlobKind::Tree as i32, blob_id.clone());
    write_frame(&mut stdin, &req4).await.unwrap();
    let resp4: StoreResponse = read_frame(&mut stdout).await.unwrap().unwrap();
    match resp4.response {
        Some(store_response::Response::GetBlob(reply)) => {
            assert!(reply.found);
            assert_eq!(reply.bytes, b"tree-data");
        }
        other => panic!("expected GetBlob, got {other:?}"),
    }

    // HasBlob for Commit kind (not put) — should be false
    let req5 = has_blob_request(5, BlobKind::Commit as i32, blob_id.clone());
    write_frame(&mut stdin, &req5).await.unwrap();
    let resp5: StoreResponse = read_frame(&mut stdout).await.unwrap().unwrap();
    match resp5.response {
        Some(store_response::Response::HasBlob(reply)) => {
            assert!(!reply.found, "Commit kind was never put");
        }
        other => panic!("expected HasBlob, got {other:?}"),
    }

    drop(stdin);
    let status = child.wait().await.unwrap();
    assert!(status.success(), "kiki kk serve exited with: {status}");
}
