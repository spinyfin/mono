import CryptoKit
import Darwin
import Foundation

/// Bounded restart policy for an app-managed engine. The schedule is indexed
/// from one, and stays at its final value when callers allow more attempts
/// than explicit delays. Keeping this policy separate from process launching
/// makes the operational limits straightforward to test and tune.
struct EngineRestartPolicy: Equatable, Sendable {
    static let `default` = EngineRestartPolicy(
        backoffSchedule: [1, 2, 4, 8, 16, 30],
        maximumAttempts: 6,
        pollInterval: 1,
        healthyResetInterval: 60
    )

    let backoffSchedule: [TimeInterval]
    let maximumAttempts: Int
    let pollInterval: TimeInterval
    let healthyResetInterval: TimeInterval

    init(
        backoffSchedule: [TimeInterval],
        maximumAttempts: Int,
        pollInterval: TimeInterval = Self.default.pollInterval,
        healthyResetInterval: TimeInterval = Self.default.healthyResetInterval
    ) {
        self.backoffSchedule = backoffSchedule.isEmpty ? Self.default.backoffSchedule : backoffSchedule
        self.maximumAttempts = max(1, maximumAttempts)
        self.pollInterval = max(0.1, pollInterval)
        self.healthyResetInterval = max(0, healthyResetInterval)
    }

    func delay(forAttempt attempt: Int) -> TimeInterval {
        backoffSchedule[min(max(0, attempt - 1), backoffSchedule.count - 1)]
    }

    static func fromEnvironment(_ environment: [String: String] = ProcessInfo.processInfo.environment) -> Self {
        let schedule = environment["BOSS_ENGINE_RESTART_BACKOFF_SECONDS"]?
            .split(separator: ",")
            .compactMap { TimeInterval($0.trimmingCharacters(in: .whitespaces)) }
            .filter { $0 > 0 && $0.isFinite }
        let attempts = environment["BOSS_ENGINE_RESTART_MAX_ATTEMPTS"].flatMap(Int.init)
        return Self(
            backoffSchedule: schedule ?? [],
            maximumAttempts: attempts.map { max(1, $0) } ?? Self.default.maximumAttempts
        )
    }
}

/// State sent to the app chrome while the controller is recovering from an
/// unexpected engine exit. A manual restart resets the policy and returns to
/// `.running` once a socket is reachable.
enum EngineSupervisionState: Equatable, Sendable {
    case running
    case restarting(attempt: Int, retryAfter: TimeInterval)
    case restartFailed(attempt: Int?, message: String)
    case gaveUp(attempts: Int, lastError: String?)
}

final class EngineProcessController: @unchecked Sendable {
    private struct RunningEngine {
        let socketPath: String
        let pidPath: String
        let pid: pid_t?
    }

    private let paths: BossEnginePaths
    private let lockFilePath: String
    private let launchDirectory: String
    private let forceRestart: Bool
    private let stopOnExit: Bool
    private let restartPolicy: EngineRestartPolicy
    private let supervisionQueue = DispatchQueue(label: "Boss.EngineProcessController.supervision")
    private var supervisionTimer: DispatchSourceTimer?
    private var pendingRestart: DispatchWorkItem?
    private var supervisor: EngineSupervisor
    private var lastSupervisionState: EngineSupervisionState = .running
    private var lastLaunchError: String?
    private let socketControl: any EngineSocketControlling
    private let bundledEnginePathOverride: String?
    private let launchHandler: (@Sendable (String, String?, String) throws -> pid_t)?
    private let supervisionStopLock = NSLock()
    private var supervisionStopped = false

    var onOutputLine: (@MainActor @Sendable (String) -> Void)?
    var onSupervisionStateChange: (@MainActor @Sendable (EngineSupervisionState) -> Void)?

    /// Liveness signal for the per-`pollInterval` supervision tick. When set
    /// (normally to `EngineClient.isReachable`, which reflects the app's
    /// existing long-lived connection), the tick reuses that connection's
    /// state instead of opening and closing a fresh socket to the engine's
    /// frontend listener every tick — each such probe drives a full
    /// `handle_frontend_connection` session setup/teardown on the engine
    /// side. Falls back to a raw socket probe (`discoverRunningEngine`) when
    /// unset, e.g. in tests that exercise supervision without an
    /// `EngineClient`.
    var livenessProbe: (@Sendable () -> Bool)?

