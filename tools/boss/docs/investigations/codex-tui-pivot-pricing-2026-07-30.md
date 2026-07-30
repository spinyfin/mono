# Pricing the pivot from `codex exec` to an interactive TUI session

- **Date:** 2026-07-30
- **Kind:** investigation / spike — pricing and sequencing only, no implementation
- **Code sha:** all `file:line` citations are against `main` @ `c2e20f1d`
- **CLI pin:** `codex-cli 0.145.0` (an update to 0.146.0 was offered and declined, to keep the pin honest)
- **GhosttyKit pin:** `ghosttykit-5659cef`, sha256 `82b8d947…a814a1`, the current `MODULE.bazel:212-217` pin
- **Related:** [`designs/codex-as-a-first-class-agent-driver.md`](../designs/codex-as-a-first-class-agent-driver.md), [`codex-driver-execution-shape-postmortem-2026-07-29.md`](codex-driver-execution-shape-postmortem-2026-07-29.md), [`ghostty-codex-pane-viability.md`](ghostty-codex-pane-viability.md), [`ghostty-grok-pane-viability.md`](ghostty-grok-pane-viability.md)

## Verdict

**Make the TUI the only Codex path. Do not ship two shapes.** Every open validation question came back favourable to the TUI, and the "support both" option is not a configuration choice — it is a spawn-line contract conflict that no existing mechanism can express for one driver slug.

Two of the three flags the conformance contract _requires_ on a Codex spawn line are hard argument errors on the bare TUI, and the flag the TUI needs is a hard argument error on `exec`. That is measured, not inferred.

## What this spike did not re-open

The choice of `codex exec` over the TUI was never made as a decision — the design doc says so itself (`designs/codex-as-a-first-class-agent-driver.md:653-670`) and the postmortem documents how the inheritance happened. The three possible grounds for it are already refuted there. None of that is re-argued here, and the four faithfully-settled spike questions (Q1, Q2, Q6, Q7 on Layer D) were not re-run.

What remained was a cost and sequencing deferral. That is what this document prices.

## Framing correction that shrinks the estimate

Codex is **already pane-hosted in a GhosttyKit pane**. `CodexDriver::spawn_invocation` builds a pane command line and a `SpawnPlan` exactly as Claude and Grok do (`engine/driver/src/codex.rs:1159-1180`); the only direct `Command::new("codex")` in the tree is a conformance test.

So the work is _"make the already-pane-hosted Codex process interactive and long-lived"_, not _"move Codex into a pane"_. The whole `ProgressIngress::AgentJsonlFile` file-tail transport survives untouched — it exists because Ghostty owns the pty master, not because of `exec` (`engine/driver/src/codex.rs:1368-1383`). It is out of scope for the pivot, and this spike confirmed it already handles a real multi-turn TUI rollout (V6 below).

## Method and apparatus honesty

The committed Grok spike host (`ghostty-grok-pane-viability-artifacts/ghosttykit_host/`) was copied to a throwaway scratchpad directory and re-pointed at `codex`, keeping its `SPIKE_SCENARIO` switch. It embeds libghostty through the same C API calls Boss uses — `ghostty_surface_new` with `ghostty_surface_config_s.initial_input`, `ghostty_surface_read_text` to observe, `ghostty_surface_text` + `ghostty_surface_key` Return (`0x24`) to inject (the `submitText` / `SendToPane` path), `ghostty_surface_key` Esc (`0x35`) to interrupt — and links the pinned `ghosttykit-5659cef` xcframework.

This is the **Layer D / Boss-faithful** topology: an in-process embedded surface, not standalone Ghostty.app, not `script(1)`, not a Python pty. Q3, Q4 and Q5 had never run under it; that is why they were re-validated and nothing else was.

No repository files were changed by the apparatus. One throwaway `#[test]` was temporarily added to `engine/driver/src/codex/progress.rs` to drive captured rollouts through the shipped parser under `bazel test`, and was reverted; the working copy is clean apart from this document.

**One harness defect found and fixed mid-run, disclosed here because it initially produced a false reading.** The first V4 attempt used `SLEEP_DONE` as the turn-1 completion marker, and the prompt itself contained that literal — the screen scrape matched the prompt's own echo at t=4.1s and suppressed the injection entirely. Every marker was then reworded so the model _constructs_ the answer token (`the word SLEEP immediately followed by the word DONE`) and the prompt cannot satisfy the match. All results below are from the corrected harness.

