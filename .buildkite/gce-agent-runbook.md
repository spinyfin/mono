# Standing up a Linux CI agent on GCE

Written 2026-08-24, after a power cut killed every Linux CI host (`empiricist`, `zoologist`, `diziet`) and none returned. A replacement was provisioned on a Google Cloud VM and taken to green end-to-end. This is what that took.

Companion to `linux-agents-runbook.md`, which covers diagnosing an _existing_ host. This one covers building a new one.

## Minimum viable path

Order matters — step 3 creates the user that steps 4 onward depend on. Steps 4, 8 and 10 are where this goes wrong; the rest is mechanical.

1. **x86_64 only.** `n2-standard-16`, 500 GB pd-balanced. Do not pick an Axion/`t2a` type.
2. Install packages (§ Packages), plus Node ≥ 22 and `npx` (§ Node).
3. **Install the `buildkite-agent` package.** This creates the `buildkite-agent` user that later steps run as.
4. **Disable the AppArmor userns restriction** (Ubuntu). Without this every `bazel test` fails.
5. `mkdir -p /mnt/ssd/bazel`, chown to `buildkite-agent`.
6. Install bazelisk as `/usr/bin/bazel`.
7. Set the agent token and `tags="queue=bazel-any,os=linux,arch=amd64"`.
8. SSH credential at `/var/lib/buildkite-agent/.ssh/`, mode 0600, plus `known_hosts`.
9. `systemctl enable --now buildkite-agent`.
10. Verify with `bk agent list`, then smoke-test on `bazel-any-test` before joining the real queue.

Tailnet membership and passwordless SSH are optional — get green first.

## Architecture: x86_64, not negotiable

`tools/checkleft/release.toml` (and `tools/release/release.toml`) declare the required asset names, including `checkleft-x86_64-unknown-linux-gnu` and its musl equivalent. `steps/checkleft-release.sh`'s `phase_musl` then _executes_ the built musl binary on the agent to verify its version, dying with "musl version check could not execute the binary" (or a version mismatch) if the agent architecture is wrong. `steps/changelog-release.sh:522` does the same for `${ASSET_PREFIX}-x86_64-unknown-linux-gnu`.

`MODULE.bazel` does register `wasm_tools_aarch64_linux`, so arm64 is not technically impossible for `bazel-build-test` — it would still break the release pipelines.

## Create the VM

Confirm the image family first; the exact published name changes over time.

```sh
gcloud compute images list --filter="family~ubuntu-2604" --project=ubuntu-os-cloud
```

```sh
gcloud compute instances create bk-bazel-any-gce-1 \
  --project=PROJECT \
  --zone=ZONE \
  --machine-type=n2-standard-16 \
  --image-family=ubuntu-2604-lts-amd64 \
  --image-project=ubuntu-os-cloud \
  --boot-disk-size=500GB \
  --boot-disk-type=pd-balanced \
  --boot-disk-device-name=bk-bazel-any-gce-1 \
  --metadata=enable-oslogin=TRUE \
  --labels=purpose=buildkite-ci,stopgap=true \
  --tags=buildkite-agent
```

```sh
gcloud compute ssh bk-bazel-any-gce-1 --zone=ZONE --project=PROJECT
```

- **No inbound firewall rule.** The Buildkite agent is outbound-only (long-poll to `agent.buildkite.com`). Reach the box via `gcloud compute ssh` or the tailnet.
- **No special scopes.** Nothing in the build reads GCP APIs — there is no GCS remote cache. The default service account is fine.
- **External IP matters.** It is what gives egress to github.com, buildkite.com, the BCR, crates.io and GitHub release tarballs. If project policy forbids external IPs you need Cloud NAT on the subnet, or the first bazel build hangs fetching dependencies.

### Sizing

