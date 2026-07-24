import Foundation

extension EngineClient {
    func sendListProducts() {
        sendLine(["type": "list_products"])
    }

    /// Ask the engine for the current live runtime snapshot of every
    /// allocated worker slot. Pair this with a subscription to the
    /// `worker.live_states` topic to keep up to date in real time.
    func sendListWorkerLiveStates() {
        sendLine(["type": "list_worker_live_states"])
    }

    /// Ask the engine for the current set of slot ids that have the
    /// live-status summarizer disabled. Used at session start so the
    /// Agents-tab toggle reflects the persisted state.
    func sendListLiveStatusDisabledSlots() {
        sendLine(["type": "list_live_status_disabled_slots"])
    }

    /// Toggle the live-status summarizer for one slot. The engine
    /// persists the choice in its metadata KV so it survives an
    /// engine restart.
    func sendSetLiveStatusEnabled(slotId: Int, enabled: Bool) {
        sendLine([
            "type": "set_live_status_enabled",
            "slot_id": slotId,
            "enabled": enabled,
        ])
    }

    /// Ask the engine for the per-installation settings snapshot.
    /// Used by the Settings window on appear so the rendered state
    /// reflects what the engine has persisted.
    func sendGetSettings() {
        sendLine(["type": "get_settings"])
    }

    /// Ask the engine for its user-visible configuration health.
    /// Called once at session-start (after `connected`) so the
    /// top-of-window banner surfaces a missing `ANTHROPIC_API_KEY`
    /// before the user notices summaries never appear (#699). Cheap
    /// — the engine just reads `Option::is_some` on the agent-config
    /// key; no IO.
    func sendGetEngineHealth() {
        sendLine(["type": "get_engine_health"])
    }

    /// Pause or resume global dispatch — the same `SetDispatchPaused`
    /// RPC `bossctl dispatch resume` drives. Replies with
    /// `FrontendEvent::DispatchStateResult`; the caller re-polls
    /// engine health afterward to clear the paused banner.
    func sendSetDispatchPaused(paused: Bool) {
        sendLine(["type": "set_dispatch_paused", "paused": paused])
    }

    /// Set one per-installation setting. Engine persists to
    /// `settings.toml` and replies with `setting_set` once the
    /// in-memory store is updated.
    func sendSetSetting(key: String, enabled: Bool) {
        sendLine([
            "type": "set_setting",
            "key": key,
            "enabled": enabled,
        ])
    }

    /// Fetch the full host registry (including `local`).
    func sendListHosts() {
        sendLine(["type": "list_hosts"])
    }

    /// Fetch one host by id.
    func sendGetHost(id: String) {
        sendLine(["type": "get_host", "id": id])
    }

    /// Register a new SSH remote host. The engine pushes the wrapper
    /// script and replies with `host_result` (enabled) or `host_result`
    /// with `enabled = false` and `last_error_text` set (push failed).
    func sendAddHost(id: String, sshTarget: String, poolSize: Int = 1, tags: [String] = []) {
        sendLine([
            "type": "add_host",
            "id": id,
            "ssh_target": sshTarget,
            "pool_size": poolSize,
            "tags": tags,
        ])
    }

    /// Enable or disable a registered host.
    func sendSetHostEnabled(id: String, enabled: Bool) {
        sendLine([
            "type": "set_host_enabled",
            "id": id,
            "enabled": enabled,
        ])
    }

    /// Deregister a remote host. Fails for the built-in `local` host.
    func sendRemoveHost(id: String) {
        sendLine(["type": "remove_host", "id": id])
    }

    /// Add a user-defined capability tag to a host.
    func sendAddHostTag(hostId: String, tag: String) {
        sendLine(["type": "add_host_tag", "host_id": hostId, "tag": tag])
    }

    /// Remove a user-defined capability tag from a host.
    func sendRemoveHostTag(hostId: String, tag: String) {
        sendLine(["type": "remove_host_tag", "host_id": hostId, "tag": tag])
    }

    /// Ask the engine for a live snapshot of every registered metric.
    /// Used by the Metrics debug pane on appear and on its 5-second
    /// polling timer so values refresh without a manual reload.
    func sendMetricsListLive() {
        sendLine(["type": "metrics_list_live"])
    }

