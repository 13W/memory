//! Versioned, code-aware lexical preprocessing and the FTS manifest identity
//! (spec 09 §2, 03 §1.2/§4.3) — T08-01.
//!
//! The FTS5 virtual table `fts_occurrences` (spec 03 §4.3, DDL in
//! [`super::open`]) uses SQLite's built-in `unicode61` tokenizer, which only
//! splits on Unicode word boundaries and folds diacritics — it has **no**
//! notion of `camelCase`/`snake_case`/`kebab-case` word boundaries, and by
//! default treats `_` as a token character (not a separator), so
//! `extract_imports` would index as a single opaque token. [`tokenize_identifier`]
//! and its column-specific siblings ([`tokenize_path`], [`tokenize_qualified_name`],
//! [`tokenize_signature`]) do that splitting **app-side, before insert** (09 §2)
//! so a search for `extract` or `imports` alone still matches. Insertion itself —
//! the generation materializer that calls these functions and writes rows — is
//! T08-02; this module ships the pure functions plus the deterministic
//! [`fts_manifest_hash`] identity.
//!
//! ## Splitting algorithm
//!
//! Every entry point NFC-normalizes its input first (spec 03 §1.3's Unicode
//! convention, already used by [`super::super::code::normalize`]), then runs a
//! single left-to-right scan over Unicode scalar values, classifying each
//! alphanumeric character as `Upper` (`is_uppercase()`), `Digit`
//! (`is_numeric()`), or `Lower` (everything else alphanumeric — this folds
//! non-cased scripts like CJK into one undifferentiated class, so such runs are
//! never split character-by-character). Any non-alphanumeric character
//! (`!is_alphanumeric()`) is a **hard delimiter**: it ends the current run, is
//! never itself emitted, and consecutive delimiters collapse (no empty parts).
//! This single rule already covers `snake_case`, `kebab-case`, and — reused for
//! the path/qualified-name columns — `/`, `.`, `::`.
//!
//! Within a maximal alphanumeric run, a boundary is inserted between positions
//! `i-1`/`i` when (checking classes `c[i-1]`, `c[i]`, and — for the fourth rule
//! only — `c[i+1]`):
//!
//! 1. **letter→digit**: `c[i-1] != Digit && c[i] == Digit`;
//! 2. **digit→letter**: `c[i-1] == Digit && c[i] != Digit`;
//! 3. **lower→upper**: `c[i-1] == Lower && c[i] == Upper`;
//! 4. **acronym-run end**: `c[i-1] == Upper && c[i] == Upper && c[i+1] == Lower`
//!    (only when `i+1` is in bounds) — an acronym's last uppercase letter joins
//!    the following lowercase word (`HTTPServer` → `HTTP`, `Server`; a bare
//!    trailing acronym like `parseXML` or a whole-atom `HTTP` stays unsplit,
//!    since rule 4 needs a lowercase letter *after* the run).
//!
//! `[SPEC — digit-boundary splitting is not spec-mandated]`: splitting at
//! letter↔digit transitions in both directions is a deliberate choice (not an
//! oversight) for recall parity with common subword conventions (`sha256` →
//! `sha`, `256`, while the fused `sha256` token is still retained — see below).
//!
//! Splitting runs on the **original casing** — lowering first would destroy the
//! very `Lower`/`Upper` signal the boundary rules depend on. Each resulting
//! part is folded to lowercase independently via [`casefold::simple_fold`]
//! `[SPEC]` (the same primitive [`local_rag_core::identity::path`] already uses
//! for case-insensitive path identity), not `str::to_lowercase()`: `simple_fold`
//! is a strict one-codepoint-to-one-codepoint fold, whereas full Unicode
//! lowering can expand length (e.g. Turkish `İ` → `i̇`, two codepoints) — a
//! surprising token-count change this codebase's other Unicode-handling
//! (NFC + simple fold) already avoids.
//!
//! **Known limitation, documented not silently glossed**: Unicode Mark
//! characters that survive NFC (e.g. Arabic combining diacritics, unlike most
//! precomposed Latin/Cyrillic/Greek) are not `is_alphanumeric()` and are
//! treated as hard delimiters, which may over-split such scripts. Accepted for
//! v0's code-identifier scope.
//!
//! ## The "fused whole atom" rule
//!
//! An atom's *raw* split — hard delimiters only, ignoring case/digit
//! sub-boundaries — determines whether the whole atom is **also** emitted as
//! one extra token, lowered: only when that raw split has exactly one piece
//! (the atom contains no punctuation/`_`/`-` at all — e.g. `parseHTML2Response`,
//! `HTTPServer`, `café`, `foo`). When the raw split has more than one piece
//! (`snake_case_name`, `kebab-case`), the fused form is **not** emitted.
//!
//! This is FTS5-mechanics-driven, not arbitrary: `unicode61` already treats
//! `_`/`-`/`.`/`:`/`/` as separators by default, so re-emitting a punctuated
//! string as a single app-side "fused" token is a no-op at the index level (it
//! decomposes into the exact same words the split parts already produce) and
//! would **double-count term frequency** for every such term — a real
//! BM25-skewing bug. For a punctuation-free camelCase atom, FTS5 has nothing to
//! split on, so the fused form is a genuinely distinct, useful index term (an
//! exact match on `parsehtml2response`).
//!
//! All tokens (fused-if-applicable + split parts, each lowered) are
//! deduplicated preserving first-occurrence order before space-joining — this
//! is what keeps a plain word like `foo` from emitting `"foo foo"`.
//!
//! ## Path and qualified-name columns: split into components *before* fusing
//!
//! [`tokenize_path`] and [`tokenize_qualified_name`] first split their input
//! into components (on `/`, or on runs of `.`/`:` respectively), then run the
//! full atom pipeline above **independently on each component**. This is not
//! equivalent to just handing the whole path/qualified-name string to
//! [`tokenize_identifier`]: doing so would make the *whole-string* fusion
//! decision see the `/`/`.` punctuation and suppress fusion everywhere,
//! silently losing a useful fused token for a punctuation-free component (e.g.
//! `barBaz` in `src/foo/barBaz.rs` would never get its own `barbaz` token).
//! Splitting into components first lets each one make its own fusion decision.
//!
//! Accepted simplification: a path component is not further split into
//! "file stem" vs. "extension" — `barBaz.rs` as one `/`-component still yields
//! `bar`, `baz`, `rs` (via the component's own internal `.` hard-delimiter), but
//! no separate fused `barbaz` token (the component itself contains punctuation).
//!
//! ## Signature tokens
//!
//! [`tokenize_signature`] takes already-extracted fragments (e.g. a raw
//! parameter-list/return-type string) — it does not know or care where they
//! came from. Plumbing real signature text out of the tree-sitter parser
//! adapters (`crates/index`, which today hash such text straight into the
//! opaque `signature_fingerprint` and discard the raw string) is explicitly
//! **out of scope for this task** (T08-02+). A whole fragment is never fused —
//! only its split parts are emitted, since a raw fragment like
//! `"(name: String)"` is not itself a meaningful search term.

