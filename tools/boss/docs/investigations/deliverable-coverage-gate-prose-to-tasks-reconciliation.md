# Boss: Deliverable Coverage Gate — reconciling design prose commitments against task decomposition and closeout

- **Type:** Investigation / design proposal (investigation-writeup deliverable; proposes a mechanism, does not build it)
- **Date:** 2026-07-24
- **Execution:** `exec_18c52be7b1c05790_14e` (`investigation_implementation`)
- **Work item:** Planner: reconcile design prose commitments against task decomposition + closeout coverage gate
- **Worked example:** Trunk merge-queue integration (`proj_18c3dfc555074eb0_cbc` / P2951), design [`trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`](../designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md); confirmed miss filed as T3215.
- **Builds on:** [`auto-populate-project-tasks-on-design-pr-merge.md`](../designs/auto-populate-project-tasks-on-design-pr-merge.md) (the Planner / Populator / Materializer this design augments), [`0007-p545-distributed-execution-milestone-gap.md`](0007-p545-distributed-execution-milestone-gap.md) (the same class of gap, one level up: milestone accounting).
- **Status:** Proposal, pending operator review.

## TL;DR

The Planner decomposes a design into tracked tasks and does that job well. But a deliverable a design commits to only in **prose** — one that never becomes a numbered task line-item — has no lifecycle: there is no row to leave un-done, and nothing reconciles "what the design promised" against "what shipped." This proposal adds a **deliverable-coverage layer** on top of the existing Planner: extract each concrete deliverable from a design, require every one to map to ≥1 covering task at decompose time, force each to a **disposition** (`shipped` / `deferred-to-vN` / `dropped(reason)`), and re-audit dispositions against `main` before a project can close. The highest-leverage combo is the **decompose-time coverage check** plus the **closeout coverage gate** — reconcile prose→tasks up front, then gate closeout on coverage against the merged code. "Silently absent" stops being a reachable state.

## Problem

Every Boss project has exactly one `kind = 'design'` task. When its PR merges, the merge poller records `project.design_doc_path` and the **Populator** invokes the **Planner**, which reads the merged prose and proposes a typed task graph; a deterministic **Materializer** writes those tasks through `create_task` / `add_dependency` (see [`auto-populate-project-tasks-on-design-pr-merge.md`](../designs/auto-populate-project-tasks-on-design-pr-merge.md)). This is solid: the infer step is separated from the apply step, the apply is transactional and cycle-checked, and every run leaves a `planner_runs` audit row.

The failure mode this proposal targets is not in that pipeline. It is the **gap between the design's full prose commitments and the task list the Planner produces from it.** The Planner faithfully tracks the tasks it created. It does not track the deliverables it _didn't_ turn into tasks. A commitment that lives in a sentence of prose — "settable via the CLI **plus** the app's settings pane" — but never becomes its own line-item simply has no representation anywhere in the system. It cannot be marked un-done, because there is no row for it. It cannot be flagged at closeout, because closeout only sees tasks. It vanishes without leaving a hole.

This is the same shape as investigation [0007](0007-p545-distributed-execution-milestone-gap.md), one level up: there, a _milestone_ the plan promised was never what its phase was scoped to deliver, and the gap sat invisible for ~13 days because the ledger showed the phase `done`. Here, a _deliverable_ the design promised was never what any task was scoped to deliver. In both cases nothing was misrepresented and nothing was deleted — what went missing was the distinction between _what was promised_ and _what the tracked units actually covered_.

## Worked example: the two dropped Trunk UI controls

The Trunk merge-queue design committed, for two settings, to a "CLI verb **plus** app settings" pairing — the CLI half and a macOS-app control half, explicitly, in prose:

- **`merge_mechanism`** — "the design keeps that a config flip (`boss product set-merge-mechanism mono trunk_queue` …)" and, in the settings prose, settable _"plus the app's product settings."_
- **Trunk API token** — provisioned via `boss engine trunk set-token` _"or the app's settings pane."_

