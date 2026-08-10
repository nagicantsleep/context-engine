# Execution Plan: Data-Flow Language Expansion Task 4

Date: 2026-08-09

## Status

Completed

## Outcome

Extend the existing `FlowSpec`-driven `DataFlowsTo` extraction to Lua, Luau,
Swift, and Kotlin, preserving all shipped-language behavior and keeping the
library test suite green.

## Context

- The completed predecessor plans are
  `docs/plans/completed/phase-dataflow-language-expansion.md` (tasks #1/#2:
  Rust/Python/Go/JS/TS/Ruby/Java/Dart/Pascal) and
  `docs/plans/completed/phase-dataflow-language-expansion-task3.md`
  (task #3: C#/PHP/C/C++).
- Task #4 (Lua/Luau/Swift/Kotlin) is the explicitly named next group in both
  completed plans. Task #5 (data-flow-only BFS) and task #6 (Option-A doc
  rewrite) follow after #3/#4.
- Generic descriptor and collectors are in `src/parsing/mod.rs`: `FlowSpec`,
  `NodeRef`, `resolve_ident_arg`, `resolve_binding`, `resolve_rhs`,
  `unwrap_operand`, `collect_param_forward_edges`,
  `collect_intermediate_flow_edges`, plus language-specific parameter helpers
  (`c_cpp_param_names` precedent).
- Target extractors already emit call edges: `extract_lua_node` (~4121),
  `extract_luau_node` (~4231), `extract_swift_node` (~3405),
  `extract_kotlin_node` (~3590). All four currently attribute calls via
  `scope_to_qualified`, not symbol clones.
- Grammar crates are already dependencies; pinned by `Cargo.lock`:
  tree-sitter-lua 0.5.0, tree-sitter-luau 1.2.0, tree-sitter-swift 0.7.3,
  tree-sitter-kotlin-ng 1.1.0. Registry sources with `node-types.json` and
  `grammar.json` are available locally.
- Baseline library suite (task #3 final): 772 executed, 766 passed, 0 failed,
  6 ignored.

## Scope

In scope:

- One verified `FlowSpec` mapping (or the smallest shared mechanism) and
  reachable extractor wiring per language: Lua, Luau, Swift, Kotlin.
- Parameter-forward, literal-negative, and intermediate-variable tests per
  language; every language must include a genuine
  `x = foo(); bar(x)`-shaped fixture that proves collector wiring reaches
  real statements.
- Shared-code changes only where verified grammar shapes require them;
  baseline test count must remain exactly 772/766/6 after shared changes and
  before adding new-language tests.
- One commit per language, then plan completion.

Out of scope:

- Task #5 (data-flow-only BFS in `graph_expand.rs`) and task #6 (Option-A doc
  rewrite) — separate follow-ons.
- Swift `init_declaration`, Lua/Luau anonymous `function_definition`, Kotlin
  lambdas/`when`/`for` bindings, and other exotic parameter/binding shapes
  unless a fixture proves they are reachable through the same wiring.
- Integration/index-to-query proof beyond the library parser tests.
- Unrelated extractor cleanup or call-edge redesign.

## Verified Grammar Evidence

### Lua (tree-sitter-lua 0.5.0)

- Call node: `function_call` with fields `name` (callee) and `arguments`
  (required); arguments are bare expression children — no argument wrapper.
- Function node: `function_declaration` with fields `name`, `parameters`
  (required), `body` (optional `block`).
- Parameters: the `parameters` node has a repeated `name` FIELD of
  `identifier` nodes.
- Binding shape (design gate): `local x = foo()` is
  `variable_declaration -> assignment_statement -> variable_list` (FIELD
  `name` -> `variable` -> `identifier`) plus sibling `expression_list` (FIELD
  `value` -> `expression`). Plain `x = foo()` is a bare `assignment_statement`.
  LHS name and RHS call live in sibling wrappers — one-level
  `child_by_field_name` / `resolve_rhs` cannot reach both from one binding
  node.

### Luau (tree-sitter-luau 1.2.0)

- Call and function nodes: identical to Lua (`function_call` fields `name`/
  `arguments`; `function_declaration` fields `name`/`parameters`/`body`).
- Parameters DIFFER from Lua: the `parameters` node's children are `parameter`
  nodes, not identifiers. `grammar.json` shows a `parameter` as
  `identifier | vararg_expression` with optional `: type`; `node-types.json`
  omits the identifier child. Fixture-verify the parameter name node before
  writing the spec.
