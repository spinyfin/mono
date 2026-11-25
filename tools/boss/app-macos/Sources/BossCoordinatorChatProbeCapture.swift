import AppKit
import Foundation
import SwiftUI

/// Quiet, isolated fixture for the coordinator chat probe:
/// `--capture-reveal-state=coordinator-chat-probe --capture-to <png>`.
///
/// Hosts `CoordinatorChatProbeView` in a never-presented window, launches
/// `boss-acp-probe` by path, and drives one real conversation from a script
/// of operator actions, capturing the window at the points the script names.
/// The session directory is rendered here by the same code that renders the
/// real coordinator's, into a directory the caller chooses.
///
/// Flags (all `--name=value`):
/// - `--capture-chat-probe` the built `boss-acp-probe` executable.
/// - `--capture-chat-session` directory the session files are rendered into.
/// - `--capture-chat-journal` file the probe writes the wire journal to.
/// - `--capture-chat-script` JSON array of steps, see
///   `CoordinatorChatProbeScriptStep`.
/// - `--capture-chat-probe-arg` repeatable; passed to `boss-acp-probe converse`
///   after the session directory and journal (the isolated engine's socket,
///   the model, the mode, then `--` and the agent command).
/// - `--capture-chat-render-stats` optional file; when given, every streamed
///   answer delta is laid out and drawn synchronously and its cost recorded.
@MainActor
enum BossCoordinatorChatProbeCapture {
    static let stateName = "coordinator-chat-probe"

    private static var runner: CoordinatorChatProbeCaptureRunner?

    /// True in an isolated capture instance that was started to host this
    /// fixture.
    static var isSelected: Bool {
        BossCaptureArgs.shared.isCaptureMode && BossEnginePaths.isIsolatedInstance
            && CommandLine.arguments.contains("--capture-reveal-state=\(stateName)")
    }

    static func start(path: String) {
        guard isSelected else { return }
        do {
            let options = try CoordinatorChatProbeCaptureOptions.parse(CommandLine.arguments)
            try options.launch.validate(
                isIsolatedInstance: BossEnginePaths.isIsolatedInstance,
                fileExists: FileManager.default.isExecutableFile(atPath:)
            )
            let script = try CoordinatorChatProbeScriptStep.parse(Data(contentsOf: options.script))
            // The same files this build gives the real coordinator, written by
            // the same code, into the fixture's own directory.
            BossPaneModel.renderSessionDirectory(at: options.launch.sessionDirectory, directDeveloperMode: false)
            let runner = CoordinatorChatProbeCaptureRunner(options: options, script: script, finalCapture: path)
            self.runner = runner
            try runner.start()
        } catch {
            fail("\(error)")
        }
    }

    static func fail(_ message: String) -> Never {
        fputs("coordinator chat probe capture failed: \(message)\n", stderr)
        exit(1)
    }
}

/// What the fixture was asked to run.
struct CoordinatorChatProbeCaptureOptions: Equatable {
    enum ParseError: Error, Equatable, CustomStringConvertible {
        case missing(String)

        var description: String {
            switch self {
            case .missing(let flag): "\(flag)=<value> is required"
            }
        }
    }

    var launch: CoordinatorChatProbeLaunch
    var script: URL
    var renderStats: URL?

    static func parse(_ arguments: [String]) throws -> CoordinatorChatProbeCaptureOptions {
        func values(_ flag: String) -> [String] {
            arguments.filter { $0.hasPrefix("\(flag)=") }.map { String($0.dropFirst(flag.count + 1)) }
        }
        func required(_ flag: String) throws -> URL {
            guard let value = values(flag).last, !value.isEmpty else { throw ParseError.missing(flag) }
            return URL(fileURLWithPath: value)
        }
        return CoordinatorChatProbeCaptureOptions(
            launch: CoordinatorChatProbeLaunch(
                probe: try required("--capture-chat-probe"),
                sessionDirectory: try required("--capture-chat-session"),
                journal: try required("--capture-chat-journal"),
                probeArguments: values("--capture-chat-probe-arg")
            ),
            script: try required("--capture-chat-script"),
            renderStats: values("--capture-chat-render-stats").last.map { URL(fileURLWithPath: $0) }
        )
    }
}

