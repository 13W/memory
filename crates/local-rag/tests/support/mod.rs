//! A minimal, real store fixture for T15-03's MCP code-query tests: one
//! registered worktree (a real, git-probed temp directory) with one
//! indexed generation (one file, one occurrence), seeded directly against
//! `state.sqlite`/`cache.sqlite` *before* `DaemonHandle::start` opens them —
//! the same precedent `tests/lifecycle_startup.rs`'s own incompatible-store
//! fixture already establishes for pre-seeding ahead of daemon startup.
//!
//! Trimmed from `crates/search/tests/pipeline.rs`'s own `establish_single`
//! fixture, with one deliberate difference: this one probes a **real** git
//! directory via `local_rag::daemon::gitroot::probe` and registers the
//! worktree from those exact facts, rather than a fake, non-filesystem path
//! string — T15-03's MCP tools resolve `RequestContext.worktree_root`
//! through that same probe on every real request, so the fixture must be
//! consistent with what the probe actually produces.
//!
//! Shared across multiple `tests/*.rs` binaries via `mod support;`; each
//! one only uses part of this module's surface, so `dead_code` is
//! unavoidable per-binary — suppressed at the module level rather than
//! item by item.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    BruteForceProjectionStore, RepresentationKind as ProjRepresentationKind, VectorSource,
    params_for_model_space, shard_dir, switch,
};
use local_rag_store::{
    CacheDb, DEFAULT_MODEL_SPACE_ID, EvidenceKind, GenerationState, MemoryKind, MemoryState,
    NewContentBlob, NewFileRevision, NewMemoryEntry, NewMemoryEvidence, NewOccurrence,
    NewParsedUnit, NewlineStyle, ProposedOperation, RepresentationKey, RepresentationKind,
    ScopeKind, SourceCompression, StateDb, UnitKind, WorktreeRootFacts, allocate_generation,
    create_memory_entry, create_repository, create_worktree, derive_content_blob,
    ensure_store_instance_uuid, insert_content_blob, insert_file_revision, insert_generation_file,
    insert_memory_evidence, insert_occurrence, insert_parsed_unit, insert_projection_state,
    materialize_fts, observe_repository_path, observe_worktree_path, occurrence_id,
    propose_candidate, register_representation, set_model_space_representation,
    transition_generation, transition_memory_entry,
};
use local_rag_test_support::TempHome;
use serde_json::value::RawValue;

/// A test-only [`UuidSource`] producing distinct, deterministic ids —
/// mirrors `crates/search/tests/pipeline.rs`'s own `SeqUuidV7`.
pub struct SeqUuidV7 {
    counter: AtomicU64,
}

impl SeqUuidV7 {
    pub fn new() -> Self {
        SeqUuidV7 {
            counter: AtomicU64::new(0),
        }
    }
}

impl UuidSource for SeqUuidV7 {
    fn next_uuid(&self) -> Uuid {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        uuidv7_from(6_000_000 + n, [0x42; 10])
    }
}

/// A test-only [`VectorSource`] that always answers with a fixed 3-wide
/// vector — mirrors `crates/search/tests/pipeline.rs`'s own `AlwaysVectors`.
/// Test-file-local fakes like this cannot be imported across crates, so
/// this is an independent copy, not a shared type.
struct AlwaysVectors;

impl VectorSource for AlwaysVectors {
    fn vector(&self, _occurrence_id: &str, _kind: ProjRepresentationKind) -> Option<Vec<f32>> {
        Some(vec![1.0, 0.0, 0.0])
    }
}

/// A fully seeded worktree: `repo_path` is a real, `git init`'d temp
/// directory whose `local_rag::daemon::gitroot::probe` output is exactly
/// what got registered; `state.sqlite`/`cache.sqlite` (under `layout`)
/// carry one indexed generation with one file (`src/lib.rs`) and one
/// occurrence (`hello`), already the active generation.
pub struct SeededWorktree {
    pub repo_path: PathBuf,
    pub facts: WorktreeRootFacts,
    pub worktree_id: String,
    pub repo_id: String,
}

