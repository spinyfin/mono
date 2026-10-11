import Foundation
import os.log

private let modelLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "updater")

// MARK: - ManualUpdateFeedback

/// Transient feedback state surfaced only by a user-initiated "Check for Updates" action.
/// Background polling never sets this — only ``UpdateModel/presentUpdateSheet()`` does.
public enum ManualUpdateFeedback: Equatable, Sendable {
    case checking
    case upToDate
    case networkError(String)
    case rateLimited(Date)
}

// MARK: - UpdateMode

/// The three update-operation modes, ordered by escalating automation.
/// Stored in UserDefaults as a raw string so the value survives app upgrades.
public enum UpdateMode: String, CaseIterable, Sendable {
    /// No automatic polling; the user triggers checks manually via the menu item.
    case manual = "manual"
    /// Polls every 6 hours and shows a chrome badge when an update is available. Default.
    case notify = "notify"
    /// Same as notify, plus downloads and installs updates automatically at a safe boundary.
    case automatic = "automatic"
}

// MARK: - UpdateDownloadState

/// Lifecycle of the in-app download/stage step, surfaced to the UI so the Settings
/// pane, badge popover, and result sheet can show progress and the
/// "downloaded — will install on quit/relaunch" state.
///
/// This is distinct from ``UpdateCheckResult`` (which only reports *availability*).
/// A check finding an update sets `lastCheckResult = .available`; staging the bundle
/// for that update drives this enum from `.downloading` to `.readyToInstall`.
public enum UpdateDownloadState: Equatable, Sendable {
    /// No download in flight and nothing staged this session.
    case idle
    /// Actively downloading/verifying `version`. `progress` is best-effort and
    /// reflects the download step only (verification that follows it reports no
    /// further progress).
    case downloading(version: VersionTuple, progress: DownloadProgress)
    /// `version` is downloaded, verified, and staged under `Updates/<version>/`; the
    /// next safe boundary (quit/startup in automatic mode, or an explicit
    /// "Install & Relaunch") swaps it in.
    case readyToInstall(version: VersionTuple)
    /// The download or one of its integrity checks failed.
    case failed(version: VersionTuple, reason: String)
    /// The in-place install step failed after staging succeeded (e.g. bundle not writable,
    /// swap failed). The staged bundle remains on disk. This is a terminal state — the
    /// dialog shows an error and does not open a browser.
    case installFailed(version: VersionTuple, reason: String)
    /// The bundle swap was applied — the new `version` is live on disk — but the app
    /// has not yet relaunched into it, so the running process is still the old version.
    /// The update completes on the next quit. `willRelaunch` is `true` when a relaunch
    /// helper will be armed to reopen Boss automatically on quit (the common case), and
    /// `false` when the helper is unavailable and the user must reopen Boss manually.
    /// Reached when the user pressed "Install & Relaunch" but the quit was vetoed (e.g.
    /// agents still working), or when the helper could not be armed. NOT a failure.
    case installedPendingRelaunch(version: VersionTuple, willRelaunch: Bool)
}

// MARK: - UpdateStager

/// Seam over ``UpdateDownloader`` so ``UpdateModel`` can drive the download/stage
/// step without a hard dependency on the network/filesystem at test time.
///
/// `.live` builds a real `UpdateDownloader` rooted at the default Updates directory
/// and resolves the running bundle's signing Team ID lazily — the downloader rejects
/// a staged bundle whose Team ID differs from the running one, so a swap can never
/// move us to a differently-signed bundle (equal `nil`s — today's ad-hoc-signed
/// reality — match). `.noop` never stages (placeholder/preview/non-bundle models);
/// tests inject their own closure.
struct UpdateStager: Sendable {
    /// Download, verify, and stage `update`, reporting download progress. Returns
    /// the staged version on success (freshly staged or already present), `nil` if
    /// any step failed.
    let stage: @Sendable (
        _ update: AvailableUpdate,
        _ onProgress: @Sendable @escaping (DownloadProgress) -> Void
    ) async -> VersionTuple?

    /// Never stages. Used by placeholder/preview models and non-bundle launches.
    static let noop = UpdateStager { _, _ in nil }

