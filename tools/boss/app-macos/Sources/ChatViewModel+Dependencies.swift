import Foundation

extension ChatViewModel {
    /// All prereqs per task, resolved to display rows. Read O(1) per card;
    /// rebuilt lazily on first read after invalidation (see `rebuildPrereqCache`).
    var dependencyPrereqsByTaskID: [String: [WorkDependencyRow]] {
        if cachedDependencyPrereqs == nil { rebuildPrereqCache() }
        return cachedDependencyPrereqs ?? [:]
    }

    /// Unsatisfied (still-gating) prereqs per task. Read O(1) per card.
    var gatingPrereqsByTaskID: [String: [WorkDependencyRow]] {
        if cachedGatingPrereqs == nil { rebuildPrereqCache() }
        return cachedGatingPrereqs ?? [:]
    }

    /// Build the O(1) id → work-item index from the four task buckets. First
    /// writer wins, preserving the bucket-search precedence the previous
    /// linear `task(withID:)` had (ids are unique across buckets, so this only
    /// matters defensively). Buckets, in order:
    /// - `tasksByProjectID` / `choresByProductID`: project-laned tasks/chores.
    /// - `productLevelRevisionsByProductID`: chore-parented revisions, which
    ///   live in neither bucket above (issue #789).
    /// - `productLevelTasksByProductID`: product-level investigations and any
    ///   other product-level kind (issue #886).
    func rebuildTaskIndex() {
        var index: [String: WorkTask] = [:]
        for tasks in tasksByProjectID.values {
            for task in tasks where index[task.id] == nil { index[task.id] = task }
        }
        for chores in choresByProductID.values {
            for chore in chores where index[chore.id] == nil { index[chore.id] = chore }
        }
        for revisions in productLevelRevisionsByProductID.values {
            for revision in revisions where index[revision.id] == nil { index[revision.id] = revision }
        }
        for tasks in productLevelTasksByProductID.values {
            for task in tasks where index[task.id] == nil { index[task.id] = task }
        }
        taskIndexByID = index
    }

    /// Resolve the human-readable label for the rows currently gating
    /// `task` — i.e. its incomplete `blocks` prerequisites. Used by
    /// the kanban card to show "Blocked by <prereq title>" under the
    /// task name when the engine has parked the row in `blocked`. The
    /// caller is expected to gate on `task.status == "blocked"` so we
    /// don't compute this for cards that aren't rendering the badge.
    func blockedByLabel(for task: WorkTask) -> String? {
        let edges = dependenciesByProductID[task.productID] ?? []
        guard !edges.isEmpty else { return nil }
        let names: [String] = edges.compactMap { edge in
            guard edge.dependentID == task.id, edge.relation == "blocks" else {
                return nil
            }
            guard let name = workItemName(for: edge.prerequisiteID),
                  !isWorkItemSatisfied(edge.prerequisiteID)
            else {
                return nil
            }
            return name
        }
        guard !names.isEmpty else { return nil }
        return names.joined(separator: ", ")
    }

    /// All `blocks` prereqs for `task` joined against the work tree,
    /// rendered in card-detail and tooltip order. Includes already-
    /// satisfied edges so the popover can show the full picture (the
    /// chain badge tooltip and the auto-block predicate filter further
    /// for "incomplete" only).
    func dependencyPrereqs(for taskID: String) -> [WorkDependencyRow] {
        guard let productID = task(withID: taskID)?.productID
            ?? project(withID: taskID)?.productID
        else {
            return []
        }
        let edges = dependenciesByProductID[productID] ?? []
        return edges
            .filter { $0.dependentID == taskID && $0.relation == "blocks" }
            .map { workDependencyRow(forID: $0.prerequisiteID) }
    }

