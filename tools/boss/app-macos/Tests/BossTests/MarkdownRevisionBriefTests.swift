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

    **Review summary:** All findings require a fix.
    """

    func testExactGeneratedBrief() throws {
        let source = RevisionBriefCaptureFixture.markdown
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: [RevisionBriefCollapsibleHeadings.hardRule])
        let parsed = MarkdownParsedDocument.parse(source: source, chunks: chunks, baseURL: nil)
        XCTAssertEqual(chunks.count, 2)
        XCTAssertTrue(chunks[1].renderedText.contains("The default is to do the work."))
        let paragraphs = parsed[1].runs[\.presentationIntent].compactMap { intent, range -> AttributedString? in
            guard intent?.components.first?.kind == .paragraph else { return nil }
            return AttributedString(parsed[1][range])
        }
        let wrapped = try XCTUnwrap(paragraphs.first { String($0.characters).hasPrefix("If — and only if") })
        XCTAssertEqual(String(wrapped.characters), "If — and only if — a finding genuinely cannot or should not be fixed within this revision's scope, you must EXPLICITLY SURFACE that in your response: state which finding you are not fixing and exactly why (technical blocker, out-of-scope dependency, requires a decision from the repo owner, etc.). Do NOT silently file a follow-up or leave a TODO. The default is to do the work.")
        let bold = wrapped.runs.filter { $0.inlinePresentationIntent?.contains(.stronglyEmphasized) == true }
            .map { String(wrapped[$0.range].characters) }.joined()
        XCTAssertEqual(bold, "If — and only if — a finding genuinely cannot or should not be fixed within this revision's scope")
        let metadata = try XCTUnwrap(paragraphs.first { String($0.characters).hasPrefix("File:") })
        XCTAssertEqual(String(metadata.characters), "File: tools/boss/engine/host-execution/src/lib.rs (workspace::Executor::dispatch result-size ensure ~L1762-1766)\nCategory: correctness\nConfidence: medium")
        XCTAssertTrue(String(parsed[1].characters).contains("[high] Workspace result admission"))
        XCTAssertFalse(String(parsed[0].characters).contains("If — and only if"))

        // Unfolded chunks must also preserve every character and block attribute,
        // including when the next heading immediately follows the paragraph.
        let unfolded = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: [])
        let contents = MarkdownParsedDocument.parse(source: source, chunks: unfolded, baseURL: nil)
        let whole = try AttributedStringMarkdownParser.markdown().attributedString(for: source)
        XCTAssertEqual(contents.reduce(into: AttributedString()) { $0.append($1) }, whole)
        XCTAssertTrue(String(contents[1].characters).hasSuffix("The default is to do the work."))
        XCTAssertTrue(String(contents[2].characters).hasPrefix("[high] Workspace"))
        XCTAssertEqual(MarkdownEditorialMetrics.chunkTopSpacing(for: contents[1]), 3)
        XCTAssertEqual(MarkdownEditorialMetrics.chunkTopSpacing(for: contents[2]), 2,
                       "An H3 finding must retain its own spacing across a lazy chunk boundary")
    }

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

    func testExactBriefUsesOutlineBoundariesWithAnyHeadingAndLineEnding() throws {
        for newline in ["\n", "\r\n", "\r"] {
            let source = (RevisionBriefCaptureFixture.markdown.replacingOccurrences(
                of: RevisionBriefCollapsibleHeadings.hardRule, with: "Review instructions"
            ) + "\n## Next section\nVisible.").replacingOccurrences(of: "\n", with: newline)
            let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: ["Review instructions"])
            XCTAssertEqual(chunks.count, 3)
            let parsed = MarkdownParsedDocument.parse(source: source, chunks: chunks, baseURL: nil)
            XCTAssertTrue(String(parsed[1].characters).contains("The default is to do the work."))
            XCTAssertTrue(String(parsed[1].characters).hasSuffix("Verified: dispatch only ensures ..."))
            XCTAssertEqual(String(parsed[2].characters), "Next sectionVisible.")
        }
    }

    func testDefaultFoldIncludesDescendantFindingsAndSummary() {
        let chunks = MarkdownHeadingSections.chunks(in: Self.brief, collapsibleHeadings: RevisionBriefCollapsibleHeadings.collapsedByDefault(in: Self.brief))
        XCTAssertEqual(chunks.count, 1)
        let parsed = MarkdownParsedDocument.parse(source: Self.brief, chunks: chunks, baseURL: nil)
        XCTAssertTrue(String(parsed[0].characters).contains("If — and only if"))
        XCTAssertTrue(String(parsed[0].characters).contains("The default is to do the work."))
        XCTAssertTrue(String(parsed[0].characters).contains("[high] Base-only"))
        XCTAssertTrue(String(parsed[0].characters).hasSuffix("Review summary: All findings require a fix."))
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
        XCTAssertEqual(windowsChunks.count, 1)
        XCTAssertTrue(windowsChunks[0].renderedText.contains("### [high]"))
    }

    func testSetextFoldAndUnicodeWithinParagraph() {
        let source = "Title\n=====\nbody\u{2028}continuation\n\nPeer\n====\nvisible"
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: ["Title"])
        XCTAssertEqual(chunks, [.collapsible(heading: "Title", body: "body\u{2028}continuation\n\n"), .plain("Peer\n====\nvisible")])
    }

}
