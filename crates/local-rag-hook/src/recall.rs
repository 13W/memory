//! Read-only recall RPC + `additionalContext` injection for `SessionStart`/
//! `UserPromptSubmit` (spec 11 §3.2 `[SPEC]`, §5) — T15-06.
//!
//! This is the missing second half of the hook pipeline: `main.rs`'s own
//! module doc already named this as deliberately deferred, so a later task
//! could add it "without restructuring" the spool-append path. It runs
//! *after* that append has already durably succeeded — never before, and
//! never at all if the append itself failed — and it can never start the
//! daemon (no spawn from hooks, `[SPEC]`).
//!
//! # Sync, not async
//!
//! `local-rag-hook` carries no production async runtime, and does not need
//! one here: this is a single one-shot round trip (connect, HELLO/WELCOME,
//! one `tools/call`, one response), not a long-lived multiplexed session.
//! [`local_rag_protocol`] is deliberately tokio-free by its own design
//! specifically so it composes with either an async caller
//! (`local-rag-proxy`) or a sync one (here) — this module talks it over a
//! blocking [`std::os::unix::net::UnixStream`] with
//! `set_read_timeout`/`set_write_timeout` bounding every I/O call against a
//! single [`RECALL_BUDGET`] deadline recomputed before each call, so the
//! *whole* exchange — not each call individually — stays under budget.
//! [`read_bounded_line`]/[`write_message`] are a sync port of
//! `local-rag-proxy/src/transport.rs`'s identical `fill_buf`/`consume`
//! algorithm — a third copy of the same ~40-line fragment that file's own
//! doc already accepts duplicating rather than sharing a crate (D-002/D-010:
//! `local_rag_protocol` must stay free of any I/O runtime).
//!
//! `UnixStream::connect` itself is not bounded by [`RECALL_BUDGET`]: a local
//! UDS connect either succeeds near-instantly or fails immediately
//! (`ENOENT`/`ECONNREFUSED`) when nothing is listening — std gives no way to
//! attach a timeout to `connect()` itself, and the one residual gap (a full
//! accept backlog blocking `connect()` briefly) is accepted, not engineered
//! around, since the daemon's own accept loop spawns per-connection work
//! immediately and never blocks on it.
//!
//! # Fail-open by construction
//!
//! [`recall_and_print`] never returns an error and cannot propagate a panic
//! from ordinary failure — every fallible step collapses to `Option` via
//! `.ok()?`, and the final stdout write uses [`std::io::Write::write_all`]
//! with its `Result` discarded (never `println!`, which panics on a broken
//! pipe) rather than relying solely on `main.rs`'s outer `catch_unwind` as a
//! backstop. Any failure anywhere in the chain — unreachable daemon,
//! timeout, a malformed/unexpected response, `MigrationOnly` degradation —
//! is observably identical: nothing is printed, the caller proceeds to its
//! own unconditional `exit 0`.
//!
//! # Query: termless for `SessionStart`, the prompt for `UserPromptSubmit`
//!
//! Spec 08 §6 ties termless recall specifically to `SessionStart` ("before
//! any prompt exists") — `UserPromptSubmit` is the one hook event where a
//! prompt *does* exist (`UserPromptSubmitPayload.prompt` is a hard-required
//! field), and `local_rag_memory::recall::pipeline`'s lexical/dense legs both
//! use `query` to rank toward relevance; termless recall falls back to pure
//! recency. Passing the real prompt on `UserPromptSubmit` is what makes the
//! injected context relevant to what the user is about to ask, distinct from
//! `SessionStart`'s orientation-only role.
//!
//! # Double JSON nesting in the response
//!
//! `tools/call`'s `result.content[0].text` is itself a **string** containing
//! the tool's own JSON (`local-rag`'s `content::ok`/`content::err`,
//! `crates/local-rag/src/daemon/mcp/content.rs`) — [`extract_additional_context`]
//! parses the outer JSON-RPC envelope, then re-parses that string as a
//! second, independent JSON document. A `MigrationOnly`-degraded response
//! (`isError: true`, an `ErrorEnvelope` with no `additional_context` field)
//! and a JSON-RPC-level error (no `result` key at all) both collapse to
//! `None` through this same path with no dedicated branch for either.