The dead hosts' CPU and RAM were never recorded. `n2-standard-16` (16 vCPU / 64 GB) is inferred from observed action parallelism (58 concurrent test actions at peak) and warm-build timings: Linux `bazel-build-test` p50 18s / p90 152s across 77 sampled jobs, roughly at parity with the M-series Macs. `n2-standard-8` works; expect the p90 cases to stretch. Do not go below 32 GB — the hosts set `--shutdown_on_low_sys_mem`, and `.bazelrc:1` sets `--jobs=200`.

Disk: each mono output base measures ~10 GB, the shared disk cache ~40 GB. 500 GB is comfortable. Note `.ci.bazelrc:4` sets `--experimental_disk_cache_gc_max_size=3T`, so cache GC will never fire on a smaller disk. If disk climbs:

```sh
sudo rm -rf /mnt/ssd/bazel/disk_cache
```

**Do not edit `.ci.bazelrc`** — it is fleet-wide config sized for the real hosts.

Use persistent disk, not Local SSD. Local SSD is ephemeral, so every stop/start rebuilds the whole cache.

## Packages

```sh
sudo apt-get update
sudo apt-get install -y \
  build-essential git perl python3 unzip zip \
  jq curl ca-certificates gnupg patch coreutils \
  pkg-config libssl-dev
```

Why each — this list is derived from the build graph, not generic:

- **`build-essential`** — `MODULE.bazel` registers only Apple and zig-musl CC toolchains, so native Linux glibc builds fall through to the autodetected host compiler.
- **`git perl python3 unzip coreutils`** — `tools/test-sandbox/repositories.bzl` snapshots audited host tools and hard-fails if any are missing: `bash env git git-receive-pack git-upload-pack kill mkdir mktemp perl rm sed sh sleep`.
- **`jq`** — used by the release scripts on Linux agents.
- **`patch`** — `MODULE.bazel` patches `rules_rust` and `rules_apple`.
- **`pkg-config` + `libssl-dev`** — **flunge only, not mono.** The `openssl-sys` crate's build script shells out to `pkg-config` and fails with exit code 101 without it. The error names only `pkg-config` because that check runs first; installing it alone gets you the missing-headers failure next.

**Not needed:** pnpm, and npm as a package manager — no `package.json`, no `rules_js`. Node and `npx` **are** required; see § Node. Not rustup either: Rust comes hermetically from `rules_rust` via `rust-toolchain.toml`, and `rustup` appears only in the darwin-only cross-build phase.

**Expect more of these.** The base list was derived from mono's graph. Any `-sys` crate in another pipeline can want a system library the image lacks. Read the build script's stderr; it names the package.

## Install the Buildkite agent

Do this **before** the steps below, because they run commands as `buildkite-agent` and chown files to it. That user does not exist until this package is installed. Configuration comes later.

```sh
sudo mkdir -p /usr/share/keyrings
curl -fsSL https://keys.openpgp.org/vks/v1/by-fingerprint/32A37959C2FA5C3C99EFBC32A79206696452D198 \
  | sudo gpg --dearmor -o /usr/share/keyrings/buildkite-agent-archive-keyring.gpg
echo "deb [signed-by=/usr/share/keyrings/buildkite-agent-archive-keyring.gpg] https://apt.buildkite.com/buildkite-agent stable main" \
  | sudo tee /etc/apt/sources.list.d/buildkite-agent.list
sudo apt-get update
sudo apt-get install -y buildkite-agent=3.127.1
sudo apt-mark hold buildkite-agent

# confirm the user exists before continuing
id buildkite-agent
```

Fleet version is 3.127.1 across all hosts — match it. Version drift reads as flakiness. The GPG fingerprint rotates occasionally; cross-check against Buildkite's Ubuntu docs if the key step fails.

The package provides `/usr/bin/buildkite-agent`, `/etc/buildkite-agent/buildkite-agent.cfg`, `/etc/buildkite-agent/hooks/`, the `buildkite-agent` system user, `/var/lib/buildkite-agent`, and a plain `buildkite-agent.service`. It will not start usefully until it has a token — that is expected.

