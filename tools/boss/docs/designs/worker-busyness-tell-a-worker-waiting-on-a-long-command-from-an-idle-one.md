# Worker busyness: Boss must issue busyness, not infer it

- **Date:** 2026-08-06
- **Status:** proposed — design only, no code changed in this run
- **Parent project:** Worker busyness: tell a worker waiting on a long command from an idle one (`mono` product Boss)
- **Deliverable:** design document plus an implementation task breakdown
- **Primary sources read:** `stale_worker_sweep.rs`, `husk_pane_sweep.rs`, `dead_pid_sweep.rs`, `background_children.rs`, `build_wait.rs`, `build_wait_tracker.rs`, `completion/nudge.rs`, `live_worker_state.rs`, `engine/driver/src/lib.rs`, `runner/prompt.rs`, `codex_unobserved_command.rs`, `worker_registry.rs`, and — for the enforcement section — `worker_setup.rs`, `engine/driver/src/claude.rs`, `engine/driver/src/codex.rs`, `app/worker_events.rs`, `worker_sandbox_audit.rs`
- **Related docs:** [`worker-liveness-contract.md`](../worker-liveness-contract.md), [`codex-exit-code-surfacing.md`](../investigations/codex-exit-code-surfacing.md), [`codex-as-a-first-class-agent-driver.md`](./codex-as-a-first-class-agent-driver.md), [`codex-pretooluse-guard-coverage-2026-07-29.md`](../investigations/codex-pretooluse-guard-coverage-2026-07-29.md)

Boss has five subsystems that each answer "is this worker busy?" from a different proxy, and every one of those proxies is an _observation of the worker's environment_ rather than a fact Boss owns. This document argues that observation is the wrong shape for the question, and proposes that Boss issue and observe the obligation instead.

## Verdict

**The central bet, stated so a reviewer can disagree with it: busyness must be a Boss-issued, Boss-observed obligation with an owner process and a named outcome — not anything inferred from the worker's tool stream, its process tree, or its prose.** Every current signal is an inference, and each one breaks precisely where the inference diverges from the thing it stands in for.

Concretely: a `boss run -- <argv>` wrapper opens an obligation the engine records, attributes to the run by peer-pid ancestry, and keeps open until the wrapper reports a verdict or the wrapper's own process dies. One verdict function, `busyness(execution)`, then serves reaping, husk/dead-pid corroboration, and the auto-nudge. The _mechanism_ is strictly additive — a worker that never calls `boss run` is judged exactly as it is today, so nothing regresses on non-adoption — but additive is not the same as optional: [Enforcement](#enforcement-what-stops-a-worker-sidestepping-the-wrapper) sets out the four layers that hold a worker to the wrapper and which of them v1 ships.

**The foreground mandate stays until that lands.** Relaxing it first trades silent gate-blindness for silent reaping, which is worse. Once `boss run` exists the mandate is not deleted but _re-expressed at the level of the property it was always trying to protect_.

## Goals

- One notion of worker busyness, defined once, consumed by every subsystem that currently reimplements it.
- A busyness signal whose correctness does not depend on which agent driver is running. It must hold for Claude's blocking `Bash` tool and for Codex's yield-and-poll `exec_command` cell without a per-driver special case.
- A worker-facing contract that can express three distinct states — _still running_, _finished with verdict X_, _finished-or-not, verdict unobtainable_ — and that renders the third one loudly rather than as success.
- A verification story that demonstrates both directions against a real engine: a legitimately busy worker is not reaped, and a genuinely dead one still is.
- Make backgrounding safe, so the foreground mandate can be replaced by something enforceable on every driver.
- An enforcement story that does not rest on the worker's cooperation: the wrapper must be the cheapest path to take, evading it must be visible to the engine, and a claim resting on a command Boss never observed must not be cashable. See [Enforcement](#enforcement-what-stops-a-worker-sidestepping-the-wrapper).

## Non-goals

- **Fixing Codex's exit-code surfacing.** The mechanism (a cell that reports `Script completed` while the shell command runs on, with no completion record anywhere) is filed separately and covered in [`codex-exit-code-surfacing.md`](../investigations/codex-exit-code-surfacing.md). This project covers Boss's inability to know a worker is busy, which is why the workaround exists at all. The two designs meet at `boss run`, and that overlap is stated in the breakdown rather than hidden.
- **Weakening reaping.** Genuinely dead workers must still be reclaimed. Nothing here adds a "never reap" path.
- **A general worker-health model.** Busyness is one bit with a subject. Whether the worker is making _useful_ progress is a different question, out of scope.
- **Changing the `ProgressFidelity` tiering.** The `Coarse`/`Minimal` cadence exemption is orthogonal and stays as-is.
- **Implementation.** No `.rs`, build, or app file is touched by this run.

## What is actually broken

### 1. `current_tool` is a tool-call span, and a tool-call span is not a command span

`stale_worker_sweep.rs:277` skips reaping when `state.current_tool.is_some()`. Its stated rationale is sound: a foreground `bazel build //...` on a cold cache runs for many minutes with no intervening hook, and reaping that would be the regression we must not cause.

The rationale is sound; the _implementation of it_ silently assumes a property that was never written down anywhere: **that the interval between `PreToolUse` and `PostToolUse` equals the wall-clock duration of the command the worker is waiting on.** That holds for Claude, whose `Bash` tool blocks on child exit. It does not hold for Codex. There, `tools.exec_command` returns after a model-chosen `yield_time_ms` (observed 10 s and 30 s) with the shell command still running inside a cell; the tool call closes and a tool-output record is written. The span measures a _polling window_, not a command.

