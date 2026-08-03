# Claude Code

Repository authority for product behavior, architecture, plans, and validation
lives in `AGENTS.md` and `docs/WORKFLOW.md`. This file defines the Claude Code
multi-agent operating policy only.

Start Claude Code as the orchestrator:

```bash
claude --agent orchestrator
```

## Multi-agent workflow policy

For implementation tasks, the main session remains the user-facing orchestrator.

Never delegate an entire engineering task to a premium planner, architect,
reviewer, or debugger.

Use adaptive routing:

- Obvious one-file fix:
  executor -> verifier

- Unknown code path:
  explorer -> executor -> verifier

- Unknown root cause:
  explorer -> decision-specialist(debugger) -> executor -> verifier

- Cross-module design:
  explorers -> decision-specialist(architect) -> executor -> verifier

- Review-only request:
  explorer when needed -> decision-specialist(reviewer)
  Do not invoke executor.

Premium specialist budget:
- default: zero calls for low-risk tasks;
- maximum: one call for medium-risk tasks;
- high-risk tasks require an explicit reason for each additional call.

Repair policy:
- maximum one executor repair loop by default;
- after the second verification failure, report BLOCKED;
- do not repeatedly call the premium specialist with unchanged evidence.

## Role boundaries

```text
Orchestrator: communication + TaskContract + LeaderReport consolidation
Task Leader:  owns one outcome; does not edit code
Explorer:     EvidencePack only
Decision:     DecisionRecord only (debugger | architect | planner | reviewer)
Executor:     ChangeManifest only
Verifier:     VerificationReport only; never edits
```

Depth is capped at 2 under the main conversation via
`.claude/settings.json` (`main -> task-leader -> workers`). Workers must not
spawn further agents.

## Hard guardrails

Tool allowlists are the first protection layer:

```text
Orchestrator: no Edit, Write, or Bash
Leader:       no Edit or Write
Explorer:     no Edit or Write
Specialist:   no Edit or Write
Executor:     Edit and Write allowed; Bash filtered by scripts/validate-agent-command.sh
Verifier:     no Edit or Write
```
