# GhosttyKit + Grok Build pane viability spike

- **Date:** 2026-07-27
- **Kind:** empirical throwaway harness — no Boss integration, no GrokDriver code
- **Pinned versions (re-checked before each observation cluster):**
  - `grok 0.2.112 (9bbd559437aa) [stable]` (`~/.grok/bin/grok` → `~/.grok/downloads/grok-0.2.112-macos-aarch64`)
  - **GhosttyKit** prebuilt `ghosttykit-5659cef` (`spinyfin/ghostty-prebuilts`, same pin as `MODULE.bazel` `@ghostty_kit`, sha256 `82b8d947484a21e1a9d186628b8af5e3f2e81dc96925f3cdbc1766ececa814a1`)
- **Host:** macOS (Darwin), no `/proc/<pid>/fd`
- **Related:** [ghostty-codex-pane-viability.md](./ghostty-codex-pane-viability.md); Claude driver spawn path in `tools/boss/engine/driver/src/claude.rs` (`spawn_invocation`, hooks, turn boundary)

## Why this spike exists

Blocking depth-0 gate for a Grok-as-first-class-driver project: can xAI's Grok Build CLI (`grok`) run as a first-class interactive TUI worker inside a **GhosttyKit pane** the way Claude Code does?

Claude bar (what "first-class" means here):

1. Engine never spawns the agent process.
2. Engine composes a shell command → `SpawnWorkerPane` → GhosttyKit pty with `initial_input`.
3. No headless/`--print`/`-p` rescue for the pane path.
4. Probe via surface text + Return; interrupt Esc; stop/reap via pane release + SIGTERM/SIGKILL.
5. Progress via hooks (or an equivalent engine-readable channel).

**Hard apparatus rule for this document:** Q4 / probe / Esc / seed / resize / alt-screen verdicts come from **GhosttyKit-hosted panes only**. Standalone Ghostty.app experiments (if any) are discarded for the pane-viability verdict.

## Verdict (read this first)

### Is interactive-TUI-in-a-GhosttyKit-pane viable for Grok?

**Yes.**

Grok Build's interactive TUI runs under GhosttyKit with the same Boss embedding APIs used for Claude (`ghostty_surface_new`, `ghostty_surface_read_text`, `ghostty_surface_text` + Return, Esc). Positional prompts auto-submit; surface text is observable; SendToPane inject works for follow-up probes and tool prompts; Esc mid-turn cancels with `stop_reason=cancelled` / `cancellation_context.trigger=esc`; resize survives; `/quit` exits cleanly; session files under `$GROK_HOME/sessions/` plus Claude-compatible hooks give a progress/turn-boundary path without relying on engine-owned stdout.

### What transfers unmodified / needs translation / has no equivalent