    init(
        paths: BossEnginePaths,
        launchDirectory: String = ProcessInfo.processInfo.environment["BUILD_WORKSPACE_DIRECTORY"]
            ?? FileManager.default.currentDirectoryPath,
        forceRestart: Bool = ProcessInfo.processInfo.environment["BOSS_ENGINE_FORCE_RESTART"] == "1",
        stopOnExit: Bool = ProcessInfo.processInfo.environment["BOSS_ENGINE_STOP_ON_EXIT"] == "1",
        restartPolicy: EngineRestartPolicy = .fromEnvironment(),
        socketControl: any EngineSocketControlling = EngineSocketControl(),
        bundledEnginePathOverride: String? = nil,
        launchHandler: (@Sendable (String, String?, String) throws -> pid_t)? = nil,
        livenessProbe: (@Sendable () -> Bool)? = nil
    ) {
        self.paths = paths
        self.lockFilePath = "\(paths.pidPath).lock"
        self.launchDirectory = launchDirectory
        self.forceRestart = forceRestart
        self.stopOnExit = stopOnExit
        self.restartPolicy = restartPolicy
        self.supervisor = EngineSupervisor(policy: restartPolicy)
        self.socketControl = socketControl
        self.bundledEnginePathOverride = bundledEnginePathOverride
        self.launchHandler = launchHandler
        self.livenessProbe = livenessProbe
    }

    deinit {
        supervisionTimer?.cancel()
        pendingRestart?.cancel()
    }

    func start() throws {
        do {
            try start(resetRestartBudget: true)
        } catch {
            reportLaunchFailure(error, attempt: nil)
            throw error
        }
    }

    private func start(resetRestartBudget: Bool) throws {
        let socketPath = paths.socketPath
        try withStartLock {
            if forceRestart, let running = discoverRunningEngine() {
                emit("[engine restart] terminating existing engine \(describe(running))")
                try stopRunningEngine(running)
            }

            if let running = discoverRunningEngine() {
                // An engine is already running. Check if its binary matches
                // the app's bundled engine. If not, replace it so the user
                // always gets the version that shipped with this app launch.
                //
                // Each branch emits a distinct log line so a user reporting
                // "engine wasn't restarted after I updated Boss" can grep
                // the system messages and tell exactly which path fired:
                //   [engine version-check skipped: <reason>] — check didn't run
                //   [engine version-check ok] — ran, fingerprints matched
                //   [engine upgrade] — ran, fingerprints differed, restarting
                //
                // BOSS_ENGINE_CMD wins unconditionally (see
                // resolveEngineCommand): a dev running against a custom
                // engine binary has nothing bundled to compare it to, and
                // fingerprinting the *bundled* binary against it would
                // always mismatch and tear down the dev's own engine.
                if ProcessInfo.processInfo.environment["BOSS_ENGINE_CMD"] != nil {
                    emit("[engine version-check skipped: BOSS_ENGINE_CMD is set (developer custom engine)] attaching to \(describe(running))")
                    return
                }
                guard let bundledPath = bundledEnginePath() else {
                    emit("[engine version-check skipped: no bundled engine in app resources (dev/bazel-run mode)] attaching to \(describe(running))")
                    return
                }
                guard let bundledFP = computeBinaryFingerprint(path: bundledPath) else {
                    throw controllerError("could not fingerprint bundled engine at \(bundledPath)")
                }

                // A single missed 3s deadline can mean "the engine is busy",
                // not "the engine is stale" — retry a couple of times before
                // treating non-response as a mismatch, so a slow-but-healthy
                // engine isn't torn down and replaced (and doesn't then hit
                // the SIGTERM/SIGKILL fallback if the follow-on shutdown RPC
                // also times out).
                var runningFP: String?
                var fingerprintAttempts = 0
                let maxFingerprintAttempts = 3
                while fingerprintAttempts < maxFingerprintAttempts {
                    fingerprintAttempts += 1
                    runningFP = socketControl.fingerprint(socketPath: running.socketPath, timeoutSeconds: 3.0)
                    if runningFP != nil { break }
                }

                if let runningFP, bundledFP == runningFP {
                    emit("[engine version-check ok] running=\(runningFP) matches bundled — attaching to \(describe(running))")
                    return
                }

                if runningFP == nil {
                    emit("[engine upgrade] running fingerprint unavailable after \(fingerprintAttempts) attempts bundled=\(bundledFP) — replacing \(describe(running))")
                } else {
                    emit("[engine upgrade] running=\(runningFP ?? "unavailable") bundled=\(bundledFP) — replacing \(describe(running))")
                }
                try stopRunningEngine(running)
                emit("[engine upgrade] old engine stopped — launching new engine from bundle")
            }

            let (command, bossBinDir) = resolveEngineCommand(socketPath: socketPath)

            let pid = try launchEngine(command: command, bossBinDir: bossBinDir, socketPath: socketPath)
            lastLaunchError = nil
            emit("[engine launch] detached pid=\(pid) socket=\(socketPath) \(command)")
        }
        // `disableSupervision()` may be called while a supervised relaunch is
        // waiting on filesystem or socket I/O above. It sets this lock-backed
        // flag without waiting for this queue, so do not reactivate polling
        // after that cancellation has won.
        guard !isSupervisionStopped else { return }
        enableSupervision(resetRestartBudget: resetRestartBudget)
    }