## The AppArmor user-namespace fix

`.bazelrc:64` sets `test:linux --strategy=TestRunner=linux-sandbox` — one strategy, deliberately no fallback, so a host that cannot provide it fails closed. Ubuntu 23.10+ ships `kernel.apparmor_restrict_unprivileged_userns=1`, which blocks it. Bazel probes sandbox support silently once at server startup; if the probe fails the strategy is never registered, surfacing much later as:

```
ERROR: 'linux-sandbox' was requested for mnemonic TestRunner but no strategy with that identifier was registered.
```

```sh
echo 'kernel.apparmor_restrict_unprivileged_userns = 0' \
  | sudo tee /etc/sysctl.d/60-bazel-linux-sandbox.conf
sudo sysctl --system
```

Verify **as the agent user, never under sudo**:

```sh
sudo -u buildkite-agent unshare -Urm true && echo OK || echo FAIL
sysctl kernel.apparmor_restrict_unprivileged_userns user.max_user_namespaces
```

Root can always create namespaces, so `sudo unshare -Urm true` succeeds regardless and proves nothing. Following that false signal is what misled the 2026-07-27 diagnosis.

This re-enables unprivileged user namespaces host-wide, which is a real local-privesc surface. Accepted for a dedicated single-purpose builder.

**On Debian this sysctl may not exist** — it is a downstream patch. Run the probe and let it decide.

## `/mnt/ssd`

`.ci.linux.startup.bazelrc:1` sets `--output_user_root=/mnt/ssd/bazel/output_base`, and `.ci.bazelrc` puts the repository and disk caches there. `steps/ci-env.sh` passes that startup rc on every CI invocation. If `/mnt/ssd` is not writable by `buildkite-agent`, nothing builds.

```sh
sudo mkdir -p /mnt/ssd/bazel
sudo chown -R buildkite-agent:buildkite-agent /mnt/ssd
```

A plain directory on the boot disk is fine. The separate physical disk the old hosts used is not required. If you did attach a second disk:

```sh
sudo mkfs.ext4 -F /dev/disk/by-id/google-DEVICE_NAME
sudo mkdir -p /mnt/ssd
sudo mount -o discard,defaults /dev/disk/by-id/google-DEVICE_NAME /mnt/ssd
echo "/dev/disk/by-id/google-DEVICE_NAME /mnt/ssd ext4 discard,defaults,nofail 0 2" \
  | sudo tee -a /etc/fstab
sudo mkdir -p /mnt/ssd/bazel
sudo chown -R buildkite-agent:buildkite-agent /mnt/ssd
```

## Bazel

Install **bazelisk** as `/usr/bin/bazel` — the repo owns the version via `.bazelversion`.

```sh
BAZELISK_VERSION=v1.27.0   # check the bazelisk releases page for current
sudo curl -fsSL -o /usr/bin/bazel \
  "https://github.com/bazelbuild/bazelisk/releases/download/${BAZELISK_VERSION}/bazelisk-linux-amd64"
sudo chmod 0755 /usr/bin/bazel
```

Outside a workspace `bazel --version` prints bazelisk's own default, not the pinned version. That is expected and has confused people before.

The dead hosts also had a system-wide `/etc/bazel.bazelrc` that exists in no repo, recovered from Bazel's rc-reading output in job logs:

```sh
sudo tee /etc/bazel.bazelrc >/dev/null <<'RCEOF'
startup --output_user_root=/mnt/ssd/bazel/output_base
startup --shutdown_on_low_sys_mem
build --jobs=HOST_RAM*.0003
RCEOF
```

Nice to have, not required — `.ci.linux.startup.bazelrc` already sets the same output root for CI. Its value is the OOM guard and sane `--jobs` for manual invocations outside a workspace.

**There is no remote cache.** No `--remote_cache`, `--bes_backend`, `--remote_executor` or `--google_credentials` anywhere. Caching is purely per-agent local disk, so a new host starts stone cold and its first build compiles the whole graph. Budget tens of minutes.

