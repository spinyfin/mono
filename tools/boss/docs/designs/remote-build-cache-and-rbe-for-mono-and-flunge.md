# Bazel remote cache first (not RBE): shared cache for mono and flunge

- **Date:** 2026-08-10
- **Status:** design (not implemented)
- **Kind:** project design for shared Bazel remote caching / optional remote execution
- **Repos:** `spinyfin/mono` (this doc); intended consumers include `spinyfin/mono`, flunge, and future Bazel repos
- **Related:** [Distributed agent execution](distributed-agent-execution-register-and-dispatch-to-remote-ssh-hosts.md) (whole-agent offload); [Boss CI / Buildkite](boss-ci-buildkite-pipeline-mirroring-flunge.md) (disk cache shipped, remote cache still open); [Flunge Buildkite reference](flunge-buildkite-pipeline-reference.md) (unused BuildBuddy `config:remote`); [Xcode pin runbook](../mac-toolchain-xcode-pinning.md); investigation [checkleft CI timing](../../../../docs/investigations/checkleft-checks-ci-timing.md)

## Verdict

**Ship a shared remote _cache_ first. Defer remote _execution_ (RBE) until measurements after cache-only show residual load that still justifies it.**

The contested property is simple: for the operator's MacBook, Linux RBE does **not** move the work that saturates the laptop. Boss workers build `darwin_arm64` artifacts. Linux executors produce a different platform key. The actions that dominate wall-clock on agent-sized edits (`Rustc` of large crates such as `engine_lib`) only leave the laptop via (a) a cache hit from another host that already built the same `darwin_arm64` action, or (b) macOS remote executors, or (c) moving the whole agent to another Mac (distributed agent execution).

Remote cache captures most of the cross-worker / cross-CI benefit at a fraction of the ops cost. Full RBE is a later, optional lever — and a legitimate end-state is "cache forever, never RBE."

---

## Goals

- Reduce aggregate Bazel CPU load on the operator's laptop when many concurrent Boss workers build mono (and, where relevant, flunge).
- Provide **shared** remote infrastructure usable by mono, flunge, and future Bazel repos — not a per-repo one-off.
- Quantify how much load can actually leave the laptop (measured, not assumed).
- Separate **remote cache** from **remote execution** and recommend a sequencing call.
- Produce a cost and hosting comparison that can be re-derived when assumptions change.
- Itemize hermeticity work required before either cache or execution can be trusted.
- State how this composes with distributed agent execution (whole agents on remote SSH hosts).
- Keep Bazel as the only sanctioned build path (no cargo/xcodebuild fallbacks).

## Non-goals

- Implementing cache or RBE in this design PR (doc only).
- Buying hardware, signing vendor contracts, or provisioning cloud accounts (operator decisions; described, not tasked).
- macOS App Store signing/notarization pipelines.
- Replacing or delaying distributed agent execution; that remains the primary whole-agent relief path.
- Unifying mono and flunge into one workspace or one MODULE.
- Weakening hermetic test wrappers, sandbox policies, or CI build gates to make remoting easier.
- Designing multi-tenant or multi-operator shared infrastructure.

---

## Problem framing

Boss runs many concurrent workers on the operator's Mac. Each worker repeatedly runs Bazel (`build` / `test` / checkleft's clippy aspect). Local disk cache (`~/.cache/bazelcache`) is already on in mono's `.bazelrc`, and CI has large per-agent SSD disk caches — but those caches are **per host**. Concurrent workers on one laptop share one disk cache; workers on other Macs and CI agents do not share with the laptop.

The open question is not "is remote Bazel nice?" It is: **what fraction of laptop CPU-seconds actually leaves the machine under a realistic design?**

---

## Measured offload analysis

### Methodology (re-runnable)

Host for all local measurements (2026-08-10):

- Apple M2 Max, 12 cores, 64 GB RAM, macOS 26.5 (`Darwin 25.5.0`)
- Bazel **9.1.0** (`.bazelversion`)
- Workspace: this mono checkout

Tools used (Bazel 9.1.0):

- `--profile=<file.profile.gz>` (Chrome-trace JSON; `analyze-profile` is not a Bazel 9 command)
- Process summary lines from the Bazel client (`N processes: … action cache hit, … disk cache hit, … local`)
- `bazel aquery 'mnemonic(".*", deps(<target>))'` for action-count mix (analysis graph, not wall time)
- Forced cold build: `--nouse_action_cache --disk_cache=<empty dir>`
- Incremental: `touch` on `tools/boss/engine/core/src/lib.rs` then rebuild
- Profiles parsed for `cat=action processing` and `cat=critical path component` events

Flunge was **not** built in this workspace (separate private repo; API not available to this worker). Flunge conclusions use the in-repo audit at [flunge-buildkite-pipeline-reference.md](flunge-buildkite-pipeline-reference.md) plus the same platform logic.

### Action mix (aquery counts — structure, not CPU)

