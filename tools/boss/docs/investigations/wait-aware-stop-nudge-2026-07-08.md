# Design: Wait-aware Stop handling — let workers block on background subagents without tripping the produce-a-PR nudge

**Date:** 2026-07-08
**Status:** Investigation + design. **No code changes** — the deliverable is this doc. Concrete follow-up code tasks are listed in §10 for the operator to file separately.
**Lineage:** produce-PR nudge-loop diagnostics (T740 era, the "Worf" loop `exec_18b3945c5b7d7e78_1b`), the sanctioned no-op marker (`NO_CHANGES_NEEDED`, T1868), the worker-escalation markers (`[effort-escalation]` / `[blocked]`, T2085), and the automation-triage decision-marker work (T2348). This doc extends that same marker/nudge-suppression machinery to a new failure mode.

## TL;DR

A worker that spawns a background subagent (the Claude Code `Agent`/`Task` tool) and **deliberately stops its turn to wait** for the subagent's completion is doing the correct, token-efficient thing: the harness re-invokes the session when the tracked background work finishes. But the engine's Stop-boundary handler cannot tell that Stop apart from "worker finished, no PR" — so it fires the `PROBE_NO_PR` nudge (`completion.rs:1656-1663`), which is injected into the pane _as if the human typed it_ (`app/worker_events.rs:567-591`). That injection pre-empts the wait: in the observed incident the worker abandoned its pending subagent, re-did the investigation inline (duplicated work, wasted tokens), then spawned yet another agent.

**Root cause:** when a worker Stops, the engine decides finalize/nudge/park purely from PR/branch state + execution kind + final-message markers. It has **no signal that a harness-tracked background agent is still outstanding** (`WorkerEvent` has 7 variants, none of them `SubagentStop`; `driver/claude.rs:107-113`).

**Authoritative hook facts** (verified against `code.claude.com/docs/en/hooks.md`, 2026-07-08):

- The `Stop` payload carries `last_assistant_message` (the final assistant text of the turn) plus the common fields (`session_id`, `transcript_path`, `cwd`, `permission_mode`, …). **There is no documented field that signals pending background/async work at the main Stop.** (An earlier research pass floated a `background_tasks` array; it is **not** in the official reference. It is not rejected outright — see §3.2 and §9 for the empirical work that would be needed to earn trust in it as an opportunistic corroborator — but it must not be a sole or primary signal today.)
- `SubagentStop` **is** a documented, stable hook that fires "when a subagent finishes," carrying `agent_id` and `agent_type`. Boss does **not** currently wire it.
- The transcript JSONL schema is documented as "internal and subject to change," so transcript-shape inspection is explicitly unsupported.

**Recommendation (§7):** a **hybrid**. Primary signal is a worker-emitted **`[waiting]` marker**, parsed from `last_assistant_message` in the same family as `[effort-escalation]`/`[blocked]` but with _self-clearing, no-human-attention_ semantics. Backstop signal is **engine-side SubagentStop-vs-spawn counting** (wire the documented `SubagentStop` hook; the engine already sees the spawn via `PreToolUse{tool_name:"Task"}`). Suppression is **bounded** — by the subagent actually returning, by an absolute waiting cap analogous to `ABSOLUTE_MAX_NUDGES`, and by the existing `stale_worker_sweep` as the ultimate hang backstop — so hang detection does not regress.

---

## 1. The incident

Operator report, 2026-07-07 (verbatim worker transcript excerpt):

> I'll pause here and wait for the research agent's findings before proceeding.
>
> ✻ Waiting for 1 background agent to finish
>
> ❯ You stopped without producing a PR for this work. If the work is complete, open the PR with `cube pr create --branch <bookmark>` … If you're blocked, explain what you need.
>
> The wait mechanism isn't giving me a notification promptly, so I'll investigate directly instead of blocking on the background agent.

