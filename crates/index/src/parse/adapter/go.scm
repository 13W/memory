; Go symbol/reference query set — query_version = 1 (ADR-0015, ADR-0002).
;
; Captures declaration NODES as @decl.<lang_kind>; names and signatures are
; extracted in Rust (a non-identifier name yields an ordinal anchor rather than
; dropping the unit). Import paths are captured as @ref.import and refined in
; Rust. Bumping this set is a `queries=` rebuild event.
;
; Rows match at any depth, the shape `bash.scm` settled on: parents are assigned
; by span containment in the engine, never by the query, so a `type Local struct{}`
; declared inside a function body routes under it as
; `function:Outer/type:Local` without the query saying anything about parents.
;
; THE SPEC, NOT THE DECLARATION, IS THE UNIT (owner decision, T24-03). Go groups
; declarations idiomatically, and `type_declaration`/`const_declaration` hold their
; `type_spec`/`const_spec` children directly while `var_declaration` wraps grouped
; ones in a `var_spec_list` — capturing the spec makes all three shapes uniform and
; keeps every declared name findable. The cost, accepted: the `type`/`const`/`var`
; keyword falls outside the unit's span, and a spec that declares several names
; (`const C, D = 3, 4`) is one unit named by the first, with every name in the
; signature — the rule T24-02 settled for Bash declaration commands.
;
; A `func_literal` (`inner := func() {}`) is deliberately not a unit: it is an
; expression, has no name, and would only ever produce ordinal anchors.

; --- functions and methods ---
; A method's name is a `field_identifier`, not an `identifier` — the specific
; reason T24-01 widened `is_identifier_kind`. Methods are top-level nodes in Go, so
; the route is flat (`method:Start`); the receiver lives in the signature, which is
; what keeps two types' identically named methods distinct.
(function_declaration) @decl.function
(method_declaration) @decl.method

; --- types, constants, variables: one unit per spec ---
(type_spec) @decl.type
(const_spec) @decl.const
(var_spec) @decl.var

; --- import paths (unresolved references), one per spec ---
; The inner content node is captured, not the literal, so the specifier arrives
; already unquoted — the shape `typescript.scm` uses for `string_fragment`. Both
; literal forms are covered: `import "fmt"` and the legal-but-rare `import ` + a
; backtick-quoted path. The alias is deliberately not recorded: `f "path/filepath"`,
; `_ "embed"` and `. "os"` are all references to the path, not to the alias.
(import_spec path: (interpreted_string_literal (interpreted_string_literal_content) @ref.import))
(import_spec path: (raw_string_literal (raw_string_literal_content) @ref.import))
