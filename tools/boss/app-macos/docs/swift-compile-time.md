# Swift compile measurements and decision

## Scope and limitations

Measured on 2026-10-12. The original investigation used main at
`095e809a`, including the 185-file `boss_mac_app_lib` module (~58k lines).
The revision measures a real one-file edit on the PR base, with Xcode 27.0
(27A266a), Swift 6.4, Apple M2 Max (12 cores). CI uses Xcode
26.5.0.17F42, so these are not controlled cross-host comparisons.

**Host contention is a plausible contributor, not a measured dominant cause.**
The original 65–75 s idle estimate and 3x contention multiplier are withdrawn.
No exclusive idle host was available: local load averages sampled during this
revision ranged from 45 to 124. CPU time does not establish idle wall time.
Idle cold and incremental measurements still require an exclusive host; retain
the normal Bazel caches and measure an ordinarily invalidated compile action.

## CI observations

[Build 20478](https://buildkite.com/flunge/mono/builds/20478), `boss-release`
on `anaplian.local-2`, reported 243 s compiling this module, approximately
18:58:43–19:02:47 UTC. The host had four Buildkite agents (two macos-arm64,
two bazel-any). Overlapping jobs were:

| Job                                     | Agent                | Start (UTC) |
| --------------------------------------- | -------------------- | ----------- |
| mono 20480 bazel-build-test             | anaplian.local-2-any | 18:59:25    |
| mono 20481 mac-app-build                | anaplian.local-1     | 19:00:15    |
| mono-integrity 6521 integrity-checkleft | anaplian.local-1-any | 19:00:21    |

Correction: 20481 did **not** execute another Swift compile, as originally
claimed. Its build summary reports zero workers; Swift actions were cached.
Packaging/tests and other builds can still compete for resources, but job
overlap is not a measurement of CPU contention.

Additional samples from 2026-10-12, all selecting Xcode 26.5.0.17F42:

| Sample                                                          | Host           | Module compile interval / duration                                                                | Other host jobs                                                                  |
| --------------------------------------------------------------- | -------------- | ------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| [20490 release](https://buildkite.com/flunge/mono/builds/20490) | anaplian.local | start ~19:41:33; still compiling at 19:42:51 (78 s); completed by 19:42:56; approximately 78–84 s | 20494 bazel-build-test, 19:41:12–19:44:01                                        |
| [20481 mac-app](https://buildkite.com/flunge/mono/builds/20481) | anaplian.local | cached; zero worker actions; build invocation 370.35 s is **not** compile time                    | 20478 release, 20480 bazel-build-test and integrity 6521 during parts of the job |
| [20476 mac-app](https://buildkite.com/flunge/mono/builds/20476) | anaplian.local | cached; zero worker actions; build invocation 82.71 s is **not** compile time                     | none in returned organisation job history during 18:51:52–18:53:19               |

The 20490 interval is bounded by timestamped progress messages, not an action
profile. Its only worker action was the Swift compile. Source revisions differ
between samples. The shorter compile with fewer overlapping jobs supports
investigating contention, but cannot establish causality or its contribution.

Mac-app samples 20474, 20482 and 20490 also reported zero worker actions.
Thus an executed mac-app compile and an uncontended executed compile remain
unavailable from the inspected samples. The uncontended _job_ in 20476 does
not prove an idle host: non-Buildkite work is invisible to this history.

Method: `bk api '/builds?created_from=2026-10-12T00:00:00Z&per_page=100'`
for organisation-wide job intervals, then
`/pipelines/mono/builds/<number>/jobs/<uuid>/log` for timestamped logs.
Historical CPU utilisation, load and memory-pressure telemetry were not present
in those records/logs; the CI runbooks identify no historical metrics endpoint.
Historical host metrics remain unavailable through these investigation surfaces.

## Local measurements

### Historical opt and fastbuild runs

The original investigation used Bazel profiles and Swift frontend statistics,
with caches enabled throughout. The loaded-host observations were:

| Configuration                         | Load average | Action wall | Frontend user CPU |
| ------------------------------------- | ------------ | ----------- | ----------------- |
| opt, WMO                              | ~25          | 82 s        | unavailable       |
| opt, WMO with stats                   | ~113         | 275 s       | 108 s             |
| fastbuild, no driver parallelism flag | 23–43        | 91 s        | ~36 s             |
| fastbuild, no flag with stats         | 27–57        | 67 s        | 36 s              |
| fastbuild, eight jobs                 | 57–69        | 24 s        | 46 s              |

Opt CPU phases were type checking 14.4 s, SILGen 5.5 s, SIL optimization
39.3 s, IRGen 3.1 s and LLVM 39.4 s. WMO uses `-num-threads 12`;
serial phase CPU totals do not predict idle wall time on CI hardware.

The unconfigured fastbuild driver ran one emit-module job and eight batches
sequentially. Eight jobs improved one local observation but were not validated
on CI and oversubscribed Bazel's one-CPU action accounting.

### Revision: real one-file incremental experiment

The revision uses **two frontend jobs in fastbuild only**. Both measurements
below use the same statistics and type-check flags, with normal caches enabled.
The baseline compiled after the real BUILD change from eight to two jobs.
Then only `Sources/ContentView.swift` changed: the modifier chain was divided
into three computed properties. No cache clean, bypass or artificial source
edit was used. This is an action-cold baseline, not an idle-machine cold build.

| Measurement                       | Compile action wall | Frontend CPU                 | Executed actions                                                                           |
| --------------------------------- | ------------------- | ---------------------------- | ------------------------------------------------------------------------------------------ |
| baseline, before ContentView edit | 27.16 s             | 39.97 s user + 2.66 s system | 7 Swift workers across dependencies and Boss; 1 internal; 1 disk hit; 40 action-cache hits |
| one-file ContentView edit         | 76.32 s             | 38.05 s user + 4.52 s system | 1 Swift worker (Boss), 1 archive sandbox action, 1 internal                                |

Total Bazel invocation times were 70.96 s and 77.42 s respectively. Load
samples around the baseline were 116.66 and 45.76; around the edit build,
45.76 and 47.59 (one-minute averages). These averages lag actual scheduling;
the incremental action's longer wall time is **not** evidence that the
refactor regressed compiler CPU cost.

The edit rebuilt all eight batches plus emit-module. Batch user CPU, identified
by the stats filename's first primary file:

| First primary                    | User CPU |
| -------------------------------- | -------- |
| all (emit-module)                | 3.91 s   |
| ActivityLogView.swift            | 6.16 s   |
| CommentHighlightingParser.swift  | 6.66 s   |
| ChatViewModel_FeatureFlags.swift | 1.96 s   |
| SpawnCapability.swift            | 4.28 s   |
| FlowLayout.swift                 | 3.90 s   |
| Models_Attachments.swift         | 3.46 s   |
| RevealCardViewport.swift         | 3.30 s   |
| ReleaseNotesContent.swift        | 4.43 s   |

The baseline statistics directory was initially missing. JSON emission failed,
but the successful compiler action retained its timing tables in stderr.
A subsequent cache hit recovered that stderr with the UI output cap removed;
it was not counted as a new compile measurement.

Type-check profiling measured the original `ContentView.body` at 912.56 ms
in this baseline (1184 ms in the earlier investigation). After extraction:
`navigationContent` 48.23 ms, `configuredContent` 74.77 ms, `body` 88.24 ms.
These are wall times on a loaded host, not a controlled speedup ratio.
Computed properties preserve the root ZStack, modifier order, state ownership
and conditional mounting; no new View wrapper is introduced.

## Shipped compiler setting and split decision

`BUILD.bazel` selects `-j 2` for fastbuild only; opt and dbg receive no
added driver flag. This avoids changing opt action inputs through this setting.
Bazel still accounts for the complete Swift action as one CPU, so even two
frontends can exceed that accounting. Two is a conservative compromise, not a
claim of CI speedup; `--local_cpu_resources` does not constrain Swift's internal
parallelism.

The one-file experiment confirms that a source edit still rebuilds this whole
module. A split could reduce that cost. However, the original type-reference
graph found a 41-file strongly connected component spanning ChatViewModel,
Models, EngineClient/EngineEvent, Comments and views; including references
to ChatViewModel extension members expanded it to 174 of 185 files.
About 44 leaf files could be extracted without moving types, with access/import
changes in over 100 test files.

Concrete reverse dependencies include Models needing RevisionChainItem from
TranscriptViewerView and AIReviewBadgePresentation from WorkCardBadges;
Models+WorkBoard needing WorkBoardSectionCollapse from WorkBoardToolbar;
and Models+Products needing CommentArtifactRef from Comments/CommentLayer.
This makes a split invasive, not useless. Keep the module intact in this PR
while fixing the small measured type-check hotspot. The earlier claim that a
split could not improve the 243 s observation was unsupported. Reassess using
idle measurements and representative edit costs on the CI toolchain.

## CI operator recommendation

The CI fleet operator can test the contention hypothesis by dedicating a
macos-arm64 host without bazel-any agents, or using one agent per Mac for
Bazel jobs. This operational change is not delivered by this PR.
An alternative is reducing each server's `local_cpu_resources`; that limits
surrounding actions but cannot account for Swift's internal threads
(including the existing opt `-num-threads 12`). Measure before choosing a
fleet-wide setting; this PR changes no agent configuration.

## Reproduction

Create the statistics output directory first. Keep the same compiler flags
and directory for the baseline and real one-file edit, and use separate Bazel
profile filenames. Profile filenames and UI limits do not change action keys.

```sh
mkdir -p /tmp/swift-stats
bazel build //tools/boss/app-macos:boss_mac_app_lib \
  --profile=/tmp/before.gz \
  --experimental_ui_max_stdouterr_bytes=-1 \
  --@build_bazel_rules_swift//swift:copt=-stats-output-dir \
  --@build_bazel_rules_swift//swift:copt=/tmp/swift-stats \
  --@build_bazel_rules_swift//swift:copt=-Xfrontend \
  --@build_bazel_rules_swift//swift:copt=-debug-time-function-bodies
```

After a real source change, repeat with `--profile=/tmp/after.gz`.
Sum `time.swift-frontend.*.user` and `.sys` in Boss statistics only.
Use the profile's SwiftCompile action-processing duration, not total build
time. Stats filenames identify the first primary file of each batch, not
the CPU of an individual file. Add `-c opt` for the release configuration.

## Validation on the updated base

On 2026-10-27, after integrating main at `dd4935c4`, the full mac-app CI
target set was validated again with normal Bazel caches:

- `bazel build //tools/boss/app-macos/... //tools/boss/installer/...` passed.
- `bazel test --test_output=errors //tools/boss/app-macos/... //tools/boss/installer/...`
  passed all 14 targets (9 executed, 5 cached), including the reconnect
  integration test and installer payload test.

The build initially reported a missing remote-cache input for the unrelated
`libsqlite3-sys` build script. The repository wrapper retried automatically
and the build passed without changes or cache overrides. This is an
environmental cache-availability failure, not a Swift compile failure.
The host load averages sampled before compilation were 185.36, 192.48 and
153.09; this validation does not supply the missing idle-host timings or
replace the instrumented one-file experiment above.
