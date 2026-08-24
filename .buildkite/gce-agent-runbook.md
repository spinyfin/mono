# Standing up a Linux CI agent on GCE

Written 2026-08-24, after a power cut killed every Linux CI host (`empiricist`, `zoologist`, `diziet`) and none returned. A replacement was provisioned on a Google Cloud VM and taken to green end-to-end. This is what that took.

Companion to `linux-agents-runbook.md`, which covers diagnosing an _existing_ host. This one covers building a new one.

## Minimum viable path

Order matters — step 3 creates the user that steps 4 onward depend on.

1. **x86_64 only.** `n2-standard-16`, 500 GB pd-balanced. Do not pick an Axion/`t2a` type.
2. Install packages (see "Packages" below).
3. **Install the `buildkite-agent` package.** This creates the `buildkite-agent` user. Configure it later.
4. **Disable the AppArmor userns restriction** (Ubuntu). Without this every `bazel test` fails.
5. `mkdir -p /mnt/ssd/bazel`, chown to `buildkite-agent`.
6. Install bazelisk as `/usr/bin/bazel`.
7. Set the agent token and `tags="queue=bazel-any,os=linux,arch=amd64"`.
8. SSH credential at `/var/lib/buildkite-agent/.ssh/`, mode 0600, plus `known_hosts`.
9. `systemctl enable --now buildkite-agent`.
10. Verify with `bk agent list`, then smoke-test on `bazel-any-test` before joining the real queue.

## Architecture: x86_64, not negotiable

`steps/checkleft-release.sh:531` hardcodes the asset name `checkleft-x86_64-unknown-linux-gnu` and `:570` the musl equivalent. Line 565 then _executes_ the built musl binary on the agent to verify its version, dying with "check agent architecture" on a mismatch. `steps/changelog-release.sh:522` does the same.

`MODULE.bazel` does register `wasm_tools_aarch64_linux`, so arm64 is not technically impossible for `bazel-build-test` — it would still break the release pipelines.

## Sizing

The dead hosts' CPU and RAM were never recorded. `n2-standard-16` (16 vCPU / 64 GB) is inferred from observed action parallelism (58 concurrent test actions at peak) and warm-build timings: Linux `bazel-build-test` p50 18s / p90 152s across 77 sampled jobs, roughly at parity with the M-series Macs. `n2-standard-8` works; expect the p90 cases to stretch. Do not go below 32 GB — the hosts set `--shutdown_on_low_sys_mem`, and `.bazelrc:1` sets `--jobs=200`.

Disk: each mono output base measures ~10 GB, the shared disk cache ~40 GB. 500 GB is comfortable. Note `.ci.bazelrc:4` sets `--experimental_disk_cache_gc_max_size=3T`, so cache GC will never fire on a smaller disk — if disk climbs, `rm -rf /mnt/ssd/bazel/disk_cache` between builds. **Do not edit `.ci.bazelrc`**; it is fleet-wide config sized for the real hosts.

Use persistent disk, not Local SSD — Local SSD is ephemeral, so every stop/start rebuilds the whole cache.

## Packages

```
build-essential git perl python3 unzip zip jq curl ca-certificates gnupg patch coreutils pkg-config libssl-dev
```

- `build-essential` — `MODULE.bazel` registers only Apple and zig-musl CC toolchains, so native Linux glibc builds fall through to the autodetected host compiler.
- `git perl python3 unzip coreutils` — `tools/test-sandbox/repositories.bzl` snapshots audited host tools and hard-fails if any are missing: `bash env git git-receive-pack git-upload-pack kill mkdir mktemp perl rm sed sh sleep`.
- `jq` — used by the release scripts on Linux agents.
- `patch` — `MODULE.bazel` patches `rules_rust` and `rules_apple`.
- `pkg-config` + `libssl-dev` — **flunge only, not mono.** The `openssl-sys` crate's build script shells out to `pkg-config` and fails with exit code 101 without it. The error names only `pkg-config` because that check runs first; installing it alone gets you the missing-headers failure next.

