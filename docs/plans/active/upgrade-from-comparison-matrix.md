# Execution Plan: Upgrade from Comparison Matrix

Date: 2026-09-04

## Status

Active

## Outcome

context-engine closes the highest-value gaps vs Graphify, GitNexus, Serena, Repomix/Gitingest, Tabby, and Joern — better recall, token efficiency, and agent onboarding — while staying a Rust single-binary retrieval core per `docs/decisions/0001-core-strategy.md`.

## Context

- Product truth: `README.md` (embed → graph-expand → rerank pipeline, MCP `codebase-retrieval` + `file-retrieval`, router + per-repo workers, Web UI + SSE).
- Architecture: `src/query/engine.rs` + `graph_expand.rs` + `merger.rs` + `reranker.rs`; `src/indexing/frameworks/` (13 resolvers); `src/parsing/mod.rs` (`detect_language`, 22 langs); `src/embedding/voyage.rs` (`Provider::Voyage/OpenAI`); `src/mcp.rs` (2 tools); SurrealDB `kv-rocksdb` per-repo + sidecars.
- Decisions: `docs/decisions/0001-core-strategy.md` (CE is integration core; no MCP fan-out, no Python fork, no viz in core; SCIP deferred); `docs/decisions/0002-multi-repo-namespace.md` (lazy cross-repo, caller-owned edges).
- Roadmap: `ROADMAP.md` Phase 1–2 done (confidence, frameworks, cross-language bridges, multi-repo); Phase 3 SCIP deferred.
- Matrix evidence: primary-source GitHub READMEs for Graphify, GitNexus, Understand-Anything, Serena, Repomix, Gitingest, Tabby, Joern.

## Scope

In scope:

- Phase 1 quick wins: lexical + RRF fusion, surface edge confidence, token cap/counts, `setup` command, `list_repos`, secret redaction.
- Phase 2 mid-term: read-only `symbol-context` / `trace-path` / `impact` MCP tools, local/ONNX embedding provider, proto/vue (+ evidence-gated) languages, `graph.json` export.
- Validation per item: focused `cargo test`, `bench-query` before/after, `scripts/ab_bench.sh` / `graph_bench.sh`, `:6699/api/config` smoke + MCP call.

Out of scope:

- MCP meta-orchestration fan-out, forking Python tools, in-core graph visualization / Leiden clustering (per 0001).
- Symbol rename/edit write path, SCIP ingestion, CPG vuln DSL, docs/PDF/media nodes — evidence-gated Phase 3 only.

## Approach

Smallest coherent sequence; update when evidence changes it.

1. Retrieval quality first: lexical fallback + RRF (1.1), then surface confidence tags (1.2), then token cap/counts (1.3).
2. Agent onboarding: `setup` command (1.4), `list_repos` + multi-repo query scope (1.5), secret redaction (1.6).
3. Read-only tools: `trace-path` first (smallest), then `symbol-context`, then `impact` (2.1).
4. Portability: `graph.json` export (2.4) before offline embeddings (2.2) before new languages (2.3).
5. Phase 3 only on measured gap: SCIP, rename/edit, clustering, CPG.

## Risks And Recovery

- RRF/lexical fusion regresses latency: gate on `ab_bench.sh`; keep fusion behind flag until neutral.
- New grammar deps break build (vendored toolchain risk): one language per change, each with extractor + `FlowSpec` + tests.
- Offline provider splits vector spaces: `EmbeddingIdentity` isolates caches; mismatch triggers re-index, never mixed query.
- Recovery: each item independently revertible; per-repo DBs + sidecars untouched by query-path changes.

## Progress

