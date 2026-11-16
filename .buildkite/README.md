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
  pipeline-release.yml                # shared release tool: prepare step, fans out to native builds
  pipeline-release-builds.yml         # shared release-tool build fragment, uploaded dynamically by prepare
  REQUIRED_CHECKS.md                  # branch-protection contract for buildkite/mono/<step-key> checks
  steps/
    bazel-build-test.sh    # bazel build //... then bazel test //... (one agent, reuses build outputs)
    mac-app-build.sh       # macOS app build
    checks.sh              # CHECKS.yaml runner (checkleft, no-generated-artifacts, etc.)
    boss-release.sh        # boss release (main only, macos-arm64)
    checkleft-release.sh   # checkleft prebuilt-binary release (prepare/linux/musl/darwin phases)
    release-release.sh     # shared release tool prebuilt-binary release
    ci-env.sh              # shared env/toolchain setup sourced by other steps
    remote-execution.sh    # decides whether a step's bazel actions execute on the remote macOS workers (sourced by ci-env.sh)
    integrity-commit-delta.sh # mono-integrity: commit-delta check
    integrity-bazel.sh        # mono-integrity: full bazel build + test
    integrity-checkleft.sh    # mono-integrity: checkleft check
  README.md               # this file
  linux-agents-runbook.md # Linux bazel-any host config + maintenance runbook
  gce-agent-runbook.md    # Standing up a new Linux CI agent on GCE
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

