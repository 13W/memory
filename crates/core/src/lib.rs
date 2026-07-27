//! Foundational types for `local-rag`.
//!
//! Beyond the shared version string, this crate hosts the [`paths`] platform
//! abstraction (store/config directory resolution, layout, endpoint, and
//! permission primitives) that every binary shares, the [`config`] model (the
//! versioned `config.toml` and the `data_policy` restrictiveness merge), the
//! vendored [`hash`] digest used for stable namespacing and migration checksums,
//! the [`identity`] primitives (UUIDv7, domain-separated BLAKE3 hashing, path
//! canonicalization, remote normalization) that the registry and every durable
//! ID are built from, the shared, versioned [`redaction`] secret scanner
//! reused by file classification, spool ingestion, and remote transmission,
//! and the [`spool`] LRSP wire-format primitives (T13-03, relocated here from
//! `local-rag-hook` so the hook write path and the daemon-side read path
//! share one CRC/header/frame implementation, never two that could drift).

pub mod config;
pub mod hash;
pub mod identity;
pub mod paths;
pub mod redaction;
pub mod spool;

pub use config::{Config, ConfigError, DataPolicy};

/// The workspace version, shared by every `local-rag` binary.
///
/// ```
/// assert!(!local_rag_core::VERSION.is_empty());
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Format the canonical `version` line for a binary named `bin`.
///
/// ```
/// assert_eq!(
///     local_rag_core::version_line("local-rag"),
///     format!("local-rag {}", local_rag_core::VERSION),
/// );
/// ```
pub fn version_line(bin: &str) -> String {
    format!("{bin} {VERSION}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn version_line_combines_name_and_version() {
        assert_eq!(version_line("demo"), format!("demo {VERSION}"));
    }
}
