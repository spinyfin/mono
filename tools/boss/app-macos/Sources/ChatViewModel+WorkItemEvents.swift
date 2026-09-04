import Foundation

/// Reception of engine-pushed work-item mutations: the `workItemCreated` /
/// `workItemsCreated` / `workItemUpdated` arms of
/// `ChatViewModel+EventHandling.swift`, plus the store surgery and selection
/// reconciliation they drive. Split out of the event switch itself because
/// this is where the in-memory work tree is actually rewritten — the switch
/// only routes.
extension ChatViewModel {
    /// Reception of a full `WorkTree` snapshot for one product: replaces that
    /// product's slice of every bucket, re-sorts it, and re-runs the
    /// reconciliations a fresh snapshot invalidates (optimistic overrides,
    /// selection, subscriptions, design-doc state).
    func applyWorkTree(
        product: WorkProduct,
        projects: [WorkProject],
        tasks: [WorkTask],
        chores: [WorkTask],
        taskRuntimes: [WorkTaskRuntime],
        dependencies: [WorkItemDependency],
        ideas: [WorkIdea]
    ) {
        // Fan-out regression counter (design entry 2): full work-tree applies.
        UIUpdateCounters.shared.recordApplyWorkTree()
        // Population-timing (T2101 R1): time this @MainActor apply burst
        // and its two hot sub-steps. `popCtx` carries the flow/seq tag
        // decoded off-main so every segment of one fetch reads together.
        let popCtx = PopulationTiming.shared.takeContextForApply(productId: product.id)
        let popApplyStartNanos = PopulationTiming.now()
        upsertProduct(product)
        if currentSelectedProductID == nil {
            selectedWorkProductID = product.id
        }
        projectsByProductID[product.id] = projects.sorted(by: projectSort)
        let popBucketStartNanos = PopulationTiming.now()
        // Evict exactly this product's previous buckets (tracked
        // incrementally in [[trackedProjectIDsByProductID]]) instead of
        // filtering the whole dictionary — `tasksByProjectID` accumulates
        // every product ever viewed this session, so the naive
        // `tasksByProjectID.filter { ... }` scan cost O(every product's
        // tasks) on every single refresh, not O(this product's tasks).
        if let staleProjectIDs = trackedProjectIDsByProductID[product.id] {
            for projectID in staleProjectIDs {
                tasksByProjectID.removeValue(forKey: projectID)
            }
        }
        var productLevelRevisions: [WorkTask] = []
        var productLevelTasks: [WorkTask] = []
        var freshProjectIDs: Set<String> = []
        for task in tasks {
            guard let projectID = task.projectID else {
                // Product-level rows (`project_id IS NULL`) have no project
                // lane to live under. Route every one of them into a bucket
                // rather than dropping the ones we don't special-case — a
                // chore-parented revision rolls up under its parent (issue
                // #789), and everything else (investigations, any future
                // product-level kind) renders as a first-class card (issue
                // #886). The `else` is a catch-all on purpose: nothing the
                // engine sends should silently disappear here.
                if task.kind == "revision" {
                    productLevelRevisions.append(task)
                } else {
                    productLevelTasks.append(task)
                }
                continue
            }
            tasksByProjectID[projectID, default: []].append(task)
            freshProjectIDs.insert(projectID)
        }
        trackedProjectIDsByProductID[product.id] = freshProjectIDs
        let popBucketEndNanos = PopulationTiming.now()
        for projectID in freshProjectIDs {
            if let projectTasks = tasksByProjectID[projectID] {
                tasksByProjectID[projectID] = projectTasks.sorted(by: taskSort)
            }
        }
        choresByProductID[product.id] = chores.sorted(by: taskSort)
        productLevelRevisionsByProductID[product.id] = productLevelRevisions.sorted(by: taskSort)
        productLevelTasksByProductID[product.id] = productLevelTasks.sorted(by: taskSort)
        let popSortEndNanos = PopulationTiming.now()
        mergeTaskRuntimes(taskRuntimes, for: product.id, tasks: tasks, chores: chores)
        dependenciesByProductID[product.id] = dependencies
        ideasByProductID[product.id] = ideas
        seedReviewTaskIDs(tasks: tasks, chores: chores, productID: product.id)
        // After tasksByProjectID reflects real engine state, clear optimistic
        // overrides for cards whose true column now matches the target.
        // Done before the @Published assignments take effect in the view so
        // the next render uses real boardColumn values — no visible flicker.
        reconcileOptimisticOverrides(from: tasks + chores)
        reconcileWorkSelection()
        refreshWorkSubscriptions()
        refreshDesignDocStates(for: projects)
        engine.sendListAttentionItemsForWorkItem(workItemID: product.id)
        engine.sendListAttentionGroups(productId: product.id)
        workErrorMessage = nil
        if let pending = pendingRevealScrollID {
            let allIDs = Set(tasks.map(\.id) + chores.map(\.id))
            if allIDs.contains(pending) {
                pendingRevealScrollID = nil
                triggerRevealScroll(pending)
            }
        }
        recordPopulationApplyBurst(
            context: popCtx,
            applyStartNanos: popApplyStartNanos,
            bucketStartNanos: popBucketStartNanos,
            bucketEndNanos: popBucketEndNanos,
            sortEndNanos: popSortEndNanos
        )
    }

