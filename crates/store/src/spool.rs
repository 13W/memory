//! Daemon-side decoder for LRSP spool segments (spec 07 §2/§5) — the read-side
//! counterpart to `local-rag-hook`'s segment writer (T13-02). A pure
//! filesystem-bytes → in-memory-struct transform: no database/DDL awareness.
//! The `observation_envelope`/`observation_path`/`observation_payload`/
//! `spool_import_cursor` DDL (spec 03 §2.5) and the actual transactional
//! import are T13-04's ("Transactional importer/cursor"), which consumes this
//! module's decoded, classified output.
//!
//! Operates on byte slices, not `std::io::Read`: segments are hard-capped at
//! 8 MiB (`local_rag_hook`'s rotation threshold) and no consumer needs partial
//! results before an unread tail is fully available — T13-04's own batch
//! design reads a session's un-imported suffix once per batch. "Streaming" in
//! the group-13 card's wording is read as *sequential incremental frame
//! consumption with a bounded, clean stop*, not unbounded-source I/O; this
//! also keeps symmetry with the encoder's own byte-oriented API
//! (`local_rag_core::spool::encode_frame_bytes`).
//!
//! # Torn tail vs. corruption
//!
//! A frame's `len` is checked against [`local_rag_core::spool::MAX_FRAME_PAYLOAD_BYTES`]
//! **before** checking whether enough trailing bytes exist. An impossible
//! length can never come from a legitimate in-progress write, so it is
//! corruption regardless of what follows; only a *legal* `len` with
//! insufficient trailing bytes is a torn tail (spec 07 §2: "the appending hook
//! holds the flock until its frame is complete, so no valid frame can follow a
//! torn one within a segment"). A buffer that ends exactly on a frame boundary
//! is [`StopReason::EndOfInput`], deliberately distinct from
//! [`StopReason::TornTail`].

use local_rag_core::spool::{self, FramePayload, HeaderError};

/// A decoded observation's dedup identity, per spec 07 §4's stable/best-effort
/// split — a named, testable classification rather than an ad-hoc
/// `dedup_key.is_some()` check scattered at call sites (matching this
/// codebase's "single shared component" posture, e.g.
/// `local_rag_core::redaction::Scanner`, `tokenize_identifier`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DedupClass {
    /// `PostToolUse`/`PostToolUseFailure`/`SubagentStop`: `dedup_key` present,
    /// deduplicates exactly (UNIQUE, at import — T13-04).
    Stable { dedup_key: String },
    /// `UserPromptSubmit`/`Stop`/`SessionStart`/`SessionEnd`: `dedup_key` is
    /// `null`; dedup is a bounded import-side window (T13-04), not exact.
    BestEffort,
}

/// One successfully decoded frame, classified and ready for T13-04 to import.
#[derive(Debug, Clone)]
pub struct DecodedObservation {
    pub payload: FramePayload,
    pub classification: DedupClass,
    /// Byte offset of this frame's `len` field, relative to the slice passed
    /// to [`decode_frames`]/the post-header remainder passed to [`decode_segment`].
    pub frame_offset: usize,
    /// Total byte length of this frame (`8 + payload.len()`).
    pub frame_len: usize,
}

/// `payload.event_type`/`dedup_key` are internally inconsistent with spec 07
/// §4's table — a corrupted or (future) buggy frame claiming a shape it
/// shouldn't have, caught here rather than silently poisoning T13-04's
/// `UNIQUE(dedup_key)` logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassificationError {
    /// `event_type` is not one of spec 07 §1's seven capture-set members.
    UnrecognizedEventType(String),
    /// `event_type`'s expected stability (spec 07 §4) disagrees with whether
    /// `dedup_key` is actually present.
    DedupKeyEventTypeMismatch {
        event_type: String,
        dedup_key_present: bool,
    },
}

impl std::fmt::Display for ClassificationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClassificationError::UnrecognizedEventType(t) => {
                write!(f, "unrecognized event_type {t:?}")
            }
            ClassificationError::DedupKeyEventTypeMismatch {
                event_type,
                dedup_key_present,
            } => write!(
                f,
                "event_type {event_type:?} disagrees with dedup_key present={dedup_key_present}"
            ),
        }
    }
}