| Claude shape                                         | Grok transfer                         | Notes                                                                                                                                                                                                 |
| ---------------------------------------------------- | ------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Compose shell command → pane `initial_input`         | **Transfers**                         | Same topology.                                                                                                                                                                                        |
| Positional prompt auto-submits first turn            | **Transfers**                         | `grok … "$(cat .grok/initial-prompt.txt)"` works.                                                                                                                                                     |
| Interactive TUI stays alive after first turn         | **Transfers**                         | Not a one-shot process; need `/quit` or kill for reap.                                                                                                                                                |
| Folder-trust pre-seed                                | **Needs translation**                 | Grok shows "Do you trust the contents of this directory?" until `--trust` / `trusted_folders.toml` / `GROK_FOLDER_TRUST=0`. Claude uses `~/.claude.json` pre-trust.                                   |
| `--dangerously-skip-permissions`                     | **Needs translation**                 | Grok: `--always-approve` and/or `--permission-mode bypassPermissions` / config `permission_mode`.                                                                                                     |
| `.claude/` config dir + hooks in `settings.json`     | **Needs translation**                 | Grok: `$GROK_HOME` (`~/.grok` by default), `~/.grok/hooks/*.json`, project `.grok/hooks/`, optional Claude-compat scan. Prefer isolated `GROK_HOME` per worker.                                       |
| Hook event names (SessionStart, PreToolUse, Stop, …) | **Mostly transfers**                  | Same Claude-shaped names; payloads are Grok-native JSON on stdin + `GROK_*` env.                                                                                                                      |
| Turn boundary = Stop hook                            | **Transfers**                         | `Stop` payload has `reason`, `stopHookActive`, `lastAssistantMessage`. Esc-cancelled turns **skip** Stop hooks (`StopFailure`/interrupt path).                                                        |
| Surface scrape via `ghostty_surface_read_text`       | **Transfers**                         | Works for markers; TUI is screen-scraped (not JSONL-on-stdout). Prefer hooks/session files for structured progress.                                                                                   |
| Esc interrupt                                        | **Transfers** (default UI mode)       | GhosttyKit Esc → mid-turn cancel confirmed. **Caveat:** docs say Esc does **not** cancel in fullscreen **vim** mode (use Ctrl+C). Boss default should keep non-vim / minimal+default Esc cancel path. |
| Progress via hooks forwarder                         | **Transfers with translation**        | Wire `~/.grok/hooks` (or managed config) to `boss-event` equivalent; do not depend on Claude settings unless `compat.claude.hooks=true`.                                                              |
| `claude --print` / JSON stream as pane progress      | **No equivalent as pane transport**   | Grok has headless `-p` + `--output-format streaming-json`, but that is **not** the interactive TUI pane shape. Streaming-json is thought/text/end tokens, not Claude hook events.                     |
| Engine-owned agent stdout                            | **No equivalent under pane topology** | Same as Claude/Codex: app owns pty; engine has `shell_pid` only.                                                                                                                                      |
| Per-run `CODEX_HOME`-style isolation                 | **Transfers as `GROK_HOME`**          | Fully isolatable; sessions land under `$GROK_HOME/sessions/<encoded-cwd>/<sid>/`.                                                                                                                     |
| Model menu                                           | **Thin today**                        | `grok models` advertised only `grok-4.5` (default). Effort via `--reasoning-effort` / `--effort`.                                                                                                     |

**Do not soften:** headless `-p` is useful for CLI probes and is **not** a substitute for the interactive-TUI-in-GhosttyKit gate. This spike's pane verdict does not rest on headless mode.

---

## Method / apparatus

| Layer                     | What it is                                                                  | Used for                                                                                                                                                     |
| ------------------------- | --------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **CLI / headless**        | `grok -p … --output-format json` with `GROK_HOME=/tmp/grok-pane-spike/home` | Q1 install, Q2 concurrency, Q3 large prompts, Q5 hooks, Q6 fail-open/trust, Q7 session files + streaming-json shape, Q9 models/effort/isolation, Q10 interop |
| **GhosttyKit embed host** | Minimal AppKit process linking pinned GhosttyKit; same C APIs as Boss       | **Q4**, **Q8** (seed, observe, inject, Esc, resize, alt-screen, quit)                                                                                        |

Throwaway host + evidence: [`ghostty-grok-pane-viability-artifacts/`](./ghostty-grok-pane-viability-artifacts/). GhosttyKit source: [`ghosttykit_host/`](./ghostty-grok-pane-viability-artifacts/ghosttykit_host/). CLI samples: [`cli/`](./ghostty-grok-pane-viability-artifacts/cli/).

Isolation: all Grok state for this spike used `GROK_HOME=/tmp/grok-pane-spike/home` (auth copied once from the real user home). Project cwd: `/tmp/grok-pane-spike/cwd`.

Prompts deliberately short (`reply with exactly: …`, `sleep N`) to limit cost/time.

### GhosttyKit host (Boss-equivalent APIs)

