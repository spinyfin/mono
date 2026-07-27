# Test action hermeticity

## Audit findings

The repository uses Bazel 9.1.0. Before this policy, `--incompatible_strict_action_env` was already enabled by default and test actions used Bazel's `standalone` test action context routed through the platform spawn strategy. On macOS, ordinary test actions therefore appeared as `darwin-sandbox`; the `ci-linux` and `ci-darwin` configurations changed cache locations and the Apple toolchain but did not change the test strategy.

That existing sandbox was not an outside-world boundary:

- A no-RC run of the hermeticity guard inherited `PATH=.:/bin:/usr/bin:/usr/local/bin`.
- The guard established a TCP connection to `1.1.1.1:53`.
- The guard created a file at a fixed path under `/private/tmp`.
- Bazel's generated macOS profile used `(allow default)`, permitted network by default, and allowed writes to shared host temp, cache, log, and developer directories.

The developer shell had `ANTHROPIC_API_KEY` set during the audit, but the same no-RC `bazel test` did not receive it. Bazel's test runner was already scrubbing that ambient credential. Executing a built test binary directly is different: it bypasses Bazel's test runner and all repository test policy, so it inherits the caller's environment. Direct `bazel-bin/..._test` execution is not a supported profiling path; use `bazel test` so the policy remains in force.

## Enforcement

Every `bazel test` now uses `//tools/test-sandbox:hermetic_test_wrapper` at the repository-owned test-code boundary. The wrapper:

- removes common API credentials even if a caller attempts to inject them;
- replaces the developer's `PATH` with Bazel-declared runtime inputs and, on macOS, denies execution of undeclared absolute host binaries;
- points `TMPDIR`, `TMP`, and `TEMP` at the test's private temporary root;
- denies external network while retaining loopback and Unix-domain sockets for in-process mock servers and integration fixtures;
- denies Keychain database reads and securityd IPC on macOS;
- on macOS, applies a Seatbelt profile that denies filesystem writes outside the private test root and declared Bazel output paths.

Linux retains Bazel's `linux-sandbox` test strategy and its mount/network namespaces. macOS uses a local Bazel TestRunner action because Bazel's built-in Darwin profile hardcodes broad host-writable paths and macOS rejects a stricter nested sandbox. Build and compilation actions continue to use their normal Bazel strategies. The repository wrapper applies the stricter Seatbelt profile before repository test code starts.

The macOS wrapper creates a short, unique `/tmp/mono-test.XXXXXX` root (physically `/private/tmp/mono-test.XXXXXX`) and removes it after the test. EXIT, INT, TERM, and HUP traps preserve the test status and forward signals to the complete test process group. The wrapper retains the group identity until the group is confirmed dead and uses a bounded TERM-then-KILL escalation before removing the root. `//tools/test-sandbox:cleanup_guard_test` runs as a dedicated outer supervisor selected by its physical Bazel executable identity, not by target-controlled environment, and exercises TERM and HUP with a signal-ignoring child and descendant. It verifies that neither the root nor either process survives. A short path is required for Unix-domain-socket integration tests: Bazel's local TestRunner output-base path plus a tempfile component exceeds `sockaddr_un.sun_path`.

## Declared runtime and target-level capabilities

There are no ambient credential exemptions. External network is available only through the target-level capability below.

The following local capabilities remain:

- Loopback TCP and Unix-domain sockets are available to all tests for hermetic mock servers and in-process engine/client integration. External addresses remain denied.
- `@test_runtime_tools//:runtime` is a local configured repository that resolves the fixed POSIX, Git, Python, archive, and checksum runtime set to final realpaths. The wrapper consumes that target as runfiles, builds its PATH only from those inputs, and translates the current target's runfiles manifest into exact process-exec grants. On macOS, the repository resolves `DEVELOPER_DIR` from Bazel's tracked `--repo_env` value (with `xcode-select` only as the local fallback), selects the configured developer Python instead of the `/usr/bin/python3` dispatcher, and records the final Python framework tree required by that executable. The same configured root supplies Git and its helper tree. Operator CLIs remain absent, and the guard proves an absolute `/opt/homebrew/bin/gh` attempt is denied.
- Xcode is a separate target-level capability. Its exact canonical developer root comes from the configured runtime repository; the wrapper exports that value as `DEVELOPER_DIR` and grants process execution only to that root plus the system `xcrun`/`xcodebuild` dispatchers. There is no `/Applications/Xcode.app` assumption. The five hostless `macos_unit_test` targets use a repository-owned direct runner that invokes `${DEVELOPER_DIR}/usr/bin/xctest`, rejects UI-test bundles and any application host, and therefore avoids `testmanagerd` dropping the action's path policy.
- Every top-level or mounted macOS filesystem root discovered at action startup is write-protected. The exceptions are the private root and each Bazel-declared output path, applied within every protected root, so an output base under `/private/var/tmp` remains usable without granting that tree. `/dev` device nodes are not persistent host storage and remain available. Hostless XCTest also needs Foundation's atomic-item replacement scratch directory; that sole extra exception is regex-scoped to `TemporaryItems/NSIRD_xctest_*` under the user's Darwin temporary root. No general `/private/var/folders` exception exists. The Rust and XCTest guards prove private and declared-output writes work, Foundation atomic replacement works, and arbitrary writes to the host home, shared `/private/tmp`, and the Darwin temporary root fail with `EPERM`.
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
