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

        let activityState: AgentActivityState? = column == .doing
            ? .forDoingCard(
                runtime: runtime,
                liveState: liveState,
                isDispatchPending: isDispatchPending,
                isResolvingConflicts: isResolvingConflicts,
                isRemediatingCI: isRemediatingCI,
                isAIReviewing: isAIReviewing)
            : nil

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

        VStack(alignment: .leading, spacing: 6) {
            Button {
                model.selectWorkCard(isSelected ? nil : task.id)
            } label: {
                WorkBoardCardView(
                    task: task,
                    projectName: projectName,
                    isSelected: isSelected,
                    activityState: activityState,
                    assignedSlotId: column == .doing ? liveState?.slotId : nil,
                    liveStatus: liveStatusForCard,
                    liveStatusActivity: isDispatchPending ? nil : (column == .doing ? liveState?.activity : nil),
                    liveStatusLastEventAt: isDispatchPending ? nil : (column == .doing ? liveState?.lastEventAt : nil),
                    blockedBy: blockedBy,
                    isAutoBlocked: isAutoBlocked,
                    gatingPrereqs: gatingPrereqs,
                    repoChip: repoChip,
                    showsConflictClearedBadge: model.showsConflictClearedBadge(forPR: task.prURL),
                    showsCIAutoFixedBadge: model.showsCIAutoFixedBadge(forPR: task.prURL),
                    ciFailureBadge: model.ciFailureBadge(forPR: task.prURL),
                    isResolvingConflicts: isResolvingConflicts,
                    isRemediatingCI: isRemediatingCI,
                    isFrontierHighlighted: isFrontierHighlighted,
                    designDocState: designDocState,
                    onOpenDesignDoc: designDocProject.map { proj in { model.openProjectDesignDoc(proj) } }
                        ?? (task.docLinkState != nil ? { model.openTaskDoc(task) } : nil),
                    ciRequiredState: (column == .review || task.isInMergingSection)
                        ? (task.ciRequiredState ?? "in_progress")
                        : nil,
                    ciRequiredDetail: (column == .review || task.isInMergingSection) ? task.ciRequiredDetail : nil,
                    reviewRequiredState: column == .review ? task.reviewRequiredState : nil,
                    reviewRequiredDetail: column == .review ? task.reviewRequiredDetail : nil,
                    mergeQueueState: task.isInMergingSection ? task.mergeQueueState : nil,
                    mergeQueueDetail: task.isInMergingSection ? task.mergeQueueDetail : nil,
                    externalRefLink: externalRefLink,
                    ambiguousRepoNames: model.ambiguousVisibleRepoNames,
                    inReviewRevisions: inReviewRevisions,
                    parentShortID: parentShortID,
                    onDepBadgeHover: { hovering in
                        model.setDepBadgeHover(hovering ? task.id : nil)
                    },
                    onRevisionBadgeHover: { hovering in
                        model.setRevisionBadgeHover(hovering ? task.id : nil)
                    },
                    onOpenTerminal: ((column == .review || column == .done) && task.prURL != nil && !(task.prURL?.isEmpty ?? true))
                        ? { model.openReviewTerminal(for: task) }
                        : (liveState != nil)
                            ? { model.openLiveWorkspaceTerminal(for: task) }
                            : nil,
                    terminalTooltip: (column == .review || column == .done)
                        ? "Open terminal on PR branch"
                        : "Open terminal in workspace",
                    onMergeWhenReady: (column == .review &&
                                       task.status == "in_review" &&
                                       task.prURL != nil &&
                                       !(task.prURL?.isEmpty ?? true) &&
                                       task.mergeQueueState == nil)
                        ? { model.mergeWhenReady(for: task) }
                        : nil,
                    deferredScopeItems: column == .review ? model.deferredScopeAttentions(forWorkItemID: task.id) : [],
                    onAcceptDeferredScope: { id in model.acceptDeferredScopeAttention(id: id) },
                    onCreateTaskFromDeferredScope: { id in model.createTaskFromDeferredScopeAttention(attentionID: id) }
                )
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

struct FlowLayout: Layout {
    /// Horizontal gap between chips on the same line.
    var horizontalSpacing: CGFloat = 6
    /// Vertical gap between wrapped lines.
    var verticalSpacing: CGFloat = 3

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout Void) -> CGSize {
        let maxWidth = proposal.width ?? .infinity
        let rows = computeRows(maxWidth: maxWidth, subviews: subviews)
        let width = rows.map(\.width).max() ?? 0
        let height = rows.reduce(0) { $0 + $1.height }
            + verticalSpacing * CGFloat(max(0, rows.count - 1))
        return CGSize(width: min(width, maxWidth), height: height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout Void) {
        let rows = computeRows(maxWidth: bounds.width, subviews: subviews)
        var y = bounds.minY
        for row in rows {
            var x = bounds.minX
            for index in row.indices {
                let size = subviews[index].sizeThatFits(.unspecified)
                subviews[index].place(
                    at: CGPoint(x: x, y: y),
                    anchor: .topLeading,
                    proposal: ProposedViewSize(size)
                )
                x += size.width + horizontalSpacing
            }
            y += row.height + verticalSpacing
        }
    }

    private struct Row {
        var indices: [Int] = []
        var width: CGFloat = 0
        var height: CGFloat = 0
    }

    private func computeRows(maxWidth: CGFloat, subviews: Subviews) -> [Row] {
        var rows: [Row] = []
        var current = Row()
        for index in subviews.indices {
            let size = subviews[index].sizeThatFits(.unspecified)
            let lead = current.indices.isEmpty ? 0 : horizontalSpacing
            // Wrap when the chip would overflow — but never strand a chip on an
            // empty line, so the first chip of a row always fits even if it is
            // itself wider than the lane (it will clip, but that is degenerate).
            if !current.indices.isEmpty && current.width + lead + size.width > maxWidth {
                rows.append(current)
                current = Row()
            }
            let gap = current.indices.isEmpty ? 0 : horizontalSpacing
            current.width += gap + size.width
            current.height = max(current.height, size.height)
            current.indices.append(index)
        }
        if !current.indices.isEmpty {
            rows.append(current)
        }
        return rows
    }
}

struct WorkBoardCardView: View {
    let task: WorkTask
    let projectName: String?
    let isSelected: Bool
    let activityState: AgentActivityState?
    /// Slot id of the worker currently bound to this card, when the
    /// card lives in the Doing lane. Drives the small crew portrait
    /// in the title row so a glance at the board tells you which
    /// crew member is on which task.
    let assignedSlotId: Int?
    /// Free-text one-sentence "what is the worker doing right now"
    /// fed by the engine's live-status summarizer. Rendered as a
    /// subtitle row between the title row and the footer when the
    /// card is in the Doing lane and the string is non-empty.
    /// `nil` collapses the row entirely so idle/blank states don't
    /// leave awkward whitespace.
    var liveStatus: String? = nil
    /// Activity of the worker behind `liveStatus`. `WaitingForInput`
    /// now surfaces a `WorkerWaitingIndicator` icon next to the
    /// subtitle (rather than tinting the text accent-blue, which was
    /// ambiguous and an accessibility problem); `Errored` reads in
    /// red, `Idle` dims further than `.secondary`. The default `nil`
    /// is treated as the plain `.secondary` colour.
    var liveStatusActivity: WorkerActivity? = nil
    /// ISO-8601 `last_event_at` of the worker behind `liveStatus`,
    /// passed straight through from `LiveWorkerState`. Feeds the
    /// "No response for …" elapsed time in the waiting indicator's
    /// tooltip. `nil` when there is no live worker or no event yet.
    var liveStatusLastEventAt: String? = nil
    /// Comma-joined names of the prereqs currently gating this card.
    /// Non-nil only on `blocked` rows — the kanban surfaces these in
    /// the Backlog column with a lock + "Blocked by …" subtitle so the
    /// reader can tell at a glance which Backlog items are gated and
    /// by what.
    let blockedBy: String?
    /// True when the row is engine-blocked (auto-block) rather than a
    /// human choice. Drives the chain badge in the footer per design
    /// Q7 — manual blocks already get the lane and would double up.
    var isAutoBlocked: Bool = false
    /// Resolved prereq rows used by the chain badge's hover tooltip.
    /// Empty for cards that aren't gated; populated regardless of
    /// `isAutoBlocked` because the popover Dependencies subsection
    /// reuses this list to render hyperlinks.
    var gatingPrereqs: [WorkDependencyRow] = []
    /// Per-card repo chip presentation, populated only when the
    /// kanban is in multi-repo mode (any card override or mixed
    /// resolved URLs across the visible board). `nil` in single-repo
    /// mode, where the chip lives on the product header instead — see
    /// `WorkBoardRepoMode` for the mode rule.
    var repoChip: RepoChipPresentation? = nil
    /// True when this card's PR was the target of a successful
    /// conflict-resolution attempt inside the freshness window
    /// (Phase 5 #15 of the merge-conflict design). Renders the
    /// `"🔧 conflict cleared"` chip in the footer; ages out after 24h
    /// via [[ChatViewModel.showsConflictClearedBadge(forPR:)]].
    var showsConflictClearedBadge: Bool = false
    /// True when this card's PR has a successful CI auto-fix inside
    /// the 24h freshness window. Renders the `"✅ ci auto-fixed"` chip
    /// per design Q11 / Phase 11 #37.
    var showsCIAutoFixedBadge: Bool = false
    /// In-flight / exhausted CI-failure chip for the PR, or `nil` when
    /// no CI auto-fix is currently tracked. Renders `🟧 ci failing
    /// (used/budget)` or `🛑 ci failing (exhausted)` per design Q11.
    var ciFailureBadge: CiFailureBadge? = nil
    /// True when this card is in the Doing column because a merge-
    /// resolution worker is actively running against it. Suppresses the
    /// blocked-row orange chrome and renders the `"resolving conflicts"`
    /// indicator instead so the user can tell at a glance what the
    /// active work is.
    var isResolvingConflicts: Bool = false
    /// True when this card is in the Doing column because a CI-remediation
    /// worker is actively running against it. Symmetric to
    /// [[isResolvingConflicts]]; suppresses orange chrome and renders the
    /// `"resolving CI failure"` badge instead.
    var isRemediatingCI: Bool = false
    /// True when this card is a prerequisite frontier card for a
    /// currently-hovered Dependency badge. Drives the green card background.
    var isFrontierHighlighted: Bool = false
    /// Resolved design-doc state for the parent project. Non-nil only
    /// for `kind=design` tasks whose parent project has populated
    /// `design_doc_*` columns. `nil` hides the affordance entirely.
    var designDocState: ProjectDesignDocState? = nil
    /// Invoked when the user taps the design-doc affordance. Only
    /// called when `designDocState` is non-nil and produces a
    /// non-nil `ProjectDesignDocAffordancePresentation`.
    var onOpenDesignDoc: (() -> Void)? = nil
    /// Aggregate required-CI state for the PR indicator. Mirrors
    /// `WorkTask.ciRequiredState`; supplied by the parent only when the
    /// card is in the Review lane and `task.prURL` is non-nil.
    var ciRequiredState: String? = nil
    /// JSON-encoded failing check detail for the CI tooltip.
    var ciRequiredDetail: String? = nil
    /// Required-review state for the review indicator. Mirrors
    /// `WorkTask.reviewRequiredState`; supplied by the parent under the
    /// same conditions as `ciRequiredState`.
    var reviewRequiredState: String? = nil
    /// JSON-encoded reviewer list for the review tooltip.
    var reviewRequiredDetail: String? = nil
    /// Merge-queue / auto-merge state for the Merging-section badge.
    /// `"queued"` or `"auto_merge_enabled"` when the card is in the
    /// kanban's "Merging" section; `nil` otherwise (including for a
    /// Review-lane card, which is never in that section — see
    /// `WorkTask.isInMergingSection`). When set, replaces the CI indicator
    /// with `MergeQueueBadge`.
    var mergeQueueState: String? = nil
    /// JSON-encoded merge-queue sub-state (`{"position", "state",
    /// "enqueued_at", "section_order"}`), mirrors `WorkTask.mergeQueueDetail`.
    /// `nil` unless `mergeQueueState` is non-nil. Parsed by `MergeQueueBadge`
    /// to render the queue position and readiness icon.
    var mergeQueueDetail: String? = nil
    /// Upstream-link affordance derived from `task.externalRef`. `nil`
    /// when the task has no external binding — the affordance is hidden
    /// entirely in that state. Bound refs show an accent-colored `↗ #N`
    /// link; stale refs (binding cleared upstream) show it dimmed with a
    /// strikethrough so the history is preserved but the staleness is
    /// communicated at a glance.
    var externalRefLink: ExternalRefLinkPresentation? = nil
    /// Repo names whose bare-`repo#n` PR label would be ambiguous on
    /// the current board — see
    /// [[ChatViewModel.ambiguousVisibleRepoNames]]. Threaded into
    /// `PRURLLink` so a card's PR link can drop the org prefix when
    /// its repo is unique among visible cards.
    var ambiguousRepoNames: Set<String> = []
    /// Revisions to display as rollup lines on this card's footer. Populated
    /// in the Review lane (in-review revisions) and the Done lane (done
    /// revisions). Empty for Backlog/Doing cards and parent tasks with no
    /// nested revisions. Ordered by `revisionSeq`.
    var inReviewRevisions: [WorkTask] = []
    /// Short ID of the parent task, used to render "revises T<n>" on
    /// revision cards in Backlog/Doing. `nil` for non-revision tasks
    /// and revision tasks whose parent can't be resolved.
    var parentShortID: Int? = nil
    /// Called with `true` when the pointer enters a Dependency badge
    /// (the text badge or the chain link icon); `false` on exit.
    /// `nil` when the card doesn't need to report badge hover (e.g.
    /// in the Designs viewer).
    var onDepBadgeHover: ((Bool) -> Void)? = nil
    /// Called with `true` when the pointer enters the "In revision" badge;
    /// `false` on exit. Same hover-highlight protocol as `onDepBadgeHover`.
    var onRevisionBadgeHover: ((Bool) -> Void)? = nil
    /// Invoked when the user taps the terminal icon. `nil` hides the
    /// button. Callers pass a closure either for a Review/Done-column
    /// card with a PR URL (opens a fresh workspace on the PR branch) or
    /// for a Doing-column card whose work item has a live execution
    /// (opens a terminal in the execution's already-leased workspace) —
    /// gated on data availability, not the column itself.
    var onOpenTerminal: (() -> Void)? = nil
    /// Tooltip/accessibility text for the terminal button; varies by
    /// which of the two flows above `onOpenTerminal` was wired to.
    var terminalTooltip: String = "Open terminal on PR branch"
    /// Invoked after the user confirms the "Merge When Ready" button on a
    /// Review-column card. `nil` hides the button — callers only pass a
    /// closure when the card is in the Review lane, has a PR URL, and Merge
    /// When Ready hasn't already been requested (`mergeQueueState == nil`).
    /// Once requested, the card leaves Review for the Done column's
    /// "Merging" section (see `WorkTask.isInMergingSection`), so the button
    /// naturally disappears with it.
    var onMergeWhenReady: (() -> Void)? = nil
    /// Open `deferred_scope` attention items filed against this work item.
    /// Empty hides the badge entirely — callers only populate this for
    /// Review-lane cards (mirrors `ciRequiredState`'s column gate above).
    var deferredScopeItems: [DeferredScopeAttention] = []
    /// Invoked with an attention item id when the popup's "Accept" button
    /// is tapped. `nil` when `deferredScopeItems` is always empty for this
    /// card kind.
    var onAcceptDeferredScope: ((String) -> Void)? = nil
    /// Invoked with an attention item id when the popup's "Create task"
    /// button is tapped.
    var onCreateTaskFromDeferredScope: ((String) -> Void)? = nil

    @Environment(\.kanbanBoardStyle) private var boardStyle

    @State private var isHovered: Bool = false
    @State private var showMergeConfirmation: Bool = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if task.kind == "revision", let seq = task.revisionSeq {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    RevisionBadge(seq: seq)
                    if let origin = EngineRevisionOrigin(createdVia: task.createdVia) {
                        EngineRevisionBadge(origin: origin)
                    }
                    if let parentID = parentShortID {
                        Text("revises T" + String(parentID))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 0)
                }
            }
            HStack(alignment: .top, spacing: 6) {
                if let activityState {
                    AgentActivityDot(state: activityState)
                        .padding(.top, 5)
                }
                if let slotId = assignedSlotId,
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
                        if task.status == "blocked" && !isResolvingConflicts && !isRemediatingCI {
                            Image(systemName: "lock.fill")
                                .font(.caption)
                                .foregroundStyle(.orange)
                                .accessibilityLabel("Blocked")
                        }
                        Text(task.name)
                            .font(.body.weight(.medium))
                            .foregroundStyle(.primary)
                            .multilineTextAlignment(.leading)
                            // Revision descriptions can be multi-paragraph; cap
                            // the card body to 2 lines so the card stays compact.
                            // The full text is accessible via the detail popover.
                            .lineLimit(task.kind == "revision" ? 2 : nil)
                            .truncationMode(.tail)
                    }
                    if let blockedBy, !blockedBy.isEmpty {
                        let prefix = task.status == "blocked" ? "Blocked by" : "Waiting on:"
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

            if let liveStatus, !liveStatus.isEmpty {
                HStack(alignment: .firstTextBaseline, spacing: 4) {
                    WorkerWaitingIndicator(
                        activity: liveStatusActivity,
                        lastEventAt: liveStatusLastEventAt
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

            if hasFooterContent {
                // Wrap the whole metadata cluster so a full badge set — effort,
                // CI status, repo, work-item id, and the trailing action chips —
                // flows onto additional lines within the lane width instead of
                // overflowing past the card's right edge and clipping (#1172).
                FlowLayout(horizontalSpacing: 6, verticalSpacing: 4) {
                    let parsedPriority = WorkPriority.parse(task.priority)
                    if parsedPriority == .high {
                        PriorityChip(priority: parsedPriority)
                    }
                    if let effortLevel = task.effortLevel,
                       !effortLevel.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                        EffortChip(effortLevel: effortLevel)
                    }
                    if task.deferred {
                        FutureScopeBadge()
                    }
                    if let projectName, !projectName.isEmpty {
                        WorkStatusBadge(text: projectName)
                    }
                    if task.aiReviewing && task.status == "active" {
                        ReviewingAIBadge()
                    }
                    if isResolvingConflicts {
                        ResolvingConflictsBadge()
                    } else if isRemediatingCI {
                        ResolvingCIFailureBadge()
                    } else if let blockedText = WorkBlockedBadge.badgeText(for: task) {
                        let isDependencyBadge = blockedText == WorkBlockedBadge.label(forReason: "dependency")
                        WorkStatusBadge(
                            text: blockedText,
                            tooltip: WorkBlockedBadge.badgeTooltip(for: task),
                            hasMoreInfo: WorkBlockedBadge.hasMoreInfo(for: task)
                        )
                            .onHover { hovering in
                                if isDependencyBadge {
                                    onDepBadgeHover?(hovering)
                                }
                            }
                    }
                    if isAutoBlocked {
                        Image(systemName: "link")
                            .font(.caption2.weight(.semibold))
                            .foregroundStyle(.orange)
                            .help(autoBlockTooltip)
                            .accessibilityLabel("Auto-blocked by dependencies")
                            .accessibilityValue(autoBlockTooltip)
                            .onHover { hovering in
                                onDepBadgeHover?(hovering)
                            }
                    }
                    if conflictClearedBadgeVisible {
                        ConflictClearedBadge()
                    }
                    if showsCIAutoFixedBadge && ciFailureBadge == nil {
                        CIAutoFixedBadge()
                    }
                    if let ciFailureBadge, !isRemediatingCI {
                        CIFailureChip(badge: ciFailureBadge)
                    }
                    if let repoChip {
                        RepoChipView(presentation: repoChip)
                    }
                    if task.sourceAutomationId != nil {
                        Image(systemName: "wand.and.stars")
                            .font(.caption2.weight(.semibold))
                            .foregroundStyle(.purple)
                            .help("Created by automation")
                            .accessibilityLabel("Created by automation")
                    }
                    if task.isPlannerStaged {
                        Image(systemName: "sparkle.magnifyingglass")
                            .font(.caption2.weight(.semibold))
                            .foregroundStyle(.indigo)
                            .help("Staged by the Planner — release the project to begin dispatch")
                            .accessibilityLabel("Staged by the Planner")
                    }
                    if let extRef = externalRefLink {
                        ExternalRefLinkView(presentation: extRef)
                    }
                    // Doc-link icon. `designDocState` is non-nil for design
                    // cards (resolved from the parent project) AND for
                    // project-less docs-backed items like investigations
                    // (resolved on the task itself), so the kind gate is no
                    // longer needed — eligibility is already encoded in the
                    // state. Other kinds carry a nil state and render nothing.
                    if let state = designDocState,
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
                    if let openTerminal = onOpenTerminal {
                        Button {
                            openTerminal()
                        } label: {
                            Image(systemName: "terminal")
                                .font(.caption)
                                .foregroundStyle(Color.secondary)
                                .accessibilityLabel(terminalTooltip)
                        }
                        .buttonStyle(.plain)
                        .help(terminalTooltip)
                    }
                    if onMergeWhenReady != nil {
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
                    if !deferredScopeItems.isEmpty {
                        DeferredScopeCardBadge(
                            items: deferredScopeItems,
                            onAccept: { onAcceptDeferredScope?($0) },
                            onCreateTask: { onCreateTaskFromDeferredScope?($0) }
                        )
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }

            if let prURL = task.prURL, !prURL.isEmpty {
                HStack(alignment: .center, spacing: 6) {
                    if let mergeQueueState {
                        MergeQueueBadge(
                            mergeQueueState: mergeQueueState,
                            detail: mergeQueueDetail,
                            ciRequiredState: ciRequiredState
                        )
                        .layoutPriority(-1)
                    } else if let ciState = ciRequiredState {
                        PrCiIndicator(state: ciState, detail: ciRequiredDetail)
                    }
                    PRURLLink(
                        urlString: prURL,
                        font: .caption,
                        ambiguousRepoNames: ambiguousRepoNames
                    )
                    .layoutPriority(1)
                    if task.hasInProgressRevision {
                        PrInRevisionIndicator()
                            .onHover { hovering in
                                onRevisionBadgeHover?(hovering)
                            }
                    }
                    Spacer(minLength: 0)
                    if let id = task.shortID {
                        Text("T" + String(id))
                            .font(.system(.caption2, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .accessibilityLabel("T" + String(id))
                            .lineLimit(1)
                            .fixedSize(horizontal: true, vertical: false)
                    }
                }
            }

            if task.prURL != nil, let reviewState = reviewRequiredState {
                HStack(spacing: 6) {
                    PrReviewIndicator(state: reviewState, detail: reviewRequiredDetail)
                    Spacer(minLength: 0)
                }
            }

            // A revision task's own `prURL` is stamped with the chain root's
            // PR while an automated review pass holds it (see P992 in the
            // engine); once that happens it's the identical PR already
            // rendered above, so only show this second row when it points
            // somewhere new — otherwise the card displays the same PR link
            // twice (#1829 duplicate report).
            if task.kind == "revision", let prURL = task.revisionParentPrUrl, !prURL.isEmpty,
               !(task.prURL.map { sameGitHubPR($0, prURL) } ?? false) {
                HStack(alignment: .center, spacing: 6) {
                    PRURLLink(
                        urlString: prURL,
                        font: .caption,
                        ambiguousRepoNames: ambiguousRepoNames
                    )
                    Spacer(minLength: 0)
                }
            }

            if task.prURL == nil || task.prURL!.isEmpty, let id = task.shortID {
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

            if !inReviewRevisions.isEmpty {
                Divider()
                    .padding(.vertical, 2)
                ForEach(inReviewRevisions) { revision in
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
                .brightness(isHovered && !isSelected ? 0.04 : 0)
                .overlay(
                    RoundedRectangle(cornerRadius: 12, style: .continuous)
                        .strokeBorder(borderColor, lineWidth: isSelected ? 2 : 1)
                )
        )
        .shadow(
            color: (boardStyle == .airy || boardStyle == .elevated) ? Color.black.opacity(0.07) : .clear,
            radius: 4, x: 0, y: 1.5
        )
        .draggable(task.id)
        .onHover { hovering in
            withAnimation(.easeInOut(duration: 0.15)) {
                isHovered = hovering
            }
        }
    }

    /// The footer renders the priority chip on every card so a glance
    /// at the board immediately separates `[HIGH]` work from the rest
    /// without authors having to prefix names. The other footer
    /// elements (project tag, blocked tag) appear conditionally.
    private var hasFooterContent: Bool {
        true
    }

    /// True when the "conflict cleared" badge may render: the cleared flag
    /// is set AND no active "Merge Conflict" badge is showing. Enforces
    /// the T795 mutual-exclusion invariant — the two states must never
    /// co-render. Delegates to [[WorkBlockedBadge.conflictClearedVisible]].
    private var conflictClearedBadgeVisible: Bool {
        WorkBlockedBadge.conflictClearedVisible(
            forTask: task,
            cleared: showsConflictClearedBadge,
            isResolvingConflicts: isResolvingConflicts
        )
    }

    /// Tooltip body for the chain badge. Mirrors the CLI `show`
    /// output's prereq list so a hover tells the reader the same
    /// thing without opening the popover.
    var autoBlockTooltip: String {
        guard !gatingPrereqs.isEmpty else {
            return "Auto-blocked by dependencies"
        }
        let summary = gatingPrereqs
            .map { "\($0.title) (\($0.status.replacingOccurrences(of: "_", with: " ")))" }
            .joined(separator: ", ")
        return "Gated by: \(summary)"
    }

    /// Tint for the live-status subtitle row. Red for errored runs, a
    /// dimmer grey when the worker is idle, and the normal `.secondary`
    /// grey otherwise. The `waitingForInput` case is intentionally
    /// *not* tinted: it now carries its meaning via the explicit
    /// `WorkerWaitingIndicator` icon + tooltip instead of an ambiguous
    /// accent-blue subtitle (hue alone is an accessibility problem).
    private var liveStatusColor: Color {
        switch liveStatusActivity {
        case .errored:
            return .red
        case .idle:
            return Color(nsColor: .tertiaryLabelColor)
        default:
            return .secondary
        }
    }

    private var cardBackground: Color {
        if isSelected {
            return Color.accentColor.opacity(0.08)
        }
        if isFrontierHighlighted {
            return Color.green.opacity(0.07)
        }
        if !isResolvingConflicts && !isRemediatingCI && task.status == "blocked" {
            return Color.orange.opacity(0.08)
        }
        // Future-scope items get a muted neutral fill so parked work reads as
        // "set aside" at a glance, distinct from genuinely-queued backlog
        // cards. Ranked below `blocked` so a deferred-and-gated card still
        // shows the blocked-orange chrome (the "Future" badge conveys the
        // classification either way).
        if task.deferred {
            return Color.secondary.opacity(0.10)
        }
        switch boardStyle {
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
        if isSelected {
            return .accentColor
        }
        if !isResolvingConflicts && !isRemediatingCI && task.status == "blocked" {
            return .orange
        }
        // Soft muted outline reinforces the parked/future-scope treatment
        // established by `cardBackground` and the "Future" badge.
        if task.deferred {
            return Color.secondary.opacity(0.45)
        }
        switch boardStyle {
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