**Not needed:** Node, pnpm, npm (zero references in the build graph, no `package.json`). Not rustup either — Rust comes hermetically from `rules_rust`.

**Expect more of these.** The base list was derived from mono's graph. Any `-sys` crate in another pipeline can want a system library the image lacks. Read the build script's stderr; it names the package.

## The AppArmor user-namespace fix

`.bazelrc:64` sets `test:linux --strategy=TestRunner=linux-sandbox` — one strategy, deliberately no fallback, so a host that cannot provide it fails closed. Ubuntu 23.10+ ships `kernel.apparmor_restrict_unprivileged_userns=1`, which blocks it. Bazel probes sandbox support silently once at server startup; if the probe fails the strategy is never registered, surfacing much later as:

```
ERROR: 'linux-sandbox' was requested for mnemonic TestRunner but no strategy with that identifier was registered.
```

```
echo 'kernel.apparmor_restrict_unprivileged_userns = 0' | sudo tee /etc/sysctl.d/60-bazel-linux-sandbox.conf
sudo sysctl --system
```

Verify **as the agent user, never under sudo** — root can always create namespaces, so `sudo unshare -Urm true` succeeds regardless and proves nothing. Following that false signal is what misled the 2026-07-27 diagnosis.

```
sudo -u buildkite-agent unshare -Urm true && echo OK || echo FAIL
```

This re-enables unprivileged user namespaces host-wide, which is a real local-privesc surface. Accepted for a dedicated single-purpose builder.

**On Debian this sysctl may not exist** — it is a downstream patch. Run the probe and let it decide.

## `/mnt/ssd`

`.ci.linux.startup.bazelrc:1` sets `--output_user_root=/mnt/ssd/bazel/output_base`, and `.ci.bazelrc` puts the repository and disk caches there. `steps/ci-env.sh` passes that startup rc on every CI invocation. If `/mnt/ssd` is not writable by `buildkite-agent`, nothing builds. A plain directory on the boot disk is fine; the separate physical disk the old hosts used is not required.

## Bazel

Install **bazelisk** as `/usr/bin/bazel` — the repo owns the version via `.bazelversion`. Outside a workspace `bazel --version` prints bazelisk's default, not the pinned version; that is expected and has confused people before.

The dead hosts also had a system-wide `/etc/bazel.bazelrc` that exists in no repo, recovered from Bazel's rc-reading output in job logs:

```
startup --output_user_root=/mnt/ssd/bazel/output_base
startup --shutdown_on_low_sys_mem
build --jobs=HOST_RAM*.0003
```

Nice to have, not required — `.ci.linux.startup.bazelrc` already sets the same output root for CI. Its value is the OOM guard and sane `--jobs` for manual invocations.

**There is no remote cache.** No `--remote_cache`, `--bes_backend`, `--remote_executor` or `--google_credentials` anywhere. Caching is purely per-agent local disk, so a new host starts stone cold and its first build compiles the whole graph. Budget tens of minutes.

## A cold cache exposes dependencies the fleet has been hiding

Every long-lived host has a warm `repository_cache` and never re-fetches an external repo it already holds. A fresh host fetches everything — so any dependency whose remote has rotted fails **there and only there**, looking like a problem with the new machine.

This happened on the first GCE host: flunge's `MODULE.bazel` pinned `rules_mypy` via `git_override` to a personal fork that had been deleted from GitHub four days earlier. Every surviving host still built fine from cache.

**Read the failure before reaching for credentials.** GitHub answers a nonexistent or inaccessible repo over HTTPS with an auth challenge — `could not read Username for 'https://github.com'` — which reads exactly like a missing credential and is not one. Over SSH the same condition says `Repository not found`. If a public dependency asks for authentication, the URL is wrong, not your key.

