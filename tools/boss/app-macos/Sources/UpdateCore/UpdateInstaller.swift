import Foundation

// MARK: - Public install types

/// A verified, quarantine-stripped bundle discovered under `Updates/<version>/`,
/// ready for `UpdateInstaller` to swap into the live install location. Mirrors the
/// `internal` `StagedUpdate` that `UpdateDownloader` produces, but is `public` so the
/// app target can drive the swap without reaching into module-internal types.
public struct ReadyUpdate: Equatable, Sendable {
    public let version: VersionTuple
    public let tag: String
    /// `Updates/<version>/Boss.app`.
    public let bundleURL: URL
    /// The attempt id the downloader minted when it staged this version (from
    /// `manifest.json`), so the swap/relaunch records join the download records.
    public let attemptID: String?

    public init(version: VersionTuple, tag: String, bundleURL: URL, attemptID: String? = nil) {
        self.version = version
        self.tag = tag
        self.bundleURL = bundleURL
        self.attemptID = attemptID
    }
}

/// The concrete file moves a swap performs, plus whether to relaunch afterwards.
///
/// `applySwap` executes this plan as two atomic-when-same-volume renames:
/// `installBundleURL → backupURL` then `stagedBundleURL → installBundleURL`. The
/// `~/Applications` common case keeps `Updates/` and the install location on the same
/// APFS volume, so both renames are atomic; `FileManager.moveItem` degrades to
/// copy+remove on the rare cross-volume case without breaking the rollback ordering.
public struct SwapPlan: Equatable, Sendable {
    public let version: VersionTuple
    /// Source: `Updates/<version>/Boss.app`.
    public let stagedBundleURL: URL
    /// Destination: the live bundle (`Bundle.main.bundleURL`).
    public let installBundleURL: URL
    /// Where the current bundle is moved aside, retained until first-launch-OK.
    public let backupURL: URL
    /// `true` for the startup / "Install & Relaunch" path (helper relaunches us);
    /// `false` for the plain-quit path (swap in place, next manual launch runs it).
    public let relaunch: Bool
    /// The version being replaced (the one running when the plan was made).
    public let fromVersion: VersionTuple?
    /// The attempt id every log record for this swap/relaunch/handshake carries.
    public let attemptID: String

    public init(
        version: VersionTuple,
        stagedBundleURL: URL,
        installBundleURL: URL,
        backupURL: URL,
        relaunch: Bool,
        fromVersion: VersionTuple? = nil,
        attemptID: String = ""
    ) {
        self.version = version
        self.stagedBundleURL = stagedBundleURL
        self.installBundleURL = installBundleURL
        self.backupURL = backupURL
        self.relaunch = relaunch
        self.fromVersion = fromVersion
        self.attemptID = attemptID
    }
}

/// Why a swap can — or can't, or needn't — proceed. Drives graceful degradation:
/// the app surfaces `.notWritable` as "reveal the download in Finder" rather than
/// silently escalating privileges (design doc §4 privilege handling).
public enum SwapDecision: Equatable, Sendable {
    case swap(SwapPlan)
    /// The install location's parent directory isn't user-writable (e.g. a
    /// `/Applications` install without admin rights). No auto-swap in v1.
    case notWritable(installBundleURL: URL, stagedBundleURL: URL)
    /// This version previously failed its first launch and is blocklisted.
    case blocked(VersionTuple)
    /// Nothing newer than the running version is staged.
    case upToDate
}

/// Why the relaunch helper judged a first launch failed and rolled back. The raw
/// values are what `relaunch-helper.sh` writes into the rolled-back marker.
public enum RollbackReason: String, Codable, Equatable, Sendable {
    /// `open(1)` returned non-zero — LaunchServices refused to launch the bundle.
    case openFailed = "open-failed"
    /// `open` succeeded but no process running the installed bundle's executable
    /// appeared within the launch window.
    case launchNotStarted = "launch-not-started"
    /// The new process started but exited before writing the first-launch-OK flag.
    case processExited = "process-exited"
    /// The new process stayed alive but never wrote the flag inside the watchdog.
    case handshakeTimeout = "handshake-timeout"
    /// The pre-launch assessment of the swapped bundle failed (missing executable,
    /// unreadable Info.plist).
    case bundleInvalid = "bundle-invalid"
    /// A marker in the legacy single-line format (version only); the helper that
    /// wrote it predates reason recording.
    case unknown = "unknown"

    /// Human-readable phrase for the rollback notice.
    public var userDescription: String {
        switch self {
        case .openFailed: return "macOS refused to launch the new version"
        case .launchNotStarted: return "the new version never started"
        case .processExited: return "the new version exited before finishing its launch"
        case .handshakeTimeout: return "the new version did not finish launching in time"
        case .bundleInvalid: return "the new app bundle was invalid after installation"
        case .unknown: return "the new version failed to launch"
        }
    }
}

