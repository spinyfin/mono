// Throwaway GhosttyKit embed host for Grok pane viability — NOT production Boss code.
//
// Mirrors the Boss macOS app integration surface:
//   - bootstrap: ghostty_init / ghostty_config_new / ghostty_app_new
//   - surface:   ghostty_surface_new with GHOSTTY_PLATFORM_MACOS + nsview
//   - observe:   ghostty_surface_read_text (same path as Claude monitor scrape)
//   - inject:    ghostty_surface_text + ghostty_surface_key Return
//                (same path as GhosttyTerminalHostView.submitText / SendToPane)
//   - interrupt: ghostty_surface_key Esc (same path as host.sendInterrupt)
//
// Apparatus honesty:
//   This host owns the GhosttyKit surface in-process. It is the embedding
//   topology (Boss-app-like), not standalone Ghostty.app and not the
//   engine-only shell_pid outsider topology.

import AppKit
import Foundation
import GhosttyKit

// MARK: - Paths / pins

enum SpikePaths {
    static let hostDir = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()
        .deletingLastPathComponent()
    static let outDir = hostDir.appendingPathComponent("run_out")
    static let spikeRoot = URL(fileURLWithPath: "/tmp/grok-pane-spike")
    static let grokBin: String = {
        if let env = ProcessInfo.processInfo.environment["GROK_BIN"], !env.isEmpty {
            return env
        }
        let local = NSString(string: "~/.grok/bin/grok").expandingTildeInPath
        if FileManager.default.isExecutableFile(atPath: local) { return local }
        return "grok"
    }()
    static let grokHome: String = {
        if let env = ProcessInfo.processInfo.environment["GROK_HOME"], !env.isEmpty {
            return env
        }
        return spikeRoot.appendingPathComponent("home").path
    }()
    static let cwd: String = {
        if let env = ProcessInfo.processInfo.environment["SPIKE_CWD"], !env.isEmpty {
            return env
        }
        return spikeRoot.appendingPathComponent("cwd").path
    }()
    /// Scenario selector:
    ///   seed_observe (default) — positional prompt, observe marker, mid inject, probe, quit
    ///   esc_interrupt — long turn + Esc cancel mid-turn
    ///   alt_screen — default alt-screen (no --no-alt-screen)
    ///   resize — resize surface mid-session
    static let scenario: String =
        ProcessInfo.processInfo.environment["SPIKE_SCENARIO"] ?? "seed_observe"
}

// MARK: - Logging

final class SpikeLog {
    private let handle: FileHandle
    private let lock = NSLock()
    let path: URL

    init(path: URL) throws {
        self.path = path
        FileManager.default.createFile(atPath: path.path, contents: nil)
        handle = try FileHandle(forWritingTo: path)
        line("=== ghosttykit_grok_spike start \(ISO8601DateFormatter().string(from: Date())) ===")
    }

    func line(_ s: String) {
        lock.lock()
        defer { lock.unlock() }
        let row = "[\(String(format: "%.1f", CFAbsoluteTimeGetCurrent() - t0))] \(s)\n"
        if let data = row.data(using: .utf8) {
            handle.write(data)
        }
        fputs(row, stderr)
        fflush(stderr)
    }

    private let t0 = CFAbsoluteTimeGetCurrent()
}

// MARK: - Ghostty callbacks (C → Swift)

private func spikeWakeup(_ userdata: UnsafeMutableRawPointer?) {
    guard let userdata else { return }
    let bits = UInt(bitPattern: userdata)
    DispatchQueue.main.async {
        guard let ptr = UnsafeMutableRawPointer(bitPattern: bits) else { return }
        Unmanaged<SpikeHost>.fromOpaque(ptr).takeUnretainedValue().tick()
    }
}

private func spikeAction(
    _ app: ghostty_app_t?,
    _ target: ghostty_target_s,
    _ action: ghostty_action_s
) -> Bool {
    _ = app
    _ = target
    _ = action
    return true
}