use local_rag_core::identity::Domain;
use local_rag_core::identity::domain;
use unicode_normalization::UnicodeNormalization;

/// `fts_projection_head.lexical_schema_version` (spec 03 §4.3) — bump when the
/// `fts_doc`/`fts_occurrences`/`fts_projection_head` DDL shape changes in a way
/// that invalidates an existing FTS view (06 §4).
pub const LEXICAL_SCHEMA_VERSION: u32 = 1;

/// `fts_projection_head.tokenizer_version` (spec 09 §2) — bump whenever the
/// splitting/folding/token-emission rules in this module change in a way that
/// would produce different tokens for the same input. Any bump invalidates
/// every worktree's FTS head (06 §4) and forces a rebuild (T08-02/T08-03).
pub const TOKENIZER_VERSION: u32 = 1;

/// A character's class for boundary detection (module docs above).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Upper,
    Lower,
    Digit,
}

fn classify(c: char) -> Option<CharClass> {
    if c.is_numeric() {
        Some(CharClass::Digit)
    } else if c.is_uppercase() {
        Some(CharClass::Upper)
    } else if c.is_alphanumeric() {
        Some(CharClass::Lower)
    } else {
        None
    }
}

/// Split `text` into maximal alphanumeric runs (hard-delimiter split only,
/// case/digit-agnostic). Used both to decide fusion eligibility and as the
/// outer component splitter for `path`/`qualified_name`.
fn hard_delimiter_runs(text: &str) -> Vec<&str> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    let mut last_end = 0;
    for (i, c) in text.char_indices() {
        if classify(c).is_some() {
            if start.is_none() {
                start = Some(i);
            }
            last_end = i + c.len_utf8();
        } else if let Some(s) = start.take() {
            runs.push(&text[s..last_end]);
        }
    }
    if let Some(s) = start {
        runs.push(&text[s..last_end]);
    }
    runs
}

