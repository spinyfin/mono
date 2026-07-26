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

private func dispatchWaitReasonLabel(_ reason: String) -> String {
    switch reason {
    case "pool_exhausted":
        return "Waiting — worker pool full"
    case "pending_first_attempt":
        return "Waiting for a slot"
    default:
        return "Waiting — \(reason)"
    }
}

/// Wrapper for a single kanban card. Observes `LiveWorkerStateStore`
/// so live-state pushes invalidate the card without touching
/// `ContentView` or `ChatViewModel`. Doing-column cards re-resolve
/// their live state on every store publish; other columns ignore the
/// store entirely.

struct WorkBoardCardItem: View {
    let task: WorkTask
    let projectName: String?
    let column: WorkBoardColumnKey
    let runtime: WorkTaskRuntime?
    let isSelected: Bool
    var isRevealed: Bool = false
    /// True when this card is part of the actionable prerequisite
    /// frontier for a currently-hovered Dependency badge. Adds an
    /// amber border overlay so the reader can see "what needs to happen
    /// next" without opening the popover.
    var isFrontierHighlighted: Bool = false
    @ObservedObject var model: ChatViewModel
    @ObservedObject var liveStates: LiveWorkerStateStore
    @Environment(\.openWindow) private var openWindow
    /// Captured into the equatable snapshot so a board-style flip
    /// invalidates already-mounted card bodies (see `WorkCardSnapshot.boardStyle`).
    @Environment(\.kanbanBoardStyle) private var boardStyle
    @State private var showingDeleteConfirmation = false

