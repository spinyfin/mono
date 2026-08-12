# Antigravity (`agy`) as Boss's fourth driver: everything rides on a Boss-owned per-run `HOME`

- **Date:** 2026-08-12
- **Status:** design, pre-implementation. No `AgyDriver` code exists.
- **Kind:** project design doc
- **Subject CLI:** `agy` 1.1.12 (`~/.local/bin/agy`), Google Antigravity, OAuth against a personal Google account
- **Sibling designs:** [`grok-as-a-first-class-interactive-agent-driver.md`](./grok-as-a-first-class-interactive-agent-driver.md), [`codex-as-a-first-class-agent-driver.md`](./codex-as-a-first-class-agent-driver.md), [`agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md`](./agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md)
- **Perishable status/ledger:** kept out of this doc deliberately — see the acceptance-ledger entry in the task breakdown

## The contested property, stated first

**This driver has exactly one isolation mechanism, and no fallback: a Boss-owned per-run `HOME`.**

`agy` exposes no config-directory override, no hooks-path flag, and no per-run permission flag other than a blanket allow-all. Per-run isolation, per-worker-kind permission posture, workspace trust, the conversation store, and the transcript location are _all_ reached by relocating `HOME`. A reviewer can reasonably object that this bets the whole integration on one undocumented behaviour of a closed-source binary that auto-updates. That objection is correct, and it is the thing to land on before any code is written: if scoped `HOME` is unsound inside a GhosttyKit pane, this project has no plan B and should stop rather than improvise one.

## TL;DR / verdict

Technically the integration looks tractable: `agy` has a Claude-shaped hook system with a real `Stop` hook, an interactive TUI suitable for a pane, and — verified in this pass — a state tree that relocates cleanly under a scoped `HOME`.

Economically it looks marginal at best. `agy`'s measured per-request overhead (~10–25k input tokens, ~9.4k floor even cached) against a weekly quota metered across all Antigravity surfaces means a 16-worker fleet is very unlikely to be affordable. The honest expected end state is a **pinned-only, zero-share driver** — reachable by explicit `--driver agy`, receiving no allocated traffic — and this design is built so that state is the _default_, enforced in code, rather than a phase that ends when someone decides it has.

## Goals

1. Add `agy` as Boss's fourth registered driver, running as a first-class **interactive TUI worker in a GhosttyKit pane**, on the same terms as `claude`, `codex`, and `grok`.
2. Resolve the three coupled risks — ingress behind a TUI, per-run isolation, per-worker-kind permission posture — _before_ the driver crate is built, because an isolation strategy that fails later invalidates everything stacked on it.
3. Migrate the driver traffic split from three shares to four, across the Rust protocol, the DB, and the Swift app, without a broken intermediate state.
4. Produce a spend and quota model early enough that "this driver is not economically viable at fleet scale" is a cheap outcome rather than an expensive one.
5. Define a phase gate that is a **state enforced by code**, not a checkpoint that only blocks what follows it.

## Non-goals

- **A JSONL / stream-json dump in the worker pane.** Ruled out by the operator: the pane is a human-readable TUI, full stop. `agy -p --output-format stream-json` is a research and test instrument only. This is a constraint on the design, not an option weighed and rejected, and it is not revisited below.
- **Revisiting the registry's `ProgressIngress::StdoutJsonl` ban** (`engine/driver/src/registry.rs:77`) or its `WorkerProcessLifetime::OneTurnPerProcess` sibling (`:112`). Both are settled and consistent with the pane constraint.
- **Making `agy` eligible for `ConflictResolution` / `CiRemediation`.** Those execution kinds mark `Capability::CommandOutcomeObservation` required-strict; no driver but `claude` declares it. `agy` will not either in v1.
- **PR review on `agy`.** Review dispatches on the review pool's fixed driver by deliberate policy (`work/driver_allocation.rs`, `decide_execution_driver` step 1), so it does not participate in the split at all. Changing that is a change to reviewer dispatch policy, not to this driver.
- **Remote / SSH host support** for `agy` workers. v1 is local-macOS only.
- **Raising `agy`'s traffic share.** This design ships the fourth share at zero and defines what would justify raising it. It does not raise it.

## Method, and what was actually verified in this pass

The project brief arrived with substantial pre-work. Per its own instruction, load-bearing claims were re-checked against the tree and the installed binary rather than taken on trust. What follows separates **verified in this pass**, **corrected**, and **still unconfirmed**.

### Verified against the binary

- `agy --help` flag surface, on 1.1.12: `-i` / `--prompt-interactive`, `--add-dir` (repeatable), `--effort low|medium|high`, `--model`, `--conversation`, `-c`/`--continue`, `--sandbox`, `--mode accept-edits|plan`, `--dangerously-skip-permissions`, `--json-schema`, `--print-timeout`, `--disable-slash-commands`, `--project` / `--new-project`. Subcommands: `agent(s)`, `models`, `plugin(s)`, `install`, `update`, `changelog`, `help`. There is **no** `--no-subagents` and **no** config-dir flag.
- **No `AGY_HOME` equivalent exists.** A symbol sweep of the binary surfaces `AGY_ADC_AUTH`, `AGY_BUSINESS_PAYGO_TIER`, `AGY_CLI_DISABLE_AUTO_UPDATE`, `AGY_CLI_DISABLE_LATEX`, four `AGY_ONBOARDING_*` flags, and a set of `ANTIGRAVITY_*` names that are editor/sidecar/telemetry oriented. One name is worth a probe rather than a conclusion: `ANTIGRAVITY_EXECUTABLE_DATA_DIR`. Its semantics are unknown from strings alone.
- **`HOME` relocation works, and is total.** Running `env HOME=<scratch> agy models` created `<scratch>/.gemini/config/{config.json,mcp_config.json,projects/}` and `<scratch>/.gemini/antigravity-cli/{conversation_summaries.db,brain/,conversations/,presence/,log/,cache/,knowledge/,updater/,installation_id,jetski_state.pbtxt}` — i.e. the entire mutable state surface named in Risk 2 followed `HOME`. It also created `<scratch>/Library/Caches/ms-playwright-go`, so the browser-subagent cache follows too.
- **Scoped `HOME` fails closed on auth**, exiting non-zero with `Error: Please sign in to view available models.` The OAuth credential is `HOME`-scoped, so relocating `HOME` _breaks authentication_ unless the credential is deliberately delegated back. That is a feature for safety and a hard requirement for design.
- **Permission and trust state are `HOME`-scoped files**, and there are two of them: `~/.gemini/settings.json` holds `security.auth.selectedType`, while `~/.gemini/antigravity-cli/settings.json` holds `trustedWorkspaces` (observed on the host with real entries), `colorScheme`, and `enableTelemetry`.
- **`.agents/` is discovered at the root of a workspace directory, not only the repo.** The binary documents `.agents/` (with `.agent/`, `_agents/`, `_agent/` accepted) "at the root of" a workspace path, alongside `GEMINI.md` / `AGENTS.md` / `.agents/rules/*.md` for rules, and states that for a hook "the working directory is set to the directory containing `hooks.json`". Since `--add-dir` adds a directory to the workspace, this is the mechanism that explains the brief's undocumented `--add-dir`-flips-hook-discovery finding — and it means Boss's hook config need not be written into the repo at all.

