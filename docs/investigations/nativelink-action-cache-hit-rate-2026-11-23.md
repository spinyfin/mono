# Why the NativeLink hub's action-cache hit rate drops when CI gets busy

- **Date:** 2026-11-23. All times are America/New_York (EST, UTC−05:00).
- **Window studied:** 2026-11-22 10:00 to 2026-11-23 10:00, the period shown on the "Action cache hit rate" panel of the NativeLink overview dashboard on `headcrash`.
- **Status:** root-caused. No code is changed here; the proposed changes are listed at the end for filing separately.
- **Evidence read:** Prometheus and the hub journal on `headcrash` (read-only), the dashboard JSON and hub config, 765 Buildkite job logs and 871 compact execution logs from the `mono` pipeline, Bazel source at 9.1.0 and 9.3.3, NativeLink source at v1.8.6, and Bazel server logs on the laptop.
- **Related:** [Postmortem: remote-execution outage on 2026-11-18](remote-execution-outage-2026-11-18-postmortem.md), [NativeLink remote execution: findings and a small macOS worker pool](nativelink-macos-remote-execution.md).

The panel falls to 20–30% whenever real work arrives and reads 100% overnight. Both readings are correct for what the panel counts, and neither says how well caching works. The low reading has two parts: a Bazel upgrade that stopped most cache hits from reaching the hub, and builds that genuinely execute several hundred actions each, about three quarters of them because a source file they depend on changed.

## Verdict

- **The panel is arithmetically right and badly named.** It is `GetActionResult` answers: found over found-plus-not-found. Every not-found is followed by one execution (63,945 misses against 64,239 `Execute` calls in 24 hours), so it is really "of the actions that reached the hub, the share that did not have to run".
- **Most cache hits never reach the hub, and since Bazel 9.3 almost none do.** In CI, 86.6% of actions are answered by the agent's own output base and 8.4% by its disk cache. Bazel 9.3.0 stopped checking that a disk-cache result's files are present, so those lookups no longer fall through to the hub. A scheduled build of unchanged `main` made about 420 hub lookups per job on 9.1.0 and makes none on 9.3.3. The same builds that read 30% on the hub were 76.5% cached at build level.
- **The misses are real.** Of 22,907 remote executions in the logs, none repeated an action that had already finished elsewhere, and no action produced different bytes when it ran twice. Nothing in the sample was lost or evicted.
- **About three quarters of the misses are source changes and what follows from them.** 51% trace to a changed source file in the first build of a commit, and 24% are the second, uncacheable spawn Bazel runs after every test that ran. Stacked PRs each needed 230 to 350 actions because the files they touched sit under most of the engine: 3 of the 7 sampled edited `tools/boss/protocol/src/wire*`, which 190 to 240 actions per build depend on, and 2 more edited `work-db`.
- **About 18% have nothing to do with the change under test.** A commit rebuilt for the other operating system (4.1% directly, 9.0% of executions counting everything in those jobs), a per-agent-host fingerprint in every Linux test key (4.1%), the per-commit stamp (4.1%), the same action running in two builds at once (3.3%), and a flag that splits CI's own Rust compiles (2.2%). The changes proposed below address four of the five.
- **Eviction is not a cause today, and upload failures are not a cause in CI.** The upload error reported from another repository is real, comes only from the laptop, predates Bazel 9.2, and fails the build that hits it instead of silently skipping the cache.

## How the evidence is labelled

| Label     | Meaning                                                                                      |
| --------- | -------------------------------------------------------------------------------------------- |
| Measured  | Counted in this investigation from a named source. The method section says how to repeat it. |
| Source    | Read in Bazel's or NativeLink's source at the version named.                                 |
| Inference | Reasoning from the two above. The text says what would confirm it.                           |

