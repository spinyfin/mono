// Throwaway GhosttyKit viewer host for the agy pane viability spike.
// NOT production Boss code.
//
// It reproduces the Boss macOS app's worker-pane embedding:
//   bootstrap : strip GHOSTTY_* env, ghostty_init, ghostty_config_new +
//               load_default_files + load_recursive_files + finalize,
//               ghostty_app_new                 (GhosttyRuntime / GhosttyBootstrap)
//   surface   : ghostty_surface_new with GHOSTTY_PLATFORM_MACOS + nsview and
//               `initial_input`                 (GhosttyTerminalHostView.makeSurface)
//   observe   : ghostty_surface_read_text(GHOSTTY_POINT_VIEWPORT)
//                                               (readVisibleContents -> PaneMonitorSnapshot)
//   type      : ghostty_surface_text + ghostty_surface_key(Return 0x24)
//                                               (submitText / sendReturnKey)
//   interrupt : ghostty_surface_key(Esc 0x35)
//
// The surface's shell is told what to run through `initial_input`, exactly as
// Boss does. For a worker pane that is `exec tmux -S <socket> attach-session
// -t <session>`; the worker process itself lives in the tmux session.
//
// The host is driven from outside through command files so a scenario script
// can sequence steps on evidence instead of on timers:
//   $VIEWER_OUT/cmd/<nnn>.cmd  one command per file:
//       text:<utf8>      ghostty_surface_text (the paste pathway)
//       key:return       Return press
//       key:esc          Escape press + release
//       submit:<utf8>    text + Return (Boss submitText)
//       resize:<W>x<H>   resize the host view and the surface, in points
//       focus:<0|1>      ghostty_surface_set_focus
//       snapshot:<name>  write viewport + screen text to snap_<name>.*.txt
//       quit             final snapshot, free the surface, exit

import AppKit
import Foundation
import GhosttyKit

let env = ProcessInfo.processInfo.environment

func envValue(_ key: String, _ fallback: String) -> String {
    if let value = env[key], !value.isEmpty { return value }
    return fallback
}

let outDir = URL(fileURLWithPath: envValue("VIEWER_OUT", FileManager.default.currentDirectoryPath + "/viewer_out"))
let cmdDir = outDir.appendingPathComponent("cmd")
let initialInput = envValue("VIEWER_INITIAL_INPUT", "")
let surfaceCwd = envValue("VIEWER_CWD", NSHomeDirectory())
let pollInterval = (Double(envValue("VIEWER_POLL_MS", "250")) ?? 250) / 1000.0
let showWindow = envValue("VIEWER_SHOW", "0") == "1"
let hardDeadline = Double(envValue("VIEWER_DEADLINE_S", "900")) ?? 900
let initialSize: NSSize = {
    let parts = envValue("VIEWER_SIZE", "1000x640").split(separator: "x").compactMap { Double($0) }
    return parts.count == 2 ? NSSize(width: parts[0], height: parts[1]) : NSSize(width: 1000, height: 640)
}()

final class HostLog {
    private let handle: FileHandle
    private let t0 = CFAbsoluteTimeGetCurrent()

    init(path: URL) {
        FileManager.default.createFile(atPath: path.path, contents: nil)
        handle = try! FileHandle(forWritingTo: path)
    }

    func elapsed() -> Double { CFAbsoluteTimeGetCurrent() - t0 }

    func line(_ text: String) {
        let row = String(format: "[%8.2f] ", elapsed()) + text + "\n"
        try? handle.write(contentsOf: Data(row.utf8))
    }
}

private func viewerWakeup(_ userdata: UnsafeMutableRawPointer?) {
    guard let userdata else { return }
    let bits = UInt(bitPattern: userdata)
    DispatchQueue.main.async {
        guard let pointer = UnsafeMutableRawPointer(bitPattern: bits) else { return }
        Unmanaged<ViewerHost>.fromOpaque(pointer).takeUnretainedValue().tick()
    }
}