| Operation         | Boss call site                        | GhosttyKit C API                                                  |
| ----------------- | ------------------------------------- | ----------------------------------------------------------------- |
| Bootstrap         | `GhosttyRuntime` / `GhosttyBootstrap` | `ghostty_init`, `ghostty_config_new`, `ghostty_app_new`           |
| Embed surface     | `GhosttyTerminalHostView.makeSurface` | `ghostty_surface_new` + `GHOSTTY_PLATFORM_MACOS` + `nsview`       |
| Spawn worker      | `TerminalLaunchSpec.initialInput`     | `ghostty_surface_config_s.initial_input`                          |
| Observe pane text | Claude monitor scrape                 | `ghostty_surface_read_text` (`GHOSTTY_POINT_VIEWPORT` / `SCREEN`) |
| SendToPane        | `host.submitText`                     | `ghostty_surface_text` + `ghostty_surface_key` Return (`0x24`)    |
| Interrupt         | `host.sendInterrupt`                  | `ghostty_surface_key` Esc (`0x35`)                                |
| Shell pid         | `ghostty_surface_foreground_pid`      | same                                                              |

Scenario switch via `SPIKE_SCENARIO`: `seed_observe` | `esc_interrupt` | `resize` | `alt_screen`.

Pane script shape (Boss-like):

```sh
export GROK_HOME=…
grok --no-alt-screen --always-approve --trust --session-id "$SID" --cwd "$CWD" \
  "reply with exactly the single token: gkit-grok-ok. do not use tools."
```

`--trust` is required on a fresh directory; without it the TUI blocks on the folder-trust dialog and never runs the positional prompt (observed on the first GhosttyKit run before trust pre-seed).

---

## Q1 — Install / run

**Verified-by-execution.**

```text
$ which grok
/Users/brianduff/.grok/bin/grok
$ grok --version
grok 0.2.112 (9bbd559437aa) [stable]
```

Headless one-shot (revalidated 2026-07-27):

```sh
GROK_HOME=/tmp/grok-pane-spike/home \
  grok -p "reply with exactly: REVAL_OK" --always-approve --session-id <uuid> \
    --cwd /tmp/grok-pane-spike/cwd --output-format json
# → text=REVAL_OK stopReason=EndTurn
```

Interactive CLI is the default (no subcommand): positional `[PROMPT]` starts the TUI. Headless is `-p` / `--single` / `--prompt-file` / `--prompt-json`. There is also `grok agent` ("Run Grok without the interactive UI").

**Auth:** logged in via `grok.com` OAuth (`auth.json` under `GROK_HOME`). `grok models` requires login.

**Finding:** install/run is fine for a driver. Binary is a standalone Mach-O under `~/.grok/`.

---

## Q2 — Entitlement / concurrency (up to 16 workers)

**Verified-by-execution.**

Sixteen concurrent headless sessions (distinct `-s` UUIDs, same `GROK_HOME`, same cwd), all exit 0 with expected markers:

```text
i=1..16 exit=0 text=CONC_{i}_OK  dur≈11.8–15.7s
OK_COUNT=16/16
```

Evidence: `ghostty-grok-pane-viability-artifacts/cli/conc16/*.summary`.

Earlier spike also ran 8 concurrent successfully.

**Finding:** no hard concurrency entitlement block observed at 16 parallel workers on this account. (Does not prove infinite quota; proves the Boss target of "many concurrent workers" is not immediately dead.)

---

## Q3 — Interactive seeding + large prompt / pty buffer

### Positional prompt auto-submit

**Verified-by-execution (GhosttyKit + headless).**

- Headless: `-p` / positional single-turn works.
- Interactive TUI under GhosttyKit: `initial_input` runs `grok … "reply with exactly: gkit-grok-ok…"` with **no extra Enter from the host** beyond the shell newline that launches the command. Surface showed `gkit-grok-ok` within ~2–3s (`seed_observe` and `alt_screen`).

Claude-shaped seed:

```text
grok --always-approve --trust --session-id <uuid> --cwd <ws> \
  "$(cat .grok/initial-prompt.txt)"
```

(or embed the prompt as a shell-quoted argument in the composed command).

### Large prompts

**Verified-by-execution (headless).**

| Prompt size             | Path                   | Result                         |
| ----------------------- | ---------------------- | ------------------------------ |
| ~9.7 KiB                | `-p` / `--prompt-file` | `SAW_LARGE_PROMPT_MARKER_Z9Q7` |
| ~41.6 KiB (brief-sized) | `--prompt-file`        | `BRIEF_SIZE_OK`                |

