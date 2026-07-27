//! LRSP segment header and frame wire format (spec 07 §2/§3).
//!
//! ```text
//! Segment header (16 bytes): magic "LRSP" | version u16 LE | flags u16 LE | reserved (8)
//! Frame (repeated): len u32 LE | crc32c u32 LE | payload (len bytes, canonical JSON UTF-8)
//! ```
//!
//! Shared between the hook write path (`local-rag-hook`, T13-02) and the
//! daemon-side read path (`local-rag-store::spool`, T13-03) so both encode and
//! decode against exactly one CRC algorithm and header/frame layout — the same
//! "single shared component" posture this crate already takes for
//! [`redaction::Scanner`] (reused by file classification, spool ingestion, and
//! remote transmission) and [`identity::domain`]'s hashing. This module was
//! originally built inside `local-rag-hook` (write-only, T13-02); T13-03
//! relocated the wire-format primitives here — verbatim, no behavior change —
//! specifically so the new decoder (daemon-side, `local-rag-store`) never
//! risks drifting from the encoder on the CRC table or header/frame layout.
//! `local-rag-hook` remains the sole owner of the higher-level write pipeline
//! (redaction, identity computation, segment rotation/locking) that produces
//! the bytes this module encodes.
//!
//! # `payload` is a JSON string, not a nested object
//!
//! Spec 07 §3's illustration shows `"payload": { /* redacted event body */ }`
//! — a literal nested object. But `local_rag_hook::payload::PreparedPayload`
//! (T13-01) only guarantees its redacted/capped bytes are valid **UTF-8**, not
//! valid JSON (a truncation can land mid-structure). Embedding that content as
//! a raw nested object would make the *whole frame* invalid JSON whenever a
//! payload happened to be capped. Encoding `payload` as an ordinary JSON
//! **string** (double-encoded — a string whose unescaped content is itself
//! JSON text in the common, uncapped case) sidesteps this entirely: the outer
//! frame is structurally always valid JSON by construction of
//! `serde_json::to_vec` over a typed struct, regardless of what happened to
//! the inner content. `[SPEC]` amendment to 07 §3's illustration.

/// Segment header magic (spec 07 §3).
pub const MAGIC: [u8; 4] = *b"LRSP";
/// Segment wire format version (spec 07 §3 `[SPEC]`).
pub const FORMAT_VERSION: u16 = 1;
/// Segment header length in bytes (spec 07 §3).
pub const HEADER_LEN: usize = 16;
/// Frame cap: "larger frames are invalid by format" (spec 07 §2). Not reachable
/// via the normal hook pipeline (256 KiB `PAYLOAD_CAP_BYTES`, T13-01, plus a few
/// hundred bytes of envelope fields, is nowhere near 1 MiB) — this is an
/// internal safety-net invariant, not a realistically-hit path.
pub const MAX_FRAME_PAYLOAD_BYTES: usize = 1024 * 1024;

/// The 16-byte segment header (spec 07 §3).
pub fn encode_segment_header() -> [u8; HEADER_LEN] {
    let mut buf = [0u8; HEADER_LEN];
    buf[0..4].copy_from_slice(&MAGIC);
    buf[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    // buf[6..8] flags = 0, buf[8..16] reserved: already zeroed.
    buf
}

/// A decoded segment header (spec 07 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    /// The wire format version this segment was written with.
    pub version: u16,
    /// Reserved flags bitfield — always `0` in `FORMAT_VERSION` 1.
    pub flags: u16,
}

/// A failure decoding a segment header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// Fewer than [`HEADER_LEN`] bytes were available.
    Truncated,
    /// The first 4 bytes are not [`MAGIC`].
    BadMagic,
    /// The header's `version` is newer than this build supports — a
    /// "reportable incompatibility, not silent loss" (spec 11 §4): a newer
    /// container format may have restructured the frame layout itself, so no
    /// frame in this segment is attempted.
    UnsupportedFormatVersion {
        /// The version found in the header.
        found: u16,
        /// The newest version this build's decoder accepts (== [`FORMAT_VERSION`]).
        max_supported: u16,
    },
}

