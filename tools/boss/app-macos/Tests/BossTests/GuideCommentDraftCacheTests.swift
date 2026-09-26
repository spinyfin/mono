import AppKit
import XCTest
@testable import Boss

@MainActor
final class GuideCommentDraftCacheTests: XCTestCase {
    func testRestartPreservesOrderedDraftsAndSubmittedAnchorUntilEcho() throws {
        let directory = URL(fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"]
            ?? NSTemporaryDirectory()).appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let store = GuideCommentDrafts(directory: directory)
        let first = GuideCommentDraft(seriesId: "series", quote: "quote", occurrenceIndex: 2,
                                      body: "feedback", composerId: UUID())
        let second = GuideCommentDraft(seriesId: "series", quote: "other", occurrenceIndex: 0,
                                       body: "second feedback", composerId: UUID())
        store.upsert(first, version: "old")
        store.upsert(second, version: "old")
        store.upsert(first, version: "new")
        store.submitted[first.composerId] = first.withQuote("normalized")
        let restored = GuideCommentDrafts(directory: directory)
        XCTAssertEqual(restored.drafts(for: "old"), [first, second])
        XCTAssertEqual(restored.drafts(for: "new"), [first])
        XCTAssertEqual(restored.submitted[first.composerId]?.quote, "normalized")
        restored.acknowledge(WorkComment(
            id: "persisted", artifactId: "series", anchor: CommentAnchor(exact: "normalized"),
            artifactKind: WireArtifactKind.reviewGuide, author: "user:test", body: "feedback",
            createdAt: "1", guideContext: GuideCommentContext(versionId: "old",
                comparisonId: "comparison", packetHash: "hash", baseSha: "base",
                mergeBaseSha: "merge", headSha: "head")))
        let afterEcho = GuideCommentDrafts(directory: directory)
        XCTAssertEqual(afterEcho.drafts(for: "old"), [second])
        XCTAssertEqual(afterEcho.drafts(for: "new"), [first])
        afterEcho.removeAll(for: "old")
        afterEcho.remove(version: "new", composerId: first.composerId)
        XCTAssertTrue(GuideCommentDrafts(directory: directory).byVersion.isEmpty)
        XCTAssertTrue(try FileManager.default.contentsOfDirectory(atPath: directory.path).isEmpty)
    }

    func testRequestedComposerWinsOverFirstDraft() {
        let layer = CommentLayer()
        let store = layer.draftStore
        let backend = CommentEngineBridge(
            engine: EngineClient(socketPath: "/tmp/guide-drafts-unused.sock"), draftStore: store)
        layer.configure(source: "quote", baseURL: nil,
                        artifact: .reviewGuide(seriesID: "series", versionID: "old"), backend: backend)
        let first = GuideCommentDraft(seriesId: "series", quote: "quote", occurrenceIndex: 0,
                                      body: "first", composerId: UUID())
        let second = GuideCommentDraft(seriesId: "series", quote: "quote", occurrenceIndex: 0,
                                       body: "second", composerId: UUID())
        store.upsert(first, version: "old")
        store.upsert(second, version: "old")
        store.pendingResumeVersionId = "old"
        store.pendingResumeComposerId = second.composerId
        layer.resumeGuideDraft()
        XCTAssertEqual(layer.pendingTypeahead, "second")
    }

    func testNilSelectionProviderOverridesSelectedTextView() {
        let layer = CommentLayer()
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 200, height: 200),
                              styleMask: [.borderless], backing: .buffered, defer: false)
        let textView = NSTextView(frame: window.contentView!.bounds)
        textView.string = "selected text"
        window.contentView = textView
        window.makeFirstResponder(textView)
        textView.setSelectedRange(NSRange(location: 0, length: 8))
        layer.setHostWindow(window)
        layer.liveSelectionProvider = { nil }
        XCTAssertFalse(layer.hasCurrentSelection())
    }
}
