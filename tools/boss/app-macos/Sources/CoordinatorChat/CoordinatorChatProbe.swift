import AppKit
import Foundation
import SwiftUI
import Textual
import os

// Coordinator chat probe fixture.
//
// A minimal native conversation view over `boss-acp-probe`, the manual harness
// in `tools/boss/acp_client` that holds a conversation with a real ACP agent
// as a child process. The app does not link that crate: it launches the probe
// by path and exchanges one JSON object per line with it. Commands go in on
// the probe's stdin (`prompt`, `cancel`, `permission`); events come back on its
// stdout.
//
// This exists to validate the protocol with a real front end before the
// engine's chat service and the real panel are built. It is reachable only
// from the quiet capture path of an isolated instance
// (`BossCoordinatorChatProbeCapture`); nothing in the normal app mounts it.

// MARK: - Events

/// One event line from the probe, reduced to what the view renders. An event
/// this build does not know decodes as `.other`, so a newer probe cannot break
/// an older fixture.
struct CoordinatorChatProbeEvent: Equatable, Sendable {
    enum Role: String, Equatable, Sendable {
        case agent, thought, user
    }

    struct ToolCallChange: Equatable, Sendable {
        var id: String
        var title: String?
        var kind: String?
        var status: String?
        var input: String?
        var output: String?
    }

    struct PermissionOption: Equatable, Sendable, Identifiable {
        var id: String
        var name: String
        var kind: String
    }

    struct PermissionRequest: Equatable, Sendable {
        var requestID: String
        var title: String
        var input: String?
        var options: [PermissionOption]
    }

    enum Kind: Equatable, Sendable {
        case ready(sessionID: String, summary: String)
        case turnStarted(turn: Int, text: String, images: Int)
        case messageChunk(role: Role, text: String, messageID: String?)
        case toolCall(ToolCallChange)
        case permissionRequest(PermissionRequest)
        case permissionResolved(requestID: String, optionName: String?, cancelled: Bool)
        /// The agent took its own permission request back; it expects no answer.
        case permissionWithdrawn(requestID: String)
        /// Work the agent reports outside a prompt: a background command or
        /// agent, or (`taskType` "turn") its own follow-up turn. `phase` is
        /// "spawned" or "state"; `state` is set on the latter.
        case asyncTask(phase: String, id: String, taskType: String?, state: String?, name: String?)
        case turnEnded(turn: Int, stopReason: String)
        case turnFailed(turn: Int, error: String)
        case usage(used: Int, size: Int, model: String?, origin: String?)
        case mode(String)
        case cancelSent
        case ended(String)
        case commandError(String)
        case other(String)
    }

    var kind: Kind
    /// Prompts awaiting a response when the event was produced. Zero on an
    /// update means the agent is working with no prompt in flight.
    var promptsInFlight: Int?

