# Codex as a first-class agent driver

- **Date:** 2026-07-24
- **Project:** Codex as a first-class agent driver
- **Depends on:** [P1422 — agent-driver abstraction](agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md)
- **Supersedes intent of:** [P284 — Copilot CLI as alternative worker backend](copilot-cli-as-alternative-worker-backend.md) (its "JSON stream schema spike" method is reused here)
- **Boss tree verified at:** `7859b6c4` (`main`), 2026-07-24
- **Codex verified at:** `codex-cli 0.145.0`, `macos-aarch64`, standalone install, `~/.local/bin/codex`
- **Previously verified at:** `codex-cli 0.137.0` — every claim re-run on 0.145.0; see [Version delta](#version-delta-01370--01450)
- **Revised by operator decision, 2026-07-24:** hook-based `ToolUseInterception` is the chosen guardrail mechanism for Codex; the `PATH`-shim relocation becomes a follow-on project sequenced after this one. This overturns the recommendation the doc originally reached — see [Operator decision](#operator-decision)
- **Amended 2026-07-26 (T3718 / pane-viability spike):** empirical Ghostty + `codex exec` findings from `tools/boss/docs/investigations/ghostty-codex-pane-viability.md` (merged PR #2392) rewrite the transport story for pane-hosted workers — see [G-5](#g-5-progressobservation--the-top-gap), [Chosen approach](#chosen-approach), and [GhosttyKit embedder can observe](#ghosttykit-embedder-can-observe). No new product choice is made here beyond recording what the spike measured.

## TL;DR / verdict

Codex is a **better** fit for the P1422 abstraction than the abstraction currently assumes, and a **worse** fit for the parts of Boss that never went through the abstraction at all.

The brief's highest-severity claim — _"Codex has no Stop hook, so a Codex worker would never complete"_ — **is wrong, but the conclusion it drives is still right, for a different reason.** Codex emits `turn.started` / `turn.completed` as native, typed events on its `--json` stdout stream, so turn boundaries are strictly _better_ than Claude's (in-band and structural, not a hook that must be installed). Codex also ships a stable, Claude-wire-compatible hooks system — including a `Stop` hook.

The real blocker was one layer down: **Boss's only production progress ingress was a unix socket fed by the `boss-event` shim.** PR #2363 added a generic stdout JSONL reader, but that reader only helps directly when the **engine owns the pipe/pty master**. Under the pane-hosted Boss shape the **app** owns the pty and the engine receives only `shell_pid`; an outsider with only `shell_pid` **cannot** read that stdout on macOS (pane-viability spike Q1). The resolved design therefore adds a distinct engine-side `AgentJsonlFile` transport that tails the raw Codex rollout under the run-private `CODEX_HOME` and feeds the same reader/fan-out.

Second finding, revised on 0.145.0: **Codex hooks do fire under `codex exec`, and `PreToolUse` deny genuinely blocks a command before it runs.** On 0.137.0 no hook fired in nine configurations; on 0.145.0 the _identical_ configuration fires reliably, with Claude-shaped payloads. **This is the mechanism the Codex driver uses.** `ToolUseInterception` is therefore Codex's chosen guardrail carrier, not a degraded fallback: Codex reaches parity on the mechanism already running in production for Claude, with no new guardrail substrate to build, validate, and cut over first. That is the simplest incremental path, and it is the one being taken — by [operator decision](#operator-decision), which overturned this doc's original recommendation.

What that leaves to settle is narrower, and it is real: hooks fail **open and silently** in two independent ways — an untrusted hook is skipped with no warning, and a hook whose command does not exist produces no diagnostic. So the guarantee rests on Boss provisioning hook trust deterministically and being able to tell when a hook did not run. That is [OQ-1](#oq-1-hook-trust-provisioning) / [T-01](#t-01-codex-hook-trust-provisioning), which the decision moves onto the critical path ahead of the first Codex worker.

Third finding, now scoped to a project of its own: a stronger guardrail mechanism already half-exists. Boss prepends `BOSS_BIN_DIR` to the worker's `PATH` (`engine/core/src/runner/pane_spawn.rs:382`). Moving Boss's command-level guardrails from `PreToolUse` hooks into **`PATH` shims** would make them driver-agnostic, make them fail **closed**, and close a real hole in the Claude path — a hook cannot see a command run inside a subshell. That argument stands on its own merits and the analysis below is retained in full. It is a **follow-on project sequenced after this one**, not a prerequisite for it. See [Guardrail integrity](#guardrail-integrity).

## Goals

- Add OpenAI Codex as a real driver behind the P1422 agent-driver abstraction, so a work item dispatched with `--driver codex` runs end-to-end to a PR with the same lifecycle guarantees a Claude worker has today.
- Produce a **complete gap analysis** — the primary deliverable. Where Codex does not fit the current trait surface, name the abstraction gap and specify the fix _in the abstraction_, never as Codex-specific special-casing in the engine.
- Feed those findings back into P1422's remaining tasks. This project and P1422 are deliberately co-dependent; the [Proposed P1422 amendments](#proposed-p1422-amendments) section is the handoff.
- Identify the seams a future Codex/Claude load balancer will need, so this work does not foreclose it.

## Non-goals

- **Implementing the load balancer.** Out of scope by operator direction. This doc identifies the seams it attaches to and specifies nothing about policy.
- **Removing or de-privileging the Claude path.** Claude remains the reference driver and the default.
- **Codex Cloud, `codex app-server`, `codex mcp-server`, `codex remote-control`.** v1 drives `codex exec` only. The app-server is a strictly richer surface and a plausible v2 (see [Alternative 3](#alternative-3-drive-codex-app-server-over-json-rpc)).
- **Driver-aware kanban / Swift UI.** The kanban already reads abstract `WorkerActivity`; nothing in the product surface needs to know which driver ran. The app still owns GhosttyKit, but the chosen rollout transport is engine-only and does not add surface scraping or app IPC.
- **Remote/SSH dispatch for Codex.** `engine/core/remote/boss-remote-run.sh:84,159,162` is 100% hardcoded Claude. Deferred, and filed as such.
- **Re-litigating the P1422 capability vocabulary.** The 12 capabilities are the right decomposition; this doc changes signatures and adds two, it does not re-open the model.

## Method

Everything about Codex below was established by **running Codex on this host on 2026-07-24**, not from recollection. Where a claim comes from the binary's embedded generated schemas rather than an observed run, it is marked _(binary)_. Where I could not establish something, it is an explicit open question rather than an assertion.

The doc was first written against `0.137.0`. On operator request, **every Codex claim was then re-run against `0.145.0`** — the version now installed — rather than having the version string bumped. The body below states 0.145.0 behaviour; [Version delta](#version-delta-01370--01450) reports what moved, because the churn across eight minor versions is itself a design input.

Boss-side claims were re-verified against `7859b6c4` by locating symbols, not line numbers. **The brief's ground-truth section has already drifted**: the dispatch gate it cites as `engine/core/src/runner.rs:1320-1335` is now `engine/core/src/runner/worker_spawn.rs:597-601` — `runner.rs` has been split into a module directory. Treat the line numbers in _this_ doc the same way.

The spike harness (isolated `CODEX_HOME`, throwaway git repo, hook handler logging its stdin) is reproduced inline in [Appendix A](#appendix-a-reproducing-the-codex-spike).

A second empirical campaign — `tools/boss/docs/investigations/ghostty-codex-pane-viability.md` (PR #2392, 2026-07-26) — tested the **pane-hosted** execution topology specifically: outsider-with-`shell_pid` vs GhosttyKit embedder observation, `SendToPane`-equivalent inject, `codex exec resume`, Esc abort on the TUI, and rollout live-tail. Those findings amend transport, control, and transcript claims below; they do **not** invent a product choice among the remaining open seams.

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

**What this delta settled, and what it left open.** D-2 is what makes hook-based interception available to Codex at all, and by [operator decision](#operator-decision) it is the mechanism the driver uses. It does not, on its own, make that mechanism _reliable_: hooks fail **open, silently, in two independent ways**:

| Failure mode                | Observed behaviour                                                                   |
| --------------------------- | ------------------------------------------------------------------------------------ |
| Hook not trusted            | Command runs normally. **No warning, no stream event, no log line.**                 |
| Hook command does not exist | Turn completes normally. **No diagnostic** — the 0.137.0 control reproduces exactly. |

Hooks run only under `--dangerously-bypass-hook-trust` or a persisted trust record — `[hooks] trusted_hash`, a real key (`--strict-config` accepts it; `HookStateToml.trusted_hash` _(binary)_). A wrong or stale hash is indistinguishable from no hooks at all, and Boss rewrites worker config per run, so a hash that goes stale would silently disarm every guardrail with no signal. That is the residual risk the design carries, and closing it is [T-01](#t-01-codex-hook-trust-provisioning) — a gate on the first Codex worker, not a reason to route guardrails elsewhere. `PATH` shims fail **closed** by comparison, which is why the shim argument survives as a [follow-on project](#guardrail-integrity) rather than being discarded.

[Alternative 1](#alternative-1-replicate-the-claude-architecture--make-codex-emit-hook-callbacks) is still rejected on these same fail-open semantics — but note that it concerns **progress ingress**, not interception, and the two are not symmetric: progress has an unconditional trust-free channel (stdout) and interception has none.

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

One clarification for the landed stdout-reader work: **`--json` events go to stdout and the human-readable `Reading additional input from stdin...` notice goes to stderr.** The JSONL stream is uncontaminated, so the reader needs no filtering.

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

A fourth point, verified because the whole transport design depends on it: **the JSONL goes to stdout, and only to stdout.** The one human-readable line Codex emits (`Reading additional input from stdin...`) goes to stderr, so a reader attached to stdout sees clean JSONL with no filtering. A fifth point, from the pane-viability spike: **attachment is the hard part.** When the engine owns the pipe (or a local pty master), that clean JSONL is fully readable. When the app owns the pane pty and the engine holds only `shell_pid`, an outside process reads **0 bytes** from the slave tty path on macOS (no `/proc/<pid>/fd`; slave open is not the master stream). The GhosttyKit **embedder** can still recover the lines as rendered surface text — see [GhosttyKit embedder can observe](#ghosttykit-embedder-can-observe) — but that is an app-process path, not an engine-with-`shell_pid` path.

### Session, turn, and transcript identity

- Session identity is **`thread_id`** (UUIDv7), not `session_id`. Note the collision hazard: Codex's _hook_ payloads use the field name `session_id` _(binary)_, while its _stream_ uses `thread_id`. These are different names for the same concept and a driver must not confuse them.
- Turn identity is `turn_id`, exposed to hooks as a documented _"Codex extension"_ _(binary)_.
- Transcripts ("rollouts") are JSONL at `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<local-timestamp>-<thread_id>.jsonl`. Verified from the original spike and re-confirmed by the pane-viability campaign: e.g. `sessions/2026/07/24/rollout-2026-07-24T12-26-47-019f9598-31f6-78e3-94c2-34836872ae2c.jsonl`. The **local start timestamp** in the filename means the path is **not** fully predictable a priori from `thread_id` alone — discovery is `glob **/rollout-*-{thread_id}.jsonl` under `$CODEX_HOME/sessions` (or a dir watch; new files appeared at t≈0 in the live-tail experiment). The container format is JSONL, so `engine/transcript-tail` is reusable at the **container** level — but the **line schema is Codex's, not Claude's, and is also not the same dialect as `codex exec --json` stdout** (rollout uses `event_msg` / `response_item` / `custom_tool_call_output`; stdout uses `item.*` / `command_execution.aggregated_output`). Reuse the tailer shell; do not share a parser.

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

| Boss rule                                            | Codex equivalent                                                                                  | Fidelity                                         |
| ---------------------------------------------------- | ------------------------------------------------------------------------------------------------- | ------------------------------------------------ |
| Reviewer read-only                                   | `--sandbox read-only`                                                                             | **Exact**, and OS-enforced rather than advisory  |
| Deny writes to `~/Library/Application Support/Boss/` | `workspace-write` — the Boss data dir is outside the workspace, so it is denied _by construction_ | **Stronger than today**                          |
| Deny `rm -rf`, `sudo`                                | none                                                                                              | **Lost** — no per-command grammar                |
| Deny `bossctl`                                       | none                                                                                              | **Lost** as a rule; carried by `PreToolUse` deny |
| Block `jj git push` / `gh pr create`                 | none                                                                                              | **Lost** as a rule; carried by `PreToolUse` deny |

The two "lost" rows are what [Guardrail integrity](#guardrail-integrity) resolves.

### Bazel under the Codex sandbox

`--sandbox workspace-write` renders with no `[sandbox_workspace_write]` table in the per-run `config.toml` at all, so `writable_roots` and `network_access` take Codex's own binary defaults — `[]` and `false` — rather than anything Boss chose. Every bazel-gated repo was unbuildable under `--driver codex` as a result; a control run of the same smoke test on `--driver claude` in the same repo passed.

Root cause, reproduced rather than inferred: Bazel's client/server handshake (netty/gRPC) needs to bind a localhost TCP socket, which `network_access = false` denies. `bazel`'s shutdown path then calls `ProcessHandleImpl.children()`, which shells out to `sysctl kern.proc.all` — not in Codex's seatbelt allowlist — turning a `SocketException` into `FATAL: bazel crashed due to an internal error`. The `sysctl` failure is a **consequence**, not an independent blocker: once the bind succeeds it degrades to a harmless `WARNING: failed to get value of sysctl kern.maxprocperuid`. Separately, Bazel's default output base (`~/Library/Caches/bazel/_bazel_$USER` on macOS) sits outside the workspace, so empty `writable_roots` denies it too — reproduced standalone: with `writable_roots` granted but `network_access` still `false`, `bazel build` reproduces the original crash verbatim; both keys are required together.

The fix (`render_sandbox_workspace_write_toml` / `bazel_output_user_root` in `engine/driver/src/codex.rs`) emits an explicit `[sandbox_workspace_write]` table with `network_access = true` and `writable_roots` set to Bazel's default `output_user_root`, derived rather than hardcoded: `TEST_TMPDIR` first (Bazel's own convention for a bazel-in-bazel test invocation), else the platform cache-dir default Bazel itself falls back to when no `--output_user_root` flag applies (`~/Library/Caches/bazel` on macOS, `${XDG_CACHE_HOME:-~/.cache}/bazel` elsewhere).

`network_access = true` is a **full outbound-network grant, not localhost-only** — the `sandbox_workspace_write` schema is two-valued (`restricted` / `enabled`); there is no localhost-only tier to select instead. A newer `[permissions]` profile system with `network.mitm.allowed_domains`-shaped domain scoping appears in the 0.145.0 binary but was not confirmed reachable from the `exec` path in the time available for this fix; a future pass that confirms reachability should prefer a domain-scoped grant (localhost + whatever bazel/bzlmod endpoints are actually needed) over this full grant. The full grant is also load-bearing beyond localhost: bzlmod/module-registry fetches and, absent a pinned `.bazelversion`, bazelisk's own version-resolution call both need real egress on a cold cache.

This grant is scoped to `--sandbox workspace-write` only. Reviewer uses `--sandbox read-only` ([G-3](#g-3-permissionpolicy)), under which Codex does not consult `sandbox_workspace_write` at all — the table is inert for that mode — so no worker-kind branch was needed. Reviewers don't run build gates, so leaving them fully network-denied costs nothing.

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

Both failure modes are silent and fail-open. Boss regenerates worker config every run, so a `trusted_hash` that goes stale would disarm every guardrail with nothing to observe. This doc originally concluded that this disqualified hooks as a guardrail carrier outright; the [operator decision](#operator-decision) is that it does not — it makes trust provisioning a **prerequisite to solve**, not a mechanism to abandon, since the alternative is building an entire second guardrail substrate before Codex can run at all.

**What remains open is therefore trust _provisioning_, not activation:** how to compute and persist `trusted_hash` so a Boss worker gets hooks without shipping `--dangerously-bypass-hook-trust`. That flag is not an acceptable default — it also trusts **project-local** `.codex/` hooks from the repository under work, which in Boss's threat model is attacker-controllable content. Under the chosen approach this is on the critical path rather than beside it: it gates the first Codex worker. See [OQ-1](#oq-1-hook-trust-provisioning) and [T-01](#t-01-codex-hook-trust-provisioning).

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

| #    | Capability              | What Codex offers natively                       | Class    | Verdict                                                     |
| ---- | ----------------------- | ------------------------------------------------ | -------- | ----------------------------------------------------------- |
| G-1  | `Spawn`                 | `codex exec` + flags                             | **(b)**  | Signature is Claude-shaped                                  |
| G-2  | `WorkspaceProvisioning` | `AGENTS.md`, `CODEX_HOME`, trust registry        | **(a)**  | Fits; needs `CODEX_HOME` lifecycle                          |
| G-3  | `PermissionPolicy`      | 3 sandbox modes, `writable_roots`, `.rules`      | **(b)**  | Trait is a file path; Codex needs argv+env                  |
| G-4  | `ModelAndEffortMenu`    | `-m`, `model_reasoning_effort`, `debug models`   | **(a)**  | Needs Codex descriptor + model-aware runtime menu adapter   |
| G-5  | `ProgressObservation`   | `--json` stdout JSONL; rollout file; app surface | **(c)**  | **Transport topology still open for pane-hosted** — top gap |
| G-6  | `ToolUseInterception`   | deny-only `PreToolUse`, works but fails open     | **(a)**† | Declared deny-only; gated on T-01                           |
| G-7  | `TurnBoundary`          | `turn.started` / `turn.completed`                | **(c)**  | Native, but no trait method                                 |
| G-8  | `StructuredOutput`      | `--output-schema`, `--output-last-message`       | **(b)**  | Better than Claude's; no trait method                       |
| G-9  | `TranscriptAccess`      | rollout JSONL, Codex line schema                 | **(b)**  | Trait method exists but is dead code                        |
| G-10 | `ControlVerbs`          | process signals; `codex exec resume`             | **(b)**  | Trait has one method, never called                          |
| G-11 | `ToolProvisioning`      | MCP, plugins, skills                             | **(a)**  | Unused in v1, as designed                                   |
| G-12 | `PromptComposition`     | `AGENTS.md` + preamble                           | **(b)**  | Shared body asserts Claude mechanics                        |

† **G-6 was classified (d) in the original pass; the [operator decision](#operator-decision) reclassifies it (a).** Codex's `PreToolUse` exists, fires, and blocks pre-execution on 0.145.0, and it is implementable against the current trait — the earlier (d) recorded Boss _declining_ to declare a working mechanism, which the legend has no code for. Two qualifications survive the reclassification: the capability is **deny-only** (tool-input rewriting is unreachable), and it is **gated on [T-01](#t-01-codex-hook-trust-provisioning)** because hooks fail open when untrusted. See [G-6](#g-6-tooluseinterception).

### G-1 `Spawn`

The trait signature is Claude-shaped (`engine/driver/src/lib.rs:553-560`): `settings_path: Option<&Path>`, `non_opus_auto_mode: bool`, `permission_mode_override: Option<&str>`. `non_opus_auto_mode` is a Claude model-family concept with no Codex meaning; `settings_path` presumes a single settings _file_, whereas Codex needs `CODEX_HOME` (a directory) plus argv.

Worse, `spawn_invocation` returns a `String` that the engine wraps at `engine/core/src/runner/pane_spawn.rs:382`:

```rust
"[ -n \"$BOSS_BIN_DIR\" ] && export PATH=\"$BOSS_BIN_DIR:$PATH\"; unset ANTHROPIC_API_KEY; {}"
```

The `unset ANTHROPIC_API_KEY` is a Claude-ism in shared code (also asserted in tests at `:868,870,942`). It is harmless for Codex but wrong in principle, and it is the wrong shape: Codex needs `CODEX_HOME=<dir>` _exported_, which a string-returning method cannot express cleanly.

**Fix (in the abstraction):** replace the positional Claude-shaped parameters with an opaque `SpawnRequest` struct, and have `spawn_invocation` return a structured `SpawnPlan { env: Vec<(String,String)>, argv_or_shell: String }` so environment mutation is driver-supplied rather than hardcoded in `pane_spawn.rs`. The `PATH`/`BOSS_BIN_DIR` prepend stays engine-side — it is Boss policy, not Claude policy, and the [follow-on `PATH`-shim project](#the-path-shim-design--retained-as-a-follow-on-project) will make it load-bearing for both drivers.

### G-2 `WorkspaceProvisioning`

Fits the current trait (`provision_workspace(&self, workspace, prompt_text, run_id)`, `engine/driver/src/lib.rs:566`). The Codex driver writes `AGENTS.md` instead of `CLAUDE.md`, provisions a per-run `CODEX_HOME`, and pre-stamps `[projects."<workspace>"] trust_level = "trusted"` to suppress the first-run trust prompt — which is precisely what the capability's doc comment says it is for.

One gap: the trait gives no hook for **teardown**. A per-run `CODEX_HOME` accumulates rollout files (the host's `~/.codex` currently holds 279 active rollouts / 323 MB). Claude needed no teardown so none was designed. Minor, but real.

### G-3 `PermissionPolicy`

`ClaudeDriver::write_permission_config` is still `unimplemented!()` (`engine/driver/src/claude.rs:547`), and the real rendering still lives in `engine/core/src/worker_setup.rs`. Its signature has already moved to `PermissionArtifacts` (`engine/driver/src/lib.rs:901-905`), so T1479 is now extraction-only. The remaining blocker is the one-way `core -> driver` dependency: the settings and deny-rule rendering in `worker_setup` must first be ported into the driver crate.

The former Claude-shaped `PathBuf` signature is gone. For Codex the policy is `--sandbox <mode>`, `--ignore-rules` / `--strict-config`, `[sandbox_workspace_write] writable_roots`, and `CODEX_HOME` — argv and config as well as files. (`-a/--ask-for-approval` used to belong on that list; it was removed from `codex exec` between 0.137.0 and 0.145.0 — see [D-1](#deltas-that-change-the-design) — which does not weaken the point, since what remains is still argv plus config plus a directory.)

The method now takes Boss's abstract policy and returns `PermissionArtifacts { config_files: Vec<PathBuf>, extra_args: Vec<String>, env: Vec<(String,String)> }`; the remaining T1479 extraction must preserve that shape.

### G-4 `ModelAndEffortMenu`

`menu_for_driver_in` now resolves through `DriverRegistry` (`engine/effort/src/lib.rs:158,267`), with coverage that it resolves per slug rather than hardcoding Claude (`:571-620`). Effort values are now driver-local through each descriptor's `effort_value_for_level` (for Claude, `claude.rs:34,137`).

Codex fits the `ModelMenu` model cleanly, but **not as the fixed `Degrade` case this doc originally recorded**. On 0.145.0 the effort ladder is per-model and reaches six values (`low, medium, high, xhigh, max, ultra`) — meeting or exceeding Boss's 5-value ladder on the newer models, and varying between models within one catalog. So the mapping is neither a uniform degrade nor a static table: it must be resolved per selected model from `codex debug models` at runtime.

The remaining Codex-specific work is to add a `CodexDriver` descriptor and a runtime menu adapter that reads the available models and their effort ladders, then presents only the effort values supported by the selected model. The already-landed registry and driver-local effort seam should be reused rather than duplicated; this is not blocked on, or a reason to reopen, the completed T3326 work.

### G-5 `ProgressObservation` — the top gap

**This is the finding that most changes P1422's remaining work.**

`ProgressIngress` now distinguishes `HookCallback(ProgressObservationWiring)`, `StdoutJsonl`, and `AgentJsonlFile`; `ProgressObservationWiring` is only the hook-callback payload. The spawn flow accepts the documented empty-hooks cases, and the events socket accepts an injected driver rather than hardcoding `ClaudeDriver`. PR #2363 landed the engine-side generic JSONL reader.

**#2363 settles the reader, not the pane-hosted topology.** The pane-viability spike (PR #2392) split two observers that this design had collapsed:

| Topology                                            | Who observes         | Can they see `codex exec --json`?                                                                                                                                                                                    |
| --------------------------------------------------- | -------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Engine owns** pipe/pty master (engine-spawn)      | Engine parent        | **Yes** — full JSONL (direct pipe and local master harness). #2363's reader is correct for this shape.                                                                                                               |
| **App owns** pane pty; engine has only `shell_pid`  | Engine-like outsider | **No** — 0 bytes on macOS (no `/proc/<pid>/fd`; opening the slave path is not the master stream). #2363 alone does **not** make pane-shaped progress implementable from the engine.                                  |
| **GhosttyKit embedder** owns the surface (Boss app) | App process          | **Yes** — `ghostty_surface_read_text` recovered full exec JSONL as rendered surface text (Layer D). Same family of API Boss uses for the Claude monitor. This is **not** engine ingress until something forwards it. |

So the earlier design sentence _"the transport split and reader are no longer future work"_ is **true for engine-owned streams** and was **false for pane-hosted progress into the engine** until the operator chose the first candidate below:

1. **`AgentJsonlFile` / rollout tail — chosen and implemented.** The file exists and grows live under the run-private `CODEX_HOME`; the engine snapshots pre-spawn candidates, discovers one new rollout, verifies `session_meta` cwd/thread correlation, and feeds its raw bytes to the existing generic reader through a rollout-specific normaliser.
2. **App-forwarded channel** — app scrapes surface text (or tees the master) and forwards into the engine. Layer D proves scrape works; **app→engine IPC for that scrape is not implemented** and is an open design seam ([seam 5](#the-five-engine-seams-this-needs)).
3. **Topology change** — engine owns spawn/pipe (or otherwise receives a real stream fd). Would make `StdoutJsonl` + #2363 sufficient, but changes the pane-hosted Boss shape the P1422 non-goals hold fixed unless an explicit decision says otherwise.

**PR-URL capture dialect depends on which channel lands.** Stdout `command_execution` items carry `aggregated_output` (regex-friendly). Rollout encodes the same tool runs as `custom_tool_call` / `custom_tool_call_output` — not a drop-in for the same extractor. A driver-supplied PR-URL path must declare which dialect it reads.

A related, smaller problem: `WorkerEvent` requires `session_id` on every variant (`protocol/src/worker_event.rs`), and `SessionStartSource` mirrors Claude's `startup|resume|compact`. Codex's identity is `thread_id`, and its `SessionStart` trigger set _(binary)_ is `startup|resume|clear|compact` — a superset. Both need widening.

Finally, `progress_fidelity()` is now registered per live-worker slot and consulted by the stale-worker sweep through `ProgressFidelity::stale_threshold_secs` (`live_worker_state.rs:165-177`; `stale_worker_sweep.rs:290-291`).

### G-6 `ToolUseInterception`

Codex hooks **do** fire on 0.145.0 and `PreToolUse` deny genuinely blocks a command pre-execution ([D-2](#deltas-that-change-the-design)). **The Codex driver declares this capability, deny-only** — it is the same mechanism the Claude path enforces with today, reached without building anything new.

That is a reversal of what this doc originally recommended, and the reasoning is recorded at [Operator decision](#operator-decision). The short form: declining to declare a working mechanism only pays off if something better is already in place to carry the guardrails instead, and nothing is — the alternative was to build, validate, and cut over an entire second substrate before the first Codex worker could run.

Two things the declaration does **not** wave away.

**It is gated on trust provisioning.** A capability Boss declares is one Boss promises to enforce, and Codex hooks fail **open and silently** when untrusted or misconfigured. So the declaration is contingent on [T-01](#t-01-codex-hook-trust-provisioning) establishing that Boss can stamp `trusted_hash` deterministically and observe a hook that did not run. This is the residual risk of the chosen path, stated plainly rather than designed around.

**It is deny-only.** Re-verified on 0.145.0: `unsupported permissionDecision:allow` and `unsupported permissionDecision:ask` both persist _(binary)_, and `updatedInput` requires the rejected `allow`. So the _rewrite_ half of editorial enforcement (`PreToolUseDecision::AllowWithRewrite`, `engine/core/src/editorial_hook.rs:78-81`) is unreachable. That enum distinguishes two rewrite paths, and they fare differently:

- `AllowWithRewrite { updated_command: None }` — the redaction landed in a `--body-file` overwritten on disk. **Works**: the hook rewrites the file and returns no decision, so the command proceeds.
- `AllowWithRewrite { updated_command: Some(cmd) }` — the inline `--body "..."` case, which needs `updatedInput`. **Unreachable under Codex hooks.** It is handled by denying with a reason instructing the worker to use `--body-file`; see [the editorial case](#the-editorial-case-precisely).

The `PATH`-shim design recovers the inline case properly, since a shim can rewrite argv freely. That is one of the reasons it remains worth doing as a [follow-on project](#guardrail-integrity).

Separately, and independent of Codex: the `Degrade` disposition now has an engine dispatch path on `PostToolUse` (`worker_events.rs:793-835`). It calls a driver's registered post-hoc handler when present and makes the bare-degrade loss of guards visible rather than silently dropping it. Codex still does not land on `Degrade`, but the formerly latent abstraction bug is no longer an unimplemented path.

### G-7 `TurnBoundary`

PR #2361 is in flight with the `TurnBoundary` trait method and routes its consumers through the resolved driver. Its engine synthesizer remains deliberately unbuilt, so this document does not duplicate that in-flight revision.

The brief rates this the highest-severity gap on the premise that Codex cannot signal turn end. **That premise is wrong** — `turn.completed` is native, in-band, and carries token usage. Codex's turn boundary is _better_ than Claude's.

The remaining gap is the deliberately deferred synthesizer for a hypothetical driver with neither hooks nor turn events. Codex needs neither that synthesizer nor a duplicate of PR #2361's trait-method work: `turn.completed` maps directly onto `WorkerEvent::Stop` once its driver normaliser is present.

One genuine subtlety: Claude's `Stop` fires per _assistant turn_ within a session; Codex's `codex exec` is **one turn per process**, exiting after `turn.completed`. Boss's probe/nudge loop assumes it can inject a follow-up prompt into a live session (`engine/core/app/pane_delivery.rs`). Under Codex that becomes `codex exec resume`, i.e. a **new process**, not a message into a running one. The resume CLI itself is now validated (same `thread_id`, re-emitted `thread.started`, delivery via `turn.started` — [OQ-5](#risks--open-questions)); the remaining gap is pane lifecycle and engine observation of that new process. This lifecycle difference is still the main reason [T-17](#t-17-controlverbs-on-the-trait-plus-codex-probenudge-via-exec-resume-a-7) is its own task.

### G-8 `StructuredOutput`

Enum variant at `engine/driver/src/lib.rs:43`, **no trait method**. The engine-side file contract exists as `BOSS_STRUCTURED_OUTPUT` (`engine/core/src/spawn_flow.rs:59`) — covering review findings, task followups, postmortem followups. Still transcript-scraped: triage (`engine/core/src/automation_triage.rs:498 parse_triage_decision`) and PR URL (`engine/core/src/pr_url_capture.rs`, which reads `tool_response.stdout` from **`PostToolUse` hook events** — a Claude-hook dependency, re-verified at `pr_url_capture.rs:1-6`).

Codex is **better** here than Claude: `--output-schema <FILE>` constrains the final response to a JSON Schema, and `--output-last-message <FILE>` writes it to a known path. That is a native, enforced structured-output contract — strictly stronger than "ask the agent to write a file and hope."

Two consequences:

- **T1476 (file-based `StructuredOutput` contract) is well-directed and should proceed**, because the env-var file contract is the common denominator that works for both drivers. Its scope is sufficient for Codex _as far as it goes_.
- **The file-based structured-output scope was insufficient in one respect:** PR URL capture is `PostToolUse`-derived. The rollout normaliser now emits `PostToolUse` from correlated `custom_tool_call_output` / `function_call_output` records, and the Codex driver supplies `payload.output` text to the shared URL regex rather than reading stdout `aggregated_output`.

### G-9 `TranscriptAccess`

`transcript_path_for_session()` is now on the driver trait and `live_status_loop` calls `normalize_transcript_entry` before redaction. `engine/transcript-tail` is still Claude-framed, which leaves Codex rollout-path derivation and tailer generalisation as the remaining work.

Codex rollouts are also JSONL, so the **tailer container** is reusable — but path discovery and **line schema** are the problems.

**Path discovery.** Claude's path is discovered because Claude stamps `transcript_path` on hook payloads (`engine/core/src/events_socket.rs`, `live_status_loop.rs`). Codex's `--json` stream does **not** carry `transcript_path` (verified — no such field in any captured envelope). The on-disk pattern is `$CODEX_HOME/sessions/<Y>/<M>/<D>/rollout-<local-timestamp>-<thread_id>.jsonl`. Because the filename embeds a **local start timestamp**, the path is **not** fully constructible from `thread_id` alone: discovery is a **glob** `**/rollout-*-{thread_id}.jsonl` under `$CODEX_HOME/sessions` (or a sessions-dir watch). Pane-viability Q7 observed the file appear at t≈0 after process start and grow while the session ran — discovery latency is not a practical obstacle once `thread_id` or a dir watch is available.

Codex's **hook** payloads _do_ carry `transcript_path` — confirmed on 0.145.0 ([D-2](#deltas-that-change-the-design)) — and the design now does wire hooks, so this is a live option rather than a hypothetical one. It is still not the right primary discovery route: a hook payload only arrives once the worker uses a tool, and only if hooks were trusted, whereas `thread_id` is known from the first `thread.started` (when the observer has a stream) and the driver owns `CODEX_HOME`. Glob-from-`thread_id` stays the primary mechanism because it is unconditional relative to hooks; the hook field is a cross-check, not a dependency.

**Schema ≠ stdout.** Rollout and `codex exec --json` stdout are **different event dialects**. Rollout carries `session_meta`, `event_msg` (`task_started`, `agent_message`, `turn_aborted`, …), and `response_item` / `custom_tool_call` / `custom_tool_call_output`. Stdout carries `thread.started` / `turn.*` / `item.*` with `command_execution.aggregated_output`. A driver cannot treat them as the same parser with a different source. Abort events live in rollout (`event_msg.turn_aborted`); they were **not** observed on exec stdout (Esc abort was spiked on the **TUI**, not on `exec`).

**Landed shape:** rollout discovery is scoped to the exact run-private `CODEX_HOME`, validates `session_meta` cwd/thread identity, and records the selected canonical path. Rollout and stdout retain separate normaliser sessions while sharing the bounded JSONL reader.

### G-10 `ControlVerbs`

The trait has only `classify_error` (`engine/driver/src/lib.rs:644`), and it is **never called** — `engine/core/src/transient_recovery.rs` calls `extract_worker_error` / `classify_claude_error` directly. probe / interrupt / stop / reap are not on the trait at all.

For Codex, stop/reap are process signals and work generically at the process level. `probe` does not — see [G-7](#g-7-turnboundary): a probe into a Claude session is a message to a live process; into Codex it is `codex exec resume`. Delivery confirmation is worse: today it depends on Claude's `UserPromptSubmit` hook (`engine/core/app/pane_delivery.rs`, with a transcript-scan fallback). Codex has neither, so confirmation must come from observing a new `turn.started` on the resumed session's stream.

**Resume probing is now CLI-validated (pane-viability Q6).** On 0.145.0:

```sh
codex exec resume --json … <thread_id> "<follow-up prompt>" </dev/null
```

delivers the follow-up, exits 0, reuses the same `thread_id`, and **re-emits `thread.started`** on the new process before a fresh `turn.started` / `item.*` / `turn.completed`. That makes `turn.started` a usable delivery confirmation for the probe/nudge shape. Resume is a **new process** that reattaches to the thread — not inject into a live pane — which is still a lifecycle difference for [T-17](#t-17-controlverbs-on-the-trait-plus-codex-probenudge-via-exec-resume-a-7), but the CLI half of [OQ-5](#risks--open-questions) is no longer unspiked.

**Interrupt is not symmetric with Claude Esc.** Pane-viability Q5 showed Esc on the **interactive TUI** yields rollout `turn_aborted` (`reason: "interrupted"`), the process survives, and a second turn is accepted. That path is **TUI-only**. `codex exec` is non-interactive — there is no Esc surface on the exec worker shape this design drives. Abort-via-signal (SIGINT/SIGTERM) to `exec` mid-turn was **not** spiked; do not assume Esc semantics transfer. Outsider slave-path inject and `TIOCSTI` are **denied** on modern macOS and are not a stand-in for typed interrupt either.

Error classification is entirely provider-specific (rate limits, quota, auth expiry all have different shapes and different retry semantics) and Codex's must not route through `classify_claude_error`.

### G-11 `ToolProvisioning`

Unused in v1 for any driver, as P1422 intended. Codex has a rich surface here (MCP servers, plugins, skills, marketplaces) but Boss injects nothing. **No gap.** Noted only because Codex's plugin system is a plausible future home for Boss's own tooling.

### G-12 `PromptComposition`

Only the preamble is driver-supplied (`engine/driver/src/claude.rs:602-604`). The shared prompt body still asserts Claude's _mechanism_ — `"A PreToolUse hook blocks these"` at `engine/core/src/worker_setup.rs:309` and `:372`, plus `engine/core/src/runner.rs` and the editorial-enforcement sentence.

The original pass rated this a correctness defect on the grounds that these sentences assert a guarantee that is false for a Codex worker. **Under the chosen hook-based mechanism they are true.** A Codex worker really is running behind a `PreToolUse` hook that blocks these commands, so the existing wording is accurate for both drivers as they stand, and the [operator decision](#operator-decision) removes the urgency here entirely.

What survives is hygiene, not correctness: the shared prompt body still hardcodes one driver's _mechanism name_ into prose every driver receives. That is wrong in principle and will become wrong in fact twice over — for a third driver that blocks commands some other way, and for both drivers once the [`PATH`-shim project](#guardrail-integrity) changes what the enforcing mechanism actually is. The wording should come from the driver; it is no longer something that must land before Codex runs. See [A-10](#proposed-p1422-amendments) / [T-20](#t-20-driver-supplied-enforcement-wording-in-prompts-a-10).

---

## Guardrail integrity

Boss's safety properties are enforced today through Claude's `PreToolUse` hook. The brief requires an explicit refuse-vs-degrade call per guardrail. The answer for all five is **neither refuse nor degrade**: Codex carries them on the same `PreToolUse` mechanism, plus its OS sandbox where that is stronger.

<a id="operator-decision"></a>

### Operator decision — hooks carry Codex's guardrails; `PATH` shims become a follow-on project

This doc originally reached the opposite conclusion, and the operator refuted it:

> I think the guardrails moving to PATH shims should be a separate project, and we shouldn't try to tackle that at the same time as everything else (it's going to make the scope too large) — we confirmed that codex can use tooluseinterception the way we do with claude today, and that's the simplest incremental path.

**What the doc originally recommended** (retained below, and not withdrawn on its merits): reject both options P1422 framed for the `ToolUseInterception` absence policy — post-hoc degrade ([Alternative 2](#alternative-2-post-hoc-only-guardrails-via-the-existing-degrade-path)) and refuse — and instead relocate all command-level guardrails to `PATH` shims in `BOSS_BIN_DIR`, on the grounds that a guardrail must fail **closed** and Codex hooks fail **open and silently** when untrusted or misconfigured. From that it derived a hard scheduling edge: the shims had to land and be verified on the Claude path _before any Codex worker ran_, because they would be Codex's sole guardrail carrier.

**What was decided instead**, and why:

1. **Codex can use `ToolUseInterception` the way Claude does today.** This is not a concession — it is what this doc's own evidence establishes. Codex ships a stable, Claude-wire-compatible hooks system; it was verified live on codex-cli 0.145.0 with payload captures in [D-2](#deltas-that-change-the-design), including a `PreToolUse` deny that blocked a command before execution with the reason reaching the model. Hook-based interception is the **chosen mechanism** for the Codex driver, not a degraded fallback.
2. **It is the simplest incremental path.** Codex reaches parity on the guardrail mechanism already running in production, with nothing new to build, validate, and cut over first. The rejected ordering required a full second guardrail substrate to land on the live Claude path before Codex could produce any value at all.
3. **Scope.** Bundling a rewrite of Boss's live guardrail enforcement into the Codex driver project made that project too large. The shim work is real work and gets its own project.

**What is withdrawn.** The derived scheduling constraint — that shipping the Codex driver before the shims would ship it "unguarded" — **is no longer accurate and does not stand.** Under hook-based interception a Codex worker is guarded by the same class of mechanism that guards a Claude worker today. The `PATH` shims do not gate the first Codex worker.

**What replaces it.** A narrower gate, in the same place in the graph: hooks fail open when untrusted, so [T-01](#t-01-codex-hook-trust-provisioning) (trust provisioning, and detecting a hook that did not run) moves onto the critical path ahead of the first Codex worker. That is a `small` investigation, not a `large` rewrite of live guardrails — which is precisely the scope difference the decision is about.

**What is not withdrawn.** The argument for `PATH` shims stands on its own merits, unchanged: they are driver-agnostic, they fail **closed**, and they close a real hole that exists on the Claude path _today_ — a `PreToolUse` hook sees only the top-level `Bash` tool call, so a command nested in a subshell evades it. The analysis is retained in full [below](#the-path-shim-design--retained-as-a-follow-on-project), reframed from prerequisite to follow-on. It is sequenced **after** this project.

### Per-guardrail calls

| Guardrail                              | Enforced today                                                            | Under Codex                                                                                  | Call                                                                   |
| -------------------------------------- | ------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| **Boss data-dir path guard**           | `PreToolUse` deny (`PATH_GUARD_SCRIPT`, `worker_setup.rs:918`)            | `--sandbox workspace-write`: the Boss data dir is outside the workspace and denied by the OS | **Preserved, strengthened.** Advisory hook → kernel-enforced boundary. |
| **Reviewer read-only**                 | per-kind deny rules (`reviewer_deny_rules`)                               | `--sandbox read-only`                                                                        | **Preserved, strengthened.** Exact semantic match, OS-enforced.        |
| **checkleft push guard**               | `PreToolUse` deny (`CHECKLEFT_PUSH_GUARD_SCRIPT`, `worker_setup.rs:1072`) | `PreToolUse` deny, same script, Codex hook config                                            | **Preserved, same mechanism.**                                         |
| **Revision-PR guard / no direct push** | `PreToolUse` deny                                                         | `PreToolUse` deny, same script, Codex hook config                                            | **Preserved, same mechanism.**                                         |
| **Editorial enforcement**              | `PreToolUse` deny **and rewrite**                                         | `PreToolUse` deny; the inline-`--body` rewrite is unreachable and becomes a deny             | **Preserved by deny-instead-of-rewrite** — see below.                  |

The first two rows are unaffected by the decision: they were never hook-relocations, and Codex's OS sandbox enforces them more strongly than Claude's hook does. The middle two are now a straight reuse of the existing guard scripts behind Codex's hook config rather than a rewrite into shims. Only the editorial row changes character.

### The editorial case, precisely

`PreToolUseDecision` has three outcomes (`engine/core/src/editorial_hook.rs:70-84`). Under Codex's deny-only `PreToolUse` hook:

- `Deny { reason }` — **works.** Codex's deny carries `permissionDecisionReason` to the model, verified in [D-2](#deltas-that-change-the-design).
- `AllowWithRewrite { updated_command: None }` — **works.** The redaction is written into the `--body-file` on disk and the hook returns no decision, so the command proceeds against the corrected file.
- `AllowWithRewrite { updated_command: Some(cmd) }` — the inline `--body "..."` case. **Unreachable**: it needs `updatedInput`, which requires `permissionDecision:allow`, which Codex rejects ([G-6](#g-6-tooluseinterception)).

**The call: deny instead of rewrite.** The third case becomes a `Deny` whose reason instructs the worker to re-issue with `--body-file`. The safety property is fully preserved — unreviewed prose still never reaches GitHub, which is the whole point of the control — at the cost of one extra agent round-trip. This is also already what Boss's worker rules mandate (`.claude/CLAUDE.md` forbids inline `--body` outright, because the shell evaluates backticks inside it), so the deny is enforcing a documented convention rather than inventing a restriction.

The `PATH`-shim project recovers the inline rewrite properly, since a shim can rewrite argv before delegating. That is a genuine improvement, and it is one of the things the follow-on project buys — not something Codex needs first.

The residual risk in every row is that a worker reaches the GitHub API without going through `gh` (raw `curl`, or a language binding). That risk exists identically today on the Claude path, so it is not a Codex regression.

**Net: no guardrail requires refusing Codex, and none is silently degraded.** The `KindRequirements` escalation mechanism stays unused for guardrail reasons in v1 — but see [Codex-eligible kinds](#which-work-item-kinds-are-codex-eligible) for kinds refused on _other_ grounds.

### The `PATH`-shim design — retained as a follow-on project

Kept in full because the argument does not depend on Codex, and because a design doc that deletes the road not taken is less useful than one that records it. **Sequenced after this project; it does not gate any Codex work.**

Boss already prepends `BOSS_BIN_DIR` to every worker's `PATH` (`engine/core/src/runner/pane_spawn.rs:382`). A guard implemented as an executable named `gh` / `jj` / `git` in `BOSS_BIN_DIR`, which evaluates the invocation and then delegates to the real binary, is:

- **driver-agnostic** — it needs no hook, no settings file, no per-agent wire format, and no trust record;
- **fail-closed** — a missing shim means the real binary is not on `PATH` and the command errors loudly, where a missing or untrusted hook is skipped in silence;
- **strictly more robust than the hook** — a `PreToolUse` hook sees the top-level `Bash` tool call, so `sh -c 'gh pr create ...'` nested in a script or a subshell evades it; a `PATH` shim catches every invocation regardless of nesting;
- **already the enforcement point Boss tells workers to use** — `.claude/CLAUDE.md` instructs workers to use `cube pr create`, which is itself a Boss-controlled binary;
- **able to rewrite argv**, which recovers the inline-`--body` editorial case that neither Claude's nor Codex's hook can reach cleanly.

This is not a Codex workaround and never was. It closes a real hole in the Claude path that exists whether or not Codex is ever adopted, and it should be adopted for both drivers — on its own schedule. The work is scoped in [T-02](#t-02-relocate-command-guardrails-to-path-shims-follow-on-project) and [T-03](#t-03-relocate-editorial-enforcement-to-a-gh-path-shim-follow-on-project).

---

## Alternatives considered

### Alternative 1: Replicate the Claude architecture — make Codex emit hook callbacks

Configure Codex hooks to invoke the existing `boss-event` shim, reusing the unix socket, `events_socket.rs`, and the whole ingress path unchanged. Codex's hook payloads are Claude-wire-compatible, so `normalize_progress_event` would need only light adaptation. Zero new engine machinery.

**Rejected — and the 0.145.0 delta strengthens the rejection rather than weakening it.** This was originally rejected partly because no hook could be made to fire. Hooks now demonstrably do fire ([D-2](#deltas-that-change-the-design)), so that objection is gone — but the decisive one remains: an untrusted or misconfigured hook is skipped in **complete silence**. Progress instead uses the raw rollout file, which requires no hook installation or trust record and is observable directly by the engine under the pane-hosted topology.

**Note the asymmetry with interception, which _does_ use hooks** ([operator decision](#operator-decision)). These are not in tension. For progress there is an unconditional, trust-free alternative already on the wire — stdout JSONL — so depending on hooks would be choosing the fragile channel over the robust one for no gain. For interception there is no such alternative short of building one, which is the follow-on `PATH`-shim project. Hooks being wired for interception does make the marginal cost of _additional_ hook events low, so a driver may later declare them as defence-in-depth; that is not a reason to make progress ingress contingent on them.

### Alternative 2: Post-hoc-only guardrails via the existing `Degrade` path

Declare `ToolUseInterception` absent, land on `AbsenceDisposition::Degrade`, and implement the already-typed `PostHocInterceptionFn` / `PostHocInterceptionAction` (`engine/driver/src/lib.rs:497-525`) to check after the fact — scan the transcript for a push that should not have happened, then flag or revert.

**Rejected, and the [operator decision](#operator-decision) does not revive it.** Post-hoc detection of an _already-pushed_ commit or an _already-posted_ GitHub comment is not enforcement; the side effect is public. For editorial controls specifically the whole point is that unreviewed prose never reaches GitHub. Codex's `PreToolUse` deny is genuine **pre-execution** enforcement and is available now, so the degrade path is not what Codex lands on — it is not the second-best option here, it is the wrong shape of option.

The post-hoc types are now dispatched on the `Degrade` path and bare degrade emits a visible loss-of-guards signal. Codex still declares `ToolUseInterception`, so it does not use that fallback; [A-8](#proposed-p1422-amendments) retains the rationale and completion record.

### Alternative 3: Drive `codex app-server` over JSON-RPC

Instead of `codex exec`, run the persistent `codex app-server` and drive it over its JSON-RPC protocol (`thread/start`, `turn/start`, `turn/steer`, `thread/resume`, `thread/interrupt`, plus `TurnCompletedNotification`, `ItemStartedNotification`, `HookRunSummary` — all present in the binary).

**Rejected for v1, strong candidate for v2.** It is a much richer surface: real steering of a _live_ turn (fixing the probe/nudge mismatch in [G-7](#g-7-turnboundary) properly rather than via process restart), structured interrupts, and explicit hook-run reporting. But it is marked `[experimental]` in `codex --help`, it is a fundamentally different execution model from Boss's "agent CLI in a ghostty pane" (which the P1422 design explicitly holds fixed as a non-goal), and it would front-load a large protocol client before the basic driver works. Filed as a deferred task so the option stays visible.

---

## Chosen approach

Drive **`codex exec --json` as the worker CLI** (positional prompt, `< /dev/null`, non-interactive one-turn-per-process), with `--output-last-message` + the existing `BOSS_STRUCTURED_OUTPUT` file contract for structured results, per-worker `CODEX_HOME` for isolation, Codex's OS sandbox for filesystem guardrails, and **Codex's `PreToolUse` hook for command guardrails** — the same mechanism the Claude path enforces with today ([operator decision](#operator-decision)).

**The earlier phrasing — "pane-embedded worker with stdout JSONL as the progress transport" — is not implementable as written for the engine under the current app/engine split.** Empirically (pane-viability spike):

- **Engine-spawned** `codex exec --json` (engine owns stdout pipe/pty master): stdout JSONL + PR #2363's reader **works**. Keep `ProgressIngress::StdoutJsonl` for that topology.
- **Pane-hosted** worker (app owns GhosttyKit/pty; engine receives `shell_pid` only): the engine **cannot** attach to that stdout. The selected transport is the engine-side, run-correlated rollout-file tail (`ProgressIngress::AgentJsonlFile`), not rendered scrollback, PTY reads, or new app IPC.

What remains decided for v1: non-interactive `codex exec --json` is the **agent CLI shape**; pane progress normalisation targets the distinct rollout dialect (`session_meta` / `event_msg` / `response_item`) and does not pretend rollout is stdout; hooks carry guardrails; structured output uses the file contracts above.

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

The argv remains pane-hosted. Progress is additive: the engine tails Codex's independently written rollout file, while Ghostty continues to own and render stdout unchanged.

### The five engine seams this needs

1. **A progress reader matched to topology.** For **engine-owned** stdout: the landed #2363 JSONL reader + `ProgressIngress::StdoutJsonl`. For **pane-hosted** Codex: `ProgressIngress::AgentJsonlFile` tails one run-correlated rollout and feeds the same reader/fan-out with a rollout-dialect session normaliser. PR-URL capture reads `response_item.payload.output`, not stdout `aggregated_output`.
2. **A `TurnBoundary` trait method.** `turn.completed` → `WorkerEvent::Stop` when that event is on the channel the engine actually sees, so `completion/stop.rs` stops being hardwired to a Claude hook.
3. **Driver-supplied transcript path discovery**, via `thread_id` **glob** under `$CODEX_HOME/sessions` (filename embeds a local timestamp — not a hard-coded path template), plus actually calling `normalize_transcript_entry` on the **rollout** dialect (≠ stdout dialect).
4. **Codex hook config carrying Boss's existing guard scripts.** Not a new engine seam so much as a driver-supplied one: the guard-script emission at `worker_setup.rs:918,1072` currently writes Claude settings-file grammar, and must become driver-supplied so the same scripts can be wired into `CODEX_HOME`'s `[[hooks.PreToolUse]]` TOML. Gated on [T-01](#t-01-codex-hook-trust-provisioning) for trust provisioning; landed in [T-11](#t-11-codexdriver-spawn-and-workspace-provisioning).
5. **Pane-to-engine progress transport — resolved.** The engine tails the raw rollout JSONL under the exact run-private `CODEX_HOME`. It prepares before pane spawn, activates after live-state registration, rejects stale/wrong-workspace/ambiguous candidates, and stops on pane teardown. Rendered Ghostty scrollback, raw PTY reads from `shell_pid`, and new app IPC remain explicitly outside this path.

### GhosttyKit embedder can observe

The pane-viability spike's Layer D used a throwaway AppKit host linked against the same pinned GhosttyKit prebuilt Boss uses (`ghosttykit-5659cef`) and the same observation/inject APIs as production (`ghostty_surface_read_text` with viewport selection; `ghostty_surface_text` + Return for SendToPane-equivalent).

**What was measured (not product decisions):**

| Signal                                                                                     | Result                                                                         |
| ------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------ |
| Full `codex exec --json` JSONL recoverable via surface text while codex runs               | **Yes** (`thread.started` / `turn.*` / `item.*` / agent text)                  |
| Same path family as Claude monitor scrape in `GhosttyTerminalHostView.readVisibleContents` | **Yes**                                                                        |
| Mid-`codex exec` inject treated as agent prompt                                            | **No** — echoed into the visual stream only                                    |
| Buffered mid-inject executed by **interactive zsh** after codex exits                      | **Yes** — safety-relevant for SendToPane (see [Risks](#risks--open-questions)) |

**Caveats that block treating Layer D as "engine progress is solved":**

1. Recovered text is **rendered scrollback**, not a dedicated master-fd tee. Fine for short exec JSONL lines; weaker for a noisy TUI.
2. Observation lives in the **app process** that owns GhosttyKit. The engine still only gets `shell_pid` today; **there is no app→engine IPC for surface text yet.**
3. Therefore: "the app can see the worker" ≠ "the engine has progress ingress." Seam 5 remains open.

This section exists so later design work does not re-litigate "is pane content readable at all?" — it is, in-process on the embedder — and so "outsider cannot open the slave" is not misread as "Boss cannot observe the pane."

### Capability declaration for `CodexDriver` (v1)

Provided: `Spawn`, `WorkspaceProvisioning`, `PermissionPolicy`, `ModelAndEffortMenu`, `ProgressObservation`, `TurnBoundary`, `StructuredOutput`, `TranscriptAccess`, `ControlVerbs`, `PromptComposition`, **`ToolUseInterception` (deny-only)**.

Not provided: `ToolProvisioning` (→ `Degrade`, unused for every driver).

`ToolUseInterception` is declared because hooks fire and `PreToolUse` deny blocks pre-execution on 0.145.0 ([D-2](#deltas-that-change-the-design)), and because that is the [chosen mechanism](#operator-decision). Two conditions attach to it, and both are the driver's to satisfy rather than caveats on the declaration:

- **Deny-only.** `permissionDecision:allow`, `:ask`, and `updatedInput` are all rejected, so the trait's rewrite path is unreachable and the inline-`--body` editorial case is handled by denying with a corrective reason ([the editorial case](#the-editorial-case-precisely)).
- **Gated on [T-01](#t-01-codex-hook-trust-provisioning).** An untrusted hook is skipped in silence, so the declaration is only honest once Boss can provision `trusted_hash` deterministically and detect a hook that did not run. T-01 therefore gates the first Codex worker — it is the one hard sequencing edge this design carries, and it is a `small` investigation.

If T-01 established that trust cannot be provisioned deterministically, the fallback is the `PATH`-shim project promoted back ahead of Codex — i.e. the ordering this doc originally recommended. That is the contingency, not the plan.

### Which work-item kinds are Codex-eligible

Phased, with an acceptance criterion per phase. Refusals here are expressed through `KindRequirements`, and they are about **output-contract maturity**, not guardrails — guardrails are carried uniformly by the `PreToolUse` hook on both drivers.

**Phase 1 — chores and project tasks.** The plain "make a change, open a PR" loop. Acceptance: 10 consecutive chores dispatched `--driver codex` reach an open PR with green CI, no engine intervention, and the PR URL captured on the primary path (not a `jj log` reconstruction fallback).

**Phase 2 — design, investigation, postmortem.** These are document-producing kinds and depend on the `BOSS_STRUCTURED_OUTPUT` file contract (T1476) plus followups parsing. Acceptance: a Codex-authored design doc lands with a correctly parsed `Proposed implementation task breakdown`, and its followups materialise.

**Phase 3 — review and conflict resolution.** Review needs `--sandbox read-only` to be verified as a real reviewer-read-only equivalent, and structured `ReviewResult` output. Conflict resolution needs write access plus the merge-conflict telemetry path. Acceptance: a Codex reviewer produces a structured `ReviewResult` on a real PR that a human agrees with, and demonstrably cannot write to the workspace.

**Deferred indefinitely — triage and answer-agent.** Not because of guardrails but because both are **transcript-scraped**: `parse_triage_decision` (`engine/core/src/automation_triage.rs:498`) reads the final assistant message, and the answer agent depends on `UserPromptSubmit`-based delivery confirmation (`engine/core/app/pane_delivery.rs`) that Codex does not have. Ironically Codex's `--output-schema` would make triage _more_ reliable than Claude's — but that is a rewrite of the triage contract, not a driver task. Refuse via `KindRequirements` until then.

### Load-balancing seams

Design _for_, do not design _now_. Three seams, with attachment points:

1. **Per-driver capacity accounting.** Slots are one global pool today. The seam is the dispatch gate at `engine/core/src/runner/worker_spawn.rs:597` — it already resolves `(kind, driver)` and is the natural place for an in-flight count keyed by driver slug. Requirement on this project: **do not add a second, driver-blind admission path.** Progress-ingress work (stdout reader, rollout tail, or app-forwarded channel) must not spawn workers outside this gate.
2. **Per-provider rate-limit state.** Codex hands this over for free: `turn.completed` carries `input_tokens`, `cached_input_tokens`, `cache_write_input_tokens`, `output_tokens`, `reasoning_output_tokens` (verified in the capture above), and the binary carries `RateLimitSnapshot` / `RateLimitWindow` types. The seam is the progress reader — it should record per-turn usage against the driver rather than discarding it. **Treat the usage field set as open:** `cache_write_input_tokens` was added between 0.137.0 and 0.145.0 with no wire signal ([D-4](#stream-drift--all-silent-all-additive)), so a balancer that destructures a fixed set of counters will break on the next upgrade. Claude has no equivalent in-band signal, which is itself worth knowing before a balancer assumes symmetry.
3. **Capability-aware routing.** `CapabilityResolver::check_dispatch` already computes exactly the predicate a balancer needs ("can driver D run kind K"). It must stay a **pure, side-effect-free query** so a balancer can call it speculatively across candidate drivers before choosing. Requirement on this project: do not make `check_dispatch` mutate state or log dispatch decisions as a side effect.

### Migration and coexistence

- **Per-host auth.** `CODEX_HOME/auth.json`, symlinked from a host-level credential. No env-var collision with Claude. `unset ANTHROPIC_API_KEY` becomes driver-supplied ([G-1](#g-1-spawn)).
- **Config collisions.** Solved by per-worker `CODEX_HOME`. Without it, concurrent workers race on `~/.codex/config.toml`'s project-trust registry.
- **Workspace layout.** Codex uses `AGENTS.md` and `.codex/`; Claude uses `CLAUDE.md` and `.claude/`. They do not collide _by name_ — but Codex's `external_agent_config.detect` actively looks for `.claude/settings.json`, `CLAUDE.md`, and `hooks.json` and offers to import them. The Codex driver must disable that import (`external_config_migration_prompts`). A workspace that has run both drivers will contain both `.claude/` and `.codex/`; both must be engine-gitignored.
- **A second import vector, new in 0.145.0.** Alongside config import, 0.145.0 adds an `external_agent_memory_import` feature (currently _under development_, default off) and an `external_agent_config_imports` table in `state_5.sqlite`. Suppressing config-migration prompts is therefore not a one-time fix — the surface is growing, and the Codex driver should assert its intended import posture explicitly rather than relying on a single flag's default. Per-worker `CODEX_HOME` limits the blast radius, since the import bookkeeping lives in the run's own state DB.
- **Cube.** Nothing in `cube`'s workspace provisioning assumes an agent — it manages jj workspaces, leases, and PRs. `cube pr create` is agent-neutral and is, usefully, the enforcement point both the current hook guards and the [follow-on `PATH`-shim project](#the-path-shim-design--retained-as-a-follow-on-project) lean on. **No cube changes required**, which is a genuinely good outcome and worth stating explicitly.

---

## Risks / open questions

<a id="oq-1-hook-trust-provisioning"></a>
**OQ-1 — How does Boss provision Codex hook trust, and detect a hook that did not run?** The original form of this question ("do hooks fire under `codex exec`?") is **answered: they do, on 0.145.0**, and `PreToolUse` deny genuinely blocks. What replaces it is narrower and more operational. Hooks run only when trusted, via `--dangerously-bypass-hook-trust` or a persisted `[hooks] trusted_hash`; an untrusted hook is skipped in complete silence, as is a hook whose command is missing. The bypass flag is not an acceptable default because it would also trust project-local `.codex/` hooks originating in the repository under work. So: what is `trusted_hash` computed over, can Boss stamp it deterministically when it regenerates worker config, and is there any observable signal that a configured hook did not fire?

**This question is now load-bearing rather than exploratory.** The [operator decision](#operator-decision) makes hooks Codex's guardrail carrier, so T-01 must establish deterministic trust provisioning and whether a skipped hook can be observed before the first Codex worker runs. → [T-01](#t-01-codex-hook-trust-provisioning).

<a id="oq-2"></a>
**OQ-2 — Version pinning and churn. Now evidenced rather than precautionary.** The `--json` stream still carries **no schema version**, and re-running this analysis across 0.137.0 → 0.145.0 produced four concrete breaks in eight minor versions: a removed flag that would have made the prescribed launch command fail (`-a`), an added `usage` field, a changed item-ID base, a second meaning for `error` items, plus four new `TurnItem` variants and a new hook event. None of it was announced on the wire. This is no longer a hypothetical risk — it is the observed release cadence. Recommendation firms up accordingly: **pin the tested version, add `--strict-config` for the config half, and gate upgrades on the conformance harness (T1483 / [T-22](#t-22-extend-the-reference-driver-conformance-harness-a-12-amends-t1483))**. Note `--strict-config` covers config keys only; nothing validates the event stream, so the harness remains the sole defence there. "Pin the agent CLI version" is still a policy decision with operational cost, and still the operator's call.

<a id="oq-3-what-is-the-codex-rules-execpolicy-format"></a>
**OQ-3 — What is the Codex `.rules` execpolicy format?** On 0.145.0 `--ignore-rules` is a **documented** `codex exec` flag (_"Do not load user or project execpolicy `.rules` files"_) rather than the binary-string inference it was on 0.137.0, which raises confidence that the system is real and reachable. It might restore some per-command deny fidelity natively — as a fail-closed, config-declared alternative to the hook that carries Codex's guardrails today, and potentially a cheaper answer than the follow-on shim project. Still unexamined — I did not want to design against a surface I had not run.

**OQ-4 — Rollout disk growth.** `~/.codex` on this host holds 279 active + 241 archived rollouts at ~865 MB. Per-worker `CODEX_HOME` multiplies this across workspaces. `--ephemeral` avoids it entirely but would forfeit `TranscriptAccess`. Needs a retention policy; not a v1 blocker.

**OQ-5 — `codex exec` is one turn per process (CLI half answered; pane residual remains).** Claude's probe/nudge injects into a live session; Codex requires `codex exec resume`, a new process. **Pane-viability Q6 spiked the resume CLI:** on 0.145.0, `codex exec resume --json <thread_id> <prompt>` delivers the follow-up, reuses `thread_id`, re-emits `thread.started`, and produces a fresh `turn.started` usable as delivery confirmation. So the probe/nudge **mechanism** for [T-17](#t-17-controlverbs-on-the-trait-plus-codex-probenudge-via-exec-resume-a-7) is no longer unvalidated at the CLI layer.

What remains open (and is still where surprises live):

- **Pane topology across process restart** — launching resume inside a Boss pane, correlating the new process with the worker slot, and feeding its progress through whichever [seam 5](#the-five-engine-seams-this-needs) channel is chosen.
- **Abort-by-signal for `exec`** — Esc/`turn_aborted` is TUI-only (Q5); SIGINT/SIGTERM mid-`exec` turn was not spiked ([G-10](#g-10-controlverbs)).

<a id="oq-6-codex-exec-review"></a>
**OQ-6 — Is `codex exec review` a better substrate for Boss's review kind than a plain read-only exec run?** New in this pass ([D-3](#delta-that-changes-a-tasks-scope)). It is purpose-built, takes `--base` / `--commit` / `--uncommitted`, and has a dedicated `codex-auto-review` model. It may also impose its own output shape that does not match Boss's `ReviewResult`. Unexamined; folded into [T-25](#t-25-codex-eligibility-for-review-and-conflict-resolution-kinds).

**OQ-7 — Which pane-to-engine progress channel?** Recorded as [seam 5](#the-five-engine-seams-this-needs). Not answered by this doc; blocked on a product/topology pick among rollout tail, app-forwarded observation, and engine-owned spawn.

**Risk — the `PATH`-shim relocation is a change to the Claude path.** It touches live guardrails on the driver that runs everything today. It is a net improvement (it closes the subshell-evasion hole) but it is not risk-free.

The original pass paired that risk with a claim that has since been **withdrawn**: that the ordering was "correct and non-negotiable — shipping Codex first means shipping it unguarded". That is not accurate under hook-based interception, where a Codex worker is guarded by the same class of mechanism as a Claude worker. The risk is real; the scheduling consequence drawn from it was not. Both the risk and its mitigation now belong to the [follow-on `PATH`-shim project](#the-path-shim-design--retained-as-a-follow-on-project) — where it should still be a human's call before [T-02](#t-02-relocate-command-guardrails-to-path-shims-follow-on-project) starts, just not a call that blocks Codex.

**Risk — Codex's guardrails inherit Claude's fail-open hook semantics, plus a trust gate Claude does not have.** This is the cost of the incremental path, stated in one place: Boss's command guardrails on Codex are exactly as strong as its hook wiring, and Codex adds a silent trust failure mode on top. [T-01](#t-01-codex-hook-trust-provisioning) is what makes this acceptable, and it must genuinely answer the detection half — "can Boss tell a hook did not run" — not just the provisioning half. A T-01 that provisions trust but cannot observe a skipped hook leaves this risk open.

**Risk — `SendToPane` while `codex exec` is mid-turn is a safety footgun, not hygiene.** Pane-viability Q2 (Layer D, Boss-equivalent `ghostty_surface_text` + Return into a real interactive shell): inject during a foreground `codex exec` is **not** consumed as agent input; the line is echoed and **survives** across codex exit; when the pane returns to interactive zsh, that shell **executes** the buffered command. Outsider slave-path write / `TIOCSTI` does **not** reproduce this (permission denied / non-representative) — the realistic path is master-side / GhosttyKit inject, i.e. production SendToPane. A guard ("is this worker accepting typed input") is required before SendToPane on a Codex exec worker. **Cross-ref the guard work item** (do not treat as optional polish).

**Correction (2026-07-27) — the guard must be conditioned on the driver, not on activity alone.** The first implementation of that guard keyed purely on live `WorkerActivity`, refusing every mid-turn `SendToPane` on every driver. That over-applied it: the footgun above is a property of `codex exec`'s foreground process (one turn per process, stdin on `/dev/null`), not of "the pane is busy". Claude Code is a long-lived interactive TUI that reads stdin for the whole session and holds mid-turn input as its next prompt, so a mid-turn write there is consumed by the agent and never reaches the shell. Because the urgent-probe path fires on `PostToolUse`, where the activity is `Working` by construction, an activity-only guard made `bossctl probe --urgent` structurally undeliverable for _every_ driver — observed in the field as two probes reported "queued" and never delivered to a healthy Claude worker across ~27 tool boundaries. The decision is now `activity × AgentDriver::mid_turn_pane_input()`, defaulting to reject so a new driver is safe until it establishes otherwise; `codex` declares reject explicitly.

---

## Proposed P1422 amendments

Discrete, filed-work-item-sized. The original design pass could not create Boss work items; this revision materializes the immediate P3330 gates as T3681 through T3686. The remaining entries stay as the coordinator's handoff until they are independently scheduled.

| #    | Proposed name                                                                        | Effort    | Amends / new                                                                      | Brief                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| ---- | ------------------------------------------------------------------------------------ | --------- | --------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A-1  | `ProgressObservation`: abstract the ingress transport, not just normalisation        | `large`   | **Amends the prior transport abstraction**                                        | Landed for the pane topology: `ProgressIngress` separates hook callbacks, engine-owned stdout JSONL, and run-correlated agent JSONL files. Pane-hosted Codex uses the rollout-file arm and the existing generic reader; the app-owned PTY remains visual only.                                                                                                                                                                                                                                                                                                                                                                     |
| A-2  | `PermissionPolicy`: return permission _artifacts_, not a single file path            | `medium`  | **Amends T1479**                                                                  | The signature now already returns `PermissionArtifacts`; only T1479's extraction remains. It must first move the settings and deny-rule rendering from `worker_setup` across the one-way `core -> driver` boundary, retaining the existing config-files, args, and env artifact shape.                                                                                                                                                                                                                                                                                                                                             |
| A-3  | `Spawn`: replace Claude-shaped parameters with a `SpawnRequest`/`SpawnPlan` pair     | `medium`  | **New**                                                                           | Landed in PR #2355: `SpawnRequest` and `SpawnPlan` replace Claude-shaped parameters and let each driver supply its command and environment directives, including Codex's `CODEX_HOME`. This row retains the architectural rationale.                                                                                                                                                                                                                                                                                                                                                                                               |
| A-4  | `TurnBoundary` trait method — decouple completion from `WorkerEvent::Stop`           | `medium`  | **Amends T3325** (re-scopes; drops the synthesizer from the critical path)        | PR #2361 is in flight with the trait method and driver-routed consumers. Codex's native `turn.completed` maps directly to `WorkerEvent::Stop`; the synthesizer remains separate future work for a driver with neither hooks nor turn events.                                                                                                                                                                                                                                                                                                                                                                                       |
| A-5  | `StructuredOutput` trait method + driver-supplied PR-URL extraction                  | `medium`  | **Amends T1476** (adds PR-URL; T1476's own scope is sufficient as far as it goes) | `StructuredOutput` (`lib.rs:43`) has no trait method. More urgently, PR-URL capture is derived from `PostToolUse` hook events (`pr_url_capture.rs:1-6`) and is out of T1476's scope — under Codex it breaks completely, and the PR URL is the acceptance criterion for nearly every work item. Stdout dialect: `command_execution.aggregated_output` is regex-friendly. Rollout dialect: `custom_tool_call_output` — not the same extractor. Make extraction driver-supplied and dialect-aware of [seam 5](#the-five-engine-seams-this-needs). Also surface `--output-schema`, which is a stronger contract than the env-var file. |
| A-6  | `TranscriptAccess`: driver-supplied path discovery, and actually call the normaliser | `small`   | **New**                                                                           | Landed: Codex rollout discovery is contained to the exact run home, the selected path flows through normalised events, and live status uses a separate rollout transcript normaliser.                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| A-7  | `ControlVerbs`: put probe/interrupt/stop/reap on the trait and call `classify_error` | `medium`  | **New**                                                                           | The trait has only `classify_error` (`lib.rs:644`) and it is never called — `transient_recovery.rs` calls `classify_claude_error` directly. probe/interrupt/stop/reap are absent entirely, yet probe is precisely where Claude and Codex diverge (live-session message vs `codex exec resume`). **CLI resume probing is spiked** (Q6: same `thread_id`, re-emitted `thread.started`, delivery via `turn.started`); residual is pane topology + exec abort-by-signal (Esc/`turn_aborted` is TUI-only). Error classification is provider-specific and must not route through Claude's classifier.                                    |
| A-8  | Implement the post-hoc interception degrade path                                     | `medium`  | **New** — deferred                                                                | Landed: `worker_events` dispatches the `Degrade` path at `PostToolUse`, invokes a registered `PostHocInterceptionFn` when present, and emits a visible loss-of-guards signal for bare degrade. Codex declares the capability and does not land there; this row remains as the rationale and record of the completed safety correction.                                                                                                                                                                                                                                                                                             |
| A-9  | Widen `WorkerEvent` session identity and `SessionStartSource`                        | `small`   | **New**                                                                           | `WorkerEvent` requires `session_id` on every variant (`protocol/src/worker_event.rs`) and `SessionStartSource` mirrors Claude's `startup\|resume\|compact`. Codex's identity is `thread_id` and its trigger set is `startup\|resume\|clear\|compact` — a superset. Note the trap: Codex's _hooks_ say `session_id` while its _stream_ says `thread_id`.                                                                                                                                                                                                                                                                            |
| A-10 | `PromptComposition`: driver-supplied enforcement wording                             | `small`   | **New** — deferred                                                                | `worker_setup.rs:364` tells the worker _"A PreToolUse hook blocks these"_. The original pass rated this a correctness defect because the sentence was false for a Codex worker; under hook-based interception **it is true for both drivers**, so the defect is gone and this is hygiene. Still worth doing — shared prompt prose should not hardcode one driver's mechanism name — and it becomes live again when the `PATH`-shim project changes what actually enforces. Deferred, not closed.                                                                                                                                   |
| A-11 | Resolve or delete `progress_fidelity()`                                              | `trivial` | **New**                                                                           | Landed: spawn records each driver's fidelity on the live-worker slot, and the stale-worker sweep consults `ProgressFidelity::stale_threshold_secs`. A Codex driver's declared tier now affects stale detection.                                                                                                                                                                                                                                                                                                                                                                                                                    |
| A-12 | Extend T1483's conformance harness to cover transport and turn boundaries            | `medium`  | **Amends T1483**                                                                  | T1483 (blocked on T1476 + T1479) was scoped against a Claude-shaped driver. It must also assert: stdout-JSONL ingress produces the same `WorkerEvent` sequence as hook ingress; a turn boundary drives completion identically from either source; and a pinned agent-CLI version is verified, given Codex's unversioned stream ([OQ-2](#oq-2)).                                                                                                                                                                                                                                                                                    |

**Verdict on the existing tasks, as required by the brief:** T3324 (cut over every call site) remains sufficient and correctly scoped. T3326's registry-backed menu and driver-local effort work landed. T1476's shared file contract is present but its remaining prerequisite role needs verification; it still does not cover PR-URL capture (A-5). T1479 is now extraction-only (A-2). PR #2361 carries the T3325 trait-method work while leaving the synthesizer separate (A-4). T3328's transport split + #2363 reader landed for **engine-owned** streams (A-1); **pane-to-engine handoff (seam 5) is still open**. T1483 still needs the cross-transport coverage in A-12.

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

The original 0.137.0 control still reproduces as a **silent** failure on 0.145.0 — swap the handler for `command = "/definitely/not/a/real/binary-xyz"` and the turn completes with no error, no warning, and no stream event. Together these two silences are the residual risk in hook-carried guardrails, and reproducing them is the starting point for [T-01](#t-01-codex-hook-trust-provisioning): the task's real question is whether _anything_ distinguishes these runs from a healthy one ([OQ-1](#oq-1-hook-trust-provisioning)).

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

**Re-scoped by the 0.145.0 delta pass, and promoted to a hard gate by the [operator decision](#operator-decision).** The original question — do hooks fire under `codex exec`? — is answered: they do, and `PreToolUse` deny blocks pre-execution ([D-2](#deltas-that-change-the-design)). What remains is trust provisioning. Determine what `[hooks] trusted_hash` is computed over (`HookStateToml`), whether Boss can stamp it deterministically when it regenerates worker config each run, and — the part that actually gates the capability — whether there is **any** observable signal distinguishing "hook ran and allowed" from "hook was silently skipped". Also assess the blast radius of `--dangerously-bypass-hook-trust`, which trusts project-local `.codex/` hooks from the repo under work. Check `$CODEX_HOME/hook_outputs` and the binary's `hook_started` / `hook_completed` / `hook_denied` / `hook_run_id` telemetry vocabulary as candidate signals. Output is a written finding plus a reproducible harness, not code.

**This is the one hard sequencing edge in the graph.** Hooks are Codex's guardrail carrier, so this must land and answer both halves — provisioning _and_ detection — before the first Codex worker runs. It replaces the withdrawn "shims must land first" constraint, at a fraction of the scope. If the answer is that trust cannot be provisioned deterministically, escalate: the fallback is promoting the `PATH`-shim project ([T-02](#t-02-relocate-command-guardrails-to-path-shims-follow-on-project), [T-03](#t-03-relocate-editorial-enforcement-to-a-gh-path-shim-follow-on-project)) back ahead of Codex, which is a scope decision for the operator, not for this task.

- **Effort:** `small`
- **Depends on:** none
- **Scope:** in-scope — **gates [T-11](#t-11-codexdriver-spawn-and-workspace-provisioning) and everything downstream of it**

### T-02 Relocate command guardrails to `PATH` shims (follow-on project)

Move the checkleft push guard, revision-PR guard, and direct-push blocks out of the `PreToolUse` guard scripts (`worker_setup.rs:918`, `:1072`) into executables in `BOSS_BIN_DIR` that evaluate the invocation and delegate to the real binary. Behaviour-preserving from the worker's perspective, and it closes the subshell-evasion hole the hook has today on **both** drivers.

**Moved out of this project by [operator decision](#operator-decision).** It was originally scoped as a Claude-path prerequisite that had to land before any Codex worker ran; that constraint is withdrawn, because Codex carries the same guardrails on its own `PreToolUse` hook. Bundling a rewrite of live guardrail enforcement made the Codex project too large. The technical case is undiminished — see [the retained analysis](#the-path-shim-design--retained-as-a-follow-on-project) — and this belongs to a follow-on project sequenced after this one. Listed here so the argument and its scope stay attached to the analysis that produced them.

- **Effort:** `large`
- **Depends on:** none
- **Scope:** follow-on project (sequenced after this project; gates nothing here)

### T-03 Relocate editorial enforcement to a `gh` `PATH` shim (follow-on project)

Move `editorial_hook.rs` evaluation from the `PreToolUse` hook to a `gh` shim, preserving all three `PreToolUseDecision` outcomes including inline `--body` argv rewriting — the one outcome no hook can reach on either driver, and the concrete thing this buys beyond the hook path. Separate PR from T-02: different subsystem (`boss-editorial` + audit log), different risk profile, and T-02's shims are the prerequisite mechanism.

- **Effort:** `medium`
- **Depends on:** T-02
- **Scope:** follow-on project (with T-02; Codex handles the inline-`--body` case by denying with a corrective reason in the meantime)

### T-08 `PermissionPolicy` artifacts signature (P1422 amendment A-2, amends T1479)

Extract the remaining Claude permission rendering for T1479 behind the existing `PermissionArtifacts { config_files, extra_args, env }` shape. `write_permission_config` still has an `unimplemented!()` at `claude.rs:547`; port `worker_setup`'s settings and deny-rule rendering into the driver crate before completing that extraction.

- **Effort:** `medium`
- **Depends on:** none
- **Scope:** in-scope

### T-09 Resolve driver at every call site (existing T3324)

The cutover: replace every hardcoded `ClaudeDriver` construction with a registry resolution. Confirmed still open and unchanged in scope. Listed here as an explicit dependency edge because a Codex driver cannot be exercised until it lands, and it is easier after the remaining T-08 extraction and the in-flight PR #2361 turn-boundary routing settle.

- **Effort:** `large`
- **Depends on:** PR #2361, PR #2355, T-08
- **Scope:** in-scope

### T-10 `CodexDriver` skeleton: descriptor, capabilities, model menu

The crate and struct: `DriverDescriptor` (`AGENTS.md`, `.codex`), `CapabilitySet` per this design, and a `ModelMenu` sourced from `codex debug models`. No spawning yet.

- **Effort:** `medium`
- **Depends on:** T-09
- **Scope:** in-scope

### T-11 `CodexDriver` spawn and workspace provisioning

Implement `spawn_invocation` (the `codex exec --json` line, including `< /dev/null`) and `provision_workspace` (per-run `CODEX_HOME`, `auth.json` symlink, `AGENTS.md`, pre-stamped project trust, `external_config_migration_prompts` disabled). Produces a Codex worker that starts, but whose progress is not yet observed end-to-end.

**Pane launch half-answer from the viability spike:** the CLI line is fine in a pane (positional prompt auto-runs; `< /dev/null` still required). What is **not** answered by implementing spawn alone: how the engine observes that pane. Do **not** assume #2363 attaches via `shell_pid`. T-11 must either (a) document that progress depends on a later [seam 5](#the-five-engine-seams-this-needs) decision and leave observation to a follow-on task, or (b) implement the chosen channel once that decision exists. Spawning without a chosen ingress is a valid intermediate milestone; claiming "pane-hosted Codex works" without seam 5 is not.

**Includes Codex's guardrail wiring**, which the [operator decision](#operator-decision) puts here rather than in a separate shim project: emit Boss's existing guard scripts (the path/checkleft scripts begin at `worker_setup.rs:972` and `:1131`, with wiring at `:580-610`) plus editorial enforcement into `CODEX_HOME`'s `[[hooks.PreToolUse]]` TOML, and stamp hook trust per T-01's finding. The guard-script emission is currently hardcoded to Claude settings-file grammar and must become driver-supplied — the scripts themselves are reusable as-is, since Codex's payloads carry `tool_name: "Bash"` and Claude's `tool_input` shape ([D-2](#deltas-that-change-the-design)). Handle the inline-`--body` editorial case as a `Deny` with a corrective reason, per [the editorial case](#the-editorial-case-precisely).

- **Effort:** `large`
- **Depends on:** T-01, T-10
- **Scope:** in-scope

### T-12 `CodexDriver` progress normaliser

Map both Codex dialects onto `WorkerEvent` with separate reader-owned normalisers. Stdout handles `thread.*`, `turn.*`, and `item.*`; rollout handles `session_meta`, `event_msg`, and correlated `response_item` tool calls/outputs. Both feed the same generic reader and ordered fan-out.

Three constraints from the 0.145.0 delta pass, each a real trap: item IDs are **0-based** and must not be treated as ordinal or 1-based; `item.completed` with `type:"error"` carries **operational warnings as well as** turn failures, so it must not be mapped unconditionally to a failed turn; and the `TurnItem` enum grew by four variants across eight minor versions, so unknown variants must be ignored-with-logging rather than rejected.

- **Effort:** `large`
- **Depends on:** PR #2361, T-11
- **Scope:** in-scope

### T-13 Widen `WorkerEvent` session identity and `SessionStartSource` (A-9)

Accommodate Codex's `thread_id` and its `startup|resume|clear|compact` trigger set. Small and mechanical, but it touches `boss-protocol` and therefore every consumer, so it is its own PR. **File overlap:** co-edits the driver normalisers with T-12 — land T-12 first, and forward-port its mappings preservingly.

- **Effort:** `small`
- **Depends on:** T-12
- **Scope:** in-scope

### T-14 Driver-supplied PR-URL extraction (A-5)

PR-URL capture remains triggered by shared `PostToolUse`, while the driver supplies dialect-specific feed text. Codex rollout capture scans correlated `response_item.payload.output` from both observed output variants and reuses the shared URL matcher/command gates.

**Ordering note:** PR #2361 rewires the trigger onto the turn boundary; land this after #2361. This is not a duplicate: `pr_url_capture.rs:1-6` is still derived from `PostToolUse` events.

- **Effort:** `medium`
- **Depends on:** T-12
- **Scope:** in-scope

### T-15 `StructuredOutput` trait method and `--output-schema` wiring (A-5)

Put `StructuredOutput` on the trait and have the Codex driver use `--output-schema` / `--output-last-message` alongside the shared `BOSS_STRUCTURED_OUTPUT` file contract. Depends on T1476 landing the file contract first.

**Verification note:** the `BOSS_STRUCTURED_OUTPUT` file contract already exists at `spawn_flow.rs:59`. Verify whether any remaining T1476 work is still a prerequisite; do not silently discard that dependency.

- **Effort:** `medium`
- **Depends on:** T-14
- **Scope:** in-scope

### T-16 `TranscriptAccess`: driver-supplied path discovery (A-6)

Discover Codex's rollout path by **glob** `**/rollout-*-{thread_id}.jsonl` under `$CODEX_HOME/sessions` (local timestamp in the filename blocks pure path construction) and generalise `engine/transcript-tail` beyond its "claude transcript files" framing at the **container** level only. Keep a separate line normaliser for the rollout dialect. `transcript_path_for_session()` is already on the trait, and `live_status_loop` already calls `normalize_transcript_entry`.

- **Effort:** `medium`
- **Depends on:** T-12
- **Scope:** in-scope

### T-17 `ControlVerbs` on the trait, plus Codex probe/nudge via `exec resume` (A-7)

Put probe/interrupt/stop/reap on the trait, route `transient_recovery.rs` through `classify_error` instead of `classify_claude_error`, and implement Codex probing as `codex exec resume` with delivery confirmed by observing a new `turn.started`.

**CLI half answered (pane-viability Q6 / [OQ-5](#risks--open-questions)):** resume delivers the follow-up prompt on 0.145.0; same `thread_id`; re-emits `thread.started`; `turn.started` is a usable confirmation. Remaining work is **Boss integration**, not CLI discovery: pane topology for launching resume as a new process in the worker slot, wiring confirmation through the chosen progress channel, and defining interrupt for non-interactive `exec` (Esc/`turn_aborted` is TUI-only; SIGINT mid-turn unvalidated — do not invent Esc semantics for exec).

- **Effort:** `large`
- **Depends on:** T-12
- **Scope:** in-scope

### T-18 `TurnBoundary` engine synthesizer (remainder of T3325)

The synthesize-from-a-lower-fidelity-channel path, for a future driver with neither hooks nor native turn events. It remains outside the turn-boundary trait-method work on PR #2361 because Codex does not need it and it should not gate Phase 1.

- **Effort:** `medium`
- **Depends on:** PR #2361
- **Scope:** deferred (future / not a v1 blocker) — Codex has native turn events; needed only for a third driver with neither hooks nor turn boundaries

### T-20 Driver-supplied enforcement wording in prompts (A-10)

Replace the hardcoded _"A PreToolUse hook blocks these"_ at `worker_setup.rs:364` with driver-supplied wording.

**Downgraded from correctness to hygiene by the [operator decision](#operator-decision).** The original justification was that this sentence asserts a false guarantee to a Codex worker; under hook-based interception it is **true** for a Codex worker, so nothing false is being told to anyone and this no longer blocks Codex. It is still worth doing — shared prompt prose should not hardcode one driver's mechanism name — and it becomes live again when the `PATH`-shim project changes what actually enforces, so re-check it then.

- **Effort:** `small`
- **Depends on:** T-10
- **Scope:** deferred (future / not a v1 blocker) — the existing wording is accurate for both drivers as they stand

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

Investigate Codex's execpolicy `.rules` system ([OQ-3](#oq-3-what-is-the-codex-rules-execpolicy-format)) to see whether it restores native per-command deny fidelity in a fail-closed form. If it does, it is a candidate hardening for the hook-carried guardrails and possibly a cheaper answer than the follow-on shim project. Discovery task, sequenced independently.

- **Effort:** `small`
- **Depends on:** none
- **Scope:** deferred (future / not a v1 blocker) — the `PreToolUse` hook covers the requirement in v1; this is a potential simplification

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

- **Depth 0:** T-01, T-08, T-27 — genuinely independent. **Start T-01 first regardless of slack:** it is the only hard gate, it is `small`, and T-11 cannot land without it.
- **Depth 1:** PR #2361 supplies the in-flight turn-boundary routing; T-12 follows T-11 and that PR.
- **Depth 2:** T-12 supplies the Codex normaliser. T-13, T-14, T-16, and T-17 follow their stated edges.
- **Not in this graph:** T-02 and T-03 belong to the follow-on `PATH`-shim project and are independent of everything above.

**File-overlap cautions — order these rather than running them concurrently:**

- **T-12 and T-13** both edit the driver normalisers. Land T-12 first; T-13 integrates rather than replaces its mappings.
- **T-02 and T-03** both edit `worker_setup.rs` guard-script emission and `BOSS_BIN_DIR` provisioning. The dependency edge serialises them; keep it. Both also collide with **T-11**, which makes the same guard-script emission driver-supplied — a further reason the shim work is better done as a follow-on project than concurrently.

T-09 is a deliberate barrier: it touches nearly every engine call site, so nothing else should be in flight against those files while it lands.
