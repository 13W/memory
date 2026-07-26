//! `get_file_context(path)` (spec 11 §2) — T12-04.
//!
//! "File's occurrence list (ids, kinds, names, spans) + snippet from
//! `source_blob` of the active generation."
//!
//! Same discipline as `search_code`: resolve the request's worktree *before*
//! any lock, then read the active generation under `L2.read` (spec 06 §3). The
//! lock matters here for the same reason it does for search — the answer names
//! a generation, and a concurrent switch must not let the occurrence list come
//! from one generation and the snippets from another.
//!
//! # Absent is not one thing
//!
//! A path can be missing from the active generation for two very different
//! reasons, and the caller deserves to know which: it was never seen, or it was
//! deliberately **skipped** (binary, too large, secret-flagged, ignored — spec
//! 06 §2.2). Both are `PATH_NOT_INDEXED`, distinguished by `details`. Reporting
//! them identically would make "why can't I see my file?" unanswerable, and
//! reporting a skipped file as empty-but-present would be worse: a `secret`
//! file has no `source_blob` at all (12 §5), so there is nothing to show and
//! that fact is the answer.

use local_rag_protocol::{ErrorEnvelope, FileContext, FileOccurrence, GenerationRef};
use local_rag_store::rusqlite::Connection;
use local_rag_store::{OccurrenceMetadata, generation_number, occurrences_for_path, skip_reason};

use crate::pipeline::SearchInfraError;
use crate::snippet::cut_batch;

/// Build the file context for `normalized_path` in `generation_id`.
///
/// The caller supplies a `state.sqlite` read connection already opened under
/// the worktree's `L2.read`.
pub(crate) fn file_context(
    conn: &Connection,
    generation_id: &str,
    normalized_path: &str,
) -> Result<Result<FileContext, ErrorEnvelope>, SearchInfraError> {
    let number = generation_number(conn, generation_id)
        .map_err(SearchInfraError::StateRead)?
        .ok_or_else(|| SearchInfraError::MissingGeneration(generation_id.to_string()))?;

    let metadata = occurrences_for_path(conn, generation_id, normalized_path)
        .map_err(SearchInfraError::StateRead)?;

    if metadata.is_empty() {
        // No occurrences: either skipped (a recorded decision) or simply not
        // part of this generation.
        let skipped = skip_reason(conn, generation_id, normalized_path)
            .map_err(SearchInfraError::StateRead)?;
        let details = match skipped {
            Some(reason) => format!("skipped, reason={}", reason.as_str()),
            None => "no such path in the active generation".to_string(),
        };
        return Ok(Err(ErrorEnvelope::path_not_indexed(
            normalized_path,
            details,
        )));
    }

    let refs: Vec<&OccurrenceMetadata> = metadata.iter().collect();
    // Snippets from the stored bytes, batched one read per revision — every
    // occurrence of one file shares a revision, so this is a single read here.
    let (snippets, _diagnostics) = cut_batch(conn, &refs).map_err(SearchInfraError::StateRead)?;

    let occurrences = metadata
        .iter()
        .zip(snippets)
        .map(|(meta, snippet)| FileOccurrence {
            occurrence_id: meta.occurrence_id.clone(),
            unit_kind: meta.unit_kind.as_str().to_string(),
            name: meta.local_name.clone().unwrap_or_default(),
            qualified_name: meta.qualified_name.clone(),
            span: [meta.span_start, meta.span_end],
            snippet,
        })
        .collect();

    Ok(Ok(FileContext {
        path: normalized_path.to_string(),
        generation: GenerationRef {
            id: generation_id.to_string(),
            number,
        },
        occurrences,
    }))
}
