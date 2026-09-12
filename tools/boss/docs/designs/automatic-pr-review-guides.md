# Automatic PR review guides: immutable explanations, feedback that revises the PR

- Date: 2026-09-12
- Status: proposed design; no implementation or rollout performed
- Project: Automatic PR review guides
- Source: the authoritative `boss-pr-review-guide-brief.md` in `/Users/brianduff/pr-review-evaluations/2026-09-12-multi-pr/`, read in full; historical evidence remains unchanged
- Repository inspection: `spinyfin/mono` at `2dcf50e3`, including the current PR lifecycle, execution driver, markdown viewer, comment dispatch, and merge implementations
- Related designs: [Markdown comments](comments-in-markdown-viewer.md), [comment revisions](comment-triggered-document-revisions.md), [revision tasks](revision-tasks.md), and [merge queues](trunk-merge-queue-integration-queue-backed-merges-merging-ui.md)

A guide is an immutable explanation of one PR comparison, while feedback submitted from it targets the current PR implementation. Boss should share its existing viewer, comments, revision lifecycle, and merge action, with explicit guide identity separating explanation from approval.

## Decision

Generate asynchronously with **`gpt-6-astra` at `high`**, using the brief's generic prompt verbatim. Store versioned guides in the engine, expose their state on Review cards, and let the existing viewer submit **Revise PR** feedback and invoke **Merge When Ready**. Generation never changes implementation, comment resolution, CI state, review approval, or merge eligibility.

## Goals

- Automatically explain every newly created Boss PR once its identity and pinned comparison are available, even before its card reaches Review.
- Explain the problem, core fix, causal implementation path, and test changes, including a source-checked worked example and short faithful excerpts.
- Provide validated GitHub diff navigation where supported and exact source links for historical or unchanged context.
- Keep generation, readiness, refresh, and retry visible on the Review card, with durable content available after navigation and restart.
- Reuse design-document commenting interactions while directing implementation feedback to the same PR, preserving original quotes and revisions through regeneration.
- Reuse the live card merge action inside the markdown viewer without making a guide a merge prerequisite.
- Make failures, missing context, provenance, usage, and latency diagnosable without inserting job internals into the guide.

## Non-goals

Model selection, effort selection, and routing are settled. There is no size classifier, routing pre-pass, escalation, or user-facing low/high selector. This feature does not replace automated PR review, create approval verdicts, execute tests during generation, publish repository markdown or unsolicited GitHub comments, add another merge implementation, or backfill every historical PR. It does not change models for implementation or comment-answer agents, and does not add a general document version-control product.

## Existing components and gaps

Paths below are relative to `tools/boss/`. These are inspected implementation findings, not assumptions based on older design documents.

| Seam                | Existing implementation                                                                                                                                                                                                                                                                                                                    | Reuse and required extension                                                                                                                                                                                                    |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| PR detection        | [app/worker_events.rs](../../engine/core/src/app/worker_events.rs) feeds successful command observations into `StagedPrUrlCache`; [pr_url_capture.rs](../../engine/core/src/pr_url_capture.rs) distinguishes binding from finalization commands.                                                                                           | Reuse normalized driver events and verified execution/PR association. Add a durable guide wake-up after successful creation; an arbitrary PR URL in prose is insufficient.                                                      |
| Completion          | `WorkerCompletionHandler::finalize_pr_transition` in [completion/pr_transition.rs](../../engine/core/src/completion/pr_transition.rs), with transactional `record_worker_pr_completion` in [work/pr_flow.rs](../../engine/core/src/work/pr_flow.rs). Revision tasks normally derive the PR from the chain root.                            | Add an idempotent reconciliation wake-up, including successful revision completion. Do not couple guide readiness to worker completion or the automated review gate.                                                            |
| PR reconciliation   | [merge_poller/probe.rs](../../engine/core/src/merge_poller/probe.rs) already fetches `baseRefOid` and `headRefOid`; [sweep.rs](../../engine/core/src/merge_poller/sweep.rs) handles open, merged, and closed PRs and publishes updates.                                                                                                    | Reuse probes and scheduling. Persist both comparison endpoints for guides; the existing head/CI projection alone is insufficient. This naturally supplies Review-entry catch-up and external-push refresh.                      |
| Jobs                | [work/answer_agent_runs.rs](../../engine/core/src/work/answer_agent_runs.rs) tracks durable non-card runs bound to executions; coordinator, runner, completion/recovery, and driver transcript paths already exist.                                                                                                                        | Add a distinct `pr_review_guide` execution kind with a run binding, not a new kanban task or a `pr_review` verdict. Reuse admission, launch records, cancellation, and execution diagnostics.                                   |
| Model configuration | [engine/effort](../../engine/effort/src/lib.rs) resolves `SpawnResolutionInput` into `SpawnConfig`; [driver/codex.rs](../../engine/driver/src/codex.rs) maps `EffortLevel::Medium` to `high` and emits `model_reasoning_effort`.                                                                                                           | Pin driver `codex`, model override `gpt-6-astra`, and the existing effort mapping for this execution only; assert the resolved value is exactly `high`. Never infer it from task complexity or product defaults.                |
| Source access       | [boss_github](../../github/src/lib.rs) supplies PR parsing, `pr_files::fetch_pr_view_json`, SHA-aware Contents/Tree reads, and shared `gh` execution; [design-docs](../../engine/design-docs/src/lib.rs) supplies an injectable source and immutable-ref cache precedent.                                                                  | Extend the existing GitHub layer for complete comparison acquisition. Reuse credentials, transport, telemetry, and `boss_http_retry`; do not write another HTTP client. A mutable ref cache is not authoritative guide storage. |
| Viewer              | `AsyncMarkdownViewerViewModel`, `AsyncMarkdownViewerView`, and `MarkdownViewerView` in [DesignsView.swift](../../app-macos/Sources/DesignsView.swift), [ChatViewModel+DesignDocs.swift](../../app-macos/Sources/ChatViewModel+DesignDocs.swift), and [MarkdownDocumentChrome.swift](../../app-macos/Sources/MarkdownDocumentChrome.swift). | Reuse the existing async window and chrome, including Textual rendering, search, code highlighting, tables, and comments. Add typed review-guide context instead of pretending the guide is a workspace file.                   |
| Cards               | [Models+WorkCardSnapshot.swift](../../app-macos/Sources/Models+WorkCardSnapshot.swift), [WorkBoardCardBadgeStrip.swift](../../app-macos/Sources/WorkBoardCardBadgeStrip.swift), and [WorkCardPopover.swift](../../app-macos/Sources/WorkCardPopover.swift).                                                                                | Add a small guide projection and badge-strip action; preserve the snapshot/equality mechanism so status changes actually redraw cards. Keep history access in task detail/popover.                                              |
| Comments            | [protocol/types/comment.rs](../../protocol/src/types/comment.rs), [work/comments.rs](../../engine/core/src/work/comments.rs), [app/comments.rs](../../engine/core/src/app/comments.rs), and [Comments/CommentLayer.swift](../../app-macos/Sources/Comments/CommentLayer.swift).                                                            | Reuse quotes, plain-text projection hashes, sidebar, intent classification, thread replies, batch submission, and lifecycle reconciliation. Add immutable guide-version association and a typed feedback target.                |
| Feedback ownership  | `resolve_doc_owner` in [work/products_design.rs](../../engine/core/src/work/products_design.rs) only resolves design/investigation-owned `pr_doc` artifacts; [work/revise_doc.rs](../../engine/core/src/work/revise_doc.rs) creates a revision or falls back to a chore.                                                                   | Extend target resolution to guide-owned PRs of any task kind. Keep existing document behavior; guide feedback must not take the document-edit or post-merge chore fallback.                                                     |
| Merge               | `mergeWhenReady(for:)` in [ChatViewModel+ReviewActions.swift](../../app-macos/Sources/ChatViewModel+ReviewActions.swift), card confirmation, [app/review.rs](../../engine/core/src/app/review.rs), and [merge_when_ready.rs](../../engine/core/src/merge_when_ready.rs).                                                                   | Extract the current confirmation/presentation into a component shared by card and viewer. Keep the existing RPC, engine eligibility, Direct/Trunk selection, head guard, progress, errors, and reconciliation unchanged.        |

