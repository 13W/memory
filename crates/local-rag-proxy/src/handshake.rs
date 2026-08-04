//! HELLO/WELCOME/INCOMPATIBLE and the upgrade-request state machine (spec
//! 02 §4.2, 13 §4) — the proxy side of `local-rag`'s own `daemon::handshake`.

use std::path::Path;
use std::time::Duration;

use local_rag_core::paths::Env;
use local_rag_protocol::{Hello, Message, PROTO_VERSION, ShutdownRequest, Welcome, decode_message};
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::connect::{DEFAULT_BACKOFF, connect_or_spawn};
use crate::error::ProxyError;
use crate::transport::{read_bounded_line, write_message};

/// How long to wait for the *old* daemon to close the connection after this
/// proxy sends `SHUTDOWN_REQUEST` (spec 13 §4's upgrade flow) — this card's
/// own number ("30s upgrade timeout"), picked and documented as chosen, not
/// derived, the same precedent `LIVENESS_PROBE_TIMEOUT_MS` sets.
pub const UPGRADE_CLOSE_TIMEOUT_MS: u64 = 30_000;

/// A bound on upgrade-retry rounds inside [`establish_session`] — a
/// defensive cap against a persistently flapping/misconfigured daemon that
/// never converges on a matching version, not a number spec 13 §4 names.
pub const MAX_UPGRADE_ROUNDS: u32 = 2;

/// The env var `$LOCAL_RAG_SESSION_ID` (spec 02 §3.3's routing/telemetry
/// `session_id`), consulted before falling back to a fresh id.
const SESSION_ID_VAR: &str = "LOCAL_RAG_SESSION_ID";

/// This proxy's own identity for HELLO (spec 02 §3.3, 11 §1) — and the
/// fixed [`RequestContext`] every relayed call on this connection carries.
#[derive(Debug, Clone)]
pub struct SessionParams {
    pub session_id: String,
    pub worktree_root: Option<String>,
}

/// `session_id` from `$LOCAL_RAG_SESSION_ID` if set (and non-empty), else
/// `uuid_source()`; `worktree_root` from `current_dir()`.
///
/// The real npm/plugin launch contract that would set `$LOCAL_RAG_SESSION_ID`
/// does not exist yet in this repository (packaging is a later group) — this
/// is a deliberately provisional, non-blocking default; a later task
/// localizes the change to this one function if the contract needs
/// something else. `env`/`uuid_source` are injected seams (mirroring
/// `local_rag_core::paths::Env`'s own precedent) so this is unit-testable
/// without mutating the real process environment.
pub fn resolve_session_params(
    env: &impl Env,
    uuid_source: impl FnOnce() -> String,
) -> SessionParams {
    let session_id = env
        .var(SESSION_ID_VAR)
        .and_then(|v| v.into_string().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(uuid_source);
    let worktree_root = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    SessionParams {
        session_id,
        worktree_root,
    }
}

/// Send HELLO and read the daemon's WELCOME or INCOMPATIBLE reply.
pub async fn do_handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    params: &SessionParams,
) -> Result<Welcome, ProxyError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let hello = Message::Hello(Hello {
        proto: PROTO_VERSION,
        proxy_version: local_rag_core::VERSION.to_string(),
        session_id: params.session_id.clone(),
        worktree_root: params.worktree_root.clone(),
        harness: "claude-code".to_string(),
    });
    write_message(writer, &hello)
        .await
        .map_err(ProxyError::Transport)?;

    let line = read_bounded_line(reader)
        .await
        .map_err(ProxyError::Transport)?
        .ok_or(ProxyError::HandshakeClosed)?;
    match decode_message(&line).map_err(ProxyError::Protocol)? {
        Message::Welcome(welcome) => Ok(welcome),
        Message::Incompatible(i) => Err(ProxyError::Incompatible {
            min_proto: i.min_proto,
            max_proto: i.max_proto,
            daemon_version: i.daemon_version,
        }),
        _ => Err(ProxyError::UnexpectedMessage),
    }
}

/// Wait for the connection to close (EOF), bounded by `timeout`. Any stray
/// lines received while waiting are read and discarded — this proxy has
/// already sent `SHUTDOWN_REQUEST` and has nothing further to say; it is
/// only waiting for the old daemon's own drain to finish (spec 02 §4.3, 13
/// §4).
async fn wait_for_close<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    timeout: Duration,
) -> Result<(), ProxyError> {
    let wait_for_eof = async {
        loop {
            match read_bounded_line(reader).await {
                Ok(None) => return,
                Ok(Some(_)) => continue,
                Err(_) => return,
            }
        }
    };
    tokio::time::timeout(timeout, wait_for_eof)
        .await
        .map_err(|_| ProxyError::UpgradeTimedOut)
}

