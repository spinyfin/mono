import XCTest
@testable import UpdateCore

/// The attempt id minted when a version is staged must follow it through the manifest,
/// `ReadyUpdate`, the `SwapPlan`, the pending record, and the helper invocation, so
/// download records and swap/relaunch records share one id in the update log.
final class UpdateAttemptTrailTests: XCTestCase {

    private var tempRoot: URL!

    override func setUpWithError() throws {
        tempRoot = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
            isDirectory: true
        ).appendingPathComponent("UpdateAttemptTrailTests-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: updatesDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: installParent, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        if let tempRoot { try? FileManager.default.removeItem(at: tempRoot) }
    }

    func testAttemptIDFlowsFromStagingThroughSwapToHelperInvocation() async throws {
        let log = UpdateLog(directory: UpdateLog.directory(forUpdatesDirectory: updatesDir))
        let update = AvailableUpdate(
            tagName: "boss-v1.0.28", version: v("1.0.28"),
            assetURL: URL(string: "https://example.invalid/boss-v1.0.28/Boss-1.0.28.zip")!,
            assetSize: 64, releaseNotes: "")
        let root = tempRoot!
        let downloader = UpdateDownloader(
            updatesDirectory: updatesDir, currentVersion: v("1.0.27"), runningTeamID: nil,
            assetDownloader: AssetDownloader { _, onProgress in
                let file = root.appendingPathComponent("dl-\(UUID().uuidString).zip")
                _ = FileManager.default.createFile(atPath: file.path, contents: Data(count: 64))
                onProgress(.determinate(1.0))
                return file
            },
            bundleOps: BundleOperations(
                extract: { _, dest in
                    let bundle = dest.appendingPathComponent("Boss.app")
                    try FileManager.default.createDirectory(
                        at: bundle.appendingPathComponent("Contents"), withIntermediateDirectories: true)
                    let plist: [String: Any] = ["CFBundleShortVersionString": "1.0.28"]
                    let data = try PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0)
                    try data.write(to: bundle.appendingPathComponent("Contents/Info.plist"))
                },
                verifyCodeSignature: { _ in },
                readTeamID: { _ in nil },
                stripQuarantine: { _ in }),
            log: log)

        // 1. Stage: the downloader mints the attempt and records every step under it.
        guard case .ready(let staged) = await downloader.download(update) else {
            return XCTFail("download must succeed")
        }
        let attempt = try XCTUnwrap(staged.attemptID)
        XCTAssertTrue(attempt.hasPrefix("upd-"))
        let manifestData = try Data(contentsOf: staged.manifestURL)
        let manifest = try JSONDecoder().decode(UpdateManifest.self, from: manifestData)
        XCTAssertEqual(manifest.attemptID, attempt, "attempt id persisted in manifest.json")

        let stagingEvents = log.records(attempt: attempt).map { "\($0.stage).\($0.event)" }
        XCTAssertEqual(stagingEvents, [
            "download.start", "download.complete", "verify.size-ok", "verify.sha256", "extract.complete",
            "verify.codesign-ok", "verify.team-id-ok", "stage.quarantine-stripped", "stage.ready",
        ])
        let start = log.records(attempt: attempt).first
        XCTAssertEqual(start?.detail["url"], update.assetURL.absoluteString)
        XCTAssertEqual(start?.detail["expectedBytes"], "64")
        XCTAssertEqual(start?.from, "1.0.27")
        XCTAssertEqual(start?.to, "1.0.28")
        XCTAssertNotNil(log.records(attempt: attempt).first { $0.event == "complete" }?.detail["durationMs"])
        XCTAssertEqual(log.records(attempt: attempt).first { $0.event == "sha256" }?.detail["sha256"], manifest.sha256)

