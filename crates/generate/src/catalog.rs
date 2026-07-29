//! What a local generator model is made of: file, size, digest, license
//! (spec 10 §5 `[FIXED policy]` — not embedding-specific, mirrored from
//! `local_rag_models::catalog`).
//!
//! Unlike `local_rag_models::ModelCatalogEntry`, a GGUF model is normally
//! **one file** — the format embeds the vocabulary/merges/architecture
//! metadata that ONNX needed a separate `tokenizer.json` for. `AssetFile`
//! below still allows more than one (a future multi-part quantization),
//! kept structurally close to `local_rag_models::AssetFile` for the same
//! reason `install.rs`'s own doc explains: the shape is duplicated, not
//! shared, but deliberately kept parallel so the two installers stay easy to
//! compare.

/// One file of a model: where it comes from, how big it is, what it hashes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetFile {
    /// Path relative to the model directory (`models/<model_id>/`).
    pub relative_path: &'static str,
    /// Path relative to the source repository root.
    pub source_path: &'static str,
    /// Exact size in bytes.
    pub size: u64,
    /// Lowercase hex SHA-256 of the file's contents.
    pub sha256: &'static str,
}

/// A local generator model's complete asset set plus the metadata
/// `manifest.json` records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratorCatalogEntry {
    /// The id this installs under — also the directory name
    /// (`models/<model_id>/`).
    pub model_id: &'static str,
    /// Human-readable source (repository URL).
    pub source: &'static str,
    /// The pinned upstream revision.
    pub revision: &'static str,
    /// License name.
    pub license: &'static str,
    /// Where the license text lives.
    pub license_url: &'static str,
    /// The model's context window, in tokens (`LlamaContextParams::with_n_ctx`).
    pub context_length: u32,
    /// Files to install, in install order.
    pub files: &'static [AssetFile],
    /// A literal Jinja chat-template *source* to render instead of the
    /// GGUF's own embedded `tokenizer.chat_template` metadata (T14-09; see
    /// `crate::chat_template` for the renderer). `None` for every entry
    /// whose GGUF carries usable template metadata — which, since T14-09
    /// replaced `llama.cpp`'s fixed-signature template detector with a real
    /// Jinja interpreter (`minijinja`) rendering that raw metadata directly,
    /// is every entry in this catalog today, Gemma 4 included (see
    /// [`GEMMA4_E2B_IT_Q4_0`]'s own doc for that history). Reserved as a
    /// rare escape hatch for a future model whose GGUF conversion ships no
    /// `tokenizer.chat_template` metadata at all, or one `minijinja` cannot
    /// parse/render even after `crate::chat_template`'s own preprocessing.
    ///
    /// Before T14-09 this field held a *name* (`"chatml"`, `"gemma"`) into
    /// `llama-chat.cpp`'s `LLM_CHAT_TEMPLATES` map, forcing
    /// `llama_chat_apply_template`'s heuristic detector to a specific known
    /// template instead of auto-detecting one from the GGUF's own text — a
    /// different mechanism with the opposite failure mode (a name the
    /// detector already knows, vs. literal source text rendered directly).
    /// The field was renamed alongside that semantic change so old
    /// comments/history could not silently mislead a reader into thinking
    /// the meaning had stayed the same.
    pub raw_chat_template_override: Option<&'static str>,
}

impl GeneratorCatalogEntry {
    /// The download URL of `file` in this entry's source repository.
    ///
    /// HuggingFace's `resolve/<revision>/<path>` form, with the revision
    /// pinned so the bytes behind a URL never change — the same convention
    /// `local_rag_models::ModelCatalogEntry::url_for` uses.
    pub fn url_for(&self, file: &AssetFile) -> String {
        format!(
            "{}/resolve/{}/{}",
            self.source, self.revision, file.source_path
        )
    }

    /// Total bytes this entry will download (before any reuse).
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// The single GGUF file this entry installs (every catalog entry today
    /// has exactly one — see the module doc).
    pub fn gguf_file(&self) -> &'static AssetFile {
        self.files
            .first()
            .expect("every catalog entry has at least one file")
    }
}

