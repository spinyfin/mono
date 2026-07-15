import Foundation

/// The engine-event dispatch switch: every `EngineEvent` the socket emits
/// lands here and is routed to the model state or the handler that owns it.
/// Arms that need more than a few lines of work delegate to a named helper
/// elsewhere — work-item mutations to `ChatViewModel+WorkItemEvents.swift`,
/// connection lifecycle to `ChatViewModel+Connection.swift`, attention groups
/// to `ChatViewModel+Attentions.swift`, and so on. Keep this file about
/// routing; put the work in the extension that owns the state.
extension ChatViewModel {
    /// Point the engine's event stream at `handle(_:)`. Called from
    /// `commonInit`; kept here so the dispatch itself stays private to this
    /// file and every event path is visible in one place.
    func bindEngineEventStream() {
        engine.onEvent = { [weak self] event in
            self?.handle(event)
        }
    }

    private func handle(_ event: EngineEvent) {
        switch event {
        case .connected:
            markConnected()
            resetConnectionLostBanner()
            engine.sendRegisterAppSession()
            refreshWorkSubscriptions()
            // Re-subscribe any open markdown viewers' comment topics and reload
            // them; the engine dropped every subscription on the disconnect.
            commentBridge.handleReconnected()
            engine.sendListProducts()
            engine.sendListWorkerLiveStates()
            engine.sendListLiveStatusDisabledSlots()
            // Pull the engine's configuration health on every (re)connect
            // so the top-of-window banner reflects the *current* engine,
            // not the one we attached to before a restart (#699).
            engine.sendGetEngineHealth()
            // Pull the current GitHub OAuth auth state so the "GitHub
            // account" settings subsection reflects a token persisted by a
            // prior session (the engine restores it from the keychain at
            // boot) without waiting for a device-flow transition.
            engine.sendGitHubAuthStatus()
            if let productID = currentSelectedProductID {
                // Cold-start population of the restored product. NOTE: the
                // `.productsList` handler below fetches this same product a
                // SECOND time (the confirmed cold-start double-fetch, T2101
                // R2) — the per-product fetch counter makes both visible.
                engine.sendGetWorkTree(productId: productID, flow: .coldStart)
                engine.sendListAttentionGroups(productId: productID)
            }
        case .resyncRequired:
            handleResyncRequired() // socket never went down; see ChatViewModel+Connection.swift
        case .appSessionRegistered:
            isAppSessionRegistered = true
            maybeRegisterBossSession()
            engine.sendRegisterCapabilities(capabilityIds: CapabilityRegistry.shared.all)
        case .bossSessionRegistered:
            break
        case .engineRequest(let requestId, let request):
            handleEngineRequest(requestId: requestId, request: request)
        case .disconnected:
            isConnected = false
            isAppSessionRegistered = false
            subscribedWorkTopics.removeAll()
            for (productID, state) in automationsFetchStateByProductID {
                if case .loading = state {
                    automationsFetchStateByProductID[productID] = .failed("Connection lost")
                }
            }
            scheduleConnectionLostBannerCheck()
        case .workInvalidated(let topic, let productId, _):
            if CommentEngineBridge.isCommentTopic(topic) {
                // A comment row on an open viewer's artifact changed elsewhere;
                // the bridge reloads the bound layer(s). Invalidation-not-patch.
                commentBridge.handleCommentInvalidation(topic: topic)
            }
            if topic == "work.products" {
                engine.sendListProducts()
            }
            if let selectedProductID = currentSelectedProductID,
               topic == workTopic(forProductID: selectedProductID)
            {
                refetchForInvalidation(productID: selectedProductID)
            } else if let productId,
                      productId == currentSelectedProductID {
                refetchForInvalidation(productID: productId)
            }
        case .productsList(let products):
            self.products = products.sorted(by: { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending })
            let activeIDs = Set(activeProducts.map(\.id))
            if let selectedWorkProductID,
               !activeIDs.contains(selectedWorkProductID) {
                let archivedName = self.products.first(where: { $0.id == selectedWorkProductID })?.name
                self.selectedWorkProductID = nil
                self.selectedProjectFilterIDs = []
                self.selectedWorkCardID = nil
                persistSelectedProductID(nil)
                persistProjectFilterIDs()
                if let archivedName {
                    workErrorMessage = "Product \"\(archivedName)\" was archived elsewhere; switching to the next active product."
                }
            }
            if currentSelectedProductID == nil, let first = activeProducts.first {
                self.selectedWorkProductID = first.id
                persistSelectedProductID(first.id)
                engine.sendGetWorkTree(productId: first.id, flow: .coldStart)
            } else if let productID = currentSelectedProductID {
                // On cold start this is the redundant SECOND fetch of the
                // already-restored product (see `.connected` above); it also
                // fires on later `products_list` refreshes. Tagged cold_start
                // because the double-fetch is the case worth spotting; the
                // per-product fetch counter disambiguates either way.
                engine.sendGetWorkTree(productId: productID, flow: .coldStart)
            }
            refreshWorkSubscriptions()
        case .projectsList(let productId, let projects):
            projectsByProductID[productId] = projects.sorted(by: projectSort)
        case .workTree(let product, let projects, let tasks, let chores, let taskRuntimes, let dependencies):
            applyWorkTree(
                product: product,
                projects: projects,
                tasks: tasks,
                chores: chores,
                taskRuntimes: taskRuntimes,
                dependencies: dependencies
            )
        case .workItemCreated(let item):
            handleCreatedWorkItem(item)
        case .workItemsCreated(let items):
            handleCreatedWorkItemsBatch(items)
        case .workItemUpdated(let item):
            handleUpdatedWorkItem(item)
        case .projectTasksReordered(let projectId, _):
            if let productID = project(withID: projectId)?.productID {
                engine.sendGetWorkTree(productId: productID, flow: .itemRefetch)
            }
        case .workItemDeleted(let id):
            let deletedTask = task(withID: id)
            if selectedTask?.id == id {
                selectedWorkCardID = nil
            }
            if let productID = deletedTask?.productID ?? currentSelectedProductID {
                engine.sendGetWorkTree(productId: productID, flow: .itemRefetch)
            }
        case .workError(let message):
            // Allow the user to retry any in-flight review terminal or
            // merge-when-ready request that failed.
            if case .loading = editorialEvaluationState {
                editorialEvaluationState = .failed(message)
            }
            openingReviewTerminalIDs.removeAll()
            openingLiveWorkspaceTerminalIDs.removeAll()
            mergingWhenReadyIDs.removeAll()
            plannerActionInFlightProjectIDs.removeAll()
            if case .loading = reviewTerminalVM.state {
                reviewTerminalVM.state = .idle
            }
            if !pendingMoveOriginByTaskID.isEmpty {
                // Error is likely from an in-flight kanban move: bounce the
                // card(s) back and show an inline non-blocking notice instead
                // of interrupting with a modal dialog.
                bounceBackOptimisticMoves(message: message)
            } else {
                workErrorMessage = message
            }
        case .error(let message):
            if Self.isSocketTransportError(message) {
                // Transport errors fire continuously while the engine
                // is unreachable (every reconnect attempt re-emits a
                // `socket waiting:` line). Routing them through the
                // work-error modal makes the app unusable: dismissing
                // re-opens it on the next retry. The disconnected
                // banner in the main chrome is the user-facing signal
                // for this state — see `showConnectionLostBanner` in
                // ContentView.
                appendSystemMessage(message)
                return
            }
            workErrorMessage = message
        case .workerLiveStatesList(let states):
            liveWorkerStates.update(states: states)
        case .liveStatusDisabledSlotsList(let slotIds):
            liveStatusDisabledSlotIDs = Set(slotIds)
        case .liveStatusEnabledSet(let slotId, let enabled):
            if enabled {
                liveStatusDisabledSlotIDs.remove(slotId)
            } else {
                liveStatusDisabledSlotIDs.insert(slotId)
            }
        case .featureFlagsList(let flags):
            featureFlags = flags
        case .featureFlagSet(let name, let enabled):
            // Patch the cached snapshot so the toggle commits without
            // a second round-trip. The engine has already persisted
            // the value at this point — the patch is a UI mirror.
            if let idx = featureFlags.firstIndex(where: { $0.name == name }) {
                let prior = featureFlags[idx]
                featureFlags[idx] = FeatureFlag(
                    name: prior.name,
                    description: prior.description,
                    category: prior.category,
                    defaultEnabled: prior.defaultEnabled,
                    enabled: enabled,
                    capabilityPresent: prior.capabilityPresent
                )
            }
        case .engineHealthResult(let apiKeyPresent, let issues):
            engineAnthropicApiKeyPresent = apiKeyPresent
            engineHealthIssues = issues
        case .enginePoolConfig(let workerSlots, let automationSlots, let reviewSlots, let coordinatorModel):
            panePoolConfigHandler?(workerSlots, automationSlots, reviewSlots, coordinatorModel)
        case .settingsList(let settings):
            engineSettings = settings
        case .settingSet(let key, let enabled):
            if let idx = engineSettings.firstIndex(where: { $0.key == key }) {
                let prior = engineSettings[idx]
                engineSettings[idx] = EngineSetting(
                    key: prior.key,
                    description: prior.description,
                    defaultEnabled: prior.defaultEnabled,
                    enabled: enabled
                )
            }
        case .hostsList(let hosts):
            registeredHosts = hosts
        case .hostResult(let host):
            if let idx = registeredHosts.firstIndex(where: { $0.hostId == host.hostId }) {
                registeredHosts[idx] = host
            } else {
                registeredHosts.append(host)
            }
        case .hostUpdated(let host):
            if let idx = registeredHosts.firstIndex(where: { $0.hostId == host.hostId }) {
                registeredHosts[idx] = host
            }
        case .hostRemoved(let id):
            registeredHosts.removeAll { $0.hostId == id }
        case .metricsListLiveResult(let entries):
            engineMetrics = entries
        case .projectDesignDocResolved(let output):
            // Batch bookkeeping and its timing log are private to
            // ChatViewModel.swift, alongside `refreshDesignDocStates`.
            applyResolvedProjectDesignDoc(output)
        case .conflictResolutionsList(let attempts):
            conflictResolutions = attempts
        case .conflictResolutionStarted(_, _, _, let prURL):
            // A (re)dispatch of the conflict resolver means the PR is
            // conflicting again — the prior "conflict cleared" badge is
            // stale and must be removed (T778). Mirrors the ciRemediationStarted
            // arm that clears recentlyClearedCIPRs for the same reason.
            recentlyClearedConflictPRs.removeValue(forKey: prURL)
            engine.sendListConflictResolutions(limit: 200)
        case .conflictResolutionFailed, .conflictResolutionAbandoned:
            // Refreshes the engine-tab list so the status column re-renders.
            // These don't touch the badge: failure/abandon don't un-clear a
            // previously cleared conflict — only a new start signals re-conflict.
            engine.sendListConflictResolutions(limit: 200)
        case .conflictResolutionSucceeded(_, _, _, let prURL):
            // Stamp the PR url so the kanban card shows the
            // "🔧 conflict cleared" chip for the next 24h (#15). The
            // engine doesn't carry a finished_at on the push, so we
            // record the wall-clock observation time — close enough
            // for an ageing window measured in hours.
            recentlyClearedConflictPRs[prURL] = Date()
            engine.sendListConflictResolutions(limit: 200)
        case .ciRemediationsList(let attempts):
            ciRemediations = attempts
            // Reconcile both PR-card chips against the row list, which is
            // freshest-first (`created_at DESC, id DESC`) — so the first
            // row seen per PR is that PR's latest attempt:
            //  - latest non-terminal (pending/running): mark the failure
            //    chip `in_flight` if none exists yet, and drop any stale
            //    "ci auto-fixed" chip — a fresh attempt means the prior
            //    auto-fix claim no longer holds even if the
            //    `ciRemediationStarted` push that would normally clear it
            //    was missed (T2764: the push is the only other writer of
            //    `recentlyClearedCIPRs`, so a dropped push stranded the
            //    badge for up to the full freshness window).
            //  - latest succeeded and still fresh: (re)stamp the
            //    "ci auto-fixed" chip using the engine's own timestamp
            //    rather than local observation time, so a missed
            //    `ciRemediationSucceeded` push self-heals on the next
            //    list refresh instead of never showing the chip at all.
            // Exhausted chips are sticky until the user clears them via
            // retry — they are not derivable from the row list alone (the
            // engine tracks them via `task_blocked_signals`), so we leave
            // pre-existing exhausted chips alone.
            var seenPRs = Set<String>()
            for row in attempts {
                guard seenPRs.insert(row.prURL).inserted else { continue }
                switch row.status {
                case "pending", "running":
                    if ciFailureBadges[row.prURL] == nil {
                        ciFailureBadges[row.prURL] = CiFailureBadge(
                            state: .inFlight,
                            attemptsUsed: 0,
                            budget: 0,
                        )
                    }
                    recentlyClearedCIPRs.removeValue(forKey: row.prURL)
                case "succeeded":
                    if let observedAt = Self.parseCiRemediationTimestamp(row.finishedAt ?? row.createdAt),
                       Date().timeIntervalSince(observedAt) < badgeFreshnessWindow {
                        recentlyClearedCIPRs[row.prURL] = observedAt
                    }
                default:
                    break
                }
            }
        case .ciRemediationStarted(_, _, _, let prURL, _):
            // A fresh CI attempt was created (detect path or `retry`).
            // The card stays in `blocked: ci_failure` — the in-flight
            // chip lives until the next probe either reports clean or
            // hits the budget. We don't know used/budget here; the
            // exhausted arm carries those. Show a stub chip with
            // (0, 0) so the card surfaces the in-flight state until
            // the next list refresh fills in real numbers.
            // A new failure makes any prior "ci auto-fixed" claim stale:
            // if the auto-fix didn't stick, the badge is misleading (T606).
            recentlyClearedCIPRs.removeValue(forKey: prURL)
            if ciFailureBadges[prURL] == nil {
                ciFailureBadges[prURL] = CiFailureBadge(state: .inFlight, attemptsUsed: 0, budget: 0)
            } else if var existing = ciFailureBadges[prURL] {
                existing.state = .inFlight
                ciFailureBadges[prURL] = existing
            }
            engine.sendListCiRemediations(limit: 200)
        case .ciRemediationSucceeded(_, _, _, let prURL):
            // Engine observed CI back at clean and retired the attempt.
            // Drop the failure chip and stamp the "✅ ci auto-fixed"
            // chip for the next 24h (per design Q11).
            ciFailureBadges.removeValue(forKey: prURL)
            recentlyClearedCIPRs[prURL] = Date()
            engine.sendListCiRemediations(limit: 200)
        case .ciFailureCleared(_, _, let prURL):
            // Engine cleared `blocked: ci_failure` but found no active
            // remediation attempt (the prior attempt was already terminal).
            // Clear the failure badge only — do NOT set the auto-fixed badge
            // because the clearance was not driven by an auto-fix (T606).
            ciFailureBadges.removeValue(forKey: prURL)
        case .ciRemediationFailed(_, _, _, _, _),
             .ciRemediationAbandoned(_, _, _, _, _):
            // Terminal failures keep the parent `blocked: ci_failure`
            // until the engine either retries or exhausts. The list
            // refresh keeps the engine tab consistent.
            engine.sendListCiRemediations(limit: 200)
        case .ciRemediationExhausted(_, _, let prURL, let used, let budget):
            // Budget exhausted means CI is still failing and auto-fix
            // cannot help further. Any prior "ci auto-fixed" claim is now
            // stale (T606).
            recentlyClearedCIPRs.removeValue(forKey: prURL)
            ciFailureBadges[prURL] = CiFailureBadge(state: .exhausted, attemptsUsed: used, budget: budget)
            engine.sendListCiRemediations(limit: 200)
        case .attentionItemsForWorkItemList(let workItemID, let items):
            attentionItemsByWorkItemID[workItemID] = items
        case .attentionItemCreated, .attentionItemUpdated, .attentionItemConverted, .deferredScopeAttentionsList:
            handleDeferredScopeEvent(event)
        case .plannerRunsList(let projectID, let runs):
            plannerRunsByProjectID[projectID] = runs
        case .releaseProjectResult(let projectID, _, _):
            handlePlannerActionResult(projectID: projectID)
        case .unpopulateProjectResult(let projectID, _, _, _):
            handlePlannerActionResult(projectID: projectID)
        case .attentionGroupsList(let productID, let groups, let members):
            applyAttentionGroupsList(productID: productID, groups: groups, members: members)
        case .attentionGroupResult(let group, let members):
            upsertAttentionGroup(group)
            attentionMembersByGroupID[group.id] = members
        case .attentionCreated(let attention, let group):
            upsertAttentionGroup(group)
            upsertAttentionMember(attention)
        case .attentionGroupUpdated(let group, let members):
            upsertAttentionGroup(group)
            attentionMembersByGroupID[group.id] = members
        case .attentionGroupActioned(let group, let members):
            upsertAttentionGroup(group)
            attentionMembersByGroupID[group.id] = members
        case .attentionMergesList(let attentionID, let merges):
            attentionMergesByAttentionID[attentionID] = merges
        case .reviewTerminalReady(let workItemID, let workspacePath, let leaseID):
            openingReviewTerminalIDs.remove(workItemID)
            let content = reviewTerminalContent(
                workItemID: workItemID,
                workspacePath: workspacePath,
                leaseID: leaseID
            )
            if reviewTerminalVM.windowIsOpen {
                reviewTerminalVM.state = .ready(content)
            } else {
                // Window was closed while the engine was still setting up.
                // Release the lease immediately since nobody will consume it.
                engine.sendReleaseReviewTerminal(leaseID: leaseID)
            }
        case .liveWorkspaceTerminalReady(let workItemID, let workspacePath):
            openingLiveWorkspaceTerminalIDs.remove(workItemID)
            let content = reviewTerminalContent(
                workItemID: workItemID,
                workspacePath: workspacePath,
                leaseID: nil
            )
            // No lease was created for this path, so unlike the review-
            // terminal case above there is nothing to release if the
            // window already closed — just drop the content.
            if reviewTerminalVM.windowIsOpen {
                reviewTerminalVM.state = .ready(content)
            }
        case .mergeWhenReadyAccepted(let workItemID, _, _):
            // Engine successfully initiated the merge. Clear the in-flight
            // guard so the button re-enables if the user wants to retry.
            // The PR-reconciler was kicked on the engine side, so a
            // WorkItemUpdated event carrying the new merge-queue / merged
            // state will arrive shortly.
            mergingWhenReadyIDs.remove(workItemID)
        case .gitHubAuthState(let state):
            // The engine pushes this on every device-flow transition (and
            // as the reply to a `git_hub_auth_*` request). The settings
            // subsection observes `gitHubAuthState` and re-renders.
            gitHubAuthState = state
        case .executionsList(let taskId, let executions):
            executionsByTaskID[taskId] = executions
        case .executionTranscriptResult(let executionId, let segments, let isLive, let complete):
            transcriptsByExecutionID[executionId] = .loaded(
                TranscriptDoc(
                    executionId: executionId,
                    segments: segments,
                    isLive: isLive,
                    complete: complete
                )
            )
        case .executionTranscriptUnavailable(let executionId, let reason):
            transcriptsByExecutionID[executionId] = .unavailable(reason: reason)
        // MARK: Automation events
        case .automationsList(let productID, let automations, let openTaskCounts):
            automationsByProductID[productID] = automations
            automationsFetchStateByProductID[productID] = .loaded
            for (id, count) in openTaskCounts {
                openTaskCountByAutomationID[id] = count
            }
            for automation in automations {
                engine.sendListAutomationRuns(automationId: automation.id)
            }
        case .automationCreated(let automation):
            upsertAutomation(automation)
            selectedAutomationID = automation.id
            engine.sendGetAutomationOpenTaskCount(automationId: automation.id)
            engine.sendListAutomationRuns(automationId: automation.id)
        case .automationResult(let automation):
            upsertAutomation(automation)
            engine.sendListAutomationRuns(automationId: automation.id)
        case .automationUpdated(let automation):
            upsertAutomation(automation)
            engine.sendGetAutomationOpenTaskCount(automationId: automation.id)
            engine.sendListAutomationRuns(automationId: automation.id)
        case .automationDeleted(let automationID):
            for productID in automationsByProductID.keys {
                automationsByProductID[productID]?.removeAll { $0.id == automationID }
            }
            openTaskCountByAutomationID.removeValue(forKey: automationID)
            automationRunsByID.removeValue(forKey: automationID)
        case .automationOpenTaskCount(let automationID, let count):
            openTaskCountByAutomationID[automationID] = count
        case .automationRunsList(let automationID, let runs):
            automationRunsByID[automationID] = runs
        // MARK: Editorial controls events
        case .editorialActionsList(let productID, let actions):
            editorialActionsByProductID[productID] = actions
            editorialActionsFetchStateByProductID[productID] = .loaded
        case .editorialRulesEvaluated(let productID, let decision, let findings, let rewrittenBody):
            guard productID == editorialControlsProductID else { break }
            editorialEvaluationState = .result(
                decision: decision,
                findings: findings,
                rewrittenBody: rewrittenBody
            )
        // MARK: Comments (P529 Phase 2)
        case .commentsList(let artifactKind, let artifactId, let comments):
            commentBridge.handleCommentsList(artifactKind: artifactKind, artifactId: artifactId, comments: comments)
        case .commentsResolved(let artifactKind, let artifactId, let comments):
            commentBridge.handleCommentsResolved(artifactKind: artifactKind, artifactId: artifactId, comments: comments)
        case .commentResult(let comment):
            commentBridge.handleCommentResult(comment)
        case .commentsBannerState(let artifactKind, let artifactId, let state):
            commentBridge.handleCommentsBannerState(artifactKind: artifactKind, artifactId: artifactId, state: state)
        case .commentsReviseDocResult(let outcome):
            commentBridge.handleCommentsReviseDocResult(outcome)
        }
    }