The worker stopped its main turn (idle, "Waiting for 1 background agent"). The engine saw a `Stop`, found no PR on the branch, and injected `PROBE_NO_PR`. The nudge arrived as the next user prompt, the worker read it as "you should be producing a PR, not idling," abandoned the pending subagent, and redid the work inline.

**Two distinct harms:**

1. **The observed one — wasted work.** The worker discards a running subagent and re-does it inline. Pure token/latency waste, and it defeats the entire point of fanning work out to a subagent.
2. **The more dangerous cousin — already partially mitigated by prompt.** A worker that opens the _terminal_ PR while background work is still in flight loses the background result entirely, because PR creation reaps the worker. This is exactly the incident behind `pr_terminal_directive` (`runner.rs:1720-1735`): "a worker opened a PR, then tried to wait for background review subagents … The engine terminated the worker the moment the PR was created, so the review was never consumed." The nudge can _push a waiting worker toward_ this failure by telling it to open the PR now.

---

## 2. How the Stop → nudge path works today (grounding)

### 2.1 Hook plumbing

Every worker's Claude settings wire **seven** lifecycle hooks — `SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop, Notification, SessionEnd` (`driver/claude.rs:107-113`) — to the `boss-event` shim (`event-shim/src/main.rs`), which forwards the raw hook JSON over the engine's Unix events socket, splicing in `_boss_run_id` (`event-shim/src/main.rs:180-191`). `normalize_hook_event` turns each payload into a typed `WorkerEvent` (`protocol/src/worker_event.rs:84-135`).

The `Stop` variant carries almost nothing:

```rust
// protocol/src/worker_event.rs:34-38
Stop {
    session_id: String,
    stop_hook_active: bool,
    stop_reason: StopReason,   // always normalized to Completed; a sequencer may overwrite
}
```

- `transcript_path` rides the **envelope** (`IncomingHookEvent`, `events_socket.rs:52-57`), not the `Stop` variant, and is persisted onto `WorkRun` the first time it is seen. So the engine already _has_ the transcript path at every Stop.
- **No `SubagentStop`.** `WorkerEvent` has exactly the seven variants above; `normalize_hook_event` returns `UnknownEvent` for anything else (`worker_event.rs:133`). The engine is structurally blind to subagent lifecycle.
- `last_assistant_message` is present in the raw Stop payload (per the hook docs) but is **not currently captured** by `normalize_hook_event` — today the engine reconstructs the final message by tailing the transcript instead (`read_final_triage_message`, `completion.rs:2874-2930`).

### 2.2 The nudge decision

`dispatch_completion_on_stop` (`app/worker_events.rs:946-963`) routes a `Stop` into `WorkerCompletionHandler::on_stop` → `on_stop_inner` (`completion.rs:1095`/`1121`). After the staged-PR fast path and `detect_pr`, the no-PR branch (`PrStatus::None | Closed`, `completion.rs:1558`) walks a series of guards — bound sibling PR? `ci_remediation`/`revision` with no PR → park? `NO_CHANGES_NEEDED` marker → no-op success? — and, only if all decline, fires the nudge:

```rust
// completion.rs:1656-1663
return self
    .nudge_or_park(&execution, PROBE_NO_PR, "no_pr", None, StopOutcome::AwaitingInput)
    .await;
```

`nudge_or_park` (`completion.rs:4047`) is the **single choke point** for every auto-nudge. It (a) refuses to nudge and returns `EscalationPending` if the worker has an unresolved `[effort-escalation]`/`[blocked]` attention item (`completion.rs:4055-4071`), else (b) records intent against the `NudgeBreaker` and either queues the probe or parks:

```rust
// completion.rs:4072-4090
match self.nudge_breaker.record(&execution.id, fingerprint, self.max_unproductive_nudges) {
    NudgeDecision::Proceed { count } => { … self.probe_queuer.queue_probe(&execution.id, probe_text); proceed_outcome }
    NudgeDecision::Trip  { count } => { self.park_for_unproductive_nudges(…).await }
}
```

