// Throwaway GhosttyKit embed host — NOT production Boss code.
//
// Mirrors the Boss macOS app integration surface:
//   - bootstrap: ghostty_init / ghostty_config_new / ghostty_app_new
//   - surface:   ghostty_surface_new with GHOSTTY_PLATFORM_MACOS + nsview
//   - observe:   ghostty_surface_read_text (same path as Claude monitor scrape)
//   - inject:    ghostty_surface_text + ghostty_surface_key Return
//                (same path as GhosttyTerminalHostView.submitText / SendToPane)
//
// Apparatus honesty:
//   This host owns the GhosttyKit surface in-process. It is the embedding
//   topology (Boss-app-like), not the engine-only shell_pid outsider topology.

import AppKit
import Foundation
import GhosttyKit

// MARK: - Paths / pins

enum SpikePaths {
    static let hostDir = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()
        .deletingLastPathComponent()
    static let outDir = hostDir.appendingPathComponent("run_out")
    static let codexBin: String = {
        if let env = ProcessInfo.processInfo.environment["CODEX_BIN"], !env.isEmpty {
            return env
        }
        let local = NSString(string: "~/.local/bin/codex").expandingTildeInPath
        if FileManager.default.isExecutableFile(atPath: local) { return local }
        return "codex"
    }()
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
        line("=== ghosttykit_spike start \(ISO8601DateFormatter().string(from: Date())) ===")
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
    // Bit-pattern hop: Swift 6 won't let us capture UnsafeMutableRawPointer
    // into a main-actor closure; the host is main-thread-only by construction.
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
    // No-op: we only need surface IO. Returning true acknowledges.
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

    private var midInjectDone = false
    private var postExitInjectDone = false
    private var sawCodexDoneMarker = false
    private var sawThreadStarted = false
    private var lastViewportText = ""
    private var lastScreenText = ""
    private var viewportSnapshots: [(t: Double, n: Int, preview: String)] = []
    private let t0 = CFAbsoluteTimeGetCurrent()

