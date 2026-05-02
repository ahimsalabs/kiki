//! `OpHeadsStore` impl that drives the daemon's per-mount catalog
//! (M10.5, PLAN.md §10.5).
//!
//! `jj-lib`'s default `SimpleOpHeadsStore` writes one empty file per
//! op-head id under `<repo>/op_heads/heads/`. With M7's pinned-`.jj/`
//! subtree those writes go through FUSE into the daemon's per-mount
//! Store — content-addressed, but **not** catalog-arbitrated. Two
//! CLI processes against a shared remote (`dir://`, future S3/grpc)
//! would silently clobber each other's "advance op-heads" updates.
//!
//! `KikiOpHeadsStore` replaces the file-per-head shape with a single
//! `op_heads` ref in the catalog. `update_op_heads` is one
//! compare-and-swap against the daemon's `CasCatalogRef` RPC; the
//! daemon dispatches to either the configured remote (multi-daemon)
//! or the per-mount `LocalRefs` (single-daemon). The set of op-heads
//! is the unit of arbitration — exactly what CAS was built for.
//!
//! Wire format for the ref value: a sequence of length-prefixed
//! op-id byte blocks. `[u32 BE len][bytes][u32 BE len][bytes]...`
//! Empty value (`Bytes::new()`) means "no op-heads" — distinct from
//! "ref does not exist", which the catalog returns as `None` and we
//! also treat as "no op-heads". Empty length entries (`len = 0`) are
//! never written but tolerated on read for forward compat.
//!
//! Names: a single `op_heads` ref. PLAN.md §10.5.2 decision 4
//! explains why no workspace suffix.

use std::collections::BTreeSet;

use async_trait::async_trait;
use jj_lib::object_id::ObjectId;
use jj_lib::op_heads_store::{
    OpHeadsStore, OpHeadsStoreError, OpHeadsStoreLock,
};
use jj_lib::op_store::OperationId;
use proto::jj_interface::{CasCatalogRefReq, GetCatalogRefReq};

use crate::blocking_client::BlockingJujutsuInterfaceClient;

/// Single ref name used for the op-heads set.
///
/// §10.5.2 decision 4: unscoped (no workspace suffix). Op-heads is a
/// repo-level concept; the jj-lib `OpHeadsStore` trait takes no
/// workspace argument.
const OP_HEADS_REF: &str = "op_heads";

/// Shorthand for the trait's error variants. Wrapping a `String`
/// `into()` an `Err::Other(_)` is a tiny pattern that shows up in
/// every method.
fn read_err<E>(source: E) -> OpHeadsStoreError
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    OpHeadsStoreError::Read(source.into())
}

fn write_err<E>(new_op_id: OperationId, source: E) -> OpHeadsStoreError
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    OpHeadsStoreError::Write {
        new_op_id,
        source: source.into(),
    }
}