| Target                                     |                                                                                                                        Dominant mnemonics (counts) | Notes                                                                           |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------: | ------------------------------------------------------------------------------- |
| `//tools/boss/engine/core:engine_lib_test` |                                      Symlink 1092; **Rustc 446**; ExtractCargoTomlEnvVars 306; CargoBuildScriptRun 40; CppCompile 2; TestRunner 11 | Pure Rust + 2 C compiles (bundled sqlite / ring-class)                          |
| `//tools/boss/app-macos:Boss`              | Symlink 1108; **Rustc 425**; ExtractCargoTomlEnvVars 279; CargoBuildScriptRun 37; **SwiftCompile 6**; SwiftDumpAST 6; ObjcLink/Bundle/sign present | Graph is still mostly Rust; Swift is few actions but high wall on critical path |
| `//tools/checkleft:checkleft`              |                                                                                Symlink 800; **Rustc 543**; CargoBuildScriptRun 83; WasmComponent 1 | Linux-friendly except wasm tooling already hermetic                             |

Action counts and CPU diverge badly: six Swift compiles can dominate wall time for the app binary.

### Scenario results (wall clock)

| Scenario                     | Command / setup                                                                                  |     Elapsed | Critical path | Processes / cache                                                                                                                                                         |
| ---------------------------- | ------------------------------------------------------------------------------------------------ | ----------: | ------------: | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Cold `engine_lib`**        | `bazel build //tools/boss/engine/core:engine_lib` with empty disk cache + `--nouse_action_cache` | **586.9 s** |   **570.8 s** | 1516 total; **706** sandboxed local; 0 cache hits                                                                                                                         |
| **Incremental `engine_lib`** | `touch` `engine/core/src/lib.rs` then rebuild with normal caches                                 | **102.7 s** |   **100.7 s** | 4 processes; 2 action-cache hits; **3** sandboxed (almost all time in one `Rustc`)                                                                                        |
| **Warm-ish test path**       | `bazel test //tools/boss/engine/core:engine_lib_test` (existing caches)                          | **294.7 s** |   **255.9 s** | 1548 action-cache hits; 12 disk-cache hits; 22 local (+ test shards failed in this worker sandbox: `sandbox-exec: Operation not permitted` — compile profile still valid) |
| **App `Boss` (warm disk)**   | `bazel build //tools/boss/app-macos:Boss`                                                        | **243.4 s** |   **240.9 s** | 527 disk-cache hits; 31 sandboxed; 1 worker                                                                                                                               |

### CPU / wall share by toolchain (from profiles)

**Cold `engine_lib` — action-processing wall (sum of parallel spans; sum ≫ elapsed):**

| Class                   | Action-processing sum | Action count |
| ----------------------- | --------------------: | -----------: |
| Rustc rlib              |                1906 s |          331 |
| Rustc proc-macro        |                 587 s |           24 |
| ExtractCargoTomlEnvVars |                 532 s |          264 |
| CargoBuildScriptRun     |                 230 s |           36 |
| Rustc bin               |                 227 s |           39 |

Critical path top: `Compiling Rust rlib engine_lib (454 files)` **153.5 s**, then toolchain crates (`syn`, `cargo_toml_variable_extractor`, `protocol`, `bon_macros`, …). `Running Cargo build script libsqlite3-sys` ≈ **28 s**. An `apple_genrule` libtool step (~13 s) appears on the critical path because the **host is macOS** (local Apple CC tooling), not because `engine_lib` is a Swift target.

**Incremental after agent-sized edit:** ~**99.6 s / 100 s critical path** is a single `Rustc` of `engine_lib (454 files)`. Workspace-status / provenance stamping is sub-second to ~0.5 s.

**App `Boss` (warm disk):**

| Class                        |                 Action-processing sum | Critical-path role                          |
| ---------------------------- | ------------------------------------: | ------------------------------------------- |
| Swift (`boss_mac_app_lib`)   | **227 s** (1 dominant module compile) | **Critical path head**                      |
| Rustc (engine + binaries)    |      **211 s** sum across 132 actions | Parallel with Swift; engine_lib alone 121 s |
| Process/sign + bundle + link |                   ~12–15 s on CP tail | Apple-local                                 |

### What cannot be remoted (stays on the laptop under Linux RBE)

| Category                                                     | Why non-remote on Linux RBE                                                                                 | Share of measured load                                                            |
| ------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| **All `darwin_arm64` Rustc / link** when executors are Linux | Platform / exec-compatible mismatch: Linux workers produce different action keys                            | **~100% of cold/incremental engine build CPU on the laptop** under Linux-only RBE |
| **Swift / ObjC / codesign / Xcode toolchains**               | Non-hermetic, Apple-host-bound; need macOS + licensed Xcode                                                 | Dominant for app critical path (~227 s Swift in the measured app build)           |
| **Local test strategy / Seatbelt wrapper**                   | `.bazelrc` forces `TestRunner=local` on macOS; hermetic wrapper is host-specific                            | Test _execution_ stays local even if the test _binary_ were built elsewhere       |
| **Stamped / `action_env` secret surfaces**                   | `workspace_status_command`, BOSS_SHAKE `action_env` / `--define` can poison or narrow cache keys if mis-set | Small wall share; large cache-correctness risk                                    |
| **Analysis / Starlark / package loading**                    | Always local to the client                                                                                  | Seconds to tens of seconds per cold analysis; not offloaded by RBE                |

### Defensible offload fractions (laptop saturation)

Define three designs:

1. **Linux RBE only** (no Mac executors, no shared cache of darwin artifacts).
2. **Shared remote cache only** (read/write from laptop, other Macs, CI Linux — platform keys keep OS artifacts separate).
3. **Cache + optional later RBE** (Linux for CI/linux targets; Mac executors only if ever justified).

