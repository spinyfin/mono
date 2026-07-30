# Grok `Notification` vocabulary + leader process — probe artifacts

Companion to
[`../grok-notification-vocabulary-and-leader-process-2026-07-29.md`](../grok-notification-vocabulary-and-leader-process-2026-07-29.md).

| Path                                   | Contents                                                                                                  |
| -------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `scripts/mkhome.sh`                    | Builds a throwaway `GROK_HOME` (byte-copied auth, pre-seeded trust, dump-all hook wiring)                 |
| `scripts/dump_hook.py`                 | The hook body itself: writes one JSONL record per lifecycle event (stdin payload + hook env)              |
| `scripts/tui.py`                       | Minimal pty driver for an interactive `grok` TUI (scripted keystrokes, capture, bounded reap)             |
| `scripts/tui_hold.py`                  | Starts a TUI in a pty and holds it alive so another process can inspect it                                |
| `scripts/leader_explicit_lifecycle.py` | Starts `grok agent leader` in its own group; measures socket creation and SIGTERM reap                    |
| `scripts/leader_group_reap.py`         | The decisive test: TUI-spawned leader's pgid vs the pane's, then `killpg(pane_pgid, SIGTERM)`             |
| `evidence/a_notification/*.json`       | Per-probe hook streams (`hook_event`, `notificationType`, `level`, `message`, `toolName`), UUIDs redacted |
| `evidence/b_leader/measurements.txt`   | Raw `ps` process tables and leader lifecycle measurements (L1–L8)                                         |

Pinned CLI under test: **`grok 0.2.114`** — note this is _newer_ than the
`PINNED_GROK_VERSION` (`0.2.112`) the driver asserts. See the findings doc.

## Apparatus rules

- The operator's live `~/.grok` is **never** used as a runtime `GROK_HOME`.
  `auth.json` is **byte-copied** into throwaway homes.
- Model is always `grok-4.5`. `grok-code-fast-1` is retired and silently
  redirects, so it is never a probe target.
- Process liveness is read from `ps` only. An in-process `kill(pid, 0)` check
  reports **zombies as alive** and produced two wrong readings during this
  investigation before it was discarded — do not reintroduce it.

## Re-run

```bash
./scripts/mkhome.sh /tmp/gh1 /tmp/ws1 /tmp/probe.jsonl   # short paths matter, see below
python3 scripts/leader_group_reap.py /tmp/gh1 /tmp/ws1
```

Keep `GROK_HOME` short. The leader socket is `$GROK_HOME/leader.sock` and is
subject to the macOS `sun_path` limit (104 bytes); a longer home makes the
leader fail to start with `Timeout waiting for IPC socket to be created`,
which is easy to misread as "the TUI never spawns a leader".
