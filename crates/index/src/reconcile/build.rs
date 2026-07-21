//! The generation builder (spec 06 §2, "build generation N+1") — T05-03.
//!
//! [`build_generation`] turns a [`ScanManifest`](crate::scan::ScanManifest) (the
//! authoritative candidate list, T05-02) into a new `generation` with structural
//! sharing: unchanged content reuses its `file_revision` and `parsed_unit` rows
//! (no read, no parse), changed/new content is read, classified, parsed, and
//! persisted, and every indexed file gets `generation_file` membership plus one
//! deterministic `generation_unit_occurrence` per unit. The generation is born
//! `building` and reaches `projection_ready` **only** once every entry is
//! persisted; any failure transitions it to `failed`.
//!
//! # Boundaries
//!
//! This task **stops at `projection_ready`**. It does not activate the generation,
//! set `worktree.current_generation_id`, or touch `worktree_projection_state` —
//! that is the projection switch (spec 05 §5, a later group). It marks the
//! generation `failed` on **any** post-allocate error (including a failed finalize
//! transition, spec 04 §1 "error in reconcile/switch") and — under the `failpoints`
//! feature — hosts a named injection point at each build phase (T05-05). The
//! `last_error`/backoff/counter *observability* around those failures lives on the
//! reconcile driver ([`super::driver`], T05-05); persisting `last_error` into
//! `worktree_projection_state` is the projection switch (spec 05 §5, group 07).
//! Files whose extension selects no v0 language
//! ([`select_language`](crate::parse::select_language) returns `None`) are
//! **deferred**, not indexed and not skipped: the language-agnostic
//! `config_section | text_section | fallback_chunk` path is a later task (that
//! selector's own doc says so), and there is no `SkipReason` for "unsupported".
//!
//! # Concurrency
//!
//! The single global writer runs each transaction closure on one thread
//! (`FnOnce(&Transaction) + Send + 'static`), so all filesystem reads, redaction/
//! classification, `prepare_source`, and parsing happen in this function's body,
//! **before** a transaction closure is built; only the SQLite writes run inside
//! `tx`. One read connection is opened for the whole build and used for the
//! structural-sharing pre-check (an optimization only — `create_or_reuse_file_revision`
//! inside the tx is the authority, so a concurrent create is absorbed as `Reused`).

use std::path::Path;

use local_rag_core::identity::UuidSource;
use local_rag_core::redaction::Scanner;
use local_rag_store::{
    GenerationState, GenerationTransitionError, NewOccurrence, OpenError, PreparedSource,
    SkipReason, StateDb, WriteError, allocate_generation, create_or_reuse_file_revision,
    file_revision_id_by_content_key, insert_generation_file, insert_occurrence,
    insert_skipped_file, occurrence_id, parsed_units_for_revision, prepare_source,
    transition_generation,
};

use crate::classify::{Classification, ClassifierConfig, GitignoreSet, classify};
use crate::parse::{
    LanguageId, ParseOutput, parser_fingerprint, parser_for, persist_parse_output, select_language,
};
use crate::scan::ScanManifest;

/// The tally of one [`build_generation`] run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOutcome {
    /// The generation just built (in state `projection_ready`).
    pub generation_id: String,
    /// Its per-worktree monotone number.
    pub generation_number: i64,
    /// Files that became `generation_file` members.
    pub files_indexed: usize,
    /// Files recorded as `skipped_file`.
    pub files_skipped: usize,
    /// Files deferred to the (later) language-agnostic path — no recognized v0
    /// language, so neither indexed nor skipped.
    pub files_deferred: usize,
    /// Total `generation_unit_occurrence` rows written.
    pub occurrences: usize,
    /// `file_revision` rows newly created (changed/new content).
    pub revisions_created: usize,
    /// `file_revision` rows reused by content (structural sharing).
    pub revisions_reused: usize,
    /// `parsed_unit` rows newly created.
    pub units_created: usize,
    /// `parsed_unit` rows reused.
    pub units_reused: usize,
}

/// Why a [`build_generation`] failed. Carries the `generation_id` so a caller
/// (T05-05) can attach `last_error` / backoff to the failed generation.
#[derive(Debug)]
pub struct BuildError {
    /// The generation that was being built (now transitioned to `failed`,
    /// best-effort).
    pub generation_id: String,
    /// The underlying cause.
    pub kind: BuildErrorKind,
}

