import Foundation
import os.log

private let idleApplyLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "updater")

// Applying a published release without waiting for an app quit.
//
// The engine ships inside the app bundle and follows it, so "get the engine onto
// the newest release" means "swap the bundle and relaunch the app" — the launch-time
// fingerprint check then replaces the engine. This file decides *when* that is safe.
// It never performs the swap itself; the app layer injects that.
//
// Safety rules (see `IdleUpdateApplier.decide`):
// - Never while any worker is live. A live worker is one the engine reports as
//   spawning, working, waiting for input, or idle at its prompt.
// - Never when the engine is unreachable, because then the worker count is stale.
// - Never install over a dev build; engine-only bundle matching is allowed.
// - Unattended (automatic mode) applies also wait for a quiet period with no live
//   workers and for the user to be away from the keyboard.

/// Everything the apply-at-idle decision reads, captured at one instant.
public struct IdleApplySnapshot: Equatable, Sendable {
    public var mode: UpdateMode
    public var isDevBuild: Bool
    /// "Update & Restart" was pressed.
    public var userRequested: Bool
    /// A requested update is still being found or downloaded.
    public var isPreparingUpdate: Bool
    /// A verified update staged on disk (or already swapped in, awaiting relaunch).
    public var stagedVersion: VersionTuple?
    /// A launch-time binary mismatch still needs an engine-only restart, even in dev builds.
    public var engineFingerprintMismatch: Bool
    /// The running engine is older than the engine inside this app bundle.
    public var engineBehindBundle: Bool
    /// Identifies the engine/bundle fingerprint pair (versions when fingerprints are unavailable),
    /// so an unattended engine-only restart that did not change the engine version is
    /// not repeated for the same pair.
    public var engineRestartKey: String?
    public var engineReachable: Bool
    /// Workers the engine reports as alive, whether mid-turn or idle at a prompt.
    public var liveWorkerCount: Int
    /// A sheet or modal dialog is up; quitting under it would strand it.
    public var hasModalUI: Bool
    public var secondsSinceUserInput: TimeInterval

    public init(
        mode: UpdateMode,
        isDevBuild: Bool,
        userRequested: Bool,
        isPreparingUpdate: Bool = false,
        stagedVersion: VersionTuple?,
        engineBehindBundle: Bool,
        engineFingerprintMismatch: Bool = false,
        engineRestartKey: String? = nil,
        engineReachable: Bool,
        liveWorkerCount: Int,
        hasModalUI: Bool = false,
        secondsSinceUserInput: TimeInterval
    ) {
        self.mode = mode
        self.isDevBuild = isDevBuild
        self.userRequested = userRequested
        self.isPreparingUpdate = isPreparingUpdate
        self.stagedVersion = stagedVersion
        self.engineBehindBundle = engineBehindBundle
        self.engineFingerprintMismatch = engineFingerprintMismatch
        self.engineRestartKey = engineRestartKey
        self.engineReachable = engineReachable
        self.liveWorkerCount = liveWorkerCount
        self.hasModalUI = hasModalUI
        self.secondsSinceUserInput = secondsSinceUserInput
    }
}

public enum IdleApplyOutcome: Equatable, Sendable {
    case performed
    case deferred(String)
    case failed(String)
}

public enum IdleApplyAction: Equatable, Sendable {
    /// Swap the staged bundle in and relaunch the app; the relaunch replaces the engine.
    case relaunchIntoStagedUpdate(VersionTuple)
    /// The bundle is already current but the engine is older: restart only the engine.
    case restartEngine
}

public enum IdleApplyWaitReason: Equatable, Sendable {
    case preparingUpdate
    case recentAttempt
    case engineUnreachable
    case liveWorkers(Int)
    case modalUI
    case workersRecentlyActive
    case userActive
}

public enum IdleApplyDecision: Equatable, Sendable {
    /// This mode or build never applies on its own: manual/notify with no request, or a dev build.
    case notEligible
    /// Eligible, but there is no staged update and the engine matches the bundle.
    case nothingToApply
    case wait(IdleApplyWaitReason)
    case apply(IdleApplyAction)

