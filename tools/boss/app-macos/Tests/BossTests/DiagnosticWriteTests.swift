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

    func testOpenForAppendingCreatesFileAndAppendsAtEnd() throws {
        let url = makeTempFile()
        defer { try? FileManager.default.removeItem(at: url) }
        try Data("existing\n".utf8).write(to: url)

        let handle = try XCTUnwrap(DiagnosticWrite.openForAppending(atPath: url.path))
        defer { try? handle.close() }

        try handle.write(contentsOf: Data("appended\n".utf8))
        let contents = try XCTUnwrap(String(contentsOf: url, encoding: .utf8))
        XCTAssertEqual(contents, "existing\nappended\n", "O_APPEND must land the write at EOF, not overwrite from the start")
    }

    func testOpenForAppendingCreatesTheFileWhenItDoesNotExist() throws {
        let url = makeTempFile()
        defer { try? FileManager.default.removeItem(at: url) }
        XCTAssertFalse(FileManager.default.fileExists(atPath: url.path))

        let handle = try XCTUnwrap(DiagnosticWrite.openForAppending(atPath: url.path))
        defer { try? handle.close() }

        XCTAssertTrue(FileManager.default.fileExists(atPath: url.path))
    }

    func testOpenForAppendingFailsGracefullyWhenTheDirectoryDoesNotExist() {
        let missingDir = URL(fileURLWithPath: NSTemporaryDirectory(), isDirectory: true)
            .appendingPathComponent("diagnostic-write-test-missing-\(UUID().uuidString)", isDirectory: true)
        let path = missingDir.appendingPathComponent("file.jsonl").path

        XCTAssertNil(DiagnosticWrite.openForAppending(atPath: path))
    }

    /// Regression coverage for the splice bug itself: two independent
    /// writers appending to the same path via `openForAppending` — each
    /// with its own `open(2)` call, standing in for the app process and the
    /// engine process both writing the shared IPC log — must never land one
    /// writer's bytes on top of the other's. Every line in the merged file
    /// parses cleanly and every emitted record shows up exactly once.
    func testConcurrentAppendersDoNotSpliceEachOthersRecords() throws {
        let url = makeTempFile()
        defer { try? FileManager.default.removeItem(at: url) }
        FileManager.default.createFile(atPath: url.path, contents: nil)

        let writersCount = 8
        let linesPerWriter = 200
        let group = DispatchGroup()
        let queue = DispatchQueue(label: "test.concurrent-append", attributes: .concurrent)

        for writer in 0..<writersCount {
            queue.async(group: group) {
                // Each writer opens its own independent fd, mirroring two
                // separate processes (app + engine) rather than two threads
                // sharing one FileHandle — that is the exact shape of the
                // bug (independent `open()`s racing on the same offset).
                guard let handle = DiagnosticWrite.openForAppending(atPath: url.path) else {
                    XCTFail("failed to open writer \(writer)")
                    return
                }
                defer { try? handle.close() }
                var warned = false
                for line in 0..<linesPerWriter {
                    let record = "{\"writer\":\(writer),\"line\":\(line)}\n"
                    _ = DiagnosticWrite.append(Data(record.utf8), to: handle, site: "test", warned: &warned)
                }
            }
        }
        group.wait()

        let contents = try XCTUnwrap(String(contentsOf: url, encoding: .utf8))
        let lines = contents.split(separator: "\n", omittingEmptySubsequences: true)
        var seen = Set<String>()
        for line in lines {
            let data = Data(line.utf8)
            let decoded = try XCTUnwrap(
                try? JSONSerialization.jsonObject(with: data) as? [String: Int],
                "every line must parse as a complete JSON object — a splice leaves a mid-record fragment instead: \(line)"
            )
            let key = "\(decoded["writer"]!)/\(decoded["line"]!)"
            XCTAssertTrue(seen.insert(key).inserted, "record \(key) appeared more than once — a splice can duplicate as well as corrupt")
        }
        XCTAssertEqual(lines.count, writersCount * linesPerWriter, "every record from every writer must survive the race intact")
    }

    /// The pair actually named in the corruption incident: the Swift sink
    /// and a non-Swift appender (here, `/bin/sh` shelling out `printf ...
    /// >>`, standing in for the Rust engine's `day-rotated-log` writer)
    /// racing on the same path. `O_APPEND` atomicity is a kernel property
    /// of the open file description, not of the language on either end, so
    /// this must hold exactly like the Swift-vs-Swift case above — this
    /// test exists to confirm that isn't just an assumption.
    func testConcurrentSwiftAndExternalProcessAppendersDoNotSpliceRecords() throws {
        let url = makeTempFile()
        defer { try? FileManager.default.removeItem(at: url) }
        FileManager.default.createFile(atPath: url.path, contents: nil)

        let linesPerWriter = 200
        let group = DispatchGroup()
        let queue = DispatchQueue(label: "test.concurrent-append-cross-process", attributes: .concurrent)

        // Swift-side writer: same shape as the app's diagnostic sinks.
        queue.async(group: group) {
            guard let handle = DiagnosticWrite.openForAppending(atPath: url.path) else {
                XCTFail("failed to open swift writer")
                return
            }
            defer { try? handle.close() }
            var warned = false
            for line in 0..<linesPerWriter {
                let record = "{\"writer\":\"swift\",\"line\":\(line)}\n"
                _ = DiagnosticWrite.append(Data(record.utf8), to: handle, site: "test", warned: &warned)
            }
        }

        // External-process writer: a `/bin/sh` subprocess opens its own fd
        // with `O_APPEND` via shell redirection (`exec 3>>`) and writes
        // through it in a loop, mirroring a separate non-Swift process
        // (e.g. the Rust engine) appending to the same log.
        let script = """
        exec 3>>"\(url.path)"
        i=0
        while [ "$i" -lt \(linesPerWriter) ]; do
            printf '{"writer":"shell","line":%d}\\n' "$i" >&3
            i=$((i + 1))
        done
        exec 3>&-
        """
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = ["-c", script]
        let processDone = DispatchSemaphore(value: 0)
        group.enter()
        queue.async(group: group) {
            defer {
                group.leave()
                processDone.signal()
            }
            do {
                try process.run()
                process.waitUntilExit()
                XCTAssertEqual(process.terminationStatus, 0, "shell appender must exit cleanly")
            } catch {
                XCTFail("failed to launch shell appender: \(error)")
            }
        }
        group.wait()

        let contents = try XCTUnwrap(String(contentsOf: url, encoding: .utf8))
        let lines = contents.split(separator: "\n", omittingEmptySubsequences: true)
        var seen = Set<String>()
        for line in lines {
            let data = Data(line.utf8)
            let decoded = try XCTUnwrap(
                try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                "every line must parse as a complete JSON object — a splice leaves a mid-record fragment instead: \(line)"
            )
            let writer = try XCTUnwrap(decoded["writer"] as? String)
            let recordLine = try XCTUnwrap(decoded["line"] as? Int)
            let key = "\(writer)/\(recordLine)"
            XCTAssertTrue(seen.insert(key).inserted, "record \(key) appeared more than once — a splice can duplicate as well as corrupt")
        }
        XCTAssertEqual(lines.count, 2 * linesPerWriter, "every record from both the swift and shell appenders must survive the race intact")
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
