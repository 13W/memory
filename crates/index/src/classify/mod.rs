//! Deterministic file classification and skip reasons (spec 06 §2.2, 12 §2/§5).
//!
//! [`classify`] decides, for one file, whether it joins the searchable generation
//! ([`Classification::Indexed`]) or is recorded as a [`SkipReason`]
//! ([`Classification::Skipped`]). A skipped file gets a `skipped_file` row and, by
//! the schema's structural invariant, no `source_blob` and no occurrences (spec 12
//! §5) — the caller records it via `store::insert_skipped_file` and never
//! `insert_file_revision`.
//!
//! # Precedence `[SPEC]`
//!
//! The spec fixes the *set* of six reasons but not their order, while the
//! `skipped_file` primary key admits exactly one reason per path. T03-02 therefore
//! authors a deterministic **precondition chain**, first match wins:
//!
//! 1. `ignored` — gitignore / configured excludes (path only, no content read).
//! 2. `huge` — `size_bytes` exceeds the configured cap (stat only).
//! 3. `lfs` — a Git-LFS pointer file.
//! 4. `binary` — NUL heuristic or a binary extension.
//! 5. `encoding` — content is not valid UTF-8 (v0 supports only UTF-8).
//! 6. `secret` — the shared redaction scanner (spec 12 §2) flags the decoded text.
//!
//! Each step is a precondition for the next: the secret scan runs only on content
//! already known to be valid, decoded UTF-8 text. Because every outcome is a skip,
//! short-circuiting a cheaper reason before `secret` never causes a secret-bearing
//! file to be indexed. `content` is inspected only after the `ignored`/`huge`
//! gates, so a caller may pass an empty slice for a known-huge file it declined to
//! read.
//!
//! # Scope
//!
//! Pure classification only. Content hashing, `source_encoding`/`newline_style`
//! detection, and zstd/reuse are T03-03; the normalized-text cache is T03-04. The
//! authoritative directory walk is [`crate::scan`] (T05-02): its `ignore` walk
//! prunes `ignored` files during traversal, so in the reconcile pipeline the
//! `ignored` branch below is defense-in-depth (ignored files never reach it). The
//! remaining, content-based reasons (`lfs`/`binary`/`encoding`/`secret`) are
//! applied by the generation builder (T05-03) when it reads a manifest entry's
//! bytes on a `file_revision` miss.

pub mod detect;
pub mod gitignore;

use local_rag_core::config::IndexConfig;
use local_rag_core::redaction::Scanner;
use local_rag_store::SkipReason;

pub use gitignore::{GitignoreSet, GitignoreSetBuilder};

/// The outcome of classifying a single file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// The file joins the searchable generation (gets a `file_revision` +
    /// `generation_file` membership, later tasks).
    Indexed,
    /// The file is skipped for exactly one reason (gets a `skipped_file` row,
    /// never a `source_blob`).
    Skipped(SkipReason),
}

/// Tunable inputs to [`classify`] (spec 02 §3.1 `[index]`).
///
/// The binary-extension list is a built-in ([`detect::BINARY_EXTENSIONS`]) rather
/// than config; only the size cap is configurable in v0.
#[derive(Debug, Clone, Copy)]
pub struct ClassifierConfig {
    /// Files strictly larger than this are `huge` (bytes).
    pub max_file_size_bytes: u64,
}

impl ClassifierConfig {
    /// A config with an explicit byte cap.
    pub fn new(max_file_size_bytes: u64) -> ClassifierConfig {
        ClassifierConfig {
            max_file_size_bytes,
        }
    }

    /// Derive the cap from the global `[index]` config (`max_file_size_kb`).
    pub fn from_index_config(cfg: &IndexConfig) -> ClassifierConfig {
        ClassifierConfig {
            max_file_size_bytes: cfg.max_file_size_kb.saturating_mul(1024),
        }
    }
}

