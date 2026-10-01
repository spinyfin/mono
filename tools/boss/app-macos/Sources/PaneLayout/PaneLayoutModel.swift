import Foundation

/// Pure layout state for the dynamic Agents view: which run sits in which
/// logical cell, how the cells are drawn, and which page is selected.
///
/// Run identity is the execution id. Cell, page, and order are presentation
/// state owned here and never leave the app.
///
/// Mutations fall into two kinds:
/// - **Epoch changes** (`updateMembers` while visible) keep every run's logical
///   cell. Arrivals take the lowest free cell and may grow their page's grid;
///   releases leave a placeholder. The one exception is forced collapse: when
///   fewer pages would suffice, the epoch ends at once.
/// - **Layout boundaries** (page change, filter change, geometry change, Tidy,
///   entering the view, forced collapse) drop holes and placeholders, pack the
///   filtered members in `(startedAt, runId)` order, and refit each page.
///
/// Automatic boundaries and growth keep the reader's place by choosing an
/// anchor run (see `anchorRunId`) and selecting its page afterwards.
public struct PaneLayoutModel: Equatable, Sendable {
    private enum Slot: Equatable, Sendable {
        case empty
        case run(String)
        case placeholder(String)

        var occupiedRunId: String? {
            if case .run(let id) = self { return id }
            return nil
        }

        var isFree: Bool {
            if case .run = self { return false }
            return true
        }
    }

    public let policy: PaneCapacityPolicy
    public private(set) var geometry: PaneGeometry
    public private(set) var limits: GridLimits
    /// Whether `limits` came from a real cell-size measurement. Hysteresis only
    /// applies against measured limits, never the 1x1 unmeasured fallback.
    private var limitsMeasured: Bool
    public private(set) var filter: PaneFilter = .all
    /// Whether Agents is on screen. While hidden every change applies at once,
    /// since there is no reader to disturb.
    public private(set) var isVisible: Bool
    public private(set) var selectedPage = 0
    public private(set) var focusedRunId: String?
    /// The run the most recent automatic change kept in view: the focused run,
    /// otherwise the first surviving run on the selected page, otherwise the
    /// survivor nearest that page's previous first occupied cell (ties to the
    /// lower cell). `nil` when no run survived or after explicit navigation.
    public private(set) var anchorRunId: String?

    /// Every engine member before filtering.
    private var members: [String: PaneMember] = [:]
    /// Logical cells, global index = page * capacity + local index. Trailing
    /// `.empty` entries are trimmed, so the count is the used span.
    private var slots: [Slot] = []
    /// Rendered shape per page; always `pageCount` entries.
    private var shapes: [GridShape] = []

    public init(policy: PaneCapacityPolicy = .standard, geometry: PaneGeometry, isVisible: Bool = true) {
        self.policy = policy
        self.geometry = geometry
        self.limits = policy.limits(for: geometry)
        self.limitsMeasured = geometry.isMeasured
        self.isVisible = isVisible
    }

    // MARK: - Mutations

    /// Applies an authoritative snapshot of all engine-reported local members.
    ///
    /// Runs absent from the snapshot become placeholders; runs that are present
    /// but no longer pass the filter simply lose their cell. The caller must not
    /// pass malformed or unavailable snapshots: only accepted ones may remove
    /// members.
    public mutating func updateMembers(_ snapshot: [PaneMember]) {
        let before = slots
        members = Dictionary(snapshot.map { ($0.runId, $0) }, uniquingKeysWith: { _, latest in latest })
        let visible = visibleRunIds()

        for index in slots.indices {
            switch slots[index] {
            case .empty:
                break
            case .run(let id):
                if !visible.contains(id) {
                    slots[index] = members[id] == nil ? .placeholder(id) : .empty
                }
            case .placeholder(let id):
                // A run that reappears keeps the cell it had.
                if visible.contains(id) { slots[index] = .run(id) }
            }
        }

        let placed = Set(slots.compactMap(\.occupiedRunId))
        let arrivals = members.values
            .filter { visible.contains($0.runId) && !placed.contains($0.runId) }
            .sorted(by: Self.sortOrder)
        for arrival in arrivals {
            place(arrival.runId)
        }
        trimSlots()

        let anchor = chooseAnchor(before: before, page: selectedPage, survivors: survivors(of: before))
        let neededPages = Self.ceilDiv(visible.count, capacity)
        if !isVisible || neededPages < pageCount {
            repack(anchor: anchor)
        } else {
            syncShapes()
            anchorRunId = anchor
            selectAnchorPage(anchor)
            clampSelectedPage()
        }
    }

    /// Replaces the project/type filter. A layout boundary.
    public mutating func setFilter(_ newFilter: PaneFilter) {
        guard newFilter != filter else { return }
        let before = slots
        filter = newFilter
        let anchor = chooseAnchor(before: before, page: selectedPage, survivors: survivors(of: before))
        repack(anchor: anchor)
    }

