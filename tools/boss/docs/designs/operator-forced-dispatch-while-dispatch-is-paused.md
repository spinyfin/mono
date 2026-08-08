# Operator force bypasses only the observed global dispatch pause

- Date: 2026-08-08
- Provenance: operator request for one-shot dispatch while dispatch is paused
- Project: Operator-forced dispatch while dispatch is paused

An operator needs to start one work item without resuming global dispatch. The contested property is deliberately narrow: **force changes only the global-pause decision relative to ordinary explicit start; it does not bypass any other admission or eligibility constraint.**

## Verdict

Add a pause-specific override to the existing execution-request path and expose it as `bossctl work start <id> --force` and as a confirmed drag to Doing. Do not reuse the protocol's existing `RequestExecutionInput.force`: that bit currently means “skip pool-cap deferral and grow the target pool,” which is materially broader and remains reserved for `bossctl agents launch`.

## Goals

- Let an operator dispatch one eligible work item while the global dispatch pause remains active for every other item.
- Give the CLI and app one engine-owned eligibility and explanation model.
- Make every refusal name the constraint that prevented dispatch.
- Record a successful pause override with its time, entry point, and the exact pause state overridden.
- Leave the row and global pause with no force-related residue.

## Non-goals

- Bypassing the interactive concurrency cap, hard pool cap, unmet dependencies, automation pause, startup preflight, unresolved repository, live-execution deduplication, human-driven classification, or blocked/terminal/ineligible status.
- Creating a general “start at any cost” mechanism or changing `bossctl agents launch`, whose existing force semantics intentionally bypass pool-cap deferral.
- Changing the baseline semantics of explicit start. Today an explicit start can release queued-only/deferred work and can manually retry work parked by the churn guard; a forced explicit start retains those same behaviors, but the new force bit is not what authorizes them.
- Persisting force on a work item or execution, resuming dispatch globally, or adding a second spawn path.
- Extending the override to automation pause or using it as a general nudge for stuck state.

## Chosen approach

### One engine decision, two clients

Extend the existing execution-request contract with a distinct `bypass_dispatch_pause` intent and an `entry_point` enum (`cli` or `app_drag`). Keep the current pool-cap `force` field separate and rename it internally if useful for clarity, without changing `agents launch` behavior. The public CLI spelling is still `bossctl work start <id> --force`; no other work or agent verb gains this flag.

Add an engine-owned, read-only execution-admission evaluation RPC used by the app before a drag is committed. Its response contains the current pause snapshot (active state, operator/breaker origin, reason, and paused-since generation), whether that pause is overridable, and all other blocking constraints as stable reason codes plus operator-facing messages. This is choosing a shared decision seam, not merely validating a preselected UI: it prevents the app from reproducing engine rules and gives the implementation a testable comparison against the rejected client-side approach.

The app asks for this evaluation when a drag would request execution. If an operator-originated global pause is the only force-overridable condition, it presents a confirmation naming the stored reason, for example: “Dispatch is paused: investigating worker failures. Start this item without resuming dispatch?” If non-overridable blockers also exist, the alert names them and does not offer a confirm action that implies force will clear them. If there is no active pause, the drag follows the normal path without an alert.

Confirmation sends `bypass_dispatch_pause = true`, `entry_point = app_drag`, and the observed pause generation through the normal board-gesture/request-execution flow. The CLI sends the same intent with `entry_point = cli`; it does not need a preview because the explicit flag is the confirmation.

### Admission and race semantics

The engine re-evaluates every constraint at the mutating request, before creating or dispatching a new execution. A matching operator pause is skipped for this request only. A breaker-originated pause is not overridable: it says the spawn path is unhealthy, so forcing another spawn would defeat the circuit breaker rather than make an operator exception.

If the pause was lifted between preview and confirmation, the request proceeds as an ordinary start and records no override. If its origin, reason, or paused-since generation changed, the engine rejects the stale confirmation with the new pause reason; the app refreshes and asks again. Every other failed condition returns a non-success response naming the blocker. In particular, a forced request that reaches a full interactive pool fails with the cap in its message rather than growing the pool or silently queueing until resume. A failed override must not leave a newly created ready execution behind; an execution that predated the request remains unchanged.

This uses the existing request, ready-queue, claim, and spawn path. The coordinator's pause check becomes parameterized for one request; the global atomic pause remains set and all unrelated drain candidates remain held. No force state is stored on the task, and after the request is admitted its execution follows ordinary pool routing and lifecycle rules.

### Pools

Operator pauses already exempt review-pool executions, so force is normally meaningless for reviews and the evaluation reports that no pause override is needed. Breaker pauses hold reviews too and remain non-overridable. Main and automation-pool work may bypass an operator global pause, but automation work must still satisfy the independent automation-pause gate and its normal pool capacity. Thus force has identical pause-only meaning across pools without changing their routing or caps.

