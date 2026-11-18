#!/usr/bin/env python3
"""Helpers for the agy pane viability spike. One file, several subcommands.

  tools.py hooksjson <dump_hook.sh> <out-dir> [event ...]   hooks.json on stdout
  tools.py launcher-json <version>                          initial-prompt.json on stdout
  tools.py briefs <dir>                                     write the test briefs
  tools.py poll <tm> <session> <out.jsonl> [interval]       record tmux capture-pane polls
  tools.py seqs <raw-output.bin>                            terminal mode sequences an app emitted
  tools.py stops <hooks-dir>                                one line per recorded hook
  tools.py markers <runs-dir> <version> [run-prefix ...]    marker presence per phase
  tools.py keyprobe <out-file> <seconds> [enable-hex]       log the bytes a pane app receives
"""
import collections
import glob
import json
import os
import re
import select
import shlex
import subprocess
import sys
import time

EVENTS = ["SessionStart", "PreInvocation", "PostInvocation", "PreToolUse", "PostToolUse", "Stop"]


def hooksjson(argv):
    """Every hook event wired to the dump hook.

    The event name travels in argv because 1.1.12 payloads carry no hookEventName.
    """
    hook, out = argv[0], argv[1]
    events = argv[2:] or EVENTS

    def handler(event):
        return {"type": "command", "command": f"{shlex.quote(hook)} {event} {shlex.quote(out)}", "timeout": 20}

    spec = {}
    for event in events:
        if event in ("PreToolUse", "PostToolUse"):
            spec[event] = [{"matcher": "*", "hooks": [handler(event)]}]
        else:
            spec[event] = [handler(event)]
    json.dump({"spike-dump": spec}, sys.stdout, indent=2)
    print()


def launcher_json(argv):
    """The initial-prompt.json Boss's launcher reads, for one agy version.

    1.1.12 drops a prompt submitted before its account check finishes, and shows
    the composer before then. The only on-screen sign that the check is done is
    the plan name after the account in the banner, so that is its readiness
    marker. 1.5.5 takes the prompt as soon as the composer is up.
    """
    starting = [
        "Welcome to Antigravity CLI!", "Choose your color scheme", "Terms of Service", "Accessing workspace:",
        "Do you trust the contents of this project?", "Signing in...", "You are currently not signed in",
    ]
    agent = ["(Google AI"] if argv[0] == "1.1.12" else ["? for shortcuts"]
    json.dump({"agent_markers": agent, "starting_markers": starting, "paste_landed_markers": []}, sys.stdout, indent=2)
    print()


def briefs(argv):
    out = argv[0]
    os.makedirs(out, exist_ok=True)

    def write(name, lines):
        with open(os.path.join(out, name + ".txt"), "w") as handle:
            handle.write("\n".join(lines) + "\n")

    small = ["Reply with exactly the single token LAUNCH_OK_SMALL and nothing else. Do not use any tools.", "",
             "The rest of this message is filler that stands in for a multi-line worker brief; ignore it.", ""]
    small += [f"- filler line {i:03d}: the quick brown fox jumps over the lazy dog" for i in range(1, 31)]
    write("small", small)
    big = ["Reply with exactly the single token LAUNCH_OK_BIG and nothing else. Do not use any tools.", "",
           "The rest of this message is filler that stands in for a large worker brief; ignore it.", ""]
    big += [f"- filler line {i:04d}: lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt"
            for i in range(1, 451)]
    # A line ending in an image extension: the shape that makes Claude Code defer a paste.
    big += ["", "diff --git a/docs/shot.png b/docs/shot.png", "", "End of brief. Remember: reply with exactly LAUNCH_OK_BIG."]
    write("big", big)
    for seconds, token in ((60, "SLEEP_DONE"), (25, "MID_FIRST_DONE")):
        write(f"sleep{seconds}", [
            "Use your shell command tool to run exactly this command, in the foreground, and wait for it to finish:", "",
            f"    sleep {seconds}", "", "Do not skip the sleep and do not run anything else.",
            f"When the command has finished, reply with exactly the single token {token}."])
    write("count", ["Do not use any tools.", "", "Write the integers from 1 to 700, one per line, with nothing else on each line.",
                    "After the last number, write the single token COUNT_DONE on its own line."])
    write("home", [
        "Use your shell command tool to run exactly this one command, then reply with exactly the single token HOME_PROBE_DONE:", "",
        '    { echo "HOME=$HOME"; echo "USER=$(id -un)"; echo "PWD=$(pwd)"; echo "SPIKE_CANARY=${SPIKE_CANARY:-unset}"; '
        'echo "TERM=$TERM"; echo "git_user=$(git config --global user.name 2>/dev/null || echo none)"; '
        'echo "keychain=$(security default-keychain 2>&1 | tr -d \' \')"; echo "env_count=$(env | wc -l | tr -d \' \')"; '
        "env | cut -d= -f1 | sort | tr '\\n' ' '; echo; } > tool_env_probe.txt"])


