//! The lexical leg's read side: code-aware query preprocessing, the BM25
//! ranking defaults, the `name_pattern` prefix filter and the per-leg candidate
//! depth (spec 09 §1/§2/§4) — T12-01.
//!
//! [`super::fts`] owns the *write* side of the same view (tokenize → insert →
//! head-last). This module owns the *read* side, and lives next to it on
//! purpose: a query is only findable when it is tokenized by exactly the same
//! rules the row was indexed with, so both directions call
//! [`tokenize_identifier`](super::fts::tokenize_identifier) and can never drift
//! apart in separate crates.
//!
//! ## Query preprocessing
//!
//! The raw query string goes through `tokenize_identifier` — the same
//! NFC → hard-delimiter split → camelCase/snake_case/kebab-case split →
//! `simple_fold` → dedup pipeline that produced the indexed terms (spec 09 §2).
//! A natural-language query (`"embed batch of text strings"`) therefore yields
//! `embed batch of text strings`, and an identifier query (`"extractImports"`)
//! yields `extractimports extract imports` — the fused form included, exactly as
//! the `name` column stores it.
//!
//! **Known, accepted asymmetry `[SPEC]`**: `tokenize_path`/
//! `tokenize_qualified_name` make the fused-whole-atom decision *per component*
//! at index time, while a query string is tokenized as one atom. So the query
//! `src/foo/barBaz.rs` emits `src foo bar baz rs` but not the fused `barbaz`
//! the `path` column also holds. Recall is unaffected (the `bar`/`baz` parts
//! still match that row); only the exact fused term is not requested. Splitting
//! a free-text query on `/` and `.` to recover it would misfire on ordinary
//! prose ("v2.1", "and/or"), which is the far more common input.
//!
//! ## Matching semantics `[SPEC]`
//!
//! Query tokens are combined with **`OR`**, not `AND`. The 49-query benchmark
//! corpus (`fixtures/search/corpus.json`, spec 14 §7) is natural-language
//! (`"call Ollama embed API and parse embeddings response"`); requiring every
//! token would return nothing for almost every query, whereas BM25 already
//! ranks a document matching more (and rarer — IDF) terms above one matching
//! fewer. Spec 09 §2 fixes the ranking function and its weights, not the
//! boolean shape, so this is a `[SPEC]` choice tunable by T12-05 alongside the
//! weights.
//!
//! Every token is emitted as a **quoted FTS5 string** (`"embed"`), with any
//! embedded `"` doubled. This is not cosmetic: FTS5 reads bare `AND`/`OR`/
//! `NOT`/`NEAR` as operators, so an unquoted query containing the English word
//! "and" would be a syntax error (`SQLITE_ERROR`) rather than a search. Quoting
//! makes the expression total over arbitrary user input.
//!
//! ## `name_pattern` `[SPEC]`
//!
//! Spec 09 §1's "prefix-tokenized on `local_name`/`qualified_name`" is realized
//! as an FTS5 column filter over exactly those two columns, with each pattern
//! token as a prefix term, `AND`-ed together:
//!
//! ```text
//! {name qualified_name} : ("extract"* AND "impo"*)
//! ```
//!
//! `AND` (not `OR`) because a filter must *narrow*: `name_pattern="extractImp"`
//! asks for names beginning like that, so every part of the pattern has to be
//! present. Prefix semantics apply per token, which is what makes a partially
//! typed pattern work (`impo*` matches the indexed `imports`).
//!
//! An empty/whitespace-only `name_pattern` tokenizes to nothing and is treated
//! as **no filter** rather than "match nothing" — the reading a caller passing
//! an empty box almost certainly intends.

use rusqlite::{Connection, params};

use super::fts::tokenize_identifier;

/// Default `bm25(fts_occurrences, …)` column weights in declaration order —
/// `name, qualified_name, path, signature, body` (spec 09 §2, DDL in spec
/// 03 §4.3).
///
/// `[SPEC — tuned by the 49-query benchmark]`: these are the spec's stated
/// defaults, not a measured optimum. T12-05 owns retuning them, and any change
/// there is a versioned tuning change, not an edit in passing.
///
/// **The `signature` weight is currently inert on real data**: the generation
/// materializer writes `tokenize_signature(&[])` for every row, because raw
/// parameter/return-type text is not yet plumbed out of the tree-sitter
/// adapters (spec 09 §2 / 06 §4 as-built notes, T08-02). The weight is honored
/// by the ranking query — proven on directly seeded rows in
/// `crates/store/tests/fts_query.rs` — and starts affecting production ranking
/// for free once that column is populated.
pub const BM25_DEFAULT_WEIGHTS: [f64; 5] = [4.0, 3.0, 1.5, 2.0, 1.0];