- [ ] 1.1 Lexical fallback + RRF fusion (`src/query/lexical.rs`, `engine.rs`)
  - Policy (task-local, 2026-09-04): per-term SurrealDB CONTAINS pools over `chunk` (no new index/migration); RRF k=60 orders only, source scores preserved (vector cosine / lexical coverage) so UI % stays meaningful; default-off behind `CONTEXT_ENGINE_LEXICAL=1` until `ab_bench.sh` neutral; fused before filters + graph expansion, truncated to 2×top_k to preserve the expansion latency bound; lexical rows leave `symbol_kind` unset (no N+1).
  - State 2026-09-04: IMPLEMENTED but UNBENCHMARKED — `src/query/lexical.rs` (10 tests) + engine cut-in (primary + sub-query) verified by `cargo test --lib query::` (135 pass), `cargo check`, scoped rustfmt. Full `cargo test`: 798 pass / 8 fail — all 8 pre-existing on clean tree (7× `mcp::tests::file_retrieval_db_key_*` Windows-path, 1× pipeline `file_meta_absence_triggers_reprocessing`), unrelated to 1.1. `cargo build --release`: PASS (13m36s, 1 pre-existing unused-import warning). Latency gate NOT run: no `~/.context-engine`, no embedding keys in env, `ab_bench.sh` hardcodes a Windows repo path. Keep default-off until `bench-query --repo <PATH> --query <TEXT>` before/after + `ab_bench.sh` pass on a provisioned host.
- [x] 1.2 Surface confidence tags in MCP text (`graph_expand.rs`, `src/mcp.rs`)
  - Done 2026-09-04: per-edge `confidence` threaded through caller/callee stats queries → `CallerCalleeStats.callers_inferred/callees_inferred` → `CodeResult` → MCP headers as ` ~inferred` suffix (`[callers: foo, bar ~inferred]`); NULL = extracted = unmarked. Merge OR-propagates flags. Proof: `cargo test --lib -- query:: mcp::tests::merge` 146 pass (2 engine tag tests + 1 MCP merge test); scoped rustfmt; `git diff --check` clean.
- [x] 1.3 Token estimate + `max_tokens` cap (`src/mcp.rs`, `merger.rs`)
  - Done 2026-09-04: `estimate_tokens` (chars/4, char-count) + always-on `~N tokens` footer; `max_tokens` on global `codebase-retrieval`/`file-retrieval` args, threaded through `run_codebase_retrieval`/`run_file_retrieval`/`do_query` into `assemble_with_budget` (tighter of token-derived chars and 48K wins; `None` = legacy; repo/REST/chat pass `None`). Proof: 3 new tests (estimate units incl. CJK, footer, cap); `query:: + mcp::` 182 pass, only the 7 pre-existing path-separator failures; scoped rustfmt; `git diff --check` clean.
- [x] 1.4 `context-engine setup` MCP config + AGENTS.md snippet (`src/main.rs`, `config.rs`)
  - Done 2026-09-05: CLI twin of the Web UI "Auto Setup" button (`post_mcp_setup` in server.rs) — the same `mcp_setup::run_setup` writer under the same policies: repo must already be configured in `settings.repos` (matched through `normalize_repo_path`), the written URL path is always this repo's own `/mcp-repo/<sanitize_repo_name>`, prompt guidance is gated by `settings.enabled_mcp_tools` (never advertises a disabled tool), writes are atomic, and all machine-local configs are gitignored. Surface: clap subcommand `setup --repo <PATH> [--tool claude|codex|opencode|all] [--port] [--bind] [--url]` in `main.rs` (runs one-shot, never boots the engine); glue in `runtime/setup.rs` (exit 0 ok / 1 repo-or-file error / 2 usage-env error); new pure helpers in `mcp_setup.rs` (`parse_tool_list`, `build_endpoint_url`, `Target::name`, `Display for FileStatus`). `config.rs` needed no change (`ensure_dir_and_load` reused as-is).
  - Proof 2026-09-05: `cargo test --lib mcp_setup` 25 pass (5 new); `cargo test --lib` 808 pass / 8 fail — exactly the 8 pre-existing on clean tree (7× `file_retrieval_db_key_*` Windows-path, 1× `file_meta_absence_triggers_reprocessing`); `cargo check --all-targets` clean; scoped rustfmt; `git diff --check` clean. CLI smoke on a fake HOME: bad `--tool` → exit 2 + usage; unconfigured repo → exit 1 + configured-repos hint; registered repo → wrote `.codex/config.toml` (URL `http://127.0.0.1:6699/mcp-repo/tmp_ce_setup_demo`), `AGENTS.md` (codebase guidance only — file-retrieval correctly absent under default settings), `.gitignore`; second run all `[unchanged]`.
  - Follow-up: document the new command in README (en/vi/zh).
