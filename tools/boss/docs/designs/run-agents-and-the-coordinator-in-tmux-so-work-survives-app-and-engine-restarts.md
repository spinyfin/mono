# Boss: Workers outlive their supervisor — hosting agent panes and the coordinator in tmux

- **Status:** design proposal (not yet implemented). `kind=design` deliverable — architecture, failure-mode analysis and a dependency-ordered task list. No code.
- **Project:** `proj_18c8288509223d90_21` — "Run agents and the coordinator in tmux so work survives app and engine restarts".
- **Provenance:** design execution `exec_18c831d64a2ab588_6`.
- **Related design docs:** [`distributed-agent-execution-register-and-dispatch-to-remote-ssh-hosts.md`](./distributed-agent-execution-register-and-dispatch-to-remote-ssh-hosts.md) (the existing detached-worker precedent this doc must answer to); [`fleet-scaling-dynamic-panes-and-team-semantics.md`](./fleet-scaling-dynamic-panes-and-team-semantics.md) (slot-model direction of travel); [`worker-live-status.md`](./worker-live-status.md) (the per-slot live-state surface re-adoption must rebuild).
- **Related operational docs:** [`../worker-liveness-contract.md`](../worker-liveness-contract.md) (the three-layer liveness contract and the re-adopt/reap rule this design extends); [`../post-crash-recovery.md`](../post-crash-recovery.md); [`../crash-watchdog.md`](../crash-watchdog.md).
- **Related prior art in-tree:** [`../../../../docs/claude-tmux-pane-controller.md`](../../../../docs/claude-tmux-pane-controller.md) — an earlier, un-adopted proposal for driving `claude` inside tmux via `send-keys` / `capture-pane`. Its measured findings on prompt submission and busy detection are reused here; its overall framing (tmux as an _automation_ surface) is not the framing of this doc (tmux as a _durability_ surface).

## TL;DR — and the property you are being asked to agree to

The engine and the app currently own the lifetime of every agent process they supervise. This design **ends that ownership**: a worker becomes a process the engine can observe, address and terminate, but does not contain. That is the contested bet, and it is not free — several of the engine's existing safety arguments rest on "we spawned it, therefore we can always kill it".

The mechanism is one detached tmux session per execution, on a private tmux server, addressed by a random token the engine mints and commits to `state.db` _before_ the session exists. Re-adoption is an exact token match read back from the live tmux server, or it does not happen.

What you get: an engine restart mid-turn loses nothing; an app restart loses nothing; a worker parked in a slow build stops being reaped by a 30-minute timer. What you pay: a worker can now outlive the _decision_ to end it, so leak detection and teardown become first-class engine responsibilities rather than a side effect of process containment.

## Goals

- **A worker survives its supervisors.** An engine exit, an engine upgrade, an app crash, an app relaunch, or a `Cmd-Q` must leave the agent process running and its in-flight repo work intact.
- **On restart the engine re-adopts, or restarts the work visibly.** There is no third outcome. No silent orphaning, no worker running untracked, no re-dispatch on top of a live worker.
- **Re-adoption keys on an unambiguous durable pointer the system itself wrote.** Not session names, not window titles, not process-tree shape. On any doubt, restart fresh.
- **"Hook silence" stops meaning "dead".** A live tmux session gives the reconciler signals it does not have today. A worker quiet for 30 minutes inside a legitimately slow build must be distinguishable from a wedged one and from a dead one.
- **The coordinator gets the same process survival as a worker**, with an honest account of what does and does not carry across a restart.
- **The engine stays the owner of reconciliation; the app stays a thin client.** No re-adoption logic in Swift.
- **tmux is a hard dependency, and its absence fails loudly at startup.** Never a silent fallback to today's mode.

## Non-goals

