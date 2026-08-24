import XCTest
@testable import Boss

/// Coverage for the per-task doc-link icon on work-item cards.
/// Any kind may carry a `docLinkState`; the engine resolves the task's
/// own `doc_*` columns and delivers a `ProjectDesignDocState` on
/// `WorkTask.docLinkState`. The card feeds that into the same
/// `ProjectDesignDocAffordancePresentation` design cards use, and taps
/// route to `openWorkItemDoc` / `openTaskDoc`.
@MainActor
final class InvestigationDocLinkAffordanceTests: XCTestCase {
    // MARK: - Presentation (icon renders from the per-task state)

    /// An investigation carrying a resolved `docLinkState` must produce a
    /// non-nil affordance presentation — the same `doc.text` icon a design
    /// card shows. This is the icon the Review-lane card renders.
    func testInvestigationWithResolvedDocLinkStateProducesDocIcon() {
        let task = makeInvestigation(docLinkState: resolvedState(rawContentURL: nil))
        let presentation = ProjectDesignDocAffordancePresentation.from(state: task.docLinkState!)
        XCTAssertEqual(presentation?.systemImage, "doc.text")
        XCTAssertEqual(presentation?.kind, .resolved)
    }

    /// An investigation with no per-task pointer carries `nil` —
    /// the card hides the affordance (parity with `.notSet`).
    func testInvestigationWithoutDocLinkStateHasNoState() {
        let task = makeInvestigation(docLinkState: nil)
        XCTAssertNil(task.docLinkState, "no pointer -> no doc-link state -> hidden affordance")
    }

    /// A non-investigation work item with a resolved per-task pointer
    /// must render the same card affordance as an investigation. The
    /// historical kind gate hid this icon on chores even when `set-doc`
    /// had written a pointer.
    func testChoreWithResolvedDocLinkStateShowsCardAffordance() {
        let task = makeWorkItem(kind: "chore", docLinkState: resolvedState(rawContentURL: nil))
        let ctx = WorkCardSnapshotContext(
            column: .review,
            designDocState: task.docLinkState
        )
        let snap = WorkCardSnapshot.build(task: task, context: ctx)
        XCTAssertTrue(
            snap.showsDesignDocAffordance,
            "a chore with a resolved per-task doc must show the card affordance"
        )
        let presentation = ProjectDesignDocAffordancePresentation.from(state: task.docLinkState!)
        XCTAssertEqual(presentation?.systemImage, "doc.text")
        XCTAssertEqual(presentation?.kind, .resolved)
    }

    /// `workItemDocState` must return a chore's per-task pointer so the
    /// selected-card detail popover (and the kanban snapshot builder)
    /// agree with the card icon.
    func testWorkItemDocStateReturnsChorePerTaskPointer() {
        let model = makeModel()
        let task = makeWorkItem(kind: "chore", docLinkState: resolvedState(rawContentURL: nil))
        XCTAssertEqual(model.workItemDocState(for: task), task.docLinkState)
    }

    // MARK: - openTaskDoc dispatch