    /// Signal the engine that the Boss app window just became active.
    /// The engine schedules an immediate pass of every PR-state reconciler
    /// so the kanban reflects upstream GitHub changes (merged PRs, new
    /// review decisions, check-status updates) without waiting for the
    /// next periodic tick. Engine-side quiescing (15 s window) prevents
    /// repeated GitHub API calls on rapid focus-toggle events.
    func sendKickPrReconcilers() {
        sendLine(["type": "kick_pr_reconcilers"])
    }

    /// Ask the engine for the registered feature-flag set. Used by
    /// the Feature Flags debug pane on appear and after every toggle
    /// so the rendered state matches what the engine persisted.
    func sendListFeatureFlags() {
        sendLine(["type": "list_feature_flags"])
    }

    /// Toggle one feature flag. Engine persists to
    /// `feature-flags.toml`, updates the in-memory store, and replies
    /// with `feature_flag_set` once consumer-side `is_enabled` calls
    /// see the new value.
    func sendSetFeatureFlag(name: String, enabled: Bool) {
        sendLine([
            "type": "set_feature_flag",
            "name": name,
            "enabled": enabled,
        ])
    }

    /// Report the capability IDs compiled into this build to the
    /// engine. Called once per session after `RegisterAppSession` is
    /// acknowledged. The engine replies with `feature_flags_list` so
    /// the flag pane immediately reflects `capability_present`.
    func sendRegisterCapabilities(capabilityIds: [String]) {
        sendLine([
            "type": "register_capabilities",
            "capability_ids": capabilityIds,
        ])
    }

    func sendSubscribe(topics: [String]) {
        sendLine([
            "type": "subscribe",
            "topics": topics,
        ])
    }

    func sendUnsubscribe(topics: [String]) {
        sendLine([
            "type": "unsubscribe",
            "topics": topics,
        ])
    }

    /// `flow` tags the population-timing log so cold-start and
    /// product-switch breakdowns can be grepped apart, and lets
    /// [[PopulationTiming]] record the send timestamp for the
    /// request→reply segment plus the per-product session issue count
    /// (which surfaces the cold-start double-fetch — see T2101 R1).
    func sendGetWorkTree(productId: String, flow: PopulationFlow) {
        // Propagate the app-side `fetch_seq` on the wire so the engine can
        // stamp it on its `engine-population-timing-*.jsonl` segments and the
        // two sides join on `(product_id, fetch_seq)` (T2101 engine-side
        // instrumentation follow-up).
        let fetchSeq = PopulationTiming.shared.fetchIssued(productId: productId, flow: flow)
        sendLine([
            "type": "get_work_tree",
            "product_id": productId,
            "fetch_seq": fetchSeq,
        ])
    }

    func sendListAttentionItemsForWorkItem(workItemID: String) {
        sendLine([
            "type": "list_attention_items_for_work_item",
            "work_item_id": workItemID,
        ])
    }

    /// Accept an open `deferred_scope` attention item without filing a
    /// followup task. Replies with `attention_item_updated`.
    func sendAcceptDeferredScopeAttention(id: String) {
        sendLine([
            "type": "accept_deferred_scope_attention",
            "id": id,
        ])
    }

    /// File a followup task from an open `deferred_scope` attention item.
    /// Replies with `attention_item_converted`.
    func sendCreateTaskFromDeferredScopeAttention(attentionID: String) {
        sendLine([
            "type": "create_task_from_deferred_scope_attention",
            "attention_id": attentionID,
        ])
    }

    /// List every open `deferred_scope` attention item across a product.
    /// Replies with `deferred_scope_attentions_list`.
    func sendListDeferredScopeAttentions(productId: String) {
        sendLine([
            "type": "list_deferred_scope_attentions",
            "product_id": productId,
        ])
    }

    // MARK: Planner review/release/undo (auto-populate-project-tasks-on-design-pr-merge.md)

    /// List every `planner_runs` audit row for a project, newest first.
    /// Replies with `planner_runs_list`.
    func sendListPlannerRuns(projectId: String) {
        sendLine([
            "type": "list_planner_runs",
            "project_id": projectId,
        ])
    }

    /// Release a project's staged auto-populate batch: flips `autostart =
    /// true` on every task tagged with the project's live (staged) planner
    /// run. Replies with `release_project_result`.
    func sendReleaseProject(projectId: String) {
        sendLine([
            "type": "release_project",
            "project_id": projectId,
        ])
    }

