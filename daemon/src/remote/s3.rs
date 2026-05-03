//! S3-backed `RemoteStore` (`s3://` scheme).
//!
//! Blobs land at `<prefix>/<kind>/<hex(id)>`. Refs live at
//! `<prefix>/refs/<name>`. The layout mirrors `FsRemoteStore` so a
//! human can browse the bucket with `aws s3 ls` and make sense of it.
//!
//! ## CAS on refs
//!
//! S3 supports conditional writes via `If-None-Match: *` (create-only)
//! and `If-Match: <etag>` (update-only, requires the current ETag).
//! We use these to implement compare-and-swap without an external lock
//! service. This requires an S3-compatible backend that supports
//! conditional PutObject — AWS S3 (since late 2024), R2, Tigris, MinIO
//! (recent builds) all qualify.
//!
//! ## Credentials
//!
//! The AWS SDK's default credential chain is used: env vars
//! (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`), `~/.aws/credentials`,
//! IMDS, ECS task role, etc. No kiki-specific config needed.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use aws_sdk_s3::{
    Client,
    error::SdkError,
    operation::delete_object::DeleteObjectError,
    operation::get_object::GetObjectError,
    operation::head_object::HeadObjectError,
    operation::put_object::PutObjectError,
    primitives::ByteStream,
    types::ChecksumAlgorithm,
};
use bytes::Bytes;

use store::{BlobKind, CasOutcome, RemoteStore, validate_ref_name};

/// S3-backed blob CAS + ref catalog.
///
/// `bucket` is the S3 bucket name. `prefix` is the key prefix under
/// which all objects are stored (no leading `/`, may be empty, always
/// ends without `/` internally — the constructor normalizes).
#[derive(Debug)]
pub struct S3RemoteStore {
    client: Client,
    bucket: String,
    prefix: String,
}

impl S3RemoteStore {
    /// Build an `S3RemoteStore` from a pre-configured SDK client.
    pub fn new(client: Client, bucket: String, prefix: String) -> Self {
        // Normalize: strip trailing slash from prefix so key-building
        // is always `{prefix}/{kind}/{hex}` without double slashes.
        let prefix = prefix.trim_end_matches('/').to_owned();
        S3RemoteStore {
            client,
            bucket,
            prefix,
        }
    }

    /// Construct from the default AWS config (env / config file / IMDS).
    pub async fn from_env(bucket: String, prefix: String) -> Result<Self> {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = Client::new(&config);
        Ok(Self::new(client, bucket, prefix))
    }

    // ---- key helpers ------------------------------------------------

    fn blob_key(&self, kind: BlobKind, id: &[u8]) -> String {
        if self.prefix.is_empty() {
            format!("{}/{}", kind.as_str(), hex(id))
        } else {
            format!("{}/{}/{}", self.prefix, kind.as_str(), hex(id))
        }
    }

    fn ref_key(&self, name: &str) -> String {
        if self.prefix.is_empty() {
            format!("refs/{name}")
        } else {
            format!("{}/refs/{name}", self.prefix)
        }
    }

    fn refs_prefix(&self) -> String {
        if self.prefix.is_empty() {
            "refs/".to_owned()
        } else {
            format!("{}/refs/", self.prefix)
        }
    }

    // ---- internal S3 helpers ----------------------------------------

    /// Read an object's body + ETag. Returns `Ok(None)` on NoSuchKey.
    async fn get_object(&self, key: &str) -> Result<Option<(Bytes, String)>> {
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(resp) => {
                let etag = resp.e_tag().unwrap_or_default().to_owned();
                let body = resp
                    .body
                    .collect()
                    .await
                    .with_context(|| format!("reading body of s3://{}/{key}", self.bucket))?;
                Ok(Some((body.into_bytes(), etag)))
            }
            Err(SdkError::ServiceError(err))
                if matches!(err.err(), GetObjectError::NoSuchKey(_)) =>
            {
                Ok(None)
            }
            Err(e) => Err(anyhow!(e).context(format!(
                "GetObject s3://{}/{key}",
                self.bucket
            ))),
        }
    }
}

