# Agent Instructions

<!-- HARNESS:BEGIN -->
## Harness

Start with the requested outcome, then use the repository as the system of
record. Read `docs/WORKFLOW.md` and only the product, design, plan,
code, and validation material relevant to the task.

- Answers, explanations, reviews, diagnoses, plans, and status reports are
  read-only. Inspect only what is needed and do not mutate repository or Harness
  state.
- For a bounded change, use an ephemeral plan: inspect the affected behavior and
  existing proof, implement the change, and run behavior-appropriate validation.
  No control-plane operation is required.
- Create or update one file under `docs/plans/active/` when work spans sessions,
  needs coordination or an ordered sequence, has meaningful dependencies, or
  requires explicit recovery steps. Move it to `docs/plans/completed/` only
  after validation.
- Before editing, identify repository authority for each new externally
  observable policy. If materially different choices remain open, stop before
  edits; configurable defaults are not authority.
- Also pause when product intent remains ambiguous, an action is difficult to
  recover, validation would be weakened, or the request does not authorize the
  needed action.
- Claim completion only with relevant executable or observable evidence. Report
  the outcome, important changed surfaces, validation, and unresolved risks.

SQLite intake, story, trace, scoring, audit, and proposal commands are optional
compatibility features. Use them only when explicitly requested or required by
an external orchestrator.
<!-- HARNESS:END -->

## Project

**vibervn-context-engine** is a Rust binary (crate `context-engine-rs`) that indexes
repositories via Tree-sitter symbol extraction and Voyage AI embeddings, stores chunks
in SurrealDB (RocksDB backend), and exposes a Web UI and an MCP server.

### Build and run (local dev)

Requires a stable Rust toolchain and libclang (needed by the RocksDB bindgen build
dependency).

**Install libclang:**
- Windows: `choco install llvm --no-progress -y`
- Linux / WSL / macOS: `sudo apt-get install clang libclang-dev`

```bash
cargo build --release            # build
cargo run                        # router on 127.0.0.1:6699
cargo run -- --port 8080         # custom port
cargo test                       # run test suite
```

### Key paths

| Path | Purpose |
|------|---------|
| `src/main.rs` | CLI entry; dispatches router or worker mode |
| `src/runtime/router.rs` | Router boot, HTTP bind, shutdown |
| `src/engine_boot.rs` | Shared boot sequence (all binaries) |
| `src/config.rs` | Settings schema + migration chain (v1–v13) |
| `src/server.rs` | Axum + MCP route registration |
| `src/indexing/` | File watching, parsing, framework extractors |
| `src/query/` | Vector search, graph expand, LLM reranker |
| `docs/WORKFLOW.md` | Canonical agent task workflow |

### Default runtime paths

| Item | Default path |
|------|-------------|
| Settings | `~/.vibervn/context-engine/settings.json` |
| Per-repo SurrealDB | `~/.vibervn/context-engine/rocksdb/<name>/` |
| Embedding cache | `~/.vibervn/context-engine/embeddings/` |