### A cold cache exposes dependencies the fleet has been hiding

Every long-lived host has a warm `repository_cache` and never re-fetches an external repo it already holds. A fresh host fetches everything — so any dependency whose remote has rotted fails **there and only there**, looking like a problem with the new machine.

This happened on the first GCE host: flunge's `MODULE.bazel` pinned `rules_mypy` via `git_override` to a personal fork that had been deleted from GitHub four days earlier. Every surviving host still built fine from cache.

**Read the failure before reaching for credentials.** GitHub answers a nonexistent or inaccessible repo over HTTPS with an auth challenge — `could not read Username for 'https://github.com'` — which reads exactly like a missing credential and is not one. Over SSH the same condition says `Repository not found`. If a public dependency asks for authentication, the URL is wrong, not your key.

**Do not add a `url.*.insteadOf` rewrite or a credential helper to make it go away.** No such mechanism exists anywhere in the fleet; adding one on a single host is an invisible divergence that outlives the outage. Fix the dependency declaration.

## Node

```sh
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
sudo apt-get install -y nodejs
node --version && npx --version
```

`checks` shells out to `npx --yes oxfmt@0.55.0` and `oxlint@1.70.0` via checkleft's declarative external-tool resolution. Node is a host package, not a Bazel toolchain — `MODULE.bazel` has no `rules_js`, `rules_nodejs`, or node `http_archive`.

Node **≥ 22** specifically, enforced by `MIN_NODE_MAJOR_VERSION` in `tools/checkleft/src/external/declarative/resolve.rs:218`. A Node 20 host fails with `ERR_UNKNOWN_FILE_EXTENSION`, which does not look like a Node version problem.

If the agent is already running, restart it so it picks up the new PATH:

```sh
sudo systemctl restart buildkite-agent
```

`checks` is a **required merge context** (`REQUIRED_CHECKS.md:19`), and `format/oxc`'s include glob covers `md, markdown, mdx, yaml, yml, toml, json, css, html`, so it fires on nearly every PR — including doc-only ones. Without Node a fresh host can run `bazel-build-test` fine but cannot pass PR CI at all.

`mono-integrity` → `integrity-checkleft` (`.buildkite/steps/integrity-checkleft.sh:15`) also needs it. `bazel-build-test`, `mac-app-build`, `boss-release`, `checkleft-release` and `changelog-release` do not.

There is a declared fallback to a bare `oxfmt`/`oxlint` on `PATH`, but on a host with neither `npx` nor those binaries it resolves to the bare name and then fails at spawn — it does not save you.

## Configure the agent

Create a new cluster agent token in the Buildkite UI: **Agents → Default cluster → Agent Tokens → New Token**. The value is shown once.

```sh
sudo tee /etc/buildkite-agent/buildkite-agent.cfg >/dev/null <<'CFGEOF'
token="PASTE_CLUSTER_AGENT_TOKEN_HERE"
name="%hostname-%spawn"
tags="queue=bazel-any,os=linux,arch=amd64"
tags-from-gcp=false
tags-from-host=false
spawn=1
build-path="/var/lib/buildkite-agent/builds"
hooks-path="/etc/buildkite-agent/hooks"
plugins-path="/etc/buildkite-agent/plugins"
CFGEOF
sudo chown buildkite-agent:buildkite-agent /etc/buildkite-agent/buildkite-agent.cfg
sudo chmod 0600 /etc/buildkite-agent/buildkite-agent.cfg
```

The tags line is the whole point:

- `queue=bazel-any` — `pipeline.yml` uses `${BUILDKITE_ANY_QUEUE:-bazel-any}` for `bazel-build-test` and `checks`; also `pipeline-integrity.yml` and the release pipelines.
- `os=linux` — **required, not cosmetic.** The checkleft and changelog release pipelines select on it. Without this tag the host cannot claim the release jobs, which are the ones that go hard-down when Linux capacity is lost.
- `arch=amd64` — matches the fleet; nothing selects on it today.

