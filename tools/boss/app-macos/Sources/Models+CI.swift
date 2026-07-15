import Foundation

// ===========================================================================
// CI and merge-conflict remediation models. Swift mirrors of the engine's
// in-PR conflict-resolution and CI-failure remediation rows, plus the badge the
// kanban card renders for a failing check. Split out of Models.swift to keep
// that file under the repo's file-size check.
// ===========================================================================

/// Swift mirror of `boss_protocol::ConflictResolution`. One engine
/// attempt to clear a merge conflict on an `in_review` PR. Powers the
/// Engine tab's attempt-row list (design Phase 5 #14) and the
/// "🔧 conflict cleared" PR-card badge (#15).
struct WorkConflictResolution: Identifiable, Hashable {
    let id: String
    var productID: String
    var workItemID: String
    var prURL: String
    var prNumber: Int
    var headBranch: String
    var baseBranch: String
    var baseSHAAtTrigger: String?
    var headSHABefore: String?
    var headSHAAfter: String?
    /// `pending` / `running` / `succeeded` / `failed` / `abandoned` /
    /// `superseded`. See the wire-type docs in `boss_protocol::types`
    /// for the lifecycle.
    var status: String
    var failureReason: String?
    var cubeLeaseID: String?
    var cubeWorkspaceID: String?
    var workerID: String?
    /// Raw JSON blob the worker prompt was built from. Carried verbatim
    /// here so the detail panel can surface it without a separate fetch.
    var conflictDiagnosis: String?
    var createdAt: String
    var startedAt: String?
    var finishedAt: String?
    /// Soft FK to the `tasks.id` of the revision task spawned by this attempt.
    /// `nil` for pre-unification rows and attempts retired before a revision was created.
    var revisionTaskId: String? = nil
}

/// PR-card chip state for the CI auto-fix flow (design Q11 / Phase
/// 11 #37). Either "engine is still trying" (with a numeric
/// `used/budget`) or "engine has given up." The exhausted variant
/// stays visible until the user kicks `boss engine ci retry`; the
/// in-flight variant clears when the next probe observes CI back at
/// `Clean`.
struct CiFailureBadge: Equatable, Hashable {
    enum State: String, Hashable {
        /// `blocked: ci_failure` — engine still trying.
        case inFlight = "in_flight"
        /// `blocked: ci_failure_exhausted` — engine has given up.
        case exhausted
    }
    var state: State
    var attemptsUsed: Int
    var budget: Int
}

/// Swift mirror of `boss_protocol::CiRemediation`. One engine attempt
/// to clear a CI failure on an `in_review` PR. Powers the Engine
/// tab's CI rows (design Phase 11 #37) and the per-PR badges (Q11).
struct WorkCiRemediation: Identifiable, Hashable {
    let id: String
    var productID: String
    var workItemID: String
    var prURL: String
    var prNumber: Int
    var headBranch: String
    var headSHAAtTrigger: String
    var headSHAAfter: String?
    /// `"fix"` or `"retrigger"` — the engine's pre-spawn triage call.
    var attemptKind: String
    /// `1` for fix-kind attempts that actually pushed; `0` for
    /// retriggers and triage-bailouts.
    var consumesBudget: Int
    /// JSON-encoded list of failing-check snapshots captured at trigger
    /// time. Stored as a verbatim string; consumers parse on demand.
    var failedChecks: String
    /// Worker-assigned classification of the failure after reading the
    /// log — one of `tractable` / `flaky_or_infra` / `unfixable`. `nil`
    /// until the worker fills it.
    var triageClass: String?
    var logExcerpt: String?
    /// `pending` / `running` / `succeeded` / `failed` / `abandoned` /
    /// `superseded`. See the wire-type docs in `boss_protocol::types`.
    var status: String
    var failureReason: String?
    var cubeLeaseID: String?
    var cubeWorkspaceID: String?
    var workerID: String?
    var createdAt: String
    var startedAt: String?
    var finishedAt: String?
    /// Soft FK to the `tasks.id` of the revision task spawned by this attempt.
    /// `nil` for pre-unification rows, retrigger attempts, and attempts retired
    /// before a revision was created.
    var revisionTaskId: String? = nil
}
