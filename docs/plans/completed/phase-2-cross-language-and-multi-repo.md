# Execution Plan: Phase 2 — Cross-language Bridging & Multi-repo Namespace

Date: 2026-08-01

## Status

Completed

## Outcome

1. Cross-language bridge edges detected and emitted for three polyglot scenarios (Swift↔ObjC, Java↔Kotlin, React Native↔Expo) via three new FrameworkResolvers.
2. Phase 2 edge resolution queries all loaded repo DBs (any-to-any), enabling cross-repo symbol navigation without FQN format changes.

## Context

- Strategy decision: [`docs/decisions/0001-core-strategy.md`](../../decisions/0001-core-strategy.md)
- Multi-repo schema decision: [`docs/decisions/0002-multi-repo-namespace.md`](../../decisions/0002-multi-repo-namespace.md)

## Scope

In scope:

- 2.1 Three cross-language resolvers: `swift_objc`, `java_kotlin`, `expo`.
- 2.2 Multi-repo namespace: extend Phase 2 resolution to query all loaded repo DBs; schema v8.

Out of scope:

- EdgeKind::CrossLanguageBridge variant (not needed — Calls + Confidence::Inferred is sufficient).
- FQN format change (absolute-path FQNs are already globally unique per machine).
- Eager cross-repo edge materialization (lazy is accepted).
- Replication of edges to callee's DB.

## Progress

### 2.1 Cross-language bridging

- [x] `src/indexing/frameworks/swift_objc.rs` — SwiftObjcResolver + tests
- [x] `src/indexing/frameworks/java_kotlin.rs` — JavaKotlinResolver + tests
- [x] `src/indexing/frameworks/expo.rs` — ExpoResolver + tests
- [x] All three registered in `src/indexing/frameworks/mod.rs`
- [x] `cargo check` passes

### 2.2 Multi-repo namespace

- [x] `src/store/mod.rs` — DB_SCHEMA_VERSION bumped to 8; `run_migration_v7_to_v8` added
- [x] `src/store/ops.rs` — `find_symbols_by_names_with_pos_multi` added
- [x] `src/indexing/pipeline.rs` — `repo_dbs` field, `collect_db_snapshot`, `load_all_symbols_multi`; all three Phase 2 resolution paths updated
- [x] `src/indexing/mod.rs` — `.with_repo_dbs(...)` wired in production path
- [x] `cargo check` passes

## Decisions

- 2026-08-01: Use FrameworkResolver (not EdgeKind::CrossLanguageBridge) for cross-language bridges — pipeline already filters for EdgeKind::Calls; new variant would require schema + BFS changes for no benefit.
- 2026-08-01: Multi-repo uses Option B (keep FQNs, extend resolution) — see `docs/decisions/0002-multi-repo-namespace.md`.
- 2026-08-01: `collect_db_snapshot` is async and must never hold the RwLock across `.await` — clone all handles first, drop guard, return Vec.

## Validation

- Focused proof: `cargo check` green (2.1 and 2.2).
- Full suite: `cargo test` — 14 passed, 0 failed (2026-08-01).
- Manual smoke test for 2.1: index a `.swift` + `.m` project; verify `calls` edges with confidence < 1.0 appear.
- Manual smoke test for 2.2: index two repos; verify cross-repo edges in caller's DB after re-index.

## Result

`cargo test` green — 14 passed, 0 failed (2026-08-01). `cargo build --release` succeeds. All implementation complete and verified.

Files changed in Phase 2:
- Created: `src/indexing/frameworks/swift_objc.rs`, `java_kotlin.rs`, `expo.rs`
- Modified: `src/indexing/frameworks/mod.rs`, `src/store/mod.rs`, `src/store/ops.rs`, `src/indexing/pipeline.rs`, `src/indexing/mod.rs`
- Added decisions: `docs/decisions/0002-multi-repo-namespace.md`