    /// Decodes one stdout line. Returns nil for a line that is not an event.
    static func decode(line: Data) -> CoordinatorChatProbeEvent? {
        guard let object = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any],
              let name = object["event"] as? String
        else { return nil }
        func string(_ key: String) -> String? { object[key] as? String }
        func int(_ key: String) -> Int? { (object[key] as? NSNumber)?.intValue }
        let kind: Kind
        switch name {
        case "ready":
            let config = object["config"] as? [String: Any] ?? [:]
            let agent = object["agent"] as? [String: Any] ?? [:]
            let mode = string("mode_selected") ?? string("mode_at_creation") ?? "?"
            let parts = [
                "\(agent["name"] as? String ?? "agent") \(agent["version"] as? String ?? "")",
                "mode \(mode)",
                "model \(config["model"] as? String ?? "?")",
            ]
            kind = .ready(sessionID: string("session_id") ?? "", summary: parts.joined(separator: " · "))
        case "turn_started":
            kind = .turnStarted(turn: int("turn") ?? 0, text: string("text") ?? "", images: int("images") ?? 0)
        case "message_chunk":
            guard let role = string("role").flatMap(Role.init(rawValue:)) else { return nil }
            kind = .messageChunk(role: role, text: string("text") ?? "", messageID: string("message_id"))
        case "tool_call":
            guard let id = string("id") else { return nil }
            kind = .toolCall(ToolCallChange(
                id: id, title: string("title"), kind: string("kind"), status: string("status"),
                input: string("input"), output: string("output")
            ))
        case "permission_request":
            guard let requestID = string("request_id") else { return nil }
            let options = (object["options"] as? [[String: Any]] ?? []).compactMap { option -> PermissionOption? in
                guard let id = option["id"] as? String else { return nil }
                return PermissionOption(
                    id: id, name: option["name"] as? String ?? id, kind: option["kind"] as? String ?? ""
                )
            }
            kind = .permissionRequest(PermissionRequest(
                requestID: requestID, title: string("title") ?? "Permission requested",
                input: string("input"), options: options
            ))
        case "permission_resolved":
            guard let requestID = string("request_id") else { return nil }
            kind = .permissionResolved(
                requestID: requestID, optionName: string("option_name"),
                cancelled: string("outcome") == "cancelled"
            )
        case "permission_withdrawn":
            guard let requestID = string("request_id") else { return nil }
            kind = .permissionWithdrawn(requestID: requestID)
        case "async_task":
            guard let id = string("id"), let phase = string("phase") else { return nil }
            kind = .asyncTask(
                phase: phase, id: id, taskType: string("task_type"), state: string("state"), name: string("name")
            )
        case "turn_ended":
            kind = .turnEnded(turn: int("turn") ?? 0, stopReason: string("stop_reason") ?? "?")
        case "turn_failed":
            kind = .turnFailed(turn: int("turn") ?? 0, error: string("error") ?? "unknown error")
        case "usage":
            kind = .usage(
                used: int("used") ?? 0, size: int("size") ?? 0, model: string("model"), origin: string("origin")
            )
        case "mode":
            kind = .mode(string("mode") ?? "?")
        case "cancel_sent":
            kind = .cancelSent
        case "closed":
            kind = .ended(string("reason") ?? "the agent closed the stream")
        case "exit":
            kind = .ended("agent exited: \(string("status") ?? "?")")
        case "command_error":
            kind = .commandError(string("error") ?? "command failed")
        default:
            kind = .other(name)
        }
        return CoordinatorChatProbeEvent(kind: kind, promptsInFlight: int("prompts_in_flight"))
    }
}

// MARK: - Conversation items

/// One row of the conversation.
struct CoordinatorChatProbeItem: Identifiable, Equatable {
    struct Tool: Equatable {
        var title: String
        var kind: String
        var status: String
        var input: String
        var output: String
    }

    enum PermissionState: Equatable {
        case pending
        case answered(String)
        case cancelled
        /// The agent withdrew the request before it was answered.
        case withdrawn
    }

    struct Permission: Equatable {
        var requestID: String
        var title: String
        var input: String
        var options: [CoordinatorChatProbeEvent.PermissionOption]
        var state: PermissionState
    }

    enum Kind: Equatable {
        case operatorMessage(text: String, images: [URL])
        case agentMessage(text: String, streaming: Bool)
        case thought(text: String)
        case tool(Tool)
        case permission(Permission)
        /// A turn boundary or other one-line marker.
        case marker(String)
        case failure(String)
    }

    let id: String
    var kind: Kind
}

// MARK: - Model

/// Reduces probe events to conversation rows and turns composer actions
/// into probe commands. Pure state: it owns no process, so tests drive it with
/// events and read the command lines it emits.
@MainActor
final class CoordinatorChatProbeModel: ObservableObject {
    enum Status: Equatable {
        case starting
        case idle
        case working
        case waitingForPermission
        case ended(String)

        var label: String {
            switch self {
            case .starting: "Starting"
            case .idle: "Idle"
            case .working: "Working"
            case .waitingForPermission: "Waiting for permission"
            case .ended(let reason): "Ended: \(reason)"
            }
        }
    }

