# Memory-router benchmark run

## Run provenance

| Field | Value |
| --- | --- |
| Commit | `9e0baea` |
| Corpus | `fixtures/memory/index.json` (v1.0.0, 42 cases) |
| Model | `qwen2.5-0.5b-instruct-gguf-q4km` |
| Sampling | greedy |
| Router version | v0 |
| Host | aarch64-macos |

## Metrics

| Metric | Value |
| --- | --- |
| Precision | 0.3590 |
| Recall | 0.3182 |
| F1 | 0.3373 |
| Exact match rate | 0.3095 |

## Latency

| Stage | ms |
| --- | --- |
| install | 0 |
| load | 253 |
| route p50 | 272.781 |
| route p95 | 6612.380 |

## Per-case

| id | expected | predicted | correct |
| --- | --- | --- | --- |
| memory.router.op.create-decision-en-clean | create | propose_candidate | no |
| memory.router.op.create-decision-ru-clean | create | propose_candidate | no |
| memory.router.op.create-convention-en-clean | create | propose_candidate | no |
| memory.router.op.create-convention-ru-clean | create | propose_candidate | no |
| memory.router.op.create-procedure-en-clean | create | noop | no |
| memory.router.op.create-procedure-ru-clean | create | propose_candidate | no |
| memory.router.op.create-task-en-clean | create | noop | no |
| memory.router.op.create-task-ru-clean | create | propose_candidate | no |
| memory.router.op.create-hypothesis-en-brainstorm | create | propose_candidate | no |
| memory.router.op.create-hypothesis-ru-brainstorm | create | propose_candidate | no |
| memory.router.op.create-question-en | create | propose_candidate | no |
| memory.router.op.create-question-ru | create | router output still malformed after one corrective re-prompt: router output did not parse as a JSON ops array: missing field `scope_kind` at line 1 column 177 | no |
| memory.router.op.propose-candidate-model-claim-fact-en | propose_candidate | noop | no |
| memory.router.op.propose-candidate-model-claim-fact-ru | propose_candidate | propose_candidate | yes |
| memory.router.op.propose-candidate-model-claim-decision-en | propose_candidate | propose_candidate | yes |
| memory.router.op.propose-candidate-model-claim-decision-ru | propose_candidate | propose_candidate | yes |
| memory.router.op.propose-candidate-negation-no-target-en | propose_candidate | noop | no |
| memory.router.op.propose-candidate-negation-no-target-ru | propose_candidate | noop | no |
| memory.router.op.propose-candidate-temporary-suggestion-en | propose_candidate | noop | no |
| memory.router.op.propose-candidate-temporary-suggestion-ru | propose_candidate | propose_candidate | yes |
| memory.router.op.noop-routine-tool-result-en | noop | noop | yes |
| memory.router.op.noop-routine-tool-result-ru | noop | noop | yes |
| memory.router.op.noop-small-talk-en | noop | noop | yes |
| memory.router.op.noop-small-talk-ru | noop | noop | yes |
| memory.router.op.retract-existing-en | retract | router output still malformed after one corrective re-prompt: router output did not parse as a JSON ops array: EOF while parsing a string at line 23 column 45 | no |
| memory.router.op.retract-existing-ru | retract | propose_candidate | no |
| memory.router.op.reinforce-existing-en | reinforce | propose_candidate | no |
| memory.router.op.reinforce-existing-ru | reinforce | propose_candidate,noop | no |
| memory.router.op.reinforce-vs-create-duplicate-en | reinforce | propose_candidate,noop,noop | no |
| memory.router.op.supersede-existing-en | supersede | router output still malformed after one corrective re-prompt: router output did not parse as a JSON ops array: EOF while parsing a string at line 21 column 63 | no |
| memory.router.op.supersede-existing-ru | supersede | router output still malformed after one corrective re-prompt: router output did not parse as a JSON ops array: missing field `scope_kind` at line 3 column 2 | no |
| memory.router.op.resolve-existing-task-en | resolve | resolve | yes |
| memory.router.op.resolve-existing-task-ru | resolve | resolve | yes |
| memory.router.op.resolve-existing-question-en | resolve | router output still malformed after one corrective re-prompt: router output did not parse as a JSON ops array: missing field `scope_kind` at line 3 column 2 | no |
| memory.router.op.resolve-existing-question-ru | resolve | router output still malformed after one corrective re-prompt: router output did not parse as a JSON ops array: missing field `scope_kind` at line 3 column 2 | no |
| memory.router.op.code-switch-ru-en-decision | create | noop | no |
| memory.router.op.code-switch-en-ru-convention | create | propose_candidate | no |
| memory.router.op.code-switch-ru-en-noop | noop | noop | yes |
| memory.router.op.multi-observation-mixed-window-en | noop,create | noop | no |
| memory.router.op.multi-observation-mixed-window-ru | noop,create | propose_candidate | no |
| memory.router.op.adversarial-code-state-alone-en | propose_candidate | propose_candidate | yes |
| memory.router.op.adversarial-code-state-alone-ru | propose_candidate | propose_candidate | yes |
