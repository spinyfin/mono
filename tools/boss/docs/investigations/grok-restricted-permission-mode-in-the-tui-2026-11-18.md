# Grok's restricted permission mode in the interactive TUI

- **Date:** 2026-11-18
- **Kind:** empirical characterisation of a production wedge, with the launch change it led to
- **Version under test:** `grok 1.0.70 (f50904eaee6c) [stable]`, model `grok-4.6`
- **Host:** macOS aarch64
- **Related:** [`grok-notification-vocabulary-and-leader-process-2026-07-29.md`](./grok-notification-vocabulary-and-leader-process-2026-07-29.md) (the `permission_prompt` notification), [`grok-permission-isolation-2026-07-27.md`](./grok-permission-isolation-2026-07-27.md) (rule grammar, scoped `HOME`), [worker tool environment](../worker-tool-environment.md)
- **Artifacts:** [`grok-restricted-permission-mode-artifacts/`](./grok-restricted-permission-mode-artifacts/)

## What happened

All four slots of the progression pool were held by Grok progression runs for more than 80 minutes. Each pane showed Grok's tool approval dialog for the run's first shell command:

```text
›  Discover boss CLI and propose verbs
│ echo "BOSS_BIN=$BOSS_BIN"; "$BOSS_BIN" --help 2>&1 | head -80; ...
1 (○) Yes, and don't ask again for anything (always-approve mode)
2 (●) Yes, proceed
3 (○) No, reject (type to add feedback)
```

Nobody watches a progression pane, so nothing answered. Every other progression candidate was deferred with `pool_exhausted`. Claude and Codex progression runs were unaffected.

A progression run is launched with an allowlist under `--permission-mode dontAsk`: run what the list admits, refuse the rest, never ask. For Grok the engine passed `--always-approve --permission-mode dontAsk` plus the allowlist as `--allow` rules and the deny belt as `--deny` rules.

## Verdict

**The `--permission-mode dontAsk` flag does not give a deny-by-default session in Grok's interactive TUI. It gives an ask session.** The deny-by-default posture exists, and is reached through the Claude-compatible setting `permissions.defaultMode = "dontAsk"`, with `--always-approve` absent.

| Launch                                                             | Mode the hooks report | Command outside the allowlist             | `boss <verb>` (allowlisted) | `"$BOSS_BIN" <verb>` (allowlisted) |
| ------------------------------------------------------------------ | --------------------- | ----------------------------------------- | --------------------------- | ---------------------------------- |
| `--always-approve --permission-mode dontAsk` (what was shipped)    | `default`             | **approval dialog, waits for a keypress** | runs                        | **approval dialog**                |
| `--permission-mode dontAsk`                                        | `default`             | **approval dialog**                       | runs                        | **approval dialog**                |
| setting `defaultMode: dontAsk`, no `--always-approve`              | `default`             | refused at once, model told               | runs                        | refused                            |
| setting `defaultMode: dontAsk`, `--permission-mode dontAsk`        | `default`             | refused at once                           | runs                        | refused                            |
| setting `defaultMode: dontAsk`, **`--always-approve`**             | `bypassPermissions`   | **runs**                                  | runs                        | runs                               |
| setting, `--always-approve` **and** `--permission-mode dontAsk`    | `default`             | refused at once                           | runs                        | refused                            |
| `--always-approve`, structural `--deny` rules (an ordinary worker) | `bypassPermissions`   | runs                                      | runs                        | runs, see below                    |

Three things follow.

1. **The flag turns `--always-approve` off and puts nothing in its place.** With the flag present the session is in `default` mode whether or not `--always-approve` is beside it. In `default` mode a call no allow rule covers opens the dialog. That is the wedge.
2. **`--always-approve` beats the setting.** With the setting written and `--always-approve` on the command line (and no flag), a command outside the allowlist simply ran. Always-approve is allow-everything minus the `--deny` rules. A restricted kind must not be launched with it.
3. **The flag is still worth passing.** Since it turns `--always-approve` off, a restricted launch that carries it stays deny-by-default even if `--always-approve` reaches the command line by mistake (row 6).