| Workload                                       |                                                                         Linux RBE only |                                                                                      Remote cache only (N Macs + CI) | Notes                                                                                                                                                                     |
| ---------------------------------------------- | -------------------------------------------------------------------------------------: | -------------------------------------------------------------------------------------------------------------------: | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Cold `engine_lib` on laptop                    |                                                          **~0%** of compile CPU leaves |                         **High for deps, low for first producer** — first cold payers still compile; peers hit cache | Cold measured ~587 s; deps dominate parallel sum                                                                                                                          |
| Incremental agent edit of large crate          |                                                                                **~0%** |                                      **~0–20%** typical — the dirty crate usually has a unique input hash per branch | Measured ~100 s all-local `Rustc` of `engine_lib`                                                                                                                         |
| Concurrent workers, mixed branches off `main`  |                                                                                **~0%** | **~40–70% of aggregate cold-dep CPU** across the pool after the first producer (order-of-magnitude; see model below) | Cache hits on shared crates (`syn`, `tokio`, …) and on identical main-adjacent builds                                                                                     |
| App `Boss`                                     | **0% of Swift CP**; Rust parallel portion still platform-local without Mac cache peers |                                                                Swift stays local; Rust portion cacheable across Macs | Apple stays local by policy                                                                                                                                               |
| CI Linux `bazel-build-test` / checkleft clippy |                                                                         High potential |                                                                                                       High potential | Already measured cold-vs-warm clippy swing **2 s ↔ 29 s+** on the same agent class ([checkleft CI timing](../../../../docs/investigations/checkleft-checks-ci-timing.md)) |

**Bottom line for the brief's decisive question:** a design that promises "near-total offload of laptop Bazel via Linux RBE" is wrong by more than a factor of three — it is closer to **near-zero** for the darwin_arm64 agent path. The load that can leave the laptop without buying Mac executors is almost entirely **duplicate work avoided by a shared cache**, not **execution farmed out to Linux**.

### Cross-worker cache hit model (Boss shape)

Assumptions (stated so they can be re-derived):

- Pool size \(N = 8\) concurrent workers (current order of max worker pool).
- Each works a different branch off a common `main`, agent-sized edits (one crate / few files).
- Platform: mostly `darwin_arm64` for interactive Boss workers; CI also `linux` and `darwin`.
- Action graph for a typical engine rebuild: hundreds of third-party `Rustc` actions + one large dirty first-party crate.

Expected behavior:

- **Third-party / unchanged first-party crates:** high cross-worker hit rate once any peer (or CI) populated the cache for that platform + flag set — often **>80% of those actions** after warm-up.
- **Dirty first-party crate on a unique branch:** hit rate **near 0%** until merge; that is the measured ~100 s `engine_lib` recompile.
- **Exact same commit rebuilt on another host** (CI + laptop, or two agents on same SHA): near-total hit for compile actions.

So remote cache attacks **fleet-wide duplicate compile** and **cold-agent pain**, not the irreducible recompile of the crate the agent just edited.

---

## Known constraints (verified) and additional findings

### 1. Apple / Xcode — stays local

Verified:

- All Swift/Apple rules in mono live under `//tools/boss/app-macos/...` (and test-sandbox macOS helpers).
- CI already pins Xcode on Darwin (`--xcode_version` + `DEVELOPER_DIR` in `.ci.bazelrc`) after real incidents; see [mac-toolchain-xcode-pinning.md](../mac-toolchain-xcode-pinning.md).
- Measured app build: **Swift owns the critical path** (~227 s for `boss_mac_app_lib`); codesign/bundle follow.

macOS RBE workers (MacStadium bare metal, etc.) are _technically_ possible but:

- Hardware + macOS licensing cost is high relative to benefit (see cost section).
- Operationally similar to "buy another Mac and run agents there" — which is exactly distributed agent execution, at coarser granularity and with better product fit.

**Honest answer: Apple stays local. Do not plan Linux RBE for Swift.**

### 2. System-library linkage in Rust — better than feared for mono

Verified from `Cargo.toml` / `Cargo.lock` / `MODULE.bazel`:

| Dependency                                                                                      | Role                          | Hermeticity                                                                                                                                                                                                                  |
| ----------------------------------------------------------------------------------------------- | ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `rusqlite` with **`bundled`**                                                                   | SQLite via `libsqlite3-sys`   | Compiles bundled `sqlite3.c` (not system lib). Mono already annotates `libsqlite3-sys` build-script env (`LIBSQLITE3_FLAGS`, `OPT_LEVEL`) in `MODULE.bazel`. Cold profile: ~28 s `CargoBuildScriptRun` for `libsqlite3-sys`. |
| `reqwest` + `rustls` + **`ring`**                                                               | TLS                           | No `openssl-sys` in the lock for mono's chosen path; `ring` builds via `cc` crate.                                                                                                                                           |
| `security-framework` / `core-foundation`                                                        | `//tools/boss/keychain`, hood | **macOS-only** frameworks; not a Linux RBE problem if those targets stay on macOS.                                                                                                                                           |
| First-party `build.rs` files (`cli`, `bossctl`, `engine/core`, `build-provenance`, `checkleft`) | Env / version wiring          | Comments state Bazel sets `rustc_env` and **does not rely on build.rs for Bazel builds** for the Boss stamp crates; checkleft's build.rs reads `Cargo.lock` for wasmtime version (cache-key input, not host probe).          |

