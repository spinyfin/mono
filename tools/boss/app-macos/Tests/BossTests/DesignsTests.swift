import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// The Designs tab reads its document list from GitHub via the engine.
/// These cover the two things the client is still responsible for:
/// decoding the engine's classified states, and nesting the flat path
/// list the engine sends into the tree the sidebar renders.
final class DesignDocTreeStateDecodingTests: XCTestCase {
    private func decodeState(_ json: [String: Any]) throws -> DesignDocTreeState {
        let data = try JSONSerialization.data(withJSONObject: json)
        return try JSONDecoder().decode(DesignDocTreeState.self, from: data)
    }

    func testDecodesNoRepoConfigured() throws {
        let state = try decodeState(["type": "no_repo_configured"])
        XCTAssertEqual(state, .noRepoConfigured)
    }

    func testDecodesUnreachableWithReason() throws {
        let state = try decodeState([
            "type": "unreachable",
            "repo_remote_url": "git@github.com:brianduff/flunge.git",
            "reason": "Not authorized to read `brianduff/flunge`.",
        ])
        guard case .unreachable(let repo, let reason) = state else {
            return XCTFail("expected .unreachable, got \(state)")
        }
        XCTAssertEqual(repo, "git@github.com:brianduff/flunge.git")
        XCTAssertEqual(reason, "Not authorized to read `brianduff/flunge`.")
    }

    /// Rate limiting decodes to its own case rather than collapsing into
    /// `.unreachable` — the tab shows a different remedy (wait) for it.
    func testDecodesRateLimitedAsItsOwnCase() throws {
        let state = try decodeState([
            "type": "rate_limited",
            "repo_remote_url": "git@github.com:brianduff/flunge.git",
            "reason": "GitHub is rate-limiting this account.",
        ])
        guard case .rateLimited = state else {
            return XCTFail("expected .rateLimited, got \(state)")
        }
    }

    func testDecodesEmpty() throws {
        let state = try decodeState([
            "type": "empty",
            "repo_remote_url": "git@github.com:brianduff/flunge.git",
            "owner_repo": "brianduff/flunge",
            "git_ref": "abc1234",
        ])
        guard case .empty(_, let ownerRepo, let gitRef) = state else {
            return XCTFail("expected .empty, got \(state)")
        }
        XCTAssertEqual(ownerRepo, "brianduff/flunge")
        XCTAssertEqual(gitRef, "abc1234")
    }

    func testDecodesLoadedTree() throws {
        let state = try decodeState([
            "type": "loaded",
            "tree": [
                "repo_remote_url": "git@github.com:brianduff/flunge.git",
                "owner_repo": "brianduff/flunge",
                "branch": "main",
                "git_ref": "b95bd654ec91f84f70f62127ef8d53317bd52ebb",
                "fetched_at": "2026-07-23T12:00:00Z",
                "truncated": false,
                "entries": [
                    ["path": "docs/design-docs/backend-preview-environments.md", "size": 4096],
                    ["path": "README.md"],
                ],
            ],
        ])
        guard case .loaded(let tree) = state else {
            return XCTFail("expected .loaded, got \(state)")
        }
        XCTAssertEqual(tree.ownerRepo, "brianduff/flunge")
        XCTAssertEqual(tree.branch, "main")
        XCTAssertEqual(tree.gitRef, "b95bd654ec91f84f70f62127ef8d53317bd52ebb")
        XCTAssertEqual(tree.entries.count, 2)
        XCTAssertEqual(tree.entries[0].size, 4096)
        // `size` is optional on the wire — an entry without one still decodes.
        XCTAssertNil(tree.entries[1].size)
    }

    /// `truncated` carries `#[serde(default)]` on the Rust side, so an
    /// engine that predates the field omits it rather than sending null.
    func testDecodesTreeWithoutTruncatedField() throws {
        let state = try decodeState([
            "type": "loaded",
            "tree": [
                "repo_remote_url": "git@github.com:foo/bar.git",
                "owner_repo": "foo/bar",
                "branch": "main",
                "git_ref": "abc",
                "fetched_at": "2026-07-23T12:00:00Z",
                "entries": [],
            ],
        ])
        guard case .loaded(let tree) = state else {
            return XCTFail("expected .loaded, got \(state)")
        }
        XCTAssertFalse(tree.truncated)
    }

    func testUnknownStateTypeThrows() {
        XCTAssertThrowsError(try decodeState(["type": "who_knows"]))
    }

    func testDecodesDocContentBothWays() throws {
        let loaded = try JSONDecoder().decode(
            DesignDocContent.self,
            from: try JSONSerialization.data(withJSONObject: ["type": "loaded", "markdown": "# Title"])
        )
        XCTAssertEqual(loaded, .loaded(markdown: "# Title"))

        let failed = try JSONDecoder().decode(
            DesignDocContent.self,
            from: try JSONSerialization.data(withJSONObject: ["type": "failed", "reason": "Not Found"])
        )
        XCTAssertEqual(failed, .failed(reason: "Not Found"))
    }
}