/// Length-prefixed concat of op-id bytes. See module docs for the
/// rationale.
fn encode(ids: &BTreeSet<OperationId>) -> Vec<u8> {
    // Capacity: 4 bytes per length prefix + content. Most op-ids are
    // 64 bytes (blake2b); allocate accordingly.
    let mut out = Vec::with_capacity(ids.len() * (4 + 64));
    for id in ids {
        let bytes = id.as_bytes();
        let len: u32 = bytes
            .len()
            .try_into()
            .expect("op-id length fits in u32 — sane backends use <=512 bytes");
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

/// Inverse of [`encode`]. Refuses anything we can't fully consume —
/// no silent truncation.
fn decode(buf: &[u8]) -> Result<BTreeSet<OperationId>, OpHeadsStoreError> {
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i < buf.len() {
        if buf.len() - i < 4 {
            return Err(read_err(format!(
                "truncated op_heads value: {} trailing bytes < 4-byte length prefix",
                buf.len() - i
            )));
        }
        let len_bytes: [u8; 4] = buf[i..i + 4].try_into().unwrap();
        let len = u32::from_be_bytes(len_bytes) as usize;
        i += 4;
        if buf.len() - i < len {
            return Err(read_err(format!(
                "truncated op_heads entry: declared len {} but only {} bytes remain",
                len,
                buf.len() - i
            )));
        }
        if len > 0 {
            // Skip empty entries (forward-compat — never produced today,
            // but tolerated on read so future writers can use them as
            // padding/markers without breaking us).
            out.insert(OperationId::from_bytes(&buf[i..i + len]));
        }
        i += len;
    }
    Ok(out)
}

/// jj-lib `OpHeadsStore` impl backed by the daemon's catalog.
///
/// Holds a clone of [`BlockingJujutsuInterfaceClient`] (cheap — it's
/// `Arc<Mutex<…>>` internally) and the canonical working-copy path
/// the daemon will route on. Construction happens at workspace init
/// (in `Workspace::init_with_factories`) and on every load via the
/// registered store factory.
#[derive(Debug)]
pub struct KikiOpHeadsStore {
    client: BlockingJujutsuInterfaceClient,
    working_copy_path: String,
}

impl KikiOpHeadsStore {
    pub fn name() -> &'static str {
        "kiki_op_heads"
    }

    pub fn new(
        client: BlockingJujutsuInterfaceClient,
        working_copy_path: String,
    ) -> Self {
        Self {
            client,
            working_copy_path,
        }
    }

    /// One round-trip: fetch the current op-heads set, parse it.
    /// `None` from the catalog (ref does not exist) decodes to the
    /// empty set, which is the same shape jj-lib expects on a fresh
    /// repo.
    fn read_set(&self) -> Result<BTreeSet<OperationId>, OpHeadsStoreError> {
        let resp = self
            .client
            .get_catalog_ref(GetCatalogRefReq {
                working_copy_path: self.working_copy_path.clone(),
                name: OP_HEADS_REF.into(),
            })
            .map_err(|status| read_err(format!("daemon GetCatalogRef: {status}")))?
            .into_inner();
        if !resp.found {
            return Ok(BTreeSet::new());
        }
        decode(&resp.value)
    }
}

#[async_trait]
impl OpHeadsStore for KikiOpHeadsStore {
    fn name(&self) -> &str {
        Self::name()
    }

    async fn update_op_heads(
        &self,
        old_ids: &[OperationId],
        new_id: &OperationId,
    ) -> Result<(), OpHeadsStoreError> {
        // Trait contract (op_heads_store.rs:60): "The old op heads
        // must not contain the new one." Cheap to verify.
        debug_assert!(!old_ids.contains(new_id));

        // CAS retry loop. On Conflict the catalog returns the actual
        // current value, so we don't need a follow-up GetRef — just
        // re-derive the desired set from `actual` and retry. In
        // practice a localhost loop converges in 1 iteration; bound
        // it generously to surface a real bug if we ever spin.
        const MAX_RETRIES: usize = 64;
        for _ in 0..MAX_RETRIES {
            let current = self.read_set()?;
            let mut next = current.clone();
            for id in old_ids {
                next.remove(id);
            }
            next.insert(new_id.clone());

            let expected_bytes = if current.is_empty() {
                // Distinguish "no ref yet" from "ref exists but empty"
                // on the wire: only the first call should pass
                // expected = None (create-only). After that, the ref
                // exists, even if its serialized value is short.
                //
                // Heuristic: if `read_set` saw `found = false`, we'd
                // want `expected = None`; if `found = true && value
                // empty`, we'd want `expected = Some(empty)`. Today
                // we collapse both into "current.is_empty()" — but
                // that's wrong for the second case.
                //
                // Resolved by re-fetching with `found` exposed.
                match self.expected_for_empty()? {
                    ExpectedShape::Absent => None,
                    ExpectedShape::Present(bytes) => Some(bytes),
                }
            } else {
                Some(encode(&current))
            };
            let new_bytes = encode(&next);

            let resp = self
                .client
                .cas_catalog_ref(CasCatalogRefReq {
                    working_copy_path: self.working_copy_path.clone(),
                    name: OP_HEADS_REF.into(),
                    expected: expected_bytes,
                    new: Some(new_bytes),
                })
                .map_err(|status| {
                    write_err(new_id.clone(), format!("daemon CasCatalogRef: {status}"))
                })?
                .into_inner();
            if resp.updated {
                return Ok(());
            }
            // Conflict: another writer advanced concurrently. Loop —
            // the next read_set picks up their value and we retry.
        }
        Err(write_err(
            new_id.clone(),
            format!(
                "op_heads CAS failed to converge after {MAX_RETRIES} retries; \
                 likely a runaway concurrent writer or a backend that always \
                 reports Conflict"
            ),
        ))
    }

    async fn get_op_heads(&self) -> Result<Vec<OperationId>, OpHeadsStoreError> {
        let set = self.read_set()?;
        Ok(set.into_iter().collect())
    }

    /// PLAN.md §10.5.2 decision 6: no separate lock primitive. CAS is
    /// the arbitration; the trait doc says the lock isn't needed for
    /// correctness ("implementations are free to return a type that
    /// doesn't hold a lock").
    async fn lock(
        &self,
    ) -> Result<Box<dyn OpHeadsStoreLock + '_>, OpHeadsStoreError> {
        Ok(Box::new(NoLock))
    }
}

