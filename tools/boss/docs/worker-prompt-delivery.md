# Worker prompt delivery: how the brief reaches the driver, and what proves it was accepted

A worker's first prompt (its brief) is pasted into the driver's interactive composer and submitted with Return. This document covers that path end to end: who performs each step, what evidence each step waits for, where each step is logged, and what happens when a step does not complete.

## The failure this path is built around

On 2026-11-10 three `pr_review` workers sat idle for 15 to 30 minutes. Each pane showed the Claude Code banner and `❯ [Pasted text #1 +N lines]` in the composer. Pressing Return by hand started each one.

Two things were wrong, and either one alone would have been survivable.

**Return was sent on a timer.** The launcher wrote the bracketed paste and sent Return 100 to 150 ms later. Claude Code does not always commit a paste to its composer straight away. When any line of the paste ends in an image extension (confirmed for `.png` and `.jpg`), it treats the paste as a possible image drop: it holds the paste, runs `osascript -e 'get POSIX path of (the clipboard as «class furl»)'` to ask the macOS clipboard for a file, and commits the text only afterwards. Until then the composer is empty and shows `Pasting…`. Return arriving in that window lands on an empty composer and is discarded. The paste then commits and sits there.

How long that hold lasts is outside Boss's control. It measured 84 ms to 3.3 s on a host at its normal load, and up to 46 s under heavy load. So the timer was a race: when the hold ended inside 100 ms the brief was submitted, and when it did not the worker sat idle. The same host gave both outcomes 45 minutes apart.

A PR diff triggers this easily: `diff --git a/docs/shot.png b/docs/shot.png` ends in an image extension. All three stuck prompts contained such a line. An image extension in the middle of a line does not trigger it. Ordinary briefs were never affected, at any host load, because Claude Code handles an ordinary paste and the Return after it in order.

**`SessionStart` was read as "the worker started".** Spawn confirmation waited for the first driver hook. `SessionStart` fires when the CLI comes up, before any prompt, so confirmation passed and the worker was counted as live.

## The path

| Step               | Performed by                               | Waits for                                                                                          |
| ------------------ | ------------------------------------------ | -------------------------------------------------------------------------------------------------- |
| Pane spawn         | engine (`PaneSpawnRunner`)                 | tmux session created, pane pid reported                                                            |
| Composer readiness | launcher (`initial_prompt.py`)             | raw terminal, bracketed paste enabled, driver chrome visible, no startup dialog, stable for 200 ms |
| Paste              | launcher                                   | every byte of the bracketed paste written to the driver                                            |
| Landing            | launcher                                   | the composer **showing** the pasted brief                                                          |
| Submit             | launcher                                   | Return written to the driver; the `initial-prompt.delivered` marker is created                     |
| Driver start       | engine (spawn confirmation, waits 1 and 2) | driver chrome in the pane, then any driver hook or a `transcript_path`                             |
| Prompt accepted    | engine (spawn confirmation, wait 3)        | a turn event for this run: `UserPromptSubmit`, or any tool, stop or model-progress event           |

The launcher is a relay that owns the driver's terminal. It delivers the brief without the engine's help, so an engine restart can neither interrupt nor repeat a delivery.

### Landing: Return waits for the composer, not for a timer

After the last paste byte is written, the launcher watches the driver's screen and sends Return only when one of these appears:

- **`marker`**: text the driver declares its composer shows once it holds a pasted brief ([`AgentDriver::paste_landed_markers`](../engine/driver/src/lib.rs)). Claude declares `[Pasted text #`.
- **`literal`**: the opening text of the brief itself, which is how a composer shows a short paste.
- **`repaint`**: any output from the driver after the paste was fully written. This applies only to a driver that declares no marker, which is a statement that it handles input in order.

A driver that can commit a paste late **must** declare a marker. Without one, its first unrelated output releases Return.

If no evidence appears within `LANDING_TIMEOUT`, the launcher sends Return once anyway and records `paste_unverified`. That is a degraded mode, logged as a warning, and the engine's third wait decides whether it worked. It exists so that a driver update that rewords its placeholder slows spawns down instead of stopping them.

