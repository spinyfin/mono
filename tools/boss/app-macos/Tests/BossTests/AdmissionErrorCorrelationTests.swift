import Darwin
import XCTest
@testable import Boss

@MainActor
final class AdmissionErrorCorrelationTests: XCTestCase {
    func testOnlyMatchingAdmissionFailsForEngineErrorAndMalformedReply() throws {
        for type in ["error", "dispatch_admission_evaluated"] {
            try withConnectedModel { model, peer in
                for id in ["first", "second"] {
                    let pending = ChatViewModel.PendingDragAdmissionCheck(taskID: id, column: .doing, group: nil)
                    model.pendingDragAdmissionChecks[id] = pending
                    model.pendingMoveOriginByTaskID[id] = .backlog
                    model.optimisticColumnByTaskID[id] = .doing
                    model.engine.sendEvaluateDispatchAdmission(workItemId: id, requestId: pending.requestID)
                }
                let first = try envelope(peer)
                let second = try envelope(peer)
                XCTAssertEqual(second["request_id"] as? String, model.pendingDragAdmissionChecks["second"]?.requestID)
                try reply(peer, requestID: XCTUnwrap(first["request_id"] as? String), payload: ["type": type, "message": "refused"])
                XCTAssertTrue(waitUntil { model.pendingDragAdmissionChecks["first"] == nil })
                XCTAssertNil(model.optimisticColumnByTaskID["first"])
                XCTAssertNotNil(model.pendingDragAdmissionChecks["second"])
                XCTAssertEqual(model.optimisticColumnByTaskID["second"], .doing)
            }
        }
    }

    func testInFlightCommentResolutionDropDoesNotReportUncertainty() throws {
        try withConnectedModel { model, peer in
            model.commentBridge.resolveComments(artifactKind: "work_item", artifactId: "doc", plainText: "Document", guideVersionId: nil)
            let sent = try envelope(peer)
            XCTAssertEqual((sent["payload"] as? [String: Any])?["type"] as? String, "comments_resolve")
            peer.close()
            XCTAssertTrue(waitUntil { !model.isConnected })
            XCTAssertEqual(model.engine.awaitingReplyCountForTesting, 0)
            XCTAssertEqual(model.engine.unsentActionCountForTesting, 0)
            XCTAssertTrue(model.engine.uncertainActionsForTesting.isEmpty)
            XCTAssertNil(model.uncertainActionNotice)
            XCTAssertNil(model.disconnectedActionNotice)
        }
    }

    private func withConnectedModel(_ body: (ChatViewModel, TestSocketServer.Peer) throws -> Void) throws {
        let dir = ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory()
        let path = "\(dir)/admission-\(UUID().uuidString.prefix(8)).sock"
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let model = ChatViewModel(socketPath: path)
        model.engine.onEvent = { event in
            if case .connected = event { model.isConnected = true }
            else { model.applyEventForTest(event) }
        }
        model.engine.start()
        defer { model.engine.stop() }
        let peer = try XCTUnwrap(server.accept(timeout: 5))
        defer { peer.close() }
        XCTAssertTrue(waitUntil { model.isConnected })
        try body(model, peer)
    }

    private func envelope(_ peer: TestSocketServer.Peer) throws -> [String: Any] {
        try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(peer.readLine(timeout: 5))) as? [String: Any])
    }

    private func reply(_ peer: TestSocketServer.Peer, requestID: String, payload: [String: Any]) throws {
        var data = try JSONSerialization.data(withJSONObject: ["request_id": requestID, "payload": payload])
        data.append(0x0A)
        peer.sendBytes(data)
    }

    private func waitUntil(_ predicate: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(5)
        while !predicate(), Date() < deadline {
            RunLoop.current.run(until: Date().addingTimeInterval(0.01))
        }
        return predicate()
    }
}
