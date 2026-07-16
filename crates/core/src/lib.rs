//! Foundational types for `local-rag`.
//!
//! This crate is intentionally minimal at the T00-02 scaffold stage: it holds
//! only the shared version string used by every binary. No business logic
//! lives here yet — later groups grow it in place.

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
