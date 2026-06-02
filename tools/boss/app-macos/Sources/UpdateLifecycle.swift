import AppKit
import Foundation
import UpdateCore
import os.log

private let lifecycleLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "updater")

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
enum UpdateLifecycle {

    /// Matches `UpdateModel.StorageKeys.mode`.
    private static let modeKey = "boss.update.mode"
    /// Seconds the relaunch helper waits for the new version to report a clean launch
    /// before rolling back. Generous: a cold engine-coordinating launch is slow.
    private static let watchdogSeconds = 30

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
    /// other case, so the caller proceeds with a normal launch.
    @discardableResult
    static func applyStartupSwapIfNeeded() -> Bool {
        applySwapIfNeeded(relaunch: true)
    }

    /// Run from `applicationWillTerminate` (design §4 "swap-on-quit"). Applies a
    /// staged update in place without relaunching; the next launch runs the new
    /// version. Best-effort and non-blocking — a failed swap leaves the current
    /// bundle untouched and the staged version waits for the next boundary.
    static func applyQuitSwapIfNeeded() {
        _ = applySwapIfNeeded(relaunch: false)
    }

    /// Explicit, user-initiated install of the newest staged update — the result
    /// sheet / badge "Install & Relaunch" action. Unlike the automatic quit/startup
    /// paths this is **not** gated on automatic mode (the user asked for it directly),
    /// but it still refuses to swap over a dev build. On success the swap is applied,
    /// the detached relaunch helper is spawned, and `true` is returned so the caller
    /// terminates the app and lets the helper relaunch into the new version. Returns
    /// `false` (changing nothing) if there is nothing staged, the install location is
    /// not writable, or the swap fails — the caller should fall back to a manual path.
    @discardableResult
    static func installStagedAndRelaunch() -> Bool {
        applySwapIfNeeded(relaunch: true, userInitiated: true)
    }

    // MARK: - Implementation

    private static func applySwapIfNeeded(relaunch: Bool, userInitiated: Bool = false) -> Bool {
        guard userInitiated || isAutomaticMode else {
            lifecycleLog.debug("update swap skipped: not automatic mode and not user-initiated (relaunch=\(relaunch, privacy: .public))")
            return false
        }
        guard !isDevBuild else {
            lifecycleLog.debug("update swap skipped: dev build")
            return false
        }
        guard let running = runningVersion else {
            lifecycleLog.error("update swap skipped: could not determine running version from bundle")
            return false
        }

        let installer = installer()

        // For a user-initiated install, also consider versions that were previously
        // blocklisted (failed a prior watchdog launch). The background automatic swap
        // skips them permanently, but when the user explicitly requests "Install &
        // Relaunch" they are intentionally retrying. The watchdog will roll back again
        // if the new version still cannot launch cleanly.
        var ready = installer.newestReadyUpdate(currentVersion: running)
        if ready == nil && userInitiated {
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
            return false
        }

        switch installer.planSwap(for: ready, currentVersion: running, relaunch: relaunch) {
        case .swap(let plan):
            do {
                try installer.applySwap(plan)
            } catch {
                lifecycleLog.error("update swap failed (relaunch=\(relaunch, privacy: .public)): \(error, privacy: .public)")
                return false
            }

            guard relaunch else {
                lifecycleLog.info("update: swapped in \(plan.version, privacy: .public) on quit; will run on next launch")
                return false
            }

            // Relaunch path: hand off to the detached helper, then exit.
            guard let script = helperScriptURL() else {
                // Swap already applied; without the helper we can't relaunch, but the
                // next manual launch will run the new version. Don't exit.
                lifecycleLog.error("update: swapped in \(plan.version, privacy: .public) but relaunch-helper.sh is missing from the bundle")
                return false
            }
            let invocation = installer.helperInvocation(
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
            lifecycleLog.info("update: swapped in \(plan.version, privacy: .public); relaunching via helper (pid=\(getpid(), privacy: .public))")
            return true

        case .notWritable(let installURL, let stagedURL):
            // /Applications-without-write: degrade gracefully (design §4). The UI
            // surfaces (T3/T4) reveal the staged bundle in Finder; here we only log.
            lifecycleLog.warning(
                "update swap skipped: \(ready.version, privacy: .public) is staged but \(installURL.path, privacy: .sensitive) is not writable; staged at \(stagedURL.path, privacy: .sensitive)")
            return false

        case .blocked(let version):
            // Should only be reached if the plan's version was blocklisted between the
            // `ready` discovery above and the `planSwap` call (extremely unlikely).
            lifecycleLog.warning("update swap skipped: \(version, privacy: .public) is blocklisted (failed a prior launch)")
            return false

        case .upToDate:
            return false
        }
    }
}