    @Published private(set) var items: [CoordinatorChatProbeItem] = []
    @Published private(set) var status: Status = .starting
    @Published private(set) var sessionSummary = ""
    @Published private(set) var contextUsage: String?
    @Published private(set) var attachments: [URL] = []
    @Published var draft = ""

    /// Counters a scripted run waits on.
    private(set) var promptsInFlight = 0
    private(set) var turnsStarted = 0
    private(set) var turnsFinished = 0
    private(set) var agentTurnsFinished = 0
    private(set) var agentChunks = 0

    private let sendCommand: (String) -> Void
    private var nextItemNumber = 0
    /// Images attached to prompts that were sent but not yet echoed back as a
    /// started turn, oldest first.
    private var sentImages: [[URL]] = []
    /// Set while updates arrive with no prompt in flight, so one marker opens
    /// the agent-initiated stretch instead of one per update.
    private var inAgentInitiatedStretch = false
    /// Tasks the agent reported as spawned and not yet finished: id to type.
    private var openTasks: [String: String] = [:]

    /// The agent's own turns that started with no prompt and have not ended.
    var agentTurnsOpen: Int {
        openTasks.values.filter { $0 == Self.agentTurnTaskType }.count
    }

    private static let agentTurnTaskType = "turn"
    /// The states in which the agent has finished with a task.
    private static let finishedTaskStates: Set<String> = ["completed", "failed", "stopped"]

    /// `sendCommand` receives one JSON command line, without its newline.
    init(sendCommand: @escaping (String) -> Void) {
        self.sendCommand = sendCommand
    }

    // MARK: Composer actions

    var canSend: Bool {
        guard status != .starting, !isEnded else { return false }
        return !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !attachments.isEmpty
    }

    var isEnded: Bool {
        if case .ended = status { return true }
        return false
    }

    /// Sends the draft and its attachments as one prompt. The text is sent as
    /// typed: nothing is trimmed or rewritten on the way.
    func send() {
        guard canSend else { return }
        var command: [String: Any] = ["op": "prompt", "text": draft]
        if !attachments.isEmpty {
            command["images"] = attachments.map(\.path)
        }
        sentImages.append(attachments)
        emit(command)
        draft = ""
        attachments = []
    }

    /// Stops the turn in flight. The probe answers pending permission requests
    /// `cancelled` before it sends the cancel, as the protocol requires.
    func stop() {
        emit(["op": "cancel"])
    }

    /// Answers a permission card with one of the agent's own options.
    func answer(requestID: String, optionID: String) {
        emit(["op": "permission", "request_id": requestID, "option_id": optionID])
    }

    func attach(_ image: URL) {
        attachments.append(image)
    }

    func removeAttachment(_ image: URL) {
        attachments.removeAll { $0 == image }
    }

    private func emit(_ command: [String: Any]) {
        guard let data = try? JSONSerialization.data(withJSONObject: command, options: [.sortedKeys]),
              let line = String(data: data, encoding: .utf8)
        else { return }
        sendCommand(line)
    }

    // MARK: Events

