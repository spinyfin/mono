import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class NavigatorReviewTests: XCTestCase {
    private func root(_ id: String = "root", version: String = "version") -> WorkTask {
        var task = NavigatorTestFixture.task(id)
        task.reviewGuideReadableVersionId = version
        task.reviewGuideSelectedComparisonId = "source"
        task.reviewGuideLifecycle = "ready"
        return task
    }

    private func content(_ id: String = "version", comparison: String = "source") -> ReviewGuideVersionContent {
        ReviewGuideVersionContent(id: id, seriesId: "series", comparisonId: comparison,
                                  attemptId: "attempt", markdown: "## Guide\nReview the change.", contentHash: "hash",
                                  promptVersion: "1", generatedAt: "2026-11-17T00:00:00Z")
    }

    func testReadersAndStandaloneWindowOwnRepliesIndependently() throws {
        let first = root()
        let second = root("second", version: "second-version")
        let model = NavigatorTestFixture.model([first, second])
        model.loadNavigatorReviewGuide(for: first)
        let inlineID = try XCTUnwrap(model.navigatorReviewGuides.readers[first.id]?.requestID)
        model.openReviewGuide(for: first)
        let windowID = try XCTUnwrap(model.pendingReviewGuideRequestId)
        XCTAssertNotEqual(inlineID, windowID)
        model.applyEventForTest(.reviewGuideContent(versionId: "version", content: content()))
        XCTAssertNil(model.navigatorReviewGuides.readers[first.id]?.content)
        guard case .loading = model.asyncMarkdownViewerVM.state else { return XCTFail("anonymous reply stole window ownership") }
        model.loadNavigatorReviewGuide(for: second)
        let secondID = try XCTUnwrap(model.navigatorReviewGuides.readers[second.id]?.requestID)
        model.applyEventForTest(.reviewGuideContent(versionId: "version", content: content(), requestId: inlineID))
        XCTAssertEqual(model.navigatorReviewGuides.readers[first.id]?.content, content())
        guard case .loading = model.asyncMarkdownViewerVM.state else { return XCTFail("inline reply stole window ownership") }
        model.applyEventForTest(.reviewGuideContent(versionId: "second-version", content: content("second-version"), requestId: secondID))
        XCTAssertEqual(model.navigatorReviewGuides.readers[second.id]?.content?.id, "second-version")
        model.applyEventForTest(.reviewGuideContent(versionId: "version", content: content(), requestId: windowID))
        guard case .loaded(_, let markdown, _) = model.asyncMarkdownViewerVM.state else { return XCTFail("window did not load") }
        XCTAssertEqual(markdown, content().markdown)
    }

    func testSameVersionRetryRejectsLateSuccessFailureAndAnonymousReplies() {
        let store = NavigatorReviewGuideStore()
        store.load(root: root()) { _ in "old" }
        store.load(root: root(), refresh: true) { _ in "new" }
        store.receive(versionID: "version", content: content(), requestID: "old")
        store.receive(versionID: "version", content: nil, requestID: nil)
        XCTAssertFalse(store.fail(message: "Old error", requestID: "old"))
        XCTAssertEqual(store.readers["root"]?.state, .loading)
        store.receive(versionID: "wrong", content: content(), requestID: "new")
        XCTAssertEqual(store.readers["root"]?.state, .loading)
        store.receive(versionID: "version", content: content(), requestID: "new")
        XCTAssertEqual(store.readers["root"]?.content, content())
        XCTAssertFalse(store.fail(message: "Late error", requestID: "new"))
    }

    func testPinnedVersionSurvivesNewReadableVersionAndPRRebindingClearsHistory() {
        let store = NavigatorReviewGuideStore()
        store.load(root: root()) { _ in "initial" }
        store.receive(versionID: "version", content: content(), requestID: "initial")
        var updated = root(version: "updated")
        store.load(root: updated) { _ in XCTFail("must not replace a pinned guide"); return nil }
        XCTAssertEqual(store.readers["root"]?.versionID, "version")
        store.load(root: updated, versionID: "updated") { _ in "updated-request" }
        store.receive(versionID: "updated", content: content("updated"), requestID: "updated-request")
        XCTAssertEqual(store.readers["root"]?.versions.map(\.id), ["version", "updated"])
        store.load(root: updated, versionID: "version") { _ in XCTFail("viewed immutable content is cached"); return nil }
        XCTAssertEqual(store.readers["root"]?.content?.id, "version")
        updated.prURL = "https://github.com/spinyfin/mono/pull/7"
        store.load(root: updated) { _ in "replacement" }
        XCTAssertEqual(store.readers["root"]?.versions.count, 0)
        XCTAssertEqual(store.readers["root"]?.versionID, "updated")
        store.receive(versionID: "version", content: content(), requestID: "initial")
        XCTAssertEqual(store.readers["root"]?.state, .loading)
    }

    func testDisconnectOnlyFailsPendingReadersAndRetryRejectsOldResponse() {
        let model = NavigatorTestFixture.model([root(), root("other")])
        let store = model.navigatorReviewGuides
        store.load(root: root()) { _ in "pending" }
        store.load(root: root("other")) { _ in "loaded" }
        store.receive(versionID: "version", content: content(), requestID: "loaded")
        model.applyEventForTest(.disconnected)
        XCTAssertEqual(store.readers["root"]?.state, .failed("Connection lost while loading this guide."))
        XCTAssertEqual(store.readers["other"]?.content, content())
        store.receive(versionID: "version", content: content(), requestID: "pending")
        XCTAssertNil(store.readers["root"]?.content)
        store.load(root: root(), refresh: true) { _ in "retry" }
        model.applyEventForTest(.workError(message: "Load refused", requestId: "unrelated"))
        XCTAssertEqual(store.readers["root"]?.state, .loading)
        model.applyEventForTest(.workError(message: "Load refused", requestId: "retry"))
        XCTAssertEqual(store.readers["root"]?.state, .failed("Load refused"))
        store.load(root: root(), refresh: true) { _ in nil }
        guard case .failed = store.readers["root"]?.state else { return XCTFail("dropped send must fail") }
    }

    func testMissingOrMismatchedContentIsRetryableFailure() {
        let store = NavigatorReviewGuideStore()
        store.load(root: root()) { _ in "request" }
        store.receive(versionID: "version", content: content("other"), requestID: "request")
        XCTAssertEqual(store.readers["root"]?.state, .failed("This review guide is no longer available."))
        store.load(root: root(), refresh: true) { _ in "retry" }
        store.receive(versionID: "version", content: nil, requestID: "retry")
        XCTAssertEqual(store.readers["root"]?.state, .failed("This review guide is no longer available."))
    }

    func testAllSlotsStayRegisteredAndReviewSurvivesSectionMovement() {
        let registry = NavigatorDetailRegistry.navigator(workspace: nil)
        XCTAssertEqual(Set(registry.bodies.keys), Set(NavigatorDetailSlot.allCases))
        let model = NavigatorTestFixture.model([root()])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.navigator.tab = .review
        model.isConnected = true
        model.beginNavigatorBriefEdit(root())
        model.setNavigatorBriefText("root", text: "Unsaved original")
        for section in ["working", "needs_attention", "backlog", "landed"] {
            let task = NavigatorTestFixture.task(section: section)
            NavigatorTestFixture.snapshot(model, [task])
            XCTAssertEqual(model.navigator.selectedRootID, "root")
            XCTAssertEqual(model.navigator.tab, .review)
            XCTAssertTrue(WorkNavigatorDetail.visibleTabs(model: model, root: task).contains(.review))
            XCTAssertEqual(model.navigatorBriefText(for: task), "Unsaved original")
            XCTAssertEqual(WorkNavigatorReview.diffURL(task.prURL)?.path, "/spinyfin/mono/pull/3629/files")
        }
    }

    func testMergeControlsFollowExistingEligibilityAndKeepWindowConfirmationSeparate() {
        var task = root()
        task.status = "in_review"
        XCTAssertTrue(task.isMergeWhenReadyEligible)
        XCTAssertTrue(WorkNavigatorReview.gateExplanation(task).contains("checks the current PR gates"))
        task.mergeQueueState = "queued"
        XCTAssertFalse(task.isMergeWhenReadyEligible)
        XCTAssertTrue(WorkNavigatorReview.gateExplanation(task).contains("Waiting for the engine"))
        let model = NavigatorTestFixture.model([task])
        model.isReviewGuideViewerWindowOpen = true
        model.mergingWhenReadyIDs.insert(task.id)
        model.mergeRevisionConfirmationOrigins[task.id] = .board
        model.handleMergeConfirmation(workItemID: task.id, revisions: [OpenMergeRevision(id: "revision", label: "Revision", status: "active")])
        XCTAssertTrue(model.shouldPresentMergeRevisionConfirmation(on: .board))
        XCTAssertFalse(model.shouldPresentMergeRevisionConfirmation(on: .reviewGuideViewer))
    }
}
