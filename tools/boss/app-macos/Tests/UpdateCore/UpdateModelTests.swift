import XCTest
@testable import UpdateCore

// MARK: - UpdateModel tests

/// Covers `UpdateModel` and `UpdateMode`:
/// - Mode enum raw values and round-trip
/// - Default state and UserDefaults persistence across instances
/// - `checkNow()` result propagation and lastCheckDate bookkeeping
/// - `skipCurrentVersion()` / `clearSkippedVersion()` persistence
/// - `setMode(_:)` transitions and their effect on the polling scheduler
/// - Polling start/stop based on mode (scheduler integration smoke tests)
/// - Rate-limit result propagation
@MainActor
final class UpdateModelTests: XCTestCase {

    private var defaults: UserDefaults!
    private var suiteName: String!

    override func setUp() async throws {
        try await super.setUp()
        suiteName = "com.boss.test.UpdateModelTests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suiteName)!
    }

    override func tearDown() async throws {
        defaults.removePersistentDomain(forName: suiteName)
        defaults = nil
        suiteName = nil
        try await super.tearDown()
    }

    // MARK: - UpdateMode enum

    func testUpdateModeRawValues() {
        XCTAssertEqual(UpdateMode.manual.rawValue, "manual")
        XCTAssertEqual(UpdateMode.notify.rawValue, "notify")
        XCTAssertEqual(UpdateMode.automatic.rawValue, "automatic")
    }

    func testUpdateModeRoundTrip() {
        for mode in UpdateMode.allCases {
            XCTAssertEqual(UpdateMode(rawValue: mode.rawValue), mode)
        }
    }

    func testUpdateModeAllCasesCount() {
        XCTAssertEqual(UpdateMode.allCases.count, 3)
    }

    func testUpdateModeInvalidRawValue() {
        XCTAssertNil(UpdateMode(rawValue: "turbo"))
    }

    // MARK: - Initialization defaults

    func testDefaultModeIsNotify() {
        let model = makeModel(result: .upToDate)
        XCTAssertEqual(model.mode, .notify)
    }

    func testDefaultLastCheckDateIsNil() {
        let model = makeModel(result: .upToDate)
        XCTAssertNil(model.lastCheckDate)
    }

    func testDefaultSkippedVersionIsEmpty() {
        let model = makeModel(result: .upToDate)
        XCTAssertEqual(model.skippedVersion, "")
    }

    func testDefaultLastCheckResultIsNil() {
        let model = makeModel(result: .upToDate)
        XCTAssertNil(model.lastCheckResult)
    }

    func testDefaultIsCheckingIsFalse() {
        let model = makeModel(result: .upToDate)
        XCTAssertFalse(model.isChecking)
    }

    // MARK: - UserDefaults persistence on init

    func testReadsModeFromUserDefaults() {
        defaults.set("automatic", forKey: "boss.update.mode")
        let model = makeModel(result: .upToDate)
        XCTAssertEqual(model.mode, .automatic)
    }

    func testFallsBackToNotifyForUnknownStoredMode() {
        defaults.set("warp-speed", forKey: "boss.update.mode")
        let model = makeModel(result: .upToDate)
        XCTAssertEqual(model.mode, .notify)
    }

    func testReadsLastCheckFromUserDefaults() {
        let epoch: Double = 1_700_000_000
        defaults.set(epoch, forKey: "boss.update.lastCheck")
        let model = makeModel(result: .upToDate)
        XCTAssertEqual(model.lastCheckDate?.timeIntervalSince1970 ?? 0, epoch, accuracy: 0.001)
    }

    func testLastCheckDateNilWhenStoredEpochIsZero() {
        defaults.set(0.0, forKey: "boss.update.lastCheck")
        let model = makeModel(result: .upToDate)
        XCTAssertNil(model.lastCheckDate)
    }

    func testReadsSkippedVersionFromUserDefaults() {
        defaults.set("1.0.42", forKey: "boss.update.skippedVersion")
        let model = makeModel(result: .upToDate)
        XCTAssertEqual(model.skippedVersion, "1.0.42")
    }

