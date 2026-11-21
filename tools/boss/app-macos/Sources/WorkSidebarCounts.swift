import Foundation

struct WorkSidebarCounts: Equatable {
    struct Counts: Equatable {
        var parked = 0
        var blocked = 0
        var unblocked = 0
    }
    var projects: [String: Counts] = [:]
    var products: [String: Counts] = [:]
}

extension ChatViewModel {
    /// One pass per work-data generation, shared by every sidebar row. Runtime
    /// and live-worker updates do not invalidate this index.
    var sidebarCounts: WorkSidebarCounts {
        if let cachedSidebarCounts { return cachedSidebarCounts }
        let parkedProjects = parkedProjectsByID
        func count(_ tasks: [WorkTask]) -> WorkSidebarCounts.Counts {
            var result = WorkSidebarCounts.Counts()
            for task in tasks {
                if isParked(task, parkedProjects: parkedProjects) {
                    if task.kind != "revision" { result.parked += 1 }
                } else if task.status == "todo" {
                    result.unblocked += 1
                } else if task.status == "blocked", task.blockedReason == "dependency" {
                    result.blocked += 1
                }
            }
            return result
        }
        var result = WorkSidebarCounts()
        for (id, tasks) in tasksByProjectID { result.projects[id] = count(tasks) }
        for id in Set(choresByProductID.keys).union(productLevelTasksByProductID.keys) {
            result.products[id] = count((choresByProductID[id] ?? []) + (productLevelTasksByProductID[id] ?? []))
        }
        cachedSidebarCounts = result
        return result
    }
}
