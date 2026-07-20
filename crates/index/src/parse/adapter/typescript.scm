; TypeScript symbol/reference query set — query_version = 1 (ADR-0002).
;
; Captures declaration NODES as @decl.<lang_kind>; names/signatures are extracted
; in Rust (so a computed/anonymous name yields an ordinal anchor rather than
; dropping the unit). Module specifiers are captured as @ref.* and refined in Rust
; (import vs `import type`). Bumping this set is a `queries=` rebuild event.

; --- callable / type declarations (matched at any nesting) ---
(function_declaration) @decl.function
(generator_function_declaration) @decl.function
(function_signature) @decl.function

(class_declaration) @decl.class
(abstract_class_declaration) @decl.class

(interface_declaration) @decl.interface
(enum_declaration) @decl.enum
(type_alias_declaration) @decl.type_alias

(internal_module) @decl.namespace
(module) @decl.namespace

(method_definition) @decl.method
(method_signature) @decl.method
(abstract_method_signature) @decl.method

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
