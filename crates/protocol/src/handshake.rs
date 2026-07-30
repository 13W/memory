//! Proxy ↔ daemon wire protocol: HELLO/WELCOME/INCOMPATIBLE/SHUTDOWN_REQUEST
//! and the MCP passthrough envelope (spec 02 §4.2, 11 §1/§4) — T15-02.
//!
//! # Framing: NDJSON, one [`Message`] per line
//!
//! UDS/named-pipe is a reliable, ordered byte stream — unlike
//! `local_rag_core::spool`'s durable on-disk LRSP format (magic+version
//! header, length-prefixed CRC32C-checked frames), which needs corruption
//! detection because a crash can tear a *file* mid-write. A live socket's
//! only failure mode is an abrupt close, which any framing scheme observes
//! as an `UnexpectedEof` — there is nothing here that needs a checksum.
//! JSON's own grammar requires every control character (0x00–0x1F, including
//! `\n`) inside a string literal to be escaped, so a syntactically valid JSON
//! document — which is exactly what [`encode_message`] always produces —
//! structurally cannot contain a raw `\n` byte, even when it wraps an opaque,
//! caller-supplied MCP payload (see below). That makes newline-delimited
//! JSON a correct, zero-dependency framing for this transport; a bounded
//! reader (see [`MAX_MESSAGE_BYTES`]) is still required so a malformed or
//! hostile peer that never sends `\n` cannot force unbounded buffering.
//!
//! # Why the MCP payload is `Box<RawValue>`, not `serde_json::Value`
//!
//! The proxy is a *pass-through* for MCP JSON-RPC (spec 11 §1): it must
//! never need to understand tool schemas to relay a call. Parsing into
//! [`serde_json::Value`] and re-serializing would risk losing precision on
//! large `id` numbers, reordering object fields, and paying an allocation
//! per relayed message — a real (if small) violation of "thin". `RawValue`
//! captures the exact byte span of an already-well-formed JSON value and
//! re-emits it byte-for-byte, so the proxy's own (de)serialization never
//! touches MCP semantics at all.
//!
//! # Why [`Message`] is *adjacently* tagged, not internally tagged
//!
//! An internally tagged enum (`#[serde(tag = "type")]`, no `content`) was
//! the first design tried here — and fails to deserialize any variant that
//! contains a `RawValue` field, confirmed by direct reproduction: serde's
//! internally tagged representation deserializes by first buffering the
//! whole object into a generic `Content` value (so it can peek `"type"`
//! before committing to a variant), then re-deserializing each variant's
//! fields *from that buffer* — and `RawValue`'s own `Deserialize` impl only
//! works when driven directly by the original `Deserializer` (that is how
//! it captures the exact source bytes), not from a replayed `Content` tree.
//! Adjacent tagging (`#[serde(tag = "type", content = "data")]`,
//! `{"type": "request", "data": {...}}`) does not need that buffering step —
//! confirmed working by the same direct reproduction — while still keeping
//! one self-describing, `nc`/`socat`-readable wire shape.
//!
//! # What is *not* here
//!
//! MCP tool dispatch, `initialize`/`instructions` semantics, and anything
//! about *which* tools exist are out of scope — this module is the
//! transport + context envelope only. The daemon-side per-connection
//! handler and its `RequestHandler` seam are `local_rag::daemon::handshake`
//! (T15-02); real MCP routing is T15-03's.

use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// This crate's own envelope protocol version (spec 02 §4.2's `proto`).
///
/// Versions *this* transport/handshake/context-envelope layer, independent
/// of the MCP JSON-RPC content it carries opaquely (that content's own
/// version, if any, is Claude Code's concern, never inspected here).
pub const PROTO_VERSION: u16 = 1;

/// The inclusive `proto` range this build of the daemon accepts (spec 02
/// §4.2's `INCOMPATIBLE{min_proto, max_proto, ..}`). A single-version range
/// in v0; daemon-side callers may pass a different range in tests to
/// exercise the incompatible path without a second real binary.
pub const SUPPORTED_PROTO_RANGE: RangeInclusive<u16> = 1..=1;

/// The version of the `{context, mcp}` request/response envelope *shape*
/// itself (spec 11 §4's "MCP passthrough version") — distinct from
/// [`PROTO_VERSION`] (the handshake envelope: HELLO/WELCOME/INCOMPATIBLE/
/// SHUTDOWN_REQUEST) and from `spool_max_format_version` (the durable spool
/// container format, `local_rag_core::spool::FORMAT_VERSION`). Advertised in
/// [`Welcome`] as an informational field in v0 — see this module's own
/// as-built note in spec 11 §4 for why it does not (yet) gate
/// [`Incompatible`].
pub const MCP_PASSTHROUGH_VERSION: u16 = 1;

