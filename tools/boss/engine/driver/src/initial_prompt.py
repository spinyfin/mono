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

    def text(self):
        return "\n".join("".join(row).rstrip() for row in self.rows).encode()

    def resize(self, height, width):
        self.height, self.width = max(1, height), max(1, width)
        self.rows = [row[:self.width] for row in self.rows[:self.height]]
        self.rows.extend([] for _ in range(self.height - len(self.rows)))
        self.x = min(self.x, self.width - 1)
        self.y = min(self.y, self.height - 1)


def ready_for_prompt(view, raw, paste_enabled, markers, starting_markers):
    visible = view.tail()
    return (raw and paste_enabled and any(marker in visible for marker in markers)
            and not any(marker in visible for marker in starting_markers))


# How long the composer may take to show a pasted brief before Return is sent
# without that evidence. Measured against Claude Code 2.1.322 with 47 KB briefs,
# from the last paste byte written to the placeholder appearing: 6-22 ms for an
# ordinary brief and 0.08-3.3 s for one it defers as possible image paths, on a
# host at its normal load; 0.4-2.9 s and 1.5-46 s with 250 busy-loop processes on
# 12 cores. 180 s is 3.9 times that peak. Expiry is not a failure: Return is sent
# once anyway and the engine confirms the turn started.
LANDING_TIMEOUT = 180


def brief_head(prompt):
    """The brief's opening text as a composer shows a short paste, if distinctive."""
    for line in prompt.decode("utf-8", "replace").splitlines():
        head = line.strip()[:24].strip()
        if head:
            return head.encode() if len(head) >= 8 else None
    return None


def brief_landed(screen, echo, baseline, head, landed_markers):
    """How the composer shows it now holds the pasted brief, or None while it does not.

    A composer may commit a paste some time after reading it: Claude Code holds a
    paste with a line that looks like an image path while it asks the system
    clipboard for a file, which takes from a tenth of a second to most of a
    minute. Return sent before that commit reaches an empty composer and is
    dropped, leaving the brief pasted but never submitted, so Return waits for
    this evidence instead of for a fixed delay.
    """
    if head and head in screen and head not in baseline:
        return "literal"
    if any(marker not in baseline and (marker in screen or marker in echo) for marker in landed_markers):
        return "marker"
    # A driver that declares no marker handles input in order: its first output
    # after the whole paste was written is the composer taking it.
    if not landed_markers and (echo or screen != baseline):
        return "repaint"
    return None


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


