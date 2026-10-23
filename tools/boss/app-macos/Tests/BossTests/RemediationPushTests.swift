import XCTest
@testable import Boss

@MainActor
final class RemediationPushTests: XCTestCase {
    private let pr = "https://example.test/pull/42"

    private func row(_ kind: String, _ status: String, id: String = "attempt") -> EngineAttemptListEntry {
        let now = ISO8601DateFormatter().string(from: Date())
        return EngineAttemptListEntry(
            id: id, productID: "prod", createdAt: now,
            extra: ["attempts_used": "2", "budget": "3"], kind: kind, prURL: pr,
            status: status, failureReason: status == "failed" ? "failed check" : nil,
            finishedAt: status == "succeeded" ? now : nil, startedAt: now, workItemID: "task"
        )
    }

    private func event(_ row: EngineAttemptListEntry) -> EngineEvent {
        switch (row.kind, row.status) {
        case ("conflict", "running"):
            return .conflictResolutionStarted(productID: "prod", workItemID: "task", attemptID: row.id, prURL: pr, attempt: row)
        case ("conflict", "succeeded"):
            return .conflictResolutionSucceeded(productID: "prod", workItemID: "task", attemptID: row.id, prURL: pr, attempt: row)
        case ("conflict", "failed"):
            return .conflictResolutionFailed(productID: "prod", workItemID: "task", attemptID: row.id, prURL: pr, failureReason: "failed check", attempt: row)
        case ("conflict", _):
            return .conflictResolutionAbandoned(productID: "prod", workItemID: "task", attemptID: row.id, prURL: pr, failureReason: "closed", attempt: row)
        case ("ci", "running"):
            return .ciRemediationStarted(productID: "prod", workItemID: "task", attemptID: row.id, prURL: pr, attemptKind: "fix", attempt: row)
        case ("ci", "succeeded"):
            return .ciRemediationSucceeded(productID: "prod", workItemID: "task", attemptID: row.id, prURL: pr, attempt: row)
        case ("ci", "failed"):
            return .ciRemediationFailed(productID: "prod", workItemID: "task", attemptID: row.id, prURL: pr, failureReason: "failed check", attempt: row)
        default:
            return .ciRemediationAbandoned(productID: "prod", workItemID: "task", attemptID: row.id, prURL: pr, failureReason: "closed", attempt: row)
        }
    }

    func testWireDecoderPreservesEveryPushedProjection() async throws {
        let model = ChatViewModel(socketPath: "/tmp/boss-remediation-\(UUID()).sock")
        let recorder = PayloadRecorder()
        model.outboundRecorder = { recorder.value.append($0) }
        for kind in ["conflict", "ci"] {
            for phase in ["started", "failed", "abandoned", "succeeded"] {
                let status = phase == "started" ? "running" : phase
                let expected = row(kind, status, id: "\(kind)-\(phase)")
                let payload: [String: Any] = [
                    "type": "\(kind == "ci" ? "ci_remediation" : "conflict_resolution")_\(phase)",
                    "product_id": "prod", "work_item_id": "task", "attempt_id": expected.id, "pr_url": pr,
                    "attempt": [
                        "id": expected.id, "product_id": "prod", "work_item_id": "task", "pr_url": pr,
                        "kind": kind, "status": status, "created_at": expected.createdAt, "extra": expected.extra
                    ]
                ]
                let bytes = try JSONSerialization.data(withJSONObject: ["payload": payload])
                model.engine.consumeLineForTesting(String(decoding: bytes, as: UTF8.self))
            }
        }
        for _ in 0..<100 {
            if model.engineAttempts.count == 8 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(model.engineAttempts.count, 8)
        XCTAssertEqual(model.ciRemediations.count, 4)
        XCTAssertEqual(model.conflictResolutions.count, 4)
        XCTAssertTrue(recorder.value.isEmpty)
    }

    func testEveryLifecyclePushUpsertsWithoutRequestsHiddenOrVisible() {
        for visible in [false, true] {
            let model = ChatViewModel(socketPath: "/tmp/boss-remediation-\(UUID()).sock")
            model.activityIsVisible = visible
            let recorder = PayloadRecorder()
            model.outboundRecorder = { recorder.value.append($0) }
            for kind in ["conflict", "ci"] {
                for status in ["running", "failed", "abandoned", "succeeded"] {
                    let attempt = row(kind, status, id: kind)
                    model.applyEventForTest(event(attempt))
                    model.applyEventForTest(event(attempt))
                    XCTAssertEqual(model.engineAttempts.filter { $0.kind == kind }, [attempt])
                    XCTAssertEqual(kind == "ci" ? model.ciRemediations : model.conflictResolutions, [attempt])
                    if status == "running", kind == "ci" {
                        XCTAssertEqual(model.ciFailureBadges[pr], CiFailureBadge(state: .inFlight, attemptsUsed: 2, budget: 3))
                    }
                }
            }
            XCTAssertNotNil(model.recentlyClearedCIPRs[pr])
            XCTAssertNotNil(model.recentlyClearedConflictPRs[pr])
            XCTAssertNil(model.ciFailureBadges[pr])
            XCTAssertTrue(recorder.value.isEmpty, "16 delivered pushes must cost zero requests")
        }
    }

    func testConnectAndResyncFetchOpenSetAndOnlyVisibleHistory() throws {
        for visible in [false, true] {
            let model = ChatViewModel(socketPath: "/tmp/boss-remediation-\(UUID()).sock")
            let recorder = PayloadRecorder()
            model.outboundRecorder = { recorder.value.append($0) }
            model.activityIsVisible = visible
            model.applyEventForTest(.connected)
            for _ in 0..<2 {
                let requests = recorder.value.filter { $0["type"] as? String == "list_engine_attempts" }
                XCTAssertEqual(requests.filter { $0["open_prs_only"] as? Bool == true }.count, 1)
                XCTAssertEqual(requests.filter { $0["limit"] as? Int == 200 }.count, visible ? 1 : 0)
                XCTAssertFalse(recorder.value.contains { ["list_ci_remediations", "list_conflict_resolutions"].contains($0["type"] as? String ?? "") })
                recorder.value.removeAll()
                model.applyEventForTest(.resyncRequired)
                model.fireResyncRefetch()
            }
            model.applyEventForTest(.disconnected)
        }
    }

    func testOpenSnapshotRecoversMissedPushesAndClearsStaleChips() throws {
        let model = ChatViewModel(socketPath: "/tmp/boss-remediation-\(UUID()).sock")
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        model.ciFailureBadges["stale"] = CiFailureBadge(state: .exhausted, attemptsUsed: 3, budget: 3)
        let requestID = try XCTUnwrap(model.remediationStateRequestID)
        model.applyEventForTest(.engineAttemptsList(
            attempts: [row("ci", "succeeded"), row("conflict", "succeeded"), row("ci_state", "clear")],
            backgroundWork: [], requestId: requestID
        ))
        XCTAssertTrue(model.ciFailureBadges.isEmpty)
        XCTAssertNotNil(model.recentlyClearedCIPRs[pr])
        XCTAssertNotNil(model.recentlyClearedConflictPRs[pr])
        XCTAssertTrue(model.engineAttempts.isEmpty, "open-set recovery must not replace Activity history")

        model.refreshRemediationState()
        let next = try XCTUnwrap(model.remediationStateRequestID)
        model.applyEventForTest(.engineAttemptsList(
            attempts: [row("ci_state", "exhausted")], backgroundWork: [], requestId: next
        ))
        XCTAssertEqual(model.ciFailureBadges[pr]?.state, .exhausted)
        XCTAssertNil(model.recentlyClearedCIPRs[pr])
        XCTAssertNil(model.recentlyClearedConflictPRs[pr])
    }

    func testProductSwitchRecoversPushesMissedWhileUnsubscribedWithoutHistory() {
        let model = ChatViewModel(socketPath: "/tmp/boss-remediation-\(UUID()).sock")
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        let recorder = PayloadRecorder()
        model.outboundRecorder = { recorder.value.append($0) }
        model.selectWorkProduct("other-product")
        let requests = recorder.value.filter { $0["type"] as? String == "list_engine_attempts" }
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?["open_prs_only"] as? Bool, true)
        XCTAssertNil(requests.first?["limit"])
        recorder.value.removeAll()
        model.refreshWorkSubscriptions()
        XCTAssertTrue(recorder.value.isEmpty, "unchanged subscriptions need no recovery read")
    }

