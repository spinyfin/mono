import Foundation

/// Navigation, product/project filters, reveal, and panel persistence.
extension ChatViewModel {
    func toggleBossPanelCollapsed() {
        isBossPanelCollapsed.toggle()
        defaults.set(isBossPanelCollapsed, forKey: bossPanelCollapsedDefaultsKey)
    }

    func setBossPanelWidth(_ width: CGFloat) {
        bossPanelWidth = width
        defaults.set(width, forKey: bossPanelWidthDefaultsKey)
    }

    /// Run-keyed focus shared by `bossctl agents focus` and Doing-card clicks:
    /// validate and reveal the run, then switch to Agents so the pending
    /// responder request can focus the mounted terminal or card.
    @discardableResult
    func focusAgentRun(_ runId: String) -> EngineFocusResult {
        guard let handler = paneFocusHandler else {
            return .failure(.internalFailure(Self.noPaneAllocatorReason))
        }
        let result = handler(runId)
        if case .success = result { setNavigationMode(.agents) }
        return result
    }

    func focusAgentFromWorkCard(_ runId: String) {
        switch focusAgentRun(runId) {
        case .success:
            workErrorMessage = nil
        case .failure(.unknownSlot):
            workErrorMessage = "This agent is no longer available in this window."
        case .failure(.internalFailure(let reason)):
            workErrorMessage = "Could not show agent: \(reason)"
        }
    }

    func setNavigationMode(_ mode: NavigationMode, refreshWorkOnEntry: Bool = true) {
        // Instrument the tab switch so the pane-grid relayout it provokes
        // (how many panes rebuild, the settle wall-time, any unexpected
        // teardown) is measurable for the high-CPU investigation. See
        // [[TerminalLoopMonitor]].
        if navigationMode != mode {
            TerminalLoopMonitor.shared.noteTabSwitch(
                from: navigationMode.rawValue,
                to: mode.rawValue
            )
        }
        navigationMode = mode
        defaults.set(mode.rawValue, forKey: navigationModeDefaultsKey)
        if mode == .work && refreshWorkOnEntry {
            refreshWork()
        }
        if mode == .automations {
            refreshAutomations()
        }
    }

    func selectWorkProduct(_ productID: String) {
        let isAlreadyShowingProductBoard =
            selectedWorkProductID == productID
            && selectedProjectFilterIDs.isEmpty
            && selectedWorkCardID == nil
        guard !isAlreadyShowingProductBoard else { return }
        selectedWorkProductID = productID
        selectedProjectFilterIDs = []
        selectedWorkCardID = nil
        workErrorMessage = nil
        persistSelectedProductID(productID)
        persistProjectFilterIDs()
        refreshWorkSubscriptions()
        if isConnected {
            engine.sendGetWorkTree(productId: productID, flow: .productSwitch)
            engine.sendListDeferredScopeAttentions(productId: productID)
        }
    }

    func toggleProjectFilter(_ projectID: String) {
        if filterToChoresOnly {
            filterToChoresOnly = false
            defaults.set(false, forKey: filterToChoresOnlyDefaultsKey)
        }
        if selectedProjectFilterIDs.contains(projectID) {
            selectedProjectFilterIDs.remove(projectID)
        } else {
            selectedProjectFilterIDs.insert(projectID)
        }
        selectedWorkCardID = nil
        persistProjectFilterIDs()
    }

    func clearProjectFilters() {
        guard !selectedProjectFilterIDs.isEmpty || filterToChoresOnly else { return }
        selectedProjectFilterIDs = []
        filterToChoresOnly = false
        defaults.set(false, forKey: filterToChoresOnlyDefaultsKey)
        selectedWorkCardID = nil
        persistProjectFilterIDs()
    }

    func setFilterToChoresOnly(_ value: Bool) {
        guard filterToChoresOnly != value else { return }
        filterToChoresOnly = value
        defaults.set(value, forKey: filterToChoresOnlyDefaultsKey)
        if value {
            selectedProjectFilterIDs = []
            persistProjectFilterIDs()
        }
        selectedWorkCardID = nil
    }

