import Foundation

/// A pending, not-yet-confirmed-by-the-engine edit to one idea's draft.
/// Written to disk on every local-cache autosave tick (see
/// `ChatViewModel+Ideas.swift`) so an app crash or force-quit between that
/// write and the next successful `update_idea` round trip cannot lose
/// keystrokes. A long unsent draft is unrecoverable once the session
/// holding it dies, so the on-disk copy — not optimistic in-memory state —
/// is what makes the buffer survivable.
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

    /// Whether an unsynced local draft file exists for `ideaID`. Used to
    /// assert cache state directly; the sidebar indicator reads
    /// `ChatViewModel.ideaIDsWithPendingLocalDraft`, which mirrors this
    /// state in memory.
    static func hasPendingDraft(ideaID: String, in directory: URL = defaultDirectory) -> Bool {
        FileManager.default.fileExists(atPath: path(for: ideaID, in: directory).path)
    }

    /// All idea ids with an unsynced local draft currently on disk. Used to
    /// seed `ChatViewModel.ideaIDsWithPendingLocalDraft` once, rather than
    /// stat-ing every row's cache file on every render.
    static func allIdeaIDsWithPendingDrafts(in directory: URL = defaultDirectory) -> Set<String> {
        let fm = FileManager.default
        guard let entries = try? fm.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil) else {
            return []
        }
        return Set(entries.filter { $0.pathExtension == "json" }.map { $0.deletingPathExtension().lastPathComponent })
    }
}
