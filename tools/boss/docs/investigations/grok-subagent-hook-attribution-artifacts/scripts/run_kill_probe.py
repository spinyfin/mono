#!/usr/bin/env python3
"""Kill-mid-subagent probe.

Spawns the real TUI worker shape, waits for `subagent_start` to appear in the
hook log, then SIGKILLs the `grok` process while the subagent is still doing
work. Answers two questions the happy path cannot:

  1. Can a Grok subagent outlive its parent? (Its side effect -- the file the
     child was told to write -- is the observable.)
  2. Does anything reach the hook stream on a hard kill, or does Boss get
     pure silence (the same shape a killed Claude worker produces)?
"""

import fcntl
import json
import os
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time
import uuid

PROBE = os.path.expanduser("~/.cache/grok-subagent-hook-probe")


def main():
    label = sys.argv[1]
    prompt_file = sys.argv[2]
    kill_delay = float(sys.argv[3]) if len(sys.argv) > 3 else 10.0

    run = os.path.join(PROBE, "evidence", label)
    cwd = os.path.join(run, "cwd")
    subprocess.run(["rm", "-rf", run], check=False)
    os.makedirs(cwd, exist_ok=True)
    subprocess.run(
        [sys.executable, os.path.join(PROBE, "scripts", "setup_home.py"), label, cwd],
        check=True,
        env=dict(os.environ, PROBE_DENY_MARKER="PROBE_FORBIDDEN"),
        stdout=subprocess.DEVNULL,
    )

    hook_log = os.path.join(run, "hooks.jsonl")
    argv = [
        "grok",
        "--model", "grok-4.5",
        "--reasoning-effort", "high",
        "--no-alt-screen",
        "--always-approve",
        "--trust",
        "--session-id", str(uuid.uuid4()),
        "--cwd", cwd,
        "--no-memory",
        open(prompt_file).read(),
    ]
    env = {
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:" + os.path.expanduser("~/.local/bin"),
        "HOME": os.path.join(PROBE, "claude_home"),
        "GROK_HOME": os.path.join(PROBE, "home"),
        "TERM": "xterm-256color",
    }

    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execvpe("grok", argv, env)
        os._exit(127)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 200, 0, 0))

    out = open(os.path.join(run, "pane.raw"), "wb")
    started_at = None
    killed = False
    deadline = time.time() + 300

    def saw(event):
        if not os.path.exists(hook_log):
            return False
        try:
            return any(json.loads(l).get("event_env") == event for l in open(hook_log))
        except (OSError, json.JSONDecodeError):
            return False

    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 1.0)
        if r:
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            out.write(data)
            out.flush()
        if started_at is None and saw("subagent_start"):
            started_at = time.time()
            print("subagent_start observed")
        if started_at and not killed and time.time() - started_at >= kill_delay:
            print("SIGKILL to grok pid", pid)
            with open(os.path.join(run, "kill_wall.txt"), "w") as fh:
                fh.write(str(time.time()) + "\n")
            os.kill(pid, signal.SIGKILL)
            killed = True
            deadline = time.time() + 90  # watch for late hooks / late side effects
    out.close()
    try:
        os.waitpid(pid, os.WNOHANG)
    except ChildProcessError:
        pass
    subprocess.run(["ls", "-la", cwd], stdout=open(os.path.join(run, "cwd_listing.txt"), "w"))
    subprocess.run(["pgrep", "-fl", "grok --model grok-4.5 --reasoning-effort high"],
                   stdout=open(os.path.join(run, "survivors.txt"), "w"))
    print("done, killed =", killed)


if __name__ == "__main__":
    main()
