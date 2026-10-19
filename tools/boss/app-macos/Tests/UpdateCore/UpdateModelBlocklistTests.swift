import XCTest
@testable import UpdateCore

/// `UpdateModel` × the failed-version blocklist: a version the relaunch watchdog rolled
/// back is never re-staged by background polling or the plain Download action, the
/// recorded rollback is surfaced as a notice until dismissed, and only the explicit
/// retry clears the block and stages again.
@MainActor
final class UpdateModelBlocklistTests: XCTestCase {

    private var defaults: UserDefaults!
    private var suiteName: String!
    private var tempRoot: URL!

    private let mockVersion = VersionTuple(major: 1, minor: 0, patch: 99)

    override func setUp() async throws {
        try await super.setUp()
        suiteName = "com.boss.test.UpdateModelBlocklistTests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suiteName)!
        tempRoot = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
            isDirectory: true
        ).appendingPathComponent("UpdateModelBlocklistTests-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: tempRoot, withIntermediateDirectories: true)
    }

    override func tearDown() async throws {
        defaults.removePersistentDomain(forName: suiteName)
        if let tempRoot { try? FileManager.default.removeItem(at: tempRoot) }
        try await super.tearDown()
    }

    func testCheckAndStageShareAttemptContext() async {
        let installer = makeInstaller()
        let recorder = StageCounter()
        let model = makeModel(stager: recorder.stager(), installer: installer)
        await model.checkNow()
        await model.checkNow()
        model.downloadAvailableUpdate()
        await model.awaitStagingForTesting()
        let checks = log.allRecords().filter { $0.event == "available" }
        XCTAssertEqual(checks.count, 2)
        XCTAssertEqual(checks[0].attempt, checks[1].attempt, "re-checks of one release stay in one attempt")
        let check = checks[0]
        let stage = log.allRecords().first { $0.event == "begin" }!
        XCTAssertNotNil(check.attempt)
        XCTAssertEqual(stage.attempt, check.attempt)
        XCTAssertEqual(stage.from, check.from)
        XCTAssertEqual(stage.to, check.to)
    }

    func testAutomaticModeDoesNotReStageBlocklistedVersion() async throws {
        defaults.set("automatic", forKey: "boss.update.mode")
        let installer = makeInstaller()
        installer.blockVersion(mockVersion)
        let recorder = StageCounter()
        let model = makeModel(stager: recorder.stager(), installer: installer)

        await model.checkNow()
        await model.awaitStagingForTesting()

        XCTAssertEqual(model.downloadState, .idle, "a blocklisted version must not be auto-staged")
        let count = await recorder.count
        XCTAssertEqual(count, 0)
        let skipped = log.allRecords().filter { $0.stage == "stage" && $0.event == "skipped-blocklisted" }
        XCTAssertEqual(skipped.count, 1, "the refusal is recorded in the update log")
        XCTAssertEqual(skipped.first?.to, "1.0.99")
        XCTAssertEqual(log.allRecords().first { $0.stage == "check" }?.detail["blocked"], "true")
    }

    func testDownloadButtonRefusesBlocklistedVersion() async throws {
        let installer = makeInstaller()
        installer.blockVersion(mockVersion)
        let recorder = StageCounter()
        let model = makeModel(stager: recorder.stager(), installer: installer)

        await model.checkNow()
        model.downloadAvailableUpdate()
        await model.awaitStagingForTesting()

        XCTAssertEqual(model.downloadState, .idle)
        let count = await recorder.count
        XCTAssertEqual(count, 0, "plain Download is not an explicit retry")
        XCTAssertTrue(model.isBlocked(mockVersion))
    }

    func testRetryUnblocksStagesAndPresentsSheet() async throws {
        let installer = makeInstaller()
        installer.blockVersion(mockVersion)
        let recorder = StageCounter()
        let model = makeModel(stager: recorder.stager(), installer: installer)
        await model.checkNow()

        model.retryBlockedUpdate(mockVersion)
        await model.awaitStagingForTesting()

        XCTAssertFalse(installer.isBlocked(mockVersion), "retry clears the block")
        XCTAssertEqual(model.downloadState, .readyToInstall(version: mockVersion))
        XCTAssertTrue(model.showUpdateSheet, "the sheet opens so the user can press Install & Relaunch")
        let count = await recorder.count
        XCTAssertEqual(count, 1)
        let events = log.allRecords().map { "\($0.stage).\($0.event)" }
        XCTAssertTrue(events.contains("blocklist.retry-requested"), "\(events)")
        XCTAssertTrue(events.contains("blocklist.removed"), "\(events)")
        // The retry is one new attempt from request through staging.
        let records = log.allRecords()
        let retry = try XCTUnwrap(records.first { $0.event == "retry-requested" }?.attempt)
        XCTAssertNotEqual(retry, records.first { $0.event == "available" }?.attempt, "a retry is a new attempt")
        XCTAssertEqual(records.first { $0.stage == "blocklist" && $0.event == "removed" }?.attempt, retry)
        XCTAssertEqual(records.last { $0.stage == "stage" && $0.event == "begin" }?.attempt, retry)
    }

