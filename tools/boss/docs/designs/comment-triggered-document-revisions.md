# Boss: Comment-Triggered Document Revisions

Extend the in-app markdown-viewer comment system so that, for **design** and **investigation** documents, the pile of unresolved comments on a doc can trigger a task that revises the underlying document — closing the loop between reviewing a doc and getting it changed. A banner (`5 unresolved comments. [Revise]`) offers a single action; clicking it asks the engine to create-and-start the right kind of work item, mark the addressed comments `in_revision` with a link back to that work item, and — when the work item reaches a terminal state — reconcile the comments (resolve on merge, reopen on abandon).

This design is a **thin producer on top of two mechanisms that already exist**: the comment model from [`comments-in-markdown-viewer`](comments-in-markdown-viewer.md) (P529) and the revision substrate from [`revision-tasks`](revision-tasks.md) (P654). It is, in the vocabulary of [`unify-pr-remediation-on-revisions`](unify-pr-remediation-on-revisions.md) (P707), a new **Source** — an in-app "doc-comment" producer that sits alongside the operator (Source A), the deferred GitHub PR-comment triage (Source B), merge-conflict (Source C), and CI-fix (Source D) producers. Almost all of the machinery is built; the new work is the routing decision, the comment↔task association, and the completion reconciliation.

## Current-state grounding

Line numbers are as of writing and will drift; the module/function names are the durable anchors. The engine crate root is `tools/boss/engine/core/` (the `core` crate), protocol is `tools/boss/protocol/`, macOS app is `tools/boss/app-macos/`.

### Comment model (P529 — engine implemented, UI Phase-1 in-memory)

