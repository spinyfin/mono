import XCTest
@testable import UpdateCore

final class UpdateStartupTrailTests: XCTestCase {
    private var root: URL!
    private var log: UpdateLog!
    private var installer: UpdateInstaller!
    private let old = VersionTuple(major: 1, minor: 0, patch: 27)
    private let target = VersionTuple(major: 1, minor: 0, patch: 28)

    override func setUpWithError() throws {
        // Under `bazel test` only TEST_TMPDIR is writable (the sandbox denies the
        // system temp directory).
        root = URL(fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
                   isDirectory: true).appendingPathComponent("UpdateStartupTrailTests-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        log = UpdateLog(directory: root.appendingPathComponent("log"))
        installer = UpdateInstaller(updatesDirectory: root, installBundleURL: root.appendingPathComponent("Boss.app"), log: log)
    }

    override func tearDownWithError() throws { if let root { try? FileManager.default.removeItem(at: root) } }

    func testEarlyRecordPrecedesFlagAndPreservesFailedTarget() {
        log.append(UpdateLogRecord(ts: "2026-10-11T00:00:00Z", attempt: "attempt", from: old.description,
            to: target.description, actor: "helper", pid: 1, stage: "relaunch", event: "helper-started", level: "info", detail: [:]))
        let trail = UpdateStartupTrail(installer: installer, running: old)
        trail.record("process-started", detail: ["executable": "/test/Boss", "runningVersion": old.description])
        XCTAssertFalse(installer.launchSucceeded(version: old))
        installer.markLaunchSucceeded(version: old)
        trail.record("flag-written", stage: "handshake")
        trail.record("decision", stage: "engine")
        let records = log.records(attempt: "attempt")
        XCTAssertEqual(records.map(\.event), ["helper-started", "process-started", "flag-written", "decision"])
        XCTAssertTrue(records.allSatisfy { $0.from == old.description && $0.to == target.description })
    }

    func testDetectsBlocklistConsumedByLegacyAppOnce() throws {
        XCTAssertEqual(installer.reconcileAfterLaunch(currentVersion: old), .noChange)
        try Data("{\"blocklist\":[\"1.0.28\"]}".utf8).write(to: root.appendingPathComponent("install-state.json"))
        guard case .rolledBack(let record) = installer.reconcileAfterLaunch(currentVersion: old) else {
            return XCTFail("blocklist growth must be surfaced")
        }
        XCTAssertEqual(record.reason, .unknown)
        XCTAssertTrue(record.detail.contains("blocklist grew"))
        XCTAssertEqual(installer.reconcileAfterLaunch(currentVersion: old), .noChange)
    }

    func testDetectsDanglingSwapOnce() {
        log.record(stage: "swap", event: "committed", attempt: "dangling", from: old, to: target)
        guard case .rolledBack(let record) = installer.reconcileAfterLaunch(currentVersion: old) else {
            return XCTFail("incomplete attempt must be surfaced")
        }
        XCTAssertEqual(record.reason, .unknown)
        XCTAssertEqual(record.attemptID, "dangling")
        XCTAssertEqual(log.allRecords().last?.event, "rollback-detected")
        XCTAssertEqual(installer.reconcileAfterLaunch(currentVersion: old), .noChange)
    }

    func testOrdinaryLaunchAfterAnsweredAttemptCarriesNoAttempt() {
        log.append(UpdateLogRecord(ts: "2026-10-11T00:00:00Z", attempt: "done", from: old.description,
            to: target.description, actor: "helper", pid: 1, stage: "relaunch", event: "committed", level: "info", detail: [:]))
        log.record(stage: "handshake", event: "flag-written", attempt: "done", from: old, to: target)
        let trail = UpdateStartupTrail(installer: installer, running: target)
        XCTAssertNil(trail.attempt, "a routine launch must not attach itself to a finished attempt")
        XCTAssertEqual(trail.record("process-started")?.detail["runningVersion"], target.description)
    }

    func testUnrelatedVersionDoesNotAdoptAnOldHelperAttempt() {
        // A helper attempt for 1.0.27 -> 1.0.28 that no app answered (1.0.27 reopened
        // after a rollback predates startup records). A later 1.0.30 launch is not it.
        log.append(UpdateLogRecord(ts: "2026-10-11T00:00:00Z", attempt: "old-failure", from: old.description,
            to: target.description, actor: "helper", pid: 1, stage: "rollback", event: "completed", level: "error", detail: [:]))
        let trail = UpdateStartupTrail(installer: installer, running: VersionTuple(major: 1, minor: 0, patch: 30))
        XCTAssertNil(trail.attempt)
    }

    func testOrphanedSidecarDoesNotDisturbALaterInFlightSwap() throws {
        // 1.0.27 -> 1.0.28 rolled back and an old app consumed the one-line marker,
        // leaving the sidecar. Then 1.0.27 -> 1.0.29 swapped and relaunched: its helper
        // is still watching, so reconcile must complete 1.0.29 normally and only fold
        // the 1.0.28 reason into the rollback history.
        let next = VersionTuple(major: 1, minor: 0, patch: 29)
        try """
        version=1.0.28
        reason=process-exited
        detail=pid 9 exited 1s after starting; exitCode=3
        attempt=failed-attempt
        from=1.0.27
        restored=1.0.27
        """.write(to: installer.rolledBackDetailURL, atomically: true, encoding: .utf8)
        let backup = installer.backupURL
        try FileManager.default.createDirectory(at: backup, withIntermediateDirectories: true)
        let plan = SwapPlan(version: next, stagedBundleURL: root.appendingPathComponent("1.0.29/Boss.app"),
            installBundleURL: root.appendingPathComponent("Boss.app"), backupURL: backup, relaunch: true,
            fromVersion: old, attemptID: "in-flight")
        installer.recordPending(plan)

        XCTAssertEqual(installer.reconcileAfterLaunch(currentVersion: next), .installed(next))
        let record = try XCTUnwrap(installer.lastRollback())
        XCTAssertEqual(record.version, "1.0.28")
        XCTAssertEqual(record.reason, .processExited)
        XCTAssertEqual(record.attemptID, "failed-attempt")
        XCTAssertFalse(installer.isBlocked(target), "the sidecar never blocklists on its own")
        XCTAssertFalse(FileManager.default.fileExists(atPath: installer.rolledBackDetailURL.path))
        XCTAssertEqual(log.records(attempt: "in-flight").last?.event, "installed")
    }

    func testStaleSidecarNeverRelabelsANewMarker() throws {
        try "version=1.0.26\nreason=open-failed\nattempt=stale\n"
            .write(to: installer.rolledBackDetailURL, atomically: true, encoding: .utf8)
        try "1.0.28\n".write(to: installer.rolledBackMarkerURL, atomically: true, encoding: .utf8)
        guard case .rolledBack(let record) = installer.reconcileAfterLaunch(currentVersion: old) else {
            return XCTFail("the marker must be reconciled")
        }
        XCTAssertEqual(record.version, "1.0.28")
        XCTAssertEqual(record.reason, .unknown)
        XCTAssertNil(record.attemptID)
        XCTAssertTrue(installer.isBlocked(target))
    }

    func testSwapsThatNeverCommittedAreNotRollbacks() {
        log.record(stage: "swap", event: "skipped-not-writable", attempt: "ro", from: old, to: target)
        log.record(stage: "swap", event: "requested", attempt: "undone", from: old, to: target)
        log.record(stage: "swap", event: "rolled-back-in-process", attempt: "undone", from: old, to: target)
        XCTAssertEqual(installer.reconcileAfterLaunch(currentVersion: old), .noChange)
    }

    func testCommittedSwapSeenFromAnotherVersionIsNotARollback() {
        // e.g. a dev build launched against the same Updates directory.
        log.record(stage: "swap", event: "committed", attempt: "elsewhere", from: old, to: target)
        XCTAssertEqual(installer.reconcileAfterLaunch(currentVersion: VersionTuple(major: 1, minor: 0, patch: 4)), .noChange)
    }

    func testHelperReplayResumesWithoutRepeatsOrGapsWithinOneSecond() {
        let sink = RecordSink()
        let mirrored = UpdateLog(directory: log.directory, mirror: { sink.append($0) })
        func helper(_ event: String) -> UpdateLogRecord {
            UpdateLogRecord(ts: "2026-10-11T00:00:05Z", attempt: "a", from: old.description, to: target.description,
                            actor: "helper", pid: 1, stage: "rollback", event: event, level: "info", detail: [:])
        }
        log.append(helper("reopened-previous"))
        mirrored.replayHelperRecords()
        // The helper appends again within the same second after the app replayed.
        log.append(helper("completed"))
        mirrored.replayHelperRecords()
        mirrored.replayHelperRecords()
        XCTAssertEqual(sink.records.map(\.event), ["reopened-previous", "completed"])
    }

    func testHelperReplayUsesMirrorWithoutDuplicatingDurableRecords() {
        let sink = RecordSink()
        let mirrored = UpdateLog(directory: log.directory, mirror: { sink.append($0) })
        let helper = UpdateLogRecord(ts: "2026-10-11T00:00:00Z", attempt: "helper", from: old.description,
            to: target.description, actor: "helper", pid: 1, stage: "rollback", event: "completed", level: "error", detail: [:])
        log.append(helper)
        mirrored.replayHelperRecords()
        XCTAssertEqual(sink.records.count, 1)
        XCTAssertEqual(sink.records.first?.detail["originalTimestamp"], helper.ts)
        XCTAssertEqual(mirrored.allRecords(), [helper])
    }
}

private final class RecordSink: @unchecked Sendable {
    private let lock = NSLock()
    private var values: [UpdateLogRecord] = []
    var records: [UpdateLogRecord] { lock.lock(); defer { lock.unlock() }; return values }
    func append(_ record: UpdateLogRecord) { lock.lock(); defer { lock.unlock() }; values.append(record) }
}