    // MARK: - Engine requests

    /// The engine's request/response arm: the engine asks the app to do
    /// something only it can (drive a libghostty pane, reveal a card) and
    /// waits on a matching response keyed by `requestId`. Every arm must
    /// send exactly one response — a build with no pane allocator (Bazel
    /// without GhosttyKit) answers with `internalFailure` rather than
    /// leaving the engine's request hanging.
    private func handleEngineRequest(requestId: String, request: EngineRequestKind) {
        switch request {
        case .spawnWorkerPane(let spawn):
            let result = paneSpawnHandler.map { $0(spawn) } ?? .failure(.internalFailure(Self.noPaneAllocatorReason))
            engine.sendSpawnWorkerPaneResponse(requestId: requestId, result: result)
        case .releaseWorkerPane(let slotId, let killGrace):
            let result = paneReleaseHandler.map { $0(slotId, killGrace) } ?? .failure(.internalFailure(Self.noPaneAllocatorReason))
            engine.sendReleaseWorkerPaneResponse(requestId: requestId, result: result)
        case .sendToPane(let slotId, let text):
            let result = paneSendHandler.map { $0(slotId, text) } ?? .failure(.internalFailure(Self.noPaneAllocatorReason))
            engine.sendSendToPaneResponse(requestId: requestId, result: result)
        case .focusWorkerPane(let slotId):
            let result = paneFocusHandler.map { $0(slotId) } ?? .failure(.internalFailure(Self.noPaneAllocatorReason))
            engine.sendFocusWorkerPaneResponse(requestId: requestId, result: result)
        case .interruptWorkerPane(let slotId):
            let result = paneInterruptHandler.map { $0(slotId) } ?? .failure(.internalFailure(Self.noPaneAllocatorReason))
            engine.sendInterruptWorkerPaneResponse(requestId: requestId, result: result)
        case .revealWorkItem(let workItemId, let productId):
            switch revealWorkCard(workItemId, productID: productId) {
            case .revealed, .deferred:
                engine.sendRevealWorkItemResponse(requestId: requestId, result: .success)
            case .unreachable(let reason):
                engine.sendRevealWorkItemResponse(
                    requestId: requestId,
                    result: .failure(.internalFailure(reason))
                )
            }
        case .listHostedPanes:
            let panes = paneListHostedHandler?() ?? []
            engine.sendListHostedPanesResponse(requestId: requestId, panes: panes)
        }
    }

