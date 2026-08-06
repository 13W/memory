//! The in-process ONNX embedding provider (spec 10 §1 `[FIXED]`, D-008).
//!
//! Spec 10 §1 fixes that embeddings run **in-process** via `fastembed` (ONNX
//! Runtime) or `Candle`; ADR-0005 picks ONNX Runtime through `ort` with the
//! `load-dynamic` feature. That feature is the load-bearing detail: nothing is
//! downloaded or linked at build time, and `libonnxruntime` is resolved at
//! runtime, so `cargo xtask ci` keeps running offline — the objection D-008
//! raised against `ort`'s default `download-binaries`.
//!
//! # Assets come through the consumer contract, not from here
//!
//! [`OnnxEmbedder::open`] locates weights **only** via
//! `local_rag_embed::require_model_assets`, which returns the directory when its
//! `.ok` marker is present and `EmbedError::ModelAssetsMissing` otherwise
//! (spec 10 §5). This type therefore performs no filesystem policy of its own
//! and, by construction, no network access: a missing model is a typed error,
//! never an implicit download.
//!
//! # Runtime requirements
//!
//! Loading a session needs the ONNX Runtime shared library on the host.
//! [`ensure_ort_initialized`] resolves it, once per process: an explicit
//! `ORT_DYLIB_PATH` always wins, else a file bundled next to the running
//! executable (T17-03) — the same flat, no-manifest convention
//! `local-rag-proxy::connect::resolve_daemon_binary_path` uses to find its
//! daemon. Either way this module calls `ort::init_from` on that path
//! itself, on a watchdog thread; it never lets `ort` discover
//! `ORT_DYLIB_PATH` on its own, and when neither an override nor a bundled
//! file exists, [`OnnxEmbedder::open_dir`] returns a typed
//! [`OnnxError::Runtime`] without calling into `ort` at all. See
//! [`resolve_ort_source`]'s and [`ensure_ort_initialized`]'s doc comments
//! for why both of those are load-bearing rather than defensive
//! redundancy — `ort` 2.0.0-rc.12 can hang instead of erroring when its own
//! lazy setup fails.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use local_rag_embed::{EmbedError, EmbedRequest, Embedder, Vector};
use local_rag_store::RepresentationKey;
use ndarray::{Array2, Axis};
use ort::session::Session;
use ort::value::TensorRef;
use tokenizers::Tokenizer;

use crate::catalog::ModelCatalogEntry;

/// Tokens per sequence; longer inputs are truncated.
///
/// Raised from 256 to 1024 by D-016. The 256 that ADR-0004 measured with was
/// silently cutting embedded text about three times more aggressively than the
/// v1 baseline this project is measured against, which truncated at 3000
/// *characters* (`scripts/benchmark.ts::MODEL_CONFIGS`) — roughly 750–1000 tokens
/// of code. Comparing retrieval quality across that gap measures the truncation,
/// not the retrieval. 1024 covers v1's window with room to spare and still sits
/// at half of EmbeddingGemma's 2048-token context.
///
/// The window is **not** one of the six `RepresentationKey` fields, so changing
/// it would otherwise produce different vectors under an unchanged
/// `representation_id` and let `embedding_cache` serve incomparable rows as
/// valid. `ModelCatalogEntry::representation_key`'s `representation_version` is
/// bumped in the same change for exactly that reason.
pub const MAX_SEQUENCE_TOKENS: usize = 1024;

/// The name a sentence-transformers ONNX export gives the output that *is* the
/// model's embedding: mean pooling, the trained Dense projection modules, and
/// normalization, all inside the graph.
///
/// Selecting it by name is the whole point of [`select_output`] — see there for
/// what taking the first output instead cost (D-017).
pub const POOLED_OUTPUT: &str = "sentence_embedding";

