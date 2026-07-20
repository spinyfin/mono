# Worker proposal API: replace fragile worker-to-engine seams

- **Date:** 2026-07-19
- **Provenance:** execution `exec_18c3dea63b093930_c8a` (project_design), project "Worker proposal API: replace fragile worker-to-engine seams"
- **Related:** T2945 (marker-recovery diagnosis), T2944 (produced-task dedup gate), T2935 (design postmortem — requirement 7), T303/candidateImpact (phantom follow-up), P783 (task-breakdown auto-populate)
- **Related designs:** [attentions](attentions.md), [auto-populate-project-tasks-on-design-pr-merge](auto-populate-project-tasks-on-design-pr-merge.md), [v2-design-risks](v2-design-risks.md) (R3 worker isolation), [engine-app-rpc](engine-app-rpc.md), [agent-driver-abstraction](agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md) (T1414), [unify-blocking-signal-remediation](unify-blocking-signal-remediation.md), [engine-counter-metrics-framework](engine-counter-metrics-framework.md)

**TL;DR:** Workers today tell the engine things by emitting marker lines and fenced blocks that the engine scrapes out of transcript tails — a contract that is empirically failing (100% of recent triage finalizations rode the marker-recovery WARN heuristic; "filed as a follow-up" claims are structurally false). This design replaces those seams with a mediated **proposal API**: a small worker-tier verb set on the existing `boss` CLI that submits typed, validated, idempotent proposal rows over the engine's existing Unix socket, plus **read-only access to the work taxonomy** so workers stop working blind. Writes stay mediated — proposals are durable rows the engine auto-applies where policy allows and routes to the coordinator/human where judgment is needed. The runtime half of worker isolation (dispatch state, transcripts, `bossctl`, sibling sessions) is unchanged.

## Goals

- Replace every parse-based worker→engine structured-text contract with a single reliable, synchronous, validated submission mechanism, so worker-reported facts (escalations, blockers, follow-ups, triage outcomes, deferred scope) are durable rows the moment the worker states them — not reconstructions scraped from prose after the fact.
- Give workers an immediate, typed error when a submission is malformed, instead of a silent parse failure discovered (or not) at run completion.
- Make every proposal attributable to a specific execution and rate-limitable, using verified identity rather than trusting worker-supplied strings.
- Expose the work taxonomy (products, projects, tasks/chores, statuses, dependency edges, PR bindings) to workers read-only, ending stale-brief confusion and duplicated effort from workers that cannot see sibling tasks or their own chore's state.
- Preserve the two established mediation invariants: proposals never auto-create tasks without an explicit human gesture (attentions.md), and inference is separated from application — the worker LLM never writes taxonomy rows directly (P783).
- Degrade loudly: a worker that cannot reach the mechanism produces a recorded, visible failure in the run outcome, never an unparseable prose fallback.

## Non-goals

- **Relaxing the runtime half of worker isolation.** Dispatch state, slots, traces, other workers' runs and transcripts, live-status, `bossctl`, and `~/Library/Application Support/Boss` remain off-limits. This project relaxes only the model-visibility half of the isolation policy (v2-design-risks R3's threat model — confused/misled/prompt-injected workers — still holds; the mitigations for it are unchanged).
- **Direct taxonomy writes from workers.** Workers do not gain `boss task create/update` authority; in fact this design _closes_ the existing gap where those verbs are technically callable from worker sessions (see "Worker RPC tier" below).
- **Replacing the design-question sibling manifest (`<slug>.attentions.json`) in v1.** That seam is content-coupled to the design doc, versioned and reviewed with the PR, and has not shown the failure modes of transcript scraping. It stays; migrating it is listed as future work.
- **Replacing the reviewer's `ReviewResult` artifact in v1.** The structured-output file artifact is already the primary channel there; only the transcript-scrape _fallback_ is on the eventual kill list. Migration to a proposal kind is future work.
- **Remote-worker authentication hardening.** Remote SSH workers cannot present a local socket peer pid; v1 scopes the proposal API to local workers and records the remote gap as an open question (token-based auth per R3 option B is the likely answer).
- **A general-purpose workflow/plugin system.** The proposal kind set is closed and engine-owned; adding a kind is an engine change, not a worker-side extension point.

