import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// Tests for the comment system. Since P529 Phase 2 the layer is engine-backed;
/// these exercise both the in-memory fallback (bare `CommentLayer`) and the
/// engine path (a `FakeCommentBackend`), plus the W3C anchoring, the wire
/// Codable mirrors, and the SwiftUI layout of the sidebar/popover.
@MainActor
final class CommentLayerTests: XCTestCase {

    // MARK: - Comment model

    func testCommentModelEquality() {
        let date = Date()
        let a = Comment(id: "c1", anchor: CommentAnchor(exact: "hello"), body: "world", author: "user:me", createdAt: date)
        let b = Comment(id: "c1", anchor: CommentAnchor(exact: "hello"), body: "world", author: "user:me", createdAt: date)
        XCTAssertEqual(a, b)
    }

    func testCommentModelIdentityDiffersForDifferentIDs() {
        let date = Date()
        let a = Comment(id: "c1", anchor: CommentAnchor(exact: "x"), body: "y", author: "user:me", createdAt: date)
        let b = Comment(id: "c2", anchor: CommentAnchor(exact: "x"), body: "y", author: "user:me", createdAt: date)
        XCTAssertNotEqual(a, b)
    }

    func testCommentAnchorEqualityUsesAllThreeFields() {
        let a = CommentAnchor(exact: "foo", prefix: "pre ", suffix: " suf")
        let b = CommentAnchor(exact: "foo", prefix: "pre ", suffix: " suf")
        let c = CommentAnchor(exact: "foo", prefix: "different ", suffix: " suf")
        XCTAssertEqual(a, b)
        XCTAssertNotEqual(a, c)
    }

    func testCommentQuotedTextAliasesAnchorExact() {
        let c = Comment(id: "c1", anchor: CommentAnchor(exact: "rename", prefix: "please ", suffix: " it"), body: "note", author: "user:me", createdAt: Date())
        XCTAssertEqual(c.quotedText, "rename")
        XCTAssertEqual(c.anchor.exact, "rename")
        XCTAssertEqual(c.anchor.prefix, "please ")
        XCTAssertEqual(c.anchor.suffix, " it")
    }

    // MARK: - CommentLayer (in-memory fallback)

    func testAddCommentAppendsToArray() {
        let layer = CommentLayer()
        XCTAssertTrue(layer.comments.isEmpty)
        layer.addComment(quoted: "selected text", body: "my comment")
        XCTAssertEqual(layer.comments.count, 1)
        XCTAssertEqual(layer.comments[0].quotedText, "selected text")
        XCTAssertEqual(layer.comments[0].body, "my comment")
        XCTAssertFalse(layer.isEngineBacked)
    }

    func testAddCommentIgnoresBlankBody() {
        let layer = CommentLayer()
        layer.addComment(quoted: "some text", body: "   ")
        XCTAssertTrue(layer.comments.isEmpty)
    }

    func testAddCommentTrimsBodyWhitespace() {
        let layer = CommentLayer()
        layer.addComment(quoted: "", body: "  hello  ")
        XCTAssertEqual(layer.comments[0].body, "hello")
    }

    func testDismissRemovesCommentInMemory() {
        let layer = CommentLayer()
        layer.addComment(quoted: "a", body: "first")
        layer.addComment(quoted: "b", body: "second")
        let toRemove = layer.comments[0]
        layer.dismiss(toRemove)
        XCTAssertEqual(layer.comments.count, 1)
        XCTAssertEqual(layer.comments[0].body, "second")
    }

    func testAddCommentClosesPopoverAndClearsPending() {
        let layer = CommentLayer()
        layer.pendingQuotedText = "selection"
        layer.isShowingPopover = true
        layer.addComment(quoted: "selection", body: "note")
        XCTAssertFalse(layer.isShowingPopover)
        XCTAssertEqual(layer.pendingQuotedText, "")
    }

    // MARK: - W3C anchor capture

    func testCaptureAnchorSlicesPrefixAndSuffix() {
        let plain = "we ship the widget to prod every friday"
        let anchor = CommentLayer.captureAnchor(quoted: "widget", occurrenceIndex: 0, in: plain)
        XCTAssertEqual(anchor.exact, "widget")
        XCTAssertTrue(anchor.prefix.hasSuffix("we ship the "))
        XCTAssertTrue(anchor.suffix.hasPrefix(" to prod"))
    }

    func testCaptureAnchorDisambiguatesRepeatedTextByOccurrence() {
        let plain = "alpha beta alpha gamma"
        let first = CommentLayer.captureAnchor(quoted: "alpha", occurrenceIndex: 0, in: plain)
        let second = CommentLayer.captureAnchor(quoted: "alpha", occurrenceIndex: 1, in: plain)
        // Same exact, different surrounding context.
        XCTAssertEqual(first.exact, "alpha")
        XCTAssertEqual(second.exact, "alpha")
        XCTAssertTrue(first.suffix.hasPrefix(" beta"))
        XCTAssertTrue(second.prefix.hasSuffix("beta "))
    }

