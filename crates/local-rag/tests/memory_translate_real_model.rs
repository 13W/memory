//! T21-04: the translator against the real local model, env-gated.
//!
//! The unit tests in `local_rag_memory::normalize::translate` drive a scripted
//! generator: they pin every rejection branch and the injection defence, but by
//! construction they never learn whether a real model's answer survives
//! validation. This test does exactly that, and nothing more — it asserts the
//! **contract**, not translation quality: a `NonLatin` entry comes back
//! `Translated`, in Latin script, within the length band the validator
//! enforces.
//!
//! It lives here rather than in `crates/memory` on purpose: this crate already
//! depends on `local-rag-generate`, while `crates/memory` deliberately does not
//! (see its own module doc's "why this crate never depends on
//! local-rag-generate"), and adding it there would make every
//! `cargo test -p local-rag-memory` build `llama-cpp-sys-2`.
//!
//! Without `LOCAL_RAG_TEST_MODEL_HOME` it prints a loud SKIP and passes — no
//! `#[ignore]`, so it is visible in an ordinary run rather than silently
//! filtered out (the same convention `cli_rebuild.rs`'s `with_real_model`
//! module already uses).

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;

use local_rag_core::config::DataPolicy;
use local_rag_core::paths::StoreLayout;
use local_rag_embed::{GeneratorEntry, GeneratorPool};
use local_rag_memory::normalize::detect::{ScriptClass, script_class};
use local_rag_memory::normalize::translate::{
    MAX_LENGTH_RATIO, MIN_LENGTH_RATIO, TranslateRequest, Translation, translate,
};

/// Two entries in the shape this store actually holds: prose with identifiers,
/// paths and a commit hash embedded in it.
const SOURCES: &[&str] = &[
    "Для фьюжна поиска остановились на RRF с k=45 вместо линейной комбинации весов, \
     потому что веса приходилось перекалибровывать после каждой смены модели",
    "Правил apply_run в crates/store/src/memory/runner.rs: дедупликация цитат на границе \
     недоверенного ввода, коммит cf50a5c",
];

fn require_model_home() -> Option<StoreLayout> {
    let Ok(model_home) = std::env::var("LOCAL_RAG_TEST_MODEL_HOME") else {
        eprintln!(
            "SKIP: LOCAL_RAG_TEST_MODEL_HOME is unset — point it at a store root whose \
             models/{} is installed to run the real-model translation test.",
            local_rag_generate::DEFAULT_MODEL_ID
        );
        return None;
    };
    let layout = StoreLayout::new(PathBuf::from(&model_home));
    let dir = layout.model_dir(local_rag_generate::DEFAULT_MODEL_ID);
    if !dir.join(".ok").is_file() {
        eprintln!(
            "SKIP: {} does not hold an installed {} (no .ok marker).",
            dir.display(),
            local_rag_generate::DEFAULT_MODEL_ID
        );
        return None;
    }
    Some(layout)
}

fn real_pool(layout: &StoreLayout) -> GeneratorPool {
    let entry = local_rag_generate::find(local_rag_generate::DEFAULT_MODEL_ID)
        .expect("the default generator is in the catalog");
    let generator =
        local_rag_generate::LlamaGenerator::open(layout, entry).expect("open the real model");
    GeneratorPool::new(vec![GeneratorEntry::local(
        entry.model_id,
        Arc::new(generator),
    )])
}

#[test]
fn the_real_model_produces_a_translation_the_validator_accepts() {
    let Some(layout) = require_model_home() else {
        return;
    };
    let pool = real_pool(&layout);

    for source in SOURCES {
        assert_eq!(
            script_class(source),
            ScriptClass::NonLatin,
            "the fixture must be worth translating in the first place",
        );

        let outcome = translate(
            &pool,
            DataPolicy::LocalOnly,
            TranslateRequest {
                memory_id: "real-model-probe",
                text: source,
            },
        );

        let Ok(Translation::Translated { english }) = outcome else {
            panic!("the real model's answer must survive validation: {outcome:?}");
        };
        eprintln!("[real model] {source}\n         ->  {english}");

        assert_eq!(
            script_class(&english),
            ScriptClass::English,
            "the accepted answer must be Latin script: {english:?}",
        );
        let ratio = english.chars().count() as f64 / source.chars().count() as f64;
        assert!(
            (MIN_LENGTH_RATIO..=MAX_LENGTH_RATIO).contains(&ratio),
            "length ratio {ratio:.2} outside the band for {english:?}",
        );
    }
}
