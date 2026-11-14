# Antigravity (`agy`) as Boss's fourth driver: everything rides on a Boss-owned per-run `HOME`

- **Date:** 2026-08-12
- **Status:** design, pre-implementation. No `AgyDriver` code exists. Revised 2026-11-14 against the permission and shared-HOME studies. Per-run `HOME` remains the baseline by repo owner decision on 2026-11-14; shared HOME is a credible alternative requiring further qualification.
- **Kind:** project design doc
- **Subject CLI:** `agy` 1.5.9, Google Antigravity, OAuth against a personal Google account. The conformance pin must be **1.5.7 or later**: `auto_approve` and `deny_unless_prior_grant` were measured on 1.5.7 and 1.5.9, not 1.5.5 (permission study, "Recommendation"). Earlier pane/hook observations retain their measured versions below; they do not qualify a newer release automatically. Launch a qualified Boss-owned executable, verify its identity at spawn, and export `AGY_CLI_DISABLE_AUTO_UPDATE=true`; the owner's updater can replace a shared installed executable even when workers disable updates (shared-HOME study, §6).
- **Evidence:** [pane viability study](../investigations/ghostty-agy-pane-viability.md), [hook surface study](../investigations/agy-hook-surface.md), [permission study](../investigations/agy-permission-trust-soft-deny.md), [shared-HOME study](../investigations/agy-shared-home-hook-permissions.md), [corrected credential study](../investigations/scoped-home-credential-delegation.md). Section citations below distinguish these sources; unqualified Q-number citations refer to the pane study.
- **Sibling designs:** [`grok-as-a-first-class-interactive-agent-driver.md`](./grok-as-a-first-class-interactive-agent-driver.md), [`codex-as-a-first-class-agent-driver.md`](./codex-as-a-first-class-agent-driver.md), [`agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md`](./agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md)
- **Perishable status/ledger:** [`antigravity-driver-acceptance-ledger.md`](./antigravity-driver-acceptance-ledger.md) — kept out of this doc so status can be refreshed without touching durable reasoning.

## The contested property, stated first

**The selected state-isolation mechanism is a Boss-owned per-run `HOME`.**

`HOME` relocates hooks, conversations, transcripts, and mutable CLI state; the pane study measured this in a long-lived TUI and its tool children with the login-keychain bridge (Q8). Permission posture and workspace trust can instead be selected independently per session through `--settings` and hooks (permission study, §§1, 7, 10; shared-HOME study, §4). Private state therefore remains a deliberate isolation choice, not a prerequisite for distinct worker policies. Alternative C records the credible shared-HOME option and its remaining acceptance work; it is not an automatic fallback if provisioning fails.

## TL;DR / verdict

Technically the integration looks tractable: `agy` has a Claude-shaped hook system with a real `Stop` hook, an interactive TUI suitable for a pane, and — verified in this pass and by the pane viability study (Q8) — a state tree that relocates cleanly under a scoped `HOME`.

The initial operating state is a **pinned-only, zero-share driver** — reachable by explicit `--driver agy`, receiving no allocated traffic — as a behaviour-preserving rollout default, not an economic verdict. Live gate status lives in the [acceptance ledger](./antigravity-driver-acceptance-ledger.md). The operator has a paid Gemini subscription, but this pass did not establish its entitlement or an apples-to-apples quota comparison with `claude`, `codex`, or `grok`. Spend and quota accounting belongs in a separate, driver-neutral project; it is not an implementation gate for this driver.

## Goals

1. Add `agy` as Boss's fourth registered driver, running as a first-class **interactive TUI worker in a GhosttyKit pane**, on the same terms as `claude`, `codex`, and `grok`.
2. Resolve the three coupled risks — ingress behind a TUI, per-run isolation, per-worker-kind permission posture — _before_ the driver crate is built, because an isolation strategy that fails later invalidates everything stacked on it.
3. Migrate the driver traffic split from three shares to four, across the Rust protocol, the DB, and the Swift app, without a broken intermediate state.
4. Define a phase gate that is a **state enforced by code**, not a checkpoint that only blocks what follows it.

## Non-goals

- **A JSONL / stream-json dump in the worker pane.** Ruled out by the operator: the pane is a human-readable TUI, full stop. `agy -p --output-format stream-json` is a research and test instrument only. This is a constraint on the design, not an option weighed and rejected, and it is not revisited below.
- **Revisiting the registry's `ProgressIngress::StdoutJsonl` ban** (`engine/driver/src/registry.rs:77`) or its `WorkerProcessLifetime::OneTurnPerProcess` sibling (`:112`). Both are settled and consistent with the pane constraint.
- **Making `agy` eligible for `ConflictResolution` / `CiRemediation`.** Those execution kinds mark `Capability::CommandOutcomeObservation` required-strict; no driver but `claude` declares it. `agy` will not either in v1. The hook surface study measured `PostToolUse` as carrying no structured exit status, so hooks cannot unblock this (hook study, "PostToolUse does not establish command outcome").
- **PR review on `agy`.** Review dispatches on the review pool's fixed driver by deliberate policy (`work/driver_allocation.rs`, `decide_execution_driver` step 1), so it does not participate in the split at all. Changing that is a change to reviewer dispatch policy, not to this driver.
- **Remote / SSH host support** for `agy` workers. v1 is local-macOS only.
- **Raising `agy`'s traffic share.** This design ships the fourth share at zero and defines what would justify raising it. It does not raise it.
- **Driver-specific spend, quota, or cost accounting.** The driver-abstraction design already records token/quota and cost accounting as a known cross-driver absence. Building an `agy`-only model would create the wrong boundary; a later mechanism should compare all drivers under their actual subscription entitlements.

## Method, and what was actually verified in this pass

The project brief arrived with substantial pre-work. Per its own instruction, load-bearing claims were re-checked against the tree and the installed binary rather than taken on trust. What follows separates **verified in this pass**, **corrected**, and **still unconfirmed**.

### Verified against the binary

- `agy --help` flag surface, on 1.1.12 (the pass predating the study; the effort ladder is wider on 1.5.5, see the table below): `-i` / `--prompt-interactive`, `--add-dir` (repeatable), `--effort low|medium|high`, `--model`, `--conversation`, `-c`/`--continue`, `--sandbox`, `--mode accept-edits|plan`, `--dangerously-skip-permissions`, `--json-schema`, `--print-timeout`, `--disable-slash-commands`, `--project` / `--new-project`. Subcommands: `agent(s)`, `models`, `plugin(s)`, `install`, `update`, `changelog`, `help`. There is **no** `--no-subagents` and **no** config-dir flag.
- **No `AGY_HOME` equivalent exists.** A symbol sweep of the binary surfaces `AGY_ADC_AUTH`, `AGY_BUSINESS_PAYGO_TIER`, `AGY_CLI_DISABLE_AUTO_UPDATE`, `AGY_CLI_DISABLE_LATEX`, four `AGY_ONBOARDING_*` flags, and a set of `ANTIGRAVITY_*` names that are editor/sidecar/telemetry oriented. One name is worth a probe rather than a conclusion: `ANTIGRAVITY_EXECUTABLE_DATA_DIR`. Its semantics are unknown from strings alone.
- **`HOME` relocation works, and is total.** Running `env HOME=<scratch> agy models` created `<scratch>/.gemini/config/{config.json,mcp_config.json,projects/}` and `<scratch>/.gemini/antigravity-cli/{conversation_summaries.db,brain/,conversations/,presence/,log/,cache/,knowledge/,updater/,installation_id,jetski_state.pbtxt}` — i.e. the entire mutable state surface named in Risk 2 followed `HOME`. It also created `<scratch>/Library/Caches/ms-playwright-go`, so the browser-subagent cache follows too.
- **Scoped `HOME` fails closed on auth**, exiting non-zero with `Error: Please sign in to view available models.` The credential is **not** a `HOME`-scoped file: it is an item in the macOS login keychain, read through `/usr/bin/security`, which cannot find the user's keychain under a scoped `HOME`. That is why a scoped `HOME` is signed out, and the remedy is the login-keychain bridge (a symlink to the real login keychain inside the scoped `HOME`), not credential-file links (study, Q8 "The credential is in the login keychain"). Failing closed is a feature for safety and a hard requirement for design.
- **Permission and trust state are a `HOME`-scoped file.** `~/.gemini/antigravity-cli/settings.json` holds `trustedWorkspaces` (observed on the host with real entries), `colorScheme`, and `enableTelemetry`. `~/.gemini/settings.json` and `trustedFolders.json` are **not read** by `agy`; neither binary references them (study, "Corrections to the design" and Q8). The earlier belief that they held the auth type and a trust store was wrong.
- **`.agents/` is discovered at the root of a workspace directory, not only the repo.** The binary documents `.agents/` (with `.agent/`, `_agents/`, `_agent/` accepted) "at the root of" a workspace path, alongside `GEMINI.md` / `AGENTS.md` / `.agents/rules/*.md` for rules, and states that for a hook "the working directory is set to the directory containing `hooks.json`". Since `--add-dir` adds a directory to the workspace, this is the mechanism that explains the brief's undocumented `--add-dir`-flips-hook-discovery finding — and it means Boss's hook config need not be written into the repo at all.

