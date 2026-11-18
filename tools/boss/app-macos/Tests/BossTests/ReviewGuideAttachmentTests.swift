import XCTest
@testable import Boss

@MainActor
final class ReviewGuideAttachmentTests: XCTestCase {
    func testOpeningGuideFetchesWholeChainEvenWithoutRootAttachmentBadge() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var requests: [[String: Any]] = []
        model.outboundRecorder = { requests.append($0) }
        model.attachmentsByTaskID["root"] = [attachment("old", "Before")]

        model.openReviewGuide(versionId: "version", rootTaskId: "root")

        let request = requests.first { $0["type"] as? String == "list_attachments_for_work_item" }
        XCTAssertEqual(request?["work_item_id"] as? String, "root")
        XCTAssertEqual(request?["include_revision_chain"] as? Bool, true)
        XCTAssertNil(model.attachmentsByTaskID["root"])
        XCTAssertTrue(model.attachmentsInFlightTaskIDs.contains("root"))
    }

    func testSwitchingGuidesDoesNotMixLateAttachmentResponses() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.outboundRecorder = { _ in }
        model.openReviewGuide(versionId: "version-a", rootTaskId: "root-a")
        model.openReviewGuide(versionId: "version-b", rootTaskId: "root-b")
        model.applyEventForTest(.attachmentsList(taskId: "root-a", attachments: [attachment("a", "Before")]))
        XCTAssertEqual(model.asyncMarkdownViewerVM.reviewGuideRootTaskId, "root-b")
        XCTAssertNil(model.attachmentsByTaskID["root-b"])
        model.applyEventForTest(.attachmentsList(taskId: "root-b", attachments: []))
        XCTAssertEqual(model.attachmentsByTaskID["root-b"], [])
    }

    func testInterleavedComparisonsBecomeAdjacentBeforeThenAfterAcrossRevisions() {
        let input = [
            attachment("wide-after", "Wide table, AFTER the fix", workItem: "revision"),
            attachment("narrow-after", "Narrow table: after"),
            attachment("other", "Unrelated evidence"),
            attachment("wide-before", "wide table — before the fix"),
            attachment("narrow-before", "Narrow table: before"),
        ]
        XCTAssertEqual(ReviewGuideAttachmentOrdering.arrange(input).map(\.id),
                       ["wide-before", "wide-after", "narrow-before", "narrow-after", "other"])
    }

    func testLoadingAndRefreshingEvidenceDoesNotChangeExportableMarkdown() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.outboundRecorder = { _ in }
        model.openReviewGuide(versionId: "version", rootTaskId: "root")
        let source = "# Guide\n\n## Evidence\nRegistered screenshots are available in Boss."
        model.applyReviewGuideContent(versionId: "version", content: ReviewGuideVersionContent(
            id: "version", seriesId: "series", comparisonId: "comparison", attemptId: "attempt",
            markdown: source, contentHash: "hash", promptVersion: "review-guide-v1", generatedAt: "2026-10-20T12:00:00Z"
        ))
        model.applyEventForTest(.attachmentsList(taskId: "root", attachments: [attachment("a", "After")]))
        model.loadAttachments(taskId: "root")
        model.applyEventForTest(.attachmentsList(taskId: "root", attachments: [attachment("b", "Before")]))
        guard case .loaded(_, let markdown, _) = model.asyncMarkdownViewerVM.state else {
            return XCTFail("Expected the guide to stay loaded")
        }
        XCTAssertEqual(markdown, source)
    }

    func testUnpairedAmbiguousAndEmptyCaptionsKeepTheirOrderAndEveryRow() {
        let input = [
            attachment("one", "Before wide"), attachment("two", "After narrow"),
            attachment("three", "Before and after"), attachment("four", ""),
            attachment("five", "Afterward"), attachment("six", "Before before"),
        ]
        XCTAssertEqual(ReviewGuideAttachmentOrdering.arrange(input), input)
        XCTAssertEqual(ReviewGuideAttachmentOrdering.arrange([]), [])
        XCTAssertEqual(ReviewGuideAttachmentOrdering.arrange([input[0]]), [input[0]])
    }

    func testBarePhaseCaptionsAndRepeatedEvidenceAreNotDropped() {
        let input = [attachment("a", "After"), attachment("b", "BEFORE"),
                     attachment("c", "After"), attachment("d", "Before"), attachment("e", "After")]
        XCTAssertEqual(ReviewGuideAttachmentOrdering.arrange(input).map(\.id), ["b", "a", "d", "c", "e"])
    }

    private func attachment(_ id: String, _ caption: String, workItem: String = "root") -> AttachmentVM {
        AttachmentVM(id: id, executionId: "execution", workItemId: workItem, caption: caption,
                     contentDigest: id, createdAt: "1700000000", mediaType: "png",
                     pixelWidth: 10, pixelHeight: 10, sizeBytes: 100, sourceName: "shot.png", reclaimedAt: nil)
    }
}
