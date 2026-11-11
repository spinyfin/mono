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
2. If no turn starts within `SUBMIT_CONFIRM_TIMEOUT` of a Return, the engine presses Return in the pane (`tmux send-keys C-m`) and waits again, at most `SUBMIT_RETRIES` times.
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

## What this does not cover

- A worker adopted after an engine restart is not re-confirmed. Confirmation is the spawning engine's wait. A worker whose spawn the engine did not finish confirming is covered only by the driver-start sweep, which a `SessionStart` satisfies.
- Claude Code attaches an image, and drops the line, when a pasted line names an image file that exists. A brief that quotes a real image path on its own line is therefore not delivered byte for byte. The engine does not rewrite briefs to avoid this.
