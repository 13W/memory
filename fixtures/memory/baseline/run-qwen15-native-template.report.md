# Memory-router benchmark run

## Run provenance

| Field | Value |
| --- | --- |
| Commit | `9e0baea` |
| Corpus | `fixtures/memory/index.json` (v1.0.0, 42 cases) |
| Model | `qwen2.5-1.5b-instruct-gguf-q4km` |
| Sampling | greedy |
| Router version | v0 |
| Host | aarch64-macos |

## Metrics

| Metric | Value |
| --- | --- |
| Precision | 0.3571 |
| Recall | 0.3409 |
| F1 | 0.3488 |
| Exact match rate | 0.3333 |

## Latency

| Stage | ms |
| --- | --- |
| install | 0 |
| load | 321 |
| route p50 | 525.945 |
| route p95 | 1001.704 |

## Per-case

| id | expected | predicted | correct |
| --- | --- | --- | --- |
| memory.router.op.create-decision-en-clean | create | router output still malformed after one corrective re-prompt: router output did not parse as a JSON ops array: missing field `scope_kind` at line 1 column 159 | no |
| memory.router.op.create-decision-ru-clean | create | create | yes |
| memory.router.op.create-convention-en-clean | create | propose_candidate | no |
| memory.router.op.create-convention-ru-clean | create | propose_candidate | no |
| memory.router.op.create-procedure-en-clean | create | noop | no |
| memory.router.op.create-procedure-ru-clean | create | noop | no |
| memory.router.op.create-task-en-clean | create | noop | no |
| memory.router.op.create-task-ru-clean | create | noop | no |
| memory.router.op.create-hypothesis-en-brainstorm | create | noop | no |
| memory.router.op.create-hypothesis-ru-brainstorm | create | noop | no |
| memory.router.op.create-question-en | create | propose_candidate | no |
| memory.router.op.create-question-ru | create | noop | no |
| memory.router.op.propose-candidate-model-claim-fact-en | propose_candidate | propose_candidate | yes |
| memory.router.op.propose-candidate-model-claim-fact-ru | propose_candidate | propose_candidate | yes |
| memory.router.op.propose-candidate-model-claim-decision-en | propose_candidate | propose_candidate | yes |
| memory.router.op.propose-candidate-model-claim-decision-ru | propose_candidate | propose_candidate | yes |
| memory.router.op.propose-candidate-negation-no-target-en | propose_candidate | noop | no |
| memory.router.op.propose-candidate-negation-no-target-ru | propose_candidate | router output still malformed after one corrective re-prompt: router output did not parse as a JSON ops array: missing field `scope_kind` at line 1 column 224 | no |
| memory.router.op.propose-candidate-temporary-suggestion-en | propose_candidate | noop | no |
| memory.router.op.propose-candidate-temporary-suggestion-ru | propose_candidate | noop | no |
| memory.router.op.noop-routine-tool-result-en | noop | noop | yes |
| memory.router.op.noop-routine-tool-result-ru | noop | noop | yes |
| memory.router.op.noop-small-talk-en | noop | noop | yes |
| memory.router.op.noop-small-talk-ru | noop | router output still malformed after one corrective re-prompt: router output did not parse as a JSON ops array: missing field `scope_kind` at line 1 column 195 | no |
| memory.router.op.retract-existing-en | retract | retract | yes |
| memory.router.op.retract-existing-ru | retract | retract | yes |
| memory.router.op.reinforce-existing-en | reinforce | retract | no |
| memory.router.op.reinforce-existing-ru | reinforce | retract,create | no |
| memory.router.op.reinforce-vs-create-duplicate-en | reinforce | retract | no |
| memory.router.op.supersede-existing-en | supersede | retract | no |
| memory.router.op.supersede-existing-ru | supersede | retract | no |
| memory.router.op.resolve-existing-task-en | resolve | retract | no |
| memory.router.op.resolve-existing-task-ru | resolve | retract | no |
| memory.router.op.resolve-existing-question-en | resolve | retract | no |
| memory.router.op.resolve-existing-question-ru | resolve | retract,create | no |
| memory.router.op.code-switch-ru-en-decision | create | router output still malformed after one corrective re-prompt: router output did not parse as a JSON ops array: missing field `scope_kind` at line 1 column 201 | no |
| memory.router.op.code-switch-en-ru-convention | create | create | yes |
| memory.router.op.code-switch-ru-en-noop | noop | noop | yes |
| memory.router.op.multi-observation-mixed-window-en | noop,create | propose_candidate,propose_candidate | no |
| memory.router.op.multi-observation-mixed-window-ru | noop,create | noop,noop | no |
| memory.router.op.adversarial-code-state-alone-en | propose_candidate | propose_candidate | yes |
| memory.router.op.adversarial-code-state-alone-ru | propose_candidate | propose_candidate | yes |
