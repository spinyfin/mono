import XCTest
@testable import PaneLayout

final class PaneCapacityPolicyTests: XCTestCase {
    private let policy = PaneCapacityPolicy.standard

    private func geometry(_ width: Double, _ height: Double, cell: PaneSize = PaneSize(width: 6, height: 13)) -> PaneGeometry {
        PaneGeometry(area: PaneSize(width: width, height: height), cell: cell)
    }

    // MARK: Capacity

    func testMinimumPaneSizeIsColumnsByRowsPlusHeader() {
        let size = policy.minPaneSize(cell: PaneSize(width: 6, height: 13))
        XCTAssertEqual(size.width, 420)
        XCTAssertEqual(size.height, 24 * 13 + policy.headerHeight)
    }

    func testLaptopGeometryFitsFourByTwo() {
        let limits = policy.limits(for: Fixture.laptop)
        XCTAssertEqual(limits, GridLimits(columns: 4, rows: 2))
        XCTAssertEqual(policy.capacity(of: limits), 8)
    }

    func testCapacityIsCappedAtSixteenPanesPerPage() {
        let limits = policy.limits(for: geometry(4000, 3000))
        XCTAssertGreaterThan(limits.columns * limits.rows, 16)
        XCTAssertEqual(policy.capacity(of: limits), 16)
    }

    func testGapIsCountedBetweenPanesNotAfterTheLast() {
        // Exactly four minimum panes and three gaps fit across.
        let minWidth = policy.minPaneSize(cell: PaneSize(width: 6, height: 13)).width
        let exact = 4 * minWidth + 3 * policy.gap
        XCTAssertEqual(policy.limits(for: geometry(exact, 720)).columns, 4)
        XCTAssertEqual(policy.limits(for: geometry(exact - 0.5, 720)).columns, 3)
    }

    func testWindowBelowMinimumStillGetsOnePane() {
        let limits = policy.limits(for: geometry(300, 200))
        XCTAssertEqual(limits, GridLimits(columns: 1, rows: 1))
        XCTAssertEqual(policy.capacity(of: limits), 1)
        XCTAssertEqual(
            policy.shape(forUsedSpan: 1, limits: limits, geometry: geometry(300, 200)),
            GridShape(columns: 1, rows: 1)
        )
    }

    func testDegenerateMeasurementsFallBackToOnePane() {
        let zeroCell = geometry(1700, 720, cell: PaneSize(width: 0, height: 0))
        XCTAssertEqual(policy.limits(for: zeroCell), GridLimits(columns: 1, rows: 1))
        let zeroArea = geometry(0, 0)
        XCTAssertEqual(policy.limits(for: zeroArea), GridLimits(columns: 1, rows: 1))
        let notANumber = geometry(.nan, .infinity)
        XCTAssertEqual(policy.limits(for: notANumber), GridLimits(columns: 1, rows: 1))
    }

    func testPortraitWindowStacksPanes() {
        let portrait = geometry(800, 1500)
        let limits = policy.limits(for: portrait)
        XCTAssertEqual(limits, GridLimits(columns: 1, rows: 4))
        XCTAssertEqual(policy.shape(forUsedSpan: 2, limits: limits, geometry: portrait), GridShape(columns: 1, rows: 2))
    }

    // MARK: Dead band

    func testDeadBandHoldsCapacityJustBelowAThreshold() {
        let previous = policy.limits(for: Fixture.laptop)
        // 1690pt is below the four-column threshold (1692pt) but within the band.
        let nudged = policy.stableLimits(previous: previous, for: geometry(1690, 720))
        XCTAssertEqual(nudged.columns, 4)
        XCTAssertEqual(policy.limits(for: geometry(1690, 720)).columns, 3, "without hysteresis the column would drop")
    }

    func testDeadBandReleasesOnceThresholdIsClearedByMoreThanTheBand() {
        let previous = policy.limits(for: Fixture.laptop)
        XCTAssertEqual(policy.stableLimits(previous: previous, for: geometry(1670, 720)).columns, 3)
    }

    func testDeadBandHoldsGrowthUntilClearedByTheBand() {
        let previous = GridLimits(columns: 3, rows: 2)
        // 1700pt already admits four columns, but not with 16pt to spare.
        XCTAssertEqual(policy.stableLimits(previous: previous, for: geometry(1700, 720)).columns, 3)
        XCTAssertEqual(policy.stableLimits(previous: previous, for: geometry(1710, 720)).columns, 4)
    }

