# Global dispatch outage

A **dispatch outage** is the condition where every registered host is excluded for every dispatchable item, so no new worker can start. It is not one item failing to fit one host. The engine detects it, the macOS app shows a persistent banner, `bossctl dispatch state` and `bossctl dispatch diagnose` report it, and it ends only when a new dispatch actually claims a host.

It exists because incident 008 and incident 010 (see [postmortems](postmortems/)) each refused every launch for many minutes, and the only signals were red text on individual queued cards and an attention row nobody reads.

## What counts as an outage

A host selection that finds no eligible host is global only when **every** registered host has an item-independent exclusion:

| Kind               | Meaning                                                                 | Recovery action the engine names                                                                                                                                                                   |
| ------------------ | ----------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `host_disabled`    | The registry has the host disabled.                                     | `bossctl hosts probe <id>`, then `bossctl hosts enable <id>`.                                                                                                                                      |
| `local_quarantine` | The local host is held by the startup quarantine of historical workers. | Restarting and cancelling the held execution do not lift it; confirm the held executions have no live worker, then delete the `local_worker_startup_quarantine` row from `metadata` in `state.db`. |

These do **not** raise an outage: a missing capability or driver, a pin or `--host` request to another host, a full pool. Each of those depends on the item, so another item may still launch. A registry with no hosts is not an outage either.

The classification lives in `classify_host_exclusions` (`engine/execution-contracts/src/dispatch_outage.rs`). When a supported command to clear the local quarantine ships, `LOCAL_QUARANTINE_RECOVERY` in that file is the one place to point at it.

## The record

`DispatchOutageTracker` holds one record per outage: start time, each host's exclusion (kind, a rendered cause, the engine's refusal text **verbatim**, and the recovery action), and counters for the refused attempts and distinct work items. Every further refused launch folds into the record. The per-item failure handling is untouched: each item still gets its own `host_selected:error`, pre-start failure and retry, so nothing is lost or reset when the outage ends.

## Recovery is a launch

The outage ends when a dispatch gets past host selection (`host_selected` with `outcome=ok`), not when a flag clears. Clearing a hold or re-enabling a host leaves the banner up until the next launch proves it worked. The engine then logs `dispatch outage ended` and emits `dispatch_outage_recovered` with the duration.

## Where it shows

- **Dispatch events** (`bossctl dispatch tail --stage dispatch_outage_started`): `dispatch_outage_started` (once, `outcome=error`) and `dispatch_outage_recovered` (once, with `duration_secs`, `refused_attempts`, `refused_work_items`, `claimed_by_execution_id`, `claimed_host_id`). Refusals in between are counted in the record, not re-emitted.
- **Engine health report**: `dispatch_outage` (structured) plus a `dispatch_outage` issue listed first. The app banner renders the issue, keeps it expanded, and offers no way to collapse or dismiss it. It is not an attention item. The engine pushes a fresh report when the outage starts, ends or changes its exclusions, and every minute while it lasts so the duration stays current.
- **`bossctl dispatch state`** and **`bossctl state`**: the same fields, from the live record (`--json` carries `dispatch_outage`).
- **`bossctl dispatch diagnose <id>`**: the same fields, rebuilt from the dispatch event stream, so it answers with the engine down; it also names the last recovery. `--json` carries `dispatch_outage` and `last_dispatch_outage_recovery`.
