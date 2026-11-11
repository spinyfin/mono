# GhosttyKit + Antigravity (`agy`) pane viability spike

- **Date:** 2026-11-11
- **Kind:** empirical gate study — throwaway apparatus, no Boss integration, no `AgyDriver` code
- **Versions under test:**
  - `agy` **1.1.12** — the version the design names; sha256 `c8fd3c00…d4d344`, recovered from the copy the updater left behind
  - `agy` **1.5.5** — what the host runs since the binary replaced itself on the morning of the study; sha256 `42b591f2…f4d6f3`
  - **GhosttyKit** `ghosttykit-71c2d68e`, the `MODULE.bazel` `@ghostty_kit` pin, linked from Bazel's fetched copy
  - tmux 3.8, macOS 27.0.1 (arm64), one signed-in account (banner: `Google AI Ultra`), model `gemini-3.8-flash-low`
- **Related:** [design](../designs/antigravity-driver-fourth-driver-google-gemini-cli.md) · [Grok spike](./ghostty-grok-pane-viability.md) · [worker prompt delivery](../worker-prompt-delivery.md)
- **Artifacts:** [`ghostty-agy-pane-viability-artifacts/`](./ghostty-agy-pane-viability-artifacts/) (apparatus sources, sanitized evidence)

`agy` runs as an interactive TUI worker in a GhosttyKit-hosted pane, on both versions tested, so the gate passes. The design cannot be built as written: six findings change it, and one of them is in shared engine code that has to land before any `agy` worker can be driven.

## Verdict (read this first)

### Is interactive-TUI-in-a-GhosttyKit-pane viable for `agy`?

**Yes.**

Hooks fire in interactive mode. A scoped `HOME` holds up for a long-lived TUI and its tool children. Boss's unmodified initial-brief launcher delivers a 50 KB brief. Surface text is readable through the attached GhosttyKit viewer. Escape cancels a turn and the pane takes the next prompt. Neither of the two outcomes that would have stopped the project occurred: hooks silent in interactive mode, or scoped `HOME` breaking in a pane.

"Viable" here means the pane topology works. It does not mean the design can be built as written.

### What has to change before it can be built

| #   | Finding                                                                                                                                                                                                     | Consequence                                                                                                                                                                   |
| --- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | `tmux send-keys C-m` is **not Return** to `agy` under Boss's tmux options. It arrives as `ESC [27;5;109~`. `send-keys Enter` arrives as `\r` and works.                                                     | Three engine call sites end a delivery with `C-m`. Every probe, nudge, answer and Return-retry would sit unsubmitted in the composer. Shared-code change, outside the driver. |
| 2   | The credential is in the **macOS login keychain**, not in `~/.gemini/oauth_creds.json`. Neither binary references that file.                                                                                | Per-run `HOME` needs the keychain bridge Boss already uses for Grok's `gh`, not file links. The design's credential section and topology diagram are wrong.                   |
| 3   | A `PreToolUse` hook that replies `{}` **denies the tool**. So does a non-zero exit or malformed output. Only empty stdout or an explicit `allow` lets the tool run.                                         | The progress forwarder can stay silent, but must never print an empty object, and a forwarder that exits non-zero blocks the tool call it was reporting.                      |
| 4   | An Escape-cancelled turn fires **no `Stop` hook**, on either version, for tool calls and for generation.                                                                                                    | The Grok trap. Interrupt recovery is required, not optional.                                                                                                                  |
| 5   | `Stop` is not a turn boundary by itself. A command that outlives the tool's wait becomes a background task; `Stop` fires with `fullyIdle=false`, and the agent **invokes itself again** when the task ends. | Only `Stop` with `fullyIdle=true` is idle.                                                                                                                                    |
| 6   | A fresh `HOME` boots into onboarding, a pre-checked data-collection consent box and a workspace-trust dialog, and replaces its own binary on first run.                                                     | Provisioning must pre-seed two files and export `AGY_CLI_DISABLE_AUTO_UPDATE=true` (`=1` is ignored).                                                                         |

### The nine checklist items

