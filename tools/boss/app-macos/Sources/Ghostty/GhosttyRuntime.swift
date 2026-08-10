import AppKit
import CrashWatchdog
import Foundation
import GhosttyKit
import os.log

private let ghosttyBootstrapLog = Logger(
    subsystem: "dev.spinyfin.bossmacapp", category: "ghostty-bootstrap"
)

private func ghosttyRuntimeWakeup(_ userdata: UnsafeMutableRawPointer?) {
    GhosttyRuntime.wakeup(userdata)
}

private func ghosttyRuntimeAction(
    _ app: ghostty_app_t?,
    _ target: ghostty_target_s,
    _ action: ghostty_action_s
) -> Bool {
    guard app != nil else { return false }
    return GhosttyRuntime.action(target: target, action: action)
}

private func ghosttyRuntimeReadClipboard(
    _ userdata: UnsafeMutableRawPointer?,
    _ location: ghostty_clipboard_e,
    _ state: UnsafeMutableRawPointer?
) -> Bool {
    GhosttyRuntime.readClipboard(userdata, location: location, state: state)
}

private func ghosttyRuntimeWriteClipboard(
    _ userdata: UnsafeMutableRawPointer?,
    _ location: ghostty_clipboard_e,
    _ content: UnsafePointer<ghostty_clipboard_content_s>?,
    _ len: Int,
    _ confirm: Bool
) {
    GhosttyRuntime.writeClipboard(
        userdata,
        location: location,
        content: content,
        len: len,
        confirm: confirm
    )
}

private func ghosttyRuntimeCloseSurface(_ userdata: UnsafeMutableRawPointer?, _ needsConfirmation: Bool) {
    GhosttyRuntime.closeSurface(userdata, needsConfirmation: needsConfirmation)
}

extension Notification.Name {
    /// Posted on `NotificationCenter.default` when `GhosttyRuntime` observes
    /// the system's displays waking from sleep (`NSWorkspace.didWakeNotification`
    /// / `screensDidWakeNotification`). Panes still in the surface-less
    /// placeholder state (#800 — `ghostty_surface_new` returned NULL because
    /// no display was active) observe this to retry `attemptSurfaceCreation()`
    /// immediately, rather than waiting on `NSApplication.didChangeScreenParametersNotification`
    /// (which does not reliably fire on every wake) or the next engine spawn
    /// attempt. See `GhosttyTerminalHostView.installScreenObserverIfNeeded()`.
    static let ghosttyDisplaysDidWake = Notification.Name("GhosttyDisplaysDidWake")
}

enum GhosttyBootstrap {
    private static let initialized: Void = {
        // Strip `GHOSTTY_*` env vars from the process environment before
        // libghostty initializes. When Boss is launched from inside a
        // Ghostty.app terminal pane (the dev workflow with `swift run Boss`
        // from a Ghostty shell), the parent injects `GHOSTTY_RESOURCES_DIR`,
        // `GHOSTTY_BIN_DIR`, `GHOSTTY_SHELL_FEATURES` and friends. Those
        // point at the host Ghostty.app's resource tree, which can be a
        // different libghostty version than the one bundled in this app —
        // and `ghostty_init` / `ghostty_app_new` consume them. The observed
        // failure mode is `ghostty_surface_new` returning NULL after every
        // other input checks out (see #613 dump path). Removing the
        // pollution before we touch libghostty closes that surface.
        //
        // Subprocesses (the Claude panes) are unaffected — their env is
        // built separately and passed via `ghostty_surface_config_s.env_vars`.
        stripGhosttyEnvVars()

        let result = ghostty_init(UInt(CommandLine.argc), CommandLine.unsafeArgv)
        guard result == GHOSTTY_SUCCESS else {
            fatalError("ghostty_init failed with status \(result)")
        }

        // `ghostty_init` is what initializes Sentry (and, through it,
        // Breakpad's SIGABRT handler). Chaining our bounded-termination
        // watchdog in *after* that call is what puts it in front of them in
        // the handler chain — install it earlier and an unbounded spin inside
        // Sentry never reaches us. See [[CrashWatchdog]] for the 2026-07-29
        // livelock this closes.
        let hooked = CrashWatchdog.install()
        ghosttyBootstrapLog.info(
            "crash watchdog installed for \(hooked.count, privacy: .public) signal(s)"
        )
    }()

