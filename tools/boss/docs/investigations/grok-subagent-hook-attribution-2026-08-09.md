# Grok subagent hook attribution under a Boss-owned GROK_HOME

- **Date:** 2026-08-09
- **Kind:** empirical investigation — findings + throwaway harness only; the only engine change it justifies is a comment and a regression test
- **Pinned version:** `grok 1.0.0 (3cd0d0cbcebe) [stable]` (`~/.local/bin/grok`)
- **Host:** macOS aarch64
- **Question:** can `--no-subagents` (`tools/boss/engine/driver/src/grok.rs:210`) be removed?
- **Answer:** **No.** Permission interception is sound; **progress attribution is not**.
- **Related:** [grok-permission-isolation-2026-07-27.md](./grok-permission-isolation-2026-07-27.md) (apparatus rules, `GROK_HOME` + `HOME` scoping), [grok-pretooluse-decision-vocabulary-and-tool-name-map.md](./grok-pretooluse-decision-vocabulary-and-tool-name-map.md) (`deny` is the only vocabulary that blocks), [ghostty-grok-pane-viability.md](./ghostty-grok-pane-viability.md)
- **Artifacts:** [`grok-subagent-hook-attribution-artifacts/`](./grok-subagent-hook-attribution-artifacts/)

## Verdict (read this first)

| #   | Question                                                                       | Result                                                                                                                                                                                                                                                                                                         |
| --- | ------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | Does a Grok subagent inherit and fire the global `$GROK_HOME/hooks/` handlers? | **Yes**, every wired event.                                                                                                                                                                                                                                                                                    |
| 2   | Are the subagent's tool calls intercepted by permission policy?                | **Yes.** A `PreToolUse` `deny` from the global hook set blocks a subagent's shell call exactly as it blocks the top-level session's. **No safety gap.**                                                                                                                                                        |
| 3   | Are the subagent's turns attributed to the right transcript and turn boundary? | **Partly — and the part that fails is load-bearing.** The subagent gets its own `sessionId` and `transcriptPath`, and its turn end fires `subagent_stop`, not `stop`. But it also fires **`session_end`**, whose payload is _shape-identical_ to the top-level session's, on Boss's currently-wired event set. |
| 4   | What does Boss observe while a subagent runs?                                  | Progress ingress stays live. **But `background_children.rs` false-idle suppression does not apply at all** — a Grok subagent is in-process, so there is no descendant process to count.                                                                                                                        |

**Bottom line: leave `--no-subagents` in place.** Enabling it today makes every Grok worker that delegates emit a `SessionEnd` that Boss cannot distinguish from its own session ending.

## The blocking finding: a subagent's `session_end` is indistinguishable from the worker's

A subagent's `session_end` and the top-level session's `session_end` carry the **same key set and the same `reason`**:

```
subagent keys : cwd hookEventName permissionMode reason sessionId timestamp transcriptPath workspaceRoot
top-level keys: cwd hookEventName permissionMode reason sessionId timestamp transcriptPath workspaceRoot
same key set  : True
reason        : "shutdown"   (both)
differs       : sessionId, transcriptPath, timestamp   (only)
```

Verbatim payloads: the `subagent.session_end` and `top_level.session_end` entries of [`hook_payloads.json`](./grok-subagent-hook-attribution-artifacts/hook_payloads.json).

The only discriminator is the `sessionId` **value**, and Boss never compares it:

- `tools/boss/engine/core/src/events_socket.rs:340-370` routes a connection by `_boss_run_id` (spliced in by the `boss-event` shim from `BOSS_RUN_ID`) — the same value for parent and subagent, because they are the same process under the same env.
- `tools/boss/engine/driver/src/grok/progress.rs:54` maps `session_end` → `SessionEnd`; `tools/boss/protocol/src/worker_event.rs:149` builds `WorkerEvent::SessionEnd`.
- `tools/boss/engine/core/src/live_worker_state.rs:1022` applies it **by `slot_id`**, not by session id, and sets `activity = WorkerActivity::Terminated`.
- `tools/boss/engine/core/src/events_socket.rs:481` additionally publishes `Event::AnswerAgentDied` for the run.

`WorkerActivity::Terminated` is terminal (`tools/boss/protocol/src/live_worker_state.rs:73`), so while the flag is set:

