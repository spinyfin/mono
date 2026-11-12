# Antigravity scoped HOME and credential delegation

- **Dates:** initial measurements 2026-11-11; forced-expiry soak 2026-11-12 06:29–07:34 UTC; real-home readbacks and staggered natural-expiry soaks 2026-11-12 17:31–21:47 UTC.
- **Scope:** macOS credential concurrency, authorization prompts, updater concurrency, host-tool delegation, and directory overrides.
- **Host:** macOS 27.0.1 (26A434), arm64; existing signed-in login keychain. The host is shared: the owner's own `agy` sessions and other tools that call `/usr/bin/security` ran throughout.
- **Primary binary:** `agy` 1.5.5, SHA-256 `42b591f206a868dd0b924d5b6b396638a108539367695a5dbe2f7dcfdbf4d6f3`.
- **Update specimen:** vendor-signed 1.5.6, SHA-256 `c55b821d5efcfd7ca7fdadbc870d1247cc0ccd31137291fee83fb0e342dc753a`.
- **Installed binary at re-measurement:** vendor-signed 1.5.7, SHA-256 `e872cc54b5381f401c798fc947937a0a930876b849bc3fee654996ba4ab236a8`, CDHash `c139c23db990e3a8f0d1a8e75418c8386c408d58`, same `cli` identifier and team `EQHXZ8M8AV`. The installed file changed from 1.5.5 at 17:24 UTC that day, the minute one of the owner's own sessions started, which fits the CLI's automatic update.
- **Related:** [driver design](../designs/antigravity-driver-fourth-driver-google-gemini-cli.md), [pane viability study](ghostty-agy-pane-viability.md), [hook surface study](agy-hook-surface.md), [public pane-study PR](https://github.com/spinyfin/mono/pull/3550).

Scoped `HOME` remains the proposed isolation mechanism for the Antigravity driver. This document records how the shared sign-in behaves when many sessions refresh it, through the existing login-keychain symlink bridge and, for comparison, in the real home. It uses the vendor CLI as shipped and introduces no driver, credential helper, keychain policy change, or alternate credential store.

Two kinds of home appear throughout. A **per-run home** is a fresh directory that holds only the bridge symlink, which is what a provisioned worker would get. The **real home** is the owner's ordinary `HOME`, where their own sessions run. Every result below says which one it was measured in.

## Verdict

**The shared sign-in was never lost, and nobody had to sign in again.** An earlier revision of this document said that concurrent refresh destroyed the login-keychain credential and left the host needing a new sign-in. That was wrong. Every readback behind it was taken during one hour in which the keychain item was absent, and the two that ran the CLI ran it in fresh per-run homes. Nothing was run from the real home in that hour, and nothing was read again afterwards. The vendor CLI re-created the item an hour after it disappeared. From the real home, `agy models` exits 0 and the item is present at every check made since, and the owner's own sessions kept refreshing on schedule throughout.

What the measurements support, by condition and kind of home:

| Condition                                 | Homes                     | Sessions sharing one token     | Failed saves | Keychain item                             | Fresh sessions afterwards                                |
| ----------------------------------------- | ------------------------- | ------------------------------ | ------------ | ----------------------------------------- | -------------------------------------------------------- |
| A: forced, sixteen refreshes inside 41 ms | Per-run                   | 16                             | 14           | Deleted; absent for 60 minutes            | Per-run: 0/16 during the absence. Real home: not tried   |
| B: staggered, natural expiry              | Per-run, two arms at once | 32, plus one real-home session | 2            | Deleted twice; back after 5.0 s and 0.3 s | Per-run: 16/16. Real home: `agy models` exit 0           |
| B: staggered, natural expiry              | Real home                 | 16, plus one real-home session | 0            | Never deleted                             | Per-run: 16/16. Real home: 16/16 and `agy models` exit 0 |
| B: staggered, natural expiry              | Per-run, alone            | 16                             | 0            | Never deleted                             | Per-run: 16/16. Real home: 16/16 and `agy models` exit 0 |

In every row all the soaking sessions finished their work and exited 0, and no authorization UI was detected. The "one real-home session" is the owner's own, which happened to hold the same token.

- **A save fails when two sessions refresh within a few tens of milliseconds of each other.** The failure is the keychain's duplicate-item error from Apple's `security` tool, which the vendor CLI runs. Refreshes 7 ms and 43 ms apart produced a failure; refreshes 93 ms or more apart never did. Nothing ties it to the bridge: the bridge resolves to the same keychain file, and a real-home session saved in the same minute as thirty-two per-run sessions.
- **After a failed save the vendor CLI deletes the keychain item.** The item stays absent until some session saves again. With staggered sessions that took seconds. After the forced burst nothing saved again for an hour.
- **What a per-run home changes is where the fallback copy goes.** A session whose save fails writes its token to a file under its own home. In a per-run home nothing else ever reads that file. In the real home the same file would sit where every real-home session looks. That real-home path is read from the binary, not observed: no save failed in the real home.
- **A real-home failed save was not observed, so the two kinds of home are compared only where both succeeded.** Sixteen staggered sessions in the real home had no two refreshes closer than 93 ms, and the owner's own session logs show 30 refreshes with no failed save.

**For provisioning.** The bridge is not disqualified by credential loss, because there was none. Staggered sessions through the bridge signed in, refreshed, finished, raised no UI, and left a credential that fresh sessions could use. One exposure is open before a fleet is enabled. The soaking sessions refreshed on a once-a-minute schedule fixed by their start time, so sessions started at the same moment are expected to refresh at the same moment. The forced soak shows what follows in per-run homes: the item is absent until some session saves again, and a fresh per-run session cannot sign in meanwhile. Whether that happens without a forced pause, and what the real home does in the same case, has not been measured. Nothing here calls for a Boss credential accessor, an external lock, or delegating the fallback files.

Concurrent self-update also races, and neither proposed directory override replaces scoped `HOME` in 1.5.5.

## Method and boundaries

The apparatus runs in a scratch directory that is never committed: an ignored directory inside the workspace for the November 11 work and the forced-expiry soak, and a temporary directory outside it for the staggered soaks. Each session has its own working directory, and each per-run session its own home. In a per-run home the only credential delegation is:

```text
<run-home>/Library/Keychains/login.keychain-db
  -> <host-home>/Library/Keychains/login.keychain-db
```

This reproduces `LOGIN_KEYCHAIN_LEAF`, `require_delegate`, and `replace_with_symlink` in [Grok environment provisioning](../../engine/driver/src/grok/environment.rs). It does not copy the keychain or extract a token. No Boss app or engine is launched. The installed host binary is not updated; all update experiments use disposable executable copies.

The completed experiments recorded on November 11 include an interactive override matrix, host-tool configuration comparisons, manual ACL inspection, and updater races. Those observations are identified separately below. The earlier interactive soak had no recorded completion result and contributes no acceptance evidence.

The November 12 forced-expiry soak (condition A below) uses sixteen real **noninteractive** CLI processes with model `gemini-3.8-flash-low`, `-p`, and stdin connected to `/dev/null`. Each prompt requests exactly `/bin/sleep 3900`, waits for its completion, and then requests `SOAK_COMPLETE`. Each scoped `settings.json` trusts its scratch working directory and permits only that shell command. The CLI remains alive while the tool runs in the background. No command is typed into an interactive session. These are neither GhosttyKit panes nor a Boss dispatch; [the pane study](ghostty-agy-pane-viability.md#method--apparatus) supplies the separate pane-topology evidence.

The staggered soaks (condition B) keep that session shape with three differences. Sessions start about twenty seconds apart on a seeded schedule (20 s ± 6 s, sixteen sessions over about five minutes) instead of together. They are never paused. Their permission and trust settings are passed with `--settings`, which the CLI documents as "for this session only, never saved", and each log goes to a scratch path with `--log-file`. Each session runs one permitted `/bin/sleep 2100` and then answers `SOAK_COMPLETE`, which keeps it alive across the stored token's natural expiry. A one-session pilot with a fifteen-second tool passed in each kind of home before the arms started.

Four staggered arms ran. Two per-run arms, one on the installed 1.5.7 binary and one on a scratch copy of 1.5.5, ran at the same time and crossed the same expiry. A real-home control on 1.5.7 then ran alone, and a third per-run arm on 1.5.7 ran alone after it on the same schedule. The per-run arms use fresh homes that contain only the keychain bridge. The real-home control uses the ordinary `HOME`: the study edits nothing in the owner's settings, trust list or log symlink, although the CLI writes its usual conversation and cache state there.

All credential and override probes export `AGY_CLI_DISABLE_AUTO_UPDATE=true` and `AGY_CLI_HIDE_ACCOUNT_INFO=true`. Updater races deliberately omit the update-disable variable. The earlier interactive override matrix also pre-seeded the pane study's onboarding file; that does not establish account-level data-collection consent, which remains the pane study's separate open question.

Evidence consists of process exit statuses, CLI logs, native conversation transcripts, file inventories, manual ACL metadata inspection, code-signature inspection, system log entries for the `security` tool, and a continuous authorization-UI monitor. No password is requested, saved, or supplied; keychain locking and ACLs are not changed; dialogs are neither dismissed nor suppressed. The apparatus does not extract credential values. The vendor CLI created fallback token files in per-run homes when a save failed: fourteen in the forced soak and two in the staggered ones. Only their paths, modes, sizes and modification times were recorded. They were never opened, copied or linked, and all were deleted from the scratch homes after their sessions exited.

The real-home facts come from three manual sources. `agy models` and a `security find-generic-password -s gemini -a antigravity` lookup were run from the real `HOME`; that lookup prints attributes only, and `-g` and `-w` were not used. The token and keyring lines of the owner's own CLI session logs under the real home were read. Persisted system log entries for the `security` process were read with `log show`, and a `log stream` for the same process ran during the staggered soaks. Those entries carry API names, error codes and file paths. They carry no item name, no secret and no parent process, and the tool also runs for other callers on this host (about 2,000 runs during one forty-minute arm, the study's own sessions included), so attribution to particular sessions is by timestamp against the CLI logs. Unrelated callers also create and delete keychain items of their own; a handful of such pairs appear outside the refresh minutes, and the credential's creation date did not move with them.

## What the credential ACL actually trusts

The November 11 manual `security dump-keychain -a <login-keychain>` inspection recorded one relevant generic-password item. The following is its non-secret metadata and selected ACL content; no `-d`, `-g`, or `-w` secret dump was used. This is manual diagnostic evidence, not a query performed by the soak harness or a Boss binary:

```text
class: "genp"
"acct"<blob>="antigravity"
"svce"<blob>="gemini"
"cdat": 20260731213328Z
"mdat": 20261111190935Z
access: 5 entries
  decrypt derive export_clear export_wrapped mac sign
  don't-require-password
  applications (1):
    /usr/bin/security (OK)
    requirement: identifier "com.apple.security" and anchor apple
  partition_id: apple-tool:
  change_acl: applications (0)
```

**The trusted application is `/usr/bin/security`, not `agy`.** There is no `agy` path or Google signing requirement in this item's ACL. A November 12 manual metadata inspection after the binary replacement and sixteen new-version sessions found the same decrypt application, Apple signing requirement, `apple-tool:` partition, and empty change-ACL application list. Its modification timestamp was `20261112062714Z`; the method did not retrieve the credential value.

The 1.5.5 binary contains `google3/third_party/golang/github_com/zalando/go_keyring/v/v0/keyring.macOSXKeychain.Get`/`.Set` symbols and `find-generic-password`. Upstream [go-keyring's macOS implementation](https://github.com/zalando/go-keyring/blob/85eb737c1258e4849bfba6456dc0c48228ca8242/keyring_darwin.go) invokes Apple's tool to read and update generic-password items. This supports the observed access identity, but is not a claim that Google's vendored source is byte-for-byte identical to upstream.

This distinction matters after updates. The vendor CLI can change path or code hash while continuing to invoke the same Apple-signed accessor. Boss must not become another accessor, directly or through a `security` subprocess. A stable vendor signature alone would not establish access to an arbitrary item whose ACL does not trust the actual accessor.

The item inspected above no longer exists. The forced refresh removed it for an hour, and the vendor CLI has re-created it more than once since (see [the credential sections](#credential-concurrency-two-conditions)). The ACL of a re-created item was not dumped again, so there is no post-refresh ACL comparison. What was observed is behaviour: every fresh session started after a re-creation read the credential through the same accessor, and the authorization monitor recorded no UI.

Apple documents that an [ACL entry associates operations with trusted applications](https://developer.apple.com/documentation/security/access-control-lists); a caller outside that list can trigger a confirmation prompt. [Designated requirements identify code across updates](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements). The reported development-build prompt problem is therefore a reason to keep Boss binaries out of this path, not a reason to add ACL grants, sign the helpers differently, or broker secrets through them. This study does not independently attribute the owner's earlier dialogs to a particular historical call stack.

Manual `security show-keychain-info <login-keychain>` returned `no-timeout`. That was the pre-existing host setting, not a change made by the study. An unlocked keychain and an accepted application ACL are separate conditions; these observations do not establish behavior after a manual lock or a sleep-induced lock.

### Binary identities and update probe

`codesign -dv --verbose=4` and `codesign -dr -` report:

| Property               | 1.5.5                                      | 1.5.6                                      |
| ---------------------- | ------------------------------------------ | ------------------------------------------ |
| Identifier             | `cli`                                      | `cli`                                      |
| Signing authority      | Google LLC                                 | Google LLC                                 |
| Team identifier        | `EQHXZ8M8AV`                               | `EQHXZ8M8AV`                               |
| CDHash                 | `e51eb328e78a547f03c70080c7ddf2b24e793d78` | `d474db8491e8dff7f5bc64af7b491254d43c7c6f` |
| Designated requirement | Google Developer ID requirement for `cli`  | Same requirement                           |

A disposable 1.5.5 copy's `agy update` downloaded, verified, and installed 1.5.6, exiting 0. The November 12 repeat took 5.97 seconds and produced the recorded 1.5.6 hash; `codesign --verify --strict` exited 0. An atomic replacement then installed that copy at the scratch executable path while all sixteen original 1.5.5 processes remained alive. Sixteen fresh homes launched the replacement through the same keychain bridge: **16/16 returned `POST_UPDATE_OK`, exit 0, in 6.16 seconds for the batch**, during the continuous UI monitor. The earlier November 11 batch separately recorded 16/16 in 6.75 seconds. The primary qualification remains 1.5.5; these are update-transition observations, not a full qualification of 1.5.6.

## Credential concurrency: two conditions

The study ran two conditions. They answer different questions and their results differ, so they are reported separately.

- **Condition A, forced synchronized expiry.** Sixteen sessions in per-run homes are paused across their token's expiry and resumed together. All sixteen refresh and write the keychain within tens of milliseconds of each other.
- **Condition B, staggered natural expiry.** Sessions start about twenty seconds apart, are never paused, and refresh when their own next request finds the token expired. It ran in per-run homes and, as a control, in the real home.

### What the real home shows

These readbacks were taken from the real `HOME`, outside any per-run home, on November 12.

| Time (UTC) | Readback from the real home              | Result                                                                |
| ---------- | ---------------------------------------- | --------------------------------------------------------------------- |
| 17:31      | Item attributes                          | Present; created 08:27:14, modified 17:03:45                          |
| 17:38      | `agy models`                             | Exit 0, 21 models, signed in from the keychain                        |
| 18:04      | Item attributes                          | Present; created 18:04:27, modified 18:04:33                          |
| 18:05      | `agy models`                             | Exit 0, 21 models, signed in from the keychain                        |
| 20:05      | `agy models`                             | Exit 0, 21 models, signed in from the keychain                        |
| 20:06      | Item attributes                          | Present; created 18:04:27, modified 19:35:13                          |
| 20:36      | `agy models`                             | Exit 0, 21 models, signed in from the keychain                        |
| 20:36      | Item attributes                          | Present; created 18:04:27, modified 20:36:00                          |
| 20:36      | Sixteen fresh real-home sessions at once | 16/16 exit 0 with the expected reply; all signed in from the keychain |
| 20:46      | `agy models`                             | Exit 0, 21 models, signed in from the keychain                        |
| 21:36      | `agy models`                             | Exit 0, 21 models, signed in from the keychain                        |
| 21:36      | Item attributes                          | Present; created 18:04:27, modified 21:36:49                          |
| 21:36      | Sixteen fresh real-home sessions at once | 16/16 exit 0 with the expected reply; all signed in from the keychain |
| 21:47      | `agy models`                             | Exit 0, 21 models, signed in from the keychain                        |
| 21:47      | Item attributes                          | Present; created 18:04:27, modified 21:36:49                          |

No fallback token file existed in the real home at the 18:04, 20:05, 20:36, 20:46, 21:36 or 21:47 checks.

The item's creation date is the trace of what happened to it. It was `20260731213328Z` before the forced soak, `20261112082714Z` after the item came back that morning, and `20261112180427Z` after the first staggered soak. Each change is a delete followed by a fresh create. The date did not move during the real-home soak or the per-run soak that followed it.

The owner's own session logs in the real home agree. One session ran from 09:20 UTC on November 11 to 17:24 UTC on November 12 and refreshed 26 times. Its successor started at 17:24 UTC and had refreshed 4 times by 21:45 UTC. Neither log contains a failed save, a skipped save, or a request to sign in.

### Condition A: controlled sequence

1. Start the UI monitor before spawning the sixteen sessions.
2. Start sixteen 1.5.5 headless sessions with different homes and the same keychain bridge. Verify that each actually starts its long-running tool; process startup alone is insufficient.
3. Keep all sixteen alive across their initially loaded token's expiry. All sixteen logged expiry `2026-11-12 01:27:12.946134 -0600`.
4. After at least 31 minutes, pause only these sixteen recorded CLI PIDs; resume them about ten seconds after that expiry. This forces their loaded credentials to age together without editing a token, changing the system clock, or invoking a keychain API. The paused interval is excluded from active soak duration. Their sleep subprocesses continue during the pause.
5. Let the tool complete and require a subsequent native model response from every session. Inspect refresh messages, persistence/skipping messages, transcript responses, and eventual exit statuses separately.
6. Exercise the distinct signed update described above during the monitored interval.
7. Let every headless session exit, collect its real status, and report the monitored interval and remaining ACL limitations explicitly.

### Automated prompt detection

The monitor records `pgrep -x 'SecurityAgent|SecurityUIAgent'` approximately once per second, the number of the sixteen known child processes still alive, and whether its log-stream process is alive. It also continuously runs:

```sh
/usr/bin/log stream --style ndjson --level info \
  --predicate 'process == "SecurityAgent" OR process == "SecurityUIAgent"'
```

The process-name observation detects authorization UI; it is not used to infer ownership of unrelated commands. A positive process observation stops the study's child processes and fails the zero-prompt criterion; a dead log stream also fails the measurement. Authorization log entries are checked separately. Polling and available system logs are bounded evidence, not a universal proof that every possible macOS UI surface is observable. Condition B used the same monitor.

### Condition A results

The monitored interval ran from **2026-11-12 06:29:30 UTC to 07:34:39 UTC**. All sixteen sessions were alive and unpaused for at least **2,293.99 seconds (38.23 minutes)**, allowing conservatively for the largest polling gap. The 1,611.47-second forced-expiry pause is excluded. The harness exited 0 only after collecting all child exit statuses and stopping its log monitor.

| Observation                                            | Result                                                                                                           |
| ------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------- |
| Original sessions                                      | 16/16 returned `SOAK_COMPLETE` and exited 0                                                                      |
| Forced refresh                                         | 16/16 logged `token refreshed`; completion timestamps span 41.083 ms                                             |
| Keychain write-back                                    | 14/16 logged `Failed keyring SaveToken ... exit status 45`                                                       |
| Vendor fallback                                        | 14 per-home `antigravity-oauth-token` regular files, mode `0600`, 558–559 bytes                                  |
| Shared keychain item                                   | Deleted at 07:27:23 UTC; re-created at 08:27:14 UTC by a real-home session that was already running              |
| Fresh sessions in fresh per-run homes, 07:28–07:29 UTC | 0/16 loaded a credential; all exited 1 after the CLI's own 60-second authentication timeout; batch 62.52 seconds |
| Manual metadata query of the keychain file, 07:28 UTC  | 0 matching credential items; command completed with pipe failure propagation enabled, exit 0                     |
| `agy models` in a fresh per-run home, 07:35:41 UTC     | Exited 1: `Please sign in to view available models`                                                              |
| Any readback from the real home during the absence     | Not taken                                                                                                        |
| Any readback after the item returned                   | Not taken until the re-measurement, about nine hours later                                                       |
| Authorization-process samples                          | 3,725; zero positive observations; largest gap 2.17 seconds                                                      |
| Authorization log stream                               | Alive throughout; only its filter header, no matching events                                                     |
| Binary replacement during monitoring                   | Signed 1.5.6 replacement; 16/16 additional fresh sessions succeeded before the refresh phase                     |

The original sessions kept working because each held its refreshed token in memory. Their successful final model responses therefore say nothing about what a newly started session would find. Fresh homes expose what continued sessions hide.

The fourteen fallback files correspond exactly to the fourteen write-back failures; sessions 00 and 07 had neither. Their contents were not inspected, so this study does not assert their encoding or encryption. All fourteen generated files were removed from the scratch homes after every original session exited. They were never copied, linked, committed, or used to restore the host credential. No restoration was attempted, and none turned out to be needed.

The preliminary authentication pilot returned `AUTH_READY`, exit 0, and refreshed without detected authorization UI. A longevity pilot exited 0 but reported an auto-denied command; it is excluded from successful-session counts. Adding the precise sleep permission produced `LONG_READY` after the tool completed.

### Where the condition A readbacks were taken

Every readback that reported a missing credential was taken inside the hour in which the item was absent, and the two that ran the CLI ran it in fresh per-run homes.

- The sixteen fresh sessions used sixteen new directories that held only the bridge symlink.
- The final `agy models` reused the first of those directories. Its log records that directory, not the real home, as the CLI app data directory.
- The manual metadata query opened the keychain file by path. It described the real database correctly as it stood at 07:28 UTC, five minutes after the item was deleted.

No command was run from the real `HOME` during that hour, and no readback of any kind was repeated afterwards. Those readbacks were accurate for their moment. They do not show that the credential was destroyed or that anyone had to sign in again, and neither happened.

### How the item came back

A 1.5.5 session in the real home had been running since 09:20 UTC on November 11. Its log shows a refresh roughly every hour, including one at 07:27:08 UTC, fourteen seconds before the forced burst, and one at 08:27:13.997 UTC. The system log for that second shows a lookup that found nothing, then `SecKeychainItemCreateFromContent` at 08:27:14.015 and a committed write at 08:27:14.029. When the item's attributes were next read, at 17:31 UTC, its creation date was `20261112082714Z`; before the burst it had been `20260731213328Z`.

The vendor CLI therefore re-created the item from a credential it still held in memory. The absence lasted from 07:27:23 to 08:27:14 UTC. A session that was already running was unaffected. A session started in that window finds no keychain item, which is what the per-run readbacks recorded; whether a fresh real-home session would also have failed was not measured, because none was started.

### Condition B: staggered natural expiry

No session was paused and no token was aged. Each arm started sixteen sessions over five minutes, timed so that the token they all loaded expired while every session was running.

#### Per-run homes: two arms at once

Two per-run arms ran at the same time and crossed the same expiry, so thirty-two per-run sessions shared one token. One arm used the installed 1.5.7 binary and one a scratch copy of 1.5.5. A real-home session of the owner's, started at 17:24 UTC, held the same token.

| Observation                                   | 1.5.7 arm                                    | 1.5.5 arm                                    |
| --------------------------------------------- | -------------------------------------------- | -------------------------------------------- |
| Sessions started (UTC)                        | 17:41:29–17:46:27                            | 17:45:09–17:50:06                            |
| Loaded token expiry (UTC)                     | 18:03:44                                     | 18:03:44                                     |
| Sessions returning `SOAK_COMPLETE`, exit 0    | 16/16                                        | 16/16                                        |
| Sessions that refreshed                       | 16/16, 2.6–47.8 s after expiry               | 16/16, 4.2–49.0 s after expiry               |
| `Failed keyring SaveToken ... exit status 45` | 1 (session 12)                               | 1 (session 00)                               |
| Skip messages                                 | 0                                            | 0                                            |
| Fallback token files                          | 1, in that session's home, `0600`, 559 bytes | 1, in that session's home, `0600`, 559 bytes |
| Bridge links still symlinks afterwards        | 16/16                                        | 16/16                                        |
| Authorization-process samples                 | 2,389; zero positive; largest gap 1.04 s     | 2,390; zero positive; largest gap 1.04 s     |
| Authorization log stream                      | Alive throughout; filter header only         | Alive throughout; filter header only         |

The thirty-two refreshes spanned 46.4 seconds. Thirty saved. Each of the two failures led to the keychain item being deleted, as described under [what a session does after a failed save](#what-a-session-does-after-a-failed-save). The next session to refresh created it again, 4.96 seconds after the first delete and 0.33 seconds after the second. The owner's real-home session refreshed at 18:04:31, inside the same minute, and saved without a failure.

Readbacks, taken while all thirty-two sessions were still running:

| Time (UTC) | Readback                                                        | Result                                                                                      |
| ---------- | --------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| 18:04:47   | Item attributes, from the real home                             | Present; created 18:04:27, modified 18:04:33                                                |
| 18:04:47   | Fallback token file in the real home                            | None                                                                                        |
| 18:05:02   | Sixteen fresh sessions in fresh per-run homes, launched at once | 16/16 exit 0 with the expected reply; all signed in from the keychain; 6.19 s for the batch |
| 18:05:08   | `agy models` from the real home                                 | Exit 0, 21 models, signed in from the keychain                                              |

A fresh session started inside one of the two short gaps would have found no keychain item. None was started then, so that is not measured.

The two fallback files stayed in their scratch homes, unread, until 21:47 UTC and were then deleted without being opened or copied.

#### Real home: sixteen sessions

The control ran sixteen sessions in the real home on the same seeded schedule, alone. The owner's own real-home session, still running, held the same token.

| Observation                                | Result                                                           |
| ------------------------------------------ | ---------------------------------------------------------------- |
| Sessions started (UTC)                     | 20:06:24–20:11:22, binary 1.5.7                                  |
| CLI app data directory                     | The real home's `.gemini/antigravity-cli`, in all sixteen logs   |
| Loaded token expiry (UTC)                  | 20:35:12.59                                                      |
| Sessions returning `SOAK_COMPLETE`, exit 0 | 16/16                                                            |
| Sessions that refreshed                    | 16/16, from 9.5 s before expiry to 48.3 s after it               |
| `Failed keyring SaveToken`                 | 0                                                                |
| Skip messages                              | 0                                                                |
| Fallback token file in the real home       | None, at 20:36 and again at 20:46 after every session had exited |
| `security` writes during the refreshes     | 17, all committed; no duplicate-item error and no delete         |
| Keychain item                              | Present throughout; creation date unchanged at 18:04:27          |
| Closest pair of refreshes                  | 93 ms apart; both saved                                          |
| Authorization-process samples              | 2,395 over 20:06:22–20:46:26; zero positive; largest gap 1.03 s  |
| Authorization log stream                   | Alive throughout; filter header only                             |

The seventeenth write was the owner's own session, which refreshed at 20:35:13 and saved.

Readbacks:

| Time (UTC) | Readback                                                        | Result                                                                |
| ---------- | --------------------------------------------------------------- | --------------------------------------------------------------------- |
| 20:36:13   | `agy models` from the real home                                 | Exit 0, 21 models, signed in from the keychain                        |
| 20:36:13   | Item attributes, from the real home                             | Present; created 18:04:27, modified 20:36:00                          |
| 20:36:14   | Sixteen fresh sessions in fresh per-run homes, launched at once | 16/16 exit 0 with the expected reply; all signed in from the keychain |
| 20:36:19   | Sixteen fresh sessions in the real home, launched at once       | 16/16 exit 0 with the expected reply; all signed in from the keychain |
| 20:46:44   | `agy models` from the real home, after every session had exited | Exit 0, 21 models, signed in from the keychain                        |

This is the behaviour the owner describes from daily use: many concurrent real-home sessions, a refresh each, and no trouble. It does not show what the real home does after a failed save, because no save failed.

#### Per-run homes: sixteen sessions alone

A third per-run arm repeated the control's schedule in per-run homes, alone, across the next expiry. It started 28 minutes 49 seconds before that expiry; the real-home control had started 28 minutes 48 seconds before its own.

| Observation                                | Result                                                          |
| ------------------------------------------ | --------------------------------------------------------------- |
| Sessions started (UTC)                     | 21:07:10–21:12:07, binary 1.5.7                                 |
| Loaded token expiry (UTC)                  | 21:35:59.91                                                     |
| Sessions returning `SOAK_COMPLETE`, exit 0 | 16/16                                                           |
| Sessions that refreshed                    | 16/16, from 8.4 s before expiry to 49.6 s after it              |
| `Failed keyring SaveToken`                 | 0                                                               |
| Skip messages                              | 0                                                               |
| Fallback token files                       | None, in any per-run home or in the real home                   |
| `security` writes during the refreshes     | 16, all committed; no duplicate-item error and no delete        |
| Keychain item                              | Present throughout; creation date unchanged at 18:04:27         |
| Closest pair of refreshes                  | 399 ms apart; both saved                                        |
| Bridge links still symlinks afterwards     | 16/16                                                           |
| Authorization-process samples              | 2,392 over 21:07:08–21:47:11; zero positive; largest gap 1.06 s |
| Authorization log stream                   | Alive throughout; filter header only                            |

Readbacks:

| Time (UTC) | Readback                                                        | Result                                                                |
| ---------- | --------------------------------------------------------------- | --------------------------------------------------------------------- |
| 21:36:50   | `agy models` from the real home                                 | Exit 0, 21 models, signed in from the keychain                        |
| 21:36:50   | Item attributes, from the real home                             | Present; created 18:04:27, modified 21:36:49                          |
| 21:36:52   | Sixteen fresh sessions in fresh per-run homes, launched at once | 16/16 exit 0 with the expected reply; all signed in from the keychain |
| 21:36:57   | Sixteen fresh sessions in the real home, launched at once       | 16/16 exit 0 with the expected reply; all signed in from the keychain |
| 21:47:22   | `agy models` from the real home, after every session had exited | Exit 0, 21 models, signed in from the keychain                        |

With sixteen staggered sessions and no two refreshes close together, the per-run bridge and the real home gave the same result. The two failures in the first staggered soak came with thirty-two sessions, two pairs of which refreshed within 50 ms.

### When refreshes collide

The CLI logs fit one rule in all sixty-four staggered sessions. A soaking session makes a request once a minute, on a schedule fixed by the moment it started, and refreshes at the first of those requests that falls later than about ten seconds before its token expires. Every refresh came 0.06 to 0.78 seconds after a whole number of minutes from the session's first log line. The earliest was 9.5 seconds before expiry and the latest 49.6 seconds after it. Sessions that share a token therefore refresh at separate moments spread over about a minute, unless they started at the same point in a minute. The owner's interactive sessions are less regular: 21 of their 30 refreshes came within 62 seconds of the token's expiry, and the other 9 between 13 minutes and 2 hours 43 minutes after it. Whether a worker session in a pane follows the soaking sessions' schedule was not measured.

The saves that failed were the ones that landed together:

| Soak                         | Gap between a refresh and the one before it | Save result           |
| ---------------------------- | ------------------------------------------- | --------------------- |
| A, forced                    | All sixteen inside 41.083 ms                | 14 of 16 failed       |
| B, per-run, two arms at once | 7 ms                                        | One of the two failed |
| B, per-run, two arms at once | 43 ms                                       | One of the two failed |
| B, per-run, two arms at once | 101 ms, 130 ms and every larger gap         | Saved                 |
| B, real home                 | 93 ms, 236 ms and every larger gap          | Saved                 |
| B, per-run, alone            | 399 ms and every larger gap                 | Saved                 |

The sixteen condition A sessions had been started together: their first log lines fall within 0.72 seconds. The pause then released sixteen overdue requests at once and compressed that spread to 41 ms. By the rule above they would have refreshed within about 0.7 seconds of each other even without the pause, and several pairs among them started less than 50 ms apart. That is an inference from the timing rule. Sessions started at the same moment were not measured across a natural expiry.

The binary contains a saving token source, stored-token update methods, writer identity support, and these diagnostic branches:

```text
Skipping refreshed token save because stored token changed externally
Skipping refreshed token save because another process is the active token writer
Skipping refreshed token save because token was removed from storage externally
Skipping refreshed token save because stored token could not be verified: %v
```

These strings establish the presence of defensive branches, not that this consumer-login path uses them. **None of those skip messages appeared in any soak log, in either condition or either kind of home.** Every session refreshed for itself and wrote the result. In condition A fourteen recorded a storage failure (local time, UTC−6):

```text
01:27:22.472355 browser.go:265] token refreshed, new expiry=...02:27:21.472325...
01:27:22.513438 browser.go:265] token refreshed, new expiry=...02:27:21.513422...
01:27:22.876818 composite_token_storage.go:481] Failed keyring SaveToken, falling back to file when applicable: exit status 45
01:27:22.981774 composite_token_storage.go:481] Failed keyring SaveToken, falling back to file when applicable: exit status 45
```

The first two lines are the earliest and latest refresh completions; the last two bound the fourteen reported save failures. This is per-process refresh activity, not one shared refresh result. There is no evidence of a cross-process lock that serializes the whole read/refresh/write transaction.

### The underlying keychain error

The CLI log keeps only `exit status 45`. The system log for the `security` tool identifies it: the persisted log for condition A, and a live stream captured during condition B. Every failed write, in both conditions, logged the same sequence:

```text
SecKeychainItemModifyContent
CSSM Exception: -2147413733 CSSMERR_DL_RECORD_NOT_FOUND
CSSM Exception: -2147413719 CSSMERR_DL_INVALID_UNIQUE_INDEX_DATA
integrity: possible duplicate, trying to delete invalid items
integrity: duplicate item exception is real; throwing it on
SecKeychainItemCreateFromContent
CSSM Exception: -2147413719 CSSMERR_DL_INVALID_UNIQUE_INDEX_DATA
integrity: duplicate item exception is real; throwing it on
```

`security error -25299` reads "The specified item already exists in the keychain", and the tool exits with the low byte of that status, which is 45. Another process replaced the record between this process's lookup and its modify. The modify then could not find its record, and the fallback add collided with the record the other writer had just committed. This is Apple's own update path losing a race between processes. It is not an ACL failure and it raises no authorization UI.

Counting `security` processes by what they called:

| Soak and stage (UTC)                            | Writes | Committed | Failed with duplicate item | Deletes |
| ----------------------------------------------- | ------ | --------- | -------------------------- | ------- |
| A, first write per session, 07:27:22.498–.960   | 16     | 2         | 14                         |         |
| A, second write, 07:27:22.986–23.400            | 14     | 5         | 9                          |         |
| A, delete, 07:27:23.107–.415                    |        |           |                            | 8       |
| B, per-run, two arms at once, 18:03:47–18:04:33 | 35     | 33        | 2                          | 2       |
| B, real home, 20:35:03–20:36:01                 | 17     | 17        | 0                          | 0       |
| B, per-run, alone, 21:35:51–21:36:50            | 16     | 16        | 0                          | 0       |

The 35 writes in the per-run row are the first save of each of the thirty-two sessions, one follow-up write from each of the two failing sessions, and one save from a real-home session of the owner's that refreshed at 18:04:31.

### What a session does after a failed save

The duplicate-item error does not remove the item. What follows it does. In the staggered soak the same steps follow each of the two failures, at the same intervals:

| Step                                              | Staggered failure 1 (UTC) | Staggered failure 2 (UTC) |
| ------------------------------------------------- | ------------------------- | ------------------------- |
| Write fails with duplicate item                   | 18:04:10.575              | 18:04:26.322              |
| Fallback token file appears in the session's home | 18:04:10.699              | 18:04:26.631              |
| One more write, which commits                     | 18:04:10.753              | 18:04:26.679              |
| `SecKeychainItemDelete`, which commits            | 18:04:10.853              | 18:04:26.764              |
| Next session to refresh creates the item again    | 18:04:15.815              | 18:04:27.094              |

Condition A shows the same steps in bulk: the fourteen failing sessions produced fourteen second writes and eight deletes inside one second. No session refreshed after them, so nothing put the item back until the real-home session's next refresh an hour later.

The CLI binary contains `fallbackSaveTokenToFile`, `removeKeyringQuietly` and `healKeyringTokenFromFile` in its composite token storage, in 1.5.5, 1.5.6 and 1.5.7 alike. Those names fit a design in which a session that cannot save to the keychain keeps its token in a file under its own home, removes the keychain copy, and can restore the keychain from that file later. The symbol names and the timing support that reading. The vendor source was not inspected, and the system log does not name the parent of each `security` process, so attributing the follow-up write and the delete to the failing session rests on the same steps following each failure at the same intervals.

### Per-run bridge and real home compared

The same in both kinds of home:

- **The keychain write.** Every commit in the system log lands on the real keychain path. The bridge symlink is resolved before the write, the links are still links afterwards, and per-run homes and the real home share one database and one lock. A real-home session and thirty-two per-run sessions wrote the same item inside the same minute.
- **Who refreshes.** Each session refreshes for itself and saves the result. Sixteen sessions sharing the real home's state directory did sixteen refreshes and sixteen saves, exactly as sixteen sessions in sixteen per-run homes did on the same schedule. No session in either kind of home logged that it skipped a save because another had written.
- **No authorization UI**, in any soak.

Different, or not known:

- **Where the fallback copy goes.** The file is written under the session's own home. In a per-run home nothing else ever reads it, so once the keychain copy is removed the only remaining copies are in memory and in private directories. In the real home the same file would sit in the directory that every real-home session shares. That difference was not exercised: no real-home session failed a save in this study, so the real-home fallback path is inferred from the binary, not observed.
- **Whether a real-home save can fail.** Nothing measured here protects the real home. Its sessions use the same tool on the same file and each writes for itself. No failure was seen there, and no two real-home refreshes came closer than 93 ms.
- **What a fresh real-home session finds while the item is absent.** Not measured. The only fresh sessions started during an absence were in per-run homes.

Do not transplant Grok's `auth.json.lock` protocol, add an external lock around credential contents, relax ACLs, or turn the fallback files into delegates.

## Concurrent fresh homes race to update the shared binary

**Confirmed on 1.5.5.** Each home owns a separate `.gemini/antigravity-cli/updater/update.lock`, but each updater stages files beside the same executable. Those per-home locks do not protect the shared staging names.

| Experiment                                                   | Real command results               | Updater result                                       | Final executable    |
| ------------------------------------------------------------ | ---------------------------------- | ---------------------------------------------------- | ------------------- |
| 16 unauthenticated fresh homes, `agy models`                 | 16 exit 1, sign-in required        | 16 background processes; all reported update failure | Original 1.5.5 hash |
| 16 authenticated fresh homes, `agy -p`                       | 16 exit 0 and `UPDATE_RACE_OK`     | 16 background processes; 1 success, 15 failures      | Valid 1.5.6 hash    |
| 16 foreground `agy update` invocations, separate fresh homes | 1 exit 0, 15 exit 1; 24.46 seconds | 9 extraction failures, 6 installation failures       | Valid 1.5.6 hash    |
| Single foreground `agy update` control                       | Exit 0                             | Download, verification, and installation succeeded   | Valid 1.5.6 hash    |

The four rows above are the November 11 observations. An independent November 12 repeat of the sixteen foreground commands took **31.91 seconds: 0 successes, 16 exit-1 extraction failures**, leaving the original 1.5.5 hash. Fifteen errors explicitly named a missing shared staging file; the single-updater control succeeded in 5.97 seconds. Concurrency can therefore leave every updater unsuccessful, not merely produce one winner and noisy losers.

The foreground reproduction exposes the failures hidden behind the background updater's generic status:

```text
auto_updater.go:573] Failed to extract binary from archive:
  open <shared-bin>/.new.tar.gz: no such file or directory
Error: failed to extract files

auto_updater.go:594] Swap Phase 2 Failure:
  rename <shared-bin>/.new <shared-bin>/agy: no such file or directory
Error: failed to install update
```

This is a concrete shared-file race. The November 11 updated binary matched the successful control; the November 12 race instead retained the original checksum. Neither final state proves uninterrupted availability during swaps or safety for every interleaving. Successful model turns also do not mean their background update succeeded.

Provisioning must retain the already-established `AGY_CLI_DISABLE_AUTO_UPDATE=true` setting, and binary updates must happen outside concurrent worker startup. Do not share mutable CLI homes just to share the updater lock. This study does not implement an update manager or alter the installed host CLI.

## Directory override choice

**Choose neither override as a substitute for scoped `HOME` on 1.5.5.** This is the investigated choice; the broader scoped-home design is unchanged.

On November 11, five cells ran once in headless mode and once as interactive PTY sessions. The interactive cells placed different `Stop` canaries in the default global customization root and each candidate override root. All ten sessions exited 0; the five headless sessions returned `OVERRIDE_OK`, and only the default-root hook fired in every interactive cell. The November 12 repeat independently ran all five headless cells: 5/5 returned `OVERRIDE_OK`, exit 0, created the default CLI state root, and created none of the candidate override directories. The interactive hook observations below remain the earlier measurements.

| Environment                                               | CLI mutable state               | Global hook selected              |
| --------------------------------------------------------- | ------------------------------- | --------------------------------- |
| Neither variable                                          | `$HOME/.gemini/antigravity-cli` | `$HOME/.gemini/config/hooks.json` |
| `ANTIGRAVITY_EXECUTABLE_DATA_DIR=<absolute scratch path>` | Same                            | Default root only                 |
| `ANTIGRAVITY_CONFIG_REL_DIR=.agy-study-config`            | Same                            | Default root only                 |
| `ANTIGRAVITY_CONFIG_REL_DIR=<absolute scratch path>`      | Same                            | Default root only                 |
| Both variables, relative config value                     | Same                            | Default root only                 |

Inventories include conversation databases, installation state, presence files, caches, built-in assets, and `brain/<conversation>/.system_generated/logs/transcript_full.jsonl`. The headless override destinations were not created by the CLI. Interactive runs used pre-created canary directories, so hook selection, not directory existence, was the distinguishing observation.

Google's [sidecar documentation](https://antigravity.google/docs/sidecars#runtime-data) describes `ANTIGRAVITY_EXECUTABLE_DATA_DIR` as the persistent-data directory passed to a sidecar. That agrees with the binary's embedded guide and explains why treating it as the CLI's global data-root override was misplaced. The presence of `ANTIGRAVITY_CONFIG_REL_DIR` in the binary does not make it an effective CLI isolation contract; its purpose outside the measured CLI entry points remains unestablished.

## Host-tool delegation: the complete Grok inventory

Retain the shared host-tool environment contract. Preserving environment variables removes Grok's particular reason for redundant filesystem delegates; it does not make a relocated home find the host's tool configuration by itself. Reuse shared provisioning rather than creating a second collection of path resolvers.

| Facility                 | Existing native environment and filesystem delegation    | Evidence and need                                                                                                                                                                                                                                                                                               |
| ------------------------ | -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `gh` configuration       | `GH_CONFIG_DIR`; `.config/gh` link                       | `gh config get git_protocol --host github.com` changed under an empty home; explicit delegation restored the host result. This reads configuration, not a keychain token.                                                                                                                                       |
| Cube registry            | `CUBE_DATA_DIR`; `.local/share/cube` link                | [Cube path resolution](../../../cube/src/paths.rs) otherwise selects the new home's registry. Do not create a separate worker registry.                                                                                                                                                                         |
| Cube configuration       | `CUBE_CONFIG_DIR`; `.config/cube` link                   | [Cube configuration resolution](../../../cube/src/config.rs) uses the scoped home when no override is supplied.                                                                                                                                                                                                 |
| `jj` identity            | `JJ_CONFIG`; platform config-file link                   | `jj config get user.name` was empty under a fresh home; delegation restored the host value. Respect the existing resolver's legacy-file precedence.                                                                                                                                                             |
| Git global configuration | `GIT_CONFIG_GLOBAL`; `.gitconfig` link                   | The pane study's actual tool child lost global Git identity. Preserve the existing resolved host file. No additional raw Git invocation was used here.                                                                                                                                                          |
| SSH                      | `.ssh` link; retain the inherited agent socket           | `ssh -G -v github.com` still read the host's `.ssh/config` under a scoped `HOME` and produced identical configuration. Native OpenSSH therefore did not need the link for this lookup; the shared link still serves tools that resolve `$HOME/.ssh`. No network connection or private-key operation was tested. |
| Login keychain           | `Library/Keychains/login.keychain-db` link               | Load-bearing for the vendor CLI's authentication. Only filesystem provisioning belongs in Boss.                                                                                                                                                                                                                 |
| XDG cache root           | `XDG_CACHE_HOME`                                         | Preserve the host root for tools that honor it. The current helper does not link the entire `.cache` directory.                                                                                                                                                                                                 |
| repobin cache            | `REPOBIN_CACHE_DIR`; `.cache/repobin` link               | [repobin's resolver](../../../repobin/src/cache.rs) otherwise falls back through XDG to the new home. Avoid duplicating tool downloads/checkouts.                                                                                                                                                               |
| Bazelisk cache           | `BAZELISK_HOME`; `Library/Caches/bazelisk` link on macOS | Preserve the existing shared Bazel distribution cache.                                                                                                                                                                                                                                                          |
| Bazel output root        | `TEST_TMPDIR`; `Library/Caches/bazel` link on macOS      | Preserve the existing output/server-root contract in `GrokProcessEnvironment`; this study does not start sixteen Bazel servers.                                                                                                                                                                                 |

The November 11 read-only host/scoped/delegated `gh` and `jj` matrix had six successful command exits; successful exit alone was insufficient because the scoped outputs differed. Configuration checks do not establish remote GitHub/SSH authorization, and no tool credential probe was added to Boss.

The repository also specifies `build --disk_cache=~/.cache/bazelcache` in [`.bazelrc`](../../../../.bazelrc). Neither the existing `.cache/repobin` link nor `XDG_CACHE_HOME` alone proves that this separate tilde path stays shared. The [hook study's real build](agy-hook-surface.md#real-builds-non-idle-stop-and-automatic-continuation) failed twice when repository-cache defaults landed under the scoped home inside the checkout, then failed when `BOSS_WORKER_TOOL_PATH` was absent. Its successful run restored the ordinary host home **only for the Bazel subprocess**, preserving 1,608 action-cache hits and 53 disk-cache hits. Provisioning must preserve `BOSS_WORKER_TOOL_PATH` and verify repository cache, disk cache, and output/server roots against the host's normal locations; the existing Grok delegate list alone is insufficient evidence. No cache was relocated, disabled, cleared, or bypassed in this validation.

## Findings carried forward without re-measurement

The updated pane study already settles these points for 1.5.5:

- Tool children retain the parent environment; Grok's environment-stripping behavior does not transfer to `agy`.
- `AGY_CLI_DISABLE_AUTO_UPDATE=true` disables automatic updates; `=1` does not.
- 1.5.5 does not create the relocated `ms-playwright-go` cache seen in 1.1.12. There is no corresponding per-run cache-install cost to budget for this version. This is not a claim that the entire scoped home is free to initialize.
- `oauth_creds.json` and `google_accounts.json` are not the credential delegation mechanism for this CLI. Do not create or link them as part of its provisioning.

See [Q8](ghostty-agy-pane-viability.md#q8--does-scoped-home-hold-up-in-a-pane) and the pane study's [scoped-home evidence](ghostty-agy-pane-viability-artifacts/evidence/11-scoped-home.txt). These findings were used as inputs, not repeated as new experiments.

## Static boundary and implementation consequences

Inspection of the existing bridge finds only source existence checks, parent-directory creation, destination symlink metadata, removal of an old destination symlink, and symlink creation. It refuses to replace a regular destination file. The bridge does not inspect keychain contents or invoke a keychain API.

The driver-source scan for `SecItem`, `Security.framework`, `/usr/bin/security`, and `Command::new("security")` found no such call in the Grok provisioning path. There is no new Antigravity driver code in this change, so this is an audit of the reusable precedent, not an assertion that a future implementation has passed its own static gate. Unit tests use dummy files, not live keychains.

When the driver is implemented, it must extract the existing bridge into shared provisioning used by both drivers, preserving its behavior. It must not add credential item names, ACL edits, refresh-token manipulation, direct Security-framework calls, or a `security` subprocess to any Boss engine, app, CLI, hook, launcher, helper, or test. The item names above are manual diagnostic evidence, not implementation constants. The open exposure described in the verdict is not permission to introduce another keychain path.

Follow-up work remains separate from this document:

1. Before a fleet is enabled, measure sessions started at the same moment across a natural expiry, in per-run homes and in the real home, with fresh-session readbacks from both kinds of home during any absence. This study reached that case only by forcing it, and only in per-run homes.
2. Reuse shared symlink and host-tool provisioning; keep Antigravity settings, hooks, databases, transcripts, and presence state private to the run.
3. Preserve `AGY_CLI_DISABLE_AUTO_UPDATE=true`, qualify binary upgrades outside concurrent startup, and keep the ineffective directory overrides out of the 1.5.5 isolation contract.
4. Add static regression coverage for the Boss/vendor credential-access boundary when the driver exists, without touching a real keychain from tests.

## Repository validation

The document is the only tracked change, and no Bazel target takes it as an input. On the tree that carries it, the target set CI uses was built and tested with caches enabled: `bazel build -- //... -//tools/boss/app-macos/... -//tools/boss/installer/...` exited 0 for 389 targets, and `bazel test` over the same set exited 0 with **189 of 189 test targets passing**. `//tools/boss/engine/driver:driver_test`, which covers the existing bridge, reported 585 passed and none failed or ignored. The source scan independently confirmed that the reusable bridge performs filesystem operations only. These checks validate the repository and the existing precedent. They say nothing about the live credential measurements above.

## Open questions and limits

- Sessions started at the same moment were not measured across a natural expiry. The timing rule predicts they refresh together; the forced soak is the only evidence of what follows, and it used a pause.
- No save failed in the real home, so what the vendor CLI does there after a failure is not observed: whether the fallback file appears in the shared directory, whether the keychain item is deleted, and whether a later session restores it.
- No fresh session was started from the real home while the item was absent. Whether it would have signed in is unknown.
- The vendor steps after a failed save are read from the system log's timing and the binary's symbol names. The vendor source was not inspected.
- The ACL of a re-created item was not dumped. Fresh sessions read re-created items without any authorization UI.
- Each staggered arm crossed one expiry. A session that lives for several hours crosses several. If its once-a-minute schedule holds from hour to hour, a pair that collides once would collide again; that was not measured.
- This host's pre-existing keychain configuration has no timeout. Locked-keychain and sleep/wake behavior are not inferred from an awake, unlocked soak; no locking policy was changed to obtain a result. The forced pause was not compared with real sleep.
- The host is shared. The owner's sessions and unrelated callers of the `security` tool ran during every soak, and other work loaded the machine.
- The updater race leaves a valid executable in the measured final states, but uninterrupted availability during every rename interleaving was not measured.
- The directory override conclusion is specific to the 1.5.5 CLI entry points tested. It says nothing about another product's use of the same variable names.
- No implementation or rollout ships with this document. The measurements constrain subsequent provisioning work; they do not authorize credential workarounds.