    static func ensureInitialized() {
        _ = initialized
    }

    private static func stripGhosttyEnvVars() {
        // Snapshot the names first; `unsetenv` mutates `environ`, so
        // iterating it in-place is unsafe.
        var toRemove: [String] = []
        var envp: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?> = environ
        while let raw = envp.pointee {
            if let entry = String(validatingCString: raw),
               let eq = entry.firstIndex(of: "="),
               entry[..<eq].hasPrefix("GHOSTTY_") {
                toRemove.append(String(entry[..<eq]))
            }
            envp = envp.successor()
        }
        for name in toRemove {
            unsetenv(name)
        }
    }
}

final class GhosttyRuntime: @unchecked Sendable {
    /// Singleton instance. libghostty's app handle (`ghostty_app_t`)
    /// owns global state — multiple instances would race on input
    /// dispatch and the wakeup callback. All callers share `shared`.
    static let shared = GhosttyRuntime()

    private let config: ghostty_config_t
    private(set) var app: ghostty_app_t! = nil
    private var observers: [NSObjectProtocol] = []
    /// Tokens registered on `NSWorkspace.shared.notificationCenter` (a
    /// different center than `observers`, which uses `NotificationCenter.default`
    /// — removal must go through the same center that created the token).
    private var workspaceObservers: [NSObjectProtocol] = []

    /// Invoked after `.ghosttyDisplaysDidWake` is broadcast, once per wake.
    /// `ContentView` wires this to report "spawn capability restored" to the
    /// engine (`ChatViewModel.spawnCapabilityRestored()`), so a sleep/wake
    /// cycle that stranded a worker-pane spawn is redispatched immediately
    /// instead of waiting for the engine's periodic sweeps.
    var onDisplaysDidWake: (() -> Void)?

    private init() {
        GhosttyBootstrap.ensureInitialized()

        guard let config = ghostty_config_new() else {
            fatalError("ghostty_config_new failed")
        }
        self.config = config

        ghostty_config_load_default_files(config)
        ghostty_config_load_recursive_files(config)
        // Boss embed overrides must load *after* user/default files so a
        // personal `window-vsync = true` cannot re-enable the fatal
        // DisplayLink path on GhosttyKit 5659cef (see applyBossEmbedConfigOverrides).
        Self.applyBossEmbedConfigOverrides(config)
        ghostty_config_finalize(config)

        var runtimeConfig = ghostty_runtime_config_s(
            userdata: Unmanaged.passUnretained(self).toOpaque(),
            supports_selection_clipboard: false,
            wakeup_cb: ghosttyRuntimeWakeup,
            action_cb: ghosttyRuntimeAction,
            read_clipboard_cb: ghosttyRuntimeReadClipboard,
            confirm_read_clipboard_cb: { _, _, _, _ in },
            write_clipboard_cb: ghosttyRuntimeWriteClipboard,
            close_surface_cb: ghosttyRuntimeCloseSurface
        )

        guard let app = ghostty_app_new(&runtimeConfig, config) else {
            fatalError("ghostty_app_new failed")
        }
        self.app = app

        ghostty_app_set_focus(app, false)
        installObservers()
    }

    deinit {
        for observer in observers {
            NotificationCenter.default.removeObserver(observer)
        }
        for observer in workspaceObservers {
            NSWorkspace.shared.notificationCenter.removeObserver(observer)
        }
        ghostty_app_free(app)
        ghostty_config_free(config)
    }

    func tick() {
        // Count the app-loop tick for the terminal event-loop diagnostics
        // (see [[TerminalLoopMonitor]]). Cheap, and on a path the spin
        // cannot bypass.
        TerminalLoopMonitor.shared.recordTick()
        ghostty_app_tick(app)
    }

