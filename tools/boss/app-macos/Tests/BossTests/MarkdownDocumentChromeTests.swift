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
    /// key (only `MarkdownDocumentChrome.documentBody` does).
    func testEditorialStyleDefaultsOff() {
        XCTAssertFalse(EnvironmentValues().markdownEditorialStyle)
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

    /// The default (empty) heading set must leave every document — every
    /// caller except a revision task's description — split as a single
    /// unmodified `.plain` chunk, so today's single-`StructuredText`
    /// rendering path is unaffected.
    func testHeadingSectionsWithNoCollapsibleHeadingsIsSinglePlainChunk() {
        let chunks = MarkdownHeadingSections.chunks(in: Self.revisionBrief, collapsibleHeadings: [])
        XCTAssertEqual(chunks.count, 1)
        guard case .plain(let text) = chunks[0] else {
            return XCTFail("expected a single .plain chunk")
        }
        XCTAssertEqual(text, Self.revisionBrief)
    }

    /// A heading set that names no heading actually present in the source
    /// must also fall back to a single `.plain` chunk (e.g. a design doc
    /// that happens to be checked against a heading text it doesn't have).
    func testHeadingSectionsWithNonMatchingHeadingIsSinglePlainChunk() {
        let chunks = MarkdownHeadingSections.chunks(in: Self.revisionBrief, collapsibleHeadings: ["Not present anywhere"])
        XCTAssertEqual(chunks.count, 1)
        guard case .plain = chunks[0] else {
            return XCTFail("expected a single .plain chunk")
        }
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
}
