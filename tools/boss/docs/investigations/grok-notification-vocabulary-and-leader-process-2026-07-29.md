# Grok `Notification` vocabulary and the leader process

- **Date:** 2026-07-29
- **Kind:** empirical post-integration characterisation — findings + throwaway harness; the only production change is a comment/test correction plus **no** capability declaration
- **Version under test:** `grok 0.2.114 (0c785038798)` — at the time newer than the driver's then-current `PINNED_GROK_VERSION` (`0.2.112`); the version pin was removed 2026-08-01 (operator decision), see [Version drift](#version-drift-the-driver-would-currently-refuse-to-provision)
- **Host:** macOS aarch64
- **Related:** design [G-13](../designs/grok-as-a-first-class-interactive-agent-driver.md) / T-24 (a) and [OQ-4](../designs/grok-as-a-first-class-interactive-agent-driver.md) / T-27 (b); [`grok-pretooluse-decision-vocabulary-and-tool-name-map.md`](./grok-pretooluse-decision-vocabulary-and-tool-name-map.md); [`grok-tui-liveness-markers-under-ghosttykit.md`](./grok-tui-liveness-markers-under-ghosttykit.md)
- **Artifacts:** [`grok-notification-and-leader-artifacts/`](./grok-notification-and-leader-artifacts/)

## Why this investigation exists

Two questions the design left explicitly uncharacterised:

**(a)** Which `notificationType` / `level` values positively mean "blocked awaiting a human", and can any of them occur for a Boss worker at all given `--always-approve` and pre-seeded folder trust? `Capability::AwaitingInputSignal` may only be declared on a measured mapping — its contract forbids synthesising the state from a lower-fidelity channel.

**(b)** Whether a per-run `GROK_HOME` gives each worker its own leader (assumed, never measured), whether a leader outlives its worker, and whether SIGTERM reap of the pane process group actually reaps it.

---

## Verdict (read this first)

### (a) `AwaitingInputSignal` stays **undeclared**. Negative result, and it is the correct one.

The genuine awaiting-input signal **exists** — but it is **unreachable for a Boss worker**.

| `notificationType`  | `level` | Observed message                  | Means "blocked on a human"? | Reachable under Boss flags?                                      |
| ------------------- | ------- | --------------------------------- | --------------------------- | ---------------------------------------------------------------- |
| `permission_prompt` | `info`  | `Tool permission requested`       | **Yes**                     | **No** — `--always-approve` suppresses the prompt that raises it |
| `task_complete`     | `info`  | `Background task completed: <id>` | No — informational          | **Yes**                                                          |

Boss spawns with `--always-approve --trust --no-subagents --no-memory` (`grok.rs:154-157`). Under exactly those flags the only `Notification` observed across every probe was `task_complete`, on background-task completion. Declaring `AwaitingInputSignal` and mapping it to `task_complete` would be precisely the fabricated `WaitingForInput` the capability contract prohibits.

**So the population is not empty — but the _blocked_ population is.** A Grok worker shows `Working` / `Idle` and never a fabricated `WaitingForInput`. That is the correct fallback, and the omission gates nothing.

### (b) Per-run `GROK_HOME` **does** isolate the leader — but the leader **escapes Boss's pane reap**.

| Question                                                    | Answer                                                                                                                  |
| ----------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| Does a per-run `GROK_HOME` give each worker its own leader? | **Yes.** Socket and lock are home-derived: `$GROK_HOME/leader.sock`, `$GROK_HOME/leader.lock`. No sharing across homes. |
| Does a Boss-shaped TUI worker spawn a leader today?         | **No.** Leader mode is off unless `[cli] use_leader = true`; Boss never sets it.                                        |
| Does a leader outlive its worker?                           | **Yes.** It reparents to launchd (`ppid 1`) and persists; exit-on-disconnect did not reclaim it.                        |
| Does SIGTERM of the pane process group reap it?             | **No.** The leader is auto-spawned into its **own process group**, so `killpg(pane_pgid, …)` never signals it.          |
| Does the TUI itself reap correctly?                         | **Yes.** It exits on SIGTERM (goes `<defunct>` within ~2s). Only the leader escapes.                                    |

