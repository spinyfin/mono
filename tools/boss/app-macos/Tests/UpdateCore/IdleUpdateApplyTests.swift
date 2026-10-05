import XCTest
@testable import UpdateCore

/// The apply-at-idle gate: a published release is applied without waiting for a
/// quit, but never while a worker is live and never over a dev build.
@MainActor
final class IdleUpdateApplyTests: XCTestCase {

    private let staged = VersionTuple(major: 1, minor: 0, patch: 686)

    /// Mutable world the applier reads through its injected closures.
    private final class World {
        var snapshot: IdleApplySnapshot
        var now = Date(timeIntervalSince1970: 1_000_000)
        var performed: [IdleApplyAction] = []
        init(_ snapshot: IdleApplySnapshot) { self.snapshot = snapshot }
    }

    private func automaticSnapshot(liveWorkers: Int) -> IdleApplySnapshot {
        IdleApplySnapshot(
            mode: .automatic,
            isDevBuild: false,
            userRequested: false,
            stagedVersion: staged,
            engineBehindBundle: false,
            engineReachable: true,
            liveWorkerCount: liveWorkers,
            secondsSinceUserInput: 3600
        )
    }

    private func makeApplier(_ world: World) -> IdleUpdateApplier {
        IdleUpdateApplier(
            policy: .init(workerQuietSeconds: 60, userIdleSeconds: 120, retryCooldownSeconds: 600),
            snapshot: { world.snapshot },
            perform: { action, completion in world.performed.append(action); completion(.performed) },
            now: { world.now }
        )
    }

    // MARK: - Required: no restart mid-turn, restart once idle

    func testAutomaticDoesNotApplyWhileAWorkerIsMidTurnAndAppliesOnceIdle() {
        let world = World(automaticSnapshot(liveWorkers: 1))
        let applier = makeApplier(world)

        // A worker is mid-turn: however long we wait, nothing is applied.
        for _ in 0..<50 {
            XCTAssertEqual(applier.tick(), .wait(.liveWorkers(1)))
            world.now.addTimeInterval(60)
        }
        XCTAssertEqual(world.performed, [])

        // The worker finishes. The quiet period starts now, not when it was staged.
        world.snapshot.liveWorkerCount = 0
        XCTAssertEqual(applier.tick(), .wait(.workersRecentlyActive))
        world.now.addTimeInterval(59)
        XCTAssertEqual(applier.tick(), .wait(.workersRecentlyActive))
        XCTAssertEqual(world.performed, [])

        world.now.addTimeInterval(1)
        XCTAssertEqual(applier.tick(), .apply(.relaunchIntoStagedUpdate(staged)))
        XCTAssertEqual(world.performed, [.relaunchIntoStagedUpdate(staged)])
    }

    func testAWorkerStartingDuringTheQuietPeriodResetsIt() {
        let world = World(automaticSnapshot(liveWorkers: 0))
        let applier = makeApplier(world)
        XCTAssertEqual(applier.tick(), .wait(.workersRecentlyActive))
        world.now.addTimeInterval(50)

        world.snapshot.liveWorkerCount = 2
        XCTAssertEqual(applier.tick(), .wait(.liveWorkers(2)))
        world.now.addTimeInterval(50)

        world.snapshot.liveWorkerCount = 0
        XCTAssertEqual(applier.tick(), .wait(.workersRecentlyActive))
        world.now.addTimeInterval(59)
        XCTAssertEqual(applier.tick(), .wait(.workersRecentlyActive))
        XCTAssertEqual(world.performed, [])
    }

    func testIdleTimeBeforeStagingCounts() {
        // Idle all along, update only now finishes staging: apply on the next tick.
        var snapshot = automaticSnapshot(liveWorkers: 0)
        snapshot.stagedVersion = nil
        let world = World(snapshot)
        let applier = makeApplier(world)
        XCTAssertEqual(applier.tick(), .nothingToApply)
        world.now.addTimeInterval(600)
        world.snapshot.stagedVersion = staged
        XCTAssertEqual(applier.tick(), .apply(.relaunchIntoStagedUpdate(staged)))
    }

