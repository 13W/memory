//! `local-rag` protocol types (proxy <-> daemon handshake, MCP tool contracts).
//!
//! Beyond the workspace version re-export, the only protocol logic so far is
//! [`ErrorEnvelope`]'s canonical error/degraded vocabulary (spec 02 §6, T09-03)
//! — the wire shape shared by every daemon subsystem. JSON (de)serialization
//! and the rest of the proxy/daemon handshake and MCP tool contracts remain
//! group 15.

pub use local_rag_core::VERSION;

mod error;
pub use error::{DegradedMode, ErrorCode, ErrorEnvelope};