/// Split one hard-delimiter run into camelCase/snake_case-boundary parts
/// (module docs above, rules 1-4). `run` must already be a maximal
/// alphanumeric run (no delimiters inside its class sequence, but the classifier
/// itself still governs boundary placement).
fn split_case_boundaries(run: &str) -> Vec<&str> {
    let chars: Vec<(usize, char, CharClass)> = run
        .char_indices()
        .map(|(i, c)| {
            (
                i,
                c,
                classify(c).expect("hard_delimiter_runs yields alphanumeric-only runs"),
            )
        })
        .collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut parts = Vec::new();
    let mut part_start = chars[0].0;
    for i in 1..chars.len() {
        let (idx, _, cls) = chars[i];
        let (_, _, prev_cls) = chars[i - 1];
        let next_cls = chars.get(i + 1).map(|&(_, _, c)| c);
        let boundary = match (prev_cls, cls) {
            (CharClass::Digit, CharClass::Digit) => false,
            (prev, CharClass::Digit) if prev != CharClass::Digit => true, // 1: letter→digit
            (CharClass::Digit, cur) if cur != CharClass::Digit => true,   // 2: digit→letter
            (CharClass::Lower, CharClass::Upper) => true,                 // 3: lower→upper
            (CharClass::Upper, CharClass::Upper) => next_cls == Some(CharClass::Lower), // 4
            _ => false,
        };
        if boundary {
            parts.push(&run[part_start..idx]);
            part_start = idx;
        }
    }
    parts.push(&run[part_start..]);
    parts
}

/// Fold `s` to lowercase via [`casefold::simple_fold`] (module docs above).
fn fold_lower(s: &str) -> String {
    casefold::simple_fold(s.to_string())
}

/// Push `tok` onto `tokens` unless it is empty or already present (preserves
/// first-occurrence order — the "fused + parts, deduplicated" rule).
fn push_unique(tokens: &mut Vec<String>, tok: String) {
    if !tok.is_empty() && !tokens.contains(&tok) {
        tokens.push(tok);
    }
}

/// Tokenize one identifier-shaped atom: NFC-normalize, decide fusion
/// eligibility from the raw hard-delimiter split, then emit
/// fused-if-applicable + every case-boundary sub-part, each lowered and
/// deduplicated in first-occurrence order.
fn tokenize_atom(atom: &str) -> Vec<String> {
    if atom.is_empty() {
        return Vec::new();
    }
    let nfc: String = atom.nfc().collect();
    let raw_runs = hard_delimiter_runs(&nfc);

    let mut tokens: Vec<String> = Vec::new();

    if raw_runs.len() == 1 {
        // No internal punctuation: the whole atom is a real, distinct FTS5
        // term (see module docs "fused whole atom" rule).
        push_unique(&mut tokens, fold_lower(&nfc));
    }
    for run in &raw_runs {
        for part in split_case_boundaries(run) {
            push_unique(&mut tokens, fold_lower(part));
        }
    }
    tokens
}

/// Tokenize `identifier` for the FTS5 `name` column (spec 09 §2): original +
/// camelCase/snake_case/kebab-case parts, lowercased, space-joined. See the
/// module docs for the exact splitting/fusion rules.
pub fn tokenize_identifier(identifier: &str) -> String {
    tokenize_atom(identifier).join(" ")
}

/// Tokenize `path` for the FTS5 `path` column (spec 09 §2): split on `/` into
/// non-empty components, each tokenized independently (own fusion decision;
/// see module docs), concatenated in order, space-joined.
pub fn tokenize_path(path: &str) -> String {
    let mut tokens: Vec<String> = Vec::new();
    for component in path.split('/').filter(|c| !c.is_empty()) {
        tokens.extend(tokenize_atom(component));
    }
    tokens.join(" ")
}

