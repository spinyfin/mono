import AppKit
import Combine
import UpdateCore
import os.log

private let freshnessLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "updater")

/// App-side glue that closes the gap between "release published" and "running".
///
/// Two jobs, both built on the existing updater — there is no second release poller:
///
/// 1. **Report.** Forward the newest published release `UpdateModel` has seen to the
///    engine on every connect and whenever it changes, so the engine's health report
///    (and `bossctl health`) can say when the running engine is behind it.
/// 2. **Apply at idle.** Tick ``IdleUpdateApplier``, which decides when a staged
///    update may be swapped in without waiting for an app quit. The rules
///    live there and are unit-tested; this type only gathers the inputs and performs
///    the two side effects (relaunch the app, or restart the engine).
///
/// Never started in capture instances or under XCTest.
@MainActor
final class EngineFreshnessDriver {
    /// How often the idle gate is re-evaluated. Each tick reads a handful of
    /// in-memory values, so this is cheap; state changes also tick immediately.
    private static let tickInterval: TimeInterval = 10

    private let updateModel: UpdateModel
    private let liveWorkerStates: LiveWorkerStateStore
    private weak var chatModel: ChatViewModel?
    private var applier: IdleUpdateApplier?
    private var timer: Timer?
    private var cancellables = Set<AnyCancellable>()
    /// The "nothing to apply" note is on screen; keep it until the next request.
    private var showingUnfulfillableRequest = false

    init(updateModel: UpdateModel, chatModel: ChatViewModel, liveWorkerStates: LiveWorkerStateStore) {
        self.updateModel = updateModel
        self.chatModel = chatModel
        self.liveWorkerStates = liveWorkerStates
    }

    func start() {
        guard applier == nil, let chatModel else { return }
        applier = IdleUpdateApplier(
            snapshot: { [weak self] in self?.snapshot() ?? Self.inertSnapshot },
            perform: { [weak self] action in self?.perform(action) }
        )

        // Report the newest published release once connected, and again whenever
        // either side changes (a reconnect means a possibly different engine).
        updateModel.$newestPublishedVersion
            .combineLatest(chatModel.$isConnected)
            .sink { [weak self] version, isConnected in
                guard isConnected, let version else { return }
                Task { @MainActor [weak self] in
                    self?.chatModel?.reportNewestPublishedRelease(version.description)
                }
            }
            .store(in: &cancellables)

        // An explicit request overrides the retry cooldown and is evaluated at once.
        updateModel.$applyWhenIdleRequested
            .removeDuplicates()
            .sink { [weak self] requested in
                guard requested else { return }
                Task { @MainActor [weak self] in
                    self?.applier?.noteUserRequest()
                    self?.showingUnfulfillableRequest = false
                    self?.tick()
                }
            }
            .store(in: &cancellables)

        // Staging finishing, or the last worker exiting, is exactly when an apply
        // may become possible; do not make it wait for the next timer tick.
        updateModel.$downloadState
            .removeDuplicates()
            .sink { [weak self] _ in Task { @MainActor [weak self] in self?.tick() } }
            .store(in: &cancellables)
        liveWorkerStates.$activeAgentCount
            .removeDuplicates()
            .sink { [weak self] _ in Task { @MainActor [weak self] in self?.tick() } }
            .store(in: &cancellables)

        timer = Timer.scheduledTimer(withTimeInterval: Self.tickInterval, repeats: true) { [weak self] _ in
            Task { @MainActor [weak self] in self?.tick() }
        }
    }

    private func tick() {
        guard let applier else { return }
        let decision = applier.tick()
        switch decision {
        case .nothingToApply where updateModel.applyWhenIdleRequested:
            // Requested, but nothing could be applied: no installable newer build, or a
            // failed download/install. Say which instead of queueing forever.
            updateModel.clearApplyWhenIdleRequest()
            updateModel.setIdleApplyStatus(unfulfillableRequestMessage(downloadState: updateModel.downloadState))
            showingUnfulfillableRequest = true
        case .notEligible, .nothingToApply:
            if !showingUnfulfillableRequest {
                updateModel.setIdleApplyStatus(nil)
            }
        case .wait, .apply:
            showingUnfulfillableRequest = false
            updateModel.setIdleApplyStatus(decision.statusText)
        }
    }

