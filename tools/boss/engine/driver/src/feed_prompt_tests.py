"""Headless PTY contract tests, invoked by the Bazel Rust test harness."""
import fcntl
import os
from pathlib import Path
import pty
import runpy
import select
import signal
import struct
import subprocess
import sys
import termios
import time

script, root = sys.argv[1], Path(sys.argv[2])
assert runpy.run_path(script)["READY_SECS"] == 120
prompt = root / "prompt.txt"
prompt.write_bytes((b"paragraph\r\n\t/slash-line\n" * 50000))
expected = prompt.read_bytes().replace(b"\r\n", b"\n")
assert len(expected) > 1048576
fake = root / "fake.py"
fake.write_text(r'''
import fcntl, os, signal, struct, sys, termios, time, tty
from pathlib import Path
root = Path(sys.argv[1])
assert len(sys.argv) == 3 and sum(len(arg) for arg in sys.argv) < 4096
time.sleep(float(sys.argv[2]))
tty.setraw(0)
# Split the capability sequence across writes to exercise streaming detection.
os.write(1, b'\x1b[?200')
time.sleep(0.05)
os.write(1, b'4h')
buf = b''
while not buf.endswith(b'\x1b[201~'):
    buf += os.read(0, 65536)
assert buf.startswith(b'\x1b[200~')
paste_finished = time.monotonic()
assert os.read(0, 1) == b'\r'
assert time.monotonic() - paste_finished >= 0.35
body = buf[6:-6]
assert b'\x1b[201~' not in body
(root / 'received').write_bytes(body)
def resized(*args):
    (root / 'size').write_bytes(fcntl.ioctl(0, termios.TIOCGWINSZ, b'\0' * 8))
signal.signal(signal.SIGWINCH, resized)
signal.signal(signal.SIGHUP, signal.SIG_IGN)
def terminated(*args):
    time.sleep(0.3)
    (root / 'terminated').touch()
    sys.exit(0)
signal.signal(signal.SIGTERM, terminated)
(root / 'ready').touch()
data = os.read(0, 4)
(root / 'controls').write_bytes(data)
while True:
    time.sleep(0.05)
''')


def wait_file(name):
    deadline = time.monotonic() + 10
    while not (root / name).exists():
        assert time.monotonic() < deadline, name
        time.sleep(0.01)
    return (root / name).read_bytes()


master, slave = pty.openpty()
saved = termios.tcgetattr(slave)
env = dict(os.environ)
env.pop("BOSS_FEED_PROMPT_READY_SECS", None)
p = subprocess.Popen(
    [sys.executable, script, str(prompt), sys.executable, str(fake), str(root), "0.3"],
    stdin=slave, stdout=slave, stderr=slave, start_new_session=True, env=env,
)
try:
    wait_file("ready")
    assert (root / "received").read_bytes() == expected
    assert not termios.tcgetattr(slave)[3] & termios.ICANON
    os.write(master, b"\x03\x1b[A")
    assert wait_file("controls") == b"\x03\x1b[A"
    size = struct.pack("HHHH", 43, 117, 0, 0)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, size)
    os.kill(p.pid, signal.SIGWINCH)
    assert wait_file("size") == size
    # Exactly the engine's teardown target: only the feeder's group.
    os.killpg(p.pid, signal.SIGTERM)
    status = p.wait(timeout=5)
    output = os.read(master, 65536) if select.select([master], [], [], 0)[0] else b""
    assert status == 143, (status, output)
    wait_file("terminated")
    restored = termios.tcgetattr(slave)
    # Darwin sets PENDIN itself on a noncanonical -> canonical transition
    # to request input reprocessing; it is not a saved terminal setting.
    restored[3] &= ~getattr(termios, "PENDIN", 0)
    saved[3] &= ~getattr(termios, "PENDIN", 0)
    assert restored == saved, (saved, restored)
finally:
    if p.poll() is None:
        os.killpg(p.pid, signal.SIGTERM)
        p.wait(timeout=5)
    os.close(master)
    os.close(slave)

# Explicit deadlines still fail with elapsed time, without hanging on a
# slow child; the default above allowed the same delayed startup to finish.
env["BOSS_FEED_PROMPT_READY_SECS"] = "0.05"
out = subprocess.run(
    [sys.executable, script, str(prompt), sys.executable, str(fake), str(root), "0.3"],
    capture_output=True, timeout=5, env=env,
)
assert out.returncode == 125, out
assert b"after " in out.stderr and b"readiness timeout" in out.stderr, out.stderr

# Both an already-exited leader and an exit during graceful teardown must
# preserve the readiness diagnostic and exit code, including on Darwin.
for code in (
    "pass",
    "import signal,time; signal.signal(signal.SIGTERM, lambda *_: (time.sleep(0.3), exit(0))); time.sleep(10)",
):
    out = subprocess.run(
        [sys.executable, script, str(prompt), sys.executable, "-c", code],
        capture_output=True, timeout=5, env={**env, "BOSS_FEED_PROMPT_READY_SECS": "0.2"},
    )
    assert out.returncode == 125, out
    assert b"prompt was not injected" in out.stderr, out.stderr
    assert b"Traceback" not in out.stderr, out.stderr

# Input must reach a canonical-mode confirmation prompt during readiness.
out = subprocess.run(
    [sys.executable, script, str(prompt), sys.executable, "-c",
     "import sys; assert input() == 'confirm'; print('confirmation received', flush=True)"],
    input=b"confirm\n", capture_output=True, timeout=5,
    env={**env, "BOSS_FEED_PROMPT_READY_SECS": "1"},
)
assert out.returncode == 125, out
assert b"confirmation received" in out.stdout, out
assert b"Traceback" not in out.stderr, out.stderr

# Deterministically reproduce both killpg races even on non-Darwin hosts.
for fail_call in (1, 2):
    wrapper = root / "killpg-race.py"
    wrapper.write_text('''
import errno, os, runpy, sys
real_killpg = os.killpg
fail_call = int(sys.argv[1])
calls = 0
def killpg(pid, sig):
    global calls
    calls += 1
    try:
        real_killpg(pid, sig)
    except ProcessLookupError:
        pass
    if calls == fail_call:
        raise PermissionError(errno.EPERM, 'zombie-only process group')
os.killpg = killpg
sys.argv = sys.argv[2:]
runpy.run_path(sys.argv[0], run_name='__main__')
''')
    out = subprocess.run(
        [sys.executable, str(wrapper), str(fail_call), script, str(prompt),
         sys.executable, "-c", "import time; time.sleep(10)"],
        capture_output=True, timeout=5, env=env,
    )
    assert out.returncode == 125, out
    assert b"prompt was not injected" in out.stderr, out.stderr
    assert b"Traceback" not in out.stderr, out.stderr

# Raw input alone is insufficient: don't inject into a CLI that has not
# advertised bracketed paste support.
out = subprocess.run(
    [sys.executable, script, str(prompt), sys.executable, "-c",
     "import os,tty; tty.setraw(0); os.read(0, 1); print('unexpected input')"],
    capture_output=True, timeout=5, env=env,
)
assert out.returncode == 125, out
assert b"unexpected input" not in out.stdout, out
