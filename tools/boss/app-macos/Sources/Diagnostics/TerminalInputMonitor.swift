import AppKit
import Foundation
import OSLog
import os

/// Always-on instrumentation for keyboard delivery to the embedded
/// terminal panes. Records into [[TerminalInputLog]] so an intermittent
/// "typed a key, heard a beep, the key never arrived" report can be
/// attributed from the log instead of folklore.
///
/// What it watches, and which suspect each covers:
///
/// 1. **First-responder changes** on every window that hosts a terminal
///    pane (KVO on `NSWindow.firstResponder`, old/new responder types).
///    A `keyDown` whose first responder is not the terminal view travels
///    the responder chain and — when nothing handles it — ends in
///    `NSResponder.noResponder(for:)`, which calls `NSBeep`. That is the
///    AppKit beep: a drop *and* a beep from one event.
/// 2. **Key window changes** for the same windows, so a transient panel
///    stealing key status shows up next to the dropped key.
/// 3. **Every `keyDown` not delivered to a terminal pane** in a
///    terminal-hosting window, via a local `NSEvent` monitor installed at
///    launch (so it runs before any other local monitor that might swallow
///    the event). The monitor never consumes or alters events.
/// 4. **Main-thread stalls** over [[stallThresholdMs]], measured by how late
///    a main-queue heartbeat fires. No thread suspension, no backtrace —
///    [[MainThreadStallMonitor]] (opt-in) does that; this one is cheap
///    enough to leave on so a stall can be lined up with a dropped key.
/// 5. **libghostty's own warnings/errors** from unified logging, mirrored
///    into the same file. libghostty's pty writer logs `write error: …`
///    there when a write to the pty fails, and the embedder has no other
///    view of that path (the C API returns `void` from
///    `ghostty_surface_text` and only consumed/not-consumed from
///    `ghostty_surface_key`).
///
/// The pane views themselves add the rest of the vocabulary (focus,
/// window attach/detach, no-surface drops, not-consumed keys, bells) —
/// see `GhosttyTerminalHostView` and `GhosttyRuntime`.
///
/// Started once from `applicationDidFinishLaunching` for non-isolated
/// instances. `registerTerminalWindow` is called by every pane when it
/// joins a window; it is a no-op until `start()` has run, so capture /
/// isolated instances pay nothing.
@MainActor
final class TerminalInputMonitor: NSObject {
    static let shared = TerminalInputMonitor()

    /// Main-thread unavailability that counts as a stall, in ms. Matches
    /// `MainThreadStallMonitor.Config.thresholdMs` so the two surfaces agree
    /// on what "a hang" is.
    static let stallThresholdMs: Double = 250
    static let heartbeatIntervalMs: Double = 100

    private let log: TerminalInputLog
    private let libghosttyMirror: LibghosttyLogMirror
    private var started = false
    private var keyMonitor: Any?
    private var heartbeat: DispatchSourceTimer?
    private var lastBeatNanos: UInt64 = 0
    private var windows: [ObjectIdentifier: WindowEntry] = [:]
    /// Windows a pane joined before `start()` ran; registered on start.
    private var pendingWindows: [WeakWindow] = []

    private struct WeakWindow {
        weak var window: NSWindow?
    }

    /// Per-window observation state. `window` is weak so a closed window is
    /// pruned rather than retained by the monitor.
    @MainActor
    private final class WindowEntry {
        weak var window: NSWindow?
        let keyContext = TerminalInputKeyContext()
        var observation: NSKeyValueObservation?
        var lastResponderChangeNanos: UInt64 = 0
        /// Coalescing state for keys delivered to a legitimate text field:
        /// one `key_to_text_input` line per focus episode, no key codes.
        var textInputKeys = 0
        var textInputResponder: String?
        var textInputEpisodeStartNanos: UInt64 = 0

        init(window: NSWindow) {
            self.window = window
        }
    }

    init(log: TerminalInputLog = .shared) {
        self.log = log
        self.libghosttyMirror = LibghosttyLogMirror(log: log)
        super.init()
    }

    /// Install the key monitor, heartbeat, window notifications and the
    /// libghostty log mirror. Idempotent; main thread.
    func start() {
        guard !started else { return }
        started = true

        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            // Local monitors fire on the main thread before dispatch. We
            // only observe: the event is always returned unchanged.
            MainActor.assumeIsolated {
                self?.handleKeyDown(event)
            }
            return event
        }