The breaker (`nudge_breaker.rs`) caps `DEFAULT_MAX_UNPRODUCTIVE_NUDGES = 3` consecutive _same-fingerprint_ nudges and `ABSOLUTE_MAX_NUDGES = 12` cross-fingerprint total; the fingerprint for the no-PR case is the constant `"no_pr"`, so a repeatedly-waiting worker never changes fingerprint and is parked after 3 nudges. **Parking is itself a bad outcome here** — it files a `nudge_breaker_tripped` attention item and releases the lease/slot on a worker that was healthy and mid-wait.

The queued probe is delivered by `dispatch_probe_on_stop` (`app/worker_events.rs:567-591`) via `SendToPane` — literally typed into the worker's tmux pane, so "claude treats it as the next user prompt." That is why the nudge derails the wait.

### 2.3 Existing crude mitigations (and why they're insufficient)

- **`pr_terminal_directive`** (`runner.rs:1720-1735`): a _prompt_ telling the worker not to open the PR while background work is in flight. Prevents harm #2 by convention, does nothing about the nudge that provokes it.
- **`automation_triage` forbids the Agent tool** (`automation_triage.rs:90-93`): "Do NOT use the `Agent` tool. Spawning a sub-agent provides no resume mechanism — the session will hang waiting for a result that never returns." This is the **blunt current workaround** — ban the capability. Note the apparent contradiction with the incident (where the worker _was_ re-invoked): it is resolved by execution kind. A **triage** run is single-shot — its completion handler finalizes on the first Stop by parsing the decision marker, so the run is reaped before any subagent could return. A **primary-implementation** worker (like the one in §1) is _not_ reaped on a bare Stop — it is nudged — so its harness auto-resume _does_ fire, and the nudge is what interferes. The fix in this doc is for the implementation-worker case; triage's ban remains appropriate for single-shot runs.
- **`stale_worker_sweep`** (`stale_worker_sweep.rs:297-310`): reaps a worker after `stale_threshold_secs` with no hook events, "presumed wedged on a backgrounded/idle wait." This is the genuine hang backstop — but it cannot, on its own, tell a healthy-but-slow wait from a wedge (see §4.3).

---

## 3. Question 1 — Detection: what signal distinguishes "waiting, will be re-invoked" from "done/blocked"?

### 3.1 What the Stop boundary actually carries

Per the official hooks reference (`code.claude.com/docs/en/hooks.md`, fetched 2026-07-08):

| Field                                                     | On `Stop`?                          | Notes                                                                                                                                     |
| --------------------------------------------------------- | ----------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| `last_assistant_message`                                  | ✓ documented                        | The final assistant text of the turn. Docs recommend using it _instead of_ reading the transcript.                                        |
| `session_id`, `transcript_path`, `cwd`, `permission_mode` | ✓ common fields                     | Engine already extracts `transcript_path`.                                                                                                |
| `stop_hook_active`                                        | present in payload, engine reads it | Means "this Stop hook is itself running inside a stop-finalization loop" — an anti-infinite-loop guard, **not** a background-work signal. |
| `background_tasks`                                        | ✗ **not documented**                | Floated by an earlier research pass from third-party blogs; **not** in the official reference. Do not build on it.                        |
| pending-background indicator                              | ✗ **none documented**               | No field, on any hook, says "N background agents still running."                                                                          |

Separately, `SubagentStop` is a **documented, stable** hook: it fires "when a subagent finishes" and carries `agent_id` + `agent_type`; the docs note "for subagents, `Stop` hooks are automatically converted to `SubagentStop`." Boss does not wire it today.

### 3.2 Candidate signals, ranked by robustness

