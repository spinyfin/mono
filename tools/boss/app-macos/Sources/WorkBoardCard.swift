import AppKit
import os.log
import SwiftUI
import UpdateCore

// Debug logger for the investigation doc-link render path. Uses .debug() so
// it is silent in normal use; enable via Console.app subsystem filter or
// Xcode debug console. Surfaces work_item_id, kind, pr_url value, column,
// and whether PRURLLink will render — letting the operator identify which
// of the three known gap sites (delivery, render, stale build) is live.
private let kanbanDocLinkLog = Logger(
    subsystem: "dev.spinyfin.bossmacapp",
    category: "kanban-doc-link"
)

/// Column-local card list. Observes `LiveWorkerStateStore` so Doing-lane
/// live-status ticks recompute snapshots without every card subscribing
/// (design entry 6). Does **not** observe `ChatViewModel` — the parent
/// `ContentView` already does; `model` is held for snapshot construction
/// and passed non-observing into each card for action dispatch only.
struct WorkBoardSectionItemsView: View {
    let items: [WorkTask]
    let column: WorkBoardColumnKey
    let boardStyle: KanbanBoardStyle
    /// Non-observing. Parent `ContentView` owns `ChatViewModel` observation.
    let model: ChatViewModel
    /// Observed here so live-state publishes rebuild Doing snapshots once
    /// per section instead of invalidating every mounted card.
    @ObservedObject var liveStates: LiveWorkerStateStore

    var body: some View {
        let selectedID = model.selectedTask?.id
        let highlightID = model.revealHighlightID
        let frontierIDs = model.depFrontierHighlightIDs
        let revisionIDs = model.revisionHighlightIDs
        let selectedRevisionParentID = model.selectedRevisionParentID
        let dragRefusal = model.dragRefusalNotice
        let mergeFeedback = model.mergeFeedbackNotice
        // Lazy so off-screen cards aren't instantiated/hit-tested at all — with
        // the default (ungrouped) board layout each column is a single section,
        // so this was the actual eagerly-built list of every card in the
        // column regardless of scroll position. Combined with whole-model
        // `@Published` invalidation that hover badges trigger, a plain
        // `VStack` here meant hovering one badge while scrolling re-evaluated
        // and re-hit-tested every card on the board, not just the visible
        // ones. `LazyVStack` + `ScrollViewReader` + `.id(task.id)` below is
        // the supported combo for reveal-scroll, so this doesn't change that
        // behavior. Entry 6 further stops per-card observation of the model /
        // live store: the container rebuilds snapshots; `.equatable()` skips
        // card bodies whose snapshot did not change.
        LazyVStack(alignment: .leading, spacing: 10) {
            ForEach(items) { task in
                let isSelected = selectedID == task.id
                let isFrontierHighlighted = frontierIDs.contains(task.id)
                    || revisionIDs.contains(task.id)
                    || selectedRevisionParentID == task.id
                let liveState: WorkerLiveState? = {
                    guard column == .doing,
                          let executionID = model.taskRuntime(for: task.id)?.executionID
                    else { return nil }
                    return liveStates.byRunID[executionID]
                }()
                let snapshot = model.workCardSnapshot(
                    for: task,
                    column: column,
                    isSelected: isSelected,
                    isFrontierHighlighted: isFrontierHighlighted,
                    boardStyle: boardStyle,
                    liveState: liveState
                )
                WorkBoardCardItem(
                    task: task,
                    column: column,
                    snapshot: snapshot,
                    model: model,
                    isRevealed: highlightID == task.id,
                    dragRefusalMessage: dragRefusal?.taskID == task.id
                        ? dragRefusal?.message : nil,
                    mergeFeedbackMessage: mergeFeedback?.taskID == task.id
                        ? mergeFeedback?.message : nil
                )
                .id(task.id)
            }
        }
    }
}

