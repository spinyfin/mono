import Foundation

enum EngineEvent {
    case connected
    case disconnected
    /// The engine had to drop pending invalidations for this session's
    /// outbound queue while riding out a publish burst (e.g. a
    /// merge-poller sweep across many products/executions) instead of
    /// disconnecting. The socket is still up — treat this like the
    /// refetch-on-reconnect path, silently, with no connection-lost UI.
    case resyncRequired
    case workInvalidated(topic: String, productId: String?, itemIds: [String])
    case appSessionRegistered
    case bossSessionRegistered
    case engineRequest(requestId: String, request: EngineRequestKind)
    case productsList(products: [WorkProduct])
    case projectsList(productId: String, projects: [WorkProject])
    case workTree(product: WorkProduct, projects: [WorkProject], tasks: [WorkTask], chores: [WorkTask], taskRuntimes: [WorkTaskRuntime], dependencies: [WorkItemDependency])
    case workItemCreated(item: WorkItemPayload)
    /// Batch counterpart of `workItemCreated`, pushed on a product's work
    /// topic by background jobs that create many rows at once with no
    /// originating session to reply to directly (e.g. the auto-populate
    /// Populator staging a design's task breakdown — design
    /// auto-populate-project-tasks-on-design-pr-merge.md §"Surfacing").
    /// Unlike `workItemCreated`, applying this must not steal the
    /// operator's current selection/filters — it's a passive data refresh.
    case workItemsCreated(items: [WorkItemPayload])
    case workItemUpdated(item: WorkItemPayload)
    case projectTasksReordered(projectId: String, taskIds: [String])
    case workItemDeleted(id: String)
    case workError(message: String)
    case error(message: String)
    /// Snapshot of every allocated worker slot's live runtime state.
    /// Delivered both as a one-shot reply to
    /// `list_worker_live_states` and as a topic push on
    /// `worker.live_states` whenever any slot changes.
    case workerLiveStatesList(states: [WorkerLiveState])
    /// Snapshot of slot ids whose live-status summarizer has been
    /// manually disabled by the human. Sourced from a one-shot reply
    /// to `list_live_status_disabled_slots`.
    case liveStatusDisabledSlotsList(slotIds: [Int])
    /// Echoed result of a `set_live_status_enabled` toggle. The UI
    /// uses this to confirm the engine accepted the change before
    /// flipping local state.
    case liveStatusEnabledSet(slotId: Int, enabled: Bool)
    /// Engine reply to a `ResolveProjectDesignDoc` RPC. Carries the
    /// per-project `ProjectDesignDocState` the kanban consumes to
    /// pick the right icon affordance and open dispatch.
    case projectDesignDocResolved(output: ResolveProjectDesignDocOutput)
    /// Response to `list_conflict_resolutions` — the filtered set of
    /// rows for the Engine tab. Phase 5 #13/#14 of the merge-conflict
    /// design.
    case conflictResolutionsList(attempts: [WorkConflictResolution])
    /// Response to `list_ci_remediations` — the filtered set of
    /// `ci_remediations` rows for the Engine tab. Phase 11 #37 of
    /// the merge-conflict design (CI extensions).
    case ciRemediationsList(attempts: [WorkCiRemediation])
    /// Activity-feed push: a fresh conflict-resolution attempt was
    /// created (or a `retry` reset an existing one) and a worker is
    /// about to take over. The Engine tab refreshes; the badge state
    /// is unaffected (only `succeeded` counts as a "cleared" event).
    case conflictResolutionStarted(productID: String, workItemID: String, attemptID: String, prURL: String)
    /// Activity-feed push: an attempt finished successfully. Drives the
    /// "🔧 conflict cleared" PR-card badge (Phase 5 #15) and refreshes
    /// the Engine tab.
    case conflictResolutionSucceeded(productID: String, workItemID: String, attemptID: String, prURL: String)
    /// Activity-feed push: an attempt failed terminally.
    case conflictResolutionFailed(productID: String, workItemID: String, attemptID: String, prURL: String, failureReason: String)
    /// Activity-feed push: an attempt was abandoned on purpose.
    case conflictResolutionAbandoned(productID: String, workItemID: String, attemptID: String, prURL: String, failureReason: String)
    /// Activity-feed push: a fresh CI-remediation attempt was created
    /// for an in-review PR. `attemptKind` is `"fix"` or `"retrigger"`
    /// per the engine's pre-spawn triage. Mirrors
    /// `conflictResolutionStarted` (merge-conflict-handling-in-review
    /// Phase 10 #34).
    case ciRemediationStarted(productID: String, workItemID: String, attemptID: String, prURL: String, attemptKind: String)
    /// Activity-feed push: the engine observed the parent PR back at
    /// CI clean and retired the remediation attempt. The parent has
    /// been flipped from `blocked: ci_failure` back to `in_review`.
    case ciRemediationSucceeded(productID: String, workItemID: String, attemptID: String, prURL: String)
    /// Activity-feed push: the engine cleared `blocked: ci_failure` on a
    /// task but found no active remediation attempt to retire — the prior
    /// attempt was already terminal (failed/abandoned). Distinct from
    /// `ciRemediationSucceeded`: the `ci failing` badge should be cleared
    /// but the `ci auto-fixed` badge must NOT be set.
    case ciFailureCleared(productID: String, workItemID: String, prURL: String)
    /// Activity-feed push: a CI-remediation attempt terminated in
    /// `failed`. Fired when the worker calls
    /// `boss engine ci mark-failed` or when the completion-path
    /// catch-all (`no_push_no_classification`) fires.
    case ciRemediationFailed(productID: String, workItemID: String, attemptID: String, prURL: String, failureReason: String)
    /// Activity-feed push: a CI-remediation attempt was abandoned on
    /// purpose (parent PR closed externally, manual move, etc.).
    case ciRemediationAbandoned(productID: String, workItemID: String, attemptID: String, prURL: String, failureReason: String)
    /// Activity-feed push: the engine has given up auto-fixing this
    /// PR's CI. The parent is now `blocked: ci_failure_exhausted` and
    /// the user is the next actor (typically via
    /// `boss engine ci retry <work-item-id>`).
    case ciRemediationExhausted(productID: String, workItemID: String, prURL: String, attemptsUsed: Int, budget: Int)
    /// Response to `list_feature_flags` — a snapshot of every
    /// registered engine feature flag and its current value. Drives
    /// the Feature Flags debug pane.
    case featureFlagsList(flags: [FeatureFlag])
    /// Echoed result of a `set_feature_flag` toggle: the engine has
    /// persisted the new value and consumer-side `is_enabled` checks
    /// will see it immediately. The debug pane uses this as the
    /// "reload confirmed" signal to render the toggle as committed.
    case featureFlagSet(name: String, enabled: Bool)
    /// Response to `get_engine_health` — a snapshot of the engine's
    /// user-visible configuration health (currently
    /// `ANTHROPIC_API_KEY` presence). Empty `issues` means healthy;
    /// any element drives the top-of-window banner and the Settings
    /// pane warning. Introduced after #699 where a missing API key
    /// silently broke summarization with no UI affordance.
    case engineHealthResult(apiKeyPresent: Bool, issues: [EngineHealthIssue])
    /// Engine's live pool-size configuration, pushed immediately after
    /// `app_session_registered` so `WorkersWorkspaceModel` can configure
    /// its slot ranges before any `SpawnWorkerPane` request arrives.
    /// This is the single source of truth: the engine's runtime config
    /// drives the app's capacity check so they can never drift out of sync.
    /// `coordinatorModel` is the `--model` slug the Boss pane must use —
    /// derived from `effort=max` so it follows the effort table automatically.
    case enginePoolConfig(workerSlots: Int, automationSlots: Int, reviewSlots: Int, coordinatorModel: String)
    /// Response to `get_settings` — snapshot of every per-installation
    /// setting and its current value. Drives the Settings window.
    case settingsList(settings: [EngineSetting])
    /// Echoed result of a `set_setting` toggle: the engine has
    /// persisted the new value. The Settings window uses this as the
    /// "saved" signal.
    case settingSet(key: String, enabled: Bool)
    /// Response to `list_hosts` — all registered hosts with capabilities.
    case hostsList(hosts: [EngineHost])
    /// Response to `add_host` or `get_host` — one host with capabilities.
    case hostResult(host: EngineHost)
    /// Response to `set_host_enabled`, `add_host_tag`, or `remove_host_tag`.
    case hostUpdated(host: EngineHost)
    /// Response to `remove_host` — the named host was deleted.
    case hostRemoved(id: String)
    /// Response to `metrics_list_live` — bulk snapshot of every
    /// registered engine counter and gauge, sorted by name. Drives the
    /// Metrics debug pane's initial load and its polling timer.
    case metricsListLiveResult(entries: [EngineMetric])
    /// Response to `list_attention_items_for_work_item` — open and
    /// resolved attention items for a given product/work-item id.
    case attentionItemsForWorkItemList(workItemID: String, items: [WorkAttentionItem])
    /// Live push: a worker filed a new attention item (e.g. a
    /// `[deferred-scope]` marker). Consumers filter on `item.kind`.
    case attentionItemCreated(item: WorkAttentionItem)
    /// Response to `accept_deferred_scope_attention`, also pushed live on
    /// the owning product's work-tree topic.
    case attentionItemUpdated(item: WorkAttentionItem)
    /// Response to `create_task_from_deferred_scope_attention`, also
    /// pushed live on the owning product's work-tree topic. `task` is the
    /// followup filed from the deferred-scope marker.
    case attentionItemConverted(item: WorkAttentionItem, task: WorkTask)
    /// Response to `list_deferred_scope_attentions` — every open
    /// `deferred_scope` item across a product, paired with the id of the
    /// work item whose execution recorded it. Backs the kanban
    /// review-lane card affordance.
    case deferredScopeAttentionsList(productID: String, items: [DeferredScopeAttention])
    /// Response to `list_planner_runs` — every `planner_runs` audit row
    /// for the project, newest first. Drives the Planner review/release/
    /// undo surface (design auto-populate-project-tasks-on-design-pr-merge.md
    /// task 10).
    case plannerRunsList(projectID: String, runs: [PlannerRun])
    /// Response to `release_project` — the engine flipped `autostart =
    /// true` on every task in `runID`'s staged batch; dispatch begins on
    /// the next reconcile pass.
    case releaseProjectResult(projectID: String, runID: String, released: Int)
    /// Response to `unpopulate_project` — `deleted` carries the ids of
    /// tasks soft-deleted; `preserved` carries tasks that already had an
    /// execution (released and dispatched) and were left alone.
    case unpopulateProjectResult(
        projectID: String,
        runID: String,
        deleted: [String],
        preserved: [UnpopulatePreservedTask]
    )
    /// Response to `open_review_terminal` — the engine has leased a
    /// workspace, fetched the PR branch, and created a new jj commit
    /// atop `<branch>@origin`. The app should open a Ghostty terminal
    /// window rooted at `workspacePath`.
    case reviewTerminalReady(workItemID: String, workspacePath: String, leaseID: String)
    /// Response to `open_live_workspace_terminal` — the work item's live
    /// execution already holds a leased cube workspace at `workspacePath`.
    /// The app should open a Ghostty terminal window rooted there; there
    /// is no lease to release when the window closes.
    case liveWorkspaceTerminalReady(workItemID: String, workspacePath: String)
    /// Response to `merge_when_ready` — the engine has successfully
    /// initiated the merge process for the PR. `action` is one of:
    /// `"enqueued"` (merge queue), `"auto_merge_enabled"` (will merge
    /// when checks pass), `"merged"` (directly merged). The PR-reconciler
    /// is kicked on the engine side so the kanban state refreshes promptly.
    case mergeWhenReadyAccepted(workItemID: String, prURL: String, action: String)
    /// GitHub OAuth auth-state push (OAuth device-flow design §4).
    /// Delivered both as the immediate reply to a `git_hub_auth_*`
    /// request and proactively on the `github.auth` topic as the
    /// engine's device-flow poll loop advances. The DTO is display-safe;
    /// the token and private device code never appear in it.
    case gitHubAuthState(state: GitHubAuthState)
    /// Response to `list_executions` — all historical execution rows for
    /// one task, newest-first. Drives the transcript viewer's left pane.
    case executionsList(taskId: String, executions: [ExecutionVM])
    /// Reply to `execution_transcript` — the rendered, lazily-displayable
    /// segments for one execution plus live/complete flags. Drives the
    /// transcript viewer's right pane (transcript-viewer.md task 4).
    case executionTranscriptResult(
        executionId: String,
        segments: [TranscriptSegmentVM],
        isLive: Bool,
        complete: Bool
    )
    /// Reply to `execution_transcript` when the transcript file is absent
    /// (rotated, GC'd, or never recorded). `reason` is human-readable and
    /// surfaced as a "transcript unavailable" state, never an error.
    case executionTranscriptUnavailable(executionId: String, reason: String)
    // MARK: Automation events (maintenance-tasks.md T7)
    /// Response to `list_automations` — all automations for a product.
    /// `openTaskCounts` maps automation id → open-task count, batched inline.
    case automationsList(productID: String, automations: [AppAutomation], openTaskCounts: [String: Int])
    /// Response to `create_automation` — the newly created automation.
    case automationCreated(automation: AppAutomation)
    /// Response to `get_automation` — a single automation row.
    case automationResult(automation: AppAutomation)
    /// Response to `update_automation`, `enable_automation`, or `disable_automation`.
    case automationUpdated(automation: AppAutomation)
    /// Response to `delete_automation` — the id of the deleted row.
    case automationDeleted(automationID: String)
    /// Response to `get_automation_open_task_count`.
    case automationOpenTaskCount(automationID: String, count: Int)
    /// Response to `list_automation_runs` — the run history for one automation.
    case automationRunsList(automationID: String, runs: [AppAutomationRun])
    // MARK: Editorial controls events
    /// Response to `list_editorial_actions` — audit rows for a product,
    /// ordered freshest-first.
    case editorialActionsList(productID: String, actions: [EditorialAction])
    // MARK: Attention events (attentions.md — Notifications toolbar + window)
    /// Reply to `list_attention_groups` — the groups for a product plus
    /// every group's member rows (flattened; bucketed client-side by
    /// `Attention.groupID`).
    case attentionGroupsList(productID: String, groups: [AttentionGroup], members: [Attention])
    /// Reply to `get_attention_group` — one group plus its members.
    case attentionGroupResult(group: AttentionGroup, members: [Attention])
    /// Reply to `create_attention`; also pushed live on the owning
    /// product's work-tree topic when the engine creates an attention.
    case attentionCreated(attention: Attention, group: AttentionGroup)
    /// Pushed (and returned) whenever a group's state or a member's
    /// answer-state changes — e.g. after `answer_attention` /
    /// `dismiss_attention`. Carries the group's refreshed members.
    case attentionGroupUpdated(group: AttentionGroup, members: [Attention])
    /// Pushed after `action_attention_group` succeeds: the now-`actioned`
    /// group, its terminal members, and the produced-artifact ref.
    case attentionGroupActioned(group: AttentionGroup, members: [Attention])
    /// Reply to `list_attention_merges` — the `attention_merges` provenance
    /// rows folded into one canonical `Attention`, chronological.
    case attentionMergesList(attentionID: String, merges: [AttentionMerge])
    // MARK: Editorial controls events
    /// Response to `evaluate_editorial_rules` — the outcome of running the
    /// product's rules against the supplied body + optional title.
    /// `decision` is `"allow"`, `"rewrite"`, or `"deny"`.
    /// `findings` lists human-readable descriptions of every triggered rule.
    /// `rewrittenBody` is present when `decision == "rewrite"`.
    case editorialRulesEvaluated(
        productID: String,
        decision: String,
        findings: [String],
        rewrittenBody: String?
    )
    // MARK: Comments in the markdown viewer (P529 Phase 2)
    /// Reply to `comments_create` / `comments_dismiss` / `comments_set_status`
    /// / `comments_update_anchor` — the single-comment `comment_result` echo of
    /// the persisted `WorkComment` row.
    case commentResult(comment: WorkComment)
    /// Reply to `comments_list` — each comment with its thread entries and the
    /// answer-agent-running flag, for the artifact.
    case commentsList(artifactKind: String, artifactId: String, comments: [CommentWithThread])
    /// Reply to `comments_resolve` (wire type `comments_resolved`) — each active
    /// comment paired with its anchor [`CommentResolution`] against the supplied
    /// plain-text projection.
    case commentsResolved(artifactKind: String, artifactId: String, comments: [ResolvedComment])
    /// Reply to `comments_banner_state` — a read-only `[Revise]`-banner summary
    /// for the artifact.
    case commentsBannerState(artifactKind: String, artifactId: String, state: CommentsBannerState)
    /// Reply to `comments_revise_doc`. Carries no artifact identity (see
    /// `ReviseDocOutcome`'s definition in `wire.rs`), so the bridge correlates
    /// it to the call that issued it by send order.
    case commentsReviseDocResult(outcome: ReviseDocOutcome)
}