/// One scripted operator action, or a point to wait for or capture at.
///
/// A script is a JSON array of objects, each with a `do` key:
///
/// ```json
/// [{"do": "wait", "until": "ready", "timeout": 60},
///  {"do": "send", "text": "hello"},
///  {"do": "wait", "until": "agent_chunks", "count": 3, "timeout": 120},
///  {"do": "capture", "to": "/tmp/streaming.png"},
///  {"do": "wait", "until": "idle", "timeout": 300}]
/// ```
enum CoordinatorChatProbeScriptStep: Equatable {
    enum Condition: Equatable {
        /// The session exists and accepts prompts.
        case ready
        /// At least this many answer deltas have arrived since the start.
        case agentChunks(Int)
        /// The answer at the end of the conversation is still streaming and
        /// already has at least this many characters.
        case streamingCharacters(Int)
        /// At least this many turns have ended since the start.
        case turnsFinished(Int)
        /// A permission card is waiting for an answer.
        case pendingPermission
        /// At least this many tool rows have completed since the start.
        case completedTools(Int)
        /// At least this many of the agent's own turns, started with no
        /// prompt in flight, have ended since the start.
        case agentTurnsFinished(Int)
        /// No prompt is in flight and every turn that started has ended.
        case idle
    }

    enum ParseError: Error, Equatable, CustomStringConvertible {
        case notAnArray
        case invalidStep(index: Int, reason: String)

        var description: String {
            switch self {
            case .notAnArray: "the script must be a JSON array of steps"
            case .invalidStep(let index, let reason): "script step \(index): \(reason)"
            }
        }
    }

    /// Types the text into the composer and presses Send.
    case send(text: String)
    /// Puts the image on a private pasteboard and runs the composer's paste.
    case pasteImage(file: String)
    case wait(Condition, timeout: TimeInterval)
    /// Presses the button of the waiting permission card whose option has
    /// this kind (`allow_once`, `reject_once`, ...).
    case answer(kind: String)
    /// Presses Stop.
    case stop
    case capture(to: String)
    case sleep(TimeInterval)

    static func parse(_ data: Data) throws -> [CoordinatorChatProbeScriptStep] {
        guard let steps = (try? JSONSerialization.jsonObject(with: data)) as? [[String: Any]] else {
            throw ParseError.notAnArray
        }
        return try steps.enumerated().map { index, step in
            func fail(_ reason: String) -> ParseError { .invalidStep(index: index, reason: reason) }
            func string(_ key: String) throws -> String {
                guard let value = step[key] as? String, !value.isEmpty else { throw fail("`\(key)` is required") }
                return value
            }
            func number(_ key: String) throws -> Double {
                guard let value = (step[key] as? NSNumber)?.doubleValue, value >= 0 else {
                    throw fail("`\(key)` must be a non-negative number")
                }
                return value
            }
            switch step["do"] as? String {
            case "send": return .send(text: try string("text"))
            case "paste_image": return .pasteImage(file: try string("file"))
            case "answer": return .answer(kind: try string("kind"))
            case "stop": return .stop
            case "capture": return .capture(to: try string("to"))
            case "sleep": return .sleep(try number("seconds"))
            case "wait":
                let condition: Condition
                switch try string("until") {
                case "ready": condition = .ready
                case "agent_chunks": condition = .agentChunks(Int(try number("count")))
                case "streaming_characters": condition = .streamingCharacters(Int(try number("count")))
                case "turns_finished": condition = .turnsFinished(Int(try number("count")))
                case "pending_permission": condition = .pendingPermission
                case "completed_tools": condition = .completedTools(Int(try number("count")))
                case "agent_turns_finished": condition = .agentTurnsFinished(Int(try number("count")))
                case "idle": condition = .idle
                case let other: throw fail("unknown wait condition `\(other)`")
                }
                return .wait(condition, timeout: try number("timeout"))
            case let other:
                throw fail("unknown action `\(other ?? "(none)")`")
            }
        }
    }
}

