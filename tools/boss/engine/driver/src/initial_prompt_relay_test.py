"""Hermetic nested-terminal test, invoked only by the driver's Bazel test."""

import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time


root = Path(sys.argv[1])
stub = root / "stub.py"
stub.write_text('''
import fcntl, os, signal, struct, termios, tty
tty.setraw(0)
os.write(1, b'\\x1b[6n')
response = b''
while not response.endswith(b'R'):
    response += os.read(0, 4096)
assert response == b'\\x1b[1;1R', response
os.write(1, b'\\x1b[?2004hClaude Code')
data = b''
while not data.endswith(b'\\x1b[201~\\r'):
    data += os.read(0, 4096)
assert data == b'\\x1b[200~private sentinel\\x1b[201~\\r', data
def resized(*_):
    size = fcntl.ioctl(0, termios.TIOCGWINSZ, bytes(8))
    if struct.unpack('HHHH', size)[:2] == (37, 113):
        os.write(1, b'RESIZED')
signal.signal(signal.SIGWINCH, resized)
os.write(1, b'FIRST_DELIVERED')
followup = b''
while not followup.endswith(b'\\r'):
    followup += os.read(0, 4096)
assert followup == b'follow-up probe\\r', followup
os.write(1, b'FOLLOWUP_DELIVERED')
raise SystemExit(23)
''')
master, slave = pty.openpty()
proc = subprocess.Popen(
    [sys.executable, str(root / "initial-prompt.py"), sys.executable, str(stub)],
    stdin=slave, stdout=slave, stderr=slave, start_new_session=True,
    preexec_fn=lambda: signal.signal(signal.SIGHUP, signal.SIG_IGN),
)
os.close(slave)
captured = b""


def wait_for(marker):
    global captured
    deadline = time.monotonic() + 10
    while marker not in captured:
        if time.monotonic() >= deadline:
            raise AssertionError((marker, captured))
        readable, _, _ = select.select([master], [], [], 0.1)
        if readable:
            data = os.read(master, 65536)
            if not data:
                raise AssertionError(("early EOF", captured))
            captured += data


try:
    wait_for(b"\x1b[6n")
    os.write(master, b"\x1b[1;1R")
    wait_for(b"FIRST_DELIVERED")
    os.kill(proc.pid, signal.SIGHUP)
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 37, 113, 0, 0))
    os.kill(proc.pid, signal.SIGWINCH)
    wait_for(b"RESIZED")
    os.write(master, b"follow-up probe\r")
    wait_for(b"FOLLOWUP_DELIVERED")
    assert proc.wait(timeout=10) == 23
finally:
    if proc.poll() is None:
        proc.kill()
        proc.wait(timeout=10)
    os.close(master)