    /// Undo an auto-populate batch: soft-deletes every task tagged with
    /// `runId` that has no execution yet, and clears the run's idempotency
    /// gate. Replies with `unpopulate_project_result`.
    func sendUnpopulateProject(projectId: String, runId: String) {
        sendLine([
            "type": "unpopulate_project",
            "project_id": projectId,
            "run_id": runId,
        ])
    }

    // MARK: Attention groups (attentions.md — Notifications toolbar + window)

    /// List attention groups for a product. Omitting `state` lets the engine
    /// default to open + partially_answered — the actionable set the
    /// Notifications window renders. Replies with `attention_groups_list`.
    func sendListAttentionGroups(
        productId: String,
        projectId: String? = nil,
        taskId: String? = nil,
        kind: String? = nil,
        state: String? = nil
    ) {
        var payload: [String: Any] = [
            "type": "list_attention_groups",
            "product_id": productId,
        ]
        if let projectId { payload["project_id"] = projectId }
        if let taskId { payload["task_id"] = taskId }
        if let kind { payload["kind"] = kind }
        if let state { payload["state"] = state }
        sendLine(payload)
    }

    /// Fetch one group (`atg_…` or `A<n>`) plus its members. Replies with
    /// `attention_group_result`.
    func sendGetAttentionGroup(id: String) {
        sendLine(["type": "get_attention_group", "id": id])
    }

    /// Record the human's resolution of one member (`atn_…`): an `answer`
    /// (value for questions, omitted to "accept" a followup), `skip`, or
    /// `dismiss`. Replies with `attention_group_updated`.
    func sendAnswerAttention(id: String, answer: String?, skip: Bool, dismiss: Bool) {
        var payload: [String: Any] = [
            "type": "answer_attention",
            "id": id,
            "skip": skip,
            "dismiss": dismiss,
        ]
        if let answer { payload["answer"] = answer }
        sendLine(payload)
    }

    /// Action a group (`atg_…` or `A<n>`) — produce the downstream artifact
    /// and transition it to `actioned`. `skipUnanswered` marks every open
    /// member skipped first so the human needn't touch every row. Replies
    /// with `attention_group_actioned`.
    func sendActionAttentionGroup(id: String, skipUnanswered: Bool) {
        sendLine([
            "type": "action_attention_group",
            "id": id,
            "skip_unanswered": skipUnanswered,
        ])
    }

    /// Dismiss a whole group (`atg_…`) or a single member (`atn_…`) without
    /// producing anything. Replies with `attention_group_updated`.
    func sendDismissAttention(id: String, reason: String? = nil) {
        var payload: [String: Any] = ["type": "dismiss_attention", "id": id]
        if let reason { payload["reason"] = reason }
        sendLine(payload)
    }

    /// Fetch the `attention_merges` provenance rows folded into one
    /// canonical `Attention` (`atn_…`) — feeds the Notifications window's
    /// merge-provenance affordance. Replies with `attention_merges_list`.
    func sendListAttentionMerges(attentionID: String) {
        sendLine(["type": "list_attention_merges", "attention_id": attentionID])
    }

    /// Ask the engine to lease a workspace for the given Review-column
    /// work item, check out the PR head branch, and return the workspace
    /// path for opening a Ghostty terminal. The engine replies with
    /// `review_terminal_ready` or `work_error`.
    func sendOpenReviewTerminal(workItemID: String) {
        sendLine([
            "type": "open_review_terminal",
            "work_item_id": workItemID,
        ])
    }

    /// Ask the engine for a terminal into a Doing-column work item's
    /// already-live execution workspace — no new lease, just the path.
    /// The engine replies with `live_workspace_terminal_ready` or
    /// `work_error`.
    func sendOpenLiveWorkspaceTerminal(workItemID: String) {
        sendLine([
            "type": "open_live_workspace_terminal",
            "work_item_id": workItemID,
        ])
    }

    /// Ask the engine to merge (or queue for merging) the PR associated
    /// with `workItemID`. The task must be `in_review` and carry a PR URL;
    /// any violation is surfaced as a `workError` event. On success the
    /// engine replies with a `mergeWhenReadyAccepted` event and kicks the
    /// PR-reconciler so the kanban state updates promptly.
    func sendMergeWhenReady(workItemID: String) {
        sendLine([
            "type": "merge_when_ready",
            "work_item_id": workItemID,
        ])
    }

