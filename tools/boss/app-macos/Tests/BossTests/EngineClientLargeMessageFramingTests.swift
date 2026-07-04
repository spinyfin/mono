import Darwin
import XCTest
import os
@testable import Boss

/// Regression guard for the `consumeLines()` O(n²) buffer rescan documented
/// in `docs/investigations/task-population-latency-on-start-and-product-switch.md`
/// §11: every appended ~64 KiB chunk of a large single-line message used to
/// re-scan the *whole* accumulated buffer from the start looking for the
/// newline delimiter, making parse time quadratic in message size (measured
/// ~7.5s of pure scan overhead for a ~6 MB `work_tree` reply in production).
/// `EngineClient` now tracks a scan cursor so each newly appended chunk only
/// scans its own bytes.
///
/// This spins up a real Unix-domain socket (no Rust engine needed), writes
/// one large newline-terminated JSON line to it, and asserts the client
/// parses it well within the time the old quadratic scan would have taken.
final class EngineClientLargeMessageFramingTests: XCTestCase {
    func testLargeSingleLineMessageParsesPromptly() throws {
        let socketPath = "/tmp/boss-engineclient-test-\(UUID().uuidString).sock"
        defer { unlink(socketPath) }

        // ~6 MB payload — matches the size the investigation measured
        // taking ~7.5s of scan overhead under the O(n²) bug; the fixed
        // client should handle it in a small fraction of a second.
        let bigString = String(repeating: "x", count: 6 * 1024 * 1024)
        let envelope: [String: Any] = [
            "request_id": "test",
            "payload": ["type": "error", "message": bigString],
        ]
        var line = try JSONSerialization.data(withJSONObject: envelope, options: [])
        line.append(0x0A)

        let server = try LineWritingUnixSocketServer(path: socketPath)
        server.acceptOnceAndWrite(line)

        let received = OSAllocatedUnfairLock(initialState: String?.none)
        let exp = expectation(description: "large error event received")
        let client = EngineClient(socketPath: socketPath)
        client.onEvent = { event in
            if case .error(let message) = event, message == bigString {
                received.withLock { $0 = message }
                exp.fulfill()
            }
        }
        client.start()
        defer { client.stop() }

        let start = Date()
        wait(for: [exp], timeout: 15)
        let elapsed = Date().timeIntervalSince(start)

        XCTAssertEqual(received.withLock { $0 }?.count, bigString.count)
        // Generous relative to the fixed client's expected sub-second cost,
        // but far below what the O(n²) scan took on a similarly sized
        // payload in production — catches a reintroduced full-buffer rescan.
        XCTAssertLessThan(
            elapsed, 4.0,
            "large single-line message took \(elapsed)s to parse — possible quadratic buffer rescan regression"
        )
    }
}

/// Minimal Unix-domain socket listener for tests: accepts exactly one
/// connection and writes a fixed payload to it. Mirrors the raw-socket
/// pattern in `EngineProcessController`'s version-check probe, but as the
/// server side, so tests don't need a real engine process.
private final class LineWritingUnixSocketServer {
    private let listenFD: Int32

    init(path: String) throws {
        unlink(path)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else {
            throw NSError(domain: "LineWritingUnixSocketServer", code: 1)
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
            throw NSError(domain: "LineWritingUnixSocketServer", code: 2)
        }
        guard listen(fd, 1) == 0 else {
            close(fd)
            throw NSError(domain: "LineWritingUnixSocketServer", code: 3)
        }
        listenFD = fd
    }

    /// Accepts one connection on a background queue and writes `payload` to
    /// it as a single `write()` call — the OS and `NWConnection`'s
    /// `maximumLength: 64 * 1024` receive cap are what fragment it into
    /// ~64 KiB chunks on the client side, matching production.
    func acceptOnceAndWrite(_ payload: Data) {
        let fd = listenFD
        DispatchQueue.global(qos: .userInitiated).async {
            let clientFD = accept(fd, nil, nil)
            guard clientFD >= 0 else { return }
            defer { close(clientFD) }
            payload.withUnsafeBytes { (buf: UnsafeRawBufferPointer) in
                guard let base = buf.baseAddress else { return }
                var offset = 0
                while offset < buf.count {
                    let n = Darwin.write(clientFD, base + offset, buf.count - offset)
                    if n <= 0 { break }
                    offset += n
                }
            }
        }
    }

    deinit {
        close(listenFD)
    }
}
