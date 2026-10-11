import Darwin
import XCTest
@testable import Boss

@MainActor
final class QueuedReadReconnectTests: XCTestCase {
    func testWorkTreeAndReviewGuideQueuedDuringBackoffAreDelivered() async throws {
        let directory = ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory()
        let path = "\(directory)/replay-\(UUID().uuidString).sock"
        defer { unlink(path) }
        let model = ChatViewModel(socketPath: path)
        model.asyncMarkdownViewerOpener = {}
        let client = model.engine
        let disconnected = expectation(description: "entered reconnect backoff")
        let contentReceived = expectation(description: "viewer received queued guide response")
        var sawDisconnect = false
        var droppedRequests: [String] = []
        var transportErrors: [String] = []
        client.onEvent = { event in
            model.applyEventForTest(event)
            switch event {
            case .disconnected where !sawDisconnect:
                sawDisconnect = true
                disconnected.fulfill()
            case .notConnected(let kind): droppedRequests.append(kind)
            case .transportError(let message): transportErrors.append(message)
            case .reviewGuideContent: contentReceived.fulfill()
            default: break
            }
        }
        client.start()
        defer {
            client.onEvent = nil
            client.stop()
            model.applyEventForTest(.disconnected)
        }
        await fulfillment(of: [disconnected], timeout: 5)
        XCTAssertFalse(transportErrors.isEmpty, "a failed socket attempt must report a typed transport error")
        XCTAssertNil(model.workErrorMessage, "connection cleanup must not route socket failures to the modal")

        let treeID = try XCTUnwrap(client.sendLine(
            ["type": "get_work_tree", "product_id": "product"], queueIfDisconnected: true
        ))
        model.openReviewGuide(versionId: "version", rootTaskId: "root")
        let guideID = try XCTUnwrap(model.pendingReviewGuideRequestId)
        XCTAssertEqual(client.queuedReadCountForTesting, 2)
        guard case .loading = model.asyncMarkdownViewerVM.state else {
            return XCTFail("guide must wait for reconnect")
        }

        let wireReads = expectation(description: "both queued envelopes reached socket")
        wireReads.expectedFulfillmentCount = 2
        let server = try QueuedReadSocketServer(path: path) { payload, requestID in
            let kind = payload["type"] as? String
            // The model also refreshes its restored product on .connected;
            // distinguish that new read from the one queued during backoff.
            if kind == "get_work_tree", payload["product_id"] as? String == "product" {
                XCTAssertEqual(requestID, treeID)
                wireReads.fulfill()
            } else if kind == "get_review_guide_content" {
                XCTAssertEqual(requestID, guideID)
                wireReads.fulfill()
            }
        }
        defer { server.stop() }
        await fulfillment(of: [wireReads, contentReceived], timeout: 10)
        XCTAssertEqual(client.queuedReadCountForTesting, 0)
        XCTAssertTrue(droppedRequests.isEmpty)
        XCTAssertNil(model.pendingReviewGuideRequestId)
        guard case .loaded(_, let markdown, _) = model.asyncMarkdownViewerVM.state else {
            return XCTFail("the replayed guide response must finish loading")
        }
        XCTAssertEqual(markdown, "# Reconnected guide")
        XCTAssertNil(model.workErrorMessage)
    }
}

/// Mirrors the POSIX listener used by EngineClientReconnectBackoffTests,
/// retaining the accepted socket to inspect lines and answer the guide read.
private final class QueuedReadSocketServer {
    private let listenFD: Int32

    init(path: String, onRead: @escaping @Sendable ([String: Any], String) -> Void) throws {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw POSIXError(.EIO) }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let capacity = MemoryLayout.size(ofValue: address.sun_path)
        guard path.utf8.count < capacity else {
            close(fd)
            throw POSIXError(.ENAMETOOLONG)
        }
        _ = path.withCString { source in
            withUnsafeMutablePointer(to: &address.sun_path) { destination in
                memcpy(UnsafeMutableRawPointer(destination), source, strlen(source) + 1)
            }
        }
        let result = withUnsafePointer(to: address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard result == 0, listen(fd, 1) == 0 else {
            let error = POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
            close(fd)
            throw error
        }
        listenFD = fd
        DispatchQueue.global().async {
            var descriptor = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
            guard poll(&descriptor, 1, 10_000) > 0 else { return }
            let peer = accept(fd, nil, nil)
            guard peer >= 0 else { return }
            defer { close(peer) }
            var timeout = timeval(tv_sec: 10, tv_usec: 0)
            setsockopt(peer, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
            var noSigPipe: Int32 = 1
            setsockopt(peer, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout<Int32>.size))
            var buffer = Data()
            var bytes = [UInt8](repeating: 0, count: 4096)
            while true {
                let count = read(peer, &bytes, bytes.count)
                guard count > 0 else { return }
                buffer.append(contentsOf: bytes.prefix(count))
                while let newline = buffer.firstIndex(of: 0x0A) {
                    let line = Data(buffer[..<newline])
                    buffer.removeSubrange(...newline)
                    guard let envelope = try? JSONSerialization.jsonObject(with: line) as? [String: Any],
                          let id = envelope["request_id"] as? String,
                          let payload = envelope["payload"] as? [String: Any],
                          let kind = payload["type"] as? String else { continue }
                    onRead(payload, id)
                    if kind == "get_review_guide_content" {
                        let reply: [String: Any] = [
                            "request_id": id,
                            "payload": [
                                "type": "review_guide_content", "version_id": "version",
                                "content": [
                                    "id": "version", "series_id": "series", "comparison_id": "comparison",
                                    "attempt_id": "attempt", "markdown": "# Reconnected guide",
                                    "content_hash": "hash", "prompt_version": "v1", "generated_at": "2026-01-01",
                                ],
                            ],
                        ]
                        guard var data = try? JSONSerialization.data(withJSONObject: reply) else { return }
                        data.append(0x0A)
                        data.withUnsafeBytes { bytes in
                            var offset = 0
                            while offset < bytes.count {
                                let written = write(peer, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
                                guard written > 0 else { return }
                                offset += written
                            }
                        }
                    }
                }
            }
        }
    }

    func stop() {
        shutdown(listenFD, SHUT_RDWR)
        close(listenFD)
    }
}
