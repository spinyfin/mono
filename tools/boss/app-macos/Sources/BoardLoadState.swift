import Foundation

/// What the engine last said about a cold board build it is running for a
/// product (`board_population_progress`). Display-only: it never gates
/// whether a board is shown, only what the loading state says.
struct BoardLoadProgress: Equatable {
    /// The `population_timing` segment the engine is entering (e.g.
    /// `db.tasks`); `nil` right after the build started.
    var stage: String?
    /// Engine-side milliseconds since it received the request.
    var engineElapsedMs: Int
    /// Cards (tasks + chores) the engine has counted so far, once known.
    var items: Int?

    /// A short, user-facing description of the current stage.
    var stageDescription: String? {
        guard let stage else { return nil }
        return Self.stageDescriptions[stage] ?? stage
    }

    private static let stageDescriptions: [String: String] = [
        "db.product": "reading the product",
        "db.projects": "reading projects",
        "db.tasks": "reading tasks",
        "db.chores": "reading chores",
        "db.task_runtimes": "reading task runtimes",
        "db.dependencies": "reading dependencies",
        "db.ai_reviewing": "reading review activity",
        "db.ai_review_state": "reading review state",
        "db.has_attachments": "reading attachments",
        "db.review_guide_state": "reading review guides",
        "db.doc_pointers": "resolving document links",
        "serialize": "serializing board",
        "socket_write": "sending board",
        "assemble": "assembling cards",
    ]
}

/// Which products have received their first board snapshot, and for the
/// ones still waiting, since when and how far the engine says it has got.
///
/// Existing columns remain visible during reconnects, with an inline refresh
/// indicator until the next accepted snapshot.
struct BoardLoadTracker: Equatable {
    private var refreshing: Set<String> = []
    private var failed: Set<String> = []
    private var loaded: Set<String> = []
    private var requestedAt: [String: Date] = [:]
    private var progress: [String: BoardLoadProgress] = [:]

    func isRefreshing(_ productID: String) -> Bool { refreshing.contains(productID) }
    func hasFailed(_ productID: String) -> Bool { failed.contains(productID) }

    mutating func noteReadFailed(_ productID: String) {
        requestedAt[productID] = nil
        progress[productID] = nil
        refreshing.remove(productID)
        failed.insert(productID)
    }

    func isLoaded(_ productID: String) -> Bool {
        loaded.contains(productID)
    }

    /// When the wait for `productID`'s next snapshot began (its first
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
        if failed.remove(productID) != nil, loaded.contains(productID) {
            refreshing.insert(productID)
        }
        guard !loaded.contains(productID) || refreshing.contains(productID) else { return }
        guard requestedAt[productID] == nil else { return }
        progress[productID] = nil
        requestedAt[productID] = date
    }

    /// The engine reported cold-build progress. Late reports for a board
    /// that already arrived, and reports that move backwards, are dropped.
    mutating func noteProgress(_ productID: String, _ update: BoardLoadProgress, started: Bool = false) {
        if started {
            progress[productID] = nil
            failed.remove(productID)
            if loaded.contains(productID) { refreshing.insert(productID) }
            if requestedAt[productID] == nil { requestedAt[productID] = Date() }
        }
        guard !failed.contains(productID),
              !loaded.contains(productID) || refreshing.contains(productID) else { return }
        if let current = progress[productID], update.engineElapsedMs < current.engineElapsedMs { return }
        var next = update
        next.items = update.items ?? progress[productID]?.items
        progress[productID] = next
    }

    /// A snapshot for `productID` was applied.
    mutating func noteSnapshot(_ productID: String) {
        loaded.insert(productID)
        refreshing.remove(productID)
        failed.remove(productID)
        requestedAt[productID] = nil
        progress[productID] = nil
    }

    /// The connection dropped. Reads in flight are gone and the engine may
    /// have restarted, so what it reported about a build it was running no
    /// longer applies; the reconnect re-requests and re-starts the clock.
    mutating func noteDisconnected() {
        refreshing.formUnion(loaded)
        failed.removeAll()
        requestedAt.removeAll()
        progress.removeAll()
    }
}