**Do not add a `url.*.insteadOf` rewrite or a credential helper to make it go away.** No such mechanism exists anywhere in the fleet; adding one on a single host is an invisible divergence that outlives the outage. Fix the dependency declaration.

## Buildkite agent

Fleet version is pinned across all hosts — match it rather than taking stable, since version drift reads as flakiness.

```
tags="queue=bazel-any,os=linux,arch=amd64"
tags-from-gcp=false
tags-from-host=false
spawn=1
```

- `queue=bazel-any` — `pipeline.yml` uses `${BUILDKITE_ANY_QUEUE:-bazel-any}` for `bazel-build-test` and `checks`; also `pipeline-integrity.yml` and the release pipelines.
- `os=linux` — **required, not cosmetic.** The checkleft and changelog release pipelines select on it. Without this tag the host cannot claim the release jobs, which are the ones that go hard-down when Linux capacity is lost.
- `arch=amd64` — matches the fleet; nothing selects on it today.

`tags-from-gcp=false` matters on GCE — otherwise the agent auto-appends instance metadata tags and its metadata looks nothing like the fleet's.

Known fleet inconsistency: `empiricist-1/-2` registered `arg=amd64` (typo). Use `arch=`.

Start with `spawn=1`. Two agents on one box means two concurrent `--jobs=200` builds fighting over the same output base and disk cache.

## Credentials

**SSH key — required.** Buildkite checks out over SSH (`github.com:spinyfin/mono`), so without a working key every job fails in "Preparing working directory". `spinyfin/mono` has **zero deploy keys registered**; the Linux agents used an ambient key on a user or machine account, and the mechanism was never recorded. Recover it from a surviving host rather than guessing.

**Push capability is required for the release pipelines**, not just read — the release script pushes the tag on the ambient credential. A read-only key gets you `bazel-build-test` and `checks` but leaves the release steps failing at the push.

**`gh` — release pipelines only.** The release scripts call `gh release` and `gh api`. `checkleft` also calls `gh auth token`, but that path is best-effort and fails silently, so `checks` works without it. Authenticate as the `buildkite-agent` user.

**Azure CLI (`az`) — required for flunge's deploy step.** See the Azure CLI section above.

**Not needed:** the `BOSS_SHAKE_*` secrets (read only by the macOS-only boss release step), any remote-cache credential, `jj`, or Buildkite hooks — all three dead hosts had only stock `*.sample` hooks.

## Checkout

Nothing to do. CI hosts do not use cube. Buildkite manages the checkout at `/var/lib/buildkite-agent/builds/<agent-name>/flunge/<pipeline-slug>`. `repobin` is built per-build by `steps/ci-env.sh`.

## Locale

Minimal cloud images ship no generated locales while SSH clients forward `LANG`/`LC_*`, producing a wall of `setlocale` warnings.

Debian — `locale-gen` **ignores** a locale passed as an argument:

```
sudo apt-get install -y locales
sudo sed -i 's/^# *en_US.UTF-8 UTF-8/en_US.UTF-8 UTF-8/' /etc/locale.gen
sudo locale-gen
locale -a | grep -i en_US        # must list en_US.utf8 before continuing
sudo update-locale LANG=en_US.UTF-8
```

Ubuntu accepts the argument directly: `sudo locale-gen en_US.UTF-8`.

`update-locale` rejects a locale that has not actually been generated, with a confusing "invalid locale settings" error — hence the `locale -a` check.

**This changes builds, not just your shell.** `update-locale` sets the system default, which the `buildkite-agent` service inherits; `LC_COLLATE` governs sort order and locale-sensitive tests can pass under one setting and fail under another. To silence the warnings only, run `locale-gen` and skip `update-locale`. The locale the dead hosts used was never recorded.

## Azure CLI

The flunge release pipelines shell out to `az` to log in with a service principal and push container images. Without it the deploy step fails _after_ a successful image push, at the login.

