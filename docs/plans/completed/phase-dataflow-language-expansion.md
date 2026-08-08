# Execution Plan: Data-Flow Language Expansion via FlowSpec

Date: 2026-08-08

## Status

Completed

## Outcome

Extend `DataFlowsTo` edge extraction (param-forwarding + intermediate-variable
flow) from Rust/Python-only to every language this repo indexes, by
generalizing the existing per-language helpers into one `FlowSpec` descriptor
in `src/parsing/mod.rs` and adding one spec + wiring + tests per language.

## Context

- Prior state: `docs/plans/completed/phase-dataflow-option-a.md` shipped
  `DataFlowsTo` for Rust + Python only, via a narrow `return <call>` pattern,
  and explicitly deferred param-forwarding, intermediate-variable tracking,
  and other languages as follow-ups.
- Commit `bdde1b5` (2026-08-03) silently already closed two of those
  follow-ups for Rust/Python (`collect_intermediate_flow_edges_*`,
  `collect_param_forward_edges_*`) but left six near-duplicate per-language
  functions and did not touch other languages. That doc is now STALE — see
  task 6 below.
- This plan starts from a refactor (`722559e`) that unified those six
  functions into a `FlowSpec`-driven descriptor system, then adds languages
  one at a time.
- No architect/decision doc exists for this beyond the two commits' bodies —
  the commit messages ARE the design record for each step. Read them with
  `git log --oneline` / `git show <sha>` before continuing.

## Scope

In scope (this plan):

- `FlowSpec` descriptor + `NodeRef`, `unwrap_operand`, `resolve_binding`,
  `resolve_ident_arg` in `src/parsing/mod.rs`.
- One `*_FLOW_SPEC` const + extractor wiring + tests per language, for:
  Go, JavaScript/TypeScript/Tsx/Svelte, Ruby, Java, Dart, Pascal (this
  plan), then C#/PHP/C/C++ and Lua/Luau/Swift/Kotlin (follow-on plans /
  tasks #3, #4 — not started).
- Keeping `cargo test --lib` green throughout; zero behavior change for
  already-shipped languages.

Out of scope (tracked as separate tasks, not this plan):

