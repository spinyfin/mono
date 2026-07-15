import Foundation

struct WorkTask: Identifiable, Hashable {
    let id: String
    let productID: String
    let projectID: String?
    let kind: String
    var name: String
    var description: String
    var status: String
    var priority: String
    var ordinal: Int?
    var prURL: String?
    var deletedAt: String?
    var createdAt: String
    var updatedAt: String
    /// `'human'` (default) when the most recent status change came
    /// from a CLI / app caller; `'engine'` when the engine flipped
    /// the status itself. The kanban renders the auto-block chain
    /// badge only when this is `'engine'` so manual blocks stay
    /// visually quiet (they already get the lane).
    var lastStatusActor: String = "human"
    /// The surface that filed this row — `cli`, `bossctl`, `mac_app`,
    /// `engine_auto`, or `unknown`. Pre-column rows arrive as
    /// `unknown` from the engine's migration default.
    var createdVia: String = "unknown"
    /// Per-work-item repo override. `nil` → inherit from the parent
    /// product's `repoRemoteURL`. Pre-column rows decode as `nil`
    /// because serde skips the field when it's `None` on the wire
    /// (see `Task.repo_remote_url` in `boss_protocol::types`).
    var repoRemoteURL: String? = nil
    /// When `status == "blocked"`, the engine's discriminator for
    /// *why* — `"dependency"`, `"merge_conflict"`, `"review_feedback"`,
    /// `"ci_failure"`, `"ci_failure_exhausted"`. `nil` for non-blocked
    /// rows and for legacy blocked rows without a tracked reason.
    /// Phase 1 of the merge-conflict design only populates this; the
    /// kanban renders it as no-op decoration until a later phase wires
    /// the badge labels through.
    var blockedReason: String? = nil
    /// Soft FK to the engine attempt currently trying to clear the
    /// block (a `conflict_resolutions.id` for `merge_conflict`).
    /// Discriminated by `blockedReason`; `nil` for blocks without an
    /// engine-managed attempt.
    var blockedAttemptID: String? = nil
    /// Per-product short id. `nil` only on rows predating the migration
    /// (the engine backfills these at startup, so `nil` is transient).
    /// Mirrors `Task.short_id` on the wire.
    var shortID: Int? = nil
    /// When `true` the engine will dispatch a worker the moment a slot
    /// is free. Rows with `status=todo AND autostart=true` that have no
    /// active execution are "dispatch-pending" — the engine has committed
    /// to running them but the pool is full. The kanban routes these to
    /// the Doing column with a distinct waiting indicator rather than
    /// leaving them in Backlog. Defaults to `false` when absent from the
    /// wire so legacy rows without the field stay in Backlog (unchanged).
    var autostart: Bool = false
    /// Aggregate required-CI state at last merge-poller probe. One of:
    /// `"in_progress"`, `"success"`, `"fail"`, `"unknown"`. `nil` until the
    /// first probe completes. Only rendered when `status == "in_review"` and
    /// `prURL` is non-nil; hidden otherwise.
    var ciRequiredState: String? = nil
    /// JSON-encoded list of failing check objects for the CI tooltip.
    /// Each object has `"name"` and `"conclusion"` keys. `nil` unless
    /// `ciRequiredState == "fail"`.
    var ciRequiredDetail: String? = nil
    /// Review-gating state at last merge-poller probe. One of:
    /// `"required"`, `"approved"`, `"changes_requested"`, `"unknown"`. `nil`
    /// until the first probe completes. Only rendered when `status == "in_review"`
    /// and `prURL` is non-nil; hidden otherwise.
    var reviewRequiredState: String? = nil
    /// JSON-encoded list of reviewer login strings for the review tooltip.
    /// `nil` unless `reviewRequiredState` is `"approved"` or
    /// `"changes_requested"`.
    var reviewRequiredDetail: String? = nil
    /// RFC 3339 timestamp of the most recent successful poll that wrote the
    /// PR state fields above. `nil` until the first probe completes.
    var prStatePolledAt: String? = nil
    /// Merge-queue / auto-merge state at last poll. `"queued"` when the PR
    /// is currently in GitHub's merge queue; `"auto_merge_enabled"` when
    /// GitHub auto-merge is armed (Merge When Ready is still waiting on
    /// checks, or the repo has no merge queue) but the PR hasn't reached a
    /// queue; `nil` when neither. Either non-nil value moves the task into
    /// the kanban's "Merging" section — see `boardColumn` /
    /// `isInMergingSection`.
    var mergeQueueState: String? = nil
    /// JSON-encoded merge-queue/auto-merge sub-state: `{"position",
    /// "state", "enqueued_at", "section_order"}`. `nil` unless
    /// `mergeQueueState` is non-nil. Parsed by `MergeQueueDetail.parse(json:)`
    /// for the Merging section's compact badge and sort order.
    var mergeQueueDetail: String? = nil
    /// Stable upstream pointer to the external tracker issue linked to this
    /// work item. `nil` when no binding exists. Mirrors `Task.external_ref`.
    var externalRef: WorkItemExternalRef? = nil
    /// Soft FK to the parent task for `kind == "revision"` rows. `nil`
    /// for non-revision rows. Mirrors `Task.parent_task_id` on the wire.
    var parentTaskId: String? = nil
    /// Engine-computed R-number for revision tasks (1-based, chain-root-scoped).
    /// `nil` for non-revision rows. Mirrors the derived `revision_seq` field.
    var revisionSeq: Int? = nil
    /// Denormalized parent chain-root PR URL for fast revision card rendering.
    /// `nil` for non-revision rows. Mirrors `revision_parent_pr_url` on the wire.
    var revisionParentPrUrl: String? = nil
    /// `true` when any descendant revision task in the chain has status
    /// `todo` or `active`. Indicates new commits are still incoming and the
    /// PR should not be merged yet. Only meaningful on chain-root tasks that
    /// carry a `prURL`. Mirrors `has_in_progress_revision` on the wire.
    var hasInProgressRevision: Bool = false
    /// Size estimate for this work item. One of `trivial`, `small`, `medium`,
    /// `large`, `max`. `nil` when the row has no effort estimate (pre-column
    /// rows or items where the engine emitted `null`). Mirrors
    /// `Task.effort_level` on the wire; absent means unset, NOT medium.
    var effortLevel: String? = nil
    /// Non-null when this task was produced by an automation triage run.
    /// Mirrors `Task.source_automation_id` on the wire. Used to display
    /// the automation-provenance badge on the card and to route execution
    /// to the automations worker pool. Cards with this set DO appear on
    /// the kanban — the purple wand icon distinguishes them from human-filed
    /// work so the operator can still review and merge their PRs.
    var sourceAutomationId: String? = nil
    /// `true` while an independent `pr_review` reviewer execution is running
    /// for this task. The task is held in the Doing column until the reviewer
    /// finalises (or a timeout forces the advance). Surfaces as a
    /// "Reviewing (AI)" badge on the card so the user can see the hold is
    /// intentional. Mirrors `Task.ai_reviewing` on the wire; `false` when
    /// absent (older engines / tasks not undergoing an AI review pass).
    var aiReviewing: Bool = false
    /// Resolved doc-link state for a **project-less** docs-backed item —
    /// chiefly `kind == "investigation"`. Mirrors `Task.doc_link_state`
    /// on the wire: the engine resolves the task's own `doc_*` columns
    /// into the same `ProjectDesignDocState` the kanban already renders
    /// for design cards (whose state comes from the parent project). The
    /// card feeds this into the doc-link affordance so investigations get
    /// the Review-lane icon — parity with design cards. `nil` when the
    /// item has no per-task pointer (hides the affordance).
    var docLinkState: ProjectDesignDocState? = nil

