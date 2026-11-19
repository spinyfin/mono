import AppKit
import os.log
import SwiftUI
import UpdateCore

// Debug logger for the work-item doc-link render path. Uses .debug() so
// it is silent in normal use; enable via Console.app subsystem filter or
// Xcode debug console. Surfaces work_item_id, kind, pr_url value, column,
// and whether PRURLLink will render — letting the user identify which
// of the three known gap sites (delivery, render, stale build) is live.
private let kanbanDocLinkLog = Logger(
    subsystem: "dev.spinyfin.bossmacapp",
    category: "kanban-doc-link"
)

/// Stable membership only. Each mounted card owns its observed render state.
struct WorkBoardSectionItemsView: View {
    let itemIDs: [String]
    let column: WorkBoardColumnKey
    let boardStyle: KanbanBoardStyle
    let model: ChatViewModel

    init(items: [WorkTask], column: WorkBoardColumnKey, boardStyle: KanbanBoardStyle,
         model: ChatViewModel) {
        itemIDs = items.map(\.id)
        self.column = column
        self.boardStyle = boardStyle
        self.model = model
    }

    /// Live reviewers and supervisors shared by PR identity: primary Review
    /// cards and held revisions in Doing both link to the same review workers.
    static func reviewWorkers(for task: WorkTask, index: [ReviewPRIdentity: [WorkerLiveState]]) -> [WorkerLiveState] {
        guard (task.kind == "revision" && task.aiReviewing) || (task.kind != "revision" && task.boardColumn == .review),
              let url = task.prURL ?? task.revisionParentPrUrl, !url.isEmpty
        else { return [] }
        return index[reviewerKey(url)] ?? []
    }

    static func reviewerKey(_ url: String) -> ReviewPRIdentity {
        ReviewPRIdentity(url)
    }

    static func reviewerIndex(states: [WorkerLiveState]) -> [ReviewPRIdentity: [WorkerLiveState]] {
        var index: [ReviewPRIdentity: [WorkerLiveState]] = [:]
        for state in states where state.executionKind == "pr_review" {
            guard let url = state.prURL, !url.isEmpty else { continue }
            index[reviewerKey(url), default: []].append(state)
        }
        for key in index.keys { index[key]?.sort { $0.slotId < $1.slotId } }
        return index
    }

    var body: some View {
        LazyVStack(alignment: .leading, spacing: 10) {
            ForEach(itemIDs, id: \.self) { id in
                WorkBoardObservedCard(id: id, column: column, style: boardStyle, model: model)
                    .equatable()
                    .id(id)
            }
        }
        .scrollTargetLayout()
    }
}

/// Wrapper for a single kanban card. Receives a pre-built
/// [[WorkCardSnapshot]] and resolved banner messages as **values** from
/// its row state — it does **not** observe `ChatViewModel` or
/// `LiveWorkerStateStore`. Holding `model` without `@ObservedObject`
/// keeps action dispatch (`selectWorkCard`, terminal, merge, delete)
/// without the "any of 77 `@Published` properties invalidates every
/// visible card" fan-out (design entry 6).
///
/// Selection popover content (`WorkCardPopoverView`) is attached only
/// while the card is selected so idle cards do not hold that tree
/// (design entry 10). Confirmation dialogs stay always-attached — they
/// are cheap, and mount-with-true is an intermittent presentation failure.
struct WorkBoardCardItem: View {
    let task: WorkTask
    /// Board column for action routing / debug logs only (value, not observed).
    let column: WorkBoardColumnKey
    let snapshot: WorkCardSnapshot
    var reviewers: [WorkerLiveState] = []
    /// Non-observing; action dispatch only. The row state owns
    /// observation and snapshot construction.
    let model: ChatViewModel
    /// The only observed hover state on a card. This keyed cell publishes
    /// only when this card enters or leaves the revision highlight set.
    @ObservedObject var revisionHighlight: WorkBoardRevisionHighlightState
    var isRevealed: Bool = false
    /// Pre-resolved by the row state from `dragRefusalNotice`.
    var dragRefusal: ChatViewModel.DragRefusalNotice? = nil
    /// Pre-resolved by the row state from `mergeFeedbackNotice`.
    var mergeFeedbackMessage: String? = nil
    var isRequestingMerge = false
    @Environment(\.openWindow) private var openWindow
    @State private var showingDeleteConfirmation = false

