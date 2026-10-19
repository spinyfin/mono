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
    host-bazel-lock.sh     # one heavy Bazel job per Mac host at a time (taken by ci-env.sh)
    tests/                 # bazel sh_tests for the step scripts (host_bazel_lock_test)
    integrity-commit-delta.sh # mono-integrity: commit-delta check
    integrity-bazel.sh        # mono-integrity: full bazel build + test
    integrity-checkleft.sh    # mono-integrity: checkleft check
  README.md               # this file
  linux-agents-runbook.md # Linux bazel-any host config + maintenance runbook
  mac-agents-runbook.md   # Mac host layout, the one-heavy-job-per-host rule, host config
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

Sourced by other steps to set up the shared bazel wrapper and CI config rather than run standalone. Takes the per-host heavy-Bazel lock on macOS before anything else (see `host-bazel-lock.sh` below), sets `CI_BAZEL_STARTUP_FLAGS` (which carries `--max_idle_secs=1800` for every pipeline and OS, CI only, so idle Bazel servers release memory after 30 minutes; the single source of truth for bazel startup options in CI — every code path that shells out to bazel must read from here or the workspace ends up running two daemons at once), detects and recovers from stale-Xcode-cache failures on macOS, and installs `repobin` tools into `bin/`.

### `host-bazel-lock.sh`

Enforces **one heavy Bazel job per Mac host at a time**. "Heavy" is any step that runs Bazel — a build, a test, or a checkleft run (whose `lint/rust` check builds the clippy aspect through Bazel) — which in this repo is exactly the set of steps that source `ci-env.sh`. On Darwin, `ci-env.sh` takes a host-wide `mkdir` lock under `${HOME}/.cache/buildkite-host-bazel-lock/` before the first Bazel invocation and holds it until the step exits; a second heavy job scheduled onto the same host parks with a visible `--- [host-bazel-lock] waiting` group in its log until the first finishes. Linux hosts are not throttled. Stale locks (dead pid, pre-reboot, ownerless; a live holder is never evicted) are reclaimed automatically. The library is self-contained so sibling repos sharing the Mac hosts (appoint, flunge) can copy it verbatim; the lock directory is the cross-pipeline contract. Background, measurements, host configuration and the adoption checklist are in [`mac-agents-runbook.md`](mac-agents-runbook.md).

### `bazel-build-test.sh`

Runs `bazel build //...` then `bazel test //...` in one step, on one agent. The build phase catches build-graph rot (visibility violations, missing deps, broken generated files) that cargo cannot see; the test phase is the canonical rust test step and, including the engine lib tests (`rust_test(name = "engine_lib_test", crate = ":engine_lib")` in `tools/boss/engine/BUILD.bazel`), covers the engine lib tests that the 2026-05-12 drift incident exposed, in addition to the integration test targets. Each phase logs under its own collapsible `---` group (`[bazel-build]` / `[bazel-test]`) so a build breakage vs. a test failure stays distinguishable in the log even though they're one Buildkite step. The step's target GitHub commit status is `buildkite/mono/bazel-build-test` — see `REQUIRED_CHECKS.md`.

### `checks.sh`

Runs the `CHECKS.yaml` checks via `checkleft` (or the equivalent runner). Scoped to changed paths on PR builds. Does not invoke `jj`; base-ref detection uses git. checkleft provisions its own SHA-verified, pinned Node runtime.

## Agents and queue

Most steps run on the `bazel-any` queue (`${BUILDKITE_ANY_QUEUE:-bazel-any}` in `pipeline.yml`), a heterogeneous fleet mixing personal Macs and Linux cloud agents — see "Pushing from CI" below for why that matters. `mac-app-build` and `boss-release` pin to `macos-arm64` (`${BUILDKITE_MACOS_QUEUE:-macos-arm64}`) since they need a real Mac toolchain. Each step's `ci-env.sh` / inline setup handles hermetic Rust and Bazel provisioning; see [`gce-agent-runbook.md`](gce-agent-runbook.md) for the real Linux host requirements.

For the Linux `bazel-any` hosts specifically — host inventory, the unprivileged-user-namespace requirement `linux-sandbox` depends on, the Bazel-server-restart procedure, and safe maintenance steps — see [`linux-agents-runbook.md`](linux-agents-runbook.md).

The Mac hosts (`anaplian`, `skaffen`) each run two `macos-arm64` agents and two `bazel-any` agents, so up to four jobs can land on one 16 GB machine; a commit that invalidates `engine_lib` then has every one of them compile it at once, which has exhausted memory and timed out jobs repeatedly. Heavy jobs on a Mac host are therefore serialised by `host-bazel-lock.sh` (above). Host inventory, the agent process layout, the recommended one-spawn-per-queue host configuration and the measurements are in [`mac-agents-runbook.md`](mac-agents-runbook.md).

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
