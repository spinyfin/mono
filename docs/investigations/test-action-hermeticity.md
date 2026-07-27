# Test action hermeticity

## Audit findings

The repository uses Bazel 9.1.0. Before this policy, `--incompatible_strict_action_env` was already enabled by default and test actions used Bazel's `standalone` test action context routed through the platform spawn strategy. On macOS, ordinary test actions therefore appeared as `darwin-sandbox`; the `ci-linux` and `ci-darwin` configurations changed cache locations and the Apple toolchain but did not change the test strategy.

That existing sandbox was not an outside-world boundary:

- A no-RC run of the hermeticity guard inherited `PATH=.:/bin:/usr/bin:/usr/local/bin`.
- The guard established a TCP connection to `1.1.1.1:53`.
- The guard created a file at a fixed path under `/private/tmp`.
- Bazel's generated macOS profile used `(allow default)`, permitted network by default, and allowed writes to shared host temp, cache, log, and developer directories.

The developer shell had `ANTHROPIC_API_KEY` set during the audit, but the same no-RC `bazel test` did not receive it. Bazel's test runner was already scrubbing that ambient credential. Executing a built test binary directly is different: it bypasses Bazel's test runner and all repository test policy, so it inherits the caller's environment. Direct `bazel-bin/..._test` execution is not a supported profiling path; use `bazel test` so the policy remains in force.

Buildkite build 9590 supplied the missing Linux evidence. The canonical `bazel-build-test` job ran on the amd64 `empiricist-1` agent, and Bazel selected `processwrapper-sandbox` for every test action. That strategy is a working-directory wrapper, not a mount or network namespace: both hermeticity guards connected to `1.1.1.1:53` and created files under `/var/tmp`. The same log showed at least 17 engine/CLI targets timing out because the wrapper nested `TMPDIR` below Bazel's deep `processwrapper-sandbox/.../_tmp/<hash>/tmp` path and their Unix sockets exceeded `sockaddr_un.sun_path`.

## Enforcement

Every `bazel test` now uses `//tools/test-sandbox:hermetic_test_wrapper` at the repository-owned test-code boundary. The wrapper:

- removes common API credentials and all three `BOSS_SHAKE_*` GitHub App build credentials even if a caller attempts to inject them;
- replaces the developer's `PATH` with Bazel-declared runtime inputs and, on macOS, denies execution of undeclared absolute host binaries;
- points `TMPDIR`, `TMP`, and `TEMP` at the test's private temporary root;
- denies external network while retaining loopback and Unix-domain sockets for in-process mock servers and integration fixtures;
- denies Keychain database reads and securityd IPC on macOS;
- on macOS, applies a Seatbelt profile that denies filesystem writes outside the private test root and declared Bazel output paths.

Linux explicitly pins the TestRunner mnemonic to Bazel's `linux-sandbox`; fallback to `processwrapper-sandbox` is not permitted. `/tmp` is a per-action sandbox tmpfs, and the wrapper creates its private root at `/tmp/mono-test.XXXXXX`. The mount namespace therefore denies host writes, the network namespace enforces the default external-network denial, and Unix sockets get a short action-private path. If a Linux agent cannot create the namespace-capable sandbox, Bazel fails the test action instead of running it with weaker isolation. macOS uses a local Bazel TestRunner action because Bazel's built-in Darwin profile hardcodes broad host-writable paths and macOS rejects a stricter nested sandbox. Build and compilation actions continue to use their normal Bazel strategies. The repository wrapper applies the stricter Seatbelt profile before repository test code starts.

On both supported hosts the wrapper creates and removes a short, unique `/tmp/mono-test.XXXXXX` root (physically `/private/tmp/mono-test.XXXXXX` on macOS and inside the per-action tmpfs on Linux). EXIT, INT, TERM, and HUP traps preserve the test status and forward signals to the complete test process group. The wrapper retains the group identity until the group is confirmed dead and uses a bounded TERM-then-KILL escalation before removing the root. `//tools/test-sandbox:cleanup_guard_test` runs as a dedicated outer supervisor selected by its physical Bazel executable identity, not by target-controlled environment, and exercises TERM and HUP with a signal-ignoring child and descendant. It verifies that neither the root nor either process survives.

The local development host for this revision is macOS, so it cannot execute the Linux namespace boundary itself. `//tools/test-sandbox:linux_policy_source_test` pins the exact fail-closed strategy, per-action tmpfs, short-root construction, and credential configuration in a hermetic source test; the Rust guard additionally validates the Linux root shape and the 108-byte `sun_path` budget when compiled on Linux. The next canonical Buildkite Linux run is the deciding capability and enforcement validation.

## Declared runtime and target-level capabilities

There are no ambient credential exemptions. External network is available only through the target-level capability below.

The following local capabilities remain:

