import AppKit
import SwiftUI
import XCTest

@testable import Boss

/// Covers the reconciled markdown chrome that File ▸ Open, the kanban
/// "Read full description", the async design-doc viewer, and the Designs-tab
/// reader all render through, plus the collapsed-by-default comment rail's
/// expand-reachability rule.
///
/// These are the chrome's only guardrails: no test instantiates the individual
/// viewers with color/background assertions, so the hosting tests below are the
/// canary that the merged view still builds and lays out, and the unit tests pin
/// the one piece of branch logic (does the collapsed rail always let you reach
/// resolved comments?).
@MainActor
final class MarkdownDocumentChromeTests: XCTestCase {
    private static let representative = """
    # Incident review

    Some intro paragraph with **bold**, *italic*, `inline code`, and a
    [link](https://example.com) for the document treatment.

    ## Context

    This heading carries the document rule that separates the following prose.

    ### Findings

    #### Evidence

    ##### Supporting detail

    ###### Footnote

    ```swift
    struct Greeter {
        let name: String
    }
    ```

    | Observed at | Build ID | Outcome |
    | ----------- | -------- | ------- |
    | 2026-08-18 15:42:00 | run-abc123 | completed |

    > This callout preserves a warning-colored rail and a readable body.

    - top level
      - nested one
    - another top

    1. first ordered item
    2. second ordered item

    ---
    """

    // MARK: - Editorial opt-in

    /// The editorial treatment is opt-in. The environment default must stay
    /// false so transcript bubbles, comment cards, release notes, and the
    /// find bar keep compact system-font markdown unless a caller sets the
    /// key (only the document-column chunk ForEach does).
    func testEditorialStyleDefaultsOff() {
        XCTAssertFalse(EnvironmentValues().markdownEditorialStyle)
    }

    func testEditorialTypographyAndCollapsedHeadingMetrics() {
        XCTAssertEqual(
            MarkdownEditorialMetrics.headingScales[0], 3.08, accuracy: 0.0001)
        XCTAssertEqual(
            MarkdownEditorialMetrics.codeBlockScale, 0.9184, accuracy: 0.0001)
        XCTAssertEqual(MarkdownEditorialMetrics.compactHeadingSpacing.top, 16)
        XCTAssertEqual(MarkdownEditorialMetrics.compactHeadingSpacing.bottom, 8)
        XCTAssertEqual(BossHeadingStyle.bodyPointSize * BossHeadingStyle.compactFontScales[1], 22, accuracy: 0.0001)
        XCTAssertEqual(BossHeadingStyle.bodyPointSize * MarkdownEditorialMetrics.headingScales[1], 24.752, accuracy: 0.0001)
        XCTAssertGreaterThan(MarkdownEditorialMetrics.headingScales[1], BossHeadingStyle.compactFontScales[1])
        XCTAssertEqual(MarkdownEditorialMetrics.editorialH2Spacing.top, 3)
        XCTAssertEqual(MarkdownEditorialMetrics.editorialH2Spacing.bottom, 0.75)
    }

    // MARK: - Document measure selection

    /// A wide table gets the extra document width; the delimiter row (e.g.
    /// `| -------- | -------- |`) is what's detected, not a bare `|`, which
    /// also shows up in prose and inline code without forming a table.
    func testMeasureWidensForDocumentsContainingATable() {
        XCTAssertEqual(
            MarkdownDocumentMeasure.forSource(Self.representative),
            MarkdownDocumentMeasure.wide)
        XCTAssertTrue(MarkdownDocumentMeasure.containsTable(Self.representative))
    }

    /// Prose-only documents keep today's centered reading column.
    func testMeasureStaysReadableForProseOnlyDocuments() {
        let prose = """
        # Title

        A paragraph with **bold** text and a | pipe | that is not a table.

        - a list item
        - another item

        > a blockquote
        """
        XCTAssertEqual(MarkdownDocumentMeasure.forSource(prose), MarkdownDocumentMeasure.readable)
        XCTAssertFalse(MarkdownDocumentMeasure.containsTable(prose))
    }

    /// A thematic break (`---`) alone must not be mistaken for a one-column
    /// table delimiter row.
    func testThematicBreakAloneIsNotATable() {
        let prose = """
        Some prose.

        ---

        More prose.
        """
        XCTAssertFalse(MarkdownDocumentMeasure.containsTable(prose))
    }

