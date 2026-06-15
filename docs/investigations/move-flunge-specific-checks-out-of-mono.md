# Moving the flunge-specific checkleft checks (`frontend_no_legacy_api`, `api_breaking_surface`) out of mono

Status: investigation / recommendation. No code changes in this PR.
Author: Boss worker (`exec_18b92bc4bd276da0_14f`).
Date: 2026-06-14.

## TL;DR

Two bundled checkleft checks — `frontend-no-legacy-api` and `api-breaking-surface` — live in mono (`tools/checkleft/src/checks/`) but are used **only by flunge**; mono's own `CHECKS.yaml` configures neither. They are the brief's subjects.

The central, somewhat surprising finding is that **the "flunge-specificity" is already entirely in flunge's config, not in the Rust code.** Both checks are already config-driven and generic-in-shape. So the real question is not "how do we ship custom flunge logic from flunge" — there is almost no custom logic — it is "how do we stop baking a flunge-flavoured check id into the shared binary."

Recommendation: **approach (a), GENERALIZE, for both — but they take two different sub-forms, and they do _not_ collapse into one shared check.**

- **`frontend-no-legacy-api` is a redundant one-off.** It is a strict subset of the already-generic, already-bundled `forbidden-imports-deps` check, which flunge already uses in the same `CHECKS.yaml`. **Delete it from mono** and re-express flunge's policy as a `forbidden-imports-deps` rule in flunge's own `CHECKS.yaml`. Zero new generic check needs to be written; the generalization already exists.
- **`api-breaking-surface` is already a coherent generic check** ("when files matching `trigger_globs` change, require a companion change matching `required_globs`"). Its implementation contains zero flunge-specifics; only its _name_ encodes a flunge domain concept. **Keep the check in checkleft, rename it to a domain-neutral id** (e.g. `require-companion-change`), document it as generic, and let flunge keep supplying its globs via config (it already does).

Approaches (b) custom WASI check and (c) standalone CLI are **not recommended** for these two checks. (b) is not feasible for an external repo without first building new infrastructure (the SDK is not published, the build path is mono-internal Bazel only, there is no remote artifact distribution). (c) is technically runnable on flunge's prebuilt binary but would reintroduce exactly the build/distribution complexity flunge **deliberately removed** in its move to a pinned-prebuilt, bundled-checks-only model — and it is strictly worse than (a) here because (a) needs no new artifact at all. Establishing checkleft's first external-custom-check is genuinely valuable, but these two checks are the wrong vehicle for it precisely because both generalize cleanly. That capability should be proven on a check that is _irreducibly_ specific.

---

## 1. What the two checks actually do today, and why they are "flunge-specific"

### 1.1 `frontend-no-legacy-api`

Source: `tools/checkleft/src/checks/frontend_no_legacy_api.rs`. Registered as a built-in at `tools/checkleft/src/checks/mod.rs:7,30`; has a built-in bypass mapping (`BYPASS_FRONTEND_NO_LEGACY_API`) at `tools/checkleft/src/bypass.rs:147,156,181-182`; documented as a canned check at `tools/checkleft/userdoc/docs/canned-checks.md:314`.

Behaviour (`frontend_no_legacy_api.rs:31-85`):

- For each changed (non-deleted) file under `frontend/src` with extension `.ts`/`.tsx` (`is_frontend_source_file`, lines 127-132)…
- …scan each line for an ES import (`^\s*import\b[^;]*\bfrom\s*["']([^"']+)["']`, line 32)…
- …normalize the module specifier (strip `./` / `../` prefixes, trim slashes; lines 134-143)…
- …and flag it if the normalized specifier **ends with** any configured `legacy_modules` entry (lines 59-65).

Config (`legacy_modules`, `severity`, `remediation`; lines 88-96). It is already fully config-driven: nothing in the Rust hard-codes flunge module names.

Why it is "flunge-specific": the **only** thing tying it to flunge is its configured `legacy_modules` values — and those live in flunge, not mono. flunge's `CHECKS.yaml` (fresh checkout `brianduff/flunge`) configures:

```yaml
- id: frontend-no-legacy-api
  check: frontend-no-legacy-api
  config:
    legacy_modules:
      - "api/fencingtracker"
      - "api/usafencing"
    remediation: "Use supported frontend API modules under frontend/src/api/."
```