    /// All `blocks` dependents of `taskID`. Used by the card detail
    /// Dependencies subsection to show "what does this gate?".
    func dependencyDependents(for taskID: String) -> [WorkDependencyRow] {
        guard let productID = task(withID: taskID)?.productID
            ?? project(withID: taskID)?.productID
        else {
            return []
        }
        let edges = dependenciesByProductID[productID] ?? []
        return edges
            .filter { $0.prerequisiteID == taskID && $0.relation == "blocks" }
            .map { workDependencyRow(forID: $0.dependentID) }
    }

    /// Subset of `dependencyPrereqs` that are still gating the row —
    /// i.e. not yet in a satisfied status. Drives the chain badge's
    /// hover tooltip ("gated by …") and the auto-block predicate.
    func gatingPrereqs(for taskID: String) -> [WorkDependencyRow] {
        dependencyPrereqs(for: taskID).filter { !isWorkItemSatisfied($0.id) }
    }

    /// True iff the engine parked the row in `blocked` (rather than the
    /// user choosing it). The chain badge appears only for these rows
    /// per design Q7 — manual blocks already get the lane and would
    /// double up with the icon.
    func isAutoBlocked(_ task: WorkTask) -> Bool {
        task.status == "blocked"
            && task.lastStatusActor == "engine"
            && !gatingPrereqs(for: task.id).isEmpty
    }

    /// True iff the row currently has at least one unsatisfied gating
    /// prereq. Drag refusal keys on this rather than `lastStatusActor`
    /// because the engine refuses *any* manual move out of `blocked`
    /// while gated, regardless of who set the status last (Q4).
    func hasGatingPrereqs(_ task: WorkTask) -> Bool {
        !gatingPrereqs(for: task.id).isEmpty
    }

    // MARK: - Dependency badge hover / frontier highlight

    /// The parent task ID when the currently selected card is a revision.
    /// Drives the green-border highlight on the parent card so clicking a
    /// revision lights up the task it amends — the inverse of the "In
    /// revision" badge hover which highlights child revisions.
    var selectedRevisionParentID: String? {
        guard let task = selectedTask, task.kind == "revision" else { return nil }
        return task.parentTaskId
    }

    /// Called when the pointer enters or leaves a Dependency badge on a
    /// kanban card. On enter, computes the actionable prerequisite
    /// frontier — the set of reachable, unblocked, open prerequisites —
    /// and publishes them so every frontier card gets a transient
    /// highlight. On leave (`nil`), clears the set.
    ///
    /// **The write is equality-gated, and that is load-bearing.** This
    /// is a hit-test callback: SwiftUI re-runs hover delivery at the end
    /// of every graph update whose layout moved anything under the
    /// pointer (`Update.dispatchActions()` →
    /// `EventBindingManager.enqueueHoverUpdateIfNeeded()`). An
    /// unconditional assignment to a `@Published` fires
    /// `objectWillChange` even when the value is identical, and
    /// `WorkBoardSectionItemsView` observes the whole `ChatViewModel`
    /// and reads this set — so one no-op hover tick re-evaluated the
    /// column, rebuilt every card's `WorkCardSnapshot`, and re-applied
    /// the entire `LazyVStack` list, which invalidated the responder
    /// tree and enqueued the next hover update. That is a closed
    /// re-entrant loop with no fixed point; see
    /// `tools/boss/docs/investigations/work-board-layout-livelock-2026-08-07.md`.
    func setDepBadgeHover(_ taskID: String?) {
        let next: Set<String> = taskID.map { actionablePrereqFrontier(for: $0) } ?? []
        guard next != depFrontierHighlightIDs else { return }
        depFrontierHighlightIDs = next
    }

    /// Walk `parentTaskId` links from `startID` through `kind == "revision"`
    /// rows to the first non-revision ancestor, mirroring the engine's
    /// `walk_to_root` in `attach_in_progress_revision_flag`
    /// (tools/boss/engine/core/src/work/revision_helpers.rs) — that function
    /// flags the CHAIN ROOT, not the direct parent, so a revision-of-revision
    /// must resolve the same way here. Returns `nil` if the chain is broken
    /// (missing ancestor) or exceeds the same 20-hop guard the engine uses.
    private func revisionChainRootID(for startID: String) -> String? {
        var currentID = startID
        for _ in 0..<20 {
            guard let current = task(withID: currentID) else { return nil }
            if current.kind != "revision" { return currentID }
            guard let parentID = current.parentTaskId else { return nil }
            currentID = parentID
        }
        return nil
    }

