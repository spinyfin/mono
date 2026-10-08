import XCTest
@testable import Boss

final class AIReviewStateBadgeTests: XCTestCase {
    @MainActor
    func testFindingsClickOpensRevisionBriefWithoutRevealingParentCard() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let parent = makeTask(id: "parent", kind: "chore")
        var revision = makeTask(id: "revision", kind: "revision")
        revision.parentTaskId = parent.id
        var openCount = 0
        model.asyncMarkdownViewerOpener = { openCount += 1 }

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
            XCTAssertNil(model.pendingRevealScrollID)
            XCTAssertNil(model.revealHighlightID)
        }
        XCTAssertEqual(openCount, 4)
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

    func testOnlyCurrentHeadAllClearRendersGreen() {
        XCTAssertEqual(AIReviewStateBadge(state: "reviewed_all_clear").tint, .green)
        for state in ["not_reviewed", "reviewed_clean_pending", "reviewed_with_findings",
                      "reviewing", "review_queued", "review_not_required"] {
            XCTAssertNotEqual(AIReviewStateBadge(state: state).tint, .green, state)
        }
    }

    func testUnknownHeadAndCleanButNotReadyHaveDistinctExplanations() {
        let unreviewed = AIReviewStateBadge(state: "not_reviewed")
        XCTAssertEqual(unreviewed.systemImage, "questionmark.circle")
        XCTAssertTrue(unreviewed.tooltip.contains("no completed AI review"))
        let pending = AIReviewStateBadge(state: "reviewed_clean_pending")
        XCTAssertTrue(pending.tooltip.contains("AI review passed"))
        XCTAssertTrue(pending.tooltip.contains("prevent readiness"))
    }
}