    // MARK: - Other safety rules

    func testAutomaticWaitsForUserToBeAway() {
        let world = World(automaticSnapshot(liveWorkers: 0))
        world.snapshot.secondsSinceUserInput = 5
        let applier = makeApplier(world)
        _ = applier.tick()
        world.now.addTimeInterval(120)
        XCTAssertEqual(applier.tick(), .wait(.userActive))
        world.snapshot.secondsSinceUserInput = 120
        XCTAssertEqual(applier.tick(), .apply(.relaunchIntoStagedUpdate(staged)))
    }

    func testUnreachableEngineIsNeverTreatedAsIdle() {
        // No connection means the worker count is stale, not zero.
        let world = World(automaticSnapshot(liveWorkers: 0))
        world.snapshot.engineReachable = false
        let applier = makeApplier(world)
        world.now.addTimeInterval(3600)
        XCTAssertEqual(applier.tick(), .wait(.engineUnreachable))
        XCTAssertEqual(world.performed, [])
    }

    func testModalUIBlocksApply() {
        let world = World(automaticSnapshot(liveWorkers: 0))
        world.snapshot.hasModalUI = true
        let applier = makeApplier(world)
        _ = applier.tick()
        world.now.addTimeInterval(600)
        XCTAssertEqual(applier.tick(), .wait(.modalUI))
    }

    func testDevBuildIsNeverAppliedEvenWhenRequested() {
        let world = World(automaticSnapshot(liveWorkers: 0))
        world.snapshot.isDevBuild = true
        world.snapshot.userRequested = true
        world.snapshot.engineBehindBundle = true
        let applier = makeApplier(world)
        world.now.addTimeInterval(3600)
        XCTAssertEqual(applier.tick(), .notEligible)
        XCTAssertEqual(world.performed, [])
    }

    func testNotifyAndManualModesDoNotApplyUnattended() {
        for mode in [UpdateMode.notify, .manual] {
            let world = World(automaticSnapshot(liveWorkers: 0))
            world.snapshot.mode = mode
            let applier = makeApplier(world)
            _ = applier.tick()
            world.now.addTimeInterval(3600)
            XCTAssertEqual(applier.tick(), .notEligible, "\(mode)")
            XCTAssertEqual(world.performed, [])
        }
    }

    // MARK: - One-click "Update & Restart"

    func testRequestedApplyWaitsForLiveWorkersThenAppliesImmediately() {
        let world = World(automaticSnapshot(liveWorkers: 3))
        world.snapshot.mode = .notify
        world.snapshot.userRequested = true
        world.snapshot.secondsSinceUserInput = 0  // just clicked
        let applier = makeApplier(world)

        XCTAssertEqual(applier.tick(), .wait(.liveWorkers(3)))
        XCTAssertEqual(world.performed, [])

        // No quiet period and no away-from-keyboard wait for an explicit request.
        world.snapshot.liveWorkerCount = 0
        XCTAssertEqual(applier.tick(), .apply(.relaunchIntoStagedUpdate(staged)))
        XCTAssertEqual(world.performed, [.relaunchIntoStagedUpdate(staged)])
    }

    func testRequestedApplyWaitsWhileUpdateIsStillDownloading() {
        let world = World(automaticSnapshot(liveWorkers: 0))
        world.snapshot.mode = .notify
        world.snapshot.userRequested = true
        world.snapshot.stagedVersion = nil
        world.snapshot.isPreparingUpdate = true
        let applier = makeApplier(world)
        XCTAssertEqual(applier.tick(), .wait(.preparingUpdate))
        world.snapshot.isPreparingUpdate = false
        XCTAssertEqual(applier.tick(), .nothingToApply)
    }

    // MARK: - Engine older than the bundle

    func testEngineBehindBundleRestartsOnlyTheEngineAtIdle() {
        let world = World(automaticSnapshot(liveWorkers: 1))
        world.snapshot.stagedVersion = nil
        world.snapshot.engineBehindBundle = true
        let applier = makeApplier(world)
        XCTAssertEqual(applier.tick(), .wait(.liveWorkers(1)))

        world.snapshot.liveWorkerCount = 0
        _ = applier.tick()
        world.now.addTimeInterval(60)
        XCTAssertEqual(applier.tick(), .apply(.restartEngine))
        XCTAssertEqual(world.performed, [.restartEngine])
    }

