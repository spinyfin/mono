import XCTest
import SwiftUI
@testable import Boss

@MainActor
final class GuideCommentTests: XCTestCase {
    func testAuthoringUsesVersionAndOriginalProjection() {
        let layer = CommentLayer()
        let backend = GuideCommentBackend()
        layer.configure(source: "Original quote", baseURL: nil,
                        artifact: .reviewGuide(seriesID: "series", versionID: "old"), backend: backend)
        layer.addComment(quoted: "Original quote", body: "Keep this behavior")
        XCTAssertEqual(backend.createdVersion, "old")
        XCTAssertEqual(backend.createdHash, CommentProjection.docVersion(forPlainText: "Original quote"))
        XCTAssertEqual(backend.resolvedVersion, "old")
        XCTAssertEqual(backend.resolveCount, 1)
        layer.updateSource("New guide prose", baseURL: nil)
        XCTAssertEqual(backend.resolveCount, 1, "guides must not re-resolve against mutated prose")
        XCTAssertEqual(layer.currentProjection(), CommentProjection.plainText(for: "New guide prose"))
        layer.reviseDoc()
        XCTAssertFalse(backend.didRevise)
    }

    func testUpdateSourceAssignsEmptyGuideMarkdownWithoutReresolving() {
        let layer = CommentLayer()
        let backend = GuideCommentBackend()
        layer.configure(source: "", baseURL: nil,
                        artifact: .reviewGuide(seriesID: "series", versionID: "late"), backend: backend)
        XCTAssertEqual(backend.resolveCount, 0)
        layer.updateSource("Original quote", baseURL: nil)
        XCTAssertEqual(layer.currentProjection(), CommentProjection.plainText(for: "Original quote"))
        XCTAssertEqual(backend.resolveCount, 0)
    }

    func testOlderFeedbackKeepsThreadsAndOnlyCurrentVersionHighlights() {
        let layer = CommentLayer()
        let backend = GuideCommentBackend()
        layer.configure(source: "Same quote", baseURL: nil,
                        artifact: .reviewGuide(seriesID: "series", versionID: "new"), backend: backend)
        let old = wireComment(id: "old-comment", version: "old")
        let current = wireComment(id: "new-comment", version: "new")
        layer.applyList([old, current])
        XCTAssertEqual(layer.currentVersionComments.map(\.id), ["new-comment"])
        XCTAssertEqual(layer.otherVersionComments.map(\.id), ["old-comment"])
        XCTAssertEqual(layer.otherVersionComments.first?.threadEntries.count, 1)
        var opened: String?
        layer.openOriginalGuide = { opened = $0 }
        layer.jumpTo(layer.otherVersionComments[0])
        XCTAssertEqual(opened, "old")
        XCTAssertNil(layer.flashingAnchor)
        layer.applyResolved([ResolvedComment(comment: old.comment,
            resolution: CommentResolution(kind: "orphan", length: nil, score: nil, start: nil))])
        XCTAssertEqual(layer.otherVersionComments[0].status, .active, "late replies for other versions cannot orphan feedback")
        layer.applyResolved([ResolvedComment(comment: current.comment,
            resolution: CommentResolution(kind: "fuzzy", length: 5, score: 0.95, start: 5))])
        XCTAssertEqual(layer.currentVersionComments[0].displayAnchor.exact, "quote")
        XCTAssertEqual(layer.currentVersionComments[0].quotedText, "Same quote")
        layer.applyList([old, current])
        XCTAssertEqual(layer.currentVersionComments[0].displayAnchor.exact, "quote")
        layer.reload()
        XCTAssertEqual(layer.otherVersionComments[0].quotedText, "Same quote")
    }

    func testDraftSurvivesViewerReplacementAndRemainsBoundToVersion() {
        let backend = GuideCommentBackend()
        let old = CommentLayer()
        old.configure(source: "Original quote", baseURL: nil,
                      artifact: .reviewGuide(seriesID: "series", versionID: "draft-old"), backend: backend)
        old.pendingQuotedText = "Original quote"
        old.pendingOccurrenceIndex = 2
        old.saveGuideDraft(body: "Unsaved feedback")
        old.reload()
        old.unbindFromEngine()
        let replacement = CommentLayer()
        replacement.configure(source: "New prose", baseURL: nil,
                              artifact: .reviewGuide(seriesID: "series", versionID: "draft-new"), backend: backend)
        XCTAssertNil(replacement.guideDraft)
        let original = CommentLayer()
        original.configure(source: "Original quote", baseURL: nil,
                           artifact: .reviewGuide(seriesID: "series", versionID: "draft-old"), backend: backend)
        XCTAssertEqual(original.guideDraft?.body, "Unsaved feedback")
        XCTAssertEqual(original.guideDraft?.quote, "Original quote")
        XCTAssertEqual(original.guideDraft?.occurrenceIndex, 2)
        original.resumeGuideDraft()
        original.cancelNewComment()
        XCTAssertNil(original.guideDraft)
    }

