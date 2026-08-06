//! `local-rag init [--download-models]` (spec 11 §6, D-013).
//!
//! Registration is gated on **disk state**, not on the `--download-models`
//! flag: both bare `init` and `init --download-models` attempt to register
//! the `code_raw` representation whenever the default model is already
//! installed (`.ok` marker present), and both skip registration — printing a
//! hint instead — when it is not. D-013's own card asks "for the installed
//! model", not "for the flag", and gating on disk state is what makes a
//! repeated `init` genuinely idempotent: there is no separate "did I already
//! register this run" flag to get out of sync with what is actually on disk.
//!
//! `register_representation`'s own `ON CONFLICT` on the six-field key (T11-01)
//! already makes registration itself idempotent; running this twice converges
//! on the same `representation_id` rather than creating a second row.

use std::process::ExitCode;

use local_rag_core::identity::{SystemUuidV7, UuidSource};
use local_rag_embed::Embedder;
use local_rag_models::{
    DEFAULT_MODEL_ID, HttpFetcher, OnnxEmbedder, find, install_model, is_installed,
};
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, RepresentationKey, RepresentationKind, StateDb, WriteError,
    register_representation, set_model_space_representation,
};

use super::{block_on, fail, resolve_layout_and_config, system_now_ms};

const BIN: &str = "local-rag";

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Fetch and install the default embedding model's weights before
    /// checking whether `code_raw` can be registered.
    #[arg(long)]
    download_models: bool,
}

