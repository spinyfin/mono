**1. Problem being solved**

[PR #2886](https://github.com/spinyfin/mono/pull/2886) prevents uncertain health checks from unnecessarily disrupting an existing Boss engine.

Previously, two signals triggered recovery too aggressively:

- **Unavailable fingerprint:** Startup discovered a reachable engine, requested its binary fingerprint three times, then treated continued non-response as grounds for replacement. That could send a busy engine through graceful shutdown and, if shutdown also failed, the existing SIGTERM/SIGKILL fallback.
- **One failed liveness check:** Supervision immediately scheduled a restart after a single negative observation, even though a disconnected client does not establish that the engine process exited.

The new contract preserves an engine when its version is unknown and requires sustained unreachability before scheduling recovery.

**2. Core implementation**

Start with [`EngineProcessController.swift`, around line 187](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L187). The three fingerprint attempts, each passing a three-second timeout, already existed. The decisive change is what happens afterward.

The new control flow is, with logging omitted:

```swift
guard let runningFP else {
    return
}
if bundledFP == runningFP {
    return
}
try stopRunningEngine(running)
```

This separates three outcomes:

| Fingerprint result               | Previous behavior    | New behavior                                    |
| -------------------------------- | -------------------- | ----------------------------------------------- |
| Matches bundled engine           | Keep existing engine | Keep existing engine                            |
| Differs from bundled engine      | Stop and replace     | Stop and replace                                |
| Unavailable after three attempts | Stop and replace     | Keep existing engine; log an inconclusive check |

“Unavailable” includes more than timeouts: the [socket helper](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/BossEnginePaths.swift#L280) also returns `nil` for an unsuccessful request, an unexpected response type, or a missing fingerprint field. None proves a version mismatch.

**Worked example, drawn directly from the new regression test**

[`testUnresponsiveReachableEngineIsKeptWithoutReplacement`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L224) supplies this state:

- The primary socket is reachable.
- A bundled fixture binary exists and can be fingerprinted.
- The fake socket control returns `nil` for every fingerprint request.
- Forced restart is disabled; shutdown and launch calls are recorded.

Calling `controller.start()` proceeds as follows:

| Step                | Before                                                                                | After                                                          |
| ------------------- | ------------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| Discover engine     | Finds the reachable primary socket                                                    | Same                                                           |
| Request fingerprint | Three requests all return `nil`                                                       | Same                                                           |
| Decide              | Falls through to the replacement path                                                 | Enters the new `guard` and returns from the start-lock closure |
| Observable effect   | With this fake’s successful shutdown, records one shutdown and one replacement launch | Records neither shutdown nor launch                            |

The test asserts exactly three fingerprint requests, zero shutdown requests, and zero launches. Its fake returns immediately, so it tests the decision after non-response, not actual timeout timing.

An important control-flow detail: that `return` exits the closure passed to `withStartLock`, not the entire `start(resetRestartBudget:)` method. Normal supervision setup still follows.

The contrasting boundary is already covered by `testStaleLegacyEngineIsStoppedBeforeReplacementUsesStateRootSocket`: a returned fingerprint of `"stale-engine"` still causes shutdown of the legacy endpoint and a replacement launch on the primary endpoint. **Unknown version is tolerated; a confirmed mismatch still triggers replacement.**

**3. Major logic and architecture changes**

The existing separation remains: `EngineSupervisor` makes decisions, while `EngineProcessController` performs I/O and schedules work.

In [`EngineSupervisor.swift`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineSupervisor.swift#L32), a new `consecutiveFailures` counter and `.unreachable` action introduce an intermediate state before restart:

```swift
guard consecutiveFailures >= policy.consecutiveFailureThreshold else {
    return .unreachable(
        consecutiveFailures: consecutiveFailures,
        requiredFailures: policy.consecutiveFailureThreshold
    )
}
consecutiveFailures = 0
```

The counter increments on failed observations. A healthy observation or explicit `reset()` clears it. Reaching the threshold also clears it before entering the existing restart-budget logic, so subsequent restart attempts require another sequence of failures.

The [policy](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L9) defaults to **three failures**, clamped to at least one. Existing defaults remain: one-second polling, six restart attempts, delays of `1, 2, 4, 8, 16, 30` seconds, and budget replenishment after 60 seconds of observed health. This is a count of observations, not an elapsed-time deadline.

The [controller’s action handling](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L393) now:

- Logs `.unreachable` observations without scheduling work or publishing a new UI supervision state.
- Identifies the source as either `"engine client connection"` or `"socket probe"`.
- Describes restart evidence as consecutive unreachable ticks instead of claiming “engine exited.”

The source-selection behavior itself is unchanged: production uses `EngineClient.isReachable`, wired in `ChatViewModel`; socket discovery is the fallback when no probe closure exists.

A scheduled restart still calls `start(resetRestartBudget: false)`. It therefore rediscovers the engine and applies the version-check rules; scheduling recovery does not itself unconditionally terminate a reachable engine.

One review cleanup: the comment immediately above the fingerprint retry loop still says non-response will be treated as a mismatch. That now contradicts the implementation.

**4. Tests and supporting infrastructure**

Two tests were added:

- **Controller regression:** The worked example above checks three fingerprint requests and no shutdown or replacement.
- **Supervisor regression:** [`testDebouncesSingleUnreachableTickBeforeSchedulingRestart`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/EngineRestartPolicyTests.swift#L75) checks this exact sequence:

  ```swift
  .unreachable(consecutiveFailures: 1, requiredFailures: 3)
  .unreachable(consecutiveFailures: 2, requiredFailures: 3)
  .restart(attempt: 1, delay: 1)
  ```

Supporting changes are limited to existing test fixtures:

- `FakeSocketControl.expectedFingerprint` becomes optional.
- `Fixture` gains `fingerprintAvailable`, defaulting to `true`, so socket reachability and fingerprint availability can vary independently.
- Three existing budget/reset tests explicitly set `consecutiveFailureThreshold: 1`, preserving their focus on retry-budget behavior.

No build-system or test-runner files changed. The new supervisor test exercises pure decisions at a fixed timestamp; it does not test timer execution or recovery between failed ticks.

The PR description reports Bazel builds, `BossTestsShard3`, installer/application builds, and `checkleft run` validation. I inspected the code and before/after changes; I did not rerun those checks.
