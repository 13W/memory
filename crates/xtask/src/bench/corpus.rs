//! The 49-query benchmark corpus (`fixtures/search/corpus.json`, spec 14 §7) —
//! T12-05.
//!
//! The corpus is `[FIXED]` input, imported implementation-neutrally by T00-01
//! and kept **verbatim from v1** so the benchmark stays comparable ("baseline on
//! v1, gate on v2"). Nothing here may normalize, reorder or repair it: a corpus
//! that drifts silently makes every recorded number incomparable to every other.
//! [`Corpus::validate`] therefore refuses a corpus that does not match what the
//! recorded baseline was measured against, rather than adapting to it.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use serde::Deserialize;

/// How many queries the corpus must hold (spec 14 §7's "49-query benchmark").
pub const EXPECTED_QUERY_COUNT: usize = 49;

/// The corpus document.
#[derive(Debug, Clone, Deserialize)]
pub struct Corpus {
    /// Fixture family; always `search`.
    pub family: String,
    /// Corpus version (semver-ish string).
    pub version: String,
    /// How relevance judgments are shaped.
    pub relevance: Relevance,
    /// The metric names the corpus is scored with.
    pub metrics: Vec<String>,
    /// The queries themselves, in corpus order.
    pub queries: Vec<Query>,
}

/// The corpus's relevance model.
#[derive(Debug, Clone, Deserialize)]
pub struct Relevance {
    /// `single-relevant` — exactly one ground-truth target per query.
    pub kind: String,
    /// Whether judgments carry a grade (they do not).
    pub graded: bool,
    /// How many judgments each query has (1).
    pub judgments_per_query: u32,
}

/// One benchmark query and its single ground-truth target.
#[derive(Debug, Clone, Deserialize)]
pub struct Query {
    /// Stable query id (`sc-01`…).
    pub id: String,
    /// Grouping label, for reporting only.
    pub group: String,
    /// The natural-language query text, verbatim from v1.
    pub query: String,
    /// The single relevant target.
    pub expected: Expected,
}

/// The ground-truth target of a query.
#[derive(Debug, Clone, Deserialize)]
pub struct Expected {
    /// Substring the result's path must contain.
    pub file: String,
    /// Substring the result's symbol name must contain. `None` means a
    /// **file-level** match: any symbol of the right file counts.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Matching mode; always `substring` in this corpus.
    #[serde(rename = "match")]
    pub match_mode: String,
}

/// Why a corpus was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorpusError {
    /// The file could not be read or parsed.
    Load(String),
    /// The corpus is not the 49-query one the baseline was measured against.
    QueryCount {
        /// How many queries were found.
        found: usize,
    },
    /// Two queries share an id, so per-query results could not be keyed.
    DuplicateId(String),
    /// A query's ground truth is unusable.
    EmptyTarget(String),
    /// A matching mode this runner does not implement.
    UnsupportedMatchMode {
        /// The offending query.
        id: String,
        /// The mode it asked for.
        mode: String,
    },
    /// The relevance model is not the single-relevant one the metrics assume.
    UnexpectedRelevance(String),
    /// The corpus is not the `search` family, or does not declare the metrics
    /// this runner computes.
    UnexpectedShape(String),
}

impl fmt::Display for CorpusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CorpusError::Load(e) => write!(f, "corpus could not be loaded: {e}"),
            CorpusError::QueryCount { found } => write!(
                f,
                "corpus holds {found} queries, expected {EXPECTED_QUERY_COUNT} \
                 (the baseline was measured on the 49-query corpus)"
            ),
            CorpusError::DuplicateId(id) => write!(f, "duplicate query id {id:?}"),
            CorpusError::EmptyTarget(id) => {
                write!(f, "query {id:?} has an empty `expected.file`")
            }
            CorpusError::UnsupportedMatchMode { id, mode } => {
                write!(f, "query {id:?} asks for unsupported match mode {mode:?}")
            }
            CorpusError::UnexpectedRelevance(kind) => write!(
                f,
                "corpus relevance is {kind:?}, but the metrics assume `single-relevant`"
            ),
            CorpusError::UnexpectedShape(why) => write!(f, "corpus shape: {why}"),
        }
    }
}

impl std::error::Error for CorpusError {}