No pty-buffer truncation observed for headless prompts at brief size. Interactive TUI under GhosttyKit was only exercised with short prompts for cost; large interactive seeds should use a **file on disk + `$(cat …)`** in the composed command (Claude pattern), not paste tens of KB through `initial_input` alone.

### Folder trust (seed blocker)

**Verified-by-execution (GhosttyKit).**

First GhosttyKit run without trust pre-seed stuck forever on:

```text
Do you trust the contents of this directory?
  /private/tmp/grok-pane-spike/cwd
  Yes, proceed  y
```

Fix options (all viable for a driver provisioner):

1. Pass `--trust` on the spawn command.
2. Pre-write `$GROK_HOME/trusted_folders.toml` with the workspace path (both `/tmp/…` and `/private/tmp/…` forms on macOS).
3. `GROK_FOLDER_TRUST=0` (disables the gate entirely — also ungates project hooks/MCP; use carefully).

---

## Q4 — GhosttyKit pane (interactive TUI)

**Verified-by-execution on GhosttyKit only. This is the gate.**

### Q4a — Does the interactive TUI run and complete a seeded turn?

**Yes.** `SPIKE_SCENARIO=seed_observe`:

| Signal                                      | Result                             |
| ------------------------------------------- | ---------------------------------- |
| `saw_gkit_grok_ok` via surface text         | **true**                           |
| `saw_probe_ok` (SendToPane follow-up)       | **true**                           |
| Shell side-effect from injected tool prompt | **`GKIT_INJECT_VIA_SURFACE_TEXT`** |
| `grok_exit` after `/quit`                   | **0**                              |
| Session files written under `GROK_HOME`     | **yes**                            |

Evidence: `ghosttykit_host/evidence/seed_observe/SUMMARY.txt`, `host.log`, `injected_side_effect.txt`.

### Q4b — Alt-screen vs `--no-alt-screen`

**Both work under GhosttyKit.**

| Scenario       | Flag                 | `saw_gkit_grok_ok` | Notes                                                                                                |
| -------------- | -------------------- | ------------------ | ---------------------------------------------------------------------------------------------------- |
| `seed_observe` | `--no-alt-screen`    | true               | Inline; more scrollback visible in surface scrape after long turns                                   |
| `alt_screen`   | (default alt screen) | true               | Fullscreen TUI; on `/quit`, alt screen tears down and main buffer shows residual title + resume hint |

Claude workers often use non-alt or terminal-default; Grok docs recommend `--no-alt-screen` for inline / multiplexer sanity. **Recommendation for Boss:** default `--no-alt-screen` (or config `[terminal] alt_screen`) so surface scrape + scrollback behave more like Claude monitoring; re-test if product wants fullscreen chrome.

### Q4c — Resize

**Verified-by-execution.** `SPIKE_SCENARIO=resize`: shrink surface to 500×320 then grow to 1100×700 via `ghostty_surface_set_size`; post-resize SendToPane probe returned `RESIZE_OK`; `grok_exit=0`.

### Q4d — What the surface scrape actually sees

Surface text is the **rendered TUI**, not a clean JSONL stream. Markers like `gkit-grok-ok` / `GKIT_PROBE_OK` are recoverable while still in the viewport. After scroll/quit they may leave the viewport (`viewport_contains_*` at finish can be false even when `saw_*` was true mid-run). Structured progress must not depend on surface scrape alone.

---

## Q5 — Hook payload fields

**Verified-by-execution** (global hooks under `$GROK_HOME/hooks/dump-all.json` → `dump_hook.sh` capturing ENV + stdin).

Events observed firing: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `Notification`. (Others registered but not hit by the short prompts: `SessionEnd`, `Subagent*`, `*Compact`, `StopFailure`, `PermissionDenied`, `PostToolUseFailure`.)

### Env vars on every hook