    private static let noPaneAllocatorReason = "no pane allocator wired into this build (Bazel without GhosttyKit)"

    // MARK: - Arm helpers

    /// Refetch everything a dropped/observed invalidation for `productID`
    /// could have covered. Shared by the topic-matched and payload-matched
    /// arms of `.workInvalidated`, which want identical refreshes.
    private func refetchForInvalidation(productID: String) {
        engine.sendGetWorkTree(productId: productID, flow: .invalidationRefetch)
        engine.sendListAttentionItemsForWorkItem(workItemID: productID)
        engine.sendListAttentionGroups(productId: productID)
        engine.sendListDeferredScopeAttentions(productId: productID)
        refreshPlannerRuns(forProductID: productID)
    }

    /// Clear the in-flight guard for a completed `release_project` /
    /// `unpopulate_project` request and pull the audit trail plus the
    /// board, both of which the action just rewrote.
    private func handlePlannerActionResult(projectID: String) {
        plannerActionInFlightProjectIDs.remove(projectID)
        engine.sendListPlannerRuns(projectId: projectID)
        if let productID = project(withID: projectID)?.productID {
            engine.sendGetWorkTree(productId: productID, flow: .itemRefetch)
        }
    }

    /// Build the terminal window's content payload, resolving the task's
    /// display name/short id if the work tree has it loaded.
    private func reviewTerminalContent(
        workItemID: String,
        workspacePath: String,
        leaseID: String?
    ) -> ReviewTerminalContent {
        let resolved = task(withID: workItemID)
        return ReviewTerminalContent(
            workItemID: workItemID,
            workspacePath: workspacePath,
            leaseID: leaseID,
            taskName: resolved?.name,
            taskShortID: resolved?.shortID
        )
    }