/// The floor of spec 09 §4's per-leg candidate depth `max(limit·4, 50)`.
pub const MIN_CANDIDATE_DEPTH: usize = 50;

/// Spec 09 §4's per-leg candidate depth: `max(limit·4, 50)`.
///
/// Defined here, with the lexical leg, because it is the first caller; the
/// dense leg and RRF (T12-02/T12-03) take the *same* depth from this one
/// function rather than restating the formula — the two legs must fuse over
/// comparably deep candidate lists for RRF ranks to mean anything.
pub fn candidate_depth(limit: usize) -> usize {
    limit.saturating_mul(4).max(MIN_CANDIDATE_DEPTH)
}

/// One lexical-leg candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct LexicalHit {
    /// The matching occurrence (spec 03 §1.2), from `fts_doc`.
    pub occurrence_id: String,
    /// 1-based position in this leg's result order — the `rank_leg(d)` RRF
    /// consumes (spec 09 §4).
    pub rank: usize,
    /// The raw `bm25()` value. SQLite's `bm25()` is **more negative for a
    /// better match**, so this decreases as `rank` increases. Kept for
    /// diagnostics; fusion uses `rank`, never this.
    pub bm25: f64,
}

/// One lexical-leg request.
#[derive(Debug, Clone, PartialEq)]
pub struct LexicalQuery<'a> {
    /// The raw user query, tokenized here (never passed to FTS5 verbatim).
    pub query: &'a str,
    /// Optional prefix filter on `local_name`/`qualified_name` (spec 09 §1).
    pub name_pattern: Option<&'a str>,
    /// The caller's requested result count; candidate depth is derived from it
    /// via [`candidate_depth`].
    pub limit: usize,
    /// BM25 column weights — normally [`BM25_DEFAULT_WEIGHTS`].
    pub weights: [f64; 5],
}

impl<'a> LexicalQuery<'a> {
    /// A query with the spec's default weights.
    pub fn new(query: &'a str, name_pattern: Option<&'a str>, limit: usize) -> Self {
        Self {
            query,
            name_pattern,
            limit,
            weights: BM25_DEFAULT_WEIGHTS,
        }
    }
}

