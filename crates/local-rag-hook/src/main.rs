//! `local-rag-hook` — spool writer invoked by Claude Code hooks.
//!
//! Scaffold only (T00-02): the only wired command is `version`. Durable spool
//! append + fail-open behavior arrive in group 13.

use std::process::ExitCode;

const BIN: &str = "local-rag-hook";

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
