# `boss_mac_app_lib` Swift compile time: measurements and decision

Measured 2026-10-12 against `main` at `095e809a` (the commit the slow
release build compiled). This records what the single monolithic Swift
module (`//tools/boss/app-macos:boss_mac_app_lib`, 185 files, ~58k lines)
actually costs, how much of the observed CI wall time is host contention,
and why the module was **not** split as a result.

## The symptom

Buildkite `mono` build 20478, `boss-release` step (`-c opt`), agent
`anaplian.local-2`:

```
[1,898 / 1,909] Compiling Swift module //tools/boss/app-macos:boss_mac_app_lib; 243s disk-cache, worker
```

The compile ran from 18:58:43Z to 19:02:47Z (log timestamps).

## 1. Host load during that compile

`anaplian.local` runs **four** Buildkite agents (two on the `macos-arm64`
queue, two on `bazel-any`). During the 243 s compile the other three were
all busy with Bazel work on the same box:

| job                                       | agent                | started   | what it runs                                              |
| ----------------------------------------- | -------------------- | --------- | --------------------------------------------------------- |
| mono 20480 `bazel-build-test`             | anaplian.local-2-any | 18:59:25Z | `bazel build //...` + `bazel test //...` (whole repo)     |
| mono 20481 `mac-app-build`                | anaplian.local-1     | 19:00:15Z | another fastbuild compile of this same module + its tests |
| mono-integrity 6521 `integrity-checkleft` | anaplian.local-1-any | 19:00:21Z | checkleft over the repo                                   |

(Source: `bk api "/builds?created_from=…"` filtered to
`agent.hostname == anaplian.local`.) Each of those Bazel servers assumes it
owns every core (`--jobs=200`, default `--local_cpu_resources`), so the
release compile shared the machine's cores with three other full builds.

The same compile on a lightly loaded box is 3-4x faster (below), and on
this laptop the _identical_ opt action swung between 82 s and 275 s with
nothing but the load average changing. Host load is the dominant factor in
the 243 s number.

## 2. What the module itself costs

Measured locally (Apple M2 Max, 12 cores, Xcode 27.0 / Swift 6.4) with
`bazel build … --profile` for action wall time and
`--@build_bazel_rules_swift//swift:copt=-stats-output-dir` for per-phase
CPU time from `swift-frontend` itself. No `bazel clean`, no cache flags
touched. The machine was shared with other agents the whole time; the
load average is recorded with each number because it matters.

### Opt (`-c opt`, what `boss-release` builds)

rules_swift compiles opt builds whole-module (`-O -whole-module-optimization
-num-threads 12`): one frontend process, in which parsing, type checking,
SILGen and SIL optimization are **serial**, and only the LLVM pipeline runs
on the 12 threads.

| phase                      | CPU (user) | wall at load ~113 |
| -------------------------- | ---------- | ----------------- |
| type checking              | 14.4 s     | 73 s              |
| SILGen                     | 5.5 s      | 17 s              |
| SIL optimization           | 39.3 s     | 135 s             |
| IRGen                      | 3.1 s      | 15 s              |
| LLVM pipeline (12 threads) | 39.4 s     | 23 s              |
| **whole frontend**         | **108 s**  | **275 s**         |

Serial floor is therefore about 62 s of single-core work plus a few seconds
of parallel LLVM: roughly 65-75 s on an idle machine, and the measured
82 s at load ~25 is consistent with that. The 243 s on CI is that floor
times a ~3x contention multiplier.

### Fastbuild (what `mac-app-build` and the dev loop build)

Batch mode: the driver partitions the 185 files into 8 batches (its default
25-file size limit) plus one emit-module job. Total CPU is only ~36 s
(`sum user` across the nine frontend processes), but the per-process stats
timestamps show the jobs ran **strictly one after another**:

```
start=+ 0.0s end=+ 3.4s  all (emit-module)
start=+ 3.5s end=+ 8.5s  batch 1
start=+ 8.6s end=+10.5s  batch 2
…
start=+56.0s end=+65.4s  batch 8      sum wall=64.5s  sum user=35.9s
```

Cause: rules_swift does not pass `-j`, and swift-driver's default when
`-j` is absent is one job at a time. Bazel treats the whole action as one
CPU anyway, so an 8-batch module burns one core for a minute while the
rest idle.

With `-j 8` the eight batches start together (`start=+0.0s` for all
eight; `span=42.4s` at load ~65, where each process was starved to
~20% of a core):