/// Durable record of one watchdog rollback, persisted in `install-state.json`
/// (`rollbacks`) by `reconcileAfterLaunch` and surfaced to the user as the
/// "Update to X failed: <reason>; rolled back to Y" notice.
public struct RollbackRecord: Codable, Equatable, Sendable {
    /// The version whose first launch failed (now blocklisted).
    public let version: String
    /// The version the helper restored and relaunched (`nil` if unknown).
    public let restoredVersion: String?
    public let reason: RollbackReason
    /// Reason-specific detail from the helper: elapsed seconds, watchdog length, exit
    /// status of `open`, whether the process was still alive, …
    public let detail: String
    public let attemptID: String?
    /// ISO-8601 time the rollback was reconciled (folded into state).
    public let recordedAt: String
    /// `true` once the user dismissed the notice for this rollback.
    public var acknowledged: Bool

    public init(
        version: String, restoredVersion: String?, reason: RollbackReason, detail: String,
        attemptID: String?, recordedAt: String, acknowledged: Bool = false
    ) {
        self.version = version
        self.restoredVersion = restoredVersion
        self.reason = reason
        self.detail = detail
        self.attemptID = attemptID
        self.recordedAt = recordedAt
        self.acknowledged = acknowledged
    }

    /// `"Update to 1.0.736 failed: the new version never started; rolled back to 1.0.734"`.
    public var noticeText: String {
        var text = "Update to \(version) failed: \(reason.userDescription)"
        if let restoredVersion { text += "; rolled back to \(restoredVersion)" }
        return text
    }
}

/// Outcome of the at-launch reconciliation pass (`reconcileAfterLaunch`).
public enum LaunchReconciliation: Equatable, Sendable {
    /// A pending swap completed: we are now running the new version. `.bak` dropped.
    case installed(VersionTuple)
    /// The helper rolled back a version that failed its first launch; it has been
    /// blocklisted so we never re-attempt it, and the reason is recorded.
    case rolledBack(RollbackRecord)
    case noChange
}

/// A detached command the app layer spawns to relaunch into a freshly-swapped
/// bundle and watchdog its first launch. Built here (tested); executed by the app
/// (`Process`), which is a lifecycle side-effect that doesn't belong in this leaf.
public struct HelperInvocation: Equatable, Sendable {
    public let executableURL: URL
    public let arguments: [String]

    public init(executableURL: URL, arguments: [String]) {
        self.executableURL = executableURL
        self.arguments = arguments
    }
}

/// Errors from the swap mechanics. `applySwap` rolls back before throwing, so a
/// thrown error always leaves the install location holding the *previous* bundle.
public enum InstallerError: Error, CustomStringConvertible, Equatable {
    case backupFailed(String)
    case swapFailed(String)
    case noBackup

    public var description: String {
        switch self {
        case .backupFailed(let m): return "could not move current bundle aside: \(m)"
        case .swapFailed(let m): return "could not move staged bundle into place: \(m)"
        case .noBackup: return "no backup bundle to roll back to"
        }
    }
}

// MARK: - Persisted install state

/// `Updates/install-state.json` — the durable record the installer owns. The
/// rolled-back marker is *not* here: it's a separate plain-text file written by the
/// shell helper (which can't merge JSON), then folded into `blocklist` and
/// `rollbacks` by the next `reconcileAfterLaunch`.
private struct InstallState: Codable, Equatable {
    var pending: PendingRecord?
    var blocklist: [String]
    /// Most recent last; capped at `maxRollbackHistory`.
    var rollbacks: [RollbackRecord]

    static let empty = InstallState(pending: nil, blocklist: [], rollbacks: [])
    static let maxRollbackHistory = 20

    init(pending: PendingRecord?, blocklist: [String], rollbacks: [RollbackRecord]) {
        self.pending = pending
        self.blocklist = blocklist
        self.rollbacks = rollbacks
    }

    /// Tolerates files written before `rollbacks` existed.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        pending = try c.decodeIfPresent(PendingRecord.self, forKey: .pending)
        blocklist = try c.decodeIfPresent([String].self, forKey: .blocklist) ?? []
        rollbacks = try c.decodeIfPresent([RollbackRecord].self, forKey: .rollbacks) ?? []
    }
}

private struct PendingRecord: Codable, Equatable {
    let version: String
    let installURL: String
    let backupURL: String
    let stagedURL: String
    let relaunch: Bool
    var fromVersion: String?
    var attemptID: String?
    var startedAt: String?
}

/// The rolled-back marker `relaunch-helper.sh` leaves for the next launch. Current
/// helpers write `key=value` lines (`version=`, `reason=`, `detail=`, `attempt=`,
/// `from=`, `restored=`, `at=`); the original format was a bare version string.
struct RolledBackMarker: Equatable {
    let version: VersionTuple
    let reason: RollbackReason
    let detail: String
    let attemptID: String?
    let restoredVersion: String?

    static func parse(_ text: String) -> RolledBackMarker? {
        let lines = text.split(separator: "\n").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
        guard let first = lines.first else { return nil }
        if !first.contains("=") {
            // Legacy single-line marker: just the version.
            guard let v = VersionTuple.parse(first) else { return nil }
            return RolledBackMarker(version: v, reason: .unknown, detail: "", attemptID: nil, restoredVersion: nil)
        }
        var fields: [String: String] = [:]
        for line in lines {
            guard let eq = line.firstIndex(of: "=") else { continue }
            fields[String(line[..<eq])] = String(line[line.index(after: eq)...])
        }
        guard let v = fields["version"].flatMap(VersionTuple.parse) else { return nil }
        let reason = fields["reason"].flatMap(RollbackReason.init(rawValue:)) ?? .unknown
        return RolledBackMarker(
            version: v,
            reason: reason,
            detail: fields["detail"] ?? "",
            attemptID: fields["attempt"].flatMap { $0.isEmpty ? nil : $0 },
            restoredVersion: fields["restored"].flatMap { $0.isEmpty ? nil : $0 }
        )
    }
}

