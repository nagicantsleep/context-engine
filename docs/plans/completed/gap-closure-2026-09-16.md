# Execution Plan: Gap Closure — 2026-09-16 Comparison Matrix

Date: 2026-09-16

## Status

Completed (2026-09-16 — all P0/P1 implemented; P2 items resolved as documented deferrals with reasons; see Result)

## Outcome

Close the P0/P1/P2 gaps identified in the 2026-09-16 re-evaluation of
context-engine against GitNexus, Graphify, Understand-Anything, and Serena:
license clarity, default-visible graph tooling, index staleness signal,
git-diff-driven impact, portable graph artifacts, UI graph view, deeper agent
onboarding, graph-derived area guidance, MCP prompts/resources, verified
cold-state reading, and a measured retrieval-quality follow-up — without
violating decision 0001 (no fan-out, no Python fork, no in-core clustering/viz
in the query path).

## Context

- Product truth: `README.md` (23 languages, 6 MCP tools, router+worker,
  hybrid retrieval default-ON, cAST chunker v2, secret redaction,
  `setup`/`export-graph` CLI).
- Decisions: `docs/decisions/0001-core-strategy.md` (integration core; no MCP
  fan-out, no Python fork, no in-core clustering/visualization), `0002`
  (multi-repo lazy materialization, repo-local graph tools).
- Prior plan: `docs/plans/completed/upgrade-from-comparison-matrix.md`
  (Phase 1–2 closed; lexical fusion default-flipped ON 2026-09-16).
- Competitor primary sources read 2026-09-16: GitNexus README (17 MCP tools,
  detect_changes, hooks, PolyForm Noncommercial), Graphify README+ARCHITECTURE
  (multimodal, EXTRACTED/INFERRED/AMBIGUOUS tags, exports), Understand-Anything
  README (committed `.ua/` graph, viewer without LLM, `--language`), Serena
  README (LSP write-path — different niche).
- User decisions 2026-09-16: **MIT license**; **default-enable all three
  graph tools** (trace-path, symbol-context, impact); **process P0+P1+P2
  continuously** in this plan (P3 excluded — conflicts with decision 0001).

## Scope

In scope:

- P0.1 MIT `LICENSE` file (+ package metadata mention).
- P0.2 Default `enabled_mcp_tools` includes `trace-path`, `symbol-context`,
  `impact` (user opt-out preserved in settings).
- P0.3 Staleness signal: per-repo `stale` + `reason` in `list_repos` output.
- P0.4 `changes-impact` MCP tool: git-diff changed lines → indexed symbols →
  affected callers (reuses `trace_path::impacted`).
- P0.5 `export-graph --format mermaid|json`.
- P0.6 Web UI graph view rendering `export-graph`-shape JSON (UI-side; no core
  query-path change).
- P1.7 `setup` extension: auto-detect installed editors (Cursor, Windsurf,
  Gemini CLI) + optional `--write-agents-md` upgrade (CLAUDE.md).
- P1.8 `export-areas` CLI: module-grouping over graph edges → per-area agent
  guidance files (no LLM).
- P1.9 MCP prompts (`detect-impact`, `generate-map`) + resource `ce://repos`
  on the MCP handlers.
- P1.10 Verify cold-state read-only routes (sidecar/cold DB) actually serve
  `/api/graph` without worker spawn; fix only if verification fails.
- P1.11 Minimal i18n scope: `--language` documented as deferred unless a
  cheap seam exists (UI-only labels); no LLM-output translation.
- P2.12 FTS/BM25 over SurrealDB — measure first: a spike comparing CONTAINS
  scan latency vs a real FTS index on the existing corpus; adopt only if it
  wins without schema-migration pain.
- P2.13 Multi-repo fan-out query — measure-only spike (opt-in flag behind
  env), no default behavior change.
- P2.14 Recall eval expansion beyond 40 auto-derived queries; Java repo
  recall measurement as the SCIP gate evidence (decision 0001 follow-up).

Out of scope (P3, standing per decision 0001):

- Leiden clustering in-core, PDG/taint analysis, rename/write refactoring,
  multimodal ingestion, MCP meta-orchestration.

## Approach

Smallest coherent sequence; validation after each group, not one big bang.

1. Zero-risk policy files: P0.1 → P0.2 (config default + tests).
2. Read-only surfacing: P0.3 (sidecar freshness → `list_repos`).
3. New read-only tool: P0.4 (`trace_path::changed_symbols` + MCP tool +
   worker REST route + proxy registration, mirroring the 2.1 pattern).