    func testCaptureAnchorFallsBackToBareExactWhenProjectionEmpty() {
        let anchor = CommentLayer.captureAnchor(quoted: "hello", occurrenceIndex: 0, in: "")
        XCTAssertEqual(anchor.exact, "hello")
        XCTAssertEqual(anchor.prefix, "")
        XCTAssertEqual(anchor.suffix, "")
    }

    func testCaptureAnchorFallsBackWhenOccurrenceOutOfRange() {
        let anchor = CommentLayer.captureAnchor(quoted: "alpha", occurrenceIndex: 9, in: "alpha beta")
        XCTAssertEqual(anchor.exact, "alpha")
        XCTAssertEqual(anchor.prefix, "")
    }

    // MARK: - W3C anchor capture — list-item text-space mismatch
    //
    // Regression coverage for a comment that orphaned within ~2s of creation
    // (`anchor_json` `{"exact":"  • binary","prefix":"","suffix":""}`, empty
    // context, on a doc whose markdown source has no literal "•" — it uses "-"
    // list syntax). Root cause: `captureCurrentSelection()` reads the selection
    // via a simulated "Copy", which for Textual's NSTextInteractionView
    // serialises the selected fragment through `Formatter.plainText()` — that
    // reconstructs the surrounding list item's block structure from
    // presentation intents and prepends a "• "/"<n>. " marker plus
    // two-space-per-level indentation to *every* line, even for a selection
    // that only covers part of the item. `CommentProjection.plainText` (the
    // engine-resolved projection) carries none of that decoration — it's a
    // bare `AttributedString.characters` flatten. So `quoted` never located in
    // `plain`, and `captureAnchor` silently fell back to a bare, contextless
    // anchor that the engine's very next `resolve_anchor` orphaned outright,
    // since that decorated text doesn't occur in the projection either.

    func testCaptureAnchorStripsListMarkerDecorationFromPastedSelection() {
        let plain = "Embedded (built-in) checks — compiled into the binary"
        // What a "Copy" of just the word "binary" inside a one-level-deep
        // unordered list item actually yields on the pasteboard.
        let quoted = "  • binary"
        let anchor = CommentLayer.captureAnchor(quoted: quoted, occurrenceIndex: 0, in: plain)
        XCTAssertEqual(anchor.exact, "binary")
        // The whole point of the fix: prefix/suffix context is now captured
        // instead of coming back empty.
        XCTAssertTrue(anchor.prefix.hasSuffix("compiled into the "))
        XCTAssertFalse(anchor.prefix.isEmpty)
    }

    func testCaptureAnchorStripsOrderedListMarkerDecoration() {
        let plain = "first step second step third step"
        let anchor = CommentLayer.captureAnchor(quoted: "  2. second step", occurrenceIndex: 0, in: plain)
        XCTAssertEqual(anchor.exact, "second step")
        XCTAssertTrue(anchor.prefix.hasSuffix("first step "))
    }

    func testCaptureAnchorPrefersVerbatimMatchOverDecorationStripping() {
        // A selection that happens to literally start with "• " in the
        // projection itself (not list decoration) must still match as-is —
        // stripping is only attempted once the verbatim match fails.
        let plain = "notes: • not a list marker here"
        let anchor = CommentLayer.captureAnchor(quoted: "• not a list marker here", occurrenceIndex: 0, in: plain)
        XCTAssertEqual(anchor.exact, "• not a list marker here")
    }

    func testStripCopyListDecorationIsNoOpForUndecoratedText() {
        XCTAssertEqual(CommentLayer.stripCopyListDecoration("plain text"), "plain text")
        XCTAssertEqual(CommentLayer.stripCopyListDecoration(""), "")
    }

    func testStripCopyListDecorationStripsPerLineForMultilineSelections() {
        let decorated = "  • line one\n  line two"
        XCTAssertEqual(CommentLayer.stripCopyListDecoration(decorated), "line one\nline two")
    }

    // MARK: - Projection + doc version

    func testDocVersionIsDeterministicAndVersionPrefixed() {
        let a = CommentProjection.docVersion(forPlainText: "the same text")
        let b = CommentProjection.docVersion(forPlainText: "the same text")
        let c = CommentProjection.docVersion(forPlainText: "different text")
        XCTAssertEqual(a, b)
        XCTAssertNotEqual(a, c)
        XCTAssertTrue(a.hasPrefix("sha256:"))
    }

    func testPlainTextProjectionStripsMarkdownMarkup() {
        let plain = CommentProjection.plainText(for: "# Heading\n\nSome **bold** text.")
        XCTAssertFalse(plain.contains("#"))
        XCTAssertFalse(plain.contains("**"))
        XCTAssertTrue(plain.contains("Heading"))
        XCTAssertTrue(plain.contains("bold"))
    }

    // MARK: - Intent classification badge (artifact-less fallback, unchanged behaviour)