    // MARK: - Version-check helpers

    /// Path to the engine binary shipped inside the current app bundle.
    /// Returns `nil` in dev/bazel-run mode where no bundle engine exists.
    private func bundledEnginePath() -> String? {
        if let bundledEnginePathOverride {
            return bundledEnginePathOverride
        }
        guard let resourcePath = Bundle.main.resourcePath else { return nil }
        let path = "\(resourcePath)/bin/\(BossEngineBinary.executableName)"
        guard FileManager.default.fileExists(atPath: path) else { return nil }
        return path
    }

    /// Compute a binary fingerprint of `path` using the same algorithm
    /// as `boss_engine::build_info::binary_fingerprint`:
    ///   SHA-256 of up to 64 MiB of file content → first 6 bytes as
    ///   12 lowercase hex digits, optionally suffixed "-truncated".
    private func computeBinaryFingerprint(path: String) -> String? {
        guard let fh = FileHandle(forReadingAtPath: path) else { return nil }
        defer { try? fh.close() }

        let cap: Int = 64 * 1024 * 1024
        var hasher = SHA256()
        var readTotal = 0
        var truncated = false
        let chunkSize = 64 * 1024

        while true {
            let remaining = cap - readTotal
            guard remaining > 0 else {
                // Probe for more bytes to set the truncated flag.
                let probe = (try? fh.read(upToCount: 1)) ?? Data()
                if !probe.isEmpty { truncated = true }
                break
            }
            let toRead = min(chunkSize, remaining)
            guard let chunk = try? fh.read(upToCount: toRead), !chunk.isEmpty else { break }
            hasher.update(data: chunk)
            readTotal += chunk.count
            if chunk.count < toRead {
                // EOF before cap.
                break
            }
        }

        let digest = hasher.finalize()
        let firstSixBytes = digest.prefix(6)
        let hex = firstSixBytes.map { String(format: "%02x", $0) }.joined()
        return truncated ? "\(hex)-truncated" : hex
    }

    /// Resolve the engine command and the BOSS_BIN_DIR to export.
    ///
    /// Resolution order (per design doc Q3):
    ///   1. BOSS_ENGINE_CMD env override — wins unconditionally so a dev
    ///      running `bazel run //tools/boss/app-macos:Boss` against a custom
    ///      engine still works.
    ///   2. Bundle-relative path: `<Bundle.main.resourcePath>/bin/<engine>` —
    ///      the installed app path; BOSS_BIN_DIR is set to the bin/ dir so
    ///      the engine can resolve its sibling CLIs.
    ///   3. `bazel run` fallback — dev mode for when the bundle lacks the
    ///      pre-built engine (e.g. iterating on just the Swift side).
    private func resolveEngineCommand(socketPath: String) -> (command: String, bossBinDir: String?) {
        if let override = ProcessInfo.processInfo.environment["BOSS_ENGINE_CMD"] {
            return (override, nil)
        }
        if let resourcePath = Bundle.main.resourcePath {
            let enginePath = "\(resourcePath)/bin/\(BossEngineBinary.executableName)"
            if FileManager.default.fileExists(atPath: enginePath) {
                let bossBinDir = "\(resourcePath)/bin"
                return ("\(shellQuote(enginePath)) --socket-path \(shellQuote(socketPath))", bossBinDir)
            }
        }
        return ("\(BossEngineBinary.bazelRunCommand) -- --socket-path \(shellQuote(socketPath))", nil)
    }