| config                       | load avg | action wall |
| ---------------------------- | -------- | ----------- |
| before, `-j` absent          | 23-43    | 91 s        |
| before, `-j` absent (+stats) | 27-57    | 67 s        |
| after, `-j 8` via `copts`    | 57-69    | **24 s**    |

On an idle machine the span is bounded by the longest batch (~8-10 s of
CPU). This is the change shipped in `BUILD.bazel`.

### Type-check hotspots (`-Xfrontend -debug-time-function-bodies`)

10,110 function bodies, 14.6 s total (wall, loaded host). Flat
distribution: 28 bodies over 50 ms, 7 over 100 ms, and one outlier:

| body                                                                                             | time    |
| ------------------------------------------------------------------------------------------------ | ------- |
| `ContentView.body` getter (`ContentView.swift:35`, a ~450-line single modifier-chain expression) | 1184 ms |
| `WorkBoardCardItem.body`                                                                         | 196 ms  |
| `DriverQuotaRow.body`                                                                            | 152 ms  |
| `BossMacApp.body`                                                                                | 124 ms  |
| `EngineClient.decodeLine`                                                                        | 123 ms  |

`ContentView.body` is 8% of all body type-checking and 6x the next
entry, but still only ~1 s of a 36-108 s compile, and that file is an
active merge-conflict surface, so it was left alone here. If someone is
in there anyway, splitting the chain after `.toolbar { … }` into a
couple of `private var` stages (same modifier order) is the fix.

## 3. Why the module was not split

A split along real dependency boundaries was the planned remedy. A
type-reference graph over the 185 files (top-level type declarations ->
files that mention them, an under-approximation) finds a 41-file
strongly-connected component that includes `ChatViewModel`, every
`Models*.swift`, `EngineClient`/`EngineEvent`, the Comments stack and a
dozen views. The cycles run models -> views: `Models.swift` needs
`RevisionChainItem` (declared in `TranscriptViewerView.swift`) and
`AIReviewBadgePresentation` (`WorkCardBadges.swift`);
`Models+WorkBoard.swift` needs `WorkBoardSectionCollapse`
(`WorkBoardToolbar.swift`); `Models+Products.swift` needs
`CommentArtifactRef` (`Comments/CommentLayer.swift`). Counting member
references (view code calling `ChatViewModel` extension methods spread
across 30 files) the component grows to 174 of 185 files. Only ~44 leaf
files (Diagnostics, Search, Update views, a few model files) could be
carved out without moving types, and every crossing needs `package`/
`public` annotations plus `@testable import` changes in 100+ test files.

Against that cost, the measured payoff is small: the release build's
own serial floor is ~1 minute, and a split only parallelises it on a
host with spare cores, which is exactly what the CI Macs don't have
today. Splitting would not have moved the 243 s number.

## 4. Recommended CI-side change (not made here)

The release step's wall time is set by how many Bazel servers share one
Mac. Options, in order of preference:

1. Give the `macos-arm64` queue a host with no `bazel-any` agents (the
   Linux fleet already has five `bazel-any` agents), or run exactly one
   agent per Mac host for Bazel steps.
2. Failing that, cap each CI Bazel server's view of the machine so N
   concurrent servers don't each schedule for 10-12 cores, e.g.
   `build:ci-darwin --local_cpu_resources=HOST_CPUS*0.5` (the opt Swift
   action still uses `-num-threads 12`, so this only limits the
   surrounding Bazel scheduling).

Re-measure `boss-release` after that change; if the opt compile is
then ~65-75 s and still the critical path, revisit a split using the
graph above (start by moving the four view-declared model types into
`Models*.swift`).

## Reproducing

```sh
# wall time of the action
bazel build //tools/boss/app-macos:boss_mac_app_lib --profile=/tmp/p.gz
# per-phase CPU from the frontend
bazel build //tools/boss/app-macos:boss_mac_app_lib \
  --@build_bazel_rules_swift//swift:copt=-stats-output-dir \
  --@build_bazel_rules_swift//swift:copt=/tmp/swift-stats
# per-function type-check time (2+ MB of stderr; raise the UI cap)
bazel build //tools/boss/app-macos:boss_mac_app_lib \
  --experimental_ui_max_stdouterr_bytes=-1 \
  --@build_bazel_rules_swift//swift:copt=-Xfrontend \
  --@build_bazel_rules_swift//swift:copt=-debug-time-function-bodies
```

Add `-c opt` for the release configuration. The `stats-*.json` file
names carry the process start time in microseconds, which is how the
job schedule above was reconstructed. `-driver-time-compilation` is
silently ignored by the current swift-driver; use `-stats-output-dir`.
