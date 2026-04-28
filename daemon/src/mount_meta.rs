//! Persistent per-mount metadata (M8 / Layer B).
//!
//! Each mount's redb file lives at
//! `<storage_dir>/mounts/<hash(wc_path)>/store.redb`. Alongside it,
//! `mount.toml` holds the small bag of per-mount state the daemon
//! otherwise keeps in memory: working-copy path, remote, op id,
//! workspace id, current root tree id.
//!
//! The store is content-addressed (id = hash(value)); an out-of-date
//! root_tree_id in `mount.toml` is recoverable by re-running snapshot.
//! The op id is jj-managed and must be in lockstep with what jj-lib
//! wrote into the WC's `.jj/`. We update `mount.toml` on every
//! `SetCheckoutState` and `Snapshot` so a daemon restart sees the
//! same `op_id` / `root_tree_id` the previous instance handed out.
//!
//! Format choice: TOML, atomically rewritten via `<file>.tmp` +
//! rename. Bytes are hex-encoded since TOML has no native byte type.
//! Performance is fine — these writes only happen on mutating RPCs
//! (~once per `jj` command), and at ~200 bytes each they're well below
//! the page-cache threshold.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::hash::blake3_bytes;

/// Subdirectory name under `storage_dir` that holds all mounts.
pub const MOUNTS_DIR: &str = "mounts";

/// File name (under each mount's directory) that holds the metadata.
pub const META_FILE: &str = "mount.toml";

/// File name (under each mount's directory) that holds the redb store.
pub const STORE_FILE: &str = "store.redb";

/// On-disk shape of `mount.toml`. All `Vec<u8>` fields round-trip as
/// lowercase hex strings; empty bytes serialize as `""`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MountMetadata {
    /// Canonical working-copy path. Redundant with the parent
    /// directory's name (which is `hash(working_copy_path)`), but
    /// useful for reverse lookup and humans grepping the storage dir.
    pub working_copy_path: String,
    /// `Initialize.remote`. Surfaced via `DaemonStatus`.
    pub remote: String,
    /// Last operation id pushed by the CLI via `SetCheckoutState`.
    /// Empty until first set. Hex-encoded.
    #[serde(default, with = "hex_bytes")]
    pub op_id: Vec<u8>,
    /// Workspace identifier (proto bytes). Empty until first set.
    /// Hex-encoded.
    #[serde(default, with = "hex_bytes")]
    pub workspace_id: Vec<u8>,
    /// Currently checked-out root tree id (32 bytes). Hex-encoded.
    #[serde(default, with = "hex_bytes")]
    pub root_tree_id: Vec<u8>,
}

impl MountMetadata {
    /// Atomically write the metadata to `path`, creating parent
    /// directories as needed. Uses the standard tmp-then-rename idiom:
    /// readers either see the previous version or the new one — never
    /// a torn write.
    pub fn write_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self).context("serializing mount.toml")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| {
            format!("renaming {} -> {}", tmp.display(), path.display())
        })?;
        Ok(())
    }

    /// Read the metadata at `path`. Returns the parsed struct or an
    /// error chain that names the file.
    pub fn read_from(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let meta: Self = toml::from_str(&body)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(meta)
    }
}