    /// Live stager: a real `UpdateDownloader` rooted at
    /// `~/Library/Application Support/Boss/Updates`, with the running bundle's Team ID
    /// resolved once and cached.
    static func live(currentVersion: VersionTuple) -> UpdateStager {
        let teamID = RunningTeamIDCache()
        return UpdateStager { update, onProgress in
            let downloader = UpdateDownloader.live(
                currentVersion: currentVersion,
                runningTeamID: await teamID.resolve()
            )
            switch await downloader.download(update, onProgress: onProgress) {
            case .ready(let staged), .alreadyStaged(let staged):
                return staged.version
            case .failed:
                return nil
            }
        }
    }
}

// MARK: - UpdateBlocklist

/// Seam over ``UpdateInstaller``'s failed-version blocklist and rollback history so
/// ``UpdateModel`` can refuse to re-stage a version that failed its first launch,
/// surface the recorded rollback to the user, and clear the block only on an explicit
/// retry. `.noop` never blocks (placeholder/preview models); tests inject their own.
public struct UpdateBlocklist: Sendable {
    public let isBlocked: @Sendable (VersionTuple) -> Bool
    public let rollback: @Sendable (VersionTuple) -> RollbackRecord?
    public let lastRollback: @Sendable () -> RollbackRecord?
    public let unblock: @Sendable (VersionTuple) -> Void
    public let acknowledgeRollbacks: @Sendable () -> Void

    public init(
        isBlocked: @escaping @Sendable (VersionTuple) -> Bool,
        rollback: @escaping @Sendable (VersionTuple) -> RollbackRecord?,
        lastRollback: @escaping @Sendable () -> RollbackRecord?,
        unblock: @escaping @Sendable (VersionTuple) -> Void,
        acknowledgeRollbacks: @escaping @Sendable () -> Void
    ) {
        self.isBlocked = isBlocked
        self.rollback = rollback
        self.lastRollback = lastRollback
        self.unblock = unblock
        self.acknowledgeRollbacks = acknowledgeRollbacks
    }

    public static let noop = UpdateBlocklist(
        isBlocked: { _ in false }, rollback: { _ in nil }, lastRollback: { nil },
        unblock: { _ in }, acknowledgeRollbacks: {})

    /// Backed by the live installer's `install-state.json`.
    public static func live(installer: UpdateInstaller) -> UpdateBlocklist {
        UpdateBlocklist(
            isBlocked: { installer.isBlocked($0) },
            rollback: { installer.rollbackRecord(for: $0) },
            lastRollback: { installer.lastRollback() },
            unblock: { installer.unblockVersion($0) },
            acknowledgeRollbacks: { installer.acknowledgeRollbacks() })
    }
}

/// Resolves the running bundle's signing Team ID once and caches it. Reading the
/// Team ID shells out to `codesign`, so we avoid repeating it on every download.
private actor RunningTeamIDCache {
    private var cached: String??

    func resolve() async -> String? {
        if let cached { return cached }
        let value = (try? await BundleOperations.live.readTeamID(Bundle.main.bundleURL)) ?? nil
        cached = .some(value)
        return value
    }
}

// MARK: - UpdateModel

/// Observable model that owns update state and drives the polling scheduler.
///
/// All state is `@MainActor`-isolated. App-layer SwiftUI views import this via `import UpdateCore`
/// and observe it directly — there is no back-dependency on app-level types.
///
/// **Lifecycle:** call `startPollingIfNeeded()` once from `applicationDidFinishLaunching`.
/// Manual checks are always available via `checkNow()` regardless of mode.
@MainActor
public final class UpdateModel: ObservableObject {

    // MARK: - Constants

    /// Default polling interval per the design (6 hours).
    public static let pollInterval: TimeInterval = 6 * 3600

    // MARK: - Published state