### Corrected from the brief

- The traffic-split migration precedent is at `work/migrations_b.rs:2687` (`migrate_driver_traffic_split_from_codex_percentage`), not `work/schema_init.rs:732`.
- The registry guard is narrower than "bans stdout-JSONL by construction". `refuse_stdout_jsonl_ingress` asserts only over the built-in slugs constructed in `Default`; `DriverRegistry::with_driver` is deliberately exempt so the shared JSONL reader crate can register synthetic test drivers. The ban is a consistency guard against declaring an ingress that `prepare_progress_ingress` does not wire, not a blanket prohibition on the enum variant. This changes nothing for `agy` — the pane constraint already decides the question — but the guard should be cited accurately.
- "Permission posture is a settings _file_, not a flag" is right about per-worker-kind posture but understates the CLI surface: `--sandbox`, `--mode plan`, and `--mode accept-edits` all exist and are per-run. Whether `--mode plan` is a usable expression of Reviewer read-only is a real question the characterisation task must answer, not assume.

### Still unconfirmed — and therefore gating

Everything in the spike checklist below. In particular: whether hooks fire at all in interactive mode. The captured `Stop` payload is headless-only evidence, and the pane path is the only path that ships.

## What `agy` is, in Boss's vocabulary

| Boss shape                                   | `agy` equivalent                                                                                                                                                                | Confidence                                  |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------- |
| Pane `initial_input` runs an interactive TUI | `agy -i "<prompt>"`                                                                                                                                                             | Flag exists; auto-submit unverified         |
| Turn boundary                                | `Stop` hook, `terminationReason` + `fullyIdle`                                                                                                                                  | Verified headless only                      |
| Per-tool progress cadence                    | `PreToolUse` / `PostToolUse` hooks                                                                                                                                              | Verified headless only                      |
| Model-invocation boundaries                  | `PreInvocation` / `PostInvocation` hooks                                                                                                                                        | Exists; semantics unmapped                  |
| "Awaiting human input" signal                | _none observed_                                                                                                                                                                 | No `Notification`-shaped event in the set   |
| Session identity                             | `conversationId` on every hook payload                                                                                                                                          | Verified in captured payload                |
| Transcript                                   | `transcriptPath` stamped on the `Stop` payload, under `brain/<cid>/.system_generated/logs/transcript_full.jsonl`                                                                | Verified in captured payload                |
| Per-run config isolation                     | scoped `HOME`                                                                                                                                                                   | Verified for state; unverified in a pane    |
| Credential                                   | `$HOME/.gemini/oauth_creds.json` + `google_accounts.json`                                                                                                                       | Verified `HOME`-scoped                      |
| Permission posture                           | `$HOME/.gemini/antigravity-cli/settings.json` (`toolPermission`, `permissions.allow`) plus `--sandbox` / `--mode`                                                               | File verified to exist; keys unverified     |
| Workspace trust                              | `trustedWorkspaces` in the same file; `trustedFolders.json` in `~/.gemini/`                                                                                                     | Files verified; gating behaviour unverified |
| Effort ladder                                | `--effort low\|medium\|high` — three rungs, same shape as Grok                                                                                                                  | Verified                                    |
| Model menu                                   | `gemini-3.6-flash-{high,medium,low}`, `gemini-3.5-flash-{high,medium,low}`, `gemini-3.1-pro-{high,low}`, `claude-sonnet-4-6`, `claude-opus-4-6-thinking`, `gpt-oss-120b-medium` | From `agy models` (brief)                   |
| Subagents                                    | `invoke_subagent` / `define_subagent` / `browser_subagent`, no disable flag                                                                                                     | Verified absent from `--help`               |

## Economics: model this before the crate exists

Boss already records `input_tokens`, `output_tokens`, `cache_creation_tokens`, and `cache_read_tokens` per work execution, so the shape of the question is answerable — but not by a worker, which must never read the engine's DB. The spend model therefore has to be built from direct measurement of `agy` plus published quota terms.

The measured inputs available today: a trivial "reply PONG" run consumed **17,644 input tokens**; per-request overhead is **~10–25k**; the system-prompt and tool-surface floor is **~9.4k even when cached**.

The multiplier that matters is **requests per Boss run, not runs per hour**. Every tool call is a model request, so a Boss chore that makes 40–120 tool calls pays the overhead 40–120 times. At a 15k midpoint that is **0.6M–1.8M input tokens of pure overhead per run**, before any conversation context. Sixteen concurrent workers turning over a few runs an hour puts the fleet in the tens of millions of overhead tokens per hour against a quota that Antigravity meters **weekly** and **across all surfaces** — including the operator's own IDE use of the same account.

Two conclusions follow, and the design is built on them:

1. **A meaningful traffic share is not plausible on a personal-account OAuth quota.** The default four-way split must ship `agy` at zero, and the driver's realistic operating mode is explicit per-row pinning.
2. **The request-count assumption above is the single largest source of error in that estimate, and it is measurable.** That measurement is a task, sequenced in parallel with the spike, and it is allowed to return "stop": if the model says a single pinned chore routinely exhausts a week of quota, the correct outcome is to record that and not build the crate.