    func apply(_ event: CoordinatorChatProbeEvent) {
        if let inFlight = event.promptsInFlight {
            promptsInFlight = inFlight
        }
        switch event.kind {
        case .ready(_, let summary):
            sessionSummary = summary
            status = .idle
        case .turnStarted(_, let text, _):
            closeStreamingMessage()
            inAgentInitiatedStretch = false
            turnsStarted += 1
            let images = sentImages.isEmpty ? [] : sentImages.removeFirst()
            append(.operatorMessage(text: text, images: images))
            status = .working
        case .messageChunk(let role, let text, _):
            markAgentInitiatedIfNeeded()
            appendChunk(role: role, text: text)
        case .toolCall(let change):
            markAgentInitiatedIfNeeded()
            applyToolCall(change)
        case .permissionRequest(let request):
            closeStreamingMessage()
            append(.permission(.init(
                requestID: request.requestID, title: request.title, input: request.input ?? "",
                options: request.options, state: .pending
            )))
            status = .waitingForPermission
        case .permissionResolved(let requestID, let optionName, let cancelled):
            updatePermission(requestID) { permission in
                permission.state = cancelled ? .cancelled : .answered(optionName ?? "answered")
            }
            settleAfterPermission()
        case .permissionWithdrawn(let requestID):
            updatePermission(requestID) { permission in
                if permission.state == .pending { permission.state = .withdrawn }
            }
            settleAfterPermission()
        case .asyncTask(let phase, let id, let taskType, let state, let name):
            applyAsyncTask(phase: phase, id: id, taskType: taskType, state: state, name: name)
        case .turnEnded(_, let stopReason):
            closeStreamingMessage()
            turnsFinished += 1
            append(.marker("Turn ended · \(stopReason)"))
            status = isAgentBusy ? .working : .idle
        case .turnFailed(_, let error):
            closeStreamingMessage()
            turnsFinished += 1
            append(.failure(error))
            status = isAgentBusy ? .working : .idle
        case .usage(let used, let size, let model, _):
            if size > 0 {
                let percent = Int((Double(used) / Double(size) * 100).rounded())
                contextUsage = "\(percent)% of context" + (model.map { " · \($0)" } ?? "")
            }
        case .mode(let mode):
            append(.marker("Mode: \(mode)"))
        case .cancelSent:
            append(.marker("Stop requested"))
        case .ended(let reason):
            closeStreamingMessage()
            status = .ended(reason)
        case .commandError(let error):
            append(.failure(error))
        case .other:
            break
        }
    }

    private var hasPendingPermission: Bool {
        items.contains { item in
            if case .permission(let permission) = item.kind { return permission.state == .pending }
            return false
        }
    }

    /// A prompt is awaiting its response, or the agent is in a turn of its own.
    private var isAgentBusy: Bool {
        promptsInFlight > 0 || agentTurnsOpen > 0
    }

    private func settleAfterPermission() {
        if status == .waitingForPermission, !hasPendingPermission {
            status = isAgentBusy ? .working : .idle
        }
    }

    /// The agent's task extension brackets work that outlives a prompt. Its
    /// own follow-up turn is one such task, which is what tells a front end
    /// when that turn starts and, reliably, when it ends.
    private func applyAsyncTask(phase: String, id: String, taskType: String?, state: String?, name: String?) {
        switch phase {
        case "spawned":
            let type = taskType ?? "task"
            openTasks[id] = type
            closeStreamingMessage()
            if type == Self.agentTurnTaskType {
                // The turn has its own marker; the generic one would repeat it.
                inAgentInitiatedStretch = true
                append(.marker("Agent turn started · no prompt in flight"))
                status = .working
            } else {
                append(.marker("Background \(type) started · \(name ?? id)"))
            }
        case "state":
            guard let state, Self.finishedTaskStates.contains(state),
                  let type = openTasks.removeValue(forKey: id)
            else { return }
            closeStreamingMessage()
            if type == Self.agentTurnTaskType {
                agentTurnsFinished += 1
                append(.marker("Agent turn ended · \(state)"))
                if !isAgentBusy, !hasPendingPermission { status = .idle }
            } else {
                append(.marker("Background \(type) \(state)"))
            }
        default:
            break
        }
    }

    /// Updates with no prompt in flight and no agent turn open are background
    /// work the agent started earlier, such as a background agent's tool calls.
    private func markAgentInitiatedIfNeeded() {
        guard promptsInFlight == 0, turnsStarted > 0, !inAgentInitiatedStretch else { return }
        inAgentInitiatedStretch = true
        closeStreamingMessage()
        append(.marker("Background activity · no prompt in flight"))
    }