/// Why the ONNX provider could not be created.
#[derive(Debug)]
#[non_exhaustive]
pub enum OnnxError {
    /// The model's assets are not installed (no `.ok` marker) — spec 10 §5.
    Assets(EmbedError),
    /// The ONNX Runtime library or the session could not be loaded.
    Runtime(String),
    /// `tokenizer.json` could not be loaded.
    Tokenizer(String),
}

impl fmt::Display for OnnxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OnnxError::Assets(e) => write!(f, "{e}"),
            OnnxError::Runtime(e) => write!(
                f,
                "ONNX Runtime unavailable ({e}); install the runtime or set ORT_DYLIB_PATH"
            ),
            OnnxError::Tokenizer(e) => write!(f, "could not load the model tokenizer: {e}"),
        }
    }
}

impl std::error::Error for OnnxError {}

impl From<OnnxError> for EmbedError {
    fn from(e: OnnxError) -> Self {
        match e {
            // An asset error is already the typed one the pool understands.
            OnnxError::Assets(inner) => inner,
            // Everything else is a provider that cannot serve at all: permanent
            // for this provider, so the pool falls back rather than retrying.
            other => EmbedError::permanent(other.to_string()),
        }
    }
}

/// An `Embedder` backed by a locally installed ONNX model.
pub struct OnnxEmbedder {
    key: RepresentationKey,
    tokenizer: Tokenizer,
    /// `ort` sessions are not `Sync`; the pool calls `embed` from one worker at
    /// a time, and the mutex makes that structurally true rather than assumed.
    session: Mutex<Session>,
    model_dir: PathBuf,
    /// Which graph output carries the embedding, resolved once at open time by
    /// [`select_output`] and then addressed **by name** on every run.
    output_name: String,
}

impl OnnxEmbedder {
    /// Open the provider for `entry`, reading assets from the store layout.
    ///
    /// Returns `ModelAssetsMissing` (through [`OnnxError::Assets`]) when the
    /// model has not been installed — without touching the network.
    pub fn open(
        layout: &local_rag_core::paths::StoreLayout,
        entry: &ModelCatalogEntry,
    ) -> Result<Self, OnnxError> {
        let dir = local_rag_embed::require_model_assets(layout, entry.model_id)
            .map_err(OnnxError::Assets)?;
        Self::open_dir(&dir, entry)
    }

    /// Open the provider against an already-resolved asset directory.
    ///
    /// Split out so a caller that has already validated the directory (or a test
    /// working against a fixture) does not repeat the marker check; the
    /// marker-checking entry point is [`OnnxEmbedder::open`].
    pub fn open_dir(dir: &Path, entry: &ModelCatalogEntry) -> Result<Self, OnnxError> {
        Self::open_dir_with_key(dir, entry, entry.representation_key())
    }

    /// Open the provider for `entry`'s `memory` representation (D-036) — same
    /// on-disk model as [`Self::open`], a second `ort` session tagged with
    /// [`ModelCatalogEntry::memory_representation_key`] instead of
    /// [`ModelCatalogEntry::representation_key`]. `kind` gates identity only
    /// (`Embedder::embed`'s own check); tokenization/inference are identical.
    pub fn open_for_memory(
        layout: &local_rag_core::paths::StoreLayout,
        entry: &ModelCatalogEntry,
    ) -> Result<Self, OnnxError> {
        let dir = local_rag_embed::require_model_assets(layout, entry.model_id)
            .map_err(OnnxError::Assets)?;
        Self::open_dir_with_key(&dir, entry, entry.memory_representation_key())
    }

    /// Shared body of [`Self::open_dir`]/[`Self::open_for_memory`]: everything
    /// but which [`RepresentationKey`] the resulting embedder identifies as.
    fn open_dir_with_key(
        dir: &Path,
        entry: &ModelCatalogEntry,
        key: RepresentationKey,
    ) -> Result<Self, OnnxError> {
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| OnnxError::Tokenizer(e.to_string()))?;

