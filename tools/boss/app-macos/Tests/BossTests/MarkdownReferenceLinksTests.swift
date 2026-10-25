import AppKit
import Foundation
import SwiftUI
import Textual
import XCTest
@testable import Boss

@MainActor
final class MarkdownReferenceLinksTests: XCTestCase {
    static let destination = "https://github.com/spinyfin/mono/blob/d0c8ef8bdfdc61082611b1289ba1cd6bac2cf69f/tools/checkleft/checks/proto/breaking/src/schema.rs#L342-L379"
    static let guide = """
    # Review guide

    ## Implementation
    [Field conversion][fields] distinguishes repeated and required fields.
    [`compare_message`][fields] looks up each old field by number.

    ## Evidence
    Regression tests cover type changes.

    [fields]: \(destination)
    """

    func testGuideLinksResolveAcrossSections() throws {
        let chunks = MarkdownHeadingSections.chunks(in: Self.guide, collapsibleHeadings: [])
        let before = try AttributedStringMarkdownParser.markdown().attributedString(for: chunks[1].renderedText)
        XCTAssertTrue(String(before.characters).contains("[Field conversion][fields]"))
        XCTAssertTrue(before.runs.allSatisfy { $0.link == nil })
        let parsed = MarkdownParsedDocument.parse(source: Self.guide, chunks: chunks, baseURL: nil)
        XCTAssertTrue(String(parsed[1].characters).contains("Field conversion"))
        XCTAssertFalse(String(parsed[1].characters).contains("[fields]"))
        XCTAssertEqual(parsed[1].runs.compactMap(\.link).map(\.absoluteString), [Self.destination, Self.destination])
        // Preserve the native parser's exact styling, including its treatment
        // of code within link labels, rather than inventing a second renderer.
        let whole = try AttributedStringMarkdownParser.markdown().attributedString(for: Self.guide)
        XCTAssertEqual(parsed.reduce(into: AttributedString()) { $0.append($1) }, whole)
        XCTAssertFalse(parsed.map { String($0.characters) }.joined().contains(Self.destination))
    }

