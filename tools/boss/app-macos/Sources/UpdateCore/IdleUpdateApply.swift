import Foundation
import os.log

private let idleApplyLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "updater")

// Applying a published release without waiting for the operator to quit.
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
// - Never over a dev build.
// - Unattended (automatic mode) applies also wait for a quiet period with no live
//   workers and for the operator to be away from the keyboard.

/// Everything the apply-at-idle decision reads, captured at one instant.
public struct IdleApplySnapshot: Equatable, Sendable {
    public var mode: UpdateMode
    public var isDevBuild: Bool
    /// The operator pressed "Update & Restart".
    public var userRequested: Bool
    /// A requested update is still being found or downloaded.
    public var isPreparingUpdate: Bool
    /// A verified update staged on disk (or already swapped in, awaiting relaunch).
    public var stagedVersion: VersionTuple?
    /// The running engine is older than the engine inside this app bundle.
    public var engineBehindBundle: Bool
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
        self.engineReachable = engineReachable
        self.liveWorkerCount = liveWorkerCount
        self.hasModalUI = hasModalUI
        self.secondsSinceUserInput = secondsSinceUserInput
    }
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
    private let perform: (IdleApplyAction) -> Void
    private let now: () -> Date

    private var workersIdleSince: Date?
    private var lastAttemptAt: Date?
    public private(set) var lastDecision: IdleApplyDecision = .notEligible

    public init(
        policy: Policy = Policy(),
        snapshot: @escaping () -> IdleApplySnapshot,
        perform: @escaping (IdleApplyAction) -> Void,
        now: @escaping () -> Date = { Date() }
    ) {
        self.policy = policy
        self.snapshot = snapshot
        self.perform = perform
        self.now = now
    }

    /// A fresh operator request overrides the retry cooldown.
    public func noteUserRequest() {
        lastAttemptAt = nil
    }

    /// Evaluate once and, if the decision is `.apply`, perform it.
    @discardableResult
    public func tick() -> IdleApplyDecision {
        let decision = decide(snapshot(), at: now())
        if decision != lastDecision {
            idleApplyLog.info("update apply-at-idle: \(String(describing: decision), privacy: .public)")
            lastDecision = decision
        }
        if case .apply(let action) = decision {
            lastAttemptAt = now()
            perform(action)
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

        guard snapshot.userRequested || snapshot.mode == .automatic else { return .notEligible }
        // Dev builds are reported as behind elsewhere; they are never installed over.
        guard !snapshot.isDevBuild else { return .notEligible }

        let action: IdleApplyAction
        if let version = snapshot.stagedVersion {
            action = .relaunchIntoStagedUpdate(version)
        } else if snapshot.engineBehindBundle {
            action = .restartEngine
        } else {
            return snapshot.isPreparingUpdate ? .wait(.preparingUpdate) : .nothingToApply
        }

        if let lastAttemptAt, date.timeIntervalSince(lastAttemptAt) < policy.retryCooldownSeconds {
            return .wait(.recentAttempt)
        }
        guard snapshot.engineReachable else { return .wait(.engineUnreachable) }
        guard snapshot.liveWorkerCount == 0 else { return .wait(.liveWorkers(snapshot.liveWorkerCount)) }
        guard !snapshot.hasModalUI else { return .wait(.modalUI) }

        // The operator asked for it and nothing is live: apply now.
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
