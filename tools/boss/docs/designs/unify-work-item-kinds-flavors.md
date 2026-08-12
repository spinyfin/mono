# Boss: Unify Work-Item Kinds into One Task Entity with Flavors

> **Status: drift-reviewed 2026-07-29. Partially superseded in mechanism, still open in substance.**
>
> This document was written 2026-05-29 and last substantively edited 2026-05-30. A line-by-line re-review against the tree on 2026-07-29 found that **one of its two headline deliverables shipped by a different route**, that its factual description of the codebase is comprehensively stale, and that **the operator-facing gap it exists to close is still wide open**. Every claim below has been re-checked against source; see [§Drift review](#drift-review-2026-07-29) for the summary and the per-claim verdicts inline. Anchors in this doc are `file:line` against the tree at review time and will drift again — treat them as pointers, not guarantees.

## Drift review 2026-07-29

### What changed under the doc

**The engine was split into crates.** Every path in the original doc read `engine/src/work/…`. That tree no longer exists: `tools/boss/engine/` is now ~40 sibling crates (`core`, `driver`, `effort`, `event-bus`, `feature-flags`, …) and the code this doc describes lives under `tools/boss/engine/core/src/`. Every path and line anchor in the original was wrong. They have been re-derived.

**`kind` is no longer a free-form string.** The original opened by describing `tasks.kind` as "a free-form `TEXT` column, no `CHECK`, validated in Rust". It is now a real protocol enum, `TaskKind` (`protocol/src/types/task.rs:399`), whose variant list, `ALL`, `as_str`, and `FromStr` are all generated from a single `task_kind_variants!` invocation (`task.rs:469-478`) specifically so a new variant cannot be added without every derived surface being forced to account for it. The _database_ column is still `TEXT NOT NULL` with no `CHECK` (`engine/core/src/work/schema_init.rs:155`) — that half of the claim survives, and it matters for migration shape (below).

**Five kinds became eight.** `followup`, `design_postmortem`, and `task` were added. This is the single biggest challenge to the doc's model and is assessed in [§Does the collapse still fit?](#does-the-collapse-still-fit-eight-kinds-four-flavors).

**The doc's predicted failure mode recurred, in production, after it was written.** The original argued that a hand-maintained `kind IN (…)` partition would keep silently dropping kinds from list surfaces. On 2026-07-20 exactly that happened again: `design_postmortem` and `followup` were invisible on _every_ listing surface because `TaskKind::ALL` was a second hand-authored literal that had gone stale. The incident (`postmortem-archived-fanout-2026-07-20`) is now cited in three places in the source as the reason for the current compile-time enforcement (`task.rs:423-439`, `workitems.rs:22-29`, `workitems.rs:53-62`). (It is referenced eleven times across six files repo-wide, but the other eight citations are about the sweep's backfill behavior, not about kind-set exhaustiveness.) **This is the strongest available evidence for the doc's central premise, and it postdates the doc.**

### Verdict per deliverable

| Original item                                           | Verdict                                                              | Evidence                                   |
| ------------------------------------------------------- | -------------------------------------------------------------------- | ------------------------------------------ |
| **T-A** — flavor-complete `list`                        | **Landed, different mechanism.** Filters not delivered.              | `workitems.rs:40-50`, `64-71`, `1127-1170` |
| **T-B** — `flavor` column + derived `kind`              | **Not started.** No `flavor` column or struct field exists.          | grep: only doc comments + `libc` (below)   |
| **T-C** — reparenting via `--project`/`--unset-project` | **Not started.** The gap is live.                                    | `cli/src/commands.rs:2761-2766`            |
| **T-D** — engine `kind`-branch collapse                 | **Not started**, and partially pre-empted by `CHORE_LIKE_KINDS_SQL`. | `engine/core/src/work.rs:22-26`            |
| **T-E** — unified `create --type`                       | **Not started.** Insert paths went from 2 to ≥5.                     | `work/create_entities.rs:185-224`          |
| **T-F / T-G / T-H / T-I**                               | **Not started.**                                                     | —                                          |

_T-B evidence, stated precisely:_ `grep -rn flavor --include='*.rs' tools/boss/` does return hits, but none of them is a `flavor` column or a `flavor` struct field. They are (a) roughly eight "flavor-complete" doc comments — e.g. `work/workitems.rs:32`, `populator.rs:1621`, and several test files — describing `list_tasks`'s post-T-A completeness, and (b) an unrelated `libc` identifier, `flavor: libc::c_int` for `PROC_PIDTBSDINFO` at `worker_registry.rs:196,229`. No storage or protocol surface carries the concept.

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
- **Zero-break compatibility**; `T<n>` short ids stable. — Still satisfiable; `tasks_product_short_id_idx` is a _partial_ unique index — `ON tasks(product_id, short_id) WHERE short_id IS NOT NULL` (`migrations_b.rs:917-918`) — and remains independent of `project_id` either way.
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

