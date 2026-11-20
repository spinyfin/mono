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
# Reply shapes for the queries above. A real attached terminal reports the
# actual cursor position, not the synthesized 1;1 used when outer_tty is false.
REPLY = re.compile(
    rb"\x1b\[(?:\??\d+;\d+R|\??[\d;]*c|>[\d;]*c|\?\d+u|\?\d+;\d+\$y)"
)
REPLY_BODY = {
    b"?": frozenset(b"0123456789;$"),
    b">": frozenset(b"0123456789;"),
    None: frozenset(b"0123456789;?$"),
}


_CSI = re.compile(r"\x1b\[([0-?]*)([ -/]*)([@-~])")
_OSC_END = re.compile(r"\x07|\x1b\\")


class TerminalTail:
    """Bounded current-screen text for startup detection, excluding scrollback."""

    def __init__(self, height=50, width=220):
        self.height, self.width = height, width
        self.rows = [[] for _ in range(height)]
        self.x = self.y = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    MAX_PENDING = 4096

    def feed(self, output):
        text = self.pending + self.decoder.decode(output)
        pos = 0
        size = len(text)
        while pos < size:
            char = text[pos]
            consumed = 1
            if char == "\x1b":
                if size - pos < 2:
                    break
                nxt = text[pos + 1]
                if nxt == "[":
                    match = _CSI.match(text, pos)
                    if not match:
                        if size - pos > self.MAX_PENDING:
                            consumed = 2  # unterminated sequence: drop the introducer
                        else:
                            break
                    else:
                        self.control(match[1], match[3])
                        consumed = match.end() - pos
                elif nxt == "]":
                    match = _OSC_END.search(text, pos)
                    if not match:
                        if size - pos > self.MAX_PENDING:
                            consumed = 2  # unterminated OSC: drop it, bounded memory
                        else:
                            break
                    else:
                        consumed = match.end() - pos
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
            pos += consumed
        self.pending = text[pos:]

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


class BlockingDialogOnScreen(RuntimeError):
    def __init__(self, marker):
        self.marker = marker.decode("utf-8", "replace")
        super().__init__(f"BlockingDialogOnScreen: pane input refused because a blocking dialog was on screen; matched marker {self.marker!r}")


class BriefDropped(RuntimeError):
    """The CLI reported, on screen, that it discarded the submitted brief."""

    def __init__(self, marker):
        self.marker = marker.decode("utf-8", "replace")
        super().__init__(f"submitted brief was dropped by the driver: screen shows {self.marker!r}")


# How long after the submit the screen is watched for a drop notice. A drop is
# reported within seconds; later output is the turn itself.
POST_SUBMIT_WATCH = 30


def _squash(text):
    return b" ".join(text.split())


def _is_vendor_error(row, marker):
    # `<marker>: <reason>: <4xx/5xx status>`: the shape the CLI prints when it
    # discards a brief.
    return bool(re.match(re.escape(marker) + rb":\s.*\b[45][0-9][0-9]\b", row))

PROMPT_MARK = "❯".encode()
_BOX_PREFIXES = (b"|", "│".encode(), "┃".encode(), "║".encode())


def _strip_box_glyphs(row):
    while row.startswith(_BOX_PREFIXES):
        prefix = next(prefix for prefix in _BOX_PREFIXES if row.startswith(prefix))
        row = row[len(prefix):]
    while row.endswith(_BOX_PREFIXES):
        prefix = next(prefix for prefix in _BOX_PREFIXES if row.endswith(prefix))
        row = row[:-len(prefix)]
    return row.strip()


def _dialog_line(line):
    row = _strip_box_glyphs(line.strip())
    # `❯` marks the selected choice in Claude's and Codex's menus, `>` in
    # Antigravity's.
    while row.startswith((PROMPT_MARK, b">")):
        row = row[len(PROMPT_MARK) if row.startswith(PROMPT_MARK) else 1:]
    return row.strip()


def _is_later_composer(line):
    row = _strip_box_glyphs(line.strip())
    return bool(row) and (PROMPT_MARK in row or row.startswith(b">"))


