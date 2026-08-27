import XCTest
@testable import Boss

/// Presentation contract for the background-work toolbar button and
/// read-only popover: zero/one/many, the 99+ cap, accessibility copy,
/// disabled-flag emptiness, planner elapsed rendering, conflict elapsed
/// omission, and automatic dismissal after the last completion. The
/// SwiftUI views are a thin reflection of these fields.
final class BackgroundWorkToolbarTests: XCTestCase {

    // MARK: - Visibility and badge

    func testZeroCountHidesTheButtonAndBadge() {
        XCTAssertFalse(BackgroundWorkToolbarChrome.isVisible(count: 0))
        XCTAssertNil(BackgroundWorkToolbarChrome.badgeText(count: 0))
        XCTAssertTrue(
            BackgroundWorkToolbarChrome.shouldDismissPopover(visibleCount: 0),
            "an empty snapshot must dismiss rather than show an empty popover"
        )
    }

    /// Mechanical feature flags default off; the engine then contributes
    /// zero conflict items via a real `ListEngineAttempts` reply carrying
    /// an empty background-work array. The chrome must stay hidden — the
    /// same empty snapshot as "nothing is running", not a disabled/empty
    /// popover. Drives the actual model path rather than a literal
    /// empty array, so a regression in the apply/publish wiring would
    /// fail this test.
    @MainActor
    func testDisabledMechanicalFlagsKeepTheButtonHidden() {
        let model = ChatViewModel(socketPath: "/tmp/boss-bgwork-toolbar-test-\(UUID().uuidString).sock")
        XCTAssertTrue(model.applyBackgroundWorkSnapshot([], generation: 1))
        XCTAssertEqual(model.backgroundWorkVisibleCount, 0)
        XCTAssertFalse(BackgroundWorkToolbarChrome.isVisible(count: model.backgroundWorkVisibleCount))
        XCTAssertTrue(BackgroundWorkToolbarChrome.rows(
            items: model.backgroundWork,
            projectName: { _ in nil },
            workItemName: { _ in nil },
            now: Date()
        ).isEmpty)
    }

    func testOneCountShowsUncappedBadgeAndSingularAccessibility() {
        XCTAssertTrue(BackgroundWorkToolbarChrome.isVisible(count: 1))
        XCTAssertEqual(BackgroundWorkToolbarChrome.badgeText(count: 1), "1")
        XCTAssertEqual(
            BackgroundWorkToolbarChrome.accessibilityLabel(count: 1),
            "1 background operation running"
        )
        XCTAssertFalse(BackgroundWorkToolbarChrome.shouldDismissPopover(visibleCount: 1))
    }

    func testManyCountShowsUncappedBadgeAndPluralAccessibility() {
        XCTAssertEqual(BackgroundWorkToolbarChrome.badgeText(count: 2), "2")
        XCTAssertEqual(
            BackgroundWorkToolbarChrome.accessibilityLabel(count: 2),
            "2 background operations running"
        )
        XCTAssertEqual(BackgroundWorkToolbarChrome.badgeText(count: 99), "99")
        XCTAssertEqual(
            BackgroundWorkToolbarChrome.accessibilityLabel(count: 99),
            "99 background operations running"
        )
    }

    func testCountAboveCapShows99PlusButAccessibilityKeepsTheTrueCount() {
        XCTAssertEqual(BackgroundWorkToolbarChrome.badgeText(count: 100), "99+")
        XCTAssertEqual(
            BackgroundWorkToolbarChrome.accessibilityLabel(count: 100),
            "100 background operations running"
        )
        XCTAssertTrue(BackgroundWorkToolbarChrome.isVisible(count: 100))
    }

    func testCompletionWhileOpenDismissesOnlyOnceTheSnapshotIsEmpty() {
        XCTAssertFalse(BackgroundWorkToolbarChrome.shouldDismissPopover(visibleCount: 2))
        XCTAssertFalse(BackgroundWorkToolbarChrome.shouldDismissPopover(visibleCount: 1))
        XCTAssertTrue(BackgroundWorkToolbarChrome.shouldDismissPopover(visibleCount: 0))
    }

    // MARK: - Elapsed time

    func testPlannerEpochStartedAtRendersElapsed() {
        let now = Date(timeIntervalSince1970: 1_000_000_000 + 90)
        XCTAssertEqual(BackgroundWorkElapsed.text(startedAt: "1000000000", now: now), "1m")
    }

    func testPlannerIsoStartedAtRendersElapsed() {
        let now = WorkerStaleness.parse("2026-08-26T12:01:30Z")!
        XCTAssertEqual(
            BackgroundWorkElapsed.text(startedAt: "2026-08-26T12:00:00Z", now: now),
            "1m"
        )
    }

    func testMissingOrUnparseableStartedAtOmitsElapsed() {
        let now = Date()
        XCTAssertNil(BackgroundWorkElapsed.text(startedAt: nil, now: now))
        XCTAssertNil(BackgroundWorkElapsed.text(startedAt: "", now: now))
        XCTAssertNil(BackgroundWorkElapsed.text(startedAt: "not-a-timestamp", now: now))
    }