private func viewerAction(_ app: ghostty_app_t?, _ target: ghostty_target_s, _ action: ghostty_action_s) -> Bool {
    false
}

private func viewerReadClipboard(
    _ userdata: UnsafeMutableRawPointer?,
    _ location: ghostty_clipboard_e,
    _ state: UnsafeMutableRawPointer?
) -> Bool {
    false
}

private func viewerWriteClipboard(
    _ userdata: UnsafeMutableRawPointer?,
    _ location: ghostty_clipboard_e,
    _ content: UnsafePointer<ghostty_clipboard_content_s>?,
    _ len: Int,
    _ confirm: Bool
) {}

private func viewerCloseSurface(_ userdata: UnsafeMutableRawPointer?, _ processAlive: Bool) {
    guard let userdata else { return }
    let bits = UInt(bitPattern: userdata)
    DispatchQueue.main.async {
        guard let pointer = UnsafeMutableRawPointer(bitPattern: bits) else { return }
        Unmanaged<ViewerHost>.fromOpaque(pointer).takeUnretainedValue().surfaceClosed(processAlive: processAlive)
    }
}

final class ViewerHostView: NSView {
    override var isFlipped: Bool { true }
    override var acceptsFirstResponder: Bool { true }
}

final class ViewerHost: NSObject {
    let log: HostLog
    private var config: ghostty_config_t!
    private var app: ghostty_app_t!
    private var surface: ghostty_surface_t?
    private var hostView: ViewerHostView!
    private var window: NSWindow!
    private var pollTimer: Timer?
    private var polls: FileHandle!
    private var pollIndex = 0
    private var lastViewport: String?
    private var finished = false

    init(log: HostLog) {
        self.log = log
        super.init()
    }

