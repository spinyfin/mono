import Foundation

/// The board's "not loaded yet" state. Until a product's first
/// `work_tree` snapshot has been applied, the columns hold no data, so the
/// board shows a loading state rather than the zero counts and "No items" a
/// genuinely empty board gets. See [[BoardLoadTracker]].
extension ChatViewModel {
    /// Apply `change` to the load tracker, publishing only if it altered
    /// anything: most board reads are for products that are already loaded,
    /// and a no-op write to a `@Published` property would still re-render
    /// every view observing the model.
    func updateBoardLoad(_ change: (inout BoardLoadTracker) -> Void) {
        var next = boardLoad
        change(&next)
        if next != boardLoad { boardLoad = next }
    }

    /// Whether `productID`'s board has received its first snapshot.
    func isBoardLoaded(_ productID: String) -> Bool {
        boardLoad.isLoaded(productID)
    }
}