/// The underlying cause of a [`BuildError`].
#[derive(Debug)]
pub enum BuildErrorKind {
    /// Opening the read connection for the pre-check failed.
    Open(OpenError),
    /// Reading a changed file's bytes failed.
    ReadFile {
        /// The file whose read failed.
        normalized_path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A write transaction failed (infrastructure).
    Write(WriteError),
    /// A generation state transition was rejected (domain).
    Transition(GenerationTransitionError),
    /// A named build-phase failpoint fired (test-only, `failpoints` feature): the
    /// deterministic per-phase crash injection the T05-05 retry/failure tests arm
    /// (spec 04 §1 `building → failed` edge). Never present in a release build.
    #[cfg(feature = "failpoints")]
    Failpoint(&'static str),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "generation build {} failed: ", self.generation_id)?;
        match &self.kind {
            BuildErrorKind::Open(e) => write!(f, "open read connection: {e}"),
            BuildErrorKind::ReadFile {
                normalized_path,
                source,
            } => write!(f, "read {normalized_path}: {source}"),
            BuildErrorKind::Write(e) => write!(f, "write: {e}"),
            BuildErrorKind::Transition(e) => write!(f, "transition: {e}"),
            #[cfg(feature = "failpoints")]
            BuildErrorKind::Failpoint(name) => write!(f, "failpoint {name} fired"),
        }
    }
}

impl std::error::Error for BuildError {}

/// Build a new generation for `worktree_id` from `manifest` (spec 06 §2).
///
/// `root` is the worktree's canonical absolute path (changed files are read as
/// `root/display_path`). `cfg`/`scanner` are the `huge`/`secret` inputs to
/// [`classify`]. `uuids`/`now_ms` are the caller's id/clock seams (production
/// `SystemUuidV7` + wall clock; tests a seeded source + fixed millis), keeping the
/// builder deterministic.
///
/// On success the generation is `projection_ready`. On any error it is
/// transitioned to `failed` (best-effort) and the error is returned; because the
/// new generation is a distinct row set, no previously-built generation is mutated.
#[allow(clippy::too_many_arguments)]
pub async fn build_generation(
    db: &StateDb,
    worktree_id: &str,
    root: &Path,
    manifest: &ScanManifest,
    cfg: &ClassifierConfig,
    scanner: &Scanner,
    uuids: &(dyn UuidSource + Send + Sync),
    now_ms: i64,
) -> Result<BuildOutcome, BuildError> {
    let generation_id = uuids.next_uuid().to_string();

    // Phase 0: allocate the generation (`building`).
    let generation_number = {
        let (wt, genr) = (worktree_id.to_string(), generation_id.clone());
        db.writer()
            .transaction(move |tx| allocate_generation(tx, &wt, &genr, now_ms))
            .await
            .map_err(|e| BuildError {
                generation_id: generation_id.clone(),
                kind: BuildErrorKind::Write(e),
            })?
    };

    // Phases 1..N + finalize. Any error after allocation funnels through one path
    // that marks the generation `failed` (best-effort) — including a failure of the
    // final `building → projection_ready` transition itself (spec 04 §1 "error in
    // reconcile/switch"), so a generation never lingers in `building` after a failed
    // build. Because the new generation is a distinct row set, no previously-built
    // generation is mutated.
    let built: Result<BuildOutcome, BuildErrorKind> = async {
        let outcome = run_build(
            db,
            &generation_id,
            generation_number,
            root,
            manifest,
            cfg,
            scanner,
            uuids,
            now_ms,
        )
        .await?;
        // Final phase: `building → projection_ready` only once complete.
        let genr = generation_id.clone();
        db.writer()
            .transaction(move |tx| {
                transition_generation(tx, &genr, GenerationState::ProjectionReady)
            })
            .await
            .map_err(BuildErrorKind::Write)?
            .map_err(BuildErrorKind::Transition)?;
        Ok(outcome)
    }
    .await;

    match built {
        Ok(outcome) => Ok(outcome),
        Err(kind) => {
            let genr = generation_id.clone();
            // Best-effort; if this write also fails the generation stays `building`
            // and is still GC fodder (never routed).
            let _ = db
                .writer()
                .transaction(move |tx| transition_generation(tx, &genr, GenerationState::Failed))
                .await;
            Err(BuildError {
                generation_id,
                kind,
            })
        }
    }
}

/// Persist every manifest entry into the `building` generation, returning the
/// tally. Any error aborts the build (the caller marks it `failed`).
#[allow(clippy::too_many_arguments)]
async fn run_build(
    db: &StateDb,
    generation_id: &str,
    generation_number: i64,
    root: &Path,
    manifest: &ScanManifest,
    cfg: &ClassifierConfig,
    scanner: &Scanner,
    uuids: &(dyn UuidSource + Send + Sync),
    now_ms: i64,
) -> Result<BuildOutcome, BuildErrorKind> {
    // Phase failpoint: fail immediately after allocation, before any per-file work
    // (the generation is `building`; the caller marks it `failed`).
    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        "reconcile.build.after_allocate",
        Err(BuildErrorKind::Failpoint("reconcile.build.after_allocate"))
    );