    /// Notify the engine that a review terminal window closed so it can
    /// release the associated workspace lease. Fire-and-forget.
    func sendReleaseReviewTerminal(leaseID: String) {
        sendLine([
            "type": "release_review_terminal",
            "lease_id": leaseID,
        ])
    }

    // MARK: GitHub OAuth device-flow (OAuth device-flow design §4)
    //
    // Four unit requests drive the engine-owned device-flow state machine.
    // The engine replies to each with a `git_hub_auth_state` event and also
    // pushes further `git_hub_auth_state` events on the `github.auth` topic
    // as its poll loop advances. The `type` strings are serde's snake_case
    // rendering of the `FrontendRequest::GitHubAuth*` variants.

    /// Begin (or restart) the GitHub OAuth device flow for github.com.
    func sendGitHubAuthStart() {
        sendLine(["type": "git_hub_auth_start"])
    }

    /// Abort an in-progress device-flow authorization.
    func sendGitHubAuthCancel() {
        sendLine(["type": "git_hub_auth_cancel"])
    }

    /// Delete the stored OAuth token and return to `Disconnected`.
    func sendGitHubAuthDisconnect() {
        sendLine(["type": "git_hub_auth_disconnect"])
    }

    /// Request the current GitHub auth state. When connected this also
    /// re-runs the engine's org/SSO probe, so it doubles as the "Re-check"
    /// affordance behind the org-approval / SSO banners (design §7).
    func sendGitHubAuthStatus() {
        sendLine(["type": "git_hub_auth_status"])
    }

    /// Store the Trunk org API token (never logged, persisted to the OS
    /// Keychain by the engine). Replies with `trunk_status` reflecting the
    /// newly stored token.
    func sendTrunkSetToken(token: String) {
        sendLine([
            "type": "trunk_set_token",
            "token": token,
        ])
    }

    /// Request whether a Trunk org API token is currently configured.
    func sendTrunkStatus() {
        sendLine(["type": "trunk_status"])
    }

    func sendCreateProduct(name: String, description: String, repoRemoteURL: String) {
        sendLine([
            "type": "create_product",
            "name": name,
            "description": description,
            "repo_remote_url": repoRemoteURL,
        ])
    }

    func sendCreateProject(productId: String, name: String, description: String, goal: String) {
        sendLine([
            "type": "create_project",
            "product_id": productId,
            "name": name,
            "description": description,
            "goal": goal,
        ])
    }

    func sendCreateTask(
        productId: String,
        projectId: String,
        name: String,
        description: String,
        repoRemoteURL: String? = nil
    ) {
        var payload: [String: Any] = [
            "type": "create_task",
            "product_id": productId,
            "project_id": projectId,
            "name": name,
            "description": description,
            "created_via": "mac_app",
        ]
        if let repoRemoteURL, !repoRemoteURL.isEmpty {
            payload["repo_remote_url"] = repoRemoteURL
        }
        sendLine(payload)
    }

    func sendCreateChore(
        productId: String,
        name: String,
        description: String,
        repoRemoteURL: String? = nil
    ) {
        var payload: [String: Any] = [
            "type": "create_chore",
            "product_id": productId,
            "name": name,
            "description": description,
            "created_via": "mac_app",
        ]
        if let repoRemoteURL, !repoRemoteURL.isEmpty {
            payload["repo_remote_url"] = repoRemoteURL
        }
        sendLine(payload)
    }

    func sendUpdateWorkItem(id: String, patch: [String: Any]) {
        sendLine([
            "type": "update_work_item",
            "id": id,
            "patch": patch,
        ])
    }

    /// Ask the engine to schedule an execution for `workItemId`.
    /// Mirrors the bossctl `work start` path. Idempotent — the
    /// engine treats a non-terminal latest execution as the current
    /// owner and won't create a duplicate. Used by the kanban's
    /// drop-into-Doing flow described in
    /// `tools/boss/docs/designs/work-kanban.md` §1.
    func sendRequestExecution(workItemId: String) {
        sendLine([
            "type": "request_execution",
            "work_item_id": workItemId,
        ])
    }

    func sendDeleteWorkItem(id: String) {
        sendLine([
            "type": "delete_work_item",
            "id": id,
        ])
    }

