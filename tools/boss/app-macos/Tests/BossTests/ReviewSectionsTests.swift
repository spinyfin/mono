import XCTest
@testable import Boss

/// Coverage for the Review column's labelled sections. The engine classifies
/// each card (`Task.review_section`, see `classify_review_section`); the board
/// only groups by the key it is given. These tests pin that the column always
/// shows every card, in the fixed section order, with empty sections hidden
/// and each section's incoming card order preserved.
@MainActor
final class ReviewSectionsTests: XCTestCase {

    func testSectionsAppearInFixedOrderWithTheirCards() {
        let model = makeModel()
        model.choresByProductID = ["prod_test": [
            makeReviewItem(id: "blocked", section: .blocked),
            makeReviewItem(id: "pending", section: .pending),
            makeReviewItem(id: "ai", section: .aiReview),
            makeReviewItem(id: "revision", section: .inRevision),
            makeReviewItem(id: "ready", section: .ready),
        ]]

        let sections = model.workSections(in: .review)

        XCTAssertEqual(sections.map(\.title), ["Ready", "In Revision", "AI Review", "Pending", "Blocked"])
        XCTAssertEqual(
            sections.map { $0.items.map(\.id) },
            [["ready"], ["revision"], ["ai"], ["pending"], ["blocked"]]
        )
    }

    func testEmptySectionsAreHidden() {
        let model = makeModel()
        model.choresByProductID = ["prod_test": [
            makeReviewItem(id: "ready", section: .ready),
            makeReviewItem(id: "blocked", section: .blocked),
        ]]

        XCTAssertEqual(model.workSections(in: .review).map(\.title), ["Ready", "Blocked"])
    }

    func testEveryCardIsShownExactlyOnce() {
        let model = makeModel()
        let all = ReviewSection.allCases.enumerated().flatMap { index, section in
            [
                makeReviewItem(id: "a\(index)", section: section),
                makeReviewItem(id: "b\(index)", section: section),
            ]
        }
        model.choresByProductID = ["prod_test": all]

        let shown = model.workSections(in: .review).flatMap { $0.items.map(\.id) }
        XCTAssertEqual(shown.count, all.count)
        XCTAssertEqual(Set(shown), Set(all.map(\.id)))
        XCTAssertEqual(
            model.workItems(in: .review).count, all.count,
            "the column no longer filters: every Review card is in workItems"
        )
    }

    func testCardOrderWithinASectionIsPreserved() {
        let model = makeModel()
        // `reviewBoardSort` orders by creation time, newest first, so the
        // section must keep whatever order the column produced.
        var older = makeReviewItem(id: "older", section: .ready)
        older.createdAt = "2026-05-01T00:00:00Z"
        var newer = makeReviewItem(id: "newer", section: .ready)
        newer.createdAt = "2026-05-20T00:00:00Z"
        model.choresByProductID = ["prod_test": [older, newer]]

        let columnOrder = model.workItems(in: .review).map(\.id)
        let section = model.workSections(in: .review).first
        XCTAssertEqual(section?.items.map(\.id), columnOrder)
        XCTAssertEqual(columnOrder, ["newer", "older"])
    }

    func testPendingSectionIsOrderedByMostRecentActivityDescending() {
        let model = makeModel()
        func pending(_ id: String, updated: String, pr: String? = nil) -> WorkTask {
            var t = makeReviewItem(id: id, section: .pending)
            t.updatedAt = updated
            t.prUpdatedAt = pr
            return t
        }
        model.choresByProductID = ["prod_test": [
            pending("old", updated: "2026-05-01T00:00:00Z"),
            pending("newest-row", updated: "2026-05-20T00:00:00Z"),
            // Row is old but the PR moved most recently: it must lead.
            pending("pr-newer", updated: "2026-05-02T00:00:00Z", pr: "2026-05-25T00:00:00Z"),
            // PR older than the row: the row's time wins.
            pending("pr-older", updated: "2026-05-10T00:00:00Z", pr: "2026-05-03T00:00:00Z"),
            // Tie with "tie-a" on activity: broken on id.
            pending("tie-b", updated: "2026-05-15T00:00:00Z"),
            pending("tie-a", updated: "2026-05-04T00:00:00Z", pr: "2026-05-15T00:00:00Z"),
        ]]

        XCTAssertEqual(
            model.workSections(in: .review).first?.items.map(\.id),
            ["pr-newer", "newest-row", "tie-a", "tie-b", "pr-older", "old"]
        )
    }

    // MARK: - Cards without an engine section

