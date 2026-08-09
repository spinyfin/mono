#!/usr/bin/env bash
# Run one headless Grok probe under an isolated, Boss-shaped GROK_HOME.
#
#   run_probe.sh <label> <prompt-file> [extra grok args...]
#
# Apparatus rules (inherited from grok-permission-isolation-2026-07-27.md):
#   - never point at the operator's live ~/.grok (auth.json is byte-copied)
#   - scratch root is NOT under /tmp (every sandbox profile makes /tmp writable)
#   - model is grok-4.5 (grok-code-fast-1 is retired and silently redirects)
set -u

PROBE="$HOME/.cache/grok-subagent-hook-probe"
LABEL="$1"; shift
PROMPT_FILE="$1"; shift

RUN="$PROBE/evidence/$LABEL"
CWD="$RUN/cwd"
rm -rf "$RUN"
mkdir -p "$CWD"

export PROBE_DENY_MARKER="${PROBE_DENY_MARKER:-PROBE_FORBIDDEN}"
python3 "$PROBE/scripts/setup_home.py" "$LABEL" "$CWD" >/dev/null

SESSION_ID="$(python3 -c 'import uuid;print(uuid.uuid4())')"
echo "$SESSION_ID" > "$RUN/session_id.txt"
grok --version > "$RUN/grok_version.txt" 2>&1
command -v grok > "$RUN/grok_bin.txt" 2>&1

env -i \
  PATH="/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:$HOME/.local/bin" \
  HOME="$PROBE/claude_home" \
  GROK_HOME="$PROBE/home" \
  TERM=dumb \
  grok -p "$(cat "$PROMPT_FILE")" \
    --model grok-4.5 \
    --always-approve \
    --trust \
    --session-id "$SESSION_ID" \
    --cwd "$CWD" \
    --output-format json \
    "$@" \
    > "$RUN/agent_stdout.json" 2> "$RUN/agent_stderr.txt"
echo "$?" > "$RUN/agent_exit.txt"

ls -la "$CWD" > "$RUN/cwd_listing.txt" 2>&1
echo "=== exit $(cat "$RUN/agent_exit.txt") ==="
