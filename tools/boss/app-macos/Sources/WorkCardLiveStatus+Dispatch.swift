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
    /// Every branch renders a code the engine stated: a hold reason for a
    /// dispatch-pending row with nothing queued, a wait reason for a pending
    /// execution. The engine supplies one in both cases and logs the rare
    /// shape where it cannot; when a payload still carries none, the line says
    /// only what the app knows, that the row is queued.
    static func queuedLabel(runtime: WorkTaskRuntime?, now: Date) -> String {
        guard isPendingExecution(runtime) else {
            if let hold = runtime?.dispatchHoldReason, !hold.isEmpty {
                return dispatchHoldReasonLabel(hold)
            }
            return Self.queuedWithoutReasonLabel
        }
        if let blocker = runtime?.dispatchWaitBlocker,
           ["waiting_dependency", "ordinal_chain"].contains(runtime?.dispatchWaitReason ?? "") {
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
        guard let reason = runtime?.dispatchWaitReason, !reason.isEmpty else {
            return Self.queuedWithoutReasonLabel
        }
        let label = dispatchWaitReasonLabel(reason)
        if let since = runtime?.dispatchWaitSince {
            return "\(label) (\(AutomationTime.relative(since, now: now)))"
        }
        return label
    }

    /// Shown only when the runtime carries no engine reason at all.
    static let queuedWithoutReasonLabel = "Queued"

    /// Mirrors the `DISPATCH_HOLD_*` vocabulary in
    /// `engine/core/src/work/exec_status_helpers.rs`.
    static func dispatchHoldReasonLabel(_ reason: String) -> String {
        switch reason {
        case "human_driven": return "Held — human-driven; no worker will run"
        case "deferred": return "Held — deferred until approved"
        case "pr_awaits_review": return "Held — already has a PR; file a revision to change it"
        case "deliberately_parked": return "Held — parked after its last run; start it to resume"
        case "repo_unresolved": return "Held — no repository resolved for this item"
        case "awaiting_reconcile": return "Queued — waiting for the engine to reconcile"
        case "no_execution_kind": return "Held — this kind of item never runs on its own"
        case "no_project": return "Held — not attached to a project"
        case "revision_parent_unresolved": return "Held — the revised item cannot be found"
        case "revision_awaiting_parent_pr": return "Held — waiting for the revised item to open a PR"
        case "blocked_reason": return "Held — blocked until the worker failure or question is resolved"
        case "shadowed_pending_execution": return "Held — an older queued run is still pending"
        default: return "Held — \(reason)"
        }
    }

    private static func dispatchWaitReasonLabel(_ reason: String) -> String {
        switch reason {
        case "waiting_dependency": return "Queued — waiting for a dependency"
        case "ordinal_chain": return "Queued — waiting for the earlier project task"
        case "awaiting_scheduler": return "Queued — waiting for the scheduler"
        case "dispatch_paused": return "Queued — dispatch paused"
        case "automation_paused": return "Queued — automation paused"
        case "pool_exhausted": return "Queued — worker pool full"
        case "not_before": return "Queued — held until a scheduled time"
        default: return "Queued — \(reason)"
        }
    }
}