## Validation results

Six items were open. All six are now answered.

### V1 — positional prompt auto-submits on the bare TUI, under GhosttyKit — CONFIRMED

Seeded via `ghostty_surface_config_s.initial_input`, the bare `codex` TUI (no `exec` subcommand) accepted a positional prompt, auto-submitted it without any synthetic Return, ran the turn, and the answer was readable via `ghostty_surface_read_text`. A second turn submitted afterwards through the Boss `submitText` path also landed and completed.

Previously this had only run under `script(1)` against a pty master.

### V2 — `--no-alt-screen` under the pinned GhosttyKit build — CONFIRMED

Both modes render readably and are scrapeable. The operative difference is scrollback: with `--no-alt-screen` the viewport and full-screen reads diverge and the screen read grows (1693 vs 1830 bytes on a short run; 1647 vs 2098 on a longer one), so history accumulates. Under default alt-screen the two reads were byte-identical (1606 = 1606), i.e. the readable text is capped at one screenful.

For a long-lived multi-turn session that difference matters — `--no-alt-screen` is the correct choice, and it is sane under the pinned build specifically.

### V3 — Esc aborts a turn without killing the process, and _does_ reach a turn boundary — CONFIRMED

`ghostty_surface_key` with keycode `0x35` into a live turn interrupted it. The busy affordance went down and back up for the follow-up turn (`busy_transitions = 1.7:true 7.7:false 10.1:true 14.9:false`), the process survived (`process_exited=false`, foreground pid still live), and the session accepted and completed a follow-up turn.

The pane renders `■ Conversation interrupted - tell the model what to do differently.`

Crucially, **this is not Grok's skip-the-boundary behaviour.** The rollout records `event_msg.turn_aborted`, and driving that rollout through the shipped parser emits both a `Notification("turn aborted: interrupted")` and a real `Stop(Interrupted)` — a genuine turn boundary. Confirmed against production code, not by inspection.

### V4 — mid-turn `submitText` into a live TUI composer: **`Buffers`**, with a caveat — CONFIRMED

This is the item that had never been measured in any apparatus; Q2 measured injection into `codex exec`, never into a live TUI turn.

Injecting mid-turn through the exact Boss `submitText` path produced an explicit, first-class affordance from Codex itself:

```
• Messages to be submitted after next tool call (press esc to interrupt and send immediately)
  ↳ MIDPROBE: do not use tools. Reply with exactly one token: …
```

The message was queued, delivered at the next tool-call boundary, and answered. Nothing landed in a tty line discipline and nothing was executed by a shell. `mid_turn_pane_input()` should become `MidTurnPaneInput::Buffers` — and now on measured evidence, which is the postmortem's own standard.

**The caveat is a new finding and it is load-bearing.** The buffered message is folded into the _running_ turn, not deferred into a new one. The rollout for that session carries two `user_message` records but only **one** `task_started` and **one** `task_complete`:

```
V4 event_msg types: Counter({'user_message': 2, 'agent_message': 2, 'token_count': 2,
                             'task_started': 1, 'task_complete': 1})
```

The shipped parser therefore emits one `UserPromptSubmit` and one `Stop` for two prompts. A Boss nudge or steer delivered mid-turn will be _acted on_ but will not produce its own turn boundary, and the second prompt is invisible to the normaliser (`event_msg/user_message` is an unmapped record). Any design that counts turns, or waits for a boundary per delivered prompt, must account for this.

Observed side effect worth noting: the model answered the _newer_ instruction and never emitted turn 1's answer token.

### V5 — Codex TUI liveness markers and pane mode, for a `PaneMonitorSpec` — CAPTURED

Verbatim literals from the GhosttyKit surface reads:

| role               | Codex TUI literal                                                |
| ------------------ | ---------------------------------------------------------------- |
| agent header       | `>_ OpenAI Codex (v0.145.0)`, `/model to change`, `permissions:` |
| busy               | `esc to interrupt` (e.g. `• Working (19s • esc to interrupt)`)   |
| starting           | `Booting MCP server:`                                            |
| prompt prefix      | `›`                                                              |
| interrupted        | `■ Conversation interrupted`                                     |
| session id on exit | `To continue this session, run codex resume <uuid>`              |