**This is latent, not live.** Two independent conditions each keep it off today, so no fix is warranted in this PR — but both are one config change away, so the doc records the trigger rather than the patch (per the brief: _if reap proves unreliable, say so explicitly with a proposed fix rather than patching reap inside this PR_).

---

## (a) The `Notification` channel, measured

### There are two separate "notification" systems — do not conflate them

`05-configuration.md` documents `[ui.notifications]` with `events = ["turn_complete", "approval_required", "session_ready", "task_complete", "agent_error"]`, and separate `[[ui.notifications.hooks]]` entries receiving `$GROK_EVENT` / `$GROK_MESSAGE`. That is the **terminal-notification** system (OSC 9/99/777/BEL, focus-gated).

The lifecycle **`Notification` hook** is a different channel:

- `$GROK_EVENT` and `$GROK_MESSAGE` were **null on every hook invocation** across all probes — the two channels do not share an env contract.
- Its vocabulary is **not** the documented five. `permission_prompt` is not in that list at all, and is undocumented everywhere in the bundled user guide.
- Ungating the terminal system entirely (`condition = "always"`, `idle_threshold_secs = 0`, all five events enabled) produced **zero** `Notification` hook events across a complete tool-using turn. The hook is not driven by that config.

`level` is undocumented; both observed values were `info`. No `warn`/`error` level was produced by any probe, so the level vocabulary remains only partially characterised — it is **not** a reliable severity discriminator on this evidence.

> A binary-strings pass suggested a three-variant enum (`unknown` / `tool_execution` / `permission_prompt`). Empirically that is **wrong or unrelated** — `task_complete` fires on the wire and is absent from it. Recorded so the next reader does not re-derive a vocabulary from packed strings; the wire is the only authority here.

### Probe matrix

All probes: `grok 0.2.114`, model `grok-4.5`, isolated `GROK_HOME`, dump-all hook on all fourteen lifecycle events. Boss-shaped flags unless stated.

| #   | Configuration                                                     | `Notification` fired                   | Blocked on a human? |
| --- | ----------------------------------------------------------------- | -------------------------------------- | ------------------- |
| P1  | headless `-p`, `--always-approve`                                 | none                                   | no                  |
| P2  | interactive TUI, Boss flags                                       | none                                   | no                  |
| P3  | interactive TUI, Boss flags + `[ui.notifications]` fully ungated  | none                                   | no                  |
| P4  | interactive TUI, **no `--always-approve`**, sat on the prompt 35s | **`permission_prompt` / `info`**       | **YES**             |
| P5  | interactive TUI, Boss flags + background task                     | **`task_complete` / `info`**           | no (informational)  |
| P6  | `--always-approve`, no trust seed, cwd under `/tmp`               | none — no trust dialog, turn completed | no                  |
| P7  | `--always-approve`, no trust seed, cwd **outside** `/tmp`         | none — no trust dialog, turn completed | no                  |
| P8  | Boss flags + `PreToolUse` hook **deny**                           | none (guard held, no `post_tool_use`)  | no                  |
| P9  | Boss flags + `--deny` rule                                        | none (`permission_denied` hooks only)  | no                  |

Notes that matter:

- **P4 is the positive control.** Removing `--always-approve` produced a real blocking prompt and the `permission_prompt` notification, followed by `permission_denied` when the prompt was abandoned. The signal shape the design hypothesised is real — it is just gated off by Boss's own spawn flags.
- **P6/P7 retire a worry.** `--always-approve` suppresses the folder-trust dialog outright, with or without a pre-seeded trust store, inside and outside `/tmp`. Boss's trust pre-seeding is belt-and-braces; the dialog is not a latent blocked state for a Boss worker.
- **P8/P9 cover Boss's actual guardrails.** Neither a `PreToolUse` hook deny nor a `--deny` rule raises a notification or blocks on a human. Denials surface as `permission_denied` lifecycle events, which are not an awaiting-input signal.
- **P8 also re-confirms the adapter hazard.** A first attempt using Claude's `hookSpecificOutput` / `permissionDecision` shape **fail-opened** — the tool ran. Only Grok's documented `{"decision":"deny","reason":…}` blocked it. This is the design's "hooks fire and approve" failure mode reproduced incidentally; it is already owned by the hooks-adapter work and is not re-litigated here.