        let graph = entry
            .files
            .iter()
            .map(|f| f.relative_path)
            .find(|p| p.ends_with(".onnx"))
            .unwrap_or("model_quantized.onnx");

        // Checked here, immediately before the first `ort` API touch, not
        // earlier: a directory with a bad `tokenizer.json` must still fail
        // with `OnnxError::Tokenizer` regardless of whether a runtime is
        // available, per `a_marked_directory_gets_past_the_gate_and_fails_
        // on_the_asset_itself` (`crates/models/tests/onnx.rs`) — the gate
        // this function passes through is the asset marker, not the runtime.
        ensure_ort_initialized()?;

        let session = Session::builder()
            .and_then(|mut b| b.commit_from_file(dir.join(graph)))
            .map_err(|e| OnnxError::Runtime(e.to_string()))?;

        let names: Vec<&str> = session.outputs().iter().map(|o| o.name()).collect();
        let output_name = select_output(&names)
            .ok_or_else(|| OnnxError::Runtime(format!("{graph} declares no outputs")))?
            .to_string();

        Ok(OnnxEmbedder {
            key,
            tokenizer,
            session: Mutex::new(session),
            model_dir: dir.to_path_buf(),
            output_name,
        })
    }

    /// The directory the assets were loaded from.
    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }

    /// The graph output this provider reads its vectors from.
    ///
    /// Exposed so a run can *assert* which output it embedded with instead of
    /// assuming it: D-017 was exactly the case where the assumption was wrong
    /// and nothing said so.
    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    /// Tokenize a batch into the `(input_ids, attention_mask, token_type_ids)`
    /// tensors the encoder expects, padded to a common length.
    fn encode(&self, texts: &[String]) -> Result<(Array2<i64>, Array2<i64>), EmbedError> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| EmbedError::permanent(format!("tokenization failed: {e}")))?;

        let width = encodings
            .iter()
            .map(|e| e.get_ids().len().min(MAX_SEQUENCE_TOKENS))
            .max()
            .unwrap_or(1)
            .max(1);

        let mut ids = Array2::<i64>::zeros((encodings.len(), width));
        let mut mask = Array2::<i64>::zeros((encodings.len(), width));
        for (row, encoding) in encodings.iter().enumerate() {
            for (col, (&id, &attn)) in encoding
                .get_ids()
                .iter()
                .zip(encoding.get_attention_mask())
                .take(width)
                .enumerate()
            {
                ids[[row, col]] = id as i64;
                mask[[row, col]] = attn as i64;
            }
        }
        Ok((ids, mask))
    }
}

impl Embedder for OnnxEmbedder {
    fn embed(&self, req: EmbedRequest) -> Result<Vec<Vector>, EmbedError> {
        if req.kind != self.key.kind {
            return Err(EmbedError::permanent(format!(
                "provider serves representation kind {}, request asked for {}",
                self.key.kind.as_str(),
                req.kind.as_str()
            )));
        }
        if req.texts.is_empty() {
            return Ok(Vec::new());
        }

        let (ids, mask) = self.encode(&req.texts)?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| EmbedError::permanent("onnx session mutex poisoned"))?;

        let mut inputs: Vec<(&str, ort::session::SessionInputValue<'_>)> = Vec::new();
        let ids_tensor = TensorRef::from_array_view(&ids)
            .map_err(|e| EmbedError::permanent(format!("input_ids tensor: {e}")))?;
        let mask_tensor = TensorRef::from_array_view(&mask)
            .map_err(|e| EmbedError::permanent(format!("attention_mask tensor: {e}")))?;
        inputs.push(("input_ids", ids_tensor.into()));
        inputs.push(("attention_mask", mask_tensor.into()));

        let outputs = session
            .run(inputs)
            .map_err(|e| EmbedError::retryable(format!("onnx inference failed: {e}")))?;

        // The output chosen at open time, addressed by name — a graph may declare
        // several, and position says nothing about which one is the embedding.
        let value = outputs.get(&self.output_name).ok_or_else(|| {
            EmbedError::permanent(format!(
                "onnx session produced no `{}` output",
                self.output_name
            ))
        })?;
        let (shape, data) = value
            .try_extract_tensor::<f32>()
            .map_err(|e| EmbedError::permanent(format!("output tensor: {e}")))?;

        let vectors = pool(shape, data, &mask, self.key.dimensions as usize)?;
        Ok(vectors)
    }