**Not present as a primary mono path:** classic `openssl-sys` / system OpenSSL, `pkg-config` discovery of host OpenSSL, libsso-style deps. The operator's general RBE pain point is real in the industry; for **this** mono graph it is largely already avoided by bundled sqlite + rustls/ring.

Flunge must be re-audited in-repo when a flunge worker runs (not measured here). Expect more native surface if mobile/iOS toolchains and Node/backend differ.

### 3. Toolchain hermeticity and `bazel clean --expunge`

Mono has documented Xcode / `apple_support` pin fragility requiring expunge or targeted output-base surgery. Under **remote cache**:

- A poisoned local toolchain config still breaks _local_ analysis; remote cache does not fix bad repo-rule snapshots.
- Under **RBE with a container image**, Linux toolchains become "whatever is in the image" — an argument _for_ RBE on Linux CI, because the image is the pin.
- That argument does **not** transfer to macOS Xcode (you cannot put a licensed Xcode stack in a portable Linux container).

RBE/Linux is a hermeticity win for Linux CI. It is not a fix for LaunchServices / Xcode registration on Mac hosts.

### 4. Additional constraints found

- **`--stamp` + workspace status:** always on in mono `.bazelrc`. Comments claim only version plist / build-info consume `ctx.info_file`. Keep it that way; any accidental stamp on heavy compile actions destroys cache hit rate.
- **`BOSS_SHAKE_*` via `--action_env` / `--define`:** empty defaults in `.bazelrc`; non-empty values change cache keys for targets that consume them. Remote cache must document "dev builds with empty defines" vs release defines.
- **`--jobs=200`:** saturates a laptop by design when many actions are local. Cache hits reduce action execution; they do not reduce analysis thrash.
- **Analysis-cache flips:** `.bazelrc` already documents build/cquery/test `run_under` alignment. Remote cache does not fix analysis thrash; option discipline still matters (see clippy aspect discarding analysis cache in CI timing investigation).
- **Network-denied tests:** good for hermeticity; remote test execution would need explicit network requirements (already a mono pattern via `network_enabled_rust_test`).
- **Large artifacts:** Ghostty prebuilt xcframework, wasm tools archives — cache storage and transfer costs matter more than CPU for these.
- **Case sensitivity / absolute paths:** risk for any action that embeds host paths; rules_rust + bundled deps are mostly fine; custom genrules must be audited.
- **Flunge `config:remote` BuildBuddy keys in public `.bazelrc`:** already noted in the flunge audit as intentional but unused. Do not copy secrets into mono; use private headers / CI secrets.

---

## Remote cache vs remote execution

| Dimension                  | Remote cache                                                                    | Remote execution                                              |
| -------------------------- | ------------------------------------------------------------------------------- | ------------------------------------------------------------- |
| What moves                 | Action digests + outputs                                                        | Inputs + execution + outputs                                  |
| Laptop darwin_arm64 relief | Duplicate compiles avoided                                                      | Only with **Mac** executors (or wrong-platform Linux results) |
| Ops burden                 | Low (`bazel-remote` on one box)                                                 | High (workers, images, scheduling, failure domains)           |
| Hermeticity bar            | Outputs must be correct for the key; non-hermetic local tools still run locally | Full hermetic toolchains on executors                         |
| Cost at this scale         | Tens–low hundreds USD/mo self-host; or free tier managed with care              | Managed enterprise quotes; Mac cores expensive                |

**Sequencing recommendation: remote cache first; RBE later or never.**

Conditions to revisit RBE:

1. Cache-only has been live ≥4 weeks with metrics (hit rate, bytes, laptop load).
2. Residual pain is **CI Linux wall time** or **throughput**, not MacBook fans — then Linux RBE (or more CI machines) is in scope.
3. Residual pain is still laptop-local **despite** cache + distributed agents — only then evaluate Mac executors, and compare them directly to "another agent host" under distributed agent execution.

---

## Chosen approach

### Architecture (v1): shared remote cache only

```text
  [Boss laptop] ──┐
  [zakalwe / other Macs] ──┼── gRPC/HTTP ──► Remote cache service  ◄── CI Linux agents
  [Future repos] ──┘                         (CAS + AC)
```

- Protocol: Bazel's remote cache API (HTTP and/or gRPC). Prefer **gRPC** for performance; HTTP is fine for a first smoke test.
- Software default for a solo operator: **`buchgr/bazel-remote`** (cache-only, single binary/container, disk-backed, max-size GC). See software comparison below.
- Instance layout: **one logical cache service** shared by mono and flunge (and future repos). Action keys already include repository/workspace content hashes; mixing repos is safe. Optionally separate **instances** (`--remote_instance_name`) later for blast-radius, not for correctness.
- Auth: mTLS or a shared bearer token via `--remote_header` / credential helper. **Never commit live keys** to mono public history.
- Client config: opt-in `--config=remote-cache` in each repo's `.bazelrc` (not on by default until soak), with:

  - `--remote_cache=grpc://…`
  - `--remote_upload_local_results=true` (or false on pure CI readers if desired)
  - `--remote_timeout=…`
  - **No** `--remote_executor` in v1

