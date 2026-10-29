import Foundation

/// What the engine last said about a cold board build it is running for a
/// product (`board_population_progress`). Display-only: it never gates
/// whether a board is shown, only what the loading state says.
struct BoardLoadProgress: Equatable {
    /// The `population_timing` segment the engine just finished (e.g.
    /// `db.tasks`); `nil` right after the build started.
    var stage: String?
    /// Engine-side milliseconds since it received the request.
    var engineElapsedMs: Int
    /// Cards (tasks + chores) the engine has counted so far, once known.
    var items: Int?

    /// A short, user-facing description of the last finished stage.
    var stageDescription: String? {
        guard let stage else { return nil }
        return Self.stageDescriptions[stage] ?? stage
    }

    private static let stageDescriptions: [String: String] = [
        "db.product": "read the product",
        "db.projects": "read projects",
        "db.tasks": "read tasks",
        "db.chores": "read chores",
        "db.task_runtimes": "read task runtimes",
        "db.dependencies": "read dependencies",
        "db.ai_reviewing": "read review activity",
        "db.ai_review_state": "read review state",
        "db.has_attachments": "read attachments",
        "db.review_guide_state": "read review guides",
        "db.doc_pointers": "resolved document links",
        "assemble": "assembled cards",
    ]
}

/// Which products have received their first board snapshot, and for the
/// ones still waiting, since when and how far the engine says it has got.
///
/// A product that is not `isLoaded` has *no* board data yet, so an empty
/// column there means "not loaded", never "no items". Once a snapshot lands
/// the product stays loaded for the session: a reconnect keeps showing the
/// last snapshot while the refetch runs, and only products that never got
/// one go back to waiting.
struct BoardLoadTracker: Equatable {
    private var loaded: Set<String> = []
    private var requestedAt: [String: Date] = [:]
    private var progress: [String: BoardLoadProgress] = [:]

    func isLoaded(_ productID: String) -> Bool {
        loaded.contains(productID)
    }

    /// When the wait for `productID`'s first snapshot began (its first
    /// request), or `nil` if none has been sent since the app last connected.
    func waitingSince(_ productID: String) -> Date? {
        requestedAt[productID]
    }

    func progress(for productID: String) -> BoardLoadProgress? {
        progress[productID]
    }

    /// A board read was sent. The first one starts the wait clock; repeats
    /// while still waiting (a refetch, a product switch back) keep it, since
    /// the user has been looking at the same absent board the whole time.
    mutating func noteRequested(_ productID: String, at date: Date = Date()) {
        guard !loaded.contains(productID), requestedAt[productID] == nil else { return }
        requestedAt[productID] = date
    }

    /// The engine reported cold-build progress. Late reports for a board
    /// that already arrived, and reports that move backwards, are dropped.
    mutating func noteProgress(_ productID: String, _ update: BoardLoadProgress) {
        guard !loaded.contains(productID) else { return }
        if let current = progress[productID], update.engineElapsedMs < current.engineElapsedMs { return }
        var next = update
        next.items = update.items ?? progress[productID]?.items
        progress[productID] = next
    }

    /// A snapshot for `productID` was applied.
    mutating func noteSnapshot(_ productID: String) {
        loaded.insert(productID)
        requestedAt[productID] = nil
        progress[productID] = nil
    }

    /// The connection dropped. Reads in flight are gone and the engine may
    /// have restarted, so what it reported about a build it was running no
    /// longer applies; the reconnect re-requests and re-starts the clock.
    mutating func noteDisconnected() {
        requestedAt.removeAll()
        progress.removeAll()
    }
}
