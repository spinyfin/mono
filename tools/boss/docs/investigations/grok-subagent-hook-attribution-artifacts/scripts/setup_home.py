#!/usr/bin/env python3
"""Provision the probe's isolated GROK_HOME, mirroring Boss's own provisioning.

Config block is a byte-copy of `render_base_config_toml()` from
tools/boss/engine/driver/src/grok/home.rs; the hooks file is shaped like
`grok/hooks.rs::write_hooks` (forwarder on Boss's event set, guards appended
onto the same PreToolUse array), with the boss-event forwarder replaced by a
dump-all observer and the five Boss guards by one probe deny guard.
"""

import json
import os
import shlex
import sys
import time

PROBE = os.path.expanduser("~/.cache/grok-subagent-hook-probe")
HOME = os.path.join(PROBE, "home")
SCRIPTS = os.path.join(PROBE, "scripts")

# Boss's wired event set: driver/src/grok/hooks.rs GROK_HOOK_EVENTS.
BOSS_EVENTS = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "Notification",
    "SessionEnd",
]
# Events Boss does NOT wire today; probed to see whether they exist at all.
EXTRA_EVENTS = ["SubagentStart", "SubagentStop", "PostToolUseFailure", "PermissionDenied"]

CONFIG_TOML = """# Boss-owned Grok config. Written every provision (idempotent overwrite).
# Compat surfaces off so reused cube workspaces that still contain
# `.claude/CLAUDE.md` / `.claude/settings.json` do not load under Grok.
# Official externalCompat cells: hooks/agents/skills/mcps/rules/sessions
# (no plugins surface -- writing plugins=false is a silent no-op).

[compat.claude]
hooks = false
agents = false
skills = false
mcps = false
rules = false
sessions = false

[compat.cursor]
hooks = false
agents = false
skills = false
mcps = false
rules = false
sessions = false

[ui]
vim_mode = false
"""


def main():
    label = sys.argv[1]
    cwd = sys.argv[2]
    log = os.path.join(PROBE, "evidence", label, "hooks.jsonl")
    guard_log = os.path.join(PROBE, "evidence", label, "guard.jsonl")
    os.makedirs(os.path.dirname(log), exist_ok=True)

    dump = "PROBE_LOG={} python3 {}".format(
        shlex.quote(log), shlex.quote(os.path.join(SCRIPTS, "dump_hook.py"))
    )
    guard = "PROBE_DENY_MARKER={} PROBE_GUARD_LOG={} python3 {}".format(
        shlex.quote(os.environ.get("PROBE_DENY_MARKER", "PROBE_FORBIDDEN")),
        shlex.quote(guard_log),
        shlex.quote(os.path.join(SCRIPTS, "deny_guard.py")),
    )

    hooks = {}
    for ev in BOSS_EVENTS + EXTRA_EVENTS:
        hooks[ev] = [{"matcher": "*", "hooks": [{"type": "command", "command": dump}]}]
    # Guard appended onto the same PreToolUse array, after the observer --
    # matches hooks.rs ordering so the forwarder sees every call first.
    hooks["PreToolUse"][0]["hooks"].append({"type": "command", "command": guard})

    os.makedirs(os.path.join(HOME, "hooks"), exist_ok=True)
    with open(os.path.join(HOME, "hooks", "boss-hooks.json"), "w") as fh:
        json.dump({"hooks": hooks}, fh, indent=2)
    with open(os.path.join(HOME, "config.toml"), "w") as fh:
        fh.write(CONFIG_TOML)

    # trusted_folders.toml -- belt for the hidden --trust flag (design D-3).
    now = int(time.time())
    variants = {cwd, os.path.realpath(cwd)}
    with open(os.path.join(HOME, "trusted_folders.toml"), "w") as fh:
        for p in sorted(variants):
            fh.write('[folders."{}"]\ntrusted = true\ndecided_at = {}\n\n'.format(p, now))

    print(log)


if __name__ == "__main__":
    main()