    func testRetryWithoutPriorCheckChecksFirst() async throws {
        let installer = makeInstaller()
        installer.blockVersion(mockVersion)
        let recorder = StageCounter()
        let model = makeModel(stager: recorder.stager(), installer: installer)

        model.retryBlockedUpdate(mockVersion)
        // The retry runs an async check before staging; wait for it to settle.
        for _ in 0..<200 {
            if case .readyToInstall = model.downloadState { break }
            try await Task.sleep(for: .milliseconds(10))
            await model.awaitStagingForTesting()
        }

        XCTAssertEqual(model.downloadState, .readyToInstall(version: mockVersion))
        XCTAssertNotNil(model.lastCheckResult, "retry ran a check when none had happened yet")
    }

    func testRetryRecheckNetworkErrorReportsViaToastNotSheet() async throws {
        let installer = makeInstaller()
        installer.blockVersion(mockVersion)
        let model = makeModel(stager: .noop, installer: installer, fetcher: failingFetcher(status: 500))

        model.retryBlockedUpdate(mockVersion)
        XCTAssertFalse(model.showUpdateSheet, "no sheet while the re-check is in flight")
        XCTAssertEqual(model.manualCheckFeedback, .checking)
        try await waitForFeedback(model) { if case .networkError = $0 { return true } else { return false } }
        XCTAssertFalse(model.showUpdateSheet)
    }

    func testRetryRecheckRateLimitedReportsViaToastNotSheet() async throws {
        let installer = makeInstaller()
        installer.blockVersion(mockVersion)
        let model = makeModel(stager: .noop, installer: installer, fetcher: failingFetcher(status: 429))

        model.retryBlockedUpdate(mockVersion)
        try await waitForFeedback(model) { if case .rateLimited = $0 { return true } else { return false } }
        XCTAssertFalse(model.showUpdateSheet)
    }

    func testRetryRecheckUpToDateReportsViaToastNotSheet() async throws {
        let installer = makeInstaller()
        installer.blockVersion(mockVersion)
        let model = makeModel(stager: .noop, installer: installer, fetcher: emptyFeedFetcher())

        model.retryBlockedUpdate(mockVersion)
        try await waitForFeedback(model) { $0 == .upToDate }
        XCTAssertFalse(model.showUpdateSheet)
    }

    func testRollbackNoticeSurfacesUntilDismissed() throws {
        let installer = makeInstaller()
        try plantRollback(installer: installer, version: "1.0.99", reason: "handshake-timeout")
        let model = makeModel(stager: .noop, installer: installer)

        XCTAssertNil(model.rollbackNotice, "nothing until refreshed at launch")
        model.refreshRollbackNotice()
        let notice = try XCTUnwrap(model.rollbackNotice)
        XCTAssertEqual(notice.version, "1.0.99")
        XCTAssertEqual(notice.reason, .handshakeTimeout)
        XCTAssertEqual(notice.noticeText,
                       "Update to 1.0.99 failed: the new version did not finish launching in time; rolled back to 1.0.98")

        // Survives a second launch until dismissed…
        let again = makeModel(stager: .noop, installer: installer)
        again.refreshRollbackNotice()
        XCTAssertNotNil(again.rollbackNotice)

        model.dismissRollbackNotice()
        XCTAssertNil(model.rollbackNotice)
        XCTAssertTrue(installer.isBlocked(mockVersion), "dismissing keeps the block")

        // …and stays dismissed afterwards.
        let afterDismiss = makeModel(stager: .noop, installer: installer)
        afterDismiss.refreshRollbackNotice()
        XCTAssertNil(afterDismiss.rollbackNotice)
    }

