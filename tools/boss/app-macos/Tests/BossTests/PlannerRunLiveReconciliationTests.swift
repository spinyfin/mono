import XCTest
@testable import Boss

/// The project-card hourglass shares running identity with the global
/// background-work snapshot (`source_id` = `planner_runs.id`) and keeps
/// the project-scoped `list_planner_runs` cache for staged/applied
/// history and popover detail. Design:
/// background-task-visibility-toolbar-affordance-for-engine-background-work.md
/// § "Polling and planner reconciliation".
@MainActor
final class PlannerRunLiveReconciliationTests: XCTestCase {

    // MARK: - Run discovered globally before project refresh

    /// The hourglass must appear from the snapshot alone so a planner
    /// that crossed the engine's anti-flicker gate is visible before
    /// `list_planner_runs` has returned for that project.
    func testGlobalRunSurfacesHourglassBeforeProjectRefresh() {
        let model = makeModel()
        let item = makePlannerItem(projectID: "proj_1", sourceID: "run_99")
        model.backgroundWork = [item]

        XCTAssertNil(
            model.latestPlannerRun(forProjectID: "proj_1"),
            "project-scoped history must stay empty until list_planner_runs replies"
        )
        XCTAssertTrue(model.plannerRuns(forProjectID: "proj_1").isEmpty)

        let presentation = model.plannerAffordancePresentation(forProjectID: "proj_1")
        XCTAssertEqual(presentation?.runID, "run_99")
        XCTAssertEqual(presentation?.isRunning, true)
        XCTAssertEqual(presentation?.systemImage, "hourglass")
        XCTAssertEqual(presentation?.tooltip, "Planner is running…")
        XCTAssertNil(
            model.plannerRunForAffordancePopover(forProjectID: "proj_1"),
            "popover detail waits for the cached row; the hourglass does not"
        )
        XCTAssertEqual(
            model.livePlannerBackgroundItem(forProjectID: "proj_1")?.sourceID,
            "run_99"
        )
    }

    /// Once the project-scoped row arrives, popover detail binds to the
    /// same identity the hourglass already claimed.
    func testProjectRefreshBindsPopoverToGloballyDiscoveredRun() {
        let model = makeModel()
        model.backgroundWork = [makePlannerItem(projectID: "proj_1", sourceID: "run_99")]

        model.applyEventForTest(.plannerRunsList(
            projectID: "proj_1",
            runs: [makeRun(id: "run_99", projectID: "proj_1", outcome: "running")]
        ))

        XCTAssertEqual(model.latestPlannerRun(forProjectID: "proj_1")?.id, "run_99")
        XCTAssertEqual(
            model.plannerRunForAffordancePopover(forProjectID: "proj_1")?.id,
            "run_99"
        )
        XCTAssertEqual(
            model.plannerAffordancePresentation(forProjectID: "proj_1")?.isRunning,
            true
        )
    }

    // MARK: - Completion arriving through either path

    /// Global snapshot drops the item (next poll after the run finishes)
    /// while the project cache still says `running`. The hourglass must
    /// go dark; history is not rewritten by the snapshot.
    func testCompletionViaGlobalSnapshotHidesHourglass() {
        let model = makeModel()
        let run = makeRun(id: "run_1", projectID: "proj_1", outcome: "running")
        model.plannerRunsByProjectID["proj_1"] = [run]
        model.backgroundWork = [makePlannerItem(projectID: "proj_1", sourceID: "run_1")]
        XCTAssertEqual(model.plannerAffordancePresentation(forProjectID: "proj_1")?.isRunning, true)

        model.backgroundWork = []

        XCTAssertNil(
            model.plannerAffordancePresentation(forProjectID: "proj_1"),
            "a cache-only running row is not current running state"
        )
        XCTAssertEqual(
            model.latestPlannerRun(forProjectID: "proj_1")?.outcome,
            "running",
            "project-scoped history stays until list_planner_runs refreshes"
        )
    }

    /// Same completion via the snapshot-replace API the poller uses, so
    /// a `limit = 0` reply that drops the planner item is enough on its
    /// own — the project-scoped list does not have to race it.
    func testCompletionViaAppliedSnapshotHidesHourglass() {
        let model = makeModel()
        model.plannerRunsByProjectID["proj_1"] = [
            makeRun(id: "run_1", projectID: "proj_1", outcome: "running"),
        ]
        XCTAssertTrue(
            model.applyBackgroundWorkSnapshot(
                [makePlannerItem(projectID: "proj_1", sourceID: "run_1")],
                generation: 1
            )
        )
        XCTAssertEqual(model.plannerAffordancePresentation(forProjectID: "proj_1")?.isRunning, true)

        XCTAssertTrue(model.applyBackgroundWorkSnapshot([], generation: 2))
        XCTAssertNil(model.plannerAffordancePresentation(forProjectID: "proj_1"))
        XCTAssertEqual(model.latestPlannerRun(forProjectID: "proj_1")?.outcome, "running")
    }