/// Wrap `token` as an FTS5 string literal, doubling any embedded `"`.
fn quote_fts_string(token: &str) -> String {
    let mut quoted = String::with_capacity(token.len() + 2);
    quoted.push('"');
    for c in token.chars() {
        if c == '"' {
            quoted.push('"');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// The indexed terms `text` reduces to, in first-occurrence order.
///
/// [`tokenize_identifier`] returns them space-joined and no token can itself
/// contain whitespace (whitespace is a hard delimiter), so splitting the joined
/// form back apart is lossless — and keeps this module using the *exact* same
/// entry point the materializer does, with no second code path to drift.
fn terms(text: &str) -> Vec<String> {
    tokenize_identifier(text)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Build the FTS5 `MATCH` expression for `query` (spec 09 §1/§2).
///
/// Returns `None` when neither the query nor the pattern yields a single term —
/// there is nothing to match, and the caller must skip the SQL entirely rather
/// than send FTS5 an empty expression (which is a syntax error).
///
/// Shapes (see the module docs for the reasoning):
///
/// ```text
/// query only   ("embed" OR "batch")
/// filter only  ({name qualified_name} : ("extract"*))
/// both         ("embed" OR "batch") AND ({name qualified_name} : ("extract"*))
/// ```
pub fn fts_match_expression(query: &str, name_pattern: Option<&str>) -> Option<String> {
    let query_terms = terms(query);
    let pattern_terms = name_pattern.map(terms).unwrap_or_default();

    let query_expr = (!query_terms.is_empty()).then(|| {
        let ored: Vec<String> = query_terms.iter().map(|t| quote_fts_string(t)).collect();
        format!("({})", ored.join(" OR "))
    });
    let filter_expr = (!pattern_terms.is_empty()).then(|| {
        let anded: Vec<String> = pattern_terms
            .iter()
            .map(|t| format!("{}*", quote_fts_string(t)))
            .collect();
        format!("({{name qualified_name}} : ({}))", anded.join(" AND "))
    });

    match (query_expr, filter_expr) {
        (None, None) => None,
        (Some(q), None) => Some(q),
        (None, Some(f)) => Some(f),
        (Some(q), Some(f)) => Some(format!("{q} AND {f}")),
    }
}

/// Run one already-built `match_expr` against the FTS view, scoped to
/// `(worktree_id, generation_id)`, returning at most `depth` hits best-first.
///
/// The `generation_id` predicate is defence in depth, not the primary
/// staleness control: the caller must already have validated
/// `fts_projection_head` (spec 06 §4) and must not call this at all for an
/// invalid view. But because `fts_doc` carries each row's own generation, this
/// predicate makes it *structurally* impossible to serve another generation's
/// occurrences even if a stale head somehow passed validation — spec 06 §3's
/// "the read lock prevents mixing" guarantee, restated in SQL.
///
/// Ordering is `bm25 ASC` (SQLite's `bm25()` is more negative for a better
/// match) with `occurrence_id ASC` as the tie-break — the same deterministic
/// tie-break spec 09 §4 fixes for fusion, applied here so that a truncation at
/// `depth` is reproducible rather than dependent on storage order.
pub fn query_fts(
    conn: &Connection,
    worktree_id: &str,
    generation_id: &str,
    match_expr: &str,
    weights: [f64; 5],
    depth: usize,
) -> rusqlite::Result<Vec<LexicalHit>> {
    let mut stmt = conn.prepare(
        "SELECT fts_doc.occurrence_id, \
                bm25(fts_occurrences, ?2, ?3, ?4, ?5, ?6) AS rank \
         FROM fts_occurrences JOIN fts_doc ON fts_doc.fts_rowid = fts_occurrences.rowid \
         WHERE fts_occurrences MATCH ?1 \
           AND fts_doc.worktree_id = ?7 \
           AND fts_doc.generation_id = ?8 \
         ORDER BY rank ASC, fts_doc.occurrence_id ASC \
         LIMIT ?9",
    )?;
    let rows = stmt
        .query_map(
            params![
                match_expr,
                weights[0],
                weights[1],
                weights[2],
                weights[3],
                weights[4],
                worktree_id,
                generation_id,
                i64::try_from(depth).unwrap_or(i64::MAX),
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .enumerate()
        .map(|(i, (occurrence_id, bm25))| LexicalHit {
            occurrence_id,
            rank: i + 1,
            bm25,
        })
        .collect())
}

/// The lexical leg of spec 09 §1: preprocess, filter, rank, truncate.
///
/// The single entry point the search pipeline calls. When the query and pattern
/// together yield no terms this returns an empty `Vec` **without touching
/// SQLite** — an empty result, not an error and not a full-table scan.
///
/// The caller is responsible for only invoking this on a validated view (spec
/// 06 §4): an invalid/stale `fts_projection_head` means the lexical leg does
/// not run at all and the response is explicitly `dense_only`, never a silently
/// empty lexical result `[FIXED]`.
pub fn lexical_leg(
    conn: &Connection,
    worktree_id: &str,
    generation_id: &str,
    query: &LexicalQuery<'_>,
) -> rusqlite::Result<Vec<LexicalHit>> {
    let Some(match_expr) = fts_match_expression(query.query, query.name_pattern) else {
        return Ok(Vec::new());
    };
    query_fts(
        conn,
        worktree_id,
        generation_id,
        &match_expr,
        query.weights,
        candidate_depth(query.limit),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- candidate depth (spec 09 §4) ---------------------------------------

    #[test]
    fn candidate_depth_is_limit_times_four_floored_at_fifty() {
        assert_eq!(candidate_depth(0), 50);
        assert_eq!(candidate_depth(1), 50);
        assert_eq!(candidate_depth(12), 50, "48 < 50, floor wins");
        assert_eq!(candidate_depth(13), 52, "52 > 50, limit·4 wins");
        assert_eq!(candidate_depth(100), 400);
    }

    #[test]
    fn candidate_depth_saturates_instead_of_overflowing() {
        assert_eq!(candidate_depth(usize::MAX), usize::MAX);
    }

    // ---- match expression goldens (spec 09 §1/§2) ---------------------------

    /// A natural-language query: one term per word, `OR`-combined, no fused
    /// whole-string token (the raw split has more than one piece).
    #[test]
    fn natural_language_query_ors_its_words() {
        assert_eq!(
            fts_match_expression("embed batch of text strings", None).as_deref(),
            Some(r#"("embed" OR "batch" OR "of" OR "text" OR "strings")"#)
        );
    }

    #[test]
    fn camel_case_query_keeps_the_fused_term_and_its_parts() {
        assert_eq!(
            fts_match_expression("extractImports", None).as_deref(),
            Some(r#"("extractimports" OR "extract" OR "imports")"#)
        );
    }

    #[test]
    fn snake_and_kebab_queries_split_on_their_delimiters() {
        assert_eq!(
            fts_match_expression("extract_imports", None).as_deref(),
            Some(r#"("extract" OR "imports")"#)
        );
        assert_eq!(
            fts_match_expression("extract-imports", None).as_deref(),
            Some(r#"("extract" OR "imports")"#)
        );
    }

    /// The documented path asymmetry: components split, but no fused `barbaz`
    /// (that decision is made per component only at index time).
    #[test]
    fn path_shaped_query_splits_into_components_without_fusing_them() {
        assert_eq!(
            fts_match_expression("src/foo/barBaz.rs", None).as_deref(),
            Some(r#"("src" OR "foo" OR "bar" OR "baz" OR "rs")"#)
        );
    }

    #[test]
    fn acronym_and_digit_boundaries_match_the_indexing_rules() {
        assert_eq!(
            fts_match_expression("parseHTML2Response", None).as_deref(),
            Some(r#"("parsehtml2response" OR "parse" OR "html" OR "2" OR "response")"#)
        );
    }

    /// FTS5 keywords must arrive quoted, or the expression is a syntax error
    /// rather than a search. (That it really parses is proven end-to-end in
    /// `crates/store/tests/fts_query.rs`.)
    #[test]
    fn fts5_operator_keywords_are_quoted_not_operators() {
        assert_eq!(
            fts_match_expression("and or not near", None).as_deref(),
            Some(r#"("and" OR "or" OR "not" OR "near")"#)
        );
    }

    /// A `"` in the query cannot terminate the string literal early.
    #[test]
    fn embedded_quotes_are_doubled() {
        // `"` is a hard delimiter, so it never survives into a token from
        // `tokenize_identifier`; quote the term directly to prove the escaper.
        assert_eq!(quote_fts_string(r#"ex"tract"#), r#""ex""tract""#);
        // …and the tokenizer's own handling: the quote splits the atom.
        assert_eq!(
            fts_match_expression(r#"ex"tract"#, None).as_deref(),
            Some(r#"("ex" OR "tract")"#)
        );
    }

    #[test]
    fn unicode_query_is_nfc_folded_like_the_index() {
        assert_eq!(
            fts_match_expression("Café", None).as_deref(),
            Some(r#"("café")"#)
        );
    }

    #[test]
    fn empty_and_punctuation_only_queries_yield_no_expression() {
        assert_eq!(fts_match_expression("", None), None);
        assert_eq!(fts_match_expression("   \t\n ", None), None);
        assert_eq!(fts_match_expression("--- ///", None), None);
    }

    // ---- name_pattern goldens (spec 09 §1) ----------------------------------

    #[test]
    fn single_token_pattern_becomes_a_column_scoped_prefix() {
        assert_eq!(
            fts_match_expression("", Some("extract")).as_deref(),
            Some(r#"({name qualified_name} : ("extract"*))"#)
        );
    }

    #[test]
    fn multi_token_pattern_ands_every_prefix() {
        assert_eq!(
            fts_match_expression("", Some("extractImp")).as_deref(),
            Some(r#"({name qualified_name} : ("extractimp"* AND "extract"* AND "imp"*))"#)
        );
    }

    #[test]
    fn empty_pattern_is_no_filter_not_an_impossible_one() {
        assert_eq!(
            fts_match_expression("parse", Some("")).as_deref(),
            Some(r#"("parse")"#)
        );
        assert_eq!(
            fts_match_expression("parse", Some("   ")).as_deref(),
            Some(r#"("parse")"#)
        );
        assert_eq!(fts_match_expression("", Some("")), None);
    }

    #[test]
    fn query_and_pattern_combine_with_and() {
        assert_eq!(
            fts_match_expression("parse imports", Some("extract")).as_deref(),
            Some(r#"("parse" OR "imports") AND ({name qualified_name} : ("extract"*))"#)
        );
    }

    #[test]
    fn lexical_query_new_uses_the_spec_default_weights() {
        let q = LexicalQuery::new("parse", None, 10);
        assert_eq!(q.weights, [4.0, 3.0, 1.5, 2.0, 1.0]);
        assert_eq!(q.limit, 10);
        assert_eq!(q.name_pattern, None);
    }
}
