# Slow-check threshold

`checkleft run` prints nothing extra on a normal run. If a single check has been running longer than `DEFAULT_SLOW_CHECK_THRESHOLD` (`src/progress/slow.rs`), it prints `checks: still running <check-id> on <path> (<s>s)` (the file a declarative tool is on; `on <n> files` or `on <done>/<total> files` when only counts are known, never a fabricated `0/N`), repeats at 2x, 4x, ... the threshold, and prints `checks: <check-id> finished after <s>s` when a reported check completes. Reporting only: nothing is timed out or killed and results and exit codes are unchanged.

## Measurement

Per-check wall time was recorded from the `finish` event (temporary instrumentation, not committed) on a laptop, running the Bazel-built binary against this repo.

| Scenario                                                                                                                                      | Peak single check                                                                      | Notes                                                                   |
| --------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| Typical change (current PR diff), cold                                                                                                        | 36s (`lint/rust`)                                                                      | same check 8s on re-run; next slowest `format/rust` 6.7s                |
| Large changesets (diffs spanning 5, 15 and 40 recent commits), machine loaded, runs cut at 100s                                               | 87s (`rust/giant-structs`), 83s (`file/size`), 51s (`format/bazel`)                    | worst case; many checks waiting on the same shared work finish together |
| CI `checks` job, 44 passing `main` builds of pipeline `mono` (builds 21583 to 21728), summary line `checks: no findings (N checks ran in Xs)` | median 3s, p90 103s, max 335s (build 21682, 2 modified files); next 283s (build 21633) | whole-run wall time, an upper bound on any single check                 |

CI logs do not contain per-check durations: non-interactive output only prints the total, so the whole run is the tightest bound the logs give (checks run concurrently, so a single check never exceeds it). Only 2 of the 44 healthy runs exceeded 180s, and the 335s run touched only 2 files, so it is not explained by changeset size (the logs do not say what it waited on).

The incident that motivated this (a `checks` job silent for 5+ minutes) is on the order of the slowest healthy CI runs, so wall time alone does not separate a hang from a slow healthy run in CI; the line is informational and nothing is killed.

## Choice

600s. The only CI evidence is the whole-run bound above (max 335s across 44 sampled healthy runs), so the threshold is set clearly above that bound rather than inside it: 600s is about 1.8x the 335s maximum and 2.1x the second-slowest run (283s), and about 7x the worst local single-check peak (87s). Because a single check never exceeds the whole-run time, no sampled healthy CI run could have printed a slow-check line at this threshold. None of the measured local runs could have printed it. On a real hang it reports at 10 minutes and again at 20.

Uncertainty: the bound is a sample of 44 runs, so a future healthy run slower than 600s could still print the line; the line is informational and nothing is killed. The cost of choosing above the bound is that a hang is first reported later (10 minutes, not the 5+ minutes of the incident that motivated this).

The injected-clock regression test `default_stays_quiet_above_sampled_healthy_ci_bound` checks silence through the 335s bound plus 50% headroom (502.5s), silent completion, and reporting at the configured threshold. It guards against reducing the default into the sampled healthy range without relying on real-time sleeps.

## Deferred: per-check CI timings

The logs carry no per-check durations, so the threshold cannot yet be set from the per-check peak in CI. Gathering them needs a CI-side change (opt-in per-check timing output in the `checks` job) that is outside this change. Until then 600s over the whole-run bound is the justified margin; once per-check CI timings exist, it should be re-derived from their peak.
