import Darwin
import XCTest
import os
@testable import Boss

/// Drives a real `EngineClient` over a Unix socket with one decode blocked,
/// asserting that lifecycle events (`.disconnected`, `.connected`) are
/// delivered behind the replies already handed to the decoder.
final class EngineClientLifecycleOrderingTests: XCTestCase {
    func testAcceptedQueuedWritesReserveCapacityAcrossDisconnect() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let client = EngineClient(socketPath: path)
        let entered = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let rejected = OSAllocatedUnfairLock(initialState: 0)
        client.onEvent = { event in
            if case .actionNotSaved = event { rejected.withLock { $0 += 1 } }
        }
        client.start()
        defer { release.signal(); client.stop() }
        let first = try XCTUnwrap(server.accept(timeout: 5))
        XCTAssertTrue(waitUntil(5) { client.isReachable })
        client.setBeforeWriteHookForTesting { _, _ in
            entered.signal()
            _ = release.wait(timeout: .now() + 10)
        }
        client.sendLine(["type": "create_chore", "name": "attempted"])
        XCTAssertEqual(entered.wait(timeout: .now() + 5), .success)
        // Teardown is ordered ahead of these writes, but the caller still sees ready.
        client.stop()
        for index in 0..<EngineClient.maxUnsentActions {
            XCTAssertNotNil(client.sendLine(["type": "create_chore", "name": "reserved-\(index)"]))
        }
        XCTAssertNil(client.sendLine(["type": "create_chore", "name": "rejected"]))
        client.setBeforeWriteHookForTesting(nil)
        release.signal()
        XCTAssertTrue(waitUntil(5) { client.unsentActionCountForTesting == EngineClient.maxUnsentActions })
        XCTAssertTrue(waitUntil(5) { rejected.withLock { $0 == 1 } })
        first.close()
        client.start()
        let second = try XCTUnwrap(server.accept(timeout: 5))
        defer { second.close() }
        XCTAssertTrue(waitUntil(5) { client.isReachable })
        XCTAssertTrue(client.retryUnsentActions())
        var names: Set<String> = []
        for _ in 0..<EngineClient.maxUnsentActions {
            let envelope = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(second.readLine(timeout: 5))) as? [String: Any])
            let payload = try XCTUnwrap(envelope["payload"] as? [String: Any])
            names.insert(try XCTUnwrap(payload["name"] as? String))
        }
        XCTAssertEqual(names, Set((0..<EngineClient.maxUnsentActions).map { "reserved-\($0)" }))
        XCTAssertNil(second.readLine(timeout: 0.2))
    }

    func testBufferedEngineRequestAfterSocketLossCannotEnterRetryBatch() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let client = EngineClient(socketPath: path)
        let started = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let answered = OSAllocatedUnfairLock(initialState: false)
        client.setDecodeHookForTesting { data in
            if String(decoding: data, as: UTF8.self).contains("engine_request") {
                started.signal()
                _ = release.wait(timeout: .now() + 10)
            }
        }
        client.onEvent = { event in
            if case .engineRequest(let id, _) = event {
                client.sendListHostedPanesResponse(requestId: id, panes: [])
                answered.withLock { $0 = true }
            }
        }
        client.start()
        defer { release.signal(); client.stop() }
        let first = try XCTUnwrap(server.accept(timeout: 5))
        XCTAssertTrue(waitUntil(5) { client.isReachable })
        var request = try JSONSerialization.data(withJSONObject: [
            "payload": ["type": "engine_request", "request_id": "old-rpc", "request": ["kind": "list_hosted_panes"]],
        ])
        request.append(0x0A)
        first.sendBytes(request)
        XCTAssertEqual(started.wait(timeout: .now() + 5), .success)
        first.close()
        XCTAssertTrue(waitUntil(5) { !client.isReachable })
        client.sendLine(["type": "create_chore", "name": "the-only-mutation"])
        client.sendLine(["type": "list_products"])
        release.signal()
        XCTAssertTrue(waitUntil(5) { answered.withLock { $0 } })
        XCTAssertEqual(client.unsentActionCountForTesting, 1)
        XCTAssertTrue(client.uncertainActionsForTesting.isEmpty)
        let second = try XCTUnwrap(server.accept(timeout: 10))
        defer { second.close() }
        XCTAssertTrue(waitUntil(5) { client.isReachable })
        XCTAssertTrue(client.retryUnsentActions())
        let envelope = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(second.readLine(timeout: 5))) as? [String: Any])
        XCTAssertEqual((envelope["payload"] as? [String: Any])?["type"] as? String, "create_chore")
        XCTAssertNil(second.readLine(timeout: 0.2))
    }

    @MainActor
    func testMixedSocketOutcomesKeepUncertaintyAfterRetryAcknowledgment() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let model = ChatViewModel(socketPath: path)
        let client = model.engine
        client.onEvent = { event in
            if case .connected = event { model.isConnected = true }
            else { model.applyEventForTest(event) }
        }
        client.start()
        defer { client.stop() }
        let first = try XCTUnwrap(server.accept(timeout: 5))
        XCTAssertTrue(waitUntil(5) { model.isConnected })
        let uncertainID = try XCTUnwrap(client.sendLine(["type": "update_work_item", "id": "task", "patch": [:]]))
        XCTAssertNotNil(first.readLine(timeout: 5))
        first.close()
        XCTAssertTrue(waitUntil(5) { !model.isConnected })
        client.sendLine(["type": "create_chore", "name": "queued"])
        XCTAssertTrue(waitUntil(2) { model.hasUnsentActions })
        XCTAssertTrue(model.disconnectedActionNotice?.contains(uncertainID) == true)
        let second = try XCTUnwrap(server.accept(timeout: 10))
        defer { second.close() }
        XCTAssertTrue(waitUntil(5) { model.isConnected })
        model.retryDisconnectedActions()
        let envelope = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(second.readLine(timeout: 5))) as? [String: Any])
        var reply = try JSONSerialization.data(withJSONObject: [
            "request_id": try XCTUnwrap(envelope["request_id"] as? String),
            "payload": ["type": "work_error", "message": "fixture refusal"],
        ])
        reply.append(0x0A)
        second.sendBytes(reply)
        XCTAssertTrue(waitUntil(5) { !model.retryingDisconnectedActions })
        XCTAssertTrue(model.disconnectedActionNotice?.contains(uncertainID) == true)
        model.dismissDisconnectedActions()
        XCTAssertTrue(model.disconnectedActionNotice?.contains(uncertainID) == true)
        XCTAssertEqual(client.uncertainActionsForTesting, [uncertainID: "update_work_item"])
    }

    func testSessionReplyWithoutDirectResponseDoesNotAwaitConfirmation() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let client = EngineClient(socketPath: path)
        client.start()
        defer { client.stop() }
        let peer = try XCTUnwrap(server.accept(timeout: 5))
        XCTAssertTrue(waitUntil(5) { client.isReachable })
        client.sendLine(["type": "engine_response", "request_id": "rpc", "response": ["kind": "pane_state"]])
        XCTAssertNotNil(peer.readLine(timeout: 5))
        client.sendLine(["type": "release_review_terminal", "lease_id": "closed-window"])
        XCTAssertNotNil(peer.readLine(timeout: 5))
        XCTAssertEqual(client.awaitingReplyCountForTesting, 0)
        peer.close()
        XCTAssertTrue(waitUntil(5) { !client.isReachable })
        XCTAssertTrue(client.uncertainActionsForTesting.isEmpty)
        XCTAssertFalse(client.hasUnsentActions)
    }

    @MainActor
    func testDisconnectedDragRetriesAdmissionThenExactlyOneMove() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let model = ChatViewModel(socketPath: path)
        let client = model.engine
        client.onEvent = { event in
            if case .connected = event { model.isConnected = true }
            else {
                if case .dispatchAdmissionEvaluated = event {
                    // Discard must not erase the already-started semantic retry.
                    model.dismissDisconnectedActions()
                    XCTAssertTrue(client.hasRetriedActions)
                }
                model.applyEventForTest(event)
            }
        }
        let task = WorkTask(id: "drag-task", productID: "product", projectID: nil, kind: "chore",
                            name: "Drag me", description: "", status: "todo", priority: "medium",
                            ordinal: nil, prURL: nil, deletedAt: nil, createdAt: "", updatedAt: "", autostart: false)
        model.choresByProductID = ["product": [task]]
        model.applyEventForTest(.disconnected)
        XCTAssertTrue(model.attemptDrop(task.id, onColumn: .doing, group: nil))
        XCTAssertTrue(waitUntil(2) { model.hasUnsentActions })
        XCTAssertNotNil(model.pendingDragAdmissionCheck)
        client.start()
        defer { client.stop() }
        let peer = try XCTUnwrap(server.accept(timeout: 5))
        defer { peer.close() }
        XCTAssertTrue(waitUntil(5) { model.isConnected })
        model.retryDisconnectedActions()
        let admission = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(peer.readLine(timeout: 5))) as? [String: Any])
        XCTAssertEqual((admission["payload"] as? [String: Any])?["type"] as? String, "evaluate_dispatch_admission")
        var reply = try JSONSerialization.data(withJSONObject: [
            "request_id": try XCTUnwrap(admission["request_id"] as? String),
            "payload": ["type": "dispatch_admission_evaluated", "admission": [
                "work_item_id": task.id, "would_dispatch": true,
                "pause": ["active": false, "overridable": false], "blockers": [],
            ]],
        ])
        reply.append(0x0A)
        peer.sendBytes(reply)
        XCTAssertTrue(waitUntil(5) { model.pendingDragAdmissionCheck == nil })
        XCTAssertTrue(model.retryingDisconnectedActions, "admission is not completion of the move")
        let move = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(peer.readLine(timeout: 5))) as? [String: Any])
        XCTAssertEqual((move["payload"] as? [String: Any])?["type"] as? String, "move_work_item_on_board")
        XCTAssertNil(peer.readLine(timeout: 0.2), "exactly one move follows admission")
        var acknowledgment = try JSONSerialization.data(withJSONObject: [
            "request_id": try XCTUnwrap(move["request_id"] as? String),
            "payload": ["type": "work_error", "message": "fixture refusal"],
        ])
        acknowledgment.append(0x0A)
        peer.sendBytes(acknowledgment)
        XCTAssertTrue(waitUntil(5) { !model.retryingDisconnectedActions })
        XCTAssertNil(model.disconnectedActionNotice)
    }

    func testPeerClosesImmediatelyBeforeAttemptedMutationWrite() throws {
        for kind in ["create_chore", "update_work_item", "answer_operator_question"] {
            let path = socketPath()
            defer { unlink(path) }
            let server = try TestSocketServer(path: path)
            let client = EngineClient(socketPath: path)
            let entered = DispatchSemaphore(value: 0)
            let release = DispatchSemaphore(value: 0)
            let generation = OSAllocatedUnfairLock(initialState: 0)
            client.start()
            defer { release.signal(); client.stop() }
            let first = try XCTUnwrap(server.accept(timeout: 5))
            XCTAssertTrue(waitUntil(5) { client.isReachable })
            client.setBeforeWriteHookForTesting { _, value in
                generation.withLock { $0 = value }
                entered.signal()
                _ = release.wait(timeout: .now() + 10)
            }
            let id = try XCTUnwrap(client.sendLine(["type": kind, "id": "example"]))
            XCTAssertEqual(entered.wait(timeout: .now() + 5), .success)
            XCTAssertTrue(client.isReachable, "the old connection must still appear live at submit")
            XCTAssertGreaterThan(generation.withLock { $0 }, 0)
            first.close()
            client.setBeforeWriteHookForTesting(nil)
            release.signal()
            XCTAssertTrue(waitUntil(5) { client.uncertainActionsForTesting[id] == kind })
            XCTAssertTrue(waitUntil(5) { client.failedWriteCompletionsForTesting == 1 }, "exercise the failed completion path")
            XCTAssertEqual(client.uncertainActionsForTesting, [id: kind])
            XCTAssertFalse(client.hasUnsentActions, "an attempted write cannot safely be replayed")
            let second = try XCTUnwrap(server.accept(timeout: 10))
            defer { second.close() }
            XCTAssertNil(second.readLine(timeout: 0.2))
        }
    }

    func testDisconnectedEditRetainsItsExactPatch() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let client = EngineClient(socketPath: path)
        XCTAssertNil(client.sendUpdateWorkItem(id: "task", patch: ["description": "unsaved text"]))
        client.start()
        defer { client.stop() }
        let peer = try XCTUnwrap(server.accept(timeout: 5))
        defer { peer.close() }
        XCTAssertTrue(waitUntil(5) { client.isReachable })
        XCTAssertTrue(client.retryUnsentActions())
        let envelope = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(peer.readLine(timeout: 5))) as? [String: Any])
        let payload = try XCTUnwrap(envelope["payload"] as? [String: Any])
        XCTAssertEqual(payload["type"] as? String, "update_work_item")
        XCTAssertEqual((payload["patch"] as? [String: String])?["description"], "unsaved text")
        XCTAssertNil(peer.readLine(timeout: 0.2))
    }

    func testActionDuringDisconnectIsRetriedExactlyOnce() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let connected = OSAllocatedUnfairLock(initialState: 0)
        let disconnected = OSAllocatedUnfairLock(initialState: false)
        let client = EngineClient(socketPath: path)
        client.onEvent = { event in
            if case .connected = event { connected.withLock { $0 += 1 } }
            if case .disconnected = event {
                XCTAssertNil(client.sendLine(["type": "create_chore", "name": "retained action"]))
                XCTAssertFalse(client.retryUnsentActions(), "retry during the gap keeps the action")
                disconnected.withLock { $0 = true }
            }
        }
        client.start()
        defer { client.stop() }
        let first = try XCTUnwrap(server.accept(timeout: 5))
        XCTAssertTrue(waitUntil(5) { connected.withLock { $0 == 1 } })
        first.close()
        XCTAssertTrue(waitUntil(5) { disconnected.withLock { $0 } })
        let second = try XCTUnwrap(server.accept(timeout: 10))
        defer { second.close() }
        XCTAssertTrue(waitUntil(5) { connected.withLock { $0 == 2 } })
        XCTAssertNil(second.readLine(timeout: 0.1), "mutations are not automatically replayed")
        XCTAssertTrue(client.retryUnsentActions())
        XCTAssertTrue(client.retryUnsentActions(), "a second click cannot resend the drained action")
        let data = try XCTUnwrap(second.readLine(timeout: 5))
        let envelope = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let payload = try XCTUnwrap(envelope["payload"] as? [String: Any])
        XCTAssertEqual(payload["type"] as? String, "create_chore")
        XCTAssertEqual(payload["name"] as? String, "retained action")
        XCTAssertNil(second.readLine(timeout: 0.2), "exactly one mutation reached the peer")
    }

    func testOnlyUserMutationsAreRetainedWhileDisconnected() {
        let client = EngineClient(socketPath: "/nonexistent/boss-retention.sock")
        // Replies to a dead session and reads must never enter the retry batch.
        XCTAssertNil(client.sendLine([
            "type": "engine_response", "request_id": "r1", "response": ["kind": "pane_state"],
        ]))
        XCTAssertNil(client.sendLine(["type": "get_work_tree", "product_id": "p"]))
        XCTAssertNil(client.sendLine(["type": "list_products"]))
        XCTAssertFalse(client.hasUnsentActions)
        XCTAssertEqual(client.unsentActionCountForTesting, 0)

        XCTAssertNil(client.sendLine(["type": "create_chore", "name": "kept"]))
        XCTAssertEqual(client.unsentActionCountForTesting, 1)
        for i in 0..<(EngineClient.maxUnsentActions + 10) {
            client.sendLine(["type": "create_chore", "name": "bulk-\(i)"])
        }
        XCTAssertEqual(client.unsentActionCountForTesting, EngineClient.maxUnsentActions, "buffer is capped")
        client.discardUnsentActions()
        XCTAssertFalse(client.hasUnsentActions)
    }

    func testRetryBatchContainsOnlyMutationsAfterNoiseWhileDisconnected() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let client = EngineClient(socketPath: path)
        client.sendLine(["type": "engine_response", "request_id": "r1", "response": ["kind": "k"]])
        client.sendLine(["type": "get_work_tree", "product_id": "p"])
        client.sendLine(["type": "create_chore", "name": "real"])
        client.start()
        defer { client.stop() }
        let conn = try XCTUnwrap(server.accept(timeout: 5))
        defer { conn.close() }
        XCTAssertTrue(waitUntil(5) { client.isReachable })
        XCTAssertTrue(client.retryUnsentActions())
        let data = try XCTUnwrap(conn.readLine(timeout: 5))
        let envelope = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let payload = try XCTUnwrap(envelope["payload"] as? [String: Any])
        XCTAssertEqual(payload["type"] as? String, "create_chore")
        XCTAssertNil(conn.readLine(timeout: 0.3), "nothing but the one mutation was replayed")
    }

    /// First-attempt send whose connection then dies without a reply: the
    /// outcome is surfaced as uncertain, not lost silently, and not replayed.
    func testUnansweredFirstAttemptIsReportedUncertainAndNotReplayed() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let seen = OSAllocatedUnfairLock(initialState: [Seen]())
        let client = makeClient(
            path: path, seen: seen, blocking: "never", started: DispatchSemaphore(value: 0),
            release: DispatchSemaphore(value: 0))
        client.start()
        defer { client.stop() }
        let first = try XCTUnwrap(server.accept(timeout: 5))
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0 == [.connected] } })
        XCTAssertNotNil(client.sendLine(["type": "create_chore", "name": "in-flight"]))
        XCTAssertNotNil(first.readLine(timeout: 5))
        XCTAssertEqual(client.awaitingReplyCountForTesting, 1)
        first.close()  // no reply, connection lost
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0.contains(.retryUncertain) } })
        XCTAssertEqual(client.awaitingReplyCountForTesting, 0)
        XCTAssertFalse(client.hasUnsentActions, "a written action is never auto-retained for replay")
        let second = try XCTUnwrap(server.accept(timeout: 10))
        defer { second.close() }
        XCTAssertNil(second.readLine(timeout: 0.3), "an uncertain action is not replayed")
    }

    /// A kind answered with a reply echoing `request_id` clears the in-flight
    /// record and confirms a retry; a retried action whose connection dies
    /// before any reply reports uncertainty.
    func testRetriedActionWithoutReplyReportsUncertainThenReplyConfirms() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let seen = OSAllocatedUnfairLock(initialState: [Seen]())
        let client = makeClient(
            path: path, seen: seen, blocking: "never", started: DispatchSemaphore(value: 0),
            release: DispatchSemaphore(value: 0))
        client.sendLine(["type": "create_chore", "name": "a"])
        client.start()
        defer { client.stop() }
        let first = try XCTUnwrap(server.accept(timeout: 5))
        XCTAssertTrue(waitUntil(5) { client.isReachable })
        XCTAssertTrue(client.retryUnsentActions())
        XCTAssertNotNil(first.readLine(timeout: 5))
        first.close()
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0.contains(.retryUncertain) } })

        // Same flow on a fresh connection, this time answered.
        client.sendLine(["type": "create_chore", "name": "b"])
        let second = try XCTUnwrap(server.accept(timeout: 10))
        defer { second.close() }
        XCTAssertTrue(waitUntil(5) { client.isReachable })
        XCTAssertTrue(client.retryUnsentActions())
        let data = try XCTUnwrap(second.readLine(timeout: 5))
        let envelope = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        var reply = try JSONSerialization.data(withJSONObject: [
            "request_id": try XCTUnwrap(envelope["request_id"] as? String),
            "payload": ["type": "error", "message": "ok"],
        ])
        reply.append(0x0A)
        second.sendBytes(reply)
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0.contains(.retryConfirmed) } })
        XCTAssertEqual(client.awaitingReplyCountForTesting, 0)
    }

    private enum Seen: Equatable {
        case reply(String)
        case connected
        case disconnected
        case retryConfirmed
        case retryUncertain
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
            case .retriedActionsStatus(let message):
                seen.withLock { $0.append(message == nil ? .retryConfirmed : .retryUncertain) }
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
        first.sendBytes(try line("old-reply"))
        XCTAssertEqual(started.wait(timeout: .now() + 5), .success)
        first.close()  // EOF while the old decode is blocked

        let second = try XCTUnwrap(server.accept(timeout: 10), "client reconnected")
        second.sendBytes(try line("new-reply"))
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

    func testDelayedDisconnectDoesNotInvalidateRetryOnReplacementConnection() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let seen = OSAllocatedUnfairLock(initialState: [Seen]())
        let started = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let client = makeClient(
            path: path, seen: seen, blocking: "old-reply", started: started, release: release)
        XCTAssertNil(client.sendLine(["type": "create_product", "name": "pending"]))
        client.start()
        defer { client.stop() }
        defer { release.signal() }
        let first = try XCTUnwrap(server.accept(timeout: 5))
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0 == [.connected] } })
        first.sendBytes(try line("old-reply"))
        XCTAssertEqual(started.wait(timeout: .now() + 5), .success)
        first.close()
        let second = try XCTUnwrap(server.accept(timeout: 10))
        defer { second.close() }
        XCTAssertTrue(waitUntil(5) { client.isReachable })
        XCTAssertTrue(client.retryUnsentActions())
        let data = try XCTUnwrap(second.readLine(timeout: 5))
        let envelope = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let id = try XCTUnwrap(envelope["request_id"] as? String)
        var response = try JSONSerialization.data(withJSONObject: [
            "request_id": id, "payload": ["type": "error", "message": "new-reply"],
        ])
        response.append(0x0A)
        second.sendBytes(response)
        release.signal()
        let expected: [Seen] = [
            .connected, .reply("old-reply"), .disconnected, .connected, .retryConfirmed, .reply("new-reply"),
        ]
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0.count >= expected.count } })
        XCTAssertEqual(seen.withLock { $0 }, expected)
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
        first.sendBytes(Data("invalid-frame\n".utf8) + (try line("rejected-reply")))
        XCTAssertEqual(started.wait(timeout: .now() + 5), .success)
        first.close()
        let second = try XCTUnwrap(server.accept(timeout: 10))
        defer { second.close() }
        second.sendBytes(try line("new-reply"))
        release.signal()

        let expected: [Seen] = [.connected, .disconnected, .connected, .reply("new-reply")]
        XCTAssertTrue(waitUntil(5) { seen.withLock { $0.count >= expected.count } })
        XCTAssertEqual(seen.withLock { $0 }, expected)
        second.sendBytes(try line("still-connected"))
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
        conn.sendBytes(try line("old-reply"))
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
        func readLine(timeout: TimeInterval) -> Data? {
            let deadline = Date().addingTimeInterval(timeout)
            var data = Data()
            while Date() < deadline {
                var pfd = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
                guard poll(&pfd, 1, Int32(max(1, deadline.timeIntervalSinceNow * 1000))) > 0 else { return nil }
                var byte: UInt8 = 0
                guard Darwin.read(fd, &byte, 1) == 1 else { return nil }
                if byte == 0x0A { return data }
                data.append(byte)
            }
            return nil
        }
        func sendBytes(_ data: Data) {
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