    fn key(&self) -> RepresentationKey {
        self.key.clone()
    }
}

/// The bundled ONNX Runtime shared library's platform-specific file name —
/// the name every platform's official release archive gives it.
fn ort_dylib_file_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    }
}

/// Locate an ONNX Runtime shared library bundled next to `exe` — the same
/// flat, no-manifest convention `local-rag-proxy::connect::
/// resolve_daemon_binary_path` uses to find its daemon (one npm platform
/// package / `target/{debug,release}` directory, no separate lookup path).
///
/// `None` when no such file sits beside `exe`: a dev build, `cargo test`, or
/// a platform package that has not bundled a runtime. Callers treat that as
/// "no bundled default", not an error — `ort`'s own resolution still applies.
fn bundled_ort_dylib_path(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let candidate = dir.join(ort_dylib_file_name());
    candidate.is_file().then_some(candidate)
}

/// Decide which ONNX Runtime shared library [`ensure_ort_initialized`]
/// should explicitly load, given an explicit override (`ORT_DYLIB_PATH`,
/// read by the caller so this stays a plain function to unit-test without
/// touching process environment state) and the running executable's own
/// path.
///
/// `Ok(path)` — `dylib_path_override` when set, else a runtime bundled next
/// to `exe`. Either way the caller must go on to explicitly
/// `ort::init_from(path)` **itself**, rather than leaving `ORT_DYLIB_PATH`
/// to `ort`'s own env-var-driven lazy pickup inside `Session::builder()`:
/// that indirection means the *first* `ort` API touch in the process — and
/// therefore any hang inside it — happens somewhere this module does not
/// control. See [`ensure_ort_initialized`]'s doc comment for why that
/// distinction is load-bearing, not stylistic.
///
/// `Err` — neither is available. The message is raw (not yet wrapped in
/// [`OnnxError::Runtime`]'s `Display`) — [`ensure_ort_initialized`] is the
/// single place that applies that wrapping, so a caller never sees the
/// "ONNX Runtime unavailable (ONNX Runtime unavailable (...))" double wrap a
/// second `OnnxError::Runtime(err.to_string())` would produce.
fn resolve_ort_source(
    dylib_path_override: Option<&std::ffi::OsStr>,
    exe: &Path,
) -> Result<PathBuf, String> {
    if let Some(over) = dylib_path_override {
        return Ok(PathBuf::from(over));
    }
    bundled_ort_dylib_path(exe).ok_or_else(|| {
        format!(
            "no ONNX Runtime found: ORT_DYLIB_PATH is unset and no {} sits beside {}",
            ort_dylib_file_name(),
            exe.display()
        )
    })
}

