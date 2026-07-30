# Linux CI agent host runbook

This runbook documents the configuration of the Linux hosts backing the `bazel-any` Buildkite queue, and the maintenance procedures for touching them safely. It exists because a 2026-07-27 incident (below) cost hours to diagnose purely because none of this was written down anywhere durable — it lived only in a person's head and in an incident thread.

Host facts here were verified against the live hosts (`ssh empiricist` / `ssh zoologist` / `ssh diziet` as the `bduff` user) and the live Buildkite agent registrations (`bk agent list`) on 2026-07-27, except where explicitly noted as unverified. There is **no passwordless sudo** on any of these hosts for the `bduff` account, so anything that requires root is called out explicitly in "Needs operator input" at the end rather than guessed.

## Host inventory

The Linux side of the `bazel-any` queue is three hosts running four agent registrations. Other Linux agents exist on other queues (`sma-ci-1`/`sma-ci-2` on `bazel-any-test`, `sma-release-1` on `linux-release`); they are out of scope here and their userns config has not been checked — they may have the same AppArmor exposure described below.

| Host         | Agent(s)                       | OS                            |
| ------------ | ------------------------------ | ----------------------------- |
| `empiricist` | `empiricist-1`, `empiricist-2` | Ubuntu 26.04 LTS (`resolute`) |
| `zoologist`  | `zoologist-1`                  | Ubuntu 26.04 LTS (`resolute`) |
| `diziet`     | `diziet-1`                     | Ubuntu 26.04 LTS (`resolute`) |

`empiricist` runs two agent registrations from a single `buildkite-agent start` process (one `/usr/bin/buildkite-agent start` process, two build directories — `/var/lib/buildkite-agent/builds/empiricist-1/` and `.../empiricist-2/`); `zoologist` and `diziet` each run one. All four register on `queue=bazel-any` and are visible via `bk agent list --output json`.

Per `.buildkite/README.md`, `bazel-any` is a **heterogeneous fleet**: these four Linux agents share the queue with macOS agents (`anaplian.localdomain-1-any`/`-2-any`, `skaffen.localdomain-1-any`/`-2-any`). A `bazel-build-test` or `checks` job lands on whichever agent is free — worker code cannot pin a job to a specific host.