The execution-log sample is every compact execution log the `mono` pipeline uploaded between the change that introduced them (#3873) and 09:31 on 11-23: 871 logs from builds 28765 to 28913, holding 175,890 spawns. A log records every spawn Bazel ran or took from a disk or remote cache, with its action digest, arguments, environment and inputs. It does not record an action answered by the output base's own action cache.

## 1. What the panel measures

The panel's query (measured, from `nativelink-overview.json`):

```
100 * sum(rate(nativelink_rpc_server_duration_seconds_count{rpc_method="GetActionResult",rpc_grpc_status_code="0"}[5m]))
    / sum(rate(nativelink_rpc_server_duration_seconds_count{rpc_method="GetActionResult",rpc_grpc_status_code=~"0|5"}[5m]))
```

- **A hit** is a `GetActionResult` call the hub answered OK. **A miss** is one it answered `NOT_FOUND`. Nothing else is counted: not `FindMissingBlobs`, not reads or writes.
- **The metric has no client label.** It cannot be split by CI, laptop, pipeline or Bazel version. Only `GetCapabilities` calls are logged with a client address.
- **A miss is an execution.** Over 24 hours the hub counted 63,945 misses, 64,239 `Execute` calls, 61,487 completed executions and 61,378 stored results. Bazel does not look up speculatively: it asks immediately before running a spawn, and in CI a miss cannot run on the agent instead (`--noremote_local_fallback`; 1,334 of 1.97 million CI actions ran locally).
- **An evicted result reads as a miss.** The action cache is wrapped in `completeness_checking`, which answers `NOT_FOUND` when any output is gone from the CAS. The metric cannot tell that from a result that never existed. Section 5 shows it is not happening at a measurable rate.

What does not reach the hub matters more than what does. Bazel answers an action from three places before it asks the hub, and its own end-of-build summary counts each (measured, 765 job logs, 20:55 on 11-22 to 09:31 on 11-23):

| Answered by                    | Actions   | Share | Seen by the hub |
| ------------------------------ | --------- | ----- | --------------- |
| The output base's action cache | 1,709,900 | 86.6% | No              |
| The agent host's disk cache    | 166,357   | 8.4%  | No              |
| The hub's action cache (a hit) | 58,661    | 3.0%  | Yes             |
| Remote execution (a miss)      | 37,288    | 1.9%  | Yes             |
| Run on the agent               | 1,334     | 0.1%  | No              |

CI did not execute 98% of its actions. The hub saw 4.9% of them and the panel is computed from that slice alone.

### Bazel 9.3 moved the hits off the hub

CI's verification builds use `--remote_download_minimal`, so a result taken from the hub is recorded in the agent's disk cache without its output files. Up to 9.2 the disk cache refused to serve such a record (source, `DiskCacheClient.java` at 9.1.0: "If at least one of the referenced blobs is missing, consider the action result to be stale"), so the lookup fell through to the hub, which answered it: a hit on the panel. Bazel 9.3.0 made that check conditional (source, `RemoteModule.java` at 9.3.3; the variable does not exist at 9.0.0, 9.1.0 or 9.2.0):

```java
// Verifying that the blobs referenced by a disk cache action result are present locally turns
// most disk cache hits into misses when Build without the Bytes is enabled, ... This allows disk
// cache AC checks to be entirely local, reducing server load and avoiding a network round trip.
boolean checkDiskCacheActionResultIntegrity =
    buildRequestOptions == null || !buildRequestOptions.rewindLostInputs;
```

`--rewind_lost_inputs` defaults to true in 9.3.3, so the check is off. `mono` moved from 9.1.0 to 9.3.3 at 22:47 on 11-22 (#3880).

The scheduled build of `main` every 15 minutes is the same work before and after (measured, `bazel-build-test` job logs):

| Build | When        | Agent     | Bazel | Bazel's summary                                                 |
| ----- | ----------- | --------- | ----- | --------------------------------------------------------------- |
| 28722 | 11-22 17:45 | headcrash | 9.1.0 | 6,060 action cache hit, 420 remote cache hit                    |
| 28725 | 11-22 18:30 | rascal    | 9.1.0 | 6,075 action cache hit, 1 disk cache hit, 420 remote cache hit  |
| 28837 | 11-23 02:00 | headcrash | 9.3.3 | 1,465 action cache hit, 2,107 disk cache hit; no remote lookups |
| 28851 | 11-23 05:30 | headcrash | 9.3.3 | 5,609 action cache hit, 97 disk cache hit; no remote lookups    |

Between 01:00 and 07:00 on 11-23 the 24 scheduled `mono` builds made no hub lookups at all: their logs hold 11,206 disk-cache hits, no remote hits and no executions. The hub still counted 2,122 hits in those hours, in four bursts of a minute or two at 01:02, 02:02, 03:32 and 05:32. The two that were checked line up with the `integrity` job of the separate `mono-integrity` pipeline (02:00:10 to 02:02:49 on `headcrash`, 05:30:11 to 05:33:04 on `anaplian`). The overnight 100% is a few hundred lookups from another pipeline that builds an unchanged tree.

### What the number means

Of the actions that no cache on the asking machine could answer, the share that another machine had already built. A machine asks the hub about a given action roughly once: after that its own disk cache answers. So the panel is close to 100% when nothing new is being built, and close to the share of work that is new to the asking host but not new to the fleet when something is. It cannot go high during development, and it would read low on a perfectly healthy cache.

## 2. The dips, one by one

| When (11-22 unless noted) | Hub misses | Hub hits | What it was                                                                                                                                                                                                                                                                                                                                                                                                                       |
| ------------------------- | ---------- | -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 14:50 to 14:55            | about 16   | 0        | The first lookups after the hub restart marked at 14:45. One five-minute point at 0%.                                                                                                                                                                                                                                                                                                                                             |
| 17:15 to 17:40            | 4,533      | 1,426    | One build. Scheduled build 28720 was the first Linux job to execute against this hub: 3,430 executions in `bazel-build-test`, of a tree no Linux worker here had built. The one-hour line stayed at 18–30% for an hour afterwards. There was no other activity.                                                                                                                                                                   |
| 21:00 to 01:00            | 33,852     | 102,923  | The other Linux agents began using this hub at about 21:00 (#3884 merged at 21:03), and it held no Linux results; around the two hub restarts at 21:39 and 21:52, 13 builds failed with `UNAVAILABLE` and were built again; and the Bazel upgrade at 22:47 gave every test a new key on both systems, because a test takes Bazel's embedded tools as inputs. Still on 9.1.0 for most of it, so hits reached the hub: 75% overall. |
| 07:00 to 10:00 on 11-23   | 25,361     | 7,707    | Ordinary PR and merge-queue traffic, all on 9.3.3: 23%. This is the steady state to expect from now on.                                                                                                                                                                                                                                                                                                                           |

The annotation at 14:45 is not what the 14:45 to 18:30 trough was. The trough is a single cold build two and a half hours later, drawn wide by the one-hour window.

## 3. Is the disk-cache explanation right?

Half right. It explains the rate and not the load.

It is right that repeats are answered locally and never reach the hub, and section 1 gives the mechanism and the date it became nearly total. Measured on the same spawns (execution logs):

| Sample               | Spawns  | Disk-cache hits | Hub hits | Remote executions | Cached, build level | Hub-visible rate |
| -------------------- | ------- | --------------- | -------- | ----------------- | ------------------- | ---------------- |
| All logs             | 175,890 | 128,719         | 24,264   | 22,907            | 87.0%               | 51.4%            |
| 11-23 07:00 to 09:31 | 61,145  | 40,591          | 6,199    | 14,355            | 76.5%               | 30.2%            |

A cache shared by all agents in place of the per-host disk caches would raise the panel to about those build-level figures and would not remove one execution. The disk cache does not hide work; it hides hits.

It does not explain why stacked PRs that touch a few files each run 250 to 470 actions. They do, the hub is not being spared anything it could have answered, and the disk cache has nothing to do with it. Section 4 is about why.

## 4. Are the misses real, and what caused each one?

### Method

Every remotely executed spawn in the logs was compared with earlier spawns of the same action (same label, mnemonic and output) that had a different key. The two were diffed: environment, arguments, platform properties, status files, main-repository sources, external inputs, generated inputs. A generated input that differed was traced to the two spawns that produced the two versions, and those were diffed in turn, down to a root. Where several earlier variants existed, the one with the mildest difference was kept, so an action is only put down to a source change if no variant existed that differed in something else alone.

Two checks ran first. An action whose key had already been executed, or served from a cache, was set aside as a repeat. And every key that executed more than once was checked for identical output bytes.

Limits: an earlier variant that an agent answered from its output base is not in any log, so the comparison sometimes falls back to an older variant and reports a source difference where a closer one existed. That biases the table towards "source change" and against the non-source rows, which are therefore minimums. The commit-level table further down does not have this bias.

### Result

| Why the action had no usable cached result                                                           | All logs | Share | 11-23 07:00 to 09:31 | Share |
| ---------------------------------------------------------------------------------------------------- | -------- | ----- | -------------------- | ----- |
| A source file it depends on differs; first build of that commit                                      | 11,764   | 51.4% | 8,453                | 58.9% |
| The `test.xml` spawn that follows any test that ran                                                  | 5,391    | 23.5% | 3,182                | 22.2% |
| No earlier variant in the sample (mostly the sample's first 40 minutes)                              | 989      | 4.3%  | 58                   | 0.4%  |
| Only the stamp input differs (`stable-status.txt`)                                                   | 944      | 4.1%  | 680                  | 4.7%  |
| Source differs from the nearest variant, but the commit had been built on the other operating system | 928      | 4.1%  | 707                  | 4.9%  |
| Only the agent host's test-runtime fingerprint differs                                               | 928      | 4.1%  | 516                  | 3.6%  |
| The same key was already executing in a concurrent build                                             | 746      | 3.3%  | 355                  | 2.5%  |
| Only Bazel's embedded tools differ (9.1.0 against 9.3.3)                                             | 634      | 2.8%  | 2                    | 0.0%  |
| Only `-Cstrip=symbols` differs                                                                       | 509      | 2.2%  | 402                  | 2.8%  |
| Everything else                                                                                      | 74       | 0.3%  | 0                    | 0.0%  |
| **Total**                                                                                            | 22,907   |       | 14,355               |       |

"Everything else" is 56 source differences in jobs whose commit had run on the same system before, 10 actions of the release step (whose action environment differs from every other step's by design, so it shares nothing with them), 7 that trace to an upstream output with two versions under one key, and one test retried after it failed.

What was looked for and not found:

- **A repeat of finished work.** No action was executed after an execution of the same key had completed elsewhere. Nine (0.04%, all tests) ran 60 to 90 minutes after another job had been served that key from a cache; they are unexplained and too few to matter.
- **Non-determinism.** About 800 keys executed twice or more. None produced different bytes for the same output, test logs aside.
- **Environment drift.** No environment variable differed between variants except in the release step. No toolchain, SDK, Xcode or absolute-path input differed between agents of one operating system, apart from the fingerprint below. A laptop build agreed with CI's keys on 14 of 17 actions (section 6).
- **Platform properties.** None differed within an operating system.

### The stacked merge-queue builds

The PRs that were stacked in the merge queue from 07:30 on 11-23, in queue order. Each `bazel-build-test` job's executions were traced to source files and checked against that PR's own diff (measured):

| Build | PR    | Files changed | Agent (OS)         | Executed | Traced to a file this PR changed | Most common root file, and actions tracing to it                  |
| ----- | ----- | ------------- | ------------------ | -------- | -------------------------------- | ----------------------------------------------------------------- |
| 28862 | #3885 | 9             | empiricist (Linux) | 7        | 1                                | Scripts and docs only. 6 of the 7 are the stamp.                  |
| 28863 | #3840 | 9             | empiricist (Linux) | 296      | 236                              | `tools/boss/protocol/src/wire/events.rs`: 235                     |
| 28865 | #3835 | 7             | headcrash (Linux)  | 304      | 190                              | `tools/boss/protocol/src/wire.rs`: 189                            |
| 28868 | #3860 | 16            | diziet (Linux)     | 249      | 144                              | `tools/boss/engine/work-db/src/schema_migration_registry.rs`: 142 |
| 28870 | #3856 | 25            | anaplian (macOS)   | 321      | 151, and 62 to earlier entries   | `tools/boss/protocol/src/wire.rs` (earlier entries): 210          |
| 28876 | #3870 | 16            | zoologist (Linux)  | 354      | 242                              | `tools/boss/protocol/src/wire/events.rs`: 241                     |
| 28883 | #3882 | 12            | headcrash (Linux)  | 228      | 89                               | `tools/boss/engine/spoke-adapter/src/lib.rs`: 89                  |

No execution traced to a source file outside the queue's own diffs. The remainder in each row is mostly `test.xml` spawns and other actions whose comparison partner was not in the logs (55 to 94 per job), then strip, stamp and fingerprint differences in single or double digits. PR #3868 was queued behind #3870 and is not in the table: its queue build 28877 (469 executions) failed and that commit never reached `main`.

So the answer to "a few files should not need 250 to 470 fresh actions" is that these few files do. `boss-protocol` and `work-db` sit under nearly every engine crate. A change to one recompiles each dependent crate and its test crate, relinks each test binary, and re-runs each test: up to 180 compiles and 120 to 175 test spawns per job. There is no early cutoff: in every traced case an edit to a library changed its compiled output, and every dependent takes that file as an input. Each queue entry builds on the one before, so each is a tree nobody has built, and the entries build at the same time. PR #3885, which touched no Rust, cost 7.

### What is not explained by the change under test

**The same commit, the other operating system.** `bazel-build-test` and `checks` run on the `bazel-any` queue, which mixes four Linux and six macOS hosts (72 Linux and 40 macOS `bazel-build-test` jobs in the sample). A merge-queue commit and the `main` commit it becomes are the same tree, but land on whichever agent is free. Counting every execution by whether that commit and step had run before:

| Step               | State of the commit when the job ran | Jobs | Executions | Per job |
| ------------------ | ------------------------------------ | ---- | ---------- | ------- |
| `bazel-build-test` | Never built                          | 55   | 14,033     | 255     |
| `bazel-build-test` | Built, on the other system only      | 9    | 1,839      | 204     |
| `bazel-build-test` | Built on this system                 | 48   | 1,157      | 24      |
| `checks`           | Never built                          | 49   | 1,306      | 27      |
| `checks`           | Built, on the other system only      | 6    | 219        | 37      |
| `checks`           | Built on this system                 | 32   | 0          | 0       |
| `mac-app-build`    | Never built                          | 55   | 3,528      | 64      |
| `mac-app-build`    | Built on this system                 | 57   | 111        | 2       |

All nine of those `bazel-build-test` jobs were `main` builds. Build 28878 (1 execution, 299 remote hits) is the lucky case: its merge-queue build had also run on a Mac. Build 28874 is the unlucky one: 236 executions on `determinist` for a commit whose merge-queue build had run on `diziet`, on Linux, five minutes earlier. In total 2,058 executions (9.0%) were in jobs re-doing a commit for the second system.

**A per-host fingerprint in every Linux test key.** Every test takes the `fingerprint` file of the generated `test_runtime_tools` repository as an input. `tools/test-sandbox/repositories.bzl` writes it on the machine running Bazel, from that machine's OS build and the SHA-256 of each audited system tool, so that a host update invalidates cached test results. With remote execution the test runs on a worker, and the fingerprint still describes the agent. The four Linux agents have four different fingerprints; the five Macs seen in the sample share one (measured, digests in the logs):

| Hosts                                              | Kernel release                         | Distinct fingerprints |
| -------------------------------------------------- | -------------------------------------- | --------------------- |
| zoologist, empiricist, diziet, headcrash (Linux)   | 7.0.0-31, 7.0.0-34, 7.0.0-38, 7.0.0-44 | 4                     |
| anaplian, arbitrary, determinist, skaffen, zakalwe | Darwin 27.2.0 on all                   | 1                     |

A test result is therefore never shared between two Linux agents. Of the 1,268 executions in jobs whose commit had already run on the same system, 1,086 were Linux test spawns. 928 executions trace directly to a fingerprint-only difference; on `main` builds that is 15.9% of executions.

**The stamp.** `BuildProvenanceRs` reads `stable-status.txt` on every build for the commit SHA, so every commit recompiles the provenance crate, relinks the engine binaries and rebuilds the app bundle: 944 executions, 596 of them in `mac-app-build`. For PR #3885 that was 6 of 7 executions in `bazel-build-test` and 17 of 18 in `mac-app-build`. Separately, Bazel writes `BUILD_HOST` and `BUILD_USER` into the same file, so the four actions that read it are keyed per agent host: 54 of 55 commits had more than one version of the file, 2.9 on average across 2.4 hosts. That second effect costs only the four small actions, because their output is the same on every host.

**The second spawn of every test.** A Rust test does not write `test.xml`, so Bazel runs a second remote spawn to generate it from the log. Its inputs are the fresh `test.log` and its arguments include the measured duration and exit code, so it can never hit a cache. 5,391 of the 11,385 `TestRunner` executions are these: a quarter of all misses. Bazel 9.3.3 has no flag to merge it back into the test action (`--experimental_split_xml_generation` is not recognised).

**One compile, two flavours.** `bazel_verify_only` adds `--config=ci-strip`, which passes `-Cstrip=symbols` to every target-configuration `rustc` action, libraries included, although it changes only linked outputs. Invocations that build without it (the `repobin` build at the start of every step, and `checks`) compile the same crates under a second key: 509 executions.

**Concurrent builds.** 746 executions were of a key another build was executing at that moment. Speculative merge-queue builds and the `main` build of the previous entry overlap. The hub's `Execute` calls exceed its completed executions by 4%, which is the most the scheduler's merging of identical in-flight actions can be absorbing.

**Bazel versions.** 634 executions, all tests, differed from an existing result only in `external/bazel_tools`, that is, in the Bazel release. All but 2 were on the night of the upgrade. This is finished.

## 5. Eviction

- **The CAS is full and turning over.** It reached its cap (3,734,977,866,444 bytes) between 21:00 and 22:00 on 11-22 and has evicted as fast as it fills since: 334 GB written and 334 GB evicted between 22:00 and 23:00, 214 GB between 07:00 and 08:00, 307 GB between 09:00 and 10:00. That is 17 to 22 MB per executed action.
- **Retention today is about 37 hours, not 9.** The entries evicted on the morning of 11-23 were last used 37 to 38 hours earlier, because the weekend was quiet. At the busy-hour rates above the cap is 11 to 17 hours of writes; 9.2 hours corresponds to about 400 GB an hour. A full working day will bring retention down towards those figures.
- **Action-cache entries do outlive their blobs, by design.** The action cache holds 605,922 entries in 358 MB, 7% of its 5 GB cap, so it is not evicting. Its oldest entries are 140 hours old, far older than anything in the CAS. `completeness_checking` answers `NOT_FOUND` for an entry whose outputs are gone, which is the intended behaviour and is counted as a miss.
- **It is not costing executions in this window.** No key was re-executed after it had completed (section 4). The hub logged 1,236 reads of blobs it did not have (202 distinct blobs) between 20:00 on 11-21 and 23:00 on 11-22, 132 of them from `headcrash` during the cold Linux build at 17:15, and none on 11-23. "Lost inputs no longer available remotely" appears in 8 CI jobs, all between 17:15 and 21:55 on 11-22 and all around the Linux move and the hub restarts.

Eviction will become a cause if retention falls below the gap between a result being stored and being wanted again, which for third-party crates is weeks. Agents keep those in their disk caches and re-upload them as inputs, which is part of why write volume is so high. That is a capacity question for a working day's data, not something this window shows.

## 6. Traffic mix

**CI and everything else.** From 21:00 on 11-22 to 09:00 on 11-23 the hub counted 46,803 misses and 109,607 hits. The `mono` pipeline's job logs account for 33,193 executions and 56,223 remote hits: 71% of the misses and 51% of the hits. The rest, 13,610 misses and 53,384 hits, came from the laptop and from other pipelines on the same agents (`mono-integrity` at least, whose jobs match the overnight bursts). The hub's metrics cannot divide that remainder. By `GetCapabilities` calls, which are logged per client, the laptop made 2,184 of 8,221 (27%) over the window and 36 to 50% in the morning hours.

**Do laptop builds share keys with CI?** Yes, except for the strip flag. A normal developer-configuration build of `//tools/boss/engine/work-db:work-db` on the laptop at `main` produced 17 `rustc` spawns. 14 had exactly the action digest CI's Macs had recorded for the same crates. Of the other three, two differed from the nearest CI spawn only by `-Cstrip=symbols`. The third matched it in arguments, environment and sources, so it differed in a generated input; the nearest CI spawn was from a different commit. So paths, Xcode, toolchains and environment agree between the laptop and the CI Macs, and the strip flag is the only thing keeping laptop builds and CI's `bazel-build-test` from sharing compiles on macOS. Tests were not compared. Laptop results that ran locally are deliberately not uploaded (`--noremote_upload_local_results`).

**Bazel versions.**

- CI builds each commit with its own `.bazelversion`: 9.1.0 before #3880 and 9.3.3 after. Mixed versions overnight on 11-22 account for the 634 executions above and nothing since.
- `/usr/bin/bazel` on `headcrash` is a symlink to `bazelisk`, not a 9.1.0 binary. Outside a checkout it resolved to 9.3.3 when checked; in a checkout it follows `.bazelversion`.
- The laptop has 75 Bazel output bases: 36 last used with 9.1.0, 26 with 9.3.3, 11 with 9.0.0. Of the 29 used inside the window, 23 were on 9.3.3, 4 on 9.1.0 (last used between 16:53 on 11-22 and 00:34 on 11-23) and 2 on 9.0.0 in another repository's workspaces. A workspace on a branch cut before #3880 keeps using 9.1.0 until it is rebased, and shares no keys with 9.3.3 builds.

## 7. Compressed upload failures

The report from another repository is that Bazel 9.2.0's parallel chunk transfers (bazelbuild/bazel#29796), combined with `--remote_cache_compression`, make large uploads fail with `Received out of order compressed data. Got N, expected 0`, so results are never cached.

The error is real. The cause given is not, and it does not explain misses in `mono` CI.

- **It happens, on the laptop only.** The hub logged it 176 times between 19:00 on 11-21 and 09:45 on 11-23. Every one came from the laptop's address. None came from a CI agent or a worker. 765 CI job logs contain neither that text nor `INVALID_ARGUMENT`. Of 2.14 million `Write` calls in that period, 239 ended `INVALID_ARGUMENT`.
- **It predates 9.2.** At least 100 of the 176 were logged before `mono` left 9.1.0. A Bazel 9.1.0 server log on the laptop from 11-18 has the same error while uploading a Node.js binary; two 9.3.3 logs from 11-22 have it for a headless Chrome binary. All three are from another repository's workspaces.
- **#29796 is not on the path.** It is "Support bounded parallel chunk transfers" for `--experimental_remote_cache_chunking`, which defaults to false, is not set in this repository, and needs a server that advertises `SplitBlob` and `SpliceBlob`. The hub advertises neither.
- **The mechanism is a retry (source).** When an upload attempt fails, Bazel's `ByteStreamUploader` asks `QueryWriteStatus` and resumes from the offset it is given; the code is the same at 9.1.0 and 9.3.3. NativeLink 1.8.6 tracks a compressed upload's received bytes and reports them, but its compressed write path says "Compressed uploads intentionally do not support the identity upload resume protocol" and requires a new stream to start at offset 0. So any interrupted compressed upload whose first stream is still registered is resumed at a non-zero offset and refused. The offsets Bazel resumed at ranged up to 63 MB, with a median of 2.9 MB. `--noremote_cache_compression` avoids it because uncompressed uploads can resume.
- **It fails loudly.** In the laptop logs it is `(Exit 34): while uploading file …`, on an input of a remote test, and the build stops. It does not leave a result silently uncached.
- **Share of the miss volume: none in CI, negligible overall.** No CI client saw it, and no CI action was re-executed after completing. On the laptop, 176 errors at five attempts per file is about 35 failed uploads in 38 hours, against some 64,000 misses a day.

One real difference between the versions (source): 9.1.0's uploader set a deadline on each `Write` and `QueryWriteStatus` call and 9.3.3's does not. That changes what can trigger the first failure, not what happens after it.

## Proposed changes

Each is one reviewable change. The middle column gives the measured size of what the change addresses in the 22,907-execution sample; the effects overlap and do not add up exactly.

| #   | Change                                                                                                                                                                                                                           | Evidence it would move the number                                                                                                                                    | Caveats                                                                                                                                                                             |
| --- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | **Rename the panel and add the counts beside it.** Title it for what it is (hub lookups answered; excludes each agent's own caches), and plot executions per hour, which is the cost.                                            | Section 1. The panel read 30% on builds that were 76.5% cached.                                                                                                      | Moves no executions. It stops the next investigation of a healthy number.                                                                                                           |
| 2   | **Publish a build-level figure from CI.** At the end of each step, sum the execution logs the step already writes into action-cache, disk, remote-hit and executed counts, annotate the build, and export them.                  | The four numbers already exist per step in `bazel-execlogs/` and in Bazel's summary line.                                                                            | Covers CI only.                                                                                                                                                                     |
| 3   | **Run a commit's `bazel-build-test` and `checks` on one operating system.** Pin the steps' agents, or choose the system from the commit so the merge-queue and `main` builds agree.                                              | 2,058 executions (9.0%). A `main` `bazel-build-test` job cost 204 executions when the queue had used the other system and 24 when it had not.                        | Today each commit's Rust tests run on a random one of the two systems; pinning makes that deliberate. Which system, and whether the queue should run both, is the owner's decision. |
| 4   | **Key remote tests on the worker pool, not the submitting agent.** When tests execute remotely, have the test-runtime fingerprint come from a value pinned with the worker image instead of the agent's kernel and tool digests. | 928 executions traced directly (4.1%); 1,086 Linux test spawns re-run for commits already tested on Linux. Four Linux agents, four fingerprints; the Macs, one.      | The fingerprint exists so a host update invalidates results. The pinned value has to change when the workers' tools change, or stale results are served.                            |
| 5   | **Write `test.xml` from the test wrapper.** `hermetic_test_wrapper` already runs around every test; have it write `$XML_OUTPUT_FILE`, so Bazel does not start the generating spawn.                                              | 5,391 executions (23.5%), none of which can ever hit. In the morning sample the panel would read 35.7% instead of 30.2% with no other change.                        | Inference: that Bazel skips the spawn when the remote test returns the file should be confirmed on one target first. The flaky-test report reads `test.xml` and must keep working.  |
| 6   | **Apply the strip flag to link actions only.** Replace `extra_rustc_flag=-Cstrip=symbols` with a mechanism that reaches binaries and tests but not library compiles.                                                             | 509 executions (2.2%) inside CI. It is also the only key difference found between laptop builds and CI's verification builds on macOS (14 of 17 keys already equal). | Needs a check that the replacement leaves library keys untouched under the pinned `rules_rust`.                                                                                     |
| 7   | **Give the stamp actions a filtered status file.** A small action copies only the `STABLE_BOSS_*` keys they need out of `stable-status.txt`; the stamp actions read that.                                                        | Removes the per-host key (`BUILD_HOST`, `BUILD_USER`) from four actions per job. Sets up the larger saving below.                                                    | Alone this saves only those small actions.                                                                                                                                          |
| 8   | **Use the placeholder SHA in verification builds.** As is already done for the version, so a commit that changes no engine source relinks nothing.                                                                               | 944 executions (4.1%), and a floor of about 24 executions on every commit, however small (PR #3885: 23 of 25).                                                       | Binaries from verification builds would no longer carry their SHA. They do not ship; the owner should confirm nothing reads it.                                                     |
| 9   | **Have the hub report no progress for a compressed upload.** Patch `inner_query_write_status` to answer 0 for a compressed upload, so Bazel restarts it instead of resuming, and offer the fix upstream.                         | Section 7: 176 failures, all a resume that the write path refuses.                                                                                                   | Not a hit-rate change. It fixes failed laptop builds with large inputs.                                                                                                             |

Not proposed:

- **Replacing the agents' disk caches with the hub.** It would raise the panel to about 76–87% and execute exactly as much. `.ci.bazelrc` also records why the disk cache has to stay in front.
- **Anything for eviction or for the upload error in CI.** Neither is costing executions now.

The largest item has no single change. Half of all executions are a commit's first build of code that really changed, and their number is set by how much depends on the files that change most. Three of seven sampled PRs edited `tools/boss/protocol/src/wire*`, at 190 to 240 actions each time. Whether those definitions can be split so that fewer crates depend on the ones that churn is a design question for its own investigation.

## What this could not establish

- **The laptop's share of misses.** The hub does not label lookups by client. The 29% outside the `mono` pipeline is the laptop and other pipelines together.
- **Why about a quarter of each queue build's executions could not be traced.** They are mostly `test.xml` spawns whose comparison partner was answered from an output base and so is in no log. They follow tests that were traced.
- **Per-action causes before #3873 merged at 22:31 on 11-22.** Execution logs did not exist, apart from that change's own branch builds from 21:47. The evening's causes rest on job logs and hub metrics, which count but do not attribute.
- **Whether laptop tests share keys with CI.** Only compiles were compared.
- **The nine tests that ran after another job had been served the same key.** Too few to pursue.

## Follow-up code changes

To be filed separately; none is made here. Numbers refer to the table above.

1. Dashboard: rename the hit-rate panels and add executions per hour (1).
2. CI: per-step cache summary from the execution logs (2).
3. Pipeline: one operating system per commit for `bazel-build-test` and `checks` (3).
4. Test sandbox: worker-pool fingerprint for remotely executed tests (4).
5. Test sandbox: `test.xml` written by the wrapper, after a one-target trial (5).
6. CI config: strip at link time only (6).
7. Build: filtered status file for the stamp actions, then the placeholder SHA in verification builds (7, 8).
8. Remote execution: `QueryWriteStatus` patch for compressed uploads, and an upstream issue (9).
9. A separate investigation into the dependents of `boss-protocol`'s wire definitions.

## Method

- **Hub metrics.** Prometheus on `headcrash`, `nativelink_rpc_server_duration_seconds_count` by `rpc_method` and `rpc_grpc_status_code`, as hourly and five-minute `increase()`. Eviction figures are the `nativelink_hub_cas_*` series. Kernel releases are `node_uname_info`.
- **Hub logs.** The journal of `nativelink-macos-hub.service`, system unit and the earlier user unit, read as JSON lines: `get_capabilities` events per client address, and every `WARN` and `ERROR`.
- **Job logs.** `bk job log` for every script job of `mono` builds 28716 to 28726 and 28735 to 28913, keeping Bazel's `processes:` lines and error text.
- **Execution logs.** Every `bazel-execlogs/**/*.bin.zst` artifact of builds 28760 to 28913, decoded against `spawn.proto` at Bazel 9.3.3. Spawns were indexed by label, mnemonic and first output, with digests of their environment, arguments and each class of input, and compared as section 4 describes. A remote execution's own start time orders it; a cache hit is placed at its job's start, because its recorded time is that of the execution that produced the stored result.
- **Source diffs.** `jj diff --name-only` for each merge-queue commit.
- **The laptop comparison.** One `bazel build` of one target in the developer configuration with `--execution_log_compact_file`, and nothing else changed. No cache was disabled or cleared for any measurement.
- **Source.** Bazel `RemoteModule.java`, `CombinedCache.java`, `DiskCacheClient.java`, `ByteStreamUploader.java` and `RemoteOptions.java` at 9.1.0 and 9.3.3; NativeLink `nativelink-service/src/bytestream_server.rs` at `37f3b206` (v1.8.6).