Two findings constrain the design. First, current `revise_doc` creates a task before claiming its comment batch; copying that sequence would allow losing concurrent submissions to leave surplus work. The guide path must reuse the transaction-level revision insertion helpers and claim comments in the same transaction. Second, [answer_agent.rs](../../engine/core/src/answer_agent.rs) describes Claude-specific permission enforcement; the inspected Codex driver groups answer agents with ordinary workspace-write workers when its sandbox flag is enabled. An answer-agent label is not proof that Astra generation cannot write. A guide-specific enforced capability profile is required.

## Alternatives considered

| Approach                                                         | Checkable benefit                                                                                                                            | Reason not selected                                                                                                                                                                                                                                                      |
| ---------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Commit a generated guide beside each PR and treat it as `pr_doc` | Existing repository doc fetch and owner lookup would need less adaptation. Repository-backed design documents already use this successfully. | Here generation must not edit the PR; a guide commit also changes the comparison it describes and turns regeneration into another push. Design documents are intentional editable deliverables, which is why that precedent remains appropriate for them.                |
| Add guide prose to the existing automated `pr_review` result     | Existing review execution, PR identity, and reporting infrastructure are available.                                                          | That result has gate/verdict and remediation semantics. The guide is required to remain available independently of approval, and its completion cannot move review gates. Share execution machinery, not the verdict/result type.                                        |
| Run a detached LLM call only when the viewer opens               | Minimal initial UI/backend wiring; existing design-doc fetches already detach network requests.                                              | This misses the required creation trigger and loses durable job admission/recovery. Detaching an ordinary fetch is suitable when an authoritative repository copy can be fetched again; a costly generated document and its comments need durable identity and attempts. |
| Use the existing answer-agent worker configuration unchanged     | Already supports ephemeral source-reading work without a card.                                                                               | Its output and completion mutate a comment thread, and its permissions are driver-specific. Reuse its execution-binding pattern, but give generation an enforced read-only profile and artifact-only completion.                                                         |

These alternatives compare integration strategies. The rollout study below validates the selected Astra-high product flow; it does not reopen the model decision or claim to compare architectures.

## Chosen approach

### Ownership and invariants

The canonical owner is the PR's Boss revision-chain root. Repository host/identity plus PR number identify the PR; branches, card titles, and revision-task IDs do not. A guide series belongs to that PR, with card associations pointing to the same series rather than creating duplicate documents. Replacing the card's PR attaches a different series and preserves the previous one in history.

The load-bearing invariants are:

1. Every published claim has an inspectable comparison identity: repository, base tip, merge base, head, prompt version, and input provenance. Every supplied source read names an immutable revision.
2. Only the current desired comparison and active attempt may advance the visible ready-guide pointer. A late completion can be retained as historical output, never replace newer content.
3. A readable version's prose and authored context remain immutable, even when its source comparison is unchanged and the prose is regenerated. Retry of a failed attempt and replacement of a readable guide are distinct operations. A presentation-only link destination change preserves that prose/projection and is recorded separately.
4. A comment permanently identifies the version, quote, projection version, and source references it was authored against. Resolving a display anchor does not change that original evidence.
5. A guide's job can only read source and return an explanation. Only submitted user feedback can create implementation work; only the existing merge action can request a merge.
6. Guide status is an independent axis from CI, approval, task status, and merge readiness. “Ready” means content is available, not that the implementation is correct.

### Data and persistence

Use additive `WorkDb` migrations and existing protocol conventions. Proposed names below are new. Structs exceeding five fields use the repository's builder convention; existing task projections gain optional/defaulted fields rather than breaking construction sites.

| Record                        | Durable contents and constraints                                                                                                                                                                                                                                                                                       |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `pr_review_guides`            | One series per canonical repository/PR; owner-root association, canonical PR URL, current lifecycle, latest observation sequence, desired comparison/version, monotonic request epoch, readable version ID, and timestamps. A separate card association permits shared identity without duplicate series.              |
| `pr_review_guide_comparisons` | Base repository/tip SHA, head repository/SHA (including forks), merge-base SHA, prompt ID/hash, packet ID/hash, captured title/body, source collection state, omitted-file manifest, reference map, and timestamps. Unique `(series, base_tip, head, prompt_version)`; pin and verify merge base inside that record.   |
| `pr_review_guide_attempts`    | Comparison, request epoch, ordinal, execution ID, status/phase, selected and actual model/effort/driver, generation/source/publication timestamps, usage categories, retries, classified error, and cancellation/supersession reason. At most one active execution for a desired request; retry tokens are idempotent. |
| `pr_review_guide_versions`    | Immutable accepted Markdown, original model output, render/reference-validator version, comparison/attempt binding, generation timestamp, completeness result, and content hashes. Explicit regeneration may create a new version for the same comparison.                                                             |
| Guide comment context         | Extend existing `work_comments` with an optional guide-version FK, or a one-to-one context row if it keeps the existing mapper smaller. Store selected reference IDs, original projection hash, and quote. Existing thread/status/task fields stay authoritative.                                                      |

Store final Markdown and raw output durably with the version, not solely in a cache or leased workspace. Store potentially large packet blobs under the engine's own state-root artifact area, indexed by hashes, with atomic file publication before committing references. Restart cleanup may collect unreferenced partial writes; a missing referenced blob is an explicit source failure, never an empty file. Keep immutable version content, original output, comments, reference mappings, and enough retrieved source to inspect every guide while the owning PR history exists. Do not apply the evictable design-doc body cache's limits to the only copy.

