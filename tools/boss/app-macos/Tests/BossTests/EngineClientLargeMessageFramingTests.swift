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
    func testPartialFrameIsNotJoinedToNextConnection() throws {
        try assertReconnects(
            after: Data("{\"payload\":{\"type\":\"error\",\"message\":\"partial".utf8),
            closeFirstImmediately: true
        )
    }

    func testInvalidFrameDisconnectsWithoutDeliveringRemainingFramesOrModal() throws {
        try assertReconnects(after: Data("not-json\n{\"payload\":{\"type\":\"error\",\"message\":\"must not deliver\"}}\n".utf8))
    }

    func testInvalidEnvelopeDisconnectsAndResyncs() throws {
        try assertReconnects(after: Data("{\"payload\":{\"type\":42}}\n".utf8))
    }

    func testDiagnosticsPreserveBoundedRawBytes() throws {
        let bytes = Data([0xFF, 0x00, 0x0A] + Array(repeating: UInt8(0x80), count: 1024))
        let diagnostic = EngineClient.frameDiagnostic(bytes, error: "invalid UTF-8")
        XCTAssertEqual(diagnostic["length_bytes"] as? Int, bytes.count)
        XCTAssertEqual(diagnostic["error"] as? String, "invalid UTF-8")
        XCTAssertEqual(Data(base64Encoded: try XCTUnwrap(diagnostic["prefix_base64"] as? String)), bytes.prefix(128))
        XCTAssertEqual(Data(base64Encoded: try XCTUnwrap(diagnostic["suffix_base64"] as? String)), bytes.suffix(128))
        XCTAssertThrowsError(try EngineClient.decodeEnvelope(bytes))
    }

    private func assertReconnects(after firstFrame: Data, closeFirstImmediately: Bool = false) throws {
        let directory = ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory()
        let path = "\(directory)/boss-reconnect-\(UUID().uuidString).sock"
        defer { unlink(path) }
        let server = try LineWritingUnixSocketServer(path: path)
        let recovered = Data("{\"payload\":{\"type\":\"error\",\"message\":\"recovered\"}}\n".utf8)
        server.acceptAndWrite([firstFrame, recovered], closeFirstImmediately: closeFirstImmediately)
        let received = expectation(description: "fresh connection decodes normally")
        let client = EngineClient(socketPath: path)
        client.onEvent = { event in
            if case .error(let message) = event {
                XCTAssertEqual(message, "recovered", "transport failures must not become modal work errors")
                if message == "recovered" { received.fulfill() }
            }
        }
        client.start()
        defer { client.stop() }
        wait(for: [received], timeout: 5)
        withExtendedLifetime(server) {}
    }

    func testLargeSingleLineMessageParsesPromptly() throws {
        let temporaryDirectory = ProcessInfo.processInfo.environment["TEST_TMPDIR"]
            ?? NSTemporaryDirectory()
        let socketPath = "\(temporaryDirectory)/boss-engineclient-test-\(UUID().uuidString).sock"
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
        acceptAndWrite([payload])
    }

    func acceptAndWrite(_ payloads: [Data], closeFirstImmediately: Bool = false) {
        // Retain the listener until the final accept completes; a captured
        // integer descriptor does not keep the owning object alive.
        DispatchQueue.global(qos: .userInitiated).async { [self] in
            for (index, payload) in payloads.enumerated() {
                let clientFD = accept(listenFD, nil, nil)
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
                if index == 0 && closeFirstImmediately {
                    // Deliver an orderly EOF after the partial bytes, rather
                    // than resetting the socket while Network is reading it.
                    shutdown(clientFD, SHUT_WR)
                }
                // Keep the read half open until the client closes it. This
                // proves malformed frames themselves cause reconnect, and
                // avoids racing successful delivery with a server reset.
                var byte: UInt8 = 0
                while Darwin.read(clientFD, &byte, 1) > 0 {}
            }
        }
    }

    deinit {
        close(listenFD)
    }
}
