# Checkleft: Component-Model wasm external checks (capability FS + ergonomic typed API)

Status: design (no implementation in this change)
Project: P1446 / `proj_18b65280ce1e7948_2bd`

## Overview

Checkleft runs three kinds of checks: built-in (compiled into the binary), declarative (`declarative-v1`: framework-owned binary invocations + declarative transforms), and wasm-external (`sandbox-v1`: a sandboxed wasm artifact). Most checks will live **out-of-tree** — independently versioned, authored by teams that do not ship inside the checkleft binary — and we want sandboxed, any-language-ish authoring for them.

The current `sandbox-v1` wasm runtime is a hand-rolled CORE-wasm ABI. Authors write raw pointer/length code, manage linear-memory buffers by hand, cannot read any files, and run under a fixed, tight fuel budget. None of that pain is intrinsic to wasm — it is intrinsic to a bespoke ABI that reimplements, badly, what the **WebAssembly Component Model** already standardizes.

This document audits `sandbox-v1`, then proposes re-architecting the wasm tier onto the Component Model so that:

1. **Ergonomic typed authoring** — an author writes `fn check(input: CheckInput) -> Vec<Finding>` against real Rust structs. No pointers, no manual `memory.grow`/offset math. All marshalling is generated glue (`wit-bindgen` on the guest, `wasmtime::component::bindgen!` on the host).
2. **Capability-scoped file access** — checks *can* read files, but the **host** decides and enforces exactly which paths each invocation may read. Deny-by-default, finer than whole-FS.

The current `sandbox-v1` restrictions are not sacrosanct; a fresh-slate rewrite of the wasm tier is explicitly in scope and is what this design recommends.

## Goals

- Out-of-tree checks are authored as plain typed Rust functions over generated native structs — no raw pointers, no manual linear-memory management. The guest SDK hides the ABI entirely.
- The host grants each check invocation a capability-scoped, deny-by-default set of readable files, enforced host-side and finer than whole-filesystem.
- One artifact can export many checks (one component, N checks), self-describing enough that the bundled provider + CHECKS source directive can enumerate them.
- Timeouts and memory are policy knobs with generous defaults and per-check / per-bundle overrides — not the prototype's fixed, tight fuel ceiling.
- Checks run in-process (no per-invocation process spawn), AOT-compiled and cached, with a quantified cost story versus the current ABI and versus the exec/declarative alternative.
- The existing executor/provider/runner architecture, sha256 artifact pinning, and CHECKS resolution semantics are preserved and reworked, not thrown away.
- The end-to-end proof is porting `rust-giant-structs-use-builder` (currently a built-in; ported to `sandbox-v1` in T1444/PR 1410) onto the new model.

## Non-goals

- Multi-language guest authoring in v1. Rust-only is acceptable and aligns with the wit-bindgen/`cargo component` sweet spot (this preserves the T1371/PR 1372 decision). The WIT contract does not preclude other guest languages later, but no non-Rust SDK ships in v1.
- Replacing or re-litigating the **declarative** (`declarative-v1`) tier. The declarative tier keeps owning the "run an arbitrary binary" use case (e.g. `buildifier`). This project only re-architects the **wasm** tier.
- Network access from checks. Checks remain pure-compute plus host-mediated file reads. No host-imported network capability.
- Write access to the working tree from checks. `SuggestedFix`/`FileEdit` data flows *out* as part of findings (already in the output schema); checks never mutate files directly.
- A general OS-process sandbox for wasm guests. Wasm's own isolation plus the host capability boundary is the sandbox; we are not adding seccomp/Landlock/sandbox-exec around the host process for this tier.
- Remote/registry distribution of components beyond what the existing external-URL provider path already does. Fetch/caching of remote components is called out as future work, not a v1 blocker.

## Audit of the current `sandbox-v1` runtime

Source: `tools/checkleft/src/external/runtime.rs`, `mod.rs`, `command_policy.rs`, `bundled.rs`, `provider.rs`, and `runtime/tests.rs`.

### How it works today