The decomposition produced **CLI-framed** tasks — P2951 task 2970 ("set-merge-mechanism CLI + read plumbing") and 2971 ("Trunk token provisioning + status") — which shipped the CLI half of each. Task 2979 ("Merging UI: Trunk queue chrome") shipped queue-state **display**, not a settings control. The **app-settings UI half of both settings was never its own task.** So both UI controls were never built — confirmed by a design-vs-`main` audit and filed as T3215.

Two aggravating details make this systemic rather than unlucky:

1. **The auto-enable-on-push-restriction path masked it.** The feature "worked" end-to-end without the manual controls, so their absence was never _felt_. A missing last-mile control that a self-heal path routes around is exactly the kind that stays missing.
2. **Nothing here was undisciplined.** The design author _was_ rigorous about deferral: webhooks, ETA, the priority picker, `trunk_target_branch`, multi-org tokens, and board-column promotion were all explicitly marked deferred/non-goal and correctly omitted from the task list. The gap is not "the author forgot to mark deferrals." The gap is that **prose commitments not lifted into the task list have no lifecycle at all** — neither the shipped ones nor, in principle, the deferred ones.

The lesson: the two dropped surfaces were **spec'd, non-deferred, and user-facing**, and they disappeared between prose and tasks with zero signal. A coverage check at decompose time would have flagged "the app-settings control for `merge_mechanism` maps to 0 tasks" before implementation started; a closeout audit would have caught it against `main` before the project went Done.

## Goal

Augment the Planner so a design's **full prose commitments** are reconciled against its **task decomposition**, and against what actually ships — without reinventing decomposition (it works) and without auto-generating implementation tasks from prose unreviewed (out of scope). Concretely:

1. **Deliverable extraction** — derive the concrete, verifiable deliverables from a design (every CLI verb, UI control, schema column, RPC, behavior, one-off repair), each linked to its design section.
2. **Decompose-time coverage check** — when a design is decomposed, verify every extracted deliverable maps to ≥1 covering task; flag uncovered deliverables _before_ implementation starts.
3. **Mandatory disposition** — every deliverable resolves to `shipped` / `deferred-to-vN` / `dropped(reason)`. "Silently absent" becomes unreachable.
4. **Closeout coverage gate (backstop)** — before a project goes Done, run the design-vs-`main` audit automatically; anything uncovered blocks closeout or auto-files a follow-up chore.
5. **Design ↔ PR provenance** — implementation PRs cite the design section they satisfy; a section with zero citing PRs is a coverage hole the gate surfaces.
6. **Flag the last-mile class** — tag deliverables by layer (schema/RPC/engine/CLI/UI) and require explicit sign-off per user-facing surface, since that is the systematically-dropped class.

## Non-goals

- **Reinventing the Planner's core decomposition.** It works; this is a layer on top, reusing the existing infer/apply split.
- **Auto-generating implementation tasks from prose without human review.** The coverage check _flags_ uncovered deliverables and can _propose_ covering tasks, but a human dispositions and releases them — consistent with the existing staged-task (`autostart = false`) + operator-`release` flow.
- **Mirroring the design doc into the DB.** As today, Boss stores a pointer and audit records; the doc stays on GitHub. The new deliverable rows reference design sections by anchor, not by copying prose.
- **A bespoke review UI.** Follow the auto-populate precedent: engine owns the state (deliverable rows, coverage verdicts, dispositions); the surface is an attention item + the existing kanban + thin actions.

## Background: the pipeline as it exists today

From [`auto-populate-project-tasks-on-design-pr-merge.md`](../designs/auto-populate-project-tasks-on-design-pr-merge.md) (as built), the units this proposal extends, with current anchors (line numbers drift):

