import XCTest
@testable import Boss

@MainActor
enum NavigatorTestFixture {
    static let product = WorkProduct(id: "product", name: "Navigator", slug: "navigator", description: "",
                                    repoRemoteURL: nil, status: "active", createdAt: "", updatedAt: "")
    static func task(_ id: String = "root", section: String = "working", order: String = "0",
                     project: String? = nil, rootID: String? = nil, detail: String = "activity", kind: String? = nil) -> WorkTask {
        var task = WorkTask(id: id, productID: product.id, projectID: project,
                            kind: kind ?? (rootID == nil ? "task" : "revision"), name: "Improve navigation \(id)",
                            description: "A readable **brief** for \(id).", status: "active", priority: "medium",
                            ordinal: nil, prURL: "https://github.com/spinyfin/mono/pull/3629", deletedAt: nil,
                            createdAt: "", updatedAt: "")
        var wire: [String: Any] = ["version": 1, "root_task_id": rootID ?? id]
        if rootID == nil {
            wire["root"] = [
                "section": section, "sort_key": order, "default_detail": detail,
                "reason": ["type": "active", "activity": "revising"], "reason_label": "Revising",
                "revisions": ["open": 2, "total": 3, "current_task_id": "revision"],
                "activities": [["execution_id": "execution", "task_id": "revision", "run_id": "run"]],
                "actions": [["type": "attention", "kind": "question", "attention_id": "question", "task_id": "revision"]],
                "commands": [["kind": "open_pr", "task_id": id]],
            ] as [String: Any]
        }
        task.navigator = NavigatorProjection.parse(wire)
        return task
    }

    static func model(_ tasks: [WorkTask]) -> ChatViewModel {
        let defaults = UserDefaults(suiteName: "NavigatorTests.\(UUID().uuidString)")!
        let model = ChatViewModel(socketPath: "/tmp/navigator-\(UUID().uuidString).sock", defaults: defaults)
        model.selectedWorkProductID = product.id
        snapshot(model, tasks)
        return model
    }

    static func snapshot(_ model: ChatViewModel, _ tasks: [WorkTask]) {
        let projects = ["alpha", "beta"].map {
            WorkProject(id: $0, productID: product.id, name: $0, slug: $0, description: "", goal: "",
                        status: "active", priority: "medium", createdAt: "", updatedAt: "")
        }
        model.applyWorkTree(product: product, projects: projects, tasks: tasks, chores: [], taskRuntimes: [],
                            dependencies: [], ideas: [], attentionItems: [], attentionGroups: [], attentionMembers: [])
    }
}
