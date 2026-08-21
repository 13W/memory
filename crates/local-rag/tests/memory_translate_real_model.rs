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

/// T21-15: how long a *query*-sized translation actually takes on the real
/// model, measured rather than assumed.
///
/// This exists because `RECALL_BUDGET` had to move and the new number must come
/// from data. ADR-0010's "≈ 800 ms (p50)" was measured for the **router**,
/// whose prompt carries a whole observation window plus recall candidates; a
/// user prompt is one or two sentences, so reusing that figure would have set
/// the budget from the wrong workload.
///
/// Not an assertion: hardware varies, and a test that fails on a slower machine
/// would be a flaky gate rather than a measurement. The number it prints is
/// what the budget is justified by, and that justification lives in the
/// evidence, not in a threshold here.
#[test]
fn measure_query_sized_translation_latency() {
    let Some(layout) = require_model_home() else {
        return;
    };
    let pool = real_pool(&layout);

    // Prompt-shaped, not entry-shaped: what a user actually types before
    // Claude Code starts working.
    let queries = [
        "почему для поиска выбрали именно такое значение k у RRF-фьюжна",
        "где в демоне обрабатывается перезапуск консолидации после падения",
        "покажи что мы решили про кэш нормализованного текста",
    ];

    let mut samples = Vec::new();
    for query in queries {
        // One warm-up per query is deliberate: the first call after opening the
        // model pays for weights the later ones do not, and the budget governs
        // a daemon that has been running, not one starting up.
        for round in 0..2 {
            let started = std::time::Instant::now();
            let outcome = translate(
                &pool,
                DataPolicy::LocalOnly,
                TranslateRequest {
                    memory_id: "budget-probe",
                    text: query,
                },
            );
            let elapsed = started.elapsed();
            if round == 1 {
                samples.push(elapsed.as_millis());
                eprintln!(
                    "[budget-probe] {} ms — {:?}",
                    elapsed.as_millis(),
                    outcome.map(|t| match t {
                        Translation::Translated { english } => english,
                        Translation::Passthrough { class } => format!("passthrough {class:?}"),
                    })
                );
            }
        }
    }

    samples.sort_unstable();
    eprintln!(
        "QUERY_TRANSLATION_MS min={} median={} max={}",
        samples.first().copied().unwrap_or_default(),
        samples[samples.len() / 2],
        samples.last().copied().unwrap_or_default(),
    );
}

/// T21-16: how often the translator refuses on entries shaped like the ones
/// that actually broke it, measured before and after grammar-constrained
/// decoding.
///
/// The fixtures are **synthetic on purpose**. The card asked for "the owner's
/// real entries that it failed on", and those exist — but committing somebody's
/// private notes to a repository to serve as a test fixture publishes them.
/// What broke the single-line JSON envelope was never the content: it was the
/// shape — a thousand-plus characters of prose carrying quotes, backslashes,
/// newlines and code fragments, every one of which the model had to escape
/// correctly to produce parseable output. These reproduce that shape.
#[test]
fn measure_long_entry_refusal_rate() {
    let Some(layout) = require_model_home() else {
        return;
    };
    let pool = real_pool(&layout);

    let mut refused = Vec::new();
    for (i, source) in HOSTILE.iter().enumerate() {
        assert_eq!(
            script_class(source),
            ScriptClass::NonLatin,
            "fixture {i} must be worth translating in the first place",
        );
        match translate(
            &pool,
            DataPolicy::LocalOnly,
            TranslateRequest {
                memory_id: "refusal-rate-probe",
                text: source,
            },
        ) {
            Ok(Translation::Translated { english }) => {
                eprintln!("[hostile {i}] ok ({} chars)", english.chars().count());
            }
            Ok(Translation::Passthrough { class }) => {
                eprintln!("[hostile {i}] passthrough {class:?}");
            }
            Err(e) => {
                eprintln!("[hostile {i}] REFUSED: {e}");
                refused.push(i);
            }
        }
    }

    eprintln!(
        "HOSTILE_REFUSAL_RATE {}/{} refused={refused:?}",
        refused.len(),
        HOSTILE.len(),
    );
}

