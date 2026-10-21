import AppKit
import SwiftUI
import Textual
import XCTest

@testable import Boss

/// Pins the one rule the list-rhythm work is about: **a list shares the
/// paragraph's leading, and the gap between two bullets is never tighter than
/// the gap between a bullet's own wrapped lines.**
///
/// Textual spaces list items with its own environment value
/// (`listItemSpacing`) that is resolved at the *list's* font, while the lines
/// inside an item are spaced by the paragraph style's `lineSpacing`, resolved
/// at the item's (editorially enlarged) font. The item gap is therefore
/// derived from the body leading, so wrapped lines never sit further apart
/// than consecutive bullets.
///
/// Every measurement here is real AppKit layout of a `StructuredText` hosted
/// in a never-presented `NSWindow`, with hard line breaks (`  ` + newline)
/// producing a deterministic two-line item instead of width-dependent
/// wrapping. Heights are compared, not absolute values, so the assertions
/// hold across Dynamic Type sizes:
///
/// - one paragraph of two lines vs. one line gives the paragraph's line pitch;
/// - one bullet of two lines vs. one line gives the in-item line pitch;
/// - two one-line bullets vs. one two-line bullet gives (item gap − leading).
@MainActor
final class MarkdownListRhythmTests: XCTestCase {
    private static let width: CGFloat = 600
    /// Two heights that differ by less than this are the same leading.
    private static let tolerance: CGFloat = 0.5
    /// A bullet gap that beats the in-item leading by less than this does not
    /// read as item separation.
    private static let minimumSeparation: CGFloat = 2

    /// Retains AppKit's window/content-view pairs for the test's lifetime so
    /// a measured hierarchy is never torn down mid-assertion.
    private var hosted: [(NSWindow, NSView)] = []

    override func tearDown() {
        hosted.removeAll()
        super.tearDown()
    }

    /// Lays `markdown` out through the production theme in a real window and
    /// returns its natural height at a fixed width.
    private func height(
        _ markdown: String, editorial: Bool, file: StaticString = #filePath, line: UInt = #line
    ) -> CGFloat {
        let view = StructuredText(markdown: markdown)
            .bossMarkdown()
            .environment(\.markdownEditorialStyle, editorial)
            .frame(width: Self.width, alignment: .leading)
            .fixedSize(horizontal: false, vertical: true)
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: Self.width, height: 400),
            styleMask: [.titled], backing: .buffered, defer: false)
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: Self.width, height: 400)
        window.contentView = hosting
        for _ in 0..<4 {
            hosting.layoutSubtreeIfNeeded()
            RunLoop.current.run(until: Date().addingTimeInterval(0.02))
        }
        XCTAssertFalse(
            window.isVisible, "layout window must never be presented", file: file, line: line)
        hosted.append((window, hosting))
        let measured = hosting.fittingSize.height
        XCTAssertGreaterThan(measured, 0, "nothing laid out for \(markdown)", file: file, line: line)
        return measured
    }

    private struct Rhythm {
        /// Distance from one line to the next inside a paragraph.
        var paragraphLinePitch: CGFloat
        /// Distance from one line to the next inside a single list item.
        var itemLinePitch: CGFloat
        /// (gap between two bullets) − (leading between a bullet's own lines).
        /// Positive means the bullets are further apart than wrapped lines.
        var itemGapOverLeading: CGFloat
        /// The same for a nested list.
        var nestedGapOverLeading: CGFloat
        /// (nested item gap) − (top-level item gap).
        var nestedMinusTopLevelGap: CGFloat
        /// (gap between bullets carrying inline code and a link) − leading.
        var inlineGapOverLeading: CGFloat
    }

    private func measure(editorial: Bool) -> Rhythm {
        let paragraph1 = height("one", editorial: editorial)
        let paragraph2 = height("one  \ntwo", editorial: editorial)
        let item1 = height("- one", editorial: editorial)
        let item2 = height("- one  \n  two", editorial: editorial)
        let twoItems = height("- one\n- two", editorial: editorial)
        let nestedItem1 = height("- a\n  - one", editorial: editorial)
        let nestedItem2 = height("- a\n  - one  \n    two", editorial: editorial)
        let nestedTwoItems = height("- a\n  - one\n  - two", editorial: editorial)
        let inlineItem2 = height("- `code` one  \n  [link](https://example.com) two", editorial: editorial)
        let inlineTwoItems = height("- `code` one\n- [link](https://example.com) two", editorial: editorial)
        return Rhythm(
            paragraphLinePitch: paragraph2 - paragraph1,
            itemLinePitch: item2 - item1,
            itemGapOverLeading: twoItems - item2,
            nestedGapOverLeading: nestedTwoItems - nestedItem2,
            nestedMinusTopLevelGap: (nestedTwoItems - nestedItem1) - (twoItems - item1),
            inlineGapOverLeading: inlineTwoItems - inlineItem2)
    }

    private func assertRhythm(editorial: Bool, file: StaticString = #filePath, line: UInt = #line) {
        let rhythm = measure(editorial: editorial)
        let label = editorial ? "editorial" : "compact"
        XCTAssertEqual(
            rhythm.itemLinePitch, rhythm.paragraphLinePitch, accuracy: Self.tolerance,
            "\(label): lines inside a bullet must use the paragraph leading",
            file: file, line: line)
        XCTAssertGreaterThanOrEqual(
            rhythm.itemGapOverLeading, Self.minimumSeparation,
            "\(label): consecutive bullets must sit visibly further apart than a bullet's wrapped lines",
            file: file, line: line)
        XCTAssertGreaterThanOrEqual(
            rhythm.nestedGapOverLeading, Self.minimumSeparation,
            "\(label): nested bullets must sit visibly further apart than their wrapped lines",
            file: file, line: line)
        XCTAssertEqual(
            rhythm.nestedMinusTopLevelGap, 0, accuracy: Self.tolerance,
            "\(label): nested and top-level bullets must share one item gap",
            file: file, line: line)
        XCTAssertGreaterThanOrEqual(
            rhythm.inlineGapOverLeading, Self.minimumSeparation,
            "\(label): inline code and links must not change the item gap",
            file: file, line: line)
    }

    /// The document viewer (design docs, investigation docs, expanded
    /// descriptions): enlarged body with generous leading, where the item gap
    /// must be expressed at the *unscaled* root font to match the leading.
    func testEditorialListsShareParagraphLeadingAndSeparateItems() {
        assertRhythm(editorial: true)
    }

    /// Compact surfaces (transcript, comment cards, release notes) follow the
    /// same rule at their own, smaller leading.
    func testCompactListsShareParagraphLeadingAndSeparateItems() {
        assertRhythm(editorial: false)
    }

    /// The item gap is derived from the paragraph leading rather than set
    /// beside it, so the two can't drift apart in a later typography change.
    func testListItemGapIsDerivedFromBodyLeading() {
        XCTAssertGreaterThan(
            MarkdownEditorialMetrics.listItemGap(editorial: true),
            MarkdownEditorialMetrics.bodyLeading(editorial: true))
        XCTAssertGreaterThan(
            MarkdownEditorialMetrics.listItemGap(editorial: false),
            MarkdownEditorialMetrics.bodyLeading(editorial: false))
        XCTAssertEqual(
            MarkdownEditorialMetrics.listItemGap(editorial: true)
                - MarkdownEditorialMetrics.bodyLeading(editorial: true),
            MarkdownEditorialMetrics.listItemGap(editorial: false)
                - MarkdownEditorialMetrics.bodyLeading(editorial: false),
            accuracy: 0.0001)
    }
}