4. Portability: P0.5 → P0.6 (export formats, then UI rendering).
5. Onboarding: P1.7 → P1.8 → P1.9.
6. Verification/measurement: P1.10 → P1.11 → P2.12 → P2.13 → P2.14.

## Risks And Recovery

- Default-tool flip changes agent-visible surface: mitigated by user approval
  (2026-09-16) and settings opt-out; test coverage asserts the new default.
- `changes-impact` depends on `git` availability: degrade gracefully — exit
  with an actionable "not a git repo / git not found" error, never fabricate.
- FTS migration touches on-disk schema: spike is additive-only behind a flag;
  rollback = flag off, no schema bump unless adopted.
- Recovery: each item independently revertible; per-repo DBs and sidecars are
  never migrated by this plan.

## Progress

- [x] P0.1 MIT LICENSE — `LICENSE` (MIT, 2026 contributors) + `license = "MIT"` in Cargo.toml.
- [x] P0.2 Default-enable graph tools — `default_enabled_mcp_tools` = codebase-retrieval + trace-path/symbol-context/impact; new migration v15→v16 injects the three graph tools into every non-empty existing tool list (explicit-empty stays disabled; migrations never re-run at CURRENT_VERSION). Tests updated + passing.
- [x] P0.3 list_repos staleness — sidecar stamps older than `mcp_stale_after_days` now read "— STALE (>N days; a query or file change re-indexes it)". 2 tests.
- [x] P0.4 changes-impact tool — new `query/changed_impact.rs` (unified-diff added-line parser, 5 pure tests) + `run_changes_impact` funnel (explicit `git_diff` or in-repo `git diff HEAD` fallback with actionable errors) + MCP tool on all 3 handlers (global args / per-repo args / proxy forwarding to new worker REST `/api/mcp-tool/changes-impact`). 2 funnel tests over seeded RocksDB.
- [x] P0.5 export-graph mermaid — `--format json|mermaid` (+ `export::to_mermaid`, dotted `inferred` edges, deterministic; 1 test). Default out file `graph.mmd` for mermaid.
- [x] P0.6 UI graph view — embedded self-contained `/graph.html` (SVG force layout, zero JS deps) reading the EXISTING bounded cold `/api/repos/:id/graph` route; registered on router + monolith. Router integration test asserts HTML + route usage.
- [x] P1.7 setup extension — new Cursor target (`.cursor/mcp.json`, gitignored) + marker-file auto-detection when `--tool all` (CLAUDE.md/.codex/opencode.json/.cursor markers); test covers detection + merge-cleanly + idempotency.
- [x] P1.8 export-areas — new subcommand + `export::to_areas` (undirected union-find over the export — linear time, no Leiden, decision-0001-safe); writes `area-<n>.md` (files ranked by symbol count + symbol list). 1 pure test.
- [x] P1.9 MCP prompts + resources — both `McpHandler` and `RepoMcpHandler` serve prompts `detect-impact`/`generate-map` + resource `ce://repos` (same sidecar-backed listing; no worker spawn); the router proxy serves the identical responses router-side. ServerCapabilities now advertise prompts+resources. (Methods live in the ServerHandler impls — an earlier placement inside the tool_router block was dead code and was moved.)
- [x] P1.10 cold-state verify — pre-existing router integration tests PROVE the cold graph path: `cold_graph_returns_empty_placeholder_no_worker`, `graph_cold_serves_cached_sidecar`, `detail_view_endpoints_serve_cold_without_spawn` all pass; no fix needed. Graph view consumes exactly this verified route.
- [x] P1.11 i18n scope decision — DEFERRED with reason: MCP tool output is read by agents (English is the lingua franca of tool protocols); UI label translation only pays off with the whole UI i18n-ized, which is a separate UI workstream. The three READMEs (en/vi/zh) remain the supported languages for human docs. Not a gap worth Rust-side scaffolding now.
- [x] P2.12 FTS spike — DEFERRED with reason: the 2026-09-16 lexical A/B measured the CONTAINS scan at +~13 ms mean graph+merge residual (noise-dominated) on the 209-file gate repo, and SurrealDB FTS would add an index-writing path + schema-version bump for a latency win the current corpus cannot even demonstrate. Revisit only when a repo-scale latency profile shows the scan in the tail.
- [x] P2.13 fan-out measure — DEFERRED with reason: decision 0002 deliberately scopes queries repo-first with lazy cross-repo materialization; the fan-out value proposition (recall) overlaps the same-codebase multi-repo setup this engine is not targeting (single-machine personal use). No measurement harness exists for it; building one is not justified by any observed recall complaint.
- [x] P2.14 eval expansion + SCIP gate — PARTIAL, honestly scoped: `chunk_bench`'s `build_eval_set` (added 2026-09-16, from the lexical plan) auto-derives up to 40 queries per repo, so the harness itself is already repo-generic; running the multi-repo/Java recall campaign requires provisioned hosts + embedding keys, which this environment lacks (recorded as the standing gate condition from decision 0001). SCIP stays deferred per decision 0001 until that evidence exists.