`tags-from-gcp=false` matters on GCE — otherwise the agent auto-appends instance metadata tags and its metadata looks nothing like the fleet's.

Known fleet inconsistency: `empiricist-1/-2` registered `arg=amd64` (typo). Use `arch=`.

Start with `spawn=1`. Two agents on one box means two concurrent `--jobs=200` builds fighting over the same output base and disk cache. To run two later, set `spawn=2` and restart — one process, two registrations. Do not create a second systemd unit.

## Credentials

### SSH key — required

Buildkite checks out over SSH (`github.com:spinyfin/mono`), so without a working key every job fails in "Preparing working directory".

```sh
sudo -u buildkite-agent mkdir -p /var/lib/buildkite-agent/.ssh
sudo -u buildkite-agent chmod 700 /var/lib/buildkite-agent/.ssh
sudo -u buildkite-agent ssh-keyscan github.com \
  | sudo -u buildkite-agent tee /var/lib/buildkite-agent/.ssh/known_hosts >/dev/null
sudo install -o buildkite-agent -g buildkite-agent -m 0600 \
  /path/to/private_key /var/lib/buildkite-agent/.ssh/id_ed25519
sudo -u buildkite-agent ssh -T git@github.com
```

`spinyfin/mono` has **zero deploy keys registered**; the Linux agents used an ambient key on a user or machine account, and the mechanism was never recorded. Recover it from a surviving host rather than guessing.

**Push capability is required for the release pipelines**, not just read — the release script pushes the tag on the ambient credential. A read-only key gets you `bazel-build-test` and `checks` but leaves the release steps failing at the push.

### `gh` — release pipelines only

The release scripts call `gh release` and `gh api`. `checkleft` also calls `gh auth token`, but that path is best-effort and fails silently, so `checks` works without it.

```sh
sudo mkdir -p -m 755 /etc/apt/keyrings
curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
  | sudo tee /etc/apt/keyrings/githubcli-archive-keyring.gpg >/dev/null
sudo chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg
echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
  | sudo tee /etc/apt/sources.list.d/github-cli.list >/dev/null
sudo apt-get update
sudo apt-get install -y gh
sudo -u buildkite-agent -H gh auth login
sudo -u buildkite-agent -H gh auth status
```

### Azure CLI — flunge deploy step

The flunge release pipelines shell out to `az` to log in with a service principal and push container images. Without it the deploy step fails _after_ a successful image push, at the login.

```sh
curl -sL https://aka.ms/InstallAzureCLIDeb | sudo bash
az version
```

The failure looks like ``failed to run `az login --service-principal --username ... --password ...`: No such file or directory (os error 2)`` — `ENOENT`, meaning the binary was missing and the login never ran.

**That message prints the service principal password in cleartext into the build log**, alongside the username and tenant, giving any log reader the complete credential triple. Buildkite logs are retained and API-readable. If you hit this, treat the secret as compromised and rotate it — installing `az` stops the error but does nothing about what is already in the log.

### Not needed

The `BOSS_SHAKE_*` secrets (read only by the macOS-only boss release step), any remote-cache credential, `jj`, and Buildkite hooks — all three dead hosts had only stock `*.sample` hooks.

## Checkout

Nothing to do. CI hosts do not use cube. Buildkite manages the checkout at `/var/lib/buildkite-agent/builds/<agent-name>/flunge/<pipeline-slug>`, doing `git clean -ffxdq`, `git fetch --prune`, `git checkout -f`. `repobin` is built per-build by `steps/ci-env.sh`.

## Locale

Minimal cloud images ship no generated locales while SSH clients forward `LANG`/`LC_*`, producing a wall of `setlocale` warnings.

Debian — `locale-gen` **ignores** a locale passed as an argument:

```sh
sudo apt-get install -y locales
sudo sed -i 's/^# *en_US.UTF-8 UTF-8/en_US.UTF-8 UTF-8/' /etc/locale.gen
sudo locale-gen
locale -a | grep -i en_US        # must list en_US.utf8 before continuing
sudo update-locale LANG=en_US.UTF-8
```

Ubuntu accepts the argument directly:

```sh
sudo apt-get install -y locales
sudo locale-gen en_US.UTF-8
sudo update-locale LANG=en_US.UTF-8
```

`update-locale` rejects a locale that has not actually been generated, with a confusing "invalid locale settings" error — hence the `locale -a` check.

**This changes builds, not just your shell.** `update-locale` sets the system default, which the `buildkite-agent` service inherits; `LC_COLLATE` governs sort order and locale-sensitive tests can pass under one setting and fail under another. To silence the warnings only, run `locale-gen` and skip `update-locale`. The locale the dead hosts used was never recorded.

## Start the agent

```sh
sudo systemctl enable --now buildkite-agent
sudo systemctl status buildkite-agent
sudo journalctl -u buildkite-agent -f
```

The packaged unit matches the fleet: `User=buildkite-agent`, `Environment=HOME=/var/lib/buildkite-agent`, `ExecStart=/usr/bin/buildkite-agent start`. `HOME` matters — `.bazelrc:2` sets `--disk_cache=~/.cache/bazelcache` for non-CI invocations.

## Verification

### Registered on the right queue

```sh
bk agent list --output json \
  | jq '.[] | select(.hostname|test("bk-bazel-any-gce")) | {name, connection_state, version, meta_data}'
```

Expect `connected`, a `linux; amd64` user agent, and `["queue=bazel-any","os=linux","arch=amd64"]`.

### Sandbox probe — before trusting any green build

```sh
sudo -u buildkite-agent unshare -Urm true && echo OK || echo FAIL
```

FAIL means the AppArmor step did not take, and every `bazel test` will fail with an error that looks like a Bazel config problem rather than a host problem.

### Smoke-test on `bazel-any-test` first

Bring the agent up tagged `queue=bazel-any-test`, then:

```sh
bk build create --pipeline mono --branch main \
  --env BUILDKITE_ANY_QUEUE=bazel-any-test
```

`pipeline.yml` documents that override at the top. Expect the first build to be slow — cold caches, full graph. When `bazel-build-test` and `checks` both pass, flip the tag to `queue=bazel-any` and restart the agent. This keeps a broken agent from poisoning real PR builds.

### Fastest signal — one test on the box

```sh
sudo -u buildkite-agent -H bash -c '
  cd /var/lib/buildkite-agent/builds/*/flunge/mono &&
  bazel --bazelrc=.ci.linux.startup.bazelrc test --config=ci-linux \
    --test_output=errors -- //tools/boss/engine/worker-policy:worker-policy_test'
```

If `linux-sandbox` is unregistered you see it here in seconds instead of after a 30-minute build.

### Prove the release path

```sh
bk build list --pipeline mono-checkleft-release --limit 3 --output json \
  | jq '.[] | {number, state, jobs: [.jobs[] | select(.type=="script") | {step_key, state, agent: .agent.name}]}'
```

`checkleft-release-prepare` should stop showing `state: "limited"` and get claimed by the new agent.

## Tailnet and passwordless SSH

Optional; do it after the build is green. Generate a pre-authorized auth key in the Tailscale admin console — a headless VM cannot do the interactive browser login.

```sh
curl -fsSL https://tailscale.com/install.sh | sh
sudo tailscale up \
  --auth-key=tskey-auth-XXXXX \
  --hostname=bk-bazel-any-gce-1 \
  --ssh
tailscale status
tailscale ip -4
```

Then from a laptop:

```sh
ssh <local-user>@bk-bazel-any-gce-1
```