### Prompt accepted: a turn, not a session

[`LiveWorkerStateRegistry::record_turn_evidence`](../engine/live-worker-state/src/prompt_acceptance.rs) records the first event that only a running turn can produce. `SessionStart`, `SessionEnd` and `Notification` do not count. A `Notification` is the opposite of evidence: Claude raises one because the composer has sat idle.

Spawn confirmation's third wait ([`confirm_prompt_submitted`](../engine/execution-runner/src/runner/spawn_confirmation.rs)) requires that evidence:

1. It waits for the launcher to report that it sent Return (the `initial-prompt.delivered` marker). Until then the engine presses nothing: Return would land in whatever the driver is showing, which may be a startup dialog.
2. If no turn starts within `SUBMIT_CONFIRM_TIMEOUT` of a Return, the engine presses Return in the pane (`tmux send-keys Enter`) and waits again, at most `SUBMIT_RETRIES` times.
3. If there is still no turn, the spawn fails with `PromptNotSubmitted`, the pane is reaped, and the coordinator raises `pane_spawn_failed`.

The retry is Return and nothing else. The brief is pasted exactly once, by the launcher, so a retry cannot deliver a second copy, and Return on an empty composer does nothing.

A launcher that reports failure (`initial-prompt.undelivered`), or reports nothing within `DELIVERY_REPORT_TIMEOUT`, fails the spawn with `PromptNotDelivered` without any Return being pressed.

When this wait fails, the driver has already signalled, so its `SessionStart` must not veto the reap the way a driver signal vetoes a never-started reap. Only a turn that starts before the reap commits does (`commit_unsubmitted_reap`).

**Remote (SSH) workers** have no pane the engine can type into. Their launcher's own Return is the only submit. The engine still requires turn evidence, and reaps and fails a remote worker that produces none within the launcher's whole budget plus one confirm window.

## Deadlines and the measurements behind them

Measured on a 12-core macOS host against Claude Code 2.1.322 with 47 KB, 500-line briefs, in both `--dangerously-skip-permissions` and `--permission-mode auto`: 101 runs of the launcher (71 at normal load, 30 at heavy load) plus 92 and 34 hook timings. "Normal load" is the host's ordinary state while other workers build. "Heavy load" adds 250 busy-loop processes, which held the 1-minute load average between 117 and 437.

| Interval                                       | Normal load    | Heavy load     |
| ---------------------------------------------- | -------------- | -------------- |
| Pane spawn to composer ready                   | 0.5 to 1.9 s   | 8.6 to 18.6 s  |
| Writing the paste                              | 16 ms to 1.3 s | 1.9 to 12.5 s  |
| Paste written to composer showing it, ordinary | 6 to 22 ms     | 0.4 to 2.9 s   |
| Paste written to composer showing it, deferred | 84 ms to 3.3 s | 1.5 to 46.0 s  |
| Return written to `UserPromptSubmit` hook      | 37 to 198 ms   | 0.13 to 4.0 s  |
| Pane spawn to Return written                   | 0.6 to 4.9 s   | 13.9 to 63.8 s |

| Deadline                  | Value                          | Measured peak | Margin | When it fires                                                   |
| ------------------------- | ------------------------------ | ------------- | ------ | --------------------------------------------------------------- |
| `LANDING_TIMEOUT`         | 180 s                          | 46.0 s        | 3.9x   | Return is sent once without evidence; the engine's wait decides |
| `SUBMIT_CONFIRM_TIMEOUT`  | 30 s                           | 4.0 s         | 7.5x   | the engine presses Return again, up to `SUBMIT_RETRIES` (2)     |
| `DELIVERY_REPORT_TIMEOUT` | 300 s (launcher budget + 30 s) | 63.8 s        | 4.7x   | the spawn fails with `PromptNotDelivered`                       |

