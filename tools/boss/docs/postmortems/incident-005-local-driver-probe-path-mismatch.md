# Incident 005 — A driver-capability probe resolved binaries through a PATH the pane launcher never uses, and every dispatch halted

- **Date:** 2026-08-18, 18:05–18:19 local (CDT). All timestamps below are America/Chicago (CDT, UTC−5) and are labelled as such. Focal window: Boss 1.0.555 installed 18:05:50 CDT; first successful dispatch 18:18:56 CDT.
- **Severity:** High — total dispatch outage on the maintainer's machine; zero workers could start. Throughput was zero, not degraded: the only enabled host advertised no drivers, and the only other registered host (`anaplian`) was already disabled.
- **Class:** A capability probe that models the launch environment differently from the launcher it describes. The gate and the producer that feeds it shipped in the same change and disagreed about what "installed" means.
- **Status:** Mitigated by three manually asserted `source='user'` driver tags on the local host; root-cause fix filed separately and in progress. This document is doc-only and does not implement that fix.
- **Related:** [`incident-003-engine-startup-schema-ordering.md`](incident-003-engine-startup-schema-ordering.md) (a precise diagnosis already written to a log, with no client that reads it, and a total outage on the maintainer's machine); introducing change [mono#2753](https://github.com/spinyfin/mono/pull/2753) ("Make installed drivers a hard host placement capability").

## 1. Verdict

Boss 1.0.555 stopped dispatching every work item for thirteen minutes because it asked one question about the machine and acted on the answer to a different one.

[mono#2753](https://github.com/spinyfin/mono/pull/2753) made the resolved agent driver a hard host-placement capability: a host must carry `driver=<slug>` before it can receive that work. The same change added the local probe that is supposed to populate those tags. A review revision inside that PR, landed 2026-08-17 14:34 CDT, overrode the probe's `PATH` to `WORKER_SANITIZED_PATH` so a `bossctl` command that opens the work DB could not rewrite host capabilities from whatever shell it happened to run in. That hazard is genuine. The override modelled the wrong launch path.

The probe runs `sh -c 'command -v <binary>'` with `PATH` forced to `/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin`. The pane launches the same binary by bare name under the user's login shell, which re-prepends `~/.local/bin` from `~/.zprofile` / `~/.zshrc`. The drivers live in `~/.local/bin`. The probe was correct about the question it asked and wrong about the world it was describing.

Workers could always run `claude`. The probe could never see it. The local host therefore advertised `drivers-probed=true` with zero `driver=` tags, every placement failed closed, and the queue had nowhere to go.

## 2. Summary

Boss 1.0.555 — the first build carrying [mono#2753](https://github.com/spinyfin/mono/pull/2753) — installed and launched at 18:05 CDT on 2026-08-18. From the engine's first moment in that build, no work item could be placed on any host.

Executions cycled `request_recorded` → `worker_claimed` → `host_selected/error` roughly every five seconds (**estimated** from the retry cadence recorded in the dispatch stream; the first and last error timestamps below are **determined**), backing off and eventually failing:

```
no eligible host for work item task_…_52b: no enabled host has driver grok;
  local: missing driver grok; anaplian: disabled, missing driver grok
```

The same error appeared for `claude`. That was the tell: this was not one driver missing, it was the local host advertising _no drivers at all_.

Detection was maintainer observation of cards that would not move, not alerting. The engine had already written the diagnosis in plain language five seconds after start; nothing was watching that log line. Mitigation was three `source='user'` driver tags applied by hand after verifying each binary through a login shell. First successful dispatch: 18:18:56 CDT, `host_selected/ok host_id=local`.

## 3. Observed effects

### Placement

With the only enabled host ineligible and `anaplian` disabled, the queue had nowhere to go. Throughput was **zero**, not degraded. No worker pane started during the outage.

The local host's capability set had four entries and none of them was a driver (**determined**, counted from the host capability listing at incident time):

```
host local — enabled, pool=1
  arch=arm64            [auto]
  drivers-probed=true   [auto]      ← the probe ran
  gh-authed=true        [auto]
  os=macos              [auto]      ← and found nothing
```

`drivers-probed=true` is the marker that discovery completed. Combined with zero `driver=` tags it means "checked, missing" — which is exactly the fail-closed input the new gate is specified to honour. The gate behaved correctly on a false inventory.

### Retry loop

One traced execution (the `grok` example above; work-item id truncated in the source notes) produced:

| Time (CDT) | Event                                        | Label          |
| ---------- | -------------------------------------------- | -------------- |
| 18:07:26   | First `host_selected/error`                  | **determined** |
| 18:08:31   | Last retry; execution goes terminal `failed` | **determined** |

The intervening retries ran on a roughly five-second cadence (**estimated** from that pair of timestamps and the "roughly every five seconds" observation in the investigation notes). Global dispatch had been paused and was resumed by maintainer request at ~18:07 CDT, un-masking the failure; the underlying ineligibility existed from the first moment of 1.0.555 (warning at 18:05:55 CDT).

### What a restart would have done

Nothing useful. Capability discovery re-runs on every schema init (`tools/boss/engine/core/src/work/schema_init.rs:58` on a fresh database, `:405` on the migration-chain path), and there is no durable latch on `drivers-probed=true` — only a process-lifetime `OnceLock` cache (`tools/boss/engine/core/src/host_registry.rs:229-231`). Restarting the engine would have re-run discovery and produced the identical empty result.

## 4. Timeline

All times local (CDT), 17–18 August 2026. Clock times with seconds are **determined** from logs / traces; times marked `~` are **estimated** from the investigation notes.

| Time                    | Event                                                                                                                                                                                                  |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Aug 14 16:55 CDT        | [mono#2753](https://github.com/spinyfin/mono/pull/2753) opened — driver becomes a hard placement capability.                                                                                           |
| **Aug 17 14:34 CDT**    | **Review revision adds `.env("PATH", WORKER_SANITIZED_PATH)` to the probe. The defect enters.**                                                                                                        |
| Aug 17 15:57 CDT        | [mono#2753](https://github.com/spinyfin/mono/pull/2753) merged to `main`.                                                                                                                              |
| Aug 18 18:05:50 CDT     | Boss 1.0.555 installed — first build carrying [mono#2753](https://github.com/spinyfin/mono/pull/2753).                                                                                                 |
| **Aug 18 18:05:55 CDT** | **`WARN host_registry: local host refresh found no installed drivers; driver-constrained dispatch will hold`** — the only occurrence in the entire trace corpus (**determined**). Nobody was watching. |
| Aug 18 18:06:10 CDT     | Coordinator session starts under tmux. Coincident, not causal (see §7).                                                                                                                                |
| Aug 18 ~18:07 CDT       | Global dispatch resumed by maintainer request, un-masking the failure.                                                                                                                                 |
| Aug 18 18:07:26 CDT     | First `host_selected/error`. Retry loop begins.                                                                                                                                                        |
| Aug 18 18:08:31 CDT     | Last retry; execution goes terminal `failed`.                                                                                                                                                          |
| Aug 18 ~18:15 CDT       | Root cause identified: probe and pane resolve differently.                                                                                                                                             |
| Aug 18 ~18:18 CDT       | Mitigation — three `source='user'` driver tags applied to the local host.                                                                                                                              |
| Aug 18 18:18:56 CDT     | First successful dispatch. `host_selected/ok host_id=local`. Worker live.                                                                                                                              |
| Aug 18 18:22:52 CDT     | Fix filed as a chore against Boss.                                                                                                                                                                     |

Outage length, install to first success: **13 minutes 6 seconds, determined** (18:05:50 → 18:18:56 CDT). The investigation notes round this to ~13 minutes (18:05–18:19 CDT).

## 5. Investigation and root cause

### 5.1 The two halves of [mono#2753](https://github.com/spinyfin/mono/pull/2753)

The change introduced both a gate and the thing that feeds it. They disagree about what "exists" means.

**The gate** — `tools/boss/engine/core/src/coordinator/execution.rs:192-194` inserts `driver=<slug>` into `required_capabilities`. Failure is raised at `execution.rs:239-243`, phrased by `summarize_ineligibility` at `tools/boss/engine/core/src/coordinator.rs:2252-2286`. There is no silent fall-back to a different driver or to `local`.

**The producer** — `tools/boss/engine/core/src/host_registry.rs:178` `refresh_local_host_auto_capabilities` → `:229` `discover_local_capabilities` → `:274` `discover_local_driver_capabilities` → `tools/boss/engine/core/src/host_capability_probe.rs:200-218`.

**The defect** — `host_capability_probe.rs:247-261` `local_command_on_path`.

### 5.2 What the probe asks

`local_command_on_path` (`host_capability_probe.rs:250-261`):

```
sh -c 'command -v claude'
PATH = /opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin
→ not found
```

A bare `sh` with `PATH` forcibly overridden to `WORKER_SANITIZED_PATH` (`tools/boss/engine/core/src/spawn_flow.rs:51`):

```rust
pub(crate) const WORKER_SANITIZED_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";
```

### 5.3 What the pane actually does

The pane command is a bare binary name (`claude --model {model}`, `tools/boss/engine/driver/src/claude.rs:578`; `binary: "claude"` at `claude.rs:134`, `codex.rs:233`, `grok.rs:90`), run under the user's login shell:

```
zsh -l -c 'command -v claude'
# login shell reads profiles, which re-prepend ~/.local/bin
→ /Users/<user>/.local/bin/claude
```

The drivers live in `~/.local/bin`. That directory is not in the sanitized `PATH`, but it _is_ restored by `~/.zprofile` and `~/.zshrc` before any worker starts.

### 5.4 The false premise, stated in the source

The probe's own doc comment (`host_capability_probe.rs:247-249`) claims it "uses the pane launcher's sanitized PATH rather than the process opening state.db, so a CLI invocation cannot rewrite capabilities using its ambient environment." The first half is the bug: the pane launcher does not resolve the driver through that PATH.

The override was not in the original design. It was added by a **review revision inside [mono#2753](https://github.com/spinyfin/mono/pull/2753)** on 17 August at 14:34 CDT, hardening against a real hazard — a `bossctl` command that opens the work DB triggers capability refresh, and would otherwise rewrite host capabilities from whatever shell the caller happened to be in. The hazard is genuine. The fix modelled the wrong launch path.

### 5.5 Why it wasn't caught

**The test asserted the wrong direction.** [mono#2753](https://github.com/spinyfin/mono/pull/2753) shipped with a test named `local_driver_probe_does_not_fabricate_missing_drivers` (`host_capability_probe.rs:460-463`). It verifies the probe returns empty (plus the `drivers-probed=true` marker) when nothing is on `PATH` — a real property, and the exact property that was never in doubt. Nothing asserted the positive direction: that a driver a worker _can_ launch is a driver the probe _does_ find. A test that only guards against false positives cannot catch a producer that returns false negatives for everything.

**The gate and its producer shipped together.** Because both halves landed in one PR, there was no window in which the gate ran against a producer that had been exercised in production. The first machine to run them together was the first machine to run them at all.

**The warning existed and went nowhere.** The engine said exactly what was wrong, in plain language, five seconds after start (`host_registry.rs:182-186`):

```
WARN host_registry: local host refresh found no installed drivers; driver-constrained dispatch will hold
```

That line is the only occurrence in the entire trace corpus (**determined**). It went to a log file with no alerting attached. Detection came from a human noticing two cards that would not move — roughly twelve minutes later (**estimated** from the investigation notes; the warning is 18:05:55 CDT and mitigation begins ~18:18 CDT), and only because someone happened to be looking.

## 6. Where things stand

Dispatch is healthy. The local host carries three manually asserted capabilities — `driver=claude`, `driver=codex`, `driver=grok` — added only after verifying each binary genuinely resolves through a login shell, so nothing false is being claimed.

### The mitigation is load-bearing and must be removed

These tags are written with `source='user'`. `replace_auto_capabilities` (`host_registry.rs:202-215`) deletes only `source='auto'` rows, so they survive engine restarts and re-probes indefinitely — which is exactly why they worked, and exactly why they are dangerous. They will keep asserting a driver is installed after it is uninstalled or renamed: the precise fail-open the gate exists to prevent. They also mask the bug, so the real fix cannot be verified on this machine while they remain.

The fix is filed and running: local detection must resolve the binary exactly as the pane launches it, sharing one code path so the two cannot drift again, with a regression test that fails if they can. The brief names the forbidden shortcuts explicitly — exempting the local host from the gate, treating `drivers-probed=true` with no drivers as "assume all present," hardcoding `~/.local/bin` into the sanitized path, or falling back to ambient `PATH` and restoring the hazard the revision was added to close.

## 7. Incomplete evidence (stated plainly)

- **Cause of one vanished work item is unestablished.** A merge-conflict revision task present at 17:25 CDT no longer resolves by short id or canonical id. It may have been deleted deliberately. Flagged, not diagnosed. This document does not treat it as a consequence of the probe/pane mismatch.
- **Exact moment of human detection is estimated, not determined.** The investigation notes say detection came from noticing two cards that would not move, "roughly twelve minutes" after the 18:05:55 CDT warning. No log line records when those cards were first looked at.
- **How many executions failed during the window is not counted here.** The notes establish that every dispatch failed closed and quote one representative error (plus the same shape for `claude`). They do not enumerate the full failed-execution set. This document does not invent that count.
- **The twenty-five interleaved dispatch-event lines are a real writer defect and not a cause.** All recoverable, all confined to a rotated segment. Not diagnosed here; carried as AI-5.
- **tmux is not implicated, and that has been established** — listed here so it is not re-opened. The probe overrides `PATH` explicitly, so no ambient environment could influence it; the engine (pid 13271) started at 18:05:54 CDT, sixteen seconds before tmux (**determined**), and is not its child. The proximity of the two changes was the most tempting wrong answer available, and it was wrong.

## 8. Action items

Owners are **surfaces** (files / subsystems), not people. **AI-N** means action item _N_ in this section. None of these are implemented by this document. AI-1 is already filed and in progress; AI-4 is covered in that same fix brief. AI-2 is an operational step that must follow AI-1. AI-3 and AI-5 are unfiled.

1. **Land the probe/pane resolution fix.** One shared code path so local detection resolves the binary exactly as the pane launches it, plus a regression test that fails when probe and pane resolution can diverge. Surface: `tools/boss/engine/core/src/host_capability_probe.rs` `local_command_on_path` (`:247-261`) and `discover_local_driver_capabilities` (`:200-218`); pane launch spelling at `tools/boss/engine/driver/src/claude.rs:134,578`, `codex.rs:233`, `grok.rs:90`. Forbidden shortcuts (do not do any of these): exempt the local host from the gate; treat `drivers-probed=true` with no drivers as "assume all present"; hardcode `~/.local/bin` into `WORKER_SANITIZED_PATH`; fall back to ambient `PATH` (restores the `bossctl`-refresh hazard the 17 August revision was added to close). _(Filed, in progress.)_

2. **Remove the three `source='user'` driver tags from the local host the moment the fix ships, and confirm auto-discovery repopulates them.** Until this happens the fix is unverifiable on this machine, and the tags remain a fail-open. Surface: `tools/boss/engine/core/src/host_registry.rs` `replace_auto_capabilities` (`:202-215`) — user-sourced rows are the rows that call deliberately does not delete; the tags themselves are the live `host_capabilities` rows on `host_id=local`. _(Coordinator-owned operational step, not a code change.)_

3. **Attach alerting to the driver-hold warning.** The engine already emits a precise diagnosis (`host_registry.rs:182-186`); a total-dispatch-stall condition should not depend on someone watching a kanban. Surface: `tools/boss/engine/core/src/host_registry.rs:182-186` (the `tracing::warn!` that fired once and went nowhere), to be wired into the attention / alerting path so a local refresh that finds zero `driver=` tags is visible without reading the log. _(Unfiled.)_

4. **Handle hosts with no `drivers-probed` marker.** `anaplian` carries none. Re-enabling it without an explicit `bossctl hosts probe` puts it straight into the same silent hold. Surface: `tools/boss/engine/core/src/host_capability_probe.rs` `DRIVERS_PROBED_CAPABILITY` (`:93-99`) and `discover_remote_driver_capabilities` (`:176-196`); `tools/boss/bossctl/src/hosts.rs` (`bossctl hosts probe`). _(Covered in the AI-1 fix brief.)_

5. **Investigate interleaved writes in the dispatch event stream.** Twenty-five multi-record lines, all recoverable, all confined to a rotated segment — not a cause here, but a real defect in the writer. Surface: `tools/boss/engine/dispatch-events/src/lib.rs` `JsonlFileSink` emit (`:1175-1206`) and `tools/boss/engine/jsonl-append/src/lib.rs` (the append path that exists to serialize concurrent writes). _(Unfiled.)_

## 9. What went well

- **The gate failed closed.** An empty driver inventory produced zero dispatch, not a worker launched against a missing binary. That is the specified behaviour of [mono#2753](https://github.com/spinyfin/mono/pull/2753), and it is the reason this was a thirteen-minute stall rather than a string of spawn failures with a false "driver is installed" claim.
- **The engine already emitted the diagnosis**, in one sentence, five seconds after start. The line names both the empty inventory and the consequence (`driver-constrained dispatch will hold`). Instrumentation for detection is present; only the last step — something that reads it — is missing.
- **Mitigation was applied only after verifying each binary through a login shell.** The three `source='user'` tags do not claim anything the pane cannot actually launch.
- **tmux was correctly ruled out** rather than chased. The PATH override and the pid/start-time evidence close that hypothesis; the tempting coincidence did not consume the investigation.
- **Root cause was identified in about eight minutes** after the first `host_selected/error` (~18:07:26 → ~18:15 CDT, **estimated**), and dispatch was restored three minutes after that.

## 10. What went badly

- A review revision that closed a real hazard (ambient-PATH rewrite via `bossctl`) modelled the pane launcher's environment incorrectly, and that incorrect model became the production probe.
- The only test for the producer asserted the property that was never in doubt (no fabricated drivers) and did not assert the property the gate depends on (a launchable driver is a discovered driver).
- The gate and its producer shipped in one PR, so production was the first joint test.
- The warning that named the outage went to a log file. Detection depended on a human watching two cards, roughly twelve minutes later.
- Restart — the obvious first instinct — would have reproduced the empty inventory. That is worth knowing before the next one, and it was not obvious from the warning text alone.
- The mitigation that restored dispatch is itself a fail-open that masks the bug and will outlive an uninstall. It must be removed the moment AI-1 ships; until then the fix cannot be verified on this machine.

## 11. Lessons

1. **A probe that models a different environment than the launcher is answering the wrong question.** Sharing one code path is the only way the two cannot drift again; matching them by coincidence (same `PATH` string, same `command -v`) is how this shipped.
2. **A test that only guards false positives cannot catch a producer that returns false negatives for everything.** The positive direction — "a driver a worker can launch is a driver the probe finds" — is the property the gate actually needs.
3. **Shipping a gate with its unexercised producer makes first production the first test.** Split them, or soak the producer against the real launch path before the gate starts failing closed.
4. **Recording a diagnosis is not surfacing it.** The engine wrote down exactly what was wrong, in a known file, by design, once — and it cost thirteen minutes anyway, because no client reads that line. Same lesson as incident 003, different log.
5. **User-sourced overrides that survive re-probe are a fail-open.** They are the right tool for an emergency assertion and the wrong tool to leave in place, because they keep claiming a driver after it is gone and they hide the producer from verification.
6. **Do not assume restart helps when discovery is deterministic.** A `OnceLock` cache and a probe that will give the same answer are a restart that changes nothing.

## 12. Follow-up code changes

This document is **doc-only**. It describes the 2026-08-18 outage and recommends work; it does not change engine, probe, or placement code. The probe/pane resolution fix (AI-1) is already filed as its own chore and is deliberately not duplicated here. Removing the three `source='user'` tags (AI-2) is an operational step that must follow that fix. Alerting on the driver-hold warning (AI-3) and the interleaved dispatch-event writes (AI-5) should be filed as separate chores against the host-registry and dispatch-event writer surfaces.
