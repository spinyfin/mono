# Boss: Comment Intent Classification & Handling

> **Supersedes** the original narrower scope of this design ("larger change → revision/update task" only). Restructured around a **three-intent comment model**: every new comment on a design/investigation doc is classified by an LLM into exactly one of **directive**, **question**, or **larger change**, and routed accordingly. Buckets 1 and 3 (directive, larger-change) share one handling path — the revision/update-task machinery below — and **replace the per-comment "magic wand" entirely** (see [Migration](#migration-retiring-the-magic-wand)). Bucket 2 (question) is new: a read-only mini-coordinator agent answers in the comment thread. The classifier (bucket 1) is the prerequisite piece — nothing else in this design can run before every comment has an intent.

Extend the in-app markdown-viewer comment system so that, for **design** and **investigation** documents, every comment triggers engine-owned handling appropriate to what the operator actually wants: a nudge to start a revision (directive / larger-change), or an answer in the thread (question). This closes the loop between reviewing a doc and either getting it changed or getting it explained — today only the change path partially exists (via the magic wand, which this retires) and the question path has **zero support**.

This design is a **thin producer on top of three mechanisms**: the comment model from [`comments-in-markdown-viewer`](comments-in-markdown-viewer.md) (P529), the revision substrate from [`revision-tasks`](revision-tasks.md) (P654), and — new in this revision — an **LLM-backed intent classifier** and a **read-only answer agent**, both engine-owned. In the vocabulary of [`unify-pr-remediation-on-revisions`](unify-pr-remediation-on-revisions.md) (P707), the directive/larger-change path is a new **Source** — an in-app "doc-comment" producer alongside the operator (Source A), deferred GitHub PR-comment triage (Source B), merge-conflict (Source C), and CI-fix (Source D) producers. The question path is not a P707 Source at all — it never creates a task; it is a self-contained read-only conversation loop.

## Why this is a rewrite, not an extension

The original design assumed every comment implied a wanted change and asked one question ("does this doc have unresolved comments that should trigger a revision batch?"). That's still true for two of the three real cases, but it silently had no model for the most common one: an operator asking "why did you choose X over Y?" or "what does this mean?" is not asking for an edit, and forcing it through the revise-or-ignore banner is wrong on both branches — reviser churns on a comment with no actionable change, ignore loses the question entirely. The classifier is the missing piece that makes routing correct instead of assumed. Everything from the original design (comment state machine additions, association model, the revision-vs-chore decision, reconciliation) survives, relabeled as the **1&3 branch** of a three-way router; the magic wand — a hold-over single-comment path this design was originally going to leave coexisting — is now explicitly removed, because the classifier supersedes its reason to exist (bucket 1&3 nudges toward the same revision path the magic wand used to auto-apply into, and bucket 2 covers what the magic wand never could).

## Current-state grounding

Line numbers are as of writing and will drift; the module/function names are the durable anchors. The engine crate root is `tools/boss/engine/core/` (the `core` crate), protocol is `tools/boss/protocol/`, macOS app is `tools/boss/app-macos/`.

### Comment model (P529 — engine implemented, UI Phase-1 in-memory)

- Table `work_comments` — DDL in `engine/core/src/work/migrations_b.rs:1072` (`migrate_work_comments_table`), invoked from `schema_init.rs:306`. Columns: `id, artifact_kind, artifact_id, doc_version, anchor_json, body, author, status, status_actor, last_resolved_with, plain_text_projection_version, created_at, updated_at, dismissed_at`, plus index `work_comments_by_artifact`.
- Status values (`protocol/src/types.rs:863`): `active`, `resolved`, `orphaned`, `dismissed`, `dispatched`; validated in `engine/core/src/work/comments.rs:91`. `active` is the default (`default_comment_status`, `types.rs:878`). This design adds `in_revision`, `answering`, `answered`, `awaiting_followup` (see [Comment/thread state machine](#commentthread-state-machine)) and retires `dispatched` (see [Migration](#migration-retiring-the-magic-wand)).
- `artifact_kind ∈ {work_item, pr_doc}`; `artifact_id` is either a work-item id (engine-owned description) or the synthetic `pr_doc:<repo_remote_url>:<branch>:<path>` for a markdown file on a PR branch.
- Structs: `WorkComment` (`types.rs:3103`), `CommentAnchor` (`types.rs:835`, a `{exact, prefix, suffix}` W3C `TextQuoteSelector`), `CommentResolution` (`types.rs:887`), `CreateCommentInput` (`types.rs:1176`).
- RPCs (`protocol/src/wire.rs:260`): `CommentsCreate`, `CommentsList`, `CommentsResolve`, `CommentsDismiss`, `CommentsSetStatus`, `CommentsUpdateAnchor`, `CommentsDispatchMagicWand`, `CommentsApplyMagicWand`, `CommentsDiscardMagicWand`. Handlers in `engine/core/src/app/comments.rs`; routed at `app.rs:2311`. Subscription helper `comment_topic()` (`wire.rs:58`). The three `*MagicWand` RPCs are **removed** by this design (see [Migration](#migration-retiring-the-magic-wand)); `CommentsClassify` (implicit, see [Classifier](#the-classifier-p1-foundation)) and `CommentsReviseDoc` / answer-agent RPCs are added.
- A sibling table keyed to comments already exists: `magic_wand_dispatches` (DDL `migrations_b.rs:1017`, `id, comment_id, artifact_kind, artifact_id, doc_version, status, input_tokens, output_tokens, result_md, error_kind, anchor_warning, created_at, resolved_at`, plus `chore_id` added at `migrations_b.rs:1047`; index `magic_wand_dispatches_by_comment`). This table and its handlers (`handle_comments_dispatch_magic_wand` / `_apply_magic_wand` / `_discard_magic_wand`, `engine/core/src/app/comments.rs:251,611,659`, backed by `crate::magic_wand::dispatch`, `magic_wand.rs`) are **retired** by this design, not preserved alongside the new routing (see [Migration](#migration-retiring-the-magic-wand)).
- The macOS UI (`app-macos/Sources/Comments/`: `CommentLayer`, `CommentSidebar`, `CommentHighlightOverlay`, `CommentPopover`, `MagicWandResultSheet`, wired into `DesignsView.swift` and `DesignRendererView.swift` via `.withComments()`) is **Phase-1 in-memory only** — `CommentLayer.swift:19` states comments do not yet persist through the engine RPCs. Persisting the UI against the (already-built) RPCs is prerequisite plumbing this design depends on, not part of it. `MagicWandResultSheet` is deleted as part of the magic-wand removal; its role (showing an applied/staged diff) has no successor in v1 — bucket 1&3 never auto-applies, it only nudges (see [Buckets 1 & 3](#buckets-1--3--unified-directive--larger-change)).

### How a doc maps to its owning work item and PR

- Every task carries `tasks.pr_url` (`schema_init.rs:62`) — the single source of the owning PR for **any** kind.
- The doc-location pointer lives on the **project** for design tasks: `projects.design_doc_repo_remote_url / design_doc_branch / design_doc_path` (`schema_init.rs:45`, migration `migrate_project_design_doc_columns`, `migrations_b.rs:81`). CLI `boss project set-design-doc` (`cli/src/main.rs:296`, input `SetProjectDesignDocInput`, `types.rs:2619`, store `WorkDb::set_project_design_doc`, `products_design.rs:159`).
- Project-less items (investigations, project-less design tasks) carry a per-task doc triple `tasks.doc_repo_remote_url / doc_branch / doc_path` (`migrate_tasks_doc_pointer_columns`, `migrations_b.rs:629`).
- Every project has exactly one `kind='design'` task at `ordinal=0`, dispatched with execution kind `project_design` (this very task). The worker writes `tools/boss/docs/designs/<slug>.md`, opens a normal PR (`compose_design_directive`, `runner.rs:1262`). `active → in_review` is `WorkerCompletion::finalize_pr_transition` (`completion.rs:2637`), which writes `tasks.pr_url` (`pr_flow.rs:96`); routing at `completion.rs:2847` sends `kind==Design && project_id.is_some()` rows to `design_detector::on_design_pr_detected` (`design_detector.rs:58`), which scans the PR's changed files for a single `docs/designs/*.md` off the **head** branch (so the doc is fetchable while the PR is open) and records the pointer. `TaskKind` enum: `{Chore, Design, …, Investigation}` (`types.rs:2633`).
- **The reverse mapping — doc path → owning task → PR state — does not exist.** Every lookup today is forward (task id → doc): `resolve_project_design_doc` (`products_design.rs:225`), `task_doc_path` (`products_design.rs:560`). There is no `WHERE design_doc_path = …` / `WHERE doc_path = …` query. Building this reverse resolver is a load-bearing piece of both the classifier's context assembly and buckets 1&3's routing (see [Decision model](#the-revision-vs-general-task-decision)).

### The revision substrate (P654)

- `tasks.kind='revision'` + `tasks.parent_task_id` (soft FK); deliverable is **a new commit on the parent's existing PR branch**, no new PR. Create path `WorkDb::create_revision → assert_parent_revisable → insert_revision_in_tx`, gated on the chain root's PR being **Open**. Dispatch produces a `revision_implementation` execution; worker directive `compose_revision_directive` (`runner.rs`) fetches, edits the parent branch, pushes back; a PreToolUse hook hard-blocks `gh pr create`. Lifecycle rides the parent: `mark_chore_pr_merged` flips in-review revisions to `done`; merged-mid-flight → `blocked` + attention.
- `CreateRevisionInput.created_via` already carries arbitrary provenance; P707 established the grammar (`operator` · `pr-comment:<repo>#<pr>:<cid>` · `merge-conflict:<id>` · `ci-fix:<id>`), extended via the single choke point `canonicalize_created_via`.

### PR-merge & terminal-state detection (the reconciliation signals to hook)

- `PrLifecycleState { Open(OpenPrStatus), Merged, ClosedUnmerged }` (`merge_poller.rs:229`), computed by `classify_state` (`merge_poller.rs:1206`) from `gh pr view` JSON. The probe primitive is `MergeProbe::probe(pr_url)` (`merge_poller.rs:430`); there is **no** targeted single-PR "poll now" RPC — the public entrypoint `run_one_pass` (`merge_poller.rs:1334`) is batch-only over candidate lists.
- **Merge →** `mark_merged` (`merge_poller.rs:2532`, from the `Merged` arm at `:1871`) calls `WorkDb::mark_chore_pr_merged` (`pr_flow.rs:504`): sets `status='done'`, cascades dependents, then `flip_in_review_revisions_to_done` and `block_pending_revisions_on_parent_close`. This is exactly the hook point where "resolve the addressed comments" belongs, mirroring how the merge path already fans out to revision bookkeeping.
- **Closed-without-merge is currently a no-op** — the `PrLifecycleState::ClosedUnmerged` arm of `sweep_one` (`merge_poller.rs:1984`) only logs and defers to the unimplemented [`chore-lifecycle-pr-closed-unmerged`](chore-lifecycle-pr-closed-unmerged.md) design (`needs_attention`/`abandoned` statuses, `prior_pr_urls`). The completion detector separately defines `PrStatus::Closed{url}` (`completion.rs:195`) as "don't advance." **This matters:** the "reopen comments on abandon" half of buckets-1&3 reconciliation has no terminal signal to hang off until that lifecycle work lands (or until we add a minimal close-detection hook). Called out as a dependency in [Risks](#risks--open-questions).
- `TaskStatus` (`types.rs:504`): `{Todo, Active, Blocked, InReview, Done, Archived, Cancelled}`; terminal set `is_terminal()` = `Done | Archived | Cancelled` (`types.rs:530`).
- General-task creation is a first-class RPC: `CreateChore { input: CreateChoreInput }` (`wire.rs:377`) → `handle_create_chore` (`app.rs:2332`) → `WorkDb::create_chore` (`create_entities.rs:161`, `kind='chore'`). This is the "otherwise → general task" vehicle.

### Coordinator visibility surface (what the bucket-2 agent reuses)

- The Boss coordinator's read surface over products/projects/tasks/PRs/engine state is the same query layer this design's read-only answer agent is built on (see [Bucket 2](#bucket-2--question-read-only-mini-coordinator-answer-agent)); no new read APIs are invented, only a capability-restricted execution mode that can call them.
- Workspace leasing (`cube workspace lease`) is the existing mechanism ordinary workers use to get a checked-out repo to investigate; the answer agent reuses it read-only (leases, never pushes, never opens a PR).

## Goals

- **Classify every new top-level comment** on a design/investigation doc into exactly one of **directive**, **question**, **larger-change**, via an engine-owned LLM call, before any routing happens.
- **Buckets 1 & 3 (directive / larger-change):** on classification, immediately post an engine-authored reply in the thread nudging the operator to start a revision, and expose the existing `[Revise]`-style banner/action; reuse the create-revision/create-chore machinery, comment↔task association, and completion reconciliation designed below. **No auto-apply** — this replaces the magic wand's silent-apply behavior with an explicit nudge.
- **Bucket 2 (question):** spawn a read-only mini-coordinator answer agent, scoped to the document + comment + thread, that can read anything the Boss coordinator can see and lease workspaces to investigate code, but cannot write anything; show a thinking indicator while it works; post a comprehensive reply (which may propose but not apply edits) when done.
- **Thread conversation loop:** a bucket-2 reply's follow-up is itself classified — another question re-enters bucket 2 with thread context; a request for changes routes into the bucket 1&3 path, bridging a proposed edit from an answer into a revision.
- Durably **associate** the addressed comments with whatever the routing spawned (a task for 1&3, an answer-agent run for 2), so completion/answer-done can find and reconcile/update them.
- Keep all decisioning in the **engine** ([[feedback_engine_owns_reconciliation_not_ui]]); the markdown viewer stays a thin renderer of thread state (classification badge, thinking indicator, banner, chips) and never classifies, routes, or decides anything itself.
- **Remove the magic wand** (`CommentsDispatchMagicWand`/`ApplyMagicWand`/`DiscardMagicWand`, `magic_wand_dispatches` table, `magic_wand.rs`, `MagicWandResultSheet`) — see [Migration](#migration-retiring-the-magic-wand) for how existing persisted state is retired safely.
- Scope strictly to **design/investigation** docs; every other doc/artifact kind gets no banner, no classification, and all new RPCs are a no-op.

## Non-goals

- **Building the comment model or the revision substrate.** Both exist (P529 engine side; P654). This is a producer on top, plus a new classifier and a new read-only agent kind.
- **Wiring the macOS comment UI to the engine RPCs.** That is separate, prerequisite P529 Phase-2 plumbing; this design assumes persisted comments and depends on it.
- **Threaded replies as a general comment-model feature.** The base comment model is single-level (P529 non-goal). Both the 1&3 nudge and the bucket-2 agent's replies are engine-authored **thread entries** on the existing comment (see [Reply/link mechanics](#replylink-mechanics)) — this design adds just enough "conversation" shape (an ordered list of engine/operator turns per comment) to support classify → reply → follow-up → reclassify, not a general-purpose threading model for user-authored side conversations.
- **Selecting a subset of comments to address in the 1&3 path.** v1 addresses _all currently-unaddressed, directive/larger-change-classified_ comments as one batch per `[Revise]` click; subset selection is deferred (open question, unchanged from the original design).
- **A new kanban column or card type.** Bucket 1&3 work renders as ordinary revision cards / ordinary chores; `created_via` may drive subtle chrome only.
- **The bucket-2 agent taking any write action.** It cannot create tasks, open PRs, mutate comment/task state beyond posting its own reply, or push code. It can lease a workspace to _read_ code but never commits or pushes from it. This boundary is load-bearing and detailed in [Bucket 2](#bucket-2--question-read-only-mini-coordinator-answer-agent).
- **Engine-owned (`work_item`) doc revisions/questions via this feature.** Work-item _descriptions_ are not design/investigation docs and are out of scope for classification and both bucket paths; this was previously the magic wand's exclusive territory and now has no comment-driven affordance at all (call-out in [Migration](#migration-retiring-the-magic-wand)).
- **Implementing the closed-unmerged lifecycle.** The reopen-on-abandon path _consumes_ the signal that `chore-lifecycle-pr-closed-unmerged` (or a minimal hook) provides; it does not build that lifecycle.

## Alternatives considered

### Alternative A — Fan out one per-comment magic-wand dispatch per unresolved comment

Keep the built `CommentsDispatchMagicWand` path unconditionally and, for larger-change comments, loop it over every unresolved comment.

- **Pros:** zero new dispatch code for that one bucket.
- **Cons:** N comments → N tasks → N PRs/commits — the noise the batch `[Revise]` shape avoids. No batch identity to reconcile against. No revision-vs-general routing. No completion reconciliation. And critically: it has **no answer to bucket 2 at all** — a "why did you do X" comment would silently auto-apply an edit nobody asked for, which is worse than doing nothing. This is the core reason the magic wand is retired outright rather than kept as a fourth path: it cannot distinguish intents, so every comment routed through it is either over-applied (questions) or under-specified (batches). Rejected.

### Alternative B — Decide intent and revision-vs-general in the banner/UI

Have the macOS client read the doc's PR state and comment text, guess the intent client-side, and drive comment transitions itself.

- **Pros:** no new engine RPC.
- **Cons:** violates engine-owns-reconciliation; the UI has no LLM access model of its own, no read-only sandboxing for a would-be local answer agent, and races on PR state exactly as in the original design. Intent classification is inherently probabilistic and needs a durable override/audit trail (`classified_as`, `classified_by`, reclassification), which belongs in engine-owned rows, not client state. Rejected.

### Alternative C — One engine-owned pipeline: classify → route → (batch-revise | answer-agent) (chosen)

A `CommentsClassify` step runs (engine-triggered, see [Classifier](#the-classifier-p1-foundation)) on every new top-level comment, writing `work_comments.intent` + `intent_confidence` + `classified_at`. Routing branches on `intent`:

- `directive` / `larger_change` → the same `CommentsReviseDoc` batch RPC as before (renamed conceptually to "the 1&3 path"), which lists intent-eligible unaddressed comments, resolves the doc's owner + PR lifecycle, creates-and-starts the right work item, stamps association, flips comments to `in_revision`, posts the nudge reply — all in one transaction where it touches comment rows.
- `question` → a new `CommentsSpawnAnswerAgent` RPC spawns the read-only mini-coordinator, flips the comment to `answering`, and on completion posts the agent's reply and flips to `answered`.

Both share the underlying comment/thread state machine and reconciliation infrastructure; only the routing target differs.

- **Pros:** all decisioning and atomicity live in the engine; the client renders thread state and never classifies or routes; the classifier is a single foundational choke point everything else depends on; reuses `assert_parent_revisable`, `create_revision`, `create_chore`, `mark_chore_pr_merged`, and the comment RPCs; the bucket-2 path is additive and doesn't disturb 1&3's already-designed mechanics.
- **Cons:** requires the new classifier (latency, cost, misclassification handling) and the new read-only agent execution mode (a capability boundary that must be enforced, not just documented) in addition to the reverse resolver and association column the original design already needed.
- **Chosen.** Detailed below.

For the sub-decisions inside C, the chosen options (argued in place) are: **classification via a single engine LLM call per top-level comment, with manual override** (not silent, not unclassified-until-clicked); **association via a column on `work_comments`** for the 1&3 path (not a join table); **reply-as-engine-authored-thread-entry** for both paths (not a new general threaded-comment row type); and **outright removal of the magic wand**, not coexistence (not deferred convergence).

## Chosen approach

### The classifier (P1 — foundation)

Every other piece of this design routes on the classifier's output, so it is built and validated first.

**Trigger.** On every new **top-level** comment (`CommentsCreate` with no parent/thread-position — the base comment model is single-level, so in practice this is every comment) on an artifact whose `resolve_doc_owner` (below) resolves to a `Design`/`Investigation` task. **Replies within a bucket-2 thread are also classified** (see [Reclassifying follow-ups](#reclassifying-follow-ups)), but replies are a new thread-entry concept this design introduces, not a second top-level comment — see [Comment/thread state machine](#commentthread-state-machine).

**Where it runs.** Engine-side, synchronously triggered from the `CommentsCreate` handler but executed as a detached async task (mirroring the existing magic-wand dispatch pattern of spawning work off the request path and publishing completion over `comment_topic`) — the create RPC itself returns immediately with the comment in a `classifying` state; classification is not on the create request's critical path.

**Input.** The comment body, its anchor (`exact`/`prefix`/`suffix` — gives the classifier the quoted doc text, not just the raw comment), the doc's plain-text projection around the anchor for local context, and — for a reply — the prior thread turns (see below). No repo-wide context is given to the classifier itself; it is a fast, cheap, single-call step, not the answer agent.

**Output.** One of `directive | question | larger_change`, plus a confidence score. Written to new columns on `work_comments`:

```sql
ALTER TABLE work_comments ADD COLUMN intent TEXT;              -- 'directive'|'question'|'larger_change', NULL while classifying
ALTER TABLE work_comments ADD COLUMN intent_confidence REAL;    -- 0.0–1.0, engine-reported
ALTER TABLE work_comments ADD COLUMN intent_classified_at TEXT; -- when the LLM call completed
ALTER TABLE work_comments ADD COLUMN intent_overridden_by TEXT; -- NULL, or 'user' when manually reclassified
```

**Latency/UX while it runs.** The comment renders immediately (optimistic, as today) with a small "classifying…" badge in place of the intent chip; no banner, no thread entry, no agent spawn happens until `intent` is non-NULL. This is a few-hundred-ms–to-low-seconds LLM round trip; the badge is the only UX concession, matching the existing thinking-indicator pattern reused for bucket 2.

**Misclassification / override.** The classification is **not** a black box the operator is stuck with: the sidebar's intent badge is clickable and lets the operator manually reclassify a comment (`CommentsSetIntent{comment_id, intent}`), which sets `intent_overridden_by='user'` and re-runs routing from that point (e.g. a comment the classifier called `question` but was actually a `directive` gets re-routed into the 1&3 path once overridden, with no re-classification LLM call needed since the override _is_ the classification). `intent_overridden_by` is preserved permanently as an audit trail distinguishing engine calls from human corrections — useful both for the operator (trust) and for a future classifier-quality feedback loop (explicitly out of scope for v1, noted in [Risks](#risks--open-questions)).

**Reclassifying follow-ups.** A reply added to a comment already in the `answered`/`awaiting_followup` state (bucket 2) is classified fresh, with the prior thread's Q&A turns as classifier input, into the same three-way intent: another `question` re-enters bucket 2 (answer agent runs again with full thread context); a `directive`/`larger_change` follow-up ("ok then just add a section on X") routes into the bucket 1&3 path, **bridging** the thread's proposed edits (if the agent suggested any) into the revision's directive — see [Bridging a bucket-2 answer into a revision](#bridging-a-bucket-2-answer-into-a-revision). A follow-up on a comment already `in_revision` (bucket 1&3 in flight) is _not_ reclassified — mirrors the original design's "comments added after a revision is in flight form a new batch," except here the new activity is a reply on the same comment row, so it is treated as a fresh top-level-equivalent event once the existing batch resolves.

### Comment/thread state machine

The original single-track `active → in_revision → {resolved|active}` state machine is extended with a parallel bucket-2 track. Both tracks share the base `status` column; a comment is in **exactly one** track at a time, determined by its `intent`.

```
                                   ┌──────────────────────┐
                                   │  classifying          │  (transient; intent NULL)
                                   └──────────┬────────────┘
                                              │ CommentsClassify completes
                    ┌─────────────────────────┼─────────────────────────┐
                    │ intent=directive/        │ intent=question         │
                    │ larger_change             │                         │
                    ▼                          ▼                         │
             ┌────────────┐            ┌───────────────┐                │
             │   active   │            │   answering   │◀───────────────┘ (re-entry: fresh
             │ (nudge     │            │ (agent running,│                  question follow-up)
             │  reply      │            │  thinking      │
             │  posted)    │            │  indicator)    │
             └─────┬──────┘            └───────┬────────┘
                   │ [Revise] batch             │ agent posts reply
                   ▼                            ▼
             ┌────────────┐            ┌───────────────┐
             │in_revision │            │   answered    │
             │(revise_    │            │ (awaits       │
             │ task_id set)│            │  follow-up)   │
             └─────┬──────┘            └───────┬────────┘
                   │ task terminal              │ operator replies
        ┌──────────┴──────────┐                 │
        │ done (PR merged)     │ abandoned/      ▼
        ▼                      │ closed-unmerged  ┌──────────────────────┐
  ┌───────────┐                ▼                  │ awaiting_followup    │
  │ resolved  │          ┌───────────┐             │ (reclassify the      │
  └───────────┘          │  active   │◀────────────│  reply — see above)  │
                         │(reopened) │             └───────────┬───────────┘
                         └───────────┘                          │
                                                     intent=question │ intent=directive/larger_change
                                                                 │                 │
                                                                 ▼                 ▼
                                                          back to `answering`   into the
                                                          (loop)                `active`/nudge
                                                                                 branch above
                                                                                 (bridge, see below)

   orphaned  — anchor lost; excluded from any banner/classification; never auto-addressed.
   dismissed — user hid it; excluded; never addressed.
```

`dispatched` (the old magic-wand state) is **removed**, not retained — see [Migration](#migration-retiring-the-magic-wand).

| From                | To                                       | Trigger                                                   | Who                                                        | Idempotency                                                                                                                            |
| ------------------- | ---------------------------------------- | --------------------------------------------------------- | ---------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| (new comment)       | `classifying`                            | `CommentsCreate` on an eligible doc                       | engine                                                     | comment starts here; `intent` NULL                                                                                                     |
| `classifying`       | `active` (1&3) or `answering` (bucket 2) | classifier completes                                      | engine (`CommentsClassify`)                                | only fires once per `intent_classified_at IS NULL`; a re-run on an already-classified row is a no-op unless it's an explicit override  |
| `active`            | `in_revision`                            | `[Revise]` batch addresses this comment                   | engine (`CommentsReviseDoc`)                               | only `active`→`in_revision`; a comment already `in_revision`/`resolved`/`dismissed`/`orphaned` is skipped, so double-Revise is a no-op |
| `in_revision`       | `resolved`                               | the associated task reaches `done` (its PR merged)        | engine (reconciliation, hooked on the terminal transition) | only `in_revision`→`resolved`; re-firing on an already-`resolved` row is a no-op; sets `last_resolved_with`                            |
| `in_revision`       | `active`                                 | the associated task is abandoned / its PR closed-unmerged | engine (reconciliation)                                    | only `in_revision`→`active`; clears `revise_task_id`; re-firing is a no-op                                                             |
| `in_revision`       | `active`                                 | manual override (reviewer un-dispatches)                  | user (`CommentsSetStatus`)                                 | allowed; clears `revise_task_id`                                                                                                       |
| `answering`         | `answered`                               | answer agent posts its reply                              | engine (agent completion callback)                         | only fires once per agent run id                                                                                                       |
| `answered`          | `awaiting_followup`                      | operator posts a reply in the thread                      | engine (on `CommentsCreate` of a reply)                    | —                                                                                                                                      |
| `awaiting_followup` | `answering`                              | reclassified reply has `intent=question`                  | engine                                                     | loops back into bucket 2 with accumulated thread context                                                                               |
| `awaiting_followup` | `active`                                 | reclassified reply has `intent∈{directive,larger_change}` | engine                                                     | bridges into the 1&3 path (see below); comment's `status` becomes `active` so the next `[Revise]` batch picks it up                    |
| any bucket-2 state  | (manual override)                        | reviewer reclassifies via the intent badge                | user (`CommentsSetIntent`)                                 | re-enters routing at the new intent's entry point                                                                                      |

`active`/`in_revision`/`resolved` (1&3 track) and `answering`/`answered`/`awaiting_followup` (bucket-2 track) are mutually exclusive per comment at any instant, but a comment can **cross tracks** exactly once per reclassification event (bucket-2 → bucket-1&3 bridge above; the reverse — a directive later turning out to want discussion — is handled by manual override, not automatically).

`in_revision`/`answering`/`answered`/`awaiting_followup` comments are **excluded from the `[Revise]` banner's count**; only `active` (1&3-track, unaddressed) comments count toward it. Bucket-2 comments never contribute to the revise banner — they have their own thread-level indicators (thinking spinner, "answered" checkmark) instead.

### Buckets 1 & 3 — unified (directive / larger change)

Directives and larger-changes are handled identically — there is no separate minor-update path, matching the original design's decision to unify them.

**On classification as `directive` or `larger_change`,** the engine immediately posts an engine-authored reply in the comment's thread nudging the operator toward starting a revision, e.g. _"This looks like it wants a doc change — click [Revise] to start one."_ This is a change from the original design's plain banner-only approach: previously the banner alone signaled unresolved comments; now the classifier's confidence in "this wants a change" is loud and immediate per-comment, not just an aggregate count. The `[Revise]` banner (`{n} unresolved comment(s). [Revise]`) still exists and still batches; the nudge reply is what tells the operator _why_ a specific comment is sitting in that count.

Everything designed for the revision/update-task machinery in the original design is preserved verbatim as this branch's implementation:

#### Association model

**A soft-FK column on `work_comments`, not a join table.**

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

- The name is deliberately **`revise_task_id`, not `revision_task_id`** — the target can be a `kind='revision'` _or_ a `kind='chore'` (general-task path).
- A join table (`comment_revise_batches`) was considered and rejected: a comment belongs to **exactly one** in-flight revise batch at a time, so the relationship is 1:1 while in flight — a column expresses it without a second table and without a join on the hot reconciliation path. The historical "which batches has this comment been through" question is answered by the audit trail on the _task_ side (`created_via` + the tasks themselves).
- **Provenance on the task** carries the reverse pointer and the doc-comment origin: `created_via = "doc-comment:<artifact_kind>:<artifact_id>"` (extending `canonicalize_created_via`, the one choke point P707 already uses).

**Batch scope.** v1: **all comments with `status='active'` (i.e. classified `directive`/`larger_change` and not yet addressed) on the artifact at click time.** Orphaned/dismissed excluded. Subset selection is deferred (open question; the RPC signature reserves an optional `comment_ids` filter).

**Comments added after a revision is in flight.** New comments are freshly classified; if `directive`/`larger_change`, they land `active` and are _not_ swept into the in-flight batch. A second `[Revise]` spawns a **second** work item — a revision-of-revision (chain-root-numbered `R2`, per P654 OQ2) if the first is still an open revision on the same PR, or a general chore if the PR merged in between.

#### The revision-vs-general-task decision

Decided **in the engine, at RPC time**, keyed on the doc's current PR lifecycle.

**Reverse resolver** `resolve_doc_owner(artifact_kind, artifact_id) -> Option<DocOwner>` where `DocOwner { task_id, task_kind, chain_root_id, pr_url, pr_lifecycle }` — this same resolver also gates classifier eligibility (only `Design`/`Investigation`-owned docs get classified at all, so the classifier itself never runs on out-of-scope artifacts):

1. Parse `artifact_id`. For `pr_doc:<repo>:<branch>:<path>`:
   - If `<branch>` is an execution's engine-supplied `expected_branch` (`boss/exec_*`), map branch → execution → task. That task is the doc's owner; read `tasks.pr_url` and its cached PR poll-state.
   - Else (e.g. `<branch> = main` after merge) match `projects WHERE design_doc_repo_remote_url=<repo> AND design_doc_branch=<branch> AND design_doc_path=<path>` → the project's `kind='design'`, `ordinal=0` task; and `tasks WHERE doc_repo_remote_url=<repo> AND doc_branch=<branch> AND doc_path=<path>` → project-less investigation/design tasks.
2. For `artifact_kind='work_item'`: not a design/investigation doc — return `None` (scope guard; no classification, no banner, no bucket-2 either — the magic wand's old exclusive territory now has **no** comment-driven affordance, per [Migration](#migration-retiring-the-magic-wand)).
3. Resolve `pr_lifecycle` from the owner's `pr_url` + the poll-state columns the merge poller already maintains; a present `pr_url` with no terminal marker reads as **Open**.

**Decision table** (evaluated engine-side, authoritative at click):

| Owner resolves to              | PR lifecycle              | Action                                                                                                     |
| ------------------------------ | ------------------------- | ---------------------------------------------------------------------------------------------------------- |
| design/investigation task      | **Open** (unmerged)       | `create_revision(parent = owner's chain root)` — a `revision` that adds a commit to the doc's existing PR. |
| design/investigation task      | **Merged**                | `create_chore` — general task to update the (now-landed) doc; opens a fresh PR against `main`.             |
| design/investigation task      | **ClosedUnmerged**        | `create_chore` — general task (the old PR is dead; fresh work).                                            |
| design/investigation task      | **no PR** (`pr_url` NULL) | `create_chore` — general task (nothing to amend).                                                          |
| not a design/investigation doc | —                         | no-op; RPC returns `NotApplicable`; no banner, no classification.                                          |

Both vehicles autostart. Both carry `created_via = doc-comment:…` and both get the addressed comments stamped with `revise_task_id`.

**Edge cases** (unchanged from the original design): PR-open-with-a-revision-already-active → revision-of-revision; PR-merged-between-render-and-click → the resolver re-checks lifecycle at RPC time and `assert_parent_revisable` is the backstop; doc-never-had-a-PR → chore; two-reviewers-click-simultaneously → the guarded `UPDATE … WHERE status='active'` makes the loser's click `AlreadyInFlight`.

#### Engine RPC surface

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
    pub comment_ids: Option<Vec<String>>, // v1: None ⇒ all directive/larger_change active; reserved for subset
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
2. Load `active` comments with `intent ∈ {directive, larger_change}` (or the `comment_ids` intersection); if empty → `NoUnresolvedComments`.
3. Apply the decision table → build a `create_revision` or `create_chore` call. For a revision, `assert_parent_revisable(chain_root)`; on gate failure (raced merge) fall through to the chore branch.
4. Create the work item with `created_via="doc-comment:<kind>:<artifact_id>"`, autostart on.
5. `UPDATE work_comments SET status='in_revision', revise_task_id=<task>, status_actor='engine', updated_at=now WHERE artifact_...=... AND status='active' AND intent IN ('directive','larger_change') [AND id IN (...)]`.
6. Publish on `comment_topic(artifact)` so viewers refetch; return `Created{…}`.

**Reconciliation** (engine-owned, hooked on existing terminal signals — no new poller), unchanged from the original design:

```rust
fn reconcile_comments_for_task(tx, task_id, outcome: {Resolved | Reopened})
```

- `Resolved`: `UPDATE work_comments SET status='resolved', last_resolved_with='revise:<task_id>', status_actor='engine' WHERE revise_task_id=? AND status='in_revision'`.
- `Reopened`: `UPDATE … SET status='active', revise_task_id=NULL, status_actor='engine' WHERE revise_task_id=? AND status='in_revision'`.

Called from every place a task reaches a terminal state — merge → resolved via `mark_chore_pr_merged` / `flip_in_review_revisions_to_done` fan-out; abandon/closed-unmerged → reopened, gated on the same not-yet-wired terminal signal flagged in [Risks](#risks--open-questions).

### Bucket 2 — question: read-only "mini-coordinator" answer agent

This is the entirely new piece. Today a question comment gets **zero** engine response; this closes that gap without ever letting the answer path mutate anything.

**Trigger.** Classification as `question` (fresh, or a re-entered follow-up per [Reclassifying follow-ups](#reclassifying-follow-ups)).

**What it is.** An ephemeral, engine-spawned LLM agent given: the question's text, its anchor (quoted doc text), the surrounding doc content, the full prior thread on this comment (if any — earlier Q&A turns), and read access to the same product/project/task/PR/engine-state surface the Boss coordinator itself queries. It may **lease a workspace** (via the same `cube workspace lease` mechanism ordinary workers use) to check out and read code when the question needs it (e.g. "why does the retry logic do X" needs to actually read the retry logic).

**Capability boundary — read-only, precisely:**

| Capability                                                                                  | Allowed?                                                                                                                     |
| ------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| Read products/projects/tasks/executions/PR state via the coordinator's existing query layer | Yes — identical visibility to the coordinator                                                                                |
| Read the commented-on document's full content (not just the anchor snippet)                 | Yes — required; the agent is given the whole doc, not a fragment                                                             |
| Read the comment + its full thread (anchor, prior replies, classification history)          | Yes — required for context-aware answers                                                                                     |
| Lease a workspace to check out a repo and read files                                        | Yes — read-only checkout; the agent never commits or pushes from it                                                          |
| Create/mutate a task, project, or comment status (other than posting its own reply)         | **No**                                                                                                                       |
| Open, update, or push to a PR                                                               | **No**                                                                                                                       |
| Apply a magic-wand-style edit or write to the doc                                           | **No** — this is the exact behavior being removed; bucket 2 replaces "silently apply" with "explain, and optionally propose" |
| Release/mutate cube lease state beyond the read-only lease it holds                         | **No**                                                                                                                       |

Enforcement is not just prompt instruction: the agent's execution is spawned with a **capability-restricted tool set** (no `CreateChore`/`CreateRevision`/`CommentsSetStatus`/`cube pr create`/`cube pr update`/`jj git push`등 in its allowed RPC/tool list at all — the boundary is enforced by what the agent process is _given access to_, mirroring how a PreToolUse hook hard-blocks `gh pr create` for revision workers today, not by asking the model nicely). Concretely: the answer agent runs under a new execution kind (`answer_agent`, distinct from `revision_implementation`/`project_design`/etc.) whose dispatch path wires a reduced tool/RPC surface — read-only DB queries, `cube workspace lease` (read), and the "post a thread reply" RPC — and omits every mutating RPC/tool a normal worker gets. This is the same category of enforcement the revision PreToolUse hook already demonstrates (block at the tool-availability layer, not the prompt layer), applied to a much smaller allowed set.

**How it differs from a normal worker**, spelled out because it's easy to conflate with an ordinary chore/investigation dispatch:

- **Ephemeral:** exists only for the duration of answering one thread turn; no persistent task row backs it (no `tasks` row, no kanban card) — it is tracked as an **agent run** (see below), not a task.
- **Read-only:** enforced at the tool/RPC-availability layer, not by convention.
- **Thread-scoped:** its entire mandate is "answer this question in this thread," not "complete a work item"; it has no PR to open, no completion criterion beyond posting a reply.
- **Spawned/tracked by the engine, not the coordinator:** the coordinator is a separate, longer-lived process; the answer agent is spawned directly by the engine's comment-handling path (mirroring how the magic wand's `crate::magic_wand::dispatch` was engine-spawned, not coordinator-spawned) and tracked in a small new table:

```sql
CREATE TABLE IF NOT EXISTS answer_agent_runs (
    id             TEXT PRIMARY KEY,
    comment_id     TEXT NOT NULL REFERENCES work_comments(id),
    artifact_kind  TEXT NOT NULL,
    artifact_id    TEXT NOT NULL,
    doc_version    TEXT NOT NULL,
    thread_turn    INTEGER NOT NULL,   -- 0 for the first answer, 1+ for follow-ups
    status         TEXT NOT NULL,      -- 'running' | 'replied' | 'failed'
    workspace_lease_id TEXT,           -- set if it leased a workspace; released on completion
    reply_body     TEXT,
    error_kind     TEXT,
    created_at     TEXT NOT NULL,
    completed_at   TEXT
);
CREATE INDEX IF NOT EXISTS answer_agent_runs_by_comment ON answer_agent_runs(comment_id, created_at);
```

This deliberately mirrors the shape of `magic_wand_dispatches` (comment-keyed, per-run row, status + result) because it is solving the analogous problem — track one ephemeral LLM call against a comment — just with a different capability profile and a thread-reply instead of an apply/discard result.

**Thinking indicator.** While `status='running'`, the sidebar shows a thinking/typing indicator in the thread (the same visual language a chat UI uses for "the other party is typing"), driven by the comment's `answering` status + the existence of a `running` `answer_agent_runs` row, pushed over `comment_topic`. No polling — the same subscription mechanism the rest of the comment system already uses.

**Reply.** On completion, the agent posts a comprehensive reply as an engine-authored thread entry (same "reply" mechanic as the 1&3 nudge — see [Reply/link mechanics](#replylink-mechanics)), the comment flips `answering → answered`, and `answer_agent_runs.status='replied'`. The reply **may include concrete proposed edits** ("I'd suggest changing the retry backoff to exponential — here's a sketch: …") but the agent has no mechanism to apply them; it is prose (with optional embedded diff/snippet), not a patch the engine executes.

**Follow-up loop.** The operator's next reply in the thread is classified (see [Reclassifying follow-ups](#reclassifying-follow-ups)):

- Another question → `answered → answering` (re-entry), agent runs again with the accumulated thread as context (`thread_turn` increments).
- A request for changes → bridges into the revision path.

#### Bridging a bucket-2 answer into a revision

When a follow-up reclassifies to `directive`/`larger_change` (either the operator explicitly says "yes, make that change" or a fresh directive comment references the prior answer), the comment's `status` moves to `active` (see state machine) and is picked up by the next `[Revise]` batch exactly like any other directive comment — **with one addition**: if the most recent `answer_agent_runs` row for this comment has a non-null `reply_body` containing a proposed edit, that reply text is included verbatim in the revision/chore's directive (`compose_revision_directive` / the chore's doc-edit directive already assembles "path + anchors + comment bodies" per the original design — the bucket-2 reply is appended to that assembly as additional context, not a replacement for the comment body itself). This means the revision worker sees both the original question, the agent's proposed answer/edit, and the operator's follow-up confirming they want it — strictly more context than a 1&3 comment gets today, at no cost to the shared machinery.

### Reply/link mechanics

Both bucket 1&3's nudge and bucket 2's answer are **engine-authored thread entries** on the comment, not new user-facing comment rows and not general-purpose threading. Concretely, this design adds one small append-only table shared by both:

```sql
CREATE TABLE IF NOT EXISTS comment_thread_entries (
    id           TEXT PRIMARY KEY,
    comment_id   TEXT NOT NULL REFERENCES work_comments(id),
    entry_kind   TEXT NOT NULL,   -- 'nudge' | 'answer' | 'operator_followup'
    author       TEXT NOT NULL,   -- 'engine' | operator identity
    body         TEXT NOT NULL,
    revise_task_id TEXT,          -- set on a 'nudge' entry once [Revise] is clicked (may postdate the entry)
    answer_agent_run_id TEXT REFERENCES answer_agent_runs(id), -- set on an 'answer' entry
    created_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS comment_thread_entries_by_comment ON comment_thread_entries(comment_id, created_at);
```

This is the minimal "conversation" shape the brief asks for (classification badge, nudge, agent replies, operator follow-ups, re-routing) without generalizing the base comment model's deliberate single-level non-threading — `comment_thread_entries` rows are _always_ children of exactly one `work_comments.id` and are rendered inline under that comment, never as independent top-level comments.

- **1&3 nudge:** an `entry_kind='nudge'` row posted at classification time; its `revise_task_id` is filled in once a `[Revise]` batch actually claims the comment (may be well after the nudge was posted, if the operator waits). The sidebar renders the live chip **`⟳ In revision → R2 · T712 ↗`** / **`⟳ Update task → T900 ↗`** from the comment's own `revise_task_id` (unchanged from the original design) — the thread entry is the conversational trace of _why_, the chip on the comment is the current _state_.
- **Bucket-2 answer:** an `entry_kind='answer'` row with `answer_agent_run_id` set, rendered as the agent's message in the thread.
- **Operator follow-up:** an `entry_kind='operator_followup'` row, which is what gets reclassified (see [Reclassifying follow-ups](#reclassifying-follow-ups)) — this is the thread-native equivalent of "a new comment," scoped to the existing comment rather than creating a sibling `work_comments` row.
- **On the GitHub side** (PR-backed docs), the reviewer-facing breadcrumb remains the revision worker's existing `[boss-revision] R<n>: <description>` PR comment (P654 OQ6, gated by editorial-controls) for the 1&3 path. Bucket 2 has no GitHub-side breadcrumb — it never touches the PR at all.

### UI / thread behavior (thin client)

- The engine exposes both banner state and thread state as part of the existing comment load. `CommentsList` (or a small companion read) now also returns, per comment: `{ intent: "directive"|"question"|"larger_change"|null, intent_confidence, status, thread_entries: [...], answer_agent_running: bool }`. `revisable` (whether the `[Revise]` banner should show) is still true iff `resolve_doc_owner` yields a design/investigation task **and** there is at least one `active` comment with `intent ∈ {directive, larger_change}`.
- `DesignRendererView` / `MarkdownViewerView` render: (1) the classification badge on each comment ("classifying…" / a small directive/question/larger-change icon, clickable to override), (2) the `[Revise]` banner when applicable, (3) the thinking/typing indicator under a comment while its `answer_agent_runs` row is `running`, (4) the thread entries (nudge / answer / follow-up) inline under the comment in chronological order, (5) the `in_revision`/`resolved`/`reopened` chip on 1&3-track comments.
- All of this is engine-computed state pushed over `comment_topic`; the client refetches on push and renders — no decisioning, no PR-state knowledge, no classification, no task-creation logic, no agent-spawn logic in the client.

### Concurrency/idempotency

- **Double-click / rapid re-Revise.** Unchanged from the original design: the `active→in_revision` flip is a single guarded `UPDATE … WHERE status='active'` inside the create transaction; a racing second call finds nothing left and returns `AlreadyInFlight{task_id}`.
- **Rapid-fire follow-up questions.** If the operator posts a second follow-up while the answer agent is still `answering` the first, the second `operator_followup` entry is queued (not classified/dispatched) until the in-flight `answer_agent_runs` row completes — the comment stays in `answering` and the queued entry's classification/dispatch happens on the existing run's completion callback, ensuring at most one live answer agent per comment at a time.
- **Multiple reviewers / concurrent threads across different comments.** Independent — each comment's classification, `[Revise]` batch membership, and answer-agent run are all keyed by `comment_id`/`artifact_id`; nothing serializes across comments except the batch `UPDATE` within one `[Revise]` click.
- **Reconciliation idempotency.** Both `Resolved` and `Reopened` updates are guarded on `status='in_revision'`; re-running a sweep never double-transitions. Answer-agent completion is guarded on `answer_agent_runs.status='running'` so a duplicate completion callback is a no-op.

### Scope guard

Unchanged: gated at the one authoritative point, `resolve_doc_owner` returns an owner only when the resolved task's `kind ∈ {Design, Investigation}`. `artifact_kind='work_item'` or a `pr_doc` owned by a plain chore/project-task PR → `None` → no classification, no banner, no bucket-2 agent. Detection is engine-side and derived from `tasks.kind`, never sniffed in the UI.

## Migration: retiring the magic wand

The magic wand is not deprecated-but-kept; it is **removed**, because the classifier supersedes both of its use cases (bucket 1&3's nudge-then-revise replaces its silent-apply-a-single-comment path; bucket 2 covers what it never could — questions). This section is new relative to the original design, which had explicitly deferred this decision ("they coexist in v1... future convergence noted as not a v1 blocker"); this revision makes the removal explicit and immediate, since keeping a silent-apply path around while the classifier exists is actively wrong on the question bucket.

**What gets removed:**

1. **Wire protocol** (`protocol/src/wire.rs`, `protocol/src/types.rs`): the three RPCs `CommentsDispatchMagicWand`, `CommentsApplyMagicWand`, `CommentsDiscardMagicWand` and their request/response types.
2. **Engine handlers** (`engine/core/src/app/comments.rs`): `handle_comments_dispatch_magic_wand` (:251), `handle_comments_apply_magic_wand` (:611), `handle_comments_discard_magic_wand` (:659), and their routing entries in `app.rs`.
3. **The dispatch module** (`engine/core/src/magic_wand.rs`) and its store methods (`create_magic_wand_dispatch`, `create_pr_backed_magic_wand_dispatch`, `complete_magic_wand_dispatch`, `apply_magic_wand_dispatch`, `discard_magic_wand_dispatch` — `work/comments.rs`, `work/mappers.rs`).
4. **The `dispatched` comment status** — removed from the valid `work_comments.status` set (`comments.rs:91`); no comment can be created or transitioned into `dispatched` after this lands.
5. **macOS UI**: `MagicWandResultSheet.swift` deleted; the magic-wand trigger affordance removed from `CommentSidebar.swift`/`CommentPopover`; replaced by the classification badge + `[Revise]` banner + thread entries designed above.
6. **The `magic_wand_dispatches` table** is _not_ dropped outright (see below) but is no longer written to by any live code path after this migration completes.

**Retiring persisted state without breaking existing comments** — the two things that must not silently break: comments already sitting in `status='dispatched'`, and historical rows in `magic_wand_dispatches` that operators or future audits might want to inspect.

- **Data migration for `dispatched` comments.** A one-time migration (`migrate_retire_magic_wand_dispatched_comments`, run once at startup after this PR lands, mirroring the idempotent `migrate_*` pattern already used for schema changes) walks every `work_comments` row with `status='dispatched'` and:
  - If its `magic_wand_dispatches.status` was `'applied'` → set `status='resolved'`, `last_resolved_with='magic-wand:<dispatch_id>'` (preserves the historical fact that it _was_ addressed, without inventing a fictional `revise_task_id`).
  - If `'running'`/`'staged'`/anything non-terminal → set `status='active'` (the in-flight dispatch is abandoned by this migration; the comment falls back into the normal pool and will be classified fresh next time comment state is loaded, since `intent` is NULL for it).
  - Idempotent: only rows still `status='dispatched'` are touched; running twice is a no-op on the second pass because there are no more `dispatched` rows left to migrate.
- **The `magic_wand_dispatches` table itself is left in place, unread by new code, as a historical record** — it is not dropped in this migration. Dropping it destroys the `applied`/`result_md` audit trail referenced by `last_resolved_with='magic-wand:<dispatch_id>'` above with no compensating benefit (it's a handful of columns, not a hot table). A follow-up cleanup migration to drop it entirely is noted as `future / not a v1 blocker` once enough time has passed that no one needs the audit trail.
- **In-flight dispatches at deploy time.** Because the migration reclassifies any non-`applied` dispatch's comment back to `active` (see above), an in-flight magic-wand call that completes _after_ this migration runs simply finds its target comment no longer in `dispatched` state; its completion callback (`complete_magic_wand_dispatch`) is deleted along with the rest of the module, so there is nothing left to call — the in-flight LLM call, if any survives the deploy, completes into a void and is discarded. This is acceptable because magic-wand dispatches are already fire-and-forget single-shot operations with no other side effects to unwind.
- **`work_item` comments lose their comment-driven affordance entirely.** The magic wand was the _only_ comment-driven mechanism for work-item description comments (`artifact_kind='work_item'`), and this design's classifier/banner/answer-agent are all gated to `Design`/`Investigation` docs only (per the scope guard). After this migration, a comment on a work-item description is inert — no nudge, no banner, no answer agent — until/unless a future design extends the three-intent model to that artifact kind. This is called out explicitly as a **regression in coverage**, not an oversight: it was in scope before (magic wand covered `work_item`) and is out of scope now. Flagged in [Risks](#risks--open-questions) as a decision the operator should explicitly bless or push back on.

## Risks / open questions

- **Classifier accuracy and cost/latency.** Every top-level comment (and every bucket-2 follow-up) is now an LLM call on the critical path of "does anything happen at all" for that comment. A wrong classification routes a real question into silent-revision-limbo (no nudge reply text tells the operator why nothing was answered) or, worse, treats a genuine directive as a question and never surfaces a `[Revise]` nudge. _Mitigation:_ the manual-override badge makes misclassification cheap to fix (one click, no re-run of the LLM call needed since the override doubles as the classification), and `intent_overridden_by` gives a durable signal for a future accuracy audit. _Open question:_ should low-confidence classifications (`intent_confidence` below some threshold) default to a distinguishable UI state (e.g. "uncertain — question or directive?") rather than silently picking one? Not decided in v1; picking the higher-probability bucket and relying on override is the v1 default. (Manifest.)
- **Per-comment LLM cost/latency at scale.** A doc with dozens of comments during an active review round means dozens of classifier calls plus, for every question, a full answer-agent run (which may itself lease a workspace and read code — not cheap). _Mitigation:_ none built into v1 beyond the classifier being a small/cheap model-appropriate call; explicitly flagged as a cost question for the operator rather than solved here — batching/rate-limiting classifier calls is `future / not a v1 blocker` if this becomes a real cost concern.
- **The answer agent's read-only sandbox guarantees.** The design asserts enforcement happens "at the tool-availability layer," mirroring the existing revision PreToolUse hook — but the revision hook blocks exactly one command (`gh pr create`); the answer agent needs a much larger _allowlist-not-blocklist_ posture (no mutating RPC reachable at all, not just one blocked command). _Open question:_ what is the concrete mechanism — a distinct execution-kind dispatch path with a hard-coded reduced tool table (recommended, and what this design assumes), vs. a runtime capability-token check on every RPC call? The former is simpler and matches existing patterns; the latter is more defense-in-depth but is new machinery. Needs a decision before Phase 3 implementation starts. (Manifest.)
- **Concurrency of multiple threads / multiple answer agents.** v1's answer-agent concurrency guard is per-comment only (see [Concurrency](#concurrencyidempotency)); it does not cap how many answer agents run simultaneously _across_ comments/docs system-wide. If a reviewer leaves ten questions on one doc, ten answer agents (each possibly leasing a workspace) spin up near-simultaneously. _Open question:_ does this need a global concurrency cap / queue, or is per-comment independence acceptable for v1? Not decided. (Manifest.)
- **Reopen-on-abandon has no terminal signal today** (carried over from the original design, unchanged): the poller's `ClosedUnmerged` arm is a no-op (`merge_poller.rs:1984`); `chore-lifecycle-pr-closed-unmerged` is unimplemented. Until a closed/abandoned terminal transition exists, comments addressed by a rejected revision/chore in the 1&3 path would sit at `in_revision` indefinitely. _Mitigation / decision needed:_ depend on that lifecycle work, or add a minimal comment-only reopen hook in the `ClosedUnmerged` arm now. (Manifest.)
- **The reverse resolver is a new trust path** (carried over): `resolve_doc_owner` infers owner from branch/path; a mismatch could mis-route both classification eligibility and 1&3 routing. _Mitigation:_ prefer the execution-branch mapping (exact) over the path match; ambiguous path match → `None` (no classification/banner at all) rather than guess.
- **Batch-vs-subset for `[Revise]`.** v1 addresses all `directive`/`larger_change`-classified unaddressed comments in one batch. _Open question:_ ship subset selection in v1 or defer? (Manifest, carried over.)
- **Work-item comments lose all comment-driven affordance.** Called out in [Migration](#migration-retiring-the-magic-wand) as an explicit coverage regression versus the magic wand's prior scope. _Open question:_ is this acceptable, or does work-item description commenting need its own (smaller?) slice of the three-intent model before/alongside this ships? (Manifest.)
- **Bridging a bucket-2 proposed edit into a revision directive** relies on the agent's free-text reply containing something structured enough to quote usefully (see [Bridging](#bridging-a-bucket-2-answer-into-a-revision)). If the agent's answer is prose-only with no concrete proposed diff, the bridge degrades gracefully to "just the comment + follow-up, no proposed-edit context" — acceptable, but means bridge quality is only as good as how concretely the agent chose to phrase its answer, which isn't independently guaranteed. Noted as a soft risk, not blocking.
- **General-chore / revision doc-edit directive quality** (carried over, still applies to the 1&3 path): the chore/revision must be told precisely which file + which comments (now: plus any bridged bucket-2 context) to address. _Mitigation:_ unchanged from the original design — directive assembly includes doc path, quoted anchors, and comment bodies.

## Proposed implementation task breakdown

PR-sized tasks in dependency order, organized by the three phases the operator called out: **P1 (classifier + intent model) is the explicit prerequisite for everything else** — no bucket-1&3 unification, magic-wand removal, or bucket-2 work should start before it lands. Effort hints: `trivial | small | medium | large`. Tasks at the same depth with no edge between them may run in parallel.

### Phase 1 — the classifier + intent model (prerequisite)

**1a. Intent columns + classifier plumbing.** Add `work_comments.intent / intent_confidence / intent_classified_at / intent_overridden_by` (migration mirroring `migrate_work_comments_table`). Add the `classifying` transient comment state (or represent it as `intent IS NULL` without a new `status` value — implementation detail for the classifier task itself to settle). Wire the detached-async classifier call off `CommentsCreate`, writing the intent columns on completion and publishing on `comment_topic`. **Effort:** `medium`. **Dependencies:** none.

**1b. `CommentsSetIntent` manual override RPC.** Add the RPC + handler that lets the sidebar's badge reclassify a comment, setting `intent_overridden_by='user'` and re-running routing from the new intent's entry point. **Effort:** `small`. **Dependencies:** 1a.

**1c. Reverse doc→owner resolver.** `resolve_doc_owner(artifact_kind, artifact_id) -> Option<DocOwner>` as specified above — this gates _both_ classifier eligibility and 1&3 routing, so it belongs in Phase 1 even though the original design scoped it under the revision-routing work. **Effort:** `medium`. **Dependencies:** none (parallel with 1a/1b).

**1d. Classification badge (thin client).** Render the classifying/directive/question/larger-change badge + override control in `CommentSidebar`/`CommentPopover`. **Effort:** `small`. **Dependencies:** 1a, 1b; **external prerequisite:** P529 UI persistence.

### Phase 2 — unify buckets 1 & 3, remove the magic wand

**2a. `CommentsReviseDoc` RPC + routing + comment transition.** As specified in [Buckets 1 & 3](#buckets-1--3--unified-directive--larger-change): the `revise_task_id` column, `ReviseDocInput`/`ReviseDocOutcome`, `handle_comments_revise_doc` filtering on `intent ∈ {directive, larger_change}`, the decision table, autostart + `created_via`, the guarded batch `UPDATE`. **Effort:** `medium`. **Dependencies:** 1a, 1c.

**2b. Nudge reply on classification.** Post the `entry_kind='nudge'` `comment_thread_entries` row immediately on `directive`/`larger_change` classification (before `[Revise]` is even clicked). Requires the `comment_thread_entries` table. **Effort:** `small`. **Dependencies:** 2a.

**2c. Completion reconciliation — resolve on merge / reopen on abandon.** Unchanged in substance from the original design's tasks 4–5: `reconcile_comments_for_task`, hooked into `mark_chore_pr_merged` / `flip_in_review_revisions_to_done` for resolve, and the not-yet-wired closed-unmerged signal for reopen (soft-depends on `chore-lifecycle-pr-closed-unmerged`, per [Risks](#risks--open-questions)). **Effort:** `medium` (resolve) + `small`/`medium` (reopen, depending on which mitigation option is chosen). **Dependencies:** 2a.

**2d. Banner state on the comment read path.** Extend `CommentsList`/a `CommentsBannerState` read with `{revisable, unresolved_count (directive/larger_change only), in_revision_count, doc_kind}`. **Effort:** `small`. **Dependencies:** 1c, 2a.

**2e. Magic-wand removal.** Delete the three RPCs, the three handlers, `magic_wand.rs`, the store methods, `MagicWandResultSheet.swift`, the sidebar trigger affordance, and the `dispatched` status value from validation; run the one-time `migrate_retire_magic_wand_dispatched_comments` data migration. Leave `magic_wand_dispatches` table in place, unread (see [Migration](#migration-retiring-the-magic-wand)). **Effort:** `medium`. **Dependencies:** 2a, 2b (the replacement paths must exist before the old one is deleted, so operators aren't left with neither).

**2f. macOS `[Revise]` banner + `in_revision`/nudge chips (thin client).** Render the banner, wire `[Revise]`, render nudge/`in_revision`/resolved/reopened chips and thread entries. **Effort:** `medium`. **Dependencies:** 2a, 2b, 2d; **external prerequisite:** P529 UI persistence.

### Phase 3 — the bucket-2 read-only answer agent + thread conversation loop

**3a. `answer_agent_runs` table + `answer_agent` execution kind + capability-restricted dispatch.** The new table, the new execution kind distinct from existing worker kinds, and the reduced tool/RPC surface enforced at the dispatch layer (per [Risks](#risks--open-questions) — this task must resolve the allowlist-vs-capability-token open question before implementation). Includes read-only workspace-lease wiring. **Effort:** `large`. **Dependencies:** 1a, 1c (needs the doc-owner resolver and classified intent to trigger on).

**3b. Answer-agent spawn + reply + thinking indicator.** Wire `question`-classified comments to spawn a `3a` agent run, transition `active→answering`, push the thinking indicator over `comment_topic`, and on completion post the `entry_kind='answer'` thread entry and transition `answering→answered`. **Effort:** `medium`. **Dependencies:** 3a.

**3c. Follow-up reclassification loop.** Operator replies become `entry_kind='operator_followup'` rows; reclassify per [Reclassifying follow-ups](#reclassifying-follow-ups); route `question` back into 3b, route `directive`/`larger_change` into the [bridge](#bridging-a-bucket-2-answer-into-a-revision) (comment → `active`, bridged context appended for the next `[Revise]` batch to pick up via 2a's directive assembly). **Effort:** `medium`. **Dependencies:** 2a, 3b.

**3d. Thread UI (thin client).** Render the thinking indicator, answer entries, follow-up composer, and the bridge transition (comment moving from the bucket-2 visual track to the bucket-1&3 track) in `CommentSidebar`. **Effort:** `medium`. **Dependencies:** 3b, 3c, 1d; **external prerequisite:** P529 UI persistence.

### Deferred / future (not a v1 blocker)

- **Subset selection** for `[Revise]` batches — the reserved `comment_ids` filter. Depends on 2a.
- **Low-confidence classification UX** (distinguishable "uncertain" state instead of silently picking a bucket). Depends on 1a.
- **Global answer-agent concurrency cap.** Depends on 3a.
- **Classifier-accuracy feedback loop** using `intent_overridden_by` as training/eval signal. Depends on 1a, 1b.
- **Dropping the `magic_wand_dispatches` table** entirely once its audit trail is no longer needed. Depends on 2e.
- **Extending the three-intent model to `work_item` comments**, restoring (in a better form) the coverage the magic wand's removal drops for that artifact kind. Depends on all of Phase 1–3 landing first; explicitly flagged in [Risks](#risks--open-questions) as needing an operator decision, not assumed.

## Related designs

- [`comments-in-markdown-viewer`](comments-in-markdown-viewer.md) (P529) — the comment model, statuses, anchoring, `pr_doc` artifact keys, and the per-comment magic-wand path this design retires.
- [`revision-tasks`](revision-tasks.md) (P654) — the revision substrate, `create_revision`, the gate, chain-root numbering, and lifecycle the 1&3 path produces onto.
- [`unify-pr-remediation-on-revisions`](unify-pr-remediation-on-revisions.md) (P707) — the "producer on the revision substrate" pattern and `created_via` provenance grammar the 1&3 path is an instance of.
- [`chore-lifecycle-pr-closed-unmerged`](chore-lifecycle-pr-closed-unmerged.md) — the closed-unmerged terminal signal the reopen-on-abandon path depends on.
- [`design-producing-tasks`](design-producing-tasks.md) / [`auto-populate-project-tasks-on-design-pr-merge`](auto-populate-project-tasks-on-design-pr-merge.md) — how design docs become PR-backed and how `mark_chore_pr_merged` / `on_design_pr_merged` fire, the merge signals the 1&3 path hooks.
- [`project-design-doc-pointer`](project-design-doc-pointer.md) — the `design_doc_path` pointer used by the reverse resolver.
