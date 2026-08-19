import XCTest
@testable import Boss

/// Covers `ChatViewModel.openTaskDescription`'s collapsed-by-default wiring:
/// only a `kind == "revision"` task's description opts the "HARD RULE"
/// heading into `MarkdownDocumentChrome.collapsedByDefaultHeadings` — every
/// other task/chore kind (and the design-doc fetch path) renders exactly as
/// it always has. See `MarkdownDocumentChromeTests` for the chunking logic
/// this wiring feeds into.
@MainActor
final class RevisionBriefCollapsedHeadingsTests: XCTestCase {
    func testRevisionTaskCollapsesHardRuleHeadingByDefault() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var windowOpens = 0
        model.asyncMarkdownViewerOpener = { windowOpens += 1 }
        let task = makeTask(
            kind: "revision",
            description: "Automated PR review found 1 finding(s).\n\n## HARD RULE: no punting — do the actual work\n\n..."
        )

        model.openTaskDescription(task)

        XCTAssertEqual(windowOpens, 1)
        XCTAssertEqual(
            model.asyncMarkdownViewerVM.collapsedByDefaultHeadings,
            [RevisionBriefCollapsibleHeadings.hardRule]
        )
        if case .loaded(let title, let markdown, let artifact) = model.asyncMarkdownViewerVM.state {
            XCTAssertEqual(title, task.name)
            XCTAssertEqual(markdown, task.description)
            XCTAssertEqual(artifact, .workItem(id: task.id))
        } else {
            XCTFail("expected .loaded state; got \(model.asyncMarkdownViewerVM.state)")
        }
    }

    func testNonRevisionTaskDoesNotCollapseAnyHeading() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.asyncMarkdownViewerOpener = {}
        let task = makeTask(kind: "chore", description: "# Some chore\n\nDo the thing.")

        model.openTaskDescription(task)

        XCTAssertTrue(model.asyncMarkdownViewerVM.collapsedByDefaultHeadings.isEmpty)
    }

    /// The VM/window are a shared singleton across opens — a stale
    /// collapsed-heading set from a previously-viewed revision brief must
    /// not leak into a subsequently-viewed non-revision task's description.
    func testCollapsedHeadingsResetsWhenSwitchingFromRevisionToNonRevisionTask() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.asyncMarkdownViewerOpener = {}
        let revision = makeTask(kind: "revision", description: "## HARD RULE: no punting — do the actual work\n")
        model.openTaskDescription(revision)
        XCTAssertFalse(model.asyncMarkdownViewerVM.collapsedByDefaultHeadings.isEmpty)

        let chore = makeTask(kind: "chore", description: "# Some chore")
        model.openTaskDescription(chore)
        XCTAssertTrue(model.asyncMarkdownViewerVM.collapsedByDefaultHeadings.isEmpty)
    }

    // MARK: - Helpers

    private func makeTask(kind: String, description: String) -> WorkTask {
        WorkTask(
            id: "task_1",
            productID: "prod_test",
            projectID: "proj_test",
            kind: kind,
            name: "Test task",
            description: description,
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z"
        )
    }
}
