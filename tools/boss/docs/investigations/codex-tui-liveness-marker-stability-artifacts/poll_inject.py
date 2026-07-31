#!/usr/bin/env python3
"""Run 3: does the mid-turn queued-message affordance leave `esc to interrupt`
in the viewport after the turn ends? (Boss delivers nudges/steers mid-turn.)"""
import subprocess
import sys
import time
from pathlib import Path

S = Path(sys.argv[1])
SESSION = "codexmk"
POLLS = int(sys.argv[2])
INTERVAL = float(sys.argv[3])
INJECT_AT = int(sys.argv[4])

out = S / "polls-run3"
out.mkdir(exist_ok=True)
rows = []
for i in range(1, POLLS + 1):
    if i == INJECT_AT:
        subprocess.run(["tmux", "send-keys", "-t", SESSION,
                        "MIDPROBE: no tools, just finish the current work", "Enter"])
    p = subprocess.run(["tmux", "capture-pane", "-p", "-t", SESSION],
                       capture_output=True, text=True)
    if p.returncode != 0:
        break
    view = p.stdout
    (out / f"viewport-{i:03d}.txt").write_text(view)
    rows.append((i, "esc to interrupt" in view, "Messages to be submitted" in view,
                 "esc to interrupt and send immediately" in view))
    time.sleep(INTERVAL)


def runs(idx):
    spans, start = [], None
    for i, r in enumerate(rows):
        if r[idx] and start is None:
            start = r[0]
        elif not r[idx] and start is not None:
            spans.append((start, rows[i - 1][0]))
            start = None
    if start is not None:
        spans.append((start, rows[-1][0]))
    return spans


print(f"polls={len(rows)} inject_at={INJECT_AT}")
print("esc-to-interrupt spans:      ", runs(1))
print("queued-affordance spans:     ", runs(2))
print("queued-affordance-with-esc:  ", runs(3))
