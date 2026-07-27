# Test action hermeticity

## Audit findings

The repository uses Bazel 8.3.0. Before this policy, `--incompatible_strict_action_env` was already enabled by default and test actions used Bazel's `standalone` test action context routed through the platform spawn strategy. On macOS, ordinary test actions therefore appeared as `darwin-sandbox`; the `ci-linux` and `ci-darwin` configurations changed cache locations and the Apple toolchain but did not change the test strategy.

That existing sandbox was not an outside-world boundary:

- A no-RC run of the hermeticity guard inherited `PATH=.:/bin:/usr/bin:/usr/local/bin`.
- The guard established a TCP connection to `1.1.1.1:53`.
- The guard created a file at a fixed path under `/private/tmp`.
- Bazel's generated macOS profile used `(allow default)`, permitted network by default, and allowed writes to shared host temp, cache, log, and developer directories.

The developer shell had `ANTHROPIC_API_KEY` set during the audit, but the same no-RC `bazel test` did not receive it. Bazel's test runner was already scrubbing that ambient credential. Executing a built test binary directly is different: it bypasses Bazel's test runner and all repository test policy, so it inherits the caller's environment. Direct `bazel-bin/..._test` execution is not a supported profiling path; use `bazel test` so the policy remains in force.

## Enforcement

Every `bazel test` now uses `//tools/test-sandbox:hermetic_test_wrapper` at the repository-owned test-code boundary. The wrapper:

- removes common API credentials even if a caller attempts to inject them;
- replaces the developer's `PATH` with a fixed allowlist that does not contain `gh`, `bk`, `codex`, `claude`, or `cube`;
- points `TMPDIR`, `TMP`, and `TEMP` at the test's private temporary root;
- denies external network while retaining loopback and Unix-domain sockets for in-process mock servers and integration fixtures;
- on macOS, applies a Seatbelt profile that denies filesystem writes outside the private test root and declared Bazel output paths.

Linux retains Bazel's `linux-sandbox` test strategy and its mount/network namespaces. macOS uses a local Bazel TestRunner action because Bazel's built-in Darwin profile hardcodes broad host-writable paths and macOS rejects a stricter nested sandbox. Build and compilation actions continue to use their normal Bazel strategies. The repository wrapper applies the stricter Seatbelt profile before repository test code starts.

The macOS wrapper creates a short, unique `/private/tmp/mono-test.XXXXXX` root and removes it after the test. A short path is required for Unix-domain-socket integration tests: Bazel's local TestRunner output-base path plus a tempfile component exceeds `sockaddr_un.sun_path`.

## Explicit exemptions

There are no external-network or credential exemptions.

The following local capabilities remain:

- Loopback TCP and Unix-domain sockets are available to all tests for hermetic mock servers and in-process engine/client integration. External addresses remain denied.
- The curated runtime PATH contains only the POSIX, Git, Python, archive, checksum, and Xcode tools required by existing tests and Bazel-generated runners: `awk`, `basename`, `bash`, `cat`, `chmod`, `cp`, `cut`, `date`, `dirname`, `echo`, `env`, `false`, `find`, `git`, `grep`, `head`, `ln`, `mkdir`, `mkfifo`, `mktemp`, `mv`, `od`, `printf`, `pwd`, `python3`, `rm`, `sed`, `sh`, `shasum`, `sleep`, `sort`, `tail`, `tee`, `touch`, `tr`, `true`, `uname`, `unzip`, `wc`, `xcodebuild`, and `xcrun`. It deliberately excludes operator-installed service CLIs. These tools should move to declared Bazel toolchains over time; the fixed list prevents discovery of additional host binaries in the meantime.
- `//tools/boss/engine/ci-log-reader:ci-log-reader_test` sets `MONO_TEST_ALLOW_OUTSIDE_WRITES=1`. Its fake CLI fixtures use shell here-documents whose anonymous output pipes cannot be expressed as path allowlists in a Seatbelt profile. External network, credentials, and the curated PATH policy still apply.
- The five `macos_unit_test` targets under `//tools/boss/app-macos` set `MONO_TEST_ALLOW_OUTSIDE_WRITES=1`. Apple's `xcodebuild` test runner writes DerivedData and result-bundle support data outside `TEST_TMPDIR`. External network, credentials, and the curated PATH policy still apply.

No target can reach external network by default. A future test that genuinely requires it must set `MONO_TEST_ALLOW_NETWORK=1` on the target and document why.

## Measurements

The representative target was `//tools/boss/engine/comment-classifier:comment-classifier_test`, forced to execute with `--cache_test_results=no` on a warm build cache:

| Path                                  | Wall clock | Test action |
| ------------------------------------- | ---------: | ----------: |
| Previous Darwin sandbox behavior      |     0.37 s |       0.1 s |
| Enforced wrapper and Seatbelt profile |     0.42 s |       0.1 s |

The measured warm overhead was about 50 ms. Build actions are unaffected by the test-only policy; `bazel build` continues to use the disk cache and normal Darwin sandbox/local strategy selection. Switching strategy flags invalidates Bazel's analysis cache, so cold one-shot comparisons were dominated by reanalysis and were not representative of steady-state cost.