- [x] 1.5 Read-only `list_repos` + multi-repo query scope (`src/mcp.rs`, `server.rs`, `query/cross_repo.rs`)
  - Done 2026-09-05: new always-exposed MCP tool `list_repos` on BOTH the global `/mcp` handler and the per-repo `/mcp-repo/<name>` handler (per-repo variant marks the pre-bound workspace). Read-only by construction: reads the live settings snapshot + durable `RepoSidecar` per repo (`read_sidecar` — state / last_indexed_at / file_count / embedding model); never registers a repo, spawns a worker, or triggers indexing, so it is safe on the cold global route. Deliberately NOT gated by `enabled_mcp_tools` (commented at both gate sites): hiding discovery behind the same opt-in list as the retrieval tools would make default settings undiscoverable. Multi-repo scope delivery: `workspace_full_path` error messages (codebase + file retrieval, empty and unknown-path) now point agents at `list_repos`; the tool output itself lists valid `workspace_full_path` values, per-repo endpoint names, and the lazy cross-repo navigation note (decision 0002 semantics — no fan-out per 0001). `server.rs`/`query/cross_repo.rs` needed no change.
  - Proof 2026-09-05: `cargo test --lib mcp::` 48 pass / 7 fail — the 7 are exactly the pre-existing `file_retrieval_db_key_*` Windows-path baseline; 3 new tests (`run_list_repos` sidecar state + this-repo marking, empty-settings onboarding hint, error-hint via `run_file_retrieval` empty-workspace path); `cargo check --all-targets` clean; scoped rustfmt; `git diff --check` clean. rmcp no-arg tool form verified against rmcp-macros 1.7.0 (`schema_for_empty_input`; annotations syntax `annotations(read_only_hint = true)`).
  - Follow-up: document the tool in README (en/vi/zh) together with the 1.4 `setup` command; live MCP handshake smoke (`tools/list`) on a provisioned host.
- [x] 1.6 Secret-pattern redaction (`parsing/generated.rs`, `query/content_fence.rs`)
  - Done 2026-09-05: second content fence — `content_fence::redact_secrets` (pure string transform, 13 precision-first rules in one `LazyLock` table): vendor-shaped tokens (Anthropic/OpenAI/GitHub incl. fine-grained, Google, Slack, npm, Stripe, AWS, JWT, PEM private-key blocks) redact unconditionally as `[REDACTED:<kind>]`; `Authorization: Bearer/Basic/Token` and userinfo DSN URLs (`postgres://user:pass@…`, any scheme) keep the trusted prefix and drop the credential; the generic secret-assignment rule fires only on a secret-shaped value (token charset, contains a digit or ≥16 chars, no dots/colons/spaces) and preserves the identifier prefix — `db_password=hunter2hunter2` redacts, `password: String`, `x = process.env.API_KEY`, `token = serde_json::Value`, `host:port` URLs pass untouched. Idempotent (markers never re-match).
  - Wiring (all LLM-bound and output-bound content paths): `read_lines_from_fs` (single FS-read choke point — main query numbered text, file-retrieval, agentic `read`/`synth_pool_chunk`, merge re-reads), stored merge-pool content in `query/engine.rs` (reranker input + output fallback), stored chunks in `run_file_retrieval`, and the agentic `grep`/`read` tool outputs in `reranker.rs` (their results go to the rerank LLM as tool results). Redaction therefore happens BEFORE the rerank LLM payload is built, not only at final output. `parsing/generated.rs` needed no change.
  - Proof 2026-09-05: `cargo test --lib query::content_fence` 12 pass (8 new redaction tests incl. false-positive and idempotency cases); `cargo test --lib query::` 145 pass; `cargo test --lib` 818–819 pass with the same 8 pre-existing failures (7× `file_retrieval_db_key_*` Windows-path, 1× pipeline) — one extra intermittent failure `load_repos_tests::full_rebuild_window…` (RocksDB `count_chunks` visibility precondition under parallel load) appears and disappears across full-suite runs, is not in any touched path, and fails its own precondition, i.e. a pre-existing load flake, not a 1.6 regression. `cargo check --all-targets` clean; scoped rustfmt; `git diff --check` clean.
  - Follow-up: document the redaction behavior (and markers) in README together with the 1.4/1.5 follow-ups; consider an opt-out env var only if a real false-positive class shows up.
