import XCTest
@testable import Boss

/// Covers `ChatViewModel.archiveProject(id:)` — the model method
/// wired to the right-click "Archive" context menu on navigator
/// project rows — and the reply/error paths that settle it.
///
/// The acceptance criteria ask that:
///   - calling `archiveProject(id:)` sends `update_work_item` with
///     `status = "archived"` for the named project id.
///   - the outbound payload carries no extra keys beyond `type`,
///     `id`, and `patch`.
///   - the gesture gives immediate feedback (`archivingProjectIDs`)
///     without showing the project as archived before the engine
///     confirms, and that feedback always has an exit: the reply, a
///     `work_error`, or a disconnect.
///   - the engine's project reply is applied in place; it must not
///     trigger a second, un-debounced full work-tree fetch on top of the
///     one the engine's own invalidation push schedules.
@MainActor
final class ProjectArchiveContextMenuTests: XCTestCase {

    /// Invoking `archiveProject` fires an `update_work_item` patch
    /// with `status = "archived"` on the wire.
    func testArchiveProjectSendsArchivedStatusPatch() {
        let model = makeModel()
        let recorder = installRecorder(on: model)

        model.archiveProject(id: "proj_abc")

        XCTAssertEqual(recorder.value.count, 1, "expected exactly one outbound message")
        let payload = recorder.value[0]
        XCTAssertEqual(payload["type"] as? String, "update_work_item")
        XCTAssertEqual(payload["id"] as? String, "proj_abc")
        let patch = payload["patch"] as? [String: Any]
        XCTAssertEqual(patch?["status"] as? String, "archived")
    }

    /// Archiving a different project sends the correct id.
    func testArchiveProjectUsesSuppliedID() {
        let model = makeModel()
        let recorder = installRecorder(on: model)

        model.archiveProject(id: "proj_xyz")

        XCTAssertEqual(recorder.value.first?["id"] as? String, "proj_xyz")
    }

    /// The gesture marks the project as archiving the instant it is sent
    /// — that is the row's immediate feedback — and a second click while
    /// the first is in flight does not send a second RPC.
    func testArchiveProjectMarksPendingAndDedupesWhileInFlight() {
        let model = makeConnectedModel()
        let recorder = installRecorder(on: model)

        model.archiveProject(id: "proj_abc")
        model.archiveProject(id: "proj_abc")

        XCTAssertTrue(model.archivingProjectIDs.contains("proj_abc"))
        XCTAssertEqual(recorder.value.count, 1, "a pending archive must not be re-sent")
        XCTAssertEqual(model.archiveRequestProjectIDs.values.first, "proj_abc")
    }

    /// The pending mark is feedback, not an optimistic status: until the
    /// engine replies, the project in the store is still `active`.
    func testPendingArchiveDoesNotChangeStoredStatus() {
        let model = makeConnectedModel()
        model.projectsByProductID["prod_test"] = [makeProject(status: "active")]

        model.archiveProject(id: "proj_test")

        XCTAssertEqual(model.projectsByProductID["prod_test"]?.first?.status, "active")
    }

    /// The engine's `work_item_updated` reply for the project is applied
    /// in place: the stored project flips to `archived`, the pending mark
    /// clears, and — critically — no `get_work_tree` is issued from this
    /// arm. The one debounced refetch comes from the engine's own
    /// `work_invalidated` push, not from here.
    func testProjectReplyAppliesInPlaceWithoutFullRefetch() {
        let model = makeConnectedModel()
        model.projectsByProductID["prod_test"] = [makeProject(status: "active")]
        model.archiveProject(id: "proj_test")
        let recorder = installRecorder(on: model)

        model.applyEventForTest(.workItemUpdated(item: .project(makeProject(status: "archived"))))

        XCTAssertEqual(model.projectsByProductID["prod_test"]?.count, 1)
        XCTAssertEqual(model.projectsByProductID["prod_test"]?.first?.status, "archived")
        XCTAssertFalse(model.archivingProjectIDs.contains("proj_test"))
        XCTAssertTrue(model.archiveRequestProjectIDs.isEmpty)
        let fetches = recorder.value.filter { ($0["type"] as? String) == "get_work_tree" }
        XCTAssertTrue(fetches.isEmpty, "project reply must not trigger an un-debounced full work-tree fetch")
    }

    /// A project reply for a project the store has not seen yet is
    /// inserted rather than dropped (another session created it).
    func testProjectReplyInsertsUnknownProject() {
        let model = makeConnectedModel()
        model.projectsByProductID["prod_test"] = []

        model.applyEventForTest(.workItemUpdated(item: .project(makeProject(status: "active"))))

        XCTAssertEqual(model.projectsByProductID["prod_test"]?.map(\.id), ["proj_test"])
    }

