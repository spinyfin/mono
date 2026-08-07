import AppKit
import CoreGraphics
import CoreVideo
import Foundation

/// Whether this host can currently create a libghostty worker-pane surface,
/// and — when it cannot — the **measured** reason why.
///
/// ## Why this exists
///
/// `ghostty_surface_new` returns NULL with no error code, so before this
/// type every caller could do was *assert* a cause. The NACK string shipped
/// to the engine was a hardcoded "likely no active display after
/// sleep/wake", which sent the 2026-07-30 investigation toward sleep/wake
/// when the operator's machine had simply locked its screen — the lid was
/// never closed and the system never slept. A diagnostic that guesses is
/// worse than one that measures: it misdirects the next reader too.
///
/// ## The actual constraint (measured, not inferred)
///
/// libghostty's renderer drives its frame clock from a `CVDisplayLink`
/// built over the active display set. CoreVideo refuses to build one from
/// an empty set:
///
/// ```text
/// CVDisplayLinkCreateWithCGDisplays(count: 0)  -> -6661 kCVReturnInvalidArgument, link NULL
/// CVDisplayLinkCreateWithCGDisplays(bogus id)  -> -6670 kCVReturnInvalidDisplay,  link NULL
/// CVDisplayLinkCreateWithCGDisplays(main)      ->     0 kCVReturnSuccess,         link non-null
/// ```
///
/// So an active-display count of 0 makes surface creation **impossible**,
/// not merely unlikely — and it is impossible identically for every work
/// item, which is what makes it an environmental condition rather than a
/// failure of the work being dispatched.
///
/// ## What this deliberately does NOT claim
///
/// It does not claim to know *which* host state drove the count to 0 (lid
/// closed, display asleep, screen locked long enough for the display to
/// sleep, all monitors disconnected). It records the state it can observe —
/// counts, asleep flags, lock/console flags — and lets the reader see it.
/// [[SpawnCapabilityDiagnosticsTests]] pins the classification; the
/// snapshot fields are what a future incident greps.
struct HostDisplaySnapshot: Codable, Equatable, Sendable {
    /// Displays CoreGraphics considers active — the set
    /// `CVDisplayLinkCreateWithActiveCGDisplays` would build over, and
    /// therefore the only field the verdict keys on.
    var activeDisplayCount: Int
    /// Displays that are connected at all. A non-zero `online` with a zero
    /// `active` is the signature of "a monitor is attached but not drawable"
    /// (asleep / mirrored off), as opposed to "nothing is attached".
    var onlineDisplayCount: Int
    /// `CGDisplayIsAsleep(CGMainDisplayID())`.
    var mainDisplayAsleep: Bool
    /// `CGSSessionScreenIsLocked` — true while the login session is showing
    /// the lock screen. Recorded to settle the lock-versus-display-sleep
    /// question the next time this fires, rather than guessing again.
    var sessionLocked: Bool
    /// `kCGSSessionOnConsoleKey` — false when fast-user-switched away.
    var sessionOnConsole: Bool
    /// AppKit's view of the same thing. Kept alongside the CG counts because
    /// they can legitimately disagree, and a disagreement is itself a clue.
    var screenCount: Int

    private enum CodingKeys: String, CodingKey {
        case activeDisplayCount = "active_display_count"
        case onlineDisplayCount = "online_display_count"
        case mainDisplayAsleep = "main_display_asleep"
        case sessionLocked = "session_locked"
        case sessionOnConsole = "session_on_console"
        case screenCount = "screen_count"
    }

    /// One-line human summary for a NACK reason / log line. Names the
    /// measured facts only — never a supposition about lid or sleep.
    var summary: String {
        "active_displays=\(activeDisplayCount) online_displays=\(onlineDisplayCount) "
            + "main_display_asleep=\(mainDisplayAsleep) session_locked=\(sessionLocked) "
            + "session_on_console=\(sessionOnConsole) ns_screens=\(screenCount)"
    }
}

enum SpawnCapability {
    /// Verdict for a host snapshot.
    enum Verdict: Equatable {
        /// Surface creation has an active display to bind a display link to.
        /// (It may still fail for an unrelated reason — this is a necessary
        /// precondition, not a guarantee.)
        case canHostPane
        /// Surface creation cannot succeed until the host changes. `reason`
        /// is operator-facing: it names the measured condition AND what
        /// clears it, because it is rendered verbatim in the app's dispatch
        /// -paused banner.
        case environmentUnavailable(reason: String)

        var isBlocked: Bool {
            if case .environmentUnavailable = self { return true }
            return false
        }
    }

    /// Pure classification — the whole decision, unit-testable without a
    /// display. Keyed solely on `activeDisplayCount` because that is the
    /// input to the `CVDisplayLink` creation measured above; every other
    /// field on the snapshot is recorded evidence, not a predicate.
    static func verdict(for snapshot: HostDisplaySnapshot) -> Verdict {
        guard snapshot.activeDisplayCount == 0 else { return .canHostPane }
        return .environmentUnavailable(
            reason: "no active display, so libghostty cannot create a worker-pane surface "
                + "(CoreVideo rejects a display link over an empty display set); "
                + "measured \(snapshot.summary). Clears automatically when a display "
                + "becomes active again — wake or reconnect a display, or unlock the screen."
        )
    }

    /// Read the live host state. Cheap (four CoreGraphics queries and an
    /// AppKit array read); safe to call on every spawn.
    static func snapshot() -> HostDisplaySnapshot {
        let session = CGSessionCopyCurrentDictionary() as? [String: Any] ?? [:]
        return HostDisplaySnapshot(
            activeDisplayCount: displayCount(CGGetActiveDisplayList),
            onlineDisplayCount: displayCount(CGGetOnlineDisplayList),
            mainDisplayAsleep: CGDisplayIsAsleep(CGMainDisplayID()) != 0,
            sessionLocked: session["CGSSessionScreenIsLocked"] as? Bool ?? false,
            sessionOnConsole: session[kCGSessionOnConsoleKey as String] as? Bool ?? true,
            screenCount: NSScreen.screens.count
        )
    }

    /// Live verdict — `verdict(for: snapshot())`.
    static func current() -> Verdict {
        verdict(for: snapshot())
    }

    /// Shared shape of `CGGetActiveDisplayList` / `CGGetOnlineDisplayList`:
    /// call once with a nil buffer to size, and trust the count. A CG error
    /// reports 0, which routes to the same "cannot host" verdict a genuine
    /// zero would — the conservative direction, since a host whose display
    /// list cannot even be read is not one to dispatch a pane into.
    private static func displayCount(
        _ query: (UInt32, UnsafeMutablePointer<CGDirectDisplayID>?, UnsafeMutablePointer<UInt32>?) -> CGError
    ) -> Int {
        var count: UInt32 = 0
        guard query(0, nil, &count) == .success else { return 0 }
        return Int(count)
    }
}