    func testMissingSectionFaultIsDeduplicatedPerCardVersion() {
        var card = makeReviewItem(id: UUID().uuidString, section: nil)
        XCTAssertTrue(ReviewSectionDiagnostics.claimFaultReport(card))
        XCTAssertFalse(ReviewSectionDiagnostics.claimFaultReport(card))
        card.updatedAt = "2026-05-16T00:00:00Z"
        XCTAssertTrue(ReviewSectionDiagnostics.claimFaultReport(card))
        XCTAssertFalse(ReviewSectionDiagnostics.claimFaultReport(card))
        let other = makeReviewItem(id: UUID().uuidString, section: nil)
        XCTAssertTrue(ReviewSectionDiagnostics.claimFaultReport(other))
        card.updatedAt = "2026-05-15T00:00:00Z"
        XCTAssertFalse(ReviewSectionDiagnostics.claimFaultReport(card))
    }

    func testRepeatedMissingSectionsStillReachTheTestObserver() {
        let card = makeReviewItem(id: UUID().uuidString, section: nil)
        let reports = capturingMissingSectionReports {
            ReviewSectionDiagnostics.reportMissingSection(card)
            ReviewSectionDiagnostics.reportMissingSection(card)
        }
        XCTAssertEqual(reports, [card.id, card.id])
    }

    /// Documented fallback 1: the engine predates `review_section` (its
    /// payload had no such key, so `reviewSectionReported` is false). The
    /// card lands in Pending, unhidden, and is not a defect.
    func testCardFromAPreFieldEngineLandsInPendingWithoutAReport() {
        let model = makeModel()
        var card = makeReviewItem(id: "unclassified", section: nil)
        card.reviewSectionReported = false
        model.choresByProductID = ["prod_test": [card]]

        let reported = capturingMissingSectionReports {
            let sections = model.workSections(in: .review)
            XCTAssertEqual(sections.map(\.title), ["Pending"])
            XCTAssertEqual(sections.first?.items.map(\.id), ["unclassified"])
        }
        XCTAssertEqual(reported, [], "a pre-field engine is the legitimate fallback, not a defect")
    }

    /// A section-aware engine (the key was present, here as `null`) sent a
    /// Review-lane card with no section: a defect in some engine update
    /// path. The card is still shown — under Pending, never hidden, never
    /// reclassified by the client — but it is reported, which under XCTest
    /// is a test failure (see `BossTestsPrincipal`).
    func testSectionAwareEngineCardWithoutASectionIsReportedAndStillShown() {
        let model = makeModel()
        var card = makeReviewItem(id: "dropped", section: nil)
        card.reviewSectionReported = true
        model.choresByProductID = ["prod_test": [card]]

        let reported = capturingMissingSectionReports {
            let sections = model.workSections(in: .review)
            XCTAssertEqual(sections.map(\.title), ["Pending"])
            XCTAssertEqual(sections.first?.items.map(\.id), ["dropped"])
        }
        XCTAssertEqual(reported, ["dropped"])
    }

    /// Documented fallback 2: a card the user just dragged into Review. Its
    /// engine state still says Doing; only the optimistic override puts it
    /// in the column, and the engine's reply to the drop will bring the
    /// section. Not a defect.
    func testCardJustDraggedIntoReviewWithoutASectionIsNotReported() {
        let model = makeModel()
        var card = makeReviewItem(id: "dragged", section: nil)
        card.status = "active"
        card.reviewSectionReported = true
        model.choresByProductID = ["prod_test": [card]]
        model.optimisticColumnByTaskID["dragged"] = .review

        let reported = capturingMissingSectionReports {
            let sections = model.workSections(in: .review)
            XCTAssertEqual(sections.map(\.title), ["Pending"])
            XCTAssertEqual(sections.first?.items.map(\.id), ["dragged"])
        }
        XCTAssertEqual(reported, [])
    }

    #if BOSS_BAZEL_TEST_BUILD
    /// Under `bazel test`, the shard's Info.plist names `BossTestsPrincipal`
    /// and XCTest instantiates it before the first test, so the bundle-wide
    /// observer that turns a reported defect into `XCTFail` is in place.
    /// This pins that the plist wiring actually took effect in the built
    /// bundle; `swift test` has no such plist and skips this check.
    func testBundlePrincipalInstalledTheFailingObserver() {
        XCTAssertNotNil(
            ReviewSectionDiagnostics.onMissingSection,
            "BossTestsPrincipal did not run: check NSPrincipalClass in Tests/BossTests/Info.plist and infoplists in BUILD.bazel"
        )
    }
    #endif

