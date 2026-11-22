# Antigravity (`agy`) acceptance ledger

- **Date:** 2026-11-21
- **Kind:** perishable status, companion to the [Antigravity driver design](./antigravity-driver-fourth-driver-google-gemini-cli.md)
- **Purpose:** record whether the pinned-only gate criterion has been met, and the runs that count toward it, without rewriting the design's durable reasoning

This document is the live status surface. Refresh rows here when pinned `agy` executions land. Do not copy that status back into the design.

Spend, quota, and cost accounting do not appear here. They are a cross-driver concern, not a gate on this driver.

## Current gate state: open by operator decision

The repo owner opened allocated `agy` traffic on **2026-11-21 before the ledger criterion was met**. This is an explicit rollout decision, not evidence that the criterion passed. The recorded qualifying count remains 0 / N.

The gate is now the persisted per-driver `launch_enabled` flag alongside the traffic split. Existing splits without flags default to enabled for all implemented drivers, including `agy`, so the already-configured 35% agy share takes effect when this ships. No production database edit is required. An installation without a configured split still starts with agy at 0%.

## Criterion

N pinned `agy` executions, each reaching a merged PR, with no engine-side incident on those runs.

- **N** is an operator-chosen count. This ledger does not invent one. Record the chosen N in the table header when the first qualifying run is entered.
- **Pinned** means the execution was routed because of an explicit `tasks.driver` or product `default_driver` of `agy`, recorded as reason `explicit`. Allocated traffic does not count toward this pinned-run criterion.
- **Merged PR** means the work item's PR merged. An open PR, a closed-unmerged PR, or a `no-changes-needed` run does not count.
- **No engine-side incident** means the engine did not fail the run: spawn, pane, events-socket routing, credential delegation, hook adapter identity, progress identity, or turn-boundary handling. Model quality, ordinary worker questions, and review comments are not engine-side incidents.
- **Real Boss dispatch only.** A hand-built harness, a standalone PTY, or a pane-viability probe does not count. The design's rule is that acceptance evidence must come from real dispatch, because a harness is assembled from the same beliefs that produced the code.

The criterion is unmet. The evidence table is empty.

## How the operator controls the gate

`bossctl dispatch traffic-split` shows configured percentages, launch flags, effective percentages after launch exclusions, and exclusion reasons. `bossctl dispatch traffic-split --disable agy` closes allocated traffic while retaining its configured share; `--enable agy` reopens it. Settings → Engine → driver split exposes the same flags and configured/effective split. Work-kind and model requirements can further restrict eligible drivers.

The whole split and its flags are stored atomically in one metadata value, survive engine restarts, and are read at each allocation, dispatch claim, and launch composition. A closed launch flag yields `agy excluded: launch flag off` in the allocation's `execution_driver_decisions.split_at_decision.exclusions` JSON. Remaining shares are renormalised visibly. If no enabled eligible driver has a non-zero share, allocation fails loudly; it never revives a zero-share driver. Running workers retain their routing.

Explicit task/product pins still route when they clear the capability gate, allowing canaries while allocated launches are disabled. Pool-bound work retains its fixed-driver policy. The code-level `launch_implemented` bit only distinguishes unimplemented skeletons; rollout never requires a code edit.

The original zero-share rollout came from [mono#3565](https://github.com/spinyfin/mono/pull/3565). Zero still means zero: a launch flag does not raise a configured share. The owner controls both the share and whether allocated launches are enabled. Filling the evidence table changes neither setting automatically.

## Evidence

Qualifying runs. Empty until the first pinned `agy` execution merges a PR without engine-side incident.

| #   | Date | Execution | PR  | Merged | Engine-side incident | Notes |
| --- | ---- | --------- | --- | ------ | -------------------- | ----- |

Count toward criterion: **0 / N**. Gate state: **open by the repo owner on 2026-11-21, criterion unmet**.