/// The `main` branch at the time each entry's digests were captured — a
/// branch name, not a commit, because HuggingFace model repos do not expose
/// a stable non-`main` alias for a GGUF quantization the way a versioned
/// release tag would; each entry's pinned `sha256` is what actually makes
/// its download verifiable regardless of what `main` later points to.
const MAIN_REVISION: &str = "main";

const APACHE_2_0: &str = "Apache-2.0";

/// ADR-0006's selected default (revised, T14-07 Phase 7's real Gemma 4
/// comparison): `Gemma 4 E2B` — see [`GEMMA4_E2B_IT_Q4_0`]'s own doc for the
/// full measured rationale (roughly double the F1 of either Qwen2.5
/// candidate below) and its `chat_template_override` caveat.
pub const DEFAULT_MODEL_ID: &str = "gemma-4-e2b-it-gguf-q4-0";

const QWEN2_5_0_5B_INSTRUCT_Q4KM_FILES: &[AssetFile] = &[AssetFile {
    relative_path: "model.gguf",
    source_path: "qwen2.5-0.5b-instruct-q4_k_m.gguf",
    size: 491_400_032,
    sha256: "74a4da8c9fdbcd15bd1f6d01d621410d31c6fc00986f5eb687824e7b93d7a9db",
}];

/// ADR-0006's *original* selected default, superseded by [`GEMMA4_E2B_IT_Q4_0`]
/// once T14-07 Phase 7's real benchmark run measured the alternative — kept
/// catalogued (never removed) as a small, cheap-to-install comparison
/// candidate. Digests verified live against HuggingFace's tree API (Git LFS
/// `oid` *is* the file's SHA-256 — not assumed, confirmed by byte length and
/// format) rather than transcribed from a model card.
pub const QWEN2_5_0_5B_INSTRUCT_Q4KM: GeneratorCatalogEntry = GeneratorCatalogEntry {
    model_id: "qwen2.5-0.5b-instruct-gguf-q4km",
    source: "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF",
    revision: MAIN_REVISION,
    license: APACHE_2_0,
    license_url: "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/blob/main/LICENSE",
    context_length: 32_768,
    files: QWEN2_5_0_5B_INSTRUCT_Q4KM_FILES,
    raw_chat_template_override: None,
};

const QWEN2_5_1_5B_INSTRUCT_Q4KM_FILES: &[AssetFile] = &[AssetFile {
    relative_path: "model.gguf",
    source_path: "qwen2.5-1.5b-instruct-q4_k_m.gguf",
    size: 1_117_320_736,
    sha256: "6a1a2eb6d15622bf3c96857206351ba97e1af16c30d7a74ee38970e434e9407e",
}];

/// ADR-0006's comparison candidate: `Qwen2.5-1.5B-Instruct`, same
/// repository family and `q4_k_m` quantization as
/// [`QWEN2_5_0_5B_INSTRUCT_Q4KM`], so the two entries differ only in
/// parameter count — a fair A/B measured before the Gemma 4 comparison.
/// Digest verified live against HuggingFace's tree API the same way
/// [`QWEN2_5_0_5B_INSTRUCT_Q4KM`]'s was, at the point T14-07 Phase 5
/// actually measured it against the fixture corpus — not transcribed from a
/// model card, not a placeholder.
pub const QWEN2_5_1_5B_INSTRUCT_Q4KM: GeneratorCatalogEntry = GeneratorCatalogEntry {
    model_id: "qwen2.5-1.5b-instruct-gguf-q4km",
    source: "https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF",
    revision: MAIN_REVISION,
    license: APACHE_2_0,
    license_url: "https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/blob/main/LICENSE",
    context_length: 32_768,
    files: QWEN2_5_1_5B_INSTRUCT_Q4KM_FILES,
    raw_chat_template_override: None,
};

const GEMMA4_E2B_IT_Q4_0_FILES: &[AssetFile] = &[AssetFile {
    relative_path: "model.gguf",
    source_path: "gemma-4-E2B_q4_0-it.gguf",
    size: 3_349_516_256,
    sha256: "fa401b55b07ee70a54c6dae3903c783a6e65064312529ea57175cb5f8dec6634",
}];