Pane mode: `--no-alt-screen` (per V2).

### V6 — the shipped rollout parser handles a multi-turn TUI rollout end to end — CONFIRMED

Real captured TUI rollouts were driven through the production `CodexRolloutProgressSession` under `bazel test`. A two-turn session with an Esc abort normalises cleanly:

```
   0 session_meta    -> SessionStart(Startup)
   1 event_msg       -> UserPromptSubmit
  12 response_item   -> PreToolUse(Bash)
  13 response_item   -> PostToolUse(Bash)
  16 event_msg       -> Notification(turn aborted: interrupted)
  16 event_msg       -> Stop(Interrupted)
  18 event_msg       -> UserPromptSubmit
  27 event_msg       -> Stop(Completed)
```

One `SessionStart` for the whole session (no spurious re-`Startup` per turn), correctly paired tool events, and one `Stop` per real turn. The unmapped records (developer/user/assistant `message`, `reasoning`, `user_message`, `agent_message`, `token_count`) return `NormalizeError::UnknownEvent` and are **counted and skipped, never fatal**, which is the documented tolerance contract of the reader (`engine/stdout-progress/src/lib.rs:50-56`, `:348-356`).

No parser work is required for the pivot.

### The one argument for keeping `exec`, now measured: idle process cost

A long-lived TUI holds a process per slot between turns where `exec` holds none. The design doc names this and the spike listed it as uncharacterised. It is now characterised.

An idle `codex` TUI parked at its composer after answering one trivial prompt, sampled every 5s over a minute:

| t (s) | RSS    | %CPU |
| ----- | ------ | ---- |
| 20    | 282 MB | 2.3  |
| 30    | 316 MB | 2.5  |
| 40    | 335 MB | 0.6  |
| 50    | 356 MB | 0.6  |
| 60    | 344 MB | 1.3  |

So roughly **280–360 MB resident and 0.6–2.5% of one core per idle slot**, and the resident set grew over the sample rather than settling immediately. At eight concurrent slots that is ~2.4–2.8 GB held between turns on an operator's laptop.

This is a real cost and it is the honest counterweight in the recommendation below. It is not, however, an argument for keeping a _second shape_ — it is an argument for slot-count discipline and for reaping idle sessions, which applies equally to Claude and Grok, both of which are already `Persistent` (`engine/driver/src/claude.rs:1842-1848`).

## The central question: second path, or the only path?

### Evidence that "both" is cheap

Every shape-relevant declaration is an `&self` method rather than an associated const, and `CodexDriver` already branches its own spawn line on config — `codex_sandbox_extra_args(input.worker_kind, input.codex_sandbox_enforced)` varies the sandbox flag per worker kind (`engine/driver/src/codex.rs:1352`, helper at `:329`). So a per-run branch is expressible in principle.

### Evidence that "both" is a trap — and one piece of it is now measured

**1. The two declarations the engine actually consults are per-driver, not per-run.** `worker_process_lifetime()` (`codex.rs:1589-1591`) and `mid_turn_pane_input()` (`codex.rs:1574-1576`) take only `&self`. They have no access to the run, the request, or config. The two shapes need opposite values for both.

**2. The spawn-line flag contract conflicts, and this is a hard argument error, not a style preference.** Measured directly against `codex-cli 0.145.0` by invoking each flag under both subcommands:

| flag                        | bare TUI                                       | `codex exec`                         | conformance status                           |
| --------------------------- | ---------------------------------------------- | ------------------------------------ | -------------------------------------------- |
| `--color always`            | **rejected** — `unexpected argument '--color'` | accepted                             | **required** (`conformance/fixtures.rs:246`) |
| `--skip-git-repo-check`     | **rejected** — `unexpected argument`           | accepted                             | **required** (same)                          |
| `--strict-config`           | accepted                                       | accepted                             | **required** (same)                          |
| `--no-alt-screen`           | accepted                                       | **rejected** — `unexpected argument` | not in contract; needed by the TUI           |
| `-a` / `--ask-for-approval` | accepted                                       | **rejected** — `unexpected argument` | **forbidden** (`fixtures.rs:258`)            |
| `--json`                    | rejected (no such flag)                        | accepted                             | **forbidden** (same)                         |

