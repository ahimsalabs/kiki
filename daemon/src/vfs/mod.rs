//! Per-mount virtual filesystem.
//!
//! Layout (mirrors `docs/PLAN.md` §4.3):
//!
//! - [`kiki_fs`] — `JjKikiFs` trait + concrete `KikiFs` impl. Owns the inode
//!   slab and walks the content store. The interesting code lives here.
//! - [`inode`] — the slab itself. Stable, monotonic `u64` ids that fit
//!   both `nfsserve::nfs::fileid3` and `fuse3::Inode`.
//! - [`nfs_adapter`] — adapter onto `nfsserve::vfs::NFSFileSystem`. macOS
//!   primary path; also useful on Linux for testing the read path
//!   without a kernel mount.
//!
//! The FUSE adapter (Linux primary) lands later in M3 — it reuses
//! `JjKikiFs` unchanged.

pub mod fuse_adapter;
mod inode;
// nfs_adapter is the macOS transport. Compile it on macOS, and on every
// platform under `cfg(test)` so the read-side tests run on Linux CI too.
// Without this, Linux lib builds emit dead-code warnings for every
// helper inside, since nothing uses them outside of tests.
#[cfg(any(target_os = "macos", test))]
pub mod nfs_adapter;
mod kiki_fs;

// Re-exports kept tight: only the symbols `vfs_mgr.rs` and `service.rs`
// need at the crate root. The full per-module surface is reachable via
// `crate::vfs::nfs_adapter::*` / `crate::vfs::fuse_adapter::*` for the
// platform-agnostic unit tests.
//
// The transport adapters are platform-gated to mirror `vfs_mgr`: Linux
// uses FUSE, macOS uses NFS. The unused module is still compiled (so
// the cross-platform read-side tests run on either OS) but the
// crate-root re-export is gated to keep `unused_imports` quiet.
#[cfg(target_os = "linux")]
pub use fuse_adapter::FuseAdapter;
pub use inode::ROOT_INODE;
#[cfg(target_os = "macos")]
pub use nfs_adapter::NfsAdapter;
pub use kiki_fs::{FsError, JjKikiFs, KikiFs};
// `FileKind` is consumed only by tests today (the M10 §10.6 service-
// level read-through tests that walk a `readdir` result for a kind
// assertion). Re-exported under `cfg(test)` so production builds
// don't see an "unused import" warning while keeping the symbol
// reachable from the existing `crate::vfs::FileKind` import path.
#[cfg(test)]
pub use kiki_fs::FileKind;