        let center = NotificationCenter.default
        center.addObserver(
            self, selector: #selector(windowDidBecomeKey(_:)),
            name: NSWindow.didBecomeKeyNotification, object: nil
        )
        center.addObserver(
            self, selector: #selector(windowDidResignKey(_:)),
            name: NSWindow.didResignKeyNotification, object: nil
        )
        center.addObserver(
            self, selector: #selector(windowWillClose(_:)),
            name: NSWindow.willCloseNotification, object: nil
        )

        Self.probeMonitor = self
        Self.installWindowNoResponderProbe()
        startHeartbeat()
        libghosttyMirror.start()
        log.record(event: "monitor_started", fields: [
            "stall_threshold_ms": Self.stallThresholdMs,
            "heartbeat_interval_ms": Self.heartbeatIntervalMs,
        ])

        // Panes that joined a window before launch finished.
        let pending = pendingWindows
        pendingWindows.removeAll()
        for item in pending {
            if let window = item.window {
                registerTerminalWindow(window)
            }
        }
    }

    func stop() {
        guard started else { return }
        started = false
        if Self.probeMonitor === self { Self.probeMonitor = nil }
        if let keyMonitor {
            NSEvent.removeMonitor(keyMonitor)
            self.keyMonitor = nil
        }
        NotificationCenter.default.removeObserver(self)
        heartbeat?.cancel()
        heartbeat = nil
        libghosttyMirror.stop()
        for entry in windows.values {
            entry.observation?.invalidate()
        }
        windows.removeAll()
    }

    // MARK: - Window registration

    /// Called by a terminal pane whenever it joins `window`. Installs the
    /// first-responder observation once per window. Before `start()` the
    /// window is only remembered (and picked up by `start()`), so capture /
    /// isolated instances — which never start the monitor — install nothing.
    func registerTerminalWindow(_ window: NSWindow) {
        guard started else {
            if !pendingWindows.contains(where: { $0.window === window }) {
                pendingWindows.removeAll { $0.window == nil }
                pendingWindows.append(WeakWindow(window: window))
            }
            return
        }
        let key = ObjectIdentifier(window)
        if let existing = windows[key], existing.window === window {
            return
        }
        pruneDeadWindows()
        let entry = WindowEntry(window: window)
        entry.lastResponderChangeNanos = Self.nowNanos()
        entry.observation = window.observe(\.firstResponder, options: [.old, .new]) { [weak self] window, change in
            // KVO fires on the thread that mutates the property; AppKit
            // only changes a window's first responder on the main thread.
            let old = change.oldValue ?? nil
            let new = change.newValue ?? nil
            MainActor.assumeIsolated {
                self?.firstResponderChanged(window: window, old: old, new: new)
            }
        }
        windows[key] = entry
        log.record(event: "terminal_window_registered", fields: [
            "window": window.windowNumber,
            "first_responder": Self.describe(window.firstResponder),
            "responder_chain_tail": Self.describe(TerminalInputFallback.tail(of: window)),
        ])
    }

    private func entry(for window: NSWindow?) -> WindowEntry? {
        guard let window, let entry = windows[ObjectIdentifier(window)], entry.window === window else {
            return nil
        }
        return entry
    }

    private func pruneDeadWindows() {
        for (key, entry) in windows where entry.window == nil {
            entry.observation?.invalidate()
            windows.removeValue(forKey: key)
        }
    }

    // MARK: - First responder / key window

    private func firstResponderChanged(window: NSWindow, old: NSResponder?, new: NSResponder?) {
        guard let entry = entry(for: window) else { return }
        let now = Self.nowNanos()
        flushTextInputEpisode(entry, now: now)
        entry.lastResponderChangeNanos = now
        log.record(event: "first_responder_changed", fields: [
            "window": window.windowNumber,
            "is_key_window": window.isKeyWindow,
            "old": Self.describe(old),
            "old_kind": Self.kind(of: old).rawValue,
            "new": Self.describe(new),
            "new_kind": Self.kind(of: new).rawValue,
        ])
    }

    @objc private func windowDidBecomeKey(_ note: Notification) {
        keyWindowChanged(note.object as? NSWindow, becameKey: true)
    }

    @objc private func windowDidResignKey(_ note: Notification) {
        keyWindowChanged(note.object as? NSWindow, becameKey: false)
    }

    @objc private func windowWillClose(_ note: Notification) {
        guard let window = note.object as? NSWindow, let entry = entry(for: window) else { return }
        flushTextInputEpisode(entry, now: Self.nowNanos())
        entry.observation?.invalidate()
        windows.removeValue(forKey: ObjectIdentifier(window))
    }

    private func keyWindowChanged(_ window: NSWindow?, becameKey: Bool) {
        guard let window, entry(for: window) != nil else { return }
        log.record(event: "key_window_changed", fields: [
            "window": window.windowNumber,
            "became_key": becameKey,
            "first_responder": Self.describe(window.firstResponder),
            "app_key_window": NSApp.keyWindow?.windowNumber ?? 0,
        ])
    }

    // MARK: - keyDown routing

    private func handleKeyDown(_ event: NSEvent) {
        guard let window = event.window, let entry = entry(for: window) else { return }
        entry.keyContext.record(event, window: window)
        let responder = window.firstResponder
        let kind = Self.kind(of: responder)
        let now = Self.nowNanos()

        switch kind {
        case .terminal:
            flushTextInputEpisode(entry, now: now)

        case .textInput:
            // Legitimate typing into a field in the same window. Coalesce
            // per focus episode and never record key codes for it.
            if entry.textInputKeys == 0 {
                entry.textInputEpisodeStartNanos = now
                entry.textInputResponder = Self.describe(responder)
            }
            entry.textInputKeys += 1

        case .window, .other, .none:
            flushTextInputEpisode(entry, now: now)
            var fields = entry.keyContext.fields(
                for: event, window: window, selector: #selector(NSResponder.keyDown(with:))
            )
            fields["window"] = window.windowNumber
            fields["is_key_window"] = window.isKeyWindow
            fields["responder"] = Self.describe(responder)
            fields["responder_kind"] = kind.rawValue
            fields["is_repeat"] = event.isARepeat
            fields["since_responder_change_ms"] = Self.elapsedMs(from: entry.lastResponderChangeNanos, to: now)
            log.record(event: "key_not_delivered", fields: fields)
        }
    }

    private func flushTextInputEpisode(_ entry: WindowEntry, now: UInt64) {
        guard entry.textInputKeys > 0 else { return }
        log.record(event: "key_to_text_input", fields: [
            "window": entry.window?.windowNumber ?? 0,
            "responder": entry.textInputResponder ?? "nil",
            "count": entry.textInputKeys,
            "episode_ms": Self.elapsedMs(from: entry.textInputEpisodeStartNanos, to: now),
        ])
        entry.textInputKeys = 0
        entry.textInputResponder = nil
    }

    // MARK: - Window-level beep site

    private static var probeInstalled = false
    private static weak var probeMonitor: TerminalInputMonitor?

    /// Observe the base implementation so window controllers after a window
    /// are covered too. Always preserve AppKit's implementation for all selectors.
    private static func installWindowNoResponderProbe() {
        guard !probeInstalled else { return }
        probeInstalled = true
        let selector = #selector(NSResponder.noResponder(for:))
        guard let method = class_getInstanceMethod(NSResponder.self, selector) else { return }
        typealias Original = @convention(c) (AnyObject, Selector, Selector) -> Void
        let original = unsafeBitCast(method_getImplementation(method), to: Original.self)
        let block: @convention(block) (NSResponder, Selector) -> Void = { responder, eventSelector in
            MainActor.assumeIsolated {
                TerminalInputMonitor.probeMonitor?.windowNoResponder(responder: responder, selector: eventSelector)
            }
            original(responder, selector, eventSelector)
        }
        method_setImplementation(method, imp_implementationWithBlock(block))
    }

    private func windowNoResponder(responder: NSResponder, selector: Selector) {
        guard let window = TerminalInputFallback.window(for: responder),
              let entry = entry(for: window) else { return }
        var fields = entry.keyContext.fields(for: NSApp.currentEvent, window: window, selector: selector)
        fields["window"] = window.windowNumber
        fields["is_key_window"] = window.isKeyWindow
        fields["selector"] = NSStringFromSelector(selector)
        fields["beep_candidate"] = TerminalInputFallback.isBeepCandidate(selector)
        fields["receiver"] = Self.describe(responder)
        fields["responder"] = Self.describe(window.firstResponder)
        fields["responder_kind"] = Self.kind(of: window.firstResponder).rawValue
        log.record(event: "no_responder_window", fields: fields)
    }

    // MARK: - Main-thread heartbeat

    private func startHeartbeat() {
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(
            deadline: .now(),
            repeating: .milliseconds(Int(Self.heartbeatIntervalMs)),
            leeway: .milliseconds(20)
        )
        timer.setEventHandler { [weak self] in
            MainActor.assumeIsolated {
                self?.heartbeatFired()
            }
        }
        heartbeat = timer
        lastBeatNanos = Self.nowNanos()
        timer.resume()
    }

    private func heartbeatFired() {
        let now = Self.nowNanos()
        defer { lastBeatNanos = now }
        guard lastBeatNanos != 0, now > lastBeatNanos else { return }
        guard let blockedMs = Self.stallBlockedMs(
            gapNanos: now - lastBeatNanos,
            intervalMs: Self.heartbeatIntervalMs,
            thresholdMs: Self.stallThresholdMs
        ) else { return }
        let keyWindow = NSApp.keyWindow
        log.record(event: "main_thread_stall", fields: [
            "blocked_ms": (blockedMs * 10).rounded() / 10,
            "key_window": keyWindow?.windowNumber ?? 0,
            "first_responder": Self.describe(keyWindow?.firstResponder),
        ])
    }

    /// How long the main thread was unavailable, given that a heartbeat
    /// scheduled every `intervalMs` fired `gapNanos` after the previous
    /// one. Returns `nil` below `thresholdMs`. Pure for tests. Timer
    /// leeway (20 ms) is not subtracted, so the value is an upper bound by
    /// at most that much.
    static func stallBlockedMs(gapNanos: UInt64, intervalMs: Double, thresholdMs: Double) -> Double? {
        let blocked = Double(gapNanos) / 1_000_000.0 - intervalMs
        return blocked > thresholdMs ? blocked : nil
    }

    // MARK: - Helpers

    static func describe(_ responder: NSResponder?) -> String {
        let paneId = (responder as? GhosttyTerminalHostView)?.session.id
        return TerminalInputDescribe.responderDescription(responder, paneId: paneId)
    }

    static func kind(of responder: NSResponder?) -> TerminalInputDescribe.ResponderKind {
        TerminalInputDescribe.responderKind(responder, isTerminal: responder is GhosttyTerminalHostView)
    }

    private static func nowNanos() -> UInt64 {
        DispatchTime.now().uptimeNanoseconds
    }

    private static func elapsedMs(from start: UInt64, to end: UInt64) -> Double {
        guard end > start, start != 0 else { return 0 }
        return (Double(end - start) / 1_000_000.0 * 10).rounded() / 10
    }
}