    @Published public private(set) var mode: UpdateMode
    @Published public private(set) var lastCheckDate: Date?
    @Published public private(set) var skippedVersion: String
    @Published public private(set) var lastCheckResult: UpdateCheckResult?
    @Published public private(set) var isChecking: Bool = false
    /// Set to `true` to present the update-result sheet. Dismissed when the user closes the sheet.
    @Published public var showUpdateSheet: Bool = false
    /// Transient toast state set only by ``presentUpdateSheet()``. `nil` when no toast is visible.
    @Published public private(set) var manualCheckFeedback: ManualUpdateFeedback? = nil
    /// State of the in-app download/stage step. Drives the badge/sheet/Settings
    /// "downloading…" / "ready to install" affordances. See ``UpdateDownloadState``.
    @Published public private(set) var downloadState: UpdateDownloadState = .idle
    /// Set when `NSApplication.terminate` returned without quitting after an
    /// Install & Relaunch / Quit to Finish. The sheet is re-presented with a
    /// status line so a cancelled or vetoed quit is never silent.
    @Published public private(set) var quitReturnedWithoutTerminating: Bool = false
    /// The most recent watchdog rollback the user has not dismissed yet. Set by
    /// ``refreshRollbackNotice()`` at launch (after `reconcileAtLaunch` folded the
    /// helper's marker into state); drives the "Update to X failed … rolled back to Y"
    /// chrome banner. Cleared by ``dismissRollbackNotice()`` or ``retryBlockedUpdate(_:)``.
    @Published public private(set) var rollbackNotice: RollbackRecord?

    // MARK: - Private

    private var toastDismissTask: Task<Void, Never>?
    private var stagingTask: Task<Void, Never>?

    private enum StorageKeys {
        static let mode = "boss.update.mode"
        static let lastCheck = "boss.update.lastCheck"
        static let skippedVersion = "boss.update.skippedVersion"
    }

    private let defaults: UserDefaults
    private let checker: UpdateChecker
    private let stager: UpdateStager
    private let blocklist: UpdateBlocklist
    /// See ``attemptID(for:)``.
    private var attemptIDs: [String: String] = [:]
    private let log: UpdateLog?
    private let jitterRange: ClosedRange<TimeInterval>
    private var schedulerTask: Task<Void, Never>?

    // MARK: - Init

    /// Convenience factory that reads version info from the running bundle and wires
    /// the live downloader so automatic mode can stage updates.
    ///
    /// Pass the app's isolation-aware defaults store (e.g. `BossDefaults.store`)
    /// so capture instances never write update prefs into `UserDefaults.standard`.
    public static func fromBundle(defaults: UserDefaults = .standard) -> UpdateModel? {
        guard let checker = UpdateChecker.fromBundle() else { return nil }
        let current = (Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String)
            .flatMap(VersionTuple.parse) ?? VersionTuple(major: 0, minor: 0, patch: 0)
        let installer = UpdateInstaller.live(installBundleURL: Bundle.main.bundleURL)
        return UpdateModel(
            checker: checker,
            defaults: defaults,
            stager: .live(currentVersion: current),
            blocklist: .live(installer: installer),
            log: installer.log
        )
    }

    /// Returns a no-op model used when the bundle is unavailable (e.g. SwiftUI previews).
    /// Badge is never shown; checks are no-ops.
    public static func placeholder() -> UpdateModel {
        UpdateModel(
            checker: UpdateChecker(
                currentVersionString: "0.0.0",
                fullVersionString: "0.0.0-dev-placeholder",
                fetcher: .noop
            )
        )
    }

    /// Designated initializer. `defaults`, `jitterRange`, `stager`, `blocklist` and
    /// `log` are injectable for testing. `stager` defaults to `.noop` so tests that
    /// don't exercise the download path never touch the network/filesystem.
    init(
        checker: UpdateChecker,
        defaults: UserDefaults = .standard,
        jitterRange: ClosedRange<TimeInterval> = 30...120,
        stager: UpdateStager = .noop,
        blocklist: UpdateBlocklist = .noop,
        log: UpdateLog? = nil
    ) {
        self.defaults = defaults
        self.checker = checker
        self.stager = stager
        self.blocklist = blocklist
        self.log = log
        self.jitterRange = jitterRange

        let modeRaw = defaults.string(forKey: StorageKeys.mode) ?? UpdateMode.notify.rawValue
        self.mode = UpdateMode(rawValue: modeRaw) ?? .notify

        let epoch = defaults.double(forKey: StorageKeys.lastCheck)
        self.lastCheckDate = epoch == 0 ? nil : Date(timeIntervalSince1970: epoch)

        self.skippedVersion = defaults.string(forKey: StorageKeys.skippedVersion) ?? ""
    }

    // MARK: - Public API

    /// `true` when the running binary is a dev build (BossFullVersion contains `-dev-`).
    /// The sheet uses this to suppress auto-install while still showing update availability.
    public var isDevBuild: Bool { checker.isDevBuild }