- Loopback TCP and Unix-domain sockets are available to all tests for hermetic mock servers and in-process engine/client integration. External addresses remain denied.
- `@test_runtime_tools//:runtime` is a local configured repository that resolves the fixed POSIX, Git, Python, archive, and checksum runtime set to final realpaths. The wrapper consumes that target as runfiles, builds its PATH only from those inputs, and translates the current target's runfiles manifest into exact process-exec grants. On macOS, the repository resolves `DEVELOPER_DIR` from Bazel's tracked `--repo_env` value (with `xcode-select` only as the local fallback), selects the configured developer Python instead of the `/usr/bin/python3` dispatcher, and records the final Python framework tree required by that executable. The same configured root supplies Git and its helper tree. Operator CLIs remain absent, and the guard proves an absolute `/opt/homebrew/bin/gh` attempt is denied.
- Xcode is a separate target-level capability. Its exact canonical developer root comes from the configured runtime repository; the wrapper exports that value as `DEVELOPER_DIR` and grants process execution only to that root plus the system `xcrun`/`xcodebuild` dispatchers. There is no `/Applications/Xcode.app` assumption. The five hostless `macos_unit_test` targets use a repository-owned direct runner that invokes `${DEVELOPER_DIR}/usr/bin/xctest`, rejects UI-test bundles and any application host, and therefore avoids `testmanagerd` dropping the action's path policy. Under `bazel coverage`, the same headless runner records LLVM profiles and uses the exact configured Xcode `llvm-profdata` and `llvm-cov` binaries to emit Bazel's declared LCOV output (and optional JSON); coverage does not introduce an application or UI host.
- Every top-level or mounted macOS filesystem root discovered at action startup is write-protected. The exceptions are the private root and each Bazel-declared output path, applied within every protected root, so an output base under `/private/var/tmp` remains usable without granting that tree. `/dev` device nodes are not persistent host storage and remain available. Hostless XCTest also needs Foundation's atomic-item replacement scratch directory; that sole extra exception is regex-scoped to `TemporaryItems/NSIRD_xctest_*` under the user's Darwin temporary root. No general `/private/var/folders` exception exists. The Rust and XCTest guards prove private and declared-output writes work, Foundation atomic replacement works, and arbitrary writes to the host home, shared `/private/tmp`, and the Darwin temporary root fail with `EPERM`.
- The live update downloader gets its stable archive directory from `FileManager.default.temporaryDirectory` and passes that validated Foundation URL into the URLSession delegate. It does not reinterpret ambient `TMPDIR` text; tests cover empty, relative, nonexistent, and malformed values while retaining Foundation's production behavior.
- `//tools/boss/engine/ci-log-reader:ci-log-reader_test` needs no write capability. Its four fake CLI heredocs use shell `printf`, so the normal private-root-only policy applies.

No target can reach external network by default. A Rust test that genuinely requires external network must use `network_enabled_rust_test`. That single target-level macro adds Bazel's `requires-network` execution requirement for Linux and the wrapper marker for macOS. `//tools/test-sandbox:network_opt_in_test` validates the opt-in while the default hermeticity guard validates the deny path.

## Measurements

The representative build target was `//tools/boss/engine/comment-classifier:comment-classifier_test`. Each side used an initial analysis-warming build followed by three identical `bazel build --noshow_progress` runs:

| State                            |   Warm wall-clock runs |    Mean | Cache behavior                         |
| -------------------------------- | ---------------------: | ------: | -------------------------------------- |
| Before declared runtime revision | 0.47 s, 0.45 s, 0.50 s | 0.473 s | 9 action-cache hits, 1 internal action |
| After declared runtime revision  | 0.47 s, 0.45 s, 0.49 s | 0.470 s | 9 action-cache hits, 1 internal action |

The measured warm build difference was -3 ms (within run-to-run noise), and cache behavior was identical. The initial post-change build took 1.00 s versus 1.04 s before; both reloaded roughly 330 packages after the test run-under analysis option changed and both were dominated by reanalysis. Build actions continue to use the disk cache and normal Darwin sandbox/local strategy selection.

The same representative target was measured with actual `bazel test` commands before and after the final policy revision. Forced runs used `bazel test --noshow_progress --cache_test_results=no --test_output=errors //tools/boss/engine/comment-classifier:comment-classifier_test`; the action and disk caches were warm, but Bazel executed the test each time. Default-cache runs omitted `--cache_test_results=no`.

| State                 | First run with reanalysis | Warm forced wall-clock runs | Warm forced mean | Default-cache wall-clock runs | Test-result cache behavior |
| --------------------- | ------------------------: | --------------------------: | ---------------: | ----------------------------: | -------------------------- |
| Before final revision |                    1.37 s |              0.55 s, 0.53 s |          0.540 s |                0.47 s, 0.53 s | `(cached)`, `Executed 0`   |
| After final revision  |                    0.84 s |              0.67 s, 0.64 s |          0.655 s |                0.43 s, 0.45 s | `(cached)`, `Executed 0`   |

The final root enumeration and process-policy setup added 115 ms to the mean warm forced execution for this small test. Test-result caching remained enabled and skipped test execution on both default-cache runs; its mean wall time changed from 0.500 s to 0.440 s. The first-run numbers are reported separately because their analysis state differed (329 packages loaded before versus 70 after) and are not a direct execution-cost comparison.

The Linux/coverage revision used the same representative target after the full non-UI suite had warmed action and disk caches:

| State                     | First forced run | Warm forced wall-clock runs | Warm forced mean | Default-cache wall-clock runs | Test-result cache behavior |
| ------------------------- | ---------------: | --------------------------: | ---------------: | ----------------------------: | -------------------------- |
| Published head `1a017643` |           0.84 s |              0.67 s, 0.64 s |          0.655 s |                0.43 s, 0.45 s | `(cached)`, `Executed 0`   |
| Linux/coverage revision   |           1.00 s |              0.74 s, 0.64 s |          0.690 s |        0.45 s, 0.47 s, 0.45 s | `(cached)`, `Executed 0`   |

The new credential scrub and policy checks add 35 ms to the warm forced mean on macOS, while the default test-result cache still executes zero tests (0.457 s mean). Three warm `bazel build --noshow_progress` runs took 0.51 s, 0.46 s, and 0.43 s (0.467 s mean versus the prior 0.470 s), with action-cache hits and no rebuild. The Linux strategy and `/tmp` tmpfs affect local test execution only; they do not change compile action inputs, disk-cache configuration, or remote-cache behavior. Coverage uses Bazel's existing coverage configuration and therefore remains in its separate instrumented action-cache namespace.