/// How long [`ensure_ort_initialized`] waits for `ort::init_from` before
/// giving up on it as hung rather than merely slow. Real loads finish in
/// well under a second — G11's own measured loads were 300–800ms — so this
/// bound is generous on purpose, not tight.
const ORT_INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Point `ort` at `path`, once per process, before the first session is
/// created (`ort::init_from`'s own contract: it "must be called before any
/// other `ort` APIs are used") — or fail with a typed error before `ort` is
/// touched at all when [`resolve_ort_source`] found nothing to point at.
///
/// Memoized in a `OnceLock` rather than `std::sync::Once`, because unlike a
/// best-effort initializer this one has an outcome every caller after the
/// first needs: `ort::init_from`/`Session::builder()` may only run once per
/// process, so a later `OnnxEmbedder::open_dir` call must see the same
/// success-or-error the first one produced, not silently retry and hit the
/// same deadlock this function exists to avoid.
///
/// # Why the actual `ort::init_from` call runs on a watchdog thread
///
/// `ort` 2.0.0-rc.12's `load-dynamic` path does not fail fast when its setup
/// fails for *any* reason — constructing its own "could not load" error
/// re-enters the same `OnceLock` its lazy environment setup is already
/// inside, and the thread deadlocks on `Once::call_once_force` waiting for
/// itself. Confirmed twice, independently, sampling real builds: (1) no
/// library findable anywhere (`ORT_DYLIB_PATH` unset, nothing bundled) —
/// stuck in `ort::environment::current` → `ort::setup_api` →
/// `ort::error::Error::new_internal`; (2) a real, correctly-fetched,
/// checksum-verified `.dylib` that exists on disk but fails to `dlopen`
/// (observed with T17-03's `darwin-x64` bundle — ONNX Runtime's own
/// `osx-x86_64` release binaries are unsigned, and loading one under Rosetta
/// on this Apple Silicon dev machine hit exactly this) — stuck inside
/// `ort::init_from` itself, same `ort::error::Error::new_internal` →
/// `try_init_inner` → `Once::call_once_force` → `queue::wait` shape one
/// level deeper. Both are the same root cause (any setup failure's own
/// error-reporting path is reentrant), not two different bugs, and nothing
/// short of never calling into `ort` at all rules it out in general — a
/// future ONNX Runtime build could fail to load for a reason neither of
/// these two runs hit.
///
/// The only mitigation reachable from outside `ort`'s own source is a
/// timeout: run the call on its own thread and stop waiting after
/// [`ORT_INIT_TIMEOUT`]. On a timeout the spawned thread is abandoned
/// (deliberately leaked, not joined or killed — Rust has no thread-kill, and
/// the alternative is the caller hanging with it); [`ensure_ort_initialized`]
/// only ever spawns one such thread per process, via the `OnceLock`, so the
/// worst case is one permanently-blocked thread for the life of the daemon,
/// which is a bounded, known cost — not the unbounded hang this replaces.
fn ensure_ort_initialized() -> Result<(), OnnxError> {
    static INIT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        let dylib_path_override = std::env::var_os("ORT_DYLIB_PATH");
        let exe = std::env::current_exe().map_err(|e| {
            format!(
                "could not resolve the running executable's own path to look \
                 for a bundled ONNX Runtime: {e}"
            )
        })?;
        let path = resolve_ort_source(dylib_path_override.as_deref(), &exe)?;
        init_ort_with_timeout(path)
    })
    .clone()
    .map_err(OnnxError::Runtime)
}

/// The watchdog-guarded call itself — see [`ensure_ort_initialized`]'s doc
/// comment for why this cannot simply call `ort::init_from` inline.
fn init_ort_with_timeout(path: PathBuf) -> Result<(), String> {
    run_with_timeout(ORT_INIT_TIMEOUT, move || {
        ort::init_from(&path)
            .map(|builder| {
                builder.commit();
            })
            .map_err(|e| format!("failed to load the ONNX Runtime at {}: {e}", path.display()))
    })
    .unwrap_or_else(|| {
        Err(format!(
            "loading the ONNX Runtime did not finish within {ORT_INIT_TIMEOUT:?} \
             — treating it as hung rather than waiting forever (see \
             `ensure_ort_initialized`'s doc comment for why `ort` itself \
             cannot be trusted to report this as an error)"
        ))
    })
}