    /// Relative path under Application Support for the Boss embed override file.
    static let embedOverrideConfigRelativePath = "Boss/ghostty-embed-overrides.config"

    /// Contents of the Boss embed override file. Exposed for tests that pin
    /// the contract without standing up libghostty.
    static let embedOverrideConfigContents = """
        # Regenerated on every launch — edits here are discarded. window-vsync must stay false; see GhosttyRuntime.applyBossEmbedConfigOverrides.
        window-vsync = false

        """

    /// Whether the last bootstrap successfully wrote and loaded the embed
    /// override that forces `window-vsync = false`. Surfaced on every
    /// `HostDisplaySnapshot` / `surface_failed` host object so a silent
    /// override failure is visible from `bossctl logs spawn` without
    /// unified logging.
    ///
    /// Written once during `GhosttyRuntime` init (main thread) and then only
    /// read from surface-failure paths; `nonisolated(unsafe)` matches other
    /// bootstrap flags that are set-before-use rather than concurrent.
    nonisolated(unsafe) private(set) static var embedVsyncOverrideApplied: Bool = false

    /// Load Boss-owned config keys last so they win over user Ghostty files.
    ///
    /// ## `window-vsync = false` — why this is the surface-creation fix
    ///
    /// GhosttyKit pin `5659cef` (prebuilt; we do not fork the library in-tree)
    /// initializes the Metal renderer with
    /// (`src/renderer/generic.zig` ~L692–697 on that commit):
    ///
    /// ```text
    /// if (options.config.vsync)
    ///     try macos.video.DisplayLink.createWithActiveCGDisplays()
    /// else
    ///     null
    /// ```
    ///
    /// `CVDisplayLinkCreateWithActiveCGDisplays` fails when CoreGraphics has
    /// zero *active* displays — the common case once the screen has been
    /// locked long enough for the display to sleep. The `try` aborts surface
    /// init, so `ghostty_surface_new` returns NULL and every worker spawn
    /// NACKs. That guard is incidental, not essential: the PTY, shell, and
    /// Metal device do not need a display link.
    ///
    /// On the same pin, when vsync is false `display_link` is null and
    /// `hasVsync()` is false (`generic.zig` ~L1020–1024), so
    /// `renderer.Thread.drawFrame` is not gated on a display link
    /// (`Thread.zig` ~L504–515) — mailbox wake / draw_now still paints.
    /// Upstream ghostty#13639 later made DisplayLink creation optional even
    /// when vsync is on; until the prebuilt is bumped past that commit,
    /// forcing `window-vsync = false` skips the fatal call while keeping a
    /// working event-driven draw path that already existed on 5659cef.
    ///
    /// Worker panes do not need a 60 Hz vsync clock (they are often occluded
    /// or unfocused); event-driven redraws on terminal output are correct.
    /// Keeping vsync off also avoids the ~2 CVDisplayLink threads per surface
    /// that the UI performance audit flagged.
    ///
    /// Written to a stable path under Application Support so the override is
    /// inspectable on a misbehaving host (`cat …/ghostty-embed-overrides.config`).
    /// Failures to write/load do not abort bootstrap — `embedVsyncOverrideApplied`
    /// is left false and the next `surface_failed` host object records that.
    static func applyBossEmbedConfigOverrides(_ config: ghostty_config_t) {
        embedVsyncOverrideApplied = false
        let path: URL
        do {
            path = try writeEmbedOverrideConfigFile()
        } catch {
            ghosttyBootstrapLog.error(
                "failed to write ghostty embed overrides: \(String(describing: error), privacy: .public)"
            )
            return
        }

        let diagnosticsBefore = ghostty_config_diagnostics_count(config)
        path.path.withCString { cPath in
            ghostty_config_load_file(config, cPath)
        }
        let diagnosticsAfter = ghostty_config_diagnostics_count(config)
        if diagnosticsAfter > diagnosticsBefore {
            var messages: [String] = []
            for i in diagnosticsBefore..<diagnosticsAfter {
                let diag = ghostty_config_get_diagnostic(config, i)
                if let msg = diag.message {
                    messages.append(String(cString: msg))
                }
            }
            ghosttyBootstrapLog.error(
                "ghostty embed overrides produced config diagnostics at \(path.path, privacy: .public): \(messages.joined(separator: "; "), privacy: .public)"
            )
            return
        }

        embedVsyncOverrideApplied = true
        ghosttyBootstrapLog.info(
            "loaded ghostty embed overrides from \(path.path, privacy: .public) (window-vsync=false)"
        )
    }

