//! Turning incoming text into the canon before it reaches the store — the
//! write half of [ADR-0011] §Decision 2 (`T21-14`).
//!
//! # One function, three answers, no store
//!
//! [`normalize_for_write`] takes text and returns what the canon should be. It
//! opens no database, holds no lock and knows nothing about `memory_entry`:
//! deciding *what* to store and actually storing it are the callers' jobs, and
//! keeping them apart is what lets the same decision serve `remember`,
//! `edit_memory` and the consolidation router's output without any of them
//! sharing a transaction shape.
//!
//! # The common case must cost nothing, and it does so structurally
//!
//! The detector (`local_rag_memory::normalize::detect`) is a pure function over
//! the text: no model, no network, no state. English input — which is what
//! `T21-11` asks every caller for, in the server instructions, the tool
//! descriptions and the router's own prompt — returns [`Normalized::AlreadyEnglish`]
//! without the generator ever being consulted. That is ADR-0010 Decision 8,
//! still in force, and it is the reason a translation step is tolerable on a
//! request path at all.
//!
//! # A refusal is an outcome, not an error
//!
//! No installed model, a policy-blocked pool, a torn JSON envelope, an answer
//! the validator would not accept — all of them come back as
//! [`Normalized::Refused`], carrying the retry vocabulary D-050 established.
//! None of them may cost the caller their note: ADR-0011 §Decision 3's
//! invariant is *eventually* English, so the caller stores the author's text as
//! it stands, records the refusal, and lets `T21-17`'s sweep install the canon
//! later. Losing somebody's note because a local model produced malformed JSON
//! would be the worse failure, and this type is shaped so that outcome is not
//! expressible.
//!
//! [ADR-0011]: ../../../../../docs/adr/0011-english-canon-for-durable-memory.md

use std::sync::Arc;

use local_rag_core::config::DataPolicy;
use local_rag_core::hash::sha256_hex;
use local_rag_embed::GeneratorPool;
use local_rag_memory::normalize::detect::{ScriptClass, script_class};
use local_rag_memory::normalize::translate::{
    TRANSLATOR_VERSION, TranslateFailureKind, TranslateRequest, Translation,
    classify_translate_failure, translate,
};
use local_rag_store::{
    CURRENT_NORMALIZER_VERSION, GeneratedOp, NormalizationStatus, NormalizationWrite,
    ProposedOperation,
};

/// What the canon should be for one incoming text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Normalized {
    /// The detector answered on its own: the text is already the canon and the
    /// generator was never called.
    AlreadyEnglish { class: ScriptClass },
    /// A validated English variant. `english` becomes the canon; `original` is
    /// kept as provenance so the author can still read their own words.
    Translated {
        english: String,
        original: String,
        model_id: String,
    },
    /// No usable canon was produced. The caller stores the author's text
    /// unchanged and records why — never a half-translation, never nothing.
    Refused {
        reason: String,
        kind: TranslateFailureKind,
    },
}

impl Normalized {
    /// The text to store, given the text that came in.
    ///
    /// Deliberately total: every variant has an answer, so a caller cannot
    /// reach the store holding "no canon".
    pub fn canon<'a>(&'a self, incoming: &'a str) -> &'a str {
        match self {
            Normalized::Translated { english, .. } => english,
            Normalized::AlreadyEnglish { .. } | Normalized::Refused { .. } => incoming,
        }
    }

    /// The detector's answer as an advisory label, or the translator's own
    /// verdict. Advisory only — nothing branches on it.
    pub fn source_language(&self) -> Option<&'static str> {
        match self {
            Normalized::AlreadyEnglish { class } => Some(script_label(*class)),
            Normalized::Translated { .. } => Some(script_label(ScriptClass::NonLatin)),
            Normalized::Refused { .. } => None,
        }
    }

    /// The prompt generation behind a translation, for the provenance row.
    pub fn prompt_version(&self) -> Option<i64> {
        match self {
            Normalized::Translated { .. } => Some(TRANSLATOR_VERSION),
            _ => None,
        }
    }
}

/// The detector's answer as a stored label.
pub fn script_label(class: ScriptClass) -> &'static str {
    match class {
        ScriptClass::English => "en",
        ScriptClass::NonLatin => "non-latin",
        ScriptClass::Undetermined => "undetermined",
    }
}