    // MARK: - checkNow

    func testCheckNowSetsUpToDateResult() async {
        let model = makeModel(result: .upToDate)
        let result = await model.checkNow()
        XCTAssertEqual(result, .upToDate)
        XCTAssertEqual(model.lastCheckResult, .upToDate)
    }

    func testCheckNowSetsLastCheckDate() async {
        let before = Date()
        let model = makeModel(result: .upToDate)
        await model.checkNow()
        let after = Date()
        guard let checkDate = model.lastCheckDate else {
            return XCTFail("lastCheckDate should be set after checkNow()")
        }
        XCTAssertGreaterThanOrEqual(checkDate.timeIntervalSince1970, before.timeIntervalSince1970 - 1)
        XCTAssertLessThanOrEqual(checkDate.timeIntervalSince1970, after.timeIntervalSince1970 + 1)
    }

    func testCheckNowPersistsLastCheckEpochToDefaults() async {
        let model = makeModel(result: .upToDate)
        await model.checkNow()
        XCTAssertGreaterThan(defaults.double(forKey: "boss.update.lastCheck"), 0)
    }

    func testCheckNowReturnsNetworkError() async {
        let model = makeModel(result: .networkError("mock error"))
        let result = await model.checkNow()
        if case .networkError = result {
            // expected
        } else {
            XCTFail("Expected .networkError, got \(result)")
        }
    }

    func testCheckNowReturnsRateLimited() async {
        let retryDate = Date(timeIntervalSinceNow: 3600)
        let model = makeModel(result: .rateLimited(retryAfter: retryDate))
        let result = await model.checkNow()
        if case .rateLimited(let date) = result {
            XCTAssertEqual(date.timeIntervalSince1970, retryDate.timeIntervalSince1970, accuracy: 2.0)
        } else {
            XCTFail("Expected .rateLimited, got \(result)")
        }
    }

    func testCheckNowReturnsAvailable() async {
        // Mock returns boss-v1.0.99; checker (current: 1.0.0) → .available
        let model = makeModel(result: .availableMock)
        let result = await model.checkNow()
        if case .available(let update) = result {
            XCTAssertEqual(update.version.description, "1.0.99")
        } else {
            XCTFail("Expected .available, got \(result)")
        }
    }

    func testIsCheckingFalseAfterCheck() async {
        let model = makeModel(result: .upToDate)
        await model.checkNow()
        XCTAssertFalse(model.isChecking)
    }

    // MARK: - skipCurrentVersion / clearSkippedVersion

    func testSkipCurrentVersionStoresVersion() async {
        let model = makeModel(result: .availableMock)
        await model.checkNow()
        model.skipCurrentVersion()
        XCTAssertEqual(model.skippedVersion, "1.0.99")
    }

    func testSkipCurrentVersionPersistsToDefaults() async {
        let model = makeModel(result: .availableMock)
        await model.checkNow()
        model.skipCurrentVersion()
        XCTAssertEqual(defaults.string(forKey: "boss.update.skippedVersion"), "1.0.99")
    }

    func testSkipCurrentVersionNoOpWhenUpToDate() async {
        let model = makeModel(result: .upToDate)
        await model.checkNow()
        model.skipCurrentVersion()
        XCTAssertEqual(model.skippedVersion, "")
    }

    func testSkipCurrentVersionNoOpWhenNotYetChecked() {
        let model = makeModel(result: .upToDate)
        model.skipCurrentVersion()
        XCTAssertEqual(model.skippedVersion, "")
    }

    func testClearSkippedVersionEmptiesPublishedValue() async {
        let model = makeModel(result: .availableMock)
        await model.checkNow()
        model.skipCurrentVersion()
        XCTAssertEqual(model.skippedVersion, "1.0.99")
        model.clearSkippedVersion()
        XCTAssertEqual(model.skippedVersion, "")
    }

