//! The bilingual memory-recall corpus (`fixtures/memory-recall/corpus.json`)
//! — X-010.
//!
//! Unlike `crate::bench`'s 49-query corpus (imported verbatim from v1 and
//! therefore `[FIXED]`), this corpus is new — authored for this task, not
//! comparable against any prior baseline. [`Corpus::validate`] still refuses
//! anything that does not match the shape [`score`](super::score) assumes,
//! for the same reason `bench::corpus` gives: a corpus that drifts silently
//! makes every recorded number incomparable to every other run of it.
//!
//! Every entry and query carries two text variants — `*_original` (as a real
//! user would actually write it: Russian, English, or a mix) and
//! `*_english` (a hand-translated English equivalent, authored once,
//! offline, alongside the fixture — never produced by a runtime translation
//! component; see the corpus's own `description` field and the X-010/X-011
//! task cards). [`run`](super::run) picks which field feeds the store/query
//! per `--config`.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use local_rag_store::MemoryKind;
use serde::Deserialize;

/// How many entries/queries the corpus must hold — fixed once the fixture is
/// authored, exactly like `bench::corpus::EXPECTED_QUERY_COUNT`, so a
/// silently truncated or padded fixture is refused rather than scored.
pub const EXPECTED_ENTRY_COUNT: usize = 200;
pub const EXPECTED_QUERY_COUNT: usize = 60;

/// The corpus document.
#[derive(Debug, Clone, Deserialize)]
pub struct Corpus {
    /// Fixture family; always `memory-recall`.
    pub family: String,
    /// Corpus version (semver-ish string).
    pub version: String,
    /// How relevance judgments are shaped.
    pub relevance: Relevance,
    /// The metric names the corpus is scored with.
    pub metrics: Vec<String>,
    /// The memory entries to seed, in corpus order.
    pub entries: Vec<Entry>,
    /// The queries themselves, in corpus order.
    pub queries: Vec<Query>,
}

/// The corpus's relevance model — identical shape to `bench::corpus::Relevance`
/// (single-relevant, non-graded), deliberately not shared: the two corpora
/// are unrelated fixtures that happen to use the same simple model.
#[derive(Debug, Clone, Deserialize)]
pub struct Relevance {
    pub kind: String,
    pub graded: bool,
    pub judgments_per_query: u32,
}

/// One memory entry to seed into the throwaway store before recall runs.
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    /// Stable corpus id (`mr-01`…), not a real `memory_id` — `run` mints a
    /// UUIDv7 per entry and keeps a corpus-id → `memory_id` map.
    pub id: String,
    /// A `local_rag_store::MemoryKind` wire string (`fact`, `decision`, …).
    pub kind: String,
    /// The entry's text as a real user would write it.
    pub text_original: String,
    /// The same entry, hand-translated to English.
    pub text_english: String,
}

/// One benchmark query and its single ground-truth target.
#[derive(Debug, Clone, Deserialize)]
pub struct Query {
    /// Stable query id (`mrq-01`…).
    pub id: String,
    /// `(entry_language, query_language)`, e.g. `ru-ru`, `en-ru` — reporting
    /// only, so a same-language control and a cross-lingual case are never
    /// silently averaged together into one number.
    pub lang_pair: String,
    /// The [`Entry::id`] this query's single relevant target is.
    pub expected_entry_id: String,
    /// The query as a real user would write it.
    pub query_original: String,
    /// The same query, hand-translated to English.
    pub query_english: String,
}

/// The four `lang_pair` values this corpus authors — same-language controls
/// and the cross-lingual cases the normalization question is about.
pub const KNOWN_LANG_PAIRS: [&str; 4] = ["ru-ru", "en-en", "ru-en", "en-ru"];

/// Why a corpus was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorpusError {
    Load(String),
    EntryCount { found: usize },
    QueryCount { found: usize },
    DuplicateEntryId(String),
    DuplicateQueryId(String),
    UnknownTarget { query_id: String, entry_id: String },
    UnknownKind { entry_id: String, kind: String },
    UnknownLangPair { query_id: String, lang_pair: String },
    EmptyText { id: String, field: &'static str },
    UnexpectedRelevance(String),
    UnexpectedShape(String),
}