    func testNewCommentHasNoIntentUntilClassified() {
        let layer = CommentLayer()
        layer.addComment(quoted: "some text", body: "a note")
        XCTAssertNil(layer.comments[0].intent)
        XCTAssertFalse(layer.comments[0].intentOverriddenByUser)
    }

    func testSetIntentUpdatesCommentAndMarksOverridden() {
        let layer = CommentLayer()
        layer.addComment(quoted: "some text", body: "a note")
        layer.setIntent(.directive, for: layer.comments[0])
        XCTAssertEqual(layer.comments[0].intent, .directive)
        XCTAssertTrue(layer.comments[0].intentOverriddenByUser)
    }

    func testSetIntentIgnoresUnknownComment() {
        let layer = CommentLayer()
        layer.addComment(quoted: "some text", body: "a note")
        let stray = Comment(id: "stray", anchor: CommentAnchor(exact: "x"), body: "y", author: "user:me", createdAt: Date())
        layer.setIntent(.directive, for: stray)
        XCTAssertNil(layer.comments[0].intent)
    }

    // MARK: - Intent classification badge (engine-backed: real CommentsSetIntent RPC)

    func testSetIntentSendsRPCWhenEngineBackedAndDoesNotMutateLocally() {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(source: "x", baseURL: nil, artifact: .workItem(id: "t"), backend: backend)
        layer.applyList([Self.wireComment(id: "cmt_1", exact: "alpha", body: "one")])
        layer.setIntent(.directive, for: layer.comments[0])

        XCTAssertEqual(backend.setIntentCalls.count, 1)
        XCTAssertEqual(backend.setIntentCalls[0].commentId, "cmt_1")
        XCTAssertEqual(backend.setIntentCalls[0].intent, "directive")
        // No local mutation — the layer waits for the `comment_result` echo's reload.
        XCTAssertNil(layer.comments[0].intent)
        XCTAssertFalse(layer.comments[0].intentOverriddenByUser)
    }

    // MARK: - `[Revise]` banner + chips (artifact-less fallback, unchanged behaviour)

    func testDirectiveClassificationPostsNudgeAndMakesBannerRevisable() {
        let layer = CommentLayer()
        layer.addComment(quoted: "some text", body: "a note")
        layer.setIntent(.directive, for: layer.comments[0])
        XCTAssertTrue(layer.bannerState.revisable)
        XCTAssertEqual(layer.comments[0].threadEntries.count, 1)
        XCTAssertEqual(layer.comments[0].threadEntries[0].entryKind, .nudge)
        XCTAssertEqual(layer.comments[0].revisionChipState, .nudged)
    }

    func testReviseDocTransitionsMatchingCommentsToInRevision() {
        let layer = CommentLayer()
        layer.addComment(quoted: "a", body: "first")
        layer.setIntent(.directive, for: layer.comments[0])
        layer.reviseDoc()
        XCTAssertEqual(layer.comments[0].status, .inRevision)
        XCTAssertNotNil(layer.comments[0].reviseTaskId)
    }

    func testReviseDocWithNoUnresolvedCommentsIsNoOp() {
        let layer = CommentLayer()
        layer.addComment(quoted: "a", body: "first")
        // No comment has been classified directive/larger_change, so there is
        // nothing to batch.
        layer.reviseDoc()
        XCTAssertEqual(layer.comments[0].status, .active)
        XCTAssertNil(layer.comments[0].reviseTaskId)
    }

    // MARK: - `[Revise]` banner + revise doc (engine-backed: real RPCs)

    func testReloadFetchesBannerState() {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(source: "x", baseURL: nil, artifact: .workItem(id: "t"), backend: backend)
        XCTAssertEqual(backend.fetchBannerStateCalls.count, 1)
        XCTAssertEqual(backend.fetchBannerStateCalls[0].kind, "work_item")
        XCTAssertEqual(backend.fetchBannerStateCalls[0].id, "t")
    }

    func testApplyBannerStateUpdatesPublishedBannerState() {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(source: "x", baseURL: nil, artifact: .workItem(id: "t"), backend: backend)
        layer.applyBannerState(CommentsBannerState(revisable: true, unresolvedCount: 2, inRevisionCount: 1))
        XCTAssertTrue(layer.bannerState.revisable)
        XCTAssertEqual(layer.bannerState.unresolvedCount, 2)
        XCTAssertEqual(layer.bannerState.inRevisionCount, 1)
    }

    func testReviseDocSendsRPCWhenEngineBacked() {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(
            source: "x", baseURL: nil,
            artifact: .prDoc(repoRemoteURL: "git@github.com:o/r.git", branch: "main", path: "d.md"),
            backend: backend
        )
        layer.reviseDoc()
        XCTAssertEqual(backend.reviseDocCalls.count, 1)
        XCTAssertEqual(backend.reviseDocCalls[0].kind, "pr_doc")
        XCTAssertEqual(backend.reviseDocCalls[0].id, "pr_doc:git@github.com:o/r.git:main:d.md")
    }

