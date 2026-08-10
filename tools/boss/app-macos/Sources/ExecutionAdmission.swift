import Foundation

/// Engine-owned admission decision for an explicit start. Mirrors
/// `boss_protocol::ExecutionAdmissionEvaluation` on the wire.
struct ExecutionAdmissionEvaluation: Equatable {
    var workItemID: String
    var wouldAdmit: Bool
    var pause: DispatchPauseSnapshot
    var pauseOverridable: Bool
    var blockers: [ExecutionAdmissionBlocker]
    var wouldOverridePause: Bool

    init(
        workItemID: String,
        wouldAdmit: Bool,
        pause: DispatchPauseSnapshot,
        pauseOverridable: Bool,
        blockers: [ExecutionAdmissionBlocker],
        wouldOverridePause: Bool
    ) {
        self.workItemID = workItemID
        self.wouldAdmit = wouldAdmit
        self.pause = pause
        self.pauseOverridable = pauseOverridable
        self.blockers = blockers
        self.wouldOverridePause = wouldOverridePause
    }

    init?(payload: [String: Any]) {
        guard let workItemID = payload["work_item_id"] as? String else { return nil }
        let wouldAdmit = payload["would_admit"] as? Bool ?? false
        let pausePayload = payload["pause"] as? [String: Any] ?? [:]
        let pause = DispatchPauseSnapshot(payload: pausePayload)
        let pauseOverridable = payload["pause_overridable"] as? Bool ?? false
        let wouldOverridePause = payload["would_override_pause"] as? Bool ?? false
        let rawBlockers = payload["blockers"] as? [[String: Any]] ?? []
        let blockers = rawBlockers.compactMap(ExecutionAdmissionBlocker.init(payload:))
        self.init(
            workItemID: workItemID,
            wouldAdmit: wouldAdmit,
            pause: pause,
            pauseOverridable: pauseOverridable,
            blockers: blockers,
            wouldOverridePause: wouldOverridePause
        )
    }
}

struct DispatchPauseSnapshot: Equatable {
    var paused: Bool
    var origin: String?
    var reason: String?
    var pausedSinceEpochS: UInt64?
    var reviewsExempt: Bool

    init(
        paused: Bool = false,
        origin: String? = nil,
        reason: String? = nil,
        pausedSinceEpochS: UInt64? = nil,
        reviewsExempt: Bool = false
    ) {
        self.paused = paused
        self.origin = origin
        self.reason = reason
        self.pausedSinceEpochS = pausedSinceEpochS
        self.reviewsExempt = reviewsExempt
    }

    init(payload: [String: Any]) {
        paused = payload["paused"] as? Bool ?? false
        origin = payload["origin"] as? String
        reason = payload["reason"] as? String
        if let n = payload["paused_since_epoch_s"] as? NSNumber {
            pausedSinceEpochS = n.uint64Value
        } else if let i = payload["paused_since_epoch_s"] as? UInt64 {
            pausedSinceEpochS = i
        } else {
            pausedSinceEpochS = nil
        }
        reviewsExempt = payload["reviews_exempt"] as? Bool ?? false
    }
}

struct ExecutionAdmissionBlocker: Equatable {
    var code: String
    var message: String
    var forceOverridable: Bool

    init(code: String, message: String, forceOverridable: Bool) {
        self.code = code
        self.message = message
        self.forceOverridable = forceOverridable
    }

    init?(payload: [String: Any]) {
        guard let code = payload["code"] as? String,
              let message = payload["message"] as? String
        else { return nil }
        self.init(
            code: code,
            message: message,
            forceOverridable: payload["force_overridable"] as? Bool ?? false
        )
    }
}

/// What the kanban should do after an admission evaluation for a drag-to-Doing.
enum ForceDispatchUIDecision: Equatable {
    /// No pause in play (or already clear) — proceed with ordinary start.
    case proceedNormally
    /// Operator pause is the only overridable condition — show confirm.
    case confirmPauseOverride(pauseReason: String, nonOverridableNotes: [String])
    /// Hard blockers — bounce with message; do not offer force confirm.
    case refuse(message: String)
}

/// Pure decision: maps an engine evaluation (evaluated with
/// `bypass_dispatch_pause = false` for the preview) into UI action.
///
/// The engine owns eligibility; this only chooses confirm vs proceed vs refuse.
func forceDispatchUIDecision(from evaluation: ExecutionAdmissionEvaluation) -> ForceDispatchUIDecision {
    let hard = evaluation.blockers.filter { !$0.forceOverridable }
    let overridable = evaluation.blockers.filter(\.forceOverridable)

    if !hard.isEmpty {
        let message = hard.map(\.message).joined(separator: "; ")
        return .refuse(message: message)
    }

    if evaluation.pause.paused, evaluation.pauseOverridable, !overridable.isEmpty {
        let reason = evaluation.pause.reason ?? "dispatch is paused"
        // Non-overridable notes for the dialog body when present (none here
        // if hard is empty). Kept for the contract that the alert lists
        // blockers force will not clear.
        return .confirmPauseOverride(pauseReason: reason, nonOverridableNotes: [])
    }

    if evaluation.wouldAdmit {
        return .proceedNormally
    }

    // Fallback: something refused without a hard blocker list.
    let message = evaluation.blockers.map(\.message).joined(separator: "; ")
    if message.isEmpty {
        return .refuse(message: "cannot start \(evaluation.workItemID): admission refused")
    }
    return .refuse(message: message)
}

/// Pending drag-to-Doing waiting on admission evaluation or force confirm.
struct PendingDoingDispatch: Equatable {
    var taskID: String
    var originColumn: WorkBoardColumnKey
    /// Pause generation observed at evaluation time; echoed on confirm.
    var observedPauseGeneration: UInt64?
    var pauseReason: String?
    var nonOverridableNotes: [String]
    /// When true, the confirmation dialog is showing.
    var awaitingConfirm: Bool
}