- [x] 2.1 Read-only `trace-path`, `symbol-context`, `impact` MCP tools
  - `trace-path` DONE 2026-09-05: new gated MCP tool on BOTH handlers (global takes `workspace_full_path`, per-repo variant pre-binds it). New `src/query/trace_path.rs`: `resolve_symbol` (bare `name` / `::name` / `file.rs::name` / full-FQN forms; name-column lookup + FQN-suffix filter + exact-FQN fast path; >1 hit → candidate list, never a guess; record-id `⟨…⟩` escaping stripped) and `trace_paths` (depth-bounded DFS over the repo's own `calls` table, `direction` callees/callers, per-path cycle guard, extracted edges outrank inferred, deterministic fqn ordering, expansion budget 512 + path cap 3 with honest `truncated` flag — depth-cap exhaustion is a stated bound, NOT partiality). Repo-local edges only (decision 0002: edge lives in caller's DB; no fan-out per 0001). Shared runner in `mcp.rs` opens the DB via the same `get_or_open` path as `file-retrieval`; requires schema ≥ 2 (`read_db_schema_version`); `~inferred` edge marks use the same `confidence < 1.0` rule as the 1.2 tags. Proof: `cargo test --lib query::trace_path` 9 pass (chained-path, caller-direction, ambiguity, depth-cap, cycle, inferred-vs-extracted, path-cap truncation, start==goal); `cargo test --lib` 828 pass / 8 pre-existing failures; `cargo check --all-targets` clean; scoped rustfmt; `git diff --check` clean.
  - `symbol-context` DONE 2026-09-05: new gated MCP tool on BOTH handlers (global takes `workspace_full_path`, per-repo pre-binds it; arg is a single `symbol` reference in the same accepted forms as trace-path). Shared runner `run_symbol_context` in `mcp.rs`: `trace_path::resolve_unique` (ambiguity → candidate listing) → `read_lines_from_fs` for the numbered definition source (secret-fenced by 1.6 for free; unreadable file degrades to metadata-only; truncated at 200 lines / 16K chars with an explicit note) → `query_caller_callee_stats` (now `pub(crate)`) for caller/callee counts + proximity-sorted names rendered through the SAME enriched tags retrieval uses. Enablers extracted for reuse: `resolve_unique` hoisted into `trace_path.rs` (run_trace_path now uses it too), `ResolvedSymbol.kind` surfaced. Proof: `cargo test --lib symbol_context` 2 pass (end-to-end over a real temp source file + seeded RocksDB DB: numbered source, kind, range, `[callers: other]` tag; missing-symbol actionable error); `cargo test --lib` 830 pass / 8 pre-existing failures; `cargo check --all-targets` clean; scoped rustfmt; `git diff --check` clean. Test note: hand the already-open DB handle to `repo_dbs` (get_or_open reuses it) — a second open of the same RocksDB dir fights the exclusive LOCK for the full 30s retry window.
- [x] 2.1 Read-only `trace-path`, `symbol-context`, `impact` MCP tools
  - `impact` DONE 2026-09-05: new gated MCP tool on BOTH handlers; completes 2.1. `trace_path::impacted` — reverse call-graph BFS over caller edges (reuses `neighbors`), levels grouped by hop distance (level 1 = direct callers), global visited set (a symbol reports once at its shallowest level, cycles absorbed), node budget 512 with honest `truncated` flag; depth default 3 / cap 8 (depth-cap exhaustion is a stated bound, not partiality). Runner `run_impact` in `mcp.rs` renders per-level listings (capped 40/level) plus a most-affected-files summary (top 5 by caller count) for triage; unused-symbol output explains the three possible reasons (unused / entry point / cross-repo callers). Proof: `query::trace_path` 14 pass (5 new: level grouping+dedup, cycle absorption, depth-cap, budget truncation, unused symbol); `mcp::tests::impact_*` 2 pass (end-to-end 2-level analysis over seeded RocksDB incl. most-affected summary; missing-symbol error). Two test-authored SurrealQL pitfalls recorded: backtick-quoted INSERT values parse as record IDs, not strings (bind params instead), and NULL columns fail serde i64 deserialization even with `#[serde(default)]` (use `Option<i64>`).
  - Suite note: the known load flake resurfaced as its SIBLING `load_repos_tests::incremental_window_does_not_search_stale_resident_shard` in one full run (RocksDB delete-visibility precondition under parallel load; 12/12 pass scoped). Baseline remains the 8 pre-existing failures.
  - Follow-ups unchanged: README (en/vi/zh) for the new tools; live `tools/list` handshake smoke on a provisioned host.
