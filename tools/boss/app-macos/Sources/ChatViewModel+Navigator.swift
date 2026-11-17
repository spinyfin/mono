import Foundation

extension ChatViewModel {
    var navigatorScopeKey: String {
        ([selectedWorkProductID ?? ""] + visibleSelectedProjectFilterIDs.sorted()).joined(separator: "|")
            + (filterToChoresOnly ? "|unprojected" : "")
    }

    var navigatorTasks: [WorkTask] {
        guard let productID = selectedWorkProductID else { return [] }
        return tasksByProjectID.values.flatMap { $0 }.filter { $0.productID == productID }
            + (choresByProductID[productID] ?? [])
            + (productLevelTasksByProductID[productID] ?? [])
            + (productLevelRevisionsByProductID[productID] ?? [])
    }

    func navigatorIncludes(_ task: WorkTask) -> Bool {
        task.productID == selectedWorkProductID
            && (!filterToChoresOnly || task.projectID == nil)
            && (visibleSelectedProjectFilterIDs.isEmpty || task.projectID.map(visibleSelectedProjectFilterIDs.contains) == true)
    }

    var navigatorCompatibilityMessage: String? {
        if let cachedNavigatorCompatibility { return cachedNavigatorCompatibility.message }
        let message = computeNavigatorCompatibilityMessage()
        cachedNavigatorCompatibility = (message, true)
        return message
    }

    private func computeNavigatorCompatibilityMessage() -> String? {
        let population = navigatorTasks.filter { $0.deletedAt == nil && $0.status != "archived" }
        guard population.allSatisfy({ task in
            guard let projection = task.navigator else {
                // Excluded families deliberately have no projection. Revisions
                // never manufacture rows or resolve ancestry in the client.
                return task.sourceAutomationId != nil || task.kind == "revision"
            }
            guard projection.version == 1 else { return false }
            return projection.rootTaskId != task.id || projection.root?.knownSection != nil
        }) else {
            return "Navigator is unavailable for this engine projection. Use Kanban or update the engine and app."
        }
        return nil
    }

    var navigatorSections: [NavigatorSectionRows] {
        if let cachedNavigatorSections { return cachedNavigatorSections }
        let tasks = navigatorTasks
        let roots = tasks.filter { navigatorIncludes($0) && $0.deletedAt == nil && $0.status != "archived" }
            .compactMap { task -> NavigatorRow? in
                guard let projection = task.navigator, projection.version == 1,
                      projection.rootTaskId == task.id, let root = projection.root,
                      root.knownSection != nil else { return nil }
                return NavigatorRow(task: task, projection: root, matchingAskID: nil)
            }
            .sorted {
                if $0.projection.sortKey == $1.projection.sortKey { return $0.id < $1.id }
                return $0.projection.sortKey.utf8.lexicographicallyPrecedes($1.projection.sortKey.utf8)
            }
        let query = activeWorkSearchQuery
        let matchingAsks = tasks.filter { task in
            guard let query else { return false }
            return task.name.localizedCaseInsensitiveContains(query)
                || task.description.localizedCaseInsensitiveContains(query)
        }.sorted { $0.id < $1.id }
        let asksByRoot = Dictionary(grouping: matchingAsks, by: { $0.navigator?.rootTaskId ?? $0.id })
        let sections = NavigatorSection.allCases.map { section in
            let all = roots.filter { $0.projection.knownSection == section }
            let shown = all.compactMap { row -> NavigatorRow? in
                guard let query else { return row }
                if row.task.name.localizedCaseInsensitiveContains(query)
                    || row.task.description.localizedCaseInsensitiveContains(query) { return row }
                guard let ask = asksByRoot[row.id]?.first else { return nil }
                return NavigatorRow(task: row.task, projection: row.projection, matchingAskID: ask.id)
            }
            return NavigatorSectionRows(section: section, rows: shown, total: all.count)
        }
        cachedNavigatorSections = sections
        return sections
    }

    var navigatorCollapsed: Set<NavigatorSection> {
        navigator.collapsedByScope[navigatorScopeKey] ?? [.landed]
    }

    var navigatorVisibleRows: [NavigatorRow] {
        navigatorSections.filter { !navigatorCollapsed.contains($0.section) }.flatMap(\.rows)
    }