/// Tokenize `qualified_name` for the FTS5 `qualified_name` column (spec 09 §2):
/// `None`/empty → `""` (no v2 caller derives a qualified name yet — 06 §2
/// as-built note). `Some(q)` splits `q` on runs of `.`/`:` (covers both
/// dotted `a.b.c` and Rust-style `a::b::c`), each component tokenized
/// independently, concatenated in order, space-joined.
pub fn tokenize_qualified_name(qualified_name: Option<&str>) -> String {
    let Some(q) = qualified_name else {
        return String::new();
    };
    let mut tokens: Vec<String> = Vec::new();
    for component in q.split(['.', ':']).filter(|c| !c.is_empty()) {
        tokens.extend(tokenize_atom(component));
    }
    tokens.join(" ")
}

/// Tokenize already-extracted signature fragments (e.g. a raw parameter-list or
/// return-type string) for the FTS5 `signature` column (spec 09 §2). Each
/// fragment contributes only its case-boundary sub-parts — a whole fragment is
/// never fused, since it is not itself a meaningful search term. Empty
/// fragments are skipped.
pub fn tokenize_signature(fragments: &[&str]) -> String {
    let mut tokens: Vec<String> = Vec::new();
    for fragment in fragments {
        let nfc: String = fragment.nfc().collect();
        for run in hard_delimiter_runs(&nfc) {
            for part in split_case_boundaries(run) {
                let folded = fold_lower(part);
                if !folded.is_empty() && !tokens.contains(&folded) {
                    tokens.push(folded);
                }
            }
        }
    }
    tokens.join(" ")
}

/// Sort `ids` ascending bytewise and de-duplicate (spec 03 §1.2 "occurrence IDs
/// sorted"), mirroring `local_rag_projection::identity::sorted_unique`'s set
/// semantics — this store-side copy exists because `crates/store` cannot
/// depend on `crates/projection` (the dependency runs the other way).
fn sorted_unique<'a>(ids: &[&'a str]) -> Vec<&'a str> {
    let mut ids: Vec<&str> = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// `H(fts_manifest, worktree_id, generation_id, occurrence IDs sorted ascending