pub fn run(args: InitArgs) -> ExitCode {
    let download = args.download_models;

    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    // The daemon's own startup does `ensure → open` (spec 02 §4.1); `init` is
    // routinely the very first command run against a store `serve` has never
    // touched, so it needs the identical ordering before anything below
    // opens `state.sqlite`.
    if let Err(e) = layout.ensure() {
        return fail(BIN, &format!("could not prepare the store directory: {e}"));
    }

    let Some(entry) = find(DEFAULT_MODEL_ID) else {
        return fail(
            BIN,
            &format!("model {DEFAULT_MODEL_ID:?} is not in this build's catalog"),
        );
    };

    if download
        && let Err(e) = install_model(&layout, entry, &HttpFetcher::new(), &mut std::io::stdout())
    {
        return fail(BIN, &format!("could not install {DEFAULT_MODEL_ID}: {e}"));
    }

    if !is_installed(&layout, DEFAULT_MODEL_ID) {
        println!(
            "{BIN}: {DEFAULT_MODEL_ID} is not installed yet; run `local-rag init \
             --download-models` to fetch it. Until then, search_code/recall stay \
             lexical-only."
        );
        return ExitCode::SUCCESS;
    }

    let embedder = match OnnxEmbedder::open(&layout, entry) {
        Ok(embedder) => embedder,
        Err(e) => {
            return fail(
                BIN,
                &format!("{DEFAULT_MODEL_ID} is installed but could not be opened: {e}"),
            );
        }
    };

    let state = match StateDb::open(layout.state_db()) {
        Ok(state) => state,
        Err(e) => return fail(BIN, &format!("could not open state.sqlite: {e}")),
    };

    let key = embedder.key();
    let representation_id = SystemUuidV7.next_uuid().to_string();
    let now_ms = system_now_ms();

    match block_on(register_code_raw_representation(
        &state,
        key,
        representation_id,
        now_ms,
    )) {
        Ok(id) => {
            println!("{BIN}: registered code_raw representation {id} for {DEFAULT_MODEL_ID}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(BIN, &format!("could not register the representation: {e}")),
    }
}

/// Register `key` as the default model space's `code_raw` representation,
/// returning its `representation_id`.
///
/// Split out from [`run`] so the registration itself — the part D-013 tests —
/// is exercisable without an installed model or the ONNX runtime, the same
/// separation `local_rag_models::tests::onnx` draws between "policy" tests
/// (always run) and "real inference" tests (env-gated, see
/// `tests/cli_init.rs`).
pub(crate) async fn register_code_raw_representation(
    state: &StateDb,
    key: RepresentationKey,
    representation_id: String,
    now_ms: i64,
) -> Result<String, WriteError> {
    state
        .writer()
        .transaction(move |tx| {
            let id = register_representation(tx, &representation_id, &key, now_ms)?;
            set_model_space_representation(
                tx,
                DEFAULT_MODEL_SPACE_ID,
                RepresentationKind::CodeRaw,
                &id,
                true,
                now_ms,
            )?;
            Ok(id)
        })
        .await
}

#[cfg(test)]
mod tests {
    use local_rag_core::paths::StoreLayout;
    use local_rag_store::{DistanceMetric, representation_key};
    use local_rag_test_support::TempHome;

    use super::*;

    fn fixture_key(dimensions: u32) -> RepresentationKey {
        RepresentationKey {
            kind: RepresentationKind::CodeRaw,
            representation_version: 1,
            normalization_version: 1,
            model_id: "fixture-model".to_string(),
            dimensions,
            distance_metric: DistanceMetric::Cosine,
        }
    }

    fn open_state() -> (TempHome, StateDb) {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        (home, state)
    }

    #[tokio::test]
    async fn after_registration_the_default_space_requires_code_raw() {
        let (_home, state) = open_state();
        register_code_raw_representation(&state, fixture_key(8), "rep-a".to_string(), 1_000)
            .await
            .expect("register");

        let conn = state.open_read().expect("read connection");
        let coverage: (String, i64) = conn
            .query_row(
                "SELECT representation_id, required FROM model_space_representation \
                 WHERE model_space_id = ?1 AND representation_kind = 'code_raw'",
                [DEFAULT_MODEL_SPACE_ID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("membership row exists");
        assert_eq!(
            coverage.1, 1,
            "code_raw must be required, not merely present"
        );
        assert!(!coverage.0.is_empty());
    }

    #[tokio::test]
    async fn the_registered_key_matches_the_installed_providers_key() {
        let (_home, state) = open_state();
        let key = fixture_key(768);
        let id = register_code_raw_representation(&state, key.clone(), "rep-b".to_string(), 1_000)
            .await
            .expect("register");

        let conn = state.open_read().expect("read connection");
        let stored = representation_key(&conn, &id)
            .expect("read back")
            .expect("row exists");
        assert_eq!(stored, key);
    }

    #[tokio::test]
    async fn params_for_model_space_reports_the_registered_dimensions() {
        let (_home, state) = open_state();
        register_code_raw_representation(&state, fixture_key(512), "rep-c".to_string(), 1_000)
            .await
            .expect("register");

        let conn = state.open_read().expect("read connection");
        let model_space_id: local_rag_core::identity::Uuid = DEFAULT_MODEL_SPACE_ID
            .parse()
            .expect("default model space id parses");
        let params = local_rag_projection::params_for_model_space(&conn, &model_space_id)
            .expect("a code_raw representation is now registered");
        assert_eq!(params.dimensions, 512);
        assert_eq!(params.distance_metric, DistanceMetric::Cosine);
    }

    #[tokio::test]
    async fn repeated_registration_is_idempotent() {
        let (_home, state) = open_state();
        let key = fixture_key(8);
        let first =
            register_code_raw_representation(&state, key.clone(), "attempt-1".to_string(), 1_000)
                .await
                .expect("first registration");
        let second = register_code_raw_representation(&state, key, "attempt-2".to_string(), 2_000)
            .await
            .expect("second registration");
        assert_eq!(first, second, "the same key must converge on one row");

        let conn = state.open_read().expect("read connection");
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM representation", [], |r| r.get(0))
            .expect("count");
        assert_eq!(rows, 1, "no duplicate representation row was created");
    }

    #[test]
    fn bare_init_without_download_models_is_a_light_no_op() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        assert!(!is_installed(&layout, DEFAULT_MODEL_ID));

        // No process spawn needed here: exercising exactly the disk-state check
        // `run` itself makes before ever touching the network or ONNX.
        assert!(
            find(DEFAULT_MODEL_ID).is_some(),
            "the default model must be catalogued for this check to mean anything"
        );
    }

    #[test]
    fn init_rejects_an_unknown_argument() {
        use clap::Parser;
        let result = crate::cli::Cli::try_parse_from(["local-rag", "init", "--bogus"]);
        assert!(result.is_err(), "{result:?}");
    }
}
