# Execution Plan: Data-Flow Language Expansion Task 3

Date: 2026-08-09

## Status

Completed

## Outcome

Extend the existing `FlowSpec`-driven `DataFlowsTo` extraction to C#, PHP,
C, and C++, while preserving existing language behavior and keeping the
library test suite green.

## Context

- The completed predecessor plan is
  `docs/plans/completed/phase-dataflow-language-expansion.md`.
- The generic descriptor and collectors are in `src/parsing/mod.rs`:
  `FlowSpec`, `NodeRef`, `resolve_ident_arg`, `resolve_binding`,
  `unwrap_operand`, `collect_param_forward_edges`, and
  `collect_intermediate_flow_edges`.
- The target extractors currently emit call edges only:
  `extract_csharp_node`, `extract_php_node`, and the shared
  `extract_c_cpp_node`.
- Grammar versions are pinned by `Cargo.lock`: tree-sitter-c 0.23.4,
  tree-sitter-cpp 0.23.4, tree-sitter-c-sharp 0.23.5, and tree-sitter-php
  0.24.2. Node-types evidence was read from those exact registry sources.
- Existing parser tests are in `src/parsing/mod.rs`: `cpp_tests`,
  `csharp_tests`, and `php_tests`.

## Scope

In scope:

- One verified FlowSpec mapping and reachable extractor wiring for C#.
- One verified FlowSpec mapping and reachable extractor wiring for PHP.
- C and C++ data-flow extraction through the shared extractor, with separate
  specs or a small descriptor extension if their verified binding shapes cannot
  be represented by one existing spec.
- Parameter-forward, literal-negative, and intermediate-variable tests for
  every target language. Every language must include a real
  `x = foo(); bar(x)`-shaped fixture appropriate to its grammar.
- Focused tests, `cargo check --lib`, and full `cargo test --lib`.
- One commit per coherent language change, followed by completion of this
  plan after all four languages pass.

Out of scope:

- Lua, Luau, Swift, Kotlin, JS arrow functions/class methods, data-flow-only
  graph BFS, and the Option-A documentation rewrite. Those remain separate
  follow-on tasks from the predecessor plan.
- Integration/index-to-query proof beyond the existing library/parser tests.
- Unrelated extractor cleanup or call-edge redesign.

## Verified Grammar Evidence

### C#

- Call node: `invocation_expression`.
- Call fields: `function` and `arguments`; arguments are an `argument_list`.
- Bare identifier arguments are wrapped in named `argument` nodes, so the
  existing `arg_unwrap_kinds` mechanism is relevant.
- Functions/methods: `method_declaration` has `name`, `parameters`, and
  `body`; `body` is a `block` when block-bodied.
- Parameters: `parameter_list` children are `parameter`; each parameter has a
  `name` field whose node kind is `identifier`.
- Local bindings: `local_declaration_statement` contains a
  `variable_declaration`, which contains one or more `variable_declarator`
  children. `variable_declarator` has a `name` field but its initializer is an
  unnamed expression child; it has no `value` field.
- The existing FlowSpec fields therefore do not yet represent the C# binding
  without a verified helper/descriptor extension. This is an explicit design
  gate, not an assumption.

### PHP

- Call node: `function_call_expression` with `function` and `arguments`
  fields; `arguments` is an `arguments` node.
- Bare arguments are named `argument` nodes whose expression child can contain
  a `variable_name` node.
- Assignment node: `assignment_expression` with `left` and `right` fields.
- Function node: `function_definition` has `name`, `parameters`, and `body`,
  with `body` a `compound_statement`.
- PHP parameter/identifier and statement-wrapper details must be confirmed
  with parser-backed fixtures before writing the spec; do not infer from the
  unavailable convenience grammar source path.
- PHP member/scoped calls are existing call-edge cases and are not silently
  broadened unless the data-flow fixture proves the generic call shape.

### C and C++

- Both use `call_expression` with `function` and `arguments` fields.
- Both use an `argument_list` node for arguments; bare identifiers are
  expression children, so no argument wrapper is currently evidenced.
- Function definitions expose a `body` field containing a
  `compound_statement`.
- Local declaration initialization uses `init_declarator` with `declarator`
  and `value` fields, under a `declaration` node.
- Assignment uses `assignment_expression` with `left` and `right` fields.
- C/C++ parameter declarations expose a `declarator` field; the existing
  `declarator_name` helper must be used or extended only after fixture
  evidence confirms the concrete parameter node shapes.
- The two incompatible binding shapes (`init_declarator` versus
  `assignment_expression`) are an explicit descriptor design gate. Do not
  force both into one spec by guessing.

## Approach

1. For each language, create parser-backed probe tests/fixtures or inspect
   the exact current tree-sitter node shape before editing the extractor.
2. Add the smallest descriptor/helper representation supported by verified
   shapes. If a shared helper changes, run the pre-change language tests and
   verify the full library count is unchanged before adding new tests.
3. Wire the collector calls directly in the function/method extraction arm,
   cloning the symbol before `symbols.push` and preserving recursive descent.
4. Add focused positive, literal-negative, and intermediate-variable tests
   immediately for that language. Do not treat a green compile as proof of
   wiring.
5. Run focused language tests, `cargo check --lib`, and full `cargo test
   --lib` before each commit. Commit one language at a time.
6. After all four languages are committed and validated, update this plan's
   progress/result and move it to `docs/plans/completed/`.

## Implementation Notes

