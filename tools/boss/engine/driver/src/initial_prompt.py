"""Deliver a private brief to an interactive CLI without exposing it in argv.

Own the child terminal for the lifetime of the worker, independently of the
engine. Only the driver and its flags are arguments. Subsequent input, resize,
signals, output and exit status pass through to the same interactive session.
"""

import codecs
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
import unicodedata


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


class TerminalTail:
    """Bounded current-screen text for startup detection, excluding scrollback."""

    def __init__(self, height=50, width=220):
        self.height, self.width = height, width
        self.rows = [[] for _ in range(height)]
        self.x = self.y = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, output):
        self.pending += self.decoder.decode(output)
        while self.pending:
            char = self.pending[0]
            consumed = 1
            if char == "\x1b":
                if len(self.pending) < 2:
                    break
                if self.pending[1] == "[":
                    match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", self.pending)
                    if not match:
                        break
                    self.control(match[1], match[3])
                    consumed = match.end()
                elif self.pending[1] == "]":
                    match = re.search(r"\x07|\x1b\\", self.pending)
                    if not match:
                        break
                    consumed = match.end()
                else:
                    consumed = 2
            elif char == "\r":
                self.x = 0
            elif char == "\n":
                self.y += 1
            elif char == "\b":
                self.x = max(0, self.x - 1)
            elif char >= " ":
                if self.x >= self.width:
                    self.x = 0
                    self.y += 1
                self.scroll()
                row = self.rows[self.y]
                while len(row) <= self.x:
                    row.append(" ")
                row[self.x] = char
                self.x += 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
            self.scroll()
            self.pending = self.pending[consumed:]

    def scroll(self):
        while self.y >= self.height:
            self.rows.pop(0)
            self.rows.append([])
            self.y -= 1

    def control(self, parameters, command):
        if any(char not in "0123456789;" for char in parameters):
            return
        values = [int(value or "0") for value in parameters.split(";")]
        count = values[0] or 1
        if command in ("H", "f"):
            self.y = min(self.height - 1, max(0, count - 1))
            self.x = min(self.width - 1, max(0, (values[1] if len(values) > 1 else 1) - 1))
        elif command == "A":
            self.y = max(0, self.y - count)
        elif command == "B":
            self.y = min(self.height - 1, self.y + count)
        elif command == "C":
            self.x = min(self.width - 1, self.x + count)
        elif command == "D":
            self.x = max(0, self.x - count)
        elif command == "G":
            self.x = min(self.width - 1, count - 1)
        elif command == "J":
            if values[0] in (2, 3):
                self.rows = [[] for _ in range(self.height)]
            elif values[0] == 0:
                self.rows[self.y] = self.rows[self.y][:self.x]
                for row in range(self.y + 1, self.height):
                    self.rows[row] = []
        elif command == "K":
            if values[0] == 2:
                self.rows[self.y] = []
            elif values[0] == 0:
                self.rows[self.y] = self.rows[self.y][:self.x]

    def tail(self):
        lines = ["".join(row).rstrip() for row in self.rows]
        while lines and not lines[-1]:
            lines.pop()
        return "\n".join(lines[-12:]).encode()


def ready_for_prompt(view, raw, paste_enabled, markers, starting_markers):
    visible = view.tail()
    return (raw and paste_enabled and any(marker in visible for marker in markers)
            and not any(marker in visible for marker in starting_markers))


def write_all(fd, data):
    while data:
        count = os.write(fd, data)
        data = data[count:]


def spawn_supervised(argv, master, slave):
    # The supervisor shares the driver session, but survives TERM/HUP. A
    # private pipe ties that session to the relay even when SIGKILL prevents
    # the relay from forwarding signals or running its finally block.
    reader, writer = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(writer)
        os.close(master)
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
            signal.signal(signum, signal.SIG_IGN)
        child = os.fork()
        if child == 0:
            os.close(reader)
            for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
                signal.signal(signum, signal.SIG_DFL)
            for fd in (0, 1, 2):
                os.dup2(slave, fd)
            if slave > 2:
                os.close(slave)
            os.environ.setdefault("TERM", "xterm-256color")
            os.execvp(argv[0], argv)
        os.close(slave)
        for fd in (0, 1, 2):
            os.close(fd)
        while True:
            ended, status = os.waitpid(child, os.WNOHANG)
            if ended:
                os._exit(os.WEXITSTATUS(status) if os.WIFEXITED(status) else 128 + os.WTERMSIG(status))
            readable, _, _ = select.select([reader], [], [], 0.05)
            if readable and not os.read(reader, 1):
                os.killpg(os.getpid(), signal.SIGKILL)
    os.close(reader)
    os.close(slave)
    return pid, writer


def run(argv, prompt, markers, starting_markers, readiness_timeout=30):
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
    pid, lifeline = spawn_supervised(argv, master, slave)

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
    view = TerminalTail()
    query_buffer = b""
    delivered = False
    paste_started = False
    submitting = False
    paste_enabled = False
    ready_since = None
    submit_at = None
    deadline = time.monotonic() + readiness_timeout
    stdin_open = outer_tty
    try:
        if outer_tty:
            tty.setraw(0, termios.TCSANOW)
        while True:
            now = time.monotonic()
            if not delivered and now >= deadline:
                raise TimeoutError(f"driver did not become ready for initial brief within {readiness_timeout}s")
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
                    view.feed(output)
                    # The mode transition can span reads. A raw terminal and
                    # driver chrome are both required: never paste into a shell
                    # or a canonical terminal that could truncate the brief.
                    enable = screen.rfind(b"\x1b[?2004h")
                    disable = screen.rfind(b"\x1b[?2004l")
                    if enable >= 0 or disable >= 0:
                        paste_enabled = enable > disable
            if not paste_started:
                raw = not termios.tcgetattr(master)[3] & termios.ICANON
                if ready_for_prompt(view, raw, paste_enabled, markers, starting_markers):
                    if ready_since is None:
                        ready_since = now
                    elif now - ready_since >= 0.2:
                        pending += b"\x1b[200~" + prompt + b"\x1b[201~"
                        paste_started = True
                else:
                    ready_since = None
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
        return os.WEXITSTATUS(status) if os.WIFEXITED(status) else 128 + os.WTERMSIG(status)
    finally:
        try:
            os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        os.close(lifeline)
        os.close(master)
        if saved is not None:
            termios.tcsetattr(0, termios.TCSADRAIN, saved)
        # Closing our master hangs up only the child terminal, never another
        # worker. Reap the direct child when an input/setup error aborts launch.
        try:
            child, _ = os.waitpid(pid, os.WNOHANG)
            if child == 0:
                os.killpg(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
        except ChildProcessError:
            pass


def main():
    argv = sys.argv[1:]
    if not argv:
        raise ValueError("missing worker driver")
    config = json.loads(Path(__file__).with_name("initial-prompt.json").read_text())
    markers = [s.encode() for s in config["agent_markers"]]
    starting_markers = [s.encode() for s in config["starting_markers"]]
    if not markers:
        raise ValueError("missing driver readiness markers")
    prompt_path = Path(__file__).with_name("initial-prompt.txt")
    with prompt_path.open("rb") as prompt_file:
        if os.fstat(prompt_file.fileno()).st_mode & 0o777 != 0o600:
            raise PermissionError("initial brief must have mode 0600")
        prompt = prompt_file.read()
    return run(argv, prompt, markers, starting_markers)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print(f"boss initial brief delivery failed: {error}", file=sys.stderr)
        sys.exit(1)