/// Long Russian prose carrying exactly the features that made the envelope
/// tear: embedded double quotes, backslashes, newlines, and code fragments.
///
/// **Length is the point.** The first attempt at this corpus used ~450-character
/// sources and every one of them translated cleanly, which proved nothing: the
/// observed failures were on entries of 1183+ characters, tearing at output
/// columns 1136 and 1892. A fixture that does not reach that size does not
/// reproduce the defect, and a fix validated against it would be a fix
/// validated against nothing.
const HOSTILE: [&str; 5] = [
    "Решение по консолидации, записанное после живого инцидента: если роутер вернул окно, где \
     один и тот же \"observation_id\" встречается дважды, то apply_run обязан дедуплицировать \
     цитаты с сохранением порядка, иначе ограничение PRIMARY KEY candidate_evidence срабатывает \
     детерминированно и ран уходит в бесконечный ретрай. Замерено на стенде владельца: 581 \
     попытка примерно по пятнадцать секунд локальной модели на одном окне из трёх наблюдений, \
     это около шести часов работы GPU за сутки, и всё это время демон выглядел здоровым, потому \
     что ни stats, ни doctor не показывали залипший ран вообще. Классификация обязана быть \
     Mechanical, а не Transient, и next_retry_at при этом не выставляется вовсе, потому что \
     ждать тут нечего: тот же текст на той же сборке даст тот же отказ. Отдельно важно, что \
     дедлеттер ключуется fingerprint'ом сборки, поэтому пересборка честно даёт запаркованному \
     рану ровно одну новую попытку, а не бесконечный кредит, и это правило пришлось чинить \
     дважды: сначала оно вовсе отсутствовало, а потом оказалось, что стартовый resume-свип и \
     первый тик триггера читают stale_runs одновременно и выдают две попытки вместо одной.",
    "Заметка про идентичность путей, которую легко нарушить новой миграцией. Колонка \
     normalized_path это сам путь, использованный как естественный ключ принадлежности \
     поколению, а вовсе не хэш от пути, и спецификация разрешает ровно один внешний ключ через \
     него: generation_unit_occurrence(generation_id, normalized_path) -> generation_file. Всё \
     остальное, что выглядит как \\\\path\\\\to\\\\file или как C:\\\\Users\\\\zero, \
     обязано жить только в наблюдательных леджерах, и schema_audit проверяет это автоматически \
     на каждой новой миграции, обходя все таблицы свежемигрированного стора. Правило \
     формулируется так: ни один долговечный идентификатор не выводится из пути файловой \
     системы, а path-derived хэш допустим исключительно как ключ поиска, например \
     worktree_path.path_fingerprint, и никогда как цель внешнего ключа для долговечного \
     состояния. Практическое следствие в том, что переименование каталога воркдерева не должно \
     ломать ничего вообще, а идентичность воркдерева остаётся стабильным UUID, выданным при \
     регистрации, и переживает и переезд, и повторное клонирование.",
    "Про кэш и его границы, чтобы больше не спорить. Файл cache.sqlite никогда не мигрируется: \
     при смене версии он просто сбрасывается и пересобирается целиком, и отсюда следует главное \
     правило — ничего канонического в нём не хранить, даже если очень хочется и даже если это \
     кажется временным. Проверять расхождение удобно так:\nsqlite3 cache.sqlite \"select \
     count(*) from embedding_cache where subject_kind='memory_entry'\"\nи сравнивать с \
     ожидаемым набором субъектов, который считает state.sqlite. Если числа разъезжаются, виноват \
     backfill, а не кэш, и чинить надо ожидаемый набор. Отдельная ловушка здесь в том, что \
     partition_expected обходит только ожидаемые ключи и удаляет строки исключительно по \
     признаку повреждения, а перечисления строк, которых больше никто не ждёт, в системе нет \
     вовсе, поэтому осиротевшие векторы копятся и уходят только при вытеснении по LRU или при \
     полной пересборке, то есть недетерминированно и без всякого сигнала наружу.",
    "Наблюдение про хуки и порядок операций, важное для восстановления после падений. Хуки \
     пишут только в durable spool append и никогда не отправляют ингест демону напрямую — это \
     зафиксированное решение, переоткрывать его не надо. Практическое следствие в том, что при \
     упавшем демоне не теряется ничего: сегменты лежат на диске, и resume-проход импортирует их \
     при следующем старте. Но там есть тонкость, стоившая нам отдельной девиации: разбор \
     рабочего каталога первого фрейма идёт через RootResolver, который шелится в git, а \
     субпроцесс нельзя запускать под write-локом стора, поэтому резолвер обязан вызываться \
     ровно один раз на батч и строго ДО открытия транзакции записи. Пока это правило \
     нарушалось, 21985 строк observation_envelope из 21986 имели repo_id равный NULL, и \
     guard::resolve_scope_owner молча ронял каждую операцию роутера со скоупом repository или \
     worktree в Noop, хотя все примеры в промпте роутера используют именно repository.",
    "Решение про отчётность, принятое после того как doctor оказался бесполезно красным. Он \
     обязан возвращать ненулевой код выхода только на том, что владелец действительно может \
     починить. Выключенный переключатель это осознанный выбор, неустановленная модель это \
     состояние установки, и ни то ни другое не двигает код выхода — они печатаются, и всё. А \
     вот залипший консолидационный ран, поколение, собранное но так и не активированное, и \
     запись, на которой нормализатор сдался, это работа, которую система начала и бросила, и \
     вот она обязана быть красной. Из этого же следует, что у каждого такого состояния должен \
     быть путь наружу: у консолидации лекарство наступает само при пересборке, потому что \
     дедлеттер ключуется fingerprint'ом сборки, а у нормализации — при смене версии \
     нормализатора. Если пути наружу нет, красный код выхода превращается в постоянный шум, на \
     который перестают смотреть, и тогда он хуже, чем его отсутствие.",
];

