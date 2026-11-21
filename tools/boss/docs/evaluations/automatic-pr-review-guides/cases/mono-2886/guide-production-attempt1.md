# PR #2886: Preserve engines when checks are inconclusive

This guide covers base `6f8ca35153d0fcae9aea5e9852894ea549989ca0` → head `68e51b6cfd5cb8ccac6473273d04d57fe972d7a4`. Links use revision-pinned source because exact diff anchors were not supplied.

## 1. Problem being solved

Boss could initiate engine replacement from evidence that did not establish a stale build or a dead process:

- **During startup**, an engine’s socket could be reachable while its fingerprint RPC returned no value. After three unsuccessful requests, the old implementation treated that uncertainty like a fingerprint mismatch and called `stopRunningEngine`.
- **During supervision**, a single failed liveness tick immediately consumed a restart attempt and scheduled recovery.

The first path could be particularly disruptive: the unchanged shutdown helper tries an RPC first, then can escalate to SIGTERM/SIGKILL if a suitable running engine PID is available and shutdown fails. The PR prevents an unavailable fingerprint from initiating that path. [Before: startup and shutdown logic](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/EngineProcessController.swift)

“Healthy” in the title is the motivation, not a newly verified property. The implementation preserves an engine discovered through socket reachability; it does not establish that all engine operations work.

## 2. Core fix / implementation

The change separates **unknown build identity**, **confirmed fingerprint mismatch**, and **repeated unreachability**:

| Evidence                                             | Revised behavior                                             |
| ---------------------------------------------------- | ------------------------------------------------------------ |
| Running fingerprint unavailable after three attempts | Log an inconclusive check and preserve the discovered engine |
| Running fingerprint matches the bundle               | Keep the existing engine                                     |
| Returned fingerprint differs from the bundle         | Stop the existing engine and proceed to launch               |
| Failed supervision tick below the threshold          | Record/log the failure without scheduling recovery           |
| Failure streak reaches the threshold                 | Apply the existing restart budget and backoff                |

The architecture remains split between `EngineProcessController`, which performs I/O and schedules work, and `EngineSupervisor`, which makes restart decisions. The policy gains `consecutiveFailureThreshold`, defaulting to **3** and clamped to at least **1**. The default polling interval remains one second; the threshold counts processed ticks, not elapsed seconds. No environment override for the new threshold is added. [Controller and policy](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift), [supervisor](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineSupervisor.swift)

## 3. Implementation walkthrough and worked example

**Start with the fingerprint decision in `start(resetRestartBudget:)`.** Discovery and fingerprint acquisition are unchanged: after finding a reachable endpoint, the controller computes the bundled fingerprint and requests the running fingerprint up to three times, passing a three-second timeout each time. It stops retrying at the first non-`nil` response.

The decisive change is the early return for an unavailable fingerprint. Condensed pseudocode, with logging omitted:

```swift
guard let runningFP else { return }       // Newly preserves unknown builds
if bundledFP == runningFP { return }
try stopRunningEngine(running)
// Continue to launch.
```

These returns leave the `withStartLock` **closure**, not the enclosing `start` method. Execution still reaches the existing supervision setup unless supervision has been stopped. Thus preservation does not disable subsequent monitoring. [Core fix and surrounding control flow](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift)

The scope is deliberately narrower than “never replace an unresponsive engine.” Explicit `forceRestart` handling occurs before this check, and manual `restart()` retains its stop-and-launch behavior. Conversely, an inconclusive check can preserve an older build: matching the bundle is no longer required when the running fingerprint is unavailable. The unchanged comment describing retries before “treating non-response as a mismatch” is stale; the new guard determines the behavior.

**Worked example: reachable socket, unavailable fingerprint.** This uses the setup of the added `testUnresponsiveReachableEngineIsKeptWithoutReplacement`, with `BOSS_ENGINE_CMD` assumed unset so version checking runs:

- The primary socket is reachable.
- The fixture supplies a readable bundled file containing `"bundled engine fixture"`.
- `forceRestart` and `stopOnExit` are false.
- The fake fingerprint method returns `nil` on every request.
- The fake shutdown succeeds, `waitForClose` returns true, and a launch handler records calls and would return PID `4242`.