    func testConflictItemOmitsElapsedRatherThanFabricatingAttemptAge() {
        let item = makeItem(
            id: "conflict_remediation:crz_1",
            kind: .conflictRemediation,
            title: "Conflict remediation",
            phase: "Rebasing Chore",
            projectID: nil,
            startedAt: nil,
            workItemID: "task_1"
        )
        let row = BackgroundWorkRowPresentation.from(
            item: item,
            projectName: nil,
            workItemName: "Chore",
            now: Date()
        )
        XCTAssertNil(row.elapsed)
        XCTAssertEqual(row.phase, "Rebasing Chore")
        XCTAssertEqual(row.context, "Chore")
    }

    // MARK: - Rows

    func testPlannerRowUsesEngineTitlePhaseProjectContextAndElapsed() {
        let item = makeItem(
            id: "project_planner:run_1",
            kind: .projectPlanner,
            title: "Project planner",
            phase: "Planning Alpha",
            projectID: "proj_1",
            startedAt: "1000000000",
            workItemID: nil
        )
        let now = Date(timeIntervalSince1970: 1_000_000_000 + 45)
        let row = BackgroundWorkRowPresentation.from(
            item: item,
            projectName: "Alpha",
            workItemName: nil,
            now: now
        )
        XCTAssertEqual(row.title, "Project planner")
        XCTAssertEqual(row.context, "Alpha")
        XCTAssertEqual(row.phase, "Planning Alpha")
        XCTAssertEqual(row.elapsed, "45s")
    }

    func testContextFallsBackToTheEngineSuppliedIdWhenTheNameIsUnknown() {
        let planner = makeItem(
            id: "project_planner:run_1",
            kind: .projectPlanner,
            title: "Project planner",
            phase: "Planning proj_1",
            projectID: "proj_1",
            startedAt: nil,
            workItemID: nil
        )
        let plannerRow = BackgroundWorkRowPresentation.from(
            item: planner,
            projectName: nil,
            workItemName: nil,
            now: Date()
        )
        XCTAssertEqual(plannerRow.context, "proj_1")

        let conflict = makeItem(
            id: "conflict_remediation:crz_1",
            kind: .conflictRemediation,
            title: "Conflict remediation",
            phase: "Applying deterministic resolution",
            projectID: nil,
            startedAt: nil,
            workItemID: "task_1"
        )
        let conflictRow = BackgroundWorkRowPresentation.from(
            item: conflict,
            projectName: nil,
            workItemName: nil,
            now: Date()
        )
        XCTAssertEqual(conflictRow.context, "task_1")
    }

    func testRowsPreserveEngineOrderAndUnknownKinds() {
        let items = [
            makeItem(
                id: "project_planner:run_1",
                kind: .projectPlanner,
                title: "Project planner",
                phase: "Planning Alpha",
                projectID: "proj_1",
                startedAt: "1000000000",
                workItemID: nil
            ),
            makeItem(
                id: "conflict_remediation:crz_1",
                kind: .conflictRemediation,
                title: "Conflict remediation",
                phase: "Rebasing Chore",
                projectID: nil,
                startedAt: nil,
                workItemID: "task_1"
            ),
            makeItem(
                id: "future_worker:src_1",
                kind: .unknown("future_worker"),
                title: "Future work",
                phase: "Working",
                projectID: nil,
                startedAt: nil,
                workItemID: nil
            ),
        ]
        let rows = BackgroundWorkToolbarChrome.rows(
            items: items,
            projectName: { $0 == "proj_1" ? "Alpha" : nil },
            workItemName: { $0 == "task_1" ? "Chore" : nil },
            now: Date(timeIntervalSince1970: 1_000_000_000 + 15)
        )
        XCTAssertEqual(rows.map(\.id), items.map(\.id))
        XCTAssertEqual(rows.count, items.count)
        XCTAssertEqual(rows[0].elapsed, "15s")
        XCTAssertNil(rows[1].elapsed)
        XCTAssertNil(rows[2].elapsed)
        XCTAssertNil(rows[2].context)
    }

    func testNoContextWhenTheEngineSuppliedNeitherProjectNorWorkItem() {
        let item = makeItem(
            id: "future_worker:src_1",
            kind: .unknown("future_worker"),
            title: "Future work",
            phase: "Working",
            projectID: nil,
            startedAt: nil,
            workItemID: nil
        )
        let row = BackgroundWorkRowPresentation.from(
            item: item,
            projectName: "ignored",
            workItemName: "ignored",
            now: Date()
        )
        XCTAssertNil(row.context)
        XCTAssertNil(row.elapsed)
    }

    // MARK: - Helpers

    private func makeItem(
        id: String,
        kind: BackgroundWorkKind,
        title: String,
        phase: String,
        projectID: String?,
        startedAt: String?,
        workItemID: String?
    ) -> BackgroundWorkItem {
        BackgroundWorkItem(
            id: id,
            kind: kind,
            phase: phase,
            productID: "prod_1",
            sourceID: "src_\(id)",
            title: title,
            projectID: projectID,
            startedAt: startedAt,
            workItemID: workItemID
        )
    }
}