| #   | Question                                                                           | Answer                                                                                                                                                        | Versions |
| --- | ---------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------- |
| 1   | Does `-i "<prompt>"` auto-submit in a pty?                                         | **Yes**, in a tmux pane and in a direct GhosttyKit pty. Do not use it: see Q1.                                                                                | both     |
| 2   | Do hooks fire in interactive mode?                                                 | **Yes.** `SessionStart`, `PreInvocation`, `PostInvocation`, `PreToolUse`, `PostToolUse`, `Stop`.                                                              | both     |
| 3   | Is `--add-dir` required for discovery? Does an out-of-repo control directory work? | Not required for `<cwd>/.agents/`. A control directory outside the repo works with `--add-dir`. So does `$HOME/.gemini/config/hooks.json`, with no flag.      | both     |
| 4   | Does workspace trust independently gate discovery?                                 | Only for `<cwd>/.agents/`. Hooks from an `--add-dir` directory and from the global root load whether or not anything is trusted.                              | both     |
| 5   | Does Esc emit `Stop` or skip it?                                                   | **Skips it.** 4 of 4 interrupted turns, plus two re-runs on 1.5.5.                                                                                            | both     |
| 6   | Is mid-turn pane input buffered or rejected?                                       | **Buffered**: held as a queued message and run when the turn ends.                                                                                            | both     |
| 7   | Which `terminationReason` values are emitted?                                      | Observed: `NO_TOOL_CALL` (with `fullyIdle` true and false) and `TERMINAL_CUSTOM_HOOK` (1.5.5 only). `USER_CANCELED` exists in the enum and was never emitted. | both     |
| 8   | Does scoped `HOME` hold up in a pane?                                              | **Yes**, given the keychain bridge. All state, hooks and tool children stay under it; the real `~/.gemini` was not touched.                                   | both     |
| 9   | Banner / busy / prompt / starting strings?                                         | Measured; see [Proposed `PaneMonitorSpec`](#proposed-panemonitorspec).                                                                                        | both     |

## Why this spike exists

Depth-0 gate for the Antigravity driver: can `agy` run as a first-class interactive TUI worker in a GhosttyKit pane, the way `claude`, `codex` and `grok` do? The design marks everything in its spike checklist as unconfirmed and therefore gating, and says a negative answer stops the project rather than prompting a workaround.

**Hard apparatus rule:** the answer to each of the nine checklist items comes from sessions with a GhosttyKit surface attached. Nothing rests on a standalone terminal emulator, and headless `agy -p` was not used at all.

Three supporting measurements were taken in a tmux pane with no viewer attached, and are labelled where they appear: the raw start-up stream, the key-byte probe, and the onboarding screens, which were walked through by hand. The credential and updater probes are one-shot `agy models` commands, not pane measurements.

## Method / apparatus

### The topology has changed since the Grok spike

The Grok spike hosted the agent directly in a GhosttyKit pty and seeded it with a positional prompt. Boss no longer works that way. A worker today is:

1. a **tmux session** on Boss's private server, created by the engine;
2. running the **initial-brief launcher** (`engine/driver/src/initial_prompt.py`), which owns the driver's pty, waits for the composer, bracketed-pastes the brief and writes Return;
3. viewed by a **GhosttyKit surface** in the app whose shell runs `exec tmux -S <socket> attach-session -t <session>`;
4. controlled by the engine through `tmux send-keys` and `paste-buffer`, and scraped by the app through `ghostty_surface_read_text`.

The design predates this and still draws `shell → agy -i "$(cat initial-prompt.txt)"`. The spike tests the topology that ships.

### Layers

| Layer           | What ran                                                                                                                                                                          | Same as Boss?                                                                                       |
| --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| tmux server     | Private socket; `exit-empty off`, `history-limit 2000`, `remain-on-exit on`, `extended-keys on`, `terminal-features[100] xterm*:extkeys`, `focus-events on`; session `status off` | Options copied from `boss_tmux`                                                                     |
| Session command | `export HOME=<scoped>; export AGY_CLI_DISABLE_AUTO_UPDATE=true; exec python3 initial-prompt.py agy …`                                                                             | Launcher is the repository file, byte for byte                                                      |
| Viewer          | AppKit host, one GhosttyKit surface, initial input `exec tmux -S … attach-session -t …`                                                                                           | Same GhosttyKit calls as `GhosttyRuntime` and `GhosttyTerminalHostView`                             |
| Control         | `send-keys -l`, `send-keys Enter` / `C-m` / `Escape`, `load-buffer` + `paste-buffer -p -d`                                                                                        | Same argv as `Tmux::send_keys` / `send_key`, except where the submit key is the variable under test |
| Observation     | `tmux capture-pane -p` every 200 ms; viewport text every 250 ms; a hook on every event recording stdin and environment; `agy --log-file`                                          | —                                                                                                   |

One scenario (`iflag direct`) removes tmux and runs `agy` in the surface's own pty, the Grok spike's shape.

The viewer window is created off-screen and never ordered front. It reads and types through GhosttyKit without being visible, as a worker pane on a background tab does.

### What was not exercised

No Boss app, no engine, no dispatch. Spawn confirmation, the events socket, `boss-event` and the guard scripts did not run. The design's rule that acceptance evidence must come from real Boss dispatch is not met by this study and is not meant to be.

### Volume

61 scenario sessions (30 on 1.1.12, 31 on 1.5.5), 338 recorded hook payloads, 67 `Stop` payloads, 1,586 classified pane polls. On 1.5.5 the transcripts account for 53 model invocations and about 479k input tokens on Gemini 3.8 Flash (Low); 1.1.12 transcripts carry no token counts, and a similar volume is assumed. Ten further 1.5.5 sessions re-ran scenarios from the apparatus as committed, to check it; they are not in these counts.

### Two versions, and why

The design names 1.1.12. On the morning of the study the installed binary replaced itself with 1.5.5 during an unrelated session (`01-versions-and-updater.txt`). A gate run only on a version the host no longer runs would answer the wrong question; a gate run only on 1.5.5 would not answer the one asked. Every scenario ran on both, from private copies. They agree on all nine checklist items. Where they differ it is called out.

---

## Q1 — Does `-i "<prompt>"` auto-submit in a pty?

**Yes, on both versions, in both topologies.** No key was sent by the host; `SessionStart` fired 1.0–3.1 s after `agy` detected its terminal, the turn ran to `Stop`, and the TUI stayed interactive for a typed follow-up. Evidence: `05-prompt-interactive-flag.txt`.

**It should not be Boss's seeding path**, for two reasons.

- Boss does not pass briefs in argv. The launcher exists so the brief never appears in a process listing, and so that delivery is confirmed by evidence.
- `-i` runs the prompt **behind the workspace-trust dialog**. With an untrusted workspace the dialog was on screen and unanswered while the turn ran to completion, and with `--dangerously-skip-permissions` a shell tool wrote a canary file behind it. See Q4.

### The path Boss actually uses: launcher paste

The unmodified launcher delivered every brief it was given on 1.5.5 (15 of 15 sessions, plus 5 re-runs) and on 1.1.12 once its readiness marker was right (14 of 14).

| Interval                             | 1.5.5                  | 1.1.12                 |
| ------------------------------------ | ---------------------- | ---------------------- |
| Pane spawn to composer ready         | 0.49–2.76 s            | 1.83–11.96 s           |
| Writing the paste (174 B – 49.8 KB)  | 53–102 ms              | 53–68 ms               |
| Paste written to composer showing it | 7–19 ms                | 7–344 ms               |
| Landing evidence                     | `literal` or `repaint` | `literal` or `repaint` |

The 49,782-byte, 458-line brief arrived intact: `lastUserInput` in the `Stop` payload is 49,781 characters. It includes a line ending in `.png`, the shape that makes Claude Code defer a paste; `agy` did not defer it.

`agy` enables bracketed paste (`CSI ?2004h`) and raw mode before its composer appears, so the launcher's readiness conditions are satisfiable. A multi-line paste shows in the composer as literal text; there is no placeholder, so no `paste_landed_markers` entry is needed.

### 1.1.12 loses a brief submitted too early

On 1.1.12 the composer appears about 0.9 s before the account's plan is known. A prompt submitted in that window is accepted by the input loop and then discarded with `⚠ Verifying your account... Please try again shortly.` The composer is left empty, so a Return retry does nothing, and no hook ever fires. The launcher had already written `initial-prompt.delivered`.

The only on-screen sign that the window has closed is the plan name appearing after the account in the banner. Using that as the readiness marker fixed it (14 of 14). The message is absent from the 1.5.5 binary, and 1.5.5 accepted briefs submitted as early as 0.49 s after spawn. Evidence: `04-launcher-delivery.txt`.

## Q2 — Do hooks fire at all in interactive mode?

**Yes.** This was the design's first risk and the one that would have stopped the project.

Order for a turn with one tool call:

```text
SessionStart → PreInvocation → PreToolUse → PostToolUse → PostInvocation → PreInvocation → PostInvocation → Stop
```

- `SessionStart` fires once, when the first prompt of a conversation is submitted, not when the CLI starts. Both binaries fire it; the 1.1.12 embedded guide does not list it.
- Hooks are synchronous and run with the directory holding `hooks.json` as cwd. The hook process sees the scoped `HOME` and `ANTIGRAVITY_CONVERSATION_ID`.
- Every payload carries `conversationId`, `transcriptPath`, `artifactDirectoryPath`, `modelName` and `workspacePaths`. 1.5.5 adds `hookEventName`, `cwd`, `lastUserInput` and `toolCall.id`. On 1.1.12 a single hook command cannot tell which event it is handling from the payload alone.
- `lastUserInput` is the **whole prompt, on every event**. With a 50 KB brief each hook payload is 51 KB.
- `PostToolUse` carries `error` (empty on success) and no exit status or output.

Samples: `09-hook-payloads.json`.

## Q3 — Is `--add-dir` required, and does an out-of-repo control directory work?

Eight cells per version; each is a fresh scoped `HOME` with an `-i` prompt. Evidence: `06-hook-discovery-and-trust.txt`.

| Cell | `hooks.json` at         | Trusted before launch | `--add-dir` | Trust dialog | Hooks fired       |
| ---- | ----------------------- | --------------------- | ----------- | ------------ | ----------------- |
| d1   | `<cwd>/.agents/`        | cwd                   | no          | no           | yes               |
| d2   | `<cwd>/.agents/`        | nothing               | no          | yes          | **no**            |
| d3   | `<control>/.agents/`    | cwd                   | control     | no           | yes               |
| d4   | `<control>/.agents/`    | cwd + control         | control     | no           | yes               |
| d5   | `<control>/.agents/`    | cwd + control         | no          | no           | no (control case) |
| d6   | `$HOME/.gemini/config/` | cwd                   | no          | no           | yes               |
| d7   | `$HOME/.gemini/config/` | nothing               | no          | yes          | yes               |
| d8   | `<control>/.agents/`    | control only          | control     | yes          | yes               |

Identical on both versions.

- `--add-dir` is **not** required when the workspace is trusted and `hooks.json` is in its own `.agents/`.
- A control directory outside the repo **works** when passed with `--add-dir` (d3, d4), and is ignored when not passed (d5).
- The **global customization root** `$HOME/.gemini/config/hooks.json` works with no flag (d6). Under a Boss-owned per-run `HOME` that file is Boss-owned.

The global root is the better home for Boss's hooks. `--add-dir` puts the control directory in the agent's workspace (`workspaceDirs=[repo control]`), which invites the agent to read and edit it. The global root adds nothing to the workspace and needs no extra argument.

## Q4 — Does workspace trust independently gate discovery?

**Partly.** Trust is evaluated for the primary working directory only.

- `<cwd>/.agents/hooks.json` is **not loaded** while cwd is untrusted (`loaded 0 named hooks from 0 hooks.json file(s)`), and is loaded the moment trust is granted (d2).
- Hooks from an `--add-dir` directory load whether or not that directory, or cwd, is trusted (d3, d8).
- Global hooks load regardless (d7).

Trust is stored as `trustedWorkspaces`, a list of absolute symlink-resolved paths, in `$HOME/.gemini/antigravity-cli/settings.json`. Pre-seeding it suppresses the dialog. `~/.gemini/trustedFolders.json` is not read by either binary.

### The trust dialog does not stop an `-i` turn

In d2, d7 and d8 the conversation was created within four seconds of start, while the dialog was on screen. In d7 and d8 the turn's hooks fired with the dialog still up: all four of them in three of the four runs, and in the fourth `Stop` landed within 10 ms of the keypress that answered it. In d2 the turn ran with **no hooks at all**, because the only hooks were in the untrusted workspace.

A further scenario left the dialog unanswered and asked for a shell command, with `--dangerously-skip-permissions`:

```text
dialog_still_up=1 canary=present hooks=SessionStart PreInvocation PreToolUse PostToolUse PostInvocation PreInvocation PostInvocation Stop
```

The tool ran and wrote its file with the dialog still up. Selecting "No, exit" afterwards exited cleanly and left `settings.json` unchanged. Both versions.

For Boss this means an `-i`-seeded worker in an untrusted workspace with in-repo hooks would run its first turn with no progress ingress and no `PreToolUse` guards, behind a dialog. The launcher path does not have this failure: it treats the dialog as a starting screen, holds the brief, and reports `initial-prompt.undelivered` after 30 s (exit 64, nothing pasted). Both versions.

## Q5 — Does Esc emit `Stop` or skip it?

**It skips it.** One `tmux send-keys Escape` cancelled the turn every time; the pane was idle 0.3–0.9 s later; no `Stop`, `PostInvocation` or `PostToolUse` fired for the cancelled turn.

| Version | Interrupted during   | Presses | Pane idle after | `Stop` from the cancelled turn | Next prompt                |
| ------- | -------------------- | ------- | --------------- | ------------------------------ | -------------------------- |
| 1.1.12  | `sleep 60` tool call | 1       | 0.84 s          | none                           | ran, `Stop` `NO_TOOL_CALL` |
| 1.1.12  | long generation      | 1       | 0.73 s          | none                           | ran                        |
| 1.5.5   | `sleep 60` tool call | 1       | 0.32 s          | none                           | ran                        |
| 1.5.5   | long generation      | 1       | 0.29 s          | none                           | ran                        |

The `sleep 60` child was gone after the cancel. The process survived and stayed interactive.

What does record the cancel:

- **The pane.** `⎿  Interrupted · What should Antigravity CLI do instead?` above an idle composer.
- **`agy`'s log.** `Cancelling in-progress response for conversation <cid>`, within 50 ms of the key. `--log-file` puts that log at a path the driver chooses.

What does not: `transcript_full.jsonl`. The cancelled tool step is simply missing, and a cancelled generation is recorded as `DONE`.

Evidence: `07-escape-interrupt.txt`.

## Q6 — Is mid-turn pane input buffered or rejected?

**Buffered.** Text typed while a turn is in flight goes into the composer. On Enter it becomes a queued message, shown as `▸ <text> (alt+m to send immediately)` with the footer `Press up to edit queued messages`. `agy` logs `HandleUserInput` at once. The message runs when the current execution ends: its `PreInvocation` followed the first `Stop` by 0.2 s on 1.5.5 and 0.5 s on 1.1.12.

This is the `MidTurnPaneInput::Buffers` shape: the foreground process consumes the bytes; nothing is left in the tty for a shell.

Two caveats.

- The queue is the default of a `/config` setting, "Queued Messages", whose other value is "Send Immediately". Only the default was tested.
- "When the turn ends" means the next `Stop`, including one with `fullyIdle=false`. In the test the queued prompt ran while the first prompt's background command was still running.

Evidence: `08-midturn-input-and-background-tasks.txt`.

## Q7 — What `terminationReason` values does the binary emit?

The `Stop` payload carries the `ExecutorTerminationReason` enum name with its prefix stripped, on both versions. The 1.1.12 embedded guide says `"model_stop"`, `"max_steps_exceeded"`, `"error"`; that binary emits `NO_TOOL_CALL` all the same. The 1.5.5 guide was corrected.

| Value                                                                                                                                                                        | In the enum | Observed           | How                                                                                      |
| ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------- | ------------------ | ---------------------------------------------------------------------------------------- |
| `NO_TOOL_CALL`                                                                                                                                                               | both        | 31 and 35 payloads | Every normal turn end, `fullyIdle` true or false                                         |
| `TERMINAL_CUSTOM_HOOK`                                                                                                                                                       | both        | 1 payload, 1.5.5   | `PreInvocation` hook replying `terminationBehavior: "terminate"`; no model call was made |
| `USER_CANCELED`                                                                                                                                                              | both        | **never**          | Escape produces no `Stop` at all                                                         |
| `ERROR`, `MAX_INVOCATIONS`, `MAX_FORCED_INVOCATIONS`, `MAX_TOKEN_BUDGET_EXCEEDED`, `TERMINAL_STEP_TYPE`, `INJECTED_RESPONSE`, `EARLY_CONTINUE`, `HALTED_STEP`, `UNSPECIFIED` | both        | not triggered      | —                                                                                        |
| `CHAINED`                                                                                                                                                                    | 1.5.5 only  | not triggered      | —                                                                                        |

On 1.1.12 the same `PreInvocation` reply did not terminate the turn: the model was invoked and `Stop` reported `NO_TOOL_CALL`.

`executionNum` was `0` in every `Stop` and `invocationNum` restarts at `0` for each execution. Neither counts turns.

### `fullyIdle` decides whether a `Stop` is a boundary

A `sleep 25` asked for "in the foreground" still became a background task: the model set `WaitMsBeforeAsync: 10000`, and after 10 s the tool returned "running as a background task".

```text
04:07:18.477  PreToolUse    run_command
04:07:28.533  PostInvocation               ← tool went to the background after 10 s
04:07:30.712  Stop  NO_TOOL_CALL fullyIdle=False     "Waiting for sleep 25 to finish."
04:07:43.561  PostToolUse   error=''       ← the command finished
04:07:43.618  PreInvocation                ← the agent invoked itself; nothing was typed
04:07:45.263  Stop  NO_TOOL_CALL fullyIdle=True      "MID_FIRST_DONE"
```

Between the two `Stop`s the composer looks idle (`? for shortcuts`), with a task tray under it and `· 1 task(s) · /tasks` in the footer. Both versions. Any command that outlives the wait the model picks (5 s or 10 s in these runs) takes this path; a build or a test run will.

Evidence: `13-termination-reasons.txt`, `08-midturn-input-and-background-tasks.txt`.

## Q8 — Does scoped `HOME` hold up in a pane?

**Yes**, with one correction to how the credential gets there.

### The credential is in the login keychain

Neither binary contains the strings `oauth_creds`, `google_accounts` or `trustedFolders`. Both link `zalando/go-keyring` and call `/usr/bin/security`. The files the design names under `~/.gemini/` are not `agy`'s.

Under a scoped `HOME`, `security` cannot find the user's keychain (`A default keychain could not be found`), which is why a scoped `HOME` is signed out. One symlink fixes it:

```text
<scoped home>/Library/Keychains/login.keychain-db → <real home>/Library/Keychains/login.keychain-db
```

This is the bridge `grok/environment.rs` already provisions (`LOGIN_KEYCHAIN_LEAF`). With it, both versions log `keyringAuth: loaded token … ChainedAuth: authenticated via keyring`, and `security default-keychain` works from tool children too. No keychain prompt appeared in any session.

Token refresh goes through the same item. The stored token was due to expire mid-study. A scoped session refreshed it at 04:14:07 and wrote it back; a session that was already running in the real home, outside the study, refreshed again at 04:15:51. Later sessions of both versions loaded the newer token. Nothing failed on either side. That is one observation of two refreshers, not a concurrency result.

### State, hooks and tool children

- The whole mutable tree lands under the scoped `HOME`: `.gemini/config/` and `.gemini/antigravity-cli/` (settings, conversation databases, `brain/<cid>/…/transcript_full.jsonl`, presence locks, history, updater state).
- A hook process and a tool child both see `HOME=<scoped>`. A variable exported in the pane shell reached the tool child, so the tool shell does not strip the parent environment the way Grok's does.
- `git config --global user.name` returned nothing from a tool child. Host tool identity (`git`, `gh`, `jj`, `ssh`) needs delegating, as it does for Grok.
- Resizing the GhosttyKit surface (124×37 → 64×19 → 149×44) re-rendered cleanly and the next prompt ran.
- `/quit` exits with status 0 and prints `agy --conversation=<cid>`.

### The real `~/.gemini`

An inventory before the first scoped run and after the last differs only in files owned by the unrelated session noted above: its log, which it holds open for writing, and its conversation-summaries database. No scoped session's log names a path under the real home. Evidence: `11-scoped-home.txt`.

### What a fresh `HOME` shows first

Without pre-seeding, a scoped `HOME` starts in the alternate screen with three blocking screens:

1. `Welcome to Antigravity CLI!` — colour scheme.
2. `Terms of Service & Data Use` — a **pre-checked** box agreeing to let Google collect and use "Interactions data".
3. `Accessing workspace:` … `Do you trust the contents of this project?`

Writing two files first skips all three, on both versions:

```text
$HOME/.gemini/antigravity-cli/cache/onboarding.json
  {"consumerOnboardingComplete": true, "enterpriseOnboardingComplete": false, "onboardingComplete": true}
$HOME/.gemini/antigravity-cli/settings.json
  {"trustedWorkspaces": ["<absolute, symlink-resolved workspace path>"]}
```

### The updater

A fresh `HOME` has no update-check stamp, so the first `agy` run in it checks for an update and replaces its own executable. This happened to the study's 1.1.12 copy on its first invocation: 1.1.12 became 1.5.5 in seconds, under a scoped unauthenticated `HOME`, with `AGY_CLI_DISABLE_AUTO_UPDATE=1` set.

| Lever                              | Effect                                                                                                   |
| ---------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `AGY_CLI_DISABLE_AUTO_UPDATE=1`    | **Ignored.** The update ran.                                                                             |
| `AGY_CLI_DISABLE_AUTO_UPDATE=true` | `Auto-update disabled via environment variable` (both versions)                                          |
| Binary directory not writable      | `Directory … is not fully accessible (readable: true, writable: false), skipping update` (both versions) |

By the same mechanism every worker that starts in a fresh `HOME` would attempt to replace the one shared binary. That was not tried with concurrent runs. Evidence: `01-versions-and-updater.txt`.

## Q9 — Banner / busy / prompt / starting strings

Counted per poll, with phases taken from hook timestamps rather than from the screen. 16 sessions per version. The two sources agree to within sampling; the table shows `tmux capture-pane`. Full tables for both sources: `12-pane-markers.txt`.

| Substring                          | 1.5.5 busy  | 1.5.5 idle  | 1.5.5 bg-wait | 1.1.12 busy | 1.1.12 idle | 1.1.12 bg-wait |
| ---------------------------------- | ----------- | ----------- | ------------- | ----------- | ----------- | -------------- |
| `esc to cancel`                    | 205/235     | 0/243       | 0/52          | 513/542     | 0/270       | 0/55           |
| `Press up to edit queued messages` | 30/235      | 0/243       | 0/52          | 29/542      | 0/270       | 0/55           |
| either of the two above            | **235/235** | 0/243       | 0/52          | **542/542** | 0/270       | 0/55           |
| `? for shortcuts`                  | 0/235       | **243/243** | 52/52         | 0/542       | 264/270     | 55/55          |
| `task(s) · /tasks`                 | 12/235      | 0/243       | **52/52**     | 11/542      | 0/270       | **55/55**      |
| `Antigravity CLI` (banner)         | 224/235     | 212/243     | 52/52         | 514/542     | 236/270     | 55/55          |
| line starting with `>`             | 235/235     | 242/243     | 52/52         | 542/542     | 269/270     | 55/55          |
| `Generating...`                    | 87/235      | 0           | 0             | 409/542     | 0           | 0              |
| `Running command...`               | 143/235     | 0           | 0             | 0/542       | 0           | 0              |

"bg-wait" is the stretch after a `Stop` with `fullyIdle=false`.

Readings:

- The left of the footer is the state indicator: `? for shortcuts` when idle with an empty composer, `esc to cancel` when busy, `Press up to edit queued messages` when busy with a queued message. It is blank while the composer holds unsent text. Every idle miss, in both sources, is such a frame: a probe caught half-typed.
- The banner scrolls out as soon as a long brief is echoed. It is not a durable agent marker.
- Spinner labels differ by version (`Running command...` is 1.5.5 only). The footer does not.
- The composer line is a bare `>` with no placeholder text, so "has input" is decidable. User messages in history are also echoed with a leading `>`; the composer is always the lowest such line.
- `esc to cancel` also shows with the slash-command menu open and on the tool-approval dialog. A pane waiting for approval reads as busy.

Starting screens, all captured: `Signing in...` under `Welcome to the Antigravity CLI. You are currently not signed in.` (about 0.7 s in the starts inspected frame by frame), the sign-in picker (`Select login method:`), the three onboarding screens, and the trust dialog. Evidence: `03-startup-screens.txt`.

### Proposed `PaneMonitorSpec`

```text
agent_markers        ["? for shortcuts", "esc to cancel", "Press up to edit queued messages"]
busy_markers         ["esc to cancel", "Press up to edit queued messages", "task(s) · /tasks"]
starting_markers     ["Signing in..."]
prompt_prefixes      [">"]
idle_debounce_polls  2

launcher_starting_markers
                     ["Welcome to Antigravity CLI!", "Choose your color scheme", "Terms of Service & Data Use",
                      "Accessing workspace:", "Do you trust the contents of this project?",
                      "You are currently not signed in", "Select login method:"]
paste_landed_markers []
```

Notes on the choices:

- The agent markers are the three footer states. One of them is on screen in 530/530 `tmux capture-pane` polls after start-up on 1.5.5 and 861/867 on 1.1.12; through the GhosttyKit viewport, 508/509 and 827/836. The misses are the half-typed frames above. `Antigravity CLI` is left out on purpose: it appears on the sign-in, onboarding and trust screens, and the launcher uses agent markers to decide the composer is ready.
- `task(s) · /tasks` as a busy marker makes a pane with a background task read as working. That matches what the agent will do next. It also means a deliberately long-lived background process keeps the pane "working"; hooks, not the scrape, should decide.
- Under `AGY_CLI_HIDE_ACCOUNT_INFO=true` (1.5.5) the banner omits the account line. Worth exporting for worker panes.
- This spec is for 1.5.5. It also classifies 1.1.12 correctly, but 1.1.12's launcher readiness needs the plan-name marker described in Q1, which is account-specific.

---

## Beyond the checklist

### The submit key

`agy` asks the terminal for xterm modifyOtherKeys mode 2 at start-up (`CSI > 4 ; 2 m`). Boss's tmux server has `extended-keys on`, so tmux honours the request. Bytes delivered to a pane application, measured with a raw-mode probe on the same server:

| `tmux send-keys` | No mode set | After `CSI > 4 ; 2 m` |
| ---------------- | ----------- | --------------------- |
| `C-m`            | `\r`        | `ESC [27;5;109~`      |
| `Enter`          | `\r`        | `\r`                  |
| `Escape`         | `ESC`       | `ESC`                 |
| `C-c`            | `\x03`      | `ESC [27;5;99~`       |

Against the real composer, on both versions: text followed by `C-m` left the text in the composer for 4 s with no `HandleUserInput` log line and no hook; `Enter` then started the turn. A Return pressed through the GhosttyKit surface (`ghostty_surface_key`, keycode `0x24`) also submits: in the direct pty (`05`), and once through the attached tmux client in the exploratory session.

Boss ends a delivery with `C-m` in three places: `Tmux::send_keys` (`tmux/src/lib.rs`), spawn confirmation's Return retry (`pane_spawn.rs`), and the spawn-ack sweep's `resubmit_prompt` (`engine/app/src/app.rs`). The launcher is unaffected, because it writes a raw `\r` into the pty. So the first brief lands, and nothing the engine types afterwards is ever submitted.

`Enter` produced `\r` in every mode combination measured. Evidence: `02-terminal-modes-and-submit-key.txt`.

### What a `PreToolUse` reply does

With `--dangerously-skip-permissions`, one shell command requested. Identical on both versions.

| Hook reply                | Tool ran | `agy` says                               |
| ------------------------- | -------- | ---------------------------------------- |
| `{"decision":"allow"}`    | yes      | —                                        |
| nothing on stdout, exit 0 | yes      | —                                        |
| `{}`                      | **no**   | `tool call denied by pre-tool hook`      |
| `NOT JSON`                | no       | `failed to unmarshal result from hook …` |
| nothing, exit 1           | no       | `command failed: exit status 1`          |
| nothing, exit 2           | no       | `command failed: exit status 2`          |

- `PreToolUse` fails **closed** on a crashed or malformed hook. That is the opposite of Grok and the property the design asks its adapter to have.
- `boss-event` prints nothing on success, so it does not deny. It exits non-zero when it cannot deliver or buffer an event, and on `PreToolUse` that blocks the tool.
- `allow` does not bypass the permission system. In the default mode (`toolPermission=request-review`) the ordinary approval dialog still appeared after the hook allowed.
- On the other events a malformed reply is logged and ignored; the turn proceeds.

**Every denied case ended the same way:** the model replied with the completion token it had been asked for, and `Stop` reported `NO_TOOL_CALL`, `fullyIdle=true`. On 1.1.12 the pane does not even mark the tool row as failed. This is the design's soft-deny risk, reproduced in interactive mode on both versions. Evidence: `10-hook-reply-semantics.txt`.

### Per-request overhead

On 1.5.5, the first invocation of a one-line prompt in an empty workspace with no rules files cost 8.9k–9.0k input tokens across 23 sessions (`input_tokens` plus `cache_read_tokens` in `transcript_full.jsonl`; the cache-read share was zero in all but one, where it was 4.1k of 8.9k). The 2 KB brief cost 9.5k and the 50 KB brief 20.2k. This is a single account, one model, an empty workspace. The design removed earlier figures of this kind because they had no source; these have one.

## Corrections to the design

| Design says                                                                             | Measured                                                                                                                                      |
| --------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| Subject CLI is 1.1.12                                                                   | The host runs 1.5.5 as of 2026-11-11. 1.1.12 loses early briefs; 1.5.5 does not.                                                              |
| Credential is `oauth_creds.json` + `google_accounts.json`, delegated by filesystem link | Credential is a keychain item. Delegate with the keychain bridge. Refresh serialisation is the keychain's and `agy`'s, not a file lock.       |
| `~/.gemini/settings.json` holds the auth type; `trustedFolders.json` is a trust store   | Neither is read by `agy`. Only `antigravity-cli/settings.json` (`trustedWorkspaces`) matters.                                                 |
| Pane runs `agy -i "$(cat initial-prompt.txt)" --add-dir <control-dir>`                  | Pane runs the launcher. `-i` is unnecessary and runs behind the trust dialog.                                                                 |
| Hooks go in a control directory via `--add-dir`                                         | That works. `$HOME/.gemini/config/hooks.json` works with less and keeps the directory out of the agent's workspace.                           |
| No `SessionStart`; first `PreInvocation` carries the identity claim                     | `SessionStart` fires on both versions and carries `conversationId`.                                                                           |
| `Stop` → `TurnEnd`                                                                      | Only when `fullyIdle=true`.                                                                                                                   |
| Interrupt recovery "if the spike found that Esc-cancelled turns skip `Stop`"            | It found that.                                                                                                                                |
| `MidTurnPaneInput` stays `Rejects` unless proven                                        | Proven `Buffers` under the default queue setting.                                                                                             |
| `boss-event` reused unchanged on all five events                                        | Safe on stdout. A delivery failure on `PreToolUse` blocks the tool; decide whether that is wanted.                                            |
| Effort ladder is three rungs                                                            | 1.5.5 accepts `low\|medium\|high\|xhigh\|max`.                                                                                                |
| Model menu as listed                                                                    | 1.5.5 lists `gemini-4-carbon-*`, `gemini-4-argon-*`, `gemini-3.8-flash-*`, `claude-opus-5-5-*`, `claude-sonnet-5-5-*`, `gpt-oss-120b-medium`. |
| Subscription tier unidentified                                                          | The banner reads `Google AI Ultra`.                                                                                                           |
| `AGY_CLI_DISABLE_AUTO_UPDATE` is "the obvious lever to probe"                           | It works only as `=true`.                                                                                                                     |
| Scoped `HOME` relocates the Playwright cache                                            | 1.1.12 created `Library/Caches/ms-playwright-go` under the scoped `HOME` on `agy models`; 1.5.5 did not.                                      |

## Negatives worth keeping

1. **`C-m` is not Return.** Until the engine can send `Enter`, an `agy` worker accepts its brief and then ignores everything the engine types.
2. **A cancelled turn is silent.** No hook and no transcript record. Only the pane and the log show it.
3. **`Stop` with `fullyIdle=false` looks like a finished turn and is not.** The pane looks idle too.
4. **A hook that says nothing useful denies.** `{}` on `PreToolUse` blocks every tool while every `Stop` still reports a clean turn.
5. **A denied tool does not stop the model claiming success.** Four of four denied cases per version ended with the requested completion token.
6. **The trust dialog is not a barrier.** An `-i` turn, including a shell command, runs behind it.
7. **A fresh `HOME` updates the shared binary.** `=1` does not stop it.
8. **Onboarding includes a pre-checked data-collection consent.** Pre-seeding skips the screen; it does not answer the question the screen asks.
9. **Every hook payload carries the whole prompt.** A 50 KB brief is 51 KB per event through the forwarder.
10. **The version moved during the study.** A pin in a design document does not hold a self-updating binary still.

## Limits of this study

- Not the Boss app and not a Boss dispatch. See [What was not exercised](#what-was-not-exercised).
- Short sessions: the longest turn was under a minute. No sustained output, no context compaction, no long-lived conversation.
- No subagents, no browser subagent, no MCP servers. Whether a subagent's `Stop` carries its own `conversationId` is untested.
- No deliberate concurrency. Sessions were run one at a time, with two incidental overlaps; the two token refreshes were an accident of timing.
- One account, one plan, one model, one host. Footer and banner text for other models was not captured.
- Hundreds of polls per phase, not thousands. The marker counts show no misses where it matters; they do not bound a rare one.
- `--dangerously-skip-permissions` for every tool scenario. No per-worker-kind posture was tested.
- The viewer was never visible or focused. `agy` does not request focus reporting, so this probably does not matter; it was not tested.
- Only `Stop` values the scenarios could reach cheaply were triggered.

## Open questions

1. **Telemetry under a pre-seeded `HOME`.** Onboarding was completed once by hand with the consent box unticked; `settings.json` stayed `{}` and nothing in the log shows the choice being sent anywhere. Pre-seeded homes never see the screen. What data-collection state they run under is not established, and it applies to an account, not a directory.
2. **Refresh under real concurrency.** Sixteen sessions sharing one keychain item, some refreshing at once. The 1.5.5 binary contains `Skipping refreshed token save because stored token changed externally`, which suggests it expects this.
3. **Subagent identity.** Unchanged from the design: there is no flag to disable subagents.
4. **Is the three-site `C-m` change safe for the other drivers,** or should the submit key be declared per driver the way `InterruptGesture.key` is?
5. **Should a forwarder failure on `PreToolUse` block the tool?** Today's `boss-event` exit codes make it so on `agy`.
6. **What reaches the model when a hook denies.** The step error is in the transcript; whether the model's claim of success can be detected from `PostToolUse` absence alone is for the soft-deny work.
7. **`--mode auto` and `--settings`** are new in 1.5.5 and may express per-run posture without a settings file. Untested.
8. **`ANTIGRAVITY_CONFIG_REL_DIR`** appears in the 1.5.5 binary alongside `ANTIGRAVITY_EXECUTABLE_DATA_DIR`. Neither was probed.

## Follow-up work

Code and document changes this study implies. None is made here.

1. **Engine: make the pane submit key correct for `agy`.** Either send `Enter` instead of `C-m` at the three call sites, after checking the three shipped drivers, or add a driver-declared submit key defaulting to `C-m`. Prerequisite for control verbs, spawn confirmation's Return retry, and answer delivery.
2. **Design: revise against these findings.** Credential and topology sections, hook location, `SessionStart`, `fullyIdle`, the subject version, and the task entries that inherit them (per-run `HOME` provisioning, hook wiring, turn boundary).
3. **Provisioning task:** keychain bridge; `onboarding.json` and `trustedWorkspaces` pre-seed; `AGY_CLI_DISABLE_AUTO_UPDATE=true`; a binary location the updater cannot write; `AGY_CLI_HIDE_ACCOUNT_INFO=true`; `--log-file` at a run-private path.
4. **Turn-boundary task:** `TurnEnd` on `Stop` with `fullyIdle=true` only; interrupt recovery keyed on the log line or the pane text.
5. **Hook-wiring task:** global root rather than control directory; forwarder reply and exit-code contract on `PreToolUse`.
6. **Version policy:** qualify against 1.5.5, not 1.1.12, and decide who moves the binary forward.

## Appendix A — Evidence index

| Question                      | Evidence                                    |
| ----------------------------- | ------------------------------------------- |
| Versions, updater             | `01-versions-and-updater.txt`               |
| Terminal modes, submit key    | `02-terminal-modes-and-submit-key.txt`      |
| Start-up, onboarding, sign-in | `03-startup-screens.txt`                    |
| Q1 launcher delivery          | `04-launcher-delivery.txt`                  |
| Q1 `-i`                       | `05-prompt-interactive-flag.txt`            |
| Q3, Q4                        | `06-hook-discovery-and-trust.txt`           |
| Q5                            | `07-escape-interrupt.txt`                   |
| Q6, `fullyIdle`               | `08-midturn-input-and-background-tasks.txt` |
| Q2 payloads                   | `09-hook-payloads.json`                     |
| Hook replies                  | `10-hook-reply-semantics.txt`               |
| Q8                            | `11-scoped-home.txt`                        |
| Q9                            | `12-pane-markers.txt`                       |
| Q7                            | `13-termination-reasons.txt`                |

All under [`ghostty-agy-pane-viability-artifacts/evidence/`](./ghostty-agy-pane-viability-artifacts/evidence/).

## Appendix B — Re-running

See the artifacts [`README.md`](./ghostty-agy-pane-viability-artifacts/README.md). A scenario needs a signed-in `agy` account on the host and spends its quota.

## Appendix C — What the study touched on the host

- Ran private copies of both binaries from a scratch directory; the installed `agy` and the `.old` file beside it are unchanged (checksums in `01`).
- Created scratch `HOME`s holding a symlink to the login keychain. Through it, one scoped session refreshed the account's stored token.
- Ran the installed binary three times at the outset for `--version`, `--help` and `changelog`; `changelog` rewrote a cache file under the real `~/.gemini/antigravity-cli/cache/`.
- Read, and did not modify: directory listings under the real `~/.gemini`; `antigravity-cli/settings.json` and `cache/onboarding.json`, to learn their shape; the two small Gemini CLI files `settings.json` and `state.json`; and `antigravity-cli/log/`, to learn how the version changed. The credential files there were not opened.
- Completed onboarding once in a scratch `HOME`, with the data-collection box unticked.
- Started a private tmux server on its own socket and stopped it afterwards.
