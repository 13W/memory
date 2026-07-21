//! The advisory fast-path stat cache (spec 06 §1).
//!
//! Spec 06 §1 `[FIXED]`: the fast-path cache is `(mtime, size, file_id)` per path,
//! **advisory only, and lives in memory**. "Any mismatch or doubt escalates the
//! file to content hashing." This module is that cache: a plain in-memory map from
//! a worktree-relative `normalized_path` to its last-observed [`StatKey`] and the
//! `content_hash` computed then. The scanner ([`super::scan`]) consults it in
//! [`ScanMode::Fast`](super::ScanMode) to skip re-reading unchanged files and
//! bypasses it entirely in [`ScanMode::Strict`](super::ScanMode).
//!
//! The cache never affects the manifest's *content* (a reused hash is by
//! construction the same hash re-reading would produce), only whether bytes are
//! read — so the manifest stays deterministic while the cache stays a pure
//! performance hint that may be dropped at any time (it is not persisted).

use std::collections::HashMap;
use std::fs::Metadata;
use std::time::SystemTime;

/// A file's fast-path identity tuple (spec 06 §1): modification time, size, and
/// the filesystem's file id (inode on Unix).
///
/// Fast-path reuse requires **all three** to match and the `file_id` to be known
/// (`Some`): a missing file id (`None` — a platform or filesystem that cannot
/// supply a stable one) counts as doubt and forces content hashing, never a false
/// reuse (spec 06 §1 "any mismatch or doubt escalates").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatKey {
    /// Last modification time (`Metadata::modified`), or `UNIX_EPOCH` if the
    /// platform could not report one.
    pub mtime: SystemTime,
    /// File size in bytes.
    pub size: u64,
    /// Stable filesystem file id (Unix inode), or `None` when unavailable.
    pub file_id: Option<u64>,
}

impl StatKey {
    /// Derive a [`StatKey`] from a file's [`Metadata`].
    ///
    /// `mtime` falls back to [`SystemTime::UNIX_EPOCH`] if the platform cannot
    /// report a modification time (a mismatch then simply forces a re-hash, which
    /// is safe because the cache is advisory).
    pub fn from_metadata(metadata: &Metadata) -> StatKey {
        StatKey {
            mtime: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: metadata.len(),
            file_id: file_id_of(metadata),
        }
    }

    /// Whether `self` is a *trustworthy* fast-path match for `current`: every
    /// field equal **and** the file id known. An unknown file id (`None`) is
    /// doubt, so it never matches (spec 06 §1).
    fn is_trustworthy_match(&self, current: &StatKey) -> bool {
        self.file_id.is_some() && self == current
    }
}

#[cfg(unix)]
fn file_id_of(metadata: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.ino())
}

#[cfg(not(unix))]
fn file_id_of(_metadata: &Metadata) -> Option<u64> {
    None
}

/// The in-memory, advisory fast-path cache (spec 06 §1): `normalized_path →
/// (StatKey, content_hash)`.
///
/// Owned by the long-lived reconcile scheduler (T05-04) and threaded into
/// [`super::scan`] by `&mut` so it survives across reconcile cycles; a cold start
/// passes an empty one. It is never persisted — losing it only makes the next scan
/// re-hash, never wrong.
#[derive(Debug, Default, Clone)]
pub struct StatCache {
    entries: HashMap<String, (StatKey, String)>,
}

impl StatCache {
    /// An empty cache (cold start).
    pub fn new() -> StatCache {
        StatCache::default()
    }

    /// The trusted `content_hash` for `normalized_path` if the cached [`StatKey`]
    /// is a trustworthy match for `current` (spec 06 §1); otherwise `None`
    /// (escalate to hashing).
    pub fn reuse(&self, normalized_path: &str, current: &StatKey) -> Option<&str> {
        let (cached_key, cached_hash) = self.entries.get(normalized_path)?;
        cached_key
            .is_trustworthy_match(current)
            .then_some(cached_hash.as_str())
    }

    /// Record `(normalized_path → key, content_hash)`, replacing any prior entry.
    pub fn record(&mut self, normalized_path: String, key: StatKey, content_hash: String) {
        self.entries.insert(normalized_path, (key, content_hash));
    }

    /// Directly seed an entry (tests: build a controlled cache without a scan).
    #[cfg(test)]
    pub fn seed(&mut self, normalized_path: &str, key: StatKey, content_hash: &str) {
        self.entries
            .insert(normalized_path.to_string(), (key, content_hash.to_string()));
    }

    /// Number of cached paths.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(size: u64, id: Option<u64>) -> StatKey {
        StatKey {
            mtime: SystemTime::UNIX_EPOCH,
            size,
            file_id: id,
        }
    }

    #[test]
    fn reuse_requires_full_match_and_known_file_id() {
        let mut cache = StatCache::new();
        cache.seed("a.rs", key(10, Some(1)), "hashA");

        // Exact match with a known file id → reuse.
        assert_eq!(cache.reuse("a.rs", &key(10, Some(1))), Some("hashA"));
        // Size differs → miss.
        assert_eq!(cache.reuse("a.rs", &key(11, Some(1))), None);
        // file id differs → miss.
        assert_eq!(cache.reuse("a.rs", &key(10, Some(2))), None);
        // Unknown current file id → doubt → miss.
        assert_eq!(cache.reuse("a.rs", &key(10, None)), None);
        // Absent path → miss.
        assert_eq!(cache.reuse("b.rs", &key(10, Some(1))), None);
    }

    #[test]
    fn unknown_cached_file_id_never_reuses() {
        let mut cache = StatCache::new();
        cache.seed("a.rs", key(10, None), "hashA");
        // Even an otherwise-identical current key cannot trust a None cached id.
        assert_eq!(cache.reuse("a.rs", &key(10, None)), None);
    }

    #[test]
    fn record_overwrites_prior_entry() {
        let mut cache = StatCache::new();
        cache.record("a.rs".to_string(), key(10, Some(1)), "old".to_string());
        cache.record("a.rs".to_string(), key(20, Some(1)), "new".to_string());
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.reuse("a.rs", &key(20, Some(1))), Some("new"));
        assert_eq!(cache.reuse("a.rs", &key(10, Some(1))), None);
    }
}