### Auditability

When, and only when, the engine actually admits a request through an active matching pause, append a structured dispatch event tied to the execution. The event records an override stage, timestamp, entry point, work item and execution IDs, and the pause origin, reason, and paused-since generation. This uses the existing dispatch-event ledger rather than a task flag; it remains inspectable afterwards through the execution's dispatch diagnosis/history surface and distinguishes forced admission from normal dispatch. Rejections are also logged with their stable reason code, but do not claim an override occurred.

### Invariants

- For equivalent engine state, an ordinary and forced request differ only when an operator global pause is the blocking property.
- A successful override does not change the global pause, the row's future eligibility, pool capacity, dependency graph, or later dispatch behavior; a failed override does not enqueue latent work.
- The UI renders engine-provided reasons; it does not infer pause origin or eligibility from cached board state.
- A confirmation is valid only for the pause generation and reason the operator saw.
- The existing pool-cap force and the new pause override are distinct on the wire and cannot be set accidentally by the same CLI flag path.

## Alternatives considered

### Reuse the existing `RequestExecutionInput.force`

Rejected because its documented and tested behavior grows the target pool past its configured cap for `agents launch`, including correctly routing review and automation work to their own pools. Reusing it would make `work start --force` bypass both pause and capacity, violating the requested safety boundary. Keeping two explicit intents preserves that established operational tool while making the new override checkable.

### Temporarily resume, start, then pause again

Rejected because the ready-queue drain can admit unrelated rows between the state changes, and a crash can leave dispatch resumed. It also changes durable global state and audit history for what must be a one-shot exception.

### Let the app inspect health state and decide locally

Rejected because the app's snapshot can be stale and does not own dependency, status, churn, pool, or preflight rules. The existing board gesture deliberately sends a target rather than a derived status so the engine owns interpretation; forced dispatch should preserve that precedent.

## Risks / open questions

- The existing protocol uses the name `force` for pool growth. Implementation should choose unambiguous internal and wire names, while retaining the operator-friendly CLI spelling `--force` only on `work start`.
- The admission evaluator and mutating request must share one reason-producing function; two similar implementations would drift and could make the confirmation promise differ from the actual refusal.
- Dispatch-event retention is the existing durability boundary. If operators later need a permanent per-execution badge or database query, that is a separate observability enhancement, not a reason to persist force on the row in v1.
- Existing manual-start behavior already serves some stuck-row recovery, notably churn-guard retries. This design does not broaden that behavior; any general override taxonomy needs a separate safety review.

## Proposed implementation task breakdown

Breakdown size: 4 entries (3 in-scope, 1 deferred) — this is below the 8–14 entry multi-subsystem anchor because the feature is one shared admission seam with two thin clients, and splitting its protocol and engine halves would create an unreviewable intermediate contract.

### Pause-only engine admission contract

Scope: Add the shared read-only admission evaluation, distinct pause-override intent and entry-point provenance to the protocol and engine. Route confirmed requests through the existing request/queue/claim/spawn path; bypass only a matching operator pause, emit structured override/refusal events, preserve pool-cap force behavior, and ensure a refusal leaves no newly queued execution. Cover pause generation races, every non-overridden blocker, pool routing, and no-residue behavior with Bazel tests.

Effort hint: medium

Dependencies: none

Scope: in-scope

### Coordinator `work start --force`

Scope: Add `--force` only to `bossctl work start`, map it to the pause-only intent with CLI provenance, and surface engine refusal messages and JSON results without mapping it to the existing pool-growth bit. Add CLI parsing and engine-boundary tests.

Effort hint: small

Dependencies: Pause-only engine admission contract

Scope: in-scope

### Confirm paused drag in the macOS app

Scope: Before a drag-to-Doing that requests execution, call the engine admission evaluator, render its pause reason and any non-overridable blockers, and send the confirmed pause generation with app-drag provenance through the existing board gesture. Add Swift tests for confirm, cancel, changed/lifted pause, and refusal bounce-back behavior.

Effort hint: medium

Dependencies: Pause-only engine admission contract

Scope: in-scope

The coordinator CLI and macOS app tasks are at the same dependency depth and may run in parallel; they edit different client trees and share only the already-landed protocol. If incidental protocol adjustments are needed, each client task must forward-port the engine contract preservingly rather than redefining it.

### General manual-dispatch override taxonomy

Scope: Investigate whether capacity, churn parking, deferred approval, blocked statuses, or stuck-state recovery should have separately named override intents and authorization/audit policies. Do not extend the pause-only flag or block v1 on this work.

Effort hint: medium

Dependencies: Pause-only engine admission contract

Scope: deferred (future / not a v1 blocker) — broader overrides require separate safety decisions
