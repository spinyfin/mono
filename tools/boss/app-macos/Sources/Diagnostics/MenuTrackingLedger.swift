import Foundation

/// One menu-tracking session the app has seen open and not yet seen close.
///
/// `key` is the identity of the `NSMenu` object, not a stable identifier
/// across launches — it exists only to pair a `didBeginTracking`
/// notification with its `didEndTracking` partner.
struct OpenMenuSession: Equatable, Sendable {
    let key: ObjectIdentifier
    /// Human-identifiable name for the control that owns this menu — see
    /// [[MenuTrackingLedger.label(title:itemTitles:)]]. A SwiftUI `Menu`
    /// bridges to an `NSMenu` with an empty `title`, so the item titles are
    /// usually the only thing that names the offending control.
    let label: String
    let beganAtMs: Int64
    /// How many escalating report tiers this session has already emitted.
    /// Drives the backoff in `dueReports` so an orphaned session logs a
    /// handful of lines rather than one per tick, forever.
    var reportedTiers: Int

    func openMs(asOfMs: Int64) -> Int64 { max(0, asOfMs - beganAtMs) }
}

/// A session that has closed, with the duration it was open for.
struct ClosedMenuSession: Equatable, Sendable {
    let label: String
    let openMs: Int64
}

/// A session's label and how long it has been open — the shape `dueReports`
/// hands to the still-open reporting path. Deliberately not `ClosedMenuSession`:
/// every session this produces is, by construction, still open.
struct OpenSessionReport: Equatable, Sendable {
    let label: String
    let openMs: Int64
}

/// Pure begin/end accounting for AppKit menu-tracking sessions.
///
/// Split out from [[MenuTrackingMonitor]] so the part that decides *what is
/// open, for how long, and when to say so* is testable without opening a real
/// menu — driving a genuine `NSMenu` tracking session from a unit test means
/// entering AppKit's nested modal event loop, which a test process cannot
/// leave deterministically.
///
/// Nesting is expected and is not an error: opening a submenu posts its own
/// begin/end pair inside the parent's, so `openCount` legitimately exceeds 1.
struct MenuTrackingLedger: Equatable, Sendable {
    /// Escalating thresholds (ms open) at which an open session is reported.
    /// A menu a human is reading sits open for a second or two and produces
    /// one or two lines; a session that never ends keeps reporting, slowly.
    static let reportTiersMs: [Int64] = [1_000, 2_000, 5_000, 10_000, 30_000]
    /// After the last tier, report once per this interval.
    static let steadyIntervalMs: Int64 = 60_000
    /// At or beyond this, an open session is worth an `os_log` notice: no
    /// human holds a menu open this long, so the session is a leak candidate.
    static let orphanSuspicionMs: Int64 = 30_000

    private(set) var open: [OpenMenuSession] = []
    /// `didEndTracking` seen with no matching `didBeginTracking`. Non-zero
    /// means the monitor started mid-session (benign, at most once per
    /// launch) or that AppKit ended a session the monitor never saw begin.
    /// The first occurrence is the benign case; anything past it is worth
    /// distinguishing, which is why `end` reports whether a given end was
    /// the first unbalanced one.
    private(set) var unbalancedEndCount: Int = 0

    var openCount: Int { open.count }
    var isTracking: Bool { !open.isEmpty }

    /// Record a session opening. Returns `false` (and changes nothing) if
    /// this exact menu object is already recorded as open — AppKit can post a
    /// second begin for a menu that is re-shown without an intervening end,
    /// and double-counting it would make every later report wrong.
    @discardableResult
    mutating func begin(key: ObjectIdentifier, label: String, atMs: Int64) -> Bool {
        guard !open.contains(where: { $0.key == key }) else { return false }
        open.append(OpenMenuSession(key: key, label: label, beganAtMs: atMs, reportedTiers: 0))
        return true
    }

    /// Record a session closing, returning how long it was open.
    /// `nil` means no matching begin was on file (counted in
    /// `unbalancedEndCount`).
    mutating func end(key: ObjectIdentifier, atMs: Int64) -> ClosedMenuSession? {
        guard let index = open.lastIndex(where: { $0.key == key }) else {
            unbalancedEndCount += 1
            return nil
        }
        let session = open.remove(at: index)
        return ClosedMenuSession(label: session.label, openMs: session.openMs(asOfMs: atMs))
    }

    /// Whether the just-recorded unbalanced end (see `end`) was the first one
    /// this ledger has seen — the benign mid-session-start case — rather than
    /// a later one, which means AppKit ended a session this monitor never saw
    /// begin.
    var isFirstUnbalancedEnd: Bool { unbalancedEndCount == 1 }

    /// The session that has been open longest, if any.
    var longestOpen: OpenMenuSession? {
        open.min(by: { $0.beganAtMs < $1.beganAtMs })
    }

    /// Sessions whose next report tier has come due, marking them reported.
    ///
    /// Call once per tick while anything is open; the returned durations are
    /// what the caller writes to the diagnostics log.
    mutating func dueReports(asOfMs: Int64) -> [OpenSessionReport] {
        var due: [OpenSessionReport] = []
        for index in open.indices {
            let openMs = open[index].openMs(asOfMs: asOfMs)
            var tiers = open[index].reportedTiers
            var reported = false
            while openMs >= Self.thresholdMs(forTier: tiers) {
                tiers += 1
                reported = true
            }
            if reported {
                open[index].reportedTiers = tiers
                due.append(OpenSessionReport(label: open[index].label, openMs: openMs))
            }
        }
        return due
    }

    /// Open-milliseconds at which tier `tier` (0-based) becomes due.
    static func thresholdMs(forTier tier: Int) -> Int64 {
        if tier < reportTiersMs.count { return reportTiersMs[tier] }
        let stepsPastLast = Int64(tier - reportTiersMs.count + 1)
        return (reportTiersMs.last ?? 0) + steadyIntervalMs * stepsPastLast
    }

    /// A name for the control that owns a menu.
    ///
    /// AppKit menus built by SwiftUI's `Menu` / `Picker` bridge carry an empty
    /// `title`, so the item titles are what actually identify the control —
    /// "New Product / New Project / New Task / New Chore" names one specific
    /// toolbar menu, where "(untitled)" names nothing and leaves the next
    /// reader exactly where this investigation started.
    static func label(title: String, itemTitles: [String]) -> String {
        let trimmedTitle = title.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmedTitle.isEmpty { return trimmedTitle }
        let usable = itemTitles
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
        guard !usable.isEmpty else { return "(untitled, no items)" }
        let shown = usable.prefix(maxLabelItems).map(truncateItem)
        let suffix = usable.count > maxLabelItems ? " / …" : ""
        return shown.joined(separator: " / ") + suffix
    }

    /// Items included in a derived label. Enough to identify a control,
    /// few enough that a long menu (e.g. a transcript's per-turn jump list)
    /// does not write a kilobyte per line.
    static let maxLabelItems = 4
    /// Per-item cap in a derived label.
    static let maxLabelItemLength = 40

    private static func truncateItem(_ item: String) -> String {
        guard item.count > maxLabelItemLength else { return item }
        return String(item.prefix(maxLabelItemLength)) + "…"
    }
}
