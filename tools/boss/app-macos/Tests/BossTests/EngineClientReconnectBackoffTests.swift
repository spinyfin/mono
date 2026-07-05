import Darwin
import XCTest
import os
@testable import Boss

/// Regression guard for the reconnect backoff added so a dropped
/// frontend socket (e.g. the engine tearing down a wedged/slow app
/// session, or a transient socket error) doesn't hammer the engine with
/// a fresh connect attempt every second forever. `EngineClient` now
/// escalates its reconnect delay across consecutive failed attempts
/// (`reconnectDelays`) instead of a flat 1-second retry, and only
/// resets the delay once `RegisterAppSession` actually completes.
///
/// This spins up a real Unix-domain socket server (no Rust engine
/// needed) that accepts each connection and immediately closes it
/// without ever sending `app_session_registered`, so every reconnect
/// attempt counts as a fresh failure and the gaps between accepts
/// should escalate.
final class EngineClientReconnectBackoffTests: XCTestCase {
    func testReconnectDelayEscalatesAcrossConsecutiveFailures() throws {
        let socketPath = "/tmp/boss-engineclient-backoff-test-\(UUID().uuidString).sock"
        defer { unlink(socketPath) }

        let server = try ImmediatelyClosingUnixSocketServer(path: socketPath)
        let acceptTimes = OSAllocatedUnfairLock(initialState: [Date]())
        let neededAccepts = 4
        let exp = expectation(description: "enough reconnect attempts observed")

        server.acceptRepeatedlyAndCloseImmediately {
            let count: Int = acceptTimes.withLock {
                $0.append(Date())
                return $0.count
            }
            if count == neededAccepts {
                exp.fulfill()
            }
        }

        let client = EngineClient(socketPath: socketPath)
        client.onEvent = { _ in }
        client.start()
        defer { client.stop() }

        // Delays are 0.5, 1, 2, 4... seconds; four accepts span ~3.5s of
        // scheduled backoff plus connection overhead.
        wait(for: [exp], timeout: 10)

        let times = acceptTimes.withLock { $0 }
        XCTAssertEqual(times.count, neededAccepts)

        let gaps = zip(times, times.dropFirst()).map { $1.timeIntervalSince($0) }
        XCTAssertEqual(gaps.count, neededAccepts - 1)

        // The core property under test: backoff, not a flat retry — each
        // gap must be meaningfully larger than the previous one. A flat
        // 1-second retry (the pre-fix behavior) would make every gap
        // roughly equal and fail this.
        for i in 1..<gaps.count {
            XCTAssertGreaterThan(
                gaps[i], gaps[i - 1] * 1.3,
                "reconnect gap \(i) (\(gaps[i])s) did not escalate over gap \(i - 1) (\(gaps[i - 1])s) — backoff regression?"
            )
        }

        // Loose absolute bounds matching the 0.5/1/2 schedule, generous
        // enough to absorb scheduling jitter without caring about exact
        // timing.
        XCTAssertGreaterThan(gaps[0], 0.3)
        XCTAssertLessThan(gaps[0], 1.2)
        XCTAssertGreaterThan(gaps[2], 1.2)
    }
}

/// Minimal Unix-domain socket listener for tests: accepts connections in
/// a loop and closes each one immediately after `invokingAcceptHandler`,
/// simulating an engine that never completes the `RegisterAppSession`
/// handshake. Mirrors `LineWritingUnixSocketServer` in
/// `EngineClientLargeMessageFramingTests.swift` but repeats instead of
/// accepting once.
private final class ImmediatelyClosingUnixSocketServer {
    private let listenFD: Int32

    init(path: String) throws {
        unlink(path)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else {
            throw NSError(domain: "ImmediatelyClosingUnixSocketServer", code: 1)
        }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let sunPathMax = MemoryLayout.size(ofValue: addr.sun_path)
        _ = path.withCString { cStr in
            withUnsafeMutablePointer(to: &addr.sun_path) { dst in
                memcpy(UnsafeMutableRawPointer(dst), cStr, min(strlen(cStr), sunPathMax - 1))
            }
        }
        let bindResult = withUnsafePointer(to: addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard bindResult == 0 else {
            close(fd)
            throw NSError(domain: "ImmediatelyClosingUnixSocketServer", code: 2)
        }
        guard listen(fd, 8) == 0 else {
            close(fd)
            throw NSError(domain: "ImmediatelyClosingUnixSocketServer", code: 3)
        }
        listenFD = fd
    }

    /// Accepts connections forever (until the test process tears the
    /// listener down in `deinit`), closing each one right away and
    /// invoking `onAccept` synchronously from the accept loop's
    /// background queue.
    func acceptRepeatedlyAndCloseImmediately(onAccept: @escaping @Sendable () -> Void) {
        let fd = listenFD
        DispatchQueue.global(qos: .userInitiated).async {
            while true {
                let clientFD = accept(fd, nil, nil)
                guard clientFD >= 0 else { return }
                close(clientFD)
                onAccept()
            }
        }
    }

    deinit {
        close(listenFD)
    }
}