/// ADR-0006's **revised default** (T14-07 Phase 7, after the user asked
/// whether Gemma could be used at all): `Gemma 4 E2B` (Google DeepMind),
/// Google's own official QAT `q4_0` GGUF release. Selected over both Qwen2.5
/// candidates above by a real, measured `cargo xtask memory-bench` run —
/// precision 0.6667 / recall 0.6364 / F1 0.6512 against Qwen2.5-0.5B's
/// 0.3784 / 0.3182 / 0.3457 and Qwen2.5-1.5B's 0.3659 / 0.3409 / 0.3529 on
/// the identical 42-case fixture corpus (`fixtures/memory/baseline/
/// run-gemma-4-e2b.json`) — roughly double the F1 of either Qwen candidate,
/// which is why the default moved despite Gemma 4 E2B being markedly larger
/// (~3.2 GiB vs. ~470 MiB). Digest verified live against HuggingFace's tree
/// API the same way the Qwen entries were.
///
/// Only the text GGUF is catalogued — the repository also ships a
/// `~941 MiB` `mmproj` vision-encoder file for Gemma 4's multimodal (image/
/// audio) input, which [`local_rag_generate::llama::LlamaGenerator`] has no
/// use for (spec 10 §1's `Generator` contract is text-in, text-out).
///
/// `context_length` is `32_768`, not the model's real 128K native window
/// (README-confirmed): `n_ctx` sizes the KV cache allocated on **every**
/// `LlamaGenerator::generate_greedy` call (one fresh context per call, no
/// reuse across calls today), so requesting the full 128K would allocate a
/// vastly oversized cache for every short router prompt this crate ever
/// sends — 32K matches the Qwen entries exactly (a fair, apples-to-apples
/// context budget for the ADR-0006 comparison) and is already generous
/// next to what `local_rag_memory::prompt`'s actual prompts need.
///
/// Note for whoever revisits this: HuggingFace gates `google/gemma-3-*`
/// (manual approval required — its tree API masks the LFS digest for an
/// unauthenticated request), so a Gemma 3 catalog entry could not be added
/// without either an approved account or accepting an unverified digest,
/// which this catalog does not do. `google/gemma-4-*` repositories are
/// **not** gated as of this entry's verification.
///
/// **History (T14-07/ADR-0006):** this entry originally shipped with
/// `chat_template_override: Some("gemma")` because real inference against
/// the installed weights failed outright (`ApplyChatTemplateError::
/// FfiError(-1)`) when handing the GGUF's own embedded Jinja template,
/// unmodified, to `llama_chat_apply_template`. That function's
/// `llm_chat_detect_template` pattern-matches a **fixed set** of known
/// template signatures against the raw Jinja string — it is not a Jinja
/// interpreter — and Gemma 4's own template text (a real, newer shape;
/// Gemma 4's README advertises native system-role support Gemma 1-3 lacked)
/// matched none of them, so detection fell through to
/// `LLM_CHAT_TEMPLATE_UNKNOWN` and hard-failed. The named override worked
/// around it by skipping detection entirely and forcing the older
/// `LLM_CHAT_TEMPLATE_GEMMA` (`google/gemma-7b-it`-era) format, whose own
/// implementation merges the system turn into the first user turn instead of
/// emitting it as its own turn — a real, disclosed quality cost, since it
/// could not exercise Gemma 4's actual native system-role template.
///
/// **As-built (T14-09):** `raw_chat_template_override` is now `None`. The
/// prompt is rendered by `crate::chat_template::render` — a real Jinja
/// interpreter (`minijinja`) applied directly to this GGUF's own embedded
/// template — bypassing `llm_chat_detect_template` altogether, so the
/// detector's signature list is no longer relevant to this entry at all.
/// Gemma 4's actual native template, system turn included, is what gets
/// rendered now. See `docs/adr/0006-local-router-runtime.md`'s Amendment
/// section for the real, measured effect on the memory-quality benchmark.
pub const GEMMA4_E2B_IT_Q4_0: GeneratorCatalogEntry = GeneratorCatalogEntry {
    model_id: DEFAULT_MODEL_ID,
    source: "https://huggingface.co/google/gemma-4-E2B-it-qat-q4_0-gguf",
    revision: MAIN_REVISION,
    license: APACHE_2_0,
    license_url: "https://ai.google.dev/gemma/docs/gemma_4_license",
    context_length: 32_768,
    files: GEMMA4_E2B_IT_Q4_0_FILES,
    raw_chat_template_override: None,
};

