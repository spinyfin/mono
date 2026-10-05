import XCTest
@testable import Boss
@testable import UpdateCore

@MainActor
final class EngineFreshnessDriverTests: XCTestCase {
    func testAutomaticRestartDoesNotConsumeARequestQueuedWhileItRuns() {
        let model = UpdateModel(
            checker: UpdateChecker(currentVersionString: "1.0.0", fullVersionString: "1.0.0", fetcher: .noop),
            defaults: UserDefaults(suiteName: "freshness-test-\(UUID().uuidString)")!
        )
        var finish: (@MainActor @Sendable (IdleApplyOutcome) -> Void)?
        let driver = EngineFreshnessDriver(
            updateModel: model, chatModel: nil, liveWorkerStates: LiveWorkerStateStore(),
            snapshot: {
                IdleApplySnapshot(
                    mode: .automatic, isDevBuild: false, userRequested: model.applyWhenIdleRequested,
                    stagedVersion: nil, engineBehindBundle: true, engineRestartKey: "old->new",
                    engineReachable: true, liveWorkerCount: 0, secondsSinceUserInput: 3600
                )
            },
            restart: { finish = $0 }
        )
        driver.startApplying(policy: .init(workerQuietSeconds: 0))
        driver.tick()
        XCTAssertNotNil(finish)
        model.requestUpdateAndRestart()
        finish?(.performed)
        XCTAssertTrue(model.applyWhenIdleRequested)
    }

    func testDeferredRestartPreservesExplicitRequestInEveryMode() {
        for mode in [UpdateMode.automatic, .notify, .manual] {
            let model = UpdateModel(
                checker: UpdateChecker(currentVersionString: "1.0.0", fullVersionString: "1.0.0", fetcher: .noop),
                defaults: UserDefaults(suiteName: "freshness-test-\(UUID().uuidString)")!
            )
            model.requestUpdateAndRestart()
            var attempts = 0
            let driver = EngineFreshnessDriver(
                updateModel: model, chatModel: nil, liveWorkerStates: LiveWorkerStateStore(),
                snapshot: {
                    IdleApplySnapshot(
                        mode: mode, isDevBuild: false, userRequested: model.applyWhenIdleRequested,
                        stagedVersion: nil, engineBehindBundle: true, engineRestartKey: "old->new",
                        engineReachable: true, liveWorkerCount: 0, secondsSinceUserInput: 3600
                    )
                },
                restart: { completion in
                    attempts += 1
                    completion(.deferred("Waiting for 2 live workers."))
                }
            )
            driver.startApplying()
            driver.tick()
            XCTAssertTrue(model.applyWhenIdleRequested)
            XCTAssertEqual(model.idleApplyStatus, "Waiting for 2 live workers.")
            driver.tick()
            XCTAssertTrue(model.applyWhenIdleRequested)
            XCTAssertEqual(attempts, 2)
        }
    }

    func testInstallFailureSurvivesTheApplyTickAndLaterTicks() {
        let previousRelaunch = UpdateLifecycle.pendingRelaunch
        defer { UpdateLifecycle.pendingRelaunch = previousRelaunch }
        UpdateLifecycle.pendingRelaunch = nil
        let version = VersionTuple(major: 1, minor: 2, patch: 3)
        let defaults = UserDefaults(suiteName: "freshness-test-\(UUID().uuidString)")!
        let model = UpdateModel(
            checker: UpdateChecker(currentVersionString: "1.0.0", fullVersionString: "1.0.0", fetcher: .noop),
            defaults: defaults
        )
        model.requestUpdateAndRestart()
        var attempts = 0
        let driver = EngineFreshnessDriver(
            updateModel: model, chatModel: nil, liveWorkerStates: LiveWorkerStateStore(),
            snapshot: {
                IdleApplySnapshot(
                    mode: .notify, isDevBuild: false, userRequested: model.applyWhenIdleRequested,
                    stagedVersion: model.applyWhenIdleRequested ? version : nil,
                    engineBehindBundle: false, engineReachable: true, liveWorkerCount: 0,
                    secondsSinceUserInput: 3600
                )
            },
            install: { _ in attempts += 1; return .notInstalled }
        )
        driver.startApplying()
        driver.tick()
        XCTAssertEqual(attempts, 1)
        XCTAssertFalse(model.applyWhenIdleRequested)
        XCTAssertTrue(model.idleApplyStatus?.contains(UpdateInstallAction.notInstalledReason) == true)
        let failure = model.idleApplyStatus
        driver.tick()
        XCTAssertEqual(model.idleApplyStatus, failure)
        XCTAssertEqual(attempts, 1)
    }
}