    /// Short id of the reviewed task that produced this follow-up.
    /// `nil` for every task whose `kind` is not `"followup"`.
    /// Mirrors `Task.origin_task_short_id` on the wire.
    var originTaskShortId: Int? = nil
    /// GitHub PR number that was under review when the findings were filed.
    /// `nil` for every task whose `kind` is not `"followup"`.
    /// Mirrors `Task.origin_pr_number` on the wire.
    var originPrNumber: Int? = nil

    /// Unix epoch seconds (decimal string) at which this task last
    /// transitioned into a terminal status (`done`, `archived`, or
    /// `cancelled`). `nil` for non-terminal rows and for terminal rows
    /// pre-dating the engine migration (those get `created_at` as a
    /// conservative backfill). The Done-lane bucketing groups by this
    /// field so a bulk mutation that re-stamps `updated_at` on many done
    /// rows does not mis-count them as completed today.
    /// Mirrors `Task.completed_at` on the wire.
    var completedAt: String? = nil

    /// Machine discriminator for a dispatch failure the engine gave up
    /// retrying (e.g. `"cube_workspace_lease_failed"`) — set only when a
    /// pre-start dispatch attempt (cube repo ensure, workspace lease,
    /// change create, run start, …) failed non-transiently and the engine
    /// bounced this row back to Backlog with `autostart` cleared. `nil`
    /// for every task with no unresolved dispatch failure — the
    /// overwhelming majority. Distinguishes a card that is genuinely
    /// broken from one that is merely `status=="todo" && autostart` and
    /// waiting on a free worker slot (which never sets this field).
    /// Mirrors `Task.dispatch_failed_reason` on the wire.
    var dispatchFailedReason: String? = nil
    /// Human-readable error text for `dispatchFailedReason` (e.g. the
    /// underlying cube lease error message). Rendered directly on the
    /// kanban card so the operator can see why without digging into
    /// dispatch logs. Mirrors `Task.dispatch_failed_error` on the wire.
    var dispatchFailedError: String? = nil
    /// RFC 3339 timestamp of the dispatch failure recorded in
    /// `dispatchFailedReason`. `nil` whenever that field is `nil`.
    /// Mirrors `Task.dispatch_failed_at` on the wire.
    var dispatchFailedAt: String? = nil

