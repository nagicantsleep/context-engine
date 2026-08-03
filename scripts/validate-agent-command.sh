#!/usr/bin/env bash
# scripts/validate-agent-command.sh
# PreToolUse guard for executor Bash calls.

input="$(cat)"
command="$(jq -r '.tool_input.command // ""' <<< "$input")"

case "$command" in
  *"git push"*|*"git reset --hard"*|*"rm -rf"*|*"npm publish"*|*"terraform apply"*)
    echo "Blocked: executor cannot perform destructive or external actions" >&2
    exit 2
    ;;
esac

exit 0
