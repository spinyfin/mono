import Foundation

/// Outcome of resolving a reveal target (`bossctl reveal` / attention-item
/// jumps) to the actual card that should be scrolled to and highlighted.
enum RevealCardResult: Equatable {
    /// A real, visible card exists with this id — either the requested
    /// item's own card, or (for a revision rolled up into its parent
    /// because it reached `in_review`/`done`) the parent's card.
    case revealed(cardID: String)
    /// The id isn't loaded locally yet — the common cross-product case,
    /// before the target product's work tree has been fetched into this
    /// session. `revealWorkCard` proceeds optimistically against the raw
    /// id; the `pendingRevealScrollID` handshake finishes the job once the
    /// tree arrives. Reachability genuinely can't be verified before then.
    case deferred
    /// The id resolves locally but has no reachable card anywhere on the
    /// board: it's deleted, or it's a rolled-up revision whose parent
    /// couldn't be resolved either.
    case unreachable(reason: String)
}

extension ChatViewModel {
    // MARK: Revisions

    /// Resolves the reveal target for `taskID`. Tasks, chores, projects, and
    /// products always host their own card. A revision that has reached
    /// `in_review` or `done` never gets a standalone card — `workItems(in:)`
    /// filters it out of the Review/Done columns, and it never had one in
    /// Backlog/Doing since its own status already moved it past those — it
    /// only ever surfaces as a rollup line on its PARENT's card (see the
    /// `inReviewRevisions` computation in `ContentView`). So the reveal
    /// target for such a revision is its parent's card, wherever that
    /// parent's own status happens to place it (Backlog included — a parent
    /// blocked for a non-review reason still renders there).
    func revealCardTarget(for taskID: String) -> RevealCardResult {
        guard let item = task(withID: taskID) else {
            return .deferred
        }
        guard item.deletedAt == nil else {
            return .unreachable(reason: "\(revealLabel(for: item)) is deleted")
        }
        guard item.kind == "revision", item.status == "in_review" || item.status == "done" else {
            return .revealed(cardID: item.id)
        }
        guard let parentID = item.parentTaskId, let parent = task(withID: parentID) else {
            return .unreachable(
                reason: "no visible card for \(revealLabel(for: item)): its parent (\(item.parentTaskId ?? "unknown")) could not be resolved"
            )
        }
        guard parent.deletedAt == nil else {
            return .unreachable(
                reason: "no visible card for \(revealLabel(for: item)): parent \(revealLabel(for: parent)) is deleted"
            )
        }
        return .revealed(cardID: parent.id)
    }

    private func revealLabel(for task: WorkTask) -> String {
        task.shortID.map { "T\($0)" } ?? task.id
    }


    /// `kind == "revision"` tasks for parentID with status `"in_review"`.
    /// O(1) per card — reads the cache built by `rebuildRevisionCache()`,
    /// which groups every project- and product-level revision by parent id
    /// and status in a single pass. Called once per visible card on every
    /// kanban render, so this used to be an O(total tasks) scan per card;
    /// see `cachedInReviewRevisionsByParentID`.
    func inReviewRevisions(forParentTaskID parentID: String) -> [WorkTask] {
        if cachedInReviewRevisionsByParentID == nil { rebuildRevisionCache() }
        return cachedInReviewRevisionsByParentID?[parentID] ?? []
    }

    /// `kind == "revision"` tasks for parentID with status `"done"`. O(1)
    /// per card; see `inReviewRevisions(forParentTaskID:)`.
    func doneRevisions(forParentTaskID parentID: String) -> [WorkTask] {
        if cachedDoneRevisionsByParentID == nil { rebuildRevisionCache() }
        return cachedDoneRevisionsByParentID?[parentID] ?? []
    }

    /// `kind == "revision"` tasks for parentID regardless of status. Used
    /// only by the transcript viewer's one-shot chain lookup (not a
    /// per-render hot path), so it keeps the direct scan rather than
    /// growing the cache to cover the chore/product-task buckets too.
    func allRevisions(forParentTaskID parentID: String) -> [WorkTask] {
        revisions(forParentTaskID: parentID, status: nil, includeChoresAndProductTasks: true)
    }

    // Shared impl: project-task-parented revisions live under their project; chore-
    // parented ones live in the product-level bucket (issue #789). Pass
    // includeChoresAndProductTasks=true to also search choresByProductID and
    // productLevelTasksByProductID (needed for allRevisions).
    private func revisions(
        forParentTaskID parentID: String,
        status: String?,
        includeChoresAndProductTasks: Bool
    ) -> [WorkTask] {
        let matches: (WorkTask) -> Bool = { task in
            task.kind == "revision"
                && task.parentTaskId == parentID
                && (status == nil || task.status == status)
        }
        var result: [WorkTask] = []
        for tasks in tasksByProjectID.values {
            result.append(contentsOf: tasks.filter(matches))
        }
        if includeChoresAndProductTasks {
            for chores in choresByProductID.values {
                result.append(contentsOf: chores.filter(matches))
            }
        }
        for revisions in productLevelRevisionsByProductID.values {
            result.append(contentsOf: revisions.filter(matches))
        }
        if includeChoresAndProductTasks {
            for tasks in productLevelTasksByProductID.values {
                result.append(contentsOf: tasks.filter(matches))
            }
        }
        return result.sorted { ($0.revisionSeq ?? 0) < ($1.revisionSeq ?? 0) }
    }