    var navigatorSelectedTask: WorkTask? {
        navigator.selectedRootID.flatMap { task(withID: $0) } ?? navigator.retainedTask
    }

    var navigatorSelectionNotice: String? {
        guard let id = navigator.selectedRootID, let selected = navigatorSelectedTask else { return nil }
        guard task(withID: id) != nil, selected.deletedAt == nil, selected.status != "archived" else {
            return "This task was archived or removed. Showing the last available detail."
        }
        if !navigatorSections.flatMap(\.rows).contains(where: { $0.id == id }) {
            return "This task is hidden by the current search."
        }
        if let section = selected.navigator?.root?.knownSection,
           section != navigator.selectedAtSection || navigatorCollapsed.contains(section) {
            return "Moved to \(section.title)."
        }
        return nil
    }

    func selectNavigatorRoot(_ taskID: String, askID: String? = nil) {
        guard let task = task(withID: taskID), let projection = task.navigator, projection.version == 1,
              let root = self.task(withID: projection.rootTaskId), let detail = root.navigator?.root else {
            navigator.selectedRootID = nil
            navigator.retainedTask = nil
            if navigator.mode == .navigator { selectionHistory.rememberRoot(nil, scope: navigatorScopeKey) }
            return
        }
        navigator.askID = detail.revisions.currentTaskId
        if navigator.selectedRootID != root.id {
            navigator.selectedRootID = root.id
            navigator.selectedAtSection = detail.knownSection
            navigator.tab = NavigatorDetailSlot(rawValue: detail.defaultDetail) ?? .activity
            navigator.tabs = [.activity, .history, navigator.tab]
        }
        if let historicalID = askID ?? (task.id != root.id ? task.id : nil) {
            navigator.historicalDocumentByRoot[root.id] = historicalID
            navigator.documentByRoot[root.id] = historicalID == detail.revisions.currentTaskId ? .currentAsk : .historical
        }
        navigator.retainedTask = root
        retainNavigatorActivity(for: root)
        if root.prURL != nil { navigator.tabs.insert(.review) }
        syncNavigatorAttention(for: root)
        if navigator.mode == .navigator { selectionHistory.rememberRoot(root.id, scope: navigatorScopeKey) }
    }

    /// Attention tab presence and selection follow the resolved entry list,
    /// never the raw engine actions. An intervention-only or unresolved
    /// question projection must not leave an empty Attention slot selected.
    func syncNavigatorAttention(for root: WorkTask? = nil) {
        let root = root ?? navigatorSelectedTask
        guard let root else { return }
        let hasAttention = !WorkNavigatorAttention.entries(model: self, root: root).isEmpty
        if hasAttention {
            navigator.tabs.insert(.attention)
        } else {
            navigator.tabs.remove(.attention)
            if navigator.tab == .attention {
                navigator.tab = .activity
            }
        }
    }

    func reconcileNavigatorSelection() {
        defer {
            if navigator.mode == .navigator, selectedWorkCardID != navigator.selectedRootID {
                // Align the shared card id even when the root has left the
                // store. selectedWorkCardID's didSet skips selectNavigatorRoot
                // for a missing task, so a retained/removed root is not
                // re-resolved and wiped.
                selectedWorkCardID = navigator.selectedRootID
            }
        }
        if let selected = navigatorSelectedTask, !navigatorIncludes(selected) {
            navigator.selectedRootID = nil
            navigator.retainedTask = nil
        }
        if let id = navigator.selectedRootID, let selected = task(withID: id) {
            navigator.retainedTask = selected
            if selected.prURL != nil { navigator.tabs.insert(.review) }
            syncNavigatorAttention(for: selected)
            if let current = selected.navigator?.root?.revisions.currentTaskId {
                if task(withID: current) != nil {
                    navigator.askID = current
                } else if navigator.askID.flatMap({ task(withID: $0) }) == nil {
                    navigator.askID = nil
                }
                // Keep the previous ask while a non-nil current revision
                // has not loaded into the store yet.
            } else {
                retireNavigatorAsk(rootID: selected.id)
                navigator.askID = nil
            }
            retainNavigatorActivity(for: selected)
        } else if navigator.selectedRootID == nil,
                  let id = selectionHistory.root(scope: navigatorScopeKey),
                  let task = task(withID: id), navigatorIncludes(task) {
            selectNavigatorRoot(id)
        }
    }

