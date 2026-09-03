import Foundation

/// Work-item create/edit/delete, kanban moves, repos, and product tracker settings.
extension ChatViewModel {
    private var taskCreationProject: WorkProject? {
        if let selectedProject {
            return selectedProject
        }
        if let selectedTask, let projectID = selectedTask.projectID {
            return project(withID: projectID)
        }
        return nil
    }

    func presentCreateProduct() {
        pendingWorkCreateRequest = WorkCreateRequest(kind: .product)
    }

    func presentCreateProject() {
        guard let productID = currentSelectedProductID else { return }
        pendingWorkCreateRequest = WorkCreateRequest(kind: .project(productID: productID))
    }

    func presentCreateTask() {
        guard let project = taskCreationProject else { return }
        pendingWorkCreateRequest = WorkCreateRequest(
            kind: .task(productID: project.productID, projectID: project.id)
        )
    }

    func presentCreateChore() {
        guard let productID = currentSelectedProductID else { return }
        pendingWorkCreateRequest = WorkCreateRequest(kind: .chore(productID: productID))
    }

    func dismissWorkCreateRequest() {
        pendingWorkCreateRequest = nil
    }

    func presentEditSelectedWorkItem() {
        if let task = selectedTask {
            pendingWorkEditRequest = WorkEditRequest(item: task.isChore ? .chore(task) : .task(task))
        } else if let project = selectedProject {
            pendingWorkEditRequest = WorkEditRequest(item: .project(project))
        } else if let product = selectedProduct {
            pendingWorkEditRequest = WorkEditRequest(item: .product(product))
        }
    }

    func presentEditSelectedProduct() {
        guard let product = selectedProduct else { return }
        pendingWorkEditRequest = WorkEditRequest(item: .product(product))
    }

    func dismissWorkEditRequest() {
        pendingWorkEditRequest = nil
    }

    func submitWorkCreateRequest(
        _ request: WorkCreateRequest,
        name: String,
        description: String,
        repoRemoteURL: String = "",
        goal: String = "",
        setAsProductDefault: Bool = false
    ) {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedName.isEmpty else { return }

        workErrorMessage = nil
        let repoOverride = repoRemoteURL.trimmingCharacters(in: .whitespacesAndNewlines)
        switch request.kind {
        case .product:
            engine.sendCreateProduct(
                name: trimmedName,
                description: description,
                repoRemoteURL: repoRemoteURL
            )
        case .project(let productID):
            engine.sendCreateProject(
                productId: productID,
                name: trimmedName,
                description: description,
                goal: goal
            )
        case .task(let productID, let projectID):
            engine.sendCreateTask(
                productId: productID,
                projectId: projectID,
                name: trimmedName,
                description: description,
                repoRemoteURL: repoOverride.isEmpty ? nil : repoOverride
            )
            if setAsProductDefault && !repoOverride.isEmpty {
                engine.sendUpdateWorkItem(
                    id: productID,
                    patch: ["repo_remote_url": repoOverride]
                )
            }
        case .chore(let productID):
            engine.sendCreateChore(
                productId: productID,
                name: trimmedName,
                description: description,
                repoRemoteURL: repoOverride.isEmpty ? nil : repoOverride
            )
            if setAsProductDefault && !repoOverride.isEmpty {
                engine.sendUpdateWorkItem(
                    id: productID,
                    patch: ["repo_remote_url": repoOverride]
                )
            }
        }

        pendingWorkCreateRequest = nil
    }