    /// Factory that always returns a model. Falls back to a version-less dev build if
    /// the bundle's `CFBundleShortVersionString` is unavailable (e.g. `swift run` without plist).
    ///
    /// Callers in the macOS app must pass `BossDefaults.store` so isolated capture
    /// instances keep update prefs out of the production defaults suite.
    public static func makeForApp(defaults: UserDefaults = .standard) -> UpdateModel {
        if let model = fromBundle(defaults: defaults) { return model }
        // No `.app` bundle (e.g. `swift run` / bazel-run local dev). This is always a
        // dev build, which never auto-stages, so a `.noop` stager is correct.
        return UpdateModel(
            checker: UpdateChecker(
                currentVersionString: "0.0.0",
                fullVersionString: "0.0.0-dev-local",
                fetcher: .live
            ),
            defaults: defaults,
            stager: .noop
        )
    }

    /// Starts a manual update check and surfaces transient toast feedback.
    /// Shows the result sheet only when an update is available; all other outcomes
    /// (up-to-date, error, rate-limited) show a self-dismissing toast instead.
    public func presentUpdateSheet() {
        modelLog.info("update check triggered: source=manual-menu")
        toastDismissTask?.cancel()
        manualCheckFeedback = .checking
        Task {
            let result = await checkNow()
            switch result {
            case .upToDate:
                manualCheckFeedback = .upToDate
                scheduleFeedbackDismissal()
            case .available:
                manualCheckFeedback = nil
                showUpdateSheet = true
            case .networkError(let message):
                manualCheckFeedback = .networkError(message)
                scheduleFeedbackDismissal()
            case .rateLimited(let retryAfter):
                manualCheckFeedback = .rateLimited(retryAfter)
                scheduleFeedbackDismissal()
            }
        }
    }

    private func scheduleFeedbackDismissal() {
        toastDismissTask?.cancel()
        toastDismissTask = Task {
            try? await Task.sleep(for: .seconds(3))
            guard !Task.isCancelled else { return }
            manualCheckFeedback = nil
        }
    }

    /// Starts the polling scheduler if the current mode enables it.
    /// Call once from `applicationDidFinishLaunching`.
    public func startPollingIfNeeded() {
        guard mode != .manual else {
            modelLog.info("update polling: skipped (mode=manual)")
            return
        }
        modelLog.info("update polling: starting background scheduler (mode=\(self.mode.rawValue, privacy: .public) intervalSeconds=\(Self.pollInterval, privacy: .public))")
        startPolling()
    }

    /// Changes the update mode, persists it, and starts or stops polling as needed.
    public func setMode(_ newMode: UpdateMode) {
        let oldMode = mode
        modelLog.info("update mode changed: \(oldMode.rawValue, privacy: .public) → \(newMode.rawValue, privacy: .public)")
        mode = newMode
        defaults.set(newMode.rawValue, forKey: StorageKeys.mode)
        switch (oldMode, newMode) {
        case (.manual, .notify), (.manual, .automatic):
            startPolling()
        case (_, .manual):
            stopPolling()
        default:
            break  // already polling; no restart needed for notify ↔ automatic transitions
        }
    }

    /// Performs an immediate update check regardless of mode, updates `lastCheckDate`,
    /// and returns the result. Safe to call from a "Check for Updates…" menu action.
    @discardableResult
    public func checkNow() async -> UpdateCheckResult {
        isChecking = true
        defer { isChecking = false }
        var result = await checker.checkForUpdates()
        if case .available(var update) = result {
            // The attempt starts at the update decision: this id is carried through
            // staging/download (manifest), the swap plan and pending record, the
            // helper, and reconcile.
            update.attemptID = attemptID(for: update.version)
            result = .available(update)
        }
        lastCheckResult = result
        let now = Date()
        lastCheckDate = now
        defaults.set(now.timeIntervalSince1970, forKey: StorageKeys.lastCheck)

        switch result {
        case .upToDate:
            modelLog.info("update check complete: up-to-date")
            log?.record(stage: "check", event: "up-to-date", attempt: nil, from: checker.currentVersion)
        case .available(let update):
            modelLog.info(
                "update check complete: update available version=\(update.version.description, privacy: .public) tag=\(update.tagName, privacy: .public)"
            )
            log?.record(stage: "check", event: "available", attempt: update.attemptID, from: checker.currentVersion, to: update.version,
                        detail: ["tag": update.tagName, "url": update.assetURL.absoluteString,
                                 "bytes": String(update.assetSize), "mode": mode.rawValue,
                                 "blocked": String(blocklist.isBlocked(update.version))])
        case .rateLimited(let retryAfter):
            modelLog.warning("update check complete: rate-limited, retry after \(retryAfter.description, privacy: .public)")
            log?.record(stage: "check", event: "rate-limited", level: "warning", attempt: nil,
                        from: checker.currentVersion, detail: ["retryAfter": retryAfter.description])
        case .networkError(let message):
            modelLog.error("update check complete: network error — \(message, privacy: .public)")
            log?.record(stage: "check", event: "network-error", level: "warning", attempt: nil,
                        from: checker.currentVersion, detail: ["error": message])
        }

        // In automatic mode, a known-available update is downloaded and staged here
        // so the next quit/startup boundary can swap it in. Without this step the
        // installer's `newestReadyUpdate` never finds anything and the app stays on
        // the running version forever even with "Automatic" selected.
        maybeAutoStage(result)

        return result
    }