    var body: some View {
        let isFrontierHighlighted = snapshot.isFrontierHighlighted
            || revisionHighlight.isHighlighted
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
            ? { model.mergeWhenReady(for: task, origin: .board) }
            : nil
        let onOpenReviewGuide: (() -> Void)? = snapshot.reviewGuidePresentation?.showsDocumentButton == true
            ? { model.openReviewGuide(for: task) }
            : nil
        let onRetryReviewGuide: (() -> Void)? = snapshot.reviewGuidePresentation?.showsRetry == true
            ? { model.retryReviewGuide(for: task) }
            : nil
        let reviewGuideMenuPresentation = ReviewGuideCardPresentation.from(
            lifecycle: task.reviewGuideLifecycle,
            readableVersionId: task.reviewGuideReadableVersionId,
            staleSource: task.reviewGuideStaleSource ?? false,
            error: task.reviewGuideError
        )
        let onRevealAIReviewFindings: (() -> Void)? = (snapshot.aiReviewState == "reviewed_with_findings"
            || snapshot.aiReviewBadge?.findingsMarkdown != nil)
            ? {
                model.openAIReviewFindings(
                    revisionID: snapshot.aiReviewFindingsRevisionId,
                    findingsMarkdown: snapshot.aiReviewBadge?.findingsMarkdown)
            }
            : nil
        let onOpenDesignDoc: (() -> Void)? = {
            guard snapshot.showsDesignDocAffordance else { return nil }
            return { model.openWorkItemDoc(task) }
        }()
        let onOpenAttachments: (() -> Void)? = snapshot.showsAttachmentsAffordance
            ? { openWindow(id: "attachment-viewer", value: AttachmentViewerRef(taskId: task.id)) }
            : nil

        // Doing cards with a live run can jump to that run's Agents cell.
        let onFocusAgent: (() -> Void)? = {
            guard column == .doing,
                  let runId = model.taskRuntime(for: task.id)?.executionID
            else { return nil }
            return { model.focusAgentFromWorkCard(runId) }
        }()

        VStack(alignment: .leading, spacing: 6) {
            Button {
                model.selectWorkCard(snapshot.isSelected ? nil : task.id)
            } label: {
                WorkBoardCardView(
                    snapshot: snapshot,
                    onFocusAgent: onFocusAgent,
                    onRevealQueueBlocker: { blocker in
                        if case .unreachable(let reason) = model.revealWorkCard(blocker.workItemID, productID: blocker.productID) {
                            model.workErrorMessage = "Couldn't reveal \(blocker.label): \(reason)"
                        }
                    },
                    isRevisionHighlighted: revisionHighlight.isHighlighted,
                    onOpenDesignDoc: onOpenDesignDoc,
                    onDepBadgeHover: { hovering in
                        model.setDepBadgeHover(hovering ? task.id : nil)
                    },
                    onRevisionBadgeHover: { hovering in
                        model.setRevisionBadgeHover(hovering ? task.id : nil)
                    },
                    onRevisionBadgeTap: {
                        guard let revision = model.mostRecentActiveRevision(forParentID: task.id) else {
                            model.workErrorMessage = "Couldn't find the in-progress revision for \(task.name) — it may have just finished or been deleted."
                            return
                        }
                        switch model.revealWorkCard(revision.id, productID: revision.productID) {
                        case .revealed, .deferred:
                            break
                        case .unreachable(let reason):
                            model.workErrorMessage = "Couldn't reveal the in-progress revision for \(task.name): \(reason)"
                        }
                    },
                    onOpenTerminal: onOpenTerminal,
                    onMergeWhenReady: onMergeWhenReady,
                    isRequestingMerge: isRequestingMerge,
                    onOpenReviewGuide: onOpenReviewGuide,
                    onRetryReviewGuide: onRetryReviewGuide,
                    onOpenAttachments: onOpenAttachments,
                    onRevealAIReviewFindings: onRevealAIReviewFindings,
                    aiReviewFindingsTooltipNote: snapshot.aiReviewState == "reviewed_with_findings"
                        ? model.aiReviewFindingsTooltipNote(revisionID: snapshot.aiReviewFindingsRevisionId)
                        : nil,
                    onAcceptDeferredScope: { id in model.acceptDeferredScopeAttention(id: id) },
                    onCreateTaskFromDeferredScope: { id in
                        model.createTaskFromDeferredScopeAttention(attentionID: id)
                    },
                    onAnswerOperatorQuestion: { answer in
                        model.answerOperatorQuestion(for: task, answer: answer)
                    },
                    onDismissOperatorQuestion: { model.dismissOperatorQuestion(for: task) }
                )
                // `WorkBoardCardView` is `Equatable` over its snapshot and
                // keyed revision-hover boolean: without `.equatable()`, every
                // column render rebuilds every card body. Closures stay
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
                        Color.green.opacity(isFrontierHighlighted ? 0.7 : 0),
                        lineWidth: 2
                    )
                    .animation(.easeInOut(duration: 0.15), value: isFrontierHighlighted)
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
                if let title = task.generateReviewGuideMenuTitle {
                    if let failureTitle = reviewGuideMenuPresentation?.contextMenuFailureTitle {
                        Button(failureTitle) {}
                            .disabled(true)
                            .help(reviewGuideMenuPresentation?.tooltip ?? failureTitle)
                    }
                    Button(title) {
                        model.generateReviewGuide(for: task)
                    }
                    .help(reviewGuideMenuPresentation?.tooltip ?? title)
                }
                Divider()
                switch model.parkMenuAction(for: task) {
                case .unpark:
                    Button("Unpark") {
                        model.unpark(id: task.id)
                    }
                case .unparkDisabled(let reason):
                    Button("Unpark") {}
                        .disabled(true)
                        .help(reason)
                case .park:
                    Button("Park…") {
                        model.requestPark(task: task)
                    }
                case .none:
                    EmptyView()
                }
                Divider()
                Button("Delete", role: .destructive) {
                    showingDeleteConfirmation = true
                }
            }
            // Selection popover: attach only while selected so the ~700-line
            // `WorkCardPopoverView` tree is not in every idle card's graph.
            // isPresented reads live model selection (not a constant true) so
            // dismiss set(false) and subsequent get stay consistent.
            .lazyWorkCardSelectionPopover(
                isSelected: model.navigator.mode == .kanban && snapshot.isSelected,
                isPresented: Binding(
                    get: { model.isWorkCardPopoverPresented(task.id) },
                    set: { presented in
                        if !presented {
                            model.dismissWorkCardPopover()
                        }
                    }
                )
            ) {
                WorkCardPopoverView(model: model, task: task)
            }

            if let dispatchFailedReason = task.dispatchFailedReason {
                WorkDispatchFailureBanner(reason: dispatchFailedReason, errorText: task.dispatchFailedError)
            }

            // A dispatch failure already explains why autostart is off.
            if task.dispatchFailedReason == nil, let notice = task.autoStopNotice {
                WorkAutoStopBanner(notice: notice) { model.acknowledgeAutoStopNotice(for: task) }
            }

            if let dragRefusal {
                WorkDragRefusalBanner(
                    message: dragRefusal.message,
                    actionTitle: dragRefusal.suggestedAction == "add_revision" ? "Add revision…" : nil,
                    onAction: { model.performDragRefusalAction(dragRefusal) }
                ) {
                    model.clearDragRefusal()
                }
            }

            ForEach(reviewers, id: \.runId) { reviewer in
                ReviewWorkerLink(reviewer: reviewer, model: model)
            }

            if let mergeFeedbackMessage {
                WorkMergeFeedbackBanner(message: mergeFeedbackMessage) {
                    model.clearMergeFeedback()
                }
            }
        }
        .sheet(isPresented: Binding(
            get: { model.revisionComposerTaskID == task.id },
            set: { presented in
                if !presented, model.revisionComposerTaskID == task.id {
                    model.cancelRevisionComposer()
                }
            }
        )) {
            WorkRevisionComposer(
                taskName: task.name,
                onSubmit: { model.submitRevision(parentTaskID: task.id, description: $0) },
                onCancel: { model.cancelRevisionComposer() }
            )
        }
        .onAppear { logDocLinkState("appeared") }
        .onChange(of: task.prURL) { _, _ in logDocLinkState("prURL-changed") }
        // Always-attached: confirmationDialog needs false→true while
        // installed; mount-with-true is a known intermittent failure.
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

/// A live review worker on a revision card held in Doing for AI review.
struct ReviewWorkerLink: View {
    let reviewer: WorkerLiveState
    let model: ChatViewModel
    var title: String { "\(reviewer.displayName) — \(reviewer.badgeLabel)" }
    let tooltip = "Show live review worker"

