import XCTest
@testable import Boss

/// App side of parking (design section 1.6): the wire decode, the board's
/// default exclusion and the "Show parked" toggle, the chip and its
/// "finishing current run" sub-label, the sidebar treatment, and the
/// park / un-park requests the menus send.
@MainActor
final class ParkingUITests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_800_000_000)

    // MARK: - Decode

    func testTaskDecodesParkFields() {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let task = client.parseTask(taskPayload(extra: [
            "parked_at": "1799990000", "parked_by": "human",
            "parked_reason": "not now", "parked_via": "project:proj_a",
        ]))
        XCTAssertEqual(task?.parkedAt, "1799990000")
        XCTAssertEqual(task?.parkedBy, "human")
        XCTAssertEqual(task?.parkedReason, "not now")
        XCTAssertEqual(task?.parkedVia, "project:proj_a")
        XCTAssertEqual(task?.isParkedViaProject, true)
    }

    func testTaskWithoutParkFieldsIsNotParked() {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let task = client.parseTask(taskPayload())
        XCTAssertNil(task?.parkedAt)
        XCTAssertEqual(task?.hasOwnParkStamp, false)
    }

    func testProjectDecodesParkFields() {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let project = client.parseProject([
            "id": "proj_a", "product_id": "prod_test", "name": "A", "slug": "a", "description": "",
            "goal": "", "status": "active", "priority": "medium",
            "created_at": "2026-10-01T00:00:00Z", "updated_at": "2026-10-01T00:00:00Z",
            "parked_at": "1799990000", "parked_by": "human", "parked_reason": "later", "parked_via": "direct",
        ])
        XCTAssertEqual(project?.parkedReason, "later")
        XCTAssertEqual(project?.isParked, true)
    }

    /// A merged or archived row keeps its stamp as history but is finished
    /// work: it must neither hide from the board nor read as parked.
    func testFinishedRowsAreNeverParked() {
        var task = makeTask("t", status: "done")
        task.parkedAt = "1799990000"
        XCTAssertFalse(task.hasOwnParkStamp)
        var project = makeProject("p")
        project.parkedAt = "1799990000"
        project.status = "archived"
        XCTAssertFalse(project.isParked)
    }

    // MARK: - Board

    func testDefaultBoardLeavesParkedRowsOff() {
        let model = makeModel(tasks: [makeTask("open"), parked(makeTask("parked"))])
        XCTAssertEqual(visibleIDs(model), ["open"])
    }

    func testShowParkedAddsRowsWithoutHidingUnparkedOnes() {
        let model = makeModel(tasks: [makeTask("open"), parked(makeTask("parked"))])
        model.setShowParked(true)
        XCTAssertEqual(visibleIDs(model), ["open", "parked"])
    }

    func testShowParkedComposesWithBlockedOnly() {
        let model = makeModel(tasks: [
            makeTask("open"),
            makeTask("blocked", status: "blocked"),
            parked(makeTask("parked-todo")),
            parked(makeTask("parked-blocked", status: "blocked")),
        ])
        model.setShowParked(true)
        model.setShowBlockedOnly(true)
        XCTAssertEqual(visibleIDs(model), ["blocked", "parked-blocked"])
    }

    func testParkedLifecycleColumnIsKept() {
        let model = makeModel(tasks: [parked(makeTask("review", status: "in_review", pr: "https://github.com/o/r/pull/1"))])
        model.setShowParked(true)
        XCTAssertEqual(model.workItems(in: .review).map(\.id), ["review"])
    }

    /// A task in a parked project is parked even before its own stamp lands.
    func testTaskInParkedProjectIsParkedWithoutOwnStamp() {
        var project = makeProject("proj_test")
        project.parkedAt = "1799990000"
        project.parkedReason = "later"
        let model = makeModel(tasks: [makeTask("child")], project: project)
        XCTAssertTrue(model.isParked(model.task(withID: "child")!))
        XCTAssertEqual(visibleIDs(model), [])
    }

    func testShowParkedPersistsInDefaults() {
        let suite = "dev.spinyfin.boss.tests.parking.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock", defaults: defaults)
        model.setShowParked(true)
        let reloaded = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock", defaults: defaults)
        XCTAssertTrue(reloaded.showParked)
    }

    func testRevealOfParkedCardShowsParkedForTheSession() {
        let model = makeModel(tasks: [parked(makeTask("parked"))])
        XCTAssertEqual(visibleIDs(model), [])
        model.revealWorkCard("parked", productID: "prod_test")
        XCTAssertTrue(model.showParked)
        XCTAssertEqual(visibleIDs(model), ["parked"])
        XCTAssertEqual(model.revealScrollTarget, "parked")
    }

    // MARK: - Chip and sub-label

    func testChipReadsParkedRelativeTimeAndReason() {
        let chip = ParkedChipPresentation.make(
            parkedAt: String(Int(now.timeIntervalSince1970) - 3 * 3600),
            reason: "waiting on design review", viaProject: false, finishingCurrentRun: false, now: now
        )
        XCTAssertTrue(chip.text.hasPrefix("Parked · "), chip.text)
        XCTAssertTrue(chip.text.hasSuffix(" · waiting on design review"), chip.text)
        XCTAssertTrue(chip.text.contains("3"), chip.text)
        XCTAssertFalse(chip.finishingCurrentRun)
    }

    func testChipWithoutReasonOrParsableTimeStillSaysParked() {
        let chip = ParkedChipPresentation.make(
            parkedAt: "not-a-number", reason: "  ", viaProject: false, finishingCurrentRun: false, now: now
        )
        XCTAssertEqual(chip.text, "Parked")
    }

    func testChipTimeIsClampedWhenTheStampIsInTheFuture() {
        let chip = ParkedChipPresentation.make(
            parkedAt: String(Int(now.timeIntervalSince1970) + 600),
            reason: nil, viaProject: false, finishingCurrentRun: false, now: now
        )
        XCTAssertFalse(chip.text.contains("in "), chip.text)
    }

    func testParkedSnapshotCarriesChipAndFinishingSubLabelForLiveExecution() {
        let model = makeModel(
            tasks: [parked(makeTask("live", status: "active")), parked(makeTask("idle", status: "todo"))],
            runtimes: [
                WorkTaskRuntime(
                    workItemID: "live", executionStatus: "running", runStatus: "running", executionID: "exec_live",
                    dispatchRetryAt: nil, dispatchWaitReason: nil, dispatchWaitSince: nil
                ),
            ]
        )
        model.setShowParked(true)
        let live = snapshot(model, "live")
        let idle = snapshot(model, "idle")
        XCTAssertEqual(live.parked?.finishingCurrentRun, true)
        XCTAssertNotNil(idle.parked)
        XCTAssertEqual(idle.parked?.finishingCurrentRun, false)
    }

    func testUnparkedSnapshotHasNoChip() {
        let model = makeModel(tasks: [makeTask("open")])
        XCTAssertNil(snapshot(model, "open").parked)
    }

    // MARK: - Sidebar

    func testSidebarSubtitleAndImageForParkedProject() {
        var project = makeProject("p")
        project.shortID = nil
        XCTAssertNil(project.sidebarSubtitle)
        XCTAssertEqual(project.sidebarSubtitle(parkedItemCount: 2), "2 parked")
        project.parkedAt = "1799990000"
        XCTAssertEqual(project.sidebarSubtitle(parkedItemCount: 5), "Parked · 5 parked")
        XCTAssertEqual(project.sidebarSystemImage, "pause.circle")
    }

    func testParkMenuActionBranches() {
        typealias A = ParkMenuAction
        func decide(
            kind: String = "task", status: String = "todo", parked: Bool, direct: Bool = false, project: Bool = false
        ) -> A {
            A.decide(kind: kind, status: status, isParked: parked, hasDirectStamp: direct, projectParked: project)
        }
        XCTAssertEqual(decide(parked: false), .park)
        XCTAssertEqual(decide(status: "done", parked: false), .none)
        XCTAssertEqual(decide(kind: "revision", parked: false), .none)
        XCTAssertEqual(decide(parked: true, direct: true), .unpark)
        // Park task, then park its project: the engine refuses a per-row unpark.
        if case .unparkDisabled = decide(parked: true, direct: true, project: true) {} else { XCTFail() }
        if case .unparkDisabled = decide(parked: true, direct: false, project: true) {} else { XCTFail() }
        if case .unparkDisabled = decide(kind: "revision", parked: true, direct: true) {} else { XCTFail() }
    }

    func testMenuActionForDirectlyParkedTaskInParkedProject() {
        var project = makeProject("proj_test")
        project.parkedAt = "1799990000"
        let model = makeModel(tasks: [parked(makeTask("t"))], project: project)
        if case .unparkDisabled = model.parkMenuAction(for: model.task(withID: "t")!) {} else { XCTFail() }
    }

    func testParkedTasksDoNotCountAsActionableInTheSidebar() {
        let model = makeModel(tasks: [
            makeTask("a"), parked(makeTask("b")),
            parked(makeTask("blocked", status: "blocked", reason: "dependency")),
        ])
        XCTAssertEqual(model.unblockedTaskCount(forProjectID: "proj_test"), 1)
        XCTAssertEqual(model.blockedTaskCount(forProjectID: "proj_test"), 0)
        XCTAssertEqual(model.parkedTaskCount(forProjectID: "proj_test"), 2)
    }

    // MARK: - Requests

    func testParkSendsParkWorkItemWithReason() {
        let model = makeModel(tasks: [makeTask("t")])
        let recorder = installRecorder(on: model)
        model.requestPark(task: makeTask("t"))
        XCTAssertEqual(model.pendingParkRequest, ParkRequest(id: "t", name: "Task t", isProject: false))

        model.submitPark(model.pendingParkRequest!, reason: "not now")

        XCTAssertNil(model.pendingParkRequest)
        XCTAssertEqual(recorder.value.count, 1)
        let payload = recorder.value[0]
        XCTAssertEqual(payload["type"] as? String, "park_work_item")
        XCTAssertEqual(payload["id"] as? String, "t")
        XCTAssertEqual(payload["parked"] as? Bool, true)
        XCTAssertEqual(payload["reason"] as? String, "not now")
    }

    func testParkWithoutReasonOmitsTheKey() {
        let model = makeModel(tasks: [])
        let recorder = installRecorder(on: model)
        model.submitPark(ParkRequest(id: "proj_test", name: "P", isProject: true), reason: nil)
        XCTAssertNil(recorder.value.first?["reason"])
    }

    func testUnparkSendsParkedFalse() {
        let model = makeModel(tasks: [])
        let recorder = installRecorder(on: model)
        model.unpark(id: "t")
        XCTAssertEqual(recorder.value.first?["type"] as? String, "park_work_item")
        XCTAssertEqual(recorder.value.first?["parked"] as? Bool, false)
        XCTAssertNil(recorder.value.first?["reason"])
    }

    func testRevisionsAndFinishedWorkAreNotParkable() {
        let model = makeModel(tasks: [])
        let revision = makeTask("rev", kind: "revision")
        XCTAssertEqual(model.parkMenuAction(for: revision), .none)
        XCTAssertEqual(model.parkMenuAction(for: makeTask("done", status: "done")), .none)
        XCTAssertEqual(model.parkMenuAction(for: makeTask("open")), .park)
    }

    // MARK: - Helpers

    private func visibleIDs(_ model: ChatViewModel) -> [String] {
        model.visibleWorkItems.map(\.id).sorted()
    }

    private func snapshot(_ model: ChatViewModel, _ id: String) -> WorkCardSnapshot {
        let task = model.task(withID: id)!
        return model.workCardSnapshot(
            for: task, column: task.boardColumn, isSelected: false, isFrontierHighlighted: false,
            boardStyle: .productDefault, liveState: nil, now: now
        )
    }

    private func parked(_ task: WorkTask) -> WorkTask {
        var task = task
        task.parkedAt = String(Int(now.timeIntervalSince1970) - 3600)
        task.parkedBy = "human"
        task.parkedVia = "direct"
        return task
    }

    private func makeTask(
        _ id: String, status: String = "todo", pr: String? = nil, reason: String? = nil, kind: String = "task"
    ) -> WorkTask {
        var task = WorkTask(
            id: id, productID: "prod_test", projectID: "proj_test", kind: kind, name: "Task \(id)",
            description: "", status: status, priority: "medium", ordinal: nil, prURL: pr, deletedAt: nil,
            createdAt: "2026-10-01T00:00:00Z", updatedAt: "2026-10-01T00:00:00Z"
        )
        task.blockedReason = reason
        return task
    }

    private func makeProject(_ id: String) -> WorkProject {
        WorkProject(
            id: id, productID: "prod_test", name: "Project", slug: id, description: "", goal: "",
            status: "active", priority: "medium", createdAt: "2026-10-01T00:00:00Z",
            updatedAt: "2026-10-01T00:00:00Z"
        )
    }

    private func makeModel(
        tasks: [WorkTask], project: WorkProject? = nil, runtimes: [WorkTaskRuntime] = []
    ) -> ChatViewModel {
        let suite = "dev.spinyfin.boss.tests.parking-model.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        addTeardownBlock { defaults.removePersistentDomain(forName: suite) }
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock", defaults: defaults)
        let product = WorkProduct(
            id: "prod_test", name: "Test Product", slug: "test", description: "",
            repoRemoteURL: "https://github.com/org/repo.git", status: "active",
            createdAt: "2026-10-01T00:00:00Z", updatedAt: "2026-10-01T00:00:00Z"
        )
        model.products = [product]
        model.selectWorkProduct("prod_test")
        model.applyEventForTest(.workTree(
            product: product, projects: [project ?? makeProject("proj_test")], tasks: tasks, chores: [],
            taskRuntimes: runtimes, dependencies: [], ideas: []
        ))
        return model
    }

    private func taskPayload(extra: [String: Any] = [:]) -> [String: Any] {
        var payload: [String: Any] = [
            "id": "task_1", "product_id": "prod_test", "kind": "task", "name": "T", "description": "",
            "status": "todo", "created_at": "2026-10-01T00:00:00Z", "updated_at": "2026-10-01T00:00:00Z",
        ]
        payload.merge(extra) { _, new in new }
        return payload
    }

    private final class PayloadRecorder {
        var value: [[String: Any]] = []
    }

    private func installRecorder(on model: ChatViewModel) -> PayloadRecorder {
        let recorder = PayloadRecorder()
        model.outboundRecorder = { payload in recorder.value.append(payload) }
        return recorder
    }
}