- `accepts_typed_input()` is false — nudges, interrupts and answer delivery are refused for a worker that is alive and working.
- `activity_for_run` / `is_run_live` (`live_worker_state.rs:903`, `:890`) skip the slot, so a mid-turn finalization guard reads "no live worker".
- `ServerState::list_husk_panes` filters terminal entries out of its live set — the exact 2026-07-26 incident shape that `husk_pane_sweep`'s module docs describe (`tools/boss/engine/core/src/husk_pane_sweep.rs:66-75`), where a spurious `SessionEnd` burst got five live workers SIGTERMed. `live_process_evidence` corroboration (added in response to that incident) would spare the pane here, but it is a backstop against a bug, not a licence to introduce one deterministically.

For a **blocking** subagent the wrong state is transient — the parent's next `pre_tool_use` restores `Working`. For a **background** subagent it is not: the parent's turn has already ended, so `Terminated` is the slot's last observed state until something else arrives.

### The measured background-subagent timeline

From [`timelines/tui_bg_outlives.md`](./grok-subagent-hook-attribution-artifacts/timelines/tui_bg_outlives.md), annotated with what Boss's reducer does with each event on the currently-wired set. Every row carries the **same** `_boss_run_id`:

| t      | event                                            | session   | Boss's resulting `WorkerActivity`                                                                                                                        |
| ------ | ------------------------------------------------ | --------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| +9.1s  | `stop` (`reason: end_turn`)                      | parent    | `Idle` — correct, the parent's turn did end                                                                                                              |
| +24.0s | `user_prompt_submit`                             | **child** | `Working` — a turn nobody submitted                                                                                                                      |
| +26.1s | `pre_tool_use run_terminal_command`              | **child** | `Working`, `current_tool = Bash`                                                                                                                         |
| +72.4s | `post_tool_use`                                  | **child** | tool ended                                                                                                                                               |
| +72.3s | `notification` (`idle_prompt` / "Turn complete") | parent    | no-op — `GrokDriver` does not declare `AwaitingInputSignal` (`grok.rs:229`, `:264`), which is the only reason this is not also a false `WaitingForInput` |
| +77.7s | **`session_end` (`reason: shutdown`)**           | **child** | **`Terminated`** — worker is alive and about to run another turn                                                                                         |
| +77.6s | `user_prompt_submit`                             | parent    | `Working` (the subagent result injected as a `<system-reminder>`)                                                                                        |
| +84.9s | `stop` (`reason: end_turn`)                      | parent    | `Idle` — the **second** `Stop` for one human prompt                                                                                                      |

So one human turn produces two `Stop` events and one spurious `Terminated`, with a `UserPromptSubmit` in between that no human sent.

### The documented mitigation does not work

Grok's bundled hooks guide (`~/.grok/docs/user-guide/10-hooks.md`) states that `Stop` input carries `backgroundTasks` "so a hook can distinguish 'session is done' from 'session is paused waiting for background work to wake it back up'", with entries of `type: subagent`.

Measured: at +9.1s, with a background subagent **already started** (`subagent_start` fired at +6.4s) and about to run for another 70 seconds, the parent's `Stop` reported:

```json
{
  "reason": "end_turn",
  "stopHookActive": false,
  "lastAssistantMessage": "PARENT_DONE_EARLY",
  "backgroundTasks": []
}
```

(the `top_level.stop_with_live_background_subagent` entry of [`hook_payloads.json`](./grok-subagent-hook-attribution-artifacts/hook_payloads.json).) The array is empty at exactly the moment it would have been load-bearing, so it cannot be used to suppress a false idle at the parent's Stop.

## The other correction: a Grok subagent is in-process, so descendant-walk suppression is inert

The review discussion that spawned this work recorded that `tools/boss/engine/core/src/background_children.rs` "would suppress false-idle nudging for a Grok subagent child exactly as it does for Claude's `subagent_type: "fork"` children today". **That premise is false.** It was measured two ways:

1. **Hook process ancestry.** Every hook invocation — parent's and subagent's alike — is forked directly from the _same_ `grok` pid. There is no intermediate process. (`scripts/dump_hook.py` records a `ps` pid→ppid walk on every event.)
2. **Direct descendant sampling.** The descendant-sampling section of [`timelines/tui_child_fails.md`](./grok-subagent-hook-attribution-artifacts/timelines/tui_child_fails.md) samples the grok pid's live descendant tree once a second for the whole run, mirroring `count_live_descendants`' walk. Over 62 samples spanning a subagent that ran for ~35s: **35 samples returned 0 descendants**, and the only non-zero samples are the transient `/bin/bash … sleep 25` of the child's own shell tool call. There is no persistent subagent process at any point.