    func testClearSkippedVersionRemovesFromDefaults() {
        defaults.set("1.0.42", forKey: "boss.update.skippedVersion")
        let model = makeModel(result: .upToDate)
        model.clearSkippedVersion()
        XCTAssertNil(defaults.string(forKey: "boss.update.skippedVersion"))
    }

    // MARK: - presentUpdateSheet (CheckForUpdates command regression guard)
    // These tests guard the fix for the "Check for Updates is a no-op" bug where
    // CheckForUpdatesCommand resolved the model via NSApp.delegate, which returns
    // SwiftUI's internal wrapper under @NSApplicationDelegateAdaptor — not AppDelegate.
    // The fix passes the model directly; these tests confirm the model is reachable
    // and that calling presentUpdateSheet() actually triggers a check.

    func testPresentUpdateSheetSetsCheckingFeedbackImmediately() {
        let model = makeModel(result: .upToDate)
        model.presentUpdateSheet()
        // Feedback transitions to .checking synchronously before the async Task fires.
        // If the model were nil (the old bug), this line would never be reached.
        XCTAssertEqual(model.manualCheckFeedback, .checking)
    }

    func testPresentUpdateSheetTriggersCheckAndResolvesUpToDate() async throws {
        let model = makeModel(result: .upToDate)
        model.presentUpdateSheet()
        XCTAssertEqual(model.manualCheckFeedback, .checking)
        // Wait for the async check to settle.
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(model.manualCheckFeedback, .upToDate)
    }

    func testPresentUpdateSheetTriggersCheckAndResolvesAvailable() async throws {
        let model = makeModel(result: .availableMock)
        model.presentUpdateSheet()
        XCTAssertEqual(model.manualCheckFeedback, .checking)
        try await Task.sleep(for: .milliseconds(200))
        // Available result clears feedback and shows the sheet instead.
        XCTAssertNil(model.manualCheckFeedback)
        XCTAssertTrue(model.showUpdateSheet)
    }

    func testPresentUpdateSheetIsIdempotentOnRepeat() async throws {
        let model = makeModel(result: .upToDate)
        model.presentUpdateSheet()
        model.presentUpdateSheet()  // second call before first settles
        XCTAssertEqual(model.manualCheckFeedback, .checking)
        try await Task.sleep(for: .milliseconds(200))
        // Last check wins; feedback should be .upToDate, not stuck at .checking.
        XCTAssertNotEqual(model.manualCheckFeedback, .checking)
    }

    // MARK: - setMode

    func testSetModeUpdatesPublishedProperty() {
        let model = makeModel(result: .upToDate)
        model.setMode(.automatic)
        XCTAssertEqual(model.mode, .automatic)
    }

    func testSetModePersistsToDefaults() {
        let model = makeModel(result: .upToDate)
        model.setMode(.automatic)
        XCTAssertEqual(defaults.string(forKey: "boss.update.mode"), "automatic")
    }

    func testModePersistsAcrossInstances() {
        let model1 = makeModel(result: .upToDate)
        model1.setMode(.automatic)
        let model2 = makeModel(result: .upToDate)
        XCTAssertEqual(model2.mode, .automatic)
    }

    func testSetModeToManualSetsCorrectMode() {
        let model = makeModel(result: .upToDate)
        model.setMode(.manual)
        XCTAssertEqual(model.mode, .manual)
    }

    // MARK: - Polling scheduler (smoke tests)

    func testManualModeDoesNotPollOnStartPollingIfNeeded() async throws {
        let (checker, counter) = makeCheckerAndCounter(result: .upToDate)
        let model = UpdateModel(checker: checker, defaults: defaults, jitterRange: 0...0)
        model.setMode(.manual)
        model.startPollingIfNeeded()
        // Yield to any queued work; manual mode must not trigger a check.
        try await Task.sleep(for: .milliseconds(50))
        let count = await counter.value
        XCTAssertEqual(count, 0)
    }