/// Path-safe stable id for a working-copy path. Uses blake3 truncated
/// to 16 hex chars (64 bits).
pub fn mount_dir_name(working_copy_path: &str) -> String {
    let hash = blake3_bytes(working_copy_path.as_bytes());
    let mut s = String::with_capacity(16);
    for b in &hash.as_bytes()[..8] {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// `<storage_dir>/mounts/<hash>/`. Both the metadata file and the
/// redb store live in here.
pub fn mount_dir(storage_dir: &Path, working_copy_path: &str) -> PathBuf {
    storage_dir
        .join(MOUNTS_DIR)
        .join(mount_dir_name(working_copy_path))
}

pub fn meta_path(storage_dir: &Path, working_copy_path: &str) -> PathBuf {
    mount_dir(storage_dir, working_copy_path).join(META_FILE)
}

pub fn store_path(storage_dir: &Path, working_copy_path: &str) -> PathBuf {
    mount_dir(storage_dir, working_copy_path).join(STORE_FILE)
}

/// Iterate every `mount.toml` under `<storage_dir>/mounts/`. Ignores
/// entries that fail to parse — they're logged at the call site so an
/// orphan/garbage subdir doesn't take the daemon down at startup.
pub fn list_persisted(storage_dir: &Path) -> Result<Vec<(PathBuf, MountMetadata)>> {
    let mounts_dir = storage_dir.join(MOUNTS_DIR);
    if !mounts_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&mounts_dir)
        .with_context(|| format!("reading {}", mounts_dir.display()))?
    {
        let entry = entry.context("reading mounts dir entry")?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let meta_file = dir.join(META_FILE);
        if !meta_file.exists() {
            continue;
        }
        match MountMetadata::read_from(&meta_file) {
            Ok(meta) => out.push((dir, meta)),
            Err(e) => {
                tracing::warn!(
                    path = %meta_file.display(),
                    error = %format!("{e:#}"),
                    "skipping unreadable mount.toml during rehydrate"
                );
            }
        }
    }
    // Stable order keeps `DaemonStatus` deterministic across restarts.
    out.sort_by(|a, b| a.1.working_copy_path.cmp(&b.1.working_copy_path));
    Ok(out)
}

/// Hex-encode/decode `Vec<u8>` for serde. Empty vec → empty string.
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(&mut out, "{b:02x}");
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s: String = String::deserialize(d)?;
        if s.is_empty() {
            return Ok(Vec::new());
        }
        if !s.len().is_multiple_of(2) {
            return Err(serde::de::Error::custom(
                "hex string length must be even",
            ));
        }
        let mut out = Vec::with_capacity(s.len() / 2);
        let bytes = s.as_bytes();
        for i in (0..bytes.len()).step_by(2) {
            let hi = decode_nibble(bytes[i])
                .ok_or_else(|| serde::de::Error::custom("invalid hex digit"))?;
            let lo = decode_nibble(bytes[i + 1])
                .ok_or_else(|| serde::de::Error::custom("invalid hex digit"))?;
            out.push((hi << 4) | lo);
        }
        Ok(out)
    }

    fn decode_nibble(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MountMetadata {
        MountMetadata {
            working_copy_path: "/tmp/repo".into(),
            remote: "localhost".into(),
            op_id: vec![0xab, 0xcd],
            workspace_id: b"default".to_vec(),
            root_tree_id: vec![0xff; 32],
        }
    }

    #[test]
    fn roundtrip_via_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mount.toml");
        sample().write_to(&path).expect("write");
        let got = MountMetadata::read_from(&path).expect("read");
        assert_eq!(got, sample());
    }

    #[test]
    fn empty_byte_fields_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mount.toml");
        let mut m = sample();
        m.op_id = Vec::new();
        m.workspace_id = Vec::new();
        m.write_to(&path).expect("write");
        let got = MountMetadata::read_from(&path).expect("read");
        assert!(got.op_id.is_empty() && got.workspace_id.is_empty());
    }

    #[test]
    fn list_persisted_skips_unreadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = dir.path();

        // Good mount
        let good_dir = mount_dir(storage, "/tmp/good");
        std::fs::create_dir_all(&good_dir).expect("mkdir good");
        let mut good = sample();
        good.working_copy_path = "/tmp/good".into();
        good.write_to(&good_dir.join(META_FILE)).expect("write good");

        // Bad mount: dir exists but mount.toml is garbage
        let bad_dir = storage.join(MOUNTS_DIR).join("garbage");
        std::fs::create_dir_all(&bad_dir).expect("mkdir bad");
        std::fs::write(bad_dir.join(META_FILE), "this isn't toml: ===")
            .expect("write garbage");

        let out = list_persisted(storage).expect("list");
        assert_eq!(out.len(), 1, "good entry survives, bad is skipped");
        assert_eq!(out[0].1.working_copy_path, "/tmp/good");
    }

    #[test]
    fn returns_empty_when_no_mounts_dir_yet() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = list_persisted(dir.path()).expect("list");
        assert!(out.is_empty());
    }

    #[test]
    fn rejects_odd_length_hex() {
        // Hand-crafted TOML with an odd-length hex string.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mount.toml");
        let body = r#"working_copy_path = "/tmp/x"
remote = ""
op_id = "abc"
workspace_id = ""
root_tree_id = ""
"#;
        std::fs::write(&path, body).expect("write");
        let err = MountMetadata::read_from(&path).expect_err("must reject odd length");
        let chained = format!("{err:#}");
        assert!(chained.contains("even"), "got: {chained}");
    }
}