private func spikeReadClipboard(
    _ userdata: UnsafeMutableRawPointer?,
    _ location: ghostty_clipboard_e,
    _ state: UnsafeMutableRawPointer?
) -> Bool {
    _ = userdata
    _ = location
    _ = state
    return false
}

private func spikeWriteClipboard(
    _ userdata: UnsafeMutableRawPointer?,
    _ location: ghostty_clipboard_e,
    _ content: UnsafePointer<ghostty_clipboard_content_s>?,
    _ len: Int,
    _ confirm: Bool
) {
    _ = userdata
    _ = location
    _ = content
    _ = len
    _ = confirm
}

private func spikeCloseSurface(_ userdata: UnsafeMutableRawPointer?, _ processAlive: Bool) {
    guard let userdata else { return }
    let bits = UInt(bitPattern: userdata)
    let alive = processAlive
    DispatchQueue.main.async {
        guard let ptr = UnsafeMutableRawPointer(bitPattern: bits) else { return }
        Unmanaged<SpikeHost>.fromOpaque(ptr).takeUnretainedValue()
            .log.line("close_surface_cb processAlive=\(alive)")
    }
}

// MARK: - Host view + surface owner

final class SpikeHostView: NSView {
    override var isFlipped: Bool { true }
    override var acceptsFirstResponder: Bool { true }
}

@MainActor
final class SpikeHost: NSObject {
    let log: SpikeLog
    let outDir: URL
    private var config: ghostty_config_t!
    private var app: ghostty_app_t!
    private var surface: ghostty_surface_t?
    private var hostView: SpikeHostView!
    private var window: NSWindow!
    private var pollTimer: Timer?
    private var scheduleTimer: Timer?

    private var lastViewportText = ""
    private var lastScreenText = ""
    private var viewportSnapshots: [(t: Double, n: Int, preview: String)] = []
    private let t0 = CFAbsoluteTimeGetCurrent()

    // Phase machine
    private var sawSeedMarker = false
    private var sawProbeMarker = false
    private var sawCancelled = false
    private var midInjectDone = false
    private var probeInjectDone = false
    private var escDone = false
    private var quitDone = false
    private var resizeDone = false
    private var longPromptDone = false
    private var finished = false

    private let sessionId: String = {
        // UUIDv4 is fine; -s requires a valid UUID that does not already exist.
        UUID().uuidString.lowercased()
    }()

    init(log: SpikeLog, outDir: URL) {
        self.log = log
        self.outDir = outDir
        super.init()
    }

