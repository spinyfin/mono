# Incident 007 — The initial-prompt feeder left workers running with no prompt

- **Date:** 2026-09-27
- **Severity:** High. Half of all worker spawns in a 27-minute window started with no prompt, and dispatch was paused.
- **Status:** Reverted by [mono#3030](https://github.com/spinyfin/mono/pull/3030). The replacement, [mono#3034](https://github.com/spinyfin/mono/pull/3034), confirms that a turn actually started and was merged 2026-09-30 23:53 UTC.

Times are UTC, with US Central Daylight Time (CDT, UTC−5) in parentheses. Evidence comes from two places: GitHub (PRs, merge commits, `boss-v1.0.*` releases) and the engine's own diagnostics, read through `bossctl logs` (sources `engine`, `audit`, `spawn`). Anything inferred rather than read from a record is marked **(inference)**.

Related: the release that carried this incident's revert (`boss-v1.0.684`) also shipped mono#3028, which caused the separate [incident 008](incident-008-codex-review-guide-spawns-refused-by-hook-trust-gate.md).

## 1. Summary

Large initial prompts could exceed `ARG_MAX`. To avoid that, [mono#3024](https://github.com/spinyfin/mono/pull/3024) ("Feed worker and coordinator prompts without argv expansion") stopped putting the prompt on the CLI's argv for every driver and for the coordinator. In its place it added a Python PTY "feeder", `.boss/feed-initial-prompt`. The feeder:

1. starts the CLI;
2. waits until the terminal is in raw mode with bracketed paste enabled;
3. pastes the prompt;
4. sleeps 500 ms;
5. sends one `\r`.

It never checks that the CLI accepted the prompt as a turn.

In production, raw mode plus bracketed paste turned out to be a weak readiness signal. All three Claude spawns and three of the eight Codex spawns in the window reached a live TUI but never had a prompt submitted. The engine's liveness check counted the Claude workers as healthy. The operator noticed within five minutes, paused dispatch, and reverted the change. The feeder was live for 27 minutes.

## 2. Timeline

| UTC (CDT)                  | Event                                                                                                                                                                                                                                        |
| -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 09-26 21:39:17 (16:39)     | #3024 opened.                                                                                                                                                                                                                                |
| 09-26 21:39:47, 22:48:21   | Automated review-guide attempts for #3024 are enqueued. Both Codex guide executions are orphan-reaped in a loop, and neither logs `submitted review guide finalized` (§5).                                                                   |
| 09-26 21:53 → 09-27 05:30  | Two automated PR-review passes (7 and 6 findings) are addressed on the branch, including "Slow CLI startup" and "Paste submission evidence" (§5).                                                                                            |
| **09-27 06:50:01 (01:50)** | **#3024 merged** as [`7b730a4d5c`](https://github.com/spinyfin/mono/commit/7b730a4d5cc5f94eab403f6290698d4c84d48c5c).                                                                                                                        |
| 06:59:25 (01:59)           | Release [`boss-v1.0.682`](https://github.com/spinyfin/mono/releases/tag/boss-v1.0.682) published (tag at `7b730a4d5c`; `boss-v1.0.681...682` = #3025, #3024).                                                                                |
| **07:00:02 (02:00)**       | **Engine restarted** (audit `start`, pid 21851, `launched_by: app`). This is the first engine carrying the feeder. **(inference:** 682 was the newest published release at that moment. The audit record carries no build identity; see §8.) |
| 07:00:58 – 07:01:50        | Six Codex review-guide workers spawn. All six log `agent JSONL progress: discovery overdue` about two minutes later.                                                                                                                         |
| **07:04:41 (02:04)**       | **Detection.** Work is filed: "Initial-prompt feeder must confirm the prompt was submitted (workers stuck on unsent paste)".                                                                                                                 |
| 07:04:54                   | A Claude chore worker spawns. It gets `session_start` but never `user_prompt_submit`.                                                                                                                                                        |
| 07:05:36                   | Five of the six Codex guides report their first turn, at a poll-starved ingest pass, and later finalize between 07:07 and 07:15. The sixth hits `driver-start timeout` at 07:07:13 (328 s silent).                                           |
| 07:08:15 (02:08)           | **Dispatch paused** by the operator. The log line reads "PR-review executions remain exempt".                                                                                                                                                |
| 07:09:44                   | A Claude PR-review worker spawns, exempt from the pause. It gets `session_start` only; no prompt.                                                                                                                                            |
| 07:13:00                   | Revert [mono#3030](https://github.com/spinyfin/mono/pull/3030) opened.                                                                                                                                                                       |
| **07:22:20 (02:22)**       | **Revert merged** as [`ae472495c8`](https://github.com/spinyfin/mono/commit/ae472495c83dc04bfa0e1b6787a60b22942d1b2a).                                                                                                                       |
| 07:24:33 – 07:24:41        | Four more pause-exempt review spawns go through the feeder. Grok submits; Claude and two Codex workers never do.                                                                                                                             |
| 07:25:43                   | [`boss-v1.0.684`](https://github.com/spinyfin/mono/releases/tag/boss-v1.0.684) published (`683...684` = #3030).                                                                                                                              |
| **07:27:08 (02:27)**       | **Revert live**: engine restarted (pid 58859). **(inference:** running 684.)                                                                                                                                                                 |
| 07:27:48 (02:27)           | **Dispatch resumed.** The first post-revert Claude worker goes from `session_start` (07:27:53) to `user_prompt_submit` (07:27:54) in one second.                                                                                             |
| 07:28:19, 07:35:37         | The stranded feeder-era executions are force-stopped.                                                                                                                                                                                        |

- Merge to deploy: 10 min.
- **Deploy to detection: 4 min 39 s.**
- Deploy to revert live: 27 min 06 s.

## 3. Impact

The feeder was live from 07:00:02 to 07:27:08. The engine trace shows 12 worker spawns in that window. Six submitted their prompt and six did not:

| Driver | Spawns                          | Prompt submitted | Never submitted                                                                                           |
| ------ | ------------------------------- | ---------------- | --------------------------------------------------------------------------------------------------------- |
| Claude | 3 (1 chore, 2 PR review)        | 0                | 3. Each logged a `session_start` hook only: no `user_prompt_submit` and no tool use.                      |
| Codex  | 8 (7 review guide, 1 PR review) | 5 guides         | 3. No rollout was ever discovered; each ended in a `driver-start timeout` reap or an operator force-stop. |
| Grok   | 1 (PR review)                   | 1                | 0                                                                                                         |

The coordinator session was not restarted in the window, so #3024's coordinator path never ran. The stranded executions were force-stopped at 07:28:19 and 07:35:37 (`force_stop: cancelled execution and demoted task`).

**Pausing dispatch did not stop new spawns.** Four of the twelve spawns, at 07:09:44 and 07:24:33–41, happened after the operator paused dispatch at 07:08:15. PR-review and review-guide executions are exempt from `dispatch pause`, so they kept going through the known-broken feeder.

## 4. Root cause

The feeder decides the CLI is ready using a terminal-mode heuristic, and it never checks whether the prompt was actually submitted. The code at `7b730a4d5c`:

- [`feed_prompt.rs:75-105`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/driver/src/feed_prompt.rs#L75-L105) (`wait_raw`) declares the CLI ready once two things are true: the PTY has left canonical mode (line 80), and the output stream has carried `ESC[?2004h`, the bracketed-paste enable (lines 97-101). A TUI does both during terminal setup. That is not the same moment as "the composer is mounted and will treat a paste plus Enter as a submission."
- [`feed_prompt.rs:248-257`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/driver/src/feed_prompt.rs#L248-L257) writes the bracketed paste, sleeps a fixed 0.5 s, writes a single `\r`, then hands off to `relay`. From that point the feeder only reports whether the bytes were _written_. If the TUI dropped the prompt or collapsed it into a paste placeholder, the feeder sees success. Consistent with that, `bossctl logs spawn` shows no feeder error text in the window.
- One change switched all three drivers and the coordinator to the feeder at once, with no flag and no per-driver rollout: [`claude.rs:858`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/driver/src/claude.rs#L858), [`codex.rs:1034`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/driver/src/codex.rs#L1034), [`grok.rs:269`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/driver/src/grok.rs#L269), and [`coordinator_tmux.rs:1105`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/core/src/coordinator_tmux.rs#L1105).

#3030's description names two failure mechanisms:

- Large pastes collapse into a paste placeholder, and the lone Enter is swallowed.
- Small prompts are lost when they are pasted before the TUI accepts input.

The engine trace confirms the _effect_: a live CLI with no submitted turn. It cannot say which mechanism hit which pane, because nothing recorded the pane contents or the prompt sizes. Claude failed 3 of 3 while Grok succeeded, which is consistent with a readiness race that depends on each CLI's startup sequence. **(inference)**

The engine's own liveness check hid the failure. For Claude, `driver-start verified` fires on the first driver-originated signal, which is the `SessionStart` hook. That hook fires before any prompt is submitted. As a result, the three stuck Claude workers were reported as successfully started within one to two seconds of spawning (07:04:58, 07:09:50, 07:24:40), and the engine had no reason to flag them. #3034 now requires turn-start evidence for the current run instead.

## 5. Why review and tests missed it

- **The tests used a fake CLI that cannot race.** [`feed_prompt_tests.py:22-38`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/driver/src/feed_prompt_tests.py#L22-L38) runs a stand-in "CLI" that enters raw mode, announces bracketed paste, and immediately blocks reading stdin. In the fake, being ready and being able to accept input happen at the same instant, so the production assumption that failed cannot fail in the test. The suite thoroughly covers the feeder's _own_ mechanics: integrity of payloads over 1 MiB, capability sequences split across reads, EPERM on `killpg`, resize, and teardown. It says nothing about any real TUI.
- **The PR said it had not been run live.** Its "CLI channel evidence" section is static: it cites inspected binaries and Codex source links, and it states "No authenticated end-to-end model session is claimed." The change replaced prompt delivery for every driver and the coordinator, yet it merged without a single live spawn of any driver.
- **Automated PR review flagged the risk and accepted a stand-in for proof.** The second review pass raised "Paste submission evidence". The response added a 500 ms delay, a capability check, and static evidence, but no check that the prompt had been submitted. An earlier finding about slow CLI startup was answered with a 120 s readiness _deadline_, which limits how long the feeder waits but does not test whether readiness was real.
- **The automated review guide produced nothing for this PR.** Two guide attempts were enqueued, at 09-26 21:39 and 22:48. Both Codex executions were orphan-reaped in a loop and neither finalized, so a human reviewer had no guide. A working guide would not have helped anyway: it explains a diff, and it would not have exercised a spawn.

## 6. Detection

Detection took 4 m 39 s, and a human did it. The first six post-deploy spawns were all Codex review guides, and all six logged `discovery overdue` about two minutes after spawning. The operator filed the defect at 07:04:41, before the first Claude spawn. **(inference:** the operator saw the stalled panes directly. Nothing in the trace shows an automated alert, and none of the stuck executions raised an attention item.)

Detection was fast because stuck panes are in front of the operator. The engine's own signals would have taken much longer:

- `driver-start verified` counted the Claude workers as healthy.
- The one `driver-start timeout` needed 300 s of total silence.

**A faster signal:** run a turn-start check on every spawn. If no `UserPromptSubmit` or first-turn event arrives within a bounded window, fail the spawn and raise an attention item. That would have fired on the first Claude worker within tens of seconds. #3034 implements this check.

## 7. Recommendations

These are recommendations only. None of them has been filed as work by this document.

1. **A spawn succeeds when a turn starts, not when a process starts.** Prompt delivery must be confirmed by driver-originated turn-start evidence for the current run, and a spawn without that evidence must fail loudly with an attention item. #3034 (merged 2026-09-30) implements this for argv delivery. Any future non-argv delivery mechanism, such as a successor to the feeder, must be gated on the same check, not on terminal-mode heuristics or byte counts.
2. **Stop counting `SessionStart` as "driver started".** `driver-start verified` fired for Claude workers that never received a prompt. Either rename the signal to what it actually proves, or move it to the first-turn event, so the liveness surfaces operators read cannot report a stuck worker as healthy.
3. **Add a post-deploy canary per driver and worker kind.** After any engine restart onto a new build, dispatch one minimal execution for each combination of driver and worker kind (standard, PR review, review guide), and alert if any fails to reach its first turn. Here, the first Claude spawn would have tripped it within about a minute.
4. **Real-TUI contract tests for prompt delivery.** Any change to how the initial prompt reaches a CLI needs a test against the real, pinned CLI binary, or a fixture recorded from one, that asserts a turn starts. A fake that is ready the instant it announces readiness cannot catch a readiness race.
5. **Require one live spawn before merging spawn-path changes.** When a PR description disclaims live validation ("No authenticated end-to-end model session is claimed") for a change to launch commands or prompt delivery, treat that as a blocker. At minimum, run one real spawn per affected driver in an isolated engine, or roll the change out behind a per-driver switch.
6. **A pause during an incident should cover review spawns.** `dispatch pause` exempts PR-review executions by design. During this incident, that exemption let four more executions through the broken feeder. Consider a pause scope that covers every spawn, for use when the spawn path itself is suspect.
7. **Record the running build.** The engine's audit `start` record says `engine_version: "0.0.0"`, so which release was live had to be inferred from publish times. Record the build SHA and release tag at start.

## 8. What went well, what went badly, lessons

**Went well**

- Detected in under five minutes.
- Reverted within 18 minutes of detection, and live within 27 minutes of the original deploy.
- The engine trace had enough structure (`kind=user_prompt_submit` hook events and audit `start` records) to separate the stuck spawns from the healthy ones after the fact.

**Went badly**

- A change to prompt delivery for every driver and the coordinator merged at 02:00 CDT without a live spawn.
- The engine's liveness signal reported unprompted Claude workers as healthy.
- Pausing dispatch did not stop review spawns from going through the broken path.

**Lesson**

- A readiness heuristic is not evidence that the prompt was delivered. When the outcome is observable (here, a turn started), check the outcome.

## 9. What could not be established

- **Which build each engine ran.** The audit `start` records carry `engine_version: "0.0.0"`. The assignments (682 at 07:00:02, 684 at 07:27:08) are inferred from the newest release published before each start.
- **Why each stuck pane failed.** It could have been a paste placeholder with a swallowed Enter, or a paste that arrived before the composer was ready. Pane contents and prompt sizes were not recorded; the two mechanisms come from #3030's description.
- **How the operator first noticed.** No alert fired. The detection time is when the defect was filed as work.
- **Why the pre-merge review-guide attempts for #3024 died.** Their Codex panes lost their shell PID and were orphan-reaped repeatedly on 09-26, before the change was deployed. That is a separate engine behavior and was not investigated here.
