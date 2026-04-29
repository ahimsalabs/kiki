//! Per-mount local-fallback ref catalog (M10.5, PLAN.md §10.5).
//!
//! The catalog API ([`crate::remote::RemoteStore::get_ref`] /
//! [`cas_ref`](crate::remote::RemoteStore::cas_ref) /
//! [`list_refs`](crate::remote::RemoteStore::list_refs)) is the wire
//! M10 shipped for daemon-to-daemon arbitration of mutable pointers
//! (op heads, branch tips). When a mount has a remote configured the
//! catalog naturally lives on the remote — that's the whole point.
//! When a mount has no remote (single-daemon case), refs need to live
//! *somewhere* so the CLI's [`YakOpHeadsStore`] (jj-lib's `OpHeadsStore`
//! impl that drives the catalog) sees a working API regardless.
//!
//! `LocalRefs` is that "somewhere": a single redb table inside the
//! per-mount `store.redb` file. CAS atomicity comes from redb's
//! `WriteTransaction` — the read-compare-swap runs inside one txn, so
//! two concurrent CASes serialize cleanly. Cross-process arbitration
//! is **not** required here (this is the no-remote single-daemon path);
//! `dir://` shared between daemons stays the multi-process story and
//! uses [`crate::remote::fs::FsRemoteStore`]'s flock dance.
//!
//! Storage layout: one redb table `refs_v1` keyed by ref name as
//! `&str`, value = ref bytes. `_v1` suffix matches the existing
//! convention in [`crate::store`]; bump to `_v2` on schema change.

use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use redb::{Database, ReadableTable, TableDefinition};

use crate::remote::{validate_ref_name, CasOutcome};

/// Per-mount refs table. See module docs for schema rationale.
const REFS: TableDefinition<&str, &[u8]> = TableDefinition::new("refs_v1");

/// Local-fallback catalog backed by the per-mount redb database.
///
/// Cloning a `LocalRefs` only clones an `Arc<Database>` — redb
/// internally serializes writers, so two clones safely race against
/// each other.
#[derive(Clone, Debug)]
pub struct LocalRefs {
    db: Arc<Database>,
}

impl LocalRefs {
    /// Construct over an existing [`redb::Database`] handle (typically
    /// the one [`crate::store::Store::database`] hands out). The
    /// table is created lazily on first write, so a fresh instance is
    /// always valid even before any ref has been set.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// See [`crate::remote::RemoteStore::get_ref`].
    pub fn get_ref(&self, name: &str) -> Result<Option<Bytes>> {
        validate_ref_name(name)?;
        let txn = self.db.begin_read().context("redb begin_read (refs)")?;
        // Empty store: the table doesn't exist yet. Treat as "no ref".
        let tbl = match txn.open_table(REFS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(anyhow::Error::from(e).context("open refs table for read")),
        };
        let raw = tbl.get(name).context("redb get (refs)")?;
        Ok(raw.map(|slot| Bytes::copy_from_slice(slot.value())))
    }

    /// See [`crate::remote::RemoteStore::cas_ref`]. Atomicity comes
    /// from running the read+compare+apply inside a single
    /// [`redb::WriteTransaction`].
    pub fn cas_ref(
        &self,
        name: &str,
        expected: Option<&Bytes>,
        new: Option<&Bytes>,
    ) -> Result<CasOutcome> {
        validate_ref_name(name)?;
        let txn = self.db.begin_write().context("redb begin_write (refs)")?;
        let outcome = {
            let mut tbl = txn
                .open_table(REFS)
                .context("open refs table for write")?;
            let actual = tbl
                .get(name)
                .context("redb get (refs CAS)")?
                .map(|slot| Bytes::copy_from_slice(slot.value()));
            if actual.as_ref() != expected {
                CasOutcome::Conflict { actual }
            } else {
                match new {
                    Some(bytes) => {
                        tbl.insert(name, bytes.as_ref())
                            .context("redb insert (refs CAS)")?;
                    }
                    None => {
                        tbl.remove(name).context("redb remove (refs CAS)")?;
                    }
                }
                CasOutcome::Updated
            }
        };
        // Always commit: on Conflict the txn is a no-op but committing
        // keeps the API symmetric (no abort path) and redb collapses
        // empty txns cheaply.
        txn.commit().context("redb commit (refs CAS)")?;
        Ok(outcome)
    }