        // 2. Discover + plan + swap: the installer reads the id back from the manifest.
        try makeBundle(at: installBundle, version: "1.0.27")
        let installer = UpdateInstaller(updatesDirectory: updatesDir, installBundleURL: installBundle, log: log)
        let ready = try XCTUnwrap(installer.newestReadyUpdate(currentVersion: v("1.0.27")))
        XCTAssertEqual(ready.attemptID, attempt)
        guard case .swap(let plan) = installer.planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: true) else {
            return XCTFail("expected a swap plan")
        }
        XCTAssertEqual(plan.attemptID, attempt)
        XCTAssertEqual(plan.fromVersion, v("1.0.27"))
        try installer.applySwap(plan)
        XCTAssertEqual(installer.pendingSwap()?.attemptID, attempt, "pending record carries the attempt")
        XCTAssertEqual(installer.pendingSwap()?.fromVersion, v("1.0.27"))

        let swapEvents = log.records(attempt: attempt).filter { $0.stage == "swap" }.map(\.event)
        XCTAssertEqual(swapEvents, ["begin", "moved-current-to-backup", "moved-staged-to-install", "committed"])
        XCTAssertEqual(log.records(attempt: attempt).last?.detail["installedVersion"], "1.0.28")

        // 3. Helper invocation hands the same id (and both versions, and the log) on.
        let invocation = installer.helperInvocation(
            scriptURL: URL(fileURLWithPath: "/x/relaunch-helper.sh"), plan: plan, bossPID: 1, watchdogSeconds: 30)
        XCTAssertEqual(argPair(invocation.arguments, "--attempt"), attempt)
        XCTAssertEqual(argPair(invocation.arguments, "--from"), "1.0.27")
        XCTAssertEqual(argPair(invocation.arguments, "--log"), log.fileURL.path)

        // 4. Reconcile in the "new app": the handshake and completion join the trail.
        XCTAssertEqual(installer.reconcileAfterLaunch(currentVersion: v("1.0.28")), .installed(v("1.0.28")))
        let tail = log.records(attempt: attempt).suffix(2).map { "\($0.stage).\($0.event)" }
        XCTAssertEqual(tail, ["handshake.flag-written", "reconcile.installed"])
    }

    func testPlanSwapMintsAttemptIDForLegacyManifestWithoutOne() throws {
        try makeBundle(at: installBundle, version: "1.0.27")
        let dir = updatesDir.appendingPathComponent("1.0.28")
        try makeBundle(at: dir.appendingPathComponent("Boss.app"), version: "1.0.28")
        let legacy = UpdateManifest(
            version: "1.0.28", tag: "boss-v1.0.28", sourceURL: "https://example/1.0.28.zip",
            etag: nil, sha256: nil, verifiedAt: "2026-01-01T00:00:00Z", state: .ready, failureReason: nil)
        try JSONEncoder().encode(legacy).write(to: dir.appendingPathComponent("manifest.json"))

        let installer = UpdateInstaller(updatesDirectory: updatesDir, installBundleURL: installBundle)
        let ready = try XCTUnwrap(installer.newestReadyUpdate(currentVersion: v("1.0.27")))
        XCTAssertNil(ready.attemptID, "legacy manifest has no attempt id")
        guard case .swap(let plan) = installer.planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: true) else {
            return XCTFail("expected a swap plan")
        }
        XCTAssertTrue(plan.attemptID.hasPrefix("upd-"), "a fresh id is minted so the relaunch is still traceable")
    }

    // MARK: Fixtures

    private var updatesDir: URL { tempRoot.appendingPathComponent("Updates", isDirectory: true) }
    private var installParent: URL { tempRoot.appendingPathComponent("Applications", isDirectory: true) }
    private var installBundle: URL { installParent.appendingPathComponent("Boss.app", isDirectory: true) }

    private func v(_ s: String) -> VersionTuple { VersionTuple.parse(s)! }

    private func makeBundle(at url: URL, version: String) throws {
        let contents = url.appendingPathComponent("Contents")
        try FileManager.default.createDirectory(at: contents, withIntermediateDirectories: true)
        let plist: [String: Any] = ["CFBundleShortVersionString": version]
        let data = try PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0)
        try data.write(to: contents.appendingPathComponent("Info.plist"))
    }

    private func argPair(_ args: [String], _ flag: String) -> String? {
        guard let i = args.firstIndex(of: flag), i + 1 < args.count else { return nil }
        return args[i + 1]
    }
}
