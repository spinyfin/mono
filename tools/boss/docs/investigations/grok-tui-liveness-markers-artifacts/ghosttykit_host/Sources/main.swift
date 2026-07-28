// Throwaway GhosttyKit embed host for Grok TUI liveness-marker capture.
// Investigation artifact only — NOT production Boss code.
//
// Mirrors Boss macOS embedding APIs:
//   bootstrap: ghostty_init / ghostty_config_new / ghostty_app_new
//   surface:   ghostty_surface_new with GHOSTTY_PLATFORM_MACOS + nsview
//   observe:   ghostty_surface_read_text (same path as Claude monitor scrape)
//   inject:    ghostty_surface_text + ghostty_surface_key Return
//
// Captures every viewport poll under SPIKE_PANE_MODE ∈ {no_alt, minimal, default}
// so marker stability can be measured across pane modes.

import AppKit
import Foundation
import GhosttyKit

// MARK: - Paths

enum Paths {
    static let hostDir = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()
        .deletingLastPathComponent()
    static let outDir = hostDir.appendingPathComponent("run_out")
    static let spikeRoot = URL(fileURLWithPath: "/tmp/grok-liveness-spike")
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
    /// Pane mode under test:
    ///   no_alt   → --no-alt-screen
    ///   minimal  → --minimal
    ///   default  → neither (fullscreen / alt-screen default)
    static let paneMode: String =
        ProcessInfo.processInfo.environment["SPIKE_PANE_MODE"] ?? "no_alt"
}

// MARK: - Logging

final class SpikeLog {
    private let handle: FileHandle
    private let lock = NSLock()
    let path: URL
    private let t0 = CFAbsoluteTimeGetCurrent()

    init(path: URL) throws {
        self.path = path
        FileManager.default.createFile(atPath: path.path, contents: nil)
        handle = try FileHandle(forWritingTo: path)
        line("=== ghosttykit_liveness start \(ISO8601DateFormatter().string(from: Date())) ===")
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
}

// MARK: - Ghostty callbacks

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
    _ = app; _ = target; _ = action
    return true
}

private func spikeReadClipboard(
    _ userdata: UnsafeMutableRawPointer?,
    _ location: ghostty_clipboard_e,
    _ state: UnsafeMutableRawPointer?
) -> Bool {
    _ = userdata; _ = location; _ = state
    return false
}

