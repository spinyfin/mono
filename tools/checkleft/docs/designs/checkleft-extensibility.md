# Checkleft: Extensibility — Bundles, Central Policy, and Host Hooks

**Project:** `proj_18b98ce3870caae8_207` — checkleft extensibility
**Status:** Design (no implementation in this PR)
**Author:** Boss worker `exec_18b98cf64218f0a0_20e`

## Overview

`checkleft` is a repository convention checker: it enforces a repo's house rules — naming conventions, forbidden imports, file-size limits, doc-link integrity, and similar policies — at change-review time, reporting findings as human-readable output or JSON so the same rules guard a CI step and a local pre-push run. It ships as both a CLI and a library and is independent of the Boss automation system.

This document designs checkleft's **extensibility** along three distinct axes. They are conceptually separate but share substrate, so the doc keeps them separate while factoring the common machinery out once.

- **Axis 1 — Extension:** package custom checks that live in one repo into a portable artifact (`.tar.gz`) that other repos consume. A bundle contains a multiplexed `.wasm` of compiled custom checks, the manifests, and declarative check definitions it exports.
- **Axis 2 — Central control:** let a repo declare "the canonical config is external to me" and pull a central `CHECKS.yaml` from another repo via a pinned version pointer, enabling org-wide policy and staged rollout.
- **Axis 3 — Host/embedder integration hooks:** compile-time trait seams (check gating, metrics) that an embedder injects at runner-construction time. These are *not* distributed artifacts and *not* sandboxed — they are proprietary host concerns with ambient/network access.

The central insight: **Axes 1 and 2 share a substrate** — a versioned pointer + sha256 integrity + fetch + cache layer. That substrate is designed once (`## Chosen approach → Shared substrate`) and both axes ride it. Axis 3 is a different *kind* of extensibility (compile-time dependency injection, not distributed artifacts) and shares no substrate with the other two, but its reproducibility requirement (capturing gate decisions in run output) interacts with how runs are reported.

This document begins with a grounding/current-state section (a folded-in audit of the existing machinery) and then designs each axis, reinventing where that yields a cleaner result. The deliverable is the design only; no implementation lands in this PR.

## Current state and gap inventory

This section inventories what exists today (with file references) and what is absent, per axis. The notes are grounding, not constraints: reinventing is acceptable where it produces a cleaner design.

### Axis 1 — external check packages today

The core type is `ExternalCheckPackage` (`tools/checkleft/src/external/mod.rs`), with two runtime tiers:

- **Component tier** (`component-v1`): a WASM Component Model artifact. `ExternalCheckComponentPackage` carries `artifact_path` (repo-root-relative `.wasm`), `artifact_sha256` (64 lowercase hex, validated), optional embedded `artifact_bytes` (first-party/bundled), `check_name` (export to dispatch), optional `limits` (`timeout_ms` clamped to 300_000 ms, `max_memory_mb` clamped to 512 MiB), and `checks: Option<Vec<String>>` — a defense-in-depth allowlist validated against the component's `list-checks` output. One component can export many checks (multiplexed).
- **Declarative tier** (`declarative-v1`): YAML carrying `needs` (named `BinaryRequirement`s, each a `Bazel(label)` or `Path(string)` binding with optional fallback), `applies_to` globs, and `invocations` (a `tool` invocation or bazel aspect, with transforms / finding templates). Declarative checks carry **no wasm**; they invoke host binaries.

Provider model — `ExternalCheckPackageProvider` (`src/external/provider.rs`) resolves an `ExternalCheckImplementationRef`:

- `NoopExternalCheckPackageProvider` — resolves nothing.
- `FileExternalCheckPackageProvider` — resolves `File(relative_path)` from a manifest on disk.
- `GeneratedExternalCheckPackageProvider` — resolves `Generated(id)` via a TOML index of `generated:<id>` entries.
- `BundledExternalCheckPackageProvider` — resolves `Bundled(name)` against compiled-in defs in `BUNDLED_CHECK_DEFS` (`bundled.rs`); component defs embed wasm via `include_bytes!`, declarative defs via `include_str!`.
- `CompositeExternalCheckPackageProvider` — tries all providers and **rejects collisions**: if more than one provider resolves the same reference, it `bail!`s ("resolved by multiple providers"). Deterministic single winner or error; no silent shadowing.

Loading: CLI `--external-checks-file` / `--external-checks-url` (mutually exclusive), root-config discovery via `settings.external_checks_url`, chaining, and TOML/YAML manifest parsers (YAML → declarative schema, TOML → unified schema).