impl std::error::Error for ClassificationError {}

/// Why decoding a frame at a given offset failed.
#[derive(Debug)]
pub enum FrameDecodeError {
    /// The payload's CRC-32C does not match the frame's recorded checksum.
    Crc { offset: usize },
    /// The frame's `len` exceeds [`local_rag_core::spool::MAX_FRAME_PAYLOAD_BYTES`]
    /// — corruption, never a torn tail (see module docs).
    LengthExceedsCap { offset: usize, len: u32 },
    /// The payload bytes are not valid UTF-8.
    Utf8 { offset: usize },
    /// The payload is valid UTF-8 JSON but does not deserialize into
    /// [`FramePayload`]'s shape (missing/mistyped identity-critical field, or
    /// an unknown field — `#[serde(deny_unknown_fields)]`).
    MalformedPayload {
        offset: usize,
        source: serde_json::Error,
    },
    /// The frame's own `format_version` disagrees with the segment header's
    /// version — a defensive cross-check (spec 11 §4's version-negotiation
    /// concern), distinct from the primary header-level rejection in
    /// [`decode_segment`].
    FrameFormatVersionMismatch {
        offset: usize,
        header_version: u16,
        frame_version: u32,
    },
    /// The frame decoded and deserialized cleanly but failed identity
    /// classification (see [`ClassificationError`]).
    Classification {
        offset: usize,
        source: ClassificationError,
    },
}

impl std::fmt::Display for FrameDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameDecodeError::Crc { offset } => write!(f, "frame at offset {offset}: CRC mismatch"),
            FrameDecodeError::LengthExceedsCap { offset, len } => write!(
                f,
                "frame at offset {offset}: length {len} exceeds the frame cap"
            ),
            FrameDecodeError::Utf8 { offset } => {
                write!(f, "frame at offset {offset}: payload is not valid UTF-8")
            }
            FrameDecodeError::MalformedPayload { offset, source } => write!(
                f,
                "frame at offset {offset}: payload does not match the expected shape: {source}"
            ),
            FrameDecodeError::FrameFormatVersionMismatch {
                offset,
                header_version,
                frame_version,
            } => write!(
                f,
                "frame at offset {offset}: format_version {frame_version} disagrees with header version {header_version}"
            ),
            FrameDecodeError::Classification { offset, source } => {
                write!(f, "frame at offset {offset}: {source}")
            }
        }
    }
}

impl std::error::Error for FrameDecodeError {}

/// Why frame decoding stopped.
#[derive(Debug)]
pub enum StopReason {
    /// The buffer ended exactly on a frame boundary — nothing more to read,
    /// and no partial bytes trailing (never confused with [`StopReason::TornTail`]).
    EndOfInput,
    /// A legal-length frame is missing some or all of its trailing bytes — a
    /// non-durable, in-progress write (spec 07 §2 `[FIXED]`). Not an error:
    /// the importer resumes here later once the writer completes the frame.
    TornTail,
    /// A frame at the current offset is corrupt in a way that is not a torn
    /// tail (see [`FrameDecodeError`]'s variants).
    Corrupt(FrameDecodeError),
}

/// The result of decoding as many whole frames as possible from a byte slice.
#[derive(Debug)]
pub struct SegmentTailDecode {
    /// Every frame successfully decoded and classified before `stop_reason`.
    pub frames: Vec<DecodedObservation>,
    /// How many bytes of the input were consumed by `frames` — the cursor's
    /// new `committed_offset` is the caller's starting offset plus this.
    pub bytes_consumed: usize,
    pub stop_reason: StopReason,
}

/// Decode a whole segment: validate the 16-byte header first (spec 07 §3),
/// then decode frames from the remainder. If the header reports a
/// newer-than-supported format version, this returns `Err` immediately —
/// **zero** frames are attempted, since a newer container format may have
/// restructured the frame layout itself ("reportable incompatibility, not
/// silent loss", spec 11 §4).
pub fn decode_segment(bytes: &[u8]) -> Result<SegmentTailDecode, HeaderError> {
    let header = spool::decode_segment_header(bytes)?;
    let rest = &bytes[spool::HEADER_LEN..];
    let mut decoded = decode_frames(rest, header.version);
    decoded.bytes_consumed += spool::HEADER_LEN;
    Ok(decoded)
}