    let mut out = BuildOutcome {
        generation_id: generation_id.to_string(),
        generation_number,
        files_indexed: 0,
        files_skipped: 0,
        files_deferred: 0,
        occurrences: 0,
        revisions_created: 0,
        revisions_reused: 0,
        units_created: 0,
        units_reused: 0,
    };

    // One read connection for the whole build (the structural-sharing pre-check).
    let read = db.open_read().map_err(BuildErrorKind::Open)?;
    // Ignored files are already pruned by the scan; classify's `ignored` branch is
    // pure defense-in-depth here, so an empty matcher is correct.
    let no_ignores = GitignoreSet::empty();

    for entry in &manifest.entries {
        // Phase failpoint: fail mid-build, while persisting file entries.
        #[cfg(feature = "failpoints")]
        local_rag_test_support::fail_point!(
            "reconcile.build.persist_file",
            Err(BuildErrorKind::Failpoint("reconcile.build.persist_file"))
        );

        let normalized_path = entry.normalized_path.as_str();

        // Huge files are stat-only (no `content_hash`); record the skip, never read.
        let Some(content_hash) = entry.content_hash.as_deref() else {
            write_skipped(db, generation_id, normalized_path, SkipReason::Huge, None).await?;
            out.files_skipped += 1;
            continue;
        };

        // Language is chosen by extension; `None` defers to the language-agnostic
        // path (a later task) — neither indexed nor skipped.
        let Some(language) = select_language(Path::new(normalized_path)) else {
            out.files_deferred += 1;
            continue;
        };
        let fingerprint = parser_fingerprint(language);

        // Structural-sharing pre-check: content already ingested under this
        // parser fingerprint ⇒ reuse the revision + its units, no read/parse.
        let reused_revision = file_revision_id_by_content_key(&read, content_hash, &fingerprint)
            .map_err(|e| BuildErrorKind::Write(WriteError::Sqlite(e)))?;

        if let Some(revision_id) = reused_revision {
            let occ = persist_member_from_revision(
                db,
                generation_id,
                normalized_path,
                &entry.display_path,
                &revision_id,
            )
            .await?;
            out.files_indexed += 1;
            out.revisions_reused += 1;
            out.occurrences += occ;
            continue;
        }

        // New/changed content: read the exact bytes, classify, and index or skip.
        let bytes = std::fs::read(root.join(&entry.display_path)).map_err(|source| {
            BuildErrorKind::ReadFile {
                normalized_path: normalized_path.to_string(),
                source,
            }
        })?;

        match classify(
            normalized_path,
            entry.size,
            &bytes,
            &no_ignores,
            cfg,
            scanner,
        ) {
            Classification::Skipped(reason) => {
                write_skipped(
                    db,
                    generation_id,
                    normalized_path,
                    reason,
                    Some(content_hash),
                )
                .await?;
                out.files_skipped += 1;
            }
            Classification::Indexed => {
                let prepared = prepare_source(&bytes);
                let parse_output = parser_for(language).parse(&bytes);
                let new_revision_id = uuids.next_uuid().to_string();
                let candidate_unit_ids: Vec<String> = (0..parse_output.units.len())
                    .map(|_| uuids.next_uuid().to_string())
                    .collect();

                let delta = persist_indexed_file(
                    db,
                    generation_id,
                    normalized_path,
                    &entry.display_path,
                    language,
                    fingerprint,
                    new_revision_id,
                    prepared,
                    bytes,
                    parse_output,
                    candidate_unit_ids,
                    now_ms,
                )
                .await?;
                out.files_indexed += 1;
                out.occurrences += delta.occurrences;
                if delta.revision_created {
                    out.revisions_created += 1;
                } else {
                    out.revisions_reused += 1;
                }
                out.units_created += delta.units_created;
                out.units_reused += delta.units_reused;
            }
        }
    }

    // Phase failpoint: fail after all entries are persisted, before the caller runs
    // the final `building → projection_ready` transition.
    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        "reconcile.build.before_finalize",
        Err(BuildErrorKind::Failpoint("reconcile.build.before_finalize"))
    );

    Ok(out)
}