- Binding shape: same `variable_declaration` / `assignment_statement` /
  `variable_list` / `expression_list` structure as Lua (design gate); Luau's
  `variable_list` may carry type annotation children.

### Swift (tree-sitter-swift 0.7.3)

- Call node: `call_expression` — `_expression` plus collapsed `call_suffix`;
  AST children are the callee expression then `value_arguments`. Callee is
  `Child(0)`; args are the `value_arguments` child (position may shift with a
  trailing closure).
- Argument wrapper: `value_argument` (labeled args have `name`/
  `value_argument_label` + `value` fields); unlabeled arguments may be bare.
- Function node: `function_declaration` with `name` and `body` fields; body is
  a `function_body` whose statement container must be fixture-confirmed
  (`code_block`?), mirroring the Dart `function_body -> block` lesson.
- Parameters: direct `parameter` children with fields `external_name`
  (`simple_identifier`), `name`, `type`. Identifier kind is
  `simple_identifier`.
- Bindings: NO `constant_declaration`/`variable_declaration` kinds exist.
  Two shapes: `assignment` (fields `target`/`operator`/`result`) for `x = foo()`,
  and `property_declaration` (fields `name` -> pattern, `value` -> expression)
  for `let`/`var` (kind must be fixture-confirmed; grammar's binding machinery
  feeds property declarations).
- Node-types operator-token pollution on `value`/`default_value` fields — trust
  `grammar.json` plus parser-backed fixtures over polluted field type lists.

### Kotlin (tree-sitter-kotlin-ng 1.1.0)

- Call node: `call_expression` with NO fields; children include `expression`,
  `value_arguments`, and optional `type_arguments`. Callee is the first
  `expression` child; args are the `value_arguments` child. The existing
  extractor's `call_suffix` first-child filter is stale for this grammar and
  must not be copied into data-flow wiring.
- Argument wrapper: `value_argument` (children `expression` | `identifier`);
  `arg_unwrap_kinds: ["value_argument"]`.
- Function node: `function_declaration` with FIELD `name`; children include
  `function_value_parameters` and `function_body` (no field). Parameter node
  children: `identifier` | `type` — the name is the `identifier` child.
- Body: `function_body` children `block` | `expression`; descend to `block`
  before collectors.
- Binding shape (design gate): `property_declaration` (no fields) contains a
  `variable_declaration` (children `annotation` | `identifier` | `type`) plus
  a separate expression child for the initializer — a 2-level binding not
  expressible with one-level field resolution.

## Approach

1. For each language in order (Lua, Luau, Swift, Kotlin — simplest call/
   parameter shape first), create parser-backed probe tests or inspect the
   exact pinned grammar node shapes before editing the extractor.
2. Add the smallest descriptor/helper representation supported by verified
   shapes. Prefer contained per-language binding helpers (precedent:
   `c_cpp_param_names` / `c_cpp_declarator_ident`) over shared-semantics
   changes; extend shared machinery only if a helper would duplicate verified
   logic. If shared code must change, re-run the full library suite and keep
   the exact baseline count (772/766/6) before adding new-language tests.
3. Wire the collectors in the extractor function/method arm: clone the
   qualified symbol before `symbols.push`, collect parameter names into a
   HashSet, guard on non-empty, call `collect_param_forward_edges` and
   `collect_intermediate_flow_edges` on the body statement container
   (descending through `function_body`/`block` as verified), then preserve
   recursive descent and existing call-edge extraction.
4. Add focused positive, literal-negative, and intermediate-variable tests
   immediately per language. Do not treat a green compile as proof of wiring
   (Dart lesson).
5. Run focused language tests, `cargo check --lib`, and full
   `cargo test --lib` before each commit. Commit one language at a time:
   `feat(parsing): add <Lang> data-flow extraction via FlowSpec`.
6. After all four languages are committed and validated, update this plan's
   progress/result and move it to `docs/plans/completed/`.

### Anchor-uniqueness check before editing

Before dispatching any edit to an extractor arm, grep the target match-arm
string across the whole arm block and confirm exactly one match. Lua and Luau
arms are near-identical (`function_declaration`); extend the anchor until it
includes the language-specific recursive call. Paste the verbatim current
line range into edit instructions; never ask an implementer to "find" the arm.