    /// All active (todo/active) revision tasks whose revision chain rolls up
    /// to `taskID` — i.e. `taskID` is the first non-revision ancestor reached
    /// by walking `parentTaskId` through `kind == "revision"` rows. Shared by
    /// the hover highlight and the click resolver below, which MUST agree on
    /// membership.
    func activeRevisions(forParentID taskID: String) -> [WorkTask] {
        let matches: (WorkTask) -> Bool = { [self] candidate in
            candidate.kind == "revision"
                && (candidate.status == "todo" || candidate.status == "active")
                && revisionChainRootID(for: candidate.id) == taskID
        }
        var candidates: [WorkTask] = []
        for tasks in tasksByProjectID.values {
            candidates.append(contentsOf: tasks.filter(matches))
        }
        for revisions in productLevelRevisionsByProductID.values {
            candidates.append(contentsOf: revisions.filter(matches))
        }
        return candidates
    }

    /// Called when the pointer enters or leaves an "In revision" badge on a
    /// kanban card. On enter, collects all active (todo/active) revision tasks
    /// whose chain rolls up to `taskID` (see `activeRevisions`) and highlights
    /// them with the same green-border overlay used by the dep frontier. On
    /// leave (`nil`), clears.
    ///
    /// Routed through per-task observable cells rather than an `@Published`
    /// property on `ChatViewModel`. A genuine transition therefore updates
    /// only the revision cards whose chrome changes. Repeated enter events
    /// for the same parent short-circuit on `lastRevisionHoverParentID`
    /// before scanning revisions; the store still equality-gates the set.
    func setRevisionBadgeHover(_ taskID: String?) {
        guard taskID != lastRevisionHoverParentID else { return }
        lastRevisionHoverParentID = taskID
        let next: Set<String> = taskID
            .map { Set(activeRevisions(forParentID: $0).map(\.id)) } ?? []
        revisionHighlightStore.setHighlightedIDs(next)
    }

    /// Stable keyed state observed by one mounted work-board card.
    func revisionHighlightState(for taskID: String) -> WorkBoardRevisionHighlightState {
        revisionHighlightStore.state(for: taskID)
    }

    /// The revision task the "In revision" badge should reveal when tapped:
    /// the most recently created (highest `revisionSeq`) active revision
    /// whose chain rolls up to `taskID` — same membership rule as
    /// `setRevisionBadgeHover`, since a task can have more than one open
    /// revision row in flight. `nil` when the badge's backing flag is stale
    /// and no such row currently resolves.
    func mostRecentActiveRevision(forParentID taskID: String) -> WorkTask? {
        activeRevisions(forParentID: taskID).max { ($0.revisionSeq ?? 0) < ($1.revisionSeq ?? 0) }
    }

    /// Transitively walks the prerequisite DAG from `taskID` and
    /// returns the IDs of every node that is:
    ///   - reachable (transitively reachable through `blocks` edges),
    ///   - unblocked (no incomplete prerequisites of its own), AND
    ///   - open (not in a terminal / satisfied status).
    ///
    /// These are the "next actionable" items: completing them advances
    /// the dependency frontier one step closer to unblocking the chore.
    /// Deeper nodes that are still blocked themselves are traversed but
    /// not added to the frontier (they aren't actionable yet); once they
    /// unblock, the frontier advances through them automatically on the
    /// next hover.
    func actionablePrereqFrontier(for taskID: String) -> Set<String> {
        guard let productID = task(withID: taskID)?.productID else { return [] }
        let edges = dependenciesByProductID[productID] ?? []

        var frontier: Set<String> = []
        var visited: Set<String> = [taskID]
        var queue: [String] = [taskID]

        while !queue.isEmpty {
            let current = queue.removeFirst()
            let prereqIDs = edges
                .filter { $0.dependentID == current && $0.relation == "blocks" }
                .map { $0.prerequisiteID }

            for prereqID in prereqIDs {
                guard !visited.contains(prereqID) else { continue }
                visited.insert(prereqID)

                // Skip already-satisfied (terminal) items — they aren't open.
                guard !isWorkItemSatisfied(prereqID) else { continue }

                // An unblocked, open item is exactly what "actionable" means.
                if gatingPrereqs(for: prereqID).isEmpty {
                    frontier.insert(prereqID)
                } else {
                    // Still blocked itself — keep walking its prerequisites
                    // so we can find the true frontier deeper in the DAG.
                    queue.append(prereqID)
                }
            }
        }

        return frontier
    }