/// Hex-encode arbitrary bytes (matches the other remotes' hex helpers).
fn hex(id: &[u8]) -> String {
    let mut s = String::with_capacity(id.len() * 2);
    for b in id {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// Check if an S3 error is a "not found" (NoSuchKey or 404).
fn is_not_found_head(err: &SdkError<HeadObjectError>) -> bool {
    matches!(err, SdkError::ServiceError(e) if e.err().is_not_found())
}

/// Check if an S3 PutObject error is a precondition failure (412).
fn is_precondition_failed(err: &SdkError<PutObjectError>) -> bool {
    match err {
        SdkError::ServiceError(e) => {
            // The SDK may not have a typed variant for 412, so also
            // check the raw HTTP status.
            e.raw().status().as_u16() == 412
        }
        SdkError::ResponseError(e) => e.raw().status().as_u16() == 412,
        _ => false,
    }
}

/// Check if an S3 DeleteObject error is a precondition failure (412).
fn is_delete_precondition_failed(err: &SdkError<DeleteObjectError>) -> bool {
    match err {
        SdkError::ServiceError(e) => e.raw().status().as_u16() == 412,
        SdkError::ResponseError(e) => e.raw().status().as_u16() == 412,
        _ => false,
    }
}

#[async_trait]
impl RemoteStore for S3RemoteStore {
    async fn get_blob(&self, kind: BlobKind, id: &[u8]) -> Result<Option<Bytes>> {
        let key = self.blob_key(kind, id);
        Ok(self.get_object(&key).await?.map(|(bytes, _etag)| bytes))
    }

    async fn put_blob(&self, kind: BlobKind, id: &[u8], bytes: Bytes) -> Result<()> {
        let key = self.blob_key(kind, id);
        // Blobs are immutable and content-addressed. We don't need
        // conditional writes — racing puts of the same (kind, id)
        // produce byte-identical objects by construction.
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .content_length(bytes.len() as i64)
            .checksum_algorithm(ChecksumAlgorithm::Sha1)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|e| {
                anyhow!(e).context(format!("PutObject s3://{}/{key}", self.bucket))
            })?;
        Ok(())
    }

    async fn has_blob(&self, kind: BlobKind, id: &[u8]) -> Result<bool> {
        let key = self.blob_key(kind, id);
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(ref e) if is_not_found_head(e) => Ok(false),
            Err(e) => Err(anyhow!(e).context(format!(
                "HeadObject s3://{}/{key}",
                self.bucket
            ))),
        }
    }

    async fn get_ref(&self, name: &str) -> Result<Option<Bytes>> {
        validate_ref_name(name)?;
        let key = self.ref_key(name);
        Ok(self.get_object(&key).await?.map(|(bytes, _etag)| bytes))
    }

    async fn cas_ref(
        &self,
        name: &str,
        expected: Option<&Bytes>,
        new: Option<&Bytes>,
    ) -> Result<CasOutcome> {
        validate_ref_name(name)?;
        let key = self.ref_key(name);

        // Step 1: Read current value + ETag.
        let current = self.get_object(&key).await?;
        let (current_val, current_etag) = match &current {
            Some((val, etag)) => (Some(val.clone()), Some(etag.clone())),
            None => (None, None),
        };

        // Step 2: Precondition check (same as FsRemoteStore).
        if current_val.as_ref() != expected {
            return Ok(CasOutcome::Conflict {
                actual: current_val,
            });
        }

        // Step 3: Apply the swap.
        match new {
            Some(value) => {
                let mut put = self
                    .client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(&key)
                    .content_length(value.len() as i64)
                    .checksum_algorithm(ChecksumAlgorithm::Sha1)
                    .body(ByteStream::from(value.clone()));

                // Conditional write: if creating, use if_none_match("*")
                // to reject if someone else created it first. If updating,
                // use if_match(etag) to reject if someone else changed it.
                match &current_etag {
                    None => {
                        put = put.if_none_match("*");
                    }
                    Some(etag) => {
                        put = put.if_match(etag);
                    }
                }

                match put.send().await {
                    Ok(_) => Ok(CasOutcome::Updated),
                    Err(ref e) if is_precondition_failed(e) => {
                        // Someone else mutated the ref between our read
                        // and write. Re-read to get the actual value for
                        // the conflict response.
                        let actual = self
                            .get_object(&key)
                            .await?
                            .map(|(bytes, _)| bytes);
                        Ok(CasOutcome::Conflict { actual })
                    }
                    Err(e) => Err(anyhow!(e).context(format!(
                        "PutObject (CAS) s3://{}/{key}",
                        self.bucket
                    ))),
                }
            }
            None => {
                let Some(etag) = current_etag else {
                    return Ok(CasOutcome::Updated);
                };

                match self
                    .client
                    .delete_object()
                    .bucket(&self.bucket)
                    .key(&key)
                    .if_match(etag)
                    .send()
                    .await
                {
                    Ok(_) => Ok(CasOutcome::Updated),
                    Err(ref e) if is_delete_precondition_failed(e) => {
                        // Someone else mutated the ref between our read
                        // and delete. Re-read to get the actual value for
                        // the conflict response.
                        let actual = self
                            .get_object(&key)
                            .await?
                            .map(|(bytes, _)| bytes);
                        Ok(CasOutcome::Conflict { actual })
                    }
                    Err(e) => Err(anyhow!(e).context(format!(
                        "DeleteObject (CAS) s3://{}/{key}",
                        self.bucket
                    ))),
                }
            }
        }
    }

    async fn list_refs(&self) -> Result<Vec<String>> {
        let prefix = self.refs_prefix();
        let mut names = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&prefix);

            if let Some(token) = &continuation_token {
                req = req.continuation_token(token);
            }

            let resp = req.send().await.map_err(|e| {
                anyhow!(e).context(format!(
                    "ListObjectsV2 s3://{}/{}",
                    self.bucket, prefix
                ))
            })?;

            for obj in resp.contents() {
                if let Some(key) = obj.key()
                    && let Some(name) = key.strip_prefix(&prefix)
                    && !name.is_empty()
                    && !name.contains('/')
                {
                    names.push(name.to_owned());
                }
            }

            if resp.is_truncated() == Some(true) {
                continuation_token = resp.next_continuation_token().map(|s| s.to_owned());
            } else {
                break;
            }
        }

        names.sort();
        Ok(names)
    }
}