    /// GFM only requires one-or-more dashes per cell, not the three this
    /// detector used to require — `|-|-|` is common generator output and is
    /// a real table.
    func testMinimalSingleDashDelimiterRowIsATable() {
        let doc = """
        | A | B |
        |-|-|
        | one | two |
        """
        XCTAssertTrue(MarkdownDocumentMeasure.containsTable(doc))
    }

    /// A legal single-column GFM table must be detected too.
    func testSingleColumnTableIsATable() {
        let doc = """
        | A |
        | --- |
        | one |
        """
        XCTAssertTrue(MarkdownDocumentMeasure.containsTable(doc))
    }

    /// Table syntax shown as an example inside a fenced code block (common
    /// in this repo's own docs) must not widen a prose-only document.
    func testTableSyntaxInsideFencedCodeBlockIsNotATable() {
        let doc = """
        Some prose describing table syntax:

        ```
        | A | B |
        |---|---|
        | one | two |
        ```

        More prose.
        """
        XCTAssertFalse(MarkdownDocumentMeasure.containsTable(doc))
    }

    // MARK: - Collapsed rail expand-reachability rule

    /// An engine-backed doc must always offer the expand button, even at zero
    /// listed comments: its comments may all be resolved (and thus filtered out
    /// of the count) yet still be reachable only by expanding to the "Show
    /// resolved" toggle. This is the trap the collapse-by-default change had to
    /// avoid.
    func testEngineBackedRailAlwaysOffersExpand() {
        XCTAssertTrue(
            CollapsedCommentRail.shouldOfferExpand(commentCount: 0, isEngineBacked: true))
        XCTAssertTrue(
            CollapsedCommentRail.shouldOfferExpand(commentCount: 3, isEngineBacked: true))
    }

    /// An in-memory (artifact-less) doc has no resolved comments to hide, so the
    /// expand button appears only once there is at least one comment to reveal.
    func testInMemoryRailOffersExpandOnlyWithComments() {
        XCTAssertFalse(
            CollapsedCommentRail.shouldOfferExpand(commentCount: 0, isEngineBacked: false))
        XCTAssertTrue(
            CollapsedCommentRail.shouldOfferExpand(commentCount: 1, isEngineBacked: false))
    }

    // MARK: - Hosting