    /// Write the embed override config file and return its URL.
    /// Throws on directory create or file write failure so the failure path
    /// is unit-testable without standing up libghostty.
    ///
    /// - Parameter applicationSupportDirectory: Override for tests; defaults
    ///   to the process Application Support directory.
    @discardableResult
    static func writeEmbedOverrideConfigFile(
        fileManager: FileManager = .default,
        contents: String = embedOverrideConfigContents,
        applicationSupportDirectory: URL? = nil
    ) throws -> URL {
        let path = embedOverrideConfigFilePath(
            fileManager: fileManager,
            applicationSupportDirectory: applicationSupportDirectory
        )
        try fileManager.createDirectory(
            at: path.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        try contents.write(to: path, atomically: true, encoding: .utf8)
        return path
    }

    /// Absolute path of the embed override config file.
    static func embedOverrideConfigFilePath(
        fileManager: FileManager = .default,
        applicationSupportDirectory: URL? = nil
    ) -> URL {
        let appSupport = applicationSupportDirectory
            ?? fileManager.urls(
                for: .applicationSupportDirectory,
                in: .userDomainMask
            ).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory(), isDirectory: true)
        return appSupport.appendingPathComponent(embedOverrideConfigRelativePath)
    }

    private func installObservers() {
        let center = NotificationCenter.default
        observers.append(
            center.addObserver(
                forName: NSApplication.didBecomeActiveNotification,
                object: nil,
                queue: .main
            ) { [weak self] _ in
                guard let self else { return }
                ghostty_app_set_focus(self.app, true)
            }
        )
        observers.append(
            center.addObserver(
                forName: NSApplication.didResignActiveNotification,
                object: nil,
                queue: .main
            ) { [weak self] _ in
                guard let self else { return }
                ghostty_app_set_focus(self.app, false)
            }
        )

        // Sleep/wake: neither notification alone covers every case —
        // `didWakeNotification` fires on full-system wake (lid open,
        // `pmset sleepnow`), `screensDidWakeNotification` fires when
        // displays that slept independently (idle display sleep) wake —
        // so both drive the same handler. A full-system wake commonly
        // fires both; the handler is idempotent (broadcast + one engine
        // notification), so a duplicate is harmless.
        let workspaceCenter = NSWorkspace.shared.notificationCenter
        workspaceObservers.append(
            workspaceCenter.addObserver(
                forName: NSWorkspace.didWakeNotification,
                object: nil,
                queue: .main
            ) { [weak self] _ in
                self?.handleDisplaysDidWake()
            }
        )
        workspaceObservers.append(
            workspaceCenter.addObserver(
                forName: NSWorkspace.screensDidWakeNotification,
                object: nil,
                queue: .main
            ) { [weak self] _ in
                self?.handleDisplaysDidWake()
            }
        )
    }

    private func handleDisplaysDidWake() {
        NotificationCenter.default.post(name: .ghosttyDisplaysDidWake, object: nil)
        onDisplaysDidWake?()
    }

    fileprivate static func runtime(from userdata: UnsafeMutableRawPointer?) -> GhosttyRuntime? {
        guard let userdata else { return nil }
        return Unmanaged<GhosttyRuntime>.fromOpaque(userdata).takeUnretainedValue()
    }