The hooks report `permissionMode: "default"` for a deny-by-default session too. Grok's hook payloads have no `dontAsk` value (bundled hooks guide: "grok emits `default`, `auto`, `plan`, or `bypassPermissions`"), so the mode field cannot tell an ask session from a deny-by-default one. Only behaviour does.

## A variable in program position is never allowlisted

Boss teaches workers to run its CLI as `"$BOSS_BIN" <verb>`, and the progression allowlist carries each rule in that spelling as well as the bare `boss` one. Under Grok the `"$BOSS_BIN"` rules never match:

| Allow rule added                | Command                     | Result under deny-by-default |
| ------------------------------- | --------------------------- | ---------------------------- |
| `Bash("$BOSS_BIN" task show:*)` | `"$BOSS_BIN" task show abc` | refused                      |
| `Bash($BOSS_BIN task show:*)`   | `"$BOSS_BIN" task show abc` | refused                      |
| `Bash("$BOSS_BIN" task show *)` | `"$BOSS_BIN" task show abc` | refused                      |
| `Bash($BOSS_BIN task show *)`   | `$BOSS_BIN task show abc`   | refused                      |
| `Bash(boss task show:*)`        | `boss task show abc`        | runs                         |

Grok matches rules against the parsed words of a command and does not expand variables; a command whose program is a variable is one it cannot check, and what it cannot check is not pre-approved. The refusal the model sees is "not pre-approved", with no hint. Given a brief that names `"$BOSS_BIN"`, the model made three attempts with that spelling, all refused, and ended its turn with nothing run and nothing declared.

A `PreToolUse` hook runs before the permission rules. With a hook that refuses a `$BOSS_BIN` command and names the bare spelling, the model's next call was `boss task show abc`, which ran, and it used `boss` for the rest of the turn.

Other shapes measured under deny-by-default:

| Command                                                           | Result  |
| ----------------------------------------------------------------- | ------- |
| `boss propose done --outcome delivered --summary "a; b"`          | runs    |
| `boss propose revise --brief '<two lines, with ; and && inside>'` | runs    |
| `cat /etc/hosts` (Grok's built-in read-only list)                 | runs    |
| `boss task show abc \| head -5`                                   | refused |
| `git status` (deny rule)                                          | refused |
| `echo "x=$(whoami)"`                                              | refused |
| file write through the write tool (`Edit(**)` deny)               | refused |

## The `permission_prompt` notification fires under always-approve too

The 2026-07-29 investigation found `notificationType: "permission_prompt"` only with `--always-approve` removed, and concluded always-approve suppresses it. On 1.0.70 it also fires in an always-approve session, for a command Grok cannot check against the `--deny` rules:

```text
t=2.74  pre_tool_use   run_terminal_command  echo "x=$(whoami)"   permissionMode=bypassPermissions
t=3.30  notification   permission_prompt     "Tool permission requested"
t=3.32  post_tool_use  run_terminal_command  exit 0
```

Always-approve answered it 0.02 s later and the command ran. `"$BOSS_BIN" task show abc` behaves the same way. So in an always-approve session the notification is not evidence that anything is stuck, and the next event after it arrives only when the tool finishes, however long that takes.

In a session without always-approve the notification means the dialog is on the pane. It stayed there for the whole of every probe (12 s holds) and for over 80 minutes in production. Under a working deny-by-default launch no notification is raised at all: a refusal produces `permission_denied` and nothing else.

## What changed

- **Launch.** The restricted kinds (answer agent, review guide, progression) are launched without `--always-approve`, with `permissions.defaultMode = "dontAsk"` written to `.claude/settings.json` under the run's scoped process `HOME`, and with `--permission-mode dontAsk` kept on the command line for the reason in point 3. The scoped `HOME` is engine-owned and per run; provisioning clears its `.claude` directory and the posture check already exempts it from the "operator settings leaked" refusal.
- **Spelling.** A `PreToolUse` guard is armed for those kinds that refuses a `$BOSS_BIN` command with the bare spelling to use. It refuses only what the permission rules were about to refuse.
- **Detection.** A restricted worker whose driver reports an approval prompt, followed by no event at all for two minutes, is failed with the reason and its pane and slot are released. This is the state the wedge was, and the one a future Grok change to any of the above would bring back. It applies only to kinds launched without always-approve, because of the previous section. Two minutes is the margin against acting on a prompt that was answered: the only automatic answer measured takes 0.02 s, and a restricted session has none.

`grok inspect --json` does not report the effective mode (`permissions.sources` is empty for a settings file that carries only `defaultMode`), so there is nothing to assert before the pane starts. The detection above is the check.

## Re-running

```bash
python3 scripts/probe.py <label> <posture> '<prompt>'
```

`posture` is a string of tokens: `progression` or `standard` picks the rule set; `noaa` drops `--always-approve`; `settings` writes the `defaultMode` setting and drops the mode flag; `flag` keeps the mode flag alongside the setting; `hook` arms the spelling guard. The script builds a throwaway `GROK_HOME` and scoped `HOME`, points `GROK_AUTH_PATH` at the host credential the way the driver does, drives the TUI in a pty, and prints the hook stream. Each probe spends one short model turn. The stand-in `boss` on `PATH` only echoes its arguments.

Probes behind the tables above, all `grok 1.0.70`:

| Posture                          | Command                          | Outcome                                                                   |
| -------------------------------- | -------------------------------- | ------------------------------------------------------------------------- |
| `progression`                    | `"$BOSS_BIN" task show abc`      | `permission_prompt`, held                                                 |
| `progression`                    | `uname -a`                       | `permission_prompt`, held                                                 |
| `progression`                    | `boss task show abc`             | ran                                                                       |
| `progression-noaa`               | `uname -a`                       | `permission_prompt`, held                                                 |
| `progression-noaa-settings`      | `uname -a`                       | `permission_denied`, turn end                                             |
| `progression-noaa-settings`      | `boss task show abc`             | ran                                                                       |
| `progression-noaa-settings`      | `"$BOSS_BIN" task show abc`      | `permission_denied` ×2                                                    |
| `progression-noaa-settings-flag` | `uname -a`                       | `permission_denied`                                                       |
| `progression-settings`           | `uname -a`                       | ran (`bypassPermissions`)                                                 |
| `progression-settings-flag`      | `uname -a`                       | `permission_denied`                                                       |
| `progression-settings-flag`      | `boss task show abc`             | ran                                                                       |
| `progression-noaa-settings-hook` | brief naming `"$BOSS_BIN"` twice | one hook refusal, then `boss task show abc` and `boss propose done …` ran |
| `progression-noaa-settings`      | the same brief                   | `permission_denied` ×3, nothing declared                                  |
| `standard`                       | `echo "x=$(whoami)"`             | `permission_prompt`, ran 0.02 s later                                     |

The throwaway `GROK_HOME` above has no `config.toml`. The launch that shipped was checked again under the engine's own (`[compat.claude]` and `[compat.cursor]` cells all off, via `PROBE_CONFIG_TOML`) and with the adapter and guard the driver installs (via `PROBE_GUARD_HOOK`):

| Posture, engine `config.toml`                                 | Command                          | Outcome                                              |
| ------------------------------------------------------------- | -------------------------------- | ---------------------------------------------------- |
| `progression` (the launch that wedged)                        | `uname -a`                       | `permission_prompt`, held                            |
| `progression-noaa-settings-flag-hook`, driver's adapter+guard | `uname -a`                       | `permission_denied` ×2, turn end                     |
| `progression-noaa-settings-flag-hook`, driver's adapter+guard | brief naming `"$BOSS_BIN"` twice | one hook refusal, then both `boss` commands ran, 7 s |

The compat cells do not stop `~/.claude/settings.json` from supplying `defaultMode`, which is consistent with the earlier finding that there is no permissions cell.

A restricted session's footer reads `Shift+Tab:mode │ Ctrl+x:shortcuts` under a `Grok 4.6 (high)` composer border, with no `always-approve` label, so the pane monitor's `Shift+Tab:mode` and `Grok 4` markers still match it.
