# Feasibility: source `checkleft` from a prebuilts repo like `buildifier`

**Status:** investigation only — no code changes. Deliverable is this writeup.
**Date:** 2026-06-03
**Scope:** Can `checkleft` be obtained as a prebuilt binary the way `buildifier` is, instead of being built from source in-repo? Produce a concrete recipe and a go/no-go.

All claims below were verified against the current checkout at `/Users/brianduff/Documents/dev/workspaces/mono-agent-002`, not from memory. File citations are inline.

---

## TL;DR

**Recommendation: No-go on a full buildifier-style prebuilts pipeline for now.** `buildifier` is a third-party, single-file, statically-linked Go binary published by an upstream project and rarely bumped. `checkleft` is a ~64 MB in-repo Rust binary with C build dependencies (tree-sitter, wasmtime/cranelift) *and* a Node runtime dependency, that changes frequently and has no upstream publisher. The two key mismatches are (1) we'd have to build and publish every platform ourselves, and (2) because `checkleft` lives in this repo and changes often, a prebuilt binary is perpetually at risk of lagging `HEAD` — a problem `buildifier` simply does not have. Bazel's build cache already amortizes the compile cost that prebuilding would save.

If the real pain is "cold first build is slow," the cheap fix is the Bazel remote cache (already wired via `--config=ci`), not a prebuilts pipeline. A prebuilt only earns its keep if we need to run `checkleft` *outside* this monorepo (a repo with no Rust toolchain) — and even then the minimal mechanism is `http_file` + `select()` over the **already-existing** `checkleft-v*` tags, not a new bespoke module.

---

## 1. How `buildifier`-from-prebuilts works today

`buildifier` is **not** sourced from a prebuilts repo we own. It comes from a public Bazel Central Registry (BCR) module:

- `MODULE.bazel` declares it as a registry dependency:
  ```python
  bazel_dep(name = "buildifier_prebuilt", version = "7.3.1")
  ```
- The `buildifier_prebuilt` module (resolved from BCR) downloads precompiled binaries from the upstream **`github.com/bazelbuild/buildtools`** GitHub releases. From `MODULE.bazel.lock`, the pinned per-platform download URLs are:
  ```
  buildtools/releases/download/v7.3.1/buildifier-darwin-amd64
  buildtools/releases/download/v7.3.1/buildifier-darwin-arm64
  buildtools/releases/download/v7.3.1/buildifier-linux-amd64
  buildtools/releases/download/v7.3.1/buildifier-linux-arm64
  buildtools/releases/download/v7.3.1/buildifier-windows-amd64.exe
  ```
  (plus the matching `buildozer-*` set). Each lands in a per-platform repo such as `buildifier_darwin_arm64`, `buildifier_linux_amd64`.

**Platform selection.** The module registers Bazel toolchains and exposes a single alias, `@buildifier_prebuilt//:buildifier`, that resolves to the correct per-platform binary via toolchain/`config_setting` resolution against the exec platform (macOS arm64 vs Linux x86_64). Consumers never name the platform — they just reference the alias.

**Version / SHA pinning.** The version (`7.3.1`) is pinned in `MODULE.bazel`. The per-platform `sha256` integrity hashes live in the upstream registry module and are frozen into `MODULE.bazel.lock`. Bumping = change the `bazel_dep` version and re-resolve the lockfile.

**Who publishes the binary.** Nobody in this repo. Upstream `bazelbuild/buildtools` CI compiles the Go binaries and attaches them to its GitHub releases. The mono repo is a pure *consumer* of an already-published, third-party artifact.

**How it's wired into the repo's tooling.**
- `REPOBIN.toml` maps the `buildifier` tool name to the prebuilt target:
  ```toml
  [tools.buildifier]
  target = "@buildifier_prebuilt//:buildifier"
  ```
- `checkleft`'s own buildifier check defaults to the same target — see `tools/checkleft/src/checks/buildifier.rs`: "If neither is configured, `buildifier_target` defaults to `@buildifier_prebuilt//:buildifier`."

### The closer in-house analog: `ghostty_kit`

Because `checkleft` is **not** on any public registry, the more relevant template is the one prebuilt artifact this repo already owns and publishes itself: the GhosttyKit xcframework.

- `MODULE.bazel` (bottom) consumes it via `http_archive`, sha256-pinned, from a GitHub-releases prebuilts repo we control:
  ```python
  http_archive(
      name = "ghostty_kit",
      build_file = "//tools/boss/app-macos:ghosttykit.BUILD",
      sha256 = "210c6914395c2222b76857cd63853916313a5c4a10964b934e38f84b8cf64a06",
      urls = ["https://github.com/spinyfin/ghostty-prebuilts/releases/download/ghosttykit-b0f827665/GhosttyKit-b0f827665.tar.gz"],
  )
  ```