### Corrected from the brief

- The traffic-split migration precedent is at `work/migrations_b.rs:2687` (`migrate_driver_traffic_split_from_codex_percentage`), not `work/schema_init.rs:732`.
- The registry guard is narrower than "bans stdout-JSONL by construction". `refuse_stdout_jsonl_ingress` asserts only over the built-in slugs constructed in `Default`; `DriverRegistry::with_driver` is deliberately exempt so the shared JSONL reader crate can register synthetic test drivers. The ban is a consistency guard against declaring an ingress that `prepare_progress_ingress` does not wire, not a blanket prohibition on the enum variant. This changes nothing for `agy` — the pane constraint already decides the question — but the guard should be cited accurately.
- Permission posture is carried by session-only `--settings` plus an explicit hook verdict for every call. `strict`, `--mode plan`, and `--sandbox` have been measured and rejected as Reviewer postures (permission study, §§3, 6, 10).

### Still unconfirmed — and therefore gating

The pane and hook studies establish interactive hooks, distinct child identity, rewrites, and Esc's missing `Stop`; the permission study now settles settings and verdict semantics. Remaining qualification includes the complete per-kind guard chain, browser and interactive command-entry coverage, dialog-aware input refusal through GhosttyKit, and naturally synchronized credential refresh across repeated expiry cycles. Plain-PTY and headless study results are evidence for the design, not real Boss dispatch acceptance.

## What `agy` is, in Boss's vocabulary

| Boss shape                                   | `agy` equivalent                                                                                                                                                                                  | Confidence                                                  |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------- |
| Pane `initial_input` runs an interactive TUI | The existing initial-brief launcher (not `agy -i`; `-i` auto-submits but runs behind the trust dialog and adds nothing; study, Q1)                                                                | Verified (study Q1: 50 KB brief delivered)                  |
| Turn boundary                                | Parent `Stop` with `fullyIdle=true` on the claimed `conversationId`; `fullyIdle=false` is a background task and the agent re-invokes itself                                                       | Verified (pane study Q7; hook study, "Verdict")             |
| Per-tool progress cadence                    | `PreToolUse` / `PostToolUse` hooks                                                                                                                                                                | Verified in pane (study, Q2)                                |
| Model-invocation boundaries                  | `PreInvocation` / `PostInvocation` bracket a model/executor invocation, not a user turn; `PostInvocation` can precede a backgrounded `PostToolUse`                                                | Verified (hook study, "Invocation events and SessionStart") |
| "Awaiting human input" signal                | _none observed_                                                                                                                                                                                   | No `Notification`-shaped event in the set                   |
| Session identity                             | `SessionStart` (fires on 1.1.12 and 1.5.5) carrying `conversationId`, which is on every hook payload                                                                                              | Verified in pane (study, Q2)                                |
| Transcript                                   | `transcriptPath` stamped on the `Stop` payload, under `brain/<cid>/.system_generated/logs/transcript_full.jsonl`                                                                                  | Verified in captured payload                                |
| Per-run config isolation                     | scoped `HOME`                                                                                                                                                                                     | Verified in a pane (study, Q8)                              |
| Credential                                   | Login-keychain item (`go-keyring` via `/usr/bin/security`), delegated by the login-keychain bridge as the Grok driver does (`LOGIN_KEYCHAIN_LEAF`)                                                | Verified (study, Q8)                                        |
| Permission posture                           | Session `--settings`: `agentMode: ""`, `toolPermission: request-review`, per-kind allowlist; explicit `PreToolUse` verdicts                                                                       | Measured on 1.5.7 and 1.5.9 (permission study, §§1, 7)      |
| Workspace trust                              | `trustedWorkspaces` via session `--settings`; `trustedFolders.json` and `~/.gemini/settings.json` are not read                                                                                    | Verified (study, Q4, Q8)                                    |
| Effort ladder                                | `--effort low\|medium\|high\|xhigh\|max` — five rungs on 1.5.5 (three on 1.1.12)                                                                                                                  | Verified (study, "Corrections")                             |
| Model menu                                   | 1.5.5 lists `gemini-4-carbon-*`, `gemini-4-argon-*`, `gemini-3.8-flash-*`, `claude-opus-5-5-*`, `claude-sonnet-5-5-*`, `gpt-oss-120b-medium`; the driver reads `agy models`, not a hardcoded list | From the study ("Corrections")                              |
| Subagents                                    | `invoke_subagent` / `define_subagent` / `browser_subagent`, no disable flag; child `conversationId` is distinct including at `Stop`                                                               | Verified (hook study, "Subagent identity")                  |

## Subscription and quota context

The operator reports having a paid Gemini subscription. The study's banner capture reads `Google AI Ultra` for the tier, but this design still has no measured Antigravity entitlement, so the fact that quota may be metered weekly across Antigravity surfaces does **not** establish that quota is restrictive for this account.

The earlier draft's token figures — 17,644 input tokens for a "reply PONG" run, an estimated 10–25k per-request overhead, and a 9.4k cached floor — came from the project brief, not from a measurement performed during this design pass. The brief included neither a capture nor the `agy` version that produced them, and this pass did not repeat the experiment on 1.1.12 or a later CLI. Their provenance and version therefore cannot be established, so this revision removes them as inputs to the verdict and makes no fleet-affordability inference from them.

The available sibling-driver evidence is not comparable enough to fill that gap. The Grok spike established that 16 concurrent sessions completed, while explicitly saying that did not prove unlimited quota; the Codex design records provider-specific quota classification as incomplete; and the shared driver-abstraction design treats token/quota and cost accounting as a known cross-driver absence. Boss therefore has no normalized baseline showing that `agy` is better or worse than `claude`, `codex`, or `grok` under the operator's subscriptions.

Quota and spend tracking should be designed once across all drivers, not as an `agy` prerequisite. This project still classifies a quota-exhaustion error correctly so dispatch does not retry blindly, but it does not measure spend, set affordability thresholds, or make quota a gate on building the driver.

## Alternatives considered

### A. Live progress ingress by tailing the transcript file, as Codex does

Codex takes `ProgressIngress::AgentJsonlFile` and explicitly leaves "hooks as the `ToolUseInterception` transport only" (`codex.rs:1611-1616`). Mirroring that for `agy` is a genuine option, and tailing a file is fully compatible with a TUI pane.

Rejected, on three checkable grounds:

- **Hooks are mandatory regardless.** `Capability::ToolUseInterception` cannot be served by reading a file after the fact — the guard must decide before the tool runs — and it is marked required-strict for the design-family kinds (`lib.rs`, `KindRequirements`). So the hook transport is being wired either way; a second live ingress adds a failure surface without adding a capability.
- **The precedent does not transfer.** Codex tails _because its hook surface is too thin to carry progress_, not because tailing is preferable. `agy` has `PreToolUse` / `PostToolUse` / `Stop`, which is the same cadence Claude's hooks provide. The reason for Codex's choice is absent here.
- **The path shape does not fit the existing ingress type.** `AgentJsonlFileIngress` matches a flat `directory` + `filename_prefix` + `filename_suffix`. `agy`'s transcript lives at a _nested_ per-conversation path, `brain/<cid>/.system_generated/logs/transcript_full.jsonl`, where `<cid>` is not known before spawn. Making this fit needs either a protocol change to the ingress type or a Boss-assigned conversation id via `--conversation` that may not accept an id for a conversation that does not exist yet. Both are avoidable dependencies.

The transcript file is still used — for `Capability::TranscriptAccess`, resolved post-hoc from the `transcriptPath` the `Stop` payload already stamps. That is what `transcript_path_for_session` is for.

### B. Reuse the concrete `ClaudeDriver` behind a compatibility shim rather than adding a driver

`agy`'s hook event names are Claude-shaped, it reads `AGENTS.md`, and its hook contract is the same stdin-JSON / stdout-JSON / `type: "command"` shape. A shim over `ClaudeDriver` looks cheap.

