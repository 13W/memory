; JavaScript symbol/reference query set — query_version = 1 (ADR-0002).
;
; A separate set from `typescript.scm`: the JavaScript grammar has no
; interface/enum/type_alias/namespace/function_signature/abstract_* node kinds, so
; the TypeScript query would fail to compile against it (`Query::new` rejects
; unknown node types). Captures declaration NODES as @decl.<lang_kind>; names and
; signatures are extracted in Rust (a computed/anonymous name yields an ordinal
; anchor rather than dropping the unit). Module specifiers are captured as @ref.*.
; Bumping this set is a `queries=` rebuild event.

; --- callable / class declarations (matched at any nesting) ---
(function_declaration) @decl.function
(generator_function_declaration) @decl.function

(class_declaration) @decl.class

(method_definition) @decl.method

; --- module-scope function/class-valued bindings → `const` symbols ---
(program
  (lexical_declaration
    (variable_declarator
      value: [(arrow_function) (function_expression) (generator_function) (class)]) @decl.const))
(program
  (export_statement
    (lexical_declaration
      (variable_declarator
        value: [(arrow_function) (function_expression) (generator_function) (class)]) @decl.const)))
(program
  (variable_declaration
    (variable_declarator
      value: [(arrow_function) (function_expression) (generator_function) (class)]) @decl.const))
(program
  (export_statement
    (variable_declaration
      (variable_declarator
        value: [(arrow_function) (function_expression) (generator_function) (class)]) @decl.const)))

; --- module specifiers (unresolved references) ---
(import_statement source: (string (string_fragment) @ref.import))
(export_statement source: (string (string_fragment) @ref.reexport))