    var isChore: Bool {
        kind == "chore" || kind == "followup"
    }

    /// Human-readable label for the work item's kind, shown in the card
    /// detail popover. Falls back to a title-cased rendering of the raw
    /// kind string so a kind the app doesn't explicitly know about still
    /// reads sensibly instead of being mislabeled "Task" (issue #886).
    var kindLabel: String {
        switch kind {
        case "chore": return "Chore"
        case "followup": return "Followup"
        case "investigation": return "Investigation"
        case "revision": return "Revision"
        case "design": return "Design"
        case "project_task", "task": return "Task"
        default:
            return kind
                .split(separator: "_")
                .map { $0.prefix(1).uppercased() + $0.dropFirst() }
                .joined(separator: " ")
        }
    }
}

/// Derivation helpers for the kanban card's "blocked" badge — the
/// orange chip in the card footer that reads e.g. `Merge Conflict` /
/// `Blocked`. Centralised so the View and unit tests share one rule.
///
/// **Rule:** the badge MUST only render when `task.status == "blocked"`.
/// Per the engine spec (`Task::blocked_reason` doc), the scalar
/// `blocked_reason` field is `NULL` on rows whose `status` is not
/// `'blocked'`. A non-blocked row carrying a non-nil `blockedReason`
/// is, by definition, locally stale (the engine has cleared the
/// scalar but the macOS reducer hasn't reconverged yet — typically
/// because an `events.sock` envelope was dropped or the work-tree
/// refresh hasn't landed). The badge must NOT mirror that stale
/// signal: the lane is the source of truth, and the lane comes from
/// `status`. So the badge derivation gates on `status` rather than
/// trusting `blockedReason` in isolation. See the chore card
/// `Kanban chore card shows stale "Merge Conflict" badge` regression.
enum WorkBlockedBadge {
    /// Footer chip text for `task`, or `nil` when no chip should
    /// appear. Callers pass the chip text straight into `WorkStatusBadge`;
    /// the `nil` path collapses the chip entirely.
    static func badgeText(for task: WorkTask) -> String? {
        guard task.status == "blocked" else { return nil }
        guard let reason = task.blockedReason else { return "Blocked" }
        return label(forReason: reason)
    }