    func upsertProduct(_ product: WorkProduct) {
        if let index = products.firstIndex(where: { $0.id == product.id }) {
            products[index] = product
        } else {
            products.append(product)
            products.sort(by: { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending })
        }
    }

    func handleCreatedWorkItem(_ item: WorkItemPayload) {
        workErrorMessage = nil
        switch item {
        case .product(let product):
            upsertProduct(product)
            selectedWorkProductID = product.id
            selectedProjectFilterIDs = []
            selectedWorkCardID = nil
            persistSelectedProductID(product.id)
            persistProjectFilterIDs()
            engine.sendGetWorkTree(productId: product.id, flow: .itemRefetch)
        case .project(let project):
            selectedWorkProductID = project.productID
            selectedProjectFilterIDs = [project.id]
            selectedWorkCardID = nil
            persistSelectedProductID(project.productID)
            persistProjectFilterIDs()
            engine.sendGetWorkTree(productId: project.productID, flow: .itemRefetch)
        case .task(let task):
            selectedWorkProductID = task.productID
            if let projectID = task.projectID {
                selectedProjectFilterIDs = [projectID]
            } else {
                selectedProjectFilterIDs = []
            }
            selectedWorkCardID = task.id
            persistSelectedProductID(task.productID)
            persistProjectFilterIDs()
            engine.sendGetWorkTree(productId: task.productID, flow: .itemRefetch)
        case .chore(let task):
            selectedWorkProductID = task.productID
            selectedWorkCardID = task.id
            setIncludeChores(true)
            persistSelectedProductID(task.productID)
            engine.sendGetWorkTree(productId: task.productID, flow: .itemRefetch)
        }
        refreshWorkSubscriptions()
    }

    /// Passive counterpart of `handleCreatedWorkItem` for a background batch
    /// push (e.g. the auto-populate Populator staging a project's task
    /// breakdown while the operator is looking at something else). Unlike
    /// the single-item path, this must never hijack the operator's current
    /// selection/filters — it only refreshes the affected product's board
    /// and, for any touched projects, their planner-run audit trail so a
    /// [[PlannerRunAffordance]] icon that hasn't appeared yet surfaces
    /// without a view remount.
    func handleCreatedWorkItemsBatch(_ items: [WorkItemPayload]) {
        var productIDs: Set<String> = []
        var projectIDs: Set<String> = []
        for item in items {
            switch item {
            case .product(let product):
                productIDs.insert(product.id)
            case .project(let project):
                productIDs.insert(project.productID)
                projectIDs.insert(project.id)
            case .task(let task), .chore(let task):
                productIDs.insert(task.productID)
                if let projectID = task.projectID {
                    projectIDs.insert(projectID)
                }
            }
        }
        for productID in productIDs where productID == currentSelectedProductID {
            engine.sendGetWorkTree(productId: productID, flow: .itemRefetch)
        }
        for projectID in projectIDs {
            refreshPlannerRuns(projectID: projectID)
        }
    }