- `query_data_flow_callees` data-flow-only BFS in `graph_expand.rs` (task #5).
- Wiring JS arrow functions / class methods (task #9) — JS/TS is currently
  wired ONLY for `function_declaration`/`function`, not arrow fns or methods.
- C#, PHP, C/C++, Lua/Luau, Swift, Kotlin (tasks #3, #4).
- Rewriting `docs/plans/completed/phase-dataflow-option-a.md` (task #6) —
  do that only after all languages in this plan + #3/#4 land, so it is
  rewritten once, correctly, not repeatedly.
- The known pre-existing flaky test `test_cancel_index_and_reindex` in
  `tests/integration.rs` (task #7) — confirmed nondeterministic and
  UNRELATED to this work (verified 5/5 pass on the Go commit); ignore it
  if it fails during a full `cargo test` run, it is not yours to fix here.

## Approach

Proven recipe (took 6 attempts for Go before this was nailed down; 2-3
attempts per language after): split each language into up to 4 small,
independent steps, submitted as SEPARATE small edits/commits, in order:

1. **Const** — insert one `<LANG>_FLOW_SPEC` next to the existing consts.
   Field order: `call_kind, callee, args, arg_unwrap_kinds, binding_kinds,
   lhs_field, rhs_field, lhs_unwrap_kinds, rhs_unwrap_kinds, stmt_unwrap,
   param_ident_kinds`. It will be `never used` (a compiler warning) until
   step 2 — that warning is EXPECTED and must NOT be silenced with
   `#[allow(dead_code)]`; it must disappear because step 2 gives it a real
   caller.
2. **Wire** — call `collect_param_forward_edges(&LANG_FLOW_SPEC, ...)` and
   `collect_intermediate_flow_edges(&LANG_FLOW_SPEC, ...)` from that
   language's `extract_<lang>_node` function, mirroring exactly how
   `extract_rust_node` / `extract_go_node` do it (param walk, guard on
   non-empty param_names, then the two collector calls). Requires adding
   `let func_sym = sym.qualified.clone();` before `symbols.push(sym);` in
   languages whose `sym` is a bare `Symbol` (all of them so far).
3. **Tests** — mirror `rust_param_forward_tests` /
   `rust_intermediate_flow_tests` module style exactly. Minimum 3-5 cases:
   param forwarding emits an edge, a literal argument does NOT, and (most
   important) an intermediate-variable case (`x = foo(); bar(x)`) emits an
   edge — this is the ONLY thing that proves the wiring actually reaches
   real statements, see the Dart lesson below.
4. **Commit** — one commit per language, `feat(parsing): add <Lang>
   data-flow extraction via FlowSpec`, referencing what changed and why.

### Critical technique: measure anchor uniqueness before editing

Before dispatching any edit to `extract_<lang>_node`, grep the target
match-arm string with `Grep({ pattern, output_mode: "count", multiline:
true })` across the WHOLE arm block you intend to anchor on (not just the
header line) and confirm it returns exactly `1`. If a header line like
`"method_declaration" | "constructor_declaration" =>` is not unique (Java
and C# share it), extend the anchor block until it includes something
language-specific (e.g. the `extract_java_node(` recursive call), and
verify uniqueness again. Paste the VERBATIM current text of that exact
line range into the edit instructions — never ask an implementer to
"find" the arm; that wastes an entire attempt.

### Known anchor hazards already found in this file

- The generic 3-line tail `let fqn = sym.qualified.fqn(); symbols.push(sym);
  let mut child_scope = scope.to_vec();` appears **38 times** across
  extractors — never anchor on it alone.
- `"method_declaration" | "constructor_declaration" =>` appears **twice**
  (Java's arm and C#'s arm) — must disambiguate by including the
  `extract_java_node(` / `extract_csharp_node(` recursive call in the
  anchor.
- `extract_js_node`'s `class_declaration` arm has a byte-identical tail to
  its `function_declaration` arm — same problem, same fix.
- `extract_dart_node`'s function arm contains **two** `extract_dart_node(`
  recursion calls in the same match arm (one in the `if let Some(name) =`
  body, one in the `else` "name not found" branch) — only the first is the
  wiring point; do not touch the second.

## Risks And Recovery

- **A green build and an unchanged `cargo test --lib` count prove NOTHING
  for a newly wired language that has no tests yet.** This is the single
  most important lesson from this plan. Dart wired cleanly, compiled with
  zero warnings, and held the test count — while silently extracting ZERO
  intermediate-flow edges and dropping ALL top-level-function params. Both
  bugs were caught only because the implementing agent traced its own
  wiring back against the grammar facts it had already gathered, not
  because any test failed. **Do not consider a language done until it has
  its own passing tests that include an intermediate-variable case.**
- If `resolve_binding` or `unwrap_operand` need to change for a new
  language (they have needed to change twice so far — see Decisions), that
  is SHARED code affecting every already-shipped language. Any such change
  must keep `cargo test --lib`'s total EXACTLY at the pre-change count
  before proceeding to add the new language's tests. A count drift means
  behavior changed for an existing language — treat it as a regression to
  fix, never as a test to edit.
- Recovery if a partial edit is left uncommitted: `git diff --stat
  src/parsing/mod.rs` and `git status --short` first. Read the actual diff
  before assuming anything is broken or lost — in this session, an
  uncommitted diff was mistaken for "nothing landed" twice when it had in
  fact landed; always verify by reading, not by assuming from a stale
  agent message.
- If a subagent orchestration budget/quota is exhausted mid-language,
  resuming an EXISTING agent (by id, via whatever messaging mechanism the
  session uses) to continue its own in-progress edit does not consume a
  new slot the way spawning a fresh agent does — prefer resuming over
  spawning when continuing the same file's work.

## Progress

- [x] `722559e` — refactor six near-duplicate helpers into `FlowSpec`
      descriptor + `NodeRef` enum. Zero behavior change (verified: equivalence
      proven by mechanically expanding each generic body with its spec's
      constants and diffing against the deleted originals).
- [x] `bc9a374` — Go: `GO_FLOW_SPEC` + `lhs_unwrap_kinds`/`rhs_unwrap_kinds`
      + `unwrap_operand` added (Go's `short_var_declaration` wraps both
      sides in `expression_list`). 4 tests.
- [x] `7e7995f` — JS/TS/Tsx/Svelte (one spec covers all four — they
      delegate to `extract_javascript`): `JS_FLOW_SPEC`. Fixed a latent bug
      in `resolve_binding` (see Decisions). Wired ONLY the
      `"function_declaration" | "function"` arm — arrow functions and class
      methods are NOT covered (tracked as task #9 outside this plan). 5
      tests.
- [x] `8f52c28` — Ruby: `RUBY_FLOW_SPEC`. First language with
      `stmt_unwrap: &[]` (bare `assignment`, no statement wrapper). Verified
      against tree-sitter-ruby 0.23's `node-types.json` rather than assumed.
      5 tests.
- [x] `cc77c4f` — Java: `JAVA_FLOW_SPEC` (callee field is `name`, not
      `function` — differs from Rust/Go/JS). Required a SECOND
      `resolve_binding` fix (see Decisions). 5 tests.
- [x] `a5d7b5c` — Dart: `DART_FLOW_SPEC`, extractor wiring, and 7 tests.
  Validation fixed two issues: top-level `function_signature` and
  `method_signature` nested `function_signature` name traversal, plus
  `function_body` -> `block` descent for the direct-child intermediate
  collector.
- [x] `ef49582` — Pascal: `PASCAL_FLOW_SPEC`, grammar-verified extractor
  wiring, and 4 new tests.

## NEXT STEP

Scope is complete after Dart and Pascal. Tasks #3/#4/#5/#6/#9 remain
separate and are not started. The option-A doc rewrite (task #6) still waits
for #3/#4.
## Decisions

- 2026-08-08: Unify six near-duplicate per-language functions into one
  `FlowSpec` descriptor instead of writing N per-language copies. Reason:
  Rust and Python's existing implementations were byte-identical except for
  one string (the call-node kind), so per-language copies would have been
  ~900 duplicated lines across the ~15 target languages.
- 2026-08-08: `FlowSpec.args` is the FIELD NAME on the call node, NOT the
  argument-list node's own kind. An earlier draft of this plan's data table
  conflated the two (e.g. wrote `Field("argument_list")` for Go, which does
  not compile/resolve as intended) — the correct value is `Field("arguments")`
  for Go/Java/C#/C/C++/Ruby even though the argument-list node's KIND is
  `argument_list`.
- 2026-08-08 (Go): added `lhs_unwrap_kinds`/`rhs_unwrap_kinds` fields to
  `FlowSpec` plus an `unwrap_operand` helper, because Go's
  `short_var_declaration` wraps BOTH the identifier side and the call side
  in an `expression_list` rather than exposing them directly — a structural
  fact the original per-language table had not captured (it recorded field
  *names* but not what those fields' values were wrapped in).
- 2026-08-08 (JS): fixed `resolve_binding`, which unwrapped `stmt_unwrap`
  kinds via `stmt.child(0)` — that includes ANONYMOUS tokens. Python's
  `expression_statement` happened to have its named `assignment` at
  child(0), so it worked, but JS's `lexical_declaration` has the anonymous
  `let`/`const` keyword there instead; the unwrap would have silently
  matched nothing. Now takes the first NAMED child, consistent with
  `unwrap_operand`'s existing approach.
- 2026-08-08 (Java): fixed `resolve_binding` a SECOND time — "first named
  child" was still purely positional and broke on Java, where
  `local_variable_declaration`'s named children are `[integral_type,
  variable_declarator]` (the type node IS named, unlike JS's anonymous
  keyword). Generalized from "take the first named child" to "search named
  children for the first one matching `binding_kinds`" — strictly more
  general, verified to keep Python/JS/Ruby/Go/Rust resolving to the exact
  same node as before. This generalization is WHY Dart's `int y = foo();`
  (also type-then-declarator) needed no further `resolve_binding` change.
- 2026-08-09 (Dart): validation found and fixed two grammar/traversal issues:
  top-level `function_signature` and `method_signature` nested
  `function_signature` require distinct name traversal, and the direct-child
  intermediate collector requires descending from `function_body` to its
  `block`; arrow bodies remain without a block and produce no intermediate
  edges.
- 2026-08-09 (Pascal): grammar verification established the Pascal decisions:
  call kind `exprCall` with callee `entity` and args `args`; assignment uses
  `lhs`/`rhs`; `stmt_unwrap` is empty; and the direct function body is passed
  to the collectors.

## Validation

- Focused proof policy: run `cargo test --lib <lang>_` for each language's
  test modules (for example, `cargo test --lib dart_`). Final focused proof:
  `cargo test --lib dart_` — 12 passed; `cargo test --lib pascal_` — 5
  passed (4 new Pascal tests plus the existing Pascal test).
- Repository-required checks: `cargo check --lib` passed with zero warnings
  and no new `#[allow(dead_code)]`; full `cargo test --lib` — 752 passed,
  0 failed, 6 ignored, 758 registered. `test_cancel_index_and_reindex` in
  the separate `tests/integration` binary remains a known pre-existing flaky
  test unrelated to this work (task #7); it was not chased here.
- Integration proof: none added — no test exercises the full index -> query
  pipeline for the new languages' `DataFlowsTo` edges end-to-end. This is
  intentionally deferred until all languages in scope (#3 and #4 included)
  land, not per-language.

## Result

Completed. Shipped `DataFlowsTo` extraction for Go,
JavaScript/TypeScript/Tsx/Svelte, Ruby, Java, Dart, and Pascal through the
generalized `FlowSpec` descriptor system. Commits are `722559e`, `bc9a374`,
`7e7995f`, `8f52c28`, `cc77c4f`, `a5d7b5c`, and `ef49582`. Proof is
`cargo check --lib` with zero warnings, focused Dart/Pascal tests, and the
full 752-pass library suite recorded above; integration proof remains none
and was not added. Follow-on tasks #3/#4/#5/#6/#9 remain out of scope and
not started, and the option-A rewrite waits for #3/#4.

