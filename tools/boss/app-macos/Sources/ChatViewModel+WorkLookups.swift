import Foundation

/// Selected-item accessors, visible-board computation, and id lookups.
extension ChatViewModel {
    // MARK: - Lookups and shared helpers

    var currentSelectedProductID: String? {
        selectedWorkProductID
    }

    var selectedProduct: WorkProduct? {
        guard let productID = currentSelectedProductID else { return nil }
        return product(withID: productID)
    }

    var selectedProject: WorkProject? {
        guard selectedProjectFilterIDs.count == 1,
              let projectID = selectedProjectFilterIDs.first else { return nil }
        return project(withID: projectID)
    }

    var projectFilterDescription: String {
        if filterToChoresOnly { return "No Project" }
        let visibleSelected = visibleSelectedProjectFilterIDs
        switch visibleSelected.count {
        case 0:
            return "All projects"
        case 1:
            if let id = visibleSelected.first, let project = self.project(withID: id) {
                return project.name
            }
            return "1 project"
        case let count:
            return "\(count) projects"
        }
    }

    var hasProjectFilters: Bool {
        !visibleSelectedProjectFilterIDs.isEmpty || filterToChoresOnly
    }

    /// Subset of `selectedProjectFilterIDs` whose projects are currently
    /// visible in the sidebar. When archived projects are hidden, their
    /// IDs may still be in the filter set (so toggling Show Archived
    /// back on restores the prior selection), but counts and badges
    /// must only reflect what the user can see.
    private var visibleSelectedProjectFilterIDs: Set<String> {
        guard !selectedProjectFilterIDs.isEmpty else { return [] }
        let visibleIDs = Set(projectsForSelectedProduct.map(\.id))
        return selectedProjectFilterIDs.intersection(visibleIDs)
    }

    var selectedTask: WorkTask? {
        guard let taskID = selectedWorkCardID else { return nil }
        return task(withID: taskID)
    }

    var projectsForSelectedProduct: [WorkProject] {
        let all = allProjectsForSelectedProduct
        guard !showArchivedProjects else { return all }
        return all.filter { $0.status != "archived" }
    }

    /// Unfiltered project list for the selected product, used by code
    /// paths that need full visibility regardless of the sidebar's
    /// Show Archived toggle (e.g. boss-agent context where the LLM
    /// must know archived projects exist so it doesn't recreate them).
    var allProjectsForSelectedProduct: [WorkProject] {
        guard let productID = currentSelectedProductID else { return [] }
        return (projectsByProductID[productID] ?? []).sorted(by: projectSort)
    }

    var visibleWorkItems: [WorkTask] {
        if let cached = cachedVisibleItems {
            return cached
        }
        let computed = computeVisibleWorkItems()
        cachedVisibleItems = computed
        return computed
    }

    /// Repo names (lowercased) that resolve to more than one org across
    /// the currently visible card set's PR URLs. Drives the board-local
    /// disambiguation rule for kanban PR-link labels: a repo name in
    /// this set must render as `org/repo#n`; everything else can drop
    /// to the bare `repo#n`. Cached on the same lifetime as
    /// [[visibleWorkItems]] — invalidated by [[invalidateWorkCache]].
    var ambiguousVisibleRepoNames: Set<String> {
        if let cached = cachedAmbiguousRepoNames {
            return cached
        }
        let computed = ambiguousPRRepoNames(in: visibleWorkItems)
        cachedAmbiguousRepoNames = computed
        return computed
    }

    /// The active board search query with surrounding whitespace removed,
    /// or `nil` when no search filter is in effect. Single source of truth
    /// for both the filter logic below and the persistent "filtered view"
    /// banner so the two can never disagree about whether the board is
    /// showing a subset (issue #1248).
    var activeWorkSearchQuery: String? {
        let query = workSearchText.trimmingCharacters(in: .whitespacesAndNewlines)
        return query.isEmpty ? nil : query
    }