    func testRetryClearsRollbackNotice() async throws {
        let installer = makeInstaller()
        try plantRollback(installer: installer, version: "1.0.99", reason: "process-exited")
        let recorder = StageCounter()
        let model = makeModel(stager: recorder.stager(), installer: installer)
        model.refreshRollbackNotice()
        XCTAssertNotNil(model.rollbackNotice)
        XCTAssertEqual(model.rollbackRecord(for: mockVersion)?.reason, .processExited)

        model.retryBlockedUpdate(mockVersion)
        XCTAssertNil(model.rollbackNotice)
        XCTAssertNil(makeModel(stager: .noop, installer: installer).rollbackNoticeAfterRefresh(),
                     "retry acknowledges the rollback durably")
    }

    // MARK: - Fixtures

    private var updatesDir: URL { tempRoot.appendingPathComponent("Updates", isDirectory: true) }
    private var log: UpdateLog { UpdateLog(directory: UpdateLog.directory(forUpdatesDirectory: updatesDir)) }

    private func makeInstaller() -> UpdateInstaller {
        UpdateInstaller(
            updatesDirectory: updatesDir,
            installBundleURL: tempRoot.appendingPathComponent("Applications/Boss.app"),
            log: log)
    }

    private func failingFetcher(status: Int) -> HTTPFetcher {
        HTTPFetcher { _ in
            let response = HTTPURLResponse(
                url: UpdateChecker.releasesURL, statusCode: status, httpVersion: nil,
                headerFields: status == 429 ? ["Retry-After": "60"] : nil)!
            return (Data(), response)
        }
    }

    private func emptyFeedFetcher() -> HTTPFetcher {
        HTTPFetcher { _ in
            let data = try JSONSerialization.data(withJSONObject: [Any]())
            let response = HTTPURLResponse(
                url: UpdateChecker.releasesURL, statusCode: 200, httpVersion: nil, headerFields: nil)!
            return (data, response)
        }
    }

    private func waitForFeedback(
        _ model: UpdateModel, _ matches: (ManualUpdateFeedback) -> Bool
    ) async throws {
        for _ in 0..<300 {
            if let feedback = model.manualCheckFeedback, matches(feedback) { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTFail("expected feedback not reached; got \(String(describing: model.manualCheckFeedback))")
    }

    private func makeModel(
        stager: UpdateStager, installer: UpdateInstaller, fetcher customFetcher: HTTPFetcher? = nil
    ) -> UpdateModel {
        let fetcher = customFetcher ?? HTTPFetcher { _ in
            let asset: [String: Any] = [
                "name": "Boss-1.0.99.zip", "size": 34_000_000,
                "browser_download_url": "https://github.com/spinyfin/mono/releases/download/boss-v1.0.99/Boss-1.0.99.zip",
            ]
            let release: [String: Any] = [
                "tag_name": "boss-v1.0.99", "draft": false, "prerelease": false, "body": "notes", "assets": [asset],
            ]
            let data = try JSONSerialization.data(withJSONObject: [release])
            let response = HTTPURLResponse(
                url: UpdateChecker.releasesURL, statusCode: 200, httpVersion: nil, headerFields: nil)!
            return (data, response)
        }
        let checker = UpdateChecker(currentVersionString: "1.0.98", fullVersionString: "1.0.98", fetcher: fetcher)
        return UpdateModel(
            checker: checker, defaults: defaults, jitterRange: 0...0, stager: stager,
            blocklist: .live(installer: installer), log: log)
    }

    /// Simulate a prior launch having reconciled a watchdog rollback of `version`.
    private func plantRollback(installer: UpdateInstaller, version: String, reason: String) throws {
        try FileManager.default.createDirectory(at: updatesDir, withIntermediateDirectories: true)
        let marker = "version=\(version)\nreason=\(reason)\ndetail=simulated\nattempt=upd-1-test\nfrom=1.0.98\nrestored=1.0.98\n"
        try marker.write(to: installer.rolledBackMarkerURL, atomically: true, encoding: .utf8)
        guard case .rolledBack = installer.reconcileAfterLaunch(currentVersion: VersionTuple.parse("1.0.98")!) else {
            return XCTFail("fixture: reconcile must fold the marker")
        }
    }
}

private extension UpdateModel {
    func rollbackNoticeAfterRefresh() -> RollbackRecord? {
        refreshRollbackNotice()
        return rollbackNotice
    }
}

/// Counts stage calls; always "succeeds" with the requested version.
private actor StageCounter {
    private(set) var count = 0
    func increment() { count += 1 }

    nonisolated func stager() -> UpdateStager {
        UpdateStager { [self] update, _ in
            await self.increment()
            return update.version
        }
    }
}
