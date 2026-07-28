# Memory-router benchmark run

## Run provenance

| Field | Value |
| --- | --- |
| Commit | `e49f585` |
| Corpus | `fixtures/memory/index.json` (v1.0.0, 42 cases) |
| Model | `gemma-4-e2b-it-gguf-q4-0` |
| Sampling | greedy |
| Router version | v0 |
| Host | aarch64-macos |

## Metrics

| Metric | Value |
| --- | --- |
| Precision | 0.6667 |
| Recall | 0.6364 |
| F1 | 0.6512 |
| Exact match rate | 0.6190 |

## Latency

| Stage | ms |
| --- | --- |
| install | 0 |
| load | 1030 |
| route p50 | 819.560 |
| route p95 | 924.173 |

## Per-case

| id | expected | predicted | correct |
| --- | --- | --- | --- |
| memory.router.op.create-decision-en-clean | create | create | yes |
| memory.router.op.create-decision-ru-clean | create | create | yes |
| memory.router.op.create-convention-en-clean | create | create | yes |
| memory.router.op.create-convention-ru-clean | create | create | yes |
| memory.router.op.create-procedure-en-clean | create | create | yes |
| memory.router.op.create-procedure-ru-clean | create | create | yes |
| memory.router.op.create-task-en-clean | create | create | yes |
| memory.router.op.create-task-ru-clean | create | create | yes |
| memory.router.op.create-hypothesis-en-brainstorm | create | propose_candidate | no |
| memory.router.op.create-hypothesis-ru-brainstorm | create | create | yes |
| memory.router.op.create-question-en | create | propose_candidate | no |
| memory.router.op.create-question-ru | create | propose_candidate | no |
| memory.router.op.propose-candidate-model-claim-fact-en | propose_candidate | propose_candidate | yes |
| memory.router.op.propose-candidate-model-claim-fact-ru | propose_candidate | propose_candidate | yes |
| memory.router.op.propose-candidate-model-claim-decision-en | propose_candidate | propose_candidate | yes |
| memory.router.op.propose-candidate-model-claim-decision-ru | propose_candidate | propose_candidate | yes |
| memory.router.op.propose-candidate-negation-no-target-en | propose_candidate | create | no |
| memory.router.op.propose-candidate-negation-no-target-ru | propose_candidate | create | no |
| memory.router.op.propose-candidate-temporary-suggestion-en | propose_candidate | propose_candidate | yes |
| memory.router.op.propose-candidate-temporary-suggestion-ru | propose_candidate | propose_candidate | yes |
| memory.router.op.noop-routine-tool-result-en | noop | propose_candidate | no |
| memory.router.op.noop-routine-tool-result-ru | noop | propose_candidate | no |
| memory.router.op.noop-small-talk-en | noop | noop | yes |
| memory.router.op.noop-small-talk-ru | noop | noop | yes |
| memory.router.op.retract-existing-en | retract | retract | yes |
| memory.router.op.retract-existing-ru | retract | retract | yes |
| memory.router.op.reinforce-existing-en | reinforce | reinforce | yes |
| memory.router.op.reinforce-existing-ru | reinforce | reinforce | yes |
| memory.router.op.reinforce-vs-create-duplicate-en | reinforce | reinforce | yes |
| memory.router.op.supersede-existing-en | supersede | retract | no |
| memory.router.op.supersede-existing-ru | supersede | retract | no |
| memory.router.op.resolve-existing-task-en | resolve | reinforce | no |
| memory.router.op.resolve-existing-task-ru | resolve | reinforce | no |
| memory.router.op.resolve-existing-question-en | resolve | reinforce | no |
| memory.router.op.resolve-existing-question-ru | resolve | reinforce | no |
| memory.router.op.code-switch-ru-en-decision | create | create | yes |
| memory.router.op.code-switch-en-ru-convention | create | create | yes |
| memory.router.op.code-switch-ru-en-noop | noop | propose_candidate | no |
| memory.router.op.multi-observation-mixed-window-en | noop,create | create | no |
| memory.router.op.multi-observation-mixed-window-ru | noop,create | create | no |
| memory.router.op.adversarial-code-state-alone-en | propose_candidate | propose_candidate | yes |
| memory.router.op.adversarial-code-state-alone-ru | propose_candidate | propose_candidate | yes |