    /// Project-scoped query returns a terminal outcome for the same id
    /// while the snapshot still lists the run. Completion through this
    /// path must hide the hourglass and show the staged/applied icon.
    func testCompletionViaProjectQueryHidesHourglass() {
        let model = makeModel()
        model.backgroundWork = [makePlannerItem(projectID: "proj_1", sourceID: "run_1")]
        XCTAssertEqual(model.plannerAffordancePresentation(forProjectID: "proj_1")?.isRunning, true)

        model.applyEventForTest(.plannerRunsList(
            projectID: "proj_1",
            runs: [makeRun(id: "run_1", projectID: "proj_1", outcome: "staged")]
        ))

        let presentation = model.plannerAffordancePresentation(forProjectID: "proj_1")
        XCTAssertEqual(presentation?.runID, "run_1")
        XCTAssertEqual(presentation?.isRunning, false)
        XCTAssertEqual(presentation?.systemImage, "tray.and.arrow.down.fill")
        XCTAssertEqual(model.latestPlannerRun(forProjectID: "proj_1")?.outcome, "staged")
        XCTAssertEqual(model.plannerRunForAffordancePopover(forProjectID: "proj_1")?.id, "run_1")
    }

    func testCompletionViaProjectQueryAppliedOutcome() {
        let presentation = PlannerRunAffordancePresentation.from(
            projectID: "proj_1",
            cachedRuns: [makeRun(id: "run_1", projectID: "proj_1", outcome: "applied")],
            backgroundWork: [makePlannerItem(projectID: "proj_1", sourceID: "run_1")]
        )
        XCTAssertEqual(presentation?.isRunning, false)
        XCTAssertEqual(presentation?.systemImage, "checkmark.circle")
    }

    // MARK: - Invariant: no conflicting running outcomes

    /// The hourglass running set equals the global planner items whose
    /// cached row is missing or still `running`. Project B cannot light
    /// an hourglass for project A's live run, and a cache-only running
    /// row cannot disagree with a snapshot that does not list it.
    func testHourglassAndGlobalSnapshotCannotDisagreeOnRunning() {
        let items = [
            makePlannerItem(projectID: "proj_a", sourceID: "run_a"),
            makePlannerItem(projectID: "proj_b", sourceID: "run_b"),
        ]
        let cached: [String: [PlannerRun]] = [
            "proj_a": [makeRun(id: "run_a", projectID: "proj_a", outcome: "running")],
            "proj_b": [],
            "proj_c": [makeRun(id: "run_c", projectID: "proj_c", outcome: "running")],
        ]
        let projectIDs = ["proj_a", "proj_b", "proj_c"]

        let hourglass = runningPairs(projectIDs: projectIDs, cached: cached, backgroundWork: items)
        let global = Set(items.compactMap { item -> String? in
            guard item.isProjectPlanner, let projectID = item.projectID else { return nil }
            if let cachedRun = cached[projectID]?.first(where: { $0.id == item.sourceID }),
               !cachedRun.isRunning {
                return nil
            }
            return "\(projectID):\(item.sourceID)"
        })

        XCTAssertEqual(hourglass, Set(["proj_a:run_a", "proj_b:run_b"]))
        XCTAssertEqual(hourglass, global)
        XCTAssertFalse(
            hourglass.contains("proj_c:run_c"),
            "a cache-only running row must not present as running when the snapshot omits it"
        )
        XCTAssertEqual(
            Set(hourglass.map { $0.split(separator: ":")[1] }).count,
            hourglass.count,
            "two planner indicators cannot claim running for the same run id"
        )
    }