# Every row of Antigravity's footer-less startup menus: the folder-trust
# dialog, the `Settings Error` screen, and the sign-in menu (numbered, no
# `esc to cancel`; Return on the default row starts Google OAuth). The
# cursor can rest on any of them (a human arrow key), so the guard must
# hold for each selection.
SELECTION_MENUS = (
    (b"Yes, I trust this folder", b"No, exit"),
    (b"Exit and fix manually", b"Continue with defaults this session",
     b"Overwrite the file with defaults"),
    (b"1. Google OAuth", b"2. Use a Google Cloud project"),
)


def _selection_menu_marker(lines, markers):
    # Antigravity's trust dialog, `Settings Error` screen, and sign-in menu
    # have no `esc to cancel` footer. Trust and Settings Error also have no
    # numbered choices; the sign-in menu is numbered. The answer is the
    # `>`-marked row, last on screen. A transcript copy is followed by the
    # real composer, so the last marker row is not a choice.
    lines = list(lines)
    while lines and not lines[-1].strip():
        lines.pop()
    selected = next((index for index in range(len(lines) - 1, -1, -1)
                     if _strip_box_glyphs(lines[index].strip()).startswith(b">")
                     or PROMPT_MARK in _strip_box_glyphs(lines[index].strip())), None)
    if selected is None or not _strip_box_glyphs(lines[selected].strip()).startswith(b">"):
        return None
    selected_row = _dialog_line(lines[selected])
    menu = next((m for m in SELECTION_MENUS if selected_row in m), None)
    if menu is None:
        return None
    # The whole option set must be on screen near the selection, so one
    # quoted row in prose or a transcript cannot qualify.
    nearby = {_dialog_line(row) for row in lines[max(0, selected - len(menu)):selected + len(menu)]}
    if not all(choice in nearby for choice in menu):
        return None
    for marker in markers:
        for index in range(selected):
            if (len(lines) - index <= 12 and marker
                    and _dialog_line(lines[index]).lstrip("⚠".encode()).strip().startswith(marker)):
                return marker
    return None


def check_blocking_dialog(view, markers, payload=b""):
    visible = view.text()
    if len(payload) <= len(visible) and b"\n" in payload:
        visible = visible.replace("❯ ".encode() + payload, b"", 1)
    lines = visible.splitlines()
    menu = _selection_menu_marker(lines, markers)
    if menu is not None:
        raise BlockingDialogOnScreen(menu)
    footer = next((index for index in range(len(lines) - 1, -1, -1)
                   if b"esc to cancel" in _dialog_line(lines[index]).lower()), None)
    if footer is None:
        return
    if any(_is_later_composer(row) for row in lines[footer + 1:]):
        return
    lines = lines[:footer + 1]
    for marker in markers:
        for index, line in enumerate(lines):
            question = _dialog_line(line)
            window = lines[max(0, index - 2):]
            tail = [_dialog_line(row) for row in window]
            numbered = any(row.startswith(b"1.") for row in tail) and any(row.startswith(b"2.") for row in tail)
            trust = (b"No, exit" in tail
                     and (b"Yes, I trust this folder" in tail or b"Yes, I accept" in tail)
                     and any(PROMPT_MARK in row for row in window))
            if len(lines) - index <= 12 and (numbered or trust):
                if marker and question.startswith(marker):
                    raise BlockingDialogOnScreen(marker)


def check_relay_input(view, markers, pending):
    """Refuse to forward relay input into a selection menu after delivery.

    A menu that appears mid-run (for example expired credentials showing the
    sign-in menu) must not receive Return or text. Pure terminal-query replies
    still pass so the CLI is not starved of them.
    """
    menu = _selection_menu_marker(view.text().splitlines(), markers)
    if menu is None:
        return
    _, rest, refused = take_terminal_replies(pending, b"")
    if refused or rest:
        raise BlockingDialogOnScreen(menu)


def has_user_bytes(pending):
    """True when pending holds anything other than recognised terminal replies."""
    _, rest, refused = take_terminal_replies(pending, b"")
    return bool(refused or rest)


def take_terminal_replies(data, buffer):
    """Split stdin into recognised terminal replies vs other input.

    Returns (forwarded, remaining_buffer, refused). Incomplete CSI that could
    still become a reply stays in remaining_buffer.
    """
    buffer += data
    forwarded = bytearray()
    while buffer:
        match = REPLY.match(buffer)
        if match:
            forwarded += match.group(0)
            buffer = buffer[match.end():]
            continue
        if _reply_prefix(buffer):
            return bytes(forwarded), buffer, False
        return bytes(forwarded), buffer, True
    return bytes(forwarded), b"", False