    func testNotifyModeStartsPollingOnStartPollingIfNeeded() async throws {
        let (checker, counter) = makeCheckerAndCounter(result: .upToDate)
        // Zero-jitter ensures the first check fires immediately after startPollingIfNeeded.
        let model = UpdateModel(checker: checker, defaults: defaults, jitterRange: 0...0)
        // Default mode is .notify; calling startPollingIfNeeded should trigger a check.
        model.startPollingIfNeeded()
        try await Task.sleep(for: .milliseconds(200))
        let count = await counter.value
        XCTAssertGreaterThan(count, 0)
    }

    func testAutomaticModeStartsPolling() async throws {
        let (checker, counter) = makeCheckerAndCounter(result: .upToDate)
        let model = UpdateModel(checker: checker, defaults: defaults, jitterRange: 0...0)
        model.setMode(.automatic)
        model.startPollingIfNeeded()
        try await Task.sleep(for: .milliseconds(200))
        let count = await counter.value
        XCTAssertGreaterThan(count, 0)
    }

    func testSwitchingToManualStopsPolling() async throws {
        let (checker, counter) = makeCheckerAndCounter(result: .upToDate)
        let model = UpdateModel(checker: checker, defaults: defaults, jitterRange: 0...0)
        model.startPollingIfNeeded()
        // Let at least one poll fire.
        try await Task.sleep(for: .milliseconds(200))
        let countAfterStart = await counter.value
        XCTAssertGreaterThan(countAfterStart, 0)
        // Switch to manual — the 6-hour sleep gets cancelled; no further checks should occur.
        model.setMode(.manual)
        let countAtStop = await counter.value
        try await Task.sleep(for: .milliseconds(100))
        let countAfterStop = await counter.value
        XCTAssertEqual(countAtStop, countAfterStop, "No new checks should fire after switching to manual")
    }

    func testSwitchingFromManualToNotifyStartsPolling() async throws {
        let (checker, counter) = makeCheckerAndCounter(result: .upToDate)
        let model = UpdateModel(checker: checker, defaults: defaults, jitterRange: 0...0)
        model.setMode(.manual)
        let countBeforeNotify = await counter.value
        XCTAssertEqual(countBeforeNotify, 0)
        model.setMode(.notify)
        try await Task.sleep(for: .milliseconds(200))
        let countAfterNotify = await counter.value
        XCTAssertGreaterThan(countAfterNotify, 0)
    }

    // MARK: - Auto-download / staging

    private static let mockVersion = VersionTuple(major: 1, minor: 0, patch: 99)

    func testDefaultDownloadStateIsIdle() {
        let model = makeModel(result: .upToDate)
        XCTAssertEqual(model.downloadState, .idle)
    }

    func testAutomaticModeStagesAvailableUpdate() async throws {
        defaults.set("automatic", forKey: "boss.update.mode")
        let recorder = StagedRecorder()
        let model = makeModel(result: .availableMock, stager: recorder.stager())

        await model.checkNow()
        await model.awaitStagingForTesting()

        XCTAssertEqual(model.downloadState, .readyToInstall(version: Self.mockVersion))
        let count = await recorder.count
        XCTAssertEqual(count, 1, "automatic mode should download/stage exactly once")
    }

