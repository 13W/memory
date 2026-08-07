//! A single-shot tokio runtime for driving `StateWriter::transaction` from this crate's fully
//! synchronous event loop (T18-05, Memory mutations). This crate's own equivalent of `local-rag`'s
//! `cli::block_on` (`crates/local-rag/src/cli/mod.rs`) — unreachable here: that helper is
//! `pub(crate)` inside `mod cli;`, itself declared only on `local-rag`'s **binary** target
//! (`main.rs`), never its library half, which is all `local-rag-tui` links.
//!
//! Builds a fresh runtime per call, exactly mirroring the CLI's own "one throwaway runtime per
//! invocation" shape — a mutation is one bounded-channel round trip to a local SQLite write, not a
//! hot path, so paying a fresh runtime's startup cost per keypress is a non-issue. `main.rs`'s own
//! event loop stays fully synchronous; this is the sole point anywhere in this crate's production
//! code that ever enters an async context.

use std::future::Future;

pub fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread tokio runtime")
        .block_on(fut)
}
