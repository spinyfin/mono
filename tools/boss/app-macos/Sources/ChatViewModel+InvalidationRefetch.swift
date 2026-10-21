import Foundation

/// What one coalesced per-product refetch burst re-reads when its timer fires.
struct RefetchScope: OptionSet, Sendable {
    let rawValue: Int

    /// `get_work_tree`. The snapshot already carries the attention items,
    /// groups and members, and each project's latest planner-run summary, so
    /// none of them has a fetch of its own.
    static let tree = RefetchScope(rawValue: 1 << 0)
    /// `list_deferred_scope_attentions`: the one product-scoped list that is
    /// not part of the tree.
    static let deferredScope = RefetchScope(rawValue: 1 << 1)
    /// `list_executions` for each invalidated item that has an open consumer.
    static let executions = RefetchScope(rawValue: 1 << 2)
    /// The event-triggered `list_engine_attempts` background-work poll.
    static let backgroundWork = RefetchScope(rawValue: 1 << 3)

    /// A reorder, delete or planner action: only the board moved.
    static let treeOnly: RefetchScope = [.tree]
    /// A `work_invalidated` push for the product on screen: anything it
    /// could have covered.
    static let invalidation: RefetchScope = [.tree, .deferredScope, .executions, .backgroundWork]
    /// A `work_invalidated` push for a product that is not on screen: only
    /// the lazily loaded data that is not tied to the board.
    static let offBoard: RefetchScope = [.executions, .backgroundWork]
}

/// Timing of the coalesced refetch. See [[ChatViewModel.scheduleRefetch]].
struct RefetchCoalescingPolicy: Equatable, Sendable {
    /// Quiet time after the latest trigger before the burst fires.
    var window: Duration
    /// Longest a burst may be postponed, counted from its first trigger, so
    /// a stream of triggers closer together than `window` still refreshes.
    var maxWait: Duration

    static let standard = RefetchCoalescingPolicy(window: .milliseconds(150), maxWait: .seconds(1))
}

/// Triggers collected for one product while its timer is pending.
struct PendingRefetchBurst {
    let firstTriggerAt: ContinuousClock.Instant
    var flow: PopulationFlow
    var scope: RefetchScope = []
    /// Union of the item ids the triggers named.
    var itemIDs: Set<String> = []
    /// The triggers could not name what changed (a dropped invalidation), so
    /// every open executions consumer is refetched.
    var includesAllOpenConsumers = false
}

/// Per-product coalescing of everything the app re-reads after the engine
/// says work changed.
///
/// A trigger never sends anything itself: it adds to the product's pending
/// burst and restarts that product's single timer. When the timer fires, the
/// burst sends its whole set once — N triggers inside the window cost one
/// set, and the trailing edge always fires, so the last trigger's state is
/// always re-read. Cold start, product switch and manual refresh are not
/// triggers; they want their data immediately and still send directly.
extension ChatViewModel {
    /// Key of the burst for an invalidation that names no product.
    static let unscopedRefetchKey = "<unscoped>"

    /// Tree-only trigger (reorder, delete, planner action). Joins whichever
    /// burst is already pending for the product.
    func scheduleWorkTreeRefetch(productID: String, flow: PopulationFlow) {
        scheduleRefetch(productID: productID, scope: .treeOnly, flow: flow)
    }

    /// The entry point for any trigger that means "the engine's work state
    /// changed under you": a `work_invalidated` push, or a `resync_required`
    /// that says pushes were dropped (`includesAllOpenConsumers`). Pass the
    /// product the invalidation belongs to, or `nil` when it names none.
    func scheduleInvalidationRefetch(
        productID: String?,
        itemIDs: [String] = [],
        includesAllOpenConsumers: Bool = false
    ) {
        let onBoard = productID != nil && productID == currentSelectedProductID
        scheduleRefetch(
            productID: productID ?? Self.unscopedRefetchKey,
            scope: onBoard ? .invalidation : .offBoard,
            flow: .invalidationRefetch,
            itemIDs: itemIDs,
            includesAllOpenConsumers: includesAllOpenConsumers
        )
    }