// MARK: - UpdateInstaller

/// Swaps a staged Boss release into the live install location and manages the
/// safety net around it (design doc §4): `.bak` rollback, the first-launch-OK flag
/// the relaunch watchdog polls, the failed-version blocklist, and the
/// `/Applications`-not-writable degradation path.
///
/// This type is a pure, synchronous value: every method touches only the filesystem
/// under `updatesDirectory` (plus the install location) and persists state in
/// `install-state.json`. There is no network, no `Process`, and no app-lifecycle
/// coupling — those live in the app target's `UpdateLifecycle`, which calls into here.
///
/// **Where the rename runs.** The design doc sketches a shell helper that performs
/// the bundle rename after the app exits. Per this project's module-placement rule
/// (swap mechanics must be unit-testable in `UpdateCore`), the rename instead runs
/// in-process here via `applySwap` at a safe boundary — startup *before* the engine
/// launches, or quit *after* the user has confirmed termination. Renaming a running
/// `.app` directory is the documented-safe operation (open handles keep the old
/// inode; the whole-bundle atomic rename never touches the signature). The shell
/// `relaunch-helper.sh` is reduced to the one thing Swift can't do after the process
/// exits: wait for the old PID, relaunch the new bundle, watchdog its first launch,
/// and roll back the `.bak` on failure.
///
/// Every durable step is also appended to the ``UpdateLog`` when one is attached
/// (`live` attaches the shared `Updates/log/` log), tagged with the swap's attempt id.
public struct UpdateInstaller: Sendable {

    private let updatesDirectory: URL
    private let installBundleURL: URL
    /// Structured trail; `nil` only in tests that don't assert on it.
    public let log: UpdateLog?

    public init(updatesDirectory: URL, installBundleURL: URL, log: UpdateLog? = nil) {
        self.updatesDirectory = updatesDirectory
        self.installBundleURL = installBundleURL
        self.log = log
    }

    /// Live installer rooted at the app-support `Updates/` directory (the same
    /// directory `UpdateDownloader` stages into), writing the shared update log.
    public static func live(installBundleURL: URL) -> UpdateInstaller {
        let updates = UpdateDownloader.defaultUpdatesDirectory()
        return UpdateInstaller(
            updatesDirectory: updates,
            installBundleURL: installBundleURL,
            log: UpdateLog.live(updatesDirectory: updates)
        )
    }

    // MARK: Discovery

    /// The filesystem path of the directory that holds staged updates. Used for diagnostics.
    public var updatesDirectoryPath: String { updatesDirectory.path }

    /// The newest `.ready` staged version strictly greater than `currentVersion`
    /// whose bundle is present and which is not blocklisted; `nil` if none qualifies.
    public func newestReadyUpdate(currentVersion: VersionTuple) -> ReadyUpdate? {
        newestReadyUpdate(currentVersion: currentVersion, ignoringBlocklist: false)
    }

    /// Like `newestReadyUpdate` but also considers versions that are blocklisted. Used
    /// exclusively by the user-initiated "Install & Relaunch" path when a prior
    /// watchdog rollback blocked the version the user is trying to install again.
    /// The caller must call `unblockVersion` before proceeding with a swap.
    public func newestReadyUpdateIgnoringBlocklist(currentVersion: VersionTuple) -> ReadyUpdate? {
        newestReadyUpdate(currentVersion: currentVersion, ignoringBlocklist: true)
    }

    private func newestReadyUpdate(currentVersion: VersionTuple, ignoringBlocklist: Bool) -> ReadyUpdate? {
        let blocked = ignoringBlocklist ? [] : blockedVersions()
        guard let entries = try? FileManager.default.contentsOfDirectory(
            at: updatesDirectory,
            includingPropertiesForKeys: [.isDirectoryKey],
            options: [.skipsHiddenFiles]
        ) else { return nil }

        var best: ReadyUpdate?
        for entry in entries {
            let name = entry.lastPathComponent
            if name == "staging" || name == "backup" || name == "log" { continue }
            guard isDirectory(entry),
                  let version = VersionTuple.parse(name),
                  version > currentVersion,
                  !blocked.contains(version),
                  let manifest = readManifest(at: entry),
                  manifest.state == .ready else { continue }
            let bundleURL = entry.appendingPathComponent("Boss.app", isDirectory: true)
            guard isDirectory(bundleURL) else { continue }

            if best == nil || version > best!.version {
                best = ReadyUpdate(
                    version: version, tag: manifest.tag, bundleURL: bundleURL, attemptID: manifest.attemptID)
            }
        }
        return best
    }

    // MARK: Feasibility

    /// `true` when the install location's *parent* directory is writable, i.e. the
    /// bundle can be replaced without admin rights. This is the `~/Applications`
    /// common case; a `/Applications` install typically returns `false`.
    public func isInstallLocationWritable() -> Bool {
        let parent = installBundleURL.deletingLastPathComponent()
        return FileManager.default.isWritableFile(atPath: parent.path)
    }