    /// Empirical known-repo set for `productID`, mirroring the CLI's
    /// `known_repos_for_product` (multi-repo design Q4). Returns the
    /// distinct, non-empty `repo_remote_url` values across the
    /// product's tasks and chores, plus the product's own default if
    /// set. Sorted by short-name for stable picker ordering, with the
    /// product default first when present so the picker leads with
    /// the "obvious" choice.
    ///
    /// All inputs come from the work tree the model already has on
    /// hand; no engine RPC. Returns an empty array when the product
    /// is unknown.
    func knownReposForProduct(_ productID: String) -> [String] {
        guard products.contains(where: { $0.id == productID }) else {
            return []
        }
        var seen: Set<String> = []
        var result: [String] = []
        let productDefault = products
            .first(where: { $0.id == productID })?
            .repoRemoteURL?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if let productDefault, !productDefault.isEmpty {
            seen.insert(productDefault)
            result.append(productDefault)
        }
        var rest: [String] = []
        let projects = projectsByProductID[productID] ?? []
        for project in projects {
            for task in tasksByProjectID[project.id] ?? [] {
                if let url = task.repoRemoteURL?.trimmingCharacters(in: .whitespacesAndNewlines),
                   !url.isEmpty, !seen.contains(url) {
                    seen.insert(url)
                    rest.append(url)
                }
            }
        }
        for chore in choresByProductID[productID] ?? [] {
            if let url = chore.repoRemoteURL?.trimmingCharacters(in: .whitespacesAndNewlines),
               !url.isEmpty, !seen.contains(url) {
                seen.insert(url)
                rest.append(url)
            }
        }
        rest.sort { shortRepoName(for: $0) < shortRepoName(for: $1) }
        result.append(contentsOf: rest)
        return result
    }

    /// Product default repo URL, looked up by id. Used by
    /// `WorkCreateSheet` to construct a `WorkCreateRepoFormState`
    /// without reaching into `products` itself. `nil` for an unknown
    /// product or one whose URL is empty / whitespace.
    func productDefaultRepoURL(_ productID: String) -> String? {
        let raw = products.first(where: { $0.id == productID })?.repoRemoteURL
        let trimmed = raw?.trimmingCharacters(in: .whitespacesAndNewlines)
        if let trimmed, !trimmed.isEmpty { return trimmed }
        return nil
    }

    func submitWorkEditRequest(
        _ request: WorkEditRequest,
        name: String,
        description: String,
        status: String,
        repoRemoteURL: String = "",
        goal: String = "",
        priority: String = "",
        prURL: String = "",
        workerBranchPrefix: String = "",
        docsRepo: String = ""
    ) {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedName.isEmpty else { return }

        var patch: [String: Any] = [
            "name": trimmedName,
            "description": description,
            "status": status,
        ]

        let id: String
        switch request.item {
        case .product(let product):
            id = product.id
            patch["repo_remote_url"] = repoRemoteURL
            patch["worker_branch_prefix"] = workerBranchPrefix
            patch["docs_repo"] = docsRepo
        case .project(let project):
            id = project.id
            patch["goal"] = goal
            patch["priority"] = priority
        case .task(let task), .chore(let task):
            id = task.id
            patch["pr_url"] = prURL
            // Only send a priority patch when the user actually
            // touched the picker — keeps unrelated edits from
            // bouncing the field through serde-validation noise.
            if !priority.isEmpty, priority != task.priority {
                patch["priority"] = priority
            }
        }

        engine.sendUpdateWorkItem(id: id, patch: patch)
        pendingWorkEditRequest = nil
    }

    func setProductExternalTracker(
        productId: String,
        kind: String,
        org: String,
        repo: String,
        projectNumber: Int,
        reverseClose: Bool
    ) {
        let config: [String: Any] = [
            "org": org,
            "repo": repo,
            "project_number": projectNumber,
            "reverse_close": reverseClose,
        ]
        engine.sendSetProductExternalTracker(productId: productId, kind: kind, config: config)
    }

    func unsetProductExternalTracker(productId: String) {
        engine.sendUnsetProductExternalTracker(productId: productId)
    }

    /// Set a product's merge mechanism (`"direct"` or `"trunk_queue"`) —
    /// how an approved merge on this product's PRs is executed.
    func setProductMergeMechanism(productId: String, mechanism: String) {
        engine.sendSetProductMergeMechanism(productId: productId, mechanism: mechanism)
    }

    /// Store the org-level Trunk API token (Settings pane's "Trunk" tab).
    func setTrunkToken(_ token: String) {
        engine.sendTrunkSetToken(token: token)
    }

    /// Re-query whether a Trunk API token is currently configured.
    func refreshTrunkStatus() {
        engine.sendTrunkStatus()
    }

    func deleteSelectedWorkItem() {
        guard let task = selectedTask else { return }
        engine.sendDeleteWorkItem(id: task.id)
    }

    func deleteWorkItem(id: String) {
        engine.sendDeleteWorkItem(id: id)
    }

