; Bash symbol/reference query set — query_version = 1 (ADR-0015, ADR-0002).
;
; Captures declaration NODES as @decl.<lang_kind>; names and signatures are
; extracted in Rust (a non-identifier name yields an ordinal anchor rather than
; dropping the unit). Sourced paths are captured as @ref.import and refined in
; Rust. Bumping this set is a `queries=` rebuild event.
;
; Upstream ships no `tags.scm` for bash, so every row here is hand-authored and
; carries the reason it exists (T24-02); nothing is transplanted from another
; grammar's vocabulary.
;
; Unlike `python.scm`, the declaration rows are NOT restricted by parent: a
; `function_definition` has SIXTEEN possible parents in grammar.json (program,
; compound_statement, subshell, list, pipeline, if_statement, elif_clause,
; else_clause, while_statement, do_group, case_item, last_case_item,
; redirected_statement, command_substitution, process_substitution,
; heredoc_redirect), so enumerating them would be a transcription with no
; benefit — parents are assigned by span containment in the engine, not by the
; query. Matching at any depth also means a definition recovered inside an ERROR
; region keeps its name (ADR-0002, "may overlap a fallback_chunk"); that is
; observed behavior for bash, not insurance: a function after an unclosed body is
; parsed as a `function_definition` INSIDE the ERROR node (T24-02, measured).

; --- functions: `name() { … }`, `function name { … }`, `function name() { … }` ---
; All three surface forms are one node kind with the same `name:` field (a
; `word`), which is why T24-01 put `word` in the shared identifier allowlist.
(function_definition) @decl.function

; --- declarations: `local`/`declare`/`typeset`/`export`/`readonly` ---
; One unit per COMMAND, not per assignment: `declare a=1 b=2` is a single node,
; named by its first declared variable, with every declared name in the
; signature. The two rows require at least one operand, so an operand-less
; command (`declare -p`, a bare `local`) is not a unit at all — the same "no
; ordinal noise" rule `python.scm` applies to non-identifier assignment targets.
; A plain `PLAIN=1` is a `variable_assignment`, not a `declaration_command`, and
; is deliberately not a unit: a shell script's every top-level assignment would
; otherwise become a symbol.
(declaration_command (variable_assignment)) @decl.variable
(declaration_command (variable_name)) @decl.variable

; --- sourced files (unresolved references) ---
; `source lib.sh` and `. lib.sh` only; `echo`/`ls`/`grep` and every other command
; are excluded by the predicate (standard text predicates ARE applied by the Rust
; binding). The captured node is any `word` argument; the Rust hook keeps only the
; FIRST one, because `source lib.sh arg1` passes `arg1` to the sourced script. A
; quoted or expanded path (`source "$DIR/lib.sh"`) is a `string`, not a `word`, and
; is deliberately not captured — the specifier is not knowable without evaluation.
(command
  name: (command_name (word) @_cmd)
  argument: (word) @ref.import
  (#match? @_cmd "^(source|\\.)$"))