Sourced by other steps to set up the shared bazel wrapper and CI config rather than run standalone. Sets `CI_BAZEL_STARTUP_FLAGS` (which carries `--max_idle_secs=1800` for every pipeline and OS, CI only, so idle Bazel servers release memory after 30 minutes; local builds get 2 hours from `startup --max_idle_secs=7200` in the root `.bazelrc`, which the command-line flag overrides; the single source of truth for bazel startup options in CI — every code path that shells out to bazel must read from here or the workspace ends up running two daemons at once) and `CI_BAZEL_FLAGS` (the per-command flags: `--config=ci-<os>` plus a `--experimental_disk_cache_gc_max_size` sized to the host — 80% of the filesystem holding the disk cache, computed by `disk-cache-gc-cap.sh` and overriding `.ci.bazelrc`'s fleet-wide `3T`; the step fails if that filesystem cannot be determined), detects and recovers from stale-Xcode-cache failures on macOS, and installs `repobin` tools into `bin/`. `REPOBIN_BAZEL_FLAGS` is set to `CI_BAZEL_FLAGS` so repobin's own bazel calls carry the same config and cap.

It also decides whether the step's bazel actions execute on the remote macOS workers, and appends the remote flags to `CI_BAZEL_FLAGS` when they do; see "Remote execution on macOS agents" below.

### `bazel-build-test.sh`

Runs `bazel build //...` then `bazel test //...` in one step, on one agent. The build phase catches build-graph rot (visibility violations, missing deps, broken generated files) that cargo cannot see; the test phase is the canonical rust test step and, since it includes `engine_lib_test` (`tools/boss/engine/BUILD.bazel:86` — `rust_test(name = "engine_lib_test", crate = ":engine_lib")`), covers the engine lib tests that the 2026-05-12 drift incident exposed, in addition to the integration test targets. Each phase logs under its own collapsible `---` group (`[bazel-build]` / `[bazel-test]`) so a build breakage vs. a test failure stays distinguishable in the log even though they're one Buildkite step. The step's target GitHub commit status is `buildkite/mono/bazel-build-test` — see `REQUIRED_CHECKS.md`.

### `checks.sh`

Runs the `CHECKS.yaml` checks via `checkleft` (or the equivalent runner). Scoped to changed paths on PR builds. Does not invoke `jj`; base-ref detection uses git. checkleft provisions its own SHA-verified, pinned Node runtime.

## Agents and queue

Most steps run on the `bazel-any` queue (`${BUILDKITE_ANY_QUEUE:-bazel-any}` in `pipeline.yml`), a heterogeneous fleet mixing personal Macs and Linux cloud agents — see "Pushing from CI" below for why that matters. `mac-app-build` and `boss-release` pin to `macos-arm64` (`${BUILDKITE_MACOS_QUEUE:-macos-arm64}`) since they need a real Mac toolchain. Each step's `ci-env.sh` / inline setup handles hermetic Rust and Bazel provisioning; see [`gce-agent-runbook.md`](gce-agent-runbook.md) for the real Linux host requirements.

For the Linux `bazel-any` hosts specifically — host inventory, the unprivileged-user-namespace requirement `linux-sandbox` depends on, the Bazel-server-restart procedure, and safe maintenance steps — see [`linux-agents-runbook.md`](linux-agents-runbook.md).

## Remote execution on macOS agents

On a Mac agent the verification steps do not build and test on the agent's own cores. Their bazel actions, tests included, execute on a pool of remote macOS workers, and the agent is a thin Bazel client. The Macs that run agents also serve as workers, and a step that builds locally competes with the worker on the same host: remote builds stall behind it and its own timing-sensitive tests flake. Linux agents are unaffected; there are no Linux workers.

`ci-env.sh` makes the decision once per step, using `remote-execution.sh` (tested by `//.buildkite/steps/tests:remote_execution_test`). A step executes remotely only when all of these hold; otherwise it runs exactly as it did before, on the agent, with the disk cache from `.ci.bazelrc`:

1. **The step opted in** by setting `CI_REMOTE_EXECUTION_ALLOWED=1` before sourcing `ci-env.sh`. `bazel-build-test.sh`, `mac-app-build.sh`, `checks.sh`, `integrity-bazel.sh` and `integrity-checkleft.sh` do. The release steps do not: a step's bazel command line and action environment are sent to the workers and stored in the remote cache, and `boss-release.sh` passes credentials through `--define`. Opting a release step in is a decision about where those values may be stored, not a performance tweak.
2. **The agent is a Mac.**
3. **The agent's environment names an endpoint** in `CI_BAZEL_REMOTE_EXECUTOR` (`grpc://<host>:<port>`), exported by the Buildkite agent's `environment` hook on that host. The endpoint is host configuration and is deliberately not in the repo: every open branch carries its own copy of the CI files, so an endpoint written here would have to be rebased into each of them whenever it moved. Removing the variable from a host's hook returns that host to local builds from its next job, with no merge. A malformed value fails the step rather than falling back.
4. **The endpoint accepts a TCP connection.** If it does not, the step builds on the agent, logs why, and adds a warning annotation to the build.

Each step logs the outcome as its `[ci-env] remote execution on|off: <reason>` group.

When a step executes remotely, `CI_BAZEL_FLAGS` gains `--config=ci-remote --remote_executor=<endpoint> --remote_cache=<endpoint>`. `build:ci-remote` in `.ci.bazelrc` raises `--jobs`, forbids local fallback inside the step, and selects `.bazelrc`'s `remote-tests` config so tests execute on the workers as well. The disk cache stays on, in front of the remote cache.

Things to know when changing a step or a test:

- **Outputs are not on the agent unless the step asks for them.** `bazel_verify_only build|test ...` (defined in `ci-env.sh`) is for commands run only for their result; on a remote step it adds `--remote_download_minimal`. Use plain `bazel build` when the next line runs, uploads or inspects the output. `test.log` and `test.xml` are downloaded either way.
- **A test must be able to run on a worker.** The test strategy is `remote` alone, so a test tagged `local`, `no-remote` or `exclusive` cannot be scheduled and fails the step. Use `exclusive-if-local` for a test that must not share a machine with its siblings when run locally. A test that needs a host tool must reach it the way `tools/test-sandbox/README.md` describes ("Remote execution on macOS workers").
- **Remote and local test results are cached apart** (`MONO_TEST_REMOTE_EXECUTION` in the `remote-tests` config), so a result produced on an agent never satisfies a remote step or the reverse.
- **One output base sees both modes** while old branches and the release steps still build locally. A remote step can read what a local step left in the disk cache, which is why `ci-remote` keeps the disk cache on. The other direction has no such bridge: when a local build needs an output that a remote build left only on the workers, the log shows `Lost inputs no longer available remotely`, and Bazel starts the build again by itself and rebuilds that output locally.

## Pushing from CI (queue heterogeneity and push identity)

The `bazel-any` queue is a **heterogeneous fleet** mixing personal Macs and Linux cloud agents. Both currently push successfully to `spinyfin/mono`:

- **Personal Macs** run the agent under a real developer's user account using their `~/.ssh/` keys, so a push lands attributed to that person's personal identity.
- **Linux cloud agents push successfully too**, on the same ambient-credential path (`git push origin`, no scoped token). This is not a one-off: Buildkite build [1360](https://buildkite.com/flunge/mono-checkleft-release/builds/1360) had its `checkleft-release.sh prepare` phase land on `zoologist-1` (a Linux `bazel-any` agent) and push the `checkleft-v0.1.0-alpha.122` tag without error; sampling the `prepare` phase across builds 1250–1368 in `mono-checkleft-release`'s history shows it landing on a Linux agent (`zoologist-1`, `diziet-1`, `empiricist-1`/`empiricist-2`) in every sampled build, always succeeding. No push failure attributable to a read-only deploy key has been observed in that history.

`spinyfin/mono` itself has **zero deploy keys** registered (`gh api repos/spinyfin/mono/keys` returns none) — pushes from Linux agents are authenticated some other way (the agent's own ambient git/SSH credential, per `checkleft-release.sh`'s comment that "every worker can push to the repo"), not via a repo-level deploy key. The exact credential mechanism on the Linux hosts has not been independently confirmed from the host side (root access would be needed to inspect `buildkite-agent`'s `~/.ssh`), but the observed behavior across the sampled build history is unambiguous: Linux `bazel-any` agents pushing tags to `spinyfin/mono` succeed reliably, not intermittently. `.buildkite/steps/checkleft-release.sh`'s `prepare` phase pushes the release tag with plain `git push origin` on the ambient agent credential — see the comment and `die` message around the `git push origin "refs/tags/${NEW_TAG}"` call in `checkleft-release.sh` — and this has not flapped in the sampled history regardless of which agent (Mac or Linux) it landed on.

Pushes don't fail here, but they still land under different identities depending on which agent class ran the job — if you ever need to attribute a push (or diagnose a future auth failure), read the per-job agent (`bk api "pipelines/<p>/builds/<n>"` → `.jobs[].agent.name`) and the job log's `known_hosts` path (a path under a personal `/Users/<name>/.ssh` indicates a Mac/personal key).

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

The pipeline is canonical — `bazel-build-test` is the source of truth for bazel build+test. `bazel-build-test.sh` uses `--config=ci`, which resolves to `--config=ci-linux` or `--config=ci-darwin` and sets `--disk_cache` to `/mnt/ssd/bazel/disk_cache` (Linux) or `/Volumes/ssd/bazel/disk_cache` (Darwin), defined in `.ci.bazelrc`.
