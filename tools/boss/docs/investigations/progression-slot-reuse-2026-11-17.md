# Progression slot reuse during spawn cleanup

The November 17 incident exposed an ownership race in cleanup and an incomplete viewer slot range. A deterministic regression reproduces cleanup freeing a replacement progression worker's slot. Releases now compare the execution identity under the pool mutex, and delayed viewer cleanup only removes the released run's live state and status task.

Source paths below are relative to `tools/boss/`.

## Why several live runs could use slot 41

The allocator does not intentionally share a slot. `claim_progression_worker` checks and assigns claims under one mutex (`engine/execution-contracts/src/progression_pool.rs:8`). The mapping puts `progression-1` at 41 and the other progression workers at 42–44 (`engine/execution-contracts/src/lib.rs:1124`).

The failure is a release after reuse:

1. Spawn confirmation fails and `PaneSpawnRunner` awaits pane teardown (`engine/execution-runner/src/runner/pane_spawn.rs:1523`).
2. `ServerState::release_worker_pane` releases the pool claim and kicks dispatch (`engine/app/src/app.rs:2344`). Another progression run can claim the freed slot immediately.
3. The original spawn returns its error. `run_progression_execution` finishes failure bookkeeping and releases the same worker ID again (`engine/dispatch/src/coordinator/progression_spawn.rs:325`). Before the fix, this release did not check which execution owned the claim.
4. A third run can now claim the replacement's slot. Registration overwrites the slot's live-state entry and emits the displaced-entry warning (`engine/live-worker-state/src/lib.rs:805`). A previous run's tmux session can still exist.

The spawn-ack and stale-worker reapers also released a slot after awaiting a reaper that could already have released it (`engine/core/src/spawn_ack_sweep.rs:1006`, `engine/core/src/stale_worker_sweep.rs:1901`). All of these paths now use the existing atomic `release_worker_if_execution` operation (`engine/execution-contracts/src/lib.rs:963`). Scheduler error cleanup follows the same ownership rule. This fixes release at its source rather than hiding the displacement warning.

Viewer detachment has a second await boundary: a replacement may register while an older detach waits for the app. Cleanup now uses `release_slot_for_run` and `stop_slot_for_run`, preserving the replacement's driver evidence and status task (`engine/app/src/app.rs:2348`).

This reproduces a mechanism consistent with the three registrations in the incident. The incident log excerpt does not include the pool-release events preceding the first collision, so it cannot identify the exact old cleanup invocation that first freed that claim.

## What the collision does and does not explain

The app rejected slot 41 because its attach validation only accepted interactive, automation, and review ranges. It now accepts the engine's fixed progression range 41–44, including when the configured review pool is smaller (`app-macos/Sources/Ghostty/WorkersWorkspaceModel.swift:24`). An occupied progression viewer receives the ordinary `SlotBusy` response.

The viewer error was not itself a prompt-delivery failure. The tmux session is independent of the app viewer; non-`SlotBusy` viewer errors are recorded while the worker remains registered and progress ingress is activated (`engine/core/src/spawn_flow.rs:890`, `engine/core/src/spawn_flow.rs:964`). `SlotBusy` is the one viewer failure that aborts spawn and stops the ingress (`engine/core/src/spawn_flow.rs:880`).

### Launcher to rollout attach

1. Codex `provision_workspace` creates the per-run `CODEX_HOME` and points `CODEX_HOME/sessions` at Boss's durable transcript directory for that execution id (`engine/driver/src/codex.rs:1964`, `engine/driver/src/codex.rs:1968`). Progress wiring watches that durable directory, not the symlink (`engine/driver/src/codex.rs:2149`).
2. Before the pane comes up, `prepare_progress_ingress` snapshots pre-existing rollout files and stores `IngressCheckpoint::Armed` (`engine/core/src/spawn_flow.rs:547`, `engine/core/src/agent_jsonl_progress.rs:569`). Discovery does not run until `activate_progress_ingress` after live-state registration (`engine/core/src/spawn_flow.rs:964`, `engine/core/src/agent_jsonl_progress.rs:765`).
3. The launcher writes the brief to its own child's PTY, not to an app slot (`engine/driver/src/initial_prompt.py:568`). Each step is appended to `initial-prompt.delivery.jsonl` with the original wall-clock `at_ms` (`engine/driver/src/initial_prompt.py:645`).
4. Spawn confirmation waits for composer readiness and then a driver hook/session event or a persisted transcript path, then prompt-turn evidence (`engine/execution-runner/src/runner/pane_spawn.rs:1772`). Each wait is a poll loop with its own deadline (`engine/execution-contracts/src/spawn_confirmation.rs:137`). The engine reads the launcher JSONL only after that confirmation future returns (`engine/execution-runner/src/runner/pane_spawn.rs:1833`).
5. Codex driver-start proof for a file-ingress worker is attach or discovery-time growth of a run-correlated rollout (`engine/core/src/agent_jsonl_progress.rs:907`, `engine/core/src/stdout_progress.rs:57`), or a later liveness-probe veto that records `CorrelatedTranscript` (`engine/core/src/spawn_ack_sweep.rs:797`). `record_driver_signal` only records against a matching live run (`engine/live-worker-state/src/lib.rs:1306`).