    func handleUpdatedWorkItem(_ item: WorkItemPayload) {
        switch item {
        case .product(let product):
            let wasSelected = selectedWorkProductID == product.id
            upsertProduct(product)
            if wasSelected && product.status == "archived" {
                workErrorMessage = "Product \"\(product.name)\" was archived; switching to the next active product."
                reconcileWorkSelection()
                if let nextID = selectedWorkProductID {
                    engine.sendGetWorkTree(productId: nextID, flow: .productSwitch)
                }
                refreshWorkSubscriptions()
                return
            }
        case .project(let project):
            engine.sendGetWorkTree(productId: project.productID, flow: .itemRefetch)
        case .task(let updatedTask), .chore(let updatedTask):
            // When the engine confirms an optimistic move, drop the origin record
            // so a subsequent work_error from an unrelated operation won't bounce
            // a card that is already confirmed.
            if let targetColumn = optimisticColumnByTaskID[updatedTask.id],
               updatedTask.boardColumn == targetColumn {
                pendingMoveOriginByTaskID.removeValue(forKey: updatedTask.id)
            } else if optimisticColumnByTaskID[updatedTask.id] != nil {
                // Engine returned a different status — move silently rejected.
                bounceBackOptimisticMoves(message: nil)
            }
            maybeFireReviewNotification(for: updatedTask)
            // Apply the update directly to the in-memory store instead of
            // fetching the full work tree. The payload already carries the
            // updated task, so a second round-trip is unnecessary.
            let isChore: Bool
            if case .chore = item { isChore = true } else { isChore = false }
            applyIncrementalTaskUpdate(updatedTask, isChore: isChore)
            // Reconcile optimistic overrides now that the local store reflects
            // the confirmed state — the update above already wrote the new
            // status, so the real column matches the optimistic target.
            reconcileOptimisticOverrides(from: [updatedTask])
        }
        workErrorMessage = nil
    }

    /// Apply a single task or chore update to the in-memory store without
    /// fetching the full work tree. Routes the task into the correct bucket
    /// based on its current `projectID` and `kind`, removing any stale entry
    /// from other buckets first (handles the rare case where these change).
    ///
    /// Cache invalidation is keyed (design entry 8): when bucket membership
    /// and kind are unchanged, `taskIndexByID` is patched in place and the
    /// dependency / revision caches are only dropped when their baked-in
    /// snapshots would go stale (status / name for prereq rows; any field
    /// on a revision row). Project-membership, kind, or unknown-previous
    /// updates still full-invalidate.
    private func applyIncrementalTaskUpdate(_ updatedTask: WorkTask, isChore: Bool) {
        // Fan-out regression counter (design entry 2): single-task board update.
        UIUpdateCounters.shared.recordIncrementalTaskUpdate()
        let previous = task(withID: updatedTask.id)
        let requiresFullInvalidation = Self.incrementalUpdateRequiresFullInvalidation(
            previous: previous,
            updated: updatedTask,
            isChore: isChore
        )

        // Suppress the bucket didSet → full invalidate so we can apply a
        // keyed drop after the mutation instead of discarding all ten caches
        // once per assignment.
        withSuppressedWorkCacheInvalidation {
            let productID = updatedTask.productID
            let taskID = updatedTask.id

            // Evict the id from every bucket it might previously have lived
            // in (chore arm included) so a kind/project/product flip cannot
            // leave a stale twin behind. Dictionary values are Arrays —
            // copy out, mutate, write back; `dict[key]?.removeAll` discards
            // the copy. Product-scoped maps are scanned across all keys so a
            // productID change does not leave the row under the old product.
            for key in Array(tasksByProjectID.keys) {
                guard var bucket = tasksByProjectID[key] else { continue }
                let before = bucket.count
                bucket.removeAll { $0.id == taskID }
                if bucket.count != before {
                    tasksByProjectID[key] = bucket
                }
            }
            for key in Array(choresByProductID.keys) {
                guard var bucket = choresByProductID[key] else { continue }
                let before = bucket.count
                bucket.removeAll { $0.id == taskID }
                if bucket.count != before {
                    choresByProductID[key] = bucket
                }
            }
            for key in Array(productLevelRevisionsByProductID.keys) {
                guard var bucket = productLevelRevisionsByProductID[key] else { continue }
                let before = bucket.count
                bucket.removeAll { $0.id == taskID }
                if bucket.count != before {
                    productLevelRevisionsByProductID[key] = bucket
                }
            }
            for key in Array(productLevelTasksByProductID.keys) {
                guard var bucket = productLevelTasksByProductID[key] else { continue }
                let before = bucket.count
                bucket.removeAll { $0.id == taskID }
                if bucket.count != before {
                    productLevelTasksByProductID[key] = bucket
                }
            }

            // Insert into the destination bucket for the updated shape.
            if isChore {
                var chores = choresByProductID[productID] ?? []
                chores.append(updatedTask)
                choresByProductID[productID] = chores.sorted(by: taskSort)
            } else if let projectID = updatedTask.projectID {
                var tasks = tasksByProjectID[projectID] ?? []
                tasks.append(updatedTask)
                tasksByProjectID[projectID] = tasks.sorted(by: taskSort)
                // Keep the tracked set authoritative: applyWorkTree only
                // evicts buckets it knows about, so a project that gains its
                // first task here (previously untracked/empty) must be
                // recorded or the next full apply will leave this bucket
                // un-evicted and duplicate the task onto itself.
                trackedProjectIDsByProductID[productID, default: []].insert(projectID)
            } else if updatedTask.kind == "revision" {
                var revisions = productLevelRevisionsByProductID[productID] ?? []
                revisions.append(updatedTask)
                productLevelRevisionsByProductID[productID] = revisions.sorted(by: taskSort)
            } else {
                var productLevelItems = productLevelTasksByProductID[productID] ?? []
                productLevelItems.append(updatedTask)
                productLevelTasksByProductID[productID] = productLevelItems.sorted(by: taskSort)
            }
        }

        if requiresFullInvalidation {
            invalidateWorkCache()
            return
        }

        // Same bucket + same kind: patch the id index and drop only the
        // caches this row can affect.
        patchTaskIndex(with: updatedTask)
        var keys: WorkCacheInvalidation = [.boardLayout]
        // Status feeds edge satisfaction (`isWorkItemRowSatisfied`); name is
        // baked into `WorkDependencyRow.title` at rebuild time. Either change
        // must drop the prereq graphs so the next read rebuilds with current
        // titles / satisfaction.
        if previous?.status != updatedTask.status
            || previous?.name != updatedTask.name
        {
            keys.insert(.dependencies)
        }
        if taskUpdateAffectsRevisionCache(previous: previous, updated: updatedTask) {
            keys.insert(.revisions)
        }
        invalidateWorkCache(keys)
    }