SDK authoring path: `#[check]` (`sdk-macro`) with `name` / `description` / `severity` / `access_scope`, plus `export_checks!` which calls `wit_bindgen::generate!` and wires the `list-checks` / `run-check` dispatch. The WIT contract is `checkleft:check@0.1.0` (`wit/check.wit`). The `preinstalled-bundle` crate (a `cdylib` → one component) proves that **one multiplexed component dedups the wasm std/SDK/serde/`syn` baseline via LTO**: six preinstalled checks link the shared baseline once, and a test asserts the consolidation invariant (all six resolve to the same `artifact_bytes` pointer and sha256). Component bytes are AOT-compiled to `.cwasm` and cached on the host via wasmtime; the sha256 is computed at runtime from the embedded bytes.

WASI sandboxing exists for the component tier (`src/external/sandbox.rs`): `AccessScope` is `ModifiedOnly` (default) / `WholeRepo` / `Globs(..)` / `ExplicitFiles(..)`. The host builds a per-invocation temp dir (hardlinking from the source tree), preopened read-only at `/`; guests read with `std::fs`.

Build tooling: `bazel/defs.bzl` provides `CheckInfo` and a `local_check` rule, but only for **declarative passthrough** checks. There are **no out-of-tree bazel rules for producing wasm components** beyond the preinstalled-bundle build.

**Gaps (Axis 1):**

1. No archive format and no **top-level bundle manifest that aggregates N packages** into a portable artifact.
2. No provider that resolves checks from an **unpacked archive directory**.
3. No **cross-repo fetch → verify(sha256) → unpack → cache** layer for bundles.
4. No **build tooling to produce the archive** from a bundle crate (only the preinstalled-bundle build exists).
5. No **check-id namespacing across bundles** or cross-bundle collision rules (the in-process `Composite` collision check is the closest analogue).

### Axis 2 — external config / central policy today

`settings.external_checks_url` (`ParsedSettings`, `tools/checkleft/src/config.rs`) is a **bare HTTP(S) URL**, **root-config-only** (a diagnostic is raised if a child directory sets it), validated via `reqwest::Url::parse`. There is **no pin** — the URL is fetched bare.

`apply_external_checks_file` merges a remote checks config into the resolved config: settings (`include_config_files`, `stale_exclusion_severity`) and, per check, `id`, `check` (impl ref, defaulting to `id`), `enabled`, `policy.severity`, `policy.allow_bypass`, `policy.bypass_name`, `config`, and `implementation`. Merge order is **external first, then local root, then child configs** — so today **local config can override external policy** (a "merge / local-override" posture).

`load_external_checks_chain` follows `settings.external_checks_url` links: max depth 8 (`EXTERNAL_CHECKS_MAX_CHAIN_DEPTH`), cycle detection via `seen_urls`, up to 5 fetch attempts with exponential backoff, and the loaded chain is reversed so upstream/root applies first and downstream repos can override.

There is a **trust boundary**: external configs may **not** use `check_definitions.exec_paths` — a directory-path source would reach into the consuming repo's filesystem (the same boundary as a `File` impl ref) — and may **not** use `File` implementation refs (only `generated:` and `bundled:` are allowed). Both are hard `bail!`s.

Inherently-local config that must remain regardless of governance posture: per-PR **BYPASS tokens** (parsed from commit/PR descriptions as `BYPASS_<CHECK>=<reason>`, `bypass.rs`), **ifchange/thenchange** triggers (in-source `LINT.IfChange` / `LINT.ThenChange` markers plus per-check config globs), and **vendored-dir exclusions** (per-check implementation concerns).

Integrity/pinning: the **only** sha256 pin today is `artifact_sha256` in **component** manifests. There is **no `checkleft.lock`** and no multitool-style pin file. Declarative manifests have no integrity field. External config fetches rely on HTTPS transport security with no content checksum.

**Gaps (Axis 2):**

1. No **versioned, integrity-pinned pointer** (repo + ref/version + sha256) — only a bare URL with no pin and no version identity.
2. No **governance dial**: today's only posture is merge/local-override. Central control wants a *strict* posture (no silent policy divergence) and an auditable *allowlisted-local-override* middle ground.
3. No **rollout model** primitives: no per-repo pin + bot-bump mechanism for staged adoption (and no explicit floating/instant alternative).
4. No **lockfile** to record resolved pins for reproducibility.

### Axis 3 — runner construction and DI seams today

`Runner` (`tools/checkleft/src/runner.rs`) holds five injected dependencies — `registry`, `resolver`, `source_tree`, `external_package_provider`, `external_executor` — supplied at construction time via `Runner::new`, `with_external_package_provider`, and `with_external`. The runner is **immutable** after construction; there is no runtime swapping. This is exactly the dependency-injection seam an embedder uses today.

