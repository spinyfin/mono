import Foundation
import XCTest
@testable import Boss

final class MarkdownNavigationTests: XCTestCase {
    func testSlugsAndDuplicateCollisions() {
        XCTAssertEqual(MarkdownHeadingAnchors.slugs([
            "Concrete Evaluation!", "Concrete Evaluation!", "Concrete Evaluation!",
            "Hello-1", "Hello", "Hello", "Café & 世界", "snake_case", "A  B"
        ]), [
            "concrete-evaluation", "concrete-evaluation-1", "concrete-evaluation-2",
            "hello-1", "hello", "hello-2", "café--世界", "snake_case", "a--b"
        ])
    }

    func testRenderedHeadingTextAndSectionOffsets() {
        let source = "Intro\n\n# **Hello** `world`!\n\n```\n# Fake\n```\n\nHello world!\n---\n"
        XCTAssertEqual(MarkdownHeadingAnchors.chunkIndex(for: "hello-world", source: source), 1)
        XCTAssertEqual(MarkdownHeadingAnchors.chunkIndex(for: "hello-world-1", source: source), 2)
        XCTAssertNil(MarkdownHeadingAnchors.chunkIndex(for: "fake", source: source))
        XCTAssertNil(MarkdownHeadingAnchors.chunkIndex(for: "missing", source: source))
    }

    func testLinkClassification() throws {
        let document = URL(fileURLWithPath: "/repo/docs/design.md")
        let base = document.deletingLastPathComponent()
        func classify(_ value: String) throws -> MarkdownLinkDestination {
            MarkdownLinkDestination.classify(try XCTUnwrap(URL(string: value)), documentURL: document, baseURL: base)
        }
        XCTAssertEqual(try classify("#concrete-evaluation"), .fragment("concrete-evaluation"))
        XCTAssertEqual(try classify("design.md#hello"), .fragment("hello"))
        XCTAssertEqual(try classify("./design.md#caf%C3%A9"), .fragment("café"))
        XCTAssertEqual(try classify("file:///repo/docs/#hello"), .fragment("hello"))
        XCTAssertEqual(try classify("../README.md#intro"), .document(URL(fileURLWithPath: "/repo/README.md"), "intro"))
        XCTAssertEqual(try classify("other.markdown"), .document(base.appendingPathComponent("other.markdown"), nil))
        for url in ["https://example.com/doc.md#heading", "http://example.com", "mailto:someone@example.com"] {
            XCTAssertEqual(try classify(url), .external)
        }
        for url in ["file:///repo/image.png", "javascript:alert(1)", "custom:thing"] {
            XCTAssertEqual(try classify(url), .ignored)
        }
    }

    func testRepositoryLinksPreserveRelativePathsAndFragments() throws {
        let document = try XCTUnwrap(URL(string: "boss-repository:/docs/design.md"))
        let link = try XCTUnwrap(URL(string: "../other.md#summary"))
        XCTAssertEqual(
            MarkdownLinkDestination.classify(link, documentURL: document, baseURL: document.deletingLastPathComponent()),
            .document(try XCTUnwrap(URL(string: "boss-repository:///other.md")), "summary")
        )
        XCTAssertEqual(
            MarkdownLinkDestination.classify(
                try XCTUnwrap(URL(string: "./design.md#summary")),
                documentURL: document, baseURL: document.deletingLastPathComponent()
            ),
            .fragment("summary")
        )
    }
}