    func activate() { model.focusAgentFromWorkCard(reviewer.runId) }

    var body: some View {
        Button(action: activate) {
            Label(title, systemImage: "person.crop.circle")
        }
        .buttonStyle(.plain)
        .font(.caption)
        .help(tooltip)
    }
}

/// Rendered directly on a kanban card whenever `task.dispatchFailedReason`
/// is set — the card has been bounced to Backlog with `autostart` cleared,
/// so it will NOT auto-retry. Distinguishes "held, needs you" from "waiting
/// for a slot": the latter never sets `dispatchFailedReason`, so this
/// banner never appears on a card that is merely queued behind a full
/// worker pool (see `WorkTask.boardColumn`). Two distinct conditions share
/// this field and this banner: the engine genuinely gave up starting the
/// row (a pre-spawn dispatch failure, or the churn guard), and a row whose
/// worker finished and deliberately parked itself for a human decision
/// (`deliberate_park` — not a failure at all). See
/// `WorkDispatchFailureBanner` for how the two are told apart in the
/// rendering.
///
/// Read-only, and deliberately so: whether the failure is still live is
/// the engine's call, not this view's. The engine clears
/// `dispatch_failed_reason` the moment any execution for the item
/// actually starts a run — the direct refutation of what the stamp
/// asserts — so this banner comes down on its own without the card ever
/// reasoning about lanes or elapsed time. See
/// `tools/boss/docs/attention-lifecycle.md`.

/// Kanban card body. Consumes a slim [[WorkCardSnapshot]] plus action
/// closures, and conforms to `Equatable` over the snapshot alone so
/// `.equatable()` at the call site can skip body evaluation when no
/// rendered input changed (design entry 5 — direct fix for the
/// `AG::LayoutDescriptor::compare` → `WorkTask.==` path). Closures are
/// intentionally outside the equatable surface: action-handler identity
/// must not force a re-layout. Board style lives on the snapshot (not
/// `@Environment`) so a `boss.kanban.boardStyle` flip participates in
/// `==` and re-styles already-mounted cards.
///
/// The body is a thin shell over five equatable subviews (design entry
/// 9) — revision header, title row, live-status row, badge strip, and
/// footer/PR row — each comparing its own snapshot slice so AttributeGraph
/// can skip subtrees whose inputs did not move. A live-status flip
/// re-lays-out the status row, not the whole card. Card chrome
/// (background / border / shadow / hover) stays here because it depends
/// on selection, blocked, deferred, and board-style fields that cross
/// every sub-region.
struct WorkBoardCardView: View, @MainActor Equatable {
    let snapshot: WorkCardSnapshot
    /// Shows the run in the Agents view; outside `==` like other handlers.
    var onFocusAgent: (() -> Void)? = nil
    var onRevealQueueBlocker: ((DispatchWaitBlocker) -> Void)? = nil
    /// Keyed revision-hover input, kept outside the snapshot so changing it
    /// does not require rebuilding every card snapshot in the column.
    var isRevisionHighlighted: Bool = false
    /// Invoked when the user taps the design-doc affordance. Only
    /// called when `snapshot.showsDesignDocAffordance` is true.
    var onOpenDesignDoc: (() -> Void)? = nil
    /// Called with `true` when the pointer enters a Dependency badge
    /// (the text badge or the chain link icon); `false` on exit.
    var onDepBadgeHover: ((Bool) -> Void)? = nil
    /// Called with `true` when the pointer enters the "In revision" badge;
    /// `false` on exit.
    var onRevisionBadgeHover: ((Bool) -> Void)? = nil
    /// Invoked when the user taps the "In revision" badge — reveals the
    /// revision task the badge is reporting on.
    var onRevisionBadgeTap: (() -> Void)? = nil
    /// Invoked when the user taps the terminal icon. `nil` hides the
    /// button (also gated by `snapshot.showsTerminalButton`).
    var onOpenTerminal: (() -> Void)? = nil
    /// Invoked after the user confirms "Merge When Ready". `nil` hides
    /// the button (also gated by `snapshot.showsMergeWhenReady`).
    var onMergeWhenReady: (() -> Void)? = nil
    var isRequestingMerge = false
    /// Invoked when the user taps the review-guide document button. `nil`
    /// hides it (also gated by `snapshot.reviewGuidePresentation`).
    var onOpenReviewGuide: (() -> Void)? = nil
    /// Invoked when the user taps the review-guide Retry affordance. `nil`
    /// hides it (also gated by `snapshot.reviewGuidePresentation`).
    var onRetryReviewGuide: (() -> Void)? = nil
    /// Invoked when the user taps the screenshots affordance. `nil` hides
    /// the button (also gated by `snapshot.showsAttachmentsAffordance`).
    var onOpenAttachments: (() -> Void)? = nil
    /// Invoked when the user taps the `reviewed_with_findings` AI-review
    /// badge — opens the revision's findings description and reveals the
    /// revision's card (or its parent's card once it is rolled up).
    /// Supplied for every findings badge; with no findings revision the
    /// click opens the task's PR instead and reveals nothing.
    var onRevealAIReviewFindings: (() -> Void)? = nil
    /// Tooltip sentence for the findings badge; see `AIReviewStateBadge`.
    var aiReviewFindingsTooltipNote: String? = nil
    /// Invoked with an attention item id when the popup's "Accept" button
    /// is tapped.
    var onAcceptDeferredScope: ((String) -> Void)? = nil
    /// Invoked with an attention item id when the popup's "Create task"
    /// button is tapped.
    var onCreateTaskFromDeferredScope: ((String) -> Void)? = nil
    /// Invoked with the user's Yes/No when they answer the inline
    /// question. Only called when `snapshot.operatorQuestion` is non-nil.
    var onAnswerOperatorQuestion: ((OperatorAnswer) -> Void)? = nil
    var onDismissOperatorQuestion: (() -> Void)? = nil