// MARK: - libghostty unified-log mirror

/// Mirrors libghostty's warning/error lines from unified logging into
/// [[TerminalInputLog]].
///
/// libghostty's `std.log` sink on macOS is `os_log` under subsystem
/// `com.mitchellh.ghostty`, with the Zig log scope as the category and
/// Zig `warn`/`err` mapped to `OSLogType.error`/`.fault`. Its pty writer
/// (`io_exec` scope) reports a failed `write(2)` to the pty as
/// `write error: <errno>`; nothing in the C API surfaces that to the
/// embedder, so reading it back from the log store is the only way Boss
/// can record a pty write failure next to the keystroke that caused it.
/// (A *short* write is not logged by libghostty at all; see the
/// "not observable" note in `terminal-input-diagnostics.md`.)
///
/// Polls `OSLogStore(scope: .currentProcessIdentifier)` on a utility queue
/// every `pollIntervalSeconds`, reading only entries newer than the last
/// one seen. Error-and-above only, capped per poll so a libghostty warning
/// storm cannot flood the file; the cap never drops a pty write error, and
/// any other overflow is summarised in a `libghostty_log_dropped` line.
final class LibghosttyLogMirror: @unchecked Sendable {
    static let subsystem = "com.mitchellh.ghostty"
    static let pollIntervalSeconds: Double = 2
    static let perPollCap = 20
    static let maxMessageLength = 500

