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
    ///
    /// Every branch states something the engine actually reported. The two
    /// fallbacks at the bottom are deliberately factual rather than a guess:
    /// a dispatch-pending row the engine has no pending execution *and* no
    /// hold reason for is a state the engine logs and converges on its next
    /// reconcile pass (`converge_stranded_autostart_row`), so the card says
    /// that nothing is queued instead of inventing a reason.
    static func queuedLabel(runtime: WorkTaskRuntime?, now: Date) -> String {
        guard isPendingExecution(runtime) else {
            if let hold = runtime?.dispatchHoldReason, !hold.isEmpty {
                return dispatchHoldReasonLabel(hold)
            }
            return Self.nothingQueuedLabel
        }
        if let blocker = runtime?.dispatchWaitBlocker,
           runtime?.dispatchWaitReason == "waiting_dependency" {
            return "Queued — waiting for \(blocker.label)"
        }
        if let raw = runtime?.dispatchRetryAt, let date = AutomationTime.parse(raw), date > now {
            return "Retrying dispatch — next attempt \(AutomationTime.relative(raw, now: now))"
        }
        if let raw = runtime?.dispatchNotBefore {
            if let date = AutomationTime.parse(raw) {
                return "Queued — not before \(date.formatted(date: .abbreviated, time: .shortened))"
            }
            return "Queued — held until a scheduled time"
        }
        guard let reason = runtime?.dispatchWaitReason, !reason.isEmpty, reason != "unknown" else {
            return Self.pendingWithoutReasonLabel
        }
        let label = dispatchWaitReasonLabel(reason)
        if let since = runtime?.dispatchWaitSince {
            return "\(label) (\(AutomationTime.relative(since, now: now)))"
        }
        return label
    }

    /// A `todo` + `autostart` row whose runtime carries no pending execution
    /// and no engine hold reason. Factual, not a guess: nothing is queued.
    static let nothingQueuedLabel = "Queued — nothing queued yet; waiting for the engine to reconcile"

    /// A pending execution the scheduler has not yet stamped a wait reason
    /// on (the window between mint and the first scheduler pass).
    static let pendingWithoutReasonLabel = "Queued — waiting for the scheduler"

    /// Mirrors the `DISPATCH_HOLD_*` vocabulary in
    /// `engine/core/src/work/dispatch_helpers.rs`.
    static func dispatchHoldReasonLabel(_ reason: String) -> String {
        switch reason {
        case "human_driven": return "Held — human-driven; no worker will run"
        case "deferred": return "Held — deferred until approved"
        case "pr_awaits_review": return "Held — already has a PR; file a revision to change it"
        case "deliberately_parked": return "Held — parked after its last run; start it to resume"
        case "repo_unresolved": return "Held — no repository resolved for this item"
        case "awaiting_reconcile": return "Queued — waiting for the engine to reconcile"
        default: return "Held — \(reason)"
        }
    }

    private static func dispatchWaitReasonLabel(_ reason: String) -> String {
        switch reason {
        case "waiting_dependency": return "Queued — waiting for a dependency"
        case "dispatch_paused": return "Queued — dispatch paused"
        case "automation_paused": return "Queued — automation paused"
        case "pool_exhausted": return "Queued — worker pool full"
        case "not_before": return "Queued — held until a scheduled time"
        default: return "Queued — \(reason)"
        }
    }
}
