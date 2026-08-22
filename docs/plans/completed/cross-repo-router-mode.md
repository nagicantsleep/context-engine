# Execution Plan: Cross-repo navigation in router mode

Date: 2026-08-22

## Status

Completed 2026-08-22.


## Outcome

Cross-repo symbol navigation works in the default process-per-project
deployment: indexing repo A materializes call edges into repo B's symbols, and
query-time BFS expansion returns repo B chunks with live content through the
router, degrading gracefully when repo B's worker is unavailable.

## Context

- `docs/decisions/0002-multi-repo-namespace.md`: Option B (absolute-path FQNs,
  lazy materialization, edges owned by the caller's DB, scatter-gather at query
  time).
- `AGENTS.md` + `README.md`: router + per-repo worker runtime model; RocksDB
  holds one exclusive handle per repo directory, so a worker cannot open another
  live worker's DB (`src/indexing/load_repos_tests.rs` lock note).
- Current gap: `BootOptions::only_repo` filters each worker to one repo, so
  `IndexPipeline::with_repo_dbs` cross-repo resolution only works in
  standalone mode.
- User-approved design (2026-08-22): symbols sidecar for Phase 2 resolution,
  router callback for query-time chunk content, scope = index-time +
  query-time.

## Scope

In scope:

- Symbols sidecar `<data_dir>/sidecar/<sanitized>.symbols.json` (schema 1)
  written best-effort after successful full/incremental publish.
- Phase 2 merges other repos' sidecar symbols (precedence: own DB > live
  foreign DBs > sidecars).
- Worker endpoint `GET /api/graph-chunk` serving a chunk for an owned FQN.
- Router endpoint `GET /api/cross-repo/chunk` resolving the owning repo via
  `path_in_repo` over `settings.repos` and proxying through
  `acquire_and_proxy`.
- `CrossRepoResolver` threaded explicitly (no globals); workers receive
  `--router-url` via spawn args; router binds its listener before
  `build_router_app` so the port is known.

Out of scope:

- Cross-repo caller/callee stats enrichment (partial for foreign callees).
- Cross-repo vector search (per-repo shards unchanged).
- Agentic-RAG sub-query expansion through the resolver.
- Subprocess-level e2e test (follow-up; embedding-free indexing prerequisite).

## Approach

1. `router/sidecar.rs`: symbol sidecar types + write/read helpers + cleanup in
   `remove_all_sidecars`.
2. `indexing/pipeline.rs`: publish own symbol sidecar inside Phase 2 success
   paths (RAM buffer reuse; own-DB load fallback; incremental delta refresh);
   merge foreign sidecar symbols in RAM path and page-scan path.
3. `query/cross_repo.rs`: `CrossRepoResolver` (router base URL + reqwest
   client, 30s timeout) fetching a remote chunk row.
4. `query/graph_expand.rs`: endpoint fetch helper — strict repo-owner DB match
   first, resolver callback when the endpoint file is not local, per-expansion
   fetch cap; callback failure drops that subtree (matches today's
   missing-endpoint behavior; no fabricated empty-content rows).
5. `server.rs`: worker `graph-chunk` handler; `AppState.cross_repo`;
   `build_router` unchanged signature (None) + worker wiring with resolver.
6. Router: `--router-url` spawn arg, `/api/cross-repo/chunk` route,
   listener-before-app boot order; `main.rs` flag; worker init.
7. Tests per phase; docs updates (README runtime model, ROADMAP 2.2 note,
   decision 0002 follow-up).

## Risks And Recovery

- Risk: RocksDB exclusive lock forbids direct cross-worker DB opens — avoided
  by design (sidecars + callback only).
- Risk: stale sidecar symbols mirror the accepted lazy-materialization
  semantics (edges refresh when the caller repo re-indexes); sidecars are
  removed with the index.
- Risk: kernel-scale sidecar rewrite cost on surface-changing incrementals —
  accepted v1 limitation, documented; comment-only edits skip the rewrite
  (empty surface delta).
- Recovery: each phase is independently revertible; sidecar absence disables
  index-time cross-repo resolution; resolver absence (no `--router-url`)
  disables query-time expansion — both fall back to current behavior.

## Progress

- [x] Survey sidecar/spawn/CLI/router/Phase-2 mechanics.
- [x] Symbol sidecar writer/reader module.
- [x] Publish sidecar after successful index runs.
- [x] Phase 2 merge sidecar symbols of other repos.
- [x] Worker graph-chunk endpoint (with boot-pinned worker scope gate).
- [x] Router cross-repo proxy route (`/api/cross-repo/chunk`).
- [x] Thread CrossRepoResolver into graph_expand (+ engine/ops/MCP funnels).
- [x] Unit tests (sidecar ×5, phase2 sidecar-merge, incremental refresh,
      graph_expand callback success/failure/no-resolver, router route ×2).
- [x] README runtime-model note; ROADMAP 2.2 note; decision 0002 amendment.

## Decisions

- 2026-08-22: Explicit `Option<&CrossRepoResolver>` threading instead of a
  process global — keeps graph_expand unit-testable against per-test stub
  routers and mirrors the codebase's explicit-handle style.
- 2026-08-22: Callback failure drops the expansion subtree rather than
  emitting a reference-only empty-content row — fabricated rows would corrupt
  rerank input and UI rendering.
- 2026-08-22: Incremental sidecar refresh keyed on changed/deleted files;
  full-table reload only when no prior sidecar exists.

## Validation

- Focused proof: `cargo test sidecar`, pipeline cross-sidecar resolution test,
  graph_expand remote fetch + failure tests, router ownership test.
- Integration or end-to-end proof: in-process two-engine loopback proving
  BFS → callback → content; subprocess e2e deferred (noted above).
- Repository-required checks: `cargo test` for touched modules; `cargo build`.


Completed 2026-08-22. Verified end to end:

- `cargo check --all-targets`: pass.
- Unit/integration: sidecar ×5, pipeline cross-sidecar + incremental refresh,
  graph_expand callback (success/failure/no-resolver), router route ×2
  (unknown-owner native 404; owned-repo proxied to real worker), monolith
  integration 14/14, mcp_session_restore 2/2, router_integration 17/17.
- Subprocess e2e (`tests/e2e_cross_repo.rs`, ignored by default): two REAL
  workers via the router + mock Voyage gateway; query in repo A returns repo
  B's chunk content through sidecar-materialized edges and the
  `/api/cross-repo/chunk` callback. PASS in ~7s.
- Fix surfaced by e2e: zero-raw-edge repos now publish their symbol table
  (`publish_symbol_sidecar_from_db` on both Phase 2 empty fast paths) — a
  library repo was previously invisible to foreign resolution.
- Pre-existing failures unrelated to this work (verified on clean HEAD):
  1× pipeline recovery, 7× mcp file_retrieval_db_key Windows-path tests.

Limitations (documented, non-blocking): cross-repo callee stats partial;
agentic-RAG sub-queries bypass the resolver; kernel-scale sidecar rewrite cost
on surface-changing incrementals.

## Result

Complete. See the verified summary above; follow-ups live in the Limitations
list and ROADMAP 2.2 note.