    /// Records the current available version as skipped so the badge is suppressed in Notify mode.
    /// No-op if `lastCheckResult` is not `.available`.
    public func skipCurrentVersion() {
        guard case .available(let update) = lastCheckResult else { return }
        skippedVersion = update.version.description
        defaults.set(skippedVersion, forKey: StorageKeys.skippedVersion)
    }

    /// Clears any previously skipped version, re-enabling badge display if an update is available.
    public func clearSkippedVersion() {
        skippedVersion = ""
        defaults.removeObject(forKey: StorageKeys.skippedVersion)
    }

    // MARK: - Download / stage

    /// Records that the in-place install+relaunch step failed with the given reason.
    /// Called by the UI so the model reflects the terminal error and the sheet shows
    /// a clear failure state — no browser fallback.
    public func markInstallFailed(version: VersionTuple, reason: String) {
        modelLog.error("update install: failed version=\(version.description, privacy: .public) reason=\(reason, privacy: .public)")
        log?.record(stage: "swap", event: "install-failed", level: "error", attempt: attemptID(for: version),
                    from: checker.currentVersion, to: version, detail: ["reason": reason])
        downloadState = .installFailed(version: version, reason: reason)
    }

    // MARK: - Attempt ids

    /// The attempt id for an update to `version`, minted once per version per process
    /// so repeated checks of the same release, its staging and a later install-failure
    /// record all share one id. A deliberate retry replaces it with a fresh id
    /// (``retryBlockedUpdate(_:)``). A version already staged by an earlier process
    /// keeps the id in its manifest; the downloader's `stage.already-staged` record
    /// carries that id and links this one as `requestedAttempt`.
    private func attemptID(for version: VersionTuple) -> String {
        if let id = attemptIDs[version.description] { return id }
        let id = UpdateAttemptID.make()
        attemptIDs[version.description] = id
        return id
    }

    // MARK: - Rollback notice / blocklist

    /// `true` when `version` failed a prior first launch and is blocklisted. The UI
    /// offers "Retry Update" instead of "Download" for such a version.
    public func isBlocked(_ version: VersionTuple) -> Bool {
        blocklist.isBlocked(version)
    }

    /// The recorded rollback for `version`, if it was ever rolled back.
    public func rollbackRecord(for version: VersionTuple) -> RollbackRecord? {
        blocklist.rollback(version)
    }

    /// Re-read the persisted rollback history and surface the latest rollback the user
    /// has not yet dismissed. Call once at launch after `reconcileAtLaunch`.
    public func refreshRollbackNotice() {
        guard let record = blocklist.lastRollback(), !record.acknowledged else {
            rollbackNotice = nil
            return
        }
        rollbackNotice = record
        log?.record(stage: "reconcile", event: "rollback-notice-shown", attempt: record.attemptID,
                    from: record.restoredVersion.flatMap(VersionTuple.parse), to: VersionTuple.parse(record.version), detail: ["reason": record.reason.rawValue])
    }

    /// The user dismissed the rollback banner. The version stays blocklisted.
    public func dismissRollbackNotice() {
        guard let record = rollbackNotice else { return }
        blocklist.acknowledgeRollbacks()
        rollbackNotice = nil
        log?.record(stage: "reconcile", event: "rollback-notice-dismissed", attempt: record.attemptID,
                    from: record.restoredVersion.flatMap(VersionTuple.parse), to: VersionTuple.parse(record.version))
    }