    /// User-visible one-liner, or `nil` when there is nothing worth showing.
    public var statusText: String? {
        switch self {
        case .notEligible, .nothingToApply:
            return nil
        case .apply(.relaunchIntoStagedUpdate(let version)):
            return "Installing Boss \(version.description) and restarting…"
        case .apply(.restartEngine):
            return "Restarting the engine onto this app's build…"
        case .wait(let reason):
            switch reason {
            case .preparingUpdate:
                return "Downloading the update; it will be applied once no workers are live."
            case .recentAttempt:
                return "An update was just attempted; will retry later."
            case .engineUnreachable:
                return "Waiting for the engine connection before applying the update."
            case .liveWorkers(let count):
                return "Update will be applied when no workers are live (\(count) live now)."
            case .modalUI:
                return "Update will be applied once the open dialog is closed."
            case .workersRecentlyActive:
                return "Workers just finished; update will be applied shortly."
            case .userActive:
                return "Update will be applied when you step away, or press Update & Restart."
            }
        }
    }
}

/// Decides when a staged release may be applied without a quit, and triggers it.
///
/// Stateful only in what the decision needs across ticks: when workers were last
/// seen live, and when an apply was last attempted. Call ``tick()`` periodically.
@MainActor
public final class IdleUpdateApplier {

    public struct Policy: Equatable, Sendable {
        /// Unattended applies need this long with no live workers, so a gap between
        /// two back-to-back dispatches is not mistaken for idle.
        public var workerQuietSeconds: TimeInterval
        /// Unattended applies need this long with no keyboard/mouse input.
        public var userIdleSeconds: TimeInterval
        /// After an attempt, wait this long before another, so a failed apply
        /// cannot become a restart loop.
        public var retryCooldownSeconds: TimeInterval

        public init(
            workerQuietSeconds: TimeInterval = 60,
            userIdleSeconds: TimeInterval = 120,
            retryCooldownSeconds: TimeInterval = 600
        ) {
            self.workerQuietSeconds = workerQuietSeconds
            self.userIdleSeconds = userIdleSeconds
            self.retryCooldownSeconds = retryCooldownSeconds
        }
    }

    private let policy: Policy
    private let snapshot: () -> IdleApplySnapshot
    private let perform: (IdleApplyAction, @escaping @MainActor @Sendable (IdleApplyOutcome) -> Void) -> Void
    private let now: () -> Date

    private var workersIdleSince: Date?
    private var inFlight = false
    public private(set) var outcome: IdleApplyOutcome?
    private var lastAttemptAt: Date?
    /// The engine/bundle pair of the last unattended engine-only restart.
    private var lastUnattendedEngineRestartKey: String?
    public private(set) var lastDecision: IdleApplyDecision = .notEligible

    public init(
        policy: Policy = Policy(),
        snapshot: @escaping () -> IdleApplySnapshot,
        perform: @escaping (IdleApplyAction, @escaping @MainActor @Sendable (IdleApplyOutcome) -> Void) -> Void,
        now: @escaping () -> Date = { Date() }
    ) {
        self.policy = policy
        self.snapshot = snapshot
        self.perform = perform
        self.now = now
    }

    /// An explicit request overrides the retry cooldown and the non-convergence guard.
    public func noteUserRequest() {
        outcome = nil
        lastAttemptAt = nil
        lastUnattendedEngineRestartKey = nil
    }

    /// Evaluate once and, if the decision is `.apply`, perform it.
    @discardableResult
    public func tick() -> IdleApplyDecision {
        if inFlight { return lastDecision }
        // Deferrals are re-evaluated each tick; only terminal failures stay visible.
        if case .deferred = outcome { outcome = nil }
        let current = snapshot()
        let decision = decide(current, at: now())
        // A failure stays visible while something is still pending, but not once the engine
        // has converged some other way (e.g. a manual Restart Engine): there is nothing
        // left to apply, so the stale warning would hide later statuses.
        if case .failed = outcome {
            switch decision {
            case .nothingToApply, .notEligible: outcome = nil
            case .wait, .apply: break
            }
        }
        if decision != lastDecision {
            idleApplyLog.info("update apply-at-idle: \(String(describing: decision), privacy: .public)")
            lastDecision = decision
        }
        if case .apply(let action) = decision {
            inFlight = true
            outcome = nil
            perform(action) { [weak self] result in
                guard let self else { return }
                self.inFlight = false
                self.outcome = result
                switch result {
                case .performed:
                    self.lastAttemptAt = self.now()
                    if action == .restartEngine, !current.userRequested {
                        self.lastUnattendedEngineRestartKey = current.engineRestartKey
                    }
                case .deferred:
                    break
                case .failed:
                    self.lastAttemptAt = self.now()
                }
            }
        }
        return decision
    }

