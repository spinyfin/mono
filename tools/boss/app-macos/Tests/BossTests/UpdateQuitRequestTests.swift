import Foundation
import UpdateCore
import XCTest
@testable import Boss

@MainActor
final class UpdateQuitRequestTests: XCTestCase {
    func testQuitWaitsForSheetDismissalThenHandsOffRelaunch() throws {
        let previousPlan = UpdateLifecycle.pendingRelaunch
        defer { UpdateLifecycle.pendingRelaunch = previousPlan }
        let plan = try makePlan()
        UpdateLifecycle.pendingRelaunch = plan
        let scheduler = FlushScheduler()
        var events: [String] = []
        var relaunchPlan: SwapPlan?
        let request = UpdateQuitRequest(
            terminate: {
                events.append("terminate")
                relaunchPlan = UpdateLifecycle.consumePendingRelaunch()
            },
            scheduleAfterDismiss: scheduler.schedule
        )

        request.request(isPresented: true) { events.append("dismiss") }
        XCTAssertEqual(events, ["dismiss"])
        XCTAssertNil(relaunchPlan)
        XCTAssertEqual(UpdateLifecycle.pendingRelaunch, plan)
        XCTAssertEqual(scheduler.count, 0, "terminate waits for onDismiss")

        request.didDismiss()
        XCTAssertEqual(events, ["dismiss"], "terminate waits one runloop turn after onDismiss")
        XCTAssertEqual(scheduler.count, 1)

        scheduler.flush()
        XCTAssertEqual(events, ["dismiss", "terminate"])
        XCTAssertEqual(relaunchPlan, plan)
        XCTAssertTrue(try XCTUnwrap(relaunchPlan).relaunch)
        XCTAssertNil(UpdateLifecycle.pendingRelaunch)
        request.didDismiss()
        scheduler.flush()
        XCTAssertEqual(events.count, 2, "a dismissal must not request termination twice")
    }

    func testCancelledQuitRetainsPlanAndCanBeRetried() throws {
        let previousPlan = UpdateLifecycle.pendingRelaunch
        defer { UpdateLifecycle.pendingRelaunch = previousPlan }
        let plan = try makePlan()
        UpdateLifecycle.pendingRelaunch = plan
        let scheduler = FlushScheduler()
        var terminationRequests = 0
        let request = UpdateQuitRequest(
            terminate: { terminationRequests += 1 },
            scheduleAfterDismiss: scheduler.schedule
        )
        request.request(isPresented: true, dismiss: {})
        request.didDismiss()
        scheduler.flush()
        XCTAssertEqual(terminationRequests, 1)
        XCTAssertEqual(UpdateLifecycle.pendingRelaunch, plan)
        request.request(isPresented: true, dismiss: {})
        request.didDismiss()
        scheduler.flush()
        XCTAssertEqual(terminationRequests, 2)
        XCTAssertEqual(UpdateLifecycle.pendingRelaunch, plan)
    }

    func testLaterOrSkipDismissalDoesNotQuit() {
        let scheduler = FlushScheduler()
        var terminationRequests = 0
        let request = UpdateQuitRequest(
            terminate: { terminationRequests += 1 },
            scheduleAfterDismiss: scheduler.schedule
        )
        request.didDismiss()
        scheduler.flush()
        XCTAssertEqual(terminationRequests, 0)
    }

    func testRequestAgainstHiddenSurfaceDoesNotStickPendingFlag() {
        let scheduler = FlushScheduler()
        var terminationRequests = 0
        let request = UpdateQuitRequest(
            terminate: { terminationRequests += 1 },
            scheduleAfterDismiss: scheduler.schedule
        )
        request.request(isPresented: false, dismiss: { XCTFail("nothing to dismiss") })
        XCTAssertEqual(terminationRequests, 0)
        scheduler.flush()
        XCTAssertEqual(terminationRequests, 1)
        request.didDismiss()
        scheduler.flush()
        XCTAssertEqual(terminationRequests, 1, "a later ordinary dismissal must not quit again")
    }

    func testMissedOnDismissDoesNotQuitALaterOrdinaryDismissal() {
        let scheduler = FlushScheduler()
        var terminationRequests = 0
        let request = UpdateQuitRequest(
            terminate: { terminationRequests += 1 },
            scheduleAfterDismiss: scheduler.schedule
        )
        request.request(isPresented: true, dismiss: {})
        request.cancelPendingDismiss()
        request.didDismiss()
        scheduler.flush()
        XCTAssertEqual(terminationRequests, 0)
    }

    func testTerminateReturnRePresentSheetViaSurfaceBinding() {
        let scheduler = FlushScheduler()
        let model = UpdateModel.placeholder()
        let request = UpdateQuitRequest(
            terminate: {},
            scheduleAfterDismiss: scheduler.schedule
        )
        UpdateQuitSurfaceBinding.bindQuitReturned(request, updateModel: model)

        var presented = true
        UpdateQuitSurfaceBinding.requestQuit(request, isPresented: presented) {
            presented = false
        }
        XCTAssertFalse(presented)
        XCTAssertFalse(model.showUpdateSheet)
        UpdateQuitSurfaceBinding.didDismiss(request)
        scheduler.flush()
        XCTAssertTrue(model.quitReturnedWithoutTerminating)
        XCTAssertTrue(model.showUpdateSheet)
        XCTAssertEqual(
            model.quitCancelledStatusNote(for: VersionTuple(major: 1, minor: 0, patch: 691)),
            "Quit was cancelled. Boss 1.0.691 is installed; quit when ready to finish the update.")
    }

