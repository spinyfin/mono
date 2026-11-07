# Eliminating the cold-start cost of checkleft's bundled WASM checks

**Status:** investigation / design writeup (no code changed)
**Date:** 2026-06-16
**Scope:** the one-time WASM compilation cost paid by checkleft's *bundled* (first-party, in-binary) checks on a cold `.cwasm` cache. Evaluates two structural fixes — precompiling the target `.cwasm` into the release binary (Option A) and native-compiling the bundled checks (Option B) — plus a CI-cache stopgap, and recommends a direction.
**Related:**
[`checkleft-checks-ci-timing.md`](checkleft-checks-ci-timing.md) (the broader `checks` CI step; clippy-via-bazel is the dominant cost there),
[`checkleft-lib-test-wasm-compile-timeout.md`](checkleft-lib-test-wasm-compile-timeout.md) (the same compile cost hurting the *test* target, and the precompile machinery that already exists to fix it).

## TL;DR

A bundled check's only cold-start cost is **Cranelift AOT-compiling one WebAssembly component** (`Engine::precompile_component`). The component bytes are already `include_bytes!`-embedded in the binary — *nothing is downloaded at runtime for bundled checks* — so the fix is to stop compiling at runtime, not to cache a download.

There are two clean ways to do that, and one band-aid:

- **Option A — embed the precompiled `.cwasm` in the release binary.** Lowest engineering cost: most of the machinery already exists (`precompile_into_cache_dir`, the `precompile_cwasm` host tool, the `precompiled_cwasm_dir` Bazel rule — built for the test target). But it permanently bloats each binary by ~6–12 MiB, leaves a `wasmtime`-version + target-ISA coupling that must be re-pinned on every `wasmtime` bump, and is awkward for the two *cross-compiled* release targets (`x86_64-apple-darwin`, `x86_64-unknown-linux-musl`).
- **Option B — native-compile the bundled checks; keep WASM only for external/third-party extensibility.** Higher one-time engineering cost (a native invocation path through the check SDK), but the best steady state: zero compile, zero deserialize, *smaller* binary (native code ≪ a 6–12 MiB `.cwasm`), and no `wasmtime`-version coupling for the bundled path. This is also exactly what the operator's insight argues for — portable WASM buys nothing for a platform-specific bundled set; WASM earns its keep only for external checks.
- **Stopgap — cache `~/.cache/checkleft` in CI.** Cheap, removes the 9–10 s from the sandbox-CI lane today, but is CI-only, adds ~6–12 MiB of cache I/O per run, and does nothing for a developer's (or an external repo's) first local run.

**Recommendation:** apply the **CI-cache stopgap now** to relieve the sandbox-CI pain this week, and commit to **Option B** as the destination. Use **Option A** only as a fallback interim if B's SDK refactor can't land this cycle — and if so, do A for the two *native* Bazel targets only, leaving the cross targets on today's JIT+cache path.

---

## 1. Current architecture

### 1.1 What "bundled checks" actually are

checkleft ships a first-party check set compiled directly into the binary so a target repo with *no* checkleft files on disk still gets checks (`src/external/bundled.rs:1-45`). The set splits two ways:

| Bundled kind | Count | How it runs | Cold cost |
|---|---|---|---|
| **Declarative** (`format/bazel`, `format/rust`, `lint/rust`, `lint/bazel`) | 4 | YAML manifest, `include_str!`-embedded; the framework shells out to `rustfmt` / `clippy` / `buildifier` | none of *our* concern here — cost is the external tool (clippy dominates; see the CI-timing doc) |
| **WASM component** (`file/forbidden-path`, `file/size`, `file/ifchange`, `md/link-integrity`, `rust/giant-structs`, `rust/giant-structs-create`) | 6 checks in **1** component | a single multiplexed Component-Model module, `include_bytes!`-embedded; run in-process via Wasmtime | **`Engine::precompile_component` — the subject of this doc** |

All six WASM checks are served by **one consolidated component** (`checkleft_preinstalled_wasm_bundle::WASM`, `preinstalled-bundle/bundled.rs:7`), so the shared runtime baseline (WASI-p2 + Component-Model glue + guest SDK + `wit-bindgen`) and heavy shared deps (`syn`, `globset`, `serde`) are linked and compiled **once**, not once per check. The host dispatches to the right check by name via the component's `list-checks` / `run-check` exports (`src/external/bundled.rs:121-147`).