| Candidate                                     | How                                                                                                                                                                   | Robustness                                                                                                                                                                                  | Verdict                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| --------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **`background_tasks` payload field**          | read array from Stop payload                                                                                                                                          | Undocumented; may not exist; schema unstable                                                                                                                                                | **Opportunistic corroborator, not sole gate.** Undocumented ⇒ no version contract, so it cannot be the load-bearing signal on its own. But "undocumented" bundles three distinct risks — presence, semantics/schema, and version contract — and only the last is actually hard to retire: Boss pins the CC version its workers run, so the risk only materializes on a deliberate upgrade, which can be gated on re-validation. Safe to gate on it as a fast-path once (a) shadow-compared against the `outstanding_subagents` counter from §3.2's SubagentStop-vs-spawn signal across real Stops, and (b) guarded by a shape/presence canary that pages on drift — see §8 and §9 item 5 for what that validation looks like concretely. Until both are in place, log only. |
| **`last_assistant_message` NL heuristic**     | classify "I'll wait for the agent…"                                                                                                                                   | Fragile NL matching; the incident worker's phrasing varies                                                                                                                                  | **Reject** — same class of fragility the marker family exists to avoid.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| **Transcript unmatched `tool_use`**           | tail JSONL (engine already has `TranscriptTail`, `transcript-tail/src/lib.rs`), find a `Task`/`Agent` `tool_use` with no terminal `tool_result`                       | Feasible with existing machinery, but schema is officially "internal, subject to change"; and a _background_ Task returns a "launched" placeholder immediately, so "unmatched" is ambiguous | **Fallback only.** Fragile; use only if the two better signals are unavailable.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| **SubagentStop-vs-spawn count** (engine-side) | count `PreToolUse{tool_name∈{Task,Agent}}` (already received) minus `SubagentStop` (needs wiring) per run; `>0` at a main Stop ⇒ a background subagent is outstanding | Documented + stable hooks; automatic; no worker cooperation needed                                                                                                                          | **Recommended backstop.** One new wired hook.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| **Worker-side `[waiting]` marker**            | worker emits `[waiting] reason="…"` on its own line; rides to the engine in `last_assistant_message`                                                                  | Fully Boss-owned; precise (the worker _knows_ it just spawned auto-resuming work); matches existing marker precedent; zero dependence on undocumented internals                             | **Recommended primary.** Needs a worker-preamble instruction + parser.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |

**Why the count discriminates cleanly:** a _foreground_ Agent call resolves within the turn — its `SubagentStop` fires _before_ the main `Stop`, so the count nets to 0 by Stop time. A _background_ spawn leaves the count at ≥1 at the main Stop. So `outstanding_subagents > 0` at a main `Stop` is a precise "waiting on auto-resuming work" signal.

### 3.3 Conclusion

There is **no free engine-side field** that answers the question. The two robust options are (a) the worker asserting intent via a marker and (b) the engine reconstructing outstanding-subagent state from the documented hook stream. They are complementary — one covers worker non-compliance, the other covers "the worker told us exactly what it's waiting on and why." The design uses both.

---

## 4. Question 2 — Engine behavior when waiting is detected

### 4.1 Suppress the nudge, cheaply and passively

When waiting is detected at a Stop, `nudge_or_park` should return **before** touching the circuit breaker (mirroring the existing `EscalationPending` early-out at `completion.rs:4055`), with a **new** outcome:

```rust
StopOutcome::WaitingOnBackgroundWork { reason: String }   // new variant
```

Semantics deliberately different from `[blocked]`/`[effort-escalation]`:

- **No human attention item.** Waiting is transient and self-resolving; it is not a cry for help. Filing a `work_attention_items` row (as `EscalationPending` does) would spam the coordinator UI.
- **Does not consume breaker budget.** The `"no_pr"` fingerprint count is left untouched, so a worker that legitimately waits, resumes, and _then_ genuinely idles still gets its full nudge budget.
- **Emit a passive live-status** instead — a new live-state reason `worker_waiting_on_background_work` carrying the reason string, published the same way `worker_escalation_pending` is (`completion.rs:4062-4069`). This answers the "emit a passive 'still waiting' status to the UI" option directly: the operator sees "waiting on: research agent" rather than either silence or a false alarm.

### 4.2 Bounded, never indefinite