/// T21-16: run the translator against the entries a real store already gave up
/// on, and report only verdicts.
///
/// The synthetic corpus above did not reproduce the defect even at the right
/// length, which is itself the finding: what tore the envelope was content, not
/// size alone. Reproducing it therefore needs the real entries — and those are
/// somebody's private notes, so this probe reads them from whatever store it is
/// pointed at, prints **identifiers, lengths and verdicts only**, and commits
/// nothing. Measure, do not publish.
///
/// Gated on `LOCAL_RAG_TEST_STORE_HOME` (the store to probe) in addition to
/// `LOCAL_RAG_TEST_MODEL_HOME` (where the weights live); without it, a loud SKIP.
#[test]
fn probe_dead_lettered_entries_of_a_real_store() {
    let Some(model_layout) = require_model_home() else {
        return;
    };
    let Ok(store_home) = std::env::var("LOCAL_RAG_TEST_STORE_HOME") else {
        eprintln!(
            "SKIP: LOCAL_RAG_TEST_STORE_HOME is unset — point it at a store whose \
             memory_text_normalization holds `failed` rows to re-probe them."
        );
        return;
    };
    let layout = StoreLayout::new(PathBuf::from(&store_home));
    let db = match local_rag_store::StateDb::open(layout.state_db()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("SKIP: could not open {store_home}: {e}");
            return;
        }
    };
    let read = db.open_read().expect("read connection");

    let mut stmt = read
        .prepare(
            "SELECT n.memory_id, e.text FROM memory_text_normalization n \
             JOIN memory_entry e ON e.memory_id = n.memory_id \
             WHERE n.status = 'failed' ORDER BY length(e.text) DESC",
        )
        .expect("prepare");
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect");

    if rows.is_empty() {
        eprintln!("PROBE: no `failed` rows in {store_home} — nothing to reproduce.");
        return;
    }

    let pool = real_pool(&model_layout);
    let mut refused = 0usize;
    for (memory_id, text) in &rows {
        let chars = text.chars().count();
        match translate(
            &pool,
            DataPolicy::LocalOnly,
            TranslateRequest { memory_id, text },
        ) {
            Ok(Translation::Translated { .. }) => {
                eprintln!("[probe] {memory_id} ({chars} chars): TRANSLATED");
            }
            Ok(Translation::Passthrough { class }) => {
                eprintln!("[probe] {memory_id} ({chars} chars): passthrough {class:?}");
            }
            Err(e) => {
                refused += 1;
                // The reason is the model's complaint about its own output, not
                // the entry — safe to print, and the whole point of the probe.
                eprintln!("[probe] {memory_id} ({chars} chars): REFUSED — {e}");
            }
        }
    }
    eprintln!("PROBE_REFUSAL_RATE {}/{}", refused, rows.len());
}
