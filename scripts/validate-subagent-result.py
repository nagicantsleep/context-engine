#!/usr/bin/env python3
"""Validate Codex subagent stop results for required workflow artifacts."""

from __future__ import annotations

import json
import sys
from typing import Any

REQUIRED_MARKERS: dict[str, tuple[str, ...]] = {
    "task_explorer": ("EvidencePack", "relevant_files", "observed_behavior"),
    "docs_researcher": ("verified behavior", "source reference", "unresolved"),
    "task_reproducer": ("ReproductionReport", "reproduced", "observed_result"),
    "decision_specialist": ("DecisionRecord", "confidence", "minimal_change"),
    "scoped_executor": ("ChangeManifest", "files_changed", "behavior_changed"),
    "standard_executor": ("ChangeManifest", "files_changed", "behavior_changed"),
    "test_runner": ("status:", "commands:", "failed:"),
    "semantic_verifier": ("VerificationReport", "PASS", "FAIL_"),
    "critical_reviewer": ("status:", "critical_findings", "residual_risks"),
}


def read_payload() -> dict[str, Any]:
    raw = sys.stdin.buffer.read().decode("utf-8-sig", errors="replace").strip()
    if not raw:
        return {}
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        print("Invalid SubagentStop payload: expected JSON", file=sys.stderr)
        sys.exit(2)
    return data if isinstance(data, dict) else {}


def message_text(payload: dict[str, Any]) -> str:
    parts: list[str] = []
    for key in ("last_assistant_message", "lastAssistantMessage", "message", "output"):
        value = payload.get(key)
        if isinstance(value, str) and value.strip():
            parts.append(value)
    transcript = payload.get("agent_transcript_path") or payload.get("agentTranscriptPath")
    if isinstance(transcript, str) and transcript.strip():
        try:
            with open(transcript, encoding="utf-8", errors="replace") as handle:
                # Only scan the tail to keep the hook cheap.
                content = handle.read()[-20000:]
                parts.append(content)
        except OSError:
            pass
    return "\n".join(parts)


def main() -> int:
    payload = read_payload()
    agent_type = str(
        payload.get("agent_type")
        or payload.get("agentType")
        or payload.get("name")
        or ""
    ).strip()
    stop_hook_active = bool(
        payload.get("stop_hook_active")
        if "stop_hook_active" in payload
        else payload.get("stopHookActive", False)
    )

    markers = REQUIRED_MARKERS.get(agent_type)
    if not markers:
        return 0

    text = message_text(payload)
    lowered = text.lower()
    missing = [marker for marker in markers if marker.lower() not in lowered]

    # semantic_verifier needs PASS or FAIL_* ; treat either family as enough.
    if agent_type == "semantic_verifier":
        has_status = ("pass" in lowered) or ("fail_" in lowered) or ("fail " in lowered)
        missing = [m for m in missing if m not in ("PASS", "FAIL_")]
        if not has_status:
            missing.append("PASS|FAIL_*")

    if not missing:
        return 0

    reason = (
        f"Agent '{agent_type}' stopped without required artifact markers: "
        + ", ".join(missing)
        + ". Return the structured artifact required by AGENTS.md."
    )
    print(reason, file=sys.stderr)

    # Ask for one continuation only; never create an unbounded repair loop.
    if not stop_hook_active and text.strip():
        print(
            json.dumps(
                {
                    "decision": "block",
                    "reason": reason,
                }
            )
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