`count_live_descendants` (`background_children.rs:55`) therefore returns 0 whenever a Grok subagent is thinking or waiting on a model call — precisely the windows in which suppression would be needed. Claude's case is structurally different and the module docs say so (`background_children.rs:4-12`): its subagents are _separate `claude` processes_ that persist for the whole delegated turn.

A corollary, measured directly: a Grok subagent **cannot outlive its parent**. SIGKILLing the `grok` process 12s into a background subagent's `sleep 45` left the child's target file unwritten and produced **no further hooks at all** — pure silence, the same shape a hard-killed Claude worker produces ([`timelines/tui_kill.md`](./grok-subagent-hook-attribution-artifacts/timelines/tui_kill.md)). No spurious `session_end` is emitted on a kill.

## What _is_ sound

### Permission interception (question 2)

The probe wired one `PreToolUse` guard into the global hook set, shaped like Boss's own — appended onto the same `PreToolUse` array after the observer, returning Grok's native `{"decision": "deny"}` (the only vocabulary that blocks; see the decision-vocabulary investigation). It denies any shell call whose input contains a marker string.

The subagent was instructed to run three shell commands, the middle one carrying the marker, and told not to stop early. Result ([`timelines/tui_happy.md`](./grok-subagent-hook-attribution-artifacts/timelines/tui_happy.md)):

| Call                                                  | Session          | Guard decision | File on disk |
| ----------------------------------------------------- | ---------------- | -------------- | ------------ |
| `echo CHILD_ALLOWED > child_allowed.txt`              | child            | `allow`        | present      |
| `echo PROBE_FORBIDDEN_PAYLOAD > child_forbidden.txt`  | child            | **`deny`**     | **absent**   |
| `echo CHILD_AFTER > child_after.txt`                  | child            | `allow`        | present      |
| `echo PARENT_ALLOWED > parent_allowed.txt`            | parent (control) | `allow`        | present      |
| `echo PROBE_FORBIDDEN_PAYLOAD > parent_forbidden.txt` | parent (control) | **`deny`**     | **absent**   |

The denied child call also has a `pre_tool_use` with **no** matching `post_tool_use` in the timeline — the call never executed. Interception on a subagent is indistinguishable from interception on the top-level session, which is the desired result.

### Transcript separation and turn boundary (question 3, the half that works)

- The subagent has its own `sessionId` and its own `transcriptPath` under `$GROK_HOME/sessions/<encoded-cwd>/<subagent-id>/updates.jsonl`. It does not write into the parent's transcript.
- The subagent's turn end fires **`subagent_stop`**, not `stop` — confirmed in all three completing probes. `subagent_stop` is not wired by Boss and would normalise to `NormalizeError::UnknownEvent` anyway (`grok/progress.rs:59`, and the existing `documented_but_unimplemented_events_are_ignored_with_logging_not_rejected` test), so it cannot corrupt turn accounting.
- `subagent_start` fires on the **parent's** `sessionId` and carries `subagentId` / `subagentType` / `description` — i.e. the correlation data a fix would need already exists on the wire (the `subagent_start` entry of [`hook_payloads.json`](./grok-subagent-hook-attribution-artifacts/hook_payloads.json)).
- A subagent whose tool call **fails** changes none of this: a child whose `sleep 25 && exit 7` exited non-zero still emitted a plain `post_tool_use` (no `post_tool_use_failure`), then `subagent_stop`, then the same `session_end { reason: "shutdown" }` ([`timelines/tui_child_fails.md`](./grok-subagent-hook-attribution-artifacts/timelines/tui_child_fails.md)). The failure path is not a distinct signature.

## What would have to change before `--no-subagents` can be removed

Roughly in dependency order. This is the fix sketch, not a design:

1. **Make session identity part of ingress, not just payload decoration.** Boss must know the top-level `sessionId` for a run (it already assigns it — `--session-id`, `grok.rs:178`) and either drop or re-tag hook events whose `session_id` is not it. `extract_session_identity` (`tools/boss/protocol/src/worker_event.rs:166-171`) already surfaces the value; nothing downstream compares it. This is the smallest change that removes the false `Terminated`, and it is driver-generic enough to want a deliberate design rather than a Grok-local hack.
2. **Wire `SubagentStart` / `SubagentStop`** into `GROK_HOOK_EVENTS` (`grok/hooks.rs:61-69`) and give them `WorkerEvent` variants, so "a subagent is in flight" becomes a state Boss models rather than infers. Both already reach `EVENT_NAME_MAP` (`grok/progress.rs:58-59`) and currently normalise to `UnknownEvent`.
3. **Replace the descendant-walk assumption for this driver.** `background_children.rs` cannot see an in-process subagent, and `Stop.backgroundTasks` is empty when it matters, so false-idle suppression for a Grok worker with a live background subagent needs the tracked `SubagentStart`/`SubagentStop` pair from (2) as its input instead.
4. **Decide what a second `Stop` per human turn means** for the completion path, since a background subagent's result is injected as a fresh `UserPromptSubmit` and produces one.

