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
        guard canSubmitUserActions(setAsProductDefault ? 2 : 1) else { return }
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
        guard canSubmitUserActions() else { return }
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

        let requestID = engine.sendUpdateWorkItem(id: id, patch: patch)
        if requestID != nil || engine.hasUnsentAction(kind: "update_work_item") {
            pendingWorkEditRequest = nil
        }
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
              let projectID = task.projectID,
              // The reorder endpoint accepts ordinary tasks and follow-ups;
              // design tasks and other project members retain their ordinals.
              var tasks = tasksByProjectID[projectID]?
                .filter({ $0.kind == "project_task" || $0.kind == "followup" }).sorted(by: taskSort),
              let currentIndex = tasks.firstIndex(where: { $0.id == task.id })
        else {
            return
        }

        let destination = currentIndex + offset
        guard tasks.indices.contains(destination) else { return }

        tasks.swapAt(currentIndex, destination)
        if let requestID = engine.sendReorderProjectTasks(projectId: projectID, taskIds: tasks.map(\.id), movedTaskId: task.id) {
            reorderRequestTaskIDs[requestID] = task.id
        }
    }

    /// A `work_error` carrying a pending reorder's own request id belongs to
    /// the card that was moved: show it there. Returns whether the request
    /// id matched a reorder.
    func settleReorderWorkError(message: String, requestId: String?) -> Bool {
        guard let requestId, let taskID = reorderRequestTaskIDs.removeValue(forKey: requestId) else { return false }
        dragRefusalNotice = DragRefusalNotice(taskID: taskID, message: message)
        scheduleDragRefusalDismiss(for: taskID)
        return true
    }

    /// The engine accepted the reorder carrying `requestId`: that request can
    /// never see an error reply. Other reorders still in flight keep their
    /// card mapping so their own replies still land on the right card.
    func settleReorderSuccess(requestId: String?) {
        guard let requestId else { return }
        reorderRequestTaskIDs.removeValue(forKey: requestId)
    }

    /// A refused "Add revision…" (attributed by the engine client to its
    /// parent card, whichever request id the request finally went out under)
    /// shows on that card.
    func settleRevisionRefused(message: String, parentTaskID: String) {
        dragRefusalNotice = DragRefusalNotice(taskID: parentTaskID, message: message, code: "create_revision_failed")
        scheduleDragRefusalDismiss(for: parentTaskID)
    }

    /// Apply an explicit column choice from the work-card popover's "Move"
    /// buttons. Same gesture as a drop on that column (see `attemptDrop`):
    /// the engine resolves it, including dispatching a worker on a move into
    /// Doing, and a move out of Doing with a live worker attached is refused
    /// except to Backlog (stop + park) or Done.
    func moveTask(_ taskID: String, to column: WorkBoardColumnKey) {
        // An explicit Move is the same gesture as dropping the card on that
        // column: route it through the drop path so the engine resolves it,
        // refusals come back as `work_item_refused`, and the live-worker
        // guard shows its message on the card.
        //
        // A blocked card is the exception: its popover button stays enabled
        // in the column it already sits in, where a drop would be a no-op
        // reorder. That button means "put it in this status" (unblock), so
        // send the explicit status and let the engine apply and vet it.
        if let task = task(withID: taskID),
           task.status == "blocked",
           effectiveBoardColumn(for: task) == column
        {
            engine.sendMoveWorkItemOnBoard(id: taskID, column: column, group: nil, explicitStatus: column.targetStatus)
            return
        }
        _ = attemptDrop(taskID, onColumn: column, group: nil)
    }

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
        case .spawning, .working, .waitingForInput, .waitingOnWork, .waitOverdue, .idle:
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

    /// Acknowledge a card's engine-stop notice up to the audit row it showed.
    /// The engine only covers rows up to that id, so a stop recorded after the
    /// card was drawn stays visible.
    func acknowledgeAutoStopNotice(for task: WorkTask) {
        guard let notice = task.autoStopNotice else { return }
        engine.sendUpdateWorkItem(id: task.id, patch: ["acknowledge_auto_stop_through": notice.auditId])
    }

    func navigatorBriefDraft(for taskID: String) -> NavigatorBriefDraft? {
        navigator.briefDrafts[taskID]
    }

    func navigatorBriefText(for task: WorkTask) -> String {
        navigator.briefDrafts[task.id]?.text ?? task.description
    }

    func navigatorBriefIsEditing(_ taskID: String) -> Bool {
        navigator.briefDrafts[taskID]?.isEditing == true
    }

    /// True when the stored document moved while a dirty draft is open.
    func navigatorBriefChangedRemotely(_ task: WorkTask) -> Bool {
        guard let draft = navigator.briefDrafts[task.id], draft.isEditing else { return false }
        return task.description != draft.baseline && task.description != draft.text
    }

    func beginNavigatorBriefEdit(_ task: WorkTask) {
        guard navigatorBriefCanEdit(task) else { return }
        let rootID = task.navigator?.rootTaskId ?? task.id
        if navigatorDocumentTask(for: self.task(withID: rootID) ?? task).id == task.id {
            navigator.draftDocumentByRoot[rootID] = task.id
        }
        let existing = navigator.briefDrafts[task.id]
        navigator.briefDrafts[task.id] = NavigatorBriefDraft(
            text: existing?.text ?? task.description,
            baseline: existing?.baseline ?? task.description,
            isEditing: true
        )
    }

    func setNavigatorBriefText(_ taskID: String, text: String) {
        guard var draft = navigator.briefDrafts[taskID] else { return }
        draft.text = text
        navigator.briefDrafts[taskID] = draft
    }

    func cancelNavigatorBriefEdit(_ task: WorkTask) {
        navigator.briefDrafts[task.id] = nil
        releaseNavigatorDraftPin(task.id)
    }

    func reloadNavigatorBrief(_ task: WorkTask) {
        navigator.briefDrafts[task.id] = NavigatorBriefDraft(
            text: task.description,
            baseline: task.description,
            isEditing: true
        )
    }

    func navigatorBriefCanEdit(_ task: WorkTask) -> Bool {
        let root = task.navigator.flatMap { self.task(withID: $0.rootTaskId) } ?? task
        return isConnected && task.deletedAt == nil && root.deletedAt == nil
            && !["done", "landed", "archived"].contains(task.status)
            && !["done", "landed", "archived"].contains(root.status)
            && root.navigator?.root?.knownSection != .landed
    }

    /// Writes the stored document. It does not send input to a running agent.
    @discardableResult
    func saveNavigatorBrief(_ task: WorkTask) -> Bool {
        guard canSubmitUserActions(), navigatorBriefCanEdit(task) else { return false }
        let text = navigator.briefDrafts[task.id]?.text ?? task.description
        let requestID = engine.sendUpdateWorkItem(id: task.id, patch: ["description": text])
        if requestID != nil || engine.hasUnsentAction(kind: "update_work_item") {
            navigator.briefDrafts[task.id] = nil
            releaseNavigatorDraftPin(task.id)
            return true
        }
        return false
    }

    func performNavigatorCommand(_ command: NavigatorCommand) {
        switch command.kind {
        case "start", "retry":
            guard canSubmitUserActions() else { return }
            engine.sendRequestExecution(workItemId: command.taskId)
        case "park":
            guard let task = task(withID: command.taskId) else { return }
            requestPark(task: task)
        case "edit_brief":
            if let task = task(withID: command.taskId) {
                revealNavigatorDocument(for: task)
                beginNavigatorBriefEdit(task)
            }
        default:
            break
        }
    }

    /// Show the document an edit targets so its draft is never hidden.
    private func revealNavigatorDocument(for task: WorkTask) {
        guard let rootID = navigator.selectedRootID ?? task.navigator?.rootTaskId,
              let root = self.task(withID: rootID) else { return }
        if task.id == root.id {
            setNavigatorDocument(.original, rootID: rootID)
        } else if task.id == root.navigator?.root?.revisions.currentTaskId {
            setNavigatorDocument(.currentAsk, rootID: rootID)
        } else {
            navigator.historicalDocumentByRoot[rootID] = task.id
            setNavigatorDocument(.historical, rootID: rootID)
        }
    }
}
