import AppKit
import Foundation
import UpdateCore
import os.log

private let lifecycleLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "updater")

/// Result of a user-initiated "Install & Relaunch" (`UpdateLifecycle.installStagedAndRelaunch`).
///
/// The swap that replaces `/Applications/Boss.app` is *durable and committed* in the
/// middle of the install, so a single boolean can't tell a caller whether the bundle
/// was actually touched. This distinguishes the three outcomes the UI must report
/// truthfully — critically, never claiming "the bundle could not be updated" once the
/// swap has in fact succeeded.
///
/// The applied version is intentionally not carried here: the swap targets the newest
/// ready staged version, which — in every reachable flow — is the version the calling
/// view is already displaying, so the caller reports that. (The full ``SwapPlan``,
/// including its version, is parked in ``pendingRelaunch`` for the terminate-time arm.)
enum InstallOutcome {
    /// Nothing changed: no staged update, the install location isn't writable, this is
    /// a dev build, or the swap failed and rolled back. The live bundle is intact, so
    /// the "app bundle could not be updated" message is accurate here.
    case notInstalled

    /// The swap was applied (or was already applied by a prior press) and a relaunch
    /// helper is available. The caller should request termination; the helper is armed
    /// only once the quit is actually confirmed (`applicationWillTerminate`), so a
    /// vetoed quit strands nothing. If `terminate` returns — meaning the quit was
    /// vetoed — the caller should reflect "update will complete on next quit".
    case relaunchPending

    /// The swap was applied but no relaunch helper is available (missing script), so
    /// Boss can't reopen itself automatically. The new version is live on disk and
    /// takes effect on the next launch; the caller should tell the user to quit Boss
    /// to finish — this is not a failure.
    case installedNoRelaunch
}

/// App-lifecycle glue for the self-updater's install/swap step (design doc §4, T7).
///
/// All of the *mechanics* — the bundle rename, `.bak` rollback, first-launch-OK
/// flag, blocklist, reconciliation — live in `UpdateCore.UpdateInstaller`, which is
/// pure and unit-tested. This enum is the thin, untested-by-design seam that wires
/// those into the app: it reads the user's update mode, detects dev builds, locates
/// the running bundle and the bundled `relaunch-helper.sh`, and performs the two
/// lifecycle side-effects the installer deliberately avoids — spawning a detached
/// `Process` and exiting the app.
///
/// Boundary cases handled here so callers stay simple:
/// - **Mode gating.** Auto-swap only runs in `automatic` mode (read straight from
///   the `@AppStorage` key `boss.update.mode`, matching `UpdateModel`'s storage).
/// - **Dev builds.** A `-dev-` `BossFullVersion` never auto-installs (design §
///   non-goals); reconciliation/flagging still runs so a dev build can complete a
///   swap that a release build staged.
/// - **Non-bundle launches.** `swift run` / bazel-run launches have no `.app`
///   install location and no bundled helper; discovery returns nothing and every
///   path no-ops.
///
/// `@MainActor` because it owns `pendingRelaunch` (mutated by the user-initiated
/// install path and consumed at terminate) and all entry points are already called
/// from the main actor (`AppDelegate`, `ChatViewModel`, SwiftUI button actions).
@MainActor
enum UpdateLifecycle {

    /// Matches `UpdateModel.StorageKeys.mode`.
    private static let modeKey = "boss.update.mode"
    /// Seconds the relaunch helper waits for the new version to report a clean launch
    /// before rolling back. Generous: a cold engine-coordinating launch is slow.
    private static let watchdogSeconds = 30

    /// A swap applied by a user-initiated "Install & Relaunch" whose relaunch helper
    /// has *not yet* been armed — arming is deferred to `applicationWillTerminate` so
    /// the detached helper is spawned only once the quit is actually confirmed. A
    /// vetoed quit therefore leaves nothing running; the plan simply waits here for the
    /// next confirmed quit. Consumed (cleared) by `consumePendingRelaunch()`.
    static var pendingRelaunch: SwapPlan?

    /// Returns and clears any `pendingRelaunch` plan. Called from
    /// `applicationWillTerminate` — a confirmed quit — so the relaunch helper is armed
    /// exactly once, at termination, and never on a vetoed quit.
    static func consumePendingRelaunch() -> SwapPlan? {
        defer { pendingRelaunch = nil }
        return pendingRelaunch
    }

    // MARK: Environment probes

    static var isAutomaticMode: Bool {
        (UserDefaults.standard.string(forKey: modeKey) ?? "notify") == UpdateMode.automatic.rawValue
    }

    /// A build whose `BossFullVersion` contains `-dev-` is local/unreleased and must
    /// never be auto-swapped over (design doc non-goals + failure table).
    static var isDevBuild: Bool {
        let full = Bundle.main.object(forInfoDictionaryKey: "BossFullVersion") as? String
            ?? Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? ""
        return full.contains("-dev-")
    }

    static var runningVersion: VersionTuple? {
        guard let short = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String else { return nil }
        return VersionTuple.parse(short)
    }