    /// Called around explicit scope changes, never background row movement.
    func changeNavigatorScope(_ change: () -> Void) {
        let previous = navigator.selectedRootID
        change()
        guard navigator.mode == .navigator else { return }
        if let previous, let task = task(withID: previous), navigatorIncludes(task) {
            selectNavigatorRoot(previous)
            selectedWorkCardID = previous
        } else {
            navigator.selectedRootID = nil
            navigator.retainedTask = nil
            reconcileNavigatorSelection()
            selectedWorkCardID = navigator.selectedRootID
        }
    }

    func setWorkViewMode(_ mode: WorkViewMode) {
        guard navigator.mode != mode else { return }
        if mode == .navigator {
            navigator.kanbanCardOnExit = selectedWorkCardID
            if let id = selectedWorkCardID, let task = task(withID: id), navigatorIncludes(task) {
                selectNavigatorRoot(id)
            } else {
                navigator.selectedRootID = nil
                navigator.retainedTask = nil
            }
        }
        navigator.mode = mode
        defaults.set(mode.rawValue, forKey: "boss.work.viewMode")
        if mode == .navigator {
            reconcileNavigatorSelection()
            if let id = navigator.selectedRootID {
                selectionHistory.rememberRoot(id, scope: navigatorScopeKey)
            }
        } else {
            revealKanbanSelectionAfterNavigator()
        }
    }

    /// Switch onto the Kanban card for the Navigator root, or the card that
    /// was selected on the last Kanban visit. A card already rendered under
    /// the current filters is selected and scrolled to in place: search,
    /// No-project scope, and Kanban toggles stay put, and the work tree is
    /// not refetched. Hidden cards still go through reveal so the filters
    /// that hide them can be cleared.
    private func revealKanbanSelectionAfterNavigator() {
        let target: WorkTask?
        if let root = navigatorSelectedTask {
            target = root
        } else if let id = navigator.kanbanCardOnExit, let task = task(withID: id), navigatorIncludes(task) {
            target = task
        } else {
            target = nil
        }
        guard let target else { return }
        if isKanbanCardRendered(target) {
            prepareReveal(target.id)
        } else {
            revealWorkCard(target.id, productID: target.productID)
        }
    }

    private func isKanbanCardRendered(_ task: WorkTask) -> Bool {
        guard isTaskVisible(task) else { return false }
        return WorkBoardColumnKey.allCases.contains { column in
            workSections(in: column).contains { $0.items.contains { $0.id == task.id } }
        }
    }

    func isWorkCardPopoverPresented(_ id: String) -> Bool {
        navigator.mode == .kanban && selectedTask?.id == id
    }

    func dismissWorkCardPopover() {
        guard navigator.mode == .kanban else { return }
        selectWorkCard(nil)
    }

    var navigatorProjectAttentionCount: Int {
        selectedProductOpenAttentionGroups.filter { group in
            guard let projectID = group.associationProjectID else { return false }
            return visibleSelectedProjectFilterIDs.isEmpty || visibleSelectedProjectFilterIDs.contains(projectID)
        }.count
    }

    func setNavigatorSection(_ section: NavigatorSection, expanded: Bool) {
        var collapsed = navigatorCollapsed
        if expanded { collapsed.remove(section) } else { collapsed.insert(section) }
        navigator.collapsedByScope[navigatorScopeKey] = collapsed
    }

    func revealNavigatorSelection() {
        workSearchText = ""
        guard let section = navigatorSelectedTask?.navigator?.root?.knownSection else { return }
        setNavigatorSection(section, expanded: true)
        navigator.selectedAtSection = section
        navigator.revealGeneration = UUID()
    }

    func confirmNavigatorReveal(rootID: String) {
        guard navigator.mode == .navigator, navigator.selectedRootID == rootID,
              navigatorRevealRootID == rootID, navigatorVisibleRows.contains(where: { $0.id == rootID }) else { return }
        finishReveal(.success)
    }