/// Decode frames from `bytes` (the segment's content **after** its header),
/// given the segment header's `version` for the per-frame cross-check. For a
/// resumed read (cursor offset already past the header and some already-
/// imported frames), a caller passes the remaining un-imported suffix
/// directly.
pub fn decode_frames(bytes: &[u8], header_version: u16) -> SegmentTailDecode {
    let mut offset = 0usize;
    let mut frames = Vec::new();

    loop {
        if offset == bytes.len() {
            return SegmentTailDecode {
                frames,
                bytes_consumed: offset,
                stop_reason: StopReason::EndOfInput,
            };
        }
        if bytes.len() - offset < 8 {
            return SegmentTailDecode {
                frames,
                bytes_consumed: offset,
                stop_reason: StopReason::TornTail,
            };
        }

        let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let expected_crc = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());

        // Checked before the trailing-bytes check: an impossible length can
        // never be a legitimate in-progress write (see module docs).
        if len as usize > spool::MAX_FRAME_PAYLOAD_BYTES {
            return SegmentTailDecode {
                frames,
                bytes_consumed: offset,
                stop_reason: StopReason::Corrupt(FrameDecodeError::LengthExceedsCap {
                    offset,
                    len,
                }),
            };
        }

        let payload_start = offset + 8;
        let payload_end = payload_start + len as usize;
        if bytes.len() < payload_end {
            return SegmentTailDecode {
                frames,
                bytes_consumed: offset,
                stop_reason: StopReason::TornTail,
            };
        }

        let payload_bytes = &bytes[payload_start..payload_end];
        if spool::crc32c(payload_bytes) != expected_crc {
            return SegmentTailDecode {
                frames,
                bytes_consumed: offset,
                stop_reason: StopReason::Corrupt(FrameDecodeError::Crc { offset }),
            };
        }

        let payload_str = match std::str::from_utf8(payload_bytes) {
            Ok(s) => s,
            Err(_) => {
                return SegmentTailDecode {
                    frames,
                    bytes_consumed: offset,
                    stop_reason: StopReason::Corrupt(FrameDecodeError::Utf8 { offset }),
                };
            }
        };

        let payload: FramePayload = match serde_json::from_str(payload_str) {
            Ok(p) => p,
            Err(source) => {
                return SegmentTailDecode {
                    frames,
                    bytes_consumed: offset,
                    stop_reason: StopReason::Corrupt(FrameDecodeError::MalformedPayload {
                        offset,
                        source,
                    }),
                };
            }
        };

        if payload.format_version != u32::from(header_version) {
            return SegmentTailDecode {
                frames,
                bytes_consumed: offset,
                stop_reason: StopReason::Corrupt(FrameDecodeError::FrameFormatVersionMismatch {
                    offset,
                    header_version,
                    frame_version: payload.format_version,
                }),
            };
        }

        let classification = match classify(&payload) {
            Ok(c) => c,
            Err(source) => {
                return SegmentTailDecode {
                    frames,
                    bytes_consumed: offset,
                    stop_reason: StopReason::Corrupt(FrameDecodeError::Classification {
                        offset,
                        source,
                    }),
                };
            }
        };

        frames.push(DecodedObservation {
            payload,
            classification,
            frame_offset: offset,
            frame_len: payload_end - offset,
        });
        offset = payload_end;
    }
}

