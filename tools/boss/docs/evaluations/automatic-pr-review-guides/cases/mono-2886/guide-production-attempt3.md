# PR #2886: Preserve reachable engines when checks are inconclusive

This guide covers the complete comparison from `6f8ca35153d0` to `68e51b6cfd5c`. Links use revision-pinned source locations because validated line-level PR diff URLs were unavailable.

## 1. Problem being solved

Boss could initiate engine recovery on insufficient evidence in two places:

- **At startup:** after discovering a reachable engine, three unsuccessful fingerprint RPCs led to the same shutdown-and-replacement path as a confirmed fingerprint mismatch.
- **During supervision:** one failed liveness tick immediately scheduled a restart attempt.

These signals do not establish that the engine has exited or is running an outdated build. Socket reachability only establishes that a connection succeeds; fingerprint retrieval separately requires a suitable RPC response. A missing fingerprint can also reflect an unexpected response type or absent fingerprint field. See the [socket and fingerprint helpers](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/BossEnginePaths.swift#L256-L291).

The startup behavior was particularly consequential: replacement enters the existing shutdown machinery, which can escalate from RPC shutdown to validated PID-based signals.

## 2. Core fix / implementation

The change introduces two separate protections:

| Decision                                                       | Before                        | After                                         |
| -------------------------------------------------------------- | ----------------------------- | --------------------------------------------- |
| Reachable engine, fingerprint unavailable after three attempts | Attempt replacement           | Keep the discovered engine                    |
| Fingerprints available and different                           | Attempt replacement           | Same                                          |
| Failed supervision tick                                        | Immediately schedule recovery | Require three consecutive failures by default |

The architecture remains split between **`EngineSupervisor`**, which makes pure state-machine decisions, and **`EngineProcessController`**, which probes, logs, schedules work, and manages processes.

The new contract favors preserving a reachable engine when its version is unknown. It does **not** prove that engine is healthy or matches the bundle.

## 3. Implementation walkthrough and worked example

**Start with the fingerprint decision in `EngineProcessController.start`.** The existing loop still makes up to three fingerprint requests, each receiving a three-second timeout argument, and stops at the first non-`nil` result. The decisive addition is:

```swift
guard let runningFP else {
    emit("[engine version-check inconclusive] running fingerprint unavailable after \(fingerprintAttempts) attempts bundled=\(bundledFP) — keeping reachable \(describe(running))")
    return
}
```

Only an available, unequal fingerprint now reaches `stopRunningEngine`. Equal fingerprints retain the engine as before. See the [core branch](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L186-L205).

Two control-flow details matter for review:

- This `return` exits the **`withStartLock` closure**, so `start` still proceeds to enable supervision unless supervision has been stopped. It does not abandon supervision.
- Reachability was established during discovery; the new branch does not recheck it after the fingerprint attempts.

The [surrounding startup path](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L142-L219) preserves existing exceptions: forced restart acts earlier, `BOSS_ENGINE_CMD` or no bundled engine skips comparison, and inability to fingerprint the bundled file throws. The unchanged comment about eventually “treating non-response as a mismatch” is now stale; the guard defines the behavior.

**Worked example: a reachable engine returns no fingerprint.** This scenario comes directly from the [new controller regression test](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L224-L238).

Relevant fixture state:

- The primary socket reports reachable.
- A temporary bundled file contains `"bundled engine fixture"` and can be fingerprinted.
- `fingerprintAvailable: false` makes every fingerprint request return `nil`.
- Forced restart is disabled; shutdown and launch calls are recorded. The fake shutdown succeeds and `waitForClose` returns `true`.

Assuming `BOSS_ENGINE_CMD` is unset so comparison runs, the trace is:

| Step                                | Base revision                                               | Head revision                                       |
| ----------------------------------- | ----------------------------------------------------------- | --------------------------------------------------- |
| Discover engine                     | Primary socket is reachable                                 | Same                                                |
| Retrieve fingerprint                | Three requests return `nil`                                 | Same                                                |
| Decide                              | Log upgrade and call shutdown                               | Log inconclusive check and return from lock closure |
| Observable result with this fixture | One shutdown request, then one launch on the primary socket | No shutdown requests and no launch calls            |

The old result follows from the [base replacement branch](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/EngineProcessController.swift#L186-L215) and fixture behavior; the new result is explicitly asserted by the added test. This example uses immediate fake `nil` responses, not real three-second timeouts.

The useful contrasting case is a returned `"stale-engine"` fingerprint: the unchanged [legacy-engine replacement test](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L207-L222) still expects shutdown on the legacy socket and replacement launch on the primary socket.

**Next, read the supervisor’s failure counter.** `EngineRestartPolicy` adds `consecutiveFailureThreshold`, defaulting to three and clamped to at least one. No environment override is added for this field. See the [policy definition](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L9-L51).

Before consuming a restart attempt, the supervisor now checks:

```swift
guard consecutiveFailures >= policy.consecutiveFailureThreshold else {
    return .unreachable(
        consecutiveFailures: consecutiveFailures,
        requiredFailures: policy.consecutiveFailureThreshold
    )
}
consecutiveFailures = 0
```

The [complete state transition](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineSupervisor.swift#L25-L65) makes the failures consecutive: a successful tick clears the counter immediately. Reaching the threshold also clears it, so subsequent attempts require another failure streak. Restart-budget replenishment remains separate and requires the existing healthy interval—60 seconds by default.

With an unused default budget, three failed calls produce `.unreachable(1, 3)`, `.unreachable(2, 3)`, then `.restart(attempt: 1, delay: 1)`. This is a **tick-count threshold**, not an elapsed-time guarantee.

**Finally, follow the action back into the controller.** The [controller integration](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L393-L447) logs intermediate failures without scheduling work or emitting a new supervision state. On threshold crossing, it emits `.restarting` and schedules the existing delayed recovery path.

The liveness source is unchanged: production [wires in `EngineClient.isReachable`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/ChatViewModel.swift#L1215-L1228), which tracks [connection state](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineClient.swift#L107-L136). Socket discovery is used only when no probe is installed; a false client result does not trigger a second socket check.

Logs now name the source on intermediate failures and describe consecutive unreachability instead of asserting “engine exited.” Scheduled recovery still calls `start(resetRestartBudget: false)`, so it rediscovers the engine and applies the fingerprint rules rather than unconditionally replacing it.

## 4. Tests and supporting infrastructure

**Added: two tests.**

- [`testUnresponsiveReachableEngineIsKeptWithoutReplacement`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L224-L238) asserts exactly three fingerprint requests, no shutdown, and no launch.
- [`testDebouncesSingleUnreachableTickBeforeSchedulingRestart`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/EngineRestartPolicyTests.swift#L75-L90) asserts two intermediate unreachable actions followed by attempt one with a one-second delay. All calls use the same timestamp, confirming count-based decision logic.

**Modified: three existing policy tests.** Budget exhaustion, healthy-interval replenishment, and explicit reset now set `consecutiveFailureThreshold: 1`, preserving their original immediate-retry assertions while isolating budget behavior. See the [modified tests](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/EngineRestartPolicyTests.swift#L32-L73). No tests were removed.

**Fixture changes:** the fake socket controller’s expected fingerprint becomes optional, and `Fixture` gains `fingerprintAvailable`, defaulting to `true`. This allows missing responses without changing existing matching/mismatching fixtures. See the [fake](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L291-L331) and [fixture setup](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L347-L408).

There are no build/configuration changes. Both test files already belong to the existing [Shard3 catch-all source glob](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/BUILD.bazel#L278-L291).

Coverage is focused: the additions do not exercise real RPC timeouts, recovery between failed ticks, or controller timer scheduling and logs end to end. Those behaviors are inferred from implementation, not established by the new assertions.

I inspected the complete revision comparison and relevant helpers/callers; I did not run tests. The supplied PR description reports successful Bazel builds for the app library, Shard3, Boss, and installer release, plus `bazel test //tools/boss/app-macos:BossTestsShard3` and `checkleft run`.
