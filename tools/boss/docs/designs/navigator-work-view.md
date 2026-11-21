# Navigator Work: one task row across every revision

- Date: 2026-11-14 (design); 2026-11-17 (as-built)
- Status: shipped; Kanban remains the other Work mode
- Product: Boss macOS; alternative presentation of Work
- Related designs: [Work Kanban](work-kanban.md), [revisions](revision-tasks.md), [review guides](automatic-pr-review-guides.md), [board projection](../board-projection.md)
- Shipped as: [mono#3623](https://github.com/spinyfin/mono/pull/3623), [mono#3625](https://github.com/spinyfin/mono/pull/3625), [mono#3629](https://github.com/spinyfin/mono/pull/3629), [mono#3634](https://github.com/spinyfin/mono/pull/3634), [mono#3635](https://github.com/spinyfin/mono/pull/3635), [mono#3661](https://github.com/spinyfin/mono/pull/3661), [mono#3662](https://github.com/spinyfin/mono/pull/3662), [mono#3671](https://github.com/spinyfin/mono/pull/3671), [mono#3674](https://github.com/spinyfin/mono/pull/3674), [mono#3675](https://github.com/spinyfin/mono/pull/3675)

The central choice is a single stable row for the original task, whose section reflects the whole revision family. A dense navigator sits beside a persistent detail area; Kanban remains available over the same work.

## Verdict

The view is live in the Work toolbar as `Kanban | Navigator`. The engine owns section membership, family identity, ordering, and action readiness on each work-tree card; the app renders that projection and never re-derives it. A diff viewer is still a static placeholder with a GitHub files link.

## Goals

- Show what is ready to review or merge, what agents are doing, what needs a person, what is waiting, and what has landed in five expandable sections.
- Keep the original task selected through revision, review, conflict resolution, CI repair, and merge.
- Make the current revision's ask and live agent visible without losing access to the original brief or PR review.
- Have the engine own membership, family resolution, action readiness, and ordering; the app renders a projection.
- Support product and project scope, compact accessible rows, keyboard operation, and live updates without interrupting reading or typing.

## Non-goals

Replace Kanban; change dispatch, merge, or review policy; give revisions separate navigator rows; implement a diff viewer; create a new terminal emulator or markdown engine; change project lifecycle; add drag-to-change-status in the navigator.

The diff region remains a static placeholder. Historical merge evidence is not backfilled from GitHub.

## Alternatives considered

| Approach                                           | Useful precedent                                                                           | Why not chosen here                                                                                                                                                                                                     |
| -------------------------------------------------- | ------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Render the four Kanban lanes as a vertical list    | Existing board columns and their gestures already express workflow transitions             | Doing includes queued work; Done includes merging PRs. A direct relabel would violate Working's activity meaning and Landed's merge meaning. Keep those semantics in Kanban.                                            |
| One flat list with state filters                   | Search already gives a compact route to a known item                                       | Mutually exclusive filters hide the other three populations; the requested overview needs simultaneous counts and expandable sections. Search remains a way to find a row.                                              |
| Separate or nested revision rows                   | Kanban currently folds some revisions after review while active revisions can remain cards | A revision would change row identity or create multiple entries for one PR. The chosen invariant explicitly requires one original-task row, including active revisions. Revision history belongs in detail.             |
| Derive sections in Swift from existing task badges | Some existing lane routing lives in the app                                                | Badges do not distinguish queued repair from running repair or provide all family/question inputs atomically. Adding another client reconciliation path would violate engine ownership. Reuse inputs, not that routing. |

This was a design choice supported by source inspection, not a usability study comparing measured outcomes. Isolated captures and interaction tests later validated the chosen design; they do not establish its superiority over alternatives.

## Chosen approach

### Existing foundations

Paths below are relative to the repository root.

| Source                                                                                                                             | Role as shipped                                                                                                                                                                                                                                                                                   |
| ---------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tools/boss/protocol/src/types/navigator.rs`, `task.rs`                                                                            | Versioned `Task.navigator` projection (`NAVIGATOR_PROJECTION_VERSION = 1`). Client-facing enums include an `Unknown` catch-all so adding a variant stays additive and does not bump the version. The field is a `Box` because an inline copy overflowed the stack of a deeply nested engine test. |
| `tools/boss/engine/work-navigator/`                                                                                                | Pure family classifier (`boss_work_navigator`). Depends on no engine crates; Bazel visibility is `//tools/boss/engine/work-db:__pkg__` only.                                                                                                                                                      |
| `tools/boss/engine/work-db/src/navigator_projection.rs`, `merge_evidence.rs`                                                       | Adapter and confirmed-merge store. `attach_navigator` is the last derivation of both full assembly and incremental card reads.                                                                                                                                                                    |
| `tools/boss/engine/board-projection/src/lib.rs`, [board projection contract](../board-projection.md)                               | Atomic family refresh, old/new relationship closure, incremental/full parity. Navigator reuses this path; there is no second cache or RPC.                                                                                                                                                        |
| `tools/boss/app-macos/Sources/ChatViewModel+WorkBoardSections.swift`                                                               | Kanban lanes are unchanged. Navigator does not call this classifier.                                                                                                                                                                                                                              |
| `tools/boss/app-macos/Sources/WorkNavigatorBrief.swift`, `MarkdownDocumentChrome.swift`, `WorkFormSheets.swift`                    | Inline brief adapter: preview plus plain-markdown edit with explicit Save/Cancel over existing description mutation.                                                                                                                                                                              |
| `tools/boss/app-macos/Sources/Ghostty/WorkersWorkspaceModel.swift`, `WorkNavigatorActivity.swift`                                  | Embeds the selected execution's existing Ghostty session, or the remote-pane viewer for a spoke-hosted worker. Does not spawn a second worker.                                                                                                                                                    |
| `tools/boss/app-macos/Sources/WorkBoardCardOperatorQuestion.swift`, `AttentionsView.swift`, `WorkNavigatorAttention.swift`         | Shared question and attention-group bodies; existing Kanban and Notifications callers remain.                                                                                                                                                                                                     |
| `tools/boss/app-macos/Sources/ChatViewModel+ReviewGuide.swift`, `NavigatorReviewGuideStore.swift`, `ReviewGuideViewerHeader.swift` | Independently keyed inline guide reader beside the standalone window. Merge-when-ready lives on the shared header, gated by the same eligibility as Kanban.                                                                                                                                       |

### Population, identity and scope

A row represents a non-deleted original task plus every recursively linked revision, regardless of revision origin or status. The engine emits its root ID; the app never walks parent links to decide membership.

A chain that cannot be resolved (deleted, missing or cross-product parent, depth or cycle overflow) becomes a diagnostic fallback row in Backlog with `UnavailableParent`, never silently dropped. When the break sits beneath an archived revision, the nearest visible revision becomes that diagnostic root. Revisions under a valid archived non-revision owner stay with that owner and never become independent rows. Archived roots are excluded from the Work population; restoring one recomputes its section.

Task, project task, chore, design, investigation, design postmortem and accepted follow-up all use the same rules. Follow-ups have independent deliverables and are independent rows, with an origin link in History. An unaccepted follow-up proposal is an attention, not yet a task. Engine-minted PR-review, merge-conflict and CI-fix revisions fold into the original row. Independent review executions are activity on that row, never work items themselves. Automation-owned families get no projection.

Product scope includes project tasks and unprojected chores/investigations/follow-ups, retaining the existing Work population exclusions. Project scope selects roots in that project; their revisions remain included even if child metadata differs. Dependency links outside scope remain visible in detail and navigate by changing scope or revealing the Kanban card. No project rows appear inside the five sections.

The navigator header is a project-within-product picker (plus "No project"), not a second product switcher: product selection stays the existing Work chrome. A project-information button opens the existing project editor. Project-only attentions stay on a header bell that opens Notifications; they are not duplicated onto every task.

A selected item archived or deleted elsewhere remains as a read-only detail with an explanatory banner until the user navigates away. A root whose merge is known is Landed even if an old child still says running.

### Engine contract

Each work-tree task may carry an optional `navigator` projection. A family root carries `section`, engine-sorted `sort_key`, `reason` plus an engine-authored `reason_label`, `default_detail`, ordered current activity references, revision summary, actionable attention references, confirmed merge evidence, and advisory command descriptors. A folded revision carries only `root_task_id`. Additive fields use builder defaults.

A missing projection, a version other than 1, or an unknown section disables Navigator with an inline compatibility message and a route to Kanban. Swift must not guess membership. Unfamiliar reason or wait variants still decode (`Unknown`) and still have a `reason_label`.

The classifier lives in `boss-work-navigator` and takes one flattened family snapshot. `work-db` resolves membership, loads off-board archived relatives with one recursive set-based query pinned to the parent index, maps runtimes/questions/PR/merge evidence, and invokes it. Full and incremental assembly share that path. No new RPC or parallel subscription exists; existing question, review-guide, brief-edit and mutation RPCs remain the action paths.

Confirmed merge evidence is the table `pr_merge_evidence`, keyed by `(root_task_id, canonical_pr_url)`, written by the merge-poller and worker-completion observations, including already-terminal and human-driven tasks. The timestamp is observation time: callers do not retain GitHub's merge timestamp, so the wire's optional `merged_at` is unused. Rebinding to a different PR hides the previous evidence; rebinding back restores it. A stale worker observation of a mismatched PR terminalizes that execution without recording evidence. Legacy rows without evidence stay Backlog (`Completed; merge unverified` or `Completed without PR`). Board reads never call GitHub.

Projection reads use one database snapshot. Inputs reuse existing journal coverage: `tasks` (including parent links), `board_runtime_epoch`, `operator_questions`, `attention_groups`/`attentions` (full-snapshot class), and `pr_merge_evidence`. Runtime-only changes refresh the family even if `tasks.status` did not change.

The crate shipped first and inert ([mono#3623](https://github.com/spinyfin/mono/pull/3623)); the adapter that publishes `Task.navigator` followed ([mono#3629](https://github.com/spinyfin/mono/pull/3629)). That split kept each PR reviewable. The original plan named both in one change.

### Membership: first matching rule wins

| Priority | Authoritative combined-family condition                                                           | Section         | Primary explanation                                                              |
| -------- | ------------------------------------------------------------------------------------------------- | --------------- | -------------------------------------------------------------------------------- |
| 1        | Confirmed merge of the root's current PR                                                          | Landed          | Merged; time and PR                                                              |
| 2        | At least one current worker/reviewer is actually progressing                                      | Working         | Implementing / Revising / Fixing CI / Resolving conflict / Reviewing             |
| 3        | Review-column Ready (`classify_review_section`) and no open question, input wait, or intervention | Ready           | Review / Ready to merge                                                          |
| 4        | Unresolved human action: question, live input wait, or explicit operator intervention             | Needs attention | Answer / Input needed / Intervention                                             |
| 5        | Everything else                                                                                   | Backlog         | Engine reason for waiting, failure, parking or completion without merge evidence |

Display order is Ready, Working, Needs attention, Backlog, Landed. Ready uses the same engine fact as the kanban Review column's Ready section. An open question still wins Needs attention even when that fact is true.

“Progressing” means a current, nonterminal execution whose latest run is live: execution `running`, run `active`, and not awaiting human input, suspended, or proven exited (`failed` / `orphaned`). A `completed` run under a still-live execution is the production spawn shape (the WorkRun tracks the spawn action, not the worker lifetime) and stays Running, including spawn and re-adoption. Driver thinking/tool-use/output changes decorate that state; silence alone does not remove it.

Claimed workspace setup with no live run is Backlog `Starting`, not an active agent. Running review agents count as Working; an `ai_reviewing` hold that is only waiting for consolidation admission does not. Deterministic verdict application without an agent is Backlog `Applying review`. Kinds that are not work on the row (answer agent, automation triage, review-guide generation, administrative progression) are not family activity.

Working wins over a concurrent question from another family member. Keep a visible question count and an attention banner in detail so that human work remains discoverable without double-listing. If all active agents are waiting for input, rule 2 no longer matches and the row moves to Needs attention. Confirmed merge wins over everything; remaining questions are still accessible in Landed detail and the attention surface.

A passive failing check, conflict or failed run alone is not a request to a human. Automatic remediation remains Backlog until running. A persisted operator-intervention signal or exhausted remediation requiring a decision matches Needs attention. Review-required PRs match Ready when `classify_review_section` would put the kanban card in Ready and no question, input wait or intervention is open; merge actions still obey the engine's stricter gates. Unknown CI or mergeability never implies readiness. An optional “Generate guide” button is not itself a reason to put a row in Needs attention.

Default detail follows the winning rule: Ready → Review; Working → Activity; Needs attention → Attention for questions/interventions, Activity for input waits, Review for human review/merge; Backlog → Review when the wait is an automatic PR wait and a PR exists, otherwise Activity; Landed → History. Review still appears as a tab whenever the root has a PR URL, including during Working and after Landed.

### Exhaustive status and signal mapping

The following table supplies inputs to the ordered rules above, not a second competing classifier. Engine matches remain exhaustive on current enum variants; the wire adds `Unknown` so an older client can still decode a newer card.

| Status/signal                                                                           | Mapping absent a higher-priority fact                                                                                                                                                                          |
| --------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Task `todo`                                                                             | Backlog; queued, parked, deferred or eligible reason                                                                                                                                                           |
| Task `active`                                                                           | Working only with progressing activity; otherwise Needs attention for a human action, else Backlog `Starting`/`Waiting`/`Activity unavailable`                                                                 |
| Task `blocked`                                                                          | Backlog with dependencies or blocking reason; explicit unanswered question/intervention elevates to Needs attention                                                                                            |
| Task `in_review`                                                                        | Ready when the Review column would be Ready and no question/input/intervention is open; otherwise Needs attention for those human actions; automatic work pending is Backlog; running review/repair is Working |
| Task `done`                                                                             | Landed only with confirmed merge; otherwise Backlog `Completed without PR` or `Completed; merge unverified`, with no implied retry                                                                             |
| Task `archived`                                                                         | Excluded by population filter; if restored, normal classification applies. Its historical child records remain available to its root                                                                           |
| Execution `queued`, `ready`, `waiting_dependency`                                       | Backlog `Queued` / `Ready` / `Waiting on dependency`; eligibility is not activity                                                                                                                              |
| Execution `claimed`                                                                     | Backlog `Starting`; no fake terminal                                                                                                                                                                           |
| Execution `running`                                                                     | Working when the latest run is progressing as above; ended, suspended or inconsistent run state yields Backlog explanation until reconciled                                                                    |
| Execution `waiting_human`                                                               | Needs attention `Input needed`, with question or existing terminal input route                                                                                                                                 |
| Execution `waiting_review`, `waiting_merge`                                             | Needs attention only if a human action is due; otherwise Backlog `Waiting for review` / `Merging` / `Waiting for checks`                                                                                       |
| Execution `completed`                                                                   | Does not establish merge; classify remaining family/PR facts                                                                                                                                                   |
| Execution `failed`, `abandoned`, `cancelled`, `orphaned`                                | No active-agent claim; Backlog with failure/history, unless a separately recorded human action elevates it                                                                                                     |
| Revision queued / blocked / running / awaiting question / completed                     | Applies above to the original row; terminal child cannot complete the root; revision count and current ask remain in detail                                                                                    |
| AI review `reviewing`                                                                   | Working only while reviewer/consolidator agent runs; admitted hold without running agent is Backlog with stage                                                                                                 |
| AI `review_queued`, `reviewed_with_findings`                                            | Backlog while automatic admission/repair is due; Working if repair runs; Needs attention for explicit manual intervention                                                                                      |
| AI `not_reviewed`                                                                       | Ready when the Review column is Ready and no question is open; otherwise Backlog while admission is due                                                                                                        |
| AI `reviewed_all_clear`, `reviewed_clean_pending`, `review_not_required`                | No activity implied; Ready when the Review column is Ready and no question is open; pending checks => Backlog                                                                                                  |
| CI pending / unknown / failed / success                                                 | Indicators only; failed with queued repair => Backlog, running repair => Working, manual action => Needs attention; success alone is not merge readiness                                                       |
| Review required / approved / changes requested / unknown                                | Required actionable human review => Ready when the Review column is Ready; questions stay in Needs attention; changes requested with automatic revision follows revision state; unknown never authorizes merge |
| Merge conflict / deferred conflict                                                      | Running resolver => Working; queued resolver or deferred-until-sibling-lands => Backlog with blocker link; explicit human decision => Needs attention                                                          |
| Merge requested, merge-when-ready, queue entry, queue CI                                | Backlog `Merging` while automatic progression needs no person; failure with actionable decision => Needs attention; confirmed merge => Landed                                                                  |
| Focus hold, dispatch paused, capacity/quota/backoff, chain hold, future not-before time | Backlog with exact engine wait label and blocker/time; do not manufacture questions for intentional scheduling policy                                                                                          |
| `autostart = false`, project parking, deferred scope                                    | Backlog `Parked` / `Deferred`; an actual scope-approval question elevates to Needs attention                                                                                                                   |
| Human-driven task                                                                       | Backlog `Human work` unless an explicit human action is due; never Working merely because its task status is active                                                                                            |
| Open/partly answered task attention or blocked signal                                   | Needs attention when still actionable; answered/actioned/dismissed/cleared records are history only. Project-only groups stay in scope-header attention surface                                                |
| Unknown string reason or missing PR observation                                         | Backlog with literal diagnostic label, never silently dropped or presented as ready                                                                                                                            |

For conflicting facts, the engine returns one winning reason plus secondary indicators. Tests cover running revision + CI fail + question, deferred conflict + focus hold, merge + stale running child, and completed execution + open PR.

### Navigator rows and ordering

Default navigator width is 360 points, resizable from 260 to 520. Rows are 30 points high; accessibility text sizing uses a second line (reason) and a 52-point hit target. No cards, paragraph previews or per-row button strips.

```text
Work                           [ Kanban | Navigator ]
[ Project: All v ]  [Search tasks and revision asks]
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

The mode control lives in the existing Work toolbar. Product selection stays the existing Work product switcher; the navigator header picks project-within-product (or No project). Search is the existing work search field, labelled for tasks and revision asks, and matches title and description.

Indicators from leading to trailing: animated activity dot when Working; task-kind glyph; title; optional project label in product scope; open revision count; one PR-health summary glyph; actionable-question count. The question count always survives narrowing. Health summary priority is conflict, failed CI, requested review changes, pending checks, approved/green; the glyph is derived in the app from the same card fields Kanban already carries, while membership stays engine-owned. Tooltips and accessibility labels enumerate all underlying facts. Never use color alone.

At widths below 340, drop project label and revision count into tooltip/detail; below that retain kind/activity, truncated title, highest-priority health glyph and question count. Effort, driver name, review-cycle count, all CI checks and timestamps live in detail, not the row. A two-line accessibility layout carries title then reason; no third line.

Fixed section order: Working, Needs attention, Backlog, Landed. Counts represent roots, not revisions or matching attention members. Working/Needs attention/Backlog start expanded; Landed starts collapsed. Preserve expansion per product/project scope. Empty sections retain a zero-count header and “No tasks” when expanded. Filtered counts read “shown / total.”

Engine ordering, with root ID as final tie-breaker:

- Working: priority descending, then current activity start ascending. Heartbeats do not reorder rows.
- Needs attention: input/question first, then explicit intervention, then human review, then ready-to-merge; within each, priority descending, actionable-since ascending.
- Backlog: failed/interrupted first, then queued/starting, automatic PR waits including merging, dependency/focus holds, parked/deferred, and completed-without-merge-evidence last; within group use priority, project ordinal where present, creation time and root ID. No implied dispatch-order guarantee.
- Landed: confirmed merge time descending, using observation time labelled as such when actual merge time is unavailable.

Project scope hides project labels, but keeps the same membership and sorting rules. Search filters projected roots by root title/description and revision ask; a revision match selects its root and opens that ask. No automatic project grouping interrupts the five sections.

### Detail shell and variants

Every detail has a stable root title and project breadcrumb, engine status/reason, PR link and CI/conflict/review summary when applicable. Tabs are **Activity**, **Review** (when a PR exists), **Attention** (when questions/actions exist or already appeared for this selection), and **History**. Initial tab follows `default_detail`; subsequent updates never switch the user's tab. A newly required action appears as a banner and tab badge. Tabs remain listed for a selected root until it is deselected, even if an attention is answered: the empty Attention body stays reachable. A newly selected root whose default was Attention with nothing to render falls back to Activity.

Working Activity uses a vertically resizable brief/terminal split. “Original brief” and “Current ask” are distinct labelled document choices. A running revision defaults to Current ask unless Original holds a draft. Show revision sequence, source (review findings, CI, conflict or manual), and the execution identity beside Current ask. Multiple simultaneous agents use an activity picker, sticky by execution ID; the engine orders actionable waits then progressing activities. Changing that picker does not change the root selection or overwrite the user's document choice. Spoke-hosted workers use the existing remote-pane viewer; local workers reuse the Agents session registry so Work and Agents never create duplicate PTYs.

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

The inline brief adapter reuses existing markdown rendering and task-description mutation; edit mode is plain markdown text with explicit Save/Cancel. Retain per-document dirty drafts across row/tab moves, including a retiring ask kept as Historical while another root is selected. Background updates never overwrite a draft: show “Brief changed remotely” and require reload or an explicit save after reviewing the latest content. Editing the stored brief does not imply delivery to the already-running agent; label that fact and retain the existing agent input route. Never repurpose the original brief field as the revision's ask. Landed briefs reject edit/save.

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

Use engine-provided labels for yes/no, multiple-choice and free-text prompts; do not synthesize Yes/No for split decisions. Reuse existing group semantics: distinguish saving an answer from accepting/actioning a group. Submission binds to attention ID/generation; an already-answered response replaces controls with the recorded answer without resubmitting. A replaced question is refused inline (“This question was replaced. Review the current question.”). Errors are inline, the draft remains, and a waiting-human execution without a structured question offers “Open agent input” and the selected terminal instead. Follow-up proposals use existing edit/accept/dismiss controls in this tab. Intervention entries render read-only with the action's own reason label.

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

Reuse guide rendering, attachments, comments and action handlers. Fetch by root, PR binding, version and request identity so a late response cannot overwrite another task or a newly selected version. Guides remain pinned until explicitly switched. Show not-generated, generating, failed, stale-source and readable states distinctly. A readable stale guide stays readable with its comparison/head label. The diff region is a static placeholder with an external PR-files link; at detail widths below 780 points it stacks above the guide. Merge-when-ready is the shared header control, gated by the same eligibility as Kanban and hidden once merge evidence exists; guide freshness never grants approval. Inline comments load and submit through the engine bridge; read-only follows durable conditions (deleted, archived, missing root), not a connection blip. Leaving Review closes the composer so keystrokes cannot keep flowing into a hidden tab.

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

Todo/queued/parked/deferred/blocked share this layout with an exact reason, retry/not-before time if known, dependency completion and links. A dependency link does not silently change scope. Starting shows “Agent pane not available yet.” Completed-without-PR and merge-unverified rows use the same shell with a completion summary, no Start implied by their section, and history links. Advisory engine commands that the Activity slot renders are Start, Retry, Park and Edit brief.

```text
FAILED / INTERRUPTED — Activity
Improve startup                        Last execution failed
[Activity*] [Review if PR] [History]
Failure: launch could not attach the worker
Last attempt / preserved work / diagnostics link
Brief [Preview | Edit]       Dependencies: none
[Retry when permitted]       [Open last transcript]
```

Failure is an execution outcome, not a new task status. Reuse existing dispatch failure diagnostics and mutation paths. Do not render a dead pane as working; retain transcript/history access. Failure diagnostics resolve the failed current revision first, then the latest failed/interrupted family attempt. A separate human intervention can put this same variant in Needs attention.

```text
MERGING / AUTOMATIC WAIT — Review
Cleanup                                Merging · queue position 2
[Activity] [Review*] [History]
PR / required CI / queue status / last observation
Review guide                        [Diff placeholder]
Waiting for queue checks; no action required
```

Merge request/queue wait is Backlog until merge is confirmed. A running conflict resolver or CI fixer instead uses Working Activity with Current ask and Ghostty; Review remains accessible with a banner identifying the repair. A deferred conflict uses Backlog Activity with the sibling blocker and resumes only when the engine changes state. No timer in Swift predicts that it has cleared. The engine may emit a `cancel_merge` command descriptor while a merge flow is live; there is no separate Navigator mutation for it, matching Kanban (queue cancel is engine-internal).

```text
LANDED — History (Review remains available when a PR exists)
Cleanup                                Merged · Nov 14, 10:32
[Activity] [Review] [Attention if any] [History*]
Merged PR / confirmed timestamp / completion summary
Revision history: initial work -> CI fix -> merged
[Open PR] [Open related follow-up]
```

Landed is read-only for the completed attempt, retains remaining attention actions, and never shows a live terminal by default. New work uses existing follow-up actions rather than editing a historical attempt into apparent activity. History lists revisions, asks, execution outcomes and review versions viewed in the session, with links to transcripts. No PR means no Review tab. No selected task means “Select a task to see its brief, activity and review”; loading/error/disconnected states keep the last coherent detail and label its freshness, without a modal.

### Live updates, selection and switching

Root ID is the selection key, never a list index, section or execution ID. Applying a work-tree delta changes section membership and counts atomically. A selected row can move to a collapsed section without changing detail, terminal attachment, document scroll, tab, dirty text or keyboard focus. Show a quiet “Moved to Needs attention” banner with Reveal, which expands/scrolls. Do not expand or scroll on background events. Maintain the top visible row's anchor where possible as other rows move.

When a current execution ends, keep its terminal/transcript selected. Announce “Execution ended; new revision available” only when another live activity exists; otherwise “Execution ended.” Selecting the new activity is explicit. Surface unavailable/remote sessions through the existing session availability rules. Attach-request episodes belong to each surface, so Navigator cleanup cannot erase the Agents view's request for the same run.

Up/Down moves among visible rows, Home/End to first/last visible row, Left/Right collapses/expands a focused section, Return focuses detail, Escape returns focus to the selected row, and Tab traverses controls normally. These shortcuts apply only when the navigator owns focus; terminal/editor keystrokes pass through unchanged. VoiceOver announces section, root title, activity, question count and health summary.

A labelled `Kanban | Navigator` control lives in the existing Work toolbar. Kanban is the initial default; the chosen mode is persisted in `boss.work.viewMode`. Shared product/project scope and root selection carry both ways; Kanban reveals the root's existing card without clearing search, No-project scope, or Kanban toggles, and without refetching the work tree unless the card is hidden. Opening Navigator from a revision selection resolves its root via the engine projection. Keep each mode's independent scroll/expansion state. Existing Kanban grouping and lane filters remain mode-specific. Shared text search carries across.

Changing scope is a user action: preserve selection if included, otherwise show an empty selection and retain the previous root in per-scope history. Switching mode preserves unsaved drafts and terminal sessions. If a root is excluded by a search or archive event, detail explicitly says why it is absent and offers clear-search/reveal where applicable. Disconnect disables mutations (Review comments stay composable across a blip), labels data stale, and reconnect uses the existing snapshot/delta-base recovery. No speculative row movement on an action acknowledgement; authoritative updates determine membership.

Reveal requests expand and scroll to projected roots in Navigator; tasks without a projection use Kanban. Revealing a revision under an already-selected root sets that revision as the current ask.

### Acceptance evidence

Engine tests enumerate task/execution combinations, family precedence, revision folding, follow-ups, deferred conflict, focus hold, merge queue, rebinding and action routing. Work-db assembly and incremental projection compare full output after question answers, runtime transitions, review admission, conflict deferral, PR rebinding and merge, including removal/reparenting, reconnect recovery, and an archived-revision broken chain. The family query plan is pinned to the parent index.

App tests cover row widths 260/360/520, large text, selection/mode/scope, collapsed destinations, reconnect, draft/session identity, attention generation races, follow-up groups, independently keyed review replies, and comment read-only. Isolated captures exist for Activity, Attention and Review. An isolated real-engine path exercises original implementation → PR → running CI revision → question → answer → review → merge-queue wait → confirmed merge, asserting the same root stays selected, Current ask names the revision, Review stays available during work, and a new run does not redirect the previously selected terminal. Worker and GitHub observations are injected into a private database; that path is not live GitHub integration evidence.

## Decisions

These were the review choices in the original design. Shipping kept them:

- **Terminal work without merge evidence is Backlog**, labelled `Completed; merge unverified` or `Completed without PR`, sorted last. Landed means confirmed merge of the current PR binding.
- **Working precedence.** A running family member keeps the root Working even when another member asks a question. The question count, Attention tab and banner keep that work discoverable.
- **Historical merge coverage is honest.** New observations are written going forward. Unknown history is not fabricated and is not backfilled from GitHub.
- **Brief editing is preview plus plain markdown**, not a new standalone editor.
- **Embedded Ghostty preserves session ownership.** The selected execution stays put when a new revision appears; remote workers use the existing remote-pane viewer.
- **Automation-owned work, archived roots and project-only questions keep their existing surfaces.** The header bell is the route to project attentions.

## Remaining

The diff viewer is still a placeholder: “Not available in this view” plus “Open PR diff on GitHub.” That was never an implementation entry.

The engine may emit advisory `cancel_merge` / `generate_guide` / `open_pr` command descriptors. Generate guide and Open PR have dedicated Review and header controls. There is no user-facing cancel-merge control in Navigator or Kanban; queue cancel stays engine-internal.

## Shipped breakdown

Six reviewable capabilities, plus three follow-on fixes found in post-merge review. There is no diff-viewer implementation entry.

| Change                                         | PR                                                      | What it delivered                                                                                                                   |
| ---------------------------------------------- | ------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| Pure family classifier                         | [mono#3623](https://github.com/spinyfin/mono/pull/3623) | `boss-work-navigator` crate, private visibility, no production consumers at merge.                                                  |
| Confirmed merge evidence                       | [mono#3625](https://github.com/spinyfin/mono/pull/3625) | `pr_merge_evidence` migration `20261115_0110_merge_evidence`, poller and completion writers, snapshot reader with no GitHub access. |
| Work-tree projection                           | [mono#3629](https://github.com/spinyfin/mono/pull/3629) | `Task.navigator` on full and incremental assembly; diagnostic fallback rows; Kanban unchanged.                                      |
| Live activity, project questions, family query | [mono#3634](https://github.com/spinyfin/mono/pull/3634) | Spawn/re-adoption stay Working; project-associated worker questions on the asking family; one recursive family query.               |
| Navigator shell                                | [mono#3635](https://github.com/spinyfin/mono/pull/3635) | Four sections, compact rows, scope/search, mode switch, persistent detail slots, compatibility message.                             |
| Attention bodies                               | [mono#3661](https://github.com/spinyfin/mono/pull/3661) | Shared question/group workflows, generation-safe submit, Open agent input, Working-plus-question precedence.                        |
| Briefs and activity                            | [mono#3662](https://github.com/spinyfin/mono/pull/3662) | Original/current-ask editor, Ghostty embed, backlog/failure/landed variants, History.                                               |
| Attention tab stability and follow-ups         | [mono#3671](https://github.com/spinyfin/mono/pull/3671) | Tab stays mounted after the last question; follow-up proposals in Attention.                                                        |
| Brief/activity lifecycle                       | [mono#3674](https://github.com/spinyfin/mono/pull/3674) | Draft retention, sticky execution, remote panes, failure-target resolution.                                                         |
| Inline PR review                               | [mono#3675](https://github.com/spinyfin/mono/pull/3675) | Independently keyed guide reader, comments, merge-when-ready header, static diff placeholder.                                       |
