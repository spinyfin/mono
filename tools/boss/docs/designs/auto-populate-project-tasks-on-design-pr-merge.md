# Boss: Auto-Populate Project Tasks on Design-PR Merge

- **Status:** Shipped (PRs #1417, #1418, #1420, #1444, #1446, #1683, #1687, #1692, #1694, #1695, #1703; merged June–July 2026). This doc describes the system **as built**; places where implementation diverged from the original plan are called out inline.
- **Parent project:** P783 `auto-populate-project-tasks-on-design-pr-merge` (`proj_18b3f1d464bce660_71`); carries forward the mandate of the archived P6 "Planner agent".
- **Code home:** `engine/core/src/{planner,populator,materializer,doc_fetcher}.rs`, the `engine/planner-validation` and `claude_client` crates, `protocol/src/planner.rs` + `protocol/src/types/planner_run.rs`, the `boss project plan/release/unpopulate/plan-runs` CLI verbs, and the macOS planner surfaces (all under `tools/boss/`).

## Problem

Every Boss project has exactly one `kind = 'design'` task, auto-created at `ordinal = 0` when the project is created (`engine/core/src/work/revision_helpers.rs`). That task dispatches with execution kind `project_design`, the worker writes a design doc under `tools/boss/docs/designs/<slug>.md`, opens a normal PR, and the PR merges. The merge poller (`engine/core/src/merge_poller.rs`) notices the merge via `gh pr view` polling, calls `mark_chore_pr_merged` (`engine/core/src/work/pr_flow.rs`) which flips the design task to `done` and cascades dependents, and then — for `kind = 'design'` rows — calls `design_detector::on_design_pr_merged`, which scans the PR for the single `tools/boss/docs/designs/*.md` file and records it as the project's `design_doc_path` pointer.

Before this project, what happened next was entirely manual. The _real_ payoff of a design doc is the pile of implementation tasks it enumerates — almost every doc in `tools/boss/docs/designs/` ends with a "Proposed implementation task breakdown" / "Follow-up Implementation Chores" section. The human coordinator read that section by hand, inferred the task graph (names, descriptions, kinds, effort, and the dependency edges that let work proceed in parallel), and typed it into a sequence of `boss task create` / `boss task depend add` calls. P707, P757, and P754 were each populated this way in the week this design was written. The design author writes the work plan, a human reads it, a human retypes it.

This document describes how that loop was closed: when a project's design-task PR merges, the engine **automatically generates the project's implementation tasks** — with their dependency edges — by reading the merged design doc through a reusable LLM **Planner** (a "mini-coordinator"). The infer step (LLM reads prose, proposes a typed task graph) is cleanly separated from the apply step (the engine deterministically writes rows through the existing task-creation/dependency write paths), which is what makes the feature testable, idempotent, and safe.

### What was already built vs. what this project added

| Already implemented before this project                                                                            | This project added                                                       |
| ------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------ |
| Design-task PR merge detection (`merge_poller::mark_merged`)                                                       | A planner invocation hooked into that same merge path                    |
| `mark_chore_pr_merged` flips design → `done`                                                                       | An idempotent "populate this project once" gate                          |
| `on_design_pr_merged` sets `project.design_doc_path`                                                               | The Planner: doc-prose → typed task-graph proposal                       |
| `create_task` / `add_dependency` write paths + cycle detection                                                     | A deterministic materializer that applies a proposal through those paths |
| Direct Anthropic API substrate (`live_status.rs`, since extracted into the shared `claude_client` crate, PR #1702) | A durable planner-run audit trail + operator review/undo surface         |

## Reconciliation with existing work

### P6 — "Planner agent for project planning and task extraction" (`proj_18a2bb9a1f7068d8_4`)

P6's stated goal was to "enable automated project planning and task breakdown through a dedicated Planner agent that produces structured markdown plans and populates project task lists." **P6 is archived** — it was never built, and there is no planner agent in the codebase today. This design does not build a parallel planner; it _carries P6's mandate forward_ and makes the Planner the central reusable deliverable of this project. Concretely: the "mini-coordinator" this project requires **is** the P6 planner agent. The design-PR-merge trigger is its first consumer, not the planner itself — the planner is defined with a caller-agnostic contract (see _The Planner_) so that operator-invoked planning, replanning, and large-chore decomposition reuse it unchanged.

### `design-producing-tasks.md` — the manifest approach that was _not_ taken

The earlier [`design-producing-tasks`](design-producing-tasks.md) design proposed that the design worker emit a structured `<slug>.tasks.json` **manifest** alongside the doc, that the doc be pushed direct-to-`main` (no PR), and that an `ApproveDesign` RPC materialize the manifest on explicit operator approval (its Q8 chose option _(δ)_: worker-emitted manifest as primary, an extractor worker as fallback). **That path was never implemented.** The flow that actually shipped is simpler: design docs ship as ordinary PRs and the engine records a `design_doc_path` pointer on merge. There is no manifest, no `ApproveDesign`, no renderer-approve surface in the code.

This project deliberately picks the _extractor_ branch of that earlier taxonomy — `design-producing-tasks.md` Q8 option _(β)_, "spawn something that reads the doc and emits the structured breakdown" — and elevates it from a fallback into the first-class, reusable Planner. The reasons we read the merged prose rather than require a worker-emitted manifest:

- **The doc already ships as prose via PR.** Requiring the worker to _also_ maintain a hand-written `.tasks.json` re-introduces a second artifact to keep in sync with the prose, and a second thing for the worker to get wrong. GitHub is already the source of truth for the merged doc.
- **The breakdown is exactly what an LLM is good at extracting.** The section exists in prose in every real example; turning it into a typed graph is a bounded transform.
- **Reusability.** A manifest is bound to the design-authoring worker. A Planner that ingests any design doc + project context is reusable by operator commands and replanning, which is P6's whole point.

Where the two designs _do_ converge is the **apply** half: the typed task-graph proposal this design defines (local handles + `depends_on` edges, kind, effort) is structurally the same shape as `design-producing-tasks.md`'s manifest, and the materializer here is the deterministic, transactional, dedup-and-cycle-checked apply step that doc's Q8 sketched. If the manifest flow is ever revived, both can feed the same materializer.

## Goals

- When a `kind = 'design'` task's PR merges, automatically generate that project's implementation tasks — names, descriptions, kinds, effort levels, and the dependency edges between them — from the merged design doc, with no human retyping.
- Build the **Planner** as a first-class, reusable component with a typed input/output contract, usable beyond this trigger (operator "plan this project", replanning, decomposing a large chore).
- **Separate infer from apply.** The LLM produces a _structured_ proposal; a deterministic engine materializer applies it. The LLM never writes rows directly.
- **Fire exactly once per project.** A hard idempotency gate prevents duplicate tasks on re-runs, retries, or concurrent triggers.
- **Fail safe and leave a durable, inspectable record.** Because the operator cannot watch the Planner run, every invocation persists its input, raw output, validation result, and materialization result, and surfaces an attention item.
- **Never create a partial or broken graph.** Validation (schema, non-empty, cap, acyclicity) gates the apply; the apply is a single all-or-nothing transaction.
- Reuse the existing `boss task create` / `boss task depend add` write paths (their engine-internal `create_task` / `add_dependency` equivalents); do not invent a parallel creation path that bypasses their gates (including cycle detection).
- Encode the coordinator's effort-estimation heuristic and kind conventions in the Planner, and emit `[effort-classification]` audit lines per [`effort-and-model-estimation`](effort-and-model-estimation.md).

## Non-Goals

- **Replacing the human design-review step.** The PR review of the design doc itself is unchanged; auto-populate runs only _after_ the design PR has merged.
- **Mirroring the design doc into Boss.** The doc is fetched live from GitHub at the merged ref. Boss stores only the pointer (already implemented) plus the planner-run audit record. The doc is never copied into the DB.
- **A new task-creation write path.** Materialization goes through `create_task` / `add_dependency`. We do not duplicate their validation, dedup, or cycle-detection logic.
- **Editing the design doc.** The Planner reads; it never writes back to the doc or proposes doc edits.
- **Auto-populating non-design merges, or chores.** The trigger is scoped to `kind = 'design'` rows with a `project_id`. (Operator-invoked planning of an arbitrary project is a _separate caller_ of the same Planner, not this trigger.)
- **Cross-product / cross-repo task graphs.** All generated tasks land in the design task's project, in the same product. Cross-product edges are out of scope (tracked elsewhere).
- **Learned task estimation.** Effort follows the existing rules-based heuristic; an ML estimator is a separate future concern.
- **A bespoke review UI.** The review surface is a thin client over engine-owned state (an attention item + the existing kanban + a `release` action). The engine owns the trigger, materialization, idempotency, and audit; the UI renders them.

## Naming

- **Planner** — the reusable LLM mini-coordinator. Entry point `Planner::plan(api_key, &PlannerInput) -> (PlannerOutcome, DecompositionAudit)`. (This is the P6 "planner agent.")
- **Task-graph proposal** — the Planner's structured output: a list of proposed tasks each with a `handle` (proposal-local id), plus a set of dependency edges referencing tasks by handle.
- **Materializer** — the deterministic engine step that applies a validated proposal by calling `create_task` / `add_dependency` in one transaction.
- **Populator** — the auto-populate orchestration triggered on design-PR merge: idempotency gate → fetch doc → Planner → validate → Materializer → audit → surface. The Populator is _a_ caller of the Planner; it is not the Planner.
- **`planner_runs`** — the durable audit ledger; one row per Planner invocation, also serving as the per-project idempotency gate.
- **Staged task** — a generated task created with `autostart = false` so it exists and is graph-wired but does not dispatch a worker until an operator _releases_ it.

---

## Alternatives considered

### Alternative A — Engine-side convention parser (no LLM)

The engine reads the merged doc, finds the `## Proposed implementation task breakdown` (or similar) heading, parses the numbered list for task names/descriptions, and infers dependencies from "_Depends on T-N_" annotations.

**Rejected.** This is `design-producing-tasks.md` Q8 option _(α)_ and it hits the brittleness wall that doc already flagged. The five docs in `tools/boss/docs/designs/` that have a breakdown use at least three different heading texts ("Proposed implementation task breakdown", "Follow-up Implementation Chores", "Implementation Plan"), three different per-task layouts (bold-name + inline desc; `T-N — name (crate)` + paragraph; numbered with separate "Acceptance:" clauses), and three different dependency notations ("_Depends on T-6_", "_Depends on: T-1, T-2_", "Critical path: T-1 → T-2 → ..."). A parser would lag doc reality forever and silently misparse, which is the worst failure mode for something the operator can't watch. Effort estimation and kind inference also can't be done by a regex.

### Alternative B — Require a worker-emitted manifest (the `design-producing-tasks.md` path)

Make the design worker write a `<slug>.tasks.json` manifest alongside the doc; the engine applies it on merge.

**Rejected for this trigger** (discussed at length under _Reconciliation_). It re-introduces a second hand-maintained artifact, depends on every design worker remembering to emit it, and produces a component bound to the authoring worker rather than a reusable Planner. The merged _prose_ is the source of truth we already have. (The materializer, however, is shared with this approach should it be revived.)

### Alternative C — A headless interactive worker that runs the CLI itself

Spawn a normal Claude worker (like any other execution) whose prompt is "read the merged doc and run `boss task create` / `boss task depend add` to populate the project."

**Rejected.** This collapses infer and apply into one un-gated step. The worker writes rows as side effects with no structured proposal to validate first, no atomic transaction, no clean idempotency key, and no point at which the engine can reject a cyclic or over-large graph before damage is done. It is also the _least_ inspectable option — the operator can't see the interaction, and a half-finished worker run leaves a half-built graph. The whole value of separating "propose" from "apply" (testability, idempotency, no-partial-graph safety) is lost. An interactive worker is also slower and costlier than a single structured-output API call for what is fundamentally a prose-to-JSON transform.

### Alternative D (chosen) — Engine-internal LLM Planner with structured output + deterministic materializer

The engine fetches the merged doc live, calls the Planner (a direct Anthropic API call returning a schema-validated task-graph proposal — _infer_), validates the proposal, then materializes it through `create_task` / `add_dependency` in one transaction (_apply_). The Planner is reusable; the trigger is one of several callers. This is the rest of this document.

---

## Chosen approach

### Architecture overview

```
                          merge_poller::mark_merged  (kind == "design", PR merged)
                                        │
                                        ▼
                          ┌──────────────────────────┐
                          │  Populator (orchestrator) │
                          └──────────────────────────┘
   1. idempotency gate ───┤  claim planner_runs row for project_id (UNIQUE)
                          │      already populated / pre-seeded? → skip + log
   2. fetch doc live  ────┤  gh api /repos/.../contents/<path>?ref=<design_doc_branch>
   3. INFER (LLM)     ────┤  Planner::plan(PlannerInput) ──► PlannerOutput
   4. validate        ────┤  schema · non-empty · cap · acyclic · confidence
   5. APPLY (engine)  ────┤  Materializer: create_task + add_dependency  (1 txn)
   6. audit           ────┤  persist input/raw-output/validation/result → planner_runs
   7. surface         ────┤  attention item on project + kanban shows staged tasks
                                        │
                                        ▼
              tasks exist, graph-wired, autostart=false (staged)
              operator reviews → `release` → dispatch begins   (undo: delete batch)
```

The engine owns steps 1, 2, 4, 5, 6, 7. Step 3 (the Planner) is the only LLM step and is the reusable component. The UI is a thin client over the attention item and the staged tasks.

### 1. Trigger & idempotency

**Where it fires.** The hook is `merge_poller::mark_merged` (`engine/core/src/merge_poller.rs`), in the `kind == 'design'` block immediately after `on_design_pr_merged` has recorded the `design_doc_path` — exactly where planned. `mark_chore_pr_merged` has already idempotently flipped the design task to `done` (it returns `Ok(None)` if the row is already `done`/`archived`, so the merge poller never re-enters this block for an already-merged design — the first idempotency layer is free). One post-ship addition: the enqueue carries its own inner `kind == Design` guard, because the surrounding merge-poller block later grew to also handle design-_postmortem_ doc merges, which must not re-trigger populate.

The merge poller must not block on a multi-second LLM call, so `enqueue_from_merge` is synchronous and cheap: it `tokio::spawn`s `Populator::run` as a detached task (no job table). Populator configuration (API key, task cap, event publisher) is delivered via a process-wide `OnceLock<PopulatorConfig>` installed at engine-server startup (`app/server.rs`); if never installed (tests, non-server binaries), the enqueue is a deliberate no-op — this avoided threading config through dozens of poller call sites.

**Idempotency key = `project_id`.** A new `planner_runs` table (see _Durable audit trail_) carries a `UNIQUE` partial index ensuring at most one `outcome IN ('applied','staged')` row per `project_id`. The Populator's first action is to _claim_ the project by inserting a `planner_runs` row in state `running` with `project_id` as the conflict target:

- If the insert succeeds, this invocation owns the populate.
- If it conflicts (a prior `running`/`staged`/`applied` row exists), the Populator **skips** and logs. This makes concurrent triggers, poller restarts (the startup sweep re-runs `run_one_pass`), and manual retries all safe — exactly one populate per project. As built, this skip is **log-only**: the `skipped_already_populated` outcome constant exists in the protocol but is never persisted, because the claim conflict is detected before any row could be inserted. The audit ledger therefore has no record of suppressed duplicate triggers.

The original design promised that a crashed `running` row (engine died mid-populate) would be reclaimable after a TTL. **That reclaim was never built** (`claim_planner_run` is a bare `INSERT OR IGNORE` with no age check): an engine crash between claim and terminal-outcome write leaves a `running` row that blocks auto-populate for that project indefinitely. The escape hatch is `boss project unpopulate --run <id>`, which deletes the row (the handler doesn't check outcome, so it works on `running` rows); no tasks were committed, since the apply is transactional. PR #1687 flagged the TTL reclaim as a follow-up.

**Project already has implementation tasks (operator pre-seeded some).** Belt-and-suspenders beyond the `planner_runs` gate: before claiming, the Populator checks for any non-design task under the project (as built: **any** non-design kind via `WorkDb::list_project_task_briefs`, deliberately broader than the originally sketched `kind IN ('project_task','task')`). If any exist, the Populator **refuses (skips), records `skipped_pre_seeded`, and raises an attention item** rather than merging:

> _Auto-populate skipped: project already has tasks. Run `boss project plan <project> --force` to add the planner's tasks anyway (existing tasks are preserved by name dedup)._

Refuse-not-merge is the safe default because the Planner cannot reason about _why_ the operator pre-seeded — merging risks duplicates and contradictory dependency edges the operator didn't intend. The escape hatch (`--force`, which routes through the same Planner via the operator caller and relies on the Materializer's `(name, project_id)` dedup) lets a human opt into merge when they know it's right. `--force` bypasses **only** this pre-seeded refusal — it never bypasses the `planner_runs` claim.

### 2. The Planner (mini-coordinator) — the core reusable component

The Planner is a pure transform: design-doc prose + project/product context **in**, a typed task-graph proposal **out**. It performs no writes and has no knowledge of the trigger that invoked it.

#### Interface contract

```rust
// boss-protocol (`protocol/src/planner.rs`) — shared so every caller (and tests)
// speaks the same shape. Builder-equipped per repo convention
// (`#[derive(bon::Builder)]`, `#[builder(on(String, into))]`).

pub struct PlannerInput {
    pub design_doc: String,             // full merged doc content, fetched live
    pub design_doc_ref: DocRef,         // { repo_remote_url, git_ref, path } (provenance)
    pub project: ProjectContext,        // id, name, slug, description, goal
    pub product: ProductContext,        // id, slug, name, repo_remote_url
    pub existing_tasks: Vec<TaskBrief>, // id + name of tasks already in the project (dedup hint)
    pub max_tasks: usize,               // hard guardrail surfaced to the model
}

pub struct PlannerOutput {
    pub tasks: Vec<ProposedTask>,
    pub edges: Vec<ProposedEdge>,       // blocking dependency edges by handle
    pub merge_order_hints: Vec<ProposedMergeOrderHint>, // non-blocking ordering hints
                                        //   (added later by the merge-conflict-reduction design)
    pub confidence: Confidence,         // High | Medium | Low ("high"/"medium"/"low" on the wire)
    pub breakdown_found: bool,          // false ⇒ no task-breakdown section in the doc
    pub notes: String,                  // free-text rationale, persisted for the operator
    pub effort_audit: Vec<String>,      // one `[effort-classification] ...` line per task
}

pub struct ProposedTask {
    pub handle: String,                 // proposal-local id, e.g. "schema-migration"
    pub name: String,
    pub description: String,
    pub kind: TaskKind,                 // shared enum; schema restricts to project_task | investigation
    pub effort: EffortLevel,            // shared enum; schema restricts to trivial..large (never `max`)
    pub ordinal: i64,                   // soft ordering hint
}

pub struct ProposedEdge {
    pub dependent: String,              // handle of the task that is gated
    pub prerequisite: String,           // handle of the task that gates it
}

pub enum Confidence { High, Medium, Low }
```

This is a **typed, structured-output schema, not free-form prose the engine re-parses.** The Planner is forced to return exactly this shape (see _Execution model_), so the engine receives validated data, never markdown. Dependencies reference tasks by `handle`; the Materializer resolves handles to real task ids at apply time, mirroring the `external_id → id` resolution `design-producing-tasks.md` Q8 described.

Two as-built notes on the contract. First, `kind` and `effort` reuse the shared `TaskKind`/`EffortLevel` enums from `types.rs` rather than planner-local restricted enums, so the Rust types admit values the contract forbids; the project_task|investigation and never-`max` restrictions are enforced **only** by the structured-output JSON schema (`planner_output_schema()`), not by Rust-side validation — the materializer maps any unexpected kind to `project_task`. Second, the entry point is richer than the `Result<PlannerOutput>` originally sketched: `Planner::plan(api_key: Option<&str>, &PlannerInput) -> (PlannerOutcome, DecompositionAudit)`, where `PlannerOutcome` is a five-variant enum (`Success | NoApiKey | ApiError | Transport | InvalidOutput`) satisfying the typed-outcomes requirement, the API key is injected so the Planner stays config-free, and the `DecompositionAudit` records the oversize-task gate (a later addition from the effort/decomposition work). `ApplyResult.created` is `Vec<String>` (task ids; no `TaskId` newtype exists), and `ApplyResult` also gained `merge_order_edges_created`.

`breakdown_found = false` is a first-class signal, distinct from "found a breakdown but it was empty/garbage" — it lets the Populator no-op cleanly when the doc simply has no task list (a pure design-rationale doc), without treating it as an error.

#### Encodes coordinator policy

The Planner's system prompt encodes the policy a human coordinator applies by hand:

- **Effort heuristic.** The rules-based heuristic from [`effort-and-model-estimation`](effort-and-model-estimation.md) Q4 (rules 1–8, first match wins; emits `trivial | small | medium | large`, never `max`). For every proposed task the Planner emits an `[effort-classification]` audit line in the exact format the coordinator/app use (`engine` ... see `BossPaneModel.swift`):

  ```
  [effort-classification] level=`medium` matched-rule=`rule 4 (multi-subsystem hint)` reasons="names engine + protocol surfaces"
  ```

  These lines are persisted in `planner_runs.effort_audit` _and_ appended to each created task's description (separated by a blank line), exactly as a hand-filed task carries its classification today. The Materializer sets each task's `effort_level` from `ProposedTask.effort`; the dispatcher then picks model/effort per the existing mapping. The Planner never sets `model_override` (per that doc's Q3 — model is a property of the level).

- **Kind conventions.** Generated tasks are `project_task` by default (they belong to a project and map to one PR each, per `work-taxonomy.md`). A proposed item framed as research/audit ("investigate", "audit", "diagnose") is emitted as `kind = 'investigation'` and classified `large` by rule 2. The Planner never emits `kind = 'design'` (one design per project, already exists) or `kind = 'chore'` (chores are product-direct, not project-scoped).
- **Dependency edges maximize safe parallelism.** The Planner is instructed to add an edge only for a _true_ prerequisite (B cannot start until A lands — e.g. "protocol types" before "engine RPC handler"), and to leave independently-startable tasks unedged so they dispatch in parallel. This mirrors how P707/P757/P754 were wired (a schema/protocol task as a shared root, then a fan-out of independent consumers, then an integration task that depends on the fan-out). `ordinal` is a soft ordering hint only; real gating is the edge set — the same separation `work-dependencies.md` draws.

#### Execution model

- **Engine-internal direct API call, not a worker spawn.** As first shipped (PR #1446), the Planner _copied_ the `live_status.rs` substrate shape (own `reqwest` client, own endpoint constants, mirrored error enum) rather than sharing code; it was later re-platformed onto the shared **`claude_client` crate** (`tools/boss/claude_client`, extracted in PR #1702), which owns the transport, typed `ClaudeError`s, and a `RetryPolicy` that retries **only transient** errors (429/5xx/overloaded/transport) — fixing the original code's retry-everything behavior (it would retry a 401). A direct API call is the right tool because the Planner needs no filesystem and no tools — it is a prose-to-JSON transform — and an interactive worker can't return structured output without re-introducing a manifest/sentinel (Alternative C). The Planner itself lives at `engine/core/src/planner.rs`.
- **Structured output is enforced**, not requested: a single forced tool call (`tool_choice: {type: "tool", name: "emit_task_graph"}`) whose `input_schema` is `planner_output_schema()`. The engine deserializes directly into the Rust type; a deserialization failure is `InvalidOutput`, never a parse-and-hope. Two robustness layers were added beyond the original plan: `coerce_stringified_array_fields` repairs an array field the model occasionally emits as a JSON-encoded string, and an outer **validation-feedback retry loop** (`PLANNER_VALIDATION_ATTEMPTS = 2`) re-prompts the model with the concrete rejection reason when its output is schema-invalid or contains an oversize task.
- **Model selection.** Planning quality matters and the call is infrequent (once per project), so the Planner uses Opus, pinned as `PLANNER_MODEL = "claude-opus-4-8"` (the Messages API takes no aliases). The model is a single constant, tunable without a schema change. Bounds as shipped: `effort = "high"`, `max_tokens = 16_384`, wall-clock timeout 180 s, `PLANNER_ATTEMPTS = 2` on transient transport errors.
- **The doc is fetched live**, never mirrored: `gh api /repos/<owner>/<repo>/contents/<path>` with `Accept: application/vnd.github.raw`, via the shared `boss_github::contents::fetch_repo_file` helper — the fetch was _consolidated_ rather than duplicated: `attentions_detector`'s copy was extracted into `tools/boss/github/src/contents.rs` and both consumers now share it, driven for the Planner by `engine/core/src/doc_fetcher.rs`. One divergence from the plan: the fetch ref is the project's stored `design_doc_branch` (falling back to `"main"`), **not** the merged `base_ref_name` carried from the poller — the ref is not threaded through `PopulateContext`. GitHub remains the source of truth for the PR-shipped artifact.

#### Reusability

`Planner::plan` is caller-agnostic. Consumers, as built:

1. **The Populator** (this project) — on design-PR merge, stamped `caller = "merge_trigger"`.
2. **Operator command** `boss project plan <project> [--force] [--dry-run]` — "plan this project now", stamped `caller = "operator"`, building `PlannerInput` from the project's stored `design_doc_path` (fetched live). `--dry-run` runs infer + validate and prints the proposal _without_ materializing — it never claims the idempotency gate, and reports distinct preview outcomes (`preview_already_populated_as_<outcome>`, `preview_pre_seeded`, `preview_<terminal>`); `--dry-run --force` previews what a forced apply would do. `--force` bypasses the pre-seeded refusal only.
3. **Replanning** — as built, this is not a distinct caller: the doc-comments still advertise `caller = "replan"`, but nothing ever stamps it. Re-planning in practice is `unpopulate` (which clears the gate) followed by `plan`, or `plan --force`; the Materializer's dedup keeps it additive (existing tasks by `(name, project_id)` are skipped, new ones added), never destructive.
4. **Decompose a large chore** (future, not built) — same contract with a chore description in place of a design doc.

All consumers share one contract, one validation path, one Materializer, and one audit ledger — merge-trigger, operator, and preview paths were refactored onto a single `attempt_plan` helper in the Populator.

### 3. The deterministic materializer (apply)

`Materializer::apply(db: &WorkDb, project_id: &str, planner_run_id: &str, output: &PlannerOutput) -> Result<ApplyResult>` (`engine/core/src/materializer.rs`) runs in a single SQLite transaction and is the _only_ thing that writes rows:

1. **Resolve every edge's endpoints to known handles.** A blocking edge referencing an unknown handle is a validation failure (reject the whole proposal — see below); we never silently drop blocking edges.
2. **Check the graph for cycles** before any insert (`check_graph`, a Kahn topo-sort that reports the stuck set). Defense in depth runs three layers deep as built: the validation crate's DFS check, the materializer's Kahn check, and `would_create_cycle` (`engine/core/src/work_dependencies.rs`) at each edge insert — even if the first two missed a cycle, the edge insert refuses it and the transaction rolls back.
3. **For each task**, dedup by `(trim(name), project_id)` — including duplicate names _within_ the proposal: if a non-deleted task with that name already exists, skip it but still record its `handle → id` mapping so edges resolve. Otherwise insert via the in-transaction bodies of the standard write path (`insert_task_in_tx` / `insert_investigation_in_tx` — the outer `create_task` owns its own transaction, so the Materializer calls the shared bodies inside its single enclosing transaction) with `autostart: false`, `created_via: "engine_auto"`, description including the effort-audit line, and `force_duplicate = true` (the product-scoped 60-second recent-duplicate heuristic is deliberately bypassed; the Materializer's own `(name, project_id)` dedup is authoritative). The new task is tagged with the originating run via `UPDATE tasks SET planner_run_id = ?` in the same transaction (a nullable column added for this purpose, excluded from `map_task`'s standard SELECT).
4. **For each blocking edge**, insert via `add_dependency_edge_in_tx` (relation `"blocks"`), resolving handles via the map. `INSERT OR IGNORE` semantics make duplicate edges a no-op (re-apply safe); `edges_created` pre-checks existence so it counts only genuinely new edges. `merge_order_hints` become non-blocking `RELATION_MERGE_ORDER` edges; unlike blocking edges, an unresolved _hint_ handle is skipped with a warning rather than failing the transaction.
5. **Commit.** Any error in steps 1–4 rolls the whole transaction back — **no partial graph is ever created.** (The Materializer itself never touches `planner_runs`; the Populator records an apply error as `planner_failed` with `result_summary = "apply failed: …"` — there is no separate `failed` outcome.)
6. Return `ApplyResult { created, skipped, edges_created, merge_order_edges_created }` for the audit record and the operator summary.

Inserting through the shared in-transaction bodies means the materializer inherits the existing gates (same-product check, cycle detection, `INSERT OR IGNORE` edge dedup) for free, per the project constraint to not bypass the standard write paths.

### 4. Graceful failure & observability (first-class)

Because the operator **cannot watch the Planner run**, this is treated as a core requirement, not an afterthought. Every invocation either commits a complete graph or commits nothing, and always leaves a durable, inspectable record.

#### Validation of the structured proposal

Between infer and apply, the proposal is validated. The validation layer shipped as its own crate — `boss-engine-planner-validation` (`engine/planner-validation`), extracted per the repo's crates-over-modules convention — as `validate(&PlannerOutput, max_tasks) -> ValidationResult`, seven ordered short-circuit checks: no-breakdown → empty → cap → duplicate handle → unknown handle → cycle → valid. Any failure is **no-op-safe** — nothing is written:

- **Deserialization** into `PlannerOutput` must succeed (enforced by the structured-output call, in the Planner's call path — the validation crate sees an already-typed value).
- **`breakdown_found == false`** → clean no-op (not an error). Record `outcome = 'no_breakdown'`, raise an informational attention item ("Auto-populate: no task breakdown found").
- **Empty `tasks`** with `breakdown_found == true` → distinct `EmptyBreakdown` variant, recorded in the DB as `no_breakdown` with its own result summary ("breakdown section present but no tasks extracted"); no-op + attention item.
- **Task cap.** `tasks.len() > max_tasks` → **do not truncate.** Silent truncation must never read as success. Record `outcome = 'rejected_too_many'`, raise an attention item quoting the count, and stage nothing. The cap is inclusive: exactly-at-cap is valid.
- **Acyclicity** → a cyclic proposal is rejected whole (iterative three-color DFS, returning a representative cycle path); `outcome = 'rejected_cycle'` + attention item.
- **Unknown handle in an edge / duplicate handle** → typed variants (`RejectedUnknownHandle` / `RejectedDuplicateHandle`), rejected whole. As built, both are _recorded_ under the `rejected_cycle` outcome (attention item: "Auto-populate rejected: malformed task graph") — a bucketing the original outcome vocabulary didn't anticipate.
- **Confidence == Low** → still materialize (staged, see review checkpoint) with a distinct attention item ("review staged tasks (low confidence)"). Note: the escalation is **textual only** — `CreateAttentionItemInput` has no severity/prominence field, so no surface renders the low-confidence item more prominently.
- **Not validated in Rust:** `kind`/`effort` contract violations (`max` effort, `design`/`chore` kinds). Only the JSON schema constrains them; the materializer silently coerces an unexpected kind to `project_task`.

The Planner additionally runs the **decomposition gate** before validation proper: `detect_oversize_tasks` (also in the planner-validation crate) flags tasks that look too big for one PR, feeding the validation-retry loop; oversize proposals surviving the retry are accepted best-effort and recorded in the `DecompositionAudit`. This gate came from the effort/decomposition work, not this design.

#### Retries, fallbacks, and the fail-safe mode

- **Doc fetch fails** (GitHub 5xx, transient `gh` error, token issue): 3 attempts with a fixed 500 ms delay. On exhaustion → `outcome = 'fetch_failed'`, attention item, no-op. The `design_doc_path` pointer is unaffected, so a later `boss project plan P` can retry once GitHub is healthy. An unparseable or non-GitHub `repo_remote_url` fails immediately. Two additional `doc_missing`-family cases shipped beyond the plan: project has no `design_doc_path` pointer at all, and no resolvable repo remote (including the `design_doc_repo_remote_url` docs-site override).
- **Doc fetch 404** (file moved/renamed since merge): no retry; `outcome = 'doc_missing'` + attention item naming the path.
- **LLM call fails**: transient errors (429/5xx/overloaded/transport) retry once (`PLANNER_ATTEMPTS = 2` with 500 ms backoff); hard errors (e.g. 401) fail immediately; schema-invalid output gets its own separate feedback-retry loop (`PLANNER_VALIDATION_ATTEMPTS = 2`, re-prompting with the rejection reason). Exhaustion → fail safe (`outcome = 'planner_failed'` + attention item). `NoApiKey` is a distinct outcome — the feature degrades to "design pointer set, tasks not auto-created" with an attention item telling the operator to configure the key, exactly as `live_status` degrades.
- **The cardinal rule:** the only state-mutating step is the single materializer transaction. Every failure mode before commit leaves the project exactly as it was (design task `done`, pointer set, zero tasks created).

#### Durable audit trail

A new `planner_runs` table is the operator's after-the-fact window into an interaction they didn't witness. The schema shipped column-for-column as sketched (migration in `engine/core/src/work/migrations_b.rs`, plus one addition: a `planner_runs_project_idx (project_id, created_at)` index for list queries):

```sql
CREATE TABLE planner_runs (
  id              TEXT PRIMARY KEY,         -- run_<...>
  project_id      TEXT NOT NULL,
  product_id      TEXT NOT NULL,
  design_task_id  TEXT,
  caller          TEXT NOT NULL,            -- 'merge_trigger' | 'operator' | 'replan'
  doc_ref         TEXT,                     -- repo|ref|path the doc was fetched from
  model           TEXT,                     -- model slug used
  input_summary   TEXT,                     -- doc length, project/product, existing-task count
  raw_output      TEXT,                     -- the model's full structured JSON (verbatim)
  effort_audit    TEXT,                     -- the [effort-classification] lines
  notes           TEXT,                     -- planner's rationale
  outcome         TEXT NOT NULL,            -- running|staged|applied|no_breakdown|
                                            --   rejected_too_many|rejected_cycle|
                                            --   fetch_failed|doc_missing|planner_failed|
                                            --   skipped_pre_seeded|skipped_already_populated
  result_summary  TEXT,                     -- created/skipped task ids, edge count, errors
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);
-- One successful populate per project (idempotency gate):
CREATE UNIQUE INDEX planner_runs_one_per_project
  ON planner_runs(project_id)
  WHERE outcome IN ('running','staged','applied');
```

Storing `raw_output` verbatim means that even when the apply succeeds, an operator can later read exactly what the model proposed and why. The doc itself is _not_ stored (it lives in GitHub at `doc_ref`); only the planner's interaction with it is. `boss project plan-runs <project>` (and the `ListPlannerRuns` engine RPC for the app) expose these rows.

How the outcome vocabulary is actually used, as built:

- The Populator writes `doc_ref`/`model`/`input_summary` **before** the slow LLM call, and persists `raw_output` **before** validation — so the audit row is informative even if the process dies mid-call, stronger than the step ordering originally sketched.
- `applied` is never written by the trigger path: the Populator always terminates at `staged`, and `applied` is set by the operator **release** action. Its meaning shifted from "auto-applied" to "released"; the auto-dispatch sense is reserved.
- `skipped_already_populated` is defined but never persisted — a claim conflict is detected before any row can be inserted, so duplicate triggers leave no audit record (log-only).
- `rejected_cycle` also covers unknown-handle and duplicate-handle rejections (see validation).

#### Operator review checkpoint — recommendation

**As built (and as recommended): tasks are auto-created in a _staged_ (non-dispatching) state, an attention item is surfaced, and one operator `release` action begins dispatch. Undo is provided regardless.**

This is human-in-the-loop on the _irreversible/expensive_ step (spawning workers) but full-auto on the _tedious_ step (extract + create + wire). Concretely, the Materializer creates tasks with `autostart = false`, so they appear on the kanban, fully graph-wired, but the dispatcher's "first-incomplete-is-ready" chain (`executions_runs.rs`) does not promote them to `ready` — no worker spawns. The operator reviews on the kanban (and can read the planner's rationale via `planner_runs`), then runs `boss project release <project>` (or taps Release in the app) to let dispatch begin.

Release, as built (`handle_release_project` in `engine/core/src/app/planner_ops.rs`): it targets the project's latest live run, requires `outcome == 'staged'`, flips `autostart = true` per task through the generic `update_work_item_as_actor` write path (so the existing invalidation/reconcile/dispatch machinery takes over — no new dispatch trigger), then patches the run to `outcome = 'applied'`. Per-task flip failures are logged and skipped, not transactional.

Why staged rather than auto-create-and-dispatch:

- **The can't-see-it constraint cuts hard here.** The operator never witnessed the Planner. Auto-dispatching would spawn workers — opening PRs, consuming cube leases, possibly doing the _wrong_ work — before any human laid eyes on the plan. Undo _after_ workers have started is messy (live PRs, in-flight leases).
- **Staging keeps 100% of the automation value.** The retyping toil — read doc, infer graph, create rows, wire deps — is exactly what's eliminated. Review-then-release is a few seconds of operator time, not minutes of typing.
- **It reuses an existing field.** `autostart` already gates dispatch; no new task status or state machine is needed. (A dedicated `proposed` status was considered and rejected: it would touch every status-aware surface for no benefit `autostart = false` doesn't already give.)

Why not pure manual approval (don't create anything until approved): that re-introduces the manual step the project exists to remove, and there'd be nothing concrete to review.

**Undo / rollback (provided regardless of the above).** Every task in a populate carries the originating `planner_runs.id`. `boss project unpopulate <project> --run <id>` (and the app's Undo affordance) deletes exactly that batch — but only the still-untouched tasks: a task with zero executions is safe to delete; a task that has already dispatched is _reported, not deleted_ (the operator decides), and any DB-read failure errs toward preservation, so undo never destroys in-flight work. Beyond the original sketch, unpopulate also accepts `applied` (released) runs, not just staged ones, and tears down live workers for revisions cascade-deleted alongside a parent task. Clearing the idempotency gate is implemented as **deleting the entire `planner_runs` row** — which also destroys that run's audit record, a sharper trade than the "clears the idempotency row" originally described (see R6).

#### Bounding & guardrails

- **Task cap.** `max_tasks` (`DEFAULT_MAX_TASKS = 30`). Exceeding it rejects the whole proposal (no silent truncation, per above) and logs the count.
- **Cost/latency budget.** One bounded API call per project (`max_tokens = 16_384`, effort `high`, 180 s timeout). The doc fetch and LLM call have small fixed retry counts. The whole populate is one infrequent operation per project lifetime.
- **Circuit breaker against runaway creation.** The `planner_runs` UNIQUE-per-project gate _is_ the circuit breaker: a project can be populated at most once automatically, so no trigger storm or poller restart can multiply tasks. The coarse global rate limit (populates per engine per minute) that was planned as a backstop against a flood of simultaneous design merges **was never built** — each simultaneous merge spawns its own `tokio::spawn` + Opus call, bounded only per-project.
- **No silent drops.** Anything dropped or truncated — over-cap, low-confidence, skipped-because-pre-seeded — is logged and surfaced as an attention item. Silence never reads as success. (Known exception, noted above: a duplicate trigger suppressed by the claim gate is log-only, with no audit row.)

#### Surfacing

The operator learns it happened without watching, via:

- **An attention item** (kind `auto_populate`) whose title/body differ by outcome: "Auto-populate: review & release staged tasks", "Auto-populate skipped: project already has tasks", "Auto-populate: no task breakdown found", "Auto-populate rejected: too many tasks" / "malformed task graph", "Auto-populate failed: …" (doc-not-found, fetch, planner, and apply variants). This is the primary signal. As built it attaches to the **design task**, not the project — `CreateAttentionItemInput` only supports execution/work-item targets; there is no project-level attachment. Each item is also published live as an `AttentionItemCreated` event.
- **The kanban**, which shows the new staged tasks immediately (a `WorkItemsCreated` batch event on the product topic lets it refresh in one round-trip; shipped in PR #1692, closing a gap PR #1687 left).
- **`planner_runs`** for the full after-the-fact record (input summary, raw output, rationale, result).

The macOS surfaces (PR #1703): staged tasks get a "sparkle" badge on kanban cards (derived as `created_via == "engine_auto" && !autostart && status == "todo"`); a project-header affordance opens a popover with the latest run's outcome, result summary, and Release/Undo buttons (Undo behind a confirmation dialog); and a run inspector sheet lists all runs, expandable to the rationale, the `[effort-classification]` lines, and the verbatim `raw_output`. One approximation, flagged at ship time: the popover renders the `PlannerRun` row rather than fetching the `WorkAttentionItem` itself — the attention item appears only via the generic attentions surface.

### 5. Edge cases

| Case                                                                | Handling                                                                                                                                                                                    |
| ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **No task-breakdown section**                                       | Planner returns `breakdown_found = false` → clean no-op + informational attention item. Not an error.                                                                                       |
| **Ambiguous / low-confidence breakdown**                            | `confidence = Low` → still staged (never auto-dispatched), with a distinct low-confidence attention item flagging the uncertainty (textual only; attention items have no prominence field). |
| **Multi-design project** (shouldn't exist — one design per project) | The `planner_runs` UNIQUE-per-project gate makes the _first_ design merge populate and any subsequent one skip. Defensive, not relied upon.                                                 |
| **Design PR merged but doc fetch fails**                            | Bounded retries; on exhaustion `outcome = 'fetch_failed'` + attention item; no tasks created. `boss project plan P` retries later.                                                          |
| **Doc moved/deleted between merge and fetch**                       | 404 → `outcome = 'doc_missing'` + attention item naming the path; no-op.                                                                                                                    |
| **Planner proposes a cyclic graph**                                 | Rejected at validation (topo-sort) before any write; `add_dependency`'s `would_create_cycle` is the second line of defense; `outcome = 'rejected_cycle'` + attention item; nothing created. |
| **Planner proposes too many tasks**                                 | Over `max_tasks` → whole proposal rejected (no truncation); attention item quotes the count.                                                                                                |
| **Project pre-seeded with implementation tasks**                    | Refuse + attention item; `boss project plan P --force` opts into additive merge (dedup by `(name, project_id)`).                                                                            |
| **Concurrent triggers / poller restart**                            | First claim of the `planner_runs` row wins; others skip (log-only — no audit row is written for the suppressed trigger).                                                                    |
| **Engine crash mid-populate**                                       | Apply is transactional → nothing committed. The planned TTL reclaim of the stale `running` row was never built: the row blocks re-populate until deleted via `unpopulate --run <id>`.       |
| **No API key configured**                                           | `outcome = 'planner_failed'` (NoApiKey) + attention item to configure the key; feature degrades gracefully, pointer still set.                                                              |

---

## Risks / open questions

**R1 — Planner output quality.** The whole feature rests on the LLM producing a sensible graph from prose. Mitigation: the staged (non-dispatching) default means a bad plan costs a review, not wasted worker runs; `--dry-run` lets operators preview; the raw output is persisted for inspection; low confidence escalates the attention item. _Open question:_ what confidence threshold (if any) should force staging-with-warning vs. a no-op? Current proposal: never auto-discard on low confidence — always stage and flag, because a human reviews before release anyway.

**R2 — Effort/kind drift from the human heuristic.** The Planner encodes the Q4 rules in a prompt, so it approximates rather than executes the deterministic heuristic. Mitigation: every task carries its `[effort-classification]` line, so a reviewer sees the reasoning; the operator can edit effort before release — though as built only via the generic `boss task update --effort` (the macOS surface renders effort read-only, no edit affordance). _Open question:_ should the engine re-run the deterministic Rust heuristic (if/when it's extracted from the coordinator into engine code) over the Planner's task names as a cross-check, overriding the LLM's effort when they disagree?

**R3 — Auto-apply vs. human-in-the-loop.** Resolved in favor of _staged + release_, shipped that way. If operators find review-then-release too heavy in practice, the alternative (auto-dispatch with prominent summary + undo) is a one-field change (`autostart = true`), so the decision is reversible.

**R4 — Doc-prose variability.** Real docs vary in how they express the breakdown. The LLM is robust to this where Alternative A's parser was not, but a doc with an unusually-structured or buried breakdown could yield a thin plan. Mitigation: `breakdown_found` + confidence signals; the operator can always re-plan or hand-fill.

**R5 — Cost.** An Opus call per project is more expensive than Haiku-tier work, but it is once per project lifetime and replaces minutes of human coordinator time. _Resolved:_ shipped on Opus, pinned as `claude-opus-4-8`; the model remains a single tunable constant should downshifting prove viable.

**R6 — `planner_runs` as both audit and idempotency ledger.** Coupling the two means an audit-row cleanup could accidentally re-open the idempotency gate. Mitigation: the UNIQUE partial index is scoped to live outcomes (`running`/`staged`/`applied`); audit rows for terminal failures don't gate, and undo deliberately clears the gate. _The coupling did bite, in the direction predicted:_ as built, unpopulate clears the gate by **deleting the whole `planner_runs` row**, destroying that run's audit record along with it — and duplicate-trigger skips are never recorded at all (the claim conflict precedes any insert). _Open question (still open):_ should idempotency live in a dedicated `projects.tasks_populated_at` column instead, with `planner_runs` purely an append-only audit log?

**R7 — Relationship to a future revived manifest flow.** If `design-producing-tasks.md`'s worker-manifest path is ever built, two producers (manifest and Planner) could feed the Materializer. Mitigation: the Materializer is producer-agnostic (it takes a validated proposal), so this is additive, not conflicting; the idempotency gate ensures only one populates a given project.

**R8 — Effort heuristic lives in the coordinator prompt, not in code.** The Q4 rules are currently applied by the coordinator LLM, not a shared Rust function. The Planner re-encodes them in _its_ prompt, so there are now two prose copies of the same rules. _Open question (still open):_ extract the heuristic into a shared engine crate and have both the Planner prompt and any future deterministic check reference one source of truth.

### Known gaps (promised but not built)

Follow-up work the implementation deferred, called out inline above and collected here:

- **Stale-`running`-row TTL reclaim** — an engine crash mid-populate wedges the project's gate until a manual `unpopulate --run`. Flagged as a follow-up in PR #1687.
- **Global populate rate limit** — the per-project UNIQUE gate is the only circuit breaker; simultaneous design merges each spawn an unbounded Opus call.
- **Audit record for suppressed duplicate triggers** — `skipped_already_populated` is defined but never persisted.
- **Rust-side enforcement of the kind/effort contract** — schema-only today; the materializer coerces unexpected kinds to `project_task` silently.

---

## Implementation task breakdown (as shipped)

Each task mapped to one PR, all merged. The shared **contract** task was the root; the Planner and Materializer were built in parallel; the trigger integrated them; observability and operator surfaces layered on top.

1. **Protocol: Planner contract types** (`boss-protocol`) — **PR #1417.** `PlannerInput`, `PlannerOutput`, `ProposedTask`, `ProposedEdge`, `Confidence`, `ApplyResult`, and `planner_output_schema()`, in `protocol/src/planner.rs`. _Depends on: none._

2. **Engine: `planner_runs` table + migration** — **PR #1420.** Schema per _Durable audit trail_, the UNIQUE-per-project partial index, `WorkDb` accessors (claim/update/list, plus `PlannerRunPatch`, `live_planner_run_for_project`, `delete_planner_run`). _Depends on: none._

3. **Engine: the Planner** — **PR #1446.** `engine/core/src/planner.rs`: system prompt encoding the Q4 effort heuristic, kind conventions, parallelism-maximizing edge guidance, and `[effort-classification]` emission; forced-tool-call structured output; bounded model/effort/timeout; typed `PlannerOutcome`. Transport later re-platformed onto the `claude_client` crate (PR #1702). _Depends on: 1._

4. **Engine: live doc fetch** — **PR #1418.** `doc_fetcher.rs` over the shared `boss_github::contents::fetch_repo_file` (consolidated from `attentions_detector` rather than duplicated), bounded retries, 404 handling. _Depends on: none._

5. **Engine: deterministic Materializer** — **PR #1683.** `materializer.rs`: handle resolution, Kahn cycle reject, trimmed-`(name, project_id)` dedup, single-transaction inserts via the shared in-tx write-path bodies (`autostart = false`), `planner_run_id` tagging (column + migration landed here). _Depends on: 1, 2._

6. **Engine: validation layer** — **PR #1444.** Seven ordered no-op-safe checks; `breakdown_found` and confidence handling. Later extracted into the `boss-engine-planner-validation` crate, which also grew the oversize-task decomposition gate. _Depends on: 1._

7. **Engine: the Populator + trigger hook** — **PR #1687.** `populator.rs` + the `tokio::spawn` enqueue from `merge_poller::mark_merged`'s design block; idempotency claim → pre-seeded check → fetch → plan → validate → apply → audit → surface; `PopulatorSteps` injection trait for no-network tests. _Depends on: 2, 3, 4, 5, 6._

8. **Engine: attention-item + event surfacing** — **PR #1692.** Outcome-specific attention items published live (`AttentionItemCreated`) and the `WorkItemsCreated` batch event for the kanban. _Depends on: 7._

9. **CLI: operator entry points** (`boss-cli`) — **PR #1695.** `boss project plan <project> [--force] [--dry-run]`, `boss project release <project>`, `boss project unpopulate <project> --run <id>`, `boss project plan-runs <project>` — thin wrappers over four new engine RPCs (`PlanProject`, `ReleaseProject`, `UnpopulateProject`, `ListPlannerRuns` in `planner_ops.rs`). _Depends on: 3, 5, 6, 7._

10. **macOS app: review/release/undo surface** (`app-macos`) — **PR #1703.** Staged-task badge, planner-run popover with Release/Undo, and the run inspector (raw output + rationale). Thin client over the engine RPCs. _Depends on: 8, 9._

11. **Tests: end-to-end fixtures** — **PR #1694.** `engine/core/tests/planner_e2e_fixtures.rs`: fixture docs as ground truth (P707 verbatim; the planned P757/P754 fixtures were replaced by P783's own doc, a notification-dedup doc, and a pure-rationale no-breakdown doc), with the LLM step faked (`FakeSteps`) so the deterministic half is what's asserted; covers no-breakdown, cyclic, over-cap, pre-seeded, fetch-failure, doc-missing, no-api-key, and double-fire idempotency. _Depends on: 5, 6, 7._

_Critical path as executed:_ **1 → 3 → 7 → 9 → 10**, with 2/4/5/6 feeding 7 in parallel and 11 validating once 5–7 landed.

---

## References

- [`design-producing-tasks`](design-producing-tasks.md) — the manifest/`ApproveDesign` design that was not built; shares the apply-step shape.
- [`effort-and-model-estimation`](effort-and-model-estimation.md) — the effort heuristic (Q4 rules) and `[effort-classification]` audit-line format the Planner encodes.
- [`work-dependencies`](work-dependencies.md) — dependency-edge semantics, `would_create_cycle`, ordinal-vs-edge separation.
- [`work-taxonomy`](work-taxonomy.md) — task/chore/project_task/design kind conventions.
- [`project-design-doc-pointer`](project-design-doc-pointer.md) — the `design_doc_path` pointer this feature reads.
- Code anchors (as-built, all under `tools/boss/`): Planner `engine/core/src/planner.rs`; Populator `engine/core/src/populator.rs`; Materializer `engine/core/src/materializer.rs`; doc fetch `engine/core/src/doc_fetcher.rs` + `github/src/contents.rs`; validation crate `engine/planner-validation/`; ledger `engine/core/src/work/planner_runs.rs` (migration in `work/migrations_b.rs`); release/undo RPCs `engine/core/src/app/planner_ops.rs`; contract `protocol/src/planner.rs` + `protocol/src/types/planner_run.rs`; trigger `engine/core/src/merge_poller.rs` (`mark_merged`); Anthropic transport `claude_client/`; CLI `cli/src/main.rs`; macOS `app-macos/Sources/{Models+Planner,ChatViewModel+Planner}.swift` + `ContentView.swift`; tests `engine/core/tests/planner_e2e_fixtures.rs`.

---

_Parent project: P783 `auto-populate-project-tasks-on-design-pr-merge` (`proj_18b3f1d464bce660_71`). Carries forward the mandate of the archived P6 "Planner agent for project planning and task extraction."_
