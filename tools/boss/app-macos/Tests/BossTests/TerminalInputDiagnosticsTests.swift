import AppKit
import OSLog
import XCTest

@testable import Boss

/// Pins the pure, deterministic pieces of the terminal keyboard-input
/// diagnostics (dropped-keystroke + beep investigation): the JSONL line
/// shape, the content-free key and responder descriptions, the stall
/// threshold arithmetic, the bell record, and the libghostty log-mirror
/// level filter. The live monitor (KVO, NSEvent monitor, heartbeat,
/// OSLogStore poll) is exercised in the running app, not here.
final class TerminalInputDiagnosticsTests: XCTestCase {

    // MARK: - JSONL line shape

    func testLineCarriesTimestampEventAndFieldsSorted() throws {
        let data = try XCTUnwrap(TerminalInputLog.line(
            event: "key_not_delivered",
            tsEpochMs: 1_700_000_000_123,
            fields: ["responder": "NSWindow", "key_code": 36]
        ))
        let text = try XCTUnwrap(String(data: data, encoding: .utf8))
        XCTAssertTrue(text.hasSuffix("\n"), "one JSONL record per line")
        XCTAssertEqual(
            text,
            "{\"event\":\"key_not_delivered\",\"key_code\":36,\"responder\":\"NSWindow\",\"ts_epoch_ms\":1700000000123}\n"
        )
    }

    func testLineFieldsCannotOverrideEventOrTimestamp() throws {
        let data = try XCTUnwrap(TerminalInputLog.line(
            event: "bell",
            tsEpochMs: 42,
            fields: ["event": "spoofed", "ts_epoch_ms": 0]
        ))
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(object["event"] as? String, "bell")
        XCTAssertEqual(object["ts_epoch_ms"] as? Int64, 42)
    }

    func testLineReturnsNilForUnencodablePayload() {
        XCTAssertNil(TerminalInputLog.line(event: "x", tsEpochMs: 1, fields: ["bad": Date()]))
    }

    func testRecordWritesDayFileUnderPrefix() throws {
        let dir = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
            isDirectory: true
        ).appendingPathComponent("terminal-input-log-\(UUID().uuidString)", isDirectory: true)
        let log = TerminalInputLog(directory: dir.path)
        log.record(event: "monitor_started", fields: ["stall_threshold_ms": 250])
        log.flushForTesting()