### 1.2 How a bundled WASM check resolves and runs

1. **Resolution.** `main.rs` builds a `CompositeExternalCheckPackageProvider` (the `providers=N` log line) with, in order: a `"file"` provider (on-disk manifests), a `"bundled"` provider (the embedded set), and conditionally a `"generated-index"` provider (`main.rs:499-542`). A bundled name resolves in `BundledExternalCheckPackageProvider` to a package whose `artifact_bytes: Some(&'static [u8])` points at the embedded component (`src/external/bundled.rs:200-225`).
2. **Load-or-compile.** `DefaultExternalCheckExecutor` computes the SHA-256 of the bytes and calls `ComponentAotCache::load_or_compile` (`src/external/runtime.rs:407-418`). On a **hit** it `Component::deserialize_file`s the cached `.cwasm` (low-ms); on a **miss** it `Engine::precompile_component`s (one full Cranelift compile), writes the `.cwasm` atomically, and returns the component (`src/external/runtime/cwasm_cache.rs:134-198`).
3. **Cache location & key.** Default `~/Library/Caches/checkleft/cwasm` (macOS) / `$XDG_CACHE_HOME/checkleft/cwasm` (Linux), overridable via `CHECKLEFT_CWASM_CACHE_DIR`. The entry filename is `SHA256(artifact_sha256 | wasmtime_version | engine_config_key | target_triple).cwasm` (`cwasm_cache.rs:237-249`). Any drift on those four axes is a clean miss, never a wrong-artifact load.
4. **No cache → JIT every run.** If the cache dir can't be opened, `component_cache` is `None` and every invocation JIT-compiles via `Component::new` (`runtime.rs:298-305, 413-417`).

### 1.3 The measured cost

Numbers from local profiling and the two related investigations (release build unless noted; `wasmtime` pinned at **42.0.2**):

| Path | Cost | Source |
|---|---|---|
| **Warm** run (`.cwasm` hit → deserialize + instantiate) | **~0.3 s** total checks ("0 s" checks); the deserialize itself is ~13–20 ms per component | task profiling; lib-test doc §1 (giant-structs warm = 13 ms release) |
| **Cold** run, arm64 macOS (compile the consolidated component) | **~2 s** | task profiling |
| **Cold** run, x86_64 CI runner | **~9–10 s**, and has been since the first sandbox-CI run | task profiling |
| Per-component cold compile, release arm64 | `giant-structs` 720 ms, `file/size` 1310 ms, SDK baseline 155 ms | lib-test doc §1 |
| Per-component cold compile, **debug** arm64 | `giant-structs` ~12 200 ms (16.9× the release figure) | lib-test doc §1 |

Three facts shape every option below:

- **It is compute, not I/O.** `precompile_component` ≈ `Component::new` ≈ one full Cranelift compile. The bytes are already in the binary; there is no download to cache for bundled checks (see §1.4). The x86-CI 9–10 s is a slower CPU paying the same Cranelift bill, plus a cold `$HOME` cache every run.
- **`syn` dominates.** ~80 % of the `giant-structs` compile is `syn` monomorphization; the WASI-p2 + Component-Model + SDK baseline alone is ~155 ms release / ~2.5 s debug.
- **The on-disk cache only helps a host whose `$HOME` persists.** Persistent Buildkite agents keep `~/.cache/checkleft` across builds, so they pay the compile only after a `wasmtime` bump, a bundle-bytes change, or the 90-day eviction. The **sandbox-CI lane that shows 9–10 s every run is using an ephemeral `$HOME`** (fresh container/runner), so it cold-misses on every invocation. That is the lane in pain.

### 1.4 Correction: there is no runtime download for bundled checks

The brief describes "checkleft re-downloads + recompiles every bundled WASM check … `~/.cache/checkleft/<version>` for the .wasm." That conflates two different things, and the distinction matters for the analysis:

- **Bundled component bytes are embedded**, not downloaded — `include_bytes!("preinstalled_bundle_component.wasm")` (`preinstalled-bundle/bundled.rs:7`). `src/install.rs` only writes a git `pre-push` hook; it fetches nothing.
- The `~/.cache/checkleft/<version>` directory an external consumer sees is the **prebuilt checkleft *binary*** downloaded from the GitHub Release (the `.wasm` lives *inside* that binary). "Downloads the modules" = downloads the binary once per version; that is amortized and orthogonal to the per-run compile.

So for bundled checks the **entire** avoidable cold cost is the Cranelift compile. Both Option A and Option B target exactly that, and neither needs to touch a download path.

---

## 2. Option A — materialize the target `.cwasm` into the release binary

Precompile the consolidated component to a Wasmtime `.cwasm` *for the exact release target* at release-build time and ship it, so first run deserializes instead of compiling.

### 2.1 What already exists (this is the cheap part)

The test-timeout investigation already built almost all of this for the *test* target:

- `precompile_into_cache_dir(cache_dir, bytes)` — precompiles using the **same** `build_wasmtime_engine()` config the runtime uses, writing under the canonical cache-key filename (`cwasm_cache.rs:296-306`).
- `precompile_cwasm.rs` — a host build tool wrapping the above (`tools/checkleft/precompile_cwasm.rs`).
- `precompiled_cwasm_dir` Bazel rule — runs that tool in the **exec (host) configuration** over a set of `rust_wasm_component` targets, emitting a TreeArtifact of `.cwasm` files keyed by content (`tools/checkleft/wasm/defs.bzl:124-191`). Today it feeds `checkleft_lib_test` as a data fixture.

Extending this to the *release binary* means: produce the target `.cwasm`, get it into the binary (or alongside it), and teach the bundled path to deserialize it.

### 2.2 The hard parts

**`.cwasm` is coupled to (wasmtime version × engine config × target ISA/CPU features).** A `.cwasm` is not portable. Wasmtime stamps a compatibility header (version + compile flags + ISA) and `deserialize` refuses a mismatch. The cache key already encodes `wasmtime_version`, `engine_config_key`, and `target_triple` (`cwasm_cache.rs:237-249`), and `CHECKLEFT_WASMTIME_VERSION` is an `IFCHANGE`-pinned constant kept in lockstep with `Cargo.toml` (`BUILD.bazel:29-37`). Embedding `.cwasm` makes this coupling a *release-artifact* invariant: **every `wasmtime` bump must regenerate every embedded `.cwasm`**, or the binary ships a blob it cannot deserialize. The pin already exists; A raises the cost of getting it wrong from "a cache miss" (silent JIT fallback) to "a shipped, broken artifact" unless a fallback is retained (see §2.4).

**Cross-compilation for the two cross targets is the real obstacle.** Release builds split (release doc "How it works"):

| Target | How built | Host == target at build? |
|---|---|---|
| `aarch64-apple-darwin` | `bazel build -c opt //tools/checkleft:checkleft` on an arm64 mac | **yes** |
| `x86_64-unknown-linux-gnu` | `bazel build -c opt` on x86 Linux | **yes** |
| `x86_64-apple-darwin` | `cargo build --target …` on an arm64 mac | **no (cross)** |
| `x86_64-unknown-linux-musl` | `bazel build //tools/checkleft:checkleft_musl` (Zig CC cross) | **no (cross)** |

`precompile_component` compiles for the **host** ISA by default, and the cache key's target axis comes from `std::env::consts::{OS,ARCH}` of the *tool that ran* (`cwasm_cache.rs:239`) — i.e. the build host, not the release target. So the existing exec-config rule produces a *host*-keyed, *host*-ISA `.cwasm`:

- For the two **native** targets that is exactly right (host == target).
- For the two **cross** targets it is wrong twice over: wrong key *and* wrong machine code. To make a cross-target `.cwasm` you must build the engine with `Config::target("x86_64")` (Wasmtime supports cross-AOT) **and** match the runtime CPU-feature flag set the target binary will use natively, or `deserialize` on the target rejects the header. Getting the flag set to agree across a `Config::target()` build and the target's *native-default* engine is the fiddly, fragile core of A.

