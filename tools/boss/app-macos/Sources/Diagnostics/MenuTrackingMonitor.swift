import AppKit
import Foundation
import os

/// Records when an AppKit menu-tracking session opens and closes, and mirrors
/// each transition into [[TerminalLoopLog]] as a [[MenuTrackingSample]].
///
/// **Why this exists.** A menu-tracking session is a nested modal event loop.
/// While one runs, `sample` shows the main thread inside
/// `-[NSPopUpButtonCell trackMouse:]` → a menu tracking session rather than in
/// the app's normal idle loop, and the app's steady state is unmeasurable —
/// the repo's own UI-performance work had to discard its baseline for exactly
/// this reason (see `tools/boss/docs/designs/boss-ui-performance-improvements.md`,
/// "Sampling caveat that constrains every comparison"). An operator then hit
/// the same thing from the other side: repeated samples that looked like a
/// menu was permanently open, with no menu visible on screen.
///
/// A profile cannot tell those two apart. It shows a nested loop running; it
/// cannot say whether a menu is genuinely open behind it, which control owns
/// it, or whether the loop outlived the menu. Those are app-state questions,
/// and this monitor is the app answering them itself:
///
/// - a `begin` with a matching `end` is a menu somebody opened and closed;
/// - a `begin` with no `end`, followed by `still_open` records whose `open_ms`
///   keeps growing, is an orphaned session — and `menu` names the control;
/// - a profile showing a nested menu loop while this log has nothing open is
///   an AppKit loop that outlived its menu, which is a third thing again.
///
/// **Cost.** Two notification observers and nothing else while no menu is
/// open: the 1 Hz probe is started on the first `begin` and torn down on the
/// last `end`. That is deliberate — this repo has already paid for a
/// diagnostic that cost ~7% of a core the moment it was switched on
/// ([[MainThreadStallMonitor]]), and a menu-state probe that distorts the
/// idle profile would be measuring its own overhead.
///
/// The probe timer is registered in `.common` run loop modes on purpose: menu
/// tracking runs the main run loop in `NSEventTrackingRunLoopMode`, and a
/// default-mode timer does not fire there at all — it would go quiet for
/// precisely the interval it exists to observe.
@MainActor
final class MenuTrackingMonitor {
    static let shared = MenuTrackingMonitor()

    private let log: TerminalLoopLog
    private let logger = Logger(subsystem: "com.boss.app", category: "menu-tracking")
    private var ledger = MenuTrackingLedger()
    private var beginObserver: NSObjectProtocol?
    private var endObserver: NSObjectProtocol?
    private var probe: Timer?
    /// One notice per session that crosses the suspicion threshold, keyed by
    /// menu identity, so a genuinely stuck session logs once rather than once
    /// per tier.
    private var noticedOrphans: Set<Int> = []

    init(log: TerminalLoopLog = .shared) {
        self.log = log
    }

    /// Whether any menu-tracking session is currently open. Reflects
    /// AppKit's own begin/end notifications, not a guess from a profile.
    var isTracking: Bool { ledger.isTracking }

    /// Snapshot of open sessions, longest-open first.
    func openSessions(asOfMs: Int64 = MenuTrackingMonitor.nowMs()) -> [OpenMenuSession] {
        ledger.open.sorted { $0.openMs(asOfMs: asOfMs) > $1.openMs(asOfMs: asOfMs) }
    }

    /// Start observing menu tracking. Idempotent; call once at launch
    /// (next to `DisplayPowerMonitor.shared.start()`).
    func start() {
        guard beginObserver == nil else { return }
        let center = NotificationCenter.default
        // `queue: nil`, not `.main`: AppKit posts both of these on the main
        // thread, and a nil queue delivers synchronously on the posting
        // thread. Routing them through `OperationQueue.main` instead would
        // add an async hop that lands *inside* the nested tracking loop the
        // records are describing, so every timestamp would be late by
        // whatever that loop was busy with — precisely the interval being
        // measured. `onMain` keeps the off-thread case safe anyway.
        //
        // The `NSMenu` itself is not `Sendable`, so each observer reduces it
        // to its identity and label — both `Sendable` — before touching the
        // main actor. The menu object never crosses the boundary.
        beginObserver = center.addObserver(
            forName: NSMenu.didBeginTrackingNotification,
            object: nil,
            queue: nil
        ) { [weak self] note in
            guard let menu = note.object as? NSMenu else { return }
            let key = Self.key(for: menu)
            let label = Self.label(for: menu)
            Self.onMain { self?.handleBegin(key: key, label: label) }
        }
        endObserver = center.addObserver(
            forName: NSMenu.didEndTrackingNotification,
            object: nil,
            queue: nil
        ) { [weak self] note in
            guard let menu = note.object as? NSMenu else { return }
            let key = Self.key(for: menu)
            let label = Self.label(for: menu)
            Self.onMain { self?.handleEnd(key: key, label: label) }
        }
    }