| Step                             | Base behavior                              | Head behavior                                                       |
| -------------------------------- | ------------------------------------------ | ------------------------------------------------------------------- |
| Discover engine                  | Finds the primary socket                   | Same                                                                |
| Request fingerprint              | Three requests return `nil`                | Same                                                                |
| Decide whether to replace        | Falls through to the upgrade/shutdown path | Logs `version-check inconclusive` and returns from the lock closure |
| Observable calls in this fixture | One shutdown, followed by one launch       | **No shutdown and no launch**                                       |
| After the lock closure           | Reaches supervision setup                  | Reaches supervision setup                                           |

The new test directly asserts the three requests and absence of shutdown/launch calls. The base outcome above follows from tracing the old controller against the supplied fake; it is not a reported test execution. [Regression test and fixture](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift), [old decision](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/EngineProcessController.swift), [new decision](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift)

The contrasting boundary remains a returned mismatch. The existing stale-legacy-engine test supplies `"stale-engine"` and asserts shutdown of the legacy endpoint followed by launch on the primary endpoint. Unknown identity and a differing fingerprint now take different branches.

**Next, read the supervisor’s failure-streak handling.** It adds `consecutiveFailures` and an internal `.unreachable` action. After incrementing the counter and checking whether the budget is already exhausted, it applies:

```swift
guard consecutiveFailures >= policy.consecutiveFailureThreshold else {
    return .unreachable(
        consecutiveFailures: consecutiveFailures,
        requiredFailures: policy.consecutiveFailureThreshold
    )
}
consecutiveFailures = 0
```

Only after this gate does it calculate and consume the next restart attempt. A reachable tick immediately clears the failure streak; resetting the restart budget still requires the existing healthy interval, or an explicit `reset()`. Because reaching the threshold also clears the streak, subsequent restart attempts require another complete streak. The threshold likewise precedes the transition to `.gaveUp`. [Supervisor state transitions](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineSupervisor.swift)

**Finally, follow the action back into the controller.** The controller still uses the injected liveness probe when present, otherwise socket discovery. A false injected result does **not** trigger a second socket check. The new `.unreachable` branch logs the signal source and failure count, then returns without scheduling work or emitting a new supervision state.

At the threshold, the log describes repeated unreachability instead of asserting that the engine exited. Existing delayed recovery calls `start(resetRestartBudget: false)`, which rediscovers the engine and applies the startup logic above; scheduling recovery does not guarantee replacement. Polling checks are skipped while a restart is pending. [Controller integration](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift)

## 4. Tests and supporting infrastructure

**Added: two tests.**

- `testUnresponsiveReachableEngineIsKeptWithoutReplacement` asserts exactly three fingerprint requests to the primary socket, no shutdown requests, and no launch calls.
- `testDebouncesSingleUnreachableTickBeforeSchedulingRestart` asserts two `.unreachable` actions with counts `1/3` and `2/3`, followed by `.restart(attempt: 1, delay: 1)`. All calls use the same timestamp, reinforcing that this is a tick-count threshold.

[Controller regression](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift), [supervisor regression](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/EngineRestartPolicyTests.swift)

**Modified: three existing budget tests.** Exhaustion across dead ticks, replenishment after a healthy interval, and immediate restoration by `reset()` now explicitly set `consecutiveFailureThreshold: 1`. Their existing assertions remain focused on budget behavior rather than the new debounce. No tests are removed. [Modified tests](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/EngineRestartPolicyTests.swift)

**Fixture changes:** `FakeSocketControl.expectedFingerprint` becomes optional, and `Fixture` gains `fingerprintAvailable`, defaulting to true. Setting it false models a reachable socket whose fingerprint RPC yields no value. Existing request recording and launch injection are reused. No build or configuration changes appear in the supplied comparison. [Fixture changes](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift)

**Coverage limits:** the fingerprint fake returns immediately, so the regression verifies the decision after missing responses, not actual timeout timing. The added supervisor test checks the action sequence, not controller timer scheduling, logging, or a recovery tick interrupting a partial failure streak.

The PR description reports Bazel builds for the app library, `BossTestsShard3`, `Boss`, and installer release; a Bazel test run of `BossTestsShard3`; and `checkleft run`. This guide is based on the supplied source comparison; I did not run those checks.
