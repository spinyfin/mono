# Coordinator session handoff

When the coordinator session restarts, the incoming session starts with none of what the operator told the outgoing one. This document describes the handoff that closes that gap: what the outgoing session writes, where it lives, how the incoming session is briefed, and what an operator should expect to see.

## The problem it solves

The coordinator is a single long-lived Claude Code session. It is replaced by a fresh one when:

- a Claude Code update ends the `claude` process (or the operator resets the coordinator to pick the update up),
- the app or engine restarts in a way that loses the tmux session,
- the `claude` process crashes or exits,
- the engine's coordinator restart supervisor recreates it after a failure streak.

Unannounced termination gives the outgoing session no chance to run anything. Everything the operator said in it — "I've taken greyarea down", "I re-enabled tmux", "don't file chores about X" — is gone, and the next session acts on stale evidence until the operator notices and repeats themselves. The incident that motivated this: a session filed a chore against a CI host the operator had shut down 40 minutes earlier, then briefed an investigation agent on tmux state the operator had already reversed.

## Design

### What triggers the write: a rolling handoff, refreshed at boundaries

A shutdown-only write cannot protect against unannounced termination. Instead the coordinator keeps a **rolling handoff** and rewrites it at natural boundaries: whenever the operator states a fact that changes the world (a host taken down or brought back, a flag or setting flipped, dispatch paused or resumed), makes a decision, or says not to do something; when an open thread starts or resolves; and before acknowledging a request to restart, update, or reset it. Each write replaces the whole handoff, carrying forward what is still true. The "Session handoff" section of the coordinator prompt (`bossSystemPrompt` in `tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift`) is what binds the coordinator to this.

### Delivery and the grace period

The request is delivered through Claude Code's local inbox, never the composer, so any unsent text in the coordinator's composer is untouched. The production coordinator is launched with `--settings '{"crossSessionInbound":"accept"}'` so delivery does not depend on user or default inbound policy.

**Trust boundary.** `accept` is the narrowest inbound setting known to confirm delivery; it also means any Claude Code session running as the same user (a Boss worker shares `~/.claude`) can put text in front of the coordinator without approval. The engine therefore never asks the coordinator to trust the sender label or a fixed marker. Each request carries a random id, `[Boss reset request id=<id>] …`, generated from OS entropy and held by the engine only while that reset is waiting on the coordinator. The coordinator must run `boss handoff verify <id>` (coordinator-only at the worker-tier gate) and acts only if the engine reports that id pending; a forged id, a guessed id, or an id whose reset already finished is refused and the message is ignored. The coordinator prompt also forbids acting on any other peer message without the operator's confirmation. Verification is idempotent; the id stops being pending when the reset ends, whether by handoff, turn end, grace expiry, or delivery failure.

If Claude Code holds the message past the delivery deadline, Boss reports failure and has no operation to withdraw it. Because the id is withdrawn when the reset returns, a message released late fails verification and is ignored. A coordinator that ignores a request has written no handoff, so a later reset still requests a fresh one.

Coordinator sessions launched without the `--settings` flag above, or with a prompt that predates id verification, follow the user's inbound policy and do not know the verify step. A reset of such a session can fail delivery or go unobeyed; the app offers "Reset anyway (no fresh handoff)", and the replacement session launches with the current settings and prompt.

Taking the coordinator lifecycle lock is bounded too (`LOCK_WAIT_TIMEOUT`, 10 s). If a competing lifecycle operation (supervisor restart, app-registration attach) holds it longer, the reset fails with the same `coordinator did not receive the handoff request` error and a `coordinator_handoff_lock_wait_timeout` audit event instead of stalling silently.

Delivery has its own bound (`DELIVERY_TIMEOUT`, 15 s) that does not consume the grace period. A `held` receipt is pending, not failure: the wait ends on a correlated transcript record or `delivered` receipt, or fails on an explicit `denied`/`expired`/`refused`/`dropped` receipt or the deadline. Stray or malformed receipt-socket traffic is ignored. If delivery fails, the session is left running and the error begins `coordinator did not receive the handoff request`; the app keeps the reset UI open and offers "Reset anyway (no fresh handoff)".

After delivery, the 30 s grace period ends at the first of: a fresh handoff write, the end of the turn the request started (an assistant `end_turn` record descends from the injected request UUID; if no write landed, the missing-handoff warning is shown), or the deadline.

Each persisted handoff has a unique `write_id`, so an earlier write cannot satisfy the wait even when its body and second-resolution timestamp match. A fresh write wakes reset immediately. If grace expires without one, Boss logs an error, displays the missing-context warning, replaces the session, and labels an older saved handoff STALE.

The transcript cursor is captured before sending the frame, so completion before delivery confirmation or between reads is retained. The lifecycle lock protects validation and frame send, then is released before awaiting confirmation. Recreation rechecks the spawn token under the lock. Dead or missing sessions skip delivery.

The handoff is written by the coordinator session itself. The engine never synthesizes one from logs or transcripts.

### Where it lives

One JSON value in the `metadata` table of the engine's own database, under the key `coordinator.handoff`:

```json
{ "body": "...", "written_at": 1756800000, "writer_spawn_token": "…", "write_id": "…" }
```

`written_at` is Unix epoch seconds; `writer_spawn_token` is the spawn token of the coordinator record that was live when the write happened. This is coordinator-private state — it never enters a repo — and it is engine-owned, so it survives app restarts and is reachable only through the engine's RPC surface (`boss handoff …`), never by reading the database.

Alongside it, the coordinator record now stamps `coordinator.tmux_spawned_at` (when the current session started). That is what lets the brief say whether the session that just ended wrote anything after it started.

### How the incoming session consumes it