    /// Default location for the set-aside previous bundle: `Updates/Boss.app.bak`.
    /// Sits on the same APFS volume as `~/Applications` in the supported case, so the
    /// `install → backup` rename stays atomic.
    public var backupURL: URL {
        updatesDirectory.appendingPathComponent("Boss.app.bak", isDirectory: true)
    }

    /// Decide whether `update` can be swapped in. Pure: performs no filesystem
    /// mutation. The caller (app layer) is responsible for mode/dev-build gating.
    ///
    /// The returned plan carries the attempt id from the staged manifest (so the swap
    /// joins the download's trail) or a fresh one when the manifest predates attempt
    /// ids.
    public func planSwap(
        for update: ReadyUpdate,
        currentVersion: VersionTuple,
        relaunch: Bool
    ) -> SwapDecision {
        guard update.version > currentVersion else { return .upToDate }
        if isBlocked(update.version) { return .blocked(update.version) }
        guard isInstallLocationWritable() else {
            return .notWritable(installBundleURL: installBundleURL, stagedBundleURL: update.bundleURL)
        }
        return .swap(SwapPlan(
            version: update.version,
            stagedBundleURL: update.bundleURL,
            installBundleURL: installBundleURL,
            backupURL: backupURL,
            relaunch: relaunch,
            fromVersion: currentVersion,
            attemptID: update.attemptID ?? UpdateAttemptID.make()
        ))
    }

    // MARK: Swap mechanics

    /// Execute `plan`: move the current bundle aside to `backupURL`, then move the
    /// staged bundle into the install location. Records a pending-swap entry first so
    /// the next launch can complete (or roll back) the transition.
    ///
    /// Rollback is built into the ordering: if the second move fails, the first is
    /// undone (backup restored over the install location) before throwing, so a
    /// thrown error always leaves the *previous* bundle live. The `.bak` is retained
    /// on success and dropped only once the new version reports a clean first launch.
    ///
    /// Each filesystem operation and its result is appended to the update log.
    public func applySwap(_ plan: SwapPlan) throws {
        let fm = FileManager.default
        try? fm.createDirectory(
            at: plan.backupURL.deletingLastPathComponent(), withIntermediateDirectories: true)
        recordPending(plan)
        logSwap(plan, "begin", detail: [
            "staged": plan.stagedBundleURL.path,
            "install": plan.installBundleURL.path,
            "backup": plan.backupURL.path,
            "relaunch": String(plan.relaunch),
        ])

        // 1. current → backup (skip if the install location is somehow absent).
        let hadInstall = fm.fileExists(atPath: plan.installBundleURL.path)
        if hadInstall {
            try? fm.removeItem(at: plan.backupURL)  // clear any stale backup
            do {
                try fm.moveItem(at: plan.installBundleURL, to: plan.backupURL)
                logSwap(plan, "moved-current-to-backup", detail: ["result": "ok"])
            } catch {
                logSwap(plan, "moved-current-to-backup", level: "error",
                        detail: ["result": "failed", "error": error.localizedDescription])
                clearPending()
                throw InstallerError.backupFailed(error.localizedDescription)
            }
        } else {
            logSwap(plan, "no-current-bundle", level: "warning",
                    detail: ["install": plan.installBundleURL.path])
        }

        // 2. staged → install.
        do {
            try? fm.removeItem(at: plan.installBundleURL)
            try fm.moveItem(at: plan.stagedBundleURL, to: plan.installBundleURL)
            logSwap(plan, "moved-staged-to-install", detail: ["result": "ok"])
        } catch {
            logSwap(plan, "moved-staged-to-install", level: "error",
                    detail: ["result": "failed", "error": error.localizedDescription])
            // Roll back: restore the previous bundle so we never end up with nothing.
            if hadInstall {
                try? fm.removeItem(at: plan.installBundleURL)
                let restored = (try? fm.moveItem(at: plan.backupURL, to: plan.installBundleURL)) != nil
                logSwap(plan, "restored-backup-after-failed-swap", level: "warning",
                        detail: ["result": restored ? "ok" : "failed"])
            }
            clearPending()
            throw InstallerError.swapFailed(error.localizedDescription)
        }
        logSwap(plan, "committed", detail: [
            "installedVersion": installedBundleVersion()?.description ?? "unreadable",
        ])
        // Success: the pending record + `.bak` survive until reconciliation.
    }

    /// Restore the backed-up bundle over the install location. Used for explicit
    /// recovery and exercised by tests; the production crash-watchdog rollback lives
    /// in `relaunch-helper.sh` because it must run after the process exits.
    public func rollback(_ plan: SwapPlan) throws {
        let fm = FileManager.default
        guard fm.fileExists(atPath: plan.backupURL.path) else { throw InstallerError.noBackup }
        try? fm.removeItem(at: plan.installBundleURL)
        try fm.moveItem(at: plan.backupURL, to: plan.installBundleURL)
        logSwap(plan, "rolled-back-in-process", level: "warning", detail: [:])
    }

    // MARK: First-launch-OK flag

    /// The flag file the relaunch watchdog polls for. The freshly-launched new
    /// version writes this on `applicationDidFinishLaunching`; the helper clears it
    /// before relaunching, so its appearance unambiguously means "this launch reached
    /// a healthy state".
    public func firstLaunchFlagURL(for version: VersionTuple) -> URL {
        updatesDirectory.appendingPathComponent("launch-ok-\(version.description).flag")
    }

