# @13w/local-rag-darwin-arm64

Native `local-rag`/`local-rag-proxy`/`local-rag-hook` binaries for darwin-arm64 (macOS, Apple
Silicon).

Do not install this package directly — install `@13w/local-rag` instead; it pulls this package in
automatically as an `optionalDependency` on a matching host and resolves it at run time.

Ships no model weights (see `docs/adr/0004-default-embedding-model.md`,
`docs/adr/0005-model-delivery.md` — weights are fetched separately via
`local-rag init --download-models`).
