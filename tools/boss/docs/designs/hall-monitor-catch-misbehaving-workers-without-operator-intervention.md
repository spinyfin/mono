# Design: Hall monitor — judgement attached to detectors that already fire, probe-only

- **Status:** design for review (no implementation).
- **Date:** 2026-09-04
- **Project:** `proj_18d22416ab83db70_1cd` — Hall monitor: catch misbehaving workers without operator intervention
- **Design task:** `task_18d22416ab88a218_1ce` (execution kind `project_design`)
- **Source analysis:** `main@origin` at `bc4c91cd` (mono#2900)
- **Related docs:** [`worker-liveness-contract.md`](../worker-liveness-contract.md), [`attention-lifecycle.md`](../attention-lifecycle.md), [`dispatch-halt-state-vs-attention-items.md`](dispatch-halt-state-vs-attention-items.md), [`automated-reviewer-pass-on-every-agent-authored-pr.md`](automated-reviewer-pass-on-every-agent-authored-pr.md), [`notification-dedup-scoring.md`](notification-dedup-scoring.md), [`forensic-surfaces.md`](../forensic-surfaces.md)

The hall monitor is a judging agent that reads a misbehaving worker's brief and transcript tail, decides whether the worker is genuinely stuck or merely acting stuck, and — only when it is the latter — queues one specific corrective probe for delivery at the worker's next turn boundary. The contested property is in the title: the monitor adds **no new detector** and **no new effector**. It attaches an agent's judgement to the exact-state signals the engine already has plus one shape heuristic over signals it already records, and its only action is a probe.

## TL;DR

- **The monitor is a one-shot utility-model call, not an execution kind.** The `pr_review` machinery does not generalise to this shape: a review needs a cube workspace, a pane, a slot in a hand-rolled third pool arm, and host reconciliation, none of which a transcript reader needs, and a lease-free dispatch path does not exist. The engine already has the right precedent in `live_status.rs` (read a worker's transcript tail, call a cheap model, tolerate failure silently) and `attentions_detector.rs`. This is the finding that makes the project **two in-scope PRs**, not four.
- **Progress is computable today, with one gap.** `semantic_tool_condition == InFlight` already means "one outstanding tool call, healthy at any age" (`stale_worker_sweep.rs:278`), which is exactly the long-build guard the brief demands. The spin case is the complement of that classifier over the same checkpoint plumbing. The one thing nobody records is _which_ tools ran and how many times; a bounded per-run ring of tool names and a `tool_calls` counter close it.
- **Three triggers, two exact and one heuristic:** a `worker_blocked`/`worker_escalation` attention row was filed for a live run (state, not marker text, so it survives the `boss propose blocked` flag flip); `activity == WaitingForInput` past a threshold; and spinning (fresh checkpoint, condition `Idle`, no growth in tokens, rounds, or PR head across N unproductive intervals, many tool calls).
- **It speaks through the existing probe queue at a boundary, never by interrupting.** `ProbeQueuer::queue_probe` is drained at every `PostToolUse` and `Stop`; a "lazy block" verdict uses the resolving variant so the probe's delivery is the ack that resumes nudging, exactly as an operator's `bossctl probe` is today. A "legitimate block" verdict writes nothing and leaves the attention and the pause untouched.
- **Every firing is a durable, readable row** (`hall_monitor_verdicts`, modelled on `pr_review_verdicts`), plus an `engine-audit.log` line and a `bossctl` read verb. Nothing lands in attention items.
- **Ship observe-only first.** The parent flag turns the triggers and the judgement on; a sub-flag turns speech on. Troi's legitimate linker block is the regression case the observe-only phase must pass before the sub-flag flips.
- **One of the four incidents is already closed at the root.** Claude workers have spawned with `--disallowedTools=AskUserQuestion` since mono#2889 (2026-09-03), so Data's exact pathology cannot recur on the Claude driver. The `WaitingForInput` trigger still earns its place for the notification-driven cases that remain, but its coverage is narrower than the project brief assumed and the doc says where.

## Goals

- **Catch the observed pathologies without an operator at the pane.** In one night the operator hand-probed four workers whose failure was visible from the transcript alone: a wait-loop on a backgrounded build (Worf), a slot parked on a question the brief had already answered (Data), a `[blocked]` whose stated reason was the worker's own unfinished command plus a mechanically resolvable conflict it had manufactured by history surgery (Troi), and a `[blocked]` on a genuine Bazel linker failure after three sound attempts (Troi, earlier). The first three should be moved by a probe; the fourth must be left alone.
- **Split "when to look" from "what to say".** A heuristic decides when to look: continuous, token-free, cheap. An agent decides what is wrong and what to say, because the cases above share a surface signature and demand different responses, one of which is "do nothing".
- **Never fire on a legitimate long foreground build.** A worker correctly running `bazel test //...` in the foreground looks, on elapsed time alone, exactly like a spinning worker. The discriminator is one outstanding tool call versus many completed ones. This is an acceptance criterion: firing on long builds would train workers away from the behaviour the brief requires.
- **Never override a legitimate block.** The monitor's failure direction must be "stayed quiet when it could have spoken", never "told a correctly-blocked worker to keep going".
- **Make every intervention reviewable.** Each firing is recorded with its trigger, the evidence it read, its reasoning, its verdict, the exact text it sent, and what the worker said back, so a mis-calibrated monitor is visible in a table rather than silently corrosive in worker behaviour.
- **Bound the cost per firing.** The monitor reads the brief plus a bounded tail of the transcript, not the whole thing; one of the night's transcripts was 348 KB.

## Non-goals

- **No new silence or stall detector.** The auto-nudge ladder (Stop with no PR, escalating to a circuit breaker), `stale_worker_sweep` (30 minutes, then a 2-hour auto-reap), and the `worker_blocked` / `worker_escalation` attentions already exist. This design attaches judgement to those. The one new heuristic (spinning) is a _shape_ test over signals the engine already writes on every hook, running inside the sweep pass that already iterates every `Working` slot; it is not a silence timer.
- **No cadence-based probing.** A probe interrupts the worker's in-flight turn, and mid-flight interruption is a live suspect in how Troi's history got mangled. The monitor triggers on events and speaks only through the boundary-delivered probe queue.
- **No effector other than a text probe.** The monitor may not stop, reap, re-dispatch, re-prioritise, hold, interrupt (no `Escape`), edit the workspace of, type control keys or permission answers into, or answer questions on behalf of a worker. Widening that is a separate trust decision.
- **No output as attention rows.** Of roughly 6,970 open attention rows, 26 are reachable in the app and one renders. Findings that land there have done nothing. (That count is a live-database measurement; it is not recorded anywhere in the repo, and the nearest written findings are the read-surface gap in `dispatch-halt-state-vs-attention-items.md` and the raise-without-lower survey at the top of `attention_lifecycle.rs`.)
- **No replacement for the coordinator or the operator.** The verdict on a genuinely blocked worker is "leave it alone", which leaves the existing `worker_blocked` attention, the nudge pause, and the coordinator handoff untouched.
- **No remote (SSH) worker coverage in v1.** Remote runs have no local transcript file and therefore no live-status summariser either (`app/worker_events.rs:646`). The monitor inherits that gap and says so rather than pretending.
- **No changes to worker briefs beyond citations.** The monitor's judging prompt quotes the rules the brief already states, by calling the same prompt-composer functions; it adds no rules.

## What exists today (findings)

This section is the evidence the rest of the doc stands on. Line numbers are against `main@origin` at `bc4c91cd`.

### The detectors that already fire

| Detector                                    | Fire point                                                                                 | What it already decides                                                                           | Existing corrective channel                                                                                                                                                        |
| ------------------------------------------- | ------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `[blocked]` / `[effort-escalation]` markers | `completion/stop.rs:713` → `completion/worker_signals.rs:36` → `:288`                      | Worker declared itself stuck, with a parsed (or malformed) reason                                 | Files `worker_blocked` / `worker_escalation` attention; **pauses** the nudge ladder (`completion/nudge.rs:67`). Delivery of any probe to the run is the ack (`app/probes.rs:998`). |
| `boss propose blocked` (flag-gated seam)    | `app/proposals.rs:249` → `work/proposal_apply.rs:619` `apply_blocked`                      | Same fact, as a typed `worker_proposals` row                                                      | Writes the **same** `worker_blocked` attention row. Gated by `worker_proposals` and `worker_signal_proposals_seam`, both default off (`feature-flags/src/lib.rs:246`, `:262`).     |
| No-PR / stale-PR / empty-PR nudge ladder    | `completion/stop.rs:1442`, `:1454`, `:1474` via `nudge_or_park` (`completion/nudge.rs:38`) | The run stopped without its deliverable                                                           | Queues a canned probe; `NudgeBreaker` (3 identical, 12 absolute, 60 s debounce; `nudge_breaker.rs:50-76`) trips to a park.                                                         |
| `stale_worker_sweep`                        | `stale_worker_sweep.rs:1179` / `:1207` / `:1262`, every 60 s                               | Driver-originated progress stopped ≥ 30 min while `Working`, tool durably `Idle`, `Rich` fidelity | Attention only at 30 min; destructive auto-reap at 2 h. Skips `WaitingForInput` and `Idle` (`:828`).                                                                               |
| `run_done` backstop                         | `run_done_backstop.rs:158` → `completion/metadata_gate.rs:603`                             | Stop-boundary silence with an idle process tree                                                   | Probes `PROBE_DECLARE_DONE` through the same `nudge_or_park`.                                                                                                                      |
| `build_wait` phrase net                     | `completion/nudge.rs:98` → `build_wait.rs:51`                                              | Worker said it is legitimately waiting on a build                                                 | Suppresses the nudge without burning breaker budget; 45-min horizon (`build_wait_tracker.rs:36`).                                                                                  |

Everything a hall monitor needs to _notice_ is in that table. What none of these do is read the transcript and decide what to say.

### The probe machinery, and the seam that speaks at boundaries

`bossctl probe` defaults to interrupting: one `Escape` per the driver's `interrupt_plan` (`driver/src/claude.rs:1162`), confirm the turn ended by watching `WorkerActivity` leave `Working` (not a sleep), then write. That is the path the project brief warns about, and it is not the one the monitor uses.

The non-interrupting seam already exists and is already load-bearing. `ProbeQueuer::queue_probe` (`app/probes.rs:73`) puts text on a per-run FIFO that is drained at three real boundaries: every `PostToolUse` (`app/worker_events.rs:1865`, mid-turn, gated on the driver declaring it buffers typed input), every `Stop` (`:1453`, run twice per fan-out so a commitment made before completion still lands), and out-of-band via `deliver_queued_probes_now` (`app/probes.rs:115`) for a parked run that will emit no further hook. The variant `queue_probe_resolving_worker_signal` (`:297`) tags the probe so that its _delivery_ resolves the run's open `worker_blocked`/`worker_escalation` rows and resumes nudging; this is what every human `bossctl probe` does today (`app/executions.rs:479`). The monitor reuses all of it and adds no transport.

Two disciplines come with the seam. Every existing probe text names `NO_CHANGES_NEEDED` verbatim, pinned by a test, because an engine that asks a question in a language it cannot read once stranded a worker for 2 h 40 m (`completion.rs:1885-1893`). And probes are not written to `engine-audit.log` today; they go to `engine-trace.jsonl`, the in-process `ProbeRecord`, and a `probe_undelivered` attention when they are lost.

### The judging-agent precedents

Boss has two kinds of judging agent, and they are very different in weight.

**`pr_review` is an `ExecutionKind`.** It is created as a `work_executions` row, leases a full cube workspace and does `cube workspace goto --pr N` (`coordinator/execution.rs:1038-1064`), spawns a pane on the review pool (slots 25–32: `review-N → N + 16 + 8`, `coordinator.rs:1473`), runs the reviewer prompt from the pure `engine/pr-review` crate, and writes its verdict as a structured-output artifact the engine gates (`finalize_pr_review_pass`, `completion/finalize_passes.rs:705`; `passes_severity_gate`, `pr-review/src/parsing.rs:355`) and records in `pr_review_verdicts` in the same transaction as completion. The pool is not a generic facility: it is a hand-rolled third arm in about six three-way branches in `coordinator/scheduler.rs`, three gauge pairs in `dispatch_metrics.rs`, a prefix-keyed free function for the always-Opus policy, a kind list duplicated in SQL (`work/migrations_b.rs:2731`), three wire fields in `EnginePoolConfig`, and a parallel `reviewSlots` array in Swift. `AnswerAgent`, the other non-mainline kind, has no pool of its own and runs on the main pool with a queue-age alarm.

**The utility-model seam is a one-shot API call.** `UtilityTask` (`engine/utility-model/src/task.rs:19`) resolves provider, endpoint, model, and billing key per task; transport is `claude_client`. Five tasks exist. Two of them already read a worker's transcript tail: `live_status.rs` (Haiku, 30 entries, 3,200 prompt bytes, three-layer redaction, typed `SummarizerOutcome` so no failure is silent) and `attentions_detector.rs` (8,000 tail chars, 2,048 output tokens, flag-gated). `comment-classifier` is the template for a JSON-verdict call with retries and a parse function unit-tested without a network. None of these account for their own token usage; `boss cost` sees only `work_runs` rows.

### Progress signals

Recorded on every hook, per run, with no schema change needed to read them:

| Signal                                                      | Where                                                                                | Durable  | During a long foreground `bazel test` | During a `Monitor`/`TaskOutput` wait-loop |
| ----------------------------------------------------------- | ------------------------------------------------------------------------------------ | -------- | ------------------------------------- | ----------------------------------------- |
| `semantic_tool_condition` (`InFlight` / `Idle` / `Unknown`) | `work_runs` + `SlotMeta` (`semantic_progress.rs:22`, `work/semantic_progress.rs:35`) | yes      | pinned `InFlight`                     | flips `InFlight` ⇄ `Idle` constantly      |
| `semantic_progress_at`                                      | same                                                                                 | yes      | frozen at the call's start            | advances on every call                    |
| `current_tool` (name of the unbalanced `PreToolUse`)        | `LiveWorkerState` only (`protocol/src/live_worker_state.rs:171`)                     | no       | `Bash`                                | `Monitor` / `TaskOutput`                  |
| `output_tokens`, `rounds`, `agent_active_ms`                | `work_runs`, rewritten on every hook (`run_cost.rs:348`, `work/run_rows.rs:276`)     | yes      | frozen                                | grow slowly                               |
| `turn_boundary_at` / `stop_seen`                            | `work_runs` / `work_executions`                                                      | yes      | unchanged                             | advances (last-write-wins, no count)      |
| `pr_head_after`, staged push / PR-URL caches                | `work_executions` / memory                                                           | yes / no | unchanged                             | unchanged                                 |

Three facts from this table carry the design:

- **The build-versus-spin discriminator already exists and already errs the right way.** `classify_semantic_staleness` returns `Healthy` for `InFlight` unconditionally, however old the checkpoint (`stale_worker_sweep.rs:278`). While a call is in flight, `semantic_progress_at` is the start time of that call, so "one `Bash` call outstanding for eleven minutes" is computable today, durably. The acceptance criterion is satisfied by reading a field the sweep already reads.
- **The same classifier is blind by construction to the spin case.** A wait-loop emits balanced `PreToolUse`/`PostToolUse` pairs continuously, so the checkpoint keeps advancing and the sweep reads Worf as healthy forever. The spin heuristic is the _complement_ of the stale classifier over the same plumbing.
- **The one real gap is tool-call history.** Every tool event reaches `dispatch_worker_event_fanout` (`app/worker_events.rs:77`) with its name, and every one is discarded after the live-state reducer. Nothing today can say "the last forty calls were all `Monitor`". (`TaskOutput` does not appear anywhere in the engine tree; delegation is tracked by process tree, not tool name.)

Two signals are explicitly **not** progress: `last_event_at` is a display field also written by `mark_stalled_spawns` and `mark_errored` with no worker activity behind it (`live_worker_state.rs:1413`), and tmux `#{window_activity}` follows the spinner repaint (`stale_worker_sweep.rs:23-33`).

### The `WaitingForInput` state, and the Data case

`WaitingForInput` is set only from the driver's own `Notification` hook followed by a `Stop`, and only when the driver declared `Capability::AwaitingInputSignal` (`live_worker_state.rs:1202-1214`); it is never inferred from silence. Its durable mirror is `work_executions.status = 'waiting_human'` with one writer and one clearer (`awaiting_input_status.rs`). The notification _subtype_ is not modelled on the wire: `Notification { session_id, message }` is flat.

The project brief states that a worker parked there is exempt from every reclaim path. That is not quite what the code says. `stale_worker_sweep` skips it (`:828`, "handled by the completion and transient-recovery paths"), and so does the 2-hour auto-reap. But `terminal_work_sweep.rs` exists precisely for the `waiting_for_input` zombie whose work item is already done, `transient_recovery.rs:773` inspects `WaitingForInput` alongside `Idle`, and `dead_pid_sweep.rs:372` reaps it when its pid is dead. The accurate statement is: **a live `WaitingForInput` worker on a live work item is exempt from cadence-based wedge detection and its destructive follow-through; it is not exempt from terminal-work, transient-recovery, or dead-pid reclamation.** Since this design is probe-only, reclaim is out of scope either way; the finding is recorded so the deferred entry below is scoped correctly.

Data's specific pathology, `AskUserQuestion`, opens a mid-turn interactive prompt with no `Stop`, so none of the Stop-boundary machinery ever ran for it. That tool has been removed from every Claude worker spawn since mono#2889 (`driver/src/claude.rs:815`, pinned by a spawn-command test and the conformance goldens). It is not a permission-gated tool, so this flag is the only lever; the fix is at the root and the monitor does not need to re-solve it.

### Reviewability surfaces

`engine-audit.log` (`audit.rs`) is the best-retained forensic surface (about 1.3 MB covering months) and `audit::record_event(name, &payload)` (`:289`) adds a record type in one line. `coordinator_prompt_nudge` (`coordinator_tmux.rs:851-933`) already uses an `outcome` / `reason` / `error` vocabulary that a monitor record should copy. The incident-003 postmortem's warning applies: a diagnosis written to a known place that nothing reads is not a surface. The per-execution `dispatch-events` JSONL mirror (`dispatch_events` crate, `Stage` enum) is the better home for per-execution, higher-volume records. Nothing writes the tmux status line or pane title today.

## Does the `pr_review` machinery generalise? No — and that decides the size

The project brief asks this first because it decides whether the project is two PRs or four. The answer is that `pr_review`'s _verdict discipline_ generalises and its _execution shape_ does not.

What the monitor needs at firing time: the execution row, the run's transcript path, the brief, a handful of progress numbers, and a way to queue text to the pane. All of those are engine-local reads. What `pr_review` provides: a cube lease and checkout, a tmux pane, a review-pool slot, host selection, lease heartbeat, host-reconcile drain handling, orphan and lost-workspace sweep participation, and a structured-output artifact path. The monitor has no use for any of that, and the one thing it would need from that path, a lease-free dispatch, does not exist: every execution-dispatch path assumes a workspace (`coordinator/execution.rs:1064` onward). Adding a `monitor` `ExecutionKind` would mean either building a no-lease dispatch path or paying 1–10 s of cube setup plus a pane spawn per firing to read a file the engine is already tailing.

So the monitor runs as **`UtilityTask::HallMonitor`**, a one-shot call through the existing utility-model seam, driven by trigger evaluation inside existing sweeps and fan-out. This reuses: the transcript tail reader and redaction, utility-model selection and billing bucket, `claude_client` transport and retry, `NudgeBreaker` for re-fire suppression, the probe queue for the intervention, and `engine-audit.log` for the record. From `pr_review` it copies the discipline, not the plumbing: a pure prompt-render and verdict-parse crate (`engine/pr-review` is the template), a gate that lives in the engine with the model's own recommendation advisory only, and a verdict table written in the same transaction as the action.

The reservation, stated so it is checkable: if calibration shows the monitor needs _tools_ (to run `jj log` itself, read the diff, query GitHub), then the `pr_review` blueprint applies almost wholesale and the cheapest route is the existing review pool (`execution_targets_review_pool` becomes `matches!(kind, PrReview | Monitor)`, one line) rather than a fourth pool. The chosen design pre-empts the most likely tool needs with a deterministic context pack collected once by the engine (below), on the `conflict-diagnosis` precedent of "cheap mechanical facts, embedded verbatim, before spending a model call".

## Alternatives considered

### A. A `monitor` `ExecutionKind` on the review pool

Rejected for v1, as argued above: every dispatch path assumes a cube lease, the review pool is a hand-rolled third arm rather than a generic facility, and a firing would cost a lease plus a pane spawn to read a local file. The precedent that would seem to argue for it, `pr_review`, needs a checkout because it reads the diff and runs tools; the monitor's evidence is the transcript. The precedent that argues against it, `AnswerAgent`, is a non-mainline restricted-surface kind that runs with no pool of its own. Kept as the fallback if tool use proves necessary.

### B. Richer rules, no agent

Boss already has two narrow substring nets doing exactly this job for exactly one phrase family each: `build_wait.rs` ("still building, waiting" suppresses a nudge) and `heuristic_blocker_detection` (three literal phrases, default off, "the marker is the contract, this is a net under it"). Both are deliberately conservative because a false positive suppresses a nudge that should have fired. They stay; this design does not replace them. What they cannot do, and the incidents required, is two things. First, distinguish Troi's linker block from Troi's squash block: both emitted `[blocked]` with a plausible reason sentence, and the difference is whether the reason names an external fact or the worker's own unfinished action, which is a reading-comprehension judgement over the transcript. Second, produce feedback that names the specific thing ("your stated reason was that your own command had not finished"), which moved the workers where a canned string would not. A rule engine that could do both would be a worse agent, so the rejection is of rules for _this_ job, not of the existing nets.

### C. A scheduled transcript reviewer over all workers

Rejected. Cost scales with workers × time instead of with incidents, and the project brief's constraint is structural, not just economic: speaking to a worker mid-turn requires an interrupt, and routine interruption of a dozen workers is a plausible source of the very history corruption the monitor is meant to catch. Even a non-interrupting scheduled reader would spend tokens on healthy workers and, worse, would need its own "is this worth speaking about" threshold, which is the heuristic this design already has.

### D. Give the monitor stop or re-dispatch powers

Rejected for this project. Every observed case was fixable with a probe. A monitor that can terminalise a run must first establish that the worker is between turns (the incident-004 postmortem recorded 224 mid-turn reaps in nine days from a function that never checked), must corroborate durable liveness, and must be trusted to be right in the "stop" direction, where the existing 2-hour auto-reap already errs conservative. That is a separate trust step with its own design.

### E. Surface verdicts as attention items

Rejected by the project constraint and by the test in `dispatch-halt-state-vs-attention-items.md`: an attention item is for something a human must decide about. A monitor verdict is a record of something the engine already did, and "if every open instance vanished, would the board still show what is going on?" is answered yes. It also fails the lifecycle test: nothing later would lower it except a human.

### F. Interrupting delivery

Rejected. The default `bossctl probe` path is correct for a human who has decided to redirect a worker now. The monitor's probes ride the boundary queue that `PostToolUse` and `Stop` already drain; for a wait-loop that means delivery within one loop iteration, and for a parked worker it means delivery immediately via the out-of-band path. Nothing is abandoned mid-flight.

## Chosen approach

### 1. Progress signals and the spin heuristic

The heuristic samples, per `Working` run with `Rich` progress fidelity, inside the pass that already iterates every slot for `stale_worker_sweep` (every 60 s, one pass on boot). The sample is a pure read of `work_runs` plus one registry lookup and runs no subprocess:

```
(rounds, output_tokens, tool_calls, pr_head_after, staged_push_seen,
 semantic_tool_condition, semantic_progress_at, current_tool)
```

Two consecutive samples with equal `(rounds, output_tokens, pr_head_after, staged_push_seen)` and a growing `tool_calls` form one **unproductive interval**. `N` consecutive unproductive intervals with `semantic_tool_condition == Idle` at each sample is a `Spinning` classification, added as a sibling of `Stale` in the sweep's verdict enum. The `InFlight` short-circuit stays exactly where it is and fires first, so a long foreground build can never reach the spin arm; that is the acceptance criterion, satisfied by construction rather than by a threshold. Suggested `N` is 5 (five minutes at the sweep cadence); it is a constant next to `DEFAULT_STALE_THRESHOLD_SECS`, not a config knob, until observe-only data says otherwise.

The heuristic reuses `NudgeBreaker`'s fingerprint-and-debounce shape (`nudge_breaker.rs:155`) with a progress fingerprint instead of a state fingerprint, in a separate instance keyed by the sweep, so the monitor's counting never touches the nudge ladder's budget.

Two small additions close the tool-call gap, both in the hot path that already writes the semantic checkpoint:

- **A bounded per-run ring of `(timestamp, tool_name)`**, capped at 64 entries, populated from the `PreToolUse` arm of `dispatch_worker_event_fanout` next to `record_semantic_progress`, and forgotten on pane release the way `RunCostCapture::forget` is. In-memory only; after an engine restart the ring is empty and the heuristic simply re-baselines. This is what lets the context pack say "last 40 calls: `Monitor` × 38, `Bash` × 2".
- **`work_runs.tool_calls INTEGER`**, incremented on `PostToolUse` in the same `UPDATE` as `semantic_tool_condition`, and **`work_runs.semantic_tool_name TEXT`**, the name of the outstanding call written alongside `InFlight`. The first gives the spin ratio `Δtool_calls / Δoutput_tokens` durably; the second lets "outstanding `Bash` for 11 min" and "outstanding `Monitor` for 11 min" be told apart after a restart.

The heuristic does **not** run `jj diff`. The only existing workspace-diff primitive (`recovery_backup::capture_workspace_diff`) takes the jj working-copy lock and snapshots `@`, which is not safe at cadence against a live worker. A working-copy read is available to the _agent_ at firing time, once, using `--ignore-working-copy` (below).

### 2. The trigger set

Each trigger produces a `MonitorTrigger { execution_id, run_id, kind, evidence }` and is recorded as a new `Stage::HallMonitorTrigger` on the per-execution `dispatch-events` mirror before anything else happens, so the "when to look" half is auditable on its own, with the agent off.

**(a) The run declared itself blocked — exact state.** The trigger is "a `worker_blocked` or `worker_escalation` attention row with `status != 'resolved'` exists for a run that is still live (`activity ∈ {Idle, WaitingForInput, Working}`)". Both write sites, the marker parser's `file_worker_signal_attention` and the proposal seam's `apply_blocked`, converge on that row, so the trigger is correct with `worker_signal_proposals_seam` off or on. It is raised as a new bus event `WorkerSignalFiled { execution_id, attention_id }` published from both write sites (the `Event` enum at `event-bus/src/event.rs:7` currently has no worker-signal, hook, or attention topic; adding one is an enum arm, a producer, a subscriber, and a `TopicFilter::all` entry), with the sweep as the at-most-once backstop over open rows lacking a verdict. The evidence carried is the parsed `reason` string (already extracted by `extract_quoted`, `worker_escalation.rs:189`) and any `parse_warning`. A run whose `run_done_outcome` is `Blocked` has stopped for good and is not probeable; it gets a diagnostic-only verdict for the record.

**(b) Parked on input — exact state.** The trigger is `activity == WaitingForInput` for longer than a threshold (suggested 5 minutes, matching the scale of Data's 10 m 14 s) with the run's work item still active. It is evaluated in the same sweep pass, from the state the sweep already reads and currently skips; the `not_working_skipped` branch grows a `WaitingForInput` arm. The evidence is the `Notification` message text (the only subtype signal available on the wire) and the transcript tail. Because `AskUserQuestion` is gone from Claude spawns, what remains here on Claude is permission prompts and the driver's idle notifications; for other drivers it is whatever they declare with `AwaitingInputSignal`. A permission prompt needs a human by design, and typing text into it would be read as an answer, so for a notification whose message says it is a permission prompt the monitor records a verdict and does not speak. For any other `WaitingForInput`, the pane posture is `Parked` and a text probe is safe.

**(c) Spinning — heuristic.** As defined in §1. Evidence: the tool-name ring, the sample deltas, the outstanding-call name and age.

Triggers (a) and (b) are read from durable state and survive an engine restart. Trigger (c) needs `N` sweep passes after boot to re-baseline, which is the correct behaviour.

Not in the v1 trigger set, stated so the omission is visible: the second unproductive nudge at the same fingerprint in the no-PR ladder (the canned `PROBE_NO_PR` did not move the worker; judged text might), and the moment before a breaker trip parks a run. Both are cheap to add later at `nudge_or_park` because `probe_text` and `fingerprint` are separate arguments. They are left out of v1 to keep the trigger set to the three the brief named and to keep the monitor away from park semantics; see the open questions.

### 3. Firing: what the agent reads

One firing builds a **context pack** deterministically, then makes one model call. The pack is collected by the engine, embedded verbatim, and is the mechanism that keeps the agent tool-free:

- **The brief.** The composed prompt from `<workspace>/.claude/initial-prompt.txt` (written by `provision_workspace`, `driver/src/claude.rs:866`), falling back to the transcript's first `user` record. Included in full: the whole point of the monitor is a fresh reader holding the brief.
- **The transcript tail.** The last 48 KiB via the existing bounded reader (`transient_recovery::read_transcript_tail`, which the engine already uses at 256 KiB), normalised through `driver_transcript` so Codex and Claude dialects both work, passed through `live_status_redact` (`redact_text`, `truncate_large_values`, `should_drop_entry`) with tool results capped at 4 KiB each. Never the whole file.
- **The trigger and its evidence**, including the parsed `reason` for (a), the notification message for (b), and the tool ring plus deltas for (c).
- **Mechanical workspace facts**, collected once with read-only commands that do not take the working-copy lock: whether the workspace directory exists, `jj log -r '@ | @-' --ignore-working-copy` (change ids, descriptions, empty or not), `jj bookmark list --ignore-working-copy` for the expected bookmark, and whether a PR exists for the expected branch from `pr status`'s stored snapshot (no GitHub call). This is what let the operator say "your workspace has no `bin/` directory".
- **The rules the brief already states**, obtained by calling the same composer functions the brief was built from (`bazel_prepush_gate_text`, `worker_escalation_protocol_directive`, the CI-monitoring directive, `run_done_directive`; `runner/prompt.rs:814`, `:1085`, `:1184`), so judge and judged never drift.

The model is asked for a **forced structured verdict**, not prose, on the `notification-dedup-scoring.md` pattern: a single forced tool call whose schema is the verdict, deserialisation failure is a decision failure that fails safe to `no_action`, never parse-and-hope.

```
verdict:     leave_alone | speak | no_action
diagnosis:   legitimate_block | self_inflicted_block | wait_loop | parked_answerable | permission_prompt | unclear
confidence:  high | medium | low
reasoning:   string (bounded; recorded, never sent)
message:     string | null (the specific corrective paragraph; sent only when verdict == speak)
```

The engine, not the model, applies the gate: `speak` is honoured only with `confidence == high`, only when the trigger is still current at send time (the attention still open, the activity still `WaitingForInput`, the spin fingerprint unchanged), only when the run is not held (`hold_registry`), and only within the per-execution bounds in §5. Everything else degrades to `no_action` with the model's verdict still recorded. The asymmetry is deliberate and is the design's answer to Troi's linker block: the cost of a false "speak" is a worker told to keep going past a real obstacle, the cost of a false "leave alone" is that today's behaviour continues.

### 4. Speaking: the verdict-to-probe path

A `speak` verdict composes the probe text as the model's `message` followed by a fixed engine-authored footer that names the sanctioned terminal moves (`NO_CHANGES_NEEDED`, `boss propose done`, `boss propose blocked` with a reason that names an external fact), and prefixes it `[hall-monitor]` so the worker and any transcript reader can tell it from a coordinator probe. The footer is pinned by the same style of test as `probe_texts_name_the_no_op_marker`.

Delivery uses the existing queue and nothing else:

- Trigger (a), lazy block: `queue_probe_resolving_worker_signal` then `deliver_queued_probes_now` (the run is parked and will emit no hook). Delivery resolves the `worker_blocked` row and resumes the nudge ladder, which is precisely what the operator's manual probe did. A legitimate block gets no probe, so the attention and the pause stay.
- Trigger (b): `queue_probe` then `deliver_queued_probes_now`; posture is `Parked`.
- Trigger (c): `queue_probe` only. The next `PostToolUse` drains it mid-turn (Claude declares it buffers typed input), which for a wait-loop is seconds away, or the next `Stop` does. No interrupt is ever issued.

The reply comes back for free: `dispatch_probe_reply_on_stop` (`app/worker_events.rs:2318`) already reads the transcript from the dispatch offset at the next boundary and emits `ProbeReplied`; the monitor stores that reply on its verdict row as the outcome evidence. An undeliverable probe follows the existing `probe_undelivered` path and is recorded as such on the row; the monitor does not retry on its own.

### 5. Bounds

Stated at the level of the property, not the mechanism:

- **Text only, boundary only.** The monitor's output is a string handed to `ProbeQueuer`. It never calls the interrupt path, never sends a named key, never writes to a pane in `Refused` posture, and never answers a permission prompt. The transport it uses has no other capability, which is the enforcement.
- **No state change except through delivery.** The only durable effect on the worker's run is the one every human probe already has: a delivered probe resolves the run's worker-signal attentions. It does not touch `autostart`, status, priority, holds, slots, leases, or the workspace.
- **Bounded per execution.** At most one `speak` per `(execution, trigger kind, fingerprint)`, at most two `speak`s per execution overall, and never within 10 minutes of a previous monitor probe to the same run. Implemented on a `NudgeBreaker` instance namespaced to the monitor; an undelivered probe reverts its count (`revert_undelivered`, `nudge_breaker.rs:210`) as the ladder's probes do. After the cap the monitor still judges and records but does not speak; a worker the monitor could not move twice is the coordinator's.
- **Never on evidence it does not have.** No firing for `InFlight` (by construction), for fidelity below `Rich`, for held runs, for remote runs without a local transcript, or when the transcript path is unknown.
- **Fail safe on every failure.** No API key, model error, timeout, malformed verdict, or stale trigger all resolve to `no_action` with the failure class recorded (a typed outcome enum on the `SummarizerOutcome` pattern). No flag combination can make a failure speak.

### 6. Reviewability

Every firing writes one row to a new `hall_monitor_verdicts` table, modelled on `pr_review_verdicts` and written in the same transaction as the probe enqueue so a probe cannot exist without its record:

```
id, execution_id, run_id, work_item_id, trigger_kind, trigger_fingerprint,
evidence_json, model, input_tokens, output_tokens, verdict, diagnosis,
confidence, reasoning, message_sent, gate_outcome, probe_id, probe_state,
worker_reply, created_at
```

`gate_outcome` is a closed vocabulary that distinguishes "model said speak and we spoke" from "model said speak and the gate refused (low confidence / trigger stale / capped / held)" from "model failed" from "observe-only": the `review_verdicts.rs` lesson that `completed` alone made a clean pass indistinguishable from a destroyed one.

The same firing appends one `engine-audit.log` line via `record_event("hall_monitor_verdict", …)` carrying the row's id, trigger, verdict, gate outcome, and the message, on the `coordinator_prompt_nudge` field vocabulary, and one `Stage::HallMonitorVerdict` on the per-execution `dispatch-events` mirror.

The read surface ships with the writer, so this does not repeat "durably recorded and nothing reads it": `bossctl monitor list [--execution <id>] [--since]` and `bossctl monitor show <verdict-id>`, plus the row surfaced under the existing `bossctl agents transcript`-adjacent view of a run. `input_tokens` and `output_tokens` come from `MessagesResponse::usage()` on the row itself; that is the v1 cost accounting for engine-owned inference, sufficient because firings are event-bounded and capped per execution. A general accounting seam for utility-model calls is a real gap the research surfaced, but this project does not need it and does not propose it.

### 7. Cost per firing

Input is bounded by construction: the brief (typically 8–15k tokens), a 48 KiB redacted tail (about 12k tokens), the context pack (under 2k), and the rule citations (about 1k). Output is capped at 1,024 tokens. The default model is `claude-sonnet-4-6` (the same tier `PaneSummary` uses), overridable per the seam's convention via `BOSS_UTILITY_MODEL_HALL_MONITOR`, with a dedicated billing bucket `BOSS_HALL_MONITOR_API_KEY` falling back to the shared key. The judgement is the hard part of this design and the Troi calibration is the argument against Haiku here; the observe-only phase is where that choice gets measured (see the questions manifest). Firings per execution are capped at the speak bound plus re-judgements when a trigger changes shape, so per-execution cost is bounded by a small constant times one call.

### 8. Flags and rollout

Two flags in the existing registry, both `default_enabled: false`, category `monitor`, on the parent-plus-sub-flag shape of `notification_dedup`:

- `hall_monitor` — trigger evaluation, the spin classification arm, the context pack, the model call, the verdict row, the audit line. With only this on, the monitor **observes**: every verdict is recorded with `gate_outcome = observe_only` and nothing is sent.
- `hall_monitor_speak` — honours `speak` verdicts through the gate in §3.

The monitor is not mentioned in the worker brief in v1, so there is no second half to keep in step with the flag (the `worker_signal_proposals_seam` both-halves rule does not apply yet). If a later change adds "a hall monitor may probe you" to the brief, it must read the same flag.

### 9. Calibration and regression

Calibration is a **validation** study of the chosen approach, not a comparison between approaches, and it is stated as such: its output is a false-speak rate and a false-leave-alone rate against real firings, which is what decides whether `hall_monitor_speak` flips.

- **The four incidents are the fixture set.** Worf (wait-loop) → `speak`/`wait_loop`; Data → historically `parked_answerable`, now unreachable on Claude; Troi (squash) → `speak`/`self_inflicted_block`; Troi (linker) → `leave_alone`/`legitimate_block`. If those transcripts still resolve on disk (Claude Code keeps them about 28 days; the engine records the path in `work_runs.transcript_path`), they should be captured as redacted fixtures during PR 2. If they have aged out, the observe-only phase builds the corpus from live firings.
- **The gate is tested in code, without a model.** The verdict parser, the confidence asymmetry, the trigger-still-current check, the cap, and the hold check are pure functions with unit tests, on the `comment-classifier` pattern.
- **The judgement is measured end to end, not reproduced.** A hand-built replay of the engine's trigger path would be built from the same beliefs as the code and could not find the integration bugs (a probe delivered to the wrong posture, a fingerprint that never matches). Observe-only mode on the real engine, against real workers, is the study; its rows are the evidence.
- **The long-build acceptance criterion is a test on the classifier.** A run with `InFlight` and a 40-minute-old checkpoint must classify `Healthy`, never `Spinning`, regardless of the sample history. It sits next to the existing `classify_semantic_staleness` tests.

## Risks / open questions

- **The Troi linker case is the whole risk.** The design's answer is the asymmetric gate (speak only on high confidence), the fail-safe default, the observe-only phase, and the reasoning field on every row. If observe-only shows the model calling a legitimate block "self-inflicted" at high confidence more than rarely, the fix is prompt and model tier, and `hall_monitor_speak` stays off until it is fixed. A reviewer should decide what "rarely" means before implementation starts; the doc proposes zero false `speak` verdicts on legitimate blocks across the observe-only window as the bar.
- **Probe delivery resolves the block attention.** Reusing the human-probe ack semantics means a wrong "speak" on a genuine block also resumes the nudge ladder against a worker that cannot make progress. The bound is the breaker (three unproductive nudges to a park), so the blast radius is one park rather than an unbounded loop, but it is the largest consequence a wrong verdict can have and is raised in the questions manifest.
- **`WaitingForInput` coverage is narrower than the brief assumed.** With `AskUserQuestion` gone, the Claude-side cases are permission prompts (human by design) and idle notifications. The trigger still earns its place for non-Claude drivers and as the exact-state hook the reclaim question needs, but a reviewer may reasonably ask whether (b) should ship in v1 at all. The doc keeps it because it is cheap (one arm in a branch that already exists) and because the verdict rows are how we learn what actually parks there.
- **Second-nudge judgement is left out.** The highest-value place the research found for judged text is the second unproductive canned nudge in the no-PR ladder, before the breaker parks. It is deliberately not in v1 (park semantics, trigger-set discipline). If the operator wants it, it is a fourth trigger at `nudge_or_park` with `probe_text` supplied by a verdict, and belongs in a follow-up rather than a widening of PR 2.
- **The brief is large and is included in full.** Cost per firing is dominated by it. Prompt caching across firings for the same run would cut that but is an optimisation with no correctness impact; not proposed for v1.
- **Remote workers are uncovered.** Stated as a non-goal. The `remote_transcript.rs` collection path exists for teardown, not live tailing; extending it is a separate piece of work the monitor would then inherit for free.
- **Codex notification subtypes.** The wire `Notification` is flat; Codex-specific unobserved-command notifications are staged separately. The context pack includes the raw message text, which is enough for the model to tell a permission prompt from an idle notification on Claude; other drivers may need the subtype modelled. Not a v1 blocker.
- **Verdict rows outlive their transcripts.** Transcripts expire in about a month; the row keeps the reasoning and the message but not the tail. That is the same retention shape as `pr_review_verdicts` and is acceptable.

## Proposed implementation task breakdown

Breakdown size: 3 entries (2 in-scope, 1 deferred) — the change is two real seams in one subsystem (the engine): a read-only signal-and-trigger layer that is reviewable and measurable on its own with the agent absent, and the agent-plus-verdict-plus-probe layer that is spawned by it; the `pr_review` finding removes the pool and execution-kind plumbing that would otherwise have been two more entries, and the `WaitingForInput` reclaim question the project raised is recorded as a deferred entry because it is an effector this design declines.

### Progress signals and the hall-monitor trigger set

Scope: add the tool-call ring (in-memory, bounded, populated in `dispatch_worker_event_fanout`, forgotten on pane release), the `work_runs.tool_calls` and `work_runs.semantic_tool_name` columns written in the same hot-path `UPDATE` as the semantic checkpoint, and the `Spinning` classification arm alongside `Stale` in the stale-worker sweep pass with its progress-fingerprint breaker instance; add the `WaitingForInput` threshold arm in the same pass; publish `WorkerSignalFiled` from both `worker_blocked`/`worker_escalation` write sites with the sweep as backstop; introduce the `hall_monitor` feature flag and the `MonitorTrigger` type; record every trigger as `Stage::HallMonitorTrigger` on the per-execution dispatch-events mirror and as a counter family in the metrics registry. No model call, no probe. Tests: the long-build acceptance test on the classifier (`InFlight` at any age is never `Spinning`), the spin fingerprint over synthetic sample sequences, the trigger-(a) path with the proposals seam both off and on, and restart re-baselining.

Effort: medium

Dependencies: none

Scope: in-scope

### Hall-monitor agent and verdict-to-probe path

Scope: add `UtilityTask::HallMonitor` (slug, model env, billing bucket, default `claude-sonnet-4-6`); a new leaf crate `engine/hall-monitor` holding the context-pack type, the prompt renderer (calling the existing prompt-composer rule functions), the forced-structured-output verdict schema and its parser, and the engine-side gate (confidence asymmetry, trigger-still-current, cap, hold), all pure and unit-tested without a network; the engine glue that subscribes to `MonitorTrigger`s, collects the context pack (brief file with transcript-head fallback, 48 KiB redacted tail via the existing reader and redactor, read-only `jj … --ignore-working-copy` facts, stored PR snapshot), makes the call, and on `speak` enqueues through `queue_probe` / `queue_probe_resolving_worker_signal` / `deliver_queued_probes_now` per trigger kind with the `[hall-monitor]` prefix and the pinned footer; the `hall_monitor_verdicts` table written in the same transaction as the enqueue, populated with the probe id, state, and the reply captured by the existing `ProbeReplied` path; the `hall_monitor_verdict` audit record and `Stage::HallMonitorVerdict`; the `hall_monitor_speak` sub-flag; and the `bossctl monitor list|show` read verbs. Capture the four incident transcripts as redacted fixtures if they still resolve. This entry is one PR because the verdict table, the probe enqueue, and the read verb have no exercised caller without the agent, and the agent has no reviewable output without them.

Effort: large

Dependencies: Progress signals and the hall-monitor trigger set

Scope: in-scope

### Reclaim path for a worker parked in `WaitingForInput` on a live work item

Scope: decide, from the `hall_monitor_verdicts` rows for trigger (b), whether a live `WaitingForInput` worker on a live work item needs a cadence-based reclaim (today it is skipped by `stale_worker_sweep` and the 2-hour auto-reap, but is already handled by terminal-work, transient-recovery, and dead-pid reclamation), and if so add it as a non-destructive-then-destructive ladder on the stale sweep's pattern with a re-verify before acting.

Effort: small

Dependencies: Hall-monitor agent and verdict-to-probe path

Scope: deferred (future / not a v1 blocker) — this is a stop effector, which this project's non-goals exclude; the monitor's verdict rows are the evidence needed to decide whether it is warranted, and `AskUserQuestion`'s removal may have made it moot on the Claude driver.

Parallelism: the two in-scope entries are strictly serial; the second is spawned by the first and edits the same fan-out and sweep files. The deferred entry does not run until the in-scope work has produced data.