**Consequence: host configuration must be identical across all Linux agents, or failures appear intermittent and read as flakiness.** A job that fails on `empiricist` and passes on `zoologist` looks like a flaky test until someone checks whether the two hosts are actually configured the same way. The [known inconsistency](#known-inconsistency-buildkite-agent-tag-typo) below is exactly this trap.

## The unprivileged-user-namespace requirement

### Background

Bazel's `linux-sandbox` execution strategy needs to create an unprivileged user namespace plus a mount namespace for each sandboxed action. If the kernel refuses that syscall, Bazel's sandbox-support probe (run once, at server startup) fails silently and the `linux-sandbox` strategy is never registered — there is no warning at that point, only a downstream failure once a `TestRunner` (or similar) action actually needs the strategy.

### The 2026-07-27 incident (worked example)

Every `bazel test` on the Linux agents failed with:

```
ERROR: 'linux-sandbox' was requested for mnemonic TestRunner but no strategy with that
identifier was registered. Valid values are: [dynamic_worker, processwrapper-sandbox,
standalone, dynamic, remote, worker, sandboxed, local]
INFO: 0 processes.
ERROR: Build did NOT complete successfully
```

On-host inspection of `empiricist` found:

```
$ unshare -Urm true
unshare: write failed /proc/self/uid_map: Operation not permitted
exit=1

$ sysctl kernel.apparmor_restrict_unprivileged_userns
kernel.apparmor_restrict_unprivileged_userns = 1
```

Root cause: Ubuntu 23.10+ ships an AppArmor restriction (`kernel.apparmor_restrict_unprivileged_userns`) that blocks the `uid_map` write an unprivileged user namespace create requires. That blocks Bazel's sandbox-support probe, so `linux-sandbox` never registers, and Bazel fails closed.

### Diagnosing it

Run the probe **as the `buildkite-agent` user**, not as yourself and not under `sudo`:

```sh
sudo -u buildkite-agent unshare -Urm true && echo OK || echo FAIL
```

**The trap, stated prominently: running this probe under `sudo` (or as any account with `CAP_SYS_ADMIN`/root) succeeds regardless of the AppArmor restriction, because root can always create namespaces.** A `sudo unshare -Urm true` that reports `OK` proves nothing about whether the actual unprivileged `buildkite-agent` user can create a namespace, and following that false signal is what misled the original diagnosis of this incident. Always drop to the unprivileged agent user first (`sudo -u buildkite-agent ...`, not `sudo su -` then run it as root).

Then check the three relevant sysctls:

```sh
sysctl kernel.apparmor_restrict_unprivileged_userns \
       user.max_user_namespaces \
       kernel.unprivileged_userns_clone
```

- `kernel.apparmor_restrict_unprivileged_userns` — must be `0` for unprivileged user namespaces to work under AppArmor's restriction (Ubuntu 23.10+ default is `1`, which blocks it).
- `user.max_user_namespaces` — must be non-zero (a `0` here disables user namespaces globally regardless of the AppArmor sysctl).
- `kernel.unprivileged_userns_clone` — legacy/Debian-lineage gate for the same feature; must be `1` if present.

As of 2026-07-27 (post-fix), all three Linux hosts report:

```
kernel.apparmor_restrict_unprivileged_userns = 0
user.max_user_namespaces = <large, host-specific>
kernel.unprivileged_userns_clone = 1
```

### The remedy

```sh
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

...and persist it across reboots via a drop-in, since a bare `sysctl -w` reverts on reboot:

```sh
# /etc/sysctl.d/60-bazel-linux-sandbox.conf
kernel.apparmor_restrict_unprivileged_userns = 0
```

This file exists on all three hosts today (verified 2026-07-27), containing exactly that one line.

### The security tradeoff (state it honestly)

Setting `kernel.apparmor_restrict_unprivileged_userns=0` re-enables unprivileged user namespace creation host-wide, for every user on the box, not just `buildkite-agent`. Unprivileged user namespaces are a real local-privilege-escalation attack surface — several kernel CVEs over the years have specifically required an unprivileged user namespace as a prerequisite. This is a **modest, deliberately accepted** increase in attack surface, justified because:

- These are dedicated, single-purpose CI builders, not general-purpose shared hosts.
- The Bazel `linux-sandbox` strategy genuinely requires this to function — there is no way to keep the restriction on and still get sandboxed local execution.

If a host's threat model changes (e.g. it stops being a dedicated CI builder), this tradeoff needs to be re-evaluated.

## Bazel server restart after this — and the two output roots

Bazel probes sandbox-strategy support **once, at server startup**, and caches the result for the life of that server process. Flipping the sysctl does **not** fix an already-running Bazel daemon — it will keep reporting `linux-sandbox` unregistered until it is restarted. This is the step most likely to be missed: the sysctl fix looks complete (the probe now succeeds standalone) while the daemon that CI actually talks to is still the stale one.

The Bazel servers on these hosts run as the `buildkite-agent` user, and there are **two separate output roots** in play, so a restart needs to account for both:

1. **`/mnt/ssd/bazel/output_base`** — used by CI `mono` builds. `.ci.linux.startup.bazelrc` sets `startup --output_user_root=/mnt/ssd/bazel/output_base`, and `.buildkite/steps/ci-env.sh` passes `--bazelrc=.ci.linux.startup.bazelrc` as part of `CI_BAZEL_STARTUP_FLAGS` for every wrapped CI bazel invocation. Confirmed live: running jobs on `empiricist` had server processes with `--output_user_root=/mnt/ssd/bazel/output_base` and workspace directories under `/var/lib/buildkite-agent/builds/empiricist-{1,2}/flunge/mono`.
2. **`/var/lib/buildkite-agent/.cache/bazel`** — Bazel's ordinary default (`$HOME/.cache/bazel`, and the `buildkite-agent` systemd unit sets `HOME=/var/lib/buildkite-agent`), used by any bazel invocation that does _not_ go through the CI wrapper's startup flags (a manual `bazel` call on the host, or any code path `.buildkite/steps/ci-env.sh`'s comment warns about — one that doesn't read from `CI_BAZEL_STARTUP_FLAGS`). This directory exists and is populated (multiple `_bazel_buildkite-agent/<hash>` server dirs, tens of GB) on all three hosts, confirming it is genuinely in use, not just a theoretical fallback.

Restart both:

**Blunt (kills every Bazel server the `buildkite-agent` user owns, both roots at once):**

```sh
sudo pkill -u buildkite-agent -f 'A-server.jar'
```

**Graceful, per-workspace** (run from inside each workspace directory as `buildkite-agent`, once per output root/workspace you need to shut down — this only stops the server matching that workspace's specific flags, so it's the right choice if you need to avoid disturbing an in-flight build in the _other_ root):

```sh
sudo -u buildkite-agent bazel --output_user_root=/mnt/ssd/bazel/output_base shutdown
sudo -u buildkite-agent bazel shutdown   # default output root, i.e. ~/.cache/bazel
```

A killed/shut-down server restarts automatically and lazily on the next `bazel` invocation — there is no separate "start" step.

## Safe maintenance procedure

These are live CI hosts feeding a shared queue — a job can land mid-restart if you don't pause first. Before touching a host's Bazel state, kernel sysctls, or anything else that could disrupt an in-flight or about-to-land job:

```sh
# Pause every agent on the host you're about to touch — run it once per registration
# (empiricist has two: empiricist-1 and empiricist-2). <agent-id> is the UUID from
# `bk agent list --output json` (`.[] | {name, id}`), not the agent name.
bk agent pause <agent-id> --timeout-in-minutes <N>
```

Pick `<N>` generously enough to cover the maintenance window — **pauses auto-expire on the timeout**, so an under-estimated window can let a job land while you're still mid-restart. When done:

```sh
bk agent resume <agent-id>
```

`bk agent list --output json` shows each agent's current `paused` / `paused_at` / `paused_by` / `paused_note` / `paused_timeout_in_minutes`, so you can confirm the pause took effect and see who paused it and why, before proceeding.

## Known inconsistency: buildkite-agent tag typo

`empiricist-1` and `empiricist-2` register with the `meta_data` tag `arg=amd64`, while `zoologist-1` and `diziet-1` register with `arch=amd64` (verified live via `bk agent list --output json`, 2026-07-27):

```
empiricist-1: queue=bazel-any, os=linux, arg=amd64,  host=empiricist
empiricist-2: queue=bazel-any, os=linux, arg=amd64,  host=empiricist
zoologist-1:  queue=bazel-any, os=linux, arch=amd64, host=zoologist
diziet-1:     queue=bazel-any, os=linux, arch=amd64, host=diziet
```

This is a real, live inconsistency in the agent bootstrap configuration on `empiricist`, not a typo in this document — it is recorded here deliberately rather than silently normalized, per the host-config-must-be-identical consequence above. It does not currently break scheduling (nothing in `.buildkite/pipeline.yml` selects on `arch=`), but it means `empiricist`'s two agents cannot be targeted by an `arch=amd64` tag selector the way `zoologist`/`diziet` can, and it is exactly the kind of drift that turns into a confusing failure later. Fixing the bootstrap config that sets this tag is host configuration change, out of scope for this doc-only runbook; whoever owns that config should correct `empiricist`'s tag to `arch=amd64` to match the other two hosts.

## Disk layout

All three hosts have the same two-disk shape:

- `/` — the boot/OS disk (`nvme0n1p2`, ~900 GB–950 GB depending on host), ext4.
- `/mnt/ssd` — a much larger secondary disk (`sda1`, 3.6 TB across all three hosts), ext4, mounted specifically to host Bazel's caches.

Bazel's CI-mode output root, disk cache, and repository cache are all pointed at `/mnt/ssd` rather than the boot disk:

- `startup --output_user_root=/mnt/ssd/bazel/output_base` (`.ci.linux.startup.bazelrc`)
- `build:ci-linux --disk_cache=/mnt/ssd/bazel/disk_cache` (`.ci.bazelrc`)
- `build:ci-linux --repository_cache=/mnt/ssd/bazel/repo_cache` (`.ci.bazelrc`)

This keeps Bazel's large, disk-hungry action cache and sandboxed build outputs off the smaller boot disk and on dedicated capacity sized for it (disk cache GC is configured for up to 3 TB / 60 days via `experimental_disk_cache_gc_max_size`/`_max_age` in `.ci.bazelrc`, which would not fit on the boot disk alongside the OS and everything else on it).

## Deploy-key posture

Per `.buildkite/README.md` ("Pushing from CI"), pushes from Linux Buildkite agents to `spinyfin/mono` succeed reliably: `spinyfin/mono` has zero deploy keys registered, and sampling `checkleft-release.sh`'s `prepare` phase across builds 1250–1368 in `mono-checkleft-release`'s history shows it landing on a Linux agent (`zoologist-1`, `diziet-1`, `empiricist-1`/`empiricist-2`) in every sampled build, always succeeding — no push failure attributable to a read-only deploy key has been observed. What is unconfirmed is the exact credential mechanism Linux agents push with (root access would be needed to inspect `buildkite-agent`'s `~/.ssh` from the host side — see "Needs operator input"), not whether pushes work. See the README's "Pushing from CI" section for the full detail.

## Bazel startup rc files

- `.ci.linux.startup.bazelrc` — Linux-only, sets the `/mnt/ssd` output root (see "Disk layout" above). Read by `.buildkite/steps/ci-env.sh` when `OS_TYPE=linux`.
- `.ci.darwin.startup.bazelrc` — the macOS equivalent; sets the darwin output root to `/private/var/tmp/bazel_darwin_ci_output_base` (it must stay on the internal case-insensitive volume — see the comment in that file, which documents that pointing it at `/Volumes/ssd` is exactly what broke `mac-app-build` under Xcode 26.5 and was deliberately reverted). The macOS Xcode pin is in `.ci.bazelrc` (`build:ci-darwin`), not here — out of this doc's scope.
- Both are startup-option `.bazelrc`s specifically because `--output_user_root` (and similar) must be a **startup** flag, not a build flag — see the comment in `.buildkite/steps/ci-env.sh` explaining why `CI_BAZEL_STARTUP_FLAGS` is the single source of truth every CI code path must read from, to avoid two daemons running against the same output base at once.

## Other configuration observed

- **systemd unit**: `buildkite-agent.service` (plain, not the `buildkite-agent@.service` template) runs as `User=buildkite-agent`, `Environment=HOME=/var/lib/buildkite-agent`, `ExecStart=/usr/bin/buildkite-agent start` on all three hosts. `empiricist`'s second agent registration (`empiricist-2`) is **not** a second systemd unit instance — both `empiricist-1` and `empiricist-2` come from the single running `buildkite-agent start` process (confirmed via `ps aux`: one process, two Bazel server children with distinct `--workspace_directory=.../empiricist-{1,2}/flunge/mono`). The exact mechanism (multiple `spawn`s configured in `buildkite-agent.cfg`) could not be confirmed without root — see "Needs operator input".
- **Hooks**: `/etc/buildkite-agent/hooks/` on all three hosts contains only the stock `*.sample` files — no active custom hooks (`checkout`, `command`, `environment`, etc.) are configured on any of the three Linux agents.
- **Stale build directories**: `zoologist` has a leftover `/var/lib/buildkite-agent/builds/zoologist-2/` (a `mono` checkout, last touched 2026-06-05) and `diziet` has a leftover `/var/lib/buildkite-agent/builds/diziet-2/` (a `flunge-ci` checkout, last touched 2026-06-14). Neither corresponds to a currently-registered agent in `bk agent list` — these are checkouts left behind by a previously-registered second agent slot on each host that no longer exists. They are not part of the current live agent inventory; do not treat their presence as evidence of a second active agent on those hosts.
- **Bazel version**: all three hosts run bazelisk via `/usr/bin/bazel`. Outside a workspace this resolves to `9.2.0` (bazelisk's default, verified on `empiricist`; `zoologist`/`diziet` re-downloaded the release on invocation, `empiricist` had it cached) — but that fallback is not what CI runs. Inside the actual agent checkout (e.g. `/var/lib/buildkite-agent/builds/empiricist-1/flunge/mono`), `bazel --version` reports `9.1.0`, because the repo pins `.bazelversion` to that value as of 2026-07-27. Debugging a version-specific sandbox/strategy issue should reason about `9.1.0`, not `9.2.0`.
- **OS**: Ubuntu 26.04 LTS ("resolute") on all three hosts, kernel `7.0.0-22-generic` (`empiricist`/`zoologist`) or `7.0.0-28-generic` (`diziet`).

## Needs operator input

The following could not be verified without root, and are **not** guessed at above — if you need them, run these as an operator with sudo on the relevant host:

- **Full `buildkite-agent.cfg` contents** (tags beyond what's visible via `bk agent list`, the `spawn` setting believed to explain `empiricist`'s two agents from one process, hooks-path overrides, etc.): `sudo cat /etc/buildkite-agent/buildkite-agent.cfg`
- **The `buildkite-agent` user's SSH/deploy-key material** (to directly confirm which credential mechanism Linux agents push with, as described in "Deploy-key posture", rather than relying solely on `.buildkite/README.md`'s observed-behavior evidence): `sudo ls -la /var/lib/buildkite-agent/.ssh/` and `sudo cat /var/lib/buildkite-agent/.ssh/config` (if present).
- **Confirming which config mechanism produces two agent registrations from one `empiricist` process** (spawn count in the cfg file vs. some other mechanism): `sudo grep -i spawn /etc/buildkite-agent/buildkite-agent.cfg`