/// Run `f` on its own thread and wait up to `timeout` for it — `None` if it
/// does not finish in time. The thread is not joined or killed on a timeout
/// (Rust has no thread-kill); it is deliberately abandoned to keep running,
/// which is only sound because every caller here treats a `None` as a
/// terminal failure rather than something to retry, so at most one thread is
/// ever leaked per [`ensure_ort_initialized`] call site.
fn run_with_timeout<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    // Send failing (the receiver dropped, i.e. this already timed out) is
    // exactly the case this discards — nothing is waiting to hear about it.
    let spawned = std::thread::Builder::new().spawn(move || {
        let _ = tx.send(f());
    });
    if spawned.is_err() {
        return None;
    }
    rx.recv_timeout(timeout).ok()
}

/// Pick the graph output that carries the embedding.
///
/// [`POOLED_OUTPUT`] wins whenever the graph declares it, **regardless of its
/// position**; only a graph without it falls back to the first output, which
/// [`pool`] then mean-pools by rank.
///
/// This function exists because the position-based version of it was a defect
/// (D-017). EmbeddingGemma's export declares `last_hidden_state` first and
/// `sentence_embedding` second, so taking "the first output" silently skipped
/// the model's own trained Dense head (`st/dense_1` 768→3072, `st/dense_2`
/// 3072→768) and embedded into a space the model was never trained to produce.
/// Nothing downstream could notice: both outputs are 768-wide, both normalize
/// cleanly, and query and documents went through the same wrong path — so the
/// space stayed self-consistent and the only visible symptom was retrieval
/// quality. On the 49-query benchmark the dense leg scored MRR 0.4939 against
/// the v1 baseline's 0.6963, which ran the full pipeline through Ollama; reading
/// this output instead puts it at 0.7007.
fn select_output<'a>(names: &[&'a str]) -> Option<&'a str> {
    names
        .iter()
        .copied()
        .find(|name| *name == POOLED_OUTPUT)
        .or_else(|| names.first().copied())
}

/// Reduce an encoder output to one unit-length vector per input.
///
/// A `[batch, dims]` output is already pooled; a `[batch, tokens, dims]` one is
/// mean-pooled over the tokens the attention mask keeps — masked positions carry
/// padding, and averaging them in would make a vector depend on its batch-mates'
/// lengths.
fn pool(
    shape: &[i64],
    data: &[f32],
    mask: &Array2<i64>,
    dimensions: usize,
) -> Result<Vec<Vector>, EmbedError> {
    let batch = mask.len_of(Axis(0));
    let mut out = Vec::with_capacity(batch);

    match shape.len() {
        2 => {
            let dims = shape[1] as usize;
            if dims != dimensions {
                return Err(dimension_error(dimensions, dims));
            }
            for row in 0..batch {
                let start = row * dims;
                out.push(normalized(&data[start..start + dims]));
            }
        }
        3 => {
            let tokens = shape[1] as usize;
            let dims = shape[2] as usize;
            if dims != dimensions {
                return Err(dimension_error(dimensions, dims));
            }
            for row in 0..batch {
                let mut acc = vec![0.0_f32; dims];
                let mut kept = 0.0_f32;
                for token in 0..tokens {
                    if mask[[row, token]] == 0 {
                        continue;
                    }
                    let start = (row * tokens + token) * dims;
                    for (slot, value) in acc.iter_mut().zip(&data[start..start + dims]) {
                        *slot += *value;
                    }
                    kept += 1.0;
                }
                if kept > 0.0 {
                    for slot in acc.iter_mut() {
                        *slot /= kept;
                    }
                }
                out.push(normalized(&acc));
            }
        }
        other => {
            return Err(EmbedError::permanent(format!(
                "unexpected onnx output rank {other}"
            )));
        }
    }
    Ok(out)
}

fn dimension_error(expected: usize, actual: usize) -> EmbedError {
    EmbedError::permanent(format!(
        "model produced {actual}-dimensional vectors, the representation declares {expected}"
    ))
}

