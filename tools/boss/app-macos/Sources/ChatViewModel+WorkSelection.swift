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

    func setNavigationMode(_ mode: NavigationMode) {
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
        if mode == .work {
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
            engine.sendListAttentionItemsForWorkItem(workItemID: productID)
            engine.sendListAttentionGroups(productId: productID)
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

    func archiveProject(id: String) {
        engine.sendUpdateWorkItem(id: id, patch: ["status": "archived"])
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

    func setReviewReadyOnly(_ value: Bool) {
        guard reviewReadyOnly != value else { return }
        reviewReadyOnly = value
        defaults.set(value, forKey: reviewReadyOnlyDefaultsKey)
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
    /// unfiltered state before scrolling so the revealed card is
    /// guaranteed visible.
    ///
    /// `taskID` itself is not always the card that gets scrolled to/
    /// highlighted — see `revealCardTarget(for:)`: a revision rolled up
    /// onto its parent's card redirects to the parent. The returned
    /// `RevealCardResult` tells the caller (the `reveal_work_item` IPC
    /// handler) whether a real card was reached, deferred pending a
    /// product-tree fetch, or unreachable — so it can answer bossctl
    /// truthfully instead of always claiming success.
    @discardableResult
    func revealWorkCard(_ taskID: String, productID: String) -> RevealCardResult {
        let outcome = revealCardTarget(for: taskID)
        let hostCardID: String
        switch outcome {
        case .revealed(let cardID):
            hostCardID = cardID
        case .deferred:
            hostCardID = taskID
        case .unreachable:
            return outcome
        }
        setNavigationMode(.work)
        clearWorkFiltersForReveal()
        selectedWorkCardID = hostCardID
        let isProductSwitch = currentSelectedProductID != productID
        if isProductSwitch {
            selectWorkProduct(productID)
            pendingRevealScrollID = hostCardID
        } else {
            triggerRevealScroll(hostCardID)
        }
        revealHighlightID = hostCardID
        let capturedID = hostCardID
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in
            if self?.revealHighlightID == capturedID {
                self?.revealHighlightID = nil
            }
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
        reviewReadyOnly = false
    }

    func triggerRevealScroll(_ taskID: String) {
        revealScrollTarget = taskID
        let capturedID = taskID
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in
            if self?.revealScrollTarget == capturedID {
                self?.revealScrollTarget = nil
            }
        }
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