Every _fresh_ coordinator session is launched with a **session-start brief** as `claude`'s positional initial prompt — the same mechanism worker panes use for their initial prompt. The engine composes the brief in `start_new` (`tools/boss/engine/core/src/coordinator_tmux.rs`, `prepare_session_start_brief`), writes it to `.claude/handoff-brief.txt` in the coordinator session directory, and launches:

```
exec claude --model <model> --permission-mode auto --settings '{"crossSessionInbound":"accept"}' "$(cat '<session dir>/.claude/handoff-brief.txt')"
```

So the incoming session reads the handoff on its very first turn, before the operator types anything, with no pane injection to time and no dependence on the model choosing to open a file. If the brief file cannot be written, the engine launches with a short inline notice instead (naming the error and pointing at `boss handoff show`), never a bare session.

The brief also carries, after the handoff, a "Product coordinator guidance" section: every non-archived product's `BOSS_COORDINATOR.md`, read from GitHub at the default branch's HEAD and shown with the commit sha it was read at. That is a separate mechanism with its own states and audit event; see [coordinator-product-guidance.md](coordinator-product-guidance.md).

An _adopted_ session — the engine restarted but the coordinator's tmux session and `claude` process survived — keeps its own context and receives no brief. That path is the existing prompt-change nudge (`maybe_nudge_prompt_change` in the same file), which re-reads the rendered `CLAUDE.md` when its content changed across the restart. The two mechanisms are complementary and share the coordinator lifecycle code; neither replaces the other.

The brief also carries **why** the previous session ended (tmux session missing, `claude` process exited, operator reset, model change, forced reset without a fresh handoff, or first creation), rendered from `CoordinatorStartReason`.

### The states the brief reports

The brief opens with exactly one of these, and the incoming session is instructed to state which applies in its first reply:

| State                            | Meaning                                                                                                                                                                                                                                                                  | What the session is told to do                                                                      |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------- |
| **HANDOFF PRESENT**              | The session that just ended wrote it (its spawn token matches the writer), written _N_ ago.                                                                                                                                                                              | Summarize it; treat every fact as "as of when it was written"; confirm before acting.               |
| **HANDOFF STALE**                | A handoff exists, but it was written by an _earlier_ session; the one that just ended never wrote one (abrupt kill before its first refresh). Also used when the reset was forced without waiting for a fresh handoff, even if the outgoing session wrote the saved one. | Say so; ask the operator what changed since the handoff's timestamp before relying on any of it.    |
| **NO HANDOFF AVAILABLE**         | No session has ever written one, and a previous session existed.                                                                                                                                                                                                         | Say so explicitly — this is _not_ "nothing to hand off" — and ask the operator what it should know. |
| **NO HANDOFF: none is expected** | First coordinator session on this engine; there is no previous session.                                                                                                                                                                                                  | Nothing; not an alarm.                                                                              |
| **HANDOFF UNREADABLE**           | A value is stored but the engine could not decode it.                                                                                                                                                                                                                    | Treat as missing _and_ tell the operator the stored handoff is corrupt so it can be investigated.   |

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

# Coordinator: read it back (age, writer, body).
boss handoff show
boss handoff show --json
```

`write` and `show` are coordinator-only at the worker-tier gate: a cube worker calling either gets a `CoordinatorOnly` denial.

## Forensics

The handoff's durable `engine-audit.log` events (see [forensic-surfaces.md](forensic-surfaces.md)) include:

- `coordinator_handoff_written` — every successful write: byte count, writer spawn token, `written_at`.
- `coordinator_handoff_brief` — every fresh coordinator launch: `outcome` (`present` / `missing` / `unreadable`, or `brief_unwritable` when the file could not be written), `start_reason`, the previous session's spawn token and start time, and whether the handoff was written by that previous session.

Together they answer "was a handoff available when session X started, and who wrote it?" without opening the database.

Reset timing is also recorded at each boundary. The app's IPC JSONL records `coordinator_reset_gesture` (Settings or update banner), `coordinator_reset_confirmed`, and `coordinator_reset_rpc_send`. These carry millisecond wall-clock timestamps. Engine audit records `coordinator_reset_rpc_received`, `coordinator_reset_handler_started`, `coordinator_handoff_delivery_started`, `coordinator_handoff_lock_acquired`, `coordinator_handoff_requested` (confirmed delivery), `coordinator_handoff_grace_started`, and `coordinator_handoff_received` or `coordinator_handoff_timeout` (grace end), with millisecond timestamps and elapsed delivery/lock timing. Join the app's RPC send and engine receipt by request ID, then the handoff events by spawn token. Draft text and inbox authentication data are never logged.

To locate where time went in a slow reset, compare the timestamps in order: `coordinator_reset_rpc_send` → `coordinator_reset_rpc_received` → `coordinator_handoff_lock_acquired` → `coordinator_handoff_requested` → `coordinator_handoff_grace_started`. A long gap before `rpc_send` is app or dialog time; `rpc_send` to `rpc_received` is the RPC path; `delivery_started` to `lock_acquired` is `coordinator_tmux` lock contention; `lock_acquired` to `requested` is inbox delivery; `grace_started` to `received`, `timeout` or `turn_ended_without_write` is time spent waiting on the coordinator itself. Both reset UI entry points share the same RPC path, and the grace timer starts only after delivery is confirmed.

Inbox delivery also records `coordinator_inbox_delivery_stage` for registration read, connection, frame write, lifecycle lock release, and transcript or receipt confirmation. Each includes an elapsed duration and wall-clock timestamp. These measurements distinguish a slow transport from waiting for Claude to accept input.

### Manual verification targets

`coordinator_inbox_live_test` and `coordinator_inbox_real_model_test` are manual targets, not part of the wildcard suite. The real-model check needs an approved credential fixture because the hermetic test wrapper strips `ANTHROPIC_API_KEY`.
