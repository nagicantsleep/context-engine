# Execution Plan: Merge upstream vibervn-context-engine (2026-09-27)

Date: 2026-09-27

## Status

Completed

## Outcome

`master` contains the upstream content added since v0.1.71 — `ff7410e`
(feat(mcp): progress heartbeats) and `ed377ea` (feat(mcp): persist sessions so
idle workers restore instead of 404) plus the v0.1.74 release state — while the
local gap-closure work (MCP proxy tools, export graph, hybrid fusion) stays
intact. Full test suite green. Version moves to 0.1.75.

## Context

- Upstream: `github.com/nullmastermind/vibervn-context-engine`, master =
  `98b61f7` (v0.1.74), snapshotted locally at `refs/heads/upstream-snapshot`.
- Merge base: `38dd6ec` (v0.1.71, 2026-07-28). Local ahead ~30 commits,
  upstream ahead 9 (3 with real content; UI/v0.1.72 commits are content-equal
  cherry-picks already present locally under different SHAs).
- Dry-run `git merge-tree --write-tree HEAD FETCH_HEAD` flagged 9 conflicted
  paths: `Cargo.lock`, `Cargo.toml`, `src/assets/mod.rs`,
  `src/mcp.rs`, `src/mcp_session_store.rs`, `src/router/mod.rs`,
  `src/router/routes.rs`, `src/server.rs`, `tests/router_integration.rs`.
- Local heavy edits: commit `206a3f5` (global /mcp proxy tools, graph export,
  `src/assets/graph.html`). Upstream heavy edits: heartbeats
  (`src/mcp/progress.rs`, new file, merges clean) and session persistence
  (`src/mcp_session_store.rs` +541).

## Scope

In scope:

- Merge upstream master into local master via a working branch.
- Hand-resolve the 9 conflicted paths.
- Version bump to 0.1.75 (avoid collision with upstream-released 0.1.73/0.1.74
  and the local npm `ncs-context-engine@0.1.73`).
- Full `cargo check --all-targets` + `cargo test`.

Out of scope:

- Push to `origin`, npm/crates publish, new features, ROADMAP changes.

## Approach

1. Branch `merge-upstream-20260927`; commit this plan on it.
2. `git merge upstream-snapshot`; resolve the 9 files:
   - `Cargo.toml`: keep local deps/features + upstream's added dependency;
     version 0.1.75.
   - `Cargo.lock`: regenerate via cargo after `Cargo.toml` is resolved.
   - `src/assets/mod.rs` (both-added): keep local `graph.html` embed plus any
     upstream additions.
   - `src/mcp.rs`, `src/mcp_session_store.rs`, `src/router/mod.rs`,
     `src/router/routes.rs`, `src/server.rs`, `tests/router_integration.rs`:
     keep the local MCP proxy/graph surface and graft upstream heartbeats +
     session persistence into it.
3. `cargo check --all-targets`, then `cargo test`.

## Risks And Recovery

- Risk: semantic break between the local MCP proxy surface and upstream's
  persistent-session logic touching the same files.
  Mitigation: keep upstream's `tests/mcp_session_restore.rs` and the new
  `tests/router_integration.rs` cases compiling and passing alongside local
  cases.
- Risk: long build/test cycles on the RocksDB/bindgen dependency.
  Mitigation: budget time; do not parallel-edit while cargo holds the lock.
- Recovery: all work happens on `merge-upstream-20260927`; `master` is not
  touched until the suite is green. `git merge --abort` backs out of a partial
  resolution; `upstream-snapshot` and `origin/master` preserve both sides.

## Progress

- [x] Plan committed on the working branch
- [x] Merge started, conflicts enumerated (9 paths, as predicted)
- [x] Cargo.toml/Cargo.lock resolved (0.1.75; upstream's
      `Win32_Security_Authorization` feature auto-merged)
- [x] src/assets/mod.rs resolved (local superset: `serve_graph_page`)
- [x] src/mcp.rs resolved (5 blocks: upstream `with_progress_heartbeat`
      wrapper + local extra args; dropped duplicate `service::RoleServer`
      import)
- [x] src/mcp_session_store.rs resolved (doc: per-process persistent stores;
      upstream disk-persist section kept verbatim)
- [x] src/router/mod.rs resolved (router `/mcp` now uses upstream's
      `with_persist` store; local fresh-store override removed)
- [x] src/router/routes.rs + src/server.rs resolved (kept `graph.html` route)
- [x] tests/router_integration.rs resolved (local tests kept; upstream
      `initialize_mcp`/`post_mcp_ping` helpers appended — their caller test
      auto-merged at the top of the file)
- [x] cargo check --all-targets clean (pre-existing warnings only)
- [x] cargo test green (897 lib + 16 integration + 4 mcp_session_restore +
      19 router_integration; true `cargo test` exit 0)
- [x] Merge commit; branch fast-forwarded into master
- [x] Plan moved to docs/plans/completed/

## Decisions

- 2026-09-27: version jumps to 0.1.75, not 0.1.74 — upstream already released
  0.1.73 and 0.1.74, and local already published npm `ncs-context-engine@0.1.73`;
  a fresh number avoids ambiguous tags/packages.
- 2026-09-27: merge uses a local `upstream-snapshot` branch ref instead of
  adding a persistent git remote (no config mutation).
- 2026-09-27: router global `/mcp` adopts upstream's persistent
  `BoundedSessionStore::with_persist` and the local fresh in-memory store
  override is removed — upstream's ed377ea is the evolved form of the same
  intent (store present + survives process restart), and local's proxy
  handler signature (home/data dirs for router-side list_repos) is kept.
- 2026-09-27: `src/mcp.rs` tool handlers keep local's extra call args
  (`cross_repo`, `max_tokens`) inside upstream's
  `with_progress_heartbeat(ctx.peer, &ctx.meta, ctx.ct, MCP_PROGRESS_HEARTBEAT, …)`
  wrapper; local's `service::RoleServer` import is dropped because the merged
  import tree already brings `rmcp::RoleServer` (same type) from upstream.

## Validation

- Focused proof: upstream `tests/mcp_session_restore.rs` + new
  `tests/router_integration.rs` cases pass alongside local cases.
- Integration or end-to-end proof: full `cargo test`.
- Repository-required checks: `cargo check --all-targets`.

## Result

Completed 2026-09-27 (branch `merge-upstream-20260927`, merge commit
`4cb4a82`, then fast-forwarded into `master`).

Verified outcome:

- Upstream `ff7410e` (progress heartbeats), `ed377ea` (persistent MCP
  sessions), and the v0.1.74 release state are merged; local MCP proxy/graph
  surface and hybrid-fusion work intact; version 0.1.75.
- `cargo check --all-targets` clean (pre-existing warnings only).
- Full `cargo test` exit 0: 897 lib + 16 integration + 4
  mcp_session_restore + 19 router_integration passed, including upstream's
  new session-restore and heartbeat tests running against the merged
  router store wiring.

Limitations:

- `indexing::load_repos_tests::incremental_window_does_not_search_stale_resident_shard`
  failed once under full-suite parallel load (precondition assertion at
  load_repos_tests.rs:617), then passed 3/3 isolated and in the final full
  run. The merge touches no `src/indexing/` file (verified via
  `git diff HEAD --name-only -- src/indexing/` = empty), so this is a
  pre-existing local flake, not a merge regression. Worth a dedicated
  deflake pass later.

Follow-up (not attempted here):

- Push `master` to `origin` (user's call).
- npm/crates publish for 0.1.75 was not requested.