**`Component::deserialize` is `unsafe`.** It trusts the bytes were produced by a compatible engine; a mismatch is *checked* (returns `Err`) rather than UB, but the contract is still "you produced this." For a blob your own release pipeline produced for this exact target, that trust holds — the existing cache already relies on it (`cwasm_cache.rs:163-171`).

### 2.3 Binary-size impact

`.cwasm` is markedly larger than the source `.wasm` (lib-test doc §2): `giant-structs` 5.05 MiB `.wasm` → 6.53 MiB `.cwasm`; `file/size` 6.12 MiB → 11.72 MiB. The consolidated bundle's `.cwasm` is not separately measured here but is on the order of **~12–16 MiB**, versus the single embedded `.wasm` today (~6–8 MiB). If we ship `.cwasm` *instead of* `.wasm` for the bundled path, the net delta is roughly **+6–10 MiB per release binary** (and each per-target binary carries its own — fine, they are platform-specific anyway). If we ship `.cwasm` *in addition to* `.wasm` (for fallback), it is the full `.cwasm` size on top.

### 2.4 Embed via `include_bytes!` vs ship-alongside

- **`include_bytes!` the `.cwasm`, deserialize directly, bypass the disk cache for bundled checks.** Cleanest: the bundled path becomes `Component::deserialize(engine, EMBEDDED_CWASM)` with no key computation and no `~/.cache` round-trip — the on-disk cwasm cache becomes *dead code for the bundled set* (it stays only for external checks). Risk: if `deserialize` ever fails (a botched cross-build, a missed `wasmtime` re-pin), the check is dead unless a fallback exists. Safe form: keep the `.wasm` embedded too and fall back to `precompile_component` on deserialize error — at the cost of carrying both blobs.
- **Ship the `.cwasm` alongside the binary (in the release tarball).** Avoids bloating the binary image, but checkleft is distributed as a *single* prebuilt binary (one asset per triple, per the release doc); a sidecar file breaks that single-artifact contract and reintroduces a "where is my cwasm" lookup. Not worth it here.

### 2.5 musl / static-link considerations

The musl target is already a hermetic Zig-CC cross-build (`checkleft_musl`, `musl/BUILD.bazel`, `BUILD.bazel:101-114`) and is **best-effort** in the pipeline (skipped with a warning if tooling is absent — release doc step 7). A `.cwasm` is pure data (`include_bytes!`), so it adds no link-time/static-linking complexity itself; the difficulty is purely the cross-AOT-for-x86-musl-target problem from §2.2. A static binary that embeds its own `.cwasm` and deserializes it needs no writable cache dir at all — a genuine plus for locked-down/static deployments.

### 2.6 How A obsoletes the on-disk cache for bundled checks

With embedded-and-deserialized `.cwasm`, the bundled path never touches `~/.cache/checkleft/cwasm` — no key compute, no read, no write, no eviction sweep. The disk cache remains only for **external** checks (which are genuinely portable `.wasm` resolved at runtime and must still be compiled on first use). That is a clean separation and removes the "ephemeral `$HOME` → cold every run" failure mode for the bundled set entirely.

### 2.7 Net assessment of A

A is the **low-engineering-effort, permanent-tax** option: it reuses existing machinery and kills the cold start for the two native targets almost for free, but it (a) bloats every binary by ~6–10 MiB, (b) still pays ~13–20 ms deserialize + instantiation per component at runtime, (c) couples every release to the `wasmtime` version with a "broken artifact" failure mode unless a fallback blob is also carried, and (d) needs real cross-AOT work to cover the two cross-compiled targets. It does not advance the architecture — it optimizes the WASM path rather than questioning whether bundled checks should be WASM at all.

---

## 3. Option B — native-compile the bundled checks; WASM for external only

Make the bundled checks plain native Rust, invoked directly with no Wasmtime in the loop. Keep Wasmtime solely for external/third-party checks.

This is the option the operator's key insight points at: a release binary is platform-specific, so the *portability* WASM provides is worthless for the bundled set; WASM was adopted for the bundled checks mainly to prove the component path works, and its real value is **external extensibility**.

### 3.1 Single source of truth for check logic

