# Hermetic test wrapper subprocess cost on macOS

- **Date:** 2026-08-03
- **Provenance:** controlled follow-up to the cross-fleet timing hypothesis
- **Repository revision:** `6ae6bac31865bf7b4ace1d8a31f057dad213f33d`
- **Local host:** Apple M2 Max, 12 cores, 64 GB RAM; macOS 26.5.2 (`Darwin 25.5.0`); Xcode 26.6; Bazel 9.1.0
- **Related investigation:** [`test-action-hermeticity.md`](test-action-hermeticity.md)

This investigation tests whether the macOS Seatbelt policy in `hermetic_test_wrapper` adds roughly 0.45–0.50 seconds to every subprocess exec. It uses same-machine A/B measurements and a fixed-count exec benchmark; cross-machine CI timings are retained only as context.

## Verdict

**The large per-spawn-cost claim is false.** On the same Mac, enabling the wrapper changed libtest's median clock by **+0.13 s** for `ci-log-reader_test` and **+0.05 s** for `cube_lib_test`, not the reported tens of seconds. An exact 5,000-exec benchmark under the real emitted Seatbelt profile completed the entire protected loop in 8.69–14.28 s; a 0.45–0.50 s marginal penalty would instead add 2,250–2,500 s.

The wrapper does have a measurable **one-time test-action startup cost** on this loaded host: subtracting each run's libtest clock from its Bazel action duration gives paired median setup deltas of 0.45–0.61 s across the four main targets. That cost appears on spawn-heavy and spawn-free targets alike. It is profile/runtime setup, not a cost paid per child process.

The earlier residual analysis mistook cross-fleet and target-specific variance for a causal wrapper signal. Its fitted line described those two CI samples; it did not identify the mechanism behind their residuals.

## What was tested

The repository applies the wrapper at `.bazelrc:66`. macOS runs the TestRunner locally at `.bazelrc:51`, after which the wrapper:

- creates and prepares a private root (`tools/test-sandbox/hermetic_test_wrapper.sh:45-85`);
- constructs the Seatbelt exec allowlist from the runtime manifest and executable runfiles (`tools/test-sandbox/hermetic_test_wrapper.sh:258-293`); and
- starts the test once through `sandbox-exec` (`tools/test-sandbox/hermetic_test_wrapper.sh:311-320`).

Linux shares the private-root and environment setup, then immediately `exec`s the test at `tools/test-sandbox/hermetic_test_wrapper.sh:144-146`. It never constructs or enters the Seatbelt profile.

The evidence being tested was observational: one commit's uploaded logs reported `cube_lib_test` at 3.75 s on a Linux agent and 17.27 s on a Mac agent; a fit over 114 common targets gave `darwin ≈ 3.0 s + 1.17 × linux`. The largest reported positive residuals were `ci-log-reader_test` (0.2 s Linux, 35.5 s Mac, +32.2 s residual), `checkleft_lib_test_declarative` (+15.5 s), and `cube_lib_test` (+11.1 s), while three SQLite-heavy targets had residuals of −5.5, −5.1, and −3.1 s. Those are real descriptions of the supplied logs, but the machines, fleet conditions, and target environments were not controlled. The 0.45–0.50 s figure was then back-derived from those residuals and source-estimated spawn counts; it was never an observed per-spawn measurement.

The target matrix was:

| Target                                                 | Role                                 | Retained test count | Why selected                                                                                                                                                       |
| ------------------------------------------------------ | ------------------------------------ | ------------------: | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `//tools/boss/engine/ci-log-reader:ci-log-reader_test` | spawn-heavy case from the claim      |                  47 | Production paths use `tokio::process::Command` (`tools/boss/engine/ci-log-reader/src/lib.rs:28-32`, `:587-610`), and integration tests execute fake provider CLIs. |
| `//tools/cube:cube_lib_test`                           | operator's original spawn-heavy case |                 434 | Large mixed suite with command-runner coverage; this is the target behind the reported 34.1 s local observation.                                                   |
| `//tools/boss/engine/core:work_crud_test`              | in-process SQLite control            |                  27 | Its shared harness starts the engine as a Tokio task over an in-memory SQLite DB, not an OS process (`tools/boss/engine/core/tests/common/mod.rs:58-100`).         |
| `//tools/boss/engine/core:comments_crud_test`          | in-process SQLite control            |                   3 | Uses the same in-process harness.                                                                                                                                  |

The control sources and shared harness contain no `std::process` or `tokio::process` use. Their `tokio::spawn` is an in-process task. That is a source classification, not an observed target spawn count; no numeric target spawn count is used anywhere in the result.

## Method

### Same-machine macOS A/B

