//! T21-02 source lint (ADR-0010 Decision 4): the raw memory-subject function
//! has exactly two call sites in production code — its own definition and the
//! one hasher every reader goes through.
//!
//! [`EffectiveText`](local_rag_store::EffectiveText) makes it impossible to
//! hash an *arbitrary* string as a stored entry's subject: it has private
//! fields and no constructor, so the only way to obtain one is to ask
//! `decide_effective_text`. What the type cannot prevent is a third reader
//! reaching past it and calling `local_rag_core::identity::domain::
//! subject_memory_entry(id, text)` directly with whatever text it happens to
//! hold — which is exactly the divergence this group exists to make
//! impossible, and which no Rust visibility rule can forbid across crates for
//! a `pub` item of a dependency.
//!
//! So it is forbidden here instead. The lint matches **calls**
//! (`subject_memory_entry(`), not mentions: prose referring to the rule — the
//! hook crate's identity doc, `subjects.rs`'s own reminder — is the lint doing
//! its job, not a violation of it.
//!
//! Deterministic: reads the workspace's own sources from disk, no network, no
//! external tool (no `rg`/`grep` dependency).

use std::path::{Path, PathBuf};

/// Files allowed to call the raw subject function, relative to the workspace
/// root:
///
/// - the definition itself, next to its siblings and its known-answer tests;
/// - the one hasher (`memory_entry_subject_hash`), which takes an
///   `EffectiveText` and is what every reader must use.
const ALLOWED: &[&str] = &[
    "crates/core/src/identity/domain.rs",
    "crates/store/src/memory/effective_text.rs",
];

fn workspace_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `<root>/crates/store`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root is two levels above crates/store")
        .to_path_buf()
}

/// Every `.rs` file under `crates/*/src` — production code only. Integration
/// tests (`crates/*/tests`) legitimately call the raw function to pin its
/// known answers, and are deliberately out of scope.
fn production_sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let crates_dir = root.join("crates");
    let mut stack: Vec<PathBuf> = std::fs::read_dir(&crates_dir)
        .expect("read crates/")
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("src"))
        .filter(|p| p.is_dir())
        .collect();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read a src dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    assert!(
        out.len() > 50,
        "sanity: the walk must actually find this workspace's sources, found {}",
        out.len()
    );
    out
}

#[test]
fn the_raw_memory_subject_function_has_exactly_two_call_sites() {
    let root = workspace_root();
    let mut callers: Vec<String> = Vec::new();
    for path in production_sources(&root) {
        let text = std::fs::read_to_string(&path).expect("read a source file");
        if text.contains("subject_memory_entry(") {
            let relative = path
                .strip_prefix(&root)
                .expect("every source is under the workspace root")
                .to_string_lossy()
                .replace('\\', "/");
            callers.push(relative);
        }
    }
    callers.sort();

    let expected: Vec<String> = ALLOWED.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        callers, expected,
        "the raw subject function must be called only by its definition and by \
         `memory_entry_subject_hash`; a third caller can silently disagree with the \
         other two about which text an entry is embedded under, which is invisible \
         at runtime — the dense leg just returns nothing (ADR-0010 Decision 4)",
    );
}

/// Guard the guard: if the search string stopped matching anything, the lint
/// above would pass vacuously forever.
#[test]
fn the_lint_would_notice_an_extra_caller() {
    let root = workspace_root();
    let definition = root.join(ALLOWED[0]);
    let text = std::fs::read_to_string(&definition).expect("read the definition");
    assert!(
        text.contains("pub fn subject_memory_entry("),
        "the lint's search string must still match the real definition",
    );
}
