<!-- Ground-truth fixture for planner e2e tests (design task 11).
     Verbatim excerpt of the "Proposed implementation task breakdown" section of
     tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md (project P783).
     Do not edit by hand except to re-sync with the source doc. -->

## Proposed implementation task breakdown

Bite-sized; each maps to roughly one PR. Dependencies are listed explicitly; the Planner-generated version of _this_ project would wire these same edges. The shared **contract** task is the root; the Planner and Materializer can then be built in parallel; the trigger integrates them; observability and operator surfaces layer on top.

1. **Protocol: Planner contract types** (`boss-protocol`). Add `PlannerInput`, `PlannerOutput`, `ProposedTask`, `ProposedEdge`, `Confidence`, and the `ApplyResult` shape. Structured-output JSON schema for `PlannerOutput`. Effort: `small`. _Depends on: none._

2. **Engine: `planner_runs` table + migration** (`boss-engine`). Schema add per _Durable audit trail_, idempotent migration, the UNIQUE-per-project partial index, and `WorkDb` accessors (claim/update/list). Effort: `medium`. _Depends on: none._

3. **Engine: the Planner** (`boss-engine`). `Planner::plan(PlannerInput) -> Result<PlannerOutput>` reusing the `live_status.rs` Anthropic substrate; system prompt encoding the Q4 effort heuristic, kind conventions, parallelism-maximizing edge guidance, and `[effort-classification]` emission; structured-output enforcement; bounded model/effort/timeout; typed outcomes. Effort: `large`. _Depends on: 1._

4. **Engine: live doc fetch** (`boss-engine`). Fetch the design doc at the merged ref via `gh api /contents` (reuse the `design_detector` fetch shape), with bounded retries and 404 handling. Effort: `small`. _Depends on: none._

5. **Engine: deterministic Materializer** (`boss-engine`). `Materializer::apply(project_id, &PlannerOutput)`: handle resolution, topo-sort + cycle reject, `(name, project_id)` dedup, single-transaction `create_task` (`autostart = false`) + `add_dependency`, `ApplyResult`. Tag created tasks with the `planner_runs.id`. Effort: `large`. _Depends on: 1, 2._

6. **Engine: validation layer** (`boss-engine`). Schema/non-empty/cap/acyclicity/handle-integrity checks producing the no-op-safe outcomes; `breakdown_found` and confidence handling. Effort: `medium`. _Depends on: 1._

7. **Engine: the Populator + trigger hook** (`boss-engine`). Enqueue a background Populator job from `merge_poller::mark_merged`'s `kind == 'design'` block; orchestrate idempotency claim → pre-seeded check → fetch → plan → validate → apply → audit → surface. Effort: `large`. _Depends on: 2, 3, 4, 5, 6._

8. **Engine: attention-item + event surfacing** (`boss-engine`). Outcome-specific `WorkAttentionItem` text and the `work_items_created` batch event for the kanban. Effort: `medium`. _Depends on: 7._

9. **CLI: operator entry points** (`boss-cli`). `boss project plan <project> [--force] [--dry-run]`, `boss project release <project>`, `boss project unpopulate <project> --run <id>`, `boss project plan-runs <project>`. These exercise the reusable Planner/Materializer from outside the trigger. Effort: `medium`. _Depends on: 3, 5, 6, 7._

10. **macOS app: review/release/undo surface** (`app-macos`). Render the staged tasks, the planner attention item, a "release" affordance, and a planner-run inspector (raw output + rationale). Thin client over the engine RPCs. Effort: `medium`. _Depends on: 8, 9._

11. **Tests: end-to-end fixtures** (`boss-engine`). Use P707/P757/P754 design docs as ground-truth fixtures: assert the Planner+Materializer produce the expected task set and dependency edges; cover the no-breakdown, cyclic, over-cap, pre-seeded, and fetch-failure paths; assert idempotency under double-fire. Effort: `large`. _Depends on: 5, 6, 7._

_Critical path:_ **1 → 3 → 7 → 9 → 10**, with 2/4/5/6 feeding 7 in parallel and 11 validating once 5–7 land.

---
