# Update attempt log: following one self-update end to end

Every self-update (check → download → verify → stage → swap → relaunch → first-launch
handshake → reconcile, or → rollback) leaves a durable, structured trail. This page says
where that trail lives, what one record looks like, and how to read one attempt across the
three processes involved: the old app, the detached relaunch helper, and the new app.

## Where the records live

| Surface               | Location                                                                                                                                       | Written by                                                                      |
| --------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| JSONL file (primary)  | `<app support>/Boss/Updates/log/update-log.jsonl`, rotated by size to `update-log.1.jsonl` … `update-log.5.jsonl` (1 MiB each, oldest dropped) | `UpdateCore.UpdateLog` (app) and `relaunch-helper.sh` (helper, via `printf >>`) |
| Unified log mirror    | subsystem `dev.spinyfin.bossmacapp`, category `update-lifecycle`, level `notice`/`error`                                                       | `UpdateLog.live` (app only; the helper has no `os_log`)                         |
| Rollback reason       | `<app support>/Boss/Updates/install-state.json` → `rollbacks[]` (last 20) and `blocklist[]`                                                    | `UpdateInstaller.reconcileAfterLaunch`                                          |
| Helper → app hand-off | `<app support>/Boss/Updates/rolled-back` (`key=value` lines; consumed on the next launch)                                                      | `relaunch-helper.sh`                                                            |

`<app support>` is the user's `Library/Application Support` directory (the same root the
staged `Updates/<version>/` directories live under). One file holds on the order of 5,000
records, so the rotation window covers many dozens of complete attempts.

The unified-log mirror deliberately uses `notice`, not `info`: `info` is memory-only by
default and vanishes from `log show`, which is why the original incident had "no updater
lines" even though the app had logged at `info`. The older `updater` category still
carries the verbose `info`/`debug` lines.

```sh
# Everything the app logged about updates, persisted:
log show --last 2h --predicate 'subsystem == "dev.spinyfin.bossmacapp" AND category == "update-lifecycle"'
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

| Key           | Meaning                                                                                                                                                                                                                                                                                                                                                                                                                        |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ts`          | UTC ISO-8601 (the app adds milliseconds; the helper does not)                                                                                                                                                                                                                                                                                                                                                                  |
| `attempt`     | The per-update attempt id, `upd-<unix-ms>-<4 hex>`. Minted by the downloader when a version is staged, persisted in that version's `manifest.json`, carried into the swap plan and `install-state.json`'s pending record, passed to the helper as `--attempt`, written into the rolled-back marker, and read back by the new/restored app at reconcile. `null` for records outside any attempt (a routine "up to date" check). |
| `from` / `to` | The version being replaced / installed                                                                                                                                                                                                                                                                                                                                                                                         |
| `actor`       | `app` or `helper`                                                                                                                                                                                                                                                                                                                                                                                                              |
| `pid`         | The writing process (old app, helper, or new app — they differ, which is itself useful)                                                                                                                                                                                                                                                                                                                                        |
| `stage`       | `check`, `download`, `verify`, `extract`, `stage`, `swap`, `relaunch`, `handshake`, `rollback`, `reconcile`, `blocklist`, `engine`                                                                                                                                                                                                                                                                                             |
| `event`       | Short event within the stage (see below)                                                                                                                                                                                                                                                                                                                                                                                       |
| `level`       | `info`, `warning`, `error`                                                                                                                                                                                                                                                                                                                                                                                                     |

## Following one attempt

Pick the attempt id from the rollback notice, from `install-state.json`, or from the last
`stage.ready` record, then (with `LOG` set to the JSONL path from the table above):

```sh
grep '"attempt":"upd-1760209681123-3f9a"' "$LOG" | jq -r '[.ts, .actor, .pid, .stage, .event, (.detail|tostring)] | @tsv'
```

A healthy attempt reads, in order:

