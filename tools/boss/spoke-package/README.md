# Signed spoke packages and services

`boss-spoke-package` builds, signs, verifies and installs the `boss-spoke`
worker-host daemon. It implements the install half of the hub-and-spoke design
([Manual enrollment and upgrades](../docs/designs/hub-and-spoke-distributed-engine.md)):
a person installs a signed package by hand on a host, and the host does nothing
until it is enrolled. **Installing files enrolls nothing, and an un-enrolled
host's service never starts.**

## What a release is

For each platform (`macos-arm64`, `linux-amd64`, `linux-arm64`) a release is:

| Asset                                     | What it is                                                                         |
| ----------------------------------------- | ---------------------------------------------------------------------------------- |
| `boss-spoke-<platform>.tar.gz`            | `bin/boss-spoke` (the daemon) and `bin/boss-spoke-package` (this installer).       |
| `boss-spoke-<platform>.manifest.json`     | Schema, release id, platform, and the package's file name, URL, SHA-256 and size.  |
| `boss-spoke-<platform>.manifest.json.sig` | Detached Ed25519 signature over the manifest's exact bytes.                        |
| `boss-spoke-package-<platform>`           | The installer on its own: what a person runs first on a host that has nothing yet. |
| `boss-spoke-package-<platform>.sig`       | Detached Ed25519 signature over the standalone installer bytes.                    |

Verification order is fixed and every step is a hard stop (there is no unsigned
fallback): the **signature over the raw manifest bytes** (the manifest is never
parsed before it verifies), then the manifest's own validity (schema, plain
release id, `https` URL, 64-hex digest), then the **platform** against the host,
then the package's **size and SHA-256**. Failures carry a stable code:
`manifest_sig`, `manifest_malformed`, `platform_mismatch`, `digest_mismatch`,
`origin_not_https`, `unsupported_host`, `key_invalid`. The archive is then
unpacked as hostile input (regular files only, plain relative paths, no links,
bounded size, never setuid or group/world-writable).

## Trust

The release key is an Ed25519 key, separate from the hub's host CA.

- `boss-spoke-package keygen --private-out release.key --public-out release.pub`
  makes a pair. The private half goes to the release pipeline's secret store as
  `BOSS_SPOKE_RELEASE_SIGNING_KEY`; the public half and its fingerprint are
  published to the people who install.
- The **first** install on a host needs `--trusted-key release.pub`. After
  confirming the fingerprint the installer prints with the release owner, the
  key is **pinned** in `/etc/boss-spoke/release-verify-key.pub` (root-owned).
  Later installs verify against the pinned key. Giving a different key is
  refused (`trusted_key_changed`); to trust a new key, remove the pinned file as
  root, deliberately.
- Nothing is pinned until a release has verified.

This repository holds no release key: generating it, storing the secret and
registering the release pipeline are the fleet administrator's steps, so
packages ship inert until then.

### Verifying the first-run installer

The program a person runs first on a fresh host is itself a release asset. Its
SHA-256 is listed as `installer` in the signed manifest, and the release also
publishes `boss-spoke-package-<platform>.sig` over the binary bytes. Confirm the
release public key's fingerprint with the owner, then either:

1. Verify the **manifest** signature, then check the installer digest against
   `installer.sha256` in that manifest (`sha256sum` / `shasum -a 256`).
2. Verify the **installer** signature directly. The signature file is base64 of
   a raw Ed25519 signature over the binary. Convert the `ed25519:<base64>`
   public key to SPKI PEM (prefix `302a300506032b6570032100` plus the 32 raw
   key bytes) and run `openssl pkeyutl -verify -rawin -pubin`.

A host that already has a trusted copy of this installer can keep using
`boss-spoke-package verify` for later packages. The tarball is verified the
same way before `install` unpacks it.

## Installing (as root, on the worker host)

```sh
boss-spoke-package install \
  --manifest boss-spoke-linux-amd64.manifest.json \
  --signature boss-spoke-linux-amd64.manifest.json.sig \
  --package boss-spoke-linux-amd64.tar.gz \
  --trusted-key release.pub \
  --host-id <host-id> --hub-url https://hub.example:7443 --hub-ca hub-ca.pem
```

It lays out, and is safe to run again with the same inputs (nothing changes,
nothing restarts):

| Path (Linux; macOS differs only in prefix, home and definition) | Owner / mode            | Purpose                                             |
| --------------------------------------------------------------- | ----------------------- | --------------------------------------------------- |
| `/opt/boss-spoke/releases/<release-id>/`                        | root, 0755              | Immutable release; a release id is never reused.    |
| `/opt/boss-spoke/current`                                       | root, symlink           | Atomically switched to the active release.          |
| `/var/lib/boss-worker/spoke`                                    | `boss-worker`, **0700** | State: journal, tmux socket, credentials, homes.    |
| `…/spoke/credentials`                                           | `boss-worker`, **0700** | `ca.pem` (hub trust); enrollment adds cert and key. |
| `…/spoke/homes`                                                 | `boss-worker`, **0700** | `BOSS_CODEX_HOMES_DIR`: per-run driver homes.       |
| `/etc/boss-spoke/{install.json,release-verify-key.pub}`         | root, 0644              | What was installed, and the pinned release key.     |
| `/etc/systemd/system/boss-spoke.service` (Linux)                | root, 0644              | The service definition.                             |
| `/Library/LaunchDaemons/dev.boss.spoke.plist` (macOS)           | root, 0644              | The service definition.                             |