    /// Rebuild the gating/dependency prereq caches from current edge and
    /// task/project data. Invoked lazily on first read after
    /// `invalidateWorkCache(.dependencies)` (or full invalidation) so a
    /// burst of engine events coalesces into one rebuild at the next render
    /// rather than one full graph walk per event. O(total edges) per call.
    func rebuildPrereqCache() {
        var gating: [String: [WorkDependencyRow]] = [:]
        var prereqs: [String: [WorkDependencyRow]] = [:]
        for edges in dependenciesByProductID.values {
            // Group block-edges by dependentID in one pass over this product's edges.
            var byDependent: [String: [WorkItemDependency]] = [:]
            for edge in edges where edge.relation == "blocks" {
                byDependent[edge.dependentID, default: []].append(edge)
            }
            for (taskID, taskEdges) in byDependent {
                let rows = taskEdges.map { workDependencyRow(forID: $0.prerequisiteID) }
                prereqs[taskID] = rows
                // Filter on the status already resolved into each row rather
                // than calling isWorkItemSatisfied(_:), which would look the
                // same id up a second time — halving the lookups per edge.
                gating[taskID] = rows.filter { !isWorkItemRowSatisfied($0) }
            }
        }
        cachedDependencyPrereqs = prereqs
        cachedGatingPrereqs = gating
    }

    /// Row-status equivalent of `isWorkItemSatisfied(_:)`: mirrors the
    /// engine's `status_satisfies` rule — every work item kind (task,
    /// chore, or project) is satisfied at `done` or `archived`. An
    /// unresolved prereq (kind `.unknown`, status `"unknown"`) is treated
    /// as unsatisfied, matching the id-based helper's nil-lookup behaviour.
    private func isWorkItemRowSatisfied(_ row: WorkDependencyRow) -> Bool {
        return row.status == "done" || row.status == "archived"
    }

    private func workDependencyRow(forID id: String) -> WorkDependencyRow {
        if id.hasPrefix("proj_") {
            if let project = project(withID: id) {
                return WorkDependencyRow(
                    id: project.id,
                    title: project.name,
                    status: project.status,
                    kind: .project
                )
            }
        } else if let task = task(withID: id) {
            return WorkDependencyRow(
                id: task.id,
                title: task.name,
                status: task.status,
                kind: task.isChore ? .chore : .task
            )
        }
        return WorkDependencyRow(id: id, title: id, status: "unknown", kind: .unknown)
    }

    private func workItemName(for id: String) -> String? {
        if id.hasPrefix("proj_") {
            return project(withID: id)?.name
        }
        return task(withID: id)?.name
    }

    /// Mirrors the engine's `status_satisfies` rule: every work item kind
    /// (task, chore, or project) is satisfied at `done` or `archived`.
    /// Used to hide already-finished prereqs from the "Blocked by …"
    /// label on the off-chance an edge survives a status change
    /// momentarily.
    private func isWorkItemSatisfied(_ id: String) -> Bool {
        if id.hasPrefix("proj_") {
            guard let status = project(withID: id)?.status else { return false }
            return status == "done" || status == "archived"
        }
        guard let status = task(withID: id)?.status else { return false }
        return status == "done" || status == "archived"
    }
}