    // Inject payload — side-effect file proves shell consumption if it lands.
    private lazy var injectCommand: String = {
        let side = outDir.appendingPathComponent("injected_side_effect.txt").path
        return "echo GKIT_INJECT_VIA_SURFACE_TEXT > \(side)"
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
        // Avoid loading the user's full ghostty config (could alter shell / keys).
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

        // Window + host NSView (required for GHOSTTY_PLATFORM_MACOS).
        let frame = NSRect(x: 80, y: 80, width: 900, height: 560)
        window = NSWindow(
            contentRect: frame,
            styleMask: [.titled, .closable, .resizable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = "ghosttykit_spike (throwaway)"
        hostView = SpikeHostView(frame: frame)
        hostView.wantsLayer = true
        hostView.layer?.backgroundColor = NSColor.black.cgColor
        window.contentView = hostView
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        // Build pane script that runs codex, then returns to the interactive shell.
        let scriptPath = outDir.appendingPathComponent("pane_script.sh")
        let codex = SpikePaths.codexBin
        let script = """
        #!/bin/zsh
        # GhosttyKit-owned pane script (throwaway spike).
        echo $$ > "\(outDir.path)/shell_pid.txt"
        tty > "\(outDir.path)/tty.txt"
        print -r -- "CODEX_BIN=\(codex)" > "\(outDir.path)/codex_bin.txt"
        "\(codex)" --version > "\(outDir.path)/codex_version.txt" 2>&1
        print -r -- "codex-start $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "\(outDir.path)/timeline.txt"
        "\(codex)" exec --json --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \\
          "run: sleep 18; reply with exactly: gkit-embed-done"
        print -r -- $? > "\(outDir.path)/codex_exit.txt"
        print -r -- "codex-exit $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "\(outDir.path)/timeline.txt"
        print -r -- "SCRIPT_DONE"
        date -u +%Y-%m-%dT%H:%M:%SZ > "\(outDir.path)/script_finished.txt"
        """
        try! script.write(to: scriptPath, atomically: true, encoding: .utf8)
        try! FileManager.default.setAttributes(
            [.posixPermissions: 0o755],
            ofItemAtPath: scriptPath.path
        )

        // Boss shape: default shell + initial_input runs the worker command.
        // Trailing newline submits to the interactive shell.
        let initialInput = "zsh \(scriptPath.path)\n"
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
                surfaceConfig.font_size = 12.0
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

        // Size the surface to the view.
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

        // Poll for observe + inject schedule (Timer fires on main runloop).
        pollTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            Task { @MainActor in
                self?.poll()
            }
        }
        // Hard deadline so we don't hang forever if codex stalls.
        scheduleTimer = Timer.scheduledTimer(withTimeInterval: 90, repeats: false) { [weak self] _ in
            Task { @MainActor in
                self?.log.line("HARD_DEADLINE — finishing")
                self?.finish(reason: "deadline")
            }
        }

        log.line("timers armed; waiting for codex in embed surface")
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

    /// Boss-equivalent SendToPane: paste body via ghostty_surface_text, then
    /// synthesise Return via ghostty_surface_key (matches submitText).
    private func sendToPane(_ raw: String, label: String) {
        guard let surface else {
            log.line("sendToPane(\(label)): no surface")
            return
        }
        // Strip trailing newlines the way Boss submissionPlan does; then Return.
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
        // Also dump a record file for the artifact index.
        let rec = outDir.appendingPathComponent("inject_\(label).txt")
        try? "\(body)\n".write(to: rec, atomically: true, encoding: .utf8)
    }

    private func poll() {
        tick()
        guard let surface else { return }

        let fg = ghostty_surface_foreground_pid(surface)
        let exited = ghostty_surface_process_exited(surface)
        let viewport = readText(tag: GHOSTTY_POINT_VIEWPORT)
        let screen = readText(tag: GHOSTTY_POINT_SCREEN)

        if viewport != lastViewportText {
            lastViewportText = viewport
            let preview = viewport
                .replacingOccurrences(of: "\n", with: "\\n")
                .prefix(180)
            viewportSnapshots.append((t: elapsed(), n: viewport.count, preview: String(preview)))
            log.line("viewport_bytes=\(viewport.count) fg_pid=\(fg) exited=\(exited) preview=\(preview)")
            // Persist latest full viewport for inspection.
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

        // Q1 markers: can the embedding process see codex JSONL / final text?
        if !sawThreadStarted,
           viewport.contains("thread.started")
            || screen.contains("thread.started")
            || viewport.contains("\"type\":\"thread.started\"")
            || screen.contains("\"type\":\"thread.started\"") {
            sawThreadStarted = true
            log.line("OBSERVED thread.started in surface text")
        }
        if !sawCodexDoneMarker,
           viewport.contains("gkit-embed-done") || screen.contains("gkit-embed-done") {
            sawCodexDoneMarker = true
            log.line("OBSERVED gkit-embed-done in surface text")
        }

        // Also detect via SCRIPT_DONE marker and codex_exit file.
        let codexExitPath = outDir.appendingPathComponent("codex_exit.txt")
        let scriptFinished = FileManager.default.fileExists(
            atPath: outDir.appendingPathComponent("script_finished.txt").path
        )
        let codexExitExists = FileManager.default.fileExists(atPath: codexExitPath.path)

        // Mid-run inject ~6s after start (codex sleeps 18s) — Boss SendToPane path.
        if !midInjectDone, elapsed() >= 6.0 {
            midInjectDone = true
            // Only inject while codex is likely still foreground.
            if !codexExitExists {
                sendToPane(injectCommand, label: "mid_codex")
            } else {
                log.line("skip mid inject — codex already exited")
            }
        }

        // After script finishes, wait a beat for interactive shell to consume
        // any leftover input buffer, then optionally post-exit inject.
        if scriptFinished, !postExitInjectDone, elapsed() >= 6.0 {
            // First: check whether mid inject produced a side effect via the
            // real interactive shell (not harness read/eval).
            let side = outDir.appendingPathComponent("injected_side_effect.txt")
            if FileManager.default.fileExists(atPath: side.path) {
                let body = (try? String(contentsOf: side, encoding: .utf8)) ?? ""
                log.line("SIDE_EFFECT after mid inject (interactive shell): \(body.trimmingCharacters(in: .whitespacesAndNewlines))")
            } else {
                log.line("no side-effect file yet after mid inject + script finish")
            }

            // Second inject after exit: does SendToPane work on idle shell?
            postExitInjectDone = true
            let postSide = outDir.appendingPathComponent("post_exit_side_effect.txt")
            let postCmd = "echo GKIT_POST_EXIT_INJECT > \(postSide.path)"
            sendToPane(postCmd, label: "post_exit")

            // Give the shell a couple seconds to run it, then finish.
            Timer.scheduledTimer(withTimeInterval: 3.0, repeats: false) { [weak self] _ in
                Task { @MainActor in
                    self?.finish(reason: "post_exit_window")
                }
            }
            return
        }

        // Fallback finish if we never see script_finished (e.g. shell hang).
        if sawCodexDoneMarker, elapsed() > 40, !postExitInjectDone {
            log.line("saw done marker but no script_finished yet; continuing")
        }
    }

    private func finish(reason: String) {
        pollTimer?.invalidate()
        scheduleTimer?.invalidate()
        log.line("finish reason=\(reason)")

        // Final reads.
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
        let sidePost = (try? String(
            contentsOf: outDir.appendingPathComponent("post_exit_side_effect.txt"),
            encoding: .utf8
        )) ?? "(absent)"
        let codexExit = (try? String(
            contentsOf: outDir.appendingPathComponent("codex_exit.txt"),
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

        let summary = """
        reason: \(reason)
        saw_thread_started: \(sawThreadStarted)
        saw_gkit_embed_done: \(sawCodexDoneMarker)
        viewport_final_bytes: \(viewport.count)
        screen_final_bytes: \(screen.count)
        viewport_contains_thread_started: \(viewport.contains("thread.started") || screen.contains("thread.started"))
        viewport_contains_gkit_embed_done: \(viewport.contains("gkit-embed-done") || screen.contains("gkit-embed-done"))
        viewport_contains_jsonl_turn: \(viewport.contains("turn.started") || screen.contains("turn.started"))
        mid_inject_side_effect: \(sideMid.trimmingCharacters(in: .whitespacesAndNewlines))
        post_exit_side_effect: \(sidePost.trimmingCharacters(in: .whitespacesAndNewlines))
        codex_exit: \(codexExit.trimmingCharacters(in: .whitespacesAndNewlines))
        shell_pid_file: \(shellPid.trimmingCharacters(in: .whitespacesAndNewlines))
        tty: \(tty.trimmingCharacters(in: .whitespacesAndNewlines))
        foreground_pid_at_end: \(fg)
        snapshot_count: \(viewportSnapshots.count)
        inject_api: ghostty_surface_text + ghostty_surface_key(Return)  # Boss SendToPane / submitText
        observe_api: ghostty_surface_read_text(GHOSTTY_POINT_VIEWPORT|SCREEN)  # Boss Claude monitor path
        """
        try? summary.write(
            to: outDir.appendingPathComponent("SUMMARY.txt"),
            atomically: true,
            encoding: .utf8
        )
        log.line("SUMMARY written:\n\(summary)")

        if let surface {
            ghostty_surface_set_focus(surface, false)
            ghostty_surface_free(surface)
            self.surface = nil
        }
        ghostty_app_free(app)
        ghostty_config_free(config)

        // Exit the app so the process terminates cleanly.
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

        // Stamp pins.
        let pins = """
        host: ghosttykit_spike (throwaway)
        codex_bin: \(SpikePaths.codexBin)
        ghosttykit_prebuilt: ghosttykit-5659cef (spinyfin/ghostty-prebuilts)
        observe_api: ghostty_surface_read_text
        inject_api: ghostty_surface_text + ghostty_surface_key
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

        // codex version probe
        let ver = Process()
        ver.executableURL = URL(fileURLWithPath: SpikePaths.codexBin)
        ver.arguments = ["--version"]
        let pipe = Pipe()
        ver.standardOutput = pipe
        ver.standardError = pipe
        try? ver.run()
        ver.waitUntilExit()
        let verOut = String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
        log.line("codex --version: \(verOut.trimmingCharacters(in: .whitespacesAndNewlines))")

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
app.setActivationPolicy(.regular)
app.run()
