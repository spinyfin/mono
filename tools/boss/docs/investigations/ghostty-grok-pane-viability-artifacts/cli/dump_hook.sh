#!/bin/bash
EVENT="$1"
DIR="${SPIKE_PAYLOADS:-/tmp/grok-pane-spike/artifacts/hook_payloads}"
mkdir -p "$DIR"
TS=$(date +%s%N)
# Also dump env vars of interest
{
  echo "===ENV==="
  env | grep -E '^(GROK_|CLAUDE_|HOOK)' | sort
  echo "===STDIN==="
  cat
} > "$DIR/${EVENT}_${TS}.json"
# allow by default for PreToolUse
if [ "$EVENT" = "PreToolUse" ]; then
  echo '{"decision":"allow"}'
fi
exit 0