| Env                   | Example                                    |
| --------------------- | ------------------------------------------ |
| `GROK_HOME`           | `/tmp/grok-pane-spike/home`                |
| `GROK_AGENT`          | `1`                                        |
| `GROK_HOOK_EVENT`     | `pre_tool_use` (snake)                     |
| `GROK_HOOK_NAME`      | `global/dump-all:pre_tool_use[0].hooks[0]` |
| `GROK_SESSION_ID`     | UUID                                       |
| `GROK_WORKSPACE_ROOT` | project cwd                                |
| `CLAUDE_PROJECT_DIR`  | same as cwd (compat)                       |

### Stdin JSON (top keys by event)

| Event              | Keys (observed)                                                                                       |
| ------------------ | ----------------------------------------------------------------------------------------------------- |
| `SessionStart`     | `hookEventName`, `sessionId`, `cwd`, `workspaceRoot`, `timestamp`, `permissionMode`, `source`         |
| `UserPromptSubmit` | … + `promptId`, `prompt`, `transcriptPath`                                                            |
| `PreToolUse`       | … + `toolName`, `toolUseId`, `toolInput`, `toolInputTruncated`                                        |
| `PostToolUse`      | … + `toolResult`, `toolResultTruncated`, `isBackgrounded`                                             |
| `Stop`             | … + `promptId`, `reason`, `stopHookActive`, `lastAssistantMessage`, `backgroundTasks`, `sessionCrons` |
| `Notification`     | … + `notificationType`, `message`, `level`                                                            |

Samples: `cli/hook_payloads/*.sample.json`.

### Doc inventory (verified-by-official-doc)

From bundled `10-hooks.md`: SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, PostToolUseFailure, PermissionDenied, Stop, StopFailure, Notification, SubagentStart, SubagentStop, PreCompact, PostCompact, SessionEnd. Cursor camelCase aliases accepted. Claude tool-name aliases in matchers (`Bash` → `run_terminal_command`, etc.).

**Driver implication:** a Grok progress forwarder can mirror Claude's hook list with payload field renames (`toolName` vs Claude's names; `transcriptPath` → session `updates.jsonl`).

---

## Q6 — Hook trust / fail-open

**Verified-by-execution + verified-by-official-doc.**

### Project-hook trust

Untrusted project hooks under `<proj>/.grok/hooks/` are **silently skipped** until `/hooks-trust` or `--trust` (folder-trust store: `$GROK_HOME/trusted_folders.toml`).

Empirical: evil project hooks that would write `untrusted_ran.txt` / deny all tools did **not** run; agent still wrote `evil.txt`. Global hooks under `$GROK_HOME/hooks/` always run.

### Fail-open matrix (PreToolUse against a write)

| Hook behavior                                   | Attack file created?                  | Interpretation                                                     |
| ----------------------------------------------- | ------------------------------------- | ------------------------------------------------------------------ |
| `exit 1` (crash)                                | **yes** `ATTACK_crash_OK`             | fail-open                                                          |
| stdout `NOT JSON`                               | **yes** `ATTACK_malformed_OK`         | fail-open                                                          |
| `sleep 30` then deny (timeout)                  | **yes** `ATTACK_timeout_OK`           | fail-open (timeout)                                                |
| `decision=allow` + `updatedInput` rewrite       | **yes** original `ATTACK_rewrite_OK`  | rewrite **not applied** to write path in this version (or ignored) |
| Claude-shaped `hookSpecificOutput.updatedInput` | **yes** original `ATTACK_rewrite2_OK` | same                                                               |
| `{"decision":"deny",…}`                         | **no**                                | **blocks**                                                         |
| stderr message + **exit 2** (no JSON)           | **no**                                | **blocks** (Claude-compatible exit-2 deny)                         |

Doc quote (bundled hooks guide): failures (timeouts, crashes, malformed output) are fail-open; only explicit deny blocks. Exit 2 without JSON is the Claude Code block convention and was observed to block.

**Driver implication:** Boss guards that `exit 2` or emit `{"decision":"deny"}` both work. Do not rely on `updatedInput` rewrite without a dedicated re-test per tool. Prefer isolated `GROK_HOME` hooks over project hooks so trust dialogs do not gate engine guards.

---

