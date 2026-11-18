# PR #2886: Preserve reachable engines when checks are inconclusive

## Orientation

Boss should keep using a reachable background engine when a build check cannot produce an answer, avoiding an unnecessary replacement. The engine is the separate process that the Boss macOS app connects to; it can remain running across app launches. This bug fix changes the app component responsible for starting, reusing, and recovering that process.

Previously, an unanswered build check could cause Boss to stop the existing engine and launch another. Now, an unavailable answer preserves the discovered engine; a confirmed build mismatch still triggers replacement. During ongoing monitoring, Boss also waits for three consecutive unreachable checks before scheduling recovery, instead of reacting to the first failed check.

A **fingerprint** is a short identifier derived from the engine executable, used to compare the running engine with the copy shipped in the app. An **RPC** is a request sent to the engine for a response. **Supervision** is periodic reachability monitoring; each check is a **tick**, and **backoff** is the delay before a recovery attempt.

## Change overview

The change separates two kinds of evidence:

- **Build identity:** three unsuccessful fingerprint requests mean “inconclusive,” while an actual unequal fingerprint means “replace.”
- **Reachability:** one or two failed supervision ticks mean “keep waiting,” while the third consecutive failure permits a recovery attempt.

The [startup decision](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L186-L205) preserves the existing three attempts, each with a three-second timeout. The [restart policy](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L9-L35) adds a default consecutive-failure threshold of three, clamped to at least one. Its existing one-second polling interval, six-attempt budget, delay schedule, and 60-second healthy-reset interval remain.

## Core implementation

The existing division of responsibilities remains: `EngineProcessController` discovers and launches processes, while `EngineSupervisor` makes recovery decisions from reachability observations and retry history. The supervisor adds a failure counter and an `.unreachable(consecutiveFailures:requiredFailures:)` action; the controller handles that action by logging and returning without scheduling recovery.

The normal app caller [starts the controller before starting its engine client](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/ChatViewModel.swift#L1957-L1977). It also [supplies the existing client connection as the supervision signal](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/ChatViewModel.swift#L1215-L1228). That signal becomes true when the connection is ready and false on waiting, failure, or cancellation ([connection transitions](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineClient.swift#L107-L139)). Without an injected signal, the controller discovers an engine through a fresh local socket connection—a socket is the local communication endpoint between the app and engine.

## Implementation walkthrough

### 1. Keep “unknown build” separate from “different build”

[Discovery](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L628-L646) first establishes that an endpoint accepts a connection. The subsequent [fingerprint helper](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/BossEnginePaths.swift#L280-L291) returns an optional string: an unsuccessful request, unexpected response type, or absent string fingerprint yields `nil`.

At the merge base, [both missing and unequal fingerprints reached the stop-and-replace path](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/EngineProcessController.swift#L186-L204). The new guard returns when the fingerprint remains unavailable. The resulting decision is:

```text
Condensed pseudocode:
request running fingerprint up to three times
if no fingerprint:
    log inconclusive; keep discovered engine
else if running fingerprint equals bundled fingerprint:
    keep discovered engine
else:
    stop existing engine; launch replacement
```

The [actual branch and launch continuation](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L186-L219) matter together: the early return exits the start-lock closure, so the outer method still enables supervision. It skips replacement, not ongoing monitoring.

This contract applies after discovery and the normal version-check prerequisites. [Forced restart, a custom engine command, absence of a bundled executable, and failure to fingerprint that executable](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L143-L178) retain their existing branches.

### 2. Worked example: reachable engine, unanswered fingerprint requests

The [new regression test](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L224-L238) provides a concrete input: the primary endpoint is reachable, the fixture supplies a bundled executable, and `fingerprintAvailable: false` makes every fingerprint request return `nil`. The [fixture](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L373-L403) disables forced restart and records launch requests.

1. Discovery finds the reachable endpoint.
2. Both versions request its fingerprint three times without obtaining a value.
3. Previously, execution fell through to `stopRunningEngine` and then replacement launch.
4. Now, the missing-value guard logs an inconclusive check and returns from the closure. The observable test result is three fingerprint requests, **zero shutdown requests, and zero launches**.

The contrasting boundary is a positive mismatch: the [existing stale-engine test](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L209-L222) supplies `"stale-engine"` and still expects shutdown of the older endpoint and replacement launch on the primary endpoint.

### 3. Require consecutive failures before spending a recovery attempt

Previously, [one failed tick immediately advanced the restart budget](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/EngineSupervisor.swift#L43-L54). Now, after incrementing the failure counter and checking whether the budget is exhausted, the supervisor applies this gate:

```swift
guard consecutiveFailures >= policy.consecutiveFailureThreshold else {
    return .unreachable(
        consecutiveFailures: consecutiveFailures,
        requiredFailures: policy.consecutiveFailureThreshold
    )
}
consecutiveFailures = 0
```

[Source](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineSupervisor.swift#L47-L65). Only after this gate does it increment the attempt count or report that the budget has run out. A reachable tick immediately clears the failure streak; replenishing the retry budget still requires the configured healthy interval. An explicit reset clears both counters ([reset and healthy handling](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineSupervisor.swift#L25-L44)).

With the default threshold, failed observations produce `unreachable(1, 3)`, `unreachable(2, 3)`, then `restart(attempt: 1, delay: 1)`. An intervening reachable observation starts the streak over. This is a count of observations, rather than an elapsed-time test.

### 4. Turn the decision into recovery and diagnostic output

The [controller action handler](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L393-L438) logs the signal source—“engine client connection” or “socket probe”—and failure count for deferred recovery. At the threshold, the message reports consecutive unreachability instead of claiming the process exited, then emits the restarting state and schedules the existing delayed work.

Ticks are skipped while a restart is pending. When the delay expires, recovery calls `start(resetRestartBudget: false)`, preserving the attempt budget and reusing the same discovery/version-check rules. Scheduling recovery therefore does not itself guarantee that a process will be replaced. Output continues through the [existing system-message callback](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/ChatViewModel.swift#L1275-L1281).

## Tests and test infrastructure

**Added tests:**

- [`testUnresponsiveReachableEngineIsKeptWithoutReplacement`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L224-L238) asserts three fingerprint requests and no shutdown or launch.
- [`testDebouncesSingleUnreachableTickBeforeSchedulingRestart`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/EngineRestartPolicyTests.swift#L75-L90) feeds three failed ticks at the same timestamp. It asserts counts one and two, then the first restart with a one-second delay.

**Modified tests:** the [three existing budget tests](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/EngineRestartPolicyTests.swift#L32-L73) explicitly set the threshold to one. Their assertions continue covering budget exhaustion followed by waiting, replenishment after 60 healthy seconds, and immediate restoration through reset. No tests were removed.

**Fixture changes:** [`FakeSocketControl`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L291-L322) now accepts an optional fingerprint. The fixture adds `fingerprintAvailable`, defaulting to true, so reachability and fingerprint availability can vary independently. No build configuration changes are included; both test files remain covered by the [existing Shard3 source collection](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/BUILD.bazel#L278-L291).

## Evidence

The PR description reports the following validation:

- The author ran `bazel build //tools/boss/app-macos:boss_mac_app_lib //tools/boss/app-macos:BossTestsShard3`.
- The author ran `bazel test //tools/boss/app-macos:BossTestsShard3`.
- The author ran `bazel build //tools/boss/app-macos:Boss //tools/boss/installer:release`.
- The author ran `checkleft run`.

No screenshots are registered.
