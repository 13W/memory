//! The recall pipeline's lexical (FTS) leg (spec 08 §6) — T14-08.
//!
//! # Ephemeral, not a persisted materialized view
//!
//! Code's FTS view (group 08, `local_rag_store::cache::fts`) is a
//! `cache.sqlite`-persisted, validate-on-open, manifest-hashed structure
//! because it is rebuilt at the frequency of *generation switches* —
//! infrequent, atomic events. Memory mutates through many small,
//! individually-transactional ops (create/edit/retract/supersede/merge,
//! T14-02…05); there is no generation-shaped checkpoint to hang a manifest
//! off, and keeping a persisted `cache.sqlite` index transactionally current
//! with every `state.sqlite` memory op would need a cross-database write per
//! op, which this codebase's architecture forbids (no writable cross-DB
//! `ATTACH`). Spec 08 §6's own `[SPEC ≤ 20k entries]` cardinality guard
//! already bounds the candidate set in memory, so this leg instead builds a
//! **short-lived, in-process** FTS5 table per recall call, seeded from the
//! already-fetched (scope-unioned, guarded) candidate set, and drops it when
//! the call returns. There is nothing to validate on open and nothing that
//! can go stale: the table is always freshly derived from the exact
//! `RecallCandidate`s the caller already read from `state.sqlite`.
//!
//! # Natural-language text, not code identifiers
//!
//! `local_rag_store::cache::fts::tokenize_identifier` and its siblings split
//! on camelCase/snake_case boundaries — tuned for symbol names, a domain
//! memory text is not part of. SQLite's built-in `unicode61` tokenizer
//! already does reasonable natural-language tokenization (word-splitting,
//! case-folding), so this leg uses it directly rather than reusing or
//! reimplementing the code-search splitter.
//!
//! # Query term handling
//!
//! Mirrors `local_rag_store::cache::fts_query`'s established idiom for the
//! *same reason* it was chosen there (spec 09 §2's as-built note): terms are
//! lowercased, split on non-alphanumeric boundaries, quoted individually
//! (`"term"`, embedded `"` doubled — FTS5 reads bare `AND`/`OR`/`NOT`/`NEAR`
//! as operators) and combined with `OR` (recall queries are natural language;
//! requiring every term would return nothing for most of them). No terms
//! survive tokenization ⇒ the leg returns empty **without issuing SQL** — an
//! empty `MATCH` expression is itself a syntax error, the same guard the
//! code-search leg has.

use local_rag_store::RecallCandidate;
use local_rag_store::rusqlite::{self, Connection, params};

/// One lexical-leg hit: the entry, its 1-based rank, and the raw `bm25()`
/// score (diagnostics only — fusion ranks, never scores, on this leg too).
#[derive(Debug, Clone, PartialEq)]
pub struct LexicalRecallHit {
    pub memory_id: String,
    pub rank: usize,
    pub bm25: f64,
}