    var body: some View {
        let liveState: WorkerLiveState? = {
            guard column == .doing,
                  let executionID = runtime?.executionID
            else { return nil }
            return liveStates.byRunID[executionID]
        }()

        // A dispatch-pending card has status=todo+autostart=true; it
        // landed in Doing because the engine intends to run it. This
        // covers two distinct waits — the row may not have an execution
        // yet (queued for scheduling; T2655 incident) or it may be `ready`
        // and genuinely waiting on pool capacity — see `liveStatusForCard`
        // below, which picks the subtitle apart by `runtime?.executionStatus`
        // instead of assuming capacity is always the cause.
        let isDispatchPending = task.status == "todo" && task.autostart

        // `dispatchRetryAt` is set only while the engine is withholding
        // this execution from dispatch because a *pre-spawn* attempt
        // already failed and is backing off before retrying — a
        // genuinely different wait than "no free slot" (T215 incident:
        // the card read "Waiting for a slot" while dispatch had actually
        // already failed and given up). Once the retry cap is exhausted
        // the engine clears `autostart` and stamps `dispatchFailedReason`
        // instead, which already renders its own failure banner outside
        // the Doing column — this is only the brief in-process backoff
        // window before that.
        let dispatchRetryAt: Date? = runtime?.dispatchRetryAt.flatMap(AutomationTime.parse)
        let isDispatchRetryPending = isDispatchPending && (dispatchRetryAt.map { $0 > Date() } ?? false)

        // A conflict-resolution card is status=blocked+merge_conflict with
        // an active resolution attempt. It routes to Doing for the duration
        // of the worker run; we surface a distinct "resolving conflicts"
        // indicator rather than the generic agent-activity dot.
        let isResolvingConflicts = column == .doing
            && task.status == "blocked"
            && task.blockedReason == "merge_conflict"

        // A CI-remediation card is status=blocked+ci_failure with an active
        // remediation attempt. Symmetric to the merge-conflict path above.
        let isRemediatingCI = column == .doing
            && task.status == "blocked"
            && task.blockedReason == "ci_failure"

        let isAIReviewing = column == .doing && task.aiReviewing && task.status == "active"

        let liveStatusForCard: String? = {
            guard column == .doing else { return nil }
            if isDispatchRetryPending, let dispatchRetryAtRaw = runtime?.dispatchRetryAt {
                return "Retrying dispatch — next attempt \(AutomationTime.relative(dispatchRetryAtRaw, now: Date()))"
            }
            // The dispatcher's real defer reason, when known — replaces the
            // generic "Waiting for a slot" so an operator isn't sent hunting
            // for free capacity when the actual cause is serialization or
            // gating (the T251 incident: `chain_serialized` read as slot
            // exhaustion for ~20 minutes with 8+ slots free).
            if isDispatchPending, let reason = runtime?.dispatchWaitReason {
                let label = dispatchWaitReasonLabel(reason)
                if let sinceRaw = runtime?.dispatchWaitSince {
                    return "\(label) (\(AutomationTime.relative(sinceRaw, now: Date())))"
                }
                return label
            }
            // No `dispatchWaitReason` means the scheduler hasn't stamped a
            // defer reason for this row — either because it hasn't reached
            // `ready` yet (no execution row at all, or still
            // `waiting_dependency`) or because it just became `ready` and
            // the scheduler hasn't evaluated it against the pool. Only the
            // latter is an actual capacity wait; genuine pool exhaustion
            // always gets stamped `pool_exhausted` (handled above) within
            // one scheduler pass. Claiming "Waiting for a slot" for the
            // former misdirects diagnosis toward pool capacity when the
            // pool had free workers the whole time (T2655 incident).
            if isDispatchPending {
                return runtime?.executionStatus == "ready" ? "Waiting for a slot" : "Queued"
            }
            if isResolvingConflicts { return nil }
            if isRemediatingCI { return nil }
            if isAIReviewing { return nil }
            // Transient-recovery banner wins outright — a worker being
            // auto-resumed after a Claude API error looks idle to every
            // other signal, so without this the card would silently show
            // stale/no text instead of "recovering from API error …".
            if let recovering = liveState?.recoveryStatus, !recovering.isEmpty {
                return recovering
            }
            return liveState?.liveStatus
        }()

        // Read precomputed prereq caches — O(1) per card instead of
        // scanning all dependency edges and tasks on every render pass.
        let cachedGating = model.gatingPrereqsByTaskID[task.id] ?? []
        let blockedBy: String? = {
            if task.status == "blocked" {
                let names = cachedGating.filter { $0.kind != .unknown }.map(\.title)
                return names.isEmpty ? nil : names.joined(separator: ", ")
            }
            if task.blockedReason == "dependency" {
                let rows = model.dependencyPrereqsByTaskID[task.id] ?? []
                guard !rows.isEmpty else { return nil }
                return rows.map(\.title).joined(separator: ", ")
            }
            return nil
        }()

        let gatingPrereqs = cachedGating
        let isAutoBlocked = task.status == "blocked"
            && task.lastStatusActor == "engine"
            && !cachedGating.isEmpty
        let dragRefusal: String? = (model.dragRefusalNotice?.taskID == task.id)
            ? model.dragRefusalNotice?.message
            : nil
        let mergeFeedback: String? = (model.mergeFeedbackNotice?.taskID == task.id)
            ? model.mergeFeedbackNotice?.message
            : nil
        let repoChip = model.repoChip(for: task)
        let designDocProject: WorkProject? = (task.kind == "design" || task.kind == "design_postmortem")
            ? task.projectID.flatMap { model.project(withID: $0) }
            : nil
        // Design and design-postmortem cards resolve their doc-link state
        // from the parent PROJECT; project-less docs-backed items
        // (investigations) carry an engine-resolved state on the task itself
        // (`docLinkState`). Prefer the project state when present, else fall
        // back to the per-task state so investigation cards render the same
        // Review-lane doc-link icon.
        let designDocState: ProjectDesignDocState? = designDocProject
            .map { model.designDocStateByProjectID[$0.id] ?? .notSet }
            ?? task.docLinkState
        let externalRefLink = ExternalRefLinkPresentation.forTask(task)
        // Roll-up rows must render wherever the parent's OWN card lands, not
        // just in Review/Done. A revision that reaches in_review/done never
        // gets a standalone card (see `workItems(in:)`'s rollup filter), so
        // gating this on `column` left revisions with no visual
        // representation at all whenever the parent's card landed somewhere
        // else — e.g. a parent blocked for a non-review reason renders in
        // Backlog (T2189/T2143: `reveal_work_item` had nothing to point at).
        let inReviewRevisions: [WorkTask] = (
            model.inReviewRevisions(forParentTaskID: task.id) + model.doneRevisions(forParentTaskID: task.id)
        ).sorted { ($0.revisionSeq ?? 0) < ($1.revisionSeq ?? 0) }
        let parentShortID: Int? = task.kind == "revision"
            ? task.parentTaskId.flatMap { model.workTask(withID: $0)?.shortID }
            : nil

        // Build the slim Equatable snapshot once per card-item evaluation.
        // Closures stay outside the snapshot so `.equatable()` on the card
        // can skip body re-evaluation when only action-handler identity
        // would differ (design entry 5 / `WorkCardSnapshot`).
        let onOpenTerminal: (() -> Void)? = {
            if (column == .review || column == .done),
               let prURL = task.prURL, !prURL.isEmpty {
                return { model.openReviewTerminal(for: task) }
            }
            if liveState != nil {
                return { model.openLiveWorkspaceTerminal(for: task) }
            }
            return nil
        }()
        let onMergeWhenReady: (() -> Void)? = (
            column == .review
                && task.status == "in_review"
                && task.prURL.map { !$0.isEmpty } == true
                && task.mergeQueueState == nil
        ) ? { model.mergeWhenReady(for: task) } : nil
        let terminalTooltip = (column == .review || column == .done)
            ? "Open terminal on PR branch"
            : "Open terminal in workspace"
        let deferredScopeItems = column == .review
            ? model.deferredScopeAttentions(forWorkItemID: task.id)
            : []
        let snapshot = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(
                column: column,
                projectName: projectName,
                isSelected: isSelected,
                runtime: runtime,
                liveState: liveState,
                liveStatus: liveStatusForCard,
                blockedBy: blockedBy,
                isAutoBlocked: isAutoBlocked,
                gatingPrereqs: gatingPrereqs,
                repoChip: repoChip,
                showsConflictClearedBadge: model.showsConflictClearedBadge(forPR: task.prURL),
                showsCIAutoFixedBadge: model.showsCIAutoFixedBadge(forPR: task.prURL),
                ciFailureBadge: model.ciFailureBadge(forPR: task.prURL),
                isFrontierHighlighted: isFrontierHighlighted,
                designDocState: designDocState,
                externalRefLink: externalRefLink,
                ambiguousRepoNames: model.ambiguousVisibleRepoNames,
                inReviewRevisions: inReviewRevisions.map(WorkCardRevisionRollup.init(revision:)),
                parentShortID: parentShortID,
                deferredScopeItems: deferredScopeItems,
                deferredScopeActionInFlightIDs: model.deferredScopeActionInFlightIDs,
                showsTerminalButton: onOpenTerminal != nil,
                terminalTooltip: terminalTooltip,
                showsMergeWhenReady: onMergeWhenReady != nil,
                boardStyle: boardStyle
            )
        )

