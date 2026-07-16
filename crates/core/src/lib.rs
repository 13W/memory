//! Foundational types for `local-rag`.
//!
//! Beyond the shared version string, this crate hosts the [`paths`] platform
//! abstraction (store/config directory resolution, layout, endpoint, and
//! permission primitives) that every binary shares, plus the vendored [`hash`]
//! digest used for stable namespacing and migration checksums.

pub mod hash;
pub mod paths;

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
