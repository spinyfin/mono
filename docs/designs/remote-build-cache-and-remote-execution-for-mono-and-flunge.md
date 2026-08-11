# Remote build cache and remote execution for mono and flunge: scale Bazel off the coordinator

- **Date:** 2026-08-10
- **Status:** design (not implemented); supersedes the earlier attempt in [mono#2715](https://github.com/spinyfin/mono/pull/2715)
- **Repos in scope:** `spinyfin/mono`, `brianduff/flunge`, and future Bazel repos
- **Related:** [Distributed agent execution](../../tools/boss/docs/designs/distributed-agent-execution-register-and-dispatch-to-remote-ssh-hosts.md) · [Linux CI agent runbook](../../.buildkite/linux-agents-runbook.md) · [Xcode pinning runbook](../../tools/boss/docs/mac-toolchain-xcode-pinning.md) · [checkleft CI timing](../investigations/checkleft-checks-ci-timing.md)

## Executive answer

**Bazel work is the primary limit on Boss worker scale, and the next capacity must move that work off the coordinator.** At 16 implementation workers plus reviewers, the 12-core laptop was continuously at 0% idle, used 9.96 GiB of 11.26 GiB of swap, and reached load averages near 200. Running 40–50 workers on that fixed CPU budget is not achievable by enlarging a cache or retuning `--jobs`. Those changes may reduce waste, but they cannot add compute.

Proceed with a shared remote cache and remote-execution pilot now. Use **NativeLink as the first implementation to test** because one service can provide the CAS, action cache, scheduler, and Linux/macOS worker support needed by the full design. Run normal Rust compilation and tests on Linux whenever the worker only needs a build or test result; reserve Darwin execution for `Boss.app`, Apple-specific flunge work, and any action whose target or toolchain genuinely requires macOS. An owned or newly purchased Mac mini is a viable Darwin worker and is materially cheaper than continuously renting the cited high-end Mac.

The current laptop action graph happens to select Darwin because it was invoked on a Darwin client. That measurement describes today's invocation; it does **not** establish that worker builds need Darwin artifacts. Most Boss and flunge worker validation consumes exit status, logs, test results, and cache entries rather than launching the resulting binary on the coordinator. Linux-first remote development and Darwin RBE are therefore both first-class candidates:

- A remote Linux development/execution host can run Bazel against a synchronized or remotely mounted checkout and return results to the local agent.
- The Boss engine can eventually run on Linux with terminal presentation separated through the tmux transport; that converges with whole-agent distribution.
- A macOS RBE pool can execute the existing Darwin action graph without changing target platform.

Distributed agents and RBE are complementary. Whole-agent distribution moves more than Bazel, while RBE lets many local or remote agents share a much larger execution pool. Once more than one host runs agents or executors, the shared remote cache is required to avoid splitting reusable results across host-local caches.

## Problem

Boss makes one operator's workflow look like a moderate engineering team, but currently concentrates that team's Bazel servers and compilers on one laptop. Each leased workspace receives its own Bazel output base and server. During the measurement window, 20–22 servers were resident and one profiled server exposed 195 overlapping action-processing spans while other servers were also active.

The goal is not to make the laptop polite under load. The goal is to **max out useful work and finish as many independent worker builds as possible**. A local cap that divides 12 cores proportionally among 20 servers still leaves the same 12 cores saturated and does not create the several-times-greater throughput needed for 40–50 agents. The design must expand the execution pool beyond the laptop.

## Verdict

Adopt this sequence:

1. **Add durable Bazel invocation telemetry immediately.** Persist a structured summary and BEP for every Boss-triggered Bazel command outside ephemeral workspaces.
2. **Raise the undersized local disk-cache caps.** The old “disk-constrained” premise is stale: both macOS CI agents have large SSDs, CI already configures 3 TiB/60-day caches, and the coordinator had 714 GiB free when measured. The earlier local space emergency was also caused by a cube disk-usage bug rather than the intended steady state. This is a useful waste-reduction change, not the scaling strategy.
3. **Pilot NativeLink on owned Linux capacity** as the shared CAS/action cache and scheduler.
4. **Make non-Apple worker validation Linux-first.** Prove representative mono and flunge compile/test targets on Linux workers, then route them remotely by default where no local Darwin executable is consumed.
5. **Pilot a Darwin NativeLink worker** on an existing Mac or a purchased Mac mini for `Boss.app` and remaining Darwin actions.
6. **Compose this with distributed agents.** Remote agents use the same cache and may either execute locally on their host or submit actions to the same RBE pool.

Keep local fallback during rollout, but do not use local concurrency throttling as the primary remedy. `--jobs` should be high enough to keep the remote pool busy. A local-only bound is justified only for a measured reliability or interactive-latency need, not as a claim that it increases aggregate build throughput.

---

## Goals

- Scale from roughly 16 implementation workers plus reviewers toward 40–50 concurrent agents without adding that load to the coordinator.
- Evaluate remote cache, Linux RBE, Darwin RBE, remote-development hosts, and whole-agent distribution as compatible parts of one system.
- Preserve Bazel as the only sanctioned build path.
- Keep mono, flunge, and future Bazel repos on one shared cache/execution plane where action keys match.
- Make platform selection follow the artifact actually consumed, not the OS on which the agent happens to run.
- Preserve or improve hermeticity while prioritizing scale.
- Produce durable measurements that can quantify hit rates, execution placement, queueing, transfer, and end-to-end latency.

## Non-goals

- Implementing or purchasing the system in this document.
- Requiring a local executable from every worker build when the worker only needs validation results.
- Treating cache enlargement or `--jobs` tuning as sufficient for the requested scale.
- Multi-tenant service for unrelated operators.

---

## Evidence and validity

### Coordinator measurements

**Host, 2026-08-10:** Apple M2 Max, 12 cores, 64 GiB RAM, macOS `Darwin 25.5.0`, 714 GiB free on the data volume. Bazel 9.1.0 in mono.

Measurements were taken under the real saturated workload: 20–22 Bazel servers, approximately 10 `claude` and 7 `codex` processes, and load average 106–293. Wall-clock timings are not idle-host benchmarks. Action counts, selected configurations, cache classifications, and critical-path composition are the useful structural evidence.

The system-wide sample showed:

| Metric               |      Mean |   Min |   Max |
| -------------------- | --------: | ----: | ----: |
| CPU idle             | **0.01%** | 0.00% | 0.28% |
| CPU user             |     61.6% | 36.8% | 72.8% |
| CPU sys              |     38.5% | 27.2% | 63.2% |
| Load average (1 min) |     181.6 | 106.9 | 293.2 |

Per-process CPU deltas over 180 seconds included:

| Process                        | CPU-seconds | Cores |
| ------------------------------ | ----------: | ----: |
| `fseventsd`                    |         276 |  1.54 |
| `rustc`                        |         225 |  1.25 |
| `clippy-driver`                |         139 |  0.77 |
| Bazel JVM servers (20, summed) |         113 |  0.63 |
| `claude`                       |          94 |  0.52 |
| Boss                           |          68 |  0.38 |
| `syspolicyd`                   |          60 |  0.33 |
| `XprotectService`              |          26 |  0.15 |

Direct compilers, linkers, and Bazel servers accounted for roughly 39% of the attributed process CPU. Adding `fseventsd` and executable-scanning processes yields a roughly 68% Bazel-associated estimate, but that estimate is **not** a causal attribution: this study did not trace file events back to Bazel, and normal system activity also uses those services. The reliable conclusion is stronger and simpler:

- `rustc` and `clippy-driver` were continuously visible doing the avoidable work.
- Nearly every non-system process outside Bazel was either a necessary part of the normal machine or Boss/agent orchestration.
- The host was already fully saturated, so each additional local compile competes for a fixed 12-core budget.

The earlier wording that made filesystem churn sound like the primary bottleneck was unsupported. File materialization may contribute to kernel load, and `--remote_download_minimal` can reduce it, but compilation is the directly observed, addressable scaling cost.

### Re-runnable mono commands

System saturation and process attribution:

```sh
top -l 2 -s 1 -n 0
ps -Ao pid,time,comm > ps1.txt
# wait for the fixed sampling window
ps -Ao pid,time,comm > ps2.txt
```

Cold client, using a fresh output base:

```sh
bazel --output_base=<fresh-dir> build //tools/boss/engine/core:engine_lib \
  --profile=cold.profile.gz
```

Incremental build after a real content edit:

```sh
bazel build //tools/boss/engine/core:engine_lib --profile=inc.profile.gz
```

Representative tests:

```sh
bazel test //tools/boss/claude_client:claude_client_test \
  //tools/boss/cli:decision_test \
  //tools/boss/build-info:build-info_test --profile=test.profile.gz
```

### Invalid flunge experiment removed

The prior draft measured a copied flunge checkout at `a3833c02b`, roughly two months behind its tip. That was not an acceptable basis for conclusions about current cache reuse or current RBE configuration. Its hit-rate and timing results are discarded here.

The replacement audit read flunge's default branch at `00fc195b92d73162ae72ae417343fdba54385875` on 2026-08-10:

- `.bazelrc` contains `--jobs=200` and the shared local disk cache, but no remote executor, remote cache, BES backend, or API key.
- `.ci.bazelrc` configures local CI disk/repository caches, including a 3 TiB/60-day GC policy and a dedicated `/Volumes/ssd` path on Darwin.
- `.buildkite/scripts/lib.sh` selects `--config=ci-linux` or `--config=ci-darwin`; it no longer selects a remote configuration.
- [flunge#1313](https://github.com/brianduff/flunge/pull/1313) removed the unused BuildBuddy/RBE configuration on 2026-08-07.

The old claims that flunge currently executes through BuildBuddy and exposes an API key were obsolete and are removed. A valid flunge performance study must lease or otherwise use a current workspace and record its exact revision.

### The repos do share dependencies

The current `Cargo.lock` files have **126 exact crate-name/version pairs in common**. So the two repositories do have a meaningful overlapping dependency closure. That does not guarantee 126 reusable Bazel actions: toolchain versions, target platform, features, compile flags, build-script environment, and repository mapping all participate in action keys. Durable action-digest telemetry and a current-head back-to-back build are needed to measure how much of the source-level overlap becomes cache reuse.

---

## Measured mono build shapes

### Shared local cache

Every mono workspace on the coordinator points at `~/.cache/bazelcache`.

| Cache component       |  Size | Entries |
| --------------------- | ----: | ------: |
| Action cache (`ac`)   | 24 MB |   6,005 |
| Content store (`cas`) | 34 GB |  11,565 |

The configured 30 GB size limit bound before the 7-day age limit; the oldest observed CAS entry was about 1.4 hours old. That is a real retention defect. It is also actionable because the coordinator had 714 GiB free, the earlier local disk emergency came from a cube bug, and both repos' CI configs already assume persistent large SSDs and 3 TiB cache limits.

Increasing the cap should improve reuse, including reuse across the two repositories. It cannot remove the unique compile created by each source edit, cannot add CPU, and cannot make 40–50 agents fit on 12 cores. It belongs in the rollout as housekeeping and a control measurement.

### Cold client and incremental edit

Cold build of `//tools/boss/engine/core:engine_lib`:

| Metric                              |   Value |
| ----------------------------------- | ------: |
| Elapsed                             | 567.4 s |
| Critical path                       | 339.4 s |
| Total actions                       |   1,516 |
| Disk-cache hits                     |     702 |
| Bazel-internal                      |     810 |
| Locally executed (`darwin-sandbox`) |       4 |

A one-line content edit produced:

| Metric            |   Value |
| ----------------- | ------: |
| Elapsed           | 125.5 s |
| Critical path     | 120.4 s |
| Actions           |      40 |
| Action-cache hits |      34 |
| Locally executed  |       4 |

The critical path was one `aarch64-apple-darwin` Rust compile. A remote cache cannot contain the result of a line that was just authored, but RBE can execute it. A compatible Darwin executor could move nearly the whole measured critical path. A Linux executor can move it if the worker is validating a Linux artifact, or if a supported cross-toolchain makes the action Linux-executable while retaining its required target.

The observed output configurations were `darwin_arm64-fastbuild` and `darwin_arm64-opt-exec`. That is expected from a build invoked on a Darwin host with host-default configuration. It proves only that an unchanged invocation requires compatible Darwin execution; it says nothing surprising about whether a deliberately Linux-targeted worker validation can run on Linux.

### Test execution

Three representative `rust_test` targets produced 197 cache hits and nine executed actions. macOS test actions currently use `test:macos --strategy=TestRunner=local` and the repository-owned hermetic wrapper.

That policy is a current configuration, not an immutable boundary. Scale is the primary goal, and nothing is categorically out of bounds. The Linux sandbox policy already demonstrates a path to hermetic remote testing: port the wrapper's guarantees to the remote execution platform, validate network/filesystem isolation, and then allow eligible test actions to execute remotely. RBE's declared inputs and isolated action roots improve hermeticity by construction, although worker images and undeclared host dependencies still require audit.

### What `--jobs=200` means

The peak of 195 action-processing spans shows that one Bazel server was willing to expose a very large amount of parallel work. It does **not** show that lowering `--jobs` increases total throughput. With roughly 20 servers competing for 12 local cores, proportional per-server limits still sum to a saturated 12-core host. Lowering concurrency may improve responsiveness or reduce pathological spawn overhead, but it cannot provide the requested multiple of capacity.

For RBE, high available parallelism is valuable: it allows the scheduler to fill a worker pool much larger than the client. The design therefore preserves “max out useful execution” as the default and moves the work to a scalable pool.

---

## Target architecture

### One NativeLink cache and scheduler

Start with one NativeLink deployment on an owned Linux host:

- one CAS and action cache shared by mono, flunge, local workers, remote-development hosts, distributed agents, Linux executors, and Darwin executors;
- authenticated gRPC access over the private network;
- bounded storage with observable eviction and per-repo/instance metrics;
- separate execution platforms advertised by workers rather than separate products;
- local fallback during rollout and a tested outage path.

NativeLink is the most interesting first pilot because this project needs more than cache-only storage. `bazel-remote` remains a good cache-only fallback, but choosing it first would leave a separate scheduler/worker system to integrate immediately afterward. The pilot must resolve NativeLink's license boundary for a single-operator, multi-machine production fleet before rollout.

### Linux-first build and test execution

Most worker invocations do not consume a local Darwin binary. For those invocations:

1. Sync the workspace delta to a Linux development host, or expose the authoritative checkout through a filesystem/proxy mechanism.
2. Invoke Bazel with an explicit Linux target/execution platform.
3. Execute compile and test actions on Linux NativeLink workers.
4. Return stdout/stderr, test results, BEP, and requested outputs; do not download the full output tree unless the agent needs it.

Two integration shapes should be prototyped:

- **Development proxy host.** A wrapper around Bazel/cube sends commands to a Linux checkout. Workspace visibility can be implemented with an SSH-based sync, NFS, or a VFS-like mount; the prototype should choose using correctness and latency measurements rather than assumption.
- **Remote Boss engine.** Run the engine and workers on Linux. The current Ghostty-pane coupling blocks this, but the tmux transport separates process placement from terminal presentation. This is compatible with, and may become, the distributed-agent design.

Linux-first execution avoids producing Darwin artifacts that nobody launches. It also gives container-defined toolchains and test isolation. It does not cover `Boss.app` or Apple-specific deliverables.

### Darwin remote execution

Darwin RBE is viable and should be piloted, not deferred by assumption. A worker on an owned or purchased Mac mini advertises the pinned Xcode/SDK execution platform and handles:

- `Boss.app` Swift/Objective-C/codesigning actions;
- Rust actions that genuinely require the Darwin target;
- macOS tests after the hermetic test policy is adapted for remote execution.

Darwin workers cannot rely on Linux containers, so provisioning must pin Xcode, SDK, command-line tools, and repository environment and must fail closed on drift. That is operational work, not a reason to reject the architecture.

### Durable Bazel telemetry

Yes—Bazel can and should log the decision data durably. Every Boss-triggered invocation should:

- write a uniquely named `--build_event_binary_file` or `--build_event_json_file` to a Boss-owned directory that survives workspace recycling;
- append a small structured record containing execution id, repo, revision, workspace, target pattern, platform, start/end time, exit status, elapsed time, and Bazel's process-summary counts;
- record cache hits/misses, local/remote action counts, remote queue/execution time, bytes uploaded/downloaded, and critical-path actions from BEP/profile data;
- use bounded retention and stable invocation identifiers so data can be joined without scraping terminal transcripts;
- optionally publish the same BEP to a backend once the local format is proven.

Acceptance is a query over at least a week of real worker invocations, not merely the existence of log files. The query must distinguish unique source-edit compiles from re-execution of identical action digests and must report per-platform and per-host outcomes.

---

## Hermeticity and platform work

Scale is the priority, but hermeticity and RBE reinforce each other when the action boundary is real. Required work:

1. Define Linux and Darwin execution platforms and toolchain constraints explicitly.
2. Build pinned Linux worker images for Rust/C/C++ tools and any flunge system libraries.
3. Provision Darwin images with pinned Xcode/SDK versions and drift detection.
4. Audit genrules and build scripts for host paths, `$HOME` reads, ambient tools, network access, and undeclared inputs.
5. Port the test wrapper's guarantees to remote Linux and Darwin strategies instead of assuming `local` is the only hermetic strategy.
6. Freeze the cache-key surface: compilation mode, toolchain versions, `DEVELOPER_DIR`, `--xcode_version`, `--run_under`, feature flags, repository mapping, and release-only defines.
7. Keep release/dev key divergence explicit and test cross-host correctness by comparing outputs and test results.
8. Use minimal output download for validation-only builds; identify the few workflows that genuinely require a local binary or bundle.

The current mono graph has little system OpenSSL exposure, but that is not a general exemption. Flunge's current head no longer contains its former RBE image, so a new current-head audit must determine its Linux worker-image packages rather than copying a removed Dockerfile.

---

## Alternatives considered

### A. Local cache enlargement and concurrency tuning only

**Keep the cache change; reject this as the scaling plan.** The 30 GB limit is stale and unnecessarily evicts useful results, including results from overlapping dependencies. But a cache cannot serve a result for a newly edited compile, and 12 local cores remain 12 local cores regardless of how `--jobs` is divided among servers. This path cannot support 40–50 concurrent agents.

### B. Shared remote cache without execution

**Useful but insufficient alone.** It becomes necessary as soon as agents or executors span hosts, and RBE itself requires shared CAS/action-cache plumbing. It reduces duplicate work and makes distributed agents compose cleanly. It does not execute the unique compile on each edit, so it ships as part of the RBE foundation rather than as the entire answer.

### C. Linux RBE while agents stay local

**Chosen for validation-only workloads.** The earlier rejection confused the client host with the artifact requirement. Explicit Linux platform selection can move most Rust compile/test validation even when the initiating agent runs on macOS. Any actions that still require Darwin route to Darwin workers or local fallback.

### D. Remote Linux development or a remote Boss engine

**Chosen for prototyping.** A development proxy host moves Bazel servers, loading/analysis, test processes, and output materialization off the laptop, not just individual actions. A remote Boss engine moves the agent process too once tmux decouples terminal presentation. These approaches are compatible with NativeLink and may be the fastest route to Google-like client concurrency.

### E. Darwin RBE

**Chosen for a bounded pilot.** It directly serves the current Darwin graph and can move almost the entire measured incremental compile. Buying Mac minis is explicitly allowed and often cheaper than hosted capacity. The design no longer assumes scarce existing Macs must be used only as whole-agent hosts.

### F. Whole-agent distribution

**Chosen as a complementary path, not as a reason to reject RBE.** It removes analysis, Bazel-server memory, the agent process, and local output materialization from the coordinator. Those hosts should still use the shared cache and may submit actions to RBE. The scheduler then balances compute independently of where an agent session lives.

---

## Cost and hosting

All cited prices were retrieved 2026-08-10 and must be rechecked before purchase.

### Prefer owned capacity

The fleet already has Linux and macOS Buildkite hosts. The cheapest pilot is therefore NativeLink plus a Linux worker on owned capacity, followed by a Darwin worker on an existing Mac where availability permits.

For additional Darwin capacity:

| Option                     |       Published price | Design reading                        |
| -------------------------- | --------------------: | ------------------------------------- |
| MacStadium M4.S            |               $149/mo | reasonable short experiment           |
| MacStadium M4.L            |               $349/mo | purchase wins quickly                 |
| MacStadium S2.M            |               $369/mo | **reject for steady state**           |
| New Mac mini M4            |                  $799 | about **2.2 months** of a $369 rental |
| AWS EC2 `mac2-m2pro.metal` | ~$1,138/mo continuous | reject for this workload              |

Sources: [MacStadium pricing](https://www.macstadium.com/pricing), [AWS EC2 Mac minimum allocation](https://aws.amazon.com/ec2/instance-types/mac/), and the dated [$799 Mac mini price report](https://www.macrumors.com/2026/05/01/mac-mini-now-starts-at-799/).

The $369/month option is not an attractive steady-state choice: its cost buys another Mac mini roughly every two months. Use rental only for a short benchmark when purchase lead time would block the pilot.

### Software choice

| Stack                         | Fit                                                                                        |
| ----------------------------- | ------------------------------------------------------------------------------------------ |
| **NativeLink**                | **First pilot:** shared cache + scheduler + Linux/macOS workers in one system              |
| `bazel-remote`                | Excellent cache-only fallback, but does not answer the execution requirement               |
| BuildBuddy hosted/self-hosted | Useful UI and managed option; get a quote before comparing unpublished transfer/core terms |
| EngFlow                       | Technically capable; commercial terms require a quote                                      |
| Buildbarn                     | Capable but too many separately operated components for the first pilot                    |

NativeLink's FSL “shared production use” boundary remains an operator/legal question. Resolve it before production; it does not prevent a technical proof of concept.

### Failure behavior

Remote cache failure should degrade to colder builds. Remote execution failure should use a tested local/alternate-worker fallback during rollout. Once capacity is trusted, the policy can prefer failing fast over silently melting the coordinator; that is an operator choice informed by telemetry.

---

## Interaction with distributed agent execution

| Question                                            | Answer                                                                                                                                                                                                 |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Does RBE reduce the value of distributed agents?    | No. RBE scales action execution; remote agents also move analysis, server memory, materialization, and the agent process.                                                                              |
| Does distributed execution reduce the value of RBE? | No. More clients create more parallel actions for the shared scheduler and increase the value of centralized execution capacity.                                                                       |
| Does a remote cache become required?                | **Yes.** Distributing agents or actions across hosts splits local caches; the shared CAS/action cache restores cross-host reuse.                                                                       |
| Can the same host be both agent and executor?       | Yes when measured capacity allows. Roles need not be permanently exclusive; NativeLink schedules available worker slots.                                                                               |
| What should happen first?                           | Telemetry and local-cap cleanup can run in parallel with a NativeLink proof of concept; then Linux-first validation, remote-development integration, and Darwin workers proceed from measured results. |

The remote cache acceptance test is end to end: host B must consume an action result produced by host A against the real service. The RBE acceptance test is stronger: an agent on the coordinator authors a real edit, a remote worker executes the resulting unique compile/test action, and the coordinator remains responsive while multiple invocations run concurrently.

---

## Risks and open questions

1. **The mono timings are from one target on one saturated host.** They establish the shape of the bottleneck, not production RBE throughput.
2. **Flunge lacks a valid current-head performance measurement.** The stale-copy numbers are intentionally removed; the first task must replace them using a leased/current workspace.
3. **Linux-first validation needs workflow classification.** Identify commands that truly consume Darwin outputs instead of assuming either “all” or “none.”
4. **Remote-development filesystem semantics may dominate latency.** Benchmark SSH sync, NFS, and any VFS/proxy option on real edit/build cycles.
5. **Darwin worker images are harder to make repeatable.** Pinning and drift detection are mandatory, but Mac RBE remains viable.
6. **Cache overlap is not the same as dependency overlap.** The lockfiles share 126 exact crate versions; action-digest parity must be measured.
7. **NativeLink licensing must be resolved for production.**
8. **High client parallelism can overload the service.** Size scheduler, CAS bandwidth, and workers from queue-time/transfer telemetry; do not respond by defaulting back to a fixed local CPU budget.

---

## Proposed implementation task breakdown

### Depth 0 — run in parallel

**1. Persist Bazel invocation telemetry**

Add the structured summary and durable BEP described above to the Boss Bazel invocation path. Prove records survive workspace recycling and can be queried by repo, revision, platform, worker, cache result, and local/remote execution.

Effort hint: `medium`

Dependencies: none

**2. Raise mono's local disk-cache cap**

Replace the stale disk-constrained rationale and raise the laptop cap enough that age, not 30 GB size, governs retention. Preserve the existing CI-specific 3 TiB/60-day settings and verify the local retention window grows.

Effort hint: `small`

Dependencies: none

**3. Raise flunge's local disk-cache cap**

Apply the equivalent current-head change in flunge. Its current CI settings already use large persistent SSD caches; keep platform-specific paths intact.

Effort hint: `small`

Dependencies: none

**4. Re-measure flunge at current head**

Lease or otherwise use a current flunge workspace, record the exact revision, and run cold/warm and representative edit/test builds against the shared cache. Report action digests shared with mono, executed actions, platform, and critical path. Do not use a copied stale checkout.

Effort hint: `medium`

Dependencies: Persist Bazel invocation telemetry

**5. Prove NativeLink locally on owned Linux capacity**

Resolve trial licensing, deploy a single-node development instance with bounded storage and authentication, connect one Linux worker, and demonstrate remote execution plus a cross-host cache hit. Record deployment and outage procedures.

Effort hint: `large`

Dependencies: none

### Depth 1

**6. Define hermetic Linux worker platforms for mono and flunge**

Create minimal pinned worker images/toolchains for representative non-Apple Rust compile and test targets. Audit current flunge system-library requirements rather than resurrecting its removed RBE image.

Effort hint: `large`

Dependencies: Prove NativeLink locally on owned Linux capacity; Re-measure flunge at current head

**7. Add opt-in shared remote cache/execution configs**

Add authenticated, opt-in Bazel configs in both repos for the shared service, with platform selection, minimal downloads, timeouts, and local fallback. Acceptance requires a current real edit executed remotely and a demonstrated cross-host cache hit.

Effort hint: `large`

Dependencies: Define hermetic Linux worker platforms for mono and flunge

**8. Prototype the Linux development proxy**

Compare SSH synchronization, NFS, and a VFS/proxy approach for a local agent invoking Bazel on a remote Linux checkout. Measure edit propagation, analysis time, correctness, output download, and failure recovery.

Effort hint: `large`

Dependencies: Add opt-in shared remote cache/execution configs

**9. Make remote test execution preserve the hermetic policy**

Port and verify network, filesystem, tool-path, temp-directory, and credential isolation on Linux RBE. Then remove `local` strategy requirements only for tests whose remote platform satisfies the policy.

Effort hint: `large`

Dependencies: Define hermetic Linux worker platforms for mono and flunge

**10. Pilot a Darwin NativeLink worker**

Provision an existing or purchased Mac mini with pinned Xcode/SDK state, advertise a Darwin execution platform, and run the measured engine compile plus `Boss.app` actions remotely. Compare owned-hardware throughput and operations against a short rental benchmark only if needed.

Effort hint: `large`

Dependencies: Prove NativeLink locally on owned Linux capacity

### Depth 2

**11. Integrate remote Boss execution with tmux**

Use the tmux transport to run the engine/worker on Linux while presenting the session locally. Compare this with the development-proxy path and whole-agent SSH distribution; converge on one operator workflow rather than maintaining redundant control planes.

Effort hint: `large`

Dependencies: Prototype the Linux development proxy

**12. Roll out and size for 40–50 agents**

Use at least a week of durable telemetry to size Linux workers, Darwin workers, CAS storage, and network. Demonstrate concurrent real edits with remote queueing bounded, the coordinator responsive, and remote fallback behavior tested.

Effort hint: `large`

Dependencies: Add opt-in shared remote cache/execution configs; Make remote test execution preserve the hermetic policy; Pilot a Darwin NativeLink worker

### Parallelism summary

```text
Depth 0: [telemetry] [mono cap] [flunge cap] [NativeLink proof]
                    -> [current-head flunge measurement]
Depth 1:             -> [Linux platforms] -> [repo configs] -> [development proxy]
                                         \-> [remote test policy]
                      [NativeLink proof]  -> [Darwin worker]
Depth 2:                                   [remote Boss/tmux]
                                          [40–50-agent rollout]
```