    /// A card with a section is never reported, whatever the engine said
    /// about the key — the report is only for a section that is owed and
    /// missing.
    func testCardWithASectionIsNeverReported() {
        let model = makeModel()
        var card = makeReviewItem(id: "ready", section: .ready)
        card.reviewSectionReported = true
        model.choresByProductID = ["prod_test": [card]]

        let reported = capturingMissingSectionReports {
            XCTAssertEqual(model.workSections(in: .review).map(\.title), ["Ready"])
        }
        XCTAssertEqual(reported, [])
    }

    /// The wire distinction the fallback rests on: a section-aware engine
    /// always writes the key (`null` for a non-Review row), a pre-field
    /// engine omits it.
    func testParserRecordsWhetherTheEngineSentTheReviewSectionKey() throws {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var payload: [String: Any] = [
            "id": "task_1", "product_id": "prod_test", "kind": "chore", "name": "n", "description": "",
            "status": "in_review", "created_at": "2026-05-15T00:00:00Z", "updated_at": "2026-05-15T00:00:00Z",
        ]

        let absent = try XCTUnwrap(client.parseTask(payload))
        XCTAssertNil(absent.reviewSection)
        XCTAssertFalse(absent.reviewSectionReported, "no key: a pre-field engine")

        payload["review_section"] = NSNull()
        let null = try XCTUnwrap(client.parseTask(payload))
        XCTAssertNil(null.reviewSection)
        XCTAssertTrue(null.reviewSectionReported, "null: the engine classified the row and found no section")

        payload["review_section"] = "ready"
        let ready = try XCTUnwrap(client.parseTask(payload))
        XCTAssertEqual(ready.reviewSection, .ready)
        XCTAssertTrue(ready.reviewSectionReported)
    }

    func testReviewIsGroupedByStateEvenWhenBoardIsGroupedByProject() {
        let model = makeModel()
        model.workBoardGrouping = .project
        model.choresByProductID = ["prod_test": [
            makeReviewItem(id: "ready", section: .ready),
            makeReviewItem(id: "blocked", section: .blocked),
        ]]

        XCTAssertEqual(model.workSections(in: .review).map(\.title), ["Ready", "Blocked"])
    }

    func testSectionsAreCollapsibleAndDefaultExpanded() {
        let model = makeModel()
        model.choresByProductID = ["prod_test": [makeReviewItem(id: "ready", section: .ready)]]

        let section = model.workSections(in: .review).first
        XCTAssertEqual(section?.id, "review-ready")
        XCTAssertEqual(section?.isCollapsible, true)
        XCTAssertEqual(section?.defaultExpanded, true)
        XCTAssertNil(section?.groupKey, "a drop on a Review section names only the column")
    }

    func testOtherColumnsAreNotSectioned() {
        let model = makeModel()
        model.choresByProductID = ["prod_test": [makeBacklogTask(id: "backlog")]]

        XCTAssertEqual(model.workSections(in: .backlog).map(\.title), ["Backlog"])
    }

    func testReviewSectionWireValuesParse() {
        XCTAssertEqual(
            ["ready", "in_revision", "ai_review", "pending", "blocked"].compactMap(ReviewSection.init(rawValue:)),
            ReviewSection.allCases
        )
        XCTAssertNil(ReviewSection(rawValue: "something_new"))
    }

    // MARK: - Helpers

    /// Run `body` with `ReviewSectionDiagnostics.onMissingSection` replaced
    /// by a recorder, restoring the bundle-wide observer (the one that calls
    /// `XCTFail`) afterwards. Returns the ids reported.
    private func capturingMissingSectionReports(_ body: () -> Void) -> [String] {
        let previous = ReviewSectionDiagnostics.onMissingSection
        defer { ReviewSectionDiagnostics.onMissingSection = previous }
        var reported: [String] = []
        ReviewSectionDiagnostics.onMissingSection = { reported.append($0.id) }
        body()
        return reported
    }

    private func makeReviewItem(id: String, section: ReviewSection?) -> WorkTask {
        var task = WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: id,
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/org/repo/pull/1",
            deletedAt: nil,
            createdAt: "2026-05-15T00:00:00Z",
            updatedAt: "2026-05-15T00:00:00Z",
            autostart: true
        )
        task.reviewSection = section
        return task
    }

    private func makeBacklogTask(id: String) -> WorkTask {
        // `autostart: false` so a `"todo"` status task lands in Backlog, not
        // Doing (`WorkTask.boardColumn` routes `"todo"` + `autostart` to Doing).
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: id,
            description: "",
            status: "todo",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-15T00:00:00Z",
            updatedAt: "2026-05-15T00:00:00Z",
            autostart: false
        )
    }

    private func makeModel() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.products = [
            WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-13T00:00:00Z",
                updatedAt: "2026-05-13T00:00:00Z"
            )
        ]
        model.selectedWorkProductID = "prod_test"
        return model
    }
}
