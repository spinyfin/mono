# Preserving reachable Boss engines when checks are inconclusive

## Orientation

Boss should keep a working background service available instead of replacing it merely because a version check gets no answer. This bug fix changes the macOS app’s engine management: it preserves a reachable service when its version cannot be determined, still replaces a confirmed different build, and waits for repeated failed reachability checks before scheduling recovery.

The **engine** is the separate background process the app communicates with; it can survive an app restart. A **socket** is the local communication endpoint used to reach it. A **fingerprint** is an identifier calculated from the engine executable, used to compare the running build with the one bundled in the app. An **RPC** is a request sent to that separate process, such as asking for its fingerprint. **Supervision** is the app’s periodic reachability checking; each check is a **tick**.

## Change overview

Two decisions become less aggressive:

- At startup, three unsuccessful fingerprint requests previously led to stopping the reachable engine. They now produce an “inconclusive” message and retain it. A returned fingerprint that differs from the bundled fingerprint still triggers replacement.
- During supervision, one failed reachability tick previously scheduled recovery. The default now requires three consecutive failed ticks. Intermediate failures identify the signal source and failure count in diagnostic messages.

These are distinct checks: being reachable does not establish that the running build matches, and a failed reachability signal does not establish that the process exited. The changed [startup branch](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L186-L205) and [supervision dispatch](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L393-L424) preserve that distinction.

## Core implementation

The existing division of responsibility stays intact. `EngineProcessController` performs discovery, version requests, process operations, and timer scheduling. `EngineSupervisor` is the decision-making state machine: it receives a reachability boolean and a time, then returns an action. `EngineRestartPolicy` supplies the limits and **backoff**, the delay before each recovery attempt.

The policy adds `consecutiveFailureThreshold`, defaulting to three and clamped to at least one. Existing defaults remain one-second polling, six recovery attempts, delays of 1/2/4/8/16/30 seconds, and a 60-second healthy interval for replenishing the attempt allowance. See [policy initialization](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L9-L39).

## Implementation walkthrough

### 1. Treat an unavailable fingerprint as an unknown version