    func stop() {
        disableSupervision()
        guard stopOnExit else {
            return
        }

        guard let running = discoverRunningEngine() else {
            return
        }

        do {
            try stopRunningEngine(running)
            emit("[engine stop] terminated \(describe(running))")
        } catch {
            emit("[engine stop] failed: \(error.localizedDescription)")
        }
    }

    /// User-initiated recovery for a stale engine: discover the reachable
    /// engine by socket (token-auth RPC first, then a validated peer/pid-file
    /// SIGTERM/SIGKILL fallback) and relaunch
    /// from the same binary `start()` would. Used by the "Restart
    /// engine" affordance on the unreachable banner so a hung or
    /// orphaned engine no longer requires a shell `pkill` (issue #697).
    ///
    /// Holds the start lock for the whole terminate + launch sequence
    /// so a concurrent `start()` can't race and end up with two
    /// engines fighting over the same socket. Safe to call when no
    /// engine is running — falls through to the launch step.
    func restart() throws {
        disableSupervision()
        do {
            try withStartLock {
                if let running = discoverRunningEngine() {
                    emit("[engine restart] terminating existing engine \(describe(running))")
                    try stopRunningEngine(running)
                }

                let socketPath = paths.socketPath
                let (command, bossBinDir) = resolveEngineCommand(socketPath: socketPath)
                let pid = try launchEngine(command: command, bossBinDir: bossBinDir, socketPath: socketPath)
                lastLaunchError = nil
                emit("[engine restart] detached pid=\(pid) socket=\(socketPath) \(command)")
            }
            enableSupervision(resetRestartBudget: true)
        } catch {
            reportLaunchFailure(error, attempt: nil)
            throw error
        }
    }

    // MARK: - Unexpected-exit supervision

    /// The engine is deliberately detached from the app so it survives an app
    /// restart. That also means `Process.terminationHandler` cannot observe
    /// it. Polling socket reachability gives the controller the same answer
    /// without making the engine an app child, and serializing this work
    /// prevents two timer ticks from launching competing replacements.
    private func enableSupervision(resetRestartBudget: Bool) {
        // Set this before hopping onto the supervision queue so a later
        // `stop()` always wins, even if this queued block has not run yet.
        setSupervisionStopped(false)
        supervisionQueue.async { [weak self] in
            guard let self else { return }
            if resetRestartBudget {
                self.supervisor.reset()
            }
            if self.supervisionTimer == nil {
                let timer = DispatchSource.makeTimerSource(queue: self.supervisionQueue)
                timer.schedule(
                    deadline: .now() + self.restartPolicy.pollInterval,
                    repeating: self.restartPolicy.pollInterval
                )
                timer.setEventHandler { [weak self] in
                    self?.checkEngineLiveness()
                }
                self.supervisionTimer = timer
                timer.resume()
            }
            self.emitSupervisionState(.running)
        }
    }

    private func disableSupervision() {
        setSupervisionStopped(true)
        supervisionQueue.async { [weak self] in
            guard let self else { return }
            pendingRestart?.cancel()
            pendingRestart = nil
            supervisionTimer?.cancel()
            supervisionTimer = nil
        }
    }

    private func checkEngineLiveness() {
        guard !isSupervisionStopped, pendingRestart == nil else { return }
        let isAlive = livenessProbe?() ?? (discoverRunningEngine() != nil)
        let action = supervisor.tick(isAlive: isAlive, now: Date())
        switch action {
        case .running:
            lastLaunchError = nil
            emitSupervisionState(.running)
            return
        case let .gaveUp(attempts):
            emit("[engine supervision] gave up after \(attempts) restart attempts; use Restart engine to try again")
            emitSupervisionState(.gaveUp(attempts: attempts, lastError: lastLaunchError))
            return
        case let .restart(attempt, delay):
            emit("[engine supervision] engine exited; restart attempt \(attempt)/\(restartPolicy.maximumAttempts) in \(Int(delay))s")
            emitSupervisionState(.restarting(attempt: attempt, retryAfter: delay))
            let work = DispatchWorkItem { [weak self] in
                self?.relaunchAfterUnexpectedExit(attempt: attempt)
            }
            pendingRestart = work
            supervisionQueue.asyncAfter(deadline: .now() + delay, execute: work)
        case .wait:
            return
        }
    }