    /// Record that `version` launched successfully, and prune flags from other
    /// versions so the directory doesn't accumulate stale markers.
    public func markLaunchSucceeded(version: VersionTuple) {
        try? FileManager.default.createDirectory(at: updatesDirectory, withIntermediateDirectories: true)
        pruneLaunchFlags(except: version)
        try? Data().write(to: firstLaunchFlagURL(for: version))
    }

    public func launchSucceeded(version: VersionTuple) -> Bool {
        FileManager.default.fileExists(atPath: firstLaunchFlagURL(for: version).path)
    }

    // MARK: Blocklist

    public func isBlocked(_ version: VersionTuple) -> Bool {
        loadState().blocklist.contains(version.description)
    }

    public func blockedVersions() -> [VersionTuple] {
        loadState().blocklist.compactMap(VersionTuple.parse)
    }

    public func blockVersion(_ version: VersionTuple) {
        blockVersion(version, attemptID: nil, reason: "manual")
    }

    private func blockVersion(_ version: VersionTuple, attemptID: String?, reason: String) {
        var state = loadState()
        if !state.blocklist.contains(version.description) {
            state.blocklist.append(version.description)
            saveState(state)
            log?.record(stage: "blocklist", event: "added", level: "warning", attempt: attemptID, from: log?.allRecords().last(where: { $0.attempt == attemptID })?.from.flatMap(VersionTuple.parse), to: version,
                        detail: ["reason": reason, "blocklist": state.blocklist.joined(separator: ",")])
        }
    }

    /// Remove `version` from the blocklist so a subsequent swap can proceed. Used by
    /// the user-initiated "Install & Relaunch" / "Retry Update" paths to allow retrying
    /// a version that previously failed its watchdog launch. The watchdog will
    /// re-blocklist the version if it fails again.
    public func unblockVersion(_ version: VersionTuple) {
        var state = loadState()
        let before = state.blocklist.count
        state.blocklist.removeAll { $0 == version.description }
        if state.blocklist.count != before {
            saveState(state)
            let context = log?.allRecords().last { $0.to == version.description }
            log?.record(stage: "blocklist", event: "removed", attempt: context?.attempt, from: context?.from.flatMap(VersionTuple.parse), to: version,
                        detail: ["reason": "user-retry", "blocklist": state.blocklist.joined(separator: ",")])
        }
    }

    // MARK: Rollback history

    /// The most recent watchdog rollback folded into state, if any.
    public func lastRollback() -> RollbackRecord? {
        loadState().rollbacks.last
    }

    /// The most recent rollback of `version`, if any.
    public func rollbackRecord(for version: VersionTuple) -> RollbackRecord? {
        loadState().rollbacks.last { $0.version == version.description }
    }

    /// Mark every recorded rollback as acknowledged (the user dismissed the notice).
    public func acknowledgeRollbacks() {
        var state = loadState()
        guard state.rollbacks.contains(where: { !$0.acknowledged }) else { return }
        for i in state.rollbacks.indices { state.rollbacks[i].acknowledged = true }
        saveState(state)
    }

    // MARK: Pending swap

    /// The plan recorded by the most recent `applySwap`, if it hasn't been
    /// reconciled yet. `relaunch` is preserved but informational here.
    public func pendingSwap() -> SwapPlan? {
        guard let p = loadState().pending, let version = VersionTuple.parse(p.version) else { return nil }
        return SwapPlan(
            version: version,
            stagedBundleURL: URL(fileURLWithPath: p.stagedURL),
            installBundleURL: URL(fileURLWithPath: p.installURL),
            backupURL: URL(fileURLWithPath: p.backupURL),
            relaunch: p.relaunch,
            fromVersion: p.fromVersion.flatMap(VersionTuple.parse),
            attemptID: p.attemptID ?? ""
        )
    }

    /// The `CFBundleShortVersionString` of the bundle currently at the install
    /// location, or `nil` if it can't be read. Reads the on-disk `Info.plist`
    /// directly (not `Bundle.main`) so it reflects a swap that `applySwap` has
    /// already committed even though the running process still hosts the old code.
    public func installedBundleVersion() -> VersionTuple? {
        let plistURL = installBundleURL.appendingPathComponent("Contents/Info.plist")
        guard let data = try? Data(contentsOf: plistURL),
              let plist = try? PropertyListSerialization.propertyList(from: data, format: nil),
              let dict = plist as? [String: Any],
              let short = dict["CFBundleShortVersionString"] as? String else { return nil }
        return VersionTuple.parse(short)
    }

    /// The pending swap when it has *already been applied* to disk but not yet
    /// reconciled — i.e. `applySwap` moved the new bundle into the install location
    /// (so the live bundle's version now matches the pending record) but we have not
    /// relaunched into it. `nil` when there is no pending record, it isn't newer than
    /// `currentVersion`, or the live bundle doesn't match it (swap not actually done).
    ///
    /// This is what makes a repeated user-initiated "Install & Relaunch" idempotent:
    /// the first press consumes the staged `Boss.app` (it is moved, not copied), so a
    /// second `newestReadyUpdate` finds nothing — but the install genuinely succeeded
    /// and is only awaiting the relaunch. Callers treat this as success, not a
    /// missing-staged-bundle failure.
    public func appliedPendingAwaitingRelaunch(currentVersion: VersionTuple) -> SwapPlan? {
        guard let pending = pendingSwap(),
              pending.version > currentVersion,
              installedBundleVersion() == pending.version else { return nil }
        return pending
    }