- The publish recipe is a manual runbook, `tools/boss/docs/runbooks/update-ghostty-prebuilt.md`: build the artifact locally → `tar` it → `gh release create --repo spinyfin/ghostty-prebuilts ...` → update the `urls`/`sha256` in `MODULE.bazel` → open a PR.

This — "we build it, publish to GitHub releases, http_archive it with a pinned sha256" — is the actual shape any `checkleft` prebuilt would take, since there is no upstream publisher to lean on.

---

## 2. What `checkleft` is today

`checkleft` is a **Rust binary built from source inside this repo**, not a downloaded artifact.

- Build target: `//tools/checkleft:checkleft`, a `rust_binary` in `tools/checkleft/BUILD.bazel` (crate root `src/main.rs`, plus a `checkleft_lib` library and test targets).
- Crate: `checkleft` v`0.1.0-alpha.8` (`tools/checkleft/Cargo.toml`).
- `REPOBIN.toml` maps it to the source target (contrast with `buildifier`):
  ```toml
  [tools.checkleft]
  target = "//tools/checkleft:checkleft"
  [tools.checks]
  target = "//tools/checkleft:checkleft"
  ```

**How it's invoked.** Always via `repobin`, which builds the Bazel target and then `exec`s the binary with the *caller's* cwd preserved (so `CHECKS.*` config files are found — `bazel run` would set cwd to the runfiles tree and miss them). See:
- `.buildkite/steps/checks.sh` — PR pipeline, diff-scoped: `bin/checkleft run`.
- `.buildkite/steps/integrity-checkleft.sh` — integrity pipeline, full repo: `bin/checkleft run --all`, run on a **Linux** agent.
- Both first `bazel build //tools/repobin:repobin`, then `repobin install --bin-dir bin/ --no-defaults`, then call `bin/checkleft`.

**Platforms it must run on.** Two, per `.buildkite/pipeline-integrity.yml`:
- **macOS arm64** — developer machines and the `macos-arm64` agent (Zakalwe-1).
- **Linux x86_64** — the `bazel-any` queue runs `integrity-checkleft` and the PR `checks` step.

This is the same two-platform matrix buildifier needs (minus windows and linux-arm64).

**Build inputs / dependency profile** (`tools/checkleft/Cargo.toml`) — this is where it diverges sharply from buildifier:
- `wasmtime = "42.0.1"` with `cranelift` + `component-model` + `runtime` — a full WebAssembly JIT. This dominates binary size and compile time.
- `tree-sitter`, `tree-sitter-java`, `tree-sitter-starlark` — C grammars; require a **C toolchain** at build time.
- `reqwest` + `rustls`, `syn`, `tokio`, `serde`, `globset`, `regex`, etc.
- The built binary is **~64 MB** (measured: `bazel-out/darwin_arm64-fastbuild/bin/tools/checkleft/checkleft`). For comparison, a `buildifier` release binary is a single ~7 MB static Go executable.

**Runtime dependency beyond the binary (the important gotcha).** `checkleft` is **not** a hermetic single binary:
- For JS/TS external checks in *source mode* it shells out to a pinned Node/pnpm/`jco` toolchain checked into `tools/checks_js_componentizer/` (`README.md`: it runs `corepack pnpm install --frozen-lockfile` then `node scripts/build_check.mjs ...`, copying the checked-in toolchain into a per-user cache under `~/.cache/checkleft/`).
- It also shells out to `buildifier` (its own buildifier check), itself resolved from the prebuilt module or PATH.

So even a perfect prebuilt `checkleft` binary still depends on files that live in the repo checkout (the JS toolchain dir) and on `buildifier` being resolvable.

**What makes it harder to prebuild than buildifier:**
1. No upstream publisher — we'd build and publish every platform ourselves.
2. C build deps (tree-sitter, wasmtime/cranelift) mean cross-compiling (e.g. a Linux binary from a Mac) is non-trivial; the clean path is building each platform on its own native agent.
3. 64 MB artifact vs 7 MB — bigger to host, download, and cache-bust.
4. The Node runtime dependency is not captured by the binary and must still ship in the repo.
5. **It lives in *this* repo and changes frequently** — see below.

