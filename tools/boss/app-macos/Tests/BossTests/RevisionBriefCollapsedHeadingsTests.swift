import XCTest
@testable import Boss

/// Covers `ChatViewModel.openTaskDescription`'s collapsed-by-default wiring:
/// a description that carries the engine's `## HARD RULE ...` boilerplate
/// heading — whatever the work item's kind — opts that heading into
/// `MarkdownDocumentChrome.collapsedByDefaultHeadings`; every other
/// description (and the design-doc fetch path) renders exactly as it always
/// has. Both engine-produced shapes are pinned here: the pre-merge revision
/// brief and the post-merge follow-up with its provenance preamble. See
/// `MarkdownDocumentChromeTests` for the chunking logic this wiring feeds
/// into.
@MainActor
final class RevisionBriefCollapsedHeadingsTests: XCTestCase {
    /// The pre-merge shape: `render_revision_instructions` output stored
    /// verbatim as a `revision` task's description.
    private static let preMergeRevisionBrief = """
    Automated PR review of PR #117 found 5 finding(s) requiring attention.
    Address ALL findings before finalising this revision.

    ## HARD RULE: no punting — do the actual work

    Each finding below requires a real code change that resolves it.

    ### [medium] Off-by-one in the pager

    **File:** `src/pager.rs`

    Fix the bound.
    """

    /// The post-merge shape: `render_post_merge_followup_provenance`
    /// prepended to the same rendering (with the origin work-item short id
    /// stripped), stored as a `followup`. The preamble is ordinary prose
    /// ahead of the boilerplate — it must not affect detection.
    private static let postMergeFollowupBrief = """
    **Provenance:** these findings were found in post-merge review of https://github.com/org/repo/pull/117.

    The PR description you open for this follow-up MUST state explicitly that these findings were identified during a post-merge review of https://github.com/org/repo/pull/117, with a link to that PR, so a reader of the follow-up PR can tell where it came from. For example: "Found in post-merge review of https://github.com/org/repo/pull/117."

    Automated PR review of PR #117 found 5 finding(s) requiring attention.
    Address ALL findings before finalising this revision.

    ## HARD RULE: no punting — do the actual work

    Each finding below requires a real code change that resolves it.

    ### [medium] Off-by-one in the pager

    **File:** `src/pager.rs`

    Fix the bound.
    """

    func testRevisionTaskCollapsesHardRuleHeadingByDefault() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var windowOpens = 0
        model.asyncMarkdownViewerOpener = { windowOpens += 1 }
        let task = makeTask(kind: "revision", description: Self.preMergeRevisionBrief)

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

    /// The engine's post-merge follow-up is a `followup` (never a
    /// `revision`) whose description prepends a provenance preamble to the
    /// same boilerplate. It must collapse exactly like the pre-merge brief:
    /// detection keys on the heading line, not on kind or leading prose.
    func testPostMergeFollowupWithProvenancePreambleCollapsesHardRuleHeadingByDefault() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.asyncMarkdownViewerOpener = {}
        let task = makeTask(kind: "followup", description: Self.postMergeFollowupBrief)

        model.openTaskDescription(task)