    // MARK: Reconciliation

    /// Run once at launch. Completes a pending swap or rolls back a failed one:
    ///
    /// 1. If the shell helper left a rolled-back marker, blocklist that version,
    ///    record the rollback reason, clear its staged directory, and report
    ///    `.rolledBack`.
    /// 2. Otherwise, if a pending swap targeted the version we're now running, the
    ///    swap succeeded — write the first-launch-OK flag, drop the `.bak`, clear the
    ///    pending record, report `.installed`.
    /// 3. Otherwise `.noChange`.
    ///
    /// Order matters: the rolled-back marker is checked first because after a
    /// rollback we're running the *old* version, so the `pending.version == running`
    /// test in step 2 would (correctly) not fire — but we still must record the
    /// blocklist entry the marker represents.
    public func reconcileAfterLaunch(currentVersion: VersionTuple) -> LaunchReconciliation {
        let fm = FileManager.default
        // The helper has no os_log of its own; mirror whatever it wrote since the last
        // launch into the app's unified-log category.
        log?.replayHelperRecords()
        // Snapshot the blocklist on every exit path, so the next launch can tell
        // whether something other than this code (an older app) grew it.
        defer { writeBlocklistSnapshot() }

        // A sidecar whose one-line marker is already gone was left by a rollback that
        // an older app (1.0.734 and earlier) reconciled: it consumed the marker and
        // blocklisted the version, but cannot read the reason. Fold it into the
        // rollback history only. It must not touch the pending swap, staged directory
        // or backup — those may now belong to a later attempt that is still being
        // watched by its own helper.
        let orphanedRollback = foldOrphanedRollbackDetail(currentVersion: currentVersion)

        if let marker = readRolledBackMarker() {
            let pending = loadState().pending
            let attemptID = marker.attemptID ?? pending?.attemptID
            let restored = marker.restoredVersion ?? pending?.fromVersion ?? currentVersion.description
            let record = RollbackRecord(
                version: marker.version.description,
                restoredVersion: restored,
                reason: marker.reason,
                detail: marker.detail,
                attemptID: attemptID,
                recordedAt: UpdateLog.timestamp(Date())
            )
            var state = loadState()
            state.rollbacks.append(record)
            if state.rollbacks.count > InstallState.maxRollbackHistory {
                state.rollbacks.removeFirst(state.rollbacks.count - InstallState.maxRollbackHistory)
            }
            state.pending = nil
            saveState(state)
            log?.record(
                stage: "reconcile", event: "rolled-back", level: "error", attempt: attemptID,
                from: VersionTuple.parse(restored), to: marker.version,
                detail: [
                    "reason": marker.reason.rawValue,
                    "detail": marker.detail,
                    "runningVersion": currentVersion.description,
                    "stagedDirRemoved": stagedDirectory(for: marker.version).path,
                ])
            blockVersion(marker.version, attemptID: attemptID, reason: marker.reason.rawValue)
            try? fm.removeItem(at: stagedDirectory(for: marker.version))
            try? fm.removeItem(at: backupURL)
            try? fm.removeItem(at: rolledBackMarkerURL)
            try? fm.removeItem(at: rolledBackDetailURL)
            return .rolledBack(record)
        }

        if let pending = pendingSwap(), pending.version == currentVersion {
            markLaunchSucceeded(version: currentVersion)
            let attempt = pending.attemptID.isEmpty ? nil : pending.attemptID
            log?.record(
                stage: "handshake", event: "flag-written", attempt: attempt,
                from: pending.fromVersion, to: currentVersion,
                detail: ["flag": firstLaunchFlagURL(for: currentVersion).path])
            try? fm.removeItem(at: pending.backupURL)
            clearPending()
            log?.record(
                stage: "reconcile", event: "installed", attempt: attempt,
                from: pending.fromVersion, to: currentVersion,
                detail: ["backupRemoved": pending.backupURL.path])
            return .installed(currentVersion)
        }

        if let record = detectHistoricalRollback(currentVersion: currentVersion) ?? orphanedRollback {
            return .rolledBack(record)
        }
        return .noChange
    }

    // MARK: Rollbacks performed by older code

    private var blocklistSnapshotURL: URL {
        updatesDirectory.appendingPathComponent("last-seen-blocklist.json")
    }

    private func writeBlocklistSnapshot() {
        if let data = try? JSONEncoder().encode(loadState().blocklist) {
            try? data.write(to: blocklistSnapshotURL, options: .atomic)
        }
    }

