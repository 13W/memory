//! UUIDv7 generation (spec 03 §1.1; RFC 9562 §5.7).
//!
//! New random durable IDs are UUIDv7: a 48-bit big-endian Unix-millisecond
//! timestamp in the high bits (so IDs are time-ordered and give good B-tree
//! locality) followed by 74 random bits, with the version (`0b0111`) and
//! variant (`0b10`) fields stamped per RFC 9562.
//!
//! The pure [`uuidv7_from`] core takes the timestamp and entropy as arguments,
//! so it is fully deterministic and golden-testable. The OS-backed
//! [`SystemUuidV7`] wraps it with the wall clock and `/dev/urandom`.

use std::fmt;

/// A 128-bit UUID, stored as its 16 big-endian bytes.
///
/// Ordering is bytewise, which for UUIDv7 matches both chronological order and
/// the lexicographic order of the [`fmt::Display`] string.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uuid([u8; 16]);

impl Uuid {
    /// The raw 16 bytes, big-endian.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// The 4-bit version field (7 for a well-formed UUIDv7).
    pub const fn version(&self) -> u8 {
        self.0[6] >> 4
    }

    /// The 2-bit variant field (`0b10` for the RFC 4122/9562 variant).
    pub const fn variant(&self) -> u8 {
        self.0[8] >> 6
    }

    /// The embedded 48-bit Unix-millisecond timestamp.
    pub const fn timestamp_ms(&self) -> u64 {
        (self.0[0] as u64) << 40
            | (self.0[1] as u64) << 32
            | (self.0[2] as u64) << 24
            | (self.0[3] as u64) << 16
            | (self.0[4] as u64) << 8
            | (self.0[5] as u64)
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uuid({self})")
    }
}

/// Build a UUIDv7 from a 48-bit Unix-millisecond timestamp and 10 random bytes
/// (RFC 9562 §5.7).
///
/// `now_ms` is truncated to its low 48 bits. The version (`0b0111`) and variant
/// (`0b10`) bits are stamped unconditionally, so any input yields a well-formed
/// v7 UUID; the remaining 74 bits carry `rand`.
pub fn uuidv7_from(now_ms: u64, rand: [u8; 10]) -> Uuid {
    let ts = now_ms & 0x0000_FFFF_FFFF_FFFF;
    let mut bytes = [0u8; 16];
    bytes[0] = (ts >> 40) as u8;
    bytes[1] = (ts >> 32) as u8;
    bytes[2] = (ts >> 24) as u8;
    bytes[3] = (ts >> 16) as u8;
    bytes[4] = (ts >> 8) as u8;
    bytes[5] = ts as u8;
    bytes[6..16].copy_from_slice(&rand);
    bytes[6] = 0x70 | (bytes[6] & 0x0F); // version 7 in the high nibble
    bytes[8] = 0x80 | (bytes[8] & 0x3F); // variant 0b10 in the top two bits
    Uuid(bytes)
}

/// A source of fresh UUIDv7 values.
///
/// Production wiring (registry, daemon) takes a `&dyn UuidSource` so tests can
/// inject a fixed sequence instead of the wall clock (mirroring the `IdSource`
/// seam in `test-support`).
pub trait UuidSource {
    /// Produce the next UUIDv7.
    fn next_uuid(&self) -> Uuid;
}

/// OS-backed UUIDv7 generator: `SystemTime` for the timestamp and
/// `/dev/urandom` for entropy.
///
/// Unix-only for now; other platforms' entropy sources land with the rest of
/// the Windows story (spec 02 §2.1 SID lookup is likewise deferred).
#[cfg(unix)]
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemUuidV7;

#[cfg(unix)]
impl UuidSource for SystemUuidV7 {
    fn next_uuid(&self) -> Uuid {
        uuidv7_from(system_now_ms(), os_random_10())
    }
}

#[cfg(unix)]
fn system_now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn os_random_10() -> [u8; 10] {
    use std::io::Read;
    let mut buf = [0u8; 10];
    // /dev/urandom never short-reads for small buffers; a failure here means the
    // OS entropy source is unavailable, which is unrecoverable for ID minting.
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("read entropy from /dev/urandom");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAND: [u8; 10] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA];

    #[test]
    fn golden_layout() {
        // now_ms = 0x0123456789AB, rand = 11 22 33 44 …; version/variant stamped
        // over the first bytes of rand_a / rand_b. Derived by hand from RFC 9562
        // §5.7, independent of the implementation.
        let uuid = uuidv7_from(0x0123_4567_89AB, RAND);
        assert_eq!(uuid.to_string(), "01234567-89ab-7122-b344-5566778899aa");
        assert_eq!(
            uuid.as_bytes(),
            &[
                0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0x71, 0x22, 0xB3, 0x44, 0x55, 0x66, 0x77, 0x88,
                0x99, 0xAA
            ],
        );
    }

    #[test]
    fn version_and_variant_are_stamped() {
        // All-zero and all-one entropy must still yield version 7, variant 0b10.
        for rand in [[0x00u8; 10], [0xFFu8; 10]] {
            let uuid = uuidv7_from(0, rand);
            assert_eq!(uuid.version(), 7);
            assert_eq!(uuid.variant(), 0b10);
        }
    }

    #[test]
    fn timestamp_round_trips_low_48_bits() {
        let uuid = uuidv7_from(0x0123_4567_89AB, RAND);
        assert_eq!(uuid.timestamp_ms(), 0x0123_4567_89AB);
        // High bits above 48 are dropped.
        let truncated = uuidv7_from(0xFFFF_0123_4567_89AB, RAND);
        assert_eq!(truncated.timestamp_ms(), 0x0123_4567_89AB);
    }

    #[test]
    fn ordering_follows_timestamp() {
        let earlier = uuidv7_from(1000, RAND);
        let later = uuidv7_from(2000, RAND);
        assert!(earlier < later);
        // Display order matches byte order for time-ordered IDs.
        assert!(earlier.to_string() < later.to_string());
    }

    #[test]
    fn display_is_uuid_shaped() {
        let id = uuidv7_from(42, RAND).to_string();
        assert_eq!(id.len(), 36);
        let groups: Vec<usize> = id.split('-').map(str::len).collect();
        assert_eq!(groups, vec![8, 4, 4, 4, 12]);
        assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
    }

    #[cfg(unix)]
    #[test]
    fn system_source_is_well_formed_and_distinct() {
        let source = SystemUuidV7;
        let a = source.next_uuid();
        let b = source.next_uuid();
        assert_eq!(a.version(), 7);
        assert_eq!(a.variant(), 0b10);
        assert_ne!(a, b, "entropy should make consecutive IDs distinct");
    }
}
