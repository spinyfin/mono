# Boss: Unify Work-Item Kinds into One Task Entity with Flavors

> **Status: drift-reviewed 2026-07-29. Partially superseded in mechanism, still open in substance.**
>
> This document was written 2026-05-29 and last substantively edited 2026-05-30. A line-by-line re-review against the tree on 2026-07-29 found that **one of its two headline deliverables shipped by a different route**, that its factual description of the codebase is comprehensively stale, and that **the operator-facing gap it exists to close is still wide open**. Every claim below has been re-checked against source; see [§Drift review](#drift-review-2026-07-29) for the summary and the per-claim verdicts inline. Anchors in this doc are `file:line` against the tree at review time and will drift again — treat them as pointers, not guarantees.

## Drift review 2026-07-29

### What changed under the doc

**The engine was split into crates.** Every path in the original doc read `engine/src/work/…`. That tree no longer exists: `tools/boss/engine/` is now ~40 sibling crates (`core`, `driver`, `effort`, `event-bus`, `feature-flags`, …) and the code this doc describes lives under `tools/boss/engine/core/src/`. Every path and line anchor in the original was wrong. They have been re-derived.

**`kind` is no longer a free-form string.** The original opened by describing `tasks.kind` as "a free-form `TEXT` column, no `CHECK`, validated in Rust". It is now a real protocol enum, `TaskKind` (`protocol/src/types/task.rs:399`), whose variant list, `ALL`, `as_str`, and `FromStr` are all generated from a single `task_kind_variants!` invocation (`task.rs:469-478`) specifically so a new variant cannot be added without every derived surface being forced to account for it. The _database_ column is still `TEXT NOT NULL` with no `CHECK` (`engine/core/src/work/schema_init.rs:155`) — that half of the claim survives, and it matters for migration shape (below).

**Five kinds became eight.** `followup`, `design_postmortem`, and `task` were added. This is the single biggest challenge to the doc's model and is assessed in [§Does the collapse still fit?](#does-the-collapse-still-fit-eight-kinds-four-flavors).

**The doc's predicted failure mode recurred, in production, after it was written.** The original argued that a hand-maintained `kind IN (…)` partition would keep silently dropping kinds from list surfaces. On 2026-07-20 exactly that happened again: `design_postmortem` and `followup` were invisible on _every_ listing surface because `TaskKind::ALL` was a second hand-authored literal that had gone stale. The incident (`postmortem-archived-fanout-2026-07-20`) is now cited in four places in the source as the reason for the current compile-time enforcement (`task.rs:423-439`, `workitems.rs:22-29`, `workitems.rs:53-62`). **This is the strongest available evidence for the doc's central premise, and it postdates the doc.**

### Verdict per deliverable

| Original item                                           | Verdict                                                              | Evidence                                   |
| ------------------------------------------------------- | -------------------------------------------------------------------- | ------------------------------------------ |
| **T-A** — flavor-complete `list`                        | **Landed, different mechanism.** Filters not delivered.              | `workitems.rs:40-50`, `64-71`, `1127-1170` |
| **T-B** — `flavor` column + derived `kind`              | **Not started.** No `flavor` exists anywhere in `tools/boss/`.       | grep: only `tokio::test(flavor = …)`       |
| **T-C** — reparenting via `--project`/`--unset-project` | **Not started.** The gap is live.                                    | `cli/src/commands.rs:2761-2766`            |
| **T-D** — engine `kind`-branch collapse                 | **Not started**, and partially pre-empted by `CHORE_LIKE_KINDS_SQL`. | `engine/core/src/work.rs:22-26`            |
| **T-E** — unified `create --type`                       | **Not started.** Insert paths went from 2 to ≥5.                     | `work/create_entities.rs:185-224`          |
| **T-F / T-G / T-H / T-I**                               | **Not started.**                                                     | —                                          |

**T-A shipped without the schema work**, exactly as the original predicted it could ("ships independently of the schema work"). That prediction is confirmed. But it shipped as _kind-completeness enforced by an exhaustive match_, not as a `--type`/`--project`/`--no-project` filter set over a `flavor` column. The bug is closed; the model change is not.

### The motivating incident got its own verb

The original cited "mapping PR #959 → a chore row was impossible via any `task list`-based lookup". Two independent things have since addressed that: `list_tasks` now returns every kind, and a dedicated `boss task by-pr <n>` verb exists whose own help text uses PR #959 as its example (`cli/src/commands.rs`, `ByPr`/`ByPrArgs`). The specific operator failure is fixed twice over. **Note this weakens the "list invisibility" argument as a motivator for the flavor column specifically** — that argument has been satisfied. The remaining case for this project rests on reparenting and on model coherence, not on lookup.

---

## Problem

**[Re-verified 2026-07-29 — premise holds, specifics rewritten.]**

Boss splits leaf work items into eight `kind`s — `chore`, `design`, `followup`, `investigation`, `design_postmortem`, `project_task`, `revision`, `task` — stored in `tasks.kind` (`TEXT NOT NULL`, no `CHECK` at the DB layer, typed as `TaskKind` in the protocol). The engine has _already_ half-unified the model: `boss reference` says "a chore is a kind of task" (`cli/src/main.rs:336,343`), and the kind-agnostic verbs (`show`, `update`, `move`, `cancel`, `delete`, `restore`, `depend`, `bind-pr`, `link-external`, `unlink-external`) accept any leaf id under either `boss task` or `boss chore`. _(The original omitted `cancel` from this list; it is kind-agnostic too.)_

**The list partition described in the original is fixed.** `boss task list` is now kind-complete: `kind_returned_by_list_tasks` (`workitems.rs:40-50`) is an exhaustive `match` returning `true` for all eight variants, and the SQL fragment is generated by iterating `TaskKind::ALL` rather than hand-written (`workitems.rs:64-71`). Its own doc comment calls `boss task list` "the flavor-complete leaf listing surface". Narrow surfaces remain by choice: `list_chores` → `kind IN ('chore','followup')` (`workitems.rs:1311`), `list_revisions` → `kind = 'revision'` (`workitems.rs:1275`).

**The kanban tree still partitions, and still by hand.** `get_work_tree` runs two hardcoded lists — `kind IN ('project_task','design','investigation','revision','design_postmortem')` (`workitems.rs:800`) and `kind IN ('chore','followup')` (`workitems.rs:817`). These are string literals with no compile-time completeness guarantee, i.e. the same construct that caused the 2026-07-20 incident, in the one place that was deliberately left out of the fix (`workitems.rs:36-39` notes the tree "is unaffected by this filter"). **This is a live instance of the original bug class and the most concrete unfinished work this doc points at.**

What remains structurally broken is the axis conflation. The single `kind` enum still conflates two independent axes:

1. **Deliverable / behavior** — what the work _is_ and how it completes.
2. **Project membership** — whether the row belongs to a project. This is just `project_id IS NULL`.

And a third axis has appeared since the doc was written that it does not model at all:

3. **Provenance** — `created_via` plus, for followups, `origin_task_short_id` / `origin_pr_number` (`migrations_b.rs:1790-1806`). This sub-types revisions for dispatch ordering (`DispatchClass`, `work/dispatch_class.rs:44-50`; the matching SQL `CASE` at `executions_runs.rs:452-455`) and is what actually distinguishes a `followup` from a `chore`.

Conflating (1) and (2) is why **"promote a chore into a project" still has no path at all** — see below. Full motivation lives in the tracking issue: https://github.com/spinyfin/mono/issues/731.

### The operator gap, re-confirmed

**[Verified against source 2026-07-29 — still completely unaddressed.]**

There is no way to move an existing work item into or out of a project:

- `boss task update --project <P>` **does not reparent.** `--project` is a _short-id resolution_ flag — "Resolve a friendly short id against the product that owns this project" (`cli/src/commands.rs:2761-2766`). It never writes `project_id`.
- `boss task move --to <target>` **changes status only.** `TaskMoveArgs` carries exactly `id` and `--to <MoveTarget>`.
- No reparent verb exists anywhere in `TaskCommand` or `ChoreCommand`.

The only route remains delete-and-recreate, which discards the short id. This forced a real delete-and-recreate of two rows purely to file them under a project. **This is the gap the project exists to close and nothing has moved on it.**

## Prior investigation / Spike findings

**[Status: findings still directionally valid; counts stale.]**

A read-only spike ([writeup](https://github.com/spinyfin/mono/blob/main/tools/boss/docs/investigations/chore-vs-project-task-collapse-2026-05-30.md), merged in PR #1026 — the file is still present at `tools/boss/docs/investigations/chore-vs-project-task-collapse-2026-05-30.md`) audited every site treating `chore` differently from `project_task` — approximately 33 distinct sites at the time. Every one fell into pure project-membership or derived display/label.

**Re-review:** the _conclusion_ survives — no site branches chore vs project*task for a non-membership behavioral reason, and the 2026-07-29 sweep found none either. The \_counts* do not: the site count has grown with three new kinds, and the "nine macOS `isChore` checks" figure is now 17 in `app-macos/Sources/` (22 including tests). Treat every number in this section as an order-of-magnitude indication, not a checklist.

One claim in this section is now **partially realized rather than pending**: "three divergent list queries become one filter" — the list queries did collapse (T-A), but into a generated `kind IN (…)` over the existing enum, not into a `(flavor, project_id)` filter. The win was banked using the old model.

## Goals

Unchanged in intent. Re-stated with current status:

- A single leaf work-item entity with a **`flavor`** attribute and **project membership as an orthogonal nullable `project_id`**. — **Open.**
- A **single flavor-complete `list` surface**. — **Achieved for `list_tasks`** (`workitems.rs:40-71`); **not achieved for `get_work_tree`** (`workitems.rs:800,817`); `--type` / `--no-project` filters **not delivered** (`TaskListArgs`, `cli/src/commands.rs:2255+`, has `--project` but no kind or membership filter).
- **Promotion as a trivial field update.** — **Open.** See the operator gap above.
- **Zero-break compatibility**; `T<n>` short ids stable. — Still satisfiable; `tasks_product_short_id_idx` is `UNIQUE(product_id, short_id)` (`migrations_b.rs:917`) and remains independent of `project_id`.
- **Flavor-behavior preservation.** — Now covers three more behaviors than the original enumerated; see §Flavor-behavior preservation.
- A **phased path**: derive `kind` from `(flavor, project_id)` first. — Still the right shape, but the derivation is no longer sufficient on its own; see below.

## Non-goals

Unchanged, with two corrections:

- **A big-bang rewrite that drops `kind` in the first pass.** Still a non-goal.
- **A new top-level noun (`boss work`).** Still a non-goal; Alternative B's reasoning is strengthened by T-A having landed under `boss task` exactly as predicted.
- **Changing flavor semantics.** Still a hard requirement — but note the set of semantics to preserve has grown (followup provenance, postmortem scheduling, per-task doc pointers).
- ~~**Reparenting flavors other than `normal` in v1.**~~ **Revisit.** The original scoped this to `flavor=normal` because `design` had an intrinsic `project_id`. That premise is now false (§Chosen approach §1). A reviewer should decide whether project-less `design` changes the v1 scope guard.
- **Cross-product moves.** Still out of scope.
- **Removing the `boss chore *` aliases or the split `create-*` verbs.** Still out of scope.
- **A "flavor" kanban column.** Still out of scope.

## Alternatives considered

All four alternatives were re-read against current code. **A, B, and C stand unchanged** — no new evidence bears on them, and Alternative C's reasoning (executions, transcripts, attention items, dependency edges, and short ids all key on `tasks.id`) is if anything more true now that `attention_groups` and `worker_proposals` also key off it.

**Alternative D — drop `kind` immediately — is now more clearly correct to reject.** The original counted ~14 branch sites. The current count is higher and they are spread across more crates. Phasing remains right.

**A new alternative has emerged in practice and deserves to be named, because the codebase has partly adopted it:**

### Alternative E — Name the behavioral axis as SQL/helper predicates over `kind`, without a column

Rather than adding a `flavor` column, define the deliverable axis as _named, shared predicates_ over the existing `kind` enum, and route every site through them.

**This is what the codebase actually did**, twice:

- `CHORE_LIKE_KINDS_SQL` (`engine/core/src/work.rs:22-26`) — literally "the set of task kinds that behave like chores: they own their own PR and follow the active → in_review → done lifecycle. Used in every `kind IN (...)` filter that drives the merge-poller and blocking sweeps so a new kind only needs to be added here to be wired in everywhere." Value: `'chore', 'project_task', 'design', 'investigation', 'followup', 'design_postmortem'`. Consumed at `pr_flow.rs:446,489,688` and `output_types.rs:140`.
- `kind_returned_by_list_tasks` (`workitems.rs:40-50`) — the same idea with compile-time exhaustiveness instead of a string.

**Assessment.** This is a genuine, cheaper alternative to the flavor column for the _deliverable_ axis, and it has already delivered real value. It is **not** a substitute for the membership axis: it cannot express "this row is free-floating", it cannot make promotion a field write, and it leaves `chore` vs `project_task` as two enum values that must both be listed everywhere. Note that `CHORE_LIKE_KINDS_SQL` names exactly the doc's "owns its own PR" behavior and contains _both_ `chore` and `project_task` — i.e. it is already, implicitly, `flavor != 'revision' && flavor != <inert>`. **The two approaches are complementary, not competing**, but a reviewer should consciously decide whether T-D is still worth doing as written or whether it becomes "finish routing every site through named predicates" with the column deferred.

## Chosen approach

**[Re-reviewed. Direction stands. One invariant is now factually wrong and must be corrected before implementation.]**

Two orthogonal axes, a derived `kind`, and a flavor-complete `boss task` noun.

### 1. The flavor model (orthogonal axes)

| Axis                   | Storage                               | Values                                                                            |
| ---------------------- | ------------------------------------- | --------------------------------------------------------------------------------- |
| Deliverable / behavior | `flavor TEXT NOT NULL`                | `normal`, `design`, `investigation`, `revision` (**under-specified — see below**) |
| Project membership     | `project_id TEXT NULL` (exists today) | NULL (free-floating) or a project id                                              |

**The invariant table as originally written is now wrong in one row.** Corrected against source:

| `flavor`        | `project_id`                  | `parent_task_id` | `pr_url`                  | Legacy `kind`                        |
| --------------- | ----------------------------- | ---------------- | ------------------------- | ------------------------------------ |
| `normal`        | NULL or set                   | NULL             | own PR                    | `chore` if NULL, else `project_task` |
| `design`        | ~~**required**~~ **optional** | NULL             | own PR                    | `design`                             |
| `investigation` | optional                      | NULL             | own PR                    | `investigation`                      |
| `revision`      | inherited from parent         | **required**     | **NULL** (parent owns it) | `revision`                           |

**`design` no longer requires a `project_id`, and the project-less case is deliberate, implemented, and tested.** `task_uses_per_task_doc(kind, has_project)` (`design_detector.rs:355-357`) returns `true` for `TaskKind::Investigation` and for `TaskKind::Design` _when it has no project_, routing such a row to the per-task `doc_*` pointer columns instead of the per-project design-doc pointer. Its own doc comment: "`true` … for project-less `kind = design` tasks (which have no project pointer to populate)." There is an explicit assertion for it at `design_detector.rs:1155`. The project-side path correspondingly requires `project_id.is_some()` (`completion/pr_transition.rs:269`).

This is **Risk 6 of the original doc, materialized**. The original wrote: "If a future 'free-floating design' use case appears, the invariant must relax. Out of scope now." It appeared. The invariant must relax, and it is no longer a hypothetical.

Note also that `task_uses_per_task_doc` is _already a function of `(kind, has_project)`_ — the exact pair this design proposes to make first-class. The codebase reached for the two-axis predicate on its own where it needed it.

### Does the collapse still fit? Eight kinds, four flavors

**[New section — required by the drift review.]**

The original's four flavors do not cover the current eight kinds. Assessment of each:

| Current `kind`      | Maps to                                | Fits the model?                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| ------------------- | -------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `chore`             | `(normal, project_id IS NULL)`         | Yes — as designed.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `project_task`      | `(normal, project_id IS NOT NULL)`     | Yes — as designed.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `investigation`     | `(investigation, optional)`            | Yes.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `revision`          | `(revision, parent_task_id required)`  | Yes.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `design`            | `(design, optional)`                   | Yes, once the invariant is corrected.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `followup`          | `(normal, NULL)` **+ provenance**      | **Needs the third axis.** Behaviorally it _is_ a chore — its own doc comment says "Behaviorally identical to `Chore` for dispatch/execution purposes; distinct for UI rendering and provenance tracking" (`task.rs:402-406`). It shares `ChoreImplementation` (`exec_status_helpers.rs:19`), the kanban `Chore` bucket (`audit_misc.rs:296`), the `"chore"` error noun (`dispatch_helpers.rs:194`), and `list_chores` (`workitems.rs:1311`). Its only distinction is `origin_task_short_id` / `origin_pr_number`. **It should not become a flavor; it is `normal` + provenance.** |
| `design_postmortem` | `(design, project required)` + trigger | **Ambiguous.** Design-shaped: same `ExecutionKind::ProjectDesign` (`exec_status_helpers.rs:26`), same `design_repo` routing (`exec_status_helpers.rs:324`), same doc-PR completion gate (`pr_transition.rs:265-328`). Differs by _trigger_ (auto-scheduled by `project_postmortem_sweep` when a project's non-terminal count hits zero) and by prompt. Either a fifth flavor, or `design` + a provenance/trigger marker. **Reviewer decision.**                                                                                                                                   |
| `task`              | —                                      | **Vestigial. Recommend retiring.** Never constructed anywhere in production code (only in tests and match arms). Inert in dispatch — its arm is literally `// Plain task: no standalone execution; must be in a project.` (`executions_runs.rs:658-660`). Excluded from `CHORE_LIKE_KINDS_SQL` "deliberately because non-project tasks don't share the PR-on-merge lifecycle yet" (`pr_flow.rs:410-413`).                                                                                                                                                                         |

**Conclusion: the collapse model survives, but the original's "four flavors" is wrong.** The correct reading is that the kind set grew along the axes the doc _already identified as separate_ — `followup` is a provenance variant of `normal`, `design_postmortem` is a trigger variant of `design`, `task` is dead weight — which is evidence _for_ the two-axis model rather than against it. What the doc must add is an explicit third **provenance** axis (already half-present as `created_via` + the `origin_*` columns) so that `followup` and `design_postmortem` collapse onto existing flavors instead of multiplying them.

**Recommendation for a human:** flavors become `normal | design | investigation | revision`; `followup` collapses to `normal` + provenance; `design_postmortem` collapses to `design` + provenance; `task` is retired in a separate cleanup. This keeps four flavors. **This is a model change beyond the original doc's scope and should be explicitly accepted or rejected before implementation, not assumed.**

### 2. `kind` disposition: derive now, drop later

**[Still the right lever. Derivation function needs extending.]**

The `derive_kind(flavor, project_id)` helper as originally written handles four flavors and cannot produce `followup`, `design_postmortem`, or `task`. With the provenance axis above it becomes `derive_kind(flavor, project_id, provenance)`, or — cleaner — the three non-modelled kinds are handled as explicit carve-outs. This is a real complication the original did not anticipate and it makes T-B meaningfully larger.

The rationale for keeping `kind` derived rather than dropping it is **strengthened**: `kind` is now a typed protocol enum with macro-enforced exhaustiveness, so far more code depends on its exact variant set than when the doc was written.

### 3. CLI surface: make `boss task` flavor-complete

**[Half landed.]**

- `boss task list` returns every kind by default — **done** (`workitems.rs:40-71,1127-1170`).
- `--type <flavor>` / `--flavor` — **not implemented.** `TaskListArgs` (`cli/src/commands.rs:2255+`) has `--product`, `--project`, `--status`, `--priority`, `--match`, `--limit`, `--id`, `--deleted`, `--include-archived`, `--repo`, and dependency filters. No kind or flavor filter.
- `--no-project` — **not implemented.** `list_tasks(product_id, project_id: Option<&str>, …)` treats `None` as "all rows in product", not "free-floating only" (`workitems.rs:1127-1160`), so there is no way to ask for free-floating rows.
- `boss chore list` as an alias — **not done**; it remains a distinct RPC (`list_chores`, `workitems.rs:1297-1311`), now widened to `kind IN ('chore','followup')`.
- Unified `boss task create --type <flavor>` — **not done.** `TaskCreateArgs` has no kind/type flag. Create verbs remain split: `create`, `create-many`, `create-investigation`, `create-revision` under `boss task`, plus `create`/`create-many` under `boss chore`.

**The original said "two insert paths become one". That is now optimistic**: there are at least five (`insert_task_in_tx`, `insert_chore_in_tx`, `insert_investigation_in_tx`, `assert_parent_revisable_and_insert`, `insert_design_task_for_project_in_tx` — `work/create_entities.rs:185-224`), plus engine-internal minting for `followup` (`chain_helpers.rs:349`) and `design_postmortem` (`project_postmortem_sweep`). Note that `followup` and `design_postmortem` have **no create verb at all** — they are engine-minted only, which is a point in favour of the unified-create design (a `--type` flag would give them one for free, if that is even desirable).

### 4. Promotion (reparenting)

**[Entirely unimplemented. Design still sound.]**

`boss task update --project <P> <id>` / `--unset-project <id>` remain the proposed surface. Re-verified:

- The **data-preservation guarantee is still achievable as written.** `short_id` is `UNIQUE(product_id, short_id)` (`migrations_b.rs:917`) with no relationship to `project_id`, so `T<n>` stability is free.
- The **ordinal bookkeeping anchor moved** but still exists: next-ordinal-in-project is `SELECT … WHERE project_id = ?1 AND kind = 'project_task'` at `exec_status_helpers.rs:387` (originally cited as `:217`). Reorder validation uses the same predicate at `workitems.rs:923`.
- **`--project` is already taken as a resolution flag on `update`** (`cli/src/commands.rs:2761-2766`). The original did not notice this. Reusing the same flag name for reparenting is a direct collision and needs resolving — either a different flag (`--set-project`), or context-dependent behavior (surprising), or accepting that on `update` the flag becomes a write. **This is a new, concrete design decision the original doc does not address.**
- The **v1 scope guard (`flavor = normal` only)** should be revisited given project-less `design` now exists.

### 5. Migration & back-compat

**[Re-derived against current schema. Good news: the migration shape survives intact.]**

**Constraints — the specific risk raised for this review does not exist.** There is **no `CHECK` constraint on `tasks`** tying its keys to its `kind`, or otherwise. The table DDL (`schema_init.rs:151-182`) carries none, and no migration rebuilds the table — the only `tasks` rebuilds in the tree are in test fixtures (`work/tests.rs:57`, `work/tests/t06.rs:665`). Every one of the ~30 `migrate_tasks_*` functions is `ALTER TABLE … ADD COLUMN`-shaped. **A `flavor` column can still be added as a plain `ADD COLUMN` + backfill.**

One correction to the original's justification: it claimed "the schema deliberately carries no `CHECK` constraints". That is **false as a statement about the schema** — `CHECK`s exist on `work_attention_items` (`schema_init.rs:242`), `work_item_dependencies` (`schema_init.rs:264`), `attention_groups` (`migrations_b.rs:1515`), `pr_comment_policies` (`migrations_b.rs:1466`), and several boothby tables (`migrations_boothby.rs:39,51,102,104,126,131,133,134`). The narrower claim that matters — _`tasks` carries none, so its migrations stay `ADD COLUMN`-shaped_ — is true, and the house style is better described as "no `CHECK` on `tasks`" than "no `CHECK` anywhere".

**The backfill as written is now incomplete.** The original:

```sql
UPDATE tasks SET flavor = 'normal'        WHERE kind IN ('chore', 'project_task');
UPDATE tasks SET flavor = kind            WHERE kind IN ('design', 'investigation', 'revision');
```

covers five of eight kinds. It must also handle `followup`, `design_postmortem`, and `task`, per the model decision above. Assuming the recommended collapse:

```sql
UPDATE tasks SET flavor = 'normal'        WHERE kind IN ('chore', 'project_task', 'followup');
UPDATE tasks SET flavor = 'design'        WHERE kind IN ('design', 'design_postmortem');
UPDATE tasks SET flavor = kind            WHERE kind IN ('investigation', 'revision');
-- `task`: pending the retirement decision; no rows are known to exist.
```

**Row counts could not be verified for this review.** The original asserted "a single pass over the existing 700+ rows". Engine state lives in coordinator-only storage that a worker session must not read, so this figure is **unverified and should be treated as stale**. It does not change the conclusion — the migration is a single linear `UPDATE` pass over one table with no `CHECK` to re-validate and no table rebuild, so it is cheap at any plausible size — but the specific number should not be quoted as fact. **An operator with engine access should confirm the counts, and in particular whether any `kind = 'task'` rows exist**, before T-B is scheduled.

**Index:** the proposed `tasks_product_flavor_idx ON tasks(product_id, flavor, deleted_at)` still mirrors the existing `tasks_product_idx ON tasks(product_id, kind, deleted_at)` (`schema_init.rs:184-185`) and remains appropriate.

**Back-compat:** unchanged and still sound. Adding `flavor` to JSON while retaining `kind` breaks no consumer.

### 6. Flavor-behavior preservation (audit)

**[Every row re-verified 2026-07-29. All line numbers changed; several predicates changed.]**

Paths are relative to `tools/boss/engine/core/src/`.

| Site                                                         | Now branches on                                       | Decides                                  | Change since doc                                                                                                             |
| ------------------------------------------------------------ | ----------------------------------------------------- | ---------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `work/audit_misc.rs:296`                                     | `kind == Chore \|\| kind == Followup`                 | `WorkItem::Chore` vs `::Task` for kanban | Moved from `:335`; **widened to `Followup`**                                                                                 |
| `work/exec_status_helpers.rs:19-29`                          | `match kind` (6 arms)                                 | task→execution kind                      | Moved from `:21-26`; `Followup`→`ChoreImplementation`, `DesignPostmortem`→`ProjectDesign`, `Task`→`TaskImplementation` added |
| `work/exec_status_helpers.rs:324-345`                        | `Design \| DesignPostmortem` / `Investigation`        | repo routing (`design_repo`/`docs_repo`) | Moved from `:171-191`; **`DesignPostmortem` shares `design_repo`**                                                           |
| `work/exec_status_helpers.rs:387`                            | `kind = 'project_task'` (SQL)                         | next ordinal in project                  | Moved from `:217`; predicate unchanged                                                                                       |
| `work/executions_runs.rs:590-700`                            | `match kind` (8 arms)                                 | dispatch gating                          | Moved from `:424-504`; `DesignPostmortem` dispatches independently, `Task` is inert                                          |
| `work/executions_runs.rs:452-455`                            | `kind='revision'` + `created_via`                     | **ready-queue ordering**                 | **New axis, absent from the original doc**                                                                                   |
| `work/revision_helpers.rs:60,66,268,281,293,316,358,369,382` | `kind = 'revision'` / `!= Revision`                   | chain walking / sequence                 | Moved from `:29,186,199`; more sites                                                                                         |
| `work/revision_helpers.rs:516`                               | `Design \| Investigation \| DesignPostmortem`         | default effort `"large"`                 | **New site**                                                                                                                 |
| `work/chain_helpers.rs:100,130,170,271,518`                  | `kind = 'revision'`                                   | parent-chain walk; child lookup          | Moved from `:42,106`                                                                                                         |
| `work/chain_helpers.rs:349`                                  | mints `TaskKind::Followup`                            | followup creation from chain             | **New site**                                                                                                                 |
| `work/dispatch_helpers.rs:194-200`                           | `match kind` → `"chore"`/`"task"`                     | error-message noun                       | Moved from `:159-162`; now exhaustive                                                                                        |
| `work/pr_flow.rs:72,110`                                     | `kind == Revision`                                    | keep `pr_url` NULL; in-review gate       | Moved from `:64`                                                                                                             |
| `work/pr_flow.rs:446,489,688`, `work/output_types.rs:140`    | `CHORE_LIKE_KINDS_SQL`                                | merge/blocking pollers                   | **Replaced hardcoded lists with a shared constant** (`work.rs:26`)                                                           |
| `work/blocking.rs:1376`                                      | `kind = 'revision'`                                   | conflict sweep                           | Moved from `:18`; predicate inverted in form                                                                                 |
| `completion/pr_transition.rs:265-328`                        | `Design \| DesignPostmortem` + `project_id.is_some()` | design-doc completion gate               | **Moved out of `completion.rs:1545` into a submodule; now a two-axis predicate**                                             |
| `design_detector.rs:355-357`                                 | `(kind, has_project)`                                 | per-task vs per-project doc pointer      | **New site; already two-axis**                                                                                               |
| `work/workitems.rs:800,817`                                  | two hardcoded `kind IN (…)`                           | kanban tree partition                    | Moved from `:267,281`; **still hand-maintained**                                                                             |
| `work/workitems.rs:923`                                      | `kind = 'project_task'`                               | reorder validation                       | Moved from `:321`                                                                                                            |
| `work/workitems.rs:1149,1158`                                | generated `kind IN ({…})`                             | `list_tasks`                             | **Fixed** — was `:462,471`                                                                                                   |
| `work/workitems.rs:1275,1311`                                | `kind = 'revision'` / `IN ('chore','followup')`       | narrow list RPCs                         | `:577` → `:1311`, widened to `followup`                                                                                      |

Confirmation of the three behaviors the original's acceptance criteria call out — **all three still hold**:

- **`design` seeds a project** — still keyed on kind, now at `completion/pr_transition.rs:265-328`, but **now conditioned on `project_id.is_some()`** because project-less designs exist.
- **`investigation` produces a doc-PR pointer** — `exec_status_helpers.rs:329-343` (repo routing) and `design_detector.rs:355` (per-task doc pointer); unchanged in substance. Note the column family was generalized from investigation-specific to `doc_*` (`migrate_tasks_doc_pointer_columns`, `migrations_b.rs:705`), so the original's reference to `migrate_tasks_investigation_doc_columns` is stale.
- **`revision` commits to parent's PR, gated on chain-root open PR** — `revision_helpers.rs`, `chain_helpers.rs`, `pr_flow.rs:72`; unchanged. Additionally, nested revision parentage was flattened by a migration since (`migrate_flatten_nested_revision_parents`, `migrations_a.rs`), so `parent_task_id` now always points at the chain root — which _simplifies_ the revision invariant this design must preserve.

## Risks / open questions

1. **`flavor` value naming: `normal` vs `task` vs `code`.** — **Still open**, and now sharper: a `TaskKind::Task` variant exists, so `flavor = 'task'` would collide with a real (if vestigial) kind. **Recommendation unchanged and strengthened: `normal`.**
2. **`--type` vs `--flavor` as the primary flag name.** — **Still open.** Neither exists yet.
3. **Should `--unset-project` ever be allowed for `investigation`?** — **Still open, and widened.** Project-less `design` now exists as a supported state, so the question is no longer investigation-specific: it is "which flavors may cross the membership axis, given that `design` already does?"
4. **Forward-compat for unknown flavors.** — **Partly superseded.** The codebase chose the opposite strategy for `kind`: `TaskKind::FromStr` rejects unknown values outright and the `task_kind_variants!` macro makes exhaustiveness a compile error, precisely because permissive handling caused the 2026-07-20 invisible-kind incident. A pass-through `derive_kind` would reintroduce the pattern that incident discredited. **Recommendation: make `flavor` a closed enum with the same macro treatment, not a pass-through string.**
5. **Dropping `kind` (Phase 3) needs a telemetry gate.** — **Still open**, and the bar is higher: `kind` is now a typed protocol enum consumed across the CLI, macOS app, and engine.
6. **`design` membership coupling.** — **RESOLVED BY EVENTS, against the doc's assumption.** Project-less `design` is implemented and tested (`design_detector.rs:355-357,1155`). The invariant has already relaxed. This doc's §1 table has been corrected; any implementation must not re-impose the constraint.
7. **NEW — the kanban tree is still a hand-maintained partition.** `get_work_tree` (`workitems.rs:800,817`) is the last hardcoded `kind IN (…)` pair and is structurally identical to what caused the 2026-07-20 incident. It was consciously left out of that fix. **This is the highest-value remaining bug-class carve-out and is independent of the flavor column.**
8. **NEW — `--project` flag collision on `boss task update`.** Already a short-id resolution flag (`cli/src/commands.rs:2761-2766`). Reparenting cannot reuse the name without either a semantic change or a new flag. Needs a decision.
9. **NEW — provenance is an unmodelled third axis.** `created_via` + `origin_task_short_id`/`origin_pr_number` already distinguish `followup` from `chore` and sub-type revisions for dispatch (`dispatch_class.rs:44-50`). The two-axis model must either absorb it or explicitly declare it out of scope.
10. **NEW — `TaskKind::Task` is vestigial.** Never constructed in production; inert in dispatch (`executions_runs.rs:658-660`). Its disposition (retire vs keep) blocks a clean backfill.
11. **NEW — is the flavor column still worth it?** T-A landed the headline bug fix, and `boss task by-pr` closed the motivating lookup failure, both without touching the schema. `CHORE_LIKE_KINDS_SQL` covers much of the deliverable axis. The remaining unique justification for the column is **reparenting** and model coherence. That justification is real — the operator gap is live and delete-and-recreate is still the only workaround — but it is narrower than when the doc was written. **A human should confirm the project is still worth its cost at this reduced scope.** This review does not recommend cancelling it; it recommends the decision be made consciously rather than inherited.

## Proposed implementation task breakdown

**[Re-scoped by the drift review. No work items were created, deleted, or re-scoped by this review — the status column reports what the code shows; acting on it is the coordinator's call.]**

### Already delivered

**T-A: Flavor-complete `boss task list`** — **LANDED** (mechanism differs). `list_tasks` returns all eight kinds via a compile-time-exhaustive filter (`workitems.rs:40-71,1127-1170`). **Not delivered from T-A's original scope:** `--type`, `--no-project`, `boss chore list` as an alias, and flavor/membership columns in the printed output. The tree query (`workitems.rs:800,817`) was explicitly excluded.

### Depth 0 — may run in parallel

**T-A′: Close the last hand-maintained kind partition (kanban tree)**
_Scope:_ Bring `get_work_tree`'s two hardcoded `kind IN (…)` lists (`workitems.rs:800,817`) under the same compile-time-exhaustive treatment as `kind_returned_by_list_tasks`. Pure bug-class removal; no schema dependency.
_Effort:_ small. _Dependencies:_ none. **Promoted to depth 0 by this review** — it is the residue of the incident T-A was created to prevent.

**T-A″: `--type` / `--no-project` filters on `boss task list`**
_Scope:_ The filter half of the original T-A, unbuilt. Maps onto the existing `kind` set; no schema dependency.
_Effort:_ small. _Dependencies:_ none.

**T-B: Schema + protocol — add `flavor`, backfill, centralize derived `kind`**
_Scope:_ As originally written, **plus**: extend the backfill to all eight kinds (§5); resolve `followup`/`design_postmortem`/`task` per the model decision; make `flavor` a closed enum with `task_kind_variants!`-style exhaustiveness rather than a pass-through string (Risk 4); correct the `design`/`project_id` invariant (§1).
_Effort:_ **medium → large.** The original's `medium` assumed four flavors, five kinds, and a permissive derive helper; none of those hold.
_Dependencies:_ a human decision on the flavor set (§Does the collapse still fit?) and on `TaskKind::Task` (Risk 10).

### Depth 1 — after T-B

**T-C: `boss task update` reparenting** — as written, plus resolving the `--project` flag collision (Risk 8) and revisiting the `flavor = normal` scope guard now that project-less `design` exists. _Effort:_ small→medium. _Dependencies:_ T-B.

**T-D: Engine `kind`-branch collapse** — the audit table in §6 is the re-verified spec. Note several sites already route through `CHORE_LIKE_KINDS_SQL` and may need only a predicate swap, while three sites (`design_detector.rs:355`, `pr_transition.rs:269`, `dispatch_class.rs`) are already two- or three-axis and may need no change. _Effort:_ medium. _Dependencies:_ T-B.

**T-E: Unified `boss task create --type <flavor>`** — as written, but against ≥5 insert paths rather than 2, and with a decision on whether engine-only kinds (`followup`, `design_postmortem`) gain create verbs. _Effort:_ small→medium. _Dependencies:_ T-B.

**T-F: Display — surface flavor + membership** — as written; macOS surface is larger than the original assumed (17 `isChore` sites in `app-macos/Sources/`). _Effort:_ medium. _Dependencies:_ T-B.

### Depth 2 — future / not a v1 blocker

**T-G: Drop the derived `kind` column** — unchanged; bar raised (Risk 5). _Dependencies:_ T-D + human telemetry gate.

**T-H: Deprecate `boss chore *` aliases and split `create-*` verbs** — unchanged. _Dependencies:_ T-A″, T-E + usage telemetry.

**T-I: Extend reparenting beyond `normal`** — reframed by Risk 3: the question is now which flavors may cross the membership axis given `design` already does. _Dependencies:_ T-C.

**T-J: Retire `TaskKind::Task`** — **new.** Remove the vestigial variant and its match arms once confirmed no rows exist. _Effort:_ small. _Dependencies:_ operator confirmation of row counts (§5).

**Parallelism summary:** T-A′ and T-A″ are unblocked bug-fix/UX carve-outs needing no schema work and should go first. T-B is gated on two human model decisions and is larger than originally estimated. T-C/T-D/T-E/T-F follow T-B in parallel. Depth-2 items remain gated on human judgment.
