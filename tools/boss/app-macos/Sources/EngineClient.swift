import Foundation
import Network

enum EngineEvent {
    case connected
    case disconnected
    case productsList(products: [WorkProduct])
    case projectsList(productId: String, projects: [WorkProject])
    case workTree(product: WorkProduct, projects: [WorkProject], tasks: [WorkTask], chores: [WorkTask])
    case workItemCreated(item: WorkItemPayload)
    case workItemUpdated(item: WorkItemPayload)
    case projectTasksReordered(projectId: String, taskIds: [String])
    case workItemDeleted(id: String)
    case workError(message: String)
    case agentCreated(agentId: String, name: String)
    case agentList(agents: [(id: String, name: String)])
    case agentRemoved(agentId: String)
    case chunk(agentId: String, text: String)
    case done(agentId: String, stopReason: String)
    case toolCall(agentId: String, name: String, status: String)
    case terminalStarted(agentId: String, id: String, title: String, command: String, cwd: String?)
    case terminalOutput(agentId: String, id: String, text: String)
    case terminalDone(agentId: String, id: String, exitCode: Int?, signal: String?)
    case permissionRequest(agentId: String, id: String, title: String)
    case agentReady(agentId: String)
    case error(agentId: String?, message: String)
}

final class EngineClient: @unchecked Sendable {
    var onEvent: (@MainActor @Sendable (EngineEvent) -> Void)?

    private let socketPath: String
    private let queue = DispatchQueue(label: "BossMacApp.EngineClient")
    private var connection: NWConnection?
    private var buffer = Data()
    private var shouldReconnect = false

    init(socketPath: String) {
        self.socketPath = socketPath
    }

    func start() {
        shouldReconnect = true
        connect()
    }

    func stop() {
        shouldReconnect = false
        connection?.cancel()
        connection = nil
        buffer.removeAll(keepingCapacity: false)
    }

    private func connect() {
        guard connection == nil else {
            return
        }

        let parameters = NWParameters(tls: nil, tcp: NWProtocolTCP.Options())
        let endpoint = NWEndpoint.unix(path: socketPath)
        let connection = NWConnection(to: endpoint, using: parameters)
        self.connection = connection

        connection.stateUpdateHandler = { [weak self] (state: NWConnection.State) in
            guard let self else { return }
            switch state {
            case .ready:
                self.emit(.connected)
                self.receiveNext()
            case .waiting(let error):
                self.emit(.error(agentId: nil, message: "socket waiting: \(error.localizedDescription)"))
                self.connection = nil
                connection.cancel()
                self.emit(.disconnected)
                self.scheduleReconnect()
            case .failed(let error):
                self.emit(.error(agentId: nil, message: "socket failed: \(error.localizedDescription)"))
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
            case .cancelled:
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
            default:
                break
            }
        }

        connection.start(queue: queue)
    }

    func sendCreateAgent(name: String?) {
        var payload: [String: Any] = ["type": "create_agent"]
        if let name {
            payload["name"] = name
        }
        sendLine(payload)
    }

    func sendListAgents() {
        sendLine(["type": "list_agents"])
    }

    func sendRemoveAgent(agentId: String) {
        sendLine([
            "type": "remove_agent",
            "agent_id": agentId,
        ])
    }

    func sendPrompt(agentId: String, text: String) {
        sendLine([
            "type": "prompt",
            "agent_id": agentId,
            "text": text,
        ])
    }

    func sendPermissionResponse(agentId: String, id: String, granted: Bool) {
        sendLine([
            "type": "permission_response",
            "agent_id": agentId,
            "id": id,
            "granted": granted,
        ])
    }

    func sendListProducts() {
        sendLine(["type": "list_products"])
    }

    func sendSubscribe(topics: [String]) {
        sendLine([
            "type": "subscribe",
            "topics": topics,
        ])
    }

    func sendUnsubscribe(topics: [String]) {
        sendLine([
            "type": "unsubscribe",
            "topics": topics,
        ])
    }

    func sendGetWorkTree(productId: String) {
        sendLine([
            "type": "get_work_tree",
            "product_id": productId,
        ])
    }

    func sendCreateProduct(name: String, description: String, repoRemoteURL: String) {
        sendLine([
            "type": "create_product",
            "name": name,
            "description": description,
            "repo_remote_url": repoRemoteURL,
        ])
    }

    func sendCreateProject(productId: String, name: String, description: String, goal: String) {
        sendLine([
            "type": "create_project",
            "product_id": productId,
            "name": name,
            "description": description,
            "goal": goal,
        ])
    }

    func sendCreateTask(productId: String, projectId: String, name: String, description: String) {
        sendLine([
            "type": "create_task",
            "product_id": productId,
            "project_id": projectId,
            "name": name,
            "description": description,
        ])
    }

    func sendCreateChore(productId: String, name: String, description: String) {
        sendLine([
            "type": "create_chore",
            "product_id": productId,
            "name": name,
            "description": description,
        ])
    }

    func sendUpdateWorkItem(id: String, patch: [String: Any]) {
        sendLine([
            "type": "update_work_item",
            "id": id,
            "patch": patch,
        ])
    }

