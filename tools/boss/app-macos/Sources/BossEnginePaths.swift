import Darwin
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

    /// Path of the engine's pid file. Used as an optimisation for process
    /// identification and signalling; socket reachability is the liveness
    /// oracle.
    let pidPath: String

    /// Path of the engine-control token file. Read by the shutdown
    /// RPC caller to authenticate against the running engine.
    let controlTokenPath: String

    /// Legacy runtime locations retained for one migration window. These are
    /// nil for explicit/test profiles and present only for the normal
    /// production profile.
    let legacySocketPath: String?
    let legacyPIDPath: String?

    /// Construct a paths instance explicitly. Public so tests can use
    /// `forTest(...)` (a thin wrapper); production code should call
    /// `BossEnginePaths.production()` instead so the test-context
    /// refusal is enforced.
    init(
        socketPath: String,
        pidPath: String,
        controlTokenPath: String,
        legacySocketPath: String? = nil,
        legacyPIDPath: String? = nil
    ) {
        self.socketPath = socketPath
        self.pidPath = pidPath
        self.controlTokenPath = controlTokenPath
        self.legacySocketPath = legacySocketPath
        self.legacyPIDPath = legacyPIDPath
    }

    var socketPaths: [String] {
        [socketPath] + (legacySocketPath.map { [$0] } ?? [])
    }

    var endpointPaths: [(socketPath: String, pidPath: String)] {
        var endpoints = [(socketPath, pidPath)]
        if let legacySocketPath, let legacyPIDPath {
            endpoints.append((legacySocketPath, legacyPIDPath))
        }
        return endpoints
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
        let socketOverride = ProcessInfo.processInfo.environment["BOSS_SOCKET_PATH"]
        let pidOverride = ProcessInfo.processInfo.environment["BOSS_ENGINE_PID_PATH"]
        let tokenOverride = ProcessInfo.processInfo.environment["BOSS_ENGINE_CONTROL_TOKEN_PATH"]
        let socketPath = socketOverride ?? productionSocketPath()
        return BossEnginePaths(
            socketPath: socketPath,
            pidPath: pidOverride ?? socketOverride.map { derivedSiblingPath(socketPath: $0, suffix: "pid") }
                ?? productionPIDPath(),
            controlTokenPath: tokenOverride
                ?? socketOverride.map { derivedSiblingPath(socketPath: $0, suffix: "control-token") }
                ?? productionControlTokenPath(),
            legacySocketPath: socketOverride == nil ? legacyProductionSocket : nil,
            legacyPIDPath: socketOverride == nil ? legacyProductionPID : nil
        )
    }

    private static func derivedSiblingPath(socketPath: String, suffix: String) -> String {
        let url = URL(fileURLWithPath: socketPath)
        let stem = url.deletingPathExtension().lastPathComponent
        return url.deletingLastPathComponent().appendingPathComponent("\(stem).\(suffix)").path
    }

    /// Legacy production paths retained for discovery during migration.
    static let legacyProductionSocket = "/tmp/boss-engine.sock"
    static let legacyProductionPID = "/tmp/boss-engine.pid"

    /// Compatibility name used by capture validation during the legacy
    /// migration window.
    static let defaultProductionSocket = legacyProductionSocket

    /// Production socket path. Honours `BOSS_SOCKET_PATH` env override;
    /// otherwise resolves under the Boss state root.
    /// Triggers a `fatalError` in test context — see `forTest(...)`.
    static func productionSocketPath() -> String {
        refuseFromTestContext("productionSocketPath()")
        if let override = ProcessInfo.processInfo.environment["BOSS_SOCKET_PATH"] {
            return override
        }
        return URL(fileURLWithPath: productionStateRoot())
            .appendingPathComponent("engine.sock").path
    }

    /// `true` when `BOSS_SOCKET_PATH` points at a non-production socket.
    ///
    /// One signal drives three capture-instance behaviours: the toolbar
    /// badge, quiet (`.accessory` + no-activate) launch, and the
    /// per-instance `UserDefaults` suite. Mirrors engine-side
    /// `is_test_fixture_socket` — no new env var.
    ///
    /// Safe to call from tests (reads the process environment only; does
    /// not go through `productionSocketPath()`'s test-context refusal).
    static var isIsolatedInstance: Bool {
        guard let sock = ProcessInfo.processInfo.environment["BOSS_SOCKET_PATH"],
              !sock.isEmpty
        else {
            return false
        }
        let normalized = URL(fileURLWithPath: sock).standardizedFileURL.path
        let legacy = URL(fileURLWithPath: legacyProductionSocket).standardizedFileURL.path
        if normalized == legacy {
            return false
        }
        // Production-shaped Application Support paths are not isolation.
        if sock.contains("Library/Application Support/Boss") {
            return false
        }
        return true
    }

    /// Production pid-file path. Honours `BOSS_ENGINE_PID_PATH`;
    /// otherwise resolves under the Boss state root.
    /// Triggers a `fatalError` in test context.
    static func productionPIDPath() -> String {
        refuseFromTestContext("productionPIDPath()")
        if let override = ProcessInfo.processInfo.environment["BOSS_ENGINE_PID_PATH"] {
            return override
        }
        return URL(fileURLWithPath: productionStateRoot())
            .appendingPathComponent("engine.pid").path
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

    private static func productionStateRoot() -> String {
        let home = ProcessInfo.processInfo.environment["HOME"] ?? NSHomeDirectory()
        return URL(fileURLWithPath: home, isDirectory: true)
            .appendingPathComponent("Library", isDirectory: true)
            .appendingPathComponent("Application Support", isDirectory: true)
            .appendingPathComponent("Boss", isDirectory: true)
            .path
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
protocol EngineSocketControlling: Sendable {
    func isReachable(socketPath: String, timeoutSeconds: Double) -> Bool
    func peerPID(socketPath: String, timeoutSeconds: Double) -> pid_t?
    func fingerprint(socketPath: String, timeoutSeconds: Double) -> String?
    func shutdown(socketPath: String, tokenPath: String, timeoutSeconds: Double) throws -> pid_t?
    func waitForClose(socketPath: String, timeoutSeconds: Double) -> Bool
}

extension EngineSocketControlling {
    func isReachable(socketPath: String) -> Bool {
        isReachable(socketPath: socketPath, timeoutSeconds: 1)
    }
}

struct EngineSocketControl: EngineSocketControlling {
    struct ShutdownCredential {
        let token: String
        let socketPath: String
        let pid: pid_t?
    }

    func isReachable(socketPath: String, timeoutSeconds: Double = 1.0) -> Bool {
        guard let socket = connect(socketPath: socketPath, timeoutSeconds: timeoutSeconds) else {
            return false
        }
        Darwin.close(socket)
        return true
    }

    func peerPID(socketPath: String, timeoutSeconds: Double = 1.0) -> pid_t? {
        guard let socket = connect(socketPath: socketPath, timeoutSeconds: timeoutSeconds) else {
            return nil
        }
        defer { Darwin.close(socket) }

        var pid: pid_t = 0
        var length = socklen_t(MemoryLayout<pid_t>.size)
        guard getsockopt(socket, SOL_LOCAL, LOCAL_PEERPID, &pid, &length) == 0,
              pid > 1
        else {
            return nil
        }
        return pid
    }

    func fingerprint(socketPath: String, timeoutSeconds: Double) -> String? {
        guard let payload = request(
            socketPath: socketPath,
            requestID: "version-check",
            payload: ["type": "get_engine_version"],
            timeoutSeconds: timeoutSeconds
        ), payload["type"] as? String == "engine_version_result"
        else {
            return nil
        }
        return payload["binary_fingerprint"] as? String
    }

    func readShutdownCredential(tokenPath: String) throws -> ShutdownCredential {
        let raw = try Data(contentsOf: URL(fileURLWithPath: tokenPath))
        guard let json = try JSONSerialization.jsonObject(with: raw) as? [String: Any],
              let token = json["token"] as? String,
              let socketPath = json["socket_path"] as? String
        else {
            throw failure("malformed engine-control token file: \(tokenPath)")
        }
        let pid = (json["pid"] as? NSNumber).map { pid_t($0.int32Value) }
        return ShutdownCredential(token: token, socketPath: socketPath, pid: pid)
    }

    func shutdown(socketPath: String, tokenPath: String, timeoutSeconds: Double) throws -> pid_t? {
        let credential = try readShutdownCredential(tokenPath: tokenPath)
        guard standardized(credential.socketPath) == standardized(socketPath) else {
            throw failure(
                "engine-control token names socket \(credential.socketPath), not reachable socket \(socketPath)"
            )
        }
        guard let payload = request(
            socketPath: socketPath,
            requestID: "engine-stop",
            payload: ["type": "shutdown", "token": credential.token],
            timeoutSeconds: timeoutSeconds
        ) else {
            throw failure("shutdown RPC did not return a response from \(socketPath)")
        }
        switch payload["type"] as? String {
        case "shutdown_accepted":
            return credential.pid
        case "shutdown_rejected":
            throw failure("shutdown RPC rejected: \((payload["reason"] as? String) ?? "unknown reason")")
        default:
            throw failure("shutdown RPC returned an unexpected response from \(socketPath)")
        }
    }

    func waitForClose(socketPath: String, timeoutSeconds: Double) -> Bool {
        let deadline = Date().addingTimeInterval(timeoutSeconds)
        while Date() < deadline {
            if !isReachable(socketPath: socketPath, timeoutSeconds: 0.5) {
                return true
            }
            Thread.sleep(forTimeInterval: 0.1)
        }
        return !isReachable(socketPath: socketPath, timeoutSeconds: 0.5)
    }

    private func request(
        socketPath: String,
        requestID: String,
        payload: [String: Any],
        timeoutSeconds: Double
    ) -> [String: Any]? {
        guard let socket = connect(socketPath: socketPath, timeoutSeconds: timeoutSeconds) else {
            return nil
        }
        defer { Darwin.close(socket) }

        let envelope: [String: Any] = ["request_id": requestID, "payload": payload]
        guard var body = try? JSONSerialization.data(withJSONObject: envelope) else {
            return nil
        }
        body.append(0x0A)
        guard sendAll(socket: socket, data: body) else {
            return nil
        }

        var responseBuffer = Data()
        var readBuffer = [UInt8](repeating: 0, count: 4096)
        while responseBuffer.count <= 256 * 1024 {
            let count = Darwin.recv(socket, &readBuffer, readBuffer.count, 0)
            if count <= 0 {
                return nil
            }
            responseBuffer.append(contentsOf: readBuffer[..<count])
            while let newline = responseBuffer.firstIndex(of: 0x0A) {
                let line = Data(responseBuffer[..<newline])
                responseBuffer.removeSubrange(...newline)
                guard let json = try? JSONSerialization.jsonObject(with: line) as? [String: Any],
                      json["request_id"] as? String == requestID,
                      let responsePayload = json["payload"] as? [String: Any]
                else {
                    continue
                }
                return responsePayload
            }
        }
        return nil
    }

    private func connect(socketPath: String, timeoutSeconds: Double) -> Int32? {
        let socket = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard socket >= 0 else { return nil }

        var timeout = timeval(
            tv_sec: Int(timeoutSeconds),
            tv_usec: Int32((timeoutSeconds.truncatingRemainder(dividingBy: 1)) * 1_000_000)
        )
        setsockopt(socket, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(socket, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))

        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let maximumLength = MemoryLayout.size(ofValue: address.sun_path)
        let pathLength = socketPath.lengthOfBytes(using: .utf8)
        guard pathLength < maximumLength else {
            Darwin.close(socket)
            return nil
        }
        _ = socketPath.withCString { source in
            withUnsafeMutablePointer(to: &address.sun_path) { destination in
                memcpy(UnsafeMutableRawPointer(destination), source, pathLength + 1)
            }
        }
        let result = withUnsafePointer(to: address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(socket, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard result == 0 else {
            Darwin.close(socket)
            return nil
        }
        return socket
    }

    private func sendAll(socket: Int32, data: Data) -> Bool {
        data.withUnsafeBytes { bytes in
            guard let base = bytes.baseAddress else { return false }
            var sent = 0
            while sent < bytes.count {
                let count = Darwin.send(socket, base.advanced(by: sent), bytes.count - sent, 0)
                if count <= 0 {
                    return false
                }
                sent += count
            }
            return true
        }
    }

    private func standardized(_ path: String) -> String {
        URL(fileURLWithPath: path).resolvingSymlinksInPath().standardizedFileURL.path
    }

    private func failure(_ message: String) -> NSError {
        NSError(
            domain: "Boss.EngineSocketControl",
            code: 1,
            userInfo: [NSLocalizedDescriptionKey: message]
        )
    }
}