Directories the service user controls are verified, never repaired: an existing
one that is a symlink, not owned by the service user, or not owner-only stops
the install, because root must not chown or chmod paths another user can swap.

The `boss-worker` account is created when missing: `useradd --system` on Linux,
and a hidden non-login account (created with `dscl`, a free UID in 200–400,
primary group `staff`, and shell `/usr/bin/false`) on macOS. The service definition names only the user, so
systemd and launchd use that account's primary group.

### The service environment

Both definitions run the daemon as `boss-worker` with a private umask and the
same environment, defined once in `service.rs`: `HOME`, a `PATH` that finds
tmux, cube, jj, git, gh, python3 and the driver CLIs, and
**`BOSS_CODEX_HOMES_DIR` pinned to `<state>/homes`**. An unset variable would
fall back to a shared temp directory, which a managed host must never use.

Both are written so the daemon is **not started until it is enrolled** (systemd
`ConditionPathExists` on the certificate and key; launchd `KeepAlive/PathState`
on the certificate), and so a **restart keeps running workers**: the daemon's
private tmux server lives in the service's process group, so systemd uses
`KillMode=process` and launchd `AbandonProcessGroup`. The restarted daemon
adopts the surviving panes by spawn token and replays its journal (covered by
`//tools/boss/engine/spoke:spoke_supervisor_tmux_test`, which restarts a real
daemon over a live tmux session).

### Upgrades

Install a newer signed package the same way. `current` is switched, and an
enrolled host's service is restarted and must stay up (default: 10 s stable
within 120 s). Nothing rolls back automatically: the previous release stays in
`releases/` and the person reinstalls its package by hand. A host id never
changes after the first install (the journal belongs to it).

## Checking a host: `preflight`

```sh
boss-spoke-package preflight [--storage-only] [--repo <url>]...
```

Run as root or as `boss-worker`, it re-runs the **installed**
`boss-spoke-package` (the root-owned 0755 copy under `current/`) **as the
service user with exactly the installed service environment** and prints one
`PASS`/`FAIL` line
per check, then `failed_preflights: …` and a non-zero exit if any failed:

| Check                                                 | Passes when                                                                     |
| ----------------------------------------------------- | ------------------------------------------------------------------------------- |
| `private_storage`                                     | State (and credential) directory are real, owner-only and owned by the user.    |
| `private_homes`                                       | `BOSS_CODEX_HOMES_DIR` is **set**, is `<state>/homes`, owner-only and writable. |
| `disk_space`, `clock`                                 | At least 16 GiB free; wall clock is not before 2026-01-01.                      |
| `tmux`, `tool_cube`, `tool_jj`, `tool_git`, `tool_gh` | Present and runnable; tmux is at least the version Boss requires.               |
| `tool_claude`, `tool_codex`                           | Present and runnable (`--version`).                                             |
| `python_pty`                                          | `python3` can import `fcntl`, `pty` and `termios` (the brief relay needs them). |
| `github_credentials`                                  | `gh auth status` succeeds. Its output is discarded: it names accounts.          |
| `claude_credentials`                                  | `claude -p /usage --output-format json` succeeds. Output is discarded.          |
| `codex_credentials`                                   | Codex `account/rateLimits/read` over app-server succeeds. Output is discarded.  |
| `hub_reachable`                                       | A TCP connection to the hub opens.                                              |
| `repo_access`                                         | `git ls-remote` reads every `--repo`. Failures name a count, never a URL.       |

`install` runs the storage checks the same way before it points `current` or a
service definition at the new release; a failure stops it. The daemon runs the
same checks at startup and every five minutes (without `hub_reachable`, which a
connected daemon proves, and `repo_access`) and reports the failing names as
`Inventory.failed_preflights` (`preflight_pending` until the first run ends), so
a host that has not proven itself stays unschedulable. The hub side of that is
the capacity scheduling work.

## Releasing

`.buildkite/steps/spoke-release.sh` (pipeline `.buildkite/pipeline-spoke-release.yml`,
product record `tools/boss/spoke-release.toml`) builds with Bazel on each
platform's agent, runs `build` and `sign`, and publishes through `//tools/release`.
It refuses to run without the signing key. It is not registered anywhere yet.
There is no Linux arm64 agent, so those assets are optional.

## Tests

`bazel test //tools/boss/spoke-package/... //tools/boss/engine/spoke-preflight/...`

- `verification_test`: signature, digest, platform, manifest validation, key
  handling, hostile archives.
- `install_test`: the installer against a staging root with the real binaries
  executed as the service user: layout and modes, idempotence, upgrade and
  restart, key pinning, wrong platform and bad digest installing nothing, unsafe
  state directories refused, the unset-pin refusal, and both service definitions.

The real `System` host (account creation, systemctl, launchctl) is exercised only
on a real host: nothing in the test suite creates accounts or drives a service
manager. `install --root DIR` stages into a directory with those steps disabled.