I first ran all four targets once with each configuration and discarded those runs to prime compilation, action, and disk caches. Each retained invocation used:

```text
bazel test --noshow_progress --cache_test_results=no --test_output=errors \
  --run_under=<condition> <one-target>
```

The conditions were the repository wrapper and `/usr/bin/env`, a no-op argv passthrough that bypasses the wrapper. Empty `--run_under` is invalid in Bazel 9.1.0. All other repository and TestRunner settings were unchanged.

There were five retained pairs per target. Condition order alternated by pair. `--cache_test_results=no` forced execution, while the primed binaries prevented compilation from entering the measurement. Switching `--run_under` does discard Bazel's analysis cache, so overall command elapsed time is unsuitable; the results below use only:

- Bazel's per-target `PASSED in` action duration, which includes wrapper startup; and
- libtest's `finished in` duration from that invocation's `test.log`, which begins inside the test binary and therefore excludes wrapper profile construction while retaining effects on child execs.

The host was heavily contended. One-minute load before retained runs ranged from **39.22 to 216.03** on 12 cores. The design therefore relies on interleaved pairs, reports the complete range, and does not treat an isolated outlier as a mechanism.

### Direct Seatbelt exec benchmark

Because Bazel action duration showed a wrapper difference, I copied the unmodified Seatbelt profile emitted by a real wrapped `cube_lib_test` while the action was running. The profile was 346 lines / 34,146 bytes and contained the real `(deny process-exec)` policy plus the target's runtime and executable-runfile grants.

The benchmark ran the same `/bin/bash` loop outside Seatbelt and under `/usr/bin/sandbox-exec -f <captured-profile>`. The loop invoked `/usr/bin/true` exactly 5,000 times, used `set -e`, and asserted that the counter reached 5,000. Thus the benchmark has an observed, fixed successful-exec count rather than a count inferred from application source. Five pairs alternated order.

A five-pair zero-child control measured one `sandbox-exec` entry at 0.01 s versus 0.00 s plain at `/usr/bin/time -p` resolution. The 5,000-child result therefore overwhelmingly reflects the loops, not profile entry.

### Linux context

I used uploaded `test.log` artifacts from three amd64 Buildkite jobs that had the wrapper enabled and compilation primed by the pipeline's preceding `bazel build`. Each selected test actually executed and uploaded a log. The repository pins Bazel 9.1.0 in `.bazelversion:1`; the agents were `empiricist-1`/`empiricist-2`, reported by Buildkite as Linux amd64 with agent 3.127.1.

The retained CI metadata does **not** record the Linux distribution or kernel version. That missing OS version is a limitation and is not guessed here. These Linux runs are also not used as causal A/B evidence because they are different machines; they only verify the wrapper's non-Seatbelt path.

## Results

### macOS: wrapper versus bypass

Values are median (minimum–maximum), in seconds. “Paired Δ” is wrapped minus bypass for each pair, then the median and full pair range.

| Target               | Clock        |          Wrapped |           Bypass |                Paired Δ |
| -------------------- | ------------ | ---------------: | ---------------: | ----------------------: |
| `ci-log-reader_test` | Bazel action |    2.7 (2.4–3.8) |    2.0 (1.9–2.1) |        +0.7 (+0.4–+1.9) |
|                      | libtest      | 2.02 (1.95–2.99) | 1.89 (1.83–2.00) | **+0.13 (+0.02–+1.16)** |
| `cube_lib_test`      | Bazel action |    5.9 (5.6–6.1) |    5.3 (5.0–5.6) |        +0.5 (+0.3–+0.9) |
|                      | libtest      | 5.23 (5.01–5.50) | 5.22 (4.93–5.55) | **+0.05 (−0.25–+0.30)** |
| `work_crud_test`     | Bazel action |    1.8 (1.6–4.4) |    1.2 (1.0–2.7) |        +0.5 (+0.4–+3.1) |
|                      | libtest      | 1.22 (1.00–3.55) | 1.11 (0.94–2.56) |     +0.06 (−0.19–+2.37) |
| `comments_crud_test` | Bazel action |    1.7 (1.1–2.5) |    0.9 (0.7–1.2) |        +1.0 (+0.2–+1.6) |
|                      | libtest      | 1.31 (0.79–1.73) | 0.81 (0.67–1.07) |     +0.64 (−0.05–+0.89) |

The action-level wrapper delta is similar across roles; it does not grow on the spawn-heavy cases. Inside libtest, `cube_lib_test` is effectively unchanged and `ci-log-reader_test` moves by tenths, not the reported +32 seconds. The control outliers demonstrate how noisy this host was: the largest paired libtest delta belongs to a source-audited spawn-free control, not to either spawn-heavy case.