Rejected: Grok already ran this experiment and it produced the worst possible failure mode. Grok's `PreToolUse` accepts Claude's payload shape but **silently fails open** on Claude's `block` verdict, reserving `block` for stop-gates — so wiring Claude's five guards unmodified "would run every guard, always approve, and look identical to a healthy configuration" (`grok/hooks.rs` module docs, grounded in `grok-pretooluse-decision-vocabulary-and-tool-name-map.md`). `agy`'s verdict vocabulary is _known to differ already_ — `allow` / `deny` / `ask` / `force_ask` — so this alternative starts from the exact condition that produced the Grok defect. Separately, the registry is keyed by slug and every consuming call site resolves through it (`registry.rs`, `require`); a shim would have to lie about the slug or about the capability set, and `agy` genuinely differs on capabilities (no `Notification`-shaped event, therefore no `AwaitingInputSignal`) and on the model menu.

That rejects reuse of a **concrete driver**, not code sharing. `AgyDriver` plugs into the existing `AgentDriver` trait, `DriverRegistry` / `CapabilityResolver`, `ProgressIngress`, `ProgressSessionNormalizer`, `ProgressIdentityStore`, durable transcript store, structured-output env-file contract, transient-recovery seam, and shared conformance suites. Its hooks use the unchanged `boss-event` transport and the existing guard chain behind a dialect adapter extended for structural command denies. Driver-local code is limited to facts that genuinely differ: the descriptor and model menu, spawn flags, scoped-`HOME` layout, permission syntax, hook payload and verdict translation, transcript paths and records, control verbs, and provider error vocabulary.

The implementation tasks below must reuse those shared seams first. If characterisation reveals byte-for-byte lifecycle, provisioning, or adapter logic already present in another driver, that logic moves into a narrow lower-level helper or crate rather than being copied into `agy`; the provider-specific driver remains the consumer so shared code never depends on a higher-level driver implementation.

### C. One shared `HOME`, with runs separated by `--project` / `--new-project` / `--conversation`

**Credible alternative; not selected for the baseline (repo owner decision, 2026-11-14).** The old rejection claimed that a Reviewer and Standard worker could not hold different postures in one HOME. The shared-HOME study (§4) refutes that claim: session-only `--settings` and environment-selected hooks express different simultaneous policies without rewriting the owner's settings. It avoids relocated-home credential delegation, but not the shared sign-in's refresh exposure.

The shared-store evidence also replaces speculation about inevitable concurrency failure: sixteen 45-minute tools completed, all sixteen sessions refreshed, databases passed integrity checks, and transcripts held distinct canaries. Fifteen CLI sessions exited 0; one exited 3 on a final-continuation TLS timeout. These are bounded headless measurements, not a clean sixteen-session interactive acceptance result (shared-HOME study, §1).

Adoption would deliberately replace the private-state invariant below. The remaining qualification is concrete (shared-HOME study, §§2–6 and “Remaining acceptance work”):

- Actual owner interactive coexistence; composition with existing owner hooks; atomic installation/removal of one named dispatcher while preserving unrelated entries; measure the owner's no-op overhead in real use.
- Complete per-kind guards, including shell/file/background, lazy and eager MCP, browser paths, and interactive command entry. Deny unqualified paths or prove them unavailable; tool denials cannot prevent MCP startup or other CLI initialization.
- Hook liveness at spawn and during a run; missing/disabled hooks, malformed or partial identity, failed policy lookup, large payloads, and attempts to modify enforcement configuration. Reuse chain-attestation machinery.
- Per-run exact trust paths, settings, instructions, private logs, project allocation, persisted parent conversation identity, validated transcript paths, durable copies, and ownership-safe cleanup that cannot remove owner data. Shared global worker rules are unsuitable.
- Naturally synchronized starts and repeated expiry cycles, including the corrected credential conclusions; preserve owner onboarding/consent state.
- A qualified Boss-owned executable and disabled worker updater, since the owner's updater can still replace the installed copy.

## Chosen approach

### The invariant, stated at the load-bearing level

Not "each run gets its own `HOME`" — that is the container, and an implementation could satisfy it while symlinking the conversation DB back to a shared one and breaking everything downstream. The invariant is:

> **No two concurrent `agy` runs may share a mutable `agy` state artifact — the conversation DB, `settings.json`, the presence lock directory, `brain/`, `conversations/`, or the updater state — and the operator's own `~/.gemini` is never one of those artifacts for any run.**
>
> Exactly one artifact is deliberately shared, and it is shared because it is a credential rather than state: the login-keychain item holding the OAuth token, reached through the login-keychain bridge. Refresh serialisation is the keychain's and `agy`'s own, not a file lock Boss can take or wait on (study, Q8).

This exception describes the credential dependency, not an item-scoped access control: the filesystem bridge links the login keychain file, not just one item. Host-tool configuration and caches are also deliberately delegated; the private-state invariant applies to mutable `agy` state. The corrected credential study demonstrates refresh collisions, so delegating refresh to the vendor does not establish reliable serialization.

A per-run `HOME` is the mechanism that satisfies this. The invariant is what tests and posture assertions must check.

### The equivalence to `GROK_HOME`, and the dimension it holds on

Scoped `HOME` for `agy` is equivalent to `CODEX_HOME` / `GROK_HOME` **on the state-redirection dimension only**. On the auth dimension `agy` has no `GROK_AUTH_PATH`-style override and no `boss_codex_auth`-style snapshot; its credential lives in the login keychain, so it is delegated with the **login-keychain bridge**: a symlink `<per-run home>/Library/Keychains/login.keychain-db` → the real login keychain, the same bridge `grok/environment.rs` provisions as `LOGIN_KEYCHAIN_LEAF`. With it, `security` works from the TUI and from tool children, and no keychain prompt appeared in the study (Q8). This is _filesystem delegation_ of one keychain file, not env-var delegation and not credential-file links.

### Credential restriction: experiment scope versus product provisioning

The shared-HOME study's “Credential and host safety” subsection says a current safety restriction prevented creating that symlink **during the study**, without naming the restriction's source. Its non-replay is evidence about the study's execution boundary, not a demonstrated product prohibition or a failed bridge. The design treats it as a restriction on the investigating agent, rather than on the driver's filesystem provisioning. This is an inference from the recorded scope and the positive evidence below; the originating safety policy is not recorded, and this revision does not claim to have identified it or authorize a new live-keychain experiment.

The positive evidence is the [credential study's static boundary](../investigations/scoped-home-credential-delegation.md#static-boundary-and-implementation-consequences), which explicitly requires extracting the existing bridge into shared provisioning for both drivers, and [Grok's actual provisioning code](../../engine/driver/src/grok/environment.rs): `provision_home_delegates` delegates `LOGIN_KEYCHAIN_LEAF` through `require_delegate` and `replace_with_symlink`. Those helpers check the source, create destination parents, refuse to replace a regular destination file, and create the link. They neither read keychain contents nor invoke a keychain API. The credential study's corrected verdict records successful staggered per-run authentication and refresh with that bridge; it does not establish permanent sign-in loss or a need to sign in again.

Accordingly, retain this viable credential path and reuse the filesystem helper. The vendor CLI alone accesses and refreshes credentials. Boss code, hooks, launchers, helpers, and tests must not extract/copy tokens, alter ACLs, unlock keychains, add credential item identifiers, call Security.framework, or invoke `security`; tests use dummy files. Missing delegation or failed vendor authentication stops provisioning without an alternate credential mechanism. Naturally synchronized refresh and repeated expiry cycles remain qualification work, not permission for a workaround. No real keychain was read, linked, or modified for this design revision.

### Topology

```
GhosttyKit pane (what the operator reads: agy's own TUI)
  └─ dialog-aware initial-brief launcher → qualified agy --model … --effort … --settings <file>
       env: HOME=<per-run home>, AGY_CLI_DISABLE_AUTO_UPDATE=true
            (+ host-tool delegation, per Grok's environment.rs)
       submit key: Enter (engine change: C-m is not Return to agy)

<per-run home>/
  ├─ Library/Keychains/login.keychain-db  ← symlink to the real login keychain (credential bridge)
  └─ .gemini/
       ├─ config/
       │    └─ hooks.json                 ← global-root hooks: boss-event forwarder +
       │                                     PreToolUse guards behind the adapter
       └─ antigravity-cli/
            ├─ settings.json              ← private CLI state; posture/trust supplied via --settings
            ├─ onboarding.json            ← pre-seeded (skips onboarding; not a consent answer)
            ├─ conversation_summaries.db  ← run-private
            ├─ presence/, conversations/, brain/, log/, cache/
            └─ brain/<cid>/.system_generated/logs/transcript_full.jsonl   ← TranscriptAccess (post-hoc)

<control-dir>/                            ← Boss-owned, outside the repo and outside the agent's workspace
  ├─ initial-prompt.txt                   ← consumed by the launcher
  ├─ session-settings.json                ← explicit default agent mode, review preset, trust, rules
  └─ agent rules, if placed here rather than in the per-run home (location open; see below)

Not read by agy, so not written: ~/.gemini/settings.json, trustedFolders.json,
oauth_creds.json, google_accounts.json.
```