    private static func hostView(from userdata: UnsafeMutableRawPointer?) -> GhosttyTerminalHostView? {
        guard let userdata else { return nil }
        return Unmanaged<GhosttyTerminalHostView>.fromOpaque(userdata).takeUnretainedValue()
    }

    private static func hostView(for target: ghostty_target_s) -> GhosttyTerminalHostView? {
        guard target.tag == GHOSTTY_TARGET_SURFACE else { return nil }
        guard let userdata = ghostty_surface_userdata(target.target.surface) else { return nil }
        return Unmanaged<GhosttyTerminalHostView>.fromOpaque(userdata).takeUnretainedValue()
    }

    private static func string(_ value: UnsafePointer<CChar>?) -> String {
        guard let value else { return "" }
        return String(cString: value)
    }

    fileprivate static func wakeup(_ userdata: UnsafeMutableRawPointer?) {
        guard let runtime = runtime(from: userdata) else { return }
        // Count every libghostty wakeup for the terminal event-loop
        // diagnostics. A surface IO loop spinning on a dead fd floods the
        // app mailbox and drives this callback, so a hot wakeup rate is
        // the Swift-side signature of the spin (see [[TerminalLoopMonitor]]).
        // The increment is an unfair-lock bump — negligible next to the
        // main-queue hop this path already performs.
        TerminalLoopMonitor.shared.recordWakeup()
        OperationQueue.main.addOperation {
            MainActor.assumeIsolated {
                runtime.tick()
            }
        }
    }

