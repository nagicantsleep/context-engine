#!/usr/bin/env python3
"""Validate Codex subagent starts for the multi-agent workflow."""

from __future__ import annotations

import json
import sys
from typing import Any

KNOWN_AGENTS = {
    "task_explorer",
    "docs_researcher",
    "task_reproducer",
    "decision_specialist",
    "scoped_executor",
    "standard_executor",
    "test_runner",
    "semantic_verifier",
    "critical_reviewer",
}

WRITE_AGENTS = {
    "scoped_executor",
    "standard_executor",
    "task_reproducer",
}


def read_payload() -> dict[str, Any]:
    raw = sys.stdin.buffer.read().decode("utf-8-sig", errors="replace").strip()
    if not raw:
        return {}
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        print("Invalid SubagentStart payload: expected JSON", file=sys.stderr)
        sys.exit(2)
    return data if isinstance(data, dict) else {}


def main() -> int:
    if len(sys.argv) < 2 or sys.argv[1] != "start":
        print("usage: validate-workflow-action.py start", file=sys.stderr)
        return 2

    payload = read_payload()
    agent_type = str(
        payload.get("agent_type")
        or payload.get("agentType")
        or payload.get("name")
        or ""
    ).strip()

    if not agent_type:
        print(
            json.dumps(
                {
                    "systemMessage": "SubagentStart missing agent_type; continuing without enrichment."
                }
            )
        )
        return 0

    if agent_type not in KNOWN_AGENTS:
        print(
            f"Unknown workflow agent_type '{agent_type}'. "
            "Prefer one of the project .codex/agents roles.",
            file=sys.stderr,
        )
        # Advisory only: do not hard-block exploratory ad-hoc agents.
        print(
            json.dumps(
                {
                    "hookSpecificOutput": {
                        "hookEventName": "SubagentStart",
                        "additionalContext": (
                            f"Agent '{agent_type}' is outside the documented workflow roles. "
                            "Keep scope bounded and return a structured artifact."
                        ),
                    }
                }
            )
        )
        return 0

    write_note = (
        "Only one writer may modify the shared checkout at a time."
        if agent_type in WRITE_AGENTS
        else "Remain read-only; return a distilled artifact only."
    )
    print(
        json.dumps(
            {
                "hookSpecificOutput": {
                    "hookEventName": "SubagentStart",
                    "additionalContext": (
                        f"Workflow role '{agent_type}' started. {write_note} "
                        "Do not return raw transcripts to the main thread."
                    ),
                }
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