def run(argv, prompt, markers, starting_markers, readiness_timeout=30, landed_markers=(),
        landing_timeout=LANDING_TIMEOUT, log=lambda step, **fields: None):
    if b"\x1b[201~" in prompt or b"\0" in prompt:
        raise ValueError("initial brief contains a terminal paste terminator or NUL")
    outer_tty = os.isatty(0)
    saved = termios.tcgetattr(0) if outer_tty else None
    master, slave = pty.openpty()
    view = TerminalTail()

    def resize(*_):
        size = (fcntl.ioctl(0, termios.TIOCGWINSZ, bytes(8)) if outer_tty
                else struct.pack("HHHH", 50, 220, 0, 0))
        height, width, _, _ = struct.unpack("HHHH", size)
        height, width = height or 50, width or 220
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        view.resize(height, width)

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
    query_buffer = b""
    delivered = False
    paste_started = False
    submitting = False
    paste_enabled = False
    ready_since = None
    started = time.monotonic()
    # Each stage is bounded on its own: a slow start must not eat the time the
    # composer needs to take the paste.
    stage, deadline = "ready", started + readiness_timeout
    head = brief_head(prompt)
    baseline = echo = b""
    paste_queued_at = paste_written_at = None

    def elapsed_ms(since):
        return int((time.monotonic() - since) * 1000)
    stdin_open = outer_tty
    exit_status = None
    drain_until = None
    try:
        if outer_tty:
            tty.setraw(0, termios.TCSANOW)
        while True:
            now = time.monotonic()
            if exit_status is None:
                # A descendant can hold the pty slave open after the driver
                # exits, so EOF alone never ends the relay. Once the
                # supervisor is gone, drain briefly, then let finally reap the group.
                ended, status = os.waitpid(pid, os.WNOHANG)
                if ended:
                    exit_status, drain_until = status, now + 0.5
            elif now >= drain_until:
                break
            if stage == "landing":
                evidence = brief_landed(view.text(), echo, baseline, head, landed_markers)
                if evidence or now >= deadline:
                    # Without evidence by the deadline Return is still sent, once:
                    # the engine confirms the turn started and fails the spawn if not.
                    log("paste_landed" if evidence else "paste_unverified",
                        evidence=evidence, wait_ms=elapsed_ms(paste_written_at))
                    pending += b"\r"
                    submitting = True
                    stage, deadline = "submit", now + readiness_timeout
            elif stage != "delivered" and now >= deadline:
                raise TimeoutError({
                    "ready": f"driver did not become ready for initial brief within {readiness_timeout}s",
                    "paste": f"driver did not read the initial brief within {readiness_timeout}s",
                    "submit": f"driver did not read the submit key within {readiness_timeout}s",
                }[stage])
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
                if not submitting:
                    view.feed(output)
                if stage == "landing":
                    echo = (echo + output)[-65536:]
                if not paste_started:
                    screen = (screen + output)[-65536:]
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
                        paste_queued_at = now
                        stage, deadline = "paste", now + readiness_timeout
                        log("ready", wait_ms=elapsed_ms(started))
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
                if not pending and submitting and not delivered:
                    delivered = True
                    stage = "delivered"
                    # Durable proof the moment the submit bytes are fully
                    # written, not when the (long-lived) driver exits.
                    write_marker(DELIVERED_MARKER)
                    log("submit_sent")
                elif not pending and stage == "paste":
                    # Return follows only once the composer shows the brief.
                    # The composer cannot have reacted to the whole paste before
                    # its last byte was written, so this is what it must change.
                    baseline = view.text()
                    paste_written_at = time.monotonic()
                    stage, deadline = "landing", paste_written_at + landing_timeout
                    log("paste_sent", bytes=len(prompt), write_ms=elapsed_ms(paste_queued_at))
        if exit_status is None:
            _, exit_status = os.waitpid(pid, 0)
        status = exit_status
        if not delivered:
            raise RuntimeError("driver exited before receiving the initial brief")
        return os.WEXITSTATUS(status) if os.WIFEXITED(status) else 128 + os.WTERMSIG(status)
    finally:
        try:
            os.killpg(pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            # macOS reports EPERM for a group holding an unreaped zombie
            # (the exited driver); the live members still receive the signal.
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
        except (ChildProcessError, ProcessLookupError, PermissionError):
            pass


# EX_USAGE: the driver never received the brief. Distinct from a driver that
# exits 1 after a delivered turn. The marker file is the durable signal.
BRIEF_NOT_DELIVERED_EXIT = 64
DELIVERED_MARKER = "initial-prompt.delivered"
UNDELIVERED_MARKER = "initial-prompt.undelivered"
# One JSON object per delivery step, for the engine to report: the pane is the
# driver's terminal, so the relay has nowhere else to log.
DELIVERY_LOG = "initial-prompt.delivery.jsonl"


def prompt_home():
    prompt_path = Path(__file__).with_name("initial-prompt.txt")
    try:
        return prompt_path.resolve().parent
    except OSError:
        return Path(__file__).parent


def write_marker(name, data=b"", flags=os.O_TRUNC):
    path = prompt_home() / name
    flags |= os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW
    try:
        fd = os.open(path, flags, 0o600)
        try:
            os.fchmod(fd, 0o600)
            write_all(fd, data)
        finally:
            os.close(fd)
    except OSError:
        pass


def log_delivery(step, **fields):
    record = {"step": step, "at_ms": int(time.time() * 1000), **fields}
    write_marker(DELIVERY_LOG, json.dumps(record).encode() + b"\n", os.O_APPEND)


def main():
    # Attempt-scoped evidence for remote cleanup; absent means inconclusive.
    Path(__file__).with_name("worker.pid").write_text(str(os.getpid()))
    argv = sys.argv[1:]
    if not argv:
        raise ValueError("missing worker driver")
    config = json.loads(Path(__file__).with_name("initial-prompt.json").read_text())
    markers = [s.encode() for s in config["agent_markers"]]
    starting_markers = [s.encode() for s in config["starting_markers"]]
    landed_markers = [s.encode() for s in config.get("paste_landed_markers", [])]
    if not markers:
        raise ValueError("missing driver readiness markers")
    prompt_path = Path(__file__).with_name("initial-prompt.txt")
    with prompt_path.open("rb") as prompt_file:
        if os.fstat(prompt_file.fileno()).st_mode & 0o777 != 0o600:
            raise PermissionError("initial brief must have mode 0600")
        prompt = prompt_file.read()
    # A retried launch starts its own record.
    write_marker(DELIVERY_LOG)
    return run(argv, prompt, markers, starting_markers, landed_markers=landed_markers, log=log_delivery)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print(f"boss initial brief delivery failed: {error}", file=sys.stderr)
        log_delivery("failed", reason=str(error))
        if (prompt_home() / DELIVERED_MARKER).is_file():
            sys.exit(1)
        write_marker(UNDELIVERED_MARKER)
        sys.exit(BRIEF_NOT_DELIVERED_EXIT)
