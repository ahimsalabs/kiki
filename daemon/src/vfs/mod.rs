//! Per-mount virtual filesystem.
//!
//! Layout (mirrors `docs/PLAN.md` §4.3):
//!
//! - [`yak_fs`] — `JjYakFs` trait + concrete `YakFs` impl. Owns the inode
//!   slab and walks the content store. The interesting code lives here.
//! - [`inode`] — the slab itself. Stable, monotonic `u64` ids that fit
//!   both `nfsserve::nfs::fileid3` and `fuse3::Inode`.
//! - [`nfs_adapter`] — adapter onto `nfsserve::vfs::NFSFileSystem`. macOS
//!   primary path; also useful on Linux for testing the read path
//!   without a kernel mount.
//!
//! The FUSE adapter (Linux primary) lands later in M3 — it reuses
//! `JjYakFs` unchanged.

pub mod fuse_adapter;
mod inode;
pub mod nfs_adapter;
mod yak_fs;

// Re-exports kept tight: only the symbols `vfs_mgr.rs` and (eventually)
// `service.rs` need at the crate root. The full per-module surface is
// reachable as `crate::vfs::yak_fs::*` for tests and the FUSE adapter.
#[allow(unused_imports)] // FuseAdapter isn't wired up to a mount until M4.
pub use fuse_adapter::FuseAdapter;
pub use inode::ROOT_INODE;
pub use nfs_adapter::NfsAdapter;
pub use yak_fs::{JjYakFs, YakFs};
