//! `local-rag` protocol types (proxy <-> daemon handshake, MCP tool contracts).
//!
//! Two wire contracts live here so far, both shared by callers rather than
//! owned by one subsystem: [`ErrorEnvelope`]'s canonical error/degraded
//! vocabulary (spec 02 §6, T09-03), and the `search_code` response and mode
//! vocabulary (spec 09 §5/§7, T12-03).
//!
//! JSON **serialization** of the search response is wired here (its shape is
//! `[SPEC]`-fixed by spec 09 §7, and byte-stability is a T12-03 acceptance
//! criterion); deserialization, the proxy/daemon handshake and the MCP tool
//! framing around these types remain group 15.

pub use local_rag_core::VERSION;

mod error;
mod search;
pub use error::{DegradedMode, ErrorCode, ErrorEnvelope};
pub use search::{GenerationRef, LegRanks, SearchMode, SearchResponse, SearchResult};
