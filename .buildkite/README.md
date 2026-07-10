# Boss/mono Buildkite CI

This directory contains the Buildkite CI pipeline for the mono repo. It mirrors the shape of the [flunge pipeline](../tools/boss/docs/designs/flunge-buildkite-pipeline-reference.md) but adapts for mono's rust + bazel + node surface.

The full design is at [`tools/boss/docs/designs/boss-ci-buildkite-pipeline-mirroring-flunge.md`](../tools/boss/docs/designs/boss-ci-buildkite-pipeline-mirroring-flunge.md).

## Directory layout

```
.buildkite/
  pipeline.yml          # Buildkite reads this; declares steps, queue tags, depends_on only
  steps/
    bootstrap.sh        # Prime the agent: rust toolchain, bazelisk, pnpm, cache restore
    bazel-build-test.sh # bazel build //... then bazel test //... (one agent, reuses build outputs)
    checks.sh           # CHECKS.yaml runner (checkleft, no-generated-artifacts, etc.)
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

### `bootstrap.sh`

Ensures the agent has the required toolchain:

- Rust: installs / pins via `rust-toolchain.toml` using `rustup`.
- Bazel: `bazelisk` should be on `$PATH`; version is read from `.bazelversion`.
- pnpm: installs if not present, pins to the version in `package.json#packageManager`.
- Restores the agent-local bazel disk cache (uses `~/.cache/bazelcache` from `.bazelrc`).

### `bazel-build-test.sh`

Runs `bazel build //...` then `bazel test //...` in one step, on one agent. The build phase catches build-graph rot (visibility violations, missing deps, broken generated files) that cargo cannot see; the test phase is the canonical rust test step and, with P1 landed (`tools/boss/engine/BUILD.bazel:86` — `rust_test(name = "engine_lib_test", crate = ":engine_lib")`), covers the engine lib tests that the 2026-05-12 drift incident exposed, in addition to the integration test targets. Each phase logs under its own collapsible `---` group (`[bazel-build]` / `[bazel-test]`) so a build breakage vs. a test failure stays distinguishable in the log even though they're one Buildkite step. The step emits a single GitHub commit status, `buildkite/mono/bazel-build-test` — see `REQUIRED_CHECKS.md`.

### `checks.sh`

Runs the `CHECKS.yaml` checks via `checkleft` (or the equivalent runner). Scoped to changed paths on PR builds. Does not invoke `jj`; base-ref detection uses git.

## Agents and queue

All steps run on `queue=linux-amd64`. The `bootstrap.sh` step handles toolchain setup (rust, bazel, pnpm, cache restoration).

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
