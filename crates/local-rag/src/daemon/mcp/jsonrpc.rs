//! JSON-RPC 2.0 envelope types (<https://www.jsonrpc.org/specification>) —
//! the *inner* MCP framing this daemon must parse and answer, distinct from
//! and orthogonal to `local_rag_protocol::handshake`'s own `RequestEnvelope`/
//! `ResponseEnvelope` (the outer transport frame HELLO/WELCOME/Request/
//! Response ride in over the UDS connection) — T15-03.

use serde::Serialize;
use serde_json::{Map, Value};

/// Standard JSON-RPC 2.0 error codes this dispatcher can produce.
///
/// `PARSE_ERROR` is defined for completeness (it is part of the spec's own
/// vocabulary) but unreachable from this daemon: `mcp: Box<RawValue>` is
/// already syntactically valid JSON by construction of `local_rag_protocol::
/// handshake::Message`'s own deserialization, and `local-rag-proxy`'s
/// `relay.rs` rejects malformed JSON on stdin before it is ever wrapped in a
/// `RequestEnvelope`. Nothing on this daemon's own path can produce
/// unparseable JSON — no branch below returns this code, and none simulates
/// the case to reach it artificially. Kept defined (not deleted) so the
/// vocabulary this module implements is visibly complete against the
/// JSON-RPC 2.0 spec, rather than silently missing a code a future reader
/// might otherwise assume was overlooked.
#[allow(dead_code)]
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

/// One incoming JSON-RPC 2.0 request or notification, already confirmed to
/// be a JSON object.
///
/// `id: None` means the `"id"` key was **absent** — a notification (JSON-RPC
/// 2.0 §4.1: no response, ever, not even an error). `id: Some(Value::Null)`
/// means the key was present with a `null` value — a valid request that
/// still gets answered. This distinction is why [`Request::parse`] is
/// hand-written rather than `#[derive(Deserialize)]`: a derived
/// `Option<Value>` field would collapse both cases to `None`, since serde's
/// default `Option<T>` deserialization treats a JSON `null` *value* the same
/// as an *absent* key — exactly the distinction this dispatcher cannot
/// afford to lose.
#[derive(Debug)]
pub struct Request {
    pub jsonrpc: Option<String>,
    pub id: Option<Value>,
    pub method: Option<String>,
    pub params: Option<Value>,
}

impl Request {
    /// Parse `map` into a [`Request`]. `Err` names a human-readable reason
    /// — a present field with the wrong JSON type (e.g. `"method": 5`) is
    /// the only failure mode; every field's absence is tolerated here and
    /// validated by the caller (`dispatch`), which can give a more specific
    /// `-32600 Invalid Request` message than a generic parse failure would.
    pub fn parse(mut map: Map<String, Value>) -> Result<Request, String> {
        let jsonrpc = match map.remove("jsonrpc") {
            None => None,
            Some(Value::String(s)) => Some(s),
            Some(_) => return Err("\"jsonrpc\" must be a string".to_string()),
        };
        // `Map::remove` returns `None` exactly when the key is absent, and
        // `Some(Value::Null)` when it is present with a `null` value — the
        // distinction this whole type exists to preserve.
        let id = map.remove("id");
        let method = match map.remove("method") {
            None => None,
            Some(Value::String(s)) => Some(s),
            Some(_) => return Err("\"method\" must be a string".to_string()),
        };
        let params = map.remove("params");
        Ok(Request {
            jsonrpc,
            id,
            method,
            params,
        })
    }
}

/// A successful JSON-RPC 2.0 response.
#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    pub result: Value,
}

impl Response {
    pub fn new(id: Value, result: Value) -> Self {
        Response {
            jsonrpc: "2.0",
            id,
            result,
        }
    }
}

/// A JSON-RPC 2.0 error response.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    pub error: RpcError,
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl ErrorResponse {
    pub fn new(id: Value, code: i64, message: impl Into<String>) -> Self {
        ErrorResponse {
            jsonrpc: "2.0",
            id,
            error: RpcError {
                code,
                message: message.into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(json: &str) -> Map<String, Value> {
        match serde_json::from_str(json).unwrap() {
            Value::Object(map) => map,
            other => panic!("expected an object, got {other:?}"),
        }
    }

    #[test]
    fn an_absent_id_parses_as_none() {
        let req = Request::parse(obj(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        ))
        .unwrap();
        assert_eq!(req.id, None);
    }

    #[test]
    fn a_present_null_id_parses_as_some_null() {
        let req = Request::parse(obj(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#)).unwrap();
        assert_eq!(req.id, Some(Value::Null));
    }

    #[test]
    fn a_string_and_a_number_id_round_trip() {
        let req = Request::parse(obj(r#"{"id":"abc","method":"ping"}"#)).unwrap();
        assert_eq!(req.id, Some(Value::String("abc".to_string())));

        let req = Request::parse(obj(r#"{"id":42,"method":"ping"}"#)).unwrap();
        assert_eq!(req.id, Some(Value::Number(42.into())));
    }

    #[test]
    fn a_non_string_method_is_rejected() {
        assert!(Request::parse(obj(r#"{"id":1,"method":5}"#)).is_err());
    }

    #[test]
    fn a_non_string_jsonrpc_is_rejected() {
        assert!(Request::parse(obj(r#"{"jsonrpc":2,"id":1,"method":"ping"}"#)).is_err());
    }

    #[test]
    fn success_response_serializes_in_declaration_order() {
        let body = serde_json::to_string(&Response::new(
            Value::Number(1.into()),
            serde_json::json!({"ok": true}),
        ))
        .unwrap();
        assert_eq!(body, r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#);
    }

    #[test]
    fn error_response_serializes_in_declaration_order() {
        let body =
            serde_json::to_string(&ErrorResponse::new(Value::Null, METHOD_NOT_FOUND, "nope"))
                .unwrap();
        assert_eq!(
            body,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32601,"message":"nope"}}"#
        );
    }
}
