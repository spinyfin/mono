import Foundation

/// A pending, not-yet-confirmed-by-the-engine edit to one idea's draft.
/// Written to disk on every local-cache autosave tick (see
/// `ChatViewModel+Ideas.swift`) so an app crash or force-quit between that
/// write and the next successful `update_idea` round trip cannot lose
/// keystrokes — the acceptance criterion this feature exists for (the
/// Ideas project description: a coordinator session once died
/// mid-composition and the unsent buffer was gone for good).
struct IdeaDraftCacheEntry: Codable, Equatable {
    let ideaID: String
    let productID: String
    var name: String
    var body: String
    var savedAt: Date

    enum CodingKeys: String, CodingKey {
        case ideaID = "idea_id"
        case productID = "product_id"
        case name
        case body
        case savedAt = "saved_at"
    }
}

/// File-based write-through cache for in-progress idea edits — the
/// offline crash floor the Ideas autosave design requires. One JSON file
/// per idea, named by idea id, under Application Support.
///
/// Never the source of truth: the engine's `ideas` table is authoritative.
/// A cache file exists only while an edit has not yet been confirmed by an
/// `idea_updated` echo whose body matches what was written here — see
/// `ChatViewModel.handleIdeaUpdated`.
enum IdeaDraftCache {
    /// Production cache directory: `~/Library/Application Support/Boss/idea-drafts`.
    /// Every call site defaults to this; tests pass a scratch directory
    /// instead of touching real state (see `BossPaneModel`'s
    /// `ensureBossWorkingDirectory` for the same Application-Support
    /// pattern this mirrors).
    static var defaultDirectory: URL {
        let fm = FileManager.default
        let appSupport = fm
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first ?? URL(fileURLWithPath: NSHomeDirectory())
                .appendingPathComponent("Library/Application Support")
        return appSupport.appendingPathComponent("Boss/idea-drafts")
    }

    private static func path(for ideaID: String, in directory: URL) -> URL {
        directory.appendingPathComponent("\(ideaID).json")
    }

    private static let encoder: JSONEncoder = {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        return encoder
    }()

    private static let decoder: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        return decoder
    }()

    /// Write (or overwrite) the cached draft for `entry.ideaID`. Best
    /// effort: a failure here is silently swallowed — there is no
    /// user-facing action to take beyond the in-memory draft the editor
    /// still holds — but this must never throw or block typing.
    static func write(_ entry: IdeaDraftCacheEntry, in directory: URL = defaultDirectory) {
        let fm = FileManager.default
        try? fm.createDirectory(at: directory, withIntermediateDirectories: true)
        guard let data = try? encoder.encode(entry) else { return }
        try? data.write(to: path(for: entry.ideaID, in: directory), options: .atomic)
    }

    static func read(ideaID: String, in directory: URL = defaultDirectory) -> IdeaDraftCacheEntry? {
        guard let data = try? Data(contentsOf: path(for: ideaID, in: directory)) else { return nil }
        return try? decoder.decode(IdeaDraftCacheEntry.self, from: data)
    }

    /// Remove the cached draft once the engine has confirmed it persisted
    /// this exact edit. Best effort — a failed delete just means the next
    /// reconciliation re-checks an already-synced file, which is harmless.
    static func clear(ideaID: String, in directory: URL = defaultDirectory) {
        try? FileManager.default.removeItem(at: path(for: ideaID, in: directory))
    }

    /// Whether an unsynced local draft exists for `ideaID`. Drives the
    /// sidebar's "unsaved local changes" indicator so a draft that never
    /// got reconciled (the operator crashed, then opened a different idea)
    /// stays visible instead of silently waiting to be rediscovered.
    static func hasPendingDraft(ideaID: String, in directory: URL = defaultDirectory) -> Bool {
        FileManager.default.fileExists(atPath: path(for: ideaID, in: directory).path)
    }
}
