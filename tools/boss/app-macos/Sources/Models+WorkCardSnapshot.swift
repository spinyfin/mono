import Foundation

// ===========================================================================
// Slim Equatable snapshot of everything `WorkBoardCardView` renders.
//
// Phase 1 of `tools/boss/docs/designs/boss-ui-performance-improvements.md`
// (entry 4). Holds the subset of `WorkTask` fields the card body reads, the
// already-resolved per-card context the column container supplies, and the
// booleans that today are recomputed inside `body` / `WorkBoardCardItem`
// (`isDispatchPending`, `isResolvingConflicts`, `isRemediatingCI`,
// `isAIReviewing`, per-badge visibility). Closures stay outside the
// snapshot — only the presence booleans that gate their buttons land here.
//
// Model + builder only: the view does not consume this type yet (sibling
// entry 5 wires `.equatable()`). Unit tests assert that two `WorkTask`s
// differing only in non-rendered fields produce equal snapshots, and that
// every rendered field participates in equality.
// ===========================================================================

/// Slim rollup row for nested revisions shown under a parent card. Mirrors
/// the fields `RevisionRollupLine` reads off a revision `WorkTask`.
struct WorkCardRevisionRollup: Equatable, Identifiable {
    let id: String
    let revisionSeq: Int?
    let name: String
    let revisionParentPrUrl: String?

    init(id: String, revisionSeq: Int?, name: String, revisionParentPrUrl: String?) {
        self.id = id
        self.revisionSeq = revisionSeq
        self.name = name
        self.revisionParentPrUrl = revisionParentPrUrl
    }

    init(revision: WorkTask) {
        self.id = revision.id
        self.revisionSeq = revision.revisionSeq
        self.name = revision.name
        self.revisionParentPrUrl = revision.revisionParentPrUrl
    }
}

/// Already-resolved per-card inputs the column container has computed
/// before building a [[WorkCardSnapshot]]. Closures (open terminal, merge
/// when ready, design-doc open, badge hover, deferred-scope actions) are
/// intentionally absent — only the presence flags that gate them appear
/// on the snapshot.
struct WorkCardSnapshotContext: Equatable {
    var column: WorkBoardColumnKey
    var projectName: String?
    var isSelected: Bool = false
    var runtime: WorkTaskRuntime? = nil
    var liveState: WorkerLiveState? = nil
    /// Pre-resolved free-text live-status subtitle. Callers compute any
    /// time-relative phrasing (dispatch retry / wait-since) before
    /// building so the snapshot itself stays pure / Equatable-stable.
    var liveStatus: String? = nil
    var liveStatusActivity: WorkerActivity? = nil
    var liveStatusLastEventAt: String? = nil
    var blockedBy: String? = nil
    var isAutoBlocked: Bool = false
    var gatingPrereqs: [WorkDependencyRow] = []
    var repoChip: RepoChipPresentation? = nil
    var showsConflictClearedBadge: Bool = false
    var showsCIAutoFixedBadge: Bool = false
    var ciFailureBadge: CiFailureBadge? = nil
    var isFrontierHighlighted: Bool = false
    var designDocState: ProjectDesignDocState? = nil
    var externalRefLink: ExternalRefLinkPresentation? = nil
    var ambiguousRepoNames: Set<String> = []
    var inReviewRevisions: [WorkCardRevisionRollup] = []
    var parentShortID: Int? = nil
    var deferredScopeItems: [DeferredScopeAttention] = []
    var deferredScopeActionInFlightIDs: Set<String> = []
    /// True when the caller will supply an `onOpenTerminal` closure.
    var showsTerminalButton: Bool = false
    var terminalTooltip: String = "Open terminal on PR branch"
    /// True when the caller will supply an `onMergeWhenReady` closure.
    var showsMergeWhenReady: Bool = false
}

/// Equatable value type holding exactly the fields `WorkBoardCardView`
/// renders, plus the booleans recomputed today inside body / item.
struct WorkCardSnapshot: Equatable {
    // MARK: - Identity / drag

    let id: String

    // MARK: - Rendered WorkTask fields

    let kind: String
    let name: String
    let status: String
    let priority: String
    let tags: [String]
    let effortLevel: String?
    let reasoning: String?
    let deferred: Bool
    let shortID: Int?
    let prURL: String?
    let revisionParentPrUrl: String?
    let revisionSeq: Int?
    let createdVia: String
    let hasInProgressRevision: Bool
    let sourceAutomationId: String?
    /// Engine-origin badge for revision cards (`nil` when operator-driven).
    let engineRevisionOrigin: EngineRevisionOrigin?

