import Foundation

extension WorkCardLiveStatus {
    static func isPendingExecution(_ runtime: WorkTaskRuntime?) -> Bool {
        ["queued", "ready", "waiting_dependency"].contains(runtime?.executionStatus ?? "")
    }

    static func isQueued(task: WorkTask, runtime: WorkTaskRuntime?) -> Bool {
        if task.humanDriven && task.status == "active" { return false }
        if ["claimed", "running", "waiting_human"].contains(runtime?.executionStatus ?? "") {
            return false
        }
        let hasWait = !(runtime?.dispatchWaitReason?.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ?? true)
        return isPendingExecution(runtime) || (task.status == "todo" && task.autostart)
            || (["active", "in_review"].contains(task.status) && hasWait)
    }

    /// The grey status line of a queued card: why it is waiting, as the
    /// engine's wait classifier reports it. This view never decides a reason
    /// of its own; it formats the two deadlines the engine sends and renders
    /// the engine's phrase for everything else.
    static func queuedLabel(runtime: WorkTaskRuntime?, now: Date, lead: String = "Queued") -> String {
        let status = queuedStatus(runtime: runtime, now: now)
        return "\(status.lead == queuedLead ? lead : status.lead) — \(status.detail)"
    }

    /// The first runnable main-pool card. A hold it still carries (a full
    /// pool, say) stays visible after the marker; a card that is simply
    /// waiting for the dispatcher's next pass needs nothing after it.
    static func nextUpLabel(runtime: WorkTaskRuntime?, now: Date) -> String {
        let status = queuedStatus(runtime: runtime, now: now)
        if status.isAwaitingDispatch { return "Next up" }
        return status.lead == queuedLead
            ? "Next up — \(status.detail)"
            : "Next up — \(status.lead) — \(status.detail)"
    }

    private static let queuedLead = "Queued"

    /// Replaces "Queued" when the engine could not name the wait. The card
    /// says so outright instead of looking like an ordinary queue entry.
    private static let unexpectedLead = "Unexpected state"

    /// `TaskRuntime.dispatch_wait_reason` for a wait the engine could not
    /// name. Mirrors `boss_protocol::DISPATCH_WAIT_REASON_UNCLASSIFIED`.
    private static let unclassifiedReason = "unclassified"

    /// `TaskRuntime.dispatch_wait_reason` for a ready execution the
    /// dispatcher has not reached yet.
    private static let awaitingDispatchReason = "awaiting_dispatch"

    /// `TaskRuntime.dispatch_wait_reason` for a retry after a pre-start or
    /// transient failure. The engine names the attempt; the app only formats
    /// the deadline.
    private static let failureBackoffReason = "failure_backoff"

    /// `TaskRuntime.dispatch_wait_reason` for a spoke-lease park. Not a
    /// failure: the engine's phrase names the host wait.
    private static let hostLeaseParkReason = "host_lease_park"

    /// A queued card's status as a leading word plus the reason behind it.
    private struct QueuedStatus {
        let lead: String
        let detail: String
        var isAwaitingDispatch = false
    }

    private static func notBeforeDetail(_ date: Date) -> String {
        "not before \(date.formatted(date: .abbreviated, time: .shortened))"
    }

    private static func queuedStatus(runtime: WorkTaskRuntime?, now: Date) -> QueuedStatus {
        // The two deadlines are engine facts; only their formatting is local,
        // because a relative or local-time rendering cannot be done engine-side.
        // Why the deadline was stamped is the engine's wait code: this view
        // never classifies the source itself.
        if isPendingExecution(runtime) {
            if runtime?.dispatchWaitReason == failureBackoffReason,
               let label = runtime?.dispatchWaitLabel, !label.isEmpty,
               let raw = runtime?.dispatchNotBefore, let date = AutomationTime.parse(raw) {
                return QueuedStatus(lead: label, detail: notBeforeDetail(date))
            }
            if let raw = runtime?.dispatchNotBefore, let date = AutomationTime.parse(raw) {
                if runtime?.dispatchWaitReason == hostLeaseParkReason,
                   let label = runtime?.dispatchWaitLabel, !label.isEmpty {
                    return QueuedStatus(
                        lead: queuedLead,
                        detail: "\(label) — \(notBeforeDetail(date))")
                }
                return QueuedStatus(lead: queuedLead, detail: notBeforeDetail(date))
            }
        }
        // The engine sends a phrase for every wait. A queued card without one
        // has no explanation, and says that rather than guessing at a reason.
        guard let label = runtime?.dispatchWaitLabel, !label.isEmpty else {
            return QueuedStatus(lead: unexpectedLead, detail: "the engine sent no reason for this wait")
        }
        if runtime?.dispatchWaitReason == unclassifiedReason {
            return QueuedStatus(lead: unexpectedLead, detail: label)
        }
        let detail = runtime?.dispatchWaitSince.map { "\(label) (\(AutomationTime.relative($0, now: now)))" } ?? label
        return QueuedStatus(
            lead: queuedLead, detail: detail,
            isAwaitingDispatch: runtime?.dispatchWaitReason == awaitingDispatchReason)
    }
}
