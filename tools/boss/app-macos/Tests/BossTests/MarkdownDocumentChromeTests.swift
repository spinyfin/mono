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
    # Task title

    Some intro paragraph with **bold**, *italic*, `inline code`, and a
    [link](https://example.com).

    ```swift
    struct Greeter {
        let name: String
    }
    ```

    | Column A | Column B |
    | -------- | -------- |
    | one      | two      |

    - top level
      - nested one
    - another top
    """

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
