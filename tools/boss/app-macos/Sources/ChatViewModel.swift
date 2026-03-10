import Foundation

struct PendingPermission: Identifiable {
    let id: String
    let agentId: String
    let title: String
}

@MainActor
final class ChatViewModel: ObservableObject {
    @Published var navigationMode: NavigationMode = .agents
    @Published var agents: [Agent] = []
    @Published var selectedAgentID: String?
    @Published var draft: String = ""
    @Published var isConnected: Bool = false
    @Published var pendingPermission: PendingPermission?
    @Published var products: [WorkProduct] = []
    @Published var projectsByProductID: [String: [WorkProject]] = [:]
    @Published var tasksByProjectID: [String: [WorkTask]] = [:]
    @Published var choresByProductID: [String: [WorkTask]] = [:]
    @Published var selectedWorkNodeID: WorkNodeID?
    @Published var pendingWorkCreateRequest: WorkCreateRequest?
    @Published var workErrorMessage: String?

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

    var selectedProduct: WorkProduct? {
        if case .product(let productID) = selectedWorkNodeID {
            return product(withID: productID)
        }
        if let productID = currentSelectedProductID {
            return product(withID: productID)
        }
        return nil
    }

    var selectedProject: WorkProject? {
        guard case .project(let projectID) = selectedWorkNodeID else { return nil }
        return project(withID: projectID)
    }

    var selectedTask: WorkTask? {
        switch selectedWorkNodeID {
        case .task(let taskID), .chore(let taskID):
            return task(withID: taskID)
        default:
            return nil
        }
    }

    var workSidebarRows: [WorkSidebarRow] {
        var rows: [WorkSidebarRow] = []

        for product in products.sorted(by: { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }) {
            rows.append(
                WorkSidebarRow(
                    id: .product(product.id),
                    title: product.name,
                    subtitle: product.status.capitalized,
                    systemImage: "shippingbox",
                    depth: 0
                )
            )

            for project in (projectsByProductID[product.id] ?? []).sorted(by: projectSort) {
                rows.append(
                    WorkSidebarRow(
                        id: .project(project.id),
                        title: project.name,
                        subtitle: project.status.capitalized,
                        systemImage: "folder",
                        depth: 1
                    )
                )

                for task in (tasksByProjectID[project.id] ?? []).sorted(by: taskSort) {
                    rows.append(
                        WorkSidebarRow(
                            id: .task(task.id),
                            title: task.name,
                            subtitle: task.status.capitalized,
                            systemImage: "circle.hexagongrid",
                            depth: 2
                        )
                    )
                }
            }

            for chore in (choresByProductID[product.id] ?? []).sorted(by: taskSort) {
                rows.append(
                    WorkSidebarRow(
                        id: .chore(chore.id),
                        title: chore.name,
                        subtitle: "Chore",
                        systemImage: "wrench.and.screwdriver",
                        depth: 1
                    )
                )
            }
        }

        return rows
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

    func setNavigationMode(_ mode: NavigationMode) {
        navigationMode = mode
        if mode == .work {
            refreshWork()
        }
    }

    func selectWorkNode(_ nodeID: WorkNodeID?) {
        selectedWorkNodeID = nodeID
        if let productID = productID(for: nodeID) {
            engine.sendGetWorkTree(productId: productID)
        }
    }

    func presentCreateProduct() {
        pendingWorkCreateRequest = WorkCreateRequest(kind: .product)
    }

    func presentCreateProject() {
        guard let productID = currentSelectedProductID else { return }
        pendingWorkCreateRequest = WorkCreateRequest(kind: .project(productID: productID))
    }

    func presentCreateTask() {
        guard let project = selectedProject else { return }
        pendingWorkCreateRequest = WorkCreateRequest(
            kind: .task(productID: project.productID, projectID: project.id)
        )
    }

    func presentCreateChore() {
        guard let productID = currentSelectedProductID else { return }
        pendingWorkCreateRequest = WorkCreateRequest(kind: .chore(productID: productID))
    }

    func dismissWorkCreateRequest() {
        pendingWorkCreateRequest = nil
    }

    func submitWorkCreateRequest(
        _ request: WorkCreateRequest,
        name: String,
        description: String,
        repoRemoteURL: String = "",
        goal: String = ""
    ) {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedName.isEmpty else { return }

        workErrorMessage = nil
        switch request.kind {
        case .product:
            engine.sendCreateProduct(
                name: trimmedName,
                description: description,
                repoRemoteURL: repoRemoteURL
            )
        case .project(let productID):
            engine.sendCreateProject(
                productId: productID,
                name: trimmedName,
                description: description,
                goal: goal
            )
        case .task(let productID, let projectID):
            engine.sendCreateTask(
                productId: productID,
                projectId: projectID,
                name: trimmedName,
                description: description
            )
        case .chore(let productID):
            engine.sendCreateChore(
                productId: productID,
                name: trimmedName,
                description: description
            )
        }

        pendingWorkCreateRequest = nil
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
        pendingPermission = nil
        showNextPermissionIfNeeded()
    }

    func refreshWork() {
        guard isConnected else { return }
        engine.sendListProducts()
        if let productID = currentSelectedProductID {
            engine.sendGetWorkTree(productId: productID)
        }
    }

    // MARK: - Event Handling

    private func handle(_ event: EngineEvent) {
        switch event {
        case .connected:
            isConnected = true
            hasConnectedOnce = true
            if agents.isEmpty {
                createAgent()
            }
            engine.sendListProducts()
            if let productID = currentSelectedProductID {
                engine.sendGetWorkTree(productId: productID)
            }
        case .disconnected:
            isConnected = false
            for i in agents.indices {
                agents[i].isSending = false
                agents[i].activeAssistantMessageID = nil
            }
        case .productsList(let products):
            self.products = products.sorted(by: { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending })
            if selectedWorkNodeID == nil, let first = self.products.first {
                selectedWorkNodeID = .product(first.id)
                engine.sendGetWorkTree(productId: first.id)
            } else if let productID = currentSelectedProductID {
                engine.sendGetWorkTree(productId: productID)
            }
        case .projectsList(let productId, let projects):
            projectsByProductID[productId] = projects.sorted(by: projectSort)
        case .workTree(let product, let projects, let tasks, let chores):
            upsertProduct(product)
            projectsByProductID[product.id] = projects.sorted(by: projectSort)
            tasksByProjectID = tasksByProjectID.filter { _, existingTasks in
                existingTasks.first?.productID != product.id
            }
            for task in tasks {
                guard let projectID = task.projectID else { continue }
                tasksByProjectID[projectID, default: []].append(task)
            }
            for (projectID, projectTasks) in tasksByProjectID where
                projectTasks.first?.productID == product.id {
                tasksByProjectID[projectID] = projectTasks.sorted(by: taskSort)
            }
            choresByProductID[product.id] = chores.sorted(by: taskSort)
            workErrorMessage = nil
        case .workItemCreated(let item):
            handleCreatedWorkItem(item)
        case .workItemUpdated(let item):
            handleUpdatedWorkItem(item)
        case .projectTasksReordered(let projectId, _):
            if let productID = productID(forProjectID: projectId) {
                engine.sendGetWorkTree(productId: productID)
            }
        case .workItemDeleted:
            if let productID = currentSelectedProductID {
                engine.sendGetWorkTree(productId: productID)
            }
        case .workError(let message):
            workErrorMessage = message
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
            } else {
                workErrorMessage = message
            }
        }
    }

