import XCTest
@testable import UpdateCore

/// Runs the real `relaunch-helper.sh` against stub bundles, with `open`, `codesign`,
/// `xattr` and `plutil` replaced by PATH stubs, and asserts the whole attempt trail —
/// helper records, rolled-back marker, and the app-side reconciliation that folds the
/// marker into `install-state.json` — for a simulated failed first launch.
///
/// The "new app" is a shell script at `<install>/Contents/MacOS/Boss`; the stub `open`
/// launches it detached exactly as LaunchServices would, so the helper's process
/// discovery (`ps` by executable path), watchdog, termination and rollback run for real.
final class RelaunchHelperTests: XCTestCase {

    private var tempRoot: URL!
    private let attemptID = "upd-1760200000000-beef"

    override func setUpWithError() throws {
        tempRoot = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
            isDirectory: true
        ).appendingPathComponent("RelaunchHelperTests-\(UUID().uuidString)", isDirectory: true)
        for dir in [updatesDir, installParent, binDir] {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        }
        try installToolStubs(openSucceeds: true)
    }

    override func tearDownWithError() throws {
        if let tempRoot { try? FileManager.default.removeItem(at: tempRoot) }
    }

    // MARK: - The required scenario: failed launch → complete trail → recorded reason

    func testFailedLaunchProducesCompleteAttemptTrailEndingInRollbackReason() throws {
        // New version starts but never writes its first-launch flag (hangs in launch).
        let status = try runHelper(newAppBehaviour: .hangsWithoutFlag, watchdogSeconds: 2)
        XCTAssertEqual(status, 1, "helper exits 1 after a rollback")

        // Filesystem outcome: previous bundle restored, backup consumed, marker written.
        XCTAssertEqual(installedVersion(), "1.0.27", "the previous bundle must be back in place")
        XCTAssertFalse(exists(backupURL), "the .bak is consumed by the restore")
        let bareMarker = try String(contentsOf: installer().rolledBackMarkerURL, encoding: .utf8)
        XCTAssertEqual(VersionTuple.parse(bareMarker.trimmingCharacters(in: .whitespacesAndNewlines)), v("1.0.28"))
        let markerText = try String(contentsOf: installer().rolledBackMarkerURL.appendingPathExtension("detail"), encoding: .utf8)
        XCTAssertTrue(markerText.contains("version=1.0.28\n"), markerText)
        XCTAssertTrue(markerText.contains("reason=handshake-timeout\n"), markerText)
        XCTAssertTrue(markerText.contains("attempt=\(attemptID)\n"), markerText)
        XCTAssertTrue(markerText.contains("restored=1.0.27\n"), markerText)

        // Helper trail: every record tagged with the attempt and both versions, in order.
        let log = UpdateLog(directory: UpdateLog.directory(forUpdatesDirectory: updatesDir))
        let helperEvents = log.records(attempt: attemptID).filter { $0.actor == "helper" }
        XCTAssertTrue(helperEvents.allSatisfy { $0.from == "1.0.27" && $0.to == "1.0.28" },
                      "every helper record carries from/to versions")
        XCTAssertEqual(helperEvents.map { "\($0.stage).\($0.event)" }, [
            "relaunch.helper-started",
            "relaunch.old-process-exited",
            "relaunch.bundle-assessment",
            "handshake.watch-started",
            "relaunch.open",
            "relaunch.new-process-started",
            "handshake.timeout",
            "rollback.decision",
            "rollback.terminated-new-process",
            "rollback.removed-failed-bundle",
            "rollback.restored-backup",
            "rollback.marker-written",
            "rollback.reopened-previous",
            "rollback.completed",
        ], "trail details: \(helperEvents.map { "\($0.event) \($0.detail)" }.joined(separator: "\n"))")
        let byEvent = Dictionary(helperEvents.map { ($0.event, $0) }, uniquingKeysWith: { a, _ in a })
        XCTAssertEqual(byEvent["watch-started"]?.detail["flagVersion"], "1.0.28", "watcher names the version whose flag it polls")
        XCTAssertEqual(byEvent["watch-started"]?.detail["watchdogSeconds"], "2")
        XCTAssertEqual(byEvent["watch-started"]?.detail["flag"], installer().firstLaunchFlagURL(for: v("1.0.28")).path)
        XCTAssertEqual(byEvent["bundle-assessment"]?.detail["installedVersion"], "1.0.28")
        XCTAssertEqual(byEvent["bundle-assessment"]?.detail["executableState"], "executable")
        XCTAssertEqual(byEvent["bundle-assessment"]?.detail["codesign"], "ok")
        XCTAssertEqual(byEvent["bundle-assessment"]?.detail["quarantine"], "absent")
        XCTAssertEqual(byEvent["open"]?.detail["exit"], "0")
        XCTAssertNotNil(Int32(byEvent["new-process-started"]?.detail["pid"] ?? ""), "new process pid recorded")
        XCTAssertEqual(byEvent["new-process-started"]?.detail["version"], "1.0.28")
        XCTAssertEqual(byEvent["timeout"]?.detail["watchdogSeconds"], "2")
        XCTAssertEqual(byEvent["timeout"]?.detail["processAlive"], "yes")
        XCTAssertEqual(byEvent["decision"]?.detail["reason"], "handshake-timeout")
        XCTAssertEqual(byEvent["decision"]?.level, "error")
        XCTAssertEqual(byEvent["restored-backup"]?.detail["result"], "ok")
        XCTAssertEqual(byEvent["restored-backup"]?.detail["restoredVersion"], "1.0.27")

        // Next launch of the restored version folds the marker into install-state.
        let reconciler = UpdateInstaller(updatesDirectory: updatesDir, installBundleURL: installBundle, log: log)
        guard case .rolledBack(let record) = reconciler.reconcileAfterLaunch(currentVersion: v("1.0.27")) else {
            return XCTFail("reconcile must report the rollback")
        }
        XCTAssertEqual(record.version, "1.0.28")
        XCTAssertEqual(record.restoredVersion, "1.0.27")
        XCTAssertEqual(record.reason, .handshakeTimeout)
        XCTAssertEqual(record.attemptID, attemptID)
        XCTAssertTrue(record.detail.contains("within 2s"), record.detail)
        XCTAssertFalse(record.acknowledged)
        XCTAssertEqual(record.noticeText,
                       "Update to 1.0.28 failed: the new version did not finish launching in time; rolled back to 1.0.27")
        XCTAssertEqual(reconciler.lastRollback(), record, "reason persisted in install-state.json")
        XCTAssertTrue(reconciler.isBlocked(v("1.0.28")))
        XCTAssertFalse(exists(reconciler.rolledBackMarkerURL), "marker consumed")

        let tail = log.records(attempt: attemptID).suffix(2).map { "\($0.stage).\($0.event)" }
        XCTAssertEqual(tail, ["reconcile.rolled-back", "blocklist.added"],
                       "the attempt's trail ends with the recorded rollback reason and the blocklist change")
        XCTAssertEqual(log.records(attempt: attemptID).last?.detail["reason"], "handshake-timeout")
    }

    /// Boss 1.0.734 arms the swapped-in bundle's helper with only
    /// --pid/--install/--backup/--flag/--rolled-back-marker/--version/--watchdog.
    func testLegacyInvocationWritesTrailAndCompatibleMarker() throws {
        XCTAssertEqual(try runHelper(newAppBehaviour: .hangsWithoutFlag, watchdogSeconds: 1, legacy: true), 1)
        let log = UpdateLog(directory: UpdateLog.directory(forUpdatesDirectory: updatesDir))
        let records = log.allRecords()
        XCTAssertEqual(records.first?.event, "helper-started")
        XCTAssertEqual(records.first?.detail["derivedArguments"], "log attempt from ")
        XCTAssertEqual(records.last.map { "\($0.stage).\($0.event)" }, "rollback.completed")
        for event in ["old-process-exited", "bundle-assessment", "open", "new-process-started", "timeout", "decision"] {
            XCTAssertTrue(records.contains { $0.event == event }, "missing \(event) in \(records.map(\.event))")
        }
        let attempt = try XCTUnwrap(records.first?.attempt)
        XCTAssertNotNil(attempt.range(of: #"^upd-[0-9]+-[0-9a-f]{4}$"#, options: .regularExpression), attempt)
        XCTAssertTrue(records.allSatisfy { $0.attempt == attempt && $0.from == "1.0.27" && $0.to == "1.0.28" })

        // 1.0.734's reader: `VersionTuple.parse(text.trimmingCharacters(in: .whitespacesAndNewlines))`.
        let marker = try String(contentsOf: installer().rolledBackMarkerURL, encoding: .utf8)
        XCTAssertEqual(VersionTuple.parse(marker.trimmingCharacters(in: .whitespacesAndNewlines)), v("1.0.28"),
                       "the failed version must still be blocklisted by an app that predates reasons")

        // Simulate 1.0.734 reconciling it: blocklist the version and delete only the
        // marker. The next launch of this code recovers the reason from the sidecar,
        // without touching install state that may belong to a later attempt.
        try Data("{\"blocklist\":[\"1.0.28\"],\"rollbacks\":[]}".utf8)
            .write(to: updatesDir.appendingPathComponent("install-state.json"))
        try FileManager.default.removeItem(at: installer().rolledBackMarkerURL)
        let reconciler = UpdateInstaller(updatesDirectory: updatesDir, installBundleURL: installBundle, log: log)
        guard case .rolledBack(let record) = reconciler.reconcileAfterLaunch(currentVersion: v("1.0.27")) else {
            return XCTFail("the sidecar left behind must still surface the rollback")
        }
        XCTAssertEqual(record.reason, .handshakeTimeout)
        XCTAssertEqual(record.attemptID, attempt)
        XCTAssertFalse(exists(reconciler.rolledBackDetailURL), "sidecar consumed")
        XCTAssertEqual(reconciler.reconcileAfterLaunch(currentVersion: v("1.0.27")), .noChange, "surfaced once")
    }

    func testEveryHelperRecordAlsoGoesToTheUnifiedLog() throws {
        XCTAssertEqual(try runHelper(newAppBehaviour: .hangsWithoutFlag, watchdogSeconds: 1), 1)
        let durable = helperEvents()
        let unified = try String(contentsOf: binDir.appendingPathComponent("logger-lines"), encoding: .utf8)
            .split(separator: "\n").map(String.init)
        XCTAssertEqual(unified.count, durable.count, "one logger line per durable record")
        XCTAssertTrue(unified.allSatisfy { $0.contains("-t boss-relaunch-helper") }, unified.joined(separator: "\n"))
        XCTAssertTrue(unified.contains { $0.hasPrefix("-p user.err ") && $0.contains("\"event\":\"completed\"") })
        XCTAssertTrue(unified.contains { $0.hasPrefix("-p user.notice ") && $0.contains("\"event\":\"helper-started\"") })
    }

    func testPreexistingProcessIsNeverAdoptedOrKilled() throws {
        // A process already running the installed executable before `open` (say, a
        // copy the user opened by hand) must not be mistaken for the new launch.
        let bystander = Process()
        bystander.executableURL = URL(fileURLWithPath: "/bin/sh")
        bystander.arguments = ["-c", "sleep 30"]
        bystander.standardInput = FileHandle.nullDevice
        bystander.standardOutput = FileHandle.nullDevice
        bystander.standardError = FileHandle.nullDevice
        try bystander.run()
        defer { if bystander.isRunning { bystander.terminate() }; bystander.waitUntilExit() }
        let exe = installBundle.appendingPathComponent("Contents/MacOS/Boss").path
        try "\(bystander.processIdentifier) \(exe)\n".write(
            to: binDir.appendingPathComponent("launched-pids"), atomically: true, encoding: .utf8)

        XCTAssertEqual(try runHelper(newAppBehaviour: .hangsWithoutFlag, watchdogSeconds: 1), 1)
        let events = helperEvents()
        XCTAssertEqual(events.first { $0.event == "preexisting-process" }?.detail["pids"],
                       String(bystander.processIdentifier))
        let adopted = events.first { $0.event == "new-process-started" }?.detail["pid"]
        XCTAssertNotNil(adopted)
        XCTAssertNotEqual(adopted, String(bystander.processIdentifier), "the bystander is not the new launch")
        XCTAssertTrue(bystander.isRunning, "rollback terminates only the process it launched")
    }

    // MARK: - Other failure classifications

    func testEarlyExitIsRecordedAsProcessExited() throws {
        let status = try runHelper(newAppBehaviour: .exitsImmediately, watchdogSeconds: 5)
        XCTAssertEqual(status, 1)
        XCTAssertEqual(installedVersion(), "1.0.27")
        let marker = try String(contentsOf: installer().rolledBackMarkerURL.appendingPathExtension("detail"), encoding: .utf8)
        XCTAssertTrue(marker.contains("reason=process-exited\n"), marker)
        XCTAssertTrue(marker.contains("exitCode=3"), marker)
        XCTAssertEqual(helperEvents().first { $0.event == "process-exited" }?.detail["termination"], "exitCode=3")
        let events = helperEvents().map(\.event)
        XCTAssertTrue(events.contains("new-process-started"), "\(events)")
        XCTAssertTrue(events.contains("process-exited"), "\(events)")
        XCTAssertFalse(events.contains("terminated-new-process"), "nothing left to terminate")
    }

    func testSignalExitIsRecorded() throws {
        XCTAssertEqual(try runHelper(newAppBehaviour: .signalExit, watchdogSeconds: 5), 1)
        let marker = try String(contentsOf: installer().rolledBackMarkerURL.appendingPathExtension("detail"), encoding: .utf8)
        XCTAssertTrue(marker.contains("signal=15"), marker)
        XCTAssertEqual(helperEvents().first { $0.event == "process-exited" }?.detail["termination"], "signal=15")
    }

    func testBlockedOpenDoesNotSuspendHandshakeDeadline() throws {
        try installToolStubs(openSucceeds: true, blocks: true)
        let start = Date()
        XCTAssertEqual(try runHelper(newAppBehaviour: .hangsWithoutFlag, watchdogSeconds: 1), 1)
        XCTAssertLessThan(Date().timeIntervalSince(start), 60, "a hung open (120 s) must not hold the deadlines")
        XCTAssertTrue(helperEvents().contains { $0.event == "timeout" })
    }

    func testBlockedOpenWithoutProcessHitsLaunchDeadline() throws {
        try writeExecutable(binDir.appendingPathComponent("open"), "#!/bin/sh\nsleep 120\n")
        let start = Date()
        XCTAssertEqual(try runHelper(newAppBehaviour: .hangsWithoutFlag, watchdogSeconds: 1, launchTimeout: 3), 1)
        XCTAssertLessThan(Date().timeIntervalSince(start), 60, "a hung open (120 s) must not hold the deadlines")
        XCTAssertTrue(helperEvents().contains { $0.event == "decision" && $0.detail["reason"] == "launch-not-started" })
    }

    func testOpenFailureIsRecordedAsOpenFailed() throws {
        try installToolStubs(openSucceeds: false)
        let status = try runHelper(newAppBehaviour: .writesFlag, watchdogSeconds: 5)
        XCTAssertEqual(status, 1)
        XCTAssertEqual(installedVersion(), "1.0.27")
        let marker = try String(contentsOf: installer().rolledBackMarkerURL.appendingPathExtension("detail"), encoding: .utf8)
        XCTAssertTrue(marker.contains("reason=open-failed\n"), marker)
        XCTAssertTrue(marker.contains("-10810"), "open's stderr is preserved in the detail: \(marker)")
        let open = helperEvents().first { $0.event == "open" }
        XCTAssertEqual(open?.detail["exit"], "1")
    }

    func testHealthyLaunchCommitsAndDropsBackup() throws {
        let status = try runHelper(newAppBehaviour: .writesFlag, watchdogSeconds: 5)
        XCTAssertEqual(status, 0, "helper exits 0 when the new version reports a healthy launch")
        XCTAssertEqual(installedVersion(), "1.0.28", "new bundle stays in place")
        XCTAssertFalse(exists(backupURL), "backup dropped once the launch is confirmed")
        XCTAssertFalse(exists(installer().rolledBackMarkerURL), "no rollback marker on success")
        let events = helperEvents().map(\.event)
        XCTAssertEqual(Array(events.suffix(3)), ["flag-observed", "backup-removed", "committed"], "\(events)")
        XCTAssertNotNil(helperEvents().first { $0.event == "flag-observed" }?.detail["secondsAfterProcessStart"])
    }

    func testSlowOldProcessStillGetsWatchedLaunch() throws {
        // A stand-in for a Boss process that outlives the wait: must NOT trigger `open`
        // (LaunchServices would just activate it) and must NOT roll back the install.
        let sleeper = Process()
        sleeper.executableURL = URL(fileURLWithPath: "/bin/sh")
        sleeper.arguments = ["-c", "sleep 2"]
        sleeper.standardInput = FileHandle.nullDevice
        sleeper.standardOutput = FileHandle.nullDevice
        sleeper.standardError = FileHandle.nullDevice
        try sleeper.run()
        defer { if sleeper.isRunning { sleeper.terminate() }; sleeper.waitUntilExit() }

        let status = try runHelper(
            newAppBehaviour: .writesFlag, watchdogSeconds: 5, oldPID: sleeper.processIdentifier, oldExitTimeout: 1)
        XCTAssertEqual(status, 0)
        XCTAssertEqual(installedVersion(), "1.0.28")
        XCTAssertFalse(exists(backupURL))
        XCTAssertFalse(exists(installer().rolledBackMarkerURL))
        let events = helperEvents().map(\.event)
        XCTAssertTrue(events.contains("old-process-still-running"))
        XCTAssertTrue(events.contains("old-process-exited"))
        XCTAssertTrue(events.contains("flag-observed"))
        XCTAssertEqual(events.last, "committed", "the late launch is still watched to completion")
        let exited = try XCTUnwrap(events.firstIndex(of: "old-process-exited"))
        XCTAssertLessThan(try XCTUnwrap(events.firstIndex(of: "old-process-still-running")), exited)
        XCTAssertLessThan(exited, try XCTUnwrap(events.firstIndex(of: "open")),
                          "open never runs while the old process is alive")
    }
}

// MARK: - Fixtures

extension RelaunchHelperTests {

    fileprivate enum NewAppBehaviour {
        /// Stays alive without ever writing the flag (a launch that stalls).
        case hangsWithoutFlag
        /// Exits with status 3 before writing the flag (a crash / early exit).
        case exitsImmediately
        case signalExit
        /// Writes the first-launch flag shortly after starting, then idles.
        case writesFlag
    }

    private var updatesDir: URL { tempRoot.appendingPathComponent("Updates", isDirectory: true) }
    private var installParent: URL { tempRoot.appendingPathComponent("Applications", isDirectory: true) }
    private var installBundle: URL { installParent.appendingPathComponent("Boss.app", isDirectory: true) }
    private var backupURL: URL { installer().backupURL }
    private var binDir: URL { tempRoot.appendingPathComponent("bin", isDirectory: true) }

    private func v(_ s: String) -> VersionTuple { VersionTuple.parse(s)! }
    private func exists(_ url: URL) -> Bool { FileManager.default.fileExists(atPath: url.path) }

    private func installer() -> UpdateInstaller {
        UpdateInstaller(updatesDirectory: updatesDir, installBundleURL: installBundle)
    }

    private func helperEvents() -> [UpdateLogRecord] {
        UpdateLog(directory: UpdateLog.directory(forUpdatesDirectory: updatesDir))
            .records(attempt: attemptID).filter { $0.actor == "helper" }
    }

    /// The helper script: from Bazel runfiles when running under `bazel test`, else
    /// relative to this source file (SwiftPM / Xcode).
    private func scriptURL() throws -> URL {
        let env = ProcessInfo.processInfo.environment
        let relative = "tools/boss/app-macos/Sources/Resources/RelaunchHelper/relaunch-helper.sh"
        if let srcdir = env["TEST_SRCDIR"], let workspace = env["TEST_WORKSPACE"] {
            let url = URL(fileURLWithPath: srcdir).appendingPathComponent(workspace).appendingPathComponent(relative)
            if exists(url) { return url }
        }
        let fromSource = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Sources/Resources/RelaunchHelper/relaunch-helper.sh")
        if exists(fromSource) { return fromSource }
        throw XCTSkip("relaunch-helper.sh not found in runfiles (\(env["TEST_SRCDIR"] ?? "-")) or source tree")
    }

    /// `open` / `ps` / `codesign` / `xattr` / `plutil` stand-ins on PATH.
    ///
    /// The `open` stub starts `<bundle>/Contents/MacOS/Boss` detached, like
    /// LaunchServices, and appends `<pid> <executable>` to a registry file; the `ps`
    /// stub answers `-axww -o pid=,command=` and `-p PID -o command=` from that
    /// registry for pids that are still alive. The real `ps` cannot run inside the
    /// Bazel test sandbox (process-exec of `/bin/ps` is denied), so the process-table
    /// oracle is simulated here while the helper's control flow, timing, termination,
    /// rollback and logging run for real. (The real `ps` path is covered by running the
    /// same scenarios outside Bazel; see tools/boss/docs/update-attempt-log.md.)
    private func installToolStubs(openSucceeds: Bool, blocks: Bool = false) throws {
        let registry = binDir.appendingPathComponent("launched-pids").path
        let open = openSucceeds
            ? """
            #!/bin/sh
            exe="$1/Contents/MacOS/Boss"
            [ -x "$exe" ] || { echo "LSOpenURLsWithRole() failed with error -10810" >&2; exit 1; }
            "$exe" </dev/null >/dev/null 2>&1 &
            printf '%s %s\\n' "$!" "$exe" >> "\(registry)"
            \(blocks ? "sleep 120" : "exit 0")

            """
            : """
            #!/bin/sh
            echo "LSOpenURLsWithRole() failed with error -10810 for the file $1." >&2
            exit 1

            """
        try writeExecutable(binDir.appendingPathComponent("open"), open)
        let ps = """
        #!/bin/sh
        reg="\(registry)"
        [ -f "$reg" ] || exit 0
        if [ "$1" = "-p" ]; then
          while read -r pid exe; do
            [ "$pid" = "$2" ] && kill -0 "$pid" 2>/dev/null && printf '/bin/sh %s\\n' "$exe"
          done < "$reg"
          exit 0
        fi
        while read -r pid exe; do
          kill -0 "$pid" 2>/dev/null && printf '%s /bin/sh %s\\n' "$pid" "$exe"
        done < "$reg"
        exit 0

        """
        try writeExecutable(binDir.appendingPathComponent("ps"), ps)
        try writeExecutable(binDir.appendingPathComponent("codesign"), "#!/bin/sh\nexit 0\n")
        try writeExecutable(binDir.appendingPathComponent("xattr"), "#!/bin/sh\nexit 1\n")
        try writeExecutable(binDir.appendingPathComponent("plutil"), "#!/bin/sh\nexit 1\n")
        // The unified-log sink: record each invocation's arguments, one per line.
        let loggerLines = binDir.appendingPathComponent("logger-lines").path
        try writeExecutable(binDir.appendingPathComponent("logger"),
                            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"\(loggerLines)\"\n")
    }

    private func writeExecutable(_ url: URL, _ contents: String) throws {
        try contents.write(to: url, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: url.path)
    }

    /// A stub `.app`: XML Info.plist with the version, and a shell-script executable.
    private func plantBundle(at url: URL, version: String, executable: String) throws {
        let macos = url.appendingPathComponent("Contents/MacOS", isDirectory: true)
        try FileManager.default.createDirectory(at: macos, withIntermediateDirectories: true)
        let plist = """
        <?xml version="1.0" encoding="UTF-8"?>
        <plist version="1.0"><dict>
        <key>CFBundleExecutable</key>
        <string>Boss</string>
        <key>CFBundleShortVersionString</key>
        <string>\(version)</string>
        </dict></plist>

        """
        try plist.write(to: url.appendingPathComponent("Contents/Info.plist"), atomically: true, encoding: .utf8)
        try writeExecutable(macos.appendingPathComponent("Boss"), executable)
    }

    private func installedVersion() -> String? {
        installer().installedBundleVersion()?.description
    }

    /// Plant 1.0.27 as the backup and 1.0.28 as the (already swapped-in) install, then
    /// run the helper exactly as `UpdateInstaller.helperInvocation` would spawn it, with
    /// PATH stubs in front. Returns the helper's exit status.
    private func runHelper(
        newAppBehaviour: NewAppBehaviour,
        watchdogSeconds: Int,
        oldPID: Int32? = nil,
        oldExitTimeout: Int = 60,
        // Generous by default: a loaded CI host can take seconds to fork the stub
        // `open` and its child, and a launch deadline that fires first turns every
        // scenario into `launch-not-started`. Tests of that deadline pass a short one.
        launchTimeout: Int = 15,
        legacy: Bool = false
    ) throws -> Int32 {
        let flag = installer().firstLaunchFlagURL(for: v("1.0.28"))
        // Real binaries keep their argv[0] path for life; the stubs stay a `sh` running
        // the script (no `exec`) so `ps` keeps showing the bundle executable path.
        let newApp: String
        switch newAppBehaviour {
        case .hangsWithoutFlag:
            newApp = "#!/bin/sh\ntrap 'kill $! 2>/dev/null; exit 0' TERM\nsleep 20 &\nwait\n"
        case .exitsImmediately:
            // Lives just long enough to be observed starting (a crash during launch);
            // a process gone before the helper's first poll is indistinguishable from
            // one that never started and is classified `launch-not-started`.
            newApp = "#!/bin/sh\nsleep 0.5\nexit 3\n"
        case .signalExit:
            newApp = "#!/bin/sh\nsleep 0.5\nkill -TERM $$\n"
        case .writesFlag:
            newApp = "#!/bin/sh\ntrap 'kill $! 2>/dev/null; exit 0' TERM\nsleep 0.2\ntouch \"\(flag.path)\"\nsleep 3 &\nwait\n"
        }
        try plantBundle(at: backupURL, version: "1.0.27", executable: "#!/bin/sh\nexit 0\n")
        try plantBundle(at: installBundle, version: "1.0.28", executable: newApp)

        // The "old Boss pid": a process that has already exited unless a live one is given.
        let pid: Int32
        if let oldPID {
            pid = oldPID
        } else {
            let gone = Process()
            gone.executableURL = URL(fileURLWithPath: "/bin/sh")
            gone.arguments = ["-c", "exit 0"]
            try gone.run()
            gone.waitUntilExit()
            pid = gone.processIdentifier
        }

        let plan = SwapPlan(
            version: v("1.0.28"),
            stagedBundleURL: updatesDir.appendingPathComponent("1.0.28/Boss.app"),
            installBundleURL: installBundle,
            backupURL: backupURL,
            relaunch: true,
            fromVersion: v("1.0.27"),
            attemptID: attemptID)
        let log = UpdateLog(directory: UpdateLog.directory(forUpdatesDirectory: updatesDir))
        let withLog = UpdateInstaller(updatesDirectory: updatesDir, installBundleURL: installBundle, log: log)
        let invocation = withLog.helperInvocation(
            scriptURL: try scriptURL(), plan: plan, bossPID: pid, watchdogSeconds: watchdogSeconds)

        let proc = Process()
        proc.executableURL = invocation.executableURL
        var arguments = invocation.arguments
        if legacy {
            for flag in ["--log", "--attempt", "--from"] {
                if let index = arguments.firstIndex(of: flag) { arguments.removeSubrange(index...index + 1) }
            }
        }
        proc.arguments = arguments + ["--old-exit-timeout", String(oldExitTimeout), "--launch-timeout", String(launchTimeout)]
        var env = ProcessInfo.processInfo.environment
        env["PATH"] = binDir.path + ":" + (env["PATH"] ?? "/usr/bin:/bin")
        proc.environment = env
        proc.standardInput = FileHandle.nullDevice
        proc.standardOutput = FileHandle.nullDevice
        proc.standardError = FileHandle.nullDevice
        try proc.run()
        proc.waitUntilExit()
        return proc.terminationStatus
    }
}