    private let log: TerminalInputLog
    private let queue = DispatchQueue(label: "Boss.LibghosttyLogMirror", qos: .utility)
    private var timer: DispatchSourceTimer?
    private var store: OSLogStore?
    private var lastSeen = Date()
    private var consecutiveFailures = 0

    init(log: TerminalInputLog) {
        self.log = log
    }

    func start() {
        queue.async { [self] in
            guard timer == nil else { return }
            lastSeen = Date()
            let timer = DispatchSource.makeTimerSource(queue: queue)
            timer.schedule(
                deadline: .now() + Self.pollIntervalSeconds,
                repeating: Self.pollIntervalSeconds,
                leeway: .milliseconds(500)
            )
            timer.setEventHandler { [weak self] in self?.poll() }
            self.timer = timer
            timer.resume()
        }
    }

    func stop() {
        queue.async { [self] in
            timer?.cancel()
            timer = nil
            store = nil
        }
    }

    /// Zig `warn` arrives as `.error`, Zig `err` as `.fault`. Everything
    /// below that is libghostty's routine chatter and is not mirrored.
    static func shouldMirror(level: OSLogEntryLog.Level) -> Bool {
        level == .error || level == .fault
    }

    static func levelName(_ level: OSLogEntryLog.Level) -> String {
        switch level {
        case .fault: "fault"
        case .error: "error"
        case .notice: "notice"
        case .info: "info"
        case .debug: "debug"
        default: "undefined"
        }
    }

