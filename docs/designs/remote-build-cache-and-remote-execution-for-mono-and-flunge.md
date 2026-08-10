# Remote build cache and RBE for mono and flunge: prefer agent hosts over executor-only Macs

- **Date:** 2026-08-10
- **Status:** design (not implemented); supersedes the earlier attempt in [mono#2715](https://github.com/spinyfin/mono/pull/2715)
- **Repos in scope:** `spinyfin/mono`, `brianduff/flunge`, and future Bazel repos
- **Related:** [Distributed agent execution](../../tools/boss/docs/designs/distributed-agent-execution-register-and-dispatch-to-remote-ssh-hosts.md) · [Linux CI agent runbook](../../.buildkite/linux-agents-runbook.md) · [Xcode pinning runbook](../../tools/boss/docs/mac-toolchain-xcode-pinning.md) · [checkleft CI timing](../investigations/checkleft-checks-ci-timing.md)
- **Measurement artifacts:** all figures below were produced on the coordinator laptop on 2026-08-10; every command is reproduced in [Method](#method-re-runnable)

The platform-aware result this design rests on: **for builds that run on the coordinator's laptop, a shared remote cache and a Linux RBE cluster would each move approximately none of the load, while a compatible Darwin RBE cluster could move most of the measured compile critical path.** RBE is not assumed to mean Linux; the two executor platforms are evaluated separately below.

Linux RBE is 0% for the current configuration because both repos' laptop builds select `darwin_arm64` toolchains and declare no Linux-execution-compatible cross-toolchain. Darwin RBE has a much higher ceiling — up to 120.4 of 125.5 seconds in the measured incremental edit — but still leaves loading and analysis, policy-pinned local tests, the Bazel server, and the agent itself on the laptop. A Mac used as a whole-agent host removes all of those costs, so the recommendation is still to spend scarce Mac capacity on distributed agents rather than executor-only workers.

## Problem

The coordinator laptop is saturated by 20–22 concurrent Bazel servers and worker processes: it is using 9.96 GiB of 11.26 GiB of swap, while Bazel-caused file churn also drives the machine's largest sustained CPU consumer, `fseventsd`. This design decides which intervention best reduces that laptop saturation: local cache and concurrency changes, a shared remote cache, Linux RBE, Darwin RBE, or moving whole workers to other Macs.

## Verdict

**Do not build a shared remote cache or a remote execution cluster for laptop relief now.** Linux RBE cannot execute the current laptop action graph; Darwin RBE can execute much of it, but a Mac assigned to whole-agent distribution removes more load than the same Mac assigned only to remote actions. Fix the local shared cache's retention window, bound local Bazel concurrency, and put the next Mac capacity into distributed agent execution.

A shared remote cache becomes genuinely valuable at exactly one moment: when a second machine starts running Boss workers. Design it now; build it then. That sequencing is a dependency on distributed agent execution, not a hedge.

One measurement points the other way and is not hidden: a cold flunge build on this laptop drew 4 cache hits out of 1,041 needed. That is the reuse property genuinely failing — but failing _locally_, against a 30 GB cap on a volume with 714 GiB free. The study that decides whether the fix is local capacity or a network cache is entry 7 of the breakdown, and it is in scope.

---

## Goals

- Determine, by measurement, what fraction of the coordinator laptop's Bazel load could leave the machine under (a) a shared remote cache, (b) Linux RBE, (c) Darwin RBE.
- Evaluate remote cache and remote execution **separately**, since they have different cost, risk and operational profiles.
- Serve both `mono` and `flunge`, and future Bazel repos, from shared infrastructure if any is built.
- Produce a cost comparison with dated, cited figures and stated workload assumptions, so it can be re-derived when either changes.
- Itemise the hermeticity work each option would require.
- State how this composes with distributed agent execution.
- Keep Bazel as the only sanctioned build path. Nothing here proposes a `cargo` / `xcodebuild` / `swift build` fallback or a weakened build gate.

## Non-goals

- Implementing anything. This document is design only; the accompanying breakdown is what implementation would be filed against.
- Buying hardware, opening vendor accounts, or provisioning cloud resources. Those are decisions for a human, described here and deliberately not emitted as tasks.
- Changing what `flunge` CI does today. Flunge already runs BuildBuddy remote execution for its Linux CI builds and that is correct; see [Alternatives](#alternatives-considered).
- Redesigning the hermetic test wrapper, the sandbox strategy policy, or the Xcode pinning mechanism.
- Reducing agent (LLM CLI) CPU or memory. That is a real share of the machine but out of scope for a Bazel design.
- Multi-tenant or multi-operator infrastructure.

---

## The property that has to hold

It is easy to state this design's goal at the wrong level and then satisfy the letter of it while achieving nothing. The tempting statement is "workers should share a build cache." That is a statement about a _container_. mono and flunge already satisfy it: both `.bazelrc` files set `--disk_cache=~/.cache/bazelcache`, the same absolute path, so **every workspace of both repos on this laptop already shares one cache.** Adding a remote cache would satisfy "workers share a cache" a second time and change nothing.

The load-bearing property is narrower:

> An action executed by one worker must not be executed again by another worker **within the window in which it is still wanted**.

That is a statement about _reuse_, and it has two failure modes: no shared cache at all (not the case here), or a shared cache whose retention window is shorter than the reuse interval (measured below, and it is). It also makes the cross-host case precise: the property holds across two machines only if the cache is reachable from both, which is what a remote cache is actually for.

Two caches being "equivalent" is likewise a claim that needs its dimension named. A remote cache and the local disk cache are equivalent **on correctness of hits** — both are keyed by Bazel's action digest, both return byte-identical outputs. They are _not_ equivalent on reach (one host vs many), on retention (governed by different limits), or on latency (page cache vs network). Every claim below states which dimension it is about.

---

## Method (re-runnable)

**Host, 2026-08-10:** Apple M2 Max, 12 cores, 64 GiB RAM, macOS `Darwin 25.5.0`, 714 GiB free on the data volume. Bazel 9.1.0 in mono (`.bazelversion`), 9.0.0 in flunge.

**Observer effect, stated up front.** Every measurement was taken while the machine was doing its normal work: 20–22 concurrent Bazel servers, ~10 `claude` and ~7 `codex` processes, load average 106–293. Wall-clock figures are therefore _inflated relative to an idle machine_ and should not be compared against numbers taken on a quiet host. The structural figures — action counts, cache-hit counts, configuration mix — are load-independent and are what the conclusions rest on. This is deliberate: the question is about a saturated machine, so the measurements were taken on one.

**What each study was for.** The build scenarios below were designed to _choose_ between five options (local cache tuning, shared remote cache, Linux RBE, Darwin RBE, whole-agent distribution), not to validate a pre-chosen one. Concretely, the discriminating measurement is the pair (how many actions actually execute locally, and in which Bazel configuration): a design that offloads execution can only offload actions that execute, and can only offload them to a platform their toolchain can target. The one study below that is _validating_ rather than choosing is the cache-retention observation — it establishes that a limit is binding, not that lifting it helps. That distinction is carried into the task breakdown.

### Commands

System-wide CPU saturation, sampled every 15 s (`top`'s second frame; the first is a lifetime average and is misleading):

```sh
top -l 2 -s 1 -n 0     # header only: "CPU usage: u% user, s% sys, i% idle" and Load Avg
```

Exact per-process CPU attribution, by differencing cumulative CPU time over a fixed window (this accounts for every process, unlike sampling the top-N):

```sh
ps -Ao pid,time,comm > ps1.txt ; sleep 180 ; ps -Ao pid,time,comm > ps2.txt
# per-pid delta of the TIME column / 180 s = cores consumed by that process
```

Cold-client build — a fresh output base models exactly what a newly-leased cube workspace does, because Bazel derives the output base from the workspace path, so every new workspace starts with an empty local action cache and only the shared disk cache to fall back on:

```sh
bazel --output_base=<fresh dir> build //tools/boss/engine/core:engine_lib \
  --profile=cold.profile.gz
```

Incremental build after an agent-sized edit — note that `touch` is **not** a valid edit for this purpose. Bazel keys on content, so a `touch`-only rebuild completed in 1.7 s and measures nothing. A real one-line change is required:

```sh
printf '\n// scratch\n' >> tools/boss/engine/core/src/lib.rs
bazel build //tools/boss/engine/core:engine_lib --profile=inc.profile.gz
```

Test run:

```sh
bazel test //tools/boss/claude_client:claude_client_test \
           //tools/boss/cli:decision_test \
           //tools/boss/build-info:build-info_test --profile=test.profile.gz
```

Cache state:

```sh
du -sh ~/.cache/bazelcache/{ac,cas}
find ~/.cache/bazelcache/cas -type f -exec stat -f '%m' {} + | sort -n   # age distribution
```

Profiles were parsed for `cat == "action processing"` spans (count and duration per mnemonic) and `cat == "critical path component"`. Actions were classified as exec-configuration when their span name carries rules_rust's `[for tool]` marker, and as target-configuration otherwise.

Flunge was measured from a copy of the shared checkout placed in scratch space, with its own output base and the same shared disk cache, so that no state outside this workspace was modified.

---

## Measured results

### 1. What is actually consuming the laptop

System-wide, over 25 samples spanning ~9 minutes:

| Metric               |      Mean |   Min |   Max |
| -------------------- | --------: | ----: | ----: |
| CPU idle             | **0.01%** | 0.00% | 0.28% |
| CPU user             |     61.6% | 36.8% | 72.8% |
| CPU sys              | **38.5%** | 27.2% | 63.2% |
| Load average (1 min) |     181.6 | 106.9 | 293.2 |

The machine is continuously at 0% idle, and **roughly 38% of all CPU is kernel time** — about 4.6 of 12 cores spent in the kernel rather than in a compiler.

Exact per-process attribution over a 180 s window (1,242 CPU-seconds consumed, 6.90 of 12 cores attributable to identifiable processes):

| Process                        | CPU-seconds | Cores | Share |
| ------------------------------ | ----------: | ----: | ----: |
| `fseventsd`                    |         276 |  1.54 | 22.2% |
| `rustc`                        |         225 |  1.25 | 18.1% |
| `clippy-driver`                |         139 |  0.77 | 11.2% |
| `claude`                       |          94 |  0.52 |  7.6% |
| `Boss`                         |          68 |  0.38 |  5.5% |
| `syspolicyd`                   |          60 |  0.33 |  4.8% |
| `WindowServer`                 |          43 |  0.24 |  3.5% |
| Bazel JVM servers (20, summed) |         113 |  0.63 |  9.1% |
| `XprotectService`              |          26 |  0.15 |  2.1% |

Grouped: Bazel compilers and linkers 30.3%, Bazel servers 9.1%, OS/system 35.5%, LLM CLIs 9.0%, Boss app and engine 6.8%.

Three things follow.

**"Almost entirely bazel" is about two-thirds right, but not in the way it sounds.** Direct Bazel process time (compilers, linkers, servers) is ~39% of consumed CPU. But `fseventsd` at 1.54 sustained cores is the single largest consumer on the machine, and it is doing filesystem-event bookkeeping for the churn Bazel creates — sandbox trees created and destroyed, output trees written, symlink forests built. `syspolicyd` and `XprotectService` add another 0.48 cores of Gatekeeper/malware scanning over freshly written executables. Counting those as Bazel-caused, Bazel is responsible for roughly **68%** of consumed CPU.

**That overhead is a function of local file churn, and a remote cache does not remove it.** A cache _hit_ still materialises output files locally; it avoids the sandbox and the subprocess, which is cheaper than executing, but it is not free. The only configuration that meaningfully reduces local file materialisation is remote execution with `--remote_download_minimal`. It is unavailable to a Linux executor pool for the reason established in §3, but available in principle to a compatible Darwin RBE pool — which is why §7 gives Darwin RBE a large reduction here and Linux RBE 0%.

**20–22 concurrent Bazel servers hold 3.8 GB resident, and the machine is swapping**: 9.96 GB of 11.26 GB of swap in use. Each server independently believes it owns all 12 cores for local-resource accounting.

### 2. The shared local cache is already doing the job a remote cache is supposed to do

`~/.cache/bazelcache` is shared by every mono and flunge workspace on this machine, because both repos hardcode the same absolute path in `.bazelrc`.

| Cache component       |  Size | Entries |
| --------------------- | ----: | ------: |
| Action cache (`ac`)   | 24 MB |   6,005 |
| Content store (`cas`) | 34 GB |  11,565 |

Configured limits in mono's `.bazelrc` are `--experimental_disk_cache_gc_max_size=30G` and `--experimental_disk_cache_gc_max_age=7d`. The store is at 34 GB against a 30 GB cap, so **size is the binding limit and the age limit is inert**. The observed consequence: the _oldest_ entry in the content store was **1.4 hours old**. Median 0.3 h.

The effective retention window is therefore about **90 minutes, not 7 days** — a factor of ~110 shorter than configured intent.

The recorded rationale for the 30 GB cap is a comment in `.bazelrc`: "sized conservatively for macOS agents (disk-constrained)." On this host that premise is false — 714 GiB are free. The comment is a genuine recorded reason that has drifted out of date, not an absent one.

`--jobs=200` is different. It appears in both repos' `.bazelrc` with **no comment and no recorded rationale anywhere in either repo**. The stronger reading is not that a reason existed and was lost; it is that the value was never deliberately chosen against this machine's core count. That is a finding, and it matters: the cold-client profile shows **195 concurrent action-processing spans** at peak, from one server, on a 12-core machine, with 19 other servers doing the same thing.

### 3. Cold client: 99.4% of executable actions are already served locally

`bazel --output_base=<fresh> build //tools/boss/engine/core:engine_lib`

| Metric                                  |   Value |
| --------------------------------------- | ------: |
| Elapsed                                 | 567.4 s |
| Critical path                           | 339.4 s |
| Total actions                           |   1,516 |
| **Disk-cache hits**                     | **702** |
| Bazel-internal (symlinks, manifests)    |     810 |
| **Locally executed (`darwin-sandbox`)** |   **4** |

**702 of the 706 actions that needed a result came from the shared local cache. Four executed.** A remote cache's entire theoretical contribution to this build is to serve some subset of those four — and only if another _macOS_ host had already built the identical action.

Where the time went, from the profile (491.3 s of action-processing spans; spans overlap, so the sum exceeds elapsed):

| Mnemonic                  | Count | Span-seconds | Share |
| ------------------------- | ----: | -----------: | ----: |
| `Rustc`                   |    63 |        355.4 | 72.3% |
| `Symlink`                 |   529 |         79.0 | 16.1% |
| `ExtractCargoTomlEnvVars` |    63 |         18.3 |  3.7% |
| `SymlinkTree`             |    40 |         13.9 |  2.8% |
| `CargoBuildScriptRun`     |    14 |          6.0 |  1.2% |
| everything else           |   105 |         18.7 |  3.8% |

Critical-path head: `Compiling Rust rlib engine_lib (454 files)` at **334.7 s** — 98.6% of the critical path is one action. Approximately 7 minutes of the 567 s elapsed was loading and analysis, before any action ran; that cost is paid fresh by every new workspace and is not addressable by any cache, local or remote.

Split by Bazel configuration:

| Configuration         |     Actions | Span-seconds | Share of span time |
| --------------------- | ----------: | -----------: | -----------------: |
| target `darwin_arm64` | 649 (79.7%) |        456.2 |          **92.9%** |
| exec / host tool      | 165 (20.3%) |         35.1 |               7.1% |

The build materialises exactly two output configurations: `darwin_arm64-fastbuild` and `darwin_arm64-opt-exec`. **There is no Linux configuration anywhere in it.**

### 4. Incremental agent-sized edit: one darwin compile is the whole build

This is the shape Boss workers overwhelmingly produce — edit one crate, rebuild.

| Scenario                                 |     Elapsed | Critical path | Actions |                         Cache hits | Executed |
| ---------------------------------------- | ----------: | ------------: | ------: | ---------------------------------: | -------: |
| Rebuild, no source change (`touch` only) |       1.7 s |         0.5 s |       0 |                                  — |        0 |
| Rebuild, same target, warm output base   |     331.5 s |       327.7 s |   1,516 | 1,482 action-cache + 16 disk-cache |        3 |
| **Rebuild after one real added line**    | **125.5 s** |   **120.4 s** |  **40** |                **34 action-cache** |    **4** |

For the real edit: **120.4 of 125.5 seconds is a single `aarch64-apple-darwin` `Rustc` action**, plus three trivial companions (workspace status, build-info, provenance). The 34 cache hits cost about 5 seconds in total.

The offload arithmetic for this scenario is not an estimate:

- **Shared remote cache: 0 s.** The executed action's inputs include the line the agent just wrote. No peer can have its result. This is not a low hit rate; it is a structurally impossible hit.
- **Linux RBE as currently configured: 0 s.** The selected toolchain requires Darwin execution; neither repo defines a Linux-exec/Darwin-target cross-toolchain. Creating one could make some pure Rust compile actions eligible for Linux workers, but a complete Apple-target build would still require a hermetic Apple SDK and linker that cannot be licensed for Linux hosts.
- **Darwin RBE: up to 120.4 s** — nearly the whole build — if Darwin executors existed with a matching toolchain and SDK, minus input upload and output download for a large crate.
- **Whole-agent distribution: 125.5 s**, plus the agent process, plus that worker's share of the `fseventsd` and Gatekeeper overhead, plus its Bazel server's 190 MB and its analysis cost.

### 5. Test run

`bazel test` over three representative `rust_test` targets, from a warm output base:

| Metric                                 |                            Value |
| -------------------------------------- | -------------------------------: |
| Elapsed                                |                          344.7 s |
| Critical path                          |                          340.6 s |
| Action-cache hits                      |                              192 |
| Disk-cache hits                        |                                5 |
| Executed — `darwin-sandbox` (compiles) |                                7 |
| Executed — `local` (test runners)      |                            **2** |
| Tests executed vs cached               | 1 of 3 (`decision_test`, 13.1 s) |

Two things to read off this. First, the same pattern as every other scenario: 197 results served from cache, nine actions executed, all of them `aarch64-apple-darwin`. Second, the test actions ran under the **`local`** strategy, not `darwin-sandbox` — visible in the progress output as `Testing //tools/boss/cli:decision_test; 13s local`.

That second fact is structural and comes from `.bazelrc`, not from a stopwatch: `test:macos --strategy=TestRunner=local` forces every test action on a macOS host to the local strategy, and `test --run_under=//tools/test-sandbox:hermetic_test_wrapper` wraps user code in a repo-owned Seatbelt profile that is macOS-specific. **Under any remote execution design, test execution stays on the local machine until that policy is deliberately changed** — and it exists for hermeticity reasons documented in `.bazelrc` and in [`docs/investigations/test-action-hermeticity.md`](../investigations/test-action-hermeticity.md). Weakening it to make remoting easier is out of bounds.

### 6. Flunge — the same platform story, the opposite cache story

`bazel --output_base=<fresh> build //backend:backend`, run from a copy of the shared flunge checkout with its own output base and the **same** shared disk cache:

| Metric                                  |     Value |
| --------------------------------------- | --------: |
| Elapsed                                 | 1,019.8 s |
| Critical path                           |   830.1 s |
| Total actions                           |     1,420 |
| **Disk-cache hits**                     |     **4** |
| Bazel-internal                          |       379 |
| **Locally executed (`darwin-sandbox`)** | **1,037** |

**A flunge cold client drew 4 results from the shared cache. A mono cold client drew 702.** Same machine, same cache, same day, ten minutes apart.

Action-processing spans totalled 15,088 s (spans overlap heavily; elapsed was 1,020 s):

| Mnemonic                  | Count | Span-seconds | Share |
| ------------------------- | ----: | -----------: | ----: |
| `Rustc`                   |   537 |      8,659.4 | 57.4% |
| `ExtractCargoTomlEnvVars` |   445 |      5,361.0 | 35.5% |
| `CargoBuildScriptRun`     |    51 |        943.7 |  6.3% |
| everything else           |   293 |        124.0 |  0.8% |

Critical-path head: `Compiling Rust rlib blob (89 files)` at 235.7 s, then third-party crates (`tokio` 60.6 s, `syn` 52.5 s, `proc_macro2` 34.4 s).

Two readings of the 4-vs-702 gap, and they are not cleanly separable from a single measurement:

- **Eviction.** The shared store's retention window is ~90 minutes (§2). Flunge's dependency closure is not in it, because mono workers have been filling the 30 GB with mono's closure. Two repos sharing one undersized cache means the less-recently-active one is effectively uncached.
- **Revision skew.** The copied checkout is at `a3833c02b` (2026-06-16), roughly two months behind flunge's tip, so its first-party actions genuinely have no peer. This does not explain the third-party misses — `tokio`, `syn`, `proc_macro2` and friends are keyed on crate version and flags, not on flunge's first-party revision, and they missed too.

The third-party misses are the informative part, and they point at eviction rather than skew. This is the first direct evidence in this document that the reuse property is failing **today** — and note _where_ it is failing: locally, for want of room, not for want of network reach. That distinction is what the recommendation turns on.

Also worth extracting: 445 `ExtractCargoTomlEnvVars` actions — trivial metadata extractions — consumed 5,361 span-seconds, roughly 12 seconds each. On an unloaded machine these take a fraction of a second. That gap is sandbox setup and process spawn under contention, and it is the `--jobs=200` × 20-servers problem showing up as measurable per-action overhead rather than as a load average.

Configuration mix: `darwin_arm64-fastbuild` and `darwin_arm64-opt-exec`, and nothing else. **Flunge's laptop builds materialise no Linux configuration either.**

Configuration facts, verified by reading the repo:

- Flunge's `.bazelrc` defines a complete `build:remote` configuration pointing at BuildBuddy: `--remote_executor`, `--remote_cache`, `--bes_backend`, and an API key committed directly into the tracked file.
- `.buildkite/scripts/lib.sh` adds `--config=ci --config=remote` **for every non-Darwin CI job**. Flunge therefore runs BuildBuddy remote execution in production today. This directly contradicts the earlier attempt's claim that the configuration was dormant.
- Remote actions execute in `docker://flunge.azurecr.io/rbe:latest`, built from `rbe/Dockerfile` — `debian:bookworm-slim` plus `libssl-dev`, `openssl`, `pkg-config`, `build-essential`, `libsqlite3-dev`, `libzstd-dev`. The system-library problem named as a known constraint is solved there by baking the libraries into the image, with `PKG_CONFIG_PATH` set so `pkg-config` finds them.
- ACR pull credentials are injected at build time via `--remote_exec_header=x-buildbuddy-platform.container-registry-{username,password}`, documented in `docs/references/buildkite-azure-acr-auth.md`.
- Darwin CI jobs (the iOS app) get neither `--config=ci` nor `--config=remote`. Apple work already stays local, by construction.
- Flunge sets the **same** `--disk_cache=~/.cache/bazelcache` and the same `--jobs=200` as mono, so §2 and its consequences apply to flunge workers on this laptop identically.

### 7. The offload table — the answer to the decisive question

| Workload                                                                                                | Where it runs today       |                                 Shared remote cache |                      Linux RBE |                    Darwin RBE | Whole-agent distribution |
| ------------------------------------------------------------------------------------------------------- | ------------------------- | --------------------------------------------------: | -----------------------------: | ----------------------------: | -----------------------: |
| Laptop: incremental agent edit (measured 125.5 s, 4 actions)                                            | laptop                    |                                              **0%** |                         **0%** |                    up to ~96% |                 **100%** |
| Laptop: newly-leased mono workspace, unmodified target (measured 567 s, 4 of 706 actions executed)      | laptop                    |                                             **≲1%** |                         **0%** | ~59% of measured elapsed time |                 **100%** |
| Laptop: newly-leased flunge workspace, cold closure (measured 1,020 s, 1,037 of 1,041 actions executed) | laptop                    | **0% today**, large once a second macOS host exists |                         **0%** |                         large |                 **100%** |
| Laptop: analysis + loading (~7 min on a fresh output base)                                              | laptop                    |                                                  0% |                             0% |                            0% |                     100% |
| Laptop: `fseventsd` / Gatekeeper overhead (1.9 sustained cores)                                         | laptop                    |                                     small reduction |                             0% |               large reduction |                     100% |
| Laptop: test execution                                                                                  | laptop                    |                                                  0% |                             0% |               **0%** (policy) |                     100% |
| Laptop: Swift / Apple app build                                                                         | laptop                    |                                                  0% | **0%** (no hermetic toolchain) |                      possible |                     100% |
| Flunge Linux CI                                                                                         | already on BuildBuddy RBE |                                             already |      **already in production** |                           n/a |                      n/a |
| mono Linux CI (`bazel-any` Linux agents)                                                                | 3 owned Linux hosts       |                                      plausible gain |                 plausible gain |                           n/a |                      n/a |

**The platform split is the headline: Linux RBE moves ~0% of the laptop's Bazel CPU with the toolchains configured today; Darwin RBE could move up to ~96% of the measured incremental build's elapsed time (the offloadable action is the entire measured critical path).** The Linux column is zero because both repos' laptop builds materialise only `darwin_arm64` configurations and provide no Linux-execution-compatible cross-toolchain. The Darwin column is not zero because a Darwin worker can advertise the matching execution platform and SDK. Its limit is scope: analysis, local-policy test actions, the Bazel server, and the agent process do not move with the remote compile.

**Under a shared remote cache, ≲1% today**, because the only other machines that could populate darwin action keys are CI Macs that do not build these targets. The flunge row is the important qualifier: there _is_ a large body of re-executed work on this laptop, but it is work a **sibling worker on the same machine** already did and the local cache evicted. That is a local-capacity failure with a local fix, and reaching for a network cache to solve it would be paying for reach to solve a retention problem.

### Executor platform is a decision, not an assumption

The Remote Execution API is platform-neutral. An action carries execution-platform requirements, and a scheduler routes it to a worker that advertises compatible properties. This design includes Linux RBE because mono already has Linux CI capacity and flunge already uses Linux BuildBuddy executors; it includes Darwin RBE because that is the compatible way to remote the laptop's current action graph. They are different infrastructure choices with different offload ceilings, costs, and failure modes.

A Darwin cluster therefore changes the answer from "cannot execute these actions" to "can execute most measured compile time." It does not change the chosen approach: the same owned Macs are candidates for distributed agent execution, where they remove the compile plus loading, analysis, local tests, Bazel-server memory, filesystem-security overhead, and the agent process. Darwin RBE remains the next action-level option if distributed agents ship and compile load still saturates the laptop.

---

## Constraints, verified

### Apple and Xcode toolchains — stays local, confirmed

All Swift, Objective-C, and codesigning rules in mono live under `//tools/boss/app-macos/...` plus macOS test-sandbox helpers. `.ci.bazelrc` pins both `--xcode_version=26.5.0.17F42` and `--repo_env=DEVELOPER_DIR=...` on the darwin CI queue after two documented incidents. There is no hermetic, licensable path to running these actions on Linux, and there is no proposal here that tries.

The more useful observation is that **Apple is not the interesting constraint for the laptop**, because the laptop's dominant cost is not Swift. In the measured window, `swift-frontend` consumed 10 CPU-seconds against `rustc`'s 225 and `clippy-driver`'s 139. The app build is expensive when it happens, but it is not what saturates the machine. The binding constraint is that **Rust targeting `aarch64-apple-darwin` is just as un-remotable to Linux as Swift is** — and that is 92.9% of measured action time.

### System-library linkage in Rust — a real constraint, already solved where it bites

Mono's `Cargo.lock` contains no `openssl-sys` and no `native-tls`. The `*-sys` crates present are `libsqlite3-sys` (built from bundled `sqlite3.c`, with `LIBSQLITE3_FLAGS` and `OPT_LEVEL` pinned via a `crate.annotation` in `MODULE.bazel`), `core-foundation-sys` and `security-framework-sys` (macOS frameworks, used by `//tools/boss/keychain`), plus target-gated `windows-sys`, `linux-raw-sys`, `js-sys`, `web-sys`, `jni-sys`, `dirs-sys`. TLS is `rustls` + `ring`, which builds through the `cc` crate rather than probing a system OpenSSL. First-party `build.rs` files exist in `tools/checkleft`, `tools/boss/{bossctl,build-provenance,cli,engine/core}` and wire environment and version data rather than probing the host.

So the named pain point is real in general but largely absent from mono's graph — and where it _is_ present, flunge has already solved it the standard way, by baking `libssl-dev` and `pkg-config` into the RBE image. This is worth stating plainly because it removes system-library hermeticity as an argument either for or against the recommendation here: it is not what is blocking laptop offload.

### Toolchain hermeticity under RBE — containers on Linux, managed images on Darwin

mono's history of Xcode / `apple_support` pin mismatches requiring `bazel clean --expunge` is exactly the failure class that container-defined Linux toolchains eliminate: the toolchain is part of the worker image, and a host that drifts cannot half-build against the wrong SDK. That argument is sound for Linux RBE, and flunge's production setup is the proof.

Darwin RBE cannot use that portable-container model, but it can still route actions only to workers provisioned with the pinned Xcode and SDK versions. That makes executor bootstrap, image rollout, and drift detection part of the service rather than eliminating them. Linux RBE would harden mono's Linux CI toolchain but cannot serve the current laptop build; Darwin RBE could serve it, at the higher operational burden evaluated in Alternative C.

### Additional constraints found

- **`--jobs=200` with no recorded rationale.** Peak measured concurrency was 195 action-processing spans from a single server on 12 cores, with ~20 servers resident. Bazel's local-resource accounting is per-server, so 20 servers each independently schedule against the full core count. This is the most likely direct cause of the load averages above 200, and it is a local scheduling problem that no remote infrastructure addresses.
- **Retention window ~90 minutes** (§2). The shared cache's configured 7-day age limit is inert behind a binding 30 GB size cap on a volume with 714 GiB free.
- **Per-workspace analysis cost.** Bazel derives the output base from the workspace path, so every newly-leased cube workspace re-runs loading and analysis from scratch — measured at roughly 7 minutes for one Rust target under load. No cache, local or remote, removes this. It scales with the number of distinct workspaces, which is a Boss design parameter.
- **`--stamp` is on for every build** via `--workspace_status_command`. The `.bazelrc` comment asserts only the version plist and `build_info_rs` consume `ctx.info_file`; if that ever stopped being true, heavy compile actions would acquire a volatile input and cache hit rates would collapse. Nothing currently enforces the assertion.
- **`BOSS_SHAKE_*` reaches action keys through `--define`.** The `.bazelrc` comment is explicit that `--define` _is_ part of the cache key while `--action_env` is not forwarded by rules_rust. Dev builds use empty defines; a release build with real values produces different keys for anything consuming them. On a shared remote cache this would need either a separate instance or a documented understanding that release and dev keys diverge.
- **A live BuildBuddy API key is committed to flunge's tracked `.bazelrc`.** The repo is private, which limits but does not remove the exposure; every worker session, CI log, and workspace copy carries it. It should move to a secret and be rotated.
- **Test execution is pinned local on macOS** by `test:macos --strategy=TestRunner=local` plus the repo-owned hermetic wrapper.
- **Large prebuilt inputs** — the Ghostty `xcframework` and the pinned `wasm-tools` archives — are cheap on a local disk cache and expensive on a network cache. They shift the cost of a remote cache from CPU to transfer.
- **Cross-platform cache keys do not collide, and that is the problem, not the safety.** Linux CI writing to a shared cache cannot serve a darwin worker, by design. A shared cache between mono's Linux CI agents and the laptop is correct and useless in the same breath.

---

## Alternatives considered

### A. Shared remote cache for both repos, now (the earlier attempt's recommendation)

**Case for it:** low operational burden, one binary, obvious fit for "many workers building close revisions of the same repos."

**Why not, and this is checkable:** the argument for it assumes workers are not already sharing. They are. Both repos point every workspace at `~/.cache/bazelcache`, and the cold-client measurement shows a brand-new workspace drawing **702 of 706 needed results from that shared cache**. The remaining four are darwin-unique compiles that no peer possesses. The incremental-edit measurement shows the same thing more sharply: 34 hits worth ~5 seconds, and 120 seconds in an action whose input the agent just authored.

**The strongest counter-evidence, stated rather than buried:** the flunge cold client on the same machine got 4 hits out of 1,041. Sharing is clearly not working there. But look at _who could have supplied_ those misses — sibling flunge workers on this same laptop, whose results the 30 GB store evicted inside 90 minutes. Reaching for a network cache to fix that is buying reach to solve a retention problem, on a machine with 714 GiB of unused disk. If entry 7's study shows a larger local cap does not recover it, this rejection does not survive and the remote cache should be built; that condition is written into the entry.

This rejection is otherwise scoped, not general. It says a remote cache cannot help **this laptop while it is the only machine running workers**. It does not say remote caches are bad, and it does not disqualify the one already in production: flunge's CI jobs use BuildBuddy's cache across ephemeral CI agents that share nothing locally, which is precisely the case where the reuse property fails without a network cache. The same reasoning that rejects it here endorses it there, which is how a rejection should behave.

### B. Linux remote execution for mono, mirroring flunge

**Case for it:** it works. Flunge runs it in production, the image is small, the system-library problem is solved, and container-defined toolchains would harden mono's Linux CI against the drift class that has already cost hours.

**Why not for the stated problem:** it addresses CI, not the laptop. mono's Linux CI already runs on three owned hosts (`empiricist`, `zoologist`, `diziet`) with local SSD disk caches. The laptop's builds materialise no Linux configuration at all — the cold-client build produced exactly `darwin_arm64-fastbuild` and `darwin_arm64-opt-exec` and nothing else. Adding Linux executors would move 0% of the measured laptop load.

It is worth checking whether the requirement used to reject this is real rather than an artifact. It is: the requirement is "reduce laptop saturation," as defined in the [Problem](#problem) section, and Linux RBE fails it on a measurement rather than on a judgement. If the goal were instead "reduce mono's Linux CI wall time," Linux RBE would be a reasonable candidate and this rejection would not apply. That is a different project.

### C. Darwin RBE on Macs the fleet already owns

**Case for it:** this is the only action-level design that can move `aarch64-apple-darwin` compiles off the laptop, and it does not require buying anything. The fleet already contains macOS Buildkite agents (`anaplian`, `skaffen`) and a second macOS host (`zakalwe`) named in the distributed-agent design. NativeLink and EngFlow both support Darwin RBE workers; BuildBuddy prices Apple Silicon cores on its published matrix. Measured ceiling: ~96% of an incremental agent build.

**Why not now:**

1. It competes for exactly the hardware that distributed agent execution needs, and loses on value per machine. A Mac serving as an RBE executor absorbs _compile_ actions. The same Mac running a Boss worker absorbs the compile, the analysis, the Bazel server's memory, the agent process, and that worker's share of the `fseventsd` and Gatekeeper overhead — measured together as the largest single consumer on the machine. Distributed agents dominate on every axis except granularity.
2. Test execution would still stay local under current policy, so the ceiling is lower than the compile share suggests.
3. Operating a Darwin RBE executor pool is the highest-burden option surveyed: no container isolation, Xcode pinned per host by hand, and the same drift class that already caused two incidents — now on machines nobody is sitting in front of.

Kept as a deferred entry, with an explicit trigger: if distributed agents ship and the laptop is _still_ saturated by Bazel compile, this is the next lever, not before.

### D. Do nothing at all

**Case for it:** the load is tolerable and the machine is doing useful work.

**Why not:** two measured defects are cheap to fix and are actively costing reuse and stability — a 90-minute cache retention window on a volume with 714 GiB free, and an unexamined `--jobs=200` on a 12-core host running twenty Bazel servers. Declining to build remote infrastructure is not the same as declining to fix the local configuration, and conflating them would leave free wins on the floor.

---

## Chosen approach

### Now: fix what is measurably broken locally

1. **Raise the local disk-cache size cap in both repos** so the age limit governs instead of the size limit. The recorded justification for 30 GB ("disk-constrained") is contradicted by 714 GiB free on this host. This is the only change available that increases cross-worker reuse, it costs nothing, and its failure mode is bounded disk use. The flunge cold-client measurement (§6) is the evidence that this is not merely tidying: a second repo sharing the same 30 GB store got 4 hits out of 1,041, including on third-party crates whose keys do not depend on that repo's revision at all. What it does _not_ establish is how much of that a larger cap recovers — the size of the recovery is what entry 7 measures.
2. **Instrument every worker Bazel invocation** — record the process-summary line Bazel already prints (`N processes: X action cache hit, Y disk cache hit, Z internal, W local/sandbox`) plus elapsed time, target, and repo. This is the missing instrument. Every claim in this document about hit rates comes from hand-run builds; a week of real worker data is what should decide whether a remote cache is ever worth building.
3. **Choose a defensible local concurrency bound.** `--jobs=200` has no recorded rationale in either repo, and per-server local-resource accounting means twenty servers each schedule against all 12 cores. Determine an appropriate `--jobs` and/or `--local_resources` from measurement and record the reasoning in the file, so the next reader is not in this position.
4. **Move flunge's committed BuildBuddy key to a secret and rotate it.**

### Not now, and under what conditions

**A shared remote cache is designed here and deliberately not built.** The design, when it is built, is unremarkable and should not be over-thought: `buchgr/bazel-remote` (cache-only, single Go binary, HTTP and gRPC REAPI, disk-backed with LRU eviction, `.htpasswd` or mTLS) on an existing owned Linux host, one logical instance shared by both repos, reached over the existing private network, enabled behind an opt-in `--config=remote-cache` in each repo rather than defaulted on. Bazel treats an unreachable cache as a soft failure, so the failure mode is "builds get colder," which is the correct failure mode for a cache.

The condition that should trigger building it is specific: **a second machine starts running Boss workers.** At that moment the reuse property stops holding — two machines, two local caches, no shared reach — and a network cache becomes the mechanism that restores it. This is a dependency on distributed agent execution, and it is why the remote-cache entries below sit behind it rather than beside it.

A second, independent trigger: if the instrumentation in (2) shows that a material share of worker builds are re-executing actions that _another worker on this same machine already executed and the cache evicted_, and raising the size cap in (1) does not fix it. That would mean the local cache cannot hold the working set at all, and a larger backing store becomes worth its network cost.

**Linux RBE for mono is not proposed.** If mono's Linux CI wall time becomes the pain, that is a different project with a different justification, and flunge's setup is the template.

**Darwin RBE is not proposed.** See Alternative C.

### Hermeticity work, itemised

This is the work that would be required _if_ a remote cache were built, and it is listed here so the deferred entries below are not mistaken for small ones. None of it is required by the "now" list.

1. **Freeze and document the cache-key surface.** Enumerate every flag that participates in the action key and must match across writers: `--compilation_mode`, the `BOSS_SHAKE_*` `--define`s, checkleft's clippy aspect flags, `--repo_env=DEVELOPER_DIR`, `--xcode_version`, and the `--run_under` alignment already documented in `.bazelrc`. Laptop builds and darwin CI builds currently differ on at least `--xcode_version` and `DEVELOPER_DIR`; whether that shards the cache for Rust actions or only for Apple actions is unverified and is a prerequisite, not a detail.
2. **Assert the stamp boundary.** Add a check or test that fails if a heavy compile action acquires a dependency on `ctx.info_file`. Today this is an unverified comment.
3. **Decide the release-vs-dev key policy** for `BOSS_SHAKE_*`: separate instance names, or an accepted key divergence.
4. **Audit for host-path and network leakage** in genrules and build scripts — the `cc`-crate paths, the `apple_genrule` libtool step, anything reading `$HOME` or an absolute tool path.
5. **Cross-host correctness soak:** build a fixed target set on two hosts against the shared cache and compare digests and test results before trusting hits.
6. **Confirm the offline failure mode** end to end, including a deliberately unreachable cache.

---

## Cost and hosting

All figures retrieved **2026-08-10** unless stated. Prices change; every figure below is dated and sourced so it can be re-checked rather than trusted.

### Workload assumptions, derived from measurement rather than guessed

| Parameter                                              | Value                                       | Source                    |
| ------------------------------------------------------ | ------------------------------------------- | ------------------------- |
| Laptop compiler/linker CPU                             | 2.09 sustained cores ≈ **50 CPU-hours/day** | measured, §1              |
| Laptop Bazel-server CPU                                | 0.63 cores ≈ 15 CPU-hours/day               | measured, §1              |
| Actions actually executed per incremental worker build | **4**                                       | measured, §4              |
| Share of those executable on a Linux worker            | **0**                                       | measured, §3 config split |
| Concurrent Bazel servers                               | 20–22                                       | measured, §1              |
| Cache working set (current, evicting)                  | 34 GB                                       | measured, §2              |

The critical assumption to re-derive if anything changes: **50 CPU-hours/day is the entire prize**, and it is available only to a design that can execute `aarch64-apple-darwin` actions. Every Linux-only option is competing for a prize of zero.

### Managed vendors

| Vendor                                                                                             | Tier       | Published price                                                                      | Metered on                                                 | Darwin RBE executors                                                               |
| -------------------------------------------------------------------------------------------------- | ---------- | ------------------------------------------------------------------------------------ | ---------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| **BuildBuddy** ([pricing](https://www.buildbuddy.io/pricing/))                                     | Personal   | free                                                                                 | ≤10 users, **100 GB** cache transfer, ≤80 Linux RBE cores  | no                                                                                 |
|                                                                                                    | Team       | "**$X / GB** of cache transfer over 100 GB" — _list price not published on the page_ | cache transfer; ≤800 Linux cores                           | **$45 / core** on the feature matrix; the page does not state the billing period   |
|                                                                                                    | Enterprise | quote                                                                                | unlimited cores, SSO/SAML, isolated infra                  | yes                                                                                |
| **EngFlow** ([pricing](https://www.engflow.com/product/pricing))                                   | Free       | free                                                                                 | single machine, ≤32 cores, Linux only, includes cache + UI | no                                                                                 |
|                                                                                                    | Enterprise | quote — no public $/CPU-hour                                                         | —                                                          | yes (Linux/macOS/Windows)                                                          |
| **NativeLink** ([site](https://nativelink.com/), [enterprise](https://enterprise.nativelink.com/)) | Self-host  | free for individual cache use (FSL 1.1 → Apache 2.0 future licence)                  | —                                                          | server runs on Linux and macOS; workers target any platform the toolchains support |
|                                                                                                    | Enterprise | **$180k/yr** (private, <$100M valuation); **$360k/yr** (public or ≥$100M)            | —                                                          | yes                                                                                |

Two honest gaps: BuildBuddy does not publish a numeric $/GB for Team cache transfer, and does not state whether "$45 / core" is monthly. Neither is guessed at here. NativeLink's FSL restricts _shared production use_ to licensed deployments; whether a solo operator's own multi-machine fleet counts is a licensing question for a human, not a technical one.

Flunge's current BuildBuddy usage sits inside the Personal tier's shape (one user, Linux cores). Whether it is inside the 100 GB/month cache-transfer limit is not observable from the repo and should be read off the BuildBuddy console before any decision that increases usage.

### Bare-metal rental and cloud

| Option                         | Spec                                                                             | Price                                                                                                                                                                                                                                             | Notes                                                       |
| ------------------------------ | -------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------- |
| **Hetzner AX42-1**             | Ryzen 7 PRO 8700GE 8c, 64 GB DDR5 ECC, 2×512 GB NVMe, 1 Gbit/s unlimited traffic | **€97.30/mo + €49 setup** (Falkenstein/Helsinki), effective 15 Jun 2026 ([Hetzner price adjustment](https://docs.hetzner.com/general/infrastructure-and-availability/price-adjustment/))                                                          | more than adequate as a cache host; egress not billed       |
| **Hetzner AX41-1-LTD**         | prior-gen Ryzen, 64 GB                                                           | **€57.30/mo**, no setup                                                                                                                                                                                                                           | cheapest credible cache host                                |
| **AWS EC2 `mac2-m2pro.metal`** | M2 Pro, 12 vCPU, 32 GiB                                                          | **$1.56/hr** us-east-1 ([Vantage, updated 2026-08-10](https://instances.vantage.sh/aws/ec2/mac2-m2pro.metal)); Dedicated Host with a **24-hour minimum allocation** per Apple's macOS SLA ([AWS](https://aws.amazon.com/ec2/instance-types/mac/)) | ≥$37.44 per allocation; **~$1,138/mo** if held continuously |
| **MacStadium M4.S**            | M4 10-core, 16 GB, 256 GB                                                        | **$149/mo** ([pricing](https://www.macstadium.com/pricing))                                                                                                                                                                                       |                                                             |
| **MacStadium M4.L**            | M4 Pro 12-core, 48 GB, 1 TB                                                      | **$349/mo**                                                                                                                                                                                                                                       |                                                             |
| **MacStadium S2.M**            | M2 Ultra 24-core, 64 GB, 2 TB                                                    | **$369/mo**                                                                                                                                                                                                                                       | best cores-per-dollar of the Mac rentals                    |

For a cache-heavy workload, egress usually dominates compute on public cloud. It does not arise here for a self-hosted option: Hetzner bills no traffic on the standard uplink, and the strongest option bills nothing at all, because —

### On-prem: the hardware is already bought

This is the finding that reframes the cost question. The fleet already contains, per [`.buildkite/linux-agents-runbook.md`](../../.buildkite/linux-agents-runbook.md) (host facts verified against live hosts on 2026-07-27):

- **Linux:** `empiricist` (2 agent registrations), `zoologist`, `diziet` — all Ubuntu 26.04, all on the `bazel-any` queue; plus `sma-ci-1`, `sma-ci-2` on `bazel-any-test` and `sma-release-1` on `linux-release`.
- **macOS:** `anaplian` and `skaffen`, two agent registrations each, on `bazel-any`; a `macos-arm64` queue for the app build and release; and `zakalwe`, named in the distributed-agent design as the first remote worker host.

**The marginal cost of a shared remote cache on this fleet is one container on a host that is already powered, patched and networked.** `bazel-remote` is a single Go binary with a disk-backed LRU store. Against that baseline, every rental option above is a worse deal, and the break-even question mostly dissolves.

For completeness, the break-evens that remain:

| Comparison                                                                                                                                                         | Break-even                                                                                                                                                                                                                           |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `bazel-remote` on an owned Linux host vs Hetzner AX41-1-LTD                                                                                                        | Immediate — €57.30/mo buys nothing not already owned, unless the requirement is off-site durability.                                                                                                                                 |
| New Mac mini M4 (**$799**, 16 GB / 512 GB, [price raised May 2026](https://www.macrumors.com/2026/05/01/mac-mini-now-starts-at-799/)) vs MacStadium M4.S ($149/mo) | **5.4 months.** Buying wins for any horizon past half a year.                                                                                                                                                                        |
| New Mac mini M4 ($799) vs BuildBuddy Apple Silicon at $45/core                                                                                                     | If $45/core is monthly, a single purchased 10-core mini pays for itself against **two** reserved cores in under 9 months. Buying wins decisively — but the mini is better spent as a distributed-agent host than as an RBE executor. |
| Owned Linux host vs AWS EC2 Mac                                                                                                                                    | Not comparable; EC2 Mac at ~$1,138/mo continuous is disqualified for this workload on price alone.                                                                                                                                   |

### Software stack, if self-hosting

| Stack                        | Role                                                                                             | Ops burden for one person                                   | Verdict                                               |
| ---------------------------- | ------------------------------------------------------------------------------------------------ | ----------------------------------------------------------- | ----------------------------------------------------- |
| **`bazel-remote`**           | cache only (HTTP + gRPC REAPI, AC + CAS, disk/S3/GCS/Azure backends, `.htpasswd`/mTLS/LDAP auth) | one binary, one config file, LRU eviction handled           | **the only sane default** for a cache-only deployment |
| **BuildBuddy (self-hosted)** | cache + optional RE + web UI                                                                     | more moving parts; the UI is genuinely useful               | reasonable if the invocation UI is wanted             |
| **NativeLink**               | cache + RE, Linux/macOS servers                                                                  | newer; licence question above                               | the option to revisit _if_ Darwin RBE is ever pursued |
| **Buildbarn**                | full RE platform                                                                                 | scheduler, workers, storage, browser as separate components | disqualified — needs a platform team                  |
| **EngFlow self-hosted**      | full platform, commercial                                                                        | quote-driven                                                | disproportionate                                      |

### Operational honesty

If the cache host dies, Bazel falls back to local execution and builds get colder — nothing breaks. That is why cache-only is a safe thing to own alone. If an _execution_ cluster dies without local fallback configured, the laptop regains full load **and** CI goes red simultaneously, which is worse than today. Any future execution work must carry `--remote_local_fallback` and a tested outage drill before it is trusted.

---

## Interaction with distributed agent execution

The [distributed agent execution design](../../tools/boss/docs/designs/distributed-agent-execution-register-and-dispatch-to-remote-ssh-hosts.md) dispatches whole Boss workers to SSH-reachable hosts. The judgement that it is the right primary solution is supported by these measurements, not merely compatible with them.

| Question                                        | Answer                                                                                                                                                                                                                                                                                                                                                                                                |
| ----------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Does RBE reduce its value?                      | **No — for this workload, RBE is the dominated option.** A Darwin RBE executor absorbs compile actions only. A remote worker host absorbs compile, analysis, the Bazel server's memory, the agent process, and its share of the `fseventsd` and Gatekeeper overhead that §1 measured as the machine's largest single consumer. Linux RBE reduces its value by exactly zero, because it moves nothing. |
| Does a remote cache reduce its value?           | No. They are complementary, and the dependency runs one way: distributing agents is what _creates_ the need for a remote cache.                                                                                                                                                                                                                                                                       |
| Would a remote agent host use RBE or the cache? | It would use the **cache**, and this is the whole argument. Once workers run on two machines, the property in [§The property that has to hold](#the-property-that-has-to-hold) fails without a network cache — each host would re-execute what the other already built.                                                                                                                               |
| Does that change the sizing?                    | Yes, and in the direction that makes a cache worth building: cache value scales with the number of Bazel hosts sharing it, which is currently one.                                                                                                                                                                                                                                                    |
| Could the same hardware serve both?             | Yes for the **cache** — it is one container on an already-owned Linux host and should simply live there. No for **agent hosts vs Darwin RBE executors**: the same Mac cannot usefully be both, and per the table above the agent-host role is worth more.                                                                                                                                             |
| Which delivers relief sooner?                   | **Distributed agents, by a wide margin.** Every measured path that removes load from the laptop goes through moving the whole worker. The cache work is days of repo change but delivers ~0 until a second host exists.                                                                                                                                                                               |
| Correct order?                                  | Land the local fixes now (they are hours of work and independent of everything). Ship distributed agents. Build the shared cache as its dependent, before the second host carries real load. Revisit Darwin RBE only if the laptop is still saturated afterwards.                                                                                                                                     |

One accounting note, since this project sits downstream of a milestone-accounting failure recorded in [investigation 0007](../../tools/boss/docs/investigations/0007-p545-distributed-execution-milestone-gap.md): the acceptance gesture for the cache work is not "the config landed." It is "a build on host B hits an action host A executed, demonstrated end-to-end against the real service." A gate written at the end of the cache phase can only block what comes after it; it has no authority over the phase it belongs to, so the demonstration belongs _inside_ the entry that enables the cache, not after it.

---

## Risks / open questions

1. **The measurements are from one repo's Rust engine target, on a loaded machine.** The structural findings (four executed actions, no Linux configuration, 92.9% darwin span time) are robust to load. The wall-clock figures are not, and should not be quoted as performance baselines.
2. **The strongest case against this recommendation is eviction, and it is partly borne out.** The two cold-client measurements disagree sharply — mono 702 hits of 706, flunge 4 of 1,041 — and the flunge result shows the reuse property failing today. This document reads that as a local-capacity failure with a local fix, because the misses include third-party crates whose action keys do not depend on flunge's first-party revision, and because 714 GiB of disk sit unused behind a 30 GB cap. A reader could reasonably read it the other way: as proof that the working set of two repos does not fit locally at all, in which case a large remote store is the right answer and the second-host trigger is beside the point. **Entry 7 is the study that decides between those two readings, and it is decision-critical rather than optional.** If it returns "a larger local cap does not absorb it," the recommendation flips to building the remote cache immediately.

   The flunge measurement carries a confound that entry 7 must control for: the copied checkout was ~2 months stale, so its first-party misses are explained by revision skew rather than eviction. Only the third-party misses discriminate, and a purpose-built study should compare a _current_ revision of each repo, back to back, against a warm and then a capped cache.

3. **`--jobs=200` may be load-bearing for reasons nobody wrote down.** The measurement says it is oversubscription; the absence of a rationale says nobody decided. Both can be true, and the concurrency entry should measure before changing rather than assume.
4. **Whether laptop and darwin-CI builds produce matching action keys is unverified.** `--xcode_version` and `DEVELOPER_DIR` differ between them. If Rust action keys are unaffected, darwin CI could seed a shared cache for laptop workers, which would materially change the cache's value. If they are affected, it could not. This is answerable in an afternoon and nobody has answered it.
5. **BuildBuddy's Team cache-transfer rate and the billing period for Apple Silicon cores are not published.** Any decision that depends on them needs a quote first.
6. **NativeLink's FSL "shared production use" boundary** is a licensing question for a human if Darwin RBE is ever pursued.
7. **The committed flunge API key** should be rotated regardless of anything else in this document.

---

## Proposed implementation task breakdown

Breakdown size: 10 entries (6 in-scope, 4 deferred) — the recommendation builds no remote infrastructure, so the in-scope work is the two `.bazelrc` seams this design found broken (one per repo, necessarily separate PRs), the instrumentation that is missing before any remote decision can be evidence-based, the concurrency decision that instrumentation feeds, the eviction study that decides whether a remote cache is ever built, and the credential remediation the flunge remote-execution audit surfaced; the four deferred entries are the remote-cache client config in each repo, its key-parity prerequisite, and the Darwin-RBE revisit, recorded rather than dropped because the design explicitly decided against them now and named the conditions that would reverse that.

### Depth 0 — may run in parallel

**1. Raise the local disk-cache size cap in mono**

Scope: In mono's `.bazelrc`, raise `--experimental_disk_cache_gc_max_size` from `30G` to a value proportional to available disk so that the existing `--experimental_disk_cache_gc_max_age=7d` becomes the governing limit rather than an inert one, and replace the "disk-constrained" comment with the measured retention figure and the free-space observation that contradicts it. Verify after the change that the store grows past 30 GB and that the oldest content-store entry ages past the previously observed ~1.4 hours. Do not touch the `build:ci` overrides, which already set 3T.

Effort hint: `small`

Dependencies: none

Scope: in-scope

**2. Raise the local disk-cache size cap in flunge**

Scope: The same change in `brianduff/flunge`'s `.bazelrc`, which shares the identical `~/.cache/bazelcache` path and the identical 30 GB-class problem. Separate repo, therefore necessarily a separate PR. Record the same rationale so the two files do not drift apart silently.

Effort hint: `trivial`

Dependencies: none

Scope: in-scope

**3. Record Bazel invocation outcomes for every worker build**

Scope: Add a lightweight recorder that captures, per Bazel invocation in a worker session, the process-summary counts Bazel already emits (`action cache hit` / `disk cache hit` / `internal` / locally executed), elapsed time, the target pattern, and the repo, appending them to a durable log outside the workspace. This is the instrument every remote-infrastructure decision in this document is currently missing; today the only hit-rate figures anywhere are from hand-run builds. Implementation only — analysis of what it collects is a separate entry.

Effort hint: `medium`

Dependencies: none

Scope: in-scope

**4. Rotate flunge's committed BuildBuddy API key into a secret**

Scope: Remove the API key literal from flunge's tracked `.bazelrc` `build:remote` stanza, source it from the CI secret mechanism already used for the ACR registry credentials, rotate the exposed key, and document the new wiring alongside the existing Azure ACR auth runbook. Surfaced by this design's audit of the only production remote-execution configuration in the fleet.

Effort hint: `small`

Dependencies: none

Scope: in-scope

**5. Verify whether laptop and darwin-CI builds share action keys**

Scope: Determine empirically whether the `--xcode_version` and `--repo_env=DEVELOPER_DIR` differences between local builds and the `ci-darwin` configuration change the action keys of Rust compile actions, or only of Apple-toolchain actions. Compare action digests for a fixed target set under both flag sets and record the result in `docs/investigations/`. The answer decides whether darwin CI could ever seed a shared cache for laptop workers, which is the largest single unknown in the remote-cache value case. Read-only investigation; touches no build configuration.

Effort hint: `small`

Dependencies: none

Scope: deferred (future / not a v1 blocker) — only load-bearing if a shared remote cache is built, but cheap enough to run whenever the question next comes up

_Depth-0 parallelism: entries 1, 2, 3, 4 and 5 are independent. Entries 1 and 2 are in different repos. Entries 3 and 5 touch no `.bazelrc`. Entry 4 touches flunge's `.bazelrc` and therefore overlaps entry 2 in the same file — run entry 2 first and have entry 4 forward-port it preservingly, integrating rather than replacing the cache-cap change._

### Depth 1

**6. Determine and set a defensible local Bazel concurrency bound in mono**

Scope: Using the invocation data from entry 3 plus direct measurement of concurrent action counts against the 12-core host and ~20 resident Bazel servers, determine an appropriate `--jobs` and/or `--local_resources` setting for laptop worker builds, apply it to mono's `.bazelrc`, and record the reasoning in the file so the value is no longer undocumented. Measure before and after; if the measurement does not support a change, land the recorded rationale for the existing value instead. Same file as entry 1, so it must forward-port that change.

Effort hint: `medium`

Dependencies: Record Bazel invocation outcomes for every worker build; Raise the local disk-cache size cap in mono

Scope: in-scope

### Depth 2

**7. Cache eviction and reuse study across a representative week**

Scope: Analyse a week of the data from entry 3 to answer the question this design could not: what share of locally executed actions were actions another worker on the same machine had already executed and the cache had evicted, and how much of that the larger cap from entries 1 and 2 absorbed. Control for the confound in the flunge cold-client measurement by comparing _current_ revisions of both repos back to back rather than a stale checkout. This is a _choosing_ study, not a validating one — its permitted outcomes include "the local cache is now sufficient, build nothing remote," "raise the cap further," and "the working set does not fit locally, build the remote cache now," and it must be able to return any of them. Write the finding into `docs/investigations/` with the raw counts.

Effort hint: `medium`

Dependencies: Record Bazel invocation outcomes for every worker build; Raise the local disk-cache size cap in mono; Raise the local disk-cache size cap in flunge

Scope: in-scope

### Deferred, gated on the outcome above

**8. Shared remote cache client configuration for mono**

Scope: Add an opt-in `--config=remote-cache` to mono's `.bazelrc` pointing at a `bazel-remote` instance, covering endpoint, auth header sourcing from a secret, timeout, and upload policy, with no executor flags and no default-on behaviour; plus a runbook covering enablement, the offline failure mode, and the cache-key surface enumerated in the hermeticity list. This entry's acceptance gesture is a demonstrated cross-host hit — a build on one host consuming an action another host executed, against the real service — not merely that the configuration parses; that demonstration belongs inside this entry, not in a later gate.

Effort hint: `medium`

Dependencies: Cache eviction and reuse study across a representative week; Verify whether laptop and darwin-CI builds share action keys

Scope: deferred (future / not a v1 blocker) — gated on a second machine running Boss workers, or on the eviction study returning "the working set does not fit locally"

**9. Shared remote cache client configuration for flunge**

Scope: The mirror of entry 8 in `brianduff/flunge`, reusing the same instance and the same key-surface documentation, and keeping flunge's existing `build:remote` executor configuration untouched and separate from the new cache-only config. Separate repo, therefore a separate PR; file it once the mono side has demonstrated a cross-host hit rather than in parallel, so a broken assumption is found once instead of twice.

Effort hint: `small`

Dependencies: Shared remote cache client configuration for mono

Scope: deferred (future / not a v1 blocker) — same gate as entry 8

**10. Darwin RBE evaluation against distributed agent hosts**

Scope: If the laptop remains Bazel-saturated after distributed agent execution has shipped and the local fixes have landed, evaluate Darwin RBE executors on already-owned Macs against simply adding another agent host: measure the compile share actually offloadable given that test execution stays local by policy, price the Xcode-pinning and host-drift operational burden against the two incidents already on record, and record a recommendation. Investigation and write-up only; no infrastructure.

Effort hint: `medium`

Dependencies: Cache eviction and reuse study across a representative week

Scope: deferred (future / not a v1 blocker) — the design's measured position is that a Mac is worth more as an agent host than as an executor; this entry exists only to revisit that if the premise changes

### Operator decisions — described here, deliberately not filed as tasks

- Whether to run `bazel-remote` on an existing owned Linux host, and which one.
- Whether to buy any additional Mac, and whether it serves as an agent host or a Darwin RBE executor.
- Whether to obtain a BuildBuddy quote for the unpublished Team cache-transfer rate and Apple Silicon core billing period.
- Whether NativeLink's FSL "shared production use" boundary permits a single-operator multi-machine deployment.

### Parallelism summary

```text
Depth 0:  [1 mono cap] [2 flunge cap] [3 instrumentation] [4 flunge key] [5 key-parity check]
          # 2 before 4 (both edit flunge .bazelrc); all others fully parallel
Depth 1:  [6 concurrency bound]          # after 3 and 1; forward-ports 1's edit to mono .bazelrc
Depth 2:  [7 eviction study]             # after 1, 2, 3 — the decision point
Deferred: [8 mono remote cache config]   # gated on 7 and 5
              -> [9 flunge mirror]       # after 8 proves a cross-host hit
          [10 Darwin RBE evaluation]     # gated on 7
```