/// bytewise + de-duplicated)` — spec 03 §1.2 / §4.3 `fts_projection_head.
/// manifest_hash`. Unlike `projection::identity::manifest_hash`, there is no
/// `model_space_id` axis: the FTS view is generation-scoped only.
pub fn fts_manifest_hash(
    worktree_id: &str,
    generation_id: &str,
    occurrence_ids: &[&str],
) -> String {
    let ids = sorted_unique(occurrence_ids);
    let mut fields: Vec<&[u8]> = Vec::with_capacity(2 + ids.len());
    fields.push(worktree_id.as_bytes());
    fields.push(generation_id.as_bytes());
    fields.extend(ids.iter().map(|id| id.as_bytes()));
    domain::hash(Domain::FtsManifest, &fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── tokenize_identifier ──────────────────────────────────────────────

    #[test]
    fn tokenize_identifier_golden_table() {
        let cases: &[(&str, &str)] = &[
            ("", ""),
            ("foo", "foo"),
            ("camelCase", "camelcase camel case"),
            ("PascalCase", "pascalcase pascal case"),
            ("HTTPServer", "httpserver http server"),
            (
                "parseHTML2Response",
                "parsehtml2response parse html 2 response",
            ),
            ("getIDValue", "getidvalue get id value"),
            ("ID", "id"),
            ("snake_case_name", "snake case name"),
            ("kebab-case-name", "kebab case name"),
            ("sha256", "sha256 sha 256"),
            ("v2", "v2 v 2"),
        ];
        for (input, expected) in cases {
            assert_eq!(&tokenize_identifier(input), expected, "input={input:?}");
        }
    }

    #[test]
    fn tokenize_identifier_handles_unicode() {
        // Accented Latin: case-preserving, no crash, whole atom fused (no
        // internal punctuation).
        assert_eq!(tokenize_identifier("café"), "café");
        assert_eq!(tokenize_identifier("CaféBar"), "cafébar café bar");

        // CJK: a run of non-cased characters is one undifferentiated `Lower`
        // class run — never split character-by-character.
        assert_eq!(tokenize_identifier("变量名"), "变量名");

        // Emoji act as hard delimiters without panicking.
        assert_eq!(tokenize_identifier("foo😀bar"), "foo bar");
    }

    #[test]
    fn tokenize_identifier_normalizes_nfc_before_folding() {
        let decomposed = "cafe\u{0301}"; // e + combining acute
        let precomposed = "café";
        assert_eq!(
            tokenize_identifier(decomposed),
            tokenize_identifier(precomposed)
        );
    }

    // ── tokenize_path ────────────────────────────────────────────────────

    #[test]
    fn tokenize_path_golden_table() {
        assert_eq!(tokenize_path(""), "");
        assert_eq!(tokenize_path("foo"), "foo");
        // "barBaz.rs" is one `/`-component containing a `.`, so it is not
        // punctuation-free and gets no fused "barbaz" token (accepted
        // simplification, module docs) — only its internal split parts.
        assert_eq!(tokenize_path("src/foo/barBaz.rs"), "src foo bar baz rs");
        // Leading/trailing/duplicate slashes collapse, no empty components.
        assert_eq!(tokenize_path("/src//foo/"), "src foo");
    }

    // ── tokenize_qualified_name ──────────────────────────────────────────

    #[test]
    fn tokenize_qualified_name_golden_table() {
        assert_eq!(tokenize_qualified_name(None), "");
        assert_eq!(tokenize_qualified_name(Some("")), "");
        assert_eq!(
            tokenize_qualified_name(Some("parser.extractImports")),
            "parser extractimports extract imports"
        );
        assert_eq!(
            tokenize_qualified_name(Some("crate::parser::ExtractImports")),
            "crate parser extractimports extract imports"
        );
    }

    // ── tokenize_signature ───────────────────────────────────────────────

    #[test]
    fn tokenize_signature_golden_table() {
        assert_eq!(tokenize_signature(&[]), "");
        assert_eq!(tokenize_signature(&[""]), "");
        // Signature fragments get no whole-run fusion (unlike
        // tokenize_identifier/path/qualified_name) — "u32" only contributes
        // its split parts "u"/"32", never a combined "u32" token.
        assert_eq!(
            tokenize_signature(&["(name: String, count: u32)", "-> Response"]),
            "name string count u 32 response"
        );
    }

    // ── fts_manifest_hash ────────────────────────────────────────────────

    const OCC_A: &str = "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a";
    const OCC_B: &str = "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b";
    const OCC_C: &str = "0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c";

    #[test]
    fn fts_manifest_hash_is_exact_golden() {
        let wt = "01234567-89ab-7122-b344-5566778899aa";
        let gen_id = "0000000a-0000-7000-8000-00000000000b";
        let ids = [OCC_A, OCC_B, OCC_C];
        let got = fts_manifest_hash(wt, gen_id, &ids);

        // Independent cross-check via a direct domain::hash call.
        let expected = domain::hash(
            Domain::FtsManifest,
            &[
                wt.as_bytes(),
                gen_id.as_bytes(),
                OCC_A.as_bytes(),
                OCC_B.as_bytes(),
                OCC_C.as_bytes(),
            ],
        );
        assert_eq!(got, expected);
        assert_eq!(got.len(), 64);
        assert!(
            got.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn fts_manifest_is_independent_of_order_and_duplicates() {
        let wt = "wt";
        let gen_id = "gen";
        let sorted = [OCC_A, OCC_B, OCC_C];
        let shuffled = [OCC_C, OCC_A, OCC_B, OCC_A]; // shuffled + duplicate
        assert_eq!(
            fts_manifest_hash(wt, gen_id, &sorted),
            fts_manifest_hash(wt, gen_id, &shuffled),
        );
    }

    #[test]
    fn fts_manifest_binds_the_worktree() {
        let gen_id = "gen";
        let ids = [OCC_A];
        let base = fts_manifest_hash("wt-1", gen_id, &ids);
        let other = fts_manifest_hash("wt-2", gen_id, &ids);
        assert_ne!(base, other);
    }

    #[test]
    fn fts_manifest_binds_the_generation() {
        let wt = "wt";
        let ids = [OCC_A];
        let base = fts_manifest_hash(wt, "gen-1", &ids);
        let other = fts_manifest_hash(wt, "gen-2", &ids);
        assert_ne!(base, other);
    }

    #[test]
    fn every_occurrence_id_changes_the_hash() {
        let wt = "wt";
        let gen_id = "gen";
        let base = fts_manifest_hash(wt, gen_id, &[OCC_A, OCC_B]);
        let added = fts_manifest_hash(wt, gen_id, &[OCC_A, OCC_B, OCC_C]);
        let altered = fts_manifest_hash(wt, gen_id, &[OCC_A, OCC_C]);
        assert_ne!(base, added);
        assert_ne!(base, altered);
        assert_ne!(added, altered);
    }
}