    private func appendChunk(role: CoordinatorChatProbeEvent.Role, text: String) {
        switch role {
        case .agent:
            agentChunks += 1
            if let last = items.indices.last, case .agentMessage(let existing, true) = items[last].kind {
                items[last].kind = .agentMessage(text: existing + text, streaming: true)
            } else {
                append(.agentMessage(text: text, streaming: true))
            }
        case .thought:
            if let last = items.indices.last, case .thought(let existing) = items[last].kind {
                items[last].kind = .thought(text: existing + text)
            } else {
                closeStreamingMessage()
                append(.thought(text: text))
            }
        case .user:
            // The echoed prompt text is already shown in its row.
            break
        }
    }

    /// A tool call is one row for its whole life: later changes merge into it.
    private func applyToolCall(_ change: CoordinatorChatProbeEvent.ToolCallChange) {
        let rowID = "tool-\(change.id)"
        if let index = items.firstIndex(where: { $0.id == rowID }), case .tool(var tool) = items[index].kind {
            tool.title = change.title ?? tool.title
            tool.kind = change.kind ?? tool.kind
            tool.status = change.status ?? tool.status
            tool.input = change.input ?? tool.input
            tool.output = change.output ?? tool.output
            items[index].kind = .tool(tool)
            return
        }
        closeStreamingMessage()
        items.append(CoordinatorChatProbeItem(id: rowID, kind: .tool(.init(
            title: change.title ?? "Tool call", kind: change.kind ?? "other",
            status: change.status ?? "pending", input: change.input ?? "", output: change.output ?? ""
        ))))
    }

    private func updatePermission(_ requestID: String, _ update: (inout CoordinatorChatProbeItem.Permission) -> Void) {
        guard let index = items.firstIndex(where: { $0.id == "permission-\(requestID)" }),
              case .permission(var permission) = items[index].kind
        else { return }
        update(&permission)
        items[index].kind = .permission(permission)
    }

    private func closeStreamingMessage() {
        guard let last = items.indices.last, case .agentMessage(let text, true) = items[last].kind else { return }
        items[last].kind = .agentMessage(text: text, streaming: false)
    }

    private func append(_ kind: CoordinatorChatProbeItem.Kind) {
        let id: String
        if case .permission(let permission) = kind {
            id = "permission-\(permission.requestID)"
        } else {
            nextItemNumber += 1
            id = "item-\(nextItemNumber)"
        }
        items.append(CoordinatorChatProbeItem(id: id, kind: kind))
    }
}

// MARK: - Pasted images

/// The part of a pasteboard the image paste reads, so the same code runs
/// against the real pasteboard and against a fabricated one in tests.
protocol CoordinatorChatImagePasteSource {
    func data(forType type: NSPasteboard.PasteboardType) -> Data?
}

extension NSPasteboard: CoordinatorChatImagePasteSource {}

