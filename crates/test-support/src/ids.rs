//! Reproducible identifier sources for deterministic tests.
//!
//! Durable identifiers in production are UUID-shaped. Tests need a source that
//! is (a) reproducible from a seed and (b) free of the external `uuid` crate, to
//! keep the workspace dependency-free. [`SeqUuids`] formats a 128-bit
//! `(seed, counter)` pair into an RFC-4122-shaped string; the same seed always
//! yields the same sequence.

use std::sync::atomic::{AtomicU64, Ordering};

/// A source of identifier strings.
pub trait IdSource {
    /// Produce the next identifier.
    fn next_id(&self) -> String;
}

/// A deterministic, seeded generator of UUID-shaped identifiers.
///
/// The high 64 bits carry the seed and the low 64 bits carry a per-instance
/// counter, so two generators created with the same seed emit byte-identical
/// sequences.
///
/// ```
/// use local_rag_test_support::{IdSource, SeqUuids};
/// let a = SeqUuids::seeded(7);
/// let b = SeqUuids::seeded(7);
/// assert_eq!(a.next_id(), b.next_id());
/// assert_eq!(a.next_id(), b.next_id());
/// // Distinct within one generator.
/// let source = SeqUuids::seeded(7);
/// assert_ne!(source.next_id(), source.next_id());
/// ```
#[derive(Debug)]
pub struct SeqUuids {
    seed: u64,
    counter: AtomicU64,
}

impl SeqUuids {
    /// Create a generator whose output is fully determined by `seed`.
    pub fn seeded(seed: u64) -> Self {
        Self {
            seed,
            counter: AtomicU64::new(0),
        }
    }
}

impl IdSource for SeqUuids {
    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&self.seed.to_be_bytes());
        bytes[8..].copy_from_slice(&n.to_be_bytes());
        format_uuid(&bytes)
    }
}

/// Format 16 bytes as a lowercase `8-4-4-4-12` hex string.
fn format_uuid(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (i, b) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_is_uuid_shaped() {
        let id = SeqUuids::seeded(0).next_id();
        assert_eq!(id.len(), 36);
        let groups: Vec<&str> = id.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
    }
}