Suppression must terminate. Three independent bounds, cheapest first:

1. **Event-driven clear (happy path).** When the background subagent returns, the harness re-invokes the worker; it takes another turn and Stops again. If that Stop produced a PR → finalize; if it's genuinely idle with no outstanding subagent and no `[waiting]` marker → nudge normally. With the SubagentStop signal wired, the engine _knows_ the wait ended (count back to 0) and needs no timer for the common case.
2. **Absolute waiting cap.** Analogous to `ABSOLUTE_MAX_NUDGES`: cap the number of consecutive _waiting-suppressed_ Stops per execution (proposal: 12, reusing the existing constant's spirit). A worker that emits `[waiting]` forever — buggy, or gaming the nudge — is eventually nudged/parked anyway. Track this in the `NudgeBreaker` (or a sibling in-memory map) keyed by execution id.
3. **`stale_worker_sweep` remains the ultimate hang backstop** (see §4.3).

### 4.3 Interaction with hang detection — the load-bearing constraint

**The backgrounded-bazel idle-wait pattern (nudging IS correct).** A worker that idles on a raw `&` shell build, a `sleep`, or a foreground poll has **no** harness-tracked auto-resume — it is genuinely stuck. The discriminator is exactly the positive signal from §3: no `[waiting]` marker _and_ `outstanding_subagents == 0` ⇒ default behavior (nudge) is unchanged. Suppression requires a positive assertion; it is never the default. This keeps the known-good nudge behavior intact.

**The silent-wait vs wedge tension.** A legitimately-waiting worker stops and then goes **silent** (no further hook events from its _own_ main loop) until the subagent returns. `stale_worker_sweep` reaps on "no hook event for > `stale_threshold_secs`" (`stale_worker_sweep.rs:308-310`). So a subagent slower than the threshold risks reaping a healthy worker. Two mitigations, both needed:

- **Make the sweep waiting-aware.** While `outstanding_subagents > 0` for a run (or a `[waiting]` marker is the last observed final message), the sweep should extend its patience — but **must not** disable itself: a hard ceiling (e.g. `stale_threshold_secs × k`, or a separate `waiting_stale_threshold_secs`) still reaps a wait that has clearly hung. This is the one place the design must be conservative: a never-returning subagent has to be caught.
- **Confirm whether subagent-internal tool hooks refresh liveness.** A running subagent makes its own tool calls; if those fire `PreToolUse`/`PostToolUse` that reach `boss-event` and refresh `last_event_at`, then an _active_ subagent already keeps its parent "alive" to the sweep, and only a subagent that _itself_ hangs (no tool calls) trips it — which is the correct outcome. This is an empirical unknown (§9) that determines how much explicit sweep work is required.

**Pane-death / orphan reconciliation is unaffected.** If the subagent never returns and the worker's process dies, `dead_pid_sweep`/`lost_workspace_sweep` reap it exactly as today. The design adds no path that keeps a dead worker's slot held.

---

## 5. Question 3 — Worker-side convention (`[waiting]` marker)

### 5.1 Spec

A worker that is about to stop-and-wait emits, on its own line as (part of) its final message:

```
[waiting] reason="<one-line what/why>"
```

Optional structured fields for richer UI + tighter engine gating:

```
[waiting] reason="research agent: transport survey" on=subagent expect=auto-resume
```

- `reason="…"` — required, double-quoted, one line (identical discipline to `[blocked]`).
- `on=subagent|background-bash|workflow|ci` — optional; what is being awaited (feeds §6 generalization + the passive UI status).
- `expect=auto-resume` — optional; asserts the awaited work re-invokes the session. Absence is treated as `auto-resume` for `on=subagent`.

### 5.2 Parser — reuse the `worker_escalation` family

Add to `worker_escalation.rs` alongside `[effort-escalation]`/`[blocked]`:

- `WAITING_MARKER = "[waiting]"`, a `WorkerSignalKind::Waiting`, and a `validate_waiting_fields`.
- Matching discipline: **trimmed-line prefix** match (`strip_prefix("[waiting]")`), exactly like the sibling markers (`worker_escalation.rs:92-111`). Line-start anchoring means a worker that merely _mentions_ the protocol in prose does not trip it — the same false-positive guard the family already relies on and tests.
- **Parse source:** prefer `last_assistant_message` straight from the Stop payload (documented, avoids a transcript read entirely). Capture it by extending `WorkerEvent::Stop` with an optional `last_assistant_message` field in `normalize_hook_event`. Fall back to the existing joined-final-message reader (`read_final_triage_message`, `completion.rs:2874-2930`) if the field is absent — that reader already joins _all_ assistant turns to dodge the known Stop-hook/flush race.

### 5.3 Engine hook-in

In `nudge_or_park`, after the `unresolved_worker_signal_reason` check and before the breaker, add: if the current final message carries a live `[waiting]` marker (and the absolute waiting cap is not exhausted) → return `WaitingOnBackgroundWork { reason }`. Unlike the escalation path, do **not** file an attention item and do **not** require coordinator ack to clear — the next Stop re-evaluates from scratch.

### 5.4 Marker vs hook-count — robustness tradeoff

|            | `[waiting]` marker                                                             | SubagentStop-vs-spawn count                                      |
| ---------- | ------------------------------------------------------------------------------ | ---------------------------------------------------------------- |
| Depends on | worker compliance (preamble instruction)                                       | one documented hook wired + correct spawn/finish pairing         |
| Precision  | high — worker states _what_ and _why_                                          | binary — "something outstanding"                                 |
| Covers     | any auto-resuming wait the worker declares (§6)                                | only Agent/Task subagents                                        |
| Fails when | worker forgets to emit it (the §1 worker did — but it had no such instruction) | tool name isn't `Task`/`Agent`; nested/forwarded spawns miscount |
| UI value   | carries a human reason string                                                  | none by itself                                                   |

They cover each other's failure modes, which is why the recommendation uses both: the marker is the primary, precise, UI-friendly signal; the count is the automatic safety net for workers that forget.

---

## 6. Question 4 — Scope: generalize beyond the Agent tool?

**Yes — the same false nudge fires for any legitimate stop-and-wait**, not just Agent-tool waits: a worker that backgrounds a long CI/build via `run_in_background` Bash and stops to be re-invoked on exit, a Monitor-style wait, or a `Workflow` run. All share the property that makes stopping correct: **the harness re-invokes the session when the tracked work completes.**

Generalize the _marker_ to cover "harness-tracked auto-resuming background work" (the `on=` field expresses which kind), and keep the engine treatment identical. **Do not** generalize to "any Stop" — that reintroduces the very hang the nudge exists to catch. The positive signal (marker, or outstanding-subagent count) is precisely what scopes suppression to the auto-resuming cases and leaves genuine idles (raw `&` builds, sleeps, dead waits) getting nudged as today.

The engine-side count (§3.2) only covers subagents. If the operator wants automatic (marker-free) coverage of background _Bash_, that needs a second counter over `run_in_background` Bash lifecycle events — feasible with the same `PreToolUse`/`PostToolUse` stream, but a larger change. Recommendation: ship the marker (which already covers all cases the worker declares) first; add the Bash counter only if non-compliance data shows it's needed.

---

## 7. Recommendation

**A hybrid, marker-primary design with an automatic backstop and hard bounds.**

1. **Worker-side `[waiting]` marker (primary).** New marker in the `worker_escalation` family, parsed from `last_assistant_message`; suppresses the produce-PR nudge with _self-clearing, no-attention-item, no-breaker-consumption_ semantics; publishes a passive `worker_waiting_on_background_work` live status. Add a short worker-preamble instruction (next to the `NO_CHANGES_NEEDED` / escalation instructions in `runner.rs`) teaching workers to emit it before stopping to wait.
2. **SubagentStop-vs-spawn count (automatic backstop).** Wire the documented `SubagentStop` hook (8th lifecycle hook); add `WorkerEvent::SubagentStop`; maintain a per-run outstanding-subagent counter from `PreToolUse{tool_name∈{Task,Agent}}` minus `SubagentStop`. Treat `count > 0` at a main Stop as an implicit waiting state even when the marker is absent.
3. **Hard bounds so hang detection can't regress.** Absolute cap on consecutive waiting-suppressed Stops (reuse the `ABSOLUTE_MAX_NUDGES = 12` ceiling's spirit); a waiting-aware but still-ceilinged `stale_worker_sweep`; unchanged pane-death/orphan reconciliation.