    func testRequestNewCommentPrefersLiveSelectionOverSavedDraft() {
        let backend = GuideCommentBackend()
        let layer = CommentLayer()
        layer.configure(source: "Original quote then Different quote", baseURL: nil,
                        artifact: .reviewGuide(seriesID: "series", versionID: "draft-sel"), backend: backend)
        layer.pendingQuotedText = "Original quote"
        layer.pendingOccurrenceIndex = 0
        layer.saveGuideDraft(body: "Unsaved feedback")
        layer.liveSelectionProvider = { "Different quote" }
        layer.requestNewComment()
        XCTAssertEqual(layer.pendingQuotedText, "Different quote")
        XCTAssertFalse(layer.pendingResumeDraft)
        XCTAssertEqual(layer.guideDraft?.quote, "Original quote")
        XCTAssertEqual(layer.guideDraft?.body, "Unsaved feedback")
        let composer = NSHostingController(rootView: CommentPopover(layer: layer))
        composer.loadView()
        layer.saveGuideDraft(body: "")
        layer.saveGuideDraft(body: "New feedback")
        layer.cancelNewComment()
        XCTAssertEqual(layer.guideDraft?.body, "Unsaved feedback")
        layer.requestNewComment(firstChar: "N")
        layer.saveGuideDraft(body: "New feedback")
        layer.addComment(quoted: "Different quote", body: "New feedback")
        XCTAssertEqual(layer.guideDraft?.quote, "Original quote")
        XCTAssertEqual(layer.guideDraft?.body, "Unsaved feedback")
    }

    func testResumeGuideDraftIgnoresLiveSelection() {
        let backend = GuideCommentBackend()
        let layer = CommentLayer()
        layer.configure(source: "Original quote then Different quote", baseURL: nil,
                        artifact: .reviewGuide(seriesID: "series", versionID: "draft-resume"), backend: backend)
        layer.pendingQuotedText = "Original quote"
        layer.pendingOccurrenceIndex = 2
        layer.saveGuideDraft(body: "Unsaved feedback")
        layer.liveSelectionProvider = { "Different quote" }
        layer.resumeGuideDraft()
        XCTAssertEqual(layer.pendingQuotedText, "Original quote")
        XCTAssertEqual(layer.pendingOccurrenceIndex, 2)
        XCTAssertEqual(layer.pendingTypeahead, "Unsaved feedback")
        XCTAssertTrue(layer.pendingResumeDraft)
    }

    func testEmptyQuoteDraftSurvivesViewerReplacementOnOriginalVersion() {
        let backend = GuideCommentBackend()
        let old = CommentLayer()
        old.configure(source: "Guide prose", baseURL: nil,
                      artifact: .reviewGuide(seriesID: "series", versionID: "empty-quote-old"), backend: backend)
        old.pendingQuotedText = ""
        old.pendingOccurrenceIndex = 0
        old.saveGuideDraft(body: "General feedback")
        old.reload()
        old.unbindFromEngine()
        let replacement = CommentLayer()
        replacement.configure(source: "New prose", baseURL: nil,
                              artifact: .reviewGuide(seriesID: "series", versionID: "empty-quote-new"), backend: backend)
        XCTAssertNil(replacement.guideDraft)
        let original = CommentLayer()
        original.configure(source: "Guide prose", baseURL: nil,
                           artifact: .reviewGuide(seriesID: "series", versionID: "empty-quote-old"), backend: backend)
        XCTAssertEqual(original.guideDraft?.body, "General feedback")
        XCTAssertEqual(original.guideDraft?.quote, "")
        XCTAssertEqual(original.guideDraft?.occurrenceIndex, 0)
        original.liveSelectionProvider = { "" }
        original.resumeGuideDraft()
        original.addComment(quoted: "", body: "General feedback")
        XCTAssertNil(backend.createdVersion)
        XCTAssertNotNil(original.guideDraft)
        original.liveSelectionProvider = { "Guide prose" }
        original.resumeGuideDraft()
        XCTAssertEqual(original.pendingQuotedText, "Guide prose")
        original.addComment(quoted: original.pendingQuotedText, body: "General feedback")
        XCTAssertEqual(backend.createdVersion, "empty-quote-old")
        XCTAssertNotNil(original.guideDraft, "keep draft until the engine confirms persistence")
        let persisted = WorkComment(
            id: "persisted", artifactId: "series", anchor: CommentAnchor(exact: "Guide prose"),
            artifactKind: WireArtifactKind.reviewGuide, author: "user:test", body: "General feedback",
            createdAt: "1", guideContext: GuideCommentContext(versionId: "empty-quote-old",
                comparisonId: "comparison", packetHash: "hash", baseSha: "base",
                mergeBaseSha: "merge", headSha: "head"))
        backend.guideCommentDrafts!.acknowledge(persisted)
        XCTAssertNil(original.guideDraft)
    }