There is no inspected universal retention policy that guarantees generated guide artifacts indefinitely. V1 therefore deletes a referenced guide/source packet only through explicit owning-history deletion, preserving versions referenced by outstanding comments. Reuse execution-transcript retention independently; losing a transcript must not lose the guide's source manifest or raw output. Record artifact bytes for diagnostics and revisit storage policy with evidence, without adding an automatic expiry requirement to this project.

Protocol additions provide `GetReviewGuide` (series summary plus selected/current version), `RetryReviewGuide` (idempotency token and expected desired epoch), and a guide-state invalidation event. Board/task detail replies include only summary metadata, not full Markdown. Follow the existing request-ID/event pattern, publish only after durable writes, and re-fetch summaries on reconnect. The document viewer's existing singleton must carry an explicit content identity so a late response for a previously opened PR cannot overwrite the current document.

### Triggers and scheduling

Add one engine entry point, `reconcile_review_guide(root, observation)`, called by three existing paths:

- A successful PR-create observation in `app/worker_events.rs`, after the existing command/association checks and a read verifies repository, PR identity, and revisions. Persist the request independently of the in-memory `StagedPrUrlCache`. Queue source collection immediately; do not wait for the human to open a viewer, CI, or automated review.
- PR completion/revision reconciliation in `finalize_pr_transition` / `record_worker_pr_completion`, including fallback `pr_recheck` recovery when the primary event was missed. Resolve revisions to their root PR.
- The existing merge-poller sweep after a successful current probe. This supplies catch-up for an already-open PR in Review with no guide and detects externally changed base/head revisions. Extend the relevant durable observation, not a second GitHub poller.

The PR-state observation has a monotonic engine sequence allocated when its probe starts. A delayed older response cannot roll the desired comparison backward. Timestamps are diagnostics, not ordering authority. The creation hook and poller share this observation path; a fresh probe at publication detects a push or merge missed while a generation was running. A push after that probe remains possible: “current” always means matching the latest observed comparison, and the viewer displays the observation time or an unknown-current-state banner when disconnected.

For an initial PR, queue immediately. For subsequent Boss revisions, record changing desired endpoints and mark an old guide stale immediately, but wait to launch a replacement while a writer in the root's revision chain is active. Once the pushed revision is ready for review and the chain has no active writer, generate the entire base-to-head PR comparison. Do not generate one guide per local commit or each push within an unfinished revision. External pushes with no Boss writer use the same latest-desired queue coalescing. Base-only comparison changes also regenerate; title/body-only edits do not automatically trigger another expensive run. Explicit Retry/Regenerate covers verified explanation errors or material metadata corrections.

Use `ExecutionKind::PrReviewGuide` plus a bound attempt, following the answer-agent precedent of an execution without a `tasks` row. Route it through existing low-priority automation admission/capacity and execution diagnostics, with its own fixed driver/model policy overriding the automation pool's usual model pin. Do not create another pool or send it through PR-review batch admission. Extend launch lookup, synthetic work context, closed-owner checks, resume/recovery, and finalization for the new binding; otherwise code that assumes `work_item_id` is a task will strand these runs. Implementation feedback continues to use the normal revision queue and serializing dependencies.

### Generation and read-only enforcement

Put source-packet, reference, prompt, and guide-output logic in a focused `tools/boss/engine/review-guide` crate. Keep `engine/core` responsible for database transactions, events, and lifecycle orchestration. The new crate depends downward on `boss_github` and small shared types; it does not import `engine/core`. Reuse driver and effort crates rather than creating a parallel provider client. Give its Bazel targets only the visibility needed by core and their tests.

Resolve an explicit `codex`/`gpt-6-astra` request through `SpawnResolutionInput`, with `EffortLevel::Medium` yielding provider `high`; verify and persist `SpawnConfig.effort_value == "high"`. Suppress ordinary implementation effort addenda and PR-deliverable worker instructions. The prompt below is the guide task; source material and the enforced read capability are separate context. A missing driver, unsupported `high`, unavailable model, quota refusal, or fallback from the requested configuration is a retryable generation failure. No spillover or recovery path may silently change model/effort.

Add `WorkerKind::ReviewGuide` to the existing driver/worker-policy integration. Its only source operations are list/search/read against the engine-created immutable packet and a bounded revision-aware source-read broker. Arbitrary shell execution, file edits, VCS mutation, network mutation, connector tools, worker dispatch, comment posting, and merge commands are unavailable. The engine collects final Markdown from the driver's completion output; there is no model-callable publish/reply command and no model-selected output path.

Enforce this at tool dispatch and filesystem/process capability boundaries, including the indirect `exec` tool routes, rather than by prompt text or a filename convention. Give the driver its own writable session/log area, with source snapshots read-only and no implementation workspace, shared object-store write access, or production Boss control credential exposed. The source broker validates series/attempt, repository, SHA, and path; its credential can request reads only. Reuse the current guard-chain attestation and event diagnostics, but a missing mandatory guide capability guard fails launch. Prove this profile against the actual Astra driver before rollout; do not claim the existing answer-agent profile already provides it.

### Source acquisition at pinned revisions

The packet identifies both the observed base tip and the merge base. GitHub PR diffs normally describe the merge-base-to-head change; comparing the current base tree directly to the head can add unrelated base-branch changes. Compute the merge base from immutable commit identities and record the comparison mode. The worked example's “before” files come from the merge base, and “after” files from the head; relevant current-base context is separately labeled. Supply `BASE_SHA` as the observed base tip and name the actual diff-base SHA in the accompanying packet.

Reuse `boss_github::pr_url`, `pr_files`, `contents`, `trees`, and shared `gh` telemetry. Extend these for paginated comparison metadata and immutable tree/blob reads. Verify changed-file coverage against the pinned trees; a missing `files` key is an error here, even though the existing paths-only helper returns an empty list. Account for renames, deletions, added files, modes, binaries, submodules, generated files, and inaccessible blobs. Never treat an omitted API patch as an empty diff. GitHub's API caps must be detected; fall back to complete pinned object/tree acquisition through the existing repository tooling and compute the comparison from those objects, or report collection incomplete. The existing [post-merge reviewer](../../engine/pr-review/src/post_merge_render.rs) already uses explicit-revision `jj file show`; reuse that immutable-object reading pattern in the engine-owned collector, with explicit from/to SHAs for diffs. Do not silently reduce the comparison to the files that fit.