So the flunge-flavoured part (the deprecated fencing API modules) already lives in flunge. What lives in mono is a generic-ish "forbidden import scan over a path subset" — and that is the problem: it is a **narrow, single-purpose re-implementation** of a capability checkleft already has generically (see §3).

### 1.2 `api-breaking-surface`

Source: `tools/checkleft/src/checks/api_breaking_surface.rs`. Registered at `tools/checkleft/src/checks/mod.rs:1,22`.

Behaviour (`api_breaking_surface.rs:31-74`):

- Over the changed-file set, compute two booleans/sets: did any changed file match `trigger_globs`, and did any changed file match `required_globs`?
- If any trigger file changed **and** no required file changed, emit one finding per trigger file. Otherwise pass.

Config: `trigger_globs`, `required_globs`, `message`, `remediation` (lines 77-87). This is a **completely generic** "if you touch X you must also touch Y" / companion-file-required check. There is nothing about APIs, backends, or fencing in the implementation — that is purely the check's _name_ and its _description_ string ("requires API-facing backend changes to include configured documentation/version marker updates", line 21).

flunge's `CHECKS.yaml` supplies all the specifics:

```yaml
- id: api-breaking-surface
  policy: { allow_bypass: true }
  config:
    trigger_globs: [ "backend/blob/src/app.rs", "backend/blob/src/v2/mod.rs", ... ]
    required_globs: [ "docs/backend.md", "docs/product-specs/**" ]
    message: "Potential backend API surface change without docs update."
    remediation: "When API behavior changes, update docs/backend.md or a relevant product spec in this PR."
```

Why it is "flunge-specific": again, **only via config** (the `backend/blob/...` globs and `docs/backend.md`), which already lives in flunge. The mono code is a clean generic primitive that happens to carry a flunge-domain name.

### 1.3 The common situation

Both checks share the same shape of problem:

| | flunge-specific logic in Rust? | config (the flunge part) location | used by mono itself? |
|---|---|---|---|
| `frontend-no-legacy-api` | none — generic import scan | flunge `CHECKS.yaml` | no |
| `api-breaking-surface` | none — generic companion-file rule | flunge `CHECKS.yaml` | no |

