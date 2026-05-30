import Foundation

/// Single source of truth for the on-disk locations the macOS app and
/// its in-process callers use to find a running Boss engine. Centralises
/// what used to be a handful of inline `/tmp/boss-engine.{sock,pid}`
/// literals scattered across `EngineProcessController`, `ChatViewModel`,
/// and miscellaneous test scaffolding.
///
/// The production accessors (`productionSocket`, `productionPID`,
/// `productionControlToken`) refuse to run from a test bundle. Tests
/// that need engine paths must construct a `BossEnginePaths.forTest(...)`
/// instance explicitly — the type system then prevents an XCTest from
/// accidentally compiling a call against the canonical paths and
/// SIGTERM'ing a 9-hour-old engine (issue #705).
///
/// `EngineProcessController.init` and `ChatViewModel.init` both take a
/// `BossEnginePaths` instance rather than reading env-fallback strings,
/// so every code path that resolves a production location goes through
/// these accessors and is subject to the test-context check.
struct BossEnginePaths {
    /// Path of the unix-domain frontend socket the engine binds.
    let socketPath: String

    /// Path of the engine's pid file. Read by the start lock and by
    /// the version-check helper; cleared on engine exit.
    let pidPath: String

    /// Path of the engine-control token file. Read by the shutdown
    /// RPC caller to authenticate against the running engine.
    let controlTokenPath: String

    /// Construct a paths instance explicitly. Public so tests can use
    /// `forTest(...)` (a thin wrapper); production code should call
    /// `BossEnginePaths.production()` instead so the test-context
    /// refusal is enforced.
    init(socketPath: String, pidPath: String, controlTokenPath: String) {
        self.socketPath = socketPath
        self.pidPath = pidPath
        self.controlTokenPath = controlTokenPath
    }

    // MARK: - Production accessors

    /// Build the production paths instance. Refuses to run from a
    /// test bundle so an accidentally-imported XCTest cannot end up
    /// here. Tests must use `forTest(...)` instead.
    ///
    /// Each field honours its established env override
    /// (`BOSS_SOCKET_PATH`, `BOSS_ENGINE_PID_PATH`,
    /// `BOSS_ENGINE_CONTROL_TOKEN_PATH`) so existing dev workflows
    /// (alternate sockets for parallel engines, test-instance profiles)
    /// keep working unchanged.
    static func production() -> BossEnginePaths {
        BossEnginePaths(
            socketPath: productionSocketPath(),
            pidPath: productionPIDPath(),
            controlTokenPath: productionControlTokenPath()
        )
    }

    /// Production socket path. Honours `BOSS_SOCKET_PATH` env override;
    /// otherwise resolves to the canonical `/tmp/boss-engine.sock`.
    /// Triggers a `fatalError` in test context — see `forTest(...)`.
    static func productionSocketPath() -> String {
        refuseFromTestContext("productionSocketPath()")
        return ProcessInfo.processInfo.environment["BOSS_SOCKET_PATH"]
            ?? "/tmp/boss-engine.sock"
    }

    /// Production pid-file path. Honours `BOSS_ENGINE_PID_PATH`;
    /// otherwise resolves to the canonical `/tmp/boss-engine.pid`.
    /// Triggers a `fatalError` in test context.
    static func productionPIDPath() -> String {
        refuseFromTestContext("productionPIDPath()")
        return ProcessInfo.processInfo.environment["BOSS_ENGINE_PID_PATH"]
            ?? "/tmp/boss-engine.pid"
    }

    /// Production control-token path. Honours
    /// `BOSS_ENGINE_CONTROL_TOKEN_PATH`; otherwise resolves to
    /// `~/Library/Application Support/Boss/engine-control.token`,
    /// matching the engine's `default_token_path` on the Rust side.
    /// Triggers a `fatalError` in test context.
    static func productionControlTokenPath() -> String {
        refuseFromTestContext("productionControlTokenPath()")
        if let override = ProcessInfo.processInfo.environment["BOSS_ENGINE_CONTROL_TOKEN_PATH"] {
            return override
        }
        let home = ProcessInfo.processInfo.environment["HOME"] ?? NSHomeDirectory()
        return "\(home)/Library/Application Support/Boss/engine-control.token"
    }

    // MARK: - Test construction

    /// Construct an instance with explicit per-test paths. Tests that
    /// need to exercise `EngineProcessController` or `ChatViewModel`
    /// must use this — there is no production-default fallback in test
    /// context, by design.
    static func forTest(
        socketPath: String,
        pidPath: String,
        controlTokenPath: String
    ) -> BossEnginePaths {
        BossEnginePaths(
            socketPath: socketPath,
            pidPath: pidPath,
            controlTokenPath: controlTokenPath
        )
    }

    // MARK: - Test-context detection

    /// `true` when the running process loaded `XCTest`. Mirrors the
    /// detection in `ReviewNotificationCenter.isBundleContextSafe`
    /// (`NSClassFromString("XCTestCase") != nil`). Centralised here so
    /// the engine-paths gate uses the same signal the rest of the app
    /// already trusts for "running inside a test bundle".
    static var isRunningInTestContext: Bool {
        NSClassFromString("XCTestCase") != nil
    }

    /// Fail loudly when a production accessor is called from a test
    /// bundle. The message names the accessor and points at the
    /// `forTest(...)` escape hatch so a developer triaging the crash
    /// has the exact fix in the failure text. Mirroring
    /// `ReviewNotificationCenter`'s pattern; see issue #705 for the
    /// rationale (a `bazel test` resolved a hard-coded production
    /// path and SIGTERM'd a live engine).
    private static func refuseFromTestContext(_ accessor: String) {
        if isRunningInTestContext {
            fatalError(
                """
                BossEnginePaths.\(accessor) was called from an XCTest \
                bundle. Production engine paths are unavailable in test \
                context — construct BossEnginePaths.forTest(...) \
                explicitly. See issue #705 for the design rationale.
                """
            )
        }
    }
}