    // MARK: - Resolved context

    let projectName: String?
    let isSelected: Bool
    let activityState: AgentActivityState?
    let assignedSlotId: Int?
    let liveStatus: String?
    let liveStatusActivity: WorkerActivity?
    let liveStatusLastEventAt: String?
    let blockedBy: String?
    let isAutoBlocked: Bool
    let gatingPrereqs: [WorkDependencyRow]
    let autoBlockTooltip: String
    let repoChip: RepoChipPresentation?
    let ciFailureBadge: CiFailureBadge?
    let isFrontierHighlighted: Bool
    let designDocState: ProjectDesignDocState?
    let ciRequiredState: String?
    let ciRequiredDetail: String?
    let reviewRequiredState: String?
    let reviewRequiredDetail: String?
    let mergeQueueState: String?
    let mergeQueueDetail: String?
    let prMergeableState: String?
    let externalRefLink: ExternalRefLinkPresentation?
    let ambiguousRepoNames: Set<String>
    let inReviewRevisions: [WorkCardRevisionRollup]
    let parentShortID: Int?
    let terminalTooltip: String
    let deferredScopeItems: [DeferredScopeAttention]
    let deferredScopeActionInFlightIDs: Set<String>

    // MARK: - Lane / activity booleans (recomputed today in WorkBoardCardItem)

    let isDispatchPending: Bool
    let isResolvingConflicts: Bool
    let isRemediatingCI: Bool
    let isAIReviewing: Bool

    // MARK: - Precomputed badge / section visibility

    /// Orange lock next to the title when the card is blocked and not mid
    /// conflict/CI remediation.
    let showsBlockedLock: Bool
    let showsHighPriorityChip: Bool
    let showsEffortChip: Bool
    let showsReasoningChip: Bool
    let showsDeferredBadge: Bool
    let showsProjectBadge: Bool
    let showsAIReviewingBadge: Bool
    let showsResolvingConflictsBadge: Bool
    let showsResolvingCIBadge: Bool
    /// Precomputed `WorkBlockedBadge.badgeText` (nil collapses the chip).
    let blockedBadgeText: String?
    let blockedBadgeTooltip: String?
    let blockedBadgeHasMoreInfo: Bool
    /// True when the blocked chip is the Dependency variant (hover → frontier).
    let isDependencyBlockedBadge: Bool
    let showsAutoBlockedChain: Bool
    /// Mutual-exclusion result for the "conflict cleared" chip.
    let conflictClearedBadgeVisible: Bool
    let showsCIAutoFixedBadge: Bool
    let showsCIFailureChip: Bool
    let showsRepoChip: Bool
    let showsAutomationBadge: Bool
    let showsPlannerStagedBadge: Bool
    let showsExternalRefLink: Bool
    let showsDesignDocAffordance: Bool
    let showsTerminalButton: Bool
    let showsMergeWhenReady: Bool
    let showsDeferredScopeBadge: Bool
    let hasTagChips: Bool
    let hasLiveStatus: Bool
    let hasPRRow: Bool
    let hasReviewRow: Bool
    /// Second PR row for a revision whose parent PR differs from its own.
    let hasRevisionParentPRRow: Bool
    /// Standalone trailing short-id row when there is no PR URL.
    let hasStandaloneShortID: Bool
    let hasInReviewRevisions: Bool
    /// Orange blocked chrome (background + border) when not remediating.
    let showsBlockedChrome: Bool

    // MARK: - Builder

