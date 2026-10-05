import AppKit
import Foundation
import os

/// Durable daily JSONL record of keyboard-input delivery to the embedded
/// libghostty terminal panes, written so a dropped keystroke (and the beep
/// that accompanies it) can be attributed after the fact.
///
/// Files live under the Boss state root at
/// `diagnostics/terminal-input-YYYY-MM-DD.jsonl` (same directory as the
/// spawn and terminal-loop diagnostics) and are read with
/// `bossctl logs terminal-input`. Every line also goes to unified logging
/// (`com.boss.app` / `terminal-input`) so the stream can be watched live
/// while reproducing:
///
///     log stream --predicate 'subsystem == "com.boss.app" AND category == "terminal-input"'
///
/// Event vocabulary (the `event` field; see
/// `tools/boss/docs/terminal-input-diagnostics.md` for the full table):
///
/// - `first_responder_changed` — the host window's first responder moved
///   (old/new responder types, whether either is a terminal pane).
/// - `key_window_changed` — a terminal-hosting window gained or lost key.
/// - `key_not_delivered` — a `keyDown` arrived in a terminal-hosting window
///   while something other than a terminal pane was first responder. This
///   is the AppKit path that ends in `noResponder(for:)` → `NSBeep` when
///   the responder chain does not handle the key.
/// - `key_to_text_input` — same, but the responder was a legitimate text
///   field; coalesced per focus episode and without key codes.
/// - `terminal_focus` — a pane itself became / resigned first responder.
/// - `host_window_detached` / `host_window_attached` — a pane's NSView left
///   or joined a window (SwiftUI re-parenting; AppKit resets the first
///   responder when the responder's view leaves its window).
/// - `key_dropped_no_surface` — a key reached a pane with no live surface.
/// - `key_not_consumed` — libghostty returned "not consumed" for a key.
/// - `do_command` — AppKit routed a `doCommand(by:)` selector to a pane.
/// - `bell` — a BEL arrived from a pane's pty; `rang_system_alert` says
///   whether Boss played the system alert for it.
/// - `main_thread_stall` — the main thread was unavailable for longer than
///   the threshold (no backtrace; see `MainThreadStallMonitor` for those).
/// - `libghostty_log` — a warning/error libghostty wrote to unified logging
///   (its pty writer logs `write error: …` here on a failed pty write).
///
/// Keystroke content is never recorded: letters and digits are reduced to
/// a class (`letter`, `digit`); only control / function keys are named.
final class TerminalInputLog: @unchecked Sendable {
    /// Isolated / capture instances keep the os_log mirror but write no
    /// file: the diagnostics directory belongs to the production app's
    /// state root, and a test-fixture launch must not append to it.
    static let shared: TerminalInputLog = {
        guard !BossEnginePaths.isIsolatedInstance else {
            return TerminalInputLog(directory: nil)
        }
        let appSupport = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first!
        let dir = appSupport.appendingPathComponent("Boss/diagnostics", isDirectory: true)
        return TerminalInputLog(directory: dir.path)
    }()

    static let filePrefix = "terminal-input-"

    /// `nil` directory means no disk mirror (used by tests).
    private let directory: String?
    private let retainDays: Int
    private let queue = DispatchQueue(label: "Boss.TerminalInputLog")
    private let logger = Logger(subsystem: "com.boss.app", category: "terminal-input")
    private var currentDate = ""
    private var fileHandle: FileHandle?
    /// Throttles the write-failure warning to at most one per rotation —
    /// see [[DiagnosticWrite]].
    private var writeFailureWarned = false
    private let dateFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "yyyy-MM-dd"
        f.timeZone = TimeZone(identifier: "UTC")
        return f
    }()

    init(directory: String?, retainDays: Int = 7) {
        self.directory = directory
        self.retainDays = retainDays
    }

    /// Append one event. Safe from any thread; the JSON line is built on
    /// the caller's thread (so `fields` need not be Sendable) and the file
    /// write happens on the private queue.
    func record(event: String, fields: [String: Any] = [:]) {
        let now = Date()
        let epochMs = Int64(now.timeIntervalSince1970 * 1000)
        guard let lineData = Self.line(event: event, tsEpochMs: epochMs, fields: fields) else {
            return
        }
        if let text = String(data: lineData, encoding: .utf8) {
            logger.notice("\(text.trimmingCharacters(in: .newlines), privacy: .public)")
        }

        queue.async { [self] in
            guard directory != nil else { return }
            let dateStr = dateFormatter.string(from: now)
            if dateStr != currentDate || fileHandle == nil {
                if dateStr != currentDate {
                    pruneOldFiles()
                }
                openFile(dateStr: dateStr)
            }
            if let handle = fileHandle {
                DiagnosticWrite.append(
                    lineData, to: handle, site: "TerminalInputLog", warned: &writeFailureWarned
                )
            }
        }
    }

    /// Block until queued file writes have drained. Test-only helper.
    func flushForTesting() {
        queue.sync {}
    }

    /// Pure, testable builder for one JSONL line (trailing newline
    /// included). `ts_epoch_ms` and `event` are always present; `fields`
    /// are merged in (they cannot override those two). Returns `nil` only
    /// if the payload is not JSON-encodable.
    static func line(event: String, tsEpochMs: Int64, fields: [String: Any]) -> Data? {
        var entry: [String: Any] = [:]
        for (key, value) in fields {
            entry[key] = value
        }
        entry["ts_epoch_ms"] = tsEpochMs
        entry["event"] = event
        guard JSONSerialization.isValidJSONObject(entry),
              let jsonData = try? JSONSerialization.data(withJSONObject: entry, options: [.sortedKeys])
        else {
            return nil
        }
        return jsonData + Data([0x0A])
    }

    private func openFile(dateStr: String) {
        guard let directory else { return }
        DiagnosticWrite.closeQuietly(fileHandle)
        fileHandle = nil

        do {
            try FileManager.default.createDirectory(atPath: directory, withIntermediateDirectories: true)
        } catch {
            return
        }

        let path = (directory as NSString).appendingPathComponent("\(Self.filePrefix)\(dateStr).jsonl")
        guard let handle = DiagnosticWrite.openForAppending(atPath: path) else { return }
        fileHandle = handle
        currentDate = dateStr
        writeFailureWarned = false
    }

    private func pruneOldFiles() {
        guard let directory else { return }
        let cutoff = Date().addingTimeInterval(-Double(retainDays) * 86_400)
        let cutoffStr = dateFormatter.string(from: cutoff)

        guard let entries = try? FileManager.default.contentsOfDirectory(atPath: directory) else {
            return
        }
        for name in entries {
            guard name.hasPrefix(Self.filePrefix), name.hasSuffix(".jsonl") else { continue }
            let dateStr = String(name.dropFirst(Self.filePrefix.count).dropLast(".jsonl".count))
            if dateStr < cutoffStr {
                let fullPath = (directory as NSString).appendingPathComponent(name)
                try? FileManager.default.removeItem(atPath: fullPath)
            }
        }
    }
}

