import Foundation

/// Isolation-aware `UserDefaults` for the macOS app.
///
/// Production uses `UserDefaults.standard` (bundle id
/// `dev.spinyfin.bossmacapp`). An isolated capture instance
/// (`BossEnginePaths.isIsolatedInstance`) uses a dedicated suite so
/// window frames, filters, and panel widths do not bleed into the
/// operator's live app.
///
/// `@AppStorage` call sites pass `store: BossDefaults.store`. Direct
/// `UserDefaults` readers/writers use the same accessor.
enum BossDefaults {
    /// Suite name for isolated / agent-capture instances. Same bundle id
    /// as production (TCC identity stays put); only the defaults domain
    /// is split.
    static let captureSuiteName = "dev.spinyfin.bossmacapp.capture"

    /// Defaults store for the current process. Resolved once from the
    /// isolation signal so every call site shares the same suite.
    ///
    /// `nonisolated(unsafe)`: `UserDefaults` is not `Sendable`, but the
    /// process-wide suite is fixed at first access from the isolation
    /// signal (env var, immutable for the process lifetime) and is the
    /// same pattern AppKit itself uses for `.standard`.
    nonisolated(unsafe) static let store: UserDefaults = {
        if BossEnginePaths.isIsolatedInstance {
            return UserDefaults(suiteName: captureSuiteName) ?? .standard
        }
        return .standard
    }()
}