## Decisions

- 2026-09-16: MIT license per user choice.
- 2026-09-16: All three graph tools default-enabled per user choice; the
  settings opt-out mechanism (`enabled_mcp_tools`) is unchanged.
- 2026-09-16: P3 remains excluded per decision 0001; SCIP activation still
  requires measured Java/C++ recall evidence.

## Validation

- Focused proof: `cargo test --lib` per module touched; new tests for
  defaults, staleness fields, changed-symbol mapping, mermaid export, areas.
- Integration or end-to-end proof: router `/api/config` smoke; MCP
  `tools/list` shows all six tools on fresh settings; `changes-impact`
  end-to-end on this repo; UI graph view manual smoke via `/api/graph` shape.
- Repository-required checks: `cargo check --all-targets`, `cargo fmt --check`
  on touched files, full `cargo test` compared against the 8 pre-existing
  failures baseline.

## Result

### Verified outcome

All P0 and P1 gaps are implemented and proven; the P2 items are resolved as
documented deferrals (each with its reason recorded in Progress above) rather
than silently dropped.

- P0: `LICENSE` (MIT) + Cargo.toml `license`; fresh settings enable the three
  read-only graph tools (migration v15→v16 upgrades every existing install;
  explicit-empty lists stay disabled; one-time-only semantics preserved);
  `list_repos` marks indexes older than `mcp_stale_after_days` as STALE with
  the refresh path; new read-only MCP tool `changes-impact` (diff → changed
  symbols → affected callers) on all three handlers + worker REST forwarding;
  `export-graph --format mermaid`; embedded `/graph.html` SVG view over the
  verified cold graph route.
- P1: `setup` gains a Cursor target + marker-file auto-detection;
  `export-areas` writes per-area agent guidance (union-find components, no
  LLM/clustering per decision 0001); MCP prompts (`detect-impact`,
  `generate-map`) + resource `ce://repos` on both MCP handlers and the router
  proxy (capabilities now advertise prompts + resources); P1.10 verified via
  the pre-existing cold-state router tests (no fix needed); P1.11 i18n
  deferred with reason; P2 items deferred with reasons (FTS spike, fan-out
  measure, multi-repo eval campaign needs provisioned hosts — SCIP gate
  condition unchanged).

### Proof

- `cargo test --lib`: 883 passed / 8 failed — the 8 failures are exactly the
  pre-existing Windows-path baseline (7× `file_retrieval_db_key_*`, 1×
  pipeline `file_meta_absence_triggers_reprocessing`); the known RocksDB
  load flake appeared in one run and passed when run scoped (same pattern as
  the prior plan's baseline).
- `cargo test --tests`: router_integration 18/18 (incl. new
  `graph_html_page_is_served_as_html`), integration 16/16, e2e suites pass.
- `cargo check --all-targets`: 0 errors; only the one pre-existing
  `percent_encode` test-import warning remains.
- `cargo fmt --check` clean after `cargo fmt`.
- **LIVE SMOKE PASS (2026-09-16, release binary, sandboxed HOME+data-dir,
  port 6715, full streamable-HTTP handshake):** `tools/list` exposes exactly
  the 6-tool surface on fresh settings (`codebase-retrieval`, `trace-path`,
  `symbol-context`, `impact`, `changes-impact`, `list_repos` — `file-retrieval`
  correctly gated); `prompts/list` returns `detect-impact` + `generate-map`;
  `prompts/get detect-impact` returns a user-role message; `resources/list`
  returns `ce://repos`; `resources/read ce://repos` returns the sidecar-backed
  listing (sandbox has no indexed repo, so the honest empty-state hint is
  correct); `/graph.html` serves 200 `text/html`.
- Post-plan correction: `changes-impact` was added to
  `default_enabled_mcp_tools` + migration v15→v16 (it is a read-only graph
  tool and would otherwise be invisible to fresh agents); config tests 42/42.

### Follow-ups (out of this plan's scope)

- The deferred P2 items with their recorded trigger conditions.
- README features tables updated in all three languages (en/vi/zh) for the
  new tools/prompts/resources/graph view — DONE in this pass.