enum CoordinatorChatImagePaste {
    /// Writes the pasteboard's image to a PNG file in `directory` and returns
    /// it, or nil when the pasteboard holds no image. PNG bytes are kept as
    /// they are; a TIFF, which is what a screenshot copied from most apps
    /// arrives as, is re-encoded.
    static func writeImage(from source: CoordinatorChatImagePasteSource, into directory: URL) -> URL? {
        let png: Data
        if let data = source.data(forType: .png), isPNG(data) {
            png = data
        } else if let tiff = source.data(forType: .tiff),
                  let converted = NSBitmapImageRep(data: tiff)?.representation(using: .png, properties: [:])
        {
            png = converted
        } else {
            return nil
        }
        let file = directory.appendingPathComponent("pasted-\(UUID().uuidString).png")
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            try png.write(to: file, options: .atomic)
            return file
        } catch {
            return nil
        }
    }

    static func isPNG(_ data: Data) -> Bool {
        data.starts(with: [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
    }
}

// MARK: - Probe process

/// Why the fixture refuses to launch the probe.
enum CoordinatorChatProbeLaunchError: Error, Equatable, CustomStringConvertible {
    case notIsolated
    case productionPath(String)
    case probeMissing(String)

    var description: String {
        switch self {
        case .notIsolated:
            "the coordinator chat probe only runs in an isolated capture instance"
        case .productionPath(let path):
            "\(path) belongs to the installed Boss app; the probe must not use it"
        case .probeMissing(let path):
            "no probe executable at \(path)"
        }
    }
}

/// What the fixture launches: the probe, its session directory and journal,
/// and the arguments that name the agent and its environment.
struct CoordinatorChatProbeLaunch: Equatable {
    var probe: URL
    var sessionDirectory: URL
    var journal: URL
    /// Passed through to `boss-acp-probe converse` after the paths above.
    var probeArguments: [String]

    var arguments: [String] {
        ["converse", "--cwd", sessionDirectory.path, "--journal", journal.path] + probeArguments
    }

    /// The fixture is for isolated instances only, and nothing it is given may
    /// point into the installed app's state.
    func validate(isIsolatedInstance: Bool, fileExists: (String) -> Bool) throws {
        guard isIsolatedInstance else { throw CoordinatorChatProbeLaunchError.notIsolated }
        for path in [sessionDirectory.path, journal.path] + probeArguments
        where BossEnginePaths.isProductionPath(path) {
            throw CoordinatorChatProbeLaunchError.productionPath(path)
        }
        guard fileExists(probe.path) else { throw CoordinatorChatProbeLaunchError.probeMissing(probe.path) }
    }
}

/// Runs the probe and exchanges lines with it. Lines are delivered on the main
/// queue in the order the probe wrote them.
final class CoordinatorChatProbeProcess: @unchecked Sendable {
    private let process = Process()
    private let input = Pipe()
    private let output = Pipe()
    private let buffer = OSAllocatedUnfairLock(initialState: Data())
    private let onLine: @MainActor @Sendable (Data) -> Void
    private let onExit: @MainActor @Sendable (Int32) -> Void

    init(
        launch: CoordinatorChatProbeLaunch,
        onLine: @escaping @MainActor @Sendable (Data) -> Void,
        onExit: @escaping @MainActor @Sendable (Int32) -> Void
    ) {
        self.onLine = onLine
        self.onExit = onExit
        process.executableURL = launch.probe
        process.arguments = launch.arguments
        process.standardInput = input
        process.standardOutput = output
        // The probe's diagnostics, and the agent's unless the arguments send
        // them to a file, join this instance's stderr.
        process.standardError = FileHandle.standardError
    }

    func start() throws {
        output.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let chunk = handle.availableData
            guard let self else { return }
            if chunk.isEmpty {
                handle.readabilityHandler = nil
                return
            }
            for line in self.completeLines(adding: chunk) {
                DispatchQueue.main.async { MainActor.assumeIsolated { self.onLine(line) } }
            }
        }
        process.terminationHandler = { [weak self] finished in
            let status = finished.terminationStatus
            DispatchQueue.main.async { MainActor.assumeIsolated { self?.onExit(status) } }
        }
        try process.run()
    }

    /// Writes one command line. A probe that has gone away is not an error
    /// here: its exit is reported through `onExit`.
    func send(line: String) {
        guard process.isRunning else { return }
        try? input.fileHandleForWriting.write(contentsOf: Data((line + "\n").utf8))
    }

    /// Ends the conversation: the probe closes the agent's input and exits.
    func finish() {
        send(line: #"{"op":"shutdown"}"#)
        try? input.fileHandleForWriting.close()
    }

    func terminate() {
        if process.isRunning { process.terminate() }
    }

    private func completeLines(adding chunk: Data) -> [Data] {
        buffer.withLock { pending in
            pending.append(chunk)
            var lines: [Data] = []
            while let newline = pending.firstIndex(of: 0x0A) {
                lines.append(pending.subdata(in: pending.startIndex..<newline))
                pending.removeSubrange(pending.startIndex...newline)
            }
            return lines
        }
    }
}

// MARK: - View

struct CoordinatorChatProbeView: View {
    @ObservedObject var model: CoordinatorChatProbeModel
    /// Where pasted images are written before they are sent.
    let attachmentDirectory: URL

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            conversation
            Divider()
            statusRow
            composer
        }
        .frame(minWidth: 480, minHeight: 360)
    }

    private var header: some View {
        HStack(spacing: 8) {
            Image(systemName: "bubble.left.and.text.bubble.right")
            Text("Coordinator chat probe").font(.headline)
            Spacer()
            Text(model.sessionSummary).font(.caption).foregroundStyle(.secondary).lineLimit(1)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
    }

    private var conversation: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 10) {
                    ForEach(model.items) { item in
                        CoordinatorChatProbeRow(item: item, answer: model.answer)
                            .id(item.id)
                    }
                }
                .padding(14)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .onChange(of: model.items.last) { _, last in
                // Follow the tail while the answer streams.
                if let last { proxy.scrollTo(last.id, anchor: .bottom) }
            }
        }
    }

    private var statusRow: some View {
        HStack(spacing: 8) {
            if model.status == .working {
                ProgressView().controlSize(.small)
            }
            Text(model.status.label).font(.caption)
            if model.promptsInFlight > 1 {
                Text("\(model.promptsInFlight) prompts in flight").font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            if let usage = model.contextUsage {
                Text(usage).font(.caption).foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 6)
    }

    private var composer: some View {
        VStack(alignment: .leading, spacing: 6) {
            if !model.attachments.isEmpty {
                HStack(spacing: 6) {
                    ForEach(model.attachments, id: \.self) { image in
                        CoordinatorChatProbeAttachmentChip(image: image) { model.removeAttachment(image) }
                    }
                }
            }
            HStack(alignment: .bottom, spacing: 8) {
                TextEditor(text: $model.draft)
                    .font(.body)
                    .frame(minHeight: 44, maxHeight: 110)
                    .scrollContentBackground(.hidden)
                    .padding(6)
                    .background(RoundedRectangle(cornerRadius: 8).fill(Color(nsColor: .textBackgroundColor)))
                    .overlay(RoundedRectangle(cornerRadius: 8).stroke(Color.secondary.opacity(0.3)))
                    .onPasteCommand(of: [.png, .tiff]) { _ in
                        if let image = CoordinatorChatImagePaste.writeImage(
                            from: NSPasteboard.general, into: attachmentDirectory
                        ) {
                            model.attach(image)
                        }
                    }
                if model.status == .working || model.status == .waitingForPermission {
                    Button("Stop", role: .cancel) { model.stop() }
                        .keyboardShortcut(.escape, modifiers: [])
                }
                Button("Send") { model.send() }
                    .keyboardShortcut(.return, modifiers: [.command])
                    .disabled(!model.canSend)
            }
        }
        .padding(.horizontal, 14)
        .padding(.bottom, 12)
    }
}

private struct CoordinatorChatProbeAttachmentChip: View {
    let image: URL
    let remove: () -> Void

    var body: some View {
        HStack(spacing: 4) {
            if let thumbnail = NSImage(contentsOf: image) {
                Image(nsImage: thumbnail).resizable().scaledToFit().frame(width: 28, height: 28)
            }
            Text(image.lastPathComponent).font(.caption2).lineLimit(1).frame(maxWidth: 120)
            Button(action: remove) { Image(systemName: "xmark.circle.fill") }
                .buttonStyle(.plain)
                .accessibilityLabel("Remove image")
        }
        .padding(4)
        .background(RoundedRectangle(cornerRadius: 6).fill(Color.secondary.opacity(0.12)))
    }
}

private struct CoordinatorChatProbeRow: View {
    let item: CoordinatorChatProbeItem
    let answer: (String, String) -> Void

    var body: some View {
        switch item.kind {
        case .operatorMessage(let text, let images):
            VStack(alignment: .leading, spacing: 6) {
                Text("You").font(.caption).foregroundStyle(.secondary)
                if !text.isEmpty {
                    Text(text).textSelection(.enabled)
                }
                ForEach(images, id: \.self) { image in
                    if let picture = NSImage(contentsOf: image) {
                        Image(nsImage: picture).resizable().scaledToFit().frame(maxWidth: 220, maxHeight: 160)
                            .clipShape(RoundedRectangle(cornerRadius: 6))
                    }
                }
            }
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(RoundedRectangle(cornerRadius: 8).fill(Color.accentColor.opacity(0.10)))
        case .agentMessage(let text, let streaming):
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    Text("Coordinator").font(.caption).foregroundStyle(.secondary)
                    if streaming {
                        Text("streaming").font(.caption2).foregroundStyle(.orange)
                    }
                }
                StructuredText(markdown: text)
                    .bossMarkdown()
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        case .thought(let text):
            DisclosureGroup("Thinking") {
                Text(text).font(.callout).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .font(.caption)
        case .tool(let tool):
            CoordinatorChatProbeToolRow(tool: tool)
        case .permission(let permission):
            CoordinatorChatProbePermissionCard(permission: permission, answer: answer)
        case .marker(let text):
            HStack(spacing: 8) {
                VStack { Divider() }
                Text(text).font(.caption2).foregroundStyle(.secondary).fixedSize()
                VStack { Divider() }
            }
        case .failure(let text):
            Label(text, systemImage: "exclamationmark.triangle.fill")
                .font(.callout)
                .foregroundStyle(.red)
        }
    }
}

/// One generic row per tool call: title, kind, status, and a bounded body.
private struct CoordinatorChatProbeToolRow: View {
    let tool: CoordinatorChatProbeItem.Tool

    private var symbol: String {
        switch tool.status {
        case "completed": "checkmark.circle.fill"
        case "failed": "xmark.octagon.fill"
        case "in_progress": "arrow.triangle.2.circlepath"
        default: "circle.dotted"
        }
    }

    private var tint: Color {
        switch tool.status {
        case "completed": .green
        case "failed": .red
        default: .secondary
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Image(systemName: symbol).foregroundStyle(tint)
                Text(tool.title).font(.callout.weight(.medium)).lineLimit(2)
                Spacer()
                Text("\(tool.kind) · \(tool.status)").font(.caption2).foregroundStyle(.secondary)
            }
            if !tool.input.isEmpty {
                Text(tool.input).font(.system(.caption, design: .monospaced)).lineLimit(4)
                    .foregroundStyle(.secondary)
            }
            if !tool.output.isEmpty {
                Text(tool.output).font(.system(.caption, design: .monospaced)).lineLimit(8)
                    .textSelection(.enabled)
            }
        }
        .padding(8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.secondary.opacity(0.08)))
    }
}

/// A permission request with the agent's own options as buttons.
private struct CoordinatorChatProbePermissionCard: View {
    let permission: CoordinatorChatProbeItem.Permission
    let answer: (String, String) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label("Permission requested", systemImage: "hand.raised.fill").font(.callout.weight(.semibold))
            Text(permission.title).font(.callout)
            if !permission.input.isEmpty {
                Text(permission.input).font(.system(.caption, design: .monospaced)).lineLimit(6)
                    .foregroundStyle(.secondary)
            }
            switch permission.state {
            case .pending:
                HStack(spacing: 8) {
                    ForEach(permission.options) { option in
                        Button(option.name) { answer(permission.requestID, option.id) }
                    }
                }
            case .answered(let name):
                Label("Answered: \(name)", systemImage: "checkmark.seal.fill").font(.caption)
                    .foregroundStyle(.secondary)
            case .cancelled:
                Label("Cancelled with the turn", systemImage: "slash.circle").font(.caption)
                    .foregroundStyle(.secondary)
            case .withdrawn:
                Label("Withdrawn by the agent", systemImage: "arrow.uturn.backward.circle").font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.orange.opacity(0.12)))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(Color.orange.opacity(0.5)))
    }
}