    private func relaunchAfterUnexpectedExit(attempt: Int) {
        pendingRestart = nil
        guard !isSupervisionStopped else { return }
        do {
            try start(resetRestartBudget: false)
        } catch {
            lastLaunchError = error.localizedDescription
            emit("[engine supervision] restart launch failed: \(error.localizedDescription)")
            emitSupervisionState(.restartFailed(attempt: attempt, message: error.localizedDescription))
            // The next timer tick applies the next backoff slot.
        }
    }

    private func launchEngine(command: String, bossBinDir: String?, socketPath: String) throws -> pid_t {
        if let launchHandler {
            return try launchHandler(command, bossBinDir, socketPath)
        }
        return try launchDetached(command: command, bossBinDir: bossBinDir, socketPath: socketPath)
    }

    private func launchDetached(command: String, bossBinDir: String?, socketPath: String) throws -> pid_t {
        let launchErrorPath = URL(fileURLWithPath: paths.pidPath)
            .deletingLastPathComponent()
            .appendingPathComponent("engine-launch.log")
            .path
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/bin/zsh")
        // zsh's BG_NICE option (on by default) reduces the priority of any
        // job started with `&` by a non-interactive shell — nice(5) rather
        // than the caller's own nice(0) — because a `zsh -c` script has no
        // job control (monitor mode) active. That silently demoted the
        // detached engine, and everything it forks (the private tmux
        // server, then the coordinator's `claude` pane inside it) inherit
        // that nice(5) by plain fork inheritance. `NO_BG_NICE` keeps the
        // backgrounded engine at the same priority as this app.
        //
        // This only reaches a tmux server started by a post-fix engine: a
        // private server already running under the durable state-root socket
        // is never killed by this codebase, so on an existing install it
        // keeps the nice(5) it was born with even after this fix ships. After
        // upgrading, `tmux -S "$HOME/Library/Application Support/Boss/tmux.sock"
        // kill-server` (or a reboot) once so the next engine launch creates a
        // fresh server under this corrected priority. A leftover `-L boss`
        // server is drained at boot rather than relocated.
        let script = """
        setopt NO_BG_NICE
        : > \(shellQuote(launchErrorPath))
        nohup \(command) >/dev/null 2>\(shellQuote(launchErrorPath)) &
        child=$!
        print -r -- $child
        sleep 0.25
        if ! kill -0 $child 2>/dev/null; then
          wait $child
          exit $?
        fi
        """
        proc.arguments = ["-c", script]
        proc.currentDirectoryURL = URL(fileURLWithPath: launchDirectory, isDirectory: true)
        // Tell the engine the app's pid explicitly. `bazel run`
        // daemonizes its server, which reparents the engine binary
        // away from the app's process tree, so `getppid()` and any
        // ancestor walk both miss the real app. The engine reads
        // BOSS_APP_PID to set its trust root for `RegisterAppSession`.
        var env = ProcessInfo.processInfo.environment
        env["BOSS_APP_PID"] = String(getpid())
        // The engine's tracing layer continues writing its bounded text log,
        // while direct startup failures from Rust's main function land in the
        // launch-error file above for this controller to surface verbatim.
        env["BOSS_ENGINE_STDERR_LOGGING"] = "0"
        // BOSS_BIN_DIR tells the engine where its sibling CLIs live
        // (boss, bossctl, boss-event) in installed mode. The engine
        // propagates this to workers so they resolve the bundled copies
        // rather than any PATH match. Unset in dev mode (no bundle bin/).
        if let dir = bossBinDir {
            env["BOSS_BIN_DIR"] = dir
        }
        // When launched from Finder/Dock/launchctl, the app inherits a minimal
        // launchd GUI session PATH (/usr/bin:/bin:/usr/sbin:/sbin) that omits
        // developer tool directories. The engine and its cube subprocesses need
        // jj, mint, and other tools that live outside that minimal set.
        //
        // We prepend well-known locations rather than shelling out to read the
        // user's login-shell PATH (which would be more accurate but brittle — a
        // misbehaving shell init could hang the app or print garbage). Extra
        // segments that don't exist on a given machine are ignored by the kernel.
        env["PATH"] = augmentedPATH(current: env["PATH"] ?? "/usr/bin:/bin:/usr/sbin:/sbin")
        // If the user pasted an ANTHROPIC_API_KEY into Settings → Engine,
        // override whatever the launchd parent inherited so the Settings
        // value always wins (work item #735). When no Settings value is
        // stored, leave the inherited env entry untouched so a shell
        // `export ANTHROPIC_API_KEY=…` followed by `boss` from a terminal
        // continues to work for users who prefer that path.
        if let stored = APIKeyStore.readAnthropicApiKey() {
            env["ANTHROPIC_API_KEY"] = stored
        }
        proc.environment = env
        let output = Pipe()
        let shellError = Pipe()
        proc.standardOutput = output
        proc.standardError = shellError

        try proc.run()
        proc.waitUntilExit()
        let outputData = output.fileHandleForReading.readDataToEndOfFile()
        let shellErrorData = shellError.fileHandleForReading.readDataToEndOfFile()
        let childPID = String(data: outputData, encoding: .utf8)?
            .trimmingCharacters(in: .whitespacesAndNewlines)
            .split(separator: "\n")
            .first
            .flatMap { pid_t($0) }
        if proc.terminationStatus != 0 {
            let shellMessage = String(data: shellErrorData, encoding: .utf8)?
                .trimmingCharacters(in: .whitespacesAndNewlines)
            throw controllerError(
                launchFailureMessage(
                    prefix: "engine process exited during launch with status \(proc.terminationStatus)",
                    launchErrorPath: launchErrorPath,
                    fallback: shellMessage
                ),
                code: Int(proc.terminationStatus)
            )
        }
        guard let childPID, childPID > 1 else {
            throw controllerError("detached engine launcher did not report the child pid")
        }

        // The frontend socket binds late in engine startup — after
        // `WorkDb::open`, the tmux preflight probe (which shells out to
        // tmux), and planner-run recovery — so a slow-but-healthy start can
        // legitimately outlast this window, especially the `bazel run`
        // command shape used in dev mode, where a cold build alone can
        // exceed it. Only the child actually exiting (checked every
        // iteration above) is treated as a launch failure; a still-running
        // child that simply hasn't bound its socket yet is not killed —
        // supervision (`checkEngineLiveness`) observes the outcome once the
        // socket comes up, or once the process actually exits.
        let readinessWindow: TimeInterval = command.contains(BossEngineBinary.bazelRunCommand) ? 120 : 30
        let deadline = Date().addingTimeInterval(readinessWindow)
        while Date() < deadline {
            if socketControl.isReachable(socketPath: socketPath, timeoutSeconds: 0.5) {
                return childPID
            }
            if !isProcessRunning(childPID) {
                throw controllerError(
                    launchFailureMessage(
                        prefix: "engine process pid=\(childPID) exited before its socket became reachable",
                        launchErrorPath: launchErrorPath,
                        fallback: nil
                    )
                )
            }
            Thread.sleep(forTimeInterval: 0.1)
        }

        emit(
            "[engine launch] pid=\(childPID) socket \(socketPath) not yet reachable after \(Int(readinessWindow))s; still running, leaving it to supervision rather than killing it"
        )
        return childPID
    }

