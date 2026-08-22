import SwiftUI

// ===========================================================================
// Metadata badge strip (priority / effort / blocked / CI / design-doc /
// terminal / merge / deferred-scope, …).
//
// Equatable over its own slice of [[WorkCardSnapshot]]. Action closures
// stay outside `==` so handler-identity churn does not force a re-layout
// of the strip (design entry 9; same closure rule as `WorkBoardCardView`).
// ===========================================================================

/// Inputs the badge strip paints. Presence booleans + the small values
/// each chip needs; closures are on the view, not the slice.
struct WorkBoardCardBadgeStripSlice: Equatable {
    let showsHighPriorityChip: Bool
    let showsEffortChip: Bool
    let effortLevel: String?
    let showsReasoningChip: Bool
    let showsDeferredBadge: Bool
    let showsHumanDrivenBadge: Bool
    let showsProjectBadge: Bool
    let projectName: String?
    let aiReviewState: String?
    let aiReviewFindingsRevisionId: String?
    let showsResolvingConflictsBadge: Bool
    let showsResolvingCIBadge: Bool
    let blockedBadgeText: String?
    let blockedBadgeTooltip: String?
    let blockedBadgeHasMoreInfo: Bool
    let isDependencyBlockedBadge: Bool
    let showsAutoBlockedChain: Bool
    let autoBlockTooltip: String
    let conflictClearedBadgeVisible: Bool
    let showsCIAutoFixedBadge: Bool
    let showsCIFailureChip: Bool
    let ciFailureBadge: CiFailureBadge?
    let showsRepoChip: Bool
    let repoChip: RepoChipPresentation?
    let showsAutomationBadge: Bool
    let showsPlannerStagedBadge: Bool
    let showsExternalRefLink: Bool
    let externalRefLink: ExternalRefLinkPresentation?
    let showsDesignDocAffordance: Bool
    let designDocState: ProjectDesignDocState?
    let showsAttachmentsAffordance: Bool
    let showsTerminalButton: Bool
    let terminalTooltip: String
    let showsMergeWhenReady: Bool
    let showsDeferredScopeBadge: Bool
    let deferredScopeItems: [DeferredScopeAttention]
    let deferredScopeActionInFlightIDs: Set<String>

    init(snapshot: WorkCardSnapshot) {
        self.showsHighPriorityChip = snapshot.showsHighPriorityChip
        self.showsEffortChip = snapshot.showsEffortChip
        self.effortLevel = snapshot.effortLevel
        self.showsReasoningChip = snapshot.showsReasoningChip
        self.showsDeferredBadge = snapshot.showsDeferredBadge
        self.showsHumanDrivenBadge = snapshot.showsHumanDrivenBadge
        self.showsProjectBadge = snapshot.showsProjectBadge
        self.projectName = snapshot.projectName
        self.aiReviewState = snapshot.aiReviewState
        self.aiReviewFindingsRevisionId = snapshot.aiReviewFindingsRevisionId
        self.showsResolvingConflictsBadge = snapshot.showsResolvingConflictsBadge
        self.showsResolvingCIBadge = snapshot.showsResolvingCIBadge
        self.blockedBadgeText = snapshot.blockedBadgeText
        self.blockedBadgeTooltip = snapshot.blockedBadgeTooltip
        self.blockedBadgeHasMoreInfo = snapshot.blockedBadgeHasMoreInfo
        self.isDependencyBlockedBadge = snapshot.isDependencyBlockedBadge
        self.showsAutoBlockedChain = snapshot.showsAutoBlockedChain
        self.autoBlockTooltip = snapshot.autoBlockTooltip
        self.conflictClearedBadgeVisible = snapshot.conflictClearedBadgeVisible
        self.showsCIAutoFixedBadge = snapshot.showsCIAutoFixedBadge
        self.showsCIFailureChip = snapshot.showsCIFailureChip
        self.ciFailureBadge = snapshot.ciFailureBadge
        self.showsRepoChip = snapshot.showsRepoChip
        self.repoChip = snapshot.repoChip
        self.showsAutomationBadge = snapshot.showsAutomationBadge
        self.showsPlannerStagedBadge = snapshot.showsPlannerStagedBadge
        self.showsExternalRefLink = snapshot.showsExternalRefLink
        self.externalRefLink = snapshot.externalRefLink
        self.showsDesignDocAffordance = snapshot.showsDesignDocAffordance
        self.designDocState = snapshot.designDocState
        self.showsAttachmentsAffordance = snapshot.showsAttachmentsAffordance
        self.showsTerminalButton = snapshot.showsTerminalButton
        self.terminalTooltip = snapshot.terminalTooltip
        self.showsMergeWhenReady = snapshot.showsMergeWhenReady
        self.showsDeferredScopeBadge = snapshot.showsDeferredScopeBadge
        self.deferredScopeItems = snapshot.deferredScopeItems
        self.deferredScopeActionInFlightIDs = snapshot.deferredScopeActionInFlightIDs
    }
}