    fileprivate static func action(target: ghostty_target_s, action: ghostty_action_s) -> Bool {
        // Two-phase handling: *resolve* now (on whatever thread libghostty
        // called us from), *apply* later (on the main runloop).
        //
        // The `action` struct contains raw C pointers into memory
        // libghostty owns (e.g. `set_title.title`, `pwd.pwd`, `open_url.url`).
        // Those pointers are valid only for the duration of this callback —
        // libghostty's `performPreAction` in `apprt/embedded.zig` literally
        // `alloc.free()`s previous values when a follow-up action arrives.
        // The previous code deferred the *whole* action onto the main queue,
        // which let the pointers go stale before they were deref'd, producing
        // `EXC_BAD_ACCESS / EXC_ARM_DA_ALIGN` crashes inside `String(cString:)`
        // (fixed in PR #209 by switching to synchronous handling). So we
        // still read every pointer synchronously here — `resolve` copies the
        // C strings into owned Swift `String`s and resolves the target host
        // view before this callback returns. The deferred `apply` closure
        // touches *no* libghostty memory, so the stale-pointer hazard cannot
        // recur.
        //
        // Why defer `apply` at all, rather than handle inline as PR #209 did:
        //
        //  1. Off-main safety. libghostty *normally* invokes this from the
        //     macOS main runloop, but "normally" is not "always": a surface
        //     action delivered from a background (renderer / IO) thread would
        //     make a blind `MainActor.assumeIsolated` trip the fatal
        //     libdispatch main-thread assertion and `abort()` the process —
        //     issue #799, observed on background thread `114b158` during a
        //     Metal-renderer storm. Hopping to main *synchronously* would
        //     avoid the abort but risks deadlock (the renderer thread can be
        //     blocked inside a `ghostty_surface_*` call the main thread is
        //     simultaneously driving). An async hop is safe on any thread.
        //
        //  2. No publishing from within view updates. `apply` mutates
        //     `@Published` state on the pane's `TerminalPaneSession`. libghostty
        //     re-enters this callback synchronously from inside the
        //     `ghostty_surface_*` calls that `GhosttyTerminalView.updateNSView`
        //     makes during the SwiftUI view-update pass; mutating observed
        //     state there is the "Publishing changes from within view updates"
        //     violation (issue #799's runaway warning storm). Deferring the
        //     mutation to a fresh main-runloop turn moves it out of the update
        //     pass. Ordering is preserved: actions apply FIFO on the main queue.
        let resolved = resolve(target: target, action: action)
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                apply(resolved)
            }
        }
        return true
    }

    /// A surface action resolved into owned Swift values on the calling
    /// thread, so `apply` can run on a later main-runloop turn without
    /// dereferencing libghostty-owned pointers that are freed the moment
    /// this callback returns.
    ///
    /// `@unchecked Sendable`: the only reference it carries is a
    /// `GhosttyTerminalHostView` (an NSView), created and torn down on the
    /// main thread and kept alive for the app's lifetime; it is only ever
    /// read back inside `apply`, which runs on the main actor. The C
    /// payloads (sizes, color, mouse shape) are plain scalar structs.
    private struct ResolvedAction: @unchecked Sendable {
        enum Kind {
            case setTitle(String)
            case setWorkingDirectory(String)
            case rendererHealth(Bool)
            case mouseShape(ghostty_action_mouse_shape_e)
            case mouseVisibility(Bool)
            case initialSize(ghostty_action_initial_size_s)
            case cellSize(ghostty_action_cell_size_s)
            case colorChange(ghostty_action_color_change_s)
            case ringBell
            case openURL(String)
            case childExited(UInt32)
            case ignored
        }

        let host: GhosttyTerminalHostView?
        let kind: Kind
    }

    /// Reads every libghostty-owned pointer in `action` into owned Swift
    /// values. Safe to call from any thread — it only reads memory that is
    /// live for the duration of the action callback and touches no
    /// main-actor state.
    private static func resolve(target: ghostty_target_s, action: ghostty_action_s) -> ResolvedAction {
        let host = hostView(for: target)
        let kind: ResolvedAction.Kind = switch action.tag {
        case GHOSTTY_ACTION_SET_TITLE:
            .setTitle(string(action.action.set_title.title))
        case GHOSTTY_ACTION_PWD:
            .setWorkingDirectory(string(action.action.pwd.pwd))
        case GHOSTTY_ACTION_RENDERER_HEALTH:
            .rendererHealth(action.action.renderer_health == GHOSTTY_RENDERER_HEALTH_HEALTHY)
        case GHOSTTY_ACTION_MOUSE_SHAPE:
            .mouseShape(action.action.mouse_shape)
        case GHOSTTY_ACTION_MOUSE_VISIBILITY:
            .mouseVisibility(action.action.mouse_visibility == GHOSTTY_MOUSE_VISIBLE)
        case GHOSTTY_ACTION_INITIAL_SIZE:
            .initialSize(action.action.initial_size)
        case GHOSTTY_ACTION_CELL_SIZE:
            .cellSize(action.action.cell_size)
        case GHOSTTY_ACTION_COLOR_CHANGE:
            .colorChange(action.action.color_change)
        case GHOSTTY_ACTION_RING_BELL:
            .ringBell
        case GHOSTTY_ACTION_OPEN_URL:
            .openURL(string(action.action.open_url.url))
        case GHOSTTY_ACTION_SHOW_CHILD_EXITED:
            .childExited(action.action.child_exited.exit_code)
        default:
            .ignored
        }
        return ResolvedAction(host: host, kind: kind)
    }

    @MainActor
    private static func apply(_ resolved: ResolvedAction) {
        switch resolved.kind {
        case .setTitle(let title):
            resolved.host?.session.setTitle(title)

        case .setWorkingDirectory(let pwd):
            resolved.host?.session.workingDirectory = pwd

        case .rendererHealth(let healthy):
            resolved.host?.session.rendererHealthy = healthy

        case .mouseShape(let shape):
            resolved.host?.setCursorShape(shape)

        case .mouseVisibility(let visible):
            resolved.host?.setCursorVisible(visible)

        case .initialSize(let size):
            resolved.host?.applyInitialSize(size)

        case .cellSize(let size):
            resolved.host?.setCellSize(size)

        case .colorChange(let change):
            resolved.host?.applyColorChange(change)

        case .ringBell:
            NSSound.beep()

        case .openURL(let raw):
            if let url = URL(string: raw), url.scheme != nil {
                NSWorkspace.shared.open(url)
            } else {
                NSWorkspace.shared.open(URL(fileURLWithPath: raw))
            }

        case .childExited(let exitCode):
            // Suppress for the Boss pane, which handles its own restart
            // and shows "Picard restarting…" instead. Gated on role
            // rather than `onChildExited == nil` — worker panes now set
            // that closure too (to report pane death to the engine), so
            // a nil-check would wrongly suppress this message for them.
            if resolved.host?.session.role != .boss {
                resolved.host?.session.statusMessage = "Command exited (\(exitCode))"
            }

        case .ignored:
            break
        }
    }

    fileprivate static func readClipboard(
        _ userdata: UnsafeMutableRawPointer?,
        location _: ghostty_clipboard_e,
        state: UnsafeMutableRawPointer?
    ) -> Bool {
        guard let host = hostView(from: userdata) else { return false }
        // libghostty invokes the clipboard-read callback synchronously on the
        // main runloop, so the surface is already main-actor isolated here.
        // The opaque `state` token is round-tripped through an integer so the
        // non-Sendable raw pointer is not captured across the actor boundary.
        let stateAddress = UInt(bitPattern: state)
        return MainActor.assumeIsolated {
            guard let surface = host.surface else { return false }
            guard let text = NSPasteboard.general.string(forType: .string) else {
                return false
            }

            let statePointer = UnsafeMutableRawPointer(bitPattern: stateAddress)
            text.withCString { ptr in
                ghostty_surface_complete_clipboard_request(surface, ptr, statePointer, false)
            }
            return true
        }
    }

    fileprivate static func writeClipboard(
        _ userdata: UnsafeMutableRawPointer?,
        location _: ghostty_clipboard_e,
        content: UnsafePointer<ghostty_clipboard_content_s>?,
        len: Int,
        confirm _: Bool
    ) {
        guard hostView(from: userdata) != nil else { return }
        guard let content, len > 0 else { return }

        let pasteboard = NSPasteboard.general
        pasteboard.clearContents()

        for index in 0..<len {
            let item = content[index]
            guard let mime = item.mime, String(cString: mime) == "text/plain" else {
                continue
            }
            pasteboard.setString(String(cString: item.data), forType: .string)
            break
        }
    }

    /// Classify libghostty's close callback using the facts that actually
    /// describe a child exit. The callback's Boolean is `needsConfirmQuit()`
    /// in the bundled libghostty API; it is not process liveness.
    static func shouldReportChildExit(
        needsConfirmation: Bool,
        isCurrentAttachedSurface: Bool,
        isReleased: Bool,
        processExited: Bool
    ) -> Bool {
        !needsConfirmation && isCurrentAttachedSurface && !isReleased && processExited
    }

    fileprivate static func closeSurface(_ userdata: UnsafeMutableRawPointer?, needsConfirmation: Bool) {
        guard let host = hostView(from: userdata) else { return }
        OperationQueue.main.addOperation {
            MainActor.assumeIsolated {
                let isCurrentAttachedSurface = host.session.terminalReady && host.session.hostView === host
                let processExited = host.surface.map { ghostty_surface_process_exited($0) } ?? false
                guard Self.shouldReportChildExit(
                    needsConfirmation: needsConfirmation,
                    isCurrentAttachedSurface: isCurrentAttachedSurface,
                    isReleased: host.session.isReleased,
                    processExited: processExited
                ) else {
                    host.session.statusMessage = "Surface requested close"
                    return
                }
                if host.session.role == .boss, let onExit = host.session.onChildExited {
                    // Boss pane: delegate to the restart callback instead of
                    // showing a bare "Surface closed" message.
                    onExit()
                } else {
                    // Worker pane: show the closed status and report the
                    // death to the engine (if a handler is installed) so
                    // reconciliation fires immediately instead of waiting
                    // for the periodic dead-pid sweep.
                    host.session.statusMessage = "Surface closed"
                    host.session.onChildExited?()
                }
            }
        }
    }
}