/// Spec 07 §4's stable/best-effort table, cross-checked against the frame's
/// actual `dedup_key` presence (see [`ClassificationError`]).
fn classify(payload: &FramePayload) -> Result<DedupClass, ClassificationError> {
    let expected_stable = match payload.event_type.as_str() {
        "PostToolUse" | "PostToolUseFailure" | "SubagentStop" => true,
        "UserPromptSubmit" | "Stop" | "SessionStart" | "SessionEnd" => false,
        other => {
            return Err(ClassificationError::UnrecognizedEventType(
                other.to_string(),
            ));
        }
    };
    match (&payload.dedup_key, expected_stable) {
        (Some(key), true) => Ok(DedupClass::Stable {
            dedup_key: key.clone(),
        }),
        (None, false) => Ok(DedupClass::BestEffort),
        (dedup_key, _) => Err(ClassificationError::DedupKeyEventTypeMismatch {
            event_type: payload.event_type.clone(),
            dedup_key_present: dedup_key.is_some(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(event_type: &str, source_event_id: &str, dedup_key: Option<&str>) -> FramePayload {
        FramePayload {
            format_version: 1,
            source_event_id: source_event_id.to_string(),
            dedup_key: dedup_key.map(str::to_string),
            event_type: event_type.to_string(),
            captured_at: 1_700_000_000_000,
            session_id: "s1".to_string(),
            agent_id: None,
            turn_id: None,
            batch_id: None,
            worktree_root: Some("/repo".to_string()),
            commit: None,
            evidence_kind: "tool_result".to_string(),
            trust: "normal".to_string(),
            paths: vec![],
            payload: None,
            short_evidence_excerpt: None,
        }
    }

    fn encode(fp: &FramePayload) -> Vec<u8> {
        spool::encode_frame(fp).expect("under cap")
    }

    fn decode_ok(bytes: &[u8]) -> SegmentTailDecode {
        let d = decode_frames(bytes, spool::FORMAT_VERSION);
        assert!(
            matches!(d.stop_reason, StopReason::EndOfInput),
            "expected clean end of input, got {:?}",
            d.stop_reason
        );
        d
    }

    // ---- each identity table row (spec 07 §4) -------------------------------

    #[test]
    fn post_tool_use_is_stable() {
        let fp = fixture("PostToolUse", "pt:s1:t1:ok", Some("pt:s1:t1:ok"));
        let decoded = decode_ok(&encode(&fp));
        assert_eq!(decoded.frames.len(), 1);
        assert_eq!(decoded.frames[0].payload.source_event_id, "pt:s1:t1:ok");
        assert_eq!(
            decoded.frames[0].classification,
            DedupClass::Stable {
                dedup_key: "pt:s1:t1:ok".to_string()
            }
        );
    }

    #[test]
    fn post_tool_use_failure_is_stable() {
        let fp = fixture("PostToolUseFailure", "pt:s1:t1:fail", Some("pt:s1:t1:fail"));
        let decoded = decode_ok(&encode(&fp));
        assert_eq!(
            decoded.frames[0].classification,
            DedupClass::Stable {
                dedup_key: "pt:s1:t1:fail".to_string()
            }
        );
    }

    #[test]
    fn subagent_stop_is_stable() {
        let fp = fixture("SubagentStop", "ss:s1:a1:3", Some("ss:s1:a1:3"));
        let decoded = decode_ok(&encode(&fp));
        assert_eq!(
            decoded.frames[0].classification,
            DedupClass::Stable {
                dedup_key: "ss:s1:a1:3".to_string()
            }
        );
    }

    #[test]
    fn user_prompt_submit_is_best_effort() {
        let fp = fixture("UserPromptSubmit", "up:s1:abc:42", None);
        let decoded = decode_ok(&encode(&fp));
        assert_eq!(decoded.frames[0].classification, DedupClass::BestEffort);
    }

    #[test]
    fn stop_is_best_effort() {
        let fp = fixture("Stop", "st:s1:abc:7", None);
        let decoded = decode_ok(&encode(&fp));
        assert_eq!(decoded.frames[0].classification, DedupClass::BestEffort);
    }

    #[test]
    fn session_start_is_best_effort() {
        let fp = fixture("SessionStart", "se:s1:start:5", None);
        let decoded = decode_ok(&encode(&fp));
        assert_eq!(decoded.frames[0].classification, DedupClass::BestEffort);
    }

    #[test]
    fn session_end_is_best_effort() {
        let fp = fixture("SessionEnd", "se:s1:end:5", None);
        let decoded = decode_ok(&encode(&fp));
        assert_eq!(decoded.frames[0].classification, DedupClass::BestEffort);
    }

    // ---- identical prompts remain best-effort -------------------------------

    #[test]
    fn identical_prompts_both_decode_as_best_effort_without_collision() {
        let fp = fixture("UserPromptSubmit", "up:s1:samehash:100", None);
        let mut bytes = encode(&fp);
        bytes.extend_from_slice(&encode(&fp));
        let decoded = decode_ok(&bytes);
        assert_eq!(decoded.frames.len(), 2);
        for f in &decoded.frames {
            assert_eq!(f.payload.source_event_id, "up:s1:samehash:100");
            assert_eq!(f.classification, DedupClass::BestEffort);
        }
    }

    // ---- CRC/len/version/UTF-8 errors ---------------------------------------

    #[test]
    fn crc_mismatch_is_reported_and_stops_decode() {
        let fp = fixture("Stop", "st:s1:x:1", None);
        let mut bytes = encode(&fp);
        // Flip a byte inside the payload region (after the 8-byte prefix)
        // without recomputing the CRC.
        let payload_start = 8;
        bytes[payload_start] ^= 0xFF;
        let decoded = decode_frames(&bytes, spool::FORMAT_VERSION);
        assert!(decoded.frames.is_empty());
        assert!(matches!(
            decoded.stop_reason,
            StopReason::Corrupt(FrameDecodeError::Crc { offset: 0 })
        ));
    }

    #[test]
    fn length_field_exceeding_cap_is_reported_distinctly_from_torn_tail() {
        let mut bytes = Vec::new();
        let bad_len = (spool::MAX_FRAME_PAYLOAD_BYTES + 1) as u32;
        bytes.extend_from_slice(&bad_len.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // crc, irrelevant
        let decoded = decode_frames(&bytes, spool::FORMAT_VERSION);
        assert!(decoded.frames.is_empty());
        assert!(matches!(
            decoded.stop_reason,
            StopReason::Corrupt(FrameDecodeError::LengthExceedsCap { offset: 0, len }) if len == bad_len
        ));
    }

    #[test]
    fn invalid_utf8_payload_is_reported_distinctly_from_crc_mismatch() {
        // Invalid UTF-8 bytes with a *matching* recomputed CRC, so the CRC
        // check does not shortcut past the UTF-8 check.
        let payload: &[u8] = &[0xFF, 0xFE, 0x00];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&spool::crc32c(payload).to_le_bytes());
        bytes.extend_from_slice(payload);
        let decoded = decode_frames(&bytes, spool::FORMAT_VERSION);
        assert!(decoded.frames.is_empty());
        assert!(matches!(
            decoded.stop_reason,
            StopReason::Corrupt(FrameDecodeError::Utf8 { offset: 0 })
        ));
    }

    #[test]
    fn malformed_json_payload_is_reported_distinctly() {
        // Valid CRC, valid UTF-8, but not a `FramePayload` shape at all.
        let payload = b"{\"not\":\"a frame payload\"}";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&spool::crc32c(payload).to_le_bytes());
        bytes.extend_from_slice(payload);
        let decoded = decode_frames(&bytes, spool::FORMAT_VERSION);
        assert!(decoded.frames.is_empty());
        assert!(matches!(
            decoded.stop_reason,
            StopReason::Corrupt(FrameDecodeError::MalformedPayload { offset: 0, .. })
        ));
    }

    #[test]
    fn frame_format_version_mismatch_is_reported_distinctly() {
        let mut fp = fixture("Stop", "st:s1:x:1", None);
        fp.format_version = 999;
        let bytes = encode(&fp);
        // Header claims FORMAT_VERSION (1), frame claims 999.
        let decoded = decode_frames(&bytes, spool::FORMAT_VERSION);
        assert!(decoded.frames.is_empty());
        assert!(matches!(
            decoded.stop_reason,
            StopReason::Corrupt(FrameDecodeError::FrameFormatVersionMismatch {
                offset: 0,
                header_version,
                frame_version: 999,
            }) if header_version == spool::FORMAT_VERSION
        ));
    }

    #[test]
    fn unrecognized_event_type_is_a_classification_error() {
        let fp = fixture("PreCompact", "se:s1:x:1", None);
        let bytes = encode(&fp);
        let decoded = decode_frames(&bytes, spool::FORMAT_VERSION);
        assert!(matches!(
            decoded.stop_reason,
            StopReason::Corrupt(FrameDecodeError::Classification {
                offset: 0,
                source: ClassificationError::UnrecognizedEventType(ref t),
            }) if t == "PreCompact"
        ));
    }

    #[test]
    fn dedup_key_event_type_mismatch_is_a_classification_error() {
        // A "stable" event type whose dedup_key is missing.
        let fp = fixture("PostToolUse", "pt:s1:t1:ok", None);
        let bytes = encode(&fp);
        let decoded = decode_frames(&bytes, spool::FORMAT_VERSION);
        assert!(matches!(
            decoded.stop_reason,
            StopReason::Corrupt(FrameDecodeError::Classification {
                offset: 0,
                source: ClassificationError::DedupKeyEventTypeMismatch {
                    dedup_key_present: false,
                    ..
                },
            })
        ));
    }

    // ---- torn tail -----------------------------------------------------------

    #[test]
    fn torn_tail_at_prefix_stops_cleanly() {
        let bytes = [0u8, 1, 2]; // fewer than 8 bytes total
        let decoded = decode_frames(&bytes, spool::FORMAT_VERSION);
        assert!(decoded.frames.is_empty());
        assert_eq!(decoded.bytes_consumed, 0);
        assert!(matches!(decoded.stop_reason, StopReason::TornTail));
    }

    #[test]
    fn torn_tail_mid_payload_stops_cleanly() {
        let fp = fixture("Stop", "st:s1:x:1", None);
        let full = encode(&fp);
        // Keep the 8-byte prefix (claiming the full length) but only half the
        // payload bytes — a legal `len` with insufficient trailing bytes.
        let torn = &full[..8 + (full.len() - 8) / 2];
        let decoded = decode_frames(torn, spool::FORMAT_VERSION);
        assert!(decoded.frames.is_empty());
        assert_eq!(decoded.bytes_consumed, 0);
        assert!(matches!(decoded.stop_reason, StopReason::TornTail));
    }

    #[test]
    fn torn_tail_after_several_good_frames_preserves_them() {
        let fp = fixture("Stop", "st:s1:x:1", None);
        let one_frame = encode(&fp);
        let mut bytes = Vec::new();
        for _ in 0..3 {
            bytes.extend_from_slice(&one_frame);
        }
        let good_len = bytes.len();
        // Append a torn 4th frame: a valid-looking prefix, half the payload.
        bytes.extend_from_slice(&one_frame[..8 + (one_frame.len() - 8) / 2]);

        let decoded = decode_frames(&bytes, spool::FORMAT_VERSION);
        assert_eq!(decoded.frames.len(), 3);
        assert_eq!(decoded.bytes_consumed, good_len);
        assert!(matches!(decoded.stop_reason, StopReason::TornTail));
    }

    #[test]
    fn clean_end_of_input_is_distinguished_from_torn_tail() {
        let fp = fixture("Stop", "st:s1:x:1", None);
        let mut bytes = encode(&fp);
        bytes.extend_from_slice(&encode(&fp));
        let decoded = decode_frames(&bytes, spool::FORMAT_VERSION);
        assert_eq!(decoded.frames.len(), 2);
        assert_eq!(decoded.bytes_consumed, bytes.len());
        assert!(matches!(decoded.stop_reason, StopReason::EndOfInput));
    }

    // ---- newer format incompatibility diagnostic ------------------------------

    #[test]
    fn decode_segment_stops_immediately_on_unsupported_header_version_without_attempting_any_frames()
     {
        let mut header = spool::encode_segment_header();
        let newer = spool::FORMAT_VERSION + 1;
        header[4..6].copy_from_slice(&newer.to_le_bytes());

        let fp = fixture("Stop", "st:s1:x:1", None);
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(&encode(&fp)); // a valid-looking frame follows

        let err = decode_segment(&bytes).expect_err("must reject incompatible header");
        assert_eq!(
            err,
            HeaderError::UnsupportedFormatVersion {
                found: newer,
                max_supported: spool::FORMAT_VERSION,
            }
        );
    }

    #[test]
    fn decode_segment_decodes_frames_after_a_valid_header() {
        let header = spool::encode_segment_header();
        let fp = fixture("Stop", "st:s1:x:1", None);
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(&encode(&fp));

        let decoded = decode_segment(&bytes).expect("valid header");
        assert_eq!(decoded.frames.len(), 1);
        assert_eq!(decoded.bytes_consumed, bytes.len());
        assert!(matches!(decoded.stop_reason, StopReason::EndOfInput));
    }
}