def poll(argv):
    """Poll `tmux capture-pane -p` (Boss's Tmux::capture_pane) and record every poll.

    Stops when <out.jsonl>.stop appears or the session is gone.
    """
    tm, session, out = argv[0], argv[1], argv[2]
    interval = float(argv[3]) if len(argv) > 3 else 0.25
    start = time.time()
    last = None
    index = 0
    with open(out, "w") as handle:
        while not os.path.exists(out + ".stop"):
            proc = subprocess.run([tm, "capture-pane", "-p", "-t", session], capture_output=True, text=True)
            if proc.returncode != 0:
                break
            index += 1
            text = proc.stdout
            record = {"i": index, "t": round(time.time() - start, 3), "wall_ms": int(time.time() * 1000), "changed": text != last}
            if text != last:
                record["text"] = text
                last = text
            handle.write(json.dumps(record, ensure_ascii=False) + "\n")
            handle.flush()
            time.sleep(interval)


SEQUENCE_NAMES = {
    b"?1049h": "alt screen on", b"?1049l": "alt screen off", b"?2004h": "bracketed paste on", b"?2004l": "bracketed paste off",
    b"?1004h": "focus reporting on", b"?25h": "cursor show", b"?25l": "cursor hide", b"?2026h": "synchronized output begin",
    b"?2026l": "synchronized output end", b"?1000h": "mouse click on", b"?1002h": "mouse drag on", b"?1003h": "mouse any on",
    b"?1006h": "mouse sgr on", b"?u": "kitty keyboard query", b">4;2m": "xterm modifyOtherKeys mode 2",
    b">4m": "xterm modifyOtherKeys reset", b">5u": "kitty keyboard push, flags 5", b"=1;1u": "kitty keyboard set, flags 1",
    b"=0;1u": "kitty keyboard set, flags 0", b"<1u": "kitty keyboard pop", b"?2026$p": "synchronized output query",
    b"?2027$p": "grapheme clustering query",
}


def seqs(argv):
    data = open(argv[0], "rb").read()
    pattern = re.compile(rb"\x1b\[([?>=<]?[0-9;:]*[$ ]?[a-zA-Z~])")
    count = collections.Counter()
    first = {}
    for match in pattern.finditer(data):
        body = match.group(1)
        if body[-1:] in b"mHJKABCDGdfXPLMST@r" and not body.startswith((b"?", b">", b"=", b"<")):
            continue  # plain styling and cursor movement
        count[body] += 1
        first.setdefault(body, match.start())
    for body, offset in sorted(first.items(), key=lambda item: item[1]):
        print(f"offset={offset:7d} n={count[body]:5d} CSI {body.decode('latin1'):10s} {SEQUENCE_NAMES.get(body, '')}")
    osc = collections.Counter(m.group(1).decode() for m in re.finditer(rb"\x1b\]([0-9]+);", data))
    print("OSC numbers:", dict(osc))
    print("bytes:", len(data))