    /// A `work_error` carrying the archive's own request id is the error
    /// path: the pending mark clears, the message surfaces, and the
    /// project is still `active` in the store — nothing was shown as
    /// archived that has to be undone.
    func testWorkErrorForArchiveRequestClearsPendingAndSurfacesMessage() {
        let model = makeConnectedModel()
        model.projectsByProductID["prod_test"] = [makeProject(status: "active")]
        model.archiveProject(id: "proj_test")
        let requestID = model.archiveRequestProjectIDs.keys.first
        XCTAssertNotNil(requestID)

        model.applyEventForTest(.workError(message: "cannot archive: engine refused", requestId: requestID))

        XCTAssertFalse(model.archivingProjectIDs.contains("proj_test"))
        XCTAssertTrue(model.archiveRequestProjectIDs.isEmpty)
        XCTAssertEqual(model.workErrorMessage, "cannot archive: engine refused")
        XCTAssertEqual(model.projectsByProductID["prod_test"]?.first?.status, "active")
    }

    /// A refused archive settles onto the archive alone: a kanban move and a
    /// merge-when-ready in flight are neither bounced nor given the message.
    func testWorkErrorForArchiveDoesNotFanOutToOtherInFlightState() {
        let model = makeConnectedModel()
        model.archiveProject(id: "proj_test")
        let requestID = model.archiveRequestProjectIDs.keys.first
        model.mergingWhenReadyIDs = ["task_merge"]

        model.applyEventForTest(.workError(message: "archive refused", requestId: requestID))

        XCTAssertEqual(model.workErrorMessage, "archive refused")
        XCTAssertEqual(model.mergingWhenReadyIDs, ["task_merge"])
        XCTAssertNil(model.mergeErrorNoticesByTaskID["task_merge"])
        XCTAssertFalse(model.archivingProjectIDs.contains("proj_test"))
    }

    /// An undecodable `work_item_updated` reply is the archive's only reply;
    /// it must clear the pending mark rather than strand the row.
    func testUndecodableArchiveReplyClearsPendingAndSurfacesMessage() {
        let model = makeConnectedModel()
        model.archiveProject(id: "proj_test")
        let requestID = model.archiveRequestProjectIDs.keys.first

        model.applyEventForTest(.workItemUpdateDecodeFailed(message: "bad payload", requestId: requestID))

        XCTAssertTrue(model.archivingProjectIDs.isEmpty)
        XCTAssertTrue(model.archiveRequestProjectIDs.isEmpty)
        XCTAssertEqual(model.workErrorMessage, "bad payload")
    }

    /// A `work_error` for some *other* request leaves the archive pending:
    /// its own reply is still coming.
    func testWorkErrorForUnrelatedRequestLeavesArchivePending() {
        let model = makeConnectedModel()
        model.archiveProject(id: "proj_test")

        model.applyEventForTest(.workError(message: "unrelated", requestId: "some-other-request"))

        XCTAssertTrue(model.archivingProjectIDs.contains("proj_test"))
    }

    /// Losing the socket mid-request means no reply can ever arrive;
    /// the pending mark must not survive the disconnect.
    func testDisconnectClearsPendingArchive() {
        let model = makeConnectedModel()
        model.archiveProject(id: "proj_test")

        model.applyEventForTest(.disconnected)

        XCTAssertTrue(model.archivingProjectIDs.isEmpty)
        XCTAssertTrue(model.archiveRequestProjectIDs.isEmpty)
    }

    // MARK: - Helpers

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-archive-test-\(UUID().uuidString).sock")
    }

    /// A model whose client reports every send as delivered (and hands
    /// back an envelope request id), so the in-flight bookkeeping is
    /// exercised instead of the not-connected short-circuit.
    private func makeConnectedModel() -> ChatViewModel {
        let model = makeModel()
        model.engine.simulateConnectedForTesting = true
        return model
    }

    private func makeProject(status: String) -> WorkProject {
        WorkProject(
            id: "proj_test",
            productID: "prod_test",
            name: "Make tmux the only pane hosting mode",
            slug: "make-tmux-the-only-pane-hosting-mode",
            description: "",
            goal: "",
            status: status,
            priority: "medium",
            createdAt: "2026-10-01T00:00:00Z",
            updatedAt: "2026-10-11T00:00:00Z"
        )
    }

    private final class PayloadRecorder {
        var value: [[String: Any]] = []
    }

    private func installRecorder(on model: ChatViewModel) -> PayloadRecorder {
        let recorder = PayloadRecorder()
        model.outboundRecorder = { payload in recorder.value.append(payload) }
        return recorder
    }
}