The launcher budget is three 30 s stages (readiness, the driver reading the paste, the driver reading Return) plus `LANDING_TIMEOUT`. `DELIVERY_REPORT_TIMEOUT` sits above it so the engine never fails a spawn the launcher is still working on. A file-tailing driver (Codex) reports its turn through a rollout the engine must first discover, so its confirm window is the discovery deadline instead of `SUBMIT_CONFIRM_TIMEOUT`.

The launcher's 30 s stage bound is not new. It used to cover readiness, the paste and Return together, and now covers each on its own, so no stage is tighter than before.

None of these deadlines is what makes the common case work. A healthy spawn passes on evidence: 0.6 to 5 s after pane spawn at normal load.

## Reading the logs

The launcher cannot log to the pane, which is the driver's terminal. It appends one JSON object per step to `initial-prompt.delivery.jsonl` beside the brief in the run's private home, and the engine reports those steps when confirmation ends. For one `run_id`, in order:

| Log message                                                    | Fields                                 | Meaning                                        |
| -------------------------------------------------------------- | -------------------------------------- | ---------------------------------------------- |
| `composer readiness wait started`                              | `started_at`                           | confirmation began                             |
| `driver start confirmed; awaiting prompt submission`           | `elapsed_ms`, `first_hook_event`       | waits 1 and 2 passed                           |
| `prompt submit sent by the launcher; awaiting turn evidence`   | `elapsed_ms`                           | the engine saw the delivered marker            |
| `prompt accepted: first turn evidence received for this run`   | `evidence`                             | the registry recorded the turn                 |
| `prompt submit confirmed: the driver started a turn`           | `retries`, `elapsed_ms`                | wait 3 passed                                  |
| `prompt submit retried: ... pressed Return again`              | `attempt`, `of`, `elapsed_ms`, `error` | no turn within the confirm window              |
| `prompt submit failed: ...`                                    | `retries`, `elapsed_ms`                | the spawn is about to fail                     |
| `prompt delivery: composer ready`                              | `detail.wait_ms`                       | launcher step `ready`: how long readiness took |
| `prompt delivery: paste sent`                                  | `detail.bytes`, `detail.write_ms`      | launcher step `paste_sent`                     |
| `prompt delivery: composer showed the pasted brief`            | `detail.evidence`, `detail.wait_ms`    | launcher step `paste_landed`                   |
| `prompt delivery: composer never showed the pasted brief; ...` | `detail.wait_ms`                       | launcher step `paste_unverified` (warning)     |
| `prompt delivery: submit sent`                                 |                                        | launcher step `submit_sent`                    |
| `prompt delivery: launcher failed`                             | `detail.reason`                        | launcher step `failed` (warning)               |

Each `prompt delivery:` line carries `at_ms`, the launcher's own wall-clock time for the step, because the engine reports the steps after the fact.

A `paste_landed` whose `wait_ms` is far above the usual tens of milliseconds is a deferred paste: the brief contained a line ending in an image extension. That is expected and harmless now.

A `paste_unverified` on every spawn of one driver means its composer no longer shows the declared marker. Record the new text from the installed CLI and update that driver's `paste_landed_markers`.

A driver start that arrives only after the start wait expired rescues the spawn from that failure and nothing more: the prompt wait still runs, with its Return retries, and fails the spawn if no turn begins.

## Engine restart and spoke workers

New local launches durably mark prompt confirmation pending before creating the pane, and complete that marker after confirmation. The adoption sweep resumes the same bounded delivery and Return-only confirmation for pending launches. Persisted turn evidence also satisfies recovery if the engine stopped before writing success. A SessionStart alone does not. Legacy rows without the marker are exempt from prompt re-confirmation.

Spoke supervisors budget the launcher's entire delivery window plus 30 seconds. The hub waits for current-run turn evidence after the host reports a live pane; a worker that never accepts its prompt is stopped and spawn fails. The spoke provider remains inactive until installed by the engine and enabled for a host.

Cancellation is checked on every prompt-confirmation poll and immediately before a Return retry, including recovery after restart.

## Text typed into a running worker's pane: probes and nudges