    /// Record a rolled-back sidecar whose one-line marker an older app already
    /// consumed (see `reconcileAfterLaunch`). History only: no blocklist change (the
    /// older app's reading of the marker is authoritative, and the user may since
    /// have retried), no pending/staged/backup cleanup.
    private func foldOrphanedRollbackDetail(currentVersion: VersionTuple) -> RollbackRecord? {
        let fm = FileManager.default
        guard !fm.fileExists(atPath: rolledBackMarkerURL.path),
              let text = try? String(contentsOf: rolledBackDetailURL, encoding: .utf8) else { return nil }
        try? fm.removeItem(at: rolledBackDetailURL)
        guard let marker = RolledBackMarker.parse(text) else { return nil }
        var state = loadState()
        if let attempt = marker.attemptID, state.rollbacks.contains(where: { $0.attemptID == attempt }) {
            return nil
        }
        let record = RollbackRecord(
            version: marker.version.description,
            restoredVersion: marker.restoredVersion ?? currentVersion.description,
            reason: marker.reason,
            detail: marker.detail,
            attemptID: marker.attemptID,
            recordedAt: UpdateLog.timestamp(Date())
        )
        state.rollbacks.append(record)
        state.rollbacks = Array(state.rollbacks.suffix(InstallState.maxRollbackHistory))
        saveState(state)
        log?.record(
            stage: "reconcile", event: "rollback-detected", level: "warning", attempt: marker.attemptID,
            from: marker.restoredVersion.flatMap(VersionTuple.parse), to: marker.version,
            detail: [
                "evidence": "rolled-back.detail sidecar left after an older app consumed the marker",
                "reason": marker.reason.rawValue,
                "detail": marker.detail,
                "runningVersion": currentVersion.description,
                "blocklisted": String(state.blocklist.contains(marker.version.description)),
            ])
        return record
    }

    /// Detect a rollback that code without this reconcile performed, so it still
    /// reaches the trail and the rollback notice (reason `.unknown`). Two signals:
    ///
    /// - **The blocklist grew** since the snapshot the previous launch of this code
    ///   wrote. Only reconcile code blocklists, and this code's own blocklisting
    ///   returns before reaching here, so growth means an older app consumed a
    ///   rolled-back marker. With no snapshot yet (the first launch of this code),
    ///   blocklisted versions with no rollback record at all count.
    /// - **A dangling attempt**: a committed swap (`swap.committed`) or a started
    ///   helper (`relaunch.helper-started`) for a version other than the one running,
    ///   while the running version is that attempt's `from` version — i.e. the
    ///   previous version is back — with no success record (`reconcile.installed`,
    ///   `handshake.flag-written`, `relaunch.committed`) and no rollback already
    ///   recorded for it. Swap attempts that never committed (`rolled-back-in-process`,
    ///   `skipped-*`) are not rollbacks and are ignored.
    private func detectHistoricalRollback(currentVersion: VersionTuple) -> RollbackRecord? {
        var state = loadState()
        let snapshot = (try? Data(contentsOf: blocklistSnapshotURL)).flatMap {
            try? JSONDecoder().decode([String].self, from: $0)
        }
        let grown = state.blocklist.filter { version in
            if let snapshot { return !snapshot.contains(version) }
            return !state.rollbacks.contains { $0.version == version }
        }

        let running = currentVersion.description
        let records = log?.allRecords() ?? []
        let resolved: Set<String> = Set(records.compactMap { record -> String? in
            guard let attempt = record.attempt else { return nil }
            switch (record.stage, record.event) {
            case ("reconcile", "installed"), ("handshake", "flag-written"), ("relaunch", "committed"),
                 ("reconcile", "rolled-back"), ("reconcile", "rollback-detected"):
                return attempt
            default:
                return nil
            }
        }).union(state.rollbacks.compactMap(\.attemptID))
        let dangling = records.last { record in
            guard let attempt = record.attempt, !resolved.contains(attempt),
                  let target = record.to, target != running, record.from == running else { return false }
            return (record.stage == "swap" && record.event == "committed")
                || (record.actor == "helper" && record.event == "helper-started")
        }

        guard let version = grown.last ?? dangling?.to else { return nil }
        var evidence: [String] = []
        if !grown.isEmpty {
            evidence.append(snapshot == nil
                ? "blocklisted with no rollback record: \(grown.joined(separator: ","))"
                : "blocklist grew since last startup: \(grown.joined(separator: ","))")
        }
        if let dangling, let attempt = dangling.attempt {
            evidence.append("attempt \(attempt) \(dangling.stage).\(dangling.event) for \(dangling.to ?? "?") with no successful launch")
        }
        let attempt = dangling?.to == version ? dangling?.attempt : nil
        let record = RollbackRecord(version: version, restoredVersion: running,
            reason: .unknown, detail: evidence.joined(separator: "; "), attemptID: attempt,
            recordedAt: UpdateLog.timestamp(Date()))
        state.rollbacks.append(record)
        state.rollbacks = Array(state.rollbacks.suffix(InstallState.maxRollbackHistory))
        saveState(state)
        log?.record(stage: "reconcile", event: "rollback-detected", level: "warning",
                    attempt: attempt, from: currentVersion, to: VersionTuple.parse(version),
                    detail: ["evidence": record.detail, "runningVersion": running])
        return record
    }

    // MARK: Helper invocation

    /// The path the shell helper writes the failed version into on a watchdog
    /// rollback; `reconcileAfterLaunch` reads and clears it on the next launch.
    public var rolledBackMarkerURL: URL {
        updatesDirectory.appendingPathComponent("rolled-back")
    }

