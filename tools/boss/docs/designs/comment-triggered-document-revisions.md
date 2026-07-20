# Boss: Comment Intent Classification & Handling

- **Status:** implemented. All three phases plus a gap-closing wave merged between 2026-07-02 and 2026-07-05; this doc was revised post-implementation (2026-07-20) to describe as-built reality.
- **Implementation PRs:** Phase 1 — #1690 (1a), #1705 (1b), #1691 (1c), #1707 (1d) · Phase 2 — #1709 (2a), #1727 (2b), #1730 (2c), #1728 (2d), #1738 (2e), #1740 (2f) · Phase 3 — #1713 (3a), #1721 (3b), #1756 (3c), #1757 (3d) · gap-closing wave — #1765 (W1), #1794 (W2, the P529 Phase-2 macOS wiring), #1798 (W3), #1803 (W4), #1801 (async-viewer artifact wiring).
- **Known as-built gaps** (detailed in [Risks](#risks--open-questions)): the classifier is not gated by `resolve_doc_owner` and runs on every top-level comment regardless of artifact kind; a `CommentsSetIntent` manual override does not re-run routing; completion reconciliation does not push a `comment_topic` invalidation to open viewers.

> **Supersedes** the original narrower scope of this design ("larger change → revision/update task" only). Restructured around a **three-intent comment model**: every new comment on a design/investigation doc is classified by an LLM into exactly one of **directive**, **question**, or **larger change**, and routed accordingly. Buckets 1 and 3 (directive, larger-change) share one handling path — the revision/update-task machinery below — and **replace the per-comment "magic wand" entirely** (see [Migration](#migration-retiring-the-magic-wand)). Bucket 2 (question) is new: a read-only mini-coordinator agent answers in the comment thread. The classifier (bucket 1) is the prerequisite piece — nothing else in this design can run before every comment has an intent.

Extend the in-app markdown-viewer comment system so that, for **design** and **investigation** documents, every comment triggers engine-owned handling appropriate to what the operator actually wants: a nudge to start a revision (directive / larger-change), or an answer in the thread (question). This closes the loop between reviewing a doc and either getting it changed or getting it explained — today only the change path partially exists (via the magic wand, which this retires) and the question path has **zero support**.

This design is a **thin producer on top of three mechanisms**: the comment model from [`comments-in-markdown-viewer`](comments-in-markdown-viewer.md) (P529), the revision substrate from [`revision-tasks`](revision-tasks.md) (P654), and — new in this revision — an **LLM-backed intent classifier** and a **read-only answer agent**, both engine-owned. In the vocabulary of [`unify-pr-remediation-on-revisions`](unify-pr-remediation-on-revisions.md) (P707), the directive/larger-change path is a new **Source** — an in-app "doc-comment" producer alongside the operator (Source A), deferred GitHub PR-comment triage (Source B), merge-conflict (Source C), and CI-fix (Source D) producers. The question path is not a P707 Source at all — it never creates a task; it is a self-contained read-only conversation loop.

## Why this is a rewrite, not an extension

The original design assumed every comment implied a wanted change and asked one question ("does this doc have unresolved comments that should trigger a revision batch?"). That's still true for two of the three real cases, but it silently had no model for the most common one: an operator asking "why did you choose X over Y?" or "what does this mean?" is not asking for an edit, and forcing it through the revise-or-ignore banner is wrong on both branches — reviser churns on a comment with no actionable change, ignore loses the question entirely. The classifier is the missing piece that makes routing correct instead of assumed. Everything from the original design (comment state machine additions, association model, the revision-vs-chore decision, reconciliation) survives, relabeled as the **1&3 branch** of a three-way router; the magic wand — a hold-over single-comment path this design was originally going to leave coexisting — is now explicitly removed, because the classifier supersedes its reason to exist (bucket 1&3 nudges toward the same revision path the magic wand used to auto-apply into, and bucket 2 covers what the magic wand never could).

## Current-state grounding

This section describes the codebase **as it stood before implementation** — it is the baseline the design was argued against, kept for the record. The magic-wand surfaces described below have since been removed (2e, PR #1738), the reverse resolver now exists (`resolve_doc_owner`, PR #1691), and the macOS comment UI has since been engine-backed (W2, PR #1794). Line numbers are as of design time and will drift; the module/function names are the durable anchors. The engine crate root is `tools/boss/engine/core/` (the `core` crate), protocol is `tools/boss/protocol/`, macOS app is `tools/boss/app-macos/`.

### Comment model (P529 — engine implemented, UI Phase-1 in-memory)

- Table `work_comments` — DDL in `engine/core/src/work/migrations_b.rs:1072` (`migrate_work_comments_table`), invoked from `schema_init.rs:306`. Columns: `id, artifact_kind, artifact_id, doc_version, anchor_json, body, author, status, status_actor, last_resolved_with, plain_text_projection_version, created_at, updated_at, dismissed_at`, plus index `work_comments_by_artifact`.
- Status values (`protocol/src/types.rs:863`): `active`, `resolved`, `orphaned`, `dismissed`, `dispatched`; validated in `engine/core/src/work/comments.rs:91`. `active` is the default (`default_comment_status`, `types.rs:878`). This design adds `in_revision`, `answering`, `answered`, `awaiting_followup` (see [Comment/thread state machine](#commentthread-state-machine)) and retires `dispatched` (see [Migration](#migration-retiring-the-magic-wand)).
- `artifact_kind ∈ {work_item, pr_doc}`; `artifact_id` is either a work-item id (engine-owned description) or the synthetic `pr_doc:<repo_remote_url>:<branch>:<path>` for a markdown file on a PR branch.
- Structs: `WorkComment` (`types.rs:3103`), `CommentAnchor` (`types.rs:835`, a `{exact, prefix, suffix}` W3C `TextQuoteSelector`), `CommentResolution` (`types.rs:887`), `CreateCommentInput` (`types.rs:1176`).
- RPCs (`protocol/src/wire.rs:260`): `CommentsCreate`, `CommentsList`, `CommentsResolve`, `CommentsDismiss`, `CommentsSetStatus`, `CommentsUpdateAnchor`, `CommentsDispatchMagicWand`, `CommentsApplyMagicWand`, `CommentsDiscardMagicWand`. Handlers in `engine/core/src/app/comments.rs`; routed at `app.rs:2311`. Subscription helper `comment_topic()` (`wire.rs:58`). The three `*MagicWand` RPCs are **removed** by this design (see [Migration](#migration-retiring-the-magic-wand)); `CommentsClassify` (implicit, see [Classifier](#the-classifier-p1-foundation)) and `CommentsReviseDoc` / answer-agent RPCs are added.
- A sibling table keyed to comments already exists: `magic_wand_dispatches` (DDL `migrations_b.rs:1017`, `id, comment_id, artifact_kind, artifact_id, doc_version, status, input_tokens, output_tokens, result_md, error_kind, anchor_warning, created_at, resolved_at`, plus `chore_id` added at `migrations_b.rs:1047`; index `magic_wand_dispatches_by_comment`). This table and its handlers (`handle_comments_dispatch_magic_wand` / `_apply_magic_wand` / `_discard_magic_wand`, `engine/core/src/app/comments.rs:251,611,659`, backed by `crate::magic_wand::dispatch`, `magic_wand.rs`) are **retired** by this design, not preserved alongside the new routing (see [Migration](#migration-retiring-the-magic-wand)).
- The macOS UI (`app-macos/Sources/Comments/`: `CommentLayer`, `CommentSidebar`, `CommentHighlightOverlay`, `CommentPopover`, `MagicWandResultSheet`, wired into `DesignsView.swift` and `DesignRendererView.swift` via `.withComments()`) was **Phase-1 in-memory only** at design time — comments did not persist through the engine RPCs. Persisting the UI against the (already-built) RPCs was framed as prerequisite plumbing outside this design, but in practice it was delivered _inside_ this project as the gap-closing W2 (PR #1794) — see [UI / thread behavior](#ui--thread-behavior-thin-client). `MagicWandResultSheet` is deleted as part of the magic-wand removal; its role (showing an applied/staged diff) has no successor in v1 — bucket 1&3 never auto-applies, it only nudges (see [Buckets 1 & 3](#buckets-1--3--unified-directive--larger-change)).

### How a doc maps to its owning work item and PR

- Every task carries `tasks.pr_url` (`schema_init.rs:62`) — the single source of the owning PR for **any** kind.
- The doc-location pointer lives on the **project** for design tasks: `projects.design_doc_repo_remote_url / design_doc_branch / design_doc_path` (`schema_init.rs:45`, migration `migrate_project_design_doc_columns`, `migrations_b.rs:81`). CLI `boss project set-design-doc` (`cli/src/main.rs:296`, input `SetProjectDesignDocInput`, `types.rs:2619`, store `WorkDb::set_project_design_doc`, `products_design.rs:159`).
- Project-less items (investigations, project-less design tasks) carry a per-task doc triple `tasks.doc_repo_remote_url / doc_branch / doc_path` (`migrate_tasks_doc_pointer_columns`, `migrations_b.rs:629`).
- Every project has exactly one `kind='design'` task at `ordinal=0`, dispatched with execution kind `project_design` (this very task). The worker writes `tools/boss/docs/designs/<slug>.md`, opens a normal PR (`compose_design_directive`, `runner.rs:1262`). `active → in_review` is `WorkerCompletion::finalize_pr_transition` (`completion.rs:2637`), which writes `tasks.pr_url` (`pr_flow.rs:96`); routing at `completion.rs:2847` sends `kind==Design && project_id.is_some()` rows to `design_detector::on_design_pr_detected` (`design_detector.rs:58`), which scans the PR's changed files for a single `docs/designs/*.md` off the **head** branch (so the doc is fetchable while the PR is open) and records the pointer. `TaskKind` enum: `{Chore, Design, …, Investigation}` (`types.rs:2633`).
- **The reverse mapping — doc path → owning task → PR state — did not exist** at design time. Every lookup was forward (task id → doc): `resolve_project_design_doc` (`products_design.rs:225`), `task_doc_path` (`products_design.rs:560`). There was no `WHERE design_doc_path = …` / `WHERE doc_path = …` query. The reverse resolver shipped as `WorkDb::resolve_doc_owner` (1c, PR #1691) and is load-bearing for buckets 1&3's routing and bucket 2's spawn gate (see [Decision model](#the-revision-vs-general-task-decision)) — though **not**, as it turned out, for classifier eligibility (see the as-built note under [The classifier](#the-classifier-p1-foundation)).

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
- **Wiring the macOS comment UI to the engine RPCs.** That is separate, prerequisite P529 Phase-2 plumbing; this design assumes persisted comments and depends on it. _(As-built: this non-goal did not survive contact. The thin-client tasks 1d/2f/3d landed as in-memory stubs while the prerequisite was still missing, and the P529 Phase-2 wiring was then pulled into this project as gap-closing task W2 (PR #1794), with W3/W4 (PRs #1798/#1803) swapping the stubs for the real RPCs.)_
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
- `question` → the engine spawns the read-only mini-coordinator directly from the classifier's completion path (as-built there is no client-callable spawn RPC — `spawn_answer_agent` is engine-internal), flips the comment to `answering`, and on completion posts the agent's reply and flips to `answered`.

Both share the underlying comment/thread state machine and reconciliation infrastructure; only the routing target differs.

- **Pros:** all decisioning and atomicity live in the engine; the client renders thread state and never classifies or routes; the classifier is a single foundational choke point everything else depends on; reuses `assert_parent_revisable`, `create_revision`, `create_chore`, `mark_chore_pr_merged`, and the comment RPCs; the bucket-2 path is additive and doesn't disturb 1&3's already-designed mechanics.
- **Cons:** requires the new classifier (latency, cost, misclassification handling) and the new read-only agent execution mode (a capability boundary that must be enforced, not just documented) in addition to the reverse resolver and association column the original design already needed.
- **Chosen.** Detailed below.

For the sub-decisions inside C, the chosen options (argued in place) are: **classification via a single engine LLM call per top-level comment, with manual override** (not silent, not unclassified-until-clicked); **association via a column on `work_comments`** for the 1&3 path (not a join table); **reply-as-engine-authored-thread-entry** for both paths (not a new general threaded-comment row type); and **outright removal of the magic wand**, not coexistence (not deferred convergence).

## Chosen approach

### The classifier (P1 — foundation)

Every other piece of this design routes on the classifier's output, so it is built and validated first.

**Trigger.** On every new **top-level** comment (`CommentsCreate` with no parent/thread-position — the base comment model is single-level, so in practice this is every comment). **Replies within a bucket-2 thread are also classified** (see [Reclassifying follow-ups](#reclassifying-follow-ups)), but replies are a new thread-entry concept this design introduces, not a second top-level comment — see [Comment/thread state machine](#commentthread-state-machine).

> **As-built divergence (scope guard).** The design intended classification to be gated on `resolve_doc_owner` resolving to a `Design`/`Investigation` task, so the classifier "never runs on out-of-scope artifacts." As shipped, **the classifier runs unconditionally on every top-level comment on every artifact kind** — including `work_item` comments — and the doc-owner scope guard is enforced _downstream_ instead: at answer-agent spawn (`spawn_answer_agent` leaves an unresolvable comment `active` with no bucket-2 affordance), at `CommentsReviseDoc` (`NotApplicable`), and on the banner read (`revisable` false). P1a (PR #1690) landed before the resolver (1c) and flagged the missing gate; nothing went back to wire it, and the code comment in `app/comments.rs` acknowledges "the classifier itself runs unconditionally today." Consequence: out-of-scope comments still cost a classifier LLM call and get a nudge thread entry posted on a `directive`/`larger_change` result, with no `[Revise]` affordance to follow it. Flagged in [Risks](#risks--open-questions) as follow-up work.

**Where it runs.** Engine-side, synchronously triggered from the `CommentsCreate` handler but executed as a detached async task (`tokio::spawn`, mirroring the old magic-wand dispatch pattern of spawning work off the request path and publishing completion over `comment_topic`) — the create RPC itself returns immediately with the comment in a `classifying` state; classification is not on the create request's critical path. As-built (`comment_classifier` module, PR #1690): a single-shot **Claude Haiku 4.5** call, API key resolved from `BOSS_INTENT_CLASSIFIER_API_KEY` falling back to `ANTHROPIC_API_KEY` (the same dedicated-billing-bucket pattern the magic wand and attention detector used). Transient failures are retried inside `classify`; once retries are exhausted (or no API key is configured) the failure is recorded terminally on the comment row (`intent_classification_failed_at` / `intent_classification_error`, columns added after P1a) and published, so the UI shows a failed state instead of an indefinite "classifying…" spinner.

**Input.** The comment body, its anchor (`exact`/`prefix`/`suffix` — gives the classifier the quoted doc text, not just the raw comment), the doc's plain-text projection around the anchor for local context, and — for a reply — the prior thread turns (see below). No repo-wide context is given to the classifier itself; it is a fast, cheap, single-call step, not the answer agent.

**Output.** One of `directive | question | larger_change`, plus a confidence score. Written to new columns on `work_comments` (shipped verbatim in `migrate_work_comments_intent_columns`, schema v22, PR #1690; the two failure columns were added later):

```sql
ALTER TABLE work_comments ADD COLUMN intent TEXT;              -- 'directive'|'question'|'larger_change', NULL while classifying
ALTER TABLE work_comments ADD COLUMN intent_confidence REAL;    -- 0.0–1.0, engine-reported
ALTER TABLE work_comments ADD COLUMN intent_classified_at TEXT; -- when the LLM call completed
ALTER TABLE work_comments ADD COLUMN intent_overridden_by TEXT; -- NULL, or 'user' when manually reclassified
ALTER TABLE work_comments ADD COLUMN intent_classification_failed_at TEXT; -- terminal classifier failure (post-P1a addition)
ALTER TABLE work_comments ADD COLUMN intent_classification_error TEXT;
```

The transient `classifying` state is represented as **`intent IS NULL`** — no new `work_comments.status` value was added (the design left this as an implementer's call; P1a settled it). `WorkDb::set_comment_intent` is guarded on `intent_classified_at IS NULL`, so a raced/duplicate classifier completion is a no-op.

**Latency/UX while it runs.** The comment renders immediately (optimistic, as today) with a small "classifying…" badge in place of the intent chip; no banner, no thread entry, no agent spawn happens until `intent` is non-NULL. This is a few-hundred-ms–to-low-seconds LLM round trip; the badge is the only UX concession, matching the existing thinking-indicator pattern reused for bucket 2.

**Misclassification / override.** The classification is **not** a black box the operator is stuck with: the sidebar's intent badge is clickable and lets the operator manually reclassify a comment (`CommentsSetIntent{comment_id, intent}`, shipped in PR #1705; wired to the real RPC in the macOS client by W3, PR #1798). As-built, `WorkDb::override_comment_intent` overwrites any prior classification with no `intent_classified_at IS NULL` guard, **clears `intent_confidence`** (a manual override has no numeric confidence — the override _is_ the classification, no re-classification LLM call), and stamps `intent_overridden_by='user'`, preserved permanently as an audit trail distinguishing engine calls from human corrections.

> **As-built divergence (override routing).** The design promised that an override "re-runs routing from that point" — e.g. a comment the classifier mislabeled `question` gets the 1&3 nudge once overridden to `directive`, and an override _to_ `question` spawns the answer agent. As shipped, `handle_comments_set_intent` only persists the override and publishes a comment invalidation — **no routing re-runs**. PR #1705 landed before either routing path existed ("there is nothing else to re-run today") and the later routing PRs never revisited the override handler. Consequences today: overriding to `question` never spawns an answer agent; overriding to `directive`/`larger_change` posts no nudge entry (the comment does become eligible for the next `[Revise]` batch, since batch selection keys on `intent` + `status='active'` at click time, so the 1&3 half degrades gracefully; the bucket-2 half is simply unreachable via override). Flagged in [Risks](#risks--open-questions) as follow-up work.

**Reclassifying follow-ups.** As-built (3c, PR #1756): the operator's reply arrives via a dedicated **`CommentsPostFollowup` RPC** (not a `CommentsCreate` variant as originally sketched), which appends an `operator_followup` thread entry, transitions `answered → awaiting_followup`, and reclassifies off the request's critical path via `comment_classifier::classify_followup` (same three-intent rubric, with the original comment and prior thread turns as context). `reclassify_comment_intent` — unlike the once-only `set_comment_intent` — always overwrites, and **clears `intent_overridden_by`** rather than stamping `'user'` (a follow-up is a fresh engine classification event, not a human correction). A reply added to a comment already in the `answered`/`awaiting_followup` state (bucket 2) is classified fresh, with the prior thread's Q&A turns as classifier input, into the same three-way intent: another `question` re-enters bucket 2 (answer agent runs again with full thread context); a `directive`/`larger_change` follow-up ("ok then just add a section on X") routes into the bucket 1&3 path, **bridging** the thread's proposed edits (if the agent suggested any) into the revision's directive — see [Bridging a bucket-2 answer into a revision](#bridging-a-bucket-2-answer-into-a-revision). A follow-up on a comment already `in_revision` (bucket 1&3 in flight) is _not_ reclassified — mirrors the original design's "comments added after a revision is in flight form a new batch," except here the new activity is a reply on the same comment row, so it is treated as a fresh top-level-equivalent event once the existing batch resolves.

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

As-built notes on the diagram: `classifying` is not a `status` value — it is `intent IS NULL` (with the terminal-failure columns distinguishing "still classifying" from "classification failed"); the new persisted statuses are `in_revision` (2a), `answering`/`answered` (3b), and `awaiting_followup` (3c). One transition the diagram lacks shipped in 3b's failure path: if the answer agent dies without ever replying (crash / out of turns), the Stop-time finalizer (`finalize_answer_agent`) marks the run `failed`, posts an apology thread entry, and force-transitions the comment `answering → answered` — so a comment never sits in `answering` forever. 3c also added a compensation transition `answering → awaiting_followup` for a failed follow-up re-spawn.

| From                | To                                       | Trigger                                                   | Who                                                        | Idempotency                                                                                                                            |
| ------------------- | ---------------------------------------- | --------------------------------------------------------- | ---------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| (new comment)       | `classifying`                            | `CommentsCreate` on an eligible doc                       | engine                                                     | comment starts here; `intent` NULL                                                                                                     |
| `classifying`       | `active` (1&3) or `answering` (bucket 2) | classifier completes                                      | engine (`CommentsClassify`)                                | only fires once per `intent_classified_at IS NULL`; a re-run on an already-classified row is a no-op unless it's an explicit override  |
| `active`            | `in_revision`                            | `[Revise]` batch addresses this comment                   | engine (`CommentsReviseDoc`)                               | only `active`→`in_revision`; a comment already `in_revision`/`resolved`/`dismissed`/`orphaned` is skipped, so double-Revise is a no-op |
| `in_revision`       | `resolved`                               | the associated task reaches `done` (its PR merged)        | engine (reconciliation, hooked on the terminal transition) | only `in_revision`→`resolved`; re-firing on an already-`resolved` row is a no-op; sets `last_resolved_with`                            |
| `in_revision`       | `active`                                 | the associated task is abandoned / its PR closed-unmerged | engine (reconciliation)                                    | only `in_revision`→`active`; clears `revise_task_id`; re-firing is a no-op                                                             |
| `in_revision`       | `active`                                 | manual override (reviewer un-dispatches)                  | user (`CommentsSetStatus`)                                 | allowed; clears `revise_task_id`                                                                                                       |
| `answering`         | `answered`                               | answer agent posts its reply                              | engine (agent completion callback)                         | only fires once per agent run id                                                                                                       |
| `answered`          | `awaiting_followup`                      | operator posts a reply in the thread                      | engine (`CommentsPostFollowup`)                            | rejected (not queued) unless the comment is `answered`                                                                                 |
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

**Reverse resolver** `resolve_doc_owner(artifact_kind, artifact_id) -> Option<DocOwner>` where `DocOwner { task_id, task_kind, chain_root_id, pr_url, pr_lifecycle }` (shipped in PR #1691). As-built, `pr_lifecycle` is `DocOwnerPrLifecycle { Open, Merged, NoPr }` — a **cheap, DB-only summary** derived from `tasks.pr_url`/`tasks.status`, deliberately coarser than the merge poller's live-probe `PrLifecycleState` and with **no `ClosedUnmerged` variant** (see the decision-table note below). Artifact-id parsing uses `rsplitn(3, ':')` so repo remote URLs containing colons (`git@host:owner/repo.git`, `https://`) parse unambiguously. Note the resolver does **not** gate classifier eligibility as originally intended (see the as-built divergence under [The classifier](#the-classifier-p1-foundation)); it gates answer-agent spawn, `CommentsReviseDoc`, and the banner read:

1. Parse `artifact_id`. For `pr_doc:<repo>:<branch>:<path>`:
   - If `<branch>` is an execution's engine-supplied `expected_branch` (`boss/exec_*`), map branch → execution → task. That task is the doc's owner; read `tasks.pr_url` and its cached PR poll-state.
   - Else (e.g. `<branch> = main` after merge) match `projects WHERE design_doc_repo_remote_url=<repo> AND design_doc_branch=<branch> AND design_doc_path=<path>` → the project's `kind='design'`, `ordinal=0` task; and `tasks WHERE doc_repo_remote_url=<repo> AND doc_branch=<branch> AND doc_path=<path>` → project-less investigation/design tasks.
2. For `artifact_kind='work_item'`: not a design/investigation doc — return `None` (scope guard; no classification, no banner, no bucket-2 either — the magic wand's old exclusive territory now has **no** comment-driven affordance, per [Migration](#migration-retiring-the-magic-wand)).
3. Resolve `pr_lifecycle` from the owner's `pr_url` + the poll-state columns the merge poller already maintains; a present `pr_url` with no terminal marker reads as **Open**.

**Decision table** (evaluated engine-side, authoritative at click):

| Owner resolves to              | PR lifecycle              | Action                                                                                                                   |
| ------------------------------ | ------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| design/investigation task      | **Open** (unmerged)       | `create_revision(parent = owner's chain root)` — a `revision` that adds a commit to the doc's existing PR.               |
| design/investigation task      | **Merged**                | `create_chore` — general task to update the (now-landed) doc; opens a fresh PR against `main`.                           |
| design/investigation task      | **ClosedUnmerged**        | `create_chore` — general task (the old PR is dead; fresh work).                                                          |
| design/investigation task      | **no PR** (`pr_url` NULL) | `create_chore` — general task (nothing to amend).                                                                        |
| not a design/investigation doc | —                         | no-op; RPC returns `NotApplicable`; no banner (classification still happens — see the classifier's as-built divergence). |

Both vehicles autostart. Both carry `created_via = doc-comment:<artifact_kind>:<artifact_id>` and both get the addressed comments stamped with `revise_task_id`.

As-built (PR #1709), the **ClosedUnmerged row is not a distinct resolver state**: `DocOwnerPrLifecycle` has no `ClosedUnmerged` variant, so a closed-unmerged PR reads as `Open` from the cheap DB summary and the chore fallback is reached via the revision gate instead — `create_revision` fails `assert_parent_revisable` with a genuine `RevisionGateError` and `revise_doc` falls through to `create_chore` (any _other_ error propagates rather than being silently reinterpreted as a gate refusal). Same outcome as the table, different mechanism. Two further shipped details: `create_revision`/`create_chore` are called with `force_duplicate(true)` so the recent-duplicate guard never preempts the guarded comment `UPDATE`, which is the single source of race arbitration; and `AlreadyInFlight{task_id}` is only reported when the competing claim is still genuinely `in_revision`, not a stale `revise_task_id` left from a completed batch.

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

Handler `handle_comments_revise_doc` (`engine/core/src/app/comments.rs`, delegating to `WorkDb::revise_doc` in `work/revise_doc.rs` as-built), doing, **in one transaction** where it touches comment rows:

1. `resolve_doc_owner(artifact_kind, artifact_id)`; if `None`/not design-or-investigation → `NotApplicable`.
2. Load `active` comments with `intent ∈ {directive, larger_change}` (or the `comment_ids` intersection); if empty → `NoUnresolvedComments`.
3. Apply the decision table → build a `create_revision` or `create_chore` call. For a revision, `assert_parent_revisable(chain_root)`; on gate failure (raced merge) fall through to the chore branch.
4. Create the work item with `created_via="doc-comment:<kind>:<artifact_id>"`, autostart on.
5. `UPDATE work_comments SET status='in_revision', revise_task_id=<task>, status_actor='engine', updated_at=now WHERE artifact_...=... AND status='active' AND intent IN ('directive','larger_change') [AND id IN (...)]`.
6. Publish on `comment_topic(artifact)` so viewers refetch; return `Created{…}`.

**Reconciliation** (engine-owned, hooked on existing terminal signals — no new poller), shipped in 2c (PR #1730):

```rust
fn reconcile_comments_for_task(tx, task_id, outcome: {Resolved | Reopened})
```

- `Resolved`: `UPDATE work_comments SET status='resolved', status_actor='engine' WHERE revise_task_id=? AND status='in_revision'`. **Deliberate deviation from the original sketch:** the design had this also set `last_resolved_with='revise:<task_id>'`, but in the shipped schema `last_resolved_with` is the anchor-resolution-mode field (`exact`/`fuzzy`/`orphan`, driving the sidebar's ⚠ glyph) — overloading it would destroy anchor history for no benefit. Instead `revise_task_id` is **left set on the resolved comment** as provenance of which batch addressed it.
- `Reopened`: `UPDATE … SET status='active', revise_task_id=NULL, status_actor='engine' WHERE revise_task_id=? AND status='in_revision'`.

Called from every place a task reaches a terminal state — merge → resolved via `mark_chore_pr_merged` / `flip_in_review_revisions_to_done`'s per-revision fan-out. The **reopen-on-abandon half shipped ahead of its soft dependency**: rather than wait for `chore-lifecycle-pr-closed-unmerged`, 2c took the design's own suggested interim mitigation and wired a minimal, comment-only `reopen_comments_for_closed_unmerged_pr` into the merge poller's previously-no-op `ClosedUnmerged` arm (reconciling the direct task match plus every revision in the chain, without touching the task's own lifecycle), with a `merge_poller.comments_reopened` metric and a `comments_reopened_on_pr_closed_unmerged` work-item event.

> **As-built gap (no live push).** Reconciliation runs inside the DB transaction layer and never publishes a `comments.artifact.*` topic invalidation — an already-open viewer doesn't see the resolved/reopened chip until it reloads for an unrelated reason. Surfaced by W3 (PR #1798) while deleting the client-side stand-ins; flagged in [Risks](#risks--open-questions) as follow-up work.

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

This deliberately mirrors the shape of `magic_wand_dispatches` (comment-keyed, per-run row, status + result) because it is solving the analogous problem — track one ephemeral LLM call against a comment — just with a different capability profile and a thread-reply instead of an apply/discard result. The table shipped as specified in 3a (PR #1713), with CRUD guarded so the `running → replied|failed` completion transition is idempotent.

**As-built spawn/reply mechanics** (3b, PR #1721):

- **Spawn** (`spawn_answer_agent`, fired from the classifier's completion path on `intent = question`): resolves the doc owner (the scope-guard point — an unresolvable artifact leaves the comment `active` with no bucket-2 affordance), requires the owner task to carry a `repo_remote_url`, flips `active → answering` (guarded), creates the `answer_agent_runs` row **and a real `answer_agent` `work_execution` bound to the comment id**, then kicks the coordinator so the normal dispatch pipeline (cube lease → spawn) picks it up. A synthetic `Chore` work item (`synthetic_answer_agent_work_item`, mirroring the triage pattern) lets the task-centric spawn plumbing serve a comment-bound execution. If a DB write fails partway, the handler compensates — marks any just-created run `failed` and returns the comment to `active` — so a comment can never strand in `answering` with no execution behind it.
- **Prompt** (`compose_answer_agent_prompt`): the doc content fetched via `gh api` at the doc's own ref, the quoted anchor span, any prior thread entries, and the single `boss comment reply` instruction.
- **Reply**: `boss comment reply --body <TEXT>` → the worker-callable `CommentsPostAnswer` RPC. The target run is resolved from the caller's **own `BOSS_RUN_ID`** (→ its bound execution → the comment → that comment's `running` run); nothing in the request names a comment, so a read-only session can only ever reply to its own thread. On success: run `replied`, `entry_kind='answer'` thread entry appended, comment `answering → answered`.
- **Failure path** (`finalize_answer_agent`, at Stop): a run still `running` when the session ends (crash, out of turns) is marked `failed`, an apology thread entry is posted, and the comment is force-transitioned to `answered` — never left `answering` forever. Either way the execution is finalized and the pane + cube workspace released.
- **Local-only**: the remote wrapper launches workers with `--dangerously-skip-permissions`, which would bypass the read-only sandbox, so `host_adapter` **fails closed** and refuses to spawn an answer agent remotely. Answer agents run locally until the remote wrapper can honour a restricted permission mode.

**Thinking indicator.** While `status='running'`, the sidebar shows a thinking/typing indicator in the thread (the same visual language a chat UI uses for "the other party is typing"), driven by the comment's `answering` status + the existence of a `running` `answer_agent_runs` row, pushed over `comment_topic`. No polling — the same subscription mechanism the rest of the comment system already uses.

**Reply.** On completion, the agent posts a comprehensive reply as an engine-authored thread entry (same "reply" mechanic as the 1&3 nudge — see [Reply/link mechanics](#replylink-mechanics)), the comment flips `answering → answered`, and `answer_agent_runs.status='replied'`. The reply **may include concrete proposed edits** ("I'd suggest changing the retry backoff to exponential — here's a sketch: …") but the agent has no mechanism to apply them; it is prose (with optional embedded diff/snippet), not a patch the engine executes.

**Follow-up loop.** The operator's next reply in the thread is classified (see [Reclassifying follow-ups](#reclassifying-follow-ups)):

- Another question → `answered → answering` (re-entry), agent runs again with the accumulated thread as context (`thread_turn` increments).
- A request for changes → bridges into the revision path.

#### Bridging a bucket-2 answer into a revision

When a follow-up reclassifies to `directive`/`larger_change` (either the operator explicitly says "yes, make that change" or a fresh directive comment references the prior answer), the comment's `status` moves to `active` (see state machine) and is picked up by the next `[Revise]` batch exactly like any other directive comment — **with one addition**: if the most recent `answer_agent_runs` row for this comment has a non-null `reply_body` containing a proposed edit, that reply text is included verbatim in the revision/chore's directive (`compose_revision_directive` / the chore's doc-edit directive already assembles "path + anchors + comment bodies" per the original design — the bucket-2 reply is appended to that assembly as additional context, not a replacement for the comment body itself). This means the revision worker sees both the original question, the agent's proposed answer/edit, and the operator's follow-up confirming they want it — strictly more context than a 1&3 comment gets today, at no cost to the shared machinery. Shipped in 3c (PR #1756): `compose_doc_comment_directive` (the directive assembly `CommentsReviseDoc` uses) appends, per comment, the latest answer-agent reply and any operator follow-up bodies from that comment's thread; the bridge transition (`awaiting_followup → active`) also posts the same nudge thread entry the top-level classifier posts.

### Reply/link mechanics

Both bucket 1&3's nudge and bucket 2's answer are **engine-authored thread entries** on the comment, not new user-facing comment rows and not general-purpose threading. Concretely, this design adds one small append-only table shared by both (shipped in 2b, PR #1727, exactly as below; `nudge` writes landed there, `answer` in 3b, `operator_followup` in 3c):

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
- All of this is engine-computed state pushed over `comment_topic`; the client refetches on push and renders — no decisioning, no PR-state knowledge, no classification, no task-creation logic, no agent-spawn logic in the client. There is deliberately **no app-callable classify RPC** — classification and answer-agent spawning are engine-internal, fired by `CommentsCreate`/`CommentsPostFollowup`; the app's only writes are comment CRUD, `CommentsSetIntent`, `CommentsReviseDoc`, and `CommentsPostFollowup`.

**How this actually shipped.** The thin-client tasks (1d PR #1707, 2f PR #1740, 3d PR #1757) all landed _before_ the P529 Phase-2 persistence prerequisite existed, as in-memory stubs that logged in place of RPCs — following the old `dispatchMagicWand` stub pattern. A four-part gap-closing wave then made them real:

- **W1 (PR #1765):** the engine read path grew `CommentWithThread { comment, thread_entries, answer_agent_running }`, changing `FrontendEvent::CommentsList`'s payload from `Vec<WorkComment>` — the per-comment thread/indicator shape this section specifies. Banner state ships separately as the `CommentsBannerState` companion RPC (2d, PR #1728) rather than on `CommentsList` itself.
- **W2 (PR #1794):** engine-backed `CommentLayer` — the deferred P529 Phase-2 wiring, delivered inside this project. Swift Codable wire mirrors (`CommentWire.swift`), `EngineClient` send/decode for the `comments_*` RPCs, artifact identity threaded through both viewers (`pr_doc:<repo>:<branch>:<path>` from `DesignRendererView`, `work_item:<id>` from the task-description viewer), real W3C `TextQuoteSelector` anchoring with a SHA-256 plain-text-projection `doc_version`, a resolve-on-load loop with ⚠/orphan glyphs, and a `CommentEngineBridge` subscribing to `comments.artifact.<kind>:<id>` for live updates. Comment authorship is stamped `user:<NSUserName()>` — a placeholder until the app has a real identity notion.
- **W3 (PR #1798):** the 1d/2f stubs became real calls — `CommentsSetIntent`, `CommentsBannerState`, `CommentsReviseDoc` (non-`Created` outcomes render a transient sidebar message). The client-side `resolveRevision`/`reopenRevision` stand-ins were deleted outright since engine reconciliation owns those transitions.
- **W4 (PR #1803):** the 3d stubs became real — `postFollowup` calls `CommentsPostFollowup`; the manual `FollowupClassificationBadge` menu was deleted in favour of a passive "Reclassifying…" indicator (the engine's async reclassifier is authoritative); `ThinkingIndicatorView` is driven by the wire `answer_agent_running` flag rather than trusting `status == answering` alone.
- **PR #1801:** the async raw-URL design-doc viewer (`AsyncMarkdownViewerView`) — previously in-memory-only because its payload lacked repo/branch/path — now threads a `CommentArtifactRef` from `ResolvedDesignDoc`, so comments on docs opened via raw GitHub URLs are engine-backed too.

Interactive end-to-end verification of the full flow (post a question on a real doc, watch the agent think and reply, follow up, observe both reclassifier outcomes; comments surviving app restart) has **not** been performed — both W2 and W4 flag it as outstanding, since it needs a live engine + app window no headless worker has.

### Concurrency/idempotency

- **Double-click / rapid re-Revise.** Unchanged from the original design: the `active→in_revision` flip is a single guarded `UPDATE … WHERE status='active'` inside the create transaction; a racing second call finds nothing left and returns `AlreadyInFlight{task_id}`.
- **Rapid-fire follow-up questions.** The design called for queuing a follow-up posted while the answer agent is still `answering`; **as shipped (3c, PR #1756), `CommentsPostFollowup` instead rejects with a `WorkError` on any status other than `answered`** — a deliberate simplification, called out in that PR, that still guarantees at most one live answer agent per comment. The client mitigates: the follow-up composer only renders on an `answered` comment. Queuing remains the eventual UX if the rejection proves annoying in practice (see [Deferred / future](#deferred--future-not-a-v1-blocker)).
- **Multiple reviewers / concurrent threads across different comments.** Independent — each comment's classification, `[Revise]` batch membership, and answer-agent run are all keyed by `comment_id`/`artifact_id`; nothing serializes across comments except the batch `UPDATE` within one `[Revise]` click.
- **Reconciliation idempotency.** Both `Resolved` and `Reopened` updates are guarded on `status='in_revision'`; re-running a sweep never double-transitions. Answer-agent completion is guarded on `answer_agent_runs.status='running'` so a duplicate completion callback is a no-op.

### Scope guard

Gated at the one authoritative point: `resolve_doc_owner` returns an owner only when the resolved task's `kind ∈ {Design, Investigation}`. `artifact_kind='work_item'` or a `pr_doc` owned by a plain chore/project-task PR → `None` → no banner, no `[Revise]`, no bucket-2 agent. Detection is engine-side and derived from `tasks.kind`, never sniffed in the UI. **As-built caveat:** the guard is enforced at the routing/read surfaces but _not_ at classification time — out-of-scope comments are still classified (and nudged) even though nothing downstream will ever act on the result; see the divergence note under [The classifier](#the-classifier-p1-foundation).

## Migration: retiring the magic wand

Shipped as specified in 2e (PR #1738) — removal, data migration, and the kept-but-unread table all landed as designed; the only nuance worth recording is that the migration keys each `dispatched` comment's disposition off its _most recent_ `magic_wand_dispatches` row. The magic wand is not deprecated-but-kept; it is **removed**, because the classifier supersedes both of its use cases (bucket 1&3's nudge-then-revise replaces its silent-apply-a-single-comment path; bucket 2 covers what it never could — questions). This section is new relative to the original design, which had explicitly deferred this decision ("they coexist in v1... future convergence noted as not a v1 blocker"); this revision makes the removal explicit and immediate, since keeping a silent-apply path around while the classifier exists is actively wrong on the question bucket.

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
- **The answer agent's read-only sandbox guarantees.** The design asserts enforcement happens "at the tool-availability layer," mirroring the existing revision PreToolUse hook — but the revision hook blocks exactly one command (`gh pr create`); the answer agent needs a much larger _allowlist-not-blocklist_ posture (no mutating RPC reachable at all, not just one blocked command). ~~_Open question:_ what is the concrete mechanism — a distinct execution-kind dispatch path with a hard-coded reduced tool table (recommended, and what this design assumes), vs. a runtime capability-token check on every RPC call?~~ **Resolved (P3a):** the **hard-coded reduced tool table** — the recommended option — implemented via Claude Code's native deny-by-default `dontAsk` permission mode, not a runtime capability-token check (rejected as new machinery). Concretely, the `answer_agent` execution kind maps to a new `WorkerKind::AnswerAgent` whose worker settings set `permissions.defaultMode: "dontAsk"` (auto-denies every tool call except `permissions.allow` matches and built-in read-only Bash) with an explicit `permissions.allow` allowlist of exactly the read-only tools plus the single thread-reply command, plus a comprehensive `permissions.deny` belt (deny always wins over allow) covering the known-catastrophic mutating surfaces (`Edit`/`Write`/`NotebookEdit`, push/PR, all of `cube`). The dispatch layer additionally forces `--permission-mode dontAsk` at launch so the mode can never be downgraded to `auto` or `--dangerously-skip-permissions` (both of which would defeat the allowlist). See `worker_setup::{permissions_value, answer_agent_allow_rules, answer_agent_deny_rules, worker_kind_for_execution}` and the `answer_agent` module. The remaining risk (unchanged) is that the enforcement is only as strong as Claude Code's honoring of `dontAsk` + deny rules — worth a periodic re-verification as the harness evolves.
- **Concurrency of multiple threads / multiple answer agents.** v1's answer-agent concurrency guard is per-comment only (see [Concurrency](#concurrencyidempotency)); it does not cap how many answer agents run simultaneously _across_ comments/docs system-wide. If a reviewer leaves ten questions on one doc, ten answer agents (each possibly leasing a workspace) spin up near-simultaneously. _Open question:_ does this need a global concurrency cap / queue, or is per-comment independence acceptable for v1? Not decided. (Manifest.)
- ~~**Reopen-on-abandon has no terminal signal today**~~ **Resolved (2c, PR #1730):** the "minimal comment-only reopen hook" option was taken — `reopen_comments_for_closed_unmerged_pr` is wired into the merge poller's previously-no-op `ClosedUnmerged` arm, reopening addressed comments (direct task match plus the whole revision chain) without touching the task's own lifecycle. `chore-lifecycle-pr-closed-unmerged` remains unimplemented; when it lands, task-lifecycle handling can join the comment hook.
- **The reverse resolver is a new trust path** (carried over): `resolve_doc_owner` infers owner from branch/path; a mismatch could mis-route both classification eligibility and 1&3 routing. _Mitigation:_ prefer the execution-branch mapping (exact) over the path match; ambiguous path match → `None` (no classification/banner at all) rather than guess.
- **Batch-vs-subset for `[Revise]`.** v1 addresses all `directive`/`larger_change`-classified unaddressed comments in one batch. _Open question:_ ship subset selection in v1 or defer? (Manifest, carried over.)
- **Work-item comments lose all comment-driven affordance.** Called out in [Migration](#migration-retiring-the-magic-wand) as an explicit coverage regression versus the magic wand's prior scope. _Open question:_ is this acceptable, or does work-item description commenting need its own (smaller?) slice of the three-intent model before/alongside this ships? (Manifest.)
- **Bridging a bucket-2 proposed edit into a revision directive** relies on the agent's free-text reply containing something structured enough to quote usefully (see [Bridging](#bridging-a-bucket-2-answer-into-a-revision)). If the agent's answer is prose-only with no concrete proposed diff, the bridge degrades gracefully to "just the comment + follow-up, no proposed-edit context" — acceptable, but means bridge quality is only as good as how concretely the agent chose to phrase its answer, which isn't independently guaranteed. Noted as a soft risk, not blocking.
- **General-chore / revision doc-edit directive quality** (carried over, still applies to the 1&3 path): the chore/revision must be told precisely which file + which comments (now: plus any bridged bucket-2 context) to address. _Mitigation:_ shipped as designed — `compose_doc_comment_directive` assembles doc path, quoted anchors, comment bodies, and bridged thread context.

### As-built gaps surfaced post-implementation

Three places where shipped reality falls short of what this doc promises, each a candidate for scheduled follow-up work:

- **The classifier is not gated by `resolve_doc_owner`.** It runs (and spends an LLM call) on every top-level comment on every artifact kind, and posts nudge entries on out-of-scope `directive` results nothing downstream will act on. P1a flagged this for 1c's implementer; the gate was never wired at classification time. See [The classifier](#the-classifier-p1-foundation).
- **`CommentsSetIntent` overrides do not re-run routing.** Overriding to `question` never spawns an answer agent; overriding to `directive`/`larger_change` posts no nudge (though the next `[Revise]` batch still picks the comment up). The design's "re-runs routing from that point" promise is unimplemented. See [Misclassification / override](#the-classifier-p1-foundation).
- **Reconciliation publishes no `comment_topic` invalidation.** Resolve-on-merge and reopen-on-abandon mutate comment rows inside the DB layer with no push, so open viewers show stale chips until an unrelated reload. Surfaced by W3 (PR #1798). See [Reconciliation](#buckets-1--3--unified-directive--larger-change).

Additionally, **interactive end-to-end verification** of the engine-backed comment UI (P529 Phase 2's own acceptance criterion, plus the live answer-agent loop) has not been performed — flagged as outstanding by both W2 (PR #1794) and W4 (PR #1803); it needs a human at a real app window.

## Implementation task breakdown (all shipped)

PR-sized tasks in dependency order, organized by the three phases the operator called out: **P1 (classifier + intent model) was the explicit prerequisite for everything else**. Every task below has merged; each is annotated with its PR. A fourth, unplanned **gap-closing wave** (below) followed the three phases, because the thin-client tasks shipped as stubs against the missing P529 Phase-2 prerequisite and the read path lacked the thread shape this doc specified.

### Phase 1 — the classifier + intent model (prerequisite)

**1a. Intent columns + classifier plumbing.** Add `work_comments.intent / intent_confidence / intent_classified_at / intent_overridden_by` (migration mirroring `migrate_work_comments_table`). Add the `classifying` transient comment state (or represent it as `intent IS NULL` without a new `status` value — implementation detail for the classifier task itself to settle). Wire the detached-async classifier call off `CommentsCreate`, writing the intent columns on completion and publishing on `comment_topic`. **Effort:** `medium`. **Dependencies:** none. **Shipped:** PR #1690 (`classifying` = `intent IS NULL`; Haiku 4.5 classifier; no scope gate — see Risks).

**1b. `CommentsSetIntent` manual override RPC.** Add the RPC + handler that lets the sidebar's badge reclassify a comment, setting `intent_overridden_by='user'` and re-running routing from the new intent's entry point. **Effort:** `small`. **Dependencies:** 1a. **Shipped:** PR #1705 (override + invalidation only; routing re-run never wired — see Risks).

**1c. Reverse doc→owner resolver.** `resolve_doc_owner(artifact_kind, artifact_id) -> Option<DocOwner>` as specified above — this gates _both_ classifier eligibility and 1&3 routing, so it belongs in Phase 1 even though the original design scoped it under the revision-routing work. **Effort:** `medium`. **Dependencies:** none (parallel with 1a/1b). **Shipped:** PR #1691 (`DocOwnerPrLifecycle {Open, Merged, NoPr}` — no `ClosedUnmerged` variant).

**1d. Classification badge (thin client).** Render the classifying/directive/question/larger-change badge + override control in `CommentSidebar`/`CommentPopover`. **Effort:** `small`. **Dependencies:** 1a, 1b; **external prerequisite:** P529 UI persistence. **Shipped:** PR #1707 as an in-memory stub; made real by W3 (PR #1798).

### Phase 2 — unify buckets 1 & 3, remove the magic wand

**2a. `CommentsReviseDoc` RPC + routing + comment transition.** As specified in [Buckets 1 & 3](#buckets-1--3--unified-directive--larger-change): the `revise_task_id` column, `ReviseDocInput`/`ReviseDocOutcome`, `handle_comments_revise_doc` filtering on `intent ∈ {directive, larger_change}`, the decision table, autostart + `created_via`, the guarded batch `UPDATE`. **Effort:** `medium`. **Dependencies:** 1a, 1c. **Shipped:** PR #1709.

**2b. Nudge reply on classification.** Post the `entry_kind='nudge'` `comment_thread_entries` row immediately on `directive`/`larger_change` classification (before `[Revise]` is even clicked). Requires the `comment_thread_entries` table. **Effort:** `small`. **Dependencies:** 2a. **Shipped:** PR #1727 (also introduced the shared `comment_thread_entries` table).

**2c. Completion reconciliation — resolve on merge / reopen on abandon.** Unchanged in substance from the original design's tasks 4–5: `reconcile_comments_for_task`, hooked into `mark_chore_pr_merged` / `flip_in_review_revisions_to_done` for resolve, and the not-yet-wired closed-unmerged signal for reopen (soft-depends on `chore-lifecycle-pr-closed-unmerged`, per [Risks](#risks--open-questions)). **Effort:** `medium` (resolve) + `small`/`medium` (reopen, depending on which mitigation option is chosen). **Dependencies:** 2a. **Shipped:** PR #1730, including the minimal comment-only reopen hook (no wait on `chore-lifecycle-pr-closed-unmerged`) and the `last_resolved_with` deviation.

**2d. Banner state on the comment read path.** Extend `CommentsList`/a `CommentsBannerState` read with `{revisable, unresolved_count (directive/larger_change only), in_revision_count, doc_kind}`. **Effort:** `small`. **Dependencies:** 1c, 2a. **Shipped:** PR #1728 as the `CommentsBannerState` companion RPC.

**2e. Magic-wand removal.** Delete the three RPCs, the three handlers, `magic_wand.rs`, the store methods, `MagicWandResultSheet.swift`, the sidebar trigger affordance, and the `dispatched` status value from validation; run the one-time `migrate_retire_magic_wand_dispatched_comments` data migration. Leave `magic_wand_dispatches` table in place, unread (see [Migration](#migration-retiring-the-magic-wand)). **Effort:** `medium`. **Dependencies:** 2a, 2b (the replacement paths must exist before the old one is deleted, so operators aren't left with neither). **Shipped:** PR #1738.

**2f. macOS `[Revise]` banner + `in_revision`/nudge chips (thin client).** Render the banner, wire `[Revise]`, render nudge/`in_revision`/resolved/reopened chips and thread entries. **Effort:** `medium`. **Dependencies:** 2a, 2b, 2d; **external prerequisite:** P529 UI persistence. **Shipped:** PR #1740 as an in-memory stub; made real by W3 (PR #1798).

### Phase 3 — the bucket-2 read-only answer agent + thread conversation loop

**3a. `answer_agent_runs` table + `answer_agent` execution kind + capability-restricted dispatch.** The new table, the new execution kind distinct from existing worker kinds, and the reduced tool/RPC surface enforced at the dispatch layer (per [Risks](#risks--open-questions) — this task must resolve the allowlist-vs-capability-token open question before implementation). Includes read-only workspace-lease wiring. **Effort:** `large`. **Dependencies:** 1a, 1c (needs the doc-owner resolver and classified intent to trigger on). **Status:** the allowlist-vs-capability-token open question is resolved (hard-coded reduced tool table via `dontAsk`; see [Risks](#risks--open-questions)). The dispatch surface, `WorkerKind::AnswerAgent`, and the `answer_agent_runs` table/CRUD ship in P3a; the read-only lease is provided engine-side (the answer-agent worker gets an already-leased read-only checkout — it is denied `cube` entirely — rather than running `cube workspace lease` itself). The read-only engine-query commands in the allowlist and the actual `boss comment reply` command are wired in 3b alongside the spawn path that exercises them. **Shipped:** PR #1713, with a focused adversarial security review of the sandbox (no concrete v1 bypass found; residual risks documented in the PR: unrestricted read means a prompt-injected agent could exfiltrate via its one reply, and write-prevention ultimately rests on Claude Code honouring `dontAsk`'s read-only-Bash classification — worth periodic re-verification). Also from that review: `Bash(boss comment reply:*)` trusts the command's args, so the reply command must stay strictly single-purpose.

**3b. Answer-agent spawn + reply + thinking indicator.** Wire `question`-classified comments to spawn a `3a` agent run, transition `active→answering`, push the thinking indicator over `comment_topic`, and on completion post the `entry_kind='answer'` thread entry and transition `answering→answered`. **Effort:** `medium`. **Dependencies:** 3a. **Shipped:** PR #1721 (adds `CommentsPostAnswer` / `boss comment reply`, the Stop-time failure finalizer, and the synthetic work-item spawn plumbing).

**3c. Follow-up reclassification loop.** Operator replies become `entry_kind='operator_followup'` rows; reclassify per [Reclassifying follow-ups](#reclassifying-follow-ups); route `question` back into 3b, route `directive`/`larger_change` into the [bridge](#bridging-a-bucket-2-answer-into-a-revision) (comment → `active`, bridged context appended for the next `[Revise]` batch to pick up via 2a's directive assembly). **Effort:** `medium`. **Dependencies:** 2a, 3b. **Shipped:** PR #1756 (as the dedicated `CommentsPostFollowup` RPC; rejects rather than queues a follow-up mid-`answering`).

**3d. Thread UI (thin client).** Render the thinking indicator, answer entries, follow-up composer, and the bridge transition (comment moving from the bucket-2 visual track to the bucket-1&3 track) in `CommentSidebar`. **Effort:** `medium`. **Dependencies:** 3b, 3c, 1d; **external prerequisite:** P529 UI persistence. **Shipped:** PR #1757 as an in-memory stub; made real by W4 (PR #1803).

### Gap-closing wave (added scope, unplanned)

Four tasks added after Phase 3 closed, when review showed the design's client surface was still entirely stubbed and the read path incomplete — plus one adjacent viewer fix:

**W1. Expose thread entries + `answer_agent_running` on the comment read path.** `CommentWithThread` wrapper; `CommentsList` payload change. **Shipped:** PR #1765.

**W2. Engine-back `CommentLayer`** — the P529 Phase-2 macOS wiring this design had scoped out as an external prerequisite, delivered here instead: wire mirrors, `EngineClient` comment RPCs, artifact identity through both viewers, W3C anchoring + SHA-256 `doc_version`, `CommentEngineBridge` live updates. **Shipped:** PR #1794.

**W3. Swap the 1d/2f stubs for real RPCs** (`CommentsSetIntent`, `CommentsBannerState`, `CommentsReviseDoc`); delete the client-side reconciliation stand-ins. **Shipped:** PR #1798 (and surfaced the reconciliation-push gap in Risks).

**W4. Swap the 3d thread-UI stubs for the real answer-agent flow** (`CommentsPostFollowup`, `answer_agent_running`-driven thinking indicator, passive reclassify indicator). **Shipped:** PR #1803.

**Async-viewer artifact wiring.** `AsyncMarkdownViewerView` (raw-URL design docs) gains a `CommentArtifactRef` so its comments are engine-backed like the renderer's. **Shipped:** PR #1801.

### Deferred / future (not a v1 blocker)

- **Subset selection** for `[Revise]` batches — the reserved `comment_ids` filter. Depends on 2a.
- **Low-confidence classification UX** (distinguishable "uncertain" state instead of silently picking a bucket). Depends on 1a.
- **Global answer-agent concurrency cap.** Depends on 3a.
- **Queue (rather than reject) a follow-up posted mid-`answering`** — the design's original concurrency sketch; 3c shipped rejection as a deliberate simplification.
- **Remote answer-agent spawning** — currently fails closed (local-only) because the remote wrapper can't honour a restricted permission mode.
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