    func testStringBackedChromeRenders() {
        let view = MarkdownDocumentChrome(title: "Read full description", source: Self.representative)
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 760, height: 640)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
        XCTAssertGreaterThan(hosting.fittingSize.width, 0)
    }

    func testDiskStyleChromeWithRichHeaderRenders() {
        let view = MarkdownDocumentChrome(
            title: "Design doc",
            repoLabel: "spinyfin/mono",
            subtitle: "/workspaces/mono/docs/design.md",
            webURL: "https://github.com/spinyfin/mono/blob/main/docs/design.md",
            source: Self.representative,
            baseURL: URL(fileURLWithPath: "/workspaces/mono/docs/")
        )
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 880, height: 700)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
    }

    func testErrorStateRenders() {
        let view = MarkdownDocumentChrome(
            title: "Broken",
            webURL: "https://github.com/spinyfin/mono/blob/main/docs/design.md",
            source: "",
            loadError: "Failed to read /nope.md: no such file"
        )
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 760, height: 640)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThanOrEqual(hosting.fittingSize.height, 0)
    }

    func testCommentsDisabledChromeRenders() {
        let view = MarkdownDocumentChrome(
            title: "Designs tab doc",
            subtitle: "docs/x.md @ abc1234",
            source: Self.representative,
            commentsEnabled: false
        )
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 760, height: 640)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
    }

    func testCollapsedRailRenders() {
        let view = CollapsedCommentRail(
            commentCount: 2,
            isEngineBacked: true,
            onExpand: {},
            onAddComment: {}
        )
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 36, height: 600)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
    }

    // MARK: - Collapsible sections

    /// A revision brief's exact shape: a summary paragraph, the
    /// `## HARD RULE ...` boilerplate heading, then findings rendered as
    /// `### [severity] ...` headings with no intervening heading. The
    /// literal text this test builds mirrors `render_revision_instructions`
    /// in `tools/boss/engine/pr-review/src/render.rs`.
    private static let revisionBrief = """
    Automated PR review of PR #5 found 1 finding(s) requiring attention.
    Address ALL findings before finalising this revision.

    ## HARD RULE: no punting — do the actual work

    Each finding below requires a real code change that resolves it.
    The following are FORBIDDEN — they do NOT count as addressing a finding:

    - Filing a follow-up task, chore, or issue in lieu of fixing the finding.

    ### [high] Null pointer dereference

    **File:** `src/main.rs`

    The handle function dereferences without a null check; add a guard.

    **Review summary:** One bug found.
    """

    /// Must stay byte-for-byte identical to the heading text
    /// `render_revision_instructions` emits (after stripping `## `) — see
    /// that function's doc comment in `render.rs` for the matching half of
    /// this cross-language contract.
    func testRevisionBriefHardRuleHeadingTextIsPinned() {
        XCTAssertEqual(
            RevisionBriefCollapsibleHeadings.hardRule,
            "HARD RULE: no punting — do the actual work"
        )
    }

    /// An empty collapsible set still splits on every heading so each
    /// section is its own `StructuredText`. Concatenating the `.plain`
    /// chunks must reconstruct the source exactly.
    func testHeadingSectionsSplitsOnHeadingsEvenWithoutCollapsibleSet() {
        let chunks = MarkdownHeadingSections.chunks(in: Self.revisionBrief, collapsibleHeadings: [])
        XCTAssertEqual(chunks.count, 3, "prefix + HARD RULE heading + finding heading: \(chunks)")
        XCTAssertTrue(chunks.allSatisfy { if case .plain = $0 { return true }; return false })
        XCTAssertEqual(chunks.map(\.renderedText).joined(), Self.revisionBrief)
        guard case .plain(let hardRule) = chunks[1] else {
            return XCTFail("expected the HARD RULE heading as a .plain chunk")
        }
        XCTAssertTrue(hardRule.hasPrefix("## HARD RULE"))
        guard case .plain(let finding) = chunks[2] else {
            return XCTFail("expected the finding heading as a .plain chunk")
        }
        XCTAssertTrue(finding.contains("### [high]"))
    }

    /// A heading set that names no heading actually present in the source
    /// still splits on real headings; it just never produces a `.collapsible`
    /// chunk.
    func testHeadingSectionsWithNonMatchingHeadingStillSplitsOnRealHeadings() {
        let chunks = MarkdownHeadingSections.chunks(in: Self.revisionBrief, collapsibleHeadings: ["Not present anywhere"])
        XCTAssertEqual(chunks.count, 3)
        XCTAssertTrue(chunks.allSatisfy { if case .plain = $0 { return true }; return false })
    }

    /// A document with no ATX headings stays a single `.plain` chunk.
    func testHeadingSectionsWithNoHeadingsIsSinglePlainChunk() {
        let source = "Just a paragraph.\n\nAnd another.\n"
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: [])
        XCTAssertEqual(chunks, [.plain(source)])
    }

    /// The core invariant: the `## HARD RULE ...` section folds, but the
    /// findings after it (rendered as a *deeper* `###` heading, with no H2
    /// in between) never end up inside the collapsible chunk. A same-level
    /// boundary rule would get this wrong — this pins "next heading of any
    /// level" as the actual behavior.
    func testHeadingSectionsFoldsHardRuleButNeverFindings() {
        let chunks = MarkdownHeadingSections.chunks(
            in: Self.revisionBrief,
            collapsibleHeadings: [RevisionBriefCollapsibleHeadings.hardRule]
        )
        XCTAssertEqual(chunks.count, 3, "expected prefix, collapsible section, suffix: \(chunks)")

        guard case .plain(let prefix) = chunks[0] else { return XCTFail("expected a leading .plain chunk") }
        XCTAssertTrue(prefix.contains("Automated PR review of PR #5"))
        XCTAssertFalse(prefix.contains("HARD RULE"))

        guard case .collapsible(let heading, let body) = chunks[1] else {
            return XCTFail("expected a .collapsible chunk")
        }
        XCTAssertEqual(heading, RevisionBriefCollapsibleHeadings.hardRule)
        XCTAssertTrue(body.contains("FORBIDDEN"))
        XCTAssertFalse(body.contains("### [high]"), "a finding heading must never be folded into the collapsed body")
        XCTAssertFalse(body.contains("Review summary"))

        guard case .plain(let suffix) = chunks[2] else { return XCTFail("expected a trailing .plain chunk") }
        XCTAssertTrue(suffix.contains("### [high] Null pointer dereference"))
        XCTAssertTrue(suffix.contains("Review summary"))
        XCTAssertFalse(suffix.contains("FORBIDDEN"))
    }

    /// Concatenating the three chunks (re-adding back the `## ` marker and
    /// heading-line newline the collapsible chunk strips) must reconstruct
    /// the original source exactly — chunking must never drop or duplicate
    /// text, since this same text is what the app shows the reader.
    func testHeadingSectionsChunksReassembleToOriginalSource() {
        let chunks = MarkdownHeadingSections.chunks(
            in: Self.revisionBrief,
            collapsibleHeadings: [RevisionBriefCollapsibleHeadings.hardRule]
        )
        var reassembled = ""
        for chunk in chunks {
            switch chunk {
            case .plain(let text):
                reassembled += text
            case .collapsible(let heading, let body):
                reassembled += "## \(heading)\n\(body)"
            }
        }
        XCTAssertEqual(reassembled, Self.revisionBrief)
    }

    func testChromeWithCollapsedByDefaultHeadingRenders() {
        let view = MarkdownDocumentChrome(
            title: "Revision brief",
            source: Self.revisionBrief,
            collapsedByDefaultHeadings: [RevisionBriefCollapsibleHeadings.hardRule]
        )
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 760, height: 640)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
        XCTAssertGreaterThan(hosting.fittingSize.width, 0)
    }

    /// A '#' line inside a fenced code block must not be mistaken for a
    /// heading/section boundary, matching the fence-aware convention
    /// `MarkdownDocumentMeasure.containsTable` already establishes in this
    /// same file.
    func testHeadingLinesIgnoreHeadingsInsideFencedCodeBlocks() {
        let doc = """
        # Title

        ```
        # not a heading
        ```

        ## Real heading

        body
        """
        let chunks = MarkdownHeadingSections.chunks(in: doc, collapsibleHeadings: ["not a heading"])
        XCTAssertFalse(
            chunks.contains(where: {
                if case .collapsible(let heading, _) = $0 { return heading == "not a heading" }
                return false
            }),
            "a '#' line inside a fence must not be treated as a heading/section boundary"
        )
        XCTAssertEqual(chunks.map(\.renderedText).joined(), doc)
        XCTAssertEqual(chunks.count, 2, "expected Title + Real heading chunks, not a split on the fenced '#': \(chunks)")
    }

    /// CommonMark permits up to three leading spaces before an ATX heading's
    /// `#` markers.
    func testHeadingTextRecognizesUpToThreeLeadingSpaces() {
        let doc = """
        Intro

           ## Indented heading

        body

        ### Next
        """
        let chunks = MarkdownHeadingSections.chunks(in: doc, collapsibleHeadings: ["Indented heading"])
        XCTAssertEqual(chunks.count, 3, "expected prefix, collapsible section, suffix: \(chunks)")
        guard case .collapsible(let heading, _) = chunks[1] else { return XCTFail("expected a .collapsible chunk") }
        XCTAssertEqual(heading, "Indented heading")
    }

    // MARK: - Find-in-document over a chunked document

    /// The invariant the index-shift bug violated: the sum of per-chunk
    /// match counts must equal `MarkdownFindState`'s global `matches.count`
    /// for the same source, and every global index must map to exactly one
    /// in-range local index across the chunks. Before chunk-aware search,
    /// `matches` was computed once over the whole source — including the
    /// collapsible heading's own text, which no chunk's parser ever renders —
    /// so the global index space and the sum of per-chunk match counts could
    /// disagree.
    @MainActor
    func testFindStateEveryGlobalMatchIndexMapsToExactlyOneChunkLocalIndex() {
        let state = MarkdownFindState()
        let heading = RevisionBriefCollapsibleHeadings.hardRule
        state.updateSource(Self.revisionBrief, baseURL: nil, collapsibleHeadings: [heading])
        state.query = "the"

        let chunkCount = MarkdownHeadingSections.chunks(in: Self.revisionBrief, collapsibleHeadings: [heading]).count
        XCTAssertGreaterThan(
            state.matches.count, 1,
            "fixture must have more than one match, spanning more than one chunk, to exercise the mapping")

        for global in 0..<state.matches.count {
            if global > 0 { state.selectNext() }
            XCTAssertEqual(state.currentIndex, global)

            var chunksReportingCurrent: [Int] = []
            for index in 0..<chunkCount {
                let (matches, currentLocal) = state.chunkMatches(index)
                if let currentLocal {
                    XCTAssertTrue(matches.indices.contains(currentLocal), "chunk \(index) reported an out-of-range local index")
                    chunksReportingCurrent.append(index)
                }
            }
            XCTAssertEqual(
                chunksReportingCurrent.count, 1,
                "global match \(global) must map to exactly one chunk's local index, got \(chunksReportingCurrent)")
        }

        let summedLocalMatches = (0..<chunkCount).reduce(0) { $0 + state.chunkMatches($1).matches.count }
        XCTAssertEqual(summedLocalMatches, state.matches.count)
    }

    /// A match inside a still-collapsed section must be identified so the
    /// viewer can auto-expand it — otherwise the find bar counts a hit whose
    /// highlight is never in the view hierarchy to paint.
    @MainActor
    func testFindStateIdentifiesCollapsedChunkForAutoExpand() {
        let state = MarkdownFindState()
        let heading = RevisionBriefCollapsibleHeadings.hardRule
        state.updateSource(Self.revisionBrief, baseURL: nil, collapsibleHeadings: [heading])
        state.query = "FORBIDDEN"
        XCTAssertEqual(state.matches.count, 1)
        XCTAssertEqual(state.currentCollapsibleHeadingToExpand, heading)
    }

    /// A match outside any collapsible chunk must not report a heading to
    /// expand.
    @MainActor
    func testFindStateReportsNoHeadingToExpandForAPlainChunkMatch() {
        let state = MarkdownFindState()
        let heading = RevisionBriefCollapsibleHeadings.hardRule
        state.updateSource(Self.revisionBrief, baseURL: nil, collapsibleHeadings: [heading])
        state.query = "Null pointer"
        XCTAssertEqual(state.matches.count, 1)
        XCTAssertNil(state.currentCollapsibleHeadingToExpand)
    }

    // MARK: - Highlight refresh (no .id() remount)

    func testHighlightRefreshTagIsStrippedBeforeParse() {
        let source = "# Title\n\nHello."
        XCTAssertEqual(MarkdownHighlightRefreshParser.stripNonce(from: source), source)
        let tagged = MarkdownHighlightRefreshParser.tagged(source, generation: 7)
        XCTAssertNotEqual(tagged, source)
        XCTAssertEqual(MarkdownHighlightRefreshParser.stripNonce(from: tagged), source)
        XCTAssertEqual(MarkdownHighlightRefreshParser.tagged(source, generation: 0), source)
    }

    // MARK: - Comment anchors across heading chunks

    /// An `exact` that recurs in two heading sections must paint once in the
    /// whole document — the occurrence disambiguated by prefix/suffix —
    /// not once per chunk that happens to contain the same words.
    func testCommentAnchorRecurringExactHighlightsOnceAcrossHeadingChunks() throws {
        let source = """
        # Alpha

        run checkleft run here please

        # Beta

        run checkleft run there instead
        """
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: [])
        XCTAssertEqual(chunks.count, 2)
        let anchor = CommentAnchor(exact: "checkleft run", prefix: "run ", suffix: " here")
        let partitioned = MarkdownCommentAnchorMap.partition(
            chunks: chunks,
            highlighted: [anchor],
            flashing: nil,
            baseURL: nil
        )
        let nonempty = partitioned.enumerated().filter { !$0.element.highlighted.isEmpty }
        XCTAssertEqual(nonempty.count, 1, "exactly one chunk should receive the recurring exact")
        XCTAssertEqual(nonempty.first?.offset, 0, "prefix/suffix must pick the Alpha occurrence")

        var highlightedChunks = 0
        for (index, chunk) in chunks.enumerated() {
            let parser = HighlightingMarkdownParser(
                highlightedAnchors: partitioned[index].highlighted
            )
            let result = try parser.attributedString(for: chunk.renderedText)
            if Self.containsHighlight(in: result) { highlightedChunks += 1 }
        }
        XCTAssertEqual(highlightedChunks, 1)
    }

    /// A selection whose `exact` straddles a heading boundary must still
    /// paint — clipped onto each overlapping chunk — rather than vanishing
    /// because no single chunk contains the whole quote.
    func testCommentAnchorStraddlingAHeadingHighlightsOverlappingChunks() throws {
        let source = """
        # Alpha

        starts here
        # Beta
        and continues
        """
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: [])
        XCTAssertEqual(chunks.count, 2)
        let plains = chunks.map { CommentProjection.plainText(for: $0.renderedText) }
        let concat = plains.joined()
        let boundary = plains[0].count
        XCTAssertGreaterThan(boundary, 4)
        XCTAssertLessThan(boundary, concat.count - 4)
        let start = concat.index(concat.startIndex, offsetBy: boundary - 4)
        let end = concat.index(concat.startIndex, offsetBy: boundary + 4)
        let needle = String(concat[start..<end])
        let range = concat.range(of: needle)!
        let prefixStart = concat.index(range.lowerBound, offsetBy: -6, limitedBy: concat.startIndex) ?? concat.startIndex
        let suffixEnd = concat.index(range.upperBound, offsetBy: 4, limitedBy: concat.endIndex) ?? concat.endIndex
        let anchor = CommentAnchor(
            exact: needle,
            prefix: String(concat[prefixStart..<range.lowerBound]),
            suffix: String(concat[range.upperBound..<suffixEnd])
        )
        let partitioned = MarkdownCommentAnchorMap.partition(
            chunks: chunks,
            highlighted: [anchor],
            flashing: nil,
            baseURL: nil
        )
        let nonempty = partitioned.enumerated().filter { !$0.element.highlighted.isEmpty }
        XCTAssertEqual(nonempty.count, 2, "a heading-straddling exact must clip onto both chunks")

        var highlightedChunks = 0
        for (index, chunk) in chunks.enumerated() {
            let parser = HighlightingMarkdownParser(
                highlightedAnchors: partitioned[index].highlighted
            )
            let result = try parser.attributedString(for: chunk.renderedText)
            if Self.containsHighlight(in: result) { highlightedChunks += 1 }
        }
        XCTAssertEqual(highlightedChunks, 2)
    }

    private static func containsHighlight(in result: AttributedString) -> Bool {
        result.runs.contains { $0.swiftUI.backgroundColor != nil }
    }

    // MARK: - Layout cost

    /// Pins that a multi-heading document still hosts. Chunking cost is
    /// recorded in the PR description, not asserted here.
    func testLargeDocumentChunksAndHosts() {
        let source = Self.largeHeadedDocument(sections: 24)
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: [])
        XCTAssertEqual(chunks.count, 25, "title + 24 sections: \(chunks.count)")

        let view = MarkdownDocumentChrome(
            title: "Large doc",
            source: source,
            commentsEnabled: false
        )
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 760, height: 640)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
    }

    /// The 24-section fixture in a short viewport must not instantiate every
    /// `DocumentStructuredText` — that is the signal that `ForEach` flattened
    /// into `LazyVStack` rather than wrapping as one non-lazy child.
    func testLargeDocumentLazyStackInstantiatesOnlyOnscreenChunks() {
        let source = Self.largeHeadedDocument(sections: 24)
        let total = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: []).count
        let probe = MarkdownChunkAppearProbe()
        let view = MarkdownDocumentChrome(
            title: "Large doc",
            source: source,
            commentsEnabled: false
        )
        .environment(\.markdownChunkAppearProbe, probe)
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 760, height: 400)
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 760, height: 400),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.contentView = hosting
        hosting.layoutSubtreeIfNeeded()
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.05))
        XCTAssertGreaterThan(probe.appeared.count, 0, "at least the on-screen chunks must appear")
        XCTAssertLessThan(
            probe.appeared.count,
            total,
            "LazyVStack must not instantiate every heading chunk; appeared=\(probe.appeared.count) total=\(total)"
        )
        _ = window
    }

    private static func largeHeadedDocument(sections: Int) -> String {
        var parts: [String] = ["# Large document\n\nIntro paragraph with **bold** and a [link](https://example.com).\n"]
        for i in 1...sections {
            parts.append("""

            ## Section \(i)

            Paragraph \(i) with nested structure:

            - top
              - nested
                - deeper still, item \(i)

            ```swift
            let value = \(i)
            ```

            """)
        }
        return parts.joined()
    }
}
