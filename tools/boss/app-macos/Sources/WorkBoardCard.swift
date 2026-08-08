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

/// Column-local card list. Observes `ChatViewModel` so model publishes
/// (selection, highlights, drag/merge notices, `taskRuntimesByID`, prereq
/// caches, …) recompute per-card `WorkCardSnapshot`s once per column
/// (design entry 6). Also observes `LiveWorkerStateStore` so Doing-lane
/// live-status ticks rebuild snapshots without every card subscribing.
/// Cards stay non-observing and receive snapshots as values —
/// `.equatable()` skips bodies whose snapshot did not change.
struct WorkBoardSectionItemsView: View {
    let items: [WorkTask]
    let column: WorkBoardColumnKey
    let boardStyle: KanbanBoardStyle
    /// Observed here so snapshot construction re-runs when model state
    /// that feeds cards changes (including pure `taskRuntimesByID`
    /// updates for Doing dispatch/slot status). Cards hold a
    /// non-observing reference for action dispatch only.
    @ObservedObject var model: ChatViewModel
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
        let onRevealAIReviewFindings: (() -> Void)? = snapshot.aiReviewFindingsRevisionId.map { revisionID in
            {
                switch model.revealWorkCard(revisionID, productID: task.productID) {
                case .revealed, .deferred:
                    break
                case .unreachable(let reason):
                    model.workErrorMessage = "Couldn't reveal the review findings for \(task.name): \(reason)"
                }
            }
        }
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
                    onRevealAIReviewFindings: onRevealAIReviewFindings,
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
            // Selection popover: attach only while selected so the ~700-line
            // `WorkCardPopoverView` tree is not in every idle card's graph.
            // isPresented reads live model selection (not a constant true) so
            // dismiss set(false) and subsequent get stay consistent.
            .lazyWorkCardSelectionPopover(
                isSelected: snapshot.isSelected,
                isPresented: Binding(
                    get: { model.selectedTask?.id == task.id },
                    set: { presented in
                        if !presented {
                            model.selectWorkCard(nil)
                        }
                    }
                )
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

/// Rendered directly on a kanban card whenever the engine gave up
/// starting it (`task.dispatchFailedReason` set) — the card has been
/// bounced to Backlog with `autostart` cleared, so it will NOT
/// auto-retry. Distinguishes "failing to start" from "waiting for a
/// slot": the latter never sets `dispatchFailedReason`, so this banner
/// never appears on a card that is merely queued behind a full worker
/// pool (see `WorkTask.boardColumn`).
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
    /// Invoked when the user taps the `reviewed_with_findings` AI-review
    /// badge — reveals the follow-up revision carrying the review
    /// comments. Only called when `snapshot.aiReviewFindingsRevisionId`
    /// is non-nil.
    var onRevealAIReviewFindings: (() -> Void)? = nil
    /// Invoked with an attention item id when the popup's "Accept" button
    /// is tapped.
    var onAcceptDeferredScope: ((String) -> Void)? = nil
    /// Invoked with an attention item id when the popup's "Create task"
    /// button is tapped.
    var onCreateTaskFromDeferredScope: ((String) -> Void)? = nil

    @State private var isHovered: Bool = false

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.snapshot == rhs.snapshot
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
            WorkBoardCardTitleRow(slice: WorkBoardCardTitleRowSlice(snapshot: snap))
                .equatable()
            if let liveStatus = WorkBoardCardLiveStatusRowSlice(snapshot: snap) {
                WorkBoardCardLiveStatusRow(slice: liveStatus)
                    .equatable()
            }
            WorkBoardCardBadgeStrip(
                slice: WorkBoardCardBadgeStripSlice(snapshot: snap),
                onOpenDesignDoc: onOpenDesignDoc,
                onDepBadgeHover: onDepBadgeHover,
                onOpenTerminal: onOpenTerminal,
                onMergeWhenReady: onMergeWhenReady,
                onRevealAIReviewFindings: onRevealAIReviewFindings,
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