/// T17-04: this proxy's sibling `local-rag-hook` (same release, same compiled
/// `local_rag_core::spool::FORMAT_VERSION`) could write a spool segment the
/// connected daemon's advertised `Welcome.spool_max_format_version` cannot
/// yet import (spec 11 §4 `[FIXED concern]`: "a newer hook binary writing a
/// newer format than the running daemon supports is a reportable
/// incompatibility, not silent loss" — the proxy-side half of this the
/// T15-02 as-built note named as remaining later work).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpoolFormatWarning {
    /// What this proxy's sibling hook would write (`FORMAT_VERSION`).
    pub compiled_format_version: u16,
    /// What the connected daemon advertised it can import.
    pub daemon_max_format_version: u16,
}

impl std::fmt::Display for SpoolFormatWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the connected daemon supports spool format versions up to {}, but this \
             release's local-rag-hook writes format version {} — spool segments it \
             captures may not import until the daemon is upgraded",
            self.daemon_max_format_version, self.compiled_format_version
        )
    }
}

/// `None` unless this proxy's sibling hook could write a spool format the
/// connected daemon cannot yet import. Direction matters:
/// `daemon_max_format_version < compiled_format_version` means an
/// already-upgraded hook could produce bytes an as-yet-un-upgraded daemon
/// will stall on; `daemon_max_format_version >= compiled_format_version` is
/// always fine (the daemon is at least as capable as this release's hook).
pub fn check_spool_format_compatibility(
    compiled_format_version: u16,
    daemon_max_format_version: u16,
) -> Option<SpoolFormatWarning> {
    (compiled_format_version > daemon_max_format_version).then_some(SpoolFormatWarning {
        compiled_format_version,
        daemon_max_format_version,
    })
}

/// A live, version-matched session: the split UDS connection plus the
/// WELCOME the daemon answered with.
pub struct EstablishedSession {
    pub reader: tokio::io::BufReader<OwnedReadHalf>,
    pub writer: OwnedWriteHalf,
    pub welcome: Welcome,
}