Confirmed: neither id appears in any mono `CHECKS.yaml` (root config configures `docs-link-integrity`, `no-generated-artifacts`, `file/size`, `format/bazel`, `lint/bazel`, `rust-test-rule-coverage`, `todo-expiry`, `repo-visibility`, `format/rust`, `lint/rust` — and nothing else). In mono they exist only as framework code + tests + the doc examples (`frontend-no-legacy-api` is in fact checkleft's running _example_ of a repo-local/external check: `tools/checkleft/docs/designs/bazel-repo-local-checks.md:193-203,301-303`; `tools/checkleft/userdoc/docs/checks-config.md:155-165`).

This matters for the recommendation: we are not relocating bespoke flunge code. We are removing two flunge-flavoured _names_ from the shared binary, while the actual policy data already lives in flunge.

---

## 2. How a flunge-specific check could be owned and run from flunge — the mechanism landscape

checkleft (mono, crate `checkleft` `0.1.0-alpha.8`, "experimental") supports three families of check provenance (`tools/checkleft/README.md` "Architecture"; `tools/checkleft/src/external/`):

1. **Bundled built-in** — compiled into the binary, resolved by id, no `implementation:` line. Both subject checks are here today. flunge consumes these because it pins a prebuilt checkleft binary (see below).
2. **External "component" check** — a WebAssembly Component Model `.wasm`, loaded from disk (or embedded for bundled), run under wasmtime in a capability-FS sandbox (`src/external/runtime.rs`). This is approach (b).
3. **External "declarative" check** — checkleft spawns a declared binary/script with templated argv and parses its output (`src/external/declarative/`). This is approach (c).

**Crucial distribution constraint (drives everything below).** flunge does **not** build checkleft from source. It consumes a **pinned, prebuilt binary** downloaded from `spinyfin/mono` GitHub releases via a gradlew/bazelisk-style bootstrap (`flunge/bin/checkleft-bootstrap.sh`, `flunge/bin/README.md:5-32`), sha256-verified fail-closed, with **intentionally no build-from-source fallback**. Today, per `flunge/docs/checks.md:28`, "Every check Flunge configures is a bundled first-party check that ships inside the checkleft binary, so no `implementation:` line and no generated index are needed."

flunge arrived here on purpose: it **previously** had custom external-check packages — JS checks componentized to WASM via `@bytecodealliance/componentize-js`/`jco`, with `tools/checks_bazel` / `tools/checks_js_componentizer` build helpers (flunge `docs/design-docs/sandboxed-polyglot-checks.md`) — and **removed all of it**. Rationale (`flunge/bin/README.md:24-38`): "Building the checks binary from source on every checkout was slow and coupled the checks toolchain to the full Bazel graph… There is intentionally no build-from-source fallback… the previous custom external-check packages … were removed." This history is the single most important input to evaluating (b)/(c): flunge tried the external-custom-check road and deliberately walked back from it.

---

## 3. Approach (a): GENERALIZE — assessment and concrete proposal

checkleft's own design principle pushes hard toward this: prefer evolving/generalizing **one** shared, typed built-in over proliferating narrow one-offs.

- `tools/checkleft/docs/designs/code-patterns.md:33-34`: add "one built-in language-aware code-pattern check rather than a family of narrow language-specific one-offs."
- `tools/checkleft/docs/designs/forbidden-paths-evolution.md:21-26`: "Rather than adding a second overlapping check, we should make this an evolution of `forbidden-paths`… Keep one built-in check for path-based repository policy."
- `tools/checkleft/docs/designs/bazel-policy-checks.md:400-405`: repo-specific one-offs are steered toward the repo-local external mechanism, but "the baseline … families belong in the built-in set."

The principle also bounds _how_ generic to go: keep config typed and policy-shaped, **not** an open-ended query DSL (`bazel-policy-checks.md:385-398`; `code-patterns.md:47,292-298`). Both proposals below respect that — they reuse typed, narrow config, not a generic engine.

### 3.1 `frontend-no-legacy-api` → the existing generic `forbidden-imports-deps` (delete the one-off)

A coherent generalization not only exists, it is **already shipped and already used by flunge**. `tools/checkleft/src/checks/forbidden_imports_deps.rs` is a generic content-regex check:

- per-rule `pattern` (regex, matched per line), `message`, `severity`, `remediation`;
- per-rule `include_globs` / `exclude_globs` (alias `exclude_files`) path filters;
- skips deleted files, reads file content, scans every line (lines 41-87, 99-111).

`frontend-no-legacy-api` is a **strict subset** of this: a content-regex over a path subset. flunge already runs `forbidden-imports-deps` in the very same `CHECKS.yaml` (the `fetch(url(` rule), so this is a proven, in-use pattern — not a new dependency.

Concrete faithful translation (drop-in for flunge's `CHECKS.yaml`):

```yaml
- id: frontend-no-legacy-api          # keep the friendly id/label
  check: forbidden-imports-deps        # but run the generic check
  config:
    rules:
      - pattern: '^\s*import\b[^;]*\bfrom\s+["''][^"'']*api/(fencingtracker|usafencing)["'']'
        message: "import from deprecated frontend API module"
        include_globs: ["frontend/src/**/*.ts", "frontend/src/**/*.tsx"]
        remediation: "Use supported frontend API modules under frontend/src/api/."
```

Notes on fidelity:

- The original matches imports whose normalized specifier **ends with** a legacy module (after stripping `./`/`../`). The `[^"']*api/(…)["']` form is anchored on the closing quote, so it matches `./api/fencingtracker`, `../../api/fencingtracker`, `@/api/fencingtracker`, `frontend/src/api/fencingtracker`, etc. — equivalent to the original's `ends_with`. If a tighter path-boundary match is wanted, anchor as `["'](?:[^"']*/)?api/(…)["']`.
- `include_globs` reproduces the hard-coded `frontend/src` + `.ts`/`.tsx` filter (`is_frontend_source_file`).
- This could also simply be **added as another rule to flunge's existing `forbidden-imports-deps` block** rather than a separate stanza.

Outcome: the `frontend-no-legacy-api` bundled check **leaves mono entirely** (deleted), and the policy lands in flunge as ordinary config of a generic check flunge already runs. This is the textbook application of checkleft's de-duplication principle.

### 3.2 `api-breaking-surface` → already generic; rename + reframe, keep in checkleft

Here the "generalization" is not something to build — **it already is the implementation.** The check's mechanism ("touching `trigger_globs` requires a companion change in `required_globs`") is domain-neutral and broadly reusable (docs-must-accompany-API-change, schema-must-accompany-migration, changelog-must-accompany-release, etc.). The only flunge-coupling is cosmetic: the id `api-breaking-surface` and the description string.

Proposal: **rename the check to a generic id** — e.g. `require-companion-change` (alternatives: `coupled-change`, `change-requires-companion`) — update its description to the generic statement, and keep it as a first-class generic checkleft built-in. flunge keeps its own `id:`/label and its globs; it just points `check:` at the new generic id.

This answers the brief's "does a coherent generalization exist?" with: **yes, it already exists in the code; what's missing is only a generic name and framing.** The check does not need to _leave_ checkleft — leaving would just re-create a one-off elsewhere. The flunge-specific _policy_ already lives in flunge's config; reframing the check as generic removes the "flunge concept baked into mono" smell while keeping a genuinely reusable primitive in the shared set.

### 3.3 Do the two checks share a single common solution?

**No — and they should not be forced to.** They are structurally different (a per-line content/regex scan vs. a changed-path-set companion requirement). They do not collapse into one generic check, and inventing a meta-check to host both would itself violate the "don't build an over-generic engine" guidance. They share a common _strategy_ (approach (a)), realized two ways: one folds into an existing generic check and disappears; the other is already generic and just needs a neutral name.

---

## 4. Approach (b): custom WASI/Component-Model check authored from flunge

How it would work: flunge writes a Rust crate using the guest SDK (`#[check(...)]` + `export_checks!`; `tools/checkleft/sdk/src/lib.rs`, example `tools/checkleft/sdk/examples/trivial-check/`), compiles it to a Component-Model `.wasm`, references it from `CHECKS.yaml` via a manifest pinning `artifact_path` + `artifact_sha256`, and checkleft loads/runs it under wasmtime (`tools/checkleft/src/external/runtime.rs:330-381`).

The host runtime is **real and shipped** in the prebuilt binary (`component-v1`, `src/external/runtime.rs:437-463`; default executor wired at `src/main.rs:489-493`). The blocker is the **authoring/build/distribution toolchain**, which is mono-internal:

- **The SDK is not published.** `checkleft-check-sdk` / `-sdk-macro` are depended on **by path** (`sdk/examples/trivial-check/Cargo.toml`), not via crates.io. An external repo has no crate to depend on. (Note: the `checkleft` _binary_ is installable via `cargo install checkleft`/release downloads, but the _guest SDK_ is not.)
- **No external build path.** The only supported componentization is the mono-internal Bazel rule `rust_wasm_component` + a hermetic `wasm-tools` toolchain + a `wasm32-wasip2` platform (`tools/checkleft/wasm/defs.bzl`, `sdk/examples/trivial-check/BUILD.bazel`). The component-model design doc is explicit that Bazel is the required path and "you can build it with cargo" is not accepted.
- **WIT version coupling.** The guest embeds `wit/check.wit` at proc-macro compile time (`sdk-macro/src/lib.rs:18`); there is no mechanism to vend the matching WIT to an out-of-tree author per pinned checkleft release.
- **No remote artifact distribution.** The host loads `.wasm` only from local disk or, for _bundled_ checks, from bytes embedded into the checkleft binary at checkleft's build time (`runtime.rs:345-351`; `src/external/bundled.rs:78-140`). There is no registry/remote fetch for an arbitrary external `.wasm`.

Net for flunge specifically: in a no-build-from-source world, flunge would still have to **build the `.wasm` itself** (which it cannot, per the gaps above) and check it into its tree. This is precisely the build/componentize/Bazel-coupling burden flunge **already removed** (§2). 

Gaps that would have to be built first: publish the SDK + a versioned WIT to crates.io; provide a supported non-Bazel (cargo/`cargo component`) build path; add remote component distribution or an out-of-tree build/release flow. **Verdict: not viable for an external repo today**, and a poor fit even if built, for two checks that need no custom logic.

---

## 5. Approach (c): standalone CLI/script invoked via the declarative runtime

How it would work: flunge writes a declarative manifest (`mode = "declarative"`, `runtime = "declarative-v1"`) describing a binary/script, its argv template (`{{files}}`/`{{file}}`/`{{repo_root}}`), an exit-code→outcome map, and an output transform; checkleft spawns it and parses findings (`tools/checkleft/src/external/declarative/{mod.rs,executor.rs}`). It is wired in `CHECKS.yaml` either via `implementation: <manifest>` (`src/external/provider.rs:44-58`) or via a `check_definitions.exec_paths` directory + name resolution (`src/config.rs:563-660`).

Maturity: the declarative runtime is **fully implemented and shipped in the prebuilt binary** — no checkleft change needed. The contract is spawn-based (not stdin/JSON): templated argv, `current_dir = repo_root`, a **mandatory** exit-code map with a `default` (so a crash surfaces as error, not "clean"), and transforms `passthrough` (binary emits checkleft findings JSON), `json` (jq-subset), or `linelist` (`mod.rs:160-190,509-580`; `regex`/`sarif` are reserved/unimplemented).

So (c) is **technically runnable on flunge's prebuilt binary** — this is the realistic form of "flunge's first external check." The caveat is distribution of the tool itself: checkleft only _spawns_ it (`executor.rs:368`). The `bazel` binary binding (`resolve.rs:159-220`) needs a Bazel workspace and is unusable for prebuilt-consuming flunge; flunge must use a `path` binding to either a **checked-in script** (zero build — viable) or a binary it builds and places via its **own** toolchain (reintroduces build/distribution work).

Why it is still **not recommended for these two checks**:

- For `frontend-no-legacy-api` and `api-breaking-surface`, a CLI/script would re-implement, in flunge, logic checkleft **already provides generically** — strictly more code to own and test, for no behavioural gain.
- Even the zero-build script form adds a moving part (a maintained script + a manifest + exec_paths wiring) versus approach (a)'s pure config diff.
- It nudges flunge back toward the external-check/build-coupling posture it intentionally abandoned (§2).

(c) becomes the right answer the day flunge needs a check whose logic genuinely **cannot** be expressed by a generic checkleft check — see §7.

---

## 6. Recommendation

**Adopt approach (a) for both checks**, in the two sub-forms of §3:

1. **`frontend-no-legacy-api`: delete the bundled one-off from mono; re-express as a `forbidden-imports-deps` rule in flunge's `CHECKS.yaml`** (§3.1). It is a strict subset of an existing generic check flunge already runs.
2. **`api-breaking-surface`: keep the (already generic) check in checkleft, rename it to a domain-neutral id (`require-companion-change`), document it generically; flunge updates `check:` and keeps its globs** (§3.2).

Rationale, judged on the brief's stated merits:

- **Maintainability / who owns it:** (a) leaves flunge's _policy_ (module names, globs, messages) entirely in flunge config — where it already is — and leaves only genuinely generic, reusable code in checkleft. No new artifacts, builds, SDK pins, or manifests to maintain. (b)/(c) add an owned build/distribution surface to flunge for no logic that isn't already generic.
- **Alignment with checkleft's generic-reuse principle:** (a) _is_ the principle (`forbidden-paths-evolution.md`, `code-patterns.md`). `frontend-no-legacy-api` is the exact "narrow one-off overlapping a generic check" the principle says to collapse; `api-breaking-surface` is a generic primitive mis-labelled as a one-off.
- **Build & distribution:** (a) needs none. (c) needs flunge to ship a tool; (b) needs flunge to build a `.wasm` it currently cannot.
- **SDK / runtime maturity:** the declarative runtime is ready (c), but the WASM authoring/distribution toolchain is **not** ready for an external repo (b) (§4). Neither maturity question even arises under (a).
- **Value of establishing the external-custom-check pattern:** real, but not worth forcing onto two checks that generalize cleanly. Doing so would be capability-driven, not need-driven, and would re-introduce complexity flunge consciously shed. Prove the pattern on an irreducibly-specific check (§7).

### Migration path

**`frontend-no-legacy-api` (ordering matters — flunge pins a specific checkleft version):**

1. In **flunge**: replace the `frontend-no-legacy-api` stanza with the `forbidden-imports-deps` rule from §3.1 (or fold it into the existing `forbidden-imports-deps` block). Both the old bundled check and `forbidden-imports-deps` exist in the currently-pinned binary, so this is safe to land first. Verify with `bin/checkleft run`.
2. In **mono/checkleft**: delete `src/checks/frontend_no_legacy_api.rs`; remove its registration (`src/checks/mod.rs:7,30`), its bypass mapping (`src/bypass.rs:147,156,181-182`), the external-check **test fixture** reference (`src/external/tests.rs:142`), and the canned-check entry (`userdoc/docs/canned-checks.md:314`). Update the design/userdoc **examples** that use it (`docs/designs/bazel-repo-local-checks.md`, `userdoc/docs/checks-config.md:155-165`) to a still-extant example. Cut a new checkleft release.
3. In **flunge**: bump `bin/checkleft.lock` to the release from step 2. By then flunge no longer references the removed id, so nothing breaks.
4. What happens to the id/config: the bundled id `frontend-no-legacy-api` disappears from checkleft. flunge retains a check whose `id:` (friendly label, optionally still `frontend-no-legacy-api`) maps to `check: forbidden-imports-deps`; the `legacy_modules` list becomes the regex alternation. flunge currently sets no bypass for it, so no bypass-name migration is required.

**`api-breaking-surface` (rename, keep the code):**

1. In **mono/checkleft**: register the same `ConfiguredCheck` under the new generic id (`require-companion-change`); keep `api-breaking-surface` as a **deprecated alias** for one release window. Update the description string and `userdoc` to the generic framing.
2. In **flunge**: change `check: api-breaking-surface` → `check: require-companion-change` (keeping `id: api-breaking-surface` as the local label, plus all existing globs/policy). Land after a checkleft release that knows the new id.
3. Remove the deprecated alias from checkleft after flunge has migrated.
4. What happens to the id/config: flunge's config is unchanged in substance (same globs/message/remediation); only the referenced `check:` id changes. The check stays a checkleft built-in — now legitimately generic rather than flunge-named.

(If a rename is judged not worth the churn, a strictly-smaller variant is acceptable: keep the id `api-breaking-surface` but re-document it as a generic companion-change check. The correctness point — "it is already the generalization, keep it in checkleft" — holds either way; the rename is polish for the generic-reuse principle.)

---

## 7. When (b)/(c) _would_ be the right call (deferred, not rejected forever)

The external-custom-check capability is worth establishing — just not on these two. The right trigger is a flunge check whose logic is **irreducibly specific**: it cannot be expressed as a regex/glob/companion rule and needs real parsing or domain computation (e.g. validating a flunge-specific schema, cross-referencing generated fencing-bout data, AST-level TS analysis beyond line regexes). At that point:

- **Prefer (c) with a checked-in script** (`path`-bound, `passthrough`/`json` transform) — it runs on the prebuilt binary today with no checkleft change and no compiled-artifact distribution. This is the lowest-friction way to make flunge checkleft's first external consumer.
- **Pursue (b)** only after the SDK + versioned WIT are published and a non-Bazel build path exists (§4 gap list); then it becomes attractive for sandboxed, performance-sensitive, or richly-typed checks.

These are recorded as follow-ups, not part of this change.

---

## Appendix: key citations

- Subject checks: `tools/checkleft/src/checks/frontend_no_legacy_api.rs`; `tools/checkleft/src/checks/api_breaking_surface.rs`; registry `tools/checkleft/src/checks/mod.rs`.
- Generic check that subsumes (a.1): `tools/checkleft/src/checks/forbidden_imports_deps.rs`.
- Neither subject check used in mono `CHECKS.yaml`; both used in flunge `CHECKS.yaml`.
- External runtimes: `tools/checkleft/src/external/{mod.rs,runtime.rs,bundled.rs,provider.rs,declarative/}`; config resolution `tools/checkleft/src/config.rs:563-660`; executor wiring `tools/checkleft/src/main.rs:444-493`.
- SDK / WASM authoring: `tools/checkleft/sdk/{Cargo.toml,src/lib.rs}`, `tools/checkleft/sdk-macro/src/lib.rs`, `tools/checkleft/wit/check.wit`, `tools/checkleft/wasm/defs.bzl`, `tools/checkleft/docs/buildkite-release-setup.md`.
- flunge distribution model & external-check backout: `flunge/bin/README.md:5-38`, `flunge/docs/checks.md:24-28`, `flunge/docs/design-docs/sandboxed-polyglot-checks.md`.
- Principle (generic over one-offs): `tools/checkleft/docs/designs/{code-patterns.md,forbidden-paths-evolution.md,bazel-policy-checks.md}`.
- `frontend-no-legacy-api` as the design's repo-local-check example: `tools/checkleft/docs/designs/bazel-repo-local-checks.md:193-203,301-303`; `tools/checkleft/userdoc/docs/checks-config.md:155-165`.
