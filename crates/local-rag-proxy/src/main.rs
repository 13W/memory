//! `local-rag-proxy` — per-session stdio MCP proxy entry point.
//!
//! Scaffold only (T00-02): the only wired command is `version`. The proxy
//! <-> daemon handshake arrives in group 15.

use std::process::ExitCode;

const BIN: &str = "local-rag-proxy";

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("version" | "--version" | "-V") => {
            println!("{}", local_rag_core::version_line(BIN));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: {BIN} version");
            ExitCode::from(2)
        }
    }
}
