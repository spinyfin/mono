import Combine

/// Per-card revision-hover state. A card observes only its own cell, so a
/// highlight transition invalidates the cards whose chrome actually changes
/// instead of publishing through the board's whole `ChatViewModel`.
@MainActor
final class WorkBoardRevisionHighlightState: ObservableObject {
    let taskID: String
    @Published private(set) var isHighlighted: Bool

    init(taskID: String, isHighlighted: Bool) {
        self.taskID = taskID
        self.isHighlighted = isHighlighted
    }

    fileprivate func setHighlighted(_ next: Bool) {
        guard next != isHighlighted else { return }
        isHighlighted = next
    }
}

/// Routes revision-hover changes to stable per-task cells. The registry keeps
/// weak references so cards that leave the work tree do not accumulate for the
/// lifetime of `ChatViewModel`; mounted views retain the cells they observe.
@MainActor
final class WorkBoardRevisionHighlightStore {
    /// Sweep zeroed weak entries after this many allocations. Large enough
    /// that a full board of mounted cards never prunes mid-hover, small enough
    /// that churn without strong holders cannot grow the registry unbounded.
    private static let pruneAfterCreations = 256

    private final class WeakState {
        weak var value: WorkBoardRevisionHighlightState?

        init(_ value: WorkBoardRevisionHighlightState) {
            self.value = value
        }
    }

    private var statesByTaskID: [String: WeakState] = [:]
    private var statesCreatedSincePrune = 0
    private(set) var highlightedIDs: Set<String> = []

    /// Live (non-zeroed) registry entries. Used by tests to assert the weak
    /// lifetime contract without racing MainActor deallocation timing.
    var liveRegisteredStateCount: Int {
        statesByTaskID.values.reduce(0) { partial, box in
            partial + (box.value != nil ? 1 : 0)
        }
    }

    func state(for taskID: String) -> WorkBoardRevisionHighlightState {
        if let existing = statesByTaskID[taskID]?.value {
            return existing
        }
        let state = WorkBoardRevisionHighlightState(
            taskID: taskID,
            isHighlighted: highlightedIDs.contains(taskID)
        )
        statesByTaskID[taskID] = WeakState(state)
        statesCreatedSincePrune += 1
        if statesCreatedSincePrune >= Self.pruneAfterCreations {
            statesByTaskID = statesByTaskID.filter { $0.value.value != nil }
            statesCreatedSincePrune = 0
        }
        return state
    }

    func setHighlightedIDs(_ next: Set<String>) {
        guard next != highlightedIDs else { return }
        let changedIDs = highlightedIDs.symmetricDifference(next)
        highlightedIDs = next
        for taskID in changedIDs {
            statesByTaskID[taskID]?.value?.setHighlighted(next.contains(taskID))
        }
    }
}
