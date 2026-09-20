import XCTest
@testable import Boss

@MainActor
final class GuideCommentTests: XCTestCase {
    func testAuthoringUsesVersionAndOriginalProjection() {
        let layer = CommentLayer()
        let backend = GuideCommentBackend()
        layer.configure(source: "Original quote", baseURL: nil,
                        artifact: .reviewGuide(seriesID: "series", versionID: "old"), backend: backend)
        layer.updateSource("New guide prose", baseURL: nil)
        layer.addComment(quoted: "Original quote", body: "Keep this behavior")
        XCTAssertEqual(backend.createdVersion, "old")
        XCTAssertEqual(backend.createdHash, CommentProjection.docVersion(forPlainText: "Original quote"))
        XCTAssertEqual(backend.resolvedVersion, "old")
        layer.reviseDoc()
        XCTAssertFalse(backend.didRevise)
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
        defer { old.discardGuideDraft() }
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
        original.cancelNewComment()
        XCTAssertNil(original.guideDraft)
    }

    func testRequestNewCommentPrefersLiveSelectionOverSavedDraft() {
        let backend = GuideCommentBackend()
        let layer = CommentLayer()
        layer.configure(source: "Original quote then Different quote", baseURL: nil,
                        artifact: .reviewGuide(seriesID: "series", versionID: "draft-sel"), backend: backend)
        defer { layer.discardGuideDraft() }
        layer.pendingQuotedText = "Original quote"
        layer.pendingOccurrenceIndex = 0
        layer.saveGuideDraft(body: "Unsaved feedback")
        layer.testingLiveSelection = "Different quote"
        layer.requestNewComment()
        XCTAssertEqual(layer.pendingQuotedText, "Different quote")
        XCTAssertFalse(layer.pendingResumeDraft)
        XCTAssertEqual(layer.guideDraft?.quote, "Original quote")
        XCTAssertEqual(layer.guideDraft?.body, "Unsaved feedback")
    }

    func testResumeGuideDraftIgnoresLiveSelection() {
        let backend = GuideCommentBackend()
        let layer = CommentLayer()
        layer.configure(source: "Original quote then Different quote", baseURL: nil,
                        artifact: .reviewGuide(seriesID: "series", versionID: "draft-resume"), backend: backend)
        defer { layer.discardGuideDraft() }
        layer.pendingQuotedText = "Original quote"
        layer.pendingOccurrenceIndex = 2
        layer.saveGuideDraft(body: "Unsaved feedback")
        layer.testingLiveSelection = "Different quote"
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
        defer { old.discardGuideDraft() }
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
        original.cancelNewComment()
        XCTAssertNil(original.guideDraft)
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
    var createdVersion: String?
    var resolvedVersion: String?
    var createdHash: String?
    var didRevise = false
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
    }
    func dismissComment(commentId: String) {}
    func setStatus(commentId: String, status: String) {}
    func updateAnchor(commentId: String, anchor: CommentAnchor, newDocVersion: String) {}
    func setIntent(commentId: String, intent: String) {}
    func fetchBannerState(artifactKind: String, artifactId: String) {}
    func postFollowup(commentId: String, body: String) {}
    func reviseDoc(artifactKind: String, artifactId: String) { didRevise = true }
}