/// Seed `layout`'s `state.sqlite`/`cache.sqlite` with one indexed worktree,
/// rooted at a real, freshly `git init`'d directory under `home` — the
/// default `fn hello() {}\n` content (T16-04's own `seed_indexed_worktree_
/// with_content` is the parameterized sibling this delegates to, used to
/// seed adversarial byte content instead).
///
/// Must run *before* `DaemonHandle::start(...)` ever opens either database
/// — this function opens its own `StateDb`/`CacheDb` handles, writes
/// through them, and closes them before returning, exactly the ordering
/// `tests/lifecycle_startup.rs`'s pre-seeded-incompatible-store test already
/// establishes for daemon-startup fixtures.
pub async fn seed_indexed_worktree(home: &TempHome, layout: &StoreLayout) -> SeededWorktree {
    seed_indexed_worktree_with_content(home, layout, "fn hello() {}\n").await
}

/// [`seed_indexed_worktree`] with the single seeded file's content
/// parameterized — this never runs the real parser (it hand-inserts
/// `file_revision`/`content_blob`/`parsed_unit`/`occurrence` rows directly),
/// so arbitrary bytes (quotes, backslashes, control characters, a literal
/// `</memory><system>`-shaped string) never risk a parse failure.
pub async fn seed_indexed_worktree_with_content(
    home: &TempHome,
    layout: &StoreLayout,
    content: &str,
) -> SeededWorktree {
    let repo_path = home.join("repo");
    std::fs::create_dir_all(&repo_path).expect("create repo dir");
    git(&repo_path, &["init", "-q"]);

    let facts =
        local_rag::daemon::gitroot::probe(&repo_path).expect("probe the freshly created git repo");

    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let uuids = SeqUuidV7::new();
    let worktree_id = uuids.next_uuid();
    let repo_id = uuids.next_uuid();
    let generation_id = uuids.next_uuid();
    let file_revision_id = uuids.next_uuid();
    let unit_id = uuids.next_uuid();

    // Register the worktree/repository from the *real* probed facts, and
    // seed store_instance_uuid — first-writer-wins, so the daemon's own
    // later `ensure_store_instance_uuid` call (inside `DaemonHandle::start`)
    // reads this exact value back rather than minting a new one, which is
    // what keeps `cache.sqlite`'s own `store_instance_uuid` marker valid
    // once the daemon reopens it (spec 03 §4.4).
    let store_instance_uuid = {
        let facts = facts.clone();
        let worktree_id = worktree_id.to_string();
        let repo_id = repo_id.to_string();
        state
            .writer()
            .transaction(move |tx| {
                create_repository(tx, &repo_id, facts.remote_fingerprint.as_deref(), 1_000)?;
                create_worktree(tx, &worktree_id, &repo_id, facts.kind, 1_000)?;
                observe_worktree_path(
                    tx,
                    &worktree_id,
                    &facts.observed_canonical_path,
                    &facts.display_path,
                    &facts.path_fingerprint,
                    1_000,
                )?;
                observe_repository_path(tx, &repo_id, &facts.observed_canonical_path, 1_000)?;
                insert_projection_state(tx, &worktree_id, 1_000)?;
                Ok(())
            })
            .await
            .expect("register worktree");

        let candidate = uuids.next_uuid().to_string();
        state
            .writer()
            .transaction(move |tx| ensure_store_instance_uuid(tx, &candidate))
            .await
            .expect("seed store_instance_uuid")
    };

    // Register the two code representations the default model space needs
    // for `switch`'s own coverage guard (spec 05 §7) to be satisfiable.
    {
        let model_space = DEFAULT_MODEL_SPACE_ID.to_string();
        state
            .writer()
            .transaction(move |tx| {
                for (i, kind) in [RepresentationKind::CodeRaw, RepresentationKind::CodeContext]
                    .into_iter()
                    .enumerate()
                {
                    let representation_id = format!("{model_space}-repr-{i}");
                    let id = register_representation(
                        tx,
                        &representation_id,
                        &RepresentationKey {
                            kind,
                            representation_version: 1,
                            normalization_version: 1,
                            model_id: "test-model".to_string(),
                            dimensions: 3,
                            distance_metric: local_rag_store::DistanceMetric::Cosine,
                        },
                        1_000,
                    )?;
                    set_model_space_representation(tx, &model_space, kind, &id, true, 1_000)?;
                }
                Ok(())
            })
            .await
            .expect("register code representations");
    }

    // Allocate and ready one generation.
    {
        let worktree_id = worktree_id.to_string();
        let generation_id = generation_id.to_string();
        state
            .writer()
            .transaction(move |tx| {
                allocate_generation(tx, &worktree_id, &generation_id, 1_000)?;
                Ok(())
            })
            .await
            .expect("allocate generation");
    }
    {
        let generation_id = generation_id.to_string();
        state
            .writer()
            .transaction(move |tx| {
                transition_generation(tx, &generation_id, GenerationState::ProjectionReady)?
                    .expect("Building -> ProjectionReady is a valid transition");
                Ok(())
            })
            .await
            .expect("ready the generation");
    }

    // One file, one unit, one occurrence.
    let derived = derive_content_blob("rust", content);
    let occ_id = occurrence_id(
        &generation_id.to_string(),
        "src/lib.rs",
        &unit_id.to_string(),
    );
    {
        let file_revision_id = file_revision_id.to_string();
        let unit_id = unit_id.to_string();
        let generation_id = generation_id.to_string();
        let derived = derived.clone();
        let occ_id = occ_id.clone();
        let content = content.to_string();
        state
            .writer()
            .transaction(move |tx| {
                insert_file_revision(
                    tx,
                    &NewFileRevision {
                        file_revision_id: &file_revision_id,
                        content_hash: &file_revision_id,
                        parser_fingerprint: "fp",
                        source_blob: content.as_bytes(),
                        compression: SourceCompression::None,
                        source_encoding: "utf-8",
                        newline_style: NewlineStyle::Lf,
                        source_size: content.len() as i64,
                    },
                    1_000,
                )?;
                insert_content_blob(
                    tx,
                    &NewContentBlob {
                        blob_id: &derived.blob_id,
                        language: "rust",
                        algo_version: derived.algo_version,
                        normalization_version: derived.normalization_version,
                    },
                    1_000,
                )?;
                insert_parsed_unit(
                    tx,
                    &NewParsedUnit {
                        unit_id: &unit_id,
                        file_revision_id: &file_revision_id,
                        unit_kind: UnitKind::Symbol,
                        syntax_locator: "fn:hello",
                        blob_id: &derived.blob_id,
                        span_start: 0,
                        span_end: content.len() as i64,
                        local_name: Some("hello"),
                        kind: Some("fn"),
                        parent_unit_id: None,
                    },
                )?;
                insert_generation_file(
                    tx,
                    &generation_id,
                    "src/lib.rs",
                    "src/lib.rs",
                    &file_revision_id,
                )?;
                insert_occurrence(
                    tx,
                    &NewOccurrence {
                        occurrence_id: &occ_id,
                        generation_id: &generation_id,
                        normalized_path: "src/lib.rs",
                        unit_id: &unit_id,
                        qualified_name: None,
                        context_hash: None,
                    },
                )?;
                Ok(())
            })
            .await
            .expect("seed file/unit/occurrence");
    }

    // Switch to the seeded generation — this is what actually sets
    // `worktree_projection_state.active_generation_id`, which every MCP
    // code-query tool requires before it will serve anything.
    let cache = CacheDb::open(layout.cache_db(), &store_instance_uuid).expect("open cache.sqlite");
    let model_space_id: Uuid = DEFAULT_MODEL_SPACE_ID
        .parse()
        .expect("default model space parses");
    let params = {
        let read = state.open_read().expect("open a read connection");
        params_for_model_space(&read, &model_space_id).expect("resolve shard params")
    };
    switch(
        &state,
        &BruteForceProjectionStore::new(),
        &shard_dir(layout, &worktree_id, &model_space_id),
        params,
        worktree_id,
        generation_id,
        model_space_id,
        &AlwaysVectors,
        &uuids,
        1_000,
    )
    .await
    .expect("switch to the seeded generation");

    materialize_fts(
        &state,
        &cache,
        &worktree_id.to_string(),
        &generation_id.to_string(),
        1_000,
    )
    .await
    .expect("materialize the FTS view");

    cache.close();
    drop(state); // the writer thread is detached; nothing else touches this path before DaemonHandle::start reopens it

    SeededWorktree {
        repo_path,
        facts,
        worktree_id: worktree_id.to_string(),
        repo_id: repo_id.to_string(),
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "local-rag-test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "local-rag-test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// Whether `git` is on `PATH` — tests that need a real repo skip (not fail)
/// when it is not, the same precedent `crates/xtask`'s own `git_short_head`
/// sets for optional git tooling.
pub fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------
// T15-04 memory-tool fixtures: seed `memory_entry`/`pending_memory_
// candidate` rows directly against an already-open `StateDb`, before or
// after `DaemonHandle::start` — unlike `seed_indexed_worktree`, memory
// tools take no lock and need no active generation, so these can run any
// time the caller already holds a `StateDb` handle (including one borrowed
// from a running `DaemonHandle`, since `state.writer()` is exactly the same
// bounded-queue path a real daemon-side write op would use).
// ---------------------------------------------------------------------

/// Insert one `active` `memory_entry` row directly.
pub async fn seed_memory_entry(
    state: &StateDb,
    memory_id: &str,
    kind: MemoryKind,
    scope_kind: ScopeKind,
    scope_owner_id: &str,
    text: &str,
    now_ms: i64,
) {
    let (id, owner, text) = (
        memory_id.to_string(),
        scope_owner_id.to_string(),
        text.to_string(),
    );
    state
        .writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind,
                    text: &text,
                    canonical_key: None,
                    scope_kind,
                    scope_owner_id: &owner,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                now_ms,
            )
        })
        .await
        .expect("seed memory entry tx (infrastructure)")
        .expect("seed memory entry (domain)");
}