    func stop() {
        let center = NotificationCenter.default
        if let beginObserver { center.removeObserver(beginObserver) }
        if let endObserver { center.removeObserver(endObserver) }
        beginObserver = nil
        endObserver = nil
        stopProbe()
        // Drop any session still on file. Keeping it would leave `isTracking`
        // claiming a menu is open that this monitor has stopped being able to
        // see close — a diagnostic that lies about its own blind spot is
        // worse than one that says nothing.
        ledger = MenuTrackingLedger()
        noticedOrphans.removeAll()
    }

    // MARK: - Transitions

    private func handleBegin(key: Int, label: String) {
        let now = Self.nowMs()
        guard ledger.begin(key: key, label: label, atMs: now) else { return }
        record(event: "begin", menu: label, openMs: nil, at: now)
        startProbeIfNeeded()
    }

    private func handleEnd(key: Int, label: String) {
        let now = Self.nowMs()
        noticedOrphans.remove(key)
        guard let closed = ledger.end(key: key, atMs: now) else {
            // No begin on file. Expected at most once per launch if the
            // monitor started while a menu was already up; otherwise it means
            // AppKit ended a session this app never saw open, which is itself
            // worth having in the log rather than dropping.
            record(event: "unbalanced_end", menu: label, openMs: nil, at: now)
            return
        }
        record(event: "end", menu: closed.label, openMs: closed.openMs, at: now)
        if !ledger.isTracking { stopProbe() }
    }

    // MARK: - Probe

    private func startProbeIfNeeded() {
        guard probe == nil else { return }
        let timer = Timer(timeInterval: 1.0, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.tick() }
        }
        // `.common` so the probe keeps firing inside the nested tracking loop
        // — see the type comment.
        RunLoop.main.add(timer, forMode: .common)
        probe = timer
    }

    private func stopProbe() {
        probe?.invalidate()
        probe = nil
    }

    private func tick() {
        let now = Self.nowMs()
        for due in ledger.dueReports(asOfMs: now) {
            record(event: "still_open", menu: due.label, openMs: due.openMs, at: now)
        }
        guard let longest = ledger.longestOpen(asOfMs: now),
              longest.openMs(asOfMs: now) >= MenuTrackingLedger.orphanSuspicionMs,
              !noticedOrphans.contains(longest.key)
        else { return }
        noticedOrphans.insert(longest.key)
        logger.notice(
            """
            menu-tracking: session open \(longest.openMs(asOfMs: now), privacy: .public)ms \
            with no end — menu=\(longest.label, privacy: .public) \
            openCount=\(self.ledger.openCount, privacy: .public)
            """
        )
    }

    // MARK: - Helpers

    private func record(event: String, menu: String, openMs: Int64?, at tsEpochMs: Int64) {
        log.record(
            MenuTrackingSample(
                tsEpochMs: tsEpochMs,
                event: event,
                menu: menu,
                openMs: openMs,
                openCount: ledger.openCount,
                runloopMode: Self.currentRunLoopMode()
            )
        )
    }

    nonisolated static func nowMs() -> Int64 { Int64(Date().timeIntervalSince1970 * 1000) }

    /// Run `body` on the main actor, synchronously when already there.
    ///
    /// Staying synchronous matters: these notifications arrive on the main
    /// thread while a nested tracking loop is running, and an async hop would
    /// be serviced by that loop rather than ahead of it.
    nonisolated private static func onMain(_ body: @escaping @MainActor () -> Void) {
        if Thread.isMainThread {
            MainActor.assumeIsolated { body() }
        } else {
            Task { @MainActor in body() }
        }
    }

    /// Identity of the menu object, used only to pair a begin with its end.
    ///
    /// Address-derived, so a deallocated menu's identity can in principle be
    /// reused by a later one. The only consequence is that a *already
    /// orphaned* session could swallow the `begin` of a new menu allocated at
    /// the same address (the duplicate-begin guard drops it), which biases
    /// toward under-reporting rather than inventing sessions.
    nonisolated private static func key(for menu: NSMenu) -> Int {
        ObjectIdentifier(menu).hashValue
    }

    nonisolated private static func label(for menu: NSMenu) -> String {
        MenuTrackingLedger.label(
            title: menu.title,
            itemTitles: menu.items.filter { !$0.isSeparatorItem }.map(\.title)
        )
    }

    /// The main run loop's current mode, or `nil` if it is not running one.
    nonisolated private static func currentRunLoopMode() -> String? {
        CFRunLoopCopyCurrentMode(CFRunLoopGetMain()).map { $0.rawValue as String }
    }
}
