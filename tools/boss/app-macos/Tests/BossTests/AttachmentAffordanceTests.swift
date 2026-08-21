import XCTest
@testable import Boss

/// Screenshot-viewer surface: the kanban card's `hasAttachments`-gated
/// affordance, the `WorkAttachment` wire decode, the `list_attachments_for_work_item`
/// RPC wiring, and the window-open payload's identity. Mirrors the coverage
/// shape of `TranscriptViewTests` for the near-identical transcript-viewer
/// surface.
@MainActor
final class AttachmentAffordanceTests: XCTestCase {

    // MARK: - Card affordance gating

    /// The badge-strip affordance is a direct reflection of the engine-
    /// resolved `hasAttachments` flag — no extra app-side gating, matching
    /// `showsDesignDocAffordance`'s shape.
    func testShowsAttachmentsAffordanceReflectsHasAttachments() {
        var withEvidence = makeTask(id: "task_with_evidence")
        withEvidence.hasAttachments = true
        var withoutEvidence = makeTask(id: "task_without_evidence")
        withoutEvidence.hasAttachments = false

        let ctx = WorkCardSnapshotContext(column: .backlog)
        let withSnap = WorkCardSnapshot.build(task: withEvidence, context: ctx)
        let withoutSnap = WorkCardSnapshot.build(task: withoutEvidence, context: ctx)

        XCTAssertTrue(withSnap.showsAttachmentsAffordance, "a card with evidence must show the affordance")
        XCTAssertFalse(withoutSnap.showsAttachmentsAffordance, "a card with no evidence must not show the affordance")
    }

    func testAttachmentsAffordanceParticipatesInBadgeStripSliceEquality() {
        var task = makeTask(id: "task_badge")
        task.hasAttachments = false
        let ctx = WorkCardSnapshotContext(column: .backlog)
        let before = WorkBoardCardBadgeStripSlice(snapshot: WorkCardSnapshot.build(task: task, context: ctx))

        task.hasAttachments = true
        let after = WorkBoardCardBadgeStripSlice(snapshot: WorkCardSnapshot.build(task: task, context: ctx))

        XCTAssertNotEqual(before, after, "flipping hasAttachments must flip the badge-strip slice")
    }

    // MARK: - Wire decode

    func testParseAttachmentVMDecodesFullShape() {
        let client = EngineClient(socketPath: "/tmp/boss-attachment-decode-test.sock")
        let attachment = client.parseAttachmentVM([
            "id": "atc_1",
            "execution_id": "exec_1",
            "work_item_id": "task_1",
            "caption": "wide table, after the fix",
            "content_digest": "deadbeefcafe",
            "media_type": "png",
            "pixel_width": 1200,
            "pixel_height": 800,
            "size_bytes": 45_000,
            "source_name": "after.png",
            "created_at": "1700000000",
            "reclaimed_at": NSNull(),
        ])
        XCTAssertEqual(attachment?.id, "atc_1")
        XCTAssertEqual(attachment?.executionId, "exec_1")
        XCTAssertEqual(attachment?.workItemId, "task_1")
        XCTAssertEqual(attachment?.caption, "wide table, after the fix")
        XCTAssertEqual(attachment?.contentDigest, "deadbeefcafe")
        XCTAssertEqual(attachment?.mediaType, "png")
        XCTAssertEqual(attachment?.pixelWidth, 1200)
        XCTAssertEqual(attachment?.pixelHeight, 800)
        XCTAssertEqual(attachment?.sizeBytes, 45_000)
        XCTAssertEqual(attachment?.sourceName, "after.png")
        XCTAssertEqual(attachment?.createdAt, "1700000000")
        XCTAssertNil(attachment?.reclaimedAt)
        XCTAssertFalse(attachment?.isReclaimed ?? true)
    }

    /// An absent caption falls back to empty (the gallery's own fallback to
    /// `sourceName` lives on `AttachmentVM.displayTitle`, not the parser).
    func testParseAttachmentVMDefaultsMissingCaptionToEmpty() {
        let client = EngineClient(socketPath: "/tmp/boss-attachment-decode-test.sock")
        let attachment = client.parseAttachmentVM([
            "id": "atc_2",
            "execution_id": "exec_1",
            "work_item_id": "task_1",
            "content_digest": "digest",
            "media_type": "jpeg",
            "pixel_width": 10,
            "pixel_height": 10,
            "size_bytes": 100,
            "source_name": "shot.jpg",
            "created_at": "1700000000",
        ])
        XCTAssertEqual(attachment?.caption, "")
        XCTAssertEqual(attachment?.displayTitle, "shot.jpg", "empty caption must fall back to the source filename")
        XCTAssertEqual(attachment?.fileExtension, "jpg", "jpeg media type stores under a .jpg extension")
    }

