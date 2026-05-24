# Boss/mono Buildkite CI

This directory contains the Buildkite CI pipeline for the mono repo. It mirrors the shape of the [flunge pipeline](../tools/boss/docs/designs/flunge-buildkite-pipeline-reference.md) but adapts for mono's rust + bazel + node surface.

The full design is at [`tools/boss/docs/designs/boss-ci-buildkite-pipeline-mirroring-flunge.md`](../tools/boss/docs/designs/boss-ci-buildkite-pipeline-mirroring-flunge.md).

## Directory layout

```
.buildkite/
  pipeline.yml          # Buildkite reads this; declares steps, queue tags, depends_on only
  steps/
    bootstrap.sh        # Prime the agent: rust toolchain, bazelisk, pnpm, cache restore
    bazel-build.sh      # bazel build //... (dependency-graph compile guard)
    bazel-test.sh       # bazel test //... (canonical rust + integration tests)
    checks.sh           # CHECKS.yaml runner (checkleft, no-generated-artifacts, etc.)
    boss-release.sh     # post-merge: build Boss.app with credentials, publish GitHub Release
  README.md             # this file
```

## Pipeline shape

```
bootstrap (queue=linux-amd64)┬──► bazel-build ──┐
                             ├──► mac-app-build ─┼──► (wait) ──► bazel-test ──► green
                             ├──► checks      ──┘                     │
                                                                       └──► boss-release (main only)
```

- `bootstrap` runs first; all other steps depend on it.
- `bazel-build`, `mac-app-build`, and `checks` run in parallel after bootstrap.
- `bazel-test` runs only after all static checks pass (the `wait` step).
- `boss-release` runs after `bazel-test` passes, but only on the `main` branch. It rebuilds Boss.app with shake credentials embedded and publishes a versioned GitHub Release.

## Step details

### `bootstrap.sh`

Ensures the agent has the required toolchain:
- Rust: installs / pins via `rust-toolchain.toml` using `rustup`.
- Bazel: `bazelisk` should be on `$PATH`; version is read from `.bazelversion`.
- pnpm: installs if not present, pins to the version in `package.json#packageManager`.
- Restores the agent-local bazel disk cache (uses `~/.cache/bazelcache` from `.bazelrc`).

### `bazel-build.sh`

Runs `bazel build //...`. Catches build-graph rot (visibility violations, missing deps, broken generated files) that cargo cannot see.

### `bazel-test.sh`

Runs `bazel test //...`. This is the canonical rust test step. With P1 landed (`tools/boss/engine/BUILD.bazel:86` — `rust_test(name = "engine_lib_test", crate = ":engine_lib")`), this covers the engine lib tests that the 2026-05-12 drift incident exposed, in addition to the integration test targets.

### `checks.sh`

Runs the `CHECKS.yaml` checks via `checkleft` (or the equivalent runner). Scoped to changed paths on PR builds. Does not invoke `jj`; base-ref detection uses git.

### `boss-release.sh`

Post-merge only (guarded by `if: build.branch == 'main'` in `pipeline.yml`). Runs on `macos-arm64` after `bazel-test` passes.

Reads three secrets from the pipeline environment (`BOSS_SHAKE_APP_ID`, `BOSS_SHAKE_INSTALLATION_ID`, `BOSS_SHAKE_PRIVATE_KEY_PEM`), rebuilds `//tools/boss/app-macos:Boss` with those credentials embedded at compile time, packages the resulting `.app` using `ditto`, computes the next sequential `v1.0.N` tag, and creates a GitHub Release on `spinyfin/mono` with the zipped `.app` as the artifact.

See `tools/boss/docs/buildkite-shake-secrets-setup.md` for one-time provisioning instructions.

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

Required checks are managed via branch protection rules. The check names buildkite reports are `buildkite/mono/<step-key>`, e.g. `buildkite/mono/bazel-build`. Treat these as a public contract — renaming a step key in `pipeline.yml` requires updating branch protection in lockstep.

## Status

The pipeline is canonical — `bazel-build` and `bazel-test` are the source of truth. `bazel-build.sh` uses `--config=ci` which sets `--disk_cache=/var/cache/bazel-mono` (defined in `.bazelrc`).