/// Lowercase, split on non-alphanumeric runs, drop empties. Deliberately not
/// [`local_rag_store::tokenize_identifier`] — see the module doc.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Double every embedded `"` and wrap in quotes, so the term is a total FTS5
/// string literal regardless of its content.
fn quote_fts_string(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// Build an `OR`-combined FTS5 `MATCH` expression from `query`'s terms, or
/// `None` if none survive tokenization.
fn match_expression(query: &str) -> Option<String> {
    let terms = tokenize(query);
    if terms.is_empty() {
        return None;
    }
    Some(
        terms
            .iter()
            .map(|t| quote_fts_string(t))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

/// Run the lexical leg over `candidates` for `query`, ranked best-first
/// (`bm25 ASC` — SQLite's `bm25()` is more negative for a better match),
/// `memory_id ASC` as the deterministic tie-break, cut at `limit`.
///
/// An empty/termless `query` or an empty `candidates` returns `Ok(vec![])`
/// without touching SQLite at all — a termless query is healthy, not a
/// failure (the same treatment the code-search lexical leg gives it).
pub fn lexical_leg(
    query: &str,
    candidates: &[RecallCandidate],
    limit: usize,
) -> rusqlite::Result<Vec<LexicalRecallHit>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let Some(expr) = match_expression(query) else {
        return Ok(Vec::new());
    };

    let conn = Connection::open_in_memory()?;
    conn.execute_batch(
        "CREATE VIRTUAL TABLE recall_fts USING fts5(memory_id UNINDEXED, body, tokenize = 'unicode61');",
    )?;
    {
        let mut stmt = conn.prepare("INSERT INTO recall_fts (memory_id, body) VALUES (?1, ?2)")?;
        for candidate in candidates {
            stmt.execute(params![candidate.memory_id, candidate.text])?;
        }
    }

    let sql = "SELECT memory_id, bm25(recall_fts) AS rank FROM recall_fts \
               WHERE recall_fts MATCH ?1 ORDER BY rank ASC, memory_id ASC LIMIT ?2";
    let mut stmt = conn.prepare(sql)?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = stmt
        .query_map(params![expr, limit], |r| {
            let memory_id: String = r.get(0)?;
            let bm25: f64 = r.get(1)?;
            Ok((memory_id, bm25))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(rows
        .into_iter()
        .enumerate()
        .map(|(i, (memory_id, bm25))| LexicalRecallHit {
            memory_id,
            rank: i + 1,
            bm25,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(memory_id: &str, text: &str) -> RecallCandidate {
        RecallCandidate {
            memory_id: memory_id.to_string(),
            kind: local_rag_store::MemoryKind::Fact,
            state: local_rag_store::MemoryState::Active,
            text: text.to_string(),
            confidence: 0.5,
            created_at: 1000,
        }
    }

    #[test]
    fn ranks_a_matching_entry_above_a_non_matching_one() {
        let candidates = vec![
            candidate("a", "we decided to use JWT for authentication"),
            candidate("b", "tests live under the __tests__ directory"),
        ];
        let hits = lexical_leg("JWT authentication", &candidates, 10).expect("query");
        assert_eq!(hits[0].memory_id, "a");
        assert_eq!(hits[0].rank, 1);
    }

    #[test]
    fn empty_query_returns_empty_without_touching_sqlite() {
        let candidates = vec![candidate("a", "some text")];
        assert_eq!(lexical_leg("", &candidates, 10).expect("query"), vec![]);
        assert_eq!(lexical_leg("   ", &candidates, 10).expect("query"), vec![]);
    }

    #[test]
    fn empty_candidates_returns_empty() {
        assert_eq!(lexical_leg("query", &[], 10).expect("query"), vec![]);
    }

    #[test]
    fn a_query_with_only_operator_words_does_not_error() {
        // Bare AND/OR/NOT are FTS5 operators; quoting must make this total.
        let candidates = vec![candidate("a", "and or not are common english words")];
        let hits =
            lexical_leg("and or not", &candidates, 10).expect("query must not be a syntax error");
        assert_eq!(hits[0].memory_id, "a");
    }

    #[test]
    fn a_query_with_embedded_quotes_does_not_error() {
        let candidates = vec![candidate("a", "quoted text")];
        let hits = lexical_leg("\"quoted\"", &candidates, 10).expect("query");
        assert_eq!(hits[0].memory_id, "a");
    }

    #[test]
    fn limit_bounds_the_result() {
        let candidates: Vec<RecallCandidate> = (0..5)
            .map(|i| candidate(&format!("m{i}"), "shared term appears in every entry"))
            .collect();
        let hits = lexical_leg("shared", &candidates, 2).expect("query");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn deterministic_tie_break_by_memory_id() {
        let candidates = vec![
            candidate("c", "identical text"),
            candidate("a", "identical text"),
            candidate("b", "identical text"),
        ];
        let hits = lexical_leg("identical text", &candidates, 10).expect("query");
        let ids: Vec<&str> = hits.iter().map(|h| h.memory_id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }
}