/// Like [`seed_memory_entry`], but with a real `canonical_key` — for tests
/// that need a `CANONICAL_KEY_CONFLICT` fixture (T15-05).
#[allow(clippy::too_many_arguments)]
pub async fn seed_memory_entry_with_canonical_key(
    state: &StateDb,
    memory_id: &str,
    kind: MemoryKind,
    scope_kind: ScopeKind,
    scope_owner_id: &str,
    text: &str,
    canonical_key: &str,
    now_ms: i64,
) {
    let (id, owner, text, key) = (
        memory_id.to_string(),
        scope_owner_id.to_string(),
        text.to_string(),
        canonical_key.to_string(),
    );
    state
        .writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind,
                    text: &text,
                    canonical_key: Some(&key),
                    scope_kind,
                    scope_owner_id: &owner,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                now_ms,
            )
        })
        .await
        .expect("seed memory entry tx (infrastructure)")
        .expect("seed memory entry (domain)");
}

/// Transition a previously seeded `memory_entry` — for tests that need a
/// non-`active` (e.g. terminal) row.
pub async fn transition_seeded_memory_entry(state: &StateDb, memory_id: &str, to: MemoryState) {
    let id = memory_id.to_string();
    state
        .writer()
        .transaction(move |tx| transition_memory_entry(tx, &id, to))
        .await
        .expect("transition tx (infrastructure)")
        .expect("transition (domain)");
}