Freeze description/title and changed-file inventory, source hashes, diff hunks, omission reasons, and validated reference targets into a packet. Provide related callers/helpers/types/tests through the same revision-aware broker, recording each retrieved blob and read range. Cache by repository/object hash; no read may resolve a moving branch or the implementation worker's checkout. If a broker access fails, the run records the exact missing context. It can retry the read with the shared bounded backoff policy; if a decisive claim remains unverifiable, it cannot publish a ready guide.

Packet availability does not prove the model inspected or correctly understood every relevant line. Mechanically gate known gaps, unresolved references, missing required sections, and output explicitly reporting essential missing context. Preserve incomplete output in diagnostics, show a useful failure, and retry acquisition/generation; do not display it as a complete ready guide. Human sampling checks semantic grounding, worked examples, and unjustified guarantees that mechanical checks cannot establish. Nonessential omissions can appear as concise limitations in a valid guide.

### GitHub navigation contract

Support ordinary Markdown links on `github.com`, using canonical repository identity and full commit SHAs. Other hosts receive a clear unsupported-navigation failure until an adapter can validate them; do not synthesize GitHub URLs for another host.

| Reference             | Supported destination and interpretation                                                                                                                                                                                                                                                                                 |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Pinned source         | `https://github.com/{owner}/{repo}/blob/{full_sha}/{encoded_path}#L{start}-L{end}` (single line uses `#L{line}`). Validate file existence and line bounds against the stored blob. Use the correct repository and old/new path for forks and renames.                                                                    |
| Current PR diff       | `https://github.com/{owner}/{repo}/pull/{number}/files#diff-{file_anchor}L{line}` or `R{line}`, only when the adapter verifies GitHub's rendered file/side/line target for this comparison. Use the actual supported page href if GitHub redirects its Files changed route. Never ask the model to invent `file_anchor`. |
| Historical comparison | A full-SHA comparison page such as `/compare/{merge_base}..{head}`, with a diff-line fragment only when independently validated against the rendered comparison. Otherwise use the exact pinned source. Do not label a repository comparison as the PR's historical Files changed page.                                  |

