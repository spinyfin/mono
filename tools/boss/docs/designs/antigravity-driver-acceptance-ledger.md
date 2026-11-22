# Antigravity (`agy`) acceptance ledger

- **Date:** 2026-11-21
- **Kind:** perishable status, companion to the [Antigravity driver design](./antigravity-driver-fourth-driver-google-gemini-cli.md)
- **Purpose:** record whether the pinned-only gate criterion has been met, and the runs that count toward it, without rewriting the design's durable reasoning

This document is the live status surface. Refresh rows here when pinned `agy` executions land. Do not copy that status back into the design.

Spend, quota, and cost accounting do not appear here. They are a cross-driver concern, not a gate on this driver.

## Current gate state: open by operator decision

The repo owner opened allocated `agy` traffic on **2026-11-21 before the ledger criterion was met**. This is an explicit rollout decision, not evidence that the criterion passed. The recorded qualifying count remains 0 / N.

The gate is the driver's split share: **0% is off** for allocated work. A driver above 0% is eligible in proportion to its share. An installation without a configured split still starts with agy at 0%. On upgrade, a driver whose previous launch flag was off is stored at 0% and the remaining shares renormalise to 100%, so allocated routing does not change.

## Criterion

N pinned `agy` executions, each reaching a merged PR, with no engine-side incident on those runs.

- **N** is an operator-chosen count. This ledger does not invent one. Record the chosen N in the table header when the first qualifying run is entered.
- **Pinned** means the execution was routed because of an explicit `tasks.driver` or product `default_driver` of `agy`, recorded as reason `explicit`. Allocated traffic does not count toward this pinned-run criterion.
- **Merged PR** means the work item's PR merged. An open PR, a closed-unmerged PR, or a `no-changes-needed` run does not count.
- **No engine-side incident** means the engine did not fail the run: spawn, pane, events-socket routing, credential delegation, hook adapter identity, progress identity, or turn-boundary handling. Model quality, ordinary worker questions, and review comments are not engine-side incidents.
- **Real Boss dispatch only.** A hand-built harness, a standalone PTY, or a pane-viability probe does not count. The design's rule is that acceptance evidence must come from real dispatch, because a harness is assembled from the same beliefs that produced the code.

The criterion is unmet. The evidence table is empty.

## How the operator controls the gate

`bossctl dispatch traffic-split` shows percentages and 0% exclusion reasons. Settings → Engine → driver split is one bar and one percentage stepper per driver. Work-kind and model requirements can further restrict eligible drivers.

The whole split is stored atomically in one metadata value, survives engine restarts, and is read at each allocation, dispatch claim, and launch composition. A driver at 0% yields `agy excluded: 0% share` in the allocation's `execution_driver_decisions.split_at_decision.exclusions` JSON. If every eligible driver is at 0%, allocation fails loudly; it never revives a zero-share driver. Running workers retain their routing.

Explicit task/product pins still route when they clear the capability gate, allowing canaries while a driver is at 0%. Pool-bound work retains its fixed-driver policy. The code-level `launch_implemented` bit only distinguishes unimplemented skeletons; an unimplemented driver shows as unavailable rather than as a second control.

The original zero-share rollout came from [mono#3565](https://github.com/spinyfin/mono/pull/3565). Zero still means zero. Filling the evidence table does not change the configured share.

## Evidence

Qualifying runs. Empty until the first pinned `agy` execution merges a PR without engine-side incident.

| #   | Date | Execution | PR  | Merged | Engine-side incident | Notes |
| --- | ---- | --------- | --- | ------ | -------------------- | ----- |

Count toward criterion: **0 / N**. Gate state: **open by the repo owner on 2026-11-21, criterion unmet**.