Two things this buys that are worth stating explicitly:

- **Hook config never enters the repo, or the agent's workspace.** The study measured both routes: a control directory outside the repo works with `--add-dir`, and `$HOME/.gemini/config/hooks.json` works with no flag, loading whether or not anything is trusted (Q3, Q4). The global root is chosen because it needs less and keeps the directory out of the agent's workspace. Nothing is written into the checkout — no `.agents/` to gitignore, no engine-written files in `jj status`. The `--add-dir` control directory is the fallback if the global root regresses on a later version.
- **Boss worker rules do not collide with the repo's tracked `AGENTS.md`.** `agy` reads `AGENTS.md`, `GEMINI.md`, and `.agents/rules/*.md`; mono has a tracked `AGENTS.md` at the root. This is exactly the collision Grok solved by writing Boss rules to `$GROK_HOME/AGENTS.md` at global scope, and a Boss-owned rules location outside the repo is the `agy` equivalent. The study did not measure where rules are best placed once `--add-dir` is dropped; the provisioning task settles it (a global-root rules file under the per-run home, or `.agents/rules/` in the control directory added with `--add-dir` as the fallback).

### Ingress and turn boundary

`ProgressIngress::HookCallback` with `HookWiringDestination::DriverOwned`, wired into the per-run home's global-root `$HOME/.gemini/config/hooks.json`. The `boss-event` shim is schema-agnostic transport — it splices `_boss_run_id` and forwards bytes — so it is reused unchanged, exactly as Claude and Grok reuse it, and the `agy` dialect is decoded engine-side by the driver's `normalize_progress_event` / `progress_session`.

The events wired: `SessionStart`, `PreToolUse`, `PostToolUse`, `PreInvocation`, `PostInvocation`, `Stop`.

- **`SessionStart` exists** on both 1.1.12 and 1.5.5 and carries `conversationId` (study, Q2). It is the carrier for `WorkerEvent::SessionStart` and for claiming progress identity; the first `PreInvocation` is not needed for that. First-parent-event claiming holds **when delivery preserves the measured ordering**: in every measured fresh session the parent's `SessionStart` and first `PreInvocation` preceded the tool call that created a child. Persist identity and preserve startup delivery ordering; do not recover a missing claim by accepting an arbitrary later `SessionStart`, which looks like another conversation start with no parent marker in its payload (hook study, "Consequence for ingress").
- **`PreInvocation` / `PostInvocation` bracket a model/executor invocation**, including its synchronous tool processing, not a user turn. `invocationNum` is zero-based within an execution and resets on a later user turn or automatic execution. When a tool continues in the background, `PostInvocation` can precede `PostToolUse`, so "post invocation" does not mean all tool children have finished (hook study, "Invocation events and SessionStart"; "Real builds, non-idle Stop, and automatic continuation"). A first-`PreInvocation` fallback for the identity claim must deduplicate against the persisted claim: `invocationNum == 0` alone would produce false session starts on every later execution.
- **`PreToolUse` reply contract.** The progress transport stays silent on stdout; the authorization adapter alone returns the final verdict. In review mode, empty stdout or `allow` leaves normal permission evaluation in place and can raise a dialog; neither authorizes an unattended call. Use `auto_approve`, `deny`, or `deny_unless_prior_grant` as specified below (permission study, §7). A progress-delivery failure must remain observable and must not turn into an approval; hook wiring must qualify its failure handling. Payloads carry the whole prompt (a 50 KB brief is about 51 KB per event; pane study, Negatives 9), which forwarding cost must account for. Argument rewrites through `overwrite` belong to the adapter, not the forwarder.
- **No `Notification`.** So `Capability::AwaitingInputSignal` is **not declared**, and its absence disposition stays the default `Degrade` — never `Synthesize`. An `agy` worker shows Working/Idle and never a fabricated `WaitingForInput`. This matches Grok's measured position.

Turn boundary is `Stop` → `TurnEnd` **only when the event carries the claimed parent's `conversationId` and `fullyIdle=true`**. A `Stop` with `fullyIdle=false` means a command outlived the tool's wait and became a background task; the pane looks idle too, and the agent invokes itself again when the task ends, so treating it as a boundary would report a finished turn that is not (study, Q7 "`fullyIdle` decides whether a `Stop` is a boundary"; hook study, "Verdict"). A child's idle `Stop` is not the parent's boundary (hook study, "Subagent identity: distinct, including at Stop"). On 1.1.12, a parent can stop with `fullyIdle=false` and then emit no later parent idle `Stop` while a child finishes with `fullyIdle=true`; the normaliser must not promote that child's boundary, and a hook-only implementation must not claim it recovers that older-version case (hook study, "Ordering caveat: the older parent can stop before its child"). Observed interactive `terminationReason` values are `NO_TOOL_CALL` (with `fullyIdle` true and false) and `TERMINAL_CUSTOM_HOOK` (1.5.5 only); `USER_CANCELED` exists in the enum and was never emitted. The binaries embed a 13-value `terminationReason` enum; `CHAINED` is present only on 1.5.5 (hook study, "Complete terminationReason vocabulary from the binaries"). The binary, not the docs, remains the source of truth.

**Esc fires no `Stop`.** A cancelled turn is silent: no hook and no transcript record, on either version (study, Q5; 4 of 4 interrupted turns). Interrupt recovery is therefore required, not conditional — see the turn-boundary task.

### Subagent attribution: mandatory identity filtering, because there is no flag

Grok needed `--no-subagents` because a subagent's `session_end` was byte-identical in shape to the parent's, differing only in `sessionId`, and Boss routes hook events by `_boss_run_id` and applies session-end by slot — so a finishing subagent flipped a live worker to `Terminated`. `agy` has no equivalent flag, so the mitigation Grok's investigation named as "what would have to change" is not optional here: **filter at ingress on `conversationId`**.

**R4 is resolved: ids are distinct, including at `Stop`.** Three child probes (ordinary on 1.1.12 and 1.5.5, delayed on 1.5.5) each gave the child its own `conversationId`, `SessionStart`, tool hooks, `Stop`, transcript path, and artifact directory (hook study, "Subagent identity: distinct, including at Stop"). Parent-side `invoke_subagent` hooks retain the parent's ID; subsequent hooks for work the child executes carry the child's ID. Common runtime payloads did not expose a parent ID, root ID, or subagent Boolean.

The seam already exists: `ProgressIdentityStore::claim_progress_identity(run_id, session_id)`. Claim the parent identity once, through that mechanism, at the first parent event, and reject boundary-shaped events with another ID. Children must neither replace that claim nor become eligible to finish the worker; filtering a child's lifecycle event must not disable the child's `PreToolUse` guards (hook study, "Consequence for ingress"). First-parent-event claiming is an **ordering assumption**: it holds when delivery preserves the measured startup sequence. Persist identity rather than recovering a missing claim from an arbitrary later boundary event.

### Permission posture

`write_permission_config` writes a run-private JSON file and returns `--settings <file>` in `PermissionArtifacts::extra_args`. Every kind explicitly sets `"agentMode": ""` and `"toolPermission": "request-review"`. Auto is the default if `agentMode` is omitted on 1.5.7 and 1.5.9. Session settings override the persistent file and are never saved; unknown keys and invalid JSON exit non-zero through this flag (permission study, §§1–2).

One `PreToolUse` adapter aggregates all guards and supplies exactly one decision for **every** tool call: `auto_approve` after an authorized call passes the guards, `deny` with a reason for a forbidden call, or `deny_unless_prior_grant` with `permissions.allow` for allowlist kinds. The latter also honors built-in workspace-read grants, so it does not replace guard policy. Standard, Reviewer, and Triage use guarded approvals; AnswerAgent, ReviewGuide, and Progression use guarded allowlists. Match all kinds exhaustively without enabling currently ineligible dispatches: ReviewGuide remains pinned to Codex and this design does not change PR review routing (permission study, §7; shared-HOME study, §2).

The three former Reviewer candidates are rejected: `strict` raised dialogs on 16/16 calls and ignored allow rules; `--mode plan` only prefixes the prompt with `/plan `; `--sandbox` cannot run `jj`, refuses nested Seatbelt, and permits a model-controlled `BypassSandbox` escape. None belongs in `extra_args`, nor do `--dangerously-skip-permissions` or `always-proceed` express the posture (permission study, §§3, 6, 10).