    static func installer() -> UpdateInstaller {
        UpdateInstaller.live(installBundleURL: Bundle.main.bundleURL)
    }

    private static func helperScriptURL() -> URL? {
        Bundle.main.url(forResource: "relaunch-helper", withExtension: "sh")
    }

    // MARK: Lifecycle entry points

    /// Run once from `applicationDidFinishLaunching`. Completes a pending swap (or
    /// records a helper-driven rollback) and writes this version's first-launch-OK
    /// flag, which the relaunch watchdog polls for. Always safe; runs regardless of
    /// mode or build type because a swap staged by a release build must still settle
    /// even if the user has since switched to manual mode.
    static func reconcileAtLaunch() {
        guard let running = runningVersion else { return }
        let installer = installer()
        switch installer.reconcileAfterLaunch(currentVersion: running) {
        case .installed(let version):
            NSLog("[update] now running freshly-installed version \(version)")
        case .rolledBack(let version):
            NSLog("[update] update \(version) failed to launch and was rolled back + blocklisted")
        case .noChange:
            break
        }
        // Always (re)assert the flag last so a relaunch watchdog from the swap that
        // brought us here sees a healthy first launch even on the `.noChange` path.
        installer.markLaunchSucceeded(version: running)
    }

    /// Run at the startup chokepoint, *before* the engine launches (design §4
    /// "swap-on-startup fallback"). If a staged update can be swapped in, applies it
    /// in-process, spawns the detached relaunch helper, and returns `true` so the
    /// caller stops and lets the process exit — the helper waits for us to die, then
    /// relaunches into the new bundle. Returns `false` (and changes nothing) in every
    /// other case, so the caller proceeds with a normal launch. Unlike the
    /// user-initiated path, this exits via `exit(0)` — an unconditional, un-vetoable
    /// termination — so the helper is armed inline here rather than deferred.
    @discardableResult
    static func applyStartupSwapIfNeeded() -> Bool {
        switch performSwap(relaunch: true, userInitiated: false) {
        case .notApplied:
            return false
        case .applied(let plan):
            // Arm now: startup is followed immediately by an un-vetoable `exit(0)`.
            return armRelaunchHelper(for: plan)
        }
    }

    /// Run from `applicationWillTerminate` (design §4 "swap-on-quit"). Applies a
    /// staged update in place without relaunching; the next launch runs the new
    /// version. Best-effort and non-blocking — a failed swap leaves the current
    /// bundle untouched and the staged version waits for the next boundary.
    static func applyQuitSwapIfNeeded() {
        _ = performSwap(relaunch: false, userInitiated: false)
    }

    /// Explicit, user-initiated install of the newest staged update — the result
    /// sheet / badge "Install & Relaunch" action. Unlike the automatic quit/startup
    /// paths this is **not** gated on automatic mode (the user asked for it directly),
    /// but it still refuses to swap over a dev build.
    ///
    /// Returns an ``InstallOutcome`` rather than a bare `Bool` so the caller can report
    /// the result truthfully: the swap durably replaces the live bundle mid-way, so a
    /// post-swap hiccup must never be reported as "the bundle could not be updated".
    ///
    /// The relaunch helper is **not** spawned here. On `.relaunchPending` the swap is
    /// applied and the plan is parked in ``pendingRelaunch``; the caller then requests
    /// termination and `applicationWillTerminate` arms the helper — so a vetoed quit
    /// (e.g. agents still working) never strands a detached helper. A repeated press
    /// after a vetoed quit is idempotent: the already-applied swap is recognised and
    /// reported as `.relaunchPending` again instead of failing on the consumed staged
    /// bundle.
    static func installStagedAndRelaunch() -> InstallOutcome {
        switch performSwap(relaunch: true, userInitiated: true) {
        case .notApplied:
            return .notInstalled
        case .applied(let plan):
            guard helperScriptURL() != nil else {
                // Swap is done, but with no helper we can't reopen ourselves. The new
                // version is live on disk and runs on the next launch — tell the user
                // to quit to finish; this is not a failure.
                lifecycleLog.error(
                    "update: swapped in \(plan.version, privacy: .public) but relaunch-helper.sh is missing; will run on next launch")
                return .installedNoRelaunch
            }
            // Park the plan; the helper is armed at `applicationWillTerminate`, i.e.
            // only once the quit is actually confirmed.
            pendingRelaunch = plan
            lifecycleLog.info(
                "update: swapped in \(plan.version, privacy: .public); relaunch armed for confirmed quit")
            return .relaunchPending
        }
    }

    // MARK: - Implementation

    /// Outcome of the shared swap core: whether the live bundle was actually replaced.
    private enum SwapResult {
        /// Nothing changed — the live bundle is intact (pre-swap gate failed, nothing
        /// staged, not writable, blocked, or the swap threw and rolled back).
        case notApplied
        /// The swap is committed (or was already committed by a prior press): the live
        /// bundle now holds `plan.version`. Helper arming is the caller's decision.
        case applied(SwapPlan)
    }

