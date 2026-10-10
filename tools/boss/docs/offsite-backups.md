# state.db backups and off-machine copies

`state.db` is the engine's system of record. If the laptop dies, anything not copied off it is gone.

## Local backups (always on)

The engine snapshots `state.db` with SQLite `VACUUM INTO` at start and then hourly, into `<state_root>/backups/state.db.bak-YYYYMMDD-HHMMSS` (UTC). The newest 24 are kept. Env overrides: `BOSS_BACKUP_DIR`, `BOSS_BACKUP_INTERVAL_SECS`, `BOSS_BACKUP_RETENTION`. These live on the same disk as `state.db`, so they protect against corruption and mistakes, not against losing the machine.

## Off-machine copies (opt-in)

Boss can copy each finished local backup into a directory that a sync agent replicates off the machine (Google Drive for desktop, Dropbox, iCloud Drive, a NAS mount, ...). Boss has no provider logic and never guesses a destination: it writes files into the directory you name and the sync agent does the rest.

### Settings

In `<state_root>/settings.toml`:

```toml
[backup.offsite]
enabled = true                      # default false
destination = "/absolute/path/to/synced/folder"   # required when enabled; no default
keep_hourly = 24                    # default 24: newest copy in each of the 24 most recent hours
keep_daily = 14                     # default 14: newest copy in each of the 14 most recent days
```

Example for Google Drive for desktop:

```toml
[backup.offsite]
enabled = true
destination = "/Users/<you>/Library/CloudStorage/GoogleDrive-<account>/My Drive/boss-backups"
```

The settings are read once at engine start; restart the engine after editing. The `[backup]` table is preserved when the engine rewrites `settings.toml` for other settings.

Copies land in `<destination>/<hostname>/state.db.bak-YYYYMMDD-HHMMSS`, so several machines can share one destination. The destination directory itself must already exist (Boss only creates the per-host subfolder); a missing destination usually means the sync folder is not mounted.

Retention is the union of both windows, measured over the copies that exist (not wall-clock), so an engine that was off for a week does not prune its only copies. Retention also removes recognized crash staging files older than 24 hours: atomic-publisher `state.db.bak-YYYYMMDD-HHMMSS.<pid>.<sequence>.tmp` files. Recent staging files and unrelated files are left alone.

### How a copy is made

Only the already-consistent local snapshot is copied, never the live `state.db`/`-wal`/`-shm`. The shared atomic publisher streams the copy into an exclusively created `state.db.bak-….<pid>.<sequence>.tmp` sibling in the host folder, fsyncs it, and renames it into place, so the sync agent only ever sees a complete file under the final name. The staging file is briefly visible to the agent.

### Failures are loud, never fatal

If the feature is enabled but the destination is unset, relative, missing, not a directory, or not writable, the engine logs at ERROR at startup naming `[backup.offsite]` and increments `database_backup.offsite.config_invalid`. Unset or relative destinations are caught inline with no I/O; a missing, non-directory or unwritable destination is checked on the background worker. Neither depends on the first local snapshot succeeding. It keeps running and re-validates on every cycle, so a folder that mounts late recovers on its own. A failed copy (destination unmounted, disk full, macOS privacy denial, ...) logs at ERROR, increments `database_backup.offsite.copies_failed`, and never affects the local backup.

Destination validation, copying, and pruning run on a separate single-flight worker. Startup and the local snapshot loop never wait for destination I/O; the finished snapshot is opened before local retention runs, so even `BOSS_BACKUP_RETENTION=0` still copies it. If a previous destination operation is still running, the next copy is skipped, logged at WARN, and counted.

| Metric                                          | Meaning                                                                                                                                                                                                                                                       |
| ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `database_backup.offsite.copies_succeeded`      | counter of completed copies                                                                                                                                                                                                                                   |
| `database_backup.offsite.copies_failed`         | counter of failed copies                                                                                                                                                                                                                                      |
| `database_backup.offsite.config_invalid`        | counter of unusable-config detections                                                                                                                                                                                                                         |
| `database_backup.offsite.copies_skipped`        | counter of cycles skipped while a previous destination operation is running                                                                                                                                                                                   |
| `database_backup.offsite.retention_failed`      | counter of retention enumeration/deletion failures (also logged at ERROR)                                                                                                                                                                                     |
| `database_backup.offsite.success_record_failed` | counter: a copy landed but its success timestamp could not be written to the state root, so the age gauge reads -1 after a restart. Not counted as a failed copy.                                                                                             |
| `database_backup.offsite.last_success_age_secs` | gauge, refreshed every 60s: seconds since the last good copy, persisted locally across restarts for this destination; -1 means no durable success is known (also set when `[backup.offsite]` cannot be parsed). Alert on -1 or when this exceeds a few hours. |

### Why a synced folder rather than a cloud bucket

- **Cost/setup:** the user already runs a sync agent with terabytes free; a bucket needs an account, credentials, a secret to store and rotate, a lifecycle policy, and a new client dependency in the engine.
- **Failure modes:** a bucket client fails on credentials expiry and network; a folder fails on "not mounted" and "not writable", both of which Boss can detect locally and loudly. The tradeoff is that Boss cannot see whether the sync agent actually uploaded the file.
- A bucket alternative was deliberately not built; the `[backup.offsite]` seam would be where it is added.

### Provider caveats (Google Drive for desktop and similar)

- **Streaming vs mirroring:** in _stream_ mode files are fetched on demand and the folder lives under `~/Library/CloudStorage/`; uploads still happen but local disk is only a cache. In _mirror_ mode a full local copy also exists. Either works; verify in the Drive menu that uploads complete.
- **Online-only files:** copies Boss wrote can be evicted locally after upload. That is fine for backups, but restoring requires the file to be downloaded first (open it in Finder or `cp` it, which triggers a download).
- **Partial uploads:** the agent may upload a staging name or a half-synced file during a sync; only trust files that the agent shows as fully synced.
- **macOS privacy:** the engine process needs permission to write under `~/Library/CloudStorage` (Files and Folders / Full Disk Access). A denial surfaces as `copies_failed` plus an ERROR log with `Operation not permitted`.
- **Account state:** signing out of the sync agent leaves the folder present but stale. `last_success_age_secs` only proves the copy into the folder, not the upload; check the agent's status occasionally.
- **Sensitive data:** `state.db` contains your work metadata. Point the destination at an account/folder you are comfortable storing it in.

## Restoring into a fresh install

1. Stop the engine (quit Boss).
2. Locate the state root and make sure it has no running engine against it.
3. Pick the newest copy under `<destination>/<hostname>/` (make sure it is fully downloaded/synced) and copy it to a scratch path first.
4. Move any existing `state.db`, `state.db-wal` and `state.db-shm` aside (do not leave stale `-wal`/`-shm` next to a replaced database).
5. Copy the backup to `<state_root>/state.db`.
6. Start the engine.

An integrity check is useful, but does not prove the restored database opens in the engine. Complete steps 1–6 and confirm restored data through the engine as well:

```sh
sqlite3 "<scratch-state-root>/state.db" 'pragma integrity_check'
```

### Scratch-engine verification

Run against an isolated engine (`--socket-path` under `/tmp`, its own `BOSS_DB_PATH`, no inherited `BOSS_*` variables) with `[backup.offsite]` enabled and short intervals: copies land under `<destination>/<hostname>/`, older copies and stale staging files are pruned, and a snapshot restored per the steps above passes `pragma integrity_check` and its data is visible through the engine API after restart. Confirm the engine is up through the API, not by the socket file's presence, since the socket pathname can outlive the process. This does not verify upload by a cloud sync provider.