GitHub documents [commit-based file permalinks](https://docs.github.com/en/repositories/working-with-files/using-files/getting-permanent-links-to-files), [code snippet permalinks](https://docs.github.com/en/get-started/writing-on-github/working-with-advanced-formatting/creating-a-permanent-link-to-a-code-snippet), and [SHA comparisons](https://docs.github.com/en/pull-requests/how-tos/commit-changes/comparing-commits). These support source navigation; they do not establish that a current PR diff fragment remains immutable after a push. Treat rendered diff-fragment compatibility as a tested adapter contract, not a GitHub API guarantee.

The engine supplies Markdown reference definitions backed by structured `(repository, comparison, path, side, line range)` records. The model returns readable Markdown using those definitions or supplied URLs. Parse the output and resolve all code references through the map. Validate excerpts against the correct source range, preserve clearly labeled pseudocode, reject invented anchors, and retain raw output separately from normalized Markdown. HTTP success alone does not validate a fragment: verify the rendered path/side/line target and its correspondence to the captured hunk. Cache that evidence with its adapter version. If private-page authentication or GitHub rendering prevents exact verification, use the pinned-source fallback and record why.

For each mutable current-PR diff link, publish an adjacent link labeled with its pinned source revision. In the viewer, once the observed comparison differs, resolve the primary reference to a validated historical comparison or the pinned source and label the guide stale. Preserve a version's comment projection across this destination-only transformation. Ordinary Markdown exported from Boss retains both the explicitly labeled “current PR diff” and pinned source links, so historical meaning does not depend on the app. If source and navigation validation cannot address a core implementation/example/test reference even by pinned fallback, publication fails. A guide consisting only of fallbacks is honest but does not demonstrate the required diff-navigation path; rollout must exercise working diff anchors on supported examples.

### Job state and concurrency

Separate desired comparison, readable version, and attempt state. Do not represent refresh by overwriting the current Markdown field with a spinner or an error.

| Event/state                                            | Durable transition                                                                                                                                                            | Reader-visible result                                                 |
| ------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| First valid PR observation                             | Upsert series/comparison and request epoch; enqueue one attempt.                                                                                                              | Queued/generating; no document-open button yet.                       |
| Duplicate observation                                  | Match desired key and request token; no new attempt or document.                                                                                                              | No visible reset.                                                     |
| Source ready / model started / validating              | Advance the active attempt phase with timestamps.                                                                                                                             | Indeterminate progress, never invented percent complete.              |
| New comparison during a run                            | Advance desired key and epoch; cancel/fence the obsolete attempt; coalesce to the newest request.                                                                             | Older readable guide remains accessible and stale.                    |
| Valid output for desired key                           | In one transaction check epoch, attempt/execution binding, open PR lifecycle, latest observed endpoints, and validation success; insert version and advance readable pointer. | Current guide ready; event updates card immediately.                  |
| Late output or completion after close/merge            | Store attempt outcome as superseded/cancelled history; do not advance readable pointer or card lifecycle.                                                                     | Existing history remains readable; no new ready/merge/approval state. |
| First generation failure                               | Record classified error; preserve request and provenance.                                                                                                                     | Error plus Retry; normal PR actions remain available.                 |
| Failed refresh                                         | Record failure without changing readable version.                                                                                                                             | Open older guide, stale/error indicator, and Retry.                   |
| Retry                                                  | Idempotently create the next attempt for the latest desired key and a new epoch; repeated clicks return its identity.                                                         | Progress; older readable version survives.                            |
| Explicit regenerate after a grounded explanation error | New authorized request epoch for the same comparison; preserve old version and comments.                                                                                      | Refresh state; no comment auto-resolution.                            |

Transaction fences apply to source collection as well as generation. An older collector cannot replace a newer packet. At most one active attempt per series is admitted; cancellation can stop spend, but correctness depends on the fence even if a process cannot stop promptly. A force-push back to a previously seen comparison can reuse an already validated version rather than generating again. Metadata-only updates do not defeat the unique comparison/prompt key.

Persist queue intent before notifying the scheduler. On restart, resume ready requests from the database and reconcile running attempts against their specific execution binding and completion records. Re-adopt a confirmed live run; recover recorded output once; mark a vanished run interrupted and offer/requeue a bounded retry after fencing its old execution. Do not restart a job merely because the app reconnects. Before a retry or publication, reconcile current PR state; a missing/failed probe leaves currentness unverified, retains old content, and prevents publishing it as current. Stop queued work on close/merge; reopening re-evaluates the current comparison rather than reviving a cancelled execution.

Transport retries reuse `boss_http_retry` with persisted attempt counts and a bounded policy, rather than an unbounded polling loop. Model-unavailable, essential-context-missing, malformed-output, and validation failures end in explicit retryable states. Manual retries refresh context/configuration as needed but retain the selected model/effort and source provenance. A prompt update changes the key for subsequent creation, refresh, or explicit regeneration; it does not trigger a historical mass backfill.

### Review card and viewer

Place the compact guide affordance beside the existing terminal/merge controls in `WorkBoardCardBadgeStrip`. It appears in Review for cards with a bound PR. Task detail/popover retains “Review guide” and version history when the card leaves Review. Reuse SF Symbols, existing caption sizing, keyboard focus, help text, and accessibility labels.

| Brief state                               | Concrete presentation                                                                                                                                                         |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Queued or generating, no previous content | Small indeterminate `ProgressView`; “Generating review guide…” help/accessibility label. A status action may open job diagnostics, but there is no completed-document button. |
| Current guide ready                       | `doc.text.magnifyingglass` button, “Open review guide”. No approval checkmark or green success wording.                                                                       |
| Refreshing with earlier content           | The same document button plus progress indicator, “Open older review guide; updating…”.                                                                                       |
| Failed, no prior content                  | `exclamationmark.triangle` with short error and keyboard-accessible Retry.                                                                                                    |
| Refresh failed with prior content         | Document button remains, plus error/Retry and “Guide covers an older revision”.                                                                                               |

Open the existing async markdown window with typed context `{series_id, owner_root_id, selected_version_id}`. Get content from the engine, not from a synthetic workspace path or a GitHub design-doc fetch. Show repository/PR number and title, guide generation time, short base/head indicators, and currentness adjacent to the document. Distinguish source staleness from a same-comparison prompt/prose refresh: only mismatched source endpoints justify “older revision”; otherwise say “Updating explanation” or “Explanation refresh failed”. Include a quiet “Review guide — generated explanation” context label; never label this screen “approved”. Keep model, usage, and logs in diagnostics. Refresh PR/guide summaries via normal events while open, including CI, merge intent, failures, and disconnect/reconnect.

Do not replace selected prose while a user is selecting text or drafting a comment. A completed refresh offers “Open updated guide”; the card immediately points to the latest version, while an already-open viewer stays pinned until the user switches. Drafts remain keyed to the selected version. Outstanding feedback from earlier versions stays visible in the sidebar with its quote/revision and an “Open original guide” action. Native headings, syntax-highlighted blocks, tables, search, and external-link handling continue through `MarkdownDocumentChrome`.

Extract the existing card merge control and confirmation into one reusable presentation/action component, used by both card and viewer. It derives availability from the live root task and the same board eligibility projection, not a snapshot captured when the guide opened. Call `ChatViewModel.mergeWhenReady(for:)`, using `mergingWhenReadyIDs`, the existing acceptance/error feedback, and the same engine RPC. Do not copy the engine's rules into the viewer. The existing Direct path uses `--auto --squash --match-head-commit`; Trunk uses the product's configured mechanism and durable merge intent. A stale guide banner stays visible, but does not add a merge gate. Closed/merged state removes or disables the action with the same reason as the card.

### Comments target the implementation

Add `CommentArtifactRef.reviewGuide(seriesID)` and `artifact_kind = "pr_review_guide"` to the existing comment protocol. Keep one series-level sidebar and store the immutable guide version on each new comment. Do not overload `pr_doc:<repo>:<branch>:<path>`: a generated guide has no repository document path, and its owner is not necessarily a design task.

Resolve a typed feedback target: existing repository-document target versus `PullRequestImplementation { root_task_id, series_id, canonical_pr }`. Make ownership and banner eligibility use that result. Version IDs and source references are validated against the series server-side, not accepted as arbitrary viewer-supplied PR URLs. A guide-version quote is immutable; the existing exact/fuzzy anchor resolver operates within the selected original version. V1 does not automatically relocate a comment onto regenerated prose, even when similar words occur. Earlier-version feedback is grouped separately and remains eligible for explicit batch submission through its original context; it is not excluded merely because it cannot be highlighted in the new prose.

The interaction sequence remains the existing one:

1. Selecting text and typing creates a local draft only. Saving the comment uses `CommentsCreate` and the existing asynchronous intent classifier.
2. Question-classified saved comments follow the current read-only answer-agent/thread workflow. Include both the original guide comparison and the current PR identity; investigate current code and distinguish old behavior in the reply. This does not change the answer agent's model policy.
3. Revision-classified comments accumulate in the sidebar. Label the existing batch action **Revise PR** for this target, with a short explanation that it changes the PR implementation/tests. The existing document target retains **Revise**.
4. On submission, atomically claim still-eligible selected comments and insert one normal revision task, using the transaction-level helpers behind `create_revision`. Record the batch token, chain-root PR, submitted guide versions, quotes, comment bodies, thread context, and validated source references. A duplicate request returns the existing task; a losing concurrent claim creates no spare task.
5. Use the existing revision-chain tail dependency/admission rules, including active conflict/CI/reviewer revisions. If an initial writer is still active, hold the revision through the same lifecycle gate until it finishes. Recheck the live open PR at dispatch; do not start competing branch writers.
6. The revision agent inspects the actual current PR, addresses implementation/tests, validates with the repository's normal workflow, and updates that PR. Its directive explicitly says editing generated Markdown cannot satisfy an implementation request. An outdated quoted guide is context, not authority over the current code.
7. Normal task/comment reconciliation records delivery, reopening on failure/abandonment as today. Completion triggers reconciliation of the new full PR comparison; guide generation never resolves comments.

For the brief's retry example, feedback saying “Keep retrying on transient failures, but stop immediately on permission errors” creates one revision against the original root PR. The worker reads current retry code, updates behavior and tests, validates, pushes to the same PR, and supplies a grounded result per submitted comment. Only then does a new guide explain the new retry contract.

Handle questions, inaccurate explanations, and no-code outcomes without manufacturing edits. Reuse `comment_thread_entries` to record a grounded response and the existing explicit no-change completion path when applicable. Add an additive per-comment outcome payload to the guide-feedback batch so source changes, answered/no-change dispositions, and a requested regeneration are distinguishable. For guide comments, a generic completed task or prose refresh alone cannot resolve an implementation request: reconciliation requires that comment's recorded disposition and supporting response. Missing dispositions remain outstanding; reopening remains available. A confirmed prose error may request regeneration through an engine-owned action after the grounded response, without granting the generator any comment mutation capability.

If the PR merges/closes before feedback submission or dispatch, keep the comments and explain that this PR can no longer be revised. Do not inherit `revise_doc`'s automatic new-chore fallback: it would violate the explicit same-PR target. Ordinary task creation remains an existing separate human action, outside this guide flow. General design-document revision and post-merge behavior stay as they are.

### Prompt contract

Use the following exact template as `review-guide-v1`, stored in the implementation crate with a content hash. Substitute only the metadata placeholders; supply packet/broker context separately. The evaluated baseline is exactly the initial four-section request through “Check every step against the actual code.” The remainder is the brief's unevaluated production addition, which the rollout validates. Revision-agent instructions above are a separate template.

```text
I want you to provide me a guided summary of the changes in {{PR_URL}}. The summary should break down as:

1. a general overview of the problem being solved.
2. a general overview of the core fix / implementation.
3. a runthrough of major changes to logic and architecture in the change, with a primary focus on the core change that fixed the problem / implemented the solution.
4. a summary of what tests were added, and what test infrastructure was modified to support it.

This is meant to function as a human guide to code review, so it should reference and include code snippets, but not giant diffs.

Make the core fix concrete with one worked example. Give the input and relevant state, trace the decisive old and new behavior, and show the observable result. Choose an example supported by the implementation or tests; label invented inputs as illustrative. Include a contrasting boundary or failure case only when it helps explain the changed contract. For changes without a runtime behavior, use an equivalent concrete before/after scenario. Check every step against the actual code.

Review context:
- Repository: {{REPOSITORY}}
- PR title: {{PR_TITLE}}
- Base revision: {{BASE_SHA}}
- Head revision: {{HEAD_SHA}}
- The accompanying source context and available read tools provide the PR description, diff, before/after files, related source and tests, and validated GitHub link targets.

Ground the guide in those revisions. Inspect relevant callers, helpers, types, and tests when they determine what the change actually does. Treat the PR description and code comments as statements to verify against the implementation. Distinguish enforced behavior from conventions, prompt instructions, and assumptions. Do not turn a conditional or local check into a broader guarantee.

Organize the walkthrough in a useful reading order through the core implementation. Explain why the important pieces fit together, not just which files changed. Prioritize details that help a reviewer understand or verify the fix. Use short faithful excerpts; clearly label condensed pseudocode. Avoid repetitive summaries and incidental cleanup unless it matters to the solution.

Link the core fix, worked example, and important test changes to the supplied GitHub diff locations. Use revision-pinned source links for relevant unchanged context or lines outside the displayed diff. Reuse supplied URLs or validated reference mappings and check that each link targets the code being discussed. Do not invent diff anchors or imply that a mutable PR URL identifies an immutable revision. Where an exact diff link is unavailable, use the corresponding pinned source link.

In the tests section, distinguish added, modified, and removed tests. Name the important scenarios and assertions; identify relevant fixture, helper, and build/configuration changes. Include counts only when verified and useful. Distinguish author-reported validation from checks you actually performed and from conclusions drawn by reading the tests. Do not claim to have run tests when you have not. State important coverage limits without producing an exhaustive speculative bug hunt.

Use the complete revised PR comparison if this is a regenerated guide. Do not describe only the latest incremental commit. Existing comments may provide context, but the explanation must match the actual current source revisions.

Return only the finished Markdown guide, with a descriptive title and the four requested main sections. Put the worked example within the implementation walkthrough. Keep the guide as concise as the explanation permits while preserving useful reasoning and evidence. Do not include a chat preamble, model details, internal tool logs, a merge recommendation, or an unsupported declaration that the PR is safe to merge. If essential context cannot be obtained, state the specific limitation rather than inventing behavior.
```

Template SHA-256 (UTF-8, excluding the fence and terminal newline): `77d3ff117a07898771b4802b7d1f0c195b4fb4112cf1f58d6fb57fdb6639c543`.

### Diagnostics

Expose attempts through the existing execution/diagnostic surfaces, joined to series and source comparison. Record event-observed/queued/source-start/source-ready/model-start/model-end/validated/published times, queue time, source time, generation time, and total creation-or-update-to-publication time. If original creation time is unknown during catch-up, label latency from the catch-up observation; do not report it as creation latency. Separately measure click-to-render through the existing viewer timing instrumentation.

Retain selected and actual model/effort, prompt and validator versions, input/output/cache token categories as actually supplied, provider usage fields without lossy merging, diff/file/omission statistics, packet hashes, retries, cancellations, and failure classifications. An optional estimated cost includes pricing version/date and cache assumptions; do not embed the historical estimate as a current price. Unknown usage stays unknown. Events after publication and retrieved artifact IDs provide a diagnostic route to raw output and inputs without putting local URLs or runtime database paths in guides or PR descriptions.

## Validation and rollout

This is a validation of the chosen flow, not a model-selection experiment. The brief reports 30 historical runs across six PRs, with Astra high averaging 154 seconds of generation with prepared context. Those runs did not test the expanded prompt or anchored diff links, and did not measure source collection, queueing, storage, publication, or Boss end-to-end latency. No historic rating or report is revised by this project.

### Automated acceptance matrix

Extend existing tests rather than asserting exact generated prose. Run tests only through `bazel test`; run ordinary pre-push validation with `checkleft run`.

| Brief criteria                 | Required evidence                                                                                                                                                                                                                                                                                                                                                                             |
| ------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1, 2: creation and card states | Real engine event-handler tests for successful creation, fallback completion, Review catch-up, duplicates, and restored queued/ready/failed state. Swift presentation tests for every card row, keyboard action, and event-driven redraw.                                                                                                                                                     |
| 3, 4: content and navigation   | Prompt identity/substitution checks; pinned tree/blob and hunk fixtures covering base-tip versus merge-base, rename/delete, Unicode/path escaping, fork, missing/truncated input, old-side lines, and out-of-diff context. Validate actual rendered GitHub destinations in the rollout; fabricated URL strings alone are insufficient.                                                        |
| 5: merge reuse                 | Extend `MergeWhenReadyFeedbackTests`, card/control tests, and engine `app/review.rs` tests for shared confirmation, duplicate clicks, Direct/Trunk paths, eligibility changes while open, head movement, errors, disconnect, and post-merge updates. Guide failure/staleness never changes merge eligibility.                                                                                 |
| 6, 7: PR feedback              | Extend `comments_crud_test`, `work/revise_doc.rs` tests, `CommentsTests`, and actual request/dispatch integration: save versus submit, questions, same-PR implementation/test revision, per-comment no-change responses, duplicate submissions, active-writer serialization, and no chore on raced merge. Old comments retain original quotes/versions and remain discoverable after refresh. |
| 8: concurrency and recovery    | Delayed old probe; delayed collector; base/head changes during generation; duplicate retry; crash before/after artifact and DB commit; late old execution; close/merge while running; refresh failure; close/reopen; force-push back to a known comparison. Assert pointer/epoch/content and absence of unintended task/comment transitions.                                                  |
| 9: explanatory boundary        | Actual driver capability tests denying direct and indirect writes, shell escapes, GitHub mutations, task/comment dispatch, merge, and writes outside the source packet. Test unavailable Astra/high without fallback, missing essential context, false test-execution wording fixtures, and empty/malformed output. Human review checks meaning.                                              |
| 10: telemetry                  | Persist/reload separate phase and end-to-end times, source/prompt/model identities, usage including unknown/cache fields, retry outcomes, and raw versus transformed output.                                                                                                                                                                                                                  |

Use the real isolated engine, app, request handlers, execution runner, and completion path for integration coverage. Deterministic fake provider/GitHub boundaries test faults but are not evidence that the real provider or browser works. Extend the existing engine tests under `//tools/boss/engine/...` and macOS test shards, plus new narrowly visible source/guide crate targets. Never run test binaries directly or start the production Boss engine during validation.

### Reproducible six-PR sample

These identities were read from the archive's original `cases/*/source/manifest.json`. They make the follow-up reproducible from this repository and GitHub without access to the coordinator's memory, runtime database, or evaluation directory. All SHAs below are full immutable revisions; the base tip is deliberately distinct from the merge base where applicable.

| Saved PR                                                               | Base tip                                   | Merge base / before                        | Head / after                               |
| ---------------------------------------------------------------------- | ------------------------------------------ | ------------------------------------------ | ------------------------------------------ |
| [brianduff/flunge#1545](https://github.com/brianduff/flunge/pull/1545) | `b4e72c775d4e60b5fe0565599f666fdcd2a598cd` | `b4e72c775d4e60b5fe0565599f666fdcd2a598cd` | `ba2e4fce1b1de11fe605390ed84dfe9f0f415e99` |
| [brianduff/flunge#1552](https://github.com/brianduff/flunge/pull/1552) | `54e4e293dfe4e142bf8a63761f4f0348af205a0e` | `54e4e293dfe4e142bf8a63761f4f0348af205a0e` | `7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1` |
| [brianduff/flunge#1605](https://github.com/brianduff/flunge/pull/1605) | `9806138106ee6c5da6840b58b468bbc4f3e0b8af` | `9806138106ee6c5da6840b58b468bbc4f3e0b8af` | `256630b62986cc513c42e7e9075e57011707ce9a` |
| [spinyfin/mono#2860](https://github.com/spinyfin/mono/pull/2860)       | `97db22f5bb3fcb02dc105cf6973cc57de3810104` | `91567c86a7195ee78f3e340fbcf6c5bccc5ebedc` | `725bd759480d416ffffcd39c32ab5fa99d3acc97` |
| [spinyfin/mono#2886](https://github.com/spinyfin/mono/pull/2886)       | `6f8ca35153d0fcae9aea5e9852894ea549989ca0` | `6f8ca35153d0fcae9aea5e9852894ea549989ca0` | `68e51b6cfd5cb8ccac6473273d04d57fe972d7a4` |
| [spinyfin/mono#2890](https://github.com/spinyfin/mono/pull/2890)       | `6f8ca35153d0fcae9aea5e9852894ea549989ca0` | `6f8ca35153d0fcae9aea5e9852894ea549989ca0` | `12c538d6f328d9f4cbb62c0ef69d6fa9b301500a` |

Land a versioned corpus manifest, invocation recipe, and fresh results under `tools/boss/engine/review-guide/` and `tools/boss/docs/evaluations/automatic-pr-review-guides/`. Reconstruct sources from these SHAs through the production collector; record fetched metadata time and source hashes. Do not substitute a merged PR's current base/head. The saved six PRs remain the cases, not six convenient new ones. If an object cannot be recovered, record the missing input and keep rollout blocked until it is available; do not silently replace a case.

Run the production prompt at Astra high on all six. For explanatory-depth comparison, reproduce the exact evaluated baseline prefix at the same fixed model/effort and packet, and label these as new baseline runs, not the original historical reports. This avoids making a future worker depend on machine-local reports and supports a paired human assessment; it does not recompute historical scores. Save fresh outputs and measurements in the repository, with sensitive/internal identifiers omitted from new report text according to repository policy. No brittle prose snapshots or automatic model graders deciding approval.

A human reviews all six production guides, checking the four sections, source-supported before/after example, causal reading order, test inventory/claims, and explanation depth relative to baseline. Open every core fix/example/important-test reference in GitHub and verify intended file, revision, side, and line, including the historical fallback. Record material errors, unsupported claims, missing decisive context, broken navigation, and useful detail lost by the additions. The production additions pass only when those findings are addressed and no material grounding/navigation defect remains in this sample; this is a sample result, not a guarantee for other PRs.

### Real Boss path and release gate

Use the existing feature-flag mechanism for staged enablement of automatic guides, initially enabled only for validation. The integration PR must already exercise generation through genuine engine entry points; the rollout gate governs subsequent broad enablement, not whether preceding implementation PRs may be considered tested.

Sample at least three new real Boss PRs, including an ordinary implementation, a non-runtime/build or documentation change, and a PR that receives submitted guide feedback. Record public PR URLs, source SHAs, request/guide identities in sanitized diagnostics, output, phase timings, and human findings in a repository report. Use actual PR creation, Astra generation, viewer opening, comment submission, a validated implementation/test update to the same PR, and regenerated content. Observe an old guide and its comment after the push. Exercise real viewer merge on a PR the reviewer has explicitly chosen to merge; do not let the generation job or a fixture automatically decide approval.

Fault tests must also exercise restart, reconnect, stale output, failed refresh, and the shared merge path; real samples must measure source/queue/generation/publication/render time separately. A render capture checks appearance only, not comment dispatch or merge correctness. Run isolated capture instances with both isolation environment settings; attach captures via Boss for local evidence, and use repository reports or CI artifacts accessible to GitHub readers for the PR. No production-state database access, localhost evidence URLs, or coordinator-only outputs are prerequisites for a worker task.

Enable automatic generation by default only after the six-case and real-path findings are addressed and documented. If a check fails, fix the source collector, navigation adapter, lifecycle, or prompt context at its cause and rerun affected cases; never switch model/effort or weaken the required guide content to turn the gate green. A prompt change must be visibly versioned and revalidated. Rollback stops new guide jobs through the same flag while retaining guides/comments and existing PR/merge actions.

## Risks / open questions

- **Read-only Astra integration:** this is required implementation work, not an already-proven property of `AnswerAgent`. Review the capability boundary and actual driver tests before enabling jobs. A prompt-only restriction cannot pass acceptance.
- **GitHub fragment compatibility:** the inspected GitHub helpers do not validate rendered diff anchors. The new adapter must demonstrate this on real GitHub pages, including authenticated access where needed. Pinned-source fallback preserves correctness, but cannot be used to claim successful diff navigation in the rollout sample.
- **Semantic incompleteness:** source and link validation can establish what was available and where a link goes; neither proves that an explanation is correct. Preserve known gaps and use the human sample to evaluate the worked example and claimed guarantees.
- **Observation lag:** no engine can know an unseen external push instantly. Show comparison IDs and last observed state, fence all known newer requests, and let the existing merge action recheck the live PR. Do not advertise a timeless “current” or “safe” badge.
- **Feedback races:** transactional batch insertion and immutable comment context are necessary amendments to reuse. Existing document behavior and tests must remain intact; guide-only semantics must not silently change design-document feedback.
- **Resource use:** durable source artifacts and a fixed high-effort job add storage and queue pressure. Measure bytes and separate latency components; the historical 154-second figure is not a service-level objective. Queueing may slow guide availability, but may never block merging or source revisions.

No product choice remains open in this proposal. Design approval ratifies the explicit immutable-version and PR-target semantics above; implementation validation must resolve the technical risks before rollout. The corpus and prompt are carried here so scheduling does not depend on machine-local evidence.

## Proposed implementation task breakdown

Breakdown size: 8 entries (8 in-scope, 0 deferred) — the feature has distinct source-capture, execution, viewer/merge, comment-version, feedback-dispatch, integration-test, corpus-validation, and live-rollout seams, each with an exercised caller or a separately reviewable validation artifact.

The source and execution entries retain their own real engine callers and diagnostics; they are not empty schema or utility PRs. After generation lands, the viewer work and six-case validation can run in parallel because they touch app files versus evaluation artifacts. Comment-version work follows the viewer because both substantially modify `DesignsView.swift`, `MarkdownDocumentChrome.swift`, and viewer identity; it must forward-port the viewer changes preservingly. Feedback dispatch follows comment-version work because they share comment protocol/ownership surfaces. The corpus validation may continue alongside either. No historical backfill or speculative deferred tasks are proposed.

### Capture pinned PR sources and validated references

Implement durable PR-series/comparison capture at the existing creation/completion/poller seams, with the source packet collector, source/reference crate, shared GitHub helper extensions, rendered-target validation and pinned fallbacks, and a diagnostic read path for the captured packet. Include the source/version manifest contracts, canonical root association, observation ordering, omission/error handling, and tests through the actual reconciliation callers. This PR delivers inspectable, immutable comparison artifacts even before model execution is connected; keep automatic capture behind the rollout flag.

Effort: large

Dependencies: none.

Scope: in-scope

### Run durable Astra-high guide jobs

Connect captured comparisons to `pr_review_guide` executions using the existing coordinator/driver/effort machinery. Add the enforced read-only Astra profile, exact versioned prompt, revision-aware broker, output validation, durable attempts/versions, transactional publication fencing, retry/cancellation/restart behavior, diagnostics/usage, and summary/content/retry RPCs. Include the fixed-model refusal and capability tests with the actual launch configuration. The PR must generate a guide through the real engine path; no separate uncalled runner or persistence PR is needed.

Effort: large

Dependencies: Capture pinned PR sources and validated references — jobs consume its immutable packet and navigation contract.

Scope: in-scope

### Add Review-card guides and shared viewer merge

Wire the engine summary/events into card snapshots and all required affordance states. Open guides in the existing async markdown viewer with PR/version/currentness metadata, old-content retention, history/detail access, and response identity guards. Extract and reuse the card's merge presentation/confirmation/action in the viewer, driven by live task/CI/merge state. Extend existing markdown, card, and merge tests and verify isolated renders.

Effort: large

Dependencies: Run durable Astra-high guide jobs — the UI consumes its persisted state/content protocol.

Scope: in-scope

### Preserve guide-version comments in the existing sidebar

Add the guide comment artifact and immutable version/source-context association to existing comment storage/protocol and its actual viewer authoring/listing callers. Reuse selection, drafts, threads, projection hashing, and within-version anchoring; show outstanding older-version feedback with original-guide navigation and protect drafts during refresh. Add migration and UI/CRUD tests. Keep the PR-target action unavailable until target dispatch lands; saved comments are already durable and readable in this PR.

Effort: medium

Dependencies: Add Review-card guides and shared viewer merge — comments require selected version identity, and both changes substantially touch the shared viewer files; integrate the earlier changes without replacing them.

Scope: in-scope

### Route guide feedback to same-PR revisions and answers

Extend existing feedback ownership, classifier/answer context, and batched revision dispatch for the PR implementation target, with the thin **Revise PR** UI caller in the same PR. Atomically claim comments and create revisions using existing chain serialization, carry quotes/source/version context, enforce same-PR validation/update directives, and record grounded per-comment outcomes including no-code answers. Preserve existing document/chore semantics for document targets, refuse closed-PR guide revision without a chore fallback, and regenerate only through guide reconciliation. Extend question, revision, duplicate, failure, and no-change tests.

Effort: large

Dependencies: Preserve guide-version comments in the existing sidebar — dispatch needs immutable authored context and the new artifact; this also orders substantial edits to shared comment protocol/ownership files.

Scope: in-scope

### Exercise guide lifecycle and feedback through real integration seams

Land isolated engine/app integration coverage through production PR observation, job admission, result publication, viewer requests, submitted comments, revision lifecycle, and shared merge handlers. Cover the acceptance matrix's restart/race/error scenarios and prove an implementation/test revision to the same PR is followed by refreshed content with old feedback preserved. Use deterministic provider/GitHub boundaries for faults, explicitly distinguish these from the real model/GitHub sample, and retain fixtures/test instructions in `spinyfin/mono`.

Effort: medium

Dependencies: Route guide feedback to same-PR revisions and answers — the full feedback and refresh path must exist before this cross-subsystem integration suite can exercise it.

Scope: in-scope

### Validate the production prompt on the six saved comparisons

Land the corpus manifest from this design, a reproducible invocation recipe using the production collector/generator, fresh paired baseline/production Astra-high outputs, separate timing/usage records, and a human-reviewed navigation/grounding report. Reconstruct the exact saved comparisons from GitHub, check every core/example/test link and useful explanatory depth, fix detected defects before recording acceptance, and leave historical archive reports untouched. This validates the chosen prompt additions; all inputs and deliverables are repository-resident or pinned GitHub source.

Effort: medium

Dependencies: Run durable Astra-high guide jobs — evaluation must invoke the real source/generation/navigation path. May run in parallel with Add Review-card guides and shared viewer merge and its comment follow-ups; evaluation artifacts have no substantial app-file overlap.

Scope: in-scope

### Measure the real Boss flow and enable automatic guides

Run the three-PR live sample described above through an isolated Boss instance and real driver/GitHub path, including submitted implementation feedback, a same-PR update, preserved old comments, and reviewer-authorized viewer merge. Land the sanitized human-reviewed report, public source references, separate end-to-end/generation measurements, and the feature-flag enablement change only after findings are addressed. Retain rollback behavior and durable history; do not treat a screenshot or simulated harness as end-to-end evidence.

Effort: medium

Dependencies: Exercise guide lifecycle and feedback through real integration seams; Validate the production prompt on the six saved comparisons — deterministic integration coverage and prompt/navigation evidence both gate broad enablement, while the genuine live path supplies the final integration evidence.

Scope: in-scope
