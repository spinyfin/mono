# 0007 — P545 distributed-agent-execution: the "testable end-to-end" milestone that wasn't

**Status:** Resolved (root cause identified; corrective work re-scoped)
**Date:** 2026-05-31
**Author:** Boss (coordinator, take-the-conn)
**Area:** process (milestone accounting & follow-up sizing) — not a code defect

## Summary

Project P545 ("Distributed agent execution: register and dispatch to remote SSH hosts") was structured as eight phases. Phase 3 (T562, PR #657) was billed in the plan as _"the 'zakalwe runs a chore' milestone"_ — the point at which remote execution would be demonstrable end-to-end: register an SSH host, dispatch a chore to it, watch a worker run remotely. After Phase 3 merged, the project was treated as ~3/8 complete with the hard integration behind it.

It was not. The core of remote execution — coordinator host-routing and a functioning `SshHostAdapter::spawn_worker` — was never built in Phase 3. **Crucially, PR #657 said so plainly.** Its body explicitly deferred "the final piece of remote-claude spawn (SSH-forwarded events socket + transcript readback + signal channel)" to a follow-up, and its `spawn_worker` ships an honest `"not yet wired end-to-end"` error rather than a fake success. Phase 3 landed real, well-built _scaffolding_ (the `HostAdapter` trait + `SshHostAdapter` shell, SSH transport, wrapper distribution, a pure `select_host` scheduler) and was transparent that the wiring was still to come.

So this is **not** a case of a worker misrepresenting their work. The failure is upstream of the code: the project's _milestone accounting_ equated "Phase 3 merged `done`" with "the milestone is hit," even though Phase 3 had openly deferred the milestone's own acceptance criterion. The deferred work was captured as a follow-up task (T584) — but framed as a narrow "events socket + stdio + transcript" detail, which disguised that it actually contained the entire remote-execution integration. The gap then sat invisible for ~13 days, because the ledger showed Phase 3 done and the remaining work hid behind an innocuously-named backlog item. It surfaced only on 2026-05-31, when a worker (Data) finally executed T584, read the live code, and measured the true scope: a multi-PR feature, not a one-method follow-up.

No work was deleted. Nothing "went missing" from the task graph. What went missing was the distinction between _the milestone the plan promised_ and _what the milestone phase was actually scoped to deliver_.

## Timeline

- **2026-05-18 15:52 UTC** — T562 / PR #657 ("engine: Phase 3 — SshHostAdapter + wrapper distribution + scheduler") merges. +1809/−15. Body is explicit: workspace-lifecycle ops work over SSH; remote-claude **spawn is a deferred follow-up**. `spawn_worker` returns `"not yet wired end-to-end"`; `select_host` lands as a pure, tested helper not yet called by the dispatch loop.
- **2026-05-18 ~20:25 UTC (~4.5h later)** — T584 ("Phase 3 follow-up: `SshHostAdapter::spawn_worker` end-to-end — events socket forwarding + worker stdio + transcript readback") is filed to capture the deferral. Its framing reads as a stdio/transcript finishing detail, not "the rest of the feature."
- **2026-05-18 → 2026-05-31 (~13 days)** — T584 and Phases 4–8 (T563–T567) sit in backlog. Because T562 reads `done`, the stall looks like ordinary backlog latency rather than an unmet milestone.
- **2026-05-31** — T584 is dispatched. The worker reads current `main` and reports remote dispatch is unwired end-to-end (coordinator never selects a host; the adapter has no engine collaborators; completion/live-status/transcript are local-coupled). It pushes back twice, declining to ship a non-functional stub under an "end-to-end" label. Scope is corrected: T584 becomes PR1 of a four-PR dependent stack (T584 → T1020 → T1021 → T1022).

## Investigation

Workers cannot read Boss runtime state, so this was done from the coordinator with direct access to the task ledger, the GitHub PR record (body + diff), and current `main`.

### Ground truth from current `main`

- **`select_host` is defined but never called.** It exists as a pure helper in `host_scheduling.rs` with its own unit tests, but has **0 references in `coordinator.rs`** and **0 across `ssh_host_adapter.rs`/`host_adapter.rs`/`app.rs`/`runner.rs`/`work.rs`**. The dispatch loop never selects a host. This is the mechanical fingerprint of "scaffolding merged, integration deferred."
- **The coordinator only ever constructs `LocalHostAdapter`** and calls `spawn_worker` against it. No per-host adapter construction exists, so even a complete `SshHostAdapter` could not be reached through normal dispatch.
- **`SshHostAdapter::spawn_worker` is a deliberate stub.** In #657 it returns `"SshHostAdapter::spawn_worker not yet wired end-to-end on host {}"`. It has no `work_db`, `RuntimeConfig`, events-socket path, or `worker_registry`/`live_worker_state` — the collaborators a real spawn needs (prompt is composed in `runner.rs`; hook settings come from `worker_setup`).
- **Completion, live-status, and transcript are local-coupled.** Completion + the `in_review`/PR-URL transition run in `completion::on_stop` over the forwarded events socket; live-status routes by `worker_registry.slot_for_run`; transcript serving reads a _local_ file path. None has a remote path.

### What PR #657 delivered vs. what the milestone promised

PR #657 delivered exactly what its body claimed: SSH transport, wrapper bundling/distribution, the adapter's workspace-lifecycle operations over SSH, a pure scheduler, and host persistence — with remote-claude spawn explicitly deferred. The PR did **not** over-claim. The over-claim lived one level up, in the **plan's** label for Phase 3 ("the 'zakalwe runs a chore' milestone"), which implied end-to-end remote execution would be demonstrable when Phase 3 merged. Phase 3 was never scoped to make that true, and said so.

## Root cause

**Milestone status was inferred from phase-merge status, not from an executed acceptance gesture — and the phase had openly deferred its own milestone criterion into an under-scoped follow-up.**

Two compounding factors:

1. **Milestone accounting trusted "phase merged `done`."** Phase 3 carried the milestone label, so when it merged green the project was counted as having hit the milestone. But the milestone's acceptance gesture (register host → dispatch chore → observe remote worker) was never run, and could not have passed — `spawn_worker` was a stub by the PR's own admission. "Phase merged" and "milestone met" were treated as the same fact; they weren't.

2. **The deferred remainder was under-scoped as a "follow-up."** The honest deferral in #657 became T584, but T584 was titled and framed as "events socket + worker stdio + transcript readback" — a finishing detail. In reality it required coordinator host-selection, per-host adapter construction, collaborator injection, the events-socket reverse-forward, and remote completion/live-status/transcript — i.e. the whole integration. A follow-up sized as a detail will be prioritised, reviewed, and reasoned about as a detail.

This is a textbook instance of the standing lesson _verify a claim by exercising it, not by reading status_. Here the unexercised claim was not a PR body (that was honest) but the **plan's milestone label**, propagated into project-completeness accounting without anyone running the milestone's own gesture.

## Why it stayed hidden for ~13 days

- The ledger showed T562 `done` with a merged PR, so dashboards counted Phase 3 — and its milestone — complete.
- T584 was framed as a small detail, so its presence in backlog didn't read as "the feature isn't built."
- No automated end-to-end test pins the milestone; the milestone existed only as a plan label, which leaves no artifact and cannot regress-guard.
- Nobody executed the remote path between 05-18 and 05-31; discovery required a worker to open the code intending to implement.

## Recommendations

1. **A milestone is met when its acceptance gesture runs, not when its phase merges.** Any phase labeled a testable milestone must carry an explicit, runnable acceptance command/test as a completion gate. If it can't be run, the milestone isn't met — regardless of merge status.
2. **Deferrals must be sized honestly at the point of deferral.** When a phase defers part of its milestone (good, transparent practice — as #657 did), the follow-up task must state what is _not yet functional_ and be sized against that, not given a detail-sounding title. A follow-up that contains "the rest of the feature" must say so.
3. **Lint for orphaned integration points.** `select_host` defined-but-never-called is a reliable signal of "scaffolding landed, wiring deferred." A CI check for engine entry points that are defined but unreferenced (or a dead-code denial in the dispatch crate) would surface the deferral as an explicit, tracked state rather than a silent one.
4. **Don't let phase decomposition hide the hard part in a follow-up.** If a phase is the milestone, the integration that makes the milestone true belongs in that phase's acceptance criteria. A "Phase N + Phase N follow-up" split should never let the substance live in neither.
5. **Re-scoping when reality diverges is correct, not a failure.** Data's two pushbacks — refusing to ship a stub under an "end-to-end" label and asking for a scoping decision first — are exactly the behaviour that surfaced this. The corrective stack is the honest decomposition the milestone needed.

## Corrective state (as of 2026-05-31)

- **T584** — re-scoped to **PR1**: SSH-transport reverse-forward primitives + a pure `ssh_spawn` planning module + worker-wrapper detach/pid/tee changes + tests against a stubbed transport. Self-contained and verifiable. In progress.
- **T1020** — PR2: inject collaborators into `SshHostAdapter`; implement `spawn_worker` for real. Blocked on T584.
- **T1021** — PR3: coordinator host-selection + per-host adapter construction + host-column persistence. Blocked on T1020.
- **T1022** — PR4: live-status slot wiring + remote transcript pull + engine-restart reattach. Blocked on T1021.

The four tasks form a dependency chain (one CI-green PR each). Together they constitute the integration that the Phase-3 milestone implied but that Phase 3 deliberately deferred. Phases 4–8 (T563–T567) remain genuine future work on top of a then-real milestone.

## Appendix — evidence index

- PR #657 (T562): merged 2026-05-18 15:52 UTC, +1809/−15. Body explicitly defers remote-claude spawn; `spawn_worker` returns `"not yet wired end-to-end"`; `select_host` is a pure helper.
- Current `main`: `select_host` references = 0 in `coordinator.rs`, `ssh_host_adapter.rs`, `host_adapter.rs`, `app.rs`, `runner.rs`, `work.rs`. Coordinator constructs only `LocalHostAdapter`.
- T584 filed 2026-05-18 ~20:25 UTC (~4.5h after #657 merge); dispatched 2026-05-31; re-scoped to PR1 of a 4-PR stack the same day.
- Worker (Data) code reading, 2026-05-31, independently reached the same conclusion: remote dispatch unwired end-to-end.