| Who     | stage.event                                                                                                                                  | What to look at                                                                                                                                                         |
| ------- | -------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| old app | `check.available`                                                                                                                            | `url`, `bytes`, `mode`, `blocked` (should be `false`)                                                                                                                   |
| old app | `stage.begin` → `download.start` → `download.complete`                                                                                       | `url`, `bytes`, `durationMs`                                                                                                                                            |
| old app | `verify.size-ok`, `verify.sha256`, `extract.complete`, `verify.codesign-ok`, `verify.team-id-ok`, `stage.quarantine-stripped`, `stage.ready` | digest, Team IDs, `dir`, `totalDurationMs`                                                                                                                              |
| old app | `swap.requested` → `swap.begin` → `swap.moved-current-to-backup` → `swap.moved-staged-to-install` → `swap.committed`                         | each filesystem op's `result`; `installedVersion` read back from the swapped bundle's `Info.plist`                                                                      |
| old app | `relaunch.parked-for-confirmed-quit` → `relaunch.quit-confirmed` → `relaunch.helper-armed`                                                   | `helperPid`, `oldAppPid`, full `arguments`                                                                                                                              |
| helper  | `relaunch.helper-started` → `relaunch.old-process-exited`                                                                                    | `waitedSeconds` — how long the old app took to exit                                                                                                                     |
| helper  | `relaunch.bundle-assessment`                                                                                                                 | `installedVersion` vs `expectedVersion`, `executableState`, `quarantine`, `codesign`, `otherRunningPid`                                                                 |
| helper  | `handshake.watch-started` → `relaunch.open` → `relaunch.new-process-started`                                                                 | the flag path and **which version's flag** is polled; `open`'s `exit`/`output`; the new `pid`, its `command` (path actually executed) and `version`, `secondsAfterOpen` |
| new app | `relaunch.new-app-launched` → `handshake.flag-written` → `reconcile.installed`                                                               | `pid`, `bundle`, `executable`, `fullVersion`; backup removed                                                                                                            |
| helper  | `handshake.flag-observed` → `relaunch.backup-removed` → `relaunch.committed`                                                                 | `secondsAfterProcessStart`                                                                                                                                              |
| new app | `engine.decision`                                                                                                                            | the `[engine version-check …]` / `[engine upgrade]` / `[engine launch]` line: did the new app attach to or replace the running engine                                   |

A rolled-back attempt diverges after `relaunch.open`:

| Who          | stage.event                                                                                                                                 | Meaning                                                                                                                            |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| helper       | `relaunch.open` with `exit` ≠ 0 → `rollback.decision reason=open-failed`                                                                    | LaunchServices refused; `output` holds its error                                                                                   |
| helper       | no `new-process-started` within `launchTimeoutSeconds` → `rollback.decision reason=launch-not-started`                                      | `open` returned 0 but no process running the installed executable appeared                                                         |
| helper       | `handshake.process-exited` → `rollback.decision reason=process-exited`                                                                      | the new process died before writing the flag; `secondsAfterProcessStart`; look up that `pid` in `~/Library/Logs/DiagnosticReports` |
| helper       | `handshake.timeout` → `rollback.decision reason=handshake-timeout` → `rollback.terminated-new-process`                                      | alive but never wrote the flag within `watchdogSeconds`; the helper terminates it before restoring                                 |
| helper       | `rollback.removed-failed-bundle`, `rollback.restored-backup`, `rollback.marker-written`, `rollback.reopened-previous`, `rollback.completed` | each op's `result`, `restoredVersion`                                                                                              |
| restored app | `relaunch.new-app-launched outcome=rolled-back` → `reconcile.rolled-back` → `blocklist.added`                                               | `reason`, `detail`, the new `blocklist`                                                                                            |
| restored app | `reconcile.rollback-notice-shown` / `-dismissed`, `blocklist.retry-requested`, `blocklist.removed`                                          | what the user did about it                                                                                                         |

One more terminal shape has **no** rollback: `relaunch.old-process-still-running` (exit 2).
The old app did not exit within `oldExitTimeoutSeconds` (default 300 s), so the helper
gave up _without_ calling `open`. Opening the bundle while its previous process is still
registered with LaunchServices only activates that old process and launches nothing, so
proceeding would always have looked like a failed launch of the new version and rolled
back a good install. The swap stays committed; the next manual launch runs the new
version and `reconcile.installed` closes the attempt.

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
reason recording). The app shows the newest unacknowledged record as a chrome banner —
"Update to 1.0.736 failed: the new version did not finish launching in time; rolled back
to 1.0.734" — with **Retry Update** and **Dismiss**. Dismiss keeps the block; Retry clears
it, re-downloads, and opens the update sheet for Install & Relaunch. Background polling
and the plain Download button never re-stage a blocklisted version (`stage.skipped-blocklisted`
/ `stage.refused-blocklisted`).

## Reproducing locally

`//tools/boss/app-macos/Tests/UpdateCore:UpdateTests` → `RelaunchHelperTests` runs the real
`relaunch-helper.sh` against stub bundles with `open`/`codesign`/`xattr`/`plutil` replaced
on `PATH`, and asserts the complete trail above for a stalled launch, an early exit, an
`open` failure, a healthy launch, and an old process that never exits.