- Table `work_comments` — DDL in `engine/core/src/work/migrations_b.rs:1072` (`migrate_work_comments_table`), invoked from `schema_init.rs:306`. Columns: `id, artifact_kind, artifact_id, doc_version, anchor_json, body, author, status, status_actor, last_resolved_with, plain_text_projection_version, created_at, updated_at, dismissed_at`, plus index `work_comments_by_artifact`.
- Status values (`protocol/src/types.rs:863`): `active`, `resolved`, `orphaned`, `dismissed`, `dispatched`; validated in `engine/core/src/work/comments.rs:91`. `active` is the default (`default_comment_status`, `types.rs:878`).
- `artifact_kind ∈ {work_item, pr_doc}`; `artifact_id` is either a work-item id (engine-owned description) or the synthetic `pr_doc:<repo_remote_url>:<branch>:<path>` for a markdown file on a PR branch.
- Structs: `WorkComment` (`types.rs:3103`), `CommentAnchor` (`types.rs:835`, a `{exact, prefix, suffix}` W3C `TextQuoteSelector`), `CommentResolution` (`types.rs:887`), `CreateCommentInput` (`types.rs:1176`).
- RPCs (`protocol/src/wire.rs:260`): `CommentsCreate`, `CommentsList`, `CommentsResolve`, `CommentsDismiss`, `CommentsSetStatus`, `CommentsUpdateAnchor`, `CommentsDispatchMagicWand`, `CommentsApplyMagicWand`, `CommentsDiscardMagicWand`. Handlers in `engine/core/src/app/comments.rs`; routed at `app.rs:2311`. Subscription helper `comment_topic()` (`wire.rs:58`).
- A sibling table keyed to comments already exists: the magic-wand dispatch table with `comment_id TEXT NOT NULL REFERENCES work_comments(id)` (`migrations_b.rs:1021`).
- **The existing per-comment "magic wand"** (`CommentsDispatchMagicWand`) flips a single comment to `dispatched` and — for a `pr_doc` artifact — spawns a chore worker against the PR branch. This is the single-comment analogue of what this design does at the doc level (batch + routing + reconciliation). The two coexist; see [Relationship to the magic wand](#relationship-to-the-magic-wand).
- The macOS UI (`app-macos/Sources/Comments/`: `CommentLayer`, `CommentSidebar`, `CommentHighlightOverlay`, `CommentPopover`, `MagicWandResultSheet`, wired into `DesignsView.swift` and `DesignRendererView.swift` via `.withComments()`) is **Phase-1 in-memory only** — `CommentLayer.swift:19` states comments do not yet persist through the engine RPCs. Persisting the UI against the (already-built) RPCs is prerequisite plumbing this design depends on, not part of it.

### How a doc maps to its owning work item and PR

- Every task carries `tasks.pr_url` (`schema_init.rs:62`) — the single source of the owning PR for **any** kind.
- The doc-location pointer lives on the **project** for design tasks: `projects.design_doc_repo_remote_url / design_doc_branch / design_doc_path` (`schema_init.rs:45`, migration `migrate_project_design_doc_columns`, `migrations_b.rs:81`). CLI `boss project set-design-doc` (`cli/src/main.rs:296`, input `SetProjectDesignDocInput`, `types.rs:2619`, store `WorkDb::set_project_design_doc`, `products_design.rs:159`).
- Project-less items (investigations, project-less design tasks) carry a per-task doc triple `tasks.doc_repo_remote_url / doc_branch / doc_path` (`migrate_tasks_doc_pointer_columns`, `migrations_b.rs:629`).
- Every project has exactly one `kind='design'` task at `ordinal=0`, dispatched with execution kind `project_design` (this very task). The worker writes `tools/boss/docs/designs/<slug>.md`, opens a normal PR (`compose_design_directive`, `runner.rs:1262`). `active → in_review` is `WorkerCompletion::finalize_pr_transition` (`completion.rs:2637`), which writes `tasks.pr_url` (`pr_flow.rs:96`); routing at `completion.rs:2847` sends `kind==Design && project_id.is_some()` rows to `design_detector::on_design_pr_detected` (`design_detector.rs:58`), which scans the PR's changed files for a single `docs/designs/*.md` off the **head** branch (so the doc is fetchable while the PR is open) and records the pointer. `TaskKind` enum: `{Chore, Design, …, Investigation}` (`types.rs:2633`).
- **The reverse mapping — doc path → owning task → PR state — does not exist.** Every lookup today is forward (task id → doc): `resolve_project_design_doc` (`products_design.rs:225`), `task_doc_path` (`products_design.rs:560`). There is no `WHERE design_doc_path = …` / `WHERE doc_path = …` query. Building this reverse resolver is the load-bearing new piece of this design (see [Decision model](#the-revision-vs-general-task-decision)).

### The revision substrate (P654)

- `tasks.kind='revision'` + `tasks.parent_task_id` (soft FK); deliverable is **a new commit on the parent's existing PR branch**, no new PR. Create path `WorkDb::create_revision → assert_parent_revisable → insert_revision_in_tx`, gated on the chain root's PR being **Open**. Dispatch produces a `revision_implementation` execution; worker directive `compose_revision_directive` (`runner.rs`) fetches, edits the parent branch, pushes back; a PreToolUse hook hard-blocks `gh pr create`. Lifecycle rides the parent: `mark_chore_pr_merged` flips in-review revisions to `done`; merged-mid-flight → `blocked` + attention.
- `CreateRevisionInput.created_via` already carries arbitrary provenance; P707 established the grammar (`operator` · `pr-comment:<repo>#<pr>:<cid>` · `merge-conflict:<id>` · `ci-fix:<id>`), extended via the single choke point `canonicalize_created_via`.

### PR-merge & terminal-state detection (the reconciliation signals to hook)

- `PrLifecycleState { Open(OpenPrStatus), Merged, ClosedUnmerged }` (`merge_poller.rs:229`), computed by `classify_state` (`merge_poller.rs:1206`) from `gh pr view` JSON. The probe primitive is `MergeProbe::probe(pr_url)` (`merge_poller.rs:430`); there is **no** targeted single-PR "poll now" RPC — the public entrypoint `run_one_pass` (`merge_poller.rs:1334`) is batch-only over candidate lists.
- **Merge →** `mark_merged` (`merge_poller.rs:2532`, from the `Merged` arm at `:1871`) calls `WorkDb::mark_chore_pr_merged` (`pr_flow.rs:504`): sets `status='done'`, cascades dependents, then `flip_in_review_revisions_to_done` and `block_pending_revisions_on_parent_close`. This is exactly the hook point where "resolve the addressed comments" belongs, mirroring how the merge path already fans out to revision bookkeeping.
- **Closed-without-merge is currently a no-op** — the `PrLifecycleState::ClosedUnmerged` arm of `sweep_one` (`merge_poller.rs:1984`) only logs and defers to the unimplemented [`chore-lifecycle-pr-closed-unmerged`](chore-lifecycle-pr-closed-unmerged.md) design (`needs_attention`/`abandoned` statuses, `prior_pr_urls`). The completion detector separately defines `PrStatus::Closed{url}` (`completion.rs:195`) as "don't advance." **This matters:** the "reopen comments on abandon" half of reconciliation has no terminal signal to hang off until that lifecycle work lands (or until we add a minimal close-detection hook). Called out as a dependency in [Risks](#risks--open-questions).
- `TaskStatus` (`types.rs:504`): `{Todo, Active, Blocked, InReview, Done, Archived, Cancelled}`; terminal set `is_terminal()` = `Done | Archived | Cancelled` (`types.rs:530`).
- General-task creation is a first-class RPC: `CreateChore { input: CreateChoreInput }` (`wire.rs:377`) → `handle_create_chore` (`app.rs:2332`) → `WorkDb::create_chore` (`create_entities.rs:161`, `kind='chore'`). This is the "otherwise → general task" vehicle.

## Goals

- On a design/investigation doc with unresolved comments, show a **banner with a live unresolved count and a `[Revise]` action** (thin client — the banner renders engine-computed state and calls one engine RPC; it decides nothing).
- On `[Revise]`, the **engine** creates-and-starts the correct work item: a **revision** against the doc's owning task when that doc is in an **open, unmerged PR**; a **general chore** to update the doc otherwise (PR merged/closed, or no PR).
- Durably **associate** the addressed comments with the spawned work item, so completion can find and reconcile them.
- Transition addressed comments to a new **`in_revision`** state and surface, in the viewer, that they are being worked (with a link to the work item).
- **Reconcile on completion**, hooking existing terminal-state signals (no new GitHub polling): work item's PR **merged** → **resolve** the comments; work item **abandoned/closed-unmerged** → **reopen** them (back to unresolved) so they are never silently lost.
- Keep all decisioning in the **engine** ([[feedback_engine_owns_reconciliation_not_ui]]); reuse the revision gate, `create_chore`, `mark_chore_pr_merged`, and the comment RPCs rather than inventing parallel paths.
- Scope strictly to **design/investigation** docs; every other doc/artifact kind gets no banner and the RPC is a no-op.

## Non-goals

- **Building the comment model or the revision substrate.** Both exist (P529 engine side; P654). This is a producer on top.
- **Wiring the macOS comment UI to the engine RPCs.** That is separate, prerequisite P529 Phase-2 plumbing; this design assumes persisted comments and depends on it.
- **Per-comment magic-wand dispatch** (`CommentsDispatchMagicWand`). That single-comment path stays; this is the doc-level batch counterpart. No convergence/deprecation in v1 (see [open questions](#risks--open-questions)).
- **Threaded replies.** The base comment model is single-level (P529 non-goal). The "reply" surfaced on each comment is an engine-authored **status annotation**, not a new threaded comment row (see [Reply/link mechanics](#replylink-mechanics)).
- **Selecting a subset of comments to address.** v1 addresses _all currently-unresolved_ comments as one batch; subset selection is deferred (open question).
- **A new kanban column or card type.** Comment-triggered revisions render as ordinary revision cards / the general chore renders as an ordinary chore; `created_via` may drive subtle chrome only.
- **Reconciling the existing magic-wand `dispatched` comments.** Only comments this feature moved to `in_revision` are reconciled; extending reconciliation to `dispatched` is future work.
- **Engine-owned (`work_item`) doc revisions via this banner.** Work-item _descriptions_ are not design/investigation docs; they keep the per-comment magic-wand path. The banner is `pr_doc`/design-doc only.
- **Implementing the closed-unmerged lifecycle.** The reopen-on-abandon path _consumes_ the signal that `chore-lifecycle-pr-closed-unmerged` (or a minimal hook) provides; it does not build that lifecycle.

## Alternatives considered

### Alternative A — Fan out one per-comment magic-wand dispatch per unresolved comment

Reuse the built `CommentsDispatchMagicWand` path and, on `[Revise]`, loop it over every unresolved comment.

- **Pros:** zero new dispatch code; each comment already flips to `dispatched` and (for `pr_doc`) spawns a chore.
- **Cons:** N comments → N tasks → N PRs/commits, exactly the noise the brief's single-`[Revise]`-one-task shape avoids. No batch identity to reconcile against. No revision-vs-general routing (magic-wand always makes a chore, so it would open a _fresh_ PR even when the doc is in an open PR that should just get another commit). No completion reconciliation of comment state at all. Rejected — it solves dispatch but not association, routing, or reconciliation, which are the actual asks.

### Alternative B — Decide revision-vs-general in the banner/UI

Have the macOS client read the doc's PR state, decide whether to call `create-revision` or `create_chore`, and drive the comment transitions itself.

- **Pros:** no new engine RPC; the client already knows which doc it is showing.
- **Cons:** violates engine-owns-reconciliation. The UI would need to resolve doc→task→PR-lifecycle (a mapping that doesn't even exist yet) and re-implement the revision gate. It races: the PR can merge between banner render and click, so a UI decision is stale by construction. Comment-state transitions and completion reconciliation must be atomic with task creation, which a multi-RPC client dance cannot guarantee. Rejected.

### Alternative C — One engine RPC that batches, routes, associates, and reconciles (chosen)

A single `CommentsReviseDoc(artifact_kind, artifact_id)` RPC. The engine lists the unresolved comments, resolves the doc's owning task + PR lifecycle via a new reverse resolver, picks revision-vs-general, creates-and-starts the work item, stamps the association + flips comments to `in_revision` **in one transaction**, and reconciles comment state later off the existing merge/terminal signals.

- **Pros:** all decisioning and atomicity live in the engine; the banner is a thin trigger; routing is authoritative at click time; the association is durable and drives reconciliation; reuses `assert_parent_revisable`, `create_revision`, `create_chore`, `mark_chore_pr_merged`, and the comment RPCs. Matches the P707 "producer on the revision substrate" pattern exactly.
- **Cons:** requires the new reverse resolver and a new association column; the reopen-on-abandon half depends on a closed-unmerged terminal signal that isn't wired yet.
- **Chosen.** Detailed below.

For the two sub-decisions inside C, the chosen options (argued in place) are: **association via a column on `work_comments`** (not a join table); and **reply-as-engine-annotation** (not a threaded comment row).

## Chosen approach

### Comment state machine

One new status value, `in_revision`, joins the existing set on `work_comments.status` (`active|resolved|orphaned|dismissed|dispatched`). It is validated in the same place (`comments.rs:91`) and is a first-class status like the others.

```
                    [Revise] batch (CommentsReviseDoc)
   ┌────────┐  addressed & task created, in one tx   ┌──────────────┐
   │ active │ ─────────────────────────────────────▶ │ in_revision  │
   │(unres- │                                         │ (revise_task │
   │ olved) │ ◀───────────────────────────────────── │  _id set)    │
   └────────┘   task ABANDONED / PR closed-unmerged   └──────┬───────┘
        ▲            (reconcile: reopen)                     │
        │                                                    │ task DONE (PR merged)
        │                                                    │ (reconcile: resolve)
        │                                                    ▼
        │                                            ┌──────────────┐
        │  (manual re-activate, existing SetStatus)  │   resolved   │
        └─────────────────────────────────────────  │ (last_resol- │
                                                     │  ved_with=…) │
                                                     └──────────────┘

   orphaned  — anchor lost; excluded from the banner count; never auto-addressed.
   dismissed — user hid it; excluded; never addressed.
   dispatched — the per-comment magic-wand state; untouched by this feature.
```

Transitions, each with its trigger and idempotency rule:

| From          | To            | Trigger                                                   | Who                                                        | Idempotency                                                                                                                            |
| ------------- | ------------- | --------------------------------------------------------- | ---------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `active`      | `in_revision` | `[Revise]` batch addresses this comment                   | engine (`CommentsReviseDoc`)                               | only `active`→`in_revision`; a comment already `in_revision`/`resolved`/`dismissed`/`orphaned` is skipped, so double-Revise is a no-op |
| `in_revision` | `resolved`    | the associated task reaches `done` (its PR merged)        | engine (reconciliation, hooked on the terminal transition) | only `in_revision`→`resolved`; re-firing on an already-`resolved` row is a no-op; sets `last_resolved_with`                            |
| `in_revision` | `active`      | the associated task is abandoned / its PR closed-unmerged | engine (reconciliation)                                    | only `in_revision`→`active`; clears `revise_task_id`; re-firing is a no-op                                                             |
| `in_revision` | `active`      | manual override (reviewer un-dispatches)                  | user (`CommentsSetStatus`)                                 | allowed; clears `revise_task_id`                                                                                                       |

`in_revision` comments are **excluded from the banner's unresolved count** (they are being worked). `resolved`/`dismissed`/`orphaned` are excluded as today. So the banner count = `active` comments only.

### Association model

The batch of addressed comments must be linked to the work item it spawned, durably, so reconciliation can find them from the task side and the sidebar can render the link from the comment side.

**Decision: a soft-FK column on `work_comments`, not a join table.**

```sql
ALTER TABLE work_comments ADD COLUMN revise_task_id TEXT;  -- soft FK → tasks.id;
                                                           -- the revision OR chore that
                                                           -- this comment's revise batch
                                                           -- was dispatched to. NULL unless
                                                           -- status = 'in_revision' (or a
                                                           -- resolved/reopened comment whose
                                                           -- last batch we want to trace).
CREATE INDEX IF NOT EXISTS work_comments_by_revise_task ON work_comments(revise_task_id);
```

- The name is deliberately **`revise_task_id`, not `revision_task_id`** — the target can be a `kind='revision'` _or_ a `kind='chore'` (general-task path). This mirrors P707's `attempt.revision_task_id` reverse-link idea but generalizes for the two-vehicle routing.
- A join table (`comment_revise_batches`) was considered and rejected: a comment belongs to **exactly one** in-flight revise batch at a time (it is `active` or `in_revision`, never both), so the relationship is 1:1 while in flight — a column expresses it without a second table and without a join on the hot reconciliation path. The historical "which batches has this comment been through" question (a comment reopened once and re-revised) is answered by the audit trail on the _task_ side (`created_via` + the tasks themselves), not by mirroring comment history into a link table.
- **Provenance on the task** carries the reverse pointer and the doc-comment origin: `created_via = "doc-comment:<artifact_kind>:<artifact_id>"` (extending `canonicalize_created_via`, the one choke point P707 already uses). This lets the kanban/audit/editorial-controls gate brand the card and lets reconciliation double-check it is reconciling the right comments. `(repo, path)` is recoverable from `artifact_id`; the comment ids are recoverable by `SELECT id FROM work_comments WHERE revise_task_id = ?`.

**Batch scope (which comments one `[Revise]` addresses).** v1: **all comments with `status='active'` on the artifact at click time** (orphaned/dismissed excluded — an orphan has no anchor to revise against; a dismissed comment was intentionally hidden). Subset selection ("address these 3 of 5") is deferred (open question); the RPC signature reserves room for an optional `comment_ids` filter so the subset path is additive later.

**Comments added after a revision is in flight.** They are new `active` rows. They are _not_ swept into the in-flight batch (that batch already has a task and may be mid-run). Instead they contribute to a **fresh** banner count, and a second `[Revise]` spawns a **second** work item addressing just them. If the first work item is still an open revision on the same PR, the second is a **revision-of-revision** (chain-root-numbered `R2`, per P654 OQ2); if the PR merged in between, the second is a general chore. This is the "new batch / new banner" behavior the brief calls for, and it falls out of the state machine for free (only `active` comments are ever addressed).

### The revision-vs-general-task decision

Decided **in the engine, at RPC time**, keyed on the doc's current PR lifecycle. This needs the reverse mapping that doesn't exist today.

**New reverse resolver** `resolve_doc_owner(artifact_kind, artifact_id) -> Option<DocOwner>` where `DocOwner { task_id, task_kind, chain_root_id, pr_url, pr_lifecycle }`:

1. Parse `artifact_id`. For `pr_doc:<repo>:<branch>:<path>`:
   - If `<branch>` is an execution's engine-supplied `expected_branch` (`boss/exec_*`), map branch → execution → task. That task is the doc's owner; read `tasks.pr_url` and its cached PR poll-state.
   - Else (e.g. `<branch> = main` after merge) match `projects WHERE design_doc_repo_remote_url=<repo> AND design_doc_branch=<branch> AND design_doc_path=<path>` → the project's `kind='design'`, `ordinal=0` task; and `tasks WHERE doc_repo_remote_url=<repo> AND doc_branch=<branch> AND doc_path=<path>` → project-less investigation/design tasks. (These are the exact columns from `schema_init.rs:45` and `migrations_b.rs:629`; only the `WHERE` direction is new.)
2. For `artifact_kind='work_item'`: the owner _is_ that work item, but it is not a design/investigation doc — return `None` for banner purposes (scope guard; the per-comment magic wand covers work-item descriptions).
3. Resolve `pr_lifecycle` from the owner's `pr_url` + the poll-state columns the merge poller already maintains (`pr_state_polled_at`, etc.); a present `pr_url` with no terminal marker reads as **Open** (consistent with the revision create-time gate, P654 Q4).

**Decision table** (evaluated engine-side, authoritative at click):

| Owner resolves to                                         | PR lifecycle              | Action                                                                                                                                       |
| --------------------------------------------------------- | ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| design/investigation task                                 | **Open** (unmerged)       | `create_revision(parent = owner's chain root)` — a `revision` that adds a commit to the doc's existing PR. Reuses `assert_parent_revisable`. |
| design/investigation task                                 | **Merged**                | `create_chore` — general task to update the (now-landed) doc; opens a fresh PR against `main`.                                               |
| design/investigation task                                 | **ClosedUnmerged**        | `create_chore` — general task (the old PR is dead; fresh work).                                                                              |
| design/investigation task                                 | **no PR** (`pr_url` NULL) | `create_chore` — general task (nothing to amend).                                                                                            |
| not a design/investigation doc (work_item, or unresolved) | —                         | no-op; RPC returns `NotApplicable`; banner never shown.                                                                                      |

Both vehicles autostart (revisions autostart already; the chore is created with autostart so `[Revise]` truly "creates+starts"). Both carry `created_via = doc-comment:…` and both get the addressed comments stamped with `revise_task_id`.

**Edge cases**, precisely:

- **PR open but a revision already active on it.** The doc already has an in-flight revision (from an earlier batch). A new `[Revise]` on a _fresh_ set of `active` comments creates a **revision-of-revision** (parent = the active revision; chain-root numbering keeps it `R<n>` on the one PR). The gate is evaluated against the chain root's PR (P654 §Q2). The already-`in_revision` comments from the first batch are untouched.
- **PR merged between banner render and click.** The banner is advisory; the engine re-resolves lifecycle at RPC time. If it merged, the branch matches "Merged" → general chore, _not_ a doomed revision. `assert_parent_revisable` is the backstop: even if the resolver read stale state, the revision create refuses a merged parent and the engine falls through to the chore path. No revision is ever created against a merged PR.
- **Doc that never had a PR** (e.g. an investigation whose worker hasn't pushed, or a work-item description). `pr_url` NULL → general chore (or `NotApplicable` for work-item). Never a revision.
- **Two reviewers click nearly simultaneously.** The comment transition `active→in_revision` is done in a single guarded `UPDATE … WHERE status='active'`; the second click finds no `active` comments left, addresses an empty batch, and returns the first click's task handle (idempotent; see [Concurrency](#concurrencyidempotency)).

### Engine RPC surface

One new request/response pair, following the `Comments*` conventions:

```rust
// protocol/src/wire.rs — sibling of the other Comments* variants.
CommentsReviseDoc {
    request_id: String,
    input: ReviseDocInput,
}

// protocol/src/types.rs
pub struct ReviseDocInput {
    pub artifact_kind: String,          // "pr_doc" (v1); "work_item" → NotApplicable
    pub artifact_id: String,            // pr_doc:<repo>:<branch>:<path>
    pub comment_ids: Option<Vec<String>>, // v1: None ⇒ all active; reserved for subset
}

// Response (protocol/src/wire.rs response side)
ReviseDocResult(ReviseDocOutcome)

pub enum ReviseDocOutcome {
    Created {
        task_id: String,               // the revision or chore
        task_kind: String,             // "revision" | "chore"
        addressed_comment_ids: Vec<String>,
        pr_url: Option<String>,        // parent PR (revision) or None yet (chore)
    },
    NoUnresolvedComments,              // nothing active to address (idempotent no-op)
    AlreadyInFlight { task_id: String },// a prior batch already claimed all active comments
    NotApplicable { reason: String },  // not a design/investigation doc
}
```

Handler `handle_comments_revise_doc` (`engine/core/src/app/comments.rs`), doing, **in one transaction** where it touches comment rows:

1. `resolve_doc_owner(artifact_kind, artifact_id)`; if `None`/not design-or-investigation → `NotApplicable`.
2. Load `active` comments (or the `comment_ids ∩ active`); if empty → `NoUnresolvedComments`.
3. Apply the decision table → build a `create_revision` or `create_chore` call. For a revision, `assert_parent_revisable(chain_root)`; on gate failure (raced merge) fall through to the chore branch.
4. Create the work item (reusing `WorkDb::create_revision` / `create_chore`) with `created_via="doc-comment:<kind>:<artifact_id>"`, autostart on.
5. `UPDATE work_comments SET status='in_revision', revise_task_id=<task>, status_actor='engine', updated_at=now WHERE artifact_...=... AND status='active' [AND id IN (...)]`.
6. Publish on `comment_topic(artifact)` so viewers refetch; return `Created{…}`.

**Reconciliation (engine-owned, hooked on existing terminal signals — no new poller):** a single idempotent helper

```rust
fn reconcile_comments_for_task(tx, task_id, outcome: {Resolved | Reopened})
```

- `Resolved`: `UPDATE work_comments SET status='resolved', last_resolved_with='revise:<task_id>', status_actor='engine' WHERE revise_task_id=? AND status='in_revision'`.
- `Reopened`: `UPDATE … SET status='active', revise_task_id=NULL, status_actor='engine' WHERE revise_task_id=? AND status='in_revision'`.

Called from every place a task reaches a terminal state:

- **Merged → Resolved.** Inside `mark_chore_pr_merged` (`pr_flow.rs:504`), after the existing cascade — the same transaction that already calls `flip_in_review_revisions_to_done`. For a **revision** vehicle the addressed comments' `revise_task_id` points at the revision, and the revision reaches `done` via `flip_in_review_revisions_to_done` when the **chain root** (the doc's PR owner) merges — so `reconcile_comments_for_task` is invoked for _each_ revision flipped to done in that fan-out, plus the chain root itself. For a **chore** vehicle, the chore's own `mark_chore_pr_merged` fires it directly. Uniform: whenever a task with addressed comments reaches `done`, its comments resolve.
- **Abandoned/closed-unmerged → Reopened.** Hooked on the task's transition to a non-merged terminal state (`abandoned`, or the `needs_attention` disposition from `chore-lifecycle-pr-closed-unmerged`, or `cancelled`). **This signal is not wired today** (the poller's `ClosedUnmerged` arm is a no-op, `merge_poller.rs:1984`). v1 options: (i) depend on the closed-unmerged lifecycle landing and hook its transition; or (ii) add a minimal comment-only hook in the `ClosedUnmerged` arm that reopens comments without implementing the full status lifecycle. Recommended: (i) if that work is near; (ii) as a small stopgap otherwise. Flagged as the primary open dependency.

All decisioning stays engine-side; the UI only reads resulting comment state via the existing `CommentsList` + `comment_topic` subscription.

### Reply/link mechanics

The brief asks to "post a reply on each comment linking to the created revision/task." The base comment model is single-level (no threading — a P529 non-goal), so a literal reply would require introducing threading.

**Decision: the "reply" is an engine-authored status annotation carried on the comment itself, not a new comment row.** The `in_revision` status + `revise_task_id` render in the sidebar as an inline chip under the addressed comment: **`⟳ In revision → R2 · T712 ↗`** (revision) or **`⟳ Update task → T900 ↗`** (chore), linking to the work-item card / its PR. When the comment later resolves or reopens, the chip updates (`✓ Resolved in #1234` / `↩ Reopened — revision abandoned`). This is the reply "surfaced back in the viewer" with zero new schema beyond `revise_task_id` and no threading.

- **On the GitHub side** (PR-backed docs), the reviewer-facing breadcrumb is the revision worker's existing `[boss-revision] R<n>: <description>` PR comment (P654 OQ6, gated by editorial-controls). For the general-chore vehicle, the fresh PR's description references the addressed comments. No new GitHub-comment mechanism is introduced.
- A literal threaded reply comment row (`author='boss:comment_revise:<task>'`, same anchor) is the **alternative**; it would need single-level threading in the sidebar renderer and is listed as an open question. The annotation approach is preferred because it needs no model change and cannot drift from the comment's status.

### UI / banner behavior (thin client)

- The engine exposes banner state as part of the existing comment load: extend the `CommentsList` response (or add a tiny `CommentsBannerState` read) with `{ revisable: bool, unresolved_count: int, in_revision_count: int, doc_kind: "design"|"investigation"|null }`. `revisable` is true iff `resolve_doc_owner` yields a design/investigation task. **All eligibility logic is engine-side.**
- `DesignRendererView` / `MarkdownViewerView` render a banner above the doc when `revisable && unresolved_count > 0`: `"{unresolved_count} unresolved comment(s). [Revise]"`. The button calls `CommentsReviseDoc(artifact_kind, artifact_id)` and disables while in flight.
- On the `comment_topic` push that follows, the client refetches; the count drops (addressed comments left `active`), the `in_revision` chips appear. If the RPC returns `NotApplicable`/`NoUnresolvedComments`, the banner hides. No decisioning, no PR-state knowledge, no task-creation logic in the client.

### Concurrency/idempotency

- **Double-click / rapid re-Revise.** The `active→in_revision` flip is a single guarded `UPDATE … WHERE status='active'` inside the create transaction. The first call claims the comments and creates the task; a racing second call finds zero `active` comments, creates nothing, and returns `AlreadyInFlight{task_id}` (looked up via `revise_task_id` of the just-claimed comments). No duplicate task, no double-charged batch.
- **Multiple reviewers.** Same mechanism: whoever commits the `UPDATE` first wins the batch; the other addresses only comments still `active` (e.g. ones added since), yielding a legitimately separate batch/task.
- **Comments spanning multiple revisions.** Each comment carries exactly one `revise_task_id` while `in_revision`; reconciliation keys on it. A reopened comment clears `revise_task_id` and can be re-revised into a new batch — never double-counted.
- **A revision that spawns its own follow-up revisions.** Chain-root numbering (P654 OQ2) governs the tasks; comments stay bound to the specific batch task that addressed them. When the chain root's PR merges, `flip_in_review_revisions_to_done` walks the chain and `reconcile_comments_for_task` resolves each revision's comments — so comments across multiple revisions on one PR all resolve together at merge.
- **Reconciliation idempotency.** Both `Resolved` and `Reopened` updates are guarded on `status='in_revision'`, and `mark_chore_pr_merged` is itself idempotent (guards done/archived rows). Re-running a sweep never double-transitions.

### Scope guard

The feature is gated to **design/investigation** docs at the one authoritative point: `resolve_doc_owner` returns an owner only when the resolved task's `kind ∈ {Design, Investigation}`. Consequences:

- `artifact_kind='work_item'` (work-item descriptions) → `None` → no banner, `CommentsReviseDoc` returns `NotApplicable`. Those keep the per-comment magic wand.
- A `pr_doc` whose owner is a plain `chore`/`project_task` PR (code, not a design doc) → `None` → no banner. (Commenting on code-PR docs is out of scope; the banner simply never appears.)
- Detection is engine-side and derived from `tasks.kind`, never sniffed in the UI.

### Relationship to the magic wand

The existing `CommentsDispatchMagicWand` (per-comment) and this `CommentsReviseDoc` (per-doc batch) overlap in spirit but differ in three ways: **granularity** (one comment vs the unresolved batch), **routing** (magic wand always makes a chore / isolated-Claude; this routes revision-vs-general on live PR state), and **reconciliation** (magic wand leaves the comment `dispatched` with no completion loop; this resolves/reopens on terminal state). They coexist in v1. A future convergence — making the magic wand a `comment_ids=[one]` call into this path and giving `dispatched` the same reconciliation — is noted in the task breakdown as `future / not a v1 blocker`.

## Risks / open questions

- **Reopen-on-abandon has no terminal signal today.** The poller's `ClosedUnmerged` arm is a no-op (`merge_poller.rs:1984`); `chore-lifecycle-pr-closed-unmerged` is unimplemented. Until a closed/abandoned terminal transition exists, comments addressed by a rejected revision/chore would sit at `in_revision` indefinitely. _Mitigation / decision needed:_ depend on that lifecycle work, or add a minimal comment-only reopen hook in the `ClosedUnmerged` arm now. (Question in manifest.)
- **Revision on a _design_ task exercises the revision machinery on a `project_design`-origin PR.** Revisions were designed against chore PRs; a design-doc PR is still "a PR with a branch," and `compose_revision_directive` is doc-agnostic (fetch → edit → push, no new PR), so it should work — but this is the first use of revisions on a design-kind parent and wants an explicit end-to-end test. _Mitigation:_ covered in the dispatch phase's acceptance test.
- **The reverse resolver is a new trust path.** `resolve_doc_owner` infers owner from branch/path; a mismatch (e.g. a doc moved between branches, or two projects sharing a path) could mis-route. _Mitigation:_ prefer the execution-branch mapping (exact) over the path match; when the path match is ambiguous (>1 project), return `None` (no banner) rather than guess, and log for diagnosis.
- **Batch-vs-subset.** v1 addresses all unresolved comments. If reviewers routinely want "revise these, not those," the all-batch default forces them to dismiss the rest first. _Open question:_ ship subset selection in v1 or defer? (Manifest.)
- **Reply representation.** Engine annotation (recommended, no threading) vs a literal threaded reply comment. _Open question_ — the latter is more "chat-like" but needs threading the base model deliberately omitted. (Manifest.)
- **Magic-wand convergence.** Keep two paths (per-comment magic wand + per-doc revise) or converge? Deferred, but a v1 decision to _not_ converge should be explicit so the surfaces don't drift. (Manifest.)
- **General-chore doc-edit directive quality.** The general chore must be told precisely which file + which comments to address so it edits the right doc and opens a coherent PR. _Mitigation:_ the chore description encodes the doc path, the quoted anchors, and each comment body, mirroring the magic-wand PR-backed chore directive (P529 §"PR-backed doc → Boss chore worker").
- **Count semantics for orphaned comments.** Orphaned comments (anchor lost) are excluded from the banner count and never auto-addressed. If a doc's comments all orphan after a big edit, the banner disappears even though feedback exists. _Mitigation:_ surface orphans in the sidebar's existing "anchor lost" affordance; a future "re-attach then revise" flow is out of scope.

## Proposed implementation task breakdown

PR-sized tasks in dependency order. Effort hints: `trivial | small | medium | large`. Tasks at the same depth with no edge between them may run in parallel (noted). This section is the machine-findable handoff to scheduling.

### 1. Schema + status + provenance (foundation)

**Scope:** Add the `in_revision` value to the valid `work_comments.status` set (`comments.rs` validation) and the `work_comments.revise_task_id TEXT` column + `work_comments_by_revise_task` index (migration mirroring `migrate_work_comments_table`). Extend `canonicalize_created_via` to accept the `doc-comment:<kind>:<artifact_id>` grammar. No behavior yet — status and column exist, nothing writes them. **Effort:** `small`. **Dependencies:** none.

### 2. Reverse doc→owner resolver

**Scope:** Implement `resolve_doc_owner(artifact_kind, artifact_id) -> Option<DocOwner>`: parse `pr_doc:<repo>:<branch>:<path>`, map `boss/exec_*` branch → execution → task (exact), else match `projects.design_doc_*` / `tasks.doc_*` columns (path match, ambiguity → `None`), read `pr_url` + poll-state into a `PrLifecycle`, gate on `task.kind ∈ {Design, Investigation}`. Pure lookup + unit tests over seeded rows. **Effort:** `medium`. **Dependencies:** none (parallel with task 1).

### 3. `CommentsReviseDoc` RPC + routing + comment transition

**Scope:** Add the `CommentsReviseDoc` wire request / `ReviseDocResult` response and `ReviseDocInput`/`ReviseDocOutcome` types; `handle_comments_revise_doc` that runs the decision table (revision via `create_revision`+`assert_parent_revisable`, else `create_chore`), autostarts the task with `created_via=doc-comment:…`, and flips addressed `active` comments to `in_revision` with `revise_task_id` in one transaction; publishes on `comment_topic`. Includes the general-chore doc-edit directive (path + anchors + bodies). **Effort:** `medium`. **Dependencies:** tasks 1, 2.

### 4. Completion reconciliation — resolve on merge

**Scope:** Add idempotent `reconcile_comments_for_task(tx, task_id, Resolved)` and call it from `mark_chore_pr_merged` (`pr_flow.rs:504`) after the existing cascade, and from within the `flip_in_review_revisions_to_done` fan-out so revision-vehicle comments resolve when the chain root merges. End-to-end test: revision path (comment → revision → parent PR merge → comment resolved) and chore path (comment → chore → its PR merge → comment resolved). **Effort:** `medium`. **Dependencies:** task 3.

### 5. Completion reconciliation — reopen on abandon

**Scope:** Add the `Reopened` arm and hook it to the task's non-merged terminal transition. If `chore-lifecycle-pr-closed-unmerged` has landed, hook its `needs_attention`/`abandoned` transition; otherwise add a minimal comment-only reopen in the poller's `ClosedUnmerged` arm (`merge_poller.rs:1984`). Test: comment → task → PR closed unmerged → comment back to `active`, `revise_task_id` cleared, banner reappears. **Effort:** `small` (if hooking an existing signal) / `medium` (if adding the minimal close hook). **Dependencies:** task 3; **soft-depends** on `chore-lifecycle-pr-closed-unmerged` (P-level, external) — see open questions.

### 6. Banner state on the comment read path

**Scope:** Extend the `CommentsList` response (or add `CommentsBannerState`) with `{revisable, unresolved_count, in_revision_count, doc_kind}`, computed from `resolve_doc_owner` + a status count. Engine-only; no UI. **Effort:** `small`. **Dependencies:** task 2. May run in parallel with tasks 3–5.

### 7. macOS banner + `in_revision` chip (thin client)

**Scope:** Render the `{n} unresolved comments. [Revise]` banner in `DesignRendererView`/`MarkdownViewerView` when `revisable && unresolved_count>0`; wire the button to `CommentsReviseDoc`; render the `⟳ In revision → …` / `✓ Resolved` / `↩ Reopened` chips from comment status + `revise_task_id`; refetch on `comment_topic`. **Depends on the P529 Phase-2 UI-persistence plumbing** (comments must persist through the engine RPCs first). **Effort:** `medium`. **Dependencies:** tasks 3, 6; **external prerequisite:** P529 UI persistence.

### 8. Editorial-controls & kanban chrome polish

**Scope:** Ensure `created_via=doc-comment:*` revisions/chores render with an appropriate badge; consult the editorial-controls gate for any GitHub-visible breadcrumb (reusing P654 OQ6 / P576). **Effort:** `small`. **Dependencies:** task 3. Parallel with task 7.

### Deferred / future (not a v1 blocker)

- **Subset selection** — let `[Revise]` address a chosen subset via the reserved `comment_ids` filter. `future / not a v1 blocker`. Depends on task 3.
- **Threaded reply comment rows** — replace the annotation chip with a real single-level threaded reply, if usage demands. `future / not a v1 blocker`. Depends on task 3 + threading in the base comment model.
- **Magic-wand convergence** — make `CommentsDispatchMagicWand` a `comment_ids=[one]` call into this path and give `dispatched` the same reconciliation. `future / not a v1 blocker`. Depends on tasks 3–5.
- **Reconcile existing `dispatched` comments** — extend reconciliation beyond `in_revision`. `future / not a v1 blocker`.

## Related designs

- [`comments-in-markdown-viewer`](comments-in-markdown-viewer.md) (P529) — the comment model, statuses, anchoring, `pr_doc` artifact keys, and the per-comment magic-wand path this builds on.
- [`revision-tasks`](revision-tasks.md) (P654) — the revision substrate, `create_revision`, the gate, chain-root numbering, and lifecycle this feature produces onto.
- [`unify-pr-remediation-on-revisions`](unify-pr-remediation-on-revisions.md) (P707) — the "producer on the revision substrate" pattern and `created_via` provenance grammar this feature is a new instance of.
- [`chore-lifecycle-pr-closed-unmerged`](chore-lifecycle-pr-closed-unmerged.md) — the closed-unmerged terminal signal the reopen-on-abandon path depends on.
- [`design-producing-tasks`](design-producing-tasks.md) / [`auto-populate-project-tasks-on-design-pr-merge`](auto-populate-project-tasks-on-design-pr-merge.md) — how design docs become PR-backed and how `mark_chore_pr_merged` / `on_design_pr_merged` fire, the merge signals this hooks.
- [`project-design-doc-pointer`](project-design-doc-pointer.md) — the `design_doc_path` pointer used by the reverse resolver.