use std::io::{self, BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::value::RawValue;

use local_rag_core::paths::StoreLayout;
use local_rag_protocol::{
    Hello, MAX_MESSAGE_BYTES, Message, PROTO_VERSION, RequestContext, RequestEnvelope,
    decode_message, encode_message,
};

use crate::event::{EventPayload, ParsedEvent, event_type_name};

/// The recall RPC's hard budget (spec 11 §3.2 `[SPEC]`) — the *whole*
/// connect-through-response exchange, not per I/O call.
pub const RECALL_BUDGET: Duration = Duration::from_millis(300);

/// Best-effort read-only recall + stdout print. Never returns an error,
/// never panics on an ordinary failure (every fallible step collapses to
/// `Option`) — the caller has nothing to check and nothing to propagate.
#[cfg(unix)]
pub fn recall_and_print(layout: &StoreLayout, event: &ParsedEvent) {
    let Some(text) = try_recall(layout, event) else {
        return;
    };
    if text.is_empty() {
        // Spec 11 §5 `[FIXED]`: empty recall ⇒ no output at all, not an
        // empty-but-present `additionalContext`.
        return;
    }
    print_hook_output(event, &text);
}

/// Windows has no local transport to the daemon yet — named-pipe IPC across
/// this crate/`local-rag`/`local-rag-proxy` is not implemented (D-033,
/// tracked separately from this crate's own scope). This degrades exactly
/// like every other "daemon unreachable" case on the Unix path already does:
/// no output, no error, the caller proceeds unconditionally.
#[cfg(not(unix))]
pub fn recall_and_print(_layout: &StoreLayout, _event: &ParsedEvent) {}

#[cfg(unix)]
fn try_recall(layout: &StoreLayout, event: &ParsedEvent) -> Option<String> {
    let deadline = Instant::now() + RECALL_BUDGET;
    let stream = UnixStream::connect(layout.socket_path()).ok()?;
    let (hello, request) = build_request(event);

    set_timeouts(&stream, deadline)?;
    write_message(&mut &stream, &hello).ok()?;

    let mut reader = BufReader::new(&stream);
    set_timeouts(&stream, deadline)?;
    let line = read_bounded_line(&mut reader).ok()??;
    let Message::Welcome(_) = decode_message(&line).ok()? else {
        return None;
    };

    set_timeouts(&stream, deadline)?;
    write_message(&mut &stream, &request).ok()?;

    set_timeouts(&stream, deadline)?;
    let line = read_bounded_line(&mut reader).ok()??;
    let Message::Response(envelope) = decode_message(&line).ok()? else {
        return None;
    };

    extract_additional_context(&envelope.mcp)
}

/// Set both read and write timeouts to whatever remains of `deadline`; `None`
/// if the deadline has already passed (a zero `Duration` is rejected by
/// `set_read_timeout`/`set_write_timeout` themselves, not treated as
/// "instant" — this bails out before ever calling either).
#[cfg(unix)]
fn set_timeouts(stream: &UnixStream, deadline: Instant) -> Option<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }
    stream.set_read_timeout(Some(remaining)).ok()?;
    stream.set_write_timeout(Some(remaining)).ok()?;
    Some(())
}

/// The `(hook_event_name, query)` pair for a checkpoint-eligible event — see
/// this module's own doc for the termless-vs-prompt rationale. The catch-all
/// arm is purely defensive: `main.rs`'s own gate never calls this for any
/// other variant, but a pure function with no unreachable branch is safer to
/// keep testable in isolation than one that could panic if ever miscalled.
fn recall_query(kind: &EventPayload) -> (&'static str, Option<&str>) {
    match kind {
        EventPayload::SessionStart(_) => ("SessionStart", None),
        EventPayload::UserPromptSubmit(p) => ("UserPromptSubmit", Some(p.prompt.as_str())),
        other => (event_type_name(other), None),
    }
}

fn build_request(event: &ParsedEvent) -> (Message, Message) {
    let (_, query) = recall_query(&event.kind);

    let hello = Message::Hello(Hello {
        proto: PROTO_VERSION,
        proxy_version: local_rag_core::VERSION.to_string(),
        session_id: event.session_id.clone(),
        worktree_root: event.cwd.clone(),
        // Distinct from `local-rag-proxy`'s own "claude-code" (spec 11 §7,
        // T18-08) — the daemon's telemetry (`admin/tail_calls`/
        // `admin/tool_stats`) derives `source` straight from this free
        // string, so the two connection kinds must not collide. Still
        // unambiguously "Claude Code" (`01-overview.md`'s `[FIXED]`
        // "Claude Code is the only supported harness" is about the
        // external coding agent, not this internal component label).
        harness: "claude-code-hook".to_string(),
    });

    let mut arguments = serde_json::Map::new();
    if let Some(q) = query {
        arguments.insert(
            "query".to_string(),
            serde_json::Value::String(q.to_string()),
        );
    }
    let mcp_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "recall",
            "arguments": arguments,
        },
    });
    // `mcp_body.to_string()` is always well-formed JSON (it was just built
    // from a `serde_json::Value`) — `RawValue::from_string` cannot fail on
    // it, the same "always serializes" idiom `daemon::mcp::content`'s own
    // `ok`/`err` builders already rely on with `.expect(...)`.
    let mcp = RawValue::from_string(mcp_body.to_string())
        .expect("a freshly-serialized serde_json::Value is always well-formed JSON");

    let request = Message::Request(RequestEnvelope {
        context: RequestContext {
            session_id: event.session_id.clone(),
            worktree_root: event.cwd.clone(),
            repo_hint: None,
        },
        mcp,
    });

    (hello, request)
}