    func testApplyReviseDocOutcomeCreatedLeavesMessageNil() {
        let layer = CommentLayer()
        layer.applyReviseDocOutcome(.created(taskId: "rev_1", taskKind: "revision", addressedCommentIds: ["cmt_1"], prUrl: nil))
        XCTAssertNil(layer.reviseDocMessage)
    }

    func testApplyReviseDocOutcomeNoUnresolvedCommentsSetsMessage() {
        let layer = CommentLayer()
        layer.applyReviseDocOutcome(.noUnresolvedComments)
        XCTAssertEqual(layer.reviseDocMessage, "No unresolved comments to revise.")
    }

    func testApplyReviseDocOutcomeAlreadyInFlightIncludesRealTaskId() {
        let layer = CommentLayer()
        layer.applyReviseDocOutcome(.alreadyInFlight(taskId: "rev_42"))
        XCTAssertEqual(layer.reviseDocMessage, "Already being revised as rev_42.")
    }

    // MARK: - Bucket 2: answer agent (Phase 3d stub, unchanged behaviour)

    func testQuestionClassificationEntersAnsweringState() {
        let layer = CommentLayer()
        layer.addComment(quoted: "some text", body: "a note")
        layer.setIntent(.question, for: layer.comments[0])
        XCTAssertEqual(layer.comments[0].status, .answering)
    }

    func testAnswerAgentPostsAnswerAndTransitionsToAnswered() async throws {
        let layer = CommentLayer()
        layer.addComment(quoted: "some text", body: "a note")
        layer.setIntent(.question, for: layer.comments[0])
        try await Task.sleep(for: .seconds(2))
        XCTAssertEqual(layer.comments[0].status, .answered)
        XCTAssertEqual(layer.comments[0].threadEntries.last?.entryKind, .answer)
    }

    func testPostFollowupIgnoredBeforeAnswered() {
        let layer = CommentLayer()
        layer.addComment(quoted: "some text", body: "a note")
        // Comment is still `.active` — not yet `.answered` — so the follow-up
        // composer shouldn't be live.
        layer.postFollowup(body: "when will this ship?", for: layer.comments[0])
        XCTAssertEqual(layer.comments[0].status, .active)
        XCTAssertTrue(layer.comments[0].threadEntries.isEmpty)
    }

    func testPostFollowupAppendsEntryAndAwaitsReclassification() async throws {
        let layer = CommentLayer()
        layer.addComment(quoted: "some text", body: "a note")
        layer.setIntent(.question, for: layer.comments[0])
        try await Task.sleep(for: .seconds(2))
        XCTAssertEqual(layer.comments[0].status, .answered)

        layer.postFollowup(body: "but what about edge cases?", for: layer.comments[0])
        XCTAssertEqual(layer.comments[0].status, .awaitingFollowup)
        XCTAssertEqual(layer.comments[0].threadEntries.last?.entryKind, .operatorFollowup)
        XCTAssertEqual(layer.comments[0].threadEntries.last?.body, "but what about edge cases?")
    }

    func testPostFollowupSendsRPCWhenEngineBacked() async throws {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(source: "x", baseURL: nil, artifact: .workItem(id: "t"), backend: backend)
        layer.applyList([
            Self.wireComment(id: "cmt_1", exact: "some text", body: "a note", status: "answered")
        ])
        layer.postFollowup(body: "but what about edge cases?", for: layer.comments[0])
        XCTAssertEqual(backend.postFollowupCalls.count, 1)
        XCTAssertEqual(backend.postFollowupCalls[0].commentId, "cmt_1")
        XCTAssertEqual(backend.postFollowupCalls[0].body, "but what about edge cases?")
        // No local mutation — the engine-backed path waits for the reload
        // triggered by the topic invalidation the RPC's handler publishes.
        XCTAssertEqual(layer.comments[0].status, .answered)
    }

    // MARK: - Engine-backed path

    func testConfigureRegistersAndListsWhenArtifactPresent() {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(
            source: "the widget ships friday",
            baseURL: nil,
            artifact: .prDoc(repoRemoteURL: "git@github.com:o/r.git", branch: "main", path: "d.md"),
            backend: backend
        )
        XCTAssertTrue(layer.isEngineBacked)
        XCTAssertEqual(layer.artifactKind, "pr_doc")
        XCTAssertEqual(layer.artifactId, "pr_doc:git@github.com:o/r.git:main:d.md")
        XCTAssertEqual(backend.registerCount, 1)
        XCTAssertEqual(backend.listCalls.count, 1)
        // The layer also issues a resolve once it has a projection.
        XCTAssertEqual(backend.resolveCalls.count, 1)
    }