    private func launchFailureMessage(prefix: String, launchErrorPath: String, fallback: String?) -> String {
        let data = try? Data(contentsOf: URL(fileURLWithPath: launchErrorPath))
        let tail = data.map { Data($0.suffix(32 * 1024)) }
        let engineError = tail.flatMap { String(data: $0, encoding: .utf8) }?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let detail = [engineError, fallback]
            .compactMap { $0 }
            .first { !$0.isEmpty }
        return detail.map { "\(prefix): \($0)" } ?? prefix
    }

    private func shellQuote(_ value: String) -> String {
        "'\(value.replacingOccurrences(of: "'", with: "'\"'\"'"))'"
    }

    /// Prepend standard developer-tool directories to PATH so the engine and its
    /// subprocesses (cube, jj, mint, cargo binaries) can be found when the app is
    /// launched from Finder/Dock/launchctl with a minimal launchd PATH.
    ///
    /// Order matches typical shell precedence: Apple Silicon Homebrew, Intel/manual
    /// Homebrew, LinkedIn corporate tools, Rust/Cargo, then user-local directories.
    /// Segments that don't exist on the current machine are harmless — the kernel
    /// skips non-existent PATH entries. The original launchd PATH is preserved at the
    /// end so system tools continue to resolve normally.
    private func augmentedPATH(current: String) -> String {
        let home = ProcessInfo.processInfo.environment["HOME"] ?? NSHomeDirectory()
        let extra = [
            "/opt/homebrew/bin",        // Apple Silicon Homebrew (jj, etc.)
            "/usr/local/bin",           // Intel Homebrew, manual installs
            "/usr/local/linkedin/bin",  // LinkedIn corporate tools (mint, etc.)
            "\(home)/.cargo/bin",       // Rust binaries (jj commonly installed here)
            "\(home)/bin",              // user-local binaries
            "\(home)/.local/bin",       // XDG-style user-local binaries
        ]
        // Deduplicate: keep the first occurrence of each segment.
        var seen = Set(current.split(separator: ":").map(String.init))
        let unique = extra.filter { seen.insert($0).inserted }
        let prefix = unique.joined(separator: ":")
        return prefix.isEmpty ? current : "\(prefix):\(current)"
    }

    private func discoverRunningEngine() -> RunningEngine? {
        for endpoint in paths.endpointPaths {
            guard socketControl.isReachable(socketPath: endpoint.socketPath) else {
                continue
            }
            // Peer credentials are direct, unforgeable proof of which
            // process owns the socket we just connected to; the pid file is
            // the deletable, potentially stale artefact this discovery step
            // otherwise leans on. Prefer the peer pid and only fall back to
            // the file when peer credentials aren't available.
            let pid = socketControl.peerPID(socketPath: endpoint.socketPath, timeoutSeconds: 1)
                ?? currentEnginePID(pidPath: endpoint.pidPath)
            return RunningEngine(
                socketPath: endpoint.socketPath,
                pidPath: endpoint.pidPath,
                pid: pid
            )
        }
        return nil
    }

    private func currentEnginePID(pidPath: String) -> pid_t? {
        guard let pid = readPIDFile(pidPath: pidPath) else {
            return nil
        }

        if !isProcessRunning(pid) {
            clearPIDFileIfOwned(pid: pid, pidPath: pidPath)
            return nil
        }

        guard isLikelyEngineProcess(pid) else {
            emit("[engine pid] pid file points to non-engine process pid=\(pid)")
            return nil
        }

        return pid
    }

    private func readPIDFile(pidPath: String) -> pid_t? {
        guard let content = try? String(contentsOfFile: pidPath, encoding: .utf8) else {
            return nil
        }

        let trimmed = content.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let value = Int32(trimmed), value > 1 else {
            return nil
        }
        return value
    }

    private func clearPIDFileIfOwned(pid: pid_t, pidPath: String) {
        guard let owner = readPIDFile(pidPath: pidPath), owner == pid else {
            return
        }
        try? FileManager.default.removeItem(atPath: pidPath)
    }

    private func isProcessRunning(_ pid: pid_t) -> Bool {
        if kill(pid, 0) == 0 {
            return true
        }
        return errno == EPERM
    }

    private func isLikelyEngineProcess(_ pid: pid_t) -> Bool {
        guard let command = commandLine(for: pid) else {
            return false
        }

        return command.contains(BossEngineBinary.bazelOutputPathFragment)
            || command.contains(BossEngineBinary.bazelRunCommand)
            || command.contains(BossEngineBinary.bundlePathFragment)
    }

    private func commandLine(for pid: pid_t) -> String? {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/bin/ps")
        proc.arguments = ["-p", "\(pid)", "-o", "command="]
        let output = Pipe()
        proc.standardOutput = output
        proc.standardError = Pipe()

        do {
            try proc.run()
            proc.waitUntilExit()
        } catch {
            return nil
        }

        guard proc.terminationStatus == 0 else {
            return nil
        }

        let data = output.fileHandleForReading.readDataToEndOfFile()
        let text = String(data: data, encoding: .utf8)?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if let text, !text.isEmpty {
            return text
        }
        return nil
    }

    private func stopRunningEngine(_ running: RunningEngine) throws {
        var fallbackPID = running.pid
        var rpcFailure: Error?
        do {
            fallbackPID = try socketControl.shutdown(
                socketPath: running.socketPath,
                tokenPath: paths.controlTokenPath,
                timeoutSeconds: 5
            ) ?? fallbackPID
            if socketControl.waitForClose(socketPath: running.socketPath, timeoutSeconds: 8) {
                if let fallbackPID {
                    clearPIDFileIfOwned(pid: fallbackPID, pidPath: running.pidPath)
                }
                return
            }
            rpcFailure = controllerError(
                "shutdown RPC was accepted, but socket \(running.socketPath) remained reachable after 8 seconds"
            )
        } catch {
            rpcFailure = error
        }

        guard let pid = fallbackPID else {
            throw controllerError(
                "engine is reachable at \(running.socketPath), but graceful shutdown failed and no pid is available: \(rpcFailure?.localizedDescription ?? "unknown error")"
            )
        }
        guard isProcessRunning(pid), isLikelyEngineProcess(pid) else {
            throw controllerError(
                "engine is reachable at \(running.socketPath), but graceful shutdown failed and pid \(pid) is not a running Boss engine: \(rpcFailure?.localizedDescription ?? "unknown error")"
            )
        }

        emit("[engine stop] rpc unavailable (\(rpcFailure?.localizedDescription ?? "unknown error")); falling back to SIGTERM pid=\(pid)")
        _ = kill(pid, SIGTERM)
        for _ in 0..<50 {
            if !isProcessRunning(pid) {
                break
            }
            Thread.sleep(forTimeInterval: 0.1)
        }
        if isProcessRunning(pid) {
            emit("[engine stop] pid=\(pid) still alive after 5s; sending SIGKILL")
            _ = kill(pid, SIGKILL)
        }
        guard socketControl.waitForClose(socketPath: running.socketPath, timeoutSeconds: 3) else {
            throw controllerError("engine socket \(running.socketPath) remained reachable after stopping pid=\(pid)")
        }
        clearPIDFileIfOwned(pid: pid, pidPath: running.pidPath)
    }

    private func describe(_ running: RunningEngine) -> String {
        if let pid = running.pid {
            return "pid=\(pid) socket=\(running.socketPath)"
        }
        return "socket=\(running.socketPath) (pid file absent)"
    }

    private func controllerError(_ message: String, code: Int = 1) -> NSError {
        NSError(
            domain: "Boss.EngineProcessController",
            code: code,
            userInfo: [NSLocalizedDescriptionKey: message]
        )
    }

    private func reportLaunchFailure(_ error: Error, attempt: Int?) {
        let message = error.localizedDescription
        lastLaunchError = message
        emit("[engine launch] failed: \(message)")
        supervisionQueue.async { [weak self] in
            self?.emitSupervisionState(.restartFailed(attempt: attempt, message: message))
        }
    }

    private func withStartLock<T>(_ body: () throws -> T) throws -> T {
        let lockDirectory = URL(fileURLWithPath: lockFilePath).deletingLastPathComponent()
        try FileManager.default.createDirectory(at: lockDirectory, withIntermediateDirectories: true)
        let fd = open(lockFilePath, O_CREAT | O_RDWR, 0o600)
        guard fd >= 0 else {
            throw NSError(
                domain: "Boss.EngineProcessController",
                code: Int(errno),
                userInfo: [NSLocalizedDescriptionKey: "failed to open lock file: \(lockFilePath)"]
            )
        }

        defer {
            close(fd)
        }

        guard flock(fd, LOCK_EX) == 0 else {
            throw NSError(
                domain: "Boss.EngineProcessController",
                code: Int(errno),
                userInfo: [NSLocalizedDescriptionKey: "failed to acquire engine start lock"]
            )
        }

        defer {
            _ = flock(fd, LOCK_UN)
        }

        return try body()
    }

    private func emit(_ line: String) {
        Task { @MainActor in
            self.onOutputLine?(line)
        }
    }

    private var isSupervisionStopped: Bool {
        supervisionStopLock.lock()
        defer { supervisionStopLock.unlock() }
        return supervisionStopped
    }

    private func setSupervisionStopped(_ stopped: Bool) {
        supervisionStopLock.lock()
        supervisionStopped = stopped
        supervisionStopLock.unlock()
    }

    /// Must be called from `supervisionQueue`, which serializes state changes.
    private func emitSupervisionState(_ state: EngineSupervisionState) {
        guard state != lastSupervisionState else { return }
        lastSupervisionState = state
        Task { @MainActor in
            self.onSupervisionStateChange?(state)
        }
    }
}