## Current seams and why they are fragile

Every seam below is a text/JSON scrape of the worker's final message or transcript tail, each with its own hand-written recovery ladder (structured artifact → sentinel scrape → heuristic/LLM backstop → warn-and-guess). Inventory, with code anchors:

| #   | Seam                       | Worker emits                                                                                         | Engine parses                                                                                         | Failure evidence                                                                                                                                                                                                           |
| --- | -------------------------- | ---------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | Automation triage decision | `automation: task <id>` / `automation: skip — <reason>` as last line (`automation_triage.rs:62,157`) | `parse_triage_decision` (`automation_triage.rs:261`); recovery heuristic in `completion.rs:2478-2527` | **100% miss rate**: all 67 recent produced_task finalizations went through the marker-recovery WARN path; finalization rides on `find_most_recent_open_task_for_automation` guesswork (T2945)                              |
| 2   | Effort escalation          | `[effort-escalation] requested_level=… reason="…"` (`runner.rs:1891`)                                | `worker_escalation.rs:88` + malformed-marker protocol (`:144`)                                        | Free-text line in the final response; hand-written malformed-field handling; case/format drift risk                                                                                                                        |
| 3   | Blocked signal             | `[blocked] reason="…"` (`runner.rs:1903`)                                                            | `worker_escalation.rs:41,165`; flag-gated phrase-allowlist heuristic (`:121`)                         | Same class; heuristic backstop exists because markers get dropped                                                                                                                                                          |
| 4   | Follow-up proposals        | `FOLLOWUPS:` sentinel + fenced JSON array, or `$BOSS_STRUCTURED_OUTPUT` file (`runner.rs:2152`)      | `attentions_detector.rs:324,447,483`; flag-gated LLM backstop (`:734`)                                | "Filed as a follow-up" claims in PR bodies are structurally false — workers have no write path, so promised follow-ups silently never exist (T303/candidateImpact); the prompt now _forbids_ the phrase (`runner.rs:1946`) |
| 5   | Deferred scope             | `[deferred-scope] summary="…" reason="…"` (`runner.rs:1933`)                                         | `deferred_scope.rs:36,64`                                                                             | Same marker class as #2/#3; exists because of the phantom-follow-up incident                                                                                                                                               |
| 6   | Reviewer verdict           | `ReviewResult` JSON artifact, transcript fallback (`pr_review/render.rs:414`)                        | `pr_review/parsing.rs:71` (3-strategy scrape)                                                         | Transcript fallback is explicitly transitional (`completion.rs:2819`); no-result path re-prompts                                                                                                                           |
| 7   | Design questions           | Sibling `<slug>.attentions.json` manifest (`runner.rs:2124`)                                         | `attentions_detector.rs:131,289`                                                                      | Healthiest seam (file-based, PR-reviewed); kept in v1                                                                                                                                                                      |

Structural problems shared by #1–#5:

- **Parse-at-a-distance.** The contract is validated only at run completion, long after the worker could fix anything. A typo'd marker is indistinguishable from no marker.
- **Recovery heuristics become the real protocol.** The triage marker's 100% miss rate means the guess-based recovery path _is_ the finalization path; the documented contract is dead weight (T2945).
- **No attribution or idempotency.** A marker in a transcript can be duplicated by retries, mangled by truncation, or (for injected content the worker quotes) not authored by the worker at all.
- **Workers work blind.** Because they cannot read the taxonomy, workers cannot check whether a follow-up already exists (feeding the T2944 duplicate problem), see sibling tasks, or notice their brief is stale.

One more latent gap this design fixes: the worker-facing `boss` verbs currently execute at `RpcTier::User` — unconditionally allowed (`app.rs:781,1890`). The only things stopping a worker from `boss task update` today are prompt text and Claude-settings deny rules for `bossctl`/`boss engine start|stop` (`worker_setup.rs:622-641`). The mediation policy is currently enforced by vibes, not by the engine.

## Alternatives considered

### A. Double down on structured files parsed at Stop (extend `$BOSS_STRUCTURED_OUTPUT`)

