#!/bin/bash
# mkhome.sh <home-dir> <workspace-dir> <probe-log>
set -euo pipefail
H="$1"; WS="$2"; LOG="$3"
rm -rf "$H"; mkdir -p "$H/hooks" "$WS"
# Byte-copy the credential; never symlink (let alone use) the live ~/.grok.
# Point GROK_AUTH_SRC at a stashed copy so a host-side token refresh mid-run
# cannot pull auth out from under the probes -- that happened during this
# investigation and left ~/.grok/auth.json transiently absent.
cp "${GROK_AUTH_SRC:-$HOME/.grok/auth.json}" "$H/auth.json"
chmod 600 "$H/auth.json"
: > "$H/hooks-paths"

python3 - "$H" "$WS" "$LOG" << 'PYEOF'
import json, os, sys
h, ws, log = sys.argv[1], sys.argv[2], sys.argv[3]

# --- trusted_folders.toml: dedup path forms, TOML rejects duplicate headers ---
paths, seen = [], set()
for p in (ws, os.path.realpath(ws), "/tmp", "/private/tmp"):
    if p not in seen:
        seen.add(p); paths.append(p)
open(f"{h}/trusted_folders.toml", "w").write(
    "".join(f'[folders."{p}"]\ntrusted = true\ndecided_at = 1785000000\n\n' for p in paths))

# --- dump-all hook wiring; log path baked in, no env inheritance assumed ---
hook = {"matcher": "*", "hooks": [{"type": "command",
        "command": f"python3 {h}/hooks/dump.py {log}", "timeout": 30}]}
events = ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse",
          "PostToolUseFailure", "PermissionDenied", "Stop", "StopFailure",
          "Notification", "SubagentStart", "SubagentStop", "PreCompact",
          "PostCompact", "SessionEnd"]
json.dump({"hooks": {e: [hook] for e in events}},
          open(f"{h}/hooks/probe.json", "w"), indent=2)
PYEOF

cat > "$H/hooks/dump.py" << 'PYEOF'
import json, os, sys
log = sys.argv[1]
raw = sys.stdin.read()
try:
    payload = json.loads(raw) if raw.strip() else None
except Exception:
    payload = {"__unparseable__": raw[:2000]}
rec = {
    "GROK_HOOK_EVENT": os.environ.get("GROK_HOOK_EVENT"),
    "GROK_SESSION_ID": os.environ.get("GROK_SESSION_ID"),
    # $GROK_EVENT / $GROK_MESSAGE are the [ui.notifications].hooks channel vars;
    # capture them to prove whether the two channels are the same thing.
    "GROK_EVENT": os.environ.get("GROK_EVENT"),
    "GROK_MESSAGE": os.environ.get("GROK_MESSAGE"),
    "payload": payload,
}
with open(log, "a") as f:
    f.write(json.dumps(rec) + "\n")
PYEOF
echo "built $H"
