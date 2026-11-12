# Antigravity (`agy`) acceptance ledger

- **Date:** 2026-11-11
- **Kind:** perishable status, companion to the [Antigravity driver design](./antigravity-driver-fourth-driver-google-gemini-cli.md)
- **Purpose:** record whether the pinned-only gate criterion has been met, and the runs that count toward it, without rewriting the design's durable reasoning

This document is the live status surface. Refresh rows here when pinned `agy` executions land. Do not copy that status back into the design.

Spend, quota, and cost accounting do not appear here. They are a cross-driver concern, not a gate on this driver.

## Current gate state: `pinned-only`

The project sits in **`pinned-only`** until the criterion below is met.

`agy` is registered (once the driver crate lands), resolvable, and reachable only by an explicit pin that clears the capability gate. Allocated traffic does not go to `agy`. This is a steady state, not a waiting state: nothing is blocked, nothing silently proceeds, and the driver is exercised on real work the whole time through pins.

Meeting the criterion does **not** raise the share. Raising the share is an operator action against engine state. No implementation task performs it.

## Criterion

N pinned `agy` executions, each reaching a merged PR, with no engine-side incident on those runs.

- **N** is an operator-chosen count. This ledger does not invent one. Record the chosen N in the table header when the first qualifying run is entered.
- **Pinned** means the execution was routed because of an explicit `tasks.driver` or product `default_driver` of `agy`, recorded as reason `explicit`. Allocated traffic does not count, because under the shipped default there is none.
- **Merged PR** means the work item's PR merged. An open PR, a closed-unmerged PR, or a `no-changes-needed` run does not count.
- **No engine-side incident** means the engine did not fail the run: spawn, pane, events-socket routing, credential delegation, hook adapter identity, progress identity, or turn-boundary handling. Model quality, ordinary worker questions, and review comments are not engine-side incidents.
- **Real Boss dispatch only.** A hand-built harness, a standalone PTY, or a pane-viability probe does not count. The design's rule is that acceptance evidence must come from real dispatch, because a harness is assembled from the same beliefs that produced the code.

The criterion is unmet. The evidence table is empty.

## How `pinned-only` is enforced

`pinned-only` is existing engine code, not a checkpoint row that only blocks later work. The four-way split migration ([mono#3565](https://github.com/spinyfin/mono/pull/3565)) is the enforcement half.

1. **Zero share.** `DriverTrafficSplit::default` is `claude = 100`, `codex = 0`, `grok = 0`, `agy = 0`. An engine that has never had a split configured, and any split that still has `agy = 0`, gives `agy` an empty interval. A unit assertion pins that shipped default, so a later edit that quietly raises it fails a test rather than silently changing fleet behaviour.

2. **"Zero means zero" in `allocate_among`.** Allocation places a work item's id on the split's `[0, 100)` bucket line, renormalised over the drivers eligible for that kind (`DriverTrafficSplit::driver_for_bucket_among`). A share of 0 contributes nothing to the running total, so its two boundaries coincide and its interval is empty. Renormalisation never resurrects a driver the operator left at zero. If `agy` is the only eligible driver and its share is 0, allocation fails loudly rather than falling through to another driver. Until `agy` is registered, a non-zero `agy` share cannot reach it: allocation redistributes that share among registered eligible drivers, or falls through to the engine default when the entire share belongs to unregistered drivers.

3. **An explicit pin still routes.** `decide_execution_driver` honours a `tasks.driver` pin, then a product `default_driver`, **before** allocation, when that driver clears the dispatch capability gate for the execution kind. Those decisions are recorded with reason `explicit`. That is how `agy` is exercised while its share is zero: pin a row or a product, and the run goes there. A pin the gate refuses (for example a future `agy` pin on `ConflictResolution`, which requires `CommandOutcomeObservation`) is dropped so the row can still dispatch among eligible drivers.

4. **Pool-bound work never reaches this gate.** PR review and automation triage dispatch on the pool's fixed driver. They do not participate in the split and are not a way around `pinned-only`.

No allocated work reaches `agy` until an operator makes a deliberate, atomic, single-write change to the persisted split (one metadata row, the whole four-way value). Changing the split in Settings is that write.

## Raising the share

Raising `agy`'s share is an operator action against engine state. No task in the Antigravity driver breakdown has "raise the share" as its deliverable.

When the criterion is met, this ledger says so. The operator then decides whether to raise the share, and by how much. Filling the table is not that decision.

## Evidence

Qualifying runs. Empty until the first pinned `agy` execution merges a PR without engine-side incident.

| #   | Date | Execution | PR  | Merged | Engine-side incident | Notes |
| --- | ---- | --------- | --- | ------ | -------------------- | ----- |

Count toward criterion: **0 / N**. Gate state remains **`pinned-only`**.