    func testStagedUpdateTakesPrecedenceOverEngineRestart() {
        let world = World(automaticSnapshot(liveWorkers: 0))
        world.snapshot.engineBehindBundle = true
        world.snapshot.userRequested = true
        let applier = makeApplier(world)
        XCTAssertEqual(applier.tick(), .apply(.relaunchIntoStagedUpdate(staged)))
    }

    // MARK: - No restart loop

    func testAFailedApplyIsNotRetriedUntilTheCooldownPasses() {
        let world = World(automaticSnapshot(liveWorkers: 0))
        world.snapshot.stagedVersion = nil
        world.snapshot.engineBehindBundle = true  // and stays true: the restart "failed"
        let applier = makeApplier(world)
        _ = applier.tick()
        world.now.addTimeInterval(60)
        XCTAssertEqual(applier.tick(), .apply(.restartEngine))

        world.now.addTimeInterval(599)
        XCTAssertEqual(applier.tick(), .wait(.recentAttempt))
        XCTAssertEqual(world.performed.count, 1)

        world.now.addTimeInterval(1)
        XCTAssertEqual(applier.tick(), .apply(.restartEngine))
        XCTAssertEqual(world.performed.count, 2)
    }

    func testAnExplicitRequestOverridesTheCooldown() {
        let world = World(automaticSnapshot(liveWorkers: 0))
        world.snapshot.userRequested = true
        let applier = makeApplier(world)
        XCTAssertEqual(applier.tick(), .apply(.relaunchIntoStagedUpdate(staged)))
        XCTAssertEqual(applier.tick(), .wait(.recentAttempt))
        applier.noteUserRequest()
        XCTAssertEqual(applier.tick(), .apply(.relaunchIntoStagedUpdate(staged)))
    }

    func testEngineRestartDoesNotPreemptAnInFlightDownload() {
        let world = World(automaticSnapshot(liveWorkers: 0))
        world.snapshot.mode = .notify
        world.snapshot.userRequested = true
        world.snapshot.stagedVersion = nil
        world.snapshot.engineBehindBundle = true
        world.snapshot.isPreparingUpdate = true
        let applier = makeApplier(world)
        XCTAssertEqual(applier.tick(), .wait(.preparingUpdate))
        XCTAssertEqual(world.performed, [])
    }

    func testUnattendedEngineRestartIsNotRepeatedForTheSamePair() {
        let world = World(automaticSnapshot(liveWorkers: 0))
        world.snapshot.stagedVersion = nil
        world.snapshot.engineBehindBundle = true
        world.snapshot.engineRestartKey = "1.0.1->1.0.2"
        let applier = makeApplier(world)
        _ = applier.tick()
        world.now.addTimeInterval(60)
        XCTAssertEqual(applier.tick(), .apply(.restartEngine))

        // Still behind after the cooldown: the same pair is not retried unattended.
        world.now.addTimeInterval(3600)
        XCTAssertEqual(applier.tick(), .nothingToApply)
        XCTAssertEqual(world.performed.count, 1)

        // A different pair is eligible again, and an explicit request overrides.
        world.snapshot.engineRestartKey = "1.0.1->1.0.3"
        XCTAssertEqual(applier.tick(), .apply(.restartEngine))
        world.now.addTimeInterval(3600)
        world.snapshot.userRequested = true
        XCTAssertEqual(applier.tick(), .apply(.restartEngine))
    }

