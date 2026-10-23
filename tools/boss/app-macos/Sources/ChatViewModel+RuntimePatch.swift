import Foundation

/// Ordering state for the board reads one product has outstanding: the
/// whole-board `get_work_tree` and the few-card `get_work_runtime_patch`.
///
/// The two are served concurrently and the patch is far cheaper, so without
/// an order a slow tree that started before a write could land after the
/// patch that carries it and put the older card back. Two rules prevent that,
/// and neither depends on the engine's reply order:
///
/// - a patch is never sent while a tree is outstanding for the product, and a
///   tree request supersedes a patch already in flight (its reply is
///   dropped; the tree started later, so it reads at least as much);
/// - at most one patch is in flight per product. Ids that arrive meanwhile
///   wait, and go out together when its reply lands.
///
/// A request whose reply never comes (an error carries no request id) must
/// not wedge the product, so an outstanding request older than
/// `staleAfter` is treated as lost.
struct BoardReadFence {
    static let staleAfter: Duration = .seconds(30)

    /// `get_work_tree` requests sent and not yet answered.
    var treeRequestsOutstanding = 0
    var lastTreeRequestAt: ContinuousClock.Instant?
    var patchInFlight = false
    var patchSentAt: ContinuousClock.Instant?
    /// A tree was requested after the patch in flight was; drop its reply.
    var patchSuperseded = false
    /// Cards that asked for a patch while one was in flight.
    var queuedPatchItemIDs: Set<String> = []

    func hasTreeOutstanding(now: ContinuousClock.Instant) -> Bool {
        guard treeRequestsOutstanding > 0, let last = lastTreeRequestAt else { return false }
        return last.duration(to: now) < Self.staleAfter
    }

    func hasPatchOutstanding(now: ContinuousClock.Instant) -> Bool {
        guard patchInFlight, let sent = patchSentAt else { return false }
        return sent.duration(to: now) < Self.staleAfter
    }
}

/// The app's half of the runtime/structural invalidation split (hub-and-spoke
/// design §2.4). A `work_invalidated` push the engine classed `runtime` names
/// cards whose runtime fields changed; a burst of only those re-reads just
/// those cards (`get_work_runtime_patch`) instead of the whole board. See
/// `ChatViewModel+InvalidationRefetch.swift` for how the burst chooses, and
/// `boss_protocol::InvalidationClass` for the engine's side.
///
/// The class is a hint, not a promise. A patch is applied only when every
/// card in it still has the shape the app holds; anything else is answered
/// with the whole-board read the push would have caused before the split.
extension ChatViewModel {
    /// Most cards one patch request may name; mirrors
    /// `boss_protocol::MAX_RUNTIME_PATCH_ITEMS`. Past it the tree is cheaper.
    static let maxRuntimePatchItems = 64

    /// Hear every `get_work_tree` the client sends, wherever it is sent from.
    func bindBoardReadObserver() {
        engine.onWorkTreeRequested = { [weak self] productID in
            if Thread.isMainThread {
                MainActor.assumeIsolated { self?.noteWorkTreeRequested(productID: productID) }
            } else {
                Task { @MainActor in self?.noteWorkTreeRequested(productID: productID) }
            }
        }
    }

    func noteWorkTreeRequested(productID: String) {
        var fence = boardReadFences[productID] ?? BoardReadFence()
        fence.treeRequestsOutstanding += 1
        fence.lastTreeRequestAt = .now
        if fence.patchInFlight {
            fence.patchSuperseded = true
        }
        // The tree reads every card, so it covers whatever was waiting.
        fence.queuedPatchItemIDs.removeAll()
        boardReadFences[productID] = fence
    }

    func noteWorkTreeApplied(productID: String) {
        guard var fence = boardReadFences[productID], fence.treeRequestsOutstanding > 0 else { return }
        fence.treeRequestsOutstanding -= 1
        boardReadFences[productID] = fence
    }

    /// Drop every outstanding read: the connection that carried them is gone,
    /// so no reply is coming.
    func resetBoardReadFences() {
        boardReadFences.removeAll()
    }

    /// An error reply carries no request id. If a patch was in flight it may
    /// be the one that failed, and nothing else would ever re-read its cards.
    func failInFlightRuntimePatches() {
        for (productID, fence) in boardReadFences where fence.patchInFlight {
            var failed = fence
            failed.patchInFlight = false
            failed.patchSuperseded = false
            failed.queuedPatchItemIDs.removeAll()
            boardReadFences[productID] = failed
            if productID == currentSelectedProductID {
                refetchBoard(productID: productID)
            }
        }
    }

    /// The whole-board answer: the tree and the product-scoped list that is
    /// not part of it, sent now. This is what a structural burst sends.
    func refetchBoard(productID: String) {
        engine.sendGetWorkTree(productId: productID, flow: .invalidationRefetch)
        if productID == currentSelectedProductID {
            engine.sendListDeferredScopeAttentions(productId: productID)
        }
    }