    func testSameRunCannotShowRunningOnTwoProjects() {
        let items = [makePlannerItem(projectID: "proj_a", sourceID: "run_1")]
        let a = PlannerRunAffordancePresentation.liveRunningRunID(
            projectID: "proj_a",
            cachedRuns: [makeRun(id: "run_1", projectID: "proj_a", outcome: "running")],
            backgroundWork: items
        )
        let b = PlannerRunAffordancePresentation.liveRunningRunID(
            projectID: "proj_b",
            cachedRuns: [makeRun(id: "run_1", projectID: "proj_b", outcome: "running")],
            backgroundWork: items
        )
        XCTAssertEqual(a, "run_1")
        XCTAssertNil(
            b,
            "project B's cache cannot light a competing hourglass for project A's live run"
        )
    }

    func testGlobalIdentityWinsOverStaleCachedRunningRow() {
        let presentation = PlannerRunAffordancePresentation.from(
            projectID: "proj_1",
            cachedRuns: [makeRun(id: "run_old", projectID: "proj_1", outcome: "running")],
            backgroundWork: [makePlannerItem(projectID: "proj_1", sourceID: "run_new")]
        )
        XCTAssertEqual(presentation?.runID, "run_new")
        XCTAssertEqual(presentation?.isRunning, true)
        XCTAssertEqual(presentation?.systemImage, "hourglass")
    }

    func testConflictRemediationItemsDoNotDriveTheHourglass() {
        let presentation = PlannerRunAffordancePresentation.from(
            projectID: "proj_1",
            cachedRuns: [],
            backgroundWork: [
                BackgroundWorkItem(
                    id: "conflict_remediation:crz_1",
                    kind: .conflictRemediation,
                    phase: "Rebasing task",
                    productID: "prod_1",
                    sourceID: "crz_1",
                    title: "Conflict remediation",
                    projectID: "proj_1",
                    startedAt: nil,
                    workItemID: "task_1"
                ),
            ]
        )
        XCTAssertNil(presentation)
    }

    func testStagedHistoryStillComesFromProjectQuery() {
        let model = makeModel()
        let staged = makeRun(id: "run_done", projectID: "proj_1", outcome: "staged")
        model.plannerRunsByProjectID["proj_1"] = [staged]
        model.backgroundWork = []

        let presentation = model.plannerAffordancePresentation(forProjectID: "proj_1")
        XCTAssertEqual(presentation?.runID, "run_done")
        XCTAssertEqual(presentation?.isRunning, false)
        XCTAssertEqual(presentation?.systemImage, "tray.and.arrow.down.fill")
        XCTAssertEqual(model.latestPlannerRun(forProjectID: "proj_1")?.id, "run_done")
    }

    func testFailedHistoryStillComesFromProjectQuery() {
        let presentation = PlannerRunAffordancePresentation.from(
            projectID: "proj_1",
            cachedRuns: [makeRun(id: "run_fail", projectID: "proj_1", outcome: "planner_failed")],
            backgroundWork: []
        )
        XCTAssertEqual(presentation?.isRunning, false)
        XCTAssertEqual(presentation?.systemImage, "exclamationmark.circle")
        XCTAssertEqual(presentation?.tintKind, .failed)
    }

    // MARK: - Helpers

    private func runningPairs(
        projectIDs: [String],
        cached: [String: [PlannerRun]],
        backgroundWork: [BackgroundWorkItem]
    ) -> Set<String> {
        Set(projectIDs.compactMap { projectID in
            guard let runID = PlannerRunAffordancePresentation.liveRunningRunID(
                projectID: projectID,
                cachedRuns: cached[projectID] ?? [],
                backgroundWork: backgroundWork
            ) else { return nil }
            return "\(projectID):\(runID)"
        })
    }

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-planner-reconcile-\(UUID().uuidString).sock")
    }

    private func makePlannerItem(projectID: String, sourceID: String) -> BackgroundWorkItem {
        BackgroundWorkItem(
            id: "project_planner:\(sourceID)",
            kind: .projectPlanner,
            phase: "Planning \(projectID)",
            productID: "prod_1",
            sourceID: sourceID,
            title: "Project planner",
            projectID: projectID,
            startedAt: "2026-08-26T12:00:00Z",
            workItemID: nil
        )
    }

    private func makeRun(id: String, projectID: String, outcome: String) -> PlannerRun {
        PlannerRun(
            id: id,
            projectID: projectID,
            productID: "prod_1",
            designTaskID: nil,
            caller: "merge_trigger",
            docRef: nil,
            model: nil,
            inputSummary: nil,
            rawOutput: nil,
            effortAudit: nil,
            notes: nil,
            outcome: outcome,
            resultSummary: nil,
            createdAt: "2026-08-26T12:00:00Z",
            updatedAt: "2026-08-26T12:00:00Z"
        )
    }
}
