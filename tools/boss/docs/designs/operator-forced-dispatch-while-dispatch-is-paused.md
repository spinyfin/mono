# Operator force bypasses only the observed global dispatch pause

- **Date:** 2026-08-08 (design); revised 2026-08-10 for as-built reality after mono#2705
- **Provenance:** operator request for one-shot dispatch while dispatch is paused
- **Project:** Operator-forced dispatch while dispatch is paused
- **Shipped by:** [mono#2705](https://github.com/spinyfin/mono/pull/2705) (design: mono#2686)

An operator needs to start one work item without resuming global dispatch. The contested property is deliberately narrow: **force changes only the global-pause decision relative to ordinary explicit start; it does not bypass any other admission or eligibility constraint.**

## Verdict

A pause-specific override on the existing execution-request path, exposed as `bossctl work start <id> --force` and as a confirmed drag to Doing. The protocol's existing `RequestExecutionInput.force` stays pool-cap growth for `bossctl agents launch` only; the new intent is `bypass_dispatch_pause` with entry-point provenance. Shipped end to end in mono#2705; the design's safety boundary held through review and live e2e verification.

## Goals

- Let an operator dispatch one eligible work item while the global dispatch pause remains active for every other item.
- Give the CLI and app one engine-owned eligibility and explanation model.
- Make every refusal name the constraint that prevented dispatch.
- Record a successful pause override with its time, entry point, and the exact pause state overridden.
- Leave the row and global pause with no force-related residue.

## Non-goals

- Bypassing the interactive concurrency cap, hard pool cap, unmet dependencies, automation pause, startup preflight, unresolved repository, live-execution deduplication, human-driven classification, or blocked/terminal/ineligible status.
- Creating a general “start at any cost” mechanism or changing `bossctl agents launch`, whose existing force semantics intentionally bypass pool-cap deferral.
- Changing the baseline semantics of explicit start. An explicit start can still release queued-only/deferred work and can still manually retry work parked by the churn guard; the new force bit is not what authorizes those behaviors.
- Persisting force on a work item or execution, resuming dispatch globally, or adding a second spawn path.
- Extending the override to automation pause or using it as a general nudge for stuck state.

## Exactly what force overrides

Force overrides **only** an active, operator-originated global dispatch pause for a single request. Everything else still binds:

| Constraint                                                  | Under force                                                                                   |
| ----------------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| Operator-origin global dispatch pause                       | Overridable for this request only                                                             |
| Breaker-origin global dispatch pause                        | Never overridable                                                                             |
| Interactive concurrency cap                                 | Enforced (hard blocker)                                                                       |
| Unmet dependency                                            | Enforced (hard blocker)                                                                       |
| Ineligible / blocked / terminal status                      | Enforced (hard blocker)                                                                       |
| Human-driven classification                                 | No agent worker; drag skips admission evaluation entirely                                     |
| Automation pause, pool routing, preflight, live-exec dedupe | Unchanged ordinary-path rules                                                                 |
| `autostart: false`, churn-guard park                        | Informational on the admission surface; explicit start has always cleared both, forced or not |

The pause itself is reported on `DispatchPauseSnapshot`, not as a blocker code, because it is the one thing that _can_ be overridden.

## As-built approach

### One engine decision, two clients

The execution-request contract carries a distinct `bypass_dispatch_pause` intent, an `entry_point` enum (`cli` / `app_drag`), and an optional `observed_pause_since_epoch_s` generation token. The existing pool-cap `force` field is untouched and remains the only meaning of force on `bossctl agents launch`. Public CLI spelling for the pause override is still `bossctl work start <id> --force`; no other work or agent verb gains this flag.

A read-only `EvaluateDispatchAdmission` RPC returns a `DispatchAdmission`: pause snapshot (active, origin, reason, `paused_since_epoch_s`, overridable), `would_dispatch`, and a list of blockers with stable codes plus operator-facing messages. Both the macOS pre-check and the mutating path call `ExecutionCoordinator::evaluate_dispatch_admission`; the accept/refuse decision is `pause_bypass_decision` in both places so a confirmation cannot promise what the mutating request will refuse.

`bossctl work start --force` sends `RequestExecution` with `bypass_dispatch_pause = true` and `entry_point = cli`. It does not send an observed generation (`None` means evaluate the current pause fresh). The CLI surfaces the engine refusal reason in human and `--json` output.

### macOS drag-to-Doing

Before a non-human-driven drag into Doing commits, the app calls `EvaluateDispatchAdmission`. Human-driven rows skip evaluation: entering Doing without a worker is their normal transition, and routing them through admission would bounce an ordinary drag on the hard-blocker path even with no pause active.

Evaluation order on the reply:

1. **No active pause** — forward the drop through the ordinary board path with no alert. Hard blockers are _not_ bounced client-side here; the engine's ordinary `RequestExecution` path owns those refusals, as it did before this feature existed.
2. **Pause active and hard blockers present** — bounce immediately; no confirmation that implies force will clear them.
3. **Breaker (non-overridable) pause** — bounce with the stored pause reason.
4. **Operator pause, no hard blockers** — confirmation naming the pause reason and any informational-only blockers (`autostart` / churn-guard). Confirm sends `MoveWorkItemOnBoard` with `bypass_dispatch_pause = true`, `entry_point = app_drag`, and the observed `paused_since_epoch_s`. Cancel bounces the optimistic card back.

A lost or undecodable admission reply, or a real disconnect while the check is in flight, bounces the card rather than stranding it. Transport-level socket-waiting noise while a check is in flight does **not** bounce a valid drag.

### Admission and race semantics

The engine re-evaluates every constraint at the mutating request. A matching operator pause is skipped for this request only. A breaker-originated pause is never overridable: it says the spawn path is unhealthy, so forcing another spawn would defeat the circuit breaker rather than make an operator exception.

Stale-confirmation handling uses **`paused_since_epoch_s` as the sole generation token**. The app echoes the generation it showed; if the token no longer matches (pause lifted or re-raised), the engine rejects with the current pause reason rather than acting on a different pause than the one named in the confirmation. Origin and reason are not compared as separate fields — a re-raise advances the generation, so the token is the load-bearing equivalence.

When no pause is active at mutate time, the request proceeds as an ordinary start and records no override event.

### Pool-correct single-shot bypass (not a second spawn path)

Admitted bypasses do **not** use `force_dispatch`'s pool-growth / pool-classification-bypass path. The coordinator marks the ready execution id in a never-persisted, single-shot in-memory set (`dispatch_pause_bypass_execution_ids`), then runs the existing `drain_ready_queue` claim/spawn path so main / automation / review routing and caps stay correct. The marker is consumed on the pause gate visit (and cleared after the forced drain pass so a never-visited row cannot remain bypass-eligible).

A refused forced request leaves no newly created ready execution: a row this request created is cancelled; a pre-existing row is left alone.

**Claim-time answer (as-built refinement).** The mutating path answers once the row is claimed past the pause gate — a fast, synchronous fact — rather than waiting for the full cube-lease-bound `schedule_execution` tail. After claim, infrastructure success or failure is ordinary retry/backoff, exactly as for an unpaused drain. Waiting on that tail before answering was tried in development and produced a race; the design's load-bearing property is "admitted past the pause with no residue on refusal," not "spawn completed before the RPC returns." The `dispatch_pause_override` event fires only when the row was actually claimed; a row that clears the pause but then loses to the interactive cap (or another post-pause constraint) emits `dispatch_pause_override_refused` instead and leaves no residue.

Board-gesture re-check refusals after the status patch has already landed: the engine reverts status, answers with `work_error`, stamps `last_status_actor = engine`, and the `status_transition` dispatch event reports the reverted end state (not a surviving transition to `active`).

### Pools

Operator pauses already exempt review-pool executions. For a `pr_review` row under an operator pause, evaluation reports **no pause in effect for that row** (empty snapshot) rather than an overridable pause that would record a bogus override — matching `drain_ready_queue`'s own `paused && !is_review` gate. Breaker pauses hold reviews too and remain non-overridable. Main and automation-pool work may bypass an operator global pause, but automation work must still satisfy the independent automation-pause gate and its normal pool capacity. Force has identical pause-only meaning across pools without changing routing or caps.

### Auditability

Structured dispatch-event stages:

- `dispatch_pause_override` — override actually admitted and claimed; details carry `entry_point`, pause origin/reason/`paused_since_epoch_s`.
- `dispatch_pause_override_refused` — forced request refused; details carry `entry_point`, reason, and observed pause fields. Never claims an override occurred.

These use the existing dispatch-event ledger (not a task flag). Inspectable afterwards via the execution's dispatch diagnosis/history surface.

### Wire and reason codes

Stable blocker codes on `DispatchAdmission` (and refused force paths):

- Enforced: `interactive_concurrency_cap`, `unmet_dependency`, `ineligible_status`
- Informational only (explicit start always clears): `churn_guard_parked`, `autostart_disabled`

The app's `hardBlockers` property filters out the informational codes when deciding whether to offer confirmation.

### Invariants

- For equivalent engine state, an ordinary and forced request differ only when an operator global pause is the blocking property.
- A successful override does not change the global pause, the row's future eligibility, pool capacity, dependency graph, or later dispatch behavior; a failed override does not enqueue latent work.
- The UI renders engine-provided reasons; it does not infer pause origin or eligibility from cached board state.
- A confirmation is valid only for the pause generation the operator saw (`paused_since_epoch_s`).
- Pool-cap `force` and pause `bypass_dispatch_pause` are distinct on the wire and cannot be set accidentally by the same CLI flag path.
- Override admission is claim-past-pause, not spawn-complete; residue rules still hold when claim never happens.

## Alternatives considered

### Reuse the existing `RequestExecutionInput.force`

Rejected because its documented and tested behavior grows the target pool past its configured cap for `agents launch`, including correctly routing review and automation work to their own pools. Reusing it would make `work start --force` bypass both pause and capacity, violating the requested safety boundary. Keeping two explicit intents preserves that established operational tool while making the new override checkable. **Shipped:** two fields remain separate; CLI maps `--force` only on `work start` to `bypass_dispatch_pause`.

### Temporarily resume, start, then pause again

Rejected because the ready-queue drain can admit unrelated rows between the state changes, and a crash can leave dispatch resumed. It also changes durable global state and audit history for what must be a one-shot exception. **Shipped:** global pause never toggles; per-execution in-memory marker only.

### Let the app inspect health state and decide locally

Rejected because the app's snapshot can be stale and does not own dependency, status, churn, pool, or preflight rules. The existing board gesture deliberately sends a target rather than a derived status so the engine owns interpretation; forced dispatch preserves that precedent. **Shipped:** `EvaluateDispatchAdmission` + engine re-check; app only presents the confirmation.

## Risks closed or narrowed by shipping

- **Wire naming.** Pool-growth remains `force`; pause override is `bypass_dispatch_pause`. Operator-facing CLI spelling is `--force` only on `work start`. Closed as designed.
- **Shared reason path.** `evaluate_dispatch_admission` + `pause_bypass_decision` are shared by preview and mutate. Review caught and fixed eligibility drift between the facts query and the mutating path before merge.
- **Dispatch-event retention.** Still the existing durability boundary; no per-execution force badge or DB column in v1.
- **macOS evaluation order.** Early review found the app bouncing every Doing drag on hard blockers even with no pause. Fixed: hard-blocker bounce only when a pause is active; no-pause path forwards unchanged. Human-driven and transport-error cases also tightened in review.
- **Claim vs spawn completion.** Mutating RPC answers at claim; spawn tail stays ordinary machinery. Documented so operators do not read a successful force as “worker already running.”

## Implementation — as shipped

Single end-to-end change set in mono#2705 (protocol, engine seam, `bossctl`, macOS app, Bazel/XCTest coverage), after the design task list was collapsed to one implementation task.

| Layer    | What landed                                                                                                                                                                                                   |
| -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Protocol | `RequestExecutionInput.{bypass_dispatch_pause,entry_point,observed_pause_since_epoch_s}`, `DispatchAdmission*` types, `EvaluateDispatchAdmission` / `DispatchAdmissionEvaluated`, board-gesture bypass fields |
| Engine   | `evaluate_dispatch_admission`, `pause_bypass_decision`, `dispatch_with_pause_bypass`, single-shot drain marker, override/refusal events, board re-check status revert                                         |
| bossctl  | `work start --force` → pause-only intent + CLI entry point; refusal messages in human/JSON                                                                                                                    |
| macOS    | Pre-drag evaluation, confirmation, bounce/cancel, generation echo; human-driven and no-pause passthrough                                                                                                      |
| Tests    | Coordinator unit tests, pause-bypass e2e RPC tests, re-check refusal forensics, app XCTests for confirmation and transport non-bounce                                                                         |

Live isolated-engine verification in the PR: pause → ordinary start holds in ready → force start claims within milliseconds of the RPC → dependency-gated force still refused with no residue.

## Outstanding work

None identified relative to this design. Follow-ups that would expand scope (permanent force badge, automation-pause override, renaming pool-cap `force` on the wire) remain deliberately out of v1.