    /// See [`crate::remote::RemoteStore::list_refs`].
    pub fn list_refs(&self) -> Result<Vec<String>> {
        let txn = self.db.begin_read().context("redb begin_read (refs)")?;
        let tbl = match txn.open_table(REFS) {
            Ok(t) => t,
            // Empty store — no `refs_v1` table yet. List is empty.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(anyhow::Error::from(e).context("open refs table for list")),
        };
        let mut out = Vec::new();
        for entry in tbl.iter().context("redb iter (refs)")? {
            let (k, _v) = entry.context("redb iter row (refs)")?;
            out.push(k.value().to_string());
        }
        // Sort for stable output (mirrors FsRemoteStore::list_refs).
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn refs() -> LocalRefs {
        let store = Store::new_in_memory();
        LocalRefs::new(store.database())
    }

    #[test]
    fn missing_ref_returns_none() {
        assert!(refs().get_ref("op_heads").unwrap().is_none());
    }

    #[test]
    fn create_then_read_round_trips() {
        let r = refs();
        let v = Bytes::from_static(b"abc");
        let out = r.cas_ref("op_heads", None, Some(&v)).unwrap();
        assert_eq!(out, CasOutcome::Updated);
        assert_eq!(r.get_ref("op_heads").unwrap().as_deref(), Some(&b"abc"[..]));
    }

    #[test]
    fn create_only_against_existing_conflicts() {
        let r = refs();
        let v = Bytes::from_static(b"abc");
        r.cas_ref("op_heads", None, Some(&v)).unwrap();
        // Second create-only must report Conflict with the actual
        // existing value, not silently overwrite.
        match r.cas_ref("op_heads", None, Some(&Bytes::from_static(b"xyz"))).unwrap() {
            CasOutcome::Conflict { actual } => {
                assert_eq!(actual.as_deref(), Some(&b"abc"[..]));
            }
            CasOutcome::Updated => panic!("create-only must conflict on existing ref"),
        }
        // Original value untouched.
        assert_eq!(r.get_ref("op_heads").unwrap().as_deref(), Some(&b"abc"[..]));
    }

    #[test]
    fn advance_with_correct_expected() {
        let r = refs();
        r.cas_ref("op_heads", None, Some(&Bytes::from_static(b"v0"))).unwrap();
        let v0 = Bytes::from_static(b"v0");
        let v1 = Bytes::from_static(b"v1");
        let out = r.cas_ref("op_heads", Some(&v0), Some(&v1)).unwrap();
        assert_eq!(out, CasOutcome::Updated);
        assert_eq!(r.get_ref("op_heads").unwrap().as_deref(), Some(&b"v1"[..]));
    }

    #[test]
    fn stale_expected_returns_actual() {
        let r = refs();
        r.cas_ref("op_heads", None, Some(&Bytes::from_static(b"v0"))).unwrap();
        r.cas_ref(
            "op_heads",
            Some(&Bytes::from_static(b"v0")),
            Some(&Bytes::from_static(b"v1")),
        )
        .unwrap();
        // Now CAS against the stale "v0" — must fail with the actual "v1".
        match r
            .cas_ref(
                "op_heads",
                Some(&Bytes::from_static(b"v0")),
                Some(&Bytes::from_static(b"v2")),
            )
            .unwrap()
        {
            CasOutcome::Conflict { actual } => {
                assert_eq!(actual.as_deref(), Some(&b"v1"[..]));
            }
            CasOutcome::Updated => panic!("stale expected must conflict"),
        }
    }

    #[test]
    fn delete_removes_ref() {
        let r = refs();
        r.cas_ref("op_heads", None, Some(&Bytes::from_static(b"v0"))).unwrap();
        let out = r
            .cas_ref("op_heads", Some(&Bytes::from_static(b"v0")), None)
            .unwrap();
        assert_eq!(out, CasOutcome::Updated);
        assert!(r.get_ref("op_heads").unwrap().is_none());
    }

    #[test]
    fn delete_with_stale_expected_conflicts() {
        let r = refs();
        r.cas_ref("op_heads", None, Some(&Bytes::from_static(b"v0"))).unwrap();
        r.cas_ref(
            "op_heads",
            Some(&Bytes::from_static(b"v0")),
            Some(&Bytes::from_static(b"v1")),
        )
        .unwrap();
        match r
            .cas_ref("op_heads", Some(&Bytes::from_static(b"v0")), None)
            .unwrap()
        {
            CasOutcome::Conflict { actual } => {
                assert_eq!(actual.as_deref(), Some(&b"v1"[..]));
            }
            CasOutcome::Updated => panic!("delete with stale expected must conflict"),
        }
    }

    #[test]
    fn empty_value_distinct_from_absent() {
        let r = refs();
        // Set to empty bytes — distinct from "ref does not exist".
        r.cas_ref("op_heads", None, Some(&Bytes::new())).unwrap();
        assert_eq!(r.get_ref("op_heads").unwrap().as_deref(), Some(&[][..]));
        // Create-only must conflict because the ref exists, even though
        // its current value is empty.
        match r
            .cas_ref("op_heads", None, Some(&Bytes::from_static(b"x")))
            .unwrap()
        {
            CasOutcome::Conflict { actual } => {
                assert_eq!(actual.as_deref(), Some(&[][..]));
            }
            CasOutcome::Updated => panic!("empty must conflict with create-only"),
        }
    }

    #[test]
    fn list_refs_returns_sorted_names() {
        let r = refs();
        for name in ["op_heads", "alpha", "zeta", "branch.lock"] {
            r.cas_ref(name, None, Some(&Bytes::from_static(b"x"))).unwrap();
        }
        let names = r.list_refs().unwrap();
        assert_eq!(names, vec!["alpha", "branch.lock", "op_heads", "zeta"]);
    }

    #[test]
    fn list_refs_on_empty_store_is_ok() {
        assert!(refs().list_refs().unwrap().is_empty());
    }

    #[test]
    fn cas_ref_rejects_bad_name() {
        let r = refs();
        for bad in ["", "a/b", "..", ".", "with\0nul"] {
            r.cas_ref(bad, None, Some(&Bytes::from_static(b"x")))
                .expect_err(bad);
        }
    }

    #[test]
    fn get_and_list_persist_across_clone() {
        // Cloning is cheap and shares the same Database — operations
        // on one clone are visible on the other (one writer, many
        // readers serialization is per-Database, not per-clone).
        let store = Store::new_in_memory();
        let r1 = LocalRefs::new(store.database());
        let r2 = r1.clone();
        r1.cas_ref("k", None, Some(&Bytes::from_static(b"v"))).unwrap();
        assert_eq!(r2.get_ref("k").unwrap().as_deref(), Some(&b"v"[..]));
    }
}
