import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// Tests for the comment system. The layer is engine-backed;
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

    // MARK: - Popover show-animation dead window (keystroke buffer + focus)

    /// While the popover is showing but the text view is not yet first responder,
    /// typeable keystrokes must be consumed into the pending typeahead buffer —
    /// not dropped on the read-only document view. This is the chars-2..N half of
    /// the select-text-then-type race.
    func testKeystrokesBufferedWhilePopoverShowingWithoutTextViewFocus() {
        let layer = CommentLayer()
        let host = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 200),
            styleMask: [.borderless], backing: .buffered, defer: false)
        layer.setHostWindow(host)
        layer.isShowingPopover = true
        layer.needsCommentTextFocus = true
        layer.pendingTypeahead = "h"

        XCTAssertTrue(
            layer.shouldConsumeKeyEvent(chars: "e", mods: [], window: host),
            "second keystroke during dead window must be consumed")
        XCTAssertEqual(layer.pendingTypeahead, "he")

        XCTAssertTrue(layer.shouldConsumeKeyEvent(chars: "l", mods: [], window: host))
        XCTAssertTrue(layer.shouldConsumeKeyEvent(chars: "l", mods: [], window: host))
        XCTAssertTrue(layer.shouldConsumeKeyEvent(chars: "o", mods: [], window: host))
        XCTAssertEqual(layer.pendingTypeahead, "hello")

        // Space is preserved during the dead window (multi-word comments).
        XCTAssertTrue(layer.shouldConsumeKeyEvent(chars: " ", mods: [], window: host))
        XCTAssertEqual(layer.pendingTypeahead, "hello ")
    }

    /// Once the comment text view is first responder, the monitor must stop
    /// consuming so AppKit delivers keystrokes to the text view normally.
    func testKeyMonitorStopsConsumingWhenTextViewIsFirstResponder() {
        let layer = CommentLayer()
        let host = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 200),
            styleMask: [.borderless], backing: .buffered, defer: false)
        let textView = NSTextView(frame: NSRect(x: 0, y: 0, width: 100, height: 40))
        host.contentView = textView
        layer.setHostWindow(host)
        layer.isShowingPopover = true
        layer.setCommentTextView(textView)
        // Put the text view in a window and make it first responder so the
        // "already focused" branch of shouldConsumeKeyEvent fires.
        XCTAssertTrue(host.makeFirstResponder(textView))
        // claimCommentTextFocus should have cleared needs once first responder sticks.
        layer.claimCommentTextFocus()
        XCTAssertTrue(layer.isCommentTextViewFirstResponder)
        XCTAssertFalse(layer.needsCommentTextFocus)

        XCTAssertFalse(
            layer.shouldConsumeKeyEvent(chars: "x", mods: [], window: host),
            "monitor must not swallow keys once the form is first responder")
        XCTAssertEqual(layer.pendingTypeahead, "", "focused path must not append typeahead")
    }

    /// A responder claim in a non-key popover window must not stop forwarding
    /// host-window keys during the show animation.
    func testKeyMonitorForwardsHostEventsWhilePopoverWindowIsNotKey() {
        let layer = CommentLayer()
        let host = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 200),
            styleMask: [.borderless], backing: .buffered, defer: false)
        let popoverWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 200),
            styleMask: [.borderless], backing: .buffered, defer: false)
        let textView = NSTextView(frame: NSRect(x: 0, y: 0, width: 100, height: 40))
        textView.string = "h"
        textView.setSelectedRange(NSRange(location: 1, length: 0))
        popoverWindow.contentView = textView
        XCTAssertTrue(popoverWindow.makeFirstResponder(textView))

        layer.setHostWindow(host)
        layer.isShowingPopover = true
        layer.setCommentTextView(textView)

        XCTAssertFalse(popoverWindow.isKeyWindow)
        XCTAssertTrue(layer.isCommentTextViewFirstResponder)
        XCTAssertTrue(layer.shouldConsumeKeyEvent(chars: "e", mods: [], window: host))
        XCTAssertEqual(textView.string, "he")
    }

    /// Once the initial claim has landed, events in the form window must pass
    /// through even when a sibling control (such as Cancel) owns focus.
    func testKeyMonitorDoesNotStealFocusFromPopoverSibling() {
        let layer = CommentLayer()
        let host = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 200),
            styleMask: [.borderless], backing: .buffered, defer: false)
        let popoverWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 200),
            styleMask: [.borderless], backing: .buffered, defer: false)
        let textView = NSTextView(frame: NSRect(x: 0, y: 0, width: 100, height: 40))
        let buttonStandIn = NSView(frame: .zero)
        let container = NSView(frame: popoverWindow.contentView?.bounds ?? .zero)
        container.addSubview(textView)
        container.addSubview(buttonStandIn)
        popoverWindow.contentView = container
        XCTAssertTrue(popoverWindow.makeFirstResponder(buttonStandIn))

        layer.setHostWindow(host)
        layer.isShowingPopover = true
        layer.setCommentTextView(textView)
        XCTAssertTrue(popoverWindow.makeFirstResponder(buttonStandIn))
        layer.needsCommentTextFocus = false

        XCTAssertFalse(layer.shouldConsumeKeyEvent(chars: " ", mods: [], window: popoverWindow))
        XCTAssertEqual(textView.string, "")
        XCTAssertTrue(popoverWindow.firstResponder === buttonStandIn)
    }

    /// Direct insert into a live text view (no pendingTypeahead) when keys arrive
    /// after the view exists but before first responder sticks.
    func testForwardedKeystrokesInsertIntoLiveTextView() {
        let layer = CommentLayer()
        let host = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 200),
            styleMask: [.borderless], backing: .buffered, defer: false)
        let textView = NSTextView(frame: NSRect(x: 0, y: 0, width: 100, height: 40))
        textView.string = "h"
        textView.setSelectedRange(NSRange(location: 1, length: 0))
        // A sibling view holds first responder so the text view exists but is
        // not focused — the show-animation dead window.
        let decoy = NSView(frame: .zero)
        let container = NSView(frame: NSRect(x: 0, y: 0, width: 200, height: 200))
        container.addSubview(textView)
        container.addSubview(decoy)
        host.contentView = container
        host.makeFirstResponder(decoy)
        layer.setHostWindow(host)
        layer.isShowingPopover = true
        layer.needsCommentTextFocus = true
        // Register the text view without letting claimCommentTextFocus steal
        // first responder back (claim would make the dead-window case vanish).
        layer.setCommentTextView(textView)
        host.makeFirstResponder(decoy)
        layer.needsCommentTextFocus = true
        XCTAssertFalse(layer.isCommentTextViewFirstResponder)

        // Bypass shouldConsumeKeyEvent's claim-on-forward so we only assert
        // the insert path; call the buffer helper directly.
        XCTAssertTrue(layer.forwardKeystrokeToPendingComment(chars: "i", mods: []))
        XCTAssertEqual(textView.string, "hi")
        XCTAssertEqual(layer.pendingTypeahead, "", "live insert must not double-buffer into typeahead")
    }

    /// Events from a foreign window must not be buffered into this layer's form.
    func testKeystrokesFromOtherWindowNotBuffered() {
        let layer = CommentLayer()
        let host = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 100, height: 100),
            styleMask: [.borderless], backing: .buffered, defer: false)
        let other = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 100, height: 100),
            styleMask: [.borderless], backing: .buffered, defer: false)
        layer.setHostWindow(host)
        layer.isShowingPopover = true
        layer.pendingTypeahead = "a"

        XCTAssertFalse(layer.shouldConsumeKeyEvent(chars: "b", mods: [], window: other))
        XCTAssertEqual(layer.pendingTypeahead, "a")
    }

    func testForwardKeystrokeRejectsModifiedKeys() {
        let layer = CommentLayer()
        layer.isShowingPopover = true
        XCTAssertFalse(
            layer.forwardKeystrokeToPendingComment(chars: "a", mods: .command),
            "⌘A must not be swallowed into the comment buffer")
        XCTAssertEqual(layer.pendingTypeahead, "")
    }

    // MARK: - Window-scoped event handling (cross-window event bleed regression)

    /// Regression test: a markdown viewer's `CommentLayer` must judge "is there
    /// a selection?" against its *own* bound window, not the app-wide key window.
    /// `NSEvent` local monitors fire for events delivered to any window in the
    /// app, and `NSApp.keyWindow` can still be a stale, previously-frontmost
    /// window at the instant a monitor callback runs — checking the global key
    /// window let a leftover selection in one markdown viewer leak its
    /// right-click context menu / comment popup into a right-click that
    /// actually landed on a completely different window (e.g. the main kanban
    /// window).
    func testHasCurrentSelectionChecksOwnHostWindowNotAnyWindow() {
        let layer = CommentLayer()
        let selectingWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 100, height: 100),
            styleMask: [.borderless], backing: .buffered, defer: false)
        let selectingResponder = SelectableStubResponder(frame: .zero)
        selectingResponder.hasSelection = true
        selectingWindow.contentView = selectingResponder
        selectingWindow.makeFirstResponder(selectingResponder)

        // A distinct window whose own first responder has no selection — mirrors
        // a second markdown viewer (or the main kanban window) that a monitor
        // must not mistake for the layer's own selecting window.
        let otherWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 100, height: 100),
            styleMask: [.borderless], backing: .buffered, defer: false)
        let otherResponder = SelectableStubResponder(frame: .zero)
        otherResponder.hasSelection = false
        otherWindow.contentView = otherResponder
        otherWindow.makeFirstResponder(otherResponder)

        layer.setHostWindow(selectingWindow)
        XCTAssertTrue(layer.hasCurrentSelection(), "should see the selection in its own bound window")

        layer.setHostWindow(otherWindow)
        XCTAssertFalse(
            layer.hasCurrentSelection(),
            "a selection that lives in a different window must not count as this layer's own")
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
        layer.setIntent(.revision, for: layer.comments[0])
        XCTAssertEqual(layer.comments[0].intent, .revision)
        XCTAssertTrue(layer.comments[0].intentOverriddenByUser)
    }

    func testSetIntentIgnoresUnknownComment() {
        let layer = CommentLayer()
        layer.addComment(quoted: "some text", body: "a note")
        let stray = Comment(id: "stray", anchor: CommentAnchor(exact: "x"), body: "y", author: "user:me", createdAt: Date())
        layer.setIntent(.revision, for: stray)
        XCTAssertNil(layer.comments[0].intent)
    }

    // MARK: - Intent classification badge (engine-backed: real CommentsSetIntent RPC)

    func testSetIntentSendsRPCWhenEngineBackedAndDoesNotMutateLocally() {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(source: "x", baseURL: nil, artifact: .workItem(id: "t"), backend: backend)
        layer.applyList([Self.wireComment(id: "cmt_1", exact: "alpha", body: "one")])
        layer.setIntent(.revision, for: layer.comments[0])

        XCTAssertEqual(backend.setIntentCalls.count, 1)
        XCTAssertEqual(backend.setIntentCalls[0].commentId, "cmt_1")
        XCTAssertEqual(backend.setIntentCalls[0].intent, "revision")
        // No local mutation — the layer waits for the `comment_result` echo's reload.
        XCTAssertNil(layer.comments[0].intent)
        XCTAssertFalse(layer.comments[0].intentOverriddenByUser)
    }

    // MARK: - `[Revise]` banner + chips (artifact-less fallback, unchanged behaviour)

    /// A `revision`-classified, unclaimed comment renders no chip at all —
    /// the intent badge and the always-visible `[Revise]` button already say
    /// everything a chip would.
    func testRevisionClassificationMakesBannerRevisableWithNoChip() {
        let layer = CommentLayer()
        layer.addComment(quoted: "some text", body: "a note")
        layer.setIntent(.revision, for: layer.comments[0])
        XCTAssertTrue(layer.bannerState.revisable)
        XCTAssertTrue(layer.comments[0].threadEntries.isEmpty)
        XCTAssertNil(layer.comments[0].revisionChipState)
    }

    /// `Comment.from` with `reopenedAt` set and `status == .active` derives
    /// the `Reopened` chip; `reopenedAt == nil` derives no chip.
    func testRevisionChipStateReflectsReopenedAt() {
        let reopened = Self.wireComment(id: "cmt_1", exact: "a", body: "b", status: "active", reopenedAt: "2026-08-01T00:00:00Z")
        let notReopened = Self.wireComment(id: "cmt_2", exact: "a", body: "b", status: "active", reopenedAt: nil)

        let reopenedComment = Comment.from(reopened.comment, threadEntries: reopened.threadEntries)
        let notReopenedComment = Comment.from(notReopened.comment, threadEntries: notReopened.threadEntries)

        XCTAssertEqual(reopenedComment.revisionChipState, .reopened)
        XCTAssertNil(notReopenedComment.revisionChipState)
    }

    /// A wire thread carrying a retired `entry_kind: "nudge"` row is dropped
    /// by `Comment.from`; a recognized kind alongside it survives.
    func testCommentFromDropsNudgeThreadEntries() {
        let wire = Self.wireComment(
            id: "cmt_1", exact: "a", body: "b",
            threadEntries: [
                Self.wireThreadEntry(id: "cte_1", entryKind: "nudge", body: "legacy nudge"),
                Self.wireThreadEntry(id: "cte_2", entryKind: "answer", body: "a real answer"),
            ]
        )
        let comment = Comment.from(wire.comment, threadEntries: wire.threadEntries)
        XCTAssertEqual(comment.threadEntries.map(\.id), ["cte_2"])
    }

    func testReviseDocTransitionsMatchingCommentsToInRevision() {
        let layer = CommentLayer()
        layer.addComment(quoted: "a", body: "first")
        layer.setIntent(.revision, for: layer.comments[0])
        layer.reviseDoc()
        XCTAssertEqual(layer.comments[0].status, .inRevision)
        XCTAssertNotNil(layer.comments[0].reviseTaskId)
    }

    func testReviseDocWithNoUnresolvedCommentsIsNoOp() {
        let layer = CommentLayer()
        layer.addComment(quoted: "a", body: "first")
        // No comment has been classified `revision`, so there is
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
        layer.applyReviseDocOutcome(
            .created(taskId: "rev_1", taskKind: "revision", addressedCommentIds: ["cmt_1"], excludedCommentIds: [], prUrl: nil))
        XCTAssertNil(layer.reviseDocMessage)
    }

    /// A batch that drops comments the sidebar still badges `Revision`
    /// must say so — the badge renders `intent` alone, so a silent success
    /// toast is indistinguishable from "everything was addressed".
    func testApplyReviseDocOutcomeCreatedReportsExcludedComments() {
        let layer = CommentLayer()
        layer.applyReviseDocOutcome(
            .created(
                taskId: "rev_1", taskKind: "revision",
                addressedCommentIds: ["cmt_1", "cmt_2"], excludedCommentIds: ["cmt_3"], prUrl: nil))
        XCTAssertEqual(
            layer.reviseDocMessage,
            "Addressing 2 comments. 1 other comment was left out (already in a revision, still answering, or its anchor was lost).")
    }

    func testApplyReviseDocOutcomeCreatedPluralisesExcludedComments() {
        let layer = CommentLayer()
        layer.applyReviseDocOutcome(
            .created(
                taskId: "rev_1", taskKind: "revision",
                addressedCommentIds: ["cmt_1"], excludedCommentIds: ["cmt_2", "cmt_3"], prUrl: nil))
        XCTAssertEqual(
            layer.reviseDocMessage,
            "Addressing 1 comment. 2 other comments were left out (already in a revision, still answering, or their anchors were lost).")
    }

    /// The engine omits `excluded_comment_ids` entirely when nothing was
    /// dropped (`skip_serializing_if = "Vec::is_empty"`), so decoding must
    /// tolerate its absence rather than failing the whole reply.
    func testReviseDocOutcomeDecodesWithoutExcludedCommentIds() throws {
        let json = """
            {"type":"created","task_id":"rev_1","task_kind":"revision","addressed_comment_ids":["cmt_1"]}
            """
        let outcome = try JSONDecoder().decode(ReviseDocOutcome.self, from: Data(json.utf8))
        XCTAssertEqual(
            outcome,
            .created(taskId: "rev_1", taskKind: "revision", addressedCommentIds: ["cmt_1"], excludedCommentIds: [], prUrl: nil))
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

    func testSetIntentToQuestionLeavesAClaimedCommentInRevision() {
        let layer = CommentLayer()
        layer.addComment(quoted: "a", body: "first")
        layer.setIntent(.revision, for: layer.comments[0])
        layer.reviseDoc()
        XCTAssertEqual(layer.comments[0].status, .inRevision)

        // Mirrors the engine's `rehome_reclassified_comment`, which guards the
        // `question` spawn on `status == .active` so a claimed comment can't
        // spawn an answer agent behind the revision's back.
        layer.setIntent(.question, for: layer.comments[0])
        XCTAssertEqual(layer.comments[0].status, .inRevision)
        XCTAssertTrue(layer.comments[0].threadEntries.allSatisfy { $0.entryKind != .answer })
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
            Self.wireComment(id: "cmt_1", exact: "alpha", body: "one", status: "active", intent: "revision"),
            Self.wireComment(id: "cmt_2", exact: "beta", body: "two", status: "answering"),
        ])
        XCTAssertEqual(layer.comments.count, 2)
        XCTAssertEqual(layer.comments[0].id, "cmt_1")
        XCTAssertEqual(layer.comments[0].intent, .revision)
        XCTAssertEqual(layer.comments[1].status, .answering)
    }

    /// The engine writes `work_comments.created_at` as epoch seconds
    /// serialised to a string (e.g. "1753302417"), never ISO-8601 — see
    /// `now_string()` / `now_epoch_secs()` in `engine/core`. Every fixture
    /// above this point uses ISO or the literal "t"; none exercises the
    /// engine's actual shape. `Comment.parseWireTimestamp` must parse it,
    /// not silently fall back to "now".
    func testCommentFromParsesEngineEpochTimestamp() {
        let wc = Self.wireComment(id: "cmt_1", exact: "alpha", body: "one", createdAt: "1753302417").comment
        let comment = Comment.from(wc, threadEntries: [], answerAgentRunning: false, answerAgentFailed: false)
        XCTAssertEqual(comment.createdAt, Date(timeIntervalSince1970: 1_753_302_417))
    }

    /// A value matching neither the epoch nor ISO-8601 shape must fall back
    /// to `unparseableTimestampSentinel`, not silently render a confident
    /// wrong "now" — see `CommentAgeChip`, which renders nothing for it.
    func testParseWireTimestampFallsBackToSentinelForUnparseableValue() {
        XCTAssertEqual(Comment.parseWireTimestamp("not-a-timestamp"), Comment.unparseableTimestampSentinel)
    }

    /// Regression for the "every comment reads the same age" bug: two
    /// comments created seconds apart must keep distinct `createdAt` values
    /// across a second `applyList` (e.g. triggered by an unrelated reload),
    /// not both re-stamp to the reload instant.
    func testApplyListPreservesDistinctCreatedAtAcrossReload() {
        let layer = CommentLayer()
        let backend = FakeCommentBackend()
        layer.configure(source: "x", baseURL: nil, artifact: .workItem(id: "t"), backend: backend)
        let rows = [
            Self.wireComment(id: "cmt_1", exact: "alpha", body: "one", createdAt: "1753302000"),
            Self.wireComment(id: "cmt_2", exact: "beta", body: "two", createdAt: "1753302417"),
        ]
        layer.applyList(rows)
        let firstPass = Dictionary(uniqueKeysWithValues: layer.comments.map { ($0.id, $0.createdAt) })
        XCTAssertNotEqual(firstPass["cmt_1"], firstPass["cmt_2"])

        // Re-apply the identical wire rows, simulating a reload unrelated to
        // these two comments (self-echo, cross-session invalidation, etc.).
        layer.applyList(rows)
        let secondPass = Dictionary(uniqueKeysWithValues: layer.comments.map { ($0.id, $0.createdAt) })
        XCTAssertEqual(firstPass["cmt_1"], secondPass["cmt_1"])
        XCTAssertEqual(firstPass["cmt_2"], secondPass["cmt_2"])
        XCTAssertNotEqual(secondPass["cmt_1"], secondPass["cmt_2"])
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

    func testCommentSidebarRendersMarkdownThreadEntryBody() {
        let layer = CommentLayer()
        layer.addComment(quoted: "the quick brown fox", body: "This needs clarification.")
        layer.comments[0].threadEntries.append(
            CommentThreadEntry(
                id: "te_1",
                entryKind: .answer,
                author: "engine",
                body: "A **bold** claim with `inline code`.\n\n1. One\n2. Two\n\n```swift\nlet x = 1\n```",
                createdAt: Date()
            )
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
        answerAgentFailed: Bool = false,
        createdAt: String = "2026-07-04T12:00:00Z",
        reopenedAt: String? = nil,
        threadEntries: [WireCommentThreadEntry] = []
    ) -> CommentWithThread {
        CommentWithThread(
            comment: WorkComment(
                id: id,
                artifactId: "t",
                anchor: CommentAnchor(exact: exact),
                artifactKind: "work_item",
                author: "user:me",
                body: body,
                createdAt: createdAt,
                status: status,
                lastResolvedWith: lastResolvedWith,
                intent: intent,
                reopenedAt: reopenedAt
            ),
            threadEntries: threadEntries,
            answerAgentRunning: false,
            answerAgentFailed: answerAgentFailed
        )
    }

    /// Build a `WireCommentThreadEntry` for feeding `wireComment`'s
    /// `threadEntries` in tests.
    static func wireThreadEntry(id: String, entryKind: String, body: String = "x") -> WireCommentThreadEntry {
        WireCommentThreadEntry(
            id: id,
            commentId: "cmt_1",
            entryKind: entryKind,
            author: "engine",
            body: body,
            reviseTaskId: nil,
            answerAgentRunId: nil,
            createdAt: "2026-07-04T12:00:00Z"
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

/// Minimal `NSUserInterfaceValidations`-conforming responder that reports
/// "yes, Copy is valid" — a stand-in for a real text view with an active
/// selection, used to test `CommentLayer.hasCurrentSelection()`'s
/// responder-chain walk without depending on real `NSTextView` selection
/// behavior in a headless test run.
private final class SelectableStubResponder: NSView, NSUserInterfaceValidations {
    var hasSelection = false
    func validateUserInterfaceItem(_ item: NSValidatedUserInterfaceItem) -> Bool {
        item.action == #selector(NSText.copy(_:)) && hasSelection
    }
}
