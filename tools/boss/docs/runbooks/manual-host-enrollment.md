# Runbook: Manual spoke host enrollment

Install the signed spoke package by hand, enroll the host's identity with a single-use token from the hub, and check it. This is the only supported way to enroll a host. There is no SSH anywhere in it, and no cloud-hosted spokes for now: you install on a machine you control, and the spoke then dials out to the hub. Design: [Manual enrollment and upgrades](../designs/hub-and-spoke-distributed-engine.md); package details: [`spoke-package/README.md`](../../spoke-package/README.md).

**Enrollment establishes identity and an authenticated management stream; it does not enable dispatch.** Hub host registration, launch-path configuration and promotion to `spoke_ready` are owned by the design-breakdown entries **Remote capacity qualification** and **Capacity scheduling and unreachable hosts**. Preflight reports are sent to the hub but are not consumed by its scheduler until those entries land. Do not use this runbook to commission worker capacity.

**Package installation is a one-time step**, and the package installs an update helper that can apply signed releases (see [Upgrades](#upgrades)). The hub does not offer releases yet; that work is owned by **Hub spoke-upgrade rollout**, so a host stays on the release it was installed with until then. Certificate renewal is built into the daemon (see [Certificate lifetime](#certificate-lifetime)).

Two people may be involved: the **hub operator** (runs `bossctl` where the hub's credential state lives) and the **host installer** (has root on the new machine). They can be the same person. The only thing that passes between them is the token, which is single use, bound to one host id and valid for 15 minutes.

## What you need

| Input                                              | Where it comes from                                                                                                                                                                                                   |
| -------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A host id, e.g. `build-box-1`                      | You choose it. Starts with a letter or digit, then letters, digits, `.`, `_`, `-`. It never changes after the first install: the host's journal belongs to it.                                                        |
| The platform's release assets                      | `boss-spoke-<platform>.tar.gz`, `.manifest.json`, `.manifest.json.sig`, `boss-spoke-package-<platform>` and `boss-spoke-package-<platform>.sig` from the spoke release (`macos-arm64`, `linux-amd64`, `linux-arm64`). |
| The release public key and its fingerprint         | The release owner. Confirm the fingerprint with them before trusting the key.                                                                                                                                         |
| The hub's address, e.g. `https://hub.example:7443` | The hub's `BOSS_HUB_LISTEN` address as hosts reach it.                                                                                                                                                                |
| The hub's CA, as a PEM file (`hub-ca.pem`)         | The CA that signed the hub's TLS certificate (`BOSS_HUB_TLS_CERT`). It is what the spoke trusts the hub with; it is not the host-credential CA.                                                                       |
| An enrollment token                                | The hub operator, step 1.                                                                                                                                                                                             |
| Driver and GitHub logins for the service user      | You, step 4.                                                                                                                                                                                                          |

On the hub, the listener must already be configured (`BOSS_HUB_LISTEN`, `BOSS_HUB_CREDENTIALS_DIR`, `BOSS_HUB_TLS_CERT`, `BOSS_HUB_TLS_KEY`; all four or none, see `engine/core/src/hub.rs`). Until they are set the hub accepts no spokes and enrollment cannot succeed.

## 1. Mint a token (hub operator)

On the machine where the hub's credential directory lives:

```sh
export BOSS_HUB_CREDENTIALS_DIR=/path/to/hub-credentials
bossctl enrollment token build-box-1 --token-out ./build-box-1.token
# or, instead of the environment variable:
bossctl enrollment token --credentials-dir /path/to/hub-credentials build-box-1 --token-out ./build-box-1.token
```

This authorizes `build-box-1` (if it is new) and writes a fresh token to a new owner-only file. Without `--token-out` it prints the token once instead. The hub stores only a hash, so a lost token is replaced, not recovered: minting again invalidates the earlier unused token.

Rules the hub enforces, whatever tool you use:

- one token enrolls exactly one host id, once; a replay, another host's token, or a different key is refused;
- the token expires after 15 minutes;
- a revoked host gets no token until you pass `--reauthorize` (see [Removal](#removal-and-revocation)).

Move the file to the host over any channel you already trust (console paste, `scp` from your own shell, a password manager). It is a secret until it is used; delete your copy once the host has enrolled.

## 2. Install the package (host installer, as root)

Everything here runs as root on the new host. Put the five release assets (the table above lists them), `release.pub` and `hub-ca.pem` in a scratch directory, owned by root.

**Verify before running anything.** Check the fingerprint of the release key with the release owner, then verify the installer itself against the signed manifest as described under [Verifying the first-run installer](../../spoke-package/README.md#verifying-the-first-run-installer). Then:

```sh
chmod +x boss-spoke-package-<platform>
./boss-spoke-package-<platform> fingerprint --key release.pub      # must match what the owner told you
./boss-spoke-package-<platform> verify \
  --manifest boss-spoke-<platform>.manifest.json \
  --signature boss-spoke-<platform>.manifest.json.sig \
  --package boss-spoke-<platform>.tar.gz --trusted-key release.pub

./boss-spoke-package-<platform> install \
  --manifest boss-spoke-<platform>.manifest.json \
  --signature boss-spoke-<platform>.manifest.json.sig \
  --package boss-spoke-<platform>.tar.gz \
  --trusted-key release.pub \
  --host-id build-box-1 --hub-url https://hub.example:7443 --hub-ca hub-ca.pem
```

`install` creates the `boss-worker` service user if it is missing, lays out the owner-only state directories, pins the release key (`/etc/boss-spoke/release-verify-key.pub`, root-owned; a different key is refused later), writes the service definition and the update helper, and registers them. It enrolls nothing and starts nothing; it says `service not started: this host is not enrolled`. Running it again with the same inputs changes nothing.

|                           | macOS                                                | Linux                                                                        |
| ------------------------- | ---------------------------------------------------- | ---------------------------------------------------------------------------- |
| Package prefix            | `/usr/local/lib/boss-spoke`                          | `/opt/boss-spoke`                                                            |
| Service user home / state | `/Users/boss-worker`, state in `spoke/`              | `/var/lib/boss-worker`, state in `spoke/`                                    |
| Service definition        | `/Library/LaunchDaemons/dev.boss.spoke.plist`        | `/etc/systemd/system/boss-spoke.service`                                     |
| Update helper             | `/Library/LaunchDaemons/dev.boss.spoke.update.plist` | `/etc/systemd/system/boss-spoke-update.service` and `boss-spoke-update.path` |
| Install record            | `/etc/boss-spoke/install.json`                       | `/etc/boss-spoke/install.json`                                               |

## 3. Give the service user its tools and logins

Workers run as `boss-worker`, in the service's own environment, so that is the account that must be able to do the work. The service `PATH` is, on Linux, `~/.local/bin:/usr/local/bin:/usr/bin:/bin`, and on macOS additionally `/opt/homebrew/bin:/usr/sbin:/sbin`. Install these where that `PATH` finds them: `tmux`, `cube`, `jj`, `git`, `gh`, `python3` (with `fcntl`, `pty` and `termios`, which the brief relay needs), and the driver CLIs `claude` and `codex`.

Then, as the service user, sign in to what the host will use. Open a shell as that user:

```sh
# Linux
sudo -u boss-worker -H bash
# macOS (the account has no login shell, so name one)
sudo -u boss-worker -H /bin/zsh
```

Run each tool's own login there (`gh auth login`, `claude`, `codex login`) and make sure the account can read the repositories the host will work on. These credentials stay on the host and are the service user's alone. Credentials are never part of the enrollment token; the token only gives the host an identity.

A missing tool or login does not block identity enrollment; preflight reports the failure.

## 4. Enroll (host installer, as root)

Use `/opt/boss-spoke/current/bin/boss-spoke-package` on Linux and `/usr/local/lib/boss-spoke/current/bin/boss-spoke-package` on macOS for every installed `enroll` or `preflight` command below; install creates no PATH link. The examples use Linux paths.

```sh
/opt/boss-spoke/current/bin/boss-spoke-package enroll --token-file /root/build-box-1.token \
  --repo https://github.com/spinyfin/mono          # optional: repeat for each repo the host must read
# token from stdin instead of a file:
/opt/boss-spoke/current/bin/boss-spoke-package enroll --token-stdin \
  --repo https://github.com/spinyfin/mono
```

The token comes from an owner-only (`0600`) file or from `--token-stdin`; it is never a command-line argument. In order, `enroll`:

1. confirms the package is installed and the service registered;
2. hands the token to `boss-worker` in an owner-only file inside its credentials directory;
3. **as `boss-worker`**, generates the host's key locally, sends the CSR to the hub's single-use `Enroll` call, and installs `key.pem` then `cert.pem` (the private key never leaves the host);
4. removes the handed-over token file, and your `--token-file` once the token is spent;
5. starts the service through the launchd or systemd definition `install` registered, and waits for it to stay up;
6. runs the **as-user preflight**: the installed checker, as `boss-worker`, in the installed service environment.

A successful run prints the preflight `PASS` lines, `enrolled host … (certificate serial …)` and `service started; as-user preflight passed`. This establishes identity and management connectivity only; it does not register the host for dispatch.

### When it fails

Every failure prints `enrollment failed: <code>` and exits with the status below. There is no fallback to anything but the hub's `Enroll` call.

| Code                     | Exit | Meaning and what to do                                                                                                                                                                                                              |
| ------------------------ | ---- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `not_installed`          | 10   | No install record, service user, registered service or `ca.pem`. Run `install` (step 2) first. Nothing was changed.                                                                                                                 |
| `token_invalid`          | 11   | The text is not a 32-byte token (truncated paste, extra characters). Re-copy it. The token is never echoed.                                                                                                                         |
| `already_enrolled`       | 12   | The host already has `cert.pem` and `key.pem`. To replace the identity (lost or expired), mint a new token and run `/opt/boss-spoke/current/bin/boss-spoke-package enroll --replace-identity --token-file /root/build-box-1.token`. |
| `hub_unreachable`        | 13   | The hub could not be reached or its certificate did not verify against `ca.pem`. The token was **not** spent and your token file is kept: fix the address or CA, then run the same command again.                                   |
| `enrollment_refused`     | 14   | The hub said no: token spent, expired, replaced, for another host, or the host is revoked. Mint a new token (step 1) and run again. Your token file is deleted.                                                                     |
| `credentials_unwritable` | 15   | The credentials directory is not owner-only, not owned by `boss-worker`, or is not a real directory. Fix it as `install` describes; `install` never repairs paths the service user controls.                                        |
| `service_not_running`    | 16   | The host enrolled but the service did not start or stay up. Read the service log (below), fix the cause and restart the service. Do not re-enroll.                                                                                  |
| `preflight_failed`       | 17   | The host enrolled and the service runs, but a check failed. The failing names are printed as `failed_preflights: …`. Enrollment does not enable dispatch; fix it as `boss-worker`, then run the installed `preflight` command.      |
| `enrollment_failed`      | 18   | The as-user step failed without saying why. Its output is printed; check the `boss-worker` state directory permissions and the hub log.                                                                                             |

Preflight checks and what fixes them (the installed `preflight` command prints one `PASS`/`FAIL` line each and is safe to run repeatedly):

| Check                                                                              | Fix                                                                                                                                                                  |
| ---------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `private_storage`, `private_homes`                                                 | State, credential and driver-home directories must be real, owner-only and owned by `boss-worker`; `BOSS_CODEX_HOMES_DIR` must be `<state>/homes`. Re-run `install`. |
| `disk_space`, `clock`                                                              | At least 16 GiB free; a correct clock (a skewed clock also breaks certificates).                                                                                     |
| `tmux`, `tool_cube`, `tool_jj`, `tool_git`, `tool_gh`, `tool_claude`, `tool_codex` | Install the tool where the service `PATH` finds it (step 3).                                                                                                         |
| `python_pty`                                                                       | `python3` on that `PATH` must import `fcntl`, `pty` and `termios`.                                                                                                   |
| `github_credentials`, `claude_credentials`, `codex_credentials`                    | Log in as `boss-worker` (step 3). The checks discard their output because it names accounts.                                                                         |
| `hub_reachable`                                                                    | The host must reach the hub's host and port.                                                                                                                         |
| `repo_access`                                                                      | `boss-worker` must be able to `git ls-remote` each `--repo`. The failure names a count, never a URL.                                                                 |

The daemon repeats these checks every five minutes and reports failures to the hub, using the persisted `--repo` inputs. Passing them does not enable scheduling.

## 5. Verify

On the host:

```sh
/opt/boss-spoke/current/bin/boss-spoke-package preflight # as root; runs the checks as boss-worker
# Linux
systemctl status boss-spoke.service
journalctl -u boss-spoke.service -e
# macOS
launchctl print system/dev.boss.spoke              # state = running, plus last exit status
```

The macOS service definition configures no log file, so on macOS `launchctl print` and the hub are where you look.

On the hub:

```sh
bossctl enrollment list
# build-box-1  enrolled  certificate valid for 29d
```

`pending` means the token has been minted but not used. `enrolled` means the host holds a certificate. This list does not prove an active stream or dispatch readiness. The daemon sends failing check names as `failed_preflights`, but the scheduler does not consume them.

Task execution and PR publication cannot be verified through this enrollment flow until hub host registration and scheduling are implemented.

## Upgrades

The package installs a root-run **update helper** (a systemd path unit on Linux, launchd `WatchPaths` on macOS). When the hub offers a newer signed release, the running daemon asks the helper to apply it. The helper then:

1. fetches the manifest, signature and package **only from the origin pinned at install** and verifies them against the **root-pinned release key**;
2. refuses anything that fails, targets another platform, or would leave the host's journal unreadable;
3. stages the release, switches `current`, and restarts only the daemon, so tmux workers and the journal stay up;
4. requires lock, journal, inventory and a management handshake within two minutes, and otherwise **rolls back to `previous` by itself**.

> **Status: the hub does not offer releases yet.** That work is owned by the design-breakdown entry **Hub spoke-upgrade rollout**. The daemon and helper handle an offer, but no hub code sends one, so a host enrolled with this runbook stays on the release it was installed with. Until that entry lands, move a host to a newer release by running the signed `install` again with the **same `--host-id`**, hub URL and CA, exactly as in [Disaster recovery](#disaster-recovery) steps 2 to 5; that reinstall is not rolled back automatically if it fails, so do it on one host first.

The `boss-worker` user cannot write release binaries, `current`, the pinned key or the service definitions.

### See a host's running release and last upgrade result

On the host (read-only, any user):

```sh
readlink /opt/boss-spoke/current                  # Linux; macOS: /usr/local/lib/boss-spoke/current
readlink /opt/boss-spoke/previous
cat /etc/boss-spoke/install.json                  # release_id, host_id, hub_url, package_origin
cat /etc/boss-spoke/upgrade-status.json           # the last helper attempt
```

`upgrade-status.json` has `release_id` (what was offered), `result`, `error`, and `running_release_id` (what is actually running). `result` is one of:

| `result`                                                                 | Meaning                                                                                                                                                                                                                                                           |
| ------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ok`                                                                     | The offered release is running and healthy.                                                                                                                                                                                                                       |
| `refused`                                                                | The offer was turned away before anything changed; `error` starts with the reason (`manifest_sig`, `digest_mismatch`, `platform_mismatch`, `unapproved_origin`, `journal_incompatible`, `request_malformed`, …).                                                  |
| `failed`                                                                 | The attempt failed before the switch committed and the host kept running what it had.                                                                                                                                                                             |
| `rolled_back`                                                            | The new release failed its health check and the previous one was restored and proven healthy.                                                                                                                                                                     |
| `rollback_failed`, `rollback_refused_journal_migrated`, `restore_failed` | **Unresolved.** The helper could not prove a release healthy. `unresolved_*` fields keep the original outcome visible even after a later offer. Read the service log; if neither `current` nor `previous` can start, see [Disaster recovery](#disaster-recovery). |

The hub operator's view of each host's release and last attempt is a separate piece of the hub's management surface; the files above are the host's own record and are always authoritative for that host.

## Certificate lifetime

A host certificate lasts 30 days and the hub keeps a 24-hour overlap across renewal. The daemon renews on its own when ten days remain, with up to one day of jitter so a fleet enrolled together does not renew together; it publishes the new key and certificate atomically and reconnects the outbound session. `bossctl enrollment list` shows how long each certificate has left. If the identity is lost, expired while the daemon was down, or revoked, mint a new token for the same host id and run `/opt/boss-spoke/current/bin/boss-spoke-package enroll --replace-identity --token-file /root/build-box-1.token` (step 4). The journal is preserved.

## Disaster recovery

Reinstall a signed package by hand **only** when neither the current nor the previous release can start, so the update helper has nothing to run and cannot roll back. A failed upgrade that rolled back is not a disaster: look at `upgrade-status.json` and let the next offer fix it.

1. Confirm: `current` and `previous` both fail to start (service log; run `/opt/boss-spoke/previous/bin/boss-spoke --help`).
2. Fetch a signed release for this platform from the release owner and verify it exactly as in step 2. The pinned key means you do not pass `--trusted-key` again; a different key is refused as `trusted_key_changed`.
3. Run the downloaded `./boss-spoke-package-<platform> install` with the **same `--host-id`**, hub URL and CA. Do not change the host id: the journal belongs to it.
4. The host is already enrolled, so `install` restarts the service itself. A hand `install` of a newer package does **not** roll back automatically if it fails.
5. Run `/opt/boss-spoke/current/bin/boss-spoke-package preflight` (macOS: `/usr/local/lib/boss-spoke/current/bin/boss-spoke-package preflight`). No new token is needed unless the certificate also expired, in which case follow [Certificate lifetime](#certificate-lifetime).

## Removal and revocation

**Revoke on the hub first**, from the hub operator's machine:

```sh
bossctl enrollment revoke build-box-1 --reason "decommissioned"
```

The host's open stream closes within seconds, and it cannot reconnect, renew or enroll again. This ends what the host may report; it **cannot stop a worker already running** on a host that is offline. Stop those on the host.

To bring a revoked host back, the hub operator must open it deliberately: `bossctl enrollment token build-box-1 --reauthorize --token-out …`, then enroll with `--replace-identity`. A revoked host's old certificates stay dead.

**Then remove the host** (as root on it):

```sh
# Linux
systemctl disable --now boss-spoke-update.path boss-spoke.service
rm -f /etc/systemd/system/boss-spoke.service /etc/systemd/system/boss-spoke-update.service /etc/systemd/system/boss-spoke-update.path
systemctl daemon-reload
sudo -u boss-worker tmux -S /var/lib/boss-worker/spoke/tmux.sock kill-server    # workers survive a service stop
rm -rf /opt/boss-spoke /etc/boss-spoke
userdel boss-worker && rm -rf /var/lib/boss-worker        # optional: deletes the journal and credentials too

# macOS
launchctl bootout system/dev.boss.spoke
launchctl bootout system/dev.boss.spoke.update
rm -f /Library/LaunchDaemons/dev.boss.spoke.plist /Library/LaunchDaemons/dev.boss.spoke.update.plist
sudo -u boss-worker /usr/local/bin/tmux -S /Users/boss-worker/spoke/tmux.sock kill-server   # use your tmux path
rm -rf /usr/local/lib/boss-spoke /etc/boss-spoke
dscl . -delete /Users/boss-worker && rm -rf /Users/boss-worker       # optional
```

Keep `/var/lib/boss-worker` (or `/Users/boss-worker`) if you may re-provision the host with the same id: the journal there belongs to that id.

## Not exercised on a real host

The package's real account creation, `systemctl` and `launchctl` calls run only on a real host; the Bazel tests stage them and drive the real binaries against a real hub-side credential authority. The first fresh host installed from this runbook is also its first real-host check. In particular, how `claude` and `codex` keep their logins for a service account with no interactive session (notably on macOS) has not been confirmed here: `preflight` is the authority on whether the host can use them.