def stops(argv):
    """One line per recorded hook, with the fields that decide a turn boundary."""
    for path in sorted(glob.glob(os.path.join(argv[0], "*.stdin.json"))):
        name = os.path.basename(path)
        ms, event = name.split("-")[0], name.split(".")[1]
        try:
            payload = json.load(open(path))
        except ValueError as error:
            print(ms, event, "UNPARSEABLE", error)
            continue
        fields = []
        for key in ("conversationId", "invocationNum", "executionNum", "stepIdx", "terminationReason", "fullyIdle", "error"):
            if key in payload:
                value = payload[key]
                fields.append(f"{key}={value[:8] if key == 'conversationId' else value!r}")
        if "toolCall" in payload:
            fields.append("tool=" + str(payload["toolCall"].get("name")))
        if "lastUserInput" in payload:
            fields.append("lastUserInput=" + repr(payload["lastUserInput"][:48]))
        print(ms, f"{event:14s}", " ".join(fields))


GUARD_MS = 600
CANDIDATES = [
    ("agent", "Antigravity CLI"), ("agent", "? for shortcuts"), ("agent", " · low"), ("agent", "Gemini 3.8 Flash"),
    ("busy", "esc to cancel"), ("busy", "Generating..."), ("busy", "Running command..."), ("busy", "Working..."),
    ("busy", "task(s) · /tasks"), ("busy", "Press up to edit queued messages"),
    ("other", "Interrupted · What should Antigravity CLI do instead?"), ("other", "esc to dismiss"), ("other", "Tip:"),
]
PROMPT_LINE = re.compile(r"^\s*>(\s|$)", re.M)


def load_polls(path):
    polls, last = [], ""
    if not os.path.exists(path):
        return polls
    for line in open(path):
        try:
            record = json.loads(line)
        except ValueError:
            continue
        if "text" in record:
            last = record["text"]
        polls.append((record["wall_ms"], last))
    return polls


def phases(run):
    events = []
    for path in sorted(glob.glob(os.path.join(run, "hooks", "*.stdin.json"))):
        name = os.path.basename(path)
        ms, event = int(name.split("-")[0]), name.split(".")[1]
        idle = None
        if event == "Stop":
            try:
                idle = json.load(open(path)).get("fullyIdle")
            except ValueError:
                pass
        events.append((ms, event, idle))
    cut = None
    timeline = os.path.join(run, "timeline.txt")
    if os.path.exists(timeline):
        for line in open(timeline):
            ms, rest = line.split(" ", 1)
            if "Escape press #1" in rest:
                events.append((int(ms), "Escape", None))
            if "snapshot final" in rest and cut is None:
                cut = int(ms)  # teardown types /quit after this; not part of the measurement
    hook_times = [ms for ms, kind, _ in events if kind != "Escape"]
    if not hook_times:
        return None
    events.sort()
    first = min(hook_times)
    last = cut or max(ms for ms, _, _ in events) + 5000
    spans = [(0, first, "starting")]
    open_at, bg_from = None, None
    for ms, event, idle in events:
        if event == "PreInvocation" and open_at is None:
            if bg_from is not None:
                spans.append((bg_from, ms, "bg_wait"))
                bg_from = None
            open_at = ms
        elif event in ("Stop", "Escape") and open_at is not None:
            spans.append((open_at, ms, "busy"))
            open_at = None
            if idle is False:
                bg_from = ms
    if open_at is not None:
        spans.append((open_at, last, "busy"))
    if bg_from is not None:
        spans.append((bg_from, last, "bg_wait"))
    return spans, first, last


def phase_of(ms, spans, first):
    for start, end, phase in spans:
        if start + GUARD_MS <= ms <= end - GUARD_MS:
            return phase
        if start - GUARD_MS < ms < start + GUARD_MS or end - GUARD_MS < ms < end + GUARD_MS:
            if not (phase == "starting" and ms < end - GUARD_MS):
                return None
    return "idle" if ms > first else "starting"


