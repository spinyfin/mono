import AppKit
import CoreGraphics
import Foundation

/// Measured host display / session state at a point in time.
///
/// ## Why this exists
///
/// `ghostty_surface_new` returns NULL with no error code. `NSScreen.main`
/// must not be used to diagnose these failures: it stays non-nil while the
/// login session is locked or the display has idled off, so AppKit reports
/// a screen when CoreGraphics reports zero active displays. That produced
/// the contradictory NACK:
///
/// > a display IS active, so display availability is not the cause
///
/// on machines whose screen was simply locked (confirmed 2026-08-10
/// episode). This type records the CG/session facts that actually matter
/// for libghostty's renderer, so the next failure is diagnosable from
/// `bossctl logs spawn` without hunting the app's stderr.
///
/// Fields other than `activeDisplayCount` are evidence (lock, asleep,
/// online, and AppKit screen count), not a separate gate.
struct HostDisplaySnapshot: Codable, Equatable, Sendable {
    /// Displays CoreGraphics considers active — the set
    /// `CVDisplayLinkCreateWithActiveCGDisplays` builds over.
    var activeDisplayCount: Int
    /// Connected displays (may be non-zero while active is zero — asleep).
    var onlineDisplayCount: Int
    /// `CGDisplayIsAsleep(CGMainDisplayID())`.
    var mainDisplayAsleep: Bool
    /// `CGSSessionScreenIsLocked` — login session showing the lock screen.
    var sessionLocked: Bool
    /// `kCGSSessionOnConsoleKey` — false when fast-user-switched away.
    var sessionOnConsole: Bool
    /// AppKit `NSScreen.screens.count` (can disagree with CG; disagreement is a clue).
    var screenCount: Int
    /// Whether AppKit still has a `NSScreen.main` — recorded as evidence only;
    /// it can be true while activeDisplayCount is 0, so it must not drive attribution.
    var nsScreenMainNonNil: Bool
    private enum CodingKeys: String, CodingKey {
        case activeDisplayCount = "active_display_count"
        case onlineDisplayCount = "online_display_count"
        case mainDisplayAsleep = "main_display_asleep"
        case sessionLocked = "session_locked"
        case sessionOnConsole = "session_on_console"
        case screenCount = "screen_count"
        case nsScreenMainNonNil = "ns_screen_main_non_nil"
    }

    /// One-line human summary for NACK reasons and log lines.
    var summary: String {
        "active_displays=\(activeDisplayCount) online_displays=\(onlineDisplayCount) "
            + "main_display_asleep=\(mainDisplayAsleep) session_locked=\(sessionLocked) "
            + "session_on_console=\(sessionOnConsole) ns_screens=\(screenCount) "
            + "ns_screen_main_non_nil=\(nsScreenMainNonNil)"
    }

    /// Dictionary form for JSONL `surface_failed` extras (snake_case keys).
    var jsonObject: [String: Any] {
        [
            "active_display_count": activeDisplayCount,
            "online_display_count": onlineDisplayCount,
            "main_display_asleep": mainDisplayAsleep,
            "session_locked": sessionLocked,
            "session_on_console": sessionOnConsole,
            "screen_count": screenCount,
            "ns_screen_main_non_nil": nsScreenMainNonNil,
        ]
    }

    /// Read the live host state. Cheap (CG list sizing + session dict + AppKit).
    @MainActor
    static func capture() -> HostDisplaySnapshot {
        let session = CGSessionCopyCurrentDictionary() as? [String: Any] ?? [:]
        return HostDisplaySnapshot(
            activeDisplayCount: displayCount(CGGetActiveDisplayList),
            onlineDisplayCount: displayCount(CGGetOnlineDisplayList),
            mainDisplayAsleep: CGDisplayIsAsleep(CGMainDisplayID()) != 0,
            sessionLocked: session["CGSSessionScreenIsLocked"] as? Bool ?? false,
            sessionOnConsole: session[kCGSessionOnConsoleKey as String] as? Bool ?? true,
            screenCount: NSScreen.screens.count,
            nsScreenMainNonNil: NSScreen.main != nil
        )
    }

    /// Pure helper for tests — builds a snapshot without touching the system.
    static func make(
        activeDisplayCount: Int,
        onlineDisplayCount: Int = 0,
        mainDisplayAsleep: Bool = false,
        sessionLocked: Bool = false,
        sessionOnConsole: Bool = true,
        screenCount: Int = 0,
        nsScreenMainNonNil: Bool = false
    ) -> HostDisplaySnapshot {
        HostDisplaySnapshot(
            activeDisplayCount: activeDisplayCount,
            onlineDisplayCount: onlineDisplayCount,
            mainDisplayAsleep: mainDisplayAsleep,
            sessionLocked: sessionLocked,
            sessionOnConsole: sessionOnConsole,
            screenCount: screenCount,
            nsScreenMainNonNil: nsScreenMainNonNil
        )
    }

    private static func displayCount(
        _ query: (UInt32, UnsafeMutablePointer<CGDirectDisplayID>?, UnsafeMutablePointer<UInt32>?) -> CGError
    ) -> Int {
        var count: UInt32 = 0
        guard query(0, nil, &count) == .success else { return 0 }
        return Int(count)
    }
}
