import Foundation

/// A width/height pair in points. Used for both the Agents pane area and the
/// terminal cell size so this module stays free of AppKit/CoreGraphics.
public struct PaneSize: Equatable, Sendable {
    public var width: Double
    public var height: Double

    public init(width: Double, height: Double) {
        self.width = width
        self.height = height
    }
}

/// The two measurements capacity depends on: the area available to the pane
/// grid and the terminal cell size libghostty reports at the fixed worker font.
public struct PaneGeometry: Equatable, Sendable {
    public var area: PaneSize
    public var cell: PaneSize

    public init(area: PaneSize, cell: PaneSize) {
        self.area = area
        self.cell = cell
    }
}

/// How many minimum-size panes fit across and down the available area. The
/// per-page capacity derives from this; the rendered shape never exceeds it.
public struct GridLimits: Equatable, Sendable {
    public var columns: Int
    public var rows: Int

    public init(columns: Int, rows: Int) {
        self.columns = max(1, columns)
        self.rows = max(1, rows)
    }
}

/// The rendered grid of one page: `columns` by `rows` rectangles, mapped
/// row-major from the page's logical cell indices.
public struct GridShape: Equatable, Sendable {
    public let columns: Int
    public let rows: Int

    public init(columns: Int, rows: Int) {
        self.columns = max(1, columns)
        self.rows = max(1, rows)
    }

    public var cellCount: Int { columns * rows }

    public func position(ofLocalIndex index: Int) -> (row: Int, column: Int) {
        (row: index / columns, column: index % columns)
    }
}

/// Capacity and grid-shape policy for the Agents pane.
///
/// Every tunable lives here so the capture task can adjust them in one place.
/// The defaults are estimates from the design doc, not measurements.
public struct PaneCapacityPolicy: Equatable, Sendable {
    public static let standard = PaneCapacityPolicy()

    /// Minimum legible terminal width, in columns, at the fixed 10pt worker font.
    public var minColumns: Int
    /// Minimum legible terminal height, in rows.
    public var minRows: Int
    /// Height of the two-line pane header, in points. An estimate until the
    /// capture task measures the rendered header.
    public var headerHeight: Double
    /// Upper bound on logical cells per page; excess workers get another page.
    public var maxPanesPerPage: Int
    /// Spacing between adjacent panes, in points.
    public var gap: Double
    /// Hysteresis around each row/column threshold, in points. A measurement
    /// must clear a threshold by this much before capacity changes.
    public var deadBand: Double

    public init(
        minColumns: Int = 70,
        minRows: Int = 24,
        headerHeight: Double = 44,
        maxPanesPerPage: Int = 16,
        gap: Double = 4,
        deadBand: Double = 16
    ) {
        self.minColumns = max(1, minColumns)
        self.minRows = max(1, minRows)
        self.headerHeight = max(0, headerHeight)
        self.maxPanesPerPage = max(1, maxPanesPerPage)
        self.gap = max(0, gap)
        self.deadBand = max(0, deadBand)
    }

    /// The smallest pane that still shows `minColumns` x `minRows` of terminal
    /// plus its header.
    public func minPaneSize(cell: PaneSize) -> PaneSize {
        PaneSize(
            width: Double(minColumns) * cell.width,
            height: Double(minRows) * cell.height + headerHeight
        )
    }

    /// Per-page logical capacity for the given limits.
    public func capacity(of limits: GridLimits) -> Int {
        min(limits.columns * limits.rows, maxPanesPerPage)
    }

    /// Limits from a single measurement, with no hysteresis.
    public func limits(for geometry: PaneGeometry) -> GridLimits {
        stableLimits(previous: nil, for: geometry)
    }

    /// Limits from a measurement, holding `previous` unless the area clears a
    /// row/column threshold by more than `deadBand`. This stops a pane edge
    /// sitting on a threshold from flipping capacity on every resize.
    public func stableLimits(previous: GridLimits?, for geometry: PaneGeometry) -> GridLimits {
        // A cell size of zero (or garbage) means the terminal has not been
        // measured yet. Do not read that as "unboundedly many panes fit".
        guard geometry.cell.width.isFinite, geometry.cell.width > 0,
              geometry.cell.height.isFinite, geometry.cell.height > 0
        else {
            return previous ?? GridLimits(columns: 1, rows: 1)
        }
        let minPane = minPaneSize(cell: geometry.cell)
        return GridLimits(
            columns: stableCount(
                previous: previous?.columns, extent: geometry.area.width, minPane: minPane.width),
            rows: stableCount(
                previous: previous?.rows, extent: geometry.area.height, minPane: minPane.height)
        )
    }

    /// Chooses the rendered grid for a page whose used span is `usedSpan`
    /// (highest occupied-or-placeholder local index plus one).
    ///
    /// Among shapes with `columns * rows >= usedSpan` inside `limits`, picks the
    /// one that maximises the smaller of width/height relative to the minimum
    /// pane, then fewer empty cells, then more columns. A window smaller than
    /// one minimum pane still yields a 1x1 shape.
    public func shape(forUsedSpan usedSpan: Int, limits: GridLimits, geometry: PaneGeometry) -> GridShape {
        let span = max(1, usedSpan)
        let minPane = minPaneSize(cell: geometry.cell)
        // An axis longer than the span is dominated by clamping it to the span:
        // panes get wider and there are fewer empty cells.
        let maxColumns = min(limits.columns, span)
        let maxRows = min(limits.rows, span)

        var best: (shape: GridShape, score: Double, empty: Int)?
        for columns in 1...maxColumns {
            for rows in 1...maxRows where columns * rows >= span {
                let score = fitScore(columns: columns, rows: rows, geometry: geometry, minPane: minPane)
                let empty = columns * rows - span
                if let current = best {
                    let scoreDelta = score - current.score
                    let better: Bool
                    if abs(scoreDelta) > Self.scoreEpsilon {
                        better = scoreDelta > 0
                    } else if empty != current.empty {
                        better = empty < current.empty
                    } else {
                        better = columns > current.shape.columns
                    }
                    if !better { continue }
                }
                best = (GridShape(columns: columns, rows: rows), score, empty)
            }
        }
        return best?.shape ?? GridShape(columns: limits.columns, rows: limits.rows)
    }

    private static let scoreEpsilon = 1e-9
    private static let maxAxisCount = 10_000.0

    private func fitScore(columns: Int, rows: Int, geometry: PaneGeometry, minPane: PaneSize) -> Double {
        let paneWidth = (geometry.area.width - Double(columns - 1) * gap) / Double(columns)
        let paneHeight = (geometry.area.height - Double(rows - 1) * gap) / Double(rows)
        let widthRatio = minPane.width > 0 ? paneWidth / minPane.width : 0
        let heightRatio = minPane.height > 0 ? paneHeight / minPane.height : 0
        return min(widthRatio, heightRatio)
    }

    private func count(extent: Double, minPane: Double) -> Int {
        guard extent.isFinite, minPane.isFinite, minPane + gap > 0 else { return 1 }
        let raw = ((extent + gap) / (minPane + gap)).rounded(.down)
        guard raw.isFinite else { return 1 }
        return max(1, Int(min(raw, Self.maxAxisCount)))
    }

    private func stableCount(previous: Int?, extent: Double, minPane: Double) -> Int {
        guard let previous else { return count(extent: extent, minPane: minPane) }
        let grown = count(extent: extent - deadBand, minPane: minPane)
        if grown > previous { return grown }
        let shrunk = count(extent: extent + deadBand, minPane: minPane)
        if shrunk < previous { return shrunk }
        return previous
    }
}
