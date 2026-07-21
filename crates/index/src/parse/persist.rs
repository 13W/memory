//! Deterministic, idempotent persistence of a [`ParseOutput`] (T04-06).
//!
//! [`persist_parse_output`] writes the **content side** of one file's parse into
//! `state.sqlite` — `content_blob`, `parsed_unit`, and `unresolved_reference` rows —
//! inside a single caller-owned transaction. It is the reusable encapsulation of the
//! sequence the reconcile pipeline (group 05) runs per changed file; generation
//! membership (`generation_unit_occurrence`) is that later group's concern and is
//! **not** touched here.
//!
//! ## Invariants
//!
//! - **Atomic**: every row goes through the one `&Transaction`, so any error rolls
//!   the whole file's graph back (no partial graph, spec 06 §2.1).
//! - **Idempotent create/reuse**: `content_blob` reuses by its content-derived PK;
//!   `parsed_unit` reuses by its natural key `(file_revision_id, unit_kind,
//!   syntax_locator, span_start, span_end)`; `unresolved_reference` (which has no
//!   natural key) is cleared per revision then reinserted. Re-persisting an
//!   unchanged revision therefore produces no duplicates and returns the *same*
//!   unit ids — the stability the deterministic `occurrence_id` of group 05 needs.
//! - **Path-free**: all three tables are content-shared and carry no path/generation
//!   column (spec 03 §2.3–2.4). Byte spans index the exact `source_blob`; each
//!   `blob_id` is derived from the *normalized* text of that span (two byte worlds).
//! - **Deterministic**: no clock or id source of its own — `now_ms` and the
//!   per-unit candidate `unit_id`s are supplied by the caller (production mints them
//!   from [`local_rag_core::identity::UuidSource`]; tests from a seeded source).

use local_rag_store::rusqlite::{self, Transaction};
use local_rag_store::{
    NewParsedUnit, NewUnresolvedReference, create_or_reuse_content_blob,
    create_or_reuse_parsed_unit, delete_unresolved_references_for_revision, derive_content_blob,
    insert_unresolved_reference,
};

use crate::parse::language::LanguageId;
use crate::parse::locator::SyntaxLocator;
use crate::parse::output::ParseOutput;

/// The result of a [`persist_parse_output`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistOutcome {
    /// The persisted `unit_id` of each unit, indexed exactly like
    /// [`ParseOutput::units`] (reused id if the row already existed, otherwise the
    /// caller's candidate). Group 05 keys occurrences off these.
    pub unit_ids: Vec<String>,
    /// How many `parsed_unit` rows were newly inserted.
    pub created_units: usize,
    /// How many `parsed_unit` rows were reused.
    pub reused_units: usize,
    /// How many `unresolved_reference` rows were (re)inserted.
    pub references_inserted: usize,
}

/// Persist `output`'s content graph for `file_revision_id` (T04-06).
///
/// `source_blob` is the exact original bytes the parse ran on (valid UTF-8 by the
/// parser contract); each unit's `blob_id` is derived from the normalized text of
/// its byte span. `candidate_unit_ids` supplies one fresh `unit_id` per unit (same
/// length and order as `output.units`); a candidate is consumed only when its unit
/// is newly created, ignored on reuse. Everything runs inside `tx`, so a returned
/// `Err` leaves the store unchanged.
pub fn persist_parse_output(
    tx: &Transaction<'_>,
    file_revision_id: &str,
    language: LanguageId,
    source_blob: &[u8],
    output: &ParseOutput,
    candidate_unit_ids: &[String],
    now_ms: i64,
) -> rusqlite::Result<PersistOutcome> {
    assert_eq!(
        candidate_unit_ids.len(),
        output.units.len(),
        "candidate_unit_ids must supply one id per parsed unit"
    );

    // The parser contract guarantees valid UTF-8; spans fall on char boundaries, so
    // slicing the &str below never panics. A non-UTF-8 blob is a caller bug and is
    // surfaced (not panicked) to keep the transaction closure fallible.
    let source = std::str::from_utf8(source_blob)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

    let lang = language.as_str();
    let mut unit_ids: Vec<String> = Vec::with_capacity(output.units.len());
    let mut created_units = 0usize;
    let mut reused_units = 0usize;

    // Canonical order guarantees a parent's index is smaller than its children's, so
    // `unit_ids[parent]` is always already filled when a child is processed.
    for (i, unit) in output.units.iter().enumerate() {
        let slice = &source[unit.span.start as usize..unit.span.end as usize];
        let derived = derive_content_blob(lang, slice);
        create_or_reuse_content_blob(tx, &derived, lang, now_ms)?;

        let locator =
            SyntaxLocator::from_draft(unit.locator_draft(language), derived.blob_id.clone())
                .serialize();
        let parent_unit_id = unit.parent.map(|p| unit_ids[p].clone());

        let outcome = create_or_reuse_parsed_unit(
            tx,
            &NewParsedUnit {
                unit_id: &candidate_unit_ids[i],
                file_revision_id,
                unit_kind: unit.unit_kind,
                syntax_locator: &locator,
                blob_id: &derived.blob_id,
                span_start: unit.span.start as i64,
                span_end: unit.span.end as i64,
                local_name: unit.local_name.as_deref(),
                kind: unit.lang_kind.as_deref(),
                parent_unit_id: parent_unit_id.as_deref(),
            },
        )?;
        if outcome.is_created() {
            created_units += 1;
        } else {
            reused_units += 1;
        }
        unit_ids.push(outcome.id().to_string());
    }

    // References have no natural key (a file may repeat a specifier), so idempotent
    // re-persistence is a scoped clear-then-reinsert rather than per-row reuse.
    delete_unresolved_references_for_revision(tx, file_revision_id)?;
    for reference in &output.unresolved {
        insert_unresolved_reference(
            tx,
            &NewUnresolvedReference {
                file_revision_id,
                source_unit_id: &unit_ids[reference.source_unit],
                reference_text: &reference.reference_text,
                reference_kind: reference.reference_kind.as_str(),
            },
        )?;
    }

    Ok(PersistOutcome {
        unit_ids,
        created_units,
        reused_units,
        references_inserted: output.unresolved.len(),
    })
}