    func sendSetProductExternalTracker(
        productId: String,
        kind: String,
        config: [String: Any]
    ) {
        sendLine([
            "type": "set_product_external_tracker",
            "product_id": productId,
            "kind": kind,
            "config": config,
            "unset": false,
        ])
    }

    func sendUnsetProductExternalTracker(productId: String) {
        sendLine([
            "type": "set_product_external_tracker",
            "product_id": productId,
            "unset": true,
        ])
    }

    /// Set a product's merge mechanism. `mechanism` must be `"direct"` or
    /// `"trunk_queue"` — the engine validates against `MergeMechanism::parse`
    /// and rejects anything else.
    func sendSetProductMergeMechanism(productId: String, mechanism: String) {
        sendLine([
            "type": "set_product_merge_mechanism",
            "product_id": productId,
            "mechanism": mechanism,
        ])
    }

    func sendReorderProjectTasks(projectId: String, taskIds: [String]) {
        sendLine([
            "type": "reorder_project_tasks",
            "project_id": projectId,
            "task_ids": taskIds,
        ])
    }

    func sendRegisterAppSession() {
        sendLine([
            "type": "register_app_session",
        ])
    }

    func sendRegisterBossSession(shellPid: Int32) {
        sendLine([
            "type": "register_boss_session",
            "shell_pid": Int(shellPid),
        ])
    }

    /// Report the real shell pid for a worker pane after the libghostty
    /// surface initializes. The engine uses this to wire process tracking
    /// so the dead-pid sweep and `bossctl agents stop` can observe and
    /// reap reviewer and other pane-spawned workers.
    func sendUpdateWorkerShellPid(runId: String, shellPid: Int32) {
        sendLine([
            "type": "update_worker_shell_pid",
            "run_id": runId,
            "shell_pid": Int(shellPid),
        ])
    }

    /// Report that a worker pane died before the engine could observe it
    /// any other way — either its libghostty surface never attached or
    /// its shell process exited. The engine reaps the backing execution
    /// immediately instead of waiting for the next dead-pid sweep pass
    /// (up to 60s later) or an app restart.
    func sendWorkerPaneDied(runId: String) {
        sendLine([
            "type": "worker_pane_died",
            "run_id": runId,
        ])
    }

    /// Report that the app can once again host worker panes after a
    /// sleep/wake cycle — `GhosttyRuntime` observed `NSWorkspace`
    /// sleep/wake notifications and confirmed an active display is
    /// present. Lets the engine kick the scheduler immediately instead
    /// of waiting for the next periodic sweep to redispatch anything the
    /// sleep stranded. Fire-and-forget; no response expected.
    func sendSpawnCapabilityRestored() {
        sendLine([
            "type": "spawn_capability_restored",
        ])
    }

    /// Report that a worker pane's shell never came up — the libghostty
    /// surface failed to create (typically `ghostty_surface_new` returning
    /// NULL when there is no active display after sleep/wake). This is the
    /// proactive NACK for the false-live spawn: the spawn RPC was already
    /// answered `Ok(shell_pid: 0)` synchronously because the surface is
    /// created asynchronously, so this is the only way — short of the
    /// engine's 60s spawn-ack timeout — the engine learns the shell never
    /// started. The engine reaps the execution immediately and feeds its
    /// spawn-capability circuit breaker. Fire-and-forget; no response.
    func sendReportWorkerSpawnFailed(runId: String, reason: String) {
        sendLine([
            "type": "report_worker_spawn_failed",
            "run_id": runId,
            "reason": reason,
        ])
    }

    /// Ask the engine for all historical executions of `taskId`, newest-first.
    /// The engine replies with `executions_list`. The wire field is
    /// `work_item_id` — the engine's `ListExecutions` request and
    /// `ExecutionsList` reply both key on it (a task id *is* a work-item
    /// id); sending `task_id` here previously left the filter unset, so the
    /// engine returned every task's executions and the reply (also keyed on
    /// `work_item_id`) was dropped, leaving the viewer's left pane spinning.
    func sendListExecutions(taskId: String) {
        sendLine([
            "type": "list_executions",
            "work_item_id": taskId,
            "include_revision_chain": true,
        ])
    }