Extend the existing structured-output artifact: workers write proposal JSON files to a known path; the engine ingests them at run completion. This is the T1414 direction (transcript sentinels → known files) taken to its conclusion.

Rejected as the primary mechanism because it keeps the two worst properties of the status quo: **deferred validation** (a malformed file is discovered at Stop, when the worker can no longer fix it — exactly the silent-parse-failure class this project exists to kill) and **no read access** (a file drop cannot answer "does this follow-up already exist?"). It also complicates idempotency (retried runs re-drop files) and attribution (any process in the workspace can write the file). File drops survive as nothing more than an implementation detail of one legacy fallback during migration.

### B. MCP server exposing proposal tools to the worker's Claude session

Run an MCP server (engine-hosted) exposing `propose_followup`, `get_context`, etc. as native tools. Attractive ergonomics — typed tool schemas, no shell quoting — and immediate validation.

Rejected for v1 because it couples the mechanism to the Claude driver at exactly the moment the agent-driver abstraction is trying to decouple Boss from Claude-specific capabilities (Copilot CLI is already a contemplated alternative backend). A CLI verb set works identically under any driver that can run shell commands, rides the existing `BossClient` Unix-socket transport and peer-pid auth, and is testable without a live agent. An MCP facade over the same RPC verbs is cheap to add later and is listed as future work.

### C. Grant workers direct (non-mediated) write access to a subset of taxonomy verbs

Let workers call `boss task create --kind followup` etc. directly, and lean on dedup + review to catch mistakes.

Rejected: it violates both standing invariants — attentions.md's "a proposed followup is never turned into a task without an explicit human gesture" and P783's infer/apply separation ("the LLM never writes rows directly") — and it re-opens the T2944 duplicate-work hole at a second ingress. The automation-duplicate-work investigation (2026-07-14) shows what un-gated creation does even when _one_ privileged path does it. Mediation is the point: the proposal row is where policy, dedup, and human judgment attach.

## Chosen approach

A **worker-tier proposal API** with three pillars: (1) a `worker_proposals` table of typed, durable proposal rows with explicit states; (2) a small verb set on the existing `boss` CLI — `boss propose <kind>` and `boss context` — served over the existing frontend Unix socket with engine-verified per-execution identity; (3) an engine-side apply pipeline that auto-applies low-judgment kinds and routes judgment-requiring kinds into the existing attention-group human gesture. Each legacy seam then migrates to proposals-first with its old parser demoted to a counted fallback, and the worker prompt directives are rewritten per seam.

### Transport and authn: the worker RPC tier

Workers keep using the `boss` CLI over `BossClient`'s Unix-socket connection to the engine frontend (`client/src/lib.rs`) — no new transport. What changes is authorization:

- **New `RpcTier::Worker` classification.** `authorize_rpc` (`app.rs:1890`) already walks peer-pid ancestry (via `LOCAL_PEERPID`/`SO_PEERCRED`, `events_socket.rs`) and the worker registry already knows every worker pane's shell pid (`worker_registry.registered_pids()`). A connection whose peer descends from a registered worker pid is classified **Worker** and resolved to its specific run/execution. This is _verified_ identity — the engine derives "who is proposing" from the socket peer, not from a worker-supplied flag.
- **Worker-tier verb policy.** Worker-classified connections may call: the read-only taxonomy verbs (below), `SubmitProposal`, and the already-sanctioned telemetry verbs (`conflicts record-producer`, `project set-design-doc` for design workers). Mutating taxonomy verbs (`CreateTask`, `UpdateTask`, …) and everything `AppOrBoss`/coordinator-shaped are **denied with a typed error** naming the proposal verb to use instead. This converts the mediation policy from prompt-enforced to engine-enforced, and closes the `RpcTier::User` gap.
- **`BOSS_RUN_ID` as cross-check, not credential.** The CLI sends the env-derived run id; the engine verifies it matches the peer-pid-resolved run and rejects on mismatch (misconfigured env, copy-pasted commands across panes).
- **Rate limits.** Per-execution caps enforced at submission (defaults: 32 proposals per run total, 8 per kind; typed `rate_limited` error). Caps are generous — the goal is runaway-loop protection and attribution, not scarcity.
- **Remote workers (SSH)** have no local peer pid; v1 rejects worker-tier verbs from non-local peers and remote runs keep the legacy seams until the remote-auth follow-up lands (open question below).

