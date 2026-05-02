//! Path-safe hashing for mount directory names.
//!
//! After git convergence, content hashing is handled by git's SHA-1 via
//! [`crate::git_store::GitContentStore`]. This module retains only the
//! `blake3_bytes` helper used by [`crate::mount_meta`] for deriving
//! stable directory names from working-copy paths.

/// Hash an opaque byte slice with blake3. Used to derive stable
/// path-safe identifiers (e.g. per-mount storage subdirectories) from
/// arbitrary input.
pub fn blake3_bytes(bytes: &[u8]) -> blake3::Hash {
    blake3::hash(bytes)
}