/// Decide the canon for `text`.
///
/// `model_id` is a configuration fact the daemon already resolved (the same value
/// `doctor`'s generator section reports) — `GeneratorPool` names its *providers*,
/// not the weights behind them, so it cannot supply it.
///
/// `generators` is `None` on a daemon whose generative model is not installed.
/// That is a refusal like any other, not a panic and not a silent passthrough:
/// a store must keep accepting notes when its optional model is missing, and
/// the recorded refusal is what makes the gap visible in `stats`/`doctor`
/// instead of looking like success.
///
/// Blocking: `GeneratorPool::generate` is synchronous and a local translation
/// takes on the order of a second. A caller on a request path must run this
/// under `spawn_blocking`; a background job that owns its own tick may call it
/// directly.
pub fn normalize_for_write(
    generators: Option<&GeneratorPool>,
    policy: DataPolicy,
    model_id: &str,
    memory_id: &str,
    text: &str,
) -> Normalized {
    let class = script_class(text);
    if class != ScriptClass::NonLatin {
        return Normalized::AlreadyEnglish { class };
    }

    let Some(pool) = generators else {
        return Normalized::Refused {
            reason: "no generative model is installed, so the text was stored as written"
                .to_string(),
            kind: TranslateFailureKind::Unavailable,
        };
    };

    match translate(pool, policy, TranslateRequest { memory_id, text }) {
        Ok(Translation::Translated { english }) => Normalized::Translated {
            english,
            original: text.to_string(),
            model_id: model_id.to_string(),
        },
        // The detector already said non-Latin, so the translator disagreeing is
        // possible only if the two ever diverge. Treat it as the passthrough it
        // is rather than asserting: an entry is not the place to prove a point.
        Ok(Translation::Passthrough { class }) => Normalized::AlreadyEnglish { class },
        Err(e) => Normalized::Refused {
            reason: e.to_string(),
            kind: classify_translate_failure(&e),
        },
    }
}

/// Why a search or recall ran against a query in a language the store is not
/// kept in.
///
/// Typed rather than a bare string so the two pillars cannot drift apart in
/// what they mean, and rendered by [`label`](Self::label) so they cannot drift
/// apart in what they say either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryNotTranslated {
    /// No generative model is installed, so nothing could translate it.
    NoGenerator,
    /// The translator was asked and refused; the reason travels for
    /// diagnostics.
    Refused(String),
}

impl QueryNotTranslated {
    /// The one wording both pillars surface.
    pub fn label(&self) -> String {
        match self {
            QueryNotTranslated::NoGenerator => {
                "no_generator: the query was searched as written".to_string()
            }
            QueryNotTranslated::Refused(reason) => format!("translation_refused: {reason}"),
        }
    }
}

/// Everything a boundary needs to decide the canon, in one value.
///
/// Both boundaries — the write path (`T21-14`) and the two query paths
/// (`T21-15`, `T21-19`) — need the same three things and the same
/// `spawn_blocking` hop. Bundling them means a caller in the *code* pillar can
/// translate without being handed a `MemoryContext`, which is what the plumbing
/// would otherwise have forced.
#[derive(Clone)]
pub struct Translator {
    pub generators: Option<Arc<GeneratorPool>>,
    pub model_id: String,
    pub policy: DataPolicy,
}

impl Translator {
    /// Decide the canon for `text`, off the async worker.
    ///
    /// `GeneratorPool::generate` is synchronous and a translation takes a few
    /// hundred milliseconds. Every caller here is on a request path, where
    /// blocking a tokio worker would delay other requests the daemon is
    /// serving — so the `spawn_blocking` hop lives here once rather than at
    /// each call site.
    ///
    /// A panicked blocking task degrades to a refusal rather than an error:
    /// callers still hold the original text, and losing it to an executor
    /// mishap would be exactly the failure ADR-0011 §Decision 3 prevents.
    pub async fn decide(&self, subject: &str, text: &str) -> Normalized {
        let generators = self.generators.clone();
        let policy = self.policy;
        let model_id = self.model_id.clone();
        let (subject, text) = (subject.to_string(), text.to_string());
        tokio::task::spawn_blocking(move || {
            normalize_for_write(generators.as_deref(), policy, &model_id, &subject, &text)
        })
        .await
        .unwrap_or_else(|e| Normalized::Refused {
            reason: format!("the translation task did not finish: {e}"),
            kind: TranslateFailureKind::Transient,
        })
    }