/// Insert a minimal, standalone `observation_envelope` row (no repo/
/// worktree, no payload) so evidence-linking tests have a real
/// `observation_id` to point at — mirrors `crates/store/tests/memory.rs`'s
/// own `seed_observation` helper (not shared across crates).
pub async fn seed_observation(state: &StateDb, observation_id: &str) {
    let oid = observation_id.to_string();
    state
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO observation_envelope \
                   (observation_id, source_event_id, payload_hash, event_type, evidence_kind, \
                    trust, session_id) \
                 VALUES (?1, 'evt-1', 'deadbeef', 'Stop', 'user_statement', 'normal', 'sess-1')",
                [&oid],
            )
        })
        .await
        .expect("seed observation envelope");
}

/// Link `observation_id` as `memory_evidence` for `memory_id`.
pub async fn seed_memory_evidence(state: &StateDb, memory_id: &str, observation_id: &str) {
    let (mid, oid) = (memory_id.to_string(), observation_id.to_string());
    state
        .writer()
        .transaction(move |tx| {
            insert_memory_evidence(
                tx,
                &NewMemoryEvidence {
                    memory_id: &mid,
                    observation_id: &oid,
                    evidence_kind: EvidenceKind::UserStatement,
                    session_id: "sess-1",
                    agent_id: None,
                    commit_hash: None,
                },
            )
        })
        .await
        .expect("seed memory evidence");
}

