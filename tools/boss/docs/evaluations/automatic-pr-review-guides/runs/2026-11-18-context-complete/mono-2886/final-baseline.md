**Orientation.** This PR makes Boss more conservative about replacing an existing engine: an unavailable version fingerprint no longer justifies replacement, and automatic recovery requires three consecutive failed liveness checks. The change is confined to two production files and two test files.

**1. The problem being solved**

Boss keeps its engine running independently of the macOS app. When the app starts, it discovers an existing engine and compares its binary fingerprint with the bundled engine’s fingerprint.

Previously, three unsuccessful fingerprint requests triggered the same replacement path as a confirmed mismatch. A reachable engine that could not answer the version RPC could therefore receive a shutdown request—and potentially reach the existing signal-based termination fallback.

A separate supervision path also scheduled recovery after a single failed liveness check. A temporary connection failure immediately consumed a restart attempt.

These checks establish different things: socket reachability shows that a connection can be established; a successful fingerprint response identifies the running build. Missing the latter does not prove that the build is stale.

**2. The core fix, with a worked example**

Start with [the version-check branch in EngineProcessController.swift](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L180). The existing three attempts, each using a three-second timeout argument, remain. The decisive addition is:

```swift
guard let runningFP else {
    emit("[engine version-check inconclusive] running fingerprint unavailable after \(fingerprintAttempts) attempts bundled=\(bundledFP) — keeping reachable \(describe(running))")
    return
}
```

Only an available fingerprint proceeds to equality comparison and possible replacement.

The new contract is:

| Fingerprint outcome              | Action                                     |
| -------------------------------- | ------------------------------------------ |
| Unavailable after three attempts | Keep the discovered engine                 |
| Matches the bundled fingerprint  | Keep the discovered engine                 |
| Available and different          | Stop the engine, then launch a replacement |

A concrete example comes directly from [the new controller regression test](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L224).

**Input and relevant state:** the fixture’s primary socket is reachable, forced restart is disabled, a bundled fixture binary can be fingerprinted, and every running-engine fingerprint request returns `nil`. A launch recorder captures any replacement launch.

1. Discovery finds the primary socket. A missing PID does not prevent discovery.
2. The controller computes the bundled fingerprint.
3. All three fingerprint calls return `nil`.
4. **Before:** the matching-fingerprint branch fails, execution reaches `stopRunningEngine`, and the fake shutdown succeeds. The controller then invokes the replacement launch handler.
5. **After:** the new guard logs an inconclusive check and returns from the `withStartLock` closure. Shutdown and launch are skipped.
6. Execution continues after that closure to enable supervision. Returning here does **not** bypass supervision.

The test’s observable result is explicit:

```swift
XCTAssertEqual(fixture.socketControl.fingerprintRequests,
               Array(repeating: fixture.paths.socketPath, count: 3))
XCTAssertTrue(fixture.socketControl.shutdownRequests.isEmpty)
XCTAssertTrue(launchRecorder.socketPaths.isEmpty)
```

The old outcome above is traced from the merge-base implementation; the new test was not present there.

The useful contrasting case is an actual mismatch. The existing `testStaleLegacyEngineIsStoppedBeforeReplacementUsesStateRootSocket` supplies `"stale-engine"` and verifies shutdown on the legacy socket followed by launch on the primary socket. That replacement behavior remains intact.

**3. Major logic and architecture changes**

Read [EngineSupervisor.swift](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineSupervisor.swift#L32) next. It remains a pure decision-making component; the controller still performs scheduling and process operations.

The supervisor gains a failure counter and an intermediate action:

```swift
case unreachable(consecutiveFailures: Int, requiredFailures: Int)
```

For failed checks, the new gate precedes the existing restart-budget logic:

```swift
consecutiveFailures += 1
guard !restartBudgetExhausted else {
    return .wait
}
guard consecutiveFailures >= policy.consecutiveFailureThreshold else {
    return .unreachable(
        consecutiveFailures: consecutiveFailures,
        requiredFailures: policy.consecutiveFailureThreshold
    )
}
consecutiveFailures = 0
```

With the default threshold, the sequence is:

| Consecutive failed check | Supervisor result                                         |
| ------------------------ | --------------------------------------------------------- |
| First                    | `.unreachable(1, 3)`                                      |
| Second                   | `.unreachable(2, 3)`                                      |
| Third                    | Schedule a restart, subject to the existing attempt limit |

A successful check immediately clears the failure streak. Explicit `reset()` also clears it. Reaching the threshold clears the streak before checking the restart budget, so subsequent recovery attempts require another run of failures.

[EngineRestartPolicy](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L9) supplies the new threshold, defaults it to three, and clamps it to at least one. The existing one-second polling interval, six-attempt limit, backoff schedule, and 60-second healthy interval for replenishing the restart budget remain. The new threshold has no environment-variable override.

Then inspect [the controller’s action handling](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L392):

- `.unreachable` logs the failure count and returns without scheduling recovery or changing the supervision UI state.
- `.restart` retains the existing delayed work item and `.restarting` state.
- Checks are skipped while a restart is pending. This is a count of sampled failures, not an exact elapsed-time guarantee.

The liveness signal itself is unchanged. Production wiring uses [`EngineClient.isReachable`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/ChatViewModel.swift#L1215); controllers without that closure use socket discovery. The change labels these sources in diagnostic messages. Restart messages now describe repeated unreachability instead of asserting that the process exited.

A scheduled recovery still calls `start(resetRestartBudget: false)`, which discovers and version-checks an existing engine again. Scheduling recovery therefore does not itself guarantee a process replacement.

Two details deserve reviewer attention:

- The comment above the fingerprint retry loop still says non-response is treated as a mismatch; it now contradicts the implementation.
- Although the PR description calls these production logs, they use the existing system-message callback. [`appendSystemMessage`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/ChatViewModel.swift#L2384) suppresses ordinary messages unless system messages are enabled. This PR does not add an always-on logging destination.

**4. Tests and supporting infrastructure**

Two tests are added:

- **`testUnresponsiveReachableEngineIsKeptWithoutReplacement`** verifies three fingerprint requests, zero shutdown requests, and zero launches.
- **[`testDebouncesSingleUnreachableTickBeforeSchedulingRestart`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/EngineRestartPolicyTests.swift#L75)** verifies two intermediate `.unreachable` actions followed by the first restart action with a one-second delay.

The supporting changes are small. `FakeSocketControl.expectedFingerprint` becomes optional, and the fixture gains `fingerprintAvailable: Bool = true`. This lets tests represent a reachable socket whose fingerprint RPC provides no result while preserving existing fixture defaults. Three existing restart-budget tests explicitly choose threshold `1`, keeping their focus on budget exhaustion and reset behavior.

No build definitions or test runners change. The existing shard configuration includes these tests.

The coverage is narrower than a real timeout/reconnection scenario: the fake returns `nil` immediately, and the supervisor test calls `tick` directly with the same timestamp. The added tests do not exercise actual RPC delays, controller timer scheduling, or a successful check interrupting a partial failure streak.

The PR description lists the app/test builds, `BossTestsShard3`, installer build, and `checkleft run` as validation. I inspected the source and merge-base diff; I did not rerun those commands.

https://github.com/spinyfin/mono/pull/2886
