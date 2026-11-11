import XCTest
@testable import Boss

/// A finished Agents pane keeps naming its work item. The engine owns the
/// title, short id and PR (resolved from the durable execution record); the
/// snapshot only renders them, and never falls back to the raw run id.
@MainActor
final class FinishedPaneLabelTests: XCTestCase {
    private let runId = "exec_18e696195af1bc70_2df"
    private let prURL = "https://github.com/spinyfin/mono/pull/3456"

    // MARK: - Fixtures

    /// Decoded from the exact JSON the engine sends in `execution_labels_resolved`.
    private func label(_ json: String) throws -> ExecutionWorkLabel {
        try JSONDecoder().decode(ExecutionWorkLabel.self, from: Data(json.utf8))
    }

    private func resolvedLabel(purpose: String?, title: String = "Fix fencer scraping", shortId: Int = 42) throws -> ExecutionWorkLabel {
        let purposeField = purpose.map { #","purpose":"\#($0)""# } ?? ""
        return try label("""
        {"execution_id":"\(runId)","status":"resolved","kind":"x","product_id":"prod_boss",
         "pr_url":"\(prURL)","work_item_id":"task_abc","work_item_short_id":\(shortId),
         "work_item_title":"\(title)"\(purposeField)}
        """)
    }

    private func finishedSnapshot(label: ExecutionWorkLabel?, persona: String? = "Chakotay") -> WorkerSlotSnapshot {
        WorkerSlotSnapshot.render(
            runId: runId, viewer: nil, liveState: nil,
            cachedPersona: persona, finishedLabel: label
        )
    }

    private func assertNoRawId(_ snapshot: WorkerSlotSnapshot, file: StaticString = #filePath, line: UInt = #line) {
        for text in [snapshot.headerTitle, snapshot.finishedSubtitle, snapshot.displayName] {
            XCTAssertFalse(text.contains("exec_"), "raw id in \(text)", file: file, line: line)
            XCTAssertFalse(text.contains(runId), "raw id in \(text)", file: file, line: line)
        }
        if let links = snapshot.headerWorkLinks {
            XCTAssertFalse(links.shortIdLabel.contains("exec_"), file: file, line: line)
        }
    }

    // MARK: - Implementation, revision, review, review guide

    func testFinishedImplementationRunKeepsTitleShortIdAndPR() throws {
        let snapshot = finishedSnapshot(label: try resolvedLabel(purpose: nil))
        XCTAssertTrue(snapshot.isPlaceholder)
        XCTAssertEqual(snapshot.headerTitle, "Chakotay finished")
        XCTAssertEqual(snapshot.finishedSubtitle, "Fix fencer scraping")
        let links = try XCTUnwrap(snapshot.headerWorkLinks)
        XCTAssertEqual(links.shortIdLabel, "T42")
        XCTAssertEqual(links.prLabel, "mono#3456")
        XCTAssertEqual(links.prURL, prURL)
        XCTAssertEqual(links.productId, "prod_boss")
        XCTAssertEqual(links.workItemId, "task_abc")
        assertNoRawId(snapshot)
    }

    func testFinishedRevisionRunSaysRevisionAndKeepsPR() throws {
        let snapshot = finishedSnapshot(label: try resolvedLabel(purpose: "Revision", title: "Address review findings"))
        XCTAssertEqual(snapshot.finishedSubtitle, "Revision: Address review findings")
        XCTAssertEqual(snapshot.headerWorkLinks?.prLabel, "mono#3456")
        XCTAssertEqual(snapshot.headerWorkLinks?.shortIdLabel, "T42")
        assertNoRawId(snapshot)
    }

    func testFinishedReviewRunSaysReviewAndKeepsPR() throws {
        let snapshot = finishedSnapshot(label: try resolvedLabel(purpose: "Review", title: "Reviewed change"))
        XCTAssertEqual(snapshot.finishedSubtitle, "Review: Reviewed change")
        XCTAssertEqual(snapshot.headerWorkLinks?.prLabel, "mono#3456")
        assertNoRawId(snapshot)
    }

    func testFinishedReviewGuideRunSaysGuideAndKeepsPR() throws {
        let snapshot = finishedSnapshot(label: try resolvedLabel(purpose: "Review guide", title: "Guided change"))
        XCTAssertEqual(snapshot.finishedSubtitle, "Review guide: Guided change")
        XCTAssertEqual(snapshot.headerWorkLinks?.prLabel, "mono#3456")
        assertNoRawId(snapshot)
    }

    func testProjectWorkItemUsesProjectShortIdPrefix() throws {
        let project = try label("""
        {"execution_id":"\(runId)","status":"resolved","work_item_id":"proj_1","work_item_short_id":7,
         "work_item_title":"Dynamic Agents","purpose":"Design","product_id":"prod_boss"}
        """)
        let snapshot = finishedSnapshot(label: project)
        XCTAssertEqual(snapshot.headerWorkLinks?.shortIdLabel, "P7")
        XCTAssertNil(snapshot.headerWorkLinks?.prURL)
    }

    // MARK: - Unresolved states say so in words

    func testDeletedWorkItemSaysSoAndNeverShowsARawId() throws {
        let deleted = try label(#"{"execution_id":"\#(runId)","status":"work_item_deleted","kind":"task_implementation"}"#)
        let snapshot = finishedSnapshot(label: deleted)
        XCTAssertEqual(snapshot.finishedSubtitle, "Work item deleted")
        XCTAssertNil(snapshot.headerWorkLinks)
        XCTAssertEqual(snapshot.headerTitle, "Chakotay finished")
        assertNoRawId(snapshot)
    }

    func testRunWithoutWorkItemAndUnknownExecutionSayTheirPurposeInWords() throws {
        let answer = try label(#"{"execution_id":"\#(runId)","status":"no_work_item","purpose":"Answer"}"#)
        XCTAssertEqual(finishedSnapshot(label: answer).finishedSubtitle, "Answer run")
        let unknown = try label(#"{"execution_id":"\#(runId)","status":"execution_unknown"}"#)
        let snapshot = finishedSnapshot(label: unknown)
        XCTAssertEqual(snapshot.finishedSubtitle, "Run record unavailable")
        assertNoRawId(snapshot)
    }

    func testLabelStillInFlightIsAWordNotTheRunId() {
        let snapshot = finishedSnapshot(label: nil)
        XCTAssertEqual(snapshot.finishedSubtitle, "Loading work item…")
        XCTAssertNil(snapshot.headerWorkLinks)
        assertNoRawId(snapshot)
    }

    func testFinishedPaneWithoutPersonaDoesNotNameItselfAfterTheRunId() throws {
        let snapshot = finishedSnapshot(label: try resolvedLabel(purpose: nil), persona: nil)
        XCTAssertEqual(snapshot.headerTitle, "Worker finished")
        assertNoRawId(snapshot)
    }

    // MARK: - Live runs are untouched

    func testLiveRunIgnoresAFinishedLabel() throws {
        let client = EngineClient(socketPath: "/tmp/finished-label-\(UUID().uuidString).sock")
        let live = try XCTUnwrap(client.parseWorkerLiveState([
            "slot_id": 1, "run_id": runId, "model": "codex", "activity": "working", "name": "Chakotay",
        ]))
        let snapshot = WorkerSlotSnapshot.render(
            runId: runId, viewer: nil, liveState: live, cachedPersona: nil,
            finishedLabel: try resolvedLabel(purpose: nil)
        )
        XCTAssertFalse(snapshot.isPlaceholder)
        XCTAssertNil(snapshot.finishedLabel)
        XCTAssertNil(snapshot.headerWorkLinks, "a live pane's links come from live state, not the finished label")
    }

    func testLabelChangesInvalidateTheSlotSnapshot() throws {
        let titled = finishedSnapshot(label: try resolvedLabel(purpose: nil))
        let renamed = finishedSnapshot(label: try resolvedLabel(purpose: nil, title: "Renamed"))
        XCTAssertNotEqual(titled, renamed)
        XCTAssertNotEqual(titled, finishedSnapshot(label: nil))
        XCTAssertEqual(titled, finishedSnapshot(label: try resolvedLabel(purpose: nil)))
    }

    // MARK: - Workspace request and reply flow

    func testWorkspaceAsksOncePerReleasedRunAndStoresTheEnginesAnswer() throws {
        let workspace = WorkersWorkspaceModel()
        var requested: [[String]] = []
        workspace.onExecutionLabelsRequested = { requested.append($0) }

        workspace.requestExecutionLabels(forReleasedRuns: [runId])
        workspace.requestExecutionLabels(forReleasedRuns: [runId])
        XCTAssertEqual(requested, [[runId]], "a pending run is not asked for twice")

        workspace.applyExecutionLabels([try resolvedLabel(purpose: "Review")])
        XCTAssertEqual(workspace.executionLabelsByRunId[runId]?.workItemTitle, "Fix fencer scraping")
        workspace.requestExecutionLabels(forReleasedRuns: [runId])
        XCTAssertEqual(requested.count, 1, "a resolved run is not asked for again")
    }

    func testUnansweredRequestExpiresWithoutAVisibilityChange() {
        let workspace = WorkersWorkspaceModel()
        var requested: [[String]] = []
        workspace.onExecutionLabelsRequested = { requested.append($0) }
        let start = Date()
        workspace.requestExecutionLabels(forReleasedRuns: [runId], now: start)
        workspace.requestExecutionLabels(forReleasedRuns: [runId], now: start.addingTimeInterval(5))
        XCTAssertEqual(requested.count, 1)
        workspace.requestExecutionLabels(forReleasedRuns: [runId], now: start.addingTimeInterval(11))
        XCTAssertEqual(requested, [[runId], [runId]])
    }

    func testSessionRegistrationResendsAnUnansweredRequest() {
        let workspace = WorkersWorkspaceModel()
        let model = ChatViewModel(socketPath: "/tmp/label-reset-\(UUID().uuidString).sock")
        var requested: [[String]] = []
        workspace.onExecutionLabelsRequested = { requested.append($0) }
        model.executionLabelsInvalidatedHandler = { workspace.invalidateExecutionLabels() }
        workspace.requestExecutionLabels(forReleasedRuns: [runId])
        model.applyEventForTest(.appSessionRegistered)
        XCTAssertEqual(requested, [[runId], [runId]])
    }

    func testBoundWorkItemInvalidationRefreshesTheLabel() throws {
        let workspace = WorkersWorkspaceModel()
        let model = ChatViewModel(socketPath: "/tmp/label-update-\(UUID().uuidString).sock")
        var requested: [[String]] = []
        workspace.onExecutionLabelsRequested = { requested.append($0) }
        model.executionLabelsInvalidatedHandler = { workspace.invalidateExecutionLabels() }
        workspace.requestExecutionLabels(forReleasedRuns: [runId])
        workspace.applyExecutionLabels([try resolvedLabel(purpose: nil)])
        model.applyEventForTest(.workInvalidated(topic: "work.prod_boss", productId: "prod_boss", itemIds: ["task_abc"]))
        XCTAssertEqual(requested, [[runId], [runId]])
        workspace.applyExecutionLabels([try resolvedLabel(purpose: nil, title: "Renamed")])
        XCTAssertEqual(workspace.executionLabelsByRunId[runId]?.workItemTitle, "Renamed")
    }

    func testUnresolvedDatabaseAnswerIsRetriedAfterExpiry() throws {
        for status in ["work_item_deleted", "execution_unknown", "no_work_item"] {
            let workspace = WorkersWorkspaceModel()
            var requested: [[String]] = []
            workspace.onExecutionLabelsRequested = { requested.append($0) }
            let start = Date()
            workspace.requestExecutionLabels(forReleasedRuns: [runId], now: start)
            let unavailable = try label("{\"execution_id\":\"\(runId)\",\"status\":\"\(status)\"}")
            workspace.applyExecutionLabels([unavailable])
            workspace.requestExecutionLabels(forReleasedRuns: [runId], now: start.addingTimeInterval(5))
            XCTAssertEqual(requested.count, 1)
            workspace.requestExecutionLabels(forReleasedRuns: [runId], now: start.addingTimeInterval(11))
            XCTAssertEqual(requested.count, 2)
            workspace.applyExecutionLabels([try resolvedLabel(purpose: nil)])
            XCTAssertEqual(workspace.executionLabelsByRunId[runId]?.status, .resolved)
        }
    }

    func testWorkspaceIgnoresLabelsItNeverAskedFor() throws {
        let workspace = WorkersWorkspaceModel()
        workspace.applyExecutionLabels([try resolvedLabel(purpose: nil)])
        XCTAssertTrue(workspace.executionLabelsByRunId.isEmpty)
    }

    func testWorkspaceDropsLabelsForRunsThatLeftTheView() throws {
        let workspace = WorkersWorkspaceModel()
        workspace.requestExecutionLabels(forReleasedRuns: [runId])
        workspace.applyExecutionLabels([try resolvedLabel(purpose: nil)])
        workspace.requestExecutionLabels(forReleasedRuns: [])
        XCTAssertTrue(workspace.executionLabelsByRunId.isEmpty)
    }

    func testShowingTheViewAgainRefetchesSoRenamesAreNotStale() throws {
        let workspace = WorkersWorkspaceModel()
        var requested: [[String]] = []
        workspace.onExecutionLabelsRequested = { requested.append($0) }
        workspace.requestExecutionLabels(forReleasedRuns: [runId])
        workspace.applyExecutionLabels([try resolvedLabel(purpose: nil)])

        workspace.refreshExecutionLabels(forReleasedRuns: [runId])
        XCTAssertEqual(requested.count, 2)
        XCTAssertEqual(
            workspace.executionLabelsByRunId[runId]?.workItemTitle, "Fix fencer scraping",
            "the previous answer stays on screen while the refresh is in flight"
        )
        workspace.applyExecutionLabels([try resolvedLabel(purpose: nil, title: "Renamed")])
        XCTAssertEqual(workspace.executionLabelsByRunId[runId]?.workItemTitle, "Renamed")
    }

    func testGridRendersFinishedSnapshotFromTheWorkspaceLabels() throws {
        let workspace = WorkersWorkspaceModel()
        workspace.requestExecutionLabels(forReleasedRuns: [runId])
        workspace.applyExecutionLabels([try resolvedLabel(purpose: "Revision")])
        let grid = AgentsGridModel()
        let snapshot = grid.renderSnapshot(runId: runId, workspace: workspace, liveStates: LiveWorkerStateStore())
        XCTAssertTrue(snapshot.isPlaceholder)
        XCTAssertEqual(snapshot.finishedSubtitle, "Revision: Fix fencer scraping")
        XCTAssertEqual(snapshot.headerWorkLinks?.shortIdLabel, "T42")
    }

    // MARK: - Wire

    func testRequestAndReplyWireShape() throws {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.engine.simulateConnectedForTesting = true
        model.isConnected = true
        var sent: [[String: Any]] = []
        model.engine.outboundRecorder = { sent.append($0) }
        model.requestExecutionLabels(runIds: [runId])
        XCTAssertEqual(sent.first?["type"] as? String, "resolve_execution_labels")
        XCTAssertEqual(sent.first?["execution_ids"] as? [String], [runId])

        var received: [ExecutionWorkLabel] = []
        model.executionLabelsHandler = { received = $0 }
        model.applyEventForTest(.executionLabelsResolved(labels: [try resolvedLabel(purpose: "Review")]))
        XCTAssertEqual(received.first?.executionId, runId)
        XCTAssertEqual(received.first?.purpose, "Review")
    }
}