impl Corpus {
    /// Load and validate the corpus at `path`.
    pub fn load(path: &Path) -> Result<Self, CorpusError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CorpusError::Load(format!("{path:?}: {e}")))?;
        let corpus: Corpus =
            serde_json::from_str(&text).map_err(|e| CorpusError::Load(e.to_string()))?;
        corpus.validate()?;
        Ok(corpus)
    }

    /// The metric names this runner computes, in the order the corpus declares
    /// them.
    const DECLARED_METRICS: [&'static str; 4] = ["hit@1", "hit@3", "hit@5", "mrr"];

    /// Refuse anything the recorded baseline was not measured against.
    pub fn validate(&self) -> Result<(), CorpusError> {
        if self.family != "search" {
            return Err(CorpusError::UnexpectedShape(format!(
                "family is {:?}, expected \"search\"",
                self.family
            )));
        }
        if self.metrics != Self::DECLARED_METRICS {
            return Err(CorpusError::UnexpectedShape(format!(
                "declared metrics {:?} are not the ones this runner computes ({:?})",
                self.metrics,
                Self::DECLARED_METRICS
            )));
        }
        // A graded or multi-judgment corpus would make `Recall@5 == hit@5` false
        // and every metric here mean something different.
        if self.relevance.graded || self.relevance.judgments_per_query != 1 {
            return Err(CorpusError::UnexpectedShape(format!(
                "relevance is graded={} judgments_per_query={}, expected false/1",
                self.relevance.graded, self.relevance.judgments_per_query
            )));
        }
        if self.relevance.kind != "single-relevant" {
            return Err(CorpusError::UnexpectedRelevance(
                self.relevance.kind.clone(),
            ));
        }
        if self.queries.len() != EXPECTED_QUERY_COUNT {
            return Err(CorpusError::QueryCount {
                found: self.queries.len(),
            });
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for query in &self.queries {
            if !seen.insert(query.id.as_str()) {
                return Err(CorpusError::DuplicateId(query.id.clone()));
            }
            if query.expected.file.trim().is_empty() {
                return Err(CorpusError::EmptyTarget(query.id.clone()));
            }
            if query.expected.match_mode != "substring" {
                return Err(CorpusError::UnsupportedMatchMode {
                    id: query.id.clone(),
                    mode: query.expected.match_mode.clone(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/search/corpus.json")
            .canonicalize()
            .expect("corpus fixture exists")
    }

    fn valid() -> Corpus {
        Corpus::load(&fixture_path()).expect("the shipped corpus is valid")
    }

    #[test]
    fn the_shipped_corpus_is_the_49_query_one() {
        let corpus = valid();
        assert_eq!(corpus.family, "search");
        assert_eq!(corpus.queries.len(), EXPECTED_QUERY_COUNT);
        assert_eq!(corpus.relevance.kind, "single-relevant");
        assert!(!corpus.relevance.graded);
        assert_eq!(corpus.relevance.judgments_per_query, 1);
        assert_eq!(corpus.metrics, ["hit@1", "hit@3", "hit@5", "mrr"]);
    }

    #[test]
    fn every_query_id_is_unique_and_every_target_is_usable() {
        let corpus = valid();
        let ids: BTreeSet<&str> = corpus.queries.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(ids.len(), corpus.queries.len());
        for query in &corpus.queries {
            assert!(!query.query.trim().is_empty(), "{}", query.id);
            assert!(!query.expected.file.trim().is_empty(), "{}", query.id);
            assert_eq!(query.expected.match_mode, "substring", "{}", query.id);
        }
    }

    /// One query is deliberately file-level (`symbol: null`) — the case scoring
    /// must not treat as "no target".
    #[test]
    fn the_corpus_contains_a_file_level_target() {
        let corpus = valid();
        let file_level = corpus
            .queries
            .iter()
            .filter(|q| q.expected.symbol.is_none())
            .count();
        assert_eq!(file_level, 1, "the imported corpus has exactly one");
    }

    #[test]
    fn a_short_corpus_is_refused() {
        let mut corpus = valid();
        corpus.queries.truncate(10);
        assert_eq!(
            corpus.validate(),
            Err(CorpusError::QueryCount { found: 10 })
        );
    }

    #[test]
    fn a_duplicate_id_is_refused() {
        let mut corpus = valid();
        let first = corpus.queries[0].id.clone();
        corpus.queries[1].id = first.clone();
        assert_eq!(corpus.validate(), Err(CorpusError::DuplicateId(first)));
    }

    #[test]
    fn an_unsupported_match_mode_is_refused() {
        let mut corpus = valid();
        corpus.queries[0].expected.match_mode = "regex".to_string();
        assert_eq!(
            corpus.validate(),
            Err(CorpusError::UnsupportedMatchMode {
                id: corpus.queries[0].id.clone(),
                mode: "regex".to_string(),
            })
        );
    }

    #[test]
    fn a_graded_or_multi_relevant_corpus_is_refused() {
        let mut corpus = valid();
        corpus.relevance.kind = "graded".to_string();
        assert_eq!(
            corpus.validate(),
            Err(CorpusError::UnexpectedRelevance("graded".to_string()))
        );
    }
}