def markers(argv):
    """Count, per poll, which candidate PaneMonitorSpec substrings are on screen in each phase.

    Phases come from evidence, not from the screen:
      starting  before the first hook event of the session
      busy      from the PreInvocation that opens an execution to the Stop that closes it
                (or, for an interrupted turn, to the Escape keypress)
      bg_wait   after a Stop with fullyIdle=false, until the agent is invoked again
      idle      everything else after the first execution began
    Polls within GUARD_MS of a phase edge are dropped: the screen is mid-repaint.
    Two sources are counted separately: `tmux capture-pane` (the engine's read) and the
    GhosttyKit viewport of the attached viewer (the app's read).
    """
    runs_dir, version, prefixes = argv[0], argv[1], argv[2:]
    sources = (("tmux capture-pane", "pane_polls.jsonl"), ("GhosttyKit viewport", os.path.join("viewer", "viewport_polls.jsonl")))
    for source, rel in sources:
        totals = collections.Counter()
        hits = collections.defaultdict(collections.Counter)
        prompt_hits = collections.Counter()
        used = []
        for run in sorted(glob.glob(os.path.join(runs_dir, f"*-{version}"))):
            name = os.path.basename(run)
            if prefixes and not any(name.startswith(p) for p in prefixes):
                continue
            info = phases(run)
            polls = load_polls(os.path.join(run, rel))
            if not info or not polls:
                continue
            spans, first, last = info
            used.append(name)
            for ms, text in polls:
                if ms > last or "Pane is dead" in text or "Process exited" in text or "Do you trust" in text:
                    continue
                phase = phase_of(ms, spans, first)
                if phase is None or not text.strip():
                    continue
                totals[phase] += 1
                for _, needle in CANDIDATES:
                    if needle in text:
                        hits[needle][phase] += 1
                if PROMPT_LINE.search(text):
                    prompt_hits[phase] += 1
        order = ["starting", "busy", "bg_wait", "idle"]
        print(f"## {source} — agy {version} — {len(used)} sessions")
        print("polls per phase: " + ", ".join(f"{p}={totals[p]}" for p in order))
        print(f"{'kind':6s} {'substring':56s} " + " ".join(f"{p:>12s}" for p in order))
        for kind, needle in CANDIDATES:
            print(f"{kind:6s} {needle!r:56s} " + " ".join(f"{hits[needle][p]:5d}/{totals[p]:<6d}" for p in order))
        print(f"{'prompt':6s} {'line starting with >':56s} " + " ".join(f"{prompt_hits[p]:5d}/{totals[p]:<6d}" for p in order))
        print("sessions: " + " ".join(used))
        print()


def keyprobe(argv):
    """Raw-mode key logger: what a pane application receives for a given tmux send-keys.

    Optionally emits the keyboard-mode sequence a TUI emits at startup first.
    """
    import termios
    import tty

    out, seconds = argv[0], float(argv[1])
    enable = bytes.fromhex(argv[2]) if len(argv) > 2 else b""
    saved = termios.tcgetattr(0)
    tty.setraw(0)
    try:
        if enable:
            os.write(1, enable)
        os.write(1, b"KEYPROBE READY\r\n")
        end = time.time() + seconds
        with open(out, "w") as handle:
            while time.time() < end:
                readable, _, _ = select.select([0], [], [], 0.1)
                if readable:
                    data = os.read(0, 4096)
                    handle.write(f"{data!r}\n")
                    handle.flush()
                    os.write(1, repr(data).encode() + b"\r\n")
    finally:
        termios.tcsetattr(0, termios.TCSADRAIN, saved)


COMMANDS = {
    "hooksjson": hooksjson, "launcher-json": launcher_json, "briefs": briefs, "poll": poll, "seqs": seqs, "stops": stops,
    "markers": markers, "keyprobe": keyprobe,
}

if __name__ == "__main__":
    if len(sys.argv) < 2 or sys.argv[1] not in COMMANDS:
        sys.exit(__doc__)
    COMMANDS[sys.argv[1]](sys.argv[2:])