def _reply_prefix(buffer):
    if buffer in (b"", b"\x1b", b"\x1b["):
        return True
    if not buffer.startswith(b"\x1b[") or len(buffer) >= 32:
        return False
    rest = buffer[2:]
    allowed = REPLY_BODY.get(rest[:1], REPLY_BODY[None])
    body = rest[1:] if rest[:1] in REPLY_BODY else rest
    return not body or all(byte in allowed for byte in body)


SETTINGS_LOG_POLL = 0.25
SETTINGS_SETTLE = 2


class SettingsLogWatcher:
    """Incremental, throttled admission against a CLI settings log.

    Loads the check JSON once. Remembers initialization and only reads new
    log bytes. A truncated or missing log after initialization is fatal.
    """

    def __init__(self, check_path):
        check = json.loads(Path(check_path).read_text())
        self.log_path = Path(check["log_path"])
        self.expected = check["expected"]
        # Lines that must appear before admission, and lines that must never
        # appear. Optional: a check file without them gates on settings alone.
        self.required = frozenset(check.get("require", []))
        self.pending = set(self.required)
        self.forbid = list(check.get("forbid", []))
        self.offset = 0
        self.buf = b""
        self.initialized = False
        self.initialized_at = None
        self.last_poll = None
        self.ready = False

    def poll(self, now):
        if self.last_poll is not None and now - self.last_poll < SETTINGS_LOG_POLL:
            return self.ready
        self.last_poll = now
        try:
            size = self.log_path.stat().st_size
        except FileNotFoundError:
            self._disappeared()
            return False
        if size < self.offset:
            self._disappeared()
            return False
        with self.log_path.open("rb") as log:
            log.seek(self.offset)
            chunk = log.read()
        self.offset += len(chunk)
        self.buf += chunk
        while True:
            index = self.buf.find(b"\n")
            if index < 0:
                break
            line = self.buf[:index].decode("utf-8", "replace")
            self.buf = self.buf[index + 1:]
            self._consume(line, now, terminated=True)
        if self.buf:
            # A read can land mid-write: only the invalid-rule check is safe
            # on an unterminated tail; the initialization line is judged once
            # its newline arrives.
            self._consume(self.buf.decode("utf-8", "replace"), now, terminated=False)
        self.ready = bool(
            self.initialized and not self.pending and self.initialized_at is not None
            and now - self.initialized_at >= SETTINGS_SETTLE
        )
        return self.ready

    def _consume(self, line, now, terminated):
        if "ignoring invalid" in line:
            raise RuntimeError(f"agy dropped rendered permission rules: {line}")
        for text in self.forbid:
            if text in line:
                raise RuntimeError(f"agy reported a startup failure that disarms the guards: {line}")
        if terminated:
            self.pending = {text for text in self.pending if text not in line}
        if terminated and "CLI settings initialized" in line:
            if self.expected not in line:
                raise RuntimeError(
                    f"agy initialized settings that differ from rendered posture: {line}"
                )
            if not self.initialized:
                self.initialized = True
                self.initialized_at = now

    def _disappeared(self):
        if self.initialized:
            raise RuntimeError("agy settings initialization evidence disappeared")
        self.offset = 0
        self.buf = b""
        self.pending = set(self.required)
        self.ready = False