    func testEngineBackedAddCommentSendsCreateAndDoesNotAppendLocally() {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(
            source: "we ship the widget to prod",
            baseURL: nil,
            artifact: .workItem(id: "task_7"),
            backend: backend
        )
        layer.addComment(quoted: "widget", body: "clarify this")
        // Persisted through the engine, not appended optimistically.
        XCTAssertTrue(layer.comments.isEmpty)
        XCTAssertEqual(backend.createCalls.count, 1)
        let created = backend.createCalls[0]
        XCTAssertEqual(created.anchor.exact, "widget")
        XCTAssertTrue(created.anchor.prefix.hasSuffix("we ship the "))
        XCTAssertEqual(created.body, "clarify this")
        XCTAssertEqual(created.artifactKind, "work_item")
        XCTAssertEqual(created.artifactId, "task_7")
        XCTAssertTrue(created.docVersion.hasPrefix("sha256:"))
    }

    func testEngineBackedDismissSendsDismissRPC() {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(source: "hello world", baseURL: nil, artifact: .workItem(id: "task_7"), backend: backend)
        layer.applyList([Self.wireComment(id: "cmt_1", exact: "hello", body: "b")])
        XCTAssertEqual(layer.comments.count, 1)
        layer.dismiss(layer.comments[0])
        XCTAssertEqual(backend.dismissCalls, ["cmt_1"])
    }

    func testApplyListRebuildsCommentsFromEngineRows() {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(source: "x", baseURL: nil, artifact: .workItem(id: "t"), backend: backend)
        layer.applyList([
            Self.wireComment(id: "cmt_1", exact: "alpha", body: "one", status: "active", intent: "directive"),
            Self.wireComment(id: "cmt_2", exact: "beta", body: "two", status: "answering"),
        ])
        XCTAssertEqual(layer.comments.count, 2)
        XCTAssertEqual(layer.comments[0].id, "cmt_1")
        XCTAssertEqual(layer.comments[0].intent, .directive)
        XCTAssertEqual(layer.comments[1].status, .answering)
    }

    func testApplyResolvedStampsFuzzyAndOrphanGlyphs() {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(source: "x", baseURL: nil, artifact: .workItem(id: "t"), backend: backend)
        layer.applyList([
            Self.wireComment(id: "cmt_fuzzy", exact: "alpha", body: "one"),
            Self.wireComment(id: "cmt_orphan", exact: "beta", body: "two"),
        ])
        layer.applyResolved([
            ResolvedComment(
                comment: Self.wireComment(id: "cmt_fuzzy", exact: "alpha", body: "one").comment,
                resolution: CommentResolution(kind: "fuzzy", length: 5, score: 0.9, start: 3)
            ),
            ResolvedComment(
                comment: Self.wireComment(id: "cmt_orphan", exact: "beta", body: "two").comment,
                resolution: CommentResolution(kind: "orphan", length: nil, score: nil, start: nil)
            ),
        ])
        let fuzzy = layer.comments.first { $0.id == "cmt_fuzzy" }!
        let orphan = layer.comments.first { $0.id == "cmt_orphan" }!
        XCTAssertTrue(fuzzy.isFuzzyAnchored)
        XCTAssertFalse(fuzzy.isOrphaned)
        XCTAssertTrue(orphan.isOrphaned)
        XCTAssertFalse(orphan.isHighlightable)
    }

    func testShowResolvedToggleReListsWithIncludeResolved() {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(source: "x", baseURL: nil, artifact: .workItem(id: "t"), backend: backend)
        backend.listCalls.removeAll()
        layer.showResolved = true
        XCTAssertEqual(backend.listCalls.count, 1)
        XCTAssertEqual(backend.listCalls[0].includeResolved, true)
    }

    // MARK: - Wire Codable mirrors

    func testWorkCommentDecodesEngineJSONWithMissingOptionals() throws {
        let json = """
        {
          "id": "cmt_1",
          "artifact_id": "task_7",
          "anchor": { "exact": "the widget", "prefix": "we ship ", "suffix": " to prod" },
          "artifact_kind": "work_item",
          "author": "user:me@example.com",
          "body": "clarify",
          "created_at": "2026-07-04T12:00:00Z",
          "doc_version": "sha256:abc",
          "updated_at": "2026-07-04T12:00:00Z"
        }
        """
        let wc = try JSONDecoder().decode(WorkComment.self, from: Data(json.utf8))
        XCTAssertEqual(wc.id, "cmt_1")
        XCTAssertEqual(wc.anchor.exact, "the widget")
        XCTAssertEqual(wc.anchor.prefix, "we ship ")
        // Missing `status` defaults to active; missing projection version → 0.
        XCTAssertEqual(wc.status, "active")
        XCTAssertEqual(wc.plainTextProjectionVersion, 0)
        XCTAssertNil(wc.intent)
        XCTAssertNil(wc.reviseTaskId)
    }