### CLI surface

```
boss propose effort-escalation --level large --reason "multi-subsystem race; brief didn't mention the engine/app boundary"
boss propose blocked --reason "bazel E0583 survives clean --expunge; need direction"
boss propose followup-task --name "…" --description-file d.md --effort small --work-kind chore --rationale "…"
boss propose deferred-scope --summary "…" --reason "…"
boss propose attention --title "…" --body-file b.md
boss propose automation-outcome --produced-task <task-id>        # triage: I created this task
boss propose automation-outcome --skip --reason "repo is clean"  # triage: nothing to do
boss propose --list                                              # own proposals + states, for idempotent resume

boss context                    # one-call sanitized bundle (see read-only access)
boss context --json
```

Submission is **synchronous**: the engine validates and persists before the CLI exits. Success prints the proposal id (`prp_…`) and current state; validation failure prints a typed, field-level error and exits non-zero — the worker fixes and retries _during the run_, which is the whole point. Long text fields take `--*-file` variants so worker shells never interpolate markdown through quoting.

**Idempotency:** the CLI auto-derives an idempotency key (execution id + kind + content hash), overridable with `--idempotency-key`. Resubmission returns the existing row (`already_submitted: true`) instead of erroring, so retried commands and resumed runs are safe. `(execution_id, idempotency_key)` is UNIQUE.

### Data model

New table `worker_proposals` (migration per `schema_init.rs` conventions: idempotent `migrate_*` appended to the chain; `planner_runs` at `migrations_b.rs:1542` is the staging precedent):

```sql
CREATE TABLE IF NOT EXISTS worker_proposals (
  id               TEXT PRIMARY KEY,             -- prp_<hexnanos>_<hexcounter> via next_id("prp")
  execution_id     TEXT NOT NULL REFERENCES work_executions(id),
  work_item_id     TEXT,                         -- derived from the execution at insert
  kind             TEXT NOT NULL,                -- closed enum, see below
  payload_json     TEXT NOT NULL,                -- typed per kind, schema-validated at submission
  idempotency_key  TEXT NOT NULL,
  state            TEXT NOT NULL DEFAULT 'proposed',
                   -- proposed | applied | rejected | superseded | expired
  decided_by       TEXT,                         -- policy | coordinator | human
  decision_reason  TEXT,
  applied_ref      TEXT,                         -- id of the row the apply produced (atn/atg/task/…)
  created_at       TEXT NOT NULL,
  decided_at       TEXT,
  UNIQUE (execution_id, idempotency_key)
);
```

Protocol types (`protocol/src/types/proposal.rs`): `WorkerProposal` (builder-pattern per repo convention — >5 fields), `ProposalKind`, per-kind payload structs, `ProposalState`. Kinds and payloads, v1:

| Kind                 | Payload                                                                                   | Apply policy                                                                                                                                                                                                                        |
| -------------------- | ----------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `attention`          | `{title, body_markdown, attention_kind?}`                                                 | **Auto-apply** → attention item/group (same rows detectors write today)                                                                                                                                                             |
| `effort_escalation`  | `{requested_level, reason}`                                                               | **Auto-apply** → worker-signal attention + auto-nudge pause (same as `file_worker_signal_attention`, `completion.rs:4857`)                                                                                                          |
| `blocked`            | `{reason}`                                                                                | **Auto-apply** → worker-signal attention + nudge pause                                                                                                                                                                              |
| `deferred_scope`     | `{summary, reason}`                                                                       | **Auto-apply** → durable audit line on the work item + attention (`completion.rs:4931` behavior)                                                                                                                                    |
| `followup_task`      | `{proposed_name, proposed_description, proposed_effort?, proposed_work_kind?, rationale}` | **Gated** → upserts a member into the existing `followup` attention group for the originating task; task creation still requires the human batch-accept gesture, which runs the T2944 dedup/scoring verdicts (`attention_merges`)   |
| `automation_outcome` | `{outcome: produced_task, task_id}` or `{outcome: skip, reason}`                          | **Auto-apply with provenance check**: `produced_task` validates the task exists and has matching `source_automation_id` (as `completion.rs:2414` does today); finalization then _reads the proposal row_ instead of parsing markers |