    /// Ask the engine for the rendered transcript of one execution. The
    /// engine resolves the durable `work_executions` row (stable, even for
    /// finished/historical runs), reads the JSONL, and replies with
    /// `execution_transcript_result` (segments + live/complete flags) or
    /// `execution_transcript_unavailable` when the file is gone.
    func sendExecutionTranscript(executionId: String) {
        sendLine([
            "type": "execution_transcript",
            "execution_id": executionId,
        ])
    }

    // MARK: - Automation RPCs (maintenance-tasks.md T7)

    /// Ask the engine for all automations for a product, ordered `created_at ASC`.
    /// The engine replies with `automations_list`.
    func sendListAutomations(productId: String) {
        sendLine([
            "type": "list_automations",
            "product_id": productId,
        ])
    }

    /// Create a new automation. The engine replies with `automation_created`.
    func sendCreateAutomation(
        productId: String,
        name: String,
        cron: String,
        timezone: String,
        standingInstruction: String,
        openTaskLimit: Int = 1,
        enabled: Bool = true,
        repoRemoteURL: String? = nil
    ) {
        var payload: [String: Any] = [
            "type": "create_automation",
            "product_id": productId,
            "name": name,
            "trigger": [
                "kind": "schedule",
                "cron": cron,
                "timezone": timezone,
            ] as [String: Any],
            "standing_instruction": standingInstruction,
            "open_task_limit": openTaskLimit,
            "enabled": enabled,
            "created_via": "mac_app",
        ]
        if let repoRemoteURL, !repoRemoteURL.isEmpty {
            payload["repo_remote_url"] = repoRemoteURL
        }
        sendLine(payload)
    }

    /// Enable an automation (set `enabled = true`). Engine replies with `automation_updated`.
    func sendEnableAutomation(id: String) {
        sendLine(["type": "enable_automation", "id": id])
    }

    /// Disable an automation (set `enabled = false`). Engine replies with `automation_updated`.
    func sendDisableAutomation(id: String) {
        sendLine(["type": "disable_automation", "id": id])
    }

    /// Delete an automation and its run history. Engine replies with `automation_deleted`.
    func sendDeleteAutomation(id: String) {
        sendLine(["type": "delete_automation", "id": id])
    }

    /// Update an automation's mutable fields. Engine replies with `automation_updated`.
    func sendUpdateAutomation(id: String, patch: [String: Any]) {
        sendLine(["type": "update_automation", "id": id, "patch": patch])
    }

    /// Get the count of open tasks produced by an automation. Engine replies
    /// with `automation_open_task_count`.
    func sendGetAutomationOpenTaskCount(automationId: String) {
        sendLine(["type": "get_automation_open_task_count", "automation_id": automationId])
    }

    /// List the run history for an automation (newest first). Engine replies
    /// with `automation_runs_list`.
    func sendListAutomationRuns(automationId: String) {
        sendLine(["type": "list_automation_runs", "automation_id": automationId])
    }

    // MARK: Editorial controls

    /// Set a product's editorial rules. Engine replies with `work_item_updated`
    /// carrying the updated product row. Pass `nil` for `rules` to clear.
    func sendSetProductEditorialRules(productId: String, rules: EditorialRules?) {
        if let rules = rules,
           let data = try? JSONEncoder().encode(rules),
           let rulesObj = try? JSONSerialization.jsonObject(with: data) {
            sendLine([
                "type": "set_product_editorial_rules",
                "product_id": productId,
                "rules": rulesObj,
            ])
        } else {
            sendLine([
                "type": "set_product_editorial_rules",
                "product_id": productId,
                "rules": NSNull(),
            ])
        }
    }

    /// List recent editorial hook decisions for a product, freshest first.
    /// Engine replies with `editorial_actions_list`.
    func sendListEditorialActions(productId: String, limit: Int? = nil) {
        var msg: [String: Any] = [
            "type": "list_editorial_actions",
            "product_id": productId,
        ]
        if let limit { msg["limit"] = limit }
        sendLine(msg)
    }

    /// Evaluate a product's editorial rules against a candidate PR body +
    /// optional title without touching GitHub. The engine replies with
    /// `editorial_rules_evaluated` carrying the decision, findings list, and
    /// (when decision == "rewrite") the sanitised body.
    func sendEvaluateEditorialRules(productId: String, body: String, title: String? = nil) {
        var payload: [String: Any] = [
            "type": "evaluate_editorial_rules",
            "product_id": productId,
            "body": body,
        ]
        if let title {
            payload["title"] = title
        }
        sendLine(payload)
    }