For each run, subtracting libtest duration from Bazel action duration isolates the pre/post-test action envelope at the clocks' available precision. The paired median wrapped-minus-bypass envelope deltas were 0.57 s (`ci-log-reader_test`), 0.51 s (`cube_lib_test`), 0.61 s (`work_crud_test`), and 0.45 s (`comments_crud_test`). This is the consistent fixed startup signal.

Current runfiles manifests contained 2 executable entries for `ci-log-reader_test`, `work_crud_test`, and `comments_crud_test`, versus 51 for `cube_lib_test`. `cube_lib_test` nevertheless had the smallest action paired median of the four (tied at +0.5 s). The profile-construction loop does scale with executable runfile count by construction; these measurements show neither a large runfile-size penalty at these sizes nor any relationship to runtime spawn count.

### macOS: exact-count exec benchmark

| Pair | Seatbelt 5,000 execs | Plain 5,000 execs | Paired marginal Δ per exec |
| ---: | -------------------: | ----------------: | -------------------------: |
|    1 |              14.28 s |            9.86 s |                  +0.884 ms |
|    2 |              10.29 s |           11.38 s |                  −0.218 ms |
|    3 |              10.66 s |           22.55 s |                  −2.378 ms |
|    4 |               8.69 s |           13.55 s |                  −0.972 ms |
|    5 |               9.07 s |            8.78 s |                  +0.058 ms |

The paired marginal estimate is too noisy to distinguish from zero: median **−0.218 ms/exec**, range **−2.378 to +0.884 ms/exec**. The negative values are scheduling noise, not a speedup claim.

The useful bound needs no subtraction. The slowest complete Seatbelt loop took 14.28 s for 5,000 successful child execs, or 2.856 ms per exec **including ordinary process creation, shell-loop overhead, Seatbelt entry, and all contention**. A Seatbelt-only marginal cost of 450–500 ms per exec is therefore impossible on this host by more than two orders of magnitude; the claim would exceed the measured entire protected loop by roughly 158–175 times.

A separate seven-pair, 1,000-exec sensitivity run was likewise noisy but small: its paired marginal median was +0.59 ms/exec, range −0.76 to +1.29 ms/exec. Neither experiment supports a remotely 450 ms-scale effect.

### Linux with the wrapper enabled

These are libtest's own clocks from uploaded artifacts:

|                                                 Build | Commit         | Agent          |   `ci-log-reader_test` |        `cube_lib_test` |       `work_crud_test` |   `comments_crud_test` |
| ----------------------------------------------------: | -------------- | -------------- | ---------------------: | ---------------------: | ---------------------: | ---------------------: |
| [9632](https://buildkite.com/flunge/mono/builds/9632) | `86102bc3fa45` | `empiricist-1` |                 0.04 s |                 1.58 s |                 2.86 s |                 2.18 s |
| [9638](https://buildkite.com/flunge/mono/builds/9638) | `b6a7ca673f50` | `empiricist-2` |                 0.03 s |                 1.37 s |                 2.85 s |                 2.32 s |
| [9639](https://buildkite.com/flunge/mono/builds/9639) | `7aadae717bd5` | `empiricist-2` |                 0.02 s |                 1.33 s |                 4.39 s |                 2.76 s |
|                                    **Median (range)** |                |                | **0.03 (0.02–0.04) s** | **1.37 (1.33–1.58) s** | **2.86 (2.85–4.39) s** | **2.32 (2.18–2.76) s** |

The corresponding Bazel action ranges were 0.1–0.2 s, 1.5–1.7 s, 3.0–4.5 s, and 2.3–3.0 s. The Linux wrapper path has no spawn-heavy blow-up. These numbers must not be subtracted from the Mac numbers to estimate a platform cost: CPU, operating system, fleet contention, and target revisions differ.

## Reconciliation with the prior 115 ms validation

The prior validation used `//tools/boss/engine/comment-classifier:comment-classifier_test` (`test-action-hermeticity.md:52-68`). It compared two warm forced runs on each revision and observed means of 0.540 s before versus 0.655 s after, hence 115 ms.

That benchmark was sound for its stated question—small-test wrapper startup—but incomplete for the later per-spawn claim. The comment-classifier crate contains no process API and its current libtest clock is 0.00–0.01 s, so it has effectively zero subprocess spawns. A per-spawn effect could not have appeared there.

Repeating that target in five current A/B pairs produced:

| Clock        |                   Wrapped |             Bypass |            Paired Δ |
| ------------ | ------------------------: | -----------------: | ------------------: |
| Bazel action |    0.7 s median (0.5–1.3) |    0.1 s (0.1–0.1) |  +0.6 s (+0.4–+1.2) |
| libtest      | 0.00 s median (0.00–0.01) | 0.00 s (0.00–0.00) | 0.00 s (0.00–+0.01) |

The current one-time startup is larger and noisier than 115 ms, but there is no evidence of a wrapper regression:

- `hermetic_test_wrapper.sh` has had no change after `7aadae717bd5` (merged as `7efc90ab2192` on 2026-07-27).
- Relative to the prior published head `1a017643264b`, later wrapper changes add the Linux private-root path, credential scrubbing, and a coverage output path; they do not alter the macOS exec-policy loop or `sandbox-exec` launch.
- `.bazelrc` commit `d8281e980d3a` later aligned the build/cquery analysis configuration, but left the test wrapper line intact.
- Bazel remains 9.1.0.
- The current host is on `Darwin 25.5.0` and ran this matrix at load 39–216. The earlier document did not record its exact OS build or per-run load; a separate 2026-07-17 investigation recorded `Darwin 25.3.0` and load 20–111 on this host (`rust-test-sharding-engine-lib-test.md:141-148`).

The evidence therefore supports: **the prior test was valid but incomplete for per-spawn behavior; the new per-spawn claim nevertheless fails the direct test.** The present roughly 0.45–0.61 s fixed startup should not be retroactively presented as a stable fleet constant because this host was extremely noisy.

## Why the residual analysis was mistaken

Each known weakness changes the interpretation:

1. **Different machines were treated as a platform experiment.** The Linux and macOS logs differed in CPU, OS, load, local cache history, and concurrent jobs. A fitted slope and intercept do not make those factors exchangeable. Residuals remain the unmodeled part of all of them.
2. **The proposed per-spawn coefficient was back-solved, not measured.** The exact-count benchmark finds no measurable marginal effect and bounds the entire protected exec at milliseconds, not hundreds of milliseconds.
3. **Target spawn counts were inferred from source.** This investigation does not reuse them. Target-level spawn counts remain unobserved; the causal microbenchmark supplies a known count of 5,000 successful execs instead.
4. **“Only one slow test” and “a slow class” were conflated.** Both claimed spawn-heavy reproducers were tested. Neither shows the proposed wrapper effect, and the source-audited spawn-free controls are at least as noisy. There is neither a `cube_lib_test`-specific wrapper regression nor a spawn-heavy class effect here.

The reported regression `darwin ≈ 3.0 s + 1.17 × linux` is still a description of those 114 paired observations. Its positive residuals say that those target/machine combinations were slower than that simple line predicted; they do not say why. Assigning the residuals to Seatbelt required the missing same-machine toggle and direct exec measurement, and both reject the assignment.

## The operator's 34.1 s observation

This run did not reproduce 34.1 s. `cube_lib_test`'s wrapped libtest range was 5.01–5.50 s and its bypass range was 4.93–5.55 s, even while one-minute load reached 216.

Relative to the current bypass median of 5.22 s, the old 34.1 s observation has 28.88 s unexplained. The largest positive paired wrapper delta observed for this target was 0.30 s, less than 1% of 34.1 s; at least 28.58 s of the old gap is not reproduced by toggling the wrapper. That arithmetic quantifies the non-wrapper remainder, **not** the fraction caused specifically by contention.

The historical load range of 20–111 makes contention a credible contributor, but the current run also shows that load average alone does not predict `cube_lib_test`: it stayed near five seconds at still higher reported load. Without the original run's process pressure, I/O state, cache state, exact revision, and per-test trace, the remainder cannot honestly be divided among contention, then-current test logic, and other transient machine state.

## Repo-wide cost

There is no confirmed per-spawn cost to extrapolate repo-wide. Multiplying a fitted residual or the current loaded-host startup median by a target count would repeat the original methodological error: Bazel executes tests in parallel, cached test results skip execution, targets have different profiles, and the startup measurements are noisy.

The supported statement is narrower: on this host and revision, an executing macOS test action paid roughly 0.45–0.61 s median one-time wrapper startup in this five-target sample, while spawn-heavy libtest clocks did not acquire a large marginal exec cost. A real repo-wide startup total would require a controlled full-suite A/B with action profiles on an otherwise idle fleet host; it is unnecessary to decide the per-spawn claim and was not inferred here.

## Reproduction record

The same-machine target runs used explicit conditions:

```text
--run_under=//tools/test-sandbox:hermetic_test_wrapper
--run_under=/usr/bin/env
```

All retained target trials used `--cache_test_results=no`; compilation and disk caches were primed. The exact-count benchmark reused the unmodified profile emitted by the wrapper and changed no repository file. Linux values came directly from uploaded `test.log` artifacts in the linked builds.

No wrapper, BUILD file, test, or source file was changed for this investigation.
