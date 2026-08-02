# Grok eligibility for review and conflict-resolution kinds (Phase 3)

- **Date:** 2026-07-31
- **Kind:** empirical acceptance sweep for T-23 (Grok-as-first-class-driver design, Phase 3) — real, billed `grok-4.5` API calls; no throwaway harness beyond the driver crate's existing live-test scaffolding
- **Related:** [grok-permission-isolation-2026-07-27.md](./grok-permission-isolation-2026-07-27.md) (T-16 — sandbox profile semantics this sweep builds on), [grok-as-a-first-class-interactive-agent-driver.md](../designs/grok-as-a-first-class-interactive-agent-driver.md) (§T-23)
- **Code:** `tools/boss/engine/driver/src/grok.rs` — `write_permission_config_live_review_result_round_trips_under_reviewer_sandbox`, `write_permission_config_live_standard_sandbox_allows_workspace_write` (new); `write_permission_config_live_sandbox_denies_workspace_write` (pre-existing, re-run for this sweep)

## Why this exists

Phase 3's acceptance bar (design doc §"Proposed implementation task breakdown" → T-23) is explicit about what does **not** count as evidence: "accepting 'the model chose not to write' as evidence of read-only enforcement" and "enabling the kind on the strength of the sandbox profile name resolving." It asks for two things to be demonstrated, not assumed:

1. Review: `--sandbox read-only` is a genuine reviewer-read-only equivalent (a real denied write, not a polite refusal), **and** structured `ReviewResult` output round-trips.
2. Conflict resolution: real write access, plus the merge-conflict telemetry path.