/// Flowing badge cluster under the title / live-status rows.
struct WorkBoardCardBadgeStrip: View, @MainActor Equatable {
    let slice: WorkBoardCardBadgeStripSlice
    /// Invoked when the user taps the design-doc affordance.
    var onOpenDesignDoc: (() -> Void)? = nil
    /// Invoked when the user taps the screenshots affordance; also gated by
    /// `slice.showsAttachmentsAffordance`.
    var onOpenAttachments: (() -> Void)? = nil
    /// Dependency-badge hover enter/exit.
    var onDepBadgeHover: ((Bool) -> Void)? = nil
    /// Terminal open; also gated by `slice.showsTerminalButton`.
    var onOpenTerminal: (() -> Void)? = nil
    /// Merge-when-ready confirm; also gated by `slice.showsMergeWhenReady`.
    var onMergeWhenReady: (() -> Void)? = nil
    /// Invoked when the user taps the `reviewed_with_findings` badge —
    /// reveals the follow-up revision that carries the review comments.
    /// Only called when `slice.aiReviewFindingsRevisionId` is non-nil.
    var onRevealAIReviewFindings: (() -> Void)? = nil
    var onAcceptDeferredScope: ((String) -> Void)? = nil
    var onCreateTaskFromDeferredScope: ((String) -> Void)? = nil