        VStack(alignment: .leading, spacing: 6) {
            Button {
                model.selectWorkCard(isSelected ? nil : task.id)
            } label: {
                WorkBoardCardView(
                    snapshot: snapshot,
                    onOpenDesignDoc: designDocProject.map { proj in { model.openProjectDesignDoc(proj) } }
                        ?? (task.docLinkState != nil ? { model.openTaskDoc(task) } : nil),
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
                // (any of 77 `ChatViewModel` `@Published`s, live-state
                // ticks on sibling cards) rebuilds and re-lays-out every
                // card body — the `AG::LayoutDescriptor::compare` →
                // `WorkTask.==` hot path. `.equatable()` lets SwiftUI
                // skip body evaluation for cards whose snapshot is
                // unchanged. Closures are intentionally outside `==`.
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
                Divider()
                Button("Delete", role: .destructive) {
                    showingDeleteConfirmation = true
                }
            }
            .popover(
                isPresented: Binding(
                    get: { isSelected },
                    set: { isPresented in
                        if !isPresented, isSelected {
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

            if let dragRefusal {
                WorkDragRefusalBanner(message: dragRefusal) {
                    model.clearDragRefusal()
                }
            }

            if let mergeFeedback {
                WorkMergeFeedbackBanner(message: mergeFeedback) {
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
    //   column   — board column the card routes to ("review", "doing", …)
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
