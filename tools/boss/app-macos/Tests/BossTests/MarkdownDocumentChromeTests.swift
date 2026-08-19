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
}