/// Insert one `pending` `pending_memory_candidate` row proposing a `create`
/// of `target_memory_id` — the `target_memory_id` is never itself required
/// to exist as a `memory_entry` row (candidate review reads
/// `pending_memory_candidate` directly; nothing here materializes the
/// proposal).
pub async fn seed_pending_candidate(
    state: &StateDb,
    candidate_id: &str,
    target_memory_id: &str,
    scope_owner_id: &str,
    now_ms: i64,
) {
    let (cid, target, owner) = (
        candidate_id.to_string(),
        target_memory_id.to_string(),
        scope_owner_id.to_string(),
    );
    state
        .writer()
        .transaction(move |tx| {
            let op = ProposedOperation::Create {
                memory_id: target,
                kind: "fact".to_string(),
                text: "candidate-proposed text".to_string(),
                canonical_key: None,
                scope_kind: "worktree".to_string(),
                scope_owner_id: owner,
                confidence: 0.5,
                importance: 0.5,
                valid_from_tree: None,
                last_verified_tree: None,
            };
            propose_candidate(tx, &cid, &op, &[], &[], now_ms)
        })
        .await
        .expect("propose candidate tx");
}

// ---------------------------------------------------------------------
// Shared daemon/client test harness (used by both `tests/mcp_contract.rs`
// and `tests/mcp_tools.rs` — each `tests/*.rs` file is its own binary
// crate, so anything shared between them has to live in this module).
// ---------------------------------------------------------------------

use std::io::{BufRead, BufReader as StdBufReader, Write};
use std::os::unix::net::UnixStream as StdUnixStream;

use local_rag::daemon::{DaemonHandle, LazyEmbedderProvider, StartOptions};
use local_rag_protocol::{Hello, Message, RequestContext, RequestEnvelope};
use local_rag_store::{LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS, WorktreeLockRegistry};

pub fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