    /// A reclaimed row must decode `reclaimedAt` and report `isReclaimed`.
    func testParseAttachmentVMDecodesReclaimedTombstone() {
        let client = EngineClient(socketPath: "/tmp/boss-attachment-decode-test.sock")
        let attachment = client.parseAttachmentVM([
            "id": "atc_3",
            "execution_id": "exec_1",
            "work_item_id": "task_1",
            "content_digest": "digest",
            "media_type": "png",
            "pixel_width": 10,
            "pixel_height": 10,
            "size_bytes": 100,
            "source_name": "shot.png",
            "created_at": "1700000000",
            "reclaimed_at": "1700005000",
        ])
        XCTAssertEqual(attachment?.reclaimedAt, "1700005000")
        XCTAssertTrue(attachment?.isReclaimed ?? false)
    }

    func testParseAttachmentVMRejectsMissingRequiredFields() {
        let client = EngineClient(socketPath: "/tmp/boss-attachment-decode-test.sock")
        let attachment = client.parseAttachmentVM([
            "id": "atc_4",
            "execution_id": "exec_1",
            // work_item_id missing
            "content_digest": "digest",
            "media_type": "png",
            "pixel_width": 10,
            "pixel_height": 10,
            "size_bytes": 100,
            "source_name": "shot.png",
            "created_at": "1700000000",
        ])
        XCTAssertNil(attachment)
    }

    // MARK: - RPC wiring

    /// Mirrors `TranscriptViewTests.testLoadExecutionsSendsWorkItemId`: the
    /// app-facing verb keys on `work_item_id` and always asks for the whole
    /// revision chain, matching `list_executions`.
    func testLoadAttachmentsSendsWorkItemIdAndRevisionChainFlag() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var captured: [[String: Any]] = []
        model.outboundRecorder = { captured.append($0) }

        model.loadAttachments(taskId: "task_xyz")

