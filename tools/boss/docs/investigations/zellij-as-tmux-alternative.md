# Zellij as an alternative to tmux for Boss agent sessions

- **Date:** 2026-08-19
- **Kind:** written analysis with a recommendation — no migration, no production-path prototype
- **Related design:** [run-agents-and-the-coordinator-in-tmux-so-work-survives-app-and-engine-restarts.md](../designs/run-agents-and-the-coordinator-in-tmux-so-work-survives-app-and-engine-restarts.md) (PR #2637)
- **Zellij pin used for this write-up:** v0.44.3 (2026-05-13), official `aarch64-apple-darwin` release binary
- **tmux on this host:** 3.6a (`/opt/homebrew/bin/tmux`)

Boss should stay on tmux.

Zellij can create detached sessions, keep them alive across a parent exit, inject keys, dump a pane, and kill a named session. That is not enough. The load-bearing Boss identity scheme — a live spawn token and schema marker that can be read back, plus a server-scoped exclusivity claim — has no first-class Zellij equivalent. The rough edges that prompted this question are mostly tmux defaults Boss has not yet set, or Boss integration work still in flight. Migrating would relocate those problems onto a less mature control surface and rewrite most of the integration.

## Method

Read the in-tree tmux design and the code it produced: `tools/boss/tmux`, `tmux_preflight.rs`, `tmux_adoption.rs`, `tmux_teardown.rs`, `coordinator_tmux.rs`, `spawn_flow.rs`, `runner/pane_spawn.rs`, `settings.rs` (`workers.tmux_hosting`), schema columns on `work_runs`, and the app attach path (`BossPaneModel`, `WorkersWorkspaceModel`).

Read Zellij's own docs and v0.44.3 source, not the marketing feature list: [Programmatic Control](https://zellij.dev/documentation/programmatic-control.html), [CLI Actions](https://zellij.dev/documentation/cli-actions.html), [Commands](https://zellij.dev/documentation/commands.html), [Options](https://zellij.dev/documentation/options.html), [Keybindings](https://zellij.dev/documentation/keybindings.html) / [default.kdl](https://github.com/zellij-org/zellij/blob/v0.44.3/zellij-utils/assets/config/default.kdl), [Layouts](https://zellij.dev/documentation/creating-a-layout.html), [Session Resurrection](https://zellij.dev/documentation/session-resurrection.html), `zellij-utils/src/envs.rs`, `zellij-utils/src/consts.rs`.

Downloaded the v0.44.3 macOS binary to `/tmp/zellij-boss-eval/zellij` (Homebrew could not write `/opt/homebrew` from this worker). Dumped `--help`, `setup --dump-config`, `setup --dump-layout default`, and `setup --dump-layout compact`.

Attempted a live session with `zellij attach --create-background`. The server started, then panicked: `failed to start pty` / `EPERM: Operation not permitted`. The same sandbox rejects `os.openpty()` and `tmux -L … new-session -d`. CLI shape, config, layouts, and source are verified. Live `write-chars` / `dump-screen` / multi-client resize / key passthrough are **not** — they are cited from docs and source only.

Looked for a completed in-tree write-up of the stuck-client failure (the one this prompt says is under active investigation separately). None is on this checkout under `tools/boss/docs/investigations/`, and no merged PR title names it. That failure is left unattributed. A sibling task to hide the tmux status bar is in progress and has no PR yet.

## Verdict

**Stay on tmux.** Do not start a Zellij backend, even behind `workers.tmux_hosting`.

Revisit only if all three become true:

1. Zellij grows CLI-readable, CLI-writable per-session key/value metadata that survives detach (the `tmux show-environment` / `set-option @foo` role), **and**
2. Either Zellij exposes a per-client input-liveness signal comparable to tmux `#{client_activity}`, or Boss no longer needs that detector, **and**
3. A specific, diagnosed tmux defect is shown to be unfixable in Boss's integration and absent in Zellij — not inferred from "tmux felt flaky."

Session resurrection, layout files, and the plugin API are real Zellij strengths. None of them is a Boss requirement today. Cross-reboot resume is an explicit non-goal of the tmux design.

## What Boss actually depends on

The tmux design (PR #2637) is a durability surface, not an automation surface. Turn boundaries still come from the hook/JSONL stream. `send-keys` exists only as the replacement for the app's `SendToPane` write (probes, re-prompts, interrupt). Identity is a random token minted into `state.db` **before** the session exists, then read back from the live multiplexer. Session names are not identity.

The control crate (`tools/boss/tmux`, PR #2640) is the typed surface: `tmux -L boss` isolation, `new-session -d -e -c`, `list-sessions -F`, `show-environment`, `set-option` / `show-options` (session and server), two-phase `send-keys`, `capture-pane`, `display-message`, and token-verified `kill-session`. Preflight requires tmux ≥ 3.2 because that is when `new-session -e` landed (`MINIMUM_VERSION` in `tmux/src/types.rs`).

## Capability matrix

| #   | Boss need                                                           | Zellij                                                                                   | Maturity                                                                           | Hard blocker?                     |
| --- | ------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- | --------------------------------- |
| 1   | Detached create + command + env + cwd                               | Yes, with caveats                                                                        | 0.44 CLI is documented; not live-verified here                                     | No                                |
| 2   | Session survives parent exit                                        | Yes (`on_force_close "detach"` is the default)                                           | Core, long-standing                                                                | No                                |
| 3   | Adopt + verify "this is ours" (token, schema, owner)                | **No first-class equivalent**                                                            | Env at create; plugin-only readback; no CLI `show-environment`; no `@user` options | **Yes**                           |
| 4   | Programmatic input (`send-keys`)                                    | Yes: `write-chars`, `write`, `paste`, `send-keys`                                        | Documented 0.44; not live-verified here                                            | No                                |
| 5   | Output capture (`capture-pane`)                                     | Yes: `dump-screen`, plus `subscribe`                                                     | Documented 0.44; not live-verified here                                            | No                                |
| 6   | Enumerate sessions / panes / clients, including per-client activity | Partial. No `#{client_activity}` equivalent                                              | `list-clients` is CLIENT_ID / pane / command only                                  | Detector must be rebuilt          |
| 7   | Multi-client attach + sizing policy                                 | Attach yes. Sizing is smallest-client; not configurable like tmux `window-size`          | Community-reported; not live-verified here                                         | Not a blocker, but worse for Boss |
| 8   | Chrome off (no status / tab / frames)                               | Yes, via a custom layout + `pane_frames false`                                           | Documented; default is _more_ chrome than tmux                                     | No                                |
| 9   | Key passthrough, including Ctrl+Enter / Shift+Enter                 | Possible under `locked` + `clear-defaults` + Kitty protocol. Default binds are intrusive | Config is real; live fidelity not measured                                         | No, if locked+cleared             |
| 10  | Verified kill + leak detection                                      | `kill-session` by name only. No token check                                              | Core CLI                                                                           | Identity gap, not a missing kill  |
| 11  | Install path / version floor / cadence                              | Homebrew 0.44.3; official tarballs; floor would be ~0.44                                 | Younger and faster-moving than tmux 3.x                                            | Operational cost, not a blocker   |

### 1. Detached session creation

Documented form ([Programmatic Control](https://zellij.dev/documentation/programmatic-control.html)):

```bash
zellij attach --create-background my-session
# optional: options --default-layout /path/to/layout.kdl
```

A layout pane can take `command`, `args`, and `cwd`. Config `env { KEY VALUE }` injects variables into every pane Zellij starts ([Options](https://zellij.dev/documentation/options.html#env)). That is process-wide config, not a per-session `-e` set like `tmux new-session -e`.

There is no `new-session -e KEY=val` equivalent that binds one token to one session independently of the config file. The workable shape for Boss would be a per-session config or layout generated at spawn time, or inheriting env from the `zellij` server process. Both are clumsier than tmux `-e`.

`attach --create-background` did start a server in this sandbox; the session then died on PTY open. The CLI flag itself exists and is the documented headless path.

### 2. Session persistence

Default `on_force_close` is `"detach"`: SIGTERM / SIGHUP from a closing terminal leaves the session running. That matches the Boss requirement (app and engine may die; the agent must not).

Zellij also serializes sessions to cache and can _resurrect_ them after the server itself exits ([Session Resurrection](https://zellij.dev/documentation/session-resurrection.html)). Resurrection re-runs discovered commands behind a "Press ENTER to run" banner unless `--force-run-commands` is set. That is a different feature from "the process kept running." Boss's design explicitly does not survive reboot; treating resurrection as a win would change the product, not drop in for tmux.

### 3. Adoption metadata — the hard blocker

Boss today:

- Writes `BOSS_SPAWN_TOKEN` and `BOSS_SESSION_SCHEMA` atomically via `tmux new-session -e`.
- Mirrors the token as a session user option `@boss_spawn_token` for `list-sessions -F`.
- Treats the **environment** as authoritative for `kill_session_verified` (`tmux show-environment`).
- Claims the private server with a server-scoped `@boss_engine_owner` (`set-option -s`) so two engines cannot both adopt.

Zellij has none of those three knobs as a CLI.

What it does have:

- Config / layout `env { }` — set at session start, applied to panes.
- Plugin API `get_session_environment_variables()` — read-only snapshot of env present when the session was created ([plugin commands](https://zellij.dev/documentation/plugin-api-commands.html)). Requires a WASM plugin and the `ReadSessionEnvironmentVariables` permission. There is no `zellij action show-environment`.
- `disable_session_metadata` / on-disk `session-metadata.kdl` — Zellij's own serialization record (layout, commands), not a user key/value store. Writing into it would fight the serializer.
- `list-sessions` prints names (and exited/resurrectable sessions). No format string, no user options.

There is also no server-scoped option space. Zellij is one server process per session, sharing a socket directory (`ZELLIJ_SOCKET_DIR`, default `$TMPDIR/zellij-<uid>/contract_version_1` — `zellij-utils/src/envs.rs`, `consts.rs`). Exclusivity (`@boss_engine_owner`) would have to become a file lock or a side table. That is a new design, not a port.

Workarounds (session-name convention, `ps eww` on the pane pid, a custom plugin) are weaker than what the adoption pass and token-verified teardown require today. The design is explicit: session names are not identity, and `boss-tmux` exposes no "kill by name alone."

**This is the single most valuable finding.** A migration that cannot read back a live token cannot keep the current safety argument.

### 4. Programmatic input

[CLI Actions](https://zellij.dev/documentation/cli-actions.html) documents four injectors, all accepting `--pane-id`:

- `write-chars` — raw characters
- `write` — raw bytes
- `paste` — bracketed paste (the docs' recommended path for multi-line)
- `send-keys` — named keys (`"Ctrl a"`, `"Enter"`, `"Alt Shift b"`)

That is at least as rich as Boss's two-phase `send-keys -l` + `C-m`, and closer to the paste-buffer path already used for multi-line. Not live-verified here.

`send-keys` is a keystroke stream, same class of API as tmux. Mid-turn semantics stay driver-dependent either way.

### 5. Output capture

`zellij action dump-screen --pane-id terminal_N [--full] [--ansi]` is the `capture-pane` analogue. `zellij subscribe --pane-id … --format json` streams viewport updates as NDJSON. Boss does not need the stream today (transcripts are files; `capture-pane` is diagnostics). Nice, not load-bearing. Not live-verified here.

`list-panes --json` includes `exited` and `exit_status` for command panes — a cleaner dead-pane signal than scraping `#{pane_dead}`.

### 6. Enumeration and the activity timestamp

| Need                     | tmux                             | Zellij                            |
| ------------------------ | -------------------------------- | --------------------------------- |
| List sessions            | `list-sessions -F`               | `list-sessions` (`-n` / `-s`)     |
| List panes               | `list-panes` / `display-message` | `action list-panes --json --all`  |
| List tabs / windows      | `list-windows`                   | `action list-tabs --json`         |
| List clients             | `list-clients -F`                | `action list-clients`             |
| Per-client last activity | `#{client_activity}`             | **Not exposed**                   |
| Pane pid                 | `#{pane_pid}`                    | Not in the documented JSON fields |

Documented `list-clients` columns are `CLIENT_ID`, `ZELLIJ_PANE_ID`, `RUNNING_COMMAND` only ([CLI Actions](https://zellij.dev/documentation/cli-actions.html#list-clients)). No size, no last-input time, no "input path is dead."

Boss just used tmux's per-client `activity` timestamp to detect a client that still rendered output but no longer accepted input. That detector cannot move. Rebuilding it means a new signal (app-side input echo, a probe that must be acked, a GhosttyKit-level watch). That cost belongs in any migration estimate even if the rest of Zellij were a match.

`list-panes --json` does not include the pane pid. Adoption today re-reads `#{pane_pid}` after attach. Zellij would need another way to learn the agent pid (process-group walk from the Zellij server, or the pane command's own pid file).

### 7. Multi-client attach and sizing

Multiple clients can attach. `mirror_session` chooses one shared cursor vs. per-client cursors (default: not mirrored). A `zellij watch` / web read-only attach exists as of the 0.44 notes and does not send input or resizes.

Sizing is not configurable the way tmux `window-size` (`smallest` / `largest` / `latest` / `manual`) is. Users report the session shrinks to the smallest attached client ([discussion #3816](https://github.com/zellij-org/zellij/discussions/3816), [reddit](https://www.reddit.com/r/zellij/comments/1hodgs6/prevent_resize_when_multiple_terminals_attach_to/)). That is the _worse_ policy for Boss: an operator terminal smaller than the app pane would shrink the agent TUI for everyone, including the app.

tmux `window-size latest` letting a second client reshape the app is a **tmux default plus a Boss config omission**. The fix is `setw -g window-size manual` (or `largest`) on Boss sessions, not a multiplexer swap. Zellij cannot express that policy today.

Not live-verified here.

### 8. Chrome suppression

Default layout from `zellij setup --dump-layout default`:

```kdl
layout {
    pane size=1 borderless=true {
        plugin location="tab-bar"
    }
    pane
    pane size=1 borderless=true {
        plugin location="status-bar"
    }
}
```

`compact` still has `compact-bar`. A Boss layout with a single `pane borderless=true`, plus `pane_frames false`, plus no plugin chrome, is the documented way to get a bare full-height pane. `default_mode "locked"` removes the mode-switcher reason to keep a status bar.

This is the same class of work as hiding tmux's status line. Zellij's _default_ is more prominent, not less. A sibling task is already doing that work for tmux.

### 9. Key passthrough

Default binds (from v0.44.3 `default.kdl`) that an agent can trip, outside `locked`:

- `Ctrl g` lock, `Ctrl q` quit
- `Ctrl p` / `n` / `s` / `o` / `t` / `h` / `b` — mode switches
- `Alt n`, `Alt f`, `Alt h/j/k/l`, `Alt +/-`, `Alt i/o`, `Alt p`

In `locked`, only `Ctrl g` (unlock) is bound. The documented Boss-shaped config is:

```kdl
keybinds clear-defaults=true {
    locked {}
}
default_mode "locked"
pane_frames false
```

That is more config than tmux (whose prefix is a single key, default `C-b`, and is already not Boss's problem if the app types into the pane rather than through the prefix). It is doable.

`support_kitty_keyboard_protocol` defaults on when the host terminal supports it. That is Zellij's analogue of tmux `extended-keys`, and it is the one place Zellij might be _better_ for Ctrl+Enter / Shift+Enter — **if** GhosttyKit speaks the protocol end-to-end through a Zellij client. That path was not measured here.

tmux swallowing modified keys unless `extended-keys` is on is **tmux behaviour**, and Boss does not set `extended-keys` in this checkout. That is a one-line session option, not a reason to migrate.

`send-keys` named-key syntax (`"Ctrl Enter"`, `"Shift Enter"`) looks sufficient on paper. Not live-verified.

### 10. Teardown and leaks

`zellij kill-session <name>` kills by name. `delete-session --force` also removes resurrectable residue. There is no token check.

Leak detection can still list sessions in a private `ZELLIJ_SOCKET_DIR` and compare names to `state.db`. Without a live token, "name recycled onto a different execution" is undetectable — the exact case `kill_session_verified` exists to refuse.

`list-sessions` includes exited/resurrectable sessions. A leaked-session sweep would have to distinguish live / exited / resurrectable so it does not "adopt" a serialized ghost.

### 11. Availability and operational maturity

|               | tmux (Boss today)                    | Zellij                                                                                                                     |
| ------------- | ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------- |
| macOS install | `brew install tmux` (preflight text) | `brew install zellij` (formula 0.44.3) or GitHub tarball                                                                   |
| Version floor | 3.2 (`new-session -e`)               | Would need ~0.44 for `list-panes --json`, `send-keys`, `dump-screen`, `attach --create-background`                         |
| Cadence       | Slow; 3.6a is what this host has     | 0.42 → 0.43 (2025-08) → 0.44.0 (2026-03-23) → 0.44.3 (2026-05-13). Three months of silence after 0.44.3 as of this writing |
| Isolation     | First-class `tmux -L boss`           | `ZELLIJ_SOCKET_DIR` + `ZELLIJ_CONFIG_DIR` (env, not a first-class `-L`)                                                    |
| Contract      | Stable CLI                           | Client/server `contract_version_1` in the socket path — a bump is a hard cut                                               |

Zellij would need the same kind of preflight crate tmux has, pinned to ≥ 0.44, with an isolation env Boss owns. Shipping a binary (as GhosttyKit is shipped) is possible; it is new operational surface.

## Attribution of the observed tmux failures

Do not take "tmux is flaky" as the premise. Each known failure, as far as this checkout can tell:

### Modified keys swallowed unless `extended-keys` is on

**tmux behaviour**, plus a **Boss integration omission**. tmux 3.x requires `extended-keys on` (and a terminal that emits them) for Ctrl+Enter / Shift+Enter. This checkout never sets that option. Zellij's Kitty-protocol flag is the same class of fix, not a different class of product. Fix tmux first.

### Status bar needs explicit suppression

**tmux default chrome**, plus a **Boss integration gap**. tmux shows a status line unless `status off`. Boss does not set it at session create in this checkout. A sibling task is already doing that work. Zellij's default chrome is larger (tab-bar + status-bar plugins). A swap makes this problem bigger, then solves it the same way (a session template).

### `window-size latest` lets a second client reshape the app

**tmux behaviour**, plus a **Boss config omission**. tmux's default `window-size` is `latest` (last client to report activity). Boss does not set `window-size` in this checkout. The operator-facing fix is `manual` or `largest` on Boss sessions. Zellij's reported policy is smallest-client and is not configurable. A swap makes the "second client reshapes the app" case worse.

### Client stopped accepting input while still rendering; detach recovered it

**Unattributed.** The prompt says this is under active investigation separately and that Boss used tmux `#{client_activity}` as the detector. This checkout has no such investigation file and no `list-clients` / `client_activity` usage in `boss-tmux`. Possible homes: tmux client bug, GhosttyKit ↔ tmux-client input path, or a general "interactive program behind a multiplexer client" failure.

A migration justified by this fault would relocate an undiagnosed bug and pay for a new detector, because Zellij does not expose the signal. Wait for that investigation. If it concludes "tmux client input can die independently of the pane, and the only recovery is detach," that is still first a Boss attach-path bug to fix (rebuild the client, as `BossPaneModel` already does on child exit). It is not, by itself, a Zellij argument.

## Does Zellij solve any of those problems?

No.

It presents a different set: more default chrome, more default keybinds, smallest-client resize, no session user options, no server-scoped owner, no per-client activity, a socket-dir isolation story instead of `-L`, and a version floor that is months old rather than years old.

The one plausible improvement for _modified keys_ (Kitty protocol on by default) is also available as a tmux option.

## What Zellij is actually better at

These are real, and they are not why Boss picked a multiplexer.

- **Session resurrection** after the server dies, including optional viewport/scrollback. Boss's design rejects cross-reboot resume. Resurrection that re-runs `claude` / `codex` / `grok` behind an ENTER banner is the wrong default for an agent.
- **Layout files** that declare a pane + command + cwd as data. Boss needs one full-height pane. A layout is a fine way to suppress chrome; it is not a reason to switch.
- **Plugin / pipe surface.** Powerful. Boss would have to write and ship a WASM plugin to recover even a fraction of `show-environment`. That is extra cost, not a shortcut.
- **JSON `list-panes` / `list-tabs` and `subscribe`.** Cleaner than `tmux -F`. Not worth a rewrite.
- **Read-only attach** (`watch`, web client). Genuinely nicer than hoping a second tmux client will not resize anything — _if_ the web stack is acceptable. It does not replace the app's attached client.

## Migration cost

The honest answer is **most of it**.

This is not a shim. A Zellij backend would retouch every layer the tmux project added:

| Layer         | What exists                                                                          | What a switch touches                                                                                                                                     |
| ------------- | ------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Control crate | `tools/boss/tmux` (create, list, env, options, send, capture, verified kill)         | New crate, or a trait that both implement. Zellij cannot implement the current trait honestly (no `show-environment`, no `@` options, no server options). |
| Preflight     | `tmux_preflight.rs`, `bossctl doctor` tmux probe, tmux ≥ 3.2                         | Same shape, different binary, floor ~0.44, isolation via `ZELLIJ_SOCKET_DIR`                                                                              |
| Schema        | `work_runs.tmux_*` columns (PR #2642)                                                | Rename or add a parallel `zellij_*` set. Coordinators live in a metadata singleton.                                                                       |
| Spawn         | `start_tmux_worker` in `spawn_flow.rs`, `runner/pane_spawn.rs`                       | New create path, generated layout/config for env+cwd+command                                                                                              |
| App attach    | `exec tmux -L boss attach-session -t …` in `BossPaneModel` / `WorkersWorkspaceModel` | `exec zellij --session … attach` plus env for socket/config. Rebuild-on-client-exit logic stays.                                                          |
| Pane input    | `pane_delivery.rs` → `Tmux::send_keys` (PR #2725)                                    | Map onto `paste` + `send-keys`. Re-validate the two-phase submit finding.                                                                                 |
| Adoption      | `tmux_adoption.rs` (PRs #2647, #2648)                                                | Cannot port token/schema/owner reads. New exclusivity design.                                                                                             |
| Liveness      | `stale_worker_sweep` / pane pid via `display-message`                                | Need a pid source; `list-panes --json` does not include one                                                                                               |
| Leak sweep    | `husk_pane_sweep` + session list (PR #2652)                                          | Possible against a private socket dir; weaker without tokens                                                                                              |
| Teardown      | `tmux_teardown.rs` + `kill_session_verified` (PR #2649)                              | `kill-session` by name is the only verb. The safety argument changes.                                                                                     |
| Coordinator   | `coordinator_tmux.rs` (PR #2727)                                                     | Same lifecycle, same identity problem                                                                                                                     |
| Settings      | `workers.tmux_hosting` per-pool flag                                                 | A second backend flag is conceivable; see below                                                                                                           |
| Tests         | Stubbed `CommandRunner` coverage across the crate and engine                         | All of it, twice, if both backends live                                                                                                                   |

Plus protocol attach-request fields (`tmuxProgram`, `serverLabel`) and every fixture that speaks tmux command argv.

### Incremental vs. all-or-nothing

A second backend _behind the same per-pool flag_ is possible only as a parallel stack: `boss-zellij` next to `boss-tmux`, generalized schema, dual attach strings, dual adoption, dual teardown. It is not a thin `enum Multiplexer { Tmux, Zellij }` over the current trait, because the current trait's load-bearing methods (`show_environment`, `set_option`, `set_server_option`, format-string list) do not exist on Zellij.

Paying for both is how you get a real comparison. It is not cheaper than "just try Zellij on the review pool." Until the identity gap closes upstream, the second stack cannot offer the same adoption/teardown guarantees tmux already has. That makes incremental enablement a product lie: the flag would mean "this pool is hosted in a multiplexer we cannot re-adopt safely."

So: **not a viable incremental swap today.** All-or-nothing would still leave a window where workers cannot be verified on restart. Neither is justified.

## Recommendation

Stay on tmux. Fix the observed issues as Boss session options and attach-path bugs:

1. Set `extended-keys on` (and confirm GhosttyKit emits them) for Ctrl+Enter / Shift+Enter.
2. Set `status off` on Boss-created sessions (the in-flight sibling task).
3. Set `window-size manual` or `largest` so an operator terminal cannot reshape the app.
4. Let the stuck-client investigation finish before treating that failure as a multiplexer-choice input. If the detector is `#{client_activity}`, keep it — Zellij cannot replace it.

Do not open a Zellij tracking project. The named conditions to revisit are in [Verdict](#verdict).

## What this pass could not establish

- Live `write-chars` / `paste` / `send-keys` / `dump-screen` against a running pane (sandbox `EPERM` on PTY).
- Live multi-client resize policy (same reason). Cited as community + lack of a `window-size` equivalent in Options.
- Ctrl+Enter / Shift+Enter fidelity through Zellij + GhosttyKit.
- Whether `list-panes --json` can be made to include a pid by some undocumented flag (documented fields do not have one).
- The conclusions of the separate stuck-client investigation — it is not in this tree.
- Whether a throwaway WASM plugin could expose session env on stdout cheaply enough to close the identity gap. Even if yes, that is a new Boss-owned component to ship, sign, and version-gate.

## Follow-up (for a human to file, not this PR)

None of these are part of this deliverable:

- Session options: `extended-keys on`, `status off`, `window-size manual` (or `largest`) on every Boss-created tmux session.
- After the stuck-client investigation lands, a short addendum here if its attribution changes anything above. It should not, unless it finds a tmux defect that cannot be fixed on the attach path.