The bundled check logic already lives as ordinary Rust crates under `tools/checkleft/checks/<ns>/<name>/`, authored against the SDK (`#[check]`), and is *compiled to `wasm32-wasip2`* for the bundle (`src/external/bundled.rs:18-32`). The same crates can compile **natively** and be called in-process. The SDK macro (`sdk-macro/`) would grow a native invocation shim alongside the existing `export_checks!` WASM-export path, so one `#[check]` function body serves both targets — no logic fork.

The parity question — "if bundled checks stop being WASM, how do we keep the WASM host path tested?" — does **not** require dual-compiling all six checks forever. The host/extensibility path only needs **one** representative check compiled to WASM as a test fixture (plus the existing external-check integration tests). Concretely:

- **Production bundled path:** native call, no Wasmtime.
- **WASM host-path coverage:** keep (at least) the `giant-structs` crate compiled to `wasm32-wasip2` as a *test fixture* and keep running it through `DefaultExternalCheckExecutor` in `checkleft_lib_test`, exactly as today. This proves `list-checks` / `run-check` / the sandbox / WASI lowering still work, using the same source crate — so native and WASM provably agree on at least one non-trivial check.

That makes "single source of truth" real: the check is authored once; production runs it native; tests additionally run it as WASM to guard the contract.

### 3.2 Always-native vs a bypass-mode flag

Two shapes:

- **Always-native (recommended).** Bundled checks are simply native functions; there is no WASM artifact for them in the binary at all. Smallest binary, simplest mental model, zero compile/deserialize.
- **Bypass-mode flag.** Keep the WASM bundle and add a flag/env to run bundled checks natively as a fast path. This hedges (you can A/B native vs WASM), but it carries *both* implementations and their full size, and invites drift. Only worth it as a transition aid, not as a destination.

### 3.3 Wasmtime stays — for external checks

B does **not** remove the `wasmtime` dependency or its binary-size contribution. External/third-party checks are still portable `.wasm` loaded and compiled at runtime, so the engine, the `cranelift` feature, the WASI linker, and the on-disk `.cwasm` cache all remain for that path (`Cargo.toml:73-74` features `component-model`, `cranelift`, `runtime`). What B removes is paying that path's *compile cost* for the *bundled* set on every cold run. (External checks were never the reported pain — a repo using them opts in and the cwasm cache covers their warm runs.)

### 3.4 Testing parity between native and WASM paths

The risk in B is behavioral drift between "the native function" and "the WASM component." Mitigations:

- One shared source crate per check (§3.1) — there is no second implementation to drift.
- A golden-output parity test on the representative check: run the same changeset through the native path and the WASM-fixture path and assert identical findings. Cheap, and it pins the contract.
- The native path loses the **WASI capability sandbox** (the FS preopen scoping in `runtime.rs:124-134, 788-820`). For *first-party, trusted* checks this is acceptable — they are our code, shipped in our binary — but it is a real semantic change worth stating explicitly: a bundled check running native has normal process FS access, where today it sees only a sandbox of the changeset. Bundled checks already run with the framework's trust; external checks (the ones the sandbox is actually defending against) keep it.

### 3.5 Net assessment of B

B is the **higher one-time effort, best steady state** option: a native invocation path through the SDK is more work than wiring an existing Bazel rule, but the result is *no compile, no deserialize, no `wasmtime` version coupling for bundled, and a smaller binary* (native code is far less than a 6–12 MiB `.cwasm`). It preserves WASM precisely where it has value (external extensibility) and removes it where it only cost cold-start time. checkleft is `0.1.0-alpha`; the cost of the refactor is lowest now and the bundled-check surface is small (six checks, one component).

---

## 4. Hybrid options

- **A-native-targets-only (a real, shippable subset of A).** Embed `.cwasm` for `aarch64-apple-darwin` and `x86_64-unknown-linux-gnu` (host == target, so the existing exec-config rule Just Works) and leave the two cross targets on today's JIT+cache path. Covers the dominant developer (arm64 mac) and CI (x86 Linux) surfaces while sidestepping the cross-AOT problem entirely. This is the most defensible *interim* if B slips.
- **B for bundled + A's cache machinery for external.** This is essentially the recommended end state: bundled goes native (B); the `.cwasm` disk cache and the precompile tooling stay to serve *external* checks. Nothing about B throws away the cwasm cache — it just stops the bundled set from depending on it.
- **A now, B later.** Ship A as a quick win, then do B and delete the embedded `.cwasm`. Defensible, but it means building, testing, and maintaining the cross-AOT pipeline for a path you intend to remove — only worth it if A's native-target subset is genuinely needed before B can land.

