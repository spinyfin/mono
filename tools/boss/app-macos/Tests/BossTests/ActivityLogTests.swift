import XCTest
@testable import Boss

final class ActivityLogDecoderTests: XCTestCase {
    private func makeClient() -> EngineClient {
        EngineClient(socketPath: "/tmp/boss-activity-test-\(UUID().uuidString).sock")
    }

    func testDecodesOkEvent() {
        let line = #"{"ts_epoch_ms":1778539061840,"stage":"request_recorded","outcome":"ok","execution_id":"exec_18ae9d258b5872e8_48","work_item_id":"task_18ae9d2104410c10_3d","worker_id":null,"cube_repo_id":null,"cube_lease_id":null,"cube_workspace_id":null,"details":{"preferred_workspace_id":null}}"#
        let event = DispatchEventDecoder.decode(line: line)
        XCTAssertNotNil(event)
        XCTAssertEqual(event?.stage, "request_recorded")
        XCTAssertEqual(event?.outcome, "ok")
        XCTAssertEqual(event?.executionId, "exec_18ae9d258b5872e8_48")
        XCTAssertEqual(event?.workItemId, "task_18ae9d2104410c10_3d")
        XCTAssertNil(event?.workerId)
        XCTAssertNil(event?.errorMessage)
    }

    func testDecodesErrorEvent() {
        let line = #"{"ts_epoch_ms":1778539065203,"stage":"run_started","outcome":"error","execution_id":"exec_x","work_item_id":"task_y","worker_id":"worker-3","cube_repo_id":"mono","cube_lease_id":"abc","cube_workspace_id":"mono-agent-001","error_message":"execution is not ready","details":null}"#
        let event = DispatchEventDecoder.decode(line: line)
        XCTAssertEqual(event?.outcome, "error")
        XCTAssertEqual(event?.workerId, "worker-3")
        XCTAssertEqual(event?.errorMessage, "execution is not ready")
        XCTAssertNil(event?.detailsJSON)
    }

    func testReturnsNilForBlankLine() {
        XCTAssertNil(DispatchEventDecoder.decode(line: ""))
        XCTAssertNil(DispatchEventDecoder.decode(line: "   "))
    }

    func testReturnsNilForMalformedJSON() {
        XCTAssertNil(DispatchEventDecoder.decode(line: "{not json"))
    }

