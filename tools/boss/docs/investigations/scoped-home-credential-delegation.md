# Antigravity scoped HOME and credential delegation

- **Dates:** initial measurements 2026-11-11; independent validation 2026-11-12.
- **Scope:** macOS credential concurrency, authorization prompts, updater concurrency, host-tool delegation, and directory overrides.
- **Host:** macOS 27.0.1 (26A434), arm64; existing signed-in login keychain.
- **Primary binary:** `agy` 1.5.5, SHA-256 `42b591f206a868dd0b924d5b6b396638a108539367695a5dbe2f7dcfdbf4d6f3`.
- **Update specimen:** vendor-signed 1.5.6, SHA-256 `c55b821d5efcfd7ca7fdadbc870d1247cc0ccd31137291fee83fb0e342dc753a`.
- **Related:** [driver design](../designs/antigravity-driver-fourth-driver-google-gemini-cli.md), [pane viability study](ghostty-agy-pane-viability.md), [hook surface study](agy-hook-surface.md), [public pane-study PR](https://github.com/spinyfin/mono/pull/3550).

Scoped `HOME` remains the proposed isolation mechanism for the Antigravity driver, but its shared credential path failed the concurrent-refresh study. This document records that failure using the existing login-keychain symlink bridge and vendor CLI; it introduces no driver, credential helper, keychain policy change, or alternate credential store.

## Verdict

**Not qualified for provisioning: no authorization UI was detected, but concurrent refresh lost durable shared authentication.** Sixteen original sessions survived the soak and refreshed successfully; fourteen failed to save back to the keychain and created per-home token fallback files. The credential item was subsequently absent, and all sixteen fresh sessions failed authentication. A final `agy models` also exited 1 requesting sign-in.

Do not treat successful existing sessions or the zero-prompt observation as a credential acceptance pass. Keep provisioning and fleet enablement blocked until this failure is resolved through the unchanged shared bridge and vendor credential path; do not add a Boss credential accessor or delegate the fallback files. Concurrent self-update also races, and neither proposed directory override replaces scoped `HOME` in 1.5.5.

## Method and boundaries

The apparatus runs in an ignored scratch directory inside the workspace. Each session has a different home and working directory. The only credential delegation is:

```text
<run-home>/Library/Keychains/login.keychain-db
  -> <host-home>/Library/Keychains/login.keychain-db
```

This reproduces `LOGIN_KEYCHAIN_LEAF`, `require_delegate`, and `replace_with_symlink` in [Grok environment provisioning](../../engine/driver/src/grok/environment.rs). It does not copy the keychain or extract a token. No Boss app or engine is launched. The installed host binary is not updated; all update experiments use disposable executable copies.

The completed experiments recorded on November 11 include an interactive override matrix, host-tool configuration comparisons, manual ACL inspection, and updater races. Those observations are identified separately below. The earlier interactive soak had no recorded completion result and contributes no acceptance evidence.

The November 12 soak uses sixteen real **noninteractive** CLI processes with model `gemini-3.8-flash-low`, `-p`, and stdin connected to `/dev/null`. Each prompt requests exactly `/bin/sleep 3900`, waits for its completion, and then requests `SOAK_COMPLETE`. Each scoped `settings.json` trusts its scratch working directory and permits only that shell command. The CLI remains alive while the tool runs in the background. No command is typed into an interactive session. These are neither GhosttyKit panes nor a Boss dispatch; [the pane study](ghostty-agy-pane-viability.md#method--apparatus) supplies the separate pane-topology evidence.

All credential and override probes export `AGY_CLI_DISABLE_AUTO_UPDATE=true` and `AGY_CLI_HIDE_ACCOUNT_INFO=true`. Updater races deliberately omit the update-disable variable. The earlier interactive override matrix also pre-seeded the pane study's onboarding file; that does not establish account-level data-collection consent, which remains the pane study's separate open question.

Evidence consists of process exit statuses, CLI logs, native conversation transcripts, file inventories, manual ACL metadata inspection, code-signature inspection, and a continuous authorization-UI monitor. No password is requested, saved, or supplied; keychain locking and ACLs are not changed; dialogs are neither dismissed nor suppressed. The apparatus does not extract credential values. The vendor unexpectedly created fourteen fallback files during refresh; only their paths, modes, and sizes were recorded, and the files were removed after the sessions exited.

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

After the forced refresh, another manual metadata query completed with **zero matching items**. There was therefore no surviving item whose post-refresh ACL could be compared. The updated binary's successful initial access does not establish safe refresh persistence.

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

## Credential concurrency and zero-prompt soak

### Controlled sequence

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

The process-name observation detects authorization UI; it is not used to infer ownership of unrelated commands. A positive process observation stops the study's child processes and fails the zero-prompt criterion; a dead log stream also fails the measurement. Authorization log entries are checked separately. Polling and available system logs are bounded evidence, not a universal proof that every possible macOS UI surface is observable.

### Results

The monitored interval ran from **2026-11-12 06:29:30 UTC to 07:34:39 UTC**. All sixteen sessions were alive and unpaused for at least **2,293.99 seconds (38.23 minutes)**, allowing conservatively for the largest polling gap. The 1,611.47-second forced-expiry pause is excluded. The harness exited 0 only after collecting all child exit statuses and stopping its log monitor.

| Observation                          | Result                                                                                                           |
| ------------------------------------ | ---------------------------------------------------------------------------------------------------------------- |
| Original sessions                    | 16/16 returned `SOAK_COMPLETE` and exited 0                                                                      |
| Forced refresh                       | 16/16 logged `token refreshed`; completion timestamps span 41.083 ms                                             |
| Keychain write-back                  | 14/16 logged `Failed keyring SaveToken ... exit status 45`                                                       |
| Vendor fallback                      | 14 per-home `antigravity-oauth-token` regular files, mode `0600`, 558–559 bytes                                  |
| Fresh-session readback               | 0/16 loaded a credential; all exited 1 after the CLI's own 60-second authentication timeout; batch 62.52 seconds |
| Post-refresh manual metadata query   | 0 matching credential items; command completed with pipe failure propagation enabled, exit 0                     |
| Final vendor catalog probe           | `agy models` exited 1: `Please sign in to view available models`                                                 |
| Authorization-process samples        | 3,725; zero positive observations; largest gap 2.17 seconds                                                      |
| Authorization log stream             | Alive throughout; only its filter header, no matching events                                                     |
| Binary replacement during monitoring | Signed 1.5.6 replacement; 16/16 additional fresh sessions succeeded before the refresh phase                     |

The original sessions retained usable in-memory authentication even after shared persistence failed. Their successful final model responses are therefore insufficient acceptance evidence. Fresh homes expose the failure that continued sessions hide.

The fourteen fallback files correspond exactly to the fourteen write-back failures; sessions 00 and 07 had neither. Their contents were not inspected, so this study does not assert their encoding or encryption. All fourteen generated files were removed from the scratch homes after every original session exited. They were never copied, linked, committed, or used to restore the host credential. Further authenticated reproduction requires normal vendor sign-in; no restoration workaround was attempted.

The preliminary authentication pilot returned `AUTH_READY`, exit 0, and refreshed without detected authorization UI. A longevity pilot exited 0 but reported an auto-denied command; it is excluded from successful-session counts. Adding the precise sleep permission produced `LONG_READY` after the tool completed.

### Refresh mechanism: evidence versus inference

The binary contains a saving token source, stored-token update methods, writer identity support, and these diagnostic branches:

```text
Skipping refreshed token save because stored token changed externally
Skipping refreshed token save because another process is the active token writer
Skipping refreshed token save because token was removed from storage externally
Skipping refreshed token save because stored token could not be verified: %v
```

These strings establish the presence of defensive branches, not that this consumer-login path uses them successfully. **None of those skip messages appeared in the sixteen soak logs.** Instead, each process refreshed and fourteen recorded a storage failure:

```text
01:27:22.472355 browser.go:265] token refreshed, new expiry=...02:27:21.472325...
01:27:22.513438 browser.go:265] token refreshed, new expiry=...02:27:21.513422...
01:27:22.876818 composite_token_storage.go:481] Failed keyring SaveToken, falling back to file when applicable: exit status 45
01:27:22.981774 composite_token_storage.go:481] Failed keyring SaveToken, falling back to file when applicable: exit status 45
```

The first two lines are the earliest and latest refresh completions; the last two bound the fourteen reported save failures. This is per-process refresh activity, not one shared refresh result. There is no evidence here of a cross-process lock that safely serializes the whole read/refresh/write transaction. The final absent item and failed fresh reads disprove durable shared-authentication success for this run; they do not show that the entire keychain database was corrupted.

Manual `security error -25299` describes a duplicate-item error, whose low eight bits are 45. That is consistent with the observed subprocess status, but the CLI did not retain the underlying `security` error text, so the numeric coincidence is not a definitive diagnosis. A manual `securityd` log query over 01:27:20–01:29:20 for the service name, deletion, or prompt messages returned zero entries. The exact writer/deletion interleaving and lock implementation remain unproven. The shared host was not an exclusive credential fixture, so timing alone cannot rule out another vendor client; the fourteen failures and fallback files are directly attributable to these recorded sessions.

Do not transplant Grok's `auth.json.lock` protocol, add an external lock around credential contents, relax ACLs, or turn the fallback files into delegates. The failure needs a vendor-path resolution and a new successful concurrency/readback measurement before provisioning can proceed.

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

If credential qualification is restored, implementation must extract the existing bridge into shared provisioning used by both drivers, preserving its behavior. It must not add credential item names, ACL edits, refresh-token manipulation, direct Security-framework calls, or a `security` subprocess to any Boss engine, app, CLI, hook, launcher, helper, or test. The item names above are manual diagnostic evidence, not implementation constants. The present persistence failure keeps that implementation gated; it is not permission to introduce another keychain path.

Follow-up implementation remains separate from this document:

1. Resolve the vendor credential write-back/fallback failure and repeat the monitored soak plus fresh-session readback using the unchanged bridge before enabling provisioning.
2. Once qualified, reuse shared symlink and host-tool provisioning; keep Antigravity settings, hooks, databases, transcripts, and presence state private to the run.
3. Preserve `AGY_CLI_DISABLE_AUTO_UPDATE=true`, qualify binary upgrades outside concurrent startup, and keep the ineffective directory overrides out of the 1.5.5 isolation contract.
4. Add static regression coverage for the Boss/vendor credential-access boundary when the driver exists, without touching a real keychain from tests.

## Repository validation

The document is the only tracked change. In the validation workspace, `bazel build //tools/boss/engine/driver:driver` exited 0 (615 action-cache hits, one disk-cache hit), and `bazel test //tools/boss/engine/driver:driver_test` exited 0 using its cached sandboxed result: **584 passed, zero failed or ignored**. The source scan independently confirmed that the reusable bridge performs filesystem operations only. These checks validate the repository and existing precedent; they do not turn the failed live credential study into a pass.

After incorporating the newer upstream worker gateway change, `bazel test //tools/boss/engine/... //tools/boss/cli/... //tools/boss/protocol/... //tools/boss/protocol-spoke/... //tools/boss/spoke-package/...` exited 0: **127 test targets passed**, 124 cached and three executed. The invocation initially reported an unavailable cached `md-5` rlib, then continued to successful completion with caches enabled. That dependency-cache availability diagnostic is unrelated to this Markdown-only change; no cache override or check bypass was used.

## Open questions and limits

- Why do concurrent vendor writes fail and leave no shared credential, and what vendor change resolves this without another credential path? The recorded CLI failures establish the rejection; exact deletion attribution remains open.
- The host credential was absent at the final probe. Normal vendor sign-in is required before further live authentication experiments; the scratch fallback files are not a recovery mechanism.
- Post-refresh ACL comparison was impossible because the item was absent. The pre-refresh signed-update comparison preserved the observed Apple-tool accessor identity.
- This host's pre-existing keychain configuration has no timeout. Locked-keychain and sleep/wake behavior are not inferred from an awake, unlocked soak; no locking policy was changed to obtain a pass.
- The updater race leaves a valid executable in the measured final states, but uninterrupted availability during every rename interleaving was not measured.
- The directory override conclusion is specific to the 1.5.5 CLI entry points tested. It says nothing about another product's use of the same variable names.
- No implementation or rollout ships with this document. The measurements constrain subsequent provisioning work; they do not authorize credential workarounds.