    // MARK: - Comments in the markdown viewer (P529 Phase 2)
    //
    // The engine ships these RPCs (`engine/core/src/app/comments.rs`); this is
    // the macOS half PR #915 deferred. Requests are `FrontendRequest` variants
    // tagged by a snake_case `type`; `comments_create` / `comments_revise_doc`
    // flatten their input struct, so those fields sit at the top level. The
    // engine's `CommentAnchor` serialises `{exact, prefix, suffix}`.

    /// Create an `active` comment on an artifact. Engine replies `comment_result`
    /// with the persisted `WorkComment`.
    func sendCommentsCreate(
        artifactKind: String,
        artifactId: String,
        anchor: CommentAnchor,
        body: String,
        author: String,
        docVersion: String,
        plainTextProjectionVersion: Int
    ) {
        sendLine([
            "type": "comments_create",
            "artifact_kind": artifactKind,
            "artifact_id": artifactId,
            "anchor": anchorPayload(anchor),
            "body": body,
            "author": author,
            "doc_version": docVersion,
            "plain_text_projection_version": plainTextProjectionVersion,
        ])
    }

    /// List comments for an artifact. Excludes `resolved` / `dismissed` unless
    /// `includeResolved`. Engine replies `comments_list` with `CommentWithThread`s.
    func sendCommentsList(artifactKind: String, artifactId: String, includeResolved: Bool = false) {
        sendLine([
            "type": "comments_list",
            "artifact_kind": artifactKind,
            "artifact_id": artifactId,
            "include_resolved": includeResolved,
        ])
    }

    /// Resolve every active comment on the artifact against the renderer's
    /// current plain-text projection. The engine re-anchors (persisting fuzzy
    /// hits and flipping unresolvable comments to `orphaned`) and replies
    /// `comments_resolved` with each comment + its `CommentResolution`.
    func sendCommentsResolve(
        artifactKind: String,
        artifactId: String,
        plainText: String,
        plainTextProjectionVersion: Int
    ) {
        sendLine([
            "type": "comments_resolve",
            "artifact_kind": artifactKind,
            "artifact_id": artifactId,
            "plain_text": plainText,
            "plain_text_projection_version": plainTextProjectionVersion,
        ])
    }

    /// Soft-dismiss: transition a comment to `resolved`. Engine replies
    /// `comment_result`.
    func sendCommentsDismiss(commentId: String, actor: String? = nil) {
        var payload: [String: Any] = ["type": "comments_dismiss", "comment_id": commentId]
        if let actor { payload["actor"] = actor }
        sendLine(payload)
    }

    /// Set a comment's status (`active` / `resolved` / `orphaned` / `dismissed`).
    /// Engine replies `comment_result`.
    func sendCommentsSetStatus(commentId: String, status: String, actor: String? = nil) {
        var payload: [String: Any] = [
            "type": "comments_set_status",
            "comment_id": commentId,
            "status": status,
        ]
        if let actor { payload["actor"] = actor }
        sendLine(payload)
    }

    /// Write back a re-resolved anchor (used for a renderer-side re-anchor).
    /// Engine replies `comment_result`. Note the engine also persists fuzzy
    /// re-anchors as a side effect of `comments_resolve`, so the resolve-on-load
    /// loop does not need this in the common case.
    func sendCommentsUpdateAnchor(
        commentId: String,
        anchor: CommentAnchor,
        newDocVersion: String,
        plainTextProjectionVersion: Int
    ) {
        sendLine([
            "type": "comments_update_anchor",
            "comment_id": commentId,
            "anchor": anchorPayload(anchor),
            "new_doc_version": newDocVersion,
            "plain_text_projection_version": plainTextProjectionVersion,
        ])
    }

    private func anchorPayload(_ anchor: CommentAnchor) -> [String: Any] {
        ["exact": anchor.exact, "prefix": anchor.prefix, "suffix": anchor.suffix]
    }

    /// Manually reclassify a comment's intent (sidebar badge override).
    /// Engine replies `comment_result` with the updated `WorkComment`.
    func sendCommentsSetIntent(commentId: String, intent: String) {
        sendLine([
            "type": "comments_set_intent",
            "comment_id": commentId,
            "intent": intent,
        ])
    }