final class DesignDocTreeBuilderTests: XCTestCase {
    private func tree(_ paths: [String], gitRef: String = "sha1") -> DesignDocTree {
        DesignDocTree(
            repoRemoteURL: "git@github.com:brianduff/flunge.git",
            ownerRepo: "brianduff/flunge",
            branch: "main",
            gitRef: gitRef,
            entries: paths.map { DesignDocEntry(path: $0, size: nil) },
            fetchedAt: "2026-07-23T12:00:00Z"
        )
    }

    /// The engine sends flat paths; the sidebar must show real nesting.
    /// `docs/design-docs/foo.md` becomes `docs › design-docs › foo.md`
    /// even though GitHub never sent a directory entry for either level.
    func testNestsFlatPathsIntoDirectories() {
        let nodes = DesignDocTreeBuilder.build(from: tree([
            "docs/design-docs/backend-preview-environments.md",
            "docs/design-docs/buildkite-release-pipeline.md",
            "docs/architecture.md",
            "README.md",
        ]))

        XCTAssertEqual(nodes.map(\.name), ["docs", "README.md"])

        guard let docs = nodes.first(where: { $0.name == "docs" }) else {
            return XCTFail("docs directory missing")
        }
        XCTAssertTrue(docs.isDirectory)
        XCTAssertNil(docs.docRef, "a directory is not openable")
        XCTAssertEqual(docs.children?.map(\.name), ["design-docs", "architecture.md"])

        guard let designDocs = docs.children?.first(where: { $0.name == "design-docs" }) else {
            return XCTFail("design-docs directory missing")
        }
        XCTAssertEqual(
            designDocs.children?.map(\.name),
            ["backend-preview-environments.md", "buildkite-release-pipeline.md"]
        )
    }

    /// Directories sort ahead of files, and each group sorts
    /// case-insensitively — matching every other file sidebar on the
    /// platform.
    func testDirectoriesSortBeforeFilesAndBothSortNaturally() {
        let nodes = DesignDocTreeBuilder.build(from: tree([
            "zeta.md",
            "Alpha.md",
            "beta/one.md",
            "Apple/two.md",
        ]))
        XCTAssertEqual(nodes.map(\.name), ["Apple", "beta", "Alpha.md", "zeta.md"])
    }

    /// Each leaf carries the full `(repo, path, ref)` triple — the only
    /// handle the app has on a document. There is no local path anywhere
    /// in the model.
    func testLeafNodesCarryTheFullDocumentTriple() {
        let nodes = DesignDocTreeBuilder.build(from: tree(
            ["docs/design-docs/cloudflare-pages-frontend-hosting.md"],
            gitRef: "b95bd654ec91f84f70f62127ef8d53317bd52ebb"
        ))
        guard let leaf = DesignDocTreeBuilder.find(
            id: "docs/design-docs/cloudflare-pages-frontend-hosting.md",
            in: nodes
        ) else {
            return XCTFail("leaf not found by id")
        }
        XCTAssertFalse(leaf.isDirectory)
        XCTAssertEqual(leaf.docRef?.repoRemoteURL, "git@github.com:brianduff/flunge.git")
        XCTAssertEqual(leaf.docRef?.path, "docs/design-docs/cloudflare-pages-frontend-hosting.md")
        XCTAssertEqual(leaf.docRef?.gitRef, "b95bd654ec91f84f70f62127ef8d53317bd52ebb")
        XCTAssertEqual(leaf.docRef?.fileName, "cloudflare-pages-frontend-hosting.md")
    }

    /// Node ids are full repo-relative paths, so two files sharing a
    /// basename in different directories stay distinct selections.
    func testSameBasenameInDifferentDirectoriesGetsDistinctIDs() {
        let nodes = DesignDocTreeBuilder.build(from: tree([
            "docs/plans/index.md",
            "docs/design-docs/index.md",
        ]))
        let ids = DesignDocTreeBuilder.directoryIDs(in: nodes)
        XCTAssertEqual(Set(ids), ["docs", "docs/plans", "docs/design-docs"])

        XCTAssertNotNil(DesignDocTreeBuilder.find(id: "docs/plans/index.md", in: nodes))
        XCTAssertNotNil(DesignDocTreeBuilder.find(id: "docs/design-docs/index.md", in: nodes))
    }

    func testEmptyEntriesProduceNoNodes() {
        XCTAssertTrue(DesignDocTreeBuilder.build(from: tree([])).isEmpty)
    }