const PHI3_MINI_4K_INSTRUCT_Q4_FILES: &[AssetFile] = &[AssetFile {
    relative_path: "model.gguf",
    source_path: "Phi-3-mini-4k-instruct-q4.gguf",
    size: 2_393_231_072,
    sha256: "8a83c7fb9049a9b2e92266fa7ad04933bb53aa1e85136b7b30f1b8000ff2edef",
}];

/// T14-09's proof that the general chat-template mechanism (`crate::
/// chat_template::render`) needs no per-entry override: a fourth catalog
/// entry from a **third** template family (neither Qwen's ChatML nor
/// Gemma's `<start_of_turn>`), added with no `raw_chat_template_override`
/// at all — Microsoft's own official `q4` GGUF release of
/// `Phi-3-mini-4k-instruct` (MIT; live digest/gating/license verified the
/// same way ADR-0004/0005/0006 verified theirs — `gated: false` confirmed
/// against the real HuggingFace API response, license text fetched and
/// read, not transcribed from a model card). `context_length` is `4096`,
/// the model's real native window (its own name says so; no `n_ctx`
/// oversizing judgment call like [`GEMMA4_E2B_IT_Q4_0`]'s needed here).
///
/// **Disclosed, verified limitation of this specific template — not a
/// rendering-mechanism defect**: this GGUF's embedded chat template only
/// branches on `message['role'] in ('user', 'assistant')`; a `system`-role
/// turn (this router always sends one, `local_rag_memory::prompt`) matches
/// neither branch and is silently omitted from the rendered prompt. This
/// is Phi-3-mini-4k-instruct's own real, well-known template limitation
/// (confirmed against the live GGUF metadata, not assumed) — exactly the
/// kind of per-model quirk `raw_chat_template_override` exists to work
/// around *if* a project wanted this specific entry's quality improved
/// without a runtime change, but is not exercised here since fixing a
/// specific model's prompt quality is out of this task's scope (only the
/// mechanism is). `crate::chat_template::render` renders this template's
/// real logic faithfully either way, dropped system turn included; this is
/// what makes the fixture test in `crate::chat_template` a meaningful
/// end-to-end check rather than a hand-picked easy case.
pub const PHI3_MINI_4K_INSTRUCT_Q4: GeneratorCatalogEntry = GeneratorCatalogEntry {
    model_id: "phi-3-mini-4k-instruct-gguf-q4",
    source: "https://huggingface.co/microsoft/Phi-3-mini-4k-instruct-gguf",
    revision: MAIN_REVISION,
    license: "MIT",
    license_url: "https://huggingface.co/microsoft/Phi-3-mini-4k-instruct-gguf/blob/main/LICENSE",
    context_length: 4_096,
    files: PHI3_MINI_4K_INSTRUCT_Q4_FILES,
    raw_chat_template_override: None,
};

/// Every model this build knows how to install. [`GEMMA4_E2B_IT_Q4_0`] (the
/// default, `DEFAULT_MODEL_ID`) is listed first — `tests/llama.rs`'s
/// structural tests index `CATALOG[0]` as shorthand for "the default entry".
pub const CATALOG: &[GeneratorCatalogEntry] = &[
    GEMMA4_E2B_IT_Q4_0,
    QWEN2_5_0_5B_INSTRUCT_Q4KM,
    QWEN2_5_1_5B_INSTRUCT_Q4KM,
    PHI3_MINI_4K_INSTRUCT_Q4,
];