    func testReturnsNilForMissingRequiredField() {
        XCTAssertNil(DispatchEventDecoder.decode(line: #"{"stage":"x","outcome":"ok"}"#))
    }

    func testShortIdReturnsTrailingSegment() {
        XCTAssertEqual(DispatchEvent.shortId("exec_18ae9d258b5872e8_48"), "48")
        XCTAssertEqual(DispatchEvent.shortId("task_abcdef_12"), "12")
        XCTAssertEqual(DispatchEvent.shortId("noprefix"), "noprefix")
    }

    func testParsesUnifiedAttemptAndBackgroundWork() {
        let client = makeClient()
        let attempt = client.parseEngineAttemptListEntry([
            "id": "cir_123",
            "product_id": "prod_123",
            "created_at": "2026-08-19T12:00:00Z",
            "extra": ["attempt_kind": "fix"],
            "kind": "ci",
            "pr_url": "https://github.com/example/repo/pull/42",
            "status": "running",
            "work_item_id": "task_123",
        ])
        let backgroundWork = client.parseBackgroundWorkItem([
            "id": "conflict_remediation:crz_123",
            "kind": "conflict_remediation",
            "phase": "Resolving conflicts",
            "product_id": "prod_123",
            "source_id": "crz_123",
            "title": "Resolve merge conflict",
            "work_item_id": "task_123",
        ])

        XCTAssertEqual(attempt?.kindLabel, "CI fix")
        XCTAssertEqual(attempt?.workItemID, "task_123")
        XCTAssertEqual(backgroundWork?.kind, .conflictRemediation)
        XCTAssertEqual(backgroundWork?.sourceID, "crz_123")
    }

    func testParsesConflictMechanicalRung() {
        let resolution = makeClient().parseConflictResolution([
            "id": "crz_123",
            "product_id": "prod_123",
            "work_item_id": "task_123",
            "pr_url": "https://github.com/example/repo/pull/42",
            "pr_number": NSNumber(value: 42),
            "head_branch": "feature/attempt",
            "base_branch": "main",
            "status": "running",
            "created_at": "2026-08-19T12:00:00Z",
            "mechanical_rung_in_flight": NSNumber(value: 1),
        ])

        XCTAssertEqual(resolution?.mechanicalRungInFlight, 1)
    }
}

final class ActivityOutcomeMappingTests: XCTestCase {
    private func dispatchRow(outcome: String) -> ActivityRow {
        let event = DispatchEvent(
            tsEpochMs: 0,
            stage: "run_started",
            outcome: outcome,
            executionId: "exec_aaa_1",
            workItemId: nil,
            workerId: nil,
            cubeRepoId: nil,
            cubeLeaseId: nil,
            cubeWorkspaceId: nil,
            errorMessage: nil,
            cubeCommand: nil,
            cubeCwd: nil,
            detailsJSON: nil
        )
        return ActivityRow(id: "d:\(event.id)", timestamp: event.timestamp, payload: .dispatch(event))
    }

    func testDispatchOkMapsToSuccess() {
        XCTAssertEqual(dispatchRow(outcome: "ok").outcome, .success)
    }

    func testDispatchErrorMapsToError() {
        XCTAssertEqual(dispatchRow(outcome: "error").outcome, .error)
    }

    func testDispatchSkippedMapsToSkipped() {
        XCTAssertEqual(dispatchRow(outcome: "skipped").outcome, .skipped)
    }
}

@MainActor
final class ActivityLogModelTests: XCTestCase {
    private func makeModel(events: [DispatchEvent]) -> ActivityLogModel {
        let model = ActivityLogModel()
        model.dispatchEvents = events
        return model
    }

    private func event(
        stage: String,
        outcome: String = "ok",
        executionId: String = "exec_aaa_1",
        workItem: String? = nil,
        error: String? = nil,
        ts: UInt64 = 0
    ) -> DispatchEvent {
        DispatchEvent(
            tsEpochMs: ts,
            stage: stage,
            outcome: outcome,
            executionId: executionId,
            workItemId: workItem,
            workerId: nil,
            cubeRepoId: nil,
            cubeLeaseId: nil,
            cubeWorkspaceId: nil,
            errorMessage: error,
            cubeCommand: nil,
            cubeCwd: nil,
            detailsJSON: nil
        )
    }

    private func attempt(
        id: String = "crz_1",
        kind: String = "conflict",
        createdAt: String = "2026-08-19T12:00:00Z"
    ) -> EngineAttemptListEntry {
        EngineAttemptListEntry(
            id: id,
            productID: "prod_1",
            createdAt: createdAt,
            extra: [:],
            kind: kind,
            prURL: "https://github.com/example/repo/pull/42",
            status: "running",
            failureReason: nil,
            finishedAt: nil,
            startedAt: nil,
            workItemID: "task_1"
        )
    }

    func testSourceFilterAll() {
        let model = makeModel(events: [event(stage: "run_started")])
        let rows = model.makeRows(sourceFilter: .all)
        XCTAssertEqual(rows.count, 1)
    }

    func testSourceFilterDispatch() {
        let model = makeModel(events: [event(stage: "run_started"), event(stage: "pane_spawned")])
        let rows = model.makeRows(sourceFilter: .dispatch)
        XCTAssertEqual(rows.count, 2)
        XCTAssertTrue(rows.allSatisfy { if case .dispatch = $0.payload { return true }; return false })
    }

    func testSourceFilterEngine() {
        let model = makeModel(events: [event(stage: "run_started")])
        let rows = model.makeRows(sourceFilter: .engineAttempts, attempts: [attempt()])
        XCTAssertEqual(rows.count, 1)
        XCTAssertTrue(rows.allSatisfy { if case .engineAttempt = $0.payload { return true }; return false })
    }

    func testUnifiedAttemptRowsRetainNewestFirstOrdering() {
        let model = makeModel(events: [])
        let rows = model.makeRows(
            sourceFilter: .engineAttempts,
            attempts: [
                attempt(id: "crz_old", createdAt: "2026-08-19T11:00:00Z"),
                attempt(id: "cir_new", kind: "ci", createdAt: "2026-08-19T13:00:00Z"),
            ]
        )

        XCTAssertEqual(rows.map(\.id), ["e:cir_new", "e:crz_old"])
    }

    func testRowsSortedNewestFirst() {
        let model = makeModel(events: [
            event(stage: "run_started", ts: 1000),
            event(stage: "pane_spawned", ts: 2000),
        ])
        let rows = model.makeRows(sourceFilter: .dispatch)
        XCTAssertEqual(rows.first?.timestamp, Date(timeIntervalSince1970: 2.0))
    }

    func testSearchMatchesExecutionId() {
        let e = event(stage: "run_started", executionId: "exec_abc_42")
        let model = makeModel(events: [e])
        let rows = model.makeRows(sourceFilter: .dispatch)
        let query = "exec_abc"
        let matches = rows.filter { $0.searchHaystack.lowercased().contains(query) }
        XCTAssertEqual(matches.count, 1)
    }

    func testSearchMatchesErrorMessage() {
        let e = event(stage: "run_started", outcome: "error", error: "SlotBusy")
        let model = makeModel(events: [e])
        let rows = model.makeRows(sourceFilter: .dispatch)
        let query = "slotbusy"
        let matches = rows.filter { $0.searchHaystack.lowercased().contains(query) }
        XCTAssertEqual(matches.count, 1)
    }
}