- **Two execution paths in one function.** `DefaultExternalCheckExecutor::execute_artifact` first tries a CORE-wasm module path (`execute_core_artifact`); on an `ArtifactMismatch` it falls back to a "component" path (`execute_component_artifact`).
- **The core path is a hand-rolled ABI.** The module must export `memory` and `checkleft_run: (i32, i32) -> i64`. The host serializes `{changeset, config, capabilities}` to JSON, writes it into the guest's linear memory at offset 0 (growing memory by hand via `ensure_memory_capacity`), calls `checkleft_run(offset, len)`, and decodes the `i64` return as a packed `(offset << 32) | len` pointing at the output JSON, which it then reads back out of linear memory. The guest is responsible for allocating that output buffer and packing the pointer.
- **The "component" path is not a real Component-Model interface.** It expects an export `run: (string) -> (string)` and passes the *same JSON string* both ways (`call_component_run`). It uses `wasmtime::component`, but there are **no WIT records** — no lifting/lowering of typed structs. It is the core ABI's manual JSON marshalling wearing a component costume.
- **Limits.** Fuel only, fixed at `EXECUTION_FUEL_LIMIT = 10_000_000`, set per store. No epoch deadline, no wall-clock timeout, no configured memory cap, no per-check override.
- **No AOT / no cache.** `Module::new` / `Component::new` recompile the artifact from bytes on **every** invocation. The `Engine` is reused, but compiled artifacts are not cached to disk (`.cwasm`) or in memory across runs.
- **Capabilities are vestigial for wasm.** The manifest `capabilities.commands` list is intersected with a global ceiling (`cat`, `grep`, `sed`, `wc`) in `command_policy.rs` and serialized into the input JSON — but the guest is pure wasm with **no host imports**, so there is no way for a guest to actually run a command. The grant is plumbed but inert in this tier. (It is a leftover from when wasm and exec shared a path; the exec use case now lives in the declarative tier.)
- **sha256 pinning works.** `validate_artifact_sha256` rejects any artifact whose bytes don't match the manifest digest. This is good and must survive.
- **One artifact = one check.** The manifest (`mode = wasm`, `runtime = sandbox-v1`, `artifact_path`, `artifact_sha256`) describes a single check. There is no bundle / `list-checks` concept.

### Gap list against the two hard requirements

| Requirement | Current state | Gap |
|---|---|---|
| Ergonomic typed authoring | Guest hand-writes `checkleft_run(i32,i32)->i64`, allocates output buffer, packs pointer; or `run(string)->(string)` with manual JSON. | No typed records, no generated glue. Authors see pointers and linear memory. Fails the requirement. |
| Capability-scoped file access | `execute()` receives `&dyn SourceTree` but ignores it (`_source_tree`). Guest gets `{changeset, config, capabilities}` JSON only. No host file import. | Checks cannot read files at all. There is no capability boundary to scope, because there is no file access to scope. Fails the requirement. |
| Limits as policy | Fixed `consume_fuel` budget, no overrides, no timeout, no memory cap. | No epoch deadline, no configurable memory, no per-check/per-bundle knobs. |
| Performance | Recompiles artifact every invocation; no `.cwasm` cache. | Wasted compile cost per run; the one thing wasm-in-process is supposed to win (cheap repeated invocation) is left on the table. |
| Bundling/discovery | One artifact, one check. | No "one component, N checks" / `list-checks`. |
| Capability surface clarity | `capabilities.commands` plumbed into wasm input but unusable by a pure guest. | Misleading dead surface that should be removed from the wasm tier. |

**Conclusion of the audit:** the prototype's pain is the bespoke ABI, not wasm. The Component Model directly closes the two top-row gaps (typed authoring, capability file access via host imports) and gives us the limit/cache/bundling story for the rest. The bespoke path should be superseded, not evolved.

## Alternatives considered

### A. Evolve the bespoke core-wasm ABI

Add host-imported file-read functions to the existing `checkleft_run` core ABI and write a hand-rolled codegen layer that marshals typed structs across the `i32`/`i64` buffer boundary.