---

## 5. The CI-cache stopgap

Cache `~/.cache/checkleft` (Linux) / `~/Library/Caches/checkleft/cwasm` (macOS) in the sandbox-CI lane (e.g. `actions/cache`, or the Buildkite equivalent), keyed on the pinned checkleft version.

**Does it address the pain?** For the specific lane that shows 9–10 s every run (ephemeral `$HOME`), **yes** — a warm `.cwasm` turns the cold compile into a ~13–20 ms deserialize. Key it on the checkleft version (which transitively pins `wasmtime` version + bundle bytes); the in-file cache key already self-invalidates on the four real axes, so a coarse version key is safe (a stale entry just misses).

**What it does not fix, and its costs:**

- **CI-only.** A developer's first local run in a fresh environment, and *every external repo's* first run, still pay the full compile. The in-binary options (A/B) fix cold start *everywhere*.
- **~6–12 MiB of cache I/O per run** (upload/download of the `.cwasm`), plus cache-key maintenance and the usual cache-poisoning/eviction footguns.
- **It treats the symptom.** The component is still compiled somewhere, sometime; we are just memoizing it across CI runs.

**Verdict:** worth doing **immediately** as a band-aid for the sandbox-CI lane — it is hours of work and removes the most visible pain today — but it is not a substitute for A or B. It buys time to do B properly.

---

## 6. Comparison

| Axis | A: embed `.cwasm` | B: native bundled | Stopgap: CI cache |
|---|---|---|---|
| Cold-start fixed where? | everywhere (per shipped target) | **everywhere** | CI lane only |
| Runtime cost after fix | ~13–20 ms deserialize + instantiate | **~0 (direct call)** | ~13–20 ms deserialize |
| Binary size | **+6–10 MiB / target** (or more with fallback) | **smaller** (drops bundled `.wasm`) | unchanged |
| `wasmtime` version coupling (bundled) | **tight** — re-pin + regenerate every bump or ship a broken blob | **none** for bundled | loose (coarse key) |
| Cross-target story | **hard** for the 2 cross targets (cross-AOT + flag match) | n/a (native per target already) | n/a |
| Release-pipeline complexity | new precompile-into-binary step + cross-AOT | SDK native shim; *simpler* release | new CI cache step |
| Preserves WASM for external checks | yes (cache stays for external) | **yes** (engine stays for external) | yes |
| Engineering effort | **low** (machinery exists) for native targets; medium with cross | **medium–high** (SDK + parity) | **lowest** |
| Maintenance burden | permanent (size + version pin + cross-AOT) | one-time refactor, then clean | ongoing CI cache hygiene |
| Sandbox semantics (bundled) | unchanged (WASI sandbox) | bundled lose WASI sandbox (trusted code) | unchanged |

---

## 7. Recommendation

**Do both, in sequence:**

1. **Now — CI-cache stopgap.** Add a cache of `~/.cache/checkleft` (and the macOS path) to the sandbox-CI lane, keyed on the checkleft version. Removes the 9–10 s from that lane this week at near-zero risk. Explicitly label it a stopgap in the pipeline comment so it is not mistaken for the fix.

2. **Destination — Option B (native-compile the bundled checks).** It is the only option that makes cold start *and* binary size *and* `wasmtime`-version coupling all go away for the bundled set, and it matches the operator's insight: WASM's value is external extensibility, which B fully preserves. The bundled surface is small (six checks, one component) and checkleft is pre-1.0, so the refactor is as cheap as it will ever be.

3. **Fallback interim — Option A, native targets only.** If B's SDK work cannot land this cycle, embed the `.cwasm` for `aarch64-apple-darwin` and `x86_64-unknown-linux-gnu` only (host == target; the existing `precompiled_cwasm_dir` rule already produces exactly the right artifact), leaving the cross targets on the JIT+cache path. Treat it as throwaway scaffolding to be deleted when B lands — do **not** invest in cross-AOT for a path you intend to remove.