## Q7 — Progress transport (`~/.grok/sessions` + `-s`)

**Verified-by-execution + verified-by-official-doc.**

### Session layout

```text
$GROK_HOME/sessions/<url-encoded-cwd>/<session-id>/
  summary.json
  updates.jsonl      # ACP session update stream (authoritative conversation log)
  events.jsonl       # compact phase/tool/turn telemetry
  chat_history.jsonl
  signals.json
  …
```

`-s` / `--session-id` assigns a **new** UUID (errors if already exists). Resume is `--resume` / `-c`, not `-s`.

### Structured channels

1. **Hooks** (best match to Claude progress fidelity): PreToolUse/PostToolUse/Stop/… forwarder.
2. **`updates.jsonl`**: `user_message_chunk`, `agent_thought_chunk`, `agent_message_chunk`, `tool_call`, `tool_call_update`, `turn_completed`, `hook_execution`, …
3. **`events.jsonl`**: `turn_started`, `phase_changed`, `tool_started`, `turn_ended` (with `outcome` / `cancellation_category`), …
4. **Headless `streaming-json`**: token stream `{type: thought|text|end}` — **not** a pane channel; not hook-equivalent.

Seed session (`1b6c772c-…`) update-type histogram:

```text
hook_execution: 9
user_message_chunk: 3
agent_thought_chunk: 4
agent_message_chunk: 4
turn_completed: 3
tool_call: 1
tool_call_update: 3
```

**Finding:** for pane-hosted workers, prefer **hooks + optional session-file tail** (app or engine side with known `GROK_HOME` + sid). Do not plan on engine reading pane stdout JSONL.

---

## Q8 — Probe / interrupt / stop / reap

**Verified-by-execution on GhosttyKit** (probe, Esc, quit) **and CLI** (SIGTERM).

### Probe (SendToPane)

After seed turn completed, host injected:

```text
reply with exactly the single token: GKIT_PROBE_OK. no tools.
```

via `ghostty_surface_text` + Return. Surface showed `GKIT_PROBE_OK`. A subsequent tool-style inject produced `injected_side_effect.txt = GKIT_INJECT_VIA_SURFACE_TEXT`.

### Esc interrupt

`SPIKE_SCENARIO=esc_interrupt`: seed asked for `sleep 45` via shell tool; host sent Esc at ~6s.

Session telemetry (authoritative):

```json
{
  "type": "turn_ended",
  "outcome": "cancelled",
  "cancellation_category": "mid_turn_abort",
  "cancellation_context": { "trigger": "esc" }
}
```

```json
{"sessionUpdate":"turn_completed","stop_reason":"cancelled",…}
```

```json
{"type":"tool_result",…,"content":"Tool execution was cancelled by the user (tool `run_terminal_command` was not executed)."}
```

Post-Esc probe `ESC_AFTER_OK` succeeded; process later exited 0 after `/quit`.

**Doc caveat (verified-by-official-doc):** Esc mid-turn cancel is for default UI; in **fullscreen vim mode** Esc is a no-op for cancel (use Ctrl+C). Boss should not enable vim mode for workers.

### Stop / reap

| Mechanism                                         | Observed                                                                                           |
| ------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| SendToPane `/quit`                                | Interactive process exits; pane script records `grok_exit=0`; shell returns                        |
| `kill -TERM <grok-pid>` during headless long tool | Parent exits **143** (SIGTERM); JSON output empty (killed mid-flight)                              |
| Pane release                                      | Same as Claude: destroy surface / kill shell process group; expect SIGTERM then SIGKILL escalation |

No evidence that Grok ignores SIGTERM. Tool child shells should be reaped with the process group (standard Boss pane teardown).

---

## Q9 — Models / effort / config isolation

**Verified-by-execution.**

```text
$ grok models
Default model: grok-4.5
Available models:
  * grok-4.5 (default)
```

