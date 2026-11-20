#!/usr/bin/env python3
"""Fake agy: the worker flags listed by agy 1.5.10 --help, no vendor calls."""
import json
import os
from pathlib import Path
import sys
import tty

home = Path(os.environ["HOME"])
container = home.parent
control = container / "control"
assert sys.argv[1:] == [
    "--log-file", str(control / "agy.log"),
    "--add-dir", str(container / "rules-root"),
    "--model", "gemini-4-hydrogen-medium",
    "--effort", "medium",
    "--settings", str(control / "session-settings.json"),
], sys.argv
assert os.environ["AGY_CLI_DISABLE_AUTO_UPDATE"] == "true"
assert os.environ["AGY_CLI_HIDE_ACCOUNT_INFO"] == "true"
settings = json.loads((control / "session-settings.json").read_text())
assert settings["queuedMessages"] == "queue"
assert settings["toolPermission"] == "request-review"
assert (container / "rules-root/AGENTS.md").is_file()
check = json.loads((control / "settings-log-check.json").read_text())
log = "CLI settings initialized: " + check["expected"] + "\n"
log += "\n".join(check["require"]) + "\n"
(control / "agy.log").write_text(log)
if os.environ.get("AGY_FAKE_REJECT"):
    print("agy: rejected invocation sentinel", file=sys.stderr, flush=True)
    raise SystemExit(2)
tty.setraw(0)
os.write(1, b"\x1b[?2004h? for shortcuts")
data = b""
while not data.endswith(b"\x1b[201~"):
    data += os.read(0, 4096)
assert data == b"\x1b[200~the brief\x1b[201~", data
os.write(1, b"\r\nthe brief")
assert os.read(0, 1) == b"\r"
