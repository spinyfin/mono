import Foundation
import PaneLayout

// ===========================================================================
// Equatable view-input snapshot for one Agents-tab worker slot.
// Mirrors `WorkCardSnapshot`: a small value type holding exactly the
// fields `WorkerSlotView` renders, so `.equatable()` can skip a slot
// body when an unrelated `ChatViewModel` publish (or another slot's
// live-state tick) invalidates the parent.
// ===========================================================================

/// How the engine resolved an execution's label. Mirrors
/// `boss_protocol::ExecutionLabelStatus`.
enum ExecutionLabelStatus: String, Codable, Hashable {
    /// The work item the run was bound to still exists.
    case resolved
    /// The run was bound to a work item that has since been deleted.
    case workItemDeleted = "work_item_deleted"
    /// The run's kind is not bound to a work item (answer agent, automation
    /// triage); `purpose` still says what it was.
    case noWorkItem = "no_work_item"
    /// The engine has no execution row with this id.
    case executionUnknown = "execution_unknown"
}

/// The engine-resolved label of one execution: the work item it ran for, that
/// item's PR, and what the run was for. Mirrors
/// `boss_protocol::ExecutionWorkLabel`, answered by `ResolveExecutionLabels`
/// from the durable execution → work item → PR binding.
///
/// This is the only source of a finished pane's title and PR. The app does not
/// keep its own copy of a work item's name; it asks the engine again whenever
/// the Agents view comes into sight (see
/// `WorkersWorkspaceModel.refreshExecutionLabels`).
struct ExecutionWorkLabel: Codable, Hashable {
    var executionId: String
    var status: ExecutionLabelStatus
    var kind: String?
    var productId: String?
    var productName: String?
    var projectName: String?
    var prURL: String?
    var purpose: String?
    var workItemId: String?
    var workItemShortId: Int?
    var workItemTitle: String?

    enum CodingKeys: String, CodingKey {
        case executionId = "execution_id"
        case status
        case kind
        case productId = "product_id"
        case productName = "product_name"
        case projectName = "project_name"
        case prURL = "pr_url"
        case purpose
        case workItemId = "work_item_id"
        case workItemShortId = "work_item_short_id"
        case workItemTitle = "work_item_title"
    }
}

extension ExecutionWorkLabel {
    /// The one line that names the work in a finished pane's header, always in
    /// words. A resolved label reads `<purpose>: <title>` (just the title for
    /// a plain implementation run); every unresolved state says so plainly.
    /// The execution id is never part of it.
    var headline: String {
        switch status {
        case .resolved:
            let title = (workItemTitle ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            let name = title.isEmpty ? "Untitled work item" : title
            guard let purpose, !purpose.isEmpty else { return name }
            return "\(purpose): \(name)"
        case .workItemDeleted:
            return "Work item deleted"
        case .noWorkItem:
            if let purpose, !purpose.isEmpty { return "\(purpose) run" }
            return "No work item"
        case .executionUnknown:
            return "Run record unavailable"
        }
    }
}

/// Compact Agents-header row: product → project → short id → PR (`repo#n`),
/// each omitted when the engine did not supply it. Clicking the id reveals the
/// Work card; clicking the PR opens it in the browser. The project is the
/// first part to truncate when the row is too long.
struct WorkerHeaderWorkLinks: Equatable {
    let workItemId: String
    let productId: String?
    let productName: String?
    let projectName: String?
    let shortIdLabel: String
    let title: String
    let prURL: String?
    let prLabel: String?
}

/// Slim live-state slice that the Agents slot header actually paints.
/// `WorkerLiveState` carries extra fields (`model`, `shellPid`,
/// `currentTool`, …) the slot view never reads; folding
/// the whole struct into the snapshot would re-evaluate every slot on
/// those unused writes.
struct WorkerSlotLiveSlice: Equatable {
    let name: String?
    let activity: WorkerActivity
    let liveStatus: String?
    let recoveryStatus: String?
    let hungSince: String?
    let lastEventAt: String?
    /// Declared job wait, rendered as a caption and a Waiting pill when the
    /// engine reports the worker idle on it.
    let waitReason: String?
    let waitExpiresAt: String?
    let agentType: String?
    let reviewStage: String?
    let executionKind: String?
    let projectName: String?
    let pool: String?
    /// Read by the viewer-missing card only: `false` means the engine can
    /// never attach a pane (no durable tmux identity), so the card must not
    /// promise "Connecting…".
    let tmuxHosted: Bool?
    /// Engine-reported reason the last viewer attach failed; `nil` while
    /// nothing has failed. Read by the viewer-missing card only.
    let viewerAttachError: String?
    let workItemId: String?
    let workItemName: String?
    let workItemShortId: Int?
    let productId: String?
    let productName: String?
    let prURL: String?