impl fmt::Display for CorpusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CorpusError::Load(e) => write!(f, "corpus could not be loaded: {e}"),
            CorpusError::EntryCount { found } => write!(
                f,
                "corpus holds {found} entries, expected {EXPECTED_ENTRY_COUNT}"
            ),
            CorpusError::QueryCount { found } => write!(
                f,
                "corpus holds {found} queries, expected {EXPECTED_QUERY_COUNT}"
            ),
            CorpusError::DuplicateEntryId(id) => write!(f, "duplicate entry id {id:?}"),
            CorpusError::DuplicateQueryId(id) => write!(f, "duplicate query id {id:?}"),
            CorpusError::UnknownTarget { query_id, entry_id } => write!(
                f,
                "query {query_id:?} targets {entry_id:?}, which is not an entry id in this corpus"
            ),
            CorpusError::UnknownKind { entry_id, kind } => {
                write!(f, "entry {entry_id:?} has unknown kind {kind:?}")
            }
            CorpusError::UnknownLangPair {
                query_id,
                lang_pair,
            } => write!(
                f,
                "query {query_id:?} has lang_pair {lang_pair:?}, expected one of {KNOWN_LANG_PAIRS:?}"
            ),
            CorpusError::EmptyText { id, field } => write!(f, "{id:?} has an empty {field}"),
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

    const DECLARED_METRICS: [&'static str; 4] = ["hit@1", "hit@3", "hit@5", "mrr"];

    /// Refuse anything [`super::score`] does not know how to compute over.
    pub fn validate(&self) -> Result<(), CorpusError> {
        if self.family != "memory-recall" {
            return Err(CorpusError::UnexpectedShape(format!(
                "family is {:?}, expected \"memory-recall\"",
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
        if self.entries.len() != EXPECTED_ENTRY_COUNT {
            return Err(CorpusError::EntryCount {
                found: self.entries.len(),
            });
        }
        if self.queries.len() != EXPECTED_QUERY_COUNT {
            return Err(CorpusError::QueryCount {
                found: self.queries.len(),
            });
        }

        let mut entry_ids: BTreeSet<&str> = BTreeSet::new();
        for entry in &self.entries {
            if !entry_ids.insert(entry.id.as_str()) {
                return Err(CorpusError::DuplicateEntryId(entry.id.clone()));
            }
            if MemoryKind::from_db(&entry.kind).is_none() {
                return Err(CorpusError::UnknownKind {
                    entry_id: entry.id.clone(),
                    kind: entry.kind.clone(),
                });
            }
            if entry.text_original.trim().is_empty() {
                return Err(CorpusError::EmptyText {
                    id: entry.id.clone(),
                    field: "text_original",
                });
            }
            if entry.text_english.trim().is_empty() {
                return Err(CorpusError::EmptyText {
                    id: entry.id.clone(),
                    field: "text_english",
                });
            }
        }

        let mut query_ids: BTreeSet<&str> = BTreeSet::new();
        for query in &self.queries {
            if !query_ids.insert(query.id.as_str()) {
                return Err(CorpusError::DuplicateQueryId(query.id.clone()));
            }
            if !entry_ids.contains(query.expected_entry_id.as_str()) {
                return Err(CorpusError::UnknownTarget {
                    query_id: query.id.clone(),
                    entry_id: query.expected_entry_id.clone(),
                });
            }
            if !KNOWN_LANG_PAIRS.contains(&query.lang_pair.as_str()) {
                return Err(CorpusError::UnknownLangPair {
                    query_id: query.id.clone(),
                    lang_pair: query.lang_pair.clone(),
                });
            }
            if query.query_original.trim().is_empty() {
                return Err(CorpusError::EmptyText {
                    id: query.id.clone(),
                    field: "query_original",
                });
            }
            if query.query_english.trim().is_empty() {
                return Err(CorpusError::EmptyText {
                    id: query.id.clone(),
                    field: "query_english",
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
            .join("../../fixtures/memory-recall/corpus.json")
            .canonicalize()
            .expect("corpus fixture exists")
    }

    fn valid() -> Corpus {
        Corpus::load(&fixture_path()).expect("the shipped corpus is valid")
    }

    #[test]
    fn the_shipped_corpus_matches_its_declared_shape() {
        let corpus = valid();
        assert_eq!(corpus.family, "memory-recall");
        assert_eq!(corpus.entries.len(), EXPECTED_ENTRY_COUNT);
        assert_eq!(corpus.queries.len(), EXPECTED_QUERY_COUNT);
        assert_eq!(corpus.relevance.kind, "single-relevant");
        assert!(!corpus.relevance.graded);
        assert_eq!(corpus.metrics, ["hit@1", "hit@3", "hit@5", "mrr"]);
    }

    #[test]
    fn every_query_targets_a_real_entry_and_every_id_is_unique() {
        let corpus = valid();
        let entry_ids: BTreeSet<&str> = corpus.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(entry_ids.len(), corpus.entries.len());
        let query_ids: BTreeSet<&str> = corpus.queries.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(query_ids.len(), corpus.queries.len());
        for query in &corpus.queries {
            assert!(
                entry_ids.contains(query.expected_entry_id.as_str()),
                "{}",
                query.id
            );
        }
    }

    /// The corpus is deliberately stratified so cross-lingual recall is
    /// measurable on its own, not averaged away by the same-language controls.
    #[test]
    fn the_corpus_contains_every_lang_pair() {
        let corpus = valid();
        let pairs: BTreeSet<&str> = corpus
            .queries
            .iter()
            .map(|q| q.lang_pair.as_str())
            .collect();
        for expected in KNOWN_LANG_PAIRS {
            assert!(pairs.contains(expected), "{expected}");
        }
    }

    #[test]
    fn a_short_corpus_is_refused() {
        let mut corpus = valid();
        corpus.entries.truncate(5);
        assert_eq!(corpus.validate(), Err(CorpusError::EntryCount { found: 5 }));
    }

    #[test]
    fn a_duplicate_entry_id_is_refused() {
        let mut corpus = valid();
        let first = corpus.entries[0].id.clone();
        corpus.entries[1].id = first.clone();
        assert_eq!(corpus.validate(), Err(CorpusError::DuplicateEntryId(first)));
    }

    #[test]
    fn a_query_targeting_an_unknown_entry_is_refused() {
        let mut corpus = valid();
        corpus.queries[0].expected_entry_id = "mr-does-not-exist".to_string();
        let err = corpus.validate().unwrap_err();
        assert!(matches!(err, CorpusError::UnknownTarget { .. }), "{err}");
    }

    #[test]
    fn an_unknown_memory_kind_is_refused() {
        let mut corpus = valid();
        corpus.entries[0].kind = "not-a-real-kind".to_string();
        let err = corpus.validate().unwrap_err();
        assert!(matches!(err, CorpusError::UnknownKind { .. }), "{err}");
    }

    #[test]
    fn an_unknown_lang_pair_is_refused() {
        let mut corpus = valid();
        corpus.queries[0].lang_pair = "fr-fr".to_string();
        let err = corpus.validate().unwrap_err();
        assert!(matches!(err, CorpusError::UnknownLangPair { .. }), "{err}");
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