    func testDownloadProgressPlumbsIntoDownloadStateWhileDownloading() async throws {
        defaults.set("automatic", forKey: "boss.update.mode")
        let gate = ProgressGate()
        let stager = UpdateStager { update, onProgress in
            onProgress(.determinate(0.4))
            await gate.waitForRelease()
            return update.version
        }
        let model = makeModel(result: .availableMock, stager: stager)

        await model.checkNow()

        // The progress callback lands on the main actor asynchronously; poll
        // briefly for it rather than assuming it has landed by the time
        // `checkNow()` returns.
        var sawProgress = false
        for _ in 0..<50 {
            if case .downloading(_, .determinate(let fraction)) = model.downloadState, fraction == 0.4 {
                sawProgress = true
                break
            }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertTrue(sawProgress, "download progress must reach downloadState before staging completes")

        await gate.release()
        await model.awaitStagingForTesting()
        XCTAssertEqual(model.downloadState, .readyToInstall(version: Self.mockVersion))
    }

    func testNotifyModeDoesNotAutoStage() async throws {
        // Default mode is .notify.
        let recorder = StagedRecorder()
        let model = makeModel(result: .availableMock, stager: recorder.stager())

        await model.checkNow()
        await model.awaitStagingForTesting()

        XCTAssertEqual(model.downloadState, .idle)
        let count = await recorder.count
        XCTAssertEqual(count, 0, "notify mode must only detect, never download")
    }

    func testManualModeDoesNotAutoStage() async throws {
        defaults.set("manual", forKey: "boss.update.mode")
        let recorder = StagedRecorder()
        let model = makeModel(result: .availableMock, stager: recorder.stager())

        await model.checkNow()
        await model.awaitStagingForTesting()

        XCTAssertEqual(model.downloadState, .idle)
        let count = await recorder.count
        XCTAssertEqual(count, 0)
    }

    func testDevBuildDoesNotAutoStageInAutomaticMode() async throws {
        defaults.set("automatic", forKey: "boss.update.mode")
        let recorder = StagedRecorder()
        let model = makeModel(result: .availableMock, stager: recorder.stager(), dev: true)

        await model.checkNow()
        await model.awaitStagingForTesting()

        XCTAssertEqual(model.downloadState, .idle, "dev builds never auto-install")
        let count = await recorder.count
        XCTAssertEqual(count, 0)
    }

    func testUpToDateDoesNotStage() async throws {
        defaults.set("automatic", forKey: "boss.update.mode")
        let recorder = StagedRecorder()
        let model = makeModel(result: .upToDate, stager: recorder.stager())

        await model.checkNow()
        await model.awaitStagingForTesting()

        XCTAssertEqual(model.downloadState, .idle)
        let count = await recorder.count
        XCTAssertEqual(count, 0)
    }

    func testDownloadAvailableUpdateStagesRegardlessOfMode() async throws {
        // Default mode is .notify, which does NOT auto-stage; an explicit user click
        // (the sheet/badge "Download" button) must still stage.
        let recorder = StagedRecorder()
        let model = makeModel(result: .availableMock, stager: recorder.stager())

        await model.checkNow()
        XCTAssertEqual(model.downloadState, .idle)

        model.downloadAvailableUpdate()
        await model.awaitStagingForTesting()

        XCTAssertEqual(model.downloadState, .readyToInstall(version: Self.mockVersion))
        let count = await recorder.count
        XCTAssertEqual(count, 1)
    }

    func testDownloadAvailableUpdateIsNoOpWhenNoUpdate() async throws {
        let recorder = StagedRecorder()
        let model = makeModel(result: .upToDate, stager: recorder.stager())

        await model.checkNow()
        model.downloadAvailableUpdate()
        await model.awaitStagingForTesting()

        XCTAssertEqual(model.downloadState, .idle)
        let count = await recorder.count
        XCTAssertEqual(count, 0)
    }

    func testDownloadAvailableUpdateIsNoOpForDevBuild() async throws {
        let recorder = StagedRecorder()
        let model = makeModel(result: .availableMock, stager: recorder.stager(), dev: true)

        await model.checkNow()
        model.downloadAvailableUpdate()
        await model.awaitStagingForTesting()

        XCTAssertEqual(model.downloadState, .idle)
        let count = await recorder.count
        XCTAssertEqual(count, 0)
    }

    func testStagingFailureSetsFailedState() async throws {
        defaults.set("automatic", forKey: "boss.update.mode")
        // A stager that always fails.
        let model = makeModel(result: .availableMock, stager: UpdateStager { _, _ in nil })

        await model.checkNow()
        await model.awaitStagingForTesting()

        guard case .failed(let version, _) = model.downloadState else {
            return XCTFail("expected .failed, got \(model.downloadState)")
        }
        XCTAssertEqual(version, Self.mockVersion)
    }

    func testReStagingSameReadyVersionIsIdempotent() async throws {
        defaults.set("automatic", forKey: "boss.update.mode")
        let recorder = StagedRecorder()
        let model = makeModel(result: .availableMock, stager: recorder.stager())

        await model.checkNow()
        await model.awaitStagingForTesting()
        XCTAssertEqual(model.downloadState, .readyToInstall(version: Self.mockVersion))

        // A second poll finds the same version; it must not re-download.
        await model.checkNow()
        await model.awaitStagingForTesting()

        let count = await recorder.count
        XCTAssertEqual(count, 1, "an already-ready version must not be re-staged")
    }

    func testInstalledPendingRelaunchVersionIsNotReStaged() async throws {
        defaults.set("automatic", forKey: "boss.update.mode")
        let recorder = StagedRecorder()
        let model = makeModel(result: .availableMock, stager: recorder.stager())

        await model.checkNow()
        await model.awaitStagingForTesting()
        XCTAssertEqual(model.downloadState, .readyToInstall(version: Self.mockVersion))

        // Simulate "Install & Relaunch" applying the swap and the quit being vetoed:
        // the version is installed on disk and only awaiting relaunch.
        model.markInstalledPendingRelaunch(version: Self.mockVersion, willRelaunch: true)

        // A later automatic poll rediscovers the same version. It must NOT re-stage it —
        // re-downloading the already-installed bundle would revert the honest
        // "Installed — quit to finish" UI back to "Downloading…"/"Install & Relaunch".
        await model.checkNow()
        await model.awaitStagingForTesting()

        let count = await recorder.count
        XCTAssertEqual(count, 1, "an installed-pending-relaunch version must not be re-staged")
        XCTAssertEqual(
            model.downloadState,
            .installedPendingRelaunch(version: Self.mockVersion, willRelaunch: true),
            "the honest post-swap state must survive a subsequent poll")
    }

    func testMarkInstalledPendingRelaunchWithHelperSetsState() {
        let model = makeModel(result: .upToDate)
        model.markInstalledPendingRelaunch(version: Self.mockVersion, willRelaunch: true)
        XCTAssertEqual(
            model.downloadState, .installedPendingRelaunch(version: Self.mockVersion, willRelaunch: true))
    }

    func testMarkInstalledPendingRelaunchWithoutHelperSetsState() {
        let model = makeModel(result: .upToDate)
        model.markInstalledPendingRelaunch(version: Self.mockVersion, willRelaunch: false)
        XCTAssertEqual(
            model.downloadState, .installedPendingRelaunch(version: Self.mockVersion, willRelaunch: false))
    }

    // MARK: - Cancellation of in-flight staging

    /// A newer version superseding an in-flight download must cancel the older
    /// staging `Task` (mirroring `DownloadTaskRunner`'s cancellation of the
    /// underlying `URLSessionDownloadTask`) and leave `downloadState` on the
    /// newer version's terminal state rather than stuck reporting the cancelled one.
    func testSupersedingDownloadCancelsInFlightStagingAndLeavesSaneTerminalState() async throws {
        let v1 = VersionTuple(major: 1, minor: 0, patch: 98)
        let v2 = VersionTuple(major: 1, minor: 0, patch: 99)
        let recorder = CancellationTrackingRecorder()

        let callIndex = FetchCounter()
        let fetcher = HTTPFetcher { _ in
            await callIndex.increment()
            let index = await callIndex.value
            let tag = index == 1 ? "boss-v1.0.98" : "boss-v1.0.99"
            return try UpdateModelTests.mockAvailableResponse(tag: tag)
        }
        let checker = UpdateChecker(currentVersionString: "1.0.0", fullVersionString: "1.0.0", fetcher: fetcher)

        let stager = UpdateStager { update, _ in
            if update.version == v1 {
                // Mimic a real in-flight download that notices cancellation cooperatively
                // rather than resolving immediately, bounded so a stalled test fails loudly.
                var waited = 0
                while !Task.isCancelled && waited < 1000 {
                    try? await Task.sleep(nanoseconds: 5_000_000)
                    waited += 1
                }
                await recorder.markV1Observed(cancelled: Task.isCancelled)
                return nil
            }
            await recorder.markV2Staged()
            return update.version
        }

        let model = UpdateModel(checker: checker, defaults: defaults, jitterRange: 0...0, stager: stager)

        await model.checkNow()  // lastCheckResult = .available(v1); notify mode never auto-stages.
        model.downloadAvailableUpdate()  // begins staging v1, which blocks until cancelled.

        var startedV1 = false
        for _ in 0..<50 {
            if case .downloading(let v, _) = model.downloadState, v == v1 { startedV1 = true; break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertTrue(startedV1, "expected staging of v1 to start before it is superseded")

        await model.checkNow()  // lastCheckResult = .available(v2)
        model.downloadAvailableUpdate()  // supersedes v1: cancels its task, begins staging v2.

        await model.awaitStagingForTesting()

        XCTAssertEqual(
            model.downloadState, .readyToInstall(version: v2),
            "must land on v2's ready state, not stuck reporting the cancelled v1 download")
        let v1Cancelled = await recorder.v1WasCancelled
        XCTAssertTrue(v1Cancelled, "the superseded v1 staging must observe cancellation")
        let v2Staged = await recorder.v2Staged
        XCTAssertTrue(v2Staged)
    }
}

// MARK: - Helpers

extension UpdateModelTests {

    private func makeModel(result: UpdateCheckResult) -> UpdateModel {
        let (checker, _) = makeCheckerAndCounter(result: result)
        return UpdateModel(checker: checker, defaults: defaults, jitterRange: 0...0)
    }

    /// Builds a model with an injected stager (and optionally a dev-build checker) for
    /// the auto-download/staging tests.
    private func makeModel(
        result: UpdateCheckResult,
        stager: UpdateStager,
        dev: Bool = false
    ) -> UpdateModel {
        let (checker, _) = makeCheckerAndCounter(result: result, dev: dev)
        return UpdateModel(checker: checker, defaults: defaults, jitterRange: 0...0, stager: stager)
    }

    private func makeCheckerAndCounter(
        result: UpdateCheckResult,
        dev: Bool = false
    ) -> (UpdateChecker, FetchCounter) {
        let counter = FetchCounter()
        let fetcher = HTTPFetcher { [result] _ in
            await counter.increment()
            return try UpdateModelTests.mockResponse(for: result)
        }
        let checker = UpdateChecker(
            currentVersionString: "1.0.0",
            fullVersionString: dev ? "1.0.0-dev-abc1234" : "1.0.0",
            fetcher: fetcher
        )
        return (checker, counter)
    }

    /// Converts a desired `UpdateCheckResult` into the raw HTTP response that would cause
    /// `UpdateChecker` to produce that result. `nonisolated` so it can be called from
    /// the `@Sendable` HTTPFetcher closure.
    private nonisolated static func mockResponse(for result: UpdateCheckResult) throws -> (Data, HTTPURLResponse) {
        let url = UpdateChecker.releasesURL
        switch result {
        case .upToDate:
            let data = try JSONSerialization.data(withJSONObject: [Any]())
            return (data, HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: nil)!)

        case .available:
            // Always return boss-v1.0.99 so callers can assert on "1.0.99".
            let release: [String: Any] = [
                "tag_name": "boss-v1.0.99",
                "draft": false,
                "prerelease": false,
                "body": "Mock release notes",
                "assets": [[
                    "name": "Boss-1.0.99.zip",
                    "size": 34_000_000,
                    "browser_download_url":
                        "https://github.com/spinyfin/mono/releases/download/boss-v1.0.99/Boss-1.0.99.zip",
                ]],
            ]
            let data = try JSONSerialization.data(withJSONObject: [release])
            return (data, HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: nil)!)

        case .rateLimited(let retryAfter):
            let headers = ["X-RateLimit-Reset": String(Int(retryAfter.timeIntervalSince1970))]
            let response = HTTPURLResponse(url: url, statusCode: 429, httpVersion: nil, headerFields: headers)!
            return (Data(), response)

        case .networkError:
            throw URLError(.notConnectedToInternet)
        }
    }

    /// A raw GitHub releases-list HTTP response advertising a single available
    /// release `tag`, for tests that need distinct successive versions (unlike
    /// `mockResponse(for:)`, which always reports the fixed `boss-v1.0.99`).
    private nonisolated static func mockAvailableResponse(tag: String) throws -> (Data, HTTPURLResponse) {
        let url = UpdateChecker.releasesURL
        // `selectBestRelease` requires the asset name to be exactly
        // "Boss-<major>.<minor>.<patch>.zip" — derive it from the tag rather than
        // duplicating the "boss-v" prefix.
        let version = String(tag.dropFirst("boss-v".count))
        let release: [String: Any] = [
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "body": "Mock release notes",
            "assets": [[
                "name": "Boss-\(version).zip",
                "size": 1024,
                "browser_download_url":
                    "https://github.com/spinyfin/mono/releases/download/\(tag)/Boss-\(version).zip",
            ]],
        ]
        let data = try JSONSerialization.data(withJSONObject: [release])
        return (data, HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: nil)!)
    }
}

