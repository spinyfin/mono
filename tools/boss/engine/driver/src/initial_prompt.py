"""Deliver a private brief to an interactive CLI without exposing it in argv.

Own the child terminal for the lifetime of the worker, independently of the
engine. Only the driver and its flags are arguments. Subsequent input, resize,
signals, output and exit status pass through to the same interactive session.
"""

import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import sys
import termios
import time
import tty


ANSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")
# Detached SSH workers write a log, not an outer terminal. Answer the startup
# queries ourselves in that topology; attached tmux workers use the real
# terminal's replies. No keyboard enhancements are advertised by the log sink.
QUERIES = re.compile(rb"\x1b\[(6n|\?6n|0?c|>0?c|\?u|\?2026\$p)")
REPLIES = {
    b"6n": b"\x1b[1;1R", b"?6n": b"\x1b[?1;1R",
    b"c": b"\x1b[?1;2c", b"0c": b"\x1b[?1;2c",
    b">c": b"\x1b[>0;0;0c", b">0c": b"\x1b[>0;0;0c",
    b"?u": b"\x1b[?0u", b"?2026$p": b"\x1b[?2026;2$y",
}


def write_all(fd, data):
    while data:
        count = os.write(fd, data)
        data = data[count:]


def run(argv, prompt, markers):
    if b"\x1b[201~" in prompt or b"\0" in prompt:
        raise ValueError("initial brief contains a terminal paste terminator or NUL")
    outer_tty = os.isatty(0)
    saved = termios.tcgetattr(0) if outer_tty else None
    master, slave = pty.openpty()

    def resize(*_):
        size = (fcntl.ioctl(0, termios.TIOCGWINSZ, bytes(8)) if outer_tty
                else struct.pack("HHHH", 50, 220, 0, 0))
        fcntl.ioctl(master, termios.TIOCSWINSZ, size)

    resize()
    pid = os.fork()
    if pid == 0:
        os.close(master)
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        for fd in (0, 1, 2):
            os.dup2(slave, fd)
        if slave > 2:
            os.close(slave)
        os.environ.setdefault("TERM", "xterm-256color")
        os.execvp(argv[0], argv)
    os.close(slave)

    def forward(signum, _frame):
        try:
            os.killpg(pid, signum)
        except ProcessLookupError:
            pass

    for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        if signum == signal.SIGHUP and signal.getsignal(signum) == signal.SIG_IGN:
            # SSH's nohup deliberately detaches this process from hangups.
            continue
        signal.signal(signum, forward)
    signal.signal(signal.SIGWINCH, resize)
    pending = b""
    screen = b""
    query_buffer = b""
    delivered = False
    paste_started = False
    submitting = False
    paste_enabled = False
    submit_at = None
    deadline = time.monotonic() + 30
    stdin_open = outer_tty
    try:
        if outer_tty:
            tty.setraw(0, termios.TCSANOW)
        while True:
            now = time.monotonic()
            if not delivered and now >= deadline:
                raise TimeoutError("driver did not become ready for initial brief within 30s")
            if submit_at is not None and now >= submit_at:
                pending += b"\r"
                submit_at = None
                submitting = True
            # Terminal query replies must reach the CLI during startup too;
            # waiting for brief delivery here deadlocks cursor-position probes.
            reads = [master] + ([0] if stdin_open else [])
            readable, writable, _ = select.select(reads, [master] if pending else [], [], 0.05)
            if master in readable:
                try:
                    output = os.read(master, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    output = b""
                if not output:
                    break
                write_all(1, output)
                if not outer_tty:
                    query_buffer += output
                    consumed = 0
                    for query in QUERIES.finditer(query_buffer):
                        pending += REPLIES[query[1]]
                        consumed = query.end()
                    query_buffer = query_buffer[consumed:][-32:]
                if not paste_started:
                    screen = (screen + output)[-65536:]
                    # The mode transition can span reads. A raw terminal and
                    # driver chrome are both required: never paste into a shell
                    # or a canonical terminal that could truncate the brief.
                    enable = screen.rfind(b"\x1b[?2004h")
                    disable = screen.rfind(b"\x1b[?2004l")
                    if enable >= 0 or disable >= 0:
                        paste_enabled = enable > disable
                    raw = not termios.tcgetattr(master)[3] & termios.ICANON
                    visible = ANSI.sub(b"", screen)
                    if raw and paste_enabled and any(marker in visible for marker in markers):
                        pending += b"\x1b[200~" + prompt + b"\x1b[201~"
                        paste_started = True
            if 0 in readable:
                data = os.read(0, 65536)
                if data:
                    pending += data
                else:
                    stdin_open = False
            if master in writable:
                count = os.write(master, pending[:4096])
                pending = pending[count:]
                if not pending and submitting:
                    delivered = True
                elif not pending and paste_started and not delivered and submit_at is None:
                    # Let the TUI consume the paste event before Return; this
                    # is the same separation used by terminal paste delivery.
                    submit_at = time.monotonic() + 0.1
        _, status = os.waitpid(pid, 0)
        if not delivered:
            raise RuntimeError("driver exited before receiving the initial brief")
        code = os.waitstatus_to_exitcode(status)
        return code if code >= 0 else 128 - code
    finally:
        os.close(master)
        if saved is not None:
            termios.tcsetattr(0, termios.TCSADRAIN, saved)
        # Closing our master hangs up only the child terminal, never another
        # worker. Reap the direct child when an input/setup error aborts launch.
        try:
            child, _ = os.waitpid(pid, os.WNOHANG)
            if child == 0:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
        except ChildProcessError:
            pass


def main():
    argv = sys.argv[1:]
    if not argv:
        raise ValueError("missing worker driver")
    markers = [s.encode() for s in json.loads(Path(__file__).with_name("initial-prompt.json").read_text())]
    if not markers:
        raise ValueError("missing driver readiness markers")
    prompt_path = Path(__file__).with_name("initial-prompt.txt")
    with prompt_path.open("rb") as prompt_file:
        if os.fstat(prompt_file.fileno()).st_mode & 0o777 != 0o600:
            raise PermissionError("initial brief must have mode 0600")
        prompt = prompt_file.read()
    return run(argv, prompt, markers)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print(f"boss initial brief delivery failed: {error}", file=sys.stderr)
        sys.exit(1)