```
curl -sL https://aka.ms/InstallAzureCLIDeb | sudo bash
az version
```

The failure looks like ``failed to run `az login --service-principal --username ... --password ...`: No such file or directory (os error 2)`` — `ENOENT`, meaning the binary was missing and the login never ran.

**That message prints the service principal password in cleartext into the build log**, alongside the username and tenant, giving any log reader the complete credential triple. Buildkite logs are retained and API-readable. If you hit this, treat the secret as compromised and rotate it — installing `az` stops the error but does nothing about what is already in the log. The underlying redaction defect is tracked separately against flunge.

## Tailnet and SSH

Join with a pre-authorized auth key (a headless VM cannot do the browser login) and enable Tailscale SSH:

```
curl -fsSL https://tailscale.com/install.sh | sh
sudo tailscale up --auth-key=<key> --hostname=<name> --ssh
```

**Disable key expiry** on the node in the admin console. Every node on the tailnet is user-owned and untagged; user-owned nodes expire on schedule and then silently drop off, which on an unattended builder means losing access at the worst moment.

Tailscale SSH needs an `ssh` rule in the tailnet policy. If the rule's action is `check` rather than `accept`, it periodically re-prompts for browser re-auth — `accept` is what makes it hands-off, at the cost of a real reduction in protection.

**`Permission denied (publickey)` means Tailscale is not intercepting at all** — that message comes from the real `sshd`. Three causes, in order of likelihood:

1. The SSH username is not a real local Unix account. Tailscale SSH maps it to a local user; with OS Login enabled, GCE derives account names from the Google identity. Check `getent passwd`.
2. `--ssh` never took — check `sudo tailscale debug prefs | grep -i runssh` for `"RunSSH": true`. `tailscale up` is not always additive across invocations.
3. No `ssh` rule in the policy, so tailscaled declines and the connection falls through to sshd.

OS Login and Tailscale SSH both want to own authentication: with OS Login on, `sshd` defers to Google's `AuthorizedKeysCommand` and ignores `~/.ssh/authorized_keys`, so an `ssh-copy-id` fallback silently will not work.

Tailscale does not remove the egress requirement — the VM still fetches from github.com, the BCR and crates.io on every cold build.

## Verification

1. `bk agent list` — expect `connected`, a `linux; amd64` user agent, and the three expected tags.
2. `sudo -u buildkite-agent unshare -Urm true` — FAIL here means every `bazel test` will fail, with an error that looks like a Bazel config problem rather than a host problem.
3. **Smoke-test on `bazel-any-test` first.** Bring the agent up tagged for that queue, then trigger with `BUILDKITE_ANY_QUEUE=bazel-any-test` (documented at the top of `pipeline.yml`). Flip to `bazel-any` once green. This keeps a broken agent from poisoning real PR builds.
4. For the fastest signal, run a single test manually on the box with `--bazelrc=.ci.linux.startup.bazelrc --config=ci-linux`.

## Cost and teardown

Stopping keeps the disk and caches, so a restart is warm, and bills only for the persistent disk. Deleting discards the warm cache — drain the agent first (`bk agent stop <agent-id>`, the UUID not the name) so a running job is not killed. Also delete the cluster agent token.

`bk agent pause <agent-id> --timeout-in-minutes <N>` for maintenance — pauses auto-expire, so pick `N` generously.

**Do not use Spot or preemptible.** A preemption mid-`bazel test` shows up as a red required check on someone's PR.

## Gaps this document cannot close

1. **SSH credential provenance** — repo has zero deploy keys, mechanism unrecorded, needs root on a surviving host.
2. **How `gh` was authenticated** on the dead hosts — same limitation.
3. **CPU/RAM of the dead hosts** — never recorded; sizing here is inferred.
4. **Full `buildkite-agent.cfg` contents** — needs root on a surviving host.

If you ever have root on a surviving Linux agent, capturing those four things and folding them into this document is worth the ten minutes.
