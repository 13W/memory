//! Bounded, newline-delimited line I/O shared by the handshake and relay
//! phases (spec 02 §4.2's NDJSON framing) — generic over `AsyncBufRead`/
//! `AsyncWrite` so the same functions serve both the UDS connection
//! (`Message` lines) and stdin/stdout (raw MCP JSON-RPC lines, spec 11 §1).
//!
//! A near-duplicate of `local-rag`'s own `daemon::handshake`'s bounded-line
//! reader rather than a shared crate: `local_rag_protocol` deliberately
//! stays free of `tokio` (its own module doc: memory/index-facing types
//! that crate also hosts must "depend on nothing but core"), and a
//! two-binary-only helper this small does not earn a third crate. Same
//! trade-off already accepted at D-002/D-010.

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use local_rag_protocol::MAX_MESSAGE_BYTES;

/// Read one `\n`-terminated line, bounded to [`MAX_MESSAGE_BYTES`]. Unlike a
/// bare `AsyncBufReadExt::read_until` (which buffers without limit until it
/// finds the delimiter), this checks the accumulated length after **every**
/// underlying read, so a peer that never sends `\n` cannot force unbounded
/// growth.
///
/// `Ok(None)` is a clean EOF; a line whose content is not valid UTF-8, or
/// that exceeds the bound, is an `Err`.
pub async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<String>> {
    let mut out = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(None); // clean EOF
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
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
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

fn too_long() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "line exceeds MAX_MESSAGE_BYTES",
    )
}

/// Write `line` (no trailing `\n` expected) followed by `\n`, flushing.
pub async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

/// Encode and write one [`local_rag_protocol::Message`], flushing.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &local_rag_protocol::Message,
) -> std::io::Result<()> {
    let bytes = local_rag_protocol::encode_message(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn reads_lines_one_at_a_time_then_reports_clean_eof() {
        let (mut client, server) = tokio::io::duplex(64);
        client.write_all(b"hello\nworld\n").await.unwrap();
        let mut reader = BufReader::new(server);
        assert_eq!(
            read_bounded_line(&mut reader).await.unwrap(),
            Some("hello".to_string())
        );
        assert_eq!(
            read_bounded_line(&mut reader).await.unwrap(),
            Some("world".to_string())
        );
        drop(client); // close the write half: the reader must now observe EOF
        assert_eq!(read_bounded_line(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_oversized_line_without_a_newline_is_rejected_not_buffered_forever() {
        let (mut client, server) = tokio::io::duplex(MAX_MESSAGE_BYTES + 16);
        let oversized = vec![b'a'; MAX_MESSAGE_BYTES + 1];
        client.write_all(&oversized).await.unwrap();
        let mut reader = BufReader::new(server);
        assert!(read_bounded_line(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn write_line_appends_exactly_one_newline() {
        let (mut client, mut server) = tokio::io::duplex(64);
        write_line(&mut client, "hello").await.unwrap();
        let mut buf = [0u8; 64];
        let n = tokio::io::AsyncReadExt::read(&mut server, &mut buf)
            .await
            .unwrap();
        assert_eq!(&buf[..n], b"hello\n");
    }
}