- Platform reality: cache stores **per-platform** artifacts. Linux CI does not populate darwin_arm64 slots; Mac workers do not populate linux_x86_64 slots. That is fine — the laptop benefits from **other Macs** and from **repeat builds on the same machine after disk-cache GC**.

### What v1 explicitly does not do

- No remote executor flag.
- No requirement that tests run remotely.
- No change to hermetic test wrappers or sandbox defaults.
- No weakening of Bazel-as-source-of-truth rules.

### Interaction with distributed agent execution

Reference design: [distributed-agent-execution-register-and-dispatch-to-remote-ssh-hosts.md](distributed-agent-execution-register-and-dispatch-to-remote-ssh-hosts.md).

| Question                                       | Answer                                                                                                                                                                                                                                                                                                                        |
| ---------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Does remote cache reduce that project's value? | **No.** Cache reduces _duplicate compile_; distributed agents remove _whole worker processes_ (RAM, Bazel server, analysis, Swift, IDE-less agent loops) from the laptop. Complementary.                                                                                                                                      |
| Does RBE reduce its value?                     | Slightly, only if Mac RBE existed and absorbed compile — still would not move agent processes, pane, or LLM-side work.                                                                                                                                                                                                        |
| Would a remote agent host use the cache?       | **Yes.** Every host that runs Bazel should point at the same remote cache. That multiplies cache value.                                                                                                                                                                                                                       |
| Same hardware for cache + agent hosts?         | **Cache service:** small Linux box or a container on an existing CI host is enough. **Agent hosts:** prefer Macs for mono Boss work (Xcode). Do not overload the cache box with heavy agent Bazel if it is the only CAS node.                                                                                                 |
| What delivers laptop relief sooner?            | **Distributed agents move whole workers off the laptop immediately** when a second Mac exists. **Remote cache** helps as soon as ≥2 Bazel clients share it (laptop + CI Mac, or laptop + zakalwe). Order: wire cache config (days of repo work) in parallel with agent distribution (product work); neither blocks the other. |

### Rollout shape

1. Stand up cache service (operator).
2. Land opt-in `.bazelrc` config + docs in mono (and flunge).
3. Enable on one CI queue (read+write), measure hit rate.
4. Enable on operator laptop + secondary Macs.
5. Gate "RBE?" on metrics; default expectation is **stop at cache**.

---

## Alternatives considered

### Alternative A — Managed RBE+cache from day one (BuildBuddy / EngFlow)

**Why attractive:** UI, autoscaling, less self-host ops; flunge already has a dormant BuildBuddy-shaped config in its audit notes.

**Why not chosen for v1:**

- Linux RBE does not offload darwin_arm64 laptop work (measured platform argument).
- Managed Mac cores are expensive (BuildBuddy lists Mac cores at **$45 / core** on published pricing as of 2026-08-10).
- Enterprise tiers are quote-driven; Solo operator risk of surprise cost.
- Flunge's existing `config:remote` is **not activated by CI today** — there is no live production proof that managed RBE is needed.

Still a valid **cache backend** option (BuildBuddy remote cache without executor) if the operator prefers zero self-host.

### Alternative B — Full self-hosted RBE (Buildbarn / NativeLink / BuildBuddy on-prem)

**Why attractive:** maximum control; container-defined Linux toolchains.

**Why not chosen for v1:**

- Ops surface exceeds solo-operator budget (scheduler, workers, storage, image rebuilds).
- Does not solve Mac Swift or darwin_arm64 agent path without Mac workers.
- Disqualified stacks that need a platform team (see software comparison).

### Alternative C — Do nothing beyond per-host disk cache

**Why attractive:** already partially working; zero new infra.

**Why not chosen:**

- CI timing investigation already shows **per-agent disk cache** as the dominant variance driver (2 s vs 29 s+ for the same checkleft class of work). Remote cache is the structural fix for cross-host sharing.
- Concurrent Boss workers on multiple Macs will re-pay cold compiles without a shared CAS.

### Alternative D — Linux RBE only for CI; leave laptops alone

Compatible with this design as a **later phase**, not a substitute for shared cache. Cache alone may remove the CI trigger for RBE (Boss CI design already treats remote cache as the conditional fast-follow).

---

## Hermeticity work (itemized)

These are the real implementation bulk for a trustworthy cache (and any future RBE).

1. **Document and freeze the cache key surface** — list flags that must be identical across writers (`compilation_mode`, stamp defines, clippy aspect flags, `repo_env`, Xcode version on Darwin). Align CI and local `--config` so writers do not shard the cache accidentally.
2. **Stamp audit** — confirm only build-info / plist / provenance actions depend on stable-status; add a check or test that heavy `Rustc` actions are unstamped.
3. **Secret / define policy** — BOSS_SHAKE empty-vs-set; forbid uploading results from unclean secret defines if that would poison shared cache (or use separate instance names for release).
4. **Native build-script inventory** — mono: `libsqlite3-sys` (bundled), `ring`/`cc`, cargo scripts for crates.io deps; re-run on flunge. Ensure RBE images (if ever) include a hermetic CC matching `cc` crate expectations.
5. **Apple exclusion rules** — never mark Swift/ObjC/codesign as remotely executable in any future RBE config; keep `exec_compatible_with` / platform constraints correct.
6. **Absolute-path / network audit** — genrules, tests with fixtures, anything reading `$HOME` or absolute tool paths.
7. **Cache correctness soak** — build the same commit on two hosts; compare digests / test results; enable `--experimental_guard_against_concurrent_changes` discipline as needed.
8. **Failure mode** — if remote cache is unreachable, builds must succeed offline (Bazel default with cache soft-fail behavior; verify and document `--remote_local_fallback` if execution is ever added).

