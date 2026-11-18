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

    func testCaptureBriefMarkdownLoadsEngineShapedFixture() throws {
        let dir = ProcessInfo.processInfo.environment["TEST_TMPDIR"]
            .map { URL(fileURLWithPath: $0, isDirectory: true) }
            ?? FileManager.default.temporaryDirectory
        let temp = dir.appendingPathComponent("revision-brief-\(UUID().uuidString).md")
        try RevisionBriefCaptureFixture.engineShaped.write(to: temp, atomically: true, encoding: .utf8)
        XCTAssertEqual(
            try BossRevisionBriefCapture.loadMarkdown(from: temp.path),
            RevisionBriefCaptureFixture.engineShaped
        )
    }

    func testExactGeneratedBrief() throws {
        try assertExactBriefPreservesBlocks(RevisionBriefCaptureFixture.adjacentFindingHeading)
        try assertExactBriefPreservesBlocks(RevisionBriefCaptureFixture.engineShaped)
        let engine = RevisionBriefCaptureFixture.engineShaped
        XCTAssertTrue(
            engine.contains("do the work.\n\n### [high]"),
            "engineShaped must snapshot the blank line render.rs emits before the first finding"
        )
        XCTAssertTrue(engine.contains("### [medium]"))
        XCTAssertTrue(engine.contains("**Review summary:** Two findings require a fix."))
        let engineChunks = MarkdownHeadingSections.chunks(
            in: engine,
            collapsibleHeadings: [RevisionBriefCollapsibleHeadings.hardRule]
        )
        XCTAssertEqual(engineChunks.count, 4, "preamble + boilerplate fold + two findings: \(engineChunks)")
        XCTAssertFalse(engineChunks[1].renderedText.contains("### ["))
        XCTAssertTrue(engineChunks[2].renderedText.contains("### [high]"))
        XCTAssertTrue(engineChunks[3].renderedText.contains("### [medium]"))
        XCTAssertTrue(engineChunks[3].renderedText.contains("**Review summary:**"))
    }

    private func assertExactBriefPreservesBlocks(_ source: String) throws {
        let chunks = MarkdownHeadingSections.chunks(
            in: source,
            collapsibleHeadings: [RevisionBriefCollapsibleHeadings.hardRule]
        )
        let parsed = MarkdownParsedDocument.parse(source: source, chunks: chunks, baseURL: nil)
        XCTAssertGreaterThanOrEqual(chunks.count, 3)
        XCTAssertTrue(chunks[1].renderedText.contains("The default is to do the work."))
        XCTAssertFalse(chunks[1].renderedText.contains("### ["))
        let boilerplateParagraphs = parsed[1].runs[\.presentationIntent].compactMap { intent, range -> AttributedString? in
            guard intent?.components.first?.kind == .paragraph else { return nil }
            return AttributedString(parsed[1][range])
        }
        let wrapped = try XCTUnwrap(boilerplateParagraphs.first { String($0.characters).hasPrefix("If — and only if") })
        XCTAssertEqual(String(wrapped.characters), "If — and only if — a finding genuinely cannot or should not be fixed within this revision's scope, you must EXPLICITLY SURFACE that in your response: state which finding you are not fixing and exactly why (technical blocker, out-of-scope dependency, requires a decision from the repo owner, etc.). Do NOT silently file a follow-up or leave a TODO. The default is to do the work.")
        let bold = wrapped.runs.filter { $0.inlinePresentationIntent?.contains(.stronglyEmphasized) == true }
            .map { String(wrapped[$0.range].characters) }.joined()
        XCTAssertEqual(bold, "If — and only if — a finding genuinely cannot or should not be fixed within this revision's scope")
        let finding = parsed.first { String($0.characters).contains("[high] Workspace result admission") }
        let findingText = String(try XCTUnwrap(finding).characters)
        XCTAssertTrue(findingText.contains("File: tools/boss/engine/host-execution/src/lib.rs"))
        XCTAssertTrue(findingText.contains("Category: correctness"))
        XCTAssertTrue(findingText.contains("Confidence: medium"))
        XCTAssertFalse(String(parsed[0].characters).contains("If — and only if"))

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

    func testExactBriefUsesDefaultFoldWithHardRuleHeadingAndLineEnding() throws {
        let headings: Set<String> = [RevisionBriefCollapsibleHeadings.hardRule]
        for newline in ["\n", "\r\n", "\r"] {
            for fixture in [RevisionBriefCaptureFixture.engineShaped, RevisionBriefCaptureFixture.adjacentFindingHeading] {
                let source = fixture.replacingOccurrences(of: "\n", with: newline)
                let chunks = MarkdownHeadingSections.chunks(
                    in: source,
                    collapsibleHeadings: headings
                )
                XCTAssertGreaterThanOrEqual(chunks.count, 3, "\(newline.debugDescription): \(chunks)")
                XCTAssertFalse(chunks[1].renderedText.contains("### ["), "\(newline.debugDescription)")
                XCTAssertTrue(chunks[2].renderedText.contains("### [high]"), "\(newline.debugDescription)")
                let parsed = MarkdownParsedDocument.parse(source: source, chunks: chunks, baseURL: nil)
                XCTAssertTrue(String(parsed[1].characters).contains("The default is to do the work."))
                XCTAssertFalse(String(parsed[1].characters).contains("[high]"))
                XCTAssertTrue(String(parsed[2].characters).contains("[high]"))
            }
        }
    }

    func testDefaultFoldKeepsFindingsVisibleOutsideBoilerplate() {
        let chunks = MarkdownHeadingSections.chunks(
            in: Self.brief,
            collapsibleHeadings: RevisionBriefCollapsibleHeadings.collapsedByDefault(in: Self.brief)
        )
        XCTAssertEqual(chunks.count, 2)
        let parsed = MarkdownParsedDocument.parse(source: Self.brief, chunks: chunks, baseURL: nil)
        XCTAssertTrue(String(parsed[0].characters).contains("If — and only if"))
        XCTAssertTrue(String(parsed[0].characters).contains("The default is to do the work."))
        XCTAssertFalse(String(parsed[0].characters).contains("[high] Base-only"))
        XCTAssertTrue(String(parsed[1].characters).contains("[high] Base-only"))
        XCTAssertTrue(String(parsed[1].characters).hasSuffix("Review summary: All findings require a fix."))
    }

    func testOnlyTopLevelCommonMarkHeadingsDelimitSections() {
        let source = "# Fold\n\n- item\n  ## Nested list heading\n  continuation\n\n> ## Quoted heading\n> continuation\n\n````\n```\n## Still code\n````\n\n## Child\nbody\n\nPeer\n====\nvisible"
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: ["Fold"])
        XCTAssertEqual(chunks.count, 3)
        XCTAssertTrue(chunks[0].renderedText.contains("Nested list heading"))
        XCTAssertFalse(chunks[0].renderedText.contains("## Child"))
        XCTAssertTrue(chunks[1].renderedText.hasPrefix("## Child"))
        XCTAssertEqual(chunks[2], .plain("Peer\n====\nvisible"))
        XCTAssertFalse(MarkdownHeadingSections.containsHeading("Nested list heading", in: source))
        XCTAssertFalse(MarkdownHeadingSections.containsHeading("Quoted heading", in: source))
        XCTAssertFalse(MarkdownHeadingSections.containsHeading("Still code", in: source))
        let unfolded = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: [])
        XCTAssertEqual(unfolded.count, 3)
        XCTAssertEqual(unfolded.map(\.renderedText).joined(), source)
        let nested = MarkdownHeadingSections.chunks(
            in: "## Fold\nbody\n### Child\nchild\n# End\nend",
            collapsibleHeadings: ["Fold"]
        )
        XCTAssertEqual(nested, [
            .collapsible(heading: "Fold", body: "body\n"),
            .plain("### Child\nchild\n"),
            .plain("# End\nend"),
        ])
        let windows = Self.brief.replacingOccurrences(of: "\n", with: "\r\n")
        let windowsChunks = MarkdownHeadingSections.chunks(
            in: windows,
            collapsibleHeadings: [RevisionBriefCollapsibleHeadings.hardRule]
        )
        XCTAssertEqual(windowsChunks.count, 2)
        XCTAssertFalse(windowsChunks[0].renderedText.contains("### [high]"))
        XCTAssertTrue(windowsChunks[1].renderedText.contains("### [high]"))
    }

    func testSetextFoldAndUnicodeWithinParagraph() {
        let source = "Title\n=====\nbody\u{2028}continuation\n\nPeer\n====\nvisible"
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: ["Title"])
        XCTAssertEqual(chunks, [.collapsible(heading: "Title", body: "body\u{2028}continuation\n\n"), .plain("Peer\n====\nvisible")])
    }

}