Two of the three _required_ flags are hard errors on the shape we would be pivoting to. The conformance harness asserts one spawn line per driver (`codex_exec_reference_command`, `fixtures.rs:265`; `assert_codex_exec_spawn_contract`, `:281`), and there is no variant column and no per-driver sub-mode mechanism. A second shape therefore means either a schema change to the conformance contract or a second registered slug — which forks the driver in all but name. No driver has two shapes today.

**3. Keeping an exec path preserves the entire one-shot subsystem for a topology nothing would ship.** Priced below.

### Recommendation

**One shape: the interactive TUI.** Retire `codex exec` from the driver rather than retaining it behind a flag.

The measured evidence is uniformly favourable — auto-submit works, output is observable, Esc aborts cleanly _and_ reaches a real turn boundary, mid-turn input buffers safely, and the rollout parser already handles multi-turn sessions. The only genuine cost, idle process residency, is now a number (~300 MB/slot) rather than a fear, and it is a capacity-planning question rather than a reason to carry two incompatible spawn contracts.

If the operator nonetheless wants both, the honest price is: a conformance-schema change to express per-variant required/forbidden flag sets, per-run plumbing for `worker_process_lifetime()` and `mid_turn_pane_input()` (both currently `&self`), **plus indefinite retention of the ~750 lines of one-shot subsystem itemised below**, including the attention kind that exists solely to tell an operator that Boss cannot probe, escalate, or nudge a one-shot worker. That is the cost of the option, and it should be stated before it is bought.

## Deletion payoff — corrected accounting

The payoff only exists if `WorkerProcessLifetime::OneTurnPerProcess` has no implementor. The scope brief estimated roughly ten files and about fifteen hundred lines. **The true figure is smaller, and part of the estimate belongs to a cleanup that is available today without any pivot.** Correcting this is the point of pricing it.

### Genuinely unlocked by the pivot

| surface                                                                                              | `file:line`                                                                     | approx. lines |
| ---------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- | ------------- |
| one-shot exit classifier, `ProcessExitVerdict`, `ProgressDrain`, `ExpectedTurnExit` — the whole file | `engine/core/src/worker_process_exit.rs` (entire, 501 lines)                    | 501           |
| `ExpectedTurnExit` arms in the dead-pid sweep                                                        | `dead_pid_sweep.rs:460`, `:649`, `:754`                                         | ~40           |
| one-shot unreachable attention kind + filer + its test                                               | `dead_pid_sweep.rs:827`, `:843-870`, `:2431`                                    | ~60           |
| one-shot skip clause in the dead-pane sweep                                                          | `dead_pane_sweep.rs:67-73` (contract), `:250` (the guard), tests `:734`, `:752` | ~30           |
| `StreamHalt` tri-state collapses to binary (`Drain` arm)                                             | `agent_jsonl_progress.rs:146-158`, `:271`, `:761`                               | ~40           |
| `WorkerProcessLifetime` enum + trait method + defaults                                               | `driver/src/lib.rs:848-882`, `:2000-2005`                                       | ~45           |
| driver-side declaration and its tests                                                                | `codex.rs:1578-1591`, `:2547-2554`                                              | ~30           |

**≈ 750 lines across 6 files including tests** — meaningful, but half the brief's estimate.

### Already dead today — deletable without the pivot

No production driver returns `ProgressIngress::StdoutJsonl`. Claude returns `HookCallback` (`claude.rs:662`), Grok returns `HookCallback` (`grok.rs:502`), Codex returns `AgentJsonlFile` (`codex.rs:1376`). The only `ProgressStreamSource::StdoutJsonl` selector in the tree is the Codex driver's own match arm (`codex.rs:1409`), reachable only from an ingress nothing selects.

So the **stdout-dialect normaliser and `StdoutEnvelope` parser** — `CodexProgressSession` (`codex/progress.rs:69-338`), `enum StdoutEnvelope` (`:678-706`), `parse_stdout_envelope` (`:707-`), ≈ 420 lines plus tests — are _already_ unreachable in production. They should be counted as an independent cleanup, not as pivot payoff. Attributing them to the pivot inflates its return.

### Not deletable at all — shared with the surviving transport