    /// Groups every project- and product-level revision task by parent id
    /// for the two statuses the kanban card footer actually renders
    /// (`in_review`, `done`), in one O(total revisions) pass. Populates
    /// `cachedInReviewRevisionsByParentID` / `cachedDoneRevisionsByParentID`.
    /// Mirrors `rebuildPrereqCache` in ChatViewModel+Dependencies.swift —
    /// same buckets as the `revisions(...)` scan above with
    /// `includeChoresAndProductTasks: false` (chores/product-level tasks
    /// aren't revision parents, so they're excluded here too).
    private func rebuildRevisionCache() {
        var inReview: [String: [WorkTask]] = [:]
        var done: [String: [WorkTask]] = [:]
        func collect(_ tasks: [WorkTask]) {
            for task in tasks {
                guard task.kind == "revision", let parentID = task.parentTaskId else { continue }
                switch task.status {
                case "in_review":
                    inReview[parentID, default: []].append(task)
                case "done":
                    done[parentID, default: []].append(task)
                default:
                    break
                }
            }
        }
        for tasks in tasksByProjectID.values { collect(tasks) }
        for revisions in productLevelRevisionsByProductID.values { collect(revisions) }
        for key in inReview.keys {
            inReview[key]?.sort { ($0.revisionSeq ?? 0) < ($1.revisionSeq ?? 0) }
        }
        for key in done.keys {
            done[key]?.sort { ($0.revisionSeq ?? 0) < ($1.revisionSeq ?? 0) }
        }
        cachedInReviewRevisionsByParentID = inReview
        cachedDoneRevisionsByParentID = done
    }

    // MARK: Project / chore counts

    func projectName(for projectID: String?) -> String? {
        guard let projectID else { return nil }
        return project(withID: projectID)?.name
    }

    /// Project-badge text for a kanban card, or `nil` when the badge
    /// should be suppressed. Chores never have one; when the board is
    /// grouped by project the lane header already names the project,
    /// so the per-card badge would just duplicate the column header.
    func cardProjectBadge(for task: WorkTask) -> String? {
        if task.isChore { return nil }
        if workBoardGrouping == .project { return nil }
        return projectName(for: task.projectID)
    }

    /// Count of `todo` tasks for `projectID`. A `todo` task has no
    /// unsatisfied dependency gating it — if it did, the engine would
    /// have set `status = "blocked"`. These are tasks ready to dispatch.
    func unblockedTaskCount(forProjectID projectID: String) -> Int {
        (tasksByProjectID[projectID] ?? []).filter { $0.status == "todo" }.count
    }

    /// Count of dependency-blocked tasks for `projectID`. The engine
    /// sets `blocked_reason = "dependency"` when a task is gated by at
    /// least one unsatisfied prerequisite edge.
    func blockedTaskCount(forProjectID projectID: String) -> Int {
        (tasksByProjectID[projectID] ?? []).filter {
            $0.status == "blocked" && $0.blockedReason == "dependency"
        }.count
    }

    var unblockedChoreCount: Int {
        guard let productID = currentSelectedProductID else { return 0 }
        let chores = (choresByProductID[productID] ?? []).filter { $0.status == "todo" }
        let projectlessTasks = (productLevelTasksByProductID[productID] ?? []).filter { $0.status == "todo" }
        return chores.count + projectlessTasks.count
    }

    var blockedChoreCount: Int {
        guard let productID = currentSelectedProductID else { return 0 }
        let isBlocked: (WorkTask) -> Bool = { $0.status == "blocked" && $0.blockedReason == "dependency" }
        let chores = (choresByProductID[productID] ?? []).filter(isBlocked)
        let projectlessTasks = (productLevelTasksByProductID[productID] ?? []).filter(isBlocked)
        return chores.count + projectlessTasks.count
    }

    // MARK: Repo chip / recent repos

    /// Repo-chip mode for the kanban under the currently selected
    /// product. Drives both the product-header chip (single-repo) and
    /// the per-card chip (multi-repo) per design Q7. Computed off the
    /// *visible* work items so a project filter that hides the
    /// overriding card collapses the board back to single-repo
    /// presentation, matching the rule "every visible card resolves
    /// to the same URL".
    var workBoardRepoMode: WorkBoardRepoMode {
        guard let product = selectedProduct else { return .none }
        return WorkBoardRepoMode.compute(
            productRepoURL: product.repoRemoteURL,
            cards: visibleWorkItems
        )
    }