    func testReferenceFormsAndFirstDefinitionWinAcrossCollapsedSection() throws {
        let source = """
        [same]: /first
        [multiline]:
          /multi
          "A title"

        ## Uses
        [Same][] [same] [**styled**][ SAME ] [multiple][multiline] [missing][undefined]

        ## Definitions
        [same]: /second
        """
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: ["Uses"])
        let parsed = MarkdownParsedDocument.parse(source: source, chunks: chunks, baseURL: URL(string: "https://example.com/"))
        XCTAssertEqual(parsed[1].runs.compactMap(\.link).map(\.absoluteString),
                       ["https://example.com/first", "https://example.com/first", "https://example.com/first", "https://example.com/multi"])
        XCTAssertTrue(String(parsed[1].characters).contains("[missing][undefined]"))
        XCTAssertFalse(String(parsed[1].characters).contains("Uses"))
    }

    func testPartitionPreservesAllRenderedContentAndAttributes() throws {
        let source = """
        # Unicode 🐈
        Text **bold**, *italic*, [inline](https://example.com), and `code`.

        ## Blocks
        > Quoted [reference][ref].

        - first
        - second

        | Name | Value |
        | --- | --- |
        | Link | [reference][ref] |

        ```markdown
        # Not a section
        [ref]: https://wrong.example.com
        ```

        ## End
        ![image][ref]
        A hard break.  
        Next line.

        [ref]: https://example.com/right
        """
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: [])
        let parsed = MarkdownParsedDocument.parse(source: source, chunks: chunks, baseURL: nil)
        let whole = try AttributedStringMarkdownParser.markdown().attributedString(for: source)
        let combined = parsed.reduce(into: AttributedString()) { $0.append($1) }
        XCTAssertEqual(combined, whole)
        XCTAssertEqual(chunks.count, 3)
    }

    func testCollapsedHeadingTextIsNotMistakenForItsBody() {
        let source = "# repeated\nrepeated\n# End\nEnd\n"
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: ["repeated", "End"])
        let parsed = MarkdownParsedDocument.parse(source: source, chunks: chunks, baseURL: nil)
        XCTAssertEqual(parsed.map { String($0.characters) }, ["repeated", "End"])
        XCTAssertTrue(parsed.allSatisfy { content in
            content.runs.allSatisfy { run in
                !(run.presentationIntent?.components.contains { component in
                    if case .header = component.kind { return true }
                    return false
                } ?? false)
            }
        })
    }

    func testCacheRefreshesWhenOnlyDefinitionsOrBaseURLChange() {
        let source = "## Use\n[link][ref]\n## Definitions\n[ref]: /first"
        let cache = MarkdownParsedDocument.Cache()
        for (text, base, expected) in [
            (source, "https://example.com/", "https://example.com/first"),
            (source.replacingOccurrences(of: "/first", with: "/second"), "https://example.com/", "https://example.com/second"),
            (source, "https://other.example/", "https://other.example/first"),
        ] {
            let chunks = MarkdownHeadingSections.chunks(in: text, collapsibleHeadings: [])
            let parsed = cache.parse(source: text, chunks: chunks, baseURL: URL(string: base))
            XCTAssertEqual(parsed[0].runs.compactMap(\.link).map(\.absoluteString), [expected])
        }
    }

    func testEmptyAndStructuralBlocksMatchWholeDocument() throws {
        for source in [
            "", "---\n\n# Heading\n\n---\n",
            "# Empty\n# Next\nText\n", "# HTML\n<div>raw</div>\n\n# End\nDone",
            "# Table\n| A | B |\n| --- | --- |\n| | text |\n\n# End\nDone",
        ] {
            let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: [])
            let parsed = MarkdownParsedDocument.parse(source: source, chunks: chunks, baseURL: nil)
            let whole = try AttributedStringMarkdownParser.markdown().attributedString(for: source)
            XCTAssertEqual(parsed.reduce(into: AttributedString()) { $0.append($1) }, whole, source)
        }
    }

    func testThematicBreaksSurviveCollapsedHeading() throws {
        let source = "# Folded\n\n---\n\nText\n\n# End\nDone"
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: ["Folded"])
        let parsed = MarkdownParsedDocument.parse(source: source, chunks: chunks, baseURL: nil)
        XCTAssertEqual(String(parsed[0].characters), "⸻Text")
        XCTAssertTrue(parsed[0].runs.contains { $0.presentationIntent?.components.first?.kind == .thematicBreak })
        XCTAssertEqual(String(parsed[1].characters), "EndDone")
    }

    func testSearchAndCommentOffsetsUseResolvedLabels() throws {
        let chunks = MarkdownHeadingSections.chunks(in: Self.guide, collapsibleHeadings: [])
        let parsed = MarkdownParsedDocument.parse(source: Self.guide, chunks: chunks, baseURL: nil)
        let find = MarkdownFindState()
        find.updateSource(Self.guide, baseURL: nil)
        find.query = "Field conversion"
        XCTAssertEqual(find.chunkMatches(1).matches.count, 1)
        let range = try XCTUnwrap(find.chunkMatches(1).matches.first)
        let plain = String(parsed[1].characters)
        XCTAssertEqual(String(plain.dropFirst(range.lowerBound).prefix(range.count)), "Field conversion")
        let anchor = CommentAnchor(exact: "Field conversion")
        let map = MarkdownCommentAnchorMap.Cache().partition(
            source: Self.guide, headings: [], chunks: chunks, parsed: parsed,
            highlighted: [anchor], flashing: nil, baseURL: nil
        )
        XCTAssertEqual(map[1].highlighted.count, 1)
        let highlighted = try HighlightingMarkdownParser(
            highlightedAnchors: map[1].highlighted, parsedContent: parsed[1]
        ).attributedString(for: chunks[1].renderedText)
        XCTAssertTrue(highlighted.runs.contains { $0.link != nil && $0.backgroundColor != nil })
    }

    func testCaptureBeforeAndAfterRendering() throws {
        let chunks = MarkdownHeadingSections.chunks(in: Self.guide, collapsibleHeadings: [])
        let parsed = MarkdownParsedDocument.parse(source: Self.guide, chunks: chunks, baseURL: nil)
        for fixed in [false, true] {
            let view = VStack(alignment: .leading, spacing: 18) {
                ForEach(chunks.indices, id: \.self) { index in
                    StructuredText(chunks[index].renderedText, parser: fixed
                        ? ParsedMarkdownChunkParser(content: parsed[index]) as any MarkupParser
                        : AttributedStringMarkdownParser.markdown() as any MarkupParser)
                }
            }
            .bossMarkdown()
            .padding(28)
            .frame(width: 760, height: 500, alignment: .topLeading)
            .background(Color.white)
            .environment(\.colorScheme, .light)
            let hosting = NSHostingView(rootView: view)
            let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 760, height: 500),
                                  styleMask: [.titled], backing: .buffered, defer: false)
            window.contentView = hosting
            for _ in 0..<8 {
                hosting.layoutSubtreeIfNeeded()
                RunLoop.current.run(until: Date().addingTimeInterval(0.05))
            }
            let bitmap = try XCTUnwrap(hosting.bitmapImageRepForCachingDisplay(in: hosting.bounds))
            hosting.cacheDisplay(in: hosting.bounds, to: bitmap)
            let data = try XCTUnwrap(bitmap.representation(using: .png, properties: [:]))
            let directory = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"] ?? NSTemporaryDirectory()
            let name = fixed ? "reference-links-after.png" : "reference-links-before.png"
            try data.write(to: URL(fileURLWithPath: directory).appendingPathComponent(name))
            window.orderOut(nil)
        }
    }
}
