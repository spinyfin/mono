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
        if navigator.selectedRootID != root.id {
            navigator.selectedRootID = root.id
            navigator.selectedAtSection = detail.knownSection
            navigator.tab = NavigatorDetailSlot(rawValue: detail.defaultDetail) ?? .activity
            navigator.tabs = [.activity, .history, navigator.tab]
            navigator.askID = askID ?? (task.id != root.id ? task.id : detail.revisions.currentTaskId)
        } else if let askID {
            navigator.askID = askID
        }
        navigator.retainedTask = root
        if root.prURL != nil { navigator.tabs.insert(.review) }
        if WorkNavigatorAttention.entries(model: self, root: root).count > 0 { navigator.tabs.insert(.attention) }
        if navigator.mode == .navigator { selectionHistory.rememberRoot(root.id, scope: navigatorScopeKey) }
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
            if selected.navigator?.root?.actions.isEmpty == false { navigator.tabs.insert(.attention) }
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
}