All four original alternatives were re-read against current code and are reproduced below unchanged, each with a **drift verdict** attached. **A, B, and C stand unchanged or stronger; D is more clearly correct to reject than when written.** A fifth alternative has emerged in practice since and is added at the end.

### Alternative A — Fold project-membership into the flavor enum (rename `kind` → `flavor`)

Keep one enum but rename it: `flavor ∈ {chore, task, design, investigation, revision}`, where `chore` vs `task` still encodes project membership. This is the smallest diff — a column rename.

**Rejected.** It re-commits the original mistake: the deliverable axis and the membership axis stay conflated in one value. Promotion would still mean _changing the flavor_ (`chore` → `task`) rather than flipping a `project_id`, so the "trivial field update" goal is lost — every promotion mutates the discriminator that other code matches on. It also doesn't fix the list partition cleanly, because "list all normal-deliverable items regardless of project" still requires OR-ing two flavor values. The whole point of the project is to _separate_ these axes, not relabel their conflation.

> **Drift verdict 2026-07-29: still valid, and strengthened.** `kind` is now a macro-generated closed enum (`task.rs:469-478`) whose variant set is compile-time load-bearing across the CLI, engine, and macOS app. Mutating that discriminator on every promotion is a costlier proposition today than when the doc was written, not a cheaper one.

### Alternative B — A new neutral noun `boss work list`

Leave `boss task` / `boss chore` as-is and add a third noun, `boss work`, as the flavor-complete surface.

**Rejected.** It adds a third synonym for the same entity, fragmenting muscle memory and scripts further rather than consolidating. The engine already frames the model as "a chore is a kind of task," and the kind-agnostic verbs already live under `boss task`. Making `boss task list` complete (and keeping `boss chore list` as a narrowing alias) matches that existing framing, costs callers nothing, and means the _fix_ for the invisibility bug is "the noun you already use now shows everything." A brand-new noun would leave the old nouns as lingering partial views — the exact trap we're removing.

> **Drift verdict 2026-07-29: strengthened, and now empirically confirmed.** T-A landed exactly as this alternative predicted it should: the flavor-complete surface arrived under the existing `boss task` noun (`workitems.rs:40-71`), and the doc comment there calls `boss task list` "the flavor-complete leaf listing surface". `boss task by-pr` was likewise added under `boss task` rather than a new noun. No third noun was needed and none was created.

### Alternative C — Separate table per flavor (`designs`, `revisions`, …)

Give each deliverable shape its own table foreign-keyed to a base row.

**Rejected** — already litigated twice in this repo (`design-producing-tasks.md` Q1, `revision-tasks.md` Q1) and rejected both times. Executions, runs, transcripts, attention items, dependency edges, and short ids all key on `tasks.id`. A per-flavor table forces every join to become a `UNION ALL` or denies those flavors first-class plumbing. The whole codebase is built around one `tasks` table; splitting it is strictly more work for strictly less capability.

> **Drift verdict 2026-07-29: still valid, and strengthened.** The set of things keyed on `tasks.id` has grown — `attention_groups` and `worker_proposals` now key off it too — so the `UNION ALL` blast radius of a per-flavor split is larger than when the alternative was rejected.

### Alternative D — Drop `kind` immediately and compute everywhere

Add `flavor`, delete `kind`, and rewrite every `match kind` site in one PR.

**Rejected for v1.** The audit (§Flavor-behavior preservation) found ~14 distinct branch sites across `completion.rs`, `runner`-adjacent helpers, `executions_runs.rs`, `pr_flow.rs`, `revision_helpers.rs`, `chain_helpers.rs`, and several SQL queries. Touching all of them at once is a high-blast-radius change with no safe intermediate state and a painful rollback. The phased "derive first, collapse incrementally, drop last" path (Chosen approach) keeps every site green at each step.

> **Drift verdict 2026-07-29: still valid; its numbers understate the case.** The original counted ~14 branch sites in one crate. The re-verified audit in §6 lists more, spread across several sibling crates plus the macOS app (17 `isChore` sites in `app-macos/Sources/`, 22 including tests). The high-blast-radius / no-safe-intermediate-state argument is correspondingly stronger, and phasing remains right.