    func testInstallActionMarksInstalledBeforeRequestingQuit() throws {
        let model = UpdateModel.placeholder()
        let version = try XCTUnwrap(VersionTuple.parse("1.0.691"))
        var events: [String] = []
        UpdateInstallAction.installAndRequestQuit(
            version: version,
            updateModel: model,
            requestQuit: {
                events.append("quit")
                XCTAssertEqual(
                    model.downloadState,
                    .installedPendingRelaunch(version: version, willRelaunch: true))
            },
            install: {
                events.append("install")
                return .relaunchPending
            }
        )
        XCTAssertEqual(events, ["install", "quit"])
        XCTAssertEqual(
            model.downloadState,
            .installedPendingRelaunch(version: version, willRelaunch: true))
    }

    func testInstallActionDoesNotQuitWhenSwapIsNotApplied() throws {
        let model = UpdateModel.placeholder()
        let version = try XCTUnwrap(VersionTuple.parse("1.0.691"))
        var quit = 0
        UpdateInstallAction.installAndRequestQuit(
            version: version,
            updateModel: model,
            requestQuit: { quit += 1 },
            install: { .notInstalled }
        )
        XCTAssertEqual(quit, 0)
        XCTAssertEqual(
            model.downloadState,
            .installFailed(version: version, reason: UpdateInstallAction.notInstalledReason))
    }

    func testInstallActionWithoutHelperDoesNotQuit() throws {
        let model = UpdateModel.placeholder()
        let version = try XCTUnwrap(VersionTuple.parse("1.0.691"))
        var quit = 0
        UpdateInstallAction.installAndRequestQuit(
            version: version,
            updateModel: model,
            requestQuit: { quit += 1 },
            install: { .installedNoRelaunch }
        )
        XCTAssertEqual(quit, 0)
        XCTAssertEqual(
            model.downloadState,
            .installedPendingRelaunch(version: version, willRelaunch: false))
    }

    func testConfirmedTerminationArmsParkedRelaunchPlan() throws {
        let previousPlan = UpdateLifecycle.pendingRelaunch
        defer { UpdateLifecycle.pendingRelaunch = previousPlan }
        let plan = try makePlan()
        UpdateLifecycle.pendingRelaunch = plan
        var armed: SwapPlan?
        var quitSwap = 0
        UpdateLifecycle.handleConfirmedTermination(
            armRelaunch: { armed = $0 },
            applyQuitSwap: { quitSwap += 1 }
        )
        XCTAssertEqual(armed, plan)
        XCTAssertEqual(quitSwap, 0)
        XCTAssertNil(UpdateLifecycle.pendingRelaunch)
    }

    func testConfirmedTerminationWithoutPlanAppliesQuitSwap() {
        let previousPlan = UpdateLifecycle.pendingRelaunch
        defer { UpdateLifecycle.pendingRelaunch = previousPlan }
        UpdateLifecycle.pendingRelaunch = nil
        var armed = 0
        var quitSwap = 0
        UpdateLifecycle.handleConfirmedTermination(
            armRelaunch: { _ in armed += 1 },
            applyQuitSwap: { quitSwap += 1 }
        )
        XCTAssertEqual(armed, 0)
        XCTAssertEqual(quitSwap, 1)
    }

    func testPopoverBindingDismissesThenTerminates() {
        let scheduler = FlushScheduler()
        var presented = true
        var terminated = 0
        let request = UpdateQuitRequest(
            terminate: { terminated += 1 },
            scheduleAfterDismiss: scheduler.schedule
        )
        request.request(isPresented: presented) {
            presented = false
            request.didDismiss()
        }
        XCTAssertFalse(presented)
        XCTAssertEqual(terminated, 0)
        scheduler.flush()
        XCTAssertEqual(terminated, 1)
    }

    func testProductionSchedulerDefersTerminateUntilAfterDidDismissReturns() async {
        let exp = expectation(description: "terminate on next main-queue turn")
        var order: [String] = []
        let request = UpdateQuitRequest(terminate: {
            order.append("terminate")
            exp.fulfill()
        })
        request.request(isPresented: true, dismiss: { order.append("dismiss") })
        request.didDismiss()
        order.append("didDismiss-returned")
        await fulfillment(of: [exp], timeout: 1)
        XCTAssertEqual(order, ["dismiss", "didDismiss-returned", "terminate"])
    }

    private func makePlan() throws -> SwapPlan {
        SwapPlan(
            version: try XCTUnwrap(VersionTuple.parse("1.0.691")),
            stagedBundleURL: URL(fileURLWithPath: "/tmp/update-test/staged/Boss.app"),
            installBundleURL: URL(fileURLWithPath: "/tmp/update-test/Boss.app"),
            backupURL: URL(fileURLWithPath: "/tmp/update-test/Boss.app.bak"),
            relaunch: true)
    }
}

@MainActor
private final class FlushScheduler {
    private var actions: [() -> Void] = []

    var count: Int { actions.count }

    func schedule(_ action: @escaping () -> Void) {
        actions.append(action)
    }

    func flush() {
        let batch = actions
        actions.removeAll()
        for action in batch {
            action()
        }
    }
}
