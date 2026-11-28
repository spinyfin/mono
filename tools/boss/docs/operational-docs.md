# Operational docs workers should know

- Boss macOS app rules (no modal dialogs for non-actionable status or
  error messages; which inline/banner/log surface to use instead):
  [`tools/boss/app-macos/AGENTS.md`](../app-macos/AGENTS.md)
- Bazel / Xcode LaunchServices pin on macOS hosts:
  [`tools/boss/docs/mac-toolchain-xcode-pinning.md`](mac-toolchain-xcode-pinning.md)
- Boss forensic surfaces (`engine-audit.log`, per-task cost / transcripts):
  [`tools/boss/docs/forensic-surfaces.md`](forensic-surfaces.md)
- Worker liveness contract (what the agent indicator derives from; how the
  engine converges when it has lost track of a live worker):
  [`tools/boss/docs/worker-liveness-contract.md`](worker-liveness-contract.md)
- Worker prompt delivery (how a brief is pasted and submitted, what proves
  the driver accepted it, when the engine presses Return again, and where
  each step is logged):
  [`tools/boss/docs/worker-prompt-delivery.md`](worker-prompt-delivery.md)
- Failure-signal lifecycle (what raises an attention item / the card's
  "Failed to start" banner, and what specifically lowers it again):
  [`tools/boss/docs/attention-lifecycle.md`](attention-lifecycle.md)
- Post-crash orphan recovery:
  [`tools/boss/docs/post-crash-recovery.md`](post-crash-recovery.md)
- Revision review hold (why a finished revision stays `active`, the head its
  hold is released by, what a completion records, and what to compare when
  one stays held with nothing running):
  [`tools/boss/docs/revision-review-hold.md`](revision-review-hold.md)
- Execution recovery pointers (the three bookmarks per execution, how they
  get damaged, and how dispatch reconciles or retires them):
  [`tools/boss/docs/execution-recovery-pointers.md`](execution-recovery-pointers.md)
- Self-update attempt log (where the durable update trail lives, how to
  follow one attempt across the old app, the relaunch helper and the new
  app, and where a rollback's reason is recorded):
  [`tools/boss/docs/update-attempt-log.md`](update-attempt-log.md)
- Crash watchdog (why an abort always kills the app, and
  `BOSS_CRASH_WATCHDOG_SECONDS`):
  [`tools/boss/docs/crash-watchdog.md`](crash-watchdog.md)
- Coordinator session handoff (what the outgoing coordinator session
  writes, how the incoming one is briefed, and the three states it can
  report):
  [`tools/boss/docs/coordinator-session-handoff.md`](coordinator-session-handoff.md)
- Per-product coordinator guidance (`BOSS_COORDINATOR.md` at a product
  repo's root: coordinator-only rules, how the engine loads them from
  GitHub, and why they never go in `AGENTS.md`):
  [`tools/boss/docs/coordinator-product-guidance.md`](coordinator-product-guidance.md)
- Per-repo worker guidance (`BOSS.md` at a product repo's root: Boss-specific
  rules injected into coding workers' prompts, which executions get it, what
  the prompt carries when GitHub is down, and when to use it instead of
  `CLAUDE.md` / `AGENTS.md`, `BOSS_COORDINATOR.md` or the dispatch preamble):
  [`tools/boss/docs/repo-boss-guidance.md`](repo-boss-guidance.md)
- Worker tool environment (why bare `checkleft` resolves to the repo's
  Bazel build in a worker, how the composed `PATH` is sealed across
  driver tool shells, and what that seal does not cover):
  [`tools/boss/docs/worker-tool-environment.md`](worker-tool-environment.md)
- Background-sweep telemetry (per-pass duration / items / pass-count
  metrics for the envelope watchdog, terminal-work sweep, dead-pane
  terminal scan, JSONL tail poll and recovery-pointer passes, tick
  intervals for the two singleton sweeps, and what each one's cost scales
  with):
  [`tools/boss/docs/background-sweep-telemetry.md`](background-sweep-telemetry.md)
- Work DB read volume (how the engine attributes `state.db` page reads to
  a caller and statement, the `work DB heavy read` / `db read summary` /
  `database size report` log lines, and the rules and read-budget test that
  keep a timer-driven read from walking a table):
  [`tools/boss/docs/work-db-read-volume.md`](work-db-read-volume.md)
- Merge poller detection pass (the order a pass works in, what an abandoned
  pass does and does not lose, the `merge_detection_stalled` health issue,
  and the per-phase timing to read when a pass is slow):
  [`tools/boss/docs/merge-poller-detection-pass.md`](merge-poller-detection-pass.md)
- Answer latency (the per-hop log lines from clicking an answer on a
  question card to the card clearing, in the app and the engine, and which
  reads an answer does and does not wait for):
  [`tools/boss/docs/operator-answer-latency.md`](operator-answer-latency.md)
- Dispatch focus mode (`bossctl dispatch focus`: restrict automatic
  dispatch to chosen projects/products; scope rules, precedence against the
  pause and force-launch, the `focus_mode` hold reason):
  [`tools/boss/docs/dispatch-focus-mode.md`](dispatch-focus-mode.md)
- Run process teardown (how the engine and cube find and kill what a run
  left running in its workspace, why working directory rather than process
  group, and how a workspace's Bazel server is told apart and spared):
  [`tools/boss/docs/run-process-teardown.md`](run-process-teardown.md)
- Operator runbooks:
  [`tools/boss/docs/runbooks/`](runbooks/)