State semantics: `proposed` is the durable ingress state; `applied` records what the apply produced (`applied_ref`); `rejected` carries `decision_reason` (validation passes at submit, so rejection is a policy/human judgment, e.g. dedup verdict "already exists as T123"); `superseded` covers a newer proposal with the same idempotency scope replacing an undecided one (e.g. triage revises its outcome); `expired` covers proposals still undecided when their execution reaches a terminal state and the kind is only meaningful in-flight. Every state transition is visible to the worker via `boss propose --list`, and rejection reasons are part of the read surface — a worker told "duplicate of T123" can adjust instead of re-proposing.

Auto-apply kinds are applied **synchronously in the submission transaction** — for these, "proposal accepted" and "effect exists" are the same event, which is what makes the CLI's success output trustworthy. Gated kinds return `state: proposed` and the worker's contract is explicitly "proposed, pending review" — the prompt instructs workers to phrase PR-body references accordingly ("proposed follow-up prp\_…", never "filed").

### Read-only model access and the exposure boundary

The isolation policy splits into two halves; this project relaxes only the first:

**Exposed read-only (the model half):** products, projects, tasks/chores (all fields a coordinator sees except those below), statuses, dependency edges, PR bindings (`pr_url` columns and `FindWorkItemsByPr`), attention groups for the worker's own work item, and the worker's own proposals. Served by the existing read RPCs (`boss task list/show`, `boss project show`, …) now explicitly allowed at Worker tier, plus one new convenience verb:

- **`boss context`** returns a single sanitized bundle: the worker's own task + project + product, sibling tasks in the project (name, status, PR URL, dependency edges), edges touching the worker's own task, its chore's current state, and open attention groups on its work item. One call, one round trip, designed to be pasted into worker context cheaply. This is what kills stale-brief blindness — and it is also T2944 "Layer 0" context injection made self-serve: a worker can check for an existing task before proposing a duplicate.

**Off-limits (the runtime half), unchanged:** engine dispatch state, slots and capacity, live-status, traces, work _runs_ of other executions, transcript paths and transcripts, host/pid fields, `bossctl` verbs, engine config, the events socket protocol, and `~/Library/Application Support/Boss`. Sanitization is field-level where a row mixes halves: execution rows returned to workers have `transcript_path`, `host_id`, `remote_pid`, `shell_pid` stripped.

The worker preamble's "ask the coordinator for taxonomy context; do not query the DB yourself" instruction is deleted and replaced with "use `boss context` / read verbs; propose changes with `boss propose`".

### Failure semantics: degrade loudly

- **Malformed submission** → immediate typed error, worker fixes in-run. This is the primary win over parse-at-a-distance.
- **Mechanism unreachable** (socket gone, engine down, tier misclassification): the CLI exits non-zero with a distinctive error. The engine independently observes the failure — every worker Bash invocation flows through the hook event stream (`event-shim` → events socket), so a failed `boss propose` is detectable at completion. The completion path records `proposal_channel_error` on the run outcome and files an engine-side attention, so the degradation is _recorded_, not inferred from prose.
- **Bootstrap fallback:** the `[blocked]` marker (only) is retained indefinitely as the channel of last resort, precisely because it must work when the mechanism itself is broken. All other markers are removed at the end of migration. The prompt's existing prohibition on prose claims ("filed as a follow-up", "tracked separately") stays.
- **Legacy fallback during migration:** while a seam is in its dual-read phase, the old parser runs only when no proposal row exists for that seam, and every fallback hit increments a counter (`worker_proposals.fallback_hit{seam}` via the engine counter-metrics framework) and logs WARN. Fallback rates are the explicit exit criterion for removing each parser.

### Seam migration map