**Disable key expiry** on the node in the admin console as soon as it appears. Every node on the tailnet is user-owned and untagged; user-owned nodes expire on schedule and then silently drop off, which on an unattended builder means losing access at the worst moment.

Tailscale SSH needs an `ssh` rule in the tailnet policy. If the rule's action is `check` rather than `accept`, it periodically re-prompts for browser re-auth; `accept` is what makes it hands-off, at the cost of a real reduction in protection.

**`Permission denied (publickey)` means Tailscale is not intercepting at all** — that message comes from the real `sshd`. Three causes, in order of likelihood:

1. The SSH username is not a real local Unix account. Tailscale SSH maps it to a local user; with OS Login enabled, GCE derives account names from the Google identity.

   ```sh
   getent passwd | grep -i -E 'cairndubh|brian'
   ```

2. `--ssh` never took. `tailscale up` is not always additive across invocations.

   ```sh
   sudo tailscale debug prefs | grep -i runssh    # want "RunSSH": true
   sudo tailscale up --ssh
   ```

3. No `ssh` rule in the tailnet policy, so tailscaled declines and the connection falls through to sshd.

OS Login and Tailscale SSH both want to own authentication: with OS Login on, `sshd` defers to Google's `AuthorizedKeysCommand` and ignores `~/.ssh/authorized_keys`, so an `ssh-copy-id` fallback silently will not work. Turn OS Login off on the instance if you prefer ordinary keys.

Tailscale does not remove the egress requirement — the VM still fetches from github.com, the BCR and crates.io on every cold build.

## Cost and teardown

```sh
# Stop — keeps disk and caches, so a restart is warm. Bills only for the disk.
gcloud compute instances stop bk-bazel-any-gce-1 --zone=ZONE --project=PROJECT

# Drain before deleting so a running job is not killed.
# agent-id is the UUID from `bk agent list`, not the name.
bk agent stop AGENT_ID

gcloud compute instances delete bk-bazel-any-gce-1 --zone=ZONE --project=PROJECT
```

Also delete the cluster agent token you created. For maintenance:

```sh
bk agent pause AGENT_ID --timeout-in-minutes 60
bk agent resume AGENT_ID
```

Pauses auto-expire, so pick the timeout generously.

**Do not use Spot or preemptible.** A preemption mid-`bazel test` shows up as a red required check on someone's PR.

## Gaps this document cannot close

1. **SSH credential provenance** — repo has zero deploy keys, mechanism unrecorded, needs root on a surviving host.
2. **How `gh` was authenticated** on the dead hosts — same limitation.
3. **CPU/RAM of the dead hosts** — never recorded; sizing here is inferred.
4. **Full `buildkite-agent.cfg` contents** — needs root on a surviving host.
5. **Node is required but no provisioning artifact in the fleet installs it.** Long-lived hosts have it incidentally from a base image or a hand install. That is why a clean host failed `checks` while surviving hosts did not.

If you ever have root on a surviving Linux agent, capturing those four things and folding them into this document is worth the ten minutes.

## Appendix: required vs optional

**Required to run at all** — without these the agent cannot take `bazel-build-test` and `checks` to green:

- x86_64 host (`n2-standard-16`, 500 GB pd-balanced)
- packages in § Packages
- `node` / `npx` (Node ≥ 22; § Node)
- `buildkite-agent` 3.127.1 (creates the `buildkite-agent` user)
- AppArmor userns restriction off (Ubuntu)
- `/mnt/ssd/bazel` owned by `buildkite-agent`
- bazelisk as `/usr/bin/bazel`
- cluster token and `tags="queue=bazel-any,os=linux,arch=amd64"`
- SSH credential at `/var/lib/buildkite-agent/.ssh/`, mode 0600, plus `known_hosts`

**Not required to run at all:** `gh` (release pipelines only), Azure CLI (flunge deploy), tailnet / passwordless SSH, `/etc/bazel.bazelrc`, rustup, pnpm, npm-as-a-package-manager.
