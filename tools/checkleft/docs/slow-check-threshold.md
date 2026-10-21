# Slow-check threshold

`checkleft run` prints nothing extra on a normal run. If a single check has been running longer than `DEFAULT_SLOW_CHECK_THRESHOLD` (`src/progress/slow.rs`), it prints `checks: still running <check-id> on <n>/<total> files (<s>s)`, repeats at 2x, 4x, ... the threshold, and prints `checks: <check-id> finished after <s>s` when a reported check completes. Reporting only: nothing is timed out or killed and results and exit codes are unchanged.

## Measurement

Per-check wall time was recorded from the `finish` event (temporary instrumentation, not committed) on a laptop, running the Bazel-built binary against this repo.

| Scenario                                                                                        | Peak single check                                                   | Notes                                                                   |
| ----------------------------------------------------------------------------------------------- | ------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| Typical change (current PR diff), cold                                                          | 36s (`lint/rust`)                                                   | same check 8s on re-run; next slowest `format/rust` 6.7s                |
| Large changesets (diffs spanning 5, 15 and 40 recent commits), machine loaded, runs cut at 100s | 87s (`rust/giant-structs`), 83s (`file/size`), 51s (`format/bazel`) | worst case; many checks waiting on the same shared work finish together |
| CI `checks` job, healthy attempt                                                                | 1m32s for the whole job                                             | so no single check can exceed that in a healthy CI run                  |

The incident that motivated this (a `checks` job silent for 5+ minutes) is about 3.5x the whole healthy job.

## Choice

180s: about 2x the worst observed single-check peak (87s), 5x the typical cold peak (36s), and about 2x a whole healthy CI job. A normal run, including the large loaded ones measured, never prints it, while a 5+ minute hang is reported at 3 minutes and again at 6.

Re-measure and revisit if checks get materially slower.