extension CoordinatorChatProbeModel {
    /// The permission card that is waiting for an answer, oldest first.
    var pendingPermission: CoordinatorChatProbeItem.Permission? {
        for item in items {
            if case .permission(let permission) = item.kind, permission.state == .pending { return permission }
        }
        return nil
    }

    var completedToolCount: Int {
        items.filter { item in
            if case .tool(let tool) = item.kind { return tool.status == "completed" }
            return false
        }.count
    }

    func holds(_ condition: CoordinatorChatProbeScriptStep.Condition) -> Bool {
        switch condition {
        case .ready: status != .starting
        case .agentChunks(let count): agentChunks >= count
        case .streamingCharacters(let count):
            if case .agentMessage(let text, true)? = items.last?.kind { text.count >= count } else { false }
        case .turnsFinished(let count): turnsFinished >= count
        case .pendingPermission: pendingPermission != nil
        case .completedTools(let count): completedToolCount >= count
        case .agentTurnsFinished(let count): agentTurnsFinished >= count
        case .idle:
            status == .idle && promptsInFlight == 0 && turnsStarted == turnsFinished && agentTurnsOpen == 0
        }
    }
}

/// The cost of showing one streamed answer delta, measured on the main thread.
struct CoordinatorChatProbeRenderSample: Equatable {
    /// Characters in the streaming answer after this delta was applied.
    var characters: Int
    /// Applying the event to the model.
    var applyMilliseconds: Double
    /// Bringing the SwiftUI hierarchy up to date, which is where the markdown
    /// of the streaming answer is parsed again.
    var layoutMilliseconds: Double
    /// Drawing the whole window into a bitmap.
    var drawMilliseconds: Double

    var json: [String: Any] {
        [
            "characters": characters,
            "apply_ms": applyMilliseconds,
            "layout_ms": layoutMilliseconds,
            "draw_ms": drawMilliseconds,
        ]
    }
}

@MainActor
private final class CoordinatorChatProbeCaptureRunner {
    private let options: CoordinatorChatProbeCaptureOptions
    private let script: [CoordinatorChatProbeScriptStep]
    private let finalCapture: String
    private let window: NSWindow
    private var model: CoordinatorChatProbeModel!
    private var process: CoordinatorChatProbeProcess!
    private var exitStatus: Int32?
    private var renderSamples: [CoordinatorChatProbeRenderSample] = []
    private var renderBitmap: NSBitmapImageRep?

    private var attachmentDirectory: URL {
        options.launch.journal.deletingLastPathComponent().appendingPathComponent("pasted-images")
    }

