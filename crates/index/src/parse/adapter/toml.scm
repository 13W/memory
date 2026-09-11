; TOML section query set — query_version = 1 (ADR-0015, ADR-0002).
;
; Captures section NODES as @decl.<lang_kind>; names and signatures are extracted in
; Rust (a key that is not a safe path segment yields an ordinal anchor rather than
; dropping the unit). TOML declares no imports, so this set has no @ref row at all.
; Bumping this set is a `queries=` rebuild event.
;
; These units are `config_section`, NOT `symbol` (ADR-0015 Decision 3): a TOML key is
; not a symbol, and calling it one to fit the language path would be a lie told for
; implementation convenience. The engine takes `unit_kind` straight from the adapter,
; so no new unit kind is invented and spec 06 §2.1's five-way set is untouched.
;
; WHY THIS EXISTS AT ALL. The universal line scanner (`parse::universal::config_key`)
; recognizes only `key:` and `key =` lines, so a table header is never a section
; start: a real `Cargo.toml` came out as `name, version, serde, name`, with
; `[package]`, `[dependencies]` and `[[bin]]` invisible. A grammar decides the
; boundary instead.
;
; EVERY PAIR IS A UNIT, at any depth (owner decision, T24-04). A `table` node's span
; covers its own pairs (measured), so a key inside a table nests under it by span
; containment and `[dependencies] serde = "1"` is reachable as
; `table:dependencies/key:serde` — which is why this card is also the one that opens
; the engine's parent seam (`LanguageSpec::parent_unit_kinds`). An inline table's
; pairs are `pair` nodes too, so they nest one level further
; (`key:tokio/key:features`); that is consistent rather than special-cased.
;
; A sub-table is NOT nested: `[a.b]` is a sibling of `[a]` in the grammar, named by
; its `dotted_key` (`table:a.b`). `[bin]` and `[[bin]]` are different constructs and
; get different `lang_kind`s so their routes say which one they are.

(table) @decl.table
(table_array_element) @decl.table_array
(pair) @decl.key
