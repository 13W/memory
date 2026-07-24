# Группа 11 — Embeddings и model spaces

Цель: immutable representation keys, recoverable cache и double-buffer migration. Ссылки:
spec 03 §2.2/§4.2; 04 §3; 05 §8; 10; 12 §1; 15 O3.

## T11-01 — Representation/model-space registry

- **Результат:** typed canonical RepresentationKey, normalized membership, required coverage
  recomputation and legal build states; default must reference active model space.
- **Тесты:** six-field uniqueness; duplicate serialization converges; coverage per required kind;
  incomplete cannot projection_ready/target; retiring cannot become target.

## T11-02 — Embedding cache integrity и eviction

- **Результат:** per-kind domain subject hashes, dimensions/checksum validation, little-endian
  f32, bounded batched last_used, budget LRU with active/rebuild pins.
- **Тесты:** hash golden each kind; checksum/dimension corrupt row deleted/recomputed; eviction
  honors pins/budget; content shares across occurrences while context does not; cache loss safe.

## T11-03 — Local embedder provider pool

- **Результат:** ADR closes embedding part of O3; in-process default implements Embedder;
  retry/primary/fallback and central policy hook; no required Ollama/network.
- **Тесты:** deterministic model fixture; batch/error/retry; local_only never selects remote;
  fallback ordering; dimensions/key match registry; offline smoke.

## T11-04 — Resumable coverage backfill

- **Результат:** bounded worker computes missing expected subjects per representation, reuses
  valid rows and reports recomputable expected/ready/failed.
- **Тесты:** crash each batch resumes; already cached not re-embedded; failure doesn't inflate
  ready; concurrent request dedup; full required set gates projection_ready.

## T11-05 — Per-worktree model switch

- **Результат:** production model-axis uses standard projection switch, then updates default;
  dormant retiring/absent worktree migrates on open; old rows pinned until no refs.
- **Тесты:** no global barrier; two worktrees migrate independently; crash retains all-old or
  all-new per worktree; generation/model switches serialize; different dimensions not in-place.

## T11-06 — Model asset installer

- **Результат:** manifest/license/checksum, `.part→fsync→rename→.ok`, resumable safe init and
  fully offline reopen; no weights in npm.
- **Тесты:** bad checksum, interrupted download, existing valid asset, missing `.ok`, offline
  launch; platform path. Network tests use local fixture server only.
- **Добавлено D-008 (T11-03):** здесь же реализуется **in-process ONNX-провайдер выбранной
  ADR-0004 модели** (`embeddinggemma-300m`, 768d, cosine) — выбор рантайма `fastembed` vs
  `Candle` (spec 10 §1) принадлежит этой карточке, поскольку связывать рантайм раньше весов
  нечем. Консьюмерская половина контракта (`local_rag_embed::require_model_assets`, `.ok`-маркер,
  типизированный `ModelAssetsMissing`) уже существует с T11-03 — провайдер обязан ходить через
  неё, а не проверять файлы сам. Инсталлятор обязан показать и сохранить лицензию модели
  (Gemma Terms of Use — не OSI) в `models/embeddinggemma-300m/manifest.json` (spec 10 §5).
- **Тесты (добавлено D-008):** провайдер отдаёт `ModelAssetsMissing` без единого сетевого вызова,
  когда `.ok` отсутствует; после установки — offline-инференс, чьи `key()` и длина векторов
  совпадают с зарегистрированным `representation` (тот же контракт, что проверяет пул T11-03).

## G11 — Сверка model migration

Перечитать spec 04 §3, 05 §8, 10, 12 §1. Проверить no in-place mutation, required coverage,
cache reconstructibility, per-worktree rollback and local default. O3 decisions must cite model
quality/license/platform evidence; unresolved generator half remains visible until T14-07.
