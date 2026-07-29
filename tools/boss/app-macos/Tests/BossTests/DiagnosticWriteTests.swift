import XCTest
@testable import Boss

/// Regression coverage for the exact shape of the terminal-loop livelock
/// (Boss v1.0.427): a diagnostic log write that fails must never raise —
/// it must return `false` and let the caller keep running.
final class DiagnosticWriteTests: XCTestCase {
    private func makeTempFile() -> URL {
        URL(fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(), isDirectory: true)
            .appendingPathComponent("diagnostic-write-test-\(UUID().uuidString).jsonl")
    }

    func testAppendSucceedsOnWritableHandle() throws {
        let url = makeTempFile()
        defer { try? FileManager.default.removeItem(at: url) }
        FileManager.default.createFile(atPath: url.path, contents: nil)
        let handle = try XCTUnwrap(FileHandle(forWritingAtPath: url.path))
        defer { try? handle.close() }

        var warned = false
        let ok = DiagnosticWrite.append(Data("line\n".utf8), to: handle, site: "test", warned: &warned)

        XCTAssertTrue(ok)
        XCTAssertFalse(warned, "a successful write must not leave the warning latched")
    }

    func testAppendToNonWritableHandleFailsGracefullyInsteadOfRaising() throws {
        // A handle opened read-only can never accept a write — this is the
        // stand-in for a write syscall failure (e.g. ENOSPC on a full disk).
        // The old `FileHandle.write(_:)` convenience API raises an
        // NSException here and aborts the process; if this test method
        // returns at all, `DiagnosticWrite.append` is using the
        // throwing `write(contentsOf:)` API instead.
        let url = makeTempFile()
        defer { try? FileManager.default.removeItem(at: url) }
        FileManager.default.createFile(atPath: url.path, contents: nil)
        let handle = try XCTUnwrap(FileHandle(forReadingAtPath: url.path))
        defer { try? handle.close() }

        var warned = false
        let ok = DiagnosticWrite.append(Data("line\n".utf8), to: handle, site: "test", warned: &warned)

        XCTAssertFalse(ok)
        XCTAssertTrue(warned, "first failure on a fresh epoch must arm the warning")
    }

    func testRepeatedFailuresWithoutResetDoNotReArmTheWarning() throws {
        // Once `warned` is latched, further failures on the same epoch must
        // not toggle it again — that is what keeps a persistently-failing
        // writer (e.g. a disk that stays full) to one log line, not one
        // per dropped sample.
        let url = makeTempFile()
        defer { try? FileManager.default.removeItem(at: url) }
        FileManager.default.createFile(atPath: url.path, contents: nil)
        let handle = try XCTUnwrap(FileHandle(forReadingAtPath: url.path))
        defer { try? handle.close() }

        var warned = false
        _ = DiagnosticWrite.append(Data("line\n".utf8), to: handle, site: "test", warned: &warned)
        XCTAssertTrue(warned)

        let ok = DiagnosticWrite.append(Data("line\n".utf8), to: handle, site: "test", warned: &warned)
        XCTAssertFalse(ok)
        XCTAssertTrue(warned, "still latched — caller resets this on rotation, not on every failed call")
    }

    func testSuccessAfterFailureClearsTheWarning() throws {
        let url = makeTempFile()
        defer { try? FileManager.default.removeItem(at: url) }
        FileManager.default.createFile(atPath: url.path, contents: nil)
        let readOnly = try XCTUnwrap(FileHandle(forReadingAtPath: url.path))
        var warned = false
        _ = DiagnosticWrite.append(Data("line\n".utf8), to: readOnly, site: "test", warned: &warned)
        try readOnly.close()
        XCTAssertTrue(warned)

        let writable = try XCTUnwrap(FileHandle(forWritingAtPath: url.path))
        defer { try? writable.close() }
        let ok = DiagnosticWrite.append(Data("line\n".utf8), to: writable, site: "test", warned: &warned)

        XCTAssertTrue(ok)
        XCTAssertFalse(warned, "recovery re-arms the warning for the next failure")
    }
}
