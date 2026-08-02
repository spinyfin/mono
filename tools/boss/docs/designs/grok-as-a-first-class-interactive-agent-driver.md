# Grok as a first-class interactive agent driver

- **Date:** 2026-07-27
- **Project:** Grok as a first-class interactive agent driver
- **Runs in parallel with:** the [agent-driver abstraction](agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md) close-out and the [Codex driver](codex-as-a-first-class-agent-driver.md) project. **No dependency edges on either.**
- **Structural template:** [codex-as-a-first-class-agent-driver.md](codex-as-a-first-class-agent-driver.md) (PR #2285). This doc mirrors its section list deliberately; where the two reach different conclusions, the difference is called out rather than smoothed over.
- **Gating spike:** [ghostty-grok-pane-viability.md](../investigations/ghostty-grok-pane-viability.md) (PR #2458, 2026-07-27). Its executed evidence is authoritative over anything in this doc's framing.
- **Boss tree verified at:** `6b2a4ee6` (`main`), 2026-07-27
- **Grok verified at:** `grok 0.2.112 (9bbd559437aa) [stable]`, macOS arm64, `~/.grok/bin/grok`
- **Absorbed row:** the abstraction project's one remaining blocked item — _"agent-driver: ControlVerbs trait surface + call classify_error"_ — is in scope here and appears as [T-04](#t-04-controlverbs-trait-surface-plus-route-error-classification-through-it).

## TL;DR / verdict

**Grok is the easiest of the three drivers to land, and the reason is structural: it is the same execution topology Boss already runs.** An interactive TUI in a GhosttyKit pane, seeded by a positional prompt, alive across turns, interruptible with Esc, probeable with typed input. Everything the Codex project had to invent — a transport for pane-hosted progress, a resume-as-new-process probe model, a way to reason about a worker that exits between turns — Grok simply does not need.

**The single highest-severity conclusion of the Codex project does not apply here.** That doc's verdict was: _"`ProgressObservation` abstracts event normalisation but not event transport, and the transport for pane-hosted workers into the engine is still an open seam."_ For Grok that seam is closed by construction. Boss owns `GROK_HOME`; global hooks under `$GROK_HOME/hooks/` run **unconditionally** (Grok's folder-trust gate applies to _project_ hooks, not to the driver's own home); Grok fires the Claude-shaped lifecycle event set; and every payload carries `transcriptPath`. So Grok reuses `ProgressIngress::HookCallback`, the existing `boss-event` shim, and the existing events socket — the same channel Claude uses in production today. **No new transport, no rollout tail, no app→engine IPC.**

**But the reuse stops at the transport.** The brief assumed Grok would inherit Claude's five `PreToolUse` guard scripts unmodified, as Codex did. **It cannot.** Grok's hook payloads are camelCase with snake_case event values and Grok-native tool names — `hookEventName: "pre_tool_use"`, `toolName: "write"`, `toolInput`, `stopHookActive` — while `normalize_hook_event` (`protocol/src/worker_event.rs:107-152`) requires `hook_event_name` / `tool_name` / `tool_input`, and all five guard scripts read `inp.get('tool_input',{})` (`driver/src/claude.rs:194`, `:305`, `:354`; `core/src/worker_setup.rs:1085-1099`, `:1384-1389`). A Grok worker wired to those scripts today would have **every guard silently no-op** — they would read an empty command string and approve. This is the one place where "Grok is just Claude" is false and dangerous, and it is why [T-02](#t-02-investigation-grok-pretooluse-decision-vocabulary-and-tool-name-map) and [T-09](#t-09-grok-hook-wiring-progress-forwarder-plus-guard-script-canonicalisation) exist.

**One hazard the spike did not reach, found while writing this doc, and it is a gate.** `grok inspect --json` run under a _fresh, empty, isolated_ `GROK_HOME` reports `permissions.sources: ["/Users/<user>/.claude/settings.local.json (settings)"], loaded: 1`. Setting the spike's full `[compat.claude]` disable block flips the Claude _instructions_ file to `compatibilityStatus: "disabled"` but leaves that permission source **still loaded**. `GROK_HOME` isolates state; it does not isolate Claude-compat permission discovery. A Boss Grok worker would therefore run with the machine owner's personal Claude allow/deny rules layered into its permission posture, invisibly. See [Config discovery and isolation](#config-discovery-and-isolation--where-isolation-stops) and [T-01](#t-01-investigation-claude-permission-settings-leakage-into-an-isolated-grok_home).

**Two corrections to the project framing**, both load-bearing for sequencing:

1. **The pane monitor is not a liveness signal.** The brief states that `GhosttyTerminalView.swift:1104-1111` "determines worker liveness and busy-ness". It does not. `ClaudeMonitorState` is app-local: it feeds a _fallback_ status pill in `WorkersDetailView.swift:275-283`, explicitly used only _"until the worker's first hook fires"_, after which engine-supplied `LiveWorkerState` takes over. A Grok TUI reading as `notDetected` is a UI-fidelity defect for the first few seconds of a run, not a lifecycle blocker. The monitor still becomes driver-supplied per the settled decision — but it comes **off the critical path** for the first Grok worker.
2. **Esc-cancelled turns skip the `Stop` hook.** The spike records this in one line ([Q8](../investigations/ghostty-grok-pane-viability.md)); it is the sharpest lifecycle hazard in the project. Boss's turn boundary for a hook-driven driver _is_ `Stop` (`driver/src/claude.rs:615-632`). An interrupted Grok worker emits `turn_ended outcome=cancelled` to its session files and **nothing to the engine**, leaving the slot pinned at `Working` forever. Interrupt is a control verb Boss actually uses, so this must be solved in the same project that puts interrupt on the trait — [T-12](#t-12-turn-end-recovery-for-esc-cancelled-grok-turns).

**Model menu is settled, and smaller than the brief assumed.** Re-run on 2026-07-27: `grok models` reports `grok-4.5` as both default and the entire available list. **`grok-build-0.1` is not on the menu on this account.** The default is `grok-4.5`, which is what the settled scope asked for; the open question about `grok-build-0.1` is resolved as "not applicable today, and the driver must not hard-freeze a table that will change."

## Goals

- Add xAI's Grok Build (`grok`) as a real driver behind the agent-driver abstraction, on the **interactive-TUI-in-a-GhosttyKit-pane** topology Claude uses — so a work item dispatched `--driver grok` runs end-to-end to a green PR with the same lifecycle guarantees a Claude worker has today.
- Make `ControlVerbs` real. Grok is the first driver that genuinely needs probe / interrupt / stop / reap on the trait, so this project turns that seam from a one-method stub into the surface the engine actually calls.
- Make the pane monitor **driver-supplied**, crossing the engine/app boundary the abstraction project deliberately held out of scope.
- Produce a complete gap analysis. Where Grok does not fit the trait, name the abstraction gap and fix it _in the abstraction_, never as Grok-specific special-casing in the engine.
- Identify the seams a later Claude/Codex/Grok load balancer will need, without building it.

## Non-goals

- **Building the load balancer.** Explicitly out of scope. This doc names the attachment points and specifies no policy.
- **Headless Grok.** `-p` / `--single` / `--prompt-file` / `grok agent` are useful for CLI probes and conformance fixtures. They are **not** the worker shape. The pane verdict in the spike does not rest on headless mode and neither does this design.
- **Grok's built-in git worktree support** (`-w`, `--worktree-ref`, `grok worktree`). Cube owns workspace provisioning. This must stay unused, and the driver must never emit those flags.
- **Re-litigating the capability vocabulary.** The 13 capabilities (`driver/src/lib.rs:212-260`) are settled. This doc declares Grok's set and justifies each omission; it does not reopen the model.
- **Removing or de-privileging Claude.** Claude remains the reference driver and `ENGINE_DEFAULT_DRIVER` (`effort/src/lib.rs:15`).
- **Remote / SSH dispatch for Grok.** `core/src/app/worker_events.rs:619` hardcodes `ClaudeDriver.capabilities()` on that path. Deferred and filed as such.
- **Grok's MCP / plugin / subagent / cross-session-memory surfaces.** Rich, and Boss injects none of it. `--no-subagents` and `--no-memory` posture is a v1 decision, not a v1 feature.

## Method

Everything about Grok in this doc is either (a) quoted from the gating spike, which ran it, or (b) established by running `grok 0.2.112` on this host on 2026-07-27. Claims of the second kind that came from `--help` or an argument-parse probe rather than a completed agent turn are marked **_(surface probe)_** — they establish that a flag exists and parses, not that it behaves as named. Nothing here is recalled.

Where this doc and the spike disagree, **the spike wins and the disagreement is stated**. There is one such case ([grok-build-0.1](#model-and-effort)) and it is a resolution rather than a conflict. Where this doc adds facts the spike did not record, they are marked as new and attributed to the probe that produced them.

Boss-side claims were verified against `6b2a4ee6` by locating symbols, not by trusting line numbers. **The brief's ground truth has already drifted** in two places: `claude.rs:635-701` (the guard-script wiring) is now `:635-701` for the wiring but the scripts themselves live at `:190`, `:305`, `:354` and `worker_setup.rs:1064`, `:1250`; and `codex.rs:734-774` (the Codex capability set) is now `:909-948`. Treat the line numbers in _this_ doc the same way.

The spike's harness — an AppKit host linked against the same pinned GhosttyKit prebuilt Boss uses (`ghosttykit-5659cef`), driving `ghostty_surface_new` / `ghostty_surface_read_text` / `ghostty_surface_text` / `ghostty_surface_key` — is reproduced in [Appendix A](#appendix-a-reproducing-the-spike). The hard apparatus rule from that spike carries into this doc: **pane verdicts come from GhosttyKit-hosted panes only.**

---

## Version delta

The Codex project's version-delta section compared two CLI releases and found four silent breaks in eight minor versions. Only one Grok version has ever been characterised (`0.2.112`), so there is no release-to-release delta to report. What exists instead is a **spike-to-doc delta**: re-running the surface on the _same_ binary while writing this design turned up facts the spike did not record, three of which change the design.

### Deltas that change the design

**D-1 — Claude permission settings load into an isolated `GROK_HOME`, and the compat toggles do not stop them.** New. The spike's Q10 established that `[compat.claude] hooks = false` prevents a project `.claude/settings.json` _hook_ from firing. It did not test permission-source discovery. Under a fresh empty home:

```console
$ GROK_HOME=/tmp/probe grok inspect --json | jq '{projectInstructions, permissions}'
```

reports the user's `~/.claude/Claude.md` as a loaded project instruction (`vendor: "claude"`, `compatibilityStatus: "enabled"`) **and**:

```json
"permissions": {
  "sources": ["/Users/<user>/.claude/settings.local.json (settings)"],
  "loaded": 1,
  "skipped": [],
  "managedSettingsPath": "/Library/Application Support/ClaudeCode/managed-settings.json"
}
```

Adding the spike's full `[compat.claude]` disable block flips the instructions entry to `"compatibilityStatus": "disabled"` — and leaves `permissions.sources` / `loaded: 1` **unchanged**. Grok also probes for Claude's _managed_ settings at a system path outside any home. This is a real, unmitigated isolation hole and it gates the first Grok worker: see [T-01](#t-01-investigation-claude-permission-settings-leakage-into-an-isolated-grok_home).

**D-2 — Grok has a per-tool rule grammar _and_ a fail-closed sandbox. Codex had neither together.** New _(surface probe)_. `grok --help` on 0.2.112 documents `--allow <RULE>` / `--deny <RULE>` with the explicit compat aliases `--allowedTools` / `--disallowedTools`, plus `--tools` / `--disallowed-tools` for the built-in tool set, and `--sandbox <PROFILE>` (`env: GROK_SANDBOX`). Profile-name resolution probes distinguish built-ins from unknown names cleanly:

| Probed name                                                                                 | Result                                   |
| ------------------------------------------------------------------------------------------- | ---------------------------------------- |
| `workspace`, `read-only`, `readonly`, `strict`, `off`, `none`                               | resolved (proceeded to the auth check)   |
| `workspace-write`, `danger-full-access`, `restricted`, `default`, `permissive`, `read_only` | `Custom sandbox profile '<x>' not found` |

Custom profiles come from `~/.grok/sandbox.toml` or `.grok/sandbox.toml` with an `extends` / `read_only` shape. Crucially, **the sandbox fails closed**:

```text
error: could not apply the 'bogus-xyz' sandbox profile
(including direct global-hook write protection); refusing to start.
```

That parenthetical is load-bearing: the sandbox explicitly protects the global hook directory from agent writes, which is the mechanism by which a worker could otherwise disarm Boss's own guards. This materially strengthens [Guardrail integrity](#guardrail-integrity) relative to Codex, and it is the reason [T-16](#t-16-investigation-grok-sandbox-profiles-and-allowdeny-rule-grammar) is filed as its own investigation rather than folded into the permission-policy implementer.

**D-3 — `--trust` works but is hidden from `--help`.** New. The spike's pane launch line depends on `--trust` (without it the TUI blocks forever on the folder-trust dialog). `grok --help | grep trust` returns nothing on 0.2.112, yet `grok --trust …` parses and runs, while a genuinely unknown flag errors:

```console
$ grok --definitely-not-a-flag -p x
error: unexpected argument '--definitely-not-a-flag' found
```

So `--trust` is a real but undocumented flag. This is exactly the failure shape the Codex project hit when `-a/--ask-for-approval` was removed in a minor release — except worse, because a hidden flag's removal will not show up in a `--help` diff. The driver must not depend on `--trust` alone: it should **also** pre-seed `$GROK_HOME/trusted_folders.toml`, so losing the flag degrades to a redundant belt rather than a hung worker. See [T-07](#t-07-grok-config-isolation-and-workspace-provisioning).

### Deltas that change a task's scope

**D-4 — `grok inspect --json` is a real pre-spawn assertion surface.** New. It reports `grokVersion`, `projectRoot`, `projectTrusted`, the resolved `projectInstructions` list with per-entry `compatibilityStatus`, `permissions.sources` / `loaded` / `skipped`, and — decisively — a **`hooks` inventory**. The single hardest question in the Codex project was _"can Boss tell that a configured hook did not run?"_, and for Codex the answer was essentially no. For Grok, `grok inspect --json` answers the adjacent and more useful question — _"are Boss's hooks registered and is the compat surface off?"_ — **before** the worker starts, deterministically, without an API call. This turns a class of silent-misconfiguration risk into a startup assertion and it belongs in [T-07](#t-07-grok-config-isolation-and-workspace-provisioning) and the conformance harness ([T-20](#t-20-conformance-grok-goldens-and-version-pin)). **Superseded 2026-08-01:** of the fields listed above, only `grokVersion` stopped being gated — see the T-20 amendment. `projectTrusted`, the compat-cell matrix, and the hooks inventory are still asserted fail-closed exactly as described here.

**D-5 — `--json-schema <SCHEMA>` exists.** New _(surface probe)_. `--help` describes it as _"JSON Schema for structured output. When set, the model is constrained to produce JSON matching this schema. Implies --output-format json."_ That is the same class of native contract Codex's `--output-schema` provides and Claude lacks. The **"implies `--output-format json`"** clause is the problem: output format is a headless concept, and the worker shape here is the interactive TUI. Whether the flag is meaningful for a TUI session at all is unestablished, so [T-18](#t-18-structured-output-for-grok) evaluates it rather than assuming it.

### Re-verified against the spike

`grok --version` → `grok 0.2.112 (9bbd559437aa) [stable]`, unchanged. `grok models` → `Default model: grok-4.5`, one entry, unchanged. Login state `grok.com` OAuth, unchanged. Subcommand list confirms `agent`, `doctor`, `export`, `inspect`, `leader`, `mcp`, `memory`, `models`, `sessions`, `trace`, `worktree`, `wrap`.

### The pinning argument

Grok's release cadence is uncharacterised — one version, one day. That is _less_ evidence than the Codex project had, not more, and the two structural risks it identified both apply here with no mitigation available:

- **No schema version anywhere.** Neither hook payloads nor `updates.jsonl` / `events.jsonl` records carry a version field.
- **A hidden flag the design depends on** (D-3), whose removal a `--help` diff would not catch.

Grok does offer one thing Codex does not: `grok inspect --json` reports `grokVersion` and the resolved config, so a conformance test can assert both the pin and the effective configuration in one call. Pin the tested version, capture goldens, and gate upgrades on the harness — [T-20](#t-20-conformance-grok-goldens-and-version-pin).

**Superseded 2026-08-01.** Unlike Codex, where a human chooses when to bump the pinned CLI, Grok has no interactive update gate Boss can hold back — it auto-updates on its own schedule. "Gate upgrades on the harness" was never actually enforceable for Grok the way it is for Codex, and in practice the version half of this argument produced the opposite of its goal: an automatic bump silently broke every Grok execution (fail-closed at provisioning, before a worker even started) instead of being caught and reviewed. The version pin was removed; `grokVersion` is now observed and logged on drift (`LAST_CHARACTERISED_GROK_VERSION`) but never gates. The goldens / hidden-`--trust`-flag / `grok models`-menu half of the pinning argument still holds and remains fail-closed.

---

## What the Grok CLI actually is

### Invocation and modes

The **interactive TUI is the default**: `grok [OPTIONS] [PROMPT]` with no subcommand starts a full-screen (or inline) TUI and, when a positional prompt is present, auto-submits it as the first turn with no extra Return from the host. That is the worker shape, and it is the same shape as Claude's.

Flags that matter to the driver, from `grok --help` on 0.2.112 _(surface probe unless the spike executed them)_:

| Flag                                             | Meaning                                                                                                                     | Status                                                             |
| ------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| `[PROMPT]` positional                            | Seeds and auto-submits the first turn                                                                                       | **Executed** (spike Q3, GhosttyKit)                                |
| `-m, --model <MODEL>`                            | Model id                                                                                                                    | Executed                                                           |
| `--reasoning-effort <EFFORT>` (alias `--effort`) | Reasoning level                                                                                                             | Executed (spike Q9, `"reasoning_effort": "low"` in `summary.json`) |
| `-s, --session-id <UUID>`                        | Assign a **new** session UUID. Errors if it already exists; does **not** resume                                             | Executed                                                           |
| `--cwd <CWD>`                                    | Working directory                                                                                                           | Executed                                                           |
| `--always-approve`                               | Auto-approve all tool executions                                                                                            | Executed                                                           |
| `--permission-mode <MODE>`                       | `default\|acceptEdits\|auto\|dontAsk\|bypassPermissions\|plan` — Claude's exact enum plus `auto`                            | Surface probe                                                      |
| `--trust`                                        | Pre-accept folder trust. **Hidden from `--help`** (D-3)                                                                     | Executed (spike), presence re-probed                               |
| `--no-alt-screen`                                | Run inline rather than on the alternate screen                                                                              | Executed (spike Q4b)                                               |
| `--minimal`                                      | Scrollback-native rendering: finalized blocks printed into native scrollback, small pinned region for prompt + running turn | Surface probe                                                      |
| `--fullscreen`                                   | Force the fullscreen TUI for the session                                                                                    | Surface probe                                                      |
| `--allow` / `--deny <RULE>`                      | Per-rule permission grammar (aliases `--allowedTools` / `--disallowedTools`)                                                | Surface probe (D-2)                                                |
| `--tools` / `--disallowed-tools <TOOLS>`         | Built-in tool allow/remove list                                                                                             | Surface probe                                                      |
| `--sandbox <PROFILE>`                            | Named sandbox profile; **fails closed**                                                                                     | Surface probe (D-2)                                                |
| `--json-schema <SCHEMA>`                         | Constrain output to a JSON Schema; implies `--output-format json`                                                           | Surface probe (D-5)                                                |
| `--max-turns <N>`                                | Turn cap                                                                                                                    | Surface probe                                                      |
| `--rules <RULES>` / `--system-prompt-override`   | Prompt injection levers                                                                                                     | Surface probe                                                      |
| `--no-subagents` / `--no-memory` / `--no-plan`   | Disable subagents / cross-session memory / plan mode                                                                        | Surface probe                                                      |
| `--leader-socket <PATH>`                         | Leader process socket (default `~/.grok/leader.sock`)                                                                       | Surface probe                                                      |
| `-w, --worktree` / `--worktree-ref`              | Grok's own git worktree support                                                                                             | **Must stay unused** — cube owns this                              |

Headless exists (`-p/--single`, `--prompt-file`, `--prompt-json`, `--output-format plain|json|streaming-json`, and `grok agent`) and is useful for fixtures. It is not the worker shape.

**The folder-trust dialog is the seeding blocker.** Verified in the spike: the first GhosttyKit run without trust pre-seed hung indefinitely on _"Do you trust the contents of this directory?"_ while displaying a healthy-looking TUI. A driver that forgets this produces a worker that looks alive and never runs a turn. Three independent fixes exist (`--trust`, `$GROK_HOME/trusted_folders.toml`, `GROK_FOLDER_TRUST=0`); the design uses the first two together and rejects the third, which also ungates project hooks and MCP.

### The event / progress stream

Grok exposes **four** structured channels. They are not interchangeable and only one is the pane transport.

1. **Hooks** — the chosen channel. Claude-shaped event _names_, Grok-shaped payloads. Observed firing in the spike: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `Notification`. Documented but not hit by the spike's short prompts: `SessionEnd`, `PostToolUseFailure`, `PermissionDenied`, `StopFailure`, `SubagentStart`, `SubagentStop`, `PreCompact`, `PostCompact`.
2. **`$GROK_HOME/sessions/<pct-encoded-cwd>/<sid>/updates.jsonl`** — the ACP session update stream, the authoritative conversation log: `user_message_chunk`, `agent_thought_chunk`, `agent_message_chunk`, `tool_call`, `tool_call_update`, `turn_completed`, `hook_execution`.
3. **`…/<sid>/events.jsonl`** — compact phase/tool/turn telemetry: `turn_started`, `phase_changed`, `tool_started`, `turn_ended` with `outcome` and `cancellation_category`.
4. **Headless `--output-format streaming-json`** — a `{type: thought|text|end}` token stream. **Not a pane channel and not hook-equivalent.** Do not design against it.

**Why hooks and not the session files.** The Codex project was forced onto a file tail because Codex's hook trust model failed open and silently for the hooks _Boss itself installs_, which is disqualifying for a liveness signal. Grok's trust gate does not have that property: it gates **project** hooks under `<proj>/.grok/hooks/`, while **global hooks under `$GROK_HOME/hooks/` always run** (spike Q6, verified by execution — evil project hooks were skipped while the global dump hook captured every event). Boss owns `GROK_HOME`. Therefore Boss's hooks are unconditionally armed, and the fragility that pushed Codex off hooks does not exist for Grok.

That said, the session files are not useless, and one of them is genuinely needed:

- **`updates.jsonl` is fully constructible in advance.** Boss assigns `GROK_HOME`, `--cwd`, and `-s <uuid>`, and the path is `$GROK_HOME/sessions/<pct-encoded-cwd>/<sid>/updates.jsonl` — confirmed by the `transcriptPath` field in the spike's captured payloads. The brief anticipated this: Boss can **name** the file rather than glob for it, which is strictly better than the Codex rollout discovery (a `**/rollout-*-{thread_id}.jsonl` glob forced by a local timestamp in the filename). It needs no snapshot-diff correlation and no `session_meta` cwd check.
- **`events.jsonl` is the only channel that reports an Esc cancellation.** This is not an optimisation; see [T-12](#t-12-turn-end-recovery-for-esc-cancelled-grok-turns).

**Engine-owned agent stdout does not exist under this topology**, exactly as for Claude and Codex: the app owns the pty and the engine holds only `shell_pid`. Nothing in this design reads pane stdout.

### Payload shape — the reuse boundary

This is the finding that most changes the project's reuse posture. From the spike's committed samples (`cli/hook_payloads/*.sample.json`):

```json
{
  "hookEventName": "pre_tool_use",
  "sessionId": "7727264c-…",
  "cwd": "/private/tmp/grok-pane-spike/cwd",
  "workspaceRoot": "/private/tmp/grok-pane-spike/cwd",
  "timestamp": "2026-07-27T22:56:09.311609+00:00",
  "transcriptPath": "/tmp/grok-pane-spike/home/sessions/%2Fprivate%2F…/updates.jsonl",
  "permissionMode": "bypassPermissions",
  "toolName": "write",
  "toolUseId": "call-b8e62e24-…-0",
  "toolInput": { "file_path": "…/tool_probe.txt", "content": "TOOL_PROBE_OK\n" },
  "toolInputTruncated": false
}
```

Field-by-field against what Boss's shared parsing expects:

| Boss expects                               | Claude sends | Grok sends                                     | Transfers?                                         |
| ------------------------------------------ | ------------ | ---------------------------------------------- | -------------------------------------------------- |
| `hook_event_name: "PreToolUse"`            | ✔            | `hookEventName: "pre_tool_use"`                | **No** — key _and_ value differ                    |
| `session_id`                               | ✔            | `sessionId`                                    | **No**                                             |
| `transcript_path`                          | ✔            | `transcriptPath`                               | **No** (but present, which Codex's stdout was not) |
| `tool_name: "Bash"`                        | ✔            | `toolName: "write"` / `"run_terminal_command"` | **No** — key _and_ vocabulary differ               |
| `tool_input`                               | ✔            | `toolInput`                                    | **No**                                             |
| `tool_response`                            | ✔            | `toolResult`                                   | **No**                                             |
| `stop_hook_active`                         | ✔            | `stopHookActive`                               | **No**                                             |
| `permission_mode: "bypassPermissions"`     | ✔            | `permissionMode: "bypassPermissions"`          | **Value transfers**, key does not                  |
| `source: "startup"`                        | ✔            | `source: "new"`                                | **No** — but degrades safely, see below            |
| `reason`, `lastAssistantMessage` on `Stop` | `reason`     | `reason: "end_turn"`, `lastAssistantMessage`   | Key transfers, casing differs on the second        |

Grok's hook runner injects five reserved variables into every hook: `GROK_HOOK_EVENT` (snake), `GROK_HOOK_NAME`, `GROK_SESSION_ID`, `GROK_WORKSPACE_ROOT`, and **`CLAUDE_PROJECT_DIR`**. The last is set even for native Grok hooks, so it is a footgun in shared scripts and is called out in [Claude-Code interop](#claude-code-interop--a-coexistence-hazard). The earlier spike's `env | grep GROK` output also listed `GROK_HOME` and `GROK_AGENT=1`, but those were inherited by its harness: `GROK_HOME` came from the launched process and `GROK_AGENT` from the persistent tool shell. Neither is runner-injected and neither establishes hook identity.

**Three consequences, in descending severity.**

1. **The five guard scripts silently no-op.** `driver/src/claude.rs:194` reads `inp.get('tool_input',{}).get('command','')`; against a Grok payload that yields `''`, and every guard's early-out approves an empty command. The Boss-data-dir guard, the launch guard, the PR-redirect guard, the checkleft push guard and the revision-PR guard would all be present, all firing, and all permitting everything. **This is a guardrail-integrity failure that looks exactly like a working configuration**, which is why it gets a dedicated adapter rather than five script edits.
2. **`normalize_hook_event` cannot parse Grok payloads.** It fails at `MissingField("hook_event_name")` (`worker_event.rs:114-117`) — loudly, which is the good outcome. Grok needs its own `ProgressSessionNormalizer`. That is not a new seam: normalisers are already per-driver.
3. **`source: "new"` degrades correctly with no work.** `parse_session_start_source` maps unknown values to `SessionStartSource::Other` (`worker_event.rs:173-181`), and the reducer treats `Startup | Clear | Compact | Other` identically for the `Spawning → Idle` transition (`core/src/live_worker_state.rs:494-503`). **No protocol widening is needed** — a pleasant contrast with the Codex project, which had to widen both `WorkerEvent` session identity and `SessionStartSource`. `extract_session_identity` already falls back `session_id → thread_id` (`:166-171`); Grok's `sessionId` will be canonicalised by the adapter before it reaches that function, so no third fallback is required.

### Session, turn, and transcript identity

- **Session identity is `sessionId`**, a UUID, and Boss **assigns it** via `-s`. This is a real advantage over both peers: Claude's session id is discovered from the first hook payload and Codex's `thread_id` from the first stream envelope, whereas Grok's is known before the process starts. The engine can pre-register the identity for the run instead of racing to learn it.
- `-s` creates; it never resumes. Resume is `--resume` / `-c`, and `--fork-session` is required to combine `-s` with a resume. A driver that confuses these will fail with "session already exists" on the second turn.
- **Turn identity** is `promptId`, present on `UserPromptSubmit` and `Stop`.
- **Transcript is `transcriptPath`**, stamped on every hook payload, pointing at `…/<sid>/updates.jsonl`. `transcript_path_for_session` (already on the trait, `claude.rs:710-719`) therefore works for Grok with only a key rename — no glob, no derivation, no dir watch.
- **Line schema is Grok's.** `updates.jsonl` is an ACP session-update stream (`sessionUpdate` discriminator), not Claude's transcript dialect. The Codex project's rule applies verbatim: **reuse the tailer shell (`engine/transcript-tail`) at container level; do not share a parser.**

### Config discovery and isolation — where isolation stops

`GROK_HOME` is a complete state root: auth, hooks, sessions, config, trust store, leader socket. The spike verified per-worker isolation works, and the design uses a Boss-owned `GROK_HOME` under the execution runtime dir, exactly parallel to Codex's `CODEX_HOME`.

Layering, in the order Grok resolves it:

- `$GROK_HOME/config.toml` — the user layer Boss owns.
- `$GROK_HOME/trusted_folders.toml` — folder-trust store, keyed by absolute path with a `decided_at` epoch. **macOS needs both `/tmp/…` and `/private/tmp/…` forms**, and by extension both forms of any symlinked workspace root.
- `$GROK_HOME/hooks/*.json` — global hooks, always trusted.
- `$GROK_HOME/sandbox.toml` — custom sandbox profiles _(surface probe)_.
- Project `<proj>/.grok/` — hooks, config, sandbox profiles. **Trust-gated**, and attacker-controllable in Boss's threat model since it lives in the repo under work. The driver must not depend on it.
- CLI flags — highest precedence.

**Where isolation stops, and this is D-1.** `GROK_HOME` does not scope Claude-compat _discovery_. Under a fresh empty home, `grok inspect --json` still resolves `~/.claude/Claude.md` as a project instruction and `~/.claude/settings.local.json` as a loaded permission source, and probes `/Library/Application Support/ClaudeCode/managed-settings.json` for managed settings. The `[compat.claude]` block disables the instructions pickup (`compatibilityStatus` flips to `"disabled"`) but **not** the permission source.

In a real Boss workspace the instruction surface is broader still — `grok inspect --json` from this workspace resolves three project-instruction files: the repo's tracked `AGENTS.md`, the user's global `~/.claude/Claude.md`, and the engine-written `.claude/CLAUDE.md` inside the workspace. The last is the Claude worker-rules file; it names Claude's mechanisms and would be read by a Grok worker under compat. The `[compat.claude] rules = false` setting disables both Claude-vendored entries, verified. The permission source is the one that survives.

**Consequence:** Boss cannot honestly declare `PermissionPolicy` for Grok until this is closed. It is [T-01](#t-01-investigation-claude-permission-settings-leakage-into-an-isolated-grok_home), a `small` investigation, and it gates spawn — the same shape of gate the Codex project put on hook trust.

### Auth and coexistence

Auth is `auth.json` **inside `GROK_HOME`** (grok.com OAuth on this host), not an environment variable. Same shape as Codex, same consequences:

- A per-worker `GROK_HOME` must have `auth.json` present; symlinking the host credential is sufficient and avoids copying a secret per workspace.
- **No collision with `unset ANTHROPIC_API_KEY`** at the shared spawn wrapper — it is inert for Grok. It remains a Claude-ism in driver-generic code and belongs behind the driver.
- `grok models` requires login, which makes it a cheap liveness check on the credential without an inference call.
- **Concurrency is not entitlement-blocked at Boss's target scale**: the spike ran 16 concurrent sessions to completion, 16/16, ~12–16s each. That does not prove unlimited quota; it proves the design's premise is not immediately dead.
- **Uncharacterised: the leader process.** `--leader-socket` and `grok leader` exist, defaulting to `~/.grok/leader.sock`. Whether concurrent workers share a leader — and how that interacts with per-run `GROK_HOME` isolation and with SIGTERM reap — is unknown. Since the socket path is derived from `GROK_HOME`, per-run isolation _should_ give each worker its own leader; that is an assumption, not a measurement, and it is [OQ-4](#oq-4-the-leader-process).

### Sandbox and approval

This is where Grok is materially **better equipped than either peer**, and the design should use it rather than routing everything through hooks.

| Lever                        | Grok                                                                                       | Claude                         | Codex                  |
| ---------------------------- | ------------------------------------------------------------------------------------------ | ------------------------------ | ---------------------- |
| Per-tool rule grammar        | `--allow` / `--deny` with `--allowedTools` / `--disallowedTools` aliases                   | settings.json allow/deny rules | **none**               |
| Built-in tool set control    | `--tools` / `--disallowed-tools`                                                           | partial                        | none                   |
| Filesystem/network sandbox   | named profiles (`workspace`, `read-only`, `strict`, `off`, `none`) + custom `sandbox.toml` | none                           | three modes            |
| Sandbox failure mode         | **fails closed** — _"refusing to start"_                                                   | n/a                            | starts                 |
| Global-hook write protection | explicit, named in the sandbox error text                                                  | none                           | none                   |
| Permission mode              | Claude's exact enum + `auto`                                                               | native                         | fixed `Never` for exec |

Fidelity mapping for the rules Boss expresses today:

| Boss rule                            | Grok equivalent                                                                      | Fidelity                                      |
| ------------------------------------ | ------------------------------------------------------------------------------------ | --------------------------------------------- |
| Reviewer read-only                   | `--sandbox read-only`                                                                | **Exact**, and enforced below the hook layer  |
| Deny writes to the Boss data dir     | `--deny` rule and/or a custom sandbox profile; the data dir is outside the workspace | **At least preserved**, likely stronger       |
| Deny `rm -rf`, `sudo`                | `--deny 'Bash(...)'`-shaped rule                                                     | **Plausibly preserved** — grammar unvalidated |
| Deny `bossctl`                       | `--deny` rule, plus the existing `PreToolUse` guard                                  | **Preserved**, defence in depth               |
| Block `jj git push` / `gh pr create` | `PreToolUse` guard (as today), optionally `--deny`                                   | **Preserved** via the adapted guard           |

**Two caveats that stop this being a v1 slam dunk.** First, rule strings are **not validated at parse time** — `grok --deny '((((' models` is accepted without complaint _(surface probe)_ — so a malformed deny rule is a silent no-op, which is precisely the fail-open shape this design is trying to avoid elsewhere. Second, whether Grok's rule grammar accepts Claude's `Bash(rm -rf:*)` spelling, its own `run_terminal_command(...)` spelling, or both, is unestablished. Both questions are cheap to answer empirically and both must be answered before the driver relies on the mechanism: [T-16](#t-16-investigation-grok-sandbox-profiles-and-allowdeny-rule-grammar).

### Hooks

Configuration lives in `$GROK_HOME/hooks/*.json` (JSON files, one or more, event-keyed) and in project `<proj>/.grok/hooks/`. Matchers accept Claude tool-name aliases (`Bash` → `run_terminal_command`, per the bundled hooks guide), which is why matching works even though payloads carry native names.

**Trust.** Project hooks are silently skipped until `/hooks-trust` or `--trust`; global hooks under `$GROK_HOME/hooks/` always run. Boss owns the home, so Boss's hooks are always armed. **The trust gate is therefore a hazard only for hooks Boss does not control** — and the mitigation is simply never to depend on project hooks.

**Fail-open matrix**, verified by execution in the spike against a `PreToolUse` guarding a write:

| Handler behaviour                               | Attack file created?      | Interpretation                          |
| ----------------------------------------------- | ------------------------- | --------------------------------------- |
| `exit 1` (crash)                                | **yes**                   | fail-open                               |
| stdout `NOT JSON`                               | **yes**                   | fail-open                               |
| `sleep 30` then deny (timeout)                  | **yes**                   | fail-open                               |
| `decision=allow` + `updatedInput` rewrite       | **yes**, original content | rewrite **not applied**                 |
| Claude-shaped `hookSpecificOutput.updatedInput` | **yes**, original content | rewrite **not applied**                 |
| `{"decision":"deny", …}`                        | **no**                    | **blocks**                              |
| stderr message + **exit 2**                     | **no**                    | **blocks** (Claude's exit-2 convention) |

This is the same fail-open posture Claude has in production today, so it is not a regression — but two specifics matter for the driver:

- **`updatedInput` does not rewrite.** Same conclusion as Codex, reached by different evidence. The editorial `AllowWithRewrite { updated_command: Some(..) }` path is unreachable; see [the editorial case](#the-editorial-case-precisely).
- **Boss's guards emit `{"decision": "block"|"approve"}`** (`worker_setup.rs:1064-1069`, `:1250-1255`). The spike proved `"deny"` blocks and exit-2 blocks. **`"block"` is unverified on Grok**, and `"approve"` is unverified as a non-blocking allow. If `"block"` is not recognised, every adapted guard fails open. This is the single most important unknown in the guardrail path and it is the first half of [T-02](#t-02-investigation-grok-pretooluse-decision-vocabulary-and-tool-name-map).

### Model and effort

```console
$ grok models
You are logged in with grok.com.
Default model: grok-4.5
Available models:
  * grok-4.5 (default)
```

Re-run 2026-07-27. **One model.** The settled default `grok-4.5` is therefore also the only choice, which resolves the open question about `grok-build-0.1`: **it is not on this account's menu, and the driver must not reference it.** `grok-code-fast-1` is retired (15 May 2026) and silently redirects rather than erroring, which makes it useless as a probe target — do not use it to test model selection.

Effort is the real dial. `--reasoning-effort` (alias `--effort`) was executed at `low` in the spike and recorded as `"reasoning_effort": "low"` in the session `summary.json`. The documented ladder is `none, minimal, low, medium, high, xhigh, max` — seven levels against Boss's five-variant `EffortLevel`, so the mapping is a straightforward selection rather than a degrade. Proposed table, mirroring Claude's shape (`claude.rs:35-43`):

| Boss `EffortLevel` | Grok `--reasoning-effort` |
| ------------------ | ------------------------- |
| `Trivial`          | `low`                     |
| `Small`            | `medium`                  |
| `Medium`           | `high`                    |
| `Large`            | `xhigh`                   |
| `Max`              | `max`                     |

`none` and `minimal` are unreachable from Boss's ladder, which is correct — a Boss worker should never run with reasoning disabled.

**The menu must not be hard-frozen.** A single-model table today will be wrong the moment xAI ships a second SKU, and the Codex project's experience is that model catalogs move faster than driver code. `grok models` is machine-readable enough to be the refresh source; at minimum the descriptor needs a documented refresh path and a conformance assertion that the pinned menu still matches the live one ([A-12](#proposed-abstraction-amendments), [T-20](#t-20-conformance-grok-goldens-and-version-pin)).

### Claude-Code interop — a coexistence hazard

Grok's Claude compatibility is **on by default** and is broader than the spike measured. Three distinct vectors:

1. **Instructions.** `~/.claude/Claude.md`, project `.claude/CLAUDE.md`, and `CLAUDE.md` are resolved as project instructions with `vendor: "claude"`. In a Boss workspace that includes the engine-written `.claude/CLAUDE.md` worker-rules file, which describes Claude's mechanisms to a Grok worker. **Mitigated** by `[compat.claude] rules = false` (verified: `compatibilityStatus` flips to `"disabled"`).
2. **Hooks.** Shared project `.claude/settings.json` hooks run under Grok after folder trust; global `~/.claude` hooks are always trusted when compat hooks are enabled. In a workspace that has run both drivers, this means double-firing `boss-event` forwarders or unexpected denies. **Mitigated** by `[compat.claude] hooks = false` (verified in the spike: the canary did not run).
3. **Permissions.** `~/.claude/settings.local.json` loads as a permission source, and `/Library/Application Support/ClaudeCode/managed-settings.json` is probed for managed settings. **Not mitigated** by any `[compat.claude]` key tested (D-1).

Plus the ambient one: **`CLAUDE_PROJECT_DIR` is exported to native Grok hooks.** Any shared script that branches on that variable's presence will mis-identify a Grok worker as a Claude worker. Boss's adapter must key on the runner-injected `GROK_HOOK_EVENT`, `GROK_HOOK_NAME`, `GROK_SESSION_ID`, and `GROK_WORKSPACE_ROOT`, never on `CLAUDE_PROJECT_DIR`.

**Posture:** always Boss-owned `GROK_HOME`, always the full `[compat.claude]` and `[compat.cursor]` disable block, never point Grok at the user's live `~/.grok`, and assert the resulting posture with `grok inspect --json` before the first turn.

---

## Per-capability gap analysis

Classification per the house convention: **(a)** implementable against the current trait, **(b)** needs a trait signature change, **(c)** needs new engine machinery, **(d)** genuinely absent.

| #    | Capability              | What Grok offers natively                            | Class    | Verdict                                                    |
| ---- | ----------------------- | ---------------------------------------------------- | -------- | ---------------------------------------------------------- |
| G-1  | `Spawn`                 | interactive TUI + positional prompt + flags          | **(a)**  | Fits `SpawnRequest`/`SpawnPlan` as landed                  |
| G-2  | `WorkspaceProvisioning` | `GROK_HOME`, `trusted_folders.toml`, `--trust`       | **(a)**  | Fits; blocked by driver-generic Claude pre-trust           |
| G-3  | `PermissionPolicy`      | `--sandbox`, `--allow`/`--deny`, `--permission-mode` | **(a)**† | Richest of the three drivers; **gated on the compat leak** |
| G-4  | `ModelAndEffortMenu`    | `-m`, `--reasoning-effort`, `grok models`            | **(a)**  | Single-model menu; needs a refresh path                    |
| G-5  | `ProgressObservation`   | hooks under a Boss-owned home                        | **(b)**  | **Transport is solved**; wiring _destination_ is the gap   |
| G-6  | `ToolUseInterception`   | `PreToolUse` deny, fail-open, deny-only              | **(b)**  | Works, but **payload dialect breaks the guards**           |
| G-7  | `TurnBoundary`          | `Stop` hook with `reason` / `stopHookActive`         | **(c)**  | Maps directly — **except after an interrupt**              |
| G-8  | `StructuredOutput`      | file contract; `--json-schema` unproven for TUI      | **(a)**  | File contract suffices; native flag is a bonus             |
| G-9  | `TranscriptAccess`      | `transcriptPath` on every payload                    | **(a)**  | Best of the three; container reuse, own parser             |
| G-10 | `ControlVerbs`          | Esc, typed input, `/quit`, SIGTERM                   | **(b)**  | **The absorbed row.** Trait has one method, uncalled       |
| G-11 | `ToolProvisioning`      | MCP, plugins, skills, subagents                      | **(a)**  | Unused in v1, as designed. No gap                          |
| G-12 | `PromptComposition`     | agent-rules file + preamble                          | **(a)**  | Fits; shared body's mechanism prose stays true             |
| G-13 | `AwaitingInputSignal`   | `Notification` with `notificationType` / `level`     | **(a)**‡ | Plausible but **uncharacterised**; not declared in v1      |

† G-3 is class (a) on mechanism and blocked on isolation, not on capability. ‡ G-13 is implementable; the doc declines to declare it until the notification vocabulary is characterised — see [G-13](#g-13-awaitinginputsignal).

### G-1 `Spawn`

Fits cleanly. `SpawnRequest` / `SpawnPlan` already landed (`claude.rs:447-455`), so the driver supplies both its command and its environment directives — which is what Grok needs, because `GROK_HOME` must be _exported_, not passed as a flag.

Two Claude-shaped fields remain in `SpawnRequest` and are inert rather than wrong for Grok: `non_opus_auto_mode` (a Claude model-family concept) and `settings_path` (a single settings _file_, where Grok needs a directory). `permission_mode_override` maps directly, since Grok's `--permission-mode` accepts Claude's exact enum. No signature change is required for Grok; the inertness is noted so a fourth driver does not inherit the confusion silently.

The shared wrapper at the pane spawn site still hardcodes `unset ANTHROPIC_API_KEY`. Inert for Grok, wrong in principle, unchanged from the Codex project's finding.

### G-2 `WorkspaceProvisioning`

Fits the current trait. The Grok driver writes `.grok/initial-prompt.txt`, its agent-rules file, a per-run `GROK_HOME` (config, hooks, `auth.json` symlink, `trusted_folders.toml` pre-stamped with **both** the `/tmp` and `/private/tmp` forms of the workspace path), and asserts the result with `grok inspect --json`.

**The gap is not in the trait; it is in the driver-generic caller.** `core/src/worker_setup.rs:1702` calls `pre_trust_workspace(&input.workspace_path)` — which writes Claude's `~/.claude.json` — and `:1718` writes `CLAUDE_DIR_GITIGNORE`, both unconditionally, for every driver, from `write_workspace_files`. For a Grok worker these produce a Claude trust record for a workspace Claude will never run in, and a `.gitignore` inside a `.claude/` directory the driver did not create. Harmless today, incoherent, and it is exactly the residual coupling this project inherits. **Must fix**, not route around: [T-25](#t-25-make-pre-trust-and-config-dir-gitignore-driver-supplied).

Teardown remains unhooked on the trait, same as for Codex. Grok's per-run `GROK_HOME` accumulates a session tree per worker; because the home lives under the run directory it dies with the run, so this is materially less pressing than Codex's rollout accumulation. Noted, not filed.

### G-3 `PermissionPolicy`

Grok is the best-equipped of the three drivers here and simultaneously the only one with an isolation defect.

`ClaudeDriver::write_permission_config` is still a **functional no-op** returning `PermissionArtifacts::default()` (`claude.rs:552-564`); the real Claude renderer remains in `core/src/worker_setup.rs`. `CodexDriver` implements the method for real (`codex.rs:1091-1131`), writing into its run-private home. **Grok follows Codex**: it writes `$GROK_HOME/config.toml`, `$GROK_HOME/hooks/boss.json`, `$GROK_HOME/sandbox.toml`, and `$GROK_HOME/trusted_folders.toml`, and returns `PermissionArtifacts { config_files, extra_args, env }` with `--sandbox` / `--deny` / `--allow` in `extra_args` and `GROK_HOME` in `env`.

So Grok does **not** need the Claude extraction to land first. It routes around it. The extraction stays open, and the fact that a third driver has now routed around it is itself the argument for finishing it — filed as deferred rather than silently dropped ([T-28](#t-28-extract-claudes-permission-rendering-into-the-driver-crate)).

**The blocker is D-1.** Boss cannot declare that it applies Boss's permission policy while a Grok worker is also loading the machine owner's `~/.claude/settings.local.json`. [T-01](#t-01-investigation-claude-permission-settings-leakage-into-an-isolated-grok_home) must establish either a config key that stops it, a `HOME`-scoping strategy that does, or that the loaded rules are provably additive-and-restrictive-only (in which case the risk is bounded and the gate can be relaxed by an explicit decision rather than by assumption).

### G-4 `ModelAndEffortMenu`

Fits the `ModelMenu` struct as-is. `menu_for_driver_in` already resolves per slug through `DriverRegistry` and returns `UnknownDriverError` rather than silently falling back to Claude's table (`effort/src/lib.rs`), so registering `"grok"` at `registry.rs:46-47` is the whole integration.

The menu itself is thin: one model, a five-of-seven effort mapping. `engine_default` is `grok-4.5`; `model_for_reasoning` returns `grok-4.5` for both `Standard` and `Investigation`, because there is no second tier to choose. That is honest rather than degenerate — and it is the field most likely to be wrong within a quarter, hence the refresh path.

### G-5 `ProgressObservation` — solved transport, unsolved destination

**The Codex project's top gap does not reproduce.** Its finding was that `ProgressObservation` abstracts normalisation but not transport, and that pane-hosted workers had no engine ingress. Grok's ingress is the events socket, via hooks Boss installs in a home Boss owns, which are unconditionally trusted. That is the same production path Claude uses. Nothing new is built.

**What _is_ a gap is smaller and concrete: `ProgressIngress::HookCallback` does not say where its wiring goes.** The variant carries only `ProgressObservationWiring { hooks: serde_json::Map }` (`driver/src/lib.rs:784-790`), and `hooks_map_for_ingress` (`worker_setup.rs:647-652`) merges that map into the **Claude settings.json** the engine renders. Grok's hooks live in `$GROK_HOME/hooks/*.json`, written by the driver in `write_permission_config`. If Grok returns `HookCallback`, its map is written to a file Grok never reads; if it returns `StdoutJsonl` to dodge that, it lies about its transport and — worse — `pre_tool_use_array` (`worker_setup.rs:597`) then inserts the _interception guards_ into that same unread settings file, which is the silent-guardrail-loss failure again by a different route.

**Fix in the abstraction:** name the destination. Either a fourth variant (`HookCallbackDriverWritten`) or a `destination` field on `ProgressObservationWiring` distinguishing "merge into the worker settings file" from "the driver writes this itself". The second is preferable: it keeps one hook-callback concept and makes the engine's merge conditional on a declared property rather than on a variant match. [A-1](#proposed-abstraction-amendments) / [T-05](#t-05-progressingress-name-the-hook-wiring-destination).

`progress_fidelity()` is `Rich` for Grok — per-tool `PreToolUse`/`PostToolUse` events, same tier as Claude — and it is already consulted by the stale-worker sweep, so declaring it has real effect.

### G-6 `ToolUseInterception`

**Declared, deny-only, and gated on the payload adapter.**

The mechanism works: `PreToolUse` fires, `{"decision":"deny"}` blocks, exit-2 blocks, and the global-hook location means it is always armed. Two limits carry over from the spike and one is new to this doc:

- **Deny-only.** `updatedInput` did not rewrite in either the native or the Claude-shaped form. The trait's rewrite path is unreachable, same conclusion as Codex.
- **Fail-open on crash / malformed output / timeout.** Identical to Claude's production posture; not a regression.
- **The payload dialect breaks the guards** ([above](#payload-shape--the-reuse-boundary)). This is the new one, and it is the reason the capability is _gated_ rather than simply declared. A capability Boss declares is one Boss promises to enforce; with unadapted guards, Boss would be declaring enforcement it is not performing.

**The chosen fix is a single driver-owned canonicalisation adapter, not five script edits.** A small executable that reads the driver's stdin payload, rewrites it into Boss's canonical snake_case shape with canonical tool names, execs the unchanged guard script, and translates the script's `{"decision": "block"|"approve"}` output into whatever vocabulary the driver honours. Rationale:

- The five guard scripts stay byte-identical across drivers — one source of truth for what Boss blocks, which is the property that actually matters for safety review.
- The adapter is reusable by driver #4 by construction.
- Output translation is where the `"block"` vs `"deny"` uncertainty is absorbed, so an answer from [T-02](#t-02-investigation-grok-pretooluse-decision-vocabulary-and-tool-name-map) changes one file.

The alternative — parameterising every script on key names — was rejected: it multiplies the surface where a guard can be silently wrong, in the code whose correctness matters most.

### G-7 `TurnBoundary`

`Stop` maps directly onto `WorkerEvent::Stop`. The payload carries `reason` (`"end_turn"` observed), `stopHookActive` → `TurnEnd::continuation`, and `lastAssistantMessage`. Structurally identical to Claude's; no synthesizer needed.

**Except after an interrupt, and this is the sharpest lifecycle hazard in the project.** The spike records that Esc-cancelled turns **skip `Stop` hooks** entirely — the cancellation appears only in the session files:

```json
{
  "type": "turn_ended",
  "outcome": "cancelled",
  "cancellation_category": "mid_turn_abort",
  "cancellation_context": { "trigger": "esc" }
}
```

Boss uses interrupt. `bossctl` sends one; transient recovery sends one; a human sends one. Under this design an interrupted Grok worker would emit nothing to the engine after its last `PostToolUse`, and its slot would sit at `Working` until the stale-activity sweep eventually intervened — with the worker actually idle at its prompt, ready for input, the whole time.

Three candidate resolutions, evaluated in [T-12](#t-12-turn-end-recovery-for-esc-cancelled-grok-turns):

1. **Engine-side synthesis on interrupt.** The engine knows it sent the Esc; it can synthesise a `TurnEnd { reason: Interrupted }` after a bounded settle window. Cheapest, needs no new transport, but it asserts a state it did not observe — and if the Esc did not take (vim mode, a modal), the engine's model is now wrong in the optimistic direction.
2. **Tail `events.jsonl` for `turn_ended`.** Observes the real thing. Costs an `AgentJsonlFile`-style tail — which for Grok is _cheap_, because the path is fully constructible from `GROK_HOME` + `--cwd` + the assigned `-s` UUID. No glob, no correlation.
3. **`StopFailure`.** Documented as an event but never observed firing; the spike's `Stop`-skip note suggests it is not the interrupt path. Must be tested before being relied on.

**Recommendation: (2), with (1) as a bounded fallback.** It is the only option that observes rather than assumes, and Grok is the one driver where the file is nameable in advance rather than discoverable after the fact. This is also the honest answer to the brief's question about whether Grok has "a better option" than Codex's rollout tail: it does — but the reason to use it is interrupt correctness, not primary progress.

### G-8 `StructuredOutput`

The shared `BOSS_STRUCTURED_OUTPUT` env-file contract is driver-neutral and works for Grok unchanged — it is a file path the worker writes, not a mechanism the agent must support.

`--json-schema` is the interesting unknown ([D-5](#deltas-that-change-a-tasks-scope)). It would give Grok the same native, enforced contract Codex has and Claude lacks. Its `--help` text says it implies `--output-format json`, which is a headless notion, so its behaviour in an interactive TUI session is unestablished. [T-18](#t-18-structured-output-for-grok) evaluates it and falls back to the file contract, which is sufficient on its own.

PR-URL capture is `PostToolUse`-derived and reads `tool_response.stdout`. Grok sends `toolResult`. The adapter's canonicalisation covers the shape; whether Grok's shell tool nests its stdout the same way is a normaliser detail settled in [T-19](#t-19-pr-url-capture-for-the-grok-dialect).

### G-9 `TranscriptAccess`

**The best of the three drivers.** `transcript_path_for_session` is already on the trait and already called by `live_status_loop`; Grok's `transcriptPath` is stamped on every hook payload, so the implementation is a key rename. No glob (Codex), no derivation, no dir watch.

The line schema is Grok's ACP `sessionUpdate` dialect. Reuse `engine/transcript-tail` at container level; write a separate `TranscriptSessionNormalizer`. The tool-call/tool-result correlation the trait's per-tail state exists for is genuinely needed here: `tool_call` and `tool_call_update` are separate records.

### G-10 `ControlVerbs` — the absorbed row

Current state, verified: the trait has `classify_error` (`driver/src/lib.rs:1613`) and `mid_turn_pane_input` (`:1624`). **probe, interrupt, stop and reap are absent entirely.** `classify_error` is implemented by all three drivers and **called by none** — `core/src/transient_recovery.rs:339` calls `classify_claude_error` directly, bypassing the seam for every driver including Codex.

Grok is the first driver that makes all four verbs meaningful _and_ different enough from Claude's to be worth abstracting, because it is the first driver where they are all present but individually qualified:

| Verb               | Grok mechanism                               | Evidence                                                                                         | Qualification                                                 |
| ------------------ | -------------------------------------------- | ------------------------------------------------------------------------------------------------ | ------------------------------------------------------------- |
| **probe**          | `ghostty_surface_text` + Return              | spike Q8: `GKIT_PROBE_OK`, and a tool-style inject produced a real shell side-effect             | Post-turn and post-Esc proven; **mid-turn unproven**          |
| **interrupt**      | `ghostty_surface_key` Esc (`0x35`)           | spike Q8: `turn_ended outcome=cancelled`, `trigger: "esc"`, process survives, next turn accepted | **Skips the `Stop` hook** (G-7); no-op in fullscreen vim mode |
| **stop**           | `/quit` via SendToPane                       | spike Q8: `grok_exit=0`, shell returns                                                           | Graceful path only                                            |
| **reap**           | pane release, SIGTERM → SIGKILL on the group | spike Q8: headless SIGTERM → exit 143                                                            | Tool child shells need group reap                             |
| **classify_error** | xAI/Grok-specific error shapes               | —                                                                                                | **Must not** route through `classify_claude_error`            |

`mid_turn_pane_input` should structurally be `Buffers`: Grok is a long-lived interactive TUI that owns the pty for the whole session and does not exit between turns, so the `codex exec` footgun (bytes lingering for the shell after the process exits) cannot occur. **But the spike's probes were post-turn and post-Esc, not mid-turn.** The default is `Rejects` for exactly this reason (`lib.rs:659-664`), and the driver must not declare `Buffers` on a structural argument alone. [T-13](#t-13-grok-control-verb-implementation) proves it or leaves it at `Rejects`.

### G-11 `ToolProvisioning`

Grok has the richest surface of the three — MCP servers, plugins with a marketplace, skills, subagents, cross-session memory. **Boss injects none of it**, as the abstraction intended for v1 across every driver. **No gap.**

The v1 posture is a _decision_, though, not an absence: the driver should explicitly disable what it does not use (`--no-subagents`, `--no-memory`) rather than inheriting defaults, because a subagent or a memory carried across sessions is state Boss does not model and cannot reason about. Noted in [T-07](#t-07-grok-config-isolation-and-workspace-provisioning).

### G-12 `PromptComposition`

Fits. `render_claude_md` already takes `preamble` and `config_dir` from the descriptor (`worker_setup.rs:240`, `:1686-1716`), so the per-session agent-rules file is driver-routed already. Grok's descriptor supplies `config_dir = ".grok"`, its own `agent_rules_filename`, and a Grok-specific preamble.

The shared prompt body still hardcodes _"A PreToolUse hook blocks these"_. Under this design that sentence is **true for a Grok worker** — the mechanism really is a `PreToolUse` hook — so it is hygiene, not a correctness defect, exactly as the Codex project concluded. It stays deferred.

One Grok-specific wrinkle: because Grok resolves the workspace's `.claude/CLAUDE.md` as a project instruction under compat, a Grok worker would otherwise read Claude's worker-rules file _in addition to_ its own. `[compat.claude] rules = false` closes this, and the closure is verifiable with `grok inspect --json` rather than assumed.

### G-13 `AwaitingInputSignal`

Grok fires `Notification` with `notificationType`, `message`, and `level` — the right shape for the capability, and the interactive topology means there are real states to signal (permission prompts, tool approvals, the folder-trust dialog).

**Not declared in v1, deliberately.** The capability's own doc comment is unusually strict: absence is `Degrade`, **never** `Synthesize`, because a fabricated `WaitingForInput` is worse than an honest `Working`. Declaring it requires knowing _which_ `notificationType` values mean "blocked on a human", and the spike observed `Notification` firing without characterising the vocabulary. Under `--always-approve` / `bypassPermissions` most prompts are suppressed and `--trust` pre-empts the trust dialog, so the population of genuine awaiting-input events for a Boss worker may be near-empty — which is worth knowing before writing a mapping.

The cost of omission is bounded and correct: a Grok worker shows `Working` / `Idle` and never `WaitingForInput`. [T-24](#t-24-characterise-grok-notification-types-and-earn-awaitinginputsignal) earns the declaration with a cheap characterisation, and it does not gate the acceptance sweep. This is flagged for a human decision in the questions manifest.

---

## Guardrail integrity

Boss's safety properties are enforced today through Claude's `PreToolUse` hook. The required call per guardrail is **neither refuse nor degrade for any of them** — Grok carries all five on the same mechanism, with an adapter in front, and can additionally back several of them with levers Claude does not have.

### Per-guardrail calls

| Guardrail                              | Enforced today                              | Under Grok                                                                                   | Call                                     |
| -------------------------------------- | ------------------------------------------- | -------------------------------------------------------------------------------------------- | ---------------------------------------- |
| **Boss data-dir path guard**           | `PreToolUse` deny (`worker_setup.rs:1064+`) | Same script behind the adapter; optionally also a `--deny` rule and a `sandbox.toml` profile | **Preserved**, strengthenable            |
| **Reviewer read-only**                 | per-kind deny rules                         | `--sandbox read-only` — a mode that **fails closed**                                         | **Preserved, strengthened**              |
| **checkleft push guard**               | `PreToolUse` deny (`worker_setup.rs:1250+`) | Same script behind the adapter                                                               | **Preserved, same mechanism**            |
| **Revision-PR guard / no direct push** | `PreToolUse` deny (`claude.rs:305`, `:354`) | Same script behind the adapter                                                               | **Preserved, same mechanism**            |
| **Editorial enforcement**              | `PreToolUse` deny **and rewrite**           | deny works; inline-`--body` rewrite unreachable → deny-with-reason                           | **Preserved by deny-instead-of-rewrite** |

**Every row is contingent on the adapter.** Without it all five are present, firing, and permitting everything ([above](#payload-shape--the-reuse-boundary)). That contingency is the reason [T-02](#t-02-investigation-grok-pretooluse-decision-vocabulary-and-tool-name-map) and [T-09](#t-09-grok-hook-wiring-progress-forwarder-plus-guard-script-canonicalisation) sit ahead of any Grok worker running real work, and it is a **more urgent** gate than the Codex project's hook-trust gate was — Codex's failure mode was "hooks do not fire", which is at least uniform; Grok's is "hooks fire and approve", which is indistinguishable from healthy operation without a negative test.

**Therefore the adapter's acceptance criterion is a negative test, not a positive one:** a fixture worker that attempts each blocked command and is demonstrably refused. A test that only proves the hook _ran_ proves nothing here.

### The editorial case, precisely

`PreToolUseDecision` has three outcomes. Under Grok's deny-only `PreToolUse`:

- `Deny { reason }` — **works.** Grok's deny carries a reason to the model.
- `AllowWithRewrite { updated_command: None }` — **works.** The redaction is written into the `--body-file` on disk and the hook returns no decision, so the command proceeds against the corrected file.
- `AllowWithRewrite { updated_command: Some(cmd) }` — the inline `--body "..."` case. **Unreachable**: `updatedInput` did not apply in either form tested.

**The call: deny instead of rewrite**, identical to the Codex resolution. The safety property is fully preserved — unreviewed prose still never reaches GitHub — at the cost of one agent round-trip, and Boss's worker rules already forbid inline `--body` outright, so the deny enforces a documented convention rather than inventing a restriction.

### What Grok adds that neither peer has

Two levers worth taking _after_ the hook path works, not instead of it:

- **`--sandbox` fails closed.** _"refusing to start"_ is the property every guardrail wants and no hook has. Reviewer read-only in particular becomes a mode rather than a rule set.
- **Global-hook write protection.** The sandbox error text names it explicitly, which means the sandbox is the mechanism that stops a worker rewriting Boss's own guards in `$GROK_HOME/hooks/`. Nothing in the Claude or Codex paths has an equivalent.

Both are unvalidated beyond argument parsing, both are cheap to validate, and neither should be on the critical path for the first worker. [T-16](#t-16-investigation-grok-sandbox-profiles-and-allowdeny-rule-grammar) → [T-17](#t-17-grokdriverwrite_permission_config).

### The residual risk, stated plainly

Grok's guardrails inherit Claude's fail-open hook semantics (crash, malformed output, and timeout all permit) and add one Grok-specific silent-failure mode: a payload the adapter fails to canonicalise looks exactly like a permitted command. The mitigations are that the adapter is one file rather than five, that its acceptance criterion is a negative test, and that `grok inspect --json` can assert the hooks are registered before the worker starts. That is a better position than the Codex project shipped with, and it is not the same as safe — it is the honest ceiling of hook-carried guardrails, and it is why the `PATH`-shim follow-on project (from the Codex analysis, still unstarted) remains the right long-term answer for **all three** drivers.

---

## Alternatives considered

### Alternative 1: drive Grok headless (`-p` / `grok agent`), matching the Codex shape

Run one turn per process with `--output-format streaming-json` or `--output-format json`, keep the pane purely visual or drop it entirely, and reuse the Codex project's `StdoutJsonl` / `AgentJsonlFile` machinery wholesale.

**Rejected — and it is a settled scope decision, so the rejection is recorded rather than argued.** Beyond that, the technical case against it is strong on its own: it would forfeit every affordance this project exists to obtain. No Esc surface, so no interrupt; no live session, so probe becomes resume-as-new-process with all the pane-lifecycle complexity the Codex project is still carrying; no `AwaitingInputSignal` attachment point ever. It would also adopt the one channel the spike explicitly warns is not hook-equivalent (`streaming-json` is thought/text/end tokens, not lifecycle events), trading a rich event stream for a token stream. The only thing it buys is reuse of machinery that Grok does not need, because its transport problem is already solved.

### Alternative 2: teach the existing pane monitor Grok's strings alongside Claude's

Add Grok's TUI chrome to the `claudeVisible` / `busy` / `starting` string sets in `GhosttyTerminalView.swift:1102-1112` and ship.

**Rejected, and the rejection is a settled scope decision.** The supporting argument stands independently: the monitor already carries five literals for one driver, and the pattern is O(drivers × chrome-strings) in a Swift file that no driver author touches. Driver #4 would face the same edit with less context. There is also a subtler defect — the monitor's _state machine_ (`ClaudeMonitorTracker`, `TerminalPaneSession.swift:59-131`) encodes Claude-specific assumptions beyond the strings: a `❯` prompt prefix, a two-poll idle debounce, and a "prompt just submitted" heuristic. Adding strings would leave those wrong for Grok while looking fixed.

**Worth stating precisely what this alternative would and would not break**, because the brief overstates it: since the monitor feeds only a fallback pill superseded by engine `LiveWorkerState`, shipping Grok _without_ touching it costs a few seconds of a wrong label at spawn, not worker liveness. That is why the driver-supplied path is designed here but sequenced off the critical path.

### Alternative 3: have the app query the engine for per-slot driver identity

Instead of shipping monitor configuration to the app, let the app ask the engine "which driver is in slot N?" and keep a driver-keyed table of monitor behaviours app-side.

**Rejected.** It puts a synchronous dependency on a 0.5 s polling path (`startClaudeMonitor`, `GhosttyTerminalView.swift:1044-1052`), introduces a startup ordering problem (the monitor starts before the engine has necessarily registered the slot), and leaves the driver-specific knowledge in Swift anyway — the table just moves. The chosen approach ships the configuration _with the spawn request_, where the engine already has the driver resolved and the app already has a message to read.

### Alternative 4: session-file tail (`updates.jsonl`) as the primary progress transport

Skip hooks for progress and tail `updates.jsonl` from the engine, mirroring the Codex rollout design. The path is fully constructible in advance, so this is genuinely easier for Grok than it was for Codex.

**Rejected for v1, and worth keeping visible.** Hooks give a richer, already-normalised lifecycle stream through machinery running in production today; the file gives an ACP update stream needing a new parser and a new tail lifecycle. Hooks fail open on _handler_ errors, but the forwarder is a Boss binary in a Boss-owned home, which is the same trust posture Claude runs under. Choosing the file would mean building new machinery to avoid a risk Boss already accepts for its primary driver.

**But it is not fully rejected**: the file is the right answer for the one thing hooks provably cannot see, which is an Esc-cancelled turn ([G-7](#g-7-turnboundary)). The design adopts it _narrowly_, for interrupt observation, rather than as the progress channel.

---

## Chosen approach

Drive **`grok` as a full interactive TUI in a GhosttyKit pane**, seeded by a positional prompt from a file on disk, with a Boss-owned per-run `GROK_HOME` for isolation, **Claude-shaped hooks under that home as the progress transport** (the same events-socket path Claude uses in production), a **driver-owned canonicalisation adapter** in front of Boss's five unchanged guard scripts, `--sandbox` and `--deny` as defence in depth, and a **narrow `events.jsonl` tail** solely to observe Esc-cancelled turns.

### Execution shape

```sh
export GROK_HOME=<run-dir>/grok-home
grok \
  --model grok-4.5 \
  --reasoning-effort <resolved-from-effort-level> \
  --no-alt-screen \
  --always-approve \
  --trust \
  --session-id <boss-assigned-uuid> \
  --cwd <workspace> \
  --no-subagents \
  --no-memory \
  --sandbox <profile>                      # after T-16 validates the profile set
  "$(cat <workspace>/.grok/initial-prompt.txt)"
```

Notes on each non-obvious element:

- **Prompt from a file, not inline.** The spike verified brief-sized prompts (~41.6 KiB) through `--prompt-file` headless, and explicitly cautions against pasting tens of KB through `initial_input`. `$(cat …)` is the Claude pattern and it transfers.
- **`--trust` _and_ a pre-seeded `trusted_folders.toml`.** Redundant on purpose: `--trust` is hidden from `--help` (D-3), so the file is the belt that survives its removal. `GROK_FOLDER_TRUST=0` is rejected — it also ungates project hooks and MCP.
- **`--no-alt-screen`.** The spike verified both modes work under GhosttyKit and recommends inline for scrape and scrollback sanity. Whether `--minimal` is better still is an open decision for the human, flagged in the questions manifest.
- **`--always-approve`** rather than `--permission-mode bypassPermissions`: the observed payloads already report `permissionMode: "bypassPermissions"` under it, and it is the flag the spike executed.
- **No `-w` / `--worktree`, ever.** Cube owns workspace provisioning.
- **`--no-subagents` / `--no-memory`** are explicit posture, not defaults inherited by accident.
- **Vim mode must never be enabled** — Esc does not cancel in fullscreen vim mode, which would silently break interrupt.

The pane launch itself is unchanged from Claude's: the engine composes this as a shell command, the app hosts it via `SpawnWorkerPane` with `initial_input`, and the engine holds only `shell_pid`.

### The engine seams this needs

Four, and only the first is a genuine abstraction change:

1. **A destination for hook-callback wiring** ([G-5](#g-5-progressobservation--solved-transport-unsolved-destination)). `ProgressIngress::HookCallback` must distinguish "merge into the worker settings file" from "the driver writes this itself", so Grok's hooks land in `$GROK_HOME/hooks/` and the interception guards follow them there rather than into an unread settings.json.
2. **A driver-supplied hook-payload canonicalisation** ([G-6](#g-6-tooluseinterception)). One adapter executable, driver-configured, in front of the five unchanged guard scripts. Not a trait change so much as a driver-supplied artifact plus a place to declare it.
3. **`ControlVerbs` on the trait, and actually called** ([G-10](#g-10-controlverbs--the-absorbed-row)). probe / interrupt / stop / reap, plus routing `transient_recovery.rs:339` through `classify_error` instead of `classify_claude_error`.
4. **A narrow interrupt observer** ([G-7](#g-7-turnboundary)). A bounded tail of `$GROK_HOME/sessions/<pct-encoded-cwd>/<sid>/events.jsonl` looking for `turn_ended` with `outcome: "cancelled"`, active only around an interrupt. This reuses the existing `AgentJsonlFile` reader shape without adopting it as the progress transport.

Everything else — the events socket, the `boss-event` shim, the ordered fan-out, `LiveWorkerState`, the stale-worker sweep, the dispatch gate, the registry, the effort resolution — is reused unmodified.

### Pane and embedder role — the driver-supplied monitor

**What the pane monitor actually is.** `updateClaudeMonitorState` polls `ghostty_surface_read_text` every 0.5 s, builds a `ClaudeMonitorSnapshot` from five literal string tests, and feeds `ClaudeMonitorTracker` — a small state machine producing `unavailable | notDetected | ready | working`. That value is rendered as a status pill in `WorkersDetailView.swift:275-283`, **explicitly as a fallback**: the code comment reads _"Prefer engine-supplied LiveWorkerState — its activity is driven by hook events rather than a screen-scrape that always rendered 'Claude Unknown'. Fall back to the legacy claudeState pill until the worker's first hook fires."_ It reaches the engine nowhere.

**So the fix is a UI-fidelity fix, and the design should be proportionate.** No new RPC, no new socket, no app→engine channel. Ship the configuration with the spawn message the app already receives:

```rust
// boss-protocol, engine_app.rs — additive, all fields optional on the wire
pub struct PaneMonitorSpec {
    /// Substrings whose presence means "the agent is running in this pane".
    pub agent_markers: Vec<String>,
    /// Substrings meaning "a turn is in flight" (Claude: "esc to interrupt").
    pub busy_markers: Vec<String>,
    /// Substrings meaning "starting up, not yet at a prompt".
    pub starting_markers: Vec<String>,
    /// Line prefixes identifying the agent's input prompt (Claude: "❯").
    pub prompt_prefixes: Vec<String>,
    /// Polls of a stable prompt before declaring idle. Claude: 2.
    pub idle_debounce_polls: u8,
}

pub struct SpawnWorkerPaneInput {
    // … existing fields unchanged …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_monitor: Option<PaneMonitorSpec>,
}
```

The engine fills it from a new `AgentDriver::pane_monitor_spec() -> Option<PaneMonitorSpec>`. Swift renames `ClaudeMonitor*` → `PaneMonitor*`, replaces the hardcoded literals with spec lookups, and relabels the pill driver-neutrally (`Agent Unknown` / `Not Detected` / `Ready` / `Working`). `None` on the wire — an older engine, or a driver that declares no spec — keeps today's Claude behaviour, so the change is safe for every existing path.

**Why declarative markers rather than shipping behaviour.** The tracker's _structure_ (tail-change detection, prompt-submitted heuristic, idle debounce) is genuinely driver-agnostic; only the strings and one tuning constant are not. Shipping data keeps the state machine in one place, keeps the wire format trivially serialisable, and means driver #4 adds a Rust literal rather than a Swift branch.

**Grok's actual marker strings are not known.** The spike scraped for its own injected canaries, not for Grok's TUI chrome, and this doc will not invent them — a wrong `busy_marker` produces a monitor that is confidently incorrect, which is worse than the honest `notDetected` Grok gets today. Capturing them is [T-03](#t-03-investigation-grok-tui-liveness-markers-under-ghosttykit), a `small` GhosttyKit-hosted observation, and it must precede the Swift work. The spike also warns that markers can leave the viewport after scroll and that alt-screen teardown drops chrome entirely, so T-03 must record markers that are _stably_ present, not merely observed once.

### Capability declaration for `GrokDriver` (v1)

**Provided (11):** `Spawn`, `WorkspaceProvisioning`, `PermissionPolicy`, `ModelAndEffortMenu`, `ProgressObservation`, `ToolUseInterception` (deny-only), `TurnBoundary`, `StructuredOutput`, `TranscriptAccess`, `ControlVerbs`, `PromptComposition`.

**Not provided (2):**

- **`ToolProvisioning`** → default `Degrade`. Unused in v1 for every driver, including Claude, which declares it and injects nothing. Grok has MCP, plugins, skills and subagents; Boss injects none of them, and the driver explicitly disables subagents and memory. Declaring it would overclaim. `Degrade` is correct: no dispatch refusal, no synthesised tooling.
- **`AwaitingInputSignal`** → default `Degrade`, **never** `Synthesize`. The signal shape exists (`Notification` with `notificationType` / `level`) but its vocabulary is uncharacterised, and the capability's contract forbids guessing. A Grok worker shows `Working` / `Idle` and never a fabricated `WaitingForInput`. [T-24](#t-24-characterise-grok-notification-types-and-earn-awaitinginputsignal) earns it; the omission gates nothing.

Two conditions attach to declarations rather than qualifying them, and both are the driver's to satisfy before the declaration is honest:

- **`ToolUseInterception` is gated on the canonicalisation adapter** ([T-09](#t-09-grok-hook-wiring-progress-forwarder-plus-guard-script-canonicalisation)) and on the decision-vocabulary answer ([T-02](#t-02-investigation-grok-pretooluse-decision-vocabulary-and-tool-name-map)). Without them the guards fire and approve, which is a declaration Boss would not be honouring.
- **`PermissionPolicy` is gated on the compat-leak answer** ([T-01](#t-01-investigation-claude-permission-settings-leakage-into-an-isolated-grok_home)). Boss cannot claim to apply its policy while an un-scoped `~/.claude/settings.local.json` is also loaded.

`mid_turn_pane_input` starts at the safe default `Rejects` and moves to `Buffers` only if [T-13](#t-13-grok-control-verb-implementation) proves mid-turn injection is consumed by the agent.

### Which work-item kinds are Grok-eligible

Phased, with an acceptance criterion per phase, expressed through `KindRequirements`. Refusals are about **output-contract maturity**, not guardrails — guardrails are uniform across all kinds once the adapter lands.

**Phase 1 — chores and project tasks.** The plain "make a change, open a PR" loop. Acceptance: 10 consecutive chores dispatched `--driver grok` reach an open PR with green CI, no engine intervention, and PR-URL capture on the primary path (not a `jj log` reconstruction fallback).

**Phase 2 — design, investigation, postmortem.** Document-producing kinds, dependent on the `BOSS_STRUCTURED_OUTPUT` file contract and followups parsing. Acceptance: a Grok-authored design doc lands with a correctly parsed task-breakdown section and its followups materialise. Note that `TaskKind::Design` marks `StructuredOutput` and `ToolUseInterception` required-strict (`lib.rs:461`), so both must be genuinely declared — not merely present — before this phase.

**Phase 3 — review and conflict resolution.** Review needs `--sandbox read-only` verified as a real reviewer-read-only equivalent (including that the worker demonstrably _cannot_ write), plus structured `ReviewResult` output. Conflict resolution needs write access and the merge-conflict telemetry path.

**Phase 4 — triage and the answer agent. Reachable for Grok, unlike Codex.** The Codex project deferred these indefinitely because the answer agent depends on `UserPromptSubmit`-based delivery confirmation that Codex does not have, and because triage is transcript-scraped. **Grok has `UserPromptSubmit`** and is a live interactive session, so `pane_delivery`'s confirmation path works structurally. Triage remains blocked on the prose-scrape consumers that construct `ClaudeDriver` concretely ([T-26](#t-26-route-prose-scrape-fallback-consumers-through-the-resolved-driver)) rather than on anything Grok lacks. This is a genuine capability difference between the two non-Claude drivers and it should not be lost by copying the Codex phasing wholesale.

### Load-balancing seams

Design _for_, do not design _now_. Four attachment points, three shared with the Codex analysis and one new:

1. **Per-driver capacity accounting.** Slots are one global pool. The seam is the dispatch gate, which already resolves `(kind, driver)` and is the natural place for an in-flight count keyed by driver slug. Requirement on this project: **do not add a second, driver-blind admission path** — nothing in the hook wiring or the interrupt observer may spawn or admit work outside that gate.
2. **Per-provider rate-limit state.** Grok's hook payloads carry **no token usage** — a real asymmetry with Codex, whose `turn.completed` hands it over for free. Grok's session `summary.json` and `events.jsonl` may carry it; the progress reader is the place to record it if so. A balancer must not assume symmetry across the three drivers here: Claude has no in-band usage signal either.
3. **Capability-aware routing.** `CapabilityResolver::check_dispatch` already computes the predicate a balancer needs. Requirement on this project: keep it a **pure, side-effect-free query**, so a balancer can call it speculatively across candidate drivers.
4. **New — concurrency ceiling is per-provider and unmeasured.** The spike established 16 concurrent Grok sessions succeed on this account. That is a floor, not a ceiling, and it says nothing about the _combined_ load of Claude + Codex + Grok workers, which is what a balancer actually schedules. The seam is per-driver capacity (1); the missing input is a per-provider ceiling that nobody has measured for any of the three.

### Migration and coexistence

- **Auth.** `auth.json` inside a per-run `GROK_HOME`, symlinked from a host credential. No env-var collision with Claude; `unset ANTHROPIC_API_KEY` in the shared wrapper is inert.
- **Config collisions.** Solved by per-run `GROK_HOME` — except for Claude-compat permission discovery, which it does not solve (D-1).
- **Workspace layout.** Grok uses `.grok/`; Claude uses `.claude/`. They do not collide by name, but Grok **actively reads** `.claude/` under compat, so a workspace that has run both drivers needs the compat block off, not merely different directories. Both config dirs must be engine-gitignored.
- **Shared cube workspaces.** Workspaces are reused across runs and drivers. A workspace that ran a Claude worker yesterday still contains `.claude/CLAUDE.md` and a `.claude/settings.json` today. This is a normal, expected state, and it is precisely the state in which the compat hazard bites. Provisioning must be idempotent and compat-suppressing on every run, not only on a fresh workspace.
- **Cube.** No changes required. `cube pr create` is agent-neutral and remains the enforcement point the guards lean on. Grok's `-w` / `grok worktree` overlaps cube's job and stays unused.

---

## Risks / open questions

<a id="oq-1-claude-permission-settings-leak"></a>
**OQ-1 — Can Claude permission-settings discovery be scoped out of an isolated `GROK_HOME`?** The sharpest open question in the project and the one gate on spawn. `[compat.claude]` disables instructions, agents, skills, plugins and rules, and leaves `permissions.sources` loading `~/.claude/settings.local.json`. Options to evaluate: an undocumented config key; scoping `HOME` for the worker process (heavy-handed, and it would also move the credential); accepting the leak if the loaded rules are provably restrictive-only. **A fourth possibility must be tested and would change the severity entirely: that the reported source is discovered-and-listed but not _applied_ under `--always-approve`.** `grok inspect` says `loaded: 1`, which suggests applied, but "loaded" and "in force" are not the same claim. → [T-01](#t-01-investigation-claude-permission-settings-leakage-into-an-isolated-grok_home).

<a id="oq-2-decision-vocabulary"></a>
**OQ-2 — Does Grok honour `{"decision":"block"}`, and what is the canonical tool-name map?** Boss's five guard scripts emit `block` / `approve`; the spike proved `deny` and exit-2 block. If `block` is not recognised, every adapted guard fails open — the worst failure mode in this design, because it is indistinguishable from healthy operation. The tool-name half is equally load-bearing: the guards branch on `tool_name != "Bash"`, and Grok sends `run_terminal_command` / `write` / others. → [T-02](#t-02-investigation-grok-pretooluse-decision-vocabulary-and-tool-name-map).

<a id="oq-3-interrupt-without-stop"></a>
**OQ-3 — What ends a turn that was cancelled with Esc?** Esc-cancelled turns skip the `Stop` hook, so the engine's only turn-boundary signal never fires. Three candidate answers are laid out in [G-7](#g-7-turnboundary); the recommendation is to observe `events.jsonl` rather than synthesise, because Grok is the one driver whose session-file path is nameable in advance. A human should confirm that trade (a small new tail, versus an assumed state) before implementation. → [T-12](#t-12-turn-end-recovery-for-esc-cancelled-grok-turns).

<a id="oq-4-the-leader-process"></a>
**OQ-4 — The leader process.** `--leader-socket` defaults to `~/.grok/leader.sock` and `grok leader` manages running leaders. It is _assumed_ that a per-run `GROK_HOME` gives each worker its own leader — the socket path is home-derived — but this is unmeasured. If leaders are shared, or if one outlives its worker, then SIGTERM reap may not fully reap, and 16 concurrent workers may be sharing a process the engine does not know about. Uncharacterised in the spike and listed there as an open question.

**OQ-5 — Is `--minimal` a better pane mode than `--no-alt-screen`?** `--minimal` prints finalized blocks into native scrollback with a small pinned region, which sounds strictly better for surface scrape than either alt-screen or plain inline. It is described as experimental. The spike tested alt-screen and `--no-alt-screen`, not `--minimal`. This affects the pane monitor's marker stability ([T-03](#t-03-investigation-grok-tui-liveness-markers-under-ghosttykit)) and is worth resolving before the marker set is captured, not after. Flagged for a human decision.

**OQ-6 — Does `--json-schema` do anything useful in an interactive TUI session?** Its `--help` text implies `--output-format json`, a headless concept. If it works for a TUI, Grok gains a native structured-output contract stronger than Claude's. If not, the file contract is sufficient and nothing is lost. → [T-18](#t-18-structured-output-for-grok).

**Risk — the guards fail open in a way that looks healthy.** Stated once, plainly: a Grok worker with unadapted guards runs every blocked command while its hooks fire, log, and report success. Every other risk in this project degrades toward a stuck or noisy worker; this one degrades toward an unguarded one. The mitigations are the adapter being a single file, its acceptance criterion being a **negative** test, and `grok inspect --json` asserting registration pre-spawn. It is why [T-02](#t-02-investigation-grok-pretooluse-decision-vocabulary-and-tool-name-map) and [T-09](#t-09-grok-hook-wiring-progress-forwarder-plus-guard-script-canonicalisation) gate real work rather than merely preceding it.

**Risk — hook fail-open is inherited, not introduced.** Grok's crash / malformed / timeout behaviour is fail-open, identical to Claude's production posture. This is not a Grok regression and this project does not fix it. The `PATH`-shim follow-on project from the Codex analysis remains the right structural answer for all three drivers, and it is still unstarted.

**Risk — one version, one day of evidence.** Grok 0.2.112 is the only version anyone has characterised, and the design already depends on one flag hidden from `--help`. Pin, capture goldens, gate upgrades on the harness — and expect at least one surprise on the first upgrade, because the Codex project found four in eight minor versions with more history to go on.

**Materialised 2026-08-01, and the mitigation was the wrong shape.** Grok auto-updated 0.2.114 → 0.2.117 on its own, and the hard version pin turned that single automatic bump into a fail-closed provisioning outage for every Grok execution — the predicted surprise, except a hard version gate cannot "gate an upgrade" Boss never chose to make. The pin was removed; drift is now observed and logged (`LAST_CHARACTERISED_GROK_VERSION`) rather than gated. The hidden-flag risk this paragraph also names is unaffected and still mitigated by the (still fail-closed) `--trust`-flag and `grok models` conformance checks.

**Risk — the Swift work is small and lands last, which is when it is most likely to be dropped.** The monitor fix is genuinely off the critical path, which makes it the easiest thing to defer indefinitely once Grok workers are producing PRs. It should not be: the whole point of the settled decision is to stop the hardcoding compounding at driver #4. [T-15](#t-15-driver-supplied-pane-monitor-swift-half) gates the acceptance sweep for exactly this reason.

---

## Proposed abstraction amendments

Discrete and filed-work-item-sized. These are amendments to the agent-driver abstraction, feeding back the way the Codex project's amendment table did.

| #    | Proposed name                                                                           | Effort    | Amends / new                                                      | Brief                                                                                                                                                                                                                                                                                                                                                                                                                     |
| ---- | --------------------------------------------------------------------------------------- | --------- | ----------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A-1  | `ProgressIngress`: name the hook-wiring destination                                     | `small`   | **New**                                                           | `HookCallback` carries a hooks map with no statement of where it goes; `hooks_map_for_ingress` merges it into the Claude settings file and `pre_tool_use_array` appends the interception guards there too. A driver whose agent reads hooks from its own home gets both written to a file the agent never reads — silently. Add a declared destination so the merge is conditional on a property, not on a variant match. |
| A-2  | Driver-supplied hook-payload canonicalisation                                           | `medium`  | **New**                                                           | Boss's five guard scripts parse Claude's snake_case payload shape and Claude's tool names. Grok's are camelCase with native tool names, so the guards fire and approve. Make canonicalisation a driver-supplied artifact in front of unchanged scripts, so the scripts stay one source of truth and driver #4 supplies an adapter rather than editing safety code.                                                        |
| A-3  | `ControlVerbs`: probe / interrupt / stop / reap on the trait, and call `classify_error` | `medium`  | **Absorbs the abstraction project's blocked row**                 | The trait has `classify_error` and nothing calls it — `transient_recovery.rs:339` calls `classify_claude_error` directly for every driver. The four verbs are absent entirely. Grok is the first driver where all four are present, meaningful, and individually qualified, so this is where the seam becomes real.                                                                                                       |
| A-4  | Turn boundary must survive an interrupt that emits no turn-end event                    | `medium`  | **New**                                                           | Boss's turn boundary for hook-driven drivers is the `Stop` hook. Grok skips it on Esc-cancelled turns, pinning the slot at `Working`. The abstraction needs a way for a driver to report a turn end observed on a channel other than its primary progress ingress.                                                                                                                                                        |
| A-5  | Driver-supplied pane-monitor spec on the engine↔app spawn RPC                           | `medium`  | **New** — first engine/app protocol surface for a driver property | `GhosttyTerminalView.swift` hardcodes five Claude literals plus a Claude-shaped state machine. Add an optional `PaneMonitorSpec` to `SpawnWorkerPaneInput`, sourced from a new trait method. `None` preserves today's behaviour exactly.                                                                                                                                                                                  |
| A-6  | `PermissionPolicy`: complete the Claude extraction                                      | `medium`  | **Amends the abstraction project's extraction row**               | `ClaudeDriver::write_permission_config` still returns `PermissionArtifacts::default()` — a functional no-op — while the real renderer sits in `core/src/worker_setup.rs`. Two drivers have now routed around it. The workaround is now the pattern, which is the argument for finishing it.                                                                                                                               |
| A-7  | Driver-generic workspace provisioning must stop writing Claude artifacts                | `small`   | **New**                                                           | `write_workspace_files` calls `pre_trust_workspace()` (writes Claude's `~/.claude.json`) and `CLAUDE_DIR_GITIGNORE` unconditionally for every driver. Both belong behind the driver.                                                                                                                                                                                                                                      |
| A-8  | Prose-scrape fallback consumers must resolve the driver                                 | `medium`  | **New**                                                           | `completion/stop.rs:227`, `completion/finalize_passes.rs:44` and `:530`, and `attentions_detector.rs:503` each construct `crate::driver::ClaudeDriver` concretely for PR-URL, triage, `ReviewResult` and followups fallbacks. Blocks Grok for the design, review and triage kinds.                                                                                                                                        |
| A-9  | Remote/SSH dispatch must resolve the driver                                             | `large`   | **New** — deferred                                                | `core/src/app/worker_events.rs:619` hardcodes `ClaudeDriver.capabilities()` on the remote path. Generalising remote dispatch carries its own auth-distribution problem (`GROK_HOME` on remote hosts). Local dispatch is the v1 target.                                                                                                                                                                                    |
| A-10 | Conformance harness: a third driver, and a per-driver version pin                       | `medium`  | **Amends the harness**                                            | `core/src/conformance/` has `boundary_equivalence`, `ingress_equivalence`, `claude_goldens`, `version_pin` and a goldens tree — all shaped for Claude and Codex. Grok adds a third ingress dialect and a second live-CLI pin, including a hidden-flag assertion that `--help` would not catch.                                                                                                                            |
| A-11 | `ModelMenu` needs a documented refresh path                                             | `trivial` | **New**                                                           | Grok's menu is one model today and will not stay that way; Codex's moved twice in eight minor versions. A hardcoded table with no refresh story is a latent bug in every driver, not just this one.                                                                                                                                                                                                                       |
| A-12 | `mid_turn_pane_input` should be provable, not asserted                                  | `trivial` | **New** — deferred                                                | The enum defaults to `Rejects` so a new driver is safe until it establishes otherwise, which is right. What is missing is a shared way to _establish_ it — a fixture that proves mid-turn bytes reach the agent rather than the shell. Each driver currently argues it in a doc comment.                                                                                                                                  |

**Verdict on the inherited residual coupling**, as required: `write_permission_config`'s Claude no-op is **routed around** (Grok implements the method for real, as Codex does) and filed as deferred. `pre_trust_workspace` / `CLAUDE_DIR_GITIGNORE` **must be fixed** — they actively write Claude artifacts into a Grok workspace. The prose-scrape `ClaudeDriver` constructions **must be fixed** before the design / review / triage kinds, and are sequenced with those phases rather than ahead of Phase 1. The remote/SSH hardcode is **routed around** by scoping v1 to local dispatch. **This claim was false against the code as written and has since been fixed**: the events-socket accept loop resolved a single `ENGINE_DEFAULT_DRIVER`-slug driver for every connection, injected rather than hardcoded, but never re-resolved per connection — so every Grok hook event was normalised as Claude and dropped with `MissingField`. Injection made the accept loop _testable_; it did not make per-run resolution happen. The fix resolves the driver per connection from the payload's `_boss_run_id` via `WorkDb::get_execution_driver_slug` (`events_socket.rs`'s `resolve_connection_driver`), mirroring the precedent already used for stdout/JSONL ingress (`stdout_progress.rs`'s `driver_slug`-taking entry points).

---

## Appendix A: reproducing the spike

The gating spike's full harness, evidence tree and re-run instructions live in [ghostty-grok-pane-viability.md](../investigations/ghostty-grok-pane-viability.md) and its committed artifacts. The short form:

```sh
# 1. Isolated home. Never point at the user's ~/.grok.
export GROK_HOME=/tmp/grok-spike/home
mkdir -p "$GROK_HOME"
ln -sf ~/.grok/auth.json "$GROK_HOME/auth.json"    # symlink, don't copy the credential

# 2. Compat off — otherwise Claude hooks, rules and agents load under Grok.
cat > "$GROK_HOME/config.toml" <<'TOML'
[compat.claude]
hooks = false
agents = false
skills = false
plugins = false
rules = false
TOML

# 3. Assert the posture BEFORE running anything. This is the step the
#    original spike lacked, and it is what surfaced the permission leak.
grok inspect --json | jq '{grokVersion, projectTrusted, projectInstructions, permissions, hooks}'

# 4. Headless smoke (not the worker shape, but cheap):
grok -p "reply with exactly: OK" --always-approve \
     --session-id "$(uuidgen)" --cwd /tmp/grok-spike/cwd --output-format json
```

For the pane gate, materialise the pinned GhosttyKit prebuilt whose sha256 matches `MODULE.bazel`'s `@ghostty_kit`, point the throwaway host at it, and run the committed scenarios (`seed_observe`, `esc_interrupt`, `resize`, `alt_screen`) — the exact commands are in the spike's Appendix B. **Pane verdicts come from GhosttyKit-hosted panes only**; standalone terminal experiments do not count.

The probes that produced this doc's new findings, all cheap and none requiring an inference call:

```sh
# D-1: the permission leak, and what the compat block does and does not fix
GROK_HOME=/tmp/empty-home grok inspect --json | jq '.permissions'

# D-2: which sandbox profiles are built in (resolved vs "not found")
for p in workspace read-only strict off none workspace-write danger-full-access; do
  printf '%-20s ' "$p"; GROK_HOME=/tmp/no-auth grok --sandbox "$p" models 2>&1 | head -1
done

# D-2: are --deny rule strings validated at parse time? (they are not)
GROK_HOME=/tmp/no-auth grok --deny '((((' models

# D-3: --trust is accepted but absent from --help
grok --help | grep -c trust        # 0
grok --trust --cwd /tmp -p x       # runs
```

---

## Proposed implementation task breakdown

Dependency-ordered. Each entry is sized to one reviewable PR by one worker in one session.

### T-01 Investigation: Claude permission-settings leakage into an isolated `GROK_HOME`

Establish whether `~/.claude/settings.local.json` (reported by `grok inspect --json` as a loaded permission source even under a fresh isolated home with the full `[compat.claude]` disable block) is actually _in force_ for a Grok run, and if so how to scope it out. Test at minimum: an undocumented compat or permissions config key; a scoped `HOME` for the worker process; and a direct behavioural test — write a restrictive deny rule into a throwaway `~/.claude/settings.local.json` and observe whether a Grok run under an isolated home honours it. Also determine whether the probed `/Library/Application Support/ClaudeCode/managed-settings.json` path can affect a run. Output is a written finding plus a reproducible harness, not code.

- **Effort:** `small`
- **Depends on:** none
- **Scope: in-scope** — gates T-07 and everything downstream; Boss cannot honestly declare `PermissionPolicy` until this is answered

### T-02 Investigation: Grok `PreToolUse` decision vocabulary and tool-name map

Two questions, both blocking the guardrail path. First: does Grok honour `{"decision":"block"}` and `{"decision":"approve"}` — the vocabulary Boss's five guard scripts emit — or only `{"decision":"deny"}` and exit-2, which is all the gating spike proved? Second: enumerate the tool names Grok sends in `toolName` for the tools Boss's guards care about (shell execution, file write, file edit), and produce the canonical map from Grok's names to the names the guards branch on. Output is a written finding plus a fixture set of real payloads, not code.

- **Effort:** `small`
- **Depends on:** none
- **Scope: in-scope** — gates T-09; a wrong answer here means every guard fails open while appearing healthy

### T-03 Investigation: Grok TUI liveness markers under GhosttyKit

Capture the surface strings that stably indicate, for a Grok TUI in a GhosttyKit-hosted pane, that (a) the agent is present, (b) a turn is in flight, (c) the session is still starting, and (d) the input prompt line prefix. Must be run under the pinned GhosttyKit prebuilt, not a standalone terminal. Record marker _stability_, not just presence — the gating spike warns that markers leave the viewport after scroll and that alt-screen teardown drops chrome entirely. Capture under each candidate pane mode (`--no-alt-screen`, `--minimal`, default) so the mode decision and the marker set are settled together. Output is the concrete `PaneMonitorSpec` field values, not code.

- **Effort:** `small`
- **Depends on:** none
- **Scope: in-scope** — gates T-15; the design deliberately does not invent these strings

### T-04 `ControlVerbs` trait surface, plus route error classification through it

Put probe / interrupt / stop / reap on the `AgentDriver` trait with driver-neutral signatures, and change `core/src/transient_recovery.rs:339` to call `classify_error` through the resolved driver instead of `classify_claude_error` directly. Driver-agnostic work with no Grok dependency: Claude and Codex implementations are the existing behaviour moved behind the seam. This is the absorbed row from the agent-driver abstraction project.

- **Effort:** `medium`
- **Depends on:** none
- **Scope: in-scope**

### T-05 `ProgressIngress`: name the hook-wiring destination

Distinguish "merge this hooks map into the worker settings file" from "the driver writes this wiring itself", so a driver whose agent reads hooks from its own home does not have both its forwarder and its interception guards written into a file the agent never opens. Update `hooks_map_for_ingress` and `pre_tool_use_array` to respect the declaration. Claude's behaviour must be byte-identical afterwards.

- **Effort:** `small`
- **Depends on:** T-04 — **file overlap**, not logical dependency: both edit `driver/src/lib.rs`. Land T-04 first and forward-port its trait additions preservingly.
- **Scope: in-scope**

### T-06 `GrokDriver` skeleton: descriptor, capability set, model menu, registry entry

The crate and struct: `DriverDescriptor` (slug `grok`, binary `grok`, config dir `.grok`, agent-rules filename, initial-prompt filename), the `ModelMenu` (`grok-4.5` default, the five-of-seven effort mapping from this doc), the capability set per the declaration section, and registration alongside `claude` and `codex`. No spawning, no wiring, no hooks. Every capability omission carries its `AbsenceDisposition` and rationale as a comment, following the Codex driver's precedent.

- **Effort:** `medium`
- **Depends on:** none — may run in parallel with T-04 and T-05; touches a new file plus the registry, not the trait
- **Scope: in-scope**

### T-07 Grok config isolation and workspace provisioning

Implement `provision_workspace`: a per-run Boss-owned `GROK_HOME` under the execution runtime dir, an `auth.json` symlink to the host credential, a `config.toml` carrying the full `[compat.claude]` and `[compat.cursor]` disable block plus whatever T-01 established, a `trusted_folders.toml` pre-stamped with both the `/tmp` and `/private/tmp` forms of the workspace path, `.grok/initial-prompt.txt`, and the agent-rules file. Assert the resulting posture with `grok inspect --json` before returning, failing loudly if compat is on, the folder is untrusted, or the version does not match the pin.

- **Effort:** `medium`
- **Depends on:** T-01, T-06
- **Scope: in-scope**

### T-08 `GrokDriver::spawn_invocation` and pane launch

Emit the `SpawnPlan` for the execution shape in this doc: `GROK_HOME` exported via the plan's env directives, the flag set including `--trust`, `--session-id` with a Boss-assigned UUID, `--cwd`, `--no-alt-screen` (or whatever T-03 settles), `--no-subagents`, `--no-memory`, and the positional prompt read from the provisioned file. Must never emit `-w` / `--worktree`. Produces a Grok worker that starts and runs its seeded turn; progress is not yet observed.

- **Effort:** `medium`
- **Depends on:** T-07
- **Scope: in-scope**

### T-09 Grok hook wiring: progress forwarder plus guard-script canonicalisation

Write Boss's hook configuration into `$GROK_HOME/hooks/`: the `boss-event` forwarder on every lifecycle event, and the five interception guards. Both go behind a single driver-owned canonicalisation adapter that rewrites Grok's payload into Boss's canonical shape (using T-02's tool-name map) and translates the guards' `block` / `approve` output into whatever vocabulary T-02 established. The five guard scripts themselves must remain byte-identical. **Acceptance is a negative test**: a fixture worker attempts each blocked command and is demonstrably refused — proving the hook _ran_ proves nothing, because the failure mode under an unadapted payload is a hook that runs and approves.

- **Effort:** `large`
- **Depends on:** T-02, T-05, T-08
- **Scope: in-scope**

### T-10 `GrokDriver` progress normaliser

Map Grok's hook payload dialect onto `WorkerEvent` in a `ProgressSessionNormalizer`: camelCase keys to canonical, `hookEventName` snake values to canonical event names, `toolName` / `toolInput` / `toolResult`, `stopHookActive`, and `sessionId`. Confirm rather than assume that `source: "new"` reaches `SessionStartSource::Other` and that the reducer's `Spawning → Idle` transition fires. Unknown event names must be ignored-with-logging, not rejected — Grok documents fourteen hook events and the spike observed six.

- **Effort:** `medium`
- **Depends on:** T-09
- **Scope: in-scope**

### T-11 `TranscriptAccess` for Grok

Implement `transcript_path_for_session` from the `transcriptPath` field stamped on every hook payload, and write a `TranscriptSessionNormalizer` for the ACP `sessionUpdate` dialect in `updates.jsonl`. Reuse `engine/transcript-tail` at container level only; the per-tail correlation state is genuinely needed because `tool_call` and `tool_call_update` arrive as separate records. Do not share a parser with either the Claude or the Codex dialect.

- **Effort:** `medium`
- **Depends on:** T-10
- **Scope: in-scope**

### T-12 Turn-end recovery for Esc-cancelled Grok turns

Close the hazard that an Esc-cancelled turn skips the `Stop` hook, leaving the slot pinned at `Working` while the worker sits idle at its prompt. Implement the recommended approach — a bounded tail of `$GROK_HOME/sessions/<pct-encoded-cwd>/<sid>/events.jsonl` for `turn_ended` with `outcome: "cancelled"`, active only around an interrupt — reusing the existing agent-JSONL reader shape. The path is fully constructible from the run's `GROK_HOME`, `--cwd` and Boss-assigned session UUID, so no glob or correlation step is needed. Include the bounded synthesis fallback for the case where no cancellation record appears within the settle window.

- **Effort:** `medium`
- **Depends on:** T-04, T-10
- **Scope: in-scope**

### T-13 Grok control-verb implementation

Implement the four verbs for Grok against the trait surface T-04 established: probe as typed pane input, interrupt as Esc, stop as `/quit` followed by pane release, reap as SIGTERM→SIGKILL on the process group including tool child shells. Implement `classify_error` against Grok/xAI error shapes — explicitly not routed through Claude's classifier. Determine empirically whether mid-turn pane input is consumed by the agent rather than left in the tty, and set `mid_turn_pane_input` accordingly; leave it at the safe `Rejects` default if it is not proven.

- **Effort:** `medium`
- **Depends on:** T-12
- **Scope: in-scope**

### T-14 Driver-supplied pane-monitor spec: protocol and engine half

Add `PaneMonitorSpec` to `boss-protocol` and an optional `pane_monitor` field to `SpawnWorkerPaneInput`, plus an `AgentDriver::pane_monitor_spec()` trait method, and populate it at the pane-spawn call site from the resolved driver. Claude's spec reproduces today's five literals and its two-poll idle debounce exactly. `None` on the wire must preserve current app behaviour, so an older app paired with a newer engine is unaffected. Rust only — no Swift in this PR.

- **Effort:** `medium`
- **Depends on:** T-06
- **Scope: in-scope**

### T-15 Driver-supplied pane monitor: Swift half

Rename `ClaudeMonitorSnapshot` / `ClaudeMonitorTracker` / `ClaudeMonitorState` to driver-neutral `PaneMonitor*` names, replace the five hardcoded literals in `makeClaudeSnapshot` with lookups against the spec delivered on the spawn message, make the idle-debounce constant spec-supplied, and relabel the status pill driver-neutrally. Absent spec falls back to today's Claude literals so no existing path changes behaviour. Uses the marker values T-03 captured.

- **Effort:** `medium`
- **Depends on:** T-03, T-14
- **Scope: in-scope**

### T-16 Investigation: Grok sandbox profiles and allow/deny rule grammar

Characterise the two permission levers that only surface probes have touched. For `--sandbox`: what the built-in profiles (`workspace`, `read-only`, `strict`, `off`, `none`) actually enforce, whether `read-only` is a genuine reviewer-read-only equivalent, what the `sandbox.toml` custom-profile schema accepts, and what "direct global-hook write protection" covers. For rules: whether the grammar accepts Claude's `Bash(rm -rf:*)` spelling, Grok's native tool names, or both — and what happens to a malformed rule, given that `--deny '(((('` is accepted at parse time without complaint. Output is a written finding plus fixtures.

- **Effort:** `small`
- **Depends on:** none
- **Scope: in-scope** — gates T-17; the design must not rely on a mechanism whose grammar is unvalidated

### T-17 `GrokDriver::write_permission_config`

Render Grok's permission artifacts into the per-run home: the `--sandbox` profile selection (including reviewer read-only), any custom `sandbox.toml` profile, the `--deny` / `--allow` rule set expressing Boss's structural deny set, and the `--permission-mode` selection — returned as `PermissionArtifacts { config_files, extra_args, env }`. Follows the Codex driver's precedent of implementing the method for real rather than routing around it. Uses T-16's findings for both the profile names and the rule spelling.

- **Effort:** `medium`
- **Depends on:** T-07, T-16
- **Scope: in-scope**

### T-18 Structured output for Grok

Wire the shared `BOSS_STRUCTURED_OUTPUT` env-file contract for Grok, which is driver-neutral and sufficient on its own. Separately evaluate whether `--json-schema` is meaningful for an interactive TUI session, given that its documented behaviour implies `--output-format json` — a headless concept. If it works, surface it as the stronger native contract; if not, record the negative result so a later pass does not re-investigate.

- **Effort:** `medium`
- **Depends on:** T-10
- **Scope: in-scope**

### T-19 PR-URL capture for the Grok dialect

PR-URL capture is triggered by `PostToolUse` and reads the tool response's stdout. Supply the Grok-dialect feed text — `toolResult`, canonicalised by the adapter — to the shared URL matcher and command gates, and verify the shell tool's output nests stdout where the extractor expects. The PR URL is the acceptance criterion for nearly every work item, so this must work on the primary path rather than via the reconstruction fallback.

- **Effort:** `small`
- **Depends on:** T-10
- **Scope: in-scope**

### T-20 Conformance: Grok goldens and version pin

Extend `core/src/conformance/` with a third driver: Grok payload goldens, an ingress-equivalence assertion that Grok's hook ingress produces the same `WorkerEvent` sequence shape as Claude's for equivalent activity, a boundary-equivalence assertion that a turn boundary drives completion identically, and a live-CLI version pin. The pin must assert the **hidden** `--trust` flag still parses — a `--help` diff would not catch its removal — and that `grok models` still matches the descriptor's menu. Soft-skip when the binary is absent, with an opt-in env var to require it, following the existing Codex pin's pattern. A validation campaign over the implementations above, deliberately sequenced after them.

**Superseded 2026-08-01 — the live-CLI _version_ pin was removed (operator decision).** It fired on every one of Grok's own automatic updates — 0.2.114 → 0.2.117 broke it within days of landing — turning routine drift into a fail-closed provisioning outage before any worker was even attempted, which is not a useful thing for a hard gate to do to a CLI Boss does not control the update cadence of. `grok::home::assert_inspect_json_posture` now only observes `grokVersion` and logs a `tracing::warn!` on drift from `LAST_CHARACTERISED_GROK_VERSION`; it never gates, `bail!`s, or fails a test. The observed version is recorded on the execution record (`GrokRuntimeState::grok_version`) rather than discarded. The hidden `--trust` flag and `grok models` menu assertions this item also called for are unaffected and still fail closed — see `version_pin::hidden_trust_flag_still_parses_on_installed_grok` and `version_pin::grok_models_menu_matches_pinned_descriptor`.

- **Effort:** `medium`
- **Depends on:** T-11, T-13, T-17, T-18, T-19
- **Scope: in-scope**

### T-21 Phase-1 acceptance sweep: 10 Grok chores to green PRs

Dispatch 10 consecutive chores with `--driver grok` and verify each reaches an open PR with green CI, no engine intervention, and primary-path PR-URL capture. Record per-run wall-clock and any manual intervention so the "quality and speed" premise of the project has evidence behind it. A sweep, not an implementation — listed separately and after the work it validates.

- **Effort:** `medium`
- **Depends on:** T-15, T-20
- **Scope: in-scope**

### T-22 Grok eligibility for design / investigation / postmortem kinds

Phase 2: enable the document-producing kinds via `KindRequirements` and verify a Grok-authored design doc's task-breakdown section parses and its followups materialise. Note that the design kind marks `StructuredOutput` and `ToolUseInterception` required-strict, so both must be genuinely declared and honoured before this phase can be enabled.

- **Effort:** `medium`
- **Depends on:** T-21, T-26
- **Scope: in-scope**

### T-23 Grok eligibility for review and conflict-resolution kinds

Phase 3: verify `--sandbox read-only` is a genuine reviewer-read-only equivalent — including that the worker demonstrably _cannot_ write to the workspace, not merely that it declines to — and that structured `ReviewResult` output round-trips. Conflict resolution additionally needs write access and the merge-conflict telemetry path exercised.

- **Effort:** `medium`
- **Depends on:** T-22
- **Scope: in-scope**

### T-24 Characterise Grok `Notification` types and earn `AwaitingInputSignal`

Determine which `notificationType` / `level` values positively mean "blocked awaiting a human" as opposed to informational, and whether any of them can occur for a Boss worker at all given `--always-approve` and pre-seeded folder trust. If a genuine awaiting-input signal exists, declare the capability and map it; if the population is empty in practice, record that and leave the capability undeclared. The capability's contract forbids synthesising this state from a lower-fidelity channel, so a negative result is a valid and useful outcome.

- **Effort:** `small`
- **Depends on:** T-10
- **Scope: in-scope** — does not gate the acceptance sweep; a Grok worker shows `Working` / `Idle` meanwhile

### T-25 Make pre-trust and config-dir gitignore driver-supplied

`write_workspace_files` calls `pre_trust_workspace()` — which writes Claude's `~/.claude.json` — and writes `CLAUDE_DIR_GITIGNORE` unconditionally for every driver. For a Grok worker both produce Claude artifacts for a workspace Claude will never run in. Move both behind the driver so each supplies its own pre-trust action and its own config-dir gitignore content.

- **Effort:** `small`
- **Depends on:** T-07
- **Scope: in-scope**

### T-26 Route prose-scrape fallback consumers through the resolved driver

`completion/stop.rs:227`, `completion/finalize_passes.rs:44` and `:530`, and `attentions_detector.rs:503` each construct `crate::driver::ClaudeDriver` concretely for the PR-URL, triage, `ReviewResult` and followups fallbacks. Replace each with a registry resolution. Sequenced with the Phase 2/3 kinds rather than ahead of Phase 1, because chores reach a PR on the primary capture path without touching these fallbacks.

- **Effort:** `medium`
- **Depends on:** T-19
- **Scope: in-scope**

### T-27 Investigation: the Grok leader process under concurrent workers

Characterise `grok leader` / `--leader-socket`: whether per-run `GROK_HOME` isolation gives each worker its own leader (the socket path is home-derived, so it should, but this is unmeasured), whether a leader outlives its worker, and whether SIGTERM reap of the pane process group actually reaps it. Listed as an open question by the gating spike and unresolved here. A leader that is shared or that survives reap would mean Boss has an unmodelled process per worker.

- **Effort:** `small`
- **Depends on:** T-13
- **Scope: deferred (future / not a v1 blocker)** — a leaked helper process is an operational annoyance rather than a correctness failure, and per-run home isolation makes sharing unlikely; promote if reap proves unreliable in the acceptance sweep

### T-28 Extract Claude's permission rendering into the driver crate

`ClaudeDriver::write_permission_config` still returns `PermissionArtifacts::default()` — a functional no-op — while the real settings.json, deny-rule and hooks rendering lives in `core/src/worker_setup.rs`. Two drivers have now implemented the method for real and routed around Claude's no-op. Port the rendering across the one-way `core → driver` boundary and complete the extraction.

- **Effort:** `medium`
- **Depends on:** none
- **Scope: deferred (future / not a v1 blocker)** — Grok implements the method for real and does not need Claude's extraction to land; filed so the workaround becoming the pattern stays visible rather than silently accepted

### T-29 Remote/SSH dispatch for Grok

The remote path hardcodes `ClaudeDriver.capabilities()` and the remote runner script is Claude-shaped end to end. Generalising it carries its own auth-distribution problem — `GROK_HOME` and its `auth.json` would need provisioning on each remote host.

- **Effort:** `large`
- **Depends on:** T-21
- **Scope: deferred (future / not a v1 blocker)** — local dispatch is the v1 target

### T-30 Per-driver capacity and rate-limit accounting seams

Attach per-driver in-flight accounting at the dispatch gate, and record per-turn usage against the driver where a channel carries it. Grok's hook payloads carry no token usage, so this must also establish whether the session `summary.json` or `events.jsonl` does — and if neither, record the asymmetry explicitly so a future balancer does not assume symmetry across the three drivers. Seams only; no routing policy.

- **Effort:** `medium`
- **Depends on:** T-21
- **Scope: deferred (future / not a v1 blocker)** — load balancing is explicitly out of scope; this only ensures the seams exist and are not foreclosed

### T-31 Grok eligibility for triage and the answer agent

Phase 4. The Codex project deferred these indefinitely because the answer agent depends on `UserPromptSubmit`-based delivery confirmation Codex does not have. **Grok has it**, and is a live interactive session, so the confirmation path works structurally. What remains is triage's transcript-scraped decision parsing, which is blocked on T-26 rather than on anything Grok lacks. Listed as its own entry so this genuine Grok-vs-Codex capability difference is not lost by copying the Codex phasing.

- **Effort:** `medium`
- **Depends on:** T-23, T-26
- **Scope: deferred (future / not a v1 blocker)** — Phase 4 by sequencing, not by capability; promote once Phase 3 lands

### Parallelism and file-overlap cautions

**Depth 0 — six entries, genuinely independent, may run concurrently:** T-01, T-02, T-03, T-04, T-06, T-16. Also T-28, which is unblocked but deferred.

Start **T-01 and T-02 first regardless of slack.** They are both `small`, and they gate the two things that would otherwise be discovered late and expensively: whether Boss's permission posture is actually in force, and whether Boss's guards actually block. T-03 and T-16 are also `small` and gate later work, but a wrong answer there is visible rather than silent.

**Depth 1:** T-05 (after T-04), T-07 (after T-01 + T-06), T-14 (after T-06). T-07 and T-14 may run concurrently.

**Depth 2 onward:** T-08 → T-09 → T-10 fan out into T-11, T-12, T-18, T-19 and T-24, which are mutually independent and may run in parallel. T-15 (after T-03 + T-14) is independent of the entire progress chain and may run alongside any of them.

**File-overlap cautions — order these rather than running them concurrently:**

- **T-04 and T-05** both edit `driver/src/lib.rs` substantially: T-04 adds four trait methods, T-05 changes an enum and its consumers. The dependency edge is file overlap, not logic. Land T-04 first; T-05 forward-ports its additions preservingly rather than replacing them.
- **T-09 and T-17** both write into `$GROK_HOME` from the driver — T-09 the hooks, T-17 the permission artifacts. They are logically independent but will co-edit the driver's home-provisioning helpers. Land T-09 first (it is on the critical path and larger); T-17 integrates.
- **T-11, T-12, T-18, T-19 and T-24** all edit the Grok driver module. The overlap is incidental rather than substantial — different methods, different files once the driver is split into a `grok/` submodule directory the way `claude/` and `codex/` already are — so they stay parallel. **T-06 should create that submodule directory** rather than a single `grok.rs`, precisely so this fan-out does not serialise on one file.
- **T-25 and T-07** both touch workspace provisioning, from opposite sides: T-07 adds Grok's, T-25 removes Claude's from the driver-generic path. The edge serialises them; keep it.

**Not in this graph:** the `PATH`-shim project inherited from the Codex analysis is independent of everything above and remains the right structural answer to hook fail-open for all three drivers.