**A new alternative has emerged in practice and deserves to be named, because the codebase has partly adopted it:**

### Alternative E — Name the behavioral axis as SQL/helper predicates over `kind`, without a column

Rather than adding a `flavor` column, define the deliverable axis as _named, shared predicates_ over the existing `kind` enum, and route every site through them.

**This is what the codebase actually did**, twice:

- `CHORE_LIKE_KINDS_SQL` (`engine/core/src/work.rs:22-26`) — literally "the set of task kinds that behave like chores: they own their own PR and follow the active → in_review → done lifecycle. Used in every `kind IN (...)` filter that drives the merge-poller and blocking sweeps so a new kind only needs to be added here to be wired in everywhere." Value: `'chore', 'project_task', 'design', 'investigation', 'followup', 'design_postmortem'`. Consumed at `pr_flow.rs:446,489,688` and `output_types.rs:140`.
- `kind_returned_by_list_tasks` (`workitems.rs:40-50`) — the same idea with compile-time exhaustiveness instead of a string.

**Assessment.** This is a genuine, cheaper alternative to the flavor column for the _deliverable_ axis, and it has already delivered real value. It is **not** a substitute for the membership axis: it cannot express "this row is free-floating", it cannot make promotion a field write, and it leaves `chore` vs `project_task` as two enum values that must both be listed everywhere. Note that `CHORE_LIKE_KINDS_SQL` names exactly the doc's "owns its own PR" behavior and contains _both_ `chore` and `project_task` — i.e. it is already, implicitly, `flavor != 'revision' && flavor != <inert>`. **The two approaches are complementary, not competing**, but a reviewer should consciously decide whether T-D is still worth doing as written or whether it becomes "finish routing every site through named predicates" with the column deferred.

## Chosen approach

**[Re-reviewed. Direction stands. One invariant is now factually wrong and must be corrected before implementation.]**

**Two orthogonal axes, a derived `kind`, and a flavor-complete `boss task` noun.** Project membership stays where it already is — a nullable `project_id`. The deliverable/behavior axis becomes a new `flavor` column. `kind` survives the transition as a _derived, denormalized display hint_ computed from `(flavor, project_id)` on every write, so all existing `kind`-matching code keeps working byte-for-byte until it is migrated deliberately.

### 1. The flavor model (orthogonal axes)

Two axes, stored independently on `tasks`:

| Axis                   | Storage                               | Values                                                                            |
| ---------------------- | ------------------------------------- | --------------------------------------------------------------------------------- |
| Deliverable / behavior | `flavor TEXT NOT NULL`                | `normal`, `design`, `investigation`, `revision` (**under-specified — see below**) |
| Project membership     | `project_id TEXT NULL` (exists today) | NULL (free-floating) or a project id                                              |

`flavor` has **four** values, not five: the legacy `chore` and `project_task` kinds _both_ collapse to `flavor = 'normal'` and are distinguished purely by `project_id`. "Chore" becomes the display name for `(normal, project_id IS NULL)`; "task" is `(normal, project_id IS NOT NULL)`. (Whether four still suffices given eight kinds is the subject of the next subsection.)

The two axes are orthogonal _in storage_ but constrained by **flavor-specific invariants** on which combinations are legal — these are real today and must be preserved, enforced in Rust at the insert/update boundary, consistent with `tasks`'s no-`CHECK` house style (§5).

**The invariant table as originally written is now wrong in one row.** Corrected against source:

| `flavor`        | `project_id`                  | `parent_task_id` | `pr_url`                  | Legacy `kind`                        |
| --------------- | ----------------------------- | ---------------- | ------------------------- | ------------------------------------ |
| `normal`        | NULL or set                   | NULL             | own PR                    | `chore` if NULL, else `project_task` |
| `design`        | ~~**required**~~ **optional** | NULL             | own PR                    | `design`                             |
| `investigation` | optional                      | NULL             | own PR                    | `investigation`                      |
| `revision`      | inherited from parent         | **required**     | **NULL** (parent owns it) | `revision`                           |

**`design` no longer requires a `project_id`, and the project-less case is deliberate, implemented, and tested.** `task_uses_per_task_doc(kind, has_project)` (`design_detector.rs:355-357`) returns `true` for `TaskKind::Investigation` and for `TaskKind::Design` _when it has no project_, routing such a row to the per-task `doc_*` pointer columns instead of the per-project design-doc pointer. Its own doc comment: "`true` … for project-less `kind = design` tasks (which have no project pointer to populate)." There is an explicit assertion for it at `design_detector.rs:1155`. The project-side path correspondingly requires `project_id.is_some()` (`completion/pr_transition.rs:269`).