    /// Human-readable label for a raw `blocked_reason` string. Used by
    /// [[badgeText(for:)]] and by any future surface (e.g. detail
    /// metadata row) that needs the same vocabulary. Falls back to a
    /// title-cased version of the raw value so unknown / future reason
    /// codes degrade gracefully rather than rendering as the empty
    /// string.
    static func label(forReason reason: String) -> String {
        switch reason {
        case "dependency": return "Dependency"
        case "merge_conflict": return "Merge Conflict"
        case "ci_failure": return "CI Failure"
        case "ci_failure_exhausted": return "CI Failed"
        case "review_feedback": return "Review"
        default: return reason.replacingOccurrences(of: "_", with: " ").capitalized
        }
    }

    /// True when the "conflict cleared" badge may show: `cleared` is set
    /// AND the task is not simultaneously displaying an active "Merge
    /// Conflict" blocked badge. The two badges are mutually exclusive states
    /// (T795 / T626 analogue): if engine state is contradictory or empty,
    /// the card shows neither rather than both.
    static func conflictClearedVisible(forTask task: WorkTask, cleared: Bool, isResolvingConflicts: Bool) -> Bool {
        guard cleared else { return false }
        let activeConflict = !isResolvingConflicts
            && task.status == "blocked"
            && task.blockedReason == "merge_conflict"
        return !activeConflict
    }
}

/// Canonical priority vocabulary shared by tasks, chores, and
/// projects. Lives in one place so kanban chips, edit pickers, and
/// any future filter UI all speak the same dialect.
enum WorkPriority: String, CaseIterable, Identifiable {
    case low
    case medium
    case high

    var id: String { rawValue }

    /// Tolerant decoder. Pre-priority rows arrive without the field
    /// (older engines, unmigrated DBs, hand-built JSON in tests); we
    /// fall back to `.medium` to match the schema default rather than
    /// surfacing `nil` and forcing every call site to special-case it.
    static func parse(_ raw: String?) -> WorkPriority {
        guard let raw, let value = WorkPriority(rawValue: raw.lowercased()) else {
            return .medium
        }
        return value
    }

    var label: String {
        switch self {
        case .low: return "Low"
        case .medium: return "Medium"
        case .high: return "High"
        }
    }
}

enum WorkNodeID: Hashable {
    case product(String)
    case project(String)
    case task(String)
    case chore(String)
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

extension WorkTask {
    /// Canonical mapping from engine status → kanban column.
    ///
    /// Tasks/chores carry one of `todo`, `active`, `blocked`,
    /// `in_review`, `done` (plus `archived`, which is filtered out by
    /// `deleted_at`). `blocked` splits by reason:
    ///   • Review-phase reasons (`merge_conflict`, `ci_failure`,
    ///     `ci_failure_exhausted`, `review_feedback`) → Review. The item
    ///     has an open PR; the block is transient and in-flight. The card
    ///     shows the reason badge so the state is legible.
    ///   • Everything else (dependency, nil, unknown) → Backlog: the item
    ///     can't start yet, so from the user's perspective it sits with
    ///     the not-yet-active pile.
    ///
    /// Dispatch-pending rows (`status=todo AND autostart=true`) route to
    /// Doing rather than Backlog. From the user's perspective these rows
    /// are already committed — the engine will start them as soon as
    /// scheduling reaches them — so they belong visually with active work,
    /// not with unscheduled backlog items. The card renders a distinct
    /// hourglass indicator to distinguish "queued" from "working"; see
    /// `WorkBoardCardItem.liveStatusForCard` for how the subtitle picks
    /// apart "not yet scheduled" from "genuinely waiting on pool capacity".
    ///
    /// A row the engine gave up starting (`dispatchFailedReason` set) is
    /// NOT dispatch-pending: the engine clears `autostart` in the same
    /// transaction that stamps the failure (see
    /// `WorkDb::bounce_dispatch_failed_to_backlog`), so it falls straight
    /// through to Backlog below instead of rendering as a phantom
    /// "waiting for a slot" card indistinguishable from genuine capacity
    /// wait. The card still surfaces the failure via the error banner —
    /// see `WorkBoardCardView`.
    ///
    /// An `in_review` task that is either in GitHub's merge queue or has
    /// Merge When Ready armed (`isInMergingSection`) routes to Done instead
    /// of Review — it renders in the Done column's collapsible "Merging"
    /// section, above "Today". This is a pure re-derivation of existing
    /// engine state (`mergeQueueState`), not a new transition: the task's
    /// `status` never changes, so if the PR later drops out of the queue
    /// without merging, the next poll clears `mergeQueueState` and this
    /// same switch naturally routes the card back to Review.
    var boardColumn: WorkBoardColumnKey {
        switch status {
        case "active":
            return .doing
        case "in_review" where isInMergingSection:
            return .done
        case "in_review":
            return .review
        case "done":
            return .done
        case "todo" where autostart:
            return .doing
        case "blocked" where isReviewPhaseBlocked:
            return .review
        default:
            return .backlog
        }
    }