    func start() {
        let initStatus = ghostty_init(
            UInt(CommandLine.argc),
            CommandLine.unsafeArgv
        )
        guard initStatus == GHOSTTY_SUCCESS else {
            fatalError("ghostty_init failed: \(initStatus)")
        }
        log.line("ghostty_init ok")

        guard let cfg = ghostty_config_new() else {
            fatalError("ghostty_config_new failed")
        }
        config = cfg
        ghostty_config_finalize(config)

        var runtimeConfig = ghostty_runtime_config_s(
            userdata: Unmanaged.passUnretained(self).toOpaque(),
            supports_selection_clipboard: false,
            wakeup_cb: spikeWakeup,
            action_cb: spikeAction,
            read_clipboard_cb: spikeReadClipboard,
            confirm_read_clipboard_cb: { _, _, _, _ in },
            write_clipboard_cb: spikeWriteClipboard,
            close_surface_cb: spikeCloseSurface
        )
        guard let appHandle = ghostty_app_new(&runtimeConfig, config) else {
            fatalError("ghostty_app_new failed")
        }
        app = appHandle
        ghostty_app_set_focus(app, true)
        log.line("ghostty_app_new ok")

        let frame = NSRect(x: 60, y: 60, width: 1000, height: 640)
        window = NSWindow(
            contentRect: frame,
            styleMask: [.titled, .closable, .resizable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = "ghosttykit_grok_spike (\(SpikePaths.scenario))"
        hostView = SpikeHostView(frame: frame)
        hostView.wantsLayer = true
        hostView.layer?.backgroundColor = NSColor.black.cgColor
        window.contentView = hostView
        // Keep window ordered but do NOT steal focus aggressively if possible;
        // still need a real window server surface for GhosttyKit.
        window.orderFrontRegardless()

        let scriptPath = outDir.appendingPathComponent("pane_script.sh")
        let sidPath = outDir.appendingPathComponent("session_id.txt")
        try! sessionId.write(to: sidPath, atomically: true, encoding: .utf8)

        let noAlt: Bool
        let seedPrompt: String
        switch SpikePaths.scenario {
        case "alt_screen":
            noAlt = false
            seedPrompt = "reply with exactly the single token: gkit-grok-ok. do not use tools."
        case "esc_interrupt":
            noAlt = true
            // Long-running turn so Esc has something to cancel.
            seedPrompt = "Use the shell tool to run: sleep 45. Do not skip the sleep. After it finishes reply with exactly: SLEEP_DONE."
        case "resize":
            noAlt = true
            seedPrompt = "reply with exactly the single token: gkit-grok-ok. do not use tools. Then wait for further instructions."
        default: // seed_observe
            noAlt = true
            seedPrompt = "reply with exactly the single token: gkit-grok-ok. do not use tools."
        }

        let altFlag = noAlt ? "--no-alt-screen" : ""
        let grok = SpikePaths.grokBin
        let home = SpikePaths.grokHome
        let cwd = SpikePaths.cwd
        // Boss shape: shell command composed, then typed into pane via initial_input.
        let script = """
        #!/bin/zsh
        set -u
        export GROK_HOME="\(home)"
        export PATH="\(URL(fileURLWithPath: grok).deletingLastPathComponent().path):$PATH"
        export NO_COLOR=
        echo $$ > "\(outDir.path)/shell_pid.txt"
        tty > "\(outDir.path)/tty.txt"
        print -r -- "GROK_BIN=\(grok)" > "\(outDir.path)/grok_bin.txt"
        "\(grok)" --version > "\(outDir.path)/grok_version.txt" 2>&1
        print -r -- "scenario=\(SpikePaths.scenario)" > "\(outDir.path)/scenario.txt"
        print -r -- "sid=\(sessionId)" >> "\(outDir.path)/scenario.txt"
        print -r -- "no_alt=\(noAlt)" >> "\(outDir.path)/scenario.txt"
        print -r -- "grok-start $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "\(outDir.path)/timeline.txt"
        cd "\(cwd)"
        # Interactive TUI with positional prompt (auto-submit shape, Claude-like).
        "\(grok)" \(altFlag) --always-approve --trust --session-id "\(sessionId)" --cwd "\(cwd)" \
          "\(seedPrompt)"
        print -r -- $? > "\(outDir.path)/grok_exit.txt"
        print -r -- "grok-exit $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "\(outDir.path)/timeline.txt"
        print -r -- "SCRIPT_DONE"
        date -u +%Y-%m-%dT%H:%M:%SZ > "\(outDir.path)/script_finished.txt"
        """
        try! script.write(to: scriptPath, atomically: true, encoding: .utf8)
        try! FileManager.default.setAttributes(
            [.posixPermissions: 0o755],
            ofItemAtPath: scriptPath.path
        )

        let initialInput = "zsh \(scriptPath.path)\n"
        log.line("scenario=\(SpikePaths.scenario) sid=\(sessionId)")
        log.line("initial_input=\(initialInput.trimmingCharacters(in: .newlines))")

        let surfaceHandle: ghostty_surface_t? = initialInput.withCString { inputPtr in
            outDir.path.withCString { cwdPtr in
                var surfaceConfig = ghostty_surface_config_new()
                surfaceConfig.platform_tag = GHOSTTY_PLATFORM_MACOS
                surfaceConfig.platform = ghostty_platform_u(
                    macos: ghostty_platform_macos_s(
                        nsview: Unmanaged.passUnretained(hostView).toOpaque()
                    )
                )
                surfaceConfig.userdata = Unmanaged.passUnretained(self).toOpaque()
                surfaceConfig.scale_factor = Double(NSScreen.main?.backingScaleFactor ?? 2.0)
                surfaceConfig.font_size = 13.0
                surfaceConfig.working_directory = cwdPtr
                surfaceConfig.initial_input = inputPtr
                surfaceConfig.env_vars = nil
                surfaceConfig.env_var_count = 0
                return ghostty_surface_new(app, &surfaceConfig)
            }
        }

        guard let surfaceHandle else {
            fatalError("ghostty_surface_new returned NULL")
        }
        surface = surfaceHandle
        log.line("ghostty_surface_new ok")

        let scale = hostView.window?.backingScaleFactor
            ?? NSScreen.main?.backingScaleFactor
            ?? 2.0
        let bounds = hostView.bounds
        ghostty_surface_set_size(
            surfaceHandle,
            UInt32(bounds.width * scale),
            UInt32(bounds.height * scale)
        )
        ghostty_surface_set_content_scale(surfaceHandle, scale, scale)
        ghostty_surface_set_focus(surfaceHandle, true)
        ghostty_app_tick(app)

        pollTimer = Timer.scheduledTimer(withTimeInterval: 0.4, repeats: true) { [weak self] _ in
            Task { @MainActor in
                self?.poll()
            }
        }
        let deadline: TimeInterval
        switch SpikePaths.scenario {
        case "esc_interrupt": deadline = 75
        default: deadline = 90
        }
        scheduleTimer = Timer.scheduledTimer(withTimeInterval: deadline, repeats: false) { [weak self] _ in
            Task { @MainActor in
                self?.log.line("HARD_DEADLINE — finishing")
                self?.finish(reason: "deadline")
            }
        }
        log.line("timers armed; waiting for grok in embed surface")
    }

    func tick() {
        ghostty_app_tick(app)
    }

    private func elapsed() -> Double {
        CFAbsoluteTimeGetCurrent() - t0
    }

    private func readText(tag: ghostty_point_tag_e) -> String {
        guard let surface else { return "" }
        var text = ghostty_text_s()
        let selection = ghostty_selection_s(
            top_left: ghostty_point_s(
                tag: tag,
                coord: GHOSTTY_POINT_COORD_TOP_LEFT,
                x: 0,
                y: 0
            ),
            bottom_right: ghostty_point_s(
                tag: tag,
                coord: GHOSTTY_POINT_COORD_BOTTOM_RIGHT,
                x: 0,
                y: 0
            ),
            rectangle: false
        )
        guard ghostty_surface_read_text(surface, selection, &text) else {
            return ""
        }
        defer { ghostty_surface_free_text(surface, &text) }
        guard let ptr = text.text else { return "" }
        return String(cString: ptr)
    }

    private func sendToPane(_ raw: String, label: String) {
        guard let surface else {
            log.line("sendToPane(\(label)): no surface")
            return
        }
        var body = raw
        while body.last == "\n" || body.last == "\r" {
            body.removeLast()
        }
        log.line("INJECT \(label) body=\(body)")
        if !body.isEmpty {
            body.withCString { ptr in
                ghostty_surface_text(surface, ptr, UInt(strlen(ptr)))
            }
        }
        var keyEvent = ghostty_input_key_s()
        keyEvent.action = GHOSTTY_ACTION_PRESS
        keyEvent.mods = GHOSTTY_MODS_NONE
        keyEvent.consumed_mods = GHOSTTY_MODS_NONE
        keyEvent.keycode = 0x24 // kVK_Return
        keyEvent.text = nil
        keyEvent.composing = false
        keyEvent.unshifted_codepoint = 0x0D
        _ = ghostty_surface_key(surface, keyEvent)
        log.line("INJECT \(label) Return sent")
        let rec = outDir.appendingPathComponent("inject_\(label).txt")
        try? "\(body)\n".write(to: rec, atomically: true, encoding: .utf8)
    }

    private func sendEsc(label: String) {
        guard let surface else {
            log.line("sendEsc(\(label)): no surface")
            return
        }
        log.line("INTERRUPT Esc (\(label))")
        var keyEvent = ghostty_input_key_s()
        keyEvent.action = GHOSTTY_ACTION_PRESS
        keyEvent.mods = GHOSTTY_MODS_NONE
        keyEvent.consumed_mods = GHOSTTY_MODS_NONE
        keyEvent.keycode = 0x35 // kVK_Escape
        keyEvent.text = nil
        keyEvent.composing = false
        keyEvent.unshifted_codepoint = 0x1B
        _ = ghostty_surface_key(surface, keyEvent)
        // Also send key-up for completeness (Boss may only press).
        keyEvent.action = GHOSTTY_ACTION_RELEASE
        _ = ghostty_surface_key(surface, keyEvent)
        try? "esc \(label) t=\(elapsed())\n".write(
            to: outDir.appendingPathComponent("esc_\(label).txt"),
            atomically: true,
            encoding: .utf8
        )
    }

    private func combinedText(viewport: String, screen: String) -> String {
        viewport + "\n" + screen
    }

    private func poll() {
        if finished { return }
        tick()
        guard let surface else { return }

        let fg = ghostty_surface_foreground_pid(surface)
        let exited = ghostty_surface_process_exited(surface)
        let viewport = readText(tag: GHOSTTY_POINT_VIEWPORT)
        let screen = readText(tag: GHOSTTY_POINT_SCREEN)
        let text = combinedText(viewport: viewport, screen: screen)

        if viewport != lastViewportText {
            lastViewportText = viewport
            let preview = viewport
                .replacingOccurrences(of: "\n", with: "\\n")
                .prefix(200)
            viewportSnapshots.append((t: elapsed(), n: viewport.count, preview: String(preview)))
            log.line("viewport_bytes=\(viewport.count) fg_pid=\(fg) exited=\(exited) preview=\(preview)")
            try? viewport.write(
                to: outDir.appendingPathComponent("viewport_latest.txt"),
                atomically: true,
                encoding: .utf8
            )
        }
        if screen != lastScreenText {
            lastScreenText = screen
            try? screen.write(
                to: outDir.appendingPathComponent("screen_latest.txt"),
                atomically: true,
                encoding: .utf8
            )
        }

        // Markers
        if !sawSeedMarker, text.localizedCaseInsensitiveContains("gkit-grok-ok") {
            sawSeedMarker = true
            log.line("OBSERVED gkit-grok-ok in surface text")
        }
        if !sawProbeMarker, text.contains("GKIT_PROBE_OK") {
            sawProbeMarker = true
            log.line("OBSERVED GKIT_PROBE_OK in surface text")
        }
        if !sawCancelled,
           text.localizedCaseInsensitiveContains("cancelled")
            || text.localizedCaseInsensitiveContains("canceled")
            || text.localizedCaseInsensitiveContains("interrupted")
            || text.localizedCaseInsensitiveContains("stopped") {
            // Soft signal — log only; do not overclaim
            log.line("OBSERVED possible-cancel language in surface text")
            sawCancelled = true
        }

        switch SpikePaths.scenario {
        case "esc_interrupt":
            handleEscScenario(text: text)
        case "resize":
            handleResizeScenario(text: text)
        case "alt_screen":
            handleSeedScenario(text: text, midInject: false)
        default:
            handleSeedScenario(text: text, midInject: true)
        }

        let scriptFinished = FileManager.default.fileExists(
            atPath: outDir.appendingPathComponent("script_finished.txt").path
        )
        if scriptFinished, !quitDone {
            log.line("pane script finished (grok exited)")
            // Allow a beat for any buffered side effect, then finish.
            quitDone = true
            Timer.scheduledTimer(withTimeInterval: 2.0, repeats: false) { [weak self] _ in
                Task { @MainActor in
                    self?.finish(reason: "script_finished")
                }
            }
        }
    }

    private func handleSeedScenario(text: String, midInject: Bool) {
        // After seed marker visible, inject a follow-up probe via SendToPane path.
        if sawSeedMarker, !probeInjectDone, elapsed() >= 3.0 {
            // Wait a bit after first observation so the TUI settles at idle prompt.
            if elapsed() >= 8.0 || text.localizedCaseInsensitiveContains("gkit-grok-ok") {
                probeInjectDone = true
                sendToPane(
                    "reply with exactly the single token: GKIT_PROBE_OK. no tools.",
                    label: "probe"
                )
            }
        }

        // Optional mid-turn inject: if we fire a long prompt, inject shell command.
        // For seed_observe we inject a shell-side-effect command shortly after probe
        // while a second turn might still be running — or at idle, then after /quit.
        if midInject, sawProbeMarker, !midInjectDone {
            midInjectDone = true
            let side = outDir.appendingPathComponent("injected_side_effect.txt").path
            // This is typed into the TUI composer — if idle, Grok treats it as a prompt
            // (agent may run shell); if mid-turn, behavior is under test.
            sendToPane(
                "Use the shell tool to run exactly: echo GKIT_INJECT_VIA_SURFACE_TEXT > \(side) ; then reply DONE.",
                label: "mid_or_idle_tool"
            )
        }

        // Quit once we have seed + probe (or timeout path).
        if sawSeedMarker, sawProbeMarker, midInjectDone || !midInject, !quitDone, elapsed() > 25 {
            quitDone = true
            // Give tool inject a few seconds, then /quit
            Timer.scheduledTimer(withTimeInterval: 12.0, repeats: false) { [weak self] _ in
                Task { @MainActor in
                    guard let self else { return }
                    self.sendToPane("/quit", label: "quit")
                    Timer.scheduledTimer(withTimeInterval: 5.0, repeats: false) { [weak self] _ in
                        Task { @MainActor in
                            self?.finish(reason: "post_quit_window")
                        }
                    }
                }
            }
        }

        // If seed only and no midInject (alt_screen), quit after observe.
        if !midInject, sawSeedMarker, !quitDone, elapsed() > 12 {
            quitDone = true
            sendToPane("/quit", label: "quit")
            Timer.scheduledTimer(withTimeInterval: 5.0, repeats: false) { [weak self] _ in
                Task { @MainActor in
                    self?.finish(reason: "post_quit_window")
                }
            }
        }
    }

    private func handleEscScenario(text: String) {
        // Wait until grok has started the long turn (tool or thinking visible), then Esc.
        let looksBusy =
            text.localizedCaseInsensitiveContains("sleep")
            || text.localizedCaseInsensitiveContains("run_terminal")
            || text.localizedCaseInsensitiveContains("tool")
            || text.localizedCaseInsensitiveContains("running")
            || elapsed() >= 8.0
        if looksBusy, !escDone, elapsed() >= 6.0 {
            escDone = true
            sendEsc(label: "mid_turn")
            // After cancel settles, ask for a marker then quit.
            Timer.scheduledTimer(withTimeInterval: 4.0, repeats: false) { [weak self] _ in
                Task { @MainActor in
                    guard let self else { return }
                    self.sendToPane(
                        "reply with exactly the single token: ESC_AFTER_OK. no tools.",
                        label: "post_esc_probe"
                    )
                    Timer.scheduledTimer(withTimeInterval: 20.0, repeats: false) { [weak self] _ in
                        Task { @MainActor in
                            guard let self else { return }
                            self.sendToPane("/quit", label: "quit")
                            Timer.scheduledTimer(withTimeInterval: 5.0, repeats: false) { [weak self] _ in
                                Task { @MainActor in
                                    self?.finish(reason: "post_esc_quit")
                                }
                            }
                        }
                    }
                }
            }
        }
        if text.contains("ESC_AFTER_OK") {
            log.line("OBSERVED ESC_AFTER_OK in surface text")
        }
        if text.contains("SLEEP_DONE") {
            log.line("OBSERVED SLEEP_DONE (Esc may have failed to cancel)")
        }
    }

    private func handleResizeScenario(text: String) {
        if sawSeedMarker, !resizeDone, elapsed() >= 6.0 {
            resizeDone = true
            guard let surface else { return }
            let scale = hostView.window?.backingScaleFactor
                ?? NSScreen.main?.backingScaleFactor
                ?? 2.0
            // Shrink then grow — Boss panes get resized by the layout.
            let small = NSSize(width: 500, height: 320)
            let large = NSSize(width: 1100, height: 700)
            log.line("RESIZE small \(small)")
            hostView.setFrameSize(small)
            window.setContentSize(small)
            ghostty_surface_set_size(
                surface,
                UInt32(small.width * scale),
                UInt32(small.height * scale)
            )
            Timer.scheduledTimer(withTimeInterval: 2.0, repeats: false) { [weak self] _ in
                Task { @MainActor in
                    guard let self, let surface = self.surface else { return }
                    self.log.line("RESIZE large \(large)")
                    self.hostView.setFrameSize(large)
                    self.window.setContentSize(large)
                    ghostty_surface_set_size(
                        surface,
                        UInt32(large.width * scale),
                        UInt32(large.height * scale)
                    )
                    // Capture post-resize surface text
                    Timer.scheduledTimer(withTimeInterval: 2.0, repeats: false) { [weak self] _ in
                        Task { @MainActor in
                            guard let self else { return }
                            let v = self.readText(tag: GHOSTTY_POINT_VIEWPORT)
                            try? v.write(
                                to: self.outDir.appendingPathComponent("viewport_post_resize.txt"),
                                atomically: true,
                                encoding: .utf8
                            )
                            self.sendToPane(
                                "reply with exactly: RESIZE_OK. no tools.",
                                label: "post_resize_probe"
                            )
                            Timer.scheduledTimer(withTimeInterval: 15.0, repeats: false) { [weak self] _ in
                                Task { @MainActor in
                                    guard let self else { return }
                                    self.sendToPane("/quit", label: "quit")
                                    Timer.scheduledTimer(withTimeInterval: 5.0, repeats: false) { [weak self] _ in
                                        Task { @MainActor in
                                            self?.finish(reason: "post_resize_quit")
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if text.contains("RESIZE_OK") {
            log.line("OBSERVED RESIZE_OK in surface text")
        }
    }

    private func finish(reason: String) {
        if finished { return }
        finished = true
        pollTimer?.invalidate()
        scheduleTimer?.invalidate()
        log.line("finish reason=\(reason)")

        let viewport = readText(tag: GHOSTTY_POINT_VIEWPORT)
        let screen = readText(tag: GHOSTTY_POINT_SCREEN)
        try? viewport.write(
            to: outDir.appendingPathComponent("viewport_final.txt"),
            atomically: true,
            encoding: .utf8
        )
        try? screen.write(
            to: outDir.appendingPathComponent("screen_final.txt"),
            atomically: true,
            encoding: .utf8
        )

        let fg = surface.map { ghostty_surface_foreground_pid($0) } ?? 0
        let sideMid = (try? String(
            contentsOf: outDir.appendingPathComponent("injected_side_effect.txt"),
            encoding: .utf8
        )) ?? "(absent)"
        let grokExit = (try? String(
            contentsOf: outDir.appendingPathComponent("grok_exit.txt"),
            encoding: .utf8
        )) ?? "(absent)"
        let shellPid = (try? String(
            contentsOf: outDir.appendingPathComponent("shell_pid.txt"),
            encoding: .utf8
        )) ?? "(absent)"
        let tty = (try? String(
            contentsOf: outDir.appendingPathComponent("tty.txt"),
            encoding: .utf8
        )) ?? "(absent)"
        let version = (try? String(
            contentsOf: outDir.appendingPathComponent("grok_version.txt"),
            encoding: .utf8
        )) ?? "(absent)"

        let text = viewport + "\n" + screen
        let summary = """
        reason: \(reason)
        scenario: \(SpikePaths.scenario)
        session_id: \(sessionId)
        saw_gkit_grok_ok: \(sawSeedMarker)
        saw_probe_ok: \(sawProbeMarker)
        saw_cancel_language: \(sawCancelled)
        viewport_final_bytes: \(viewport.count)
        screen_final_bytes: \(screen.count)
        viewport_contains_gkit_grok_ok: \(text.localizedCaseInsensitiveContains("gkit-grok-ok"))
        viewport_contains_probe: \(text.contains("GKIT_PROBE_OK"))
        viewport_contains_esc_after: \(text.contains("ESC_AFTER_OK"))
        viewport_contains_sleep_done: \(text.contains("SLEEP_DONE"))
        viewport_contains_resize_ok: \(text.contains("RESIZE_OK"))
        mid_inject_side_effect: \(sideMid.trimmingCharacters(in: .whitespacesAndNewlines))
        grok_exit: \(grokExit.trimmingCharacters(in: .whitespacesAndNewlines))
        grok_version: \(version.trimmingCharacters(in: .whitespacesAndNewlines))
        shell_pid_file: \(shellPid.trimmingCharacters(in: .whitespacesAndNewlines))
        tty: \(tty.trimmingCharacters(in: .whitespacesAndNewlines))
        foreground_pid_at_end: \(fg)
        snapshot_count: \(viewportSnapshots.count)
        inject_api: ghostty_surface_text + ghostty_surface_key(Return)  # Boss SendToPane / submitText
        interrupt_api: ghostty_surface_key(Esc keycode 0x35)  # Boss sendInterrupt
        observe_api: ghostty_surface_read_text(GHOSTTY_POINT_VIEWPORT|SCREEN)  # Boss Claude monitor path
        """
        try? summary.write(
            to: outDir.appendingPathComponent("SUMMARY.txt"),
            atomically: true,
            encoding: .utf8
        )
        log.line("SUMMARY written:\n\(summary)")

        // Archive this scenario's run_out into evidence/<scenario>/
        let evidenceRoot = SpikePaths.hostDir.appendingPathComponent("evidence")
        let dest = evidenceRoot.appendingPathComponent(SpikePaths.scenario)
        try? FileManager.default.createDirectory(at: evidenceRoot, withIntermediateDirectories: true)
        try? FileManager.default.removeItem(at: dest)
        try? FileManager.default.copyItem(at: outDir, to: dest)

        if let surface {
            ghostty_surface_set_focus(surface, false)
            ghostty_surface_free(surface)
            self.surface = nil
        }
        ghostty_app_free(app)
        ghostty_config_free(config)

        DispatchQueue.main.async {
            NSApp.terminate(nil)
        }
    }
}

// MARK: - App entry

final class SpikeAppDelegate: NSObject, NSApplicationDelegate {
    var host: SpikeHost?
    var log: SpikeLog?

    func applicationDidFinishLaunching(_ notification: Notification) {
        let outDir = SpikePaths.outDir
        try? FileManager.default.removeItem(at: outDir)
        try! FileManager.default.createDirectory(at: outDir, withIntermediateDirectories: true)

        let pins = """
        host: ghosttykit_grok_spike (throwaway)
        scenario: \(SpikePaths.scenario)
        grok_bin: \(SpikePaths.grokBin)
        grok_home: \(SpikePaths.grokHome)
        cwd: \(SpikePaths.cwd)
        ghosttykit_prebuilt: ghosttykit-5659cef (spinyfin/ghostty-prebuilts)
        observe_api: ghostty_surface_read_text
        inject_api: ghostty_surface_text + ghostty_surface_key
        interrupt_api: ghostty_surface_key Esc
        date: \(ISO8601DateFormatter().string(from: Date()))
        """
        try? pins.write(
            to: outDir.appendingPathComponent("PINS.txt"),
            atomically: true,
            encoding: .utf8
        )

        let log = try! SpikeLog(path: outDir.appendingPathComponent("host.log"))
        self.log = log
        log.line(pins)

        let host = SpikeHost(log: log, outDir: outDir)
        self.host = host
        host.start()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}

let app = NSApplication.shared
let delegate = SpikeAppDelegate()
app.delegate = delegate
app.setActivationPolicy(.accessory) // avoid Dock bounce / aggressive focus if possible
app.run()
