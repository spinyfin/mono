#!/usr/bin/env python3
"""Dump-all Grok hook handler.

Appends one JSON record per hook invocation to $PROBE_LOG, capturing the
full stdin envelope, the GROK_*/CLAUDE_* env excerpt, and this handler's own
process ancestry (pid/ppid chain + argv) so we can tell which OS process
actually invoked the hook -- the parent grok session or a subagent child.

Always exits 0 (pure observer).
"""

import json
import os
import subprocess
import sys
import time

LOG = os.environ["PROBE_LOG"]


def ancestry():
    """Walk pid -> ppid up to init, recording argv for each level."""
    out = []
    pid = os.getpid()
    for _ in range(12):
        try:
            r = subprocess.run(
                ["ps", "-o", "ppid=,command=", "-p", str(pid)],
                capture_output=True,
                text=True,
                timeout=3,
            )
        except Exception as exc:  # pragma: no cover - diagnostic only
            out.append({"pid": pid, "error": str(exc)})
            break
        line = r.stdout.strip()
        if not line:
            break
        ppid_s, _, cmd = line.partition(" ")
        try:
            ppid = int(ppid_s)
        except ValueError:
            break
        out.append({"pid": pid, "ppid": ppid, "command": cmd.strip()[:400]})
        if ppid <= 1:
            break
        pid = ppid
    return out


def main():
    raw = sys.stdin.read()
    try:
        payload = json.loads(raw) if raw.strip() else None
    except json.JSONDecodeError:
        payload = {"__unparsed__": raw[:8000]}

    env = {
        k: v
        for k, v in sorted(os.environ.items())
        if k.startswith(("GROK_", "CLAUDE_", "BOSS_", "XAI_"))
    }

    rec = {
        "wall": time.time(),
        "seq_pid": os.getpid(),
        "event_env": os.environ.get("GROK_HOOK_EVENT"),
        "hook_name": os.environ.get("GROK_HOOK_NAME"),
        "env": env,
        "stdin": payload,
        "ancestry": ancestry(),
    }
    with open(LOG, "a") as fh:
        fh.write(json.dumps(rec) + "\n")
        fh.flush()
    sys.exit(0)


if __name__ == "__main__":
    main()