    init(_ live: WorkerLiveState) {
        self.name = live.name
        self.activity = live.activity
        self.liveStatus = live.liveStatus
        self.recoveryStatus = live.recoveryStatus
        self.hungSince = live.hungSince
        self.lastEventAt = live.lastEventAt
        self.waitReason = live.waitReason
        self.waitExpiresAt = live.waitExpiresAt
        self.agentType = live.agentType
        self.reviewStage = live.reviewStage
        self.executionKind = live.executionKind
        self.projectName = live.projectName
        self.pool = live.pool
        self.tmuxHosted = live.tmuxHosted
        self.viewerAttachError = live.viewerAttachError
        self.workItemId = live.workItemId
        self.workItemName = live.workItemName
        self.workItemShortId = live.workItemShortId
        self.productId = live.productId
        self.productName = live.productName
        self.prURL = live.prURL
    }

    var badgeType: AgentType { AgentType.resolve(wire: agentType, reviewStage: reviewStage, executionKind: executionKind) }
    var badgeLabel: String { badgeType.badgeLabel }
    var isExited: Bool { activity == .terminated }
    var isEngineTerminal: Bool { activity == .terminated || activity == .errored }

    /// Mirrors `WorkerLiveState.isWaitingOnJob`.
    var isWaitingOnJob: Bool {
        activity == .idle && !(waitReason ?? "").isEmpty
    }

    /// "Waiting: <reason> · until <time>" while a job wait is active.
    var waitCaption: String? {
        guard isWaitingOnJob else { return nil }
        return AgentActivityState.waitCaption(reason: waitReason, expiresAt: waitExpiresAt)
    }
}

/// Per-slot view input. Closures such as the reveal-work-item handler stay
/// off this type so a parent rebuild cannot defeat `.equatable()`.
/// `session` is compared by identity: the terminal `NSViewRepresentable`
/// is keyed on the same object, and pointer equality is the cheap "same pane"
/// signal. Equatable is isolated to the main actor (same shape as
/// `WorkBoardCardView`) because `session` is a `@MainActor` class.
struct WorkerSlotSnapshot: @MainActor Equatable {
    let slotId: Int
    let runId: String?
    let summary: String?
    let taskTitle: String?
    /// Compared by identity in `==`. `nil` with a run means the
    /// viewer-missing card; non-nil means `WorkerPaneTerminalView`
    /// stays mounted.
    let session: TerminalPaneSession?
    let live: WorkerSlotLiveSlice?
    /// Engine-released cell retained by a layout epoch ("Riker finished").
    let isPlaceholder: Bool
    /// Cached persona for placeholders after live state is gone.
    let cachedPersona: String?
    /// Why this app refused the engine's last `AttachWorkerPane` for the
    /// run (slot busy, unknown slot, bad socket path), recorded by
    /// `WorkersWorkspaceModel`. `nil` when the app never rejected one.
    /// The app-side reason wins over the engine's: it is the more specific
    /// half of the same failed exchange.
    let viewerAttachFailure: String?
    /// The engine's label for a released run: its work item's short id, title
    /// and PR, resolved from the durable execution record (the live state that
    /// carried them is gone). `nil` until the engine has answered, and always
    /// `nil` for a live run. Never a locally remembered copy of the title.
    var finishedLabel: ExecutionWorkLabel?

    var displayName: String {
        if let cachedPersona, !cachedPersona.isEmpty { return cachedPersona }
        // A finished pane that never saw its persona must not print the raw
        // run id as a name.
        if isPlaceholder, (live?.name ?? "").isEmpty { return "Worker" }
        let runId = runId ?? ""
        return WorkerLiveState.displayName(name: live?.name, runId: runId)
    }

    /// Second titlebar line of a finished pane, and the body caption under
    /// "<name> finished": the work item's title (with what the run was for),
    /// or plain words when the engine could not resolve one. Never the run
    /// id; while the engine's answer is in flight it says so.
    var finishedSubtitle: String {
        finishedLabel?.headline ?? "Loading work item…"
    }

    var badgeType: AgentType { live?.badgeType ?? .unknown }
    var isExited: Bool { live?.isExited == true }
    var isViewerMissing: Bool { runId != nil && session == nil && !isPlaceholder }