private func spikeWriteClipboard(
    _ userdata: UnsafeMutableRawPointer?,
    _ location: ghostty_clipboard_e,
    _ content: UnsafePointer<ghostty_clipboard_content_s>?,
    _ len: Int,
    _ confirm: Bool
) {
    _ = userdata; _ = location; _ = content; _ = len; _ = confirm
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

// MARK: - Host

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
    private var snapIndex = 0
    private let t0 = CFAbsoluteTimeGetCurrent()
    private var finished = false
    private var quitDone = false
    private var followUpDone = false
    private var sawSeedDone = false
    private var sawFollowUp = false
    private var longTurnSeen = false

    private let sessionId = UUID().uuidString.lowercased()

    // Candidate substrings counted per poll (stability stats).
    // Expanded from first GhosttyKit capture pass + Claude analogues; never assumed true a priori.
    private let candidates: [String] = [
        // presence / chrome
        "Grok",
        "Grok 4",
        "Grok 4.5",
        "always-approve",
        "Shift+Tab:mode",
        "Ctrl+.:shortcuts",
        "Ctrl+;:queue",
        "❯",
        "│ ❯",
        // starting
        "Starting session",
        "Starting session…",
        "Waiting for response",
        "Waiting for response…",
        "Do you trust the contents",
        "Accessing workspace",
        "Quick safety check",
        // busy / interrupt affordance
        "Esc:cancel",
        "Esc to cancel",
        "esc to interrupt",
        "Esc to interrupt",
        "to interrupt",
        "Ctrl+b:send to bg",
        "[stop]",
        "stop  [hooks",
        "Queued",
        "Queued · Enter to send now",
        // turn chrome
        "Thought for",
        "Thinking",
        "Worked for",
        "user_prompt_submit",
        "Resume this session",
        "K / 500K",
        "/ 500K",
        "◆",
        "hooks:",
        "permission",
        "bypassPermissions",
        // completion canaries (may also appear inside the user prompt text)
        "LIVE_SEED_DONE",
        "LIVE_FOLLOW_OK",
    ]

    private var candidateHits: [String: Int] = [:]
    private var candidateHitsWhileBusy: [String: Int] = [:]
    private var candidateHitsWhileIdle: [String: Int] = [:]
    private var candidateHitsWhileStart: [String: Int] = [:]
    private var pollCount = 0
    private var busyPolls = 0
    private var idlePolls = 0
    private var startPolls = 0
    private var phaseLog: [(t: Double, phase: String, notes: String)] = []
    private var lastPhase = "boot"

    init(log: SpikeLog, outDir: URL) {
        self.log = log
        self.outDir = outDir
        super.init()
        for c in Set(candidates) {
            candidateHits[c] = 0
            candidateHitsWhileBusy[c] = 0
            candidateHitsWhileIdle[c] = 0
            candidateHitsWhileStart[c] = 0
        }
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

        let frame = NSRect(x: 40, y: 40, width: 1100, height: 700)
        window = NSWindow(
            contentRect: frame,
            styleMask: [.titled, .closable, .resizable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = "ghosttykit_liveness (\(Paths.paneMode))"
        hostView = SpikeHostView(frame: frame)
        hostView.wantsLayer = true
        hostView.layer?.backgroundColor = NSColor.black.cgColor
        window.contentView = hostView
        window.orderFrontRegardless()

        let scriptPath = outDir.appendingPathComponent("pane_script.sh")
        try! sessionId.write(
            to: outDir.appendingPathComponent("session_id.txt"),
            atomically: true,
            encoding: .utf8
        )

        let modeFlag: String
        switch Paths.paneMode {
        case "minimal":
            modeFlag = "--minimal"
        case "default", "fullscreen", "alt":
            modeFlag = "" // default alt-screen / fullscreen chrome
        default:
            modeFlag = "--no-alt-screen"
        }

        let grok = Paths.grokBin
        let home = Paths.grokHome
        let cwd = Paths.cwd
        // Long enough tool turn that mid-turn chrome is scraped many times.
        // Followed by a short idle settle so prompt chrome is also captured.
        let seedPrompt =
            "Use the shell tool to run exactly: sleep 14. Do not skip the sleep. After it finishes reply with exactly: LIVE_SEED_DONE."

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
        print -r -- "pane_mode=\(Paths.paneMode)" > "\(outDir.path)/scenario.txt"
        print -r -- "mode_flag=\(modeFlag)" >> "\(outDir.path)/scenario.txt"
        print -r -- "sid=\(sessionId)" >> "\(outDir.path)/scenario.txt"
        print -r -- "grok-start $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "\(outDir.path)/timeline.txt"
        cd "\(cwd)"
        "\(grok)" \(modeFlag) --always-approve --trust --session-id "\(sessionId)" --cwd "\(cwd)" \
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
        log.line("pane_mode=\(Paths.paneMode) mode_flag=\(modeFlag) sid=\(sessionId)")
        log.line("initial_input=\(initialInput.trimmingCharacters(in: .newlines))")

        let snapsDir = outDir.appendingPathComponent("snaps")
        try? FileManager.default.createDirectory(at: snapsDir, withIntermediateDirectories: true)

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

        pollTimer = Timer.scheduledTimer(withTimeInterval: 0.35, repeats: true) { [weak self] _ in
            Task { @MainActor in
                self?.poll()
            }
        }
        // sleep 14 + tool overhead + follow-up + quit settle
        scheduleTimer = Timer.scheduledTimer(withTimeInterval: 110, repeats: false) { [weak self] _ in
            Task { @MainActor in
                self?.log.line("HARD_DEADLINE — finishing")
                self?.finish(reason: "deadline")
            }
        }
        log.line("timers armed; capturing liveness markers")
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
        try? "\(body)\n".write(
            to: outDir.appendingPathComponent("inject_\(label).txt"),
            atomically: true,
            encoding: .utf8
        )
    }

    /// Classify approximate phase from surface *chrome*, not from completion
    /// tokens (those also appear inside the user prompt text while the turn
    /// is still running).
    private func classifyPhase(viewport: String) -> String {
        let v = viewport
        let lower = v.lowercased()
        if v.contains("SCRIPT_DONE") || (lower.contains("resume this session with") && !v.contains("Esc:cancel")) {
            // After /quit the main buffer may still hold residual chrome.
            if lower.contains("resume this session") {
                return "post_exit"
            }
        }
        if lower.contains("do you trust the contents") {
            return "trust_dialog"
        }
        // Pre-TUI shell only.
        if !v.contains("Grok") && !v.contains("❯") && !v.contains("Starting session") {
            if v.count < 500 { return "starting_shell" }
        }
        // Explicit starting chrome.
        if v.contains("Starting session") {
            return "starting_tui"
        }
        // Busy chrome observed under GhosttyKit: footer Esc:cancel, waiting line,
        // tool spinner with [stop], send-to-bg affordance.
        let busyChrome =
            v.contains("Esc:cancel")
            || v.contains("Waiting for response")
            || v.contains("Ctrl+b:send to bg")
            || (v.contains("[stop]") && !v.contains("Worked for"))
        if busyChrome {
            longTurnSeen = true
            if followUpDone { return "busy_followup" }
            return "busy_seed"
        }
        // Idle: TUI present, no Esc:cancel, composer box visible.
        if v.contains("always-approve") || v.contains("Grok 4") || v.contains("│ ❯") {
            if sawSeedDone || v.contains("Worked for") {
                if followUpDone && sawFollowUp { return "idle_after_followup" }
                if followUpDone { return "idle_between_turns" }
                return "idle_after_seed"
            }
            // TUI up but first turn not clearly finished — could be very early.
            if v.contains("Starting session") { return "starting_tui" }
            if elapsed() < 4.0 { return "starting_tui" }
            return "idle_or_early"
        }
        return "unknown"
    }

    private func poll() {
        if finished { return }
        tick()
        guard surface != nil else { return }

        let fg = ghostty_surface_foreground_pid(surface)
        let exited = ghostty_surface_process_exited(surface)
        let viewport = readText(tag: GHOSTTY_POINT_VIEWPORT)
        let screen = readText(tag: GHOSTTY_POINT_SCREEN)
        let text = viewport + "\n" + screen

        pollCount += 1
        let phase = classifyPhase(viewport: viewport)
        if phase != lastPhase {
            phaseLog.append((t: elapsed(), phase: phase, notes: "bytes=\(viewport.count)"))
            log.line("PHASE \(lastPhase) → \(phase) t=\(String(format: "%.1f", elapsed())) bytes=\(viewport.count)")
            lastPhase = phase
        }

        // Bucket for stability counts
        let bucket: String
        if phase.hasPrefix("starting") || phase == "trust_dialog" {
            bucket = "start"
            startPolls += 1
        } else if phase.hasPrefix("busy") {
            bucket = "busy"
            busyPolls += 1
        } else if phase.hasPrefix("idle") {
            bucket = "idle"
            idlePolls += 1
        } else {
            bucket = "other"
        }

        for c in Set(candidates) {
            // Exact substring match on the combined surface text (viewport+screen).
            // Case-sensitive so we keep Esc:cancel distinct from hypothetical variants.
            let hit = text.contains(c)
            if hit {
                candidateHits[c, default: 0] += 1
                switch bucket {
                case "start": candidateHitsWhileStart[c, default: 0] += 1
                case "busy": candidateHitsWhileBusy[c, default: 0] += 1
                case "idle": candidateHitsWhileIdle[c, default: 0] += 1
                default: break
                }
            }
        }

        if viewport != lastViewportText {
            lastViewportText = viewport
            snapIndex += 1
            let name = String(
                format: "snap_%03d_t%05.1f_%@.txt",
                snapIndex,
                elapsed(),
                phase.replacingOccurrences(of: "/", with: "_")
            )
            let snapPath = outDir.appendingPathComponent("snaps").appendingPathComponent(name)
            try? viewport.write(to: snapPath, atomically: true, encoding: .utf8)
            try? viewport.write(
                to: outDir.appendingPathComponent("viewport_latest.txt"),
                atomically: true,
                encoding: .utf8
            )
            try? screen.write(
                to: outDir.appendingPathComponent("screen_latest.txt"),
                atomically: true,
                encoding: .utf8
            )
            let preview = viewport
                .replacingOccurrences(of: "\n", with: "\\n")
                .prefix(180)
            log.line(
                "snap=\(snapIndex) phase=\(phase) bytes=\(viewport.count) fg=\(fg) exited=\(exited) preview=\(preview)"
            )
        }

        // Completion canaries: require "Worked for" (turn footer) so we do not
        // treat the token inside the still-running user prompt as done.
        if !sawSeedDone, text.contains("Worked for"), text.contains("LIVE_SEED_DONE") {
            sawSeedDone = true
            log.line("OBSERVED seed turn complete (Worked for + LIVE_SEED_DONE)")
        }
        // Also accept Worked for after long sleep even if token scrolled away.
        if !sawSeedDone, text.contains("Worked for"), !text.contains("Esc:cancel"), elapsed() > 16 {
            sawSeedDone = true
            log.line("OBSERVED seed turn complete (Worked for, no Esc:cancel)")
        }
        if followUpDone, !sawFollowUp, text.contains("LIVE_FOLLOW_OK"), text.contains("Worked for") {
            sawFollowUp = true
            log.line("OBSERVED follow-up complete")
        }
        if followUpDone, !sawFollowUp, text.contains("LIVE_FOLLOW_OK"), !text.contains("Esc:cancel"),
           phase.hasPrefix("idle")
        {
            sawFollowUp = true
            log.line("OBSERVED follow-up complete (idle + token)")
        }

        // After true idle post-seed, inject a short follow-up turn.
        if sawSeedDone, !followUpDone, phase.hasPrefix("idle"), elapsed() > 18 {
            followUpDone = true
            sendToPane(
                "reply with exactly the single token: LIVE_FOLLOW_OK. no tools.",
                label: "followup"
            )
        }

        // Quit once follow-up settled idle, or after long capture window.
        if !quitDone {
            if sawFollowUp && phase.hasPrefix("idle") {
                quitDone = true
                Timer.scheduledTimer(withTimeInterval: 2.5, repeats: false) { [weak self] _ in
                    Task { @MainActor in
                        self?.sendToPane("/quit", label: "quit")
                        Timer.scheduledTimer(withTimeInterval: 4.0, repeats: false) { [weak self] _ in
                            Task { @MainActor in
                                self?.finish(reason: "post_followup_quit")
                            }
                        }
                    }
                }
            } else if sawSeedDone && followUpDone && elapsed() > 70 {
                quitDone = true
                sendToPane("/quit", label: "quit_timeout_followup")
                Timer.scheduledTimer(withTimeInterval: 4.0, repeats: false) { [weak self] _ in
                    Task { @MainActor in
                        self?.finish(reason: "followup_timeout")
                    }
                }
            } else if !sawSeedDone && elapsed() > 55 {
                // Capture window elapsed without clean idle — still quit with evidence.
                quitDone = true
                sendToPane("/quit", label: "quit_no_idle")
                Timer.scheduledTimer(withTimeInterval: 4.0, repeats: false) { [weak self] _ in
                    Task { @MainActor in
                        self?.finish(reason: "no_idle_timeout")
                    }
                }
            }
        }

        let scriptFinished = FileManager.default.fileExists(
            atPath: outDir.appendingPathComponent("script_finished.txt").path
        )
        if scriptFinished, !finished {
            log.line("pane script finished (grok exited)")
            Timer.scheduledTimer(withTimeInterval: 1.5, repeats: false) { [weak self] _ in
                Task { @MainActor in
                    self?.finish(reason: "script_finished")
                }
            }
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
        let grokExit = (try? String(
            contentsOf: outDir.appendingPathComponent("grok_exit.txt"),
            encoding: .utf8
        )) ?? "(absent)"
        let shellPid = (try? String(
            contentsOf: outDir.appendingPathComponent("shell_pid.txt"),
            encoding: .utf8
        )) ?? "(absent)"
        let version = (try? String(
            contentsOf: outDir.appendingPathComponent("grok_version.txt"),
            encoding: .utf8
        )) ?? "(absent)"

        // Stability table
        var rows: [String] = []
        rows.append(
            "candidate\ttotal_hits/\(pollCount)\tstart_hits/\(startPolls)\tbusy_hits/\(busyPolls)\tidle_hits/\(idlePolls)"
        )
        for c in candidates.sorted() {
            let t = candidateHits[c] ?? 0
            if t == 0 { continue }
            rows.append(
                "\(c)\t\(t)\t\(candidateHitsWhileStart[c] ?? 0)\t\(candidateHitsWhileBusy[c] ?? 0)\t\(candidateHitsWhileIdle[c] ?? 0)"
            )
        }
        let stability = rows.joined(separator: "\n") + "\n"
        try? stability.write(
            to: outDir.appendingPathComponent("marker_stability.tsv"),
            atomically: true,
            encoding: .utf8
        )

        var phases = "t\tphase\tnotes\n"
        for p in phaseLog {
            phases += "\(String(format: "%.1f", p.t))\t\(p.phase)\t\(p.notes)\n"
        }
        try? phases.write(
            to: outDir.appendingPathComponent("phases.tsv"),
            atomically: true,
            encoding: .utf8
        )

        let summary = """
        reason: \(reason)
        pane_mode: \(Paths.paneMode)
        session_id: \(sessionId)
        saw_LIVE_SEED_DONE: \(sawSeedDone)
        saw_LIVE_FOLLOW_OK: \(sawFollowUp)
        poll_count: \(pollCount)
        start_polls: \(startPolls)
        busy_polls: \(busyPolls)
        idle_polls: \(idlePolls)
        snap_count: \(snapIndex)
        viewport_final_bytes: \(viewport.count)
        screen_final_bytes: \(screen.count)
        grok_exit: \(grokExit.trimmingCharacters(in: .whitespacesAndNewlines))
        grok_version: \(version.trimmingCharacters(in: .whitespacesAndNewlines))
        shell_pid_file: \(shellPid.trimmingCharacters(in: .whitespacesAndNewlines))
        foreground_pid_at_end: \(fg)
        observe_api: ghostty_surface_read_text(GHOSTTY_POINT_VIEWPORT|SCREEN)
        inject_api: ghostty_surface_text + ghostty_surface_key(Return)
        ghosttykit_pin: ghosttykit-5659cef
        ghosttykit_sha256: 82b8d947484a21e1a9d186628b8af5e3f2e81dc96925f3cdbc1766ececa814a1
        GROK_HOME: \(Paths.grokHome)
        SPIKE_CWD: \(Paths.cwd)
        """
        try? summary.write(
            to: outDir.appendingPathComponent("SUMMARY.txt"),
            atomically: true,
            encoding: .utf8
        )

        let pins = """
        ghosttykit: ghosttykit-5659cef
        sha256: 82b8d947484a21e1a9d186628b8af5e3f2e81dc96925f3cdbc1766ececa814a1
        grok: \(version.trimmingCharacters(in: .whitespacesAndNewlines))
        pane_mode: \(Paths.paneMode)
        session_id: \(sessionId)
        apparatus: GhosttyKit embed host (not standalone Ghostty.app)
        """
        try? pins.write(
            to: outDir.appendingPathComponent("PINS.txt"),
            atomically: true,
            encoding: .utf8
        )

        log.line("SUMMARY written; exiting")
        log.line(summary)
        NSApp.terminate(nil)
    }
}

// MARK: - App entry

@main
enum LivenessMain {
    static func main() {
        let outDir = Paths.outDir
        try? FileManager.default.removeItem(at: outDir)
        try! FileManager.default.createDirectory(at: outDir, withIntermediateDirectories: true)
        let log = try! SpikeLog(path: outDir.appendingPathComponent("host.log"))

        let app = NSApplication.shared
        app.setActivationPolicy(.regular)

        let host = SpikeHost(log: log, outDir: outDir)
        // Retain for process lifetime.
        _ = Unmanaged.passRetained(host)

        DispatchQueue.main.async {
            host.start()
        }
        app.run()
    }
}