    /// Deliberate retry of a version the watchdog rolled back: clear its block, then
    /// stage it again (re-checking first when the last check is stale or missing) and
    /// present the sheet so the user can press Install & Relaunch. This is the only
    /// path that re-downloads a blocklisted version; background polling and the plain
    /// "Download" button refuse to. The watchdog re-blocklists it if it fails again.
    public func retryBlockedUpdate(_ version: VersionTuple) {
        guard !isDevBuild else { return }
        // A retry is a new attempt: mint it here and use it for everything that
        // follows (the unblock, the re-check, staging, swap, helper, reconcile).
        let retryAttempt = UpdateAttemptID.make()
        attemptIDs[version.description] = retryAttempt
        log?.record(stage: "blocklist", event: "retry-requested", attempt: retryAttempt,
                    from: checker.currentVersion, to: version, detail: ["source": "user"])
        blocklist.unblock(version)
        blocklist.acknowledgeRollbacks()
        rollbackNotice = nil
        showUpdateSheet = true
        if case .available(var update) = lastCheckResult, update.version >= version {
            update.attemptID = attemptID(for: update.version)
            beginStaging(update)
            return
        }
        Task { [weak self] in
            guard let self else { return }
            let result = await self.checkNow()
            if case .available(let update) = result, update.version >= version {
                self.beginStaging(update)
            }
        }
    }

    /// Records that the bundle swap for `version` was applied but the app has not yet
    /// relaunched into it — the update completes on the next quit. Called by the UI
    /// when "Install & Relaunch" swapped the bundle but the quit was vetoed (agents
    /// working), or when no relaunch helper could be armed. `willRelaunch` distinguishes
    /// whether the helper will reopen Boss automatically. This is a success state, not a
    /// failure — the new version is already on disk.
    public func markInstalledPendingRelaunch(version: VersionTuple, willRelaunch: Bool) {
        modelLog.info(
            "update install: swap applied, awaiting relaunch version=\(version.description, privacy: .public) willRelaunch=\(willRelaunch, privacy: .public)")
        downloadState = .installedPendingRelaunch(version: version, willRelaunch: willRelaunch)
    }

    /// `terminate` returned without ending the process (live-worker Cancel, or
    /// another veto). Re-present the sheet and explain that the install is
    /// waiting on a confirmed quit.
    public func markQuitReturnedWithoutTerminating() {
        modelLog.info("update quit: terminate returned without quitting; re-presenting update sheet")
        quitReturnedWithoutTerminating = true
    }

    public func clearQuitReturnedWithoutTerminating() {
        quitReturnedWithoutTerminating = false
    }

    /// Status shown after a quit request returns without terminating, so the
    /// user has a way through (Quit to Finish) instead of a silent close.
    public func quitCancelledStatusNote(for version: VersionTuple) -> String? {
        guard quitReturnedWithoutTerminating else { return nil }
        return "Quit was cancelled. Boss \(version.description) is installed; quit when ready to finish the update."
    }

    /// User-initiated download of the currently-available update (the result sheet /
    /// badge "Download" button). Stages regardless of mode — the user explicitly
    /// asked for it. Dev builds never stage (auto-install is a non-goal for them);
    /// no-op when no update is available.
    public func downloadAvailableUpdate() {
        guard !isDevBuild, case .available(let update) = lastCheckResult else { return }
        guard !blocklist.isBlocked(update.version) else {
            // A version that failed its first launch is only re-staged through the
            // explicit ``retryBlockedUpdate(_:)`` path, which clears the block first.
            modelLog.warning("update download: refusing to stage blocklisted version \(update.version.description, privacy: .public) without an explicit retry")
            log?.record(stage: "stage", event: "refused-blocklisted", level: "warning", attempt: update.attemptID,
                        from: checker.currentVersion, to: update.version, detail: ["source": "download-button"])
            return
        }
        beginStaging(update)
    }

