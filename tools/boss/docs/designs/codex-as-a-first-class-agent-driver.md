# Codex as a first-class agent driver

- **Date:** 2026-07-24
- **Project:** Codex as a first-class agent driver
- **Depends on:** [P1422 — agent-driver abstraction](agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md)
- **Supersedes intent of:** [P284 — Copilot CLI as alternative worker backend](copilot-cli-as-alternative-worker-backend.md) (its "JSON stream schema spike" method is reused here)
- **Boss tree verified at:** `7859b6c4` (`main`), 2026-07-24
- **Codex verified at:** `codex-cli 0.145.0`, `macos-aarch64`, standalone install, `~/.local/bin/codex`
- **Previously verified at:** `codex-cli 0.137.0` — every claim re-run on 0.145.0; see [Version delta](#version-delta-01370--01450)

## TL;DR / verdict

Codex is a **better** fit for the P1422 abstraction than the abstraction currently assumes, and a **worse** fit for the parts of Boss that never went through the abstraction at all.

The brief's highest-severity claim — _"Codex has no Stop hook, so a Codex worker would never complete"_ — **is wrong, but the conclusion it drives is still right, for a different reason.** Codex emits `turn.started` / `turn.completed` as native, typed events on its `--json` stdout stream, so turn boundaries are strictly _better_ than Claude's (in-band and structural, not a hook that must be installed). Codex also ships a stable, Claude-wire-compatible hooks system — including a `Stop` hook.

The real blocker is one layer down: **Boss's only progress ingress is a unix socket fed by the `boss-event` shim, which only exists because Claude can be made to run a command on every hook.** Codex's signal arrives on the worker's _stdout_, which the engine currently does not read at all. So the gap is not "Codex lacks turn boundaries" — it is **"`ProgressObservation` abstracts event _normalisation_ but not event _transport_."** That is the single amendment that most changes P1422's remaining work.

Second finding, revised on 0.145.0: **Codex hooks do fire under `codex exec`, and `PreToolUse` deny genuinely blocks a command before it runs.** On 0.137.0 no hook fired in nine configurations; on 0.145.0 the _identical_ configuration fires reliably. That resolves the original OQ-1 — but it does **not** change the chosen design, because hooks fail **open and silently** in two independent ways: an untrusted hook is skipped with no warning, and a hook whose command does not exist produces no diagnostic. A guardrail carrier that can silently evaporate is not one Boss can rely on. See [OQ-1](#oq-1-hook-trust-provisioning).

**The `PATH`-shim design is therefore retained, and is now better justified than when hooks appeared not to work at all**: shims are the load-bearing guardrail, and hooks become defence-in-depth that Boss may additionally declare.

Third finding: that mechanism already half-exists. Boss prepends `BOSS_BIN_DIR` to the worker's `PATH` (`engine/core/src/runner/pane_spawn.rs:382`). Moving Boss's command-level guardrails from `PreToolUse` hooks into **`PATH` shims** makes them driver-agnostic, closes a real hole in the Claude path (a hook cannot see a command run inside a subshell), and is the reason most work-item kinds can be Codex-eligible without hooks at all.

## Goals

- Add OpenAI Codex as a real driver behind the P1422 agent-driver abstraction, so a work item dispatched with `--driver codex` runs end-to-end to a PR with the same lifecycle guarantees a Claude worker has today.
- Produce a **complete gap analysis** — the primary deliverable. Where Codex does not fit the current trait surface, name the abstraction gap and specify the fix _in the abstraction_, never as Codex-specific special-casing in the engine.
- Feed those findings back into P1422's remaining tasks. This project and P1422 are deliberately co-dependent; the [Proposed P1422 amendments](#proposed-p1422-amendments) section is the handoff.
- Identify the seams a future Codex/Claude load balancer will need, so this work does not foreclose it.

## Non-goals

- **Implementing the load balancer.** Out of scope by operator direction. This doc identifies the seams it attaches to and specifies nothing about policy.
- **Removing or de-privileging the Claude path.** Claude remains the reference driver and the default.
- **Codex Cloud, `codex app-server`, `codex mcp-server`, `codex remote-control`.** v1 drives `codex exec` only. The app-server is a strictly richer surface and a plausible v2 (see [Alternative 3](#alternative-3-drive-codex-app-server-over-json-rpc)).
- **App-side / Swift changes.** The kanban already reads abstract `WorkerActivity`; nothing app-side needs to know which driver ran.
- **Remote/SSH dispatch for Codex.** `engine/core/remote/boss-remote-run.sh:84,159,162` is 100% hardcoded Claude. Deferred, and filed as such.
- **Re-litigating the P1422 capability vocabulary.** The 12 capabilities are the right decomposition; this doc changes signatures and adds two, it does not re-open the model.

## Method

Everything about Codex below was established by **running Codex on this host on 2026-07-24**, not from recollection. Where a claim comes from the binary's embedded generated schemas rather than an observed run, it is marked _(binary)_. Where I could not establish something, it is an explicit open question rather than an assertion.

The doc was first written against `0.137.0`. On operator request, **every Codex claim was then re-run against `0.145.0`** — the version now installed — rather than having the version string bumped. The body below states 0.145.0 behaviour; [Version delta](#version-delta-01370--01450) reports what moved, because the churn across eight minor versions is itself a design input.

Boss-side claims were re-verified against `7859b6c4` by locating symbols, not line numbers. **The brief's ground-truth section has already drifted**: the dispatch gate it cites as `engine/core/src/runner.rs:1320-1335` is now `engine/core/src/runner/worker_spawn.rs:597-601` — `runner.rs` has been split into a module directory. Treat the line numbers in _this_ doc the same way.

The spike harness (isolated `CODEX_HOME`, throwaway git repo, hook handler logging its stdin) is reproduced inline in [Appendix A](#appendix-a-reproducing-the-codex-spike).

---

## Version delta: 0.137.0 → 0.145.0

The whole spike was re-run on 0.145.0. **Two deltas change the design, one changes a task's scope, and the rest are drift** — but the drift is the point: eight minor versions moved the event stream three times without any version marker on the wire, which converts [OQ-2](#oq-2) from a precaution into an evidenced requirement.

### Deltas that change the design

**D-1 — `-a/--ask-for-approval` is gone from `codex exec`. This design's launch command would have failed outright.**

```
$ codex exec --json -a never -C "$REPO" "..."
error: unexpected argument '-a' found
```

Approval policy is now fixed for headless exec: a debug-log capture of a real run shows `approval_policy=Never` with no flag supplied. The requirement the flag encoded — _"any other policy can block a headless worker forever waiting for an approval nobody will give"_ — is now satisfied **by default**. The [execution shape](#execution-shape) drops the flag. Removing a stable flag in a minor release is also the single clearest argument for pinning.

**D-2 — Hooks fire. `PreToolUse` deny genuinely blocks. [OQ-1](#oq-1-hook-trust-provisioning) is resolved and inverted.**

The configuration that produced nothing across nine variants on 0.137.0 fires reliably on 0.145.0. All three probed events delivered Claude-shaped payloads:

````jsonl
{"session_id":"019f95c2-…","transcript_path":"…/rollout-2026-07-24T13-13-28-019f95c2-….jsonl","cwd":"…","hook_event_name":"SessionStart","model":"gpt-5.5","permission_mode":"bypassPermissions","source":"startup"}
{"session_id":"019f95c2-…","turn_id":"019f95c2-…","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo hooktest"},"tool_use_id":"call_HZE8…"}
{"session_id":"019f95c2-…","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"Output:\n\n```text\nhooktest\n```"}
````

A deny handler blocked the command **before execution**, and the reason reached the model:

```
ERROR codex_core::tools::router: error=Command blocked by PreToolUse hook: BOSS GUARDRAIL: this command is blocked by policy.. Command: echo shouldbeblocked
```

Three details matter more than the headline:

- `tool_name` is **`Bash`** and `permission_mode` is **`bypassPermissions`** — Claude's exact vocabulary, not a lookalike. Boss's existing hook payload parsing is closer to reusable than assumed.
- **`transcript_path` _is_ stamped on hook payloads.** [G-9](#g-9-transcriptaccess) and [A-6](#proposed-p1422-amendments) assumed Codex offers no such field and the path must be derived from `thread_id` + `CODEX_HOME`. That derivation is still needed for the hookless path, but it is no longer the only option.
- The hook payload's `session_id` and the stream's `thread_id` carried the **identical value** (`019f95c2-f090-7910-b9cc-8d8363aeb9c3`). The doc previously inferred these were the same concept under two names; that is now observed, not inferred.

**Why this does not change the chosen approach.** Hooks fail **open, silently, in two independent ways**:

| Failure mode                | Observed behaviour                                                                   |
| --------------------------- | ------------------------------------------------------------------------------------ |
| Hook not trusted            | Command runs normally. **No warning, no stream event, no log line.**                 |
| Hook command does not exist | Turn completes normally. **No diagnostic** — the 0.137.0 control reproduces exactly. |

Hooks run only under `--dangerously-bypass-hook-trust` or a persisted trust record — `[hooks] trusted_hash`, a real key (`--strict-config` accepts it; `HookStateToml.trusted_hash` _(binary)_). A wrong or stale hash is indistinguishable from no hooks at all. For a guardrail carrier that is disqualifying: Boss rewrites worker config per run, so a hash that goes stale would silently disarm every guardrail with no signal. `PATH` shims fail **closed** — a missing shim means the real binary is not on `PATH` and the command errors loudly.

So [Guardrail integrity](#guardrail-integrity) stands unchanged, and [Alternative 1](#alternative-1-replicate-the-claude-architecture--make-codex-emit-hook-callbacks) is still rejected — now on fail-open semantics rather than on non-functioning hooks, which is a stronger reason.

**Deny-only is unchanged**, so nothing in [G-6](#g-6-tooluseinterception) about rewriting relaxes: `unsupported permissionDecision:allow` and `unsupported permissionDecision:ask` are both still present _(binary)_, and `updatedInput` still requires the rejected `allow`. Tool-input rewriting remains unreachable via Codex hooks.

### Delta that changes a task's scope

**D-3 — `codex exec review` is a native non-interactive review mode**, with `--base <BRANCH>`, `--commit <SHA>`, `--uncommitted`, and `--title`; there is a `codex-auto-review` model in the catalog. [T-25](#t-25-codex-eligibility-for-review-and-conflict-resolution-kinds) assumed review would be an ordinary `codex exec` run under `--sandbox read-only`. It should evaluate this purpose-built surface first — it may be a better fit for Boss's review kind than a general exec run, and it is the one place Codex offers something Boss's Claude path has no direct analogue for.

### Stream drift — all silent, all additive

Envelope types are unchanged (`thread.started`, `turn.started`, `item.started`, `item.completed`, `turn.completed`), and there is **still no schema version field**. Underneath that stable surface, three things moved:

| #   | Change                                                                                                                                       | Consequence                                                                                                                      |
| --- | -------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| D-4 | `usage` gained **`cache_write_input_tokens`**                                                                                                | [Load-balancing seam 2](#load-balancing-seams) enumerates the usage fields; it must treat the set as open, not fixed.            |
| D-5 | Item IDs are now **0-based** (`item_0`); the 0.137.0 capture began at `item_1`                                                               | The [T-12](#t-12-codexdriver-progress-normaliser) normaliser must not assume a 1-based index or derive ordering from the number. |
| D-6 | `item.completed` with `type:"error"` now carries **operational warnings**, not just turn failures (the trust-bypass notice arrives this way) | T-12 must not treat every `error` item as a failed turn, or trust-bypass warnings will abort healthy workers.                    |

`TurnItem` grew from ten variants to **fourteen** _(binary)_ — adding `DynamicToolCall`, `CollabAgentToolCall`, `Extension`, `EnteredReviewMode`. Hook events grew from ten to **eleven**, adding `SessionEnd` _(binary)_. T-12 and [T-22](#t-22-extend-the-reference-driver-conformance-harness-a-12-amends-t1483) must both treat these enums as open and ignore-with-logging on unknown variants.

One genuinely useful addition: **`--strict-config`** _("Error out when config.toml contains fields that are not recognized by this version of Codex")_. That is a real conformance assertion Boss can run in T-22 to detect a config-schema break at startup rather than at runtime. It does **not** cover the event stream, which remains unversioned.

### Model and effort — [G-4](#g-4-modelandeffortmenu) needs restating

The reasoning ladder is **per-model, and up to six levels**, not the flat three recorded against 0.137.0. From `codex debug models`:

| Model                     | Default  | Supported reasoning levels             |
| ------------------------- | -------- | -------------------------------------- |
| `gpt-5.6-sol`             | `low`    | `low, medium, high, xhigh, max, ultra` |
| `gpt-5.6-terra`           | `medium` | `low, medium, high, xhigh, max, ultra` |
| `gpt-5.6-luna`            | `medium` | `low, medium, high, xhigh, max`        |
| `gpt-5.5`                 | `medium` | `low, medium, high, xhigh`             |
| `gpt-5.4`, `gpt-5.4-mini` | `medium` | `low, medium, high, xhigh`             |
| `gpt-5.3-codex-spark`     | `high`   | `low, medium, high, xhigh`             |
| `codex-auto-review`       | `medium` | `low, medium, high, xhigh`             |

G-4 previously read: _"3-value reasoning effort against Boss's 5-value ladder is exactly the `Degrade` case."_ That is no longer true — the range now **meets or exceeds** Boss's 5-value ladder on the newer models and varies per model within one catalog. The capability is therefore not a fixed degrade at all; the menu must be **sourced per-model at runtime**. The doc already recommended reading `codex debug models` rather than hardcoding a table — that recommendation was a convenience before and is **load-bearing now**, and it is exactly the kind of table that would have been wrong within eight minor versions.

### Re-verified unchanged

`CODEX_HOME` isolation is still complete (the spike home grew its own `sessions/`, `state_5.sqlite`, `logs_2.sqlite`, `skills/`, `plugins/`, `models_cache.json`; nothing leaked to `~/.codex`). Auth is still `File` mode with `auth.json` inside `CODEX_HOME`. Sandbox is still three modes, and a live run confirms what was previously a binary-only claim: `sandbox_policy=WorkspaceWrite { writable_roots: [], network_access: false, exclude_tmpdir_env_var: false, exclude_slash_tmp: false }`. Rollout paths keep the `sessions/YYYY/MM/DD/rollout-<ISO8601>-<thread_id>.jsonl` shape. The stdin gotcha still applies — `< /dev/null` is still required.

Worth noting for the [coexistence hazard](#migration-and-coexistence): `external_config_migration_prompts` still exists, and 0.145.0 adds an `external_agent_memory_import` feature plus an `external_agent_config_imports` table in `state_5.sqlite` — a **second** Claude-import vector beyond config, pointed at agent memory. `codex doctor` now reports 0.145.0 as current (`latest version status: current version is not older`).

One clarification the stdout-reader work depends on, checked explicitly because [T-05](#t-05-engine-side-stdout-jsonl-progress-reader) rests on it: **`--json` events go to stdout and the human-readable `Reading additional input from stdin...` notice goes to stderr.** The JSONL stream is uncontaminated, so the reader needs no filtering.

---

## What Codex actually is

### Invocation and headless mode

`codex exec` is a real, first-class non-interactive mode — not a scraped TUI. Verified flags (`codex exec --help`, 0.145.0):

| Flag                                                             | Meaning                                                              |
| ---------------------------------------------------------------- | -------------------------------------------------------------------- |
| `--json`                                                         | Emit events to stdout as JSONL                                       |
| `-o, --output-last-message <FILE>`                               | Write the agent's last message to a file                             |
| `--output-schema <FILE>`                                         | JSON Schema describing the model's final response shape              |
| `-C, --cd <DIR>`                                                 | Working root                                                         |
| `--add-dir <DIR>`                                                | Additional writable directories                                      |
| `-s, --sandbox <read-only\|workspace-write\|danger-full-access>` | Sandbox policy                                                       |
| `--dangerously-bypass-approvals-and-sandbox`                     | No sandbox, no prompts                                               |
| `-m, --model <MODEL>`                                            | Model                                                                |
| `-c, --config <key=value>`                                       | Per-invocation config override (dotted path, TOML-parsed value)      |
| `-p, --profile <NAME>`                                           | Layer `$CODEX_HOME/<NAME>.config.toml` over the base config          |
| `--ignore-user-config`                                           | Do not load `$CODEX_HOME/config.toml` (auth still uses `CODEX_HOME`) |
| `--ephemeral`                                                    | Do not persist session files                                         |
| `--skip-git-repo-check`                                          | Allow running outside a git repo                                     |
| `--dangerously-bypass-hook-trust`                                | Run enabled hooks without persisted trust                            |
| `--ignore-rules`                                                 | Do not load user or project execpolicy `.rules` files                |
| `--strict-config`                                                | Error on config fields this version does not recognise               |
| `--enable / --disable <FEATURE>`                                 | Equivalent to `-c features.<name>=true\|false`                       |
| `-i, --image <FILE>...`                                          | Attach image(s) to the initial prompt                                |

**`-a/--ask-for-approval` no longer exists on `codex exec`** — it was removed between 0.137.0 and 0.145.0, and passing it is a hard error. Headless exec now runs `approval_policy=Never` unconditionally (observed in a debug-log capture), which is what Boss wanted anyway. See [D-1](#deltas-that-change-the-design).

`codex exec resume [--last | <id>]` resumes a session. `codex exec review` runs a native non-interactive code review ([D-3](#delta-that-changes-a-tasks-scope)).

**Operational gotcha, verified.** The prompt may be a positional argument _or_ stdin. When stdin is neither a TTY nor redirected, Codex prints `Reading additional input from stdin...` and consumes it. Every spawn Boss issues must redirect `< /dev/null`. This bit the first spike run.

### The event stream

Real capture (`codex exec --json`, prompt: _"Run the shell command 'echo probe' and report its output."_):

````jsonl
{"type":"thread.started","thread_id":"019f95c2-7197-7432-a9c0-f22eb0293766"}
{"type":"turn.started"}
{"type":"item.started","item":{"id":"item_0","type":"command_execution","command":"/bin/zsh -lc 'echo probe'","aggregated_output":"","exit_code":null,"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_0","type":"command_execution","command":"/bin/zsh -lc 'echo probe'","aggregated_output":"probe\n","exit_code":0,"status":"completed"}}
{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"`echo probe` output:\n\n```text\nprobe\n```"}}
{"type":"turn.completed","usage":{"input_tokens":26318,"cached_input_tokens":14080,"cache_write_input_tokens":0,"output_tokens":49,"reasoning_output_tokens":0}}
````

Observed envelope types: `thread.started`, `turn.started`, `turn.completed`, `item.started`, `item.completed`. Observed item types: `agent_message`, `command_execution`, `error`. The full item enum _(binary, `codex_protocol::items::TurnItem`)_ is `UserMessage`, `HookPrompt`, `AgentMessage`, `Reasoning`, `DynamicToolCall`, `CollabAgentToolCall`, `WebSearch`, `ImageView`, `Extension`, `ImageGeneration`, `EnteredReviewMode`, `FileChange`, `McpToolCall`, `ContextCompaction` — fourteen variants, up from ten on 0.137.0.

Three things matter here:

1. **Turn boundaries are native and in-band.** `turn.started` / `turn.completed` are structural stream events. Nothing needs installing. This is a stronger signal than Claude's `Stop` hook, which depends on a settings file being written correctly and a shim binary being on `PATH`.
2. **`turn.completed` carries token usage.** Boss gets per-turn accounting for free — directly useful to the future balancer's rate-limit state.
3. **The stream is not versioned, and it demonstrably drifts.** No schema version field on any envelope, yet across eight minor versions `usage` gained a field, item IDs changed base, `error` items took on a second meaning, and the item enum grew by four variants — every one of them silently. Boss must pin the version it drives and conformance-test it ([D-4 through D-6](#stream-drift--all-silent-all-additive)).

A fourth point, verified because the whole transport design depends on it: **the JSONL goes to stdout, and only to stdout.** The one human-readable line Codex emits (`Reading additional input from stdin...`) goes to stderr, so a reader attached to stdout sees clean JSONL with no filtering.

### Session, turn, and transcript identity

- Session identity is **`thread_id`** (UUIDv7), not `session_id`. Note the collision hazard: Codex's _hook_ payloads use the field name `session_id` _(binary)_, while its _stream_ uses `thread_id`. These are different names for the same concept and a driver must not confuse them.
- Turn identity is `turn_id`, exposed to hooks as a documented _"Codex extension"_ _(binary)_.
- Transcripts ("rollouts") are JSONL at `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ISO8601>-<thread_id>.jsonl`. Verified from the spike: `sessions/2026/07/24/rollout-2026-07-24T12-26-47-019f9598-31f6-78e3-94c2-34836872ae2c.jsonl`. That the container format is JSONL is convenient — `engine/transcript-tail/src/lib.rs` can be reused — but the _line schema_ is Codex's, not Claude's.

### Config discovery and isolation — the concurrency question

The brief flags global-only config as _"a serious problem for concurrent workers sharing a machine."_ **Verified: it is not a problem, because `CODEX_HOME` isolates completely.**

Pointing `CODEX_HOME` at a scratch directory produced a fully independent Codex home — its own `sessions/`, `state_5.sqlite`, `logs_2.sqlite`, `skills/`, `models_cache.json`, `config.toml`. Nothing leaked to `~/.codex`. Per-worker `CODEX_HOME` is therefore the isolation lever, and it is a complete one.

Layering, in precedence order:

- `$CODEX_HOME/config.toml` — user layer.
- `$CODEX_HOME/<name>.config.toml` via `-p/--profile` — layered over the base.
- Project `.codex/config.toml` — trusted-repo settings (sandbox, MCP, hooks, model, reasoning defaults).
- `-c key=value` — per-invocation, highest precedence.
- `--ignore-user-config` — skip the user layer entirely.

The real global-state hazard is elsewhere: the default `~/.codex/config.toml` on this host contains a **per-project trust registry**:

```toml
[projects."/Users/brianduff/Documents/dev/mono"]
trust_level = "trusted"
```

Cube workspaces live at _different_ paths from the source repo, so a fresh workspace is untrusted by default. With a per-worker `CODEX_HOME` Boss controls this file and can pre-stamp the workspace as trusted — but a driver that reused `~/.codex` would be racing every other worker to write it. **Per-worker `CODEX_HOME` is not an optimisation; it is required for correctness.**

Agent-rules file is **`AGENTS.md`**, not `CLAUDE.md` — already abstracted as `DriverDescriptor.config_dir` / `agent_rules_preamble` (`engine/driver/src/lib.rs:615`).

### Auth, and coexistence with Claude

`codex doctor` reports: `auth storage mode File`, `auth file ~/.codex/auth.json`, `stored auth mode chatgpt`, `stored ChatGPT tokens true`, `stored API key false`.

Auth is a **file inside `CODEX_HOME`**, not an environment variable. Consequences:

- A per-worker `CODEX_HOME` must have `auth.json` present. Symlinking the host's is sufficient (that is what the spike did) and avoids duplicating a credential per workspace.
- There is **no collision** with the `unset ANTHROPIC_API_KEY` line at `engine/core/src/runner/pane_spawn.rs:382`. It is inert for Codex. It is still a Claude-ism sitting in shared spawn code and belongs behind the driver — see [G-1](#g-1-spawn).
- Codex may also authenticate by API key. Boss should not care; it should treat `CODEX_HOME` as opaque auth state and let the operator provision it.

### Sandbox and approval — and what Boss's deny rules become

Codex offers **three sandbox modes** and **four approval policies**. There is **no per-tool rule grammar** — nothing corresponding to Boss's `Bash(...)` / `Read(...)` deny strings emitted today by `engine/core/src/worker_setup.rs`.

What Codex offers instead is arguably stronger for the _filesystem_ half and absent for the _command_ half:

- `[sandbox_workspace_write]` with `writable_roots`, `network_access`, `exclude_tmpdir_env_var`, `exclude_slash_tmp` _(binary)_. This is an **OS-enforced** boundary, not an advisory hook.
- A `.rules` execpolicy system (`--ignore-rules`; error strings `Error parsing rules; custom rules not applied`) _(binary)_. Unexamined — see [OQ-3](#oq-3-what-is-the-codex-rules-execpolicy-format).
- Network policy with `domains` / `allow` / `actions` / `methods` / `path_prefixes` _(binary)_.

Fidelity mapping for the rules Boss expresses today:

| Boss rule                                            | Codex equivalent                                                                                  | Fidelity                                        |
| ---------------------------------------------------- | ------------------------------------------------------------------------------------------------- | ----------------------------------------------- |
| Reviewer read-only                                   | `--sandbox read-only`                                                                             | **Exact**, and OS-enforced rather than advisory |
| Deny writes to `~/Library/Application Support/Boss/` | `workspace-write` — the Boss data dir is outside the workspace, so it is denied _by construction_ | **Stronger than today**                         |
| Deny `rm -rf`, `sudo`                                | none                                                                                              | **Lost** — no per-command grammar               |
| Deny `bossctl`                                       | none                                                                                              | **Lost** as a rule; recoverable via `PATH` shim |
| Block `jj git push` / `gh pr create`                 | none                                                                                              | **Lost** as a rule; recoverable via `PATH` shim |

The two "lost" rows are what [Guardrail integrity](#guardrail-integrity) resolves.

### Hooks — the decisive investigation

This determines the `ToolUseInterception` disposition, so I went at it hard.

**What is certainly true.** `codex features list` reports `hooks   stable   true`. The binary embeds generated JSON Schemas for **eleven** hook events, as `<event>.command.input` / `<event>.command.output` pairs: `pre-tool-use`, `post-tool-use`, `permission-request`, `pre-compact`, `post-compact`, `session-start`, `session-end`, `user-prompt-submit`, `subagent-start`, `subagent-stop`, `stop`. (`session-end` is new in 0.145.0.)

The wire format is **deliberately Claude-Code-compatible**, and on 0.145.0 this is confirmed against live payloads rather than only against the embedded schemas. `PreToolUse` input carries `hook_event_name`, `session_id`, `transcript_path`, `cwd`, `model`, `permission_mode`, `tool_name`, `tool_input`, `tool_use_id`, plus Codex's `turn_id` — and an observed capture shows `tool_name: "Bash"` and `permission_mode: "bypassPermissions"`, i.e. Claude's literal tool name and enum value. `permission_mode` is Claude's exact enum: `default | acceptEdits | plan | dontAsk | bypassPermissions`. Output is `hookSpecificOutput` / `hookEventName` / `permissionDecision` / `permissionDecisionReason` / `decision` / `reason` / `continue` / `systemMessage` / `stopReason` / `suppressOutput`. The `stop.command.output` schema even carries the comment:

> _"Claude requires `reason` when `decision` is `block`; we enforce that semantic rule during output parsing rather than in the JSON schema."_

**`PreToolUse` is deny-only.** From the binary's validation strings:

- `PreToolUse hook returned unsupported permissionDecision:allow`
- `PreToolUse hook returned unsupported permissionDecision:ask`
- `PreToolUse hook returned unsupported decision:approve`
- `PreToolUse hook returned updatedInput without permissionDecision:allow`
- `PreToolUse hook returned unsupported continue:false` / `...stopReason` / `...suppressOutput`
- deny requires a non-empty `permissionDecisionReason`

Since `updatedInput` requires `permissionDecision:allow`, and `allow` is rejected, **tool-input rewriting is unreachable** even if hooks work. `PostToolUse` supports `decision: block` + `reason`. Handler kinds `async`, `prompt`, and `agent` are parsed but skipped — _"not supported yet"_.

Configuration _(confirmed against `learn.chatgpt.com/docs/config-file/config-advanced`)_:

```toml
[[hooks.PreToolUse]]
matcher = ".*"

[[hooks.PreToolUse.hooks]]
type = "command"
command = '/path/to/script'
timeout = 30
statusMessage = "Description"
```

Handlers may also live in a per-layer `hooks.json`; both in one layer loads both with a warning. Project-local hooks load only when the `.codex/` layer is trusted; user-level hooks are independent of project trust.

**Hooks fire on 0.145.0 — and did not on 0.137.0.** On the older version no hook fired in nine configurations (`hooks.json` at `$CODEX_HOME`; `hooks.json` at `<project>/.codex/`; TOML `[[hooks.PreToolUse]]` CamelCase; TOML `[[hooks.pre_tool_use]]` snake_case; `matcher = "*"`, `matcher = ".*"`, and none; `command` as a shell string and as a bare executable path; with and without `--dangerously-bypass-hook-trust`). On 0.145.0 the **first** of those configurations — CamelCase TOML, `matcher = ".*"`, with the trust-bypass flag — fires `SessionStart`, `PreToolUse`, and `Stop` reliably, and a deny decision blocks the command before it executes. Payloads and evidence are in [D-2](#deltas-that-change-the-design).

**The trust gate is the operative constraint, and it fails open.** Hooks run only if trusted. Trust comes from either `--dangerously-bypass-hook-trust` or a persisted `[hooks] trusted_hash` record (`HookStateToml` _(binary)_; `--strict-config` accepts the key, so it is real). Without trust, the hook is **skipped in complete silence** — the command runs, and there is no warning, no stream event, and no log line. Separately, a hook whose command does not exist also produces **no diagnostic**: the 0.137.0 control (`command = "/definitely/not/a/real/binary-xyz"`) reproduces exactly on 0.145.0, completing the turn as if nothing were configured.

Both failure modes are silent and fail-open. That is why this resolution does **not** move guardrails onto hooks: Boss regenerates worker config every run, so a `trusted_hash` that goes stale — or a shim path that moves — would disarm every guardrail with nothing to observe. `PATH` shims fail closed, which is the property a guardrail needs.

**What remains open is trust _provisioning_, not activation:** how to compute and persist `trusted_hash` so a Boss worker gets hooks without shipping `--dangerously-bypass-hook-trust`. That flag is not an acceptable default — it also trusts **project-local** `.codex/` hooks from the repository under work, which in Boss's threat model is attacker-controllable content. See [OQ-1](#oq-1-hook-trust-provisioning) and the re-scoped [T-01](#t-01-codex-hook-trust-provisioning).

### Model and effort

`-m/--model`; `model_reasoning_effort` in config (`-c model_reasoning_effort=...` per invocation). Config also carries `plan_mode_reasoning_effort`, `model_verbosity`, `model_supports_reasoning_summaries` _(binary)_.

The reasoning ladder is **per-model and runs up to six levels** — `low, medium, high, xhigh, max, ultra` on `gpt-5.6-sol` / `gpt-5.6-terra`, four on `gpt-5.5` — so it is not a single fixed vocabulary that Boss can map once. The full catalog is tabulated in the [version delta](#model-and-effort--g-4-needs-restating); on 0.137.0 only `low` / `medium` / `high` were observed.

`codex debug models` renders the raw model catalog as JSON, including each model's `supported_reasoning_levels` and `default_reasoning_level` — that is the right source for a `ModelMenu`, rather than a hardcoded table. Between 0.137.0 and 0.145.0 both the model list and the effort ladder changed, so a hardcoded table would already be stale.

### Claude-Code interop — a coexistence hazard

Codex ships explicit Claude Code config _import_: binary strings include `codex.external_agent_config.detect`, `codex.external_agent_config.import`, `settings.json`, `.claude`, `CLAUDE.md`, `agents`, `hooks.json`, alongside `CLAUDE_PLUGIN_ROOT` / `CLAUDE_PLUGIN_DATA` and `.claude-plugin/plugin.json`.

Every Boss workspace contains a `.claude/` directory (gitignored by the engine, per `.claude/CLAUDE.md`) with settings and hook wiring for the Claude path. **A Codex worker launched in that workspace may detect it and offer to import it.** That is a real collision — Boss's Claude hook config referencing the `boss-event` shim is exactly the wrong thing for Codex to adopt. The Codex driver must suppress this (`external_config_migration_prompts` exists in the config schema _(binary)_) and must not write `.claude/` at all. See [Migration and coexistence](#migration-and-coexistence).

---

## Per-capability gap analysis

Classification per the brief: **(a)** implementable against the current trait, **(b)** needs a trait signature change, **(c)** needs new engine machinery, **(d)** genuinely absent.

| #    | Capability              | What Codex offers natively                     | Class    | Verdict                                    |
| ---- | ----------------------- | ---------------------------------------------- | -------- | ------------------------------------------ |
| G-1  | `Spawn`                 | `codex exec` + flags                           | **(b)**  | Signature is Claude-shaped                 |
| G-2  | `WorkspaceProvisioning` | `AGENTS.md`, `CODEX_HOME`, trust registry      | **(a)**  | Fits; needs `CODEX_HOME` lifecycle         |
| G-3  | `PermissionPolicy`      | 3 sandbox modes, `writable_roots`, `.rules`    | **(b)**  | Trait is a file path; Codex needs argv+env |
| G-4  | `ModelAndEffortMenu`    | `-m`, `model_reasoning_effort`, `debug models` | **(a)**  | Blocked only by T3326                      |
| G-5  | `ProgressObservation`   | `--json` stdout JSONL                          | **(c)**  | **Transport is not abstracted** — top gap  |
| G-6  | `ToolUseInterception`   | deny-only `PreToolUse`, works but fails open   | **(d)**† | Degrade; relocate guardrails               |
| G-7  | `TurnBoundary`          | `turn.started` / `turn.completed`              | **(c)**  | Native, but no trait method                |
| G-8  | `StructuredOutput`      | `--output-schema`, `--output-last-message`     | **(b)**  | Better than Claude's; no trait method      |
| G-9  | `TranscriptAccess`      | rollout JSONL, Codex line schema               | **(b)**  | Trait method exists but is dead code       |
| G-10 | `ControlVerbs`          | process signals; `codex exec resume`           | **(b)**  | Trait has one method, never called         |
| G-11 | `ToolProvisioning`      | MCP, plugins, skills                           | **(a)**  | Unused in v1, as designed                  |
| G-12 | `PromptComposition`     | `AGENTS.md` + preamble                         | **(b)**  | Shared body asserts Claude mechanics       |

† **G-6 is (d) _by choice_, not by absence.** Codex's `PreToolUse` exists and works on 0.145.0 — the four-way legend has no code for "available but not declarable". Boss declines to declare the capability because the mechanism fails open and silently, so it is not one Boss can promise to enforce. See [G-6](#g-6-tooluseinterception).

### G-1 `Spawn`

The trait signature is Claude-shaped (`engine/driver/src/lib.rs:553-560`): `settings_path: Option<&Path>`, `non_opus_auto_mode: bool`, `permission_mode_override: Option<&str>`. `non_opus_auto_mode` is a Claude model-family concept with no Codex meaning; `settings_path` presumes a single settings _file_, whereas Codex needs `CODEX_HOME` (a directory) plus argv.

Worse, `spawn_invocation` returns a `String` that the engine wraps at `engine/core/src/runner/pane_spawn.rs:382`:

```rust
"[ -n \"$BOSS_BIN_DIR\" ] && export PATH=\"$BOSS_BIN_DIR:$PATH\"; unset ANTHROPIC_API_KEY; {}"
```

The `unset ANTHROPIC_API_KEY` is a Claude-ism in shared code (also asserted in tests at `:868,870,942`). It is harmless for Codex but wrong in principle, and it is the wrong shape: Codex needs `CODEX_HOME=<dir>` _exported_, which a string-returning method cannot express cleanly.

**Fix (in the abstraction):** replace the positional Claude-shaped parameters with an opaque `SpawnRequest` struct, and have `spawn_invocation` return a structured `SpawnPlan { env: Vec<(String,String)>, argv_or_shell: String }` so environment mutation is driver-supplied rather than hardcoded in `pane_spawn.rs`. The `PATH`/`BOSS_BIN_DIR` prepend stays engine-side — it is Boss policy, not Claude policy, and [Guardrail integrity](#guardrail-integrity) makes it load-bearing for both drivers.

### G-2 `WorkspaceProvisioning`

Fits the current trait (`provision_workspace(&self, workspace, prompt_text, run_id)`, `engine/driver/src/lib.rs:566`). The Codex driver writes `AGENTS.md` instead of `CLAUDE.md`, provisions a per-run `CODEX_HOME`, and pre-stamps `[projects."<workspace>"] trust_level = "trusted"` to suppress the first-run trust prompt — which is precisely what the capability's doc comment says it is for.

One gap: the trait gives no hook for **teardown**. A per-run `CODEX_HOME` accumulates rollout files (the host's `~/.codex` currently holds 279 active rollouts / 323 MB). Claude needed no teardown so none was designed. Minor, but real.

### G-3 `PermissionPolicy`

`ClaudeDriver::write_permission_config` is still `unimplemented!()` (`engine/driver/src/claude.rs:477-480`); the real logic remains in `engine/core/src/worker_setup.rs` emitting Claude permission grammar. T1479 moves it.

The problem for Codex is the **signature**: `async fn write_permission_config(&self, dest_dir: &Path) -> anyhow::Result<PathBuf>` (`engine/driver/src/lib.rs:573`) hardcodes the assumption that a permission policy _is a file whose path is passed to the CLI_. For Codex the policy is `--sandbox <mode>`, `--ignore-rules` / `--strict-config`, `[sandbox_workspace_write] writable_roots`, and `CODEX_HOME` — argv and config, not a single file path. (`-a/--ask-for-approval` used to belong on that list; it was removed from `codex exec` between 0.137.0 and 0.145.0 — see [D-1](#deltas-that-change-the-design) — which does not weaken the point, since what remains is still argv plus config plus a directory.)

**Fix:** the method should take Boss's _abstract_ policy (autonomous-honour-denies / reviewer-read-only / structural deny set) and return an opaque `PermissionArtifacts { config_files: Vec<PathBuf>, extra_args: Vec<String>, env: Vec<(String,String)> }`. **T1479 as currently scoped is insufficient** — it extracts Claude's logic behind a signature Codex cannot satisfy, which would force a second refactor.

### G-4 `ModelAndEffortMenu`

Re-verified: `menu_for_driver(_driver_slug: &str)` still discards its argument and returns Claude's menu unconditionally (`engine/effort/src/lib.rs:110-112`). `SpawnConfig.claude_effort` is still Claude-named (`engine/effort/src/lib.rs:34`, set at `:182`, consumed at `:62`).

Codex fits the `ModelMenu` model cleanly, but **not as the fixed `Degrade` case this doc originally recorded**. On 0.145.0 the effort ladder is per-model and reaches six values (`low, medium, high, xhigh, max, ultra`) — meeting or exceeding Boss's 5-value ladder on the newer models, and varying between models within one catalog. So the mapping is neither a uniform degrade nor a static table: it must be resolved per selected model from `codex debug models` at runtime.

**T3326 remains correctly scoped**, with two riders: it must rename `claude_effort` to a driver-neutral name (`effort_value`), otherwise the Claude-ism just moves; and the menu it builds must be **per-model**, not per-driver, or Codex's newer models will be silently capped at Boss's older assumption about their ceiling.

### G-5 `ProgressObservation` — the top gap

**This is the finding that most changes P1422's remaining work.**

`ProgressObservationWiring` _is a Claude settings-file `hooks` map_ (`engine/driver/src/lib.rs:445-451`). The engine then does:

```rust
.get_mut("PreToolUse")
.expect("Claude ProgressObservation wiring always includes PreToolUse")
```

at `engine/core/src/worker_setup.rs:530-532` — which would panic on the empty map the trait's own doc comment says a hookless driver returns. T3328 addresses exactly this.

**But T3328 is insufficient**, and this is the co-dependency the project brief was designed to surface. Making the engine _tolerate_ an empty hook map leaves a Codex worker with **no progress signal at all**, because the only ingress is a unix socket fed by the `boss-event` shim, and `engine/core/src/events_socket.rs:248` hardcodes `ClaudeDriver.normalize_progress_event`. Codex's signal is on the worker's **stdout**, which the engine never reads.

So `ProgressObservation` today abstracts _normalisation_ (`normalize_progress_event`, a `serde_json::Value` → `WorkerEvent` function — which is genuinely driver-agnostic and works fine for Codex) but not _transport_. The capability needs a third concept:

```rust
enum ProgressIngress {
    /// Driver installs handlers that call back into Boss's socket (Claude).
    HookCallback(ProgressObservationWiring),
    /// Driver emits a JSONL event stream on the worker's stdout (Codex).
    StdoutJsonl,
}
```

with an engine-side stdout reader that feeds the same normaliser and the same activity machine. Note that `normalize_progress_event` needs no signature change — the transport split is the whole gap.

A related, smaller problem: `WorkerEvent` requires `session_id` on every variant (`protocol/src/worker_event.rs`), and `SessionStartSource` mirrors Claude's `startup|resume|compact`. Codex's identity is `thread_id`, and its `SessionStart` trigger set _(binary)_ is `startup|resume|clear|compact` — a superset. Both need widening.

Finally, `progress_fidelity()` (`engine/driver/src/claude.rs:482`) still has **no callers anywhere** — re-verified. It is either dead code or an unimplemented intent; a Codex driver would declare `Rich` and nothing would consult it.

### G-6 `ToolUseInterception`

Codex hooks **do** fire on 0.145.0 and `PreToolUse` deny genuinely blocks a command pre-execution ([D-2](#deltas-that-change-the-design)). Nonetheless the Codex driver **does not declare this capability** in v1, landing on the default disposition `Degrade` (`engine/driver/src/lib.rs:92`) — because a capability Boss declares is one Boss promises to enforce, and Codex hooks fail **open and silently** when untrusted or misconfigured. Declaring `ToolUseInterception` on a mechanism that can evaporate without a signal would be a worse outcome than declaring it absent and carrying the guardrails somewhere that fails closed.

This is a change of _reason_, not of _decision_: the original basis was "hooks appear not to work at all", and the current basis is "hooks work but cannot be relied upon to have worked". Both land on `PATH` shims as the guardrail carrier.

That default disposition is nevertheless dangerous as things stand: the degrade path exists as **types only**. `PostHocInterceptionFn` and `PostHocInterceptionAction` (`engine/driver/src/lib.rs:497-525`) have **zero engine callers** — re-verified. A driver landing on `Degrade` today silently loses editorial enforcement, the path guard, the revision-PR guard, and the checkleft push guard. **Silent loss of guardrails is not acceptable**, per the brief and per basic prudence.

Even now that hooks fire, Codex's `PreToolUse` is still **deny-only** — re-verified on 0.145.0, where `unsupported permissionDecision:allow` and `unsupported permissionDecision:ask` both persist _(binary)_ — so the _rewrite_ half of editorial enforcement (`PreToolUseDecision::AllowWithRewrite`, `engine/core/src/editorial_hook.rs:78-81`) is unreachable regardless. Usefully, that enum already distinguishes two rewrite paths:

- `AllowWithRewrite { updated_command: Some(cmd) }` — needs `updatedInput`. **Unavailable under Codex.**
- `AllowWithRewrite { updated_command: None }` — the redaction landed in a `--body-file` overwritten on disk. **Works without any rewrite capability.**

That distinction is what makes the `PATH`-shim relocation in [Guardrail integrity](#guardrail-integrity) viable rather than a downgrade.

### G-7 `TurnBoundary`

Enum variant exists (`engine/driver/src/lib.rs:40`), default disposition `Synthesize` (`:92`), **no trait method** — re-verified. Completion runs directly off `WorkerEvent::Stop` (`engine/core/src/completion/stop.rs:12`, `on_stop_inner` at `:168`).

The brief rates this the highest-severity gap on the premise that Codex cannot signal turn end. **That premise is wrong** — `turn.completed` is native, in-band, and carries token usage. Codex's turn boundary is _better_ than Claude's.

The gap is therefore not severity-of-absence but **shape**: there is no trait method, so completion is hardwired to a Claude-hook-derived event. T3325 (add a `TurnBoundary` trait method + engine synthesizer) is **correctly scoped but mis-prioritised** — for Codex the synthesizer is unnecessary; what is needed is the trait method plus the [G-5](#g-5-progressobservation--the-top-gap) stdout transport, and then `turn.completed` maps straight onto `WorkerEvent::Stop`. The synthesizer is still worth building for a hypothetical third driver with neither hooks nor turn events, but it is not on Codex's critical path and should not gate it.

One genuine subtlety: Claude's `Stop` fires per _assistant turn_ within a session; Codex's `codex exec` is **one turn per process**, exiting after `turn.completed`. Boss's probe/nudge loop assumes it can inject a follow-up prompt into a live session (`engine/core/app/pane_delivery.rs`). Under Codex that becomes `codex exec resume`, i.e. a **new process**, not a message into a running one. This is a real lifecycle difference and the main reason [T-17](#t-17-controlverbs-on-the-trait-plus-codex-probenudge-via-exec-resume-a-7) is its own task.

### G-8 `StructuredOutput`

Enum variant at `engine/driver/src/lib.rs:43`, **no trait method**. Engine-side file contract exists — `STRUCTURED_OUTPUT_ENV = "BOSS_STRUCTURED_OUTPUT"` (`engine/core/src/structured_output.rs:37`) — covering review findings, task followups, postmortem followups. Still transcript-scraped: triage (`engine/core/src/automation_triage.rs:498 parse_triage_decision`) and PR URL (`engine/core/src/pr_url_capture.rs`, which reads `tool_response.stdout` from **`PostToolUse` hook events** — a Claude-hook dependency, re-verified at `pr_url_capture.rs:1-6`).

Codex is **better** here than Claude: `--output-schema <FILE>` constrains the final response to a JSON Schema, and `--output-last-message <FILE>` writes it to a known path. That is a native, enforced structured-output contract — strictly stronger than "ask the agent to write a file and hope."

Two consequences:

- **T1476 (file-based `StructuredOutput` contract) is well-directed and should proceed**, because the env-var file contract is the common denominator that works for both drivers. Its scope is sufficient for Codex _as far as it goes_.
- **T1476 is insufficient in one respect:** PR URL capture is not in its scope and is `PostToolUse`-derived. Under Codex there is no `PostToolUse`, so **PR URL capture breaks entirely** — and the PR URL is the acceptance criterion for essentially every Boss work item. Codex's `command_execution` items _do_ carry `aggregated_output` (verified in the spike capture above), so the same regex can run against the stdout stream. But that is new work, in the transport layer, not in T1476.

### G-9 `TranscriptAccess`

`normalize_transcript_entry` (`engine/driver/src/claude.rs:606-616`) is **never called** — re-verified. `engine/core/src/live_status.rs` passes raw JSONL straight to redaction. `engine/transcript-tail/src/lib.rs:1-8` documents itself as _"Incremental JSONL tail-watcher for claude transcript files"_ and hard-assumes that shape.

Codex rollouts are also JSONL, so the tailer is reusable — but path discovery is the problem. Claude's path is discovered because Claude stamps `transcript_path` on hook payloads (`engine/core/src/events_socket.rs`, `live_status_loop.rs`). Codex's `--json` stream does **not** carry `transcript_path` (verified — no such field in any captured envelope). It is derivable as `$CODEX_HOME/sessions/<Y>/<M>/<D>/rollout-*-<thread_id>.jsonl`, and since the driver owns `CODEX_HOME` it can compute it from `thread_id` on `thread.started`.

Codex's **hook** payloads _do_ carry `transcript_path` — confirmed on 0.145.0 ([D-2](#deltas-that-change-the-design)) — but that is not a usable discovery route here, because this design deliberately does not depend on hooks having fired. Derivation from `thread_id` stays the primary mechanism precisely because it works whether or not hooks are trusted.

**Fix:** add a `transcript_path_for_session(&self, session_id) -> Option<PathBuf>` to the trait so discovery is driver-supplied rather than hook-derived, and actually **call** `normalize_transcript_entry` in `live_status.rs` — otherwise Codex transcript lines reach Claude-shaped redaction and summarisation logic.

### G-10 `ControlVerbs`

The trait has only `classify_error` (`engine/driver/src/lib.rs:644`), and it is **never called** — `engine/core/src/transient_recovery.rs` calls `extract_worker_error` / `classify_claude_error` directly. probe / interrupt / stop / reap are not on the trait at all.

For Codex, stop/reap are process signals and work generically. `probe` does not — see [G-7](#g-7-turnboundary): a probe into a Claude session is a message to a live process; into Codex it is `codex exec resume`. Delivery confirmation is worse: today it depends on Claude's `UserPromptSubmit` hook (`engine/core/app/pane_delivery.rs`, with a transcript-scan fallback). Codex has neither, so confirmation must come from observing a new `turn.started` on the resumed session's stream.

Error classification is entirely provider-specific (rate limits, quota, auth expiry all have different shapes and different retry semantics) and Codex's must not route through `classify_claude_error`.

### G-11 `ToolProvisioning`

Unused in v1 for any driver, as P1422 intended. Codex has a rich surface here (MCP servers, plugins, skills, marketplaces) but Boss injects nothing. **No gap.** Noted only because Codex's plugin system is a plausible future home for Boss's own tooling.

### G-12 `PromptComposition`

Only the preamble is driver-supplied (`engine/driver/src/claude.rs:602-604`). The shared prompt body still asserts Claude's _mechanism_ — `"A PreToolUse hook blocks these"` at `engine/core/src/worker_setup.rs:309` and `:372`, plus `engine/core/src/runner.rs` and the editorial-enforcement sentence.

For a Codex worker these sentences **assert a guarantee that is false**. That is not cosmetic: the worker is being told an enforcement mechanism exists that will not stop it. Under the [`PATH`-shim design](#guardrail-integrity) the sentences become true again for both drivers, but the wording must come from the driver, not from shared prose.

---

## Guardrail integrity

Boss's safety properties are enforced today through Claude's `PreToolUse` hook. The brief requires an explicit refuse-vs-degrade call per guardrail. My answer for four of the five is **neither** — relocate the enforcement to a mechanism that does not depend on hooks.

This section is unchanged by the 0.145.0 finding that Codex hooks _do_ work ([D-2](#deltas-that-change-the-design)). The requirement was never "use whatever mechanism exists"; it is that a guardrail must fail **closed**. Codex hooks fail open and silently when untrusted or misconfigured, so they can supplement the shims but must not be what the guarantee rests on.

### The `PATH`-shim insight

Boss already prepends `BOSS_BIN_DIR` to every worker's `PATH` (`engine/core/src/runner/pane_spawn.rs:382`). A guard implemented as an executable named `gh` / `jj` / `git` in `BOSS_BIN_DIR`, which evaluates the invocation and then delegates to the real binary, is:

- **driver-agnostic** — it needs no hook, no settings file, no per-agent wire format;
- **strictly more robust than the hook** — a `PreToolUse` hook sees the top-level `Bash` tool call, so `sh -c 'gh pr create ...'` nested in a script or a subshell evades it; a `PATH` shim catches every invocation regardless of nesting;
- **already the enforcement point Boss tells workers to use** — `.claude/CLAUDE.md` instructs workers to use `cube pr create`, which is itself a Boss-controlled binary.

This is not a Codex workaround. It closes a real hole in the Claude path, and it should be adopted for both drivers.

### Per-guardrail calls

| Guardrail                              | Enforced today                                                            | Under Codex                                                                                  | Call                                                                   |
| -------------------------------------- | ------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| **Boss data-dir path guard**           | `PreToolUse` deny (`PATH_GUARD_SCRIPT`, `worker_setup.rs:918`)            | `--sandbox workspace-write`: the Boss data dir is outside the workspace and denied by the OS | **Preserved, strengthened.** Advisory hook → kernel-enforced boundary. |
| **Reviewer read-only**                 | per-kind deny rules (`reviewer_deny_rules`)                               | `--sandbox read-only`                                                                        | **Preserved, strengthened.** Exact semantic match, OS-enforced.        |
| **checkleft push guard**               | `PreToolUse` deny (`CHECKLEFT_PUSH_GUARD_SCRIPT`, `worker_setup.rs:1072`) | `PATH` shim on `jj` / `git`                                                                  | **Preserved via relocation.**                                          |
| **Revision-PR guard / no direct push** | `PreToolUse` deny                                                         | `PATH` shim on `jj` / `git` / `gh`                                                           | **Preserved via relocation.**                                          |
| **Editorial enforcement**              | `PreToolUse` deny **and rewrite**                                         | `PATH` shim on `gh`: deny works; rewrite works **only via `--body-file`**                    | **Partially preserved — this one needs a decision.**                   |

### The editorial case, precisely

`PreToolUseDecision` has three outcomes (`engine/core/src/editorial_hook.rs:70-84`). Under a `PATH` shim:

- `Deny { reason }` — works. The shim exits non-zero with the reason on stderr; the agent reads it and retries.
- `AllowWithRewrite { updated_command: None }` — works. The redaction is written into the `--body-file` on disk; the shim then execs the real `gh` with the unchanged argv.
- `AllowWithRewrite { updated_command: Some(cmd) }` — the inline `--body "..."` case. A shim _can_ rewrite argv before delegating (more easily than a hook can, in fact). So this works too.

So editorial enforcement is **fully preserved** under a `PATH` shim — better than under Codex hooks, where `updatedInput` is unreachable. The residual risk is that a worker invokes the GitHub API without going through `gh` (raw `curl`, or a language binding). That risk exists identically today with the hook, so it is not a regression; Boss's repo conventions already mandate `cube pr create`.

**Net: no guardrail requires refusing Codex, and none is silently degraded.** The `KindRequirements` escalation mechanism stays unused for guardrail reasons in v1 — but see [Codex-eligible kinds](#which-work-item-kinds-are-codex-eligible) for kinds refused on _other_ grounds.

**Hard sequencing constraint:** the `PATH` shims must land and be verified on the **Claude** path before any Codex worker runs, because they are the only guardrail carrier for Codex. Shipping the Codex driver first would mean shipping it unguarded. This ordering is reflected in the task graph.

---

## Alternatives considered

### Alternative 1: Replicate the Claude architecture — make Codex emit hook callbacks

Configure Codex hooks to invoke the existing `boss-event` shim, reusing the unix socket, `events_socket.rs`, and the whole ingress path unchanged. Codex's hook payloads are Claude-wire-compatible, so `normalize_progress_event` would need only light adaptation. Zero new engine machinery.

**Rejected — and the 0.145.0 delta strengthens the rejection rather than weakening it.** This was originally rejected partly because no hook could be made to fire. Hooks now demonstrably do fire ([D-2](#deltas-that-change-the-design)), so that objection is gone — but the decisive one remains and is now better evidenced: an untrusted or misconfigured hook is skipped in **complete silence**. Routing progress ingress through that mechanism would mean a worker that silently reports nothing and never completes, with no signal to distinguish it from a hung agent. It is the wrong dependency for the same underlying reason it was before: it makes Boss's progress ingress contingent on the most fragile, least-observable part of Codex's surface, when Codex is _already_ handing Boss a typed, in-band, structural event stream on stdout that requires no installation, no trust record, and cannot be silently skipped. It would also entrench the assumption that progress arrives via callback, which is exactly the abstraction gap ([G-5](#g-5-progressobservation--the-top-gap)) this project exists to surface.

### Alternative 2: Post-hoc-only guardrails via the existing `Degrade` path

Declare `ToolUseInterception` absent, land on `AbsenceDisposition::Degrade`, and implement the already-typed `PostHocInterceptionFn` / `PostHocInterceptionAction` (`engine/driver/src/lib.rs:497-525`) to check after the fact — scan the transcript for a push that should not have happened, then flag or revert.

**Rejected.** Post-hoc detection of an _already-pushed_ commit or an _already-posted_ GitHub comment is not enforcement; the side effect is public. For editorial controls specifically the whole point is that unreviewed prose never reaches GitHub. The `PATH`-shim approach gets genuine pre-execution enforcement for the same effort, and works for both drivers. The post-hoc types should still be implemented — they are the right answer for a _future_ driver with neither hooks nor a shimmable command surface — but they are not the answer here, and leaving them as uncalled types while a driver silently degrades onto them is the actual bug.

### Alternative 3: Drive `codex app-server` over JSON-RPC

Instead of `codex exec`, run the persistent `codex app-server` and drive it over its JSON-RPC protocol (`thread/start`, `turn/start`, `turn/steer`, `thread/resume`, `thread/interrupt`, plus `TurnCompletedNotification`, `ItemStartedNotification`, `HookRunSummary` — all present in the binary).

**Rejected for v1, strong candidate for v2.** It is a much richer surface: real steering of a _live_ turn (fixing the probe/nudge mismatch in [G-7](#g-7-turnboundary) properly rather than via process restart), structured interrupts, and explicit hook-run reporting. But it is marked `[experimental]` in `codex --help`, it is a fundamentally different execution model from Boss's "agent CLI in a ghostty pane" (which the P1422 design explicitly holds fixed as a non-goal), and it would front-load a large protocol client before the basic driver works. Filed as a deferred task so the option stays visible.

---

## Chosen approach

Drive `codex exec --json` as a pane-embedded worker, with **stdout JSONL as the progress transport**, `--output-last-message` + the existing `BOSS_STRUCTURED_OUTPUT` file contract for structured results, per-worker `CODEX_HOME` for isolation, Codex's OS sandbox for filesystem guardrails, and **`PATH` shims for command guardrails**.

### Execution shape

```
CODEX_HOME=<run-dir>/codex-home \
  codex exec --json \
    --sandbox workspace-write \
    -C <workspace> \
    -m <model> \
    -c model_reasoning_effort=<resolved-per-model> \
    -o <run-dir>/last-message.txt \
    "$(cat AGENTS-initial-prompt.txt)" \
    < /dev/null
```

**No `--ask-for-approval`.** The flag was removed from `codex exec` between 0.137.0 and 0.145.0 and now produces a hard argument error; headless exec runs `approval_policy=Never` unconditionally, which is exactly the property Boss needed (any other policy can block a headless worker forever waiting for an approval nobody will give). This is the one place where the version delta would have broken the design outright rather than merely dating it — see [D-1](#deltas-that-change-the-design).

Sandbox stays `workspace-write` (not `danger-full-access`) — that is what makes the Boss-data-dir guard structural. `model_reasoning_effort` is resolved against the selected model's `supported_reasoning_levels` rather than assumed, since the ladder is per-model and now reaches `ultra` on some models.

`--strict-config` is worth adding once Boss's Codex config generation is stable: it turns an unrecognised config key into a startup error instead of a silently ignored setting, which is a cheap guard against exactly the config drift observed across these two versions.

### The four engine seams this needs

1. **A stdout JSONL progress reader.** New. Feeds the same normaliser and the same activity machine as the socket path. This is the `ProgressIngress::StdoutJsonl` arm from [G-5](#g-5-progressobservation--the-top-gap). PR-URL capture rides on it, scanning `command_execution` items' `aggregated_output`.
2. **A `TurnBoundary` trait method.** `turn.completed` → `WorkerEvent::Stop`, so `completion/stop.rs` stops being hardwired to a Claude hook.
3. **Driver-supplied transcript path discovery**, replacing hook-stamped `transcript_path`, plus actually calling `normalize_transcript_entry`.
4. **`PATH`-shim guardrails**, replacing `PreToolUse` guard scripts, landed on the Claude path first.

### Capability declaration for `CodexDriver` (v1)

Provided: `Spawn`, `WorkspaceProvisioning`, `PermissionPolicy`, `ModelAndEffortMenu`, `ProgressObservation`, `TurnBoundary`, `StructuredOutput`, `TranscriptAccess`, `ControlVerbs`, `PromptComposition`.

Not provided: `ToolUseInterception` (→ `Degrade`, guardrails carried by `PATH` shims), `ToolProvisioning` (→ `Degrade`, unused for every driver).

Hooks firing on 0.145.0 does **not** by itself upgrade `ToolUseInterception`. The upgrade condition is not "do hooks fire" — that is now answered — but **"can Boss provision hook trust deterministically, and detect when a hook did not run"**. Until [T-01](#t-01-codex-hook-trust-provisioning) settles that, a declared capability would be a promise Boss cannot keep, because an untrusted hook is silently skipped. If T-01 succeeds, `ToolUseInterception` upgrades to provided-with-deny-only and hooks become defence-in-depth alongside the shims; nothing above needs revisiting either way.

### Which work-item kinds are Codex-eligible

Phased, with an acceptance criterion per phase. Refusals here are expressed through `KindRequirements`, and they are about **output-contract maturity**, not guardrails — guardrails are handled uniformly by the shims.

**Phase 1 — chores and project tasks.** The plain "make a change, open a PR" loop. Acceptance: 10 consecutive chores dispatched `--driver codex` reach an open PR with green CI, no engine intervention, and the PR URL captured on the primary path (not a `jj log` reconstruction fallback).

**Phase 2 — design, investigation, postmortem.** These are document-producing kinds and depend on the `BOSS_STRUCTURED_OUTPUT` file contract (T1476) plus followups parsing. Acceptance: a Codex-authored design doc lands with a correctly parsed `Proposed implementation task breakdown`, and its followups materialise.

**Phase 3 — review and conflict resolution.** Review needs `--sandbox read-only` to be verified as a real reviewer-read-only equivalent, and structured `ReviewResult` output. Conflict resolution needs write access plus the merge-conflict telemetry path. Acceptance: a Codex reviewer produces a structured `ReviewResult` on a real PR that a human agrees with, and demonstrably cannot write to the workspace.

**Deferred indefinitely — triage and answer-agent.** Not because of guardrails but because both are **transcript-scraped**: `parse_triage_decision` (`engine/core/src/automation_triage.rs:498`) reads the final assistant message, and the answer agent depends on `UserPromptSubmit`-based delivery confirmation (`engine/core/app/pane_delivery.rs`) that Codex does not have. Ironically Codex's `--output-schema` would make triage _more_ reliable than Claude's — but that is a rewrite of the triage contract, not a driver task. Refuse via `KindRequirements` until then.

### Load-balancing seams

Design _for_, do not design _now_. Three seams, with attachment points:

1. **Per-driver capacity accounting.** Slots are one global pool today. The seam is the dispatch gate at `engine/core/src/runner/worker_spawn.rs:597` — it already resolves `(kind, driver)` and is the natural place for an in-flight count keyed by driver slug. Requirement on this project: **do not add a second, driver-blind admission path.** The stdout-reader work must not spawn workers outside this gate.
2. **Per-provider rate-limit state.** Codex hands this over for free: `turn.completed` carries `input_tokens`, `cached_input_tokens`, `cache_write_input_tokens`, `output_tokens`, `reasoning_output_tokens` (verified in the capture above), and the binary carries `RateLimitSnapshot` / `RateLimitWindow` types. The seam is the progress reader — it should record per-turn usage against the driver rather than discarding it. **Treat the usage field set as open:** `cache_write_input_tokens` was added between 0.137.0 and 0.145.0 with no wire signal ([D-4](#stream-drift--all-silent-all-additive)), so a balancer that destructures a fixed set of counters will break on the next upgrade. Claude has no equivalent in-band signal, which is itself worth knowing before a balancer assumes symmetry.
3. **Capability-aware routing.** `CapabilityResolver::check_dispatch` already computes exactly the predicate a balancer needs ("can driver D run kind K"). It must stay a **pure, side-effect-free query** so a balancer can call it speculatively across candidate drivers before choosing. Requirement on this project: do not make `check_dispatch` mutate state or log dispatch decisions as a side effect.

### Migration and coexistence

- **Per-host auth.** `CODEX_HOME/auth.json`, symlinked from a host-level credential. No env-var collision with Claude. `unset ANTHROPIC_API_KEY` becomes driver-supplied ([G-1](#g-1-spawn)).
- **Config collisions.** Solved by per-worker `CODEX_HOME`. Without it, concurrent workers race on `~/.codex/config.toml`'s project-trust registry.
- **Workspace layout.** Codex uses `AGENTS.md` and `.codex/`; Claude uses `CLAUDE.md` and `.claude/`. They do not collide _by name_ — but Codex's `external_agent_config.detect` actively looks for `.claude/settings.json`, `CLAUDE.md`, and `hooks.json` and offers to import them. The Codex driver must disable that import (`external_config_migration_prompts`). A workspace that has run both drivers will contain both `.claude/` and `.codex/`; both must be engine-gitignored.
- **A second import vector, new in 0.145.0.** Alongside config import, 0.145.0 adds an `external_agent_memory_import` feature (currently _under development_, default off) and an `external_agent_config_imports` table in `state_5.sqlite`. Suppressing config-migration prompts is therefore not a one-time fix — the surface is growing, and the Codex driver should assert its intended import posture explicitly rather than relying on a single flag's default. Per-worker `CODEX_HOME` limits the blast radius, since the import bookkeeping lives in the run's own state DB.
- **Cube.** Nothing in `cube`'s workspace provisioning assumes an agent — it manages jj workspaces, leases, and PRs. `cube pr create` is agent-neutral and is, usefully, the enforcement point the [`PATH`-shim design](#guardrail-integrity) leans on. **No cube changes required**, which is a genuinely good outcome and worth stating explicitly.

---

## Risks / open questions

<a id="oq-1-hook-trust-provisioning"></a>
**OQ-1 — How does Boss provision Codex hook trust, and detect a hook that did not run?** The original form of this question ("do hooks fire under `codex exec`?") is **answered: they do, on 0.145.0**, and `PreToolUse` deny genuinely blocks. What replaces it is narrower and more operational. Hooks run only when trusted, via `--dangerously-bypass-hook-trust` or a persisted `[hooks] trusted_hash`; an untrusted hook is skipped in complete silence, as is a hook whose command is missing. The bypass flag is not an acceptable default because it would also trust project-local `.codex/` hooks originating in the repository under work. So: what is `trusted_hash` computed over, can Boss stamp it deterministically when it regenerates worker config, and is there any observable signal that a configured hook did not fire? Until that last part has an answer, hooks cannot carry a guardrail. → [T-01](#t-01-codex-hook-trust-provisioning).

<a id="oq-2"></a>
**OQ-2 — Version pinning and churn. Now evidenced rather than precautionary.** The `--json` stream still carries **no schema version**, and re-running this analysis across 0.137.0 → 0.145.0 produced four concrete breaks in eight minor versions: a removed flag that would have made the prescribed launch command fail (`-a`), an added `usage` field, a changed item-ID base, a second meaning for `error` items, plus four new `TurnItem` variants and a new hook event. None of it was announced on the wire. This is no longer a hypothetical risk — it is the observed release cadence. Recommendation firms up accordingly: **pin the tested version, add `--strict-config` for the config half, and gate upgrades on the conformance harness (T1483 / [T-22](#t-22-extend-the-reference-driver-conformance-harness-a-12-amends-t1483))**. Note `--strict-config` covers config keys only; nothing validates the event stream, so the harness remains the sole defence there. "Pin the agent CLI version" is still a policy decision with operational cost, and still the operator's call.

<a id="oq-3-what-is-the-codex-rules-execpolicy-format"></a>
**OQ-3 — What is the Codex `.rules` execpolicy format?** On 0.145.0 `--ignore-rules` is a **documented** `codex exec` flag (_"Do not load user or project execpolicy `.rules` files"_) rather than the binary-string inference it was on 0.137.0, which raises confidence that the system is real and reachable. It might restore some per-command deny fidelity natively, reducing reliance on `PATH` shims. Still unexamined — I did not want to design against a surface I had not run.

**OQ-4 — Rollout disk growth.** `~/.codex` on this host holds 279 active + 241 archived rollouts at ~865 MB. Per-worker `CODEX_HOME` multiplies this across workspaces. `--ephemeral` avoids it entirely but would forfeit `TranscriptAccess`. Needs a retention policy; not a v1 blocker.

**OQ-5 — `codex exec` is one turn per process.** Claude's probe/nudge injects into a live session; Codex requires `codex exec resume`, a new process. I believe this is tractable ([T-17](#t-17-controlverbs-on-the-trait-plus-codex-probenudge-via-exec-resume-a-7)) but it is the least-validated part of this design — I did not spike resume-based probing, and pane lifecycle across a process restart is exactly where surprises live.

<a id="oq-6-codex-exec-review"></a>
**OQ-6 — Is `codex exec review` a better substrate for Boss's review kind than a plain read-only exec run?** New in this pass ([D-3](#delta-that-changes-a-tasks-scope)). It is purpose-built, takes `--base` / `--commit` / `--uncommitted`, and has a dedicated `codex-auto-review` model. It may also impose its own output shape that does not match Boss's `ReviewResult`. Unexamined; folded into [T-25](#t-25-codex-eligibility-for-review-and-conflict-resolution-kinds).

**Risk — the `PATH`-shim relocation is a change to the Claude path.** It touches live guardrails on the driver that runs everything today. It is a net improvement (it closes the subshell-evasion hole) but it is not risk-free, and it lands before any Codex value is visible. I think this ordering is correct and non-negotiable — shipping Codex first means shipping it unguarded — but it is worth a human agreeing before [T-02](#t-02-relocate-command-guardrails-to-path-shims-claude-path) starts.

---

## Proposed P1422 amendments

Discrete, filed-work-item-sized. Boss work items **cannot** be created from this session — I have no taxonomy access and have filed nothing. This section is the handoff; the coordinator files from it.

| #    | Proposed name                                                                        | Effort    | Amends / new                                                                      | Brief                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| ---- | ------------------------------------------------------------------------------------ | --------- | --------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A-1  | `ProgressObservation`: abstract the ingress transport, not just normalisation        | `large`   | **Amends T3328** (materially widens it)                                           | T3328 as scoped only stops the `.expect()` panic at `worker_setup.rs:530-532`, leaving a hookless driver with no progress signal at all. `ProgressObservationWiring` must become an enum over transports — `HookCallback(wiring)` for Claude, `StdoutJsonl` for Codex — with an engine-side stdout reader feeding the same normaliser and activity machine. `normalize_progress_event` needs no signature change; `events_socket.rs:248`'s hardcoded `ClaudeDriver` does. Without this, no Codex worker reports progress or completes. |
| A-2  | `PermissionPolicy`: return permission _artifacts_, not a single file path            | `medium`  | **Amends T1479**                                                                  | `write_permission_config(&self, dest_dir) -> Result<PathBuf>` (`lib.rs:573`) hardcodes "a permission policy is one file whose path is passed to the CLI". Codex's policy is `--sandbox`, `--ignore-rules` / `--strict-config`, `[sandbox_workspace_write]`, and `CODEX_HOME` — argv plus config. Return `PermissionArtifacts { config_files, extra_args, env }`. Landing T1479 against the current signature guarantees a second refactor.                                                                                             |
| A-3  | `Spawn`: replace Claude-shaped parameters with a `SpawnRequest`/`SpawnPlan` pair     | `medium`  | **New**                                                                           | `spawn_invocation` (`lib.rs:553-560`) takes `settings_path`, `non_opus_auto_mode`, `permission_mode_override` — all Claude concepts — and returns a `String` the engine wraps with a hardcoded `unset ANTHROPIC_API_KEY` (`pane_spawn.rs:382`). Codex needs an exported `CODEX_HOME`. Take an opaque request struct; return `SpawnPlan { env, command }` so env mutation is driver-supplied.                                                                                                                                           |
| A-4  | `TurnBoundary` trait method — decouple completion from `WorkerEvent::Stop`           | `medium`  | **Amends T3325** (re-scopes; drops the synthesizer from the critical path)        | T3325 pairs a trait method with an engine synthesizer. Codex needs only the method: `turn.completed` is native and maps directly to `WorkerEvent::Stop`, so `completion/stop.rs:12,168` stops being hardwired to a Claude hook. The synthesizer remains worth building for a driver with neither hooks nor turn events, but it should not gate Codex. Re-scope T3325 to the trait method; split the synthesizer out.                                                                                                                   |
| A-5  | `StructuredOutput` trait method + driver-supplied PR-URL extraction                  | `medium`  | **Amends T1476** (adds PR-URL; T1476's own scope is sufficient as far as it goes) | `StructuredOutput` (`lib.rs:43`) has no trait method. More urgently, PR-URL capture is derived from `PostToolUse` hook events (`pr_url_capture.rs:1-6`) and is out of T1476's scope — under Codex it breaks completely, and the PR URL is the acceptance criterion for nearly every work item. Codex's `command_execution` items carry `aggregated_output`, so the same regex works against the stream. Make extraction driver-supplied. Also surface `--output-schema`, which is a stronger contract than the env-var file.           |
| A-6  | `TranscriptAccess`: driver-supplied path discovery, and actually call the normaliser | `small`   | **New**                                                                           | `normalize_transcript_entry` (`claude.rs:606-616`) has never been called — `live_status.rs` passes raw JSONL to redaction. Path discovery depends on Claude stamping `transcript_path` on hook payloads; Codex's stream has no such field, though the path is derivable from `thread_id` + `CODEX_HOME`. Add `transcript_path_for_session()` and wire the normaliser in.                                                                                                                                                               |
| A-7  | `ControlVerbs`: put probe/interrupt/stop/reap on the trait and call `classify_error` | `medium`  | **New**                                                                           | The trait has only `classify_error` (`lib.rs:644`) and it is never called — `transient_recovery.rs` calls `classify_claude_error` directly. probe/interrupt/stop/reap are absent entirely, yet probe is precisely where Claude and Codex diverge (live-session message vs `codex exec resume`). Error classification is provider-specific and must not route through Claude's classifier.                                                                                                                                              |
| A-8  | Implement the post-hoc interception degrade path                                     | `medium`  | **New**                                                                           | `PostHocInterceptionFn` / `PostHocInterceptionAction` (`lib.rs:497-525`) have zero engine callers, so any driver landing on `AbsenceDisposition::Degrade` for `ToolUseInterception` **silently loses every guardrail**. This project routes guardrails through `PATH` shims instead, so it is not a Codex blocker — but leaving a live silent-degrade path in the abstraction is a latent safety bug that the next driver will fall into.                                                                                              |
| A-9  | Widen `WorkerEvent` session identity and `SessionStartSource`                        | `small`   | **New**                                                                           | `WorkerEvent` requires `session_id` on every variant (`protocol/src/worker_event.rs`) and `SessionStartSource` mirrors Claude's `startup\|resume\|compact`. Codex's identity is `thread_id` and its trigger set is `startup\|resume\|clear\|compact` — a superset. Note the trap: Codex's _hooks_ say `session_id` while its _stream_ says `thread_id`.                                                                                                                                                                                |
| A-10 | `PromptComposition`: driver-supplied enforcement wording                             | `small`   | **New**                                                                           | `worker_setup.rs:309,372` tell the worker _"A PreToolUse hook blocks these"_. For a Codex worker this asserts a guarantee that is false. The sentence must come from the driver. Cheap, and it is a correctness issue in what Boss tells a worker, not a wording nit.                                                                                                                                                                                                                                                                  |
| A-11 | Resolve or delete `progress_fidelity()`                                              | `trivial` | **New**                                                                           | `claude.rs:482` — re-verified to have no callers anywhere. Either the fidelity tiers mean something and the engine should consult them, or the method should go. A Codex driver would declare `Rich` into a void.                                                                                                                                                                                                                                                                                                                      |
| A-12 | Extend T1483's conformance harness to cover transport and turn boundaries            | `medium`  | **Amends T1483**                                                                  | T1483 (blocked on T1476 + T1479) was scoped against a Claude-shaped driver. It must also assert: stdout-JSONL ingress produces the same `WorkerEvent` sequence as hook ingress; a turn boundary drives completion identically from either source; and a pinned agent-CLI version is verified, given Codex's unversioned stream ([OQ-2](#oq-2)).                                                                                                                                                                                        |

**Verdict on the existing tasks, as required by the brief:** T3324 (cut over every call site) — **sufficient and correctly scoped**; re-verified as still open, with `ClaudeDriver` hardcoded at `worker_setup.rs:66,516,536`, `spawn_flow.rs:27,238`, `events_socket.rs:19,248`, `pane_spawn.rs:307,383,943`, against a registry consulted at exactly one place (`runner/worker_spawn.rs:597-601`). T3326 — **sufficient**, if it also renames `claude_effort`. T1476 — **sufficient for what it covers**, insufficient for PR-URL (A-5). T1479 — **insufficient** (A-2). T3325 — **mis-scoped/mis-prioritised** (A-4). T3328 — **materially insufficient** (A-1). T1483 — **insufficient** (A-12).

---

## Appendix A: reproducing the Codex spike

```sh
export CH=/tmp/codex-spike/ch REPO=/tmp/codex-spike/repo
mkdir -p "$CH" "$REPO" && git -C "$REPO" init -q
ln -sf ~/.codex/auth.json "$CH/auth.json"      # symlink, don't copy the credential
printf 'model = "gpt-5.5"\nmodel_reasoning_effort = "low"\n' > "$CH/config.toml"

CODEX_HOME="$CH" codex exec --json --sandbox workspace-write -C "$REPO" \
  "Run the shell command 'echo probe' and report its output." < /dev/null
```

Confirms: JSONL envelopes, `thread.started` / `turn.started` / `turn.completed`, `command_execution` items with `aggregated_output`, per-turn `usage`, and full `CODEX_HOME` isolation (the spike home grows its own `sessions/`, `state_5.sqlite`, `skills/`, `plugins/`).

### Hooks: firing, and failing silently

A handler that logs its stdin, wired to three events:

```toml
[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = "/path/to/hooklog.sh"
timeout = 10

[[hooks.PreToolUse]]
matcher = ".*"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "/path/to/hooklog.sh"
timeout = 10
```

Run with `--dangerously-bypass-hook-trust` on **0.145.0** and all three events deliver Claude-shaped payloads. Run the **same** config **without** the flag and the hooks are skipped with no warning of any kind — the single most important observation in this appendix.

To reproduce the deny path, have the handler emit:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "permissionDecisionReason": "BOSS GUARDRAIL: this command is blocked by policy."
  }
}
```

The command is blocked before execution and the reason reaches the model.

The original 0.137.0 control still reproduces as a **silent** failure on 0.145.0 — swap the handler for `command = "/definitely/not/a/real/binary-xyz"` and the turn completes with no error, no warning, and no stream event. Together these two silences ([OQ-1](#oq-1-hook-trust-provisioning)) are why hooks do not carry guardrails in this design.

### Version-delta harness

To re-run this analysis against a future version, the checks that actually moved between 0.137.0 and 0.145.0 are worth running first:

```sh
codex exec --help | grep -c ask-for-approval        # 0 on 0.145.0 — flag removed
codex debug models | jq '.models[] | {slug, supported_reasoning_levels}'
codex features list | grep -E 'hooks|plugins'
CODEX_HOME="$CH" codex exec --json … | jq -c 'select(.type=="turn.completed") | .usage'
```

and diff the `TurnItem` / hook-event enums out of the binary's embedded schemas.

---

## Proposed implementation task breakdown

Dependency-ordered. Effort hints are per-entry, each sized to one reviewable PR by one worker in one session.

### T-01 Codex hook trust provisioning

**Re-scoped by the 0.145.0 delta pass.** The original question — do hooks fire under `codex exec`? — is answered: they do, and `PreToolUse` deny blocks pre-execution ([D-2](#deltas-that-change-the-design)). What remains is trust provisioning. Determine what `[hooks] trusted_hash` is computed over (`HookStateToml`), whether Boss can stamp it deterministically when it regenerates worker config each run, and — the part that actually gates the capability — whether there is **any** observable signal distinguishing "hook ran and allowed" from "hook was silently skipped". Also assess the blast radius of `--dangerously-bypass-hook-trust`, which trusts project-local `.codex/` hooks from the repo under work. Check `$CODEX_HOME/hook_outputs` and the binary's `hook_started` / `hook_completed` / `hook_denied` / `hook_run_id` telemetry vocabulary as candidate signals. Output is a written finding plus a reproducible harness, not code. Gates whether `ToolUseInterception` can be declared for Codex; does not gate the rest of v1, which does not rely on hooks.

- **Effort:** `small`
- **Depends on:** none
- **Scope:** in-scope

### T-02 Relocate command guardrails to `PATH` shims (Claude path)

Move the checkleft push guard, revision-PR guard, and direct-push blocks out of the `PreToolUse` guard scripts (`worker_setup.rs:918`, `:1072`) into executables in `BOSS_BIN_DIR` that evaluate the invocation and delegate to the real binary. Claude path only; behaviour-preserving from the worker's perspective. This also closes the subshell-evasion hole the hook has today. Must land and be verified before any Codex worker runs, because it is the sole guardrail carrier for Codex.

- **Effort:** `large`
- **Depends on:** none
- **Scope:** in-scope

### T-03 Relocate editorial enforcement to a `gh` `PATH` shim (Claude path)

Move `editorial_hook.rs` evaluation from the `PreToolUse` hook to a `gh` shim, preserving all three `PreToolUseDecision` outcomes including inline `--body` argv rewriting. Separate PR from T-02: different subsystem (`boss-editorial` + audit log), different risk profile, and T-02's shims are the prerequisite mechanism.

- **Effort:** `medium`
- **Depends on:** T-02
- **Scope:** in-scope

### T-04 `ProgressObservation` transport abstraction (P1422 amendment A-1)

Turn `ProgressObservationWiring` into an enum over ingress transports and remove the hardcoded `ClaudeDriver.normalize_progress_event` at `events_socket.rs:248`. Claude keeps the hook-callback arm with identical behaviour; the stdout arm is defined but has no consumer yet. Trait-and-plumbing only — the reader itself is T-05.

- **Effort:** `large`
- **Depends on:** none
- **Scope:** in-scope

### T-05 Engine-side stdout JSONL progress reader

Implement the `StdoutJsonl` ingress: read the worker process's stdout, parse JSONL envelopes, feed the driver's normaliser, drive the existing activity machine. No Codex-specific parsing here — that is the driver's normaliser. Second phase of A-1, split out because T-04 is a trait/plumbing change and this is new runtime machinery.

- **Effort:** `large`
- **Depends on:** T-04
- **Scope:** in-scope

### T-06 `TurnBoundary` trait method (P1422 amendment A-4, re-scopes T3325)

Add the trait method and route `completion/stop.rs` through it instead of directly off `WorkerEvent::Stop`. Claude's implementation returns its existing hook-derived boundary; behaviour unchanged. Excludes the engine synthesizer, which is split to T-18.

- **Effort:** `medium`
- **Depends on:** T-04
- **Scope:** in-scope

### T-07 `Spawn` signature: `SpawnRequest` / `SpawnPlan` (P1422 amendment A-3)

Replace the Claude-shaped positional parameters with an opaque request struct and a structured plan carrying driver-supplied env, moving `unset ANTHROPIC_API_KEY` out of `pane_spawn.rs:382` and behind `ClaudeDriver`. Touches `pane_spawn.rs` and its spawn-line assertions at `:868,870,942`.

- **Effort:** `medium`
- **Depends on:** none
- **Scope:** in-scope

### T-08 `PermissionPolicy` artifacts signature (P1422 amendment A-2, amends T1479)

Change `write_permission_config` to return `PermissionArtifacts { config_files, extra_args, env }` and land T1479's extraction of the Claude permission logic against that shape in one step, so the extraction is not done twice. Removes the `unimplemented!()` at `claude.rs:477-480`.

- **Effort:** `large`
- **Depends on:** T-07
- **Scope:** in-scope

### T-09 Resolve driver at every call site (existing T3324)

The cutover: replace every hardcoded `ClaudeDriver` construction with a registry resolution. Confirmed still open and unchanged in scope. Listed here as an explicit dependency edge because a Codex driver cannot be exercised until it lands, and it is far easier once the signature churn (T-06, T-07, T-08) has settled.

- **Effort:** `large`
- **Depends on:** T-06, T-07, T-08
- **Scope:** in-scope

### T-10 `CodexDriver` skeleton: descriptor, capabilities, model menu

The crate and struct: `DriverDescriptor` (`AGENTS.md`, `.codex`), `CapabilitySet` per this design, and a `ModelMenu` sourced from `codex debug models`. Includes fixing `menu_for_driver` to honour its slug and renaming `claude_effort` (existing T3326). No spawning yet.

- **Effort:** `medium`
- **Depends on:** T-09
- **Scope:** in-scope

### T-11 `CodexDriver` spawn and workspace provisioning

Implement `spawn_invocation` (the `codex exec --json` line, including `< /dev/null`) and `provision_workspace` (per-run `CODEX_HOME`, `auth.json` symlink, `AGENTS.md`, pre-stamped project trust, `external_config_migration_prompts` disabled). Produces a Codex worker that starts, but whose progress is not yet observed.

- **Effort:** `large`
- **Depends on:** T-10
- **Scope:** in-scope

### T-12 `CodexDriver` progress normaliser

Map Codex's stream envelopes onto `WorkerEvent`: `thread.started`, `turn.started`, `turn.completed`, `item.started` / `item.completed` across the `TurnItem` variants. Consumes T-05's transport and T-06's turn boundary. This is where a Codex worker first becomes observable end-to-end.

Three constraints from the 0.145.0 delta pass, each a real trap: item IDs are **0-based** and must not be treated as ordinal or 1-based; `item.completed` with `type:"error"` carries **operational warnings as well as** turn failures, so it must not be mapped unconditionally to a failed turn; and the `TurnItem` enum grew by four variants across eight minor versions, so unknown variants must be ignored-with-logging rather than rejected.

- **Effort:** `large`
- **Depends on:** T-05, T-06, T-11
- **Scope:** in-scope

### T-13 Widen `WorkerEvent` session identity and `SessionStartSource` (A-9)

Accommodate Codex's `thread_id` and its `startup|resume|clear|compact` trigger set. Small and mechanical, but it touches `boss-protocol` and therefore every consumer, so it is its own PR. **File overlap:** co-edits the driver normalisers with T-12 — land T-12 first, and forward-port its mappings preservingly.

- **Effort:** `small`
- **Depends on:** T-12
- **Scope:** in-scope

### T-14 Driver-supplied PR-URL extraction (A-5)

Make PR-URL capture driver-supplied rather than `PostToolUse`-derived (`pr_url_capture.rs`), with Codex scanning `command_execution` items' `aggregated_output`. Without this a Codex work item can never satisfy its acceptance criterion. Separate from T-12 because it changes an engine-side capture contract, not the normaliser.

- **Effort:** `medium`
- **Depends on:** T-12
- **Scope:** in-scope

### T-15 `StructuredOutput` trait method and `--output-schema` wiring (A-5)

Put `StructuredOutput` on the trait and have the Codex driver use `--output-schema` / `--output-last-message` alongside the shared `BOSS_STRUCTURED_OUTPUT` file contract. Depends on T1476 landing the file contract first.

- **Effort:** `medium`
- **Depends on:** T-14
- **Scope:** in-scope

### T-16 `TranscriptAccess`: driver-supplied path discovery (A-6)

Add `transcript_path_for_session()`, derive Codex's rollout path from `thread_id` + `CODEX_HOME`, and actually call `normalize_transcript_entry` in `live_status.rs` — it has never been called. Generalise `engine/transcript-tail` beyond its "claude transcript files" framing.

- **Effort:** `medium`
- **Depends on:** T-12
- **Scope:** in-scope

### T-17 `ControlVerbs` on the trait, plus Codex probe/nudge via `exec resume` (A-7)

Put probe/interrupt/stop/reap on the trait, route `transient_recovery.rs` through `classify_error` instead of `classify_claude_error`, and implement Codex probing as `codex exec resume` with delivery confirmed by observing a new `turn.started`. This is the least-validated area of the design ([OQ-5](#risks--open-questions)) and may need its own spike.

- **Effort:** `large`
- **Depends on:** T-12
- **Scope:** in-scope

### T-18 `TurnBoundary` engine synthesizer (remainder of T3325)

The synthesize-from-a-lower-fidelity-channel path, for a future driver with neither hooks nor native turn events. Split out of T3325 because Codex does not need it and it should not gate Phase 1.

- **Effort:** `medium`
- **Depends on:** T-06
- **Scope:** deferred (future / not a v1 blocker) — Codex has native turn events; needed only for a third driver with neither hooks nor turn boundaries

### T-19 Implement the post-hoc interception degrade path (A-8)

Wire `PostHocInterceptionFn` / `PostHocInterceptionAction`, which currently have zero callers, so a driver landing on `Degrade` does not silently lose guardrails. Not a Codex blocker under the `PATH`-shim design, but it removes a latent safety bug from the abstraction.

- **Effort:** `medium`
- **Depends on:** T-03
- **Scope:** deferred (future / not a v1 blocker) — `PATH` shims carry Codex's guardrails; this closes the trap for the next driver

### T-20 Driver-supplied enforcement wording in prompts (A-10)

Replace the hardcoded _"A PreToolUse hook blocks these"_ at `worker_setup.rs:309,372` with driver-supplied wording, so Boss does not assert a false guarantee to a Codex worker. Must be re-checked after T-02/T-03, since the shims change what is true for Claude too.

- **Effort:** `small`
- **Depends on:** T-03, T-10
- **Scope:** in-scope

### T-21 Resolve or delete `progress_fidelity()` (A-11)

`claude.rs:482` has no callers. Either consult the fidelity tiers somewhere real or remove the method.

- **Effort:** `trivial`
- **Depends on:** T-12
- **Scope:** in-scope

### T-22 Extend the reference-driver conformance harness (A-12, amends T1483)

Add cross-transport conformance: assert stdout-JSONL and hook ingress produce identical `WorkerEvent` sequences, that a turn boundary drives completion identically from either source, and that the pinned agent-CLI version is verified. This is a validation campaign over the implementations above and is deliberately sequenced after them.

The 0.137.0 → 0.145.0 delta pass gives this task its concrete regression set — every one of these actually happened, unannounced, in eight minor versions: a removed CLI flag, an added `usage` counter, a changed item-ID base, a widened meaning for `error` items, four new `TurnItem` variants, and a new hook event. The harness should assert on the flags it depends on, tolerate additive stream fields and unknown enum variants, and fail loudly on removals. Add `--strict-config` to catch the config half at startup; nothing validates the event stream, so this harness is the only defence there.

- **Effort:** `large`
- **Depends on:** T-12, T-14, T-15, T-16, T-17
- **Scope:** in-scope

### T-23 Phase-1 acceptance sweep: 10 Codex chores to green PRs

Dispatch 10 consecutive chores with `--driver codex` and verify each reaches an open PR with green CI, no engine intervention, and primary-path PR-URL capture. A sweep, not an implementation — listed separately and after the work it validates.

- **Effort:** `medium`
- **Depends on:** T-22
- **Scope:** in-scope

### T-24 Codex eligibility for design / investigation / postmortem kinds

Phase 2: enable the document-producing kinds via `KindRequirements` once the structured-output contract is proven, and verify a Codex-authored design doc's task breakdown parses and materialises followups.

- **Effort:** `medium`
- **Depends on:** T-23
- **Scope:** in-scope

### T-25 Codex eligibility for review and conflict-resolution kinds

Phase 3: verify `--sandbox read-only` is a genuine reviewer-read-only equivalent (including that the worker demonstrably cannot write), and that structured `ReviewResult` output round-trips. **Additionally evaluate `codex exec review`** — a native non-interactive review mode found in the 0.145.0 pass, with `--base` / `--commit` / `--uncommitted` and a dedicated `codex-auto-review` model ([D-3](#delta-that-changes-a-tasks-scope), [OQ-6](#oq-6-codex-exec-review)). It may fit Boss's review kind better than a general exec run, or may impose an output shape that does not match `ReviewResult`; decide between the two rather than defaulting to the general path.

- **Effort:** `medium`
- **Depends on:** T-24
- **Scope:** in-scope

### T-26 Per-driver capacity and rate-limit accounting seams

Attach per-driver in-flight accounting at the dispatch gate and record Codex's per-turn `usage` from `turn.completed` as per-provider rate-limit state. Seams only — no routing policy, which is the balancer's separate project.

- **Effort:** `medium`
- **Depends on:** T-23
- **Scope:** deferred (future / not a v1 blocker) — load balancing is explicitly out of scope; this only ensures the seams exist

### T-27 Codex `.rules` execpolicy investigation

Investigate Codex's execpolicy `.rules` system ([OQ-3](#oq-3-what-is-the-codex-rules-execpolicy-format)) to see whether it restores native per-command deny fidelity and could reduce reliance on `PATH` shims. Discovery task, sequenced independently.

- **Effort:** `small`
- **Depends on:** none
- **Scope:** deferred (future / not a v1 blocker) — `PATH` shims already cover the requirement; this is a potential simplification

### T-28 Remote/SSH dispatch for Codex

`engine/core/remote/boss-remote-run.sh:84,159,162` and `ssh_spawn.rs` / `remote_wrapper.rs` are 100% hardcoded Claude. Generalising remote dispatch is a substantial separate effort with its own auth-distribution problem (`CODEX_HOME` on remote hosts).

- **Effort:** `large`
- **Depends on:** T-23
- **Scope:** deferred (future / not a v1 blocker) — local dispatch is the v1 target; remote is hardcoded Claude end-to-end

### T-29 Drive `codex app-server` over JSON-RPC

Replace or supplement `codex exec` with the persistent app-server protocol, giving live-turn steering (fixing the probe/nudge process-restart mismatch properly), structured interrupts, and explicit hook-run reporting. Currently `[experimental]` and a different execution model from Boss's pane-embedded CLI.

- **Effort:** `large`
- **Depends on:** T-23
- **Scope:** deferred (future / not a v1 blocker) — experimental upstream surface; revisit once it stabilises

### Parallelism

At the same depth, these may run in parallel:

- **Depth 0:** T-01, T-02, T-04, T-07, T-27 — genuinely independent; different subsystems and files.
- **Depth 1:** T-03 and T-05/T-06 are parallel (editorial/shims vs. progress transport). T-08 follows T-07.
- **Depth 2:** T-13, T-14, T-16, T-17, T-21 all depend on T-12 and are otherwise independent.

**File-overlap cautions — order these rather than running them concurrently:**

- **T-04 and T-06** both edit `engine/driver/src/lib.rs`'s trait surface and `engine/core/src/worker_setup.rs`. Land T-04 first; T-06 forward-ports its enum preservingly.
- **T-07 and T-08** both edit `engine/core/src/runner/pane_spawn.rs` and the driver trait's spawn/permission signatures. The dependency edge already serialises them; keep it.
- **T-12 and T-13** both edit the driver normalisers. Land T-12 first; T-13 integrates rather than replaces its mappings.
- **T-02 and T-03** both edit `worker_setup.rs` guard-script emission and `BOSS_BIN_DIR` provisioning. The dependency edge serialises them; keep it.

T-09 is a deliberate barrier: it touches nearly every engine call site, so nothing else should be in flight against those files while it lands.
