# The tmux client input wedge

A Boss pane is a libghostty surface whose pty runs `tmux attach-session`
against Boss's private server (`tmux -L boss`). The coordinator pane has been
observed entering a state where it **renders output normally but sends no
input**: text typed from a separate `tmux attach` in a terminal appeared in
the app's view in real time, while nothing typed into the app reached the
session. There was no way to recover from inside the app.

This document records what the signals actually mean, what the evidence does
and does not establish about the cause, and the contract the detection and
recovery in [`tmux_input_watch`](../engine/core/src/tmux_input_watch.rs) is
built on.

## The observation

`tmux list-clients` on the Boss socket, sampled twice several minutes apart:

    tty=/dev/ttys002 size=67x101 activity=1787149126     <- the app, FROZEN
    tty=/dev/ttys007 size=72x61  activity=1787149452     <- operator terminal, ticking

The pane process was healthy throughout — not stopped, not a zombie, not in
copy-mode, pane not dead — and the session was fine, driven from the second
client. `refresh-client -S` redrew the app's view without reviving input.

`tmux detach-client -t /dev/ttys002` fixed it. The app reattached, input
worked again, and the session, the pane and the coordinator process all
survived.

## What the tmux fields actually mean

Measured against tmux 3.6a on a scratch server (never the `boss` socket),
with a session running `while true; do date; sleep 1; done`:

| Field                | Advances on                                 | Evidence                                                                                                                                                                            |
| -------------------- | ------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `#{client_activity}` | **input from that client only**             | Frozen at `…812` across 12s of pane output; jumped to `…825` the instant one byte was written into the client's pty                                                                 |
| `#{window_activity}` | **pane output**, with or without any client | `…645 → …648 → …651` on a session with zero clients attached                                                                                                                        |
| `#{client_pid}`      | —                                           | Equals the pid of the process that ran `attach-session`. Boss's viewers `exec` tmux from their launch shell, so it is what the app reads back from `ghostty_surface_foreground_pid` |

`detach-client -t <tty>` was measured too: the attach process exits 0 within
0.3s, `list-sessions` still reports the session, and `#{pane_pid}` is
unchanged with `pane_dead=0`.

### The consequence: "frozen activity while output flows" is not a signal

That pairing is _exactly_ what a healthy client looks like when nobody is
typing. The coordinator is an agent that emits output for minutes unattended,
so a rule built on it would fire constantly on a perfectly working pane.

From the server's side the two states are genuinely indistinguishable: "no
input was sent" and "input was sent and lost" produce identical field values.
The missing half — whether input was _attempted_ — exists only in the app.

## What the evidence establishes about the cause

The app's view kept updating **in real time** from the operator's typing on
the second client. tmux's client is a single libevent loop: server→client
output and client→server input are serviced by the same loop. A client stuck
anywhere in that loop — blocked writing to its pty, wedged in a protocol
exchange — could not have been forwarding output live.

So the client process was alive and pumping, and its `client_activity` never
moved. **No input bytes ever reached the client's pty.** The failure is
app-side, upstream of the pty.

This narrows the candidate list materially:

- **Ruled out — the tmux client process.** Its event loop was demonstrably
  servicing traffic throughout.
- **Ruled out — a pty write-back deadlock** (server→client output backing up,
  client blocked in `write()`, never returning to its input `poll()`). Same
  argument: output was flowing.
- **Ruled out — `window-size latest`.** The Boss server's `window-size` was
  found to be `latest`, so the operator's 72x61 terminal reshaped the window
  for the app's 67x101 surface. That is a geometry concern on the server; it
  cannot stop keystrokes reaching a pty, and the client loop was healthy.
  (Being addressed separately with the other Boss-owned session options.)
- **Ruled out — the second attach as trigger.** It attached at 09:20:45,
  roughly two minutes _after_ the wedge began at 09:18:46, and no mechanism
  exists by which another client affects the app's own pty write path.
- **Still open — AppKit first-responder loss.** `GhosttyTerminalHostView`
  only receives `keyDown` while it is first responder. `mouseDown` does call
  `window?.makeFirstResponder(self)`, which weakens this without eliminating
  it (we do not know a click was tried before the operator gave up).
- **Still open — libghostty's key/write path.** Ghostty services pty reads on
  a separate thread from writes, so a stalled writer gives precisely
  "renders, does not type". This would be invisible to the app: every one of
  the six `ghostty_surface_key` call sites discards its `bool` return, and
  that return means "was the key consumed", not "was it written" — so it is
  not a usable failure signal even if it were checked.

Discriminating the last two needs a live reproduction with the surface's
focus state and libghostty's write path both instrumented; it has not been
reproduced on demand. The recovery below is therefore the deliverable, and
the root cause remains narrowed rather than closed.

## Detection

The engine correlates the two sides:

- The app reports `ReportPaneClientInput` — carrying the tmux session, the
  client pid and an epoch-second stamp — on every input it delivers into a
  tmux-hosted pane, coalesced to at most one report per second per pane.
- The watch reads `list-clients -t <session>` and finds the row whose
  `#{client_pid}` matches the report.

A wedge is `last_input_epoch > client_activity + 3s`, sustained for 3
consecutive 5-second passes. An idle session cannot trip it by construction:
with nothing typed there is no report to judge, and with a report the app's
stamp is never ahead of what the server recorded.

The watch only samples sessions the app has reported input for, so an idle
Boss makes no tmux calls at all.

## Recovery

`detach-client -t <tty>`, on the app's own client and nothing else:

- Addressed **by tty**, and only the tty whose `client_pid` matches the
  app's report — an operator's terminal attached to the same session is never
  evicted.
- The client process exits 0; `BossPaneModel` observes the child exit and
  rebuilds the surface and client through its existing reattach path.
- The session, its pane, and the coordinator process are untouched. Session
  survival is the entire point of tmux hosting; a recovery that destroyed it
  would be worse than the bug.

Bounded to 3 recoveries per 10 minutes. Past that the watch latches and
escalates instead of detaching again — a viewer that re-wedges immediately is
a defect to look at, not one to keep papering over. A 30-second settle period
after each detach keeps the replacement viewer from being judged against the
outgoing client's report.

## Surfacing

Every recovery is recorded three ways, because a silent self-heal that
happens repeatedly hides a live defect:

1. A `tracing::warn!` with the session, tty, client pid and recovery count.
2. A `tmux_client_input_wedge_recovered` event in `engine-audit.log` (see
   [forensic-surfaces.md](forensic-surfaces.md)).
3. A project-scoped attention item naming the pane and how many times this
   has happened in the last 10 minutes. Attention dedup is content-keyed, so
   a rising count appends a new item to the same group rather than being
   swallowed as a duplicate.

## Worker panes

The same failure is reachable for workers once `workers.tmux_hosting` is
enabled. `WorkersWorkspaceModel.attachWorkerPane` builds the identical shape
the coordinator uses — a Ghostty surface whose pty runs `exec tmux -L boss
attach-session -t <session>` — so nothing about the wedge is specific to the
coordinator; it has simply only been observed there because worker panes are
not tmux-hosted today.

Detection is session-keyed rather than coordinator-specific, and
`attachWorkerPane` already installs a `TmuxClientInputReporter`, so
tmux-hosted workers are covered by the same watch with no further wiring.