This is also a convenient convergence rather than a coincidence: the economics and the acceptance gate independently produce the same posture — registered, zero-share, pinned-only.

## Alternatives considered

### A. Live progress ingress by tailing the transcript file, as Codex does

Codex takes `ProgressIngress::AgentJsonlFile` and explicitly leaves "hooks as the `ToolUseInterception` transport only" (`codex.rs:1611-1616`). Mirroring that for `agy` is a genuine option, and tailing a file is fully compatible with a TUI pane.

Rejected, on three checkable grounds:

- **Hooks are mandatory regardless.** `Capability::ToolUseInterception` cannot be served by reading a file after the fact — the guard must decide before the tool runs — and it is marked required-strict for the design-family kinds (`lib.rs`, `KindRequirements`). So the hook transport is being wired either way; a second live ingress adds a failure surface without adding a capability.
- **The precedent does not transfer.** Codex tails _because its hook surface is too thin to carry progress_, not because tailing is preferable. `agy` has `PreToolUse` / `PostToolUse` / `Stop`, which is the same cadence Claude's hooks provide. The reason for Codex's choice is absent here.
- **The path shape does not fit the existing ingress type.** `AgentJsonlFileIngress` matches a flat `directory` + `filename_prefix` + `filename_suffix`. `agy`'s transcript lives at a _nested_ per-conversation path, `brain/<cid>/.system_generated/logs/transcript_full.jsonl`, where `<cid>` is not known before spawn. Making this fit needs either a protocol change to the ingress type or a Boss-assigned conversation id via `--conversation` that may not accept an id for a conversation that does not exist yet. Both are avoidable dependencies.

The transcript file is still used — for `Capability::TranscriptAccess`, resolved post-hoc from the `transcriptPath` the `Stop` payload already stamps. That is what `transcript_path_for_session` is for.

### B. Reuse `ClaudeDriver` behind a compatibility shim rather than adding a driver

`agy`'s hook event names are Claude-shaped, it reads `AGENTS.md`, and its hook contract is the same stdin-JSON / stdout-JSON / `type: "command"` shape. A shim over `ClaudeDriver` looks cheap.

Rejected: Grok already ran this experiment and it produced the worst possible failure mode. Grok's `PreToolUse` accepts Claude's payload shape but **silently fails open** on Claude's `block` verdict, reserving `block` for stop-gates — so wiring Claude's five guards unmodified "would run every guard, always approve, and look identical to a healthy configuration" (`grok/hooks.rs` module docs, grounded in `grok-pretooluse-decision-vocabulary-and-tool-name-map.md`). `agy`'s verdict vocabulary is _known to differ already_ — `allow` / `deny` / `ask` / `force_ask` — so this alternative starts from the exact condition that produced the Grok defect. Separately, the registry is keyed by slug and every consuming call site resolves through it (`registry.rs`, `require`); a shim would have to lie about the slug or about the capability set, and `agy` genuinely differs on capabilities (no `Notification`-shaped event, therefore no `AwaitingInputSignal`) and on the model menu.

### C. One shared `HOME`, with runs separated by `--project` / `--new-project` / `--conversation`

`agy` has real first-class project and conversation separation, so keeping the operator's single `~/.gemini` and separating runs _inside_ it is the obvious low-friction option, and it avoids the auth-delegation problem entirely.

Rejected, because the requirement it fails is a real one and not an artifact of having already chosen scoped `HOME`:

- **Per-worker-kind permission posture is a hard requirement with an exhaustive match behind it.** `worker_setup.rs` derives `WorkerKind` from `ExecutionKind` in a deliberately exhaustive match so a new kind must decide its posture, and `WorkerKind::forced_permission_mode` exists so a restricted kind's allowlist "cannot be silently downgraded". `agy` expresses posture in `settings.json` — **one file per `HOME`**. Under a shared `HOME`, a Reviewer and a Standard worker running concurrently cannot hold different postures. That is not a tidiness loss; it is the security property failing.
- **The remaining state is process-global within a `HOME` regardless of project separation:** one `conversation_summaries.db` (SQLite, with `-wal`/`-shm` observed), one `presence/` lock directory, one `settings.json`, one updater. Sixteen concurrent writers to those is the correctness risk Risk 2 names.
- The auth-delegation problem it avoids is real but small, and Grok has already solved the same problem shape (`GROK_AUTH_PATH` plus a wait on the CLI's own refresh lock).

## Chosen approach

### The invariant, stated at the load-bearing level

Not "each run gets its own `HOME`" — that is the container, and an implementation could satisfy it while symlinking the conversation DB back to a shared one and breaking everything downstream. The invariant is:

> **No two concurrent `agy` runs may share a mutable `agy` state artifact — the conversation DB, `settings.json`, the presence lock directory, `brain/`, `conversations/`, or the updater state — and the operator's own `~/.gemini` is never one of those artifacts for any run.**
>
> Exactly one artifact is deliberately shared, and it is shared because it is a credential rather than state: the OAuth credential file, plus whatever lock the CLI itself uses to serialise refreshes of it.

A per-run `HOME` is the mechanism that satisfies this. The invariant is what tests and posture assertions must check.

### The equivalence to `GROK_HOME`, and the dimension it holds on

Scoped `HOME` for `agy` is equivalent to `CODEX_HOME` / `GROK_HOME` **on the state-redirection dimension only**. It is _not_ equivalent on the auth dimension: Grok has `GROK_AUTH_PATH` to point the credential back out of the scope explicitly, and Codex has `boss_codex_auth` snapshotting. `agy` has neither, so the credential must be delegated by filesystem link into the scoped `HOME`, which is a different mechanism with a different failure mode (a link into a per-run tree that gets reclaimed, versus an env var that is simply wrong). Grok's `environment.rs` already documents why the filesystem form is load-bearing there too — its tool sandbox strips parent-process env vars — so the precedent transfers, but as _filesystem delegation_, not as _env-var delegation_.

### Topology

```
GhosttyKit pane (what the operator reads: agy's own TUI)
  └─ shell → agy -i "$(cat <control-dir>/initial-prompt.txt)" --add-dir <control-dir> --model … --effort …
       env: HOME=<per-run home>   (+ host-tool delegation, per Grok's environment.rs)

<per-run home>/.gemini/
  ├─ settings.json                        ← Boss-written: auth type
  ├─ oauth_creds.json                     ← link to the shared host credential
  ├─ google_accounts.json                 ← link to the shared host file
  └─ antigravity-cli/
       ├─ settings.json                   ← Boss-written: toolPermission, permissions.allow,
       │                                     trustedWorkspaces  → per-worker-kind posture
       ├─ conversation_summaries.db       ← run-private
       ├─ presence/, conversations/, brain/, log/, cache/
       └─ brain/<cid>/.system_generated/logs/transcript_full.jsonl   ← TranscriptAccess (post-hoc)

<control-dir>/                            ← Boss-owned, OUTSIDE the repo, passed via --add-dir
  ├─ .agents/hooks.json                   ← boss-event forwarder + PreToolUse guards behind the adapter
  ├─ .agents/rules/boss-worker-rules.md   ← agent-rules, without touching the repo's tracked AGENTS.md
  └─ initial-prompt.txt
```

Two things this buys that are worth stating explicitly:

- **Hook config never enters the repo.** The brief's finding that `--add-dir <workspace>` flips hook discovery on is explained by `.agents/` being resolved at the root of _any_ workspace directory. Putting Boss's control directory outside the repo and adding it means the worker's checkout stays clean — no `.agents/` to gitignore, no engine-written files in `jj status`. If the spike finds `.agents/` is only honoured in the primary cwd, the fallback is an in-repo `.agents/` with the trait's existing `config_dir_gitignore` (`"*\n"`) — a real fallback, but strictly worse, and the design prefers the control directory.
- **Boss worker rules do not collide with the repo's tracked `AGENTS.md`.** `agy` reads `AGENTS.md`, `GEMINI.md`, and `.agents/rules/*.md`; mono has a tracked `AGENTS.md` at the root. This is exactly the collision Grok solved by writing Boss rules to `$GROK_HOME/AGENTS.md` at global scope, and the control directory's `rules/` is the `agy` equivalent.

### Ingress and turn boundary

`ProgressIngress::HookCallback` with `HookWiringDestination::DriverOwned`, wired into the control directory's `.agents/hooks.json`. The `boss-event` shim is schema-agnostic transport — it splices `_boss_run_id` and forwards bytes — so it is reused unchanged, exactly as Claude and Grok reuse it, and the `agy` dialect is decoded engine-side by the driver's `normalize_progress_event` / `progress_session`.

The events wired: `PreToolUse`, `PostToolUse`, `PreInvocation`, `PostInvocation`, `Stop`. Two gaps against Claude's set need explicit mapping decisions from characterisation rather than assumption:

- **No `SessionStart`.** The first `PreInvocation` is the candidate carrier for `WorkerEvent::SessionStart` and for claiming progress identity, since every payload carries `conversationId`. Unverified.
- **No `Notification`.** So `Capability::AwaitingInputSignal` is **not declared**, and its absence disposition stays the default `Degrade` — never `Synthesize`. An `agy` worker shows Working/Idle and never a fabricated `WaitingForInput`. This matches Grok's measured position.

Turn boundary is `Stop` → `TurnEnd`. The `terminationReason` vocabulary must be enumerated empirically — the brief records that the docs and the binary disagree, and the binary wins.

### Subagent attribution: mandatory identity filtering, because there is no flag

Grok needed `--no-subagents` because a subagent's `session_end` was byte-identical in shape to the parent's, differing only in `sessionId`, and Boss routes hook events by `_boss_run_id` and applies session-end by slot — so a finishing subagent flipped a live worker to `Terminated`. `agy` has no equivalent flag, so the mitigation Grok's investigation named as "what would have to change" is not optional here: **filter at ingress on `conversationId`**.

The seam already exists: `ProgressIdentityStore::claim_progress_identity(run_id, session_id)`. The run claims the parent conversation id at its first hook event, and the progress session drops boundary-shaped events carrying any other `conversationId`. If characterisation finds that `agy` subagents _share_ the parent's `conversationId`, this mitigation does not work and the subagent hazard becomes a stop-shaped finding for the design — which is why it is characterised before the crate is built.

### Permission posture

`write_permission_config` renders, into the per-run `HOME`:

- `antigravity-cli/settings.json` — `toolPermission` level plus a `permissions.allow` allowlist, per `WorkerKind`, mirroring `grok/permissions.rs::permission_mode_for_worker_kind`'s shape (a driver-local mirror of `worker_setup::WorkerKind::forced_permission_mode`, not a second source of truth).
- `trustedWorkspaces` pre-seeded with the workspace and the control directory, so the first-run trust prompt never blocks a pane.
- `PermissionArtifacts::extra_args` for whatever the characterisation task finds is genuinely per-run: `--sandbox`, and `--mode plan` if it proves to be a real read-only posture for Reviewer.

Which of `toolPermission: strict` + allowlist versus `--mode plan` versus `--sandbox` expresses Reviewer read-only is **an open choice**, and the characterisation task that answers it is a _choosing_ study, not a validating one — see the task entry, which says so explicitly.

### Hook adapter: fail closed on anything unrecognised

The adapter sits in front of Boss's unchanged guard scripts, translating `agy`'s camelCase payload into the canonical shape on the way in and Boss's `block`/`approve` into `agy`'s `deny`/`allow` on the way out — the same two-way shape as `GROK_HOOK_ADAPTER_SCRIPT`.

One requirement is stated more strongly than Grok's: **an unrecognised verdict, an unparseable payload, or an adapter error must deny.** Grok's defect was a verdict that failed open and "looked identical to a healthy configuration"; `agy` adds `ask` and `force_ask` to the vocabulary, which are neither allow nor deny and must not be guessed at. The adapter's tests must include an unknown-verdict case asserting denial.

### Soft-deny detection (Risk 3b)

A worker that soft-denies its tools and exits 0 produces a clean turn boundary on a run that accomplished nothing. Boss already has a pattern for exactly this class of lie, built for Codex: `UNOBSERVED_COMMAND_MARKER` is emitted on the progress stream ahead of the `Stop` it precedes, staged by the event dispatcher, and consumed by `on_stop_inner` to file an attention item **and** to refuse the worker's `NO_CHANGES_NEEDED` claim for the rest of the run (`codex_unobserved_command.rs`).

`agy`'s soft-deny detection reuses that shape rather than inventing one: a marker-carrying `Notification` from the progress session, staged and consumed by the same mechanism. Whether the interactive path soft-denies the way headless does is itself a characterisation question.

### The acceptance gate: a state, not a checkpoint

The Grok design's "10 consecutive green PRs" Phase-1 gate was never executed, and its own retrospective names why: _a gate at the end of a phase can only hold back the next phase, and "Phase 1 is not yet accepted" was never a state that stopped anything._ Repeating that structure here would be a known-defective design.

So the gate is inverted. Rather than a checkpoint at the end, it is the **default state, enforced by existing engine code**:

- **What enforces it.** `DriverTrafficSplit`'s shipped default gives `agy` **zero**, and `allocate_among` treats zero as literally empty — "zero means zero", with `driver_for_bucket` handing it no bucket at all. No allocated work reaches `agy` until an operator makes a deliberate, atomic, single-write change to the persisted split. The four-way migration lands with a unit assertion that the shipped default is `agy = 0`, so a later edit that quietly raises it fails a test rather than silently changing fleet behaviour.
- **The state while unmet: `pinned-only`.** `agy` is registered, resolvable, and reachable _only_ by an explicit `tasks.driver` / product `default_driver` pin that clears the capability gate. This is a steady state, not a waiting state: nothing is blocked, nothing silently proceeds, and the driver is exercised on real work the whole time.
- **What would move it.** A recorded run of N pinned `agy` executions reaching merged PRs without engine-side incident, plus a spend measurement showing a nonzero share is affordable. Both are written into a separate acceptance-ledger document, not into this doc, so the durable reasoning here is not disturbed every time the status changes.
- **Who can move it.** Raising the share is an operator action against engine state. No task in the breakdown below has "raise the share" as its deliverable, because no cube worker can or should perform it.

One more structural rule, taken from the same retrospective: **the acceptance evidence must come from real Boss dispatch, not from a harness.** A hand-built reproduction is assembled from the same beliefs that produced the code, so it is structurally incapable of finding the integration bugs — the events-socket routing, the credential strip, the adapter identity mismatch — that are precisely what killed Grok's sweep. Every phase-0 investigation likewise runs in a real GhosttyKit pane, not a simulated one.

### Capability declaration (proposed, subject to characterisation)

Declared: `Spawn`, `WorkspaceProvisioning`, `PermissionPolicy`, `ModelAndEffortMenu`, `ProgressObservation`, `ToolUseInterception`, `TurnBoundary`, `StructuredOutput`, `TranscriptAccess`, `ControlVerbs`, `PromptComposition`.

Omitted, each with its reason:

- **`ToolProvisioning`** — Degrade. Boss injects no MCP servers or tool definitions for any driver. Declaring it would overclaim.
- **`AwaitingInputSignal`** — Degrade, never Synthesize. `agy`'s hook set has no `Notification`-shaped event, so there is nothing honest to bind to.
- **`CommandOutcomeObservation`** — Degrade, never Synthesize. Whether `PostToolUse` carries a reliable per-command exit status is uncharacterised, and Codex's history is precisely that a plausible-looking `exit_code` field was not reliable. Absent evidence, this stays undeclared, which also keeps `agy` out of `ConflictResolution` / `CiRemediation` by way of the gate rather than by a hardcoded list.

`ToolUseInterception` may need to be declared **deny-only** (as Grok's is) if `agy`'s `PreToolUse` cannot rewrite tool input. Characterisation decides.

## Risks / open questions

**R1 — Hooks may not fire in interactive mode.** The whole boundary story is headless evidence. If interactive hooks do not fire, the pane constraint holds and the answer is transcript tailing or pane scraping, not a stream in the pane. This is the first question the spike answers, and a negative answer is a stop-shaped finding for the project, not a detour.

**R2 — Scoped `HOME` may behave differently in a pane than in a one-shot command.** Verified for `agy models`; unverified for a long-lived TUI that spawns tool child processes and possibly a browser subagent. The playwright cache relocating under the scoped `HOME` also implies a per-run re-download cost unless it is delegated the way Grok delegates `XDG_CACHE_HOME` and the bazel roots.

**R3 — Credential delegation under concurrency.** Sixteen runs sharing one `oauth_creds.json` will contend on token refresh. Grok's answer was to wait on the CLI's own lock protocol without ever taking it. `agy`'s refresh and locking behaviour is unknown and must be characterised before the fleet is pointed at it.

**R4 — Subagents may share the parent `conversationId`.** If so, ingress identity filtering cannot separate them and there is no `--no-subagents` to fall back on. This is the highest-severity uncharacterised risk after R1.

**R5 — `agy` auto-updates.** Grok's design records that it "auto-updates itself on its own schedule", which is why its version pin warns rather than gates. `agy` has `--update` and an `updater/` state directory, so the same drift applies, and a scoped `HOME` may cause each run to re-check for updates. `AGY_CLI_DISABLE_AUTO_UPDATE` exists in the binary and is the obvious lever to probe.

**R6 — Economics may simply refuse this driver.** Addressed above; the spend model is sequenced early precisely so this can end the project cheaply.

**R7 — Terms of service are silent on programmatic driving, and silence is not permission.** No ToS text was located either permitting or prohibiting driving `agy` programmatically. The weak reading is "not prohibited, therefore allowed". The stronger and more likely reading is that **no decision was ever made** about this use — an unaddressed gap, not a granted allowance — and a personal-account OAuth quota driven by a 16-worker fleet is exactly the case a provider would address if it addressed any. This needs an operator call before the driver runs on real work, and it is in the questions manifest.

**R8 — CI cannot authenticate.** `agy` requires a prior interactive login and caches the credential under `HOME`. Any live-CLI conformance gate (`BOSS_REQUIRE_AGY_CLI`) will therefore only be meaningful on a developer machine with a signed-in account, exactly as `BOSS_REQUIRE_GROK_CLI` is. Fixture-side pins carry the contract in CI.

**R9 — `ANTIGRAVITY_EXECUTABLE_DATA_DIR` may be an actual data-dir override.** If it is, part of the scoped-`HOME` machinery could be replaced by something narrower and better-behaved. Cheap to probe; worth probing before building the provisioning module.

## Proposed implementation task breakdown

Breakdown size: 22 entries (20 in-scope, 2 deferred) — a new fourth driver reaching across five investigations (pane viability, spend, isolation, hooks, permissions), a breaking protocol+DB+Swift split migration, a new driver module with ten distinct trait seams (skeleton, home provisioning, retention, spawn, hook wiring, guard adapter, progress normaliser, turn boundary, permission rendering, transcript), a soft-deny detector, and a conformance extension; each is its own reviewable PR, and the count sits below the 30 tasks the comparable Grok integration actually shipped.

Parallelism is noted per entry. Depth-0 entries (spike, spend model, traffic split) may all run concurrently.

### Ghostty pane viability spike for `agy`

Run `agy` 1.1.12 as an interactive TUI inside a **real GhosttyKit-hosted pane** — not a standalone terminal, not a harness — and answer the nine-item checklist: does `-i "<prompt>"` auto-submit in a PTY; do hooks fire at all in interactive mode; is `--add-dir` required for `.agents/hooks.json` discovery and does a control directory _outside_ the repo work; does workspace trust independently gate discovery; does Esc emit `Stop` or skip it; is mid-turn pane input buffered or rejected; what `terminationReason` values does the binary actually emit; does scoped `HOME` hold up in a pane; and what are the banner / busy / prompt / starting strings for a `PaneMonitorSpec`. Model the artefact on `ghostty-grok-pane-viability.md`, including its hard apparatus rule. **This study is a gate, not a validation**: it can return "not viable", and that outcome stops the project rather than prompting a workaround. Deliverable: `tools/boss/docs/investigations/ghostty-agy-pane-viability.md` plus an artifacts directory. No driver code.

- **Effort:** large
- **Dependencies:** none
- Scope: in-scope

### Antigravity spend and quota model

Instrument `agy` headlessly (explicitly permitted as a research instrument) over a representative Boss-shaped brief — a real task prompt of realistic size, run to completion — and measure what the economics section can only estimate: requests per run, per-request overhead, cache-hit behaviour across a multi-turn session, and total input tokens for one complete chore-sized run. Convert that into a fleet model against Antigravity's published weekly quota terms, at 1, 4, and 16 concurrent workers, and state a maximum affordable traffic share. **This study is choosing, not validating**: it decides whether the driver is worth building at all, and a finding that a single pinned chore routinely exhausts a week of quota is a valid terminal outcome. Must not read Boss's engine DB. Deliverable: `tools/boss/docs/investigations/agy-spend-and-quota-model.md`.

- **Effort:** medium
- **Dependencies:** none — runs in parallel with the pane viability spike
- Scope: in-scope

### Four-way driver traffic split migration

Widen `DriverTrafficSplit` from three shares to four across every layer that must move together: `protocol/src/types/driver_split.rs` (the struct, `DRIVERS_IN_BUCKET_ORDER`, `validate`, `driver_for_bucket`, the error type's message), the DB migration in `work/migrations_b.rs` following the `migrate_driver_traffic_split_from_codex_percentage` precedent, `work/driver_allocation.rs`'s renormalisation, and the Swift mirror in `app-macos/Sources/DriverTrafficSplit.swift` plus its `DriverSlug` enum, `SettingsView` stepper, and tests. Rust and Swift land in **one PR deliberately**: the split crosses the wire as a single value, so a half-landed migration breaks decoding rather than degrading. Ships `agy = 0` in `Default` — the behaviour-preserving state — with a unit assertion pinning that default, which is the enforcement half of the acceptance gate.

- **Effort:** large
- **Dependencies:** none — runs in parallel with both phase-0 investigations
- Scope: in-scope

### Acceptance ledger and pinned-only gate state

Create the acceptance ledger as a separate document from this design so status can be refreshed without touching durable reasoning: the criterion (N pinned `agy` executions reaching merged PRs without engine-side incident, plus an affordability finding from the spend model), an empty evidence table, the explicit statement that the project sits in `pinned-only` state until the criterion is met, and the explicit statement that raising the share is an operator action no task performs. Document how `pinned-only` is enforced — zero share plus "zero means zero" in `allocate_among`, with an explicit pin still routing — and cross-link it from this design.

- **Effort:** small
- **Dependencies:** Four-way driver traffic split migration
- Scope: in-scope

### Scoped-`HOME` isolation and credential delegation characterisation

Characterise the load-bearing mechanism under realistic concurrency: sixteen concurrent scoped `HOME`s each running a real `agy` session; what breaks when `oauth_creds.json` and `google_accounts.json` are delegated by filesystem link to one shared host file; how `agy` refreshes and locks that credential and whether concurrent refreshes corrupt or serialise; whether host-tool delegation (`gh`, `cube`, `jj`, git, ssh, keychain, bazel/XDG caches — the full `grok/environment.rs` list) is needed and whether `agy`'s tool sandbox strips env vars the way Grok's does; the per-run cost of the relocated `ms-playwright-go` cache; whether `AGY_CLI_DISABLE_AUTO_UPDATE` suppresses per-run update checks; and whether `ANTIGRAVITY_EXECUTABLE_DATA_DIR` is a narrower override that would replace part of this machinery. **This study is validating the chosen approach, not choosing between options** — except for the `ANTIGRAVITY_EXECUTABLE_DATA_DIR` probe, which is genuinely a choice and must be reported as one. Deliverable: an investigation doc.

- **Effort:** large
- **Dependencies:** Ghostty pane viability spike for `agy`
- Scope: in-scope

### Hook surface characterisation: event vocabulary, subagent attribution, interrupt

Enumerate what the hook stream actually carries in interactive mode: the full `terminationReason` vocabulary from the binary (not the docs); `PreInvocation` / `PostInvocation` semantics and whether the first `PreInvocation` can carry `WorkerEvent::SessionStart` and the identity claim; whether `PostToolUse` carries a reliable per-command exit status (which decides `CommandOutcomeObservation`); whether `PreToolUse` can rewrite tool input or only deny (which decides whether `ToolUseInterception` is deny-only); the exact `allow`/`deny`/`ask`/`force_ask` semantics including what an unrecognised verdict does; and — highest severity — **whether a subagent's `Stop` carries a `conversationId` distinct from its parent's**, since there is no `--no-subagents` and ingress identity filtering is the only available mitigation. Deliverable: an investigation doc.

- **Effort:** large
- **Dependencies:** Ghostty pane viability spike for `agy`. Runs in parallel with the scoped-`HOME` characterisation (separate documents, no shared files).
- Scope: in-scope

### Permission, trust, and soft-deny characterisation

Determine how Boss's per-worker-kind posture is actually expressed: the real key names and accepted values for `toolPermission` and `permissions.allow` in `antigravity-cli/settings.json`; whether `permissions.allow`'s `command(git)` / `write_file(src/)` syntax is validated at parse time or silently ignored when malformed; whether `trustedWorkspaces` or `trustedFolders.json` (or both) gate trust, and whether pre-seeding suppresses the first-run prompt in a pane; and whether Reviewer read-only is best expressed as `toolPermission: strict` plus an allowlist, as `--mode plan`, or as `--sandbox`. **This study is choosing between those three options, and must return a recommendation with evidence** — not a note that the pre-selected one works. Separately, establish whether the interactive path soft-denies an ungranted tool while still reporting a clean turn boundary, as headless does. Deliverable: an investigation doc.

- **Effort:** medium
- **Dependencies:** Scoped-`HOME` isolation and credential delegation characterisation
- Scope: in-scope

### `AgyDriver` skeleton: descriptor, capabilities, model menu, registry

The seed changeset, modelled on the Grok skeleton PR: `DriverDescriptor` (slug `agy`, binary `agy`, config dir, agent-rules filename, initial-prompt filename), the `ModelMenu`'s seven function pointers over the `agy models` catalogue with the three-rung effort ladder, the `CapabilitySet` with every omission carrying its recorded reason, registration in `DriverRegistry::default()`, and the minimum trait implementations needed to compile and keep the conformance suite green — including a native-dialect transcript fixture in `conformance/native_transcript.rs` and a transcript normaliser sufficient to surface a `[blocked]` marker from it, since that suite fails closed on any registered slug without one. No spawn line, no provisioning, no hooks. Capability declarations must be justified from the phase-0 characterisation, not asserted.

- **Effort:** medium
- **Dependencies:** Hook surface characterisation; Permission, trust, and soft-deny characterisation; Antigravity spend and quota model
- Scope: in-scope

### Per-run `HOME` provisioning and credential delegation

The `agy/home.rs` + `agy/environment.rs` pair: allocate a per-run container, materialise `.gemini/settings.json` and the scoped `antigravity-cli/` tree, delegate the OAuth credential and account file to the shared host paths with whatever refresh-lock discipline characterisation established, delegate host-tool state (`gh`, `cube`, `jj`, git, ssh, caches) per the measured need, and assert the posture before spawn so a misconfigured home fails loudly rather than running against the operator's `~/.gemini`. The posture assertion checks **the invariant, not the container**: no mutable state artifact shared between concurrent runs, and the operator's `~/.gemini` never reachable.

- **Effort:** large
- **Dependencies:** `AgyDriver` skeleton; Scoped-`HOME` isolation and credential delegation characterisation
- Scope: in-scope

### Per-run `HOME` retention sweep

Reclaim per-run `agy` homes on the same pattern as `grok_home_retention_sweep.rs` and the `grok-home-retention` crate: bounded retention, a safe-to-delete assertion that refuses to touch anything outside Boss's own homes root, and scheduling from engine core. Each home carries a SQLite conversation DB and a `brain/` tree, so unreclaimed homes are a disk-exhaustion risk at fleet scale, not housekeeping.

- **Effort:** medium
- **Dependencies:** Per-run `HOME` provisioning and credential delegation
- Scope: in-scope

### Pane spawn invocation and `PaneMonitorSpec`

Compose the interactive pane command — `agy -i "$(cat …/initial-prompt.txt)"` with `--add-dir <control-dir>`, `--model`, `--effort`, and the scoped-`HOME` env directives — and supply the `PaneMonitorSpec` agent / busy / starting / prompt markers captured from the real TUI in the spike, so the app's fallback status pill works before the first hook event arrives. Creates the Boss-owned control directory outside the repo and writes the initial prompt and agent-rules into it.

- **Effort:** medium
- **Dependencies:** Per-run `HOME` provisioning and credential delegation; Ghostty pane viability spike for `agy`
- Scope: in-scope

### Control directory hook wiring: `boss-event` progress forwarder

Write `.agents/hooks.json` into the control directory wiring `PreToolUse`, `PostToolUse`, `PreInvocation`, `PostInvocation`, and `Stop` to the unchanged `boss-event` shim with the same `BOSS_EVENTS_SOCKET` / `BOSS_LEASE_ID` / `BOSS_RUN_ID` / `BOSS_WORKSPACE` env prefix Claude and Grok use, and declare `ProgressIngress::HookCallback` with `HookWiringDestination::DriverOwned`. Raw payloads forward unchanged; decoding is the engine-side normaliser's job. Includes verifying discovery actually works from the out-of-repo control directory, with the in-repo `.agents/` fallback if it does not.

- **Effort:** medium
- **Dependencies:** Pane spawn invocation and `PaneMonitorSpec`; Hook surface characterisation
- Scope: in-scope

### Hook payload adapter and `PreToolUse` guards

Add the canonicalisation adapter in front of Boss's five unchanged guard scripts: `agy` camelCase payload to canonical shape inbound, Boss `block`/`approve` to `agy` `deny`/`allow` outbound. **Fails closed by construction**: an unrecognised verdict, an unparseable payload, an adapter crash, or the `ask` / `force_ask` values all deny, with a test asserting each. This is the entry that makes `Capability::ToolUseInterception` real, and it is deliberately separated from the forwarder above so the security-critical translation is reviewed on its own rather than mixed with transport plumbing.

- **Effort:** medium
- **Dependencies:** Control directory hook wiring: `boss-event` progress forwarder
- Scope: in-scope

### Progress normaliser and conversation-identity filtering

Implement `normalize_progress_event` / `progress_session` for the `agy` hook dialect into `WorkerEvent`, including the `PreInvocation`-to-`SessionStart` mapping characterisation settles. Claims the parent `conversationId` through `ProgressIdentityStore::claim_progress_identity` at the first event and **drops boundary-shaped events carrying any other `conversationId`**, which is the only available mitigation for `agy`'s undisableable subagents. If characterisation found subagents share the parent id, this entry instead records that finding as a blocking defect rather than shipping a filter that cannot work.

- **Effort:** large
- **Dependencies:** Control directory hook wiring: `boss-event` progress forwarder; Hook surface characterisation. Parallel with the guard adapter (separate modules).
- Scope: in-scope

### Turn boundary and interrupt recovery

Map `Stop` to `TurnEnd` over the empirically enumerated `terminationReason` vocabulary and the `fullyIdle` flag, deciding continuation semantics explicitly rather than inheriting Claude's. If the spike found that Esc-cancelled turns skip `Stop` — the Grok trap — implement `prepare_interrupt_recovery` / `is_interrupt_recovery_turn_end` against a run-private artifact the way `grok/turn_end_recovery.rs` does; if Esc does emit `Stop`, record that measurement and leave both trait methods at their defaults.

- **Effort:** medium
- **Dependencies:** Progress normaliser and conversation-identity filtering
- Scope: in-scope

### Permission config rendering per worker kind

Implement `write_permission_config` to render, into the per-run `HOME`, the `toolPermission` level and `permissions.allow` allowlist for each `WorkerKind`, the structural deny set (bossctl / state-dir / rm / sudo), and pre-seeded `trustedWorkspaces` covering the workspace and control directory — returning any genuinely per-run flags as `PermissionArtifacts::extra_args`. Mirrors `WorkerKind::forced_permission_mode` in a driver-local function as `grok/permissions.rs` does, so the restricted kinds' posture cannot be silently downgraded, and uses whichever Reviewer read-only expression the permission characterisation recommended.

- **Effort:** large
- **Dependencies:** Per-run `HOME` provisioning and credential delegation; Permission, trust, and soft-deny characterisation. **Co-edits `agy.rs`'s trait impl with the pane-spawn entry — land after it and forward-port its changes preservingly.**
- Scope: in-scope

### Soft-deny and no-op-worker detection

Detect the Risk 3b failure — an ungranted tool soft-denied while the turn boundary still reports clean — by reusing Boss's existing pattern rather than inventing one: emit a marker-carrying `Notification` from the progress session ahead of the `Stop` it precedes, stage it in the event dispatcher, and consume it in `on_stop_inner` to file an attention item and refuse the worker's `NO_CHANGES_NEEDED` claim for the rest of the run, exactly as `codex_unobserved_command.rs` does. Includes the bounded audit trail and overflow attention that pattern already defines.

- **Effort:** medium
- **Dependencies:** Progress normaliser and conversation-identity filtering; Permission config rendering per worker kind
- Scope: in-scope

### Control verbs and error classification

Declare `probe` / `interrupt` / `stop` / `reap` / `mid_turn_pane_input` from what the spike measured, not from what seems likely — `MidTurnPaneInput` stays at the `Rejects` default unless mid-turn stdin consumption was proven, per the standard Grok set. Implement `classify_error` for the `agy`/Google error surface, with **quota exhaustion as a first-class class**: given the economics, a quota-exhausted worker misclassified as a generic failure would drive redispatch straight back into the same wall.

- **Effort:** medium
- **Dependencies:** Pane spawn invocation and `PaneMonitorSpec`; Ghostty pane viability spike for `agy`. **Co-edits `agy.rs`'s trait impl — land after the permission-config entry and forward-port.**
- Scope: in-scope

### `TranscriptAccess`: durable store, path resolution, and session normaliser

Resolve `transcriptPath` from the `Stop` payload via `transcript_path_for_session`, provision the durable per-execution transcript store so the transcript outlives the recycled workspace and the reclaimed per-run `HOME`, implement `TranscriptSessionNormalizer` for `agy`'s native `transcript_full.jsonl` dialect, and replace the skeleton's minimal fixture with one captured from a real run. Includes `extract_error_from_transcript` and `structured_output_fallback` candidates for the `agy` dialect — the driver-neutral `BOSS_STRUCTURED_OUTPUT` / `BOSS_PR_URL_OUTPUT` env-file contract already applies unconditionally, so these are the fallback path only.

- **Effort:** large
- **Dependencies:** Progress normaliser and conversation-identity filtering
- Scope: in-scope

### Conformance suite extension for `agy`

Extend the conformance suite to cover the fourth driver on the same terms as the other three: `agy_goldens.rs` with captured real payloads, the boundary-equivalence assertions (`only_stop_shaped_events_are_turn_boundaries_on_any_driver` and its siblings), ingress equivalence, and a version pin with a `BOSS_REQUIRE_AGY_CLI` live-CLI gate that soft-skips when `agy` is absent — noting in the pin's docs that CI cannot authenticate `agy`, so the live check is meaningful only on a signed-in developer machine and the fixture-side pins carry the contract everywhere else.

- **Effort:** medium
- **Dependencies:** Turn boundary and interrupt recovery; `TranscriptAccess`: durable store, path resolution, and session normaliser
- Scope: in-scope

### `ConflictResolution` / `CiRemediation` eligibility for `agy`

Declare `Capability::CommandOutcomeObservation` for `agy` once a reliable per-command exit status is established on its `PostToolUse` payload, which would let the dispatch gate stop refusing those two execution kinds.

- **Effort:** medium
- **Dependencies:** Hook surface characterisation
- Scope: deferred (future / not a v1 blocker) — no driver but `claude` declares this capability today, and Codex's history is that a plausible-looking exit-status field turned out unreliable; declaring it without evidence would be exactly the overclaim the capability exists to prevent.

### Remote / SSH host support for `agy` workers

Extend `agy` provisioning to the remote-host spawn path — the `is_remote` branch that skips the engine-data-dir sandbox and forwards the events socket — so distributed execution can dispatch `agy`.

- **Effort:** large
- **Dependencies:** Per-run `HOME` provisioning and credential delegation; Permission config rendering per worker kind
- Scope: deferred (future / not a v1 blocker) — v1 is local-macOS only, and the credential-delegation story does not survive a remote host without its own design (there is no second signed-in account to delegate to).
