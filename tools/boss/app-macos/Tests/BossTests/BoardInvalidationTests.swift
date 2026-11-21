import AppKit
import Combine
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class BoardInvalidationTests: XCTestCase {
    func testUnrelatedUpdatesDoNotRebuildCardSnapshots() {
        let model = ChatViewModel(socketPath: "/tmp/boss-board-observation-\(UUID().uuidString).sock")
        let tasks = (0..<20).map { (index: Int) in
            WorkTask(id: "card-\(index)", productID: "product", projectID: nil,
                kind: "chore", name: "Card \(index)", description: "", status: "todo",
                priority: "medium", ordinal: index, prURL: nil, deletedAt: nil,
                createdAt: "", updatedAt: "")
        }
        model.choresByProductID = ["product": tasks]
        let host = NSHostingView(rootView: WorkBoardSectionItemsView(
            items: tasks, column: .backlog, boardStyle: .classic,
            model: model))
        host.frame = NSRect(x: 0, y: 0, width: 300, height: 6000)
        host.layoutSubtreeIfNeeded()
        let before = UIUpdateCounters.shared.currentCounts().cardSnapshotBuilds
        for index in 0..<10 {
            model.workErrorMessage = "Connection notice \(index)"
            model.presentationUpdates.flush()
            host.layoutSubtreeIfNeeded()
        }
        let builds = UIUpdateCounters.shared.currentCounts().cardSnapshotBuilds - before
        print("BOARD_INVALIDATION notices=10 cards=20 snapshot_builds=\(builds)")
        XCTAssertGreaterThan(before, 0, "The fixture must mount real cards")
        XCTAssertEqual(builds, 0)
    }

    func testOneTaskChangePublishesOnlyItsRowAndPreservesLayout() {
        let model = makeModel()
        let first = WorkCardRenderState(model: model, id: "first", column: .backlog, style: .classic)
        let second = WorkCardRenderState(model: model, id: "second", column: .backlog, style: .classic)
        let layout = WorkPresentationValue(updates: model.presentationUpdates) { WorkBoardLayout(model: model) }
        let sidebar = WorkPresentationValue(updates: model.presentationUpdates) { WorkSidebarSnapshot(model: model) }
        var rowChanges = [0, 0]
        var layoutChanges = 0
        var sidebarChanges = 0
        let subscriptions = [
            first.objectWillChange.sink { rowChanges[0] += 1 },
            second.objectWillChange.sink { rowChanges[1] += 1 },
            layout.objectWillChange.sink { layoutChanges += 1 },
            sidebar.objectWillChange.sink { sidebarChanges += 1 },
        ]
        model.choresByProductID["product"]?[0].name = "Renamed card"
        model.presentationUpdates.flush()
        XCTAssertEqual(rowChanges, [1, 0])
        XCTAssertEqual(layoutChanges, 0)
        XCTAssertEqual(sidebarChanges, 0)
        XCTAssertEqual(first.value?.task.name, "Renamed card")
        withExtendedLifetime(subscriptions) {}
    }

    func testBurstPublishesFinalValueOnceAndDoesNotLoseTrailingUpdate() {
        let model = makeModel()
        let row = WorkCardRenderState(model: model, id: "first", column: .backlog, style: .classic)
        var changes = 0
        let subscription = row.objectWillChange.sink { changes += 1 }
        for index in 0..<100 { model.choresByProductID["product"]?[0].name = "Name \(index)" }
        XCTAssertEqual(changes, 0)
        model.presentationUpdates.flush()
        XCTAssertEqual(changes, 1)
        XCTAssertEqual(row.value?.task.name, "Name 99")
        model.choresByProductID["product"]?[0].name = "Final name"
        model.presentationUpdates.flush()
        XCTAssertEqual(changes, 2)
        XCTAssertEqual(row.value?.task.name, "Final name")
        withExtendedLifetime(subscription) {}
    }

    func testFrameTimerDeliversTheFinalStateWithoutManualFlush() async {
        let model = makeModel()
        let row = WorkCardRenderState(model: model, id: "first", column: .backlog, style: .classic)
        let delivered = expectation(description: "Frame timer delivers last task state")
        let subscription = row.$value.dropFirst().sink { value in
            if value?.task.name == "Last value" { delivered.fulfill() }
        }
        model.choresByProductID["product"]?[0].name = "Intermediate value"
        model.choresByProductID["product"]?[0].name = "Last value"
        await fulfillment(of: [delivered], timeout: 5)
        XCTAssertEqual(row.value?.task.name, "Last value")
        withExtendedLifetime(subscription) {}
    }

    func testCountCacheChangesForTaskAndProjectParking() {
        let model = makeModel()
        XCTAssertEqual(model.unblockedChoreCount, 2)
        XCTAssertNotNil(model.cachedSidebarCounts)
        model.choresByProductID["product"]?[0].status = "blocked"
        model.choresByProductID["product"]?[0].blockedReason = "dependency"
        XCTAssertEqual(model.unblockedChoreCount, 1)
        XCTAssertEqual(model.blockedChoreCount, 1)
        let task = makeTask("project-task", projectID: "project")
        model.tasksByProjectID = ["project": [task]]
        var project = WorkProject(id: "project", productID: "product", name: "Project", slug: "project",
            description: "", goal: "", status: "active", priority: "medium", createdAt: "", updatedAt: "")
        model.projectsByProductID = ["product": [project]]
        XCTAssertEqual(model.unblockedTaskCount(forProjectID: project.id), 1)
        project.parkedAt = "100"
        model.projectsByProductID = ["product": [project]]
        XCTAssertEqual(model.unblockedTaskCount(forProjectID: project.id), 0)
        XCTAssertEqual(model.parkedTaskCount(forProjectID: project.id), 1)
        model.tasksByProjectID[project.id]?[0].status = "done"
        XCTAssertEqual(model.parkedTaskCount(forProjectID: project.id), 0)
    }

    func testSelectionAndGuideRequestReachRowsWithoutChangingLayout() {
        let model = makeModel()
        let row = WorkCardRenderState(model: model, id: "first", column: .backlog, style: .classic)
        model.selectedWorkCardID = "first"
        model.navigator.mode = .kanban
        model.presentationUpdates.flush()
        XCTAssertEqual(row.value?.snapshot.isSelected, true)
        model.retryingReviewGuideRootTaskIDs.insert("first")
        model.presentationUpdates.flush()
        XCTAssertEqual(row.value?.snapshot.reviewGuidePresentation?.kind, .generating)
        model.revisionComposerTaskID = "first"
        model.presentationUpdates.flush()
        XCTAssertEqual(row.value?.revisionComposerPresented, true)
        row.updateStyle(.minimal, model: model, id: "first", column: .backlog)
        XCTAssertEqual(row.value?.snapshot.boardStyle, .minimal)
        model.choresByProductID.removeAll()
        model.presentationUpdates.flush()
        XCTAssertNil(row.value)
    }

    func testTwentyWorkersOnlyRefreshTheChangedWorkerCard() {
        let model = makeModel()
        let tasks = (0..<20).map { index in
            var task = makeTask("worker-card-\(index)")
            task.status = "active"
            return task
        }
        model.choresByProductID = ["product": tasks]
        model.taskRuntimesByID = Dictionary(uniqueKeysWithValues: tasks.map {
            ($0.id, WorkTaskRuntime(workItemID: $0.id, executionStatus: "running", runStatus: "running",
                executionID: $0.id, dispatchRetryAt: nil, dispatchWaitReason: nil, dispatchWaitSince: nil))
        })
        func worker(index: Int, status: String) -> WorkerLiveState {
            WorkerLiveState(slotId: index + 1, runId: tasks[index].id, model: "fixture", shellPid: 0,
                lastEventAt: nil, currentTool: nil, lastToolEndedAt: nil, activity: .working,
                liveStatus: status, liveStatusAt: nil, recoveryStatus: nil, tmuxHosted: true)
        }
        var workers = tasks.indices.map { worker(index: $0, status: "Working") }
        model.liveWorkerStates.update(states: workers)
        let rows = tasks.map { WorkCardRenderState(model: model, id: $0.id, column: .doing, style: .classic) }
        var layoutReads = 0
        var sidebarReads = 0
        let layout = WorkPresentationValue(updates: model.presentationUpdates) {
            layoutReads += 1
            return WorkBoardLayout(model: model)
        }
        let sidebar = WorkPresentationValue(updates: model.presentationUpdates) {
            sidebarReads += 1
            return WorkSidebarSnapshot(model: model)
        }
        var layoutChanges = 0
        var sidebarChanges = 0
        var rowChanges = 0
        let subscriptions = rows.map { $0.objectWillChange.sink { rowChanges += 1 } } + [
            layout.objectWillChange.sink { layoutChanges += 1 },
            sidebar.objectWillChange.sink { sidebarChanges += 1 },
        ]
        let before = UIUpdateCounters.shared.currentCounts().cardSnapshotBuilds
        for tick in 0..<100 {
            workers[0] = worker(index: 0, status: "Working step \(tick)")
            model.liveWorkerStates.update(states: workers)
            model.presentationUpdates.flush()
        }
        let builds = UIUpdateCounters.shared.currentCounts().cardSnapshotBuilds - before
        print("BOARD_WORKERS workers=20 ticks=100 snapshot_builds=\(builds) row_updates=\(rowChanges) layout_updates=\(layoutChanges) sidebar_updates=\(sidebarChanges)")
        XCTAssertEqual(builds, 100)
        XCTAssertEqual(rowChanges, 100)
        XCTAssertEqual(layoutChanges, 0)
        XCTAssertEqual(sidebarChanges, 0)
        XCTAssertEqual(layoutReads, 1, "Live-only ticks must not even rederive layout")
        XCTAssertEqual(sidebarReads, 1, "Live-only ticks must not even rederive sidebar")
        withExtendedLifetime(subscriptions) {}
    }

    func testEveryCardInputRepublishesItsRow() {
        let model = makeModel()
        let row = WorkCardRenderState(model: model, id: "first", column: .backlog, style: .classic)
        var changes = 0
        let subscription = row.objectWillChange.sink { changes += 1 }
        let mutations: [(String, () -> Void)] = [
            ("selectedWorkCardID", { model.selectedWorkCardID = "first" }),
            ("depFrontierHighlightIDs", { model.depFrontierHighlightIDs = ["first"] }),
            ("revealHighlightID", { model.revealHighlightID = "first" }),
            ("mergingWhenReadyIDs", { model.mergingWhenReadyIDs = ["first"] }),
            ("revisionComposerTaskID", { model.revisionComposerTaskID = "first" }),
        ]
        for (name, mutate) in mutations {
            let before = changes
            mutate()
            model.presentationUpdates.flush()
            XCTAssertEqual(changes, before + 1, "\(name) must republish the card row")
        }
        withExtendedLifetime(subscription) {}
    }

    private func makeModel() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-board-observation-\(UUID().uuidString).sock")
        model.products = [WorkProduct(id: "product", name: "Product", slug: "product", description: "",
            repoRemoteURL: nil, status: "active", createdAt: "", updatedAt: "")]
        model.selectedWorkProductID = "product"
        model.choresByProductID = ["product": [makeTask("first"), makeTask("second")]]
        return model
    }

    private func makeTask(_ id: String, projectID: String? = nil) -> WorkTask {
        WorkTask(id: id, productID: "product", projectID: projectID, kind: "chore", name: id,
            description: "", status: "todo", priority: "medium", ordinal: nil,
            prURL: nil, deletedAt: nil, createdAt: "", updatedAt: "")
    }
}