    /// The canon for a **query**, plus why it is not English when it is not.
    ///
    /// Shared by both query boundaries so "a query we could not translate" has
    /// one meaning and one wording, not two. An empty query never reaches the
    /// translator: there is nothing to decide, and a termless recall must stay
    /// free.
    pub async fn decide_query(&self, query: &str) -> (String, Option<QueryNotTranslated>) {
        if query.trim().is_empty() {
            return (query.to_string(), None);
        }
        match self.decide("query", query).await {
            Normalized::AlreadyEnglish { .. } => (query.to_string(), None),
            Normalized::Translated { english, .. } => (english, None),
            // The search still runs, on the author's own words: the dense leg
            // is multilingual and does most of the work. What the caller must
            // not get is silence about why the lexical leg found nothing
            // (02 §6).
            Normalized::Refused { reason, kind } => {
                let why = if kind == TranslateFailureKind::Unavailable {
                    QueryNotTranslated::NoGenerator
                } else {
                    QueryNotTranslated::Refused(reason)
                };
                (query.to_string(), Some(why))
            }
        }
    }
}

/// An owned `NormalizationWrite`, so a decision can cross into a `move`
/// transaction closure without borrowing the text it describes.
///
/// `local_rag_store::NormalizationWrite` is all borrows — right for a store API,
/// wrong for a value that has to survive being sent into
/// `writer().transaction(move |tx| …)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedNormalizationRow {
    memory_id: String,
    status: NormalizationStatus,
    canon_text_sha256: String,
    source_text: Option<String>,
    source_language: Option<String>,
    normalizer_model_id: Option<String>,
    prompt_version: Option<i64>,
    attempt_count: i64,
    last_error: Option<String>,
}

impl OwnedNormalizationRow {
    /// The row that belongs beside `canon`, given how it was decided.
    ///
    /// `canon` is what the entry's text will be once the enclosing transaction
    /// commits, so it is what the staleness hash is taken over — not the text
    /// that came in. On a create the two happen to be equal for the
    /// already-English and refused cases and differ for a translation; the
    /// caller passing the canon explicitly is what keeps that from having to be
    /// re-derived here.
    pub fn for_canon(memory_id: &str, canon: &str, decided: Normalized) -> Self {
        let canon_text_sha256 = sha256_hex(canon.as_bytes());
        let source_language = decided.source_language().map(str::to_string);
        let prompt_version = decided.prompt_version();
        match decided {
            Normalized::Translated {
                original, model_id, ..
            } => OwnedNormalizationRow {
                memory_id: memory_id.to_string(),
                status: NormalizationStatus::Translated,
                canon_text_sha256,
                source_text: Some(original),
                source_language,
                normalizer_model_id: Some(model_id),
                prompt_version,
                attempt_count: 1,
                last_error: None,
            },
            Normalized::AlreadyEnglish { .. } => OwnedNormalizationRow {
                memory_id: memory_id.to_string(),
                status: NormalizationStatus::English,
                canon_text_sha256,
                source_text: None,
                source_language,
                normalizer_model_id: None,
                prompt_version: None,
                attempt_count: 0,
                last_error: None,
            },
            Normalized::Refused { reason, kind } => OwnedNormalizationRow {
                memory_id: memory_id.to_string(),
                status: NormalizationStatus::Failed,
                canon_text_sha256,
                source_text: None,
                source_language: None,
                normalizer_model_id: None,
                prompt_version: None,
                // An `Unavailable` refusal is a property of the environment,
                // not of this entry (ADR-0010 Decision 10, still in force), so
                // it must not consume one of the entry's attempts: the sweep
                // that retries it will find `attempt_count = 0` and try again
                // as soon as a model exists.
                attempt_count: i64::from(kind != TranslateFailureKind::Unavailable),
                last_error: Some(refusal_reason(&reason, LAST_ERROR_MAX_CHARS)),
            },
        }
    }