    /// Automatic-mode auto-stage: begin downloading/staging an available update so a
    /// later quit/startup boundary can swap it in. No-op for non-automatic modes, dev
    /// builds, or non-`.available` results. Idempotent — see ``beginStaging(_:)``.
    private func maybeAutoStage(_ result: UpdateCheckResult) {
        guard mode == .automatic, !isDevBuild else { return }
        guard case .available(let update) = result else { return }
        guard !blocklist.isBlocked(update.version) else {
            // The watchdog rolled this version back and `reconcileAtLaunch` reaped its
            // staged directory. Re-downloading it here would silently undo that: the
            // next quit/startup boundary would retry a version known to fail. Only an
            // explicit user retry (``retryBlockedUpdate(_:)``) may re-stage it.
            modelLog.warning("update auto-stage: skipping blocklisted version \(update.version.description, privacy: .public)")
            log?.record(stage: "stage", event: "skipped-blocklisted", level: "warning", attempt: update.attemptID,
                        from: checker.currentVersion, to: update.version, detail: ["source": "auto-stage"])
            return
        }
        beginStaging(update)
    }

    /// Kick off (or resume awareness of) a download for `update`. Idempotent: if we're
    /// already downloading or have already staged this exact version, it does nothing,
    /// so repeated checks (or a manual click on top of an auto-download) never restart
    /// an in-flight or completed stage. A newer version supersedes an older in-flight one.
    private func beginStaging(_ update: AvailableUpdate) {
        let version = update.version
        switch downloadState {
        case .downloading(let v, _) where v == version: return
        case .readyToInstall(let v) where v == version: return
        // Already swapped in and awaiting relaunch: the bundle is installed and the
        // staged copy consumed. Re-staging would needlessly re-download and revert the
        // honest "Installed — quit to finish" UI back to "Downloading…". Skip it.
        case .installedPendingRelaunch(let v, _) where v == version: return
        default: break
        }
        stagingTask?.cancel()
        downloadState = .downloading(version: version, progress: .determinate(0))
        modelLog.info("update download: staging version=\(version.description, privacy: .public)")
        log?.record(stage: "stage", event: "begin", attempt: update.attemptID, from: checker.currentVersion, to: version,
                    detail: ["mode": mode.rawValue])
        stagingTask = Task { [weak self] in
            guard let self else { return }
            let staged = await self.stager.stage(update) { [weak self] progress in
                Task { @MainActor in
                    guard let self else { return }
                    // Only advance progress if we're still on this version's download.
                    if case .downloading(let v, _) = self.downloadState, v == version {
                        self.downloadState = .downloading(version: version, progress: progress)
                    }
                }
            }
            guard !Task.isCancelled else { return }
            if let staged {
                modelLog.info("update download: staged and ready version=\(staged.description, privacy: .public)")
                self.downloadState = .readyToInstall(version: staged)
            } else {
                modelLog.error("update download: staging failed version=\(version.description, privacy: .public)")
                self.downloadState = .failed(version: version, reason: "Download or verification failed")
            }
        }
    }

    /// Test hook: await the in-flight staging task, if any, so tests can wait for the
    /// background download to settle before asserting on ``downloadState``.
    func awaitStagingForTesting() async {
        await stagingTask?.value
    }

    // MARK: - Scheduler

    private func startPolling() {
        schedulerTask?.cancel()
        schedulerTask = Task { [weak self] in
            guard let self else { return }
            await self.runSchedulerLoop()
        }
    }

    private func stopPolling() {
        schedulerTask?.cancel()
        schedulerTask = nil
    }

    /// Runs the polling loop: jittered launch delay → check → interval sleep → repeat.
    /// Backs off to `retryAfter` on rate-limit responses. Exits cleanly on cancellation.
    private func runSchedulerLoop() async {
        // Jitter prevents a fleet of machines behind one NAT from hitting the API at the same second.
        let jitter = TimeInterval.random(in: jitterRange)
        modelLog.info("update scheduler: first check in \(jitter, privacy: .public)s (jitter)")
        do {
            try await Task.sleep(for: .seconds(jitter))
        } catch {
            return
        }

        while !Task.isCancelled {
            modelLog.info("update check triggered: source=background-poll")
            let result = await checkNow()

            let nextDelay: TimeInterval
            if case .rateLimited(let retryAfter) = result {
                // Suspend until the server clears our rate limit.
                nextDelay = max(retryAfter.timeIntervalSinceNow, 0)
            } else {
                nextDelay = Self.pollInterval
            }

            modelLog.info("update scheduler: next poll in \(nextDelay, privacy: .public)s")
            do {
                try await Task.sleep(for: .seconds(nextDelay))
            } catch {
                return
            }
        }
    }
}