Carry exact, canonical workspace trust paths in `trustedWorkspaces` through the same session file. Preseed private onboarding only under an explicit consent policy; skipping its screen does not answer data-collection consent. Keep `permissions.ask` empty: ask rules override `auto_approve` and create dialogs. Use `permissions.deny` only as a second layer for native file reads/writes/edits, with per-kind paths; structural command denies belong in the adapter. Validate every rendered rule before spawn and inspect the private CLI log after initialization for `ignoring invalid` warnings before admitting pane input: malformed deny rules are otherwise silently dropped and fail open even through `--settings` (permission study, §§2, 4–5, 7).

### Blocking dialogs and pane input

The engine must refuse **all pane input while a blocking dialog is displayed**, including initial-brief submission, probes, nudges, replies, and Return retries. A paste plus Return approved a pending command; typed text beginning with `3` persisted a global always-allow rule. Dialogs do not time out; a missing hook in review mode stops at a dialog rather than providing a safe unattended fallback (permission study, §§7–8).

Confirm hook liveness and valid settings before delivery, recognize approval, trust, onboarding, and Settings Error screens, and surface a blocked-input condition through existing attention/status handling. Do not send a probe to discover whether a dialog is active or use a generic idle marker as proof of a safe composer. This needs GhosttyKit qualification: the permission measurements used a plain PTY. It does not establish a native `AwaitingInputSignal` capability.

### Hook adapter: fail closed on anything unrecognised

Reuse Boss's guard chain behind an `agy` dialect adapter: canonicalize camelCase payloads, aggregate guards, and translate an authorized approval to `auto_approve`, **never `allow`**, which still permits a dialog in review mode. For allowlist kinds, return `deny_unless_prior_grant` only after the guards pass. Rejections, unknown verdicts/tools, malformed arguments, missing policy or run identity, and adapter failures deny explicitly with a reason; never emit `ask` or `force_ask`. Silent output is not an authorization verdict. Canonical rewrites map to `overwrite`, a shallow argument merge, and must still pass policy (permission study, §7; hook study, “PreToolUse can rewrite arguments”; shared-HOME study, §2).

Move structural denies for `bossctl`, Boss state access, `sudo`, and destructive recursive removal out of command settings rules into this adapter/guard path. Rules are evaded by absolute paths, wrappers, flags, redirection and substitution; combining a command deny with a `read_file` deny even allowed the prohibited command in 6/6 probes. Native file rules do not stop shell writes. Reviewer policy must inspect shell-mediated mutations as well as native writes and publication (permission study, §5).

Deny `BypassSandbox: true`; decode and check server, tool, and arguments for both lazy `call_mcp_tool` and eager `mcp_*` calls; deny `manage_task send_input` so stdin cannot become an uninspected command channel. Child tools inherit the same policy even when their lifecycle events are filtered. Protect settings, hooks, private state, and other runs' data through both canonical file paths and command analysis. Browser and interactive shell shortcuts require coverage or structural disabling before acceptance; MCP startup is outside tool admission (permission study, §6; shared-HOME study, §2).

Loaded-hook error cases and missing discovery are different: validate the chain at spawn and detect later disarming, reusing Codex's chain-attestation precedent. Qualify failure behavior on the selected review-mode binary; crash/timeout/malformed-output measurements in the shared-HOME study used the skip flag. Test these cases plus unknown verdicts, wrappers, absolute paths, redirects, MCP variants, bypass requests, child calls, and stdin submission using harmless scratch fixtures.

### Soft-deny detection (Risk 3b)

An ungranted interactive call **blocks on a dialog**, with no matching `PostToolUse` or `Stop`. Headless soft-denial exits 0 with empty stdout and a stderr notice, also **without `Stop`**. Neither is a clean turn boundary. Interactive rule/hook denial instead yields `PreToolUse`, no `PostToolUse`, and an idle `Stop` with empty error; all 62 measured denials followed that sequence (permission study, §9).

Track pending calls by conversation identity and `toolCall.id`. At the next idle `Stop` for that conversation, a `PreToolUse` without its matching `PostToolUse` is the denied/unobserved-tool signal. A child's event must not close the parent's call, and a non-idle background `Stop` must not trigger detection. `Stop.error` and pane tool rows are not signals; transcript `status: "ERROR"` and its reason corroborate. A command that ran and failed, including a sandbox refusal, has a post-tool event and is not a soft-denial.

Reuse `UNOBSERVED_COMMAND_MARKER` and `codex_unobserved_command.rs`: emit the internal marker-carrying `Notification` ahead of the qualifying `Stop`, stage it in the dispatcher, and consume it in `on_stop_inner` to file attention and refuse a `no-changes-needed` declaration. This internal event is not a native waiting-for-input signal. The detector cannot detect a missing boundary; dialog refusal and process/error monitoring handle those paths separately.

### The acceptance gate: a state, not a checkpoint

The Grok design's "10 consecutive green PRs" Phase-1 gate was never executed, and its own retrospective names why: _a gate at the end of a phase can only hold back the next phase, and "Phase 1 is not yet accepted" was never a state that stopped anything._ Repeating that structure here would be a known-defective design.

So the gate is inverted. Rather than a checkpoint at the end, it is the **default state, enforced by existing engine code**:

- **What enforces it.** `DriverTrafficSplit`'s shipped default gives `agy` **zero**, and `allocate_among` treats zero as literally empty — "zero means zero", with `driver_for_bucket` handing it no bucket at all. No allocated work reaches `agy` until an operator makes a deliberate, atomic, single-write change to the persisted split. The four-way migration lands with a unit assertion that the shipped default is `agy = 0`, so a later edit that quietly raises it fails a test rather than silently changing fleet behaviour.
- **The state while unmet: `pinned-only`.** `agy` is registered, resolvable, and reachable _only_ by an explicit `tasks.driver` / product `default_driver` pin that clears the capability gate. This is a steady state, not a waiting state: nothing is blocked, nothing silently proceeds, and the driver is exercised on real work the whole time.
- **What would move it.** A recorded run of N pinned `agy` executions reaching merged PRs without engine-side incident, followed by an explicit operator decision to raise the share. The run evidence lives in the [acceptance ledger](./antigravity-driver-acceptance-ledger.md) so the durable reasoning here is not disturbed every time the status changes. Cross-driver spend and quota accounting is not part of this gate.
- **Who can move it.** Raising the share is an operator action against engine state. No task in the breakdown below has "raise the share" as its deliverable, because no cube worker can or should perform it. The ledger records whether the criterion is met; it does not raise the share.

One more structural rule, taken from the same retrospective: **the acceptance evidence must come from real Boss dispatch, not from a harness.** A hand-built reproduction is assembled from the same beliefs that produced the code, so it is structurally incapable of finding the integration bugs — the events-socket routing, the credential strip, the adapter identity mismatch — that are precisely what killed Grok's sweep. The pane study used GhosttyKit; the hook, permission, and shared-HOME studies also use PTY/headless apparatus. Their measurements inform implementation, while pane input safety and end-to-end acceptance must be qualified through actual Boss panes.

### Capability declaration (proposed; implementation conformance remains gating)

Declared: `Spawn`, `WorkspaceProvisioning`, `PermissionPolicy`, `ModelAndEffortMenu`, `ProgressObservation`, `ToolUseInterception`, `TurnBoundary`, `StructuredOutput`, `TranscriptAccess`, `ControlVerbs`, `PromptComposition`.

Omitted, each with its reason:

- **`ToolProvisioning`** — Degrade. Boss injects no MCP servers or tool definitions for any driver. Declaring it would overclaim.
- **`AwaitingInputSignal`** — Degrade, never Synthesize. `agy`'s hook set has no `Notification`-shaped event, so there is nothing honest to bind to.
- **`CommandOutcomeObservation`** — Degrade, never Synthesize. **Measured absent:** across 36 `PostToolUse` payloads the union of fields contains `error` but no structured exit status or command output on either version. An intentional `exit 7` produced `PostToolUse.error=""` with no `exitCode`; a successful engine build did the same; failed Bazel apparatus attempts did too. An empty hook `error` means neither "exit 0" nor "the requested work succeeded." The protobuf descriptor's `result` field was not in the runtime JSON (hook study, "PostToolUse does not establish command outcome"). This stays undeclared, which also keeps `agy` out of `ConflictResolution` / `CiRemediation` by way of the gate rather than by a hardcoded list. Hooks cannot later unblock that eligibility.

