# Boss/mono Buildkite CI

This directory contains the Buildkite CI pipeline for the mono repo. It mirrors the shape of the [flunge pipeline](../tools/boss/docs/designs/flunge-buildkite-pipeline-reference.md) but adapts for mono's rust + bazel + node surface.

The full design is at [`tools/boss/docs/designs/boss-ci-buildkite-pipeline-mirroring-flunge.md`](../tools/boss/docs/designs/boss-ci-buildkite-pipeline-mirroring-flunge.md).

## Directory layout

```
.buildkite/
  pipeline.yml                        # Main CI pipeline: bazel-build-test, mac-app-build, checks, boss-release
  pipeline-integrity.yml              # mono-integrity: periodic full-repo build/test health check
  pipeline-checkleft-release.yml      # checkleft prebuilt-binary release: prepare step, fans out to builds
  pipeline-checkleft-release-builds.yml # checkleft-release build fragment, uploaded dynamically by prepare
  REQUIRED_CHECKS.md                  # branch-protection contract for buildkite/mono/<step-key> checks
  steps/
    bazel-build-test.sh    # bazel build //... then bazel test //... (one agent, reuses build outputs)
    mac-app-build.sh       # macOS app build
    checks.sh              # CHECKS.yaml runner (checkleft, no-generated-artifacts, etc.)
    boss-release.sh        # boss release (main only, macos-arm64)
    checkleft-release.sh   # checkleft prebuilt-binary release (prepare/linux/musl/darwin phases)
    ci-env.sh              # shared env/toolchain setup sourced by other steps
    integrity-commit-delta.sh # mono-integrity: commit-delta check
    integrity-bazel.sh        # mono-integrity: full bazel build + test
    integrity-checkleft.sh    # mono-integrity: checkleft check
  README.md             # this file
```

## Pipeline shape

```
┬──► bazel-build-test (build then test, one agent) ──┐
├──► mac-app-build                                   ├──► boss-release (main only)
└──► checks                                          ┘
```

- `bazel-build-test`, `mac-app-build`, and `checks` all run in parallel with no barrier between them.
- `bazel-build-test` runs `bazel build` then `bazel test` back to back on the same agent, so the test phase reuses the build phase's local bazel outputs instead of re-analyzing/rebuilding on a different `bazel-any` host.
- `boss-release` (main branch only) explicitly `depends_on` all three so a release only happens once bazel, checks, and the mac app build have all gone green.

## Step details

### `ci-env.sh`

Sourced by other steps to set up the shared bazel wrapper and CI config rather than run standalone. Sets `CI_BAZEL_STARTUP_FLAGS` (the single source of truth for bazel startup options in CI — every code path that shells out to bazel must read from here or the workspace ends up running two daemons at once), detects and recovers from stale-Xcode-cache failures on macOS, and installs `repobin` tools into `bin/`.

### `bazel-build-test.sh`

Runs `bazel build //...` then `bazel test //...` in one step, on one agent. The build phase catches build-graph rot (visibility violations, missing deps, broken generated files) that cargo cannot see; the test phase is the canonical rust test step and, with P1 landed (`tools/boss/engine/BUILD.bazel:86` — `rust_test(name = "engine_lib_test", crate = ":engine_lib")`), covers the engine lib tests that the 2026-05-12 drift incident exposed, in addition to the integration test targets. Each phase logs under its own collapsible `---` group (`[bazel-build]` / `[bazel-test]`) so a build breakage vs. a test failure stays distinguishable in the log even though they're one Buildkite step. The step's target GitHub commit status is `buildkite/mono/bazel-build-test` — see `REQUIRED_CHECKS.md`.

### `checks.sh`

Runs the `CHECKS.yaml` checks via `checkleft` (or the equivalent runner). Scoped to changed paths on PR builds. Does not invoke `jj`; base-ref detection uses git.

## Agents and queue

Most steps run on the `bazel-any` queue (`${BUILDKITE_ANY_QUEUE:-bazel-any}` in `pipeline.yml`), a heterogeneous fleet mixing personal Macs and Linux cloud agents — see "Pushing from CI" below for why that matters. `mac-app-build` and `boss-release` pin to `macos-arm64` (`${BUILDKITE_MACOS_QUEUE:-macos-arm64}`) since they need a real Mac toolchain. Each step's `ci-env.sh` / inline setup handles toolchain provisioning (rust, bazel, pnpm) on whatever agent it lands on.

## Pushing from CI (queue heterogeneity)

The `bazel-any` queue is a **heterogeneous fleet**, and that determines whether a `git push` to `spinyfin/mono` from a step running on it succeeds:

- **Personal Macs** run the agent under a real developer's user account using their `~/.ssh/` keys. A repo admin's Mac pushes **with write access** and succeeds — but the push lands attributed to that person's personal identity.
- **Linux cloud agents** are bootstrapped with a **read-only** deploy key. Their pushes are **denied** ("Permission to spinyfin/mono.git denied to deploy key").

`spinyfin/mono` itself has **zero deploy keys** registered (`gh api repos/spinyfin/mono/keys` returns none) — read works via the agent's ambient credentials, but there is no write-capable deploy key on the repo.

Consequence: any CI step on `bazel-any` that pushes flaps — green when the step lands on a Mac, "denied to deploy key" when it lands on a Linux cloud agent. A passing run does NOT prove a push-auth fix; it may just have landed on a Mac. `.buildkite/steps/checkleft-release.sh` has this exposure today: `pipeline-checkleft-release.yml` runs every phase on `queue: bazel-any` (pinned to an OS by agent tag), and the `prepare` phase pushes the release tag with plain `git push origin` on the ambient agent credential — see the comment and `die` message around the `git push origin "refs/tags/${NEW_TAG}"` call in `checkleft-release.sh`. By contrast `boss-release` runs on `macos-arm64`, so it always lands on a Mac and never flaps.

The deterministic fix is to push over HTTPS with a scoped `GITHUB_TOKEN` (Contents: write) injected as a Buildkite secret, so the push works on every agent and stops borrowing a personal identity. That fix has not landed for `checkleft-release.sh`.

Diagnose push/auth flakiness by reading the per-job agent (`bk api "pipelines/<p>/builds/<n>"` → `.jobs[].agent.name`) and the job log's `known_hosts` path (a path under a personal `/Users/<name>/.ssh` indicates a Mac/personal key).

## Debugging a red build locally

Each `steps/*.sh` script can be run directly from the repo root. To reproduce bazel steps with CI config:

```sh
# Reproduce bazel step with CI config
bazel test //... --config=ci
```

The CI config is in `.bazelrc.ci`.

## Required checks (branch protection)

Required checks are managed via branch protection rules. The check names buildkite reports are `buildkite/mono/<step-key>`, e.g. `buildkite/mono/bazel-build-test`. Treat these as a public contract — renaming a step key in `pipeline.yml` requires updating branch protection in lockstep.

## Status

The pipeline is canonical — `bazel-build-test` is the source of truth for bazel build+test. `bazel-build-test.sh` uses `--config=ci` which sets `--disk_cache=/var/cache/bazel-mono` (defined in `.bazelrc`).