    /// True when an incremental update must discard every derived cache
    /// (id index, dep graphs, revision rollups, board layout). Covers the
    /// correctness cases: unknown previous row, project-membership change,
    /// kind change, and chore/task arm flip.
    static func incrementalUpdateRequiresFullInvalidation(
        previous: WorkTask?,
        updated: WorkTask,
        isChore: Bool
    ) -> Bool {
        guard let previous else { return true }
        if previous.kind != updated.kind { return true }
        if previous.projectID != updated.projectID { return true }
        if previous.productID != updated.productID { return true }
        if previous.isChore != isChore { return true }
        return false
    }

    /// Fire a review notification when `updatedTask` enters `in_review`
    /// for the first time (not already in [[knownReviewTaskIDs]]).
    /// Clears the task from the set when it leaves `in_review` so a
    /// subsequent re-entry (e.g. worker re-opens a revised PR) fires again.
    private func maybeFireReviewNotification(for updatedTask: WorkTask) {
        if updatedTask.status == "in_review" {
            guard !knownReviewTaskIDs.contains(updatedTask.id) else { return }
            knownReviewTaskIDs.insert(updatedTask.id)
            reviewNotifier.notifyReadyForReview(task: updatedTask)
        } else {
            knownReviewTaskIDs.remove(updatedTask.id)
        }
    }

    /// Sync [[knownReviewTaskIDs]] from a full product work-tree snapshot
    /// without firing notifications. Called on initial load and reconnect
    /// so tasks already in Review at startup don't trigger spurious banners.
    func seedReviewTaskIDs(tasks: [WorkTask], chores: [WorkTask], productID: String) {
        // Remove all IDs belonging to this product, then re-add the current in-review ones.
        // Avoids stale entries when a task leaves review between two tree snapshots.
        let productItemIDs = Set(tasks.map(\.id) + chores.map(\.id))
        knownReviewTaskIDs.subtract(productItemIDs)
        for item in tasks + chores where item.status == "in_review" {
            knownReviewTaskIDs.insert(item.id)
        }
    }

    func reconcileWorkSelection() {
        guard let selectedWorkProductID else { return }

        let activeIDs = Set(activeProducts.map(\.id))
        if !activeIDs.contains(selectedWorkProductID) {
            self.selectedWorkProductID = activeProducts.first?.id
            persistSelectedProductID(activeProducts.first?.id)
        }

        let validProjectIDs = selectedProjectFilterIDs.filter { projectID in
            project(withID: projectID)?.productID == selectedWorkProductID
        }
        if validProjectIDs != selectedProjectFilterIDs {
            selectedProjectFilterIDs = validProjectIDs
            persistProjectFilterIDs()
        }

        if let selectedTask, !isTaskVisible(selectedTask) {
            selectedWorkCardID = nil
        }

        refreshWorkSubscriptions()
    }
}
