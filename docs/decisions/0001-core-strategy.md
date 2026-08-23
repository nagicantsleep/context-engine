# 0001 Core Strategy — context-engine as the Integration Point

Date: 2026-08-01

## Status

Accepted

## Context

Comparative analysis (2026-08-01) of context-engine against: Graphify (YC S26),
codegraph/colbymchenry (Rust+SQLite), CodeGraphContext (Python+SCIP), GitNexus, Sourcegraph
SCIP, ast-grep, bloop (archived 2025), continue.dev.

context-engine's pipeline (embed → graph-expand → agentic rerank) was assessed as the most complete
on query quality and performance. Competing tools contribute narrow, separable value that does
not justify a runtime dependency or fork.

## Decision

**context-engine is the integration core.** Do not orchestrate or merge external tools at runtime.
Selectively absorb specific, well-scoped gaps directly into the Rust codebase.

## Alternatives Considered

1. **MCP meta-orchestration** — fan-out to Graphify/CodeGraphContext at query time. Rejected:
   impedance mismatch, latency compounding, maintenance overhead.
2. **Fork Python tools** — import CodeGraphContext or similar into the repo. Rejected: narrow
   value, does not justify maintaining a Python dependency.
3. **Full SCIP dependency** — adopt Sourcegraph SCIP as primary symbol source. Deferred: only
   justified when there is clear evidence of unresolved symbol recall gaps on Java/C++ repos
   (see Phase 3 in `docs/plans/`).

## Consequences

Positive:

- No external runtime dependencies; deployment stays a single Rust binary.
- Query pipeline quality is preserved and incrementally improvable.
- Each absorbed feature is testable in isolation within the existing Rust test suite.

Tradeoffs:

- Every absorbed capability must be implemented and maintained in Rust.
- Narrow value from external tools is foregone unless absorption effort is clearly justified.

## Out of Scope (standing)

| Feature | Reason |
|---------|--------|
| Graph visualization / Leiden clustering | UI concern — build separately if needed; does not affect query quality |
| MCP meta-orchestration (fan-out to external tools) | See Alternatives above |
| Fork Python tools into repo | Narrow value; does not justify maintaining Python dependency |

## Follow-Up

- Reassess SCIP ingestion (Phase 3) when concrete Java/C++ symbol recall gaps are observed.
- Revisit MCP orchestration only if a narrow, low-latency integration point emerges with
  clear user-facing benefit.
