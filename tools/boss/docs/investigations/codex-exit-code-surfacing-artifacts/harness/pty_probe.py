#!/usr/bin/env python3
"""Run a probe with codex's stdout attached to a real PTY.

Worker panes host codex with stdout on a tty, not a pipe. This isolates that
one variable: does tty-vs-pipe change the rollout JSONL (the actual engine
ingress) or the exec-tool result the model sees?
"""
import os
import pathlib
import pty
import select
import subprocess
import sys

out_path, cmd = sys.argv[1], sys.argv[2:]
master, slave = pty.openpty()
proc = subprocess.Popen(
    cmd, stdin=subprocess.DEVNULL, stdout=slave, stderr=slave, close_fds=True
)
os.close(slave)

buf = bytearray()
while True:
    ready, _, _ = select.select([master], [], [], 1.0)
    if ready:
        try:
            chunk = os.read(master, 65536)
        except OSError:
            break
        if not chunk:
            break
        buf += chunk
    elif proc.poll() is not None:
        break
proc.wait()
os.close(master)

pathlib.Path(out_path).write_bytes(bytes(buf))
print(f"pty rc={proc.returncode} captured={len(buf)} bytes")