**Why this over the alternatives:**

- _Suppress-only-on-marker_ is simplest but strands non-compliant workers (the §1 worker forgot). The count backstop fixes that.
- _Detect-only-via-count_ needs no worker change but gives the UI no reason string and can't cover background Bash / Workflow waits. The marker fixes that.
- _Grace-timer-only_ (defer the nudge N seconds, then fire) is the weakest: it still fires a false nudge on any wait longer than the grace, and picking N trades false nudges against slow hang detection. The event-driven clear (subagent-return) is strictly better and needs no magic constant on the happy path.

**Tradeoffs / residual risk:**

- Wiring an 8th hook slightly increases hook traffic (`boss-event` invocations). Negligible — the shim is already on every tool call.
- The count assumes the spawn tool is named `Task`/`Agent`; a rename or a `Workflow`-style spawner would miscount. Mitigated by the marker (name-independent) being primary, and by matching a configurable set of tool names.
- A worker could suppress nudges indefinitely by spamming `[waiting]`. Bounded by the absolute waiting cap + sweep ceiling.

---

## 8. Question 5 — Instrumentation (make false nudges visible in engine-trace)

- **Structured log on every suppression:** `execution_id`, detection source (`marker` | `subagent_count` | `both`), `reason`, `outstanding_subagents`, and the waiting-cap counter — at `info`, mirroring the existing `auto-nudge: suppressed …` line (`completion.rs:4056-4061`).
- **The false-negative detector (most important).** When the engine _does_ nudge a no-PR worker and the **very next** event for that run is a `SubagentStop` (or a `PostToolUse{tool_name:"Task"}` resolving), emit a `warn`: `nudge_landed_during_pending_subagent`. This surfaces exactly the misses — cases where detection failed and a nudge hit a real wait — and makes the false-positive rate measurable rather than invisible.
- **Counters** (whatever metrics sink the engine already uses): `waiting_suppressed_nudges_total`, `waiting_cap_tripped_total`, `nudge_during_pending_subagent_total`, `waiting_stale_sweep_reaps_total`. The first climbing with the last near-zero is the success signal; the third above zero flags detection gaps.
- **`background_tasks` shape/presence canary.** The one path by which the undocumented `background_tasks` field (§3.2) can be trusted at all is if drift in it is loud, not silent. On every Stop, log `background_tasks_presence_rate` (does the field appear, and non-empty-vs-empty-vs-absent) and emit `background_tasks_shape_changed_total` whenever its schema diverges from the expectation captured in §9 item 5. Also log `background_tasks.len()` (when present) alongside `outstanding_subagents` on every Stop as a standing shadow comparison — divergence between the two is either a `background_tasks` surprise or a counter-pairing bug in the documented signal, and either is worth knowing about. A presence-rate cliff or a shape-changed spike is the signal that a CC upgrade silently broke the corroborator, at which point detection falls back to the `[waiting]` marker and the `outstanding_subagents` counter, both unaffected.
- **Live status** `worker_waiting_on_background_work{reason}` so the waiting state is visible in the UI and in `bossctl` inspection, not just logs.