struct NoLock;
impl OpHeadsStoreLock for NoLock {}

/// Disambiguator for the CAS `expected` precondition: we need to
/// distinguish "ref does not exist" (`expected = None`, create-only)
/// from "ref exists with this exact value" (`expected = Some(bytes)`).
/// `read_set()` collapses both into the empty BTreeSet; this helper
/// re-asks the catalog for the raw state.
enum ExpectedShape {
    Absent,
    Present(Vec<u8>),
}

impl KikiOpHeadsStore {
    fn expected_for_empty(&self) -> Result<ExpectedShape, OpHeadsStoreError> {
        let resp = self
            .client
            .get_catalog_ref(GetCatalogRefReq {
                working_copy_path: self.working_copy_path.clone(),
                name: OP_HEADS_REF.into(),
            })
            .map_err(|status| read_err(format!("daemon GetCatalogRef: {status}")))?
            .into_inner();
        if resp.found {
            Ok(ExpectedShape::Present(resp.value))
        } else {
            Ok(ExpectedShape::Absent)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(byte: u8, len: usize) -> OperationId {
        OperationId::from_bytes(&vec![byte; len])
    }

    #[test]
    fn encode_empty_round_trips() {
        let set = BTreeSet::new();
        assert_eq!(encode(&set), Vec::<u8>::new());
        assert_eq!(decode(&[]).unwrap(), set);
    }

    #[test]
    fn encode_single_round_trips() {
        let mut set = BTreeSet::new();
        set.insert(op(0xAA, 32));
        let bytes = encode(&set);
        // 4-byte length prefix + 32-byte payload.
        assert_eq!(bytes.len(), 4 + 32);
        assert_eq!(&bytes[..4], &(32u32).to_be_bytes());
        assert_eq!(decode(&bytes).unwrap(), set);
    }

    #[test]
    fn encode_multiple_round_trips() {
        let mut set = BTreeSet::new();
        set.insert(op(0x11, 64));
        set.insert(op(0x22, 64));
        set.insert(op(0x33, 32)); // varying length
        let bytes = encode(&set);
        assert_eq!(decode(&bytes).unwrap(), set);
    }

    #[test]
    fn encode_is_deterministic() {
        // BTreeSet's iter order means encode() output is stable for
        // the same set, regardless of insertion order. CAS depends on
        // this: two daemons computing the same heads set must produce
        // byte-identical `expected` bytes.
        let a = {
            let mut s = BTreeSet::new();
            s.insert(op(0x33, 32));
            s.insert(op(0x11, 32));
            s.insert(op(0x22, 32));
            s
        };
        let b = {
            let mut s = BTreeSet::new();
            s.insert(op(0x11, 32));
            s.insert(op(0x22, 32));
            s.insert(op(0x33, 32));
            s
        };
        assert_eq!(encode(&a), encode(&b));
    }

    #[test]
    fn decode_truncated_length_prefix_is_error() {
        let buf = [0u8; 2]; // only 2 bytes — can't even fit u32 prefix
        decode(&buf).expect_err("truncated prefix must error");
    }

    #[test]
    fn decode_truncated_payload_is_error() {
        let mut buf = (5u32).to_be_bytes().to_vec();
        buf.extend_from_slice(b"abc"); // declared 5 bytes, only 3 follow
        decode(&buf).expect_err("truncated payload must error");
    }

    #[test]
    fn decode_zero_length_entries_are_skipped() {
        // [len=0][len=4][1,2,3,4][len=0] — middle entry only.
        let mut buf = Vec::new();
        buf.extend_from_slice(&(0u32).to_be_bytes());
        buf.extend_from_slice(&(4u32).to_be_bytes());
        buf.extend_from_slice(&[1, 2, 3, 4]);
        buf.extend_from_slice(&(0u32).to_be_bytes());
        let set = decode(&buf).unwrap();
        assert_eq!(set.len(), 1);
        assert!(set.contains(&OperationId::from_bytes(&[1, 2, 3, 4])));
    }
}