This is **Risk 6 of the original doc, materialized**. The original wrote: "If a future 'free-floating design' use case appears, the invariant must relax. Out of scope now." It appeared. The invariant must relax, and it is no longer a hypothetical.

Two invariant rows that did _not_ drift and remain load-bearing: `revision` inherits `project_id` from its parent chain and carries `parent_task_id`, with `pr_url` NULL because the chain root owns the PR (`pr_flow.rs:72`); and `investigation` is free on the membership axis (standalone or under a project).

**Why orthogonal, not folded:** the issue brief recommends the orthogonal-axis model "unless there's a concrete reason not to," and there isn't one. Promotion is a `project_id` write that never touches `flavor`; the list surface becomes a filter over two independent dimensions instead of a partition over one conflated enum; and engine code that only cares about membership (`is this free-floating?`) tests `project_id IS NULL` without consulting the deliverable axis.

Note also that `task_uses_per_task_doc` is _already a function of `(kind, has_project)`_ — the exact pair this design proposes to make first-class. The codebase reached for the two-axis predicate on its own where it needed it, which is independent corroboration of the orthogonality argument above.

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

**[Still the right lever. Derivation function needs extending — see the annotations below.]**

Keep `kind` as a **derived, denormalized column**, recomputed from `(flavor, project_id)` on every insert and every update that can change either input, via one shared helper:

```rust
fn derive_kind(flavor: &str, project_id: Option<&str>) -> &'static str {
    match flavor {
        "design" => "design",
        "investigation" => "investigation",
        "revision" => "revision",
        "normal" => if project_id.is_some() { "project_task" } else { "chore" },
        other => other, // forward-compat: unknown flavor passes through
    }
}
```

The invariant "`kind` is always consistent with `(flavor, project_id)`" is enforced in the application layer (the write path computes `kind`; callers never set it directly), matching how every other `tasks` invariant is enforced — `tasks` deliberately carries no `CHECK` constraints so migrations stay `ALTER TABLE ADD COLUMN`-shaped and error messages stay in Rust (see `revision-tasks.md` Q1 for the established rationale, and §5 below for the corrected scope of that claim).

**This is the load-bearing zero-break lever.** Because `kind` stays populated and correct, every `match task.kind` site (§Flavor-behavior preservation) keeps reading the same values it reads today. The engine collapse (rewriting those sites to read `flavor` + `project_id`) and the eventual `kind`-drop become _separate, independently-schedulable_ tasks rather than prerequisites — and the system is shippable after each one.

`boss task show <id>` will surface `(flavor, project_id, kind)` together; the derivation guarantees they are always consistent.

> **Drift annotations 2026-07-29 — none of the above is implemented; two parts need amending before it is.**
>
> - **The helper as written is incomplete.** It handles four flavors and cannot produce `followup`, `design_postmortem`, or `task`. With the provenance axis identified above it becomes `derive_kind(flavor, project_id, provenance)`, or — cleaner — the three non-modelled kinds are handled as explicit carve-outs. This is a real complication the original did not anticipate and it makes T-B meaningfully larger.
> - **The `other => other` pass-through arm should be deleted.** It is the permissive-unknown-value pattern the 2026-07-20 incident discredited; see Risk 4. `flavor` should be a closed enum given the same `task_kind_variants!` macro treatment as `TaskKind`, so an unmapped flavor is a compile error rather than a silent pass-through.
> - **The load-bearing-lever rationale is _strengthened_, not weakened.** `kind` is now a typed protocol enum with macro-enforced exhaustiveness, so far more code depends on its exact variant set than when the doc was written — which is precisely why keeping it derived rather than dropping it is still the right call.
> - The application-layer-enforcement claim survives, but its justification was overstated in the original; see §5.

### 3. CLI surface: make `boss task` flavor-complete

**[Half landed. Spec below is the intended end state; per-item status is annotated inline.]**

`boss task` becomes the single flavor-complete leaf-work-item noun. `boss chore *` and the split `create-*` verbs remain as thin back-compat aliases.

**`boss task list` returns every flavor by default** (chore, project_task, design, investigation, revision), with filters to slice:

- `--type <flavor>` (repeatable / comma-list; `--flavor` accepted as a synonym) — filter by deliverable axis. Values: `normal`, `design`, `investigation`, `revision`.
- `--project <P>` — only rows in project P (the existing flag, semantics unchanged).
- `--no-project` — only free-floating rows (`project_id IS NULL`).
- Existing `--status` / `--priority` / `--match` / `--repo` / `--id` / `--deleted` / dependency filters compose unchanged.

The query collapses to one parametric `SELECT` over `tasks WHERE product_id = ? AND deleted_at IS NULL` with optional `flavor IN (…)` and `project_id` predicates — replacing the divergent hard-coded `kind IN (…)` lists (originally cited at `workitems.rs:462/471/577`, now `:1149/:1158/:1311`) and the tree query (originally `:267/:281`, now `:800/:817`).

Back-compat aliases (behavior identical to today):

- `boss chore list` ≡ `boss task list --no-project --type normal`.
- `boss chore create` ≡ `boss task create` with no `--project`.
- `boss task create-investigation` / `create-revision` stay; a unified `boss task create --type <flavor>` is added alongside them (the split verbs become aliases that set `--type`).

The unified, flavor-complete `list` was the **highest-value early carve-out** and the first implementation deliverable (task **T-A**). It could land _before_ the schema work by mapping `--type` values onto the existing `kind` set and simply UNION-ing the currently-partitioned queries — closing the chores/revisions-invisible bug immediately, independent of the flavor column.

> **Drift annotations 2026-07-29 — status of each item above.**
>
> - **Default flavor-completeness — done**, and it landed exactly by the predicted "before the schema work" route (`workitems.rs:40-71,1127-1170`), though via a compile-time-exhaustive `kind IN (…)` rather than a UNION.
> - **`--type <flavor>` / `--flavor` — not implemented.** `TaskListArgs` (`cli/src/commands.rs:2255+`) has `--product`, `--project`, `--status`, `--priority`, `--match`, `--limit`, `--id`, `--deleted`, `--include-archived`, `--repo`, and dependency filters. No kind or flavor filter. The repeatable/comma-list semantics above remain the proposal.
> - **`--no-project` — not implemented.** `list_tasks(product_id, project_id: Option<&str>, …)` treats `None` as "all rows in product", not "free-floating only" (`workitems.rs:1127-1160`), so there is no way to ask for free-floating rows. The `boss chore list ≡ boss task list --no-project --type normal` equivalence is therefore not yet expressible.
> - **`boss chore list` as an alias — not done**; it remains a distinct RPC (`list_chores`, `workitems.rs:1297-1311`), now widened to `kind IN ('chore','followup')`. Note the widening means the equivalence above must become `--type normal` over `{chore, followup}`, i.e. it depends on the `followup`-collapses-to-`normal` model decision.
> - **The collapsed parametric `SELECT` — not done.** `list_tasks` is one query, but it filters on generated `kind IN (…)`, not on `(flavor, project_id)`.
> - **Unified `boss task create --type <flavor>` — not done.** `TaskCreateArgs` has no kind/type flag. Create verbs remain split: `create`, `create-many`, `create-investigation`, `create-revision` under `boss task`, plus `create`/`create-many` under `boss chore`.
> - **The original said "two insert paths become one". That is now optimistic**: there are at least five in-transaction insert helpers — `insert_task_in_tx` (`work/insert_helpers.rs:143`), `insert_chore_in_tx` (`:224`), `insert_investigation_in_tx` (`:277`), `assert_parent_revisable_and_insert` (`work/revision_helpers.rs:98`) and `insert_design_task_for_project_in_tx` (`:675`) — reached through the public wrappers `create_task`/`create_chore`/`create_investigation`/`create_revision` at `work/create_entities.rs:185/193/203/218`. That file also carries `import_chore_with_external_ref` (`:245`) and the bulk `create_many_tasks`/`create_many_chores` (`:292`/`:306`), plus engine-internal minting for `followup` (`chain_helpers.rs:349`) and `design_postmortem` (`project_postmortem_sweep`). Note that `followup` and `design_postmortem` have **no create verb at all** — they are engine-minted only, which is a point in favour of the unified-create design (a `--type` flag would give them one for free, if that is even desirable).

### 4. Promotion (reparenting)

**[Entirely unimplemented. Design still sound; two amendments annotated below.]**

`boss task update --project <P> <id>` and `boss task update --unset-project <id>` are the reparenting surface. No bespoke `promote` verb — promotion is a `project_id` write.