    /// Operator-authored reply in a bucket-2 comment's thread. Only valid
    /// while the comment is `answered`; the engine transitions it to
    /// `awaiting_followup` and kicks off the async reclassifier off the
    /// request's critical path — its outcome arrives on the artifact's
    /// `comments.artifact.*` topic push, not a direct reply.
    func sendCommentsPostFollowup(commentId: String, body: String, author: String) {
        sendLine([
            "type": "comments_post_followup",
            "comment_id": commentId,
            "body": body,
            "author": author,
        ])
    }

    /// Read-only `[Revise]`-banner summary for an artifact. Engine replies
    /// `comments_banner_state`.
    func sendCommentsBannerState(artifactKind: String, artifactId: String) {
        sendLine([
            "type": "comments_banner_state",
            "artifact_kind": artifactKind,
            "artifact_id": artifactId,
        ])
    }

    /// The `[Revise]`-banner action: batch-address every unaddressed
    /// directive/larger_change comment on the artifact. Engine replies
    /// `comments_revise_doc_result`.
    func sendCommentsReviseDoc(artifactKind: String, artifactId: String) {
        sendLine([
            "type": "comments_revise_doc",
            "artifact_kind": artifactKind,
            "artifact_id": artifactId,
        ])
    }

    /// Resolve a project's design-doc pointer. Engine replies with
    /// `project_design_doc_resolved` carrying a
    /// `ResolveProjectDesignDocOutput` whose `state` discriminator
    /// drives the kanban affordance and the open dispatcher. No DB
    /// writes; no topic events — callers can re-issue lazily as cards
    /// scroll into view without polluting the work tree.
    func sendResolveProjectDesignDoc(projectID: String) {
        sendLine([
            "type": "resolve_project_design_doc",
            "project_id": projectID,
        ])
    }

    /// Ask the engine for the markdown files at HEAD of `productID`'s
    /// configured repo. Engine replies with `product_design_docs_list`
    /// carrying a `DesignDocTreeState`. Read-only; nothing on this
    /// machine's filesystem is consulted.
    ///
    /// The engine validates its listing cache against the repo's live
    /// HEAD on every call, so the default (`refresh: false`) is already
    /// never stale. Pass `refresh: true` only for the explicit reload
    /// affordance — it additionally re-reads the repo's default branch.
    func sendListProductDesignDocs(productID: String, refresh: Bool = false) {
        sendLine([
            "type": "list_product_design_docs",
            "product_id": productID,
            "refresh": refresh,
        ])
    }

    /// Fetch one document's body from GitHub. `gitRef` is the commit sha
    /// the listing was read at, so the body returned is the one the
    /// operator saw listed even if the branch has since moved. Engine
    /// replies with `product_design_doc_content`.
    func sendGetProductDesignDoc(ref: DesignDocRef) {
        sendLine([
            "type": "get_product_design_doc",
            "repo_remote_url": ref.repoRemoteURL,
            "path": ref.path,
            "git_ref": ref.gitRef,
        ])
    }

    /// Engine-tab listing fetch (Phase 5 #14). `productID = nil`
    /// returns every product's attempts; `statuses` is AND-ed on the
    /// server, `limit` caps the response.
    func sendListConflictResolutions(
        productID: String? = nil,
        statuses: [String] = [],
        workItemID: String? = nil,
        limit: Int? = nil
    ) {
        var payload: [String: Any] = ["type": "list_conflict_resolutions"]
        if let productID {
            payload["product_id"] = productID
        }
        if !statuses.isEmpty {
            payload["status"] = statuses
        }
        if let workItemID {
            payload["work_item_id"] = workItemID
        }
        if let limit {
            payload["limit"] = limit
        }
        sendLine(payload)
    }

    /// Engine-tab listing fetch for CI remediations (design Phase 11
    /// #37). Mirror of `sendListConflictResolutions`.
    func sendListCiRemediations(
        productID: String? = nil,
        statuses: [String] = [],
        workItemID: String? = nil,
        limit: Int? = nil
    ) {
        var payload: [String: Any] = ["type": "list_ci_remediations"]
        if let productID {
            payload["product_id"] = productID
        }
        if !statuses.isEmpty {
            payload["status"] = statuses
        }
        if let workItemID {
            payload["work_item_id"] = workItemID
        }
        if let limit {
            payload["limit"] = limit
        }
        sendLine(payload)
    }
}
