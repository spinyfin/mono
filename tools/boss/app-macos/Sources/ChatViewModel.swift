import Foundation

struct PendingPermission: Identifiable {
    let id: String
    let agentId: String
    let title: String
}

@MainActor
final class ChatViewModel: ObservableObject {
    @Published var agents: [Agent] = []
    @Published var selectedAgentID: String?
    @Published var draft: String = ""
    @Published var isConnected: Bool = false
    @Published var pendingPermission: PendingPermission?

    var selectedAgent: Agent? {
        guard let id = selectedAgentID else { return nil }
        return agents.first { $0.id == id }
    }

    var selectedAgentTimeline: [TranscriptItem] {
        selectedAgent?.timeline ?? []
    }

    var isSelectedAgentSending: Bool {
        selectedAgent?.isSending ?? false
    }

    var isSelectedAgentReady: Bool {
        selectedAgent?.isReady ?? false
    }

    private let engine: EngineClient
    private let processController = EngineProcessController()
    private let socketPath: String
    private let showSystemMessages: Bool
    private var didStart = false
    private var didStartEngine = false
    private var hasConnectedOnce = false
    private var permissionQueue: [PendingPermission] = []

    private let maxTerminalOutputChars = 200_000

    init(
        socketPath: String = ProcessInfo.processInfo.environment["BOSS_SOCKET_PATH"]
            ?? "/tmp/boss-engine.sock"
    ) {
        self.socketPath = socketPath
        let showSystem = ProcessInfo.processInfo.environment["BOSS_SHOW_SYSTEM_MESSAGES"] ?? ""
        showSystemMessages = showSystem == "1" || showSystem.lowercased() == "true"
        engine = EngineClient(socketPath: socketPath)

        processController.onOutputLine = { [weak self] line in
            self?.appendSystemMessage(line)
        }

        engine.onEvent = { [weak self] event in
            self?.handle(event)
        }
    }

    deinit {
        processController.stop()
        engine.stop()
    }

    func createAgent(name: String? = nil) {
        engine.sendCreateAgent(name: name)
    }

    func sendDraft() {
        guard let agentId = selectedAgentID else { return }
        guard isSelectedAgentReady else { return }
        let trimmed = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }

        appendMessage(agentId: agentId, role: .user, text: trimmed)
        mutateAgent(agentId) { $0.isSending = true; $0.activeAssistantMessageID = nil }
        engine.sendPrompt(agentId: agentId, text: trimmed)
        draft = ""
    }

    func startIfNeeded() {
        guard !didStart else { return }
        didStart = true

        let autostart = ProcessInfo.processInfo.environment["BOSS_ENGINE_AUTOSTART"] != "0"
        if autostart {
            let socketPath = self.socketPath
            let processController = self.processController
            DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                do {
                    try processController.start(socketPath: socketPath)
                    DispatchQueue.main.async {
                        self?.startEngineIfNeeded()
                    }
                } catch {
                    DispatchQueue.main.async {
                        self?.appendSystemMessage(
                            "Failed to launch engine: \(error.localizedDescription)",
                            alwaysShow: true
                        )
                    }
                }
            }
        } else {
            startEngineIfNeeded()
        }
    }

    func respondToPendingPermission(granted: Bool) {
        guard let pending = pendingPermission else { return }
        engine.sendPermissionResponse(agentId: pending.agentId, id: pending.id, granted: granted)
        appendSystemMessage(
            "[permission] \(granted ? "allowed" : "denied"): \(pending.title)",
            agentId: pending.agentId
        )
        self.pendingPermission = nil
        showNextPermissionIfNeeded()
    }

    // MARK: - Event Handling

    private func handle(_ event: EngineEvent) {
        switch event {
        case .connected:
            isConnected = true
            hasConnectedOnce = true
            // Auto-create the first agent on connect
            if agents.isEmpty {
                createAgent()
            }
        case .disconnected:
            isConnected = false
            for i in agents.indices {
                agents[i].isSending = false
                agents[i].activeAssistantMessageID = nil
            }
        case .agentCreated(let agentId, let name):
            let agent = Agent(id: agentId, name: name, isReady: false)
            agents.append(agent)
            if selectedAgentID == nil {
                selectedAgentID = agentId
            }
        case .agentReady(let agentId):
            mutateAgent(agentId) { $0.isReady = true }
        case .agentList(let list):
            for entry in list {
                if !agents.contains(where: { $0.id == entry.id }) {
                    agents.append(Agent(id: entry.id, name: entry.name, isReady: true))
                }
            }
            if selectedAgentID == nil, let first = agents.first {
                selectedAgentID = first.id
            }
        case .agentRemoved(let agentId):
            agents.removeAll { $0.id == agentId }
            if selectedAgentID == agentId {
                selectedAgentID = agents.first?.id
            }
        case .chunk(let agentId, let text):
            appendAssistantChunk(agentId: agentId, text: text)
        case .done(let agentId, let stopReason):
            mutateAgent(agentId) { $0.isSending = false; $0.activeAssistantMessageID = nil }
            appendSystemMessage("[done] \(stopReason)", agentId: agentId)
        case .toolCall(let agentId, let name, let status):
            appendSystemMessage("[tool] \(name) (\(status))", agentId: agentId)
        case .terminalStarted(let agentId, let id, let title, let command, let cwd):
            mutateAgent(agentId) { $0.activeAssistantMessageID = nil }
            upsertTerminalActivity(agentId: agentId, id: id, title: title, command: command, cwd: cwd)
        case .terminalOutput(let agentId, let id, let text):
            appendTerminalOutput(agentId: agentId, id: id, text: text)
        case .terminalDone(let agentId, let id, let exitCode, let signal):
            completeTerminalActivity(agentId: agentId, id: id, exitCode: exitCode, signal: signal)
        case .permissionRequest(let agentId, let id, let title):
            enqueuePermission(agentId: agentId, id: id, title: title)
        case .error(let agentId, let message):
            if let agentId {
                mutateAgent(agentId) { $0.isSending = false; $0.activeAssistantMessageID = nil }
            }
            if shouldSuppressSocketStartupError(message) { return }
            if let agentId {
                appendSystemMessage("[error] \(message)", agentId: agentId, alwaysShow: true)
            }
        }
    }

    // MARK: - Private Helpers

    private func startEngineIfNeeded() {
        guard !didStartEngine else { return }
        didStartEngine = true
        engine.start()
    }

    private func shouldSuppressSocketStartupError(_ message: String) -> Bool {
        guard !showSystemMessages, !hasConnectedOnce else { return false }
        return message.hasPrefix("socket failed:") || message.hasPrefix("socket waiting:")
    }

    private func agentIndex(_ agentId: String) -> Int? {
        agents.firstIndex { $0.id == agentId }
    }

    private func mutateAgent(_ agentId: String, _ body: (inout Agent) -> Void) {
        guard let index = agentIndex(agentId) else { return }
        body(&agents[index])
    }

    private func appendMessage(agentId: String, role: ChatRole, text: String) {
        mutateAgent(agentId) {
            $0.timeline.append(.message(ChatMessage(role: role, text: text)))
        }
    }

    private func appendSystemMessage(_ text: String, agentId: String? = nil, alwaysShow: Bool = false) {
        guard alwaysShow || showSystemMessages else { return }
        if let agentId {
            appendMessage(agentId: agentId, role: .system, text: text)
        }
    }

    private func enqueuePermission(agentId: String, id: String, title: String) {
        let request = PendingPermission(id: id, agentId: agentId, title: title)
        if pendingPermission == nil {
            pendingPermission = request
        } else {
            permissionQueue.append(request)
        }
    }

    private func showNextPermissionIfNeeded() {
        guard pendingPermission == nil, !permissionQueue.isEmpty else { return }
        pendingPermission = permissionQueue.removeFirst()
    }

    private func appendAssistantChunk(agentId: String, text: String) {
        guard let agentIdx = agentIndex(agentId) else { return }
        let agent = agents[agentIdx]

        if let msgId = agent.activeAssistantMessageID,
           let timelineIdx = messageIndex(in: agents[agentIdx].timeline, for: msgId) {
            guard case .message(var message) = agents[agentIdx].timeline[timelineIdx] else { return }
            message.text += text
            agents[agentIdx].timeline[timelineIdx] = .message(message)
            return
        }

        let message = ChatMessage(role: .assistant, text: text)
        agents[agentIdx].activeAssistantMessageID = message.id
        agents[agentIdx].timeline.append(.message(message))
    }

    private func messageIndex(in timeline: [TranscriptItem], for id: UUID) -> Int? {
        timeline.firstIndex { item in
            guard case .message(let message) = item else { return false }
            return message.id == id
        }
    }

    private func upsertTerminalActivity(agentId: String, id: String, title: String, command: String, cwd: String?) {
        let index = ensureTerminalActivity(agentId: agentId, id: id)
        guard let agentIdx = agentIndex(agentId),
              case .terminal(var activity) = agents[agentIdx].timeline[index] else { return }
        activity.title = title
        if !command.isEmpty { activity.command = command }
        if let cwd { activity.cwd = cwd }
        activity.status = "Running…"
        agents[agentIdx].timeline[index] = .terminal(activity)
    }

    private func appendTerminalOutput(agentId: String, id: String, text: String) {
        let index = ensureTerminalActivity(agentId: agentId, id: id)
        guard let agentIdx = agentIndex(agentId),
              case .terminal(var activity) = agents[agentIdx].timeline[index] else { return }
        activity.output += text
        if activity.output.count > maxTerminalOutputChars {
            let overflow = activity.output.count - maxTerminalOutputChars
            activity.output.removeFirst(overflow)
        }
        agents[agentIdx].timeline[index] = .terminal(activity)
    }

    private func completeTerminalActivity(agentId: String, id: String, exitCode: Int?, signal: String?) {
        let index = ensureTerminalActivity(agentId: agentId, id: id)
        guard let agentIdx = agentIndex(agentId),
              case .terminal(var activity) = agents[agentIdx].timeline[index] else { return }
        if let exitCode {
            activity.status = exitCode == 0 ? "Done" : "Failed (exit \(exitCode))"
        } else if let signal, !signal.isEmpty {
            activity.status = "Terminated (signal \(signal))"
        } else {
            activity.status = "Done"
        }
        agents[agentIdx].timeline[index] = .terminal(activity)
    }

    private func ensureTerminalActivity(agentId: String, id: String) -> Int {
        guard let agentIdx = agentIndex(agentId) else {
            // Should not happen; create agent on the fly as fallback
            let agent = Agent(id: agentId, name: agentId)
            agents.append(agent)
            return ensureTerminalActivity(agentId: agentId, id: id)
        }

        if let index = agents[agentIdx].terminalEntryIndexByID[id],
           index < agents[agentIdx].timeline.count,
           case .terminal = agents[agentIdx].timeline[index] {
            return index
        }

        let activity = TerminalActivity(
            id: id, title: "Terminal command", command: "", cwd: nil, output: "", status: "Running…"
        )
        let index = agents[agentIdx].timeline.count
        agents[agentIdx].timeline.append(.terminal(activity))
        agents[agentIdx].terminalEntryIndexByID[id] = index
        return index
    }
}
