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

The request is delivered through Claude Code's local inbox, never the composer, so any unsent text in the coordinator's composer is untouched. The production coordinator is launched with `--settings '{"crossSessionInbound":"accept"}'` so delivery does not depend on user or default inbound policy. The message begins with `[Boss reset request expires=<epoch seconds>]`, and the coordinator prompt tells the coordinator to trust it even though Claude labels it as coming from another session. If Claude Code holds the message past the delivery deadline, Boss reports failure and has no operation to withdraw it; the expiry lets the coordinator ignore a request released late, when no reset is pending. A coordinator that ignores an expired request has written no handoff, so a later reset still requests a fresh one.

**Sessions launched before this change** have neither `--settings` nor the marker guidance in their prompt, so they follow the user's inbound policy and may not trust the request. The first reset after an upgrade can therefore fail delivery or go unobeyed; the app offers "Reset anyway (no fresh handoff)", and the replacement session launches with the new settings and guidance.

Delivery has its own bound (`DELIVERY_TIMEOUT`, 15 s) that does not consume the grace period. A `held` receipt is pending, not failure: the wait ends on a correlated transcript record or `delivered` receipt, or fails on an explicit `denied`/`expired`/`refused`/`dropped` receipt or the deadline. Stray or malformed receipt-socket traffic is ignored. If delivery fails, the session is left running and the error begins `coordinator did not receive the handoff request`; the app keeps the reset UI open and offers "Reset anyway (no fresh handoff)".

The text after that prefix names the step that failed and why, and the app shows it verbatim under the banner (`coordinatorResetDeliveryFailureReason` in `ChatViewModel+BossSession.swift`). The two steps are the credential (`the handoff credential for this session could not be minted (…)`) and inbox delivery (`inbox delivery failed: …`). A deadline expiry during inbox delivery names the furthest stage reached: before the Claude inbox registration could be read, while connecting to the inbox socket, while writing the request, or after the request was written but the coordinator did not accept it as input (it is mid-turn, or its inbound policy is holding cross-session messages). The audit event `coordinator_handoff_delivery_failed` carries the same `step` and `error`. A generic "could not deliver" with no reason is never shown for an engine-reported failure.

After delivery, the 30 s grace period ends at the first of: a fresh handoff write, the end of the turn the request started (an assistant `end_turn` record descends from the injected request UUID; if no write landed, the missing-handoff warning is shown), or the deadline.

Each persisted handoff has a unique `write_id`, so an earlier write cannot satisfy the wait even when its body and second-resolution timestamp match. A fresh write wakes reset immediately. If grace expires without one, Boss logs an error, displays the missing-context warning, replaces the session, and labels an older saved handoff STALE.

The transcript cursor is captured before sending the frame, so completion before delivery confirmation or between reads is retained. The lifecycle lock protects validation and frame send, then is released before awaiting confirmation. Recreation rechecks the spawn token under the lock. Dead or missing sessions skip delivery.

The handoff is written by the coordinator session itself. The engine never synthesizes one from logs or transcripts.

### Write authorization

Writes require both socket-peer ancestry to the engine-registered coordinator PID and a separate random session credential. The engine creates the credential atomically with the spawn intent, rotates it on replacement, and delivers it in the private initial input, kept outside the session working directory in the engine-owned sibling input home (file mode `0600`). The archived `.claude/handoff-brief.txt` contains no credential. Reset requests include the current credential; after compaction, `boss handoff show` from coordinator ancestry requests asynchronous re-delivery to the main session inbox, never to the CLI response. It is not exported in the environment, included in shared `CLAUDE.md`, or returned by `handoff show`. The CLI supplies it explicitly with `--authorization`; a child or subagent that merely inherits `BOSS_SPAWN_TOKEN` cannot write. Registered workers and progression agents are denied even with the correct credential, independently of the `worker_rpc_tier` flag. Worker command guards deny the entire `boss handoff` family.

The new `coordinator_handoff_rejected` event records the caller PID, bounded parent chain and best-effort argv (the real credential is redacted by substring; a caller-chosen `--authorization` value is redacted only in place). Missing identity or credentials fail closed. A reduction exceeding 80% emits `coordinator_handoff_shrink_warning` with old and new byte counts; a valid write still succeeds.

