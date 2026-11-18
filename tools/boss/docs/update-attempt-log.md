# Update attempt log: following one self-update end to end

Every self-update (check → download → verify → stage → swap → relaunch → first-launch
handshake → reconcile, or → rollback) leaves a durable, structured trail. This page says
where that trail lives, what one record looks like, and how to read one attempt across the
three processes involved: the old app, the detached relaunch helper, and the new app.

## Where the records live

All paths are under the Boss data directory, `~/Library/Application Support/Boss/`; `U` below is its `Updates/` subdirectory:

```sh
U="$HOME/Library/Application Support/Boss/Updates"
```

| Surface                         | Location                                                                                                                                                            | Written by                                                                                   |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| JSONL file (primary)            | `$U/log/update-log.jsonl`, rotated by size to `update-log.1.jsonl` … `update-log.5.jsonl` (1 MiB each, oldest dropped)                                              | `UpdateCore.UpdateLog` (app) and `relaunch-helper.sh` (helper, via `printf >>`)              |
| Unified log, app                | subsystem `dev.spinyfin.bossmacapp`, category `update-lifecycle`, level `notice`/`error`                                                                            | `UpdateLog.live`; at each launch it also replays the helper's new records (`replayed: true`) |
| Unified log, helper (real time) | process `logger`; each message is the helper's JSON line, so it contains `"actor":"helper"`                                                                         | `relaunch-helper.sh` via `/usr/bin/logger` (`logger` cannot set a subsystem)                 |
| Rollback reason                 | `$U/install-state.json` → `rollbacks[]` (last 20) and `blocklist[]`                                                                                                 | `UpdateInstaller.reconcileAfterLaunch`                                                       |
| Helper → app hand-off           | `$U/rolled-back`: the failed version on one line (the format every app version reads); `$U/rolled-back.detail`: `key=value` reason, detail, attempt, from, restored | `relaunch-helper.sh`; consumed on the next launch                                            |
| Rollback-detection state        | `$U/last-seen-blocklist.json` (blocklist snapshot from the previous launch); `$U/log/helper-mirror-cursor` (replay position)                                        | `UpdateInstaller` / `UpdateLog`                                                              |

One file holds on the order of 5,000 records, so the rotation window covers many dozens of complete attempts.

The unified-log mirror deliberately uses `notice`, not `info`: `info` is memory-only by default and vanishes from `log show`, which is why the original incident had "no updater lines" even though the app had logged at `info`. The older `updater` category still carries the verbose `info`/`debug` lines. Use `/usr/bin/log` explicitly: in zsh, `log` is a shell builtin.

```sh
# Everything the app logged about updates, persisted (includes replayed helper records):
/usr/bin/log show --last 2h --predicate 'subsystem == "dev.spinyfin.bossmacapp" AND category == "update-lifecycle"'
# The helper's records as it wrote them, even if no app with this change has launched since:
/usr/bin/log show --last 2h --predicate 'process == "logger" AND eventMessage CONTAINS "\"actor\":\"helper\""'
```

## One record

One JSON object per line. Keys are flat and stable; `detail` is a string→string map.

```json
{
  "actor": "helper",
  "attempt": "upd-1760209681123-3f9a",
  "detail": {
    "flag": "…/Updates/launch-ok-1.0.736.flag",
    "flagVersion": "1.0.736",
    "launchTimeoutSeconds": "15",
    "watchdogSeconds": "30"
  },
  "event": "watch-started",
  "from": "1.0.734",
  "level": "info",
  "pid": 48122,
  "stage": "handshake",
  "to": "1.0.736",
  "ts": "2026-10-11T19:08:01Z"
}
```

