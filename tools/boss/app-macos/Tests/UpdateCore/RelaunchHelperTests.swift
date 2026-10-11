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
        let markerText = try String(contentsOf: installer().rolledBackMarkerURL, encoding: .utf8)
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

    // MARK: - Other failure classifications

    func testEarlyExitIsRecordedAsProcessExited() throws {
        let status = try runHelper(newAppBehaviour: .exitsImmediately, watchdogSeconds: 5)
        XCTAssertEqual(status, 1)
        XCTAssertEqual(installedVersion(), "1.0.27")
        let marker = try String(contentsOf: installer().rolledBackMarkerURL, encoding: .utf8)
        XCTAssertTrue(marker.contains("reason=process-exited\n"), marker)
        let events = helperEvents().map(\.event)
        XCTAssertTrue(events.contains("new-process-started"), "\(events)")
        XCTAssertTrue(events.contains("process-exited"), "\(events)")
        XCTAssertFalse(events.contains("terminated-new-process"), "nothing left to terminate")
    }

    func testOpenFailureIsRecordedAsOpenFailed() throws {
        try installToolStubs(openSucceeds: false)
        let status = try runHelper(newAppBehaviour: .writesFlag, watchdogSeconds: 5)
        XCTAssertEqual(status, 1)
        XCTAssertEqual(installedVersion(), "1.0.27")
        let marker = try String(contentsOf: installer().rolledBackMarkerURL, encoding: .utf8)
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

    func testOldProcessThatNeverExitsGivesUpWithoutRollback() throws {
        // A stand-in for a Boss process that outlives the wait: must NOT trigger `open`
        // (LaunchServices would just activate it) and must NOT roll back the install.
        let sleeper = Process()
        sleeper.executableURL = URL(fileURLWithPath: "/bin/sh")
        sleeper.arguments = ["-c", "sleep 30"]
        sleeper.standardInput = FileHandle.nullDevice
        sleeper.standardOutput = FileHandle.nullDevice
        sleeper.standardError = FileHandle.nullDevice
        try sleeper.run()
        defer { sleeper.terminate(); sleeper.waitUntilExit() }

        let status = try runHelper(
            newAppBehaviour: .writesFlag, watchdogSeconds: 5, oldPID: sleeper.processIdentifier, oldExitTimeout: 1)
        XCTAssertEqual(status, 2, "distinct exit code: gave up waiting for the old process")
        XCTAssertEqual(installedVersion(), "1.0.28", "install left in place for the next manual launch")
        XCTAssertTrue(exists(backupURL), "backup retained")
        XCTAssertFalse(exists(installer().rolledBackMarkerURL), "no rollback")
        let events = helperEvents().map(\.event)
        XCTAssertEqual(events, ["helper-started", "old-process-still-running"], "\(events)")
        XCTAssertFalse(events.contains("open"), "open must never run while the old process is alive")
        XCTAssertNil(installer().pendingSwap(), "the helper never writes install-state itself")
    }
}

// MARK: - Fixtures

extension RelaunchHelperTests {

    fileprivate enum NewAppBehaviour {
        /// Stays alive without ever writing the flag (a launch that stalls).
        case hangsWithoutFlag
        /// Exits with status 3 before writing the flag (a crash / early exit).
        case exitsImmediately
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
    private func installToolStubs(openSucceeds: Bool) throws {
        let registry = binDir.appendingPathComponent("launched-pids").path
        let open = openSucceeds
            ? """
            #!/bin/sh
            exe="$1/Contents/MacOS/Boss"
            [ -x "$exe" ] || { echo "LSOpenURLsWithRole() failed with error -10810" >&2; exit 1; }
            "$exe" </dev/null >/dev/null 2>&1 &
            printf '%s %s\\n' "$!" "$exe" >> "\(registry)"
            exit 0

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
        oldExitTimeout: Int = 60
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
        proc.arguments = invocation.arguments + ["--old-exit-timeout", String(oldExitTimeout), "--launch-timeout", "3"]
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