### Why this does not become a capability

`Capability::AwaitingInputSignal` requires a channel that positively distinguishes "blocked on a human" from busy/idle. For a Boss Grok worker that channel carries exactly one value — `task_complete` — whose meaning is "a background task finished", i.e. the opposite of blocked. There is nothing honest to bind to.

The declaration becomes earnable only if Boss ever spawns Grok workers **without** `--always-approve`. At that point `permission_prompt` is the mapping, and it is already measured here.

---

## (b) The leader process, measured

### It is home-derived, so per-run isolation works

`grok agent leader` creates `$GROK_HOME/leader.sock` and `$GROK_HOME/leader.lock`. With distinct `GROK_HOME`s there is no shared socket and no cross-talk, which settles the design's assumption in the design's favour: **16 concurrent workers would not share one leader.**

### The socket path is subject to the macOS 104-byte `sun_path` limit

A leader under a 157-byte home failed with `Error: Timeout waiting for IPC socket to be created` (exit 1). The identical command under a 20-byte home started normally. This is a plain `sun_path` overflow with an unhelpful error.

It matters because **Boss's own per-run path already exceeds the limit**:

```
$TMPDIR/boss-grok-homes/<run-id>/grok-home/leader.sock
/var/folders/9w/…/T/boss-grok-homes/exec_<id>/grok-home/leader.sock   = 112 bytes  > 104
```

So even if `[cli] use_leader` were switched on, the leader would fail to start rather than work. That is a second, independent reason the leader is not live for Boss today — and a trap for anyone who enables leader mode and reads the timeout as a Grok bug.

### The leader escapes the pane process group

With `[cli] use_leader = true`, the TUI auto-spawns a leader as its **child** but in a **different process group**:

```
  PID  PPID  PGID  COMM
35105 35104 35105  grok                 <- TUI (pane leaf)
35170 35105 35170  .grok/bin/grok       <- leader: child of the TUI, own pgid
```

Boss reaps a pane by signalling the process **group**. Measured directly:

```
TUI pid=46324 pgid=46324
  child(leader) pid=47696 pgid=47696  in_pane_group=False
killpg(46324, SIGTERM) -> leader 47696 still live at t+2s, t+5s, t+10s
```

This is structural, not a race: the leader is not a member of the group being signalled.

### It outlives its worker

After the TUI died, the leader reparented to launchd and kept its socket live:

```
35170     1 35170  .grok/bin/grok
$ grok leader list --json
[{"pid":35170,"pidLive":35170,"classification":"Reachable","socketPath":"/tmp/gl2/leader.sock", …}]
```

Reproduced independently in a second run (leader `52231`, `ppid 1`). `--no-exit-on-disconnect` implies the default is exit-on-disconnect, but **neither leader was reclaimed** within the observation window after an abrupt worker termination — an abrupt SIGTERM of the client is evidently not the clean disconnect that triggers it. Both had to be killed by hand.

A leader signalled **directly** does terminate cleanly (SIGTERM → exit 143), and it leaves stale `leader.sock` / `leader.lock` behind on disk. The residue is inside the per-run `GROK_HOME`, which Boss owns and discards, so it is harmless.

### Field evidence this is a real failure mode

```
19176     1 19176  Rs  grok   ELAPSED 02-00:39:29
grok --no-alt-screen --always-approve --session-id eca19633-… --cwd /tmp/grok-pane-spike/cwd …
```

A TUI left over from the 2026-07-27 pane-viability spike, reparented to launchd and **still alive two days later**. It is a TUI rather than a leader — and the TUI reaps correctly when actually signalled (L7) — so this is evidence that spike harnesses leak Grok processes, not that Boss's reap is broken today. It is the shape of what a leaked leader would look like at 16-way concurrency. Left untouched; it is not this run's to clean up.