Unlike Phase 1 (chores) and Phase 2 (design/investigation/postmortem), Phase 3 does not gate through `KindRequirements` — see [Why no `KindRequirements` change](#why-no-kindrequirements-change-was-needed) below. So this pass is a verification sweep, not a capability-declaration change, matching how T-21 (Phase 1's acceptance sweep) was scoped: "a sweep, not an implementation."

## Verdict (read this first)

| Requirement                                                            | Evidence                                                                                                                                                                                                                                                                                                                                                                         | Result                                                               |
| ---------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| Reviewer sandbox genuinely denies a workspace write                    | `write_permission_config_live_sandbox_denies_workspace_write` (pre-existing, T-17): real Write-tool + shell attempt, both denied by the OS (`Operation not permitted`), file absent — re-run for this sweep                                                                                                                                                                      | **Confirmed, real denial**                                           |
| `ReviewResult` round-trips under that same read-only sandbox           | `write_permission_config_live_review_result_round_trips_under_reviewer_sandbox` (new): workspace write still denied; structured-output artifact written to the real engine-resolved path and parsed back through `boss_pr_review::ReviewResult::from_json`                                                                                                                       | **Confirmed, real round trip**                                       |
| Conflict resolution (`WorkerKind::Standard`) has real write access     | `write_permission_config_live_standard_sandbox_allows_workspace_write` (new): real file created in the workspace under the `Standard`-kind sandbox                                                                                                                                                                                                                               | **Confirmed, real write**                                            |
| Merge-conflict telemetry path is reachable for a Grok-driven execution | Code-level: the RPC/DB path carries no driver awareness at all (see [below](#conflict-resolution-telemetry-path)) — verified by reading `conflict_watch.rs`, `conflict_ladder.rs`, `conflict_remediation.rs`, `resolution_signal_capture.rs`, and `work/tests/t18.rs`'s existing coverage of `record_producer_side_conflict`, none of which reference a driver or execution kind | **Confirmed by inspection, not re-run live end-to-end — see caveat** |

**Boss implication:** the sandbox and structured-output properties Phase 3 needed evidence for are now proven — sandbox denial (T-17), `ReviewResult` round trip, and `Standard`-kind write access are all confirmed by real, live-driver tests. But that is necessary, not sufficient: this sweep's analysis stopped at `CapabilityResolver::check_dispatch` and never reached the gate that actually decides which driver reviews a PR. Every `ExecutionKind::PrReview` execution routes to the dedicated review pool (`coordinator/config.rs::execution_targets_review_pool` returns `true` unconditionally for `PrReview`, so `pool_for_execution` always sends it there), and that pool's dispatch policy is pinned: `coordinator.rs::REVIEWER_POOL_DRIVER = "claude"`, applied by `pool_dispatch_policy_for_worker_id` to every `review-`/`auto-worker-` id, with its own doc comment stating the policy deliberately "[o]verrides whatever `tasks.driver` the reviewed/automated row itself carries" so that "who authored a change must not determine who reviews it" (asserted in `coordinator_tests/pool.rs:235`). **A `PrReview` execution cannot dispatch on Grok today, full stop** — no `CapabilitySet` or `KindRequirements` change would flip this, because the pin lives entirely outside the `TaskKind` dispatch gate this sweep audited. Changing it is a deliberate design decision the pool-dispatch doc comment itself scopes as follow-on work ("configurable reviewer model, load balancing across reviewer models, two-party review"), not something this sweep can conclude away.

The same pin has a conflict-resolution analogue worth recording: `execution_targets_automation_pool` routes any work item whose `source_automation_id` is non-null to the automation pool, which carries the identical `REVIEWER_POOL_DRIVER` pin. So Grok conflict resolution — the write-access and telemetry properties this sweep did verify — only applies to main-pool work items; an automation-produced conflict-resolution execution is still forced onto `"claude"` regardless of the row's own driver.

## Why no `KindRequirements` change was needed

Phase 2 (T-22) extended `KindRequirements::for_kind`'s match arm from `TaskKind::Design` alone to `TaskKind::Design | TaskKind::Investigation | TaskKind::DesignPostmortem` — a real code change, because the dispatch gate (`CapabilityResolver::check_dispatch`, called from `runner/worker_spawn.rs`) is keyed on `TaskKind`, and those three kinds are literal `TaskKind` variants.

`PrReview` and `ConflictResolution` are not `TaskKind` variants at all — they are `ExecutionKind` variants (`boss_protocol::types::execution`). A `PrReview` or `ConflictResolution` execution's `work_item_id` still points at the _original_ Task/Chore/Revision row being reviewed or unstuck, so `runner/worker_spawn.rs`'s capability gate (`work_item_task_kind_enum` → `check_dispatch`) resolves against **that row's own `TaskKind`** — `Task`, `Chore`, or `Revision` in the overwhelming common case, none of which carry a `KindRequirements` escalation. Grok already declares `StructuredOutput` and `ToolUseInterception` unconditionally (not per-kind), so it already clears the `TaskKind` dispatch gate Claude and Codex clear for reviewing or unblocking a `Task`/`Chore`/`Revision` row — there is nothing to flip in `CapabilityResolver::check_dispatch` or `KindRequirements` for either kind.

That gate, however, is not the one that decides which driver actually reviews a PR. For `PrReview`, dispatch is redirected entirely: `coordinator/config.rs::execution_targets_review_pool` sends every `PrReview` execution to the dedicated review pool, and `coordinator.rs::REVIEWER_POOL_DRIVER` / `pool_dispatch_policy_for_worker_id` then force that pool's dispatch driver to `"claude"`, overriding whatever `tasks.driver` the reviewed row carries. So while no `KindRequirements` change is needed for the `TaskKind` gate this section is about, that is not the same as "no engine-side driver-branch fix is needed" for reviews generally — see the Verdict section above for the gate that actually blocks Grok from reviewing today.

The two properties Phase 3 actually cares about for the properties it _can_ speak to — genuine sandbox enforcement and telemetry reachability — live below the `TaskKind` gate entirely: in `GrokDriver::write_permission_config`'s sandbox rendering (`grok/permissions.rs`) and in the engine's driver-agnostic conflict-telemetry RPC. Both are proven by the evidence above rather than by any dispatch-gate change.

## `ReviewResult` round trip: why it is not in tension with read-only

The reviewer's structured-output artifact and the workspace-write denial are independent properties by construction, not something this sweep had to reconcile by loosening the sandbox. The engine resolves the `ReviewResult` artifact path under the system temp dir (`boss_engine_structured_output::default_dir()`, i.e. `std::env::temp_dir().join("boss-worker-output")` absent a `BOSS_WORKER_OUTPUT_DIR` override) — and T-16's investigation already established that Grok's built-in `read-only` sandbox profile keeps `/tmp` and the macOS per-process temp tree (`/var/folders/…`) writable unconditionally, regardless of worker kind (`grok-permission-isolation-2026-07-27.md` §"`/tmp` always writable — validation hazard"). The new live test confirms this holds for the _actual_ production path, not a stand-in: same run, same sandboxed process, workspace write denied and structured-output write succeeded.

**No transcript fallback if the artifact write fails.** `GrokDriver::structured_output_fallback` (`grok.rs:736`) returns an empty `Vec` unconditionally, for every `StructuredOutputKind`. The engine's reviewer finalizer (`completion/finalize_passes.rs:563-590`) calls into that fallback — `driver_or_default(driver).structured_output_fallback(ReviewResult, &text)` — only when the artifact file is missing or fails to parse, extracting `ReviewResult` candidates from the raw transcript text as a backstop. With Grok always returning zero candidates, `review_result_from_candidates` resolves to `(None, None)`, so a Grok reviewer whose artifact write fails (a sandbox denial, a path resolution bug, a truncated write) goes straight to the engine's re-prompt/auto-nudge loop and, failing that, to a give-up attention state with `review_result = None` — with no recovery from the transcript. The reviewer prompt's own documented ` ```json ` fenced-block backstop is inert under Grok today. This makes the structured-output artifact channel proven above a single point of failure for Grok reviews specifically, in a way it is not for drivers with a real transcript fallback.

## Conflict-resolution telemetry path

`ExecutionKind::ConflictResolution` maps to `WorkerKind::Standard` in `worker_setup::worker_kind_for_execution` — the same driver-agnostic, exhaustive match every other execution kind goes through; there is no per-driver branch to get wrong. The worker-facing telemetry surface itself (`boss engine conflicts record-producer`, documented in the worker's own `CLAUDE.md`) resolves through `app/conflict_resolution.rs::handle_record_producer_side_conflict` → `WorkDb::record_producer_side_conflict`. Neither that handler, `work/conflict_res.rs`, `conflict_watch.rs`, `conflict_ladder.rs`, `conflict_remediation.rs`, nor `resolution_signal_capture.rs` reference a driver, `ExecutionKind`, or any Claude-specific type — grepped for this sweep, zero hits. `work/tests/t18.rs`'s existing coverage of `record_producer_side_conflict` (four tests: sentinel-PR-before-PR-exists, real-PR-after-PR-exists, conflict-class classification, churn-guard interaction) never sets or asserts on a driver value, because the function has none to key off.

**Caveat — what this sweep did not re-run live.** A full end-to-end exercise (spin up an isolated test-fixture engine, create a real product/task/execution graph with `driver = "grok"`, drive a live Grok worker through an actual git conflict, and have it invoke `boss engine conflicts record-producer` against that engine) was not performed in this pass. That harness would mostly re-validate machinery Phase 1's T-21 acceptance sweep already exercised for every one of its 10 chores — a Grok worker successfully running an instructed CLI command end-to-end is exactly what `cube pr create` already proved, repeatedly, on the primary path. Given the DB/RPC layer is proven driver-blind by construction (no field to branch on) and the shell-command-execution capability is already established, standing up that harness again would not test anything specific to Grok. If we later want that redundancy removed rather than accepted, standing up the harness is a bounded follow-on rather than a blocker.

## Live evidence transcripts (redacted to the relevant fields)

**Reviewer sandbox — workspace write denied (T-17, re-run):**

```text
Outcome: Write failed; probe_write.txt was not created.
Write tool: IO Error: Operation not permitted (os error 1)
Shell (echo 'SHOULD_NOT_EXIST' > probe_write.txt): operation not permitted: probe_write.txt (exit 1)
Verification: ls / cat report No such file or directory for probe_write.txt.
```

**Reviewer sandbox — `ReviewResult` round trip (new):**

```text
1. probe_write.txt: Failed. Shell redirect returned: operation not permitted: probe_write.txt (exit 1). File was not created.
2. review-result JSON: Wrote successfully to /var/folders/.../T/boss-worker-output/run-permcfg-live-review-result-1.review-result.json (172 bytes, exit 0).
```

Parsed back via `boss_pr_review::ReviewResult::from_json`: `pr_url == "https://github.com/example/repo/pull/1"`, `revision_warranted == false`, `findings == []` — exact match to the literal written.

**Standard sandbox — workspace write allowed (new):**

```text
Created probe_write.txt in the current directory with content SHOULD_EXIST via the write tool. Outcome: success.
```

File verified present on disk by the test (not by the model's report) after the probe returned.