    func testDeadBandAppliesToRowsIndependently() {
        let previous = policy.limits(for: Fixture.laptop)
        // Two rows need 2 * 356 + 4 = 716pt; 710pt is inside the band.
        XCTAssertEqual(policy.stableLimits(previous: previous, for: geometry(1700, 710)).rows, 2)
        XCTAssertEqual(policy.stableLimits(previous: previous, for: geometry(1700, 690)).rows, 1)
    }

    // MARK: Grid selection

    func testShapesOnLaptopGeometryAreFullSizeThenSideBySideThenCompact() {
        let limits = policy.limits(for: Fixture.laptop)
        func shape(_ span: Int) -> GridShape {
            policy.shape(forUsedSpan: span, limits: limits, geometry: Fixture.laptop)
        }
        XCTAssertEqual(shape(1), GridShape(columns: 1, rows: 1))
        XCTAssertEqual(shape(2), GridShape(columns: 2, rows: 1))
        XCTAssertEqual(shape(3), GridShape(columns: 3, rows: 1))
        XCTAssertEqual(shape(4), GridShape(columns: 2, rows: 2))
        XCTAssertEqual(shape(5), GridShape(columns: 3, rows: 2))
        XCTAssertEqual(shape(6), GridShape(columns: 3, rows: 2))
        XCTAssertEqual(shape(7), GridShape(columns: 4, rows: 2))
        XCTAssertEqual(shape(8), GridShape(columns: 4, rows: 2))
    }

    func testShapeAlwaysHoldsTheSpanAndRespectsLimits() {
        let geometries = [Fixture.laptop, geometry(800, 1500), geometry(2200, 1500), geometry(3000, 900)]
        for geometry in geometries {
            let limits = policy.limits(for: geometry)
            for span in 1...policy.capacity(of: limits) {
                let shape = policy.shape(forUsedSpan: span, limits: limits, geometry: geometry)
                XCTAssertGreaterThanOrEqual(shape.cellCount, span, "span \(span) in \(geometry)")
                XCTAssertLessThanOrEqual(shape.columns, limits.columns)
                XCTAssertLessThanOrEqual(shape.rows, limits.rows)
            }
        }
    }

    func testEqualScoresPreferFewerEmptyCells() {
        // Unit panes, no header or gap: every shape that fits scores 1.0.
        let flat = PaneCapacityPolicy(minColumns: 1, minRows: 1, headerHeight: 0, gap: 0)
        let g = PaneGeometry(area: PaneSize(width: 300, height: 200), cell: PaneSize(width: 100, height: 100))
        let limits = flat.limits(for: g)
        XCTAssertEqual(limits, GridLimits(columns: 3, rows: 2))
        // 3x1 (no empties) beats 2x2 (one) and 3x2 (three), all of which score 1.0.
        XCTAssertEqual(flat.shape(forUsedSpan: 3, limits: limits, geometry: g), GridShape(columns: 3, rows: 1))
    }

    func testEqualScoresAndEmptyCellsPreferMoreColumns() {
        let flat = PaneCapacityPolicy(minColumns: 1, minRows: 1, headerHeight: 0, gap: 0)
        let g = PaneGeometry(area: PaneSize(width: 200, height: 200), cell: PaneSize(width: 100, height: 100))
        let limits = flat.limits(for: g)
        // 2x1 and 1x2 both score 1.0 with no empty cells.
        XCTAssertEqual(flat.shape(forUsedSpan: 2, limits: limits, geometry: g), GridShape(columns: 2, rows: 1))
    }

    func testSpanOfZeroIsTreatedAsOnePane() {
        let limits = policy.limits(for: Fixture.laptop)
        XCTAssertEqual(
            policy.shape(forUsedSpan: 0, limits: limits, geometry: Fixture.laptop),
            GridShape(columns: 1, rows: 1)
        )
    }

    func testShapeMapsLocalIndicesRowMajor() {
        let shape = GridShape(columns: 3, rows: 2)
        XCTAssertEqual(shape.position(ofLocalIndex: 0).row, 0)
        XCTAssertEqual(shape.position(ofLocalIndex: 2).column, 2)
        XCTAssertEqual(shape.position(ofLocalIndex: 3).row, 1)
        XCTAssertEqual(shape.position(ofLocalIndex: 3).column, 0)
    }
}
