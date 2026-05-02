//! M10.6: `KikiOpStore` — jj-lib's `OpStore` routed through the daemon.
//!
//! Wraps a [`SimpleOpStore`] as a local serialization delegate and cache.
//! On write, the delegate serializes + content-hashes the domain object
//! and writes to disk; we read back the bytes and push them to the daemon
//! (which pushes to the remote via write-through). On read, we try the
//! local delegate first; on miss we fetch from the daemon (which fetches
//! from the remote via read-through), write the bytes to the delegate's
//! disk path so the delegate can deserialize, and return the result.
//!
//! This avoids reimplementing jj-lib's private `view_to_proto` /
//! `operation_to_proto` / `view_from_proto` / `operation_from_proto`
//! conversion functions (~300 lines tightly coupled to jj-lib internals).
//! The delegate's `store_path` is the standard `<wc>/.jj/repo/op_store/`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use jj_lib::object_id::{HexPrefix, ObjectId, PrefixResolution};
use jj_lib::op_store::{
    OpStore, OpStoreError, OpStoreResult, Operation, OperationId, View, ViewId,
};
use jj_lib::simple_op_store::{SimpleOpStore, SimpleOpStoreInitError};
use pollster::FutureExt as _;

use crate::blocking_client::BlockingJujutsuInterfaceClient;

/// Operation ID length in bytes (BLAKE2b-512).
const OPERATION_ID_LENGTH: usize = 64;
/// View ID length in bytes (BLAKE2b-512).
const VIEW_ID_LENGTH: usize = 64;

#[derive(Debug)]
pub struct KikiOpStore {
    /// Local serialization delegate + cache. Handles proto encoding/
    /// decoding and content hashing. The store_path is the standard
    /// `<wc>/.jj/repo/op_store/`.
    delegate: SimpleOpStore,
    /// Path to the delegate's store directory. Kept separately because
    /// `SimpleOpStore` doesn't expose it.
    store_path: PathBuf,
    client: BlockingJujutsuInterfaceClient,
    working_copy_path: String,
    root_operation_id: OperationId,
    root_view_id: ViewId,
}

impl KikiOpStore {
    pub fn name() -> &'static str {
        "kiki_op_store"
    }

    /// Create a new op store (init path). Creates the base directories.
    pub fn init(
        store_path: &Path,
        root_data: jj_lib::op_store::RootOperationData,
        client: BlockingJujutsuInterfaceClient,
        working_copy_path: String,
    ) -> Result<Self, SimpleOpStoreInitError> {
        let delegate = SimpleOpStore::init(store_path, root_data)?;
        Ok(Self {
            delegate,
            store_path: store_path.to_path_buf(),
            client,
            working_copy_path,
            root_operation_id: OperationId::from_bytes(&[0; OPERATION_ID_LENGTH]),
            root_view_id: ViewId::from_bytes(&[0; VIEW_ID_LENGTH]),
        })
    }

    /// Load an existing op store (factory load path).
    pub fn load(
        store_path: &Path,
        root_data: jj_lib::op_store::RootOperationData,
        client: BlockingJujutsuInterfaceClient,
        working_copy_path: String,
    ) -> Self {
        let delegate = SimpleOpStore::load(store_path, root_data);
        Self {
            delegate,
            store_path: store_path.to_path_buf(),
            client,
            working_copy_path,
            root_operation_id: OperationId::from_bytes(&[0; OPERATION_ID_LENGTH]),
            root_view_id: ViewId::from_bytes(&[0; VIEW_ID_LENGTH]),
        }
    }

    fn views_dir(&self) -> PathBuf {
        self.store_path.join("views")
    }

    fn operations_dir(&self) -> PathBuf {
        self.store_path.join("operations")
    }

    /// Map a tonic status to an OpStoreError for read operations.
    fn read_error(e: tonic::Status, object_type: &str, id: &str) -> OpStoreError {
        OpStoreError::ReadObject {
            object_type: object_type.to_string(),
            hash: id.to_string(),
            source: e.to_string().into(),
        }
    }

    /// Map a tonic status to an OpStoreError for write operations.
    /// jj-lib 0.40 `WriteObject::object_type` is `&'static str`.
    fn write_error(e: tonic::Status, object_type: &'static str) -> OpStoreError {
        OpStoreError::WriteObject {
            object_type,
            source: e.to_string().into(),
        }
    }
}

#[async_trait]
impl OpStore for KikiOpStore {
    fn name(&self) -> &str {
        Self::name()
    }

    fn root_operation_id(&self) -> &OperationId {
        &self.root_operation_id
    }

    async fn read_view(&self, id: &ViewId) -> OpStoreResult<View> {
        // Root view: delegate handles it (synthetic, never stored).
        if *id == self.root_view_id {
            return self.delegate.read_view(id).block_on();
        }

        // Try local first (delegate reads from disk).
        if let Ok(view) = self.delegate.read_view(id).block_on() {
            return Ok(view);
        }

        // Fetch from daemon (which fetches from remote on miss).
        let resp = self
            .client
            .read_view(proto::jj_interface::ReadViewReq {
                working_copy_path: self.working_copy_path.clone(),
                view_id: id.as_bytes().to_vec(),
            })
            .map_err(|e| Self::read_error(e, "view", &id.hex()))?
            .into_inner();

        if !resp.found {
            return Err(OpStoreError::ObjectNotFound {
                object_type: "view".to_string(),
                hash: id.hex(),
                source: "not found on daemon or remote".into(),
            });
        }

        // Write bytes to local cache so delegate can deserialize.
        let view_path = self.views_dir().join(id.hex());
        std::fs::write(&view_path, &resp.data).map_err(|e| OpStoreError::ReadObject {
            object_type: "view".to_string(),
            hash: id.hex(),
            source: e.into(),
        })?;

        // Deserialize via delegate.
        self.delegate.read_view(id).block_on()
    }

