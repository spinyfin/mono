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
        guard isPendingExecution(runtime) else { return "Queued — reason unknown" }
        if let blocker = runtime?.dispatchWaitBlocker,
           runtime?.dispatchWaitReason == "waiting_dependency" {
            return "Queued — waiting for \(blocker.label)"
        }
        if let raw = runtime?.dispatchRetryAt, let date = AutomationTime.parse(raw), date > now {
            return "Retrying dispatch — next attempt \(AutomationTime.relative(raw, now: now))"
        }
        if let raw = runtime?.dispatchNotBefore, let date = AutomationTime.parse(raw) {
            return "Queued — not before \(date.formatted(date: .abbreviated, time: .shortened))"
        }
        guard let reason = runtime?.dispatchWaitReason, !reason.isEmpty else {
            return "Queued — reason unknown"
        }
        let label = dispatchWaitReasonLabel(reason)
        if let since = runtime?.dispatchWaitSince {
            return "\(label) (\(AutomationTime.relative(since, now: now)))"
        }
        return label
    }

    private static func dispatchWaitReasonLabel(_ reason: String) -> String {
        switch reason {
        case "waiting_dependency": return "Queued — waiting for a dependency"
        case "dispatch_paused": return "Queued — dispatch paused"
        case "automation_paused": return "Queued — automation paused"
        case "pool_exhausted": return "Queued — worker pool full"
        case "unknown", "not_before": return "Queued — reason unknown"
        default: return "Queued — \(reason)"
        }
    }
}