---

## 9. Empirical validation items (confirm before/while implementing)

These are the assumptions the design rests on. Each is cheaply checkable by capturing a real worker's forwarded hook stream (the engine already receives and can log every payload):

1. **Does a main `Stop` actually fire while a background subagent is outstanding?** The §1 incident implies yes (the nudge is `on_stop`-queued and requires a Stop). Confirm the engine isn't instead nudging via the idle-injection path (`dispatch_probe_if_idle`, `app/worker_events.rs:684-744`) — the fix location differs if so. _This is the single most important thing to confirm first._
2. **Do subagent-internal tool calls fire `PreToolUse`/`PostToolUse` that reach `boss-event`?** Determines whether an active subagent already keeps `last_event_at` fresh (so the sweep only reaps genuinely-hung subagents) or whether the sweep needs explicit waiting-awareness (§4.3).
3. **Exact spawn tool name(s):** `Task` vs `Agent` vs a `Workflow` spawner — and whether `SubagentStop.agent_type` correlates with the spawn's `tool_input.subagent_type`. Fixes the count's matcher set.
4. **Is `last_assistant_message` reliably populated on the Stop payload** for a worker whose final turn is just "I'll wait…", so the marker rides in without a transcript read?
5. **Capture a matrix of real Stop payloads for `background_tasks`**, not a single sample — this is what would let a later task actually rely on the field as the fast-path corroborator described in §3.2. For each capture, tag the exact CC version and cover: a single background `Agent`, multiple concurrent background `Agent`s, `run_in_background` Bash, a `Workflow` spawn, a _foreground_ Agent (must **not** show it at the main Stop — the key false-positive check), and a plain no-background idle Stop (control — must be empty/absent). For each shape, record: is the field present at all; is it absent-vs-empty when nothing is outstanding; its exact schema (array of ids vs objects, keys present); and whether it self-clears (empty on the _next_ Stop after the subagent returns — this is what would guarantee suppression terminates on the happy path if gated on it). Two validation gates sit on top of this capture, both required before §3.2's fast-path can be enabled, not just logged:
   - **Shadow comparison** against the `outstanding_subagents` counter (§3.2, §7): once `SubagentStop` is wired, log `background_tasks.len()` next to `outstanding_subagents` on every real Stop, gating nothing on it initially. Agreement across many Stops and several CC versions is the evidence; every divergence is a free bug report.
   - **Presence/shape canary** (§8): the runtime assertion that turns a silent CC-side change into a loud one (`background_tasks_presence_rate`, `background_tasks_shape_changed_total`). Must be wired and observed stable before the field is used as anything more than a logged corroborator.

