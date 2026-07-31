#!/usr/bin/env python3
"""Poll a tmux-hosted codex TUI viewport and record marker presence per poll.

Not the GhosttyKit apparatus — this measures *persistence across polls* of the
literals already captured under GhosttyKit (pivot spike V5). tmux capture-pane
without -S is the visible pane == the viewport read Boss's monitor performs.
"""
import subprocess
import sys
import time
from pathlib import Path

S = Path(sys.argv[1])
SESSION = "codexmk"
POLLS = int(sys.argv[2]) if len(sys.argv) > 2 else 120
INTERVAL = float(sys.argv[3]) if len(sys.argv) > 3 else 0.5

CANDIDATES = {
    "hdr:>_ OpenAI Codex": ">_ OpenAI Codex",
    "hdr:/model to change": "/model to change",
    "hdr:permissions:": "permissions:",
    "busy:esc to interrupt": "esc to interrupt",
    "start:Booting MCP server:": "Booting MCP server:",
    "prompt:›": "›",
    "irq:Conversation interrupted": "■ Conversation interrupted",
    "foot:? for shortcuts": "? for shortcuts",
    "foot:send": "send",
    "foot:Ctrl+C": "Ctrl+C",
    "work:Working": "Working",
    "work:Esc to interrupt": "Esc to interrupt",
}

(S / "polls").mkdir(exist_ok=True)
rows = []
for i in range(1, POLLS + 1):
    p = subprocess.run(
        ["tmux", "capture-pane", "-p", "-t", SESSION],
        capture_output=True, text=True,
    )
    if p.returncode != 0:
        rows.append((i, None, {}))
        break
    view = p.stdout
    (S / "polls" / f"viewport-{i:03d}.txt").write_text(view)
    hits = {k: (v in view) for k, v in CANDIDATES.items()}
    rows.append((i, view, hits))
    time.sleep(INTERVAL)

# Summary: per-marker hit counts, split by busy vs not-busy polls.
live = [(i, v, h) for (i, v, h) in rows if v is not None]
busy_polls = [h for (_, _, h) in live if h["busy:esc to interrupt"]]
idle_polls = [h for (_, _, h) in live if not h["busy:esc to interrupt"]]
print(f"polls={len(live)} busy={len(busy_polls)} nonbusy={len(idle_polls)}")
print(f"{'marker':32} {'all':>9} {'busy':>9} {'nonbusy':>9}")
for k in CANDIDATES:
    a = sum(1 for (_, _, h) in live if h[k])
    b = sum(1 for h in busy_polls if h[k])
    c = sum(1 for h in idle_polls if h[k])
    print(f"{k:32} {a:>4}/{len(live):<4} {b:>4}/{len(busy_polls):<4} {c:>4}/{len(idle_polls):<4}")

# First poll at which each marker was last seen (scroll-out detection).
print("\nlast poll each marker was present:")
for k in CANDIDATES:
    seen = [i for (i, _, h) in live if h[k]]
    print(f"  {k:32} first={seen[0] if seen else '-'} last={seen[-1] if seen else '-'}")
