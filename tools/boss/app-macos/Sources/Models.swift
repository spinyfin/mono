import Foundation

enum AgentRole: String {
    case standard
    case boss

    var title: String {
        switch self {
        case .standard:
            return "Agent"
        case .boss:
            return "The Boss"
        }
    }

    var systemImage: String {
        switch self {
        case .standard:
            return "person.circle"
        case .boss:
            return "person.crop.circle.badge.checkmark"
        }
    }
}

struct Agent: Identifiable {
    let id: String
    var name: String
    var role: AgentRole = .standard
    var isReady: Bool = false
    var timeline: [TranscriptItem] = []
    var isSending: Bool = false
    var activeAssistantMessageID: UUID?
    var terminalEntryIndexByID: [String: Int] = [:]

    var isBoss: Bool {
        role == .boss
    }
}

enum ChatRole {
    case user
    case assistant
    case system
}

struct ChatMessage: Identifiable {
    let id: UUID
    let role: ChatRole
    var text: String

    init(id: UUID = UUID(), role: ChatRole, text: String) {
        self.id = id
        self.role = role
        self.text = text
    }
}

struct TerminalActivity: Identifiable {
    let id: String
    var title: String
    var command: String
    var cwd: String?
    var output: String
    var status: String
}

enum TranscriptItem: Identifiable {
    case message(ChatMessage)
    case terminal(TerminalActivity)

    var id: String {
        switch self {
        case .message(let message):
            return "msg-\(message.id.uuidString)"
        case .terminal(let terminal):
            return "terminal-\(terminal.id)"
        }
    }
}

enum NavigationMode: String, CaseIterable, Identifiable {
    case agents = "Agents"
    case work = "Work"

    var id: String { rawValue }
}

struct WorkProduct: Identifiable, Hashable {
    let id: String
    var name: String
    var slug: String
    var description: String
    var repoRemoteURL: String?
    var status: String
    var createdAt: String
    var updatedAt: String
}

struct WorkProject: Identifiable, Hashable {
    let id: String
    let productID: String
    var name: String
    var slug: String
    var description: String
    var goal: String
    var status: String
    var priority: String
    var createdAt: String
    var updatedAt: String
}

struct WorkTask: Identifiable, Hashable {
    let id: String
    let productID: String
    let projectID: String?
    let kind: String
    var name: String
    var description: String
    var status: String
    var ordinal: Int?
    var prURL: String?
    var deletedAt: String?
    var createdAt: String
    var updatedAt: String

    var isChore: Bool {
        kind == "chore"
    }
}

enum WorkNodeID: Hashable {
    case product(String)
    case project(String)
    case task(String)
    case chore(String)
}

enum WorkBoardColumnKey: String, CaseIterable, Identifiable {
    case backlog
    case doing
    case review
    case done

    var id: String { rawValue }

    var title: String {
        switch self {
        case .backlog:
            return "Backlog"
        case .doing:
            return "Doing"
        case .review:
            return "Review"
        case .done:
            return "Done"
        }
    }

    var targetStatus: String {
        switch self {
        case .backlog:
            return "todo"
        case .doing:
            return "active"
        case .review:
            return "in_review"
        case .done:
            return "done"
        }
    }
}

enum WorkBoardGrouping: String, CaseIterable, Identifiable {
    case none
    case project

    var id: String { rawValue }

    var title: String {
        switch self {
        case .none:
            return "Ungrouped"
        case .project:
            return "Project"
        }
    }
}

enum WorkItemPayload {
    case product(WorkProduct)
    case project(WorkProject)
    case task(WorkTask)
    case chore(WorkTask)

    var id: String {
        switch self {
        case .product(let product):
            return product.id
        case .project(let project):
            return project.id
        case .task(let task), .chore(let task):
            return task.id
        }
    }
}

struct WorkSidebarRow: Identifiable {
    let id: WorkNodeID
    let title: String
    let subtitle: String?
    let statusBadge: String?
    let systemImage: String
    let depth: Int
}

enum WorkCreateKind {
    case product
    case project(productID: String)
    case task(productID: String, projectID: String)
    case chore(productID: String)
}

struct WorkCreateRequest: Identifiable {
    let id = UUID()
    let kind: WorkCreateKind
}

struct WorkEditRequest: Identifiable {
    let id = UUID()
    let item: WorkItemPayload
}

struct WorkBoardSection: Identifiable {
    let id: String
    let title: String
    let items: [WorkTask]
}

extension WorkTask {
    var boardColumn: WorkBoardColumnKey {
        switch status {
        case "active", "blocked":
            return .doing
        case "in_review":
            return .review
        case "done":
            return .done
        default:
            return .backlog
        }
    }
}
