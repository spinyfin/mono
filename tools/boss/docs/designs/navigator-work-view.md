# Navigator Work: one task row across every revision

- Date: 2026-11-14
- Status: proposed design; implementation waits for design review
- Product: Boss macOS; alternative presentation of Work
- Related designs: [Work Kanban](work-kanban.md), [revisions](revision-tasks.md), [review guides](automatic-pr-review-guides.md)

The central choice is a single stable row for the original task, whose section reflects the whole revision family. A dense navigator sits beside a persistent detail area; Kanban remains available over the same work.

## Goals

- Show what agents are doing, what needs a person, what is waiting, and what has landed in four expandable sections.
- Keep the original task selected through revision, review, conflict resolution, CI repair, and merge.
- Make the current revision's ask and live agent visible without losing access to the original brief or PR review.
- Have the engine own membership, family resolution, action readiness, and ordering; the app renders a projection.
- Support product and project scope, compact accessible rows, keyboard operation, and live updates without interrupting reading or typing.

## Non-goals

Replace Kanban; change dispatch, merge, or review policy; give revisions separate navigator rows; implement a diff viewer; create a new terminal emulator or markdown engine; change project lifecycle; add drag-to-change-status in the navigator. No implementation is part of this document's PR.

## Alternatives considered

| Approach                                           | Useful precedent                                                                           | Why not chosen here                                                                                                                                                                                                     |
| -------------------------------------------------- | ------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Render the four Kanban lanes as a vertical list    | Existing board columns and their gestures already express workflow transitions             | Doing includes queued work; Done includes merging PRs. A direct relabel would violate Working's activity meaning and Landed's merge meaning. Keep those semantics in Kanban.                                            |
| One flat list with state filters                   | Search already gives a compact route to a known item                                       | Mutually exclusive filters hide the other three populations; the requested overview needs simultaneous counts and expandable sections. Search remains a way to find a row.                                              |
| Separate or nested revision rows                   | Kanban currently folds some revisions after review while active revisions can remain cards | A revision would change row identity or create multiple entries for one PR. The chosen invariant explicitly requires one original-task row, including active revisions. Revision history belongs in detail.             |
| Derive sections in Swift from existing task badges | Some existing lane routing lives in the app                                                | Badges do not distinguish queued repair from running repair or provide all family/question inputs atomically. Adding another client reconciliation path would violate engine ownership. Reuse inputs, not that routing. |

This is a design choice supported by source inspection, not a usability study comparing measured outcomes. Later captures and interaction tests validate the chosen design; they do not establish its superiority over alternatives.

## Chosen approach

### Existing foundations and gaps

Paths below are relative to the repository root. Observations describe the inspected source, not guarantees about future revisions.