`engine/core/src/stdout_progress.rs` (663 lines) and the `boss-engine-stdout-progress` crate (432 + 1003 lines) are **shared infrastructure**, not one-shot machinery. The surviving file-tail path calls straight into them: `agent_jsonl_progress.rs:333` invokes `crate::stdout_progress::run_jsonl_progress_ingress_with_driver`. The module's own docs state the split (`stdout_progress.rs:1-32`). None of this is in scope for deletion under any option.

### Safe migration order

The one-shot consumers read the _declaration_, not the driver name, and fail closed to `Persistent` — an unknown execution, unregistered slug or DB error is treated as `Persistent` and reaped exactly as before (`worker_process_exit.rs:25-30`, `:214`). **Flipping `worker_process_lifetime()` to `Persistent` is therefore behaviour-safe before any deletion**, and should be a separate, revertible commit ahead of the removals.

## Pivot scope inventory

### Obsolete

| surface                                              | `file:line`                                     | note                                                                                                                                             |
| ---------------------------------------------------- | ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| the `codex exec` spawn line, including `< /dev/null` | `codex.rs:791-807`                              | `< /dev/null` exists so `exec` does not block on stdin; a TUI _needs_ the tty                                                                    |
| the `exec ` pane-command prefix                      | `codex.rs:813-820`, rationale at `:754-765`     | its stated rationale is that no shell survives codex exiting — the opposite of what a long-lived session wants; neither Claude nor Grok emits it |
| stdout-dialect progress session and envelope parser  | `codex/progress.rs:69-338`, `:678-706`, `:707-` | already unreachable today (see above)                                                                                                            |
| the turn-boundary column's writer and reader         | via `worker_process_exit.rs` (whole file)       | only consumer is one-shot exit classification                                                                                                    |
| one-shot subsystem                                   | see deletion table                              | gated on no `OneTurnPerProcess` implementor                                                                                                      |

### Needs rework

| surface                                 | `file:line`                                           | change                                                                                                                                                                                                                                      |
| --------------------------------------- | ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| spawn invocation, command half          | `codex.rs:766-812`                                    | drop `--color always` and `--skip-git-repo-check` (**hard errors on the TUI**, measured); keep `--strict-config` and `--sandbox`; add `--no-alt-screen`; `-a never` is now available and accepted                                           |
| `mid_turn_pane_input()`                 | `codex.rs:1574-1576`                                  | `Rejects` → `Buffers`, on V4's measured evidence. Note the caveat: buffered input folds into the running turn and yields no extra boundary                                                                                                  |
| `worker_process_lifetime()`             | `codex.rs:1589-1591`                                  | `OneTurnPerProcess` → `Persistent`; do this first, alone (fail-closed, see above)                                                                                                                                                           |
| `turn_boundary()` `continuation: false` | `codex.rs:1423-1442`                                  | the comment's premise — "`codex exec` does not re-enter via stop-hooks" — no longer holds for a session that continues after a boundary                                                                                                     |
| `AwaitingInputSignal` omission          | `codex.rs:1146-1156`                                  | the stated reasoning **inverts**: it argues `turn.completed` means exit is imminent, not "blocked on a human". A TUI parked at its composer genuinely _is_ awaiting input, and V5 gives the literal (`›` prompt prefix, busy marker absent) |
| spawn-contract conformance fixtures     | `conformance/fixtures.rs:246`, `:258`, `:265`, `:281` | required/forbidden sets are `exec`-specific and must be restated for the TUI                                                                                                                                                                |
| `structured_output_wiring`              | `codex.rs:1593-1604`                                  | env-file contract is shape-neutral; re-verify the comment's `--output-last-message` extension path against the TUI subcommand                                                                                                               |

### Unchanged

- `ProgressIngress::AgentJsonlFile` file-tail transport (`codex.rs:1368-1383`) — survives untouched; V6 proves the parser copes.
- `CodexRolloutProgressSession` and the rollout dialect (`codex/progress.rs:340-648`).
- Per-run `CODEX_HOME` provisioning, auth snapshot, trust attestation, PreToolUse guard materialisation.
- `progress_fidelity()`, capability set apart from `AwaitingInputSignal`.
- `reap()` — `ReapDelivery::ProcessGroup` (`codex.rs:1563-1565`) is shape-neutral.

## Two gaps the pivot creates — both in scope

### 1. No progress-ingress re-establishment on readoption — fatal for a long-lived session