- **Planner** — reusable LLM "mini-coordinator". `Planner::plan(api_key, &PlannerInput) -> (PlannerOutcome, DecompositionAudit)` (`engine/core/src/planner.rs:263`). Reads `PlannerInput { design_doc, design_doc_ref: DocRef, project, product, existing_tasks, max_tasks }` (`protocol/src/planner.rs:60`) and emits a **task-graph proposal** `PlannerOutput { tasks, edges, merge_order_hints, confidence, breakdown_found, notes, effort_audit }` (`:173`); each `ProposedTask { handle, name, description, kind, effort, ordinal }` (`:116`). It never writes rows.
- **Validation** — pure gate `planner-validation::validate(output, max_tasks)` (`engine/planner-validation/src/lib.rs:86`): `NoBreakdown` / `EmptyBreakdown` / `RejectedTooMany` / `RejectedDuplicateHandle` / `RejectedUnknownHandle` / `RejectedCycle`. A separate per-task **decomposition gate**, `detect_oversize_tasks` (`:423`), already counts a rough proxy for deliverables: `OVERSIZE_DELIVERABLE_CLAUSES = 2` (a task whose description packs >2 deliverable clauses is flagged oversized). That proxy is _within_ one task; this proposal adds the _across-the-design_ view the oversize gate cannot see.
- **Materializer** — deterministic engine step; `Materializer::apply(db, project_id, planner_run_id, output)` (`engine/core/src/materializer.rs:93`) in one all-or-nothing transaction, creating tasks `autostart = false` (staged), deduping by `(name, project_id)`, resolving handles → ids, wiring `blocks` edges with a `would_create_cycle` guard.
- **Populator** — orchestration triggered on design-PR merge: idempotency gate (`planner_runs`) → `fetch_doc` (the `engine/design-doc-fetcher` crate, via `populator.rs:141` — note there is no `doc_fetcher.rs`) → Planner → validate → Materializer → audit → surface an attention item (`engine/core/src/populator.rs:265`).
- **Trigger path** — merge poller `mark_merged` (`engine/core/src/merge_poller.rs`), only for `kind == Design` with a `project_id`: `on_design_pr_merged` records `project.design_doc_path` (`design_detector.rs:250`, called at `merge_poller.rs:3991`) → `populator::enqueue_from_merge(PopulateContext { project_id, product_id, design_task_id, pr_url })` (`merge_poller.rs:4014`).
- **Audit ledger** — `planner_runs`, one row per invocation, also the per-project idempotency gate via a UNIQUE partial index `planner_runs_one_per_project` over `outcome IN ('running','staged','applied')` (`protocol/src/types/planner_run.rs:150`, `engine/core/src/work/planner_runs.rs:72`).
- **Staged task** — generated `autostart = false`; graph-wired but does not dispatch until an operator releases it (the release path is RPC-side in `engine/core/src/app/planner_ops.rs:201`, flipping the run `staged → applied`; the CLI surface is `boss project plan / unpopulate / plan-runs`, `cli/src/main.rs:381` — there is no distinct `boss project release` arm).
- **`DecompositionAudit`** — today carries only `{ oversize_attempts, oversize_remaining }` (`planner.rs:232`); observational.

The key observation: the Planner already reads the whole doc in one pass and already produces an audit alongside the task graph. The extraction and coverage work proposed here is a **second output of that same pass** plus a **new closeout-time consumer** — not a new agent.

## Proposed mechanism

### 1. Deliverable extraction — hybrid (structured section + LLM), template-anchored

Two pure options and why the hybrid wins:

- **Structured-only** — require the design template to carry a machine-readable "Deliverables / Acceptance" section (a table or checklist). Precise and cheap to parse, but only as complete as the author made it; anything committed in body prose but omitted from the table is invisible again — reproducing the exact failure.
- **LLM-only** — extract deliverables from arbitrary prose. Catches body-only commitments, but is non-deterministic and prone to false positives (turning a "background: how it works today" sentence into a deliverable) and false negatives.

**Recommendation: hybrid.** The design template gains a required **Deliverables** section (see below) that the author fills in as the _authoritative spine_; the Planner _also_ extracts deliverables from the full prose and **diff**s its extraction against the authored section. Three outcomes:

- **Authored ∩ extracted** — high-confidence deliverables; proceed.
- **Authored − extracted** — author listed it, LLM didn't find a covering commitment in prose; surface as "listed but under-specified" (usually fine, flagged for author confirmation).
- **Extracted − authored** — the LLM found a prose commitment the author did **not** list. This is the class that dropped the Trunk UI controls. Each such candidate is surfaced to the operator as a _proposed deliverable requiring disposition_ — never silently added, never silently dropped.