`ToolUseInterception` is rewrite-or-deny. `PreToolUse` rewrites arguments through `overwrite` on both measured binaries (hook study, "PreToolUse can rewrite arguments"). The capability is declared as full interception; the adapter implements that rewrite.

## Risks / open questions

**R1 — Hooks may not fire in interactive mode. Resolved: they fire.** The pane viability study measured `SessionStart`, `PreInvocation`, `PostInvocation`, `PreToolUse`, `PostToolUse` and `Stop` in interactive mode on both 1.1.12 and 1.5.5 (Q2). The residual hazard is that an Esc-cancelled turn fires none of them (see the turn-boundary task).

**R2 — Scoped `HOME` in a pane. Largely resolved.** The study found all state, hooks and tool children stay under the scoped `HOME` for a long-lived TUI, and the real `~/.gemini` was not touched (Q8). Still unexercised: a browser subagent. The Playwright cache relocation seen on 1.1.12 (`Library/Caches/ms-playwright-go` created on `agy models`) was not reproduced on 1.5.5, so the per-run re-download cost is a 1.1.12 observation until a browser subagent is run.

**R3 — Credential delegation under concurrency.** The corrected credential study found successful staggered per-run sessions but also failed saves when refreshes landed tens of milliseconds apart: the vendor temporarily removed the item until a later save recreated it. It did not establish permanent sign-in loss. The shared-HOME study (§1) observed sixteen natural refreshes without failed-save/logout diagnostics under staggered starts; it does not prove shared HOME prevents the race. Qualify naturally synchronized starts, repeated expiry cycles, offline startup and sleep/wake before fleet acceptance; no Boss credential accessor or alternate store is implied.

**R4 — Subagents may share the parent `conversationId`. Resolved: they do not.** A child's `Stop.conversationId` differs from its parent's in all three child probes, including at `Stop` (hook study, "Subagent identity: distinct, including at Stop"). Ingress identity filtering is therefore a viable mitigation, and there is still no `--no-subagents` fallback. Residual hazards: the 1.1.12 missing parent idle `Stop` after a child finishes (do not treat the child's boundary as the parent's); nested or concurrent children were not exercised; first-event claiming depends on preserving startup delivery order (hook study, "Consequence for ingress" and "Ordering caveat").

**R5 — `agy` auto-updates.** Export exactly `AGY_CLI_DISABLE_AUTO_UPDATE=true`; `=1` is ineffective. A worker's disabled updater does not stop the owner or a manual update replacing the installed copy. Launch a qualified Boss-owned executable, verify its identity at spawn, and qualify upgrades outside active startup (shared-HOME study, §6).

**R6 — Quota exhaustion remains a runtime condition.** The paid subscription's entitlement is unknown, and this project deliberately does not build the missing cross-driver accounting mechanism. The driver must still classify quota exhaustion distinctly so recovery does not redispatch into the same exhausted account.

**R7 — Terms of service are silent on programmatic driving, and silence is not permission.** No ToS text was located either permitting or prohibiting driving `agy` programmatically. The weak reading is "not prohibited, therefore allowed". The stronger and more likely reading is that **no decision was ever made** about this use — an unaddressed gap, not a granted allowance — and automated fleet use on a personal Google account is exactly the case a provider would address if it addressed any. This needs an operator call before the driver runs on real work, and it is in the questions manifest.

**R8 — CI cannot authenticate.** `agy` requires a prior interactive login and stores the credential in the login keychain. Any live-CLI conformance gate (`BOSS_REQUIRE_AGY_CLI`) will therefore only be meaningful on a developer machine with a signed-in account, exactly as `BOSS_REQUIRE_GROK_CLI` is. Fixture-side pins carry the contract in CI.

**R9 — Directory override.** The corrected credential study (“Directory override choice”) measured `ANTIGRAVITY_EXECUTABLE_DATA_DIR` and `ANTIGRAVITY_CONFIG_REL_DIR` as ineffective on 1.5.5. They are not alternatives in the isolation contract; any later-version claim needs new evidence.

## Proposed implementation task breakdown

Breakdown size: 22 entries (20 in-scope, 2 deferred); the pane, hook, credential, and permission studies are complete, with shared-HOME findings also incorporated. The engine submit-key entry was added by the pane study — a new fourth driver reaching across four investigations (pane viability, isolation, hooks, permissions), a breaking protocol+DB+Swift split migration, a new driver module with ten distinct trait seams (skeleton, home provisioning, retention, spawn, hook wiring, guard adapter, progress normaliser, turn boundary, permission rendering, transcript), a soft-deny detector, and a conformance extension; each is its own reviewable PR, and the count sits below the 30 tasks the comparable Grok integration actually shipped.

Parallelism is noted per entry. The depth-0 pane spike and traffic-split migration may run concurrently.

Every entry starts from the driver-neutral machinery named in Alternative B. New `agy` code implements only the provider dialect or run-state details that the shared traits cannot express; identical logic found in an existing driver is extracted into a lower-level helper or crate instead of copied.

### Ghostty pane viability spike for `agy`

**Done:** delivered as [`ghostty-agy-pane-viability.md`](../investigations/ghostty-agy-pane-viability.md) (verdict: viable, with the corrections folded into this document). The entry is kept because later entries depend on it. Original scope: run `agy` 1.1.12 as an interactive TUI inside a **real GhosttyKit-hosted pane** — not a standalone terminal, not a harness — and answer the nine-item checklist: does `-i "<prompt>"` auto-submit in a PTY; do hooks fire at all in interactive mode; is `--add-dir` required for `.agents/hooks.json` discovery and does a control directory _outside_ the repo work; does workspace trust independently gate discovery; does Esc emit `Stop` or skip it; is mid-turn pane input buffered or rejected; what `terminationReason` values does the binary actually emit; does scoped `HOME` hold up in a pane; and what are the banner / busy / prompt / starting strings for a `PaneMonitorSpec`. Model the artefact on `ghostty-grok-pane-viability.md`, including its hard apparatus rule. **This study is a gate, not a validation**: it can return "not viable", and that outcome stops the project rather than prompting a workaround. Deliverable: `tools/boss/docs/investigations/ghostty-agy-pane-viability.md` plus an artifacts directory. No driver code.

- **Effort:** large
- **Dependencies:** none
- Scope: in-scope

### Four-way driver traffic split migration

