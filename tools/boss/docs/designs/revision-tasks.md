# Boss: Revision Tasks

- **Status:** shipped 2026-05-26 across five PRs — [#767](https://github.com/spinyfin/mono/pull/767) (schema + protocol), [#770](https://github.com/spinyfin/mono/pull/770) (CLI + create-time gate), [#778](https://github.com/spinyfin/mono/pull/778) (dispatch + completion), [#782](https://github.com/spinyfin/mono/pull/782) (hard-guard + coordinator prompt), [#775](https://github.com/spinyfin/mono/pull/775) (kanban chrome).
- **Doc state:** updated 2026-07-20 (design postmortem) to describe as-built reality. Divergences from the original plan are called out inline as **As built** notes; the most significant are the create/dispatch gate split landing inverted (Q4), the worker directive shipping on `gh pr checkout` and then evolving to engine-side workspace pre-positioning (Q3), and the OQ6 tracking comment being dropped.

## Problem

Every work-item kind Boss has today produces a _new_ artifact. A `chore` / `project_task` produces a new PR; an `investigation` / `design` produces a new markdown doc. The lifecycle plumbing reflects this: a worker pushes to its own engine-supplied bookmark (`boss/exec_<id>_<seq>`), runs `gh pr create`, and the completion detector flips the row to `in_review` when a _new_ PR URL appears (`runner.rs` spawn prelude, `runner.rs:750-758`; `PrDetector` in `completion.rs`).

There is no kind whose deliverable is _another commit on an existing PR_. Yet that is exactly the shape of the most common follow-up the operator asks for: "revise T651 to also handle the empty-list case", "for T652, can we rename that flag before it merges". Today the operator's only options are (a) reopen the merged-or-unmerged work as a brand-new chore, which produces a _second_ PR that has to be merged separately and loses the reviewer's in-context thread, or (b) drop into the worker's workspace by hand and push a commit themselves. Both fight the grain of the system.

This is also the substrate a future feature needs. A separate effort will let the operator triage GitHub PR-review comments on an in-review parent PR and act on selected ones. When that lands, each "act on this comment" decision should _create a revision task_ — the same mechanism this design defines — rather than inventing a parallel path. So the mechanism here has to be clean enough that the comment-triage UI is a thin producer on top of it.

This doc proposes a first-class **`revision`** task kind: bound to a parent task, gated on the parent's PR being open and unmerged, dispatched into (ideally) the parent's warm workspace, and delivering a _new commit on the parent's existing branch_ — no new PR. Revision tasks render as distinct cards while in Backlog/Doing and roll up under the parent's card as single-line affordances once they reach Review.

## Goals

- A new `tasks.kind = 'revision'` whose deliverable is **a new commit on the parent task's existing PR branch**, not a new PR.
- A **parent linkage** column on `tasks` that ties a revision to the task whose PR it revises, with the DB-adjacent invariant "kind = revision ⇒ parent is set".
- **Two trigger sources, one mechanism.** Source A (direct operator feedback on an in-review PR) is built now. Source B (GitHub PR-review-comment triage UI) is deferred, but the create path must be the substrate B drives — B is a thin producer, not a fork.
- A **gate** that refuses to create or dispatch a revision unless the parent has an _open, unmerged_ PR. Enforced at create time _and_ re-checked at dispatch time (the PR can merge in between).
- **Sequence numbers** (R1, R2, R3…) that are stable across reordering and meaningful to a human reading the kanban.
- A **dispatch flow** that checks out the parent's branch by name (not a fresh `boss/exec_*` bookmark), pushes back to it, and never calls `gh pr create`.
- **Cube workspace warmth**: prefer the workspace the parent last ran in, degrade gracefully when it is gone or leased.
- **Kanban chrome**: distinct revision cards in Backlog/Doing (R-badge + short revision description); a per-revision single-line affordance on the parent's card in Review. No new column.
- A **coordinator system-prompt paragraph** that teaches the Boss the verb exists and when to reach for it, without hard-coding keyphrases.
- An **effort default** for the kind that matches its usual narrowness, with documented escalation.

## Non-Goals

- **Building the Source-B comment-triage UI.** The mechanism must be extensible to it (the design anticipates the `(repo, pr#, comment-id)` pointer shape per [[feedback_github_is_source_of_truth_for_pr_artifacts]]) but the surface itself is a separate effort.
- **Auto-applying review comments** without a human gate. Operator-confirmed only.
- **Cross-PR revision.** One revision = one commit to the _one_ PR owned by its parent chain. A revision that wants to touch a different PR is a different parent.
- **Auto-rebasing a revision onto a moved-on main.** If main advanced between parent push and revision push, the worker rebases the normal way (the existing conflict-resolution flow) or fails loud. No new rebase machinery.
- **Rewriting the parent task's brief.** Revisions add commits; they do not retroactively edit the parent task's description.
- **A "revisions" kanban column.** Revisions flow through the existing Backlog/Doing/Review columns with different card chrome.
- **Auto-merging the parent PR** after N successful revisions. Always human-merged.
- **A dedicated `tasks.pr_url` of the revision's own.** A revision has no PR of its own; its artifact is a commit on the parent's PR. The revision row's `pr_url` stays `NULL`; the parent's `pr_url` is the source of truth.

## Naming

- **Kind**: `revision` (`tasks.kind = 'revision'`). CLI noun lives under the existing `boss task` umbrella: `boss task create-revision …`, mirroring `boss task create-investigation`.
- **Execution kind**: `revision_implementation` (the value carried on `executions.kind`, mirroring `investigation_implementation`). This is what the runner matches on to select the spawn directive.
- **Parent linkage**: `tasks.parent_task_id` — a soft foreign key to the `tasks` row whose PR this revision targets. `NULL` for every non-revision row.
- **Chain root**: the first non-revision ancestor reached by walking `parent_task_id` up. This is the task that _owns_ the PR. Revisions, revisions-of-revisions, etc. all share one chain root and one PR.
- **Sequence number**: `R<n>`, surfaced in UI text as "R1" / "Revision 1" — never "Friendly ID" anywhere, per [[feedback_no_friendly_id_in_ui]]. Computed, not stored (Q1).
- **Revision intent**: the operator's verbatim ask (Source A) or a short generated summary (Source B), stored in the existing `tasks.description`. This is what the Review-lane affordance renders, so it must stay short.

---

## Design Question 1 — Kind, Parent Linkage, and Sequence Numbers

### Options

- **(a) New `kind = 'revision'` + nullable `parent_task_id` column; sequence computed at render.** Same `tasks` table, one new column, one new discriminator value. Walks the parent chain to find the PR.
- **(b) Boolean `is_revision` flag layered on an existing kind (e.g. `chore`).** Keep the discriminator; add a bit.
- **(c) New table `revisions` foreign-keyed to `tasks`.** A parallel row type with its own lifecycle column.
- **(d) Encode parent + sequence in `tasks.description` / a JSON blob.** No schema change.

Sub-question on the sequence number itself:

- **(s1) Stored `revision_seq` column**, assigned at create.
- **(s2) Computed** as `(count of revision-kind siblings under the same chain root created before me)`, derived at read time.

### Discussion

The repo has already litigated (a) vs (b) vs (c) vs (d) twice — see `design-producing-tasks.md` Q1 and the investigation kind (T641). The verdict each time was (a): `kind` is the established "what shape of work is this" discriminator (`chore`, `investigation`, `project_task`, `design` today; `work.rs:1622-1656`), it is free-form `TEXT` with no enum and no CHECK constraint, and validation lives in the application layer. Adding `revision` is the natural fourth-ish extension. (b) doubles the discriminator and forces every `match task.kind` site to also test a bit; (c) duplicates executions / transcripts / attention-item / dispatch plumbing that all key on `tasks.id`; (d) hides the row's type inside a string.

The genuinely new thing a revision needs that prior kinds did not is the **parent edge**. There is no self-referencing FK on `tasks` today (`project_id` points at `projects`, `blocked_attempt_id` at a conflict attempt — neither is task→task). So this design adds one: `parent_task_id TEXT NULL`, soft FK to `tasks.id`, mirroring how `project_id` is a soft reference (no `REFERENCES` clause — Boss uses soft deletes and application-layer integrity, consistent with the rest of the schema).

The invariant "kind = revision ⇒ parent*task_id IS NOT NULL" is real and worth enforcing. SQLite supports a partial-ish CHECK: `CHECK (kind <> 'revision' OR parent_task_id IS NOT NULL)`. But Boss's existing tables carry **no** CHECK constraints (the schema deliberately keeps validation in Rust so error messages are good and migrations stay `ALTER TABLE … ADD COLUMN`-shaped; see the `migrate_tasks**`family, e.g.`migrate_tasks_investigation_doc_columns`at`work.rs:8156`). Adding a CHECK to a *new column on an existing table\* via `ALTER TABLE ADD COLUMN … CHECK(…)` is legal in SQLite but the constraint only re-validates on insert/update, and a table-level cross-column CHECK can't be added by `ALTER TABLE` at all without a table rebuild. So the recommendation keeps the invariant in the application layer (the `insert_revision_in_tx` constructor refuses a null parent; an `update` that would orphan a revision is rejected) and documents it as a column comment — consistent with how every other `tasks` invariant is enforced.

On the sequence number: (s1) stored is tempting but goes stale the moment anything is reordered, deleted, or soft-deleted, and it needs a uniqueness story under concurrency. (s2) computed is stable by construction and matches what the operator means by "R2": _the second revision in this chain, in creation order_. The count is cheap (`parent_task_id` chain is short) and the kanban already recomputes derived state on each `WorkTree` push. The number that matters for display is "position in the chain root's revision list, ordered by `created_at`", which is deterministic and reorder-proof because `created_at` never changes.

One subtlety: "count under the same _chain root_" vs "count under the same _immediate parent_". Revision chains (a revision can itself get a revision — see Decisions § OQ2) mean R2 can itself get a revision. The decision is **flat continuation, no nested numbering**: the chain-root count gives R1, R2, R3 across the whole chain (a revision-of-R2 is R3, never "R1.1"); the immediate-parent count would reset and is rejected. The chain-root reading is what a human scanning the parent card wants ("this PR has had 3 rounds of revision"), so the sequence is **chain-root-scoped, creation-ordered**.

### Recommendation

**Pick (a) + (s2).**

```sql
ALTER TABLE tasks ADD COLUMN parent_task_id TEXT;  -- soft FK → tasks.id;
                                                   -- NULL for non-revision rows;
                                                   -- required (app-enforced) when kind='revision'
CREATE INDEX IF NOT EXISTS idx_tasks_parent_task_id ON tasks(parent_task_id);
```

Migration mirrors `migrate_tasks_investigation_doc_columns` exactly — a `migrate_tasks_parent_task_id_column(conn)` that guards with `table_has_column(conn, "tasks", "parent_task_id")?` and runs the single `ALTER TABLE … ADD COLUMN`. Add the index in the same migration. No CHECK constraint; the invariant is enforced in `insert_revision_in_tx` and on update.

`kind` gains the value `'revision'`. No enum to extend (it is `TEXT`); the new value is recognized in the dispatch reconcile loop (Q3) and the completion path (Q4).

Sequence numbers are computed engine-side in Rust, not SQL (**as built**): `attach_revision_projections` (`work/revision_helpers.rs`) runs during `get_work_tree`, groups all revision tasks by chain root, sorts each group by `created_at`, and stamps a 1-based `revision_seq` plus the chain root's `pr_url` (as `revision_parent_pr_url`) onto the task projection. The kanban consumes the projected `R<n>` and never recomputes the chain.

`chain_root` shipped as a Rust helper (`work/chain_helpers.rs`), as this section preferred over a recursive CTE. Both walks are depth-capped (64 in the DB helper, 20 in the projection pass) so corrupt or cyclic parent links terminate instead of looping, and a broken parent link resolves to the deepest reachable ancestor rather than erroring — which is what lets a revision with a missing parent surface as a data-integrity warning instead of crashing a join (Risk R8).

#### Kind-of-revision: column or description?

The brief asks whether review-comment-driven vs operator-driven should be its own enum. **Recommendation: do not add a sub-kind column for v1.** The distinction is provenance, and Boss already records provenance in `tasks.created_via` (`canonicalize_created_via(..., "<kind>")`, `work.rs:7052,7092`). Source A sets `created_via = "operator"` (or the human's actor); Source B will set `created_via = "pr-comment:<repo>#<pr>:<comment-id>"`, which doubles as the `(repo, pr#, comment-id)` pointer [[feedback_github_is_source_of_truth_for_pr_artifacts]] wants without mirroring the comment body into Boss state. No schema change needed when B lands; B just passes a richer `created_via`. If a hard enum is ever wanted, it is an additive `revision_source TEXT` column later.

**As built:** exactly this. No sub-kind column shipped, and `created_via` became the provenance channel sooner than expected — the first non-operator producers were not the Source-B comment-triage UI but engine-spawned fix revisions (added by later projects): merge-conflict resolution attempts create revisions with `created_via = "merge-conflict:<attempt-id>"` (`conflict_watch.rs`) and CI remediation with `"ci-fix:<attempt-id>"` (`ci_watch.rs`), and the dispatcher parses those prefixes to tie a revision back to the attempt that spawned it (and to retire it when the attempt does). The thin-producer substrate works as designed, with zero schema changes per new producer.

---

## Design Question 2 — CLI Surface

### Options

- **(a) `boss task create-revision --parent <task-selector> --description "…"`** — a sibling verb to `create-investigation`, with its own arg struct.
- **(b) `boss task create --kind revision --parent <selector> …`** — overload the generic `task create`.
- **(c) `boss revision create …`** — a new top-level noun.

### Discussion

`boss task create-investigation` already exists as a dedicated verb with its own `InvestigationCreateArgs` (`cli/src/main.rs`; create dispatch around `main.rs:2543`). There is no generic `boss task create --kind X` today — each kind got its own verb. Following that established shape ((a)) keeps the parser honest: the revision verb _requires_ `--parent`, which a generic `--kind revision` flag could not enforce structurally. (b) would have to validate "if kind=revision then parent is required" at runtime, re-introducing the exact ambiguity the per-verb pattern avoids. (c) invents a top-level noun for something that is fundamentally a task; the existing nouns are `task`, `project`, `product`, and revisions live under `task`.

### Recommendation

**Pick (a).** A `RevisionCreateArgs` struct modeled on `InvestigationCreateArgs`:

```
boss task create-revision \
    --parent <task-selector>      # required; the task whose PR to revise.
                                  # Accepts the same selector forms as other
                                  # task verbs (short_id like "T651", full id,
                                  # or product/slug).
    --description "<ask>"         # required-ish; the verbatim operator ask or
                                  # a short summary. Stored in tasks.description
                                  # and rendered on the Review-lane affordance,
                                  # so keep it short.
    [--priority <p>]              # defaults to parent's priority
    [--effort <level>]            # defaults to 'small' (Q7); 'large' when the
                                  # chain root is design-family (design/
                                  # investigation, transitively through a
                                  # revision chain) — see the design-family
                                  # Fable-tier dispatch floor policy addendum
                                  # (2026-07-13); escalatable
    [--model <slug>]              # optional model override
    [--force-duplicate]           # same dedup-bypass flag as other creates
```

`--product`/`--project` are **not** flags: a revision inherits both from its parent (resolved at create time), so passing them would only create disagreement. `--repo` is likewise inherited — a revision must push to the parent's PR branch in the parent's repo; allowing a different repo would violate the one-PR invariant.

**As built (PR #770):** shipped as specified, with one deliberate inheritance nuance — `repo_remote_url` is _not_ copied onto the revision row (a per-task override collides when the product already carries the same URL); the dispatch flow resolves the repo from the chain root at spawn time. The verb has since grown `--name` (a concise card title, keeping the verbatim ask in `--description`), `--driver`, and `--depends-on` (explicit prerequisites on top of the automatic chain-tail gate a later project added to serialise back-to-back revisions on the same PR), plus a `boss task list-revisions` query verb.

#### Error behavior on a bad parent

`create-revision` validates the gate at create time (Q4) and returns a precise `CliError` rather than creating a doomed row:

- `--parent` resolves to a task with **no PR** → `error: T651 has no PR yet; a revision targets an existing open PR. Wait for T651 to reach review, or file a normal follow-up chore.`
- parent's PR is **merged** → `error: T651's PR (#1234) is already merged; revisions only apply to open, unmerged PRs. File a new chore against main instead.`
- parent's PR is **closed-unmerged** → `error: T651's PR (#1234) is closed without merging; there is no open PR to revise.`
- `--parent` resolves to a `revision` itself → allowed (revision chains, Decisions § OQ2); the gate is evaluated against the _chain root's_ PR, and the new revision's sequence number continues the chain-root count (a revision-of-R2 is R3).

**As built:** the messages live in a `RevisionGateError` enum produced by `assert_parent_revisable_and_insert` (`work/revision_helpers.rs`), which runs the gate and the row insert in one transaction. The helper is _not_ shared with the dispatch path as originally planned — the two gates diverged in mechanism (see Q4): create-time does the authoritative check (including a live PR-state probe), while dispatch-time trusts engine-cached state.

---

## Design Question 3 — Dispatch Flow

This is the heart of the design: a revision worker must behave _unlike_ every worker before it.

### What a normal worker is told today

The spawn prelude (`runner.rs` `spawn_prompt`, matching on `execution.kind` at `runner.rs:700`) hands a fresh-PR worker the block at `runner.rs:751-758`: create bookmark `boss/exec_<id>_<seq>`, `jj git push -b <that> --allow-new`, `gh pr create --head <that> --base main`. There is _already_ a divergent branch in that same function (`runner.rs:741-749`) for the case where the chore **already has a `pr_url`**: it tells the worker to push to the existing PR branch and NOT open a new PR, confirming with `gh pr view <n>`. The revision flow is a specialization of that existing "resume existing PR" branch, but the PR belongs to the _parent_, not to this execution.

### Options for selecting the divergent prelude

- **(a) Add a `revision_implementation` arm to the `match execution.kind`** in `spawn_prompt`, with a `compose_revision_directive(parent)` that names the parent's branch and forbids `gh pr create`. Mirrors `compose_investigation_directive` (`runner.rs:809`).
- **(b) Reuse the existing `existing_pr_url` branch** (`runner.rs:741`) by populating the revision execution's "existing PR" slot from the parent's `pr_url`.
- **(c) A post-hoc engine fast-forward** — worker pushes a normal feature branch, engine grafts it onto the parent's branch.

### Discussion

(c) is rejected for the same reasons `design-producing-tasks.md` Q4 rejected engine-side `jj`: running git plumbing in engine code outside a leased workspace is fragile and we have a standing decision against it. (b) is appealing — the "existing PR" prelude already says the right words ("push to the existing branch, don't open a new PR") — but the parent's `pr_url`/branch is not _this_ execution's, and the acceptance-criterion block keys off `existing_pr_url` being the execution's own. Threading the parent's branch through that slot blurs "this task's PR" with "the parent's PR", which matters for the completion detector (Q4). (a) is explicit and mirrors the established `compose_*_directive` pattern; the runner already branches on `execution.kind`, so one more arm is the low-surprise change.

### Recommendation

**Pick (a).** Add to the `match execution.kind` in `spawn_prompt`:

```rust
"revision_implementation" => {
    prompt.push_str(&compose_revision_directive(parent_task, parent_pr));
}
```

and add `revision_implementation` to the acceptance-criterion `matches!` set (`runner.rs:718-721`) so the worker still gets the "deliverable is a pushed branch" framing — but `compose_revision_directive` supplies the branch name and _suppresses_ the new-PR instructions.

#### `compose_revision_directive` — the worker's marching orders (as built)

The original draft templated the parent branch name into a `jj git fetch` → `jj edit <PARENT_BRANCH>` recipe and ended with a `[boss-revision] R<n>` tracking comment on the PR. What shipped diverged in three ways:

- **PR #778 (first cut):** the directive told the worker to `gh pr checkout <PR#>` rather than `jj edit` a templated branch name. The engine resolves the PR number and URL from the chain root (stamped onto `executions.pr_url` at dispatch — see Q4) and the worker discovers the branch from the checkout; `<PARENT_BRANCH>` and `R<n>` are never templated into the directive at all.
- **Superseded since:** checkout later moved out of the worker entirely. The engine now pre-positions the workspace at the PR head (`cube workspace goto --pr <n>`) before spawn, and the directive explicitly _forbids_ `jj edit` / `gh pr checkout` / `git checkout` (fetched remote commits are immutable in a jj workspace and those tools misbehave there). The worker makes the change, discovers the parent bookmark name (`jj log -r 'parents(@)' -T 'remote_bookmarks'`), advances it, and pushes via `cube pr update --branch <name>`. The directive has also grown a mandatory PR-title/description reconciliation step, pre-push build gates (with a merge-correctness variant for conflict-resolution revisions), and a rebase-only exception — `compose_revision_directive` in `runner.rs` is the living text.
- **The tracking comment (draft step 9, OQ6) never shipped** — recorded as dropped in Decisions § OQ6.

Two contracts from the draft survived unchanged: "do NOT open a new PR / do NOT create a `boss/exec_*` bookmark", and "print the parent PR URL on its own line as the final thing in your response" — the latter is what the completion path keys on alongside the sha-delta gate (Q4).

#### Any workspace is correct; warmth is an optimization

The original draft made a worker-side `jj git fetch` the load-bearing first step so a cold workspace could materialise the parent branch. The underlying invariant — a revision's needed state lives on **GitHub**, so _any_ workspace can serve it — held, but as built it is discharged by the engine-side `cube workspace goto --pr <n>` pre-positioning rather than by worker VCS steps. Warm-workspace preference survives purely as a cache-warmth optimisation, per [[feedback_cube_workspaces_are_warmed_caches_no_chore_stickiness]].

#### Cube workspace allocation — the precedence rule

The coordinator already supports `--prefer`: `lease_workspace_with_fallback` (`coordinator.rs:1817`) reads `execution.preferred_workspace_id` and, if set, leases with `--prefer <id>` (`coordinator.rs:1836`). Today the only producer of a non-null `preferred_workspace_id` is the orphan-resume path (`work.rs:671`, reusing an orphaned predecessor's `cube_workspace_id`). Revision dispatch becomes the second producer.

The precedence rule for a revision's `preferred_workspace_id`, as drafted:

1. **The workspace the chain root's most recent successful execution ran in** (`executions.cube_workspace_id` for the latest non-failed execution of the chain root). This is the warmest cache — it built the parent's branch.
2. If that execution has no recorded workspace, **any prior revision's workspace** in the same chain (next-warmest — it has the branch too).
3. If none, **no preference** — lease any free workspace.

**As built (PR #778):** rule 1 and rule 3 only. `preferred_workspace_for_chain_root` (`work/dispatch_helpers.rs`) returns the `cube_workspace_id` of the chain root's most recent non-failed execution, or nothing. Rule 2 was not implemented — with the fallback silent and workspace re-provisioning cheap, the extra chain walk never justified itself.

`--prefer` here is a **nice-to-have for cache warmth only** (Decisions § OQ5); there is no correctness reason to require the parent's workspace, because a revision's needed state lives on **GitHub**, recoverable in any workspace via `jj git fetch`. So a revision uses a **soft prefer**: ask for the preferred workspace, and if it is gone or leased, **fall back silently to any free workspace** — no retries, no warnings, no human-attention surfacing. The fallback is a non-event.

**Divergence from the orphan-resume semantics.** The existing fallback matrix says: _preferred set ⇒ terminal failure if the preferred workspace can't be leased_ (to preserve state continuity — the orphan's local commits exist _only_ there). That hard policy is correct for orphan-resume but wrong for revisions, which must degrade quietly. **As built (PRs #767 + #778)** this shipped as the additive `work_executions.prefer_is_soft` boolean (defaulted false; set true for `revision_implementation` at dispatch): `lease_workspace_with_fallback` reads it to pick `fallback_policy = "any_free"`, and the hard-fail guard respects it too, so soft-prefer executions never fail on workspace unavailability. The orphan-resume path keeps the hard "none" policy untouched. When a revision lands in a non-preferred workspace, the engine's `cube workspace goto` pre-positioning makes it correct regardless. This respects [[feedback_cube_workspaces_are_warmed_caches_no_chore_stickiness]] — warmth is an optimization, never a correctness dependency.

---

## Design Question 4 — Gate Enforcement and Completion

### The gate: "parent PR open and unmerged"

The gate must hold at two moments, because the PR can merge in between:

1. **Create time** (`create-revision`, and later Source B): reject if the parent chain root's PR is absent / merged / closed-unmerged.
2. **Dispatch time** (the moment the coordinator is about to spawn the revision worker): re-check, because minutes-to-hours can pass in Backlog and the parent may have merged.

### Where the PR state already lives

The merge poller is the single surface that knows a PR's lifecycle: `PrLifecycleState { Open(OpenPrStatus), Merged, ClosedUnmerged }` (`merge_poller.rs:203`). It writes derived state back onto the _task row_ via `update_task_pr_poll_state` (`work.rs:3871`) — `pr_state_polled_at`, `ci_required_state`, `review_required_state`, `merge_queue_state` — and flips a merged chore to `done` via `mark_chore_pr_merged` (`work.rs:3821`). The brief is explicit: **do not introduce a parallel polling loop.** Reuse this surface.

### Options

- **(a) Create-time: read the chain root's _cached_ poll state\* (`pr_state_polled_at` + a derived "is open" reading); Dispatch-time: trust the same cached state, refreshed opportunistically by the existing poller cadence.**
- **(b) Create-time and dispatch-time both do a fresh synchronous `gh pr view` against the parent PR.**
- **(c) Create-time uses cached state; dispatch-time forces one targeted poll of the parent PR through the existing poller (a `poll_now(pr_url)` entrypoint), not a new loop.**

### Discussion

(b) re-polls GitHub twice and adds latency to the CLI create path; it also duplicates the `gh pr view` parsing that the poller owns. (a) is cheapest but risks dispatching a revision against a PR that merged seconds ago and hasn't been re-polled. (c) splits correctly: create time is interactive and a slightly-stale read is fine (the operator just saw the PR in review), while dispatch time is the dangerous moment (the worker is about to edit a possibly-merged branch) and deserves a _fresh, targeted_ check — but routed through the poller's existing probe (`PrLifecycleProbe`), not a bespoke call.

### Recommendation — and the inversion that shipped

The original recommendation was (c): cached read at create, fresh targeted probe at dispatch. **As built (PRs #770 + #778), the split landed inverted:**

- **Create-time gate — authoritative, including a live probe.** `assert_parent_revisable_and_insert` first checks cached state (`pr_url` NULL → "no PR" error; chain root `status = 'done'` → "merged" error, no network call), then — because cached columns cannot distinguish an open PR from a closed-unmerged or seconds-ago-merged one — does a targeted live check through a `PrStateChecker` trait (`GhPrStateChecker` shells to `gh pr view` in production; `FakePrStateChecker` keeps the gate fully unit-testable). Create is rare and interactive, so one network round-trip buys a precise error at the moment the operator is watching.
- **Dispatch-time gate — cached only.** `reconcile_revision_execution` (`work/dispatch_helpers.rs`) runs on every reconcile tick, so a live probe per tick was never viable. It defers (stays `todo`) while the chain root has no `pr_url` yet, and treats chain root `done`/`archived` — state the merge poller maintains — as the merged signal. A merge landing inside one poller cadence is caught by the poller's sweep (`block_pending_revisions_on_parent_close`) or, mid-run, by the worker's push failing/no-op'ing, which is the terminal behaviour OQ1 specifies anyway. The sweep and the dispatch-time catch-up both route through one shared `resolve_revision_on_parent_close`, so the two paths always reach identical verdicts.

The inversion honours the design's actual goal — never spawn against a merged PR without surfacing it — while spending the network call where it is cheap (create) instead of where it is hot (every reconcile tick). The gate logic remains engine-owned, reading engine-maintained PR state — consistent with [[feedback_engine_owns_reconciliation_not_ui]]. The UI never evaluates the gate; it only renders what the engine decided.

### Completion: how a revision reaches `in_review` and `done`

A revision worker pushes a commit to the parent's branch and prints the **parent's** PR URL. The completion detector (`PrDetector`-family) must handle this:

- For a `revision_implementation` execution, the detector does **not** look for a _new_ PR. **As built (PR #778)**, the chain root's `pr_url` is stamped onto the _execution row_ at dispatch time (`work_executions.pr_url` — an addition relative to the original schema plan, back-filled if the execution was minted before the parent PR opened), `on_execution_started` snapshots the parent PR's HEAD SHA, and the existing **sha-delta gate** in `completion.rs` detects head advancement when the execution stops. On success the revision row flips to `in_review`; the parent's status is untouched. This snapshot-and-compare is exactly the higher-fidelity mechanism Risk R7 sketched as a follow-up — it shipped in v1.
- The revision row's own `pr_url` stays `NULL` (`record_worker_pr_completion` explicitly preserves this). The chain root remains the PR's owner; the execution row carries the pointer.
- **`done`** (Decisions § OQ7): a revision has **no independent doneness gate** — it rides the parent's lifecycle. A revision transitions to `done` exactly when its parent (chain root) task transitions to `done`, i.e. when the parent PR **merges or is closed**. It does _not_ go `done` when its own commit is pushed, and it does _not_ go `done` when the parent PR returns to `in_review` after the push. **As built (PR #778):** `mark_chore_pr_merged` calls `flip_in_review_revisions_to_done`, a BFS over `parent_task_id` links (`collect_chain_revision_ids`) that flips every `in_review` revision in the chain to `done` alongside the root. A revision that is still in Backlog/Doing when the parent merges/closes hits the auto-block policy (Decisions § OQ1) instead.

This keeps the revision's lifecycle entirely engine-driven off existing signals: spawn → push commit → `in_review` (detector) → `done` (parent reaches `done` via the merge poller). No new poller, no new status column, no independent doneness gate.

### Permission hard-guard

The brief flags that every worker can `gh pr create` today, and a misbehaving revision worker could open a duplicate PR despite the directive. **Decision (§ OQ3): a hard guard via a Claude Code PreToolUse hook — not a `gh` wrapper binary.** A wrapper means shipping and PATH-injecting a new binary into the worker environment; a PreToolUse hook is lighter and touches nothing on disk. The guard works as follows:

- For `revision_implementation` executions, the engine's spawn configuration registers a **PreToolUse hook matching the `Bash` tool**.
- The hook receives the tool invocation's `tool_input` (the proposed shell command string). It inspects that string and, if it parses as a `gh pr create` invocation (the `gh` executable with the `pr create` subcommand, tolerant of flags and surrounding pipeline/`GIT_DIR=…` prefixes), it **denies** the call — returning a non-zero/blocking decision with a message pointing at the revision contract ("revision tasks push a commit to the parent PR; they do not open a new PR").
- All other Bash commands pass through unchanged, so the guard is a per-execution conditional, not a global block. It is only installed for `revision_implementation`; normal workers are unaffected.

This is cheap insurance: the directive tells the worker not to, and the hook makes "not to" unbreakable without modifying PATH or shipping a binary. The hook keys on the execution kind the engine already knows at spawn time, so registering it is a per-execution decision in the spawn path.

**As built (PR #782):** `execution_kind` is threaded through `WorkerSetupInput`/`StartWorkerInput` from `runner.rs` via `spawn_flow.rs`, and for `revision_implementation` the generated `settings.json` gains a second PreToolUse entry (matcher: `Bash`) whose inline Python guard reads the proposed command from stdin and returns `{"decision":"block","reason":...}` on a `gh pr create` match. Normal executions get the standard single-entry hook unchanged. The pattern later broadened into the editorial-controls hook family (which can also intercept `gh pr edit`/`comment`/`review`), but the revision-specific `gh pr create` denial remains keyed on execution kind exactly as decided in OQ3.

---

## Design Question 5 — Kanban UI

Per [[feedback_engine_owns_reconciliation_not_ui]], the engine owns the parent↔revision relationship and computes everything the card needs (the `R<n>` sequence, the chain root id, the parent's PR URL); the kanban renders engine state and never derives the relationship itself. The task projection the app already consumes (`WorkTask` in `Models.swift`, with `kind`, and the investigation pointer fields at `Models.swift:502-511`) gains: `parentTaskId: String?`, `revisionSeq: Int?` (the computed R-number), and `revisionParentPrUrl: String?`.

Cards are rendered in `ContentView.swift` (board at `ContentView.swift:689-748`; per-kind affordances at `ContentView.swift:1711-1736` where the `design` and `investigation` doc-link affordances already live). The revision chrome slots into the same affordance area.

### Backlog / Doing — distinct revision card

A revision in `todo`/`active` renders as its **own** card, visually a sibling of the parent's card but unmistakably a revision:

```
┌──────────────────────────────────────────┐
│ ⟳ R2  ·  revises T651                      │   ← header: revision glyph + R-badge
│                                            │     + "revises <parent short id>"
│ Rename --dry-run to --plan before merge    │   ← tasks.description (the ask), 1–2 lines
│                                            │
│ T651 · #1234  ↗                            │   ← chain-root short id + parent PR number,
│                                            │     click opens the parent PR
│  small · medium-model            ● active  │   ← effort/model/status, same row chrome
└──────────────────────────────────────────┘
```

- **R-badge** (`R2`) sits top-left where other cards show their kind glyph. The `⟳` revision glyph + `R<n>` together signal "this is the 2nd revision of T651". Text reads "R2" / "Revision 2", never "Friendly ID" ([[feedback_no_friendly_id_in_ui]]).
- **Revision-description line** is `tasks.description` truncated to ~2 lines.
- **Parent reference line** shows the chain-root short id and the PR number, and is the click target to open the parent PR (`revisionParentPrUrl`).
- **Same color family as the parent, smaller emphasis.** Not indented under the parent in Backlog/Doing (kanban columns are flat status lanes; indentation would fight the layout). Instead it is tagged: same accent color as the parent's product, with the `⟳ R<n>` chip as the distinguishing mark. This keeps it a "distinct card that resembles the original but clearly shows its revision sequence and a short description", per the brief.

### Review — rolled up under the parent

A revision in `in_review` does **not** render its own card. Instead the **chain root's** Review-lane card gains one single line per revision:

```
┌──────────────────────────────────────────┐
│ T651  Wire up --plan flag        #1234 ↗   │   ← parent card, unchanged header
│ ✓ CI · ◷ review                            │   ← existing CI/review chips
│ ──────────────────────────────────────    │
│ ⟳ R1  addressing review comments      ↗    │   ← one line per in-review revision
│ ⟳ R2  rename --dry-run to --plan      ↗    │     R-badge + short intent + link
└──────────────────────────────────────────┘
```

- Each line is `⟳ R<n>` + the revision's short intent (`tasks.description`, hard-truncated — this is why the brief insists the description stays short, and why the future Source-B description must be terse, e.g. "addressing @alice's comment on foo.rs:42").
- The line's link target: **the parent PR** (`revisionParentPrUrl`). v1 links to the PR itself rather than a specific commit, because the revision row does not store the commit SHA (the worker pushes it; the engine does not capture it). If a future iteration wants per-commit links, the completion detector (Q4) can capture the pushed SHA into a new `revision_commit_sha` column and the line links to `…/pull/<n>/commits/<sha>`. Flagged, not built.
- These lines read from engine state (`parentTaskId` + `revisionSeq` + status), so the app groups in-review revisions under their chain root purely by reading fields, never by inferring the relationship.

A revision that has reached `done` (parent merged) drops off the Review affordance with the parent — the whole card moves to Done together.

### Edge: parent in Review, revision still in Doing

Common case: the operator files R1 while T651 sits in review; R1 is in Backlog/Doing (its own card) while T651 is in Review (its card, no R1 line yet). Once R1's worker pushes and R1 flips to `in_review`, R1's standalone card disappears and the `⟳ R1` line appears under T651. The transition is purely status-driven and needs no special handling.

**As built (PR #775):** shipped as specified. `WorkTask` gained `parentTaskId`/`revisionSeq`/`revisionParentPrUrl`, decoded leniently (nil against pre-revision engines, which is why this phase could merge before Phases 3–4 did); `workItems(in: .review)` suppresses `kind == "revision" && status == "in_review"` rows; the rollup lines render one `RevisionRollupLine` per in-review revision ordered by `revisionSeq`; the Backlog/Doing card carries a `RevisionBadge` chip plus "revises T\<n\>" and a parent-PR link row; no "Friendly ID" strings anywhere. A later addition projects `has_in_progress_revision` onto chain roots so the parent's Review card can also hint at revisions still in Backlog/Doing.

---

## Design Question 6 — Coordinator System-Prompt Addition

The Boss coordinator session needs to learn the verb exists and when to reach for it, without a keyphrase list (the brief is explicit: trust the coordinator to recognize feedback intent). One paragraph, added to the coordinator's system prompt:

> **Revision tasks.** When the operator gives feedback on a task whose PR is already open and in review — asking to change, add to, or fix something in that work _before it merges_ — that is a **revision**, not a new chore. A revision adds a commit to the existing PR rather than opening a new one. Create it with `boss task create-revision --parent <task> --description "<the operator's ask, kept short>"`. Reach for this whenever the operator's intent is "amend the work that produced this open PR" rather than "start something new". Do not use it if the parent has no PR yet, or if the PR is already merged or closed — in those cases a normal `boss task create` (a fresh chore) is correct, and `create-revision` will refuse with a gate error pointing you there. Pass the operator's wording through to `--description` verbatim where it is already concise; summarize only if it is long, because that text is what reviewers see on the kanban.

This teaches recognition (feedback on an in-review PR), the command shape, and the gate boundary, while deferring keyphrase judgment to the model.

**As built (PR #782):** the paragraph ships in `bossSystemPrompt()` (`app-macos/Sources/Ghostty/BossPaneModel.swift`) essentially as drafted. The CLI's guidance later gained a companion note teaching the coordinator that filing a new revision while a prior one is still in flight is safe — the engine serialises same-chain revisions via the automatic chain-tail gate — so it should not defensively queue or `--no-autostart`.

---

## Design Question 7 — Effort Classification

### How effort works today

Effort is a marker-based scan of a task's title + description (`effort.rs`, `audit_effort.rs`) — there is **no kind dimension** in the heuristic today, and there is no per-kind effort rule in any `CLAUDE.md` under `tools/boss` (the effort guidance is code, not prose). The scan counts markers to suggest a level (`trivial`/`small`/`medium`/`large`/`max`).

### Options

- **(a) No special handling** — a revision is scanned like any task by its description.
- **(b) Kind-aware default**: `revision` defaults to `small`, then the marker scan can _escalate_ (never silently downgrade) if the description carries large-effort markers.
- **(c) Always `small`/`trivial`, no escalation.**

### Discussion

Revisions are usually narrow ("rename the flag", "handle the empty case") — (c)'s instinct is right most of the time but wrong for the occasional "actually, re-architect how this handles concurrency" revision, which is rare but real. (a) ignores the strong prior that revisions are narrow. (b) encodes the prior (default `small`) while letting the existing marker machinery catch the exceptions — the same "default low, escalate on signal" shape the brief suggests.

### Recommendation

**Pick (b).** `create-revision` defaults `--effort` to `small` when the operator does not pass one. The existing marker scan runs on the description and may _raise_ the level (large/max markers win over the default) but never lowers it below `small`. Document this as the one kind-specific rule in the effort module: revisions start at `small`, escalation patterns still apply. The operator can always override with explicit `--effort`.

**As built (PR #770):** the default landed; the marker-scan escalation half did not. `insert_revision_in_tx` resolves effort as: explicit `--effort` if given, else a kind-aware default — `small`, later amended to `large` when the chain root is design-family (`design`/`investigation`/`design_postmortem`, transitively through the chain; the 2026-07-13 Fable-tier dispatch-floor policy addendum). The marker scan is never consulted on the create path. In practice the coordinator passes `--effort` explicitly when an ask is clearly bigger than a routine revision, and the sanctioned `[effort-escalation]` worker marker covers the misjudged tail, so the missing scan has not been felt.

---

## Decisions

The seven questions this design originally raised have been resolved by the operator (2026-05-26, recorded on PR #757). They are landed here as decisions; the design above is written to them. Numbering matches the original OQ labels so cross-references stay stable.

### OQ1 — Parent PR merges while a revision is in-flight

**Decision: auto-block, worker exits.** Do not convert the revision into a chore-against-`main`; do not keep going. When the dispatch-time gate (Q4) detects the parent PR is `Merged`/`ClosedUnmerged`, do not spawn — move the revision to `blocked` and surface a `WorkAttentionItem`. If the merge happens _mid-run_ (worker already spawned), the worker exits cleanly: the push fast-forwards a no-op or fails, and the revision lands in `blocked` + attention rather than silently opening a new PR. The operator re-targets manually (files a chore) after seeing the attention item. _Why it matters: the rejected alternatives either silently spawn a duplicate PR (worst) or auto-create work the operator didn't ask for._

**As built:** the shape shipped with two refinements. The `blocked_reason` is the short machine code `parent_pr_closed` rather than the drafted sentence, and both trigger paths — the merge-poller sweep (`block_pending_revisions_on_parent_close`) and the dispatch-time catch-up in `reconcile_revision_execution` (covering engine restarts and created-after-merge edges) — resolve through one shared `resolve_revision_on_parent_close`, so they always reach the same verdict. Later projects extended the resolver's vocabulary (e.g. archiving revisions rendered moot by the close, converting review-driven revisions to follow-ups), but "never silently open a new PR" is unchanged.

### OQ2 — Revision chains and sequence numbering

**Decision: flat continuation, no nested numbering.** A revision can itself have a revision (second-pass feedback on R1 — yes, this is allowed). The sequence is **chain-root-scoped and creation-ordered**: if a parent already has R1 and R2 and a revision is spawned on R1, the new one is **R3**, counted from the chain root — never `R1.1` or any `R<n>.<m>` sub-sequence. The parent linkage still records the _immediate_ parent (`parent_task_id` points at R1, not the root), so provenance is preserved; only the _display number_ is chain-root-scoped. _Why it matters: a human reading the parent card wants "this PR has had 3 rounds", and all revisions in a chain target the same PR; nested numbering would leak the chain's tree shape into the UI for no benefit and complicate the Review-lane rollup._

### OQ3 — Permission scope / `gh pr create` guard

**Decision: a Claude Code PreToolUse hook, not a `gh` wrapper.** A wrapper is heavy (a new binary, PATH injection); a hook can inspect the Bash command and reject `gh pr create` for `revision_implementation` executions without touching PATH or shipping anything. The hook matches the `Bash` tool, inspects the proposed command in `tool_input`, and denies any `gh pr create` invocation (tolerant of flags and `GIT_DIR=…` prefixes) with a message pointing at the revision contract. It is registered only for `revision_implementation`, so normal workers are unaffected (see Q4 § Permission hard-guard for the mechanism). _Why it matters: trusting the directive alone means one confused worker turn can open a duplicate PR — exactly the one-PR-per-task invariant ([[feedback_one_pr_per_task]]) this design is the sanctioned exception to. A stray second PR is the most damaging failure mode here._

### OQ4 — Source-B description shape (forward-looking constraint)

**Decision: no strong opinion from the operator; this design picks a reasonable default, revisable when the B-path UI lands.** When the deferred comment-triage UI creates a revision, `--description` should read `Addressing comment from @<user> on <file>:<line>` (e.g. "Addressing comment from @alice on runner.rs:712") — short, attribution-bearing, and fitting in the Review-card single-line affordance. The full `(repo, pr#, comment-id)` pointer is carried in `created_via` (Q1), not the description, per [[feedback_github_is_source_of_truth_for_pr_artifacts]]. _Why it matters: if B writes verbose descriptions, the Review rollup becomes unreadable; setting a terse convention now means B is built to it. The exact wording is not load-bearing and the B-path effort can refine it._

### OQ5 — Cube workspace fallback when the preferred workspace is gone/leased

**Decision: `--prefer` is nice-to-have only; fall back silently with no ceremony.** Try `--prefer <chain-root's-last-workspace>` for cache warmth, and if it is gone or leased, fall back to any free workspace — **no retries, no warnings, no human-attention surfacing**. The branch state is recoverable from GitHub, so any workspace is correct; warmth affects build speed only, and very minorly. _Why it matters: the existing matrix fails terminally when a preference can't be honored; applied to revisions that would wedge one behind a busy workspace for no reason._

**As built (PRs #767 + #778):** shipped exactly so, as `work_executions.prefer_is_soft` (default 0; set for revision dispatch). `lease_workspace_with_fallback` and its hard-fail guard both respect the flag, so soft-prefer executions never fail on workspace unavailability; orphan-resume keeps hard-prefer untouched (Q3).

### OQ6 — Boss tracking comment on the parent PR

**Decision: yes, but gated by editorial controls — subsequently dropped in implementation.** The plan was for the revision worker to post `[boss-revision] R<n>: <description>` on the parent PR after pushing, behind an `editorial_controls::should_post_comment` hook point defaulting to "post".

**As built: never shipped, and now considered dropped rather than pending.** No phase implemented it — PR #778's directive contained no comment step, and nothing in the engine posts `[boss-revision]`. Two later developments superseded the breadcrumb's purpose and polarity: the revision directive now _requires_ reconciling the parent PR's title and description after each revision (a stronger reviewer breadcrumb than a bot comment, since it keeps the PR's own summary truthful), and editorial controls landed as a PreToolUse hook regime that intercepts and can deny worker `gh pr comment` invocations — the opposite of this decision's "default to post". Reviving the tracking comment would need a fresh decision against that regime; this postmortem records it as dropped.

### OQ7 — Does a revision ever reach `done` independently of the parent?

**Decision: no.** `done == parent PR merged or closed`. Revisions have no independent doneness gate — they ride the parent's lifecycle. A revision is `in_review` (commit pushed, rolled up under the parent) and transitions to `done` exactly when its parent (chain root) task transitions to `done` — i.e. when the parent PR merges or is closed. Explicitly: _not_ when the revision's commit is pushed, and _not_ when the parent PR returns to `in_review` after the push (see Q4 § Completion). _Why it matters: if revisions could go `done` while the PR stays open, the Review rollup would lose them prematurely and the operator would lose sight of in-flight revision context._ **As built (PR #778):** `flip_in_review_revisions_to_done`, called from `mark_chore_pr_merged`, implements exactly this.

---

## Schema and Wire Summary

### Column adds

As shipped in PR #767 (the executions table is `work_executions`, a naming correction from the draft):

```sql
-- tasks: parent linkage for revisions.
ALTER TABLE tasks ADD COLUMN parent_task_id TEXT;   -- soft FK → tasks.id; NULL
                                                    -- for non-revision rows;
                                                    -- app-enforced NOT NULL when
                                                    -- kind = 'revision'.
CREATE INDEX IF NOT EXISTS idx_tasks_parent_task_id ON tasks(parent_task_id);

-- work_executions: soft-prefer signal for cube lease fallback (OQ5).
ALTER TABLE work_executions ADD COLUMN prefer_is_soft INTEGER NOT NULL DEFAULT 0;
```

One schema-adjacent addition the draft did not anticipate (PR #778): the execution row carries the revision's target PR — `work_executions.pr_url` is stamped with the chain root's `pr_url` at dispatch (back-filled if the execution was minted before the parent PR opened). It exists so the completion sha-delta gate and the worker directive read the PR pointer from the execution without re-walking the chain, while `tasks.pr_url` stays `NULL` on revision rows.

`tasks.kind` gains the value `'revision'` (no enum/CHECK; validation in the application layer, consistent with every other kind). The "kind = revision ⇒ parent_task_id IS NOT NULL" invariant is enforced in `insert_revision_in_tx` and on task update, not by a DB constraint (Q1).

Optional, deferred (flagged, not v1): `tasks.revision_commit_sha TEXT` if per-commit Review-lane links are wanted (Q5); `tasks.revision_source TEXT` if a hard sub-kind enum is ever wanted over `created_via` (Q1).

Migrations follow the `migrate_tasks_*_columns` pattern (`work.rs:8156`): `table_has_column` guard + single `ALTER TABLE … ADD COLUMN`, idempotent, no backfill (existing rows default to `NULL`/`0`).

### Protocol / wire additions

```rust
// protocol/src/wire.rs — mirrors CreateInvestigation (wire.rs:399).
CreateRevision { request_id: String, input: CreateRevisionInput }

// protocol/src/types.rs
pub struct CreateRevisionInput {
    pub parent_task_id: String,          // resolved from --parent selector
    pub description: String,             // the ask; rendered on Review rollup
    pub priority: Option<String>,        // defaults to parent's
    pub effort_level: Option<EffortLevel>, // defaults to 'small' (Q7), or 'large'
                                            // for a design-family chain root
    pub model_override: Option<String>,
    pub force_duplicate: bool,
    pub created_via: Option<String>,     // "operator" (A) or
                                         // "pr-comment:<repo>#<pr>:<cid>" (B)
}

// Task projection gains (mirrored into Models.swift WorkTask):
//   parent_task_id: Option<String>
//   revision_seq:   Option<i64>     // computed R<n>, engine-supplied
//   revision_parent_pr_url: Option<String>  // chain root's pr_url, for the card link
```

The `revision_seq` and `revision_parent_pr_url` fields are **engine-computed** projections, not stored columns — the kanban consumes them and never recomputes the chain (Q5, [[feedback_engine_owns_reconciliation_not_ui]]). `CreateExecutionInput` also gained `prefer_is_soft: bool` and `pr_url: Option<String>` (both `#[serde(default)]`) so dispatch can stamp the execution row.

### Engine touch-points (as shipped; the `work.rs` monolith has since been split into `work/*.rs` modules)

- `work/` — `insert_revision_in_tx` + `assert_parent_revisable_and_insert` + `attach_revision_projections` (`revision_helpers.rs`); `chain_root` (`chain_helpers.rs`); `reconcile_revision_execution` + `preferred_workspace_for_chain_root` (`dispatch_helpers.rs`); `flip_in_review_revisions_to_done` called from `mark_chore_pr_merged`.
- `runner.rs` — `revision_implementation` arm in the spawn-prompt kind match; `compose_revision_directive`; acceptance-criterion framing for revisions.
- `coordinator.rs` — `lease_workspace_with_fallback` reads `prefer_is_soft` to select `fallback_policy = "any_free"`.
- `completion.rs` — `on_execution_started` / `evaluate_sha_delta_gate` fall back to `execution.pr_url` for `revision_implementation`; `record_worker_pr_completion` preserves `task.pr_url = NULL`. (The drafted dispatch-time poller probe and `should_post_comment` hook point were not built — see Q4 and OQ6.)
- `cli/src/main.rs` — `create-revision` verb + `RevisionCreateArgs`; gate-error messages (Q2).
- `app-macos` — `WorkTask` fields (`Models.swift` + `EngineClient` parsing); revision card + Review rollup affordance (`ContentView.swift` and helpers).
- coordinator system prompt — the Q6 paragraph in `bossSystemPrompt()` (`BossPaneModel.swift`).
- worker spawn config — `execution_kind` on `WorkerSetupInput`/`StartWorkerInput`; per-kind PreToolUse guard in the generated `settings.json` (OQ3); no new binary, no PATH change.

---

## Risks

**R1 — Stray duplicate PR.** A revision worker ignores the directive and runs `gh pr create`, producing a second PR for the same work — the exact violation of [[feedback_one_pr_per_task]] this kind is the sanctioned exception to. _Mitigation:_ a PreToolUse hook that denies `gh pr create` for `revision_implementation` (OQ3), not prelude trust alone.

**R2 — Editing a merged branch.** The parent merges between dispatch decision and worker push. _Mitigation:_ dispatch-time re-poll via the existing poller (Q4) + the auto-block policy (OQ1); the PreToolUse hook prevents the failure mode from degrading into a new PR.

**R3 — Wrong/cold workspace breaks the branch checkout.** A revision lands in a workspace without the parent branch. _Mitigation:_ `jj git fetch` is step 1 of the directive (Q3) — the branch is always recoverable from GitHub; warmth is an optimization only ([[feedback_cube_workspaces_are_warmed_caches_no_chore_stickiness]]).

**R4 — Soft-prefer regression on orphan-resume.** Adding a soft-prefer path risks loosening the orphan-resume path that legitimately needs a _hard_ prefer. _Mitigation:_ `prefer_is_soft` defaults to `0`; only `revision_implementation` sets it. Orphan-resume (`work.rs:671`) is untouched.

**R5 — Review rollup gets noisy.** Long revision descriptions (especially from future Source B) make the parent card unreadable. _Mitigation:_ hard truncation in the affordance + the terse-description convention (OQ4) baked in before B is built.

**R6 — Sequence number drift.** A computed `R<n>` could surprise if a mid-chain revision is deleted. _Mitigation:_ sequence is creation-ordered over surviving revisions (Q1); deleting R1 renumbers R2→R1, which is the intuitive "now there's one revision and it's the first" reading. Stored numbers would have been worse (gaps).

**R7 — Completion detection false-negative.** The detector must distinguish "the parent head advanced because of _my_ push" from "advanced because something else pushed". _Mitigation (shipped):_ the sha-delta gate snapshots the parent PR's HEAD at execution start and compares at stop — the snapshot-and-compare this risk asked for landed in v1 (Q4). The per-commit `revision_commit_sha` capture for Review-lane deep links remains deferred (Q5).

**R8 — Parent linkage outlives the parent.** A parent task is deleted while revisions reference it. _Mitigation:_ `parent_task_id` is a soft FK (no cascade), consistent with `project_id`; a revision whose parent is gone is surfaced as a broken-parent attention item rather than crashing a join. Walking to chain root tolerates a missing link.

---

## Phased Implementation Plan — as shipped

All five phases merged on 2026-05-26. Phase 5 landed before Phases 3–4 (its fields decode as nil against a pre-revision engine, so ordering was free).

1. **Schema + protocol** — [#767](https://github.com/spinyfin/mono/pull/767). Shipped dark as planned: `tasks.parent_task_id` + index, `work_executions.prefer_is_soft`, `CreateRevisionInput`/`CreateRevision` wire types (handler stubbed), `chain_root` helper with depth cap and broken-parent tolerance, `'revision'` recognized in the work-tree kind filter. Fresh-init, upgrade, and chain-walk tests included.

2. **CLI `create-revision` + create-time gate** — [#770](https://github.com/spinyfin/mono/pull/770). `RevisionCreateArgs`, real `CreateRevision` handler, `assert_parent_revisable_and_insert` with the three gate errors. Divergence: the gate does a **live** PR-state check via the `PrStateChecker` trait rather than trusting cached state (Q4); `repo_remote_url` deliberately not copied onto the revision row (Q2).

3. **Dispatch + completion** — [#778](https://github.com/spinyfin/mono/pull/778). `reconcile_revision_execution`, `compose_revision_directive`, soft-prefer lease, done-cascade. Divergences: dispatch-time gate is **cached-only** (Q4 inversion); completion rides the existing sha-delta gate off the new `executions.pr_url` slot instead of a bespoke detector; the drafted `jj edit` worker recipe shipped as `gh pr checkout` (since superseded by engine pre-positioning, Q3); no tracking comment (OQ6).

4. **Permission hard-guard + coordinator prompt** — [#782](https://github.com/spinyfin/mono/pull/782). Inline-Python PreToolUse guard in the generated `settings.json`, keyed on the new `execution_kind` field of `WorkerSetupInput`/`StartWorkerInput`; coordinator paragraph in `bossSystemPrompt()`. Guard + no-guard tests for revision vs chore executions.

5. **Kanban chrome** — [#775](https://github.com/spinyfin/mono/pull/775). Shipped as planned: `WorkTask` fields, distinct `⟳ R<n>` cards, Review-column suppression + per-revision rollup lines ordered by `revisionSeq`, no "Friendly ID" strings ([[feedback_no_friendly_id_in_ui]]). 15 unit tests.

The sixth item — the Source-B comment-triage UI — remains unbuilt as designed, but the thin-producer bet has already been validated from an unexpected direction: engine-spawned merge-conflict-resolution and CI-fix revisions (later projects) create revision tasks through this same substrate, carrying their provenance in `created_via` prefixes exactly as Q1 prescribed for B.

---

## Out of Scope

- The Source-B GitHub PR-review-comment triage UI (separate effort; this is its substrate).
- Auto-applying review comments without a human gate.
- Cross-PR revision (one revision = one commit to one PR).
- Auto-rebasing revisions onto a moved-on main (normal conflict-resolution flow applies, or fail loud).
- Rewriting the parent task's brief.
- A dedicated "revisions" kanban column.
- Auto-merging the parent PR after N revisions (always human-merged).
- Per-commit Review-lane links and a hard `revision_source` sub-kind enum (both deferred, flagged in Q1/Q5).

## Related

- [[engine_owns_reconciliation_not_ui]] / [[feedback_engine_owns_reconciliation_not_ui]] — parent↔revision linkage and the `R<n>` sequence are engine-owned, UI-rendered.
- [[feedback_one_pr_per_task]] — revisions are the explicit exception: one PR per parent chain + N commits via revisions.
- [[feedback_github_is_source_of_truth_for_pr_artifacts]] — Source B stores the `(repo, pr#, comment-id)` pointer (in `created_via`), not a mirrored comment body.
- [[feedback_no_friendly_id_in_ui]] — UI uses "R1" / "Revision 1", never "Friendly ID".
- [[feedback_cube_workspaces_are_warmed_caches_no_chore_stickiness]] — `--prefer` is warmth-only; `jj git fetch` makes any workspace correct.
- T641 (investigation kind) — the parallel "add a new kind" template this design mirrors throughout (migration, CLI verb, dispatch arm, spawn directive, kanban affordance).
- T653 (engine-isolation) — sibling concern in the same engine surface; not a dependency.
- `design-producing-tasks.md`, `project-design-doc-pointer.md` — prior art for the kind/schema decisions reused in Q1.