False-positive handling: extracted-but-not-authored candidates are advisory until an operator (or the design author, in PR review) confirms or rejects each; a rejection is recorded (`dropped(reason="not a commitment — background prose")`) so it does not resurface on re-runs. This keeps the LLM's recall without letting its noise block work.

Every deliverable — however sourced — carries a **design-section anchor** (heading slug + optional line hint, matching how the repo already cites drifting anchors as "line numbers will drift").

**Design-template addition.** There is no design-doc template file in the repo today (the section conventions — `## Problem` / `## Goals` / `## Non-Goals` / `## Proposed implementation task breakdown` — are emergent, not scaffolded), so this both establishes a lightweight template convention and adds one required section: `## Deliverables`, one row per deliverable:

| Deliverable                                 | Layer  | Section   | Disposition (author's intent) |
| ------------------------------------------- | ------ | --------- | ----------------------------- |
| `boss product set-merge-mechanism` CLI verb | CLI    | Settings  | v1                            |
| `merge_mechanism` product settings control  | UI     | Settings  | v1                            |
| Trunk token: `boss engine trunk set-token`  | CLI    | Secrets   | v1                            |
| Trunk token: app settings-pane control      | UI     | Secrets   | v1                            |
| Webhook relay                               | engine | Non-goals | deferred-to-v2                |

The `Disposition` column is the author's _intent_, not the _outcome_ — the outcome is what the gate later verifies. Layers are `schema / rpc / engine / cli / ui` (extensible). The `ui` and `cli` rows are the last-mile class (§6).

### 2. Deliverable state model (schema)

There is **no existing acceptance/deliverable/requirement/coverage model** in the schema today — the only "deliverable" in the codebase is a runtime flag meaning "the PR is satisfied" (`completion.rs` `try_finalize_satisfied_deliverable_on_stop`), not a stored criterion. So this is a greenfield table, following the existing side-table precedent `project_property_audit` (`CREATE TABLE IF NOT EXISTS`, `engine/core/src/work/migrations_b.rs:102`) and the guarded-column helper `table_has_column(conn, table, col)` used throughout that file.

A new **`deliverables`** table (added via `CREATE TABLE IF NOT EXISTS` in `engine/core/src/work/migrations_b.rs`, alongside the guarded `ALTER TABLE` pattern used for column additions there), one row per extracted/authored deliverable, scoped to a project:

- `id`, `project_id`
- `title` — short description
- `layer` — `schema | rpc | engine | cli | ui` (nullable/`other`)
- `design_section` — anchor into the design doc (slug; the doc stays on GitHub)
- `source` — `authored | extracted | both` (provenance from §1's diff)
- `intended_disposition` — author's declared intent: `v1 | deferred-to-vN | dropped`
- `disposition` — **resolved** state: `shipped | deferred-to-vN | dropped`, plus `dropped_reason` / `deferred_target`
- `disposition_actor` / `disposition_at` — who resolved it and when (human vs. gate)
- `coverage_state` — computed: `covered | uncovered | partial` (from the task join below)
- `verify_state` — computed at closeout: `shipped | partial | missing` against `main`

Coverage is a **join**, not a column duplicated onto tasks: a `deliverable_tasks` link table (`deliverable_id`, `task_id`) records which tasks cover a deliverable (many-to-many — one task may cover several deliverables and vice versa). This is the machine-readable form of "what the design promised" ↔ "what the decomposition tracks." It reuses task rows as-is; no change to the `Task` struct shape or its builder is required (consistent with the builder-pattern convention — this is additive and lives in its own table).

Rationale for a side table over Task columns: deliverables outlive individual tasks (a deferred deliverable has no task yet; a dropped one never will), the relationship is many-to-many, and keeping it out of `Task` avoids touching every construction site — the exact reason the builder-pattern convention exists.

### 3. Decompose-time coverage check — the check that would have caught the Trunk miss

Fold this into the Populator's existing sequence, immediately after the Materializer applies the task graph:

```
idempotency gate → fetch doc → Planner (task graph + deliverable extraction)
  → validate → Materializer (write staged tasks)
  → [NEW] link deliverables ↔ tasks → coverage check → surface
```

For each extracted deliverable, the Planner proposes which of the just-materialized tasks cover it (it has both in hand in the same pass — the task graph it just produced and the deliverables it just extracted). The engine records the `deliverable_tasks` links and computes `coverage_state`:

- **`covered`** — ≥1 linked task. No action.
- **`uncovered`** — 0 linked tasks and `intended_disposition = v1`. **This is the flag.** For the Trunk example, "`merge_mechanism` app-settings control" and "Trunk token app settings-pane control" would each land here.
- **`partial`** — linked but the covering task's scope demonstrably omits the deliverable (e.g. a `ui` deliverable linked only to a display-only task like 2979). Heuristic, advisory; treated like `uncovered` for surfacing.

Every `uncovered`/`partial` deliverable becomes a line in the Populator's attention item: _"design promised X (§section); 0 covering tasks — disposition required."_ The operator resolves each by either (a) releasing/creating a covering task (moving it to `covered`), or (b) recording an explicit `deferred-to-vN` / `dropped(reason)` disposition. **The project's staged tasks stay un-released until every v1 deliverable is either covered or explicitly re-dispositioned** — this is the gate that converts "silently absent" into "operator must decide."

Semantics choice — **soft block at decompose time**: it does not hard-fail the merge or the Populator; it blocks _release_ (autostart) of the task graph until dispositions are clean. This matches the existing staged-task/`release` flow and keeps a human in the loop, per the non-goal on auto-generating tasks.

### 4. Mandatory disposition

Every deliverable row must reach a terminal `disposition` before the project can close. The three states and who may set them:

- **`shipped`** — set by the closeout gate (§4/§5) when it verifies the deliverable present in `main`; also settable by a human. Not human-only, because the point is machine verification.
- **`deferred-to-vN`** — human only. Records `deferred_target` (e.g. `v2`, a successor project id). This is the state the Trunk design _already used well_ for webhooks/ETA/priority — the change is making it a **row with a target**, not just prose in a Non-goals section, so it is machine-checkable and traceable to the follow-up.
- **`dropped(reason)`** — human only. Requires a non-empty `dropped_reason`. Distinguishes "decided not to build, on purpose" from "fell through the cracks."

The invariant: **no deliverable may be in `uncovered` + `intended_disposition = v1` at closeout.** Either it shipped, or a human deferred/dropped it on the record. There is no path to Done that leaves a v1 commitment unaccounted for.

### 5. Closeout coverage gate (backstop) — design-vs-`main` audit

The decompose-time check catches gaps _before_ work starts; the closeout gate is the backstop that catches everything the front gate missed (a deliverable that was covered by a task the worker under-delivered, like display-only 2979).

**Where to hook it.** There is no automatic "close project when all tasks are done" path today — a project reaches Done only via a manual `boss project move --to done` (`ProjectStatus::Done`, `protocol/src/types/project.rs:44`; write applied in `engine/core/src/work/updates.rs:105`). The one _automated_ closeout-review substrate that already exists is the **project-postmortem sweep** (`engine/core/src/project_postmortem_sweep.rs`), a periodic reconciler that auto-creates a `design_postmortem` task when a project has completions since its last postmortem. Two viable hook points, and the recommendation uses both: (a) intercept the manual `→ Done` transition in `updates.rs` to run the audit as a **precondition** (this is where the hold/gate lives); (b) additionally run the audit inside the postmortem sweep so coverage is checked on the engine's own cadence even before anyone clicks Done — the sweep already knows the project's completions and is the natural home for "what did this project actually ship?" reconciliation.

The audit runs one verification per deliverable, computing `verify_state`:

- **`shipped`** — evidence found in `main` (see verification strategy below).
- **`partial`** — some but not all of the deliverable's expected surface is present (e.g. the CLI verb exists, the UI control does not — exactly the Trunk shape).
- **`missing`** — no evidence in `main`, and disposition is not `deferred`/`dropped`.

Gate semantics — **recommend auto-file over hard block**, with a hard block as the escape hatch:

- Any `missing`/`partial` deliverable whose disposition is `shipped`/`v1` → the engine **auto-files a follow-up chore** ("close deliverable gap: X, §section") linked to the deliverable, and moves the deliverable's coverage back to `uncovered`. Closeout is **held** (not permanently blocked) with an attention item until each such gap is either resolved (chore ships it) or re-dispositioned (`deferred`/`dropped`). This mirrors the CI-remediation substrate's "spawn a follow-up, surface an attention item, don't silently pass" posture rather than a dead-stop.
- The operator may still hard-block-then-override by explicitly dispositioning: a human deferring or dropping a `missing` deliverable clears the hold. That decision is human-owned, per the repo's hard rule that relaxing a gate is a human call, never autonomous.

Who dispositions: the closeout gate never _drops_ on its own; it can only mark `shipped` (verified) or open a chore + hold. Deferral/drop stays human.

**Verification strategy — layered, provenance-first, presence-second.** The audit must verify "shipped" against `main` without being brittle:

1. **PR provenance (primary).** From §6, implementation PRs cite the design section they satisfy. The gate maps `deliverable.design_section` → citing PRs → the tasks/commits that merged them. A deliverable whose section has ≥1 citing merged PR touching the expected layer is strong evidence. A section with **zero** citing PRs is a coverage hole surfaced directly — this alone would have flagged the two Trunk UI sections.
2. **Symbol/file presence (secondary, layer-aware).** Cheap, targeted existence checks keyed on layer, not full semantic proof:
   - `cli` — the verb string appears in the CLI command table / arg parser.
   - `ui` — a control (not just a display component) referencing the setting exists in the app sources — the check that separates 2979's _display_ from an actual _settings control_.
   - `schema` — the column appears in a migration and the type struct.
   - `rpc` — the request/response variant exists in the wire protocol.
   - `engine` — the named behavior's entry point exists.
     The check reports evidence, not a boolean verdict; it feeds the LLM auditor rather than gating directly, to avoid brittleness.
3. **LLM adjudication (tie-break).** Where provenance + presence disagree or are ambiguous, an LLM auditor reads the section prose + the presence evidence and classifies `shipped/partial/missing` with a short justification, recorded on the deliverable. This is the same "read prose, judge against code" transform the Planner already does, reused.

Brittleness guard: presence checks never _auto-drop_ a deliverable; the worst they do is open a chore + hold closeout for a human. A false "missing" costs one attention item, not lost work — the asymmetry we want.

### 6. Design ↔ PR provenance and the last-mile class

**Provenance.** Today the _only_ stored PR↔work linkage is `tasks.pr_url` (`protocol/src/types/task.rs:635`, captured from worker bash output by `engine/core/src/pr_url_capture.rs`); "cite the design section" exists purely as prose convention in a few docs (e.g. `macos-modernization-audit.md:349` "the chores intentionally cite each finding's section number"; a "Citation" column in `test-instance-isolation.md:11`; the supersedes-citation rule in `runner.rs:2962`) with no data model behind it. This proposal makes it structured: require implementation PRs to cite the design section(s) they satisfy — a `Satisfies: <design-slug>#<section-anchor>` line (or the deliverable id) in the PR body, parsed by the same machinery that already scrapes `pr_url`. The gate consumes these citations two ways: forward (section → PRs, for §5 verification) and backward (deliverable → is any PR claiming it?). **A design section with zero citing PRs is, by construction, a coverage hole** — the cheapest possible detector for "this was never built," and one that needs no source parsing at all.

**Last-mile class.** User-facing CLI/UI surfaces are the systematically-dropped class, and _most_ dangerous when a self-heal/auto path masks the gap (the Trunk auto-enable path). Two mitigations, both riding the `layer` tag:

- **Per-surface sign-off.** Every `cli` and `ui` deliverable requires an explicit human `shipped` sign-off at closeout even if presence checks pass — the layers where "it compiles / a component exists" most diverges from "the user can actually do the thing."
- **Masking flag.** When a deliverable's design section also describes an automatic/self-heal path for the same capability (the Planner can detect this in prose), tag the deliverable `masking_risk = true` and escalate its surfacing: these are precisely the ones whose absence is never _felt_ at runtime, so they must be verified explicitly rather than assumed-working.

## How it fits the existing lifecycle

Two hooks, both into paths that already exist — no new agent, no parallel pipeline:

1. **Decompose time** — the Populator's post-Materializer step (`populator.rs:265`, §3) gains deliverable extraction + linking + the coverage check. It reuses the same Planner pass (deliverables are a second output alongside the task graph) and the same "surface an attention item, stage-don't-autostart" posture.
2. **Closeout time** — the manual `→ Done` transition (`updates.rs:105`) gains the coverage gate as a precondition, and the existing `project_postmortem_sweep` reconciler additionally runs the audit on the engine's cadence (§5) — both reusing the merge/PR knowledge the engine already has (it tracks the project's tasks and their `pr_url`s) plus the new citation provenance.

The `deliverables` + `deliverable_tasks` tables are additive migrations. CLI surface extends the existing `boss project` verb family: `boss project deliverables` (list + coverage/verify state), `boss project disposition <deliverable-id> <shipped|deferred:vN|dropped:"reason">`, and the closeout verb gains a `--audit` that runs §5 on demand. The kanban closeout path gains a deliverable-coverage summary on the project card, rendered from engine state (thin client, per the auto-populate precedent).

## Design questions resolved

- **Where does deliverable state live?** A dedicated `deliverables` table + `deliverable_tasks` link table (§2), _not_ columns on `Task`. Reasons: many-to-many, deliverables outlive tasks, and keeping `Task` untouched respects the builder-pattern convention.
- **Extraction: structured vs LLM vs hybrid?** Hybrid (§1). Authored `## Deliverables` section as the authoritative spine; LLM extraction over full prose as the recall net; the _diff_ between them is where dropped commitments surface. False positives are advisory-until-confirmed and remembered when rejected.
- **Gate semantics: hard block vs auto-file?** Decompose-time: soft block on _release_ (stage, don't autostart). Closeout-time: auto-file a follow-up chore + **hold** (not permanent block); human override via explicit disposition. Never an autonomous relaxation of the gate (repo hard rule).
- **Who dispositions deferred/dropped?** Humans only. The gate may set `shipped` (verified) autonomously; `deferred-to-vN` and `dropped(reason)` are human-owned and recorded with actor + reason/target.
- **How does the audit verify "shipped" without brittleness?** Layered (§5): PR-section provenance first (zero-citation sections are free coverage holes), layer-aware symbol/file presence as evidence second, LLM adjudication for ties. Presence checks never auto-drop; the worst outcome is a chore + a human decision.
- **Interaction with existing lifecycle / kanban closeout?** Two additive hooks (Populator post-Materializer; project→Done). Reuses staged-task/`release`, the attention-item surface, the CI-remediation-style "spawn follow-up + surface" posture, and the engine's existing task/PR knowledge.

## Rollout plan

Phased, each phase independently shippable and useful:

1. **Schema + provenance (foundation).** Add `deliverables` / `deliverable_tasks` migrations and the `Satisfies:` PR-citation convention (documented + a lint that warns, not blocks). No behavior change yet; establishes the data spine. _(This alone makes zero-citation sections detectable.)_
2. **Template + extraction.** Add the `## Deliverables` design-template section; extend the Planner pass to extract deliverables and record the authored-vs-extracted diff into the table. Surface only (no gating). Backfill the Trunk design as the validation fixture — the audit should reproduce T3215 from the doc + `main`.
3. **Decompose-time coverage check.** Wire §3 into the Populator: link deliverables↔tasks, compute `coverage_state`, soft-block release on uncovered v1 deliverables. This is the check that would have caught the Trunk miss up front.
4. **Closeout coverage gate.** Wire §5 into project→Done: design-vs-`main` audit, auto-file chores + hold, layer-aware verification, last-mile sign-off. Backstop complete.
5. **Last-mile hardening.** `masking_risk` detection and per-surface sign-off enforcement (§6).

Phases 1–2 are pure additions with an operator-visible payoff (the coverage summary) before any gate can block or hold work, which de-risks the behavioral phases.

## Alternatives considered

- **Manifest-first (worker emits `<slug>.deliverables.json`).** Symmetric to the `design-producing-tasks.md` manifest path the auto-populate design deliberately did _not_ take, for the same reasons: a second hand-maintained artifact drifts from the prose, and it is bound to the authoring worker rather than reusable. The hybrid keeps the authored section as _intent_ but does not trust it as _completeness_ — the LLM diff is what closes the gap the manifest approach would reopen.
- **Hard block at both gates.** Rejected in favor of soft-block-release (decompose) + auto-file-and-hold (closeout). Hard-stopping the merge poller or the Done transition on a heuristic coverage verdict would make a false negative expensive and invites autonomous gate-relaxation pressure — which the repo forbids. Auto-file + hold keeps the failure cheap and the override human.
- **Coverage columns on `Task`.** Rejected: many-to-many relationship, deliverables that outlive/predate tasks, and the builder-pattern convention's whole point (don't touch every construction site) all argue for a side table.
- **Pure closeout audit, no decompose-time check.** Would still catch the Trunk miss — but only _after_ the whole project is built, when re-doing the two UI controls is a follow-up project rather than a line item in the original decomposition. The decompose-time check moves the catch left, which is where the leverage is. Both together is the recommendation.

## Follow-up code changes to file separately

This investigation proposes; it does not build. Concrete follow-ups for the operator to file as their own work items:

1. **Schema + provenance foundation** (rollout phase 1): `deliverables` / `deliverable_tasks` migrations in `work/migrations_b.rs`; the `Satisfies:` PR-citation convention + a warn-level lint; a `boss project deliverables` read verb.
2. **Deliverable extraction in the Planner** (phase 2): extend the Planner pass to emit deliverables alongside the task graph, record the authored-vs-extracted diff; add the `## Deliverables` design-template section.
3. **Decompose-time coverage check** (phase 3): Populator post-Materializer linking + `coverage_state` + soft-block-on-release; `boss project disposition` verb.
4. **Closeout coverage gate** (phase 4): project→Done audit hook, layer-aware verification (provenance + presence + LLM adjudication), auto-file-chore + hold, kanban coverage summary.
5. **Last-mile hardening** (phase 5): `masking_risk` detection, per-surface (cli/ui) sign-off enforcement.
6. **Validation fixture:** replay the Trunk merge-queue design against `main` and assert the audit reproduces the T3215 finding (both dropped UI controls surface as `missing`/`partial`).

## Open Questions

- **Decompose-time extraction cost/latency.** Extraction is a second output of the existing Planner pass, so no extra LLM round-trip in principle — but does the combined prompt degrade task-graph quality? Needs a measured comparison on real designs before defaulting it on.
- **Section-anchor stability.** Deliverables cite design sections by heading slug; docs get edited post-merge. Do we pin to the merged ref (like the doc fetch already does) and accept staleness, or re-resolve anchors on each audit? Leaning: pin to merged ref, re-extract only on an explicit replan.
- **`partial` heuristic precision.** Distinguishing a display-only task (2979) from a real settings control is the crux of catching the Trunk-class miss, and it is the least mechanical part of §5. How much can layer-aware presence + LLM adjudication carry before per-surface human sign-off (§6) is doing the real work? Phase 2's Trunk fixture should measure this directly.
- **Scope of the closeout gate.** Applies cleanly to `kind='design'` projects with a `design_doc_path`. What about chores/tasks with no design, or projects whose design predates the `## Deliverables` template? Proposal: gate applies only where a deliverable set exists; legacy projects are grandfathered (no deliverables → no gate) rather than retro-audited, unless an operator opts one in.
- **Interaction with multiple designs per project / stacked designs.** The worked example is one design → one project. Cross-design deliverable sets (a project that adopts commitments from a related design) are out of scope here but may need the `design_section` anchor to carry a doc slug, not just a heading — the schema already allows it (`design_section` is a full anchor), but the extraction and gate would need to know which docs are in scope.