| Source                                                                                                                          | Reuse and implication                                                                                                                                                                                                                                                                         |
| ------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tools/boss/protocol/src/types/task.rs`, `execution.rs`, `review_section.rs`                                                    | Typed task/execution statuses, runtime waits, revision attribution, CI/review projections. `ready_for_review` alone does not exclude an AI review; do not equate it with permission to merge.                                                                                                 |
| `tools/boss/engine/work-db/src/workitems.rs`, `revision_helpers.rs`, `review_section.rs`                                        | Full-tree assembly and family enrichment. Existing review sections deliberately put blocked above revision; Navigator needs running revision above passive PR blockers.                                                                                                                       |
| `tools/boss/engine/board-projection/src/lib.rs`, [board projection contract](../board-projection.md)                            | Atomic family refresh, old/new relationship closure, incremental/full parity and snapshot recovery. Extend this path instead of creating another cache or subscription.                                                                                                                       |
| `tools/boss/app-macos/Sources/ChatViewModel+WorkBoardSections.swift`                                                            | Existing lanes and partial revision folding are not the proposed Navigator classifier. Kanban remains unchanged.                                                                                                                                                                              |
| `tools/boss/app-macos/Sources/DesignsView.swift`, `MarkdownDocumentChrome.swift`, `WorkFormSheets.swift`                        | Markdown rendering and existing task-description editing/save flow. Inspection found a markdown viewer and description form fields, not a standalone embeddable full markdown editor. Add a small inline edit/preview adapter over these facilities; do not claim a ready-made editor exists. |
| `tools/boss/app-macos/Sources/Ghostty/WorkersWorkspaceModel.swift`, `TerminalPaneSession.swift`, `BossPaneTerminalView.swift`   | Existing pane/session ownership and Ghostty rendering; embed a selected session without spawning another worker.                                                                                                                                                                              |
| `tools/boss/app-macos/Sources/WorkBoardCardOperatorQuestion.swift`, `AttentionsView.swift`, `ChatViewModel+Attentions.swift`    | Typed questions, explanations, choices and attention-group actions. Extract reusable bodies with existing callers retained.                                                                                                                                                                   |
| `tools/boss/app-macos/Sources/ChatViewModel+ReviewGuide.swift`, `ReviewGuideViewerHeader.swift`, `ReviewGuideAttachments.swift` | Versioned guide loading, review actions, attachments and stale-response guards. Current loading targets a singleton viewer; inline detail needs an independently keyed request destination.                                                                                                   |
| `tools/boss/engine/work-db/src/pr_flow.rs`, `pr_state.rs`                                                                       | A merge observation drives completion, but `done`/`completed_at` are not proof of merge. Human completion and other terminal paths exist. Persist explicit merge evidence for the current PR binding.                                                                                         |

No recorded Navigator precedence decision was found in these sources. The rules below are new decisions, not a reconstruction of an assumed old policy.

### Population, identity and scope

A row represents a non-deleted original task plus every recursively linked revision, regardless of revision origin or status. The engine emits its root ID; the app never walks parent links to decide membership. A malformed revision chain is surfaced by the engine as one diagnostic fallback row for the unresolved item in Backlog, with an unavailable-parent explanation; it is never silently dropped. Such a row is an integrity exception, not normal revision presentation.

Task, project task, chore, design, investigation, design postmortem and accepted follow-up all use the same rules. Follow-ups have independent deliverables and are independent rows, with an origin link in detail. An unaccepted follow-up proposal is an attention, not yet a task. Engine-minted PR-review, merge-conflict and CI-fix revisions fold into the original row. Independent review executions are activity on that row, never work items themselves.

Product scope includes project tasks and unprojected chores/investigations/follow-ups, retaining the existing Work population exclusions for automation-owned work and deleted items. Project scope selects roots in that project; their revisions remain included even if child metadata differs. Dependency links outside scope remain visible in detail and offer an explicit scope-changing navigation action. No project rows appear inside the four sections. The scope header has a project-information button opening existing project information/design-doc surfaces; project-only attentions remain accessible through an attention count in that header and the existing attention surface, not duplicated onto every task.

Every included root has exactly one section. Archived roots are outside the default Work population, matching existing archive behavior; restoring one recomputes its section. A selected item archived or deleted elsewhere remains as a read-only detail with an explanatory banner until the user navigates away. A root whose merge is known is Landed even if an old child still says running; cleanup belongs to engine reconciliation and an independently converted follow-up gets its own row.

### Engine contract

Add an optional, versioned `navigator` projection to each task in the existing work-tree snapshot/delta protocol. Roots carry `section`, engine-sorted `sort_key`, `reason`, `default_detail`, ordered current activity references (task/execution/run IDs), revision summary, actionable attention references, confirmed merge evidence, and available action descriptors. Folded revisions carry only their owning root reference. Use builder defaults for additive fields. A missing projection from an older engine disables Navigator with an inline compatibility message and leaves Kanban usable; Swift must not guess membership.

Implement the bounded pure family classifier in a small `engine/work-navigator` crate depending only on protocol types. The existing work-db assembly adapts authoritative inputs and invokes it, including the real full-tree and incremental paths in the same PR. It must not depend upwards on engine core. Bazel visibility is restricted to its consumers. No new RPC or parallel work subscription is needed; existing question, review-guide, brief-edit and mutation RPCs remain the action paths.

Introduce durable merge evidence keyed by root and canonical PR binding, written by the real merge-observation path, including observations of already-terminal tasks. Never infer it from a URL plus `done`, or from a successful merge-request response. Use the existing PR classification helper. Rebinding to a different PR cannot inherit evidence. Legacy rows without evidence stay Backlog with `Completed; merge unverified` (or `Completed without PR`); do not issue GitHub requests during board reads or invent a historical backfill. Historical exactness is disclosed rather than fabricated. This requires a small independent schema migration before the projection; use a unique UTC registry ID with `legacy_version: None` and upgrade tests.

Projection reads use one database snapshot for family, activity, actionable questions, PR and merge evidence. Add these inputs to durable journal coverage or the existing full-snapshot invalidation class. Runtime-only changes must refresh the family even if `tasks.status` did not change. Attention-answer and merge-evidence transactions must invalidate the owning family. Full and incremental output must agree on every navigator field; bounded closure overflow and sequence gaps recover using the existing full snapshot.

### Membership: first matching rule wins

| Priority | Authoritative combined-family condition                                                                               | Section         | Primary explanation                                                              |
| -------- | --------------------------------------------------------------------------------------------------------------------- | --------------- | -------------------------------------------------------------------------------- |
| 1        | Confirmed merge of the root's current PR                                                                              | Landed          | Merged; time and PR                                                              |
| 2        | At least one current worker/reviewer is actually progressing                                                          | Working         | Implementing / Revising / Fixing CI / Resolving conflict / Reviewing             |
| 3        | Unresolved human action: question, live input wait, review request, merge decision, or explicit operator intervention | Needs attention | Answer / Input needed / Review / Ready to merge / Intervention                   |
| 4        | Everything else                                                                                                       | Backlog         | Engine reason for waiting, failure, parking or completion without merge evidence |

“Progressing” means a current, nonterminal execution/run whose authoritative engine state is running and not awaiting human input, suspended, or proven exited. Driver thinking/tool-use/output changes decorate that state; silence alone does not remove it. Claimed workspace setup is Backlog `Starting`, not an active agent. Running review agents count as Working; an `ai_reviewing` hold that is only waiting for consolidation admission does not. Deterministic verdict application without an agent is Backlog `Applying review`. The engine must resolve activity from run/execution state; the app must not reinterpret a stale terminal surface as a live worker.

Working wins over a concurrent question from another family member. Keep a visible question count and an attention banner in detail so that human work remains discoverable without double-listing. If all active agents are waiting for input, rule 2 no longer matches and the row moves to Needs attention. Confirmed merge wins over everything; remaining questions are still accessible in Landed detail and the attention surface.

A passive failing check, conflict or failed run alone is not a request to a human. Automatic remediation remains Backlog until running. A persisted operator-intervention signal or exhausted remediation requiring a decision matches rule 3. Review-required PRs match rule 3 when a human review can usefully proceed against the current head and no automatic review/repair is pending; merge actions still obey the engine's stricter gates. Unknown CI or mergeability never implies readiness. An optional “Generate guide” button is not itself a reason to put a row in Needs attention.

### Exhaustive status and signal mapping

The following table supplies inputs to the ordered rules above, not a second competing classifier. All current enum variants are listed; future variants must fail exhaustive engine matches until a meaning is chosen.

| Status/signal                                                                           | Mapping absent a higher-priority fact                                                                                                                           |
| --------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Task `todo`                                                                             | Backlog; queued, parked, deferred or eligible reason                                                                                                            |
| Task `active`                                                                           | Working only with progressing activity; otherwise Needs attention for a human action, else Backlog `Starting`/`Waiting`/`Activity unavailable`                  |
| Task `blocked`                                                                          | Backlog with dependencies or blocking reason; explicit unanswered question/intervention elevates to Needs attention                                             |
| Task `in_review`                                                                        | Needs attention only for actionable review/merge; automatic work pending is Backlog; running review/repair is Working                                           |
| Task `done`                                                                             | Landed only with confirmed merge; otherwise Backlog `Completed without PR` or `Completed; merge unverified`, with no implied retry                              |
| Task `archived`                                                                         | Excluded by population filter; if restored, normal classification applies. Its historical child records remain available to its root                            |
| Execution `queued`, `ready`, `waiting_dependency`                                       | Backlog `Queued` / `Ready` / `Waiting on dependency`; eligibility is not activity                                                                               |
| Execution `claimed`                                                                     | Backlog `Starting`; no fake terminal                                                                                                                            |
| Execution `running`                                                                     | Working when current run is progressing; ended, suspended or inconsistent run state yields Backlog explanation until reconciled                                 |
| Execution `waiting_human`                                                               | Needs attention `Input needed`, with question or existing terminal input route                                                                                  |
| Execution `waiting_review`, `waiting_merge`                                             | Needs attention only if a human action is due; otherwise Backlog `Waiting for review` / `Merging` / `Waiting for checks`                                        |
| Execution `completed`                                                                   | Does not establish merge; classify remaining family/PR facts                                                                                                    |
| Execution `failed`, `abandoned`, `cancelled`, `orphaned`                                | No active-agent claim; Backlog with failure/history, unless a separately recorded human action elevates it                                                      |
| Revision queued / blocked / running / awaiting question / completed                     | Applies above to the original row; terminal child cannot complete the root; revision count and current ask remain in detail                                     |
| AI review `reviewing`                                                                   | Working only while reviewer/consolidator agent runs; admitted hold without running agent is Backlog with stage                                                  |
| AI `review_queued`, `not_reviewed`, `reviewed_with_findings`                            | Backlog while automatic admission/repair is due; Working if repair runs; Needs attention for explicit manual intervention                                       |
| AI `reviewed_all_clear`, `reviewed_clean_pending`, `review_not_required`                | No activity implied; current-head human action => Needs attention, pending checks => Backlog                                                                    |
| CI pending / unknown / failed / success                                                 | Indicators only; failed with queued repair => Backlog, running repair => Working, manual action => Needs attention; success alone is not merge readiness        |
| Review required / approved / changes requested / unknown                                | Required actionable human review => Needs attention; changes requested with automatic revision follows revision state; unknown never authorizes merge           |
| Merge conflict / deferred conflict                                                      | Running resolver => Working; queued resolver or deferred-until-sibling-lands => Backlog with blocker link; explicit human decision => Needs attention           |
| Merge requested, merge-when-ready, queue entry, queue CI                                | Backlog `Merging` while automatic progression needs no person; failure with actionable decision => Needs attention; confirmed merge => Landed                   |
| Focus hold, dispatch paused, capacity/quota/backoff, chain hold, future not-before time | Backlog with exact engine wait label and blocker/time; do not manufacture questions for intentional scheduling policy                                           |
| `autostart = false`, project parking, deferred scope                                    | Backlog `Parked` / `Deferred`; an actual scope-approval question elevates to Needs attention                                                                    |
| Human-driven task                                                                       | Backlog `Human work` unless an explicit human action is due; never Working merely because its task status is active                                             |
| Open/partly answered task attention or blocked signal                                   | Needs attention when still actionable; answered/actioned/dismissed/cleared records are history only. Project-only groups stay in scope-header attention surface |
| Unknown string reason or missing PR observation                                         | Backlog with literal diagnostic label, never silently dropped or presented as ready                                                                             |

For conflicting facts, the engine returns one winning reason plus secondary indicators. Tests must cover running revision + CI fail + question, deferred conflict + focus hold, merge + stale running child, and completed execution + open PR. These combinations are more important than tests of isolated status labels.

### Navigator rows and ordering

Default navigator width is 360 points, resizable from 260 to 520. Rows are 30 points high; accessibility text sizing can use a second line and a larger hit target. No cards, paragraph previews or per-row button strips.

```text
Work                           [ Kanban | Navigator ]
[ Product: Boss v ] [ Project: All v ]  [Search work...]
+--------------------------------------+----------------------------------+
| v Working                         3 | Improve startup                  |
| > <> Improve startup   [rev 2] ~ !1 | Working: revision 2, fixing CI    |
|   ? Audit migrations          ~     | [Activity] [Review] [History]     |
| v Needs attention                 2 |                                  |
|   <> Release workflow         ?2    | Original brief / Current ask     |
|   <> Update installer         PR    | + existing Ghostty agent pane    |
| v Backlog                        18 |                                  |
|   <> Navigation       [Focus held]  |                                  |
|   <> Cleanup             [Merging]  |                                  |
| > Landed                         42 |                                  |
+--------------------------------------+----------------------------------+
```

Indicators from leading to trailing: task-kind glyph; title; optional project label in product scope; revision count; one activity/PR-health summary glyph; actionable-question count. The question count always survives narrowing. Health summary priority is conflict, failed CI, requested review changes, pending checks, approved/green; tooltips and accessibility labels enumerate all underlying facts rather than hiding suppressed failures. Activity has its own leading animated dot beside kind when working, with an accessible textual label. Never use color alone.

At widths below 340, drop project label and revision count into tooltip/detail; below 300 retain kind/activity, truncated title, highest-priority health glyph and question count. Effort, driver name, review-cycle count, all CI checks and timestamps live in detail, not the row. A two-line accessibility layout carries title then reason; no third line. Tooltips are supplementary, not the only accessible route to information.

Fixed section order: Working, Needs attention, Backlog, Landed. Counts represent roots, not revisions or matching attention members. Working/Needs attention/Backlog start expanded; Landed starts collapsed. Preserve expansion per product/project scope. Empty sections retain a zero-count header and a short empty message when expanded.

Engine ordering, with root ID as final tie-breaker:

- Working: priority descending, then current activity start ascending. Heartbeats do not reorder rows.
- Needs attention: input/question first, then explicit intervention, then human review, then ready-to-merge; within each, priority descending, actionable-since ascending.
- Backlog: failed/interrupted first, then queued/starting, automatic PR waits including merging, dependency/focus holds, parked/deferred, and completed-without-merge-evidence last; within group use priority, project ordinal where present, creation time and root ID. No implied dispatch-order guarantee.
- Landed: confirmed merge time descending, using observation time labelled as such when actual merge time is unavailable.

Project scope hides project labels, but keeps the same membership and sorting rules. Search filters projected roots by root title and revision ask; a revision match selects its root and opens that ask. Filtered counts read “shown / total.” No automatic project grouping interrupts the four sections.

### Detail shell and variants

Every detail has a stable root title and project breadcrumb, engine status/reason, PR link and CI/conflict/review summary when applicable. Tabs are **Activity**, **Review** (when a PR exists), **Attention** (when questions/actions exist), and **History**. Initial tab follows `default_detail`; subsequent updates never switch the user's tab. A newly required action appears as a banner and tab badge. The tabs remain stable for a selected root until it is deselected, even if an attention is answered.

Working Activity uses a vertically resizable brief/terminal split. “Original brief” and “Current ask” are distinct labelled document choices. A running revision defaults to Current ask; root implementation defaults to Original brief. Show revision sequence, source (review findings, CI, conflict or manual), and the execution identity beside Current ask. Multiple simultaneous agents use an activity picker, stable by execution ID; the engine orders actionable waits then progressing activities, with creation time/ID tie-breaks. Changing that picker does not change the root selection or overwrite the user's document choice.

```text
WORKING — Activity
Improve startup                 PR #420  CI failed  Conflict none
[Activity*] [Review] [Attention 1] [History]
Revision 2 · Fix CI · Agent: Codex [activity v]
[Original brief] [Current ask*] [Preview | Edit] [Save]
| Repair the failing startup assertion...                     |
+--------------------- draggable divider ---------------------+
| Ghostty: existing execution session                         |
| Agent running tests...                                      |
```

The inline brief adapter reuses existing markdown rendering and task-description mutation; edit mode is plain markdown text with explicit Save/Cancel. Retain per-document dirty drafts across row/tab moves. Background updates never overwrite a draft: show “Brief changed remotely” and require reload or an explicit save after reviewing the latest content. Editing the stored brief does not imply delivery to the already-running agent; label that fact and retain the existing agent input route. Never repurpose the original brief field as the revision's ask.

```text
QUESTION — Attention
Release workflow                    Waiting for your answer
[Activity] [Attention*] [History]
Question 1 of 2 · from revision 1
Context: Publishing is waiting on the release target.
Question: Which channel should receive this release?
[Stable] [Preview]                    [Submit answer]
Recommendation: Preview for this trial.
> Technical details        Answer status: not sent
```

Use engine-provided labels for yes/no, multiple-choice and free-text prompts; do not synthesize Yes/No for split decisions. Reuse existing group semantics: distinguish saving an answer from accepting/actioning a group. Submission binds to attention ID/generation; an already-answered response replaces controls with the recorded answer without resubmitting. Errors are inline, the draft remains, and a waiting-human execution without a structured question offers “Open agent input” and the selected terminal instead. Follow-up proposals use existing edit/accept/dismiss controls in this tab.

```text
REVIEW — also reachable while Working
Improve startup                  PR #420 · CI running · rev 2 active
[Activity] [Review*] [Attention] [History]
Guide: version 3 · older head · [Generate/Retry when available]
+------------------------------+------------------------------+
| Generated review guide       | Diff preview                 |
| Summary / review steps       | Not available in this view   |
| Attachments / findings       | [Open PR diff on GitHub]     |
+------------------------------+------------------------------+
[Review actions from engine]   Merge unavailable: revision active
```

Reuse guide rendering, attachments, comments and action handlers. Fetch by root, version and request identity so a late response cannot overwrite another task or a newly selected version. Show not-generated, generating, failed, stale-source and readable states distinctly. A readable stale guide stays readable with its comparison/head label. The diff region is a static placeholder with an external PR-files link; no diff parsing, fetching or implementation task. At narrow detail widths stack guide and placeholder vertically. Merge/merge-when-ready stays subject to existing engine checks and confirmations; a tab never confers readiness.

```text
BACKLOG — Activity
Navigation                             Focus held
[Activity*] [History]
Brief [Preview | Edit]   [Save]
| Implement keyboard selection...                             |
Waiting: outside current dispatch focus
Dependencies: [Land shared selection]  [Protocol update]
[Existing Start/Park/Edit actions, when allowed]
```

Todo/queued/parked/deferred/blocked share this layout with an exact reason, retry/not-before time if known, dependency completion and links. A dependency link does not silently change scope. Starting shows workspace setup and “Agent pane not available yet.” Completed-without-PR and merge-unverified rows use the same shell with a completion summary, no Start implied by their section, and history links.

```text
FAILED / INTERRUPTED — Activity
Improve startup                        Last execution failed
[Activity*] [Review if PR] [History]
Failure: launch could not attach the worker
Last attempt / preserved work / diagnostics link
Brief [Preview | Edit]       Dependencies: none
[Retry when permitted]       [Open last transcript]
```

Failure is an execution outcome, not a new task status. Reuse existing dispatch failure diagnostics and mutation paths. Do not render a dead pane as working; retain transcript/history access. A separate human intervention can put this same variant in Needs attention.

```text
MERGING / AUTOMATIC WAIT — Review
Cleanup                                Merging · queue position 2
[Activity] [Review*] [History]
PR / required CI / queue status / last observation
Review guide                        [Diff placeholder]
Waiting for queue checks; no action required
[Existing cancel-merge action when available]
```

Merge request/queue wait is Backlog until merge is confirmed. A running conflict resolver or CI fixer instead uses Working Activity with Current ask and Ghostty; Review remains accessible with a banner identifying the repair. A deferred conflict uses Backlog Activity with the sibling blocker and resumes only when the engine changes state. No timer in Swift predicts that it has cleared.

```text
LANDED — Review (or completion summary when guide absent)
Cleanup                                Merged · Nov 14, 10:32
[Activity] [Review*] [Attention if any] [History]
Merged PR / confirmed timestamp / completion summary
Final readable guide (head/version labelled) [Diff placeholder]
Revision history: initial work -> CI fix -> merged
[Open PR] [Open related follow-up]
```

Landed is read-only for the completed attempt, retains remaining attention actions, and never shows a live terminal by default. New work uses existing follow-up actions rather than editing a historical attempt into apparent activity. History in all variants lists revisions, asks, execution outcomes and review versions, with links to transcripts. No PR means no Review tab. No selected task means “Select a task to see its brief, activity and review”; loading/error/disconnected states keep the last coherent detail and label its freshness, without a modal.

### Live updates, selection and switching

Root ID is the selection key, never a list index, section or execution ID. Applying a work-tree delta changes section membership and counts atomically. A selected row can move to a collapsed section without changing detail, terminal attachment, document scroll, tab, dirty text or keyboard focus. Show a quiet “Moved to Needs attention — Reveal” banner; Reveal explicitly expands/scrolls. Do not expand or scroll on background events. Maintain the top visible row's anchor where possible as other rows move.

When a current execution ends, keep its terminal/transcript selected and show “Execution ended; new revision available” rather than attaching a different agent under the user's typing. Selecting the new activity is explicit. Reuse the terminal session registry; moving between Work and Agents must not create duplicate PTYs or steal an existing first responder. Surface unavailable/remote sessions through the existing session availability rules.

Up/Down moves among visible rows, Home/End to first/last visible row, Left/Right collapses/expands a focused section, Return focuses detail, Escape returns focus to the selected row, and Tab traverses controls normally. These shortcuts apply only when the navigator owns focus; terminal/editor keystrokes pass through unchanged. VoiceOver announces section, root title, activity, question count and health summary. Background movement may be announced politely but never changes focus.

Add a labelled `Kanban | Navigator` control in the existing Work toolbar. Kanban remains the initial default; persist the chosen mode per window restoration state. Shared product/project scope and root selection carry both ways; Kanban reveals the root's existing card, and opening Navigator from a revision selection resolves its root via the engine projection. Keep each mode's independent scroll/expansion state. Existing Kanban grouping and lane filters remain mode-specific; do not silently carry a lane filter into Navigator. Shared text search may carry across with an explicit visible search field.

Changing scope is a user action: preserve selection if included, otherwise show an empty selection and retain the previous root in per-scope history. Switching mode preserves unsaved drafts and terminal sessions. If a root is excluded by a search or archive event, detail explicitly says why it is absent and offers clear-search/reveal where applicable. Disconnect disables mutations, labels data stale, and reconnect uses the existing snapshot/delta-base recovery. No speculative row movement on an action acknowledgement; authoritative updates determine membership.

### Acceptance evidence for implementation

Engine tests must enumerate every task/execution enum and family precedence case above. Exercise the actual work-db assembly and incremental projection after question answers, runtime transitions, review admission, conflict deferral, PR rebinding and merge; compare full output, including removal/reparenting and reconnect recovery. A synthetic Swift-only classifier fixture is insufficient evidence of engine ownership.

App tests and isolated captures must cover all mockup variants at 260/360/520-point navigator widths, large text, multiple activities, stale guide responses and answer-generation races. Use an isolated real engine/app path for the full sequence: original implementation -> PR -> running CI revision -> question -> answer -> review -> merge. Verify that the same root stays selected, Current ask names the revision, Review stays available during work, and typing is not redirected when a run changes. Record limitations if an external PR/merge cannot be exercised; do not present fixture success as real GitHub integration evidence. Tests run via Bazel with the existing sandbox; captures never launch the production app/engine.

## Risks / open questions

- **Review choice: terminal work without merge evidence.** Proposed behavior is Backlog with an explicit completed/unverified label, sorted last. This preserves the literal meaning of Landed and does not lose non-PR work, but may surprise users accustomed to Kanban Done. The alternative requires changing the section's meaning/name, not silently equating completion with merge.
- **Review choice: Working precedence.** A running family member keeps the root Working even when another member asks a question. The always-visible question count and Attention tab are essential. The alternative would contradict the agreed running-revision placement.
- **Historical merge coverage.** Existing terminal records may lack explicit merge evidence. The first implementation writes new evidence and leaves unknown history honest; it does not promise a mass historical GitHub reconciliation.
- **Editor integration.** The requested editing experience is supported as edit/preview over existing rendering and description mutation. A richer standalone editor was not found in source and is not assumed. Review should confirm this interaction is sufficient before implementation.
- **Density and terminal ownership.** Dimensions are design targets, not measured accessibility or usability results. Embedded Ghostty must preserve session ownership and input focus; validate through the real app path before enabling the mode.
- **Scope boundary.** Automation-owned work, archived roots and project-only questions keep their existing dedicated surfaces. The header must make project attentions reachable so absence of project rows does not hide them.

## Proposed implementation task breakdown

Breakdown size: 6 entries (6 in-scope, 0 deferred) — explicit merge evidence, engine family projection, navigator navigation, activity details, attention actions and inline PR review are six reviewable capabilities reusing the existing transport and UI components.

All entries await design approval. There is no diff-viewer implementation entry. After the navigator shell, Activity and Attention may run in parallel: they use separate detail bodies and narrow registered slots. Review follows both because it integrates their shared detail tabs and request ownership; it must forward-port those changes preservingly. Every entry includes its own tests rather than deferring correctness to a final gate.

### Record confirmed merge evidence

Persist a PR-binding-specific merge observation through the existing merge path and expose it to engine readers. This independently useful capability distinguishes actual merge from manual completion, including already-terminal observations, without querying GitHub while assembling work.

Major deliverables:

1. Unique UTC schema migration and typed reader/writer for root/PR merge evidence; no guessed legacy backfill.
2. Real merge-observation caller integration, binding-change behavior and transactional invalidation.
3. Bazel upgrade/merge-path tests for manual completion, repeated observations and rebinding.

Likely changed files:

- `tools/boss/engine/work-db/src/merge_evidence.rs` (new)
- `tools/boss/engine/work-db/src/lib.rs`
- `tools/boss/engine/work-db/src/schema_migration_registry.rs`
- `tools/boss/engine/work-db/src/schema_init.rs`
- `tools/boss/engine/work-db/src/board_projection_schema.rs`
- `tools/boss/engine/work-db/src/pr_flow.rs`
- `tools/boss/engine/work-db/src/tests/merge_evidence_tests.rs` (new)
- `tools/boss/engine/work-db/src/tests.rs`
- `tools/boss/engine/merge-poller/src/sweep.rs`
- `tools/boss/engine/merge-poller/src/tests/sweep_tests.rs`
- `tools/boss/engine/work-db/BUILD.bazel`

Estimated size: ~11 files / ~550 changed lines

Effort: medium

Dependencies: none

Scope: in-scope

### Project navigator families through the work tree

Add engine-owned section, detail reason, actions, activity references and sort keys to the existing tree, implementing the precedence table in a small pure crate with real work-db callers in the same PR. Preserve Kanban fields and behavior; wire fields are additive and unused by older clients.

Major deliverables:

1. Typed projection and family classifier with exhaustive status/combination tests, including all revisions and independent follow-ups.
2. Full/incremental assembly integration, durable input invalidation and confirmed merge consumption.
3. Real work-tree/delta parity tests for attention, runtime, PR and relationship changes; no separate RPC/cache.

Likely changed files:

- `tools/boss/protocol/src/types/navigator.rs` (new)
- `tools/boss/protocol/src/types.rs`
- `tools/boss/protocol/src/types/task.rs`
- `tools/boss/engine/work-navigator/src/lib.rs` (new)
- `tools/boss/engine/work-navigator/src/tests.rs` (new)
- `tools/boss/engine/work-navigator/Cargo.toml` (new)
- `tools/boss/engine/work-navigator/BUILD.bazel` (new)
- `tools/boss/engine/work-db/Cargo.toml`
- `tools/boss/engine/work-db/BUILD.bazel`
- `tools/boss/engine/work-db/src/lib.rs`
- `tools/boss/engine/work-db/src/navigator_projection.rs` (new)
- `tools/boss/engine/work-db/src/workitems.rs`
- `tools/boss/engine/work-db/src/revision_helpers.rs`
- `tools/boss/engine/work-db/src/mappers.rs`
- `tools/boss/engine/work-db/src/board_projection_schema.rs`
- `tools/boss/engine/work-db/src/board_patch.rs`
- `tools/boss/engine/work-db/src/tests/board_projection_tests.rs`
- `tools/boss/engine/board-projection/src/lib.rs`
- `Cargo.toml`
- `Cargo.lock`

Estimated size: ~20 files / ~1400 changed lines

Effort: large

Dependencies: Record confirmed merge evidence

Scope: in-scope

### Add the navigator shell and shared selection

Render the engine projection as four compact expandable sections, with scope/search, root selection and mode switching. Provide the persistent detail shell and registered content slots, using a basic brief/reason fallback until richer bodies land; the mode is usable and clearly labels unavailable detail integrations.

Major deliverables:

1. Projection decoding, compact accessible rows, engine ordering and scoped counts with compatibility handling.
2. Kanban/Navigator switch and persistent selection, per-mode scroll, atomic movement, keyboard and disconnected behavior.
3. Bazel UI/model tests for narrowed rows, collapsed destinations, reconnect and mode/scope changes.

Likely changed files:

- `tools/boss/app-macos/Sources/Models+Navigator.swift` (new)
- `tools/boss/app-macos/Sources/Models.swift`
- `tools/boss/app-macos/Sources/WorkNavigatorView.swift` (new)
- `tools/boss/app-macos/Sources/WorkNavigatorRow.swift` (new)
- `tools/boss/app-macos/Sources/WorkNavigatorDetail.swift` (new)
- `tools/boss/app-macos/Sources/ChatViewModel+Navigator.swift` (new)
- `tools/boss/app-macos/Sources/ChatViewModel+WorkSelection.swift`
- `tools/boss/app-macos/Sources/ChatViewModel+WorkItemEvents.swift`
- `tools/boss/app-macos/Sources/WorkSelectionHistory.swift`
- `tools/boss/app-macos/Sources/WorkBoardToolbar.swift`
- `tools/boss/app-macos/Sources/ContentView.swift`
- `tools/boss/app-macos/Tests/BossTests/NavigatorSelectionTests.swift` (new)
- `tools/boss/app-macos/Tests/BossTests/NavigatorRowTests.swift` (new)
- `tools/boss/app-macos/BUILD.bazel`

Estimated size: ~14 files / ~1200 changed lines

Effort: large

Dependencies: Project navigator families through the work tree

Scope: in-scope

### Embed briefs and current agent activity

Fill the Activity slot with original/revision document choices, inline edit/preview, existing Ghostty sessions and backlog/failure/completion variants. Preserve draft and terminal identity independently from section movement, and expose history/dependencies using existing models and actions.

Major deliverables:

1. Brief adapter with labelled current ask, explicit save, remote-change handling and per-document draft retention.
2. Current-activity picker and existing session embedding, plus dependency/failure/history states without false live indicators.
3. Bazel focus/draft/session tests and isolated working/backlog/failure/landed captures, including a real revision transition.

Likely changed files:

- `tools/boss/app-macos/Sources/WorkNavigatorActivity.swift` (new)
- `tools/boss/app-macos/Sources/WorkNavigatorBrief.swift` (new)
- `tools/boss/app-macos/Sources/WorkNavigatorHistory.swift` (new)
- `tools/boss/app-macos/Sources/WorkFormSheets.swift`
- `tools/boss/app-macos/Sources/WorkCardPopover.swift`
- `tools/boss/app-macos/Sources/ChatViewModel+WorkMutations.swift`
- `tools/boss/app-macos/Sources/Ghostty/WorkersWorkspaceModel.swift`
- `tools/boss/app-macos/Sources/Ghostty/BossPaneTerminalView.swift`
- `tools/boss/app-macos/Tests/BossTests/NavigatorActivityTests.swift` (new)
- `tools/boss/app-macos/Sources/BossNavigatorActivityCapture.swift` (new)
- `tools/boss/app-macos/BUILD.bazel`

Estimated size: ~11 files / ~1100 changed lines

Effort: large

Dependencies: Add the navigator shell and shared selection

Scope: in-scope

### Embed actionable questions and attention groups

Fill the Attention slot with existing typed question and group workflows, including revision-owned questions and terminal-only input waits. Reuse existing answer/action handlers and scope-header project attention routing; do not introduce a second question lifecycle.

Major deliverables:

1. Reusable question/group body retaining existing board and attention callers, with generation-safe submission and inline errors.
2. Navigator tab/banner/count integration and project-attention access without duplicate task rows.
3. Bazel tests and isolated question captures for all answer types, concurrent answer changes and Working-plus-question precedence.

Likely changed files:

- `tools/boss/app-macos/Sources/WorkNavigatorAttention.swift` (new)
- `tools/boss/app-macos/Sources/WorkBoardCardOperatorQuestion.swift`
- `tools/boss/app-macos/Sources/AttentionsView.swift`
- `tools/boss/app-macos/Sources/ChatViewModel+Attentions.swift`
- `tools/boss/app-macos/Tests/BossTests/NavigatorAttentionTests.swift` (new)
- `tools/boss/app-macos/Sources/BossNavigatorAttentionCapture.swift` (new)
- `tools/boss/app-macos/BUILD.bazel`

Estimated size: ~7 files / ~700 changed lines

Effort: medium

Dependencies: Add the navigator shell and shared selection

Scope: in-scope

### Embed PR review alongside ongoing work

Fill the Review slot with independently keyed guide loading, existing PR actions and a static diff placeholder. Complete shared detail integration for working revisions, review-ready work, automatic merge waits and landed history, preserving the prior Activity and Attention integrations.

Major deliverables:

1. Inline guide/version/attachment presentation with source-staleness labels and race-safe request ownership alongside the existing standalone viewer.
2. Existing review/merge controls and gate explanations, static diff placeholder and external link for every PR-bearing state.
3. Bazel tests and isolated review/merging captures; exercise the genuine engine-to-app lifecycle from revision through question and merge, documenting any external-service validation limits.

Likely changed files:

- `tools/boss/app-macos/Sources/WorkNavigatorReview.swift` (new)
- `tools/boss/app-macos/Sources/WorkNavigatorDetail.swift`
- `tools/boss/app-macos/Sources/ChatViewModel+ReviewGuide.swift`
- `tools/boss/app-macos/Sources/DesignsView.swift`
- `tools/boss/app-macos/Sources/ReviewGuideViewerHeader.swift`
- `tools/boss/app-macos/Sources/ReviewGuideAttachments.swift`
- `tools/boss/app-macos/Sources/ChatViewModel+ReviewActions.swift`
- `tools/boss/app-macos/Tests/BossTests/NavigatorReviewTests.swift` (new)
- `tools/boss/app-macos/Sources/BossNavigatorReviewCapture.swift` (new)
- `tools/boss/app-macos/BUILD.bazel`

Estimated size: ~10 files / ~1000 changed lines

Effort: large

Dependencies: Embed briefs and current agent activity; Embed actionable questions and attention groups

Scope: in-scope