    /// True while a free-text search is hiding non-matching cards. Drives
    /// the kanban filter banner so a stale search can't be mistaken for an
    /// empty or complete board.
    var isWorkSearchActive: Bool { activeWorkSearchQuery != nil }

    private func computeVisibleWorkItems() -> [WorkTask] {
        guard let productID = currentSelectedProductID else { return [] }

        let query = workSearchText.trimmingCharacters(in: .whitespacesAndNewlines)

        var items: [WorkTask] = []
        if filterToChoresOnly {
            items.append(contentsOf: (choresByProductID[productID] ?? []).sorted(by: taskSort))
            items.append(contentsOf: (productLevelTasksByProductID[productID] ?? []).sorted(by: taskSort))
            items.append(contentsOf: (productLevelRevisionsByProductID[productID] ?? []).sorted(by: taskSort))
        } else {
            let projectFilter = visibleSelectedProjectFilterIDs
            for project in projectsForSelectedProduct {
                guard projectFilter.isEmpty || projectFilter.contains(project.id) else { continue }
                items.append(contentsOf: (tasksByProjectID[project.id] ?? []).sorted(by: taskSort))
            }
            // Product-level work items (investigations, etc.) have no project, so a
            // project filter legitimately excludes them; otherwise they always
            // render. They are first-class work — not gated by the chores toggle,
            // which would otherwise hide an investigation a live worker is
            // producing against (issue #886).
            if projectFilter.isEmpty {
                items.append(contentsOf: (productLevelTasksByProductID[productID] ?? []).sorted(by: taskSort))
            }
            if includeChores && projectFilter.isEmpty {
                items.append(contentsOf: (choresByProductID[productID] ?? []).sorted(by: taskSort))
                // Chore-parented revisions have no project of their own; surface
                // them with the chores so their Backlog/Doing cards appear. The
                // in-review ones are rolled up under the parent and filtered out
                // of the Review column by `workItems(in:)`.
                items.append(contentsOf: (productLevelRevisionsByProductID[productID] ?? []).sorted(by: taskSort))
            }
        }

        // Automation-sourced chores are real work items that need human review.
        // They appear on the kanban like any other chore — the card detail view
        // marks them with a purple wand icon to indicate automation provenance.
        // Do NOT filter them out here: a chore in in_review status needs to
        // be visible so the operator can review and merge the PR.

        if showBlockedOnly {
            items = items.filter { $0.status == "blocked" }
        }

        guard !query.isEmpty else {
            return items
        }

        let matched = items.filter { item in
            item.name.localizedCaseInsensitiveContains(query)
                || item.description.localizedCaseInsensitiveContains(query)
                || (item.prURL?.localizedCaseInsensitiveContains(query) ?? false)
                || (projectName(for: item.projectID)?.localizedCaseInsensitiveContains(query) ?? false)
                || item.status.localizedCaseInsensitiveContains(query)
                || (item.shortID.map { "T\($0)" }?.localizedCaseInsensitiveContains(query) ?? false)
        }

        // A bare or "T"-prefixed number (e.g. "2801", or a leading T/t plus
        // digits) is a short id lookup. The substring match above already surfaces ids
        // containing that number anywhere (prefix search), but the id it
        // names exactly should always be the one the user actually finds —
        // pull it to the front rather than leaving it wherever taskSort put it.
        guard let exactShortID = Self.parseShortIDQuery(query) else {
            return matched
        }

        // A short-id match must surface the whole revision chain that id
        // belongs to — matching a parent also matches every revision
        // descended from it, and matching a revision also matches its
        // chain root and siblings — since a revision's own name/description
        // won't otherwise contain the parent's short id text.
        var resultIDs = Set(matched.map(\.id))
        var result = matched
        if let seed = items.first(where: { $0.shortID == exactShortID }) {
            let chainIDs = chainMemberIDs(containing: seed)
            for item in items where chainIDs.contains(item.id) && resultIDs.insert(item.id).inserted {
                result.append(item)
            }
        }

        var exact: [WorkTask] = []
        var rest: [WorkTask] = []
        for item in result {
            if item.shortID == exactShortID {
                exact.append(item)
            } else {
                rest.append(item)
            }
        }
        return exact + rest
    }

