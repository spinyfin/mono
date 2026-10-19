# Mac CI agent host runbook

This runbook covers the macOS hosts behind the `macos-arm64` and `bazel-any` Buildkite queues: what runs on them, the **one heavy Bazel job per Mac host at a time** rule, how that rule is enforced, and what an operator must apply on a host for it to hold. The Linux side of the fleet is in [`linux-agents-runbook.md`](linux-agents-runbook.md); standing up a new Linux agent is in [`gce-agent-runbook.md`](gce-agent-runbook.md).

Live agent facts below were read from `bk agent list --output json` on 2026-10-19. The agent process layout was read from a comparable Mac agent install (the per-user launchd + Homebrew config layout this fleet was set up from); it was **not** verified on `anaplian` or `skaffen` themselves, because neither host is SSH-reachable from the workspace this was written in. Run the verification commands in "Verify the host layout" before changing anything.

## Host inventory

Two Mac hosts, four agent registrations each. Each host runs the same Buildkite agent version (3.127.1 at time of writing) and registers `os=darwin,arch=arm64`.

| Host       | `macos-arm64` agents                   | `bazel-any` agents                             | Hardware                        |
| ---------- | -------------------------------------- | ---------------------------------------------- | ------------------------------- |
| `skaffen`  | `skaffen.local-1`, `skaffen.local-2`   | `skaffen.local-1-any`, `skaffen.local-2-any`   | 10 cores, 16 GB (from incident) |
| `anaplian` | `anaplian.local-1`, `anaplian.local-2` | `anaplian.local-1-any`, `anaplian.local-2-any` | not recorded; check `sysctl`    |

The agent names follow Buildkite's `%hostname-%spawn` pattern (`-any` suffix for the `bazel-any` process), which is what the two-process layout below produces. A Buildkite agent belongs to exactly one cluster queue, so a host that serves both queues necessarily runs two agent processes.

Pipelines that put Bazel work on these hosts:

- `mono` (`bazel-build-test` and `checks` via `bazel-any`; `mac-app-build` and `boss-release` via `macos-arm64`)
- `mono-integrity` (`integrity-bazel` via `macos-arm64`; `integrity-checkleft` via `bazel-any`)
- `mono-checkleft-release`, `mono-release`, `mono-repobin-release`, `mono-changelog-release` (their `darwin` build phases via `bazel-any` + `os: darwin`)
- `appoint` (`bazel-build-test`, `checks` via `bazel-any`)
- `flunge-ci` (`iOS Bazel Build` via `macos-arm64`; backend/CLI/frontend Bazel steps and `Checks Framework` via `bazel-any`)

## The rule: one heavy Bazel job per Mac host at a time

Repo owner decision, 2026-10-15. **Heavy** means any CI step that invokes Bazel, concretely:

- a `bazel build` or `bazel test` of any size (`bazel-build-test`, `mac-app-build`, `integrity-bazel`, `boss-release`, every release pipeline's `darwin` phase, appoint's `bazel-build-test`, flunge's `iOS Bazel Build` and backend/CLI/frontend Bazel steps);
- a checkleft run (`checks`, `integrity-checkleft`, appoint's `checks`, flunge's `Checks Framework`): its `lint/rust` check builds the clippy aspect through Bazel, which on a Rust change recompiles the touched crate and everything above it, exactly like a build.

Steps that do not run Bazel at all — `commit-delta`, the `prepare`/`publish` phases of the release pipelines (which are pinned to Linux anyway), pipeline-upload steps — are light and are not throttled.

### Why

A commit that invalidates `engine_lib` makes every heavy job that lands on a Mac recompile it. One `engine_lib` rustc/clippy takes roughly 1.4–3.5 GB; the Bazel server JVM behind each job has no `-Xmx` pin and may grow to the JVM default of a quarter of RAM (4 GB on a 16 GB host). Four such jobs on one 16 GB host is 12–30 GB of demand. That is what happened on `skaffen` on 2026-10-15 22:30–22:46Z (load ~48, 5 of 6 GB swap, a 4.5 GB compressor, processes stuck on memory) and again on `anaplian` at 23:00Z the same evening, and on `skaffen` on 2026-10-14 and 2026-10-13 at the 22:30Z integrity tick.

With the rule in force, a host runs at most one Bazel server doing work plus that job's own action parallelism: roughly one JVM (1–4 GB) and one `engine_lib` compile (1.4–3.5 GB) with a handful of small sidecar actions. Jobs parked behind the lock have not started a Bazel server yet (the lock is taken before the first Bazel invocation), so each costs an idle agent and a git checkout, not memory. The residual on a host is Bazel servers left idle by _previous_ jobs; shortening their idle TTL is a separate change.

### Measured effect of contention (before)

Every `mono` and `mono-integrity` job on a Mac agent between 2026-10-13 and 2026-10-19 that took longer than four minutes was checked for an `engine_lib` rlib compile, and the compile time was correlated with how many other jobs (any pipeline) overlapped it on the same host by at least a minute:

| Jobs on the host during the compile | Compiles | `engine_lib` compile, min / median / max |
| ----------------------------------- | -------- | ---------------------------------------- |
| 2                                   | 6        | 48 s / 60 s / 85 s                       |
| 3                                   | 17       | 24 s / 71 s / 174 s                      |
| 4                                   | 48       | 57 s / 213 s / 1091 s                    |

No job in that window compiled `engine_lib` with the host to itself. The incident jobs sit at the top of the 4-way row: 573–634 s on `skaffen` (22:30Z), 919–1091 s on `anaplian` (23:00Z). Normal `integrity-checkleft` runtime is 1.5–5.5 min; its clippy build of `engine_lib` under 4-way contention ran 675 s and exceeded checkleft's 582 s wall-clock limit for the check.

Serialising heavy jobs puts every `engine_lib` compile back in the uncontended column (about a minute), and a four-job pile-up becomes a queue whose total wall time is the sum of the uncontended job times (about 2–4 min each) rather than four 12–16 minute failures.

To repeat the measurement after a change, list the jobs with `bk api "/pipelines/<pipeline>/builds?created_from=…&created_to=…&per_page=100"`, keep the ones whose `.jobs[].agent.name` starts with `skaffen` or `anaplian`, fetch each job's log with `bk api "/pipelines/<pipeline>/builds/<n>/jobs/<job-id>/log" | jq -r .content`, and take the largest `Compiling Rust rlib engine_lib (N files); <S>s` figure in it. The lock's own `[host-bazel-lock] acquired … after waiting <S>s` line gives the time a job spent parked.

## How the rule is enforced: the host lock

[`steps/host-bazel-lock.sh`](steps/host-bazel-lock.sh) is a small bash library sourced by [`steps/ci-env.sh`](steps/ci-env.sh), which every Bazel-running step in this repo sources before its first Bazel call. On Darwin it takes a host-wide lock and holds it until the step's process exits; on Linux it is a no-op. There is no per-step wiring to forget: if a step runs Bazel in CI it sources `ci-env.sh`, and if it sources `ci-env.sh` it takes the lock.

- **Lock path:** `${CI_HOST_BAZEL_LOCK_DIR:-${HOME}/.cache/buildkite-host-bazel-lock}/lock`. A directory, created with an atomic `mkdir`, containing an `owner` file (`pid`, `boot`, `acquired`, `job`, `agent`, `label`). The default sits next to `${HOME}/.cache/bazelcache`, the host-state location `ci-env.sh` already uses. **This path is the cross-pipeline contract**: every pipeline on a host must use the same directory, or the serialisation is only per-repo.
- **Waiting:** a job that finds the lock held polls every 5 s and prints a Buildkite group `--- [host-bazel-lock] waiting: another heavy Bazel job holds this host`, naming the holder (pipeline/build step, pid, job id, acquire time), with a progress line every 60 s. On acquiring it prints `[host-bazel-lock] acquired … after waiting <S>s`. A parked job is therefore visible in the Buildkite log at a glance, and its wait is not charged against checkleft's per-check wall clock (checkleft starts after the lock is held). No pipeline here sets a Buildkite job timeout, so the wait cannot time a job out either.
- **Release:** an `EXIT` trap removes the lock when the step's process ends, however it ends. A step that installs its own `EXIT` trap after sourcing `ci-env.sh` must chain `host_bazel_lock_release` into it (see `changelog-release.sh`); if it forgets, the next waiter reclaims the lock as soon as the holder's pid is gone.
- **Stale locks:** a lock is reclaimed when its holder pid no longer exists, when it was taken before the last reboot (the `boot` field is the kernel boot time), or when it is older than `CI_HOST_BAZEL_LOCK_MAX_AGE_SECS` (default 3 h — a backstop for a wedged holder, well above any legitimate job). Reclaiming renames the lock aside first, which is atomic, so two waiters cannot both reclaim one lock and nobody can delete a lock that was re-acquired between the check and the removal.
- **Knobs:** `CI_HOST_BAZEL_LOCK=1|0` forces the lock on or off (set it in a pipeline's env to disable it for a test build, or to enable it on a Linux host if evidence ever says Linux needs it too); `CI_HOST_BAZEL_LOCK_POLL_SECS`, `CI_HOST_BAZEL_LOCK_REPORT_SECS` tune the polling.
- **Tests:** `bazel test //.buildkite/steps/tests:host_bazel_lock_test` covers acquire/release, waiting behind a live holder, the three stale cases, the EXIT-trap release, and that a child process with an inherited lock neither blocks nor releases.

### Invariant: every agent on a host runs as one user

The lock lives under `$HOME`, so all four agent processes on a host must run as the same user with the same `HOME`. That is how the fleet is set up (one per-user launchd agent per queue, both under the operator's account). If a host is ever split across users, point every agent at one shared directory with `CI_HOST_BAZEL_LOCK_DIR` in the agent's environment hook, or the lock silently stops being host-wide.

## Verify the host layout

On the host, as the agent user:

```sh
# Which agent processes run, as whom, with which config:
ps -axo user,pid,command | grep '[b]uildkite-agent start'

# Each config's name pattern, spawn count and queue tag (token line omitted):
for f in /opt/homebrew/etc/buildkite-agent/*.cfg; do echo "== $f"; grep -Ev '^\s*(#|$)|token' "$f"; done

# The launchd agents that start them:
ls ~/Library/LaunchAgents | grep -i buildkite

# Memory and cores:
sysctl -n hw.memsize hw.ncpu
```

From anywhere with `bk` configured, the registered agents and their queues:

```sh
bk agent list --output json | jq -r '.[] | select(.hostname | test("skaffen|anaplian")) | [.name, .hostname, (.meta_data|join(",")), .connection_state] | @tsv'
```

The expected layout per host is two config files and two launchd plists — one pair for `queue=macos-arm64` with `name="%hostname-%spawn"`, one for `queue=bazel-any` with `name="%hostname-%spawn-any"` — each started through a `run-agent.sh` wrapper that `exec`s `buildkite-agent start --config <cfg>` and logs to `~/.buildkite-agent/agent.log`. The build path is per-agent under `~/.buildkite-agent/builds/<agent-name>/`.

## Host-side configuration

### Required for the rule to hold: nothing on the host for mono's pipelines

The lock ships entirely in this repo. Once this change is on `main`, every `mono*` pipeline job that lands on a Mac takes it. The default lock directory is created on first use. The only host precondition is the one-user invariant above, which the current layout already satisfies.

### Required for the rule to hold fleet-wide: adopt the lock in the sibling repos

`appoint` and `flunge-ci` run their own step scripts from their own repos, so until they take the same lock a Mac host can still run one `mono` heavy job plus one job from each of those pipelines concurrently. Adoption is mechanical:

1. Copy `.buildkite/steps/host-bazel-lock.sh` verbatim into the repo (`.buildkite/steps/` in appoint, `.buildkite/scripts/` in flunge). Do not change the default lock directory.
2. At the top of every Bazel-running step script, after `set -euo pipefail` and before the first `bazel` call, add:

   ```sh
   source "$(dirname "${BASH_SOURCE[0]}")/host-bazel-lock.sh"
   host_bazel_lock_acquire
   ```

   appoint: `bazel-build-test.sh`, `checks.sh`. flunge: `run_ios_bazel_build.sh`, `run_backend_bazel_tests.sh`, `run_cli_bazel_tests.sh`, `run_frontend_bazel_tests.sh`, and `run_checks.sh` if it runs Bazel (it does when checkleft's `lint/rust` is enabled there); a shared `lib.sh` is the natural place in flunge.

3. Watch one build on each pipeline for the `[host-bazel-lock] acquired` line in a Mac job's log.

### Recommended: one spawn per queue per Mac host

With four agents per host and the lock in place, up to three jobs can sit parked on one host while another host is idle, because Buildkite assigns a job to any free agent and only the lock knows the host is busy. Halving the agent count per host (one `macos-arm64` agent and one `bazel-any` agent) keeps the Buildkite scheduler closer to the truth: a host advertises at most two free slots, so at most one job parks behind the running one and the rest queue centrally where any host can pick them up. Mac hosts get almost no light jobs, so this costs no real concurrency.

On each Mac host, as the agent user:

1. In each `/opt/homebrew/etc/buildkite-agent/*.cfg` that has `spawn=2`, set `spawn=1` (or delete the line; 1 is the default).
2. Restart each launchd agent so the config is re-read, waiting until the host is idle in Buildkite (`bk agent list` shows no `job` on its agents), since `bootout` stops running jobs:

   ```sh
   launchctl bootout gui/$(id -u)/<plist label>
   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/<plist label>.plist
   ```

3. Confirm with `bk agent list` that the host now registers `<host>.local-1` and `<host>.local-1-any` only, both `connected`.

This is a scheduling improvement, not part of the correctness of the rule; skipping it leaves memory bounded exactly as described above.

### Not recommended: taking the Macs out of `bazel-any`

Removing the `-any` agents would make the rule trivially true for `bazel-any` work but moves all of `bazel-build-test`, `checks`, `integrity-checkleft`, the release `prepare`/`publish` phases and the sibling pipelines' Bazel steps onto the four Linux agents. That is a fleet-capacity decision, not a fix for the memory problem, and it does nothing about two `macos-arm64` jobs colliding.

## Alternatives considered

- **Buildkite `concurrency_group`.** Groups are org-wide counts, not per-host, and a step's group is fixed at upload time before the scheduler picks a host; a group of size N bounds the fleet, not any one machine.
- **Per-invocation locking (a `bazel` wrapper that locks around each command).** checkleft and repobin invoke the `bazel` binary directly, not `ci-env.sh`'s shell function, so a wrapper would have to be injected onto `PATH` for every tool and still could not stop a job's idle server from holding heap between its commands. Job-scoped locking at the one place every step already passes through is simpler and covers clippy.
- **Bazel `--local_ram_resources` / `--jobs`.** Per-server limits; four servers on one host each believe they own the machine. They bound a job's own parallelism, not the host.
- **Raising checkleft's or Bazel's time limits, retrying timed-out jobs, skipping clippy or tests on Mac.** Explicitly ruled out: each hides the thrash instead of removing it.

## Troubleshooting

- **A Mac job shows `--- [host-bazel-lock] waiting` for a long time.** Expected while another heavy job runs on that host; the log names the holder. Look up the holder's build to see what it is doing. If the holder is a wedged job, cancel it in Buildkite; the waiter reclaims the lock as soon as the holder's process is gone.
- **The holder named in the log finished long ago.** The lock was left behind by a process that died without its EXIT trap (a hard kill, a step that replaced the trap without chaining the release). Waiters reclaim it automatically when the pid is gone, after a reboot, or after 3 h. To clear it by hand: `rm -rf ~/.cache/buildkite-host-bazel-lock/lock` on the host as the agent user — only after checking `ps -p <pid>` shows the owner is really gone.
- **Two heavy jobs are running on one Mac at the same time.** Check the second job's pipeline: if it is `appoint` or `flunge-ci`, that repo has not adopted the lock yet (see above). If both are `mono*` jobs, confirm the lock directory is the same for both agent processes (`echo $HOME` in each agent's environment; `ls ~/.cache/buildkite-host-bazel-lock/`).
- **Running a Bazel step locally outside CI.** The lock is taken on any Darwin machine that sources `ci-env.sh`, so two local runs of a step script on one laptop serialise too. Set `CI_HOST_BAZEL_LOCK=0` to opt out.
