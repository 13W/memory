//! LRSP segment header and frame wire format (spec 07 §2/§3).
//!
//! ```text
//! Segment header (16 bytes): magic "LRSP" | version u16 LE | flags u16 LE | reserved (8)
//! Frame (repeated): len u32 LE | crc32c u32 LE | payload (len bytes, canonical JSON UTF-8)
//! ```
//!
//! This module only **encodes** — the hook write path is the sole producer of
//! these bytes. Decoding (a bounded streaming reader that stops at a torn
//! tail) is a distinct design surface owned by T13-03 on the daemon side;
//! building it here would preempt decisions (buffering, partial-read handling,
//! format-version negotiation) that task should make. "Golden wire bytes"
//! tests assert exact byte equality against hand-built expectations instead of
//! round-tripping through a decoder.
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

use crate::payload::PreparedPayload;

/// Segment header magic (spec 07 §3).
pub const MAGIC: [u8; 4] = *b"LRSP";
/// Segment wire format version (spec 07 §3 `[SPEC]`).
pub const FORMAT_VERSION: u16 = 1;
/// Segment header length in bytes (spec 07 §3).
pub const HEADER_LEN: usize = 16;
/// Frame cap: "larger frames are invalid by format" (spec 07 §2). Not reachable
/// via the normal pipeline (256 KiB `PAYLOAD_CAP_BYTES`, T13-01, plus a few
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

/// One observation, ready to be embedded as a frame's payload bytes (spec 07
/// §3's frame payload fields). Field order matches the spec's illustration —
/// `serde_json`'s derived `Serialize` emits fields in declaration order, so
/// that order is also the wire byte order ("golden wire bytes" pins this).
#[derive(Debug, Clone, serde::Serialize)]
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
    /// The redacted event body, JSON-encoded as a string (see module docs).
    /// `None` for an envelope-only (denied) event.
    pub payload: Option<String>,
    /// Left unpopulated at write time: the 4 KiB evidence-excerpt cap is
    /// group 14's (spec 12 §2's as-built note, confirmed again by T13-01's
    /// evidence), a distinct field from this group's 256 KiB payload cap.
    pub short_evidence_excerpt: Option<String>,
}

/// Fold a [`PreparedPayload`] into the frame's `payload` field (`None` for
/// [`PreparedPayload::EnvelopeOnly`], a JSON string of the redacted bytes for
/// [`PreparedPayload::Included`]).
///
/// `String::from_utf8_lossy` rather than a fallible `from_utf8` even though
/// `prepare_payload` already guarantees valid UTF-8: this binary's mandate is
/// "never panic", so a future regression upstream should degrade to
/// lossy-but-safe output, not crash the hook.
pub fn payload_field(prepared: &PreparedPayload) -> Option<String> {
    match prepared {
        PreparedPayload::EnvelopeOnly => None,
        PreparedPayload::Included { bytes, .. } => {
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
    }
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

    #[test]
    fn payload_field_is_none_for_envelope_only() {
        assert_eq!(payload_field(&PreparedPayload::EnvelopeOnly), None);
    }

    #[test]
    fn payload_field_wraps_included_bytes_as_a_string() {
        let prepared = PreparedPayload::Included {
            bytes: b"{\"k\":\"v\"}".to_vec(),
            redaction_version: 1,
            secrets_found: 0,
            truncation: None,
        };
        assert_eq!(payload_field(&prepared), Some("{\"k\":\"v\"}".to_string()));
    }

    #[test]
    fn frame_payload_serializes_in_declared_field_order() {
        let fp = FramePayload {
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
            payload: Some("{\"tool_output\":\"ok\"}".to_string()),
            short_evidence_excerpt: None,
        };
        let bytes = encode_frame(&fp).expect("under cap");
        // Payload begins after the 8-byte len/crc prefix.
        let json = std::str::from_utf8(&bytes[8..]).expect("utf-8");
        let expected = "{\"format_version\":1,\"source_event_id\":\"pt:s:t:ok\",\"dedup_key\":\"pt:s:t:ok\",\"event_type\":\"PostToolUse\",\"captured_at\":1700000000000,\"session_id\":\"s\",\"agent_id\":null,\"turn_id\":null,\"batch_id\":null,\"worktree_root\":\"/repo\",\"commit\":null,\"evidence_kind\":\"tool_result\",\"trust\":\"normal\",\"paths\":[\"src/a.ts\"],\"payload\":\"{\\\"tool_output\\\":\\\"ok\\\"}\",\"short_evidence_excerpt\":null}";
        assert_eq!(json, expected);
    }
}