impl std::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderError::Truncated => {
                write!(f, "segment header is truncated (< {HEADER_LEN} bytes)")
            }
            HeaderError::BadMagic => write!(f, "segment header magic does not match {MAGIC:?}"),
            HeaderError::UnsupportedFormatVersion {
                found,
                max_supported,
            } => write!(
                f,
                "segment format version {found} is newer than the {max_supported} this build supports"
            ),
        }
    }
}

impl std::error::Error for HeaderError {}

/// Decode and validate a segment header from the start of `bytes` (spec 07
/// §3). The inverse of [`encode_segment_header`]; symmetric error handling —
/// an unsupported (newer) version is a distinctly named variant, never folded
/// into a generic corruption bucket.
pub fn decode_segment_header(bytes: &[u8]) -> Result<SegmentHeader, HeaderError> {
    if bytes.len() < HEADER_LEN {
        return Err(HeaderError::Truncated);
    }
    if bytes[0..4] != MAGIC {
        return Err(HeaderError::BadMagic);
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version > FORMAT_VERSION {
        return Err(HeaderError::UnsupportedFormatVersion {
            found: version,
            max_supported: FORMAT_VERSION,
        });
    }
    let flags = u16::from_le_bytes([bytes[6], bytes[7]]);
    Ok(SegmentHeader { version, flags })
}

/// A failure building a frame.
#[derive(Debug)]
pub enum FrameError {
    /// The serialized payload exceeds [`MAX_FRAME_PAYLOAD_BYTES`].
    PayloadTooLarge {
        /// The offending serialized length.
        len: usize,
    },
    /// The [`FramePayload`] could not be serialized to JSON.
    Serialize(serde_json::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::PayloadTooLarge { len } => {
                write!(f, "frame payload {len} bytes exceeds the 1 MiB frame cap")
            }
            FrameError::Serialize(e) => write!(f, "frame payload serialization failed: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// One observation, as a frame's payload fields (spec 07 §3's frame payload
/// fields). Field order matches the spec's illustration — `serde_json`'s
/// derived `Serialize` emits fields in declaration order, so that order is
/// also the wire byte order ("golden wire bytes" pins this). `Deserialize` is
/// order-insensitive (only `Serialize`'s declared order is byte-stability
/// sensitive) and rejects unknown fields, making strong typing the schema
/// check for identity-critical fields — no bespoke per-field validation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FramePayload {
    pub format_version: u32,
    pub source_event_id: String,
    pub dedup_key: Option<String>,
    pub event_type: String,
    pub captured_at: i64,
    pub session_id: String,
    pub agent_id: Option<String>,
    /// Always `None` at write time — no v0 caller derives a turn identity yet.
    pub turn_id: Option<String>,
    /// Always `None` at write time — no v0 caller derives a batch identity yet.
    pub batch_id: Option<String>,
    pub worktree_root: Option<String>,
    /// Always `None` — git introspection is a daemon-side concern (spec 02
    /// §3.3/03 §2.1 as-built notes: `local-rag-store` carries no git
    /// dependency, and the hook must stay exec-fast, spec 13 §1 "<50 ms cold").
    pub commit: Option<String>,
    pub evidence_kind: String,
    pub trust: String,
    pub paths: Vec<String>,
    /// The redaction scanner version that produced `payload` (spec 12 §2
    /// `[SPEC]` "versioned `redaction_version` recorded in envelopes").
    /// `None` for an envelope-only (denied) event: a denied event's payload is
    /// never scanned at all, so no scanner version applies (D-019).
    pub redaction_version: Option<u32>,
    /// The redacted event body, JSON-encoded as a string (see module docs).
    /// `None` for an envelope-only (denied) event.
    pub payload: Option<String>,
    /// Left unpopulated at write time: the 4 KiB evidence-excerpt cap is
    /// group 14's (spec 12 §2's as-built note, confirmed again by T13-01's
    /// evidence), a distinct field from this group's 256 KiB payload cap.
    pub short_evidence_excerpt: Option<String>,
}

/// Encode `payload` to a `len ‖ crc32c ‖ payload` frame (spec 07 §3).
pub fn encode_frame(payload: &FramePayload) -> Result<Vec<u8>, FrameError> {
    let json = serde_json::to_vec(payload).map_err(FrameError::Serialize)?;
    encode_frame_bytes(&json)
}

/// Encode already-serialized JSON bytes to a frame. Split from [`encode_frame`]
/// so the `>1 MiB` safety net is testable directly with a synthetic payload,
/// bypassing `FramePayload`/`prepare_payload` entirely.
pub fn encode_frame_bytes(payload_json: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload_json.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(FrameError::PayloadTooLarge {
            len: payload_json.len(),
        });
    }
    let mut out = Vec::with_capacity(8 + payload_json.len());
    out.extend_from_slice(&(payload_json.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32c(payload_json).to_le_bytes());
    out.extend_from_slice(payload_json);
    Ok(out)
}

// ---- CRC-32C (Castagnoli) ---------------------------------------------------
//
// Hand-rolled, dependency-free (no `crc`/`crc32c`/`crc32fast` crate exists
// anywhere in this workspace's `Cargo.lock`) — the same posture the redaction
// scanner (T03-02) takes for its own small, well-specified algorithm rather
// than adding an external crate. The bit-reflected Castagnoli polynomial
// `0x82F63B78` is CRC-32C (used by iSCSI/ext4/SCTP), distinct from the
// classic CRC-32 (zlib/gzip) polynomial.

const CRC32C_POLY: u32 = 0x82F6_3B78;

const fn crc32c_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC32C_POLY
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC32C_TABLE: [u32; 256] = crc32c_table();

/// CRC-32C (Castagnoli) checksum, as used by the frame format (spec 07 §3).
///
/// Known-answer test: `crc32c(b"123456789") == 0xE3069283`.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = (crc >> 8) ^ CRC32C_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize];
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_known_answer_tests() {
        assert_eq!(
            crc32c(b"123456789"),
            0xE306_9283,
            "the standard CRC-32C KAT"
        );
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"a"), 0xC1D0_4330);
        assert_eq!(crc32c(b"hello"), 0x9A71_BB4C);
        assert_eq!(crc32c(b"local-rag"), 0x63C4_A6ED);
    }

    #[test]
    fn crc32c_is_deterministic() {
        let data = b"deterministic payload bytes";
        assert_eq!(crc32c(data), crc32c(data));
    }

    #[test]
    fn segment_header_is_byte_exact() {
        let header = encode_segment_header();
        let mut expected = [0u8; HEADER_LEN];
        expected[0..4].copy_from_slice(b"LRSP");
        expected[4] = 0x01; // version = 1, LE
        expected[5] = 0x00;
        // flags (6..8) and reserved (8..16) are zero.
        assert_eq!(header, expected);
    }

    #[test]
    fn frame_bytes_are_byte_exact() {
        let payload = b"{\"a\":1}";
        let frame = encode_frame_bytes(payload).expect("under cap");
        let mut expected = Vec::new();
        expected.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        expected.extend_from_slice(&crc32c(payload).to_le_bytes());
        expected.extend_from_slice(payload);
        assert_eq!(frame, expected);
        // len ‖ crc32c ‖ payload, in that order.
        assert_eq!(&frame[0..4], &(payload.len() as u32).to_le_bytes());
        assert_eq!(&frame[4..8], &crc32c(payload).to_le_bytes());
        assert_eq!(&frame[8..], payload);
    }

    #[test]
    fn frame_over_one_mib_is_rejected() {
        let oversized = vec![b'x'; MAX_FRAME_PAYLOAD_BYTES + 1];
        let err = encode_frame_bytes(&oversized).expect_err("must reject");
        assert!(matches!(
            err,
            FrameError::PayloadTooLarge { len } if len == MAX_FRAME_PAYLOAD_BYTES + 1
        ));
    }

    #[test]
    fn frame_at_exactly_one_mib_is_accepted() {
        let exact = vec![b'x'; MAX_FRAME_PAYLOAD_BYTES];
        assert!(encode_frame_bytes(&exact).is_ok());
    }

    fn sample_payload() -> FramePayload {
        FramePayload {
            format_version: 1,
            source_event_id: "pt:s:t:ok".to_string(),
            dedup_key: Some("pt:s:t:ok".to_string()),
            event_type: "PostToolUse".to_string(),
            captured_at: 1_700_000_000_000,
            session_id: "s".to_string(),
            agent_id: None,
            turn_id: None,
            batch_id: None,
            worktree_root: Some("/repo".to_string()),
            commit: None,
            evidence_kind: "tool_result".to_string(),
            trust: "normal".to_string(),
            paths: vec!["src/a.ts".to_string()],
            redaction_version: Some(1),
            payload: Some("{\"tool_output\":\"ok\"}".to_string()),
            short_evidence_excerpt: None,
        }
    }

    #[test]
    fn frame_payload_serializes_in_declared_field_order() {
        let fp = sample_payload();
        let bytes = encode_frame(&fp).expect("under cap");
        // Payload begins after the 8-byte len/crc prefix.
        let json = std::str::from_utf8(&bytes[8..]).expect("utf-8");
        let expected = "{\"format_version\":1,\"source_event_id\":\"pt:s:t:ok\",\"dedup_key\":\"pt:s:t:ok\",\"event_type\":\"PostToolUse\",\"captured_at\":1700000000000,\"session_id\":\"s\",\"agent_id\":null,\"turn_id\":null,\"batch_id\":null,\"worktree_root\":\"/repo\",\"commit\":null,\"evidence_kind\":\"tool_result\",\"trust\":\"normal\",\"paths\":[\"src/a.ts\"],\"redaction_version\":1,\"payload\":\"{\\\"tool_output\\\":\\\"ok\\\"}\",\"short_evidence_excerpt\":null}";
        assert_eq!(json, expected);
    }

    #[test]
    fn frame_payload_redaction_version_is_none_for_envelope_only() {
        let mut fp = sample_payload();
        fp.redaction_version = None;
        fp.payload = None;
        let bytes = encode_frame(&fp).expect("under cap");
        let json = std::str::from_utf8(&bytes[8..]).expect("utf-8");
        assert!(json.contains("\"redaction_version\":null"));
        assert!(json.contains("\"payload\":null"));
        let decoded: FramePayload = serde_json::from_slice(&bytes[8..]).expect("deserialize");
        assert_eq!(decoded.redaction_version, None);
    }

    #[test]
    fn frame_payload_round_trips_through_serde() {
        let fp = sample_payload();
        let json = serde_json::to_vec(&fp).expect("serialize");
        let decoded: FramePayload = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(decoded, fp);
    }

    #[test]
    fn decode_segment_header_accepts_current_version() {
        let header = encode_segment_header();
        let decoded = decode_segment_header(&header).expect("valid header");
        assert_eq!(
            decoded,
            SegmentHeader {
                version: FORMAT_VERSION,
                flags: 0
            }
        );
    }

    #[test]
    fn decode_segment_header_rejects_bad_magic() {
        let mut header = encode_segment_header();
        header[0] = b'X';
        assert_eq!(decode_segment_header(&header), Err(HeaderError::BadMagic));
    }

    #[test]
    fn decode_segment_header_rejects_truncated_header() {
        let header = encode_segment_header();
        assert_eq!(
            decode_segment_header(&header[..HEADER_LEN - 1]),
            Err(HeaderError::Truncated)
        );
        assert_eq!(decode_segment_header(&[]), Err(HeaderError::Truncated));
    }

    #[test]
    fn decode_segment_header_reports_unsupported_newer_version() {
        let mut header = encode_segment_header();
        let newer = FORMAT_VERSION + 1;
        header[4..6].copy_from_slice(&newer.to_le_bytes());
        assert_eq!(
            decode_segment_header(&header),
            Err(HeaderError::UnsupportedFormatVersion {
                found: newer,
                max_supported: FORMAT_VERSION,
            })
        );
    }
}
