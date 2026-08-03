# 0002 Multi-repo Namespace — Schema Design

Date: 2026-08-01

## Status

Accepted

## Context

Phase 2.2 goal: cross-repo symbol navigation — allow edges from a symbol in repo A to a
symbol in repo B. Each repo is currently a separate SurrealDB/RocksDB instance. FQNs use
absolute filesystem paths (`/abs/path/file::scope::name`), which are already globally unique
on a single machine.

The architect analysis (2026-08-01) evaluated three options:
- Option A: prefix FQN with a stable `repo_id` token → requires full record-id rewrite on
  every repo (expensive, risky).
- Option B: keep FQN format; extend Phase 2 resolution to query all loaded repo DBs →
  no migration, no FQN change, routing already works via `find_db_for_file`.
- Option C: merge all repos into one SurrealDB instance → incompatible with per-repo
  isolation architecture (generation counter, Theory-A crash recovery).

## Decision

**Option B.** Keep the existing FQN format unchanged. Extend Phase 2 edge resolution
(`flush_edge_batch` / `insert_edge` in `src/indexing/pipeline.rs`) to query all repos
currently open in `RepoDbMap` when resolving `to_name` candidates, instead of only the
current repo's DB.

User decisions (2026-08-01):
- **Q1 — Lazy materialization:** Accepted. Cross-repo edges appear after both repos are
  indexed and the calling repo is re-indexed. No eager back-fill required.
- **Q2 — Scope:** Any-to-any. All repos currently loaded in `RepoDbMap` are consulted
  during Phase 2 resolution. No explicit `depends_on` config needed.
- **Q3 — Portability:** Single-machine only. Absolute-path FQNs are sufficient; no
  `repo_id` prefix needed.
- **Q4 — Edge ownership:** Edges live in the caller's DB only. No replication to callee's
  DB. Scatter-gather across all DBs is acceptable at query time.
- **Q5 — Schema version bump:** Yes. Bump to v8 to mark that this DB was indexed with
  cross-repo awareness (diagnostic value; no DDL change).

## Alternatives Considered

1. **Option A (FQN prefix):** Rejected. Full record-id rewrite on large repos is
   write-amplified and cannot be done transactionally. The `repo_id` token is mutable —
   renaming it silently invalidates all stored edges.
2. **Option C (single DB):** Rejected. Incompatible with per-repo isolation architecture
   (generation counter, Theory-A write batching, Windows Defender lock drain mitigation).

## Consequences

Positive:

- No migration required. Existing per-repo DBs continue to work unchanged.
- No FQN format change. All existing query paths (`find_db_for_file`, BFS dispatch) work
  without modification.
- Cross-repo edges materialize on next incremental re-index of the calling repo.

Tradeoffs:

- Lazy materialization: cross-repo edges do not appear until the calling repo is re-indexed
  after the callee repo is loaded.
- Phase 2 resolution now queries N repo DBs instead of 1 — O(n) lookup cost as repo count
  grows (acceptable for personal use with few repos).
- Absolute-path FQNs are machine-local. Cross-repo edges are not portable across machines.

## Follow-Up

- Document the lazy-materialization behavior in `AGENTS.md` or `docs/product/README.md`
  so users know to re-index after adding a new repo.
- If multi-machine use is ever needed, revisit Option A (FQN prefix) — it will require a
  breaking migration.