    pub fn as_write(&self) -> NormalizationWrite<'_> {
        NormalizationWrite {
            memory_id: &self.memory_id,
            status: self.status,
            // The entry's text *is* the canon by the time this row is written —
            // both writers commit them in one transaction — so the guard and
            // the stored hash are the same value here.
            expected_text_sha256: &self.canon_text_sha256,
            canon_text_sha256: &self.canon_text_sha256,
            source_text: self.source_text.as_deref(),
            source_language: self.source_language.as_deref(),
            normalizer_model_id: self.normalizer_model_id.as_deref(),
            prompt_version: self.prompt_version,
            normalizer_version: CURRENT_NORMALIZER_VERSION,
            attempt_count: self.attempt_count,
            last_error: self.last_error.as_deref(),
            next_attempt_at: None,
        }
    }
}

/// Put the router's own output through the same boundary.
///
/// Since `T21-11` the router's prompt asks for English, so this is a safety net
/// and the detector answers for free almost every time. When it does fire, the
/// translation replaces the op's text and **no provenance row is written** —
/// deliberately, and for a reason worth stating rather than inferring:
///
/// - the text belongs to our own prompt, not to a person, so there are no
///   "author's words" to preserve;
/// - the words that *are* the author's — the observations this op cites — are
///   kept verbatim as evidence and are never translated (ADR-0011 §Decision 7);
/// - `commit_apply_run` owns its transaction inside `local_rag_store`, so a
///   caller out here could only write a provenance row in a *second*
///   transaction, and a crash between them would leave an English canon whose
///   origin was lost. Not writing one is better than writing one unsafely.
///
/// The ordinary sweep marks these entries `english` afterwards, at no cost.
pub fn normalize_generated_ops(
    generators: Option<&GeneratorPool>,
    policy: DataPolicy,
    model_id: &str,
    ops: Vec<GeneratedOp>,
) -> Vec<GeneratedOp> {
    ops.into_iter()
        .map(|op| match op {
            GeneratedOp::Materialize {
                operation,
                evidence_observation_ids,
            } => GeneratedOp::Materialize {
                operation: normalize_operation(generators, policy, model_id, operation),
                evidence_observation_ids,
            },
            GeneratedOp::ProposeCandidate {
                candidate_id,
                operation,
                conflicts,
                evidence_observation_ids,
            } => GeneratedOp::ProposeCandidate {
                candidate_id,
                operation: normalize_operation(generators, policy, model_id, operation),
                conflicts,
                evidence_observation_ids,
            },
            GeneratedOp::Noop => GeneratedOp::Noop,
        })
        .collect()
}

/// The two `ProposedOperation` variants that carry text; the rest name an
/// existing entry and are returned untouched.
fn normalize_operation(
    generators: Option<&GeneratorPool>,
    policy: DataPolicy,
    model_id: &str,
    operation: ProposedOperation,
) -> ProposedOperation {
    match operation {
        ProposedOperation::Create {
            memory_id,
            kind,
            text,
            canonical_key,
            scope_kind,
            scope_owner_id,
            confidence,
            importance,
            valid_from_tree,
            last_verified_tree,
        } => {
            let decided = normalize_for_write(generators, policy, model_id, &memory_id, &text);
            let text = decided.canon(&text).to_string();
            ProposedOperation::Create {
                memory_id,
                kind,
                text,
                canonical_key,
                scope_kind,
                scope_owner_id,
                confidence,
                importance,
                valid_from_tree,
                last_verified_tree,
            }
        }
        ProposedOperation::Supersede {
            old_memory_id,
            old_expected_version,
            new_memory_id,
            new_kind,
            new_text,
            new_canonical_key,
            new_scope_kind,
            new_scope_owner_id,
            new_confidence,
            new_importance,
            new_valid_from_tree,
            new_last_verified_tree,
        } => {
            let decided =
                normalize_for_write(generators, policy, model_id, &new_memory_id, &new_text);
            let new_text = decided.canon(&new_text).to_string();
            ProposedOperation::Supersede {
                old_memory_id,
                old_expected_version,
                new_memory_id,
                new_kind,
                new_text,
                new_canonical_key,
                new_scope_kind,
                new_scope_owner_id,
                new_confidence,
                new_importance,
                new_valid_from_tree,
                new_last_verified_tree,
            }
        }
        other => other,
    }
}

/// How much of a refusal's reason the row keeps.
const LAST_ERROR_MAX_CHARS: usize = 200;