Execution flow: `run_changeset` → `schedule_runs` (resolve checks per changed file via `resolver.resolve_for_file`, dedup identical instances by a fingerprint of `(check_id, check_name, implementation_ref, config, policy)`) → spawn built-in checks (tokio task) and external checks (`spawn_blocking`, because wasmtime's blocking WASI calls panic under an active tokio runtime) → time each with `Instant` (`elapsed_ms`) → `apply_policy_to_result` (scope to changeset, apply bypass, set severity override) → collect → `audit_stale_exclusions`.

`tracing` instrumentation already exists throughout, target `checkleft`, e.g. `info!` events "running built-in check" / "built-in check complete" (with `elapsed_ms`, `findings`), "running external check" / "external check complete", "run complete" (`elapsed_ms`, `checks_ran`, `total_findings`), and `debug!` "component loaded" (`elapsed_ms`) for the AOT compile. There is **no** OTel/metrics integration, and these are flat `info!` events, not structured spans.

Run output (`src/output.rs`): `CheckResult { check_id, findings: Vec<Finding> }`; `Finding { severity, message, location, remediations, suggested_fix }`. The output carries **no execution metadata** — `elapsed_ms` is computed but not serialized, and there is no field for a gate decision or applied policy.

**Gaps (Axis 3):**

1. No **gate/experiment/shadow** hook — nothing consults a remote enable/disable signal during resolution/execution. (The only runtime knob is the static `CHECKLEFT_EXTERNAL_PROVIDER_MODE` env var.)
2. No **metrics seam** — only flat `tracing` events; nothing an embedder can attach a `tracing-opentelemetry` layer to and get useful per-check metrics, because the events aren't structured spans with stable fields.
3. The run output **cannot explain a remote-gated result** — there is no place to record "this check was Shadowed / Disabled by gate X" for reproducibility.

### Cross-cutting: supply-chain posture today

checkleft already has a supply-chain ethos — sha256-pinned component artifacts, AOT-cache-on-host, a trust boundary that refuses fs-reaching external config — but it is **partial and per-surface**. The pin exists for component artifacts but not for external config; there is no lockfile; and the fetch/cache machinery (`reqwest` + retry/backoff for config, the `.cwasm` cache for components) is duplicated per surface rather than unified. The design unifies this into one substrate.

## Goals

1. **Portable bundles.** A repo can build a bundle artifact aggregating N check packages (one multiplexed `.wasm` + declarative defs + manifests) and another repo can consume it by pointing at it, with integrity guaranteed end-to-end.
2. **Central, versioned policy.** A repo can declare its canonical config lives elsewhere and pull it via a **pinned version pointer**, enabling org-wide policy and staged rollout — with a governance dial spanning merge → strict → allowlisted-local-override.
3. **One shared substrate.** Versioned pointer + sha256 integrity + fetch + cache is designed **once** and reused by Axes 1 and 2, extending checkleft's existing pin/lock ethos.
4. **Two compile-time host seams.** A `CheckGate` seam (Enabled / Disabled / Shadow) and a metrics seam, both injected by the embedder at runner-construction time, with OSS no-op defaults and proprietary implementations in the closed embedding.
5. **Trustworthy by construction.** WASM stays sandboxed; declarative host-binary execution from third-party bundles is gated; remote-gated results are reproducible because gate decisions are captured in run output.
6. **Reproducibility.** A red/green result is always explainable: which bundle@version, which policy pin, and which gate decisions produced it can be read back from the run (and pinned/replayed).

## Non-goals

- **A dynamic/loadable plugin framework for host hooks.** Axis 3 is deliberately *two concrete compile-time trait seams*, not a general dlopen/wasm host-plugin system. Proprietary host concerns are injected by the embedder, not discovered at runtime.
- **Shipping `.cwasm` in bundles.** Precompiled `.cwasm` is Wasmtime-version- and ISA-specific and fragile. Bundles ship portable `.wasm`; the host AOT-compiles and caches `.cwasm`. (External bundles are *always* wasm-and-cache regardless of how T1893's cold-start work lands.)
- **Per-check wasm files.** One multiplexed `.wasm` per bundle (the preinstalled-bundle already proves LTO dedup); not one artifact per check.
- **Replacing the inherently-local config surface.** BYPASS tokens, ifchange/thenchange triggers, and vendored-dir exclusions remain local regardless of governance posture. "Strict" means no *silent policy divergence*, not zero local config.
- **A new metrics protocol.** We lean on existing `tracing` + `tracing-opentelemetry`; we do not invent a checkleft-specific metrics wire format.
- **Solving T1893 (cwasm cold-start) here.** That is a separate project; this design only notes its boundary.
- **OCI/registry distribution in v1.** Content-addressable registry transport is attractive but is new infra; v1 uses HTTPS + sha256, with the substrate designed to admit other transports later.

## Alternatives considered

At least two distinct approaches per axis (and for the substrate), with the reason each was not chosen.

### Shared substrate alternatives

- **(A) Bare URL + optional sha256 only** (minimal delta over today). *Rejected:* no version identity and no lockfile, so there is no way to express "which version is pinned," no bot-bump rollout, and no reproducible record of what a CI run actually fetched.
- **(B) OCI/registry, content-addressable distribution.** Attractive (digests are native, mirrors are standard). *Deferred, not chosen for v1:* it introduces new infrastructure and auth surface; HTTPS + sha256 reuses the existing `reqwest` fetch + retry/backoff and the `.cwasm`-cache patterns and gets us the same integrity guarantee. The substrate is shaped so an OCI transport can be added behind the same pointer type later.
- **(C, chosen) Versioned pointer (repo/locator + version + sha256) over HTTPS, with a `checkleft.lock` and a content-addressed cache.** Generalizes the existing component `artifact_sha256` pin to *both* surfaces and adds the missing version identity and lockfile.

### Axis 1 alternatives

- **(A) Per-check wasm + per-repo `File`/`Generated` vendoring** (no archive). *Rejected:* duplicates the std/SDK/serde baseline per check, produces many artifacts, and forces the consuming repo to vendor files with no single integrity-pinned unit. The preinstalled-bundle already demonstrates the superior multiplexed-component path.
- **(B) Ship precompiled `.cwasm` bundles.** *Rejected:* ISA/Wasmtime-version fragility; a bundle would break across host upgrades. Ship `.wasm`, AOT on host.
- **(C, chosen) A `.tar.gz` bundle archive with a top-level aggregating manifest, one multiplexed `.wasm`, declarative defs, consumed by a new archive provider over the shared substrate.**

### Axis 2 alternatives

- **(A) Strict-only (no merge mode).** *Rejected:* breaks the backwards-compatible adoption path (today's behavior is merge/local-override) and ignores legitimate local-override needs during migration. A dial is required for staged adoption.
- **(B) Fold policy distribution into Axis 1 (a bundle that *is* the config).** *Rejected as the primary model:* merging policy/enabled/severity into a *resolved config* is semantically distinct from *loading check implementations*. Keeping them separate (but sharing the substrate) is cleaner; the canonical config *references* bundles by pin rather than *being* one.
- **(C, chosen) A versioned, integrity-pinned config pointer with a governance dial (merge / strict / allowlisted-local-override) and a pinned-per-repo + bot-bump rollout model; the canonical config is also the carrier for org-wide bundle pins.**

### Axis 3 alternatives

- **(A) Dynamic/loadable plugin framework** for gating + metrics. *Rejected (explicit non-goal):* over-generalization; these are proprietary host concerns needing ambient/network access, best injected at compile time.
- **(B) A bespoke `MetricsSink`/Observer trait as the primary metrics path.** *Deferred:* larger surface than needed. The host already owns timing and findings, and `tracing` + `tracing-opentelemetry` gives the embedder metrics with no new trait — once the instrumentation is upgraded to structured spans. Keep `MetricsSink` as a future option behind a concrete need.
- **(C) Boolean on/off gate.** *Rejected:* misses **Shadow** (run-but-don't-enforce), which is exactly what fleet ramp wants — data on what a check *would* flag before enforcing.
- **(D, chosen) A `CheckGate` trait returning `Enabled | Disabled | Shadow`, injected via a new builder, with an OSS `NoopGate`; metrics via upgraded structured `tracing` (no new trait in v1); gate decisions captured in run output for reproducibility.**

## Chosen approach

### Shared substrate

Introduce a single integrity-pinned, versioned pointer reused by Axes 1 and 2. Sketch (names illustrative; not a frozen API):

```rust
/// An integrity-pinned, versioned reference to an external artifact.
/// Used for both bundle archives (Axis 1) and canonical config (Axis 2).
struct PinnedSource {
    /// Logical source identity (e.g. "github.com/acme/checks" or a bundle id).
    source: String,
    /// Human-meaningful version/ref (semver-ish tag, release name, or git ref).
    version: String,
    /// Concrete fetch locator. v1: an HTTPS URL (often derivable from source+version).
    url: String,
    /// Integrity anchor: sha256 of the fetched artifact (bundle archive or config doc).
    sha256: String, // 64 lowercase hex; reuse existing validation in external/mod.rs
}
```

#### Transport and locator model

v1 transport is **HTTPS + sha256**, reusing the existing `reqwest` fetch with the established retry/backoff (5 attempts, exponential, 429/5xx retryable). The **sha256 is the trust anchor regardless of transport** — TLS authenticates the host, the digest authenticates the bytes. The `url` may be templated from `source` + `version` so an org can express "checks repo, release v3.4.1" without hand-writing URLs. Git-repo-ref and OCI transports are admissible later behind the same `PinnedSource` type (see deferred tasks).

#### Lockfile (`checkleft.lock`)

A repo-local `checkleft.lock` records each resolved pin: `source → version → sha256 → fetched_at`. This is the missing piece relative to today (the brief's "checkleft.lock / multitool sha256 pins" ethos — confirmed *not* to exist yet, so we invent it to match the ethos). The lock is what a bot-bump PR edits; it is what makes a CI run reproducible; and it is human-reviewable.

#### Fetch → verify → cache

A content-addressed cache keyed by sha256 (under the platform cache dir already used for sandbox hardlinks and `.cwasm`):

- Bundles: `<cache>/checkleft/bundles/<sha256>/…` (unpacked archive).
- Config: `<cache>/checkleft/config/<sha256>` (the verified canonical document).

Algorithm: resolve pointer → if sha256 present in cache, **use it with no network** (offline-friendly; warm CI cache) → else fetch `url`, **verify sha256** (reject on mismatch — supply-chain tamper), then unpack/store under the digest. Cold cache + offline + a populated lock that cannot be satisfied is a **hard error** (you cannot run unknown policy). This dovetails with Axis 3's fail-to-config-default for *gating* but is distinct: a missing *policy* document is fatal, a missing *experiment* signal is not.

Both axes consume this layer: Axis 1 turns a pinned bundle pointer into an unpacked, verified bundle directory; Axis 2 turns a pinned config pointer into a verified canonical document to merge.

### Axis 1 — extension bundles

#### Archive format and top-level manifest

A checkleft **bundle** is a `.tar.gz` containing:

- `bundle.toml` — the **top-level aggregating manifest** (new; this is the central gap). Fields: `bundle_id`, `version`, `api_version`, a list of `packages` (each pointing at a package manifest within the archive), and the single shared `component.wasm` reference.
- `component.wasm` — **one multiplexed component** for *all* component-tier checks in the bundle (LTO-deduped baseline, as the preinstalled-bundle proves). Declarative checks carry no wasm.
- Per-package manifests — the existing `ExternalCheckPackage` TOML/YAML. Component packages reference the shared `component.wasm` by relative path + `artifact_sha256`.

The whole archive is integrity-pinned by the substrate (the archive's sha256 in `checkleft.lock`); the per-package `artifact_sha256` over the shared `component.wasm` is retained as defense-in-depth (the runtime already computes the component sha256 from bytes).

#### Namespacing and collision rules

Bundle check IDs are namespaced by `bundle_id`: the fully-qualified id is `<bundle_id>/<check_id>`. Within a bundle, ids must be unique (validated at build time). Across bundles, collisions on a fully-qualified id are **rejected at load**, extending the existing `CompositeExternalCheckPackageProvider` collision-rejection (`bail!` on multi-provider resolve) — deterministic, no silent shadowing. Because the central config (Axis 2) chooses which bundles@versions are active, it is also where an org arbitrates which bundles are in scope.

#### Provider over an unpacked archive

Add a `BundleExternalCheckPackageProvider` that resolves a new `ExternalCheckImplementationRef::Bundle { bundle_id, check }` from an unpacked bundle directory (produced by the substrate's fetch→verify→unpack). It validates each component package's `artifact_sha256` against the shared `component.wasm` and integrates into the existing `Composite` provider so collision and resolution semantics are uniform.

#### Build tooling

Add a bazel rule (e.g. `checkleft_bundle`) that compiles the bundle crate to a multiplexed component (the `cdylib` → component path the preinstalled-bundle uses), gathers declarative package manifests, emits `bundle.toml` with computed sha256s, and produces the `.tar.gz`. **T1894** (export_checks!/`#[check]` ergonomics cleanup) is the proving ground for the bundle-author interface and is sequenced first.

#### wasm vs cwasm

Ship portable `.wasm`; AOT-compile to `.cwasm` on the host and cache (reusing the existing `.cwasm` cache). Do **not** ship `.cwasm`. External bundles are always wasm-and-cache regardless of how T1893 lands.

### Axis 2 — central control

#### Versioned config pointer

Replace the bare `settings.external_checks_url` with a `PinnedSource` (repo/locator + version + sha256), recorded in `checkleft.lock`, fetched and verified through the substrate, then merged via the existing `apply_external_checks_file` path (which already handles the per-check and settings merge and enforces the trust boundary). The bare-URL form can be supported transitionally but is deprecated in favor of the pinned pointer; chaining semantics (depth 8, cycle detection) carry over.

#### Governance dial

A root-config setting selects the trust posture:

| Mode | Local override of central policy? | Use case |
| --- | --- | --- |
| `merge` (default, today's behavior) | Yes (local applied after external) | Backwards-compatible adoption |
| `strict` | No — central pointer is authoritative for governed checks | Org-wide policy control |
| `allowlisted-local-override` | Only an explicit, auditable allowlist of checks/fields | Central control with sanctioned exceptions |

In `strict` mode, a repo's local CHECKS.yaml may **not** override policy/enabled/severity for centrally-governed checks; it is limited to the inherently-local surface (BYPASS tokens, ifchange/thenchange triggers, vendored-dir exclusions). In `allowlisted-local-override`, the override surface is itself declared and visible to the central config / audit, so exceptions are explicit rather than silent. The existing external-config trust boundary (no `exec_paths`, no `File` refs) is preserved in all modes.

#### Carrier for bundle pins

The canonical CHECKS document is the natural carrier for **org-wide bundle pins**: one pinned document distributes both *policy* and *which bundles@versions every repo runs*. This is the Axis 1 ↔ Axis 2 tie-in — the central config references bundles by `PinnedSource`, and consuming repos resolve those bundles through the same substrate.

#### Rollout model

- **Pinned-per-repo + bot-bump (recommended default):** each repo pins a version + sha256 in its `checkleft.lock`; a bot opens PRs to bump pins, giving staged, reviewable, revertable rollout with red/green attributable to a specific pin.
- **Floating/instant (opt-in):** a floating ref (e.g. `version = "main"`) propagates org-wide immediately with no per-repo PR, at the cost of reviewability and reproducibility. Available for fast-moving orgs or the central repo's own staging, but not the default.

### Check gating (CheckGate)

A compile-time trait consulted during resolution/execution, injected by the embedder via a new builder (`Runner::with_gate(...)`, mirroring `with_external`). Sketch:

```rust
enum GateDecision { Enabled, Disabled, Shadow }

trait CheckGate: Send + Sync {
    fn decide(&self, ctx: &GateContext) -> GateDecision; // ctx: check_id, impl ref, repo/changeset metadata
}
```

- **Shadow** (in scope, leaning yes) = run the check but **do not enforce**: findings are surfaced as info / record-only and never fail the run. This is what fleet ramp wants — you ramp on data about what a check *would* flag.
- **Seam placement:** post-resolution / pre-enforcement. The natural seam (per the runner audit) wraps the execution decision; for Shadow, the **always-run path is instrumented** and the decision affects *enforcement*, not *execution* (composition insight: run-always + emit-metrics + gate-enforcement).
- **OSS default:** `NoopGate` returns the static configured decision (effectively "config default"). The proprietary build injects an experiment-backed gate (remote enable/disable, fleet ramp).
- **Reproducibility:** remote gating makes a result depend on external state, so **gate decisions are captured in run output** (see below) for replay/pin. Red/green is always explainable.
- **Fail-open vs fail-closed:** when the experiment service is unreachable (CI offline), **fail to config-default** — the gate yields the statically configured enabled/severity and the run proceeds deterministically. (This is distinct from a missing *policy* document, which is fatal.)

To support reproducibility, extend the run output: add a per-check record of the gate decision and execution metadata (`elapsed_ms`, `compile_ms`) — either by widening `CheckResult` or adding a sibling run-metadata block. Today `CheckResult` is `{check_id, findings}` with no execution metadata, so this is additive.

### Metrics

Lean on the existing `tracing` instrumentation rather than a bespoke sink: the host already orchestrates execution, times each check (including the AOT compile), and collects findings by severity. The embedder attaches a `tracing-opentelemetry` subscriber ("we just emit otel"). The required work is to **upgrade the instrumentation to structured spans with stable names and fields** (`check_id`, `implementation_ref`, `elapsed_ms`, `compile_ms`, severity counts) so a generic OTel layer produces useful metrics without a checkleft-specific trait. A `MetricsSink`/Observer trait is held as a future option behind a concrete need; it is not in v1.

### Trust and security

#### WASM tier

Sandboxed by construction (WASI + `AccessScope` `modified_only` / `whole_repo` / `globs`). A bundle's shared `component.wasm` inherits this, and is sha256-pinned end-to-end (archive digest in the lock; per-package digest as defense-in-depth).

#### Declarative checks from external bundles

The declarative tier **executes host binaries** — the sharp edge for third-party bundles. Central config already refuses fs-reaching `exec_paths` and `File` refs; the **same reasoning applies to declarative checks from external bundles**. Decision: declarative checks from a *non-first-party* bundle are **off by default**; a repo (or the central config) must **explicitly opt a bundle into declarative execution**, and the binaries it may invoke are constrained by the bundle's declared `needs`, reviewed at adoption. First-party bundled declarative checks (e.g. `format/bazel`) remain trusted.

#### Remote-gating reproducibility

Gate decisions are captured in run output (above), so a result that depended on an experiment flag can be explained and replayed/pinned. A run that was gated Disabled/Shadow reads back as such; a reviewer is never left guessing why a check did or didn't enforce.

## Sequencing and rollout

High-level order (detailed, machine-readable graph in the final section):

1. **T1894 ergonomics** — the bundle-author interface; proving ground; first.
2. **Shared substrate** — versioned pointer + sha256 + fetch + cache + `checkleft.lock`.
3. **Axis 1** — bundle archive format + aggregating manifest + provider + build tooling.
4. **Axis 2** — central-config pointer + governance dial (rides the substrate); bundle pins carried in the canonical config.
5. **Axis 3** — host hooks (`CheckGate`, structured-tracing upgrade); independent of the substrate and parallelizable from the start.

Adoption is staged: a repo first switches its external-config pointer to a pinned `PinnedSource` in `merge` mode (no behavior change), then opts into `strict`/`allowlisted-local-override` when ready. Bundles roll out via bot-bumped pins in `checkleft.lock`.

## Risks / open questions

- **Governance default.** Defaulting to `merge` preserves backwards compatibility but means central control is opt-in; defaulting to `strict` is safer org-wide but is a breaking change for existing consumers. (See attentions manifest.)
- **Shadow scope for v1.** Shadow is the highest-value gate state for fleet ramp, but it adds an enforcement/observation split to the runner and a new output field. Worth confirming it lands in v1.
- **Fail-open vs fail-closed.** "Fail to config-default" is the proposed behavior when the experiment service is unreachable; an org with strict compliance needs might prefer fail-closed (disable) instead.
- **Declarative-from-bundle trust posture.** Off-by-default + explicit opt-in is proposed; some orgs may want a stricter allowlist-of-binaries model or to forbid third-party declarative checks entirely.
- **Transport for v1.** HTTPS + sha256 reuses existing machinery; git-repo-ref or OCI would be more idiomatic for some orgs but add infra. Confirm HTTPS-first.
- **Lockfile scope.** Should `checkleft.lock` also pin first-party/bundled checks (today embedded in the binary), or only external sources? Leaning external-only for v1.
- **Bundle `api_version` compatibility.** How does a host reject a bundle built against a newer WIT/`api_version` than it supports? Needs an explicit compatibility check at load (likely folds into the archive-format task).
- **Floating-ref reproducibility.** If floating refs are allowed at all, runs become non-reproducible by construction; should floating mode require recording the resolved sha256 into the run output even though it isn't pinned in the lock?

## Proposed implementation task breakdown

PR-sized tasks in dependency order. Effort hints: `trivial | small | medium | large`. Dependencies reference task names; "none" means it can start immediately. Tasks at the same depth with no inter-dependency may run in parallel.

### Depth 0 (no dependencies — fully parallel)

- **Bundle-author ergonomics cleanup (T1894).** Reduce `export_checks!` / `#[check]` boilerplate so authoring a bundle crate is ergonomic; this is the proving ground for the bundle-author interface and gates the build tooling. *Effort:* medium. *Depends on:* none.
- **Shared versioned-pointer + integrity + fetch + cache substrate.** Define `PinnedSource`, the content-addressed cache keyed by sha256, fetch→verify→unpack/store reusing the existing `reqwest` fetch + retry/backoff, `checkleft.lock` read/write, and offline behavior. Keystone for Axes 1 and 2. *Effort:* large. *Depends on:* none.
- **`CheckGate` trait + `NoopGate` + DI seam.** Define `CheckGate` / `GateDecision` (Enabled / Disabled / Shadow), add `Runner::with_gate(...)`, and wire the decision into the resolution/execution seam (enforcement split for Shadow). Ship OSS `NoopGate`. *Effort:* medium. *Depends on:* none.
- **Structured tracing/metrics instrumentation.** Convert the key `info!` events to structured spans with stable names and fields (`check_id`, `implementation_ref`, `elapsed_ms`, `compile_ms`, severity counts) so an embedder's `tracing-opentelemetry` layer yields useful metrics. *Effort:* small. *Depends on:* none.

*Parallelism: all four are independent and may run concurrently. The Axis 3 pair (`CheckGate`, tracing) has no dependency on the substrate at all.*

### Depth 1

- **Bundle archive format + aggregating manifest + namespacing/collision rules.** Define `bundle.toml`, fully-qualified `<bundle_id>/<check_id>` ids, one multiplexed `component.wasm` per bundle (declarative carries no wasm), the manifest parser/validator, an `api_version` compatibility check, and cross-bundle collision rejection extending `Composite`. *Effort:* medium. *Depends on:* none (pure format/parser; pairs naturally with the substrate).
- **Versioned central-config pointer.** Replace the bare `external_checks_url` with a `PinnedSource`, integrate with `checkleft.lock`, fetch+verify through the substrate, and merge via the existing `apply_external_checks_file`. *Effort:* medium. *Depends on:* Shared substrate.
- **Capture gate decisions + execution metadata in run output.** Extend `CheckResult` (or add a run-metadata block) to record per-check gate decision and `elapsed_ms` / `compile_ms` for reproducibility. *Effort:* medium. *Depends on:* `CheckGate` trait + `NoopGate` + DI seam.

*Parallelism: the Axis 1 format task, the Axis 2 pointer task, and the Axis 3 output task are on independent tracks and may run concurrently.*

### Depth 2

- **`BundleExternalCheckPackageProvider` over an unpacked archive.** New provider resolving `ExternalCheckImplementationRef::Bundle { bundle_id, check }` from an unpacked bundle dir; validate per-package `artifact_sha256` against the shared `component.wasm`; integrate into `Composite`. *Effort:* medium. *Depends on:* Bundle archive format; Shared substrate.
- **Bundle build tooling (`checkleft_bundle` bazel rule).** Compile the bundle crate → multiplexed component, gather declarative manifests, emit `bundle.toml` with computed sha256s, produce the `.tar.gz`. *Effort:* large. *Depends on:* Bundle-author ergonomics cleanup (T1894); Bundle archive format.
- **Governance dial (merge / strict / allowlisted-local-override).** Implement the three trust postures; `strict` forbids local policy override for governed checks while preserving the inherently-local surface; `allowlisted-local-override` declares an auditable exception set. *Effort:* medium. *Depends on:* Versioned central-config pointer.

*Parallelism: the two Axis 1 tasks may run concurrently with the Axis 2 governance task.*

### Depth 3

- **Bundle pins carried in the canonical config + bot-bump rollout.** The central config references bundles by `PinnedSource`; a bot opens bump PRs editing `checkleft.lock`; consuming repos resolve bundles via the substrate. *Effort:* medium. *Depends on:* Versioned central-config pointer; `BundleExternalCheckPackageProvider`; Shared substrate.
- **Declarative-from-external-bundle trust gate.** External-bundle declarative checks off by default; explicit per-bundle opt-in; invocations constrained to the bundle's declared `needs`. *Effort:* medium. *Depends on:* `BundleExternalCheckPackageProvider`.

### Future / not a v1 blocker

- **Experiment-backed `CheckGate` (proprietary, closed embedding).** Implement the fleet-ramp gate (remote enable/disable, fail-to-config-default when offline) in the closed build. Lives in the proprietary embedding, not mono OSS. *Effort:* medium. *Depends on:* `CheckGate` trait; Capture gate decisions in run output. *Status:* future / not a v1 blocker (out of OSS tree).
- **`MetricsSink`/Observer trait.** Only if `tracing` + `tracing-opentelemetry` proves insufficient for a concrete embedder need. *Effort:* medium. *Status:* future / not a v1 blocker.
- **OCI/registry bundle transport.** Content-addressable distribution behind the same `PinnedSource`. *Effort:* large. *Status:* future / not a v1 blocker.
- **Git-repo-ref transport for pointers.** Resolve a git ref to a commit and fetch a path, as an alternative to templated HTTPS URLs. *Effort:* medium. *Status:* future / not a v1 blocker.
- **Floating/instant rollout polish.** Beyond the opt-in floating ref (pinned + bot-bump is the v1 default). *Effort:* small. *Status:* future / not a v1 blocker.
- **`regex` / `sarif` declarative transforms.** Already reserved-but-unimplemented in the declarative tier; out of scope for this project. *Status:* future / not a v1 blocker.
- **T1893 (cwasm cold-start: precompiled-in-binary vs native bundled).** Separate project. External bundles are always wasm-and-cache regardless of how it lands. *Status:* future / separate project.
