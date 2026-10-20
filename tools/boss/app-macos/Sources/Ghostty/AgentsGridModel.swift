import CoreGraphics
import Foundation
import PaneLayout

/// Joins engine local membership with the run-keyed viewer map and drives
/// `PaneLayoutModel`. Cell, page, and order stay app-local; run id is
/// identity. The project/type filter starts at `.all` on every launch (this
/// model is created fresh with the view) and is never persisted.
@MainActor
final class AgentsGridModel: ObservableObject {
    static let geometryQuietInterval: TimeInterval = 0.3

    @Published private(set) var layout: PaneLayoutModel
    @Published private(set) var poolStrip = AgentsGridModel.defaultPoolStrip
    @Published private(set) var feedUnavailable = false
    /// Attached viewers whose run is not in any layout cell (engine
    /// released and compacted, detach not yet arrived). Keep their
    /// surfaces mounted until `DetachWorkerPane`.
    private var viewers: [String: WorkerSlot] = [:]
    var unplacedViewers: [WorkerSlot] {
        viewers.values.filter { $0.session != nil && !placedRunIds(in: layout).contains($0.runId ?? "") }
            .sorted { $0.slotId < $1.slotId }
    }
    /// Display names for projects seen on live members, keyed by project id.
    @Published private(set) var projectNames: [String: String] = [:]
    /// Projects with at least one live local member, in display order.
    @Published private(set) var memberProjects: [AgentProject] = []
    private var inLiveResize = false
    private var exitedRunIds: Set<String> = []
    var onViewerAttachRequested: (([String]) -> Void)?

    func requestViewersForVisiblePage() {
        guard layout.isVisible else { return }
        onViewerAttachRequested?(viewerlessRunIdsOnSelectedPage)
    }

    private var personaByRun: [String: String] = [:]
    private var pendingArea: PaneSize?
    private var lastCell = PaneSize(width: 0, height: 0)
    private var quietTask: Task<Void, Never>?

    static let defaultPoolStrip = "Interactive 0/16 · Automation 0/8 · Review 0/16"

    /// Provisional metrics for the fixed 10pt worker font. Membership can
    /// arrive without any attached viewer, so pagination must not depend on
    /// libghostty creating a surface. A real cell measurement replaces these.
    static let fallbackCell = PaneSize(width: 6, height: 13)

    init(
        policy: PaneCapacityPolicy = .standard,
        geometry: PaneGeometry = PaneGeometry(
            area: PaneSize(width: 0, height: 0),
            cell: AgentsGridModel.fallbackCell
        ),
        isVisible: Bool = true
    ) {
        lastCell = geometry.cell
        layout = PaneLayoutModel(policy: policy, geometry: geometry, isVisible: isVisible)
    }

    func personaName(for runId: String) -> String? {
        personaByRun[runId]
    }

    func setVisible(_ visible: Bool) {
        replaceLayout { $0.setVisible(visible) }
    }

    func selectPage(_ page: Int) {
        replaceLayout { $0.selectPage(page) }
    }

    func tidy() {
        replaceLayout { $0.tidy() }
    }

    // MARK: - Filters

    /// Project choices: projects with live members, any project still checked
    /// (so it can be unchecked), and always Unfiled. Unfiled sorts last.
    var projectOptions: [AgentProject] {
        var options = Set(memberProjects)
        options.formUnion(layout.filter.projects ?? [])
        options.insert(.unfiled)
        return options.sorted { lhs, rhs in
            switch (lhs, rhs) {
            case (.unfiled, .unfiled): return false
            case (.unfiled, _): return false
            case (_, .unfiled): return true
            default: return projectLabel(lhs).localizedCaseInsensitiveCompare(projectLabel(rhs)) == .orderedAscending
            }
        }
    }

    /// Every badge type is offered, including Unknown, so a worker with a
    /// missing or unrecognised wire type can always be filtered to.
    var typeOptions: [AgentType] { AgentType.allCases }

    func projectLabel(_ project: AgentProject) -> String {
        switch project {
        case .unfiled: return "Unfiled"
        case .project(let id): return projectNames[id].flatMap { $0.isEmpty ? nil : $0 } ?? id
        }
    }

    func toggleProject(_ project: AgentProject) {
        var next = layout.filter
        next.toggle(project: project)
        setFilter(next)
    }

    func toggleType(_ type: AgentType) {
        var next = layout.filter
        next.toggle(type: type)
        setFilter(next)
    }

    func clearFilter() {
        setFilter(.all)
    }

    /// A filter change ends the layout epoch. It only changes which members
    /// get cells; viewers, membership and execution are untouched.
    func setFilter(_ filter: PaneFilter) {
        replaceLayout { $0.setFilter(filter) }
    }