The failure is not symmetric, which is what makes it hard to see. `stale_worker_sweep` also has a 30-minute cadence guard, and a Codex worker that keeps polling refreshes `last_event_at` on every poll — so it is usually spared for the wrong reason. The subsystems that get hurt are the two that use `current_tool` as _evidence to spare a process from a kill_:

- `husk_pane_sweep::live_process_evidence` (`husk_pane_sweep.rs:375`) — an unbalanced `PreToolUse` is the half of the corroboration that "survives arbitrarily long quiet periods, which is the whole point".
- `dead_pid_sweep::corroborating_liveness` (`dead_pid_sweep.rs:516`) — same construction, same comment, same purpose.

On Codex both fall back to hook recency alone, because the tool span expired minutes ago. Hook recency is exactly the signal both modules document as insufficient. The regression bar for this project — a sweep that once SIGTERMed five actively-working panes in a single pass — lives on this code path.

### 2. The descendant probe does not fire zero times; it suppresses every nudge

`background_children::count_live_descendants` walks from `shell_pid` and counts everything below it with no filter. The agent runtime is always a child of `shell_pid`, so the count is never zero: measured, `shell → codex → codex-code-mode-host` = 2 and `shell → claude → caffeinate` = 2.

`completion/nudge.rs:143` reads `if descendant_count > 0 { … suppress … }`, bounded by `background_children_tracker`'s 45-minute horizon. The probe is wired in production at `app.rs:1165`. Combining those: **since this probe landed, every auto-nudge has been suppressed for the first 45 minutes after each execution's first Stop.** "Zero probes queued in seven days" is not an inert check — it is a universally-firing one whose suppression happens to cover the whole lifetime of most runs.

That matters for direction. A stalled worker sits `Idle` at a Stop boundary, and `stale_worker_sweep` only considers slots whose `activity == Working` — so the nudge path is the _only_ thing watching an idle worker, and it has been switched off for 45 minutes at a time. The unit test at `completion/tests/t02.rs:2604` pins this with `FixedDescendantProbe(2)`: a test that turned the drift into a defended invariant.

The module's own doc says the intent was to detect background subagents spawned via the harness `Agent` tool. A process-tree count cannot express that intent, because the intent is about _what a process is for_, and a process tree only knows _where a process is_.

### 3. The foreground mandate states the invariant at the level of the container, not the property

`runner/prompt.rs:710` mandates running every build-class command in the foreground and forbids background-and-poll. The property it is protecting is not foregrounding. It is:

> Every build-class command's exit code must be observed by something that outlives the tool call.

"Foreground" is one implementation of that property, available on exactly one driver. Writing the mandate at the container level made it unenforceable on Codex — where there is no foreground, and where `exec_command` is _natively_ a background-and-poll shape — and made its own escape hatch self-defeating: `timeout 1800 bazel test //...` guarantees the command outlives the cell window by 20–60×.

An implementation can satisfy the letter of the mandate (issue the command in a tool call and never background it) while breaking everything downstream of it (the tool call closes, the command runs on, nothing observes the exit code). That is the signature of a constraint written at the wrong level.

### 4. There are five consumers, not three

The brief names three. Reading the code, `current_tool` and its cousins are load-bearing in five places, and any design that fixes three of them leaves two reimplementations behind:

| Consumer                   | File                         | Signal used                           | What it decides   |
| -------------------------- | ---------------------------- | ------------------------------------- | ----------------- |
| Stale-worker reaping       | `stale_worker_sweep.rs:277`  | `current_tool.is_some()`              | reap / spare      |
| Husk-pane retirement       | `husk_pane_sweep.rs:375`     | `current_tool` + hook recency         | kill pane / spare |
| Dead-pid corroboration     | `dead_pid_sweep.rs:516`      | `current_tool` + hook recency         | reap / spare      |
| Auto-nudge, build case     | `build_wait.rs:50`           | 12 English substrings in worker prose | nudge / suppress  |
| Auto-nudge, delegated case | `background_children.rs:102` | descendant process count              | nudge / suppress  |

`build_wait` deserves naming plainly, because it changes what counts as a novel proposal here. **Boss already has a worker-declared busy state.** It is implemented as case-insensitive substring matching against a hand-copied list of phrases from one 2026-07-14 incident transcript, with a 45-minute bound. "An explicit worker-declared busy state with a bound" is therefore not a new idea to be evaluated against the status quo — it _is_ the status quo, minus structure, minus identity, minus a subject, and minus any way to know whether the thing being waited on finished.

### 5. Nobody ever decided that a tool span means a command duration

There is no recorded decision anywhere in the tree that a tool-call span may stand in for a command's duration. The nearest thing is `ProgressFidelity`'s doc in `engine/driver/src/lib.rs:906-922`, which is careful and explicit that the tier "is about event **cadence** only … not about what those boundaries can tell Boss once a command has run", and separates out `Capability::CommandOutcomeObservation` for the outcome question.

The stronger and more common reading of that silence is not that a reason existed and was lost. It is that **the decision was never made at all.** `current_tool` was introduced to drive a UI field and to bound a sweep against Claude's blocking `Bash`; a second driver arrived later and inherited the assumption without anyone restating it.

There is a sharper lesson in that `ProgressFidelity` doc than "it was missed". The doc records an equivalence — Claude hooks and Codex `item.started`/`item.completed` are the same tier — and correctly qualifies the dimension it does _not_ hold on (outcome). Having named one dimension, the qualification reads as exhaustive, and the dimension it is silent on (**duration**) looks already-considered. Both drivers report per-tool boundaries at the same _cadence_; only one reports them at the same _extent_. That is the dimension this project is about, and it was never named.

## Alternatives considered

### A. Fix the process-tree probe by excluding the agent runtime

Walk the descendants but subtract the driver's own subtree, so the count reflects only work processes.