    func testFailedPresentationKeepsResumeIntent() {
        let layer = CommentLayer()
        let backend = GuideCommentBackend()
        layer.configure(source: "Quote", baseURL: nil,
                        artifact: .reviewGuide(seriesID: "series", versionID: "pending"), backend: backend)
        layer.pendingQuotedText = "Quote"
        layer.saveGuideDraft(body: "Feedback")
        backend.guideCommentDrafts!.pendingResumeVersionId = "pending"
        defer {
            layer.cancelNewComment()
        }
        XCTAssertTrue(backend.guideCommentDrafts!.hasPendingResume(for: "pending"))
        XCTAssertFalse(layer.resumeGuideDraft(), "a guide without its host window cannot present")
        XCTAssertTrue(backend.guideCommentDrafts!.hasPendingResume(for: "pending"))
        let host = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 200),
            styleMask: [.borderless], backing: .buffered, defer: false)
        host.contentView = NSView(frame: NSRect(x: 0, y: 0, width: 200, height: 200))
        layer.setHostWindow(host)
        XCTAssertFalse(
            backend.guideCommentDrafts!.hasPendingResume(for: "pending"),
            "binding the host window must retry presentation and drain the intent")
    }

    func testAcknowledgeClearsDraftWhenPersistedExactIsNormalized() {
        let backend = GuideCommentBackend()
        let listLayer = CommentLayer()
        let listSource = "Embedded (built-in) checks — compiled into the binary"
        listLayer.configure(source: listSource, baseURL: nil,
                            artifact: .reviewGuide(seriesID: "series", versionID: "ack-list"), backend: backend)
        listLayer.pendingQuotedText = "  • binary"
        listLayer.saveGuideDraft(body: "Keep binary")
        listLayer.addComment(quoted: "  • binary", body: "Keep binary")
        XCTAssertEqual(listLayer.guideDrafts.count, 1)
        let listAnchor = CommentLayer.captureAnchor(
            quoted: "  • binary", occurrenceIndex: 0, in: CommentProjection.plainText(for: listSource))
        XCTAssertEqual(listAnchor.exact, "binary")
        backend.guideCommentDrafts!.acknowledge(WorkComment(
            id: "list", artifactId: "series", anchor: CommentAnchor(exact: listAnchor.exact),
            artifactKind: WireArtifactKind.reviewGuide, author: "user:test", body: "Keep binary",
            createdAt: "1", guideContext: GuideCommentContext(versionId: "ack-list",
                comparisonId: "comparison", packetHash: "hash", baseSha: "base",
                mergeBaseSha: "merge", headSha: "head")))
        XCTAssertNil(listLayer.guideDraft)

        let wsLayer = CommentLayer()
        wsLayer.configure(source: "trailing quote", baseURL: nil,
                          artifact: .reviewGuide(seriesID: "series", versionID: "ack-ws"), backend: backend)
        wsLayer.pendingQuotedText = "quote   "
        wsLayer.saveGuideDraft(body: "Trim me")
        wsLayer.addComment(quoted: "quote   ", body: "Trim me")
        let wsAnchor = CommentLayer.captureAnchor(
            quoted: "quote   ", occurrenceIndex: 0, in: CommentProjection.plainText(for: "trailing quote"))
        XCTAssertEqual(wsAnchor.exact, "quote")
        backend.guideCommentDrafts!.acknowledge(WorkComment(
            id: "ws", artifactId: "series", anchor: CommentAnchor(exact: wsAnchor.exact),
            artifactKind: WireArtifactKind.reviewGuide, author: "user:test", body: "Trim me",
            createdAt: "1", guideContext: GuideCommentContext(versionId: "ack-ws",
                comparisonId: "comparison", packetHash: "hash", baseSha: "base",
                mergeBaseSha: "merge", headSha: "head")))
        XCTAssertNil(wsLayer.guideDraft)
    }

    func testSecondComposerDraftSurvivesViewerReplacementAndFailedSave() {
        let backend = GuideCommentBackend()
        let original = CommentLayer()
        original.configure(source: "Original quote then Different quote", baseURL: nil,
                           artifact: .reviewGuide(seriesID: "series", versionID: "two-composers"), backend: backend)
        original.pendingQuotedText = "Original quote"
        original.saveGuideDraft(body: "Parked A")
        original.liveSelectionProvider = { "Different quote" }
        original.requestNewComment()
        original.saveGuideDraft(body: "Composer B")
        original.addComment(quoted: "Different quote", body: "Composer B")
        XCTAssertEqual(
            original.guideDrafts.map(\.body).sorted(),
            ["Composer B", "Parked A"],
            "a failed persist must keep composer B alongside the parked draft")
        XCTAssertEqual(original.guideDraft?.body, "Parked A")

        original.reload()
        original.unbindFromEngine()
        let replacement = CommentLayer()
        replacement.configure(source: "Original quote then Different quote", baseURL: nil,
                              artifact: .reviewGuide(seriesID: "series", versionID: "two-composers"), backend: backend)
        XCTAssertEqual(replacement.guideDrafts.map(\.body).sorted(), ["Composer B", "Parked A"])
        replacement.resumeGuideDraft()
        XCTAssertEqual(replacement.pendingTypeahead, "Parked A")
        replacement.cancelNewComment()
        XCTAssertEqual(replacement.guideDraft?.body, "Composer B")
        replacement.resumeGuideDraft()
        XCTAssertEqual(replacement.pendingTypeahead, "Composer B")
    }

    private func wireComment(id: String, version: String) -> CommentWithThread {
        CommentWithThread(comment: WorkComment(
            id: id, artifactId: "series", anchor: CommentAnchor(exact: "Same quote"),
            artifactKind: WireArtifactKind.reviewGuide, author: "user:test", body: "Feedback", createdAt: "1",
            guideContext: GuideCommentContext(versionId: version, comparisonId: "comparison",
                packetHash: "hash", baseSha: "base", mergeBaseSha: "merge", headSha: "head")),
            threadEntries: [WireCommentThreadEntry(id: "reply", commentId: id, entryKind: "operator_followup",
                author: "user:test", body: "Reply", reviseTaskId: nil, answerAgentRunId: nil, createdAt: "2")],
            answerAgentRunning: false, answerAgentFailed: false)
    }
}

