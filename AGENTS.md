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
curl http://127.0.0.1:6699/api/config  # worker-free smoke check
cargo run -- --port 8080         # custom port
cargo test                       # run test suite
```

The router handles the UI/config surface and starts per-repo workers for
action requests. Read-only detail routes use sidecars/cold state without
spawning. Global `/mcp` calls choose a repo with `workspace_full_path`; fresh
settings enable `codebase-retrieval`, while `file-retrieval` needs explicit UI/
settings enablement.

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
| Per-repo SurrealDB | generation 0: `~/.vibervn/context-engine/rocksdb/<name>/`; generation >=1: `~/.vibervn/context-engine/rocksdb/<generation>/<name>/` |
| Embedding cache | `~/.vibervn/context-engine/embeddings/` |
| Router sidecars | `~/.vibervn/context-engine/sidecar/` |

## Multi-agent engineering workflow

Codex project roles live under `.codex/agents/`. Claude Code roles live under
`.claude/agents/`. Product authority remains `docs/WORKFLOW.md`.

### Main-thread responsibility

The main Codex thread is the user-facing orchestrator and task-leader state
machine.

The main thread owns:

- communication with the user;
- TaskContract creation;
- task status and priorities;
- adaptive workflow routing;
- worker budgets;
- acceptance criteria;
- final reporting.

The main thread should not perform routine exploration, implementation, long
test runs, or raw-log analysis itself.

### Core principle

Do not delegate an entire engineering task to one premium reasoning agent.

Premium agents make bounded decisions.
Lower-cost agents perform bounded operations.
An independent agent verifies the result.

### TaskContract

Before changing code, maintain this internal contract:

```text
task_id:
objective:
reported_symptoms:
expected_behavior:
constraints:
acceptance_criteria:
allowed_scope:
prohibited_actions:
risk: low | medium | high | critical
uncertainty: low | medium | high
budget:
  max_parallel_agents:
  max_sol_calls:
  max_repair_loops:
  max_files_changed:
```

### Adaptive workflow

Select the smallest sufficient workflow.

#### Clear, low-risk change

`scoped_executor` -> `semantic_verifier`

#### Unknown file or code path

`task_explorer` -> `scoped_executor` -> `semantic_verifier`

#### Runtime failure

`task_explorer` + `task_reproducer`
-> `standard_executor`
-> `test_runner`
-> `semantic_verifier`

#### Unknown or ambiguous root cause

`task_explorer` + `task_reproducer`
-> `decision_specialist`
-> `scoped_executor` or `standard_executor`
-> `test_runner`
-> `semantic_verifier`

#### Architecture or critical change

`task_explorer` agents where independent evidence is useful
-> `decision_specialist`
-> `standard_executor` or exceptional complex implementation
-> `test_runner`
-> `critical_reviewer`

### Role boundaries

- `task_explorer`: EvidencePack only. No edits. No broad redesigns.
- `task_reproducer`: ReproductionReport only. No application source edits.
- `decision_specialist`: DecisionRecord only. Never implement.
- `scoped_executor`: small approved ChangeManifest. No redesign.
- `standard_executor`: bounded multi-file ChangeManifest. No scope expansion.
- `test_runner`: TestReport only. No application edits.
- `semantic_verifier`: VerificationReport only. No code changes.
- `critical_reviewer`: CriticalReviewReport only. No code changes.

### Cost controls

- Do not call `decision_specialist` before evidence collection.
- Default Sol call budget is zero for low-risk tasks.
- Medium-risk tasks may use one Sol call.
- High-risk tasks may use one Sol decision call and one Sol critical review.
- Do not call Sol twice with unchanged evidence.
- Do not use Sol for raw tool loops or log processing.
- Do not spawn an agent for work requiring only one or two focused reads.
- Use no more than four concurrent agents by default.
- Permit no more than one implementation agent to write to one checkout.
- Permit at most one repair loop by default.

### Repair loop

When verification fails:

1. Produce a FailurePacket.
2. Return the FailurePacket to the original executor.
3. Allow one bounded correction.
4. Run verifier again.
5. If the second verification fails, report BLOCKED.

Do not call a new implementation agent unless the original executor is
unavailable or the failure demonstrates that the original DecisionRecord was
invalid.

### User communication

Acknowledge the task before delegation.
Report only meaningful state changes.
Keep raw logs and agent transcripts outside the user-facing conversation.
The main thread owns the final answer.