    // MARK: - Private Helpers

    private var currentSelectedProductID: String? {
        productID(for: selectedWorkNodeID)
    }

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

    private func product(withID id: String) -> WorkProduct? {
        products.first { $0.id == id }
    }

    private func project(withID id: String) -> WorkProject? {
        for projects in projectsByProductID.values {
            if let project = projects.first(where: { $0.id == id }) {
                return project
            }
        }
        return nil
    }

    private func task(withID id: String) -> WorkTask? {
        for tasks in tasksByProjectID.values {
            if let task = tasks.first(where: { $0.id == id }) {
                return task
            }
        }
        for chores in choresByProductID.values {
            if let chore = chores.first(where: { $0.id == id }) {
                return chore
            }
        }
        return nil
    }

    private func productID(for nodeID: WorkNodeID?) -> String? {
        switch nodeID {
        case .product(let productID):
            return productID
        case .project(let projectID):
            return project(withID: projectID)?.productID
        case .task(let taskID), .chore(let taskID):
            return task(withID: taskID)?.productID
        case nil:
            return nil
        }
    }

    private func productID(forProjectID projectID: String) -> String? {
        project(withID: projectID)?.productID
    }

    private func upsertProduct(_ product: WorkProduct) {
        if let index = products.firstIndex(where: { $0.id == product.id }) {
            products[index] = product
        } else {
            products.append(product)
            products.sort(by: { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending })
        }
    }

    private func handleCreatedWorkItem(_ item: WorkItemPayload) {
        workErrorMessage = nil
        switch item {
        case .product(let product):
            upsertProduct(product)
            selectedWorkNodeID = .product(product.id)
            engine.sendGetWorkTree(productId: product.id)
        case .project(let project):
            selectedWorkNodeID = .project(project.id)
            engine.sendGetWorkTree(productId: project.productID)
        case .task(let task):
            selectedWorkNodeID = .task(task.id)
            engine.sendGetWorkTree(productId: task.productID)
        case .chore(let task):
            selectedWorkNodeID = .chore(task.id)
            engine.sendGetWorkTree(productId: task.productID)
        }
    }

    private func handleUpdatedWorkItem(_ item: WorkItemPayload) {
        switch item {
        case .product(let product):
            upsertProduct(product)
        case .project(let project):
            engine.sendGetWorkTree(productId: project.productID)
        case .task(let task), .chore(let task):
            engine.sendGetWorkTree(productId: task.productID)
        }
        workErrorMessage = nil
    }
}

private func projectSort(_ lhs: WorkProject, _ rhs: WorkProject) -> Bool {
    if lhs.createdAt == rhs.createdAt {
        return lhs.name.localizedCaseInsensitiveCompare(rhs.name) == .orderedAscending
    }
    return lhs.createdAt < rhs.createdAt
}

private func taskSort(_ lhs: WorkTask, _ rhs: WorkTask) -> Bool {
    switch (lhs.ordinal, rhs.ordinal) {
    case let (left?, right?) where left != right:
        return left < right
    default:
        if lhs.createdAt == rhs.createdAt {
            return lhs.name.localizedCaseInsensitiveCompare(rhs.name) == .orderedAscending
        }
        return lhs.createdAt < rhs.createdAt
    }
}