@MainActor
private final class GuideCommentBackend: CommentBackend {
    let author = "user:test"
    let guideCommentDrafts: GuideCommentDrafts? = GuideCommentDrafts(directory: nil)
    var createdVersion: String?
    var resolvedVersion: String?
    var createdHash: String?
    var didRevise = false
    var resolveCount = 0
    func registerCommentLayer(_ layer: CommentLayer, artifactKind: String, artifactId: String) {}
    func unregisterCommentLayer(_ layer: CommentLayer) {}
    func createComment(artifactKind: String, artifactId: String, anchor: CommentAnchor, body: String,
                       docVersion: String, guideVersionId: String?) {
        createdVersion = guideVersionId
        createdHash = docVersion
    }
    func listComments(artifactKind: String, artifactId: String, includeResolved: Bool) {}
    func resolveComments(artifactKind: String, artifactId: String, plainText: String, guideVersionId: String?) {
        resolvedVersion = guideVersionId
        resolveCount += 1
    }
    func dismissComment(commentId: String) {}
    func setStatus(commentId: String, status: String) {}
    func updateAnchor(commentId: String, anchor: CommentAnchor, newDocVersion: String) {}
    func setIntent(commentId: String, intent: String) {}
    func fetchBannerState(artifactKind: String, artifactId: String) {}
    func postFollowup(commentId: String, body: String) {}
    func reviseDoc(artifactKind: String, artifactId: String) { didRevise = true }
}
