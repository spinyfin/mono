# Incident 007 — The initial-prompt feeder left workers unprompted, and the pinned-workspace change silently disabled Codex review guides

- **Date:** 2026-09-27 to 2026-09-28
- **Severity:** Incident A (prompt feeder): High, 27 minutes. Half of all worker spawns in the window started with no prompt; dispatch was paused. Incident B (review-guide hook trust): Medium, about 37 hours. Every automated review guide failed to spawn; no worker code was affected.
- **Status:** A was reverted by [mono#3030](https://github.com/spinyfin/mono/pull/3030). Its replacement, which confirms that a turn actually started, is [mono#3034](https://github.com/spinyfin/mono/pull/3034) (merged 2026-09-30 23:53 UTC). B was fixed by [mono#3036](https://github.com/spinyfin/mono/pull/3036) and tidied up by [mono#3039](https://github.com/spinyfin/mono/pull/3039). Failed guide attempts from B's window were not recovered automatically.

Times are UTC with US Central Daylight Time (CDT, UTC−5) in parentheses. Evidence comes from GitHub (PRs, merge commits, `boss-v1.0.*` releases) and from the engine's own diagnostics, read through `bossctl logs` (sources `engine`, `audit`, `dispatch`). Anything inferred rather than read from a record is marked **(inference)**.

## 1. Summary

Two Boss changes merged 18 minutes apart early on 2026-09-27 and went out in consecutive releases. Each broke worker spawning, but in different ways and with very different detection times.

**Incident A — [mono#3024](https://github.com/spinyfin/mono/pull/3024), "Feed worker and coordinator prompts without argv expansion."** Large initial prompts could exceed `ARG_MAX`, so #3024 stopped putting the prompt on the CLI's argv. Instead it added a Python PTY "feeder" (`.boss/feed-initial-prompt`). The feeder starts the CLI, waits until the terminal is in raw mode with bracketed paste enabled, pastes the prompt, sleeps 500 ms, and sends one `\r`. It never checked that the CLI had accepted the prompt as a turn. In production, raw mode plus bracketed paste turned out to be a weak readiness signal. **All three Claude spawns and three of the seven Codex spawns** in the window reached a live TUI and never received a submitted prompt. The operator noticed within five minutes, paused dispatch, and reverted the change. The feeder was live for 27 minutes.

**Incident B — [mono#3028](https://github.com/spinyfin/mono/pull/3028), "Give review guides pinned read-only source workspaces."** #3028 gave Codex review-guide workers a named permission profile, `[permissions.review-guide]`, in the per-run `config.toml`. It selected that profile only through a CLI flag (`--config default_permissions="review-guide"`) on the worker's own command line. Before any Codex worker spawns, the engine's hook-trust gate runs `codex app-server` against the same `CODEX_HOME` to confirm the PreToolUse guards are armed. That process never sees the worker's CLI flag. Codex 0.153.4 refuses a config that defines `[permissions]` profiles without `default_permissions`, falls back to defaults, and returns an empty hook list. The gate treats silence as failure, correctly, and refused every Codex review-guide spawn. **28 of 28 review-guide spawn attempts were refused** between deployment and the fix, and no review guide completed for 36 h 54 m. The earlier suspicion that the `network_proxy`/`permissions` keys were unsupported by Codex 0.153.4 was **wrong**. Both keys are in 0.153.4's schema; the rejection was the missing default selection (§4.2).

The two incidents share a shape. Both changes altered how a worker process is launched. Both were validated only against stand-ins for the real CLI: a fake TUI for A, and a test that explicitly tolerates the hook-trust failure for B. Both PR descriptions said plainly that no live end-to-end run had been done. The detection gap differed by two orders of magnitude, and the reason is visibility. A stuck worker pane is visible to the operator. A failed review guide is an ERROR line in the engine trace and a generic "failed" state in the app.

## 2. Timeline

### 2.1 Incident A — prompt feeder (#3024)

| UTC (CDT)                  | Event                                                                                                                                                                                                                                              |
| -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 09-26 21:39:17 (16:39)     | #3024 opened.                                                                                                                                                                                                                                      |
| 09-26 21:39:47, 22:48:21   | Automated review-guide attempts for #3024 are enqueued. Both Codex guide executions are orphan-reaped in a loop, and neither logs `submitted review guide finalized` (§3.4).                                                                       |
| 09-26 21:53 → 09-27 05:30  | Two automated PR-review passes (7 and 6 findings) are addressed on the branch, including "Slow CLI startup" and "Paste submission evidence" (§3.3).                                                                                                |
| **09-27 06:50:01 (01:50)** | **#3024 merged** as [`7b730a4d5c`](https://github.com/spinyfin/mono/commit/7b730a4d5cc5f94eab403f6290698d4c84d48c5c).                                                                                                                              |
| 06:59:25 (01:59)           | Release [`boss-v1.0.682`](https://github.com/spinyfin/mono/releases/tag/boss-v1.0.682) published (tag at `7b730a4d5c`; `boss-v1.0.681...682` = #3025, #3024).                                                                                      |
| **07:00:02 (02:00)**       | **Engine restarted** (audit `start`, pid 21851, `launched_by: app`). This is the first engine carrying the feeder. **(inference:** 682 was the newest published release at that moment, and the audit record carries no build identity; see §6.7). |
| 07:00:58 – 07:01:50        | Six Codex review-guide workers spawn. All six log `agent JSONL progress: discovery overdue` about two minutes later.                                                                                                                               |
| **07:04:41 (02:04)**       | **Detection.** Work is filed: "Initial-prompt feeder must confirm the prompt was submitted (workers stuck on unsent paste)".                                                                                                                       |
| 07:04:54                   | A Claude chore worker spawns. It gets `session_start` but never `user_prompt_submit`.                                                                                                                                                              |
| 07:05:36                   | Five of the six Codex guides report their first turn (at a poll-starved ingest pass). They later finalize between 07:07 and 07:15. The sixth hits `driver-start timeout` at 07:07:13 (328 s silent).                                               |
| 07:08:03                   | #3028 merged (Incident B begins its path to production).                                                                                                                                                                                           |
| 07:08:15 (02:08)           | **Dispatch paused** by the operator. The log line reads "PR-review executions remain exempt".                                                                                                                                                      |
| 07:09:44                   | A Claude PR-review worker spawns (exempt from the pause). `session_start` only; no prompt.                                                                                                                                                         |
| 07:13:00                   | Revert [mono#3030](https://github.com/spinyfin/mono/pull/3030) opened.                                                                                                                                                                             |
| 07:19:48                   | `boss-v1.0.683` published (#3028 only). No engine ever ran this build; there is no restart between 07:00:02 and 07:27:08.                                                                                                                          |
| **07:22:20 (02:22)**       | **Revert merged** as [`ae472495c8`](https://github.com/spinyfin/mono/commit/ae472495c83dc04bfa0e1b6787a60b22942d1b2a).                                                                                                                             |
| 07:24:33 – 07:24:41        | Four more exempt review spawns go through the feeder. Grok submits. Claude and two Codex workers never do.                                                                                                                                         |
| 07:25:43                   | [`boss-v1.0.684`](https://github.com/spinyfin/mono/releases/tag/boss-v1.0.684) published (`683...684` = #3030). It contains #3028 as well.                                                                                                         |
| **07:27:08 (02:27)**       | **Revert live**: engine restarted (pid 58859). **(inference:** running 684.)                                                                                                                                                                       |
| 07:27:48 (02:27)           | **Dispatch resumed.** The first post-revert Claude worker goes from `session_start` (07:27:53) to `user_prompt_submit` (07:27:54) in one second.                                                                                                   |
| 07:28:19, 07:35:37         | The stranded feeder-era executions are force-stopped.                                                                                                                                                                                              |

Merge to deploy: 10 min. **Deploy to detection: 4 min 39 s.** Deploy to revert live: 27 min 06 s.

### 2.2 Incident B — review-guide hook trust (#3028)

| UTC (CDT)                  | Event                                                                                                                                                                                                                                                                      |
| -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 09-26 22:01:26 (17:01)     | #3028 opened.                                                                                                                                                                                                                                                              |
| 09-26 22:01:38             | An automated review-guide attempt for #3028 is enqueued. The Codex pane's shell PID disappears within two minutes, and the execution is orphan-reaped and re-adopted in a loop. No guide finalizes (§4.4).                                                                 |
| 09-26 22:47 → 09-27 05:01  | An automated PR-review pass (8 findings) is addressed. None of the findings concerns Codex config loading.                                                                                                                                                                 |
| 09-27 06:52 – 06:57        | Merge-conflict revision; `MODULE.bazel.lock` digest reconciled.                                                                                                                                                                                                            |
| **07:08:03 (02:08)**       | **#3028 merged** as [`cffa1d2648`](https://github.com/spinyfin/mono/commit/cffa1d264838a6f709bbc4dccb79ed1ae7278689).                                                                                                                                                      |
| 07:19:48                   | `boss-v1.0.683` published (`682...683` = #3028). Never run (see above).                                                                                                                                                                                                    |
| 07:25:43                   | `boss-v1.0.684` published: the Incident A revert, carrying #3028 with it.                                                                                                                                                                                                  |
| **07:27:08 (02:27)**       | **#3028 live** with the revert's engine restart.                                                                                                                                                                                                                           |
| **07:36:16 (02:36)**       | **First refusal:** `spawn aborted … Codex hook-trust gate refused to arm PreToolUse guards: … hooks/list returned no hook entries — silence is not success; refusing worker`.                                                                                              |
| 07:36 → 09-28 14:53        | 28 identical refusals across 26 review-guide work items. All 28 are `driver=codex`. Zero guides finalize.                                                                                                                                                                  |
| 09-27 18:25:26 (13:25)     | Engine restarted (pid 78727) on a build without the fix. **(inference:** 685, published 07:50:34.) Failures continue.                                                                                                                                                      |
| **19:27:07 (14:27)**       | **Detection.** Work is filed: "Codex review-guide spawns refused by hook-trust gate since pinned-workspace change". Filed alongside it: "Show why a review-guide or worker spawn failed in the app" ([mono#3037](https://github.com/spinyfin/mono/pull/3037), still open). |
| 19:38:42                   | [mono#3036](https://github.com/spinyfin/mono/pull/3036) opened, with a local reproduction against the installed `codex-cli 0.153.4`.                                                                                                                                       |
| **19:53:06 (14:53)**       | **Fix merged** as [`d2847b6627`](https://github.com/spinyfin/mono/commit/d2847b66276a25586e399d4ce5e4bf0f36d1a21c).                                                                                                                                                        |
| 20:05:23 (15:05)           | [`boss-v1.0.686`](https://github.com/spinyfin/mono/releases/tag/boss-v1.0.686) published (`685...686` = #3032, #3036).                                                                                                                                                     |
| 09-28 14:53:55 (09:53)     | Last refusal. The engine started at 09-27 18:25 is still running and never picked up 686.                                                                                                                                                                                  |
| **09-28 20:21:12 (15:21)** | **Fix live**: engine restarted (pid 30905). **(inference:** running 686; 687 was not published until 21:06.)                                                                                                                                                               |
| 20:26:00 (15:26)           | First review guide finalized since 09-27 07:15.                                                                                                                                                                                                                            |
| 20:53:45                   | [mono#3039](https://github.com/spinyfin/mono/pull/3039) merged. It is a review follow-up to #3036: it makes one Bazel constant the single source for the Codex version pin, and it drops the now-redundant CLI override.                                                   |
| 21:06:07                   | [`boss-v1.0.687`](https://github.com/spinyfin/mono/releases/tag/boss-v1.0.687) published (`686...687` = #3039, #3038, #3042).                                                                                                                                              |

Merge to deploy: 19 min. Deploy to first failure: 9 min. **Deploy to detection: 12 h 00 m** (11 h 51 m from first failure). Detection to fix merged: 26 min. **Fix published to fix live: 24 h 16 m.** Total outage, deploy to fix live: **36 h 54 m**.

## 3. Incident A — prompt feeder

### 3.1 Impact

The feeder was live from 07:00:02 to 07:27:08. The engine trace shows 12 worker spawns in that window. Six submitted their prompt and six did not:

| Driver | Spawns                          | Prompt submitted | Never submitted                                                                                      |
| ------ | ------------------------------- | ---------------- | ---------------------------------------------------------------------------------------------------- |
| Claude | 3 (1 chore, 2 PR review)        | 0                | 3: `session_start` hook only, no `user_prompt_submit`, no tool use                                   |
| Codex  | 8 (7 review guide, 1 PR review) | 5 guides         | 3: no rollout ever discovered; each ended in a `driver-start timeout` reap or an operator force-stop |
| Grok   | 1 (PR review)                   | 1                | 0                                                                                                    |

The coordinator session was not restarted in the window, so the coordinator path in #3024 was never exercised. The stranded executions were force-stopped at 07:28:19 and 07:35:37 (`force_stop: cancelled execution and demoted task`).

One aggravating factor: **the dispatch pause did not stop the bleeding.** Four of the twelve spawns (07:09:44 and 07:24:33–41) came after the operator paused dispatch at 07:08:15. PR-review and review-guide executions are exempt from `dispatch pause`, so they kept going through the known-broken feeder.

### 3.2 Root cause

The feeder's only readiness test is a terminal-mode heuristic, and it never observes the outcome. At `7b730a4d5c`:

- [`feed_prompt.rs:75-105`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/driver/src/feed_prompt.rs#L75-L105) (`wait_raw`) declares the CLI ready as soon as the PTY has left canonical mode (line 80) and the output stream has carried `ESC[?2004h`, the bracketed-paste enable (lines 97-101). A TUI does both during terminal setup. That is not the same moment as "the composer is mounted and will treat a paste plus Enter as a submission."
- [`feed_prompt.rs:248-257`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/driver/src/feed_prompt.rs#L248-L257) writes the bracketed paste, sleeps a fixed 0.5 s, writes a single `\r`, and then drops into `relay`. From then on the feeder only reports whether the bytes were _written_. A prompt the TUI dropped or collapsed into a paste placeholder looks exactly like success to it. Consistent with that, `bossctl logs spawn` shows no feeder error text in the window.
- All three drivers and the coordinator were switched to it in the same change: [`claude.rs:858`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/driver/src/claude.rs#L858), [`codex.rs:1034`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/driver/src/codex.rs#L1034), [`grok.rs:269`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/driver/src/grok.rs#L269), and [`coordinator_tmux.rs:1105`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/core/src/coordinator_tmux.rs#L1105). There was no flag and no per-driver rollout.

#3030's description gives the two failure mechanisms: large pastes collapse into a paste placeholder and the lone Enter is swallowed, and small prompts are lost when pasted before the TUI accepts input. The engine trace confirms the _effect_: a live CLI with no submitted turn. It cannot say which mechanism hit which pane, because nothing recorded the pane contents or prompt sizes. Claude failing 3 of 3 while Grok succeeded is consistent with a readiness race that depends on the CLI's startup sequence. **(inference)**

The engine's own liveness check made things worse. For Claude, `driver-start verified` fires on the first driver-originated signal, which is the `SessionStart` hook. That hook fires before any prompt is submitted. So the three stuck Claude workers were reported as successfully started within one to two seconds of spawning (07:04:58, 07:09:50, 07:24:40), and the engine had no reason to flag them. #3034 now requires turn-start evidence for the current run instead.

### 3.3 Why review and tests missed it

- **Tests used a fake CLI that cannot race.** [`feed_prompt_tests.py:22-38`](https://github.com/spinyfin/mono/blob/7b730a4d5cc5f94eab403f6290698d4c84d48c5c/tools/boss/engine/driver/src/feed_prompt_tests.py#L22-L38) runs a stand-in "CLI" that enters raw mode, announces bracketed paste, and immediately blocks reading stdin. In the fake, readiness and the ability to accept input are the same instant, so the one production assumption that failed cannot fail in the test. The suite is thorough about the feeder's _own_ mechanics: more than 1 MiB of payload integrity, split capability sequences, EPERM on `killpg`, resize, and teardown. It says nothing about any real TUI.
- **The PR said so.** Its "CLI channel evidence" section is static. It cites inspected binaries and Codex source links, and it states: "No authenticated end-to-end model session is claimed." The change replaced prompt delivery for every driver and the coordinator, yet it merged without one live spawn of any driver.
- **Automated PR review saw the risk and accepted a proxy.** The second review pass raised "Paste submission evidence". The response was a 500 ms delay, a capability check, and static evidence. It was not a check that the prompt had been submitted. An earlier finding about slow CLI startup was answered with a 120 s readiness _deadline_, which bounds how long the feeder waits, not whether readiness was real.
- **The automated review guide produced nothing for this PR.** Two guide attempts (09-26 21:39 and 22:48) were enqueued. Both Codex executions were orphan-reaped in a loop and neither finalized, so no guide existed for a human reviewer. Even a working guide explains a diff; it would not have exercised a spawn.

### 3.4 Detection

Detection took 4 m 39 s, and it was human. The first six post-deploy spawns were all Codex review guides, and all six logged `discovery overdue` about two minutes after spawning. The operator filed the defect at 07:04:41, before the first Claude spawn. **(inference:** the operator saw the stalled panes directly; nothing in the trace shows an automated alert, and none of the stuck executions raised an attention item.) Detection was fast because stuck panes sit in front of the operator. The engine's own signals would not have caught it for a long time: `driver-start verified` counted the Claude workers healthy, and the one `driver-start timeout` needed 300 s of total silence.

**A faster signal:** on every spawn, a turn-start check, meaning a `UserPromptSubmit`/first-turn event within a bounded window, with a spawn failure and an attention item when it does not arrive. That would have fired on the first Claude worker within tens of seconds. #3034 implements the check.

## 4. Incident B — review-guide hook trust

### 4.1 Impact

- No automated review guide was produced from 07:27:08 on 09-27 until 20:21:12 on 09-28 (first success at 20:26:00).
- The engine trace in that window has 25 guide source captures, 25 generation enqueues, and **28 spawn attempts across 26 guide work items. All were `driver=codex`, and all 28 were refused by the hook-trust gate.** No other failure mode appears for guides in the window.
- The operator's working figure is "about 63 PRs captured but never enqueued for guides". **It could not be reproduced from the engine trace**, which shows only the 25 captures above. It may come from a different surface (the runtime database, or the app's per-PR guide state) that this investigation did not read. Either way, the failed attempts were not retried automatically after the fix went live. The first post-fix guides (20:26 onward) are new attempts, and the trace shows no bulk re-enqueue.
- Worker code, PR review, and merges were unaffected. Review guides are advisory, so reviewers merged PRs in the window without them.

### 4.2 Root cause

The worker and the hook-trust observer load the same `config.toml`, but only the worker received the flag that makes that file valid.

At `cffa1d2648`:

- [`codex.rs:702-714`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/driver/src/codex.rs#L702-L714) (`render_review_guide_config`) writes a `[permissions.review-guide]` profile (extends `:read-only`, allowing only the frontend Unix socket) plus a `features.network_proxy` table. It does **not** write `default_permissions`.
- [`codex.rs:515-520`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/driver/src/codex.rs#L515-L520) (`codex_sandbox_extra_args`) selects the profile only as a worker CLI argument: `--config default_permissions="review-guide"`.
- [`codex.rs:1728-1737`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/driver/src/codex.rs#L1728-L1737) calls `write_hooks_and_attest` with that config _before_ the extra args exist. The attestation step, [`codex-hook-trust/src/lib.rs:635-641`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/codex-hook-trust/src/lib.rs#L635-L641), launches `codex app-server` with only `CODEX_HOME` set.
- Codex 0.153.4's config loader rejects that file: [`core/src/config/mod.rs:3401-3412` at `rust-v0.153.4`](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/config/mod.rs#L3401-L3412) returns ``config defines `[permissions]` profiles but does not set `default_permissions` `` when profiles exist and none is selected. (This investigation read that source directly.) `app-server` reports a `configWarning`, continues with default config, and answers `hooks/list` with `hooks: []` and the error in an `errors` field. This behavior is reproduced in #3036's description.
- The gate, [`codex-hook-trust/src/lib.rs:715-719`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/codex-hook-trust/src/lib.rs#L715-L719), sees no hook entries and refuses: "silence is not success". **The gate behaved correctly.** Arming no guards is exactly what it exists to refuse.

**The suspected cause was wrong.** The early theory was that `network_proxy` or `permissions` keys were unsupported by Codex 0.153.4. #3036's provenance table cites 0.153.4's `config.schema.json` for every key the guide policy uses (`default_permissions`, `permissions.*.extends`, `.network.enabled`, `.domains`, `.unix_sockets`, `features.network_proxy` and its fields, `web_search`). The loader check above is the actual rejection. The fix, [`codex.rs:708` at `d2847b6627`](https://github.com/spinyfin/mono/blob/d2847b66276a25586e399d4ce5e4bf0f36d1a21c/tools/boss/engine/driver/src/codex.rs#L708), writes `default_permissions = "review-guide"` into the rendered file, so the observer and the worker load the same valid policy. #3039 then removed the redundant CLI override.

**A diagnosability defect lengthened the investigation.** The observer discards `app-server` stderr ([`lib.rs:627`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/codex-hook-trust/src/lib.rs#L627)), and the `hooks/list` parser ([`lib.rs:663-720`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/codex-hook-trust/src/lib.rs#L663-L720)) never reads the response's `errors` array. So Codex's precise, actionable message ("config defines `[permissions]` profiles but does not set `default_permissions`") never reached any Boss log. All 28 refusals say only "no hook entries". Both behaviors are unchanged on `main` at the time of writing.

### 4.3 Why review and tests missed it

- **The one test that reached the gate accepts the gate's refusal as a pass.** [`codex_tests.rs:1371-1376`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/driver/src/codex_tests.rs#L1371-L1376):
  ```rust
  // The sandbox forbids launching a live Codex process; materialization must
  // finish before the separate hook-trust attestation reports that failure.
  let result = rt.block_on(driver.write_permission_config(&input, tmp.path()));
  if let Err(error) = result {
      assert!(error.to_string().contains("hook-trust gate"), "{error:#}");
  }
  ```
  A hook-trust refusal is the exact production failure, and the test treats it as an expected outcome. It then checks the TOML shape: the profile, the socket, and `web_search`. The file is never loaded by a real Codex. The other new tests assert the CLI arguments (`default_permissions` present) and the string contents of the rendered config.
- **No test ran the real CLI against the rendered config.** The fix PR, #3036, added exactly that: a non-optional Bazel test, [`config_compatibility_tests.rs:12`](https://github.com/spinyfin/mono/blob/d2847b66276a25586e399d4ce5e4bf0f36d1a21c/tools/boss/engine/driver/src/codex/config_compatibility_tests.rs#L12), runs checksum-pinned Codex 0.153.4 binaries through the production attestation gate. It reproduces the old refusal and verifies the fix, with no model credentials. The same test before merge would have failed #3028.
- **Automated PR review (8 findings) looked at policy, not loadability.** Its findings covered permission breadth, the fail-open hook, link parsing, symlinks, and wildcard metadata directories. All were security or correctness properties of the policy. None asked whether Codex would accept the file.
- **The PR disclosed that its final state was not fully tested.** "Full-suite results below are from the preceding implementation revision; the merged branch will run its tests in CI." No live guide run was claimed.
- **The review guide for #3028 did not exist, and could not have helped.** The guide attempt at 09-26 22:01 was orphan-reaped in a loop and never finalized. More fundamentally, a PR's guide is generated by the _deployed_ engine, so it can never exercise the PR's own spawn configuration. Guides explain a change to a human; they are not a test of it.

### 4.4 Detection

Detection took 12 h 00 m from deploy (11 h 51 m from the first refusal), and it was human. The engine logged an ERROR with a precise-looking reason on every attempt. Nothing turned that into an operator-visible signal:

- **No attention item or alert.** The refusal terminalizes the execution as `failed` before any pane exists. The trace shows no attention item raised for these failures. Twenty-eight consecutive refusals of the same kind, with zero successes for over a day, never crossed any threshold.
- **The app showed a failed guide without a reason.** Work to "Show why a review-guide or worker spawn failed in the app" ([mono#3037](https://github.com/spinyfin/mono/pull/3037)) was filed at the moment of detection. Its description says the app previously fell back to a generic "execution failed" label. **(inference:** the operator saw failed or missing guides but had no cause to act on until someone read the engine log.)
- **Guides are advisory.** Nothing blocks on a guide, so their absence has no downstream symptom. Most of the window was also overnight in CDT (02:36 to about 13:00). **(inference:** part of the gap is simply that nobody was looking.)

**A faster signal:** a per-worker-kind spawn-success check that raises one attention item when a kind has N consecutive pre-start failures with no success. Here, the second or third refusal at around 07:45 would have produced a page carrying the refusal text. Even better, a post-deploy canary that spawns one of each worker kind per driver (§6.3) would have caught this within minutes of the 07:27 restart.

### 4.5 The fix sat unshipped for a day

#3036 merged 26 minutes after detection and was published in `boss-v1.0.686` at 20:05:23 on 09-27. The engine running at the time had started at 18:25:26 on a pre-fix build, and it kept running until 20:21:12 on 09-28. It refused one more guide at 14:53:55 on 09-28. That is **24 h 16 m** of outage after the fix was released, two-thirds of the incident's total length. Nothing prompted a restart: the engine does not know which release it is running (its audit `start` record says `engine_version: "0.0.0"`), and nothing compares the running build to the fixes published since.

## 5. The two incidents interacted

- **The revert shipped the second incident.** `boss-v1.0.684`, the release that carried the Incident A revert, also contained #3028, merged 14 minutes earlier. The only engine restart that fixed A therefore started B. #3030's description notes that the revert "applied cleanly on top of #3028". That is a merge-conflict check, not a risk check, and nobody decided to ship #3028 during an active incident.
- **Build 683 never ran.** There was no restart between 07:00:02 and 07:27:08, so #3028 never ran in isolation. It could not have been observed apart from the recovery.
- **Both PRs merged at about 02:00 CDT**, with automated conflict resolution on #3028 minutes before merge.

## 6. Recommendations

These are recommendations only. None of them has been filed as work by this document.

1. **Spawn success means a turn started, not a process started (A).** Prompt delivery must be confirmed by driver-originated turn-start evidence for the current run, and a spawn without it must fail loudly with an attention item. #3034 (merged 2026-09-30) implements this for argv delivery. Any future non-argv delivery mechanism, such as a successor to the feeder, must be gated on the same check rather than on terminal-mode heuristics or byte counts.
2. **Stop counting `SessionStart` as "driver started" (A).** `driver-start verified` fired for Claude workers that never received a prompt. Either rename it to what it proves or move it to the first-turn signal, so the liveness surfaces operators read cannot report a stuck worker as healthy.
3. **Post-deploy canary per driver and worker kind (A, B).** After any engine restart onto a new build, dispatch one minimal execution for each driver × worker kind (standard, PR review, review guide) and alert if any fails to reach its first turn. Both incidents failed on the very first spawn of the affected kind, so a canary would have caught A in about one minute and B in about ten, instead of five minutes (by eye) and twelve hours.
4. **Real-binary contract tests for everything Boss renders into a driver's config (B).** Extend #3036's pinned-Codex attestation test so every worker kind's rendered `config.toml` is loaded by the pinned CLI. Add the equivalent for Claude and Grok settings files where their CLIs can validate them offline. As a rule, a test must never accept the production failure as a pass: rewrite or delete assertions shaped like `codex_tests.rs:1371-1376`, which tolerated a hook-trust refusal.
5. **Make the hook-trust refusal name the real cause (B).** Parse and include the `hooks/list` `errors` array and `configWarning` notifications in `TrustGateError`, and keep a bounded tail of `app-server` stderr instead of discarding it (`lib.rs:627`, `lib.rs:1332` on `main`). The gate should keep refusing; only its message changes.
6. **Alert on streaks of pre-start failures (B).** Raise one attention item when a worker kind or driver sees N consecutive pre-start failures with no intervening success, and include the latest error text. Land [mono#3037](https://github.com/spinyfin/mono/pull/3037), open at the time of writing, so the app shows the engine's reason instead of a generic "failed".
7. **Record the running build and surface drift (B).** Have the engine's audit `start` record carry its build SHA and release tag, not `0.0.0`. Have the app or engine flag when the running engine is older than the newest published release, especially when the gap contains merged fixes. For B, this alone might have cut a day off the incident.
8. **Recover guide attempts that failed before start (B).** When a guide attempt fails before spawn and the engine later restarts on a different build, re-enqueue those attempts, or at least list them, so an outage like this does not leave a silent backlog of PRs without guides.
9. **An incident pause should include review spawns (A).** `dispatch pause` exempts PR-review executions by design. During a spawn-path incident that exemption let four more executions through the broken feeder. Consider a pause scope that covers every spawn, for use when the spawn path itself is suspect.
10. **Changes to the spawn path need one live spawn before merge (A, B).** Both PR descriptions honestly disclaimed live validation ("No authenticated end-to-end model session is claimed"; "the merged branch will run its tests in CI"). For changes to launch commands, prompt delivery, driver config, or permission artifacts, treat that disclaimer as a blocker. At minimum, run one real spawn per affected driver in an isolated engine, or roll out behind a per-driver switch.
11. **Don't ship unrelated changes in a revert release (A→B).** When a release exists to carry a revert, deploy it from the revert alone, or decide explicitly what else rides along.

## 7. What went well

- Incident A was detected in under five minutes, the change was reverted within 23 minutes of detection, and the revert was live within 27 minutes of the original deploy.
- The hook-trust gate held. It refused to launch Codex review-guide workers whose PreToolUse guards it could not prove were armed, rather than launching them unguarded.
- #3036 found the actual cause, overturning the network-proxy theory, with a reproduction against the installed CLI and upstream source links. It landed a hermetic real-binary regression test in the same PR, 26 minutes after detection.
- The engine trace had enough structure (`kind=user_prompt_submit` hook events, `spawn aborted` with full error text, audit `start` records) to reconstruct both timelines after the fact.

## 8. What went badly

- Two spawn-path changes merged at 02:00 CDT, 18 minutes apart. Neither had had a live spawn.
- A test was written to accept the exact failure that then happened in production.
- For 12 hours an ERROR repeated 28 times with zero successes, and nobody was told.
- The actionable Codex error message was discarded, so the first hypothesis was wrong.
- A fix sat published for a day because nothing knew the running engine was stale.
- The revert release carried the next incident into production.
- The engine's liveness signal reported unprompted Claude workers as healthy.

## 9. Lessons

- **A heuristic for readiness is not evidence of delivery.** Where the outcome is observable (a turn started, hooks armed), check the outcome.
- **"The gate refused" in a test is a failure, not a skip.** When a sandbox cannot run the real dependency, the answer is a hermetic pinned binary (as #3036 did), not a tolerated error.
- **Invisible failures last as long as nobody reads the logs.** The difference between a 5-minute and a 12-hour detection was whether the failure was on screen.
- **A merged fix is not a deployed fix.** Deployment state needs to be observable from inside Boss.

## 10. What could not be established

- **Which build each engine ran.** The audit `start` records carry `engine_version: "0.0.0"`. The build assignments (682 at 07:00:02, 684 at 07:27:08, 685 at 18:25:26, 686 at 09-28 20:21:12) are inferred from the newest release published before each start.
- **Why exactly each stuck feeder pane failed** (paste placeholder with swallowed Enter, or paste before the composer was ready). Pane contents and prompt sizes were not recorded. The mechanisms come from #3030's description.
- **The "about 63 PRs" figure** for guides captured but never enqueued. The engine trace for the window shows 25 captures, 25 enqueues and 28 refused spawns, and this investigation did not read the runtime database. The figure may come from a surface not examined here.
- **How the operator first noticed each incident.** No alert fired in either case. The detection times above are the times the defects were filed as work.
- **Why the pre-merge review-guide attempts for #3024 and #3028 died.** Their Codex panes lost their shell PID and were orphan-reaped repeatedly on 09-26, before either change was deployed. That is a separate engine behavior, not investigated here.