**Release status today.** `checkleft-v*` **git tags exist** (`checkleft-v0.1.0-alpha.3` … `-alpha.8`, confirmed via `gh api repos/spinyfin/mono/tags`), but there are **no `checkleft` GitHub *releases* and no binary assets** — every GitHub release on `spinyfin/mono` is `boss-v1.0.N` (confirmed via `gh release list`). The tags are version markers; the only packaging that exists is **source**, not binary (see §3).

---

## 3. Can it follow the same pattern?

Mapping the buildifier/ghostty recipe onto `checkleft`, step by step, and what would have to be built:

| Buildifier ingredient | Checkleft equivalent — does it exist? |
|---|---|
| Upstream that builds per-platform binaries | **Missing.** No upstream; we'd own it. |
| Per-platform binaries on GitHub releases | **Missing.** `checkleft-v*` tags exist but carry no binary assets. |
| A publish/release job | **Missing.** Only a *source* packager exists (below). |
| Bazel rule to download + select by platform | **Missing.** Would mirror `ghostty_kit`/`buildifier_prebuilt`. |
| Version + sha256 pin | Straightforward once the above exist. |
| `REPOBIN.toml` repoint | One-line change. |

### What already exists and partly helps

`tools/checkleft_package/` (`checkleft-package`) produces a **self-contained source tarball** (`checkleft-<version>-source.tgz`) — it flattens the workspace-inherited `Cargo.toml` into a standalone manifest, stages `src`/`api`/license/`Cargo.lock`, and generates standalone `MODULE.bazel`/`BUILD.bazel` so `checkleft` can be *built* outside the monorepo (`tools/checkleft_package/README.md`). This is aimed at crates.io/docs.rs-style source distribution. **It does not emit a compiled binary** — so it is not a prebuilt and does not solve the "skip the build" goal, though it confirms the team already thinks about shipping checkleft externally.

### The missing pieces, concretely

**(a) A place to host binaries.** Cheapest: attach assets to the **already-existing `checkleft-v*` tags** on `spinyfin/mono` (no new repo). Alternative: a dedicated `spinyfin/checkleft-prebuilts` repo, mirroring `spinyfin/ghostty-prebuilts`.

**(b) A publish/release path.** A new Buildkite step modeled on `.buildkite/steps/boss-release.sh`, but it must build on **both** platforms (boss-release is macOS-only):
- On the `macos-arm64` queue: `bazel build //tools/checkleft:checkleft` → rename to `checkleft-darwin-arm64` → `shasum -a 256`.
- On the `bazel-any` (Linux) queue: same → `checkleft-linux-amd64`.
- Attach both binaries (+ their sha256s) to a `checkleft-v<version>` GitHub release.
No cross-compilation needed if each platform builds natively. This is the single largest piece of new machinery.

**(c) Bazel wiring to consume.** Two options:
- *Minimal (ghostty pattern):* one `http_file` per platform, sha256-pinned, plus a `select()`/alias `BUILD` target that picks by `@platforms//os` + `@platforms//cpu`, exposed as `@checkleft_prebuilt//:checkleft`. Mark the file executable via a tiny genrule/`sh_binary` wrapper.
- *Full (buildifier pattern):* a small module extension/repo rule that registers per-platform repos and a toolchain. More code; only worth it if we also want clean toolchain semantics.

**(d) Version pin.** A version constant + per-platform sha256s in `MODULE.bazel` (exactly like `ghostty_kit`), bumped per release.

**(e) Repoint the tool.** Change `REPOBIN.toml`:
```toml
[tools.checkleft]
target = "@checkleft_prebuilt//:checkleft"   # was //tools/checkleft:checkleft
```

### Blockers and gotchas

1. **Chicken-and-egg / staleness (the big one).** `checkleft` is developed *in this repo* and changes often. A prebuilt binary always lags `HEAD`. A PR that modifies `checkleft` itself would then be checked by an *older* prebuilt unless the release fires first — and `integrity-checkleft --all` exists precisely to catch repo drift, which a stale prebuilt would mask or misreport. `buildifier` has none of this because it is third-party and bumped maybe once a year. This is a conceptual mismatch, not just an engineering cost.
2. **Node runtime dep persists.** A prebuilt binary does not eliminate the `tools/checks_js_componentizer/` Node toolchain dependency for JS/TS source-mode checks — that's read from the repo checkout at runtime. Prebuilding the binary neither captures nor breaks it, but it means "prebuilt checkleft" is not "checkleft with zero repo deps."
3. **Two-platform release agents.** The release job needs both a macOS arm64 and a Linux x86_64 agent. Both queues already exist, so this is wiring, not new infra.
4. **Bootstrap.** The release job still builds `checkleft` from source — so the source build path must stay healthy regardless.
5. **64 MB asset** per platform per release adds up on a frequently-tagged tool.

