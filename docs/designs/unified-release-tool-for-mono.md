# Unified release tool for mono

- Date: 2026-07-30
- Status: proposed — design only, no implementation
- Scope: `spinyfin/mono` (checkleft, Boss), `brianduff/appoint`
- Related: [`tools/checkleft/docs/buildkite-release-setup.md`](../../tools/checkleft/docs/buildkite-release-setup.md), [`tools/boss/docs/buildkite-release-setup.md`](../../tools/boss/docs/buildkite-release-setup.md), [`tools/boss/docs/designs/installable-distribution-package-for-boss.md`](../../tools/boss/docs/designs/installable-distribution-package-for-boss.md), [`tools/boss/docs/designs/automatic-boss-updates.md`](../../tools/boss/docs/designs/automatic-boss-updates.md)

Three products cut GitHub Releases today and each does it with its own copy of the same shell script. This proposes one Rust binary in mono — `//tools/release` — that owns everything from "what version is next" through "the release is published and verified", while each product keeps its own thin Buildkite step that builds its own artifacts.

## Verdict

The duplication is real and unusually literal: appoint's release script and checkleft's release script share **19 of appoint's 21 function names**, with several bodies byte-identical apart from a log prefix. Everything those two scripts share is publishing; everything they do not share is building. That is the seam.

**Recommendation: extract publishing into a Rust binary, leave building in each product's shell step, and bring Boss's release _record_ in but leave Boss's `.app` _packaging_ out.** The tool never runs a build and never learns what Bazel is.

## Goals

