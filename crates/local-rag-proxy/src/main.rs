//! `local-rag-proxy` — per-session stdio MCP proxy entry point (spec 13 §1's
//! multiplexed per-session model, 02 §4.2's HELLO/WELCOME handshake, 11 §1's
//! thin pass-through).
//!
//! Holds no project state: no `local-rag-store`/`-embed`/`-index` dependency
//! exists in this crate's `Cargo.toml` at all — a structural guarantee, not
//! a discipline one, that nothing here could accumulate state across
//! sessions even by accident. The only state this binary ever holds is one
//! open connection and the [`handshake::SessionParams`] it was launched
//! with.

mod connect;
mod error;
mod handshake;
mod relay;
mod transport;

use std::process::ExitCode;

#[cfg(unix)]
use local_rag_core::identity::{SystemUuidV7, UuidSource};
#[cfg(unix)]
use local_rag_core::paths::{StoreLayout, SystemEnv};
#[cfg(unix)]
use local_rag_protocol::RequestContext;

#[cfg(unix)]
use handshake::{check_spool_format_compatibility, establish_session, resolve_session_params};

const BIN: &str = "local-rag-proxy";

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("version" | "--version" | "-V") => {
            println!("{}", local_rag_core::version_line(BIN));
            ExitCode::SUCCESS
        }
        None => run_proxy(),
        Some(_) => {
            eprintln!("usage: {BIN} [version]");
            ExitCode::from(2)
        }
    }
}

/// This proxy's own runtime: `Builder::new_current_thread()`, not
/// `rt-multi-thread` — a single UDS connection relaying stdin/stdout is
/// entirely IO-bound, "thin" per spec 11 §1.
fn run_proxy() -> ExitCode {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{BIN}: could not start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let exit_code = rt.block_on(run());
    // `tokio::io::stdin()` reads via a background thread doing a genuine
    // blocking `read()` on fd 0 (there is no async stdin on unix). When
    // `relay` returns because the shutdown signal fired rather than because
    // stdin reached EOF, that thread is left blocked in that syscall
    // forever. `Runtime::drop` blocks its caller until every outstanding
    // `spawn_blocking` task completes — so simply letting `rt` drop here
    // would hang the process indefinitely in exactly that scenario
    // (reproduced directly with a minimal repro while building this task).
    // `std::process::exit` terminates immediately instead, abandoning that
    // thread; nothing past this point needs `Drop`-based cleanup — the UDS
    // connection and stdio handles are already meaningless once the process
    // is gone.
    std::process::exit(exit_code.into());
}

/// `0` on success, `1` on any failure — plain rather than [`ExitCode`]
/// because [`run_proxy`] must feed it to [`std::process::exit`], and
/// `ExitCode` is deliberately opaque (no `From<ExitCode> for i32`, no
/// `PartialEq`) beyond being returned from `main`.
///
/// Windows has no local IPC transport to the daemon yet (named-pipe support
/// across this crate/`local-rag`/`local-rag-hook` is not implemented — D-033,
/// a separate follow-up, not part of this platform-portability fix). This
/// exits with a clear, typed message rather than failing to compile or
/// hanging on a connect attempt that can never succeed.
#[cfg(not(unix))]
async fn run() -> u8 {
    eprintln!(
        "{BIN}: not yet supported on this platform (no local IPC transport implemented for Windows)"
    );
    1
}

#[cfg(unix)]
async fn run() -> u8 {
    // Installed before any other work — see `daemon::shutdown::
    // ShutdownSignal::install`'s own doc comment (T15-01) for why this
    // ordering is load-bearing, not stylistic: `tokio::signal::unix::signal`
    // registers with the OS at **call** time, not at the first `.recv()`.
    // A SIGTERM arriving during the (up to 20s) connect-or-spawn backoff
    // below must still be observed once `relay`'s own wait loop starts, not
    // lost to the OS default terminate-immediately disposition in the
    // meantime.
    let signal = relay::ShutdownSignal::install();

    let env = SystemEnv;
    let layout = match StoreLayout::resolve(&env) {
        Ok(layout) => layout,
        Err(e) => {
            eprintln!("{BIN}: could not resolve the store directory: {e}");
            return 1;
        }
    };

    let daemon_binary = match std::env::current_exe()
        .ok()
        .and_then(|exe| connect::resolve_daemon_binary_path(&exe))
    {
        Some(path) => path,
        None => {
            eprintln!("{BIN}: {}", error::ProxyError::DaemonBinaryNotFound);
            return 1;
        }
    };

    let params = resolve_session_params(&env, || SystemUuidV7.next_uuid().to_string());

    let session = match establish_session(&layout.socket_path(), &daemon_binary, &params).await {
        Ok(session) => session,
        Err(e) => {
            eprintln!("{BIN}: {e}");
            return 1;
        }
    };
    // Migration-only is a successful handshake in a degraded serving mode
    // (spec 02 §6 `[FIXED]`: "nothing degrades silently") — surfaced here
    // rather than left implicit in every tool call's own response.
    if session.welcome.mode != "normal" {
        eprintln!(
            "{BIN}: the daemon is running in degraded mode: {}",
            session.welcome.mode
        );
    }
    if let Some(warning) = check_spool_format_compatibility(
        local_rag_core::spool::FORMAT_VERSION,
        session.welcome.spool_max_format_version,
    ) {
        eprintln!("{BIN}: {warning}");
    }

    let context = RequestContext {
        session_id: params.session_id.clone(),
        worktree_root: params.worktree_root.clone(),
        repo_hint: None,
    };

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();

    match relay::relay(
        stdin,
        stdout,
        session.reader,
        session.writer,
        context,
        signal,
    )
    .await
    {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{BIN}: {e}");
            1
        }
    }
}
