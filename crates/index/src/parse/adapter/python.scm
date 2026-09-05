; Python symbol/reference query set — query_version = 1 (ADR-0015, ADR-0002).
;
; Captures declaration NODES as @decl.<lang_kind>; names and signatures are
; extracted in Rust (a non-identifier name yields an ordinal anchor rather than
; dropping the unit). Module specifiers are captured as @ref.import and refined in
; Rust. Bumping this set is a `queries=` rebuild event.
;
; A `function_definition` / `class_definition` has exactly three possible parents
; in the grammar — `module`, `block` and `decorated_definition` (verified in
; grammar.json). The first two are matched as DIRECT children, and a decorated
; definition is captured once, at its `decorated_definition` wrapper: the unit owns
; the decorators' span, its lang_kind stays function/class, and its route does not
; change when a decorator is added (T24-01 decision).
;
; The `ERROR` rows are insurance, not observed behavior: the v0 queries match at any
; depth, so a definition recovered inside an ERROR region still gets its name
; (ADR-0002, "may overlap a fallback_chunk"), and these rows preserve that for the
; parent-restricted patterns above. No malformed input probed at T24-01 (28 samples)
; produced an ERROR-parented definition — tree-sitter-python recovers with ERROR
; *siblings* at module level, which the `module` rows already cover.

; --- functions and classes (plain, decorated, recovered) ---
(module (function_definition) @decl.function)
(block (function_definition) @decl.function)
(decorated_definition definition: (function_definition)) @decl.function
(ERROR (function_definition) @decl.function)

(module (class_definition) @decl.class)
(block (class_definition) @decl.class)
(decorated_definition definition: (class_definition)) @decl.class
(ERROR (class_definition) @decl.class)

; --- module-level bindings to a bare identifier → `variable` symbols ---
; `X = 1`, `X: int = 1`, `x = y = 1` (one unit, `x`). Tuple / attribute /
; subscript targets are not units at all (no ordinal noise). Python has no
; `const`; the kind is `variable` rather than upstream tags.scm's `constant`.
(module
  (expression_statement
    (assignment left: (identifier)) @decl.variable))

; --- module specifiers (unresolved references), one per specifier ---
; `import os, sys` → os, sys; `import x as y` → x; `from a.b import c` → a.b;
; `from . import d` → `.`. `from __future__ import …` is a distinct node kind
; (`future_import_statement`) and is deliberately not a reference — it names a
; compiler directive, not a module dependency.
(import_statement name: (dotted_name) @ref.import)
(import_statement name: (aliased_import name: (dotted_name) @ref.import))
(import_from_statement module_name: (dotted_name) @ref.import)
(import_from_statement module_name: (relative_import) @ref.import)
