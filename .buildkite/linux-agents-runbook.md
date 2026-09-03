# Linux CI agent host runbook

For standing up a **new** Linux agent from scratch, including on GCE, see [`gce-agent-runbook.md`](gce-agent-runbook.md).

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

## Known inconsistency: BIOS version drift (diziet vs. zoologist)

`diziet` and `zoologist` are the same board (Intel NUC11PABi5) but are not on the same firmware: `diziet` is on BIOS `PATGL357.0051.2023.0420.1005` (dated 2023-04-20), while `zoologist` is on `PATGL357.0058.2025.1223.1053` (dated 2025-12-23) — nearly three years newer. See the [hardware inventory](#hardware-inventory) in "Power loss and unattended recovery" below for the full per-host BIOS table, including `empiricist`'s unrelated Beelink/AMI firmware.

This is the same trap as the `arch=` tag typo above, on firmware instead of agent metadata: a BIOS-version-dependent difference in behavior (power/ACPI settings, microcode, any firmware-level workaround) between two boards that are supposed to be identical would show up as a `diziet`-vs-`zoologist` flake, not as an obvious configuration bug. Updating `diziet`'s firmware to match `zoologist` is a host-configuration change requiring physical access to the BIOS setup menu (see below) and is out of scope for this doc-only runbook.

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
- **Open question — `diziet`'s unexplained 2026-08-21 outage**: `diziet`'s previous boot ended uncleanly at 2026-08-21 12:33:28, two days before the 2026-08-23 power-loss incident described below, and it was the only one of the five `bazel-any` hosts (three Linux, two macOS) to go down that day — neither macOS agent rebooted on 2026-08-21, so this was not a house-wide power event. This has not been diagnosed. Candidate causes, none confirmed: a separate outlet or power strip for `diziet` that lost power independently, a hard hang unrelated to power, or a PSU/barrel-jack fault. Do not attribute it to any of these without further evidence.

## Power loss and unattended recovery

This section exists for the same reason the rest of this runbook does (see the incident preamble at the top): the fleet's firmware AC-restore behavior is a real, currently-undocumented source of divergence between hosts, and it directly caused a multi-day outage that could only be resolved by physically walking up to three machines. Host facts below were gathered by read-only inspection over SSH on 2026-09-02/2026-09-03, the same way as the rest of this document; the ones that genuinely require physical BIOS access are called out as such rather than guessed.

### Incident: the 2026-08-23 outage and its 10-day tail

AC power to the office returned at approximately **Sun 2026-08-23 04:35 PDT**. The two macOS `bazel-any` agents (`anaplian`, `skaffen`) powered on unattended within about a minute of that. The three Linux `bazel-any` hosts did not: they stayed off until someone physically pressed their power buttons on **Wed 2026-09-02, 19:09–19:11 PDT** — a 3-second gap between the first two presses and a 36-second gap before the third, consistent with sequential hand presses rather than a second AC-restore event.

`journalctl --list-boots` on each host (note: `last` is **not installed** on any of the three, which is why `--list-boots` is the tool — see the recovery checklist below):

| Host         | Previous boot ended | Current boot began  | Off for   |
| ------------ | ------------------- | ------------------- | --------- |
| `diziet`     | 2026-08-21 12:33:28 | 2026-09-02 19:09:52 | 12.3 days |
| `empiricist` | 2026-08-23 01:15:22 | 2026-09-02 19:10:31 | 10.7 days |
| `zoologist`  | 2026-08-23 01:11:31 | 2026-09-02 19:09:55 | 10.7 days |

All three previous boots ended uncleanly — the journal simply stops mid-activity, with no shutdown sequence recorded — and no boot occurred on any of the three hosts between the outage and the 2026-09-02 physical recovery.

### Hardware inventory

Not previously recorded anywhere in this runbook — the "OS layout" and "other configuration" sections above describe disks, OS and kernel, but never what the machines physically are. This matters operationally because it determines the BIOS menu path below, and it rules out any remote fix: whatever the box is, there is no way to power it on except by being in front of it (see "No BMC" below).

| Host         | Model                     | Board      | BIOS version                                     | BIOS date  |
| ------------ | ------------------------- | ---------- | ------------------------------------------------ | ---------- |
| `diziet`     | Intel NUC11PAHi5          | NUC11PABi5 | `PATGL357.0051.2023.0420.1005`                   | 2023-04-20 |
| `zoologist`  | Intel NUC11PAHi5          | NUC11PABi5 | `PATGL357.0058.2025.1223.1053`                   | 2025-12-23 |
| `empiricist` | Beelink SER mini-PC (AMD) | —          | AMI BIOS `HPT.2xx.SERMP.V035.P8C0M0C15.07.BLink` | 2025-12-16 |

See "Known inconsistency: BIOS version drift" above for why `diziet` and `zoologist` sharing a board but not a firmware version matters.

### The firmware AC-restore policy is a fleet invariant — and it is not readable or settable from the OS

Every modern x86 BIOS/UEFI has a setting that decides what the machine does when AC power returns after a loss: stay off, power on, or resume whatever state it was last in. **This setting could not be read from any of the three Linux hosts over SSH — there is no supported OS-level read path for it (see "No BMC" below) — so its current value is not stated here as a fact.** What can be stated as fact is the observed behavior: AC returned at a known time, all three Linux hosts were healthy immediately beforehand, and none of them powered on. That is consistent with the AC-restore policy being set to something other than "power on" (or with a firmware bug that produces the same symptom), but it is an inference from behavior, not a read value.

Because this is a per-host firmware setting with no OS-level visibility and no remote path, it must be checked and corrected **physically, per host**, and it is exactly the kind of setting the "host configuration must be identical" consequence at the top of this doc is warning about — a fleet where three otherwise-identical Linux builders can silently diverge on outage behavior, discoverable only by an actual outage.

Menu paths (differ by vendor — do not assume the NUC path applies to the Beelink):

- **NUC (`diziet`, `zoologist`)**: press `F2` at power-on to enter setup → **Power → Secondary Power Settings → "After Power Failure"** → set to **Power On** → `F10` to save and exit.
- **Beelink SER (`empiricist`)**: press `Del` or `F7` at power-on → the option is named **"Restore AC Power Loss"** or **"State After G3"**, and depending on SER generation lives under either _Chipset → PCH/SoC Power Management_ or _Advanced → ACPI Settings_ → set to **Power On** / **S0 State**. This menu path is genuinely different from the NUC's and must not be assumed identical when someone is next in front of these machines.

**Why "Power On" and not "Last State":** "Last State" resumes whatever power state the machine was in immediately before the outage. That is wrong for an unattended CI builder specifically because it means any _deliberate_ shutdown (a manual power-off for maintenance, a firmware update, moving the box) leaves the machine off after the next outage too — the two failure modes compound instead of the AC-restore policy being a clean, single fallback. "Power On" always brings the machine back regardless of why it was off.

### No BMC exists on any host

None of these three hosts has a baseboard management controller, and none of the product lines involved (Intel NUC11, Beelink SER) ships one. This is recorded here so it is not re-derived during the next incident:

- `ipmitool` is not installed on any of the three hosts.
- `/dev/ipmi*` does not exist on any of the three.
- `/sys/class/ipmi` does not exist on any of the three.

There is consequently no remote console and no remote power-on path for any of these machines — physical presence is the only way to power one on or to read/change the AC-restore setting.

Two things that look like they might substitute for a BMC read, and don't:

- **`dmidecode` cannot answer this, even with root.** SMBIOS type 3 (System Enclosure) exposes `Boot-up State` and `Power Supply State` fields, but neither of those is the AC-restore policy — they describe enclosure/PSU status fields, not what the firmware does after an AC loss.
- **The UEFI setup variables under `/sys/firmware/efi/efivars`** (`Setup-80e1202e-…`, `PchSetup-…`, `SaSetup-…`, `AmiWrapperSetup-…`, `AMD_PBS_SETUP-…`) are vendor-private opaque binary blobs. There is no supported, documented way to decode the AC-restore bit out of them.

### Why the macOS agents recovered and the Linux agents did not

`.buildkite/README.md` already documents `bazel-any` as one heterogeneous queue spanning Mac and Linux agents (see the top of this doc); this is the specific mechanism behind the outage-recovery gap between the two halves of that queue.

**macOS (`anaplian`, `skaffen` — both `Mac16,10` Mac mini M4):**

- `pmset autorestart = 1`, `pmset sleep = 0` — the firmware/OS auto-restarts on AC restore and never sleeps.
- FileVault is **off** — no disk-unlock prompt blocks boot.
- Auto-login is enabled (`autoLoginUser = brianduff`, `/etc/kcpassword` present) — the console reaches a logged-in GUI session with no human input.
- The Buildkite agents are **user LaunchAgents** — `~/Library/LaunchAgents/homebrew.mxcl.buildkite-agent@3.plist` and `…@3-bazel-any.plist`, both `RunAtLoad=true` with `KeepAlive{SuccessfulExit=false}` — loaded in the GUI launchd domain of that logged-in user session. There are **no** buildkite LaunchDaemons on these hosts.

The macOS recovery chain is therefore login-dependent end to end: `autorestart` → boot → no FileVault prompt → auto-login → GUI-domain LaunchAgents load once a user session exists.

**Linux (`diziet`, `zoologist`, `empiricist`):** firmware AC-restore policy → boot → `buildkite-agent.service`, a **system** unit that needs no login session at all (see "Verified-good Linux boot chain" below).

**This is a deliberate difference, not a gap to close.** The Linux side does not require auto-login today and should not grow one — a system unit with no session dependency is a strictly simpler and more robust recovery path than the macOS LaunchAgent-in-a-GUI-session chain, and the fix for the Linux hosts not recovering is the firmware AC-restore setting above, not mimicking the macOS auto-login mechanism.

### Verified-good Linux boot chain

Confirmed identically on all three hosts as of the 2026-09-02 recovery, so it does not need to be re-derived during the next incident:

- `buildkite-agent.service` is `enabled` (`WantedBy=multi-user.target`) and `active (running)`. It is the plain unit described earlier in this doc, not the `@.service` template, and runs as `User=buildkite-agent`.
- No login session is required to start it: there are no user-scoped systemd units for the `buildkite-agent` account (`/var/lib/buildkite-agent/.config/systemd/user` does not exist), and uid 997 (`buildkite-agent`) is correctly **not** set to linger.
- It starts fast and unattended: on `diziet`, boot began at 19:09:52 and `Started buildkite-agent.service` was logged at 19:10:00 — 8 seconds later, with nobody logged in.
- `/mnt/ssd` (the Bazel-cache disk described in "Disk layout" above) carries `nofail` in `/etc/fstab` on all three hosts (`… /mnt/ssd ext4 defaults,nofail 0 2`). This is the specific thing that prevents a missing or slow-to-appear cache disk from dropping the boot into an emergency shell that would otherwise wait forever for interactive input that never comes on an unattended builder.
- No failed units at boot, no full-disk encryption, no boot passphrase, and Ubuntu is first in `BootOrder` on all three. Secure Boot is enabled on `diziet` and `zoologist`, disabled on `empiricist`.
- `systemd-analyze` for that boot: `diziet` — `25.835s (firmware) + 2.263s (loader) + 1.208s (kernel) + 1.580s (initrd) + 16.937s (userspace) = 47.825s`; `zoologist` — `41.561s` total.

### Recovery checklist

How to tell "the host did not power back on" apart from "it powered on but the agent failed to start," without physical access to the host:

1. `tailscale status` — a host that's up and networked shows as online; a host that's still powered off does not.
2. If it's not online, `ssh` in (if reachable another way) or wait for the next physical check, then run `journalctl --list-boots` and compare the most recent boot's start time against when you expect it to have come back (e.g. against a known AC-restore time). **`last` is not installed on any of the three hosts — do not reach for it.** `journalctl --list-boots` is the tool that actually works here.
3. If the host is up but the agent isn't registered in `bk agent list`, check `systemctl status buildkite-agent.service` and the unit's journal (`journalctl -u buildkite-agent.service`) for why it failed to start, rather than assuming a power problem.
4. Anything requiring `sudo` needs a human physically or interactively present: all three hosts prompt for a password for the `bduff` account, and there is **no passwordless sudo** on any of them (consistent with the note at the top of this doc).

## Needs operator input

The following could not be verified without root, and are **not** guessed at above — if you need them, run these as an operator with sudo on the relevant host:

- **Full `buildkite-agent.cfg` contents** (tags beyond what's visible via `bk agent list`, the `spawn` setting believed to explain `empiricist`'s two agents from one process, hooks-path overrides, etc.): `sudo cat /etc/buildkite-agent/buildkite-agent.cfg`
- **The `buildkite-agent` user's SSH/deploy-key material** (to directly confirm which credential mechanism Linux agents push with, as described in "Deploy-key posture", rather than relying solely on `.buildkite/README.md`'s observed-behavior evidence): `sudo ls -la /var/lib/buildkite-agent/.ssh/` and `sudo cat /var/lib/buildkite-agent/.ssh/config` (if present).
- **Confirming which config mechanism produces two agent registrations from one `empiricist` process** (spawn count in the cfg file vs. some other mechanism): `sudo grep -i spawn /etc/buildkite-agent/buildkite-agent.cfg`
