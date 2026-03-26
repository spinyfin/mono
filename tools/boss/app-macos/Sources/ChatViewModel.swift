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
    @Published var selectedWorkProductID: String?
    @Published var selectedWorkProjectFilterID: String?
    @Published var selectedWorkCardID: String?
    @Published var includeWorkChores = true
    @Published var workShowBlockedOnly = false
    @Published var selectedWorkNodeID: WorkNodeID?
    @Published var pendingWorkCreateRequest: WorkCreateRequest?
    @Published var pendingWorkEditRequest: WorkEditRequest?
    @Published var workErrorMessage: String?
    @Published var workSearchText: String = ""

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
        guard let productID = currentSelectedProductID else { return nil }
        return product(withID: productID)
    }

    var selectedProject: WorkProject? {
        guard let projectID = selectedWorkProjectFilterID else { return nil }
        return project(withID: projectID)
    }

    var selectedTask: WorkTask? {
        guard let taskID = selectedWorkCardID else { return nil }
        return task(withID: taskID)
    }

    var projectsForSelectedProduct: [WorkProject] {
        guard let productID = currentSelectedProductID else { return [] }
        return (projectsByProductID[productID] ?? []).sorted(by: projectSort)
    }

    var visibleWorkItems: [WorkTask] {
        guard let productID = currentSelectedProductID else { return [] }

        let query = workSearchText.trimmingCharacters(in: .whitespacesAndNewlines)
        let selectedProjectID = selectedWorkProjectFilterID

        var items: [WorkTask] = []
        for project in projectsForSelectedProduct {
            guard selectedProjectID == nil || selectedProjectID == project.id else { continue }
            items.append(contentsOf: (tasksByProjectID[project.id] ?? []).sorted(by: taskSort))
        }
        if includeWorkChores && selectedProjectID == nil {
            items.append(contentsOf: (choresByProductID[productID] ?? []).sorted(by: taskSort))
        }

        if workShowBlockedOnly {
            items = items.filter { $0.status == "blocked" }
        }

        guard !query.isEmpty else {
            return items
        }

        return items.filter { item in
            item.name.localizedCaseInsensitiveContains(query)
                || item.description.localizedCaseInsensitiveContains(query)
                || (item.prURL?.localizedCaseInsensitiveContains(query) ?? false)
                || (projectName(for: item.projectID)?.localizedCaseInsensitiveContains(query) ?? false)
                || item.status.localizedCaseInsensitiveContains(query)
        }
    }

    private let engine: EngineClient
    private let processController = EngineProcessController()
    private let socketPath: String
    private let showSystemMessages: Bool
    private var didStart = false
    private var didStartEngine = false
    private var hasConnectedOnce = false
    private var permissionQueue: [PendingPermission] = []
    private let defaults = UserDefaults.standard

    private let maxTerminalOutputChars = 200_000
    private let navigationModeDefaultsKey = "boss.navigationMode"

    init(
        socketPath: String = ProcessInfo.processInfo.environment["BOSS_SOCKET_PATH"]
            ?? "/tmp/boss-engine.sock"
    ) {
        self.socketPath = socketPath
        let showSystem = ProcessInfo.processInfo.environment["BOSS_SHOW_SYSTEM_MESSAGES"] ?? ""
        showSystemMessages = showSystem == "1" || showSystem.lowercased() == "true"
        engine = EngineClient(socketPath: socketPath)

        if let rawMode = defaults.string(forKey: navigationModeDefaultsKey),
           let persistedMode = NavigationMode(rawValue: rawMode) {
            navigationMode = persistedMode
        }

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
        defaults.set(mode.rawValue, forKey: navigationModeDefaultsKey)
        if mode == .work {
            refreshWork()
        }
    }

    func selectWorkProduct(_ productID: String) {
        guard selectedWorkProductID != productID else { return }
        selectedWorkProductID = productID
        selectedWorkProjectFilterID = nil
        selectedWorkCardID = nil
        workErrorMessage = nil
        if isConnected {
            engine.sendGetWorkTree(productId: productID)
        }
    }

    func selectWorkProjectFilter(_ projectID: String?) {
        selectedWorkProjectFilterID = projectID
        if let selectedTask, !isTaskVisible(selectedTask) {
            selectedWorkCardID = nil
        }
    }

    func selectWorkCard(_ taskID: String?) {
        selectedWorkCardID = taskID
        guard let taskID, let task = task(withID: taskID) else { return }
        selectedWorkProductID = task.productID
    }

    func setIncludeWorkChores(_ include: Bool) {
        includeWorkChores = include
        if let selectedTask, !isTaskVisible(selectedTask) {
            selectedWorkCardID = nil
        }
    }

    func setWorkShowBlockedOnly(_ show: Bool) {
        workShowBlockedOnly = show
        if let selectedTask, !isTaskVisible(selectedTask) {
            selectedWorkCardID = nil
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
        guard let project = taskCreationProject else { return }
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

    func presentEditSelectedWorkItem() {
        if let task = selectedTask {
            pendingWorkEditRequest = WorkEditRequest(item: task.isChore ? .chore(task) : .task(task))
        } else if let project = selectedProject {
            pendingWorkEditRequest = WorkEditRequest(item: .project(project))
        } else if let product = selectedProduct {
            pendingWorkEditRequest = WorkEditRequest(item: .product(product))
        }
    }

    func dismissWorkEditRequest() {
        pendingWorkEditRequest = nil
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

    func submitWorkEditRequest(
        _ request: WorkEditRequest,
        name: String,
        description: String,
        status: String,
        repoRemoteURL: String = "",
        goal: String = "",
        priority: String = "",
        prURL: String = ""
    ) {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedName.isEmpty else { return }

        var patch: [String: Any] = [
            "name": trimmedName,
            "description": description,
            "status": status,
        ]

        let id: String
        switch request.item {
        case .product(let product):
            id = product.id
            patch["repo_remote_url"] = repoRemoteURL
        case .project(let project):
            id = project.id
            patch["goal"] = goal
            patch["priority"] = priority
        case .task(let task), .chore(let task):
            id = task.id
            patch["pr_url"] = prURL
        }

        engine.sendUpdateWorkItem(id: id, patch: patch)
        pendingWorkEditRequest = nil
    }

    func deleteSelectedWorkItem() {
        guard let task = selectedTask else { return }
        engine.sendDeleteWorkItem(id: task.id)
    }

    func moveSelectedTask(offset: Int) {
        guard let task = selectedTask,
              !task.isChore,
              let projectID = task.projectID,
              var tasks = tasksByProjectID[projectID]?.sorted(by: taskSort),
              let currentIndex = tasks.firstIndex(where: { $0.id == task.id })
        else {
            return
        }

        let destination = currentIndex + offset
        guard tasks.indices.contains(destination) else { return }

        tasks.swapAt(currentIndex, destination)
        engine.sendReorderProjectTasks(projectId: projectID, taskIds: tasks.map(\.id))
    }

    func moveTask(_ taskID: String, to column: WorkBoardColumnKey) {
        guard let task = task(withID: taskID) else { return }
        let targetStatus = column.targetStatus
        guard task.status != targetStatus else { return }
        engine.sendUpdateWorkItem(id: task.id, patch: ["status": targetStatus])
    }

    func toggleBlocked(for taskID: String) {
        guard let task = task(withID: taskID) else { return }
        let nextStatus: String
        switch task.status {
        case "blocked":
            nextStatus = "active"
        case "active":
            nextStatus = "blocked"
        default:
            return
        }
        engine.sendUpdateWorkItem(id: task.id, patch: ["status": nextStatus])
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
            if let selectedWorkProductID,
               !self.products.contains(where: { $0.id == selectedWorkProductID }) {
                self.selectedWorkProductID = nil
                self.selectedWorkProjectFilterID = nil
                self.selectedWorkCardID = nil
            }
            if currentSelectedProductID == nil, let first = self.products.first {
                self.selectedWorkProductID = first.id
                engine.sendGetWorkTree(productId: first.id)
            } else if let productID = currentSelectedProductID {
                engine.sendGetWorkTree(productId: productID)
            }
        case .projectsList(let productId, let projects):
            projectsByProductID[productId] = projects.sorted(by: projectSort)
        case .workTree(let product, let projects, let tasks, let chores):
            upsertProduct(product)
            if currentSelectedProductID == nil {
                selectedWorkProductID = product.id
            }
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
            reconcileWorkSelection()
            workErrorMessage = nil
        case .workItemCreated(let item):
            handleCreatedWorkItem(item)
        case .workItemUpdated(let item):
            handleUpdatedWorkItem(item)
        case .projectTasksReordered(let projectId, _):
            if let productID = productID(forProjectID: projectId) {
                engine.sendGetWorkTree(productId: productID)
            }
        case .workItemDeleted(let id):
            let deletedTask = task(withID: id)
            if selectedTask?.id == id {
                selectedWorkCardID = nil
            }
            if let productID = deletedTask?.productID ?? currentSelectedProductID {
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
        selectedWorkProductID
    }

    private var taskCreationProject: WorkProject? {
        if let selectedProject {
            return selectedProject
        }
        if let selectedTask, let projectID = selectedTask.projectID {
            return project(withID: projectID)
        }
        return nil
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

    func projectName(for projectID: String?) -> String? {
        guard let projectID else { return nil }
        return project(withID: projectID)?.name
    }

    func workItems(in column: WorkBoardColumnKey) -> [WorkTask] {
        visibleWorkItems
            .filter { $0.boardColumn == column }
            .sorted(by: boardTaskSort)
    }

    func isTaskVisible(_ task: WorkTask) -> Bool {
        workItems(in: task.boardColumn).contains(where: { $0.id == task.id })
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
            selectedWorkProductID = product.id
            selectedWorkProjectFilterID = nil
            selectedWorkCardID = nil
            engine.sendGetWorkTree(productId: product.id)
        case .project(let project):
            selectedWorkProductID = project.productID
            selectedWorkProjectFilterID = project.id
            selectedWorkCardID = nil
            engine.sendGetWorkTree(productId: project.productID)
        case .task(let task):
            selectedWorkProductID = task.productID
            selectedWorkProjectFilterID = task.projectID
            selectedWorkCardID = task.id
            engine.sendGetWorkTree(productId: task.productID)
        case .chore(let task):
            selectedWorkProductID = task.productID
            selectedWorkCardID = task.id
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

    private func reconcileWorkSelection() {
        guard let selectedWorkProductID else { return }

        if !products.contains(where: { $0.id == selectedWorkProductID }) {
            self.selectedWorkProductID = products.first?.id
        }

        if let selectedWorkProjectFilterID,
           project(withID: selectedWorkProjectFilterID)?.productID != selectedWorkProductID {
            self.selectedWorkProjectFilterID = nil
        }

        if let selectedTask, !isTaskVisible(selectedTask) {
            selectedWorkCardID = nil
        }
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

private func boardTaskSort(_ lhs: WorkTask, _ rhs: WorkTask) -> Bool {
    if lhs.status != rhs.status {
        if lhs.status == "blocked" {
            return true
        }
        if rhs.status == "blocked" {
            return false
        }
    }
    return taskSort(lhs, rhs)
}