        XCTAssertEqual(
            model.asyncMarkdownViewerVM.collapsedByDefaultHeadings,
            [RevisionBriefCollapsibleHeadings.hardRule]
        )
        let chunks = MarkdownHeadingSections.chunks(
            in: task.description,
            collapsibleHeadings: model.asyncMarkdownViewerVM.collapsedByDefaultHeadings
        )
        let folded = chunks.compactMap { chunk -> String? in
            if case .collapsible(let heading, _) = chunk { return heading }
            return nil
        }
        XCTAssertEqual(folded, [RevisionBriefCollapsibleHeadings.hardRule], "\(chunks)")
        guard case .plain(let preamble) = chunks.first else {
            return XCTFail("expected the provenance preamble as a leading .plain chunk; got \(chunks)")
        }
        XCTAssertTrue(preamble.hasPrefix("**Provenance:**"), "the preamble stays visible ahead of the fold")
        XCTAssertTrue(
            chunks.contains { if case .plain(let text) = $0 { return text.hasPrefix("### [medium]") } else { return false } },
            "findings must never fold: \(chunks)"
        )
    }

    /// The same heading collapses for revision, followup, chore, and task kinds.
    func testHardRuleCollapseIsIndependentOfTaskKind() {
        for kind in ["revision", "followup", "chore", "task"] {
            let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
            model.asyncMarkdownViewerOpener = {}
            model.openTaskDescription(makeTask(kind: kind, description: Self.postMergeFollowupBrief))
            XCTAssertEqual(
                model.asyncMarkdownViewerVM.collapsedByDefaultHeadings,
                [RevisionBriefCollapsibleHeadings.hardRule],
                "kind \(kind) must collapse the boilerplate"
            )
        }
    }

    func testNonRevisionTaskDoesNotCollapseAnyHeading() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.asyncMarkdownViewerOpener = {}
        let task = makeTask(kind: "chore", description: "# Some chore\n\nDo the thing.")

        model.openTaskDescription(task)

        XCTAssertTrue(model.asyncMarkdownViewerVM.collapsedByDefaultHeadings.isEmpty)
    }

    /// A `revision` whose description lacks the boilerplate (rewritten by
    /// hand, or never engine-minted) has nothing to fold; prose mentions
    /// and fenced-code look-alikes do not count.
    func testRevisionWithoutHardRuleHeadingDoesNotCollapseAnyHeading() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.asyncMarkdownViewerOpener = {}
        let description = """
        Please rework the pager. Remember the HARD RULE: no punting — do the actual work.

        ```
        ## HARD RULE: no punting — do the actual work
        ```
        """
        model.openTaskDescription(makeTask(kind: "revision", description: description))

        XCTAssertTrue(model.asyncMarkdownViewerVM.collapsedByDefaultHeadings.isEmpty)
    }

    /// The VM/window are a shared singleton across opens — a stale
    /// collapsed-heading set from a previously-viewed revision brief must
    /// not leak into a subsequently-viewed non-revision task's description.
    func testCollapsedHeadingsResetsWhenSwitchingFromRevisionToNonRevisionTask() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.asyncMarkdownViewerOpener = {}
        let revision = makeTask(kind: "revision", description: Self.preMergeRevisionBrief)
        model.openTaskDescription(revision)
        XCTAssertFalse(model.asyncMarkdownViewerVM.collapsedByDefaultHeadings.isEmpty)

        let chore = makeTask(kind: "chore", description: "# Some chore")
        model.openTaskDescription(chore)
        XCTAssertTrue(model.asyncMarkdownViewerVM.collapsedByDefaultHeadings.isEmpty)
    }

    /// `openReviewGuide` clears `pendingAsyncViewerRef` so a late design-doc
    /// reply cannot overwrite a guide — `openTaskDescription` is the sibling
    /// identity-guard site for this shared singleton window and must clear
    /// the same field, or a design doc opened earlier and still in flight
    /// can land after `openTaskDescription` and overwrite the description
    /// the user is now looking at.
    func testOpenTaskDescriptionClearsPendingDesignDocIdentity() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.asyncMarkdownViewerOpener = {}
        let ref = DesignDocRef(repoRemoteURL: "git@github.com:x/y.git", path: "docs/plan.md", gitRef: "main")
        model.openDesignDocViaEngine(ref: ref, title: "Plan", artifact: nil, projectShortID: "42")
        XCTAssertEqual(model.pendingAsyncViewerRef, ref)

        let task = makeTask(kind: "chore", description: "# Task description")
        model.openTaskDescription(task)
        XCTAssertNil(model.pendingAsyncViewerRef)

        model.applyProductDesignDocContent(ref: ref, content: .loaded(markdown: "# Late design doc reply"))

        if case .loaded(let title, let markdown, _) = model.asyncMarkdownViewerVM.state {
            XCTAssertEqual(title, task.name)
            XCTAssertEqual(markdown, task.description, "a late design-doc reply must not overwrite the task description")
        } else {
            XCTFail("expected the task description to still be showing; got \(model.asyncMarkdownViewerVM.state)")
        }
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