    /// Navigator right-click → Archive. Sends the status patch and marks
    /// the project as archiving so the row reacts immediately; the engine's
    /// `work_item_updated` reply (see `handleUpdatedWorkItem`) applies the
    /// archived project in place and clears the pending state. A second
    /// click while the first is in flight is a no-op rather than a second
    /// RPC.
    func archiveProject(id: String) {
        guard !archivingProjectIDs.contains(id) else { return }
        archivingProjectIDs.insert(id)
        guard let requestID = engine.sendUpdateWorkItem(id: id, patch: ["status": "archived"]) else {
            // Not sent (socket down): the `.notConnected` event reports it;
            // there is nothing in flight to wait for.
            archivingProjectIDs.remove(id)
            return
        }
        archiveRequestProjectIDs[requestID] = id
    }

    /// Clear the pending-archive mark for `projectID` (and any request id
    /// that pointed at it). Called on the engine's reply, on an error for
    /// that request, and on paths where no reply can ever arrive.
    func settleArchiveRequest(projectID: String) {
        archivingProjectIDs.remove(projectID)
        archiveRequestProjectIDs = archiveRequestProjectIDs.filter { $0.value != projectID }
    }

    /// A `work_error` (or undecodable reply) carrying a pending archive's own
    /// request id belongs to that archive alone: drop its pending mark and
    /// surface the message, without touching any other in-flight state.
    /// Returns whether the request id matched an archive.
    func settleArchiveWorkError(message: String, requestId: String?) -> Bool {
        guard let requestId, let projectID = archiveRequestProjectIDs[requestId] else { return false }
        abandonBackgroundWorkRequest(requestId: requestId)
        settleArchiveRequest(projectID: projectID)
        workErrorMessage = message
        return true
    }

    /// Clear every pending archive: used when the link drops or an error
    /// arrives that cannot be attributed to one request.
    func settleAllArchiveRequests() {
        guard !archivingProjectIDs.isEmpty || !archiveRequestProjectIDs.isEmpty else { return }
        archivingProjectIDs.removeAll()
        archiveRequestProjectIDs.removeAll()
    }

    func setIncludeChores(_ value: Bool) {
        guard includeChores != value else { return }
        includeChores = value
        defaults.set(value, forKey: includeChoresDefaultsKey)
    }

    func setShowBlockedOnly(_ value: Bool) {
        guard showBlockedOnly != value else { return }
        showBlockedOnly = value
        defaults.set(value, forKey: showBlockedOnlyDefaultsKey)
    }

    func setShowArchivedProjects(_ value: Bool) {
        guard showArchivedProjects != value else { return }
        showArchivedProjects = value
        defaults.set(value, forKey: showArchivedProjectsDefaultsKey)
    }

    /// Persist (or clear, when `nil`) the selected product so the next
    /// launch restores the board the operator left open.
    func persistSelectedProductID(_ productID: String?) {
        if let productID {
            defaults.set(productID, forKey: selectedWorkProductDefaultsKey)
        } else {
            defaults.removeObject(forKey: selectedWorkProductDefaultsKey)
        }
    }

    /// Tell the engine which product the chooser is on, so a coordinator
    /// asking `bossctl selected-product` gets the product actually on
    /// screen instead of guessing one.
    ///
    /// Gated on `isAppSessionRegistered`, not merely `isConnected`: the
    /// engine only trusts this report from the registered app session,
    /// so a report sent before registration lands would be dropped. The
    /// `.appSessionRegistered` handler calls this once registration
    /// completes, which covers both cold start and reconnect.
    func reportSelectedProductToEngine() {
        guard isAppSessionRegistered else { return }
        engine.sendReportSelectedProduct(productId: selectedWorkProductID)
    }

    func persistProjectFilterIDs() {
        if selectedProjectFilterIDs.isEmpty {
            defaults.removeObject(forKey: selectedProjectFilterIDsDefaultsKey)
        } else {
            defaults.set(Array(selectedProjectFilterIDs).sorted(), forKey: selectedProjectFilterIDsDefaultsKey)
        }
    }

    func selectWorkCard(_ taskID: String?) {
        selectedWorkCardID = taskID
        guard let taskID, let task = task(withID: taskID) else { return }
        selectedWorkProductID = task.productID
    }