**Rejected.** The runtime's helper processes are exactly where a Codex command lives: the shell command runs inside a cell hosted by `codex-code-mode-host`, which is inside the runtime subtree. Subtracting the runtime subtree subtracts the command. Making the subtraction finer means classifying an arbitrary pid's _role_ — is this process the runtime, a helper, or work? — and the only handles available for that are the executable name and a maintained allowlist. That is what rots.

**Precedent check, because this rejection has to survive contact with something Boss already relies on.** Boss _does_ attribute processes to runs by walking the process tree: `worker_registry::lookup_with_ancestor_walk` climbs up to `ANCESTOR_WALK_DEPTH` ancestors to resolve a socket peer's pid to a run, and it has not rotted. The rejection does not disqualify it, because the two do different work. Ancestry attribution answers _"which run does this pid belong to?"_ about a pid Boss was **handed** by a process that deliberately connected to it — there is ground truth at both ends. The rejected approach must answer _"what is this pid for?"_ about a pid **nobody handed it**, and there is no ground truth for that at all. The chosen approach below uses ancestry attribution and never uses role classification.

### B. Widen the tool span per driver — treat a Codex cell as open until the cell is destroyed

Teach the Codex normaliser to hold `current_tool` for the cell's life rather than the tool call's.

**Rejected on evidence, not on taste.** Probe 6 of the exit-code investigation is the disproof: the cell reports `Script completed` at 17.7 s while only 8 of 12 ticks of a ~48 s command had been emitted. The _cell's_ lifetime is not the command's lifetime either. This buys a longer wrong number. It also re-establishes per-driver busyness semantics inside the very abstraction (`engine/driver`) built to prevent them, and it does nothing for `build_wait` or `background_children`, so it fixes at most three of five consumers.

### C. A worker heartbeat emitted while waiting

The worker emits a periodic "still waiting" signal; absence of the heartbeat means the worker is gone.

**Not rejected as wrong — rejected as answering a weaker question.** A heartbeat proves the _worker_ is alive. It does not prove the thing the worker is waiting on is alive. That gap is exactly the failure mode this project was filed over: a worker confidently reporting a phantom 30-minute test that had in fact failed to compile in 90 seconds would have heartbeated cheerfully throughout. A heartbeat with a subject is what is wanted, and that is what an obligation's owner-pid liveness is.

### D. Structured worker declaration with no owner process (`boss busy start` / `boss busy end`)

Keep the shape of `build_wait` but make it structured: the worker declares a busy interval over the existing `boss propose`-style channel.

**Rejected relative to the chosen approach, for one reason: an unowned declaration cannot be falsified.** If the worker dies mid-declaration the declaration persists. If the command dies the declaration persists. The only way to end it is a timer, which the brief rules out as a primary signal, or the worker's cooperation, which is the thing in question. The wrapper's owner pid is what makes a declaration checkable — it is the difference between a claim and an observation. That is the whole reason the chosen design is a wrapper and not a flag.

### E. Status quo — the current arrangement stands

The brief licenses this outcome, so it is evaluated rather than skipped.

**Rejected.** Three of the five consumers are demonstrably wrong today in ways that are checkable from the code alone: the two spare-from-kill corroborators degrade to a signal their own docs call insufficient on any non-Claude driver; the descendant probe suppresses every nudge for 45 minutes rather than detecting anything; and the prompt mandate is unenforceable on the driver it most needs to constrain. A "smaller correct answer" is available and is what is proposed — but "no answer" is not one of the small ones.

## Chosen approach

### The invariant

> **Busyness invariant.** For every interval in which Boss withholds a reap, a pane retirement, or a nudge on the grounds that a worker is _busy_, there exists a named obligation, attributable to that worker's run, whose completion Boss itself will observe. Absence of such an obligation is never evidence of busyness.

Stated at the level of the load-bearing property — _an observed obligation_ — rather than the container that usually carries it (a tool call, a process, a phrase).

### The mechanism: a Boss-owned command obligation

A worker runs build-class commands through a wrapper:

```sh
boss run --label "prepush-build" -- bazel test //tools/boss/engine/...
```

`boss run` does four things, and only the first is visible to the worker in the ordinary case:

1. **Runs the command** and, if it completes before the wrapper is interrupted, exits with the child's exit code and streams its output. On Claude this is byte-for-byte the experience of running the command directly.
2. **Opens an obligation** with the engine before spawning the child: `{run_id, label, argv, owner_pid, opened_at, deadline: Option}`. The engine attributes the caller to a run from the socket peer pid via `worker_registry::lookup_with_ancestor_walk`, exactly as `boss propose` already does — `BOSS_RUN_ID` remains a cross-check, not a credential.
3. **Reports the verdict** — exit code, or the signal, or the fact that the child was killed — when the child exits, whether or not anything is still reading the wrapper's stdout.
4. **Prints a handle** on its first line so a worker whose tool-call window closed early can retrieve the verdict later.

The wrapper's own pid is the owner. It is registered at open, and it is a pid Boss was handed by a process that deliberately connected to it — never one Boss found by scanning.

### The verdict function

One function in `engine/core`, consumed everywhere:

```
busyness(execution) -> Busy { obligation } | Idle | Unresolved { obligation }
```

- **`Busy`** — at least one open obligation whose owner pid probes live (`durable_liveness` / `probe_pid`, with `EPERM` read as alive per the liveness contract). Reap, retire, and nudge are all withheld.
- **`Idle`** — no open obligation. **Falls through to today's logic entirely unchanged**: cadence, `current_tool`, `last_event_at`, the fidelity exemption. A worker that never calls `boss run` loses nothing.
- **`Unresolved`** — an open obligation whose owner pid is confirmed dead with no verdict reported. The obligation is closed as `abandoned`, an attention item is filed naming the label and argv, and busyness degrades to `Idle`. **The loudness is the attention item; the reaping decision then proceeds normally.** This is what satisfies "fail loudly rather than guess" without adding a never-reap path.

