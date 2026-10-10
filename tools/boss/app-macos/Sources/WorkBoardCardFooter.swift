import SwiftUI

// ===========================================================================
// Card footer cluster — PR link / CI / merge-queue, review indicator,
// revision-parent PR, standalone short-id, and in-review revision rollups.
//
// Equatable over its own slice of [[WorkCardSnapshot]] so title / badge /
// live-status churn does not re-lay-out the footer (design entry 9).
// Hover closures stay outside `==`.
// ===========================================================================

/// Inputs the footer / PR rows paint.
struct WorkBoardCardFooterSlice: Equatable {
    let hasPRRow: Bool
    let prURL: String?
    let mergeQueueState: String?
    let mergeQueueDetail: String?
    let ciRequiredState: String?
    let ciRequiredDetail: String?
    let prMergeableState: String?
    let ambiguousRepoNames: Set<String>
    let hasInProgressRevision: Bool
    let shortID: Int?
    let hasReviewRow: Bool
    let reviewRequiredState: String?
    let reviewRequiredDetail: String?
    let hasRevisionParentPRRow: Bool
    let revisionParentPrUrl: String?
    let hasStandaloneShortID: Bool
    let hasInReviewRevisions: Bool
    let inReviewRevisions: [WorkCardRevisionRollup]

    init(snapshot: WorkCardSnapshot) {
        self.hasPRRow = snapshot.hasPRRow
        self.prURL = snapshot.prURL
        self.mergeQueueState = snapshot.mergeQueueState
        self.mergeQueueDetail = snapshot.mergeQueueDetail
        self.ciRequiredState = snapshot.ciRequiredState
        self.ciRequiredDetail = snapshot.ciRequiredDetail
        self.prMergeableState = snapshot.prMergeableState
        self.ambiguousRepoNames = snapshot.ambiguousRepoNames
        self.hasInProgressRevision = snapshot.hasInProgressRevision
        self.shortID = snapshot.shortID
        self.hasReviewRow = snapshot.hasReviewRow
        self.reviewRequiredState = snapshot.reviewRequiredState
        self.reviewRequiredDetail = snapshot.reviewRequiredDetail
        self.hasRevisionParentPRRow = snapshot.hasRevisionParentPRRow
        self.revisionParentPrUrl = snapshot.revisionParentPrUrl
        self.hasStandaloneShortID = snapshot.hasStandaloneShortID
        self.hasInReviewRevisions = snapshot.hasInReviewRevisions
        self.inReviewRevisions = snapshot.inReviewRevisions
    }

    /// True when any footer section would render content.
    var isEmpty: Bool {
        !hasPRRow
            && !hasReviewRow
            && !hasRevisionParentPRRow
            && !hasStandaloneShortID
            && !hasInReviewRevisions
    }
}

/// Frame (global space) of the rendered short id; lets tests assert its anchor.
struct ShortIDFramePreferenceKey: PreferenceKey {
    static let defaultValue: [CGRect] = []
    static func reduce(value: inout [CGRect], nextValue: () -> [CGRect]) {
        value.append(contentsOf: nextValue())
    }
}

/// PR / review / short-id / revision-rollup footer under the badge strip.
struct WorkBoardCardFooter: View, @MainActor Equatable {
    let slice: WorkBoardCardFooterSlice
    /// Called with `true` when the pointer enters the "In revision" badge;
    /// `false` on exit.
    var onRevisionBadgeHover: ((Bool) -> Void)? = nil
    /// Invoked when the user taps the "In revision" badge.
    var onRevisionBadgeTap: (() -> Void)? = nil

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.slice == rhs.slice
    }

    /// True when a row sits below the PR / badge rows (review status,
    /// revision-parent PR, or the in-review rollup list).
    private var hasRowsBelowPRRow: Bool {
        (slice.hasReviewRow && slice.reviewRequiredState != nil)
            || (slice.hasRevisionParentPRRow && slice.revisionParentPrUrl != nil)
            || slice.hasInReviewRevisions
    }

    /// The id rides on the PR row (or the badge row) only when nothing renders
    /// below it; otherwise it gets its own trailing row so it always stays the
    /// bottom-right element of the footer.
    private var idIsInline: Bool {
        slice.hasPRRow && slice.prURL != nil && !hasRowsBelowPRRow
    }

    private func shortIDLabel(_ id: Int) -> some View {
        Text("T" + String(id))
            .background(
                GeometryReader { proxy in
                    Color.clear.preference(
                        key: ShortIDFramePreferenceKey.self,
                        value: [proxy.frame(in: .global)]
                    )
                }
            )
            .font(.system(.caption2, design: .monospaced))
            .foregroundStyle(.secondary)
            .accessibilityLabel("T" + String(id))
            .lineLimit(1)
            .fixedSize(horizontal: true, vertical: false)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if slice.hasPRRow, let prURL = slice.prURL {
                HStack(alignment: .center, spacing: 6) {
                    if let mergeQueueState = slice.mergeQueueState {
                        MergeQueueBadge(
                            mergeQueueState: mergeQueueState,
                            detail: slice.mergeQueueDetail,
                            ciRequiredState: slice.ciRequiredState,
                            prMergeableState: slice.prMergeableState
                        )
                        .layoutPriority(-1)
                    } else {
                        if let ciState = slice.ciRequiredState {
                            PrCiIndicator(
                                state: ciState,
                                detail: slice.ciRequiredDetail
                            )
                        }
                        PrConflictIndicator(prMergeableState: slice.prMergeableState)
                    }
                    PRURLLink(
                        urlString: prURL,
                        font: .caption,
                        ambiguousRepoNames: slice.ambiguousRepoNames
                    )
                    .layoutPriority(1)
                    Spacer(minLength: 0)
                    // With a revision badge the id moves to that (bottom) row so
                    // it stays anchored bottom-right.
                    if idIsInline, let id = slice.shortID, !slice.hasInProgressRevision {
                        shortIDLabel(id)
                    }
                }
                if slice.hasInProgressRevision {
                    HStack(alignment: .center, spacing: 6) {
                        PrInRevisionIndicator(onTap: onRevisionBadgeTap)
                            .onHover { hovering in
                                onRevisionBadgeHover?(hovering)
                            }
                        Spacer(minLength: 0)
                        if idIsInline, let id = slice.shortID {
                            shortIDLabel(id)
                        }
                    }
                }
            }

            if slice.hasReviewRow, let reviewState = slice.reviewRequiredState {
                HStack(spacing: 6) {
                    PrReviewIndicator(state: reviewState, detail: slice.reviewRequiredDetail)
                    Spacer(minLength: 0)
                }
            }

            // Second PR row for a revision whose parent PR differs from
            // its own (avoids the #1829 double-link). Visibility is
            // precomputed on the snapshot.
            if slice.hasRevisionParentPRRow, let prURL = slice.revisionParentPrUrl {
                HStack(alignment: .center, spacing: 6) {
                    PRURLLink(
                        urlString: prURL,
                        font: .caption,
                        ambiguousRepoNames: slice.ambiguousRepoNames
                    )
                    Spacer(minLength: 0)
                }
            }

            if slice.hasInReviewRevisions {
                Divider()
                    .padding(.vertical, 2)
                ForEach(slice.inReviewRevisions) { revision in
                    RevisionRollupLine(revision: revision)
                }
            }

            if !idIsInline, slice.hasPRRow || slice.hasStandaloneShortID, let id = slice.shortID {
                HStack {
                    Spacer(minLength: 0)
                    shortIDLabel(id)
                }
            }
        }
    }
}