/// The largest single NDJSON line this protocol accepts, in bytes.
///
/// Not a `[SPEC]` number — the section fixes the framing *concern*, not a
/// size budget for it. Picked and documented as chosen, not derived, the
/// same precedent `local_rag::daemon::probe::LIVENESS_PROBE_TIMEOUT_MS` and
/// `local_rag_search::DEFAULT_L2_READ_WAIT_BUDGET` already set for internal
/// budgets the spec names only the mechanism of.
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// proxy → daemon, the first message on every connection (spec 02 §4.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The `proto` this proxy speaks.
    pub proto: u16,
    /// The proxy binary's own version (`local_rag_core::VERSION`).
    pub proxy_version: String,
    /// Opaque session identifier, supplied by the proxy (spec 02 §3.3);
    /// carried unmodified into every later [`RequestContext`] on this
    /// connection.
    pub session_id: String,
    /// The raw, un-canonicalized working directory the proxy was started
    /// in, if any. Never git-probed or resolved here — spec 02 §3.3's
    /// as-built note: that resolution is the daemon's job, and
    /// `local-rag-store` (and, by the same guardrail, this crate and the
    /// proxy) carries no git dependency.
    pub worktree_root: Option<String>,
    /// The calling harness, e.g. `"claude-code"` (spec 02 §4.2's own
    /// example). A free string, not an enum: forward-compatible with
    /// harnesses this build does not know about.
    pub harness: String,
}

/// daemon → proxy: handshake accepted (spec 02 §4.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The negotiated `proto` — always equal to the accepted [`Hello::proto`]
    /// in v0 (a single supported value; see [`negotiate_proto`]).
    pub proto: u16,
    /// The daemon binary's own version (`local_rag_core::VERSION`) — what a
    /// proxy compares against its own version to detect an upgrade (spec 13
    /// §4).
    pub daemon_version: String,
    /// The store's durable identity (spec 02 §2/§4.1's `store_instance_uuid`)
    /// — the semantic successor of T15-01's provisional
    /// `daemon::probe::Greeting::instance_uuid`.
    pub store_instance_uuid: String,
    /// Reserved for future optional-feature flags; always empty in v0.
    pub capabilities: Vec<String>,
    /// See [`MCP_PASSTHROUGH_VERSION`].
    pub mcp_passthrough_version: u16,
    /// The highest spool `format_version` (`local_rag_core::spool::
    /// FORMAT_VERSION`) this daemon can import (spec 11 §4: "a daemon MUST
    /// be able to import all spool `format_version`s ≤ its own").
    /// Informational in v0 — no hook-binary-side enforcement wires this yet
    /// (T13-03's own as-built note: "the actual proxy↔daemon handshake
    /// wiring... remains a later task").
    pub spool_max_format_version: u16,
    /// The daemon's current serving mode (`DaemonMode::as_str()` — the
    /// semantic successor of `Greeting::mode`), e.g. `"normal"` or
    /// `"migration_only"` (spec 02 §6).
    pub mode: String,
}

/// daemon → proxy: the requested `proto` is outside this build's supported
/// range (spec 02 §4.2). The **only** incompatibility axis T15-02 gates the
/// connection on — see this module's as-built note in spec 11 §4 for why
/// [`Welcome`]'s other two version fields are informational, not gating, in
/// v0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Incompatible {
    /// The lowest `proto` this daemon build accepts.
    pub min_proto: u16,
    /// The highest `proto` this daemon build accepts.
    pub max_proto: u16,
    /// The daemon binary's own version, so a proxy reporting the conflict
    /// can name both sides (spec 02 §4.2: "proxy reports an MCP
    /// initialization error naming both versions").
    pub daemon_version: String,
}

/// proxy → daemon: "you are a stale binary version; finish in-flight jobs,
/// release the store, and exit" (spec 02 §4.2, 13 §4's upgrade flow).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownRequest {
    /// The requesting proxy's own version, for the old daemon's logs.
    pub requested_by_proxy_version: String,
    /// A short, free-text reason (diagnostic only, never parsed) —
    /// currently always `"version_mismatch"`.
    pub reason: String,
}

