import XCTest
@testable import Boss

@MainActor
final class AIReviewStateBadgeTests: XCTestCase {
    /// The click opens the findings brief AND reveals the findings revision.
    /// A queued/active revision has its own card, so the reveal targets it;
    /// an in-review/done one is only a rollup line on the parent, so the
    /// reveal lands on the parent and the tooltip says so.
    @MainActor
    func testFindingsClickOpensBriefAndRevealsRevisionCard() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let parent = makeTask(id: "parent", kind: "chore")
        var revision = makeTask(id: "revision", kind: "revision")
        revision.parentTaskId = parent.id
        var openCount = 0
        model.asyncMarkdownViewerOpener = { openCount += 1 }
        model.selectWorkProduct(parent.productID)

        let expectedCard = ["todo": "revision", "active": "revision", "in_review": "parent", "done": "parent"]
        for status in ["todo", "active", "in_review", "done"] {
            revision.status = status
            model.choresByProductID = [parent.productID: [parent, revision]]
            model.openAIReviewFindings(revisionID: revision.id)

            guard case .loaded(let title, let markdown, let artifact) = model.asyncMarkdownViewerVM.state else {
                return XCTFail("Expected the findings document to open")
            }
            XCTAssertEqual(title, revision.name)
            XCTAssertEqual(markdown, revision.description)
            XCTAssertEqual(artifact, .workItem(id: revision.id))
            XCTAssertEqual(model.selectedWorkCardID, expectedCard[status], status)
            XCTAssertEqual(model.revealCardTarget(for: revision.id), .revealed(cardID: expectedCard[status]!), status)

            let note = model.aiReviewFindingsTooltipNote(revisionID: revision.id)
            XCTAssertEqual(note.contains("rather than as its own card"), expectedCard[status] == "parent", status)
        }
        XCTAssertEqual(openCount, 4)
    }

    @MainActor
    func testTooltipSaysNoFixTaskYetWithoutRevision() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        XCTAssertTrue(model.aiReviewFindingsTooltipNote(revisionID: nil).contains("No fix task yet"))
        let badge = AIReviewStateBadge(
            state: "reviewed_with_findings", presentation: presentation,
            findingsTooltipNote: "No fix task yet."
        )
        XCTAssertTrue(badge.tooltip.hasSuffix("No fix task yet."))
    }

    @MainActor
    func testMissingFindingsRevisionReportsErrorWithoutNavigating() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.asyncMarkdownViewerOpener = { XCTFail("Missing revision must not open a document") }
        model.openAIReviewFindings(revisionID: "missing")
        XCTAssertTrue(model.workErrorMessage?.contains("no longer available") == true)
        XCTAssertNil(model.pendingRevealScrollID)
    }

    private func makeTask(id: String, kind: String) -> WorkTask {
        WorkTask(
            id: id, productID: "product", projectID: nil, kind: kind,
            name: id, description: "## Findings for \(id)\nFix the incorrect click target.",
            status: "in_review", priority: "medium", ordinal: nil, prURL: nil,
            deletedAt: nil, createdAt: "2026-10-08T00:00:00Z", updatedAt: "2026-10-08T00:00:00Z"
        )
    }

    private let presentation = AIReviewBadgePresentation(
        label: "AI review: clean",
        systemImage: "checkmark.seal.fill",
        tooltip: "Last reviewed abc1234 on 2026-10-08 13:00 UTC: clean."
    )

    func testOnlyCurrentHeadAllClearRendersGreen() {
        XCTAssertEqual(AIReviewStateBadge(state: "reviewed_all_clear", presentation: presentation).tint, .green)
        for state in ["not_reviewed", "reviewed_clean_pending", "reviewed_with_findings",
                      "reviewing", "review_queued", "review_not_required"] {
            XCTAssertNotEqual(AIReviewStateBadge(state: state, presentation: presentation).tint, .green, state)
        }
    }

    func testEnginePresentationVisibleOnlyInReviewForEveryCardKind() {
        for kind in ["task", "chore", "project_task", "revision"] {
            var task = makeTask(kind: kind)
            task.aiReviewState = "reviewed_all_clear"
            task.aiReviewBadge = presentation
            task.aiReviewFindingsRevisionId = "revision"
            for lane in [WorkBoardColumnKey.backlog, .doing, .review, .done] {
                let snapshot = WorkCardSnapshot.build(task: task, context: WorkCardSnapshotContext(column: lane))
                let strip = WorkBoardCardBadgeStripSlice(snapshot: snapshot)
                if lane == .review {
                    XCTAssertEqual(strip.aiReviewBadge, presentation, kind)
                } else {
                    XCTAssertNil(strip.aiReviewBadge, "\(kind) in \(lane)")
                    XCTAssertNil(strip.aiReviewState)
                    XCTAssertNil(strip.aiReviewFindingsRevisionId)
                }
            }
        }
    }

    func testBadgeCopyParticipatesInSnapshotEquality() {
        var task = makeTask(kind: "chore")
        task.aiReviewBadge = AIReviewBadgePresentation.parse([
            "label": "AI review: clean", "system_image": "checkmark.seal.fill", "tooltip": "Earlier review"
        ])
        XCTAssertEqual(task.aiReviewBadge?.tooltip, "Earlier review")
        let context = WorkCardSnapshotContext(column: .review)
        let before = WorkCardSnapshot.build(task: task, context: context)
        task.aiReviewBadge = presentation
        let after = WorkCardSnapshot.build(task: task, context: context)
        XCTAssertNotEqual(before, after)
        XCTAssertNotEqual(WorkBoardCardBadgeStripSlice(snapshot: before), WorkBoardCardBadgeStripSlice(snapshot: after))
    }

    func testParserPreservesEngineCopyAndAllowsOlderPayloads() throws {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var payload: [String: Any] = [
            "id": "card", "product_id": "product", "kind": "chore", "name": "Card",
            "description": "", "status": "in_review", "created_at": "0", "updated_at": "0"
        ]
        XCTAssertNil(try XCTUnwrap(client.parseTask(payload)).aiReviewBadge)
        payload["ai_review_badge"] = [
            "label": presentation.label, "system_image": presentation.systemImage,
            "tooltip": presentation.tooltip
        ]
        XCTAssertEqual(try XCTUnwrap(client.parseTask(payload)).aiReviewBadge, presentation)
    }

    private func makeTask(kind: String) -> WorkTask {
        WorkTask(
            id: "card", productID: "product", projectID: nil, kind: kind,
            name: "Card", description: "", status: "active", priority: "medium",
            ordinal: nil, prURL: nil, deletedAt: nil, createdAt: "0", updatedAt: "0"
        )
    }
}