`Unresolved` reuses the shape `codex_unobserved_command` already established for abandoned Codex commands — an audit trail plus an attention item plus a refusal to let a downstream claim rest on the unobserved step. That precedent is deliberate: it is the same fact, discovered one layer lower and for every driver rather than one.

### What each consumer becomes

| Consumer                                 | Change                                                                                                      |
| ---------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `stale_worker_sweep`                     | `if state.current_tool.is_some()` → `if busyness(exec).is_busy() \|\| state.current_tool.is_some()`         |
| `husk_pane_sweep::live_process_evidence` | obligation becomes the primary corroborator; `current_tool` and hook recency stay as fallbacks              |
| `dead_pid_sweep::corroborating_liveness` | same                                                                                                        |
| `build_wait` + `build_wait_tracker`      | **deleted.** Superseded by a structured declaration with an owner                                           |
| `background_children` + its tracker      | **deleted** for the command case; the delegated-subagent case it was aimed at is a separate, deferred entry |

`current_tool` is deliberately **retained** as a disjunct rather than replaced. Removing it before `boss run` adoption is measured would reintroduce, for every Claude worker not yet using the wrapper, precisely the reaping risk the guard exists to prevent. Retiring it is a deferred entry gated on measured adoption, not a v1 step.

### Persistence

Obligations are durable, in a `work_command_observations` table, not in-memory like `nudge_breaker` and `build_wait_tracker`. That is a deliberate departure from those neighbours, and the reason is in [`worker-liveness-contract.md`](../worker-liveness-contract.md): _"Never make a liveness decision from derived bookkeeping alone when the decision is irreversible (killing a worker)."_ An in-memory registry is derived bookkeeping — an engine restart would turn every in-flight command invisible and hand the next sweep pass a fleet of apparently-idle busy workers. That is the five-SIGTERMed-panes shape, reintroduced by the fix. Durability also gives the worker a verdict that survives an engine restart, which is the point of the wrapper.

### Deadlines are a bound on trust, never the signal

An obligation may carry an optional `--deadline`. It does **not** flip `Busy` to `Idle`. It files an attention item ("this command has been open longer than declared") and nothing else. Owner-pid liveness stays the sole primary signal; the deadline exists so a wedged-but-alive command still surfaces to a human, the same job `build_wait_tracker`'s 45-minute horizon does today, without inheriting its role as the decider.

## Enforcement: what stops a worker sidestepping the wrapper

A mechanism that depends on the worker choosing to use it is a mechanism that reports on well-behaved workers only, and the interesting cases are rarely well-behaved. This section is the answer to that objection, and it changes what v1 ships: three enforcement layers move into scope, and the fourth becomes an explicit, framed decision rather than a bullet in the risk list.

### The two enforcement problems are different, and only one of them is adversarial

Treating "will workers use `boss run`?" as one question is what made the first draft's answer — _it is voluntary in v1_ — read as no answer at all. There are two questions with opposite incentive structures.

**Busyness is self-enforcing, because sidestepping harms the sidestepper.** A worker that runs `bazel test` bare gets today's inference-based judgement: its busyness is guessed from a tool span, and on any driver where that span is a polling window rather than a command, it is guessed wrong in the direction of _reapable_. The wrapper is the thing that keeps a legitimately busy worker alive across a sweep pass. Skipping it does not buy the worker anything — it forfeits protection. On this axis, enforcement is mostly a usability problem: make the wrapper the shortest path, make its absence visible, and adoption follows without coercion.

**Validation claims are adversarial, because sidestepping pays.** A worker that skips the wrapper and then reports "build and tests pass" has escaped the only thing that could have contradicted it. Here the incentive points the other way, and nothing about the busyness design constrains it. This is the half the reviewer's question is really about, and it is answered below by layer 3 — the only layer that removes the payoff rather than obstructing the act.

Holding those apart matters because they call for different machinery and land in different places in the plan. It also means "advisory" is a defensible answer for the first and an indefensible one for the second.

### The enforcement ladder

Boss already runs four kinds of enforcement against workers. The wrapper does not need new machinery; it needs to be wired into the machinery that exists.

| Layer | Mechanism for `boss run`                                                                                                                                                      | Precedent already shipping                                                                                                           | Prevents or detects?                 | Driver coverage                     |
| ----- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------ | ----------------------------------- |
| 0     | Prompt contract: build-class commands run through the wrapper                                                                                                                 | today's foreground mandate (`runner/prompt.rs:710`)                                                                                  | **neither** — states intent          | all drivers                         |
| 1     | Permission deny rules on bare build-class argv (`Bash(bazel:*)`, `Bash(cargo:*)`), with no rule matching `boss run` — a `boss run --` prefix is the only spelling that passes | `deny_rules()` in `worker_setup.rs:773` (`Bash(bossctl:*)`, `Bash(boss engine stop:*)`)                                              | prevents, coarsely                   | drivers with `PermissionPolicy`     |
| 2     | A `PreToolUse` interception guard that tokenises argv and denies a build-class command not already wrapped                                                                    | `PR_REDIRECT_GUARD_COMMAND`, `REVISION_PR_GUARD_COMMAND`, `BOSS_LAUNCH_GUARD_COMMAND`, `boss-checkleft-push-guard.py`                | prevents, precisely                  | Claude full; Codex deny-only, holed |
| 3     | Refuse a downstream claim that rests on a command with no `exited 0` obligation                                                                                               | `codex_unobserved_command.rs` — **already** refuses a worker's `NO_CHANGES_NEEDED` for the rest of a run after an unobserved command | prevents the **payoff**, not the act | all drivers — engine-side           |
| 4     | Detect a build-class command that ran outside an obligation; file an attention item                                                                                           | `worker_sandbox_audit.rs`; the unobserved-command audit trail                                                                        | detects only                         | all drivers                         |