        let payload = captured.first { ($0["type"] as? String) == "list_attachments_for_work_item" }
        XCTAssertNotNil(payload, "expected a list_attachments_for_work_item payload on the wire")
        XCTAssertEqual(payload?["work_item_id"] as? String, "task_xyz")
        XCTAssertEqual(payload?["include_revision_chain"] as? Bool, true)
    }

    /// `loadAttachments` clears any cached rows first so the viewer shows a
    /// loading state rather than stale evidence from a previous task.
    func testLoadAttachmentsClearsCachedRowsBeforeFetching() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.outboundRecorder = { _ in }
        model.attachmentsByTaskID["task_xyz"] = [
            AttachmentVM(
                id: "atc_stale",
                executionId: "exec_1",
                workItemId: "task_xyz",
                caption: "stale",
                contentDigest: "digest",
                createdAt: "1700000000",
                mediaType: "png",
                pixelWidth: 10,
                pixelHeight: 10,
                sizeBytes: 100,
                sourceName: "shot.png",
                reclaimedAt: nil
            )
        ]

        model.loadAttachments(taskId: "task_xyz")

        XCTAssertNil(model.attachmentsByTaskID["task_xyz"], "must clear to nil (loading), not an empty array")
    }

    /// An `attachments_list` reply populates `attachmentsByTaskID` under the
    /// requested task id.
    func testAttachmentsListEventUpdatesModelState() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let attachment = AttachmentVM(
            id: "atc_1",
            executionId: "exec_1",
            workItemId: "task_xyz",
            caption: "after the fix",
            contentDigest: "digest",
            createdAt: "1700000000",
            mediaType: "png",
            pixelWidth: 10,
            pixelHeight: 10,
            sizeBytes: 100,
            sourceName: "shot.png",
            reclaimedAt: nil
        )
        model.applyEventForTest(.attachmentsList(taskId: "task_xyz", attachments: [attachment]))
        XCTAssertEqual(model.attachmentsByTaskID["task_xyz"], [attachment])
    }

    // MARK: - Window-open payload identity

    /// `AttachmentViewerRef` must key on `taskId` alone (mirroring
    /// `TranscriptViewerRef`) so re-invoking the affordance for the same
    /// task focuses the existing window.
    func testAttachmentViewerRefEqualityIsTaskIdOnly() {
        let a = AttachmentViewerRef(taskId: "task_1")
        let b = AttachmentViewerRef(taskId: "task_1")
        let c = AttachmentViewerRef(taskId: "task_2")
        XCTAssertEqual(a, b)
        XCTAssertNotEqual(a, c)
        XCTAssertEqual(a.hashValue, b.hashValue)
    }

    // MARK: - Blob path resolution

    /// Mirrors the engine's `AttachmentStore::blob_path`:
    /// `<state_root>/attachments/<first-2-hex>/<digest>.<ext>`.
    func testBlobURLShardsByFirstTwoDigestCharsAndUsesStoredExtension() {
        let png = AttachmentVM(
            id: "atc_1",
            executionId: "exec_1",
            workItemId: "task_1",
            caption: "",
            contentDigest: "abcd1234",
            createdAt: "1700000000",
            mediaType: "png",
            pixelWidth: 10,
            pixelHeight: 10,
            sizeBytes: 100,
            sourceName: "shot.png",
            reclaimedAt: nil
        )
        let url = AttachmentBlobPaths.blobURL(for: png)
        XCTAssertEqual(url.lastPathComponent, "abcd1234.png")
        XCTAssertEqual(url.deletingLastPathComponent().lastPathComponent, "ab")
        XCTAssertEqual(
            url.deletingLastPathComponent().deletingLastPathComponent().lastPathComponent,
            "attachments"
        )

        let jpeg = AttachmentVM(
            id: "atc_2",
            executionId: "exec_1",
            workItemId: "task_1",
            caption: "",
            contentDigest: "ffee5678",
            createdAt: "1700000000",
            mediaType: "jpeg",
            pixelWidth: 10,
            pixelHeight: 10,
            sizeBytes: 100,
            sourceName: "shot.jpg",
            reclaimedAt: nil
        )
        XCTAssertEqual(AttachmentBlobPaths.blobURL(for: jpeg).lastPathComponent, "ffee5678.jpg")
    }

    // MARK: - Revision-chain grouping (RevisionChainGrouping.swift)
    //
    // Exercised through `AttachmentVM`'s `RevisionChainItem` conformance —
    // `TranscriptViewerView.executionGroups` and
    // `AttachmentViewerView.attachmentGroups` both call the same generic
    // `revisionChainGroups`, so pinning behavior through one conforming
    // type covers both call sites.

    /// A list entirely from the chain root collapses to a single group
    /// with an empty label — callers render this as an unlabelled section.
    func testRevisionChainGroupsRootOnlyYieldsSingleUnlabelledGroup() {
        let items = [
            makeAttachment(id: "atc_1", workItemId: "task_root"),
            makeAttachment(id: "atc_2", workItemId: "task_root"),
        ]

        let groups = revisionChainGroups(items, rootTaskId: "task_root", revisions: [])

        XCTAssertEqual(groups.count, 1)
        XCTAssertEqual(groups[0].label, "")
        XCTAssertEqual(groups[0].items.map(\.id), ["atc_1", "atc_2"])
    }

    /// Root plus revisions labels "Original" / "R<seq>" in sequence order.
    func testRevisionChainGroupsLabelsRootAndRevisionsInSequenceOrder() {
        var r1 = makeTask(id: "task_r1")
        r1.revisionSeq = 1
        var r2 = makeTask(id: "task_r2")
        r2.revisionSeq = 2

        let items = [
            makeAttachment(id: "atc_root", workItemId: "task_root"),
            makeAttachment(id: "atc_r1", workItemId: "task_r1"),
            makeAttachment(id: "atc_r2", workItemId: "task_r2"),
        ]

        let groups = revisionChainGroups(items, rootTaskId: "task_root", revisions: [r1, r2])

        XCTAssertEqual(groups.map(\.label), ["Original", "R1", "R2"])
        XCTAssertEqual(groups.map { $0.items.map(\.id) }, [["atc_root"], ["atc_r1"], ["atc_r2"]])
    }

    /// An item whose `workItemId` matches no known revision falls back to
    /// "Revision" and is appended last, rather than dropped.
    func testRevisionChainGroupsFallsBackToRevisionLabelForUnknownTaskAndAppendsLast() {
        var r1 = makeTask(id: "task_r1")
        r1.revisionSeq = 1

        let items = [
            makeAttachment(id: "atc_unknown", workItemId: "task_mystery"),
            makeAttachment(id: "atc_root", workItemId: "task_root"),
            makeAttachment(id: "atc_r1", workItemId: "task_r1"),
        ]

        let groups = revisionChainGroups(items, rootTaskId: "task_root", revisions: [r1])

        XCTAssertEqual(groups.map(\.label), ["Original", "R1", "Revision"])
        XCTAssertEqual(groups.last?.items.map(\.id), ["atc_unknown"])
    }

    // MARK: - Load failure state (ChatViewModel+EventHandling.swift .workError arm)

    /// A `WorkError` arriving while attachments are loading — with no
    /// other tracked app request in flight — populates the failure map so
    /// the viewer can render a Retry-able state instead of spinning
    /// forever.
    func testWorkErrorWhileAttachmentsInFlightPopulatesLoadFailure() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.outboundRecorder = { _ in }
        model.loadAttachments(taskId: "task_xyz")

        model.applyEventForTest(.workError(message: "boom"))

        XCTAssertEqual(model.attachmentsLoadFailureByTaskID["task_xyz"], "boom")
        XCTAssertTrue(model.attachmentsInFlightTaskIDs.isEmpty)
    }

    /// A subsequent successful `attachments_list` reply clears a
    /// previously recorded load failure for the same task.
    func testAttachmentsListEventClearsPriorLoadFailure() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.outboundRecorder = { _ in }
        model.loadAttachments(taskId: "task_xyz")
        model.applyEventForTest(.workError(message: "boom"))
        XCTAssertNotNil(model.attachmentsLoadFailureByTaskID["task_xyz"])

        model.applyEventForTest(.attachmentsList(taskId: "task_xyz", attachments: []))

        XCTAssertNil(model.attachmentsLoadFailureByTaskID["task_xyz"])
    }

    /// Retrying via `loadAttachments` clears a stale failure so the viewer
    /// shows the loading state again rather than the old error.
    func testLoadAttachmentsClearsPriorLoadFailureOnRetry() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.outboundRecorder = { _ in }
        model.loadAttachments(taskId: "task_xyz")
        model.applyEventForTest(.workError(message: "boom"))
        XCTAssertNotNil(model.attachmentsLoadFailureByTaskID["task_xyz"])

        model.loadAttachments(taskId: "task_xyz")

        XCTAssertNil(model.attachmentsLoadFailureByTaskID["task_xyz"])
    }

    /// A `WorkError` that arrives while some OTHER tracked app request is
    /// also in flight (e.g. a merge-when-ready call) is ambiguous — it
    /// must not be blamed on the attachments viewer, since the generic
    /// reply carries no request id and the error may belong to the other
    /// request instead, so the viewer receives a neutral retry-able failure
    /// rather than the unrelated error message.
    func testWorkErrorRecordsNeutralFailureWhenAnotherRequestIsInFlight() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.outboundRecorder = { _ in }
        model.loadAttachments(taskId: "task_xyz")
        model.mergingWhenReadyIDs.insert("task_other")

        model.applyEventForTest(.workError(message: "merge failed"))

        XCTAssertEqual(
            model.attachmentsLoadFailureByTaskID["task_xyz"],
            "Loading failed. Retry?",
            "ambiguous WorkError must not paint the unrelated failure message"
        )
        XCTAssertTrue(
            model.attachmentsInFlightTaskIDs.isEmpty,
            "neutral failure must clear the in-flight state so the viewer stops spinning"
        )
    }

    // MARK: - Fixture

    private func makeAttachment(id: String, workItemId: String) -> AttachmentVM {
        AttachmentVM(
            id: id,
            executionId: "exec_1",
            workItemId: workItemId,
            caption: "",
            contentDigest: "digest_\(id)",
            createdAt: "1700000000",
            mediaType: "png",
            pixelWidth: 10,
            pixelHeight: 10,
            sizeBytes: 100,
            sourceName: "shot.png",
            reclaimedAt: nil
        )
    }

    private func makeTask(id: String) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Test work",
            description: "",
            status: "todo",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-14T00:00:00Z",
            updatedAt: "2026-05-14T00:00:00Z"
        )
    }
}
