# NativeLink remote execution: findings and a small macOS worker pool

- **Date:** 2026-09-17
- **Status:** empirical companion and proposed next deployment; documentation only, no configuration enabled
- **Provenance:** operator-supplied record of interactive coordinator spikes; original transcripts, profiles, and host logs were unavailable to this investigation
- **Verification:** GitHub API and repository inspection on 2026-09-17, at mono `23d8281e`; no new remote build or host measurement
- **Related designs:** [mono#2715](https://github.com/spinyfin/mono/pull/2715) and [mono#2718](https://github.com/spinyfin/mono/pull/2718), both open and unmerged when checked

Many concurrent builders are exhausting one 12-core laptop and preventing finished changes from reaching their validation gate. This document preserves the working Darwin remote-execution mechanism, the failed approaches, and the smallest useful deployment that can start moving compilation elsewhere.

## Verdict

**Retune the existing 16 GB worker to about four slots immediately, then pilot two macOS workers on a wired private network.** The clonefile fix has demonstrated real remote Rust compilation. Two 16 GB machines offer roughly 7.2 RAM-safe simultaneous actions: useful relief, but far short of a 20-build peak, and not yet relief from locally forced test execution.

Choose macOS first for the shortest path from the proven spike to useful capacity. Keep Linux-first validation as the cheaper scaling direction after toolchain and workflow work. This proposal deploys nothing; the infrastructure owner must provision the service and opt clients in, and the NativeLink production-licensing question remains open.

## Why this is urgent

The following are historical operator measurements, not fresh samples collected for this document:

| Measurement date                 | Laptop evidence                                                                                                                                                                          | Operational meaning                                                                                    |
| -------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| 2026-09-17                       | Load 296.66 / 261.52 / 196.05 on 12 CPUs; 5,112 free 16 KB pages (about 80 MiB); eight workers `working`; 13 live Bazel servers; 83 concurrent `engine_lib_test` processes at one sample | Roughly 25 times the CPU count in one-minute load, with load still climbing                            |
| 2026-09-16                       | Load 189.65 / 157.93 / 113.61; 2,659 Bazel processes; swap 3.19 of 4 GB; 67 MB free RAM; nine workers running full suites simultaneously                                                 | This was a repeated resource crunch                                                                    |
| Earlier design, dated 2026-08-10 | 16 implementation workers plus reviewers; effectively 0% CPU idle; 9.96 GiB of 11.26 GiB swap; mean one-minute load 181.6, peak near 293                                                 | The same pattern predates this spike; see unmerged [#2718](https://github.com/spinyfin/mono/pull/2718) |

Under this load, workers that have finished their changes cannot get clean gate runs and stall instead of publishing. Test processes orphan and remain behind. A fixed 12-core budget cannot host 8–16 concurrent full builders merely by tuning each builder.

Two operator decisions are settled:

- **Remote cache alone is insufficient.** Disk cache is already enabled (`.bazelrc` still confirms this). Cache misses are doing the damaging local work; only remote execution moves those misses off the laptop. A shared cache is supporting infrastructure for the fleet, not the remedy by itself.
- **“Your laptop is fast enough” is not an answer.** The requirement is many simultaneous builds, not one acceptable local build.

Load average is not a count of independent compilation slots, and free RAM alone is not a full memory-pressure metric. Neither converts directly to an offload percentage. The combined observations establish the capacity problem without pretending to measure its exact action mix.

## Evidence boundaries and verified references

The record supplied on 2026-09-17 is the primary source for the spike and later fleet investigation. Their exact run dates were not supplied; **2026-09-17 is the date this record was captured, not an invented benchmark date**. Except for the explicitly dated laptop readings above, all timings, counts, OOM reports, network figures, and hardware estimates below are attributed to those historical sessions and were not re-measured. The original target/revision/command for the 418-process run was also not retained in the brief.

| Reference                                                                                   | Verified title or identity                                                                              | State on 2026-09-17                                                                                                                 |
| ------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| [NativeLink #2727](https://github.com/TraceMachina/nativelink/issues/2727)                  | Released aarch64-apple-darwin binary does not start: links against an absolute /nix/store libiconv path | Closed, 2026-09-05                                                                                                                  |
| [NativeLink #1608](https://github.com/TraceMachina/nativelink/issues/1608)                  | Implement a workaround for DYLD_LIBRARY_PATH in lre-rs                                                  | Open                                                                                                                                |
| [rules_rust #2331](https://github.com/bazelbuild/rules_rust/issues/2331)                    | Using MacOS client/worker RBE fails to find rustc lib                                                   | Closed, 2023-12-19                                                                                                                  |
| [Fork branch](https://github.com/brianduff/nativelink/tree/fix/macos-clonefile-executables) | `fix/macos-clonefile-executables`                                                                       | Head `3125a4fd9c0363d6264ce5ff9d5bd1b2d45eb3a7`; comparison against fork tag `v1.6.6` reports exactly one commit ahead, zero behind |
| [mono #2715](https://github.com/spinyfin/mono/pull/2715)                                    | Design: shared Bazel remote cache first for mono and flunge                                             | Open, `mergedAt: null`                                                                                                              |
| [mono #2718](https://github.com/spinyfin/mono/pull/2718)                                    | Design: scale Bazel off the coordinator with remote cache and RBE                                       | Open, `mergedAt: null`; explicitly supersedes #2715 and requires it to be closed unmerged                                           |

Both design diffs were read. Their architecture and software comparison remain there: #2718 chooses NativeLink for the first pilot, retains `bazel-remote` as a cache-only fallback, requires commercial quotes for BuildBuddy and EngFlow, and rejects Buildbarn's component count for this first deployment. This document adds empirical results and incremental sizing rather than another vendor comparison. Neither design has landed; closing #2715 is still an operator action.

The [Flunge Buildkite reference](../../tools/boss/docs/designs/flunge-buildkite-pipeline-reference.md), under “Where mono should deliberately diverge,” item 5 (line 192 at the inspected revision), says:

> The mono design proposes disk-cache-only v1, remote-cache-only v2 (no remote _execution_) — that's actually converging with, not diverging from, flunge's current state.

Adopting this proposal supersedes that no-remote-execution decision. The reference file is intentionally unchanged.

## The Darwin spike: four blockers in order

The worker was **zakalwe**, Apple silicon, 10 cores, 16 GB RAM, macOS 26.3 / Xcode 26.3. NativeLink v1.6.6 ran as an all-in-one CAS, action cache, scheduler, and worker, configured at `~/nativelink/rbe.json5`, with ports 50051 and 50061. The client was mono in a leased cube workspace. The objective was specifically Rust compilation on a Darwin remote worker.

### 1. The prebuilt binary did not start

The Nix-built `aarch64-apple-darwin` release carried an absolute `/nix/store/.../libiconv.2.dylib` dependency in its Mach-O load commands. Normal macOS installations do not have that exact Nix store object. The spike rewrote the install name and ad-hoc signed the modified binary; upstream [#2727](https://github.com/TraceMachina/nativelink/issues/2727) independently records the failure and workaround.

For that artifact, inspect `otool -L /absolute/path/to/nativelink`, then replace the exact path it reports:

```sh
install_name_tool -change \
  /nix/store/qla3pimj2nqrklkhar0yv7127fcd1v86-libiconv-113/lib/libiconv.2.dylib \
  /usr/lib/libiconv.2.dylib /absolute/path/to/nativelink
codesign -f -s - /absolute/path/to/nativelink
```

**Prefer the source build of the patched worker.** The spike found that it already links `/usr/lib/libiconv.2.dylib`, making this surgery unnecessary. Verify its load commands rather than applying the workaround unconditionally. The issue being closed does not prove every published artifact was replaced.

### 2. High job counts produced `EHOSTUNREACH`

The spike attributed connection exhaustion over WiFi to mono's default `--jobs=200`. Changing from the LAN address to the Tailscale address, with `--jobs=16` and `--remote_max_connections=8`, restored operation. These were combined changes, not an isolated causal experiment proving which network component failed. Start with these known-working client limits; they do not replace worker-side capacity limits, and 20 clients can still create substantial aggregate traffic.

### 3. Apple actions failed with `Error: DEVELOPER_DIR not set`

Propagate a valid worker Xcode developer directory through **both** `--action_env=DEVELOPER_DIR=...` and `--host_action_env=DEVELOPER_DIR=...`. An interactive shell export alone is insufficient. The path must exist on each worker, not merely on the client; use consistent Xcode provisioning and the [pinning runbook](../../tools/boss/docs/mac-toolchain-xcode-pinning.md).

### 4. rustc could not load its own driver library

The decisive error was `dyld: Library not loaded: @rpath/librustc_driver-<hash>.dylib`.

NativeLink materialized executable inputs as hardlinks from its flat content-addressed executable store into the action tree. The two names share an inode. On the spike's macOS host, the kernel path queried through `F_GETPATH` identified the CAS name, so dyld evaluated `@loader_path/../lib` beside the CAS rather than beside the toolchain's `bin/rustc`. The expected relative `lib` directory existed in the action tree, but the loader searched the wrong tree.

Preserve these ruled-out paths:

- **Not a rules_rust bug.** Issue #2331 correctly closed in 2023 with a reproducer outside Bazel. Its old comment says switching symlinks to hardlinks helped that reproducer; the newer spike found a CAS path from `F_GETPATH` with either kind of link. Do not generalize that old workaround to this macOS execution path.
- **Not a hardlink-versus-symlink choice.** A distinct executable inode is the relevant change here.
- **`DYLD_LIBRARY_PATH` is not a complete fix.** NativeLink #1608 describes this workaround (its body actually spells the variable `DYLIB_LIBRARY_PATH`, unlike its title). In the spike, protected shell processes stripped `DYLD_*` variables under macOS SIP, so shell-wrapped bootstrap actions still failed. Do not disable SIP or stage driver libraries beside the CAS as the deployment solution.
- **Not the observed Xcode skew.** The spike ruled out client Xcode 26.6 versus worker 26.3 for this failure. That is not a general guarantee that arbitrary SDK/version skew is safe.

### The implemented fix, read from the fork

[Commit `3125a4fd`](https://github.com/brianduff/nativelink/commit/3125a4fd9c0363d6264ce5ff9d5bd1b2d45eb3a7) changes three files:

| File                                               | Actual change                                                                                                                                                         |
| -------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `nativelink-util/src/fs.rs`                        | Adds macOS-only `clonefile_many`: one permit/blocking dispatch for a batch, `CLONE_NOFOLLOW`, per-entry ordered results, and defensive destination cleanup on failure |
| `nativelink-worker/src/running_actions_manager.rs` | Adds `PendingLink.executable`; `materialize_link_batch` clones macOS executables, hardlinks other inputs, and restores result ordering                                |
| `nativelink-util/tests/fs_test.rs`                 | Adds a macOS-only test for distinct inode, `nlink == 1`, content and `0555` mode preservation, ordering, and an isolated failed entry                                 |

A clone shares blocks copy-on-write but has its own inode, anchoring loader-relative paths in the action tree. The patch retains batching because APFS metadata operations contend on a volume lock. This is useful for Mach-O toolchains using loader-relative rpaths generally, not only Rust; it does not repair malformed rpaths.

**Fallback qualification:** failed clones are already retried as hardlinks with a warning. That maintains prior behavior but can reintroduce this loader bug. Use the same APFS volume for worker storage and action roots. A correctness-preserving non-APFS fallback remains follow-up work; the patch does not supply one. Cross-volume hardlinks can also fail. Copy-on-write avoids immediately duplicating file contents, not all metadata/storage costs, and near-hardlink cost is a recorded expectation rather than a benchmark in this investigation.

Eviction uses an in-memory keyed eviction map, not link counts; the [filesystem store](https://github.com/brianduff/nativelink/blob/3125a4fd9c0363d6264ce5ff9d5bd1b2d45eb3a7/nativelink-store/src/filesystem_store.rs) uses `StoreKey`/digest entries in `EvictingMap`. Changing executable link counts therefore does not invalidate that accounting. This inspection is not a new eviction stress test.

No upstream PR was opened according to the spike record; a current all-states upstream PR query for this fork branch returned none. **Opening an upstream PR is still an action item.** Only macOS workers require the fork; a scheduler/CAS-only host does not execute this materialization path.

## Acceptance evidence: what worked, and what it did not prove

Before the acceptance run, the operator restored pristine `rustc` and verified its digest, moved previously staged workaround libraries aside, ran `bazel clean`, disabled disk cache with `--disk_cache=`, and ensured no `DYLD_*` variable was set anywhere.

The recorded result was:

> 418 processes, 257 internal, 161 remote, zero local execution

There were zero `Library not loaded` errors, and the executable variant showed `nlink 1`, consistent with cloning rather than hardlinking. The cold run took about **385 seconds**. “418 actions” in the session summary includes 257 internal processes; only **161** were reported as remote executions. An earlier checkpoint reported `INFO: 7 processes: 2 internal, 5 remote`, with those five actions executed on zakalwe.

These are operator-reported acceptance results, not a run repeated here. The digest value, raw execution log, exact target, revision, and remote action-cache controls were not supplied. A reproduction should additionally disable remote cache reads or use demonstrably fresh action digests and retain execution logs, rather than relying solely on a disk-cache clean.

**“The mechanism is proven; the economics aren't.”** A separate session comparison recorded **57 seconds remote** against a warm local build on a **66 Mbps, roughly 100 ms RTT** path; the exact local elapsed time was not supplied. Do not conflate that comparison with the 385-second cold run. One remote host on a slow path need not beat warm local cache. The case here is concurrent miss execution and RAM headroom across builders, not single-build latency.

## Fleet findings and the recommendation that changed

The subsequent investigation, as preserved in the supplied record, found:

| Finding                                                                                                                                    | Consequence                                                                                                    |
| ------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------- |
| 16 GB / 10-core worker: about 3.6 RAM-safe concurrent Rust actions, configured `cpu_count: 10`, with actual `OOMKilled` worker-log entries | About 2.8 times RAM-safe concurrency; **drop `cpu_count` to about 4 at zero hardware cost**                    |
| Clean mono graph about 4,620 actions; macOS app about 2,750 more; mean 8.19 s across 672 real cache misses                                 | Order-of-magnitude demand model, not a guaranteed distribution or per-action RAM bound                         |
| Realistic combined peak about 20 builds                                                                                                    | Size for concurrent laptop workspaces, not a single benchmark                                                  |
| Buildkite utilization only 1–2%                                                                                                            | The premise that CI contention was the problem was wrong; pressure was in laptop cube workspaces               |
| 127 of 136 test targets (93.4%) platform-agnostic; byte-identical 127-target result lists on two Linux hosts at the same commit            | Strong evidence for Linux validation; not proof that every remote test strategy is configured                  |
| Darwin-only tail about 150–400 actions, roughly 3–8% of the mono graph                                                                     | A small Darwin pool can remain useful in a later Linux-heavy fleet; action counts are not CPU-time percentages |

The record's **10.5 action-hours** is consistent with `4,620 × 8.19 / 3,600 = 10.51`. Applying the same mean to an additional 2,750 actions gives **16.77 action-hours** combined, if additive and without overlap or cache reuse. These are action-duration sums, not measured CPU-hours or build wall time; internal actions, different action mixes, shared dependencies, and critical paths limit this extrapolation.

Linux-to-Darwin cross-compilation was rejected during the investigation on engineering grounds and recorded Xcode SDK licensing concerns (sections 2.5 and 2.7). Preserve that decision: the Linux path means producing Linux validation artifacts, while Apple artifacts remain on Macs. This document neither reopens the choice nor independently interprets those legal terms.

The recommendation history matters:

| Historical plan                                                                                       | Recorded estimate                     | Disposition                 |
| ----------------------------------------------------------------------------------------------------- | ------------------------------------- | --------------------------- |
| Three M5 Pro minis, each 18-core / 64 GB, plus 10GbE                                                  | About $7,000; 48–66 slots             | Overturned                  |
| Existing Linux host for CAS, one Linux small-form-factor box, retuned existing Macs, and 10GbE switch | About $2,810; 64 slots; zero new Macs | Later fleet-scale direction |

The sessions reported about **$47 per Linux slot versus $111 per Mac slot**. These are historical planning estimates, not current product availability, validated RAM-safe slot counts, or purchase quotes. The totals do not reconstruct those unit costs exactly ($2,810 / 64 is about $44); the component-level denominator was not retained. Re-price and re-measure before procurement.

The settled intended ergonomics were `test --config=linux` in `.bazelrc`, default for validation tests while `build` stays host-targeted so local tools remain Mac binaries. The known implementation blocker was registering `linux_amd64_gnu.2.28` from the existing `hermetic_cc_toolchain`. Current `MODULE.bazel` confirms that dependency and registers musl variants, not that GNU variant. This investigation makes no toolchain or `.bazelrc` changes.

## Operator's current steer: incremental macOS first

> the current resource crunch on my laptop exemplifies why we need this more urgently. I think it would help a lot to just have a couple of macos workers.

This is a useful smaller next step, even though it differs from the cheapest-per-slot fleet destination. The Darwin clonefile fix already exists and has run successfully against mono. Linux first still requires toolchain registration, deliberate Linux targeting, and a validation workflow migration. **Incremental-first and cheapest-per-slot point in different directions.** Start with two compatible Macs, preferably existing machines, to avoid making laptop relief wait for that migration.

### How much would two workers absorb?

For two 16 GB workers, use **7.2 RAM-safe action equivalents**, not their nominal 20 cores. Advertising four slots each supplies eight scheduler slots, which is slightly above the estimated safe average; monitor RSS, memory pressure, and OOMs, and reduce to three each if peaks require it. Four is a starting tune, not a memory guarantee.

At an assumed unchanged mean of 8.19 seconds per action:

| Derived quantity                              | Calculation                                     | Interpretation                                                                                            |
| --------------------------------------------- | ----------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| Saturated two-worker service rate             | `7.2 / 8.19 = 0.88 actions/s`, about 3,165/hour | Optimistic throughput before transfer, queueing, dependency, and action-mix effects                       |
| 20 builds, each ready to run one heavy action | `7.2 / 20 = 36%`                                | About seven build actions can run remotely together, with others queued; not 36% of all laptop load       |
| 20 builds, each exposing four actions         | `7.2 / 80 = 9%`                                 | Much less coverage during broad cold-build fanout                                                         |
| One cold mono graph                           | `10.51 / 7.2 = 1.46 hours`                      | Model service time if those action-hours all need this pool                                               |
| 20 independent cold mono graphs               | `20 × 10.51 / 7.2 = 29.2 hours`                 | Aggregate drain time, not an acceptable peak-build SLA; shared cache/dedup may greatly reduce real demand |

If all eligible compilation queues remotely, even a small pool can eventually absorb that eligible work instead of executing it locally, at the cost of waiting. If overloaded clients fall back locally, relief is bounded by the remote service rate. Neither model supports promising an exact reduction from load 296 to a particular number: action arrival rates, CPU/RAM distributions, local tests, and analysis costs are missing.

Under an illustrative one-core-per-action model, 7.2 remote slots add 60% to a 12-core laptop's nominal execution capacity and account for 37.5% of a combined 19.2-slot pool. This is a capacity comparison, **not** a prediction for that oversubscribed laptop. The immediate goal should be clean gates completing with lower local pressure, not keeping every local core busy.

**What spec changes the result?** More RAM per worker, together with enough cores, is the useful upgrade. A deliberately rough linear extrapolation of 3.6 slots per 16 GB gives 7.2 per 32 GB and 14.4 per 64 GB, before CPU caps and OS/service reserves. Two 32 GB Macs approach 14 slots; two 64 GB Macs with at least 15 usable cores each approach 28 slots. Two 10-core/64 GB Macs are instead CPU-capped near 20 slots. These are sizing hypotheses: validate peak Rust RSS and mixed workloads before advertising them. For 20 simultaneous single-action builds, roughly 10 safe slots per worker suggests about 45 GB each by that crude ratio, making 64 GB a sensible class to evaluate, not a purchase commitment.

### What stays local today

RBE leaves Bazel loading/analysis, resident JVMs, agents, and some output materialization on the laptop. More critically, current `.bazelrc` explicitly sets macOS `TestRunner=local` (and Linux `TestRunner=linux-sandbox`). Adding executor endpoints alone will not move the observed 83 test processes. Compilation offload is the proven first slice; remote tests need a separate change that preserves the hermetic wrapper's protections and verifies execution placement. Do not remove the wrapper or assume a remote action root provides equivalent isolation. Whole-agent distribution remains complementary because it can move these other costs too.

### Minimum viable topology

Use one existing, always-on **Linux host for CAS + action cache + scheduler**, and **two macOS workers**, one of which can be zakalwe. This follows #2718's service placement and reserves Mac RAM for compilation. If bringing up that service host delays the first smoke test, zakalwe's proven all-in-one configuration plus a second worker is a valid transitional topology, with extra service memory reserved and lower advertised execution capacity.

```text
Laptop: Bazel clients, analysis, current local tests
    | private wired LAN / Tailscale; client API :50051
Linux host: CAS + action cache + scheduler
    | worker API :50061; CAS traffic :50051
    +-- macOS worker A: patched NativeLink, APFS, ~4 slots initially
    +-- macOS worker B: patched NativeLink, APFS, ~4 slots initially
```

The laptop is **not a NativeLink worker or central CAS host** in this proposal. NativeLink already supports N workers connecting to one scheduler through `connect_worker`; the [scheduler config](https://github.com/brianduff/nativelink/blob/3125a4fd9c0363d6264ce5ff9d5bd1b2d45eb3a7/nativelink-config/src/schedulers.rs) defaults to `LeastRecentlyUsed` allocation. That is worker selection, not proof of per-build fairness or protection from memory overcommit.

Prefer wired Ethernet for the laptop, CAS, and workers; begin with available wired capacity and measure before buying 10GbE. Keep service access private and authenticated, with worker-port access limited to workers. The recorded 66 Mbps / 100 ms path was the spike's slow network, with WiFi involved; no wired control or Tailscale direct-versus-relay diagnosis was supplied. It is **not a representative wired-LAN benchmark**, but attributing all 100 ms to WiFi is also unproven. Measure RTT, bulk CAS transfers, and overlay route on the actual placement. If the hosts remain geographically remote, Ethernet alone will not remove WAN latency.

## Reproduction and new-worker setup path

This is a reconstruction from the recorded fixes and pinned source examples, **not a recovered copy of zakalwe's private `rbe.json5` or a command sequence executed during this documentation run**. Perform it in a dedicated NativeLink checkout and disposable mono output base; retain the final configuration and evidence for the next operator.

1. **Step zero: retune existing capacity.** Change the 16 GB worker's advertised `cpu_count` from 10 to about 4, drain/restart it, and confirm the scheduler sees the new capacity. Ensure actions request `cpu_count=1` and the scheduler treats it as a `minimum` consumable property. Recheck OOMs before adding demand.
2. **Prepare each Mac.** Install the agreed Xcode/SDK and command-line tools; record `sw_vers`, `xcodebuild -version`, and `xcode-select -p`. Check storage with `diskutil info`: executable store, worker temporary storage, and action roots must be on the same APFS volume. Reserve RAM for the OS and other services. Use a dedicated service account and explicit writable directories.
3. **Build the pinned fork.** Obtain `brianduff/nativelink` at full commit `3125a4fd9c0363d6264ce5ff9d5bd1b2d45eb3a7`; verify the revision, not just the moving branch name. Its root [`BUILD.bazel`](https://github.com/brianduff/nativelink/blob/3125a4fd9c0363d6264ce5ff9d5bd1b2d45eb3a7/BUILD.bazel) defines `//:nativelink`: run `bazel build //:nativelink` in that checkout. Run `bazel test //nativelink-util:integration` on macOS and confirm its `fs_test` results include `clonefile_many_clones_in_order_and_isolates_failures`. Install the resulting binary, record its digest, inspect `otool -L`, and check `--version`. No source-build command or test was run here; a version string alone does not identify the patch.
4. **Configure the service and worker.** Start from the pinned [all-in-one example](https://github.com/brianduff/nativelink/blob/3125a4fd9c0363d6264ce5ff9d5bd1b2d45eb3a7/nativelink-config/examples/local_rbe_self_test.json5) for a smoke test, or the [separate-worker example](https://github.com/brianduff/nativelink/blob/3125a4fd9c0363d6264ce5ff9d5bd1b2d45eb3a7/deployment-examples/docker-compose/worker.json5) for the intended topology. Replace example `/tmp` or `/root` paths with persistent absolute service-account paths and set bounded storage/eviction. Do not copy the Linux worker's `nproc`, x86-64 ISA, or namespace flags onto macOS: use static capacity, correct Darwin/ARM64 properties, and `use_namespaces: false`, `use_mount_namespace: false`.
5. **Wire both protocols.** `worker_api_endpoint.uri` goes to the scheduler on 50061; worker slow CAS and action-result upload stores point to the shared CAS/AC on 50051. Keep the worker's fast filesystem store local on APFS. Set `platform_properties.cpu_count.values: ["4"]`. Configure exact OS/architecture matching in the scheduler and matching client properties; the simple example's `OSFamily: "priority"` is informational, not isolation between Linux and Darwin. Keep instance names consistent across services and clients. Start the installed binary with the absolute config path (historically `~/nativelink/rbe.json5`) under the host's service manager; confirm both workers register and receive actions.
6. **Connect one client with the proven environment and limits.** Use the Tailscale address initially if reproducing the spike, then compare a measured private wired path. Match Xcode paths across workers. The following is an illustrative Rust compilation probe, not the lost original 418-process command; substitute endpoint and developer directory with actual values:

```sh
bazel --output_base=/absolute/path/to/disposable-output clean
bazel --output_base=/absolute/path/to/disposable-output build \
  //tools/boss/engine/core:engine_lib \
  --remote_executor=grpc://PRIVATE_SERVICE_ADDRESS:50051 \
  --remote_cache=grpc://PRIVATE_SERVICE_ADDRESS:50051 \
  --remote_instance_name= \
  --jobs=16 --remote_max_connections=8 \
  --remote_default_exec_properties=cpu_count=1 \
  --action_env=DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  --host_action_env=DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  --disk_cache= --noremote_accept_cached --noremote_local_fallback \
  --spawn_strategy=remote \
  --build_event_json_file=/absolute/path/to/evidence/build.bep.json \
  --execution_log_json_file=/absolute/path/to/evidence/execution.json \
  --profile=/absolute/path/to/evidence/build.profile.gz
```

Add the OS/architecture execution properties agreed in step 5; their exact labels were not retained from the spike. `--spawn_strategy=remote` and no local fallback make ordinary remote-capable compile placement auditable, but explicitly local/internal actions still need classification. Do not use this command as evidence that all tests or all action mnemonics are remote.

7. **Repeat the clean controls.** Verify pristine toolchain executable digests against the pinned distribution; move old library workarounds aside; inspect client, worker service, and action environments for `DYLD_*` variables. Keep the default disk cache intact for ordinary use; disable it only for this controlled probe. Retain action logs and reject unexplained cache hits or local compiles. Inspect materialized executables with `stat -f '%i %l %N'` against the source CAS entry: require different inodes and destination link count one, and no clone-fallback warning. A link count alone is not proof of a clone.
8. **Scale the proof, then enable normal caching.** Force known misses separately on each Mac, including a shell-wrapped bootstrap action; require no loader errors and successful remote results. Repeat unchanged work to prove cache reuse. Run representative concurrent edited builds and retain queue/execution/transfer time, local/remote counts, worker peak RSS/OOMs, laptop pressure, and gate completion rate. Preserve the current test sandbox until a separate remote-test validation passes. Exercise worker loss with an explicit bounded fallback policy: do not let every client silently spill a remote backlog onto the laptop. The probe deliberately fails closed; routine rollout fallback must be measured and agreed separately.

## Follow-up work and unresolved questions

- **Upstream the worker patch:** submit the existing commit with the clean reproduction; settle a correctness-preserving fallback for filesystems without clone support and test it. No upstream PR exists for this branch as verified here.
- **Stand up the bounded two-Mac pilot:** apply the free tuning first, capture actual wired-path performance, and measure concurrent gate completion. Decide whether available RAM is sufficient before purchasing more cores.
- **Port remote tests without weakening isolation:** audit the repository wrapper and strategy constraints, then prove equivalent filesystem, process, credential, and network protection. The current compiler success does not answer this.
- **Complete Linux validation separately:** register the GNU toolchain and implement the intended test-default/build-host-targeted workflow, preserving Darwin-only targets. Recheck the 127-target result set at the chosen revision.
- **Resolve licensing before production:** NativeLink FSL “shared production use” remains an operator/legal question. This document offers no legal conclusion.
- **Retain better evidence:** the original spike date, target/revision, raw 418-process logs, peak memory distribution, component cost sheet, and network route are unavailable here. Future runs should retain them alongside BEP and execution profiles so sizing can replace the current estimates.

The smallest useful next step is therefore a measured macOS compilation pool with conservative RAM limits. It creates a place for cache misses to execute while the broader Linux and remote-test work catches up; it does not promise that two small Macs solve the entire laptop resource crunch.