---

## Cost and hosting (numbers-first)

**Pricing retrieval date: 2026-08-10** (public pages; figures go stale).

### Workload assumptions (re-derive when wrong)

| Parameter                       | Assumed value                                                                                     | Rationale                                                                                                      |
| ------------------------------- | ------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| Concurrent Bazel clients        | 8 Boss workers + 4 CI agents ≈ **12**                                                             | Order of current pool + CI fleet                                                                               |
| Heavy mono compiles / day       | **40** cold-ish engine-scale compiles distributed across clients                                  | ~5 per client; speculative — replace with metrics post-cache                                                   |
| CPU-seconds / cold `engine_lib` | **~700 process-seconds** local sandboxed actions in 587 s wall (706 processes; not all full-core) | From cold measurement process count; treat **~0.5–1.0 CPU-hour** per cold engine-scale build as planning range |
| CPU-hours / day (fleet)         | **20–40** if many cold; **2–8** if mostly incremental                                             | Bounds for cost models                                                                                         |
| Cache storage steady state      | **200 GB – 1 TB** multi-platform                                                                  | Disk GC needed                                                                                                 |
| Cache egress                    | LAN / VPN preferred; public egress avoided                                                        | Egress dominates cloud bills if clients are off-LAN                                                            |

### Managed vendors

