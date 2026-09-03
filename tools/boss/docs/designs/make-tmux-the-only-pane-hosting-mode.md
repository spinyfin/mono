# Tmux-only local worker panes after automatic-recovery parity

- **Date:** 2026-09-02
- **Status:** design proposal
- **Project:** Make tmux the only pane hosting mode
- **Provenance:** project-design execution; no implementation code
- **Related design:** [Workers outlive their supervisor](./run-agents-and-the-coordinator-in-tmux-so-work-survives-app-and-engine-restarts.md)
- **Related remote-worker design:** [Distributed agent execution over SSH](./distributed-agent-execution-register-and-dispatch-to-remote-ssh-hosts.md)
- **Related public work:** [original tmux design PR](https://github.com/spinyfin/mono/pull/2637); [open quit-dialog correction](https://github.com/spinyfin/mono/pull/2840)

The contested property is that deleting the local app-hosted fallback is safe only after tmux-hosted workers can automatically recover from a genuine wedge and that recovery has survived a full-release soak. The current tmux path does not meet that bar, so recovery repair is a prerequisite to deletion, not post-migration cleanup.

## TL;DR

Make tmux mandatory for every **local worker pane** and delete the app-owned pty path after a staged confidence gate. Remote SSH workers remain on their already-detached SSH lifecycle, `workers.tmux_hosting` is removed after one rollback-capable release, and stale copies of that setting fail with an actionable error rather than being ignored.

The recovery gate uses durable driver-originated progress, not terminal repaint activity or process titles. A semantically quiet worker raises an attention after 30 minutes and is automatically orphaned, token-verified, reaped, released, and redispatched after two hours; an unavailable identity probe is loud and non-destructive.

## Goals

- Make a Boss-owned tmux session the only mechanism that hosts a local worker process.
- Delete the app-hosted spawn, process-ownership, input, teardown, startup-recovery, status, settings, and UI branches that exist only for local dual mode.
- Restore automatic recovery from wedged local workers before removing the current fallback.
- Preserve exact-token adoption and teardown as the authority for local worker identity.
- Make transition behavior safe for workers spawned under an older hosting mode.
- Leave no disabled app-hosted code path, no silent fallback, and no setting that suggests the old path still exists.
- Make the final architecture mechanically verifiable so a later edit cannot quietly reintroduce app-owned worker ptys.

## Non-goals

- Migrating remote SSH workers into tmux. They do not use the local app-pane path and already survive engine restarts through `remote_reattach` and remote pid reconciliation. An existing deferred work item owns convergence with the tmux model, so this design does not create a duplicate task.
- Changing the coordinator hosting model. The coordinator is already unconditionally tmux-hosted.
- Replacing driver hook or rollout progress with terminal scraping. Tmux provides process identity and a pty; driver-owned events provide semantic progress.
- Making pane scrollback the canonical transcript. Driver transcripts and rollout JSONL remain the durable record.
- Surviving machine reboot or loss of the tmux server.
- Introducing a headless worker mode or a second terminal multiplexer.
- Retaining an unreachable app-hosted implementation behind a disabled flag for emergency use.

## Current state and findings

This inventory was re-verified against `main` at commit `1a722570` on 2026-09-02. The configured installation already enables tmux for all three local pools, but the repository default remains app-hosted and every dispatch still selects one of two paths.

### The known re-adoption repair is not on `main`

`tools/boss/engine/core/src/work/run_rows.rs` still defines `TMUX_RUN_ADOPTABLE_PREDICATE` with `r.status = 'active'`. A healthy long-lived worker normally has a completed run row, so periodic adoption can still displace its live state. The already-dispatched repair for that defect also owns preserving `held`, `activity`, and `live_status`, plus honest unknown activity in the app.

This project must build on that work. It is an external prerequisite to the first implementation entry below and is deliberately not restated as a new entry.

### Automatic recovery is asymmetric

For an app-hosted worker, `stale_worker_sweep` eventually marks the execution orphaned, backs up its workspace, tears down driver and process state, releases the slot and lease, and kicks dispatch. The work self-heals.

For a tmux-hosted worker, the same high-level symptom reaches `AliveAndGenuinelyStuck`, files an attention, and stops. No long-window reap was implemented, even though the original design explicitly required one.

The tmux stuck classifier is also inert for attached agent TUIs:

- `#{window_activity}` follows display repaint. Claude's spinner advances it continuously even when semantic work is stuck.
- `#{pane_current_command}` is a presentation field, not stable process identity. Claude publishes a version string as its process title.
- A failed tmux probe skips the cadence path entirely.
- `terminal_probe_failed` and `foreground_command_mismatch` are normally invisible because the pass is only logged when it reaps something.

This refutes the original design's detached-shell validation of `window_activity`; that measurement did not reproduce an attached, continuously repainting TUI. It validated a mechanism under a different workload and cannot stand as evidence for worker progress.

### Process-tree activity is not a substitute

`tools/boss/engine/core/src/background_children.rs` records that a process-table classifier was already tried and removed. Claude and Codex helpers, ordinary tool subprocesses, and delegated work can create indistinguishable descendant/process-group shapes.

The load-bearing property is therefore **driver-originated semantic progress**, not output bytes, process-title text, CPU use, or the existence of descendants. Tmux remains authoritative for “is this the exact process container Boss created?”; it is not authoritative for “is the agent making useful progress?”.

### The transition currently consults the wrong fact

`app/readoption.rs` builds `tmux_hosted_ids` from the current pool setting. A run spawned app-hosted can survive until an engine restart after its pool is flipped to tmux; the restart may then treat a missing tmux adoption as proof that no pane exists and spawn a duplicate worker.

Hosting identity is a per-run historical fact. During mixed mode it must be read from the latest local run's durable `tmux_session_name`/spawn-token record, the same dimension on which `tmux_run_for_execution` already reasons. Current policy only decides how the **next** run is spawned.

### Worker exit-state handling contradicts the accepted design

The coordinator sets `remain-on-exit`; worker session creation does not. A single-window worker session therefore disappears when its pane exits, making the worker `PaneExited` state and its exit-status documentation unreachable.

There is no recorded worker-specific decision that explains this difference. Silence is not evidence of intent, so this design treats it as an implementation gap: workers will set `remain-on-exit=on`, and token-verified cleanup will remove the retained dead pane after observing its status.

### Scrollback persistence was never specified

Boss does not set `history-limit`; the measured live value is tmux's default 2,000 lines. The original claim that scrollback survives an app quit is true only within that bound, and no recorded decision chose a different bound.

This design makes the existing bound explicit rather than implying unlimited durability. Boss will establish `history-limit=2000` before creating the first window on its private server. The driver transcript remains the complete record; tmux history is a bounded diagnostic convenience.

### Complete dual-mode inventory

| Area                          | Current dual-mode surface                                                                                                                                                                                                                              | Tmux-only end state                                                                                                                               |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| Configuration                 | `settings.rs` models `TmuxHostingPools`; `engine_meta.rs` exposes a boolean projection; `dispatch_hosting_stamp.rs`, `bossctl doctor`, `ChatViewModel`, `SettingsView`, and `WorkersDetailView` expose the selected mode.                              | Delete the selection and rollout-only visibility. A stale key is an actionable configuration error.                                               |
| Dispatch and spawn            | `runner/pane_spawn.rs` branches through `tmux_hosting_enabled_for`; `spawn_flow.rs` accepts an optional `TmuxWorkerHost` and either creates a tmux session or asks the app to spawn a shell.                                                           | A local spawn requires a tmux host and records intent before process creation. Tmux failure fails dispatch loudly.                                |
| Engine/app lifecycle protocol | `SpawnWorkerPane`, `ReleaseWorkerPane`, `UpdateWorkerShellPid`, `WorkerPaneDied`, and `ReportWorkerSpawnFailed` exist for app-owned processes alongside attach/detach.                                                                                 | Keep attach/detach for the viewer; delete app-owned process lifecycle messages and callbacks.                                                     |
| macOS worker host             | `WorkersWorkspaceModel` can either create the worker command/env/cwd and kill its foreground process tree, or attach to tmux and detach without killing.                                                                                               | It only attaches a Ghostty viewer to an existing session and detaches that viewer.                                                                |
| Registry and teardown         | `WorkerRegistry` records `tmux_hosted`; `app.rs` chooses detach versus release and process-group reap versus token-verified tmux reap; `tmux_teardown.rs` has a `NotTmuxHosted` outcome.                                                               | Local registry entries need no hosting-mode bit. Every local teardown detaches the viewer and uses token-verified tmux teardown.                  |
| Pane input                    | `pane_delivery.rs`, `pane_ops.rs`, and `probe_interrupt.rs` use tmux when identity exists and app RPC otherwise.                                                                                                                                       | Local delivery, probe, and interrupt require a tmux identity; missing identity is an explicit invariant failure, not a fallback.                  |
| Startup and readoption        | `app/readoption.rs`, `startup_pane_reconcile.rs`, and app-session retry logic combine tmux adoption with `ListHostedPanes` and boot-only app-hosted adoption.                                                                                          | Tmux inventory plus durable run identity decide process presence. App inventory may describe viewers, never worker-process liveness.              |
| Sweep/recovery                | `stale_worker_sweep.rs` has separate tmux and cadence behavior; `dead_pane_sweep.rs`, `husk_pane_sweep.rs`, `spawn_ack_sweep.rs`, `dead_pid_sweep.rs`, `lost_workspace_sweep.rs`, and `transient_recovery.rs` retain app-pane assumptions or branches. | Preserve durable death, driver-start, and tmux-husk checks; delete only the app-owned-surface evidence and asynchronous app-spawn cases.          |
| Status                        | `TmuxAdoptionState::NotTmuxHosted`, `app/panes.rs`, `Models+WorkerActivity.swift`, `PlannerAffordances.swift`, and `bossctl agents` can mean either remote or legacy local.                                                                            | `NotTmuxHosted` remains solely for detached remote workers. A local run without tmux identity is missing/invalid, never a supported hosting mode. |
| Tests and docs                | Fixtures pin both spawn modes, app kill behavior, app-hosted husks, current-setting startup decisions, unreachable worker `PaneExited`, and unbounded-sounding scrollback claims.                                                                      | Remove superseded fixtures and update the premise and every downstream assertion in the same PR that changes it.                                  |

The protocol/glue part of that inventory includes `protocol/src/engine_app.rs` and `protocol/src/wire.rs`; `app/sessions.rs`, `app/server.rs`, and `ipc_log.rs`; and the macOS `ChatViewModel+BossSession.swift`, `EngineClient+Requests.swift`, `EngineClient+PaneResponses.swift`, `EngineEvent.swift`, `EngineProtocolTypes.swift`, `ContentView.swift`, `TerminalPaneSession.swift`, `GhosttyTerminalView.swift`, and `WorkersWorkspaceModel.swift` surfaces. `spawn_health.rs`, `execution_liveness.rs`, and `host_registry.rs` also carry app-spawn assumptions even where they do not select a mode. These are part of the deletion sweep, not unowned incidental references.

The tmux columns on `work_runs` remain nullable because remote runs legitimately do not use them and historical local rows predate them. Nullable storage does not make local absence a supported runtime mode; host kind plus execution lifecycle defines that invariant.

## Alternatives considered

### Keep dual mode indefinitely

This preserves the current emergency switch and avoids a deletion migration. It was appropriate for the original staged rollout, whose documented precedent explicitly required per-pool enablement and visible rollback while tmux was young.

It is not appropriate as the end state. The mixed-mode startup bug demonstrates that every reconciler must know whether it is reading present policy or historical run identity, and the app lifecycle protocol continues to encode two incompatible ownership models. Keeping the branch would fail the project's explicit goal and make future recovery changes prove correctness twice.

### Delete the fallback immediately and repair recovery later

This reaches the smaller code shape fastest. It is rejected by an observable behavioral regression: the fallback is presently the only path that automatically recovers a live-but-wedged worker, while the tmux classifier cannot reach its own stuck state under the attached Claude TUI.

A gate after deletion would have no authority over the deletion it was supposed to protect. Recovery and a genuine end-to-end exercise must land first; the later soak can then block deletion.

### Require remote workers to adopt tmux before declaring “tmux only”

This yields one lifecycle mechanism across hosts and would eventually allow remote attachment. It is rejected as a prerequisite because remote SSH workers are not hosted in local app panes and already use a distinct detached transport with `remote_reattach`, remote pid probing, and remote lease reconciliation.

The scope boundary is checkable: `host_id = 'local'` requires tmux; `host_id != 'local'` follows the SSH adapter. The existing deferred remote-tmux work remains valid and is not duplicated here.

### Treat captured terminal output as progress

Hashing `capture-pane`, watching `window_activity`, or normalizing known spinner rows would avoid a database write on each driver event. It is rejected because display equivalence is not semantic-progress equivalence: a repainting spinner changes the screen while work is stuck, and a quiet but valid model response can leave it unchanged while work advances.

Driver-specific screen normalization would also turn every CLI release into a liveness-parser compatibility event. Existing practice already has a semantic event layer for Claude hooks and Codex/Grok rollout events, so the terminal should not duplicate it with weaker evidence.

### Keep `workers.tmux_hosting` as a permanent deprecated no-op

This avoids breaking stale settings files. It is rejected because a control that appears accepted but cannot change behavior is operationally dishonest, and it retains a named artifact of the dual-mode world indefinitely.

The chosen transition keeps the setting functional for one rollback-capable release, then removes it. A later binary rejects the stale key with the remediation text “remove `workers.tmux_hosting`; local workers are always tmux-hosted.”

## Chosen approach

### End-state invariants

1. Every local worker process is created by the engine in a Boss-owned tmux session. The app never creates or owns that process's pty.
2. The spawn token is durably recorded before `new-session`; adoption and teardown require an exact token read from the live session.
3. The app may attach or detach a viewer without affecting worker lifetime.
4. Driver-originated semantic progress is durable per run. Engine-synthesized display timestamps cannot postpone or trigger automatic recovery.
5. A local driver is dispatchable only when it supplies rich per-tool progress boundaries. A driver without that capability fails local dispatch loudly rather than silently losing automatic wedge recovery.
6. A missing or failed tmux probe is never proof of health and never proof of death. It raises observable degraded evidence and blocks destructive action until exact identity can be re-established.
7. Remote SSH workers remain valid `NotTmuxHosted` workers. Local workers do not.
8. There is no runtime hosting-mode setting or fallback after the deletion phase.

Claude, Codex, and Grok all declare `ProgressFidelity::Rich` at current `main`, so the local-driver recovery requirement does not exclude a supported driver today. It turns a future lower-fidelity local driver into an explicit design decision instead of silently weakening recovery.

### Phase 1: repair durable recovery while dual mode still exists

Persist a semantic progress checkpoint on the local run: the last **driver-originated** event time and a tri-state tool condition (`in_flight`, `idle`, `unknown`). Update it at the same ingress boundary that updates `LiveWorkerState`, and seed re-adopted state from it. “Unknown” must never be coerced to “idle”; legacy rows remain non-destructive until a real event establishes their state.

This is intentionally stronger than persisting `LiveWorkerState.last_event_at`. That display field is also written by engine inference such as spawn-stall handling. Persisting the container rather than the load-bearing property would let an engine-generated timestamp masquerade as agent progress.

Fix transition reconciliation in the same pre-deletion phase, but in a separate PR because it has a distinct failure mode. `startup_pane_reconcile` must decide whether an existing run was tmux-hosted from its durable local run record, not the setting that would govern a new dispatch.

Worker session creation also becomes internally consistent with the accepted tmux design: establish the private server's explicit 2,000-line history limit before the first window, set `remain-on-exit=on` for worker sessions, and retain dead panes only until token-verified reconciliation records the exit status and kills the session.

### Phase 2: replace the stuck classifier and add bounded auto-reap

The stale-worker decision becomes a two-threshold state machine over semantic evidence:

| Condition                                                                               | Before 30 minutes                    | 30 minutes to 2 hours                                                  | At or beyond 2 hours                                                                     |
| --------------------------------------------------------------------------------------- | ------------------------------------ | ---------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| Recent driver event, tool in flight, non-working activity, or operator hold             | Healthy/ignored                      | Healthy/ignored; resolve prior attention                               | Healthy/ignored; resolve prior attention                                                 |
| Rich driver, `Working`, tool known idle, semantic event stale, exact live tmux identity | No action                            | Raise/update `stale_worker` attention with attach command and evidence | Re-probe exact identity, back up work, mark orphaned, tear down, release, and redispatch |
| Tool state unknown or progress fidelity below Rich                                      | No destructive action                | Raise degraded-evidence attention                                      | Continue attention; local dispatch of newly unsupported drivers is refused               |
| Tmux probe unavailable                                                                  | No health inference                  | Raise probe-unavailable attention and record the cadence result        | Do not reap until a later pass verifies exact identity                                   |
| Session absent, token mismatched, or retained pane dead                                 | Existing corroborated death handling | Existing corroborated death handling                                   | Existing corroborated death handling                                                     |

`window_activity` and `pane_current_command` remain available in diagnostics because they can help an operator inspect a session. They are removed from the boolean decision and cannot reset either threshold.

The two-hour value is four times the current 30-minute alert threshold. It preserves a long inspection interval and sharply reduces false-positive risk while still bounding a slot that would otherwise remain wedged forever. The clock is the last semantic event time, so it survives engine restart; the alert item's creation time is not the authority.

Immediately before automatic recovery the sweep must verify all destructive preconditions again: same non-terminal execution, no hold, `Working`, tool known idle, stale durable semantic timestamp, supported fidelity, live session, matching token, and live pane. It then uses the existing recovery sequence in order: recovery backup; orphan mark and audit; driver workspace teardown; token-verified worker/session reap; pool-slot and cube-lease release; coordinator kick. Release never precedes process teardown.

Probe errors and classifier counters become observable independently of reaping. A pass with a stuck candidate, probe failure, identity mismatch, or auto-reap emits structured dispatch telemetry and an info/warn summary even when `reaped == 0`.

### Phase 3: exercise the genuine path and roll out tmux by default

Unit classifiers are necessary but insufficient. Before changing the default, a Bazel integration target must drive the production local spawn, private tmux server, adoption, stale sweep, token-verified teardown, pool release, lease release, and redispatch path with shortened clocks. It must include an attached continuously repainting fixture and a process title that differs from the driver binary, proving those presentation changes do not count as progress.

The confidence gate also requires a genuine supported-driver drill rather than relying only on the fixture: launch isolated Boss through the real app/engine path, run at least Claude and Codex in tmux, restart the engine mid-turn, restart the app mid-turn, observe normal exit with `pane_dead` and exit status, and induce one controlled stale-idle recovery with test thresholds. The validation PR records commands, versions, and outcomes in a repository document; screenshots may be attached to the work item but are not the evidence record.

Once those checks pass, make all local pools default to tmux while preserving the setting as a functional, visible rollback control for one stable release. The mixed-mode fix has already landed at this point, so an opt-out during that release cannot create a duplicate-worker startup window.

### Confidence gate before deletion

No deletion entry may begin until a stable Boss release has run all local pools tmux-hosted for seven consecutive days and accumulated at least 50 terminal local executions, including at least five from each of review, automation, and interactive pools. The gate additionally requires:

- successful real engine-restart and app-restart drills with work continuing;
- successful two-threshold automatic recovery and redispatch through the genuine path;
- successful normal-exit observation through retained `pane_dead` state;
- no duplicate-worker incident;
- no unexplained token mismatch or tmux probe failure;
- no leaked session after terminal reconciliation;
- the known re-adoption repair present and no periodic live-state displacement;
- the quit-dialog correction either merged or forward-ported before its conditional wording is removed.

The gate is recorded in a dated repository report and reviewed as its own PR. This is a validation gate for the chosen tmux-only design, not a comparison study between tmux and other hosts.

### Phase 4: delete the app-hosted implementation

Deletion proceeds from process ownership outward:

1. Make the engine's local spawn and teardown types tmux-only, so no caller can construct an app-hosted local worker.
2. Delete the app-owned worker lifecycle RPCs, callbacks, and process killer.
3. Reduce startup, death, spawn-ack, and husk reconciliation to durable/tmux evidence, retaining the protections whose premise still exists.
4. Delete app-mediated input and interrupt fallbacks; a missing local tmux identity is an invariant error.
5. Narrow status semantics so `NotTmuxHosted` means remote SSH only.
6. Remove rollout UI, CLI, event stamps, and finally `workers.tmux_hosting` itself.
7. Sweep source, tests, and docs and land a regression check that proves there is no reachable local app-hosted spawn path.

Historical local rows with null tmux columns are migration input, not a supported mode. On startup, a provably dead row is orphaned with an audit reason and redispatched through tmux. A row with a live pid or indeterminate evidence is quarantined: local dispatch pauses, an attention explains that an older app-hosted worker may still exist, and the operator rolls back to the prior release to stop/drain it before retrying the tmux-only upgrade. The new build never reattaches it, reaps it on ambiguous pid identity, or redispatches over it, and no code can spawn a new null-identity local run.

### Setting removal behavior

The setting has two explicit stages:

- **Rollback-capable release:** default all pools to tmux; continue accepting `workers.tmux_hosting` and keep its effect visible. Operators may turn it off only while the fallback still exists.
- **Tmux-only release:** remove the type, setter, getter, UI, doctor row, event stamp, and spawn branch. If the flattened settings parser encounters the old key, return a hard error naming the key and telling the operator to remove it. Do not silently ignore it and do not accept it as a no-op.

The hard error is chosen over a warning because a user who believes `[]` still selects app hosting would otherwise start workers under a materially different ownership model. One functional deprecation release gives normal upgrades a rollback window; direct upgrades that skip it receive an explicit remediation instead of surprising behavior.

### Rollback after deletion

After deletion, rollback is a binary rollback, not a hidden runtime mode:

1. Pause dispatch so no new local workers start while the host is suspect.
2. Inspect or stop affected sessions through token-verified tmux controls.
3. Install the previous stable Boss bundle, whose schema remains compatible with the additive semantic-progress columns.
4. If necessary, use that previous release's still-functional setting to spawn new work app-hosted while existing tmux sessions drain by durable identity.

Keep the previous stable artifact available for at least the first tmux-only release. The deletion phase must not bump the tmux session schema in a way that makes the rollback build reject sessions created by the new build; if an unrelated schema change is required, it waits until this rollback window closes.

Tmux absence or regression in the new build fails local dispatch loudly and raises engine health. It never invokes the deleted app path.

## Risks / open questions

There are no unresolved scope forks in this proposal. Reviewers are asked to affirm the decisions below because they are the parts most likely to merit disagreement before implementation starts.

- **Two-hour recovery may be too slow or too aggressive.** It is deliberately four times the existing alert threshold and requires a second exact-identity check. Telemetry from the rollback-capable release can justify changing the constant before deletion, but deletion does not proceed without some finite auto-reap bound.
- **Per-event persistence adds write load.** The write rate follows semantic driver events, not TUI frames. Implementation should coalesce redundant timestamp/tool-state writes in the ingress transaction without weakening the property that a completed tool boundary is durable before later adoption relies on it.
- **`remain-on-exit` retains resources after an unobserved worker exit.** The existing one-session-per-worker model, two-pass husk protection, token verification, and periodic dead-pane reconciliation bound that pressure. The confidence gate explicitly checks cleanup.
- **A 2,000-line history may be too small for manual debugging.** It is an intentional bounded convenience, not transcript retention. Raising it later is an independent resource/UX decision and does not affect tmux-only correctness.
- **Hard-erroring a skipped-version configuration is disruptive.** That disruption is preferable to silently overriding an operator's explicit `[]`. The error is local, actionable, and preceded by a full functional deprecation release.
- **Remote status can be mistaken for a local exception.** Protocol docs, CLI labels, and app rendering must say “remote detached” where host context is known, even though the serialized enum variant remains `NotTmuxHosted` for compatibility.
- **A fixture cannot validate third-party TUI integration by itself.** That is why the gate requires both a repeatable Bazel integration target and real isolated Claude/Codex drills through the shipped path.

## Proposed implementation task breakdown

No deferred entry is created for remote tmux migration because an existing deferred work item already owns it; duplicating it would create two schedulable rows for the same work. The open quit-dialog PR is likewise consumed as an external prerequisite, not re-filed.

Breakdown size: 14 entries (14 in-scope, 0 deferred) — the change has five real phases (durable recovery, classifier/reap, genuine rollout validation, engine/app/protocol deletion, and a final invariant sweep) across the engine, protocol, macOS app, CLI, and documentation surfaces.

### Persist the semantic worker-progress checkpoint

Scope description: Add an additive per-run checkpoint for the last driver-originated progress time and tri-state tool condition, update it at the shared progress-ingress boundary, and seed tmux re-adoption from it without treating engine-synthesized display timestamps as progress. Include migration, query, ingress, restart, and legacy-null tests. This starts only after the already-dispatched re-adoption/state-preservation repair lands and must forward-port that work.

Effort hint: `large`

Dependencies: none

Scope: in-scope

Parallelism: May run in parallel with **Preserve worker exit state and bound scrollback**; their production file sets are distinct.

### Key mixed-mode startup recovery to durable run identity

Scope description: Change `app/readoption.rs` and `startup_pane_reconcile.rs` so pane-hosting history comes from the latest local run's durable tmux identity, never current pool settings. Add restart tests for app-hosted→tmux and tmux→app-hosted flips that prove an existing pane is neither duplicated nor stranded during the rollback-capable release.

Effort hint: `medium`

Dependencies: Persist the semantic worker-progress checkpoint

Scope: in-scope

Parallelism: May run in parallel with **Replace the inert tmux stuck classifier** after their shared prerequisite; the former edits startup/readoption while the latter edits the stale sweep.

### Preserve worker exit state and bound scrollback

Scope description: Establish the Boss private server's `history-limit=2000` before creating its first window, set `remain-on-exit=on` on worker sessions, verify `pane_dead`/`pane_dead_status` through the real tmux wrapper, and ensure dead sessions are removed by token-verified reconciliation. Correct the original tmux design and status docs so scrollback is explicitly bounded and the worker `PaneExited` arm is reachable.

Effort hint: `medium`

Dependencies: none

Scope: in-scope

Parallelism: May run in parallel with **Persist the semantic worker-progress checkpoint**; it is ordered before the recovery integration test, not before the independent schema work.

### Replace the inert tmux stuck classifier

Scope description: Rewrite `stale_worker_sweep.rs` so driver-originated progress, known-idle tool state, activity, holds, and fidelity decide semantic staleness; tmux decides exact identity and death only. Remove `window_activity` and `pane_current_command` as health vetoes, let probe failures enter the non-destructive cadence/attention path, require Rich progress for local dispatch, and make all degraded counters/events observable even when no reap occurs.

Effort hint: `medium`

Dependencies: Persist the semantic worker-progress checkpoint

Scope: in-scope

Parallelism: May run in parallel with **Key mixed-mode startup recovery to durable run identity**; no substantial file overlap is expected.

### Add the two-hour token-verified auto-reap

Scope description: Add the second stale threshold and its destructive recheck to `stale_worker_sweep.rs`. On a two-hour rich/working/known-idle candidate with exact live session and token, run recovery backup, orphan/audit, driver teardown, token-verified tmux reap, slot and lease release, and coordinator kick in that order; prove holds, new events, in-flight tools, unknown tool state, probe errors, and identity changes block the reap.

Effort hint: `large`

Dependencies: Replace the inert tmux stuck classifier

Scope: in-scope

Parallelism: None; this deliberately follows the classifier PR because both substantially edit `stale_worker_sweep.rs`, and it must forward-port the classifier tests preservingly.

### Exercise recovery through the production tmux path

Scope description: Add a Bazel integration target that drives local spawn, private tmux, adoption, stale classification, teardown, resource release, and redispatch with injectable short thresholds, including an attached repainting fixture and changed process title. Run and record isolated real-app Claude and Codex drills for app restart, engine restart, retained exit status, and controlled auto-recovery in a repository validation document.

Effort hint: `large`

Dependencies: Key mixed-mode startup recovery to durable run identity; Preserve worker exit state and bound scrollback; Add the two-hour token-verified auto-reap

Scope: in-scope

Parallelism: None; it validates the integrated behavior of all three prerequisites.

### Default every local pool to tmux for one rollback-capable release

Scope description: Change `workers.tmux_hosting` defaults to all local pools while retaining the setting's real opt-out behavior, visibility, and per-run durable teardown during this release only. Update settings tests and release-facing copy to mark the control deprecated and scheduled for removal; do not delete or no-op it yet.

Effort hint: `small`

Dependencies: Exercise recovery through the production tmux path

Scope: in-scope

Parallelism: None; this is the rollout boundary.

### Record the all-pool tmux confidence gate

Scope description: After a stable release has accumulated seven consecutive days and the stated execution-volume floor, commit a dated repository report covering restart drills, controlled auto-recovery, retained normal-exit status, session cleanup, re-adoption stability, duplicate-worker absence, and probe/token telemetry. A failed criterion leaves the deletion tasks blocked and records the observed failure rather than relaxing the gate.

Effort hint: `medium`

Dependencies: Default every local pool to tmux for one rollback-capable release

Scope: in-scope

Parallelism: None; the elapsed production exposure is the evidence this task records.

### Collapse local engine spawn and teardown to tmux only

Scope description: In `runner/pane_spawn.rs`, `spawn_flow.rs`, `worker_registry.rs`, `app.rs`, and `app/tmux_teardown.rs`, make `TmuxWorkerHost` required for local spawn; delete `tmux_hosting_enabled_for`, the optional-host/app-spawn arm, the `tmux_hosted` registry bit, detach-versus-release selection, and `TmuxTeardownOutcome::NotTmuxHosted` for local teardown. Tmux failure must return an explicit local dispatch failure and never call the app-hosted path. Add fail-closed startup quarantine for a nonterminal historical local row with null tmux identity: only proof of death permits orphan/redispatch; live or unknown evidence pauses local dispatch for rollback/drain.

Effort hint: `large`

Dependencies: Record the all-pool tmux confidence gate

Scope: in-scope

Parallelism: None; this is the first fallback deletion and the chokepoint for all later deletion tasks.

### Delete app-owned worker lifecycle RPCs and process killing

Scope description: Remove `SpawnWorkerPane`, `ReleaseWorkerPane`, `UpdateWorkerShellPid`, `WorkerPaneDied`, and `ReportWorkerSpawnFailed` from `boss-protocol`, engine dispatch/session handlers, macOS protocol decoding, `WorkersWorkspaceModel`, and tests. Delete worker command/env/cwd launch and `WorkerProcessKiller` use while preserving `AttachWorkerPane`/`DetachWorkerPane` as viewer-only operations.

Effort hint: `large`

Dependencies: Collapse local engine spawn and teardown to tmux only

Scope: in-scope

Parallelism: May run in parallel with **Remove the hosting setting and rollout-only surfaces** after the engine chokepoint lands; the former owns lifecycle protocol/app host files while the latter owns settings/UI/doctor files.

### Delete app-hosted startup, death, spawn-ack, and husk branches

Scope description: Delete boot-only app-pane adoption from `app/readoption.rs`, reduce `startup_pane_reconcile.rs` to durable tmux presence, remove `ListHostedPanes` as a worker-process oracle, delete the app-pane half of `husk_pane_sweep.rs`, and remove asynchronous-app-surface cases from `spawn_ack_sweep.rs`, `dead_pane_sweep.rs`, `dead_pid_sweep.rs`, `lost_workspace_sweep.rs`, and `transient_recovery.rs`. Preserve tmux husk confirmation, durable pid corroboration, driver-start verification, backup-before-orphan, and lease ordering.

Effort hint: `large`

Dependencies: Delete app-owned worker lifecycle RPCs and process killing

Scope: in-scope

Parallelism: None; this task and its predecessor both edit the engine/app protocol inventory. Land lifecycle deletion first and forward-port it preservingly.

### Delete app-mediated worker input and narrow hosting status

Scope description: Remove the app fallback from `pane_delivery.rs`, `pane_ops.rs`, and `probe_interrupt.rs`, then delete the worker `SendToPane`/`InterruptWorkerPane` protocol and macOS handlers while retaining viewer focus. In `app/panes.rs`, `tmux_worker_status.rs`, `bossctl agents`, `Models+WorkerActivity.swift`, and `PlannerAffordances.swift`, make local missing identity an explicit unavailable/error state and retain `NotTmuxHosted` only for remote SSH workers, building on the separate honest-unknown activity repair.

Effort hint: `medium`

Dependencies: Delete app-hosted startup, death, spawn-ack, and husk branches

Scope: in-scope

Parallelism: None; it follows the preceding protocol deletion to avoid concurrent edits to shared request/response enums and must integrate rather than restore removed variants.

### Remove the hosting setting and rollout-only surfaces

Scope description: Delete `TmuxHostingPools`, its getters/setters/snapshot, `dispatch_hosting_stamp.rs`, the `engine_meta.rs` special case, the Settings toggle, Workers-grid legacy badge, `ChatViewModel` mode projection, and the `bossctl doctor` pool report. Add an actionable hard error when `settings.toml` still contains `workers.tmux_hosting`. Start only after the open quit-dialog correction lands, then remove its now-unnecessary hosting conditional while preserving the corrected unconditional wording; do not recreate that PR's fix.

Effort hint: `medium`

Dependencies: Collapse local engine spawn and teardown to tmux only

Scope: in-scope

Parallelism: May run in parallel with **Delete app-owned worker lifecycle RPCs and process killing**. If incidental `ChatViewModel` overlap appears, the later PR must forward-port the earlier removal and may not restore obsolete callbacks.

### Enforce and verify the tmux-only local-pane invariant

Scope description: Sweep source, tests, fixtures, and operational/design docs for reachable app-hosted local spawn, release, input, startup, and status assumptions; delete residual dead branches and update every premise-dependent assertion. Add a focused architecture regression that fails if a local spawn can be constructed without durable tmux identity or if removed RPC/setting identifiers re-enter production surfaces, while explicitly allowing the separate remote detached path.

Effort hint: `medium`

Dependencies: Delete app-hosted startup, death, spawn-ack, and husk branches; Delete app-mediated worker input and narrow hosting status; Remove the hosting setting and rollout-only surfaces

Scope: in-scope

Parallelism: None; this is the final repository-wide verification sweep after every deletion seam has landed.
