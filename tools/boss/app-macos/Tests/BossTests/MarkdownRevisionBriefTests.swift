import AppKit
import SwiftUI
import Textual
import XCTest
@testable import Boss

@MainActor
final class MarkdownRevisionBriefTests: XCTestCase {
    static let brief = """
    ## HARD RULE: no punting — do the actual work

    Each finding below requires a real code change that resolves it.
    The following are FORBIDDEN — they do NOT count as addressing a finding:

    - Filing a follow-up task or issue in lieu of fixing the finding.

    **If — and only if — a finding genuinely cannot or should not be fixed within
    this revision's scope**, you must EXPLICITLY SURFACE that in your response:
    state which finding you are not fixing and exactly why (technical blocker,
    out-of-scope dependency, requires a decision from the repo owner, etc.). Do NOT silently
    file a follow-up or leave a TODO. The default is to do the work.

    ### [high] Base-only sibling on newer main than the GitHub head is still refused
    **File:** `tools/cube/src/app/pr_bookmarks.rs` (fn ensure_sides_are_obsolete, ...)  
    **Category:** deferred_scope

    Fix: exclude proven trunk/upstream ancestry from the unique set (e.g. subtract
    `::main@<remote>` or otherwise separate base history).

    ## Review summary
    All findings require a fix.
    """

    func testWrappedParagraphsAndHardBreaksPreserveCommonMarkBlocks() throws {
        let chunks = MarkdownHeadingSections.chunks(in: Self.brief, collapsibleHeadings: [])
        let parsed = MarkdownParsedDocument.parse(source: Self.brief, chunks: chunks, baseURL: nil)
        let whole = try AttributedStringMarkdownParser.markdown().attributedString(for: Self.brief)
        XCTAssertEqual(parsed.reduce(into: AttributedString()) { $0.append($1) }, whole)
        let paragraphs = parsed.flatMap { content in
            content.runs[\.presentationIntent].compactMap { intent, range -> String? in
                guard intent?.components.first?.kind == .paragraph else { return nil }
                return String(content[range].characters)
            }
        }
        XCTAssertTrue(paragraphs.contains { $0.hasPrefix("If — and only if") && $0.hasSuffix("The default is to do the work.") })
        XCTAssertTrue(paragraphs.contains { $0.hasPrefix("Fix: exclude") && $0.contains("subtract ::main@<remote> or otherwise") })
        XCTAssertTrue(paragraphs.contains { $0.hasPrefix("File:") && $0.contains("\nCategory: deferred_scope") })
    }

    func testFoldContainsWholeParagraphsAndNestedHeadings() {
        let chunks = MarkdownHeadingSections.chunks(in: Self.brief, collapsibleHeadings: [RevisionBriefCollapsibleHeadings.hardRule])
        XCTAssertEqual(chunks.count, 2)
        let parsed = MarkdownParsedDocument.parse(source: Self.brief, chunks: chunks, baseURL: nil)
        XCTAssertTrue(String(parsed[0].characters).contains("If — and only if"))
        XCTAssertTrue(String(parsed[0].characters).contains("The default is to do the work."))
        XCTAssertTrue(String(parsed[0].characters).contains("[high] Base-only"))
        XCTAssertEqual(String(parsed[1].characters), "Review summaryAll findings require a fix.")
    }

    func testOnlyTopLevelCommonMarkHeadingsDelimitSections() {
        let source = "# Fold\n\n- item\n  ## Nested list heading\n  continuation\n\n> ## Quoted heading\n> continuation\n\n````\n```\n## Still code\n````\n\n## Child\nbody\n\nPeer\n====\nvisible"
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: ["Fold"])
        XCTAssertEqual(chunks.count, 2)
        XCTAssertTrue(chunks[0].renderedText.contains("## Child\nbody"))
        XCTAssertEqual(chunks[1], .plain("Peer\n====\nvisible"))
        XCTAssertFalse(MarkdownHeadingSections.containsHeading("Nested list heading", in: source))
        XCTAssertFalse(MarkdownHeadingSections.containsHeading("Quoted heading", in: source))
        XCTAssertFalse(MarkdownHeadingSections.containsHeading("Still code", in: source))
        let unfolded = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: [])
        XCTAssertEqual(unfolded.count, 3)
        XCTAssertEqual(unfolded.map(\.renderedText).joined(), source)
        let higher = MarkdownHeadingSections.chunks(in: "## Fold\nbody\n### Child\nchild\n# End\nend", collapsibleHeadings: ["Fold"])
        XCTAssertEqual(higher, [.collapsible(heading: "Fold", body: "body\n### Child\nchild\n"), .plain("# End\nend")])
        let windows = Self.brief.replacingOccurrences(of: "\n", with: "\r\n")
        let windowsChunks = MarkdownHeadingSections.chunks(in: windows, collapsibleHeadings: [RevisionBriefCollapsibleHeadings.hardRule])
        XCTAssertEqual(windowsChunks.count, 2)
        XCTAssertTrue(windowsChunks[1].renderedText.hasPrefix("## Review summary"))
    }

    func testSetextFoldAndUnicodeWithinParagraph() {
        let source = "Title\n=====\nbody\u{2028}continuation\n\nPeer\n====\nvisible"
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: ["Title"])
        XCTAssertEqual(chunks, [.collapsible(heading: "Title", body: "body\u{2028}continuation\n\n"), .plain("Peer\n====\nvisible")])
    }

    func testCapture() throws {
        let chunks = MarkdownHeadingSections.chunks(in: Self.brief, collapsibleHeadings: [])
        let parsed = MarkdownParsedDocument.parse(source: Self.brief, chunks: chunks, baseURL: nil)
        let view = VStack(alignment: .leading, spacing: 0) {
            ForEach(chunks.indices, id: \.self) { index in
                StructuredText(chunks[index].renderedText, parser: ParsedMarkdownChunkParser(content: parsed[index]))
            }
        }.bossMarkdown().padding(28)
            .frame(width: 900, height: 1100, alignment: .topLeading)
            .background(Color.white).environment(\.colorScheme, .light)
        try capture(view, name: "revision-brief.png")
        try capture(MarkdownDocumentChrome(title: "Revision brief", source: Self.brief,
                    collapsedByDefaultHeadings: [RevisionBriefCollapsibleHeadings.hardRule]), name: "revision-brief-collapsed.png")
    }

    private func capture(_ view: some View, name: String) throws {
        let hosting = NSHostingView(rootView: view)
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 900, height: 1100), styleMask: [.titled], backing: .buffered, defer: false)
        window.contentView = hosting
        for _ in 0..<8 {
            hosting.layoutSubtreeIfNeeded()
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        }
        let bitmap = try XCTUnwrap(hosting.bitmapImageRepForCachingDisplay(in: hosting.bounds))
        hosting.cacheDisplay(in: hosting.bounds, to: bitmap)
        let data = try XCTUnwrap(bitmap.representation(using: .png, properties: [:]))
        let directory = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"] ?? NSTemporaryDirectory()
        try data.write(to: URL(fileURLWithPath: directory).appendingPathComponent(name))
        window.orderOut(nil)
    }
}