def run(argv, prompt, markers, starting_markers, readiness_timeout=30, landed_markers=(),
        landing_timeout=LANDING_TIMEOUT, log=lambda step, **fields: None, blocking_dialog_markers=(),
        settings_check=None, post_submit_failure_markers=()):
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
    stdin_reply_buf = b""
    settings_watch = None if settings_check is None else SettingsLogWatcher(settings_check)
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
    watch_until = 0
    submit_rows = set()
    guard_enabled = True
    payload_squashed = _squash(prompt)
    try:
        if outer_tty:
            tty.setraw(0, termios.TCSANOW)
        while True:
            now = time.monotonic()
            settings_ready = settings_watch is None or settings_watch.poll(now)
            if paste_started and not settings_ready:
                raise RuntimeError("agy settings initialization evidence disappeared")
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
                # Name the startup screen that held delivery back, so the failure
                # says what the driver was showing instead of only that it was slow.
                held = next((marker for marker in starting_markers if marker in view.tail()), None)
                raise TimeoutError({
                    "ready": f"driver did not become ready for initial brief within {readiness_timeout}s"
                             + (f"; it was still showing {held.decode('utf-8', 'replace')!r}" if held else ""),
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
                # The screen stays current for the whole relay: later input is
                # checked against it, not only the initial delivery.
                if not delivered:
                    view.feed(output)
                elif guard_enabled:
                    try:
                        view.feed(output)
                    except Exception as error:
                        # Fail open, loudly: the brief is already delivered and the
                        # worker is long-lived, so an emulator bug disables the
                        # relay-input guard instead of terminating the session.
                        guard_enabled = False
                        log("relay_guard_disabled", error=repr(error))
                if not delivered:
                    check_blocking_dialog(view, blocking_dialog_markers, prompt if paste_started else b"")
                elif guard_enabled and post_submit_failure_markers and now < watch_until:
                    # An accepted paste does not prove a turn started: the CLI
                    # can discard the brief with a vendor error and fire no hook.
                    # Only rows that appeared after submit count, and only
                    # ones that are not a piece of the brief itself (however
                    # far it scrolled or however it was re-wrapped) and that
                    # carry the full vendor error shape, so a half-painted row
                    # cannot qualify.
                    rows = [_dialog_line(row) for row in view.text().splitlines()]
                    dropped = next((m for m in post_submit_failure_markers
                                    if any(_is_vendor_error(row, m) and row not in submit_rows
                                           and _squash(row) not in payload_squashed for row in rows)), None)
                    if dropped is not None:
                        log("post_submit_failure", marker=dropped.decode("utf-8", "replace"))
                        raise BriefDropped(dropped)
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
                if settings_ready and ready_for_prompt(view, raw, paste_enabled, markers, starting_markers):
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
                    if settings_watch is not None and not settings_ready:
                        replies, stdin_reply_buf, refused = take_terminal_replies(data, stdin_reply_buf)
                        pending += replies
                        if refused:
                            raise RuntimeError("agy settings have not initialized; input refused")
                    else:
                        if stdin_reply_buf:
                            pending += stdin_reply_buf
                            stdin_reply_buf = b""
                        pending += data
                else:
                    stdin_open = False
            if master in writable:
                # Drain already available output before checking the current screen.
                # A dialog may have appeared after readiness or during the paste.
                # After delivery the same drain applies whenever the pending bytes
                # are anything but terminal-query replies: a menu already queued
                # on the master must reach the screen view before user bytes
                # are forwarded to it.
                if (not delivered or has_user_bytes(pending)) and select.select([master], [], [], 0)[0]:
                    continue
                if not delivered:
                    check_blocking_dialog(view, blocking_dialog_markers, prompt if paste_started else b"")
                elif guard_enabled:
                    check_relay_input(view, blocking_dialog_markers, pending)
                count = os.write(master, pending[:4096])
                pending = pending[count:]
                if not pending and submitting and not delivered:
                    delivered = True
                    stage = "delivered"
                    # Durable proof the moment the submit bytes are fully
                    # written, not when the (long-lived) driver exits.
                    write_marker(DELIVERED_MARKER)
                    watch_until = time.monotonic() + POST_SUBMIT_WATCH
                    submit_rows = {_dialog_line(row) for row in view.text().splitlines()}
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
    return run(argv, prompt, markers, starting_markers, landed_markers=landed_markers, log=log_delivery,
               blocking_dialog_markers=[s.encode() for s in config.get("blocking_dialog_markers", [])],
               post_submit_failure_markers=[s.encode() for s in config.get("post_submit_failure_markers", [])],
               settings_check=Path(__file__).with_name("settings-log-check.json")
               if config.get("check_settings_log") else None)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print(f"boss initial brief delivery failed: {error}", file=sys.stderr)
        log_delivery("failed", reason=str(error),
                     blocking_dialog_marker=error.marker if isinstance(error, BlockingDialogOnScreen) else None)
        if (prompt_home() / DELIVERED_MARKER).is_file():
            sys.exit(1)
        write_marker(UNDELIVERED_MARKER)
        sys.exit(BRIEF_NOT_DELIVERED_EXIT)