After spawn, the engine reaches a worker by typing into its pane: a coordinator probe, a description-change notice, or the engine's own "you stopped without a PR" nudge. A worker sitting at its prompt gets the text typed and submitted with Return (`tmux send-keys`, then `send-keys Enter`). This path has the same hazard as the brief: tmux reporting success for the keys says they were accepted, not that the driver took them as a prompt.

### The failure this is built around

On 2026-11-11 a Codex worker that had declared a wait, outlived it, and sat parked at its prompt was failed with "auto-nudge circuit breaker stopped the attempt after 3 nudges: no new commit, PR, or state change". It had been shown none of them. The first nudge was typed into its pane and recorded `consumed` because the write succeeded; the rollout shows no prompt and no turn after it. That probe then held the run's single delivery slot, which only a turn boundary releases, so the second and third nudges never left the queue. All three were counted when they were queued.

Two causes make a write into a parked pane start no turn while every tmux command succeeds. Neither can be told from the other after the pane is gone, which is why the engine now records what the pane showed.

- **The Return is taken as part of a paste.** Codex's TUI treats keys that arrive in a burst as a paste, and a Return landing right behind them as a newline in it. The text stays in the composer and nothing is submitted. Measured on 0.153.4 and 0.160.1 ([qualification notes](investigations/codex-0.160.1-qualification-2026-10-06.md), "Harness notes"): Return sent immediately after `send-keys -l` left the prompt unsubmitted, and a 1 s gap submitted reliably. Whether it happens depends on how long the Return's own `tmux` process takes to start, so it comes and goes with host load. That is consistent with the same code path having delivered nudges to the same worker 90 minutes earlier.
- **The pane is in a tmux mode.** A pane someone has scrolled is in copy-mode, and `send-keys` then hands every key to the mode's key table instead of the driver. Reproduced on tmux 3.8: typed text is run as copy-mode commands, `Enter` is swallowed and leaves the pane in the mode, a bracketed paste lands in the composer with its Return swallowed, and `Escape` only leaves the mode. Codex sessions run with `mouse on` ([`codex-tmux.conf`](../engine/driver/src/codex-tmux.conf)), so one wheel tick puts a pane there.

### What the engine does

| Step                | Where                                                                           | Effect                                                                                                    |
| ------------------- | ------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| Leave any tmux mode | [`leave_pane_modes_before_keys`](../engine/app/src/app/pane_delivery.rs)        | `copy-mode -q` when `#{pane_in_mode}` is 1, before any key. Costs a viewer their scroll position.         |
| Submit settle       | [`AgentDriver::pane_submit_settle`](../engine/driver/src/lib.rs)                | Wait between the text and Return. Codex declares 1 s; drivers that read input in order declare none.      |
| Hold at `injected`  | [`deliver_probe_via_pane_write`](../engine/app/src/app/worker_events.rs)        | A parked write is not `consumed` until the driver starts a turn.                                          |
| Confirm the turn    | [`confirm_parked_write_turn_started`](../engine/app/src/app/parked_delivery.rs) | Matching prompt or transcript text moves the probe to `consumed`.                                         |
| Press Return again  | [`parked_delivery`](../engine/app/src/app/parked_delivery.rs)                   | After `TURN_START_CONFIRM_TIMEOUT` with no turn, Return and nothing else, at most `RETURN_RETRIES` times. |
| Settle as failed    | same                                                                            | `unconfirmed`, delivery slot released, attention item filed with the pane's last lines.                   |

The first two steps prevent the two known causes. The rest is what makes delivery not depend on the list of causes being complete.

A main-session `UserPromptSubmit` confirms delivery only when its whitespace-normalised prompt contains the probe text. Empty prompts and tool-only events require the probe text in the transcript after the dispatch snapshot. Unrelated turns and subagent events do not confirm delivery. A pane that reads `Working` without that evidence is left alone until it returns to its prompt: Return pressed into a busy pane can answer a dialog nobody meant to answer. The text is typed exactly once, so a retry cannot deliver a second copy.