/// Classify one file into [`Classification::Indexed`] or a single [`SkipReason`],
/// applying the documented precedence chain.
///
/// - `normalized_path`: forward-slash path relative to the repository root.
/// - `size_bytes`: the file's true size (from stat), used for the `huge` gate.
/// - `content`: the file bytes; inspected only past the `ignored`/`huge` gates.
/// - `gitignore` / `cfg` / `scanner`: the `ignored`, `huge`, and `secret` inputs.
pub fn classify(
    normalized_path: &str,
    size_bytes: u64,
    content: &[u8],
    gitignore: &GitignoreSet,
    cfg: &ClassifierConfig,
    scanner: &Scanner,
) -> Classification {
    // 1. ignored — path only.
    if gitignore.is_ignored(normalized_path, false) {
        return Classification::Skipped(SkipReason::Ignored);
    }
    // 2. huge — strictly greater than the cap (a file exactly at the cap is kept).
    if size_bytes > cfg.max_file_size_bytes {
        return Classification::Skipped(SkipReason::Huge);
    }
    // 3. lfs — pointer format (small text; caught even at a binary extension).
    if detect::is_lfs_pointer(content) {
        return Classification::Skipped(SkipReason::Lfs);
    }
    // 4. binary — NUL heuristic or binary extension.
    if detect::is_binary(normalized_path, content) {
        return Classification::Skipped(SkipReason::Binary);
    }
    // 5. encoding — must be valid UTF-8 to proceed (v0 supports only UTF-8).
    let Ok(text) = std::str::from_utf8(content) else {
        return Classification::Skipped(SkipReason::Encoding);
    };
    // 6. secret — decoded text scanned by the shared redaction scanner.
    if scanner.has_secret(text) {
        return Classification::Skipped(SkipReason::Secret);
    }
    Classification::Indexed
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u64 = 1024;

    fn cfg() -> ClassifierConfig {
        ClassifierConfig::new(CAP)
    }

    fn no_ignores() -> GitignoreSet {
        GitignoreSet::empty()
    }

    fn classify_bytes(path: &str, content: &[u8]) -> Classification {
        classify(
            path,
            content.len() as u64,
            content,
            &no_ignores(),
            &cfg(),
            &Scanner::new(),
        )
    }

    #[test]
    fn clean_source_is_indexed() {
        assert_eq!(
            classify_bytes("src/main.rs", b"fn main() { println!(\"hi\"); }\n"),
            Classification::Indexed
        );
    }

    #[test]
    fn one_fixture_per_reason() {
        // ignored
        let mut b = GitignoreSetBuilder::new("/repo");
        b.add_gitignore(".", "*.log\n");
        let ignores = b.build().expect("gitignore");
        assert_eq!(
            classify("app.log", 3, b"hey", &ignores, &cfg(), &Scanner::new()),
            Classification::Skipped(SkipReason::Ignored)
        );

        // huge — size over the cap
        assert_eq!(
            classify(
                "big.rs",
                CAP + 1,
                b"",
                &no_ignores(),
                &cfg(),
                &Scanner::new()
            ),
            Classification::Skipped(SkipReason::Huge)
        );

        // lfs — pointer file
        let ptr = "version https://git-lfs.github.com/spec/v1\n\
                   oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
                   size 12345\n";
        assert_eq!(
            classify_bytes("assets/model.bin", ptr.as_bytes()),
            Classification::Skipped(SkipReason::Lfs)
        );

        // binary — NUL byte
        assert_eq!(
            classify_bytes("data/x", b"ab\0cd"),
            Classification::Skipped(SkipReason::Binary)
        );

        // encoding — invalid UTF-8, no NUL, not a binary extension
        assert_eq!(
            classify_bytes("weird.txt", &[0xFF, 0xFE, 0x41]),
            Classification::Skipped(SkipReason::Encoding)
        );

        // secret — a credential in valid text
        assert_eq!(
            classify_bytes("config.py", b"aws = \"AKIAIOSFODNN7EXAMPLE\"\n"),
            Classification::Skipped(SkipReason::Secret)
        );
    }

    #[test]
    fn huge_exact_size_edge() {
        // Exactly at the cap → indexed; one byte over → huge.
        assert_eq!(
            classify("f.rs", CAP, b"x", &no_ignores(), &cfg(), &Scanner::new()),
            Classification::Indexed
        );
        assert_eq!(
            classify(
                "f.rs",
                CAP + 1,
                b"x",
                &no_ignores(),
                &cfg(),
                &Scanner::new()
            ),
            Classification::Skipped(SkipReason::Huge)
        );
    }

    #[test]
    fn precedence_ignored_beats_everything() {
        // A gitignored, huge, binary, secret-bearing file is reported `ignored`.
        let mut b = GitignoreSetBuilder::new("/repo");
        b.add_gitignore(".", "*.png\n");
        let ignores = b.build().expect("gitignore");
        assert_eq!(
            classify(
                "assets/logo.png",
                CAP + 999,
                b"AKIAIOSFODNN7EXAMPLE\0",
                &ignores,
                &cfg(),
                &Scanner::new()
            ),
            Classification::Skipped(SkipReason::Ignored)
        );
    }

    #[test]
    fn precedence_huge_beats_binary_and_secret() {
        // Not ignored, but over the cap: `huge` wins even though content would
        // also be binary/secret. Content is not even inspected.
        assert_eq!(
            classify(
                "blob.bin",
                CAP + 1,
                b"AKIAIOSFODNN7EXAMPLE\0",
                &no_ignores(),
                &cfg(),
                &Scanner::new()
            ),
            Classification::Skipped(SkipReason::Huge)
        );
    }

    #[test]
    fn precedence_binary_beats_secret() {
        // A NUL-bearing file that also contains a credential is `binary`, not
        // `secret` (the secret scan requires valid decoded text).
        assert_eq!(
            classify_bytes("mixed", b"AKIAIOSFODNN7EXAMPLE\0more"),
            Classification::Skipped(SkipReason::Binary)
        );
    }

    #[test]
    fn config_cap_derives_from_index_config() {
        let idx = IndexConfig {
            languages: vec![],
            max_file_size_kb: 2,
        };
        let c = ClassifierConfig::from_index_config(&idx);
        assert_eq!(c.max_file_size_bytes, 2048);
    }
}