/// Wrapper for a single kanban card. Receives a pre-built
/// [[WorkCardSnapshot]] and resolved banner messages as **values** from
/// the column container — it does **not** observe `ChatViewModel` or
/// `LiveWorkerStateStore`. Holding `model` without `@ObservedObject`
/// keeps action dispatch (`selectWorkCard`, terminal, merge, delete)
/// without the "any of 77 `@Published` properties invalidates every
/// visible card" fan-out (design entry 6).
struct WorkBoardCardItem: View {
    let task: WorkTask
    /// Board column for action routing / debug logs only (value, not observed).
    let column: WorkBoardColumnKey
    let snapshot: WorkCardSnapshot
    /// Non-observing; action dispatch only. Column container owns
    /// observation and snapshot construction.
    let model: ChatViewModel
    var isRevealed: Bool = false
    /// Pre-resolved by the column container from `dragRefusalNotice`.
    var dragRefusalMessage: String? = nil
    /// Pre-resolved by the column container from `mergeFeedbackNotice`.
    var mergeFeedbackMessage: String? = nil
    @Environment(\.openWindow) private var openWindow
    @State private var showingDeleteConfirmation = false

    var body: some View {
        // Action closures stay outside the snapshot so `.equatable()` on
        // the card body can skip re-evaluation when only handler identity
        // would differ (design entry 5 / `WorkCardSnapshot`).
        let onOpenTerminal: (() -> Void)? = {
            guard snapshot.showsTerminalButton else { return nil }
            if (column == .review || column == .done),
               let prURL = task.prURL, !prURL.isEmpty {
                return { model.openReviewTerminal(for: task) }
            }
            return { model.openLiveWorkspaceTerminal(for: task) }
        }()
        let onMergeWhenReady: (() -> Void)? = snapshot.showsMergeWhenReady
            ? { model.mergeWhenReady(for: task) }
            : nil
        let onOpenDesignDoc: (() -> Void)? = {
            guard snapshot.showsDesignDocAffordance else { return nil }
            if (task.kind == "design" || task.kind == "design_postmortem"),
               let projectID = task.projectID,
               let proj = model.project(withID: projectID) {
                return { model.openProjectDesignDoc(proj) }
            }
            if task.docLinkState != nil {
                return { model.openTaskDoc(task) }
            }
            // Snapshot said the affordance shows (resolved design-doc
            // state) but project lookup failed at action-build time —
            // still wire a best-effort open via project re-lookup on tap.
            if let projectID = task.projectID {
                return {
                    if let proj = model.project(withID: projectID) {
                        model.openProjectDesignDoc(proj)
                    }
                }
            }
            return nil
        }()

        VStack(alignment: .leading, spacing: 6) {
            Button {
                model.selectWorkCard(snapshot.isSelected ? nil : task.id)
            } label: {
                WorkBoardCardView(
                    snapshot: snapshot,
                    onOpenDesignDoc: onOpenDesignDoc,
                    onDepBadgeHover: { hovering in
                        model.setDepBadgeHover(hovering ? task.id : nil)
                    },
                    onRevisionBadgeHover: { hovering in
                        model.setRevisionBadgeHover(hovering ? task.id : nil)
                    },
                    onOpenTerminal: onOpenTerminal,
                    onMergeWhenReady: onMergeWhenReady,
                    onAcceptDeferredScope: { id in model.acceptDeferredScopeAttention(id: id) },
                    onCreateTaskFromDeferredScope: { id in
                        model.createTaskFromDeferredScopeAttention(attentionID: id)
                    }
                )
                // `WorkBoardCardView` is `Equatable` over its snapshot only:
                // without `.equatable()`, every re-render of the column
                // rebuilds and re-lays-out every card body. Closures are
                // intentionally outside `==`.
                .equatable()
            }
            .buttonStyle(.plain)
            .overlay(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .strokeBorder(
                        Color.accentColor.opacity(isRevealed ? 0.85 : 0),
                        lineWidth: 3
                    )
                    .animation(.easeInOut(duration: 0.25), value: isRevealed)
            )
            .overlay(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .strokeBorder(
                        Color.green.opacity(snapshot.isFrontierHighlighted ? 0.7 : 0),
                        lineWidth: 2
                    )
                    .animation(.easeInOut(duration: 0.15), value: snapshot.isFrontierHighlighted)
            )
            .contextMenu {
                if let id = task.shortID {
                    Button("Copy ID") {
                        let pb = NSPasteboard.general
                        pb.clearContents()
                        pb.setString("T" + String(id), forType: .string)
                    }
                }
                Button("View transcripts…") {
                    openWindow(id: "transcript-viewer", value: TranscriptViewerRef(taskId: task.id))
                }
                Divider()
                Button("Delete", role: .destructive) {
                    showingDeleteConfirmation = true
                }
            }
            .popover(
                isPresented: Binding(
                    get: { snapshot.isSelected },
                    set: { isPresented in
                        if !isPresented, snapshot.isSelected {
                            model.selectWorkCard(nil)
                        }
                    }
                ),
                arrowEdge: .trailing
            ) {
                WorkCardPopoverView(model: model, task: task)
            }

            if let dispatchFailedReason = task.dispatchFailedReason {
                WorkDispatchFailureBanner(reason: dispatchFailedReason, errorText: task.dispatchFailedError)
            }

            if let dragRefusalMessage {
                WorkDragRefusalBanner(message: dragRefusalMessage) {
                    model.clearDragRefusal()
                }
            }

            if let mergeFeedbackMessage {
                WorkMergeFeedbackBanner(message: mergeFeedbackMessage) {
                    model.clearMergeFeedback()
                }
            }
        }
        .onAppear { logDocLinkState("appeared") }
        .onChange(of: task.prURL) { _, _ in logDocLinkState("prURL-changed") }
        .confirmationDialog(
            "Delete \"\(task.name)\"?",
            isPresented: $showingDeleteConfirmation,
            titleVisibility: .visible
        ) {
            Button("Delete", role: .destructive) {
                model.deleteWorkItem(id: task.id)
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This is a soft-delete and can be recovered with: boss task restore")
        }
    }

    // Emits a debug log entry capturing the full doc-link render state for
    // this card. Gated at .debug() so it is silent in normal builds; surface
    // via Console.app (filter subsystem "dev.spinyfin.bossmacapp", category
    // "kanban-doc-link") or the Xcode debug console.
    //
    // Captured fields:
    //   event    — what triggered the log ("appeared" or "prURL-changed")
    //   id       — work_item_id (T-number correlates with engine logs)
    //   kind     — task kind ("investigation", "design", …)
    //   column   — board column the card routes to (from snapshot context)
    //   prURL    — the exact pr_url value the app received from the engine
    //              ("<nil>" = field absent/null on the wire; "empty" = "")
    //   link     — whether PRURLLink will render ("shown" or "skipped")
    //   skipReason — when link == "skipped", why (nil_or_empty vs none)
    private func logDocLinkState(_ event: String) {
        let prURLDesc: String
        let linkShown: Bool
        let skipReason: String

        if let u = task.prURL {
            prURLDesc = u.isEmpty ? "empty" : u
            linkShown = !u.isEmpty
            skipReason = u.isEmpty ? "empty_string" : "none"
        } else {
            prURLDesc = "<nil>"
            linkShown = false
            skipReason = "nil"
        }

        kanbanDocLinkLog.debug(
            """
            \(event, privacy: .public) \
            id=\(task.id, privacy: .public) \
            kind=\(task.kind, privacy: .public) \
            column=\(column.rawValue, privacy: .public) \
            prURL=\(prURLDesc, privacy: .public) \
            link=\(linkShown ? "shown" : "skipped", privacy: .public) \
            skipReason=\(skipReason, privacy: .public)
            """
        )
    }
}

/// Rendered directly on a kanban card whenever the engine gave up
/// starting it (`task.dispatchFailedReason` set) — the card has been
/// bounced to Backlog with `autostart` cleared, so it will NOT
/// auto-retry. Distinguishes "failing to start" from "waiting for a
/// slot": the latter never sets `dispatchFailedReason`, so this banner
/// never appears on a card that is merely queued behind a full worker
/// pool (see `WorkTask.boardColumn`). Read-only — the underlying state
/// clears itself the next time a human retries the dispatch (drag to
/// Doing, or `bossctl work start`).

/// Kanban card body. Consumes a slim [[WorkCardSnapshot]] plus action
/// closures, and conforms to `Equatable` over the snapshot alone so
/// `.equatable()` at the call site can skip body evaluation when no
/// rendered input changed (design entry 5 — direct fix for the
/// `AG::LayoutDescriptor::compare` → `WorkTask.==` path). Closures are
/// intentionally outside the equatable surface: action-handler identity
/// must not force a re-layout. Board style lives on the snapshot (not
/// `@Environment`) so a `boss.kanban.boardStyle` flip participates in
/// `==` and re-styles already-mounted cards.
struct WorkBoardCardView: View, @MainActor Equatable {
    let snapshot: WorkCardSnapshot
    /// Invoked when the user taps the design-doc affordance. Only
    /// called when `snapshot.showsDesignDocAffordance` is true.
    var onOpenDesignDoc: (() -> Void)? = nil
    /// Called with `true` when the pointer enters a Dependency badge
    /// (the text badge or the chain link icon); `false` on exit.
    var onDepBadgeHover: ((Bool) -> Void)? = nil
    /// Called with `true` when the pointer enters the "In revision" badge;
    /// `false` on exit.
    var onRevisionBadgeHover: ((Bool) -> Void)? = nil
    /// Invoked when the user taps the terminal icon. `nil` hides the
    /// button (also gated by `snapshot.showsTerminalButton`).
    var onOpenTerminal: (() -> Void)? = nil
    /// Invoked after the user confirms "Merge When Ready". `nil` hides
    /// the button (also gated by `snapshot.showsMergeWhenReady`).
    var onMergeWhenReady: (() -> Void)? = nil
    /// Invoked with an attention item id when the popup's "Accept" button
    /// is tapped.
    var onAcceptDeferredScope: ((String) -> Void)? = nil
    /// Invoked with an attention item id when the popup's "Create task"
    /// button is tapped.
    var onCreateTaskFromDeferredScope: ((String) -> Void)? = nil