**BuildBuddy Cloud** ([buildbuddy.io/pricing](https://www.buildbuddy.io/pricing/), retrieved 2026-08-10):

| Plan       | Published limits                                                                                                | Notes                                        |
| ---------- | --------------------------------------------------------------------------------------------------------------- | -------------------------------------------- |
| Personal   | Up to **10 users**; **100 GB** cache transfer; up to **80** Linux RBE cores; remote cache + UI                  | Suitable for a smoke test                    |
| Team       | Unlimited users; cache transfer **"$X / GB"** (list price not fully numeric on page); up to **800** Linux cores | Contact / opaque mid tier                    |
| Enterprise | Unlimited cores; Mac cores advertised; quote                                                                    | **Mac: $45 / core** listed on feature matrix |
| On-prem    | MIT open-core + enterprise on-prem                                                                              | Self-host path exists                        |

**Estimated operator cost if using BuildBuddy as cache-only:** Personal tier may suffice for a solo + bots experiment until cache transfer exceeds 100 GB/month. At Team rates without a public $/GB, assume **$50–300/mo** ballpark only after measuring transfer — **do not treat ballpark as a quote**. RBE Mac cores at $45/core quickly exceed self-host Mac mini rental if many cores are reserved.

**EngFlow** ([engflow.com/product/pricing](https://www.engflow.com/product/pricing), retrieved 2026-08-10):

| Tier       | Published                                               | Notes                |
| ---------- | ------------------------------------------------------- | -------------------- |
| Free       | Single-machine RE; ≤32 cores; in-cluster cache; Linux   | Eval only            |
| Enterprise | Custom quote; Linux/macOS/Windows; managed or self-host | No public $/CPU-hour |

**AWS / Google hosted RBE legacy:** Google's hosted RBE product was retired years ago; do not plan on it.

### Self-hosted on public cloud / bare-metal rental

| Option                           | Spec (example)                                  | Published price (2026-08-10)                                                                                     | Role                                                                     |
| -------------------------------- | ----------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| **Hetzner AX102**                | Ryzen 9 7950X3D, 128–192 GB RAM, 2×1.92 TB NVMe | **€257–259 / mo** + setup (~€129) excl. VAT ([hetzner.com](https://www.hetzner.com/dedicated-rootserver/ax102/)) | Excellent **cache + Linux CI** box; unlimited traffic on standard uplink |
| **Hetzner AX42-class** (smaller) | 64 GB, 2×512 GB NVMe                            | from ~**€59–100 / mo** order of magnitude (line pricing varies)                                                  | Cache-only may fit a smaller SKU                                         |
| **AWS EC2 c7i.8xlarge**          | 32 vCPU, 64 GiB                                 | **~$1.43 / hr** on-demand (~**$1040 / mo** 24/7) via public listings                                             | Poor value vs Hetzner for always-on cache                                |
| **MacStadium bare metal**        | M4.S mini 10-core / 16 GB                       | **$149 / mo** ([macstadium.com/pricing](https://www.macstadium.com/pricing))                                     | Mac agent host or future Mac executor — not needed for cache-only        |
| **MacStadium M4.L**              | M4 Pro 12-core / 48 GB                          | **$349 / mo**                                                                                                    | Heavier Mac build host                                                   |

**Egress note:** Hetzner standard dedicated traffic is effectively free (unlimited policy on non-10G). AWS egress (~$0.09/GB public) can dominate if MacBooks pull large CAS objects over the internet — prefer VPN/Tailscale to a Hetzner/cache host, or keep the cache on LAN.

### On-prem hardware purchase (operator-leaning option)

| Item                   | Example config                                                    | Approx price (2026 retail order-of-magnitude)          | Amortization                      |
| ---------------------- | ----------------------------------------------------------------- | ------------------------------------------------------ | --------------------------------- |
| Linux cache+CI box     | Used/ workstation or mini-PC, 16+ cores, 64 GB RAM, **2 TB NVMe** | **$800–2000** once                                     | 36 months → **$22–56 / mo** capex |
| Power                  | 50–150 W average                                                  | ~$5–20 / mo @ $0.15/kWh                                | Opex                              |
| Network                | Existing home/office uplink                                       | $0 incremental if already paid                         | —                                 |
| Spare Mac (agent host) | Mac mini M4 16 GB                                                 | **~$799+** Apple retail (base configs shifted in 2026) | Dual-use: agent host ≫ cache host |

**Ops (solo operator honesty):**

- Who patches? The operator. Prefer Debian/Ubuntu + unattended-upgrades + single docker-compose for `bazel-remote`.
- When the box dies: Bazel clients fall back to local disk cache / local execution; **builds continue, just colder**. That is the right failure mode for cache-only.
- When RBE dies (if ever added): must have local fallback; otherwise the laptop regains full load **and** CI turns red — worse than today.

### Software stack comparison (self-host)

| Stack                       | Role                     | Maturity                                   | Solo-ops fit                                           |
| --------------------------- | ------------------------ | ------------------------------------------ | ------------------------------------------------------ |
| **`bazel-remote` (buchgr)** | Cache only               | High; widely used; one Go binary/container | **Best v1 default**                                    |
| **BuildBuddy OSS**          | Cache + optional RE + UI | High; more moving parts                    | Good if UI wanted; heavier than bazel-remote           |
| **NativeLink**              | Cache + RE               | Active; newer                              | Possible later RBE; not needed for v1                  |
| **Buildbarn**               | Full RE platform         | Mature at large cos                        | **Needs platform-team ops → disqualified for solo v1** |
| **EngFlow self-host**       | Full platform            | Commercial                                 | Quote + ops; overkill for cache-only                   |

### Break-even sketches

Assume cache-only on **one Hetzner AX42-class ~€80/mo (~$90/mo)** vs **BuildBuddy Personal free** until 100 GB transfer.

- If monthly cache transfer stays under Personal free tier limits → **managed free tier wins** until scale or privacy requires self-host.
- If always-on Linux RBE workers needed (16 cores 24/7): cloud VM ~$300–1000+/mo vs Hetzner dedicated ~$280/mo for far more disk — **dedicated wins** for always-on.
- MacStadium one M4.S ($149/mo) as **agent host** under distributed execution almost certainly beats Mac RBE cores at $45/core for this workload shape.
- On-prem $1500 box amortized 36 months ($42/mo) + power beats cloud always-on if the operator accepts home-lab failure modes.

**RBE vs cache spend:** do not buy RBE capacity until cache hit metrics say execution farms would run enough CPU-hours to matter. At **2–8 CPU-hours/day** residual after cache, a dedicated RBE cluster does not pay for itself.

---

## Recommendation

### Do this

1. **Implement shared remote cache** (bazel-remote or BuildBuddy cache-only) for mono + flunge + CI.
2. **Keep Apple/Swift local forever** unless a separate Mac-farm decision is made for other reasons.
3. **Continue distributed agent execution** as the primary laptop relief for whole workers.
4. **Defer RBE** until post-cache metrics demand it; default end-state may be cache-only.

### Main risk

**Cache key fragmentation** (divergent flags / Xcode pins / stamp / secret defines) produces silent low hit rates, so the operator pays ops cost without laptop relief. Mitigate with a single documented `--config=remote-cache`, CI alignment, and hit-rate dashboards from day one.

### Case against this recommendation

- If the only Bazel client that matters is a single Mac with a warm disk cache, remote cache adds network dependency for little gain.
- If the operator will finish distributed agents on powerful Macs **this week**, laptop saturation may drop enough that cache becomes "CI-only" priority.
- If flunge's dormant BuildBuddy config can be re-enabled in an afternoon for cache-only with acceptable key management, that might beat standing up bazel-remote — still cache-first, different backend.

### Conditions to recommend **not** doing this project at all

- Measured laptop load is **not** dominated by Bazel after a week of sampling (i.e. the "almost entirely bazel" diagnosis is wrong).
- Only one machine ever runs Bazel (no CI sharing, no second Mac) **and** disk cache GC is tuned so cold compiles are rare.
- Operator time is fully consumed by distributed agent execution and cache work would delay higher-leverage relief.

---

## Risks / open questions

1. **Hit-rate unknown until live** — models above are bounded by measurement of single-host builds; multi-client hit rate needs production metrics.
2. **Where to host the cache** — existing CI Linux box vs Hetzner vs BuildBuddy free tier (operator decision).
3. **VPN / reachability** — laptop on home network must reach the cache without painful latency; high RTT reduces benefit for small actions.
4. **Flunge native dependency audit** — not executed in this workspace; must be a first flunge-side task.
5. **Clippy aspect flag alignment** — CI already thrash-risks analysis cache; remote cache does not fix that (separate, already recommended).
6. **Whether release stamping should use a separate remote instance** — correctness vs hit rate.
7. **Security** — a writeable shared cache is a supply-chain surface; authn/z and preferably protected/read-only tiers for prod.

---

## Proposed implementation task breakdown

Breakdown size: 9 entries (7 in-scope, 2 deferred) — v1 is cache-only across two repos plus measurement and hermeticity seams: mono bazelrc/docs, flunge mirror config, shared client flag discipline, stamp/secret audit, CI enablement, hit-rate metrics, and a flunge native-dep investigation; RBE is deferred rather than tasked as active work.

### Depth 0 — may run in parallel

**1. Mono remote-cache client config and operator docs**

Scope: Add an opt-in `--config=remote-cache` (name flexible) to mono `.bazelrc` / `.ci.bazelrc` documentation comments pointing at a placeholder endpoint and header mechanism; write `tools/boss/docs/runbooks/bazel-remote-cache.md` covering enablement, failure mode when cache is down, and "do not commit secrets." No default-on behavior. No executor flags.

Effort hint: `small`

Dependencies: none

Scope: in-scope

**2. Flunge remote-cache client config mirror**

Scope: In the flunge repo, add the same opt-in cache-only config pattern (or rehabilitate the existing unused remote config into **cache-only**, stripping executor settings for v1). Document secret injection via CI secrets rather than committed keys. Repo-relative docs only.

Effort hint: `small`

Dependencies: none

Scope: in-scope

_Depth-0 note: tasks 1 and 2 are independent across repos and may run in parallel. No shared files._

**3. Flunge native-dependency and platform investigation**

Scope: In flunge, inventory `*-sys` crates, build scripts, iOS/macOS toolchains, and any host probes; produce a short investigation markdown with remoting/caching implications. No behavior change.

Effort hint: `medium`

Dependencies: none

Scope: in-scope

### Depth 1

**4. Cache-key flag alignment for mono CI and local configs**

Scope: Audit mono `.bazelrc`, `.ci.bazelrc`, checkleft clippy aspect flags, and CI scripts so remote-cache writers share one coherent key surface; fix any high-impact drift that would shard the cache (without broadening clippy scope). Document the aligned set in the runbook from task 1.

Effort hint: `medium`

Dependencies: Mono remote-cache client config and operator docs

Scope: in-scope

**5. Stamp and BOSS_SHAKE cache-poisoning audit (mono)**

Scope: Enumerate targets that consume stable-status / volatile status / SHAKE defines; verify heavy Rustc actions are unstamped; document release vs dev cache instance recommendations. Add a focused test or checkleft-style assertion only if a cheap, reliable guard exists — no broad refactors.

Effort hint: `small`

Dependencies: Mono remote-cache client config and operator docs

Scope: in-scope

_Depth-1 parallelism: tasks 4 and 5 may run in parallel after task 1; both edit bazelrc/docs — if simultaneous, sequence 4 then 5 and forward-port preservingly._

### Depth 2

**6. Enable remote cache on one mono CI queue (read+write) with metrics**

Scope: Wire mono Buildkite (or the chosen CI entry) to pass `--config=remote-cache` against the operator-provided endpoint via secrets; emit or capture hit-rate / transfer stats (BuildBuddy UI, bazel-remote Prometheus, or Bazel built-in cache stats in logs). Roll forward only after a green soak on non-required experiment or a single queue.

Effort hint: `medium`

Dependencies: Cache-key flag alignment for mono CI and local configs; Stamp and BOSS_SHAKE cache-poisoning audit (mono)

Scope: in-scope

**7. Cross-host correctness soak script/docs (mono)**

Scope: Add a documented procedure (script under `tools/` or runbook steps) to build a fixed target set on two hosts against the shared cache and compare critical outputs / test results; record expected pass criteria. No new always-on CI gate required in v1 if the procedure is manual-operator runnable from a leased workspace.

Effort hint: `small`

Dependencies: Enable remote cache on one mono CI queue (read+write) with metrics

Scope: in-scope

### Deferred (future / not a v1 blocker)

**8. Linux remote execution experiment for CI-only targets**

Scope: If post-cache CI metrics still show insufficient Linux throughput, prototype `--remote_executor` for non-Apple mono targets on a single experimental queue with local fallback. Explicitly excludes Swift/macOS.

Effort hint: `large`

Dependencies: Cross-host correctness soak script/docs (mono)

Scope: deferred (future / not a v1 blocker) — only if cache-only metrics demand execution capacity

**9. Mac executor evaluation note (product decision support)**

Scope: Short design addendum comparing MacStadium/Mac mini agent hosts vs managed Mac RBE cores for residual darwin compile load after cache + distributed agents. Doc-only; no infra.

Effort hint: `small`

Dependencies: Enable remote cache on one mono CI queue (read+write) with metrics

Scope: deferred (future / not a v1 blocker) — revisit only if laptop still saturates after cache + agent distribution

### Operator decisions (not filable tasks)

- Choose and provision the cache backend (bazel-remote on existing CI host vs Hetzner vs BuildBuddy Personal).
- Place network path (LAN / Tailscale / public) and auth secrets.
- Purchase any hardware (Linux box, extra Mac for agents).
- Decide whether flunge and mono share one cache instance name or two.

### Parallelism summary

```text
Depth 0:  [1 mono config] [2 flunge config] [3 flunge native audit]
Depth 1:  [4 flag alignment] [5 stamp audit]   # after 1; prefer 4→5 if same files
Depth 2:  [6 CI enable + metrics] → [7 soak]
Deferred: [8 Linux RBE experiment] [9 Mac executor note]
```