    /// Apply the newest applicable staged swap, if any. Performs the durable bundle
    /// replacement but never spawns the relaunch helper — that lifecycle side-effect is
    /// the caller's, so it can be timed to a confirmed quit. Idempotent for the
    /// user-initiated path: an already-applied-but-unreconciled swap is reported as
    /// `.applied` rather than re-attempted against the (already consumed) staged bundle.
    private static func performSwap(relaunch: Bool, userInitiated: Bool) -> SwapResult {
        guard userInitiated || isAutomaticMode else {
            lifecycleLog.debug("update swap skipped: not automatic mode and not user-initiated (relaunch=\(relaunch, privacy: .public))")
            return .notApplied
        }
        guard !isDevBuild else {
            lifecycleLog.debug("update swap skipped: dev build")
            return .notApplied
        }
        guard let running = runningVersion else {
            lifecycleLog.error("update swap skipped: could not determine running version from bundle")
            return .notApplied
        }

        let installer = installer()

        var ready = installer.newestReadyUpdate(currentVersion: running)
        if ready == nil && userInitiated {
            // Idempotency: a prior press may have already applied the swap and consumed
            // the staged bundle, so `newestReadyUpdate` now finds nothing. If the live
            // bundle already matches an unreconciled pending swap, the install really
            // succeeded and is only awaiting relaunch — report that instead of failing.
            if let applied = installer.appliedPendingAwaitingRelaunch(currentVersion: running) {
                lifecycleLog.info(
                    "update install: swap for \(applied.version, privacy: .public) already applied on a prior press; awaiting relaunch (idempotent)")
                return .applied(applied)
            }
            // Otherwise, also consider versions that were previously blocklisted (failed
            // a prior watchdog launch). The background automatic swap skips them
            // permanently, but an explicit "Install & Relaunch" is an intentional retry.
            // The watchdog will roll back again if the new version still can't launch.
            if let candidate = installer.newestReadyUpdateIgnoringBlocklist(currentVersion: running) {
                lifecycleLog.info(
                    "update install: user-initiated install of previously-blocked version \(candidate.version, privacy: .public); clearing block")
                installer.unblockVersion(candidate.version)
                ready = candidate
            }
        }

        guard let ready else {
            lifecycleLog.info(
                "update swap skipped: no staged update found in \(installer.updatesDirectoryPath, privacy: .sensitive) newer than running \(running, privacy: .public) (relaunch=\(relaunch, privacy: .public) userInitiated=\(userInitiated, privacy: .public))")
            return .notApplied
        }

        switch installer.planSwap(for: ready, currentVersion: running, relaunch: relaunch) {
        case .swap(let plan):
            do {
                try installer.applySwap(plan)
            } catch {
                // `applySwap` rolls back before throwing, so the live bundle is intact.
                lifecycleLog.error("update swap failed (relaunch=\(relaunch, privacy: .public)): \(error, privacy: .public)")
                return .notApplied
            }
            if !relaunch {
                lifecycleLog.info("update: swapped in \(plan.version, privacy: .public) on quit; will run on next launch")
            }
            return .applied(plan)

        case .notWritable(let installURL, let stagedURL):
            // /Applications-without-write: degrade gracefully (design §4). The UI
            // surfaces (T3/T4) reveal the staged bundle in Finder; here we only log.
            lifecycleLog.warning(
                "update swap skipped: \(ready.version, privacy: .public) is staged but \(installURL.path, privacy: .sensitive) is not writable; staged at \(stagedURL.path, privacy: .sensitive)")
            return .notApplied

        case .blocked(let version):
            // Should only be reached if the plan's version was blocklisted between the
            // `ready` discovery above and the `planSwap` call (extremely unlikely).
            lifecycleLog.warning("update swap skipped: \(version, privacy: .public) is blocklisted (failed a prior launch)")
            return .notApplied

        case .upToDate:
            return .notApplied
        }
    }

    /// Spawn the detached relaunch helper for an already-applied `plan`: it waits for
    /// this PID to exit, reopens the (already-swapped) new bundle, and watchdogs its
    /// first launch. Returns `true` when the helper was spawned, `false` if the helper
    /// script is missing or the process failed to launch (in which case the swap still
    /// stands and the new version runs on the next manual launch).
    @discardableResult
    static func armRelaunchHelper(for plan: SwapPlan) -> Bool {
        guard let script = helperScriptURL() else {
            lifecycleLog.error(
                "update: swapped in \(plan.version, privacy: .public) but relaunch-helper.sh is missing; will run on next launch")
            return false
        }
        let invocation = installer().helperInvocation(
            scriptURL: script, plan: plan, bossPID: getpid(), watchdogSeconds: watchdogSeconds)
        let proc = Process()
        proc.executableURL = invocation.executableURL
        proc.arguments = invocation.arguments
        do {
            try proc.run()
        } catch {
            lifecycleLog.error("update: failed to spawn relaunch helper: \(error, privacy: .public)")
            return false
        }
        lifecycleLog.info("update: relaunch helper armed for \(plan.version, privacy: .public) (pid=\(getpid(), privacy: .public))")
        return true
    }
}