## Risks And Recovery

- A green build and unchanged test count prove NOTHING for a newly wired
  language without its own intermediate-variable test. Each language ships a
  genuine `x = foo(); bar(x)` fixture.
- Shared `FlowSpec`/helper changes affect all shipped languages. Baseline
  count drift (772/766/6) after a shared change is a regression to fix, never
  a test to edit.
- Kotlin's `call_suffix`-based first-child filter is stale for kotlin-ng
  1.1.0; do not replicate it. Use `Child(0)` for callee and find `value_arguments`
  by kind (or add a small `NodeRef` variant only if verified).
- Swift positional args shift with trailing closures; keep arg resolution
  fixture-driven.
- Lua/Luau colon-method calls produce callee text like `self:other` for
  unresolved targets — acceptable, matches existing Calls-edge behavior;
  document it.
- Recovery if a partial edit is left uncommitted: inspect `git status` and the
  exact diff before assuming anything is broken or lost; read the diff, do not
  guess from stale agent messages.
- Green compile with zero warnings is expected only after wiring (dead_code
  warning on an unwired const is expected and must NOT be silenced with
  `#[allow(dead_code)]`).
- Kotlin verification blocker: the first independent verifier failed on named-argument label resolution. After a bounded correction, the second independent verifier failed again on a named-literal false-positive fallback: `value_argument` named-literal fallback could treat a label identifier as the argument when the expression is a literal. The subsequent correction is present in the worktree but remains unverified and is not proof.
- Recovery for a future run: start a fresh run, inspect the current diff, independently verify last-only expression resolution plus the named-literal negative, and commit only after PASS.

## Progress

- [x] Create this plan (record task #4 scope, evidence, gates).
- [x] Lua: probe fixtures, spec/helper, wiring, tests, and commit `e982d7b`; focused verification: 7.
- [x] Luau: probe fixtures (parameter node), spec/helper, wiring, tests, and commit `9656737`; focused verification: 5.
- [x] Swift: probe fixtures (let/var kind, body container), specs, wiring, tests, and commit `e52314f`; focused verification: 12.
- [x] Kotlin: probe fixtures, spec/helper, wiring, tests, named-argument correction, named-literal negative correction, and commit `d43eb5d`; focused verification: 10.
- [x] Final focused validation: Kotlin 10 passed/0 failed/781 filtered; Swift data-flow 4/0/787; Lua data-flow 4/0/787; Luau data-flow 4/0/787.
- [x] Final full library validation: 785 passed, 0 failed, 6 ignored, 0 measured, 0 filtered out (791 total); `cargo check --lib` and `git diff --check` succeeded.
- [x] Move and commit the completed plan.
## Validation

- Independent focused verifier PASS before the Kotlin commit: `cargo test --lib kotlin_` — 10 passed/0 failed/781 filtered; `cargo test --lib swift_data_flow_tests` — 4/0/787; `cargo test --lib lua_data_flow_tests` — 4/0/787; `cargo test --lib luau_data_flow_tests` — 4/0/787.
- The verifier inspected pinned Kotlin grammar 1.1.0 and Swift grammar 0.7.3 and confirmed the required parameter-forward, literal-negative, and intermediate-variable assertions.
- `cargo check --lib` succeeded with no warnings, and `git diff --check` succeeded.
- Independent full-suite PASS after commit: exact unpiped command `cargo test --lib`; Cargo exit code 0; final line: `test result: ok. 785 passed; 0 failed; 6 ignored; 0 measured; 0 filtered out; finished in 67.20s` (791 total; no failing tests; no files edited).
- Integration/end-to-end proof: none planned for this task.
## Result

Completed. Lua, Luau, Swift, and Kotlin data-flow extraction shipped in commits `e982d7b`, `9656737`, `e52314f`, and `d43eb5d`. Final unpiped `cargo test --lib` passed with 785 passed, 0 failed, 6 ignored, 0 measured, and 0 filtered out (791 total).

## Decisions

- 2026-08-09: Task #4 is the next roadmap item after task #3, per the
  completed predecessor plans; task #5 and task #6 follow after it.
- 2026-08-09: Prefer contained per-language binding helpers over shared
  `FlowSpec` semantic changes (precedent: C/C++ helpers), unless a helper
  would duplicate verified shared logic.
- 2026-08-09: One commit per language; each language's commit requires its own
  intermediate-variable test.