    @State private var isHovered: Bool = false

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.snapshot == rhs.snapshot
            && lhs.isRequestingMerge == rhs.isRequestingMerge
            && lhs.isRevisionHighlighted == rhs.isRevisionHighlighted
            && lhs.aiReviewFindingsTooltipNote == rhs.aiReviewFindingsTooltipNote
    }

    var body: some View {
        // Fan-out regression counter (design entry 2): each body evaluation
        // is the fan-out signal Phase 1 work is meant to narrow. Side-effect
        // is intentional instrumentation (unfair-lock increment).
        let _ = UIUpdateCounters.shared.recordCardBodyEvaluation()
        let snap = snapshot
        // Precompute so the empty-footer gate does not re-slice inside the
        // ViewBuilder, and so an empty footer never inserts a trailing
        // VStack spacing gap after the badge strip.
        let footerSlice = WorkBoardCardFooterSlice(snapshot: snap)
        VStack(alignment: .leading, spacing: 8) {
            if let revision = WorkBoardCardRevisionHeaderSlice(snapshot: snap) {
                WorkBoardCardRevisionHeader(slice: revision)
                    .equatable()
            }
            WorkBoardCardTitleRow(slice: WorkBoardCardTitleRowSlice(snapshot: snap), onFocusAgent: onFocusAgent)
                .equatable()
            if let liveStatus = WorkBoardCardLiveStatusRowSlice(snapshot: snap) {
                WorkBoardCardLiveStatusRow(slice: liveStatus, onRevealBlocker: onRevealQueueBlocker)
                    .equatable()
            }
            if let parked = snap.parked {
                WorkBoardCardParkedRow(presentation: parked)
                    .equatable()
            }
            if let operatorQuestion = WorkBoardCardOperatorQuestionSlice(snapshot: snap) {
                WorkBoardCardOperatorQuestion(slice: operatorQuestion, onAnswer: onAnswerOperatorQuestion, onDismiss: onDismissOperatorQuestion)
                    .equatable()
            }
            WorkBoardCardBadgeStrip(
                slice: WorkBoardCardBadgeStripSlice(snapshot: snap),
                onOpenDesignDoc: onOpenDesignDoc,
                onOpenAttachments: onOpenAttachments,
                onDepBadgeHover: onDepBadgeHover,
                onOpenTerminal: onOpenTerminal,
                onMergeWhenReady: onMergeWhenReady,
                isRequestingMerge: isRequestingMerge,
                onOpenReviewGuide: onOpenReviewGuide,
                onRetryReviewGuide: onRetryReviewGuide,
                onRevealAIReviewFindings: onRevealAIReviewFindings,
                aiReviewFindingsTooltipNote: aiReviewFindingsTooltipNote,
                onAcceptDeferredScope: onAcceptDeferredScope,
                onCreateTaskFromDeferredScope: onCreateTaskFromDeferredScope
            )
            .equatable()
            if !footerSlice.isEmpty {
                WorkBoardCardFooter(
                    slice: footerSlice,
                    onRevisionBadgeHover: onRevisionBadgeHover,
                    onRevisionBadgeTap: onRevisionBadgeTap
                )
                .equatable()
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .fill(cardBackground)
                .brightness(isHovered && !snap.isSelected ? 0.04 : 0)
                // Scoped to `isHovered` and to the background shape, and
                // both halves of that matter — see `onHover` below.
                .animation(.easeInOut(duration: 0.15), value: isHovered)
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
        // Plain assignment, NOT `withAnimation`. `withAnimation` sets
        // `Transaction.animation` for the entire update, so it animates
        // every animatable attribute that moves in that turn — including
        // view *frames* (`AnimatableFrameAttribute`), not just this
        // card's brightness. Hover delivery runs at the *end* of a graph
        // update (`Update.dispatchActions()` →
        // `enqueueHoverUpdateIfNeeded()`), so the transaction it joins is
        // already carrying whatever layout changed that turn. Animated
        // frames then slide cards under a stationary pointer, which
        // produces the next hover transition, which opens the next global
        // animated transaction; `AnimatorState.nextUpdate()` re-arms a
        // frame every tick, so `GraphHost.flushTransactions()` always has
        // another transaction queued and the run loop never gets a quiet
        // turn. That is the 100%-CPU work-board beachball — a livelock,
        // not a deadlock. See
        // `tools/boss/docs/investigations/work-board-layout-livelock-2026-08-07.md`.
        //
        // The `.animation(_:value:)` above gives the same visual fade
        // while confining it to a property that cannot move a frame.
        .onHover { hovering in
            isHovered = hovering
        }
    }

    private var cardBackground: Color {
        if snapshot.isSelected {
            return Color.accentColor.opacity(0.08)
        }
        if snapshot.isFrontierHighlighted || isRevisionHighlighted {
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

// MARK: - Lazy secondary UI (design entry 10)

extension View {
    /// Attaches a trailing selection popover only while `isSelected` is
    /// true so idle cards do not hold `WorkCardPopoverView`.
    ///
    /// `isPresented` must read **live** selection (e.g. model
    /// `selectedTask?.id == task.id`), not a constant `true`. Constant
    /// get fights SwiftUI after dismiss (`set(false)` then `get` still
    /// true). User dismiss → `set(false)` clears selection → get is
    /// false → next body pass drops this modifier. Selecting another
    /// card updates the model first; this card rebuilds with
    /// `isSelected == false` and removes the presenter after selection
    /// has already moved (reselect mounts cleanly on the new card with
    /// a live true binding).
    @ViewBuilder
    func lazyWorkCardSelectionPopover<Content: View>(
        isSelected: Bool,
        isPresented: Binding<Bool>,
        @ViewBuilder content: @escaping () -> Content
    ) -> some View {
        if isSelected {
            self.popover(
                isPresented: isPresented,
                arrowEdge: .trailing,
                content: content
            )
        } else {
            self
        }
    }
}