    /// Applies a new area/cell-size measurement. A layout boundary: callers
    /// should invoke it at drag end or after the geometry has been quiet, not
    /// on every drag frame. Capacity only moves once the measurement clears the
    /// policy's dead band; an identical or unmeasured (zero, negative, or
    /// non-finite cell size) measurement is a no-op.
    public mutating func updateGeometry(_ newGeometry: PaneGeometry) {
        guard newGeometry != geometry, newGeometry.isMeasured else { return }
        let anchor = automaticAnchor()
        geometry = newGeometry
        limits = policy.stableLimits(previous: limitsMeasured ? limits : nil, for: newGeometry)
        limitsMeasured = true
        repack(anchor: anchor)
    }

    /// Explicit page navigation. A layout boundary: holes are removed first,
    /// then `page` is clamped to the packed page range and selected.
    public mutating func selectPage(_ page: Int) {
        guard page != selectedPage else { return }
        repack(anchor: nil)
        selectedPage = min(max(0, page), max(0, pageCount - 1))
    }

    /// Packs the layout, keeping the reader's anchor visible.
    public mutating func tidy() {
        repack(anchor: automaticAnchor())
    }

    /// Records the run holding keyboard focus; the first candidate when
    /// choosing an anchor.
    public mutating func setFocusedRun(_ runId: String?) {
        focusedRunId = runId
    }

    /// Entering the view is a layout boundary; leaving it changes nothing but
    /// makes later updates apply immediately.
    public mutating func setVisible(_ visible: Bool) {
        guard visible != isVisible else { return }
        isVisible = visible
        if visible {
            repack(anchor: automaticAnchor())
        }
    }

    // MARK: - Pages

    public var capacity: Int { policy.capacity(of: limits) }

    /// Pages that hold at least one cell. After any boundary this is exactly
    /// `ceil(visibleCount / capacity)`; zero means the empty state.
    public var pageCount: Int { Self.ceilDiv(slots.count, capacity) }

    public var isEmpty: Bool { slots.isEmpty }

    public var pages: [PaneLayoutPage] { (0..<pageCount).map(page(at:)) }

    public var currentPage: PaneLayoutPage? {
        pageCount > 0 ? page(at: selectedPage) : nil
    }

    public func page(at pageIndex: Int) -> PaneLayoutPage {
        let shape = shapes[pageIndex]
        let cap = capacity
        var cells: [PaneCell] = []
        cells.reserveCapacity(shape.cellCount)
        for local in 0..<shape.cellCount {
            let global = pageIndex * cap + local
            let content: PaneCellContent
            // Rectangles past `capacity` are spare: they never hold a run.
            if local < cap, global < slots.count {
                content = cellContent(slots[global])
            } else {
                content = .empty
            }
            let position = shape.position(ofLocalIndex: local)
            cells.append(PaneCell(localIndex: local, row: position.row, column: position.column, content: content))
        }
        let ids = occupiedRunIds(onPage: pageIndex)
        return PaneLayoutPage(
            index: pageIndex,
            shape: shape,
            cells: cells,
            occupiedCount: ids.count,
            waitingCount: ids.filter { members[$0]?.isWaiting == true }.count
        )
    }

    /// Selectors appear only when an occupied cell lies past the first page.
    public var selectorsVisible: Bool {
        guard let last = slots.lastIndex(where: { $0.occupiedRunId != nil }) else { return false }
        return last >= capacity
    }

    public var pageSelectors: [PageSelector] {
        guard selectorsVisible else { return [] }
        return (0..<pageCount).map { index in
            let ids = occupiedRunIds(onPage: index)
            return PageSelector(
                index: index,
                occupiedCount: ids.count,
                waitingCount: ids.filter { members[$0]?.isWaiting == true }.count
            )
        }
    }

    /// Global logical cell index of a run, if it holds one.
    public func logicalCellIndex(ofRun runId: String) -> Int? {
        slots.firstIndex(of: .run(runId))
    }

    /// Whether a boundary would change the current layout (holes, placeholders,
    /// out-of-order cells, or a larger-than-needed grid). Drives Tidy's highlight.
    public var isTidyHighlighted: Bool {
        let packed = packedLayout()
        return packed.slots != slots || packed.shapes != shapes
    }

    // MARK: - Counts

    /// Members passing the filter, including cards without viewers and members
    /// awaiting release.
    public var visibleCount: Int { visibleMembers().count }

    /// Members the filter hides. Show it whenever `filter.isActive`.
    public var hiddenCount: Int { members.count - visibleCount }

    /// Waiting members among the visible ones, whether or not they have a viewer.
    public var waitingCount: Int { visibleMembers().filter(\.isWaiting).count }

    /// Waiting members the filter hides, so a filter cannot silently bury one.
    public var hiddenWaitingCount: Int {
        members.values.filter { $0.isWaiting && !filter.admits($0) }.count
    }

    // MARK: - Internals

    private static func sortOrder(_ lhs: PaneMember, _ rhs: PaneMember) -> Bool {
        if lhs.startedAt != rhs.startedAt { return lhs.startedAt < rhs.startedAt }
        return lhs.runId < rhs.runId
    }