Widen `DriverTrafficSplit` from three shares to four across every layer that must move together: `protocol/src/types/driver_split.rs` (the struct, `DRIVERS_IN_BUCKET_ORDER`, `validate`, `driver_for_bucket`, the error type's message), the DB migration in `work/migrations_b.rs` following the `migrate_driver_traffic_split_from_codex_percentage` precedent, `work/driver_allocation.rs`'s renormalisation, and the Swift mirror in `app-macos/Sources/DriverTrafficSplit.swift` plus its `DriverSlug` enum, `SettingsView` stepper, and tests. Rust and Swift land in **one PR deliberately**: the split crosses the wire as a single value, so a half-landed migration breaks decoding rather than degrading. Ships `agy = 0` in `Default` — the behaviour-preserving state — with a unit assertion pinning that default, which is the enforcement half of the acceptance gate.

- **Effort:** large
- **Dependencies:** none — runs in parallel with both phase-0 investigations
- Scope: in-scope

### Acceptance ledger and pinned-only gate state

**Done:** delivered as [`antigravity-driver-acceptance-ledger.md`](./antigravity-driver-acceptance-ledger.md). See the ledger for current gate status and evidence. Original scope: create the acceptance ledger as a separate document from this design so status can be refreshed without touching durable reasoning: the criterion (N pinned `agy` executions reaching merged PRs without engine-side incident), an empty evidence table, the explicit statement that the project sits in `pinned-only` state until the criterion is met, and the explicit statement that raising the share is an operator action no task performs. Document how `pinned-only` is enforced — zero share plus "zero means zero" in `allocate_among`, with an explicit pin still routing — and cross-link it from this design. Spend and quota do not appear in this driver-specific ledger.

- **Effort:** small
- **Dependencies:** Four-way driver traffic split migration
- Scope: in-scope

### Scoped-`HOME` isolation and credential delegation characterisation

**Done:** [corrected credential study](../investigations/scoped-home-credential-delegation.md), especially “Verdict” and “Static boundary and implementation consequences”, with [shared-HOME study](../investigations/agy-shared-home-hook-permissions.md), §§1, 4–6, providing the alternative's comparison. Retain per-run HOME and the filesystem-only bridge as explained above; preserve host tool configuration, cache paths and `BOSS_WORKER_TOOL_PATH`. Staggered success does not settle synchronized natural refresh or repeated expiry cycles. The shared-HOME study did not replay the bridge because of its execution safety restriction; that is not a measured provisioning failure.

- **Effort:** large
- **Dependencies:** Ghostty pane viability spike for `agy`
- Scope: in-scope

### Hook surface characterisation: event vocabulary, subagent attribution, interrupt

**Done:** delivered as [`agy-hook-surface.md`](../investigations/agy-hook-surface.md) (findings folded into this document). The entry is kept because later entries depend on it. Original scope: enumerate what the hook stream actually carries in interactive mode: the full `terminationReason` vocabulary from the binary (not the docs); `PreInvocation` / `PostInvocation` semantics and whether the first `PreInvocation` can carry `WorkerEvent::SessionStart` and the identity claim; whether `PostToolUse` carries a reliable per-command exit status (which decides `CommandOutcomeObservation`); whether `PreToolUse` can rewrite tool input or only deny (which decides whether `ToolUseInterception` is deny-only); the exact `allow`/`deny`/`ask`/`force_ask` semantics including what an unrecognised verdict does; and — highest severity — **whether a subagent's `Stop` carries a `conversationId` distinct from its parent's**, since there is no `--no-subagents` and ingress identity filtering is the only available mitigation. Deliverable: `tools/boss/docs/investigations/agy-hook-surface.md`. No driver code.

- **Effort:** large
- **Dependencies:** Ghostty pane viability spike for `agy`. Runs in parallel with the scoped-`HOME` characterisation (separate documents, no shared files).
- Scope: in-scope

### Permission, trust, and soft-deny characterisation

**Done:** [permission study](../investigations/agy-permission-trust-soft-deny.md), §§1–10, measured on 1.5.7 and 1.5.9. It rejects all three former Reviewer candidates and selects explicit default agent mode, review preset, session settings, and one hook verdict per call. It establishes rule-validation hazards, dialog-input hazards, and the missing-post-tool signal at an idle Stop. Remaining work is implementation qualification of the complete guards and dialog handling in Boss panes, not another choice among the rejected postures. No driver code shipped with the study.

- **Effort:** medium
- **Dependencies:** Scoped-`HOME` isolation and credential delegation characterisation
- Scope: in-scope

### `AgyDriver` skeleton: descriptor, capabilities, model menu, registry

The seed changeset, modelled on the Grok skeleton PR: `DriverDescriptor` (slug `agy`, binary `agy`, config dir, agent-rules filename, initial-prompt filename), the `ModelMenu`'s seven function pointers over the `agy models` catalogue with the five-rung `low|medium|high|xhigh|max` effort ladder (1.5.5; the study's menu lists `gemini-4-carbon-*`, `gemini-4-argon-*`, `gemini-3.8-flash-*`, `claude-opus-5-5-*`, `claude-sonnet-5-5-*`, `gpt-oss-120b-medium`), the `CapabilitySet` with every omission carrying its recorded reason, registration in `DriverRegistry::default()`, and the minimum trait implementations needed to compile and keep the conformance suite green — including a native-dialect transcript fixture in `conformance/native_transcript.rs` and a transcript normaliser sufficient to surface a `[blocked]` marker from it, since that suite fails closed on any registered slug without one. No spawn line, no provisioning, no hooks. Capability declarations must be justified from the phase-0 characterisation, not asserted: `CommandOutcomeObservation` is measured absent and omitted; `ToolUseInterception` is rewrite-or-deny via `overwrite`.

- **Effort:** medium
- **Dependencies:** Hook surface characterisation; Permission, trust, and soft-deny characterisation
- Scope: in-scope

### Engine: pane submit key `Enter` instead of `C-m`

Under Boss's tmux options `send-keys C-m` reaches `agy` as `ESC [27;5;109~`, not Return, because `agy` requests xterm modifyOtherKeys mode 2; `send-keys Enter` arrives as `\r` and submits (study, "The submit key"). Boss ends a delivery with `C-m` in three places: `Tmux::send_keys` (`tmux/src/lib.rs`), spawn confirmation's Return retry (`pane_spawn.rs`), and the spawn-ack sweep's `resubmit_prompt` (`engine/app/src/app.rs`). Without a fix the first brief lands (the launcher writes a raw `\r`) and nothing the engine types afterwards is submitted: every probe, nudge, answer and Return-retry sits in the composer. Either send `Enter` at the three call sites after checking the three shipped drivers, or add a driver-declared submit key defaulting to `C-m`. Shared-code change outside the driver, with a test per call site.

- **Effort:** medium
- **Dependencies:** none
- Scope: in-scope

### Per-run `HOME` provisioning and credential delegation

The `agy/home.rs` + `agy/environment.rs` pair: allocate a per-run container, materialise the scoped `antigravity-cli/` tree (private CLI state plus `onboarding.json` under the agreed consent policy; posture and trust in a private session-settings file), extract and reuse the existing filesystem-only login-keychain bridge (`Library/Keychains/login.keychain-db`, as `grok/environment.rs` does), preserving the credential boundary above, export `AGY_CLI_DISABLE_AUTO_UPDATE=true` and use a qualified Boss-owned executable whose identity is checked at spawn, write the global-root `.gemini/config/hooks.json`, set `AGY_CLI_HIDE_ACCOUNT_INFO=true` and a run-private `--log-file`, delegate host-tool state (`gh`, `cube`, `jj`, git, ssh, caches) per the measured need, **keeping host Bazel cache locations and `BOSS_WORKER_TOOL_PATH`**: Bazel's cache paths derive from `HOME`, so a fully-scoped home without those delegations puts the repository cache lexically inside the checkout and omits the worker's required tool path (hook study, "Real builds, non-idle Stop, and automatic continuation"). Assert the posture before spawn so a misconfigured home fails loudly rather than running against the operator's `~/.gemini`. The posture assertion checks **the invariant, not the container**: no mutable state artifact shared between concurrent runs, and the operator's `~/.gemini` never reachable.

- **Effort:** large
- **Dependencies:** `AgyDriver` skeleton; Scoped-`HOME` isolation and credential delegation characterisation
- Scope: in-scope

### Per-run `HOME` retention sweep

Reclaim per-run `agy` homes on the same pattern as `grok_home_retention_sweep.rs` and the `grok-home-retention` crate: bounded retention, a safe-to-delete assertion that refuses to touch anything outside Boss's own homes root, and scheduling from engine core. Each home carries a SQLite conversation DB and a `brain/` tree, so unreclaimed homes are a disk-exhaustion risk at fleet scale, not housekeeping.

- **Effort:** medium
- **Dependencies:** Per-run `HOME` provisioning and credential delegation
- Scope: in-scope

### Pane spawn invocation and `PaneMonitorSpec`

Compose the interactive pane command as the existing initial-brief launcher running `agy` with `--model`, `--effort`, `--settings <file>` and the scoped-`HOME` env directives — **not** `agy -i` and no `--add-dir` (the launcher delivered a 50 KB brief in the study, Q1; `-i` runs behind the trust dialog). Supply the `PaneMonitorSpec` agent / busy / starting / prompt markers from the study's "Proposed `PaneMonitorSpec`" section, so the app's fallback status pill works before the first hook event arrives. Creates the Boss-owned control directory outside the repo for the initial prompt and session settings, and writes agent-rules to whichever outside-the-repo location provisioning settled. Confirm hook liveness and inspect settings logs before delivery; refuse input on blocking dialogs, including the launcher's first brief and spawn-confirmation retries (permission study, §§2, 8).

- **Effort:** medium
- **Dependencies:** Per-run `HOME` provisioning and credential delegation; Ghostty pane viability spike for `agy`
- Scope: in-scope

### Global-root hook wiring: `boss-event` progress forwarder

Write `$HOME/.gemini/config/hooks.json` in the per-run home wiring `SessionStart`, `PreToolUse`, `PostToolUse`, `PreInvocation`, `PostInvocation`, and `Stop` to the unchanged `boss-event` shim with the same `BOSS_EVENTS_SOCKET` / `BOSS_LEASE_ID` / `BOSS_RUN_ID` / `BOSS_WORKSPACE` env prefix Claude and Grok use, and declare `ProgressIngress::HookCallback` with `HookWiringDestination::DriverOwned`. Raw payloads forward unchanged; decoding is the engine-side normaliser's job. Keep progress stdout silent and route the final authorization reply through the single adapter; never treat silent forwarding as approval. Surface delivery failures and qualify the combined path in review mode, including large payloads and missing hooks (permission study, §7; shared-HOME study, §2). Discovery from the global root is measured (study, Q3); the out-of-repo `--add-dir` control directory is the fallback.

