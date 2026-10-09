import AppKit
import XCTest

@testable import Boss

@MainActor
final class TerminalInputFallbackTests: XCTestCase {
    private let keyDown = #selector(NSResponder.keyDown(with:))
    private let keyUp = #selector(NSResponder.keyUp(with:))

    private func window() -> NSWindow {
        NSWindow(contentRect: .zero, styleMask: [], backing: .buffered, defer: false)
    }

    private func key(_ window: NSWindow, code: UInt16, characters: String) -> NSEvent {
        NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: [], timestamp: 1,
            windowNumber: window.windowNumber, context: nil, characters: characters,
            charactersIgnoringModifiers: characters, isARepeat: false, keyCode: code
        )!
    }

    func testControllerAtChainTailResolvesItsWindow() {
        let window = window()
        let controller = NSWindowController(window: window)
        window.nextResponder = controller
        XCTAssertTrue(TerminalInputFallback.tail(of: window) === controller)
        XCTAssertTrue(TerminalInputFallback.window(for: controller) === window)
        XCTAssertTrue(TerminalInputFallback.window(for: window) === window)
        XCTAssertNil(TerminalInputFallback.window(for: NSResponder()))
    }

    func testInstalledProbeRecordsWindowAndControllerFallbacks() throws {
        let directory = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
            isDirectory: true
        ).appendingPathComponent("terminal-probe-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let log = TerminalInputLog(directory: directory.path)
        let monitor = TerminalInputMonitor(log: log)
        monitor.start()
        defer { monitor.stop() }
        let window = window()
        let controller = NSWindowController(window: window)
        window.nextResponder = controller
        monitor.registerTerminalWindow(window)

        window.noResponder(for: keyUp)
        controller.noResponder(for: keyUp)
        log.flushForTesting()

        let files = try FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil)
        let records = try files.flatMap { file in
            try String(contentsOf: file, encoding: .utf8).split(separator: "\n").map { line in
                try XCTUnwrap(JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any])
            }
        }.filter { $0["event"] as? String == "no_responder_window" }
        XCTAssertEqual(records.count, 2)
        XCTAssertEqual(records.compactMap { $0["receiver"] as? String }, [
            TerminalInputMonitor.describe(window), TerminalInputMonitor.describe(controller),
        ])
        for record in records {
            XCTAssertEqual(record["window"] as? Int, window.windowNumber)
            XCTAssertEqual(record["selector"] as? String, "keyUp:")
            XCTAssertEqual(record["beep_candidate"] as? Bool, false)
            XCTAssertNil(record["key"])
        }
    }

    func testOnlyKeyDownIsBeepCandidateAndCarriesKeyFields() {
        let window = window()
        let event = key(window, code: 36, characters: "\r")
        let context = TerminalInputKeyContext()
        context.record(event, window: window)
        XCTAssertTrue(TerminalInputFallback.isBeepCandidate(keyDown))
        XCTAssertFalse(TerminalInputFallback.isBeepCandidate(keyUp))
        XCTAssertEqual(context.fields(for: event, window: window, selector: keyDown)["key"] as? String, "return")
        XCTAssertTrue(context.fields(for: event, window: window, selector: keyUp).isEmpty)
    }

    func testOtherToTextInputReplacesOldKeyContext() {
        let window = window()
        let context = TerminalInputKeyContext()
        let old = key(window, code: 36, characters: "\r")
        window.makeFirstResponder(window)
        context.record(old, window: window)
        let text = NSTextView(frame: .zero)
        window.contentView = text
        window.makeFirstResponder(text)
        let current = key(window, code: 0, characters: "a")
        context.record(current, window: window)
        let fields = context.fields(for: current, window: window, selector: keyDown)
        XCTAssertEqual(fields["key"] as? String, "letter")
        XCTAssertNil(fields["key_code"])
        XCTAssertTrue(context.fields(for: old, window: window, selector: keyDown).isEmpty)
    }

    func testTwoWindowsCannotBorrowEachOthersKey() {
        let first = window()
        let second = window()
        let firstContext = TerminalInputKeyContext()
        let secondContext = TerminalInputKeyContext()
        let firstEvent = key(first, code: 36, characters: "\r")
        let secondEvent = key(second, code: 53, characters: "\u{1b}")
        firstContext.record(firstEvent, window: first)
        secondContext.record(secondEvent, window: second)
        XCTAssertTrue(firstContext.fields(for: secondEvent, window: second, selector: keyDown).isEmpty)
        XCTAssertTrue(firstContext.fields(for: firstEvent, window: second, selector: keyDown).isEmpty)
        XCTAssertEqual(secondContext.fields(for: secondEvent, window: second, selector: keyDown)["key"] as? String, "escape")
    }

    func testFallbackWithoutMatchingEventOmitsKey() {
        let window = window()
        let context = TerminalInputKeyContext()
        let event = key(window, code: 36, characters: "\r")
        XCTAssertTrue(context.fields(for: event, window: window, selector: keyDown).isEmpty)
        context.record(event, window: window)
        XCTAssertTrue(context.fields(for: nil, window: window, selector: keyDown).isEmpty)
        let different = key(window, code: 36, characters: "\r")
        XCTAssertTrue(context.fields(for: different, window: window, selector: keyDown).isEmpty)
    }
}