// MARK: - UpdateCheckResult convenience sentinel

private extension UpdateCheckResult {
    /// A sentinel `.available` value backed by the mock HTTP response (boss-v1.0.99).
    static let availableMock: UpdateCheckResult = .available(
        AvailableUpdate(
            tagName: "boss-v1.0.99",
            version: VersionTuple(major: 1, minor: 0, patch: 99),
            assetURL: URL(string: "https://github.com/spinyfin/mono/releases/download/boss-v1.0.99/Boss-1.0.99.zip")!,
            assetSize: 34_000_000,
            releaseNotes: "Mock release notes"
        )
    )
}

// MARK: - Thread-safe call counter

actor FetchCounter {
    private(set) var value: Int = 0
    func increment() { value += 1 }
}

// MARK: - Progress-test synchronization gate

/// Lets a fake ``UpdateStager`` suspend mid-stage so a test can observe an
/// intermediate ``UpdateDownloadState/downloading(version:progress:)`` value
/// before staging completes.
actor ProgressGate {
    private var continuation: CheckedContinuation<Void, Never>?
    private var released = false

    func waitForRelease() async {
        if released { return }
        await withCheckedContinuation { continuation = $0 }
    }

    func release() {
        released = true
        continuation?.resume()
        continuation = nil
    }
}