    /// A resolved state with a `rawContentURL` (the in-review PR-head-branch
    /// case) opens the async markdown viewer immediately and fetches the
    /// content — never a local file. Mirrors the design path's behaviour.
    func testOpenTaskDocResolvedWithRawContentURLOpensViewer() async {
        let model = makeModel()
        var openedLocalFiles: [URL] = []
        model.urlOpener = { if $0.isFileURL { openedLocalFiles.append($0) } }
        var asyncWindowOpens = 0
        model.asyncMarkdownViewerOpener = { asyncWindowOpens += 1 }
        let rawURL = "https://raw.githubusercontent.com/spinyfin/mono/docs/investigations/x.md?ref=boss%2Fexec_1"
        let task = makeInvestigation(docLinkState: resolvedState(rawContentURL: rawURL))
        model.openTaskDoc(task)

        XCTAssertEqual(asyncWindowOpens, 1, "viewer window must open immediately on click")
        if case .loading = model.asyncMarkdownViewerVM.state {} else {
            XCTFail("expected .loading immediately after click; got \(model.asyncMarkdownViewerVM.state)")
        }
        model.applyProductDesignDocContent(
            ref: DesignDocRef(
                repoRemoteURL: "git@github.com:spinyfin/mono.git",
                path: "docs/investigations/x.md",
                gitRef: "boss/exec_1"
            ),
            content: .loaded(markdown: "# Investigation")
        )
        XCTAssertTrue(openedLocalFiles.isEmpty, "must not open a local file when rawContentURL is present")
        XCTAssertNil(model.workErrorMessage)
        if case .loaded(_, _, let artifact) = model.asyncMarkdownViewerVM.state {
            XCTAssertEqual(
                artifact,
                CommentArtifactRef.prDoc(
                    repoRemoteURL: "git@github.com:spinyfin/mono.git",
                    branch: "boss/exec_1",
                    path: "docs/investigations/x.md"
                ),
                "investigation doc-link comments must be engine-backed via the resolved repo/branch/path"
            )
        } else {
            XCTFail("expected .loaded state after fetch; got \(model.asyncMarkdownViewerVM.state)")
        }
    }

    /// A resolved state with no `rawContentURL` (non-GitHub repo / older
    /// engine) falls back to opening the GitHub web URL.
    func testOpenTaskDocResolvedWithoutRawContentFallsBackToWebURL() {
        let model = makeModel()
        var openedURLs: [URL] = []
        model.urlOpener = { openedURLs.append($0) }
        let task = makeInvestigation(docLinkState: resolvedState(rawContentURL: nil))
        model.openTaskDoc(task)
        XCTAssertEqual(
            openedURLs.map(\.absoluteString),
            ["https://github.com/spinyfin/mono/blob/main/docs/investigations/x.md"]
        )
        XCTAssertNil(model.workErrorMessage)
    }

    /// A broken pointer surfaces the engine's reason as a work error so the
    /// user can act on it rather than getting a silent no-op.
    func testOpenTaskDocBrokenSurfacesError() {
        let model = makeModel()
        let task = makeInvestigation(docLinkState: .broken(reason: "no repo to resolve against"))
        model.openTaskDoc(task)
        XCTAssertEqual(model.workErrorMessage, "Doc pointer is broken: no repo to resolve against")
    }

    /// A nil / `.notSet` state is a no-op — the affordance should not have
    /// been clickable, but the dispatcher holds the line either way.
    func testOpenTaskDocWithoutStateIsNoOp() {
        let model = makeModel()
        model.openTaskDoc(makeInvestigation(docLinkState: nil))
        model.openTaskDoc(makeInvestigation(docLinkState: .notSet))
        XCTAssertNil(model.workErrorMessage)
    }

    // MARK: - Helpers

    private func resolvedState(rawContentURL: String?) -> ProjectDesignDocState {
        .resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "git@github.com:spinyfin/mono.git",
                branch: rawContentURL == nil ? "main" : "boss/exec_1",
                path: "docs/investigations/x.md",
                kind: .sameProduct(productID: "prod_test")
            ),
            workspacePath: nil,
            webURL: "https://github.com/spinyfin/mono/blob/main/docs/investigations/x.md",
            rawContentURL: rawContentURL
        )
    }

    private func makeInvestigation(docLinkState: ProjectDesignDocState?) -> WorkTask {
        makeWorkItem(kind: "investigation", docLinkState: docLinkState)
    }

    private func makeWorkItem(kind: String, docLinkState: ProjectDesignDocState?) -> WorkTask {
        WorkTask(
            id: "task_\(kind)",
            productID: "prod_test",
            projectID: nil,
            kind: kind,
            name: "\(kind) with a doc",
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/spinyfin/mono/pull/1506",
            deletedAt: nil,
            createdAt: "2026-06-14T00:00:00Z",
            updatedAt: "2026-06-14T00:00:00Z",
            docLinkState: docLinkState
        )
    }

    private func makeModel() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        // Trap the production opener so a missing stub can't pop the browser.
        model.urlOpener = { url in
            XCTFail("urlOpener was invoked with \(url) — install a recording stub first.")
        }
        return model
    }
}
