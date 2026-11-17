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
    func testHostedInlineCommentsLoadSubmitAndCoexistWithStandaloneReader() throws {
        let task = root()
        let model = NavigatorTestFixture.model([task])
        model.navigator.tab = .review
        model.navigatorReviewGuides.load(root: task) { _ in "load" }
        model.navigatorReviewGuides.receive(versionID: "version", content: content(), requestID: "load")
        let inline = host(WorkNavigatorReview(context: NavigatorDetailContext(
            model: model, root: task, ask: nil, isReadOnly: false)))
        settle(inline)
        let inlineLayer = try XCTUnwrap(model.commentBridge.registeredLayersForTesting.first)
        let standalone = host(MarkdownViewerView(title: "Standalone", source: content().markdown,
            artifact: .reviewGuide(seriesID: "series", versionID: "version"))
            .environment(\.commentBackend, model.commentBridge))
        settle(inline)
        settle(standalone)
        let layers = model.commentBridge.registeredLayersForTesting
        XCTAssertEqual(layers.count, 2, "Both hosted readers must bind the engine backend")
        let persisted = CommentWithThread(comment: WorkComment(
            id: "persisted", artifactId: "series", anchor: CommentAnchor(exact: "Review"),
            artifactKind: WireArtifactKind.reviewGuide, author: "user:test", body: "Saved feedback",
            createdAt: "2026-11-17T00:00:00Z",
            guideContext: GuideCommentContext(versionId: "version", comparisonId: "source",
                packetHash: "hash", baseSha: "base", mergeBaseSha: "merge", headSha: "head")),
            threadEntries: [], answerAgentRunning: false, answerAgentFailed: false)
        model.commentBridge.handleCommentsList(artifactKind: WireArtifactKind.reviewGuide,
            artifactId: "series", comments: [persisted])
        XCTAssertTrue(layers.allSatisfy { $0.comments.first?.body == "Saved feedback" })
        inlineLayer.addComment(quoted: "Review", body: "Inline submission")
        XCTAssertTrue(model.engine.hasUnsentAction(kind: "comments_create"),
                      "A disconnected transport retains the backend submission for retry")
        XCTAssertFalse(inlineLayer.comments.contains { $0.body == "Inline submission" },
                       "Engine-backed submissions must wait for the persisted echo")
        withExtendedLifetime((inline, standalone)) {}
    }

    func testHostedReadOnlyReaderLoadsCommentsButCannotComposeOrSubmit() throws {
        let task = root()
        let model = NavigatorTestFixture.model([task])
        model.navigatorReviewGuides.load(root: task) { _ in "load" }
        model.navigatorReviewGuides.receive(versionID: "version", content: content(), requestID: "load")
        let hosting = host(WorkNavigatorReview(context: NavigatorDetailContext(
            model: model, root: task, ask: nil, isReadOnly: false)))
        settle(hosting)
        let layer = try XCTUnwrap(model.commentBridge.registeredLayersForTesting.first)
        XCTAssertTrue(layer.isEngineBacked)
        layer.ownsGuideDraft = true
        layer.saveGuideDraft(body: "Keep my draft")
        layer.isShowingPopover = true
        hosting.rootView = WorkNavigatorReview(context: NavigatorDetailContext(
            model: model, root: task, ask: nil, isReadOnly: true))
        settle(hosting)
        XCTAssertTrue(layer.isReadOnly)
        XCTAssertFalse(layer.isShowingPopover)
        XCTAssertEqual(layer.guideDraft?.body, "Keep my draft")
        layer.requestNewComment()
        XCTAssertFalse(layer.isShowingPopover)
        layer.addComment(quoted: "Review", body: "Must not submit")
        XCTAssertFalse(model.engine.hasUnsentAction(kind: "comments_create"))
    }

    func testInactiveReviewDoesNotLoadUntilSelected() {
        let task = root()
        let model = NavigatorTestFixture.model([task])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard(task.id)
        model.navigator.tab = .activity
        let hosting = host(WorkNavigatorDetail(model: model, registry: .navigator(workspace: nil)))
        settle(hosting)
        XCTAssertNil(model.navigatorReviewGuides.readers[task.id])
        model.navigator.tab = .review
        settle(hosting)
        XCTAssertNotNil(model.navigatorReviewGuides.readers[task.id])
    }

    func testLandedEvidenceHidesMergeAndExplainsRetainedGuide() throws {
        var task = root()
        task.status = "in_review"
        XCTAssertTrue(ReviewGuideViewerHeader.showsMergeControl(task))
        task.navigator = NavigatorProjection.parse([
            "version": 1, "root_task_id": task.id,
            "root": ["section": "landed", "sort_key": "0", "default_detail": "review",
                     "reason": ["type": "merged"], "reason_label": "Merged",
                     "revisions": ["open": 0, "total": 0],
                     "merge_evidence": ["pr_url": task.prURL!, "observed_at": "now"]]])
        XCTAssertNotNil(task.navigator?.root?.mergeEvidence)
        XCTAssertFalse(ReviewGuideViewerHeader.showsMergeControl(task))
        XCTAssertEqual(WorkNavigatorReview.gateExplanation(task),
                       "Merged — review guide and evidence are retained for this completed attempt.")
    }

    func testDisplayedSourceComparisonAndStalenessLabels() {
        XCTAssertEqual(ReviewGuideViewerHeader.sourceLabels(comparisonID: "old", selectedID: "new"),
                       ["Source comparison: old", "This guide covers an older source revision."])
        XCTAssertEqual(ReviewGuideViewerHeader.sourceLabels(comparisonID: "same", selectedID: "same"),
                       ["Source comparison: same"])
        XCTAssertEqual(ReviewGuideViewerHeader.sourceLabels(comparisonID: "old", selectedID: nil),
                       ["Source comparison: old"])
    }

    func testMergeRetryAndGenerateRespectReadOnlyAndDisconnectedStates() {
        let task = root()
        let model = NavigatorTestFixture.model([task])
        for connected in [false, true] {
            model.isConnected = connected
            for readOnly in [false, true] {
                let header = ReviewGuideViewerHeader(chatModel: model, rootTaskId: task.id,
                    generatedAt: nil, isReadOnly: readOnly)
                let review = WorkNavigatorReview(context: NavigatorDetailContext(
                    model: model, root: task, ask: nil, isReadOnly: readOnly))
                XCTAssertEqual(header.actionsDisabled, readOnly || !connected)
                XCTAssertEqual(review.actionsDisabled, readOnly || !connected)
            }
        }
    }

    func testHistoryVersionsOpenPinnedGuideAndSelectReviewTab() {
        let task = root()
        let model = NavigatorTestFixture.model([task])
        let store = model.navigatorReviewGuides
        store.load(root: task) { _ in "first" }
        store.receive(versionID: "version", content: content(), requestID: "first")
        store.load(root: task, versionID: "older") { _ in "second" }
        store.receive(versionID: "older", content: content("older"), requestID: "second")
        let history = NavigatorReviewHistory(model: model, root: task)
        XCTAssertEqual(history.viewedVersions.map(\.id), ["version", "older"])
        history.open("version")
        XCTAssertEqual(store.readers[task.id]?.content?.id, "version")
        XCTAssertEqual(model.navigator.tab, .review)
        var rebound = task
        rebound.prURL = "https://github.com/spinyfin/mono/pull/8"
        XCTAssertTrue(NavigatorReviewHistory(model: model, root: rebound).viewedVersions.isEmpty)
    }


    func testHeaderPollingStopsWhenReviewBecomesInactive() {
        let task = root()
        let model = NavigatorTestFixture.model([task])
        var polls = 0
        model.engine.outboundRecorder = { payload in
            if payload["type"] as? String == "get_review_guide_summary" { polls += 1 }
        }
        func header(active: Bool) -> ReviewGuideViewerHeader {
            ReviewGuideViewerHeader(chatModel: model, rootTaskId: task.id, generatedAt: nil,
                inlineGuide: InlineReviewGuideHeader(versionID: "version", comparisonID: "source",
                    artifact: .reviewGuide(seriesID: "series", versionID: "version"), openVersion: { _ in }),
                isActive: active)
        }
        let hosting = host(header(active: false))
        settle(hosting)
        XCTAssertEqual(polls, 0)
        hosting.rootView = header(active: true)
        settle(hosting)
        XCTAssertEqual(polls, 1)
        hosting.rootView = header(active: false)
        settle(hosting)
        RunLoop.current.run(until: Date().addingTimeInterval(5.2))
        XCTAssertEqual(polls, 1, "The hidden header must cancel the five-second polling task")
    }

    private func host<V: View>(_ view: V) -> NSHostingView<V> {
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 1000, height: 700)
        return hosting
    }

    private func settle<V: View>(_ host: NSHostingView<V>) {
        host.layoutSubtreeIfNeeded()
        RunLoop.current.run(until: Date().addingTimeInterval(0.15))
        host.layoutSubtreeIfNeeded()
    }

}