| Key           | Meaning                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| ------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ts`          | UTC ISO-8601 (the app adds milliseconds; the helper does not)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `attempt`     | The per-update attempt id, `upd-<unix-ms>-<4 hex>`. Minted by `UpdateModel` at the update decision (the check that found the release; re-checks of the same release reuse it, a Retry mints a new one), carried into staging and that version's `manifest.json`, the swap plan and `install-state.json`'s pending record, passed to the helper as `--attempt`, written into `rolled-back.detail`, and adopted by the new/restored app's startup records. When a helper is armed without `--attempt` (by 1.0.734) it mints one itself. `null` for records outside any attempt (a routine "up to date" check, an ordinary launch). |
| `from` / `to` | The version being replaced / the attempt's target. `to` stays the target even in records written by the restored previous version after a rollback; the version actually running is `detail.runningVersion`.                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `actor`       | `app` or `helper`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `pid`         | The writing process (old app, helper, or new app — they differ, which is itself useful)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `stage`       | `check`, `download`, `verify`, `extract`, `stage`, `swap`, `relaunch`, `handshake`, `rollback`, `reconcile`, `blocklist`, `engine`, `startup`, `quit`                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `event`       | Short event within the stage (see below)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `level`       | `info`, `warning`, `error`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |

## Following one attempt

```sh
U="$HOME/Library/Application Support/Boss/Updates"
all() { cat "$U"/log/update-log.{5,4,3,2,1}.jsonl "$U/log/update-log.jsonl" 2>/dev/null; }
# The most recent attempts, oldest first:
all | jq -r 'select(.attempt) | .attempt' | uniq | tail -5
# One attempt across all three processes, in order:
A=upd-1760209681123-3f9a
all | jq -r --arg a "$A" 'select(.attempt == $a) | [.ts, .actor, .pid, .stage, .event, (.detail|tostring)] | @tsv'
# Every startup and quit step of every launch (attempt or not), newest last:
all | jq -r 'select(.stage == "startup" or .stage == "quit" or .stage == "engine") | [.ts, .pid, .stage, .event, .detail.runningVersion, (.attempt // "-")] | @tsv' | tail -40
# What a rollback left for the next launch, and what reconcile recorded:
cat "$U/rolled-back" "$U/rolled-back.detail" 2>/dev/null
jq '{blocklist, last: .rollbacks[-1]}' "$U/install-state.json"
```

Pick the attempt id from the rollback notice, from `install-state.json` (`rollbacks[].attemptID`), from `rolled-back.detail`, or from the list above.

### Every launch and every quit

Independently of updates, every launch writes `startup.*` records from the earliest point of the process until the first-launch flag, and every confirmed quit writes `quit.*` records. They carry the launch's attempt when an update produced it, else `attempt: null`, and always `detail.runningVersion` and `pid`:

`startup.process-started` (`AppDelegate.init`: pid, executable path, version — before any engine work) → `startup.will-finish-launching` → `startup.did-finish-launching` → `startup.app-nap-assertion-acquired` → `startup.reconcile-begin` → (`relaunch.new-app-launched` when an update produced this launch) → `handshake.flag-written` → `startup.engine-handoff-begin` → `engine.decision` lines → `startup.engine-handoff-complete` → `startup.engine-connected`.

A version that hangs or dies during launch therefore shows exactly which of these steps it reached. The quit path is `quit.should-terminate` → (`quit.agent-confirmation-wait` → `quit.agent-confirmation-returned` when agents were running) → `quit.reply` (`terminate-now` / `terminate-cancel`) → `quit.will-terminate-entry` → (swap / helper arming) → `quit.will-terminate-exit`. Comparing `quit.will-terminate-exit` with the helper's `relaunch.old-process-exited` (`waitedSeconds`) shows how long the process took to actually exit after AppKit's last callback. The app does not stop the shared engine on quit; `[engine stop] …` lines appear as `engine.decision` only when `EngineProcessController.stop()` runs.

### A healthy attempt

| Who     | stage.event                                                                                                                                  | What to look at                                                                                                                                                         |
| ------- | -------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| old app | `check.available`                                                                                                                            | `url`, `bytes`, `mode`, `blocked` (should be `false`)                                                                                                                   |
| old app | `stage.begin` → `download.start` → `download.complete`                                                                                       | `url`, `bytes`, `durationMs` (`stage.already-staged` instead when an earlier process staged it; `requestedAttempt` links the two ids)                                   |
| old app | `verify.size-ok`, `verify.sha256`, `extract.complete`, `verify.codesign-ok`, `verify.team-id-ok`, `stage.quarantine-stripped`, `stage.ready` | digest, Team IDs, `dir`, `totalDurationMs`                                                                                                                              |
| old app | `swap.requested` → `swap.begin` → `swap.moved-current-to-backup` → `swap.moved-staged-to-install` → `swap.committed`                         | each filesystem op's `result`; `installedVersion` read back from the swapped bundle's `Info.plist`                                                                      |
| old app | `relaunch.parked-for-confirmed-quit` → `quit.*` → `relaunch.quit-confirmed` → `relaunch.helper-armed`                                        | `helperPid`, `oldAppPid`, full `arguments`                                                                                                                              |
| helper  | `relaunch.helper-started` → (`relaunch.old-process-still-running` every 300 s) → `relaunch.old-process-exited`                               | `waitedSeconds` — how long the old app took to exit; `derivedArguments` (`none`, or which of `log`/`attempt`/`from` it had to derive)                                   |
| helper  | `relaunch.bundle-assessment` (+ `relaunch.preexisting-process`)                                                                              | `installedVersion` vs `expectedVersion`, `executableState`, `quarantine`, `codesign`, `otherRunningPid` (excluded from discovery, never terminated)                     |
| helper  | `handshake.watch-started` → `relaunch.new-process-started` / `relaunch.open`                                                                 | the flag path and **which version's flag** is polled; `open`'s `exit`/`output`; the new `pid`, its `command` (path actually executed) and `version`, `secondsAfterOpen` |
| new app | `startup.*` → `relaunch.new-app-launched` → `handshake.flag-written` → `reconcile.installed`                                                 | `pid`, `bundle`, `executable`, `fullVersion`; backup removed                                                                                                            |
| helper  | `handshake.flag-observed` → `relaunch.backup-removed` → `relaunch.committed`                                                                 | `secondsAfterProcessStart`                                                                                                                                              |
| new app | `engine.decision`, `startup.engine-*`                                                                                                        | the `[engine version-check …]` / `[engine upgrade]` / `[engine launch]` line: did the new app attach to or replace the running engine                                   |

`relaunch.open` can come after `relaunch.new-process-started`: `open` runs in the background and the helper polls the process table and the flag while it runs, so a slow or hung `open` never delays a launch or handshake decision. A still-running `open` is killed at the decision point and its result recorded then.

### A rolled-back attempt

| Who          | stage.event                                                                                                                                 | Meaning                                                                                                                                                                                                                      |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| helper       | `relaunch.open` with `exit` ≠ 0 → `rollback.decision reason=open-failed`                                                                    | LaunchServices refused; `output` holds its error                                                                                                                                                                             |
| helper       | no `new-process-started` within `launchTimeoutSeconds` → `relaunch.process-scan` → `rollback.decision reason=launch-not-started`            | no process running the installed executable appeared (a hung `open` is killed at this deadline)                                                                                                                              |
| helper       | `handshake.process-exited` → `rollback.decision reason=process-exited`                                                                      | the new process died before writing the flag; `termination` is `exitCode=N` or `signal=N` (from the bundled `exit-observer`'s kqueue `NOTE_EXITSTATUS` watch), or `observerUnavailable=…` with the reason it could not watch |
| helper       | `handshake.timeout` → `rollback.decision reason=handshake-timeout` → `rollback.terminated-new-process`                                      | alive but never wrote the flag within `watchdogSeconds` of its process appearing; the helper terminates it before restoring                                                                                                  |
| helper       | `rollback.removed-failed-bundle`, `rollback.restored-backup`, `rollback.marker-written`, `rollback.reopened-previous`, `rollback.completed` | each op's `result`, `restoredVersion`                                                                                                                                                                                        |
| restored app | `startup.*` → `relaunch.new-app-launched outcome=rolled-back` → `reconcile.rolled-back` → `blocklist.added`                                 | `reason`, `detail`, the new `blocklist`; `to` is still the failed version, `runningVersion` the restored one                                                                                                                 |
| restored app | `reconcile.rollback-notice-shown` / `-dismissed`, `blocklist.retry-requested`, `blocklist.removed`                                          | what the user did about it (a Retry starts a new attempt id)                                                                                                                                                                 |

A slow-quitting old app never ends an attempt. The helper does not call `open` while the old process is alive (that would only activate the old process and look like a failed launch), and it does not give up either (that would leave the untried bundle installed with no first-launch watchdog): it logs `relaunch.old-process-still-running` every `oldExitTimeoutSeconds` (default 300 s) and, once the old process exits, runs the normal watched launch. While it waits, the old Boss is still running.

### Rollbacks performed by older code

A rollback can be reconciled by an app that predates this log (1.0.734 and earlier): it reads the one-line `rolled-back`, blocklists the version, deletes the marker and records nothing else. Every launch of this code checks for that and writes `reconcile.rollback-detected` (reason `unknown` unless a sidecar supplies one), adds the rollback to `install-state.json` and shows the rollback notice:

- **Leftover sidecar**: `rolled-back.detail` present with no `rolled-back` beside it. Its reason, detail and attempt are folded into the rollback history. It never blocklists (the older app already did) and never touches a pending swap, staged directory or backup, which may by then belong to a later attempt.
- **Blocklist grew** since `last-seen-blocklist.json` (written at the end of every reconcile). On the first launch of this code, blocklisted versions that have no rollback record count.
- **Dangling attempt**: a `swap.committed` or `relaunch.helper-started` whose `from` is the version now running, with no `reconcile.installed`, `handshake.flag-written` or `relaunch.committed` and no recorded rollback. Swaps that never committed (`swap.rolled-back-in-process`, `swap.skipped-*`) are not rollbacks.

## Coming from 1.0.734

The first update to a build with this change runs on 1.0.734's code until the helper starts, so part of the trail only exists from the _next_ update on. This is what each stage records when 1.0.734 is the old app (verified against the `boss-v1.0.734` tag, whose updater sources are identical to this change's base):

| Stage                                     | Recorded on the 1.0.734 → this-build update?                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| ----------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| check, download, verify, stage            | **No.** 1.0.734 has no update log; its `updater` os_log lines are `info` (memory-only, normally gone). The staged `manifest.json` has no attempt id.                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| swap (rename into `/Applications`)        | **Only as state.** `install-state.json` gets 1.0.734's pending record (version, from, backup path; no attempt id). No log record.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| quit path (how long 1.0.734 took to quit) | **Only indirectly**, as the helper's `old-process-exited waitedSeconds` (and `old-process-still-running` lines). 1.0.734 writes no `quit.*` records.                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| helper                                    | **Yes, completely**, when 1.0.734 arms a helper (Install & Relaunch, or the automatic startup-swap fallback). 1.0.734 resolves `relaunch-helper.sh` by path through `Bundle.main` _after_ the swap, so `/bin/sh` runs this build's script. 1.0.734 passes no `--log/--attempt/--from`; the helper derives the log path from `--flag`, mints an attempt id, reads `from` from the backup bundle's `Info.plist`, and records `derivedArguments=log attempt from`. Every record also goes to the unified log via `logger` at once. Exit code/signal comes from this build's `Contents/Resources/bin/exit-observer`. |
| automatic swap-on-quit (no helper)        | **No helper runs at all**: 1.0.734's quit path swaps without relaunching, by design. The trail starts with this build's own `startup.*` records on the next manual launch, and `reconcile.installed` closes it (with `attempt: null`, since nothing minted one).                                                                                                                                                                                                                                                                                                                                                 |
| new app launch                            | **Yes**: `startup.*`, `relaunch.new-app-launched`, `handshake.flag-written`, `reconcile.installed`, `engine.decision`, under the helper's minted attempt id (adopted because the pending record has none).                                                                                                                                                                                                                                                                                                                                                                                                       |
| rollback                                  | **Helper side yes; app side no, until later.** The helper records the reason and writes `rolled-back` as a bare version, so 1.0.734 (which is what reopens) still blocklists it and will not re-stage it. 1.0.734 cannot read `rolled-back.detail` and shows no reason. The sidecar stays until a build with this change next launches; that launch records `reconcile.rollback-detected` with the reason, shows the notice, and replays the helper's records into the `update-lifecycle` category. Until then, read the reason from `rolled-back.detail`, the helper's JSONL records, or the `logger` lines.    |

From the next update on (old app = this build or later) every row above is fully recorded: the attempt id starts at `check.available`, `quit.*` timings are logged, the helper gets `--attempt/--from/--log`, and the restored app reads `rolled-back.detail` and shows the reason on the first launch after a rollback.

To read a failed 1.0.734 → this-build update: find the attempt with `all | jq -r 'select(.actor == "helper" and .event == "helper-started") | [.ts, .attempt, .detail.derivedArguments] | @tsv' | tail -3`, follow it with the one-attempt command above, then `cat "$U/rolled-back.detail"`. If the JSONL is missing, `/usr/bin/log show --start "<date>" --predicate 'process == "logger" AND eventMessage CONTAINS "\"actor\":\"helper\""'` returns the same records.

## Where the rollback reason is persisted

`install-state.json`:

```json
{
  "blocklist": ["1.0.177", "1.0.736"],
  "rollbacks": [
    {
      "version": "1.0.736",
      "restoredVersion": "1.0.734",
      "reason": "handshake-timeout",
      "detail": "no first-launch flag …/launch-ok-1.0.736.flag within 30s of pid 48201 starting (process still alive)",
      "attemptID": "upd-1760209681123-3f9a",
      "recordedAt": "2026-10-11T19:08:33.412Z",
      "acknowledged": false
    }
  ]
}
```

`reason` is one of `open-failed`, `launch-not-started`, `process-exited`,
`handshake-timeout`, `bundle-invalid`, or `unknown` (a marker from a helper that predates
reason recording, or a rollback detected after older code reconciled it). The app shows the newest unacknowledged record as a chrome banner —
"Update to 1.0.736 failed: the new version did not finish launching in time; rolled back
to 1.0.734" — with **Retry Update** and **Dismiss**. Dismiss keeps the block; Retry clears
it, re-downloads, and opens the update sheet for Install & Relaunch. Background polling
and the plain Download button never re-stage a blocklisted version (`stage.skipped-blocklisted`
/ `stage.refused-blocklisted`).

## Reproducing locally

`//tools/boss/app-macos/Tests/UpdateCore:UpdateTests` → `RelaunchHelperTests` runs the real `relaunch-helper.sh` and `exit-observer` against stub bundles with `open`/`ps`/`codesign`/`xattr`/`plutil`/`logger` replaced on `PATH`, and asserts the complete trail above for a stalled launch, an early exit (exit code) and a signal exit, an `open` failure, an `open` that never returns (with and without a new process), a healthy launch, a slow-quitting old process that still gets a watched launch, a pre-existing process that is never adopted or killed, both log sinks, and the 1.0.734 argument set (complete trail, a marker 1.0.734 parses, and the reason recovered from the sidecar afterwards). `UpdateStartupTrailTests` covers the startup context, rollback detection and the helper replay.
