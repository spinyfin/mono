import Foundation

extension WorkCardLiveStatus {
    static func isPendingExecution(_ runtime: WorkTaskRuntime?) -> Bool {
        ["queued", "ready", "waiting_dependency"].contains(runtime?.executionStatus ?? "")
    }

    static func isQueued(task: WorkTask, runtime: WorkTaskRuntime?) -> Bool {
        if task.humanDriven && task.status == "active" { return false }
        if ["claimed", "running", "waiting_human", "waiting_review", "waiting_merge"].contains(runtime?.executionStatus ?? "") {
            return false
        }
        return isPendingExecution(runtime) || (task.status == "todo" && task.autostart)
    }

    /// The existing grey status line, fed only by the engine's current runtime.
    static func queuedLabel(runtime: WorkTaskRuntime?, now: Date) -> String {
        let status = queuedStatus(runtime: runtime, now: now)
        guard let detail = status.detail else { return "\(status.lead) — reason unknown" }
        return "\(status.lead) — \(detail)"
    }

    /// The first runnable main-pool card. A wait reason it still carries
    /// (a full pool, say) stays visible after the marker.
    static func nextUpLabel(runtime: WorkTaskRuntime?, now: Date) -> String {
        let status = queuedStatus(runtime: runtime, now: now)
        guard let detail = status.detail else { return "Next up" }
        return status.lead == queuedLead ? "Next up — \(detail)" : "Next up — \(status.lead) — \(detail)"
    }

    private static let queuedLead = "Queued"

    /// A queued card's status as a leading word plus the reason behind it.
    /// `detail` is nil when the engine gave no reason.
    private struct QueuedStatus {
        let lead: String
        let detail: String?
    }

    private static func queuedStatus(runtime: WorkTaskRuntime?, now: Date) -> QueuedStatus {
        guard isPendingExecution(runtime) else { return QueuedStatus(lead: queuedLead, detail: nil) }
        if let blocker = runtime?.dispatchWaitBlocker,
           runtime?.dispatchWaitReason == "waiting_dependency" {
            return QueuedStatus(lead: queuedLead, detail: "waiting for \(blocker.label)")
        }
        if let raw = runtime?.dispatchRetryAt, let date = AutomationTime.parse(raw), date > now {
            return QueuedStatus(
                lead: "Retrying dispatch",
                detail: "next attempt \(AutomationTime.relative(raw, now: now))")
        }
        if let raw = runtime?.dispatchNotBefore, let date = AutomationTime.parse(raw) {
            return QueuedStatus(
                lead: queuedLead,
                detail: "not before \(date.formatted(date: .abbreviated, time: .shortened))")
        }
        guard let reason = runtime?.dispatchWaitReason, !reason.isEmpty else {
            return QueuedStatus(lead: queuedLead, detail: nil)
        }
        let label = dispatchWaitReasonDetail(reason)
        if let since = runtime?.dispatchWaitSince {
            return QueuedStatus(
                lead: queuedLead, detail: "\(label ?? "reason unknown") (\(AutomationTime.relative(since, now: now)))")
        }
        return QueuedStatus(lead: queuedLead, detail: label)
    }

    /// nil when the reason carries no information beyond "unknown".
    private static func dispatchWaitReasonDetail(_ reason: String) -> String? {
        switch reason {
        case "waiting_dependency": return "waiting for a dependency"
        case "dispatch_paused": return "dispatch paused"
        case "automation_paused": return "automation paused"
        case "pool_exhausted": return "worker pool full"
        case "unknown", "not_before": return nil
        default: return reason
        }
    }
}
