; Rust symbol/reference query set — query_version = 1 (ADR-0002).
;
; A separate set from the JS/TS queries: the Rust grammar's declaration node kinds
; are its own (function_item, struct_item, impl_item, …), so a shared query would
; not compile. Captures declaration NODES as @decl.<lang_kind>; names and signatures
; are extracted in Rust code (an anonymous/computed name yields an ordinal anchor
; rather than dropping the unit). `use` paths are captured as @ref.use and refined
; in Rust (`use` vs `pub use`). Bumping this set is a `queries=` rebuild event.

; --- item declarations (matched at any nesting) ---
(function_item) @decl.function
(function_signature_item) @decl.function

(struct_item) @decl.struct
(enum_item) @decl.enum
(union_item) @decl.union

(trait_item) @decl.trait
(impl_item) @decl.impl
(mod_item) @decl.mod

(const_item) @decl.const
(static_item) @decl.static
(type_item) @decl.type_alias

(macro_definition) @decl.macro

; --- use declarations (unresolved references) ---
(use_declaration) @ref.use