Discovery first finds a reachable socket. The unchanged [socket helper](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/BossEnginePaths.swift#L256-L291) separates connecting successfully from obtaining a fingerprint: the latter can return `nil` when the request yields no usable response, the response type is wrong, or the fingerprint field is absent.

Startup retains its three fingerprint attempts, each with a three-second timeout argument. The decisive new branch is:

```swift
guard let runningFP else {
    emit("[engine version-check inconclusive] running fingerprint unavailable after \(fingerprintAttempts) attempts bundled=\(bundledFP) — keeping reachable \(describe(running))")
    return
}
```

In the [surrounding code](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L186-L219), this return exits the startup-lock closure, avoiding shutdown and launch; startup still proceeds to enable supervision unless supervision has been stopped. Matching fingerprints also return, while a nonmatching returned fingerprint reaches `stopRunningEngine`.

The existing [startup exceptions](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L143-L178) remain: forced restart acts before the version check, custom-engine configuration or an absent bundled engine skips comparison, and failure to fingerprint the bundled file throws an error.

### 2. Worked example: reachable service, unanswered version requests

The [new controller regression test](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L224-L238) supplies a reachable primary socket, a bundled executable fixture, and a fake fingerprint service that returns `nil`.

1. Discovery succeeds, so startup enters version comparison.
2. All three fingerprint requests return `nil`.
3. **Before:** the [merge-base branch](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/EngineProcessController.swift#L182-L207) treated this as replacement evidence and called `stopRunningEngine`, then launched a replacement if stopping succeeded.
4. **After:** the new guard logs the inconclusive result and leaves the existing process alone. The test asserts exactly three fingerprint requests, no shutdown requests, and no launches.

The observable effect is that a missing version answer no longer causes the app to replace this reachable engine. The contrasting case remains explicit: the existing [stale-build test](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L209-L222) returns `"stale-engine"` from the legacy socket—the older supported endpoint—and asserts shutdown there followed by launch on the primary socket.

### 3. Require consecutive failures before spending a recovery attempt

Previously, the supervisor proceeded from a failed tick directly to [attempt accounting](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/EngineSupervisor.swift#L43-L55). It now increments a failure counter and, while the recovery allowance is not exhausted, applies this gate:

```swift
guard consecutiveFailures >= policy.consecutiveFailureThreshold else {
    return .unreachable(
        consecutiveFailures: consecutiveFailures,
        requiredFailures: policy.consecutiveFailureThreshold
    )
}
consecutiveFailures = 0
```

The [full transition logic](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineSupervisor.swift#L25-L65) clears the streak on any healthy tick or explicit reset. Reaching the threshold clears the streak before accounting for an attempt, so later recovery attempts also require a new streak. Exhausted supervision still returns `.wait`; sustained health still restores the allowance.

The new test makes the default threshold concrete: failed ticks produce `.unreachable(1, 3)`, then `.unreachable(2, 3)`, then `.restart(attempt: 1, delay: 1)`. This is a count of checks, not a requirement that exactly three seconds elapse.

### 4. Connect the decision to scheduling and diagnostics

The app already [supplies its engine-client reachability signal](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/ChatViewModel.swift#L1215-L1226). The controller continues to use that signal when supplied, otherwise discovering a reachable socket. It now labels the source “engine client connection” or “socket probe.”

An intermediate `.unreachable` action only logs the source and failure count; it neither schedules recovery nor emits a new supervision state. At the threshold, the message describes consecutive unreachability instead of claiming “engine exited,” and the existing delayed recovery work is scheduled. Checks are skipped while that work is pending.

Crucially, recovery calls `start(resetRestartBudget: false)`, so it reuses discovery and the revised version decision rather than unconditionally killing a process. These behaviors are together in the [controller’s action handling and recovery callback](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/EngineProcessController.swift#L393-L435). Diagnostic output uses the existing [system-message callback](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Sources/ChatViewModel.swift#L1275-L1281).

## Tests and supporting infrastructure

**Added tests:**

- [`testUnresponsiveReachableEngineIsKeptWithoutReplacement`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L224-L238) checks three unanswered fingerprint requests without shutdown or launch.
- [`testDebouncesSingleUnreachableTickBeforeSchedulingRestart`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/EngineRestartPolicyTests.swift#L75-L90) checks two intermediate failure actions followed by recovery on the third consecutive failure.

**Modified tests:** Three existing supervisor tests explicitly set the threshold to one, preserving their focus on attempt exhaustion, restoration after 60 seconds of health, and immediate restoration after reset. Their [assertions remain focused on those behaviors](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/EngineRestartPolicyTests.swift#L32-L73). No tests are removed.

**Fixture changes:** [`FakeSocketControl`](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L291-L329) now accepts an optional fingerprint. The [fixture initializer](https://github.com/spinyfin/mono/blob/68e51b6cfd5cb8ccac6473273d04d57fe972d7a4/tools/boss/app-macos/Tests/BossTests/BossCaptureTests.swift#L353-L389) adds `fingerprintAvailable`, defaulting to true, so tests can model a reachable endpoint returning no fingerprint. The fake returns immediately; the new test exercises unavailable responses rather than waiting through real network timeouts. No build or test-runner configuration changes are included.

## Evidence

The author reports running these validation commands:

- `bazel build //tools/boss/app-macos:boss_mac_app_lib //tools/boss/app-macos:BossTestsShard3`
- `bazel test //tools/boss/app-macos:BossTestsShard3`
- `bazel build //tools/boss/app-macos:Boss //tools/boss/installer:release`
- `checkleft run`

The author added regression tests for retaining a reachable engine after three unavailable fingerprint responses and scheduling recovery only on the third consecutive failed tick, as detailed above.

No screenshots are registered.
