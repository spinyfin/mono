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

300s: above every healthy CI run except the single 335s outlier and the second-slowest (283s) with margin, about 3.4x the worst local single-check peak (87s) and 8x the typical cold peak (36s). Local runs never print it. In CI it would have fired on 1 of the 44 sampled healthy runs (the 335s one), and on a real hang it reports at 5 minutes and again at 10, which is the incident's scale. An earlier 180s choice would have fired on 2 of 44 healthy CI runs, so it was raised.

Re-measure and revisit if checks get materially slower, or once CI logs carry per-check timings (then the threshold can be set from the per-check peak instead of the whole-run bound).