    /// One mirrorable libghostty entry, decoupled from `OSLogEntryLog` so
    /// the cap logic is testable.
    struct Candidate: Equatable {
        let date: Date
        let category: String
        let level: String
        let message: String
    }

    /// Whether `message` is the pty writer's failed-write line. Those are
    /// the entries this mirror exists for, so the per-poll cap never drops
    /// them.
    static func isPtyWriteError(_ message: String) -> Bool {
        message.contains("write error")
    }

    /// Apply the per-poll cap: the first `cap` entries are mirrored, later
    /// ones are dropped — except pty write errors, which always are. The
    /// dropped remainder is summarised (count and time range) so an overflow
    /// is visible in the log rather than silent.
    static func plan(
        _ candidates: [Candidate], cap: Int
    ) -> (mirrored: [Candidate], dropped: (count: Int, first: Date, last: Date)?) {
        var mirrored: [Candidate] = []
        var droppedCount = 0
        var first: Date?
        var last: Date?
        for candidate in candidates {
            if mirrored.count < cap || isPtyWriteError(candidate.message) {
                mirrored.append(candidate)
            } else {
                droppedCount += 1
                first = first ?? candidate.date
                last = candidate.date
            }
        }
        guard let first, let last else { return (mirrored, nil) }
        return (mirrored, (droppedCount, first, last))
    }

    private func poll() {
        let store: OSLogStore
        if let existing = self.store {
            store = existing
        } else {
            do {
                store = try OSLogStore(scope: .currentProcessIdentifier)
                self.store = store
            } catch {
                // No log store for this process (sandbox / unsupported):
                // say so once and stop polling rather than retrying forever.
                log.record(event: "libghostty_log_mirror_unavailable", fields: [
                    "error": String(describing: error),
                ])
                timer?.cancel()
                timer = nil
                return
            }
        }

        let predicate = NSPredicate(format: "subsystem == %@", Self.subsystem)
        let entries: AnySequence<OSLogEntry>
        do {
            entries = try store.getEntries(with: [], at: store.position(date: lastSeen), matching: predicate)
            consecutiveFailures = 0
        } catch {
            consecutiveFailures += 1
            if consecutiveFailures == 1 {
                log.record(event: "libghostty_log_mirror_error", fields: [
                    "error": String(describing: error),
                ])
            }
            if consecutiveFailures >= 5 {
                timer?.cancel()
                timer = nil
            }
            return
        }

        var candidates: [Candidate] = []
        var newest = lastSeen
        for case let entry as OSLogEntryLog in entries {
            guard entry.date > lastSeen else { continue }
            if entry.date > newest { newest = entry.date }
            guard Self.shouldMirror(level: entry.level) else { continue }
            candidates.append(Candidate(
                date: entry.date, category: entry.category, level: Self.levelName(entry.level),
                message: String(entry.composedMessage.prefix(Self.maxMessageLength))
            ))
        }
        let plan = Self.plan(candidates, cap: Self.perPollCap)
        for candidate in plan.mirrored {
            log.record(event: "libghostty_log", fields: [
                "category": candidate.category,
                "level": candidate.level,
                "message": candidate.message,
                "logged_at_epoch_ms": Int64(candidate.date.timeIntervalSince1970 * 1000),
            ])
        }
        if let dropped = plan.dropped {
            log.record(event: "libghostty_log_dropped", fields: [
                "count": dropped.count,
                "first_logged_at_epoch_ms": Int64(dropped.first.timeIntervalSince1970 * 1000),
                "last_logged_at_epoch_ms": Int64(dropped.last.timeIntervalSince1970 * 1000),
            ])
        }
        lastSeen = newest
    }
}