The order matters: the stopgap buys time, B is the goal, and A is only the hedge.

### Rough implementation sketch

**Stopgap (hours):**
- In the sandbox-CI workflow, add a cache step on `~/.cache/checkleft` keyed on the resolved checkleft version (e.g. the `checkleft-v*` tag or the pinned version string). No code change in this repo.

**Option B (the real work):**
- **SDK:** add a native invocation path to the check SDK so a `#[check]` function can be called in-process. The macro already generates the WASM export glue (`sdk-macro/`, `export_checks!`); add a parallel native registration so the host can look up and call the function directly.
- **Host:** add a `NativeBundledCheckProvider` (or extend `BundledExternalCheckPackageProvider`) that resolves the six bundled names to native function handles instead of `artifact_bytes`, and a native execution arm in the executor that skips Wasmtime entirely. Map the existing `CheckInput` / `Finding` types directly (they already exist host-side in `runtime.rs` lowering/lifting).
- **Drop** `checkleft_preinstalled_wasm_bundle::WASM` from the binary (no more `include_bytes!` of the bundle). Keep the check crates building to `wasm32-wasip2` **only** as test fixtures.
- **Tests:** keep one representative check (e.g. `giant-structs`) running through the WASM host in `checkleft_lib_test`, and add a parity test asserting native and WASM produce identical findings on the same changeset. Leave the external-check integration tests untouched — they already cover the runtime-compile path.
- **Sandbox note:** document that bundled checks now run with normal process FS access (trusted first-party code); the WASI capability sandbox remains the enforcement boundary for external checks only.

**Option A, native-targets subset (only if used as the interim):**
- Add a `cwasm` output of `//tools/checkleft:checkleft`'s build that runs the existing `precompile_cwasm` tool over the consolidated bundle in the **target == host** Bazel builds, and `include_bytes!` the result into the binary behind a build feature.
- Switch the bundled path to `Component::deserialize(engine, EMBEDDED_CWASM)`; keep the embedded `.wasm` + `precompile_component` as a fallback on deserialize error (accepting the extra size) until trust in the cross-build is established.
- Leave `x86_64-apple-darwin` and `x86_64-unknown-linux-musl` (the `cargo`/Zig cross builds) without an embedded `.cwasm` — they keep today's JIT+cache behavior.
- Re-generation of the embedded `.cwasm` is automatic from the `IFCHANGE(wasmtime-version)` pin; verify the build fails loudly (not silently ships a stale blob) if the pin and the resolved `wasmtime` disagree.

---

## 8. Open questions / follow-ups for the operator

- **Confirm the 9–10 s lane is ephemeral-`$HOME`.** If the sandbox-CI runner actually persists `$HOME` between runs, the stopgap is a config nudge, not a new cache step — and the real cold cost is only on agent recycle / version bump. Worth a one-line check of the runner config.
- **Sandbox boundary for bundled checks under B.** Is dropping the WASI capability sandbox for *first-party* checks acceptable (they are our trusted code), or is there a policy reason to keep every check sandboxed regardless of origin? This is a security-posture call for the operator.
- **Measure the consolidated `.cwasm` size** before committing to A's interim — the ~12–16 MiB estimate is extrapolated from the pre-consolidation per-component figures (lib-test doc §2), not measured on the consolidated bundle.
- **Cross-AOT appetite.** If A (not the native subset) is ever wanted for *all* targets, someone must validate `Config::target()` + CPU-feature-flag matching for `x86_64-apple-darwin` and `x86_64-unknown-linux-musl` against those targets' native-default engines. This is the fragile part; confirm there is appetite before building it.

## Follow-up code work (out of scope here — file separately)

This investigation changed no code. The concrete code/pipeline items implied above, to be filed as separate work:

- **CI:** add the `~/.cache/checkleft` cache step to the sandbox-CI lane (stopgap).
- **SDK + host (Option B):** native invocation path in the check SDK; native bundled provider + executor arm; drop the embedded bundle `.wasm`; keep one WASM test fixture + add a native/WASM parity test.
- **(Only if B slips) Option A interim:** embed `.cwasm` for the two native Bazel targets behind a build feature, with a `.wasm` fallback, leaving cross targets on JIT+cache.
