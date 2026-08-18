# Grok as a first-class interactive agent driver

- **Date:** 2026-07-27 (design); 2026-08-10 (reconciled to as-built)
- **Status:** **Shipped for local interactive-TUI workers.** Grok dispatches chores, project tasks, and the document-producing kinds (`Design` / `Investigation` / `DesignPostmortem`) end-to-end on the Claude topology. Residual constraints: review-pool driver pin, `CommandOutcomeObservation` required-strict for conflict-resolution / CI-remediation, and triage / answer-agent still deferred.
- **Project:** Grok as a first-class interactive agent driver
- **Runs in parallel with:** the [agent-driver abstraction](agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md) close-out and the [Codex driver](codex-as-a-first-class-agent-driver.md) project. **No dependency edges on either.**
- **Structural template:** [codex-as-a-first-class-agent-driver.md](codex-as-a-first-class-agent-driver.md) (PR #2285). This doc mirrors its section list deliberately; where the two reach different conclusions, the difference is called out rather than smoothed over.
- **Gating spike:** [ghostty-grok-pane-viability.md](../investigations/ghostty-grok-pane-viability.md) (PR #2458, 2026-07-27). Its executed evidence is authoritative over anything in this doc's framing.
- **Boss tree verified at (original):** `6b2a4ee6` (`main`), 2026-07-27
- **Grok verified at:** `grok 0.2.112` → auto-updated through `0.2.114` / `0.2.117` during implementation; catalog and effort ladder re-verified on `grok 1.0.0` (2026-08-09)
- **Absorbed row:** the abstraction project's one remaining blocked item — _"agent-driver: ControlVerbs trait surface + call classify_error"_ — landed here as [T-04](#t-04-controlverbs-trait-surface-plus-route-error-classification-through-it) (mono#2472).
- **Shipped in:** mono#2468–#2472, #2482–#2483, #2490, #2498, #2511, #2513, #2516–#2517, #2520, #2522, #2525, #2529, #2537, #2543, #2551, #2570, #2584, #2597, #2624, #2700 (and the gating spike #2458). Per-task PR links live on each breakdown entry below.

## TL;DR / verdict

**Grok is the easiest of the three drivers to land, and the reason is structural: it is the same execution topology Boss already runs.** An interactive TUI in a GhosttyKit pane, seeded by a positional prompt, alive across turns, interruptible with Esc, probeable with typed input. Everything the Codex project had to invent — a transport for pane-hosted progress, a resume-as-new-process probe model, a way to reason about a worker that exits between turns — Grok simply does not need.

**The single highest-severity conclusion of the Codex project does not apply here.** That doc's verdict was: _"`ProgressObservation` abstracts event normalisation but not event transport, and the transport for pane-hosted workers into the engine is still an open seam."_ For Grok that seam is closed by construction. Boss owns `GROK_HOME`; global hooks under `$GROK_HOME/hooks/` run **unconditionally** (Grok's folder-trust gate applies to _project_ hooks, not to the driver's own home); Grok fires the Claude-shaped lifecycle event set; and every payload carries `transcriptPath`. So Grok reuses `ProgressIngress::HookCallback` with `HookWiringDestination::DriverOwned`, the existing `boss-event` shim, and the existing events socket — the same channel Claude uses in production today. **No new transport, no rollout tail, no app→engine IPC.**

**But the reuse stops at the transport — and that finding held.** Grok's hook payloads are camelCase with snake_case event values and Grok-native tool names (`hookEventName: "pre_tool_use"`, `toolName: "write"` / `"run_terminal_command"`, `toolInput`, `toolResult`). Boss's five guard scripts and `normalize_hook_event` expect snake_case Claude vocabulary. As shipped, a single driver-owned canonicalisation adapter (mono#2490) rewrites the payload, execs the **unchanged** guard scripts, and translates Boss `block`/`approve` into Grok `deny`/`allow` — because [T-02](../investigations/grok-pretooluse-decision-vocabulary-and-tool-name-map.md) proved that **`{"decision":"block"}` fails open on Grok** while only `deny` and exit-2 block.

**Permission isolation was the second gate, and its answer was scoped `HOME`, not a config key.** `GROK_HOME` alone does not stop Claude-compat permission discovery: under a fresh empty home, `~/.claude/settings.local.json` still loads and is **enforced** under `--always-approve` (mono#2471). There is no `permissions` cell in the compat matrix. The shipped posture scopes the worker process `HOME` to a per-run `process-home/`, keeps OAuth on `GROK_AUTH_PATH` (so credentials do not move with `HOME`), and then **bridges** only the host state `gh` / `ssh` / `git` / `cube` need into that home (mono#2482, mono#2517) — because an empty scoped `HOME` otherwise makes `cube pr create` impossible.

**What actually shipped, against that verdict.**

- The interactive-TUI topology, Boss-owned per-run `$TMPDIR/boss-grok-homes/<run>/` container (`grok-home` + `process-home`), driver-owned hooks, adapter, progress normaliser, PR-URL capture, structured-output file contract, transcript dialect, control verbs, Esc turn-end recovery via `events.jsonl`, and the driver-supplied pane monitor all landed.
- Local macOS sandboxes diverge from the original "use Grok's built-in profiles" sketch: every non-`off` built-in Grok Seatbelt template blocks login-keychain IPC that `gh` needs, so Boss wraps the pane in a Boss-owned `sandbox-exec` profile and runs Grok with `--sandbox off` inside it, still layering `--deny` rules and (on non-macOS / remote) custom `sandbox.toml` profiles (mono#2513).
- Two silent production bugs were found only on the real end-to-end path: the events socket resolved a single `ENGINE_DEFAULT_DRIVER` for every connection (Grok payloads normalised as Claude and dropped — mono#2520), and the hook adapter initially asserted `GROK_AGENT`, which the runner does not inject (every tool call denied — mono#2597). Both are closed.
- Subagents stay disabled (`--no-subagents`), but for a **measured** reason rather than posture alone: a finishing child emits a top-level-shaped `session_end` that can mark the live slot `Terminated` while the parent is still alive (mono#2700).
- Review is **not** Grok-dispatchable today: the review pool hardcodes `REVIEWER_POOL_DRIVER = "claude"`. Conflict-resolution / CI-remediation later gained a `CommandOutcomeObservation` required-strict escalation that Grok does not declare, so those execution kinds refuse Grok at the capability gate even though Phase 3 verified sandbox write/deny properties.

**Two framing corrections from the original design still hold, and both landed:**

1. **The pane monitor is not a liveness signal** — only a pre-hook fallback pill. It is now driver-supplied end-to-end (mono#2483) with Grok's measured markers under `--no-alt-screen` (mono#2470): `Shift+Tab:mode` / `always-approve` / `Grok 4`, busy `Esc:cancel` / `[stop]`, starting `Starting session`, prompt `│ ❯`.
2. **Esc-cancelled turns skip the `Stop` hook.** Recovery tails the constructible `events.jsonl` for `turn_ended outcome=cancelled`, with a bounded synthesis fallback (mono#2525).

**Model menu follows the provider default.** Authenticated `grok models` on 2026-08-18 reports `grok-4.6` as the default and `grok-4.5` as the previous generation; Boss dispatches the default generation for both reasoning modes and its legacy fallback. `grok-build-0.1` is not on the account menu. The live effort ladder is only `low` / `medium` / `high` — the original seven-rung sketch (`xhigh` / `max` included) was wrong for the installed CLI and would fail the turn after spawn.

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

Everything about Grok in the original design pass was either (a) quoted from the gating spike, which ran it, or (b) established by running `grok 0.2.112` on this host on 2026-07-27. Claims of the second kind that came from `--help` or an argument-parse probe rather than a completed agent turn are marked **_(surface probe)_** — they establish that a flag exists and parses, not that it behaves as named. Nothing here is recalled.

Where this doc and the spike disagree, **the spike wins and the disagreement is stated**. There is one such case ([grok-build-0.1](#model-and-effort)) and it is a resolution rather than a conflict. Where this doc adds facts the spike did not record, they are marked as new and attributed to the probe that produced them.

Boss-side claims in the original pass were verified against `6b2a4ee6` by locating symbols, not by trusting line numbers. Treat line numbers in _this_ doc the same way — they drift; symbols do not.

The spike's harness — an AppKit host linked against the same pinned GhosttyKit prebuilt Boss uses (`ghosttykit-5659cef`), driving `ghostty_surface_new` / `ghostty_surface_read_text` / `ghostty_surface_text` / `ghostty_surface_key` — is reproduced in [Appendix A](#appendix-a-reproducing-the-spike). The hard apparatus rule from that spike carries into this doc: **pane verdicts come from GhosttyKit-hosted panes only.**

**The as-built reconciliation (2026-08-10) has a different method, stated so the two are not confused.** Sections marked as landed, corrected, or superseded were written against the merged implementation PRs listed above and the code they left behind — not against a fresh Grok run. Where an as-built claim rests on a live measurement, that measurement was taken by the PR that made it and is cited to its investigation doc under `tools/boss/docs/investigations/grok-*`. Nothing in the original empirical sections was silently rewritten to match the implementation: where the implementation contradicts an earlier plan, both are stated.

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

On 2026-08-18, authenticated `grok models` reported `Default model: grok-4.6` with `grok-4.5` retained as an available prior generation. No new CLI parameters or effort levels were reported.

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

`GROK_HOME` is a complete state root: hooks, sessions, config, trust store, leader socket. As shipped, each run gets a Boss-owned container under `$TMPDIR/boss-grok-homes/<run_id>/` (override via `BOSS_GROK_HOMES_DIR`), holding sibling `grok-home/` and `process-home/` directories — not under the cube workspace, and never the operator's interactive `~/.grok`. Per-run homes past retention are reclaimed by a periodic sweep mirroring Codex (mono#2570; defaults 14 days / 500 MiB, env `BOSS_GROK_HOME_RETENTION_DAYS` / `BOSS_GROK_HOME_MAX_BYTES`, `bossctl grok-homes`).

Layering, in the order Grok resolves it:

- `$GROK_HOME/config.toml` — the user layer Boss owns (full `[compat.claude]` and `[compat.cursor]` disable block).
- `$GROK_HOME/trusted_folders.toml` — folder-trust store, keyed by absolute path with a `decided_at` epoch. **macOS stamps both `/tmp`↔`/private/tmp` and `/var`↔`/private/var` forms** of the workspace path and its canonical root.
- `$GROK_HOME/hooks/*.json` — global hooks, always trusted (Boss writes the adapter + guards + `boss-event` forwarder here).
- `$GROK_HOME/sandbox.toml` — Boss custom profiles (`boss-workspace` / `boss-read-only`) on non-macOS-local paths; local macOS uses external Seatbelt instead (see [G-3](#g-3-permissionpolicy)).
- Process `HOME` → per-run `process-home/` — empty of Claude state; host `gh` / `ssh` / `git` / `cube` state is **bridged in** (symlinks / env redirects), not copied.
- Auth → `GROK_AUTH_PATH` pointing at the host credential (`~/.grok/auth.json` or `BOSS_GROK_AUTH_SOURCE`); not a per-home copy, so concurrent workers share one refresh lock.
- Project `<proj>/.grok/` — hooks, config, sandbox profiles. **Trust-gated**, and attacker-controllable in Boss's threat model. The driver does not depend on it.
- CLI flags — highest precedence.

**Where isolation stops — D-1, resolved by scoped HOME (mono#2471 / mono#2482).** `GROK_HOME` alone does not scope Claude-compat _discovery_. Under a fresh empty home, `grok inspect --json` still resolves `~/.claude/settings.local.json` as a loaded permission source, and those rules are **enforced** under `--always-approve`. There is no `permissions` cell in the compat matrix (an undocumented `permissions = false` is a no-op). Project `<cwd>/.claude/settings.json` still loads when the folder is trusted, independent of `HOME` scoping. Managed settings always probe `/Library/Application Support/ClaudeCode/managed-settings.json`.

**The lever that works is a worker-scoped `HOME`** (empty `~/.claude` under `process-home/`), with auth kept on `GROK_AUTH_PATH` so credentials do not move with `HOME`. The `[compat.claude] rules = false` setting still disables Claude-vendored instruction files. Pre-spawn `grok inspect --json` asserts the resulting posture (compat cells off, hooks inventory present, no host Claude permission source under the scoped home).

### Auth and coexistence

Auth is the host `auth.json` selected through `GROK_AUTH_PATH` (grok.com OAuth on this host). Grok config and session state remain inside the isolated `GROK_HOME`, but the credential path is deliberately shared:

- Every worker uses the same explicit credential path. Grok places `auth.json.lock` beside that path, so concurrent workers share one refresh lock. A per-home symlink is not sufficient: an atomic refresh replaces the symlink with a private file while each home still uses a different lock.
- Boss disables API-key auth and removes an inherited `XAI_API_KEY`; an OAuth refresh failure must not silently switch credential families or endpoints.
- **Scoped process HOME + credential bridge (mono#2517).** The empty `process-home/` that closes D-1 also strips every credential `cube pr create` needs. As shipped, the driver bridges host `gh` config, login-keychain material, `ssh`, `git` user config, and cube data/config into `process-home` (and/or via env redirects such as `GH_CONFIG_DIR`, `CUBE_DATA_DIR`). Local macOS workers additionally run under a Boss-owned Seatbelt policy that preserves keychain IPC — Grok's built-in profiles block that IPC and make `gh` silently fall back to an invalid file credential; a keychain-file symlink alone cannot repair the kernel policy.
- **No collision with `unset ANTHROPIC_API_KEY`** at the shared spawn wrapper — it is inert for Grok. It remains a Claude-ism in driver-generic code and belongs behind the driver.
- `grok models` requires login, which makes it a cheap liveness check on the credential without an inference call.
- **Concurrency is not entitlement-blocked at Boss's target scale**: the spike ran 16 concurrent sessions to completion, 16/16, ~12–16s each. That does not prove unlimited quota; it proves the design's premise is not immediately dead.
- **Leader process — characterised (mono#2537).** Per-run `GROK_HOME` **does** isolate the leader (`$GROK_HOME/leader.sock` / `leader.lock`). Boss never enables `[cli] use_leader = true`, so a Boss-shaped TUI worker does not spawn a leader today. If a leader is started, it reparents to launchd and **escapes pane process-group SIGTERM** (own process group). Latent, not live — both conditions that would make it real are one config change away.

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
- **Boss's guards emit `{"decision": "block"|"approve"}`.** [T-02](../investigations/grok-pretooluse-decision-vocabulary-and-tool-name-map.md) (mono#2469) proved that on Grok PreToolUse **`block` fails open** (hook runs, attack file still created); only `{"decision":"deny"}` and exit-2 block. Unrecognised decision values also fail open. The official bundled hooks guide documents PreToolUse as `allow`/`deny` only — `block` is the Stop-gate vocabulary. The shipped adapter therefore translates Boss `block`→`deny` and `approve`→`allow` (or empty allow).

### Model and effort

```console
$ grok models
You are logged in with grok.com.
Default model: grok-4.6
Available models:
  * grok-4.6 (default)
  - grok-4.5
```

Re-run 2026-08-18. **Current default plus prior generation.** The settled dispatch choice is the provider default, `grok-4.6`; the driver must not select retained `grok-4.5` as a fallback. This continues to resolve the open question about `grok-build-0.1`: **it is not on this account's menu, and the driver must not reference it.** `grok-code-fast-1` is retired (15 May 2026) and silently redirects rather than erroring, which makes it useless as a probe target — do not use it to test model selection.

Effort is the real dial. `--reasoning-effort` (alias `--effort`) was executed at `low` in the spike and recorded as `"reasoning_effort": "low"` in the session `summary.json`.

**Live ladder (grok CLI / `grok-4.6`, re-probed 2026-08-18):** only `low`, `medium`, and `high` are accepted. Passing `xhigh` or `max` is rejected at request time (spawn still succeeds; the pane then shows `--effort/--reasoning-effort: unknown effort level 'xhigh'; use one of: high, medium, low`). Older docs that listed a seven-rung ladder (`none, minimal, low, medium, high, xhigh, max`) were wrong for the installed CLI — do **not** mirror Claude's five-value vocabulary onto Grok.

Per-driver table (deliberate three-into-five collapse; `Medium`/`Large`/`Max` share Grok's ceiling):

| Boss `EffortLevel` | Grok `--reasoning-effort` |
| ------------------ | ------------------------- |
| `Trivial`          | `low`                     |
| `Small`            | `medium`                  |
| `Medium`           | `high`                    |
| `Large`            | `high`                    |
| `Max`              | `high`                    |

This is a capability limit, not a silent demotion of the Boss row: operator-facing `effort_level` stays as classified; only the driver knob is capped. `Medium` already mapped to Grok's valid `high` value in live rows, so it deliberately remains there rather than moving down as in Copilot's different three-rung collapse. Re-probe when the CLI grows more rungs and re-spread the table.

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

| #    | Capability              | What Grok offers natively                            | Class   | As-built verdict                                                               |
| ---- | ----------------------- | ---------------------------------------------------- | ------- | ------------------------------------------------------------------------------ |
| G-1  | `Spawn`                 | interactive TUI + positional prompt + flags          | **(a)** | **Shipped** — `SpawnRequest`/`SpawnPlan` as designed                           |
| G-2  | `WorkspaceProvisioning` | `GROK_HOME`, `trusted_folders.toml`, `--trust`       | **(a)** | **Shipped** — scoped HOME + bridge; pre-trust driver-supplied; retention added |
| G-3  | `PermissionPolicy`      | `--sandbox`, `--allow`/`--deny`, `--permission-mode` | **(a)** | **Shipped** — D-1 closed via scoped HOME; local macOS uses Boss Seatbelt       |
| G-4  | `ModelAndEffortMenu`    | `-m`, `--reasoning-effort`, `grok models`            | **(a)** | **Shipped** — single SKU; effort collapsed to `low`/`medium`/`high`            |
| G-5  | `ProgressObservation`   | hooks under a Boss-owned home                        | **(b)** | **Shipped** — destination named; normaliser live; per-connection socket fix    |
| G-6  | `ToolUseInterception`   | `PreToolUse` deny, fail-open, deny-only              | **(b)** | **Shipped** — adapter translates `block`→`deny` + tool names                   |
| G-7  | `TurnBoundary`          | `Stop` hook with `reason` / `stopHookActive`         | **(c)** | **Shipped** — normal `Stop`; Esc recovery via `events.jsonl`                   |
| G-8  | `StructuredOutput`      | file contract; `--json-schema` unusable for TUI      | **(a)** | **Shipped** — file contract only; empty transcript fallback                    |
| G-9  | `TranscriptAccess`      | `transcriptPath` on every payload                    | **(a)** | **Shipped** — ACP dialect + bossctl path                                       |
| G-10 | `ControlVerbs`          | Esc, typed input, `/quit`, SIGTERM                   | **(b)** | **Shipped** — trait + Grok verbs; `mid_turn` stays `Rejects`                   |
| G-11 | `ToolProvisioning`      | MCP, plugins, skills, subagents                      | **(a)** | Unused; `--no-subagents` load-bearing (session_end attribution)                |
| G-12 | `PromptComposition`     | agent-rules file + preamble                          | **(a)** | **Shipped**                                                                    |
| G-13 | `AwaitingInputSignal`   | `Notification` with `notificationType` / `level`     | **(a)** | **Characterised, undeclared** — blocked population empty under Boss flags      |

### G-1 `Spawn`

Fits cleanly. `SpawnRequest` / `SpawnPlan` already landed (`claude.rs:447-455`), so the driver supplies both its command and its environment directives — which is what Grok needs, because `GROK_HOME` must be _exported_, not passed as a flag.

Two Claude-shaped fields remain in `SpawnRequest` and are inert rather than wrong for Grok: `non_opus_auto_mode` (a Claude model-family concept) and `settings_path` (a single settings _file_, where Grok needs a directory). `permission_mode_override` maps directly, since Grok's `--permission-mode` accepts Claude's exact enum. No signature change is required for Grok; the inertness is noted so a fourth driver does not inherit the confusion silently.

The shared wrapper at the pane spawn site still hardcodes `unset ANTHROPIC_API_KEY`. Inert for Grok, wrong in principle, unchanged from the Codex project's finding.

### G-2 `WorkspaceProvisioning`

**Shipped (mono#2482, mono#2498, mono#2517, mono#2570).** Fits the current trait. The Grok driver writes `.grok/initial-prompt.txt`, its agent-rules file, a per-run container under `$TMPDIR/boss-grok-homes/<run_id>/` (`grok-home` + scoped `process-home`), `trusted_folders.toml` pre-stamped with `/tmp`↔`/private/tmp` and `/var`↔`/private/var` forms, full compat-disable `config.toml`, provisional-then-real global hooks, OAuth via `GROK_AUTH_PATH`, host-tool bridging into `process-home`, and pre-spawn `grok inspect --json` plus live capability preflights.

**Driver-generic Claude pre-trust is closed (mono#2498 / T-25).** `pre_trust_workspace` and `config_dir_gitignore` are now `AgentDriver` methods. Defaults are no-op / catch-all `*\n`; only `ClaudeDriver` overrides. Codex and Grok self-trust inside their own provisioning.

**Home retention shipped after the original design called teardown "noted, not filed" (mono#2570).** Per-run homes do not die with the cube workspace — they live under a temp root — so a Codex-shaped retention sweep was required and landed (`grok-home-retention` crate, engine periodic task, `bossctl grok-homes`).

### G-3 `PermissionPolicy`

**Shipped (mono#2471 characterisation, mono#2513 implementation).** Grok is still the best-equipped of the three drivers on native levers, and the isolation defect (D-1) is closed by scoped `HOME` rather than a config key.

`ClaudeDriver::write_permission_config` remains a **functional no-op** returning `PermissionArtifacts::default()`; the real Claude renderer still sits in `core/src/worker_setup.rs`. **Grok follows Codex**: `write_permission_config` is real. It writes hook wiring into `$GROK_HOME/hooks/`, and on non-macOS-local paths a Boss-owned `$GROK_HOME/sandbox.toml` (`boss-workspace` / `boss-read-only` extending the built-in bases), returning `PermissionArtifacts { config_files, extra_args, env }` with `--sandbox` / structural `--deny` rules in `extra_args`.

**Local macOS diverges from the original "use Grok profiles" sketch — load-bearing.** Every non-`off` built-in Grok Seatbelt template blocks Security.framework mach services `gh` needs for the login keychain (and IOKit power-management Bazel needs at startup). As shipped, local macOS workers run under a **Boss-owned `sandbox-exec` wrapper** with Grok itself at `--sandbox off` inside it. That is not a relaxation of the filesystem posture: the Boss profile reconstructs workspace / read-only write rules, protects the Boss data directory and Grok's global hooks, and retains the CLI `--deny` rules. Remote workers get bare built-in profile names (`workspace` / `read-only`) with no local `sandbox.toml`. Non-macOS local workers use the custom `sandbox.toml` path.

So Grok does **not** need the Claude extraction to land first. It routes around it. The extraction stays deferred ([T-28](#t-28-extract-claudes-permission-rendering-into-the-driver-crate)).

### G-4 `ModelAndEffortMenu`

Fits the `ModelMenu` struct as-is. `menu_for_driver_in` already resolves per slug through `DriverRegistry` and returns `UnknownDriverError` rather than silently falling back to Claude's table (`effort/src/lib.rs`), so registering `"grok"` at `registry.rs:46-47` is the whole integration.

The menu is thin: a current-default model and a three-rung effort mapping (`low`/`medium`/`high`) collapsed from Boss's five levels. `engine_default` is `grok-4.6`; `model_for_reasoning` returns `grok-4.6` for both `Standard` and `Investigation`, so neither tier can silently retain the prior generation. The legacy size fallback resolves to the same current default.

### G-5 `ProgressObservation` — transport solved; destination named and shipped

**The Codex project's top gap does not reproduce.** Grok's ingress is the events socket, via hooks Boss installs in a home Boss owns, which are unconditionally trusted. That is the same production path Claude uses.

**Hook-wiring destination shipped (mono#2472 / T-05).** `ProgressObservationWiring` carries `HookWiringDestination` (`WorkerSettingsFile` | `DriverOwned`). Engine merge of hooks + interception guards into the Claude settings file is conditional on `WorkerSettingsFile`. Grok declares `DriverOwned` and writes `$GROK_HOME/hooks/` itself. Claude keeps `WorkerSettingsFile` with byte-identical settings composition.

**Progress normaliser shipped (mono#2511 / T-10).** `GrokProgressSession` canonicalises camelCase payloads into the snake_case shape `normalize_hook_event` already expects, then delegates. `source: "new"` reaches `SessionStartSource::Other` with no protocol widening. Unknown event names are ignored-with-logging.

**Integration bug found only on the real path (mono#2520).** The events-socket accept loop originally resolved a single `ENGINE_DEFAULT_DRIVER`-slug driver for every connection. Grok forwarders fired, but every payload was normalised as Claude and dropped on `MissingField`. Fix: per-connection resolution from the payload's `_boss_run_id` via `WorkDb::get_execution_driver_slug` (`events_socket.rs`'s `resolve_connection_driver`), mirroring the stdout/JSONL ingress precedent.

`progress_fidelity()` is `Rich` for Grok — per-tool `PreToolUse`/`PostToolUse` events, same tier as Claude. Grok does **not** declare `CommandOutcomeObservation` (added later by the Codex project to split outcome fidelity out of the cadence tier), even though Bash `toolResult` carries `exit_code` on the wire — see [Phase eligibility](#which-work-item-kinds-are-grok-eligible).

### G-6 `ToolUseInterception`

**Shipped, deny-only (mono#2469 / mono#2490 / mono#2597).** Declared for real once the adapter and its negative tests landed.

The mechanism works: `PreToolUse` fires, `{"decision":"deny"}` blocks, exit-2 blocks, and the global-hook location means it is always armed. Limits:

- **Deny-only.** `updatedInput` did not rewrite in either the native or the Claude-shaped form. Editorial inline-`--body` is denied-with-reason rather than rewritten — same call as Codex.
- **Fail-open on crash / malformed output / timeout.** Identical to Claude's production posture; not a regression.
- **`block` fails open; adapter translates.** Boss guards emit `block`/`approve`; Grok honours `deny`/`allow` only. The adapter (`$GROK_HOME/hooks/boss-grok-hook-adapter.py`) rewrites the field/tool-name map (`run_terminal_command`→`Bash`, `write`→`Write`, `search_replace`→`Edit`, …), execs the unchanged guard via `sh -c`, and translates decisions.
- **Fail-closed identity is runner-injected keys, not `GROK_AGENT` (mono#2597).** An early adapter revision asserted `GROK_AGENT`, which the runner does not inject (it appeared in spike env excerpts only as harness inheritance). The shipped identity check requires `GROK_HOOK_EVENT` / `GROK_HOOK_NAME` / `GROK_SESSION_ID` / `GROK_WORKSPACE_ROOT`.

Acceptance criterion remains a **negative** test: a fixture that attempts each blocked command and is demonstrably refused.

### G-7 `TurnBoundary`

`Stop` maps directly onto `WorkerEvent::Stop`. The payload carries `reason` (`"end_turn"` observed), `stopHookActive` → `TurnEnd::continuation`, and `lastAssistantMessage`. Structurally identical to Claude's; no synthesizer needed for a normal turn.

**Except after an interrupt — the sharpest lifecycle hazard, now closed.** Esc-cancelled turns **skip `Stop` hooks** entirely; the cancellation appears only in the session files:

```json
{
  "type": "turn_ended",
  "outcome": "cancelled",
  "cancellation_category": "mid_turn_abort",
  "cancellation_context": { "trigger": "esc" }
}
```

Boss uses interrupt. `bossctl` sends one; transient recovery sends one; a human sends one. Under this design an interrupted Grok worker would emit nothing to the engine after its last `PostToolUse`, and its slot would sit at `Working` until the stale-activity sweep eventually intervened — with the worker actually idle at its prompt, ready for input, the whole time.

**Shipped as (2) with (1) as bounded fallback (mono#2525 / T-12).** `grok/turn_end_recovery.rs` constructs the run's `events.jsonl` path from `GROK_HOME` + workspace cwd + stamped session UUID (percent-encoding confirmed against real session trees). Engine-side `interrupt_recovery.rs` owns the bounded tail-with-fallback executor. The three candidates evaluated were:

1. **Engine-side synthesis on interrupt** — cheapest, asserts a state it did not observe; kept only as settle-window fallback.
2. **Tail `events.jsonl` for `turn_ended`** — observes the real thing; path fully constructible — **chosen primary**.
3. **`StopFailure`** — never observed as the interrupt path; not relied on.

### G-8 `StructuredOutput`

**Shipped on the file contract; `--json-schema` evaluated and rejected for the TUI (mono#2522 / T-18).** The shared `BOSS_STRUCTURED_OUTPUT` / `BOSS_PR_URL_OUTPUT` env-file contract is the mechanism. A live pty probe of `--json-schema` under the interactive flags confirmed it is a headless notion (implies `--output-format json`); it is not wired. `structured_output_fallback` returns an empty `Vec` for every kind — a failed artifact write has no transcript recovery for Grok (relevant for review if the pool pin ever lifts).

**PR-URL capture shipped (mono#2522 / T-19).** Grok's Bash `toolResult` is `{type: "Bash", output: [<bytes>], output_for_prompt: "…", exit_code: …}` — not Claude's `stdout`/`stderr` keys. `pr_url_capture_feed` reads `output_for_prompt` (with a lossy `output` byte-array fallback). The shared URL matcher and command gates are unchanged.

### G-9 `TranscriptAccess`

**Shipped (mono#2516 / T-11; bossctl rendering mono#2584).** `transcript_path_for_session` reads `transcriptPath` from the raw hook payload — a key rename, no glob. `grok/transcript.rs` owns the ACP `sessionUpdate` dialect; `GrokTranscriptSession` correlates `tool_call` / `tool_call_update` by `toolCallId`. Container-level reuse of `engine/transcript-tail` only. `bossctl agents transcript` falls through the run's resolved driver normaliser for non-Claude/Codex dialects rather than rendering empty.

### G-10 `ControlVerbs` — the absorbed row, shipped

**Trait surface + call-site routing shipped (mono#2472 / T-04); Grok verbs + classify shipped (mono#2525 / T-13).** `AgentDriver` now carries `probe` / `interrupt` / `stop` / `reap` returning driver-neutral delivery plans. Claude and Codex implementations are the existing behaviour moved behind the seam. `transient_recovery` routes through the resolved driver's `classify_error` (unresolvable drivers fail closed to `Indeterminate`).

| Verb               | Grok mechanism                             | As-built                                          | Qualification                                                |
| ------------------ | ------------------------------------------ | ------------------------------------------------- | ------------------------------------------------------------ |
| **probe**          | pane text + Return                         | pane-delivered                                    | Post-turn and post-Esc proven; mid-turn still unproven       |
| **interrupt**      | Esc (`0x35`)                               | pane-delivered; pairs with T-12 recovery          | Skips the `Stop` hook; no-op in fullscreen vim mode          |
| **stop**           | `/quit` via SendToPane                     | pane-delivered then process release               | Graceful path only                                           |
| **reap**           | SIGTERM → SIGKILL on the process group     | process-group reap                                | Tool child shells need group reap; leader escapes if enabled |
| **classify_error** | Grok/xAI shapes (`grok/classify_error.rs`) | grounded in Grok's bundled `StopFailure` taxonomy | Not routed through `classify_claude_error`                   |

`mid_turn_pane_input` stays at the safe default **`Rejects`** — no empirical mid-turn stdin-consumption evidence was gathered, so the driver deliberately does not claim `Buffers`.

### G-11 `ToolProvisioning`

Grok has the richest surface of the three — MCP servers, plugins with a marketplace, skills, subagents, cross-session memory. **Boss injects none of it**, as the abstraction intended for v1 across every driver. **No gap.**

The v1 posture is a _decision_, though, not an absence: the driver should explicitly disable what it does not use (`--no-subagents`, `--no-memory`) rather than inheriting defaults, because a subagent or a memory carried across sessions is state Boss does not model and cannot reason about. Noted in [T-07](#t-07-grok-config-isolation-and-workspace-provisioning).

**Amendment 2026-08-09 — `--no-subagents` is now a measured requirement, not just posture.** Probed against `grok 1.0.0` in this project's own pane shape: subagents DO inherit the global `$GROK_HOME/hooks/` set and their tool calls ARE intercepted by the `PreToolUse` guards (no safety gap), but a finishing subagent emits a `session_end` whose payload is shape-identical to the top-level session's — same keys, same `reason: "shutdown"` — which Boss applies by slot as `WorkerActivity::Terminated` for a live worker. A Grok subagent is also **in-process**, so `background_children.rs`'s descendant walk cannot compensate, and `Stop.backgroundTasks` is empirically empty when a background subagent is in flight. Full evidence, the failure timeline, and what would have to change first: [`../investigations/grok-subagent-hook-attribution-2026-08-09.md`](../investigations/grok-subagent-hook-attribution-2026-08-09.md) (mono#2700). This turns a **(a) unused-by-design** cell into a real, if narrow, `ProgressObservation` gap — recorded against [G-5](#g-5-progressobservation--transport-solved-destination-named-and-shipped), where session identity belongs, rather than reopening G-11.

### G-12 `PromptComposition`

Fits. `render_claude_md` already takes `preamble` and `config_dir` from the descriptor (`worker_setup.rs:240`, `:1686-1716`), so the per-session agent-rules file is driver-routed already. Grok's descriptor supplies `config_dir = ".grok"`, its own `agent_rules_filename`, and a Grok-specific preamble.

The shared prompt body still hardcodes _"A PreToolUse hook blocks these"_. Under this design that sentence is **true for a Grok worker** — the mechanism really is a `PreToolUse` hook — so it is hygiene, not a correctness defect, exactly as the Codex project concluded. It stays deferred.

One Grok-specific wrinkle: because Grok resolves the workspace's `.claude/CLAUDE.md` as a project instruction under compat, a Grok worker would otherwise read Claude's worker-rules file _in addition to_ its own. `[compat.claude] rules = false` closes this, and the closure is verifiable with `grok inspect --json` rather than assumed.

### G-13 `AwaitingInputSignal`

**Characterised; stays undeclared — negative result, and the correct one (mono#2537 / T-24).**

The lifecycle `Notification` hook is a **separate channel** from `[ui.notifications]` terminal notifications (`$GROK_EVENT` / `$GROK_MESSAGE` were null on every hook invocation). Measured vocabulary under live probes:

| `notificationType`  | Means "blocked on a human"? | Reachable under Boss flags?                                      |
| ------------------- | --------------------------- | ---------------------------------------------------------------- |
| `permission_prompt` | **Yes**                     | **No** — `--always-approve` suppresses the prompt that raises it |
| `task_complete`     | No — informational          | **Yes**                                                          |

Under Boss's spawn flags the only observed `Notification` is `task_complete`. Mapping the capability onto that would fabricate `WaitingForInput`. The declaration becomes earnable only if Boss ever spawns Grok without `--always-approve`, at which point `permission_prompt` is the already-measured mapping. A Grok worker shows `Working` / `Idle` and never a fabricated `WaitingForInput`.

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

Drive **`grok` as a full interactive TUI in a GhosttyKit pane**, seeded by a positional prompt from a file on disk, with a Boss-owned per-run home container for isolation, **Claude-shaped hooks under that home as the progress transport** (the same events-socket path Claude uses in production), a **driver-owned canonicalisation adapter** in front of Boss's five unchanged guard scripts, platform sandbox + `--deny` as defence in depth, and a **narrow `events.jsonl` tail** solely to observe Esc-cancelled turns. **This is what shipped.**

### Execution shape

```sh
export GROK_HOME=$TMPDIR/boss-grok-homes/<run_id>/grok-home
export HOME=$TMPDIR/boss-grok-homes/<run_id>/process-home   # scoped; host tools bridged in
export GROK_AUTH_PATH=<host>/.grok/auth.json                # shared credential + refresh lock
# local macOS: the shell command is wrapped in Boss sandbox-exec; Grok sees --sandbox off
grok \
  --model grok-4.6 \
  --reasoning-effort <low|medium|high> \
  --no-alt-screen \
  --always-approve \
  --trust \
  --session-id <boss-assigned-uuid> \
  --cwd <workspace> \
  --no-subagents \
  --no-memory \
  --sandbox <off|boss-workspace|boss-read-only|workspace|read-only> \
  --deny <structural-rules…> \
  "$(cat <workspace>/.grok/initial-prompt.txt)"
```

Notes on each non-obvious element:

- **Prompt from a file, not inline.** The spike verified brief-sized prompts through `--prompt-file` headless, and explicitly cautions against pasting tens of KB through `initial_input`. `$(cat …)` is the Claude pattern and it transfers.
- **`--trust` _and_ a pre-seeded `trusted_folders.toml`.** Redundant on purpose: `--trust` is hidden from `--help` (D-3), so the file is the belt that survives its removal. Conformance still fail-closes if the flag stops parsing. `GROK_FOLDER_TRUST=0` is rejected — it also ungates project hooks and MCP.
- **`--no-alt-screen` settled (mono#2470).** `--minimal` has **no** busy `Esc:cancel` chrome (0 hits) — rejected for v1 monitor. Default fullscreen shares live markers but alt-screen teardown drops viewport chrome on exit. Inline is the recommended pane mode.
- **`--always-approve`** rather than `--permission-mode bypassPermissions`: observed payloads already report `permissionMode: "bypassPermissions"` under it. Answer-agent would additionally force `--permission-mode dontAsk` if ever dispatched.
- **Sandbox is platform-split.** Local macOS → Boss `sandbox-exec` + Grok `--sandbox off`. Non-macOS local → `--sandbox boss-workspace` / `boss-read-only` via `$GROK_HOME/sandbox.toml`. Remote → bare built-in `workspace` / `read-only`.
- **No `-w` / `--worktree`, ever.** Cube owns workspace provisioning.
- **`--no-subagents` / `--no-memory`** are explicit; `--no-subagents` is load-bearing for progress attribution (see [G-11](#g-11-toolprovisioning)).
- **Vim mode must never be enabled** — Esc does not cancel in fullscreen vim mode, which would silently break interrupt.

The pane launch itself is unchanged from Claude's: the engine composes this as a shell command, the app hosts it via `SpawnWorkerPane` with `initial_input` and optional `pane_monitor`, and the engine holds only `shell_pid`.

### The engine seams this needed — all landed

1. **Hook-wiring destination** ([G-5](#g-5-progressobservation--transport-solved-destination-named-and-shipped)) — `HookWiringDestination::DriverOwned` (mono#2472).
2. **Driver-owned payload canonicalisation** ([G-6](#g-6-tooluseinterception)) — adapter + negative tests (mono#2490, mono#2597).
3. **`ControlVerbs` on the trait, and actually called** ([G-10](#g-10-controlverbs--the-absorbed-row-shipped)) — mono#2472 + mono#2525.
4. **Narrow interrupt observer** ([G-7](#g-7-turnboundary)) — `events.jsonl` tail + synthesis fallback (mono#2525).
5. **Per-connection events-socket driver resolution** — not in the original seam list; found only end-to-end (mono#2520).

Everything else — the `boss-event` shim, the ordered fan-out, `LiveWorkerState`, the stale-worker sweep, the dispatch gate, the registry, the effort resolution — is reused unmodified.

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

**Grok's markers are measured and shipped (mono#2470 / mono#2483).** Under `--no-alt-screen`:

| Field                 | Value                                         |
| --------------------- | --------------------------------------------- |
| `agent_markers`       | `Shift+Tab:mode`, `always-approve`, `Grok 4`  |
| `busy_markers`        | `Esc:cancel`, `[stop]`                        |
| `starting_markers`    | `Starting session`                            |
| `prompt_prefixes`     | `│ ❯` (boxed; bare `❯` collides with history) |
| `idle_debounce_polls` | `2`                                           |

T-14 (protocol/engine) and T-15 (Swift) landed as **one PR** (mono#2483) rather than the planned split — the file-overlap serialisation was cheaper than two review cycles. Claude's spec reproduces the historical literals exactly; `None` on the wire keeps Claude defaults.

### Capability declaration for `GrokDriver` (as shipped)

**Provided (11):** `Spawn`, `WorkspaceProvisioning`, `PermissionPolicy`, `ModelAndEffortMenu`, `ProgressObservation`, `ToolUseInterception` (deny-only), `TurnBoundary`, `StructuredOutput`, `TranscriptAccess`, `ControlVerbs`, `PromptComposition`.

**Not provided (3):**

- **`ToolProvisioning`** → default `Degrade`. Unused in v1 for every driver. Grok has MCP / plugins / skills / subagents; Boss injects none and explicitly disables subagents and memory.
- **`AwaitingInputSignal`** → default `Degrade`, **never** `Synthesize`. Characterised (mono#2537); blocked population empty under Boss flags.
- **`CommandOutcomeObservation`** → default `Degrade`, **never** `Synthesize`. Added by the Codex project after this design's original capability table; Claude declares it, Grok/Codex do not. Grok's Bash `toolResult` does carry `exit_code` on the wire — the omission is a declaration gap relative to what the dialect exposes, and it now interacts with Phase 3 (below).

The original gates on `ToolUseInterception` (adapter) and `PermissionPolicy` (compat leak) are **closed**. `mid_turn_pane_input` remains `Rejects`.

### Which work-item kinds are Grok-eligible

Phased as planned; as-built status per phase:

**Phase 1 — chores and project tasks. Loop works; formal 10-chore acceptance sweep (T-21) was never executed as such.** Individual Grok dispatches reach open PRs with primary-path PR-URL capture (the integration bugs that would have made the sweep fail — events-socket driver, scoped-HOME credential strip, adapter identity — were each found and fixed by other means). Phases 2 and 3 were enabled without a recorded ten-consecutive-chores criterion. Same pattern the Codex postmortem recorded: a gate placed at the end of Phase 1 can only hold back Phase 2, and "Phase 1 is not yet accepted" was never a state that stopped work.

**Phase 2 — design, investigation, postmortem. Enabled (mono#2551 / T-22).** `KindRequirements` escalates `StructuredOutput` + `ToolUseInterception` to required-strict for all three document-producing kinds. Grok declares both. Downstream design/investigation/postmortem parsing is driver-agnostic.

**Phase 3 — review and conflict resolution. Sandbox properties verified; dispatch still blocked on two independent pins (mono#2624 / T-23).**

- Reviewer `boss-read-only` / Boss Seatbelt genuinely denies workspace writes (live denial, not model politeness). `ReviewResult` round-trips under that sandbox (artifact path is under the system temp tree, always writable). Standard-kind sandbox permits workspace writes.
- **`PrReview` cannot dispatch on Grok:** every such execution routes to the review pool, whose `REVIEWER_POOL_DRIVER = "claude"` overrides the row's driver. Changing that is a deliberate product decision (configurable reviewer model / load balancing), not a capability gap in this driver.
- **`ConflictResolution` / `CiRemediation` refuse Grok at the capability gate** once `KindRequirements` marks `CommandOutcomeObservation` required-strict for those execution kinds. That escalation landed with the Codex capability split and post-dates the Phase 3 sandbox verification, which had concluded that the `TaskKind` gate already cleared. Main-pool conflict resolution for Grok is therefore not dispatchable today without either declaring the capability (if exit-code observation is considered honest for Grok) or revisiting the required-strict set.

**Phase 4 — triage and the answer agent. Still deferred (T-31).** Prose-scrape consumers now resolve the driver (mono#2529 / T-26), so that particular block is cleared. Remaining work is product enablement, not a missing Grok mechanism — Grok has `UserPromptSubmit` and is a live interactive session, so `pane_delivery`'s confirmation path works structurally (a genuine Grok-vs-Codex difference).

### Load-balancing seams

Design _for_, do not design _now_. Four attachment points, three shared with the Codex analysis and one new:

1. **Per-driver capacity accounting.** Slots are one global pool. The seam is the dispatch gate, which already resolves `(kind, driver)` and is the natural place for an in-flight count keyed by driver slug. Requirement on this project: **do not add a second, driver-blind admission path** — nothing in the hook wiring or the interrupt observer may spawn or admit work outside that gate.
2. **Per-provider rate-limit state.** Grok's hook payloads carry **no token usage** — a real asymmetry with Codex, whose `turn.completed` hands it over for free. Grok's session `summary.json` and `events.jsonl` may carry it; the progress reader is the place to record it if so. A balancer must not assume symmetry across the three drivers here: Claude has no in-band usage signal either.
3. **Capability-aware routing.** `CapabilityResolver::check_dispatch` already computes the predicate a balancer needs. Requirement on this project: keep it a **pure, side-effect-free query**, so a balancer can call it speculatively across candidate drivers.
4. **New — concurrency ceiling is per-provider and unmeasured.** The spike established 16 concurrent Grok sessions succeed on this account. That is a floor, not a ceiling, and it says nothing about the _combined_ load of Claude + Codex + Grok workers, which is what a balancer actually schedules. The seam is per-driver capacity (1); the missing input is a per-provider ceiling that nobody has measured for any of the three.

### Migration and coexistence

- **Auth.** One host `auth.json` selected through `GROK_AUTH_PATH`, shared across per-run homes so refreshes use one adjacent lock. No env-var collision with Claude; `unset ANTHROPIC_API_KEY` in the shared wrapper is inert.
- **Config collisions.** Solved by per-run `GROK_HOME` — except for Claude-compat permission discovery, which it does not solve (D-1).
- **Workspace layout.** Grok uses `.grok/`; Claude uses `.claude/`. They do not collide by name, but Grok **actively reads** `.claude/` under compat, so a workspace that has run both drivers needs the compat block off, not merely different directories. Both config dirs must be engine-gitignored.
- **Shared cube workspaces.** Workspaces are reused across runs and drivers. A workspace that ran a Claude worker yesterday still contains `.claude/CLAUDE.md` and a `.claude/settings.json` today. This is a normal, expected state, and it is precisely the state in which the compat hazard bites. Provisioning must be idempotent and compat-suppressing on every run, not only on a fresh workspace.
- **Cube.** No changes required. `cube pr create` is agent-neutral and remains the enforcement point the guards lean on. Grok's `-w` / `grok worktree` overlaps cube's job and stays unused.

---

## Risks / open questions

Original OQs with as-built outcomes. Residual open items are called out explicitly.

<a id="oq-1-claude-permission-settings-leak"></a>
**OQ-1 — Can Claude permission-settings discovery be scoped out of an isolated `GROK_HOME`?** **Resolved (mono#2471 / mono#2482).** Rules are **enforced** under `--always-approve` (not merely listed). No compat key stops them. Scoped process `HOME` is the lever that works; auth stays on `GROK_AUTH_PATH`. Host-tool bridging into `process-home` (mono#2517) is the necessary follow-on so `cube pr create` still authenticates.

<a id="oq-2-decision-vocabulary"></a>
**OQ-2 — Does Grok honour `{"decision":"block"}`, and what is the canonical tool-name map?** **Resolved (mono#2469 / mono#2490).** `block` fails open; only `deny` and exit-2 block. Tool map: `run_terminal_command`→`Bash`, `write`→`Write`, `search_replace`→`Edit`. Adapter translates decisions and names.

<a id="oq-3-interrupt-without-stop"></a>
**OQ-3 — What ends a turn that was cancelled with Esc?** **Resolved (mono#2525).** Observe `events.jsonl` for `turn_ended outcome=cancelled`; bounded synthesis fallback if no record appears in the settle window.

<a id="oq-4-the-leader-process"></a>
**OQ-4 — The leader process.** **Resolved as latent (mono#2537).** Per-run home isolates the leader; Boss does not enable leader mode today; a leader that is started escapes pane process-group reap. No live fix warranted while `use_leader` stays off.

**OQ-5 — Is `--minimal` a better pane mode than `--no-alt-screen`?** **Resolved: no for v1 (mono#2470).** `--minimal` has no stable busy interrupt chrome. `--no-alt-screen` is the recommended pane mode.

**OQ-6 — Does `--json-schema` do anything useful in an interactive TUI session?** **Resolved: no (mono#2522).** File contract only; native flag not wired.

**Residual — review-pool driver pin.** `PrReview` (and automation-pool conflict resolution) force `driver = "claude"`. Not a Grok-driver defect; a product policy that blocks Phase 3 review enablement regardless of capability declarations.

**Residual — `CommandOutcomeObservation` vs conflict-resolution.** Required-strict for `ConflictResolution` / `CiRemediation`; Grok does not declare it. Blocks main-pool Grok conflict resolution at the capability gate. Grok's Bash `toolResult` carries `exit_code` — whether that is enough to declare the capability honestly is an open product/engineering call, not a measurement gap.

**Risk — the guards fail open in a way that looks healthy.** Mitigated by the shipped adapter + negative tests + pre-spawn `grok inspect` hook inventory, but the residual fail-open on crash / malformed / timeout is inherited from Claude and unchanged. The `PATH`-shim follow-on project remains the structural answer for all three drivers and is still unstarted.

**Risk — one version, one day of evidence → materialised, wrong mitigation.** Grok auto-updated on its own; the hard version pin turned drift into a fail-closed provisioning outage. Pin removed; `grokVersion` is observed/logged (`LAST_CHARACTERISED_GROK_VERSION`) and recorded on the execution. Hidden-`--trust` and `grok models` menu assertions remain fail-closed.

**Risk — Swift monitor work dropped.** Did not materialise; mono#2483 landed the full driver-supplied path (Rust + Swift) before Phase 1 acceptance.

---

## Abstraction amendments (as shipped)

| #    | Name                                                                     | Status                         | PR / note                                                                                            |
| ---- | ------------------------------------------------------------------------ | ------------------------------ | ---------------------------------------------------------------------------------------------------- |
| A-1  | `ProgressIngress`: name the hook-wiring destination                      | **Shipped**                    | mono#2472 — `HookWiringDestination`                                                                  |
| A-2  | Driver-supplied hook-payload canonicalisation                            | **Shipped**                    | mono#2490 (+ identity fix #2597)                                                                     |
| A-3  | `ControlVerbs` + call `classify_error`                                   | **Shipped**                    | mono#2472 (trait) + mono#2525 (Grok verbs)                                                           |
| A-4  | Turn boundary must survive interrupt with no `Stop`                      | **Shipped**                    | mono#2525 — `events.jsonl` tail + fallback                                                           |
| A-5  | Driver-supplied pane-monitor spec on engine↔app spawn RPC                | **Shipped**                    | mono#2483 (Rust + Swift together)                                                                    |
| A-6  | `PermissionPolicy`: complete the Claude extraction                       | **Still deferred**             | Grok/Codex route around Claude's no-op                                                               |
| A-7  | Driver-generic workspace provisioning must stop writing Claude artifacts | **Shipped**                    | mono#2498                                                                                            |
| A-8  | Prose-scrape fallback consumers must resolve the driver                  | **Shipped**                    | mono#2529 (pool-aware)                                                                               |
| A-9  | Remote/SSH dispatch must resolve the driver                              | **Still deferred**             | Local dispatch is the shipped target                                                                 |
| A-10 | Conformance harness: third driver + live-CLI pin                         | **Shipped, then pin softened** | mono#2543; version gate removed after auto-update outages; `--trust` + models menu still fail-closed |
| A-11 | `ModelMenu` documented refresh path                                      | **Documented**                 | module docs + conformance menu check; still a static table, not a live parse                         |
| A-12 | `mid_turn_pane_input` provable fixture                                   | **Still deferred**             | Grok remains at `Rejects`                                                                            |

**Additional seam found only end-to-end (not in the original amendment table):** events-socket per-connection driver resolution (mono#2520). The accept loop resolved a single `ENGINE_DEFAULT_DRIVER` for every connection, so Grok payloads were normalised as Claude and dropped. Fix resolves the driver from `_boss_run_id` via `WorkDb::get_execution_driver_slug`.

**Verdict on inherited residual coupling (as-built):** Claude's `write_permission_config` no-op is still routed around (A-6 deferred). Pre-trust / config-dir gitignore are fixed (A-7). Prose-scrape constructions are fixed and pool-aware (A-8). Remote/SSH remains out of v1 scope (A-9).

---

## Appendix A: reproducing the spike

The gating spike's full harness, evidence tree and re-run instructions live in [ghostty-grok-pane-viability.md](../investigations/ghostty-grok-pane-viability.md) and its committed artifacts. The short form:

```sh
# 1. Isolated home. Never point at the user's ~/.grok.
export GROK_HOME=/tmp/grok-spike/home
mkdir -p "$GROK_HOME"
export GROK_AUTH_PATH="$HOME/.grok/auth.json"       # shared credential and refresh lock
unset XAI_API_KEY                                   # OAuth only; no API-key fallback

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

## Implementation task breakdown (as shipped)

Original dependency order preserved for history. Each in-scope entry carries a **Shipped** line recording the PR that closed it and any divergence from the entry as written. Scope that was **added during implementation** (not in the original ladder) is listed at the end.

### T-01 Investigation: Claude permission-settings leakage into an isolated `GROK_HOME`

Establish whether `~/.claude/settings.local.json` is actually _in force_ for a Grok run under an isolated home, and how to scope it out.

- **Effort:** `small`
- **Depends on:** none
- **Scope: in-scope**
- **Shipped:** mono#2471 (findings + harness). Rules enforced under `--always-approve`; scoped `HOME` is the working lever; no compat `permissions` cell.

### T-02 Investigation: Grok `PreToolUse` decision vocabulary and tool-name map

Does Grok honour Boss `block`/`approve`? What are the on-the-wire tool names?

- **Effort:** `small`
- **Depends on:** none
- **Scope: in-scope**
- **Shipped:** mono#2469. `block` fails open; map `run_terminal_command`→`Bash`, `write`→`Write`, `search_replace`→`Edit`.

### T-03 Investigation: Grok TUI liveness markers under GhosttyKit

Capture stable `PaneMonitorSpec` field values under GhosttyKit; settle pane mode.

- **Effort:** `small`
- **Depends on:** none
- **Scope: in-scope**
- **Shipped:** mono#2470. `--no-alt-screen` recommended; `--minimal` rejected (no busy chrome). Marker set in [pane section](#pane-and-embedder-role--the-driver-supplied-monitor).

### T-04 `ControlVerbs` trait surface, plus route error classification through it

probe / interrupt / stop / reap on the trait; route `transient_recovery` through `classify_error`.

- **Effort:** `medium`
- **Depends on:** none
- **Scope: in-scope**
- **Shipped:** mono#2472 (with T-05 in the same PR — file-overlap serialisation inverted: both landed together).

### T-05 `ProgressIngress`: name the hook-wiring destination

`HookWiringDestination` on `ProgressObservationWiring`; engine merge conditional on `WorkerSettingsFile`.

- **Effort:** `small`
- **Depends on:** T-04 (file overlap)
- **Scope: in-scope**
- **Shipped:** mono#2472.

### T-06 `GrokDriver` skeleton: descriptor, capability set, model menu, registry entry

Slug `grok`, config dir `.grok`, `grok-4.6` menu, capability set, registry.

- **Effort:** `medium`
- **Depends on:** none
- **Scope: in-scope**
- **Shipped:** mono#2468. Effort table originally claimed a longer ladder including `xhigh`/`max`; live CLI only accepts `low`/`medium`/`high` and the table was corrected (see [Model and effort](#model-and-effort)). Submodule directory layout reserved as planned.

### T-07 Grok config isolation and workspace provisioning

Per-run home, compat block, trust stamp, inspect assertion, preflights.

- **Effort:** `medium`
- **Depends on:** T-01, T-06
- **Scope: in-scope**
- **Shipped:** mono#2482 (with T-08). Homes under `$TMPDIR/boss-grok-homes/<run>/` (not the cube runtime dir); scoped `process-home` from day one.

### T-08 `GrokDriver::spawn_invocation` and pane launch

Spawn plan for the interactive TUI shape; never emit worktree flags.

- **Effort:** `medium`
- **Depends on:** T-07
- **Scope: in-scope**
- **Shipped:** mono#2482.

### T-09 Grok hook wiring: progress forwarder plus guard-script canonicalisation

Adapter + five unchanged guards + `boss-event` forwarder under `$GROK_HOME/hooks/`. Negative-test acceptance.

- **Effort:** `large`
- **Depends on:** T-02, T-05, T-08
- **Scope: in-scope**
- **Shipped:** mono#2490. Identity fix mono#2597 (runner-injected keys, not `GROK_AGENT`).

### T-10 `GrokDriver` progress normaliser

CamelCase → `WorkerEvent` via shared `normalize_hook_event` after key rewrite.

- **Effort:** `medium`
- **Depends on:** T-09
- **Scope: in-scope**
- **Shipped:** mono#2511.

### T-11 `TranscriptAccess` for Grok

`transcriptPath` key rename + ACP dialect normaliser with tool-call correlation.

- **Effort:** `medium`
- **Depends on:** T-10
- **Scope: in-scope**
- **Shipped:** mono#2516. Operator-facing rendering follow-on mono#2584.

### T-12 Turn-end recovery for Esc-cancelled Grok turns

Bounded `events.jsonl` tail for cancelled turns + synthesis fallback.

- **Effort:** `medium`
- **Depends on:** T-04, T-10
- **Scope: in-scope**
- **Shipped:** mono#2525 (with T-13).

### T-13 Grok control-verb implementation

probe / interrupt / stop / reap + `classify_error`; mid-turn left at `Rejects` if unproven.

- **Effort:** `medium`
- **Depends on:** T-12
- **Scope: in-scope**
- **Shipped:** mono#2525. `mid_turn_pane_input` remains `Rejects`.

### T-14 / T-15 Driver-supplied pane monitor (protocol + Swift)

`PaneMonitorSpec` on spawn RPC; Swift renames and spec lookups; Grok markers from T-03.

- **Effort:** `medium` each (planned as two PRs)
- **Depends on:** T-06; T-15 also T-03 + T-14
- **Scope: in-scope**
- **Shipped:** mono#2483 as a **single** Rust+Swift PR (planned split collapsed).

### T-16 Investigation: Grok sandbox profiles and allow/deny rule grammar

Built-in profiles, custom `sandbox.toml`, rule grammar, fail-closed behaviour.

- **Effort:** `small`
- **Depends on:** none
- **Scope: in-scope**
- **Shipped:** mono#2471 (combined with T-01 findings doc).

### T-17 `GrokDriver::write_permission_config`

Platform sandbox + structural `--deny` rules as real `PermissionArtifacts`.

- **Effort:** `medium`
- **Depends on:** T-07, T-16
- **Scope: in-scope**
- **Shipped:** mono#2513. Local macOS uses Boss Seatbelt + Grok `--sandbox off` (divergence from "Grok profiles only").

### T-18 Structured output for Grok

File contract + evaluate `--json-schema` for TUI.

- **Effort:** `medium`
- **Depends on:** T-10
- **Scope: in-scope**
- **Shipped:** mono#2522 (with T-19). `--json-schema` negative; file contract only; empty transcript fallback.

### T-19 PR-URL capture for the Grok dialect

Grok Bash `toolResult` feed into the shared matcher.

- **Effort:** `small`
- **Depends on:** T-10
- **Scope: in-scope**
- **Shipped:** mono#2522. Reads `output_for_prompt` (not `stdout`).

### T-20 Conformance: Grok goldens and version pin

Third-driver goldens, ingress/boundary equivalence, live-CLI pin.

- **Effort:** `medium`
- **Depends on:** T-11, T-13, T-17, T-18, T-19
- **Scope: in-scope**
- **Shipped:** mono#2543. **Version pin later removed** (auto-update outages); observe/log only. Hidden `--trust` and `grok models` menu assertions still fail closed.

### T-21 Phase-1 acceptance sweep: 10 Grok chores to green PRs

Ten consecutive chores, primary-path PR-URL, no engine intervention.

- **Effort:** `medium`
- **Depends on:** T-15, T-20
- **Scope: in-scope**
- **Status: not executed as a formal sweep.** Individual Grok chores reach PRs; the integration failures that would have failed the sweep were fixed opportunistically (mono#2517, #2520, #2597). Phases 2–3 enabled without this gate firing. Same structural observation as the Codex postmortem: a gate at the end of Phase 1 only blocks what comes after, and nothing treated "Phase 1 unaccepted" as a stop state.

### T-22 Grok eligibility for design / investigation / postmortem kinds

Enable document-producing kinds via `KindRequirements`.

- **Effort:** `medium`
- **Depends on:** T-21, T-26 (planned); landed without waiting on T-21
- **Scope: in-scope**
- **Shipped:** mono#2551.

### T-23 Grok eligibility for review and conflict-resolution kinds

Verify reviewer sandbox + `ReviewResult`; conflict-resolution write + telemetry.

- **Effort:** `medium`
- **Depends on:** T-22
- **Scope: in-scope**
- **Shipped as verification, not enablement:** mono#2624. Sandbox denial, `ReviewResult` round-trip, and Standard write access proven live. `PrReview` remains pinned to Claude via the review pool. Later `CommandOutcomeObservation` required-strict additionally refuses Grok for `ConflictResolution` / `CiRemediation` at the capability gate.

### T-24 Characterise Grok `Notification` types and earn `AwaitingInputSignal`

Map blocked-on-human notifications, or record a negative result.

- **Effort:** `small`
- **Depends on:** T-10
- **Scope: in-scope**
- **Shipped:** mono#2537 (with T-27). Negative result: capability stays undeclared.

### T-25 Make pre-trust and config-dir gitignore driver-supplied

Move Claude-only workspace writes behind the driver.

- **Effort:** `small`
- **Depends on:** T-07
- **Scope: in-scope**
- **Shipped:** mono#2498.

### T-26 Route prose-scrape fallback consumers through the resolved driver

Replace concrete `ClaudeDriver` constructions with registry resolution (pool-aware).

- **Effort:** `medium`
- **Depends on:** T-19
- **Scope: in-scope**
- **Shipped:** mono#2529. Also fixed `one_turn_per_process` misclassification for pool-dispatched reviewers.

### T-27 Investigation: the Grok leader process under concurrent workers

Isolation, lifetime, reap behaviour.

- **Effort:** `small`
- **Depends on:** T-13
- **Scope: was deferred; characterisation still landed**
- **Shipped:** mono#2537. Isolated per home; not spawned under Boss flags; escapes process-group reap if enabled.

### T-28 Extract Claude's permission rendering into the driver crate

- **Effort:** `medium`
- **Depends on:** none
- **Scope: deferred (future / not a v1 blocker)** — still open; Grok and Codex implement the method for real and route around Claude's no-op.

### T-29 Remote/SSH dispatch for Grok

- **Effort:** `large`
- **Depends on:** T-21
- **Scope: deferred (future / not a v1 blocker)** — still open; local dispatch is the shipped target.

### T-30 Per-driver capacity and rate-limit accounting seams

- **Effort:** `medium`
- **Depends on:** T-21
- **Scope: deferred (future / not a v1 blocker)** — still open; load balancing remains out of scope.

### T-31 Grok eligibility for triage and the answer agent

- **Effort:** `medium`
- **Depends on:** T-23, T-26
- **Scope: deferred (future / not a v1 blocker)** — still open. T-26's prose-scrape block is cleared; product enablement remains.

### Scope added during implementation (not in the original ladder)

| Addition                                                           | PR        | Why it was not optional                                                                       |
| ------------------------------------------------------------------ | --------- | --------------------------------------------------------------------------------------------- |
| Per-connection events-socket driver resolution                     | mono#2520 | Without it every Grok hook event was normalised as Claude and dropped                         |
| Bridge `gh` / `ssh` / `git` / cube state into scoped `HOME`        | mono#2517 | Scoped HOME closed D-1 and simultaneously made `cube pr create` impossible                    |
| Grok per-run home retention sweep                                  | mono#2570 | Homes live under a temp root, not the cube workspace; without reclaim they accumulate forever |
| `bossctl agents transcript` driver normaliser path                 | mono#2584 | Grok ACP dialect has no top-level schema field the Claude/Codex direct path expects           |
| Adapter identity: runner-injected keys, not `GROK_AGENT`           | mono#2597 | Wrong identity made every tool call fail closed                                               |
| Keep `--no-subagents` with measured session_end attribution defect | mono#2700 | Subagent `session_end` can mark the parent slot `Terminated`                                  |

**Not in this graph:** the `PATH`-shim project inherited from the Codex analysis remains independent and unstarted — still the right structural answer to hook fail-open for all three drivers.
