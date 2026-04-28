//! Portable, stable hashing suitable for identifying values
//!
//! Copied from the Jujutsu source code and modified for blake3.
//! Might give up on keeping daemon a seperate code base?

use jj_lib::content_hash::ContentHash;

/// The 512-bit BLAKE2b content hash
pub fn blake3(x: &(impl ContentHash + ?Sized + std::fmt::Debug)) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    x.hash(&mut hasher);
    hasher.finalize()
}

/// Hash an opaque byte slice with blake3. Used to derive stable
/// path-safe identifiers (e.g. per-mount storage subdirectories) from
/// arbitrary input.
pub fn blake3_bytes(bytes: &[u8]) -> blake3::Hash {
    blake3::hash(bytes)
}