- **Surviving a machine reboot.** tmux servers do not survive reboot, and this design does not try to make agent work resumable across one. See [Failure modes](#7-failure-modes-and-the-behaviour-chosen-for-each) for why that is the chosen behaviour rather than an omission.
- **Driving the agent through `send-keys` as a control API.** The earlier [`claude-tmux-pane-controller.md`](../../../../docs/claude-tmux-pane-controller.md) proposal treats tmux as an automation surface: screen-scrape state, inject prompts, infer turn boundaries from footer text. This design uses tmux only as a pty host. Turn boundaries continue to come from the driver's hook/JSONL stream, not from `capture-pane` parsing. The one place `send-keys` is used — probe injection — is a straight replacement for a write the app already performs.
- **Replacing the hook/events channel.** `boss-event` over the Unix socket stays exactly as it is. It already survives engine restarts (see [What is already durable](#what-is-already-durable-and-what-is-not)).
- **Changing the slot model.** Slots stay 1-16 interactive / 17-24 automation / 25-32 review, allocated and released by the engine exactly as today. The tmux identity scheme is deliberately _subordinate_ to slot identity, not a competing one.
- **Making remote (SSH) workers tmux-hosted.** Remote workers already run detached and already re-attach. Converting them is a follow-up, listed as deferred.
- **A cross-subsystem handshake between tmux teardown and cube lease reclamation.** Chore-stuckness and lease-stuckness are independent bugs in separate subsystems, and this design keeps them that way. See [Lifecycle and cleanup](#6-lifecycle-and-cleanup).
- **Running agents headless.** Dropping the interactive pane entirely would dissolve the pty problem, but it is a different and larger project. Recorded as an alternative and as a deferred entry, not silently omitted.

## Current architecture, established with citations

### The spawn path, and who owns the pty

The engine does **not** spawn workers. It asks the app to.

1. `boss_engine::spawn_flow::start_worker` writes the workspace files, builds the env allowlist, and sends `EngineToAppRequest::SpawnWorkerPane` over the engine→app RPC — `tools/boss/engine/core/src/spawn_flow.rs:299` (entry), `:435` (the RPC send), `:45` (the sanitized worker `PATH`, which already puts `/opt/homebrew/bin` first).
2. The app handles it in `WorkersWorkspaceModel.spawnWorkerPane` — `tools/boss/app-macos/Sources/Ghostty/WorkersWorkspaceModel.swift:131`. It constructs a `TerminalLaunchSpec` (`tools/boss/app-macos/Sources/Ghostty/TerminalPaneSession.swift:75`) and a `TerminalPaneSession`, and returns `shell_pid: 0` immediately (`:245`) because the libghostty surface is created asynchronously by SwiftUI after the RPC returns.
3. The real shell pid arrives later, via `onSurfaceAttached` → `ghostty_surface_foreground_pid` → `onShellPidAvailable` → `UpdateWorkerShellPid` back to the engine (`WorkersWorkspaceModel.swift:203`).

So the process tree today is: **`Boss.app` → libghostty surface (owns the pty master fd) → login shell → `claude`/`codex`/`grok`**. The engine is a _sibling_ of that tree, not an ancestor — it is a separately-launched detached process (`tools/boss/app-macos/Sources/EngineProcessController.swift:333`).

Two consequences follow directly, and both are already documented in-tree as incident causes:

- **The app dying kills every worker.** `tools/boss/engine/core/src/dead_pane_sweep.rs:1-12` states it plainly: _"A libghostty worker pane is a child of the macOS app process. When the app relaunches (an update, a crash, an operator restart) every live worker's shell dies with it."_ The app knows this and warns the user at quit time: _"Quitting will terminate them and discard any unsaved progress"_ — `tools/boss/app-macos/Sources/BossMacApp.swift:511-541`.
- **The engine dying is survivable for the process but not for the tracking.** The agent process is not a child of the engine, so it keeps running; but `LiveWorkerStateRegistry`, the `WorkerPool` claim table and `WorkerRegistry`'s pid map are all in-memory and empty on boot (`../worker-liveness-contract.md`, "Derived bookkeeping" row). The engine also actively tears workers down on a clean shutdown — `ServerState::shutdown_workers`, `tools/boss/engine/core/src/app.rs:1759`.

### Every channel between the engine and a worker

Each of these has to keep working across a supervisor restart, so each is enumerated with what it depends on.

| Channel                  | Direction            | Mechanism                                                                                                                                                              | Depends on the app? | Survives engine restart today?                                                                                                                                              |
| ------------------------ | -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Hook / progress events   | worker → engine      | `boss-event` shim writes the payload to `$BOSS_EVENTS_SOCKET`; engine reads peer pid and `_boss_run_id` (`tools/boss/engine/core/src/events_socket.rs:42`)             | No                  | **Yes** — the shim retries, then buffers to `.boss/events-pending.jsonl` in the workspace and drains on reconnect (`tools/boss/event-shim/src/main.rs:18-42`, `:74`, `:78`) |
| Turn / stop boundary     | worker → engine      | Resolved by the driver from the same event stream, not by screen state (`events_socket.rs:57`)                                                                         | No                  | Yes, as above                                                                                                                                                               |
| Transcript capture       | engine reads a file  | `work_runs.transcript_path`, the driver's own JSONL, parsed through `AgentDriver::normalize_transcript_entry` (`tools/boss/engine/core/src/driver_transcript.rs:1-31`) | No                  | **Yes** — it is a path on disk, not pane scrollback                                                                                                                         |
| Live-status summary      | engine reads a file  | The summarizer tails the same transcript path                                                                                                                          | No                  | Yes                                                                                                                                                                         |
| Probe / notice injection | engine → worker      | `EngineToAppRequest::SendToPane` → app writes bytes to the pty (`tools/boss/engine/core/src/app/pane_delivery.rs:1-40`, `:441`)                                        | **Yes**             | No — needs a live app session                                                                                                                                               |
| Interrupt / focus        | engine → worker      | `InterruptWorkerPane` / `FocusWorkerPane` RPCs                                                                                                                         | **Yes**             | No                                                                                                                                                                          |
| Spawn / release          | engine → app         | `SpawnWorkerPane` / `ReleaseWorkerPane`                                                                                                                                | **Yes**             | No                                                                                                                                                                          |
| Pane inventory           | engine → app         | `ListHostedPanes` (`WorkersWorkspaceModel.swift:326`), diffed against the engine's live set by `husk_pane_sweep`                                                       | **Yes**             | No                                                                                                                                                                          |
| Liveness                 | engine probes the OS | `kill(pid, 0)` against `work_runs.shell_pid` (`tools/boss/engine/core/src/durable_liveness.rs`)                                                                        | No                  | Yes                                                                                                                                                                         |

The shape of that table is the design's biggest single finding: **the observation channels are already restart-durable; only the pty-owning control channels are not.** Four RPCs (`SpawnWorkerPane`, `ReleaseWorkerPane`, `SendToPane`, `InterruptWorkerPane`) plus `ListHostedPanes` are the entire surface that has to move.

### What is already durable, and what is not

Durable, per execution, in `state.db` (`tools/boss/engine/core/src/work/schema_init.rs`):

- `work_executions` (`:190`): `id`, `work_item_id`, `kind`, `status`, `repo_remote_url`, `cube_repo_id`, `cube_lease_id`, `cube_workspace_id`, `workspace_path`, `preferred_workspace_id`, timestamps.
- `work_runs` (`:210`): `id`, `execution_id`, `agent_id`, `status`, `transcript_path`, `artifacts_path`, `host_id`, `cube_workspace_id`, `remote_pid`, `shell_pid`.
- `work_executions.driver_runtime_state` — a driver-owned JSON blob, deliberately preserved across workspace teardown (`tools/boss/engine/core/src/driver_teardown.rs:488`) and used by the Codex/Grok home-retention sweeps to find per-run state on disk.
- Recovery patches keyed by execution id under the Boss data directory (`tools/boss/engine/recovery/src/lib.rs`), captured when an execution dies with uncommitted work and replayed into the resuming worker's workspace.
- A `metadata` key/value table (`schema_init.rs:107`) for engine-scoped singletons.

**Not durable, and rebuilt only from hook traffic:** `LiveWorkerStateRegistry` (slot → activity/model/tool/`last_event_at`), the `WorkerPool` claim table, and `WorkerRegistry`'s pid→run and run→slot maps.

**Not recorded at all today:** anything identifying the _host container_ of the agent process. There is a pid and a workspace path; there is no handle.

### The liveness and reaping ladder today

- `dead_pid_sweep` — `kill(pid, 0)` driven by the in-memory registry. Blind after an engine restart.
- `dead_pane_sweep` — the restart-robust version: reads `work_runs.shell_pid` from the DB instead (`dead_pane_sweep.rs:37-50`).
- `stale_worker_sweep` — the wedged-but-alive detector. Fires when a slot is `activity == Working`, has **no** `current_tool` in flight, and `last_event_at` is older than `DEFAULT_STALE_THRESHOLD_SECS = 1_800` (`tools/boss/engine/core/src/stale_worker_sweep.rs:105`), past a `STALE_GRACE_SECS = 60` guard on `started_at` (`:111`). On firing it reaps the process tree, marks the execution `orphaned`, releases the slot, emits `stale_worker_reconcile`, and kicks the orphan sweep to redispatch (`:21-41`). Drivers declaring `ProgressFidelity::Coarse` or `Minimal` are exempted from it entirely — a named, accepted gap (`:58-86`).
- `husk_pane_sweep` — asks the app what it hosts, diffs against the engine's live set, and retires panes the engine has forgotten after two consecutive confirming passes (`tools/boss/engine/core/src/husk_pane_sweep.rs:1-50`, `:198`, `:227`).
- `orphan_sweep` — redispatches `active` work items with no live execution, guarded by a durable-pid probe that refuses to redispatch onto a live process (`tools/boss/engine/core/src/orphan_sweep.rs:33-38`).
- `run_reconcile` — the boot-time probe. Consults **cube lease state**, deliberately not the events socket, and returns `Live` / `Dead` / `Unknown`, with `Unknown` treated as `Live` for dispatch purposes (`tools/boss/engine/core/src/run_reconcile.rs:1-38`).
- `worker_readoption` — the convergence policy for "the engine says terminal, the worker says alive": re-adopt when the terminal status was _inferred_ (`orphaned` / `abandoned`) and nothing else is live on the row; reap when it was _decided_ (`cancelled` / `completed` / `failed`) or a newer execution owns the row (`tools/boss/engine/core/src/worker_readoption.rs:155-177`).

That last module is important context: **the engine already has a re-adoption policy.** What it lacks is a re-adoption _mechanism_ — a way to find the worker again after the in-memory maps are gone.

### The slot model

- Interactive pool 1-16, as two display pages of 8 (`WORKER_PAGE_SIZE = 8`, `WORKER_PAGE_COUNT = 2`, `MAX_WORKER_POOL_SIZE = 16` — `tools/boss/engine/core/src/coordinator.rs:169`, `:180`, `:190`). Bridge Crew 1-8 fills before Lower Decks 9-16 spills.
- Automation pool 17-24 (`MAX_AUTOMATION_POOL_SIZE = 8`, `:196`).
- Review pool 25-32 (`MAX_REVIEW_POOL_SIZE = 8`, `:203`; the live count is pushed to the app via `EnginePoolConfig` so the two never drift — `WorkersWorkspaceModel.swift:104-115`).
- Remote runs get synthetic slots from `REMOTE_SLOT_BASE = 200` because they hold no pane at all (`tools/boss/engine/core/src/worker_registry.rs:45`).
- The engine is the sole allocator. The app hosts the slot the engine names or fails with `SlotBusy`; a mismatch is a `debug_assert` (`spawn_flow.rs:530`).
- Slot maps deterministically to a display name (`WorkerNames.swift:22-31`), which is what makes "Crusher is stuck" a usable sentence.

The existing rule this design must not violate: **slot is worker identity; the cube workspace id is ephemeral scratch.** The remote-execution design states the workspace half explicitly — _"`workspace_path` remains on the run but is interpreted only on the host that produced it — it is a debugging aid, not durable identity"_ (`distributed-agent-execution-register-and-dispatch-to-remote-ssh-hosts.md:284`).

### The coordinator, and how it differs

- Launched by the **app**, not the engine: `BossPaneModel` owns a single libghostty surface running `claude --model <m> --permission-mode auto` (`tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift:8-10`, `:42-105`).
- Its working directory is a dedicated tree under Application Support, not a cube workspace (`:172-180`).
- It has **no** execution row, no run row, no slot, no cube lease, and no `BOSS_RUN_ID`. Nothing about it is visible in `state.db`.
- It restarts itself when the child exits, after a 1.5 s delay (`:97-104`), and restarts eagerly when the engine pushes a different `coordinator_model` (`:130-136`).
- Its launch line is `exec`-based: `[ -n "$BOSS_BIN_DIR" ] && export PATH=…; unset ANTHROPIC_API_KEY; exec claude …` (`:138-140`). **This matters:** the surface already runs an arbitrary shell line and `exec`s into it, which is exactly the hook a `tmux attach` needs.

### A gap with no recorded decision behind it

The motivating incident notes that _"the running app neither restarted its engine nor reattached when a healthy one came back"_. That is not a bug in a supervision policy — there is no supervision policy.

`EngineProcessController.start()` is called exactly once, from `ChatViewModel.startIfNeeded()` at launch (`tools/boss/app-macos/Sources/ChatViewModel.swift:1787-1804`). `restart()` has only human callers: the engine-down banner button (`ContentView.swift:460`) and two Settings actions (`Settings/SettingsView.swift:223`, `:237`). `EngineClient` retries the _socket_ with a 0.5→30 s backoff (`EngineClient.swift:35`), but nothing ever relaunches the process.

Searching the design docs and the controller's own comments turns up no rationale for this — no "the engine is deliberately not supervised because…". The stronger reading is not that a reason existed and was lost; it is that the decision was never made. It is recorded here as a finding, and it gets its own task entry, because tmux-hosting delivers _survival_ without delivering _recovery_ if nothing ever brings the engine back.

## Alternatives considered

### A. Detached `nohup` processes with no multiplexer — what remote workers already do

This is the strongest alternative precisely because **this project already relies on it in production**. Remote SSH workers are launched detached with `nohup`, survive an engine restart by construction, and are re-attached on boot by `remote_reattach.rs`, which re-establishes the reverse events forward for every non-terminal run on a non-local host (`tools/boss/engine/core/src/remote_reattach.rs:1-25`). It even has a post-launch liveness ack to catch a worker that launched and died immediately (`tools/boss/engine/core/src/ssh_spawn.rs:60-70`).

So the rejection cannot be "detached processes are fragile" — that would disqualify an approach already carrying production traffic.

**Why it does not transfer to local workers:** the remote path buys its durability by _giving up interactive attachment_. A remote worker holds no pane at all; that is why `worker_registry.rs:45` hands it a synthetic slot from a disjoint 200+ range rather than a pool slot. `nohup` gives you a surviving process and a pty you can never see again. Local workers cannot make that trade, because three production paths depend on the pane being attachable and writable:

- probe and chore-notice injection through `inject_pane_text_verified` (`app/pane_delivery.rs`),
- operator focus and takeover (`FocusWorkerPane`, the Workers grid),
- the human reading a running agent's screen, which is the entire operator loop today.

That requirement is real and predates this design; it is not an artifact of having already picked tmux. A terminal multiplexer is the minimum thing that supplies "detached _and_ re-attachable".

### B. Keep libghostty ownership, make the app restart-transparent

Re-parent the worker shell out of the app's process group (double-fork, `setsid`) and have a relaunched app reclaim the pty.

Rejected on a concrete mechanism failure, not a label: the pty **master** fd lives in the app's address space and is released by `ghostty_surface_free` when the surface is torn down (`WorkersWorkspaceModel.swift:250-268` documents this teardown). A pty master cannot be handed to a process that does not yet exist. Making it survivable requires a long-lived process that holds the master and brokers reconnections — which is a terminal multiplexer, arrived at by a worse route.

### C. Build a Boss-owned pty broker daemon

Same shape as B, honestly: a `bossd-pty` daemon holding masters and serving reconnects.

Rejected on cost and on evidence quality. We would own reconnection, scrollback ring buffers, resize propagation, and alternate-screen semantics — against three different agent TUIs (Claude Code, Codex, Grok), each of which we would be debugging in production. And a hand-built reproduction of a multiplexer is structurally unable to surface the integration bugs that matter, because it is built from the same beliefs that produced it; only the real end-to-end path finds those. tmux is a twenty-year-old implementation of exactly this daemon.

### D. `dtach` / `abduco` / `screen`

`dtach` and `abduco` are the minimal versions of C and are attractive for that reason. They are rejected on a specific, checkable deficiency: **they expose no queryable session metadata.** There is no equivalent of `set-option @key value` / `show-environment` / `list-sessions -F '#{…}'`, which is exactly the substrate the durable re-adoption pointer and the liveness redefinition are built on. Without it we would be back to matching on socket filenames — i.e. name-match resume, which is explicitly out of scope. `screen` has the session list but a substantially weaker format/query surface and no per-session user options.

### E. Run agents headless and drop the pane entirely

`claude -p` (and the equivalents) removes the pty problem completely rather than relocating it.

Not chosen, but not dismissed either: it is a strictly larger change that alters the operator model (no attach, no takeover, no `SendToPane`), the permission model, and the driver abstraction's interactive-TUI support. It is recorded as a deferred entry so the option stays visible rather than being silently foreclosed by this design. Note it is _not_ mutually exclusive with tmux hosting — a headless worker could still run inside a tmux session for uniform lifecycle handling.

## Chosen approach

### 1. Session topology and identity

**One detached tmux session per execution, one window, one pane, on a private tmux server.**

- Private server: every production invocation uses `tmux -S <state-root>/tmux.sock` (next to `state.db` and `events.sock`). An explicit socket path, not a `-L` label, is what keeps Boss off tmux's default `/tmp/tmux-<uid>/` directory — that directory is cleaned on reboot and by `/tmp` sweepers, which is how a coordinator session can vanish while the rest of engine state survives. The tradeoff versus a label: `-L boss` was shorter and isolated from the operator's default server, but the resulting socket still lived under `/tmp` and was invisible to fixture-isolation gates that compare resolved paths. The socket path is resolved once onto `WorkConfig` (and `EnginePaths`) so a fixture cannot re-derive production's server from `$BOSS_DB_PATH`. `kill-server` still scopes cleanly to that one socket.
- One session per **execution**, not per slot. A per-slot session would be reused across executions, which makes the durable pointer mutable and makes teardown ambiguous — "is this session's content the execution that just finished or the one that just started?". Per-execution sessions are created and destroyed with the execution and their token is immutable for the session's whole life.
- Session name: `boss-<slot>-<short-exec-id>`, e.g. `boss-6-1d64a2ab588`. **The name is display ergonomics for a human running `tmux -S <state-root>/tmux.sock ls`. It is never identity.**

**Relationship to slot identity.** Slot identity is unchanged and remains authoritative: the `WorkerPool` claim, the `LiveWorkerState` key, the display name, and the pane the operator looks at are all slot-keyed exactly as today. The tmux session is _addressed by_ the engine through one row in `state.db`; it introduces no second notion of who a worker is. Concretely, the mapping is `execution_id → (session_name, spawn_token)`, and slot never appears on the left-hand side of a lookup.

**The invariant, stated at the level that is load-bearing.** It is tempting to write this as "session names are unique". That is the wrong level — an implementation can satisfy it and still adopt the wrong session, because a human, a stale script, or a previous Boss install can recreate a name. The property that actually has to hold is:

> **No tmux session is ever treated as belonging to an execution unless the engine can read back, from the live tmux server, the exact secret it minted for that execution.**

Everything below is in service of that sentence.

And the equivalence caveat: session name and execution id are equivalent **for display and for human debugging only**. They are not equivalent for identity, and no adoption, teardown, or injection decision may resolve one from the other.

### 2. The durable re-adoption pointer, and its write ordering

**The pointer is a 128-bit random `spawn_token`, minted by the engine, committed to `state.db` before the session exists, and carried into the session atomically at creation.**

New columns on `work_runs`:

| Column              | Meaning                                                                                                                                                                            |
| ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tmux_server_label` | Server identity: the absolute `-S` socket path, or the literal `boss` for a session still on the pre-move `-L boss` server. Recorded so a future relocate does not orphan old rows |
| `tmux_session_name` | Display/addressing name. Never used for matching                                                                                                                                   |
| `tmux_spawn_token`  | The secret. Unique index. The only thing adoption matches on                                                                                                                       |
| `tmux_spawn_state`  | `intended` → `created`. Distinguishes the two crash windows                                                                                                                        |
| `tmux_pane_pid`     | `#{pane_pid}` read back at creation; supersedes today's async `shell_pid` report                                                                                                   |

Write ordering, and why each step is where it is:

1. **Mint** `spawn_token`.
2. **Commit** `(tmux_server_label, tmux_session_name, tmux_spawn_token, tmux_spawn_state='intended')` to `work_runs`. _This commit strictly precedes any tmux call._ That ordering is the whole safety argument: it makes "a live Boss session whose token the DB has never seen" an impossible state under normal operation, so encountering one is unambiguous evidence of DB loss rather than a race to reason about.
3. **Create** the session:
   `tmux -S <state-root>/tmux.sock new-session -d -s <name> -e BOSS_SPAWN_TOKEN=<token> -e BOSS_SESSION_SCHEMA=<n> -e BOSS_RUN_ID=<exec> -e … -c <workspace> <agent-command>`
   The `-e` flags set session environment **atomically with session creation** — there is no window in which the session exists without its token. (Validated on tmux 3.6a; `-e` on `new-session` requires tmux ≥ 3.2, which sets the version floor.)
4. **Label and confirm**: set the mirror user option `tmux set-option -t <name> @boss_spawn_token <token>` (so one `list-sessions -F` call can enumerate tokens cheaply), read `#{pane_pid}`, then write `tmux_spawn_state='created'` and `tmux_pane_pid`.

The environment variable is the **authority**; the user option is a convenience mirror. Where they disagree, the environment wins and the disagreement is logged.

**Crash-window taxonomy.** Every partially-written record has a distinct, checkable signature:

| Crash point     | `state.db` says        | tmux server says                                              | Verdict                                                                                                                      |
| --------------- | ---------------------- | ------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| Before step 2   | no token row           | no session                                                    | Nothing happened. Ordinary dispatch.                                                                                         |
| Between 2 and 3 | `intended`             | no session carrying that token                                | The spawn never happened. **Restart fresh** — fail the run, release the slot, let the existing reconcile paths run.          |
| Between 3 and 4 | `intended`             | session exists, env token matches, `@boss_spawn_token` absent | **Adopt.** This is the known, benign mid-spawn signature. Repair the user option and advance `tmux_spawn_state` on adoption. |
| After 4         | `created`              | session + token                                               | **Adopt.**                                                                                                                   |
| Any             | `intended` / `created` | session gone                                                  | The worker died. Existing dead-worker reconciliation, now with `pane_dead_status` as the recorded cause when available.      |
| Any             | no row for that token  | a session carrying a Boss-shaped token                        | **Leaked.** Log loudly, emit a dispatch event, confirm on a second pass, then reap. Never silently.                          |

**Boot-time adoption algorithm** (engine-side, before the existing `run_reconcile` pass):

1. `tmux -S <state-root>/tmux.sock list-sessions -F '#{session_name}\t#{@boss_spawn_token}'`. If the server is not running the command fails with `no server running on …` (or `error connecting to … (Connection refused)` for a leftover socket file) — that is a clean zero-session answer, not an error, and every non-terminal local run falls through to existing reconciliation. A leftover socket file with no listener is unlinked at engine start before this command runs.
2. For each session, read the authoritative token with `show-environment -t <name> BOSS_SPAWN_TOKEN`. Bounded work: at most 32 local slots plus the coordinator.
3. Look the token up in `work_runs`. **Exact full-token match or no adoption.** No prefix matching, no name fallback, no "closest execution".
4. Token found, execution non-terminal → **adopt**: re-claim the slot recorded on the run, restore `WorkerRegistry`'s run→slot and pid→run entries from `tmux_pane_pid`, register `LiveWorkerState` seeded from durable state, and restart the live-status summarizer against `work_runs.transcript_path`. The worker's buffered hook events (`.boss/events-pending.jsonl`) drain on its next hook and reconcile activity naturally.
5. Token found, execution terminal → hand straight to `worker_readoption::classify_contradiction` (`worker_readoption.rs:155`). That policy is unchanged and needs no new cases: a live session is simply a new, stronger way of observing the same contradiction it already handles.
6. Token not found → leaked. See [Lifecycle and cleanup](#6-lifecycle-and-cleanup).

**Migrating an already-running `-L boss` server.** tmux cannot relocate a live server's socket. On the first boot after the socket move, the engine also constructs a label-addressed handle (`Tmux::for_legacy_label_server`), lists that server, and runs the same adoption pass against it. Matching runs stay registered in `WorkerPool` / `WorkerRegistry` / `LiveWorkerState` for the rest of their life, addressed via `-L boss`; their `tmux_server_label` remains the literal `boss` so the stale-worker inspector probes the right server. An attention item names each surviving session and the exact `tmux -L boss attach` / `kill-session` command. New sessions are created only on the durable socket. The old server is not killed automatically: killing it would SIGTERM live agents. Once those runs finish, the label server is empty and the drain is a no-op.

**Precedence over the existing lease probe.** `run_reconcile`'s cube-lease oracle stays, but it runs _after_ tmux adoption and only over what adoption did not claim. A token match is direct evidence about the worker; a green lease is circumstantial evidence about a directory. Adoption outranking it also removes the `Unknown`-treated-as-`Live` limbo for the majority of cases (`run_reconcile.rs:29-33`).

**One ambiguity that disappears entirely.** Today `SpawnWorkerPane` can time out with a genuinely unknown outcome — the app may or may not have hosted the pane — and the engine has to register the slot provisionally with `shell_pid: 0` and leave `spawn_ack_sweep` to reconcile (`spawn_flow.rs:456-522`). `tmux new-session -d` is a local, synchronous command with a definite exit status, and the engine can independently verify by reading the token back. The provisional-registration path and its ack-timeout branch become unnecessary for tmux-hosted workers.

### 3. What the app becomes

The app stops owning worker ptys and becomes an attacher.

**What changes:**

- `TerminalLaunchSpec` for a worker pane no longer carries env, cwd, or an agent command. Its `initialInput` becomes `exec tmux -S <socket> attach-session -t <session>`, with `<socket>` taken from the same `Tmux` handle that created the session. This is the same mechanism `BossPaneModel` already uses for the coordinator, so it is a substitution rather than a new capability.
- `SpawnWorkerPane` becomes `AttachWorkerPane { slot_id, session_name, summary, task_title }`. The engine has already created the session and already knows the pid; the app is told what to display.
- `ReleaseWorkerPane` becomes `DetachWorkerPane`, and **must stop killing anything**. `WorkerProcessKiller.killForegroundProcessTree` leaves the worker path entirely (`WorkersWorkspaceModel.swift:310-317`) — under tmux, killing what the app is merely _viewing_ is precisely the bug this project exists to prevent. Detach tears down the surface and nothing else.
- `SendToPane` and `InterruptWorkerPane` move engine-side, onto `tmux send-keys` (two-phase: `send-keys -l <text>` then a separate `send-keys C-m`, per the measured finding in `claude-tmux-pane-controller.md:309-335`, re-validated here). `inject_pane_text_verified`'s posture checks and delivery-verification semantics are unchanged — only the transport moves.
- `ListHostedPanes` is replaced as the pane-inventory authority by the tmux server. `husk_pane_sweep` keeps its exact shape — enumerate, diff against the engine's live set, two-pass confirm, retire — but asks tmux. This is a strict improvement: the sweep now works with the app closed.

**What improves as a side effect:**

- `work_runs.shell_pid` becomes `#{pane_pid}` read synchronously at creation, replacing the async `onSurfaceAttached` → `foregroundPid` round trip that can legitimately return 0 (`WorkersWorkspaceModel.swift:196-225`, `spawn_flow.rs:543-551`).
- Scrollback moves into tmux (`history-limit`), so quitting the app no longer discards it.
- With `remain-on-exit on`, a pane whose agent exited stays inspectable with `#{pane_dead}=1` and `#{pane_dead_status}=<exit code>` (validated). Today an exited agent's pane simply vanishes, and the exit status is lost.

**What is explicitly unaffected** — worth stating because reviewers will ask:

- **Transcript capture.** It already reads the driver's own JSONL at `work_runs.transcript_path` (`driver_transcript.rs:1-31`), never pane scrollback. Nothing about it changes.
- **The kanban live view.** It paints from `LiveWorkerState.activity` (`../worker-liveness-contract.md`), which the engine rebuilds during adoption.
- **The app acquires no liveness signal of its own.** The liveness contract's rule — _"The app has no independent liveness signal and must not acquire one"_ — is preserved. In particular the app must not run `tmux has-session` to decide what to render.

### 4. The coordinator

**What is different about it:** it is interactive and human-typed-into; it holds conversation context that exists only in the live `claude` process; it is app-launched today; and it has no representation in `state.db` at all — no execution, no run, no slot, no lease.

**Same mechanism, changed ownership.** The coordinator moves into the same tmux server as session `boss-coordinator`, and its launch moves from the app to the engine. That relocation is not incidental: keeping it app-launched would put re-adoption logic in Swift, violating the engine-owns-reconciliation principle. The app's `BossPaneModel` becomes an attacher exactly like a worker pane.

Its durable pointer cannot live on a `work_runs` row, so it gets a singleton in the existing `metadata` key/value table (`schema_init.rs:107`): `coordinator.tmux_session_name`, `coordinator.tmux_spawn_token`, `coordinator.tmux_spawn_state`. The same write ordering and the same crash-window taxonomy apply unchanged.

**Being honest about what survives.**

What genuinely carries across an app restart, an app crash, or an engine restart:

- the `claude` process itself, with its full in-memory conversation state,
- any turn that was in flight,
- the terminal scrollback.

Reattaching after an app relaunch drops you back into the same live conversation. That is a real improvement over today, where an app restart kills the coordinator outright.

What does **not** survive, and must not be implied to:

- **The tmux server dying, or a machine reboot.** The conversation is gone. There is no persistence layer under it.
- **The coordinator process itself exiting.** Today's auto-restart (`BossPaneModel.swift:97-104`) is preserved but moves engine-side, and the new process starts a _new_ Claude Code session. It is worth being precise here because it is easy to over-promise: `claude --continue` / `--resume` restores the _transcript_ from Claude Code's session file, not the model's live state, and the coordinator's system prompt is regenerated on every launch (`BossPaneModel.swift:408`). Whether to wire `--continue` into the restart path is left as an open question rather than asserted as continuity.
- **A coordinator model change.** `updateCoordinatorModel` currently restarts the surface to apply a new model (`:130-136`). Under tmux that becomes "kill and recreate the coordinator session", which loses the conversation — as it does today. No regression, but it turns a silent surface restart into a visible, deliberate destructive action, and the UI should say so before doing it.

Stated plainly: **the coordinator gains process survival, not context survival, and only for the lifetime of the tmux server.**

### 5. Liveness and reaping, redefined

Today the engine has one positive liveness signal for a local worker — `kill(pid, 0)` — and one negative progress signal — hook silence. "Wedged" is defined as the conjunction: process alive, `activity == Working`, no `current_tool`, no hook for 1800 s (`stale_worker_sweep.rs:21-41`, `:105`).

That definition produced the incident's third bullet: a worker whose backgrounded `bazel test` was still in its "Analyzing" phase was reaped after 1800 s of hook silence while it was alive and its work sat complete-but-unpushed.

**New signals, all available from a detached tmux session and all measured** (see [What the tmux probes settled](#what-the-tmux-probes-settled-and-what-they-did-not)):

1. **Session existence with token match.** Binary, authoritative, survives engine restart. This becomes the primary liveness oracle for local workers. The pid stays as a corroborator, not the source of truth — per the liveness contract, _"a pid is a durable number, not a durable handle"_, whereas a session plus its minted token is a handle.
2. **`#{pane_dead}` and `#{pane_dead_status}`** under `remain-on-exit on`. The agent process exited, and here is its exit code. **This is information the system does not have today.**
3. **`#{window_activity}`** — advances whenever the pane produces output, _including while the session is detached_. This is the output-liveness signal, and it is driver-agnostic.
4. **`#{pane_current_command}`** — the foreground command in the pane. Distinguishes "parked inside the agent" from "inside a long foreground child".

**The three-way classification:**

| Class                         | Signals                                                                                                                                                                      | Reconciler action                                                                                                                                                                                                                                                                                                                   |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **alive-and-working**         | session + token present, `pane_dead=0`, **and any of**: a hook within threshold; `window_activity` advanced within threshold; `pane_current_command` is not the agent binary | Nothing.                                                                                                                                                                                                                                                                                                                            |
| **alive-and-genuinely-stuck** | session + token present, `pane_dead=0`, **and all of**: no hook, no `window_activity` advance, `pane_current_command` _is_ the agent binary — sustained past the threshold   | **Escalate, do not silently reap.** Raise an attention item naming the execution, the session, the last output time, and the exact `tmux -S <state-root>/tmux.sock attach -t <name>` command an operator can run to look. Auto-reap only after a second, much longer window, and only with the reason recorded on a dispatch event. |
| **dead**                      | session absent (token unresolvable) **or** `pane_dead=1`                                                                                                                     | Existing reconciliation, now recording `pane_dead_status` as the cause.                                                                                                                                                                                                                                                             |

**Why both output signals are needed, precisely.** A _foreground_ `bazel build` is caught by `pane_current_command` alone. A **backgrounded** `bazel test &` — the actual incident shape — leaves `pane_current_command` as the agent binary, so only `window_activity` catches it, because the build's output still lands in the pane. Either signal alone reproduces the incident; the pair does not.

**Where it still cannot tell.** If the build's output is redirected to a file, neither signal fires and the worker looks identical to a wedged one. That case is not solvable from outside the process, and it is exactly why the middle class escalates rather than reaps. The design does not claim to have eliminated the false positive — it claims to have made the false positive non-destructive.

**A named gap that closes.** `ProgressFidelity::Coarse` and `Minimal` drivers are exempt from cadence staleness today because they emit no per-tool boundary to set `current_tool` (`stale_worker_sweep.rs:58-86`). `window_activity` and `pane_current_command` are properties of the terminal, not of the driver's event vocabulary, so those tiers get live-but-wedged detection for the first time.

### 6. Lifecycle and cleanup

**Who tears a session down: the engine, and only the engine.** The app never kills. `bossctl` asks the engine. There is exactly one verb.

**The teardown sequence**, at every existing terminal path that calls `release_worker_pane` today (completion, cancel, orphan reconcile, husk retire, stale escalation, `bossctl agents stop`):

1. Resolve the session from `work_runs.tmux_session_name`.
2. **Read the token back and require an exact match against `tmux_spawn_token`.** A `kill-session` on a name alone is forbidden — that is the mechanism by which a rebooted-into-reused-name session gets destroyed.
3. Run the existing SIGTERM→SIGKILL ladder against `#{pane_pid}`'s process group (`app/server.rs:2055-2069`). This stays because the reason it exists stays: node-based agents commonly ignore the SIGHUP that pty teardown delivers (`WorkersWorkspaceModel.swift:250-268`).
4. `tmux -S <state-root>/tmux.sock kill-session -t <name>`.
5. Clear the token columns.

**Leaked-session detection** folds into `husk_pane_sweep`, whose shape is already exactly this diff. It enumerates the tmux server and compares against `work_runs` rows with tokens:

- token has no DB row → leak; two-pass confirm, then reap, with a dispatch event.
- token maps to a terminal execution → `worker_readoption::classify_contradiction` answers it, unchanged.
- token maps to a non-terminal execution the engine is not tracking → adopt (the adoption path, re-entered).

Its existing `MAX_UNCONFIRMED_RETIREMENTS_PER_PASS` breaker (`husk_pane_sweep.rs:227`) is retained and matters more here, not less: a bug in token readback could otherwise mass-reap live workers.

**Cube leases are explicitly not part of this.** Teardown never waits on, triggers, or consults lease state, and lease reclamation never consults tmux. `cube_lease_heartbeat` and the ladder-lease reapers keep sole ownership of lease cleanup. The reason to keep the boundary clean is concrete: a handshake would make any tmux bug into a lease leak and any lease bug into a stranded session, and the two subsystems' failure modes are already independently understood. A leaked session and a stuck lease are two bugs, diagnosed and fixed separately.

### 7. Failure modes, and the behaviour chosen for each

**Engine restarts while a worker is mid-turn.** The worker never notices. tmux owns the pty; the agent's stdin/stdout are unaffected. Hook events buffer to `.boss/events-pending.jsonl` and drain on reconnect (`event-shim/src/main.rs:18-42`) — already shipped, no change needed. On boot the engine adopts by token and replays. **This is the headline case, and nothing is lost.**

**Two engines briefly running at once.** Today the pid file plus `withStartLock` guards this (`EngineProcessController.swift:645`). Add a second, tmux-scoped guard: a **server-level user option** `@boss_engine_owner = <engine boot id>`, set with `set-option -s` and checked before adoption. An engine that finds a _different_ owner whose pid is still live refuses to adopt anything and logs loudly rather than double-adopting; the destructive operations (injection, teardown) are what must not race, and refusing adoption is what prevents both. This is adequate for a single-user desktop and is flagged as an open question rather than presented as a general mutual-exclusion solution.

**The tmux server itself dies.** Every session dies with it. Detected in one call — `list-sessions` fails with `no server running on …`. Behaviour: treat every local non-terminal run as dead, reconcile through the existing paths, and raise **one** attention item naming the event, not one per worker. Never silently restart the server and carry on as if nothing happened.

**A worker outlives the task it was dispatched for.** This is the leaked/husk case. `worker_readoption`'s existing policy already answers it correctly: terminal-by-decision → reap; terminal-by-inference with nothing else live → re-adopt.

**Machine reboot.** Nothing survives, by design. On boot the tmux server is absent, no session carries any token, and every non-terminal local run reconciles as dead. This is the right answer rather than a gap: a leased workspace holding a half-finished turn with no process behind it is not resumable, and any mechanism that pretended otherwise would be the silent degradation this project exists to remove. The recovery-patch flow (`engine/recovery`) already captures uncommitted work for the resuming worker and is what carries value across the boundary.

**Version skew — an engine adopting a session an older engine spawned.** Real, because the session's command line, env, and injected `.claude/` settings were written by that older build. Handled by stamping `BOSS_SESSION_SCHEMA=<n>` into the session env atomically at creation. On adoption:

- schema within the supported window → adopt.
- schema unknown or newer than this engine understands → **refuse to adopt, then reap the session before restarting the work.** The refuse-then-reap ordering is load-bearing: refusing to adopt while leaving the session alive and redispatching would put two live workers in one cube workspace — the exact catastrophe every existing guard is built to prevent. The reap is safe because the token identifies precisely what is being killed, and it is recorded as a dispatch event plus an attention item, so it is loud rather than quiet.

**tmux absent or too old.** See below.

### 8. tmux as a hard dependency

- **Where it comes from.** macOS ships no tmux. Homebrew is the supported source; `/opt/homebrew/bin` is already first on the worker's sanitized `PATH` (`spawn_flow.rs:45`). The installer (`tools/boss/installer`) should check for it and say so at install time rather than at first dispatch.
- **Version floor: tmux ≥ 3.2**, the release that added `-e` to `new-session`. Without it there is no way to set the token atomically with session creation, and the entire crash-window taxonomy in §2 collapses. Verified working on the 3.6a currently installed on this host.
- **The engine resolves and records an absolute tmux path at startup** and invokes only that. A bare `tmux` on `PATH` is a moving target across an app upgrade or a PATH change.
- **On absence or a too-old version: refuse to dispatch local workers.** Raise a startup attention item naming the required version and the install command; surface it on the engine-health banner; fail every local dispatch with an explicit reason. There is **no fallback to the app-hosted pty mode.** That fallback would silently reintroduce exactly the durability gap this project closes, behind a mode nobody can see, and a check that cannot run must fail loudly rather than quietly pass.

### 9. Migration and rollback

**Incremental, pool by pool, in increasing blast radius.** Gated on one engine setting, `workers.tmux_hosting`, whose value is a set of pools rather than a boolean:

1. **Review pool (25-32) first.** Reviewer runs are short and non-interactive in practice; a botched reviewer costs a re-review.
2. **Automation pool (17-24).**
3. **Interactive pool (1-16).**
4. **Coordinator last.** It is the one a human is typing into, and the only one whose launch also has to move between processes.

**Operator-facing control: one on/off switch, not a per-pool selector.** The setting's storage stays a pool set — that is what lets the sweep above enable review, then automation, then interactive independently, and what lets a rollback drain one pool without touching the others. But the control the Boss UI actually exposes ("Host workers in tmux" in Settings ▸ Workers) is a single boolean applied to all three pools at once: `true` maps onto the full set, `false` clears it. The Boss UI exposes a single boolean to keep the settings surface simple, superseding the earlier assumption (visible in `SettingsStore::set_tmux_hosting_pools`'s original doc comment) that staged pool enablement would need its own multi-select. Pool-by-pool enablement for the sweep itself is driven by hand-editing `settings.toml` directly, not through the UI toggle or any dedicated CLI verb — `SettingsStore::set_tmux_hosting_pools` is the in-process method that edit round-trips through, but nothing outside `settings.rs` calls it today. A future per-pool selector (UI or CLI) remains possible without a protocol change, since the boolean is a thin projection over the same underlying setting.

**Both spawn paths coexist during migration.** The branch is per-run, on `work_runs.tmux_spawn_token IS NULL`, not on the global setting — so an in-flight legacy worker keeps being handled by the legacy reconcilers even after the setting flips for its pool. Branching reconcilers on a global mode flag would strand exactly the workers that were live at the moment of the flip.

**Rollback.** Flipping a pool back to app-hosted does not need a separate, eager drain step: teardown already keys on the durably-recorded tmux identity columns on `work_runs` (the `work_runs.tmux_spawn_token IS NULL` branch described above), not on this setting, so a tmux-hosted run that is in flight at the moment of the flip is torn down through the normal teardown path (`ServerState::reap_tmux_worker`) exactly as it would be had the setting never changed. `SettingsStore::set_tmux_hosting_enabled(false)` only clears the pool set so that _new_ dispatches for that pool take the legacy app-hosted path — it does not reach into any in-flight session itself, and it does not need to: the existing per-run teardown is what drains the pool as its tmux-hosted runs finish.

**This is not the silent fallback the constraints forbid, and the distinction is worth being precise about.** The forbidden thing is an _automatic_ degradation the system chooses on its own when tmux misbehaves. An _operator-set, logged, UI-visible_ mode is the opposite: it must be stamped on every dispatch event, shown as a badge on the Workers grid whenever any pool is not tmux-hosted, and reported by `bossctl doctor`. If it is not visible in all three places, it is not distinguishable from the forbidden thing and should not ship.

## What the tmux probes settled, and what they did not

**Stated before the fact, per the "choosing versus validating" rule: these probes were run to validate mechanisms of an already-chosen approach, not to choose between approaches.** They can only return "the mechanism works as described" or "it does not". They cannot, and were not asked to, establish that tmux beats `dtach` or a headless worker — that comparison is argued in [Alternatives considered](#alternatives-considered) on requirements, not measured here. Nothing below should later be cited as evidence for the choice itself.

Run against tmux 3.6a (Homebrew, `/opt/homebrew/bin/tmux`) on the host this design was written on:

| Claim                                                                                       | Result                                                                                                   |
| ------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `new-session -d -e K=V` sets session env atomically at creation                             | **Confirmed.** `show-environment -t <s> K` returns the value immediately.                                |
| The pane's process inherits `-e` variables                                                  | **Confirmed.** A pane command echoing `$BOSS_RUN_ID` printed the passed value.                           |
| Session user options are settable and enumerable in one call                                | **Confirmed.** `list-sessions -F '#{@boss_spawn_token}'` returns them.                                   |
| `-L <label>` gives a private server invisible to the default one                            | **Confirmed.** The default server reported `no server running` while the labelled server held a session. |
| Two-phase `send-keys -l <text>` then `send-keys C-m` submits reliably to a detached session | **Confirmed**, matching the earlier finding in `claude-tmux-pane-controller.md:309-335`.                 |
| `remain-on-exit on` preserves a dead pane with its exit status                              | **Confirmed.** `#{pane_dead}=1`, `#{pane_dead_status}=0`, session still present.                         |
| `capture-pane` reflects new output on a **detached** session                                | **Confirmed.** Captured line count advanced 1 → 2 with no client attached.                               |
| `#{window_activity}` advances on output from a detached session                             | **Confirmed.** Advanced by 3 s across a 4 s wait spanning one `echo`.                                    |
| `#{session_activity}` advances on output from a detached session                            | **Refuted.** Unchanged across the same interval.                                                         |

That last row is the one worth carrying forward: `session_activity` is the obvious-looking field and it is the wrong one. Anything built on it would have reported every detached worker as silent from the moment it was created. Use `window_activity`.

## Risks / open questions

- **The contested property, restated for the reviewer.** After this change the engine can no longer guarantee that killing what it spawned kills everything. Several existing safety arguments — most explicitly the asymmetry in `worker_readoption` ("reaping destroys in-flight work and cannot be undone") — were written when process containment was implicit. They remain correct, but their _enforcement_ moves from the OS process tree to token-verified teardown plus a leak sweep. If you do not believe leak detection can be made reliable, you should not accept this design.
- **Two engines at once.** The proposed `@boss_engine_owner` server option plus the existing pid-file lock is adequate for a single-user desktop and is not a general mutual-exclusion mechanism. Worth a reviewer's opinion on whether that is sufficient.
- **`send-keys` as the injection transport.** It is a keystroke stream, not a write to a fd. Chunking, escaping and the two-phase submit are handled (measured), but injecting into a mid-turn agent has driver-dependent semantics that `pane_delivery` already models. Moving transport should not change those semantics; a regression here would be subtle and safety-relevant.
- **Coordinator context on process exit.** Whether to wire `claude --continue` into the engine-side coordinator restart. It restores the transcript, not the model's live state. Recommendation: do not wire it in v1, and do not describe the restart as continuity.
- **`remain-on-exit` cost.** It converts "session disappears" into "session lingers with a dead pane", which is better for post-mortem and worse for leak pressure — every dead pane is now something the sweep must reclaim.
- **Threshold values.** The `window_activity` staleness window and the second, longer auto-reap window are unset in this doc deliberately. They should be chosen from observed data after the first pool is migrated, not guessed here.
- **The engine has no supervisor, and never had a recorded decision to have none.** Fixing it is in scope as its own entry, but the _policy_ (restart always? backoff? give up after N?) is a human call.
- **Scope constraint worth recording rather than filing.** Some work this design implies is not filable as a cube-worker task, because its input or output is coordinator-private runtime state rather than repo content: clearing a wedged `coordinator.tmux_*` record on a live installation, reclaiming leaked sessions on a specific machine, or pruning stale tokens from a running `state.db`. Those are operator runbook steps. The runbook itself is repo content and can be a task; the actions it describes cannot.

## Proposed implementation task breakdown

Breakdown size: 18 entries (15 in-scope, 3 deferred) — this lands in the 15+ "large build-out" band rather than the 8-14 band because the change reaches across most of the stack: a new crate, a schema migration, four distinct engine subsystems (spawn, adoption, reconcilers, teardown), the macOS app's pane layer, the engine→app protocol, the CLI, the coordinator's ownership boundary, and a staged per-pool acceptance sweep — none of which collapses into another without producing an entry too large for one reviewable PR.

**Parallelism by depth.** Depth 0 (start immediately, fully parallel): entries 1, 3, 12. Depth 1: entries 2 and 4 in parallel. Depth 2: entries 5 and 7 in parallel — different subsystems (Swift pane layer vs. engine adoption), no shared files. Depth 3: entries 6, 8, 9, 10, 11 — 8, 9 and 10 are genuinely parallel (three distinct engine modules with no shared files); 6 must follow 5 and 11 must follow 7 for the file-overlap reasons noted on those entries. Depth 4: entries 13 and 14 in parallel. Depth 5: entry 15. The two deferred entries that gate on the migration (16, 18) sit below it; deferred entry 17 has no gate.

---

**1. `boss-tmux` control crate**

Scope: a new crate at `tools/boss/tmux` wrapping the tmux CLI as a typed Rust API: absolute-path resolution, version probe, `new-session -d -e`, `list-sessions -F`, `show-environment`, `set-option`/`show-options`, `send-keys` (two-phase, chunked), `capture-pane`, `kill-session`, and `display-message` field reads. Private server label throughout. Behind a command-runner seam so the whole surface is unit-tested in-process without a live tmux server, mirroring how `ssh_spawn` tests against a stubbed transport. No dependency on `boss-engine` — the edge is one-way, engine → boss-tmux.

Effort hint: `medium`

Dependencies: none

Scope: in-scope

---

**2. tmux preflight and hard-dependency gate**

Scope: engine startup probes for tmux, enforces the ≥ 3.2 version floor, records the resolved absolute path, and on absence or a too-old version raises a startup attention item and fails every local dispatch with an explicit reason — with no fallback path. Surfaces the state in `bossctl doctor` and on the engine-health banner. Installer check for tmux.

Effort hint: `small`

Dependencies: `boss-tmux` control crate

Scope: in-scope

---

**3. Schema: tmux session columns on `work_runs`**

Scope: add `tmux_server_label`, `tmux_session_name`, `tmux_spawn_token` (unique index), `tmux_spawn_state`, `tmux_pane_pid` with the migration, plus the DB accessors and the `list_adoptable_tmux_runs` query the adoption pass will consume. No behaviour change; nothing writes the columns yet.

Effort hint: `small`

Dependencies: none

Scope: in-scope

---

**4. tmux-hosted spawn path in the engine**

Scope: the write-then-create ordering — mint token, commit `intended`, `new-session -d -e`, label, read `#{pane_pid}`, commit `created`. Env carriage moves from the `SpawnWorkerPane` payload to `-e` flags. Gated by the per-pool `workers.tmux_hosting` setting, default off, so nothing changes in production on merge. Legacy path untouched.

Effort hint: `large`

Dependencies: `boss-tmux` control crate; Schema: tmux session columns on `work_runs`

Scope: in-scope

---

**5. App: attach-mode worker panes**

Scope: `AttachWorkerPane` / `DetachWorkerPane` replace `SpawnWorkerPane` / `ReleaseWorkerPane` for tmux-hosted runs. The Ghostty surface's launch line becomes `exec tmux -S <socket> attach-session -t <session>`. `WorkerProcessKiller` is removed from the worker release path — detach tears down the surface and kills nothing. Both RPC shapes coexist while the legacy path is live.

Effort hint: `medium`

Dependencies: tmux-hosted spawn path in the engine

Scope: in-scope

---

**6. Engine-side pane input: `send-keys` replaces `SendToPane`**

Scope: route `inject_pane_text_verified` and the interrupt path through `boss-tmux` for tmux-hosted runs, preserving `PaneInputPosture` and the verified-delivery semantics exactly — transport only. Retire the `SendToPane` / `InterruptWorkerPane` RPCs for those runs.

Effort hint: `medium`

Dependencies: App: attach-mode worker panes — **file-overlap note:** this entry and entry 5 both edit the engine→app request/response enums (`engine/core/src/protocol.rs` and `app-macos/Sources/EngineProtocolTypes.swift`). Land 5 first; this entry must forward-port 5's protocol changes preservingly rather than replacing them.

Scope: in-scope

---

**7. Boot-time adoption pass**

Scope: enumerate the tmux server, read the authoritative token per session, exact-match against `work_runs`, and for each non-terminal match rebuild the slot claim, `WorkerRegistry` entries, `LiveWorkerState` and the live-status summarizer. Terminal matches hand off to the existing `worker_readoption` policy unchanged. Runs before `run_reconcile`, which then covers only what adoption did not claim. Includes the `intended`-with-live-session repair case.

Effort hint: `large`

Dependencies: tmux-hosted spawn path in the engine

Scope: in-scope

---

**8. Adoption guards: version skew and engine exclusivity**

Scope: stamp `BOSS_SESSION_SCHEMA` at creation; on adoption, refuse-then-reap for unknown or newer schemas, with the dispatch event and attention item that makes it loud. Add the server-scoped `@boss_engine_owner` option and the refuse-to-adopt-on-conflict check.

Effort hint: `medium`

Dependencies: Boot-time adoption pass

Scope: in-scope

---

**9. Liveness redefinition in `stale_worker_sweep`**

Scope: replace the single hook-cadence test with the three-way classification — alive-and-working / alive-and-genuinely-stuck / dead — consulting `window_activity`, `pane_current_command` and `pane_dead` alongside hook recency. Stuck escalates to an attention item rather than reaping. Removes the `Coarse`/`Minimal` fidelity exemption, which the terminal-level signals now cover.

Effort hint: `medium`

Dependencies: Boot-time adoption pass

Scope: in-scope

---

**10. Leaked-session sweep in `husk_pane_sweep`**

Scope: switch the pane-inventory authority from the app's `ListHostedPanes` to the tmux server. Classify each enumerated session as adoptable / leaked / terminal-contradiction and route accordingly, retaining the two-pass confirmation and the per-pass retirement breaker.

Effort hint: `medium`

Dependencies: Boot-time adoption pass

Scope: in-scope

---

**11. Teardown: token-verified `kill-session` as the reap verb**

Scope: every terminal path that calls `release_worker_pane` today resolves the session, verifies the token matches, runs the existing SIGTERM→SIGKILL ladder against the pane pid's process group, then kills the session and clears the token columns. A `kill-session` by name alone is rejected at the API boundary.

Effort hint: `medium`

Dependencies: Boot-time adoption pass — **ordering note:** this entry and entry 7 both edit `engine/core/src/app.rs` and `app/pane_ops.rs`. Land 7 first; forward-port preservingly.

Scope: in-scope

---

**12. App: the engine restarts a dead engine**

Scope: add supervision to `EngineProcessController` so an engine that exits is relaunched with backoff, instead of the app retrying a socket with no listener indefinitely. Independent of the tmux work and startable immediately; without it, tmux-hosting delivers worker survival with nothing to reattach to. Includes the policy knobs (backoff schedule, give-up threshold) and a visible banner state.

Effort hint: `small`

Dependencies: none

Scope: in-scope

---

**13. Coordinator: engine-launched tmux session, app attaches**

Scope: move the coordinator's launch from `BossPaneModel` to the engine, backed by the `coordinator.tmux_*` singleton in the `metadata` table with the same write ordering and crash-window handling. `BossPaneModel` becomes an attacher. Restart-on-child-exit and the model-change recreate move engine-side, with the model change surfaced as an explicitly destructive action.

Effort hint: `large`

Dependencies: App: attach-mode worker panes; Boot-time adoption pass

Scope: in-scope

---

**14. CLI and observability over tmux state**

Scope: `bossctl agents list` reports session name, adoption state, `pane_dead` and last output time; a `bossctl agents attach <exec>` prints the exact attach command; new dispatch events for adopt / refuse-skew / leak-detected / token-mismatch so every branch of the adoption taxonomy is greppable in `bossctl dispatch tail`.

Effort hint: `medium`

Dependencies: Boot-time adoption pass; Liveness redefinition in `stale_worker_sweep`; Leaked-session sweep in `husk_pane_sweep`

Scope: in-scope

---

**15. Staged per-pool enablement and acceptance sweep**

Scope: the migration itself — enable `workers.tmux_hosting` for review, then automation, then interactive, then coordinator; verify per pool that an engine restart mid-turn adopts cleanly, an app restart reattaches, teardown leaves no session, and the mode badge renders. Includes the rollback drain path and the visible-mode surfacing (dispatch-event stamp, Workers-grid badge, `bossctl doctor`). Listed after every implementation entry it validates and deliberately not folded into them.

Effort hint: `medium`

Dependencies: App: attach-mode worker panes; Engine-side pane input: `send-keys` replaces `SendToPane`; Boot-time adoption pass; Liveness redefinition in `stale_worker_sweep`; Leaked-session sweep in `husk_pane_sweep`; Teardown: token-verified `kill-session` as the reap verb; Coordinator: engine-launched tmux session, app attaches; CLI and observability over tmux state

Scope: in-scope

---

**16. Remote (SSH) workers adopt the same tmux model**

Scope: replace the remote `nohup` launch with a remote tmux session carrying the same token, so remote and local workers share one adoption path, one liveness classification and one teardown verb — and remote workers become attachable, which they are not today.

Effort hint: `large`

Dependencies: Staged per-pool enablement and acceptance sweep

Scope: deferred (future / not a v1 blocker) — remote workers already survive engine restarts via `remote_reattach`, so this is convergence and attachability, not a durability fix

---

**17. Headless (non-pane) worker mode**

Scope: an execution mode that runs the agent non-interactively with no pty at all, removing the pane from the durability question entirely for run kinds that never need operator attachment.

Effort hint: `large`

Dependencies: none

Scope: deferred (future / not a v1 blocker) — changes the operator model, the permission model and the driver abstraction's interactive-TUI support; recorded here so the option stays visible rather than being foreclosed by this design

---

**18. Cross-reboot work resumption**

Scope: whatever would let a machine reboot leave resumable state behind — beyond the recovery-patch capture that already exists.

Effort hint: `large`

Dependencies: Staged per-pool enablement and acceptance sweep

Scope: deferred (future / not a v1 blocker) — explicitly a non-goal of this design; listed so it is visibly considered and rejected for v1 rather than silently absent