    /// The helper's `rolled-back.detail` sidecar: reason, detail, attempt, from,
    /// restored version. The marker itself stays a bare version so apps that predate
    /// reasons (1.0.734 and earlier) still parse it and blocklist the failed version.
    public var rolledBackDetailURL: URL {
        rolledBackMarkerURL.appendingPathExtension("detail")
    }

    /// Build the detached `/bin/sh relaunch-helper.sh …` command for `plan`. The app
    /// layer spawns this *after* `applySwap` has already moved the new bundle into
    /// place, then exits; the helper waits for `bossPID`, relaunches, and watchdogs.
    /// The helper appends to the same update log (`--log`) under the plan's attempt id.
    public func helperInvocation(
        scriptURL: URL,
        plan: SwapPlan,
        bossPID: Int32,
        watchdogSeconds: Int
    ) -> HelperInvocation {
        let logURL = log?.fileURL
            ?? UpdateLog.directory(forUpdatesDirectory: updatesDirectory).appendingPathComponent(UpdateLog.fileName)
        return HelperInvocation(
            executableURL: URL(fileURLWithPath: "/bin/sh"),
            arguments: [
                scriptURL.path,
                "--pid", String(bossPID),
                "--install", plan.installBundleURL.path,
                "--backup", plan.backupURL.path,
                "--flag", firstLaunchFlagURL(for: plan.version).path,
                "--rolled-back-marker", rolledBackMarkerURL.path,
                "--version", plan.version.description,
                "--watchdog", String(watchdogSeconds),
                "--attempt", plan.attemptID,
                "--from", plan.fromVersion?.description ?? "",
                "--log", logURL.path,
            ]
        )
    }

    // MARK: - Private helpers

    private var stateURL: URL { updatesDirectory.appendingPathComponent("install-state.json") }

    private func stagedDirectory(for version: VersionTuple) -> URL {
        updatesDirectory.appendingPathComponent(version.description, isDirectory: true)
    }

    private func logSwap(_ plan: SwapPlan, _ event: String, level: String = "info", detail: [String: String]) {
        log?.record(
            stage: "swap", event: event, level: level,
            attempt: plan.attemptID.isEmpty ? nil : plan.attemptID,
            from: plan.fromVersion, to: plan.version, detail: detail)
    }

    private func loadState() -> InstallState {
        guard let data = try? Data(contentsOf: stateURL),
              let state = try? JSONDecoder().decode(InstallState.self, from: data) else {
            return .empty
        }
        return state
    }

    private func saveState(_ state: InstallState) {
        try? FileManager.default.createDirectory(at: updatesDirectory, withIntermediateDirectories: true)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        guard let data = try? encoder.encode(state) else { return }
        try? data.write(to: stateURL, options: .atomic)
    }

    // Internal (not private) so tests can stage a pending swap directly.
    func recordPending(_ plan: SwapPlan) {
        var state = loadState()
        state.pending = PendingRecord(
            version: plan.version.description,
            installURL: plan.installBundleURL.path,
            backupURL: plan.backupURL.path,
            stagedURL: plan.stagedBundleURL.path,
            relaunch: plan.relaunch,
            fromVersion: plan.fromVersion?.description,
            attemptID: plan.attemptID.isEmpty ? nil : plan.attemptID,
            startedAt: UpdateLog.timestamp(Date())
        )
        saveState(state)
    }

    private func clearPending() {
        var state = loadState()
        guard state.pending != nil else { return }
        state.pending = nil
        saveState(state)
    }

    /// The rolled-back marker the next reconcile will consume, if any (read-only).
    func pendingRollbackMarker() -> RolledBackMarker? {
        readRolledBackMarker()
    }

    /// The one-line marker, enriched from the sidecar when the sidecar describes the
    /// same version (a stale sidecar from an earlier rollback never relabels it).
    private func readRolledBackMarker() -> RolledBackMarker? {
        guard let text = try? String(contentsOf: rolledBackMarkerURL, encoding: .utf8),
              let bare = RolledBackMarker.parse(text) else { return nil }
        if let detailText = try? String(contentsOf: rolledBackDetailURL, encoding: .utf8),
           let detailed = RolledBackMarker.parse(detailText), detailed.version == bare.version {
            return detailed
        }
        return bare
    }

    private func pruneLaunchFlags(except keep: VersionTuple) {
        guard let entries = try? FileManager.default.contentsOfDirectory(
            at: updatesDirectory, includingPropertiesForKeys: nil, options: [.skipsHiddenFiles]
        ) else { return }
        let keepName = firstLaunchFlagURL(for: keep).lastPathComponent
        for entry in entries {
            let name = entry.lastPathComponent
            guard name.hasPrefix("launch-ok-"), name.hasSuffix(".flag"), name != keepName else { continue }
            try? FileManager.default.removeItem(at: entry)
        }
    }

    private func readManifest(at dir: URL) -> UpdateManifest? {
        let url = dir.appendingPathComponent("manifest.json")
        guard let data = try? Data(contentsOf: url) else { return nil }
        return try? JSONDecoder().decode(UpdateManifest.self, from: data)
    }

    private func isDirectory(_ url: URL) -> Bool {
        var isDir: ObjCBool = false
        return FileManager.default.fileExists(atPath: url.path, isDirectory: &isDir) && isDir.boolValue
    }
}
