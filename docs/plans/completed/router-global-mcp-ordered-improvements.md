# Execution Plan: Router Global MCP And Operational Documentation

Date: 2026-08-11

## Status

Completed

## Outcome

The router global `/mcp` endpoint restores sessions after rmcp's idle timeout
while preserving 404 responses for unknown sessions, and repository documentation
and release CI describe the observed router/worker operation and current storage
layout.

## Context

- The accepted cross-repo namespace behavior is recorded in
  [`docs/decisions/0002-multi-repo-namespace.md`](../../decisions/0002-multi-repo-namespace.md).
- The router global service is built in `src/router/mod.rs`; the worker-side
  bounded session-store pattern is in `src/server.rs` and
  `src/mcp_session_store.rs`.
- The test-first regression in `tests/router_integration.rs` observed a stale
  session GET returning 404 before the router store was attached.

## Scope

In scope:

- Attach the existing `BoundedSessionStore` to only the router global MCP config.
- Correct router comments, roadmap wording, README/AGENTS operational guidance,
  and the native release test gate.
- Preserve the focused regression and Cargo dev `test-util` feature.

Out of scope:

- Changes to `LocalSessionManager` or worker `src/server.rs` MCP construction.
- Router API/test seams, FQN/schema migrations beyond the accepted v8 diagnostic
  behavior, compatibility-control-plane artifacts, formatter/linter/full-suite
  execution in this implementation pass.

## Approach

1. Inspect the test-first Cargo/test diff and authoritative workflow/decision/code.
2. Update the router config and comments, then reconcile docs and CI minimally.
3. Run the focused router idle-session integration test and record both the
   pre-fix 404 and post-fix result.

## Risks And Recovery

- Risk: sharing a store with worker services would mix independent service
  lifecycles; mitigation: instantiate the store only in the router global config.
- Risk: documentation can drift from executable behavior; mitigation: derive
  claims from router routes, config defaults, store path helpers, and runtime
  defaults.
- Recovery: revert the bounded router config/doc/CI hunks; the preserved
  regression remains available to re-run.

## Progress

- [x] Inspected current Cargo/test-first state and observed pre-fix stale-session 404.
- [x] Reconciled ROADMAP.md with decision 0002 and accepted lazy materialization.
- [x] Updated README.md and AGENTS.md with router, MCP, smoke-check, and path guidance.
- [x] Attached the bounded store to the router global MCP config without changing
  LocalSessionManager or worker MCP code.
- [x] Added the native Ubuntu release test gate and build dependency.
- [x] Ran the focused router regression: stale sessions restored successfully,
  the restored session accepted global repo-addressed `tools/call`, and unknown
  session IDs remained 404; the hermetic worker returned the deterministic
  tool-level `no embedding API keys configured` error (no retrieval result claimed).
- [x] Chief/verifier broader validation completed; plan completion move recorded.

## Decisions

- 2026-08-11: Use a fresh `BoundedSessionStore` instance on the router global
  `StreamableHttpServerConfig`; worker services retain their separate stores.
- 2026-08-11: Move the plan to completed after verifier broader checks passed; cross-target execution remains unrun locally.

## Validation
- Pre-fix focused command: `cargo test --test router_integration global_router_mcp_idle_session_is_restored -- --nocapture` compiled after enabling Tokio `test-util` and failed at the stale-session assertion with `404 Not Found` instead of `200`.

- Post-fix focused command (after teardown/readiness hardening): `cargo test --test router_integration global_router_mcp_idle_session_is_restored -- --nocapture` — passed (`1 passed`, `14 filtered`). The regression observed restored stale-session GET as `200`, accepted a post-restore global repo-addressed `tools/call` with `workspace_full_path`, and observed the deterministic tool-level `no embedding API keys configured` error; unknown-session GET remained `404`. This proves session/transport restoration, not a real retrieval or index result.
- Broader validation: `cargo test --locked` passed 818 tests across 15 suites with 18 ignored; `cargo check --locked --tests` passed; workflow YAML parsed successfully. Cross-target workflow execution remains unrun locally; formatter and linter were not run per scope.

## Result

Completed. The router-global MCP store change and operational documentation are verified. The focused regression passed after teardown/readiness hardening; `cargo test --locked` passed 818 tests across 15 suites with 18 ignored; `cargo check --locked --tests` passed; and workflow YAML parsed successfully. Cross-target workflow execution remains unrun locally. This documentation-only closeout introduces no production executable behavior changes.