    /// Caption for the viewer-missing card. A live member legitimately has
    /// no viewer only while the engine's `AttachWorkerPane` is still on its
    /// way (spawn, app or engine restart re-attach, the app's own
    /// re-attach request); that reads "Connecting…". Anything the app or
    /// the engine knows went wrong is shown instead, most specific first,
    /// so a failed attach is never dressed up as still connecting.
    var viewerMissingMessage: String {
        if live?.isEngineTerminal == true { return "Worker exited" }
        if let viewerAttachFailure, !viewerAttachFailure.isEmpty {
            return "Couldn't attach the terminal: \(viewerAttachFailure)"
        }
        if let error = live?.viewerAttachError, !error.isEmpty {
            return "Couldn't attach the terminal: \(error)"
        }
        if live?.tmuxHosted == false {
            return AgentActivityState.identityUnavailableReason
        }
        return "Connecting…"
    }

    /// True while the card's only honest caption is "Connecting…" — no
    /// failure is known on either side. The view asks the engine for the
    /// viewer in this state when the pane comes into view.
    var isViewerConnecting: Bool {
        isViewerMissing && live?.isEngineTerminal != true && viewerAttachFailure == nil && live?.viewerAttachError == nil && live?.tmuxHosted != false
    }

    /// What the slot body draws. A released run is always the finished card,
    /// whether or not its viewer is still attached; an attached surface stays
    /// mounted (hidden) until the matching detach. The viewer-missing card is
    /// reserved for a run the engine still lists.
    enum BodyKind: Equatable {
        case placeholder
        case terminal
        case viewerMissing
    }

    var bodyKind: BodyKind {
        if isPlaceholder { return .placeholder }
        return session != nil ? .terminal : .viewerMissing
    }

    /// First titlebar line. A terminated run never reads as working: the
    /// exited label wins over any summary or task title. Errored is
    /// non-terminal and can return to Working, so it keeps the summary.
    var headerTitle: String {
        let name = displayName
        if isPlaceholder {
            return "\(name) finished"
        }
        if let activity = live?.activity, activity == .terminated {
            return "\(name): \(activity.agentsPaneLabel)"
        }
        if let summary, !summary.isEmpty {
            // Success path: Claude-generated gerund phrase.
            // Preserved verbatim — do NOT change this branch.
            return "\(name) is \(summary)"
        }
        if let taskTitle, !taskTitle.isEmpty {
            // Fallback path: no gerund available (no API key or
            // summarization failed). Use "<Name>: <task>" format.
            return "\(name): \(taskTitle)"
        }
        if runId != nil {
            return "\(name) is working"
        }
        return name
    }

    /// Type badge in the titlebar and on the viewer-missing card. Only an
    /// engine-listed run has a type to show; idle and finished runs show none.
    /// Idle replaces the kind so the chrome does not imply ongoing work.
    var headerBadge: AgentType? {
        guard !isPlaceholder, let live, live.activity != .idle else { return nil }
        return badgeType
    }

    /// Titlebar status pill text. `nil` means no engine state yet: the view
    /// falls back to the session-observing pane-monitor pill.
    var showsStatusPill: Bool {
        isPlaceholder || live?.activity != .working || live?.isWaitingOnJob == true || live?.hungSince != nil
    }

    var statusPillLabel: String? {
        if isPlaceholder { return "Finished" }
        if live?.isWaitingOnJob == true { return "Waiting" }
        if live?.hungSince != nil { return "Hung" }
        return live?.activity.agentsPaneLabel
    }

    /// Compact header row linking the bound work item and its PR.
    /// `nil` when the pane has no work item (ad-hoc) or the engine has
    /// not yet assigned a short id.
    var headerWorkLinks: WorkerHeaderWorkLinks? {
        if isPlaceholder { return finishedWorkLinks }
        guard let live,
              let workItemId = live.workItemId, !workItemId.isEmpty,
              let shortId = live.workItemShortId
        else { return nil }
        let prURL = live.prURL.flatMap { $0.isEmpty ? nil : $0 }
        return WorkerHeaderWorkLinks(
            workItemId: workItemId,
            productId: live.productId.flatMap { $0.isEmpty ? nil : $0 },
            productName: Self.nonEmpty(live.productName),
            projectName: Self.nonEmpty(live.projectName),
            shortIdLabel: Self.shortIdLabel(workItemId: workItemId, shortId: shortId),
            title: live.workItemName ?? "",
            prURL: prURL,
            prLabel: prURL.flatMap(Self.pullRequestCompactLabel(from:))
        )
    }

