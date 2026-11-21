import Combine
import Foundation

/// Rendering has a frame-sized trailing edge; engine events still apply in
/// arrival order immediately. One timer serves every mounted board/sidebar row.
@MainActor
final class WorkPresentationUpdates {
    private weak var model: ChatViewModel?
    private(set) var reviewersByPR: [ReviewPRIdentity: [WorkerLiveState]] = [:]
    static let clockTickSeconds: TimeInterval = 15
    let chrome = PassthroughSubject<Void, Never>()
    let cards = PassthroughSubject<Bool, Never>()
    private var subscriptions: Set<AnyCancellable> = []
    private var pending: Task<Void, Never>?
    private var chromeDirty = false
    private var cardsDirty = false
    private var liveDirty = false

    init(model: ChatViewModel) {
        self.model = model
        reviewersByPR = WorkBoardSectionItemsView.reviewerIndex(states: Array(model.liveWorkerStates.byRunID.values))
        model.objectWillChange.sink { [weak self] in
            self?.chromeDirty = true
            self?.schedule()
        }.store(in: &subscriptions)
        model.liveWorkerStates.objectWillChange.sink { [weak self] in
            self?.liveDirty = true
            self?.schedule()
        }.store(in: &subscriptions)
        observe(model.$products)
        observe(model.$projectsByProductID)
        observe(model.$tasksByProjectID)
        observe(model.$choresByProductID)
        observe(model.$productLevelTasksByProductID)
        observe(model.$productLevelRevisionsByProductID)
        observe(model.$dependenciesByProductID)
        observe(model.$taskRuntimesByID)
        observe(model.$selectedWorkProductID)
        observe(model.$selectedProjectFilterIDs)
        observe(model.$filterToChoresOnly)
        observe(model.$includeChores)
        observe(model.$showBlockedOnly)
        observe(model.$showArchivedProjects)
        observe(model.$showParked)
        observe(model.$workSearchText)
        observe(model.$workBoardGrouping)
        observe(model.$navigator)
        observe(model.$selectedWorkCardID)
        observe(model.$revealHighlightID)
        observe(model.$revealScrollTarget)
        observe(model.$revealGeneration)
        observe(model.$depFrontierHighlightIDs)
        observe(model.$dragRefusalNotice)
        observe(model.$mergeFeedbackNotice)
        observe(model.$mergingWhenReadyIDs)
        observe(model.$queuedDispatchByID)
        observe(model.$doingSectionsByID)
        observe(model.$designDocStateByProjectID)
        observe(model.$deferredScopeAttentionsByProductID)
        observe(model.$deferredScopeActionInFlightIDs)
        observe(model.$operatorAnswerInFlightByTaskID)
        observe(model.$operatorAnswerErrorByTaskID)
        observe(model.$recentlyClearedCIPRs)
        observe(model.$recentlyClearedConflictPRs)
        observe(model.$ciFailureBadges)
        observe(model.$conflictResolutions)
        observe(model.$ciRemediations)
        observe(model.$retryingReviewGuideRootTaskIDs)
        observe(model.$revisionComposerTaskID)
        // Snapshots bake in `now`-relative text (countdowns, parked chip,
        // badge freshness windows). Rebuild periodically; the render value's
        // equality gate means only cards whose text changed republish.
        Timer.publish(every: Self.clockTickSeconds, on: .main, in: .common).autoconnect().sink { [weak self] _ in
            self?.cardsDirty = true
            self?.schedule()
        }.store(in: &subscriptions)
    }

    private func observe<Value>(_ publisher: Published<Value>.Publisher) {
        publisher.dropFirst().sink { [weak self] _ in
            self?.workDataChanged()
        }.store(in: &subscriptions)
    }

    func workDataChanged() {
        chromeDirty = true
        cardsDirty = true
        schedule()
    }

    private func schedule() {
        guard pending == nil else { return }
        pending = Task { [weak self] in
            do { try await Task.sleep(for: .milliseconds(16)) } catch { return }
            self?.flush()
        }
    }

    /// Also used by deterministic render tests; reads happen after all setters
    /// and cache invalidations, never in Published's willSet callback.
    func flush() {
        pending?.cancel()
        pending = nil
        let rebuildChrome = chromeDirty
        let rebuildCards = cardsDirty
        let rebuildLive = liveDirty
        chromeDirty = false
        cardsDirty = false
        liveDirty = false
        if rebuildLive, let model {
            reviewersByPR = WorkBoardSectionItemsView.reviewerIndex(states: Array(model.liveWorkerStates.byRunID.values))
        }
        if rebuildChrome { chrome.send() }
        if rebuildCards || rebuildLive { cards.send(!rebuildCards) }
    }
}

/// A value boundary prevents an unrelated model publish from entering SwiftUI.
@MainActor
final class WorkPresentationValue<Value: Equatable>: ObservableObject {
    @Published private(set) var value: Value
    private var subscription: AnyCancellable?

    init(updates: WorkPresentationUpdates, read: @escaping () -> Value) {
        value = read()
        subscription = updates.chrome.sink { [weak self] in
            guard let self else { return }
            let next = read()
            if next != value { value = next }
        }
    }
}