    func testDeferredRestartRemainsEligibleAndPreservesRequestedModes() {
        for mode in [UpdateMode.automatic, .notify, .manual] {
            for requested in [false, true] where mode == .automatic || requested {
                let world = World(automaticSnapshot(liveWorkers: 0))
                world.snapshot.mode = mode
                world.snapshot.userRequested = requested
                world.snapshot.stagedVersion = nil
                world.snapshot.engineBehindBundle = true
                world.snapshot.engineRestartKey = "old->bundled"
                let applier = IdleUpdateApplier(
                    policy: .init(workerQuietSeconds: 0), snapshot: { world.snapshot },
                    perform: { action, completion in
                        world.performed.append(action)
                        completion(.deferred("Waiting for live workers."))
                    }, now: { world.now }
                )
                XCTAssertEqual(applier.tick(), .apply(.restartEngine))
                XCTAssertEqual(applier.tick(), .apply(.restartEngine))
                XCTAssertEqual(world.performed.count, 2)
                XCTAssertEqual(world.snapshot.userRequested, requested)
                world.snapshot.engineBehindBundle = false
                XCTAssertEqual(applier.tick(), .nothingToApply)
                XCTAssertNil(applier.outcome, "A completed external restart must clear the old waiting status")
            }
        }
    }

    func testFingerprintMismatchRecoversDevBundleEvenInManualMode() {
        let world = World(automaticSnapshot(liveWorkers: 2))
        world.snapshot.mode = .manual
        world.snapshot.isDevBuild = true
        world.snapshot.stagedVersion = nil
        world.snapshot.engineBehindBundle = false // equal version, different binary
        world.snapshot.engineFingerprintMismatch = true
        let applier = makeApplier(world)
        XCTAssertEqual(applier.tick(), .wait(.liveWorkers(2)))
        world.snapshot.liveWorkerCount = 0
        XCTAssertEqual(applier.tick(), .wait(.workersRecentlyActive))
        world.now.addTimeInterval(60)
        XCTAssertEqual(applier.tick(), .apply(.restartEngine))
    }

    func testFingerprintRecoveryDoesNotInstallStagedReleaseInNotifyMode() {
        let world = World(automaticSnapshot(liveWorkers: 0))
        world.snapshot.mode = .notify
        world.snapshot.engineFingerprintMismatch = true
        let applier = makeApplier(world)
        _ = applier.tick()
        world.now.addTimeInterval(60)
        XCTAssertEqual(applier.tick(), .apply(.restartEngine))
    }

    func testFailedDownloadIsNotReportedAsNoNewerBuild() {
        let failed = UpdateDownloadState.failed(version: staged, reason: "network down")
        XCTAssertTrue(unfulfillableRequestMessage(downloadState: failed).contains("network down"))
        let installFailed = UpdateDownloadState.installFailed(version: staged, reason: "swap failed")
        XCTAssertTrue(unfulfillableRequestMessage(downloadState: installFailed).contains("swap failed"))
        XCTAssertTrue(unfulfillableRequestMessage(downloadState: .idle).contains("no newer"))
    }

    // MARK: - Engine vs. bundle comparison

    func testEngineIsBehindBundle() {
        let bundle = VersionTuple(major: 1, minor: 0, patch: 686)
        XCTAssertTrue(engineIsBehindBundle(engineVersion: "1.0.685", bundleVersion: bundle))
        XCTAssertTrue(engineIsBehindBundle(engineVersion: "1.0.685-dev-abc1234", bundleVersion: bundle))
        XCTAssertFalse(engineIsBehindBundle(engineVersion: "1.0.686", bundleVersion: bundle))
        XCTAssertFalse(engineIsBehindBundle(engineVersion: "1.0.687", bundleVersion: bundle))
        // Unknown is never "behind": it must not trigger a restart.
        XCTAssertFalse(engineIsBehindBundle(engineVersion: "unknown", bundleVersion: bundle))
        XCTAssertFalse(engineIsBehindBundle(engineVersion: "1.0.685", bundleVersion: nil))
    }

    func testParseReleaseBase() {
        XCTAssertEqual(VersionTuple.parseReleaseBase("1.0.686"), VersionTuple(major: 1, minor: 0, patch: 686))
        XCTAssertEqual(
            VersionTuple.parseReleaseBase("1.0.686-dev-abc1234"), VersionTuple(major: 1, minor: 0, patch: 686))
        XCTAssertNil(VersionTuple.parseReleaseBase("unknown"))
        XCTAssertNil(VersionTuple.parseReleaseBase(""))
    }
}