    func moveNavigatorSelection(by offset: Int, edge: Bool = false) {
        let rows = navigatorVisibleRows
        guard !rows.isEmpty else { return }
        let current = rows.firstIndex { $0.id == navigator.selectedRootID }
        let index = edge ? (offset < 0 ? 0 : rows.count - 1)
            : current.map { min(max($0 + offset, 0), rows.count - 1) } ?? (offset < 0 ? rows.count - 1 : 0)
        selectWorkCard(rows[index].id)
        selectNavigatorRoot(rows[index].id, askID: rows[index].matchingAskID)
        navigator.revealGeneration = UUID()
    }

    /// The engine dropped the current ask. A Current ask choice would now
    /// resolve to the original brief under the wrong title, so move it to the
    /// retired ask when it holds an open draft, otherwise back to Original.
    private func retireNavigatorAsk(rootID: String) {
        guard navigator.documentByRoot[rootID] == .currentAsk else { return }
        if let previous = navigator.askID, previous != rootID, task(withID: previous) != nil,
           navigatorBriefIsEditing(previous) {
            navigator.historicalDocumentByRoot[rootID] = previous
            navigator.documentByRoot[rootID] = .historical
        } else {
            navigator.documentByRoot[rootID] = .original
        }
    }

    /// The document the Brief shows. An explicit choice wins while it still
    /// resolves; otherwise a running revision defaults to its current ask,
    /// unless the original brief holds an open draft.
    func navigatorDocument(for rootID: String) -> NavigatorBriefDocument {
        let root = task(withID: rootID)
        let ask = root?.navigator?.root?.revisions.currentTaskId
            .flatMap { $0 != rootID ? task(withID: $0) : nil }
        switch navigator.documentByRoot[rootID] {
        case .original:
            return .original
        case .currentAsk:
            return ask != nil ? .currentAsk : .original
        case .historical:
            let found = navigator.historicalDocumentByRoot[rootID].flatMap { task(withID: $0) } != nil
            return found ? .historical : .original
        case nil:
            return ask != nil && !navigatorBriefIsEditing(rootID) ? .currentAsk : .original
        }
    }

    func setNavigatorDocument(_ document: NavigatorBriefDocument, rootID: String) {
        navigator.documentByRoot[rootID] = document
    }

    func navigatorDocumentTask(for root: WorkTask) -> WorkTask {
        switch navigatorDocument(for: root.id) {
        case .original:
            return root
        case .currentAsk:
            return root.navigator?.root?.revisions.currentTaskId.flatMap { task(withID: $0) } ?? root
        case .historical:
            return navigator.historicalDocumentByRoot[root.id].flatMap { task(withID: $0) } ?? root
        }
    }

    func setNavigatorActivity(executionID: String, rootID: String) {
        guard let activity = task(withID: rootID)?.navigator?.root?.activities.first(where: {
            $0.executionId == executionID
        }) else { return }
        navigator.activityByRoot[rootID] = activity
    }

    private func retainNavigatorActivity(for root: WorkTask) {
        if navigator.activityByRoot[root.id] == nil, let first = root.navigator?.root?.activities.first {
            navigator.activityByRoot[root.id] = first
        }
    }

    /// The displayed execution retains its full reference after it ends.
    func navigatorSelectedActivity(for root: WorkTask) -> NavigatorActivity? {
        navigator.activityByRoot[root.id] ?? root.navigator?.root?.activities.first
    }

    func navigatorFamily(of root: WorkTask) -> [WorkTask] {
        navigatorTasks.filter { $0.navigator?.rootTaskId == root.id || $0.id == root.id }
            .sorted {
                if $0.id == root.id { return true }
                if $1.id == root.id { return false }
                return $0.createdAt < $1.createdAt || ($0.createdAt == $1.createdAt && $0.id < $1.id)
            }
    }

    func navigateNavigatorDependency(_ row: WorkDependencyRow) {
        guard let task = task(withID: row.id) else { return }
        let targetID = task.navigator?.rootTaskId ?? task.id
        if let target = self.task(withID: targetID), navigatorIncludes(target),
           target.navigator?.root?.knownSection != nil {
            selectWorkCard(target.id)
            selectNavigatorRoot(target.id, askID: task.id != target.id ? task.id : nil)
        } else {
            revealWorkCard(task.id, productID: task.productID)
        }
    }
}