### Why it might not be worth it

- **Bazel already caches the build.** First build is slow (64 MB Rust + wasmtime/cranelift compile), but `--config=ci` uses the remote cache and `repobin` reuses the local Bazel cache, so steady-state cost is a cache hit. Prebuilding mainly saves the *cold-cache first build* — a narrow win.
- **Frequent change makes the prebuilt a recurring footgun** (gotcha #1), unlike buildifier.
- **Ongoing maintenance**: a two-platform release pipeline + a consumption rule + per-release version/sha bumps is real recurring cost for a tool that already builds and caches fine in-repo.

---

## 4. Recommendation

**No-go on the full buildifier-style prebuilts route for in-mono use.** The cost/benefit is inverted relative to buildifier: checkleft is heavier to package, has no upstream publisher, retains a repo-resident Node dependency, and — decisively — lives in this repo and changes frequently, so a prebuilt would chronically lag source. Bazel's cache already removes the cost prebuilding would save.

### Cheapest viable alternatives (in order)

1. **Verify the cache first.** If the motivation is "checkleft is slow to get," confirm CI is getting Bazel remote-cache hits on `//tools/checkleft:checkleft` under `--config=ci`. This likely already eliminates the cost with zero new machinery. *Do this before building anything.*
2. **Warm the local cache for devs.** If developers feel cold-build pain locally, prime via the remote cache (or a one-time `bazel build //tools/checkleft:checkleft` in a bootstrap/direnv hook). Keeps a single source of truth; no staleness risk.
3. **Prebuilt only for *external* (non-mono) consumers.** A prebuilt genuinely helps only when checkleft must run in a repo with no Rust toolchain. In that case the *minimal* mechanism is:
   - Extend the **existing** release machinery: a Buildkite step modeled on `boss-release.sh` that builds on both `macos-arm64` and `bazel-any` (Linux), and attaches `checkleft-darwin-arm64` + `checkleft-linux-amd64` (with sha256s) to the **already-existing `checkleft-v*` tags**.
   - Consume via `http_file` + `select()` (the `ghostty_kit` shape), sha256-pinned in `MODULE.bazel`, exposed as `@checkleft_prebuilt//:checkleft`.
   - Repoint only the *external* consumers — keep `//tools/checkleft:checkleft` as the in-mono source of truth so the staleness problem never bites the repo that owns the code.

### Concrete step list (only if "go")

1. Add a `checkleft-release` Buildkite step (template: `.buildkite/steps/boss-release.sh`) that:
   a. resolves the next `checkleft-v*` version,
   b. builds `//tools/checkleft:checkleft` on `macos-arm64` → `checkleft-darwin-arm64`,
   c. builds it on `bazel-any` (Linux) → `checkleft-linux-amd64`,
   d. computes sha256 for each,
   e. `gh release create checkleft-v<version> --repo spinyfin/mono <assets>`.
2. Add a `@checkleft_prebuilt` repo: per-platform `http_file` + a `select()`-based executable alias keyed on `@platforms//{os,cpu}`; wire it into `MODULE.bazel` with pinned version + sha256s (mirror `ghostty_kit`).
3. Repoint `REPOBIN.toml` `[tools.checkleft]`/`[tools.checks]` to `@checkleft_prebuilt//:checkleft`.
4. Add a freshness guard: a CI check that fails if the prebuilt's version is behind `tools/checkleft/Cargo.toml`, so the staleness footgun is loud rather than silent.
5. Document the bump procedure as a runbook (template: `tools/boss/docs/runbooks/update-ghostty-prebuilt.md`).

Steps 1–2 are the bulk of the work; step 4 is non-optional given gotcha #1.

---

## Open Questions

- **What is the actual motivation?** "Cold-build latency in CI/dev" vs "run checkleft in a non-mono repo with no Rust toolchain" lead to opposite answers (alternative #1/#2 vs #3). The work item doesn't say.
- **Is the Bazel remote cache already serving `//tools/checkleft:checkleft` under `--config=ci`?** If yes, the latency case for prebuilding largely evaporates. Not measured in this investigation.
- **Do the `checkleft-v*` tags drive any existing publish (e.g. crates.io via `checkleft_package`)?** If so, a binary-release step should slot alongside it rather than duplicate the tag scheme.