#[derive(Deserialize)]
struct JsonRpcSuccessShape {
    #[serde(default)]
    result: Option<ToolResultShape>,
}

#[derive(Deserialize)]
struct ToolResultShape {
    content: Vec<ContentTextShape>,
}

#[derive(Deserialize)]
struct ContentTextShape {
    text: String,
}

#[derive(Deserialize)]
struct RecallAdditionalContext {
    additional_context: String,
}

/// Parse the double-nested `recall` response — see this module's own doc.
/// Any shape mismatch (a JSON-RPC error with no `result`, a `MigrationOnly`
/// `ErrorEnvelope` with no `additional_context`, garbage) collapses to
/// `None` through ordinary `Option`/`Result` propagation, no dedicated
/// error-shape branch.
fn extract_additional_context(mcp: &RawValue) -> Option<String> {
    let envelope: JsonRpcSuccessShape = serde_json::from_str(mcp.get()).ok()?;
    let result = envelope.result?;
    let first = result.content.first()?;
    let parsed: RecallAdditionalContext = serde_json::from_str(&first.text).ok()?;
    Some(parsed.additional_context)
}

/// Print Claude Code's own hook-output JSON — `write_all` with the result
/// discarded, never `println!` (which panics on a broken/closed stdout
/// pipe), so this call itself cannot panic either.
fn print_hook_output(event: &ParsedEvent, additional_context: &str) {
    let hook_event_name = event_type_name(&event.kind);
    let payload = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": hook_event_name,
            "additionalContext": additional_context,
        },
    });
    if let Ok(mut line) = serde_json::to_string(&payload) {
        line.push('\n');
        let _ = io::stdout().write_all(line.as_bytes());
    }
}

/// Read one `\n`-terminated line, bounded to [`MAX_MESSAGE_BYTES`] — a
/// blocking, `std::io::BufRead`-based port of `local-rag-proxy`'s own
/// `read_bounded_line` (see this module's own doc). `Ok(None)` is a clean
/// EOF; a line whose content is not valid UTF-8, that exceeds the bound, or
/// whose read times out is an `Err` — a timeout mid-read never surfaces the
/// bytes already buffered, they are simply dropped along with the `Err`.
fn read_bounded_line<R: BufRead>(reader: &mut R) -> io::Result<Option<String>> {
    let mut out = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(None);
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                out.extend_from_slice(&available[..pos]);
                reader.consume(pos + 1);
                if out.len() > MAX_MESSAGE_BYTES {
                    return Err(too_long());
                }
                return String::from_utf8(out)
                    .map(Some)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
            }
            None => {
                out.extend_from_slice(available);
                let consumed = available.len();
                reader.consume(consumed);
                if out.len() > MAX_MESSAGE_BYTES {
                    return Err(too_long());
                }
            }
        }
    }
}

fn too_long() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "line exceeds MAX_MESSAGE_BYTES")
}

