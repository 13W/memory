//! Path canonicalization (spec 03 §1.3).
//!
//! Canonicalization produces the *identity* form of a path while preserving its
//! original spelling for display. Identity never depends on the display form
//! (spec 01 §5), so both live side by side in [`Canonical`].
//!
//! Relative (worktree-relative) rules: `/` separators, no leading `./`, Unicode
//! **NFC**, and — on case-insensitive filesystems — simple case folding.
//! Absolute paths add symlink resolution, Windows drive-letter upcasing, and
//! UNC normalization.
//!
//! The filesystem-touching step (symlink resolution) is isolated in
//! [`canonicalize_absolute`]; every string-level rule is a pure function
//! ([`normalize_relative`], [`normalize_absolute_str`]) that is unit-testable on
//! any platform without touching the filesystem.

use std::io;
use std::path::Path;

use unicode_normalization::UnicodeNormalization;

/// Whether the target filesystem distinguishes case. Supplied by the caller
/// (the registry knows each worktree's filesystem); the primitive itself is
/// deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseSensitivity {
    /// Case-preserving and case-distinguishing (e.g. ext4, APFS case-sensitive).
    Sensitive,
    /// Case-insensitive (e.g. NTFS, APFS default, exFAT): fold for identity.
    Insensitive,
}

/// A canonicalized path plus its preserved display spelling.
///
/// `canonical` is the identity form (the only one that ever feeds a hash);
/// `display` is the caller's original spelling, kept verbatim for presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canonical {
    /// Identity form (`/`-separated, NFC, optionally case-folded).
    pub canonical: String,
    /// Original spelling, preserved verbatim.
    pub display: String,
}

/// Canonicalize a worktree-relative path (spec 03 §1.3).
///
/// Collapses `\` to `/`, drops `.` and empty segments (so a leading `./` and any
/// `//` disappear and a trailing `/` is removed), applies NFC, and — when
/// `case` is [`CaseSensitivity::Insensitive`] — simple case folding. `..`
/// segments are left as literal components (a worktree-relative path is not
/// resolved against a base here).
pub fn normalize_relative(input: &str, case: CaseSensitivity) -> Canonical {
    let canonical = apply_unicode(&normalize_separators_and_dots(input), case);
    Canonical {
        canonical,
        display: input.to_string(),
    }
}

/// Apply the string-level rules of absolute canonicalization to an
/// already lexically-resolved absolute path (spec 03 §1.3).
///
/// Handles Windows verbatim (`\\?\`) and verbatim-UNC (`\\?\UNC\`) prefixes,
/// collapses `\` to `/`, applies NFC and optional case folding, and upcases the
/// drive letter. It does **not** resolve symlinks or `.`/`..` — that is the job
/// of [`std::fs::canonicalize`] via [`canonicalize_absolute`].
pub fn normalize_absolute_str(input: &str, case: CaseSensitivity) -> String {
    let unified = strip_windows_verbatim(input).replace('\\', "/");
    let unicoded = apply_unicode(&unified, case);
    upcase_drive_letter(&unicoded)
}

/// Canonicalize an absolute path, resolving symlinks and `.`/`..` via the
/// filesystem, then applying the string-level rules (spec 03 §1.3).
///
/// `display` preserves the caller's original spelling.
///
/// # Errors
///
/// Returns any [`std::fs::canonicalize`] error (e.g. the path does not exist).
pub fn canonicalize_absolute(path: &Path, case: CaseSensitivity) -> io::Result<Canonical> {
    let display = path.display().to_string();
    let resolved = std::fs::canonicalize(path)?;
    Ok(Canonical {
        canonical: normalize_absolute_str(&resolved.to_string_lossy(), case),
        display,
    })
}

fn normalize_separators_and_dots(input: &str) -> String {
    let unified = input.replace('\\', "/");
    unified
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect::<Vec<_>>()
        .join("/")
}

fn apply_unicode(input: &str, case: CaseSensitivity) -> String {
    // NFC first, then simple case folding: identity is stable regardless of the
    // caller's Unicode composition, and case-insensitive filesystems match paths
    // that differ only by case.
    let nfc: String = input.nfc().collect();
    match case {
        CaseSensitivity::Sensitive => nfc,
        CaseSensitivity::Insensitive => casefold::simple_fold(nfc),
    }
}

fn strip_windows_verbatim(input: &str) -> String {
    if let Some(rest) = input.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = input.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        input.to_string()
    }
}