Until at least (1) and (3) exist, enabling subagents trades a capability Boss does not need for a lifecycle signal Boss actively misreads.

## Method / apparatus

| Layer         | What                                                                                                                                                                                                                                                                                                                        |
| ------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Scratch root  | `$HOME/.cache/grok-subagent-hook-probe/` — deliberately **not** under `/tmp` (every sandbox profile makes `/tmp` writable; see the permission-isolation investigation)                                                                                                                                                      |
| Isolated home | `$PROBE/home` with byte-copied `auth.json`, `config.toml` byte-identical to `render_base_config_toml()` (`grok/home.rs:227`), and a pre-seeded `trusted_folders.toml`                                                                                                                                                       |
| Scoped `HOME` | `$PROBE/claude_home`, so the operator's `~/.claude` permission rules are not in force                                                                                                                                                                                                                                       |
| Hook wiring   | `scripts/setup_home.py` — Boss's exact `GROK_HOOK_EVENTS` set (`grok/hooks.rs:61-69`) wired to a dump-all observer, plus `SubagentStart`/`SubagentStop`/`PostToolUseFailure`/`PermissionDenied` to see whether they fire at all; one deny guard appended onto the same `PreToolUse` array, matching `write_hooks`' ordering |
| Worker shape  | `scripts/run_tui_probe.py` runs the **real pane command** from `build_grok_pane_command` (`grok.rs:155-218`) under a pty — `--no-alt-screen --always-approve --trust --session-id --cwd --no-memory` + positional prompt — with `--no-subagents` omitted, since that is the flag under test                                 |
| Cross-check   | `scripts/run_probe.sh` runs the same prompts headless (`-p --output-format json`); the event sequence matches the TUI's in every respect that matters here                                                                                                                                                                  |
| Kill case     | `scripts/run_kill_probe.py` SIGKILLs the `grok` pid a fixed delay after `subagent_start`                                                                                                                                                                                                                                    |
| Model         | `grok-4.5`. `grok-code-fast-1` is retired and silently redirects — never a probe target                                                                                                                                                                                                                                     |

Probes, each with its full hook timeline, guard decisions and resulting cwd under [`timelines/`](./grok-subagent-hook-attribution-artifacts/timelines/):

| Probe                                                                                        | What it establishes                                                                      |
| -------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| [`tui_happy`](./grok-subagent-hook-attribution-artifacts/timelines/tui_happy.md)             | Blocking subagent: hook inheritance + permission interception                            |
| [`tui_bg_outlives`](./grok-subagent-hook-attribution-artifacts/timelines/tui_bg_outlives.md) | Background subagent outliving the parent's turn — the blocking finding                   |
| [`tui_child_fails`](./grok-subagent-hook-attribution-artifacts/timelines/tui_child_fails.md) | Child tool call exits non-zero; also the descendant-process sampling                     |
| [`tui_kill`](./grok-subagent-hook-attribution-artifacts/timelines/tui_kill.md)               | Hard kill mid-subagent: can a child outlive its parent, and what reaches the hook stream |
| [`subagent_happy`](./grok-subagent-hook-attribution-artifacts/timelines/subagent_happy.md)   | Headless (`-p`) cross-check of the blocking-subagent case                                |

Decisive raw payloads are collected in [`hook_payloads.json`](./grok-subagent-hook-attribution-artifacts/hook_payloads.json); the pinned CLI version and the exact probe argv are in [`cli.txt`](./grok-subagent-hook-attribution-artifacts/cli.txt).

## Version note

Every prior Grok investigation in this directory is pinned to `grok 0.2.112` / `0.2.114`. This one ran against **`grok 1.0.0`**. `--no-subagents` still exists and still parses; the `PreToolUse` deny vocabulary and the camelCase envelope are unchanged. Nothing here re-validates the rest of the 0.2.x findings — that is `T-20`'s version-pin job, not this probe's.