/// Encode and write one [`Message`], flushing — sync port of
/// `local-rag-proxy`'s own `write_message`.
fn write_message<W: Write>(writer: &mut W, msg: &Message) -> io::Result<()> {
    let bytes = encode_message(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    writer.write_all(&bytes)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{SessionStartPayload, UserPromptSubmitPayload};

    fn session_start_event() -> ParsedEvent {
        ParsedEvent {
            session_id: "sess-1".to_string(),
            cwd: Some("/repo".to_string()),
            kind: EventPayload::SessionStart(SessionStartPayload { source: None }),
        }
    }

    fn user_prompt_event(prompt: &str) -> ParsedEvent {
        ParsedEvent {
            session_id: "sess-1".to_string(),
            cwd: Some("/repo".to_string()),
            kind: EventPayload::UserPromptSubmit(UserPromptSubmitPayload {
                prompt: prompt.to_string(),
            }),
        }
    }

    #[test]
    fn recall_query_is_termless_for_session_start() {
        let event = session_start_event();
        assert_eq!(recall_query(&event.kind), ("SessionStart", None));
    }

    #[test]
    fn recall_query_uses_the_prompt_for_user_prompt_submit() {
        let event = user_prompt_event("what auth scheme do we use?");
        assert_eq!(
            recall_query(&event.kind),
            ("UserPromptSubmit", Some("what auth scheme do we use?"))
        );
    }

    #[test]
    fn build_request_omits_query_for_session_start() {
        let event = session_start_event();
        let (hello, request) = build_request(&event);

        let Message::Hello(hello) = hello else {
            panic!("expected Hello")
        };
        assert_eq!(hello.session_id, "sess-1");
        assert_eq!(hello.worktree_root.as_deref(), Some("/repo"));
        assert_eq!(hello.harness, "claude-code-hook");

        let Message::Request(env) = request else {
            panic!("expected Request")
        };
        let body: serde_json::Value = serde_json::from_str(env.mcp.get()).unwrap();
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], "recall");
        assert!(
            body["params"]["arguments"].get("query").is_none(),
            "SessionStart must not send a query: {body}"
        );
    }

    #[test]
    fn build_request_sends_the_prompt_as_query_for_user_prompt_submit() {
        let event = user_prompt_event("hello there");
        let (_, request) = build_request(&event);
        let Message::Request(env) = request else {
            panic!("expected Request")
        };
        let body: serde_json::Value = serde_json::from_str(env.mcp.get()).unwrap();
        assert_eq!(body["params"]["arguments"]["query"], "hello there");
    }

    fn raw(json: &str) -> Box<RawValue> {
        RawValue::from_string(json.to_string()).unwrap()
    }

    #[test]
    fn extract_additional_context_reads_a_non_empty_context() {
        let mcp = raw(
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"additional_context\":\"hello\",\"scope\":\"global\"}"}],"isError":false}}"#,
        );
        assert_eq!(extract_additional_context(&mcp), Some("hello".to_string()));
    }

    #[test]
    fn extract_additional_context_reads_an_empty_context_as_some_empty() {
        let mcp = raw(
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"additional_context\":\"\"}"}],"isError":false}}"#,
        );
        assert_eq!(extract_additional_context(&mcp), Some(String::new()));
    }

    #[test]
    fn extract_additional_context_is_none_for_a_migration_only_error_shape() {
        // isError:true, the text is an ErrorEnvelope — no additional_context field.
        let mcp = raw(
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"code\":\"INCOMPATIBLE_STORE\",\"message\":\"migration only\",\"retryable\":false}"}],"isError":true}}"#,
        );
        assert_eq!(extract_additional_context(&mcp), None);
    }

    #[test]
    fn extract_additional_context_is_none_for_a_json_rpc_level_error() {
        let mcp = raw(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad"}}"#);
        assert_eq!(extract_additional_context(&mcp), None);
    }

    #[test]
    fn extract_additional_context_is_none_for_garbage() {
        // `RawValue` only ever wraps already-well-formed JSON (structurally
        // guaranteed on the wire), so these are shapes that parse fine as
        // JSON but do not match what `extract_additional_context` expects —
        // not literally malformed JSON, which can never reach this function.
        for garbage in [
            "42",
            r#""just a string""#,
            "{}",
            r#"{"result":{"content":[]}}"#,
        ] {
            assert_eq!(extract_additional_context(&raw(garbage)), None, "{garbage}");
        }
    }

    #[test]
    fn read_bounded_line_reads_one_line_at_a_time_then_reports_eof() {
        let data = b"hello\nworld\n".to_vec();
        let mut reader = io::Cursor::new(data);
        assert_eq!(
            read_bounded_line(&mut reader).unwrap(),
            Some("hello".to_string())
        );
        assert_eq!(
            read_bounded_line(&mut reader).unwrap(),
            Some("world".to_string())
        );
        assert_eq!(read_bounded_line(&mut reader).unwrap(), None);
    }

    #[test]
    fn read_bounded_line_rejects_an_oversized_line_without_a_newline() {
        let data = vec![b'a'; MAX_MESSAGE_BYTES + 1];
        let mut reader = io::Cursor::new(data);
        assert!(read_bounded_line(&mut reader).is_err());
    }

    #[test]
    fn write_message_appends_exactly_the_encoded_bytes() {
        let mut buf = Vec::new();
        let hello = Message::Hello(Hello {
            proto: PROTO_VERSION,
            proxy_version: "0.0.0".to_string(),
            session_id: "s".to_string(),
            worktree_root: None,
            harness: "claude-code-hook".to_string(),
        });
        write_message(&mut buf, &hello).unwrap();
        assert_eq!(buf, encode_message(&hello).unwrap());
    }
}
