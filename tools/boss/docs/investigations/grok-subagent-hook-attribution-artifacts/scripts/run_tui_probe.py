#!/usr/bin/env python3
"""Run the *interactive TUI* Grok worker shape under a pty.

This is the shape Boss actually spawns into a GhosttyKit pane
(`build_grok_pane_command`, tools/boss/engine/driver/src/grok.rs): full-screen
`grok` with `--no-alt-screen`, a positional prompt, `--always-approve
--trust --session-id --cwd --no-memory`. `--no-subagents` is deliberately
OMITTED here -- that is the flag under test.

Usage: run_tui_probe.py <label> <prompt-file> [--keep-subagents-flag]

Terminates when the parent session's `stop` hook has fired (observed via the
probe hook log), or on timeout; then sends Ctrl-C twice and closes the pty.
"""

import json
import os
import pty
import select
import shlex
import signal
import subprocess
import sys
import time
import uuid

PROBE = os.path.expanduser("~/.cache/grok-subagent-hook-probe")


def main():
    label = sys.argv[1]
    prompt_file = sys.argv[2]
    extra = sys.argv[3:]

    run = os.path.join(PROBE, "evidence", label)
    cwd = os.path.join(run, "cwd")
    subprocess.run(["rm", "-rf", run], check=False)
    os.makedirs(cwd, exist_ok=True)

    env_setup = dict(os.environ, PROBE_DENY_MARKER=os.environ.get("PROBE_DENY_MARKER", "PROBE_FORBIDDEN"))
    subprocess.run(
        [sys.executable, os.path.join(PROBE, "scripts", "setup_home.py"), label, cwd],
        check=True,
        env=env_setup,
        stdout=subprocess.DEVNULL,
    )

    hook_log = os.path.join(run, "hooks.jsonl")
    session_id = str(uuid.uuid4())
    with open(os.path.join(run, "session_id.txt"), "w") as fh:
        fh.write(session_id + "\n")

    prompt = open(prompt_file).read()

    argv = [
        "grok",
        "--model", "grok-4.5",
        "--reasoning-effort", "high",
        "--no-alt-screen",
        "--always-approve",
        "--trust",
        "--session-id", session_id,
        "--cwd", cwd,
        "--no-memory",
    ] + extra + [prompt]
    with open(os.path.join(run, "argv.txt"), "w") as fh:
        fh.write(" ".join(shlex.quote(a) for a in argv) + "\n")

    env = {
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:" + os.path.expanduser("~/.local/bin"),
        "HOME": os.path.join(PROBE, "claude_home"),
        "GROK_HOME": os.path.join(PROBE, "home"),
        "TERM": "xterm-256color",
        "COLUMNS": "200",
        "LINES": "50",
    }

    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execvpe("grok", argv, env)
        os._exit(127)

    # 200x50 window so the TUI does not wrap unreadably.
    import fcntl
    import struct
    import termios

    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 200, 0, 0))

    out = open(os.path.join(run, "pane.raw"), "wb")
    # Direct measurement of the premise `background_children.rs` relies on:
    # sample the grok pid's live descendant process tree once a second for the
    # whole run, so "a subagent is a descendant process" is measured, not
    # assumed. Mirrors count_live_descendants' walk (children of children).
    desc_log = open(os.path.join(run, "descendants.tsv"), "w")
    desc_log.write("wall\tdescendant_count\tcommands\n")

    def descendants(root):
        found, frontier, depth = [], [root], 0
        while frontier and depth < 8:
            nxt = []
            for p in frontier:
                r = subprocess.run(["pgrep", "-P", str(p)], capture_output=True, text=True)
                for line in r.stdout.split():
                    nxt.append(int(line))
            found.extend(nxt)
            frontier = nxt
            depth += 1
        return found

    last_sample = 0.0
    deadline = time.time() + float(os.environ.get("PROBE_TUI_TIMEOUT", "600"))
    parent_stopped_at = None
    sent_quit = False

    def parent_stop_seen():
        if not os.path.exists(hook_log):
            return False
        try:
            for line in open(hook_log):
                r = json.loads(line)
                if r.get("event_env") == "stop":
                    return True
        except (OSError, json.JSONDecodeError):
            pass
        return False

    while True:
        if time.time() > deadline:
            break
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
        if time.time() - last_sample >= 1.0:
            last_sample = time.time()
            kids = descendants(pid)
            cmds = []
            for k in kids:
                r = subprocess.run(["ps", "-o", "command=", "-p", str(k)], capture_output=True, text=True)
                cmds.append(r.stdout.strip()[:80])
            desc_log.write("{:.1f}\t{}\t{}\n".format(last_sample, len(kids), " || ".join(cmds)))
            desc_log.flush()
        if parent_stopped_at is None and parent_stop_seen():
            parent_stopped_at = time.time()
        # Give the TUI a few seconds after the parent's stop for any late
        # hook (a background subagent that outlives the turn), then quit.
        if parent_stopped_at and time.time() - parent_stopped_at > float(
            os.environ.get("PROBE_POST_STOP_GRACE", "20")
        ):
            if not sent_quit:
                os.write(fd, b"/quit\r")
                sent_quit = True
                deadline = min(deadline, time.time() + 20)

    if not sent_quit:
        try:
            os.write(fd, b"\x03")
            time.sleep(0.5)
            os.write(fd, b"\x03")
        except OSError:
            pass
    time.sleep(2)
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        _, status = os.waitpid(pid, 0)
    except ChildProcessError:
        status = -1
    out.close()
    with open(os.path.join(run, "agent_exit.txt"), "w") as fh:
        fh.write(str(status) + "\n")
    subprocess.run(["ls", "-la", cwd], stdout=open(os.path.join(run, "cwd_listing.txt"), "w"))
    print("exit status", status)


if __name__ == "__main__":
    main()