| Seam                                             | Replacement                                                                            | Legacy endgame                                                                                                                                                                                |
| ------------------------------------------------ | -------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Triage decision markers (#1)                     | `boss propose automation-outcome`; `finalize_automation_triage` reads the proposal row | Marker parse + `recover_skip_reason` + `find_most_recent_open_task_for_automation` heuristic demoted to counted fallback, then deleted; T2945's diagnosis becomes moot                        |
| `[effort-escalation]` (#2)                       | `boss propose effort-escalation`                                                       | Marker parser deleted after soak; malformed-marker protocol obsolete (validation is at submit)                                                                                                |
| `[blocked]` (#3)                                 | `boss propose blocked`                                                                 | Marker **kept** as bootstrap fallback only; phrase-allowlist heuristic deleted                                                                                                                |
| `FOLLOWUPS:` block / structured-output file (#4) | `boss propose followup-task` (one call per follow-up, during the run, not at the end)  | Sentinel scrape + LLM backstop deleted after soak; T2935's requirement 7 should be built on `followup_task` proposals instead of a new fenced-section parser — coordinate if that lands first |
| `[deferred-scope]` (#5)                          | `boss propose deferred-scope`                                                          | Marker parser deleted after soak                                                                                                                                                              |
| ReviewResult transcript fallback (#6)            | Future: `review_verdict` proposal kind; artifact stays primary meanwhile               | Not v1                                                                                                                                                                                        |
| `.attentions.json` manifest (#7)                 | Kept (content-coupled to the doc, PR-reviewed)                                         | Future candidate                                                                                                                                                                              |

Each migration follows the same recipe: add proposals-first read in the consumer → swap the prompt directive (`runner.rs`) to instruct the verb → count fallback hits → delete the parser once fallback is quiet. All flag-gated (`worker_proposals` master flag + per-seam flags), so rollback is a flag flip.

### Prompt updates

`runner.rs` directives are rewritten per seam as its migration lands: `worker_escalation_protocol_directive`, `deferred_scope_directive`, `followups_emission_block`, and the triage preamble/CLAUDE.md (`automation_triage.rs`) each swap marker instructions for the corresponding `boss propose` verb with a one-line example. A new shared preamble section documents the proposal mechanism once (verbs, idempotency, "fix and retry on validation error", the `[blocked]` bootstrap fallback) and `worker_setup.rs::render_claude_md` gains the read-access guidance (`boss context`). Prompt text shrinks: typed `--help` on the verbs replaces prose format specifications.

## Risks / open questions

- **Remote SSH workers have no peer pid.** They currently run the same seams; v1 leaves them on legacy parsers, which means legacy parsers cannot be fully deleted until remote auth lands (likely a per-run token minted at spawn, R3 option B — the leak-vector objection weakens when the token is per-execution and short-lived). Needs its own small design.
- **Peer-pid → run resolution robustness.** Ancestry walking has known edge cases (double-forked/reparented processes lose lineage to the pane shell). The `AppOrBoss` fallback logic already navigates this; Worker-tier classification must fail _closed for writes, open for reads_… or strictly closed? Current lean: strictly closed with the typed error naming the issue, since a misclassified worker still has the `[blocked]` fallback. Reviewer input welcome.
- **Tier enforcement could break existing coordinator tooling.** Any script that runs `boss task update` from _inside_ a worker-descended shell (e.g. a human debugging in a worker pane) gets newly denied. Judged acceptable — that's the mediation policy working — but worth a release note.
- **Synchronous auto-apply couples submission latency to apply cost.** Fine for the v1 kinds (row writes). If a future kind's apply is expensive, it should be an async `proposed → applied` transition instead; the state model already permits this.
- **Proposal spam / low-quality follow-ups.** Rate caps bound volume but not quality; the existing attention dedup/scoring verdicts are the quality gate for `followup_task`. If auto-applied `attention` proposals get noisy, the policy table can flip that kind to gated without schema change.
- **Overlap with planned project P383 (worker-follow-up-task-proposal-pipeline).** P383's scope ("workers emit structured proposals for follow-up tasks on Stop boundary; coordinator evaluates and decides whether to file; no silent worker-side filing") is subsumed by the `followup_task` kind here — with the improvement that submission happens in-run with immediate validation rather than at the Stop boundary. P383 should be folded into this project (or re-scoped to just the coordinator-evaluation UX) rather than built separately.
- **Does `automation_outcome` fully subsume the triage contract?** Triage today also creates the task itself (`boss task create --automation`, a sanctioned direct write). This design keeps that create direct (it is already provenance-checked and is the one place T2944's structural gate will attach) and mediates only the _outcome declaration_. An alternative — mediate creation itself through a proposal — was considered and deferred: it would couple this project to the T2944 gate design. Flagged for reviewer confirmation.

## Proposed implementation task breakdown

Entries are PR-sized, one worker, one session. Parallelism notes call out file overlap explicitly; `completion.rs` and `runner.rs` are the contended files across seam migrations, so those are serialized with forward-port obligations.

1. **Proposal protocol types + schema migration**
   - Scope: add `protocol/src/types/proposal.rs` (`WorkerProposal` with `bon::Builder`, `ProposalKind`, per-kind payload structs with serde schemas, `ProposalState`) and the `worker_proposals` table migration (idempotent `migrate_worker_proposals`, appended to `run_full_migration_chain`, template + chain-equivalence test updated). No engine behavior change.
   - Effort: `medium`. Dependencies: none.

2. **SubmitProposal RPC: validation, idempotency, attribution, rate caps**
   - Scope: new frontend verb in the engine — payload validation per kind with field-level typed errors, `(execution_id, idempotency_key)` upsert semantics returning `already_submitted`, peer-pid→run attribution with `BOSS_RUN_ID` cross-check, per-execution rate caps, and a `ListProposals` read verb scoped to own execution. Auto-apply pipeline NOT included (state stays `proposed` for all kinds in this PR).
   - Effort: `large`. Dependencies: 1.

3. **Worker RPC tier enforcement**
   - Scope: add `RpcTier::Worker` classification to `authorize_rpc` (peer descends from a registered worker pid → Worker, resolved to its run), define the worker-tier verb policy (reads + SubmitProposal/ListProposals + sanctioned telemetry allowed; mutating taxonomy verbs denied with typed errors), field-level sanitization of execution rows returned to workers. Flag-gated.
   - Effort: `large`. Dependencies: 2 (co-edits `app.rs` dispatch/authorize with 2 — sequenced after it; forward-port 2's changes preservingly).

4. **`boss propose` CLI verb set**
   - Scope: `boss propose <kind>` subcommands with `--*-file` variants, auto-derived idempotency keys, env-derived run id, typed-error rendering, `boss propose --list`. Pure CLI + client crate; no engine changes.
   - Effort: `medium`. Dependencies: 2. (Parallel with 3 and 5 — different subsystems, no file overlap.)

5. **Apply pipeline: auto-apply kinds**
   - Scope: policy table + synchronous appliers for `attention`, `effort_escalation`, `blocked`, `deferred_scope` — each writing the same rows today's detectors write (worker-signal attentions + nudge pause, deferred-scope audit line), stamping `applied`/`applied_ref`/`decided_by=policy` in the submission transaction.
   - Effort: `medium`. Dependencies: 2. (Parallel with 3 and 4.)

6. **Apply pipeline: gated `followup_task` + `automation_outcome`**
   - Scope: `followup_task` → upsert into the originating task's `followup` attention group (existing batch-accept gesture and dedup/scoring verdicts unchanged); `automation_outcome` → provenance-checked auto-apply (task exists + `source_automation_id` match) with `rejected` + reason on failure; `superseded` handling for revised triage outcomes; `expired` sweep for undecided proposals on terminal executions.
   - Effort: `medium`. Dependencies: 5 (extends the same policy/applier module).

7. **`boss context` read bundle**
   - Scope: new read RPC + CLI verb returning the sanitized one-call bundle (own task/project/product, sibling tasks with status/PR/edges, own chore state, open attention groups, own proposals); worker-tier allowed.
   - Effort: `medium`. Dependencies: 3. (Parallel with 4, 5, 6.)

8. **Seam migration: effort-escalation + blocked**
   - Scope: `detect_and_file_worker_signals` reads proposals-first with the marker parsers as counted fallback (fallback-hit counters via the metrics framework); prompt directive `worker_escalation_protocol_directive` rewritten to instruct the verbs, keeping `[blocked]` documented as bootstrap fallback only; per-seam flag.
   - Effort: `medium`. Dependencies: 4, 5.

9. **Seam migration: deferred-scope**
   - Scope: same recipe for `[deferred-scope]` — proposals-first in `detect_and_record_deferred_scope`, counted fallback, `deferred_scope_directive` rewritten.
   - Effort: `small`. Dependencies: 8 (functionally independent, but co-edits `completion.rs` + `runner.rs` with 8 — sequenced after; forward-port preservingly).

10. **Seam migration: follow-ups**
    - Scope: `reconcile_task_followups` reads `followup_task` proposals first; `FOLLOWUPS:` sentinel scrape and LLM backstop demoted to counted fallback; `followups_emission_block` rewritten to instruct `boss propose followup-task` during the run; PR-body phrasing guidance updated ("proposed follow-up prp\_…").
    - Effort: `medium`. Dependencies: 6, and sequenced after 9 (same `completion.rs`/`runner.rs` overlap; forward-port preservingly).

11. **Seam migration: automation triage outcome**
    - Scope: `finalize_automation_triage` reads the `automation_outcome` proposal row first; marker parsing + `recover_skip_reason` + open-task recovery heuristic demoted to counted fallback; triage preamble and triage CLAUDE.md rewritten to instruct the verb after `boss task create --automation`.
    - Effort: `medium`. Dependencies: 6, and sequenced after 10 (same file overlap; forward-port preservingly).

12. **Coordinator visibility: proposals listing + metrics wiring**
    - Scope: `bossctl work proposals list` (filter by state/kind/execution), proposal counters registered in the metrics framework (submissions by kind, validation failures, rate-limit hits, fallback hits per seam), and surfacing `proposal_channel_error` run outcomes.
    - Effort: `small`. Dependencies: 2. (Parallel with everything after 2; touches `bossctl`/metrics only.)

13. **Worker preamble consolidation**
    - Scope: shared preamble section documenting the proposal mechanism once (verbs, idempotency, fix-and-retry, bootstrap fallback), `render_claude_md` gains `boss context` guidance and drops "ask the coordinator for taxonomy context"; removes now-redundant per-seam prose left by 8–11.
    - Effort: `small`. Dependencies: 8, 9, 10, 11 (last `runner.rs`/`worker_setup.rs` pass; forward-port preservingly).

14. **Legacy parser removal sweep**
    - Scope: once per-seam fallback counters are quiet over a soak window (and remote-worker auth has landed or remote runs are explicitly kept on fallback), delete the marker parsers (#1, #2, #5), the phrase-allowlist heuristic, the `FOLLOWUPS:` scrape + LLM backstop, and their tests; `[blocked]` bootstrap fallback stays.
    - Effort: `small`. Dependencies: 8–11 + soak; gated on the remote-worker decision. `future / not a v1 blocker` if soak extends.

15. **Remote-worker proposal auth** — `future / not a v1 blocker`
    - Scope: per-run token minted at spawn for SSH workers (R3 option B revisited), so remote runs can use the proposal verbs and task 14 can complete for all workers.
    - Effort: `medium`. Dependencies: 2, 3.

16. **`review_verdict` proposal kind** — `future / not a v1 blocker`
    - Scope: migrate the reviewer's transcript-scrape fallback (artifact stays primary) onto a proposal kind.
    - Effort: `medium`. Dependencies: 6.

17. **MCP tool facade over proposal/context verbs** — `future / not a v1 blocker`
    - Scope: optional Claude-native tool surface wrapping the same RPCs, per driver-capability negotiation in the agent-driver abstraction.
    - Effort: `medium`. Dependencies: 4, 7.

18. **Design-question manifest migration** — `future / not a v1 blocker`
    - Scope: evaluate migrating `<slug>.attentions.json` onto `attention` proposals once the mechanism has soaked; explicitly deferred in v1 (see Non-goals).
    - Effort: `small`. Dependencies: 14.