- [x] 2.4 `graph.json` export (nodes/edges/confidence)
  - Done 2026-09-05: `context-engine export-graph --repo <PATH> [--out FILE] [--max-nodes N] [--max-edges N] [--data-dir PATH]` writes the portable artifact (format `context-engine-graph/v1`) that the UI's bounded `/graph` payload deliberately is not: full nodes/edges with per-edge `confidence` + derived `inferred` (same `confidence < 1.0` rule as the MCP tags), RFC3339 stamp, schema version. Contract enforced by `src/export.rs::build_graph_export`: edges drive the node set (edgeless symbols excluded); stale edges with endpoint-less symbols are DROPPED and counted (`dangling_edges_dropped`) — no dangling refs ever emitted; duplicate edges collapse extracted-first (same rule as trace-path neighbors); `truncated` reports a caps hit only. v1 (pre-FQN) indexes are rejected with re-index guidance. Data-dir resolution mirrors boot precedence (CLI > env `CONTEXT_ENGINE_DATA_DIR` > settings > builtin). Proof: `cargo test --lib export::` 5 pass (shape+confidence, extracted-first dedup, dangling drop+count, caps→truncated, v1 rejection); `cargo test --lib` 842 pass / 8 pre-existing failures; `cargo check --all-targets` clean; CLI smoke on fake HOME: `--help` registers, empty DB → clean schema error exit 1, no artifact written; scoped rustfmt; `git diff --check` clean.
- [x] 2.2 Local/ONNX embedding provider + Ollama provider and docs
  - Ollama: native `/api/embed`, keyed/no-key auth, strict provider parsing, provider-aware indexing/query construction, cache namespace, and en/vi/zh docs.
  - ONNX: provider-neutral client, tract-onnx/tokenizers loader, named reordered `input_ids`/`attention_mask`/optional `token_type_ids`, dynamic-axis output validation, masked mean pooling, L2 normalization, model+tokenizer+contract fingerprint identity, config migration v14→v15, factory wiring, and en/vi/zh docs. Proof: exact real-fixture test `cargo test --lib embedding::onnx::tests::loads_and_runs_real_reordered_three_input_fixture -- --exact --nocapture` passed; fixture is test-only base64 generated with ephemeral `onnx`/`tokenizers` tooling and covers Cast/Add/Unsqueeze/Concat operators; migration test passed; ONNX suite 4 passed; Voyage/Ollama suite 14 passed; `cargo check --all-targets`, formatting check, and `git diff --check` passed.
- [ ] 2.3 proto, vue grammars; Angular/Next.js/SvelteKit resolvers

## Decisions

- 2026-09-04: Keep 0001 boundaries — no fan-out, no Python fork, no in-core viz; absorb in Rust only.
- 2026-09-04: Suggested order 1.1 → 1.2 → 1.3 → 1.4 → 1.5 → 2.1 (trace-path first) → 2.4 → 2.2 → 2.3.
- 2026-09-05: `setup` CLI intentionally shares the Web UI auto-setup policy byte-for-byte (`mcp_setup::run_setup` + the `post_mcp_setup` validation rules) instead of defining a second config-writing dialect; client-config formats live only in `mcp_setup.rs`.
- Promote lasting product/architecture choices into `docs/decisions/`.

## Validation

- Focused proof: `cargo test <module>` per item (parsing/frameworks/query).
- Integration or end-to-end proof: `bench-query --repo <PATH> --query <TEXT> --top-k N` before/after; MCP call with `workspace_full_path`.
- Repository-required checks: `scripts/ab_bench.sh`, `scripts/graph_bench.sh`; `curl :6699/api/config` smoke.

## Result

Phase 1 complete except 1.1's latency gate (implemented, default-off, host-blocked: no `~/.context-engine`/embedding keys here, `ab_bench.sh` is Windows-hardcoded). Phase 2: 2.1 complete (`trace-path`, `symbol-context`, `impact`), 2.4 complete (`export-graph` CLI). Remaining: 2.2 (local/ONNX + Ollama embedding provider — needs a provider-dependency decision before implementation) and 2.3 (proto/vue grammars + resolvers — one language per change per the risk note). README documentation (en/vi/zh) for the new CLI/MCP surfaces is a shared follow-up. Record verified outcome, limitations, and follow-up before moving to `docs/plans/completed/`.