/// Parse an `s3://` URL into (bucket, prefix).
///
/// Format: `s3://bucket` or `s3://bucket/prefix/path`.
/// Returns `None` if not an `s3://` URL.
pub fn parse_s3_url(remote: &str) -> Result<Option<(String, String)>> {
    let Some(rest) = remote.strip_prefix("s3://") else {
        return Ok(None);
    };
    if rest.is_empty() {
        return Err(anyhow!("s3:// remote requires a bucket name"));
    }
    let (bucket, prefix) = match rest.split_once('/') {
        Some((b, p)) => (b.to_owned(), p.to_owned()),
        None => (rest.to_owned(), String::new()),
    };
    if bucket.is_empty() {
        return Err(anyhow!("s3:// remote has empty bucket name"));
    }
    Ok(Some((bucket, prefix)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- URL parsing (pure, no S3 needed) ---------------------------

    #[test]
    fn parse_s3_url_bucket_only() {
        let (bucket, prefix) = parse_s3_url("s3://my-bucket").unwrap().unwrap();
        assert_eq!(bucket, "my-bucket");
        assert_eq!(prefix, "");
    }

    #[test]
    fn parse_s3_url_bucket_with_prefix() {
        let (bucket, prefix) =
            parse_s3_url("s3://my-bucket/some/prefix").unwrap().unwrap();
        assert_eq!(bucket, "my-bucket");
        assert_eq!(prefix, "some/prefix");
    }

    #[test]
    fn parse_s3_url_not_s3() {
        assert!(parse_s3_url("dir:///foo").unwrap().is_none());
        assert!(parse_s3_url("").unwrap().is_none());
        assert!(parse_s3_url("grpc://host:1234").unwrap().is_none());
    }

    #[test]
    fn parse_s3_url_empty_bucket_rejected() {
        assert!(parse_s3_url("s3://").is_err());
    }

    // ---- Key layout (pure, no S3 needed) ----------------------------

    #[test]
    fn key_layout_with_prefix() {
        let store = S3RemoteStore {
            client: make_dummy_client(),
            bucket: "b".into(),
            prefix: "kiki/myrepo".into(),
        };
        let id = [0xab; 20];
        assert_eq!(
            store.blob_key(BlobKind::Blob, &id),
            format!("kiki/myrepo/blob/{}", hex(&id))
        );
        assert_eq!(store.ref_key("head"), "kiki/myrepo/refs/head");
        assert_eq!(store.refs_prefix(), "kiki/myrepo/refs/");
    }

    #[test]
    fn key_layout_without_prefix() {
        let store = S3RemoteStore {
            client: make_dummy_client(),
            bucket: "b".into(),
            prefix: String::new(),
        };
        let id = [0x01; 20];
        assert_eq!(
            store.blob_key(BlobKind::Tree, &id),
            format!("tree/{}", hex(&id))
        );
        assert_eq!(store.ref_key("op_heads"), "refs/op_heads");
        assert_eq!(store.refs_prefix(), "refs/");
    }

    #[test]
    fn trailing_slash_stripped_from_prefix() {
        let store = S3RemoteStore::new(
            make_dummy_client(),
            "b".into(),
            "has/trailing/".into(),
        );
        assert_eq!(store.prefix, "has/trailing");
        assert_eq!(store.ref_key("head"), "has/trailing/refs/head");
    }

    fn make_dummy_client() -> Client {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .build();
        Client::from_conf(config)
    }

    // ---- Proptests (pure, no S3 needed) -----------------------------

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// parse_s3_url never panics on arbitrary input.
            #[test]
            fn parse_never_panics(input in "\\PC*") {
                let _ = parse_s3_url(&input);
            }

            /// Any valid s3://bucket/prefix round-trips through parse.
            #[test]
            fn valid_url_round_trips(
                bucket in "[a-z0-9][a-z0-9.-]{2,62}",
                prefix in "[a-zA-Z0-9/_.-]{0,128}",
            ) {
                let url = if prefix.is_empty() {
                    format!("s3://{bucket}")
                } else {
                    format!("s3://{bucket}/{prefix}")
                };
                let (b, p) = parse_s3_url(&url)
                    .expect("valid URL should parse")
                    .expect("s3:// URL should return Some");
                prop_assert_eq!(b, bucket);
                prop_assert_eq!(p, prefix);
            }

            /// Non-s3 schemes always return Ok(None).
            #[test]
            fn non_s3_returns_none(scheme in "(dir|ssh|grpc|kiki|ftp|http)") {
                let url = format!("{scheme}://anything/here");
                let result = parse_s3_url(&url).expect("should not error");
                prop_assert!(result.is_none());
            }

            /// Key construction never produces double slashes.
            #[test]
            fn keys_have_no_double_slashes(
                prefix in "[a-z]{0,20}(/[a-z]{1,10}){0,3}",
                kind_idx in 0..6usize,
                id_byte in any::<u8>(),
            ) {
                let kinds = [
                    BlobKind::Tree, BlobKind::Blob, BlobKind::Commit,
                    BlobKind::View, BlobKind::Operation, BlobKind::Extra,
                ];
                let store = S3RemoteStore::new(
                    make_dummy_client(),
                    "b".into(),
                    prefix,
                );
                let id = [id_byte; 32];
                let blob_key = store.blob_key(kinds[kind_idx], &id);
                let ref_key = store.ref_key("head");
                prop_assert!(
                    !blob_key.contains("//"),
                    "blob key has double slash: {blob_key}"
                );
                prop_assert!(
                    !ref_key.contains("//"),
                    "ref key has double slash: {ref_key}"
                );
            }
        }
    }

    // ---- RemoteStore contract tests (need real S3) -------------------
    //
    // These exercise the full RemoteStore trait against a real S3 bucket.
    // Gated behind `KIKI_TEST_S3_BUCKET` — skip in CI unless configured.
    //
    // Run manually:
    //   KIKI_TEST_S3_BUCKET=my-test-bucket \
    //     cargo test -p daemon -- remote::s3::tests::s3_contract --nocapture
    //
    // The tests use a unique prefix per run so they don't collide with
    // each other or with previous runs.

    mod s3_contract {
        use super::*;

        /// Returns an S3RemoteStore if KIKI_TEST_S3_BUCKET is set,
        /// otherwise skips the test.
        async fn make_s3_store() -> Option<S3RemoteStore> {
            let bucket = std::env::var("KIKI_TEST_S3_BUCKET").ok()?;
            let prefix = format!(
                "kiki-test/{:016x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64
            );
            Some(S3RemoteStore::from_env(bucket, prefix).await.unwrap())
        }

        fn id_of(byte: u8) -> [u8; 32] {
            [byte; 32]
        }

        #[tokio::test]
        async fn put_then_get_round_trip() {
            let Some(s) = make_s3_store().await else { return };
            let id = id_of(0xab);
            let bytes = Bytes::from_static(b"hello blob");
            s.put_blob(BlobKind::Blob, &id, bytes.clone()).await.unwrap();
            let got = s.get_blob(BlobKind::Blob, &id).await.unwrap();
            assert_eq!(got.as_deref(), Some(bytes.as_ref()));
        }

        #[tokio::test]
        async fn missing_returns_none_not_err() {
            let Some(s) = make_s3_store().await else { return };
            let got = s.get_blob(BlobKind::Tree, &id_of(0)).await.unwrap();
            assert!(got.is_none(), "missing blob must surface as Ok(None)");
        }

        #[tokio::test]
        async fn has_blob_tracks_state() {
            let Some(s) = make_s3_store().await else { return };
            let id = id_of(7);
            assert!(!s.has_blob(BlobKind::Blob, &id).await.unwrap());
            s.put_blob(BlobKind::Blob, &id, Bytes::from_static(b"x"))
                .await
                .unwrap();
            assert!(s.has_blob(BlobKind::Blob, &id).await.unwrap());
            // Different kind, same id: distinct keyspace.
            assert!(!s.has_blob(BlobKind::Tree, &id).await.unwrap());
        }

        #[tokio::test]
        async fn idempotent_put_does_not_error() {
            let Some(s) = make_s3_store().await else { return };
            let id = id_of(1);
            let b = Bytes::from_static(b"same bytes");
            s.put_blob(BlobKind::Commit, &id, b.clone()).await.unwrap();
            s.put_blob(BlobKind::Commit, &id, b.clone()).await.unwrap();
            assert_eq!(
                s.get_blob(BlobKind::Commit, &id).await.unwrap().as_deref(),
                Some(b.as_ref()),
            );
        }

        #[tokio::test]
        async fn kinds_are_partitioned() {
            let Some(s) = make_s3_store().await else { return };
            let id = id_of(0xff);
            s.put_blob(BlobKind::Blob, &id, Bytes::from_static(b"file-bytes"))
                .await
                .unwrap();
            s.put_blob(BlobKind::Tree, &id, Bytes::from_static(b"tree-bytes"))
                .await
                .unwrap();
            assert_eq!(
                s.get_blob(BlobKind::Blob, &id).await.unwrap().as_deref(),
                Some(b"file-bytes".as_ref()),
            );
            assert_eq!(
                s.get_blob(BlobKind::Tree, &id).await.unwrap().as_deref(),
                Some(b"tree-bytes".as_ref()),
            );
        }

        #[tokio::test]
        async fn put_then_get_64_byte_id() {
            let Some(s) = make_s3_store().await else { return };
            let id = [0xcd; 64];
            let data = Bytes::from_static(b"view-data");
            s.put_blob(BlobKind::View, &id, data.clone()).await.unwrap();
            let got = s.get_blob(BlobKind::View, &id).await.unwrap();
            assert_eq!(got.as_deref(), Some(data.as_ref()));
            assert!(s.has_blob(BlobKind::View, &id).await.unwrap());
            assert!(!s.has_blob(BlobKind::Operation, &id).await.unwrap());
        }

        // ---- Ref methods ----

        #[tokio::test]
        async fn ref_missing_returns_none() {
            let Some(s) = make_s3_store().await else { return };
            let got = s.get_ref("op_heads").await.unwrap();
            assert!(got.is_none());
        }

        #[tokio::test]
        async fn cas_ref_create_then_read() {
            let Some(s) = make_s3_store().await else { return };
            let v = Bytes::from_static(b"value-0");
            let outcome = s.cas_ref("head", None, Some(&v)).await.unwrap();
            assert_eq!(outcome, CasOutcome::Updated);
            let got = s.get_ref("head").await.unwrap().expect("ref should exist");
            assert_eq!(got.as_ref(), v.as_ref());
        }

        #[tokio::test]
        async fn cas_ref_create_only_conflicts_when_present() {
            let Some(s) = make_s3_store().await else { return };
            let v0 = Bytes::from_static(b"v0");
            s.cas_ref("head", None, Some(&v0)).await.unwrap();
            let outcome = s
                .cas_ref("head", None, Some(&Bytes::from_static(b"v1")))
                .await
                .unwrap();
            assert_eq!(
                outcome,
                CasOutcome::Conflict {
                    actual: Some(v0),
                }
            );
        }

        #[tokio::test]
        async fn cas_ref_advance_with_correct_expected() {
            let Some(s) = make_s3_store().await else { return };
            let v0 = Bytes::from_static(b"v0");
            let v1 = Bytes::from_static(b"v1");
            s.cas_ref("op_heads", None, Some(&v0)).await.unwrap();
            let outcome = s
                .cas_ref("op_heads", Some(&v0), Some(&v1))
                .await
                .unwrap();
            assert_eq!(outcome, CasOutcome::Updated);
            assert_eq!(s.get_ref("op_heads").await.unwrap(), Some(v1));
        }

        #[tokio::test]
        async fn cas_ref_stale_expected_returns_actual() {
            let Some(s) = make_s3_store().await else { return };
            let v0 = Bytes::from_static(b"v0");
            let v1 = Bytes::from_static(b"v1");
            s.cas_ref("head", None, Some(&v0)).await.unwrap();
            let stale = Bytes::from_static(b"stale");
            let outcome = s
                .cas_ref("head", Some(&stale), Some(&v1))
                .await
                .unwrap();
            assert_eq!(
                outcome,
                CasOutcome::Conflict {
                    actual: Some(v0),
                }
            );
            // Not clobbered.
            assert_eq!(
                s.get_ref("head").await.unwrap(),
                Some(Bytes::from_static(b"v0")),
            );
        }

        #[tokio::test]
        async fn cas_ref_delete() {
            let Some(s) = make_s3_store().await else { return };
            let v0 = Bytes::from_static(b"v0");
            s.cas_ref("transient", None, Some(&v0)).await.unwrap();
            let outcome = s
                .cas_ref("transient", Some(&v0), None)
                .await
                .unwrap();
            assert_eq!(outcome, CasOutcome::Updated);
            assert_eq!(s.get_ref("transient").await.unwrap(), None);
        }

        #[tokio::test]
        async fn cas_ref_delete_with_stale_expected_conflicts() {
            let Some(s) = make_s3_store().await else { return };
            let v0 = Bytes::from_static(b"v0");
            s.cas_ref("transient", None, Some(&v0)).await.unwrap();
            let stale = Bytes::from_static(b"wrong");
            let outcome = s
                .cas_ref("transient", Some(&stale), None)
                .await
                .unwrap();
            assert_eq!(
                outcome,
                CasOutcome::Conflict {
                    actual: Some(v0.clone()),
                }
            );
            assert_eq!(s.get_ref("transient").await.unwrap(), Some(v0));
        }

        #[tokio::test]
        async fn cas_ref_empty_value_distinct_from_absent() {
            let Some(s) = make_s3_store().await else { return };
            let empty = Bytes::new();
            s.cas_ref("e", None, Some(&empty)).await.unwrap();
            assert_eq!(s.get_ref("e").await.unwrap(), Some(empty.clone()));

            // expected = None should conflict (ref exists, value is empty).
            let outcome = s
                .cas_ref("e", None, Some(&Bytes::from_static(b"x")))
                .await
                .unwrap();
            assert_eq!(
                outcome,
                CasOutcome::Conflict {
                    actual: Some(empty.clone()),
                }
            );
            // expected = Some(empty) should succeed.
            let outcome = s
                .cas_ref("e", Some(&empty), Some(&Bytes::from_static(b"x")))
                .await
                .unwrap();
            assert_eq!(outcome, CasOutcome::Updated);
        }

        #[tokio::test]
        async fn list_refs_returns_sorted_names() {
            let Some(s) = make_s3_store().await else { return };
            for name in ["zeta", "alpha", "head"] {
                s.cas_ref(name, None, Some(&Bytes::from_static(b"v")))
                    .await
                    .unwrap();
            }
            let names = s.list_refs().await.unwrap();
            assert_eq!(names, vec!["alpha", "head", "zeta"]);
        }

        #[tokio::test]
        async fn list_refs_on_empty_store_is_ok() {
            let Some(s) = make_s3_store().await else { return };
            assert!(s.list_refs().await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn cas_ref_rejects_bad_name() {
            let Some(s) = make_s3_store().await else { return };
            let err = s
                .cas_ref("a/b", None, Some(&Bytes::from_static(b"x")))
                .await
                .expect_err("must reject names with '/'");
            assert!(err.to_string().contains("'/'"), "got: {err}");
        }
    }
}
