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
- **Reconciled to as-built, 2026-08-01:** the driver has shipped. This revision rewrites every section the implementation moved rather than leaving the original plan standing beside it — the execution shape (`codex exec` retired for the bare TUI, mono#2578), auth (symlink rejected for snapshot-with-refresh-adoption, mono#2405), guardrail trust (landed and verified live, mono#2408 / mono#2547 / mono#2561 / mono#2598), the progress dialect (stdout removed, mono#2572; rollout is a JavaScript-cell harness, not a shell call, mono#2546), control verbs (on the trait, pane-delivered), and the readoption/checkpoint contract a persistent session forced (mono#2565). Where the plan was wrong, this doc now says what shipped and why it diverged.

## TL;DR / verdict

Codex is a **better** fit for the P1422 abstraction than the abstraction currently assumes, and a **worse** fit for the parts of Boss that never went through the abstraction at all.

The brief's highest-severity claim — _"Codex has no Stop hook, so a Codex worker would never complete"_ — **is wrong, but the conclusion it drives is still right, for a different reason.** Codex emits `turn.started` / `turn.completed` as native, typed events in its structured event stream, so turn boundaries are strictly _better_ than Claude's (in-band and structural, not a hook that must be installed). That stream reaches Boss over the rollout file the engine tails under the run-private `CODEX_HOME` (`ProgressIngress::AgentJsonlFile`, `engine/driver/src/codex.rs:1501`), not over `codex exec --json` stdout — see [Chosen approach](#chosen-approach) and [Alternative 8](#alternative-8-keep---json-on-the-codex-exec-spawn-line-for-progress-transport). Codex also ships a stable, Claude-wire-compatible hooks system — including a `Stop` hook.

The real blocker was one layer down: **Boss's only production progress ingress was a unix socket fed by the `boss-event` shim.** PR #2363 added a generic stdout JSONL reader, but that reader only helps directly when the **engine owns the pipe/pty master**. Under the pane-hosted Boss shape the **app** owns the pty and the engine receives only `shell_pid`; an outsider with only `shell_pid` **cannot** read that stdout on macOS (pane-viability spike Q1). The resolved design therefore adds a distinct engine-side `AgentJsonlFile` transport that tails the raw Codex rollout under the run-private `CODEX_HOME` and feeds the same reader/fan-out.

Second finding, revised on 0.145.0: **Codex hooks do fire under `codex exec`, and `PreToolUse` deny genuinely blocks a command before it runs.** On 0.137.0 no hook fired in nine configurations; on 0.145.0 the _identical_ configuration fires reliably, with Claude-shaped payloads. **This is the mechanism the Codex driver uses.** `ToolUseInterception` is therefore Codex's chosen guardrail carrier, not a degraded fallback: Codex reaches parity on the mechanism already running in production for Claude, with no new guardrail substrate to build, validate, and cut over first. That is the simplest incremental path, and it is the one being taken — by [operator decision](#operator-decision), which overturned this doc's original recommendation.

What that leaves to settle is narrower, and it is real: hooks fail **open and silently** in two independent ways — an untrusted hook is skipped with no warning, and a hook whose command does not exist produces no diagnostic. So the guarantee rests on Boss provisioning hook trust deterministically and being able to tell when a hook did not run. That is [OQ-1](#oq-1-hook-trust-provisioning) / [T-01](#t-01-codex-hook-trust-provisioning), which the decision moves onto the critical path ahead of the first Codex worker.

**What actually shipped, against that verdict.** Both findings held. Hook-carried interception is the mechanism, and the trust gate that qualified it is closed rather than carried as residual risk: Boss stamps `[hooks.state]` `trusted_hash` itself, observes `trustStatus=trusted` over a live `hooks/list` before the worker runs, refuses the spawn otherwise, and detects a silently-skipped guard through its own per-invocation trace rather than through anything Codex emits. The transport verdict held too, and cost more than expected — not because the rollout file was the wrong channel, but because two premises underneath it were wrong. `codex exec` turned out never to have been chosen over the interactive TUI on any recorded ground, and was retired for it; and the rollout records a dispatched model produces are not shell invocations at all but JavaScript cells that yield, get polled, and sometimes are never polled again. Most of the repair work in this project came from those two, not from the capability analysis below.

Third finding, now scoped to a project of its own: a stronger guardrail mechanism already half-exists. Boss prepends `BOSS_BIN_DIR` to the worker's `PATH` (`engine/core/src/runner/pane_spawn.rs:382`). Moving Boss's command-level guardrails from `PreToolUse` hooks into **`PATH` shims** would make them driver-agnostic, make them fail **closed**, and close a real hole in the Claude path — a hook cannot see a command run inside a subshell. That argument stands on its own merits and the analysis below is retained in full. It is a **follow-on project sequenced after this one**, not a prerequisite for it. See [Guardrail integrity](#guardrail-integrity).

## Goals

- Add OpenAI Codex as a real driver behind the P1422 agent-driver abstraction, so a work item dispatched with `--driver codex` runs end-to-end to a PR with the same lifecycle guarantees a Claude worker has today.
- Produce a **complete gap analysis** — the primary deliverable. Where Codex does not fit the current trait surface, name the abstraction gap and specify the fix _in the abstraction_, never as Codex-specific special-casing in the engine.
- Feed those findings back into P1422's remaining tasks. This project and P1422 are deliberately co-dependent; the [Proposed P1422 amendments](#proposed-p1422-amendments) section is the handoff.
- Identify the seams a future Codex/Claude load balancer will need, so this work does not foreclose it.

## Non-goals

- **Implementing the load balancer.** Out of scope by operator direction. This doc identifies the seams it attaches to and specifies nothing about policy.
- **Removing or de-privileging the Claude path.** Claude remains the reference driver and the default.
- **Codex Cloud, `codex app-server`, `codex mcp-server`, `codex remote-control`.** v1 drives the bare interactive `codex` TUI only (retired `codex exec` — see [Alternative 4](#alternative-4-the-interactive-tui-bare-codex-no-subcommand) and `docs/investigations/codex-tui-pivot-pricing-2026-07-30.md`). The app-server is a strictly richer surface and a plausible v2 (see [Alternative 3](#alternative-3-drive-codex-app-server-over-json-rpc)).
- **Driver-aware kanban / Swift UI.** The kanban already reads abstract `WorkerActivity`; nothing in the product surface needs to know which driver ran. The app still owns GhosttyKit, but the chosen rollout transport is engine-only and does not add surface scraping or app IPC. **Held, with one correction found in implementation:** the app's pane monitor scrapes rendered surface text for per-CLI marker literals, and those literals are driver-specific by nature. `PaneMonitorSpec` was already a driver-supplied contract; Codex simply never declared one, so the app silently fell back to Claude's markers and pinned every Codex pane to `notDetected` (mono#2600). The fix is a Codex-supplied spec, not driver-awareness in the UI — the app still branches on nothing, it just receives the right data. See [Pane monitor markers](#pane-monitor-markers-a-driver-supplied-contract-the-driver-never-filled-in).
- **Remote/SSH dispatch for Codex.** `engine/core/remote/boss-remote-run.sh:84,159,162` is 100% hardcoded Claude. Deferred, and filed as such.
- **Re-litigating the P1422 capability vocabulary.** The 12 capabilities are the right decomposition; this doc changes signatures and adds two, it does not re-open the model.

## Method

Everything about Codex below was established by **running Codex on this host on 2026-07-24**, not from recollection. Where a claim comes from the binary's embedded generated schemas rather than an observed run, it is marked _(binary)_. Where I could not establish something, it is an explicit open question rather than an assertion.

The doc was first written against `0.137.0`. On operator request, **every Codex claim was then re-run against `0.145.0`** — the version now installed — rather than having the version string bumped. The body below states 0.145.0 behaviour; [Version delta](#version-delta-01370--01450) reports what moved, because the churn across eight minor versions is itself a design input.

Boss-side claims were re-verified against `7859b6c4` by locating symbols, not line numbers. **The brief's ground-truth section has already drifted**: the dispatch gate it cites as `engine/core/src/runner.rs:1320-1335` is now `engine/core/src/runner/worker_spawn.rs:597-601` — `runner.rs` has been split into a module directory. Treat the line numbers in _this_ doc the same way.

The spike harness (isolated `CODEX_HOME`, throwaway git repo, hook handler logging its stdin) is reproduced inline in [Appendix A](#appendix-a-reproducing-the-codex-spike).

**The as-built reconciliation has a different method, stated so the two are not confused.** Sections marked as landed, corrected, or superseded were written against the merged implementation — the PRs listed in this project and the code they left behind — not against a fresh Codex run. Where an as-built claim rests on a live measurement, that measurement was taken by the PR that made it and is cited to its investigation doc (`investigations/codex-*`), several of which exist precisely because a claim in the original pass turned out to have been captured on an unrepresentative model or an execution shape Boss no longer spawns. Nothing in the original empirical sections was silently updated to match the implementation: where the implementation contradicts an earlier finding, both are stated.

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

The launch command quoted above is this design's own original one, and it still carries `--json` because that was the transport choice at the time. `--json` was later dropped for an unrelated reason — see [Alternative 8](#alternative-8-keep---json-on-the-codex-exec-spawn-line-for-progress-transport) and [Chosen approach](#chosen-approach) — so no command Boss actually spawns carries `--json` today.

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

The table above is `codex exec --help`'s full flag surface, not Boss's chosen invocation (which no longer drives `exec` at all — see [Chosen approach](#chosen-approach)). Two rows in particular were never on the spawn line Boss actually uses: `--json` is forbidden outright (`CODEX_FORBIDDEN_LONG_FLAGS`, `engine/core/src/conformance/fixtures.rs`) because production progress ingress tails the rollout file instead (`ProgressIngress::AgentJsonlFile`, `engine/driver/src/codex.rs`); `-o, --output-last-message` is not passed either — `CodexDriver::structured_output_wiring` (`engine/driver/src/codex.rs:1732`) uses only the common-denominator `BOSS_STRUCTURED_OUTPUT` environment-file contract today and names `--output-last-message` as a possible future extension ([T-15](#t-15-structuredoutput-trait-method-and---output-schema-wiring-a-5)), not something wired in yet. See [Chosen approach](#chosen-approach).

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

Agent-rules file is **`AGENTS.md`**, not `CLAUDE.md` — abstracted as `DriverDescriptor.config_dir` / `agent_rules_preamble` (`engine/driver/src/lib.rs`).

**The filename was abstracted; the destination was not, and that broke every dispatch.** `write_workspace_files` wrote the rules file to `<workspace>/<config_dir>/<agent_rules_filename>` — the shape Claude's `.claude/CLAUDE.md` takes. Codex does not read `<workspace>/.codex/AGENTS.md` at all, so Codex workers ran with no Boss rules in their prompt for the first several dispatch attempts. Verified with `codex debug prompt-input`: a marker in a root `AGENTS.md` reaches the model-visible prompt input, the same marker in `.codex/AGENTS.md` does not, and `$CODEX_HOME/AGENTS.md` **is** read as user-level instructions, concatenated ahead of any project-level `AGENTS.md` behind Codex's own `--- project-doc ---` separator. The fix added `AgentDriver::agent_rules_destination(&self, workspace, run_id) -> PathBuf` (default unchanged, so no other driver moved) and returns `$CODEX_HOME/AGENTS.md` for Codex (mono#2447) — which also keeps the file out of the jj-tracked tree entirely and composes with a repo's own `AGENTS.md` instead of clobbering it. This is the general lesson of that PR: `config_dir` abstracted the _name_ of a per-driver convention while the engine still assumed its _shape_, and nothing failed loudly.

### Auth, and coexistence with Claude

`codex doctor` reports: `auth storage mode File`, `auth file ~/.codex/auth.json`, `stored auth mode chatgpt`, `stored ChatGPT tokens true`, `stored API key false`.

Auth is a **file inside `CODEX_HOME`**, not an environment variable. Consequences:

- A per-worker `CODEX_HOME` must have `auth.json` present.
- There is **no collision** with the `unset ANTHROPIC_API_KEY` line at `engine/core/src/runner/pane_spawn.rs:382`. It is inert for Codex. It is still a Claude-ism sitting in shared spawn code and belongs behind the driver — see [G-1](#g-1-spawn).
- Codex may also authenticate by API key. Boss should not care; it should treat `CODEX_HOME` as opaque auth state and let the operator provision it.

**The spike's symlink is not the shipped policy, and could not have been.** This doc originally said "symlinking the host's `auth.json` is sufficient (that is what the spike did) and avoids duplicating a credential per workspace." That was true of a single sequential spike run and false of concurrent workers, because **Codex rewrites `$CODEX_HOME/auth.json` in place on OAuth refresh** — probed live by forcing an expired `access_token` against a valid `refresh_token` and watching the per-run file's fingerprint and `last_refresh` advance (`investigations/codex-auth-isolation-2026-07-26.md`, mono#2405). A symlink makes that rewrite land on the operator's interactive login file, shared by every concurrent worker, with no serialisation. Making the run-local copy read-only is not an escape either: mode `0444` produces `Failed to refresh token: Permission denied (os error 13)` and the run dies.

The shipped policy is **`SnapshotWithRefreshAdoption`** (`tools/boss/codex_auth`, crate `boss-codex-auth`):

- **Provision** — take an exclusive lock beside the source, validate the credential's JSON shape, byte-copy it into the per-run `CODEX_HOME/auth.json` as a regular file at mode `0600` (replacing any pre-existing symlink), created `O_EXCL` in the parent and renamed into place so there is never a umask-readable window.
- **Run** — leave the copy writable, so a mid-run refresh persists into the isolated home and nowhere else.
- **Teardown** — re-lock, and if the run's file fingerprint changed and its `last_refresh` is strictly newer than the source's, atomically adopt the rotated bytes back into the source. Symlinked sources and symlinked per-run files are refused rather than followed, re-checked under the lock to close the TOCTOU.
- Logging carries paths, policy name, SHA-256 fingerprints and `last_refresh` only — never token material.

This is the one place the design's isolation claim ("per-worker `CODEX_HOME` is not an optimisation; it is required for correctness") turned out to be understated: it is required for the credential too, not just the trust registry. It also created a dependency that was initially dead — teardown is where adoption happens, and teardown never ran on the ordinary PR-success path (see [G-2](#g-2-workspaceprovisioning)).

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

**Deny-only means there is no allow token at all — the allow path is silence.** The list above records what Codex _rejects_; it left unstated what a guard should emit to let a call through, and the first Codex guards shipped emitting Claude's `{"decision":"approve"}` on that path, which produced one hook error per guard on every tool call. Measured against 0.145.0: the accepted allow response is **no output** (`{}` also works), the accepted refusals are `{"decision":"block","reason":<non-empty>}` and `permissionDecision:deny` + `permissionDecisionReason`, and two further traps apply — `decision:deny` is _not_ a synonym for `block` and is rejected, and a `block` whose reason is missing or empty is rejected too. Because a rejected response is fail-open, an unexplained refusal silently runs the call. Full matrix and post-fix verification: [`investigations/codex-pretooluse-decision-vocabulary-2026-07-30.md`](../investigations/codex-pretooluse-decision-vocabulary-2026-07-30.md).

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

Every Boss workspace contains a `.claude/` directory (gitignored by the engine, per `.claude/CLAUDE.md`) with settings and hook wiring for the Claude path. **A Codex worker launched in that workspace may detect it and offer to import it.** That is a real collision in principle — Boss's Claude hook config referencing the `boss-event` shim is exactly the wrong thing for Codex to adopt — and the Codex driver must not write `.claude/` at all.

**Measured rather than inferred, and the hazard is smaller than the binary strings suggest** (mono#2447). Two corrections:

- **The suppression key is not top-level.** `external_config_migration_prompts = false` as a root boolean does not exist in Codex's schema at any version; it is `notice.external_config_migration_prompts`, a table with `home`, `home_last_prompted_at`, `projects`, `project_last_prompted_at` (`codex-rs/config/src/types.rs` in `openai/codex`). Boss emitted the invalid top-level form, `--strict-config` rejected the unknown field, and **every Codex worker died at config load** — the first of the three blockers that made every dispatch fail. The driver now writes `notice.external_config_migration_prompts.home = true` plus a per-project entry.
- **That key only gates a notice, and the import itself is unreachable from the CLI.** The actual import runs in `codex-rs/app-server/src/external_agent_migration/processor.rs` and only in response to an explicit `externalAgentConfig/detect` / `/import` app-server JSON-RPC request, additionally gated on the `external_agent_memory_import` feature flag (`false` by default on 0.145.0). Neither the `exec` shape nor the bare TUI issues either request. Verified behaviourally: a marker planted in a workspace's `.claude/CLAUDE.md` never appears in `codex debug prompt-input`'s model-visible prompt input, with or without these keys set. Boss pins `[features] external_agent_memory_import = false` anyway, as belt-and-braces against a default flip, but the leak path this section originally worried about was never open on the shape Boss spawns.

See [Migration and coexistence](#migration-and-coexistence).

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

One gap, correctly identified and considerably less minor than this doc first rated it: the trait gave no hook for **teardown**. A per-run `CODEX_HOME` accumulates rollout files (the host's `~/.codex` held 279 active rollouts / 323 MB when this was written). Claude needed no teardown so none was designed.

**As built.** `provision_workspace` records `CodexRuntimeState.codex_home` on the execution as opaque `driver_runtime_state`, and a `set_driver_runtime_state` failure **fails the spawn** — a home the engine cannot name later is a home teardown can never find. `teardown_driver_workspace` then adopts any mid-run auth refresh back into the source credential and reclaims the home, under canonicalised containment checks against the homes root (`$BOSS_CODEX_HOMES_DIR`, else `$TMPDIR/boss-codex-homes`); it never scans `~/.codex`, never infers a home from the engine's own environment, and never deletes a directory it did not record.

**Teardown existing was not the same as teardown running, and the difference was invisible for weeks.** `teardown_driver_workspace` was only reached from the reap/cancel paths — `dead_pid_sweep`'s reaper, `force_release`, and the automation-triage/answer-agent finalizers. Every Stop-hook path that terminalizes a _parked-live_ execution — the ordinary PR-success completion, the reviewer-pass completion, the no-op completion, the idle-abandon park — nulled `workspace_path` in the same transaction that marked the execution complete and never called teardown at all. So on the path a healthy Codex worker actually takes, **the entire refresh-adoption half of the auth contract was dead code**, and nothing said so: the failure signature was the absence of a log line. The fix captures `workspace_path` before each terminalizing DB call and tears down in all four finalizers, ordered _after_ pane release so a still-live worker refreshing its token cannot race the adoption, plus an unconditional entry-level trace so "did teardown run for this execution?" is answerable directly instead of inferred from a driver outcome line that may never fire (mono#2545).

**Retention is a policy, not a delete-on-exit.** A terminal run's home is deliberately _retained_ as forensic evidence — the rollout is the transcript. An hourly sweep (also `bossctl codex-homes sweep`) loads only recorded roots, classifies live vs terminal from **execution status rather than mtime**, and reclaims homes that are terminal and older than 14 days, or in the oldest terminal set once total retained size exceeds 500 MiB (mono#2422). This resolves [OQ-4](#risks--open-questions).

The abstraction lesson generalises past Codex: a driver that owns per-run state outside the workspace needs (a) a place to record what it owns, (b) a teardown hook, and (c) **a wiring test per terminalization path**, because "the hook exists" and "the hook is called on every path that ends a run" are different claims and only the second one is the contract.

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
3. **Topology change** — engine owns spawn/pipe (or otherwise receives a real stream fd). Would have made `StdoutJsonl` + #2363 sufficient, but changes the pane-hosted Boss shape the agent-driver-abstraction project's non-goals hold fixed — **not pursued**; see [Chosen approach](#chosen-approach).

**PR-URL capture dialect depends on which channel lands.** Stdout `command_execution` items carry `aggregated_output` (regex-friendly). Rollout encodes the same tool runs as `custom_tool_call` / `custom_tool_call_output` — not a drop-in for the same extractor. A driver-supplied PR-URL path must declare which dialect it reads.

A related, smaller problem: `WorkerEvent` requires `session_id` on every variant (`protocol/src/worker_event.rs`), and `SessionStartSource` mirrors Claude's `startup|resume|compact`. Codex's identity is `thread_id`, and its `SessionStart` trigger set _(binary)_ is `startup|resume|clear|compact` — a superset. Both need widening.

Finally, `progress_fidelity()` is now registered per live-worker slot and consulted by the stale-worker sweep through `ProgressFidelity::stale_threshold_secs` (`live_worker_state.rs:165-177`; `stale_worker_sweep.rs:290-291`).

A related but distinct observability gap — a denied write inside an already-allowed command is invisible to exit status on either dialect — is written up on its own below: [Sandbox denials are invisible to exit status alone](#sandbox-denials-are-invisible-to-exit-status-alone--a-distinct-failure-signal-is-needed).

**Reconsideration — `ProgressFidelity::Rich` overclaims per-command outcome, not just cadence.** `CodexDriver::progress_fidelity()` (`engine/driver/src/codex.rs`) declares `Rich` on the grounds that `item.started`/`item.completed` give the same per-tool cadence as Claude's hooks — that comparison is correct, but it is a claim about _cadence_ only. It says nothing about whether the terminal record for a given command reliably tells Boss whether the command succeeded.

It does not. At the time this was written, the `("item.completed", "command_execution")` normaliser read only `item.command` and `item.aggregated_output`; the raw envelope also carries `exit_code` and `status` (visible in this doc's own captured fixtures, e.g. the raw stream excerpt earlier in this doc), and neither field was parsed anywhere in the crate. Even if they were, they would not be a dependable signal: the exit code is only sometimes present, the model's own result-projection layer can drop it before the rollout record is ever written, and once output is truncated the record becomes unparseable for this purpose. Claude has no equivalent gap — its `PostToolUse.tool_response` is a stable per-command outcome surface.

Collapsing "reports activity" and "reports per-command exit status" into one `Rich` tier lets a consumer — most concretely a future cross-driver scheduler — assume a guarantee Codex does not carry. The fix is a capability, not a fidelity-tier rename: **`Capability::CommandOutcomeObservation`**, declared by Claude and left undeclared by Codex (and, absent equivalent evidence, by Grok), with the standard `AwaitingInputSignal`-shaped contract — absence is `Degrade`, never `Synthesize`, so Boss never guesses a command's outcome from activity alone. `ProgressFidelity::Rich` keeps its existing meaning (cadence) and now says so explicitly in its doc comment; the outcome claim moves to the new capability. This is implemented in this revision (`engine/driver/src/lib.rs`, `codex.rs`, `claude.rs`, `grok.rs`).

This also names the load-balancing seam ahead of time (see [Load-balancing seams](#load-balancing-seams)): a normalised per-command outcome will need an explicit "observed" bit, because Codex's unobserved state has no Claude counterpart — a balancer that treats "no signal" as "succeeded" for both drivers alike would silently be wrong for exactly the driver most likely to need load-balancing.

### The rollout records cells, not commands — the largest single divergence from this design

Every claim above about `command_execution`, `aggregated_output` and per-command exit status was derived from `gpt-5.5`, which has no code mode. **Every model Boss actually dispatches (`gpt-5.6-terra`, `gpt-5.6-sol`) runs `tool_mode = code_mode`,** and under code mode the model does not issue a shell call at all. It writes JavaScript — `tools.exec_command({cmd, workdir, yield_time_ms, max_output_tokens})` — inside a _cell_, choosing that command's timeout and output budget itself, and then authors its own projection of the result. The rollout therefore records the cell, not the command, and the two do not have the same lifetime.

Four consequences, each of which produced a real defect before it was understood (`investigations/codex-exit-code-surfacing.md`, mono#2507; fixes in mono#2509, mono#2519, mono#2546):

- **`Script completed` is not an exit status.** It says the _cell_ finished. A cell that yields returns `Script running with cell ID N`; the command's real output arrives later on a separate `function_call name="wait"` that polls that cell. A completed cell whose forwarded chunk carries no `exit_code` means the command was still running and the model stopped looking.
- **The command's exit code is inside the chunk, behind prose.** `canonical_rollout_tool_output` derived `is_error` from a nested `metadata.exit_code` that this CLI never emits; the real value sits at the top level of a JSON object embedded as text in a content-block array, behind a `Script completed` / `Wall time N seconds` / `Output:` header. Across twelve tool-output records in eight probes Boss classified **zero** as errors — including exits 7, 9 and 137.
- **A command can simply never be observed.** If the command outlives the model-chosen `yield_time_ms` and the model never polls again, no completion record is produced _anywhere_: no `exit_code` reaches the model, and the turn still completes cleanly. A worker reporting "validation passed" on such a command is not lying to Boss; nothing in the stream contradicted it.
- **The reported "command" was the whole script.** The tool input is JavaScript source, not `{cmd: …}`, so parsing failed and the entire cell body became the command string in live status, operator notifications, and the editorial audit.

**The shipped shape makes the cell the unit of correlation and the chunk's `exit_code` the terminal signal for the command.** `boss-engine-codex-rollout`'s `cell.rs` owns the dialect facts (parse the yield/completed envelope and its forwarded payload, decide whether a completed cell's chunk reports an exit code, read a `wait` call's target `cell_id`, recover the shell command out of the JS by brace-balancing `tools.exec_command({…})`); `driver/src/codex/rollout_calls.rs`'s `RolloutCallTracker` keeps a call **open** until a continuation delivers a result terminal for the command, binds a `wait` to the cell it polls, and completes the _originating_ call — so downstream consumers see the real command paired with the real output and never learn a `wait` was involved. A call still open at a turn boundary drains as a `WorkerEvent::Notification` carrying `UNOBSERVED_COMMAND_MARKER`; the engine files an attention item naming the command and **refuses that execution's `NO_CHANGES_NEEDED` claim**, falling through to the ordinary produce-a-PR nudge (mono#2519). A correlation failure — a `wait` with no `cell_id`, or naming a cell this session never saw yield — emits the same marker rather than a `NormalizeError`, because the generic reader treats any normalise error as the expected steady state and logs it at `debug`.

Two things this cost that the design did not anticipate. **PR-URL capture was structurally dead on the primary path:** `pr_url_capture_feed` gates on the normalised tool name being `Bash`, `exec`/`exec_command` are reshaped to `Bash`, but `wait` fell through — so the record actually carrying the `cube pr create` URL was attributed to a tool named `wait` and dropped, while the record that _was_ fed to capture held only the yield placeholder. Because a cold `repobin` build of `//tools/cube:cube` routinely pushes `cube pr create` past the model's chosen return window (20.9 s on the observed execution), every Codex PR URL depended on the fallback artifact the worker optionally writes. And **one honest limitation survives:** a cell that hands its shell session to a separate `tools.write_stdin` cell reporting the exit code under its own call id leaves the first call open and is reported as unobserved. That is correct rather than wrong — the continuation carries no `cmd` and its session id appears only in a prior cell's _output_, so there is no attributable link, and Boss genuinely did not observe that command's outcome.

**The generalisable finding for the abstraction:** a driver's "tool call" record is not guaranteed to be one call with one result. A harness that yields, gets polled, and can be abandoned needs an explicit _correlation_ stage between the raw dialect and `WorkerEvent`, with an "I could not confirm this" outcome that is a first-class signal rather than a parse error. Boss's normaliser interface assumed one record in, zero-or-more events out, statelessly; Codex needed stateful correlation, and every consumer that reasoned from a single record was wrong in the same direction — toward believing a command succeeded.

### Ingress must survive an engine restart, and a persistent session is what made that mandatory

`ServerState::readopt_live_worker` restored three things after an engine restart and never re-established progress ingress. Under the retired one-turn-per-process shape that was near-harmless — the process was about to exit anyway. Under a long-lived TUI session it is fatal: the worker comes back alive and **unobserved** — no tail, therefore no turn boundary, therefore no completion — holding a fleet slot and a cube workspace lease indefinitely (mono#2565).

The shipped contract adds durable state and refuses to guess:

- **`work_runs.progress_ingress_checkpoint`** (JSON) records `not_file_ingress` / `armed` / `attached`. It sits on `work_runs` rather than `work_executions` for the same reason `turn_boundary_at` does — it describes one spawned process's rollout file, so a later run must never inherit an earlier one's offset. The absence of a row keeps meaning "no engine ever armed an ingress here" rather than doubling as "this driver doesn't tail a file".
- **The checkpoint is written after dispatch, at record granularity.** Writing it ahead of dispatch would let a crash in the gap silently skip a record — and a skipped `Stop` is a turn boundary that never happens, which is the exact failure the change exists to prevent. "After the event the offset belongs to" is per _record_, not per envelope: one Codex `task_complete` line can normalise to `[Notification.., Stop]`, and taking the offset from the first would persist the whole line while the `Stop` was still queued. The residual window is one whole record re-read on resume, stated rather than papered over.
- **Nothing falls back.** No attach-at-zero (republishes every prior turn) and no attach-at-EOF (discards whatever the worker wrote while the engine was down). A vanished rollout, a different session id, a different file under the same pathname, a file shorter than what was consumed, or an offset that is not immediately past a newline all fail loudly into a `progress_ingress_unrecoverable` attention item. A file-tailing run with **no** checkpoint takes the same loud path, because that is the same unobservable state.
- **`ProgressSessionNormalizer::resume_state` / `restore_resume_state`** make the driver's own session state a driver-owned opaque snapshot taken at the same instant as the byte offset. `CodexRolloutProgressSession` snapshots exactly the fields a fresh session would get wrong: `current_thread_id` (announced once by the head `session_meta`; resuming with `None` makes every later record fail correlation), the open calls from the tracker above **including pending cell and wait bindings**, the guard-trace read cursor (zero re-announces every guard decision already reported), `guards.records_seen` (false-with-tool-calls fabricates the guards-silent alarm), and the in-flight turn's own accounting. Every field is required on the wire — a blanket `serde(default)` would buy only the ability to accept a degenerate `{}` snapshot that deserialises into exactly the state that list calls fatal — and a snapshot the driver rejects is a _failed_ resume, not a degraded one.

This is an amendment to the agent-driver abstraction, not a Codex detail: **any** driver whose `ProgressIngress` is a file tail rather than a push channel needs a durable resume point and a driver-owned session snapshot, and the seam belongs on the trait. Recorded as [A-15](#proposed-p1422-amendments).

### A driver-reported fatal error is not a turn boundary

A Codex `pr_review` worker died 2.3 s into a turn on an HTTP 400 from the model endpoint. Boss read that as a clean turn boundary, declined to orphan the execution, re-prompted the already-dead reviewer indefinitely, and left the row parked on a dead process with its cube workspace still leased — no attention item, nothing surfaced anywhere. Root cause: the rollout adapter never looked at `payload.error` on a `task_complete` envelope, so a fatal provider error and a clean completion were **indistinguishable to the engine** (mono#2521).

Three fixes, and one vocabulary decision worth carrying into any future driver:

- `StopReason::Other` now means specifically "the driver reported an unrecoverable error", and `turn_aborted` moves to the previously unused `StopReason::Interrupted` so it no longer shares that meaning. The rollout adapter emits `Other` when `task_complete.error` is present and non-null.
- The **transcript** normaliser's `task_complete` arm surfaces a fatal error as assistant-visible text rather than the "turn completed" lifecycle filler it emitted when `last_agent_message` was null. That filler is what produced the vague "transcript had no assistant text event" diagnosis while the exact 400 sat in the file the engine had just read.
- The nudge loop gates on the driver-supplied `TurnEnd`: a `StopReason::Other` fails the execution immediately — before any kind-specific finalizer or the generic produce-a-PR nudge — marking it `failed` rather than `orphaned` (the process is not resumable; it exited on a definitive error), releasing the cube lease, and filing a `driver_terminal_error` attention item naming the provider's own diagnostic text.

The general shape: **the engine's completion path must be able to distinguish "the agent finished" from "the agent's provider killed it", and only the driver can tell it which.** A `Stop` with no reason attached is an assertion of health that no driver was asked to make.

### Pane monitor markers: a driver-supplied contract the driver never filled in

`CodexDriver` declared no `pane_monitor_spec()`, so it took the trait default of `None`, the spawn RPC carried `pane_monitor: null`, and the app fell back to `PaneMonitorSpec.claudeDefault`. Claude's agent markers (`"Claude Code"`, `"auto mode on"`, `"/effort"`) cannot render in a Codex pane, so **every Codex worker's pane monitor was pinned to `notDetected`** — while Claude's busy marker (`"esc to interrupt"`) matched Codex verbatim. Detection that can never succeed sitting next to a busy signal that always matches is the confidently-wrong state the marker discipline exists to prevent (mono#2600).

The declared spec targets the bare TUI, and two of its five fields were changed by measurement rather than transcribed from the pivot spike's capture — 910 viewport polls across three live sessions (`investigations/codex-tui-liveness-marker-stability-2026-07-31.md`):

- **The startup banner scrolls out and never returns.** Under `--no-alt-screen` the header box is ordinary scrollback: last seen at poll 25/130 in one run, 61/400 in another, absent from every poll after. A spec whose agent markers were only the banner would go `notDetected` a few seconds into the first turn — the same defect, delayed. The composer prefix `›` is what holds detection (909/910 polls, including under heavy tool output); the banner literals stay in the set for their precision during startup.
- **`permissions:` never rendered** (0/910), so it was dropped rather than declared on the strength of one earlier capture.

The abstraction point: a trait method with a `None` default is an _unfilled_ contract that fails silently, and a fallback keyed to the reference driver turns that silence into a wrong answer rather than no answer. This is the same failure shape as `agent_rules_destination` above.

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

**What declaring this capability actually cost.** The declaration was made on the strength of a payload capture that was correct and unrepresentative, and four separate repairs were needed before the guarantee it asserts was true. They are worth recording together, because each is a distinct class of "the mechanism works" not implying "the guardrail holds":

1. **The capture was taken on the wrong model.** All the hook evidence in this doc came from `gpt-5.5`, which has no code mode; every model Boss dispatches is a code-mode model. Re-measured on `gpt-5.6-terra` / `gpt-5.6-sol`, the two stated doubts resolved in Boss's favour — `tool_name` is still `Bash` and `tool_input` is still `{"command": …}`, because Codex hooks the _inner_ `tools.exec_command` call rather than the JavaScript cell wrapping it — but three other things were genuinely wrong (`investigations/codex-pretooluse-guard-coverage-2026-07-29.md`, mono#2547).
2. **Codex's tool surface is wider than Claude's, in ways a `Bash` matcher structurally cannot reach.** `apply_patch` carries its target paths in the patch body under its own tool name with no `file_path` key, so the Boss data-directory gate ran and approved every Codex file edit unread; `mcp__codex_apps__github_*` can open, push to and merge PRs with no shell command for any command guard to see; and `tools.write_stdin` feeds new command lines to an already-running process and **fires no hook at all**, which no matcher change can reach — only refusing the interactive session that would receive them. All three are closed: the path gate parses `apply_patch` headers, a Codex-only tool-surface guard denies every `mcp__*` call by default (deny-by-default rather than an allowlist, because Codex's app catalog drifts and Boss injects no MCP tooling of its own), and invocations whose only effect is to open a stdin-driven command channel are denied per-interpreter.
3. **The guards spoke the wrong dialect, and the wrong dialect is fail-open.** Every tool call emitted five hook errors — `PreToolUse hook returned unsupported decision:approve`, one per armed guard — because Boss's guard bodies are written in Claude's hook dialect and the shim re-emitted their stdout byte-for-byte. Measuring the full response matrix against the live binary established what this doc had left unstated: **there is no affirmative allow token; emitting nothing and exiting 0 is the only accepted way to say "proceed"**, and two shapes that look like refusals are rejected — `{"decision":"block"}` with a missing or empty reason, and `decision:deny` (which is _not_ a synonym for `block`, though `permissionDecision:deny` is accepted). Because a rejected response is fail-open, an unexplained refusal silently ran the call. Translation now happens at the single choke point every Codex guard passes through, mirroring the Grok driver's `translate_decision`, and the guard bodies stay in Claude's dialect because two of them are shared verbatim with the Claude path (mono#2598, `investigations/codex-pretooluse-decision-vocabulary-2026-07-30.md`).
4. **Guards must fail closed, and Boss must be able to see that they ran.** The guard bodies previously did `inp.get('tool_input',{}).get('command','')`, which raises when `tool_input` is a bare string — and a guard that dies is read by Codex as approval. They now block with an explicit reason on any payload they cannot read as a shell command. Every materialised guard also runs under a trace shim that records one JSON line per invocation to `$CODEX_HOME/guard-trace.jsonl` (guard, tool, decision, reason head, exit code, session id, `tool_use_id`, `tool_input` key set — never values), re-verifies the guard's bytes against a hash baked into its wrapper, imposes its own wall-clock budget, and converts a crash, a non-zero exit, an overrun or unparseable output into a loud `block`. The wrapper in turn verifies the shim's own digest before exec'ing it, so the trust chain is anchored at both joints rather than leaving the shim as the one unhashed link.

**And the observability reasoning did not survive the pivot to a session.** `drain_guard_trace_notifications` suppressed the silent-guards signal once any guard record had been read, on the reasoning that "once a guard has been seen to run, the hooks are armed and reachable for the rest of the run". Under one-turn-per-process that window was the tail of one turn; under a session it is hours, and it is false — demonstrated live, one TUI session, two turns, `$CODEX_HOME/guards` removed between them: turn 1 recorded five guard decisions, turn 2 ran unguarded with zero records, Codex said nothing, and the latch would have kept Boss quiet for the rest of the session. The signal now **asks disk rather than history**, re-checking at every turn boundary that every hook `command` named by the arming attestation is still a regular executable whose bytes still hash to the attested value, and reporting `[codex-guards-silent]` every turn the chain stays broken (mono#2561). `guard_records_seen` keeps only its original job: stopping a code-mode cell that invokes no inner tool from raising a false alarm.

**Finally, the same measurement is now a build gate.** `conformance/guard_conformance.rs` checks every model the driver can dispatch against a checked-in `tool_mode` fixture — hermetically, so it enforces under `bazel test`'s sandboxed `PATH` instead of soft-skipping to a silent pass — with a live companion that re-fetches `codex debug models` and fails on drift, and an opt-in probe that drives the real driver through `provision_workspace` → `write_permission_config` → `spawn_invocation` and asserts the observed `(tool_name, tool_input key set, aggregate guard decision, guard-name set)` per step (mono#2563). The drift that started this — evidence captured on a model Boss does not dispatch — now breaks the build.

`ToolUseInterception` denies a whole command _before_ it starts. It has nothing to say about a denial that happens _inside_ an already-allowed command, at the syscall level — that is a different mechanism (the OS sandbox, [G-3](#g-3-permissionpolicy)) with a different observability problem, covered next.

### Sandbox denials are invisible to exit status alone — a distinct failure signal is needed

Verified empirically, 2026-07-29, codex-cli 0.145.0 (harness matches [Appendix A](#appendix-a-reproducing-the-codex-spike), extended below). This is not a 13th capability — the non-goals section is explicit that the 12-capability vocabulary is fixed, and `G-1`–`G-12` already map 1:1 onto it — it is a cross-cutting gap inside two capabilities already declared: [G-3](#g-3-permissionpolicy) `PermissionPolicy` (which is what actually denies the write) and [G-5](#g-5-progressobservation--the-top-gap) `ProgressObservation` (which is where the denial should have become visible, and doesn't).

**The finding.** Under `--sandbox read-only` (Boss's Reviewer mode), a denied filesystem write does not fail the command it was issued in. A compound shell invocation continues past the denial and reports success:

```jsonl
{
  "type": "item.completed",
  "item": {
    "id": "item_0",
    "type": "command_execution",
    "command": "/bin/zsh -lc 'touch denied.txt; echo \"exit:$?\"'",
    "aggregated_output": "touch: denied.txt: Operation not permitted\nexit:1\n",
    "exit_code": 0,
    "status": "completed"
  }
}
```

`exit_code: 0` / `status: "completed"` is Codex's own honest report of the _outer_ shell invocation: `echo` ran and exited zero, so the compound command as a whole "succeeded." The denied `touch` and its own real exit code (`1`, printed by the prompt's own `echo "exit:$?"`) are visible only as free text buried inside `aggregated_output`. Nothing in the envelope says "a write was refused." The rollout dialect shows the identical shape one layer further removed — the `exec_command` tool's own textual wrapper (`"Process exited with code 0\n...\nOutput:\ntouch: denied.txt: Operation not permitted\n"`) reports the same top-level zero, with the denial only as prose inside `output`.

This is not an artifact of that one test — it is a structural property of shell composition, confirmed by the control case. Run the identical denied `touch` as the _only_ statement (nothing composed after it) and Codex reports it faithfully: `"exit_code":1,"status":"failed"`. Codex is not suppressing the per-command result; the ordinary case works correctly. The failure mode is specifically **a compound command whose last statement succeeds after an earlier statement was silently denied** — exactly the shape an agent's own multi-step shell scripts take (`mkdir -p x && cd x && touch y`, a script with several writes and a final `echo done`, etc.). Any consumer that infers "did this command fail" from the outer `exit_code`/`status` — the only signal the stdout normaliser threads through today, and today it doesn't even do that, see below — will pass a run whose writes were all silently refused.

**Is there a structural denial event instead?** No. Checked both live dialects (above) and the binary's own embedded string table (`strings` against the installed `codex-cli 0.145.0`) for a per-command sandbox-denial marker: no `sandbox_denied`, no field on `command_execution` / `function_call_output` naming which syscall the seatbelt profile refused. The only sandbox-named identifiers in the binary (`sandboxError`, `SandboxLandlock`, `SandboxExecutableNotProvided`) are **turn/session-level** catastrophic-failure codes — "no sandbox executor is available on this host" — not a per-syscall runtime signal, and irrelevant to a healthy turn that silently ate a denied write. This is the structural reason the mechanism can't be fixed by "read a different field": Codex's own agent loop is not told a seatbelt denial happened either. The sandbox enforces below the level Codex's process supervision can see, by design — the same property [G-3](#g-3-permissionpolicy) already calls "OS-enforced... rather than advisory," which is a real strength for _preventing_ the write and a real weakness for _reporting_ that prevention.

**Why this differs from [G-6](#g-6-tooluseinterception)'s `ToolUseInterception`.** A `PreToolUse` deny (Claude's or Codex's own) is a _whole-command_ refusal decided _before_ the process starts — the tool never runs, so there is nothing for an exit code to misreport. What this section describes happens _inside_ an already-allowed command, at the syscall level, _during_ execution — a granularity no hook, on either driver, observes. Boss's guardrail carrier (hooks) and Boss's sandbox (OS enforcement) are two different mechanisms with two different failure-observability properties, and this gap belongs to the second one. A future driver with syscall-granular OS sandboxing (Codex today; plausibly others) hits the identical shape, which is why this is written up as an abstraction gap rather than a Codex-only quirk.

**Decision.** Do not try to make exit status trustworthy here — that would require Codex to change what it reports for compound shell commands, which is out of Boss's control and arguably not even wrong (the outer command genuinely did exit 0). Instead:

1. Thread the `command_execution` item's own `exit_code` / `status` fields through the stdout-dialect normaliser instead of silently discarding them (today they are read off the raw envelope and then dropped before `WorkerEvent::PostToolUse` is built). This is a real, independent bug — even the _non_-masked case (a denied command with nothing composed after it, `status:"failed"`) is invisible to any typed consumer today — and fixing it is free once this code path is touched.
2. Add a **best-effort, Codex-driver-local heuristic** that scans `command_execution` / tool-output text for known OS write-denial phrasings (verified: macOS Seatbelt's `Operation not permitted` on a denied write) and, on a match — or on a genuine `status:"failed"` from (1) — emits an _additional_ `WorkerEvent::Notification` alongside the ordinary `PostToolUse`. This reuses the exact channel Codex already uses for its own operational warnings ([D-6](#stream-drift--all-silent-all-additive), the hook-trust-bypass notice) rather than adding a new protocol variant: `WorkerEvent::PostToolUse` is matched in 17 files across the engine, and its `tool_response` shape is explicitly relied on as-is by PR-URL capture ([A-5](#proposed-p1422-amendments)) — reshaping either was rejected as unnecessary blast radius for a signal that is, honestly, a heuristic rather than ground truth.
3. Name the heuristic's limits plainly, matching this doc's practice elsewhere: it both under-fires (a sandboxed network denial reports `Could not resolve host` / `Network is unreachable`, not `Operation not permitted` — the phrase list is neither exhaustive nor cross-platform-verified) and can over-fire (`Operation not permitted` is the generic EPERM string; a command can hit it for reasons that have nothing to do with Boss's sandbox). It is a visibility improvement, not a guardrail — it does not block or retry anything, it only makes a previously invisible class of run observable to whatever downstream policy (review-gate logic, attention surfacing, transient recovery) chooses to look at `Notification` events.

Implemented in `engine/driver/src/codex/progress.rs` ([T-30](#t-30-surface-sandboxcommand-denials-as-a-distinct-notification-signal-a-13)); no `boss-protocol` change is needed.

### G-7 `TurnBoundary`

PR #2361 is in flight with the `TurnBoundary` trait method and routes its consumers through the resolved driver. Its engine synthesizer remains deliberately unbuilt, so this document does not duplicate that in-flight revision.

The brief rates this the highest-severity gap on the premise that Codex cannot signal turn end. **That premise is wrong** — `turn.completed` is native, in-band, and carries token usage. Codex's turn boundary is _better_ than Claude's.

The remaining gap is the deliberately deferred synthesizer for a hypothetical driver with neither hooks nor turn events. Codex needs neither that synthesizer nor a duplicate of PR #2361's trait-method work: `turn.completed` maps directly onto `WorkerEvent::Stop` once its driver normaliser is present.

**The subtlety this section was built around has been deleted rather than solved.** It read: Claude's `Stop` fires per assistant turn within a session, while `codex exec` is one turn per process, exiting after `turn.completed` — so Boss's probe/nudge loop, which assumes it can inject a follow-up into a live session (`engine/core/app/pane_delivery.rs`), would have to become `codex exec resume`, a **new process** rather than a message into a running one. That asymmetry was real for `exec` and drove [T-17](#t-17-controlverbs-on-the-trait-plus-codex-probenudge-a-7), [OQ-5](#risks--open-questions), and the whole `WorkerProcessLifetime` subsystem. Retiring `exec` for the bare TUI removed it at the root: Codex is now `WorkerProcessLifetime::Persistent` like Claude and Grok, `task_complete` fires per turn inside one long-lived process, and a follow-up prompt is typed into the pane. `codex exec resume` is no longer part of the design.

One new subtlety replaced it, and it is smaller but real: **a prompt delivered mid-turn folds into the running turn and produces no boundary of its own.** Codex buffers a mid-turn message behind its own affordance (`Messages to be submitted after next tool call`), delivers it at the next tool-call boundary, and answers it — but the rollout carries two `user_message` records against a single `task_started` / `task_complete` pair, so the normaliser correctly emits one `Stop` for two prompts. Both consumers that depend on a boundary survive this for the same structural reason: they key on "the next boundary the driver emits", not "the probe's own turn" (mono#2586). A future tightening into the latter would strand every mid-turn probe in flight forever, so the reasoning is restated at both sites rather than left to be re-derived.

### G-8 `StructuredOutput`

Enum variant at `engine/driver/src/lib.rs:43`, **no trait method**. The engine-side file contract exists as `BOSS_STRUCTURED_OUTPUT` (`engine/core/src/spawn_flow.rs:59`) — covering review findings, task followups, postmortem followups. Still transcript-scraped: triage (`engine/core/src/automation_triage.rs:498 parse_triage_decision`) and PR URL (`engine/core/src/pr_url_capture.rs`, which reads `tool_response.stdout` from **`PostToolUse` hook events** — a Claude-hook dependency, re-verified at `pr_url_capture.rs:1-6`).

Codex is **better** here than Claude: `--output-schema <FILE>` constrains the final response to a JSON Schema, and `--output-last-message <FILE>` writes it to a known path. That is a native, enforced structured-output contract — strictly stronger than "ask the agent to write a file and hope."

Two consequences:

- **T1476 (file-based `StructuredOutput` contract) is well-directed and should proceed**, because the env-var file contract is the common denominator that works for both drivers. Its scope is sufficient for Codex _as far as it goes_.
- **The file-based structured-output scope was insufficient in one respect:** PR URL capture is `PostToolUse`-derived. The rollout normaliser now emits `PostToolUse` from correlated `custom_tool_call_output` / `function_call_output` records, and the Codex driver supplies `payload.output` text to the shared URL regex rather than reading stdout `aggregated_output`.

### G-9 `TranscriptAccess`

`transcript_path_for_session()` is now on the driver trait and `live_status_loop` calls `normalize_transcript_entry` before redaction. `engine/transcript-tail` has since been generalised — its own docs now describe "agent-driver transcript files" reported by `AgentDriver::transcript_path_for_session`, driver-agnostic at the container level, with callers passing each line through the run's driver normaliser. The Claude framing this section flagged is gone.

Codex rollouts are also JSONL, so the **tailer container** is reusable — but path discovery and **line schema** are the problems.

**Path discovery.** Claude's path is discovered because Claude stamps `transcript_path` on hook payloads (`engine/core/src/events_socket.rs`, `live_status_loop.rs`). Codex's `--json` stream does **not** carry `transcript_path` (verified — no such field in any captured envelope). The on-disk pattern is `$CODEX_HOME/sessions/<Y>/<M>/<D>/rollout-<local-timestamp>-<thread_id>.jsonl`. Because the filename embeds a **local start timestamp**, the path is **not** fully constructible from `thread_id` alone: discovery is a **glob** `**/rollout-*-{thread_id}.jsonl` under `$CODEX_HOME/sessions` (or a sessions-dir watch). Pane-viability Q7 observed the file appear at t≈0 after process start and grow while the session ran — discovery latency is not a practical obstacle once `thread_id` or a dir watch is available.

Codex's **hook** payloads _do_ carry `transcript_path` — confirmed on 0.145.0 ([D-2](#deltas-that-change-the-design)) — and the design now does wire hooks, so this is a live option rather than a hypothetical one. It is still not the right primary discovery route: a hook payload only arrives once the worker uses a tool, and only if hooks were trusted, whereas `thread_id` is known from the first `thread.started` (when the observer has a stream) and the driver owns `CODEX_HOME`. Glob-from-`thread_id` stays the primary mechanism because it is unconditional relative to hooks; the hook field is a cross-check, not a dependency.

**Schema ≠ stdout.** Rollout and `codex exec --json` stdout are **different event dialects**. Rollout carries `session_meta`, `event_msg` (`task_started`, `agent_message`, `turn_aborted`, …), and `response_item` / `custom_tool_call` / `custom_tool_call_output`. Stdout carries `thread.started` / `turn.*` / `item.*` with `command_execution.aggregated_output`. A driver cannot treat them as the same parser with a different source. Abort events live in rollout (`event_msg.turn_aborted`); they were **not** observed on exec stdout (Esc abort was spiked on the **TUI**, not on `exec`).

**Landed shape, and it is not the glob this section prescribed.** Rollout discovery ended up owned by the engine-side ingress rather than the driver: `AgentJsonlProgressManager` snapshots candidate rollouts under the exact run-private `CODEX_HOME` before spawn, discovers the one new file, validates `session_meta` cwd/thread identity, rejects stale, wrong-workspace and ambiguous candidates, and records the selected canonical path (later durably, as an ingress checkpoint — see [above](#ingress-must-survive-an-engine-restart-and-a-persistent-session-is-what-made-that-mandatory)). `CodexDriver::transcript_path_for_session` returns `None` and says why: the lookup needs the exact run home, which only the ingress holds. The standalone `discover_rollout_path` glob helper existed only to serve the stdout-dialect session and was deleted with it (mono#2572). The correct generalisation is therefore narrower than "driver-supplied path discovery": the driver supplies a **containment root** (`transcript_containment_root`) and the identity predicate; the engine does the discovery, because the engine is what knows when the spawn happened. Only the rollout dialect has a normaliser session now; the stdout one is gone.

### G-10 `ControlVerbs`

**This section's premise is fully overtaken, and the outcome is better than it predicted.** It read: the trait has only `classify_error` and it is never called, probe/interrupt/stop/reap are not on the trait at all, and Codex's probe would have to be `codex exec resume` — a new process — against Claude's message-into-a-live-session. All four halves moved.

**Control verbs are on the trait and Codex declares real ones.** `probe()` → `ProbeDelivery::PaneText`, `interrupt()` → `InterruptDelivery::PaneEsc`, `stop()` → `StopDelivery::ProcessOnly`, `reap()` → `ReapDelivery::ProcessGroup` (`engine/driver/src/codex.rs`). Because the driver is now a persistent TUI session, probe is an ordinary pane write down the same path Claude and Grok use — not a process restart — and `codex exec resume` disappeared from the design along with `exec` itself. Interrupt is Esc into the pane, the shape Grok already ships, and it is stronger for Codex than it is for Grok: Esc aborts the live turn, the process survives, a follow-up turn runs, and `turn_aborted` reaches a **real `Stop(Interrupted)`** rather than skipping the boundary (pivot spike V3). The pane-viability finding that Esc was "TUI-only" is now a property of the shipped shape rather than a caveat against it.

**Delivery confirmation was the real gap, and it was broken in a way nothing surfaced.** Codex has no `UserPromptSubmit` hook, so the probe-reply read is what closes the loop — and `dispatch_probe_reply_on_stop` parsed the transcript with a hand-rolled scan for `type == "assistant"` / `message.content[*].text`. No Codex rollout record has that shape, so the read returned `None` for **every** Codex probe: delivered, answered, read at the right boundary, and still no `ProbeReplied`, with the coordinator waiting forever. It now normalises through the run's own driver via `normalized_transcript_values`, split out of the existing `parse_transcript_with_driver` rather than written fresh (mono#2586). The extractor works on canonical _records_ rather than flattened `TranscriptEvent`s, because that flattening makes two adjacent single-block assistant messages indistinguishable from one two-block message — the difference between "the newest reply" and "the newest reply glued onto the previous one". This is the same class of defect `driver_transcript` was written for; the probe-reply read was simply the one site never wired to it.

**`classify_error` is now called — and Codex's is a stub.** `transient_recovery.rs` routes through `driver.classify_error(...)` rather than `classify_claude_error`, which is the fix this section asked for. `CodexDriver::classify_error` returns `Indeterminate` unconditionally, with a comment naming real classification (rate limits, quota, auth expiry) as follow-on. So Codex workers get no transient-error recovery of their own today: the seam is correct and the driver-specific half is unbuilt. The [driver-terminal-error path](#a-driver-reported-fatal-error-is-not-a-turn-boundary) covers the fatal case from the progress stream instead, which is why this has not been load-bearing yet — but it is a genuine hole, not a deliberate omission.

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

| Guardrail                              | Enforced today                                                            | Under Codex                                                                                                                                                                                                                       | Call                                                                                                                                                                       |
| -------------------------------------- | ------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Boss data-dir path guard**           | `PreToolUse` deny (`PATH_GUARD_SCRIPT`, `worker_setup.rs:1065`)           | `--sandbox danger-full-access` by default (`codex_sandbox_enforced` feature flag, off): no OS-enforced boundary, same as the Claude driver today. `--sandbox workspace-write` when the flag is on: Boss data dir denied by the OS | **Matches Claude's posture by default; opt-in kernel enforcement.** The advisory `PATH_GUARD_SCRIPT` hook remains the fence either way — see codex_sandbox_enforced below. |
| **Reviewer read-only**                 | per-kind deny rules (`reviewer_deny_rules`)                               | `--sandbox read-only`, always, independent of `codex_sandbox_enforced`                                                                                                                                                            | **Preserved, strengthened.** Exact semantic match, OS-enforced.                                                                                                            |
| **checkleft push guard**               | `PreToolUse` deny (`CHECKLEFT_PUSH_GUARD_SCRIPT`, `worker_setup.rs:1245`) | `PreToolUse` deny, same script, Codex hook config                                                                                                                                                                                 | **Preserved, same mechanism.**                                                                                                                                             |
| **Revision-PR guard / no direct push** | `PreToolUse` deny                                                         | `PreToolUse` deny, same script, Codex hook config                                                                                                                                                                                 | **Preserved, same mechanism.**                                                                                                                                             |
| **Editorial enforcement**              | `PreToolUse` deny **and rewrite**                                         | `PreToolUse` deny; the inline-`--body` rewrite is unreachable and becomes a deny                                                                                                                                                  | **Preserved by deny-instead-of-rewrite** — see below.                                                                                                                      |

The Reviewer row is unaffected by the decision and is enforced more strongly by Codex's OS sandbox than by Claude's deny rules. The data-dir row is now flag-dependent: kernel-enforced only when `codex_sandbox_enforced` is on, advisory-hook-only by default — matching, not exceeding, Claude's posture in that default case. The middle two are now a straight reuse of the existing guard scripts behind Codex's hook config rather than a rewrite into shims. Only the editorial row changes character.

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

**Rejected for v1, still a plausible v2, and the case for it has weakened.** It is a much richer surface: real steering of a _live_ turn, structured interrupts, and explicit hook-run reporting. But it is marked `[experimental]` in `codex --help`, it is a fundamentally different execution model from Boss's "agent CLI in a ghostty pane", and it would front-load a large protocol client before the basic driver works. Two of the three things it was going to buy have since been obtained on the shipped shape: the probe/nudge mismatch in [G-7](#g-7-turnboundary) dissolved with the TUI pivot rather than needing app-server steering, and interrupt is Esc into the pane. Only explicit hook-run reporting remains genuinely better there — and Boss now supplies its own equivalent through the guard trace.

**Noted as built, because it is not what this rejection implies:** `codex app-server` **is** a production dependency of the Codex driver, just not as an execution mode. The hook-trust gate spawns it to call `hooks/list` and observe `trustStatus=trusted` / `enabled=true` / matching `currentHash` for every armed hook before the worker starts ([T-01](#t-01-codex-hook-trust-provisioning)). Rejecting a surface as the _agent loop_ did not rule it out as a _control-plane query_, and the split turned out to be the useful one.

### Alternative 4: the interactive TUI (bare `codex`, no subcommand)

**This alternative was never evaluated as a competing execution mode, and re-examination shows this doc has no documented justification for rejecting it.** Three grounds might reasonably be asserted for disqualifying the TUI — a persistent multi-turn session shape, no machine-readable output, and keystroke-only interrupt — and each fails on inspection against Boss's own existing drivers, below. Citations below are to `main` at `b692473a414d181a91325962d346a5acb44da037`, the same baseline Alternative 8 (below) cites, not to any commit of this branch. The honest record is that `codex exec` was chosen without the TUI ever being weighed against it, and nothing written down since has supplied the missing comparison.

The [Non-goals](#non-goals) section covers this only implicitly, via "v1 drives `codex exec` only" (line 38) — it names Codex Cloud, `app-server`, `mcp-server`, and `remote-control` individually but never the bare TUI by name. The bare TUI appears three other times before this section, all as a comparison point for Esc/abort semantics — never as a candidate execution shape in its own right: the Q5 spike summary (line 53, "Esc abort on the TUI"), and [G-10](#g-10-controlverbs)'s pane-viability Q5 discussion (lines 523 and 541). One further mention, at line 155, comes closer to a rationale without quite being one: "`codex exec` is a real, first-class non-interactive mode — not a scraped TUI." Read in isolation that sentence could look like the one place this doc gestures at a reason to prefer `exec`, but it does not survive scrutiny as one — it is asserting that `exec`'s output is not scraped, not that the TUI's would have to be, and Ground 2 below shows the TUI writes the identical `rollout-*.jsonl` dialect `exec` does, so nothing needs to be scraped from it either. `codex --help` on `codex-cli 0.145.0` confirms the TUI accepts the identical `-a, --ask-for-approval <untrusted|on-request|never>` that `codex exec` does, so `-a never` removes interactive approval exactly as it does from `exec`; approval was never the disqualifier and no version of this section has claimed otherwise.

**Ground 1 — "persistent, multi-turn session, not a one-shot process" — does not distinguish the TUI from Boss's own Claude worker.** Boss already runs Claude this way: `transient_recovery.rs:6-8` states plainly that "Boss launches each worker as an **interactive** `claude` session... with no `--print`." The Claude spawn line (`claude.rs:486-536`) confirms it — no `--print`, only an initial prompt argument, structurally the same shape as the TUI's "Optional user prompt to start the session." Every later turn is delivered by typing into the live pane (`FrontendRequest::SendInputToWorker`, handled at `engine/core/src/app/panes.rs:67-77`, dispatching to `SendToPane`); PR #2406 ("refuse SendToPane when worker is not accepting typed input") exists specifically to guard stdin injection into that kind of live session, which only makes sense if that is what Boss is already doing. A claim that "Boss's worker model spawns one process per turn/dispatch and observes it to completion via an exit" would be false for the driver Boss has run in production the whole time this design doc has existed, so it cannot be the ground for disqualifying the TUI.

**Ground 2 — "no machine-readable output mode" — is empirically false.** The bare TUI (`codex-cli 0.145.0`) was driven under `tmux` with a fresh `CODEX_HOME` across two live typed turns and writes `$CODEX_HOME/sessions/**/rollout-*.jsonl` — same path convention, same `rollout-` prefix, same `session_meta` / `event_msg` records `CodexRolloutProgressSession` already parses for `exec`. This doc's own pane-viability Q5 spike (line 541) already relies on this: it reads a `rollout turn_aborted` event _out of a TUI session_ to establish Esc semantics, which is only possible because the TUI writes the same rollout dialect `exec` does. Boss's live progress ingress tails that file, not stdout — `progress_observation_wiring` (`engine/driver/src/codex.rs:1501`) unconditionally builds `ProgressIngress::AgentJsonlFile` over `$CODEX_HOME/sessions/**/rollout-*.jsonl`, regardless of which Codex mode wrote it. The bare TUI's lack of a `--json` flag is irrelevant to the mechanism Boss actually uses to observe progress.

**Ground 3 — "interrupt is a keystroke, not a signal" — Boss already ships this for Grok.** `grok.rs:624-631` sets `InterruptDelivery::PaneEsc` for exactly this reason ("Interrupt is Esc into the pane... verified by the Q8 spike to cancel the in-flight turn while the process survives"). Commit `0b2a8805f7ed` ("Implement Grok control-verb delivery plans and Esc turn-end recovery") added Esc turn-end recovery precisely because Esc-cancelled turns skip the `Stop` hook — the same problem shape a keystroke-only interrupt would create for Codex's TUI. It is not disqualifying for Grok; Boss built the recovery path instead.

**The fallback grounds fare no better.** A narrower "complexity and symmetry" argument might be raised instead — that directory-trust handling and live-session lifecycle management make the TUI too costly relative to `exec`. That argument does not survive either:

- **Directory trust is a solved, driver-supplied capability, not a TUI-specific cost.** `AgentDriver::pre_trust_workspace` is a trait method with a no-op default (`engine/driver/src/lib.rs:1661`) that Claude overrides (`claude.rs:594-596`, calling the shared `pre_trust_workspace`/`pre_trust_workspace_in` helpers at `claude.rs:906-939`) — landed as "pre-trust Boss-created workspaces for Claude Code" (PR #1180). Grok solves the same problem its own way, unsetting `GROK_FOLDER_TRUST` on spawn so a host-inherited `0` can never gate a worker (`grok.rs:326-329`). "Make pre-trust and config-dir gitignore driver-supplied" (PR #2498) generalized this into a per-driver contract precisely so a third driver plugs into it rather than reinventing it. Codex's own `exec` path already stamps `trust_level = "trusted"` directly into `config.toml` at spawn (`codex.rs:492`) — i.e. Codex already has a working, driver-owned answer to first-run trust, and a TUI variant of the same driver would simply reuse it unchanged. Directory trust is therefore not a cost the TUI would add.
- **"Live-session lifecycle management is hard" is not a differentiator — it is the norm Boss already runs.** Claude and Grok are both long-lived, pane-hosted, typed-input TUI sessions today (see Grounds 1 and 3). `codex exec`, a one-turn-per-process batch invocation, is the structural outlier relative to Boss's two production drivers, not the TUI. An argument that the established norm is too costly to implement a third time is hard to credit when Boss has already paid that cost twice and built recovery paths (transient-error recovery for Claude, Esc turn-end recovery for Grok) to go with it.

**Outcome: this alternative won, and is no longer an alternative.** The section below is retained as the argument that reopened the execution shape; the pricing spike that followed it (`investigations/codex-tui-pivot-pricing-2026-07-30.md`) measured the pivot, recommended the TUI as the **only** Codex path — "support both" is a spawn-line contract conflict, not a configuration choice — and mono#2578 landed it. `codex exec` is retired. Read everything below as the case that was made, not as an open question: the paragraph that ends "whether to revisit the execution shape is open, unresolved work" was accurate when written and has since been resolved in the TUI's favour.

**So: no documented ground currently distinguishes the interactive TUI from Boss's existing Claude and Grok integrations, and the record does not contain a reason to have chosen `codex exec` over it.** The choice may still be right — `codex exec`'s one-turn-per-process shape is simpler to reason about for a first driver, and re-litigating the execution shape now would slow the rest of this design down for a decision that is not on this project's critical path — but that is a scope/sequencing judgment, not a technical disqualification of the TUI, and this doc has not previously said so plainly. Whether to revisit the execution shape is open, unresolved work, not a settled decision: the parent agent-driver-abstraction design's "agent CLI in a ghostty pane" framing (invoked in [Alternative 3](#alternative-3-drive-codex-app-server-over-json-rpc) and [Alternative 6](#alternative-6-codex-remote-control) to reject `app-server`-based alternatives) describes the TUI at least as well as it describes `exec` — Boss's chosen approach is itself pane-hosted (["The argv remains pane-hosted."](#chosen-approach)), so "runs inside a ghostty pane" does not distinguish `exec` from the TUI either. If a future revision wants to reopen this, it should evaluate the TUI head-to-head against `exec` on its actual merits (crash/recovery behavior, resource cost of an idle interactive process, whether steering a live TUI turn is worth more than the mid-turn injection guard `codex exec` needed built for it) rather than retrying grounds already refuted here.

### Alternative 5: `codex mcp-server`

`codex mcp-server --help` (0.145.0): "Start Codex as an MCP server (stdio)." It takes no `PROMPT` argument and no task-shaped input at all — its only options are `-c/--config`, `--strict-config`, `--enable`/`--disable`.

**Rejected.** This mode inverts the control relationship `codex exec` gives Boss for free. Under `exec`, Codex **is** the autonomous agent: Boss hands it a prompt and a sandbox, and Codex drives its own tool-use loop to a `turn.completed`. Under `mcp-server`, Codex instead **exposes tools over MCP to an external client**, and that client is the one that must decide which tool to call, when, and when the task is done — i.e. the agent loop moves from inside Codex to whatever drives it over stdio. Adopting this mode would mean building an MCP client inside Boss that reimplements the turn-taking logic `codex exec` already implements internally, which is a larger and backwards lift compared to `exec`, not a lighter one, and it is not evaluated further for that reason: it does not run a work item to completion by itself.

### Alternative 6: `codex remote-control`

`codex remote-control --help` (0.145.0): "`[experimental]` Manage the app-server daemon with remote control enabled," with subcommands `start` (start the daemon), `stop`, and `pair` ("Create and print a short-lived manual pairing code").

**Rejected.** This is not an independent execution mode; it is device-pairing/remote-management tooling layered on top of `app-server` (start/stop the daemon, mint a pairing code for a remote client such as a phone to attach). Its own viability is bounded entirely by [Alternative 3](#alternative-3-drive-codex-app-server-over-json-rpc)'s already-recorded rejection of `app-server` — marked `[experimental]`, a different execution model than the fixed "agent CLI in a ghostty pane" shape — and pairing a remote control client to a running instance is orthogonal to Boss's problem, which is headlessly spawning and observing one worker per task, not remotely attaching a UI to one.

### Alternative 7: Codex Cloud (`codex cloud exec` / `status` / `list` / `apply` / `diff`)

`codex cloud --help` (0.145.0): "`[EXPERIMENTAL]` Browse tasks from Codex Cloud and apply changes locally." `codex cloud exec --help` requires `--env <ENV_ID>` ("Target environment identifier") and submits the prompt as an asynchronous cloud task; `codex cloud status <TASK_ID>` and `codex cloud list` poll it; `codex cloud apply`/`diff` pull the result back down as a local diff.

**Rejected.** Execution happens inside an OpenAI-hosted remote environment identified by `--env`, not inside the cube-leased workspace Boss already provisioned for the task. None of the local machinery this doc's gap analysis depends on — per-worker `CODEX_HOME` ([Config discovery and isolation](#config-discovery-and-isolation--the-concurrency-question)), the OS sandbox ([G-3](#g-3-permissionpolicy)), `PreToolUse` hooks ([G-6](#g-6-tooluseinterception)), or rollout-file tailing ([G-5](#g-5-progressobservation--the-top-gap)) — attaches to a task that never runs on a box Boss controls; there is nothing local to spawn, sandbox, or tail. It is also `[EXPERIMENTAL]` and async-by-construction (submit, then poll a task id), the opposite of the synchronous one-process-per-turn shape `codex exec` gives Boss. This is a fundamentally different substrate — remote submission plus local diff-apply — not a CLI competitor to `exec` for local autonomous execution.

### Alternative 8: keep `--json` on the `codex exec` spawn line for progress transport

This is the doc's own original choice, and it was never explicitly revisited when the transport story changed underneath it — worth recording precisely, since the gap is in this doc's reasoning, not in Codex.

`--json` was the right flag for the design this doc originally described: an engine that owns the `codex exec` pipe/pty master and reads stdout JSONL directly (`ProgressIngress::StdoutJsonl`). The [event stream](#the-event-stream) section's claim that "the JSONL stream is uncontaminated, so the reader needs no filtering" (this doc, describing that stdout dialect) was written for exactly that shape.

The pane-viability spike (PR #2392) then established that the engine cannot read pane-hosted stdout at all under Boss's actual topology — 0 bytes, no `/proc/<pid>/fd`, opening the slave tty is not the master stream (see [G-5](#g-5-progressobservation--the-top-gap)). [G-5](#g-5-progressobservation--the-top-gap) and [Chosen approach](#chosen-approach) responded by moving production progress ingress to `ProgressIngress::AgentJsonlFile` — tailing `$CODEX_HOME/sessions/**/rollout-*.jsonl`, which `codex exec` writes unconditionally whether or not `--json` is passed. That is exactly what `CodexDriver::progress_observation_wiring` does today: it unconditionally constructs `ProgressIngress::AgentJsonlFile` (`engine/driver/src/codex.rs:1501`), never `StdoutJsonl`. The stdout-dialect parser that `--json` exists to feed (`ProgressStreamSource::StdoutJsonl`, `CodexProgressSession::new`, `engine/driver/src/codex.rs:1542`) remains reachable code but is not wired into any path the driver actually selects.

**What this means, stated plainly: "Boss must use `codex exec`" does not imply "the pane must display raw JSON."** Those two were conflated in practice, and by the time of this revision the conflation has been fixed in code, not just diagnosed here.

**Rejected (keeping `--json`) — and resolved.** `mono#2532`, "Codex panes: drop vestigial `--json` from the pane spawn line," has merged to `main` (`b692473a414d181a91325962d346a5acb44da037`). The spawn line no longer carries `--json` (`engine/driver/src/codex.rs:835-878`, now `codex exec --color always --strict-config --skip-git-repo-check --sandbox workspace-write`), `--color always` keeps the transcript colorized under a pane pty where Codex's own tty auto-detection is unreliable, and `--json` is now forbidden outright in the spawn-line contract test (`CODEX_EXEC_FORBIDDEN_LONG_FLAGS`, `engine/core/src/conformance/fixtures.rs:258`). `--json` was never required for progress tracking once ingress moved to `AgentJsonlFile` — this section's conclusion and the landed fix agree.

### Is `codex exec` the only viable option? The other execution modes, individually

The three alternatives above all evaluate transports and guardrail carriers _around_ `codex exec`. They do not answer a narrower, recurring question: is `codex exec` itself the only Codex entry point that could stand in for the Claude-CLI-in-a-pane shape, or did this doc simply never look at the others? Non-goals already lists `codex app-server`, `codex mcp-server`, `codex remote-control`, and Codex Cloud as out of scope, but "out of scope" was asserted, not argued, for three of the four (`app-server` got the full writeup above). The interactive TUI is not even named in Non-goals — it appears only as a comparison point for Esc/abort semantics ([G-10](#g-10-controlverbs)). This section closes that gap so the record answers "is `codex exec` the only viable option?" and not only "was `app-server` right for v1?"

Verified against the same pinned `codex-cli 0.145.0` this whole doc uses; each was re-run for this section rather than inferred from `--help` text alone.

#### Alternative 4: The interactive TUI (bare `codex [PROMPT]`)

**Rejected — and the reason is not primarily the approval loop the brief expected to confirm.** `-a never` exists on the bare TUI exactly as on every other mode, so an approval policy alone does not block headless use. What actually blocks it, verified directly:

- **It hard-requires a real terminal on stdin.** `codex -a never -s read-only "echo probe" < /dev/null` — the identical `< /dev/null` redirect [`codex exec` requires and tolerates](#invocation-and-headless-mode) — fails immediately with `Error: stdin is not a terminal`, exit 1. `codex exec` explicitly documents stdin-as-prompt-source as a supported path (`codex exec --help`: _"If not provided as an argument (or if `-` is used), instructions are read from stdin"_); the bare TUI has no such fallback and simply refuses to start. Boss's pane spawn line already redirects stdin from `/dev/null` for every worker ([Invocation and headless mode](#invocation-and-headless-mode)); the TUI cannot be spawned that way at all.
- **Even given a real pty, it does not exit after one turn.** Run under an actual pty (`script`) with the same `-a never`, a trivial prompt, and a 12-second budget, the process is still running when the timeout fires (exit 124) — unlike `codex exec`, which reliably exits after `turn.completed` ([The event stream](#the-event-stream)). The TUI is a persistent interactive session by design (further turns, slash commands, model switching), not a bounded one-shot process. Boss's dispatch model needs a worker process that runs one work item to completion and exits so the engine can observe exit status and reap the pane; a session that stays live waiting for a human is the wrong shape independent of whatever its approval policy is set to.

So the substantiated rejection is: no non-tty invocation path, and no bounded lifecycle even when a tty is supplied — not the approval-loop reasoning originally hypothesized. The approval loop is real (interactive approval prompts are still part of the TUI's UI even under some policies) but it is not the load-bearing reason, and citing it as the reason would be citing something weaker than what was actually verified.

#### Alternative 5: `codex mcp-server`

`codex mcp-server` starts Codex as an MCP **server** speaking stdio — Codex becomes a tool provider that some other MCP **client** calls into, not a process that autonomously runs a prompt to completion. There is no "run this work item and exit" surface here at all: adopting this mode would mean Boss's engine implementing an MCP client and its own agent loop on top of Codex-as-a-tool, which is not a competing execution mode for the driver, it is a different architecture that reimplements what `codex exec` already does natively. **Rejected as inapplicable** — it doesn't offer a candidate spawn line for the driver's `Spawn` capability, so there is nothing to compare against `codex exec` here.

#### Alternative 6: `codex remote-control`

`codex remote-control {start|stop|pair}` manages an `app-server` daemon with remote pairing enabled — its own `--help` marks it `[experimental]`, and its subcommands are daemon lifecycle and short-lived pairing-code generation for a remote client (e.g. a companion device) to steer an already-running session. It presupposes `app-server` ([Alternative 3](#alternative-3-drive-codex-app-server-over-json-rpc), already rejected for v1 on separate grounds) and adds a remote-pairing concern Boss's single-host worker dispatch has no use for. **Rejected for v1 for the same reasons as Alternative 3, plus its own added scope.**

#### Alternative 7: Codex Cloud (`codex cloud`)

`codex cloud exec --env <ENV_ID> [QUERY]` (`[EXPERIMENTAL]` per `codex cloud --help`) submits a task to a remote, OpenAI-hosted environment identified by `--env`, not to the local `cube` workspace the worker was spawned in; results come back only via a separate `codex cloud apply`/`diff` step. This inverts Boss's entire execution model: the workspace, the OS sandbox, the per-worker `CODEX_HOME`, and Codex's own `PreToolUse` hooks all assume the agent is mutating files Boss can see and gate in real time ([Sandbox and approval](#sandbox-and-approval--and-what-bosss-deny-rules-become), [Hooks](#hooks--the-decisive-investigation)) — none of that applies to a task executing on infrastructure Boss doesn't control, reachable only through an apply-after-the-fact step. **Rejected** — not a drop-in execution mode, a different product entirely, and marked experimental besides.

### Why `--json` is still on the spawn line, and why that no longer means what it used to

This doc's own transport reasoning pivoted twice: first to "engine tails Codex stdout" (`ProgressIngress::StdoutJsonl`), then — once the pane-viability spike established that the pane pty is owned by the app process and an outsider-with-`shell_pid` reads 0 bytes from it ([The event stream](#the-event-stream), [G-5](#g-5-progressobservation--the-top-gap)) — to "engine tails the rollout JSONL file" (`ProgressIngress::AgentJsonlFile`). The `--json` flag on the spawn line was never revisited after that second pivot, and until now no sentence in this doc said why it is still there. That gap is exactly what let the question recur: "Boss must use `codex exec`" and "the pane must display raw JSON" are two different claims, and nothing here distinguished them even though they were being treated as one.

**Where the flag actually came from.** `--json` was chosen for the first, since-superseded design, where the engine parsed Codex's own stdout directly. This doc's own earlier claim — _"the JSONL goes to stdout, and only to stdout... a reader attached to stdout sees clean JSONL with no filtering"_ ([The event stream](#the-event-stream)) — was written for that design, and is still true as a description of the stdout dialect; it is no longer a description of what production code reads.

**What production code actually reads, verified against commit `ddd01898f7b5`.** `CodexDriver::progress_observation_wiring` (`engine/driver/src/codex.rs:1310-1325`) unconditionally returns `ProgressIngress::AgentJsonlFile`, pointed at `$CODEX_HOME/sessions` — it never returns `ProgressIngress::StdoutJsonl`. Codex writes that rollout file regardless of whether `--json` is passed (the flag governs stdout formatting only — `codex exec --help`: `--json` — _"Print events to stdout as JSONL"_ — it has no effect on `$CODEX_HOME/sessions/**/rollout-*.jsonl`, confirmed by running `codex exec` **without** `--json` and observing the identical rollout file appear and grow). The `StdoutJsonl` variant and its stdout-dialect parser exist in the crate (for the engine-owned-pipe topology this doc still keeps on the table, see [Chosen approach](#chosen-approach)) but are not wired into the Codex driver's actual `progress_observation_wiring`, so no production Codex worker consumes them today.

**Therefore: `--json` is not required for progress tracking**, and has not been since the `AgentJsonlFile` pivot landed. Its only remaining effect on a running pane is cosmetic-but-real: it suppresses Codex's own built-in, human-readable colorized transcript (the `OpenAI Codex v0.145.0 / workdir: ... / session id: ...` header and prose form, verified by running `codex exec` without `--json`) in favor of raw JSONL on the pane surface a human would see if they attached to it. **That is a real, if minor, degradation of the operator-facing pane** — not a functional requirement. Nothing else observed in this investigation depends on the flag: hooks carry guardrails over a separate channel ([Hooks](#hooks--the-decisive-investigation)), structured output uses `--output-last-message` / `--output-schema` ([G-8](#g-8-structuredoutput)), and PR-URL capture reads the rollout dialect's `response_item.payload.output`, not stdout `aggregated_output` ([G-5](#g-5-progressobservation--the-top-gap)).

State this plainly, because the conflation is the entire reason the question keeps recurring even though no single sentence in this doc asserted it: **"Boss must use `codex exec`" does not imply "the pane must display raw JSON."** The first is this doc's chosen execution shape, argued above. The second was true only under a transport design this doc no longer uses.

**Status of removing it, as of this writing.** `--json` is still present on the spawn line (`engine/driver/src/codex.rs:735`), still documented as required in that function's own contract comment (`engine/driver/src/codex.rs:696`), and still asserted by a passing test (`engine/driver/src/codex.rs:1742`, `assert!(plan.command.contains("--json"), ...)`). Removing it is tracked as separate code work in this project ("Codex panes dump raw JSONL: drop the vestigial `--json` from the pane spawn line") and had not landed as of commit `ddd01898f7b5`. This section describes the rationale and the flag's current status; it does not claim the removal is done. If that work has landed by the time this section is read, the code citations above are the ones to re-check first — a landed removal would mean `codex.rs:735` no longer contains `--json` and the test at `:1742` now asserts its absence instead.

---

## Chosen approach

**Amended 2026-07-30 (pivot to the bare interactive TUI):** `tools/boss/docs/investigations/codex-tui-pivot-pricing-2026-07-30.md`, a merged spike, priced retiring `codex exec` in favor of the bare interactive `codex` TUI and recommended one shape — the TUI — rather than keeping `exec` behind a flag: two of the three flags the `exec` contract required are hard argument errors on the TUI, and the flag the TUI needs (`--no-alt-screen`) is a hard argument error on `exec`, so "support both" is a spawn-line contract conflict, not a configuration choice. That recommendation landed; the paragraphs below describe the shape as it now ships. The rest of this section — hooks for guardrails, `CODEX_HOME` isolation, the OS sandbox, the structured-output file contract — is unchanged by the pivot; only the CLI invocation and process lifetime moved.

Drive **the bare interactive `codex` TUI as the worker CLI** (no subcommand; positional prompt; long-lived, multi-turn session — `WorkerProcessLifetime::Persistent`, matching Claude and Grok), with the existing `BOSS_STRUCTURED_OUTPUT` environment-file contract for structured results, per-worker `CODEX_HOME` for isolation, Codex's OS sandbox for filesystem guardrails, and **Codex's `PreToolUse` hook for command guardrails** — the same mechanism the Claude path enforces with today ([operator decision](#operator-decision)). `--json` never existed on the TUI as anything but a hard error; progress reaches Boss via the rollout-file tail described next, not stdout. `--output-last-message` is not part of the shape today — it is a possible future extension of the file contract ([T-15](#t-15-structuredoutput-trait-method-and---output-schema-wiring-a-5)), not something the driver currently passes (`CodexDriver::structured_output_wiring`, `engine/driver/src/codex.rs`).

**The earlier phrasing — "pane-embedded worker with stdout JSONL as the progress transport" — is not implementable as written for the engine under the current app/engine split.** Empirically (pane-viability spike):

- **Engine-spawned** `codex exec --json` (engine owns stdout pipe/pty master): stdout JSONL + PR #2363's reader **worked** as a topology, but this topology is not the one Boss runs — Codex is pane-hosted, per the next bullet — and neither shape Boss can spawn (the retired `exec` line, or the current TUI line) ever carries `--json` ([Chosen approach](#chosen-approach)). The stdout-JSONL dialect parser (`CodexProgressSession`, `StdoutEnvelope`, `parse_stdout_envelope`) this topology fed has been removed as unreachable dead code rather than kept "just in case"; if the engine-spawned topology is ever pursued, its progress reader should be designed against that day's requirements, not resurrected from this removal. `ProgressIngress::StdoutJsonl` itself is retained as a registration-time admission check — `DriverRegistry::default()` panics if a built-in driver ever declares it — rather than deleted outright, since the shared generic JSONL reader crate (`boss-engine-stdout-progress`) still legitimately exercises that ingress kind against synthetic test drivers; it remains reachable code but is not wired into any path the driver currently selects.
- **Pane-hosted** worker (app owns GhosttyKit/pty; engine receives `shell_pid` only): the engine **cannot** attach to that stdout. The selected transport is the engine-side, run-correlated rollout-file tail (`ProgressIngress::AgentJsonlFile`), not rendered scrollback, PTY reads, or new app IPC. This transport is shape-neutral — the pivot spike confirmed the shipped `CodexRolloutProgressSession` already normalises a real multi-turn TUI rollout end to end, so it survived the pivot untouched.

What remains decided for v1: the bare `codex` TUI **without `--json`** is the **agent CLI shape**; pane progress normalisation targets the distinct rollout dialect (`session_meta` / `event_msg` / `response_item`) read off the tailed rollout file, and does not pretend rollout is stdout; hooks carry guardrails; structured output uses the file contract above.

### Execution shape

The production spawn line body (`build_codex_command`, `engine/driver/src/codex.rs`, called from `CodexDriver::spawn_invocation`) sets no env prefix of its own; `spawn_invocation` sets `CODEX_HOME` separately as an `EnvDirective::Set` on the `SpawnPlan`, resolved to `<codex-homes-root>/<sanitized-run-id>` by `codex_home_for_run`, where the root is `$BOSS_CODEX_HOMES_DIR` or `$TMPDIR/boss-codex-homes` (`codex_homes_root`), never a `codex-home` leaf under a run dir. The pane types the body directly at its shell prompt — no shell `exec` wrapper, matching Claude and Grok, so the shell survives to accept later typed turns:

```
CODEX_HOME=<codex-homes-root>/<run-id> \
  codex --strict-config --no-alt-screen -a never \
    --sandbox <replaced by permission policy — see Update below> \
    -m <model> \
    -c model_reasoning_effort=<resolved-per-model> \
    "$(cat .codex/initial-prompt.txt)"
```

There is no `-C <workspace>` on this line — Codex inherits its working directory from the pane process, which the app already launches with `workingDirectory` set to the workspace (`TerminalLaunchSpec(..., workingDirectory: request.workspacePath, ...)`, `app-macos/Sources/Ghostty/WorkersWorkspaceModel.swift:167-172`; the field itself is declared in `TerminalLaunchSpec`, `app-macos/Sources/Ghostty/TerminalPaneSession.swift:77-94`), not from a flag the driver passes.

No `--json` and no `-o/--output-last-message`: progress reaches the engine by tailing the rollout file Codex writes unconditionally under `CODEX_HOME` (`ProgressIngress::AgentJsonlFile`, `engine/driver/src/codex.rs`), and structured results reach it through the `BOSS_STRUCTURED_OUTPUT` environment-file contract (`CodexDriver::structured_output_wiring`, `engine/driver/src/codex.rs`). `--json` is forbidden outright on this spawn line by the conformance contract (`CODEX_FORBIDDEN_LONG_FLAGS`, `engine/core/src/conformance/fixtures.rs`), which also forbids `--color` and `--skip-git-repo-check` — both required on the retired `exec` shape, both hard argument errors on the bare TUI. See [Alternative 8](#alternative-8-keep---json-on-the-codex-exec-spawn-line-for-progress-transport) for why the doc's original `--json`-based design was superseded, independent of the later exec-to-TUI pivot.

**`-a never`, not "no `--ask-for-approval`".** On the retired `exec` shape the flag had been removed by the CLI outright (0.137.0 → 0.145.0), so headless `exec` ran `approval_policy=Never` unconditionally with nothing on the spawn line to say so. The bare TUI accepts `-a, --ask-for-approval <untrusted|on-request|never>`, so the driver now passes `-a never` explicitly: a long-lived interactive session must never block on a human approval prompt Boss cannot answer. Boss's own `--sandbox` policy remains the real authorization boundary either way.

**Update (Claude-parity change):** sandbox defaults to `danger-full-access`, not `workspace-write` as originally designed here. Codex's seatbelt template hardcodes a mach-service allowlist that excludes LaunchServices, so `xcode-locator` fails with `kLSExecutableIncorrectFormat` under `workspace-write` — this broke every bazel build in mono for Codex workers, a strictly worse posture than the Claude driver has ever run at (Claude workers get `--permission-mode auto` with no OS sandbox). Standard/Triage/AnswerAgent now run `danger-full-access` by default, gated behind the `codex_sandbox_enforced` feature flag (default off) so the OS-enforced `workspace-write` fence can be turned back on without a rebuild once the LaunchServices gap is fixed upstream. Reviewer is unaffected — it stays `--sandbox read-only` unconditionally. The Boss-data-dir guard is no longer structural by default; it falls back to the advisory `PATH_GUARD_SCRIPT` PreToolUse hook, the same fence the Claude driver relies on. `model_reasoning_effort` is resolved against the selected model's `supported_reasoning_levels` rather than assumed, since the ladder is per-model and now reaches `ultra` on some models. The pivot spike verified `--sandbox`, `-m`, `-c model_reasoning_effort=` are all still accepted by the bare TUI, unchanged from `exec`.

`--strict-config` turns an unrecognised config key into a startup error instead of a silently ignored setting — a cheap guard against config drift, kept unconditionally on both the retired `exec` shape and the current TUI shape.

`--no-alt-screen` is new on this shape: it disables the alternate screen so the viewport and a full-screen surface read diverge and scrollback accumulates across turns, instead of capping at one screenful under the default alt-screen mode — required for a session that now lives across many turns rather than exiting after one (pivot spike, V2).

The argv remains pane-hosted. Progress is additive: the engine tails Codex's independently written rollout file, while Ghostty continues to own and render stdout unchanged.

### The five engine seams this needs

1. **A progress reader matched to topology.** For a hypothetical **engine-owned** stdout topology (not the one Boss spawns Codex under today): the landed #2363 JSONL reader + `ProgressIngress::StdoutJsonl`, reachable but unselected by any current path — that topology is not pursued (see [Chosen approach](#chosen-approach) above), and the stdout-dialect parser has been removed. For **pane-hosted** Codex, the topology Boss actually runs: `ProgressIngress::AgentJsonlFile` tails one run-correlated rollout and feeds the same reader/fan-out with a rollout-dialect session normaliser. PR-URL capture reads `response_item.payload.output`, not stdout `aggregated_output`.
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
3. Therefore: "the app can see the worker" ≠ "the engine has progress ingress."

**Seam 5 was closed the other way.** Layer D established that surface scraping _works_, and the design still did not use it: the engine tails the rollout file Codex writes independently, so no app→engine IPC for surface text was built and none is planned. The one place rendered surface text remains load-bearing is the pane monitor's status pill, which is an app-local read of app-local state — see [Pane monitor markers](#pane-monitor-markers-a-driver-supplied-contract-the-driver-never-filled-in). Recording this because "the app can already read it" is a standing temptation to route progress through the app, and the reason not to is not feasibility: a file the agent writes itself needs no cooperation from the terminal emulator, survives the app being restarted, and has byte offsets you can checkpoint.

This section exists so later design work does not re-litigate "is pane content readable at all?" — it is, in-process on the embedder — and so "outsider cannot open the slave" is not misread as "Boss cannot observe the pane."

### Capability declaration for `CodexDriver` (v1)

Provided: `Spawn`, `WorkspaceProvisioning`, `PermissionPolicy`, `ModelAndEffortMenu`, `ProgressObservation`, `TurnBoundary`, `StructuredOutput`, `TranscriptAccess`, `ControlVerbs`, `PromptComposition`, **`ToolUseInterception` (deny-only)**. This is what shipped, unchanged (`CodexDriver::capabilities`, `engine/driver/src/codex.rs`).

Not provided — three omissions, each `Degrade` and each for its own reason:

- **`ToolProvisioning`** — unused in v1 for every driver. Codex has MCP, plugins and skills; Boss injects none, so declaring it would overclaim.
- **`AwaitingInputSignal`** — omitted, but **the original reason for omitting it has been retired and must not be cited again.** It argued that a completed turn means imminent process exit rather than "blocked on a human", which inverts under a persistent TUI: a Codex worker parked at its composer genuinely _is_ waiting for someone to type. The omission survives for a different, measured reason. This capability is a claim about the _`ProgressObservation` stream_ — that some record in it positively means "blocked on a human", which `apply_event` may promote to `WaitingForInput` on a `Notification`. Codex's `Notification` vocabulary is fully enumerated by this driver's own normaliser and every member means something else (unobserved-command marker, guard-trace replay, command denial, `turn aborted`, fatal `task_complete.error`), so binding the capability would promote a denied command or an aborted turn into a fabricated `WaitingForInput`. The TUI's composer literals do not earn it either — those are pane-render strings for `pane_monitor_spec`, read by scraping the surface, not records in the progress stream this capability describes. Grok's precedent, same grounds. Nothing is lost: a parked composer already reaches the engine as `WorkerActivity::Idle`, and `accepts_typed_input` treats `Idle` and `WaitingForInput` alike.
- **`CommandOutcomeObservation`** — added by this project ([A-14](#proposed-p1422-amendments)) precisely so that Codex could decline it. See [G-5's reconsideration](#g-5-progressobservation--the-top-gap).

`ToolUseInterception` is declared because hooks fire and `PreToolUse` deny blocks pre-execution on 0.145.0 ([D-2](#deltas-that-change-the-design)), and because that is the [chosen mechanism](#operator-decision). Two conditions attach to it, and both are the driver's to satisfy rather than caveats on the declaration:

- **Deny-only.** `permissionDecision:allow`, `:ask`, and `updatedInput` are all rejected, so the trait's rewrite path is unreachable and the inline-`--body` editorial case is handled by denying with a corrective reason ([the editorial case](#the-editorial-case-precisely)).
- **Gated on [T-01](#t-01-codex-hook-trust-provisioning).** An untrusted hook is skipped in silence, so the declaration is only honest once Boss can provision `trusted_hash` deterministically and detect a hook that did not run. T-01 therefore gated the first Codex worker — the one hard sequencing edge this design carried, and a `small` investigation.

**The gate was met, and the contingency was never invoked.** Trust is provisioned deterministically (SHA-256 over the normalised hook identity, matching Codex's own `command_hook_hash`, stamped into the per-run `config.toml`), verified against a **live** `hooks/list` before the worker runs, and re-verified against disk at every turn boundary thereafter; detection of a silently-skipped guard comes from Boss's own trace rather than anything Codex emits. `--dangerously-bypass-hook-trust` appears in no shipped code path. The fallback — promoting the `PATH`-shim project back ahead of Codex — was therefore never needed, and the shims remain a follow-on on their own merits.

### Which work-item kinds are Codex-eligible

Phased, with an acceptance criterion per phase. Refusals here are expressed through `KindRequirements`, and they are about **output-contract maturity**, not guardrails — guardrails are carried uniformly by the `PreToolUse` hook on both drivers.

**Phase 1 — chores and project tasks.** The plain "make a change, open a PR" loop. Acceptance: 10 consecutive chores dispatched `--driver codex` reach an open PR with green CI, no engine intervention, and the PR URL captured on the primary path (not a `jj log` reconstruction fallback). **Status: the loop works and the acceptance sweep has not been run.** Individual Codex dispatches reach PRs, and the specific failures that would have made the sweep fail were each found and fixed by other means — the three config/flag/rules blockers, the dead primary PR-URL path, the invisible fatal-error path. But the ten-consecutive-chores criterion has never been executed as such, and Phases 2 and 3 were enabled anyway. That is worth stating plainly: the gate was placed at the end of Phase 1, which means it could only ever hold back Phase 2, and "Phase 1 is not yet accepted" was never a state that stopped work.

**Phase 2 — design, investigation, postmortem.** These are document-producing kinds and depend on the shared `BOSS_STRUCTURED_OUTPUT` file contract plus followups parsing. Acceptance: a Codex-authored design doc lands with a correctly parsed `Proposed implementation task breakdown`, and its followups materialise. **Status: enabled, with the enforcement gap closed and the live acceptance run outstanding.** `KindRequirements::for_kind` previously escalated only `Design` to require-strict `StructuredOutput` + `ToolUseInterception`; it now escalates all three design-family kinds (`Design`, `Investigation`, `DesignPostmortem`) — the same grouping `ReasoningMode::default_for` already treats as one family — closing a gap where two of the three relied on each capability's default `Degrade` disposition instead of the stricter per-kind gate (mono#2615). The doc-parse/materialize pipeline was confirmed driver-agnostic by inspection and pinned by a regression test that runs the populator against a Codex-shaped task breakdown. The live end-to-end criterion — a real dispatched Codex design execution whose followups materialise — was not run.

**Phase 3 — review and conflict resolution.** Review needs `--sandbox read-only` to be verified as a real reviewer-read-only equivalent, and structured `ReviewResult` output. Conflict resolution needs write access plus the merge-conflict telemetry path. Acceptance: a Codex reviewer produces a structured `ReviewResult` on a real PR that a human agrees with, and demonstrably cannot write to the workspace. **Status: both verified live; not reachable in production for an unrelated reason.** See [T-25](#t-25-codex-eligibility-for-review-and-conflict-resolution-kinds) — the read-only sandbox is genuine and OS-enforced, `ReviewResult` round-trips (through a transcript fallback that was silently dead until it was fixed), and `REVIEWER_POOL_DRIVER` hardcodes `"claude"` for every review-pool dispatch, so there is currently no seam to select Codex as a reviewer at all.

**Deferred indefinitely — triage and answer-agent.** Not because of guardrails but because both are **transcript-scraped**: `parse_triage_decision` (`engine/core/src/automation_triage.rs:498`) reads the final assistant message, and the answer agent depends on `UserPromptSubmit`-based delivery confirmation (`engine/core/app/pane_delivery.rs`) that Codex does not have. Ironically Codex's `--output-schema` would make triage _more_ reliable than Claude's — but that is a rewrite of the triage contract, not a driver task. Refuse via `KindRequirements` until then.

### Load-balancing seams

Design _for_, do not design _now_. Three seams, with attachment points:

1. **Per-driver capacity accounting.** Slots are one global pool today. The seam is the dispatch gate at `engine/core/src/runner/worker_spawn.rs:597` — it already resolves `(kind, driver)` and is the natural place for an in-flight count keyed by driver slug. Requirement on this project: **do not add a second, driver-blind admission path.** Progress-ingress work (stdout reader, rollout tail, or app-forwarded channel) must not spawn workers outside this gate.
2. **Per-provider rate-limit state.** Codex hands this over for free: `turn.completed` carries `input_tokens`, `cached_input_tokens`, `cache_write_input_tokens`, `output_tokens`, `reasoning_output_tokens` (verified in the capture above), and the binary carries `RateLimitSnapshot` / `RateLimitWindow` types. The seam is the progress reader — it should record per-turn usage against the driver rather than discarding it. **Treat the usage field set as open:** `cache_write_input_tokens` was added between 0.137.0 and 0.145.0 with no wire signal ([D-4](#stream-drift--all-silent-all-additive)), so a balancer that destructures a fixed set of counters will break on the next upgrade. Claude has no equivalent in-band signal, which is itself worth knowing before a balancer assumes symmetry.
3. **Capability-aware routing.** `CapabilityResolver::check_dispatch` already computes exactly the predicate a balancer needs ("can driver D run kind K"). It must stay a **pure, side-effect-free query** so a balancer can call it speculatively across candidate drivers before choosing. Requirement on this project: do not make `check_dispatch` mutate state or log dispatch decisions as a side effect.
4. **The reviewer-pool driver pin — the concrete seam this project surfaced.** `REVIEWER_POOL_DRIVER` (`core/src/coordinator.rs`) hardcodes `"claude"` for every review-pool and automation-pool dispatch, unconditional on the driver of the row under review. That is a deliberate existing invariant — who authored a change must not determine who reviews it — and it is also the reason a fully verified Codex reviewer cannot be reached in production ([T-25](#t-25-codex-eligibility-for-review-and-conflict-resolution-kinds)). Making review-pool driver selection a policy rather than a constant is a dispatch-policy decision adjacent to the balancer, so it is deliberately **not** taken here; it is named so the balancer project inherits a located seam instead of a surprise. Note the invariant it must preserve is "not the author's driver", which is weaker than "always Claude" and admits several policies.
5. **Per-command outcome, with an explicit "observed" bit.** [G-5's reconsideration](#g-5-progressobservation--the-top-gap) found that `ProgressFidelity::Rich` does not imply reliable per-command exit status, and gave Codex's absence of that signal its own capability (`Capability::CommandOutcomeObservation`) rather than folding it into the fidelity tier. A future balancer that scores drivers on command-level success/failure must not treat "not observed" as "succeeded" for a driver that never declared this capability — Codex's unobserved state has no Claude counterpart, so a normalised per-command outcome type needs a tri-state (succeeded / failed / unobserved), not a bool, or the balancer will silently misattribute Codex's silence as success. Design _for_ this now by keeping the capability distinct; do not build the tri-state type itself as part of this project.

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

**OQ-4 — Rollout disk growth. Resolved.** `~/.codex` on this host held 279 active + 241 archived rollouts at ~865 MB, and per-worker `CODEX_HOME` multiplies that across workspaces. `--ephemeral` would avoid it entirely but forfeits `TranscriptAccess`, so the answer is retention rather than avoidance: an hourly sweep (also `bossctl codex-homes sweep`) over recorded homes only, classifying live vs terminal from execution status rather than mtime, reclaiming terminal homes older than 14 days or the oldest terminal set once retained size exceeds 500 MiB, with every delete guarded by a strict containment assertion against the homes root ([G-2](#g-2-workspaceprovisioning), mono#2422). A first prototype scanned the engine's own `CODEX_HOME` / `~/.codex` and inferred Boss ownership from a rollout's `session_meta.cwd` falling under a cube workspace; that was rewired out, and "never infer ownership, only reclaim what provisioning recorded" is the rule worth carrying to any driver with per-run state on disk.

**OQ-5 — `codex exec` is one turn per process. Mooted, not answered.** The question was how Boss would probe a worker whose process exits after every turn: `codex exec resume` spawns a _new_ process, so pane topology across process restart and abort-by-signal for a non-interactive `exec` were the two residuals. Both disappeared with the execution-shape pivot rather than being solved — Codex is a persistent session, probe is a pane write, interrupt is Esc, and `exec resume` is not part of the design ([G-10](#g-10-controlverbs)). Worth recording as the shape of the answer, because two other things in this doc were built specifically to serve the premise: the `SendToPane` mid-turn guard (below) and the whole `WorkerProcessLifetime` one-shot subsystem, which has since been deleted. Retiring a premise retired more machinery than answering it would have.

<a id="oq-6-codex-exec-review"></a>
**OQ-6 — Is `codex exec review` a better substrate for Boss's review kind than a plain read-only exec run?** New in this pass ([D-3](#delta-that-changes-a-tasks-scope)). It is purpose-built, takes `--base` / `--commit` / `--uncommitted`, and has a dedicated `codex-auto-review` model. It may also impose its own output shape that does not match Boss's `ReviewResult`. Unexamined; folded into [T-25](#t-25-codex-eligibility-for-review-and-conflict-resolution-kinds).

**Resolved by operator decision, before T-25's execution: forgone deliberately, not re-examined.** The single-shape decision recorded in [`codex-tui-pivot-pricing-2026-07-30.md`](../investigations/codex-tui-pivot-pricing-2026-07-30.md) retired `codex exec` in favor of the bare interactive TUI as the driver's only spawn shape; `codex exec review` is an `exec` subcommand and carries the identical spawn-line contract conflict that retired `codex exec` itself — two of the three flags the conformance contract requires on a Codex spawn line are hard argument errors on `review`/the bare TUI, same as they were for plain `exec`. Adding it back would mean a second spawn shape for one driver slug, which is exactly what the pivot doc measured as a trap, not a configuration choice. **The cost of forgoing it, now that T-25 has actually run the general read-only path, is concrete rather than hypothetical:** see [`codex-review-eligibility-sandbox-and-structured-output-2026-07-31.md`](../investigations/codex-review-eligibility-sandbox-and-structured-output-2026-07-31.md) — the general path's `--sandbox read-only` blocks not just malicious workspace writes but the reviewer's own sanctioned `$BOSS_STRUCTURED_OUTPUT` artifact write, so every Codex review depends on the transcript fallback rather than the primary artifact channel. A purpose-built `codex exec review` plausibly returns its result over a channel that doesn't require the sandboxed process to write a file at all (stdout, `--output-last-message`, or similar), which would have sidestepped this specific conflict. That fidelity is what the single-shape decision gives up here; it is recorded as the trade-off's price, not as grounds to reopen it.

**OQ-7 — Which pane-to-engine progress channel? Answered: the rollout tail.** The pick among rollout tail, app-forwarded observation and engine-owned spawn went to the first, and both losers are now closed rather than merely unchosen — the engine-owned stdout topology's dialect has been deleted and its ingress variant is a registration-time panic guard, and no app→engine IPC for surface text was built. What the answer did not include, and should have, is the rest of a file tail's contract: discovery correlation, a durable resume point, and a driver-owned session snapshot. Those cost more than choosing the channel did ([A-15](#proposed-p1422-amendments)).

**Risk — the `PATH`-shim relocation is a change to the Claude path.** It touches live guardrails on the driver that runs everything today. It is a net improvement (it closes the subshell-evasion hole) but it is not risk-free.

The original pass paired that risk with a claim that has since been **withdrawn**: that the ordering was "correct and non-negotiable — shipping Codex first means shipping it unguarded". That is not accurate under hook-based interception, where a Codex worker is guarded by the same class of mechanism as a Claude worker. The risk is real; the scheduling consequence drawn from it was not. Both the risk and its mitigation now belong to the [follow-on `PATH`-shim project](#the-path-shim-design--retained-as-a-follow-on-project) — where it should still be a human's call before [T-02](#t-02-relocate-command-guardrails-to-path-shims-follow-on-project) starts, just not a call that blocks Codex.

**Risk — Codex's guardrails inherit Claude's fail-open hook semantics, plus a trust gate Claude does not have.** This is the cost of the incremental path, stated in one place: Boss's command guardrails on Codex are exactly as strong as its hook wiring, and Codex adds a silent trust failure mode on top. [T-01](#t-01-codex-hook-trust-provisioning) is what makes this acceptable, and it must genuinely answer the detection half — "can Boss tell a hook did not run" — not just the provisioning half. A T-01 that provisions trust but cannot observe a skipped hook leaves this risk open.

**Risk — `SendToPane` while `codex exec` is mid-turn is a safety footgun, not hygiene. Real, mitigated, and then dissolved by the pivot.** Pane-viability Q2 (Layer D, Boss-equivalent `ghostty_surface_text` + Return into a real interactive shell): inject during a foreground `codex exec` was **not** consumed as agent input; the line was echoed, **survived** across codex exit, and the interactive zsh underneath **executed** it when the pane returned. Outsider slave-path write / `TIOCSTI` does not reproduce this (permission denied / non-representative) — the realistic path was master-side / GhosttyKit inject, i.e. production `SendToPane`. Two independent mitigations landed: `inject_pane_text_verified` refuses to write when the slot's live worker is not accepting typed input, failing closed on missing live state (mono#2406), and the `exec` spawn line was wrapped so the shell was _replaced_ rather than left underneath, so there was no interactive shell left to execute a buffered line after codex exited (mono#2414).

**Under the bare TUI the hazard is gone, and the declaration inverted.** A mid-turn injection now reaches Codex's own first-class affordance — it renders `Messages to be submitted after next tool call (press esc to interrupt and send immediately)`, queues the message, delivers it at the next tool-call boundary and answers it. Nothing lands in a tty line discipline and nothing is executed by a shell, which is exactly the hazard the conservative default exists to prevent. `CodexDriver::mid_turn_pane_input()` therefore declares `Buffers`, not `Rejects` (mono#2586, measured through the exact pane `submitText` path the engine uses, under a GhosttyKit-embedded surface). The `Rejects` default remains correct for any driver whose mid-turn behaviour is unmeasured — Grok still holds it — and the cost of the flip is the folded-turn accounting described in [G-7](#g-7-turnboundary).

**Risk — one turn per process cuts both ways: the guard above stops Boss writing into a dying pane, but nothing stopped Boss reading that pane's death as a crash.** Confirmed in the field on 2026-07-28 (codex-cli 0.145.0, gpt-5.6-terra, repo `checkleft-sandbox`). A worker completed its turn cleanly — `task_complete`, `duration_ms: 92159`, well-formed final answer — and `codex exec` exited, as designed. The app's `onChildExited` fired `WorkerPaneDied`, and **160 ms later** the engine orphaned the run as `worker-pane-died`. Its completion handler arrived 60 ms after that and found the execution already terminal (`AlreadyTerminal`), so the run could not reach a PR, escalate, or answer a probe; the husk sweep then retired the pane on its two-pass cycle. The work item behind it accrued 20 executions (19 `orphaned`), 26 `churn_guard_parked` attention items, and zero PRs.

Root cause: every process-liveness reaper — `dead_pid_sweep`'s periodic pass, its app-reported `reap_reported_pane_death`, and the restart-robust `dead_pane_sweep` — was written against Claude, whose process outlives every turn, so "the process is gone" could only ever mean a crash. A pane worker is also parked `waiting_human` from spawn (`PaneSpawnRunner`), which is not terminal, so nothing gated the reap.

**The fix was `AgentDriver::worker_process_lifetime()`** (`Persistent` by default, `OneTurnPerProcess` for `codex`) — driver-conditioned for the same reason as `mid_turn_pane_input()`: only the driver knows what its foreground process is contracted to do. The exemption was _evidence-gated_, not driver-gated: a one-shot exit was expected only when the run also had a delivered turn boundary recorded (`work_runs.turn_boundary_at`), and an exit without one was still reaped. Ordering was resolved **causally rather than with a grace period** — the process exiting is what makes its rollout file final, so on an exit the engine drained that file to EOF before the reap verdict was taken.

**That entire subsystem has since been deleted, because the pivot removed the lifetime it existed for.** With Codex flipped to `Persistent` (mono#2578), no registered driver declares `OneTurnPerProcess`, so `worker_process_exit.rs` (the classifier, `ProcessExitVerdict`, the drain state machine), the sweep-side `ExpectedTurnExit` arms, the one-shot-unreachable attention kind, `StreamHalt::Drain` and `finish_run` are all gone (mono#2585). A foreground process exiting is now unconditionally a death for Codex, exactly as it already was for Claude and Grok.

**What was deliberately kept, and why it is the interesting part.** `WorkerProcessLifetime` itself survives as an **enforced admission check**: `DriverRegistry::default()` runs a `refuse_one_turn_per_process` guard and panics at registration if any built-in driver declares it — mirroring the identical guard for `ProgressIngress::StdoutJsonl`, the other discontinued topology. The alternative, deleting the enum, would mean a future author who sets that variant silently inherits the _old, wrong_ treatment (every exit read as a death, mid-turn or not) with none of the machinery that made it correct. A registration-time panic converts that into a loud startup failure. Two discontinued designs, two panic-guarded vocabulary items: this is the pattern worth reusing when a capability is removed rather than never built.

One residue is recorded honestly rather than cleaned up here: `work_runs.turn_boundary_at` was purpose-built for the deleted classifier, which was its only production reader. It is still stamped on every turn boundary and now read by nothing.

**Correction (2026-07-27), and its own reversal.** The first implementation of the `SendToPane` guard keyed purely on live `WorkerActivity`, refusing every mid-turn write on every driver. That over-applied it: the footgun was a property of `codex exec`'s foreground process (one turn per process, stdin on `/dev/null`), not of "the pane is busy". Claude Code is a long-lived interactive TUI that reads stdin for the whole session and holds mid-turn input as its next prompt, so a mid-turn write there is consumed by the agent and never reaches the shell. Because the urgent-probe path fires on `PostToolUse`, where activity is `Working` by construction, an activity-only guard made `bossctl probe --urgent` structurally undeliverable for _every_ driver — observed in the field as two probes reported "queued" and never delivered to a healthy Claude worker across ~27 tool boundaries. The decision became `activity × AgentDriver::mid_turn_pane_input()`, defaulting to reject so a new driver is safe until it establishes otherwise. Codex declared reject; **it now declares `Buffers`**, measured, for the reasons in the `SendToPane` risk above. The two-factor decision is what made that flip a one-line change rather than a re-litigation.

---

## Proposed P1422 amendments

Discrete, filed-work-item-sized. The original design pass could not create Boss work items; this revision materializes the immediate P3330 gates as T3681 through T3686. The remaining entries stay as the coordinator's handoff until they are independently scheduled.

| #    | Proposed name                                                                                         | Effort    | Amends / new                                                                                                                          | Brief                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| ---- | ----------------------------------------------------------------------------------------------------- | --------- | ------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A-1  | `ProgressObservation`: abstract the ingress transport, not just normalisation                         | `large`   | **Amends the prior transport abstraction**                                                                                            | Landed for the pane topology: `ProgressIngress` separates hook callbacks, engine-owned stdout JSONL, and run-correlated agent JSONL files. Pane-hosted Codex uses the rollout-file arm and the existing generic reader; the app-owned PTY remains visual only. The stdout arm was subsequently proven unreachable and its Codex dialect deleted, with the ingress variant retained as a registration-time admission check. **Extended by [A-15](#proposed-p1422-amendments):** abstracting the transport was necessary but not sufficient — a file-tail ingress also needs a durable resume point.                                                                                                                                                                                                                                                                                                     |
| A-2  | `PermissionPolicy`: return permission _artifacts_, not a single file path                             | `medium`  | **Amends [T-08](#t-08-permissionpolicy-artifacts-signature-p1422-amendment-a-2-amends-t1479)**                                        | The signature now already returns `PermissionArtifacts`; only T-08's extraction remains. It must first move the settings and deny-rule rendering from `worker_setup` across the one-way `core -> driver` boundary, retaining the existing config-files, args, and env artifact shape.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| A-3  | `Spawn`: replace Claude-shaped parameters with a `SpawnRequest`/`SpawnPlan` pair                      | `medium`  | **New**                                                                                                                               | Landed in PR #2355: `SpawnRequest` and `SpawnPlan` replace Claude-shaped parameters and let each driver supply its command and environment directives, including Codex's `CODEX_HOME`. This row retains the architectural rationale.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| A-4  | `TurnBoundary` trait method — decouple completion from `WorkerEvent::Stop`                            | `medium`  | **Amends [T-18](#t-18-turnboundary-engine-synthesizer-remainder-of-t3325)** (re-scopes; drops the synthesizer from the critical path) | PR #2361 is in flight with the trait method and driver-routed consumers. Codex's native `turn.completed` maps directly to `WorkerEvent::Stop`; the synthesizer remains separate future work for a driver with neither hooks nor turn events.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| A-5  | `StructuredOutput` trait method + driver-supplied PR-URL extraction                                   | `medium`  | **Amends [G-8](#g-8-structuredoutput)** (adds PR-URL; that prior scope is sufficient as far as it goes)                               | `StructuredOutput` (`lib.rs:43`) has no trait method. More urgently, PR-URL capture is derived from `PostToolUse` hook events (`pr_url_capture.rs:1-6`) and is out of that prior scope — under Codex it breaks completely, and the PR URL is the acceptance criterion for nearly every work item. Stdout dialect: `command_execution.aggregated_output` is regex-friendly. Rollout dialect: `custom_tool_call_output` — not the same extractor. Make extraction driver-supplied and dialect-aware of [seam 5](#the-five-engine-seams-this-needs). Also surface `--output-schema`, which is a stronger contract than the env-var file.                                                                                                                                                                                                                                                                  |
| A-6  | `TranscriptAccess`: driver-supplied path discovery, and actually call the normaliser                  | `small`   | **New**                                                                                                                               | Landed: Codex rollout discovery is contained to the exact run home, the selected path flows through normalised events, and live status uses a separate rollout transcript normaliser.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| A-7  | `ControlVerbs`: put probe/interrupt/stop/reap on the trait and call `classify_error`                  | `medium`  | **New**                                                                                                                               | Landed: probe/interrupt/stop/reap are trait methods with per-driver delivery plans, and `transient_recovery.rs` routes through `driver.classify_error` rather than `classify_claude_error`. Codex declares `ProbeDelivery::PaneText`, `InterruptDelivery::PaneEsc`, `StopDelivery::ProcessOnly`, `ReapDelivery::ProcessGroup`; the divergence this row was written about (live-session message vs `codex exec resume`) was dissolved by the TUI pivot rather than bridged, and the probe-reply _read_ — broken for every Codex probe — now normalises through the run's own driver. **Unbuilt:** `CodexDriver::classify_error` returns `Indeterminate` unconditionally, so Codex has no provider-specific transient-error classification. See [G-10](#g-10-controlverbs).                                                                                                                              |
| A-8  | Implement the post-hoc interception degrade path                                                      | `medium`  | **New** — deferred                                                                                                                    | Landed: `worker_events` dispatches the `Degrade` path at `PostToolUse`, invokes a registered `PostHocInterceptionFn` when present, and emits a visible loss-of-guards signal for bare degrade. Codex declares the capability and does not land there; this row remains as the rationale and record of the completed safety correction.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| A-9  | Widen `WorkerEvent` session identity and `SessionStartSource`                                         | `small`   | **New**                                                                                                                               | `WorkerEvent` requires `session_id` on every variant (`protocol/src/worker_event.rs`) and `SessionStartSource` mirrors Claude's `startup\|resume\|compact`. Codex's identity is `thread_id` and its trigger set is `startup\|resume\|clear\|compact` — a superset. Note the trap: Codex's _hooks_ say `session_id` while its _stream_ says `thread_id`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| A-10 | `PromptComposition`: driver-supplied enforcement wording                                              | `small`   | **New** — deferred                                                                                                                    | `worker_setup.rs:364` tells the worker _"A PreToolUse hook blocks these"_. The original pass rated this a correctness defect because the sentence was false for a Codex worker; under hook-based interception **it is true for both drivers**, so the defect is gone and this is hygiene. Still worth doing — shared prompt prose should not hardcode one driver's mechanism name — and it becomes live again when the `PATH`-shim project changes what actually enforces. Deferred, not closed.                                                                                                                                                                                                                                                                                                                                                                                                       |
| A-11 | Resolve or delete `progress_fidelity()`                                                               | `trivial` | **New**                                                                                                                               | Landed: spawn records each driver's fidelity on the live-worker slot, and the stale-worker sweep consults `ProgressFidelity::stale_threshold_secs`. A Codex driver's declared tier now affects stale detection.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| A-12 | Extend the reference-driver conformance harness to cover transport and turn boundaries                | `medium`  | **Amends [T-22](#t-22-extend-the-reference-driver-conformance-harness-a-12-amends-t1483)**                                            | The conformance harness (blocked on [G-8](#g-8-structuredoutput) and [T-08](#t-08-permissionpolicy-artifacts-signature-p1422-amendment-a-2-amends-t1479)) was scoped against a Claude-shaped driver. It must also assert: stdout-JSONL ingress produces the same `WorkerEvent` sequence as hook ingress; a turn boundary drives completion identically from either source; and a pinned agent-CLI version is verified, given Codex's unversioned stream ([OQ-2](#oq-2)).                                                                                                                                                                                                                                                                                                                                                                                                                               |
| A-13 | Thread `command_execution` exit status through; surface denials as a `Notification`                   | `small`   | **New**                                                                                                                               | Landed. `item.completed` carried `exit_code` / `status` that the normaliser read and then discarded; a write denied by `--sandbox read-only` inside a compound shell command reports outer `exit_code:0` / `status:"completed"` regardless, so exit status alone can never detect it ([write-up](#sandbox-denials-are-invisible-to-exit-status-alone--a-distinct-failure-signal-is-needed)). The fields are threaded through and a best-effort text-heuristic classifier emits an additional `WorkerEvent::Notification`, reusing the existing operational-warning channel rather than reshaping `PostToolUse`. **The stdout half of this row is now moot** — that dialect was deleted; recovering an exit code on the rollout dialect turned out to need the cell-envelope peel, not a field read ([above](#the-rollout-records-cells-not-commands--the-largest-single-divergence-from-this-design)). |
| A-14 | `Capability::CommandOutcomeObservation`: split per-command outcome fidelity out of `ProgressFidelity` | `small`   | **New — landed this revision**                                                                                                        | `ProgressFidelity::Rich` measured event cadence only, but its doc comment and Codex's declaration read as if it also implied reliable per-command exit status. It does not: Codex's rollout `exit_code`/`status` fields are sometimes absent, projection-dropped, or truncated-unparseable, and the normaliser never read them anyway. Added the capability (Claude declares it, Codex/Grok do not, `Degrade`-never-`Synthesize` on absence) and corrected the `ProgressFidelity` and `progress_fidelity()` doc comments to stop conflating cadence with outcome fidelity. Feeds the [load-balancing seam](#load-balancing-seams) needing a tri-state, "observed"-bit-carrying per-command outcome, which remains future work.                                                                                                                                                                         |

| A-15 | File-tail `ProgressIngress` needs a durable resume point and a driver-owned session snapshot | `large` | **New — landed** | `readopt_live_worker` restored three things and never re-established progress ingress, so a file-tailing driver came back from an engine restart alive and unobserved — no tail, no turn boundary, no completion, holding a slot and a cube lease indefinitely. Near-harmless under one-turn-per-process; fatal under a persistent session. Adds `work_runs.progress_ingress_checkpoint` (`not_file_ingress` / `armed` / `attached`), a checkpoint written **after** dispatch at **record** granularity, `ProgressSessionNormalizer::resume_state` / `restore_resume_state` so the driver's own correlation state survives the restart, and loud refusal — no attach-at-zero, no attach-at-EOF — into a `progress_ingress_unrecoverable` attention item. Applies to any driver whose ingress is a tail rather than a push. See [above](#ingress-must-survive-an-engine-restart-and-a-persistent-session-is-what-made-that-mandatory). |
| A-16 | `pane_monitor_spec()` must not silently default to the reference driver's markers | `small` | **New — landed** | A trait method with a `None` default is an unfilled contract, and the app's fallback to `PaneMonitorSpec.claudeDefault` turned that silence into a wrong answer: every Codex pane was pinned to `notDetected` while Claude's busy marker matched Codex verbatim. Same failure shape as `agent_rules_destination` — an abstraction that names a per-driver convention while the engine keeps a reference-driver assumption behind it. Codex now declares measured markers; the general fix for the abstraction is that a driver-specific scrape contract should fail loudly when undeclared rather than fall back. See [Pane monitor markers](#pane-monitor-markers-a-driver-supplied-contract-the-driver-never-filled-in). |

**Verdict on the existing abstraction tasks, as required by the brief:** the call-site cutover has landed — no `ClaudeDriver` is constructed anywhere in `engine/core` today. The registry-backed model menu and driver-local effort work landed. The shared structured-output file contract is present and is what the Codex driver uses. The permission-artifacts extraction (A-2) is **still open**, and its shape changed: `ClaudeDriver::write_permission_config` no longer panics — it returns empty `PermissionArtifacts` with a comment that porting the settings renderer out of `worker_setup` remains follow-on. That is a generically-callable method that silently produces nothing for the reference driver, which is a weaker failure mode than the original `unimplemented!()`, not a stronger one. PR #2361's trait-method work landed and the synthesizer stays separate (A-4). The transport split landed (A-1) and **seam 5 is closed** — the engine tails the run-correlated rollout — with A-15 the follow-on that made it survive a restart. The conformance-harness row (A-12) is partly satisfied by the Codex guard-conformance work; its cross-transport clause is moot now that only one Codex dialect exists.

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

**The detection half now has a mechanism, and it is not one of the candidates listed above.** Codex still emits no `hook_started` / `hook_completed` record into any stream Boss reads, so Boss supplies its own: every materialised guard runs under a trace shim that appends its decision to `$CODEX_HOME/guard-trace.jsonl`, and the rollout progress session reports that trace at each turn boundary — including, when a turn ran tool calls with **no** guard record, a `[codex-guards-silent]` notification logged at `error`. "Hook ran and allowed" and "hook was silently skipped" are now distinguishable per turn, from Boss's own signal rather than Codex's (`tools/boss/docs/investigations/codex-pretooluse-guard-coverage-2026-07-29.md`).

**This is the one hard sequencing edge in the graph.** Hooks are Codex's guardrail carrier, so this must land and answer both halves — provisioning _and_ detection — before the first Codex worker runs. It replaces the withdrawn "shims must land first" constraint, at a fraction of the scope. If the answer is that trust cannot be provisioned deterministically, escalate: the fallback is promoting the `PATH`-shim project ([T-02](#t-02-relocate-command-guardrails-to-path-shims-follow-on-project), [T-03](#t-03-relocate-editorial-enforcement-to-a-gh-path-shim-follow-on-project)) back ahead of Codex, which is a scope decision for the operator, not for this task.

**Landed, and it became a crate rather than a write-up.** The finding was that `trusted_hash` is a SHA-256 over the normalised hook identity (event label + matcher + command handler fields), matching Codex's own `command_hook_hash`, keyed `{abs_config}:{event}:{group}:{handler}` — deterministic, and therefore stampable by Boss into the per-run `config.toml`. `boss-engine-codex-hook-trust` stamps it, additionally records the content SHA-256 of each guard executable (Codex's own hash covers identity, not file bytes), and then **observes** rather than assumes: a live `codex app-server` `hooks/list` must report `trustStatus=trusted`, `enabled=true` and a matching `currentHash` for every required hook, under a wall-clock timeout, with an empty list, a missing hook, an RPC error or a hang all refusing the worker. Silence is not success. `--dangerously-bypass-hook-trust` is never passed — it would also trust project-local `.codex/` hooks from the repo under work. The detection half is Boss's own guard trace, plus a per-turn re-verification of the armed chain on disk (mono#2408 / mono#2547 / mono#2561; `investigations/codex-hook-trust-provisioning-2026-07-26.md`).

- **Effort:** `small`
- **Depends on:** none
- **Scope:** in-scope — **gated [T-11](#t-11-codexdriver-spawn-and-workspace-provisioning) and everything downstream of it; status: landed, gate met**

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

Extract the remaining Claude permission rendering behind the existing `PermissionArtifacts { config_files, extra_args, env }` shape; port `worker_setup`'s settings and deny-rule rendering into the driver crate before completing that extraction.

**Still open, and its failure mode got quieter rather than louder.** `ClaudeDriver::write_permission_config` no longer panics — it returns `PermissionArtifacts::default()` so that generic call sites shared with Codex do not blow up — while `render_settings_json` and the deny-rule builders stay in `engine/core/src/worker_setup.rs`. Codex genuinely uses the trait method (it is where the sandbox args, hook TOML and trust attestation are produced); Claude silently produces nothing through the same method and is served by the old path. That is a live trap for the next driver author, who will read the trait method as the contract.

- **Effort:** `medium`
- **Depends on:** none
- **Scope:** in-scope — **status: open**

### T-09 Resolve driver at every call site (existing T3324)

The cutover: replace every hardcoded `ClaudeDriver` construction with a registry resolution. Listed here as an explicit dependency edge because a Codex driver cannot be exercised until it lands.

**Landed.** No `ClaudeDriver` is constructed anywhere in `engine/core` today; every call site resolves through the registry. It did not, in the event, need T-08 to finish first — the empty-artifacts stub above is what let the cutover proceed with the extraction still outstanding, which is both why it landed early and why T-08 has stayed open since.

- **Effort:** `large`
- **Depends on:** PR #2361, PR #2355, T-08
- **Scope:** in-scope — **status: landed**

### T-10 `CodexDriver` skeleton: descriptor, capabilities, model menu

The crate and struct: `DriverDescriptor` (`AGENTS.md`, `.codex`), `CapabilitySet` per this design, and a `ModelMenu` sourced from `codex debug models`. No spawning yet.

**Landed, with one shortcut still standing.** The descriptor, the capability set and the registry entry shipped as designed, and unimplemented trait methods were left as `unimplemented!()` rather than silent no-ops so a declared capability could not quietly do nothing. The `ModelMenu` is a **baked snapshot** of `codex debug models` on the pinned CLI, not the runtime read [G-4](#g-4-modelandeffortmenu) argued for — per-model effort filtering is still follow-on. Given that the model list and the effort ladder both moved across eight minor versions, the snapshot is exactly the artifact G-4 warned would go stale; what mitigates it today is the conformance check that fails when the installed CLI's table drifts from the checked-in fixture, which converts staleness into a build failure rather than a wrong menu.

- **Effort:** `medium`
- **Depends on:** T-09
- **Scope:** in-scope — **status: landed; runtime model menu outstanding**

### T-11 `CodexDriver` spawn and workspace provisioning

Implement `spawn_invocation` and `provision_workspace` (per-run `CODEX_HOME`, credentials, `AGENTS.md`, pre-stamped project trust, config-migration prompts disabled). Produces a Codex worker that starts, but whose progress is not yet observed end-to-end.

**As built, four details in this row's original wording turned out to be wrong** — each is written up where it belongs and listed here because together they are why this row took several PRs rather than one: the spawn line is the bare TUI, not `codex exec --json` and not `< /dev/null` ([Execution shape](#execution-shape)); credentials are a locked byte-copy with refresh adoption, not an `auth.json` symlink ([Auth](#auth-and-coexistence-with-claude)); the migration-suppression key is `notice.external_config_migration_prompts.home`, not a top-level boolean, and the invalid form killed every dispatch at config load ([Claude-Code interop](#claude-code-interop--a-coexistence-hazard)); and `AGENTS.md` goes to `$CODEX_HOME`, not `<workspace>/.codex/` ([Config discovery](#config-discovery-and-isolation--the-concurrency-question)). A fifth was pure environment: cube workspaces are non-colocated jj workspaces with a `.jj` and no `.git`, so the retired `exec` shape additionally needed `--skip-git-repo-check` — a flag that is a hard argument error on the TUI that replaced it.

**Pane launch half-answer from the viability spike:** the CLI line is fine in a pane (positional prompt auto-runs). What is **not** answered by implementing spawn alone: how the engine observes that pane. T-11 must either (a) document that progress depends on a later [seam 5](#the-five-engine-seams-this-needs) decision and leave observation to a follow-on task, or (b) implement the chosen channel once that decision exists. Spawning without a chosen ingress is a valid intermediate milestone; claiming "pane-hosted Codex works" without seam 5 is not. **It took (a), correctly and explicitly** — the spawn PR said so in its own notes rather than implying end-to-end observation, which is the behaviour this paragraph was asking for.

**Includes Codex's guardrail wiring**, which the [operator decision](#operator-decision) puts here rather than in a separate shim project: emit Boss's existing guard scripts (the path/checkleft scripts begin at `worker_setup.rs:972` and `:1131`, with wiring at `:580-610`) plus editorial enforcement into `CODEX_HOME`'s `[[hooks.PreToolUse]]` TOML, and stamp hook trust per T-01's finding. The guard-script emission is currently hardcoded to Claude settings-file grammar and must become driver-supplied — the scripts themselves are reusable as-is, since Codex's payloads carry `tool_name: "Bash"` and Claude's `tool_input` shape ([D-2](#deltas-that-change-the-design)). Handle the inline-`--body` editorial case as a `Deny` with a corrective reason, per [the editorial case](#the-editorial-case-precisely).

**Correction, measured on the models Boss actually dispatches** (`tools/boss/docs/investigations/codex-pretooluse-guard-coverage-2026-07-29.md`): "the scripts are reusable as-is" holds for the **shell** path and only for it. `tool_name: "Bash"` and `tool_input.command` survive code mode — Codex hooks the inner `tools.exec_command` call, not the JavaScript cell that wraps it, so a code-mode model's payload is byte-shaped like `gpt-5.5`'s. But Codex's tool surface is wider than Claude's in three ways the shared scripts do not cover, two of which made the worker prompt's "pushes are blocked" assertion false in practice: `apply_patch` carries its target paths in the patch body under its own tool name (no `file_path` key, so the data-directory gate approved every Codex file edit unread); `mcp__codex_apps__github_*` tools can push, open and merge PRs with no shell command for any command guard to see; and `write_stdin` feeds an already-running process new command lines with **no hook event at all**, which no matcher change can reach — only refusing the interactive session that would receive them. All three are closed; the guards additionally fail **closed** on a payload they cannot parse, and every guard now runs under a trace shim so "did the guard fire, and what did it decide?" is answerable per execution.

- **Effort:** `large`
- **Depends on:** T-01, T-10
- **Scope:** in-scope — **status: landed**

### T-12 `CodexDriver` progress normaliser

Map Codex's dialect onto `WorkerEvent` with a reader-owned normaliser handling `session_meta`, `event_msg`, and correlated `response_item` tool calls/outputs, feeding the generic reader and ordered fan-out.

**This row originally called for two normalisers, one per dialect. Only one survives.** The stdout-JSONL dialect (`CodexProgressSession`, `StdoutEnvelope`, `parse_stdout_envelope`) was written, shipped, and then deleted as unreachable: the driver never selects `StdoutJsonl`, and the engine's ingress activation had no arm for it, so a driver that did declare it would have stalled in `Spawning` forever. Roughly 1,150 lines went with it (mono#2572). The lesson is worth keeping: a normaliser was maintained and tested for months against synthetic fixtures for a transport no production path could reach, and the tests passing is exactly what made that invisible.

Three constraints from the 0.145.0 delta pass, each a real trap: item IDs are **0-based** and must not be treated as ordinal or 1-based; `error`-typed records carry **operational warnings as well as** turn failures, so they must not be mapped unconditionally to a failed turn; and the `TurnItem` enum grew by four variants across eight minor versions, so unknown variants must be ignored-with-logging rather than rejected. A fourth, learned in implementation: the rollout is a cell harness, so normalisation needs a stateful correlation stage — [see above](#the-rollout-records-cells-not-commands--the-largest-single-divergence-from-this-design).

- **Effort:** `large`
- **Depends on:** PR #2361, T-11
- **Scope:** in-scope — **status: landed; the second normaliser this row called for was built and later deleted**

### T-13 Widen `WorkerEvent` session identity and `SessionStartSource` (A-9)

Accommodate Codex's `thread_id` and its `startup|resume|clear|compact` trigger set. Small and mechanical, but it touches `boss-protocol` and therefore every consumer, so it is its own PR. **File overlap:** co-edits the driver normalisers with T-12 — land T-12 first, and forward-port its mappings preservingly.

**Landed.** `SessionStartSource` carries `Clear` alongside `Startup`/`Resume`/`Compact` (plus `Other`), and session identity is preserved across process boundaries. The `session_id` (hooks) versus `thread_id` (stream) naming trap this row flagged did not bite — but the identity work turned out to matter for a reason nobody wrote down here: it is what makes a resumed ingress able to prove it is reading the same session's file after an engine restart ([A-15](#proposed-p1422-amendments)).

- **Effort:** `small`
- **Depends on:** T-12
- **Scope:** in-scope — **status: landed**

### T-14 Driver-supplied PR-URL extraction (A-5)

PR-URL capture remains triggered by shared `PostToolUse`, while the driver supplies dialect-specific feed text. Codex rollout capture scans correlated `response_item.payload.output` from both observed output variants and reuses the shared URL matcher/command gates.

**Landed, then found dead, then fixed.** The feed gates on the normalised tool name being `Bash`; `exec`/`exec_command` are reshaped to it but `wait` was not, so under the code-mode cell harness the record carrying the `cube pr create` URL was attributed to a tool named `wait` and dropped while the yield placeholder was fed to capture instead. Because a cold build of `//tools/cube:cube` routinely pushes `cube pr create` past the model's chosen return window, the primary path was effectively dead for every Codex run and success depended on the optional fallback artifact. Fixed by correlating the continuation back to the originating call inside the tracker, so capture needs no `wait` special case and never learns one was involved ([above](#the-rollout-records-cells-not-commands--the-largest-single-divergence-from-this-design), mono#2546).

**Ordering note:** PR #2361 rewires the trigger onto the turn boundary; land this after #2361. This is not a duplicate: `pr_url_capture.rs:1-6` is still derived from `PostToolUse` events.

- **Effort:** `medium`
- **Depends on:** T-12
- **Scope:** in-scope — **status: landed**

### T-15 `StructuredOutput` trait method and `--output-schema` wiring (A-5)

Put `StructuredOutput` on the trait and have the Codex driver use `--output-schema` / `--output-last-message` alongside the shared `BOSS_STRUCTURED_OUTPUT` file contract. Depends on T1476 landing the file contract first.

**Verification note:** the `BOSS_STRUCTURED_OUTPUT` file contract already exists at `spawn_flow.rs:59`. Verify whether any remaining T1476 work is still a prerequisite; do not silently discard that dependency.

**Not done, and Phase 3 supplied the concrete argument for it.** `CodexDriver::structured_output_wiring` still uses only the common-denominator environment-file contract; `--output-schema` / `--output-last-message` are named as a future extension and passed nowhere. That was defensible while the file contract worked — but a Reviewer-postured Codex worker runs `--sandbox read-only`, which denies every write **including its own sanctioned artifact write**, and `--add-dir` cannot carve out an exception. So for one whole worker kind the primary channel is structurally unreachable and the run depends on transcript-scraping a fenced JSON block ([T-25](#t-25-codex-eligibility-for-review-and-conflict-resolution-kinds)). A channel that does not require the sandboxed process to write a file is not a nicety for Codex; it is the only way that kind gets a primary path.

- **Effort:** `medium`
- **Depends on:** T-14
- **Scope:** in-scope — **status: open**

### T-16 `TranscriptAccess`: driver-supplied path discovery (A-6)

Discover Codex's rollout path (the local timestamp in the filename blocks pure path construction from `thread_id`) and generalise `engine/transcript-tail` beyond its "claude transcript files" framing at the **container** level only. Keep a separate line normaliser for the rollout dialect. `transcript_path_for_session()` is already on the trait, and `live_status_loop` already calls `normalize_transcript_entry`.

**Landed, with discovery on the other side of the seam than this row assumed.** The tailer is generalised and driver-agnostic. Discovery is not a driver-side glob: the engine's ingress snapshots candidates under the run-private `CODEX_HOME` before spawn and correlates the one new file by `session_meta` identity, because only the engine knows when the spawn happened. The driver supplies a containment root and the identity predicate. `CodexDriver::transcript_path_for_session` returns `None` and documents why. See [G-9](#g-9-transcriptaccess).

- **Effort:** `medium`
- **Depends on:** T-12
- **Scope:** in-scope — **status: landed**

### T-17 `ControlVerbs` on the trait, plus Codex probe/nudge (A-7)

Put probe/interrupt/stop/reap on the trait, route `transient_recovery.rs` through `classify_error` instead of `classify_claude_error`, and implement Codex probing.

**Landed, and re-scoped mid-flight.** The task was written to implement probing as `codex exec resume` — a new process reattaching to the thread, with delivery confirmed by observing a fresh `turn.started` — and the CLI half of that was spiked and worked. The TUI pivot deleted the need for it: Codex is a persistent session, so probe is `ProbeDelivery::PaneText` down the same pane path Claude and Grok use, and interrupt is `InterruptDelivery::PaneEsc` (Esc aborts the turn, the process survives, and `turn_aborted` reaches a real `Stop(Interrupted)`). Delivery confirmation needed one genuine fix that had nothing to do with resume: the probe-reply read scanned the transcript for Claude's `type == "assistant"` shape and returned `None` for every Codex probe, and now normalises through the run's driver.

**Not done:** Codex's `classify_error` is a stub returning `Indeterminate`, so provider-specific transient recovery (rate limit, quota, auth expiry) is unimplemented for Codex.

- **Effort:** `large`
- **Depends on:** T-12
- **Scope:** in-scope — **status: landed except Codex error classification**

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

**Partly landed, and its most valuable assertion was one this row never anticipated.** The spawn-line contract is pinned (required and forbidden flag sets, restated rather than relaxed across the exec→TUI rename), and a live probe loads the exact `config.toml` production writes under `--strict-config` on the pinned binary — the check that would have caught the config-key blocker, since every prior test pinned the stream contract and none pinned the config schema. A guard-conformance module then pins the _tool surface_: every model the driver can dispatch is checked against a captured `tool_mode` fixture hermetically (so it enforces under `bazel test`'s sandboxed `PATH` instead of soft-skipping to a silent pass), a live companion fails on `codex debug models` drift, and an opt-in probe drives the real driver end to end and asserts per-step `(tool_name, tool_input key set, aggregate guard decision, guard-name set)` (mono#2447, mono#2563). **The cross-transport clause is moot** — asserting stdout and hook ingress produce identical `WorkerEvent` sequences has no subject now that the stdout dialect is deleted and Codex declares neither hook ingress nor stdout.

- **Effort:** `large`
- **Depends on:** T-12, T-14, T-15, T-16, T-17
- **Scope:** in-scope — **status: config-schema and guard/tool-surface conformance landed; cross-transport clause moot**

### T-23 Phase-1 acceptance sweep: 10 Codex chores to green PRs

Dispatch 10 consecutive chores with `--driver codex` and verify each reaches an open PR with green CI, no engine intervention, and primary-path PR-URL capture. A sweep, not an implementation — listed separately and after the work it validates.

**Not run.** No merged work in this project executes it, and the two phases gated behind it were enabled anyway. The failures it would plausibly have caught were each found by other routes and at higher cost — the three dispatch-killing config/flag/rules blockers, the dead primary PR-URL path, and the invisible provider-error path were all discovered from live incidents or targeted investigations rather than from an acceptance sweep. That is the strongest available argument for actually running it: every one of those was a _first-dispatch_ failure, and eight PRs of build-out preceded any attempt to run a dispatch end to end.

- **Effort:** `medium`
- **Depends on:** T-22
- **Scope:** in-scope — **status: outstanding**

### T-24 Codex eligibility for design / investigation / postmortem kinds

Phase 2: enable the document-producing kinds via `KindRequirements` once the structured-output contract is proven, and verify a Codex-authored design doc's task breakdown parses and materialises followups.

**Landed as an enforcement change; the live half is outstanding.** `KindRequirements::for_kind` escalated only `Design` to require-strict `StructuredOutput` + `ToolUseInterception`; `Investigation` and `DesignPostmortem` fell back on each capability's default `Degrade` disposition despite delivering the same doc-plus-breakdown contract. All three now escalate together, matching the grouping `ReasoningMode::default_for` already uses. The parse/materialise pipeline was confirmed driver-agnostic by inspection — no driver-conditional code exists in `planner.rs` / `populator.rs` / `design_detector.rs` — and pinned by a populator regression test over a Codex-shaped task breakdown. The acceptance criterion as written ("a Codex-authored design doc lands... and its followups materialise" against a _real_ dispatched execution) needs a live engine with real project/repo state and was not run (mono#2615).

- **Effort:** `medium`
- **Depends on:** T-23
- **Scope:** in-scope — **status: gate landed; live acceptance outstanding**

### T-25 Codex eligibility for review and conflict-resolution kinds

Phase 3: verify `--sandbox read-only` is a genuine reviewer-read-only equivalent (including that the worker demonstrably cannot write), and that structured `ReviewResult` output round-trips. **Additionally evaluate `codex exec review`** — a native non-interactive review mode found in the 0.145.0 pass, with `--base` / `--commit` / `--uncommitted` and a dedicated `codex-auto-review` model ([D-3](#delta-that-changes-a-tasks-scope), [OQ-6](#oq-6-codex-exec-review)). It may fit Boss's review kind better than a general exec run, or may impose an output shape that does not match `ReviewResult`; decide between the two rather than defaulting to the general path.

**Both verifications ran live against the driver's real bare-TUI spawn shape** ([`codex-review-eligibility-sandbox-and-structured-output-2026-07-31.md`](../investigations/codex-review-eligibility-sandbox-and-structured-output-2026-07-31.md)). `codex exec review` was not re-evaluated as an alternative substrate — see the [OQ-6](#oq-6-codex-exec-review) update: it is forgone for the same spawn-shape-contract reason `codex exec` itself was, by the operator's binding single-shape decision, not re-opened here.

- **`--sandbox read-only` — confirmed genuine and OS-enforced.** A live write attempt failed with a shell-level `operation not permitted` and left no file on disk; a control run of the identical prompt under `danger-full-access` in the same harness succeeded and did write the file, so the harness itself is validated, not just the negative result.
- **`ReviewResult` round-trips, but only through the transcript fallback, unconditionally — and only as of a fix landed alongside this investigation.** `--sandbox read-only` denies every write, including the reviewer's own sanctioned `$BOSS_STRUCTURED_OUTPUT` artifact write outside the workspace — confirmed with a real reviewer prompt and a real diff. `--add-dir` cannot carve out an exception (`read-only` rejects it outright: "Switch to workspace-write or danger-full-access to allow them"). The primary artifact channel is therefore structurally unreachable for a Reviewer-postured Codex worker on every run, not intermittently. The live run showed the _model_ delivering a valid fenced `ReviewResult` when the write failed, but the engine's transcript-fallback path (documented in `finalize_pr_review_pass` as "TRANSITIONAL") could not have ingested it: `CodexDriver::structured_output_fallback` returned zero candidates for every kind at the time of that run. That gap is fixed — the fallback now delegates to the same driver-neutral fenced-JSON scraper the Claude driver uses — and verified by a unit test replaying the captured transcript, not by a second live run.
- **Bug found and fixed along the way:** the fallback's own retry probe told a worker to "write it to this file with the Write tool" — Claude's tool name, an instruction a read-only-sandboxed Codex reviewer can never satisfy. Fixed to be driver-agnostic (names the path, offers the fenced-JSON fallback explicitly, names no tool) — see `core/src/completion/finalize_passes.rs`. This was a real bug in shared code independent of Codex; a Codex reviewer that hit the retry path before this fix would have burned its nudge budget on an unsatisfiable instruction and then silently advanced the PR without a revision.
- **The decisive finding: none of this is reachable in production yet, for a reason unrelated to capability fidelity.** `REVIEWER_POOL_DRIVER` (`core/src/coordinator.rs:1503`) hardcodes `"claude"` for every review-pool and automation-pool dispatch, unconditional on the reviewed row's own driver — by an existing, deliberate invariant ("who authored a change must not determine who reviews it"). There is currently no seam to select Codex as a reviewer at all. Flipping that is a dispatch-policy decision adjacent to the explicitly out-of-scope load balancer, not a capability-gate fix, so it is **not** done here — it is named as the concrete seam this project exists to surface for that later decision.
- **Conflict-resolution needed no equivalent verification.** It dispatches under `WorkerKind::Standard` (ordinary writable sandbox, no OS-enforced read-only), designates no structured-output payload, and already runs on the row's own driver via the path T-23's general acceptance sweep exercises. Nothing kind-specific applies.

- **Effort:** `medium`
- **Depends on:** T-24
- **Scope:** in-scope — **status: verification landed; production reachability blocked on the reviewer-pool driver pin**

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

### T-30 Surface sandbox/command denials as a distinct `Notification` signal (A-13)

Thread `command_execution`'s `exit_code` / `status` through the normaliser (they were read off the raw envelope and discarded before `WorkerEvent::PostToolUse` was built), and add a Codex-local heuristic classifier over tool-output text for known OS write-denial phrasings (macOS Seatbelt: `Operation not permitted`, verified). On a match, or on a genuine `status:"failed"`, emit an additional `WorkerEvent::Notification` alongside the ordinary `PostToolUse` — do not reshape `PostToolUse` / `tool_response`, which PR-URL capture depends on verbatim. Explicitly a best-effort visibility signal, not a guardrail: it does not block or retry, and the phrase list is neither exhaustive nor free of false positives. See [the write-up](#sandbox-denials-are-invisible-to-exit-status-alone--a-distinct-failure-signal-is-needed) for the empirical basis.

**Landed, and the exit-status half needed more than threading a field.** On the dialect Boss actually reads, the exit code sits at the top level of a JSON chunk embedded as text behind the cell harness's own prose header, which is why every Codex command — including exits 7, 9 and 137 — classified as non-error. Making it reachable required peeling the cell envelope, not reading a different key ([above](#the-rollout-records-cells-not-commands--the-largest-single-divergence-from-this-design), mono#2509 / mono#2546).

- **Effort:** `small`
- **Depends on:** T-12
- **Scope:** in-scope — **status: landed**

### Parallelism

The graph below is how the work was ordered. It held on the dependency edges and did not hold on the acceptance edge: T-24 and T-25 both ran with T-23 outstanding, because a gate placed at the end of a phase can only hold back the _next_ phase, and every implementation row lives inside the phase it terminates. A phase gate that is meant to bind should sit in front of the work it qualifies, not behind it.

At the same depth, these may run in parallel:

- **Depth 0:** T-01, T-08, T-27 — genuinely independent. **Start T-01 first regardless of slack:** it is the only hard gate, it is `small`, and T-11 cannot land without it.
- **Depth 1:** PR #2361 supplies the in-flight turn-boundary routing; T-12 follows T-11 and that PR.
- **Depth 2:** T-12 supplies the Codex normaliser. T-13, T-14, T-16, T-17, and T-30 follow their stated edges.
- **Not in this graph:** T-02 and T-03 belong to the follow-on `PATH`-shim project and are independent of everything above.

**File-overlap cautions — order these rather than running them concurrently:**

- **T-12 and T-13** both edit the driver normalisers. Land T-12 first; T-13 integrates rather than replaces its mappings.
- **T-02 and T-03** both edit `worker_setup.rs` guard-script emission and `BOSS_BIN_DIR` provisioning. The dependency edge serialises them; keep it. Both also collide with **T-11**, which makes the same guard-script emission driver-supplied — a further reason the shim work is better done as a follow-on project than concurrently.

T-09 is a deliberate barrier: it touches nearly every engine call site, so nothing else should be in flight against those files while it lands.