    // MARK: - Inputs

    /// Used only if the driver is torn down mid-tick: nothing is ever eligible.
    private static let inertSnapshot = IdleApplySnapshot(
        mode: .manual, isDevBuild: true, userRequested: false, stagedVersion: nil,
        engineBehindBundle: false, engineReachable: false, liveWorkerCount: 0,
        secondsSinceUserInput: 0
    )

    private func snapshot() -> IdleApplySnapshot {
        IdleApplySnapshot(
            mode: updateModel.mode,
            isDevBuild: updateModel.isDevBuild,
            userRequested: updateModel.applyWhenIdleRequested,
            isPreparingUpdate: updateModel.isPreparingUpdate,
            stagedVersion: updateModel.versionReadyToApply,
            engineBehindBundle: engineBehindBundle,
            engineRestartKey: engineBehindBundle ? chatModel?.engineRelease.map { "\($0.engineVersion)->\(UpdateLifecycle.runningVersion?.description ?? "?")" } : nil,
            engineReachable: chatModel?.isConnected ?? false,
            liveWorkerCount: liveWorkerStates.activeAgentCount,
            hasModalUI: NSApp.modalWindow != nil || NSApp.windows.contains { $0.attachedSheet != nil },
            secondsSinceUserInput: CGEventSource.secondsSinceLastEventType(
                .combinedSessionState,
                // kCGAnyInputEventType
                eventType: CGEventType(rawValue: ~0)!
            )
        )
    }

    /// The engine is older than the engine inside this app bundle, so restarting it
    /// (which launches the bundled binary) would move it forward. Never true for a
    /// developer's custom engine, which has no bundled counterpart to restart onto.
    private var engineBehindBundle: Bool {
        guard ProcessInfo.processInfo.environment["BOSS_ENGINE_CMD"] == nil,
              let chatModel, !chatModel.isRestartingEngine,
              let release = chatModel.engineRelease
        else { return false }
        return engineIsBehindBundle(
            engineVersion: release.engineVersion,
            bundleVersion: UpdateLifecycle.runningVersion
        )
    }

    // MARK: - Side effects

    private func perform(_ action: IdleApplyAction) {
        let userRequested = updateModel.applyWhenIdleRequested
        updateModel.clearApplyWhenIdleRequest()
        switch action {
        case .restartEngine:
            freshnessLog.info("update apply-at-idle: restarting engine onto the bundled build (no live workers)")
            chatModel?.restartEngine(onlyIfNoLiveWorkers: true)

        case .relaunchIntoStagedUpdate(let version):
            // A swap already applied by an earlier Install & Relaunch whose quit was
            // vetoed only needs the quit; do not swap twice.
            if UpdateLifecycle.pendingRelaunch == nil {
                switch UpdateLifecycle.installStagedAndRelaunch(userInitiated: userRequested) {
                case .relaunchPending:
                    updateModel.markInstalledPendingRelaunch(version: version, willRelaunch: true)
                case .installedNoRelaunch:
                    updateModel.markInstalledPendingRelaunch(version: version, willRelaunch: false)
                    return
                case .notInstalled:
                    updateModel.markInstallFailed(version: version, reason: UpdateInstallAction.notInstalledReason)
                    return
                }
            }
            freshnessLog.info(
                "update apply-at-idle: relaunching into \(version.description, privacy: .public) (no live workers, userRequested=\(userRequested, privacy: .public))")
            // `applicationShouldTerminate` re-checks the app's cached live-worker
            // count, but the detached engine keeps dispatching while the app quits
            // and relaunches. The guarantee that no worker is killed lives where
            // the engine is actually stopped: the relaunched app's launch-time
            // upgrade attaches to an engine that reports live workers instead of
            // replacing it, and this applier restarts it later once it is idle.
            // `applicationWillTerminate` arms the relaunch helper.
            NSApp.terminate(nil)
        }
    }
}