/// The per-file delta returned by [`persist_indexed_file`].
struct IndexedDelta {
    occurrences: usize,
    revision_created: bool,
    units_created: usize,
    units_reused: usize,
}

/// Persist one indexed (new/changed) file in a single transaction: ensure the
/// `file_revision` (create or reuse), persist its parse graph on create, add the
/// `generation_file` member, and mint one occurrence per unit.
#[allow(clippy::too_many_arguments)]
async fn persist_indexed_file(
    db: &StateDb,
    generation_id: &str,
    normalized_path: &str,
    display_path: &str,
    language: LanguageId,
    fingerprint: String,
    new_revision_id: String,
    prepared: PreparedSource,
    source_blob: Vec<u8>,
    parse_output: ParseOutput,
    candidate_unit_ids: Vec<String>,
    now_ms: i64,
) -> Result<IndexedDelta, BuildErrorKind> {
    let (genr, np, dp) = (
        generation_id.to_string(),
        normalized_path.to_string(),
        display_path.to_string(),
    );
    db.writer()
        .transaction(move |tx| {
            let outcome = create_or_reuse_file_revision(
                tx,
                &prepared,
                &fingerprint,
                &new_revision_id,
                now_ms,
            )?;
            let revision_id = outcome.id().to_string();
            let (unit_ids, units_created, units_reused) = if outcome.is_created() {
                let persisted = persist_parse_output(
                    tx,
                    &revision_id,
                    language,
                    &source_blob,
                    &parse_output,
                    &candidate_unit_ids,
                    now_ms,
                )?;
                (
                    persisted.unit_ids,
                    persisted.created_units,
                    persisted.reused_units,
                )
            } else {
                // TOCTOU with the pre-check: another file/worktree created this
                // content concurrently. Reuse its already-persisted units.
                (parsed_units_for_revision(tx, &revision_id)?, 0, 0)
            };
            insert_generation_file(tx, &genr, &np, &dp, &revision_id)?;
            for unit_id in &unit_ids {
                let occ = occurrence_id(&genr, &np, unit_id);
                insert_occurrence(
                    tx,
                    &NewOccurrence {
                        occurrence_id: &occ,
                        generation_id: &genr,
                        normalized_path: &np,
                        unit_id,
                        qualified_name: None,
                        context_hash: None,
                    },
                )?;
            }
            Ok(IndexedDelta {
                occurrences: unit_ids.len(),
                revision_created: outcome.is_created(),
                units_created,
                units_reused,
            })
        })
        .await
        .map_err(BuildErrorKind::Write)
}

/// Persist a structurally-shared member (reused revision) in one transaction:
/// add the `generation_file` row and mint occurrences from the revision's
/// existing units. Returns the occurrence count.
async fn persist_member_from_revision(
    db: &StateDb,
    generation_id: &str,
    normalized_path: &str,
    display_path: &str,
    revision_id: &str,
) -> Result<usize, BuildErrorKind> {
    let (genr, np, dp, rev) = (
        generation_id.to_string(),
        normalized_path.to_string(),
        display_path.to_string(),
        revision_id.to_string(),
    );
    db.writer()
        .transaction(move |tx| {
            let unit_ids = parsed_units_for_revision(tx, &rev)?;
            insert_generation_file(tx, &genr, &np, &dp, &rev)?;
            for unit_id in &unit_ids {
                let occ = occurrence_id(&genr, &np, unit_id);
                insert_occurrence(
                    tx,
                    &NewOccurrence {
                        occurrence_id: &occ,
                        generation_id: &genr,
                        normalized_path: &np,
                        unit_id,
                        qualified_name: None,
                        context_hash: None,
                    },
                )?;
            }
            Ok(unit_ids.len())
        })
        .await
        .map_err(BuildErrorKind::Write)
}

/// Record a `skipped_file` row in one transaction.
async fn write_skipped(
    db: &StateDb,
    generation_id: &str,
    normalized_path: &str,
    reason: SkipReason,
    content_hash: Option<&str>,
) -> Result<(), BuildErrorKind> {
    let (genr, np, ch) = (
        generation_id.to_string(),
        normalized_path.to_string(),
        content_hash.map(str::to_string),
    );
    db.writer()
        .transaction(move |tx| insert_skipped_file(tx, &genr, &np, reason, ch.as_deref()))
        .await
        .map_err(BuildErrorKind::Write)
}