/// Look a model up by id.
pub fn find(model_id: &str) -> Option<&'static GeneratorCatalogEntry> {
    CATALOG.iter().find(|entry| entry.model_id == model_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_entry_is_catalogued_and_pins_a_real_digest() {
        let entry = find(DEFAULT_MODEL_ID).expect("the default model is catalogued");
        assert_eq!(entry.license, "Apache-2.0");
        assert_eq!(entry.context_length, 32_768);
        let file = entry.gguf_file();
        assert_eq!(file.sha256.len(), 64);
        assert!(
            file.sha256
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert!(file.size > 0);
    }

    #[test]
    fn every_catalogued_file_pins_a_digest_and_a_size() {
        for entry in CATALOG {
            assert!(!entry.files.is_empty(), "{} has no files", entry.model_id);
            for file in entry.files {
                assert_eq!(file.sha256.len(), 64);
                assert!(file.size > 0, "{}: size must be known", file.relative_path);
                assert!(
                    !file.relative_path.contains("..") && !file.relative_path.starts_with('/'),
                    "{}: relative_path escapes the model directory",
                    file.relative_path
                );
            }
        }
    }

    #[test]
    fn url_pins_the_revision() {
        let entry = &QWEN2_5_0_5B_INSTRUCT_Q4KM;
        let url = entry.url_for(entry.gguf_file());
        assert_eq!(
            url,
            "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/\
             qwen2.5-0.5b-instruct-q4_k_m.gguf"
        );
        assert!(url.starts_with("https://"));
    }

    #[test]
    fn the_1_5b_comparison_candidate_is_catalogued_distinctly_from_the_default() {
        let entry = find("qwen2.5-1.5b-instruct-gguf-q4km")
            .expect("the 1.5B comparison candidate is catalogued");
        assert_ne!(entry.model_id, DEFAULT_MODEL_ID);
        assert_eq!(entry.license, "Apache-2.0");
        assert_eq!(entry.context_length, 32_768);
        assert!(entry.total_bytes() > QWEN2_5_0_5B_INSTRUCT_Q4KM.total_bytes());
        assert_eq!(CATALOG.len(), 4);
    }

    #[test]
    fn the_gemma_4_default_is_catalogued_with_only_the_text_gguf_and_no_template_override() {
        let entry = find("gemma-4-e2b-it-gguf-q4-0").expect("Gemma 4 E2B is catalogued");
        assert_eq!(entry.model_id, DEFAULT_MODEL_ID);
        assert_eq!(entry.license, "Apache-2.0");
        assert_eq!(entry.context_length, 32_768);
        assert_eq!(
            entry.files.len(),
            1,
            "the mmproj vision-encoder file must not be catalogued -- text-only Generator"
        );
        assert_eq!(
            entry.raw_chat_template_override, None,
            "T14-09: minijinja renders Gemma 4's own embedded template directly, no override needed"
        );
    }

    #[test]
    fn the_qwen_0_5b_asset_budget_is_the_measured_q4km_size() {
        let mib = QWEN2_5_0_5B_INSTRUCT_Q4KM.total_bytes() as f64 / (1024.0 * 1024.0);
        assert!(
            (450.0..=500.0).contains(&mib),
            "unexpected budget: {mib:.1} MiB"
        );
    }

    #[test]
    fn the_fourth_family_entry_is_catalogued_distinctly_with_a_real_digest_and_no_override() {
        let entry =
            find("phi-3-mini-4k-instruct-gguf-q4").expect("Phi-3-mini-4k-instruct is catalogued");
        assert_ne!(entry.model_id, DEFAULT_MODEL_ID);
        assert_eq!(entry.license, "MIT");
        assert_eq!(entry.context_length, 4_096);
        assert_eq!(entry.files.len(), 1);
        let file = entry.gguf_file();
        assert_eq!(file.sha256.len(), 64);
        assert!(
            file.sha256
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert!(file.size > 0);
        assert_eq!(
            entry.raw_chat_template_override, None,
            "T14-09: proves the general mechanism needs no per-entry override for a third \
             template family (neither Qwen ChatML nor Gemma)"
        );
    }
}