    /// Distinct repo URLs known under a product, ordered by recency
    /// of the work item they last appeared on. Drives both the Repo:
    /// row's `Change…` picker (per Follow-up chore #12) and the
    /// work-item create form's recent-repos picker (chore #10) so the
    /// two affordances agree on what counts as "recent". The product
    /// default is always first when set; the rest sort by the work
    /// item's `updatedAt` descending so the most-recently-edited
    /// repo bubbles up.
    ///
    /// Pure derivation over the in-memory snapshot — no RPC. Empty
    /// list is a legal answer (a brand-new product with no overrides
    /// and no default).
    func recentRepoURLs(forProduct productID: String) -> [String] {
        var seen = Set<String>()
        var ordered: [String] = []

        func push(_ value: String?) {
            guard let trimmed = nonEmptyTrim(value) else { return }
            if seen.insert(trimmed).inserted {
                ordered.append(trimmed)
            }
        }

        if let product = product(withID: productID) {
            push(product.repoRemoteURL)
        }

        var taskRows: [WorkTask] = []
        for project in projectsByProductID[productID] ?? [] {
            taskRows.append(contentsOf: tasksByProjectID[project.id] ?? [])
        }
        taskRows.append(contentsOf: choresByProductID[productID] ?? [])
        taskRows.append(contentsOf: productLevelTasksByProductID[productID] ?? [])
        let byRecency = taskRows.sorted { lhs, rhs in
            lhs.updatedAt > rhs.updatedAt
        }
        for task in byRecency {
            push(task.repoRemoteURL)
        }

        return ordered
    }

    /// Set or clear the per-work-item repo override. `url == nil` (or
    /// an empty/whitespace-only string) routes to the engine as
    /// `repo_remote_url = ""`, which is the patch shape that clears
    /// the column and falls back to product inheritance. No-ops when
    /// the new value equals the current one.
    func setRepoOverride(for taskID: String, to url: String?) {
        guard let task = task(withID: taskID) else { return }
        let trimmed = nonEmptyTrim(url) ?? ""
        let current = nonEmptyTrim(task.repoRemoteURL) ?? ""
        guard trimmed != current else { return }
        engine.sendUpdateWorkItem(id: task.id, patch: ["repo_remote_url": trimmed])
    }

    /// Repo-row presentation for the work-item detail popover. Wraps
    /// `RepoOverridePresentation.resolve` against the cached product
    /// row so the view stays a thin reflection of a value type.
    /// Returns `nil` only when the work item itself isn't loaded.
    func repoOverridePresentation(for task: WorkTask) -> RepoOverridePresentation {
        RepoOverridePresentation.resolve(
            task: task,
            product: product(withID: task.productID)
        )
    }

    private func nonEmptyTrim(_ value: String?) -> String? {
        guard let value else { return nil }
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }

    /// Per-card chip presentation, returning `nil` whenever the chip
    /// should not render: the board is in single-repo mode (chip
    /// already lives on the product header), or the card has no
    /// resolvable URL. Read by `WorkBoardCardView` to decide whether
    /// to draw the chip in the card header.
    func repoChip(for task: WorkTask) -> RepoChipPresentation? {
        switch workBoardRepoMode {
        case .singleRepo, .none:
            return nil
        case .multiRepo:
            let product = product(withID: task.productID)
            return RepoChipPresentation.forCard(
                task: task,
                productRepoURL: product?.repoRemoteURL
            )
        }
    }

    // MARK: Effective board column / active attempts

    /// The column that `task` renders into, overriding `task.boardColumn`
    /// only for an in-flight optimistic drag. Doing is reserved for rows
    /// whose OWN primary execution is active — a parent task blocked on a
    /// review-phase reason (`merge_conflict`, `ci_failure`,
    /// `ci_failure_exhausted`, `review_feedback`) stays in Review for the
    /// full revision lifecycle, whether the active worker is an operator-
    /// filed revision, an auto CI-fix, or an auto conflict-resolution
    /// attempt. That activity surfaces via `hasInProgressRevision` (the "in
    /// revision" badge) and the reason badge, not via column movement — see
    /// `activeConflictResolution(for:)` / `activeCiRemediation(for:)` for the
    /// badge-facing lookups.
    func effectiveBoardColumn(for task: WorkTask) -> WorkBoardColumnKey {
        // Optimistic override wins while a drag is in-flight.
        if let override = optimisticColumnByTaskID[task.id] {
            return override
        }
        return task.boardColumn
    }

    /// Effective board column based solely on real engine state, ignoring any
    /// in-flight optimistic override. Used during work-tree reconciliation to
    /// compare actual task state against the optimistic position.
    func realEffectiveBoardColumn(for task: WorkTask) -> WorkBoardColumnKey {
        task.boardColumn
    }

    /// The active conflict resolution for `taskID`, if any. A resolution
    /// is "active" when its status is `pending` or `running`. Returns
    /// `nil` when no such attempt exists.
    func activeConflictResolution(for taskID: String) -> WorkConflictResolution? {
        conflictResolutions.first {
            $0.workItemID == taskID && ($0.status == "pending" || $0.status == "running")
        }
    }

    /// The active CI remediation for `taskID`, if any. A remediation is
    /// "active" when its status is `pending` or `running`. Returns `nil`
    /// when no such attempt exists. Parallel to [[activeConflictResolution(for:)]].
    func activeCiRemediation(for taskID: String) -> WorkCiRemediation? {
        ciRemediations.first {
            $0.workItemID == taskID && ($0.status == "pending" || $0.status == "running")
        }
    }
}