    func testPushWinsOverInFlightSnapshotAndHistory() throws {
        let model = ChatViewModel(socketPath: "/tmp/boss-remediation-\(UUID()).sock")
        model.activityIsVisible = true
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        let stateID = try XCTUnwrap(model.remediationStateRequestID)
        let historyID = try XCTUnwrap(model.backgroundWorkPending.first { $0.value.replacesAttempts }?.key)
        let succeeded = row("ci", "succeeded")
        model.applyEventForTest(event(succeeded))
        model.applyEventForTest(.engineAttemptsList(
            attempts: [row("ci", "running"), row("ci_state", "in_flight")],
            backgroundWork: [], requestId: stateID
        ))
        model.applyEventForTest(.engineAttemptsList(
            attempts: [row("ci", "running")], backgroundWork: [], requestId: historyID
        ))
        XCTAssertEqual(model.engineAttempts, [succeeded])
        XCTAssertEqual(model.ciRemediations, [succeeded])
        XCTAssertNil(model.ciFailureBadges[pr])
        XCTAssertNotNil(model.recentlyClearedCIPRs[pr])
    }

    func testHistoryReplyRetainsBufferedPushWhenApplyingBackgroundSnapshot() throws {
        let model = ChatViewModel(socketPath: "/tmp/boss-remediation-\(UUID()).sock")
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        let stateID = try XCTUnwrap(model.remediationStateRequestID)
        model.applyEventForTest(.engineAttemptsList(attempts: [], backgroundWork: [], requestId: stateID))
        model.activityIsVisible = true
        model.refreshEngineAttempts()
        let historyID = try XCTUnwrap(model.backgroundWorkPending.first { $0.value.replacesAttempts }?.key)
        let succeeded = row("ci", "succeeded")
        model.applyEventForTest(event(succeeded))
        model.applyEventForTest(.engineAttemptsList(
            attempts: [row("ci", "running")], backgroundWork: [], requestId: historyID
        ))
        XCTAssertEqual(model.engineAttempts, [succeeded])
        XCTAssertEqual(model.ciRemediations, [succeeded])
        XCTAssertNotNil(model.recentlyClearedCIPRs[pr])
        XCTAssertNil(model.ciFailureBadges[pr])
        XCTAssertNil(model.remediationEventsDuringRead[historyID])
    }
}

private final class PayloadRecorder: @unchecked Sendable {
    var value: [[String: Any]] = []
}