### Proposed fix — for when leader mode is ever enabled, not for now

No reap change is made in this PR: leader mode is off, and the socket path would fail anyway. If either changes, the fix is **not** to widen the pane reap to the session — that would over-reach and kill siblings. It is:

1. Keep `[cli] use_leader` **explicitly `false`** in `render_base_config_toml()`, the same way `vim_mode = false` is pinned rather than left to an upstream default that could change. This is the cheap, durable guard and the one worth doing first.
2. If leader mode is ever wanted, shorten the container path so `$GROK_HOME/leader.sock` fits in 104 bytes, and reap the leader explicitly by pid — discovered via `grok leader list --json` against the run's own `GROK_HOME`, which reports `pid` and `classification` — rather than relying on group semantics.

Recommendation (1) is a one-line config change, but it is a **behavioural change to worker spawn** rather than a characterisation, so it is surfaced here for an operator decision rather than taken unilaterally in an investigation PR.

---

## Incidental observation: `auth.json` can vanish under a host-side refresh

Mid-investigation, `~/.grok/auth.json` disappeared for an extended window, leaving only `auth.json.lock` containing `17788:1785366485` — the pid of the operator's own long-running interactive Grok session. The probes only ever _read_ that file; the removal came from that session's own token refresh. Probe homes built during the window got no credential and failed until the harness was repointed at a stashed byte-copy.

This is worth recording because `provision_grok_home` **symlinks** `$GROK_HOME/auth.json` → `~/.grok/auth.json` (`home.rs:494-510`) rather than copying it. Every concurrent Grok worker therefore shares one symlink target, and while that target is absent **every worker's credential is simultaneously absent**. A byte-copy at provision time, or a retry around the refresh window, would decouple workers from the operator's interactive session.

Not measured further (it was not this run's question, and reproducing it means racing someone else's token refresh), so it is recorded as an observation rather than a finding, and no code change is proposed on this evidence alone.

## Version drift: the driver would currently refuse to provision

`assert_inspect_json_posture` hard-fails unless `grokVersion` starts with `PINNED_GROK_VERSION` (`0.2.112`, `home.rs:40`, `:653`). The host runs **`0.2.114`**, so Grok provisioning would abort with _"Re-characterise before upgrading the pin."_

This investigation re-characterised **two** surfaces (Notification, leader) — not the whole posture (pane markers, decision vocabulary, permission isolation, sandbox grammar were all measured against `0.2.112`). Bumping the pin asserts a re-characterisation that has not happened, so **the pin is deliberately left alone** and this is surfaced as its own piece of work.

**Superseded 2026-08-01.** This predicted failure mode materialised for real: Grok auto-updated 0.2.114 → 0.2.117 on 2026-07-31 and every Grok execution died in provisioning, exactly as described above, except now with no re-characterisation available to unblock it (Grok updates itself; there is no "hold the upgrade until the harness passes" step to gate). The operator's call was to remove the version pin rather than keep re-chasing it: `assert_inspect_json_posture` no longer gates on `grokVersion` at all — it only observes the value and logs a `tracing::warn!` when it drifts from `LAST_CHARACTERISED_GROK_VERSION`. The other four posture checks this investigation did not touch (`projectTrusted`, the compat-cell matrix, the hooks inventory, and operator-`$HOME` permission-source isolation) are unaffected and still fail closed.

## What this changes in the driver

Only the honesty of the record — the capability set is unchanged:

- `grok.rs`: the `AwaitingInputSignal` omission comment said the vocabulary "is uncharacterised". It is now characterised, so the comment states the measured result and the condition under which the capability becomes earnable.
- `grok/progress.rs`: the existing `task_complete` fixture is confirmed real (its shape matches the wire exactly). A sibling test pins the `permission_prompt` payload so the measured awaiting-input shape is captured in code even though the capability stays undeclared.