// MARK: - Pure description helpers

/// Content-free descriptions of keys and responders for the input log.
/// Pure and `static` so the redaction and classification contracts are
/// unit-testable without AppKit events.
enum TerminalInputDescribe {
    /// How a responder relates to terminal input delivery.
    enum ResponderKind: String {
        /// A `GhosttyTerminalHostView` — the key reaches libghostty.
        case terminal
        /// A text field / text view that legitimately owns typing.
        case textInput = "text_input"
        /// Anything else (window, hosting view, button, …). A `keyDown`
        /// here that nothing in the chain handles is the AppKit beep.
        case other
        /// No first responder at all (AppKit treats the window as the
        /// responder; the beep path).
        case none
    }

    /// Classify a responder. `isTerminal` is injected (rather than a type
    /// check) so the pure classifier is testable with plain `NSResponder`s.
    static func responderKind(_ responder: NSResponder?, isTerminal: Bool) -> ResponderKind {
        guard let responder else { return .none }
        if isTerminal { return .terminal }
        if responder is NSTextInputClient || responder is NSText { return .textInput }
        return .other
    }

    /// Short, bounded type name for a responder (SwiftUI hosting views have
    /// very long generic names). `paneId` is appended for terminal panes so
    /// the line names which pane had focus.
    static func responderDescription(_ responder: NSResponder?, paneId: String? = nil) -> String {
        guard let responder else { return "nil" }
        var name = String(describing: type(of: responder))
        if name.count > 96 {
            name = String(name.prefix(93)) + "…"
        }
        if let paneId {
            return "\(name)(pane=\(paneId))"
        }
        return name
    }

    /// Content-free key name. Printable letters and digits collapse to a
    /// class so the log never records what was typed; control and function
    /// keys are named because *which* key was dropped matters for the
    /// diagnosis (a Return that never arrived reads very differently from a
    /// letter).
    static func keyName(keyCode: UInt16, characters: String?) -> String {
        if let named = Self.namedKeys[keyCode] {
            return named
        }
        guard let characters, let scalar = characters.unicodeScalars.first else {
            return "keycode_\(keyCode)"
        }
        if scalar.value >= 0xF700 && scalar.value <= 0xF8FF {
            return "function_\(String(scalar.value, radix: 16))"
        }
        if scalar.value < 0x20 || scalar.value == 0x7F {
            return "control_\(String(scalar.value, radix: 16))"
        }
        if scalar.properties.isAlphabetic { return "letter" }
        if scalar.properties.numericType != nil { return "digit" }
        if scalar.properties.isWhitespace { return "space" }
        return "symbol"
    }

    /// Compact modifier string in a stable order, e.g. `"shift+cmd"`;
    /// `"none"` when no device-independent modifier is held.
    static func modifierDescription(_ flags: NSEvent.ModifierFlags) -> String {
        let clean = flags.intersection(.deviceIndependentFlagsMask)
        var parts: [String] = []
        if clean.contains(.control) { parts.append("ctrl") }
        if clean.contains(.option) { parts.append("alt") }
        if clean.contains(.shift) { parts.append("shift") }
        if clean.contains(.command) { parts.append("cmd") }
        if clean.contains(.function) { parts.append("fn") }
        if clean.contains(.capsLock) { parts.append("caps") }
        return parts.isEmpty ? "none" : parts.joined(separator: "+")
    }

    /// macOS virtual key codes for the keys worth naming individually.
    private static let namedKeys: [UInt16: String] = [
        0x24: "return",
        0x30: "tab",
        0x31: "space",
        0x33: "delete",
        0x35: "escape",
        0x4C: "keypad_enter",
        0x73: "home",
        0x74: "page_up",
        0x75: "forward_delete",
        0x77: "end",
        0x79: "page_down",
        0x7B: "left_arrow",
        0x7C: "right_arrow",
        0x7D: "down_arrow",
        0x7E: "up_arrow",
    ]
}
