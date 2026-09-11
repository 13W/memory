; YAML section query set — query_version = 1 (ADR-0015, ADR-0002).
;
; Captures section NODES as @decl.<lang_kind>; names and signatures are extracted in
; Rust (a key that is not a safe path segment yields an ordinal anchor rather than
; dropping the unit). YAML declares no imports, so this set has no @ref row.
; Bumping this set is a `queries=` rebuild event.
;
; These units are `config_section`, NOT `symbol` (ADR-0015 Decision 3), and the
; adapter overrides `parent_unit_kinds` so they may parent one another — the seam
; T24-04 opened.
;
; WHY THIS EXISTS. The universal line scanner (`parse::universal::config_key`) names
; sections by top-level key lines and knows nothing about `---`, so a three-document
; k8s manifest came out as one flat list — `apiVersion, kind, metadata, spec,
; apiVersion, kind` — in which two documents' identical keys are indistinguishable.
; A `document` unit per document is what separates them.
;
; ONLY STRUCTURAL KEYS ARE UNITS (owner decision, T24-05): a `block_mapping_pair`
; becomes a section when its value is itself a mapping or a sequence, at any depth.
; A leaf scalar (`name: app`) stays as text inside its enclosing section. YAML nests
; far deeper than TOML — where T24-04 made every pair a unit — and a leaf is a field
; rather than a section: on this repository's `release.yml` the rule is ~20 units
; instead of 143 over 304 lines, while ADR-0015's own `key:spec/key:containers`
; stays reachable. The rule MUST live here rather than in Rust: `classify_capture`
; is fixed per capture name, so an adapter cannot decline to emit a unit for a node
; the query matched.

(document) @decl.document

(block_mapping_pair value: (block_node (block_mapping))) @decl.key
(block_mapping_pair value: (block_node (block_sequence))) @decl.key
