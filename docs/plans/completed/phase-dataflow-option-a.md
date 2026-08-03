# Execution Plan: Data-Flow Analysis — Option A

Date: 2026-08-01

## Status

Completed

## Outcome

`DataFlowsTo` edges extracted from `return <call>` patterns in Rust and Python source files.
Agents can query "if I change function X, what data flows downstream?" via:

```sql
SELECT in_name, out_name FROM calls WHERE flow_type = 'data_flows_to'
```

## Context

- Strategy decision: [`docs/decisions/0001-core-strategy.md`](../../decisions/0001-core-strategy.md)
- Architect analysis: Option A chosen (function-level heuristic, ~510 LOC) over Option B
  (variable-level def-use, ~1630 LOC) and Option C (security taint, narrow scope).

## Scope

In scope:

- `EdgeKind::DataFlowsTo` variant added to `src/parsing/relations.rs`.
- `flow_type: option<string>` field added to `calls` table DDL and pipeline.
- `return <call_expression>` pattern in Rust extractor (`src/parsing/mod.rs`).
- `return <call>` pattern in Python extractor (`src/parsing/mod.rs`).
- Schema v9 migration (diagnostic stamp, no data backfill).

Out of scope:

- Intermediate variable tracking (`let x = foo(); bar(x)` — not detected).
- Parameter forwarding patterns (follow-up).
- Other languages beyond Rust + Python (follow-up).
- Dedicated data-flow BFS mode in `graph_expand.rs` (follow-up).

## Progress

- [x] `EdgeKind::DataFlowsTo` added to `src/parsing/relations.rs`
- [x] `flow_type` DDL added to `src/store/schema.rs`
- [x] `DB_SCHEMA_VERSION` bumped to 9; `run_migration_v8_to_v9` added
- [x] `flow_type` field propagated through `RawEdgeRecord`, `RawEdgeRow`, 9-tuple, `flush_edge_batch`
- [x] Pipeline filters updated to pass `DataFlowsTo` edges
- [x] `store/ops.rs` updated with `DataFlowsTo` arm
- [x] Rust extractor: `return_expression` → `DataFlowsTo` edge
- [x] Python extractor: `return_statement` → `DataFlowsTo` edge
- [x] `cargo test` — 14 passed, 0 failed (2026-08-01)

## Decisions

- 2026-08-01: `DataFlowsTo` reuses the `calls` table (differentiated by `flow_type`) — no new table needed.
- 2026-08-01: `graph_expand.rs` BFS unchanged — data-flow edges are included in BFS expansion by default (richer "what's affected" traversal). Dedicated data-flow-only mode deferred.
- 2026-08-01: Start with Rust + Python only. Other languages follow the same `return_expression`/`return_statement` pattern per Tree-sitter grammar.

## Validation

- Full suite: `cargo test` — 14 passed, 0 failed (2026-08-01).
- Smoke test: index a Rust file with `fn outer() -> i32 { return inner(); }` → expect row in `calls` with `flow_type = 'data_flows_to'`.

## Result

`cargo test` green — 14 passed, 0 failed. `cargo build` clean. All implementation complete.

Known limitation: high false-negative rate (~30-50%) for flows through intermediate variables.
Intermediate variable tracking can be added as a follow-up (Option A extension).

## Follow-up candidates

- Extend to other languages (JavaScript/TypeScript, Go, Java, Rust — all have `return_expression` in Tree-sitter).
- Add parameter-forwarding detection (`fn outer(x) { inner(x) }`).
- Add `query_data_flow_callees` helper in `graph_expand.rs` for data-flow-only BFS.