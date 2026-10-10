import Darwin
import XCTest
import os
@testable import Boss

/// Drives a real `EngineClient` over a Unix socket with one decode blocked,
/// asserting that lifecycle events (`.disconnected`, `.connected`) are
/// delivered behind the replies already handed to the decoder.
final class EngineClientLifecycleOrderingTests: XCTestCase {
    private enum Seen: Equatable {
        case reply(String)
        case connected
        case disconnected
    }

    private func line(_ message: String) throws -> Data {
        let envelope: [String: Any] = [
            "request_id": "test",
            "payload": ["type": "error", "message": message],
        ]
        var data = try JSONSerialization.data(withJSONObject: envelope, options: [])
        data.append(0x0A)
        return data
    }

    private func socketPath() -> String {
        let dir = ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory()
        return "\(dir)/boss-lifecycle-\(UUID().uuidString.prefix(8)).sock"
    }

    private func makeClient(
        path: String, seen: OSAllocatedUnfairLock<[Seen]>, blocking blockedMessage: String,
        started: DispatchSemaphore, release: DispatchSemaphore
    ) -> EngineClient {
        let client = EngineClient(socketPath: path)
        client.setDecodeHookForTesting { data in
            if String(decoding: data, as: UTF8.self).contains(blockedMessage) {
                started.signal()
                release.wait()
            }
        }
        client.onEvent = { event in
            switch event {
            case .connected: seen.withLock { $0.append(.connected) }
            case .disconnected: seen.withLock { $0.append(.disconnected) }
            case .error(let message): seen.withLock { $0.append(.reply(message)) }
            default: break
            }
        }
        return client
    }

    private func waitUntil(_ timeout: TimeInterval, _ predicate: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if predicate() { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.02))
        }
        return predicate()
    }

    /// Receive-EOF termination: old reply (decode blocked) → disconnected →
    /// reconnect's connected → new reply.
    func testEofDisconnectAndReconnectStayBehindBlockedDecode() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let seen = OSAllocatedUnfairLock(initialState: [Seen]())
        let started = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let client = makeClient(
            path: path, seen: seen, blocking: "old-reply", started: started, release: release)
        client.start()
        defer { client.stop() }

        let first = try XCTUnwrap(server.accept(timeout: 5))
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0 } == [.connected] })
        first.write(try line("old-reply"))
        XCTAssertEqual(started.wait(timeout: .now() + 5), .success)
        first.close()  // EOF while the old decode is blocked

        let second = try XCTUnwrap(server.accept(timeout: 10), "client reconnected")
        second.write(try line("new-reply"))
        // Neither lifecycle event may overtake the blocked old reply.
        RunLoop.current.run(until: Date().addingTimeInterval(0.5))
        XCTAssertEqual(seen.withLock { $0 }, [.connected])

        release.signal()
        let expected: [Seen] = [
            .connected, .reply("old-reply"), .disconnected, .connected, .reply("new-reply"),
        ]
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0.count >= expected.count } })
        XCTAssertEqual(seen.withLock { $0 }, expected)
        second.close()
    }

    func testDelayedMalformedFrameCannotCloseReplacementConnection() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let seen = OSAllocatedUnfairLock(initialState: [Seen]())
        let started = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let client = makeClient(
            path: path, seen: seen, blocking: "invalid-frame", started: started, release: release)
        client.start()
        defer { client.stop() }
        defer { release.signal() }

        let first = try XCTUnwrap(server.accept(timeout: 5))
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0 } == [.connected] })
        first.write(Data("invalid-frame\n".utf8) + (try line("rejected-reply")))
        XCTAssertEqual(started.wait(timeout: .now() + 5), .success)
        first.close()
        let second = try XCTUnwrap(server.accept(timeout: 10))
        defer { second.close() }
        second.write(try line("new-reply"))
        release.signal()

        let expected: [Seen] = [.connected, .disconnected, .connected, .reply("new-reply")]
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0.count >= expected.count } })
        XCTAssertEqual(seen.withLock { $0 }, expected)
        second.write(try line("still-connected"))
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0.contains(.reply("still-connected")) } })
        XCTAssertEqual(seen.withLock { $0 }, expected + [.reply("still-connected")])
    }

    /// State-callback termination (`cancel()` → `.cancelled`) while a decode
    /// is blocked: exactly one `.disconnected`, after the old reply.
    func testStateCallbackDisconnectStaysBehindBlockedDecode() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let seen = OSAllocatedUnfairLock(initialState: [Seen]())
        let started = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let client = makeClient(
            path: path, seen: seen, blocking: "old-reply", started: started, release: release)
        client.start()

        let conn = try XCTUnwrap(server.accept(timeout: 5))
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0 } == [.connected] })
        conn.write(try line("old-reply"))
        XCTAssertEqual(started.wait(timeout: .now() + 5), .success)
        client.stop()  // cancels the connection → .cancelled state callback

        RunLoop.current.run(until: Date().addingTimeInterval(0.5))
        XCTAssertEqual(seen.withLock { $0 }, [.connected], "disconnect held behind blocked decode")

        release.signal()
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0.count >= 3 } })
        RunLoop.current.run(until: Date().addingTimeInterval(0.3))
        XCTAssertEqual(seen.withLock { $0 }, [.connected, .reply("old-reply"), .disconnected])
        conn.close()
    }
}

/// Unix-domain listener whose accepted connections the test controls.
private final class TestSocketServer {
    final class Peer {
        private let fd: Int32
        init(fd: Int32) { self.fd = fd }
        func write(_ data: Data) {
            data.withUnsafeBytes { (buf: UnsafeRawBufferPointer) in
                var offset = 0
                while let base = buf.baseAddress, offset < buf.count {
                    let n = Darwin.write(fd, base + offset, buf.count - offset)
                    if n <= 0 { break }
                    offset += n
                }
            }
        }
        func close() { Darwin.close(fd) }
    }

    private let listenFD: Int32

    init(path: String) throws {
        unlink(path)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw NSError(domain: "TestSocketServer", code: 1) }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let sunPathMax = MemoryLayout.size(ofValue: addr.sun_path)
        _ = path.withCString { cStr in
            withUnsafeMutablePointer(to: &addr.sun_path) { dst in
                memcpy(UnsafeMutableRawPointer(dst), cStr, min(strlen(cStr), sunPathMax - 1))
            }
        }
        let bound = withUnsafePointer(to: addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard bound == 0, listen(fd, 4) == 0 else {
            Darwin.close(fd)
            throw NSError(domain: "TestSocketServer", code: 2)
        }
        listenFD = fd
    }

    /// Blocks (up to `timeout`) for the next connection.
    func accept(timeout: TimeInterval) -> Peer? {
        var pfd = pollfd(fd: listenFD, events: Int16(POLLIN), revents: 0)
        guard poll(&pfd, 1, Int32(timeout * 1000)) > 0 else { return nil }
        let fd = Darwin.accept(listenFD, nil, nil)
        return fd >= 0 ? Peer(fd: fd) : nil
    }

    deinit { Darwin.close(listenFD) }
}