`ServerState::readopt_live_worker` (`engine/core/src/app/readoption.rs:119-238`) restores exactly three things, in a documented order: the execution DB row, the pool claim, and the live-state entry (including the `awaiting_input_capable` derivation at `:193-197`). It never re-prepares or re-activates progress ingress.

**How I established the absence:** `grep -n 'progress\|ingress\|jsonl' engine/core/src/app/readoption.rs` returns no matches. The ingress lifecycle is `prepare_progress_ingress` → pane request → register → `activate_progress_ingress` (`spawn_flow.rs:253`, `:263`, `:579`; manager at `agent_jsonl_progress.rs:177-226`), and it is driven only from the spawn flow.

This is harmless for a one-turn run — the run is over by the time readoption could matter. For a long-lived TUI session that outlives an engine restart it is **fatal**: no tail, therefore no turn boundary, therefore no completion. The session would sit alive and unobserved forever.

This must be scoped into the pivot, not filed behind it.

### 2. Multi-turn accumulation in two trackers reasoned against a single-turn run

- **Unobserved-command tracker.** Reasoned as _"only ever gets more true, never less"_ with a per-execution cap of 50 (`codex_unobserved_command.rs:32-35`, `:48`, `:63-70`). Under a persistent session a command abandoned in turn one would permanently refuse every later no-op completion for the life of the session, and overflow past 50 is silently dropped.
- **Rollout call tracker.** Fixed LRU bound of 256 (`codex/progress.rs:28`, evicted at `:390` and `:1004`). Sized for one turn; a long multi-turn session will evict live correlations, silently.

Both bounds are correct for a one-shot run and wrong for a session. Neither drops loudly.

## Latent defect surfaced: Codex declares no `pane_monitor_spec`

`CodexDriver` does not implement `pane_monitor_spec()`, so it falls through to the trait default of `None` (`driver/src/lib.rs:1597-1599`). The spawn flow forwards that as `None` (`spawn_flow.rs:450`), and the app's `PaneMonitorSpec.fromWire` returns `claudeDefault` for an absent payload (`app-macos/Sources/Ghostty/TerminalPaneSession.swift:27-28`).

**How I established the absence:** `grep -n 'fn pane_monitor_spec' engine/driver/src/{claude,codex,grok}.rs` returns `claude.rs:471` and `grok.rs:275` only — Codex has no override.

Claude's default literals are `agentMarkers: ["Claude Code", "auto mode on", "/effort"]` and `promptPrefixes: ["❯"]` (`TerminalPaneSession.swift:17-23`). None of those can ever appear in a Codex pane, whose header is `>_ OpenAI Codex (v0.145.0)` and whose prompt prefix is `›`. **Every Codex worker's pane monitor is therefore pinned to not-detected today**, independent of this pivot.

One nuance worth recording: Claude's `busyMarkers: ["esc to interrupt"]` _does_ match Codex verbatim — so busy detection incidentally works while agent detection can never succeed.

**Recommendation: file it as its own row, not inside the pivot.** It is a live defect on the shipped `exec` path, it is a few lines, and V5 already supplies the exact literals. Holding it behind a pivot decision leaves a known-broken indicator in place for no reason. The pivot would only need to revisit the `startingMarkers` value.

## Sequencing

**The exec-resume-probe and pane-lifecycle-semantics row is mooted by this pivot.** Its primary deliverable is abort-by-signal into a mid-turn `codex exec`; a live session probes via pane text and interrupts via Esc natively, and V3 shows the abort produces a real `Stop(Interrupted)` boundary. **Recommendation: close it, and record the mooting reason.** Retargeting it to the exec path only makes sense under the two-shape option this document recommends against; keeping it as-is blocks a phase-1 acceptance sweep on work that a TUI pivot deletes.

**The two blocked per-kind eligibility rows have no code behind them.** Driver resolution is pool-override-then-row-slug (`driver_transcript.rs:104-128`, `:133-`); nothing keys driver selection on work-item kind anywhere. _How I searched:_ `resolve_execution_driver_slug` consults `pool_override_driver_slug` (keyed on the review/automation **worker id**, not the item kind) and then `get_execution_driver_slug`; no `task_kind` or `worker_kind` term participates in either. These rows are unstarted design intent, not code to unwind.

