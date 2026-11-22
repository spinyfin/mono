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
    bazel-server-lock.sh   # stops a bazel server an earlier job left holding the command lock (sourced by ci-env.sh)
    fetch-boss-tags.sh     # fetches and prunes boss-v* tags from origin, with bounded retries (sourced by ci-env.sh)
    verify-boss-tag-cache.sh # opt-in sequential cache comparison on two macOS bazel-any agents
    remote-execution.sh    # decides whether a step's bazel actions execute on remote workers (sourced by ci-env.sh)
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

### `verify-boss-tag-cache.sh`

CI operators can set `BOSS_VERIFY_TAG_CACHE=1` when creating a build of a specific commit to enable two diagnostic jobs on compatible `bazel-any` agents. The default platform is macOS arm64; set `BOSS_TAG_CACHE_OS=linux` and `BOSS_TAG_CACHE_ARCH=amd64` to compare Linux agents instead. The jobs rendezvous while running and both fail if they landed on one host (re-run the build in that case), then the second waits for the first to populate the cache. Both use the normal CI cache configuration, print the derived workspace status, and build and test the version-dependent CLI. Each uploads workspace status, Bazel execution logs, profiles, and `host.txt` (the host it ran on) as artifacts; compare these alongside the action-cache counts and cached-test results in the job logs. Each also uploads the test runtime's `fingerprint` and `manifest` (`tools/test-sandbox/README.md`, "The fingerprint"). Those two files are inputs of every test action and describe the agent's own host, so a test the second agent runs again, although the first has just run it, is explained by comparing them. Distinct agents are not always distinct hosts (two agents of one host share its files and its disk cache), which is why the probe requires distinct hosts. These diagnostic jobs are skipped in ordinary builds.

### `ci-env.sh`