    @State private var isHovered: Bool = false
    @State private var showMergeConfirmation: Bool = false

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.snapshot == rhs.snapshot
    }

    var body: some View {
        // Fan-out regression counter (design entry 2): each body evaluation
        // is the fan-out signal Phase 1 work is meant to narrow. Side-effect
        // is intentional instrumentation (unfair-lock increment).
        let _ = UIUpdateCounters.shared.recordCardBodyEvaluation()
        let snap = snapshot
        VStack(alignment: .leading, spacing: 8) {
            if snap.kind == "revision", let seq = snap.revisionSeq {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    RevisionBadge(seq: seq)
                    if let origin = snap.engineRevisionOrigin {
                        EngineRevisionBadge(origin: origin)
                    }
                    if let parentID = snap.parentShortID {
                        Text("revises T" + String(parentID))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 0)
                }
            }
            HStack(alignment: .top, spacing: 6) {
                if let activityState = snap.activityState {
                    AgentActivityDot(state: activityState)
                        .padding(.top, 5)
                }
                if let slotId = snap.assignedSlotId,
                   let character = TrekCharacter.forSlot(slotId),
                   let nsImage = TrekIconAssets.image(character, size: .small) {
                    Image(nsImage: nsImage)
                        .resizable()
                        .interpolation(.high)
                        .aspectRatio(contentMode: .fit)
                        .frame(width: 20, height: 26)
                        .clipShape(RoundedRectangle(cornerRadius: 3, style: .continuous))
                        .help("\(character.displayName) (slot \(slotId))")
                }
                VStack(alignment: .leading, spacing: 2) {
                    HStack(alignment: .firstTextBaseline, spacing: 4) {
                        if snap.showsBlockedLock {
                            Image(systemName: "lock.fill")
                                .font(.caption)
                                .foregroundStyle(.orange)
                                .accessibilityLabel("Blocked")
                        }
                        Text(snap.name)
                            .font(.body.weight(.medium))
                            .foregroundStyle(.primary)
                            .multilineTextAlignment(.leading)
                            // Revision descriptions can be multi-paragraph; cap
                            // the card body to 2 lines so the card stays compact.
                            // The full text is accessible via the detail popover.
                            .lineLimit(snap.kind == "revision" ? 2 : nil)
                            .truncationMode(.tail)
                    }
                    if let blockedBy = snap.blockedBy, !blockedBy.isEmpty {
                        let prefix = snap.status == "blocked" ? "Blocked by" : "Waiting on:"
                        Text("\(prefix) \(blockedBy)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                            .help("\(prefix) \(blockedBy)")
                    }
                }
                // Pin the title column to the remaining lane width so the
                // title text wraps within the card instead of overflowing past
                // the right edge on long, low-break-opportunity names (#1172).
                .frame(maxWidth: .infinity, alignment: .leading)
            }

            // Free-form tags. Gated on the precomputed visibility flag so
            // zero-tag cards contribute zero height / zero gap.
            if snap.hasTagChips {
                let tagChips = WorkTagPresentation.chips(for: snap.tags)
                FlowLayout(horizontalSpacing: 4, verticalSpacing: 3) {
                    ForEach(tagChips.labels, id: \.self) { label in
                        WorkTagChip(text: label)
                    }
                    if let overflow = tagChips.overflow, overflow > 0 {
                        WorkTagChip(text: "+\(overflow)")
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: 36, alignment: .topLeading)
                .clipped()
                .accessibilityElement(children: .contain)
                .accessibilityLabel("Tags: \(tagChips.labels.joined(separator: ", "))")
            }

            if snap.hasLiveStatus, let liveStatus = snap.liveStatus {
                HStack(alignment: .firstTextBaseline, spacing: 4) {
                    WorkerWaitingIndicator(
                        activity: snap.liveStatusActivity,
                        lastEventAt: snap.liveStatusLastEventAt
                    )
                    Text(liveStatus)
                        .font(.caption)
                        .foregroundStyle(liveStatusColor)
                        .lineLimit(2)
                        .truncationMode(.tail)
                        .help(liveStatus)
                        .accessibilityLabel("Live status: \(liveStatus)")
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }

            // Wrap the whole metadata cluster so a full badge set flows onto
            // additional lines within the lane width instead of overflowing
            // past the card's right edge and clipping (#1172).
            FlowLayout(horizontalSpacing: 6, verticalSpacing: 4) {
                if snap.showsHighPriorityChip {
                    PriorityChip(priority: .high)
                }
                if snap.showsEffortChip, let effortLevel = snap.effortLevel {
                    EffortChip(effortLevel: effortLevel)
                }
                if snap.showsReasoningChip {
                    ReasoningChip()
                }
                if snap.showsDeferredBadge {
                    FutureScopeBadge()
                }
                if snap.showsProjectBadge, let projectName = snap.projectName {
                    WorkStatusBadge(text: projectName)
                }
                if snap.showsAIReviewingBadge {
                    ReviewingAIBadge()
                }
                if snap.showsResolvingConflictsBadge {
                    ResolvingConflictsBadge()
                } else if snap.showsResolvingCIBadge {
                    ResolvingCIFailureBadge()
                } else if let blockedText = snap.blockedBadgeText {
                    WorkStatusBadge(
                        text: blockedText,
                        tooltip: snap.blockedBadgeTooltip,
                        hasMoreInfo: snap.blockedBadgeHasMoreInfo
                    )
                    .onHover { hovering in
                        if snap.isDependencyBlockedBadge {
                            onDepBadgeHover?(hovering)
                        }
                    }
                }
                if snap.showsAutoBlockedChain {
                    Image(systemName: "link")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.orange)
                        .help(snap.autoBlockTooltip)
                        .accessibilityLabel("Auto-blocked by dependencies")
                        .accessibilityValue(snap.autoBlockTooltip)
                        .onHover { hovering in
                            onDepBadgeHover?(hovering)
                        }
                }
                if snap.conflictClearedBadgeVisible {
                    ConflictClearedBadge()
                }
                if snap.showsCIAutoFixedBadge {
                    CIAutoFixedBadge()
                }
                if snap.showsCIFailureChip, let ciFailureBadge = snap.ciFailureBadge {
                    CIFailureChip(badge: ciFailureBadge)
                }
                if snap.showsRepoChip, let repoChip = snap.repoChip {
                    RepoChipView(presentation: repoChip)
                }
                if snap.showsAutomationBadge {
                    Image(systemName: "wand.and.stars")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.purple)
                        .help("Created by automation")
                        .accessibilityLabel("Created by automation")
                }
                if snap.showsPlannerStagedBadge {
                    Image(systemName: "sparkle.magnifyingglass")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.indigo)
                        .help("Staged by the Planner — release the project to begin dispatch")
                        .accessibilityLabel("Staged by the Planner")
                }
                if snap.showsExternalRefLink, let extRef = snap.externalRefLink {
                    ExternalRefLinkView(presentation: extRef)
                }
                // Doc-link icon. Eligibility is already encoded in
                // `showsDesignDocAffordance` + `designDocState`.
                if snap.showsDesignDocAffordance,
                   let state = snap.designDocState,
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
                if snap.showsTerminalButton, let openTerminal = onOpenTerminal {
                    Button {
                        openTerminal()
                    } label: {
                        Image(systemName: "terminal")
                            .font(.caption)
                            .foregroundStyle(Color.secondary)
                            .accessibilityLabel(snap.terminalTooltip)
                    }
                    .buttonStyle(.plain)
                    .help(snap.terminalTooltip)
                }
                if snap.showsMergeWhenReady {
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
                if snap.showsDeferredScopeBadge {
                    DeferredScopeCardBadge(
                        items: snap.deferredScopeItems,
                        actionInFlightIDs: snap.deferredScopeActionInFlightIDs,
                        onAccept: { onAcceptDeferredScope?($0) },
                        onCreateTask: { onCreateTaskFromDeferredScope?($0) }
                    )
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            if snap.hasPRRow, let prURL = snap.prURL {
                HStack(alignment: .center, spacing: 6) {
                    if let mergeQueueState = snap.mergeQueueState {
                        MergeQueueBadge(
                            mergeQueueState: mergeQueueState,
                            detail: snap.mergeQueueDetail,
                            ciRequiredState: snap.ciRequiredState,
                            prMergeableState: snap.prMergeableState
                        )
                        .layoutPriority(-1)
                    } else if let ciState = snap.ciRequiredState {
                        PrCiIndicator(
                            state: ciState,
                            detail: snap.ciRequiredDetail,
                            prMergeableState: snap.prMergeableState
                        )
                    }
                    PRURLLink(
                        urlString: prURL,
                        font: .caption,
                        ambiguousRepoNames: snap.ambiguousRepoNames
                    )
                    .layoutPriority(1)
                    if snap.hasInProgressRevision {
                        PrInRevisionIndicator()
                            .onHover { hovering in
                                onRevisionBadgeHover?(hovering)
                            }
                    }
                    Spacer(minLength: 0)
                    if let id = snap.shortID {
                        Text("T" + String(id))
                            .font(.system(.caption2, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .accessibilityLabel("T" + String(id))
                            .lineLimit(1)
                            .fixedSize(horizontal: true, vertical: false)
                    }
                }
            }

            if snap.hasReviewRow, let reviewState = snap.reviewRequiredState {
                HStack(spacing: 6) {
                    PrReviewIndicator(state: reviewState, detail: snap.reviewRequiredDetail)
                    Spacer(minLength: 0)
                }
            }

            // Second PR row for a revision whose parent PR differs from
            // its own (avoids the #1829 double-link). Visibility is
            // precomputed on the snapshot.
            if snap.hasRevisionParentPRRow, let prURL = snap.revisionParentPrUrl {
                HStack(alignment: .center, spacing: 6) {
                    PRURLLink(
                        urlString: prURL,
                        font: .caption,
                        ambiguousRepoNames: snap.ambiguousRepoNames
                    )
                    Spacer(minLength: 0)
                }
            }

            if snap.hasStandaloneShortID, let id = snap.shortID {
                HStack {
                    Spacer(minLength: 0)
                    Text("T" + String(id))
                        .font(.system(.caption2, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .accessibilityLabel("T" + String(id))
                        .lineLimit(1)
                        .fixedSize(horizontal: true, vertical: false)
                }
            }

            if snap.hasInReviewRevisions {
                Divider()
                    .padding(.vertical, 2)
                ForEach(snap.inReviewRevisions) { revision in
                    RevisionRollupLine(revision: revision)
                }
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .fill(cardBackground)
                .brightness(isHovered && !snap.isSelected ? 0.04 : 0)
                .overlay(
                    RoundedRectangle(cornerRadius: 12, style: .continuous)
                        .strokeBorder(borderColor, lineWidth: snap.isSelected ? 2 : 1)
                )
        )
        .shadow(
            color: (snap.boardStyle == .airy || snap.boardStyle == .elevated) ? Color.black.opacity(0.07) : .clear,
            radius: 4, x: 0, y: 1.5
        )
        .draggable(snap.id)
        .onHover { hovering in
            withAnimation(.easeInOut(duration: 0.15)) {
                isHovered = hovering
            }
        }
    }

    /// Tint for the live-status subtitle row. Red for errored runs, a
    /// dimmer grey when the worker is idle, and the normal `.secondary`
    /// grey otherwise. The `waitingForInput` case is intentionally
    /// *not* tinted: it now carries its meaning via the explicit
    /// `WorkerWaitingIndicator` icon + tooltip instead of an ambiguous
    /// accent-blue subtitle (hue alone is an accessibility problem).
    private var liveStatusColor: Color {
        switch snapshot.liveStatusActivity {
        case .errored:
            return .red
        case .idle:
            return Color(nsColor: .tertiaryLabelColor)
        default:
            return .secondary
        }
    }

    private var cardBackground: Color {
        if snapshot.isSelected {
            return Color.accentColor.opacity(0.08)
        }
        if snapshot.isFrontierHighlighted {
            return Color.green.opacity(0.07)
        }
        if snapshot.showsBlockedChrome {
            return Color.orange.opacity(0.08)
        }
        // Future-scope items get a muted neutral fill so parked work reads as
        // "set aside" at a glance, distinct from genuinely-queued backlog
        // cards. Ranked below `blocked` so a deferred-and-gated card still
        // shows the blocked-orange chrome (the "Future" badge conveys the
        // classification either way).
        if snapshot.deferred {
            return Color.secondary.opacity(0.10)
        }
        switch snapshot.boardStyle {
        case .classic, .airy:
            return Color(nsColor: .windowBackgroundColor)
        case .elevated:
            // Distinct from the column's tinted panel (see `columnBackground`)
            // so card boundaries stay legible without relying on the drop
            // shadow alone — controlBackgroundColor renders as a visibly
            // lighter "elevated" surface against windowBackgroundColor in
            // dark mode, and a subtly different neutral in light mode.
            return Color(nsColor: .controlBackgroundColor)
        case .minimal:
            return Color(nsColor: .controlBackgroundColor)
        }
    }

    private var borderColor: Color {
        if snapshot.isSelected {
            return .accentColor
        }
        if snapshot.showsBlockedChrome {
            return .orange
        }
        // Soft muted outline reinforces the parked/future-scope treatment
        // established by `cardBackground` and the "Future" badge.
        if snapshot.deferred {
            return Color.secondary.opacity(0.45)
        }
        switch snapshot.boardStyle {
        case .classic:
            return Color(nsColor: .separatorColor)
        case .elevated:
            // A faint outline reinforces the card edge on top of the
            // background-color contrast, since some card kinds (revision
            // sub-rows, collapsed groups) are small enough that shadow alone
            // is easy to miss.
            return Color(nsColor: .separatorColor).opacity(0.5)
        case .airy, .minimal:
            return .clear
        }
    }
}

/// The `⟳ R<n>` chip rendered on revision cards in Backlog/Doing.
/// Uses the accent color so the chip reads as an affordance rather than
/// metadata text, and clearly signals "this is a revision" at a glance.
