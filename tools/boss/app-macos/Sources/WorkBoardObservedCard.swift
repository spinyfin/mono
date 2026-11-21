import Combine
import SwiftUI

struct WorkCardRenderValue: Equatable {
    let task: WorkTask
    let snapshot: WorkCardSnapshot
    let reviewers: [WorkerLiveState]
    let isRevealed: Bool
    let dragRefusal: ChatViewModel.DragRefusalNotice?
    let mergeFeedbackMessage: String?
    let isRequestingMerge: Bool
    let revealGeneration: UUID
    let isRevealTarget: Bool
    let revisionComposerPresented: Bool
    let findingsTooltip: String
    let parkMenuAction: ParkMenuAction
    let runtime: WorkTaskRuntime?
}

/// INVARIANT: `refresh` (and the snapshot it builds) may read only model
/// state that `WorkPresentationUpdates` observes (its `observe(model.$...)`
/// list, `invalidateWorkCache`, or the live store). A new card input that is
/// not added there leaves cards stale; `testEveryCardInputRepublishesItsRow`
/// guards the current list.
@MainActor
final class WorkCardRenderState: ObservableObject {
    @Published private(set) var value: WorkCardRenderValue?
    private var subscription: AnyCancellable?
    private var lastLiveState: WorkerLiveState?
    private var lastReviewers: [WorkerLiveState] = []
    private var style: KanbanBoardStyle

    init(model: ChatViewModel, id: String, column: WorkBoardColumnKey, style: KanbanBoardStyle) {
        self.style = style
        refresh(model: model, id: id, column: column, liveOnly: false)
        subscription = model.presentationUpdates.cards.sink { [weak self, weak model] liveOnly in
            guard let model else { return }
            self?.refresh(model: model, id: id, column: column, liveOnly: liveOnly)
        }
    }

    func updateStyle(_ style: KanbanBoardStyle, model: ChatViewModel, id: String, column: WorkBoardColumnKey) {
        self.style = style
        refresh(model: model, id: id, column: column, liveOnly: false)
    }

    private func refresh(model: ChatViewModel, id: String, column: WorkBoardColumnKey, liveOnly: Bool) {
        if liveOnly && column != .doing && column != .review { return }
        guard let task = model.task(withID: id) else {
            if value != nil { value = nil }
            return
        }
        let live = column == .doing ? model.workerLiveState(forTaskID: id) : nil
        let reviewers = (column == .doing || column == .review)
            ? WorkBoardSectionItemsView.reviewWorkers(for: task, index: model.presentationUpdates.reviewersByPR) : []
        if liveOnly && live == lastLiveState && reviewers == lastReviewers { return }
        lastLiveState = live
        lastReviewers = reviewers
        let snapshot = model.workCardSnapshot(
            for: task, column: column,
            isSelected: model.navigator.mode == .kanban && model.selectedWorkCardID == id,
            isFrontierHighlighted: model.depFrontierHighlightIDs.contains(id) || model.selectedRevisionParentID == id,
            boardStyle: style, liveState: live)
        let next = WorkCardRenderValue(
            task: task, snapshot: snapshot, reviewers: reviewers,
            isRevealed: model.revealHighlightID == id,
            dragRefusal: model.dragRefusalNotice?.taskID == id ? model.dragRefusalNotice : nil,
            mergeFeedbackMessage: model.mergeFeedbackNotice?.taskID == id ? model.mergeFeedbackNotice?.message : nil,
            isRequestingMerge: model.mergingWhenReadyIDs.contains(id),
            revealGeneration: model.revealGeneration, isRevealTarget: model.revealScrollTarget == id,
            revisionComposerPresented: model.revisionComposerTaskID == id,
            findingsTooltip: model.aiReviewFindingsTooltipNote(revisionID: task.aiReviewFindingsRevisionId),
            parkMenuAction: model.parkMenuAction(for: task), runtime: model.taskRuntime(for: id))
        if next != value { value = next }
    }
}

/// The list carries ids; only a mounted row observes its own render value.
struct WorkBoardObservedCard: View, @MainActor Equatable {
    let id: String
    let column: WorkBoardColumnKey
    let style: KanbanBoardStyle
    let model: ChatViewModel
    @StateObject private var state: WorkCardRenderState

    init(id: String, column: WorkBoardColumnKey, style: KanbanBoardStyle, model: ChatViewModel) {
        self.id = id
        self.column = column
        self.style = style
        self.model = model
        _state = StateObject(wrappedValue: WorkCardRenderState(model: model, id: id, column: column, style: style))
    }

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.id == rhs.id && lhs.column == rhs.column && lhs.style == rhs.style && lhs.model === rhs.model
    }

    var body: some View {
        if let value = state.value {
            WorkBoardCardItem(
                task: value.task, column: column, snapshot: value.snapshot,
                reviewers: value.reviewers, model: model,
                revisionHighlight: model.revisionHighlightState(for: id),
                isRevealed: value.isRevealed, dragRefusal: value.dragRefusal,
                mergeFeedbackMessage: value.mergeFeedbackMessage,
                isRequestingMerge: value.isRequestingMerge)
                .background {
                    RevealCardViewport(cardID: id, generation: value.revealGeneration,
                        isTarget: value.isRevealTarget, model: model)
                        .allowsHitTesting(false)
                }
                .onChange(of: style) { _, style in
                    state.updateStyle(style, model: model, id: id, column: column)
                }
        }
    }
}
