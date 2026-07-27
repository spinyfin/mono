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

The macOS wrapper creates a short, unique `/tmp/mono-test.XXXXXX` root (physically `/private/tmp/mono-test.XXXXXX`) and removes it after the test. EXIT, INT, TERM, and HUP traps preserve the test status and forward signals to the complete test process group before cleanup. `//tools/test-sandbox:cleanup_guard_test` exercises TERM and HUP and verifies that neither the root nor child/descendant processes survive. A short path is required for Unix-domain-socket integration tests: Bazel's local TestRunner output-base path plus a tempfile component exceeds `sockaddr_un.sun_path`.

## Declared runtime and target-level capabilities

There are no ambient credential exemptions. External network is available only through the target-level capability below.

The following local capabilities remain:

- Loopback TCP and Unix-domain sockets are available to all tests for hermetic mock servers and in-process engine/client integration. External addresses remain denied.
- `@test_runtime_tools//:runtime` is a local configured repository that resolves the fixed POSIX, Git, Python, archive, and checksum runtime set to final realpaths. The wrapper consumes that target as runfiles, builds its PATH only from those inputs, and translates the current target's runfiles manifest into exact process-exec grants. The Homebrew Python framework tree is recorded as an explicit audited runtime tree because its launcher delegates to the versioned framework binary; it is not copied into every test's runfiles tree. Operator CLIs remain absent, and the guard proves an absolute `/opt/homebrew/bin/gh` attempt is denied.
- Xcode is a separate target-level capability. The five `macos_unit_test` targets use `XCODE_TEST_ENV`, which permits the registered `/Applications/Xcode.app` toolchain, injects the private test root into the generated `.xctestrun`, sends DerivedData there, and disables system diagnostic collection. The Apple runner's shell heredoc is patched out. Because `testmanagerd` does not preserve per-action Seatbelt path extensions, these targets use explicit write denials for system roots, the host user tree except Bazel-owned result paths, and shared `/tmp` and `/var/tmp`; their fixtures write only under injected `TEST_TMPDIR`. `TestSandboxPolicyTests` proves the Xcode-backed test process cannot create files in the host home or shared temp.
- `//tools/boss/engine/ci-log-reader:ci-log-reader_test` needs no write capability. Its four fake CLI heredocs use shell `printf`, so the normal private-root-only policy applies.

No target can reach external network by default. A Rust test that genuinely requires external network must use `network_enabled_rust_test`. That single target-level macro adds Bazel's `requires-network` execution requirement for Linux and the wrapper marker for macOS. `//tools/test-sandbox:network_opt_in_test` validates the opt-in while the default hermeticity guard validates the deny path.

## Measurements

The representative build target was `//tools/boss/engine/comment-classifier:comment-classifier_test`. Each side used an initial analysis-warming build followed by three identical `bazel build --noshow_progress` runs:

| State                            |   Warm wall-clock runs |    Mean | Cache behavior                         |
| -------------------------------- | ---------------------: | ------: | -------------------------------------- |
| Before declared runtime revision | 0.47 s, 0.45 s, 0.50 s | 0.473 s | 9 action-cache hits, 1 internal action |
| After declared runtime revision  | 0.47 s, 0.45 s, 0.49 s | 0.470 s | 9 action-cache hits, 1 internal action |

The measured warm build difference was -3 ms (within run-to-run noise), and cache behavior was identical. The initial post-change build took 1.00 s versus 1.04 s before; both reloaded roughly 330 packages after the test run-under analysis option changed and both were dominated by reanalysis. Build actions continue to use the disk cache and normal Darwin sandbox/local strategy selection.