    init(options: CoordinatorChatProbeCaptureOptions, script: [CoordinatorChatProbeScriptStep], finalCapture: String) {
        self.options = options
        self.script = script
        self.finalCapture = finalCapture
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 760, height: 860),
            styleMask: [.titled], backing: .buffered, defer: false
        )
        window.title = "Boss [agent capture]"
    }

    func start() throws {
        process = CoordinatorChatProbeProcess(
            launch: options.launch,
            onLine: { [weak self] line in self?.receive(line) },
            onExit: { [weak self] status in self?.exitStatus = status }
        )
        let process = self.process!
        model = CoordinatorChatProbeModel(sendCommand: { line in process.send(line: line) })
        window.contentView = NSHostingView(
            rootView: CoordinatorChatProbeView(model: model, attachmentDirectory: attachmentDirectory))
        window.contentView?.layoutSubtreeIfNeeded()
        try process.start()
        Task { @MainActor in await self.run() }
    }

    private func receive(_ line: Data) {
        guard let event = CoordinatorChatProbeEvent.decode(line: line) else { return }
        guard options.renderStats != nil, case .messageChunk(.agent, _, _) = event.kind else {
            model.apply(event)
            return
        }
        let clock = ContinuousClock()
        let started = clock.now
        model.apply(event)
        let applied = clock.now
        window.contentView?.layoutSubtreeIfNeeded()
        let laidOut = clock.now
        draw()
        let drawn = clock.now
        var characters = 0
        if case .agentMessage(let text, _)? = model.items.last?.kind { characters = text.count }
        renderSamples.append(CoordinatorChatProbeRenderSample(
            characters: characters,
            applyMilliseconds: Self.milliseconds(applied - started),
            layoutMilliseconds: Self.milliseconds(laidOut - applied),
            drawMilliseconds: Self.milliseconds(drawn - laidOut)
        ))
    }

    private static func milliseconds(_ duration: Duration) -> Double {
        let components = duration.components
        return Double(components.seconds) * 1000 + Double(components.attoseconds) / 1e15
    }

    /// Draws the window into a reused bitmap, the way the capture does.
    private func draw() {
        guard let frameView = window.contentView?.superview else { return }
        let bounds = frameView.bounds
        if renderBitmap == nil {
            renderBitmap = frameView.bitmapImageRepForCachingDisplay(in: bounds)
        }
        if let renderBitmap {
            frameView.cacheDisplay(in: bounds, to: renderBitmap)
        }
    }

    private func run() async {
        for (index, step) in script.enumerated() {
            if let status = exitStatus {
                finish(failure: "the probe exited with status \(status) before script step \(index)")
            }
            switch step {
            case .send(let text):
                model.draft = text
                guard model.canSend else { finish(failure: "script step \(index): the composer cannot send now") }
                model.send()
            case .pasteImage(let file):
                pasteImage(file, step: index)
            case .wait(let condition, let timeout):
                let deadline = ContinuousClock.now + .seconds(timeout)
                while !model.holds(condition) {
                    if ContinuousClock.now >= deadline {
                        finish(failure: "script step \(index): timed out after \(timeout)s waiting for \(condition); status \(model.status.label)")
                    }
                    if let status = exitStatus, !model.holds(condition) {
                        finish(failure: "script step \(index): the probe exited with status \(status) while waiting for \(condition)")
                    }
                    try? await Task.sleep(for: .milliseconds(20))
                }
            case .answer(let kind):
                guard let permission = model.pendingPermission,
                      let option = permission.options.first(where: { $0.kind == kind })
                else { finish(failure: "script step \(index): no waiting permission card offers `\(kind)`") }
                model.answer(requestID: permission.requestID, optionID: option.id)
            case .stop:
                model.stop()
            case .capture(let path):
                capture(to: path)
            case .sleep(let seconds):
                try? await Task.sleep(for: .seconds(seconds))
            }
        }
        capture(to: finalCapture)
        process.finish()
        let deadline = ContinuousClock.now + .seconds(20)
        while exitStatus == nil, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(50))
        }
        finish(failure: nil)
    }

    /// Runs the composer's paste against a pasteboard of this fixture's own,
    /// so the user's clipboard is neither read nor replaced.
    private func pasteImage(_ file: String, step: Int) {
        guard let bytes = FileManager.default.contents(atPath: file) else {
            finish(failure: "script step \(step): cannot read \(file)")
        }
        let pasteboard = NSPasteboard(name: NSPasteboard.Name("dev.spinyfin.boss.chat-probe.\(UUID().uuidString)"))
        defer { pasteboard.releaseGlobally() }
        pasteboard.clearContents()
        pasteboard.setData(bytes, forType: CoordinatorChatImagePaste.isPNG(bytes) ? .png : .tiff)
        guard let image = CoordinatorChatImagePaste.writeImage(from: pasteboard, into: attachmentDirectory) else {
            finish(failure: "script step \(step): the pasteboard held no image the composer accepts")
        }
        model.attach(image)
    }

    private func capture(to path: String) {
        window.contentView?.layoutSubtreeIfNeeded()
        do {
            try BossWindowCapture.captureWindow(window, to: path)
            print("captured \(path): \(model.items.count) rows, status \(model.status.label)")
            fflush(stdout)
        } catch {
            finish(failure: "\(error)")
        }
    }

    private func finish(failure: String?) -> Never {
        if let stats = options.renderStats {
            let report: [String: Any] = ["samples": renderSamples.map(\.json)]
            if let data = try? JSONSerialization.data(withJSONObject: report, options: [.sortedKeys]) {
                try? data.write(to: stats, options: .atomic)
            }
        }
        if let failure {
            process.terminate()
            BossCoordinatorChatProbeCapture.fail(failure)
        }
        if exitStatus == nil {
            process.terminate()
        }
        exit(0)
    }
}