Layer 1 works because of a detail of the permission engine that is easy to miss: rules match the command's leading token, so `boss run -- bazel test //…` is a `boss` invocation, not a `bazel` one, and a deny on `Bash(bazel:*)` fences the bare form while leaving the wrapped form untouched. That is a genuine allowlist-by-construction and it costs two strings. It is also brittle in the ways every string match is brittle — `env bazel …`, `sh -c 'bazel …'`, a `$TOOL` indirection — which is exactly the gap layer 2 exists to close, and the existing guards already do that work: `BOSS_LAUNCH_GUARD_COMMAND` splits on shell delimiters, strips wrapper commands (`nohup`, `env`, `sudo`, `exec`, `xargs`, …) and resolves single-level shell variables before matching. A `boss run` guard is that guard with a different target list.

### The structural point: only layer 3 is out of the worker's reach

Layers 0 through 2 all execute inside the worker's own harness. They police _the act_, and their strength is bounded by what the harness lets Boss intercept. Layer 3 executes in the engine, on state the worker cannot write. It polices _the payoff_ — and it is the only layer that is uniformly available across drivers, because it needs no hook surface at all.

That is the load-bearing claim of this section: **a worker cannot be reliably prevented from running a command outside the wrapper, but it can be reliably prevented from banking a result it obtained that way.** The second is sufficient, and it is achievable on every driver.

It is also not speculative. `codex_unobserved_command` already implements the narrow case: when Boss observes a command start with no completion, it files an attention item _and_ refuses the worker's "validation passed, nothing to do" claim for the remainder of the run, on the stated ground that a downstream claim resting on an unobserved step is unconfirmed rather than verified. Generalising that from "Codex commands Boss watched start and lose" to "any build-class command with no obligation" is a widening of a live mechanism, not new machinery — which is a substantially cheaper proposition than the deferred entry's `large` hint implied when it was written.

### Where the layers break, measured rather than assumed

Enforcement claims are worth exactly as much as their weakest driver, and Boss has already paid for that lesson in evidence. [`codex-pretooluse-guard-coverage-2026-07-29.md`](../investigations/codex-pretooluse-guard-coverage-2026-07-29.md) set out to confirm the Codex guards fire and instead found two live bypasses that had been sitting behind a prompt sentence asserting the opposite:

- **`tools.write_stdin` fires no hook at all.** A cell starts a long-lived shell with `exec_command` (which the guards approve, because `sh -s` is innocuous) and then feeds it arbitrary command lines that no guard ever sees. This was demonstrated end-to-end against the push guard, not theorised.
- **App/MCP tools arrive under an `mcp__…` tool name** that no `matcher = "Bash"` guard matches.

Codex additionally declares `ToolUseInterception` as **deny-only** (`codex.rs:1275`): it honours `permissionDecision: deny` but rejects `allow` / `ask` / `updatedInput`, so a guard there can block but cannot rewrite — no "silently upgrade the bare command into a wrapped one" path exists on that driver. And a driver that declares the capability not at all lands on `AbsenceDisposition::Degrade`, where `dispatch_post_hoc_interception_on_post_tool_use` can only flag an artefact after the fact, logging in terms that this "is not equivalent to pre-hoc interception".

Two consequences follow, and the design takes both:

1. **Layer 2 is best-effort by construction, and the design must not rest on it.** It is worth shipping — it closes the accidental case, which is the common case — but a plan whose safety argument terminates at a `PreToolUse` guard is a plan that is already known to be false on one of the two drivers in production.
2. **Guards fail closed.** `boss-path-guard.py`'s rule is the one to copy: for the tools it reasons about, a payload it cannot parse is _blocked_, because a gate that waves through what it cannot read is not a gate. The `boss run` guard inherits that posture, so an unrecognised payload shape from a future driver surfaces as a block plus an operator signal rather than a silent hole.

### What v1 commits to

- **Layer 0** — entry 10 already rewrites the prompt; it now states the wrapper contract rather than the foreground container.
- **Layers 1 and 2** — new entry 11, gated on the wrapper existing.
- **Layer 4** — new entry 12, and this is what makes non-adoption _measurable_ rather than merely tolerated. Without it, "adoption is voluntary" is a claim nobody can check.
- **Layer 3** — remains deferred (entry 16), because deny-by-default claim refusal is a policy decision about how Boss treats its workers and a human should make it. What changes is the framing: it is no longer an optional extra at the bottom of a list, it is the answer to the enforcement question, and it is cheaper than first estimated because the narrow version ships today. The questions manifest puts the fork to the operator directly.

### What is deliberately not claimed

No layer here makes the wrapper unavoidable for a worker determined to evade it. Backgrounding _inside_ `boss run`, detaching with `nohup`, or driving a shell through `write_stdin` all defeat the mechanism, and no liveness signal can fix a worker that sets out to lie about its own state. The standard this design holds itself to is the one every other Boss guard is held to, and it is met on all three counts:

- the wrapped form is the cheapest path to take,
- evasion is visible to the engine rather than silent, and
- a claim resting on a command Boss never observed cannot be cashed.

The last is the one that matters, and it does not depend on the worker's cooperation.

## Answers to the five questions the brief asked

