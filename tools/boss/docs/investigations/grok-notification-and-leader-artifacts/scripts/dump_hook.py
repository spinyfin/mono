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