    /// The decision for `snapshot`. Also advances the "workers idle since" clock,
    /// which runs whether or not anything is staged, so an update that finishes
    /// downloading during a long idle stretch can apply straight away.
    func decide(_ snapshot: IdleApplySnapshot, at date: Date) -> IdleApplyDecision {
        if snapshot.engineReachable && snapshot.liveWorkerCount == 0 {
            if workersIdleSince == nil { workersIdleSince = date }
        } else {
            workersIdleSince = nil
        }

        let mayInstall = !snapshot.isDevBuild && (snapshot.userRequested || snapshot.mode == .automatic)
        guard mayInstall || snapshot.engineFingerprintMismatch else { return .notEligible }

        // Order matters: a staged release wins; then anything still being found or
        // downloaded (an engine-only restart now would consume the request and strand
        // the newer release); then the engine-only restart.
        let action: IdleApplyAction
        if let version = snapshot.stagedVersion, mayInstall {
            action = .relaunchIntoStagedUpdate(version)
        } else if snapshot.isPreparingUpdate && mayInstall {
            // A requested download is still in flight; restarting only the engine now
            // would consume the request and strand the newer release.
            return .wait(.preparingUpdate)
        } else if snapshot.engineBehindBundle || snapshot.engineFingerprintMismatch {
            // An unattended restart that left the engine version unchanged would
            // repeat forever; only an explicit request retries that pair.
            if !snapshot.userRequested,
               let key = snapshot.engineRestartKey,
               key == lastUnattendedEngineRestartKey
            {
                return .nothingToApply
            }
            action = .restartEngine
        } else {
            return .nothingToApply
        }

        if let lastAttemptAt, date.timeIntervalSince(lastAttemptAt) < policy.retryCooldownSeconds {
            return .wait(.recentAttempt)
        }
        guard snapshot.engineReachable else { return .wait(.engineUnreachable) }
        guard snapshot.liveWorkerCount == 0 else { return .wait(.liveWorkers(snapshot.liveWorkerCount)) }
        guard !snapshot.hasModalUI else { return .wait(.modalUI) }

        // Explicitly requested and nothing is live: apply now.
        if snapshot.userRequested { return .apply(action) }

        let idleFor = workersIdleSince.map { date.timeIntervalSince($0) } ?? 0
        guard idleFor >= policy.workerQuietSeconds else { return .wait(.workersRecentlyActive) }
        guard snapshot.secondsSinceUserInput >= policy.userIdleSeconds else { return .wait(.userActive) }
        return .apply(action)
    }
}

/// `true` when the running engine's stamped version is older than the app bundle's.
/// Unknown or unparseable engine versions are never treated as behind.
public func engineIsBehindBundle(engineVersion: String, bundleVersion: VersionTuple?) -> Bool {
    guard let bundleVersion, let engine = VersionTuple.parseReleaseBase(engineVersion) else { return false }
    return engine < bundleVersion
}

/// Status shown when an "Update & Restart" request ends with nothing to apply.
/// A failed download or install is named as such rather than reported as "no
/// newer build".
public func unfulfillableRequestMessage(downloadState: UpdateDownloadState) -> String {
    switch downloadState {
    case .failed(_, let reason):
        return "Update & Restart could not download the update: \(reason)"
    case .installFailed(_, let reason):
        return "Update & Restart could not install the update: \(reason)"
    default:
        return "Update & Restart found no newer installable build to apply."
    }
}