**Carry this dependency: the control verbs are dead declarations today.** `probe()`, `interrupt()`, `stop()` and `reap()` are declared by all three drivers and read by no engine code — the engine hardcodes pane-send, pane-interrupt and process reap. _How I searched:_ `grep -rn '\.probe()\|\.interrupt()\|\.reap()\|driver\.stop()'` across `tools/boss/` excluding `target/` returns hits only inside `#[cfg(test)]` assertion blocks (`claude.rs:1824-1827`, `codex.rs:2537-2540`, `grok.rs:2164-2167`, `driver/src/lib.rs:2304-2307`). The abstraction project's ControlVerbs trait-surface row is blocked, so **a validated Codex Esc semantics buys nothing until that lands.**

## Corrections to existing documents

**The postmortem contains a factual error, and it is a fresh follow-up rather than a revision** — that PR is merged.

`codex-driver-execution-shape-postmortem-2026-07-29.md:282` states: _"Nothing in the trait requires a driver to declare its process lifetime relative to a turn."_ That is false on `main`. `WorkerProcessLifetime` is a trait method with a `Persistent` default (`driver/src/lib.rs:2000-2005`, enum at `:848-882`), and Codex declares `OneTurnPerProcess` through it (`codex.rs:1589-1591`) — not, as the same paragraph says, "in prose comments". Consequently its recommendation 3 at `:300` ("add a trait declaration for process lifetime relative to a turn") is already satisfied. The surrounding argument — that the abstraction should also _reject_ a driver whose lifetime contradicts the mechanisms applied to it — still stands and is untouched by the correction.

**The design doc contradicts itself on the execution shape and needs a cleanup pass.** Its chosen-approach section still reads _"Drive `codex exec --json` as the worker CLI … with `--output-last-message`"_ (`designs/codex-as-a-first-class-agent-driver.md:706`), and the execution-shape material at `:66` and `:159-160` still presents `--json` and `-o/--output-last-message` as the shape. But the same document's Alternative 8 (`:690-700`) records that `--json` was removed from the spawn line and is now _forbidden_ by the contract, and `--output-last-message` is not on the spawn line at all — `structured_output_wiring` uses the common-denominator env-file contract and names those flags only as a possible future extension (`codex.rs:1594-1604`). The chosen-approach and execution-shape sections should be reconciled with Alternative 8.

## Estimated cost

Assuming the single-shape recommendation, and excluding the abstraction-project dependencies called out above:

| slice                                                                                       | scope                                                      | size    |
| ------------------------------------------------------------------------------------------- | ---------------------------------------------------------- | ------- |
| flip `worker_process_lifetime()` to `Persistent`, alone                                     | fail-closed, revertible, no deletions                      | trivial |
| spawn line + conformance fixtures                                                           | flag set restated for the TUI subcommand                   | small   |
| `mid_turn_pane_input()` → `Buffers`, `turn_boundary` and `AwaitingInputSignal` re-reasoning | declarations plus their doc comments and tests             | small   |
| **progress-ingress re-establishment on readoption**                                         | new plumbing; the one genuinely novel piece of engineering | medium  |
| multi-turn accumulation bounds in the two trackers                                          | re-reason both caps for session lifetime, drop loudly      | small   |
| one-shot subsystem deletion                                                                 | ~750 lines across 6 files                                  | medium  |
| `pane_monitor_spec` for Codex                                                               | _file separately_; V5 supplies the literals                | trivial |
| stdout-dialect deletion                                                                     | _file separately_; already dead, ~420 lines                | small   |

The pivot is dominated by the readoption gap, not by the shape change itself. Everything the earlier framing feared — pane hosting, progress transport, rollout parsing, abort semantics — is either already done or now measured to work.

## Follow-up code changes for separate filing

This is an investigation; no code was changed. The following are proposed as their own rows:

1. Declare `pane_monitor_spec()` on `CodexDriver` using the V5 literals — live defect on the shipped path.
2. Delete the already-unreachable Codex stdout dialect (`ProgressStreamSource::StdoutJsonl`, `CodexProgressSession`, `StdoutEnvelope`).
3. Reconcile the design doc's chosen-approach and execution-shape sections with its own Alternative 8.
4. Correct the postmortem's process-lifetime claim at `:282` and its recommendation 3 at `:300`.
5. Re-establish progress ingress on worker readoption — required by this pivot, but a real bug for any future persistent non-Claude driver regardless.