Enforcement takes effect on engine deployment. A surviving coordinator launched before authorization existed has no credential row, and a session adopted after an engine restart was briefed by an earlier engine. The engine therefore tracks, per spawn token, whether the live session has received its credential (`coordinator.handoff_authorization_delivered_to`, set when a fresh session's brief carries it and when an inbox delivery of it is confirmed, including the credential inside a confirmed reset request). Three paths converge on that marker:

- **Adoption.** When app registration re-associates a surviving session (`attach_coordinator_to_registered_app` → `brief_adopted_coordinator_with_authorization` in `tools/boss/engine/core/src/app/sessions.rs`), a session without the marker is minted a credential under `coordinator_tmux_lock` and sent a `[Boss handoff authorization]` message through its inbox, detached from the attach flow. A later re-adoption of the same session does not resend it.
- **`boss handoff show`** from coordinator ancestry requests the same delivery on demand and reports the outcome in its response (`authorization_delivery` on `CoordinatorHandoffResult`; the credential itself is never in the response): `requested: true` once the main session's inbox accepted the message, or `requested: false` with the failing step and reason. The CLI prints this as its last line. Acceptance is reported as soon as the frame is written: a coordinator that runs `show` mid-turn only reads its inbox when that turn ends, so the engine confirms in the background under `AUTHORIZATION_CONFIRMATION_TIMEOUT` (15 min) and records the marker on confirmation. Every outcome is audited as `coordinator_handoff_authorization_delivery` with `trigger` (`adoption` or `handoff_show`) and `outcome` (`accepted`, `confirmed`, `failed`, `unconfirmed`). While a delivery awaits confirmation, a repeat request reports it as pending rather than queueing the credential twice.
- **A reset request** mints the same way, carries the credential, and records the marker once the request is confirmed delivered, so the outgoing session can write a fresh handoff.

Writes still require ancestry. New sessions receive a credential automatically at spawn. This mechanism prevents ambient environment inheritance from granting write access; it is not an OS sandbox against a same-user process deliberately stealing the private brief or a credential explicitly delegated by the coordinator.

#### The 1.0.823 incident

A reset of a coordinator that had outlived the authorization deploy failed with the generic banner. The engine's error was `coordinator did not receive the handoff request: coordinator handoff authorization is unavailable`: the surviving session had no credential row, so the reset failed before any inbox message was sent, and the app dropped the reason. `boss handoff show` printed the stored handoff while its asynchronous redelivery failed on the same missing row and was only logged, so the session never learned why no credential arrived and could not write at all. The minting above (first landed in 1.0.827), the adoption delivery, the reported `show` outcome and the step-naming reset error each close one part of that failure.

#### Diagnosis

Before write authorization, the handler stamped every accepted write with the live coordinator record's spawn token. That token is issued in `start_new` (`tools/boss/engine/core/src/coordinator_tmux.rs`: `generate_token` at line 993, `record_coordinator_tmux_spawn_intent` at line 995) and exported into the tmux session environment as `BOSS_SPAWN_TOKEN` (same file, line 1033). It is read from the database, not from the caller, by `current_spawn_token` (`tools/boss/engine/core/src/app/coordinator_handoff.rs` lines 20–25). Nothing compared a caller-presented secret to it. The only gate on the frontend socket was `worker_tier_denial` (`tools/boss/engine/core/src/app/trust.rs` lines 272–280) and the related row-scope check (same file, lines 322–328). Both are inert for ordinary workers while `worker_rpc_tier` is off, and that flag is `default_enabled: false` (`tools/boss/engine/feature-flags/src/lib.rs` lines 405–421).

Writer classes that could therefore land a write, and the check that now closes each one:

- **A cube worker with `worker_rpc_tier` off (the default).** Admitted at `RpcTier::User` and stamped with the live coordinator's spawn token. Closed by the handler's `classify_peer(...).is_agent()` rejection (`coordinator_peer` at `app/coordinator_handoff.rs` lines 27–29, and the Get path at line 44), which runs whether or not the flag is on. When the flag is on, the connection gate also refuses the verb first with `WorkerTierDenied` / `CoordinatorOnly`.
- **A coordinator subagent or child that only inherited `BOSS_SPAWN_TOKEN`.** Ancestry still matches the coordinator pane, so the peer check alone would admit it. Closed by the per-spawn credential minted in `record_coordinator_tmux_spawn_intent` (`tools/boss/engine/work-db/src/coordinator_tmux.rs`; the token comes from the injectable `WorkDb` token generator, which defaults to `generate_token` in `tools/boss/engine/utils/src/token.rs`), never exported to tmux or the environment, and compared in constant time (`handle_set_coordinator_handoff` at `app/coordinator_handoff.rs` lines 136–141). `--authorization` must be supplied explicitly.
- **Any other same-user process, or a test-engine coordinator, hitting the production socket.** Closed by `coordinator_peer`: the socket peer must be a descendant of the engine-refreshed production coordinator pane pid. A foreign engine's coordinator pane is a different pid.

Identical `writer_spawn_token` values in the old audit events therefore do **not** establish that the caller inherited that token or belonged to the coordinator's process tree. Those two events cannot identify which of the classes above authored the October 2026 stub.

### Stored record

One JSON value in the `metadata` table of the engine's own database, under the key `coordinator.handoff`:

```json
{ "body": "...", "written_at": 1756800000, "writer_spawn_token": "…", "write_id": "…" }
```

`written_at` is Unix epoch seconds; `writer_spawn_token` is the spawn token of the coordinator record that was live when the write happened. This is coordinator-private state — it never enters a repo — and it is engine-owned, so it survives app restarts and is reachable only through the engine's RPC surface (`boss handoff …`), never by reading the database.

Alongside it, the coordinator record now stamps `coordinator.tmux_spawned_at` (when the current session started). That is what lets the brief say whether the session that just ended wrote anything after it started.

### How the incoming session consumes it

Every _fresh_ coordinator session receives a **session-start brief** through the terminal relay — the same mechanism worker panes use for their initial prompt. The engine composes the brief in `start_new` (`tools/boss/engine/core/src/coordinator_tmux.rs`, `prepare_session_start_brief`), archives its non-secret content at `.claude/handoff-brief.txt`, and provisions the full input including authorization in the engine-owned sibling input home outside the coordinator's working directory. The relay reads that private input and delivers it on the first turn; neither the credential nor the handoff appears in launch arguments or environment variables.

If the archive cannot be written, the relay delivers a recovery notice pointing at `boss handoff show` plus the current authorization. An authorization lookup failure is explicitly logged and reported in the delivered notice. The engine refreshes its coordinator PID trust root during supervision even without app registration, and clears stale identity when the PID lookup fails.

The brief also carries, after the handoff, a "Product coordinator guidance" section: every non-archived product's `BOSS_COORDINATOR.md`, read from GitHub at the default branch's HEAD and shown with the commit sha it was read at. That is a separate mechanism with its own states and audit event; see [coordinator-product-guidance.md](coordinator-product-guidance.md).

An _adopted_ session — the engine restarted but the coordinator's tmux session and `claude` process survived — keeps its own context and receives no brief. That path is the existing prompt-change nudge (`maybe_nudge_prompt_change` in the same file), which re-reads the rendered `CLAUDE.md` when its content changed across the restart, plus the handoff credential delivery described under "Write authorization" when the session has not yet received one. The mechanisms are complementary and share the coordinator lifecycle code; none replaces another.

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
boss handoff write --authorization '<credential from this session-start brief>' - <<'HANDOFF'
## World state (operator-stated; timestamp each)
- 2026-09-02 19:22 PDT: greyarea CI host is shut down. Do not attribute new failures to it.
## Do not
- Do not file chores about greyarea disk exhaustion.
HANDOFF

# Coordinator: read it back (age, writer, body). From the coordinator's own
# session this also requests redelivery of the write credential to the main
# session inbox and reports, on its last line, whether that was accepted or
# which step failed.
boss handoff show
boss handoff show --json
```

`write` and `show` are coordinator-only. When `worker_rpc_tier` is enabled, a cube worker calling either is refused at the connection gate with a typed `WorkerTierDenied` (`CoordinatorOnly`). With the flag at its default (off), that gate is inert and the worker is admitted at `RpcTier::User`; the handler then rejects the call as an agent (`handoff write rejected` / `coordinator handoff is unavailable to agents`). Other non-agent peers, including a human terminal, may read without coordinator ancestry; writes still require both ancestry and authorization. Omitting `--authorization` reaches the engine and produces a rejection audit event.

## Forensics

The handoff's durable `engine-audit.log` events (see [forensic-surfaces.md](forensic-surfaces.md)) include:

- `coordinator_handoff_written` — every successful write: byte count, writer spawn token, `written_at`.
- `coordinator_handoff_brief` — every fresh coordinator launch: `outcome` (`present` / `missing` / `unreadable`, or `brief_unwritable` when the file could not be written), `start_reason`, the previous session's spawn token and start time, and whether the handoff was written by that previous session.

Together they answer "was a handoff available when session X started, and who wrote it?" without opening the database.

Reset timing is also recorded at each boundary. The app's IPC JSONL records `coordinator_reset_gesture` (Settings or update banner), `coordinator_reset_confirmed`, and `coordinator_reset_rpc_send`. These carry millisecond wall-clock timestamps. Engine audit records `coordinator_reset_rpc_received`, `coordinator_reset_handler_started`, `coordinator_handoff_delivery_started`, `coordinator_handoff_lock_acquired`, `coordinator_handoff_requested` (confirmed delivery), `coordinator_handoff_grace_started`, and `coordinator_handoff_received` or `coordinator_handoff_timeout` (grace end), with millisecond timestamps and elapsed delivery/lock timing. Join the app's RPC send and engine receipt by request ID, then the handoff events by spawn token. Draft text and inbox authentication data are never logged.

To locate where time went in a slow reset, compare the timestamps in order: `coordinator_reset_rpc_send` → `coordinator_reset_rpc_received` → `coordinator_handoff_lock_acquired` → `coordinator_handoff_requested` → `coordinator_handoff_grace_started`. A long gap before `rpc_send` is app or dialog time; `rpc_send` to `rpc_received` is the RPC path; `delivery_started` to `lock_acquired` is `coordinator_tmux` lock contention; `lock_acquired` to `requested` is inbox delivery; `grace_started` to `received`, `timeout` or `turn_ended_without_write` is time spent waiting on the coordinator itself. Both reset UI entry points share the same RPC path, and the grace timer starts only after delivery is confirmed.

Inbox delivery also records `coordinator_inbox_delivery_stage` for registration read, connection, frame write, lifecycle lock release, and transcript or receipt confirmation. Each includes an elapsed duration and wall-clock timestamp. These measurements distinguish a slow transport from waiting for Claude to accept input, and the furthest stage reached is what a `coordinator_handoff_delivery_failed` deadline error names. Credential deliveries (adoption and `boss handoff show`) record `coordinator_handoff_authorization_delivery` with their trigger and outcome.

### Verification and evidence limits

The native concurrent compatibility run on Claude Code 2.1.300 confirmed inputs in 152–157 ms: connection and frame write took 0–1 ms, and confirmation followed the existing 150 ms macOS transport delay. Both idle delivery assertions remain one second. The terminal case uses the production launch helper, loads a user setting of `crossSessionInbound: refuse`, confirms the command-line `accept` override, preserves the draft, and observes the request-correlated assistant completion.

These measurements do not establish the cause of the historical delay before the handoff request. No incident trace spanning the Settings or update-banner confirmation through lock acquisition was supplied. The full isolated headless-engine reproduction of both UI entry points, including supervisor/attach contention, remains unverified; the new transport measurements must not be presented as that reproduction. Lock contention is a candidate, not an established historical cause. Delivery confirmation no longer holds the lifecycle lock, and a regression test withholds confirmation until a second lifecycle operation acquires that lock.

The opt-in `coordinator_inbox_real_model_test` exercises a real model with a private configuration and a local `boss handoff write` sink. It is not scheduled by the wildcard suite. Its attempted run failed because the mandatory hermetic test wrapper strips `ANTHROPIC_API_KEY`, including when supplied with `--test_env`. Real-model obedience therefore remains unverified. Running this check requires a repository-approved credential fixture; passing the environment variable alone is insufficient, and bypassing the wrapper is not supported.