**Data-preservation guarantee (hard requirement):** reparenting changes only `project_id` (and project-side bookkeeping below). Everything else lives on the same row and is untouched: `short_id`, `status`, `last_status_actor`, `pr_url`, `effort_level`, dependency edges (`work_item_dependencies` key on `tasks.id`), `description`, and external links (`link-external` bindings). In particular, `short_id` is uniquely indexed on `(product_id, short_id)` and has **no relationship to `project_id`**, so `T<n>` is stable across (de)assignment _for free_ — no special handling required.

**Project-side bookkeeping** (the only writes beyond `project_id`):

- On `--project <P>`: assign `ordinal = MAX(ordinal) + 1` among project P's `project_task` rows (the slot the existing next-ordinal query already computes), placing the promoted row at the end of P's task list. `kind` recomputes `chore → project_task`.
- On `--unset-project`: clear `ordinal` (set NULL) and `project_id`. `kind` recomputes `project_task → chore`.

**Scope guard (v1):** `--project`/`--unset-project` apply to `flavor = normal` only. Reparenting a `design`, an `investigation`, or a `revision` (whose membership follows its parent chain) has flavor-specific rules and is deferred — the command rejects those flavors with a clear message rather than silently doing something surprising.

> **Drift annotations 2026-07-29 — nothing above is implemented; the spec re-verifies with two amendments.**
>
> - The **data-preservation guarantee is still achievable as specified.** `tasks_product_short_id_idx` is a partial unique index — `ON tasks(product_id, short_id) WHERE short_id IS NOT NULL` (`migrations_b.rs:917-918`) — with no relationship to `project_id` either way, so `T<n>` stability is free exactly as written.
> - The **ordinal bookkeeping anchor moved** but the query still exists in the assumed shape: next-ordinal-in-project is `SELECT … WHERE project_id = ?1 AND kind = 'project_task'` at `exec_status_helpers.rs:387` (originally cited as `:217`). Reorder validation uses the same predicate at `workitems.rs:923`.
> - **Amendment 1 — `--project` is already taken as a resolution flag on `update`** (`cli/src/commands.rs:2752,2761-2766`): "Resolve a friendly short id against the product that owns this project." It never writes `project_id`. The original did not notice this. Reusing the same flag name for reparenting is a direct collision and needs resolving — either a different flag (`--set-project`), or context-dependent behavior (surprising), or accepting that on `update` the flag becomes a write. **This is a new, concrete design decision the original doc does not address.**
> - **Amendment 2 — the v1 scope guard should be revisited.** It was written on the premise that `design` has an intrinsic `project_id`; §1 shows that premise is now false, so "reject `design`" is no longer obviously the right rejection. See Risk 3.

### 5. Migration & back-compat

**[Re-derived against current schema. Good news: the migration shape survives intact.]**

**Schema:** add `flavor` via `ALTER TABLE tasks ADD COLUMN flavor TEXT` in a new `migrate_tasks_flavor_column()`, the same shape as the existing `migrate_tasks_*` family (e.g. `migrate_tasks_doc_pointer_columns`, `migrate_tasks_parent_task_id_column`). Backfill in the same migration with one `UPDATE` per legacy kind (see the corrected backfill below). After backfill, `flavor` is logically `NOT NULL`, enforced in Rust: **the column is added nullable because SQLite `ADD COLUMN NOT NULL` without a constant default on a populated table is awkward — add-nullable, backfill, then treat-as-required is the house pattern**, and it is what keeps every `tasks` migration `ALTER TABLE`-shaped.

**`kind` is retained and kept derived** (§2). No `match kind` site changes in the schema PR — they keep reading the derived value.

**Back-compat:** `boss chore *` and split `create-*` verbs stay as aliases (§3). JSON `list` output gains a `flavor` field but keeps the existing `kind` field, so no consumer breaks. Scripts that pass `--kind`-style filters or read `kind` from JSON keep working.

**`T<n>` stability:** unaffected by any of the above (`short_id` is independent of flavor and `project_id`).

**Deprecation (later, out of v1):** once the engine collapse (T-D) lands and telemetry shows nothing reads `kind`, drop the column; once usage telemetry shows the aliases are unused, deprecate `boss chore *`. Both are explicitly `future / not a v1 blocker`.

The re-verification of that plan against the current tree follows.

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

**Back-compat verdict:** the back-compat plan stated above is unchanged and still sound. Adding `flavor` to JSON while retaining `kind` breaks no consumer, and no consumer has appeared since that would make it break one.

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