A mid-turn injection that finds the worker parked also starts this watch. An unconfirmed parked write retains its delivery claim, including against later interrupting probes. If its failure withheld a Stop completion decision, the engine re-runs that decision after releasing the claim.

A write made after the engine interrupted a busy worker is reported `consumed` to the caller as before, and watched the same way. If no turn follows it is taken back to `unconfirmed`.

| Deadline                     | Value            | When it fires                                                         |
| ---------------------------- | ---------------- | --------------------------------------------------------------------- |
| `TURN_START_CONFIRM_TIMEOUT` | 30 s             | Return is pressed again                                               |
| `RETURN_RETRIES`             | 2                | after the last window the delivery is settled `unconfirmed` (at 90 s) |
| `NUDGE_DELIVERY_DEADLINE`    | 5 min from issue | a nudge still unwritten is withdrawn when its worker is idle          |

The 30 s is `SUBMIT_CONFIRM_TIMEOUT`'s measurement applied to the same interval: Return to `UserPromptSubmit` peaked at 4.0 s under heavy load. A turn that starts is seen in well under a second.

### What the nudge breaker counts

A nudge counts against the worker only once the worker was given it ([`nudge_breaker`](../engine/completion/src/nudge_breaker.rs)). Before every nudge decision the ladder reads what became of the previous nudge:

- **Not shown yet** (queued, or written and waiting for a turn): no nudge is issued and none is counted. The recurring sweep asks for delivery again, because an idle worker emits no event to carry a queued probe.
- **Never arrived**: the nudge comes back off the count. Consecutive failed deliveries are counted separately, and at the same bound (`max_unproductive_nudges`) the attempt fails with "auto-nudge delivery failed N times in a row … This is a delivery failure, not an unresponsive worker". A nudge the engine discarded before writing it, or one meant for a busy worker that could not be interrupted, comes off the count without being a failed delivery.
- **Not shown yet for too long**: a nudge that is still unwritten after `NUDGE_DELIVERY_DEADLINE` from issue, including time spent busy, when the worker is idle is not on its way; every attempt to write it is being refused or is failing. It is withdrawn, so it cannot arrive late on top of its replacement, and counted as a failed delivery. This is what keeps an unwritable pane from holding the ladder forever. A nudge that has been written is not withdrawn: its own delivery settles it within the 90 s above.
- **Arrived**: the breaker judges the worker on it. A worker that receives its nudges and changes nothing is still stopped at the cap, with the reason it has always had.

A turn boundary that lands while the engine is itself interrupting the worker to deliver a probe is not the worker stopping, and does not run the nudge decision.

### Reading the logs

| Log message                                                                             | Meaning                                                                              |
| --------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| `pane was in a tmux mode (scrolled into copy-mode); left it so the keys reach …`        | the pane had been scrolled; before this fix the write would have gone to tmux        |
| `probe delivery confirmed: the driver started a turn after the write into its parked …` | normal delivery                                                                      |
| `probe written into a parked pane started no turn within the confirm window; pressed …` | a retry (warning). Fields `pane` and `pane_tail` are the pane at that moment         |
| `probe delivery failed: the text was written into a parked pane and the driver never …` | the delivery was settled `unconfirmed` (error), with the same fields                 |
| `auto-nudge: held — the previous nudge has not reached the worker yet …`                | the ladder did not advance and did not count                                         |
| `auto-nudge: the previous nudge was not delivered; it is not counted against …`         | a nudge was handed back; `consecutive_delivery_failures` is the run toward the bound |

`pane` reads `pane_in_mode=…, foreground=…, activity=…`. `pane_tail` is the last 20 non-blank lines of the pane, where a composer is drawn: text still sitting there after a failed delivery is the swallowed-Return case.

## What this does not cover

- An interrupt keystroke (`Escape`) sent to a pane that is in a tmux mode leaves the mode instead of reaching the driver. The interrupt path does not leave the mode first, so its first press is spent on tmux.

- Claude Code attaches an image, and drops the line, when a pasted line names an image file that exists. A brief that quotes a real image path on its own line is therefore not delivered byte for byte. The engine does not rewrite briefs to avoid this.