    /// `true` when this task is blocked for a review-phase reason —
    /// it has an open PR and the block is transient (conflict resolution
    /// or CI fix in progress). These tasks render in Review, not Backlog.
    var isReviewPhaseBlocked: Bool {
        switch blockedReason {
        case "merge_conflict", "ci_failure", "ci_failure_exhausted", "review_feedback":
            return true
        default:
            return false
        }
    }

    /// `true` when the task's PR is either in GitHub's merge queue or has
    /// Merge When Ready armed (`mergeQueueState == "queued"` or
    /// `"auto_merge_enabled"`). Drives both `boardColumn` (routes the card
    /// into Done's "Merging" section) and the compact queue badge.
    ///
    /// Gated on `status == "in_review"`: a `done` task can carry a stale
    /// `mergeQueueState` if the merge transition raced the poller's next
    /// dequeue write (the engine also clears it server-side in
    /// `mark_chore_pr_merged`, but this guard keeps the client correct even
    /// against a row that predates that fix or a late-arriving write). Without
    /// the guard, `computeWorkSections` would bucket a just-merged task into
    /// the "Merging" section forever instead of the normal Done/recency
    /// buckets.
    var isInMergingSection: Bool {
        status == "in_review" && mergeQueueState != nil
    }

}

struct WorkTaskRuntime: Hashable {
    let workItemID: String
    let executionStatus: String?
    let runStatus: String?
    /// Active or most recent execution id for this work item. Used to
    /// join task → LiveWorkerState (engine registers LiveWorkerState
    /// with `run_id == execution_id`).
    let executionID: String?
    /// Unix epoch seconds (decimal string); set only during the engine's
    /// backoff after a pre-spawn dispatch failure, `nil` for a genuine
    /// capacity wait. Mirrors `TaskRuntime.dispatch_retry_at` on the wire.
    let dispatchRetryAt: String?
    /// The dispatcher's current defer reason for this `ready` execution
    /// (e.g. `chain_serialized`, `pool_exhausted`), `nil` when it isn't
    /// currently deferred. Mirrors `TaskRuntime.dispatch_wait_reason`.
    let dispatchWaitReason: String?
    /// Unix epoch seconds (decimal string) since `dispatchWaitReason` took
    /// its current value. Mirrors `TaskRuntime.dispatch_wait_since`.
    let dispatchWaitSince: String?
}

/// One row from `work_item_dependencies` — the dependent is gated by
/// the prerequisite for `relation` ("blocks" today). Carried in the
/// work tree so the kanban can render "Blocked by <prereq title>" on
/// blocked cards without an N+1 round trip.
struct WorkItemDependency: Hashable {
    let dependentID: String
    let prerequisiteID: String
    let relation: String
}

/// One execution of a task, mirroring `boss_protocol::WorkExecution`.
/// Used by the transcript viewer's execution list.
struct ExecutionVM: Identifiable, Hashable {
    let id: String
    /// The task id that owns this execution. When a transcript viewer
    /// loads the full revision chain, executions from revision tasks
    /// carry those tasks' ids here rather than the chain root's id.
    let workItemId: String
    let kind: String
    let status: String
    let model: String?
    let runId: String?
    let startedAt: String?
    let endedAt: String?
}