- One implementation of version resolution, tag creation, release-note generation, draft creation, asset upload, asset verification, and draft→publish sequencing — shared by checkleft, appoint, and Boss.
- The richer `tools/changelog` release notes become available to repos outside mono. Today appoint falls back to `gh release create --generate-notes` purely because `bin/changelog` does not exist outside mono ([appoint `release.sh:52-53`](https://github.com/brianduff/appoint/blob/main/.buildkite/steps/release.sh#L52-L53)).
- Boss inherits `.sha256` sidecars and draft-then-publish, which it does not have today; checkleft and appoint inherit Boss's hardened version resolution, which they do not have today. Unification moves safety in **both** directions.
- Keep it small. One new crate, three thin step scripts, a handful of deletions.

## Non-goals

- **Building artifacts.** The tool never invokes `bazel`, `cargo`, `codesign`, or `pkgbuild`. See "Where the build/publish boundary sits".
- **A configuration language.** No inheritance, no interpolation, no conditionals, no per-branch overrides, no build commands in config. See "Configuration".
- **Buildkite pipeline generation.** Each repo keeps its own `pipeline-*.yml` files and its own dynamic-fan-out `buildkite-agent pipeline upload` call.
- **Changing artifact names or the `.sha256` sidecar convention.** External consumers resolve on them by URL (`multitool.lock.json`).
- **Boss.app signing, notarisation, and `.pkg` assembly.** Out permanently. See "Boss.app: in or out".
- **Converging checkleft's `-alpha.N` scheme onto plain semver.** A separate, consumer-visible decision. See open questions.
- **Replacing `repobin` or `rules_multitool`.** The tool is distributed by exactly the mechanism checkleft already uses.

---

## Verified inventory

Everything below was read at current HEAD. Where the scoping brief or an existing doc disagrees with the code, the code is cited and the discrepancy is called out as a finding.

### 1. checkleft — `spinyfin/mono`

The most complete implementation, and the model the other two were copied from.

- **Script**: [`.buildkite/steps/checkleft-release.sh`](../../.buildkite/steps/checkleft-release.sh), 700 lines, five phases (`prepare` / `linux` / `musl` / `darwin` / `publish`) dispatched from `main()` at `:687-698`.
  - **Finding — brief correction.** The scoping brief places this at `tools/checkleft/checkleft-release.sh`. No such file exists; the script lives under `.buildkite/steps/`.
- **Pipelines**: [`.buildkite/pipeline-checkleft-release.yml`](../../.buildkite/pipeline-checkleft-release.yml) declares only the static `prepare` step (`:47-57`). `prepare` injects [`.buildkite/pipeline-checkleft-release-builds.yml`](../../.buildkite/pipeline-checkleft-release-builds.yml) via `buildkite-agent pipeline upload` at `checkleft-release.sh:491-494` **only when a release is actually being cut**, so a no-op cron tick runs no Bazel at all.
- **Version**: `compute_next_version` at `:130-160`. Requires `X.Y.Z-alpha.N` in `tools/checkleft/Cargo.toml` (hard `die` at `:135-137`), then next = max(cargo alpha, highest alpha among local `checkleft-v*` tags) + 1 (`:146-159`). The bump is applied to the CI working copy only (`apply_version_edits`, `:166-174`) and never committed — confirmed live: `tools/checkleft/Cargo.toml:3` reads `0.1.0-alpha.8` while the released tag pinned by appoint is `checkleft-v0.1.0-alpha.122`.
- **Version injection**: `--define=CHECKLEFT_VERSION=<ver>` on both the `build` and the `cquery` (`:530`, `:555-561`), reaching the binary through `rustc_env = {"CHECKLEFT_BUILD_VERSION": "$(CHECKLEFT_VERSION)"}` at [`tools/checkleft/BUILD.bazel:59-61`](../../tools/checkleft/BUILD.bazel), consumed by `#[command(version = option_env!("CHECKLEFT_BUILD_VERSION")...)]` at [`tools/checkleft/src/main.rs:145`](../../tools/checkleft/src/main.rs). `.bazelrc:23` defaults it to `0.0.0-dev`.
- **Artifacts**: `checkleft-<triple>` plus a `<name>.sha256` sidecar written by `stage_asset` (`:217-225`), uploaded by `upload_release_assets` (`:228-238`) with `gh release upload --clobber` and three attempts at 15/30/45 s backoff. Required: linux-gnu, linux-musl, aarch64-darwin (`:74-78`). Optional: x86_64-darwin (`:79-81`).
- **Draft-then-publish**: **already landed.** `prepare` creates the release with `--draft` at `:474-477`; `phase_publish` (`:642-683`) verifies every required asset _by re-downloading it from GitHub and re-hashing it_ (`verify_asset`, `:619-633`) before `gh release edit --draft=false` at `:680`.
  - **Finding — brief correction.** The brief describes checkleft as "created published before any binary exists". That was true; it is not true at HEAD. The in-flight change has landed.
- **musl**: built hermetically through Bazel — `bazel build -c opt --define=... //tools/checkleft:checkleft_musl` at `:557`, target at [`tools/checkleft/BUILD.bazel:116-120`](../../tools/checkleft/BUILD.bazel). Release-blocking: no `soft_fail`, no warn-and-continue, plus a version guard at `:566-570` that `die`s if the built binary's `--version` does not equal the computed version.
- **Notes**: `bin/changelog --project tools/checkleft/PROJECT.yaml --from <last> --to <new> --repo <repo> --enrich` at `:457-463`. `bin/changelog` is not committed — `bin/` is gitignored (`.gitignore:6`) and populated in CI by `repobin install --bin-dir bin/` at [`.buildkite/steps/ci-env.sh:94-96`](../../.buildkite/steps/ci-env.sh), which builds it from source.
- **Triggering**: schedule + manual. `is_manual()` (`:242-244`) reads `BUILDKITE_SOURCE`; a scheduled run skips unless `CHANGE_PATHS_RE` (`:94`) matched something since the last published tag (`should_skip`, `:285-323`).
- **Recovery**: a resume-existing-draft path at `:383-406`, gated to manual triggers (or `CHECKLEFT_RESUME_DRAFT=1`) precisely so a cron tick cannot re-fan-out an unpublishable draft forever.
- **Auth**: the agents' ambient git + `gh` credentials. No dedicated release token (`:50-53`).

### 2. appoint — `brianduff/appoint`

- **Files**: `.buildkite/steps/release.sh` (532 lines), `.buildkite/pipeline-release.yml`, `.buildkite/pipeline-release-builds.yml`. Landed in appoint PR #6.
- **Structural relationship to checkleft**: a deliberate fork of the _shape_, not a reuse. Its header comment cites `checkleft-release.sh` by `file:line` for each mirrored phase. Concretely, of appoint's 21 shell functions, **19 have an identically-named counterpart in checkleft's script**; appoint adds only `assert_allowed_trigger` and `in_buildkite_job`, and lacks checkleft's `_resolve_tag_sha`, `apply_version_edits`, `build_cross_cargo`, and `phase_musl`. `stage_asset`, `upload_release_assets`, and `verify_asset` are byte-identical apart from the log prefix.
- **Deliberate divergences**, per its header comment and `.buildkite/README.md`:
  - Two required artifacts (`appoint-aarch64-apple-darwin`, `appoint-x86_64-unknown-linux-gnu`), **no musl**, and no optional tier at all.
  - Version scheme: major.minor from `Cargo.toml`, patch computed as max(cargo patch, highest patch among `v<major>.<minor>.*` tags) + 1. Rejects a pre-release suffix outright — which is exactly why checkleft's resolver could not be reused verbatim.
  - No version is patched into the checkout: appoint's binary reads no version, so there is nothing to embed.
  - Notes via `gh release create --generate-notes`, **because `bin/changelog` is mono-internal**.
  - Both artifacts built natively via `bazel build --config=ci -c opt`; appoint PR #5 established that cross-building darwin→linux fails at analysis time.
  - `assert_allowed_trigger` refuses any `BUILDKITE_SOURCE` other than schedule/`ui`/`api` — defence in depth over the Buildkite-side push-trigger setting.
  - `in_buildkite_job()` gates meta-data calls on `BUILDKITE_JOB_ID` rather than `command -v buildkite-agent`, found empirically: a machine with the CLI but no agent token makes `command -v` a false positive that aborts a local run. **checkleft still has the `command -v` bug** (`checkleft-release.sh:103-113`).
  - No resume-existing-draft flow.
- **Triggering**: hourly cron plus manual, per `.buildkite/README.md`.
- **Consumption of checkleft**: `multitool.lock.json` pins `checkleft-v0.1.0-alpha.122` by direct release-asset URL plus `sha256`, for `macos/arm64` (the darwin binary) and `linux/x86_64` (the **musl** binary). `checks.sh` invokes `bazel run @multitool//tools/checkleft -- ...`.
- **Auth**: ambient git + `gh`, verified in appoint's README against `gh api repos/brianduff/appoint/keys` returning `[]`; noted there as **not yet exercised** because the `appoint-release` pipeline is not registered in Buildkite yet.

### 3. Boss

Two separate things wear the word "release", and conflating them is the main trap here.

**3a. `boss-release.sh` — the CI release record.** [`.buildkite/steps/boss-release.sh`](../../.buildkite/steps/boss-release.sh), 416 lines, a single step wired into the main pipeline at [`.buildkite/pipeline.yml:39-53`](../../.buildkite/pipeline.yml), gated to `main` + schedule/ui/api.

- **Version**: `boss-v1.0.N`, where `N = max(existing boss-v1.0.*) + 1` (`:206-212`). The `1.0` is a hardcoded literal, read from no manifest.
- **Version resolution is by far the most hardened of the three**, and this is the single strongest argument for bringing Boss in:
  - Releases are listed via **REST** (`gh api repos/.../releases --paginate`), explicitly _not_ `gh release list`, because the latter uses GraphQL whose rate-limit budget is shared with unrelated pollers; GraphQL exhaustion was confirmed as the cause of a real duplicate-tag incident (`:32-48`, `:51-62`).
  - A single authoritative snapshot backs every decision, because two independent queries minutes apart once disagreed (`:32-38`).
  - A `git ls-remote --tags` cross-check (`:225-250`) catches a degraded API list that under-reports the true max, and distinguishes "leaked tag from a dead run" from "degraded snapshot" with different remedies.
  - Fail-closed on an unlistable API (`:65-66`) rather than silently treating it as "no prior release".
  - **checkleft and appoint have none of this.** Both use `gh release list` (GraphQL) with `|| true` fallbacks — `checkleft-release.sh:272-275`, appoint `release.sh:265-266`.
- **Not draft-then-publish**: `gh release create` at `:394-397` publishes immediately, _then_ the asset uploads at `:400-414`. A failed upload leaves a published, empty release. This is not hypothetical — [`automatic-boss-updates.md:22`](../../tools/boss/docs/designs/automatic-boss-updates.md) records `boss-v1.0.21` existing with no asset, and forces the updater to skip assetless releases.
- **No `.sha256` sidecars.** [`automatic-boss-updates.md:182,285`](../../tools/boss/docs/designs/automatic-boss-updates.md) names a published `Boss-1.0.N.zip.sha256` as a wanted-but-absent cheap hardening.
- **Artifact name is `Boss-1.0.N.zip`** (`:254`), consumed by exact name by the auto-update design (`automatic-boss-updates.md:110`). It does **not** follow the `<name>-<triple>` convention and must not be changed.
- **Build**: `bazel build -c opt --define=BOSS_SHAKE_*` (`:325-333`) plus a GhosttyKit stub (`:308`) and a `cquery` path discovery that must use the identical flag set (`:315-338`). Three `BOSS_SHAKE_*` secrets are read from the Buildkite secret store (`:148-175`).
- **Version stamping is inverted relative to checkleft**: the tag is pushed _before_ the build (`:299-302`) so that [`tools/boss/installer/workspace-status.sh`](../../tools/boss/installer/workspace-status.sh) can resolve it with `git describe --exact-match` and stamp `STABLE_BOSS_VERSION`. `.bazelrc:12` wires that as `--workspace_status_command`.
- **Notes**: same `bin/changelog --enrich` call as checkleft (`:382-388`).

**3b. `tools/boss/installer/release.sh` — the `.pkg` packaging path.** [289 lines](../../tools/boss/installer/release.sh), run by a human via `bazel run //tools/boss/installer:release --config=release`. It codesigns every Mach-O with a Developer ID, signs the bundle, runs `pkgbuild`/`productbuild`/`productsign`, submits to `notarytool`, and staples. **It is not wired into any Buildkite pipeline and shares no code with `boss-release.sh`.** Its credentials (`BOSS_DEVELOPER_ID_*`, `AC_*`) exist nowhere in CI.

### Stale-documentation findings

`tools/checkleft/docs/buildkite-release-setup.md` **contradicts itself**, and is half-corrected rather than wholly stale:

- §7 (`:98-106`) is **correct** at HEAD: musl is hermetically Bazel-built and release-blocking. The brief's "known trap" describes a state that has since been fixed here.
- `:177` is **still wrong**, in the same document: it claims "the cross targets (`x86_64-apple-darwin`, `x86_64-unknown-linux-musl`) are built with `cargo --target`" — musl is not (`checkleft-release.sh:557`) — and that "checkleft's CLI does not embed `CARGO_PKG_VERSION`, so all binaries are byte-identical regardless of the version string". The binary _does_ embed a version, via `CHECKLEFT_BUILD_VERSION` ([`main.rs:145`](../../tools/checkleft/src/main.rs)), and `phase_musl:566-570` exists precisely to assert that the bytes differ per version.

Fixing `:177` is filed as its own task below; it is independent of everything else here.

---

## What is genuinely common, and what is genuinely per-product

The function-name overlap already answers this empirically. Sorting the three scripts' logic:

**Common — every product does this identically, or differs only in a value:**

| Concern                                                         | Evidence it is common                                                   |
| --------------------------------------------------------------- | ----------------------------------------------------------------------- |
| Resolve last published release; keep drafts strictly separate   | `checkleft:269-282`, appoint `:263-272`, `boss:64-87`                   |
| Idempotency guard (never re-release the same commit)            | `checkleft:291-294`, appoint `:281-284`, `boss:97-103`                  |
| Change detection since last tag, skipped on manual triggers     | `checkleft:296-322`, appoint `:286-312`, `boss:105-144`                 |
| Next-version = `max(manifest counter, highest tag counter) + 1` | `checkleft:130-160`, appoint `:180-209`, `boss:199-252`                 |
| Fetch tags, re-fetch before tagging, collision guard            | `checkleft:429-435`, appoint `:366-372`, `boss:288-297`                 |
| Tag + push; delete the leaked tag on failure via an EXIT trap   | `checkleft:334-348,440-445`, appoint `:323-334,374-379`, `boss:265-302` |
| Release notes                                                   | `checkleft:447-466`, `boss:362-391` (identical `bin/changelog` call)    |
| Create the release as a draft                                   | `checkleft:474-477`, appoint `:385-388`                                 |
| Stage asset + write `.sha256` sidecar                           | `checkleft:217-225` ≡ appoint `:235-243`                                |
| Upload with 3 attempts / 15-30-45 s backoff / `--clobber`       | `checkleft:228-238` ≡ appoint `:246-256`                                |
| Re-download + re-hash verification of every required asset      | `checkleft:619-633` ≡ appoint `:461-475`                                |
| Verify the declared asset set, then flip draft→published        | `checkleft:642-683` ≈ appoint `:484-511`                                |
| Buildkite meta-data hand-off of the tag between phases          | `checkleft:103-113`, appoint `:133-143`                                 |

**Per-product — genuinely different, and must stay so:**

| Concern                                     | Why it cannot be shared                                                                                                                                                                                                                                                                                                                                                             |
| ------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| How each artifact is built                  | Four mutually incompatible shapes: `bazel build` + `--define=CHECKLEFT_VERSION` (`checkleft:530`), `cargo build --target` cross (`checkleft:204-214`), a platform-transitioning `musl_binary` target (`BUILD.bazel:116-120`), and Boss's `-c opt` + three `--define` secrets + a GhosttyKit stub + `.zip` path discovery (`boss:304-346`). They share nothing but the word "build". |
| Which platforms / asset names               | checkleft: 3 required + 1 optional `<name>-<triple>`. appoint: 2 required. Boss: one `.zip` on a completely different naming convention that an external updater resolves by exact name.                                                                                                                                                                                            |
| Buildkite pipeline YAML and agent targeting | Queues, `os:` tags, `depends_on` graphs, and the fan-out fragment differ per repo and per fleet.                                                                                                                                                                                                                                                                                    |
| Which paths count as release-affecting      | Inherently a per-product path list.                                                                                                                                                                                                                                                                                                                                                 |
| Version-vs-build ordering                   | checkleft patches the version into the checkout _before_ building; Boss pushes the **tag** before building so `workspace-status.sh` can `git describe` it. Both are satisfied by "prepare runs first", but the mechanism is the product's.                                                                                                                                          |
| Build-time secrets                          | `BOSS_SHAKE_*` are inputs to Boss's _compile_, never to publishing.                                                                                                                                                                                                                                                                                                                 |

The line between the two tables is exactly the line between publishing and building.

---

## Alternatives considered

### A. A shared shell library (`.buildkite/lib/release-common.sh`)

Extract the duplicated bash into a sourced library and have each product's script source it. Smallest possible diff; no new crate; no bootstrap problem.

**Rejected.** It does not solve the stated problem — "the logic is written in shell in some cases, and rust in other cases" — it entrenches shell. Worse, it cannot cross the repo boundary: appoint cannot `source` a file that lives in mono, so appoint would need the file vendored, which is the duplication we are removing, wearing a different hat. And the logic most worth sharing is the logic most worth _testing_: version arithmetic, skip decisions, degraded-API detection. appoint already felt this — it bolted a sourcing guard onto its script (`release.sh:530-532`) specifically so a test harness could exercise `compute_next_version` in isolation. That is a workaround for shell being untestable, not a design.

### B. A Bazel rule (`release_artifacts(...)` macro) that builds _and_ publishes

Model the release as a Bazel target per product: declare the artifacts, and a `bazel run //tools/checkleft:release` does everything.

**Rejected.** Publishing is not hermetic — it mutates GitHub, pushes tags, and depends on wall-clock remote state. Wrapping it in Bazel buys nothing and costs the ability to run the phases on _different agents_, which is the whole reason the current pipelines fan out (`prepare` on Linux, `darwin` on macOS, `publish` back on Linux; `checkleft-release.sh:31-33`). It also cannot work for appoint, whose Bazel graph lives in another repo. Finally, it drags the build into the shared surface — the one thing the evidence says is irreducibly per-product.

### C. A Rust binary that owns publishing only, invoked from a thin per-repo shell step — **chosen**

See below.

### D. A Rust binary that owns publishing _and_ orchestrates builds via a declared command

A middle road: config names a shell command per artifact, and the tool runs it.

**Rejected as a config language in disguise.** The moment `build_command = "bazel build -c opt --define=..."` is a config value, the config is a program, and every product's build flags drift inside a string the tool cannot type-check. The tool would also have to reproduce Buildkite's cross-agent fan-out to be useful, since a single process cannot build the macOS and Linux artifacts. Keeping the build in the pipeline keeps the fan-out where the fan-out already works.

---

## Chosen approach

### Shape

A single Rust binary, `//tools/release`, with four subcommands that map one-to-one onto the phases that already exist:

```
release prepare  --config <path>            # resolve, skip-or-proceed, tag, notes, create draft; prints the tag
release upload   --config <path> --tag <t> --asset <name>=<path> [--asset ...]
release publish  --config <path> --tag <t>  # verify every required asset, then draft -> published
release tag      --config <path>            # read back the tag prepare recorded (for build steps)
```

Each repo keeps one small step script whose only remaining job is _building_. checkleft's `darwin` phase, for instance, collapses to roughly:

```sh
TAG="$(release tag --config tools/checkleft/release.toml)" || exit 0   # non-zero => prepare skipped this run
export CHECKLEFT_VERSION="${TAG#checkleft-v}"
bazel build -c opt "--define=CHECKLEFT_VERSION=${CHECKLEFT_VERSION}" //tools/checkleft:checkleft
BIN="$(bazel cquery -c opt "--define=CHECKLEFT_VERSION=${CHECKLEFT_VERSION}" --output=files //tools/checkleft:checkleft | grep '^bazel-out/' | head -1)"
release upload --config tools/checkleft/release.toml --tag "$TAG" \
  --asset "checkleft-aarch64-apple-darwin=$BIN"
```

That is the honest shell/Rust split the brief asks about: shell keeps Buildkite step glue, Bazel invocation, and environment plumbing — the things it is genuinely good at — and gives up arithmetic, remote-state reasoning, and error handling.

### Where the build/publish boundary sits, and why

**The tool accepts built files and never produces them.** `upload` takes `--asset <name>=<path>`; it does not know what a Bazel target is.

The hypothesis in the brief holds, and the per-product table above is the test: the four build shapes across three products share literally nothing, while every publishing helper is byte-identical or near-identical across two of them. Two further reasons make the boundary load-bearing rather than merely tidy:

1. **The phases run on different machines.** `prepare` runs on Linux, `darwin` on macOS, `publish` back on Linux. A tool that built things would have to be a distributed build system; a tool that only publishes is a stateless CLI invoked three times.
2. **Build secrets stay out of the shared surface.** `BOSS_SHAKE_*` are compile inputs. A publishing tool never sees them, which is both a security property and the reason no new credential path is needed.

The one thing that _does_ cross the boundary is the version string, and it crosses in one direction only: `prepare` computes it, records it in Buildkite meta-data, and every build step reads it back via `release tag`. That satisfies both orderings already in use — checkleft patching the version into the checkout before compiling, and Boss needing the tag pushed before `workspace-status.sh` runs.

### Language: Rust

- mono is a Rust + Bazel repo, `tools/changelog` is already Rust, and `bin/changelog` is _already a dependency of the release path_ (`checkleft-release.sh:457`, `boss-release.sh:382`). Absorbing notes generation into the same binary removes a `repobin install` prerequisite from the release path **and** is the concrete named win: it makes enriched, path-scoped release notes available to appoint, which today falls back to `--generate-notes` only because `bin/changelog` does not exist outside mono.
- The bugs actually observed in these scripts are shell bugs. A `|| true` swallowed a `gh` failure into "no prior release exists", publishing a misleading "Initial Boss release." placeholder 23 releases in (`boss-release.sh:32-38`). A degraded GraphQL response under-reported the max tag and caused a duplicate-tag collision (`:39-48`). `command -v buildkite-agent` is a false positive on a developer machine (appoint `release.sh:122-131`). Each of these is a failure category that a `Result`-typed error path and a closed enum make hard to write in the first place.
- The logic worth sharing is the logic worth unit-testing, and in Bazel that means `rust_test`: version arithmetic for both schemes, skip decisions, degraded-list detection, asset-set verification — all pure functions over fixture data, none of which needs a network.
- **What shell keeps**: invoking `bazel`, `buildkite-agent pipeline upload`, exporting env vars, and the per-product build. Each repo keeps a thin step script. This is a legitimate outcome, not a compromise.

### Subprocesses: `git` and `gh`

The tool shells out to `git` and `gh` exactly as the scripts do today, with a small synchronous runner of its own.

**Reuse note, stated explicitly per the repo's reuse rule.** `tools/boss/github/src/gh_runner.rs` already describes itself as "a generic `gh` CLI runner abstraction used by any crate that shells out to the GitHub CLI". It is nonetheless the wrong dependency here, for three reasons that are structural rather than stylistic: its Bazel visibility is `//tools/boss:__subpackages__` ([`tools/boss/github/BUILD.bazel:21-23`](../../tools/boss/github/BUILD.bazel)), so `//tools/release` cannot depend on it without widening visibility — which the repo forbids by default; it is `async` on tokio and instrumented through `boss_gh_telemetry`, neither of which a synchronous release CLI wants; and `//tools/release → //tools/boss/github` is a shared-tool→product edge, the inverted direction the repo's dependency rule prohibits. The correct reuse is a **move** — extract a product-neutral `gh` runner into a lower crate that both depend on — and that is a larger refactor than this project should absorb. It is filed as a deferred task rather than silently skipped.

### Configuration

One flat TOML file per releasable product, deserialized into a typed struct with closed enums. Not a configuration language: **no inheritance, no interpolation, no conditionals, no per-branch overrides, no commands.**

```toml
# tools/checkleft/release.toml
repo            = "spinyfin/mono"
tag_prefix      = "checkleft-v"
title_prefix    = "checkleft"

[version]
scheme   = "alpha-counter"              # | "patch-counter"
manifest = "tools/checkleft/Cargo.toml"

[notes]
source  = "changelog"                   # | "github-generated"
project = "tools/checkleft/PROJECT.yaml"

change_paths = [
  "tools/checkleft/",
  ".buildkite/steps/checkleft-release.sh",
  ".buildkite/pipeline-checkleft-release.yml",
  ".buildkite/pipeline-checkleft-release-builds.yml",
]

required_assets = [
  "checkleft-x86_64-unknown-linux-gnu",
  "checkleft-x86_64-unknown-linux-musl",
  "checkleft-aarch64-apple-darwin",
]
optional_assets = ["checkleft-x86_64-apple-darwin"]
```

**Why a file rather than flags.** The asset list must be _the same list_ in the phase that uploads and the phase that verifies. Passing it as flags means two copies in two shell invocations that can silently drift — which is precisely the failure mode this project exists to remove. `change_paths` and the version scheme are similarly read by more than one subcommand. Eight keys, no nesting beyond two fixed tables, and every enum closed at compile time.

`release tag` exits non-zero when `prepare` recorded no tag (it skipped), which is what makes the one-line guard in the step script above correct: a build phase in a no-release run exits cleanly without building anything.

**Why the version scheme is an enum with two variants and not three.** `alpha-counter` bumps the `N` in `X.Y.Z-alpha.N`; `patch-counter` bumps `Z` in `X.Y.Z`. Boss's `boss-v1.0.N` is `patch-counter` with the major.minor supplied as a literal (`major_minor = "1.0"`) instead of read from a manifest, because Boss has no manifest to read it from — one extra field on an existing variant, not a third scheme.

### How a repo outside mono consumes it

Identically to checkleft, because that path is proven. mono publishes `release-<triple>` assets with `.sha256` sidecars under `release-v*` tags; appoint adds a second entry to its existing `multitool.lock.json` beside checkleft, pinned by URL and `sha256`, and invokes `bazel run @multitool//tools/release -- prepare --config release.toml`. No new hosting, no new auth — the assets download anonymously over HTTPS, as [`tools/checkleft/docs/checkleft-sandbox.md`](../../tools/checkleft/docs/checkleft-sandbox.md) records for checkleft today. A bump is an edit to two URLs and two hashes.

### The self-release bootstrap

This needs a stated answer, so here it is in full.

**Inside mono, the tool is always built from source and never consumed as a prebuilt.** mono's release pipelines invoke it as `bin/release` (populated by `repobin install`, which builds from source at [`ci-env.sh:94-96`](../../.buildkite/steps/ci-env.sh)) or directly as `bazel run //tools/release`. There is therefore **no chicken-and-egg**: the tool at HEAD releases the tool at HEAD. A broken _published_ release can never block mono from cutting a corrected one, and cannot block checkleft's or Boss's releases either.

**Outside mono, the tool is a pinned prebuilt, and a broken publish breaks consumers until they re-pin.** If a bad `release-v*` lands, appoint's release pipeline fails. Recovery is to revert `multitool.lock.json` to the previous known-good pin — the same one-file recovery appoint already relies on for checkleft, needing no new machinery. This is a real residual sharp edge and is stated rather than engineered around: the blast radius is "external repos cannot cut releases until they re-pin", never "mono is stuck".

Two cheap guards keep that window small, and both are in the task list: the tool's own release is gated on its `rust_test` suite passing in mono CI, and its release is subject to the same draft-then-publish + checksum verification as everything else, so a truncated upload can never become a published pin.

### Auth: no new secrets

Established from code, not assumed. Tag pushes use `git push origin` and release operations use `gh`, both on the CI agents' **ambient** credentials — `checkleft-release.sh:50-53` states it, `boss-release.sh` relies on it, and appoint's README verified it against a zero-deploy-key repo. The shared tool inherits exactly this and introduces nothing. `BOSS_SHAKE_*` remain build-time secrets in Boss's own step. The Developer ID and notary credentials in `tools/boss/installer/release.sh:16-25` stay out of CI entirely, consistent with the out-of-scope verdict below.

---

## Defaults: which behaviour wins

Each row is decided, not configurable. Where a difference is inherently a per-product _value_ (an asset list, a path list) it is config; where it is a _behaviour_, one wins.

| Behaviour                                            | checkleft                              | appoint                  | Boss                                   | **Winner and why**                                                                                                                                                                |
| ---------------------------------------------------- | -------------------------------------- | ------------------------ | -------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Draft → verify → publish                             | yes                                    | yes                      | **no**                                 | **Draft-then-publish.** Two of three already do it; Boss's published-first ordering produced a real assetless release (`boss-v1.0.21`).                                           |
| Explicit expected-asset set, verified before publish | yes                                    | yes                      | none                                   | **Required.** Verified by re-downloading and re-hashing from GitHub, never by trusting what a build phase staged locally.                                                         |
| `.sha256` sidecars                                   | yes                                    | yes                      | no                                     | **Yes, universally.** Boss's own auto-update design asks for this.                                                                                                                |
| Optional-asset tier                                  | yes (1 asset)                          | no                       | n/a                                    | **Kept, minimally.** Exists for exactly one asset — checkleft's cargo-cross `x86_64-apple-darwin`. Two closed lists, not a policy engine. Removing it is a deferred task.         |
| Release listing API                                  | GraphQL                                | GraphQL                  | **REST + `git ls-remote` cross-check** | **Boss's.** It is the only one hardened against the failure that actually happened, and it is strictly safer for the other two.                                                   |
| Behaviour on an unlistable API                       | `\|\| true` → looks like "no releases" | `\|\| true`              | **fail closed**                        | **Boss's.** Silently degrading to "first release ever" is how a misleading placeholder shipped.                                                                                   |
| Auto-incrementing version, never committed           | yes                                    | yes                      | yes                                    | **Unanimous — keep.** No human bump, ever.                                                                                                                                        |
| Notes                                                | `changelog --enrich`                   | `--generate-notes`       | `changelog --enrich`                   | **`changelog`, linked as a library.** `github-generated` stays as a fallback enum variant only for a product with no `PROJECT.yaml`.                                              |
| Trigger guard (refuse push/PR builds)                | none                                   | `assert_allowed_trigger` | none                                   | **appoint's.** Defence in depth, zero cost.                                                                                                                                       |
| "Am I in a Buildkite job?"                           | `command -v`                           | `BUILDKITE_JOB_ID`       | n/a                                    | **appoint's.** checkleft's is a known false positive.                                                                                                                             |
| Resume an interrupted draft                          | yes, manual-only                       | no                       | n/a                                    | **checkleft's, gated exactly as today.** The manual-trigger gate is the load-bearing part: it stops a cron tick re-fanning-out an unpublishable draft forever.                    |
| Idempotency guard on all trigger paths               | yes                                    | yes                      | yes                                    | **Unanimous — keep.**                                                                                                                                                             |
| Leaked-tag cleanup via EXIT trap                     | yes                                    | yes                      | yes                                    | **Unanimous — keep.**                                                                                                                                                             |
| Version scheme                                       | `-alpha.N`                             | patch                    | fixed `1.0.N`                          | **Two closed enum variants** (`alpha-counter`, `patch-counter`), Boss expressed as the latter with a literal major.minor. Converging to one is out of scope — see open questions. |

**Not changed, deliberately:** asset names and the sidecar convention (external consumers resolve on them by URL), musl remaining release-blocking for checkleft, and a failed platform build never yielding a published release. All three are safety properties the brief forbids weakening, and the draft-then-publish default enforces the third structurally rather than by convention.

---

## Boss.app: in or out

**Split verdict, and the split is not a hedge — the two halves are wired to different things.**

**In scope: Boss's release _record_.** `boss-release.sh`'s version resolution, tag push, notes, release creation, asset upload, verification, and publish move to the shared tool. Boss gains draft-then-publish and `.sha256` sidecars, both of which its own auto-update design asks for and neither of which it has. The other two products gain Boss's REST-plus-cross-check version resolution, which is the best of the three. Boss keeps its own step for the parts that are its own: reading `BOSS_SHAKE_*`, the GhosttyKit stub, `bazel build -c opt --define=...`, and the `.zip` `cquery` discovery.

One constraint the shared tool must respect: **Boss's asset is `Boss-1.0.N.zip`, not `boss-<triple>`.** The `<name>-<triple>` shape is a checkleft/appoint convention, not a rule of the tool; asset names are per-product config precisely so Boss's existing name — which the auto-update design resolves by exact string — survives untouched.

**Out of scope, permanently: Boss.app packaging.** Bundle assembly, `codesign`, `notarytool`, `stapler`, `pkgbuild`/`productbuild`/`productsign` stay in `tools/boss/installer/`. Three reasons, in order of weight:

1. **It is not part of the CI release path at all.** `tools/boss/installer/release.sh` is invoked by a human via `bazel run //tools/boss/installer:release`; `boss-release.sh` never calls it. There is no duplication here to remove — it is not a third copy of the same script, it is a different program doing a different job.
2. **Its credentials do not exist in CI.** `BOSS_DEVELOPER_ID_*` and `AC_*` are developer-machine keychain and Apple-ID credentials. Pulling this in would mean provisioning notarisation secrets into Buildkite, which the brief's no-new-credentials constraint rules out without a separate, explicit decision.
3. **Signing is a build step, not a publish step.** It transforms the artifact. The boundary this design draws is exactly "the tool does not transform artifacts", and signing is the clearest possible case of transformation.

A design that pretended these three were alike would be worse than one that unifies the two that are and explains the third. The honest statement is: **two of the three products are the same program; the third is the same program plus a genuinely different artifact pipeline, and only the first half is shared.**

---

## Migration path

Ordered, with each product working at every step. No product runs both paths at once: each cuts over in a single PR that deletes its old script in the same change.

**checkleft** (first — it is in mono, so it needs no published tool).
Changes: `.buildkite/steps/checkleft-release.sh` shrinks from 700 lines to roughly 80 — three build phases that invoke Bazel and call `release upload`. Added: `tools/checkleft/release.toml`. Deleted: `compute_next_version`, `apply_version_edits`, `resolve_last_release`, `should_skip`, `stage_asset`, `upload_release_assets`, `verify_asset`, `phase_prepare`, `phase_publish`, `cleanup`, and the meta-data helpers. Unchanged: `phase_musl`'s Bazel invocation and its version guard, the cargo-cross darwin fallback, both pipeline YAMLs, all asset names, the tag scheme.

**The tool releases itself** (second — gates everything outside mono).
Adds `//tools/release` to a release pipeline producing `release-<triple>` assets with sidecars under `release-v*`, using the source-built tool. Nothing else changes.

**appoint** (third — a PR in `brianduff/appoint`, not mono).
Changes: add a `release` entry to `multitool.lock.json`; `.buildkite/steps/release.sh` shrinks from 532 lines to roughly 60. Added: `release.toml` at the repo root. Deleted: everything checkleft deletes, plus `assert_allowed_trigger` and `in_buildkite_job` (now the tool's behaviour for everyone). **Gains** `changelog`-generated notes in place of `--generate-notes`, which is the concrete user-visible win. Unchanged: two artifacts, no musl, native-only builds, both pipeline YAMLs, the `vX.Y.Z` tag scheme.

**Boss** (fourth — independent of the appoint chain; different files, can run in parallel with it).
Changes: `boss-release.sh` shrinks from 416 lines to roughly 90 — secrets, stub, build, `cquery`, then `release upload` and `release publish`. Added: `tools/boss/release.toml`. **Behaviour changes**: the release is now a draft until its asset is verified, and a `Boss-1.0.N.zip.sha256` sidecar is published. The asset name and tag scheme are unchanged, so the auto-update design's assumptions hold. Note that Boss is a single step today, so `prepare`/`upload`/`publish` are three calls in one script rather than three Buildkite steps — the tool does not care.

**How re-divergence is prevented.** The old script is deleted in the same PR that adds the new call — there is no fallback path to drift back into. The one unavoidable window is between checkleft's cutover and appoint's, since appoint needs a published tool; it is bounded by two tasks and during it appoint simply keeps running its current script unmodified. If a fourth product ever appears, the reviewable signal is a new `*-release.sh` over ~100 lines; that is a code-review matter, not something worth a check.

---

## Risks / open questions

- **Does checkleft's `-alpha.N` scheme survive, or converge on plain semver?** This design keeps it, because checkleft's tags are pinned by URL in every external consumer's `multitool.lock.json` and a scheme change is a consumer-visible event with no benefit to _this_ project's goal. The cost is one extra enum variant carried indefinitely. Converging is a legitimate separate decision and is filed as a deferred task; it wants a human call.
- **Is the optional-asset tier worth keeping?** It exists for exactly one asset — checkleft's `x86_64-apple-darwin`, the only remaining `cargo` cross-build in the whole release path. Dropping that asset would delete the entire optional/required distinction and remove `cargo` from releases altogether. It would also remove an asset some consumer might be fetching; we have no evidence either way. Deferred, flagged.
- **Boss's cutover changes observable release behaviour.** A published-immediately release becomes a draft for the duration of one build. Anything polling for the newest release will see it appear later than it does today. The auto-update design already skips assetless releases, so the change is strictly an improvement — but it is a behaviour change on a shipped product.
- **appoint's release pipeline has never run.** Its README states the `appoint-release` pipeline is not yet registered in Buildkite, so its ambient-credential assumption is documented but unexercised. If the first real run fails on credentials, that is new information this design does not have, and the fix is an operator investigation rather than a change to the tool.
- **`release` is a very generic binary name.** It is fine under `@multitool//tools/release` and in a repo-local `bin/`, but it is a weak name for something a human might put on a PATH. Alternatives: `relcut`, `cutter`, `shipwright`. Flagged for a human call.
- **The `gh` runner duplication is real and acknowledged.** `tools/release` gets its own small `gh`/`git` runner rather than depending on `boss_github`, for the visibility, async, and dependency-direction reasons given above. The correct fix is extracting a product-neutral runner crate; it is deferred, not forgotten.
- **Two products' change-detection scopes currently differ** (checkleft includes its release script and pipeline files; appoint excludes them). Under the shared tool the release script mostly ceases to exist, so the difference largely dissolves — but `change_paths` remains per-product config and each product's list should be reviewed at cutover rather than copied mechanically.

---

## Proposed implementation task breakdown

Six entries. Tasks that may run in parallel are noted, and file-overlap warnings are called out where two otherwise-independent tasks would edit the same file. The breakdown is deliberately coarse: work a single worker would do in one sitting — the same crate, the same shell script, the same doc — is one task rather than several, because splitting it would only force each PR to forward-port the last for no gain in reviewability.

### 1. Create the `//tools/release` crate: config, version resolution, and the release-state decisions

Add `tools/release/` as a new workspace crate with its Bazel `rust_binary` and `rust_test` targets, minimal visibility, and a `Cargo.toml` registered in the root workspace members. It implements four layers, all of which are pure logic over injected inputs and land together because each is a few hundred lines that only the next one consumes. **Config:** the typed `ReleaseConfig` struct — flat TOML, closed enums, no inheritance. **Version resolution:** `compute_next_version` for both the `alpha-counter` and `patch-counter` schemes, including `patch-counter`'s literal-major-minor form for Boss. **Release-state queries:** the GitHub release-listing layer over `gh api repos/.../releases --paginate` (REST, deliberately not GraphQL), the published-vs-draft split that `resolve_last_release` needs, and the `git ls-remote --tags` cross-check that fails closed when the API list under-reports the true maximum tag — porting `boss-release.sh:32-62,214-250`, the hardening the other two products lack. **The skip decision:** `should_skip` (never re-release a commit already at the head of the latest published tag; on scheduled triggers, skip unless a `change_paths` entry was touched since that tag) and appoint's `assert_allowed_trigger` (refuse anything that is not a schedule or a manual `ui`/`api` build). Unit-tested over fixture manifests, fixture tag lists, and captured fixture JSON through an injectable runner; no network, no real subprocess, and no product wired to it yet.

- Effort hint: `large`
- Dependencies: none — may start immediately.
- Scope: in-scope

### 2. Implement the CLI: `prepare`, `tag`, `upload`, and `publish`

Build the four subcommands on top of task 1. They are one task, not four, because they share the crate's CLI entry point: split apart, whichever landed second would have to forward-port the others' subcommand registration rather than replace it, and a reviewer would read them together anyway. `prepare`: fetch tags, resolve, decide skip-or-proceed, compute the version, re-fetch and collision-guard, create and push the tag, generate notes, create the release as a draft, and record the tag in Buildkite meta-data (gated on `BUILDKITE_JOB_ID`, per appoint) — including leaked-tag cleanup on failure and checkleft's manual-trigger-gated resume-existing-draft path. Notes come from `//tools/changelog:git_changelog_lib` as a library dependency — a one-line visibility widen on that target — with `github-generated` as the fallback variant. `tag`: read the tag back for later build steps. `upload`: parse `--asset <name>=<path>`, copy each file to a staging dir under its release asset name, compute and write the `<name>.sha256` sidecar in `sha256sum -c` format, and upload the staging dir with `gh release upload --clobber` over three attempts at 15/30/45 s backoff, porting `checkleft-release.sh:217-238` verbatim in behaviour. `publish`: list the release's remote assets, assert every `required_assets` entry and its sidecar are present, re-download each required asset plus any present optional asset, re-hash it against its sidecar, then `gh release edit --draft=false`, leaving the draft and its assets in place on any failure for inspection; ports `checkleft-release.sh:619-683`.

- Effort hint: `large`
- Dependencies: task 1.
- Scope: in-scope

### 3. Migrate checkleft, publish the tool as a consumable prebuilt, and rewrite checkleft's release doc

Add `tools/checkleft/release.toml` and rewrite `.buildkite/steps/checkleft-release.sh` to three build-only phases plus `release prepare` / `release upload` / `release publish` calls, deleting every duplicated helper in the same PR. Asset names, tag scheme, musl's release-blocking status and version guard, the cargo-cross darwin fallback, and both pipeline YAMLs are unchanged. This is the tool's first real exercise, and the pipeline shape it settles on is immediately reused for the tool's own release path: `//tools/release` publishes `release-<triple>` assets with `.sha256` sidecars under `release-v*` tags, built from source inside mono so no bootstrap dependency exists and gated on the crate's `rust_test` suite — this is what external repos will pin. Then rewrite `tools/checkleft/docs/buildkite-release-setup.md` to describe the shared tool, the per-product `release.toml`, the recovery paths, and the self-release bootstrap answer. That rewrite is also the fix for the doc's self-contradictory claim at `buildkite-release-setup.md:177` that the musl target is built with `cargo --target` and that all checkleft binaries are byte-identical regardless of version — both contradicted by §7 of the same document, `checkleft-release.sh:557`, and `tools/checkleft/BUILD.bazel:59-61`. Spot-fixing that line ahead of the rewrite would be work thrown away.

- Effort hint: `large`
- Dependencies: task 2.
- Scope: in-scope

### 4. Migrate Boss's release record to the shared tool and rewrite its release doc

Add `tools/boss/release.toml` and rewrite `.buildkite/steps/boss-release.sh` to keep only the secret loading, GhosttyKit stub, `bazel build -c opt --define=...`, and `.zip` `cquery` discovery, delegating everything else to the tool. Boss gains draft-then-publish and a `Boss-1.0.N.zip.sha256` sidecar. The asset name `Boss-1.0.N.zip` and the `boss-v1.0.N` tag scheme are unchanged, so the auto-update design's resolution logic is unaffected. Rewrite `tools/boss/docs/buildkite-release-setup.md` in the same PR, for the same reasons and to the same shape as checkleft's in task 3.

- Effort hint: `medium`
- Dependencies: task 2. **May run in parallel with tasks 3 and 5** — it touches only `boss-release.sh`, `tools/boss/`, and Boss's own doc, none of which those tasks edit.
- Scope: in-scope

### 5. Migrate appoint to the shared tool

A PR in `brianduff/appoint`, not mono. Add a `release` entry to `multitool.lock.json` pinned to the prebuilt from task 3, add `release.toml` at the repo root, and reduce `.buildkite/steps/release.sh` to two build phases plus three tool calls, deleting the rest. Switches appoint's release notes from `--generate-notes` to `changelog`-generated path-scoped notes. This stays its own task rather than merging into task 3 or 4: it is a different repository, so it cannot share a PR with any mono work, and it is hard-blocked on the prebuilt existing.

- Effort hint: `medium`
- Dependencies: task 3.
- Scope: in-scope

### 6. Deferred follow-ons: `gh`-runner extraction, version-scheme convergence, optional-asset retirement, and Boss.app signing

Four post-v1 items, independent of each other and each gated on a human decision rather than on engineering readiness. **(a) Extract a product-neutral `gh` runner:** move the generic parts of `tools/boss/github/src/gh_runner.rs` into a lower-level crate both `boss_github` and `//tools/release` can depend on, removing the duplicated subprocess wrapper this design knowingly introduces; requires untangling the tokio and `boss_gh_telemetry` coupling and re-pointing `boss_github`'s call sites. **(b) Converge checkleft's version scheme onto `patch-counter`:** move checkleft from `X.Y.Z-alpha.N` to plain `X.Y.Z` and retire the `alpha-counter` enum variant, leaving one scheme in the tool; requires re-pinning every external consumer (`brianduff/appoint`, `brianduff/checkleft-sandbox`) and updating docs that name `checkleft-v*-alpha.*` tags. **(c) Drop the optional-asset tier:** retire `checkleft-x86_64-apple-darwin`, deleting `build_cross_cargo`, the last `cargo` invocation in any release path, and the entire required/optional asset distinction from the tool; requires first establishing whether anything actually fetches that asset. **(d) Bring Boss.app signing and notarisation into the release pipeline:** wire `tools/boss/installer/release.sh`'s codesign / notarytool / stapler / `.pkg` path into CI so releases ship a signed, notarised artifact; requires provisioning Developer ID and Apple notary credentials into Buildkite.

- Effort hint: `large` in aggregate — `medium` for (a) and (b), `small` for (c), `large` for (d).
- Dependencies: (a) and (d) follow task 4; (b) follows task 5; (c) follows task 3.
- Scope: deferred (future / not a v1 blocker) — none of the four is a prerequisite for unification. The runner duplication is small and self-contained and the extraction touches a live Boss crate; the scheme convergence is consumer-visible with no benefit to unification itself and needs an explicit human decision first (see the attentions manifest); the asset retirement is a simplification with an unknown consumer; and signing is explicitly out of scope for this design — it is artifact _building_, is not part of the CI release path today, and needs new credential paths the brief rules out without a separate decision.