    /// The same id-and-PR row a live pane shows, built from the engine's
    /// durable label. `nil` when the work item is gone or unresolved.
    private var finishedWorkLinks: WorkerHeaderWorkLinks? {
        guard let label = finishedLabel, label.status == .resolved,
              let workItemId = label.workItemId, !workItemId.isEmpty,
              let shortId = label.workItemShortId
        else { return nil }
        let prURL = label.prURL.flatMap { $0.isEmpty ? nil : $0 }
        return WorkerHeaderWorkLinks(
            workItemId: workItemId,
            productId: label.productId.flatMap { $0.isEmpty ? nil : $0 },
            productName: Self.nonEmpty(label.productName),
            projectName: Self.nonEmpty(label.projectName),
            shortIdLabel: Self.shortIdLabel(workItemId: workItemId, shortId: shortId),
            title: label.workItemTitle ?? "",
            prURL: prURL,
            prLabel: prURL.flatMap(Self.pullRequestCompactLabel(from:))
        )
    }

    private static func nonEmpty(_ value: String?) -> String? {
        guard let trimmed = value?.trimmingCharacters(in: .whitespacesAndNewlines), !trimmed.isEmpty else { return nil }
        return trimmed
    }

    /// `T<n>` for tasks and chores, `P<n>` for projects.
    static func shortIdLabel(workItemId: String, shortId: Int) -> String {
        workItemId.hasPrefix("proj_") ? "P\(shortId)" : "T\(shortId)"
    }

    /// `repo#n` from a GitHub PR URL, dropping the owner. `nil` when the
    /// string is not a github.com `/pull/<n>` URL.
    static func pullRequestCompactLabel(from urlString: String) -> String? {
        guard let parsed = parseGitHubPRURL(urlString) else { return nil }
        return "\(parsed.repo)#\(parsed.number)"
    }

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.slotId == rhs.slotId
            && lhs.runId == rhs.runId
            && lhs.summary == rhs.summary
            && lhs.taskTitle == rhs.taskTitle
            && lhs.session === rhs.session
            && lhs.live == rhs.live
            && lhs.isPlaceholder == rhs.isPlaceholder
            && lhs.cachedPersona == rhs.cachedPersona
            && lhs.viewerAttachFailure == rhs.viewerAttachFailure
            && lhs.finishedLabel == rhs.finishedLabel
    }

    static func build(
        slot: WorkerSlot,
        liveState: WorkerLiveState?,
        isPlaceholder: Bool = false,
        cachedPersona: String? = nil,
        viewerAttachFailure: String? = nil,
        finishedLabel: ExecutionWorkLabel? = nil
    ) -> WorkerSlotSnapshot {
        WorkerSlotSnapshot(
            slotId: slot.slotId,
            runId: slot.runId,
            summary: slot.summary,
            taskTitle: slot.taskTitle,
            session: slot.session,
            live: liveState.map(WorkerSlotLiveSlice.init),
            isPlaceholder: isPlaceholder,
            cachedPersona: cachedPersona,
            viewerAttachFailure: viewerAttachFailure,
            finishedLabel: finishedLabel
        )
    }

    /// The Agents grid's render rule for one mounted surface, derived from
    /// the current inputs only — never from an earlier render. A run the
    /// engine lists is live; a run it no longer lists is finished, whatever
    /// the layout, the viewer map, or tab visibility were doing when the
    /// release arrived. There is no third state: a released run cannot be
    /// drawn as a viewerless live member with an "Unknown" type.
    ///
    /// A viewer attached before its first live-state snapshot reads as
    /// finished here, but it is never placed in the layout (membership comes
    /// from live state only), so it is mounted hidden and never drawn.
    ///
    /// `viewer` is the run's attached pane, if any; without one the slot
    /// metadata comes from the live state (the engine's slot id) so a
    /// release followed by immediate slot reuse still attributes the
    /// finished card to the right run.
    static func render(
        runId: String,
        viewer: WorkerSlot?,
        liveState: WorkerLiveState?,
        cachedPersona: String?,
        viewerAttachFailure: String? = nil,
        finishedLabel: ExecutionWorkLabel? = nil
    ) -> WorkerSlotSnapshot {
        let slotId = viewer?.slotId ?? liveState?.slotId ?? 0
        let slot = viewer ?? WorkerSlot(slotId: slotId, runId: runId, summary: nil, taskTitle: nil)
        return build(
            slot: slot,
            liveState: liveState,
            isPlaceholder: liveState == nil,
            cachedPersona: cachedPersona,
            viewerAttachFailure: viewerAttachFailure,
            finishedLabel: liveState == nil ? finishedLabel : nil
        )
    }
}