    async fn write_view(&self, view: &View) -> OpStoreResult<ViewId> {
        // Delegate serializes, content-hashes, and writes to disk.
        let id = self.delegate.write_view(view).block_on()?;

        // Read back bytes from disk and push to daemon.
        let view_path = self.views_dir().join(id.hex());
        let bytes = std::fs::read(&view_path).map_err(|e| OpStoreError::WriteObject {
            object_type: "view",
            source: e.into(),
        })?;

        self.client
            .write_view(proto::jj_interface::WriteViewReq {
                working_copy_path: self.working_copy_path.clone(),
                view_id: id.as_bytes().to_vec(),
                data: bytes,
            })
            .map_err(|e| Self::write_error(e, "view"))?;

        Ok(id)
    }

    async fn read_operation(&self, id: &OperationId) -> OpStoreResult<Operation> {
        // Root operation: delegate handles it.
        if *id == self.root_operation_id {
            return self.delegate.read_operation(id).block_on();
        }

        // Try local first.
        if let Ok(op) = self.delegate.read_operation(id).block_on() {
            return Ok(op);
        }

        // Fetch from daemon.
        let resp = self
            .client
            .read_operation(proto::jj_interface::ReadOperationReq {
                working_copy_path: self.working_copy_path.clone(),
                operation_id: id.as_bytes().to_vec(),
            })
            .map_err(|e| Self::read_error(e, "operation", &id.hex()))?
            .into_inner();

        if !resp.found {
            return Err(OpStoreError::ObjectNotFound {
                object_type: "operation".to_string(),
                hash: id.hex(),
                source: "not found on daemon or remote".into(),
            });
        }

        // Write to local cache.
        let op_path = self.operations_dir().join(id.hex());
        std::fs::write(&op_path, &resp.data).map_err(|e| OpStoreError::ReadObject {
            object_type: "operation".to_string(),
            hash: id.hex(),
            source: e.into(),
        })?;

        self.delegate.read_operation(id).block_on()
    }

    async fn write_operation(&self, operation: &Operation) -> OpStoreResult<OperationId> {
        // Delegate serializes, content-hashes, and writes to disk.
        let id = self.delegate.write_operation(operation).block_on()?;

        // Read back bytes and push to daemon.
        let op_path = self.operations_dir().join(id.hex());
        let bytes = std::fs::read(&op_path).map_err(|e| OpStoreError::WriteObject {
            object_type: "operation",
            source: e.into(),
        })?;

        self.client
            .write_operation(proto::jj_interface::WriteOperationReq {
                working_copy_path: self.working_copy_path.clone(),
                operation_id: id.as_bytes().to_vec(),
                data: bytes,
            })
            .map_err(|e| Self::write_error(e, "operation"))?;

        Ok(id)
    }

    async fn resolve_operation_id_prefix(
        &self,
        prefix: &HexPrefix,
    ) -> OpStoreResult<PrefixResolution<OperationId>> {
        // Check root operation first (not in daemon's table).
        let matches_root = prefix.matches(&self.root_operation_id);

        // Ask daemon (scans its redb table, which has all locally-
        // written + remotely-fetched ops).
        let resp = self
            .client
            .resolve_operation_id_prefix(
                proto::jj_interface::ResolveOperationIdPrefixReq {
                    working_copy_path: self.working_copy_path.clone(),
                    hex_prefix: prefix.hex(),
                },
            )
            .map_err(|e| OpStoreError::Other(e.to_string().into()))?
            .into_inner();

        let daemon_result = match resp.resolution {
            0 => PrefixResolution::NoMatch,
            1 => {
                let id = OperationId::new(resp.full_id);
                PrefixResolution::SingleMatch(id)
            }
            2 => PrefixResolution::AmbiguousMatch,
            _ => {
                return Err(OpStoreError::Other(
                    format!("unexpected resolution value: {}", resp.resolution).into(),
                ))
            }
        };

        // Merge root match with daemon result.
        match (matches_root, daemon_result) {
            (false, result) => Ok(result),
            (true, PrefixResolution::NoMatch) => {
                Ok(PrefixResolution::SingleMatch(self.root_operation_id.clone()))
            }
            (true, PrefixResolution::SingleMatch(id)) => {
                if id == self.root_operation_id {
                    Ok(PrefixResolution::SingleMatch(id))
                } else {
                    Ok(PrefixResolution::AmbiguousMatch)
                }
            }
            (true, PrefixResolution::AmbiguousMatch) => Ok(PrefixResolution::AmbiguousMatch),
        }
    }

    async fn gc(&self, head_ids: &[OperationId], keep_newer: SystemTime) -> OpStoreResult<()> {
        // Delegate to SimpleOpStore's gc for local cleanup.
        // Remote gc is out of scope (M10.6 §10.6.1).
        self.delegate.gc(head_ids, keep_newer).block_on()
    }
}