    func sendDeleteWorkItem(id: String) {
        sendLine([
            "type": "delete_work_item",
            "id": id,
        ])
    }

    func sendReorderProjectTasks(projectId: String, taskIds: [String]) {
        sendLine([
            "type": "reorder_project_tasks",
            "project_id": projectId,
            "task_ids": taskIds,
        ])
    }

    private func sendLine(_ payload: [String: Any]) {
        guard let connection else {
            emit(.error(agentId: nil, message: "engine connection is not established"))
            return
        }

        do {
            let envelope: [String: Any] = [
                "request_id": UUID().uuidString,
                "payload": payload,
            ]
            var data = try JSONSerialization.data(withJSONObject: envelope, options: [])
            data.append(0x0A)

            connection.send(content: data, completion: .contentProcessed { [weak self] error in
                guard let self else { return }
                if let error {
                    self.emit(.error(agentId: nil, message: "socket send failed: \(error.localizedDescription)"))
                }
            })
        } catch {
            emit(.error(agentId: nil, message: "failed to encode payload: \(error.localizedDescription)"))
        }
    }

    private func receiveNext() {
        connection?.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, isComplete, error in
            guard let self else { return }

            if let error {
                self.emit(.error(agentId: nil, message: "socket receive failed: \(error.localizedDescription)"))
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
                return
            }

            if let data, !data.isEmpty {
                self.buffer.append(data)
                self.consumeLines()
            }

            if isComplete {
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
                return
            }

            self.receiveNext()
        }
    }

    private func consumeLines() {
        while let newline = buffer.firstIndex(of: 0x0A) {
            let lineData = buffer[..<newline]
            buffer.removeSubrange(...newline)

            guard !lineData.isEmpty else {
                continue
            }

            guard let envelope = try? JSONSerialization.jsonObject(with: Data(lineData)) as? [String: Any],
                let payload = envelope["payload"] as? [String: Any],
                let type = payload["type"] as? String
            else {
                emit(.error(agentId: nil, message: "received invalid JSON message from engine"))
                continue
            }

            let agentId = payload["agent_id"] as? String

            switch type {
            case "products_list":
                let products = (payload["products"] as? [[String: Any]] ?? []).compactMap(parseProduct)
                emit(.productsList(products: products))
            case "projects_list":
                let productId = payload["product_id"] as? String ?? ""
                let projects = (payload["projects"] as? [[String: Any]] ?? []).compactMap(parseProject)
                emit(.projectsList(productId: productId, projects: projects))
            case "work_tree":
                guard let productPayload = payload["product"] as? [String: Any],
                      let product = parseProduct(productPayload)
                else {
                    emit(.error(agentId: nil, message: "received invalid work tree payload from engine"))
                    break
                }
                let projects = (payload["projects"] as? [[String: Any]] ?? []).compactMap(parseProject)
                let tasks = (payload["tasks"] as? [[String: Any]] ?? []).compactMap(parseTask)
                let chores = (payload["chores"] as? [[String: Any]] ?? []).compactMap(parseTask)
                emit(.workTree(product: product, projects: projects, tasks: tasks, chores: chores))
            case "work_item_created":
                guard let itemPayload = payload["item"] as? [String: Any],
                      let item = parseWorkItem(itemPayload)
                else {
                    emit(.error(agentId: nil, message: "received invalid work item payload from engine"))
                    break
                }
                emit(.workItemCreated(item: item))
            case "work_item_updated":
                guard let itemPayload = payload["item"] as? [String: Any],
                      let item = parseWorkItem(itemPayload)
                else {
                    emit(.error(agentId: nil, message: "received invalid work item payload from engine"))
                    break
                }
                emit(.workItemUpdated(item: item))
            case "project_tasks_reordered":
                let projectId = payload["project_id"] as? String ?? ""
                let taskIds = payload["task_ids"] as? [String] ?? []
                emit(.projectTasksReordered(projectId: projectId, taskIds: taskIds))
            case "work_item_deleted":
                let id = payload["id"] as? String ?? ""
                guard !id.isEmpty else {
                    break
                }
                emit(.workItemDeleted(id: id))
            case "work_error":
                let message = payload["message"] as? String ?? "unknown work error"
                emit(.workError(message: message))
            case "agent_created":
                let aid = agentId ?? ""
                let name = payload["name"] as? String ?? ""
                emit(.agentCreated(agentId: aid, name: name))
            case "agent_ready":
                emit(.agentReady(agentId: agentId ?? ""))
            case "agent_list":
                var agents: [(id: String, name: String)] = []
                if let list = payload["agents"] as? [[String: Any]] {
                    for entry in list {
                        let aid = entry["agent_id"] as? String ?? ""
                        let name = entry["name"] as? String ?? ""
                        agents.append((id: aid, name: name))
                    }
                }
                emit(.agentList(agents: agents))
            case "agent_removed":
                emit(.agentRemoved(agentId: agentId ?? ""))
            case "chunk":
                if let text = payload["text"] as? String, let aid = agentId {
                    emit(.chunk(agentId: aid, text: text))
                }
            case "done":
                let stopReason = payload["stop_reason"] as? String ?? "unknown"
                emit(.done(agentId: agentId ?? "", stopReason: stopReason))
            case "tool_call":
                let name = payload["name"] as? String ?? "tool"
                let status = payload["status"] as? String ?? "update"
                emit(.toolCall(agentId: agentId ?? "", name: name, status: status))
            case "terminal_started":
                let id = payload["id"] as? String ?? UUID().uuidString
                let title = payload["title"] as? String ?? "Terminal"
                let command = payload["command"] as? String ?? ""
                let cwd = payload["cwd"] as? String
                emit(.terminalStarted(agentId: agentId ?? "", id: id, title: title, command: command, cwd: cwd))
            case "terminal_output":
                let id = payload["id"] as? String ?? ""
                let text = payload["text"] as? String ?? ""
                guard !id.isEmpty, !text.isEmpty else {
                    break
                }
                emit(.terminalOutput(agentId: agentId ?? "", id: id, text: text))
            case "terminal_done":
                let id = payload["id"] as? String ?? ""
                guard !id.isEmpty else {
                    break
                }
                let exitCode = (payload["exit_code"] as? NSNumber)?.intValue
                let signal = payload["signal"] as? String
                emit(.terminalDone(agentId: agentId ?? "", id: id, exitCode: exitCode, signal: signal))
            case "permission_request":
                let id = payload["id"] as? String ?? ""
                let title = payload["title"] as? String ?? "Permission"
                emit(.permissionRequest(agentId: agentId ?? "", id: id, title: title))
            case "error":
                let message = payload["message"] as? String ?? "unknown engine error"
                emit(.error(agentId: agentId, message: message))
            default:
                break
            }
        }
    }

    private func emit(_ event: EngineEvent) {
        Task { @MainActor in
            onEvent?(event)
        }
    }

    private func scheduleReconnect() {
        guard shouldReconnect else {
            return
        }

        queue.asyncAfter(deadline: .now() + 1.0) { [weak self] in
            guard let self, self.shouldReconnect, self.connection == nil else {
                return
            }
            self.connect()
        }
    }

    private func parseProduct(_ payload: [String: Any]) -> WorkProduct? {
        guard let id = payload["id"] as? String,
              let name = payload["name"] as? String,
              let slug = payload["slug"] as? String,
              let description = payload["description"] as? String,
              let status = payload["status"] as? String,
              let createdAt = payload["created_at"] as? String,
              let updatedAt = payload["updated_at"] as? String
        else {
            return nil
        }

        return WorkProduct(
            id: id,
            name: name,
            slug: slug,
            description: description,
            repoRemoteURL: payload["repo_remote_url"] as? String,
            status: status,
            createdAt: createdAt,
            updatedAt: updatedAt
        )
    }

    private func parseProject(_ payload: [String: Any]) -> WorkProject? {
        guard let id = payload["id"] as? String,
              let productId = payload["product_id"] as? String,
              let name = payload["name"] as? String,
              let slug = payload["slug"] as? String,
              let description = payload["description"] as? String,
              let goal = payload["goal"] as? String,
              let status = payload["status"] as? String,
              let priority = payload["priority"] as? String,
              let createdAt = payload["created_at"] as? String,
              let updatedAt = payload["updated_at"] as? String
        else {
            return nil
        }

        return WorkProject(
            id: id,
            productID: productId,
            name: name,
            slug: slug,
            description: description,
            goal: goal,
            status: status,
            priority: priority,
            createdAt: createdAt,
            updatedAt: updatedAt
        )
    }

    private func parseTask(_ payload: [String: Any]) -> WorkTask? {
        guard let id = payload["id"] as? String,
              let productId = payload["product_id"] as? String,
              let kind = payload["kind"] as? String,
              let name = payload["name"] as? String,
              let description = payload["description"] as? String,
              let status = payload["status"] as? String,
              let createdAt = payload["created_at"] as? String,
              let updatedAt = payload["updated_at"] as? String
        else {
            return nil
        }

        let ordinal = (payload["ordinal"] as? NSNumber)?.intValue

        return WorkTask(
            id: id,
            productID: productId,
            projectID: payload["project_id"] as? String,
            kind: kind,
            name: name,
            description: description,
            status: status,
            ordinal: ordinal,
            prURL: payload["pr_url"] as? String,
            deletedAt: payload["deleted_at"] as? String,
            createdAt: createdAt,
            updatedAt: updatedAt
        )
    }

    private func parseWorkItem(_ payload: [String: Any]) -> WorkItemPayload? {
        guard let itemType = payload["item_type"] as? String else {
            return nil
        }

        switch itemType {
        case "product":
            guard let product = parseProduct(payload) else { return nil }
            return .product(product)
        case "project":
            guard let project = parseProject(payload) else { return nil }
            return .project(project)
        case "task":
            guard let task = parseTask(payload) else { return nil }
            return .task(task)
        case "chore":
            guard let task = parseTask(payload) else { return nil }
            return .chore(task)
        default:
            return nil
        }
    }
}