    func testCommentWithThreadDecodesListElement() throws {
        let json = """
        {
          "comment": {
            "id": "cmt_1", "artifact_id": "task_7",
            "anchor": { "exact": "x" }, "artifact_kind": "work_item",
            "author": "user:me", "body": "b", "created_at": "t", "doc_version": "v",
            "status": "active", "updated_at": "t", "intent": "question"
          },
          "thread_entries": [
            { "id": "te_1", "comment_id": "cmt_1", "entry_kind": "answer",
              "author": "engine", "body": "the answer", "answer_agent_run_id": "aar_1",
              "created_at": "t2" }
          ],
          "answer_agent_running": true,
          "answer_agent_failed": false
        }
        """
        let cwt = try JSONDecoder().decode(CommentWithThread.self, from: Data(json.utf8))
        XCTAssertEqual(cwt.comment.intent, "question")
        XCTAssertEqual(cwt.threadEntries.count, 1)
        XCTAssertEqual(cwt.threadEntries[0].entryKind, "answer")
        XCTAssertTrue(cwt.answerAgentRunning)
        XCTAssertFalse(cwt.answerAgentFailed)
        // Maps into the UI comment with its thread entry preserved.
        let ui = Comment.from(cwt.comment, threadEntries: cwt.threadEntries)
        XCTAssertEqual(ui.intent, .question)
        XCTAssertEqual(ui.threadEntries.first?.entryKind, .answer)
        XCTAssertEqual(ui.threadEntries.first?.id, "te_1")
    }

    func testResolvedCommentDecodesResolution() throws {
        let json = """
        {
          "comment": {
            "id": "cmt_1", "artifact_id": "t", "anchor": { "exact": "x" },
            "artifact_kind": "work_item", "author": "a", "body": "b",
            "created_at": "t", "doc_version": "v", "status": "active", "updated_at": "t",
            "last_resolved_with": "fuzzy"
          },
          "resolution": { "kind": "fuzzy", "start": 3, "length": 5, "score": 0.87 }
        }
        """
        let rc = try JSONDecoder().decode(ResolvedComment.self, from: Data(json.utf8))
        XCTAssertEqual(rc.resolution.kind, "fuzzy")
        XCTAssertEqual(rc.resolution.start, 3)
        XCTAssertTrue(rc.resolution.isFuzzy)
        XCTAssertEqual(rc.comment.lastResolvedWith, "fuzzy")
    }