/// Connect (spawning a daemon if none is reachable) and complete the
/// handshake, retrying through the upgrade flow (spec 13 §4) whenever the
/// answering daemon's version does not match this proxy's own: send
/// `SHUTDOWN_REQUEST`, wait for it to close, then connect again (a fresh
/// `connect_or_spawn` — with no daemon left holding the store, this spawns
/// the current, presumably now-matching, binary).
pub async fn establish_session(
    socket_path: &Path,
    daemon_binary: &Path,
    params: &SessionParams,
) -> Result<EstablishedSession, ProxyError> {
    for round in 1..=MAX_UPGRADE_ROUNDS {
        let stream = connect_or_spawn(socket_path, daemon_binary, DEFAULT_BACKOFF).await?;
        let (read_half, write_half) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        let mut writer = write_half;
        let welcome = do_handshake(&mut reader, &mut writer, params).await?;

        if welcome.daemon_version == local_rag_core::VERSION {
            return Ok(EstablishedSession {
                reader,
                writer,
                welcome,
            });
        }
        if round == MAX_UPGRADE_ROUNDS {
            break;
        }
        let shutdown_request = Message::ShutdownRequest(ShutdownRequest {
            requested_by_proxy_version: local_rag_core::VERSION.to_string(),
            reason: "version_mismatch".to_string(),
        });
        write_message(&mut writer, &shutdown_request)
            .await
            .map_err(ProxyError::Transport)?;
        wait_for_close(&mut reader, Duration::from_millis(UPGRADE_CLOSE_TIMEOUT_MS)).await?;
    }
    Err(ProxyError::UpgradeLoopExceeded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_protocol::{Incompatible, MCP_PASSTHROUGH_VERSION};
    use tokio::io::{AsyncWriteExt, BufReader, ReadHalf, WriteHalf, split};

    struct MockEnv(Option<String>);

    impl Env for MockEnv {
        fn var(&self, key: &str) -> Option<std::ffi::OsString> {
            if key == SESSION_ID_VAR {
                self.0.clone().map(std::ffi::OsString::from)
            } else {
                None
            }
        }
        fn home_dir(&self) -> Option<std::path::PathBuf> {
            None
        }
    }

    #[test]
    fn session_id_prefers_the_env_var_over_the_uuid_source() {
        let env = MockEnv(Some("from-env".to_string()));
        let params = resolve_session_params(&env, || "from-uuid".to_string());
        assert_eq!(params.session_id, "from-env");
    }

    #[test]
    fn an_empty_env_var_falls_back_to_the_uuid_source() {
        let env = MockEnv(Some(String::new()));
        let params = resolve_session_params(&env, || "from-uuid".to_string());
        assert_eq!(params.session_id, "from-uuid");
    }

    #[test]
    fn a_missing_env_var_falls_back_to_the_uuid_source() {
        let env = MockEnv(None);
        let params = resolve_session_params(&env, || "from-uuid".to_string());
        assert_eq!(params.session_id, "from-uuid");
    }

    fn params() -> SessionParams {
        SessionParams {
            session_id: "sess-1".to_string(),
            worktree_root: Some("/repo".to_string()),
        }
    }

    fn duplex_halves() -> (
        BufReader<ReadHalf<tokio::io::DuplexStream>>,
        WriteHalf<tokio::io::DuplexStream>,
        tokio::io::DuplexStream,
    ) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = split(client);
        (BufReader::new(client_read), client_write, server)
    }

    #[tokio::test]
    async fn a_compatible_welcome_is_returned() {
        let (mut reader, mut writer, mut server) = duplex_halves();
        let welcome = Welcome {
            proto: PROTO_VERSION,
            daemon_version: local_rag_core::VERSION.to_string(),
            store_instance_uuid: "instance-a".to_string(),
            capabilities: Vec::new(),
            mcp_passthrough_version: MCP_PASSTHROUGH_VERSION,
            spool_max_format_version: local_rag_core::spool::FORMAT_VERSION,
            mode: "normal".to_string(),
        };
        let bytes = local_rag_protocol::encode_message(&Message::Welcome(welcome.clone())).unwrap();
        server.write_all(&bytes).await.unwrap();

        let got = do_handshake(&mut reader, &mut writer, &params())
            .await
            .unwrap();
        assert_eq!(got.store_instance_uuid, "instance-a");
    }

    #[tokio::test]
    async fn an_incompatible_reply_surfaces_both_bounds() {
        let (mut reader, mut writer, mut server) = duplex_halves();
        let incompatible = Incompatible {
            min_proto: 2,
            max_proto: 3,
            daemon_version: "9.9.9".to_string(),
        };
        let bytes =
            local_rag_protocol::encode_message(&Message::Incompatible(incompatible)).unwrap();
        server.write_all(&bytes).await.unwrap();

        let err = do_handshake(&mut reader, &mut writer, &params())
            .await
            .unwrap_err();
        match err {
            ProxyError::Incompatible {
                min_proto,
                max_proto,
                ..
            } => {
                assert_eq!(min_proto, 2);
                assert_eq!(max_proto, 3);
            }
            other => panic!("expected Incompatible, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_connection_closed_before_any_reply_is_handshake_closed() {
        let (mut reader, mut writer, server) = duplex_halves();
        // Let the HELLO write land in the (ample, 64KiB) duplex buffer
        // before closing — this test isolates the *read* side observing a
        // clean close, not a write failure racing an unread buffer.
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            drop(server);
        });
        let err = do_handshake(&mut reader, &mut writer, &params())
            .await
            .unwrap_err();
        assert!(matches!(err, ProxyError::HandshakeClosed));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_close_times_out_when_the_peer_never_closes() {
        let (mut reader, _writer, _server) = duplex_halves(); // server kept alive, never closed
        let result =
            wait_for_close(&mut reader, Duration::from_millis(UPGRADE_CLOSE_TIMEOUT_MS)).await;
        assert!(matches!(result, Err(ProxyError::UpgradeTimedOut)));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_close_succeeds_once_the_peer_closes_within_the_budget() {
        let (mut reader, _writer, server) = duplex_halves();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(server);
        });
        let result =
            wait_for_close(&mut reader, Duration::from_millis(UPGRADE_CLOSE_TIMEOUT_MS)).await;
        assert!(result.is_ok());
    }

    #[test]
    fn matching_versions_produce_no_warning() {
        assert_eq!(check_spool_format_compatibility(1, 1), None);
    }

    #[test]
    fn a_daemon_ahead_of_this_release_produces_no_warning() {
        assert_eq!(check_spool_format_compatibility(1, 2), None);
    }

    #[test]
    fn a_daemon_behind_this_release_produces_a_warning_naming_both_versions() {
        let warning = check_spool_format_compatibility(2, 1)
            .expect("a hook newer than the daemon supports must warn");
        assert_eq!(warning.compiled_format_version, 2);
        assert_eq!(warning.daemon_max_format_version, 1);
    }
}