    /// Add a trigger to `productID`'s pending burst and restart its timer.
    /// The timer waits `window` after this trigger, but never past `maxWait`
    /// after the burst's first one.
    func scheduleRefetch(
        productID: String,
        scope: RefetchScope,
        flow: PopulationFlow,
        itemIDs: [String] = [],
        includesAllOpenConsumers: Bool = false
    ) {
        let now = ContinuousClock.now
        var burst = pendingRefetchBursts[productID] ?? PendingRefetchBurst(firstTriggerAt: now, flow: flow)
        burst.flow = flow
        burst.scope.formUnion(scope)
        burst.itemIDs.formUnion(itemIDs)
        burst.includesAllOpenConsumers = burst.includesAllOpenConsumers || includesAllOpenConsumers
        pendingRefetchBursts[productID] = burst

        let policy = refetchCoalescingPolicy
        let untilCap = max(Duration.zero, policy.maxWait - burst.firstTriggerAt.duration(to: now))
        let delay = min(policy.window, untilCap)
        pendingWorkTreeRefetchTasks[productID]?.cancel()
        pendingWorkTreeRefetchTasks[productID] = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled else { return }
            self?.fireRefetchBurst(productID: productID)
        }
    }

    private func fireRefetchBurst(productID: String) {
        pendingWorkTreeRefetchTasks.removeValue(forKey: productID)
        guard let burst = pendingRefetchBursts.removeValue(forKey: productID) else { return }
        if burst.scope.contains(.tree) {
            engine.sendGetWorkTree(productId: productID, flow: burst.flow)
        }
        // The user may have switched products since the trigger; the switch
        // fetched the new product's list itself.
        if burst.scope.contains(.deferredScope), productID == currentSelectedProductID {
            engine.sendListDeferredScopeAttentions(productId: productID)
        }
        if burst.scope.contains(.executions) {
            let candidates = burst.includesAllOpenConsumers
                ? Set(executionsConsumerTokensByTaskID.keys)
                : burst.itemIDs
            for taskID in candidates.sorted() where hasOpenExecutionsConsumer(taskID: taskID) {
                engine.sendListExecutions(taskId: taskID)
            }
        }
        if burst.scope.contains(.backgroundWork) {
            requestBackgroundWorkRefresh()
        }
    }

    /// Test-only: suspend until every pending burst has fired, so an
    /// assertion about what a trigger sent runs after it was sent. Returns
    /// `false` if a burst was still pending after `timeout`.
    func waitForRefetchBurstsForTest(timeout: Duration = .seconds(5)) async -> Bool {
        let deadline = ContinuousClock.now + timeout
        while !pendingWorkTreeRefetchTasks.isEmpty, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(5))
        }
        return pendingWorkTreeRefetchTasks.isEmpty
    }

    // MARK: - Executions consumers

    /// Whether a card popover or transcript viewer currently shows `taskID`'s
    /// execution history, and so wants it kept fresh.
    func hasOpenExecutionsConsumer(taskID: String) -> Bool {
        executionsConsumerTokensByTaskID[taskID] != nil
    }

    /// A view that shows `taskID`'s execution history appeared: register it
    /// and load the history. `token` identifies this view instance, so two
    /// views on one task don't evict each other.
    func beginExecutionsConsumer(taskID: String, token: UUID) {
        executionsConsumerTokensByTaskID[taskID, default: []].insert(token)
        loadExecutions(taskId: taskID)
    }

    /// The view registered under `token` went away. The last consumer of a
    /// task takes its cached history with it, which also stops the refetch
    /// that the invalidation burst would otherwise keep sending for it.
    func endExecutionsConsumer(taskID: String, token: UUID) {
        guard var tokens = executionsConsumerTokensByTaskID[taskID], tokens.remove(token) != nil else { return }
        if tokens.isEmpty {
            executionsConsumerTokensByTaskID.removeValue(forKey: taskID)
            executionsByTaskID[taskID] = nil
            executionsInFlightTaskIDs.remove(taskID)
            executionsLoadFailureByTaskID.removeValue(forKey: taskID)
        } else {
            executionsConsumerTokensByTaskID[taskID] = tokens
        }
    }
}