    /// Every directory id is reported so the tab can expand the whole
    /// tree on first load — design docs live two or three levels deep,
    /// and a fully-collapsed root is not a useful starting view.
    func testDirectoryIDsCoversEveryLevel() {
        let nodes = DesignDocTreeBuilder.build(from: tree([
            "a/b/c/deep.md",
            "top.md",
        ]))
        XCTAssertEqual(Set(DesignDocTreeBuilder.directoryIDs(in: nodes)), ["a", "a/b", "a/b/c"])
    }

    func testFindReturnsNilForUnknownID() {
        let nodes = DesignDocTreeBuilder.build(from: tree(["a.md"]))
        XCTAssertNil(DesignDocTreeBuilder.find(id: "nope.md", in: nodes))
    }

    func testShortSHAAbbreviatesToSevenAndLeavesShortRefsAlone() {
        XCTAssertEqual(shortSHA("b95bd654ec91f84f70f62127ef8d53317bd52ebb"), "b95bd65")
        XCTAssertEqual(shortSHA("abc"), "abc")
    }
}

/// The reader pane's document-body state machine, exercised through
/// [[ChatViewModel]] since that is where fetched bodies live.
@MainActor
final class DesignDocSelectionTests: XCTestCase {
    /// A per-test socket path so the model never connects to (or
    /// contends with) a real running engine.
    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    private let ref = DesignDocRef(
        repoRemoteURL: "git@github.com:brianduff/flunge.git",
        path: "docs/design-docs/backend-preview-environments.md",
        gitRef: "sha1"
    )

    func testAppliedContentIsRetrievableByRef() {
        let model = makeModel()
        model.applyProductDesignDocContent(ref: ref, content: .loaded(markdown: "# Preview envs"))
        XCTAssertEqual(model.designDocContent(for: ref), .loaded(markdown: "# Preview envs"))
    }

    /// Bodies are keyed by the full triple, so a reply that lands after
    /// the operator moved on updates its own entry rather than
    /// overwriting the document currently on screen.
    func testLateReplyForAnotherDocDoesNotDisturbTheCurrentOne() {
        let model = makeModel()
        let other = DesignDocRef(
            repoRemoteURL: ref.repoRemoteURL,
            path: "docs/design-docs/buildkite-release-pipeline.md",
            gitRef: "sha1"
        )
        model.applyProductDesignDocContent(ref: ref, content: .loaded(markdown: "# First"))
        model.selectedDesignDocRef = ref

        model.applyProductDesignDocContent(ref: other, content: .loaded(markdown: "# Second"))

        XCTAssertEqual(model.selectedDesignDocRef, ref)
        XCTAssertEqual(model.designDocContent(for: ref), .loaded(markdown: "# First"))
        XCTAssertEqual(model.designDocContent(for: other), .loaded(markdown: "# Second"))
    }

    /// The same path at a different commit is a different document, so
    /// a stale body cannot leak across a refresh that moved HEAD.
    func testSamePathAtADifferentRefIsADistinctEntry() {
        let model = makeModel()
        let newer = DesignDocRef(repoRemoteURL: ref.repoRemoteURL, path: ref.path, gitRef: "sha2")
        model.applyProductDesignDocContent(ref: ref, content: .loaded(markdown: "# Old"))
        XCTAssertNil(model.designDocContent(for: newer))
    }

    func testFailedContentIsSurfacedRatherThanDropped() {
        let model = makeModel()
        model.applyProductDesignDocContent(ref: ref, content: .failed(reason: "Not Found"))
        XCTAssertEqual(model.designDocContent(for: ref), .failed(reason: "Not Found"))
    }

    func testListingReplyClearsTheLoadingFlag() {
        let model = makeModel()
        model.designDocsLoadingProductIDs.insert("prod_1")
        model.applyProductDesignDocsList(productID: "prod_1", state: .noRepoConfigured)
        XCTAssertFalse(model.isLoadingDesignDocs(productID: "prod_1"))
        XCTAssertEqual(model.designDocTreeByProductID["prod_1"], .noRepoConfigured)
    }
}

/// `MarkdownViewerView` is the SwiftUI root of the "Read full description"
/// window, a thin wrapper around Textual's `StructuredText`. This test is
/// the canary that the view still builds and lays out against a
/// representative description (paragraphs, fenced code, a table, a nested
/// list) so a Textual upgrade that breaks the style protocol fails here
/// rather than silently at runtime when a user clicks the affordance.
@MainActor
final class MarkdownViewerViewTests: XCTestCase {
    func testRendersRepresentativeDescription() {
        let source = """
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
          - nested two
        - another top
        """

        let view = MarkdownViewerView(title: "Read full description", source: source)
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 760, height: 640)
        hosting.layoutSubtreeIfNeeded()

        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
        XCTAssertGreaterThan(hosting.fittingSize.width, 0)
    }

    func testRendersEmptySourceWithoutCrashing() {
        let view = MarkdownViewerView(title: "Empty", source: "")
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 760, height: 640)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThanOrEqual(hosting.fittingSize.height, 0)
    }
}
