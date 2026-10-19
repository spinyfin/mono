# The `critical` priority

`critical` is a fourth urgency level for tasks, chores and investigations, above `high`. It exists for one situation: the human owner has decided a fix must run _now_, and the ordinary queue would make it wait. On 2026-10-15 a fix the owner had declared critical sat `ready` for about 12 minutes behind the interactive concurrency cap while lower-priority work that had queued later kept the slots; setting `high` did not help, because `high` does not get ahead of the cap. `critical` does.

Because it bypasses capacity limits, only the human owner can set it. Everything automated — the coordinator session's CLI tier, workers, engine sweeps, automations, Boothby — is refused by the engine, not merely by the UI.

## What it does

A work item at `critical`:

- **Dispatches ahead of every non-critical item**, regardless of dispatch class, execution-level priority, queue order or age. Multiple critical items dispatch oldest-first among themselves.
- **Is not held by the interactive concurrency cap** (`bossctl dispatch concurrency`). It takes any idle slot up to the worker pool's own capacity, the hard ceiling. If the pool is completely full it is next in line ahead of everything else. Running workers are never preempted or killed for it.
- **Still dispatches under an operator dispatch pause** (`bossctl dispatch pause`). Everything else stays held, and the pause state and reason are unchanged. A _breaker_ pause (the spawn-capability circuit breaker) still holds it: the spawn path is broken, so an urgent row would only burn another attempt against it.

What it does not do: it does not clear unmet dependencies, chain holds, a blocked status, or any other eligibility rule. It changes only ordering and the two capacity gates above.

## Who can set it

The engine resolves a caller tier for every work-item write from process ancestry — the same distinction it already records in `last_status_actor`:

| Caller                                                              | Tier               | May set `critical`? |
| ------------------------------------------------------------------- | ------------------ | ------------------- |
| `boss` CLI from your own terminal, or the Boss app                  | `human`            | yes                 |
| `boss` CLI run from the coordinator session (descends from its pid) | `boss`             | no                  |
| `boss` CLI run from a worker pane (registered worker pid)           | worker             | no                  |
| Engine sweeps, automations, follow-up minting, Boothby              | `engine`/`boothby` | no                  |

A refused write fails with an error naming the tier that tried it. Lowering from `critical` is an ordinary priority change, allowed for any caller that can change priority. Re-asserting `critical` on a row that is already critical is not a raise and is not refused.

Derived rows never inherit it: a follow-up chore minted from a deferred-scope attention, or a chore minted from a merged revision, takes `high` when its source was `critical`. Engine-created revisions (CI fix, merge conflict, PR review) default to `medium` as before, and a revision cannot be _created_ at `critical` at all; raise it afterwards if you mean it.

Provenance of the raise is recorded on the row as `critical_set_by` (the tier, `human`) and `critical_set_at`, both visible in `boss task show --json`, and both cleared when the row is lowered.

## Surfaces

- `boss task create|update --priority critical`, `boss chore create|update --priority critical`, `boss investigation create --priority critical`.
- The app's priority picker (card popover and edit sheet) offers **Critical**; kanban cards show a solid red `CRIT` capsule, distinct from the tinted `H` chip for `high`.
- `boss task list|show` print the level; `--json` includes `critical_set_by` / `critical_set_at`.
- `bossctl dispatch stats` marks a still-blocked critical row with `CRITICAL` (such a row is waiting on pool capacity itself, not on the cap or a pause); `bossctl dispatch state` declares the bypass alongside the others.
- Dispatch events carry `"critical": true` on the row's `request_recorded` and `worker_claimed`/`skipped` records, and the admission is recorded as `critical`.

## Where it lives

- Write rule and provenance: `tools/boss/engine/core/src/work/critical_priority.rs`; create/update entry points in `insert_helpers.rs`, `create_entities.rs`, `updates.rs`; the worker-pane refusal in `app/work_items.rs` (`resolve_write_actor`).
- Ordering: `WorkDb::list_ready_executions` (`work/executions_runs.rs`).
- Cap and pause gates: `drain_ready_queue` in `coordinator/scheduler.rs`; the chokepoint arm `DispatchAdmission::Critical` in `coordinator/config.rs`.