    @State private var showMergeConfirmation: Bool = false

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.slice == rhs.slice
    }

    var body: some View {
        // Wrap the whole metadata cluster so a full badge set flows onto
        // additional lines within the lane width instead of overflowing
        // past the card's right edge and clipping (#1172).
        FlowLayout(horizontalSpacing: 6, verticalSpacing: 4) {
            if slice.showsHighPriorityChip {
                PriorityChip(priority: .high)
            }
            if slice.showsEffortChip, let effortLevel = slice.effortLevel {
                EffortChip(effortLevel: effortLevel)
            }
            if slice.showsReasoningChip {
                ReasoningChip()
            }
            if slice.showsDeferredBadge {
                FutureScopeBadge()
            }
            if slice.showsHumanDrivenBadge {
                HumanDrivenBadge()
            }
            if slice.showsProjectBadge, let projectName = slice.projectName {
                WorkStatusBadge(text: projectName)
            }
            if let aiReviewState = slice.aiReviewState {
                AIReviewStateBadge(
                    state: aiReviewState,
                    onRevealFindings: slice.aiReviewFindingsRevisionId != nil ? onRevealAIReviewFindings : nil
                )
            }
            if slice.showsResolvingConflictsBadge {
                ResolvingConflictsBadge()
            } else if slice.showsResolvingCIBadge {
                ResolvingCIFailureBadge()
            } else if let blockedText = slice.blockedBadgeText {
                WorkStatusBadge(
                    text: blockedText,
                    tooltip: slice.blockedBadgeTooltip,
                    hasMoreInfo: slice.blockedBadgeHasMoreInfo
                )
                .onHover { hovering in
                    if slice.isDependencyBlockedBadge {
                        onDepBadgeHover?(hovering)
                    }
                }
            }
            if slice.showsAutoBlockedChain {
                Image(systemName: "link")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.orange)
                    .help(slice.autoBlockTooltip)
                    .accessibilityLabel("Auto-blocked by dependencies")
                    .accessibilityValue(slice.autoBlockTooltip)
                    .onHover { hovering in
                        onDepBadgeHover?(hovering)
                    }
            }
            if slice.conflictClearedBadgeVisible {
                ConflictClearedBadge()
            }
            if slice.showsCIAutoFixedBadge {
                CIAutoFixedBadge()
            }
            if slice.showsCIFailureChip, let ciFailureBadge = slice.ciFailureBadge {
                CIFailureChip(badge: ciFailureBadge)
            }
            if slice.showsRepoChip, let repoChip = slice.repoChip {
                RepoChipView(presentation: repoChip)
            }
            if slice.showsAutomationBadge {
                Image(systemName: "wand.and.stars")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.purple)
                    .help("Created by automation")
                    .accessibilityLabel("Created by automation")
            }
            if slice.showsPlannerStagedBadge {
                Image(systemName: "sparkle.magnifyingglass")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.indigo)
                    .help("Staged by the Planner — release the project to begin dispatch")
                    .accessibilityLabel("Staged by the Planner")
            }
            if slice.showsExternalRefLink, let extRef = slice.externalRefLink {
                ExternalRefLinkView(presentation: extRef)
            }
            // Doc-link icon. Eligibility is already encoded in
            // `showsDesignDocAffordance` + `designDocState`.
            if slice.showsDesignDocAffordance,
               let state = slice.designDocState,
               let presentation = ProjectDesignDocAffordancePresentation.from(state: state) {
                Button {
                    onOpenDesignDoc?()
                } label: {
                    Image(systemName: presentation.systemImage)
                        .font(.caption)
                        .foregroundStyle(presentation.tint)
                        .accessibilityLabel(presentation.accessibilityLabel)
                }
                .buttonStyle(.plain)
                .help(presentation.tooltip)
            }
            if slice.showsAttachmentsAffordance {
                Button {
                    onOpenAttachments?()
                } label: {
                    Image(systemName: "photo.on.rectangle.angled")
                        .font(.caption)
                        .foregroundStyle(Color.secondary)
                        .accessibilityLabel("View screenshots")
                }
                .buttonStyle(.plain)
                .help("View screenshots attached to this task")
            }
            if slice.showsTerminalButton, let openTerminal = onOpenTerminal {
                Button {
                    openTerminal()
                } label: {
                    Image(systemName: "terminal")
                        .font(.caption)
                        .foregroundStyle(Color.secondary)
                        .accessibilityLabel(slice.terminalTooltip)
                }
                .buttonStyle(.plain)
                .help(slice.terminalTooltip)
            }
            if slice.showsMergeWhenReady {
                Button {
                    showMergeConfirmation = true
                } label: {
                    Image(systemName: "arrow.triangle.merge")
                        .font(.caption)
                        .foregroundStyle(Color.secondary)
                        .accessibilityLabel("Merge when ready")
                }
                .buttonStyle(.plain)
                .help("Merge When Ready: enqueue this PR for merging once all required checks pass")
                // Always-attached: confirmationDialog needs false→true while
                // installed; mount-with-true is a known intermittent failure.
                .confirmationDialog(
                    "Merge When Ready",
                    isPresented: $showMergeConfirmation,
                    titleVisibility: .visible
                ) {
                    Button("Confirm Merge When Ready") {
                        onMergeWhenReady?()
                    }
                    Button("Cancel", role: .cancel) {}
                } message: {
                    Text("This will queue the PR for merging once all required checks pass. This action cannot be undone.")
                }
            }
            if slice.showsDeferredScopeBadge {
                DeferredScopeCardBadge(
                    items: slice.deferredScopeItems,
                    actionInFlightIDs: slice.deferredScopeActionInFlightIDs,
                    onAccept: { onAcceptDeferredScope?($0) },
                    onCreateTask: { onCreateTaskFromDeferredScope?($0) }
                )
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
