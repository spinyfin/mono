# Coordinator session handoff

When the coordinator session restarts, the incoming session starts with none of what the operator told the outgoing one. This document describes the handoff that closes that gap: what the outgoing session writes, where it lives, how the incoming session is briefed, and what an operator should expect to see.

## The problem it solves

The coordinator is a single long-lived Claude Code session. It is replaced by a fresh one when:

- a Claude Code update ends the `claude` process (or the operator resets the coordinator to pick the update up),
- the app or engine restarts in a way that loses the tmux session,
- the `claude` process crashes or exits,
- the engine's coordinator restart supervisor recreates it after a failure streak.

None of these give the outgoing session a chance to run anything. Everything the operator said in it — "I've taken greyarea down", "I re-enabled tmux", "don't file chores about X" — is gone, and the next session acts on stale evidence until the operator notices and repeats themselves. The incident that motivated this: a session filed a chore against a CI host the operator had shut down 40 minutes earlier, then briefed an investigation agent on tmux state the operator had already reversed.

## Design

### What triggers the write: a rolling handoff, refreshed at boundaries

A shutdown-time write is the wrong design here, because the cases above kill the session with no chance to run one — a shutdown-only write would silently produce nothing in exactly the case that matters.

Instead the coordinator keeps a **rolling handoff** and rewrites it at natural boundaries: whenever the operator states a fact that changes the world (a host taken down or brought back, a flag or setting flipped, dispatch paused or resumed), makes a decision, or says not to do something; when an open thread starts or resolves; and before acknowledging a request to restart, update, or reset it. Each write replaces the whole handoff, carrying forward what is still true. The "Session handoff" section of the coordinator prompt (`bossSystemPrompt` in `tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift`) is what binds the coordinator to this.

The handoff is written by the coordinator session itself. The engine never synthesizes one from logs or transcripts.

### Where it lives

One JSON value in the `metadata` table of the engine's own database, under the key `coordinator.handoff`:

```json
{ "body": "...", "written_at": 1756800000, "writer_spawn_token": "…" }
```

`written_at` is Unix epoch seconds; `writer_spawn_token` is the spawn token of the coordinator record that was live when the write happened. This is coordinator-private state — it never enters a repo — and it is engine-owned, so it survives app restarts and is reachable only through the engine's RPC surface (`boss handoff …`), never by reading the database.

Alongside it, the coordinator record now stamps `coordinator.tmux_spawned_at` (when the current session started). That is what lets the brief say whether the session that just ended wrote anything after it started.

### How the incoming session consumes it

Every _fresh_ coordinator session is launched with a **session-start brief** as `claude`'s positional initial prompt — the same mechanism worker panes use for their initial prompt. The engine composes the brief in `start_new` (`tools/boss/engine/core/src/coordinator_tmux.rs`, `prepare_session_start_brief`), writes it to `.claude/handoff-brief.txt` in the coordinator session directory, and launches:

```
exec claude --model <model> --permission-mode auto "$(cat '<session dir>/.claude/handoff-brief.txt')"
```

So the incoming session reads the handoff on its very first turn, before the operator types anything, with no pane injection to time and no dependence on the model choosing to open a file. If the brief file cannot be written, the engine launches with a short inline notice instead (naming the error and pointing at `boss handoff show`), never a bare session.

An _adopted_ session — the engine restarted but the coordinator's tmux session and `claude` process survived — keeps its own context and receives no brief. That path is the existing prompt-change nudge (`maybe_nudge_prompt_change` in the same file), which re-reads the rendered `CLAUDE.md` when its content changed across the restart. The two mechanisms are complementary and share the coordinator lifecycle code; neither replaces the other.

The brief also carries **why** the previous session ended (tmux session missing, `claude` process exited, operator reset, model change, or first creation), rendered from `CoordinatorStartReason`.

### The states the brief reports

The brief opens with exactly one of these, and the incoming session is instructed to state which applies in its first reply:

| State                            | Meaning                                                                                                                                       | What the session is told to do                                                                      |
| -------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| **HANDOFF PRESENT**              | The session that just ended wrote it (its spawn token matches the writer), written _N_ ago.                                                   | Summarize it; treat every fact as "as of when it was written"; confirm before acting.               |
| **HANDOFF STALE**                | A handoff exists, but it was written by an _earlier_ session; the one that just ended never wrote one (abrupt kill before its first refresh). | Say so; ask the operator what changed since the handoff's timestamp before relying on any of it.    |
| **NO HANDOFF AVAILABLE**         | No session has ever written one, and a previous session existed.                                                                              | Say so explicitly — this is _not_ "nothing to hand off" — and ask the operator what it should know. |
| **NO HANDOFF: none is expected** | First coordinator session on this engine; there is no previous session.                                                                       | Nothing; not an alarm.                                                                              |
| **HANDOFF UNREADABLE**           | A value is stored but the engine could not decode it.                                                                                         | Treat as missing _and_ tell the operator the stored handoff is corrupt so it can be investigated.   |

"Missing" and "unreadable" are distinct states on purpose (`HandoffState` in `tools/boss/engine/core/src/coordinator_handoff.rs`); the engine never collapses a read failure into an empty success, and `boss handoff show` returns an error, not an empty result, for the unreadable case.

Staleness is always visible: the brief prints the absolute UTC timestamp and the elapsed time for both the handoff and the previous session's start, so "written four minutes ago by the session that just ended" and "written three days ago by some earlier session" read differently.

### What goes in it

Small and high-value. Operator-stated facts that changed the world, each with the time it was stated; decisions; open threads with ids; explicit prohibitions. Not a transcript replay, not status the session can re-read from the engine, not coordinator memory-store content. The engine enforces a 16 KiB cap and rejects blank bodies (`validate_handoff_body`).

### Transcript fallback

Prior sessions' Claude Code transcripts exist on disk under `~/.claude/projects/<encoded session dir>/*.jsonl`. When that directory exists, the brief names it as a **last resort** only: grepping transcripts is slow, unreliable, and only helps when someone already suspects the session is stale. The handoff is the proactive path; the transcript is not a substitute for it.

## CLI

```sh
# Coordinator: rewrite the handoff (stdin, heredoc-friendly).
boss handoff write - <<'HANDOFF'
## World state (operator-stated; timestamp each)
- 2026-09-02 19:22 PDT: greyarea CI host is shut down. Do not attribute new failures to it.
## Do not
- Do not file chores about greyarea disk exhaustion.
HANDOFF

# Anyone on the machine: read it back (age, writer, body).
boss handoff show
boss handoff show --json
```

`write` and `show` are coordinator-only at the worker-tier gate: a cube worker calling either gets a `CoordinatorOnly` denial.

## Forensics

Two `engine-audit.log` events (see [forensic-surfaces.md](forensic-surfaces.md)):

- `coordinator_handoff_written` — every successful write: byte count, writer spawn token, `written_at`.
- `coordinator_handoff_brief` — every fresh coordinator launch: `outcome` (`present` / `missing` / `unreadable`, or `brief_unwritable` when the file could not be written), `start_reason`, the previous session's spawn token and start time, and whether the handoff was written by that previous session.

Together they answer "was a handoff available when session X started, and who wrote it?" without opening the database.
