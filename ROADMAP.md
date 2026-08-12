# vibervn-context-engine — Roadmap

> Tổng hợp từ phân tích so sánh với Graphify, codegraph, CodeGraphContext, GitNexus, Sourcegraph, ast-grep, bloop, continue.dev (2026-08-01).

## Chiến lược

**vibervn làm core.** Không orchestrate/merge các tool bên ngoài — absorb có chọn lọc các gap cụ thể trực tiếp vào codebase Rust. Lý do: pipeline vibervn (embed → graph-expand → agentic rerank) đã hoàn chỉnh nhất về query quality và performance; các tools Python/external chỉ đóng góp narrow, separable value không đủ để justify runtime dependency hay fork.

---

## Phase 1 — Quick wins (vài ngày)

### 1.1 Edge confidence tagging
- **Nguồn cảm hứng:** Graphify (`EXTRACTED` vs `INFERRED`)
- **Mục tiêu:** Thêm `enum Confidence { Extracted, Inferred(f32) }` vào `RawEdge`; BFS expansion weight inferred edges thấp hơn → precision tốt hơn
- **Files:**
  - `src/parsing/relations.rs` — thêm confidence field vào `RawEdge`
  - `src/store/schema.rs` — schema extension (confidence column trong calls table)
  - `src/query/graph_expand.rs` — weight edges theo confidence trong BFS
- **Status:** `[x] done`

### 1.2 Thêm framework resolvers
- **Nguồn cảm hứng:** codegraph (17 frameworks)
- **Mục tiêu:** Port các resolvers còn thiếu — ít nhất: FastAPI, NestJS, Laravel, Rails
- **Files:**
  - `src/indexing/frameworks/mod.rs` — trait `FrameworkResolver` đã pluggable, thêm module mới
- **Status:** `[x] done`

---

## Phase 2 — Mid-term (vài tuần)

### 2.1 Cross-language bridging
- **Nguồn cảm hứng:** codegraph (Swift↔Objective-C, React Native↔Expo, Java↔Kotlin)
- **Mục tiêu:** Detect và model cross-language call edges trong polyglot repos
- **Cách implement:** `EdgeKind::CrossLanguageBridge` hoặc `FrameworkResolver` variant
- **Status:** `[x] done`

### 2.2 Multi-repo namespace
- **Mục tiêu:** Cross-repo symbol navigation through the existing absolute-path FQNs; resolve symbols across all loaded repository databases without changing the FQN format.
- **Hành vi:** Cross-repo edges materialize lazily after both repositories are indexed and the calling repository is re-indexed.
- **Schema:** Version v8 is a diagnostic bump marking cross-repo awareness; no DDL or FQN migration is required.
- **Quyết định:** [`docs/decisions/0002-multi-repo-namespace.md`](docs/decisions/0002-multi-repo-namespace.md)
- **Status:** `[x] done`

---

## Phase 3 — Defer (làm khi có evidence rõ về gap)

### 3.1 SCIP ingestion path
- **Nguồn cảm hứng:** Sourcegraph SCIP, CodeGraphContext
- **Mục tiêu:** Compiler-accurate symbols cho Java/C++ heavy repos — optional offline enrichment pass chạy SCIP indexer rồi upsert vào SurrealDB với high-confidence flag
- **Điều kiện trigger:** Khi có evidence rõ về unresolved symbol recall gap trên Java/C++ repos
- **Status:** `[ ] deferred`

---

## Out of scope

| Feature | Lý do |
|---------|-------|
| Graph visualization / Leiden clustering | UI concern — build separate nếu cần, không ảnh hưởng query quality |
| MCP meta-orchestration (fan-out sang Graphify/CodeGraphContext) | Impedance mismatch, latency compounding, maintenance overhead — không pragmatic |
| Fork Python tools vào repo | Overkill — giá trị narrow, không justify duy trì Python dep |

---

## Tham khảo

So sánh đầy đủ: các tool được phân tích gồm Graphify (YC S26, 99k⭐), codegraph/colbymchenry (Rust+SQLite, 63k⭐), CodeGraphContext (Python+SCIP), GitNexus, Grapuco (commercial SaaS), Sourcegraph SCIP, ast-grep, bloop (archived 2025), continue.dev.
