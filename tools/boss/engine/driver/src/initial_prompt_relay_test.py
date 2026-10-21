"""Hermetic nested-terminal test, invoked only by the driver's Bazel test."""

import faulthandler
import fcntl
import importlib.util
import json
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


trace = open(os.environ["BOSS_RELAY_TRACE"], "w")
faulthandler.dump_traceback_later(10, repeat=True, file=trace)
root = Path(sys.argv[1])
# Raw startup/composer captures from isolated, unprompted installed CLIs.
# Replay at byte boundaries to cover split CSI and UTF-8 sequences.
spec = importlib.util.spec_from_file_location("initial_prompt", root / "initial-prompt.py")
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)
configs = json.loads((root / "configs.json").read_text())
for fixture in json.loads((root / "panes.json").read_text()):
    config = configs[fixture["driver"]]
    capture = fixture["capture"].encode()
    assert b"\x1b[?2004h" in capture, fixture["driver"]
    for chunk_size in (1, 7, 4096):
        view = launcher.TerminalTail(fixture["height"], fixture["width"])
        for offset in range(0, len(capture), chunk_size):
            view.feed(capture[offset:offset + chunk_size])
        ready = launcher.ready_for_prompt(view, True, True,
            [s.encode() for s in config["agent_markers"]],
            [s.encode() for s in config["starting_markers"]])
        assert ready == fixture["ready"], (fixture["driver"], view.tail())

view = launcher.TerminalTail()
view.feed(b"Claude Code\r\nQuick safety check:")
assert not launcher.ready_for_prompt(view, True, True, [b"Claude Code"], [b"Quick safety check:"])
view.feed(b"\r\x1b[2Kready")
assert launcher.ready_for_prompt(view, True, True, [b"Claude Code"], [b"Quick safety check:"])
view.feed(b"\r\nold output" * 20)
assert not launcher.ready_for_prompt(view, True, True, [b"Claude Code"], [])

stub = root / "stub.py"
stub.write_text('''
import fcntl, os, select, signal, struct, termios, tty
tty.setraw(0)
os.write(1, b'\\x1b[6n')
response = b''
while not response.endswith(b'R'):
    response += os.read(0, 4096)
assert response == b'\\x1b[1;1R', response
os.write(1, b'\\x1b[?2004hClaude Code Quick safety check:')
assert not select.select([0], [], [], 0.3)[0], 'brief pasted into startup dialog'
os.write(1, b'\\x1b[2JClaude Code')
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


def assert_processes_exited(reader):
    # The supervisor, driver and helper inherit the write end and never
    # close it. EOF proves all have exited, even before init reaps zombies.
    try:
        readable, _, _ = select.select([reader], [], [], 10)
        assert readable and os.read(reader, 1) == b"", "driver/helper survived teardown"
    finally:
        os.close(reader)


# A stub that ignores both forwarded TERM and terminal HUP must still die
# when the entire relay group is killed without a chance to run finally.
stub.write_text('''
import os, signal, time
from pathlib import Path
signal.signal(signal.SIGTERM, signal.SIG_IGN)
signal.signal(signal.SIGHUP, signal.SIG_IGN)
name = "driver.pid" if os.fork() else "helper.pid"
Path(__file__).with_name(name).write_text(str(os.getpid()))
while True:
    time.sleep(1)
''')
lifetime_reader, lifetime_writer = os.pipe()
proc = subprocess.Popen(
    [sys.executable, str(root / "initial-prompt.py"), sys.executable, str(stub)],
    stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    start_new_session=True, pass_fds=(lifetime_writer,),
)
os.close(lifetime_writer)
try:
    deadline = time.monotonic() + 10
    while not all((root / name).exists() for name in ("driver.pid", "helper.pid")):
        assert time.monotonic() < deadline, "stub never started"
        time.sleep(0.02)
    os.killpg(proc.pid, signal.SIGKILL)
    proc.communicate(timeout=10)
    assert_processes_exited(lifetime_reader)
finally:
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait(timeout=10)


# On a readiness timeout, run() must kill the entire child group too.
for name in ("driver.pid", "helper.pid"):
    (root / name).unlink()
code = '''
import importlib.util, pathlib, sys
root = pathlib.Path(sys.argv[1])
spec = importlib.util.spec_from_file_location("launcher", root / "initial-prompt.py")
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)
launcher.run([sys.executable, str(root / "stub.py")], b"brief", [b"never ready"], [], readiness_timeout=0.5)
'''
lifetime_reader, lifetime_writer = os.pipe()
try:
    result = subprocess.run([sys.executable, "-c", code, str(root)], capture_output=True,
                            timeout=10, pass_fds=(lifetime_writer,))
finally:
    os.close(lifetime_writer)
assert result.returncode != 0 and b"TimeoutError" in result.stderr, result.stderr
assert all((root / name).exists() for name in ("driver.pid", "helper.pid"))
assert_processes_exited(lifetime_reader)