        let names = try FileManager.default.contentsOfDirectory(atPath: dir.path)
        XCTAssertEqual(names.count, 1)
        let name = try XCTUnwrap(names.first)
        XCTAssertTrue(name.hasPrefix(TerminalInputLog.filePrefix), name)
        XCTAssertTrue(name.hasSuffix(".jsonl"), name)
        let contents = try String(contentsOfFile: dir.appendingPathComponent(name).path, encoding: .utf8)
        XCTAssertTrue(contents.contains("\"event\":\"monitor_started\""), contents)
        try? FileManager.default.removeItem(at: dir)
    }

    // MARK: - Key redaction

    func testPrintableKeysCollapseToClasses() {
        XCTAssertEqual(TerminalInputDescribe.keyName(keyCode: 0x00, characters: "a"), "letter")
        XCTAssertEqual(TerminalInputDescribe.keyName(keyCode: 0x12, characters: "1"), "digit")
        XCTAssertEqual(TerminalInputDescribe.keyName(keyCode: 0x2B, characters: ","), "symbol")
        XCTAssertEqual(TerminalInputDescribe.keyName(keyCode: 0x0A, characters: "é"), "letter")
    }

    func testNamedKeysWinOverCharacters() {
        XCTAssertEqual(TerminalInputDescribe.keyName(keyCode: 0x24, characters: "\r"), "return")
        XCTAssertEqual(TerminalInputDescribe.keyName(keyCode: 0x35, characters: "\u{1B}"), "escape")
        XCTAssertEqual(TerminalInputDescribe.keyName(keyCode: 0x31, characters: " "), "space")
        XCTAssertEqual(TerminalInputDescribe.keyName(keyCode: 0x7E, characters: "\u{F700}"), "up_arrow")
    }

    func testControlAndFunctionKeysAreNamedNotRedacted() {
        // Ctrl-C arrives as 0x03 with no named key code entry.
        XCTAssertEqual(TerminalInputDescribe.keyName(keyCode: 0x08, characters: "\u{03}"), "control_3")
        // F5 is a private-use function character.
        XCTAssertEqual(TerminalInputDescribe.keyName(keyCode: 0x60, characters: "\u{F708}"), "function_f708")
        XCTAssertEqual(TerminalInputDescribe.keyName(keyCode: 0x3F, characters: nil), "keycode_63")
        XCTAssertEqual(TerminalInputDescribe.keyName(keyCode: 0x3F, characters: ""), "keycode_63")
    }

    func testModifierDescriptionIsStableAndIgnoresDeviceBits() {
        XCTAssertEqual(TerminalInputDescribe.modifierDescription([]), "none")
        XCTAssertEqual(TerminalInputDescribe.modifierDescription([.command, .shift]), "shift+cmd")
        XCTAssertEqual(
            TerminalInputDescribe.modifierDescription([.control, .option, .shift, .command]),
            "ctrl+alt+shift+cmd"
        )
        // Device-dependent bits (e.g. left/right shift distinction) are stripped.
        let withDeviceBits = NSEvent.ModifierFlags(rawValue: NSEvent.ModifierFlags.shift.rawValue | 0x2)
        XCTAssertEqual(TerminalInputDescribe.modifierDescription(withDeviceBits), "shift")
    }

    func testKeyFieldsOmitKeyCodeForPrintableClasses() {
        for (code, chars) in [(UInt16(0x00), "a"), (0x12, "1"), (0x2B, ","), (0x31, " ")] {
            let fields = TerminalInputDescribe.keyFields(
                keyCode: code, characters: chars, modifierFlags: []
            )
            XCTAssertNil(fields["key_code"], "key_code must not identify \(chars.debugDescription): \(fields)")
            XCTAssertEqual(fields["mods"] as? String, "none")
        }
        let letter = TerminalInputDescribe.keyFields(keyCode: 0x00, characters: "a", modifierFlags: [])
        XCTAssertEqual(Set(letter.keys), ["key", "mods"])
        XCTAssertEqual(letter["key"] as? String, "letter")
    }

    func testKeyFieldsKeepKeyCodeForNamedAndFunctionKeys() {
        let ret = TerminalInputDescribe.keyFields(keyCode: 0x24, characters: "\r", modifierFlags: [])
        XCTAssertEqual(ret["key"] as? String, "return")
        XCTAssertEqual(ret["key_code"] as? Int, 0x24)
        let fkey = TerminalInputDescribe.keyFields(keyCode: 0x60, characters: "\u{F708}", modifierFlags: [])
        XCTAssertEqual(fkey["key_code"] as? Int, 0x60)
    }

    // MARK: - Responder classification

    @MainActor
    func testResponderKinds() {
        XCTAssertEqual(TerminalInputDescribe.responderKind(nil, isTerminal: false), .none)
        XCTAssertEqual(TerminalInputDescribe.responderKind(NSResponder(), isTerminal: true), .terminal)
        XCTAssertEqual(TerminalInputDescribe.responderKind(NSTextView(frame: .zero), isTerminal: false), .textInput)
        XCTAssertEqual(TerminalInputDescribe.responderKind(NSView(frame: .zero), isTerminal: false), .other)
        XCTAssertEqual(TerminalInputDescribe.responderKind(NSResponder(), isTerminal: false), .other)
        let window = NSWindow(contentRect: .zero, styleMask: [], backing: .buffered, defer: true)
        XCTAssertEqual(TerminalInputDescribe.responderKind(window, isTerminal: false), .window)
    }

    @MainActor
    func testResponderDescriptionNamesTypeAndPane() {
        XCTAssertEqual(TerminalInputDescribe.responderDescription(nil), "nil")
        XCTAssertEqual(TerminalInputDescribe.responderDescription(NSView(frame: .zero)), "NSView")
        XCTAssertEqual(
            TerminalInputDescribe.responderDescription(NSView(frame: .zero), paneId: "boss"),
            "NSView(pane=boss)"
        )
    }

    // MARK: - Stall arithmetic

    @MainActor
    func testStallBlockedMsSubtractsHeartbeatInterval() {
        // Heartbeat every 100 ms fired 450 ms after the previous one → the
        // main thread was unavailable ~350 ms, over the 250 ms threshold.
        let blocked = TerminalInputMonitor.stallBlockedMs(
            gapNanos: 450_000_000, intervalMs: 100, thresholdMs: 250
        )
        XCTAssertEqual(blocked ?? 0, 350, accuracy: 0.001)
    }

    @MainActor
    func testStallBlockedMsIsNilAtOrBelowThreshold() {
        // 350 ms gap − 100 ms interval = 250 ms blocked: not strictly over.
        XCTAssertNil(TerminalInputMonitor.stallBlockedMs(
            gapNanos: 350_000_000, intervalMs: 100, thresholdMs: 250
        ))
        // An on-time heartbeat.
        XCTAssertNil(TerminalInputMonitor.stallBlockedMs(
            gapNanos: 100_000_000, intervalMs: 100, thresholdMs: 250
        ))
    }

    @MainActor
    func testMonitorThresholdMatchesStallMonitorConfig() {
        XCTAssertEqual(TerminalInputMonitor.stallThresholdMs, MainThreadStallMonitor.Config().thresholdMs)
    }

    // MARK: - Bell record

    func testBellFieldsForBossPaneSayTheAlertRang() {
        let fields = GhosttyRuntime.bellLogFields(paneId: "boss", role: .boss, rangSystemAlert: true)
        XCTAssertEqual(fields["pane"] as? String, "boss")
        XCTAssertEqual(fields["role"] as? String, "boss")
        XCTAssertEqual(fields["rang_system_alert"] as? Bool, true)
        XCTAssertNil(fields["slot"])
    }

    func testBellFieldsForWorkerPaneCarrySlotAndSilence() {
        let fields = GhosttyRuntime.bellLogFields(paneId: "run-abc", role: .worker(slot: 3), rangSystemAlert: false)
        XCTAssertEqual(fields["pane"] as? String, "run-abc")
        XCTAssertEqual(fields["role"] as? String, "worker")
        XCTAssertEqual(fields["slot"] as? Int, 3)
        XCTAssertEqual(fields["rang_system_alert"] as? Bool, false)
    }

    func testBellFieldsForUnresolvedTarget() {
        let fields = GhosttyRuntime.bellLogFields(paneId: nil, role: nil, rangSystemAlert: false)
        XCTAssertEqual(fields["pane"] as? String, "unresolved")
        XCTAssertEqual(fields["role"] as? String, "unresolved")
    }

    // MARK: - libghostty log mirror filter

    func testMirrorKeepsZigWarnAndErrOnly() {
        // Zig `warn` → .error, Zig `err` → .fault.
        XCTAssertTrue(LibghosttyLogMirror.shouldMirror(level: .error))
        XCTAssertTrue(LibghosttyLogMirror.shouldMirror(level: .fault))
        XCTAssertFalse(LibghosttyLogMirror.shouldMirror(level: .notice))
        XCTAssertFalse(LibghosttyLogMirror.shouldMirror(level: .info))
        XCTAssertFalse(LibghosttyLogMirror.shouldMirror(level: .debug))
        XCTAssertFalse(LibghosttyLogMirror.shouldMirror(level: .undefined))
    }

    func testMirrorLevelNames() {
        XCTAssertEqual(LibghosttyLogMirror.levelName(.fault), "fault")
        XCTAssertEqual(LibghosttyLogMirror.levelName(.error), "error")
        XCTAssertEqual(LibghosttyLogMirror.levelName(.undefined), "undefined")
        XCTAssertEqual(LibghosttyLogMirror.subsystem, "com.mitchellh.ghostty")
    }

    // MARK: - libghostty mirror cap

    private func candidate(_ index: Int, _ message: String) -> LibghosttyLogMirror.Candidate {
        .init(
            date: Date(timeIntervalSince1970: 1_700_000_000 + Double(index)),
            category: "io_exec", level: "error", message: message
        )
    }

    func testCapDoesNotDropPtyWriteErrorsAfterTheCap() {
        var entries = (0..<25).map { candidate($0, "noise \($0)") }
        entries.append(candidate(25, "write error: errno 32"))
        entries.append(candidate(26, "more noise"))
        let plan = LibghosttyLogMirror.plan(entries, cap: LibghosttyLogMirror.perPollCap)
        XCTAssertEqual(plan.mirrored.count, LibghosttyLogMirror.perPollCap + 1)
        XCTAssertTrue(plan.mirrored.contains { $0.message == "write error: errno 32" })
        let dropped = try? XCTUnwrap(plan.dropped)
        XCTAssertEqual(dropped?.count, 6)
        XCTAssertEqual(dropped?.first, entries[20].date)
        XCTAssertEqual(dropped?.last, entries[26].date)
    }

    func testCapUnderLimitDropsNothing() {
        let plan = LibghosttyLogMirror.plan((0..<5).map { candidate($0, "n") }, cap: 20)
        XCTAssertEqual(plan.mirrored.count, 5)
        XCTAssertNil(plan.dropped)
    }
}