/// L2-normalize, so cosine distance (ADR-0004's metric) is a dot product.
fn normalized(values: &[f32]) -> Vector {
    let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        Vector::new(values.iter().map(|v| v / norm).collect())
    } else {
        Vector::new(values.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_test_support::home::TempHome;

    #[test]
    fn no_bundled_dylib_beside_the_executable_resolves_to_none() {
        // The common case for every dev build and `cargo test` binary: the
        // executable has no runtime sitting next to it, and the resolver
        // must say so rather than pointing at a file that doesn't exist.
        let home = TempHome::new().expect("temp home");
        let exe = home.join("local-rag");
        std::fs::write(&exe, b"not a real binary").expect("write fake exe");
        assert_eq!(bundled_ort_dylib_path(&exe), None);
    }

    #[test]
    fn a_dylib_beside_the_executable_is_found_by_its_platform_name() {
        let home = TempHome::new().expect("temp home");
        let exe = home.join("local-rag");
        std::fs::write(&exe, b"not a real binary").expect("write fake exe");
        let dylib = home.join(ort_dylib_file_name());
        std::fs::write(&dylib, b"not a real runtime").expect("write fake dylib");
        assert_eq!(bundled_ort_dylib_path(&exe), Some(dylib));
    }

    #[test]
    fn an_explicit_override_always_wins_over_a_bundled_file() {
        let home = TempHome::new().expect("temp home");
        let exe = home.join("local-rag");
        std::fs::write(&exe, b"not a real binary").expect("write fake exe");
        // Even with a bundled file sitting right there, an explicit
        // `ORT_DYLIB_PATH` must not be second-guessed.
        std::fs::write(home.join(ort_dylib_file_name()), b"bundled").expect("write bundled");

        let override_value = std::ffi::OsStr::new("/somewhere/else/libonnxruntime.dylib");
        assert_eq!(
            resolve_ort_source(Some(override_value), &exe).expect("ok"),
            PathBuf::from("/somewhere/else/libonnxruntime.dylib")
        );
    }

    #[test]
    fn no_override_falls_back_to_the_bundled_file_when_present() {
        let home = TempHome::new().expect("temp home");
        let exe = home.join("local-rag");
        std::fs::write(&exe, b"not a real binary").expect("write fake exe");
        let dylib = home.join(ort_dylib_file_name());
        std::fs::write(&dylib, b"bundled").expect("write bundled");

        assert_eq!(resolve_ort_source(None, &exe).expect("ok"), dylib);
    }

    #[test]
    fn neither_an_override_nor_a_bundled_file_is_a_typed_error_not_a_hang() {
        // The regression this guards: `ort` 2.0.0-rc.12 deadlocks trying to
        // construct its own "library not found" error when `Session::
        // builder()` runs without anything ever having been initialized
        // first (see this function's doc comment for how that was
        // confirmed). `open_dir` must never reach `Session::builder()` in
        // this case — it must fail here, synchronously, with a message that
        // names what was missing.
        let home = TempHome::new().expect("temp home");
        let exe = home.join("local-rag");
        std::fs::write(&exe, b"not a real binary").expect("write fake exe");

        let message = resolve_ort_source(None, &exe).expect_err("nothing to find");
        assert!(message.contains("ORT_DYLIB_PATH"), "{message}");
        assert!(message.contains(ort_dylib_file_name()), "{message}");
    }

    #[test]
    fn run_with_timeout_returns_a_fast_result() {
        let result = run_with_timeout(Duration::from_secs(5), || 6 * 7);
        assert_eq!(result, Some(42));
    }

    #[test]
    fn run_with_timeout_gives_up_on_a_hung_closure_without_waiting_for_it() {
        // The regression this guards: `ort::init_from` can hang indefinitely
        // (see `ensure_ort_initialized`'s doc comment) — this asserts the
        // watchdog itself actually bounds the wait rather than trusting the
        // closure to cooperate. The closure here blocks forever on purpose
        // (an unfulfillable channel receive, not a sleep, so there is no way
        // for it to return even if this were mistakenly given more time).
        let started = std::time::Instant::now();
        let (_never_sent, forever) = std::sync::mpsc::channel::<()>();
        let result = run_with_timeout(Duration::from_millis(100), move || {
            let _ = forever.recv();
            "unreachable"
        });
        assert_eq!(result, None);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the watchdog must return near the timeout, not wait for the hung thread: {:?}",
            started.elapsed()
        );
    }

    fn mask(rows: usize, cols: usize, keep: usize) -> Array2<i64> {
        let mut m = Array2::<i64>::zeros((rows, cols));
        for row in 0..rows {
            for col in 0..keep {
                m[[row, col]] = 1;
            }
        }
        m
    }

    #[test]
    fn the_pooled_output_wins_even_when_the_graph_declares_it_last() {
        // EmbeddingGemma's own export order. Taking the first output here is
        // what D-017 fixed: it skips the trained Dense head.
        let chosen = select_output(&["last_hidden_state", "sentence_embedding"]);
        assert_eq!(chosen, Some("sentence_embedding"));
    }

    #[test]
    fn the_pooled_output_wins_from_any_position() {
        // Export order is not a contract, so neither is the fix's dependence on it.
        assert_eq!(
            select_output(&["sentence_embedding", "last_hidden_state"]),
            Some("sentence_embedding")
        );
        assert_eq!(
            select_output(&[
                "last_hidden_state",
                "token_embeddings",
                "sentence_embedding"
            ]),
            Some("sentence_embedding")
        );
    }

    #[test]
    fn a_graph_without_a_pooled_output_falls_back_to_token_states() {
        // Then `pool` mean-pools rank-3 states under the mask, as before.
        assert_eq!(
            select_output(&["last_hidden_state"]),
            Some("last_hidden_state")
        );
    }

    #[test]
    fn a_graph_with_no_outputs_selects_nothing() {
        // `open_dir` turns this into a typed `Runtime` error rather than a panic
        // on the first inference.
        assert_eq!(select_output(&[]), None);
    }

    #[test]
    fn a_pooled_output_is_normalized_as_is() {
        let m = mask(1, 4, 4);
        let out = pool(&[1, 3], &[3.0, 0.0, 4.0], &m, 3).expect("pool");
        assert_eq!(out.len(), 1);
        // 3-4-5 triangle: the unit vector is exactly (0.6, 0, 0.8).
        assert!((out[0].as_slice()[0] - 0.6).abs() < 1e-6);
        assert!((out[0].as_slice()[2] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn token_states_are_mean_pooled_under_the_mask() {
        // Two tokens kept, one masked: the masked token's huge values must not
        // reach the result.
        let m = mask(1, 3, 2);
        let data = vec![
            1.0, 0.0, // token 0 (kept)
            3.0, 0.0, // token 1 (kept)
            100.0, 100.0, // token 2 (masked)
        ];
        let out = pool(&[1, 3, 2], &data, &m, 2).expect("pool");
        // mean = (2, 0) → normalized = (1, 0)
        assert!((out[0].as_slice()[0] - 1.0).abs() < 1e-6, "{:?}", out[0]);
        assert!(out[0].as_slice()[1].abs() < 1e-6);
    }

    #[test]
    fn a_width_mismatch_is_a_typed_error_not_a_silent_truncation() {
        let m = mask(1, 2, 2);
        let err = pool(&[1, 5], &[0.0; 5], &m, 768).expect_err("mismatch");
        assert!(err.to_string().contains("768"), "{err}");
        assert!(
            !err.is_retryable(),
            "a wrong-width model will not fix itself"
        );
    }

    #[test]
    fn an_all_masked_row_stays_finite() {
        let m = mask(1, 2, 0);
        let out = pool(&[1, 2, 2], &[1.0, 2.0, 3.0, 4.0], &m, 2).expect("pool");
        assert!(out[0].as_slice().iter().all(|v| v.is_finite()));
    }
}