Rejected: this reinvents, by hand and worse, exactly what the Component Model's Canonical ABI already standardizes (lifting/lowering of records, strings, lists, results, options). Every new field on `CheckInput`/`Finding` becomes manual marshalling work on both sides. There is no typed contract artifact, so guest and host can silently disagree on layout. We would own and debug a marshalling layer forever to avoid adopting the one the ecosystem already maintains.

### B. exec-external (binary + JSON-over-stdio, OS-sandboxed)

We came close to choosing this route project-wide. A check is an arbitrary binary; the host invokes it with a JSON payload on stdin, reads findings on stdout, and wraps it in an OS sandbox (bazel-style).

Rejected **as the wasm-tier replacement**, but explicitly **kept for what it is good at.** The exec model *already exists* as the declarative tier (`declarative-v1`), which subsumes the former exec tier via the `passthrough` transform and owns the "wrap an existing binary like buildifier" use case. For the *typed-Rust-check* sweet spot it loses on three axes: (1) per-invocation process spawn cost (~1-5 ms each) versus in-process instantiation (~tens of µs); (2) OS-sandbox portability — a deny-by-default file capability that behaves identically on macOS and Linux CI agents is genuinely hard with `sandbox-exec`/Landlock/seccomp and is precisely the problem the declarative tier defers; (3) "typed function over real structs" still requires every author to hand-roll JSON (de)serialization in their language. The two tiers are complementary: declarative for "any binary, framework-invoked," wasm/component for "sandboxed typed check with host-scoped file reads."

### C. Native dynamic plugins (`cdylib` + `dlopen`)

Compile checks to native shared libraries and load them with a stable C ABI.