Sourced by other steps to set up the shared bazel wrapper and CI config rather than run standalone. Sets `CI_BAZEL_STARTUP_FLAGS` (which carries `--max_idle_secs=1800` for every pipeline and OS, CI only, so idle Bazel servers release memory after 30 minutes; local builds get 2 hours from `startup --max_idle_secs=7200` in the root `.bazelrc`, which the command-line flag overrides; the single source of truth for bazel startup options in CI — every code path that shells out to bazel must read from here or the workspace ends up running two daemons at once) and `CI_BAZEL_FLAGS` (the per-command flags: `--config=ci-<os>` plus a `--experimental_disk_cache_gc_max_size` sized to the host — 80% of the filesystem holding the disk cache, computed by `disk-cache-gc-cap.sh` and overriding `.ci.bazelrc`'s fleet-wide `3T`; the step fails if that filesystem cannot be determined), detects and recovers from stale-Xcode-cache failures on macOS, stops a bazel server still held by an earlier job's command (`bazel-server-lock.sh`; without it every bazel call waits on the lock until the step timeout), and installs `repobin` tools into `bin/`. `REPOBIN_BAZEL_FLAGS` is set to `CI_BAZEL_FLAGS` so repobin's own bazel calls carry the same config and cap.

It also decides whether the step's bazel actions execute on remote workers, and appends the remote flags to `CI_BAZEL_FLAGS` when they do; see "Remote execution on macOS agents" and "Remote execution on Linux agents" below.

#### Revalidating a build that predates lock recovery

Rebuilding an old commit runs that commit's checked-in pipeline and step scripts. Merging a recovery change into main does not install it into an unchanged old head. The CI operator must either recover the affected host before rebuilding the unchanged commit, or have the PR maintainer rebase onto the merged CI setup and push to trigger normal required checks on the new head.

For a host that still holds an earlier job's Bazel command lock, the host owner or CI operator must inspect the affected agent and verify the server's live argv against its recorded PID, Bazel server marker, server jar, exact workspace and output-base arguments before stopping it. Recheck that identity before escalating from SIGTERM to SIGKILL. If identity cannot be established, pause the affected agent and reschedule the build on a healthy agent instead; do not signal an unverified process. Coordinate pausing with any currently running job. Record the replacement build URL and commit SHA, and verify the required `buildkite/mono/bazel-build-test` result rather than treating a queued rebuild as a passing run.

### `bazel-build-test.sh`

Runs `bazel build //...` then `bazel test //...` in one step, on one agent. The build phase catches build-graph rot (visibility violations, missing deps, broken generated files) that cargo cannot see; the test phase is the canonical rust test step and, since it includes `engine_lib_test` (`tools/boss/engine/BUILD.bazel:86` — `rust_test(name = "engine_lib_test", crate = ":engine_lib")`), covers the engine lib tests that the 2026-05-12 drift incident exposed, in addition to the integration test targets. Each phase logs under its own collapsible `---` group (`[bazel-build]` / `[bazel-test]`) so a build breakage vs. a test failure stays distinguishable in the log even though they're one Buildkite step. The step's target GitHub commit status is `buildkite/mono/bazel-build-test` — see `REQUIRED_CHECKS.md`.

A test that fails and then passes is retried once (`--flaky_test_attempts=2`), and Bazel counts it as passed: the summary reads "N tests pass" and the step is green. `report-flaky-tests.py` is what keeps such a run from looking clean. It reads the test phase's build events and, for every target Bazel marked `FLAKY`, uploads each attempt's `test.log` and `test.xml` under `flaky-logs/`, uploads `flaky-tests.json`, adds a warning annotation to the build page, and prints an open `+++ [bazel-test] N flaky test target(s)` group in the step's log. Both name the failing test case. For a Rust target that name comes from the failed attempt's log, because the `test.xml` Bazel writes for a test binary names only the wrapper script.

### `checks.sh`

Runs the `CHECKS.yaml` checks via `checkleft` (or the equivalent runner). Scoped to changed paths on PR builds. Does not invoke `jj`; base-ref detection uses git. checkleft provisions its own SHA-verified, pinned Node runtime.

## Agents and queue

Most steps run on the `bazel-any` queue (`${BUILDKITE_ANY_QUEUE:-bazel-any}` in `pipeline.yml`), a heterogeneous fleet mixing personal Macs and Linux cloud agents — see "Pushing from CI" below for why that matters. `mac-app-build` and `boss-release` pin to `macos-arm64` (`${BUILDKITE_MACOS_QUEUE:-macos-arm64}`) since they need a real Mac toolchain. Each step's `ci-env.sh` / inline setup handles hermetic Rust and Bazel provisioning; see [`gce-agent-runbook.md`](gce-agent-runbook.md) for the real Linux host requirements.

For the Linux `bazel-any` hosts specifically — host inventory, the unprivileged-user-namespace requirement `linux-sandbox` depends on, the Bazel-server-restart procedure, and safe maintenance steps — see [`linux-agents-runbook.md`](linux-agents-runbook.md).

## Remote execution on macOS agents

On a Mac agent the verification steps do not build and test on the agent's own cores. Their bazel actions, tests included, execute on a pool of remote macOS workers, and the agent is a thin Bazel client. The Macs that run agents also serve as workers, and a step that builds locally competes with the worker on the same host: remote builds stall behind it and its own timing-sensitive tests flake. Linux agents have workers of their own and a narrower arrangement; see "Remote execution on Linux agents" below.

`ci-env.sh` makes the decision once per step, using `remote-execution.sh` (tested by `//.buildkite/steps/tests:remote_execution_test`). A step executes remotely only when all of these hold; otherwise it runs exactly as it did before, on the agent, with the disk cache from `.ci.bazelrc`:

1. **The step opted in** by setting `CI_REMOTE_EXECUTION_ALLOWED=1` before sourcing `ci-env.sh`. `bazel-build-test.sh`, `mac-app-build.sh`, `checks.sh`, `integrity-bazel.sh`, `integrity-checkleft.sh` and `boss-release.sh` do. Opting in is explicit because a step's bazel command line and action environment are sent to the workers and stored in the remote cache, so for a step that passes credentials to bazel it is a decision about where those values may be stored, not a performance tweak. `boss-release.sh` passes the shake credentials through `--define` and opts in all the same: those values are compiled into the app it publishes, so the cache holds nothing the release does not. The other release steps (`changelog-release.sh`, `checkleft-release.sh`, `release-release.sh`, `repobin-release.sh`, `spoke-release.sh`) have not been through that review and build on the agent.
2. **There are workers for the agent's platform**: macOS, or Linux on x86-64. An agent only ever uses workers of its own platform, because the workers run its actions with its own toolchain paths.
3. **The agent's environment names an endpoint** in `CI_BAZEL_REMOTE_EXECUTOR` (`grpc://<host>:<port>`), exported by the Buildkite agent's `environment` hook on that host. The endpoint is host configuration and is deliberately not in the repo: every open branch carries its own copy of the CI files, so an endpoint written here would have to be rebased into each of them whenever it moved. Removing the variable from a host's hook returns that host to local builds from its next job, with no merge. A malformed value fails the step rather than falling back.
4. **The endpoint accepts a TCP connection and answers a request for its status page.** If it does not accept a connection, does not answer within 10 s (curl exit 28), or answers that a store has failed (HTTP 503), the step builds on the agent, logs why, and adds a warning annotation to the build. The connection alone proves little: the kernel accepts one for a process that has stopped answering, and a bazel command sent to such a process has no per-RPC deadline. The same status request is repeated while the step's bazel commands run, so a hub that goes silent after the step has started is stopped with an annotation that names the executor. An endpoint that answers but serves no status page on that port is used as before, as is a probe that fails for any other reason (reset, TLS, HTTP/2-only): today's 1.6.6 hubs have no status page on the client port.

Each step logs the outcome as its `[ci-env] remote execution on|off: <reason>` group.

A step can also lose the executor after it has started. Three things bound that:

- **The scheduler fails an action it cannot run or keep running** (no worker connected, a worker lost too often, too many retries) within minutes, and the step fails with that error. `ci-env.sh` recognises these errors in the log of a failed bazel command (`ci_remote_failure_reason` in `remote-execution.sh`) and adds an error annotation saying that the executor failed, not an action of the change. Retry such a step once the executor is healthy.
- **The status watchdog in `ci-env.sh`** repeats the start-of-step status request every 15 s while the step runs, for every bazel client the step started — the `bazel()` wrapper and the clients repobin and checkleft launch with `Command::new("bazel")`. If the hub accepted a connection and then does not answer (curl exit 28) or answers 503, those clients are stopped and an error annotation names the executor's host, port and probe result. An unreachable hub is not treated as stuck. gRPC keepalive (`--grpc_keepalive_time=30s` on `ci-remote` / `ci-remote-linux`) is still set so the values live in the rc files; it was measured not to end a wait against a stopped hub (17 minutes with the 60s default).
- **`timeout_in_minutes` on every remote-enabled step** (`bazel-build-test`, `mac-app-build`, `checks`, `boss-release` in `pipeline.yml`; `integrity-bazel` and `integrity-checkleft` in the integrity pipeline) is the last resort if the watchdog cannot run. A Buildkite SIGTERM then names the executor immediately and runs a short (3 s) status probe to amend the annotation, so a silent hub cannot use up the cancel grace period before the log line is written. The verification-step values are about four times the slowest passing run of each step, not a budget to tune builds against.

The hub, its scheduler timeouts, its health check and what to do when it is unhealthy are in [`tools/remote-execution/README.md`](../tools/remote-execution/README.md).

When a step executes remotely, `CI_BAZEL_FLAGS` gains `--config=ci-remote --remote_executor=<endpoint> --remote_cache=<endpoint>`. `build:ci-remote` in `.ci.bazelrc` raises `--jobs`, forbids local fallback inside the step, selects `.bazelrc`'s `remote-tests` config so tests execute on the workers as well, sets `--grpc_keepalive_time=30s`, and turns on `--remote_cache_compression`, so blobs travel between the agent and the remote cache zstd-compressed. The disk cache stays on, in front of the remote cache.

Compression makes zstd support a requirement of the endpoint. Against an endpoint that does not advertise it, Bazel 9.1 fails the first action that touches the remote cache with `--remote_cache_compression requested but remote does not support compression`, so compression has to stay on at the endpoint for as long as `ci-remote` asks for it.

Things to know when changing a step or a test:

- **Outputs are not on the agent unless the step asks for them.** `bazel_verify_only build|test ...` (defined in `ci-env.sh`) is for commands run only for their result; on a remote step it adds `--remote_download_minimal`. Use plain `bazel build` when the next line runs, uploads or inspects the output. `test.log` and `test.xml` are downloaded either way.
- **A test must be able to run on a worker.** The test strategy is `remote` alone, so a test tagged `local`, `no-remote` or `exclusive` cannot be scheduled and fails the step. Use `exclusive-if-local` for a test that must not share a machine with its siblings when run locally. A test that needs a host tool must reach it the way `tools/test-sandbox/README.md` describes ("Remote execution on macOS workers").
- **Remote and local test results are cached apart** (`MONO_TEST_REMOTE_EXECUTION` in the `remote-tests` config), so a result produced on an agent never satisfies a remote step or the reverse.
- **The app release may finish on the agent.** `boss-release.sh` adds `--remote_local_fallback`, overriding `ci-remote`: if the workers stop answering part-way through its build, the remaining actions run on the agent. By then the tag is pushed and the draft exists, and a scheduled run does not resume a draft, so failing would leave the release waiting for a manual run. An action that runs and fails still fails the step.
- **One output base sees both modes** while old branches and the other release steps still build locally. A remote step can read what a local step left in the disk cache, which is why `ci-remote` keeps the disk cache on. The other direction has no such bridge: when a local build needs an output that a remote build left only on the workers, the log shows `Lost inputs no longer available remotely`, and Bazel starts the build again by itself and rebuilds that output locally.

## Remote execution on Linux agents

A Linux x86-64 agent sends its bazel actions, tests included, to a pool of remote Linux workers and is left as a thin Bazel client. The decision is the same one, made by the same code under the same four conditions as on a Mac: the step opted in, the platform has workers, the agent's `environment` hook exports `CI_BAZEL_REMOTE_EXECUTOR`, and the endpoint accepts a connection. The Linux agents and the Macs name the same hub: one scheduler and one cache serve both. What keeps a Linux action off a Mac and the reverse is the `OSFamily` property, which every worker declares and the scheduler matches exactly. Linux builds send it; the macOS builds send nothing, and the hub fills in `macos` for an action that arrives without it ([`tools/remote-execution/README.md`](../tools/remote-execution/README.md), "Routing").

When a Linux step executes remotely, `CI_BAZEL_FLAGS` gains `--config=ci-remote-linux --remote_executor=<endpoint> --remote_cache=<endpoint>`. `build:ci-remote-linux` in `.ci.bazelrc` is `ci-remote` with one addition, and its tests rest on a different boundary:

- **Every action is tagged `OSFamily=linux`** (`--remote_default_exec_properties`). The scheduler matches that property exactly against what each worker declares, and an action without it is scheduled as a macOS action, so a Linux build that lost the tag would wait for a Mac. The tag is also part of every action's cache key, so a remote step and a step that built on the agent never share a result, in the remote cache or in the disk cache. The first remote step on a host therefore starts cold even if that host's disk cache is warm.
- **Tests run on the workers, inside the workers' own sandbox.** `test:ci-remote-linux --config=remote-tests` replaces `test:linux --strategy=TestRunner=linux-sandbox` for the step, so Bazel's `linux-sandbox`, which is the isolation boundary for a test on an agent, is no longer around the test. Every Linux worker instead runs each action, build or test, inside [`tools/test-sandbox/linux_remote_action_sandbox.sh`](../tools/test-sandbox/linux_remote_action_sandbox.sh): a read-only host, a private `/tmp`, new PID, IPC and network namespaces, and no view of `/home` or of the sockets under `/run`. The config is only sound while every worker on the endpoint runs that script; `tools/test-sandbox/README.md`, "Remote execution on Linux workers", has what it enforces and the gate to run before trusting a worker.

The things to know when changing a step or a test are the ones listed for the Macs above, with these differences:

- **A test must be able to run on a worker**, as on a Mac: the strategy is `remote` alone, so a test tagged `local`, `no-remote` or `exclusive` cannot be scheduled.
- **The network opt-in is the environment variable, not the tag.** A worker never sees a target's tags, so `requires-network` does nothing there. The worker's sandbox keeps the network for an action whose environment carries `MONO_TEST_ALLOW_NETWORK=1`, which `network_enabled_rust_test` sets along with the tag.
- **A leaked process does not outlive its action.** The action's PID namespace is torn down when its command exits.

The workers compile with the same host C toolchain the agents use (the C/C++ toolchain is the one Bazel detects on the agent, recorded by absolute path), so every Linux agent and worker must have the same compiler at the same paths. That is the same "identical host configuration" rule the Linux hosts already follow; see [`linux-agents-runbook.md`](linux-agents-runbook.md), which also covers where the workers run, how to install the sandbox and restart a worker, and how to switch an agent between remote and local builds.

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

The CI config is in `.ci.bazelrc`.

### Diffing Bazel execution logs to diagnose cache misses

Every Bazel step sourcing `ci-env.sh` records a natively zstd-compressed compact execution log (`--execution_log_compact_file`) for each `build`, `test`, `run`, or `coverage` invocation under `bazel-execlogs/<job-id>/<seq>-<subcommand>.bin.zst` and uploads it as a Buildkite artifact (`01-build.bin.zst` is `ci-env.sh`'s `//tools/repobin:repobin` build; subsequent numbers are the step's own invocations). The flag does not alter action keys or cacheability, and `ci-env.sh` skips adding it when a caller already passes `--execution_log_*` (such as `verify-boss-tag-cache.sh`).

To find why actions missed the cache between two builds of the same commit:

1. Download the matching `.bin.zst` artifact from both Buildkite jobs (for example `02-build.bin.zst` or `03-test.bin.zst`).
2. From a `bazelbuild/bazel` checkout, convert and align both compact logs with Bazel's execlog parser, then diff the text output:

```sh
bazel run //src/tools/execlog:parser -- \
  --log_path=/path/to/build-a/03-test.bin.zst \
  --log_path=/path/to/build-b/03-test.bin.zst \
  --output_path=/tmp/build-a.txt \
  --output_path=/tmp/build-b.txt

diff -u /tmp/build-a.txt /tmp/build-b.txt
```

Look for spawns whose `cache_hit` flipped from `true` to `false`, and compare their `inputs` digests, `command_args`, and `environment_variables` in the diff.

## Required checks (branch protection)

Required checks are managed via branch protection rules. The check names buildkite reports are `buildkite/mono/<step-key>`, e.g. `buildkite/mono/bazel-build-test`. Treat these as a public contract — renaming a step key in `pipeline.yml` requires updating branch protection in lockstep.

## Status

The pipeline is canonical — `bazel-build-test` is the source of truth for bazel build+test. `bazel-build-test.sh` uses `--config=ci`, which resolves to `--config=ci-linux` or `--config=ci-darwin` and sets `--disk_cache` to `/mnt/ssd/bazel/disk_cache` (Linux) or `/Volumes/ssd/bazel/disk_cache` (Darwin), defined in `.ci.bazelrc`.
