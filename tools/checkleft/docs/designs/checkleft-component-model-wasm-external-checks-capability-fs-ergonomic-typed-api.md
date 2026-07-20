# Checkleft: Component-Model wasm external checks (capability FS + ergonomic typed API)

- **Status:** shipped — all eleven implementation tasks (T1–T11) merged; this document was updated post-implementation (2026-07-20) to record as-built reality.
- **Project:** P1446 / `proj_18b65280ce1e7948_2bd`
- **Shipped in:** PRs [#1413](https://github.com/spinyfin/mono/pull/1413) (T1), [#1421](https://github.com/spinyfin/mono/pull/1421) (T2), [#1416](https://github.com/spinyfin/mono/pull/1416) (T3), [#1414](https://github.com/spinyfin/mono/pull/1414) (T3a), [#1424](https://github.com/spinyfin/mono/pull/1424) (T4), [#1425](https://github.com/spinyfin/mono/pull/1425) (T5), [#1422](https://github.com/spinyfin/mono/pull/1422) (T6), [#1415](https://github.com/spinyfin/mono/pull/1415) (T7), [#1423](https://github.com/spinyfin/mono/pull/1423) (T8), [#1430](https://github.com/spinyfin/mono/pull/1430) (T9), [#1459](https://github.com/spinyfin/mono/pull/1459) (T10), [#1460](https://github.com/spinyfin/mono/pull/1460) (T11).

## Overview

Checkleft runs three kinds of checks: built-in (compiled into the binary), declarative (`declarative-v1`: framework-owned binary invocations + declarative transforms), and — as of this project — **Component-Model wasm** (`component-v1`). Most checks live **out-of-tree** — independently versioned, authored by teams that do not ship inside the checkleft binary — and the wasm tier gives them sandboxed, typed authoring.

This project replaced the previous `sandbox-v1` wasm runtime — a hand-rolled CORE-wasm ABI in which authors wrote raw pointer/length code, managed linear-memory buffers by hand, could not read any files, and ran under a fixed tight fuel budget — with a runtime built on the **WebAssembly Component Model**. The two requirements the rewrite delivered:

1. **Ergonomic typed authoring** — an author writes `fn check(input: CheckInput) -> Vec<Finding>` against real Rust structs. No pointers, no manual `memory.grow`/offset math. All marshalling is generated glue (`wit-bindgen` on the guest, `wasmtime::component::bindgen!` on the host).
2. **Capability-scoped file access** — checks read files with ordinary `std::fs`, but the **host** decides and enforces exactly which paths each invocation may read. Deny-by-default, finer than whole-FS, enforced structurally by what the host materialises into a per-invocation sandbox directory preopened as the WASI root.

The fresh-slate rewrite the design recommended is what shipped: `sandbox-v1` was deleted outright in T11 (#1460) with no parallel dual-tier period.

## Goals — all delivered

- Out-of-tree checks are authored as plain typed Rust functions over generated native structs — no raw pointers, no manual linear-memory management. The guest SDK (`checkleft-check-sdk`, T2) hides the ABI entirely.
- The host grants each check invocation a capability-scoped, deny-by-default set of readable files, enforced host-side and finer than whole-filesystem (T3a + T4).
- One artifact can export many checks (one component, N checks), self-describing via `list-checks` (T8). This is now exercised for real: all preinstalled wasm checks ship in a single multiplexed component (`checkleft_preinstalled_wasm_bundle`), grown after this project to six checks.
- Timeouts and memory are policy knobs with generous defaults and per-check / per-bundle overrides, clamped by a host ceiling (T5) — not the prototype's fixed, tight fuel ceiling.
- Checks run in-process (no per-invocation process spawn), AOT-compiled and cached on disk as `.cwasm` (T6).
- The existing executor/provider/runner architecture, sha256 artifact pinning, and CHECKS resolution semantics were preserved and reworked, not thrown away (T7, T8).
- The end-to-end proof: `rust-giant-structs-use-builder` re-authored on the SDK, built via the `rust_wasm_component` bazel rule, bundled, resolved, and executed through the new host with capability-scoped file reads (T10).
- **Bazel is the build path — for both the runtime and every check.** The guest pipeline (`wasm32-wasip2` cross-compilation, componentization, sha256 emission) runs entirely under bazel via new toolchain registrations and a custom `rust_wasm_component` rule (T9). No cargo-only or shell-script build path exists.

## Non-goals — held

- **Multi-language guest authoring.** Rust-only shipped, preserving the T1371/PR 1372 decision. The WIT contract does not preclude other guest languages later; no non-Rust SDK exists.
- **Replacing the declarative (`declarative-v1`) tier.** It still owns the "run an arbitrary binary" use case (e.g. `buildifier`). Only the wasm tier was re-architected.
- **Network access from checks.** No host-imported network capability exists; checks are pure-compute plus host-mediated file reads.
- **Write access to the working tree from checks.** This held, and later work built on it in the intended direction: the contract has since grown a `fix-check` export (post-project, for `checkleft fix`) in which the guest _returns_ `file-edit` records from its read-only sandbox and the **host** validates and applies them — checks still never mutate files directly.
- **A general OS-process sandbox for wasm guests.** Wasm isolation plus the host capability boundary is the sandbox; no seccomp/Landlock/sandbox-exec was added.
- **Remote/registry distribution of components** beyond the existing external-URL provider path.

## Audit of the former `sandbox-v1` runtime (removed by T11, #1460)

This audit motivated the rewrite and is preserved as the record of what was replaced. Source (as of the audit): `tools/checkleft/src/external/runtime.rs`, `mod.rs`, `command_policy.rs`, `bundled.rs`, `provider.rs`, and `runtime/tests.rs`.

### How it worked

- **Two execution paths in one function.** `DefaultExternalCheckExecutor::execute_artifact` first tried a CORE-wasm module path (`execute_core_artifact`); on an `ArtifactMismatch` it fell back to a "component" path (`execute_component_artifact`).
- **The core path was a hand-rolled ABI.** The module had to export `memory` and `checkleft_run: (i32, i32) -> i64`. The host serialized `{changeset, config, capabilities}` to JSON, wrote it into the guest's linear memory at offset 0 (growing memory by hand), called `checkleft_run(offset, len)`, and decoded the `i64` return as a packed `(offset << 32) | len` pointing at the output JSON. The guest allocated that output buffer and packed the pointer.
- **The "component" path was not a real Component-Model interface.** It expected an export `run: (string) -> (string)` and passed the same JSON string both ways — the core ABI's manual JSON marshalling wearing a component costume, with no WIT records and no typed lifting/lowering.
- **Limits.** Fuel only, fixed at 10,000,000, set per store. No epoch deadline, no wall-clock timeout, no memory cap, no per-check override.
- **No AOT / no cache.** `Module::new` / `Component::new` recompiled the artifact from bytes on every invocation.
- **Capabilities were vestigial for wasm.** The manifest `capabilities.commands` list was intersected with a global ceiling in `command_policy.rs` and serialized into the input JSON — but the guest was pure wasm with no host imports, so the grant was plumbed but inert.
- **sha256 pinning worked** (`validate_artifact_sha256`) and survived into the new runtime unchanged.
- **One artifact = one check.** No bundle / `list-checks` concept.

### Gap list against the two hard requirements

| Requirement                   | `sandbox-v1` state                                                                          | Resolution in `component-v1`                                                                           |
| ----------------------------- | ------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| Ergonomic typed authoring     | Guest hand-wrote `checkleft_run(i32,i32)->i64` or `run(string)->(string)` with manual JSON. | WIT records + generated glue on both sides; authors see only native structs (T1, T2).                  |
| Capability-scoped file access | Checks could not read files at all; no WASI, no host imports.                               | `wasm32-wasip2` + WASI-p2 preopen of a host-built sandbox dir; guests use plain `std::fs` (T3a, T4).   |
| Limits as policy              | Fixed fuel budget, no overrides.                                                            | Epoch wall-clock deadline + memory `ResourceLimiter`, manifest overrides clamped by host ceiling (T5). |
| Performance                   | Recompiled every invocation.                                                                | On-disk `.cwasm` AOT cache keyed by artifact + wasmtime version + engine config (T6).                  |
| Bundling/discovery            | One artifact, one check.                                                                    | `list-checks` self-description; one component resolves to N logical packages (T8).                     |
| Capability surface clarity    | Inert `capabilities.commands` plumbed into wasm input.                                      | Removed from the wasm tier (T7 removed the schema surface; T11 deleted `command_policy.rs`).           |

## Alternatives considered

### A. Evolve the bespoke core-wasm ABI

Add host-imported file-read functions to the existing `checkleft_run` core ABI and hand-roll a codegen layer marshalling typed structs across the `i32`/`i64` buffer boundary.

Rejected: this reinvents, by hand and worse, exactly what the Component Model's Canonical ABI already standardizes. Every new field on `CheckInput`/`Finding` becomes manual marshalling work on both sides, with no typed contract artifact to keep guest and host in agreement.

### B. exec-external (binary + JSON-over-stdio, OS-sandboxed)

Rejected **as the wasm-tier replacement**, but kept for what it is good at: the exec model already exists as the declarative tier, which owns "wrap an existing binary like buildifier." For the typed-Rust-check sweet spot it loses on per-invocation process spawn cost, OS-sandbox portability (a deny-by-default file capability that behaves identically on macOS and Linux CI is genuinely hard), and typed ergonomics (authors hand-roll JSON in every language).

### C. Native dynamic plugins (`cdylib` + `dlopen`)

Rejected: zero sandboxing (defeats requirement #2 entirely), fragile ABI across compiler/std versions, `unsafe` loading of untrusted out-of-tree code.

### D. WebAssembly Component Model — **chosen and shipped**

WIT-defined contract, `wit-bindgen` guest SDK, `wasmtime::component::bindgen!` host, WASI-p2 capability-scoped file access, epoch/memory limits, `.cwasm` cache. Detailed below as built.

## As-built architecture

### At a glance

```
author writes:                 fn check(input: CheckInput) -> Vec<Finding>
                                      |  #[check(...)] + export_checks! (SDK macros; wit-bindgen glue)
guest crate (checkleft-check-sdk) --> wasm32-wasip2 component (exports checkleft:check world)
                                      |  rust_wasm_component bazel rule -> .wasm + .wasm.sha256 sidecar
manifest (mode=component) / bundled def pins artifact + sha256 + limits
                                      |
host (checkleft) -- component::bindgen! --> component (from .cwasm AOT cache; JIT fallback)
   phase 1: instantiate with empty WASI ctx -> list-checks() -> read declared access-scope
   phase 2: create_sandbox(changeset, scope, source_tree, ceiling) -> temp dir
            re-instantiate with WasiCtxBuilder::preopened_dir(sandbox_root, "/", READ, READ)
            epoch deadline + memory ResourceLimiter on the store
            run-check(name, input) -> result<list<finding>, check-error>
```

Two details of this shape emerged during implementation rather than in the original design:

- **Two-phase instantiation (T4, #1424).** The check's declared `access-scope` lives in its `check-descriptor`, which is only obtainable by calling `list-checks` — so the host instantiates once with an empty WASI context for discovery, then re-instantiates with the scoped sandbox preopened for the actual run. Phase-1 stores get an effectively-infinite epoch deadline (`EPOCH_DEADLINE_NEVER`); phase-2 stores get the resolved policy timeout.
- **Synchronous executor on a blocking thread (T10, #1459).** `wasmtime-wasi` internally drives async WASI ops with `block_on`; calling the synchronous executor directly from an async tokio task panics ("cannot start a runtime from within a runtime"). The runner wraps executor calls in `tokio::task::spawn_blocking`.

### WIT contract (T1, #1413)

The contract lives at `tools/checkleft/wit/check.wit` as `package checkleft:check@0.1.0`, exported to bazel via `//tools/checkleft/wit`. The shipped v1 surface matched the design closely:

- `interface types` mirrors `input.rs`/`output.rs`: `change-kind`, `changed-file`, `file-line-delta`, `diff-hunk`, `file-diff`, `change-set`, `check-input`, `severity`, `location`, `file-edit`, `suggested-fix`, `finding`, plus `access-scope`, `check-descriptor`, and `check-error`. Every record carries a doc comment naming its host-side Rust counterpart.
- `world check` exports `list-checks: func() -> list<check-descriptor>` and `run-check: func(name: string, input: check-input) -> result<list<finding>, check-error>`.
- **`access-scope` declares the check's file-access appetite**: `modified-only` (the default when absent), `whole-repo` (explicit opt-in), or `globs(list<string>)` (targeted cross-file reads; changeset files always included).
- **Config crosses the boundary as `config-json`** — a JSON string the guest SDK deserializes with serde into the author's own `#[derive(Deserialize)]` config struct. Modeling arbitrary `toml::Value` config as WIT records was rejected as a v1 concern, and this pragmatic choice has held up.
- **File access is via `std::fs`, not a WIT import.** No checkleft-specific file API exists in the world; capability scoping is structural (see below).

The host-side `bindgen!` invocation (`src/external/component_bindings.rs`) reads `wit/check.wit` at compile time — this doubles as the smoke test that the WIT is valid, and it required adding the WIT file to `compile_data` on the bazel targets so the macro can see it inside the sandbox.

_Post-project evolution:_ the contract has since grown beyond this project's scope — a `fix-check` export with `fix-error` (host-applied edits for `checkleft fix`), a `declared-files` access-scope variant with a `declared-exclusion`/`exclusion-status` mechanism. Those are documented with their own projects; the v1 surface above is what this project delivered.

### Guest SDK (T2, #1421)

Shipped as **three crates** under `tools/checkleft/sdk/` rather than the single crate the design sketched:

- `checkleft-check-sdk` — native Rust types (`CheckInput`, `Finding`, `ChangeSet`, …) and the `config::<T>()` serde helper. Authors never touch WIT bindings.
- `checkleft-check-sdk-macro` — proc-macro crate providing `#[check(name = "...", description?, severity?, access_scope?)]` and **`export_checks!(fn1, fn2, ...)`**. The explicit `export_checks!` registration macro is a divergence from the design's implicit "the `#[check]` macro registers the function" sketch: `export_checks!` expands to a `wit_bindgen::generate!` call _in the author's crate_, which is what makes the component ABI export land in the right scope with no re-export issues. A consequence: check crates need `wit-bindgen` (0.51) as a direct dependency.
- `checkleft-trivial-check-example` — the trivial example check that proves the ergonomics and serves as the T9 CI smoke-test subject.

**WIT distribution diverged from the plan.** The design called for exposing `.wit` files to the compiler sandbox as bazel `data` on the guest SDK target. Instead, the macro crate embeds the WIT text at _proc-macro compile time_ via `include_str!` and emits it `inline:` into the generated `wit_bindgen::generate!` call. Guest builds therefore have no filesystem dependency on the WIT package at all — simpler and more hermetic than the `data`-attribute plan.

Author-facing shape, as shipped:

```rust
use checkleft_check_sdk::{CheckInput, Finding, Severity};
use checkleft_check_sdk_macro::{check, export_checks};

// No access_scope → modified-only by default: sandbox contains only changed files.
#[check(name = "rust-giant-structs-use-builder")]
fn run(input: CheckInput) -> Vec<Finding> {
    let cfg: MyConfig = input.config()?;              // serde over config-json
    let src = std::fs::read_to_string(&path)?;        // plain Rust; sandbox enforces access
    // ... pure analysis ...
}

export_checks!(run);
```

### Host runtime (T3 #1416, T4 #1424)

- `wasmtime::component::bindgen!` generates host-side bindings; the executor drives `call_list_checks` / `call_run_check` through them, lowering `ChangeSet` + `toml::Value` config into WIT types and lifting `finding` records (location, remediations, suggested fix) back into `output::Finding`.
- `Store<HostState>` replaced the old `Store<()>`. `HostState` carries the `WasiCtx` + `ResourceTable` (implementing `WasiView`) and the memory limiter, with constructors for the empty-WASI discovery phase and the sandboxed run phase.
- The linker is built with the full WASI preview-2 interface set via `wasmtime_wasi::p2::add_to_linker_sync` — this single `WasiCtx` covers file I/O plus the clocks/stdio interfaces `std`-using guests pull in, exactly as the design anticipated.
- The executor/provider/runner trait architecture (`ExternalCheckExecutor`, `ExternalCheckPackageProvider`, `CompositeExternalCheckPackageProvider`, the runner's scheduling) was preserved; the change concentrated in `runtime.rs` and the manifest schema, as planned.
- `wasmtime-wasi` is pinned at the workspace level alongside `wasmtime` (42.0.2).

One bazel-specific workaround surfaced in T10: adding a generated `.wasm` artifact to `checkleft_lib`'s `compile_data` flips rules_rust into symlink-sources mode, which shifts the compilation root and breaks `bindgen!(path: "wit/check.wit")`. Bundled component bytes therefore live in a dedicated micro-library (`include_bytes!` only) that `checkleft_lib` depends on, keeping the main library in source mode.

### File-capability model — shipped as designed

Checks read files with ordinary `std::fs`. The `wasm32-wasip2` target maps `std::fs` onto WASI-p2's filesystem interface, which wasmtime services; the host controls what the guest can reach by controlling what it preopens:

- The shared FS sandbox module (T3a, below) resolves the declared `access-scope` into a populated temp directory; the executor preopens it read-only as the guest's root: `WasiCtxBuilder::preopened_dir(sandbox_root, "/", DirPerms::READ, FilePerms::READ)`. A path that was not allowlisted simply does not exist from the guest's perspective.
- **The custom/virtual host FS alternative was never needed.** Sandbox-dir materialisation (hardlinks on the same filesystem) has not shown up as a cost problem; the in-memory `SourceTree`-backed dir remains an available future optimization.
- **The host-imported `read-file(path)` ABI stayed off the table**, as the design required.

### Shared FS sandbox module (T3a, #1414)

Shipped as `src/external/sandbox.rs` — pure Rust, no WASI or wasm dependency, consumable by any runtime (the wasm executor is the only consumer so far; the declarative runtime remains unwired, as scoped):

- `AccessScope` — `ModifiedOnly` (default) | `WholeRepo` | `Globs(patterns)`.
- `HostCeiling` — the repo root used as the hardlink source. Contract: it must equal the root of the `SourceTree` passed in; the SourceTree is authoritative for path discovery and reads.
- `create_sandbox(changeset, scope, source_tree, ceiling)` resolves the allowlist (`ModifiedOnly` → changeset paths; `WholeRepo` → all paths under ceiling; `Globs` → glob expansion ∪ changeset), creates a per-invocation `TempDir`, and populates it at repo-relative paths — by hardlink for regular files on the same filesystem, falling back to materialising content through `SourceTree::read_file` for virtual/git-backed trees.
- `validate_relative_path` rejects absolute paths and `..` traversal on every path — changeset-supplied and glob-derived alike — before any file I/O.
- Returns the `TempDir` (drop to clean up) plus the sorted, deterministic list of materialised paths for audit/logging.

Two hardening details were added during implementation beyond the design text: **symlink containment** (symlink entries are never hardlinked — hardlinking would copy the reference, potentially pointing outside the ceiling; they route through `SourceTree::read_file`, which rejects tree-escaping targets) and **deterministic sorted output** regardless of changeset or filesystem walk order.

### Limit / timeout policy (T5, #1425)

- **Epoch-based deadlines** replaced fuel as the timeout mechanism. An `EpochTicker` background thread ticks `Engine::increment_epoch` every 1 ms; each run-phase store sets an epoch deadline from the resolved policy, giving ~1 ms timeout resolution. An interrupted store surfaces a clear timeout error (detected via `Trap::Interrupt` in the error chain).
- **Memory cap** via a `ResourceLimiter` (`MemoryLimiter`) installed on run-phase stores, rejecting linear-memory growth beyond the resolved cap. T11 (#1460) fixed a bug found during cleanup: the limiter was being constructed but `store.limiter()` was never called, so the cap was silently unenforced until then.
- **Defaults and ceilings** (current values in `runtime.rs`): default memory 256 MiB, host ceiling 512 MiB; base timeout 5 s. T5 shipped a fixed 5 s default clamped at a 30 s ceiling; post-project tuning replaced the fixed default with a **proportional** one (`BASE_COMPONENT_TIMEOUT_MS` 5 s + 100 ms × changed-file count) and raised the ceiling to 300 s, so whole-repo changesets scale without over-budgeting small PRs. Manifest overrides (`limits.timeout_ms`, `limits.max_memory_mb`) are silently clamped to the ceiling, as designed.
- **Fuel was removed entirely — a divergence from the design.** The design kept fuel as an opt-in determinism knob, off by default. In practice fuel survived T5 only on the legacy `sandbox-v1` paths, and T11 deleted those; the component engine is configured with fuel off (see the `fuel=false` engine-config cache key) and no opt-in knob exists. Nothing has asked for deterministic budgets since; if CI determinism ever needs it, it would be new work.

### AOT compilation + caching (T6, #1422)

- `ComponentAotCache` (`src/external/runtime/cwasm_cache.rs`) wraps `Engine::precompile_component` → atomic `.cwasm` write → `Component::deserialize_file`. Default cache dir is `{repo_root}/.checkleft-cwasm/`; open failure degrades gracefully to per-run JIT.
- **Cache key:** `SHA-256(artifact_sha256 | wasmtime_version | engine_config_key | target_triple)` — exactly the version-discipline shape the design demanded, so a stale `.cwasm` from a different wasmtime release or engine configuration is never mis-loaded, only recompiled.
- Writes are atomic (temp file + rename), so concurrent writers produce equivalent valid files; corrupt entries (partial writes from crashes) are detected, removed, and rebuilt.
- The wasmtime version reaches the cache key via `build.rs` (reads workspace `Cargo.lock`) for cargo builds and a `rustc_env` pin in `BUILD.bazel` for bazel builds (kept in sync via an `IFCHANGE`/`THENCHANGE` marker, since bazel does not run `build.rs`).
- AOT precompile remained a **runtime** operation on first load, not a build-time bazel artifact — as designed.
- The cost story is validated by a feature-gated benchmark (`examples/wasm_aot_cache_bench.rs`) measuring cold JIT vs. warm cache-hit latency.

### Manifest schema (T7, #1415)

- `mode = "component"` / `runtime = "component-v1"` replaced `mode = "wasm"` / `runtime = "sandbox-v1"` in the TOML schema. `ExternalCheckComponentPackage` carries `artifact_path`, `artifact_sha256`, optional `limits` (`timeout_ms`, `max_memory_mb`), an optional `checks` allowlist, and provenance.
- `capabilities` validation was removed from this tier along with the mode — no capabilities grant exists in component mode; enforcement is structural.
- Sequencing note: T7 actually merged _before_ T3, landing a `Component` executor arm that bailed "not yet implemented"; T8 wired the real dispatch. The legacy `Artifact` implementation variant was deliberately kept through T7–T10 for the still-live `sandbox-v1` path and deleted in T11.
- **Known gap:** the `checks` allowlist was specified as defense-in-depth — "must agree with `list-checks`" — and T7 parses it, but no runtime cross-validation against `list-checks` output was ever wired; the field is currently stored and unused. (The related but distinct run-time check — refusing to run a check name the component does not export — _does_ exist in `run_component_check`.)

### Bundling / discovery (T8, #1423)

- One component maps to N logical `ExternalCheckPackage`s, one per exported check name. `ExternalCheckComponentPackage` gained two fields beyond the design sketch: `check_name` (the `run-check` selector; equals the package id for manifest-parsed single-check packages) and `artifact_bytes: Option<&'static [u8]>` (embedded bytes for bundled components, avoiding disk I/O; `artifact_path` is empty for bundled-only packages).
- `BundledCheckDef` became a kinded struct — `BundledCheckDefKind::Declarative` (embedded YAML manifest, as before) vs. `BundledCheckDefKind::Component` (raw wasm bytes via `include_bytes!` plus a `check_names` list). Resolution of `bundled:<name>` searches component defs' name lists and returns a package with `check_name = <name>`.
- **Scope added during implementation:** exec-path discovery (`find_in_exec_paths`) learned `check.toml` alongside `check.yaml` (YAML tried first, preserving declarative behaviour), so name-based CHECKS resolution finds component manifests on disk.
- _Post-project evolution:_ the bundled multi-check mechanism became the primary distribution path — all preinstalled wasm checks now ship in one multiplexed component of six checks, dispatched by name through `list-checks`/`run-check`.

## Build (bazel) — as built (T9, #1430)

Bazel was a hard requirement and is the shipped path: both the host runtime and guest components build entirely under bazel; no cargo-only or shell-script endpoint exists.

### Host / runtime side

As predicted, plain `rules_rust`: the executor, `bindgen!` expansion (with the WIT in `compile_data`), `HostState`/WASI wiring, limits, cache, and the pure-Rust sandbox module all build as ordinary `rust_library`/`rust_binary` targets. AOT precompile is a runtime side effect, not a build artifact. `wasmtime-wasi` resolves through the workspace `Cargo.lock`.

### Guest / check side

The design's warning stands: **`rules_rust`'s `rust_wasm_bindgen` rule is the wrong tool** — it wraps the `wasm-bindgen` JS-interop CLI targeting `wasm32-unknown-unknown` and has no connection to WASI or the Component Model. The shipped pipeline:

1. **`wasm32-wasip2` toolchain registration** — `rust.toolchain(extra_target_triples = ["wasm32-wasip2"])` in `MODULE.bazel` downloads the wasip2 `std` sysroot and registers the cross-compile toolchain; a new `//platforms:wasm32_wasip2` platform carries the matching constraints. Additionally — a wrinkle the design missed — `wasm32-wasip2` had to be added to crate_universe's `supported_platform_triples`, or the SDK's dependencies (`serde`, `serde_json`, `wit-bindgen`) are marked incompatible for the guest platform and analysis fails.
2. **Hermetic `wasm_tools_toolchain`** — `wasm-tools` v1.251.0 binaries, sha256-pinned per exec platform (aarch64-macos, x86_64-linux, aarch64-linux) via `http_archive`, exposed as a `toolchain_type` and registered in `MODULE.bazel`. No reliance on `PATH`.
3. **`rust_wasm_component` rule** (`//tools/checkleft/wasm:defs.bzl`) — builds the guest crate for `wasm32-wasip2` via an outgoing platform transition, validates/componentizes with the hermetic `wasm-tools`, and emits `<name>.wasm` plus a `<name>.wasm.sha256` sidecar (the digest a manifest pins as `artifact_sha256`).
4. **`.wit` exposure** — resolved differently than planned: the WIT is embedded in the SDK proc-macro at its own compile time (`include_str!` → `inline:` WIT in `generate!`), so guest builds need no WIT file in their sandbox at all. The planned `data`-attribute plumbing was unnecessary.
5. **CI smoke test** — the trivial example check builds through `rust_wasm_component` and a test asserts the output has a Component Model preamble and a matching sha256 sidecar, under plain `bazel test //...`.

**The design's biggest factual miss was in this section, and it made the work smaller, not larger:** the design assumed rustc emits a core module requiring `wasm-tools component new` (possibly with a wasip1→p2 adapter). Empirically, under rules_rust 0.70 + Rust 1.95, the `wasm32-wasip2` target links via `wasm-component-ld` and **already emits a valid Component Model component** — running `component new` on it would fail, since that subcommand only accepts core modules. The rule therefore validates and passes the component through; the core-module + adapter path is retained in `componentize.sh`, gated on the wasm preamble's layer byte, for robustness or a future `wasm32-wasip1` input.

## Disposition of prior work — executed as planned

- **T1397 / PR 1376 (`sandbox-v1` prototype): superseded.** T11 (#1460) deleted the hand-rolled core ABI, the fake `(string)->(string)` component path, all their helpers and constants, the JSON-over-memory types, `command_policy.rs`, and the `sandbox-v1` runtime constant and `Artifact` package variant. Salvaged, as planned: sha256 pinning, the executor/provider/runner architecture, manifest-parsing scaffolding, engine construction. There was no dual-tier migration period.
- **T1444 / PR 1410 (`giant-struct-no-builder` on `sandbox-v1`): superseded** by the T10 component port; eliminated with the runtime in T11.
- **T1371 / PR 1372 (Rust-only restriction): kept.**
- **T1407 / PR 1402 (bundled provider + CHECKS source directive): reworked** for component discovery in T8, as detailed above.

The end-to-end proof (T10, #1459) landed as specified: the check re-authored on the SDK, built by `rust_wasm_component`, bundled via a dedicated bytes-only micro-library, resolved through the provider, and executed through the full `component-v1` path — with e2e tests asserting both that the check fires (a 6-field struct without `bon::Builder`) and that **files outside the changeset are not readable** (the capability property, proven through a real running component). The native built-in coexisted briefly for a transition period (the stale-exclusion audit was adjusted to keep participating for bundled checks with a native counterpart) and has since been removed.

## Risks — how they played out

- **Wasmtime version discipline (rated highest risk): contained.** The `.cwasm` cache key includes the wasmtime version and engine config, so stale artifacts recompile rather than mis-load; the version is pinned once at the workspace level (42.0.2) and threaded into bazel builds via `rustc_env`. No version-skew incident occurred during the project.
- **Component Model / wit-bindgen / cargo-component tooling maturity: fine in practice.** Guest tooling is version-pinned (wit-bindgen 0.51, wasm-tools 1.251.0) and the WIT contract stayed small at `@0.1.0`. `cargo component` ended up not being needed at all — the bazel rule drives rustc's native wasip2 target directly.
- **Bazel guest-build infra (the dominant unknown): landed in one PR (T9), and was _less_ work than sized** because the componentization step largely disappeared (wasip2 emits components directly). The genuinely new pieces were the toolchain/platform registrations, the crate_universe triple addition, the hermetic wasm-tools toolchain, and the transition-based rule.
- **WASI-p2 as the file-access path: worked as designed.** One instantiation-shape consequence (the two-phase discovery/run split) and one runtime-integration consequence (`spawn_blocking`) emerged during implementation; both are recorded above.
- **Sandbox dir I/O cost: not a problem in practice.** Hardlink population is cheap; the virtual host-FS fallback was never needed.
- **Epoch timeout non-determinism: accepted.** The generous default (now proportional to changeset size) has not produced flakiness; the fuel determinism knob the design reserved was dropped rather than implemented.
- **Migration scope: as small as expected.** Only one check existed on the old tier; the proof was genuinely end-to-end under bazel.

## Delivery record

| Task                                                    | PR    | Merged     | Notable deviation from plan                                                                                                                  |
| ------------------------------------------------------- | ----- | ---------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| T1 — WIT contract package                               | #1413 | 2026-06-06 | None material; `bindgen!` smoke test doubles as WIT validation.                                                                              |
| T3a — Shared FS sandbox module                          | #1414 | 2026-06-06 | Added symlink containment + deterministic sorted output.                                                                                     |
| T7 — Manifest schema for component mode                 | #1415 | 2026-06-06 | Merged before T3 with a stub executor arm; `checks` allowlist parsed but validation never wired (see Known gaps).                            |
| T3 — Host component executor (typed call path)          | #1416 | 2026-06-06 | Dispatch restructured to a `match` on runtime; fuel config briefly shared generically with the legacy path.                                  |
| T2 — Guest SDK crate                                    | #1421 | 2026-06-06 | Split into SDK + proc-macro + example crates; explicit `export_checks!`; WIT embedded inline via proc-macro.                                 |
| T4 — WASI integration (sandbox as preopen)              | #1424 | 2026-06-06 | Two-phase instantiation invented here (scope discovery via `list-checks` before sandbox build).                                              |
| T8 — Provider + CHECKS rework                           | #1423 | 2026-06-06 | Added `check_name` + `artifact_bytes` fields; added `check.toml` exec-path discovery.                                                        |
| T9 — Bazel rules for wasm component checks              | #1430 | 2026-06-06 | wasip2 emits components directly — `wasm-tools component new` unnecessary; crate_universe triple addition required.                          |
| T5 — Limit / timeout policy                             | #1425 | 2026-06-09 | 1 ms `EpochTicker`; fuel not carried forward as an opt-in knob.                                                                              |
| T6 — AOT precompile + `.cwasm` cache                    | #1422 | 2026-06-09 | Atomic writes + corrupt-entry rebuild; dual cargo/bazel wasmtime-version plumbing.                                                           |
| T10 — Port `rust-giant-structs-use-builder` (e2e proof) | #1459 | 2026-06-10 | Bundle micro-library workaround; `spawn_blocking`; stale-exclusion audit narrowed; text-scan line attribution (no `Span::start()` in-guest). |
| T11 — Remove `sandbox-v1` + dead capability surface     | #1460 | 2026-06-10 | Also fixed the unwired memory `ResourceLimiter` (cap was silently unenforced until this PR).                                                 |

## Known gaps

- **Manifest `checks` allowlist is unenforced.** `ExternalCheckComponentPackage.checks` is parsed (T7) and documented as "must agree with what `list-checks` returns (defense-in-depth)", but no code reads it at resolution or execution time. The narrower protection that does exist: `run_component_check` refuses to run a `check_name` the component does not export.

## Deferred / future — unchanged or since picked up elsewhere

- **Multi-language guests** — still deferred; the WIT contract permits it.
- **Custom/virtual host-FS** (`SourceTree`-backed in-memory dir) — still deferred; profiling has not demanded it.
- **Base-revision (`TreeVersion::Base`) reads** — still deferred.
- **`SuggestedFix`/`FileEdit` application** — since delivered _outside this project_ via the `fix-check` contract extension (host-applied edits), consistent with the "checks never write" non-goal.
- **Remote component fetch + caching** — still deferred.
- **Instance pooling / warm-pool** — still deferred; instantiation cost has not warranted it.
- **Component signing / provenance beyond sha256 pinning** — still deferred.

## References

- [How do you build a Rust wasm binary with Bazel? (Stack Overflow)](https://stackoverflow.com/questions/78168400/how-do-you-build-a-rust-wasm-binary-with-bazel) — the starting-point reference for the bazel Rust→wasm pipeline. The design's caveat that the tooling had moved on proved correct in the best way: modern rustc's `wasm32-wasip2` target emits Component Model components directly, eliminating the separate componentization step that most older references (including this one) assume.