**1. What signal would actually be correct?** A live owner process, handed to Boss by the wrapper that spawned the command, holding a named obligation whose completion Boss records. Costs: one new CLI, one table, one RPC, and the enforcement wiring in entries 11 and 12. Fails on: a worker that does not use the wrapper (degrades to today's behaviour, never worse — and [Enforcement](#enforcement-what-stops-a-worker-sidestepping-the-wrapper) is what keeps that case rare and visible rather than merely tolerated), and a wrapper killed by the same event that killed the worker (surfaces as `Unresolved`, loudly).

**2. Can one signal serve all consumers?** Yes for four of the five, and that is the finding: reaping, husk retirement, dead-pid corroboration, and the build-wait half of the nudge are one question and get one answer. The fifth — delegated subagent work — is genuinely a different question. It is not "is a command running" but "has the runtime delegated a turn"; the correct signal there is the runtime's own event stream, not a process count. It is deferred, and the wrong probe is deleted rather than left in place looking like coverage.

**3. Would backgrounding then be safe?** Yes, once the obligation exists — and this is what dissolves the Codex problem. The 2026-07-14 hang (a worker idling forever on a notification that never arrived) is closed structurally: the thing being polled is Boss-owned, and if its owner dies without reporting, the poll returns `abandoned` rather than never returning. Backgrounding is unsafe today only because the poll target is unowned.

**4. What contract does the worker need?** A tri-state, retrievable by handle, that never renders absence as success:

| `boss run status <handle>` | Meaning                                     | Worker must                                              |
| -------------------------- | ------------------------------------------- | -------------------------------------------------------- |
| `running` (+ elapsed)      | owner alive, child has not exited           | keep waiting or poll again                               |
| `exited <code>`            | the verdict, durable                        | act on it                                                |
| `abandoned`                | owner gone, no verdict — Boss does not know | treat as **failure to observe**, never as pass; escalate |

Handles are scoped to the calling run, which forecloses the specific misdiagnosis that motivated this project: a worker cannot latch onto an unrelated workspace's orphaned process as evidence, because `boss run status` can only see this run's obligations. The prompt must additionally say, in terms, that `Script completed` refers to the JavaScript cell and not the command — that trap is documented in Finding 3 of the exit-code investigation and is the reason the phantom-test misdiagnosis was _plausible_ to the worker that made it.

**5. How is it verified?** Two claims, verified separately, at different times, by different means.

_Shadow phase — a validation study, and named as one before it runs._ Compute `busyness()` alongside every existing signal, log disagreements, and change no decision. This study can only tell us whether the chosen approach's verdict differs from today's and in which direction. **It cannot tell us whether a different approach would have been better** — it is not structured to compare, and no result it returns should be read as a comparison. If it shows the chosen approach is wrong, that is an escalation, not a hedge to be filed as a result.

_Acceptance sweep — end to end, against the real path._ Both directions must be demonstrated on a real isolated engine (`--socket-path /tmp/boss-test-…`, per the worker rules) with a real Claude worker and a real Codex worker each running a real multi-minute `bazel` build: a busy worker survives a sweep pass, and a `SIGKILL`ed worker is still reclaimed. This deliberately does **not** use a hand-built reproduction of the sweep, because a reproduction is built from the same beliefs that produced the code and is structurally unable to find the integration bug — which is what every incident on this path has actually been.

**Gate placement.** The shadow gate sits _before_ each consumer's cutover, not after the rollout. A gate at the end of a phase has no authority over the phase it belongs to; it can only block what comes after. The breakdown sequences it accordingly.

## Risks / open questions

- **Enforcement is layered, and the strongest layer is deferred to a human decision.** [Enforcement](#enforcement-what-stops-a-worker-sidestepping-the-wrapper) sets out four layers; v1 ships three of them (prompt contract, deny rules plus interception guard, and detection of unwrapped build commands). The fourth — refusing a validation claim that cites no observed exit code — is the only one that removes the _payoff_ from sidestepping rather than obstructing the act, and it is a policy decision about how Boss treats its workers. It stays deferred as entry 16 and is put to the operator in the questions manifest. If the answer is "advisory", the residual risk is precisely that a worker can sidestep the wrapper and make an unverifiable claim — entry 12's detection makes that visible after the fact but does not stop it.
- **Layer 2 is measurably incomplete on Codex, and that is load-bearing, not incidental.** `write_stdin` fires no hook, MCP app tools evade `Bash` matchers, and Codex's `ToolUseInterception` is deny-only — all measured, not assumed. Any future argument that leans on the interception guard as _the_ enforcement answer is wrong on the driver that most needs it. The guard is worth shipping for the accidental case; the safety argument must terminate at layer 3.
- **Retiring `background_children` removes coverage the nudge nominally had.** The claim in Finding 2 is that the coverage was never real. That claim is checkable and the discovery entry checks it first — but if the measurement contradicts it, the deletion must not proceed.
- **The wrapper adds a process to every build.** One extra pid per command, negligible, but it does change the process tree shape that `husk_pane_sweep` and `dead_pid_sweep` walk. The acceptance sweep must confirm no interaction.
- **A worker can still defeat the wrapper by backgrounding _inside_ it**, or by detaching, or by driving a shell through `write_stdin`. Not using the wrapper at all is addressed by the enforcement ladder; deliberate evasion from within is not, and should not be — a worker determined to lie about its own state is out of scope for a liveness signal. What the ladder buys is that evasion must be deliberate and leaves an engine-side trace, rather than being the default path.
- **`Unresolved` degrading to `Idle` is a judgement call.** The alternative — treating an abandoned obligation as continued busyness — would create the never-reap path the brief forbids. But it means an abandoned command's worker becomes reapable at the moment the attention item is filed, and a human may want a grace window there.
- **Two designs meet at `boss run`.** The separately-filed Codex exit-code work wants the same wrapper for a different reason. If both land independently they will duplicate it. The breakdown assumes this project owns the wrapper and the other consumes it; that assumption should be confirmed before entry 6 starts.

## Proposed implementation task breakdown

Breakdown size: 16 entries (13 in-scope, 3 deferred) — this spans four subsystems (engine/core, the `boss` CLI, the worker prompt, and the worker-setup guard wiring) plus a shared protocol crate, a schema change, and a discovery step. That is at the ceiling of the 8–14 anchor band rather than inside it, and the two entries above the band are the enforcement pair (11 and 12) added to answer the enforcement question; the five busyness consumers still collapse into two cutover entries instead of five.

**Parallelism summary.** Depth 0: entries 1 and 2 in parallel. Depth 1: entries 3 and 4 in parallel. Depth 3: entry 6 may run in parallel with entry 7. Depth 4: entries 8 and 9 are functionally independent but **not** file-independent — see entry 9's note. Entries 11 and 12 are independent of each other and of the consumer cutovers (8, 9); entry 11 needs only the wrapper (6), entry 12 needs only the obligation store and the verdict function (5, 7), so both can run alongside the cutover work.

---

**1. Measure the three busyness proxies against live workers**

Instrument or observe a running engine to confirm three claims this design rests on: that `count_live_descendants` returns ≥ 2 for every live worker on both drivers; that the resulting nudge suppression covers the first 45 minutes of every execution; and the observed distribution of Codex tool-span duration against actual command duration. Produce a short investigation writeup under `tools/boss/docs/investigations/` with raw artifacts. No production code changes.

- Effort hint: `small`
- Dependencies: none
- Scope: in-scope

**2. Add command-obligation wire types to `boss-protocol`**

Define `CommandObligation`, its state enum (`Open` / `Reported` / `Abandoned`), the verdict type (`exit_code: Option<i32>`, `signal: Option<i32>`, `observed: bool`), and the request/response envelopes for open, report, and status. Apply the builder convention for any struct over five fields. Types only — no engine or CLI behaviour.

- Effort hint: `small`
- Dependencies: none
- Scope: in-scope

**3. New `boss-command-obligations` crate: registry and owner-pid state machine**

A standalone crate holding the in-memory obligation registry and its state machine: open, report a verdict, and resolve an obligation whose owner pid is confirmed dead into `Abandoned`. Owner-pid probing reuses the existing `probe_pid` / `durable_liveness` seam behind a trait so the state machine is unit-testable without spawning processes. Per the repo's crate-over-module convention, and with a one-way edge from `engine/core` into it.

- Effort hint: `medium`
- Dependencies: entry 2
- Scope: in-scope

**4. Durable `work_command_observations` table and mappers**

Schema migration plus the DB mapper in `work.rs` for persisted obligations, following the existing convention that production mappers use struct literals and set every field from a named column. Includes the queries the verdict function and `boss run status` need: open obligations for a run, and a verdict by handle. No consumer wiring.

- Effort hint: `medium`
- Dependencies: entry 2
- Scope: in-scope

**5. Engine RPC handlers with peer-pid caller attribution**

Wire open / report / status handlers into the engine socket, attributing the caller to a run from the socket peer pid via `worker_registry::lookup_with_ancestor_walk` with `BOSS_RUN_ID` as a cross-check, mirroring `boss propose`'s existing authentication shape exactly. Persist through entry 4's mappers and mirror into entry 3's registry.

- Effort hint: `medium`
- Dependencies: entries 3, 4
- Scope: in-scope

**6. `boss run` worker CLI**

The wrapper: spawn the child, open the obligation before spawning, stream stdout/stderr transparently, exit with the child's code, report the verdict on child exit regardless of who is reading, print the handle on the first line, and implement `boss run status <handle>` rendering the tri-state. Confirm with the coordinator first that this project owns the wrapper rather than the separately-filed Codex exit-code work (see Risks).

- Effort hint: `medium`
- Dependencies: entries 2, 5
- Scope: in-scope

**7. `busyness()` verdict function plus shadow-mode disagreement logging**

The single verdict function in `engine/core` returning `Busy` / `Idle` / `Unresolved`, plus the `Unresolved` path that closes an abandoned obligation and files its attention item. Ships in shadow mode: every existing consumer additionally computes `busyness()` and logs agreement or disagreement with its current signal, while **changing no decision**. This is a validation study of the chosen approach, not a comparison between approaches, and the log fields should say so.

- Effort hint: `medium`
- Dependencies: entries 3, 5
- Scope: in-scope

**8. Cut the three reap-and-spare consumers over to `busyness()`**

Replace the busyness input in `stale_worker_sweep`, `husk_pane_sweep::live_process_evidence`, and `dead_pid_sweep::corroborating_liveness` with `busyness(exec).is_busy() || <existing signal>`, keeping `current_tool` as a retained disjunct. One seam, three files, one subsystem, one review. Update the sweep unit tests in the same change. Gated on entry 7's shadow output showing no unexplained disagreement.

- Effort hint: `medium`
- Dependencies: entry 7
- Scope: in-scope

**9. Cut `nudge_or_park` over; delete `build_wait` and `background_children`**

Replace both nudge suppression paths with `busyness()`, and delete `build_wait.rs`, `build_wait_tracker.rs`, `background_children.rs`, their trackers, and the tests that pin them — including `completion/tests/t02.rs`'s `FixedDescendantProbe(2)`, which pins the premise this change supersedes. Sweeping those tests belongs in this diff, not a follow-up, so the contradiction is visible in the same change that creates it. **Ordering note:** this entry and entry 8 are functionally independent but both edit `app.rs`'s engine wiring (entry 8 adds the verdict dependency, entry 9 removes `with_background_activity_probe` at `app.rs:1165`). Entry 8 lands first; entry 9 forward-ports entry 8's wiring preservingly rather than reverting it.

- Effort hint: `medium`
- Dependencies: entries 1, 7, 8
- Scope: in-scope

**10. Replace the foreground mandate with the `boss run` contract and sweep its tests**

Rewrite `runner/prompt.rs`'s pre-push gate and conflict-resolution gate blocks to state the property rather than the container: build-class commands run through `boss run`, backgrounding is permitted, the tri-state is authoritative, `abandoned` is never a pass, and `Script completed` refers to the JavaScript cell rather than the command. Update `compose_prompt_tests.rs`, which currently asserts `prompt.contains("FOREGROUND")` — the premise and the test that pins it change in one diff.

- Effort hint: `small`
- Dependencies: entries 6, 8, 9
- Scope: in-scope

**11. Enforcement layers 1 and 2: deny rules plus a `boss run` interception guard**

Add deny rules for bare build-class argv (`Bash(bazel:*)`, `Bash(cargo:*)`, and the checkleft/test shapes) to `deny_rules()` in `worker_setup.rs`, leaving `boss run --` as the only spelling that passes, and add a `PreToolUse` interception guard that catches the shapes a leading-token match cannot — wrapper commands, shell-variable indirection, `sh -c`. Build it from the existing `python_command_guard!` seam, copying `BOSS_LAUNCH_GUARD_COMMAND`'s delimiter splitting, wrapper stripping, and single-level variable expansion, and copying `boss-path-guard.py`'s fail-closed posture: a payload the guard cannot parse is blocked, not approved. Wire it through `AgentDriver::tool_use_interception_wiring` so per-driver capability decides whether it is installed, and assert in tests that it denies rather than rewrites (Codex's `ToolUseInterception` is deny-only). The guard's deny message must name the wrapped form to run instead — a guard that blocks without naming the sanctioned alternative just costs a turn.

- Effort hint: `medium`
- Dependencies: entry 6
- Scope: in-scope

**12. Enforcement layer 4: detect and report build-class commands that ran outside an obligation**

At the `PostToolUse` boundary, recognise a build-class command whose argv is not a `boss run` invocation and for which no obligation was opened, and record it against the execution — an attention item naming the argv, plus a per-execution counter, following `worker_sandbox_audit`'s observe-attempts shape and `codex_unobserved_command`'s bounded audit trail (including its overflow attention, so a pathological run cannot file unboundedly). This is what makes adoption measurable rather than assumed: without it, "adoption is voluntary" is a claim nobody can check, and the operator has no basis on which to answer the layer-3 fork. Detection only — this entry changes no decision and blocks nothing.

- Effort hint: `medium`
- Dependencies: entries 5, 7
- Scope: in-scope

**13. End-to-end acceptance sweep against a real isolated engine**

Run an isolated engine and drive a real Claude worker and a real Codex worker, each executing a genuine multi-minute `bazel` build through `boss run`, and demonstrate both directions: neither is reaped or retired across sweep passes, and a `SIGKILL`ed worker is still reclaimed on the next pass. Also exercise the enforcement path on both drivers: a bare build-class command is denied on Claude, and on Codex confirm what the guard actually does rather than assuming parity — the deny-only capability and the measured `write_stdin` hole mean the Codex result is a finding to record, not a box to tick. Record raw artifacts. Deliberately end-to-end rather than a reconstructed harness, because the failures on this path have all been integration failures.

- Effort hint: `medium`
- Dependencies: entries 8, 9, 10, 11, 12
- Scope: in-scope

**14. Retire `current_tool` as a busyness input**

Once entry 13 and production telemetry show `boss run` adoption is high enough that the retained `current_tool` disjunct never changes a decision, drop it from the three reap-and-spare consumers, leaving `busyness()` as the sole input. `current_tool` remains as a UI field. Entry 12's unwrapped-command counter is the telemetry this gate reads.

- Effort hint: `small`
- Dependencies: entries 12, 13
- Scope: deferred (future / not a v1 blocker) — gated on measured adoption that does not exist yet; retiring it early reintroduces the reaping risk for any worker not using the wrapper

**15. Busyness for delegated subagent work**

Give the auto-nudge a correct signal for the case `background_children` was aimed at: a worker whose turn genuinely ended because it delegated work to a harness subagent. The signal belongs in the driver's own event stream (a delegation boundary the runtime reports), not in a process count, and it is a different question from command busyness.

- Effort hint: `medium`
- Dependencies: entry 7
- Scope: deferred (future / not a v1 blocker) — a distinct question with no measured incident behind it; deleting the wrong probe in entry 9 is the v1 correction

**16. Enforcement layer 3: deny-by-default validation claims citing observed exit codes**

Require a worker asserting "checks passed" to cite an obligation handle whose verdict is `exited 0`, and refuse the claim otherwise. This is the exit-code investigation's option 4, the only measure that also covers sandbox denials (where the exit code is `0` and honest), and — per [Enforcement](#enforcement-what-stops-a-worker-sidestepping-the-wrapper) — the only layer that removes the payoff from sidestepping rather than obstructing the act, and the only one that needs no hook surface and so holds uniformly across drivers. Generalises the refusal `codex_unobserved_command` already applies to `NO_CHANGES_NEEDED`, so the mechanism exists and this widens its trigger; the `large` hint is carried over from the original estimate and is probably now conservative. It reaches the driver abstraction and the deliverable-coverage gate, which is why it is not folded into this project unilaterally.

- Effort hint: `large`
- Dependencies: entries 6, 12
- Scope: deferred (future / not a v1 blocker) — reaches beyond a busyness signal, and it is a policy decision about how Boss treats worker claims that a human should make. Entry 12 supplies the adoption data that decision needs. This is the fork the questions manifest puts to the operator; deferring it is the design's recommendation, **not** a judgement that enforcement is unnecessary