fn upcase_drive_letter(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let mut out = String::with_capacity(path.len());
        out.push(bytes[0].to_ascii_uppercase() as char);
        out.push_str(&path[1..]);
        out
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use CaseSensitivity::{Insensitive, Sensitive};

    #[test]
    fn relative_strips_leading_dot_and_collapses_separators() {
        assert_eq!(
            normalize_relative("./src/main.rs", Sensitive).canonical,
            "src/main.rs"
        );
        assert_eq!(
            normalize_relative("src//lib.rs", Sensitive).canonical,
            "src/lib.rs"
        );
        assert_eq!(
            normalize_relative("src/mod/", Sensitive).canonical,
            "src/mod"
        );
        assert_eq!(
            normalize_relative(r"a\b\c.rs", Sensitive).canonical,
            "a/b/c.rs"
        );
    }

    #[test]
    fn relative_preserves_display() {
        let c = normalize_relative("./SRC/Main.RS", Insensitive);
        assert_eq!(c.display, "./SRC/Main.RS");
        assert_eq!(c.canonical, "src/main.rs");
    }

    #[test]
    fn relative_applies_nfc() {
        // "cafe" + combining acute accent → NFC "café".
        let c = normalize_relative("cafe\u{0301}/x.rs", Sensitive);
        assert_eq!(c.canonical, "caf\u{00e9}/x.rs");
        // Composed and decomposed spellings map to the same identity …
        let d = normalize_relative("caf\u{00e9}/x.rs", Sensitive);
        assert_eq!(c.canonical, d.canonical);
        // … while each display keeps its original bytes.
        assert_ne!(c.display, d.display);
    }

    #[test]
    fn relative_case_fold_only_when_insensitive() {
        assert_eq!(
            normalize_relative("Dir/File.RS", Sensitive).canonical,
            "Dir/File.RS"
        );
        assert_eq!(
            normalize_relative("Dir/File.RS", Insensitive).canonical,
            "dir/file.rs"
        );
    }

    #[test]
    fn absolute_posix_is_unchanged_when_sensitive() {
        assert_eq!(
            normalize_absolute_str("/home/user/Project", Sensitive),
            "/home/user/Project"
        );
        assert_eq!(
            normalize_absolute_str("/home/user/Project", Insensitive),
            "/home/user/project"
        );
    }

    #[test]
    fn absolute_upcases_drive_letter() {
        assert_eq!(
            normalize_absolute_str("c:/Users/Foo", Sensitive),
            "C:/Users/Foo"
        );
        assert_eq!(
            normalize_absolute_str(r"\\?\C:\Users\Foo", Sensitive),
            "C:/Users/Foo"
        );
        assert_eq!(
            normalize_absolute_str(r"\\?\c:\users\foo", Sensitive),
            "C:/users/foo"
        );
    }

    #[test]
    fn absolute_drive_stays_upcased_under_folding() {
        // Folding lowercases the body, but the drive letter is upcased last.
        assert_eq!(
            normalize_absolute_str(r"\\?\C:\Users\FOO", Insensitive),
            "C:/users/foo"
        );
    }

    #[test]
    fn absolute_normalizes_unc() {
        assert_eq!(
            normalize_absolute_str(r"\\?\UNC\server\share\dir", Sensitive),
            "//server/share/dir",
        );
    }

    #[test]
    fn absolute_applies_nfc() {
        assert_eq!(
            normalize_absolute_str("/x/cafe\u{0301}", Sensitive),
            "/x/caf\u{00e9}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonicalize_resolves_symlink_and_keeps_display() {
        use local_rag_test_support::TempHome;
        use std::os::unix::fs::symlink;

        let home = TempHome::new().expect("temp home");
        let target = home.join("real.rs");
        std::fs::write(&target, b"fn main() {}").expect("write target");
        let link = home.join("link.rs");
        symlink(&target, &link).expect("create symlink");

        let via_link = canonicalize_absolute(&link, Sensitive).expect("canonicalize link");
        let via_target = canonicalize_absolute(&target, Sensitive).expect("canonicalize target");

        // The symlink resolves to the same identity as its target …
        assert_eq!(via_link.canonical, via_target.canonical);
        // … but the display preserves the path the caller passed in.
        assert_eq!(via_link.display, link.display().to_string());
        assert_ne!(via_link.display, via_link.canonical);
    }
}