    private static func ceilDiv(_ numerator: Int, _ denominator: Int) -> Int {
        (numerator + denominator - 1) / denominator
    }

    private func visibleMembers() -> [PaneMember] {
        members.values.filter(filter.admits)
    }

    private func visibleRunIds() -> Set<String> {
        Set(visibleMembers().map(\.runId))
    }

    private func cellContent(_ slot: Slot) -> PaneCellContent {
        switch slot {
        case .empty:
            return .empty
        case .run(let id):
            return members[id].map(PaneCellContent.occupied) ?? .empty
        case .placeholder(let id):
            return .placeholder(runId: id)
        }
    }

    private func occupiedRunIds(onPage pageIndex: Int) -> [String] {
        let start = pageIndex * capacity
        guard start < slots.count else { return [] }
        return slots[start..<min(start + capacity, slots.count)].compactMap(\.occupiedRunId)
    }

    /// Runs occupying a cell in `before` that are still visible now.
    private func survivors(of before: [Slot]) -> Set<String> {
        Set(before.compactMap(\.occupiedRunId)).intersection(visibleRunIds())
    }

    private func automaticAnchor() -> String? {
        chooseAnchor(before: slots, page: selectedPage, survivors: survivors(of: slots))
    }

    /// Anchor selection over the pre-change layout `before`, using the page
    /// that was selected and the capacity in force before the change.
    private func chooseAnchor(before: [Slot], page: Int, survivors: Set<String>) -> String? {
        guard !survivors.isEmpty else { return nil }
        if let focusedRunId, survivors.contains(focusedRunId) { return focusedRunId }

        let start = page * capacity
        var previousFirstOccupied: Int?
        for index in start..<max(start, min(start + capacity, before.count)) {
            guard let id = before[index].occupiedRunId else { continue }
            if previousFirstOccupied == nil { previousFirstOccupied = index }
            if survivors.contains(id) { return id }
        }

        let target = previousFirstOccupied ?? start
        var nearest: (id: String, distance: Int)?
        for (index, slot) in before.enumerated() {
            guard let id = slot.occupiedRunId, survivors.contains(id) else { continue }
            let distance = abs(index - target)
            // Ascending scan, so a tie keeps the lower cell.
            if nearest == nil || distance < nearest!.distance {
                nearest = (id, distance)
            }
        }
        return nearest?.id
    }

    /// Puts an arrival in the lowest free cell (a hole or a placeholder), or on
    /// a new page only when every existing cell is occupied.
    private mutating func place(_ runId: String) {
        if let index = slots.firstIndex(where: \.isFree) {
            slots[index] = .run(runId)
        } else {
            slots.append(.run(runId))
        }
        syncShapes()
    }

    private mutating func trimSlots() {
        while slots.last == .empty {
            slots.removeLast()
        }
    }

    /// Keeps one shape per page, growing a page's shape only when its used span
    /// no longer fits. Shapes never shrink inside an epoch.
    private mutating func syncShapes() {
        let pages = pageCount
        if shapes.count > pages { shapes.removeLast(shapes.count - pages) }
        for page in 0..<pages {
            let span = usedSpan(ofPage: page)
            if page >= shapes.count {
                shapes.append(policy.shape(forUsedSpan: span, limits: limits, geometry: geometry))
            } else if span > shapes[page].cellCount {
                shapes[page] = policy.shape(forUsedSpan: span, limits: limits, geometry: geometry)
            }
        }
    }

    /// Highest used (occupied or placeholder) local index on the page, plus one.
    private func usedSpan(ofPage page: Int) -> Int {
        let start = page * capacity
        guard start < slots.count else { return 0 }
        let pageSlots = slots[start..<min(start + capacity, slots.count)]
        guard let last = pageSlots.lastIndex(where: { $0 != .empty }) else { return 0 }
        return last - start + 1
    }

    private func packedLayout() -> (slots: [Slot], shapes: [GridShape]) {
        let packed = visibleMembers().sorted(by: Self.sortOrder).map { Slot.run($0.runId) }
        let cap = capacity
        let shapes = stride(from: 0, to: packed.count, by: cap).map { start in
            policy.shape(forUsedSpan: min(cap, packed.count - start), limits: limits, geometry: geometry)
        }
        return (packed, shapes)
    }

    private mutating func repack(anchor: String?) {
        let packed = packedLayout()
        slots = packed.slots
        shapes = packed.shapes
        anchorRunId = anchor
        selectAnchorPage(anchor)
        clampSelectedPage()
    }

    /// Selects the page holding the anchor's logical cell, if it has one.
    private mutating func selectAnchorPage(_ anchor: String?) {
        if let anchor, let index = slots.firstIndex(of: .run(anchor)) {
            selectedPage = index / capacity
        }
    }

    private mutating func clampSelectedPage() {
        selectedPage = min(max(0, selectedPage), max(0, pageCount - 1))
    }
}