    func moveSelectedTask(offset: Int) {
        guard let task = selectedTask,
              !task.isChore,
              let projectID = task.projectID,
              var tasks = tasksByProjectID[projectID]?.sorted(by: taskSort),
              let currentIndex = tasks.firstIndex(where: { $0.id == task.id })
        else {
            return
        }

        let destination = currentIndex + offset
        guard tasks.indices.contains(destination) else { return }

        tasks.swapAt(currentIndex, destination)
        engine.sendReorderProjectTasks(projectId: projectID, taskIds: tasks.map(\.id))
    }

    /// Apply an explicit column choice from the work-card popover's "Move"
    /// buttons. The drag path does not come through here — it reports its
    /// drop target and the engine resolves it
    /// (`sendMoveWorkItemOnBoard`). Two extra concerns vs. a pure status
    /// edit, both per `tools/boss/docs/designs/work-kanban.md`:
    ///
    /// - Choosing Doing (target status `active`) also fires
    ///   `RequestExecution` so the engine schedules a worker. The
    ///   engine is idempotent — a non-terminal execution already
    ///   running for this work item won't get a duplicate.
    /// - Move OUT of Doing while a live worker is attached is
    ///   blocked — except for two intentional gestures:
    ///   (a) Choosing Backlog (`todo`): engine stops the worker,
    ///       releases the lease, and parks the card — no autostart.
    ///   (b) Terminal transitions (`done`, `archived`): these mirror the
    ///       engine's own lifecycle resolutions and are always allowed.
    func moveTask(_ taskID: String, to column: WorkBoardColumnKey) {
        guard let task = task(withID: taskID) else { return }
        let targetStatus = column.targetStatus
        guard task.status != targetStatus else { return }

        if task.status == "active"
            && !Self.terminalKanbanStatuses.contains(targetStatus)
            && column != .backlog  // backlog drag = stop+park: engine handles teardown
            && hasLiveWorker(forTaskID: taskID)
        {
            appendSystemMessage(
                "\(task.name) is being worked on by a live worker. Stop the worker before moving the card out of Doing.",
                alwaysShow: true
            )
            return
        }

        // Optimistic update: move the card to the destination column immediately
        // before the RPC completes. The engine remains the authority — on failure
        // we bounce back via bounceBackOptimisticMoves.
        let originColumn = effectiveBoardColumn(for: task)
        pendingMoveOriginByTaskID[taskID] = originColumn
        optimisticColumnByTaskID[taskID] = column
        invalidateWorkCache()

        engine.sendUpdateWorkItem(id: task.id, patch: ["status": targetStatus])

        if targetStatus == "active" {
            engine.sendRequestExecution(workItemId: task.id)
        }
    }

    /// Statuses that the engine itself can drive a chore into at run
    /// completion. The kanban must allow the human to mirror those
    /// transitions even from `active` so a successful PR-merge flow
    /// can move a card to Done without first stopping the worker.
    private static let terminalKanbanStatuses: Set<String> = [
        "done",
        "archived",
    ]

    /// True iff the work item has a non-terminal worker currently
    /// attached (running, paused on input, or idle between turns). Shared
    /// with the kanban drop path (`attemptDrop`), which applies the same
    /// out-of-Doing rule as the popover's explicit Move buttons.
    /// `WorkerActivity.terminated` and `.errored` count as "no live
    /// worker" — the slot is no longer holding the run open.
    func hasLiveWorker(forTaskID taskID: String) -> Bool {
        guard let live = workerLiveState(forTaskID: taskID) else {
            return false
        }
        switch live.activity {
        case .terminated, .errored:
            return false
        case .spawning, .working, .waitingForInput, .idle:
            return true
        }
    }

    func toggleBlocked(for taskID: String) {
        guard let task = task(withID: taskID) else { return }
        let nextStatus: String
        switch task.status {
        case "blocked":
            nextStatus = "active"
        case "active":
            nextStatus = "blocked"
        default:
            return
        }
        engine.sendUpdateWorkItem(id: task.id, patch: ["status": nextStatus])
    }

    /// Update a task or chore's priority via the inline picker on the
    /// detail popover. No-ops when the new value matches the current
    /// one so an idle picker tap doesn't generate write traffic.
    func setPriority(for taskID: String, to priority: WorkPriority) {
        guard let task = task(withID: taskID) else { return }
        guard task.priority != priority.rawValue else { return }
        engine.sendUpdateWorkItem(id: task.id, patch: ["priority": priority.rawValue])
    }
}