`--reasoning-effort low` accepted; session `summary.json` recorded `"reasoning_effort": "low"`. Doc lists levels: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` (model menu may subset).

**Isolation:**

| Lever                                                  | Works?                                                           |
| ------------------------------------------------------ | ---------------------------------------------------------------- |
| `GROK_HOME`                                            | **Yes** — full state root (auth, hooks, sessions, config, trust) |
| `-s` session UUID                                      | **Yes** — pre-assign new session id                              |
| `--cwd`                                                | **Yes**                                                          |
| Spike `config.toml` with `[compat.claude] hooks=false` | **Yes** — disables Claude settings pickup                        |

Per-worker recommendation: Boss-owned `GROK_HOME` under the execution runtime dir (parallel to Codex's Boss-owned home), plus `--trust` for the cube workspace path.

---

## Q10 — Claude Code interop hazard

**Verified-by-execution + verified-by-official-doc.**

Grok **can** load Claude/Cursor hooks from `~/.claude/settings.json` and project `.claude/settings.json` when compat is enabled (default per docs). Spike config forced:

```toml
[compat.claude]
hooks = false
agents = false
skills = false
plugins = false
rules = false
```

With that config, a malicious-looking project `.claude/settings.json` SessionStart hook **did not run** (`claude_compat_ran` file absent) while the agent still completed `INTEROP_OK`.

**Hazards if compat left on:**

1. Shared project `.claude/settings.json` hooks run under Grok (after folder trust) — double-firing or unexpected denies when Claude and Grok workers share a workspace.
2. `CLAUDE_PROJECT_DIR` is set even for native Grok hooks — easy to confuse in shared scripts.
3. Global `~/.claude` hooks always trusted (doc) when compat hooks enabled — user Claude hooks would apply to Grok workers using the same account home.

**Mitigation:** always set Boss `GROK_HOME` isolation **and** disable compat hooks in that home's `config.toml` (or equivalent managed config). Do not point Grok at the user's live `~/.grok` if Claude-compat is on and the user has Claude hooks.

---

## Transfer map vs Claude driver (`claude.rs`)

| Claude driver concern                                                                                      | Grok analogue                                                                                                                                                   |
| ---------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `spawn_invocation` → `claude --model … --dangerously-skip-permissions "$(cat .claude/initial-prompt.txt)"` | `grok --model … --always-approve --trust --session-id <uuid> --cwd <ws> "$(cat .grok/initial-prompt.txt)"` (+ optional `--no-alt-screen`, `--reasoning-effort`) |
| `provision_workspace` writes `.claude/initial-prompt.txt` + gitignore + pre-trust                          | Write `.grok/initial-prompt.txt` (or prompt file path), pre-seed `trusted_folders.toml`, ensure `GROK_HOME` hooks/settings                                      |
| `progress_observation` hook forwarder on 7 Claude events                                                   | Same event names work; install under `$GROK_HOME/hooks/` JSON                                                                                                   |
| `turn_boundary` on Stop                                                                                    | Stop payload `reason` / `stopHookActive`; ignore non-`end_turn` session-end Stops; Esc cancels **skip** Stop hooks                                              |
| Permission / launch guards as PreToolUse                                                                   | Supported; fail-open on crash; use deny JSON or exit 2                                                                                                          |
| Teardown                                                                                                   | No external home if `GROK_HOME` is Boss-owned under the run dir — delete with the run                                                                           |

---

## Negatives worth keeping (do not soft-pedal)

1. **Folder-trust dialog blocks the TUI** until `--trust` / pre-seed / env disable. A driver that forgets this looks "hung" with a pretty logo.
2. **Surface scrape is not a protocol.** Viewport can lose markers after scroll; alt-screen teardown drops TUI chrome from the scrape.
3. **`updatedInput` rewrite did not rewrite writes** in fail-open tests — do not build guards that require mutation without re-proving it.
4. **Headless streaming-json is not hook parity** and is not the pane transport.
5. **Interactive process does not exit after the first turn** — Boss must `/quit` or kill; same class of concern as Claude TUI.
6. **Model menu is currently a single family** (`grok-4.5`) in `grok models` output on this account — effort is the main dial.
7. **Vim-mode Esc does not cancel** (doc) — keep worker UI defaults away from fullscreen vim.
8. **Claude-compat hooks are a footgun** if `GROK_HOME` is the user's real home.

---

## Appendix A — Scratch / throwaway locations

| Path                                                                    | Purpose                                                     |
| ----------------------------------------------------------------------- | ----------------------------------------------------------- |
| `/tmp/grok-pane-spike/`                                                 | Full empirical scratch (home, cwd, artifacts, host build)   |
| `/tmp/grok-pane-spike/ghosttykit_host/`                                 | Live build of GhosttyKit host (`.build/`, xcframework link) |
| `/tmp/GhosttyKit-5659cef.xcframework`                                   | Unpacked prebuilt (not committed)                           |
| `tools/boss/docs/investigations/ghostty-grok-pane-viability-artifacts/` | Committed samples + host sources + evidence snapshots       |

Not committed: GhosttyKit binary xcframework, `.build/`, live `auth.json`, full session trees.

## Appendix B — How to re-run GhosttyKit scenarios

```sh
# materialize GhosttyKit (sha256 must match MODULE.bazel pin)
curl -fsSL -o /tmp/GhosttyKit-5659cef.tar.gz \
  "https://github.com/spinyfin/ghostty-prebuilts/releases/download/ghosttykit-5659cef/GhosttyKit-5659cef.tar.gz"
