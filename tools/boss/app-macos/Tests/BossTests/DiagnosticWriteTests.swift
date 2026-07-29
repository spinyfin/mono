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

    func testCloseQuietlyClosesAnOpenHandleWithoutRaising() throws {
        let url = makeTempFile()
        defer { try? FileManager.default.removeItem(at: url) }
        FileManager.default.createFile(atPath: url.path, contents: nil)
        let handle = try XCTUnwrap(FileHandle(forWritingAtPath: url.path))

        DiagnosticWrite.closeQuietly(handle)

        // A handle FileHandle already closed raises on `write(_:)`, which is
        // the cheapest way to confirm `close()` actually ran.
        XCTAssertThrowsError(try handle.write(contentsOf: Data("x".utf8)))
    }

    func testCloseQuietlyToleratesNilHandle() {
        // Must not raise/crash — this is the exact call shape `openFile`
        // makes on the very first rotation, before any handle exists.
        DiagnosticWrite.closeQuietly(nil)
    }

    func testSeekToEndQuietlySucceedsOnWritableHandle() throws {
        let url = makeTempFile()
        defer { try? FileManager.default.removeItem(at: url) }
        FileManager.default.createFile(atPath: url.path, contents: Data("existing\n".utf8))
        let handle = try XCTUnwrap(FileHandle(forWritingAtPath: url.path))
        defer { try? handle.close() }

        XCTAssertTrue(DiagnosticWrite.seekToEndQuietly(handle))

        try handle.write(contentsOf: Data("appended\n".utf8))
        let contents = try XCTUnwrap(String(contentsOf: url, encoding: .utf8))
        XCTAssertEqual(contents, "existing\nappended\n", "seek must have landed at EOF, not overwritten from the start")
    }

    func testSeekToEndQuietlyFailsGracefullyOnAClosedHandleInsteadOfRaising() throws {
        // A handle whose fd is already gone can never seek — this stands in
        // for a failing `lseek(2)` (e.g. ENOSPC/EIO on rotation). The old
        // `seekToEndOfFile()` raises an NSException here; if this test
        // method returns at all, the quiet variant is in use.
        let url = makeTempFile()
        defer { try? FileManager.default.removeItem(at: url) }
        FileManager.default.createFile(atPath: url.path, contents: nil)
        let handle = try XCTUnwrap(FileHandle(forWritingAtPath: url.path))
        try handle.close()

        XCTAssertFalse(DiagnosticWrite.seekToEndQuietly(handle))
    }
}

/// End-to-end coverage: an actual rotating JSONL writer must survive (not
/// crash, not deadlock) when its on-disk write path fails outright, and
/// must keep serving its in-memory ring regardless.
final class TerminalLoopLogWriterSurvivalTests: XCTestCase {
    func testRecordSurvivesAndKeepsRingUpdatedWhenTheDirectoryIsUnwritable() throws {
        let tmpRoot = URL(fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(), isDirectory: true)
        let dir = tmpRoot.appendingPathComponent("terminal-loop-log-readonly-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer {
            try? FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: dir.path)
            try? FileManager.default.removeItem(at: dir)
        }
        // Read+execute only: `openFile` can stat the directory but neither
        // create nor write the rotated JSONL file inside it.
        try FileManager.default.setAttributes([.posixPermissions: 0o500], ofItemAtPath: dir.path)

        let log = TerminalLoopLog(directory: dir.path)
        let sample = LoopSample(
            tsEpochMs: 1_700_000_000_000,
            wakeupsPerSec: 1,
            ticksPerSec: 1,
            intervalMs: 1000,
            panes: []
        )

        log.record(sample)
        log.flushForTesting()

        // Reaching this line at all is the crash-survival assertion: the old
        // `write(_:)`/`seekToEndOfFile()` NSFileHandle APIs would have
        // raised an uncatchable NSException on this path.
        XCTAssertEqual(log.loopSnapshot(), [sample], "the in-memory ring must stay authoritative even when the disk mirror can't be written")
    }
}
