# Incident 008 — Codex review-guide spawns were refused by the hook-trust gate for 37 hours

- **Date:** 2026-09-27 to 2026-09-28
- **Severity:** Medium. Every automated review guide failed to spawn for 36 h 54 m. No worker code, PR review or merge was affected.
- **Status:** Fixed by [mono#3036](https://github.com/spinyfin/mono/pull/3036), with follow-up cleanup in [mono#3039](https://github.com/spinyfin/mono/pull/3039). Guide attempts that failed during the outage were not recovered automatically.

Times are UTC with US Central Daylight Time (CDT, UTC−5) in parentheses. Evidence comes from GitHub (PRs, merge commits, `boss-v1.0.*` releases), from the engine's own diagnostics read through `bossctl logs` (sources `engine`, `audit`, `dispatch`), and from the upstream Codex source at `rust-v0.153.4`. Anything inferred rather than read from a record is marked **(inference)**.

Related: this change reached production in `boss-v1.0.684`, the release that carried the revert for the separate [incident 007](incident-007-initial-prompt-feeder-left-workers-unprompted.md).

## 1. Summary

[mono#3028](https://github.com/spinyfin/mono/pull/3028), "Give review guides pinned read-only source workspaces", gave Codex review-guide workers a named permission profile, `[permissions.review-guide]`, in each run's `config.toml`. The change selected that profile only through a flag on the worker's own command line, `--config default_permissions="review-guide"`.

Before any Codex worker spawns, the engine's hook-trust gate runs `codex app-server` against the same `CODEX_HOME` to confirm the PreToolUse guards are armed. That process never sees the worker's command-line flag. Codex 0.153.4 rejects a config that defines `[permissions]` profiles without setting `default_permissions`. It falls back to defaults and returns an empty hook list. The gate treats an empty list as failure, which is correct, so it refused every Codex review-guide spawn.

All 28 review-guide spawn attempts were refused between deployment and the fix, and no review guide completed for 36 h 54 m. Detection took 12 hours, because the only signal was an ERROR line in the engine trace. After the fix was published, it took another 24 hours to reach the running engine.

The early suspicion that Codex 0.153.4 did not support the `network_proxy` and `permissions` keys was **wrong**. Both are in 0.153.4's schema. The actual rejection was the missing `default_permissions` selection (§4).

## 2. Timeline

| UTC (CDT)                  | Event                                                                                                                                                                                                                                                                      |
| -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 09-26 22:01:26 (17:01)     | #3028 opened.                                                                                                                                                                                                                                                              |
| 09-26 22:01:38             | An automated review-guide attempt for #3028 is enqueued. The Codex pane's shell PID disappears within two minutes, and the execution is orphan-reaped and re-adopted in a loop. No guide finalizes (§5).                                                                   |
| 09-26 22:47 → 09-27 05:01  | An automated PR-review pass (8 findings) is addressed. None of the findings concerns Codex config loading.                                                                                                                                                                 |
| 09-27 06:52 – 06:57        | Merge-conflict revision. The `MODULE.bazel.lock` digest is reconciled.                                                                                                                                                                                                     |
| **07:08:03 (02:08)**       | **#3028 merged** as [`cffa1d2648`](https://github.com/spinyfin/mono/commit/cffa1d264838a6f709bbc4dccb79ed1ae7278689).                                                                                                                                                      |
| 07:19:48                   | [`boss-v1.0.683`](https://github.com/spinyfin/mono/releases/tag/boss-v1.0.683) published (`682...683` = #3028). No engine ran this build; the engine was not restarted between 07:00:02 and 07:27:08.                                                                      |
| 07:25:43                   | [`boss-v1.0.684`](https://github.com/spinyfin/mono/releases/tag/boss-v1.0.684) published. It contains #3028.                                                                                                                                                               |
| **07:27:08 (02:27)**       | **#3028 live**: engine restarted (audit `start`, pid 58859). **(inference:** running 684. The audit record carries no build identity; see §7.)                                                                                                                             |
| **07:36:16 (02:36)**       | **First refusal:** `spawn aborted … Codex hook-trust gate refused to arm PreToolUse guards: … hooks/list returned no hook entries — silence is not success; refusing worker`.                                                                                              |
| 07:36 → 09-28 14:53        | 28 identical refusals across 26 review-guide work items, all `driver=codex`. No guide finalizes.                                                                                                                                                                           |
| 09-27 18:25:26 (13:25)     | Engine restarted (pid 78727) on a build without the fix. **(inference:** 685, published 07:50:34.) Refusals continue.                                                                                                                                                      |
| **19:27:07 (14:27)**       | **Detection.** Work is filed: "Codex review-guide spawns refused by hook-trust gate since pinned-workspace change". Filed alongside it: "Show why a review-guide or worker spawn failed in the app" ([mono#3037](https://github.com/spinyfin/mono/pull/3037), still open). |
| 19:38:42                   | [mono#3036](https://github.com/spinyfin/mono/pull/3036) opened, with a local reproduction against the installed `codex-cli 0.153.4`.                                                                                                                                       |
| **19:53:06 (14:53)**       | **Fix merged** as [`d2847b6627`](https://github.com/spinyfin/mono/commit/d2847b66276a25586e399d4ce5e4bf0f36d1a21c).                                                                                                                                                        |
| 20:05:23 (15:05)           | [`boss-v1.0.686`](https://github.com/spinyfin/mono/releases/tag/boss-v1.0.686) published (`685...686` = #3032, #3036).                                                                                                                                                     |
| 09-28 14:53:55 (09:53)     | Last refusal. The engine started at 09-27 18:25 is still running, so 686 has not been picked up.                                                                                                                                                                           |
| **09-28 20:21:12 (15:21)** | **Fix live**: engine restarted (pid 30905). **(inference:** running 686. 687 was not published until 21:06.)                                                                                                                                                               |
| 20:26:00 (15:26)           | First review guide finalized since 09-27 07:15.                                                                                                                                                                                                                            |
| 20:53:45                   | [mono#3039](https://github.com/spinyfin/mono/pull/3039) merged. This review follow-up to #3036 makes one Bazel constant the single source for the Codex version pin and drops the now-redundant command-line override.                                                     |
| 21:06:07                   | [`boss-v1.0.687`](https://github.com/spinyfin/mono/releases/tag/boss-v1.0.687) published (`686...687` = #3039, #3038, #3042).                                                                                                                                              |

| Interval                              | Duration                                         |
| ------------------------------------- | ------------------------------------------------ |
| Merge to deploy                       | 19 min                                           |
| Deploy to first failure               | 9 min                                            |
| **Deploy to detection**               | **12 h 00 m** (11 h 51 m from the first failure) |
| Detection to fix merged               | 26 min                                           |
| **Fix published to fix live**         | **24 h 16 m**                                    |
| **Total outage** (deploy to fix live) | **36 h 54 m**                                    |

## 3. Impact

- No automated review guide was produced from 07:27:08 on 09-27 until 20:21:12 on 09-28. The first success came at 20:26:00.
- In that window, the engine trace has 25 guide source captures and 25 generation enqueues. It also has **28 spawn attempts across 26 guide work items. All 28 used `driver=codex`, and the hook-trust gate refused every one.** No other guide failure mode appears in the window.
- The operator's working figure is "about 63 PRs captured but never enqueued for guides". **That figure could not be reproduced from the engine trace**, which shows only the 25 captures above. It may come from a surface this investigation did not read, such as the runtime database or the app's per-PR guide state.
- Either way, the failed attempts were not retried automatically after the fix went live. The guides from 20:26 onward are new attempts, and the trace shows no bulk re-enqueue.
- Worker code, PR review and merges were unaffected. Review guides are advisory, so reviewers merged PRs in the window without them.

## 4. Root cause

The worker and the hook-trust observer read the same `config.toml`. Only the worker received the flag that makes that file valid.

At `cffa1d2648`:

- [`codex.rs:702-714`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/driver/src/codex.rs#L702-L714) (`render_review_guide_config`) writes a `[permissions.review-guide]` profile and a `features.network_proxy` table. The profile extends `:read-only` and allows only the frontend Unix socket. The function does **not** write `default_permissions`.
- [`codex.rs:515-520`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/driver/src/codex.rs#L515-L520) (`codex_sandbox_extra_args`) selects the profile only as a worker command-line argument: `--config default_permissions="review-guide"`.
- [`codex.rs:1728-1737`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/driver/src/codex.rs#L1728-L1737) calls `write_hooks_and_attest` with that config _before_ the extra arguments exist. The attestation step, [`codex-hook-trust/src/lib.rs:635-641`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/codex-hook-trust/src/lib.rs#L635-L641), launches `codex app-server` with only `CODEX_HOME` set.
- Codex 0.153.4's config loader rejects that file. In [`core/src/config/mod.rs:3401-3412` at `rust-v0.153.4`](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/config/mod.rs#L3401-L3412), when profiles exist and none is selected, it returns ``config defines `[permissions]` profiles but does not set `default_permissions` ``. This investigation read that source directly. `app-server` then reports a `configWarning`, continues with the default config, and answers `hooks/list` with `hooks: []` and the error in an `errors` field. #3036's description reproduces this behavior.
- The gate, [`codex-hook-trust/src/lib.rs:715-719`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/codex-hook-trust/src/lib.rs#L715-L719), sees no hook entries and refuses with "silence is not success". **The gate behaved correctly.** A worker with no guards armed is exactly what it exists to refuse.

**The suspected cause was wrong.** The early theory was that Codex 0.153.4 did not support the `network_proxy` or `permissions` keys. #3036's provenance table cites 0.153.4's `config.schema.json` for every key the guide policy uses:

- `default_permissions`
- `permissions.*.extends`
- `.network.enabled`, `.domains` and `.unix_sockets`
- `features.network_proxy` and its fields
- `web_search`

The loader check above is the actual rejection. The fix, [`codex.rs:708` at `d2847b6627`](https://github.com/spinyfin/mono/blob/d2847b66276a25586e399d4ce5e4bf0f36d1a21c/tools/boss/engine/driver/src/codex.rs#L708), writes `default_permissions = "review-guide"` into the rendered file. The observer and the worker now load the same valid policy. #3039 then removed the redundant command-line override.

**A diagnosability defect slowed the investigation.** The observer discards `app-server` stderr ([`lib.rs:627`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/codex-hook-trust/src/lib.rs#L627)). The `hooks/list` parser ([`lib.rs:663-720`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/codex-hook-trust/src/lib.rs#L663-L720)) never reads the response's `errors` array. So Codex's precise, actionable message never reached any Boss log, and all 28 refusals say only "no hook entries". Both behaviors are unchanged on `main` at the time of writing.

## 5. Why review and tests missed it

- **The one test that reached the gate counts the gate's refusal as a pass.** [`codex_tests.rs:1371-1376`](https://github.com/spinyfin/mono/blob/cffa1d264838a6f709bbc4dccb79ed1ae7278689/tools/boss/engine/driver/src/codex_tests.rs#L1371-L1376):
  ```rust
  // The sandbox forbids launching a live Codex process; materialization must
  // finish before the separate hook-trust attestation reports that failure.
  let result = rt.block_on(driver.write_permission_config(&input, tmp.path()));
  if let Err(error) = result {
      assert!(error.to_string().contains("hook-trust gate"), "{error:#}");
  }
  ```
  A hook-trust refusal is the exact production failure, and this test treats it as an expected outcome. It then checks only the TOML shape: the profile, the socket and `web_search`. No real Codex ever loads the file. The other new tests assert the command-line arguments (that `default_permissions` is present) and the string contents of the rendered config.
- **No test ran the real CLI against the rendered config.** The fix PR, #3036, added exactly that test. [`config_compatibility_tests.rs:12`](https://github.com/spinyfin/mono/blob/d2847b66276a25586e399d4ce5e4bf0f36d1a21c/tools/boss/engine/driver/src/codex/config_compatibility_tests.rs#L12) is a non-optional Bazel test that runs checksum-pinned Codex 0.153.4 binaries through the production attestation gate, with no model credentials. It reproduces the old refusal and verifies the fix. Had this test existed before the merge, #3028 would have failed it.
- **Automated PR review (8 findings) checked the policy, not whether Codex could load it.** The findings covered permission breadth, the fail-open hook, link parsing, symlinks and wildcard metadata directories. All are security or correctness properties of the policy. None asked whether Codex would accept the file.
- **The PR disclosed that its final state was not fully tested.** It said: "Full-suite results below are from the preceding implementation revision; the merged branch will run its tests in CI." It claimed no live guide run.
- **No review guide existed for #3028, and one could not have helped.** The guide attempt at 09-26 22:01 was orphan-reaped in a loop and never finalized. More fundamentally, the _deployed_ engine generates a PR's guide, so a guide can never exercise the PR's own spawn configuration. Guides explain a change to a human; they do not test it.

## 6. Detection

### 6.1 Deploy to detection: 12 hours

Detection took 12 h 00 m from deploy (11 h 51 m from the first refusal), and it was human. The engine logged an ERROR with a precise-looking reason on every attempt, but nothing turned that into a signal the operator would see:

- **No attention item or alert.** The refusal marks the execution `failed` before any pane exists, and the trace shows no attention item for these failures. Twenty-eight consecutive refusals of the same kind, with zero successes for over a day, never crossed any threshold.
- **The app showed a failed guide without a reason.** Work to "Show why a review-guide or worker spawn failed in the app" ([mono#3037](https://github.com/spinyfin/mono/pull/3037)) was filed at the moment of detection. Its description says the app previously fell back to a generic "execution failed" label. **(inference:** the operator saw failed or missing guides but had nothing actionable until someone read the engine log.)
- **Guides are advisory.** Nothing waits on a guide, so a missing guide causes no downstream symptom. Most of the window was also overnight in CDT, from 02:36 to about 13:00. **(inference:** part of the gap is simply that nobody was looking.)

**A faster signal:** a spawn-success check per worker kind that raises one attention item when a kind has N consecutive pre-start failures and no success. The second or third refusal, around 07:45, would have raised an attention item carrying the refusal text. Better still, a post-deploy canary (§7, item 3) would have caught it within minutes of the 07:27 restart.

### 6.2 Fix published to fix live: 24 hours

#3036 merged 26 minutes after detection, and `boss-v1.0.686` published it at 20:05:23 on 09-27. The engine running at that time had started at 18:25:26 on a pre-fix build. It kept running until 20:21:12 on 09-28, and refused one more guide at 14:53:55 on 09-28.

That is **24 h 16 m** of outage after the fix was released, about two-thirds of the incident's total length. Nothing prompted a restart, for two reasons:

- The engine does not know which release it is running. Its audit `start` record says `engine_version: "0.0.0"`.
- Nothing compares the running build against fixes published since it started.

## 7. Recommendations

These are recommendations only. None of them has been filed as work by this document.

1. **Real-binary contract tests for everything Boss renders into a driver's config.**
   - Extend #3036's pinned-Codex attestation test so the pinned CLI loads every worker kind's rendered `config.toml`.
   - Add the equivalent for Claude and Grok settings files where their CLIs can validate them offline.
2. **A test must never accept the production failure as a pass.** Rewrite or delete assertions shaped like `codex_tests.rs:1371-1376`, which tolerated a hook-trust refusal. When a sandbox cannot run the real dependency, use a hermetic pinned binary, as #3036 did.
3. **Post-deploy canary per driver and worker kind.** After any engine restart onto a new build, dispatch one minimal execution for each combination of driver and worker kind (standard, PR review, review guide). Alert if any fails to reach its first turn. Here, the first guide spawn after 07:27 would have tripped it.
4. **Make the hook-trust refusal name the real cause.**
   - Parse the `hooks/list` `errors` array and the `configWarning` notifications, and include them in `TrustGateError`.
   - Keep a bounded tail of `app-server` stderr instead of discarding it (`lib.rs:627` and `lib.rs:1332` on `main`).

   The gate should keep refusing; only its message changes.

5. **Alert on streaks of pre-start failures.**
   - When a worker kind or driver sees N consecutive pre-start failures with no success in between, raise one attention item that includes the latest error text.
   - Land [mono#3037](https://github.com/spinyfin/mono/pull/3037), open at the time of writing, so the app shows the engine's reason instead of a generic "failed".
6. **Record the running build and surface drift.**
   - Have the engine's audit `start` record carry its build SHA and release tag, not `0.0.0`.
   - Have the app or the engine flag when the running engine is older than the newest published release, especially when the newer releases contain merged fixes.

   This alone might have cut a day off the incident.

7. **Recover guide attempts that failed before start.** When a guide attempt fails before spawn and the engine later restarts on a different build, re-enqueue those attempts, or at least list them. An outage should not leave a silent backlog of PRs without guides.
8. **Require one live spawn before merging spawn-path changes.** For a change to driver config or permission artifacts, a PR description that disclaims live validation should be a blocker. At minimum, run one real spawn of each affected worker kind in an isolated engine.

## 8. What went well, what went badly, lessons

**Went well**

- The hook-trust gate held. It refused to launch Codex review-guide workers whose PreToolUse guards it could not prove were armed, instead of launching them unguarded.
- #3036 found the actual cause and overturned the network-proxy theory. It reproduced the failure against the installed CLI and cited the upstream source. It also landed a hermetic real-binary regression test, merging 26 minutes after detection.
- The trace recorded every `spawn aborted` event with its full error text, so the failure's scope could be counted exactly afterwards.

**Went badly**

- A test was written to accept the exact failure that then happened in production.
- An ERROR repeated 28 times with zero successes for 12 hours, and nobody was told.
- Codex's actionable error message was discarded, so the first hypothesis was wrong.
- A fix sat published for a day because nothing knew the running engine was stale.

**Lessons**

- **A test that passes when the gate refuses is a test that cannot fail.** "The gate refused" is a failure, not a skip.
- **Failures that are not visible last until someone reads the logs.**
- **A merged fix is not a deployed fix.** Boss needs to be able to see its own deployment state.

## 9. What could not be established

- **Which build each engine ran.** The audit `start` records carry `engine_version: "0.0.0"`. The assignments (684 at 09-27 07:27:08, 685 at 18:25:26, 686 at 09-28 20:21:12) are inferred from the newest release published before each start.
- **The "about 63 PRs" figure** for guides captured but never enqueued. For this window the engine trace shows 25 captures, 25 enqueues and 28 refused spawns. This investigation did not read the runtime database, and the figure may come from a surface not examined here.
- **How the operator first noticed.** No alert fired. The detection time is when the defect was filed as work.
- **Why the pre-merge review-guide attempt for #3028 died.** Its Codex pane lost its shell PID and was orphan-reaped repeatedly on 09-26, before the change was deployed. That is a separate engine behavior and was not investigated here.