The recorded `ready` and `paste_sent` timestamps near 10:50:39 show the paste was written then. They do not prove Codex accepted it, wrote a rollout, or began a turn.

Displacement removes the old run's slot-backed driver evidence. Preventing premature reuse and delayed cleanup preserves a _replacement's_ evidence. Rollout discovery is keyed by execution and its private transcript directory, not by slot (`engine/driver/src/codex.rs:2160`, `engine/core/src/agent_jsonl_progress.rs:569`). The collision therefore cannot explain why the original execution's ingress stayed `Armed` without attaching a rollout: either Codex never wrote a new correlated file under that directory, or discovery never matched one, and the surviving incident excerpt does not include the discovery observations or the directory listing that would distinguish those.

## Relation to PR #3654

[PR #3654](https://github.com/spinyfin/mono/pull/3654) omitted the empty `BOSS_LEASE_ID` export for lease-free progression workers. Its production behavior change is in environment assembly (`engine/core/src/spawn_flow.rs:755`); it did not alter pool allocation, registration, the launcher, or rollout discovery. It allowed progression spawns to reach the already-existing pane lifecycle, exposing defects beyond the previously failing environment validation.

## Why prompt steps appeared after teardown

The launcher appends each step to a JSONL file with its original wall-clock `at_ms` (`engine/driver/src/initial_prompt.py:645`). The engine reads and logs the accumulated steps only after spawn confirmation returns (`engine/execution-runner/src/runner/pane_spawn.rs:1749`, `engine/execution-runner/src/runner/pane_spawn.rs:1833`). Independent sweeps can reap the run while that confirmation future is still pending. Consequently a later engine log entry can report a paste from before teardown. It is not evidence of a new paste into a dead worker.

The replay of original `at_ms` values is understood. The excerpt does not establish why the confirmation future itself resumed so late (~45 minutes after the paste). The poll helper has a deadline around each awaited predicate (`engine/execution-contracts/src/spawn_confirmation.rs:137`), but a deadline is not evidence of when an async task was actually scheduled. The fix does not change the 120s discovery-overdue threshold, the 300s driver-start grace, or treat a delayed log entry as driver-start proof.

## Regression evidence

- `delayed_progression_cleanup_cannot_release_a_replacement_in_slot_41` failed before the fix: the replacement's claim was `None` after the old cleanup. It passes with execution-scoped release and verifies a third run cannot claim the occupied slot.
- `delayed_progression_viewer_detach_preserves_the_replacement_and_its_driver_signal` holds the app's detach response, installs a replacement, then completes the old teardown. The replacement retains its pool claim, registry mapping, live state, and transcript signal.
- `testProgressionSlotsAttachAndRejectAnOccupiedSlot` covers all four progression slots, a reduced review pool, occupied-slot rejection, and rejection beyond slot 44.
- `displaced_progression_spawn_cleanup_does_not_strip_the_replacements_driver_signal` follows the incident shape with durable execution rows: the failing spawn's teardown frees slot 41, a replacement claims and registers it, the old spawn's error tail and shared reaper cleanup run late, and a correlating rollout on disk reaches the driver-start guard through the liveness probe (the production path for an armed-but-unattached ingress). The replacement is vetoed, not reaped. A no-signal control on the same fixture is reaped, so a missing execution row cannot masquerade as a passing guard. Before the reaper cleanup became run-keyed, `release_reaped_execution` removed the replacement's live entry, so the probe had nowhere to record the transcript.
- `reaped_cleanup_leaves_a_replacement_run_in_the_same_slot`, `cadence_reap_leaves_a_replacement_claim_and_live_state_in_place` and `reaped_spawn_tail_leaves_a_replacement_claim_in_place` cover the shared reaper cleanup, the stale-worker cadence tail and the spawn-ack tail. Each also asserts the stale release does not latch an extra dispatch kick.
- `claim_lost_cleanup_does_not_release_a_replacement_claim` and `schedule_error_cleanup_does_not_release_a_replacement_claim` drive the scheduler's claim-lost and `schedule_execution` error tails with a replacement already holding the worker id.

Still unexplained, and not claimed fixed by the slot-ownership change: why the original execution's armed rollout ingress never attached a file, which cleanup invocation first freed the colliding claim (the excerpt has no pool-release events), and why the confirmation future resumed tens of minutes after the paste. Those need the missing engine log lines or a live Codex session this fixture does not launch. This investigation removes one confirmed way a _replacement's_ driver evidence was destroyed.

The first of those is now explained and fixed: Codex never wrote a rollout, because it did not trust the run's working directory, replaced its composer with its folder-trust dialog, and lost the brief that had been delivered. See [codex-progression-spawn-never-attaches-2026-11-17.md](codex-progression-spawn-never-attaches-2026-11-17.md). The other two remain open.

These tests use isolated fixtures and injected tmux responses. They do not launch a live Codex session. The driver-start guard, spawn alert, displaced-entry warning, 120-second discovery-overdue threshold, and 300-second driver-start grace remain in force.