tar -xzf /tmp/GhosttyKit-5659cef.tar.gz -C /tmp
# point host at it
cd tools/boss/docs/investigations/ghostty-grok-pane-viability-artifacts/ghosttykit_host
ln -sfn /tmp/GhosttyKit.xcframework .local-GhosttyKit.xcframework   # or extracted name

export GROK_HOME=/tmp/grok-pane-spike/home   # must contain auth.json + trusted_folders
export SPIKE_SCENARIO=seed_observe           # esc_interrupt | resize | alt_screen
./run.sh
# evidence/<scenario>/SUMMARY.txt
```

## Appendix C — Evidence index

| Question             | Primary evidence                                                                 |
| -------------------- | -------------------------------------------------------------------------------- |
| Q1 version / run     | `cli/grok_version.txt`, `cli/revalidate/q1.json`                                 |
| Q2 ×16               | `cli/conc16/*.summary`                                                           |
| Q3 large prompt      | `cli/revalidate/large_headless.json`, `brief_size.json`                          |
| Q4 seed/probe/inject | `ghosttykit_host/evidence/seed_observe/`                                         |
| Q4 alt-screen        | `ghosttykit_host/evidence/alt_screen/`                                           |
| Q4 resize            | `ghosttykit_host/evidence/resize/`                                               |
| Q5 hooks             | `cli/hook_payloads/*.sample.json`                                                |
| Q6 fail-open / trust | `cli/failopen/`, untrusted project test narrative                                |
| Q7 sessions          | seed session under spike `GROK_HOME` (not fully committed); type histogram in Q7 |
| Q8 Esc               | `ghosttykit_host/evidence/esc_interrupt/` + session `events.jsonl` quote in Q8   |
| Q8 SIGTERM           | headless exit 143 narrative                                                      |
| Q9 effort / models   | `cli/revalidate/effort_low.json`                                                 |
| Q10 interop          | `cli/revalidate/interop.json` + absent canary                                    |

---

## Open questions (unresolved)

1. **Does `updatedInput` ever mutate tool args** for `run_terminal_command` / other tools, or only certain tool kinds? Writes ignored it in this version.
2. **Long-term model SKUs** beyond `grok-4.5` / `grok-4.5-build` billing id — menu will change; driver tables must not hard-freeze without a refresh path.
3. **Leader process / `leader.sock`**: CLI exposes `--leader-socket` and `grok leader`. Not characterized whether multi-session workers share a leader and how that interacts with SIGTERM isolation.
4. **Surface-scrape quality under heavy alt-screen animations** (spinners, partial redraws) for a Claude-style "is the worker stuck?" monitor — only short sessions tested.
5. **Stop-hook block / continuation** (`decision: block`) under GhosttyKit not exercised; doc claims Claude-compatible stop gates with an 8-continuation cap.
