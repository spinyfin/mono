import Foundation

/// What a rendered rectangle holds.
public enum PaneCellContent: Equatable, Sendable {
    /// Unassigned: a hole, or a spare rectangle past the page's logical capacity.
    case empty
    /// A live engine member. Draw its terminal, or a "Viewer not attached"
    /// card when `hasViewer` is false.
    case occupied(PaneMember)
    /// A released run's retained cell ("<persona> finished"). Never counts as
    /// occupied and never justifies a page.
    case placeholder(runId: String)
}

public struct PaneCell: Equatable, Sendable {
    public let localIndex: Int
    public let row: Int
    public let column: Int
    public let content: PaneCellContent
}

/// One page of the rendered grid.
public struct PaneLayoutPage: Equatable, Sendable {
    public let index: Int
    public let shape: GridShape
    /// Every rendered rectangle, row-major (`shape.cellCount` entries).
    public let cells: [PaneCell]
    public let occupiedCount: Int
    public let waitingCount: Int
}

/// A page-selector chip: occupied count and a needs-input dot.
public struct PageSelector: Equatable, Sendable {
    public let index: Int
    public let occupiedCount: Int
    public let waitingCount: Int

    public var hasWaiting: Bool { waitingCount > 0 }
}
