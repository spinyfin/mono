#!/usr/bin/env python3
"""PreToolUse permission guard, shaped like a Boss interception guard.

Denies any tool call whose serialized input contains $PROBE_DENY_MARKER, using
Grok's *native* deny vocabulary (`{"decision": "deny"}`), which is the only
vocabulary Grok honours per
tools/boss/docs/investigations/grok-pretooluse-decision-vocabulary-and-tool-name-map.md.

Logs every decision to $PROBE_GUARD_LOG so we can tell "guard never ran" apart
from "guard ran and allowed".
"""

import json
import os
import sys
import time

MARKER = os.environ["PROBE_DENY_MARKER"]
LOG = os.environ["PROBE_GUARD_LOG"]


def main():
    raw = sys.stdin.read()
    try:
        payload = json.loads(raw) if raw.strip() else {}
    except json.JSONDecodeError:
        payload = {}

    tool_name = payload.get("toolName", payload.get("tool_name", ""))
    tool_input = json.dumps(payload.get("toolInput", payload.get("tool_input", {})))
    # Shell tools only, so the marker can appear verbatim inside a
    # `spawn_subagent` prompt without the guard denying the spawn itself.
    hit = MARKER in tool_input and tool_name in ("run_terminal_command", "run_terminal_cmd", "Bash")
    rec = {
        "wall": time.time(),
        "pid": os.getpid(),
        "hook_name": os.environ.get("GROK_HOOK_NAME"),
        "session_id": payload.get("sessionId"),
        "transcript_path": payload.get("transcriptPath"),
        "tool_name": payload.get("toolName"),
        "tool_use_id": payload.get("toolUseId"),
        "decision": "deny" if hit else "allow",
        "tool_input_excerpt": tool_input[:600],
    }
    with open(LOG, "a") as fh:
        fh.write(json.dumps(rec) + "\n")
        fh.flush()

    if hit:
        print(json.dumps({"decision": "deny", "reason": "PROBE_GUARD_DENY: marker present"}))
    sys.exit(0)


if __name__ == "__main__":
    main()