/// The wire form of spec 02 §3.3's request context: `{session_id,
/// worktree_root?, repo_hint?}`, un-resolved. The daemon (not this crate, not
/// the proxy) turns `worktree_root` into a canonicalized, git-probed
/// `WorktreeRootFacts` and resolves identity via the registry — this struct
/// only carries what the proxy actually has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestContext {
    /// Same value as the connection's [`Hello::session_id`] — routing/
    /// telemetry only (spec 02 §3.3's as-built note), never an identity key.
    pub session_id: String,
    /// Same value as [`Hello::worktree_root`] in v0 (the MCP proxy has no
    /// per-call way to change it — every relayed call on one connection
    /// carries the same context). A future non-MCP caller of this same wire
    /// protocol (e.g. T15-07's CLI) may vary it per call.
    pub worktree_root: Option<String>,
    /// A `repo_id` hint, used only to break a tie between reattach
    /// candidates (spec 02 §3.3). Always `None` from the v0 MCP proxy — no
    /// MCP tool parameter feeds it yet; kept for shape completeness.
    pub repo_hint: Option<String>,
}

/// proxy → daemon: one relayed MCP call, wrapped with its context.
#[derive(Debug, Serialize, Deserialize)]
pub struct RequestEnvelope {
    /// The caller's context (spec 02 §3.3), attached by the proxy to
    /// **every** call (spec 11 §1: "adds the request context envelope...
    /// to every call").
    pub context: RequestContext,
    /// The opaque MCP JSON-RPC request, untouched (see this module's own
    /// doc on why `RawValue`).
    pub mcp: Box<RawValue>,
}

/// daemon → proxy: one MCP response. Carries no context — only requests are
/// contextualized (spec 11 §1's envelope is one-directional).
#[derive(Debug, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    /// The opaque MCP JSON-RPC response, untouched.
    pub mcp: Box<RawValue>,
}

/// Every message this protocol can carry on one connection, in either
/// direction — one NDJSON line each. See this module's own doc for the
/// framing and tagging-representation rationale.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
    Welcome(Welcome),
    Incompatible(Incompatible),
    ShutdownRequest(ShutdownRequest),
    Request(RequestEnvelope),
    Response(ResponseEnvelope),
}

/// Encode `msg` as one NDJSON line: compact JSON followed by a single `\n`.
pub fn encode_message(msg: &Message) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = serde_json::to_vec(msg)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Decode one already-trimmed line (no trailing `\n`) as a [`Message`].
pub fn decode_message(line: &str) -> Result<Message, serde_json::Error> {
    serde_json::from_str(line)
}