/// A refusal's reason, shortened for `last_error`.
///
/// The full text of a model's complaint can be long; the row exists so a human
/// can tell *why* an entry is not English, not to archive the generator's
/// prose.
pub fn refusal_reason(reason: &str, limit: usize) -> String {
    if reason.chars().count() <= limit {
        return reason.to_string();
    }
    let mut out: String = reason.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_text_never_reaches_the_generator() {
        // `None` for the pool is the strongest possible form of this assertion:
        // if the detector did not short-circuit, this would refuse instead.
        let decided = normalize_for_write(
            None,
            DataPolicy::LocalOnly,
            "test-model",
            "m-1",
            "the consolidation runner dead-letters a mechanical failure per build fingerprint",
        );
        assert!(matches!(decided, Normalized::AlreadyEnglish { .. }));
        assert_eq!(decided.canon("ignored"), "ignored");
    }

    #[test]
    fn a_missing_model_refuses_and_keeps_the_authors_text() {
        let text = "мы решили всегда запускать тесты перед коммитом";
        let decided = normalize_for_write(None, DataPolicy::LocalOnly, "test-model", "m-1", text);
        let Normalized::Refused { kind, reason } = &decided else {
            panic!("expected a refusal, got {decided:?}");
        };
        assert_eq!(*kind, TranslateFailureKind::Unavailable);
        assert!(reason.contains("stored as written"), "{reason}");
        assert_eq!(
            decided.canon(text),
            text,
            "a refusal must never cost the author their note",
        );
    }

    #[test]
    fn canon_is_total_over_every_outcome() {
        let translated = Normalized::Translated {
            english: "always run the tests before committing".to_string(),
            original: "мы решили всегда запускать тесты перед коммитом".to_string(),
            model_id: "m".to_string(),
        };
        assert_eq!(
            translated.canon("whatever came in"),
            "always run the tests before committing",
        );
        assert_eq!(translated.prompt_version(), Some(TRANSLATOR_VERSION));
        assert_eq!(
            Normalized::AlreadyEnglish {
                class: ScriptClass::English
            }
            .prompt_version(),
            None,
        );
    }

    /// The router's own output crosses the boundary, and the ops that name an
    /// existing entry rather than carrying text come back untouched.
    #[test]
    fn generated_ops_are_normalized_by_text_carrying_variant_only() {
        let create = ProposedOperation::Create {
            memory_id: "m-1".to_string(),
            kind: "fact".to_string(),
            text: "мы решили использовать pnpm".to_string(),
            canonical_key: None,
            scope_kind: "global".to_string(),
            scope_owner_id: "global".to_string(),
            confidence: 0.6,
            importance: 0.5,
            valid_from_tree: None,
            last_verified_tree: None,
        };
        let reinforce = ProposedOperation::Reinforce {
            memory_id: "m-2".to_string(),
            expected_version: 3,
            confidence: None,
        };
        let ops = vec![
            GeneratedOp::Materialize {
                operation: create,
                evidence_observation_ids: vec!["o1".to_string()],
            },
            GeneratedOp::Materialize {
                operation: reinforce.clone(),
                evidence_observation_ids: vec![],
            },
            GeneratedOp::Noop,
        ];

        // No pool: the Russian create refuses and therefore keeps its text,
        // which is the property that matters — the op survives either way and
        // nothing is dropped on the floor.
        let out = normalize_generated_ops(None, DataPolicy::LocalOnly, "test-model", ops);

        assert_eq!(out.len(), 3, "no op is lost crossing the boundary");
        let GeneratedOp::Materialize { operation, .. } = &out[0] else {
            panic!("expected the create to survive as a materialize");
        };
        let ProposedOperation::Create { text, .. } = operation else {
            panic!("expected a create");
        };
        assert_eq!(text, "мы решили использовать pnpm");
        assert!(
            matches!(&out[1], GeneratedOp::Materialize { operation, .. } if *operation == reinforce),
            "an op that carries no text is returned byte for byte",
        );
        assert_eq!(out[2], GeneratedOp::Noop);
    }

    #[test]
    fn a_refusal_reason_is_shortened_without_losing_that_it_was_shortened() {
        assert_eq!(refusal_reason("short", 32), "short");
        let long = "x".repeat(40);
        let cut = refusal_reason(&long, 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'), "{cut}");
    }
}