    func testCommentAnchorDecodesWithDefaultedPrefixSuffix() throws {
        let anchor = try JSONDecoder().decode(CommentAnchor.self, from: Data(#"{"exact":"only"}"#.utf8))
        XCTAssertEqual(anchor.exact, "only")
        XCTAssertEqual(anchor.prefix, "")
        XCTAssertEqual(anchor.suffix, "")
    }

    // MARK: - Bridge topic grammar

    func testBridgeTopicMatchesEngineGrammar() {
        XCTAssertEqual(
            CommentEngineBridge.topic(artifactKind: "work_item", artifactId: "task_7"),
            "comments.artifact.work_item:task_7"
        )
        XCTAssertTrue(CommentEngineBridge.isCommentTopic("comments.artifact.pr_doc:pr_doc:r:b:p.md"))
        XCTAssertFalse(CommentEngineBridge.isCommentTopic("work.product.p1"))
    }

    func testPrDocArtifactRefBuildsEngineCompositeId() {
        let ref = CommentArtifactRef.prDoc(
            repoRemoteURL: "git@github.com:o/r.git", branch: "boss/exec_x", path: "docs/foo.md")
        XCTAssertEqual(ref.kind, "pr_doc")
        XCTAssertEqual(ref.id, "pr_doc:git@github.com:o/r.git:boss/exec_x:docs/foo.md")
    }

    // MARK: - HighlightingMarkdownParser: W3C prefix/suffix resolution

    private func isHighlighted(at charOffset: Int, in result: AttributedString) -> Bool {
        let idx = result.characters.index(result.characters.startIndex, offsetBy: charOffset)
        return result.runs.contains { run in
            run.range.contains(idx) && run.swiftUI.backgroundColor != nil
        }
    }

    func testHighlightingParserHighlightsExactAnchor() throws {
        let source = "The fox jumped over the lazy dog and the cat sat quietly."
        let parser = HighlightingMarkdownParser(highlightedAnchors: [
            CommentAnchor(exact: "fox", prefix: "The ", suffix: " jumped"),
            CommentAnchor(exact: "cat", prefix: "the ", suffix: " sat"),
        ])
        let result = try parser.attributedString(for: source)
        let plain = String(result.characters)
        let foxOffset = plain.distance(from: plain.startIndex, to: plain.range(of: "fox")!.lowerBound)
        let catOffset = plain.distance(from: plain.startIndex, to: plain.range(of: "cat")!.lowerBound)
        XCTAssertTrue(isHighlighted(at: foxOffset, in: result))
        XCTAssertTrue(isHighlighted(at: catOffset, in: result))
    }

    func testHighlightingParserDisambiguatesRepeatedTextBySuffix() throws {
        let source = "alpha beta alpha gamma"
        // Anchor only the FIRST alpha via its trailing context.
        let parser = HighlightingMarkdownParser(
            highlightedAnchors: [CommentAnchor(exact: "alpha", prefix: "", suffix: " beta")]
        )
        let result = try parser.attributedString(for: source)
        let plain = String(result.characters)
        let firstRange = plain.range(of: "alpha")!
        let firstOffset = plain.distance(from: plain.startIndex, to: firstRange.lowerBound)
        let secondRange = plain.range(of: "alpha", range: firstRange.upperBound..<plain.endIndex)!
        let secondOffset = plain.distance(from: plain.startIndex, to: secondRange.lowerBound)
        XCTAssertTrue(isHighlighted(at: firstOffset, in: result), "First 'alpha' (suffix ' beta') must be highlighted")
        XCTAssertFalse(isHighlighted(at: secondOffset, in: result), "Second 'alpha' must not be highlighted")
    }

    func testHighlightingParserDisambiguatesRepeatedTextByPrefix() throws {
        let source = "alpha beta alpha gamma"
        let parser = HighlightingMarkdownParser(
            highlightedAnchors: [CommentAnchor(exact: "alpha", prefix: "beta ", suffix: " gamma")]
        )
        let result = try parser.attributedString(for: source)
        let plain = String(result.characters)
        let firstRange = plain.range(of: "alpha")!
        let firstOffset = plain.distance(from: plain.startIndex, to: firstRange.lowerBound)
        let secondRange = plain.range(of: "alpha", range: firstRange.upperBound..<plain.endIndex)!
        let secondOffset = plain.distance(from: plain.startIndex, to: secondRange.lowerBound)
        XCTAssertFalse(isHighlighted(at: firstOffset, in: result))
        XCTAssertTrue(isHighlighted(at: secondOffset, in: result), "Second 'alpha' (prefix 'beta ') must be highlighted")
    }

    func testHighlightingParserNoMatchIsSilentNoOp() throws {
        let source = "alpha beta gamma"
        let parser = HighlightingMarkdownParser(
            highlightedAnchors: [CommentAnchor(exact: "delta", prefix: "", suffix: "")]
        )
        let result = try parser.attributedString(for: source)
        let plain = String(result.characters)
        let alphaOffset = plain.distance(from: plain.startIndex, to: plain.range(of: "alpha")!.lowerBound)
        XCTAssertFalse(isHighlighted(at: alphaOffset, in: result))
    }

    func testResolveRangeReturnsNilWhenExactAbsent() {
        let range = HighlightingMarkdownParser.resolveRange(
            for: CommentAnchor(exact: "missing"), in: "alpha beta gamma")
        XCTAssertNil(range)
    }

    func testFlexibleMatchRangesToleratesWhitespaceRuns() {
        let plain = "the   quick\nbrown fox and the quick brown cat"
        let ranges = HighlightingMarkdownParser.flexibleMatchRanges(of: "quick brown", in: plain)
        XCTAssertEqual(ranges.count, 2)
    }

    func testHighlightingParserMatchesAcrossWhitespaceDifferences() throws {
        // Simulates a pasteboard selection where the copied text collapsed a
        // line break + leading spaces into a single space (a common outcome of
        // copying a multi-line selection out of the rendered view).
        let source = "the quick\n   brown fox jumps over the lazy dog"
        let parser = HighlightingMarkdownParser(
            highlightedAnchors: [CommentAnchor(exact: "quick brown fox", prefix: "the ", suffix: " jumps")]
        )
        let result = try parser.attributedString(for: source)
        let plain = String(result.characters)
        let offset = plain.distance(from: plain.startIndex, to: plain.range(of: "quick")!.lowerBound)
        XCTAssertTrue(isHighlighted(at: offset, in: result))
    }

    func testHighlightingParserUnderlinesInlineCodeAnchor() throws {
        // Regression guard for the "clobber-proof underline" marker: inline-code
        // runs get their own backgroundColor from the Boss inline style, which
        // overwrites a plain comment-highlight background — the colored
        // underline is the fallback that survives that clobber.
        let source = "Please rename `flavor` to `variant` everywhere."
        let parser = HighlightingMarkdownParser(
            highlightedAnchors: [CommentAnchor(exact: "flavor", prefix: "rename `", suffix: "` to")]
        )
        let result = try parser.attributedString(for: source)
        let plain = String(result.characters)
        let idx = plain.range(of: "flavor")!.lowerBound
        let charIdx = result.characters.index(result.characters.startIndex, offsetBy: plain.distance(from: plain.startIndex, to: idx))
        let hasUnderline = result.runs.contains { run in
            run.range.contains(charIdx) && run.swiftUI.underlineStyle != nil
        }
        XCTAssertTrue(hasUnderline, "Inline-code anchor must carry the fallback underline marker")
    }

    // MARK: - SwiftUI layout (unchanged surfaces still render)

    func testCommentSidebarRendersWithComment() {
        let layer = CommentLayer()
        layer.addComment(quoted: "the quick brown fox", body: "This needs clarification.")
        let hosting = NSHostingView(rootView: CommentSidebar(layer: layer))
        hosting.frame = NSRect(x: 0, y: 0, width: 280, height: 600)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
    }

    func testCommentSidebarRendersMarkdownReplyBody() {
        let layer = CommentLayer()
        layer.addComment(
            quoted: "the quick brown fox",
            body: "A **bold** claim with `inline code`.\n\n1. One\n2. Two\n\n```swift\nlet x = 1\n```"
        )
        let hosting = NSHostingView(rootView: CommentSidebar(layer: layer))
        hosting.frame = NSRect(x: 0, y: 0, width: 280, height: 600)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
    }

    func testCommentSidebarRendersFuzzyAndOrphanBadges() {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(source: "alpha beta", baseURL: nil, artifact: .workItem(id: "t"), backend: backend)
        layer.applyList([Self.wireComment(id: "c1", exact: "alpha", body: "one", lastResolvedWith: "fuzzy")])
        let hosting = NSHostingView(rootView: CommentSidebar(layer: layer))
        hosting.frame = NSRect(x: 0, y: 0, width: 280, height: 600)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
    }

    func testCommentPopoverRenders() {
        let layer = CommentLayer()
        layer.pendingQuotedText = "the selected markdown span"
        let hosting = NSHostingView(rootView: CommentPopover(layer: layer))
        hosting.frame = NSRect(x: 0, y: 0, width: 400, height: 400)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
    }

    func testMarkdownViewerWithCommentsRendersWhenEmpty() {
        let view = MarkdownViewerView(title: "Test Doc", source: "# Hello\n\nSome content.")
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 760, height: 640)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
    }

    // MARK: - Helpers

    /// Build a `CommentWithThread` for feeding `applyList` in tests.
    static func wireComment(
        id: String,
        exact: String,
        body: String,
        status: String = "active",
        intent: String? = nil,
        lastResolvedWith: String? = nil,
        answerAgentFailed: Bool = false
    ) -> CommentWithThread {
        CommentWithThread(
            comment: WorkComment(
                id: id,
                artifactId: "t",
                anchor: CommentAnchor(exact: exact),
                artifactKind: "work_item",
                author: "user:me",
                body: body,
                createdAt: "2026-07-04T12:00:00Z",
                status: status,
                lastResolvedWith: lastResolvedWith,
                intent: intent
            ),
            threadEntries: [],
            answerAgentRunning: false,
            answerAgentFailed: answerAgentFailed
        )
    }
}

/// Records the mutations a `CommentLayer` issues so tests can assert the RPC
/// surface without a live engine.
@MainActor
final class FakeCommentBackend: CommentBackend {
    let author = "user:test"

