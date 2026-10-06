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
    private let snapshotOverride: (() -> IdleApplySnapshot)?
    private let install: (Bool) -> InstallOutcome
    private let restartOverride: ((@escaping @MainActor @Sendable (IdleApplyOutcome) -> Void) -> Void)?
    private var applier: IdleUpdateApplier?
    private var timer: Timer?
    private var cancellables = Set<AnyCancellable>()
    /// The "nothing to apply" note is on screen; keep it until the next request.
    private var showingUnfulfillableRequest = false

    init(
        updateModel: UpdateModel, chatModel: ChatViewModel?, liveWorkerStates: LiveWorkerStateStore,
        snapshot: (() -> IdleApplySnapshot)? = nil,
        install: @escaping (Bool) -> InstallOutcome = { UpdateLifecycle.installStagedAndRelaunch(userInitiated: $0) },
        restart: ((@escaping @MainActor @Sendable (IdleApplyOutcome) -> Void) -> Void)? = nil
    ) {
        self.snapshotOverride = snapshot
        self.install = install
        self.restartOverride = restart
        self.updateModel = updateModel
        self.chatModel = chatModel
        self.liveWorkerStates = liveWorkerStates
    }

    func start() {
        guard applier == nil, let chatModel else { return }
        startApplying()

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

    func startApplying(policy: IdleUpdateApplier.Policy = .init()) {
        applier = IdleUpdateApplier(
            policy: policy,
            snapshot: { [weak self] in self?.snapshot() ?? Self.inertSnapshot },
            perform: { [weak self] action, completion in self?.perform(action, completion: completion) }
        )
    }

    func tick() {
        guard let applier else { return }
        let decision = applier.tick()
        if case .installFailed = updateModel.downloadState {
            updateModel.setIdleApplyStatus(unfulfillableRequestMessage(downloadState: updateModel.downloadState))
            showingUnfulfillableRequest = true
            return
        }
        if let outcome = applier.outcome {
            switch outcome {
            case .deferred(let message), .failed(let message):
                updateModel.setIdleApplyStatus(message)
                return
            case .performed: break
            }
        }
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
        case .wait(.liveWorkers(let count)) where chatModel?.bundledEngineMismatchKey != nil:
            updateModel.setIdleApplyStatus("Bundled engine differs; restart deferred: \(count) live workers.")
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
        if let snapshotOverride { return snapshotOverride() }
        return IdleApplySnapshot(
            mode: updateModel.mode,
            isDevBuild: updateModel.isDevBuild,
            userRequested: updateModel.applyWhenIdleRequested,
            isPreparingUpdate: updateModel.isPreparingUpdate,
            stagedVersion: updateModel.versionReadyToApply,
            engineBehindBundle: engineBehindBundle,
            engineFingerprintMismatch: chatModel?.bundledEngineMismatchKey != nil,
            engineRestartKey: chatModel?.bundledEngineMismatchKey ?? (engineBehindBundle ? chatModel?.engineRelease.map { "\($0.engineVersion)->\(UpdateLifecycle.runningVersion?.description ?? "?")" } : nil),
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
              let chatModel,
              let release = chatModel.engineRelease
        else { return false }
        if chatModel.bundledEngineMismatchKey != nil { return true }
        return engineIsBehindBundle(
            engineVersion: release.engineVersion,
            bundleVersion: UpdateLifecycle.runningVersion
        )
    }

    // MARK: - Side effects

    private func perform(_ action: IdleApplyAction, completion: @escaping @MainActor @Sendable (IdleApplyOutcome) -> Void) {
        let userRequested = updateModel.applyWhenIdleRequested
        switch action {
        case .restartEngine:
            freshnessLog.info("update apply-at-idle: restarting engine onto the bundled build (no live workers)")
            let finished: @MainActor @Sendable (IdleApplyOutcome) -> Void = { [weak self] outcome in
                completion(outcome)
                if outcome == .performed, userRequested { self?.updateModel.clearApplyWhenIdleRequest() }
                switch outcome {
                case .deferred(let message), .failed(let message): self?.updateModel.setIdleApplyStatus(message)
                case .performed: break
                }
            }
            if let restartOverride { restartOverride(finished) }
            else if let chatModel { chatModel.restartEngine(onlyIfNoLiveWorkers: true, completion: finished) }
            else { finished(.deferred("Waiting for the engine connection.")) }

        case .relaunchIntoStagedUpdate(let version):
            updateModel.clearApplyWhenIdleRequest()
            // A swap already applied by an earlier Install & Relaunch whose quit was
            // vetoed only needs the quit; do not swap twice.
            if UpdateLifecycle.pendingRelaunch == nil {
                switch install(userRequested) {
                case .relaunchPending:
                    updateModel.markInstalledPendingRelaunch(version: version, willRelaunch: true)
                case .installedNoRelaunch:
                    updateModel.markInstalledPendingRelaunch(version: version, willRelaunch: false)
                    completion(.performed)
                    return
                case .notInstalled:
                    updateModel.markInstallFailed(version: version, reason: UpdateInstallAction.notInstalledReason)
                    completion(.failed(unfulfillableRequestMessage(downloadState: updateModel.downloadState)))
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
            completion(.performed)
            NSApp.terminate(nil)
        }
    }
}