// MARK: - Cancellation-observing recorder

/// Records whether the superseded (v1) staging closure observed `Task.isCancelled`
/// before returning, and whether the superseding (v2) staging closure ran to
/// completion. Backs `testSupersedingDownloadCancelsInFlightStagingAndLeavesSaneTerminalState`.
actor CancellationTrackingRecorder {
    private(set) var v1WasCancelled = false
    private(set) var v2Staged = false

    func markV1Observed(cancelled: Bool) { v1WasCancelled = cancelled }
    func markV2Staged() { v2Staged = true }
}

// MARK: - Recording stager

/// A fake ``UpdateStager`` that counts how many times it was asked to stage and
/// reports a staged version (or `nil` to simulate failure). Lets the model tests
/// assert both the resulting `downloadState` and that staging happened exactly the
/// expected number of times, without any network or filesystem.
actor StagedRecorder {
    private(set) var count = 0
    private let returns: VersionTuple?

    /// `returns` is the version the stager reports on success; `nil` simulates a
    /// failed download. Defaults to the mock feed's `1.0.99`.
    init(returning returns: VersionTuple? = VersionTuple(major: 1, minor: 0, patch: 99)) {
        self.returns = returns
    }

    private func record() { count += 1 }

    nonisolated func stager() -> UpdateStager {
        let returns = self.returns  // immutable Sendable capture
        return UpdateStager { update, _ in
            await self.record()
            // Echo the requested version so the model's `.readyToInstall` carries the
            // value the checker produced (rather than a hardcoded constant).
            return returns == nil ? nil : update.version
        }
    }
}
