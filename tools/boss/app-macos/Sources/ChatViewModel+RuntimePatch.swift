import Foundation

/// Request identities and snapshot watermark for one product's board.
/// Expired reads are retired individually; their late replies cannot mutate state.
struct BoardReadFence {
    static let staleAfter: Duration = .seconds(30)
    var trees: [String: ContinuousClock.Instant] = [:]
    var patchRequestID: String?
    var patchSentAt: ContinuousClock.Instant?
    var patchSuperseded = false
    var queuedPatchItemIDs: Set<String> = []
    var revision: UInt64 = 0

    mutating func expire(now: ContinuousClock.Instant = .now) {
        trees = trees.filter { $0.value.duration(to: now) < Self.staleAfter }
        if let sent = patchSentAt, sent.duration(to: now) >= Self.staleAfter {
            patchRequestID = nil
        }
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
        engine.onWorkTreeRequested = { [weak self] productID, requestID in
            if Thread.isMainThread {
                MainActor.assumeIsolated { self?.noteWorkTreeRequested(productID: productID, requestID: requestID) }
            } else {
                Task { @MainActor in self?.noteWorkTreeRequested(productID: productID, requestID: requestID) }
            }
        }
    }

    func noteWorkTreeRequested(productID: String, requestID: String) {
        var fence = boardReadFences[productID] ?? BoardReadFence()
        fence.expire()
        fence.trees[requestID] = .now
        if fence.patchRequestID != nil { fence.patchSuperseded = true }
        fence.queuedPatchItemIDs.removeAll()
        boardReadFences[productID] = fence
    }

    func acceptWorkTree(productID: String, requestID: String?, revision: UInt64?) -> Bool {
        var fence = boardReadFences[productID] ?? BoardReadFence()
        let known = requestID.map { fence.trees.removeValue(forKey: $0) != nil } ?? true
        let fresh = revision.map { $0 >= fence.revision } ?? true
        if known && fresh { fence.revision = revision ?? fence.revision }
        boardReadFences[productID] = fence
        if known && !fresh && productID == currentSelectedProductID {
            refetchBoard(productID: productID)
        }
        return known && fresh
    }

    func noteBoardRevision(_ revision: UInt64?, item: WorkItemPayload) {
        let productID: String
        switch item {
        case .product(let product): productID = product.id
        case .project(let project): productID = project.productID
        case .task(let task), .chore(let task): productID = task.productID
        }
        noteBoardRevision(revision, productID: productID)
    }

    func noteBoardRevision(_ revision: UInt64?, productID: String) {
        guard let revision else { return }
        var fence = boardReadFences[productID] ?? BoardReadFence()
        fence.revision = max(fence.revision, revision)
        boardReadFences[productID] = fence
    }

    /// Drop every outstanding read: the connection that carried them is gone,
    /// so no reply is coming.
    func resetBoardReadFences() {
        boardReadFences.removeAll()
    }

    /// Settle only the read named by the error envelope.
    func failBoardRead(requestID: String?) {
        guard let requestID else { return }
        for (productID, var fence) in boardReadFences {
            fence.trees.removeValue(forKey: requestID)
            let patchFailed = fence.patchRequestID == requestID
            if patchFailed {
                fence.patchRequestID = nil
                fence.queuedPatchItemIDs.removeAll()
            }
            boardReadFences[productID] = fence
            if patchFailed && productID == currentSelectedProductID {
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
        fence.expire(now: now)
        boardReadFences[productID] = fence
        if itemIDs.count > Self.maxRuntimePatchItems || !fence.trees.isEmpty {
            refetchBoard(productID: productID)
            return
        }
        if fence.patchRequestID != nil {
            fence.queuedPatchItemIDs.formUnion(itemIDs)
            boardReadFences[productID] = fence
            return
        }
        fence.patchSentAt = now
        fence.patchSuperseded = false
        let ids = itemIDs.union(fence.queuedPatchItemIDs)
        fence.queuedPatchItemIDs.removeAll()
        fence.patchRequestID = engine.sendGetWorkRuntimePatch(productId: productID, itemIds: ids.sorted())
        boardReadFences[productID] = fence
    }

    /// Reception of a `work_runtime_patch` reply.
    func handleWorkRuntimePatch(
        productID: String,
        cards: [WorkRuntimePatchCard],
        absent: [String],
        isComplete: Bool,
        requestID: String?,
        revision: UInt64?
    ) {
        guard var fence = boardReadFences[productID] else { return }
        guard let requestID, fence.patchRequestID == requestID else { return }
        let superseded = fence.patchSuperseded
        let queued = fence.queuedPatchItemIDs
        fence.patchRequestID = nil
        fence.patchSuperseded = false
        fence.queuedPatchItemIDs.removeAll()
        boardReadFences[productID] = fence

        // A tree requested after the patch will read at least what it read;
        // a product that is no longer on screen is refetched when selected.
        guard productID == currentSelectedProductID else { return }
        if superseded {
            requestRuntimePatch(productID: productID, itemIDs: queued)
            return
        }

        guard revision.map({ $0 >= fence.revision }) ?? true,
              isComplete,
              absent.allSatisfy({ task(withID: $0) == nil }),
              applyRuntimePatch(productID: productID, cards: cards)
        else {
            refetchBoard(productID: productID)
            return
        }
        noteBoardRevision(revision, productID: productID)
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