    /// One click on the findings badge does both jobs: open the findings
    /// brief, and reveal the card of the revision that will fix them to show
    /// where that work lives and its state.
    /// `revealWorkCard` targets the revision's own card whenever it has one
    /// (queued / active); only a revision that reached `in_review`/`done`
    /// is rolled up onto its parent's card, and
    /// `aiReviewFindingsTooltipNote(revisionID:)` says so on the badge.
    ///
    /// Without a fix task, open the persisted verdict supplied with the badge
    /// and leave the board's reveal state untouched.
    func openAIReviewFindings(revisionID: String?, findingsMarkdown: String? = nil) {
        guard let revisionID else {
            guard let findingsMarkdown else {
                workErrorMessage = "Couldn't open the review findings: the recorded findings are unavailable."
                return
            }
            openLoadedMarkdown(title: "AI review findings", markdown: findingsMarkdown)
            return
        }
        guard let revision = task(withID: revisionID) else {
            workErrorMessage = "Couldn't open the review findings: the revision is no longer available."
            return
        }
        openTaskDescription(revision)
        switch revealWorkCard(revision.id, productID: revision.productID) {
        case .revealed, .deferred:
            break
        case .unreachable(let reason):
            workErrorMessage = "Couldn't reveal the fix for the review findings: \(reason)"
        }
    }

    /// Extra badge-tooltip sentence describing where the click reveals the
    /// findings revision.
    func aiReviewFindingsTooltipNote(revisionID: String?) -> String {
        guard let revisionID else { return "No fix task yet." }
        guard let revision = task(withID: revisionID) else {
            return "Click to open the findings. The fix task is not loaded on this board."
        }
        switch revealCardTarget(for: revision.id) {
        case .revealed(let cardID) where cardID != revision.id:
            return "Click to open the findings. The fix is already in review or done, so it is shown as a line on this card rather than as its own card."
        default:
            return "Click to open the findings and reveal the fix task's card."
        }
    }

    /// Navigate the kanban to `taskID` and play a 1.5 s highlight.
    /// Switches to the Work tab, selects the task's product, clears
    /// every active board filter, and queues a scroll. If the task's
    /// product is not the one currently loaded, the scroll is deferred
    /// until the `workTree` event for that product arrives.
    ///
    /// Reveal's contract is "show me this card", so it must override any
    /// filter that would hide the target — a stale search query, a
    /// blocked-only / chores-only toggle, a project filter, or chores
    /// being hidden — all of which can exclude the card and make the
    /// scroll silently land on nothing (#1249). We reset the board to its
    /// unfiltered state before asking the view to expand and scroll.
    ///
    /// `taskID` itself is not always the card that gets scrolled to/
    /// highlighted — see `revealCardTarget(for:)`: a revision rolled up
    /// onto its parent's card redirects to the parent. The returned
    /// `RevealCardResult` describes target resolution only. The completion
    /// reports success only after the view confirms the card is visible.
    @discardableResult
    func revealWorkCard(
        _ taskID: String, productID: String,
        completion: ((EngineRevealResult) -> Void)? = nil
    ) -> RevealCardResult {
        finishReveal(.failure(.internalFailure("reveal superseded by a newer request")))
        revealGeneration = UUID()
        revealHighlightID = nil
        revealCompletion = completion
        revealProductID = productID
        // Reveal owns its refresh: entering Work must not fetch the old
        // product before selection, or refetch the target a second time.
        setNavigationMode(.work, refreshWorkOnEntry: false)
        clearWorkFiltersForReveal()
        let switching = currentSelectedProductID != productID
        if switching {
            selectWorkProduct(productID)
        } else {
            refreshWork()
        }
        let outcome: RevealCardResult
        if switching || task(withID: taskID) == nil {
            pendingRevealScrollID = taskID
            armRevealDeadline(taskID: taskID, waitingForTree: true)
            outcome = .deferred
        } else {
            outcome = prepareReveal(taskID)
        }
        return outcome
    }

    /// Reset every board filter that could hide a reveal target so the
    /// full work board for the product is shown. Each assignment is a
    /// no-op when the filter is already in its neutral state, so this is
    /// cheap to call unconditionally. Keep this in sync with
    /// `computeVisibleWorkItems` — any new narrowing filter added there
    /// must be neutralized here too, or reveal can silently fail again.
    private func clearWorkFiltersForReveal() {
        selectedProjectFilterIDs = []
        workSearchText = ""
        showBlockedOnly = false
        filterToChoresOnly = false
        includeChores = true
    }

    func setWorkBoardGrouping(_ grouping: WorkBoardGrouping) {
        workBoardGrouping = grouping
        defaults.set(grouping.rawValue, forKey: workBoardGroupingDefaultsKey)
    }

    func refreshWork() {
        guard isConnected else { return }
        engine.sendListProducts()
        if let productID = currentSelectedProductID {
            engine.sendGetWorkTree(productId: productID, flow: .manualRefresh)
        }
    }
}