    /// Run outlined after an explicit focus request, so the reader can find
    /// the cell. Cleared after `highlightDuration`.
    @Published private(set) var highlightedRunId: String?
    /// Transient notice that a focus request cleared a hiding filter.
    @Published private(set) var filterClearedBanner: String?
    static let highlightDuration: TimeInterval = 3
    private var highlightTask: Task<Void, Never>?

    /// Reveal path for `bossctl agents focus` and Doing-card clicks: clear a
    /// hiding filter (with a banner), select the run's page, focus it, and
    /// outline it. Viewer-less members are revealed too; their card is the
    /// focus target.
    @discardableResult
    func reveal(runId: String) -> PaneRevealOutcome {
        var outcome = PaneRevealOutcome.notMember
        replaceLayout { outcome = $0.reveal(runId: runId) }
        guard outcome != .notMember else { return outcome }
        highlightedRunId = runId
        filterClearedBanner = outcome == .revealedClearingFilter
            ? "Filter cleared to show \(personaByRun[runId] ?? "this agent")."
            : nil
        highlightTask?.cancel()
        highlightTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(AgentsGridModel.highlightDuration * 1_000_000_000))
            guard !Task.isCancelled else { return }
            self?.highlightedRunId = nil
            self?.filterClearedBanner = nil
        }
        return outcome
    }

    func setFocusedRun(_ runId: String?) {
        replaceLayout { $0.setFocusedRun(runId) }
    }

    /// Live members placed on the selected page that have no viewer yet, in
    /// cell order. These are the runs whose cards read "Connecting…" when
    /// the page is shown, and the ones the view asks the engine to attach.
    /// Off-page members are left to the engine's own retry: their surfaces
    /// would be mounted invisibly, and asking for them when their page is
    /// selected keeps each request tied to something the user can see.
    var viewerlessRunIdsOnSelectedPage: [String] {
        guard let page = layout.currentPage else { return [] }
        return page.cells.compactMap { cell in
            if case .occupied(let member) = cell.content, !member.hasViewer, !exitedRunIds.contains(member.runId) {
                return member.runId
            }
            return nil
        }
    }

    /// Authoritative membership + current viewer map. Callers must pass
    /// only an accepted live-state snapshot: duplicates and a disconnected
    /// feed are handled by `LiveWorkerStateStore` before they reach here.
    ///
    /// `isVisible` orders the visibility change around the membership
    /// update. Going hidden hides first, and coming back shows last, so
    /// the snapshot lands while the model is hidden and applies at once;
    /// entering Agents then repacks current membership, not stale membership.
    func apply(
        isVisible: Bool,
        states: [WorkerLiveState],
        viewers: [String: WorkerSlot],
        interactivePoolSize: Int,
        automationPoolSize: Int,
        reviewPoolSize: Int,
        feedUnavailable: Bool = false
    ) {
        if !isVisible { setVisible(false) }
        apply(
            states: states,
            viewers: viewers,
            interactivePoolSize: interactivePoolSize,
            automationPoolSize: automationPoolSize,
            reviewPoolSize: reviewPoolSize,
            feedUnavailable: feedUnavailable
        )
        if isVisible { setVisible(true) }
    }

    func apply(
        states: [WorkerLiveState],
        viewers: [String: WorkerSlot],
        interactivePoolSize: Int,
        automationPoolSize: Int,
        reviewPoolSize: Int,
        feedUnavailable: Bool = false
    ) {
        if self.feedUnavailable != feedUnavailable {
            self.feedUnavailable = feedUnavailable
        }
        self.viewers = viewers
        let local = states.filter(\.isLocalMember)
        exitedRunIds = Set(local.filter { $0.activity == .terminated }.map(\.runId))
        for state in local {
            if let name = state.name, !name.isEmpty {
                personaByRun[state.runId] = name
            }
        }

        var names: [String: String] = [:]
        for state in local {
            if let id = state.projectId, !id.isEmpty, let name = state.projectName, !name.isEmpty {
                names[id] = name
            }
        }
        if projectNames != names { projectNames = names }
        let projects = Array(Set(local.map(\.agentProject)))
        if Set(memberProjects) != Set(projects) { memberProjects = projects }

        let members = local.map { state in
            PaneMember(
                runId: state.runId,
                startedAt: state.startedAt.flatMap(WorkerStaleness.parse) ?? .distantFuture,
                project: state.agentProject,
                type: state.badgeType,
                isWaiting: state.activity == .waitingForInput,
                hasViewer: viewers[state.runId]?.session != nil
            )
        }
        replaceLayout { $0.updateMembers(members) }

        let needed = Set(local.map(\.runId))
            .union(placedRunIds(in: layout))
            .union(viewers.keys)
        personaByRun = personaByRun.filter { needed.contains($0.key) }

        let strip = Self.poolStrip(
            states: local,
            interactivePoolSize: interactivePoolSize,
            automationPoolSize: automationPoolSize,
            reviewPoolSize: reviewPoolSize
        )
        if poolStrip != strip {
            poolStrip = strip
        }
    }

    func noteArea(width: Double, height: Double, inLiveResize: Bool) {
        guard width > 0, height > 0, width.isFinite, height.isFinite else { return }
        pendingArea = PaneSize(width: width, height: height)
        self.inLiveResize = inLiveResize
        if inLiveResize {
            quietTask?.cancel()
            quietTask = nil
            return
        }
        if !layout.geometry.isMeasured {
            applyPendingGeometry()
        } else {
            scheduleQuietApply()
        }
    }

    func noteCellSize(width: Double, height: Double) {
        guard width > 0, height > 0, width.isFinite, height.isFinite else { return }
        lastCell = PaneSize(width: width, height: height)
        applyPendingGeometry()
    }

    func liveResizeWillStart() {
        inLiveResize = true
        quietTask?.cancel()
        quietTask = nil
    }

    func liveResizeDidEnd() {
        inLiveResize = false
        quietTask?.cancel()
        quietTask = nil
        applyPendingGeometry()
    }

    /// Test seam: skip the 300 ms quiet window.
    func flushGeometryForTesting() {
        liveResizeDidEnd()
    }

    /// Run ids whose surfaces must stay mounted: every placed run (occupied
    /// or placeholder) plus every attached viewer, so an engine release that
    /// compacts the run out of the layout never tears down its surface before
    /// `DetachWorkerPane` arrives.
    func renderRunIds(attached: some Collection<String>) -> [String] {
        placedRunIds(in: layout).union(attached).sorted()
    }

    /// Frame and visibility for a run's surface within `area`. Pane
    /// rectangles use the same `policy.gap` the capacity and shape math
    /// scores with. A run in no cell (released, detach pending) gets a usable
    /// hidden frame.
    func placement(for runId: String, area: CGSize) -> (frame: CGRect, visible: Bool) {
        Self.placement(for: runId, area: area, layout: layout)
    }

    static func placement(for runId: String, area: CGSize, layout: PaneLayoutModel) -> (frame: CGRect, visible: Bool) {
        let gap = CGFloat(layout.policy.gap)
        for page in layout.pages {
            for cell in page.cells {
                let id: String?
                switch cell.content {
                case .occupied(let member): id = member.runId
                case .placeholder(let run): id = run
                case .empty: id = nil
                }
                guard id == runId else { continue }
                let columns = CGFloat(page.shape.columns)
                let rows = CGFloat(page.shape.rows)
                let width = max(1, (area.width - (columns - 1) * gap) / columns)
                let height = max(1, (area.height - (rows - 1) * gap) / rows)
                let position = page.shape.position(ofLocalIndex: cell.localIndex)
                let frame = CGRect(
                    x: CGFloat(position.column) * (width + gap),
                    y: CGFloat(position.row) * (height + gap),
                    width: width,
                    height: height
                )
                return (frame, page.index == layout.selectedPage)
            }
        }
        return (CGRect(x: 0, y: 0, width: max(420, area.width), height: max(356, area.height)), false)
    }

    static func poolStrip(
        states: [WorkerLiveState],
        interactivePoolSize: Int,
        automationPoolSize: Int,
        reviewPoolSize: Int
    ) -> String {
        let main = states.filter { $0.pool == "main" }.count
        let automation = states.filter { $0.pool == "automation" }.count
        let review = states.filter { $0.pool == "review" }.count
        return "Interactive \(main)/\(interactivePoolSize) · Automation \(automation)/\(automationPoolSize) · Review \(review)/\(reviewPoolSize)"
    }

    private func scheduleQuietApply() {
        quietTask?.cancel()
        quietTask = Task { [weak self] in
            let nanos = UInt64(AgentsGridModel.geometryQuietInterval * 1_000_000_000)
            try? await Task.sleep(nanoseconds: nanos)
            guard !Task.isCancelled else { return }
            self?.applyPendingGeometry()
        }
    }

    private func applyPendingGeometry() {
        guard !inLiveResize else { return }
        let area = pendingArea ?? layout.geometry.area
        let cell = (lastCell.width > 0 && lastCell.height > 0) ? lastCell : layout.geometry.cell
        replaceLayout { $0.updateGeometry(PaneGeometry(area: area, cell: cell)) }
    }

    private func replaceLayout(_ mutate: (inout PaneLayoutModel) -> Void) {
        var next = layout
        mutate(&next)
        if next != layout {
            layout = next
        }
        requestViewersForVisiblePage()
    }

    private func placedRunIds(in layout: PaneLayoutModel) -> Set<String> {
        var ids = Set<String>()
        for page in layout.pages {
            for cell in page.cells {
                switch cell.content {
                case .occupied(let member):
                    ids.insert(member.runId)
                case .placeholder(let runId):
                    ids.insert(runId)
                case .empty:
                    break
                }
            }
        }
        return ids
    }
}