    func start() {
        // Same hygiene as GhosttyBootstrap.stripGhosttyEnvVars.
        for key in env.keys where key.hasPrefix("GHOSTTY_") { unsetenv(key) }

        guard ghostty_init(UInt(CommandLine.argc), CommandLine.unsafeArgv) == GHOSTTY_SUCCESS else {
            fatalError("ghostty_init failed")
        }
        guard let cfg = ghostty_config_new() else { fatalError("ghostty_config_new failed") }
        config = cfg
        ghostty_config_load_default_files(config)
        ghostty_config_load_recursive_files(config)
        ghostty_config_finalize(config)

        var runtime = ghostty_runtime_config_s(
            userdata: Unmanaged.passUnretained(self).toOpaque(),
            supports_selection_clipboard: false,
            wakeup_cb: viewerWakeup,
            action_cb: viewerAction,
            read_clipboard_cb: viewerReadClipboard,
            confirm_read_clipboard_cb: { _, _, _, _ in },
            write_clipboard_cb: viewerWriteClipboard,
            close_surface_cb: viewerCloseSurface
        )
        guard let handle = ghostty_app_new(&runtime, config) else { fatalError("ghostty_app_new failed") }
        app = handle
        ghostty_app_set_focus(app, false)
        log.line("ghostty app up")

        // Off-screen unless VIEWER_SHOW=1: this runs on someone's desktop and
        // must not put a window in front of them or take focus. A worker pane
        // on a background Boss tab is not visible either.
        let origin = showWindow ? NSPoint(x: 80, y: 80) : NSPoint(x: -30000, y: -30000)
        let frame = NSRect(origin: origin, size: initialSize)
        window = NSWindow(
            contentRect: frame,
            styleMask: showWindow ? [.titled, .resizable] : [.borderless],
            backing: .buffered,
            defer: false
        )
        window.title = "agy pane viability viewer"
        window.isReleasedWhenClosed = false
        hostView = ViewerHostView(frame: NSRect(origin: .zero, size: initialSize))
        hostView.wantsLayer = true
        hostView.layer?.backgroundColor = NSColor.black.cgColor
        window.contentView = hostView
        if showWindow { window.orderFrontRegardless() }

        let created: ghostty_surface_t? = initialInput.withCString { inputPtr in
            surfaceCwd.withCString { cwdPtr in
                var surfaceConfig = ghostty_surface_config_new()
                surfaceConfig.platform_tag = GHOSTTY_PLATFORM_MACOS
                surfaceConfig.platform = ghostty_platform_u(
                    macos: ghostty_platform_macos_s(nsview: Unmanaged.passUnretained(hostView).toOpaque())
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
        guard let created else {
            log.line("ghostty_surface_new returned NULL")
            try? "surface_new_null\n".write(to: outDir.appendingPathComponent("FAILED.txt"), atomically: true, encoding: .utf8)
            exit(3)
        }
        surface = created
        applySize(initialSize)
        ghostty_surface_set_focus(created, false)
        ghostty_app_tick(app)
        log.line("surface up size=\(Int(initialSize.width))x\(Int(initialSize.height)) show=\(showWindow) initial_input=\(initialInput.trimmingCharacters(in: .newlines))")
        try? "ready\n".write(to: outDir.appendingPathComponent("READY.txt"), atomically: true, encoding: .utf8)

        let pollsPath = outDir.appendingPathComponent("viewport_polls.jsonl")
        FileManager.default.createFile(atPath: pollsPath.path, contents: nil)
        polls = try! FileHandle(forWritingTo: pollsPath)

        pollTimer = Timer.scheduledTimer(withTimeInterval: pollInterval, repeats: true) { [weak self] _ in
            self?.poll()
        }
        Timer.scheduledTimer(withTimeInterval: hardDeadline, repeats: false) { [weak self] _ in
            self?.finish(reason: "deadline")
        }
    }

    func tick() {
        if finished { return }
        ghostty_app_tick(app)
    }

    func surfaceClosed(processAlive: Bool) {
        log.line("close_surface_cb processAlive=\(processAlive)")
    }

    private func applySize(_ size: NSSize) {
        guard let surface else { return }
        let scale = window.backingScaleFactor
        hostView.setFrameSize(size)
        window.setContentSize(size)
        ghostty_surface_set_content_scale(surface, scale, scale)
        ghostty_surface_set_size(surface, UInt32(size.width * scale), UInt32(size.height * scale))
    }

    private func readText(_ tag: ghostty_point_tag_e) -> String {
        guard let surface else { return "" }
        var text = ghostty_text_s()
        let selection = ghostty_selection_s(
            top_left: ghostty_point_s(tag: tag, coord: GHOSTTY_POINT_COORD_TOP_LEFT, x: 0, y: 0),
            bottom_right: ghostty_point_s(tag: tag, coord: GHOSTTY_POINT_COORD_BOTTOM_RIGHT, x: 0, y: 0),
            rectangle: false
        )
        guard ghostty_surface_read_text(surface, selection, &text) else { return "" }
        defer { ghostty_surface_free_text(surface, &text) }
        guard let pointer = text.text else { return "" }
        return String(cString: pointer)
    }

    private func sendKey(keycode: UInt32, codepoint: UInt32, release: Bool) {
        guard let surface else { return }
        var event = ghostty_input_key_s()
        event.action = GHOSTTY_ACTION_PRESS
        event.mods = GHOSTTY_MODS_NONE
        event.consumed_mods = GHOSTTY_MODS_NONE
        event.keycode = keycode
        event.text = nil
        event.composing = false
        event.unshifted_codepoint = codepoint
        _ = ghostty_surface_key(surface, event)
        if release {
            event.action = GHOSTTY_ACTION_RELEASE
            _ = ghostty_surface_key(surface, event)
        }
    }

    private func sendText(_ body: String) {
        guard let surface, !body.isEmpty else { return }
        body.withCString { pointer in
            ghostty_surface_text(surface, pointer, UInt(strlen(pointer)))
        }
    }

    private func snapshot(_ name: String) {
        try? readText(GHOSTTY_POINT_VIEWPORT).write(
            to: outDir.appendingPathComponent("snap_\(name).viewport.txt"), atomically: true, encoding: .utf8)
        try? readText(GHOSTTY_POINT_SCREEN).write(
            to: outDir.appendingPathComponent("snap_\(name).screen.txt"), atomically: true, encoding: .utf8)
    }

    private func runCommands() {
        guard let names = try? FileManager.default.contentsOfDirectory(atPath: cmdDir.path) else { return }
        for name in names.filter({ $0.hasSuffix(".cmd") }).sorted() {
            let path = cmdDir.appendingPathComponent(name)
            guard let raw = try? String(contentsOf: path, encoding: .utf8) else { continue }
            var command = raw
            while command.last == "\n" || command.last == "\r" { command.removeLast() }
            try? FileManager.default.moveItem(at: path, to: cmdDir.appendingPathComponent(name + ".done"))
            log.line("CMD \(name): \(command.prefix(160))")
            if command == "quit" {
                finish(reason: "quit")
                return
            } else if command == "key:return" {
                sendKey(keycode: 0x24, codepoint: 0x0D, release: false)
            } else if command == "key:esc" {
                sendKey(keycode: 0x35, codepoint: 0x1B, release: true)
            } else if command.hasPrefix("text:") {
                sendText(String(command.dropFirst(5)))
            } else if command.hasPrefix("submit:") {
                sendText(String(command.dropFirst(7)))
                sendKey(keycode: 0x24, codepoint: 0x0D, release: false)
            } else if command.hasPrefix("resize:") {
                let parts = command.dropFirst(7).split(separator: "x").compactMap { Double($0) }
                if parts.count == 2 { applySize(NSSize(width: parts[0], height: parts[1])) }
            } else if command.hasPrefix("focus:") {
                if let surface { ghostty_surface_set_focus(surface, command.hasSuffix("1")) }
            } else if command.hasPrefix("snapshot:") {
                snapshot(String(command.dropFirst(9)))
            } else {
                log.line("unknown command")
            }
        }
    }

    private func poll() {
        if finished { return }
        tick()
        guard let surface else { return }
        let viewport = readText(GHOSTTY_POINT_VIEWPORT)
        let changed = viewport != lastViewport
        pollIndex += 1
        var record: [String: Any] = [
            "i": pollIndex,
            "t": (log.elapsed() * 1000).rounded() / 1000,
            "wall_ms": Int(Date().timeIntervalSince1970 * 1000),
            "changed": changed,
            "exited": ghostty_surface_process_exited(surface),
        ]
        if changed {
            record["text"] = viewport
            lastViewport = viewport
            try? viewport.write(to: outDir.appendingPathComponent("viewport_latest.txt"), atomically: true, encoding: .utf8)
        }
        if let data = try? JSONSerialization.data(withJSONObject: record, options: [.sortedKeys]) {
            try? polls.write(contentsOf: data + Data("\n".utf8))
        }
        runCommands()
    }

    func finish(reason: String) {
        if finished { return }
        snapshot("final")
        finished = true
        pollTimer?.invalidate()
        log.line("finish reason=\(reason) polls=\(pollIndex)")
        if let surface {
            ghostty_surface_free(surface)
            self.surface = nil
        }
        ghostty_app_free(app)
        ghostty_config_free(config)
        try? "\(reason)\n".write(to: outDir.appendingPathComponent("FINISHED.txt"), atomically: true, encoding: .utf8)
        DispatchQueue.main.async { exit(0) }
    }
}

final class ViewerDelegate: NSObject, NSApplicationDelegate {
    var host: ViewerHost?

    func applicationDidFinishLaunching(_ notification: Notification) {
        try? FileManager.default.createDirectory(at: cmdDir, withIntermediateDirectories: true)
        let host = ViewerHost(log: HostLog(path: outDir.appendingPathComponent("host.log")))
        self.host = host
        host.start()
    }
}

let application = NSApplication.shared
let delegate = ViewerDelegate()
application.delegate = delegate
// Accessory: no Dock icon, no menu bar, never becomes the active app.
application.setActivationPolicy(.accessory)
application.run()
