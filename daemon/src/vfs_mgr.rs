// VFS manager scaffolding. The handle/message-channel plumbing below is not
// yet driven by any caller — `VfsManagerHandle::bind` is the future entry
// point used by the gRPC service to mount a workspace. Keeping the shape
// in place (rather than deleting and re-deriving it) avoids churn when the
// NFS milestone wires this up; suppress the dead-code warnings explicitly so
// future work doesn't have to fight `#![deny(warnings)]` if it ever lands.
#![allow(dead_code)]

use std::sync::Arc;

use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use rand::Rng;
use tokio::sync::mpsc;

use crate::store::Store;
use crate::vfs::{JjYakFs, NfsAdapter, YakFs};

pub struct VfsManager {
    config: VfsManagerConfig,
    tx: mpsc::UnboundedSender<VfsManagerMessage>,
    rx: mpsc::UnboundedReceiver<VfsManagerMessage>,
}

pub struct VfsManagerConfig {
    pub min_nfs_port: usize,
    pub max_nfs_port: usize,
}

enum VfsManagerMessage {
    Bind,
}

/// Handle to the VFS Manager service
pub struct VfsManagerHandle(mpsc::UnboundedSender<VfsManagerMessage>);

impl VfsManagerHandle {
    pub fn bind(&self) -> anyhow::Result<()> {
        self.0.send(VfsManagerMessage::Bind)?;
        Ok(())
    }
}

impl VfsManager {
    pub fn new(config: VfsManagerConfig) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        VfsManager { config, tx, rx }
    }

    pub fn handle(&self) -> VfsManagerHandle {
        VfsManagerHandle(self.tx.clone())
    }

    pub async fn serve(&mut self) -> Result<(), std::io::Error> {
        while let Some(msg) = self.rx.recv().await {
            match msg {
                VfsManagerMessage::Bind => {
                    let port = rand::thread_rng()
                        .gen_range(self.config.min_nfs_port..self.config.max_nfs_port);
                    // M3 keeps the empty-mount stub: `Bind` is still not
                    // sent by anyone (M4 wires that up), but if it were,
                    // it would now serve a `YakFs` over a fresh empty
                    // store rather than the placeholder `VirtualFileSystem`.
                    // M4 will swap this for a per-mount `Arc<dyn JjYakFs>`
                    // handed in by the gRPC service.
                    let store = Arc::new(Store::new());
                    let empty_tree = store.get_empty_tree_id();
                    let yak: Arc<dyn JjYakFs> = Arc::new(YakFs::new(store, empty_tree));
                    let adapter = NfsAdapter::new(yak);
                    let _join_handle = tokio::spawn(async move {
                        // The bind itself is plumbing for an upcoming
                        // milestone, but the unwrap below would crash the
                        // daemon on any port collision. Once `bind` is
                        // wired up, this should propagate the error back
                        // through the channel response.
                        let listener =
                            NFSTcpListener::bind(&format!("127.0.0.1:{port}"), adapter)
                                .await
                                .expect("NFS listener bind failed (TODO: surface to caller)");
                        listener.handle_forever().await
                    });
                }
            }
        }
        unreachable!();
    }
}