    /// Re-read `itemIDs` of the product on screen without assembling its
    /// board, or fall back to the whole board when a patch cannot be ordered
    /// safely or would not be cheaper.
    func requestRuntimePatch(productID: String, itemIDs: Set<String>) {
        guard !itemIDs.isEmpty else { return }
        var fence = boardReadFences[productID] ?? BoardReadFence()
        let now = ContinuousClock.now
        if itemIDs.count > Self.maxRuntimePatchItems || fence.hasTreeOutstanding(now: now) {
            refetchBoard(productID: productID)
            return
        }
        if fence.hasPatchOutstanding(now: now) {
            fence.queuedPatchItemIDs.formUnion(itemIDs)
            boardReadFences[productID] = fence
            return
        }
        fence.patchInFlight = true
        fence.patchSentAt = now
        fence.patchSuperseded = false
        boardReadFences[productID] = fence
        engine.sendGetWorkRuntimePatch(productId: productID, itemIds: itemIDs.sorted())
    }

    /// Reception of a `work_runtime_patch` reply.
    func handleWorkRuntimePatch(
        productID: String,
        cards: [WorkRuntimePatchCard],
        absent: [String],
        isComplete: Bool
    ) {
        guard var fence = boardReadFences[productID], fence.patchInFlight else {
            // Nothing is waiting for this: its request outlived a reset or its
            // staleness window, so its place in the order is unknown.
            if productID == currentSelectedProductID { refetchBoard(productID: productID) }
            return
        }
        let superseded = fence.patchSuperseded
        let queued = fence.queuedPatchItemIDs
        fence.patchInFlight = false
        fence.patchSuperseded = false
        fence.queuedPatchItemIDs.removeAll()
        boardReadFences[productID] = fence

        // A tree requested after the patch will read at least what it read;
        // a product that is no longer on screen is refetched when selected.
        guard !superseded, productID == currentSelectedProductID else { return }

        guard isComplete,
              absent.allSatisfy({ task(withID: $0) == nil }),
              applyRuntimePatch(productID: productID, cards: cards)
        else {
            refetchBoard(productID: productID)
            return
        }
        if !queued.isEmpty {
            requestRuntimePatch(productID: productID, itemIDs: queued)
        }
    }

    /// True when `patched` is not just `previous` with different runtime
    /// fields: a card the app does not hold, or one that has moved, changed
    /// kind or status, been re-ordered, re-parented, or changed arm. Those are
    /// what a structural invalidation exists for, and what a board patched in
    /// place would get wrong. The class on the push is a hint; this is the
    /// check that keeps a misclassified site from leaving a stale board.
    static func runtimePatchChangesBoardStructure(previous: WorkTask?, patched: WorkTask, isChore: Bool) -> Bool {
        guard let previous else { return true }
        if incrementalUpdateRequiresFullInvalidation(previous: previous, updated: patched, isChore: isChore) {
            return true
        }
        return previous.status != patched.status
            || previous.ordinal != patched.ordinal
            || previous.parentTaskId != patched.parentTaskId
            || previous.deletedAt != patched.deletedAt
    }

    /// Apply the patch in place, or change nothing and return false when any
    /// card in it is structural. All cards are checked before any is applied,
    /// so a rejected patch leaves the board exactly as it was.
    private func applyRuntimePatch(productID: String, cards: [WorkRuntimePatchCard]) -> Bool {
        var patched: [(task: WorkTask, isChore: Bool, runtime: WorkTaskRuntime)] = []
        for card in cards {
            let task: WorkTask
            let isChore: Bool
            switch card.item {
            case .task(let value):
                (task, isChore) = (value, false)
            case .chore(let value):
                (task, isChore) = (value, true)
            case .product, .project:
                return false
            }
            guard task.productID == productID, card.runtime.workItemID == task.id,
                  !Self.runtimePatchChangesBoardStructure(previous: self.task(withID: task.id), patched: task, isChore: isChore)
            else { return false }
            patched.append((task, isChore, card.runtime))
        }

        for card in patched where self.task(withID: card.task.id) != card.task {
            applyIncrementalTaskUpdate(card.task, isChore: card.isChore)
        }
        // Stage on a local copy so the whole merge is one write to the
        // published dictionary; see `mergeTaskRuntimes`.
        var runtimes = taskRuntimesByID
        var runtimesChanged = false
        for card in patched where runtimes[card.runtime.workItemID] != card.runtime {
            runtimes[card.runtime.workItemID] = card.runtime
            runtimesChanged = true
        }
        if runtimesChanged {
            taskRuntimesByID = runtimes
        }
        reconcileOptimisticOverrides(from: patched.map(\.task))
        return true
    }
}