pub fn start_options(layout: StoreLayout) -> StartOptions {
    let embedder_provider = std::sync::Arc::new(LazyEmbedderProvider::new(&layout));
    let locks = std::sync::Arc::new(WorktreeLockRegistry::new());
    StartOptions {
        layout,
        daemon_version: "0.0.0".to_string(),
        now_ms: 1_000,
        uuids: std::sync::Arc::new(SeqUuidV7::new()),
        write_queue_capacity: 8,
        payload_ttl_hours: 72,
        consolidation_lease_ms: LEASE_DURATION_MS,
        consolidation_renew_interval_ms: LEASE_RENEW_INTERVAL_MS,
        data_policy: local_rag_core::DataPolicy::LocalOnly,
        supported_proto: local_rag_protocol::SUPPORTED_PROTO_RANGE,
        max_open_shards: 8,
        embedder_provider,
        locks,
        query_embedder: None,
        memory_query_embedder: None,
        recall_token_budget: 1500,
        consolidation_batch_size: 20,
        consolidation_queue_threshold: 50,
        consolidation_poll_interval: std::time::Duration::from_millis(50),
    }
}

pub async fn start(layout: &StoreLayout) -> DaemonHandle {
    DaemonHandle::start(start_options(layout.clone()))
        .await
        .expect("start")
}

/// A blocking, WELCOME-completed test client — a single persistent buffered
/// reader for the connection's whole lifetime (recreating one per read
/// would silently drop whatever an earlier read already buffered but did
/// not consume). Mirrors `daemon::handshake`'s own test idiom (a
/// `spawn_blocking` std client against a real listener).
pub struct Client {
    stream: StdUnixStream,
    reader: StdBufReader<StdUnixStream>,
    session_id: String,
}

impl Client {
    pub fn connect(socket_path: &Path) -> Self {
        Self::connect_with_session(socket_path, "sess-1")
    }

    /// Like [`Client::connect`], but with a caller-chosen `session_id` —
    /// carried both in the HELLO handshake and in every request's
    /// `RequestContext` (T19-05's own per-session `tools/call` counters key
    /// on exactly this value), so two `Client`s connected with different
    /// ids are genuinely distinct sessions from the daemon's point of view.
    pub fn connect_with_session(socket_path: &Path, session_id: &str) -> Self {
        let stream = StdUnixStream::connect(socket_path).expect("connect");
        let reader = StdBufReader::new(stream.try_clone().expect("clone stream"));
        let mut client = Client {
            stream,
            reader,
            session_id: session_id.to_string(),
        };
        client.write(&Message::Hello(Hello {
            proto: 1,
            proxy_version: "0.0.0".to_string(),
            session_id: client.session_id.clone(),
            worktree_root: None,
            harness: "claude-code".to_string(),
        }));
        match client.read() {
            Some(Message::Welcome(_)) => {}
            other => panic!("expected Welcome, got {other:?}"),
        }
        client
    }

    pub fn write(&mut self, msg: &Message) {
        let bytes = local_rag_protocol::encode_message(msg).expect("encode message");
        self.stream.write_all(&bytes).expect("write message");
    }

    pub fn read(&mut self) -> Option<Message> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).ok()?;
        if n == 0 {
            return None;
        }
        local_rag_protocol::decode_message(line.trim_end()).ok()
    }

    /// Send one MCP JSON-RPC line, wrapped in a `RequestEnvelope` carrying
    /// `worktree_root`.
    pub fn call(&mut self, mcp_json: &str, worktree_root: Option<&str>) {
        let mcp = RawValue::from_string(mcp_json.to_string()).expect("valid json");
        let context = RequestContext {
            session_id: self.session_id.clone(),
            worktree_root: worktree_root.map(str::to_string),
            repo_hint: None,
        };
        self.write(&Message::Request(RequestEnvelope { context, mcp }));
    }

    /// `call` + `read`, unwrapping the MCP JSON-RPC response body.
    pub fn call_and_read(
        &mut self,
        mcp_json: &str,
        worktree_root: Option<&str>,
    ) -> serde_json::Value {
        self.call(mcp_json, worktree_root);
        match self.read() {
            Some(Message::Response(env)) => {
                serde_json::from_str(env.mcp.get()).expect("valid JSON-RPC response")
            }
            other => panic!("expected a Response, got {other:?}"),
        }
    }
}