Rejected: zero sandboxing (a plugin runs with full host privileges, defeating requirement #2's entire premise), fragile ABI across compiler/std versions, and `unsafe` loading of untrusted out-of-tree code into the checkleft process. Non-starter for out-of-tree authoring.

### D. WebAssembly Component Model — **chosen**

WIT-defined contract, `wit-bindgen` guest SDK, `wasmtime::component::bindgen!` host, host-imported capability-scoped file access, epoch/memory limits, `.cwasm` cache. Detailed below.

## Chosen approach: the Component Model

### Architecture at a glance

```
author writes:                 fn check(input: CheckInput) -> Vec<Finding>
                                      |  (wit-bindgen generates the glue)
guest crate (checkleft-check-sdk) --> wasm32 component (exports checkleft:check world)
                                      |  cargo component build -> .wasm; bazel rule emits sha256
manifest (mode=component) pins artifact_path + artifact_sha256 + reads globs + limits
                                      |
host (checkleft) -- component::bindgen! --> instantiate (cached .cwasm)
   - provides host-fs import (capability allowlist enforced here)
   - epoch deadline + memory ResourceLimiter
   - calls list-checks() / run-check(name, input) -> list<finding>
```

### WIT contract

A new in-tree WIT package, e.g. `wit/check.wit`, defines `package checkleft:check@0.1.0`. The records mirror today's `input.rs` / `output.rs` types so the host lift/lower is mechanical. Sketch (illustrative, not final):

```wit
package checkleft:check@0.1.0;

interface types {
  enum change-kind { added, modified, deleted, renamed }
  record changed-file { path: string, kind: change-kind, old-path: option<string> }
  record file-line-delta { added-lines: u32, removed-lines: u32 }
  record diff-hunk {
    old-start: u32, old-lines: u32, new-start: u32, new-lines: u32,
    added-lines: u32, removed-lines: u32,
  }
  record file-diff { path: string, hunks: list<diff-hunk> }

  record change-set {
    changed-files: list<changed-file>,
    file-diffs: list<file-diff>,
    commit-description: option<string>,
    pr-description: option<string>,
    change-id: option<string>,
    repository: option<string>,
  }

  // Per-check config is dynamic (toml::Value today). v1 passes it as a JSON
  // string the guest SDK deserializes with serde. See open question Q1.
  record check-input { changeset: change-set, config-json: string }

  enum severity { error, warning, info }
  record location { path: string, line: option<u32>, column: option<u32> }
  record file-edit { path: string, old-text: string, new-text: string }
  record suggested-fix { description: string, edits: list<file-edit> }
  record finding {
    severity: severity,
    message: string,
    location: option<location>,
    remediations: list<string>,
    suggested-fix: option<suggested-fix>,
  }

  // Self-description for bundling/discovery.
  record check-descriptor {
    name: string,
    description: string,
    default-severity: severity,
    // file globs (repo-root-relative) this check wants to read; the host
    // intersects these with its ceiling to build the allowlist.
    reads: list<string>,
  }

  variant check-error { unknown-check(string), failed(string) }
}

// Host-provided, capability-scoped filesystem. Deny-by-default; the host
// enforces the allowlist. Mirrors the host SourceTree.
interface host-fs {
  use types.{location};
  enum fs-error { denied, not-found, io }
  read-file: func(path: string) -> result<list<u8>, fs-error>;
  file-exists: func(path: string) -> bool;
  list-dir: func(path: string) -> result<list<string>, fs-error>;
  glob: func(pattern: string) -> result<list<string>, fs-error>;
}

world check {
  use types.{check-input, finding, check-descriptor, check-error};
  import host-fs;
  list-checks: func() -> list<check-descriptor>;
  run-check: func(name: string, input: check-input) -> result<list<finding>, check-error>;
}
```

Design choices baked into the contract:

- **`list-checks` + `run-check(name, input)`** make one component self-describing and able to export N checks (the "one artifact, N checks" requirement). The host instantiates once, calls `list-checks()` to enumerate, and dispatches by name.
- **`host-fs` is the file capability.** It is a host *import*, so the host implements it and enforces the allowlist; the guest can only ask. This is finer than WASI preopens (see file-capability section).
- **Config as `config-json`.** `toml::Value` is dynamic; modeling arbitrary config as WIT records would force a schema per check or a recursive `variant`. v1 passes config as a JSON string and the guest SDK deserializes it into the author's own `#[derive(Deserialize)]` config struct. (Open question Q1 — could later become a typed-per-check generic.)

### Guest SDK + build pipeline

- A new crate `checkleft-check-sdk` (name TBD) wraps `wit-bindgen` so authors implement a small trait and register checks. Target ergonomics:

```rust
use checkleft_check_sdk::{check, CheckInput, Finding, Severity, host_fs};

#[check(name = "rust-giant-structs-use-builder", reads = ["**/*.rs"])]
fn run(input: CheckInput) -> Vec<Finding> {
    let cfg: MyConfig = input.config()?;          // serde over config-json
    let src = host_fs::read_file(&path)?;          // capability-scoped read
    // ... pure analysis ...
    vec![Finding::error("…").at(path, line)]
}
```

  The `#[check]` macro registers the function in the component's `list-checks`/`run-check` dispatch table and records its `reads` globs as the declared capability request. Authors never touch the WIT bindings, pointers, or memory.

- **Build:** `cargo component build` (or `wasm-tools component new` over a `wasm32-wasip2`/`wasm32-unknown-unknown` core module) produces the component `.wasm`. A bazel rule wraps this for hermetic, reproducible builds and emits the sha256 that goes in the manifest. There is **no existing rules_rust wasm-component rule in this repo today** (the `musl/` tooling cross-compiles the *host* binary, not guests), so the bazel guest-build rule is real new infra and is sized accordingly in the task breakdown (T9).

### Host runtime

- `wasmtime::component::bindgen!` generates host-side bindings from the WIT. The existing `DefaultExternalCheckExecutor` is rewritten to: load (or deserialize from cache) the component, build a `Linker` that wires the `host-fs` import to a host struct carrying the per-invocation capability state, instantiate into a `Store<HostState>` (today's store is `Store<()>` — it gains real state), call `list-checks`/`run-check`, and lift the returned `list<finding>` into the existing `Finding` type.
- The executor/provider/runner trait architecture (`ExternalCheckExecutor`, `ExternalCheckPackageProvider`, `CompositeExternalCheckPackageProvider`, the runner's `ExternalResolved` scheduling) is preserved. The change is concentrated in `runtime.rs` and the manifest schema; the wiring in `runner.rs` stays.

### File-capability model + allowlist policy

- **Primary mechanism: the `host-fs` import with a host-enforced allowlist.** Chosen over WASI preopened dirs because checkleft's `SourceTree` is not always a real directory — it can be a git-backed virtual tree, and it already distinguishes `Current` vs `Base` revisions (`read_file_versioned`). WASI preopens can express "this real dir, read-only" but cannot express "the base revision of the repo" or a virtual tree, and would require linking the full `wasmtime-wasi` filesystem. The host import maps cleanly onto the existing `SourceTree` trait, which already has `read_file`, `exists`, `list_dir`, `glob`.
- **Deny-by-default allowlist, computed per invocation** from:
  1. **Changed files** — the paths in `changeset.changed_files` are always readable (read-only). A check that inspects what changed needs no extra grant.
  2. **Check-declared `reads` globs** — from the `#[check(reads = [...])]` attribute, surfaced via `list-checks`'s `check-descriptor.reads`, intersected with a **host ceiling** (repo root, read-only). A check declaring `reads = ["Cargo.toml", "**/*.rs"]` may read matching paths anywhere under the repo root.
- **Enforcement.** Every `host-fs` call normalizes the path with the existing `validate_relative_path`, rejects absolute paths and `..` traversal, checks it against the computed allowlist, and on miss returns `fs-error::denied`. Reads route through the `SourceTree` the executor already receives (and currently ignores), so guest reads are consistent with what the rest of checkleft sees. Base-revision reads via `host-fs` are a v1-optional extension (Q + future task).
- **WASI:** because `std`-using Rust guests built via `cargo component` import some WASI interfaces (clocks, stdio) even when they never call the filesystem, the host links a **minimal `wasmtime-wasi` context with no preopened directories and no real FS access**, purely to satisfy those imports. All *real* file access goes through `host-fs` and the allowlist — never through WASI preopens. (Open question Q2: minimal-WASI-stub vs. no-WASI guests.)

### Limit / timeout policy

- **Epoch-based deadlines** replace fuel as the default timeout. The engine enables epoch interruption; a background thread (or the existing run scheduler) ticks the epoch, and the store sets an epoch deadline. Default: generous wall-clock budget (proposed 5 s), far above the prototype's tight fuel ceiling.
- **Memory cap** via a `StoreLimits`/`ResourceLimiter` on the store (proposed default 256 MiB), configurable.
- **Per-check / per-bundle overrides** in the manifest (`limits.timeout_ms`, `limits.max_memory_mb`), clamped by a host ceiling so an out-of-tree manifest cannot grant itself unbounded resources. Trusted bundles can opt into a relaxed tier.
- Fuel remains available as an optional secondary determinism knob (useful for reproducible CI) but is **off by default** in favor of epoch deadlines. (Open question Q3.)

### Bundling / discovery format

- **One component, N checks**, self-describing via `list-checks`. The host instantiates the component once and enumerates `check-descriptor`s.
- **Manifest** evolves to a new `mode = "component"` / `runtime = "component-v1"`, carrying `artifact_path`, `artifact_sha256`, optional `limits`, and (optionally) an explicit `checks = [...]` allowlist that must agree with `list-checks` (defense in depth against an artifact silently exporting an unexpected check). The legacy `mode = "wasm"` / `runtime = "sandbox-v1"` is removed (see disposition).
- The bundled provider (`bundled.rs`) embeds component bytes via `include_bytes!` (today it `include_str!`s YAML manifests). A bundled component's exported checks become resolvable by name.

### AOT compilation + caching

- Precompile with `Engine::precompile_component` to a `.cwasm`, cached on disk keyed by **`(artifact_sha256, wasmtime_version, engine_config_hash, target_triple)`**. Load with `Component::deserialize_file` (trusted: checkleft produced the file). The cache key *must* include the wasmtime version because `.cwasm` is not portable across wasmtime releases — this is the central version-discipline risk (see Risks).
- Instantiate per-invocation in-process; no process spawn.
- **Cost story (to be measured in implementation, expected order-of-magnitude):**
  - Today (`sandbox-v1`): full `Module::new` compile **per invocation** — the dominant, repeated cost.
  - New: one-time compile (tens of ms) amortized into the `.cwasm` cache; subsequent runs pay deserialize (low ms) + instantiate (tens of µs) + the typed call. Net: repeated invocations get dramatically cheaper than the current recompile-every-time path.
  - vs. exec/declarative: avoids ~1-5 ms process spawn per invocation and OS-sandbox setup.

## Changes to the T1407 check-def provider + CHECKS source directive

T1407 (PR 1402) gives us the bundled-def provider and the `check_definitions` CHECKS section (`exec_paths`, `allow_override_bundled`) — see `config.rs` (`ResolvedCheckDefinitions`) and `bundled.rs`. These **survive and are reworked**, not discarded:

- `ExternalCheckImplementationRef` (`File` / `Generated` / `Bundled`) is unchanged.
- **Component discovery.** A component artifact exports N checks. Resolution maps one component artifact to N logical `ExternalCheckPackage`s — one per exported check name — sharing the artifact and each carrying a `run-check` selector (the check `name`). Name-based resolution (bundled or exec-path) can therefore resolve `my-check` to "component X, export `my-check`."
- **Manifest loader.** `parse_external_check_manifest` / the TOML schema in `mod.rs` gains the `component` mode and `component-v1` runtime, and drops `wasm`/`sandbox-v1`. The `RawExternalCheckMode` enum gains `Component`; `validate_runtime_for_mode` maps it to `component-v1`.
- **Bundled provider.** `bundled.rs` embeds component bytes and resolves a `bundled:<name>` ref to the component package, with the enumerated check selected by name.
- **`capabilities.commands` is removed from the wasm/component tier** (it was inert there). `command_policy.rs` stays relevant only to the declarative tier's binary invocations.

## Disposition of prior work

- **T1397 / PR 1376 (`sandbox-v1` prototype): SUPERSEDE.** Delete the hand-rolled core ABI path (`checkleft_run`, manual `read_memory`/`write_memory`/`ensure_memory_capacity`/`decode_output_range`) and the fake `(string)->(string)` "component" path from `runtime.rs`. **Salvage:** sha256 pinning (`validate_artifact_sha256`), the `ExternalCheckExecutor`/provider/runner architecture, the manifest-parsing scaffolding, and the engine-construction skeleton.
- **T1444 / PR 1410 (`giant-struct-no-builder` ported to `sandbox-v1`): SUPERSEDE.** It is reference-only and built on the ABI being removed. Re-port the check onto the new component model as the end-to-end proof (final migration task), then close the old port.
- **T1371 / PR 1372 (Rust-only restriction): KEEP.** It aligns with the Rust/wit-bindgen sweet spot and the v1 non-goal of multi-language guests.
- **T1407 / PR 1402 (bundled provider + CHECKS source directive): REWORK** to load the component format, as detailed above.

## Risks / open questions

- **Wasmtime version discipline (highest risk).** `.cwasm` artifacts are not portable across wasmtime versions, and the Component Model's host/guest ABI tracks the wasmtime/wit-bindgen release train. Mitigations: the cache key includes the wasmtime version (stale `.cwasm` is recompiled, never mis-loaded); bundled in-tree components are rebuilt as part of the normal build on a wasmtime bump; the workspace already pins wasmtime (`42.0.1`/`42.0.2`) in one place, so bumps are deliberate.
- **Component Model / `wit-bindgen` / `cargo component` tooling maturity.** These move faster than the Rust release train. Mitigation: pin guest tool versions in the bazel rule; keep the WIT contract small and stable (`@0.1.0`).
- **Bazel guest-build infra is new.** There is no rules_rust wasm-component rule in this repo today. Building reproducible `wasm32` components under bazel (hermetic toolchain, deterministic output, sha emission) is the largest single unknown; sized as `large` (T9).
- **WASI linking surface.** Std Rust guests pull WASI imports; the host must satisfy them (minimal stub) or guests must be built to avoid them. Q2 below.
- **Epoch timeouts are non-deterministic.** A wall-clock deadline can fire differently across machines. Acceptable given generous defaults; fuel remains available for CI determinism if needed (Q3).
- **Migration scope is small but real.** Only `giant-struct-no-builder` exists as a wasm-tier check today, so migration risk is low — but the proof must be genuinely end-to-end (authored via the SDK, built via the bazel rule, resolved via the provider, run via the new host).

Open questions to be settled by a human are captured in the sibling `*.attentions.json` manifest. In summary they are: (Q1) config as JSON-string vs. typed-per-check vs. WIT records; (Q2) minimal-WASI-stub vs. no-WASI guests only; (Q3) epoch-only vs. fuel-also default; (Q4) host-import-only vs. host-import + WASI preopen for file access; (Q5) remove `sandbox-v1` immediately vs. keep dual support during migration.

## Migration plan

1. **Phase 0 — Design (this document).**
2. **Phase 1 — Contract + SDK.** Land the WIT package and the `checkleft-check-sdk` guest crate (T1, T2).
3. **Phase 2 — Host runtime.** New component executor with `host-fs` allowlist, epoch/memory limits, and `.cwasm` cache (T3-T6).
4. **Phase 3 — Manifest/provider/CHECKS rework + bundling** (T7, T8).
5. **Phase 4 — Bazel guest-build rule** (T9).
6. **Phase 5 — Proof.** Port `rust-giant-structs-use-builder` onto the SDK, build it via the bazel rule, resolve it via the provider, and run it through the new host with an end-to-end test (T10). This is the acceptance proof for the whole project.
7. **Phase 6 — Cleanup.** Remove `sandbox-v1` runtime paths and the inert wasm-tier command-capability surface (T11).

## Proposed implementation task breakdown

Tasks are PR-sized and listed in dependency order. "Depth" notes which tasks share a dependency level and may run in parallel. Effort hint ∈ `trivial | small | medium | large`.

### T1 — WIT contract package
**Scope:** Add the in-tree `checkleft:check@0.1.0` WIT package (`types`, `host-fs`, `check` world) mirroring `input.rs`/`output.rs`. Includes a host-side `bindgen!` smoke test that the WIT compiles and a doc comment mapping each WIT record to its Rust counterpart. No behavior wired yet.
**Effort:** small. **Depends on:** none.

### T2 — Guest SDK crate (`checkleft-check-sdk`)
**Scope:** New guest crate wrapping `wit-bindgen` with the `#[check(name, reads)]` macro, the `list-checks`/`run-check` dispatch table, a `CheckInput::config::<T>()` serde helper over `config-json`, and a `host_fs` ergonomic wrapper. Ships a trivial example check that compiles to a component. Authors see only native structs.
**Effort:** medium. **Depends on:** T1.

### T3 — Host component executor (typed call path)
**Scope:** Rewrite `DefaultExternalCheckExecutor` to load a component, build a `Store<HostState>` and `Linker`, instantiate, and call `list-checks`/`run-check`, lifting `list<finding>` into `Finding`. No file capability or cache yet (stub `host-fs` that denies all). Replaces the core/fake-component paths' call mechanics.
**Effort:** medium. **Depends on:** T1. *(Parallel with T2 — both depend only on T1.)*

### T4 — `host-fs` capability + allowlist enforcement
**Scope:** Implement the `host-fs` import against the existing `SourceTree`, with the per-invocation deny-by-default allowlist (changed files + check-declared `reads` globs ∩ host ceiling), path normalization/traversal rejection via `validate_relative_path`, and `fs-error` mapping. Tests for grant, deny, and traversal-escape attempts.
**Effort:** medium. **Depends on:** T3.

### T5 — Limit / timeout policy
**Scope:** Epoch-based deadline (engine epoch ticking + store deadline), memory `ResourceLimiter`, generous defaults, and manifest `limits.{timeout_ms,max_memory_mb}` overrides clamped by a host ceiling. Tests for timeout trip and memory cap trip.
**Effort:** medium. **Depends on:** T3. *(Parallel with T4 and T6.)*

### T6 — AOT precompile + `.cwasm` cache
**Scope:** `precompile_component` → on-disk `.cwasm` cache keyed by `(artifact_sha256, wasmtime_version, engine_config_hash, target)`, with safe deserialize-on-load and cache-miss rebuild. Benchmark cold vs. warm invocation to validate the cost story.
**Effort:** medium. **Depends on:** T3. *(Parallel with T4 and T5.)*

### T7 — Manifest schema for `component` mode
**Scope:** Add `mode = "component"` / `runtime = "component-v1"` to the TOML schema in `mod.rs` (`RawExternalCheckMode::Component`, `validate_runtime_for_mode`), carrying `artifact_path`, `artifact_sha256`, optional `limits`, optional `checks` allowlist. Remove the `wasm`/`sandbox-v1` mode and its now-inert `capabilities.commands` handling for this tier. Parser tests.
**Effort:** small. **Depends on:** T1. *(Parallel with T2/T3.)*

### T8 — Provider + CHECKS rework for component discovery
**Scope:** Map one component artifact to N logical packages (one per `list-checks` export, carrying a `run-check` selector); rework the bundled provider (`bundled.rs`) to `include_bytes!` component bytes; ensure name-based resolution (bundled + exec-path CHECKS `check_definitions`) resolves to the right component+export. Tests across composite-provider resolution.
**Effort:** medium. **Depends on:** T3, T7.

### T9 — Bazel reproducible guest-build rule
**Scope:** A hermetic bazel rule that builds a `checkleft-check-sdk` guest crate into a wasm component (`cargo component` / `wasm-tools` under a pinned toolchain), produces deterministic output, and emits the sha256 for the manifest. New infra (no existing wasm-component rule in-repo). Build a sample guest through it in CI.
**Effort:** large. **Depends on:** T2.

### T10 — Port `rust-giant-structs-use-builder` as the end-to-end proof
**Scope:** Re-author the check on the guest SDK (superseding the T1444 `sandbox-v1` port), build it via T9, bundle/resolve it via T8, and run it through the new host (T3-T6) in an end-to-end test that exercises a capability-scoped file read and a real finding. This is the project's acceptance proof.
**Effort:** medium. **Depends on:** T2, T4, T8, T9. *(T5/T6 should also be landed for a realistic run, but are not strictly gating the proof's correctness.)*

### T11 — Remove `sandbox-v1` runtime and dead capability surface
**Scope:** Delete the hand-rolled core ABI and fake-component paths from `runtime.rs`, the `sandbox-v1` constants, and the inert wasm-tier `capabilities.commands` plumbing. Update tests/docs. Strictly after nothing resolves to `sandbox-v1`.
**Effort:** small. **Depends on:** T8, T10.

### Parallelism summary (task graph, not a linear list)

- Depth 0: **T1**.
- Depth 1 (after T1, parallel): **T2**, **T3**, **T7**.
- Depth 2 (after T3, parallel): **T4**, **T5**, **T6**. (T9 also starts here, after T2.)
- Depth 3: **T8** (after T3, T7).
- Depth 4: **T10** (after T2, T4, T8, T9).
- Depth 5: **T11** (after T8, T10).

### Deferred / future — not a v1 blocker

- **Multi-language guests** (non-Rust SDKs). The WIT contract already permits it; no SDK ships in v1 (preserves T1371).
- **WASI preopened-directory file mode** as an alternative/supplement to `host-fs` (Q4).
- **Base-revision (`TreeVersion::Base`) reads via `host-fs`** — extend the import with a revision argument.
- **End-to-end `SuggestedFix`/`FileEdit` application** from component findings (data already flows out; applying it is separate).
- **Remote component fetch + caching** beyond the existing external-URL provider path.
- **Instance pooling / warm-pool** for very high check counts (instantiation is already cheap; revisit only if profiling demands it).
- **Component signing / provenance** beyond sha256 pinning.

**Effort estimate (whole project):** ~10-12 PRs. The host-side path (T1, T3-T8, T10, T11) is mostly `small`/`medium` and well-understood given the existing architecture. The dominant unknown is **T9 (bazel reproducible wasm-component build, `large`)**, which is new infrastructure; it can proceed in parallel with the host work once the SDK (T2) exists, so it need not serialize the critical path.