    var registerCount = 0
    var unregisterCount = 0
    var listCalls: [(kind: String, id: String, includeResolved: Bool)] = []
    var resolveCalls: [(kind: String, id: String, plainText: String)] = []
    var createCalls: [(artifactKind: String, artifactId: String, anchor: CommentAnchor, body: String, docVersion: String)] = []
    var dismissCalls: [String] = []
    var setStatusCalls: [(commentId: String, status: String)] = []
    var updateAnchorCalls: [(commentId: String, anchor: CommentAnchor)] = []
    var setIntentCalls: [(commentId: String, intent: String)] = []
    var fetchBannerStateCalls: [(kind: String, id: String)] = []
    var reviseDocCalls: [(kind: String, id: String)] = []
    var postFollowupCalls: [(commentId: String, body: String)] = []

    func registerCommentLayer(_ layer: CommentLayer, artifactKind: String, artifactId: String) {
        registerCount += 1
    }
    func unregisterCommentLayer(_ layer: CommentLayer) { unregisterCount += 1 }
    func createComment(artifactKind: String, artifactId: String, anchor: CommentAnchor, body: String, docVersion: String) {
        createCalls.append((artifactKind, artifactId, anchor, body, docVersion))
    }
    func listComments(artifactKind: String, artifactId: String, includeResolved: Bool) {
        listCalls.append((artifactKind, artifactId, includeResolved))
    }
    func resolveComments(artifactKind: String, artifactId: String, plainText: String) {
        resolveCalls.append((artifactKind, artifactId, plainText))
    }
    func dismissComment(commentId: String) { dismissCalls.append(commentId) }
    func setStatus(commentId: String, status: String) { setStatusCalls.append((commentId, status)) }
    func updateAnchor(commentId: String, anchor: CommentAnchor, newDocVersion: String) {
        updateAnchorCalls.append((commentId, anchor))
    }
    func setIntent(commentId: String, intent: String) {
        setIntentCalls.append((commentId, intent))
    }
    func fetchBannerState(artifactKind: String, artifactId: String) {
        fetchBannerStateCalls.append((artifactKind, artifactId))
    }
    func reviseDoc(artifactKind: String, artifactId: String) {
        reviseDocCalls.append((artifactKind, artifactId))
    }
    func postFollowup(commentId: String, body: String) {
        postFollowupCalls.append((commentId, body))
    }
}
