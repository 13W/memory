//! ADR link-check (T04-01 acceptance).
//!
//! Every relative markdown link in the architecture decision records under
//! `docs/adr/` must resolve to a file that exists on disk. This keeps ADRs (which
//! amend the specification) from rotting as files move. It is a plain, offline,
//! `$HOME`-independent Rust test: external `http(s)`/`mailto` targets are skipped
//! (the whole gate runs offline — see `CONTRIBUTING.md`), only on-disk targets are
//! verified.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // crates/xtask -> crates -> workspace root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn adr_dir() -> PathBuf {
    workspace_root().join("docs/adr")
}

/// Collect every `](target)` link target in `md`, in source order.
fn link_targets(md: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let bytes = md.as_bytes();
    let mut i = 0;
    while let Some(rel) = md[i..].find("](") {
        let start = i + rel + 2;
        // Read until the closing paren; a link title (`](path "title")`) is
        // separated from the target by whitespace, so stop at the first space too.
        let mut end = start;
        while end < bytes.len() && bytes[end] != b')' {
            end += 1;
        }
        let raw = &md[start..end];
        let target = raw.split_whitespace().next().unwrap_or("");
        if !target.is_empty() {
            targets.push(target.to_string());
        }
        i = end;
    }
    targets
}

fn is_external(target: &str) -> bool {
    target.starts_with("http://") || target.starts_with("https://") || target.starts_with("mailto:")
}

#[test]
fn adr_relative_links_resolve_on_disk() {
    let dir = adr_dir();
    let entries = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect::<Vec<_>>();
    assert!(
        !entries.is_empty(),
        "expected at least one ADR under {}",
        dir.display()
    );

    let mut checked = 0usize;
    for adr in &entries {
        let md = fs::read_to_string(adr).unwrap_or_else(|e| panic!("read {}: {e}", adr.display()));
        let base = adr.parent().expect("adr has a parent dir");
        for target in link_targets(&md) {
            // In-page anchors (`#section`) reference the same file; nothing to
            // resolve on disk.
            if target.starts_with('#') || is_external(&target) {
                continue;
            }
            // Strip any `#anchor` fragment; verify the file part exists.
            let path_part = target.split('#').next().unwrap_or(&target);
            if path_part.is_empty() {
                continue;
            }
            let resolved: PathBuf = base.join(path_part);
            assert!(
                resolved.exists(),
                "{}: link target `{target}` does not exist (resolved to {})",
                adr.display(),
                resolved.display()
            );
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "expected to verify at least one relative ADR link"
    );
}

#[test]
fn first_release_language_adr_is_well_formed() {
    let path = adr_dir().join("0001-first-release-language-set.md");
    let adr = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    for section in ["## Status", "## Context", "## Decision", "## Consequences"] {
        assert!(
            adr.contains(section),
            "ADR-0001 must contain a `{section}` section"
        );
    }
    assert!(
        adr.contains("Accepted"),
        "ADR-0001 must record an Accepted status"
    );
}

#[test]
fn syntax_locator_derivation_adr_is_well_formed() {
    let path = adr_dir().join("0002-syntax-locator-derivation.md");
    let adr = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    for section in ["## Status", "## Context", "## Decision", "## Consequences"] {
        assert!(
            adr.contains(section),
            "ADR-0002 must contain a `{section}` section"
        );
    }
    assert!(
        adr.contains("Accepted"),
        "ADR-0002 must record an Accepted status"
    );
    assert!(
        adr.contains("O7"),
        "ADR-0002 must reference the open question O7 it resolves"
    );
}

/// Guard the resolver used above so a false "all links pass" can't hide behind a
/// broken parser.
#[test]
fn link_target_parser_extracts_and_classifies() {
    let md = "see [a](./x.md) and [b](../y.md#anchor) and [c](https://z.example) \
              and [d](#local) and [e](p.md \"title\")";
    let targets = link_targets(md);
    assert_eq!(
        targets,
        vec![
            "./x.md",
            "../y.md#anchor",
            "https://z.example",
            "#local",
            "p.md"
        ]
    );
    assert!(is_external("https://z.example"));
    assert!(is_external("mailto:x@y.z"));
    assert!(!is_external("../y.md"));
    // Fragment stripping keeps the file part.
    assert_eq!(
        Path::new("../y.md#anchor")
            .to_string_lossy()
            .split('#')
            .next(),
        Some("../y.md")
    );
}
