# Worker background scheduling priority — 2026-07-15

## Rescope context

Original filing treated coordinator-pane typing sluggishness under 22 concurrent workers as an app bug to profile. Operator's `htop` showed every core pegged, load average 152 (~10x oversubscription, 9809 threads): the machine was saturated and macOS was scheduling keystroke handling on equal terms with 22 build/test workloads. Not an app-code defect — the fix is scheduling-priority isolation, not app profiling.

## What changed

`WorkersWorkspaceModel.spawnWorkerPane` (all three pools — main/interactive, automation, review) now calls `WorkerProcessPriority.applyBackgroundPriority(toShellPid:runId:)` the moment a worker pane's shell pid becomes known (`tools/boss/app-macos/Sources/Ghostty/WorkersWorkspaceModel.swift`, both the immediate and 250ms-retry `onSurfaceAttached` paths).

`WorkerProcessPriority` (`tools/boss/app-macos/Sources/Ghostty/WorkerProcessPriority.swift`) calls `setpriority(2)` with `PRIO_DARWIN_PROCESS` / `PRIO_DARWIN_BG` directly (no `taskpolicy` subprocess needed — same underlying syscall). Per `taskpolicy(8)`: "All children of the specified program also inherit these policies" — Darwin's background scheduling flag is proc-wide and inherited at fork, so clamping the shell once, before any build tooling runs under it, reaches every descendant (bazel client+server, cargo, rustc, node, `claude` itself, etc.) without touching each individually.

The Boss app process, the coordinator's own session pane (`BossPaneModel`), and pane rendering never go through `spawnWorkerPane` and are untouched — they stay at normal/user-interactive scheduling.

## Bazel daemon: deliberate choice, documented

The bazel server is long-lived and persists across build invocations, shared by unix-domain socket with whichever client talks to it — including an operator's own "take the conn" invocation against a workspace a worker is using. Because our clamp is fork-inheritance based, whichever process first spawns the bazel server determines its priority for its entire lifetime:

- If a background-clamped worker starts the daemon first, it runs at `PRIO_DARWIN_BG` even when later serving an operator-invoked build against the same workspace.
- We accept this. The goal is keeping the coordinator's own UI thread responsive under fleet-wide CPU oversubscription; a bazel daemon started by a worker doing background-priority work is, by construction, work we want deprioritized relative to the UI. An operator doing "take the conn" interactively on a worker's workspace is already opting into that worker's build state.
- Escape hatches if this proves wrong in practice: (a) the per-clamp config flag below disables the mechanism workspace/session-wide; (b) `taskpolicy -B -p <daemon_pid>` manually un-clamps a specific already-running daemon; (c) `bazel shutdown` in the workspace forces a fresh daemon on the next build, which will inherit priority from whatever process starts it next.

## Configuration

- `BOSS_WORKER_BACKGROUND_PRIORITY` env var (checked first) — any value other than `0`/`false` (case-insensitive) is "enabled".
- `defaults write <bundle-id> boss.worker.backgroundPriorityEnabled -bool NO` — persistent per-machine override via `UserDefaults`.
- Default: **enabled**. If background QoS on Apple Silicon E-cores proves too aggressive for worker throughput (workers starved indefinitely under contention rather than merely slowed), disable via either mechanism above.

## Verification note for the operator (headless caveat)

This change could not be subjectively validated in this run — no live `BossMacApp` with a real fleet under load was available. To verify:

1. Saturate the fleet (e.g. dispatch enough work to fill all Bridge Crew + Lower Decks + automation slots so every core is near 100%, matching the original `htop` evidence — load average well above core count).
2. While saturated, type in the coordinator pane and judge latency subjectively. Expected: normal, responsive typing — no more competing on equal footing with worker CPU load.
3. Spot-check that the clamp actually landed: `ps -o pid,pri,state,command -p <worker_shell_pid>` and any bazel/cargo/rustc descendants should show a lower scheduling priority than the Boss app's own pid. `taskpolicy -p <pid>` run with no other flags is not a query command, so use `ps -o pri=` or Instruments' "Darwin BG" thread state to confirm the flag is set.
4. Workers may finish somewhat slower under contention now than before — that trade is intentional and accepted (see rescope above). If throughput regresses badly rather than just "somewhat slower," that's the signal to reach for the disable flag above and file a followup on tuning (e.g. a lighter QoS clamp than full background, or exempting the bazel daemon specifically).