- **Effort:** medium
- **Dependencies:** Pane spawn invocation and `PaneMonitorSpec`; Hook surface characterisation
- Scope: in-scope

### Hook payload adapter and `PreToolUse` guards

Implement the adapter policy specified above: one final verdict after the reused guard chain, `approve` → `auto_approve`, allowlist kinds → `deny_unless_prior_grant`, and explicit `deny` with a reason for rejection or invalid input. Translate rewrites to `overwrite`. Move settings-only structural command denies into the guards; cover shell-mediated Reviewer writes, `BypassSandbox`, lazy/eager MCP, `manage_task send_input`, child calls, and protected state/configuration. Test hook liveness and each failure path in the selected review mode. This makes `ToolUseInterception` real; a silent progress forwarder cannot authorize calls (permission study, §§5–7; shared-HOME study, §2).

- **Effort:** medium
- **Dependencies:** Global-root hook wiring: `boss-event` progress forwarder
- Scope: in-scope

### Progress normaliser and conversation-identity filtering

Implement `normalize_progress_event` / `progress_session` for the `agy` hook dialect into `WorkerEvent`, mapping `SessionStart` to `WorkerEvent::SessionStart`. Claims the parent `conversationId` (carried by `SessionStart`) through `ProgressIdentityStore::claim_progress_identity` at the first event — **an ordering assumption**: that first event is the parent's, which held in every measured fresh session provided delivery preserves startup order (hook study, "Consequence for ingress"). **Drops boundary-shaped events carrying any other `conversationId`**, which is the only available mitigation for `agy`'s undisableable subagents. Characterisation found the child's id is distinct including at `Stop`, so the filter works; it must not promote a child's idle `Stop` to the parent's, including the 1.1.12 case where no later parent boundary arrived (hook study, "Ordering caveat").

- **Effort:** large
- **Dependencies:** Global-root hook wiring: `boss-event` progress forwarder; Hook surface characterisation. Parallel with the guard adapter (separate modules).
- Scope: in-scope

### Turn boundary and interrupt recovery

Map `Stop` to `TurnEnd` **only when the event carries the claimed parent's `conversationId` and `fullyIdle=true`**; a `Stop` with `fullyIdle=false` is a background task and not a boundary (study, Q7; hook study, "Verdict"). A child's idle `Stop` is never the worker's boundary. Retain the 1.1.12 missing-parent-boundary case as a regression fixture (hook study, "Driver follow-up and remaining limits"). The binaries embed a 13-value `terminationReason` enum (`CHAINED` only on 1.5.5); observed interactive values are `NO_TOOL_CALL` and `TERMINAL_CUSTOM_HOOK`; `USER_CANCELED` was never emitted (hook study, "Complete terminationReason vocabulary from the binaries"). Interrupt recovery is **required**: the study found Esc fires no `Stop` on either version, leaving no hook and no transcript record (Q5), so implement `prepare_interrupt_recovery` / `is_interrupt_recovery_turn_end` against a run-private artifact the way `grok/turn_end_recovery.rs` does, keyed on the `--log-file` line or the pane text (study, follow-up 4).

- **Effort:** medium
- **Dependencies:** Progress normaliser and conversation-identity filtering
- Scope: in-scope

### Permission config rendering per worker kind

Implement the permission posture above as run-private JSON passed by `PermissionArtifacts::extra_args = ["--settings", <file>]`: explicit `agentMode: ""`, `toolPermission: request-review`, exact `trustedWorkspaces`, per-kind `permissions.allow`, native-file denies only, and no ask rules. Mirror the exhaustive worker-kind policy without changing eligibility. Validate rule grammar before spawn and check the private log for ignored entries after initialization. Structural command denies belong in the adapter, not this settings renderer (permission study, §§1–2, 5, 7, 10).

- **Effort:** large
- **Dependencies:** Per-run `HOME` provisioning and credential delegation; Permission, trust, and soft-deny characterisation. **Co-edits `agy.rs`'s trait impl with the pane-spawn entry — land after it and forward-port its changes preservingly.**
- Scope: in-scope

### Soft-deny and no-op-worker detection

Track each `PreToolUse` by conversation and `toolCall.id`; match its `PostToolUse`, and at that conversation's next idle `Stop` flag unmatched calls. Reuse the existing marker staging, bounded audit trail, overflow attention, and refusal of `no-changes-needed` in `codex_unobserved_command.rs`. Test denied calls separately from completed-but-failed tools, non-idle background boundaries, child calls, headless denial without Stop, and interactive dialogs without Stop. Do not infer denial from `Stop.error` or pane tool rows (permission study, §9).

- **Effort:** medium
- **Dependencies:** Progress normaliser and conversation-identity filtering; Permission config rendering per worker kind
- Scope: in-scope

### Control verbs and error classification

Declare `probe` / `interrupt` / `stop` / `reap` / `mid_turn_pane_input` from what the spike measured, not from what seems likely, with every verb that types into the pane delivered via the submit-key change above — all input paths must first enforce the blocking-dialog refusal above (permission study, §8). Outside dialogs, `MidTurnPaneInput` is `Buffers`: the study proved mid-turn pane input is held as a queued message and run when the turn ends, under the default queue setting (Q6). Implement `classify_error` for the `agy`/Google error surface, with **quota exhaustion as a first-class class**: regardless of entitlement, a quota-exhausted worker misclassified as a generic failure would drive redispatch straight back into the same wall.

- **Effort:** medium
- **Dependencies:** Pane spawn invocation and `PaneMonitorSpec`; Ghostty pane viability spike for `agy`. **Co-edits `agy.rs`'s trait impl — land after the permission-config entry and forward-port.**
- Scope: in-scope

### `TranscriptAccess`: durable store, path resolution, and session normaliser

Resolve `transcriptPath` from the `Stop` payload via `transcript_path_for_session`, provision the durable per-execution transcript store so the transcript outlives the recycled workspace and the reclaimed per-run `HOME`, implement `TranscriptSessionNormalizer` for `agy`'s native `transcript_full.jsonl` dialect, and replace the skeleton's minimal fixture with one captured from a real run. Includes `extract_error_from_transcript` and `structured_output_fallback` candidates for the `agy` dialect — the driver-neutral `BOSS_STRUCTURED_OUTPUT` / `BOSS_PR_URL_OUTPUT` env-file contract already applies unconditionally, so these are the fallback path only.

- **Effort:** large
- **Dependencies:** Progress normaliser and conversation-identity filtering
- Scope: in-scope

### Conformance suite extension for `agy`

Extend the conformance suite to cover the fourth driver on the same terms as the other three: `agy_goldens.rs` with captured real payloads, the boundary-equivalence assertions (`only_stop_shaped_events_are_turn_boundaries_on_any_driver` and its siblings), ingress equivalence, and a qualified version pin of 1.5.7 or later with a `BOSS_REQUIRE_AGY_CLI` live-CLI gate that soft-skips when `agy` is absent — noting in the pin's docs that CI cannot authenticate `agy`, so the live check is meaningful only on a signed-in developer machine and the fixture-side pins carry the contract everywhere else.

- **Effort:** medium
- **Dependencies:** Turn boundary and interrupt recovery; `TranscriptAccess`: durable store, path resolution, and session normaliser
- Scope: in-scope

### `ConflictResolution` / `CiRemediation` eligibility for `agy`

Do **not** declare `Capability::CommandOutcomeObservation` for `agy` from hooks. The hook surface study measured `PostToolUse` as carrying no structured exit status or command output; an empty `error` is not exit 0, and the protobuf `result` field is absent from runtime JSON (hook study, "PostToolUse does not establish command outcome"). Hooks therefore cannot unblock `ConflictResolution` / `CiRemediation` eligibility. A later path would need a different observation source (for example the native transcript's `Command exited with code N` line), which is outside this hook-based design.

- **Effort:** medium
- **Dependencies:** Hook surface characterisation
- Scope: deferred (future / not a v1 blocker) — hooks cannot supply the capability; no driver but `claude` declares it today, and Codex's history is that a plausible-looking exit-status field turned out unreliable.

### Remote / SSH host support for `agy` workers

Extend `agy` provisioning to the remote-host spawn path — the `is_remote` branch that skips the engine-data-dir sandbox and forwards the events socket — so distributed execution can dispatch `agy`.

- **Effort:** large
- **Dependencies:** Per-run `HOME` provisioning and credential delegation; Permission config rendering per worker kind
- Scope: deferred (future / not a v1 blocker) — v1 is local-macOS only, and the credential-delegation story does not survive a remote host without its own design (there is no second signed-in account to delegate to).