    /// Build a snapshot from a `WorkTask` plus the already-resolved
    /// per-card context the column container holds.
    static func build(task: WorkTask, context: WorkCardSnapshotContext) -> WorkCardSnapshot {
        let column = context.column

        let isDispatchPending = task.status == "todo" && task.autostart
        let isResolvingConflicts = column == .doing
            && task.status == "blocked"
            && task.blockedReason == "merge_conflict"
        let isRemediatingCI = column == .doing
            && task.status == "blocked"
            && task.blockedReason == "ci_failure"
        let isAIReviewing = column == .doing
            && task.aiReviewing
            && task.status == "active"

        let activityState: AgentActivityState? = column == .doing
            ? .forDoingCard(
                runtime: context.runtime,
                liveState: context.liveState,
                isDispatchPending: isDispatchPending,
                isResolvingConflicts: isResolvingConflicts,
                isRemediatingCI: isRemediatingCI,
                isAIReviewing: isAIReviewing)
            : nil

        let assignedSlotId: Int? = column == .doing ? context.liveState?.slotId : nil

        // Live-status subtitle is pre-resolved by the caller (may include
        // time-relative phrasing). Activity / last-event fall back to the
        // live worker state when the caller left them nil — matching the
        // non-pending path in `WorkBoardCardItem`.
        let liveStatus = context.liveStatus
        let liveStatusActivity: WorkerActivity? = {
            if isDispatchPending { return nil }
            if let explicit = context.liveStatusActivity { return explicit }
            return column == .doing ? context.liveState?.activity : nil
        }()
        let liveStatusLastEventAt: String? = {
            if isDispatchPending { return nil }
            if let explicit = context.liveStatusLastEventAt { return explicit }
            return column == .doing ? context.liveState?.lastEventAt : nil
        }()

        let inMerging = task.isInMergingSection
        let ciRequiredState: String? = (column == .review || inMerging)
            ? (task.ciRequiredState ?? "in_progress")
            : nil
        let ciRequiredDetail: String? = (column == .review || inMerging) ? task.ciRequiredDetail : nil
        let reviewRequiredState: String? = column == .review ? task.reviewRequiredState : nil
        let reviewRequiredDetail: String? = column == .review ? task.reviewRequiredDetail : nil
        let mergeQueueState: String? = inMerging ? task.mergeQueueState : nil
        let mergeQueueDetail: String? = inMerging ? task.mergeQueueDetail : nil

        let blockedBadgeText = WorkBlockedBadge.badgeText(for: task)
        let blockedBadgeTooltip = WorkBlockedBadge.badgeTooltip(for: task)
        let blockedBadgeHasMoreInfo = WorkBlockedBadge.hasMoreInfo(for: task)
        let isDependencyBlockedBadge = blockedBadgeText
            == WorkBlockedBadge.label(forReason: "dependency")

        let conflictClearedBadgeVisible = WorkBlockedBadge.conflictClearedVisible(
            forTask: task,
            cleared: context.showsConflictClearedBadge,
            isResolvingConflicts: isResolvingConflicts
        )

        let autoBlockTooltip: String = {
            guard !context.gatingPrereqs.isEmpty else {
                return "Auto-blocked by dependencies"
            }
            let summary = context.gatingPrereqs
                .map { "\($0.title) (\($0.status.replacingOccurrences(of: "_", with: " ")))" }
                .joined(separator: ", ")
            return "Gated by: \(summary)"
        }()

        let effortNonEmpty = task.effortLevel
            .map { !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
            ?? false
        let projectNameNonEmpty = context.projectName
            .map { !$0.isEmpty }
            ?? false
        let prURLNonEmpty = task.prURL.map { !$0.isEmpty } ?? false
        let tagChips = WorkTagPresentation.chips(for: task.tags)
        let liveStatusNonEmpty = liveStatus.map { !$0.isEmpty } ?? false

        // Design-doc affordance: state must resolve to a presentation, and
        // the caller must have wired a button (matches today's body gate).
        let designDocPresentation: Bool = {
            guard let state = context.designDocState else { return false }
            return ProjectDesignDocAffordancePresentation.from(state: state) != nil
        }()

        // Revision parent PR row only when it points somewhere different
        // from the card's own PR (avoids the #1829 double-link).
        let hasRevisionParentPRRow: Bool = {
            guard task.kind == "revision",
                  let parentPR = task.revisionParentPrUrl,
                  !parentPR.isEmpty
            else { return false }
            if let own = task.prURL, sameGitHubPR(own, parentPR) {
                return false
            }
            return true
        }()

        let showsResolvingConflicts = isResolvingConflicts
        let showsResolvingCI = !isResolvingConflicts && isRemediatingCI
        // Blocked badge is mutually exclusive with the resolving chips.
        let showsBlockedBadge = !isResolvingConflicts
            && !isRemediatingCI
            && blockedBadgeText != nil

        return WorkCardSnapshot(
            id: task.id,
            kind: task.kind,
            name: task.name,
            status: task.status,
            priority: task.priority,
            tags: task.tags,
            effortLevel: task.effortLevel,
            reasoning: task.reasoning,
            deferred: task.deferred,
            shortID: task.shortID,
            prURL: task.prURL,
            revisionParentPrUrl: task.revisionParentPrUrl,
            revisionSeq: task.revisionSeq,
            createdVia: task.createdVia,
            hasInProgressRevision: task.hasInProgressRevision,
            sourceAutomationId: task.sourceAutomationId,
            engineRevisionOrigin: EngineRevisionOrigin(createdVia: task.createdVia),
            projectName: context.projectName,
            isSelected: context.isSelected,
            activityState: activityState,
            assignedSlotId: assignedSlotId,
            liveStatus: liveStatus,
            liveStatusActivity: liveStatusActivity,
            liveStatusLastEventAt: liveStatusLastEventAt,
            blockedBy: context.blockedBy,
            isAutoBlocked: context.isAutoBlocked,
            gatingPrereqs: context.gatingPrereqs,
            autoBlockTooltip: autoBlockTooltip,
            repoChip: context.repoChip,
            ciFailureBadge: context.ciFailureBadge,
            isFrontierHighlighted: context.isFrontierHighlighted,
            designDocState: context.designDocState,
            ciRequiredState: ciRequiredState,
            ciRequiredDetail: ciRequiredDetail,
            reviewRequiredState: reviewRequiredState,
            reviewRequiredDetail: reviewRequiredDetail,
            mergeQueueState: mergeQueueState,
            mergeQueueDetail: mergeQueueDetail,
            prMergeableState: task.prMergeableState,
            externalRefLink: context.externalRefLink,
            ambiguousRepoNames: context.ambiguousRepoNames,
            inReviewRevisions: context.inReviewRevisions,
            parentShortID: context.parentShortID,
            terminalTooltip: context.terminalTooltip,
            deferredScopeItems: context.deferredScopeItems,
            deferredScopeActionInFlightIDs: context.deferredScopeActionInFlightIDs,
            isDispatchPending: isDispatchPending,
            isResolvingConflicts: isResolvingConflicts,
            isRemediatingCI: isRemediatingCI,
            isAIReviewing: isAIReviewing,
            showsBlockedLock: task.status == "blocked"
                && !isResolvingConflicts
                && !isRemediatingCI,
            showsHighPriorityChip: WorkPriority.parse(task.priority) == .high,
            showsEffortChip: effortNonEmpty,
            showsReasoningChip: task.reasoning == "investigation",
            showsDeferredBadge: task.deferred,
            showsProjectBadge: projectNameNonEmpty,
            showsAIReviewingBadge: task.aiReviewing && task.status == "active",
            showsResolvingConflictsBadge: showsResolvingConflicts,
            showsResolvingCIBadge: showsResolvingCI,
            blockedBadgeText: showsBlockedBadge ? blockedBadgeText : nil,
            blockedBadgeTooltip: showsBlockedBadge ? blockedBadgeTooltip : nil,
            blockedBadgeHasMoreInfo: showsBlockedBadge && blockedBadgeHasMoreInfo,
            isDependencyBlockedBadge: showsBlockedBadge && isDependencyBlockedBadge,
            showsAutoBlockedChain: context.isAutoBlocked,
            conflictClearedBadgeVisible: conflictClearedBadgeVisible,
            showsCIAutoFixedBadge: context.showsCIAutoFixedBadge && context.ciFailureBadge == nil,
            showsCIFailureChip: context.ciFailureBadge != nil && !isRemediatingCI,
            showsRepoChip: context.repoChip != nil,
            showsAutomationBadge: task.sourceAutomationId != nil,
            showsPlannerStagedBadge: task.isPlannerStaged,
            showsExternalRefLink: context.externalRefLink != nil,
            showsDesignDocAffordance: designDocPresentation,
            showsTerminalButton: context.showsTerminalButton,
            showsMergeWhenReady: context.showsMergeWhenReady,
            showsDeferredScopeBadge: !context.deferredScopeItems.isEmpty,
            hasTagChips: !tagChips.labels.isEmpty,
            hasLiveStatus: liveStatusNonEmpty,
            hasPRRow: prURLNonEmpty,
            hasReviewRow: prURLNonEmpty && reviewRequiredState != nil,
            hasRevisionParentPRRow: hasRevisionParentPRRow,
            hasStandaloneShortID: !prURLNonEmpty && task.shortID != nil,
            hasInReviewRevisions: !context.inReviewRevisions.isEmpty,
            showsBlockedChrome: !isResolvingConflicts
                && !isRemediatingCI
                && task.status == "blocked"
        )
    }
}