- C#: Added `CSHARP_FLOW_SPEC` for `invocation_expression` calls, argument wrappers, local-declaration/variable-declarator bindings, and identifier parameters. Shared `resolve_rhs` now supports C#'s unnamed direct `invocation_expression` initializer child, and `resolve_binding` recursively handles statement wrappers. The C# extractor clones the qualified symbol before `symbols.push` and invokes parameter-forward and intermediate-flow collectors. Three parser-backed tests cover positive parameter forwarding, literal-negative behavior, and `x = foo(); bar(x)` intermediate flow.
- PHP: Added `PHP_FLOW_SPEC` for `function_call_expression` calls, argument wrappers, assignment-expression bindings, expression-statement wrappers, and `variable_name` parameters. The PHP extractor clones the qualified symbol before `symbols.push` and invokes parameter-forward and intermediate-flow collectors. Three parser-backed tests cover positive parameter forwarding, literal-negative behavior, and `x = foo(); bar(x)` intermediate flow.
- C/C++: Added declaration specs for `init_declarator` bindings and separate assignment specs for `assignment_expression` bindings. Shared C/C++ parameter helpers (`c_cpp_param_names` and `c_cpp_declarator_ident`) use recursive declarator-name resolution; the shared extractor selects the language-specific specs and invokes parameter-forward, declaration-intermediate, and assignment-intermediate collectors. Eight parser-backed tests cover C and C++ positive parameter forwarding, literal negatives, declaration flows, and assignment flows.

## Risks And Recovery

- C# initializer is an unnamed child and C/C++ have two binding node shapes;
  a naive field-only spec can compile while emitting no intermediate edges.
  Mitigation: require intermediate-variable tests and parser-backed shape
  checks before shipping each language.
- PHP uses `variable_name` rather than the `identifier` kind used by current
  specs. Mitigation: confirm the parameter and argument nodes with fixtures;
  use a narrow spec/helper change only if directly required.
- Shared helper changes affect already-shipped languages. Before adding new
  language assertions after any shared change, run focused existing-language
  tests and full `cargo test --lib`; test-count drift is a regression.
- Verifier residual coverage risks, not observed regressions: C#/PHP focused
  tests cover only bare direct parameter forwarding and direct local call-result
  assignment/initializer consumption. They do not directly cover
  qualified/member callees, nested scopes/blocks, destructuring or multi-variable
  bindings, or awaited/otherwise wrapped RHS calls. C# unnamed-RHS resolution
  matches only a direct `invocation_expression` child; PHP parameter discovery
  selects the first direct named `variable_name` child. Generic intermediate
  analysis remains shallow and statement-order/spec driven with a tracked-chain
  depth cap of 3; the Rust regression filter covers simple binding and
  literal/undeclared negatives, not every shared-helper language.
- Verifier residual C/C++ coverage risks, not observed regressions: the eight
  parser-backed tests cover bare identifier callees and simple scalar
  declarators only. They do not directly cover pointer/reference parameters,
  qualified or member callees, multi-declarator declarations, or intermediate
  bindings inside nested blocks.
- If a partial implementation is left uncommitted, inspect `git status` and
  the exact diff before recovery. Never assume an interrupted edit was lost.

## Progress

- [x] Read predecessor plan, FlowSpec design history, target extractors, and
      pinned grammar/node-types evidence.
- [x] Record C# and C/C++ descriptor design gates instead of guessing.
- [x] Confirm PHP parameter/statement shapes with parser-backed fixtures.
- [x] Implement and validate C# FlowSpec extraction.
- [x] Implement and validate PHP FlowSpec extraction.
- [x] Implement and validate C FlowSpec extraction.
- [x] Implement and validate C++ FlowSpec extraction.
- [x] Run final full library validation and record exact counts.
- [x] Commit code and move this plan to `docs/plans/completed/`.

## Decisions

- 2026-08-09: Continue with Task #3 as the next work item, because it is the
  stated next language group in the completed predecessor plan.
- 2026-08-09: Preserve one-language-at-a-time commits and require an
  intermediate-variable test per language; compile success alone is not proof
  of reachable collector wiring.
- 2026-08-09: Treat C#'s unnamed variable initializer and C/C++'s two binding
  node shapes as explicit descriptor design gates. Do not invent field names or
  force incompatible shapes into the current FlowSpec.
- 2026-08-09: Treat PHP grammar-source lookup failure as an environment/source
  lookup issue, not evidence that PHP fields differ. Use exact pinned
  node-types plus parser-backed fixtures as authority.

## Validation

- Focused proof: `cargo test --lib csharp_` — 8 passed;
  `cargo test --lib php_` — 7 passed; `cargo test --lib c_data_flow_tests` —
  4 passed; `cargo test --lib cpp_data_flow_tests` — 4 passed; and
  `cargo test --lib cpp_tests` — 7 passed. These include each target
  language's intermediate-variable coverage.
- Repository-required checks: `cargo check --lib` passed cleanly with zero
  warnings; full `cargo test --lib` — 772 executed, 766 passed, 0 failed,
  6 ignored; `git diff --check` was clean.
- Integration/end-to-end proof: none planned; no index-to-query fixture is
  added in this task.

## Result
Completed. Shipped `DataFlowsTo` extraction for C#, PHP, C, and C++ through
the generalized `FlowSpec` descriptor system and shared collector wiring.
C#/PHP implementation is in commit `0755ad0`; C/C++ implementation is in
commit `7c347f97f204c92081c4d84b97d45360a0a777c9`. Focused parser tests,
`cargo check --lib`, the full library suite, and `git diff --check` passed as
recorded above. Integration/end-to-end index-to-query proof remains out of
scope. The verifier's boundary-case coverage risks are retained above as
follow-up coverage considerations, not observed regressions.