/// Whether `requested` falls inside `supported` (spec 02 §4.2's `proto`
/// negotiation). `Ok` carries the negotiated version (always `requested` in
/// v0 — there is exactly one value any range can contain and still accept);
/// `Err` carries `(min, max)` of `supported`, ready to build an
/// [`Incompatible`].
///
/// Pure and total — every `(supported, requested)` pair has exactly one
/// outcome, table-tested at the boundaries.
pub fn negotiate_proto(supported: &RangeInclusive<u16>, requested: u16) -> Result<u16, (u16, u16)> {
    if supported.contains(&requested) {
        Ok(requested)
    } else {
        Err((*supported.start(), *supported.end()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hello() -> Hello {
        Hello {
            proto: PROTO_VERSION,
            proxy_version: "0.0.0".to_string(),
            session_id: "sess-1".to_string(),
            worktree_root: Some("/repo".to_string()),
            harness: "claude-code".to_string(),
        }
    }

    fn sample_welcome() -> Welcome {
        Welcome {
            proto: PROTO_VERSION,
            daemon_version: "0.0.0".to_string(),
            store_instance_uuid: "uuid-a".to_string(),
            capabilities: vec![],
            mcp_passthrough_version: MCP_PASSTHROUGH_VERSION,
            spool_max_format_version: 1,
            mode: "normal".to_string(),
        }
    }

    #[test]
    fn hello_round_trips_through_a_message_line() {
        let msg = Message::Hello(sample_hello());
        let bytes = encode_message(&msg).expect("encode");
        assert_eq!(*bytes.last().unwrap(), b'\n', "one trailing newline");
        let line = std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap();
        assert!(
            !line.contains('\n'),
            "the JSON body itself has no raw newline"
        );
        let decoded = decode_message(line).expect("decode");
        match decoded {
            Message::Hello(h) => assert_eq!(h, sample_hello()),
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn welcome_round_trips() {
        let msg = Message::Welcome(sample_welcome());
        let bytes = encode_message(&msg).unwrap();
        let line = std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap();
        match decode_message(line).unwrap() {
            Message::Welcome(w) => assert_eq!(w, sample_welcome()),
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    #[test]
    fn incompatible_round_trips() {
        let original = Incompatible {
            min_proto: 1,
            max_proto: 1,
            daemon_version: "0.0.0".to_string(),
        };
        let msg = Message::Incompatible(original.clone());
        let bytes = encode_message(&msg).unwrap();
        let line = std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap();
        match decode_message(line).unwrap() {
            Message::Incompatible(i) => assert_eq!(i, original),
            other => panic!("expected Incompatible, got {other:?}"),
        }
    }

    #[test]
    fn shutdown_request_round_trips() {
        let original = ShutdownRequest {
            requested_by_proxy_version: "0.0.1".to_string(),
            reason: "version_mismatch".to_string(),
        };
        let msg = Message::ShutdownRequest(original.clone());
        let bytes = encode_message(&msg).unwrap();
        let line = std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap();
        match decode_message(line).unwrap() {
            Message::ShutdownRequest(s) => assert_eq!(s, original),
            other => panic!("expected ShutdownRequest, got {other:?}"),
        }
    }

    /// The load-bearing case: a `Request`/`Response` carrying an opaque MCP
    /// payload must round-trip byte-for-byte through the tagged `Message`
    /// enum — this is exactly the case that failed under internal tagging
    /// (see this module's own doc comment) and is why adjacent tagging was
    /// chosen instead.
    #[test]
    fn request_with_an_opaque_mcp_payload_round_trips_byte_exact() {
        let mcp_text = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"search_code","arguments":{"query":"foo"}}}"#;
        let context = RequestContext {
            session_id: "sess-1".to_string(),
            worktree_root: Some("/repo".to_string()),
            repo_hint: None,
        };
        let msg = Message::Request(RequestEnvelope {
            context: context.clone(),
            mcp: RawValue::from_string(mcp_text.to_string()).unwrap(),
        });
        let bytes = encode_message(&msg).unwrap();
        let line = std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap();
        match decode_message(line).unwrap() {
            Message::Request(env) => {
                assert_eq!(env.context, context);
                assert_eq!(env.mcp.get(), mcp_text, "the MCP payload is byte-exact");
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn response_with_an_opaque_mcp_payload_round_trips_byte_exact() {
        let mcp_text = r#"{"jsonrpc":"2.0","id":7,"result":{"content":[]}}"#;
        let msg = Message::Response(ResponseEnvelope {
            mcp: RawValue::from_string(mcp_text.to_string()).unwrap(),
        });
        let bytes = encode_message(&msg).unwrap();
        let line = std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap();
        match decode_message(line).unwrap() {
            Message::Response(env) => assert_eq!(env.mcp.get(), mcp_text),
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn negotiate_proto_accepts_inside_the_range_and_returns_the_requested_value() {
        assert_eq!(negotiate_proto(&(1..=1), 1), Ok(1));
        assert_eq!(negotiate_proto(&(1..=3), 2), Ok(2));
        assert_eq!(negotiate_proto(&(1..=3), 1), Ok(1), "inclusive lower bound");
        assert_eq!(negotiate_proto(&(1..=3), 3), Ok(3), "inclusive upper bound");
    }

    #[test]
    fn negotiate_proto_rejects_outside_the_range_naming_both_bounds() {
        assert_eq!(negotiate_proto(&(2..=3), 1), Err((2, 3)));
        assert_eq!(negotiate_proto(&(2..=3), 4), Err((2, 3)));
        assert_eq!(negotiate_proto(&(1..=1), 0), Err((1, 1)));
        assert_eq!(negotiate_proto(&(1..=1), 2), Err((1, 1)));
    }

    #[test]
    fn different_message_types_are_never_confused_on_the_wire() {
        // A Hello line must not decode as any other variant, and vice versa —
        // guards the adjacent-tag discriminator itself, not just round-trips.
        let hello_bytes = encode_message(&Message::Hello(sample_hello())).unwrap();
        let hello_line = std::str::from_utf8(&hello_bytes[..hello_bytes.len() - 1]).unwrap();
        assert!(hello_line.contains("\"type\":\"hello\""));
        let welcome_bytes = encode_message(&Message::Welcome(sample_welcome())).unwrap();
        let welcome_line = std::str::from_utf8(&welcome_bytes[..welcome_bytes.len() - 1]).unwrap();
        assert!(welcome_line.contains("\"type\":\"welcome\""));
    }
}
