//! `local-rag` protocol types (proxy <-> daemon handshake, MCP tool contracts).
//!
//! Three wire contracts live here so far, all shared by callers rather than
//! owned by one subsystem: [`ErrorEnvelope`]'s canonical error/degraded
//! vocabulary (spec 02 §6, T09-03), the `search_code` response and mode
//! vocabulary (spec 09 §5/§7, T12-03), and the proxy ↔ daemon transport
//! itself — HELLO/WELCOME/INCOMPATIBLE/SHUTDOWN_REQUEST and the MCP
//! passthrough envelope (spec 02 §4.2, 11 §1/§4, T15-02) in [`handshake`].
//!
//! JSON **serialization** of the search response is wired here (its shape is
//! `[SPEC]`-fixed by spec 09 §7, and byte-stability is a T12-03 acceptance
//! criterion); the search response stays `Deserialize`-free (nothing reads
//! it back yet). [`handshake`]'s types derive both directions — both the
//! proxy and the daemon send *and* receive every one of them.

pub use local_rag_core::VERSION;

mod error;
pub mod handshake;
mod search;
pub use error::{DegradedMode, ErrorCode, ErrorEnvelope};
pub use handshake::{
    Hello, Incompatible, MAX_MESSAGE_BYTES, MCP_PASSTHROUGH_VERSION, Message, PROTO_VERSION,
    RequestContext, RequestEnvelope, ResponseEnvelope, SUPPORTED_PROTO_RANGE, ShutdownRequest,
    Welcome, decode_message, encode_message, negotiate_proto,
};
pub use search::{
    FileContext, FileOccurrence, GenerationRef, ImportCount, LegRanks, OverviewNode,
    ProjectOverview, SearchMode, SearchResponse, SearchResult, Snippet, Truncation,
};