    /// Parses a search query as a short-id lookup: bare digits ("2801") or a
    /// case-insensitive "T"-prefixed number. Returns `nil`
    /// for anything else so plain text search is unaffected.
    static func parseShortIDQuery(_ query: String) -> Int? {
        var digits = Substring(query)
        if let first = digits.first, first == "T" || first == "t" {
            digits = digits.dropFirst()
        }
        guard !digits.isEmpty, digits.allSatisfy({ $0.isNumber }) else { return nil }
        return Int(digits)
    }

    func workTopic(forProductID productID: String) -> String {
        "work.product.\(productID)"
    }

    private var desiredWorkTopics: Set<String> {
        // `github.auth` is a global (per-host, not per-product) topic
        // carrying GitHub OAuth auth-state pushes; the engine fans every
        // device-flow transition out on it. We stay subscribed for the
        // whole session so the "GitHub account" settings subsection
        // re-renders live (OAuth device-flow design §4, TOPIC_GITHUB_AUTH).
        // `engine.health` carries health-state changes (dispatch pause/resume,
        // etc.) so the banner updates live without polling or restarting.
        var topics: Set<String> = ["work.products", "worker.live_states", "github.auth", "engine.health"]
        if let productID = currentSelectedProductID {
            topics.insert(workTopic(forProductID: productID))
        }
        return topics
    }

    func refreshWorkSubscriptions() {
        guard isConnected else { return }
        let desired = desiredWorkTopics
        let toSubscribe = desired.subtracting(subscribedWorkTopics)
        let toUnsubscribe = subscribedWorkTopics.subtracting(desired)

        if !toUnsubscribe.isEmpty {
            engine.sendUnsubscribe(topics: Array(toUnsubscribe).sorted())
        }
        if !toSubscribe.isEmpty {
            engine.sendSubscribe(topics: Array(toSubscribe).sorted())
        }

        subscribedWorkTopics = desired
    }

    func appendSystemMessage(_ text: String, alwaysShow: Bool = false) {
        guard alwaysShow || showSystemMessages else { return }
        // `write(contentsOf:)`, not the exception-raising `write(_:)` — see
        // [[DiagnosticWrite]]. A closed/broken stderr must never abort the app.
        try? FileHandle.standardError.write(contentsOf: Data("\(text)\n".utf8))
    }

    /// Non-private: [[ChatViewModel+BoardHelpers.swift]] resolves a task's
    /// owning product across several repo/badge helpers.
    func product(withID id: String) -> WorkProduct? {
        products.first { $0.id == id }
    }

    /// Lookup a project row by id across every product the model has
    /// loaded. Non-private so view code (the kanban project-card
    /// affordance) can resolve a section's `projectID` to a full
    /// `WorkProject` without re-walking the projects map itself.
    func project(withID id: String) -> WorkProject? {
        for projects in projectsByProductID.values {
            if let project = projects.first(where: { $0.id == id }) {
                return project
            }
        }
        return nil
    }

    func task(withID id: String) -> WorkTask? {
        if taskIndexByID == nil { rebuildTaskIndex() }
        return taskIndexByID?[id]
    }

    /// Look up any task or chore by id. Used by the kanban to resolve
    /// the parent task for revision card chrome.
    func workTask(withID id: String) -> WorkTask? {
        task(withID: id)
    }

    // MARK: - Live worker state

    /// Resolve a task to its current LiveWorkerState by joining
    /// `task → execution_id → run_id`. Returns `nil` when the task
    /// has no active execution or the engine has not yet seen any
    /// hook events for the run (so the live state map is empty).
    func workerLiveState(forTaskID taskID: String) -> WorkerLiveState? {
        guard let runtime = taskRuntimesByID[taskID],
              let executionID = runtime.executionID
        else {
            return nil
        }
        return liveWorkerStates.byRunID[executionID]
    }
}