---

## 10. Follow-up code changes (out of scope for this investigation)

This doc changes no code. The concrete implementation tasks it recommends, for the operator to file separately:

1. **Wire `SubagentStop`** — add it to the driver's lifecycle-hook list (`driver/claude.rs:107-113`), add `WorkerEvent::SubagentStop { session_id, agent_id, agent_type }` + normalization (`worker_event.rs`), and a per-run outstanding-subagent counter fed by `PreToolUse{Task}` − `SubagentStop`.
2. **Capture `last_assistant_message` on `Stop`** — extend the `Stop` variant + `normalize_hook_event` so markers can be parsed from the payload without a transcript read.
3. **Add the `[waiting]` marker** — `WAITING_MARKER`, `WorkerSignalKind::Waiting`, `validate_waiting_fields` in `worker_escalation.rs`; a worker-preamble instruction in `runner.rs`.
4. **Suppress in `nudge_or_park`** — new `StopOutcome::WaitingOnBackgroundWork`, passive `worker_waiting_on_background_work` status, no attention item, no breaker consumption, absolute waiting cap.
5. **Make `stale_worker_sweep` waiting-aware** — extend patience while a subagent is outstanding, with a hard ceiling that still reaps a hung wait.
6. **Instrumentation** — the logs, counters, and the `nudge_landed_during_pending_subagent` false-negative detector from §8.
7. _(Optional, data-driven)_ background-Bash lifecycle counter for marker-free coverage of `run_in_background` waits (§6).