    private func upsertAutomation(_ automation: AppAutomation) {
        let productID = automation.productID
        if var list = automationsByProductID[productID] {
            if let idx = list.firstIndex(where: { $0.id == automation.id }) {
                list[idx] = automation
            } else {
                list.append(automation)
            }
            automationsByProductID[productID] = list
        } else {
            automationsByProductID[productID] = [automation]
        }
    }

    /// Whether an `.error` message is a transport-level signal from
    /// `EngineClient` rather than a real engine-reported error.
    /// Transport errors are emitted on every reconnect attempt while
    /// the socket can't be opened, so they must not drive any modal
    /// UI — see the `.error` arm of `handle(_:)` for context.
    private static func isSocketTransportError(_ message: String) -> Bool {
        return message.hasPrefix("socket failed:")
            || message.hasPrefix("socket waiting:")
            || message.hasPrefix("socket send failed:")
            || message.hasPrefix("socket receive failed:")
    }

    /// Parses a `ci_remediations` row timestamp for the "ci auto-fixed"
    /// badge reconciliation above. The engine stamps plain
    /// `YYYY-MM-DDTHH:MM:SSZ`, but fractional seconds are accepted too in
    /// case a different surface ever feeds this. `ISO8601DateFormatter`'s
    /// `date(from:)` is documented thread-safe, so the formatters are
    /// shared.
    private static func parseCiRemediationTimestamp(_ string: String) -> Date? {
        for formatter in ciRemediationTimestampFormatters {
            if let date = formatter.date(from: string) { return date }
        }
        return nil
    }

    private nonisolated(unsafe) static let ciRemediationTimestampFormatters: [ISO8601DateFormatter] = {
        let plain = ISO8601DateFormatter()
        plain.formatOptions = [.withInternetDateTime]
        let fractional = ISO8601DateFormatter()
        fractional.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return [plain, fractional]
    }()

    // MARK: - Test entry point

    /// Test-only entry point that funnels a synthetic engine event
    /// through the same `handle` dispatch the live socket uses, so
    /// picker-side reactions (selection fallback, archived-product
    /// fan-out) can be asserted without booting a real engine.
    func applyEventForTest(_ event: EngineEvent) {
        handle(event)
    }
}
